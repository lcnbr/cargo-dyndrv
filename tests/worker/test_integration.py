"""Qualify the real dyndrv entry point with disposable local Cargo workspaces.

Requires Linux user namespaces, Nix, and the pinned dyndrv bootstrap. Evidence,
including the private store, is retained under --directory; owned daemons stop
even after failure. No user's Cargo configuration or source checkout is edited.
"""

import argparse
import contextlib
import ctypes
import fcntl
import hashlib
import itertools
import json
import os
import re
import shutil
import signal
import stat
import subprocess
import sys
import time
import uuid
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from xml.etree import ElementTree

REPOSITORY = Path(__file__).resolve().parents[2]
TOOLING = ("tests/worker/workload.py",)
IMMUTABLE = TOOLING
FORK_LIB = """#[path = "safe.rs"]
mod fresh;

pub fn value() -> String {{
    format!("{value}+{{}}", fresh::value())
}}
"""
APPLICATION_LIB = """use support::support;
use std::env;

fn assert_runtime_environment() {
{workspace_assertion}
    let names = env::var("CARGO_DYNDRV_TEST_RUNTIME_NAMES").unwrap();
    for name in names.split(',').filter(|name| !name.is_empty()) {
        assert!(!env::var(name).unwrap().is_empty(), "Missing runtime value: {name}");
    }
}

pub fn value() -> String {
    assert_runtime_environment();
    format!("{}|{}|{}", itoa::value(), support(), env!("ARBITRARY_NEW_COMPILER_VALUE"))
}

#[cfg(test)]
mod tests {
    use super::assert_runtime_environment;
    use std::env;
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::path::PathBuf;
    use std::process::Command;

    #[test]
    fn runtime_contract() {
        assert_runtime_environment();
        let uid = Command::new("id").arg("-u").output().unwrap();
        let gid = Command::new("id").arg("-g").output().unwrap();
        assert_eq!(String::from_utf8(uid.stdout).unwrap().trim(), env::var("CARGO_DYNDRV_TEST_EXPECTED_UID").unwrap());
        assert_eq!(String::from_utf8(gid.stdout).unwrap().trim(), env::var("CARGO_DYNDRV_TEST_EXPECTED_GID").unwrap());
        let status = fs::read_to_string("/proc/self/status").unwrap();
        for name in ["CapInh:", "CapPrm:", "CapEff:", "CapAmb:"] {
            let line = status.lines().find(|line| line.starts_with(name)).unwrap();
            assert_eq!(u64::from_str_radix(line.split_whitespace().nth(1).unwrap(), 16).unwrap(), 0);
        }
        assert!(!env::var("CARGO_DYNDRV_TEST_RUNTIME_SECRET").unwrap().is_empty());
        if env::var("CARGO_DYNDRV_TEST_EXPECT_LICENSE").unwrap() == "1" {
            assert!(!env::var("CARGO_DYNDRV_TEST_LICENSE").unwrap().is_empty());
        }
        let marker = PathBuf::from(env::var_os("CARGO_TARGET_DIR").unwrap()).join("runtime-count");
        let mut output = OpenOptions::new().create(true).append(true).open(marker).unwrap();
        writeln!(output, "executed").unwrap();
    }
}
"""
APPLICATION_MAIN = """use application::value;
use std::env;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::thread;
use std::time::Duration;

fn main() {
    let value = value();
    if let Ok(path) = env::var("CARGO_DYNDRV_TEST_RUNTIME_GATE") {
        let id = env::var("CARGO_DYNDRV_TEST_RUNTIME_ID").unwrap();
        let mut gate = OpenOptions::new().create(true).append(true).open(path).unwrap();
        writeln!(gate, "start {id}").unwrap();
        let pause: u64 = env::var("CARGO_DYNDRV_TEST_RUNTIME_PAUSE_MS").unwrap().parse().unwrap();
        thread::sleep(Duration::from_millis(pause));
        if let Ok(release) = env::var("CARGO_DYNDRV_TEST_RUNTIME_RELEASE") {
            while !Path::new(&release).exists() {
                thread::sleep(Duration::from_millis(10));
            }
        }
        writeln!(gate, "end {id}").unwrap();
    }
    println!("{value}");
    println!("cwd={}", env::current_dir().unwrap().display());
}
"""


