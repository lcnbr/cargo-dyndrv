"""Qualify lazy compiler preparation and retained ownership on unchanged commands.

Runs the existing integration Harness against the configured system store.
No system collection or daemon stop is requested; fixtures and roots are retained.
"""

import json
import os
import shutil
import stat
import sys
from pathlib import Path

from test_integration import Harness


class WarmCommandsHarness(Harness):
    def completion(self, record, *, prepared, validation):
        session = Path(record["session"])
        value = json.loads((session / "cargo-result.json").read_text())
        self.require(
            value["version"] == 2 and value["status"] == 0,
            "Missing successful completion protocol",
        )
        observations = [
            json.loads(path.read_text())
            for path in (session / "invocations").glob("*.json")
        ]
        actual = sum(value.get("execution") == "nix-compiler" for value in observations)
        self.require(
            value["nix_invocations"] == actual,
            "Compiler count disagrees with observations",
        )
        self.require(
            bool(actual) == prepared, "Unexpected immutable compiler invocation"
        )
        self.require(
            (session / "prepare.json").is_file() == prepared
            and (session / "compiler-inputs.json").is_file() == prepared,
            "Compiler preparation does not match actual compiler work",
        )
        last = json.loads((session.parent.parent / "last-run.json").read_text())
        self.require(last["session"] == str(session), "Wrong worker result")
        self.require(last["ownership_validation"] == validation, "Wrong ownership path")
        if validation == "retained":
            self.require(
                "snapshot_references" not in last["phase_seconds"]
                and "retain" not in last["phase_seconds"],
                "Retained command performed a full snapshot or final retention",
            )
        return observations

    def qualify(self):
        with self.step("prepare isolated system-store workspace"):
            self.environment["CARGO_DYNDRV_STORE"] = "system"
            self.prepare()
            self.require(
                self.spec.get("artifact_delivery", "copy") == "copy",
                "Use copied artifacts to test on-consumption corruption checks",
            )

        with self.step(
            "first compilation prepares once and establishes retained ownership"
        ):
            first = self.run_value(self.a, "initial", "dirty")
            self.completion(first, prepared=True, validation="full")
            inputs = json.loads(
                (Path(first["session"]) / "compiler-inputs.json").read_text()
            )
            self.require(
                inputs["trees"] and inputs["builder_store"],
                "Incomplete prepared inputs",
            )

        with self.step(
            "unchanged command prepares nothing and preserves established roots"
        ):
            warm = self.run_value(self.a, "warm", "dirty")
            self.completion(warm, prepared=False, validation="retained")
            self.require(
                not warm["receipts"], "Warm command delivered compiler outputs"
            )

        with self.step(
            "repeated Nextest still executes tests without compiler preparation"
        ):
            self.public(self.a, "tests-first", "test", "-p", "application", "--lib")
            _, tested = self.public(
                self.a, "tests-second", "test", "-p", "application", "--lib"
            )
            self.completion(tested, prepared=False, validation="retained")
            marker = Path(tested["target"]) / "runtime-count"
            self.require(
                marker.read_text().splitlines() == ["executed", "executed"],
                "Tests did not execute twice",
            )

        with self.step("native rustdoc reruns without immutable compiler preparation"):
            _, documented = self.public(
                self.a, "doc-first", "doc", "--no-deps", "-p", "application", "--lib"
            )
            index = Path(documented["target"]) / "doc/application/index.html"
            self.require(index.is_file(), "Documentation output missing")
            index.unlink()
            _, repeated = self.public(
                self.a,
                "doc-native-repeat",
                "doc",
                "--no-deps",
                "-p",
                "application",
                "--lib",
            )
            observed = self.completion(repeated, prepared=False, validation="retained")
            self.require(
                any(row.get("execution") == "native-rustdoc" for row in observed),
                "Fixture did not actually rerun native rustdoc",
            )

        with self.step("source change is staged before Cargo and lazily materialized"):
            source = self.fork_a / "packages/itoa/src/lib.rs"
            before = source.stat()
            self.edit_fork(self.fork_a, "clean")
            os.utime(source, ns=(before.st_atime_ns, before.st_mtime_ns))
            changed = self.run_value(self.a, "same-mtime-edit", "clean")
            self.completion(changed, prepared=True, validation="full")
            self.edit_fork(self.fork_a, "dirty")
            self.run_value(self.a, "restore-source", "dirty")
            restored = self.run_value(self.a, "restore-source-warm", "dirty")
            self.completion(restored, prepared=False, validation="retained")

        with self.step(
            "restored target requires reconciliation despite zero compilers"
        ):
            target = Path(restored["target"])
            backup = self.run_directory / "saved-target"
            shutil.copytree(target, backup, symlinks=True)
            shutil.rmtree(target)
            shutil.copytree(backup, target, symlinks=True)
            reconciled = self.run_value(self.a, "restored-target", "dirty")
            self.completion(reconciled, prepared=False, validation="full")
            restored = self.run_value(self.a, "restored-target-warm", "dirty")
            self.completion(restored, prepared=False, validation="retained")

        with self.step(
            "native failure invalidates warm eligibility until reconciliation"
        ):
            environment = self.environment | {
                "CARGO_DYNDRV_TEST_RUNTIME_NAMES": "CARGO_DYNDRV_EXPECTED_MISSING_VALUE"
            }
            environment.pop("CARGO_DYNDRV_EXPECTED_MISSING_VALUE", None)
            self.public(
                self.a,
                "runtime-failure",
                "run",
                "-p",
                "application",
                env=environment,
                expected=101,
            )
            recovered = self.run_value(self.a, "runtime-recovered", "dirty")
            self.completion(recovered, prepared=False, validation="full")
            warm = self.run_value(self.a, "runtime-recovered-warm", "dirty")
            self.completion(warm, prepared=False, validation="retained")

        with self.step(
            "unused corruption is deferred, explicit audit and compiler use reject it"
        ):
            target = Path(warm["target"])
            archive = next(target.glob("ci-optim/deps/libitoa-*.rlib"))
            self.require(
                stat.S_ISREG(archive.lstat().st_mode),
                "Refusing to modify a linked artifact",
            )
            original = archive.read_bytes()
            info = archive.stat()
            consumer = self.a / "app/src/main.rs"
            consumer_source = consumer.read_text()
            try:
                archive.write_bytes(original[:-1] + bytes([original[-1] ^ 1]))
                os.utime(archive, ns=(info.st_atime_ns, info.st_mtime_ns))
                unchanged = self.run_value(self.a, "unused-corruption-warm", "dirty")
                self.completion(unchanged, prepared=False, validation="retained")
                self.public(self.a, "explicit-audit-rejects", "audit", expected=1)
                consumer.write_text(
                    consumer_source
                    + "\n// Force actual linking of the altered dependency.\n"
                )
                self.public(
                    self.a,
                    "compiler-use-rejects",
                    "build",
                    "-p",
                    "application",
                    expected=101,
                )
            finally:
                archive.write_bytes(original)
                os.utime(archive, ns=(info.st_atime_ns, info.st_mtime_ns))
                consumer.write_text(consumer_source)
            self.public(self.a, "clean-after-rejection", "clean")
            self.run_value(self.a, "rebuilt-after-clean", "dirty")
            final = self.run_value(self.a, "final-warm", "dirty")
            self.completion(final, prepared=False, validation="retained")
            self.public(self.a, "explicit-audit-passes", "audit")

        self.summary["shared_gc"] = "not requested"
        self.summary["retained"] = (
            "owned targets, recovery backup, roots and evidence for review"
        )


if __name__ == "__main__":
    sys.exit(
        WarmCommandsHarness.run_cli(
            WarmCommandsHarness.parser(__doc__, "warm-commands")
        )
    )
