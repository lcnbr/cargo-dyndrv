"""Keep native-only dependency changes correct and Cargo-fresh in both delivery modes.

Uses a copy or system-store-rlib-links bootstrap with the existing Harness.
Fixtures, targets, registered roots and logs are retained under --directory;
the shared system store is never collected and its daemon is never stopped.
"""

import hashlib
import json
import stat
import sys
from pathlib import Path

from test_integration import Harness


class NativeFreshnessHarness(Harness):
    def artifacts(self, record):
        artifacts = {}
        for suffix in ("rlib", "rmeta"):
            paths = list(Path(record["target"]).glob(f"*/deps/libsupport-*.{suffix}"))
            self.require(len(paths) == 1, f"Expected one support .{suffix}: {paths}")
            path = paths[0]
            linked = (
                suffix == "rlib"
                and self.spec["artifact_delivery"] == "system-store-rlib-links"
            )
            self.require(
                path.is_file()
                and path.is_symlink() == linked
                and (linked or stat.S_ISREG(path.lstat().st_mode)),
                f"Wrong artifact representation: {path}",
            )
            backing = path.resolve()
            if linked:
                self.require(
                    backing.is_relative_to("/nix/store")
                    and backing.stat().st_uid == 0
                    and not backing.stat().st_mode & 0o222,
                    f"Archive link lacks immutable system-store backing: {backing}",
                )
            artifacts[suffix] = {
                "path": str(path),
                "sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
                "mtime_ns": path.stat().st_mtime_ns,
                "symlink": path.is_symlink(),
                "backing": str(backing),
            }
        return artifacts

    def support_delivery(self, record):
        receipts = [r for r in record["receipts"] if r["crate"] == "support"]
        self.require(len(receipts) == 1, f"Expected one support delivery: {receipts}")
        self.require(receipts[0]["compilerSucceeded"], "Support compiler failed")
        return receipts[0]

    def run_application(self, label, expected):
        output, record = self.public(self.a, label, "run", "-vv", "-p", "application")
        # Cargo -vv also writes build-script messages to stdout.
        lines = output.splitlines()
        record["runtime_output"] = lines[-1] if lines else ""
        self.require(
            record["runtime_output"] == str(expected),
            f"{label}: expected native value {expected}, got {output!r}",
        )
        return {
            "command": label,
            "output": record["runtime_output"],
            "compiler_crates": [r["crate"] for r in record["receipts"]],
        }, record

    def qualify(self):
        self.summary["retained"] = (
            "Fixture, targets, registered store roots and command/receipt logs "
            "retained for review; shared store and daemon untouched."
        )
        with self.step(
            "prepare native static library with an unchanged Rust interface"
        ):
            self.environment["CARGO_DYNDRV_STORE"] = "system"
            self.prepare()
            self.require(
                self.spec["artifact_delivery"] in ("copy", "system-store-rlib-links"),
                "Use a copy or system-store-rlib-links bootstrap",
            )
            # A fresh source identity makes early metadata observable even when
            # the system store already contains previous runs of this test.
            self.write(
                self.a,
                "crates/support/src/lib.rs",
                f"// Native freshness run: {self.run_directory.name}\n"
                + """extern "C" { fn native_value() -> i32; }

pub fn support() -> i32 {
    // SAFETY: the bundled C function takes no arguments and returns an int.
    unsafe { native_value() }
}
""",
            )
            self.write(
                self.a,
                "crates/support/build.rs",
                """use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());
    assert!(Command::new("gcc")
        .args(["-c", "native.c", "-o"]).arg(out.join("native.o"))
        .status().unwrap().success());
    assert!(Command::new("ar")
        .arg("crs").arg(out.join("libnative.a")).arg(out.join("native.o"))
        .status().unwrap().success());
    println!("cargo:rustc-link-search=native={}", out.display());
    println!("cargo:rustc-link-lib=static=native");
    println!("cargo:rerun-if-changed=native.c");
}
""",
            )
            self.write(
                self.a,
                "app/src/lib.rs",
                "pub fn value() -> i32 { support::support() }\n",
            )
            self.write(
                self.a,
                "app/src/main.rs",
                'fn main() { println!("{}", application::value()); }\n',
            )
            self.write(
                self.a,
                "crates/support/native.c",
                "int native_value(void) { return 11; }\n",
            )
            result = self.summary["native_freshness"] = {
                "artifact_delivery": self.spec["artifact_delivery"],
                "edits": [],
            }

        with self.step("initial output 11, early metadata delivery and warm reuse"):
            # Select support as a primary package here, as in the later
            # dependency-only builds, so restoring C can reuse this invocation.
            _, initial = self.public(
                self.a,
                "initial-build",
                "build",
                "-vv",
                "-p",
                "support",
                "-p",
                "application",
            )
            before = result["initial_artifacts"] = self.artifacts(initial)
            original_delivery = self.support_delivery(initial)
            raw = json.loads(Path(original_delivery["path"]).read_text())
            ready = raw["metadataReady"]
            result["pipelining"] = {
                "receipt": original_delivery["path"],
                "early_metadata_files": raw["earlyMetadataFiles"],
                "metadata_ready": ready,
                "final_messages_seconds": raw["secondsBeforeFinalCompilerMessages"],
            }
            # Verify actual early delivery, not the receipt's policy text.
            # This does not impose a timing threshold or require task overlap.
            self.require(
                raw["earlyMetadataFiles"] > 0
                and ready
                and all(
                    Path(event["path"]).suffix == ".rmeta"
                    and event["bytes"] > 0
                    and event["offsetSeconds"]
                    < raw["secondsBeforeFinalCompilerMessages"]
                    for event in ready
                ),
                "Initial support build did not deliver metadata before final messages",
            )
            result["initial"], warm = self.run_application("initial", 11)
            self.require(not warm["receipts"], "Initial warm run invoked compilers")

        for label, value in (("changed", 22), ("restored", 11)):
            edit = {"label": label, "expected_output": str(value), "before": before}
            result["edits"].append(edit)
            with self.step(f"{label}: change C only and build support separately"):
                self.write(
                    self.a,
                    "crates/support/native.c",
                    f"int native_value(void) {{ return {value}; }}\n",
                )
                _, dependency = self.public(
                    self.a, f"{label}-dependency", "build", "-vv", "-p", "support"
                )
                delivery = edit["support_delivery"] = self.support_delivery(dependency)
                edit["support_early_metadata_files"] = json.loads(
                    Path(delivery["path"]).read_text()
                )["earlyMetadataFiles"]
                after = edit["after"] = self.artifacts(dependency)
                edit["rlib_changed"] = (
                    before["rlib"]["sha256"] != after["rlib"]["sha256"]
                )
                edit["rmeta_bytes_unchanged"] = (
                    before["rmeta"]["sha256"] == after["rmeta"]["sha256"]
                )
                self.require(
                    edit["rlib_changed"], "Native edit did not change the rlib"
                )
                # Timestamps are evidence only: advancing copied metadata may
                # legitimately be needed to make a linked archive Cargo-fresh.
                self.require(
                    edit["rmeta_bytes_unchanged"],
                    "Rust metadata changed; this no longer isolates a native-only edit",
                )
                self.require(
                    all(r["crate"] != "application" for r in dependency["receipts"]),
                    "Dependency-only build unexpectedly rebuilt the consumer",
                )
                if label == "restored":
                    edit["reused_initial_support_output"] = all(
                        delivery[key] == original_delivery[key]
                        for key in ("derivation", "output")
                    )
                    self.require(
                        edit["reused_initial_support_output"],
                        "Restoring native.c did not exercise cached support delivery",
                    )
                    self.require(
                        edit["support_early_metadata_files"] == 0,
                        "Cached restoration unexpectedly replayed early metadata",
                    )

            with self.step(f"{label}: run existing consumer and three unchanged runs"):
                edit["consumer"], consumer = self.run_application(
                    f"{label}-consumer", value
                )
                edit["warm_runs"] = []
                for index in range(1, 4):
                    warm, _ = self.run_application(f"{label}-warm-{index}", value)
                    edit["warm_runs"].append(warm)
                # Gather all three runs before checking freshness so failures
                # retain evidence of persistent, rather than one-off, rebuilding.
                self.require(
                    all(r["crate"] != "support" for r in consumer["receipts"]),
                    f"{label}: consumer rebuilt the already-built dependency; "
                    f"warm compiler crates: {[r['compiler_crates'] for r in edit['warm_runs']]}",
                )
                self.require(
                    all(not r["compiler_crates"] for r in edit["warm_runs"]),
                    f"{label}: unchanged runs invoked compiler wrappers: {edit['warm_runs']}",
                )
            before = after


if __name__ == "__main__":
    sys.exit(
        NativeFreshnessHarness.run_cli(
            NativeFreshnessHarness.parser(__doc__, "native-freshness", timeout=300)
        )
    )