class Harness:
    repository = REPOSITORY
    tooling = TOOLING
    immutable_names = IMMUTABLE

    def __init__(self, args):
        self.options = args
        self.bootstrap = args.bootstrap.resolve() if args.bootstrap else None
        policy = (
            json.loads((self.bootstrap / "environment.json").read_text())
            if self.bootstrap
            else {}
        )
        self.workspace_names = set(
            policy.get(
                "workspace_environment_names",
                ["NEXTEST_WORKSPACE_ROOT", "INSTA_WORKSPACE_ROOT"],
            )
        )
        self.runtime_forbidden = (
            set(policy.get("private_environment_names", []))
            | set(policy.get("runtime_environment_names", []))
        ) - self.workspace_names
        self.timeout = args.timeout
        self.directory = args.directory.absolute()
        self.directory.mkdir(parents=True, exist_ok=True, mode=0o700)
        info = self.directory.lstat()
        if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid():
            raise ValueError("Evidence directory must be an owned, real directory")
        owner = self.directory / ".dyndrv-integration-owner.json"
        identity = {"version": 1, "uid": os.getuid(), "repository": str(REPOSITORY)}
        if owner.exists():
            if json.loads(owner.read_text()) != identity:
                raise ValueError("Evidence directory belongs to another harness/task")
        elif any(self.directory.iterdir()):
            raise ValueError(
                "Refusing to adopt a nonempty, unmarked evidence directory"
            )
        else:
            owner.write_text(json.dumps(identity) + "\n")
        self.run_directory = self.directory / (
            time.strftime("%Y%m%dT%H%M%SZ", time.gmtime()) + "-" + uuid.uuid4().hex[:8]
        )
        self.run_directory.mkdir(mode=0o700)
        self.logs = self.run_directory / "logs"
        self.logs.mkdir()
        self.state = self.run_directory / "state"
        self.a = self.run_directory / "workspace-alpha"
        self.fork_a = self.run_directory / "fork-alpha"
        self.b = self.run_directory / "different-workspace-name"
        self.fork_b = self.run_directory / "different-fork-name"
        self.environment = os.environ.copy()
        self.environment["CARGO_DYNDRV_TEST_LICENSE"] = uuid.uuid4().hex
        self.environment.update(
            {
                "CARGO_DYNDRV_STATE": str(self.state),
                "CARGO_DYNDRV_STORE": "private",
                "CARGO_DYNDRV_TEST_EXPECTED_UID": str(os.getuid()),
                "CARGO_DYNDRV_TEST_EXPECTED_GID": str(os.getgid()),
                "CARGO_DYNDRV_TEST_EXPECT_LICENSE": str(
                    int(bool(self.environment.get("CARGO_DYNDRV_TEST_LICENSE")))
                ),
                "CARGO_DYNDRV_TEST_RUNTIME_SECRET": uuid.uuid4().hex,
                "CARGO_INCREMENTAL": "0",
            }
        )
        self.runtime_forbidden.update(
            prefix + "DYNDRV_PROBE"
            for prefix in policy.get("runtime_environment_prefixes", [])
        )
        for name in self.runtime_forbidden:
            self.environment.setdefault(name, uuid.uuid4().hex)
        self.environment["CARGO_DYNDRV_TEST_RUNTIME_NAMES"] = ",".join(
            sorted(
                name for name in self.runtime_forbidden if self.environment.get(name)
            )
        )
        self.runtime_forbidden.update(
            {
                "CARGO_DYNDRV_TEST_LICENSE",
                "CARGO_DYNDRV_TEST_RUNTIME_SECRET",
            }
        )
        self.secrets = [
            value
            for key, value in self.environment.items()
            if key in self.runtime_forbidden and value
        ]
        self.sequence = itertools.count(1)
        self.commands = []
        self.running = {}
        self.steps = []
        self.summary = {
            "version": 1,
            "directory": str(self.run_directory),
            "state": str(self.state),
            "uid": os.getuid(),
            "license_present": bool(self.environment.get("CARGO_DYNDRV_TEST_LICENSE")),
            "source_hashes": self.hashes(self.repository, self.tooling),
            "steps": self.steps,
            "commands": self.commands,
        }

    @staticmethod
    def parser(description, directory, *, timeout=240, bootstrap_required=True):
        parser = argparse.ArgumentParser(description=description)
        parser.add_argument(
            "--directory",
            type=Path,
            default=REPOSITORY / "target/dyndrv-validation" / directory,
            help="new or harness-owned evidence directory (all runs retained)",
        )
        parser.add_argument(
            "--bootstrap",
            type=Path,
            required=bootstrap_required,
            default=os.environ.get("CARGO_DYNDRV_BOOTSTRAP")
            if not bootstrap_required
            else None,
            help="prepared worker bootstrap output",
        )
        parser.add_argument("--timeout", type=float, default=timeout)
        return parser

    @classmethod
    def run_cli(cls, parser, qualification=None):
        args = parser.parse_args()
        if args.timeout <= 0:
            parser.error("--timeout must be positive")
        harness = cls(args)
        started = time.monotonic()
        status = 1

        def interrupted(signum, _frame):
            raise KeyboardInterrupt(f"interrupted by signal {signum}")

        previous = signal.signal(signal.SIGTERM, interrupted)
        try:
            try:
                if qualification is None:
                    harness.qualify()
                else:
                    qualification(harness)
                status = 0
            except BaseException as error:
                harness.summary["error"] = harness.redact(str(error))
                print(harness.summary["error"], file=sys.stderr)
            finally:
                try:
                    harness.stop()
                except BaseException as error:
                    harness.summary["cleanup_error"] = harness.redact(str(error))
                    print(harness.summary["cleanup_error"], file=sys.stderr)
                    status = 1
                harness.summary.update(
                    passed=status == 0,
                    status="passed" if status == 0 else "failed",
                    seconds=round(time.monotonic() - started, 3),
                )
                harness.save()
                print(
                    f"Evidence: {harness.run_directory / 'validation.json'}", flush=True
                )
        finally:
            signal.signal(signal.SIGTERM, previous)
        return status

    @staticmethod
    def copy_tooling(destination):
        for name in TOOLING:
            target = destination / name
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(REPOSITORY / name, target)

    @staticmethod
    def hashes(root, names):
        return {
            name: hashlib.sha256((root / name).read_bytes()).hexdigest()
            for name in names
        }

    @staticmethod
    def require(condition, message):
        if not condition:
            raise AssertionError(message)

    def redact(self, value):
        for secret in self.secrets:
            value = value.replace(secret, "<redacted>")
        return value

    @contextlib.contextmanager
    def step(self, name):
        started = time.monotonic()
        record = {"name": name, "status": "running"}
        self.steps.append(record)
        print(f"[{len(self.steps)}] {name}", flush=True)
        try:
            yield record
        except BaseException as error:
            record.update(status="failed", error=self.redact(str(error)))
            raise
        else:
            record["status"] = "passed"
        finally:
            record["seconds"] = round(time.monotonic() - started, 3)
            self.save()

    def save(self):
        payload = json.dumps(self.summary, indent=2, sort_keys=True) + "\n"
        (self.run_directory / "validation.json").write_text(payload)
        temporary = self.directory / (".validation-" + self.run_directory.name)
        temporary.write_text(payload)
        temporary.replace(self.directory / "validation.json")

    def command(self, label, arguments, *, cwd=None, env=None, expected=0):
        log = self.logs / f"{next(self.sequence):02d}-{label}"
        started = time.monotonic()
        process = subprocess.Popen(
            [str(argument) for argument in arguments],
            cwd=cwd or self.repository,
            env=env or self.environment,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            start_new_session=True,
        )
        self.running[label] = process
        failure = None
        try:
            stdout, stderr = process.communicate(timeout=self.timeout)
        except (subprocess.SubprocessError, KeyboardInterrupt, OSError) as error:
            failure = error
            with contextlib.suppress(ProcessLookupError):
                os.killpg(process.pid, signal.SIGTERM)
            try:
                stdout, stderr = process.communicate(timeout=10)
            except subprocess.TimeoutExpired:
                with contextlib.suppress(ProcessLookupError):
                    os.killpg(process.pid, signal.SIGKILL)
                stdout, stderr = process.communicate()
        finally:
            self.running.pop(label, None)
        stdout, stderr = self.redact(stdout), self.redact(stderr)
        log.with_suffix(".stdout").write_text(stdout)
        log.with_suffix(".stderr").write_text(stderr)
        record = {
            "label": label,
            "argv": [str(argument) for argument in arguments],
            "status": process.returncode,
            "seconds": round(time.monotonic() - started, 3),
            "stdout": str(log.with_suffix(".stdout")),
            "stderr": str(log.with_suffix(".stderr")),
        }
        self.commands.append(record)
        if failure is not None:
            record["error"] = self.redact(str(failure))
            raise failure
        self.require(
            process.returncode == expected,
            f"{label}: expected exit {expected}, got {process.returncode}; see {record['stderr']}\n"
            + stderr[-4000:],
        )
        return stdout, stderr, record

    def invocation(self, workspace, action, arguments, backend="dyndrv"):
        return [
            self.spec["python"],
            workspace / "tests/worker/workload.py",
            "--bootstrap",
            self.bootstrap,
            "--backend",
            backend,
            "--action",
            action,
            "--",
            *arguments,
        ]

    def public(
        self,
        workspace,
        label,
        action,
        *arguments,
        env=None,
        expected=0,
        backend="dyndrv",
    ):
        stdout, stderr, record = self.command(
            label,
            self.invocation(workspace, action, arguments, backend),
            cwd=workspace,
            env=env,
            expected=expected,
        )
        sessions = re.findall(r"^dyndrv session: (.+)$", stderr, re.MULTILINE)
        if sessions:
            self.require(len(sessions) == 1, "A command reported multiple sessions")
            record["session"] = sessions[0]
            evidence = self.receipts(Path(sessions[0]))
            record["receipts"] = evidence
            # A bootstrap change creates another policy for the same workspace.
            # Use this session's policy rather than requiring a unique policy.
            last = json.loads(
                (Path(sessions[0]).parent.parent / "last-run.json").read_text()
            )
            self.require(last["workspace"] == str(workspace), "Wrong session workspace")
            # A concurrent command can replace last-run.json after this command
            # releases its workspace lock. Its own session remains authoritative.
            cargo_status = json.loads(
                (Path(sessions[0]) / "cargo-result.json").read_text()
            )["status"]
            self.require(
                os.waitstatus_to_exitcode(cargo_status) == expected,
                "Session Cargo status differs from public command",
            )
            if last["session"] == sessions[0]:
                self.require(
                    last["status"] == expected,
                    "Worker status differs from public command",
                )
            record["target"] = last["target"]
        return stdout, record

    def last_run(self, workspace):
        matches = [
            json.loads(path.read_text())
            for path in self.state.glob("workspaces/*/*/last-run.json")
            if json.loads(path.read_text())["workspace"] == str(workspace)
        ]
        self.require(
            len(matches) == 1, f"Expected one workspace record for {workspace}"
        )
        return matches[0]

    def receipts(self, session):
        receipts = []
        for path in sorted(session.glob("invocations/*.nix-result.json")):
            result = json.loads(path.read_text())
            observation = json.loads(
                path.with_name(path.name.replace(".nix-result", "")).read_text()
            )
            arguments = [bytes(value).decode() for value in observation["arguments"]]
            crate = (
                arguments[arguments.index("--crate-name") + 1]
                if "--crate-name" in arguments
                else next(
                    argument.split("=", 1)[1]
                    for argument in arguments
                    if argument.startswith("--crate-name=")
                )
            )
            receipts.append(
                {
                    "crate": crate,
                    "output": result["output"],
                    "derivation": result["derivation"],
                    "compilerSucceeded": result["compilerSucceeded"],
                    "buildSeconds": result["buildSeconds"],
                    "path": str(path),
                }
            )
        return receipts

    @staticmethod
    def write(root, name, content):
        path = root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content)

    def cargo_environment(self, target):
        environment = self.environment | self.spec["environment"]
        environment.update(
            PATH=self.spec["runtime_path"],
            CARGO_TARGET_DIR=str(target),
            CARGO_BUILD_BUILD_DIR=str(target),
            NIX_BUILD_TOP=str(self.run_directory),
        )
        environment.pop("RUSTC_WRAPPER", None)
        return environment

    def lock_and_metadata(self, workspace, label):
        env = self.cargo_environment(self.run_directory / "ordinary-cargo-target")
        self.command(
            label + "-lock",
            [self.spec["cargo"], "generate-lockfile", "--offline"],
            cwd=workspace,
            env=env,
        )
        output, _, _ = self.command(
            label + "-metadata",
            [
                self.spec["cargo"],
                "metadata",
                "--offline",
                "--locked",
                "--format-version",
                "1",
            ],
            cwd=workspace,
            env=env,
        )
        metadata = json.loads(output)
        (self.run_directory / f"{label}-metadata.json").write_text(output)
        return metadata

    def prepare(self):
        self.require(
            sys.platform == "linux" and os.getuid() != 0, "Run as a non-root Linux user"
        )
        if self.bootstrap is None:
            raise ValueError("Pass --bootstrap with a prepared worker environment")
        self.spec = json.loads((self.bootstrap / "environment.json").read_text())
        self.summary["bootstrap"] = str(self.bootstrap)
        self.environment["PATH"] = (
            str(self.bootstrap / "tools/bin") + os.pathsep + self.environment["PATH"]
        )
        self.copy_tooling(self.a)
        self.summary["source_hashes"] = self.hashes(self.a, self.tooling)
        self.write(
            self.a,
            ".config/nextest.toml",
            '[profile.ci_dyndrv]\n\n[profile.ci_dyndrv.junit]\npath = "junit.xml"\n',
        )
        self.write(
            self.a,
            "Cargo.toml",
            """[workspace]
members = ["app", "crates/*"]
resolver = "2"

[profile.ci-optim]
inherits = "dev"
opt-level = 0

[patch.crates-io]
itoa = { path = "../fork-alpha/packages/itoa" }
""",
        )
        self.write(
            self.a,
            "app/Cargo.toml",
            """[package]
name = "application"
version = "0.1.0"
edition = "2021"

[dependencies]
itoa = "=0.1.0"
support = { path = "../crates/support" }
""",
        )
        self.write(
            self.a,
            "app/build.rs",
            'fn main() { println!("cargo:rustc-env=ARBITRARY_NEW_COMPILER_VALUE=ok"); }\n',
        )
        self.write(
            self.a,
            "app/src/lib.rs",
            APPLICATION_LIB.replace(
                "{workspace_assertion}\n",
                (
                    '    let workspace = env::var("INSTA_WORKSPACE_ROOT").expect("Missing workspace layout");\n'
                    '    assert_eq!(option_env!("INSTA_WORKSPACE_ROOT"), Some(workspace.as_str()));\n'
                    if "INSTA_WORKSPACE_ROOT" in self.workspace_names
                    else ""
                ),
            ),
        )
        self.write(self.a, "app/src/main.rs", APPLICATION_MAIN)
        self.write(
            self.a,
            "crates/support/Cargo.toml",
            '[package]\nname = "support"\nversion = "0.1.0"\nedition = "2021"\n',
        )
        self.write(
            self.a,
            "crates/support/src/lib.rs",
            'pub fn support() -> &\'static str { "support" }\n',
        )
        self.write(
            self.a,
            "nix/ci-workspace-graph.json",
            '{"deliberately_stale": "missing-every-fixture-package"}\n',
        )
        self.write(
            self.fork_a,
            "Cargo.toml",
            '[workspace]\nmembers = ["packages/*"]\nresolver = "2"\n\n[workspace.package]\nversion = "0.1.0"\n',
        )
        self.write(
            self.fork_a,
            "packages/itoa/Cargo.toml",
            '[package]\nname = "itoa"\nversion.workspace = true\nedition = "2021"\n',
        )
        self.write(
            self.fork_a,
            "packages/itoa/src/lib.rs",
            'pub fn value() -> String { "tracked-original".into() }\n',
        )
        for root in (self.a, self.fork_a):
            self.command("git-init", ["git", "init", "--quiet", root])
            self.command("git-add", ["git", "add", "."], cwd=root)
            for name in (
                "target/excluded",
                ".venv/excluded",
                ".env",
                "result-excluded",
            ):
                self.write(root, name, "excluded evidence sentinel\n")
        self.edit_fork(self.fork_a, "dirty")
        self.write(
            self.fork_a,
            "packages/itoa/src/untracked.rs",
            'pub fn value() -> &\'static str { "new" }\n',
        )
        (self.fork_a / "packages/itoa/src/safe.rs").symlink_to("untracked.rs")
        self.immutable = self.hashes(self.a, self.immutable_names)
        metadata = self.lock_and_metadata(self.a, "initial")
        itoa = next(
            package for package in metadata["packages"] if package["name"] == "itoa"
        )
        self.require(
            itoa["version"] == "0.1.0" and itoa["source"] is None,
            "Cargo did not resolve local workspace-inherited patch",
        )

    def edit_fork(self, fork, value):
        self.write(fork, "packages/itoa/src/lib.rs", FORK_LIB.format(value=value))

    def run_value(self, workspace, label, value, *, env=None):
        output, record = self.public(
            workspace, label, "run", "-p", "application", env=env
        )
        self.require(
            output.splitlines() == [f"{value}+new|support|ok", f"cwd={workspace}"],
            f"Wrong runtime output: {output}",
        )
        return record

    def stage_hashes(self, workspace):
        stage = Path(self.last_run(workspace)["target"]).parent
        return {
            str(path.relative_to(stage)): hashlib.sha256(path.read_bytes()).hexdigest()
            for root in (stage / "workspace", stage / "sources")
            for path in root.rglob("*")
            if path.is_file()
        }

    def request_policy(self):
        archive = self.run_directory / "compiler-requests"
        archive.mkdir(exist_ok=True)
        # GC may legitimately collect request inputs while retaining delivered
        # artifacts. Keep the actual immutable payloads available for review.
        paths = {
            path.name: path
            for root in (archive, self.state / "store/nix/store")
            for path in root.glob("*-stock-cargo-invocation.json")
        }
        self.require(paths, "No immutable compiler requests found")
        dynamic_value = False
        workspace_value = False
        for path in paths.values():
            raw = path.read_bytes()
            request = json.loads(raw)
            self.require(
                request["incremental"] is None,
                "Incremental state entered qualification request",
            )
            for name, value in request["environment"]:
                name, value = bytes(name).decode(), bytes(value)
                self.require(
                    name in self.workspace_names
                    or (
                        name not in self.runtime_forbidden
                        and not name.startswith(
                            (
                                "CARGO_DYNDRV_",
                                "NEXTEST_",
                                "INSTA_",
                                *self.spec.get("runtime_environment_prefixes", []),
                            )
                        )
                    ),
                    f"Runtime environment name entered compiler request: {name}",
                )
                if name in self.workspace_names:
                    self.require(
                        value
                        == (self.last_run(self.a)["logical"] + "/workspace").encode(),
                        f"Workspace compiler value is not the stable layout: {name}",
                    )
                    workspace_value |= name == "INSTA_WORKSPACE_ROOT"
                self.require(
                    not any(secret.encode() in value for secret in self.secrets),
                    "Runtime secret value entered compiler request",
                )
                dynamic_value |= (
                    name == "ARBITRARY_NEW_COMPILER_VALUE" and value == b"ok"
                )
            self.require(
                not any(secret.encode() in raw for secret in self.secrets),
                "Runtime secret entered request JSON",
            )
            destination = archive / path.name
            if not destination.exists():
                destination.write_bytes(raw)
            self.require(destination.read_bytes() == raw, "Immutable request changed")
        for path in self.state.glob("workspaces/*/*/sessions/**/*.json"):
            raw = path.read_bytes()
            self.require(
                not any(secret.encode() in raw for secret in self.secrets),
                "Runtime secret entered receipt JSON",
            )
        self.require(
            dynamic_value, "Dynamic build-script compiler environment was not captured"
        )
        if "INSTA_WORKSPACE_ROOT" in self.workspace_names:
            self.require(
                workspace_value, "Insta workspace compiler environment was not captured"
            )
        return len(paths)

    def host_bootstrap_root(self, record, label):
        root = Path(record["session"]).parent.parent / "bootstrap-root"
        self.require(
            root.is_symlink() and root.resolve() == self.bootstrap,
            "Installed launcher has no matching host bootstrap root",
        )
        environment = self.environment.copy()
        for name in self.runtime_forbidden:
            environment.pop(name, None)
        output, _, _ = self.command(
            label,
            [
                Path(self.spec["nix"]).with_name("nix-store"),
                "--store",
                "daemon",
                "--gc",
                "--print-roots",
            ],
            env=environment,
        )
        roots = set()
        for line in output.splitlines():
            name, separator, target = line.rpartition(" -> ")
            if separator:
                # Nix prints root pathnames as quoted JSON strings.
                roots.add((json.loads(name) if name.startswith('"') else name, target))
        self.require(
            (str(root), str(self.bootstrap)) in roots,
            "Host bootstrap symlink is not registered as a GC root",
        )
        return str(root)

    def wait_for(self, condition, message):
        deadline = time.monotonic() + self.timeout
        while not condition():
            self.require(time.monotonic() < deadline, message)
            time.sleep(0.05)

    @staticmethod
    def process_info(pid):
        proc = Path(f"/proc/{pid}")
        fields = (proc / "stat").read_text().rsplit(")", 1)[1].split()
        return {
            "pid": pid,
            "starttime": fields[19],
            "state": fields[0],
            "group": int(fields[2]),
            "arguments": [
                os.fsdecode(value)
                for value in (proc / "cmdline").read_bytes().rstrip(b"\0").split(b"\0")
            ],
        }

    @staticmethod
    def children(pid):
        return [
            int(child)
            for child in Path(f"/proc/{pid}/task/{pid}/children").read_text().split()
        ]

    @contextlib.contextmanager
    def adopt_orphans(self):
        # Reap the namespace process ourselves after deliberately killing its
        # worker parent; leave unrelated processes and the shared daemon alone.
        libc = ctypes.CDLL(None, use_errno=True)
        previous = ctypes.c_int()
        if libc.prctl(37, ctypes.byref(previous), 0, 0, 0) or libc.prctl(
            36, 1, 0, 0, 0
        ):
            raise OSError(ctypes.get_errno(), "Could not enable child subreaper")
        try:
            yield
        finally:
            libc.prctl(36, previous.value, 0, 0, 0)

    def hardkill(self, step):
        gate, release = (
            self.run_directory / name for name in ("hardkill-gate", "hardkill-release")
        )
        environment = self.environment | {
            "CARGO_DYNDRV_TEST_RUNTIME_GATE": str(gate),
            "CARGO_DYNDRV_TEST_RUNTIME_RELEASE": str(release),
            "CARGO_DYNDRV_TEST_RUNTIME_ID": "hardkill",
            "CARGO_DYNDRV_TEST_RUNTIME_PAUSE_MS": "0",
        }
        directory = Path(self.last_run(self.a)["target"]).parent.parent
        marker = directory / "in-progress.json"
        daemon_pid = json.loads((self.state / "daemon.json").read_text())["pid"]
        namespace = None
        killed, reaped = False, False
        with self.adopt_orphans(), ThreadPoolExecutor(max_workers=1) as pool:
            future = pool.submit(
                self.command,
                "hardkill-runtime",
                self.invocation(self.a, "run", ("-p", "application")),
                cwd=self.a,
                env=environment,
                expected=137,
            )
            try:

                def reached_gate():
                    if future.done():
                        future.result()
                    return gate.exists() and gate.read_text().splitlines() == [
                        "start hardkill"
                    ]

                self.wait_for(reached_gate, "Hardkill runtime never reached gate")
                parent = self.running["hardkill-runtime"].pid
                workers = []
                for pid in self.children(parent):
                    info = self.process_info(pid)
                    args = info["arguments"]
                    if (
                        self.spec["worker_script"] in args
                        and "--workspace" in args
                        and args[args.index("--workspace") + 1] == str(self.a)
                        and "--action" in args
                        and args[args.index("--action") + 1] == "run"
                    ):
                        workers.append(info)
                self.require(
                    len(workers) == 1, "Could not identify exactly one owned worker"
                )
                worker = workers[0]
                descendants = self.children(worker["pid"])
                self.require(
                    len(descendants) == 1, "Worker has unexpected namespace children"
                )
                namespace = self.process_info(descendants[0])
                self.require(
                    namespace["group"] == namespace["pid"]
                    and namespace["arguments"][0] == self.spec["driver"],
                    "Child is not the owned namespace process group",
                )
                progress = json.loads(marker.read_text())
                self.require(
                    progress["session"] in namespace["arguments"],
                    "Crash marker does not identify active driver session",
                )
                step.update(worker=worker, namespace=namespace, in_progress=progress)
                with os.fdopen(os.pidfd_open(worker["pid"]), "rb") as handle:
                    self.require(
                        self.process_info(worker["pid"])["starttime"]
                        == worker["starttime"],
                        "Worker PID identity changed",
                    )
                    signal.pidfd_send_signal(handle.fileno(), signal.SIGKILL)
                killed = True
                self.wait_for(
                    lambda: not Path(f"/proc/{worker['pid']}").exists(),
                    "Killed worker was not reaped by its shell",
                )
                self.require(
                    self.process_info(namespace["pid"])["starttime"]
                    == namespace["starttime"]
                    and gate.read_text().splitlines() == ["start hardkill"],
                    "Namespace runtime did not survive worker death",
                )
                with (directory / "workspace.lock").open("rb") as lock:
                    try:
                        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    except BlockingIOError:
                        step["workspace_lock_inherited"] = True
                    else:
                        fcntl.flock(lock, fcntl.LOCK_UN)
                        raise AssertionError(
                            "Worker death released the active namespace workspace lock"
                        )
                for action in ("stop", "gc"):
                    _, record = self.public(
                        self.b, "orphan-busy-" + action, action, expected=2
                    )
                    self.require(
                        "still using the daemon" in Path(record["stderr"]).read_text(),
                        "Orphan runtime lost its daemon lease",
                    )
                self.require(
                    json.loads((self.state / "daemon.json").read_text())["pid"]
                    == daemon_pid,
                    "Worker crash replaced the shared daemon",
                )
                release.touch()
                output, _, record = future.result(timeout=self.timeout)
                self.require(
                    output.splitlines() == ["after+new|support|ok", f"cwd={self.a}"]
                    and gate.read_text().splitlines()
                    == ["start hardkill", "end hardkill"],
                    "Orphan runtime did not finish after release",
                )
                status = []

                def reap_namespace():
                    pid, result = os.waitpid(namespace["pid"], os.WNOHANG)
                    if pid:
                        status.append(result)
                    return bool(pid)

                self.wait_for(
                    reap_namespace, "Owned namespace did not exit after gate release"
                )
                reaped = True
                self.require(
                    os.waitstatus_to_exitcode(status[0]) == 0,
                    "Orphan namespace failed after release",
                )
                step["namespace_status"] = 0
                record["session"] = progress["session"]
            finally:
                release.touch()
                if killed and namespace and not reaped:
                    with contextlib.suppress(ProcessLookupError):
                        os.killpg(namespace["group"], signal.SIGKILL)
                    with contextlib.suppress(ChildProcessError):
                        os.waitpid(namespace["pid"], 0)
        self.require(
            marker.exists(), "Hardkill did not preserve interrupted-command marker"
        )
        sessions = set(self.state.glob("workspaces/*/*/sessions/*"))
        _, rejected = self.public(
            self.a,
            "interrupted-target-rejected",
            "build",
            "-p",
            "application",
            expected=2,
        )
        self.require(
            "interrupted command" in Path(rejected["stderr"]).read_text()
            and "clean" in Path(rejected["stderr"]).read_text(),
            "Interrupted target lacked clean-recovery diagnosis",
        )
        self.require(
            set(self.state.glob("workspaces/*/*/sessions/*")) == sessions,
            "Rejected target started another compiler session",
        )
        self.public(self.a, "hardkill-clean", "clean")
        self.require(
            not marker.exists() and not (directory / "bootstrap-root").is_symlink(),
            "Clean kept interrupted marker or host bootstrap root",
        )
        _, rebuilt = self.public(self.a, "hardkill-rebuild", "build", "--workspace")
        self.host_bootstrap_root(rebuilt, "host-roots-after-clean")
        self.run_value(self.a, "hardkill-recovered-runtime", "after")

    def delivered_owners(self, target, crate):
        records = {
            path.name: json.loads(path.read_text())
            for path in (target / ".dyndrv-delivered").glob("*.json")
        }
        owners, metadata = set(), set()
        for record in records.values():
            self.require(record["version"] == 5, "Unexpected target ownership format")
            for backing in record["trees"].values():
                match = re.match(r"/nix/store/[a-z0-9]{32}-[^/]+", backing)
                self.require(match is not None, "Dependency source has no store owner")
                owners.add(match.group())
            for delivered, backing in record["files"].items():
                match = re.match(r"/nix/store/[a-z0-9]{32}-[^/]+", backing)
                self.require(match is not None, "Delivered artifact has no store owner")
                owner = match.group()
                owners.add(owner)
                name = Path(delivered).name
                if name.startswith(f"lib{crate}-") and name.endswith(".rmeta"):
                    metadata.add(owner)
        self.require(metadata, "New dependency has no delivered metadata owner")
        return {
            "owners": sorted(owners),
            "metadata_owners": sorted(metadata),
            "records": records,
        }

    def validate_owners(self, session, owners, label):
        backend = json.loads((Path(session).parent.parent / "backend.json").read_text())
        environment = self.environment.copy()
        for name in self.runtime_forbidden:
            environment.pop(name, None)
        environment.update(
            NIX_CONFIG="experimental-features = nix-command ca-derivations\n",
            NIX_CONF_DIR=str(self.state / "config"),
            NIX_USER_CONF_FILES="",
        )
        # Query validity only: realizing derivations would mask lost metadata.
        output, _, _ = self.command(
            label,
            [
                self.spec["nix"],
                "path-info",
                "--store",
                "unix://" + backend["socket"],
                *owners,
            ],
            env=environment,
        )
        self.require(
            sorted(output.splitlines()) == owners,
            "Delivered store owners are no longer valid",
        )

    def qualify(self):
        with self.step("prepare real Git workspaces and offline Cargo lock"):
            self.prepare()
        with self.step(
            "dirty local patch, untracked module, symlink, dynamic compiler env, original cwd"
        ) as step:
            first = self.run_value(self.a, "patched-run", "dirty")
            self.require(
                first["receipts"], "First build produced no compiler deliveries"
            )
            stage = Path(first["target"]).parent
            for root in (stage / "workspace", *list((stage / "sources").iterdir())):
                for name in (
                    ".git",
                    ".jj",
                    "target",
                    ".venv",
                    ".env",
                    "result-excluded",
                ):
                    self.require(
                        not (root / name).exists(),
                        f"Excluded path was staged: {root / name}",
                    )
            step["compiler_requests"] = self.request_policy()
        with self.step("unchanged Cargo invocation performs no Nix compilations"):
            repeat = self.run_value(self.a, "fresh-repeat", "dirty")
            self.require(
                not repeat["receipts"], "Cargo-fresh rerun produced compiler deliveries"
            )
        with self.step(
            "editing external dependency rebuilds only dependency and consumer"
        ) as step:
            self.edit_fork(self.fork_a, "edited")
            changed = self.run_value(self.a, "dirty-rebuild", "edited")
            names = {receipt["crate"] for receipt in changed["receipts"]}
            self.require(
                names == {"itoa", "application"},
                f"Unexpected recompiled crates: {names}",
            )
            step["recompiled_crates"] = sorted(names)
        with self.step(
            "renamed physical workspaces share identical Nix outputs from empty target"
        ) as step:
            # Rebuild A from an empty Cargo target to compare complete deliveries.
            self.public(self.a, "clean-a", "clean")
            baseline = self.run_value(self.a, "baseline-a", "edited")
            step["host_bootstrap_root"] = self.host_bootstrap_root(
                baseline, "host-bootstrap-roots"
            )
            for source, destination in ((self.a, self.b), (self.fork_a, self.fork_b)):
                shutil.copytree(
                    source,
                    destination,
                    symlinks=True,
                    ignore=shutil.ignore_patterns("target"),
                )
            manifest = self.b / "Cargo.toml"
            manifest.write_text(
                manifest.read_text().replace(
                    "../fork-alpha/", "../different-fork-name/"
                )
            )
            self.lock_and_metadata(self.b, "relocated")
            relocated = self.run_value(self.b, "relocated-b", "edited")
            self.require(
                baseline["target"] != relocated["target"],
                "Physical workspaces share a mutable Cargo target",
            )
            self.require(
                self.stage_hashes(self.a) == self.stage_hashes(self.b),
                "Relocated source stages differ",
            )
            paths = lambda record: sorted(
                (r["crate"], r["output"], r["derivation"]) for r in record["receipts"]
            )
            self.require(
                paths(baseline) and paths(baseline) == paths(relocated),
                "Relocation changed Nix derivations or output paths",
            )
            for receipt in relocated["receipts"]:
                stderr = Path(
                    receipt["path"].replace(".nix-result.json", ".nix-stderr")
                ).read_text()
                self.require(
                    "building" not in stderr.lower(), "Relocation started a Nix builder"
                )
                self.require(receipt["compilerSucceeded"], "Relocated compiler failed")
            step["reused_outputs"] = [entry[1] for entry in paths(relocated)]
        with self.step("Nextest listing agrees with pinned ordinary Cargo"):
            dyndrv, _ = self.public(self.a, "list-dyndrv", "list", "-p", "application")
            ordinary, _ = self.public(
                self.a,
                "list-cargo",
                "list",
                "-p",
                "application",
                backend="cargo",
                env=self.cargo_environment(
                    self.run_directory / "ordinary-cargo-target"
                ),
            )

            def tests(output):
                suites = json.loads(output)["rust-suites"]
                return sorted(
                    (suite["package-name"], name, case["ignored"])
                    for suite in suites.values()
                    for name, case in suite["testcases"].items()
                )

            self.require(
                tests(dyndrv) == tests(ordinary),
                "Pinned Cargo and dyndrv list different tests/statuses",
            )
            self.require(
                any(name == "tests::runtime_contract" for _, name, _ in tests(dyndrv)),
                "Runtime contract was not discovered",
            )
        with self.step(
            "licensed tests execute freshly with invoking UID and no capabilities"
        ) as step:
            marker = Path(self.last_run(self.a)["target"]) / "runtime-count"
            report = self.a / "target/test-reports/dyndrv/ci_dyndrv/junit.xml"
            before = marker.read_text().splitlines() if marker.exists() else []
            self.public(self.a, "tests-first", "test", "-p", "application")
            first_count = len(marker.read_text().splitlines())
            first_report = ElementTree.parse(report).getroot()
            _, second = self.public(self.a, "tests-second", "test", "-p", "application")
            second_report = ElementTree.parse(report).getroot()
            self.require(
                first_count == len(before) + 1
                and len(marker.read_text().splitlines()) == first_count + 1,
                "Cached tests did not execute exactly once per invocation",
            )
            self.require(
                not second["receipts"], "Second test invocation recompiled fresh tests"
            )
            staged_report = (
                Path(second["target"]).parent
                / "workspace/target/nextest/ci_dyndrv/junit.xml"
            )
            self.require(
                report.read_bytes() == staged_report.read_bytes(),
                "Nextest JUnit was not exported from the runtime store directory",
            )
            self.require(
                first_report.attrib["uuid"] != second_report.attrib["uuid"]
                and second_report.attrib["tests"] == "1"
                and second_report.attrib["failures"] == "0"
                and second_report.find(".//testcase").attrib["name"]
                == "tests::runtime_contract",
                "Exported JUnit does not describe a fresh successful runtime test",
            )
            step["junit"] = str(report)
            step["executions"] = 2
            step["compiler_requests"] = self.request_policy()
        with self.step(
            "glob member add, rename, remove ignores stale CI graph and fixed Nix inputs"
        ):
            self.write(
                self.a,
                "crates/added/Cargo.toml",
                '[package]\nname = "added"\nversion = "0.1.0"\nedition = "2021"\n',
            )
            self.write(
                self.a,
                "crates/added/src/lib.rs",
                "#[test]\nfn discovered_member() { assert_eq!(2 + 2, 4); }\n",
            )
            self.lock_and_metadata(self.a, "member-added")
            self.public(self.a, "member-build", "build", "--workspace")
            self.public(self.a, "member-tests", "test", "--workspace")
            (self.a / "crates/added").rename(self.a / "crates/renamed")
            self.lock_and_metadata(self.a, "member-renamed")
            self.public(self.a, "renamed-tests", "test", "--workspace")
            shutil.rmtree(self.a / "crates/renamed")
            metadata = self.lock_and_metadata(self.a, "member-removed")
            self.require(
                {p["name"] for p in metadata["packages"]}
                == {"application", "support", "itoa"},
                "Removed member remains in Cargo metadata",
            )
            self.public(self.a, "removed-build", "build", "--workspace")
            self.require(
                self.hashes(self.a, self.immutable_names) == self.immutable,
                "Workspace changes required changes to flake.lock or Nix expressions",
            )
        with self.step(
            "concurrent physical workspaces keep independent sources and targets"
        ):
            self.edit_fork(self.fork_a, "alpha")
            self.edit_fork(self.fork_b, "beta")
            daemon = json.loads((self.state / "daemon.json").read_text())["pid"]
            with ThreadPoolExecutor(max_workers=2) as pool:
                a = pool.submit(self.run_value, self.a, "concurrent-a", "alpha")
                b = pool.submit(self.run_value, self.b, "concurrent-b", "beta")
                result_a, result_b = a.result(), b.result()
            self.require(
                result_a["target"] != result_b["target"],
                "Concurrent workspaces share a Cargo target",
            )
            self.require(
                json.loads((self.state / "daemon.json").read_text())["pid"] == daemon,
                "Concurrent commands replaced the shared daemon",
            )
        with self.step(
            "same workspace serializes runtime, observes later edits, refuses busy stop and GC"
        ):
            gate = self.run_directory / "runtime-gate"
            release = self.run_directory / "runtime-release"
            first_env = self.environment | {
                "CARGO_DYNDRV_TEST_RUNTIME_GATE": str(gate),
                "CARGO_DYNDRV_TEST_RUNTIME_ID": "one",
                "CARGO_DYNDRV_TEST_RUNTIME_PAUSE_MS": "0",
                "CARGO_DYNDRV_TEST_RUNTIME_RELEASE": str(release),
            }
            second_env = first_env | {
                "CARGO_DYNDRV_TEST_RUNTIME_ID": "two",
                "CARGO_DYNDRV_TEST_RUNTIME_PAUSE_MS": "0",
            }
            with ThreadPoolExecutor(max_workers=2) as pool:
                first = pool.submit(
                    self.run_value, self.a, "serialized-one", "alpha", env=first_env
                )
                deadline = time.monotonic() + self.timeout
                while not gate.exists():
                    if first.done():
                        first.result()
                        raise AssertionError("First runtime never reached gate")
                    self.require(time.monotonic() < deadline, "Runtime gate timed out")
                    time.sleep(0.05)
                self.edit_fork(self.fork_a, "after")
                second = pool.submit(
                    self.run_value, self.a, "serialized-two", "after", env=second_env
                )
                try:
                    for action in ("stop", "gc"):
                        _, record = self.public(
                            self.b, "busy-" + action, action, expected=2
                        )
                        self.require(
                            "still using the daemon"
                            in Path(record["stderr"]).read_text(),
                            f"Busy {action} failed for an unrelated reason",
                        )
                finally:
                    release.touch()
                first.result()
                second.result()
            self.require(
                gate.read_text().splitlines()
                == ["start one", "end one", "start two", "end two"],
                "Same-workspace runtime commands overlapped",
            )
        with self.step(
            "check and Clippy, exit 101, fresh dependency metadata survives failure and idle GC"
        ) as step:
            self.public(self.a, "check", "check", "--workspace")
            self.public(self.a, "clippy", "clippy", "--workspace", "--", "-Dwarnings")
            source = self.a / "app/src/lib.rs"
            original = source.read_text()
            try:
                source.write_text(
                    original
                    + "\npub fn deliberate_clippy_warning() -> u32 { return 1; }\n"
                )
                self.public(
                    self.a,
                    "clippy-failure",
                    "clippy",
                    "-p",
                    "application",
                    "--",
                    "-Dwarnings",
                    expected=101,
                )
            finally:
                source.write_text(original)
            crate = "gc_dependency_" + uuid.uuid4().hex[:12]
            dependency = self.a / "crates" / crate
            manifest = self.a / "app/Cargo.toml"
            original_manifest = manifest.read_text()
            with_dependency = (
                f"use {crate}::value as gc_dependency_value;\n"
                + original.replace(
                    "pub fn value() -> String {\n",
                    "pub fn value() -> String {\n    assert_eq!(gc_dependency_value(), 29);\n",
                )
            )
            self.write(
                dependency,
                "Cargo.toml",
                f'[package]\nname = "{crate}"\nversion = "0.1.0"\nedition = "2021"\n',
            )
            self.write(dependency, "src/lib.rs", "pub fn value() -> u32 { 29 }\n")
            try:
                manifest.write_text(
                    original_manifest + f'{crate} = {{ path = "../crates/{crate}" }}\n'
                )
                self.lock_and_metadata(self.a, "failure-dependency-added")
                source.write_text(with_dependency + "\nthis is not valid Rust\n")
                _, failed = self.public(
                    self.a,
                    "compiler-failure",
                    "build",
                    "-p",
                    "application",
                    "--jobs",
                    "1",
                    expected=101,
                )
                outputs = {
                    receipt["output"]
                    for receipt in failed["receipts"]
                    if receipt["crate"] == crate and receipt["compilerSucceeded"]
                }
                self.require(
                    outputs, "New prerequisite never completed before consumer failure"
                )
                self.require(
                    any(
                        receipt["crate"] == "application"
                        and not receipt["compilerSucceeded"]
                        for receipt in failed["receipts"]
                    ),
                    "Consumer did not reach the intended compiler failure",
                )
                target = Path(failed["target"])
                before = self.delivered_owners(target, crate)
                self.require(
                    not outputs.intersection(before["metadata_owners"]),
                    "Regression did not produce separately addressed metadata",
                )
                (self.run_directory / "failure-before-gc.json").write_text(
                    json.dumps(before, indent=2, sort_keys=True) + "\n"
                )
                self.request_policy()
                self.validate_owners(
                    failed["session"], before["owners"], "failure-owners-before-gc"
                )
                self.public(self.a, "idle-gc-after-failure", "gc")
                after = self.delivered_owners(target, crate)
                (self.run_directory / "failure-after-gc.json").write_text(
                    json.dumps(after, indent=2, sort_keys=True) + "\n"
                )
                self.require(
                    before == after, "Idle GC changed target ownership records"
                )
                self.validate_owners(
                    failed["session"], after["owners"], "failure-owners-after-gc"
                )
                source.write_text(with_dependency)
                _, recovered = self.public(
                    self.a,
                    "compiler-recovered",
                    "build",
                    "-p",
                    "application",
                    "--verbose",
                )
                self.require(
                    all(receipt["crate"] != crate for receipt in recovered["receipts"]),
                    "Recovery recompiled the prerequisite instead of using retained metadata",
                )
                self.require(
                    re.search(
                        rf"\bFresh {crate} v0\.1\.0\b",
                        Path(recovered["stderr"]).read_text(),
                    )
                    is not None,
                    "Cargo did not report the prerequisite as fresh after GC",
                )
                self.run_value(self.a, "recovered-dependency-runtime", "after")
                step.update(
                    new_dependency=crate,
                    retained_metadata_owners=after["metadata_owners"],
                    validated_store_owners=len(after["owners"]),
                )
            finally:
                source.write_text(original)
                manifest.write_text(original_manifest)
                shutil.rmtree(dependency)
            self.lock_and_metadata(self.a, "failure-dependency-removed")
            self.run_value(self.a, "restored-topology", "after")
        with self.step(
            "worker SIGKILL preserves child locks, rejects interrupted target, clean recovers"
        ) as step:
            self.hardkill(step)
        with self.step(
            "unsupported publication fails before compiler session creation"
        ):
            before = set(self.state.glob("workspaces/*/*/sessions/*"))
            for action in ("upload-preview",):
                _, record = self.public(
                    self.a, "unsupported-" + action, action, expected=2
                )
                self.require(
                    "does not support" in Path(record["stderr"]).read_text(),
                    "Unsupported command lacked clear diagnosis",
                )
            self.require(
                set(self.state.glob("workspaces/*/*/sessions/*")) == before,
                "Unsupported command created a compiler session",
            )
            self.require(
                self.hashes(self.a, self.immutable_names) == self.immutable,
                "Fixture Nix inputs changed",
            )
            self.summary["compiler_requests"] = self.request_policy()

    def stop(self):
        if not (self.state / "daemon.json").exists():
            return
        with self.step("stop owned private daemon through public entry point"):
            pid = json.loads((self.state / "daemon.json").read_text())["pid"]
            self.public(self.a, "stop", "stop")
            status = Path(f"/proc/{pid}/status")
            self.require(
                not status.exists() or "\nState:\tZ" in "\n" + status.read_text(),
                "Owned private daemon is still running",
            )
            self.require(
                not (self.state / "daemon.json").exists(),
                "Daemon ownership record remains after stop",
            )


if __name__ == "__main__":
    sys.exit(Harness.run_cli(Harness.parser(__doc__, "integration", timeout=120)))
