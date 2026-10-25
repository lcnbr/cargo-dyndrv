"""Real changed-crate incremental reuse, stripped-state control, and recovery."""

import argparse
import concurrent.futures
import hashlib
import json
import os
import shutil
import subprocess
import time
import uuid
from pathlib import Path

from test_integration import Harness, REPOSITORY as ROOT


class Qualification:
    def __init__(self, directory, bootstrap):
        self.base = directory.resolve()
        self.base.mkdir(parents=True, exist_ok=True)
        self.run = self.base / ("run-" + uuid.uuid4().hex[:12])
        self.run.mkdir(mode=0o700)
        self.state = self.base / "state"
        self.bootstrap = bootstrap.resolve()
        self.spec = json.loads((bootstrap / "environment.json").read_text())
        self.env = os.environ | {
            "PATH": str(bootstrap / "tools/bin") + os.pathsep + os.environ["PATH"],
            "CARGO_DYNDRV_STATE": str(self.state),
            "CARGO_DYNDRV_STORE": "private",
            "CARGO_DYNDRV_OBSERVE_INCREMENTAL": "1",
            "CARGO_INCREMENTAL": "1",
        }
        self.commands = []

    def command(self, workspace, label, action="build", expected=0):
        arguments = [
            self.spec["python"],
            str(workspace / "tests/worker/workload.py"),
            "--bootstrap",
            str(self.bootstrap),
            "--action",
            action,
            "--",
        ]
        if action not in {"gc", "stop", "clean"}:
            arguments.extend(["-p", "incremental-probe"])
        started = time.monotonic()
        result = subprocess.run(
            arguments,
            cwd=workspace,
            env=self.env,
            text=True,
            capture_output=True,
            timeout=180,
            check=False,
        )
        (self.run / (label + ".stdout")).write_text(result.stdout)
        (self.run / (label + ".stderr")).write_text(result.stderr)
        self.commands.append(
            {
                "label": label,
                "status": result.returncode,
                "seconds": time.monotonic() - started,
            }
        )
        assert result.returncode == expected, (
            label,
            result.returncode,
            result.stderr[-5000:],
        )
        return result

    def fixture(self, name):
        destination = self.run / name
        Harness.copy_tooling(destination)
        (destination / "Cargo.toml").write_text(
            '[package]\nname="incremental-probe"\nversion="0.1.0"\nedition="2021"\n[workspace]\n[profile.ci-optim]\ninherits="dev"\nincremental=true\ncodegen-units=16\ndebug=false\n'
        )
        (destination / ".config").mkdir()
        (destination / ".config/nextest.toml").write_text("[profile.ci_dyndrv]\n")
        (destination / "src").mkdir()
        for index in range(16):
            (destination / f"src/module{index}.rs").write_text(
                f"#[inline(never)] pub fn value(x: u64) -> u64 {{ x.wrapping_mul({index + 2}).wrapping_add({index}) }}\n"
            )
        (destination / "src/lib.rs").write_text(
            "".join(f"mod module{i};\n" for i in range(16))
            + "pub fn value() -> u64 { "
            + " + ".join(f"module{i}::value(2)" for i in range(16))
            + " }\n#[test] fn current_value() { assert_eq!(value(), 425); }\n"
        )
        (destination / "src/main.rs").write_text(
            'fn main() { println!("{}", incremental_probe::value()); }\n'
        )
        subprocess.run(
            ["git", "init", "--quiet", str(destination)], env=self.env, check=True
        )
        subprocess.run(
            [self.spec["cargo"], "generate-lockfile", "--offline"],
            cwd=destination,
            env=self.env,
            check=True,
        )
        return destination

    def last(self, workspace):
        matches = [
            json.loads(path.read_text())
            for path in self.state.glob("workspaces/*/*/last-run.json")
        ]
        return next(item for item in matches if item["workspace"] == str(workspace))

    def receipts(self, workspace):
        session = Path(self.last(workspace)["session"])
        return [
            json.loads(path.read_text())
            for path in session.glob("invocations/*.nix-result.json")
        ]

    def witnesses(self, workspace):
        records = []
        session = Path(self.last(workspace)["session"])
        for path in session.glob("invocations/*.nix-stderr"):
            for line in path.read_text().splitlines():
                if not line.startswith("@nix "):
                    continue
                for field in json.loads(line[5:]).get("fields", []):
                    marker = "DYNDRV_STOCK_INCREMENTAL_REUSE "
                    if isinstance(field, str) and marker in field:
                        records.append(json.loads(field.split(marker, 1)[1]))
        return records

    def output(self, workspace):
        return subprocess.check_output(
            [workspace / "target/ci-optim/incremental-probe"], text=True
        ).strip()

    def qualify(self):
        a = self.fixture("producer")
        try:
            self.command(a, "producer")
            assert self.output(a) == "424"
            baseline = self.last(a)
            target = Path(baseline["target"])
            policy = target.parent.parent.name
            consumers = []
            for name, strip in (("warm", False), ("stripped", True)):
                b = self.run / name
                shutil.copytree(
                    a,
                    b,
                    ignore=shutil.ignore_patterns("target", ".git"),
                    copy_function=shutil.copy2,
                )
                subprocess.run(
                    ["git", "init", "--quiet", str(b)], env=self.env, check=True
                )
                identity = hashlib.sha256(str(b).encode()).hexdigest()[:16]
                destination = (
                    self.state / "workspaces" / identity / policy / "stage/target"
                )
                shutil.copytree(target, destination)
                if strip:
                    shutil.rmtree(destination / "ci-optim/incremental")
                module = b / "src/module0.rs"
                module.write_text(
                    module.read_text().replace("wrapping_add(0)", "wrapping_add(1)")
                )
                consumers.append(b)
            with concurrent.futures.ThreadPoolExecutor(max_workers=2) as executor:
                futures = [executor.submit(self.command, b, b.name) for b in consumers]
                for future in futures:
                    future.result()
            results = {}
            for b in consumers:
                receipts = self.receipts(b)
                assert receipts and all(item["compilerSucceeded"] for item in receipts)
                inputs = [
                    item["incremental"]["input"]
                    for item in receipts
                    if item.get("incremental")
                ]
                witnesses = self.witnesses(b)
                reused = sum(len(item["reusedProducts"]) for item in witnesses)
                if b.name == "warm":
                    assert inputs and all(inputs) and reused > 0, (inputs, witnesses)
                else:
                    assert inputs and not any(inputs) and reused == 0, (
                        inputs,
                        witnesses,
                    )
                assert self.output(b) == "425"
                results[b.name] = {
                    "inputs": inputs,
                    "witnesses": witnesses,
                    "reused_products": reused,
                    "session": self.last(b)["session"],
                }
                self.command(b, b.name + "-fresh")
                assert not self.receipts(b), "Unchanged rerun recompiled"
                self.command(b, b.name + "-tests", "test")
            warm = consumers[0]
            source = warm / "src/module0.rs"
            valid = source.read_text()
            source.write_text(valid + "\nnot valid rust\n")
            self.command(warm, "compile-error", expected=101)
            self.command(warm, "failure-gc", "gc")
            source.write_text(valid)
            self.command(warm, "recovered")
            assert self.output(warm) == "425"
            normal_units = {
                item["incremental"]["unit"]
                for item in self.receipts(warm)
                if item.get("incremental")
            }
            self.command(warm, "tests-after-recovery", "test")
            target = Path(self.last(warm)["target"])
            corrupted_units = 0
            for receipt in (target / ".dyndrv-delivered/incremental").glob("*.json"):
                record = json.loads(receipt.read_text())
                if record["unit"] in normal_units:
                    cache = next(
                        (target / "ci-optim/incremental" / record["child"]).rglob("*.o")
                    )
                    with cache.open("ab") as output:
                        output.write(b"deliberate cache corruption")
                    corrupted_units += 1
            assert corrupted_units, "No normal build cache was selected"
            source.write_text(valid.replace("wrapping_add(1)", "wrapping_add(2)"))
            corrupted = self.command(warm, "corrupt-state", expected=101)
            assert "incremental state bytes changed" in corrupted.stderr
            rejected = self.command(warm, "rejected-state", expected=101)
            assert "incremental state bytes changed" in rejected.stderr
            self.command(warm, "clean-corrupt-state", "clean")
            source.write_text(valid)
            self.command(warm, "rebuild-after-clean")
            assert self.output(warm) == "425"
            summary = {
                "passed": True,
                "results": results,
                "commands": self.commands,
                "state": str(self.state),
            }
            (self.run / "validation.json").write_text(
                json.dumps(summary, indent=2) + "\n"
            )
            print(self.run / "validation.json", flush=True)
        finally:
            self.command(a, "stop", "stop")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--directory", type=Path, default=ROOT / "target/dyndrv-validation/incremental"
    )
    parser.add_argument("--bootstrap", type=Path, required=True)
    options = parser.parse_args()
    Qualification(options.directory, options.bootstrap).qualify()
