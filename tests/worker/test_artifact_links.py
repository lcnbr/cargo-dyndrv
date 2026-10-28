"""Qualify opt-in immutable rlibs against the administrator-owned system store.

Uses the existing worker Harness; never collects or stops the shared daemon.
Targets, receipts, registered roots and command logs remain available for review.
"""

import hashlib
import json
import os
import shutil
import stat
import sys
from pathlib import Path

from test_integration import Harness


class ArtifactLinksHarness(Harness):
    def representations(self, record):
        target = Path(record["target"])
        policy = Path(record["session"]).parent.parent
        namespace = json.loads((policy / "namespace.json").read_text())
        logical_target = Path(namespace["logical"]) / "target"
        linked = {}
        copied = set()
        for path in (target / ".dyndrv-delivered").glob("*.json"):
            receipt = json.loads(path.read_text())
            self.require(receipt["version"] == 6, "Wrong ownership schema")
            for logical, representation in receipt["owned"].items():
                if (
                    hashlib.sha256(os.fsencode(logical)).hexdigest() + ".json"
                    != path.name
                ):
                    continue
                local = target / Path(logical).relative_to(logical_target)
                if not local.exists() and not local.is_symlink():
                    continue
                if representation == "store-link":
                    backing = Path(receipt["files"][logical])
                    self.require(
                        local.suffix == ".rlib"
                        and local.is_symlink()
                        and os.readlink(local) == str(backing)
                        and stat.S_ISREG(backing.lstat().st_mode)
                        and backing.stat().st_uid == 0
                        and not backing.stat().st_mode & 0o222,
                        "An owned link lacks exact protected backing",
                    )
                    sibling = local.with_suffix(".rmeta")
                    self.require(
                        sibling.is_file() and not sibling.is_symlink(),
                        "Linked rlib lost its copied metadata",
                    )
                    linked[local] = backing
                else:
                    self.require(
                        representation == "copy"
                        and stat.S_ISREG(local.lstat().st_mode),
                        "Copied output changed representation",
                    )
                    copied.add(local.suffix)
        self.require(linked, "Fixture delivered no immutable rlibs")
        self.require(
            {".rmeta", ".d", ".so"} <= copied, "Expected outputs were not copied"
        )
        return linked

    def snapshot(self, record, label, *, expected=0):
        session = Path(record["session"])
        _, _, result = self.command(
            label,
            [
                self.spec["python"],
                self.spec["worker_script"],
                "namespace",
                session.parent.parent / "namespace.json",
                "--",
                self.spec["driver"],
                "snapshot-references",
                "--directory",
                session,
                "--output",
                self.run_directory / (label + ".json"),
            ],
            expected=expected,
        )
        return result

    def qualify(self):
        with self.step("prepare system-store fixture with macro and generated probes"):
            self.environment["CARGO_DYNDRV_STORE"] = "system"
            self.prepare()
            self.require(
                self.spec.get("artifact_delivery") == "system-store-rlib-links",
                "Pass an explicitly opted-in bootstrap",
            )
            self.write(
                self.a,
                "crates/support/Cargo.toml",
                '[package]\nname="support"\nversion="0.1.0"\nedition="2021"\n'
                '[dependencies]\nsupport-macro={path="../support-macro"}\n',
            )
            self.write(
                self.a,
                "crates/support/src/lib.rs",
                "pub fn support() -> &'static str { support_macro::support!() }\n",
            )
            self.write(
                self.a,
                "crates/support-macro/Cargo.toml",
                '[package]\nname="support-macro"\nversion="0.1.0"\nedition="2021"\n'
                "[lib]\nproc-macro=true\n",
            )
            self.write(
                self.a,
                "crates/support-macro/src/lib.rs",
                "use proc_macro::TokenStream;\n"
                "#[proc_macro]\npub fn support(_: TokenStream) -> TokenStream {\n"
                '    "\\"support\\"".parse().expect("valid string literal")\n}\n',
            )
            self.write(
                self.a,
                "crates/support-macro/build.rs",
                'fn main() { println!("cargo:rerun-if-changed=build.rs"); }\n',
            )
            self.write(
                self.a,
                "app/build.rs",
                """use std::env;
use std::error::Error;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn main() -> Result<(), Box<dyn Error>> {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rustc-env=ARBITRARY_NEW_COMPILER_VALUE=ok");
    let out = PathBuf::from(env::var("OUT_DIR")?);
    for (name, source, succeeds) in [
        ("positive", "pub fn probe() {}", true),
        ("negative", "compile_error!(\\"expected probe failure\\");", false),
    ] {
        let input = out.join(format!("{name}.rs"));
        fs::write(&input, source)?;
        let status = Command::new(env::var("RUSTC")?)
            .arg("--crate-type=lib")
            .arg("--emit=metadata")
            .arg("-o")
            .arg(out.join(format!("{name}.rmeta")))
            .arg(input)
            .status()?;
        assert_eq!(status.success(), succeeds);
    }
    Ok(())
}
""",
            )
            self.lock_and_metadata(self.a, "artifact-links")

        copy_record = None
        if self.options.copy_bootstrap is not None:
            with self.step("copy policy stays fresh with identical compiler inputs"):
                link_bootstrap, link_spec = self.bootstrap, self.spec
                self.bootstrap = self.options.copy_bootstrap.resolve()
                self.spec = json.loads(
                    (self.bootstrap / "environment.json").read_text()
                )
                self.require(
                    self.spec | {"artifact_delivery": "system-store-rlib-links"}
                    == link_spec,
                    "Copy and link bootstraps differ beyond delivery policy",
                )
                try:
                    copy_record = self.run_value(self.a, "copy-initial", "dirty")
                    copy_warm = self.run_value(self.a, "copy-warm", "dirty")
                    self.require(not copy_warm["receipts"], "Copy policy recompiled")
                    target = Path(copy_warm["target"])
                    self.require(
                        all(not p.is_symlink() for p in target.glob("*/deps/*.rlib")),
                        "Copy policy installed store links",
                    )
                finally:
                    self.bootstrap, self.spec = link_bootstrap, link_spec

        with self.step(
            "deliver immutable rlibs and keep unchanged Cargo fresh"
        ) as step:
            first = self.run_value(self.a, "initial", "dirty")
            if copy_record is not None:
                identity = lambda record: sorted(
                    (r["crate"], r["derivation"], r["output"], r["compilerSucceeded"])
                    for r in record["receipts"]
                )
                self.require(
                    identity(copy_record) == identity(first),
                    "Delivery policy changed immutable compiler outputs",
                )
                step["copy_and_link_compiler_identities_equal"] = True
            links = self.representations(first)
            warm = self.run_value(self.a, "warm", "dirty")
            self.require(not warm["receipts"], "Unchanged command compiled again")
            step.update(
                linked_files=len(links),
                logical_snapshot_comparison_bytes_avoided=2
                * sum(p.stat().st_size for p in links),
            )
            self.snapshot(warm, "snapshot-accepted")

        with self.step("run Nextest twice and preserve runtime execution"):
            self.public(self.a, "test-first", "test", "-p", "application", "--lib")
            _, tested = self.public(
                self.a, "test-second", "test", "-p", "application", "--lib"
            )
            self.require(not tested["receipts"], "Second Nextest invocation compiled")
            marker = Path(tested["target"]) / "runtime-count"
            self.require(
                marker.read_text().splitlines() == ["executed", "executed"],
                "Tests did not run twice",
            )

        with self.step("rebuild dependency separately, relink consumer, then restore"):
            self.edit_fork(self.fork_a, "changed")
            self.public(self.a, "dependency-only", "build", "-p", "itoa")
            changed = self.run_value(self.a, "changed-consumer", "changed")
            self.require(changed["receipts"], "Consumer missed changed dependency")
            self.representations(changed)
            self.edit_fork(self.fork_a, "dirty")
            self.run_value(self.a, "restored-source", "dirty")
            restored = self.run_value(self.a, "restored-source-warm", "dirty")
            self.require(
                not restored["receipts"], "Restored source did not become fresh"
            )

        with self.step(
            "preserve link representation through target backup and restore"
        ):
            target = Path(restored["target"])
            backup = self.run_directory / "saved-target"
            shutil.copytree(target, backup, symlinks=True)
            shutil.rmtree(target)
            shutil.copytree(backup, target, symlinks=True)
            fresh = self.run_value(self.a, "restored-target", "dirty")
            self.require(not fresh["receipts"], "Restored target was not Cargo-fresh")
            self.representations(fresh)

        with self.step(
            "reject wrong, dangling and copied substitutes without store writes"
        ):
            local, backing = next(iter(self.representations(fresh).items()))
            other = next(
                p for p in backing.parent.iterdir() if p.is_file() and p != backing
            )
            try:
                for name, replacement in (
                    ("wrong-link", other),
                    (
                        "dangling-link",
                        Path(
                            "/nix/store/00000000000000000000000000000000-missing/artifact"
                        ),
                    ),
                    ("regular-copy", None),
                ):
                    local.unlink()
                    if replacement is None:
                        shutil.copyfile(backing, local)
                    else:
                        local.symlink_to(replacement)
                    result = self.snapshot(fresh, "reject-" + name, expected=1)
                    self.require(
                        Path(result["stderr"]).read_text(),
                        "Rejection lacked a diagnostic",
                    )
            finally:
                local.unlink(missing_ok=True)
                local.symlink_to(backing)
            self.snapshot(fresh, "snapshot-after-tamper-restore")

        with self.step("failed compiler retains backing and recovers"):
            source = self.a / "app/src/main.rs"
            original = source.read_text()
            try:
                source.write_text(
                    original + '\ncompile_error!("expected fixture failure");\n'
                )
                self.public(
                    self.a, "failed-build", "build", "-p", "application", expected=101
                )
            finally:
                source.write_text(original)
            recovered = self.run_value(self.a, "recovered", "dirty")
            links = self.representations(recovered)
            self.require(
                all(backing.exists() for backing in links.values()),
                "Recovery lost immutable backing",
            )
            self.validate_owners(
                recovered["session"],
                sorted({str(Path(*p.parts[:4])) for p in links.values()}),
                "retained-rlib-owners",
            )
            self.snapshot(recovered, "final-snapshot")

        self.summary["shared_gc"] = "not requested"
        self.summary["retained"] = (
            "owned targets, exact backup, store roots and evidence for review"
        )


if __name__ == "__main__":
    parser = ArtifactLinksHarness.parser(__doc__, "artifact-links")
    parser.add_argument("--copy-bootstrap", type=Path)
    sys.exit(ArtifactLinksHarness.run_cli(parser))
