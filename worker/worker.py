#!/usr/bin/env python3
"""Own a dyndrv worker; Cargo owns command arguments and compilation decisions."""

import argparse
import contextlib
import ctypes
import fcntl
import filecmp
import hashlib
import itertools
import json
import os
import re
import shlex
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import time
import tomllib
import uuid
from collections import Counter
from pathlib import Path

import tomli_w
from sources import SourceLayout

RUNTIME_NAMES = {
    "HOME",
    "USER",
    "LOGNAME",
    "TMPDIR",
    "TERM",
    "RUST_MIN_STACK",
    "RAYON_NUM_THREADS",
    "CARGO_MAKEFLAGS",
    "RUSTC_WRAPPER",
    "RUSTDOC",
    "NIX_REMOTE",
    "NIX_CONFIG",
    "NIX_CONF_DIR",
    "NIX_USER_CONF_FILES",
    "PWD",
    "OLDPWD",
    "SHLVL",
    "_",
}
RUNTIME_PREFIXES = ("CARGO_DYNDRV_", "NEXTEST_", "INSTA_")


def write_json(path, value):
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_name(path.name + "." + uuid.uuid4().hex)
    temporary.write_text(json.dumps(value, sort_keys=True, indent=2) + "\n")
    temporary.replace(path)


@contextlib.contextmanager
def locked(path, *, shared=False, blocking=True):
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a+b") as stream:
        operation = fcntl.LOCK_SH if shared else fcntl.LOCK_EX
        if not blocking:
            operation |= fcntl.LOCK_NB
        fcntl.flock(stream, operation)
        yield stream


def run(arguments, **kwargs):
    return subprocess.run(arguments, check=True, **kwargs)


def private_directory(path):
    path.mkdir(parents=True, exist_ok=True, mode=0o700)
    info = path.lstat()
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid():
        raise ValueError(f"Not an owned directory: {path}")
    if stat.S_IMODE(info.st_mode) != 0o700:
        raise ValueError(f"Directory must have mode 0700: {path}")


class RunTimings:
    def __init__(self):
        self.started = time.monotonic()
        self.execution_started = None
        self.phases = {}

    @contextlib.contextmanager
    def phase(self, name):
        started = time.monotonic()
        try:
            yield
        finally:
            self.phases[name] = self.phases.get(name, 0.0) + time.monotonic() - started

    def write(self, path, record):
        finished = time.monotonic()
        write_json(
            path,
            {
                **record,
                "seconds": record.get(
                    "seconds",
                    finished - self.execution_started
                    if self.execution_started is not None
                    else 0.0,
                ),
                "total_seconds": finished - self.started,
                "phase_seconds": self.phases,
            },
        )


class Daemon:
    """Access a system daemon or own a private same-user daemon."""

    def __init__(self, state, spec, *, mode="private"):
        self.state = state
        self.spec = spec
        self.mode = mode
        private_directory(state)
        identity = hashlib.sha256(str(state).encode()).hexdigest()[:16]
        if mode == "system":
            self.transport = None
            self.socket = Path("/nix/var/nix/daemon-socket/socket")
            self.store_root = Path("/")
        else:
            self.transport = Path(f"/tmp/cargo-dyndrv-daemon-{os.getuid()}-{identity}")
            private_directory(self.transport)
            self.socket = self.transport / "socket"
            self.store_root = state / "store"
        self.url = "unix://" + str(self.socket)
        for name in ("home", "config", "tmp", "leases", "roots"):
            private_directory(state / name)
        self.environment = {
            "NIX_CONFIG": (
                "experimental-features = nix-command ca-derivations\n"
                "substituters =\nbuilders =\nbuild-users-group =\nrequire-sigs = false\n"
                "sandbox = true\nsandbox-fallback = false\n"
                "max-jobs = auto\ncores = 1\n"
            ),
            "NIX_CONF_DIR": str(state / "config"),
            "NIX_USER_CONF_FILES": "",
            "HOME": str(state / "home"),
            "TMPDIR": str(state / "tmp"),
            "PATH": str(Path(spec["nix"]).parent),
        }
        self.client_environment = self.environment | {
            "NIX_CONFIG": "experimental-features = nix-command ca-derivations\n"
        }

    def healthy(self):
        if not self.socket.exists():
            return False
        try:
            result = subprocess.run(
                [self.spec["nix"], "store", "ping", "--store", self.url],
                env=self.client_environment,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                timeout=3,
                check=False,
            )
            return result.returncode == 0
        except subprocess.TimeoutExpired:
            return False

    @staticmethod
    def owned_process(record):
        try:
            command = Path(f"/proc/{record['pid']}/cmdline").read_bytes()
        except (FileNotFoundError, ProcessLookupError, PermissionError):
            return False
        return command.rstrip(b"\0").split(b"\0") == [
            os.fsencode(value) for value in record["command"]
        ]

    def start(self):
        if self.healthy():
            return
        if self.mode == "system":
            raise RuntimeError(
                "The administrator-managed system Nix daemon is unavailable"
            )
        record = self.state / "daemon.json"
        if record.exists():
            data = json.loads(record.read_text())
            if self.owned_process(data):
                raise RuntimeError(
                    "The recorded daemon is not responding; stop the owned worker daemon before retrying"
                )
            record.unlink()
        self.socket.unlink(missing_ok=True)
        with (self.state / "daemon.log").open("ab") as log:
            command = [
                self.spec["nix"],
                "daemon",
                "--store",
                "local?root=" + str(self.state / "store"),
            ]
            process = subprocess.Popen(
                command,
                env=self.environment | {"NIX_DAEMON_SOCKET_PATH": str(self.socket)},
                stdin=subprocess.DEVNULL,
                stdout=log,
                stderr=subprocess.STDOUT,
                start_new_session=True,
            )
        write_json(self.state / "daemon.json", {"pid": process.pid, "command": command})
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            if process.poll() is not None:
                raise RuntimeError(
                    f"Private Nix daemon failed; see {self.state / 'daemon.log'}"
                )
            if self.healthy():
                return
            time.sleep(0.05)
        os.killpg(process.pid, signal.SIGTERM)
        process.wait(timeout=10)
        raise RuntimeError("Private Nix daemon startup timed out")

    @contextlib.contextmanager
    def lease(self):
        marker = self.state / "leases" / uuid.uuid4().hex
        with locked(self.state / "daemon.lock"):
            self.start()
            lease = marker.open("xb")
            fcntl.flock(lease, fcntl.LOCK_EX)
        try:
            yield lease
        finally:
            lease.close()
            marker.unlink(missing_ok=True)

    def provision(self, paths, owner):
        if self.mode == "system":
            # Inputs already live in this store; keep them through compilation.
            self.retain(paths, owner)
            return
        # Both endpoints are explicit. Compiler requests themselves remain offline.
        with locked(self.state / "provision.lock"):
            run(
                [
                    self.spec["nix"],
                    "copy",
                    "--from",
                    "daemon",
                    "--to",
                    self.url,
                    *paths,
                ],
                env=self.client_environment,
            )
        # Launchers also use the current vendor mounts after a failed command.
        # Keep runtime inputs alive even when no successful snapshot is published.
        self.retain(paths, owner)

    def retain(self, references, owner, *, replace=False):
        directory = self.state / "roots" / owner
        directory.mkdir(parents=True, exist_ok=True, mode=0o700)
        references = set(references)
        if references:
            run(
                [
                    self.spec["nix"],
                    "path-info",
                    "--store",
                    self.url,
                    *sorted(references),
                ],
                env=self.client_environment,
                stdout=subprocess.DEVNULL,
            )
        store = str(Path(self.spec["nix"]).with_name("nix-store"))
        for path in sorted(references):
            root = directory / Path(path).name
            if root.is_symlink() and os.readlink(root) == path:
                continue
            run(
                [
                    store,
                    "--store",
                    self.url,
                    "--realise",
                    path,
                    "--add-root",
                    str(root),
                    "--indirect",
                ],
                env=self.client_environment,
                stdout=subprocess.DEVNULL,
            )
        if replace:
            retained = {Path(path).name for path in references}
            for root in directory.iterdir():
                if root.name not in retained:
                    root.unlink()

    def require_idle(self):
        for marker in (self.state / "leases").iterdir():
            try:
                with locked(marker, blocking=False):
                    pass
            except BlockingIOError:
                raise RuntimeError(
                    "A dyndrv command is still using the daemon"
                ) from None
            marker.unlink(missing_ok=True)

    def stop(self):
        if self.mode == "system":
            raise ValueError(
                "The system Nix daemon is administrator-managed, not by this workspace"
            )
        with locked(self.state / "daemon.lock"):
            self.require_idle()
            record = self.state / "daemon.json"
            if not record.exists():
                return
            data = json.loads(record.read_text())
            if self.owned_process(data):
                os.killpg(data["pid"], signal.SIGTERM)
                deadline = time.monotonic() + 10
                while self.owned_process(data) and time.monotonic() < deadline:
                    time.sleep(0.05)
                if self.owned_process(data):
                    os.killpg(data["pid"], signal.SIGKILL)
                    deadline = time.monotonic() + 10
                    while self.owned_process(data) and time.monotonic() < deadline:
                        time.sleep(0.05)
                    if self.owned_process(data):
                        raise RuntimeError(
                            "Owned daemon did not stop; its record was preserved"
                        )
            record.unlink(missing_ok=True)
            self.socket.unlink(missing_ok=True)


def namespace(configuration, arguments):
    """Mount before exec; the nonzero invoking UID loses namespace capabilities."""
    uid, gid = os.getuid(), os.getgid()
    if uid == 0:
        raise ValueError("Run the development worker as the invoking non-root user")
    os.unshare(os.CLONE_NEWUSER | os.CLONE_NEWNS)
    Path("/proc/self/setgroups").write_text("deny")
    Path("/proc/self/uid_map").write_text(f"{uid} {uid} 1\n")
    Path("/proc/self/gid_map").write_text(f"{gid} {gid} 1\n")
    libc = ctypes.CDLL(None, use_errno=True)
    libc.mount.argtypes = [
        ctypes.c_char_p,
        ctypes.c_char_p,
        ctypes.c_char_p,
        ctypes.c_ulong,
        ctypes.c_void_p,
    ]

    def mount(source, target, flags):
        if libc.mount(
            os.fsencode(source) if source else None,
            os.fsencode(target),
            None,
            flags,
            None,
        ):
            number = ctypes.get_errno()
            raise OSError(number, os.strerror(number), str(target))

    mount(None, "/", 16384 | 1 << 18)  # MS_REC | MS_PRIVATE
    mount(configuration["stage"], configuration["logical"], 4096 | 16384)
    if configuration["store"] != "/nix/store":
        mount(configuration["store"], "/nix/store", 4096 | 16384)
    for binding in configuration.get("packages", []):
        mount(binding["source"], binding["target"], 4096)
        # A user namespace cannot clear flags locked by the parent mount.
        locked_flags = os.statvfs(binding["target"]).f_flag & (2 | 4 | 8)
        mount(None, binding["target"], 4096 | 32 | 1 | locked_flags)
    if configuration["cwd"] is not None:
        os.chdir(configuration["cwd"])
    os.environ.update(configuration.get("environment", {}))
    os.execvpe(arguments[0], arguments, os.environ)


class Worker:
    def __init__(self, bootstrap, workspace, action, arguments):
        self.bootstrap = bootstrap
        self.spec = json.loads((bootstrap / "environment.json").read_text())
        self.source_overlay = self.spec.get("source_overlay", False)
        if not isinstance(self.source_overlay, bool):
            # Bootstrap errors use the CLI's configuration diagnostic handler.
            raise ValueError("source_overlay must be a boolean")  # noqa: TRY004
        self.runtime_names = RUNTIME_NAMES | set(
            self.spec.get("runtime_environment_names", [])
        )
        self.runtime_prefixes = RUNTIME_PREFIXES + tuple(
            self.spec.get("runtime_environment_prefixes", [])
        )
        self.private_names = set(self.spec.get("private_environment_names", []))
        self.runtime_names |= self.private_names
        self.workspace_names = set(
            self.spec.get(
                "workspace_environment_names",
                ["NEXTEST_WORKSPACE_ROOT", "INSTA_WORKSPACE_ROOT"],
            )
        )
        if overlap := self.workspace_names & self.private_names:
            raise ValueError(
                "Workspace environment names cannot be private: "
                + ", ".join(sorted(overlap))
            )
        self.layout_names = self.workspace_names - set(
            self.spec.get("forward_environment_names", [])
        )
        self.workspace = workspace.resolve()
        self.action = action
        self.arguments = arguments
        state = os.environ.get("CARGO_DYNDRV_STATE")
        self.state = (
            Path(state)
            if state
            else Path(
                os.environ.get("XDG_STATE_HOME", str(Path.home() / ".local/state"))
            )
            / "cargo-dyndrv"
        )
        self.state = self.state.absolute()
        settings = {}
        config_path = Path(
            os.environ.get("CARGO_DYNDRV_STORE_CONFIG", "/etc/cargo-dyndrv/store.json")
        )
        if config_path.is_file():
            settings = json.loads(config_path.read_text())
            if settings.get("version") != 1:
                raise ValueError("Unsupported cargo-dyndrv system store configuration")
        elif "CARGO_DYNDRV_STORE_CONFIG" in os.environ:
            raise ValueError(f"Store configuration does not exist: {config_path}")
        mode = os.environ.get(
            "CARGO_DYNDRV_STORE", settings.get("store_mode", "private")
        )
        if mode not in {"private", "system"}:
            raise ValueError("CARGO_DYNDRV_STORE must be private or system")
        self.daemon = Daemon(self.state, self.spec, mode=mode)
        if mode == "system":
            self.logical = Path(settings.get("logical_root", "/build/cargo-dyndrv"))
            info = self.logical.lstat()
            if (
                not stat.S_ISDIR(info.st_mode)
                or info.st_uid != 0
                or info.st_mode & 0o022
            ):
                raise ValueError(
                    "System store source mount must be an administrator-owned directory"
                )
        else:
            self.logical = Path(f"/tmp/cargo-dyndrv-{os.getuid()}")
            private_directory(self.logical)
        self.client = {
            name: value
            for name, value in os.environ.items()
            if name not in self.private_names
        }
        self.client["NIX_CONFIG"] = (
            "experimental-features = nix-command flakes ca-derivations\n"
        )
        self.environment = self.spec["environment"].copy()
        # Runtime values are deliberately absent from the compiler policy.
        for name, value in os.environ.items():
            if name not in self.workspace_names and (
                name in self.runtime_names
                or name.startswith((*self.runtime_prefixes, "CARGO_PROFILE_"))
                or name in self.spec.get("forward_environment_names", [])
                or name
                in {
                    "RUSTFLAGS",
                    "RUSTDOCFLAGS",
                    "CARGO_ENCODED_RUSTFLAGS",
                    "RUSTC_BOOTSTRAP",
                    "CARGO_BUILD_JOBS",
                    "CARGO_INCREMENTAL",
                    "CARGO_BUILD_INCREMENTAL",
                }
            ):
                self.environment[name] = value
        for name in (
            "RUSTC_WRAPPER",
            "NIX_REMOTE",
            "NIX_CONFIG",
            "NIX_CONF_DIR",
            "NIX_USER_CONF_FILES",
            "PWD",
            "OLDPWD",
            "SHLVL",
            "_",
        ):
            self.environment.pop(name, None)
        self.environment.update(
            {
                "NIX_BUILD_TOP": "/build",
            }
        )

    def staging_environment(self):
        environment = (
            os.environ | self.spec["environment"] | {"PATH": self.spec["runtime_path"]}
        )
        return {
            name: value
            for name, value in environment.items()
            if name not in self.private_names
        }

    def compiler_environment_name(self, name, runtime_names):
        # Layout values are rebound before Cargo starts, including names whose
        # prefixes otherwise classify caller values as runtime-only.
        return name in self.workspace_names or (
            name not in runtime_names and not name.startswith(self.runtime_prefixes)
        )

    @staticmethod
    def source_roots(packages):
        paths = {raw: Path(raw) for raw in packages}
        # Distinct raw spellings of the same lexical path cover each other, too.
        roots = Counter(paths.values())
        return {
            raw
            for raw, path in paths.items()
            if roots[path] == 1 and not any(parent in roots for parent in path.parents)
        }

    def vendor_layout(self, vendor, stage):
        """Expose each complete vendored package at a lock-independent path.

        Cargo's directory-source index may change with the lockfile. Only the
        package mounts become compiler trees; their bytes are snapshotted by the
        driver. Bind mounts avoid copying the vendor closure into every stage.
        """
        configuration = tomllib.loads((vendor / "config.toml").read_text())
        packages = []
        for name, source in configuration.get("source", {}).items():
            if "directory" not in source:
                continue
            identity = hashlib.sha256(name.encode()).hexdigest()
            relative = Path("vendor") / identity
            directory = Path(source["directory"])
            for package in sorted(directory.iterdir()):
                if not (package / "Cargo.toml").is_file():
                    raise ValueError(f"Invalid vendored Cargo package: {package}")
                packages.append(
                    {
                        "source": str(package.resolve(strict=True)),
                        "target": str(self.logical / relative / package.name),
                    }
                )
            source["directory"] = str(self.logical / relative)
        # Only empty mount points live here. The workspace lock excludes active
        # commands and launchers while the lockfile's directory index is replaced.
        shutil.rmtree(stage / "vendor", ignore_errors=True)
        for source in configuration.get("source", {}).values():
            if "directory" in source:
                (stage / Path(source["directory"]).relative_to(self.logical)).mkdir(
                    parents=True, exist_ok=True
                )
        for package in packages:
            (stage / Path(package["target"]).relative_to(self.logical)).mkdir()
        return configuration, packages

    @staticmethod
    def package_store_inputs(packages, package_sources):
        return {
            package["target"]: package["source"]
            for package in packages
            if package["target"] in package_sources
            and re.fullmatch(r"/nix/store/[^/]+", package["source"])
        }

    def vendor_input(self, lock):
        """Cache only providers determined by lock bytes and immutable inputs.

        The opt-in excludes dependence on the lock pathname, caller environment
        and mutable imports. Host store queries use the same client as builds.
        """
        if self.spec.get("vendor_lockfile_only") is not True:
            return self.build_vendor_input(lock)
        nix = str(Path(self.spec["nix"]).resolve())
        contents = lock.read_bytes()
        key = hashlib.sha256(
            json.dumps(
                {
                    "version": 1,
                    "lock": hashlib.sha256(contents).hexdigest(),
                    "expression": self.spec["vendor_expression"],
                    "nix": [self.spec["nix"], nix],
                },
                sort_keys=True,
            ).encode()
        ).hexdigest()
        directory = self.state / "vendor-inputs"
        private_directory(directory)
        cache = directory / (key + ".json")
        store_path = r"/nix/store/[0-9abcdfghijklmnpqrsvwxyz]{32}-[A-Za-z0-9+._?=-]+"
        with locked(directory / (key + ".lock")):
            try:
                entry = json.loads(cache.read_text())
            except (OSError, ValueError):
                entry = None
            if (
                isinstance(entry, dict)
                and entry.get("version") == 1
                and entry.get("key") == key
                and isinstance(entry.get("path"), str)
                and re.fullmatch(store_path, entry["path"])
                and subprocess.run(
                    [nix, "path-info", entry["path"]],
                    env=self.client,
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                    check=False,
                ).returncode
                == 0
            ):
                return Path(entry["path"])
            # A concurrent lockfile edit must not populate the old content key.
            with tempfile.TemporaryDirectory(
                dir=directory, prefix=key + "."
            ) as temporary:
                snapshot = Path(temporary) / "Cargo.lock"
                snapshot.write_bytes(contents)
                vendor = self.build_vendor_input(snapshot, nix=nix)
            if re.fullmatch(store_path, str(vendor)):
                write_json(cache, {"version": 1, "key": key, "path": str(vendor)})
            return vendor

    def build_vendor_input(self, lock, *, nix=None):
        environment = self.client | {"CARGO_DYNDRV_CARGO_LOCK": str(lock)}
        return Path(
            subprocess.check_output(
                [
                    nix or self.spec["nix"],
                    "build",
                    "--impure",
                    "--no-link",
                    "--print-out-paths",
                    "--expr",
                    self.spec["vendor_expression"],
                ],
                env=environment,
                text=True,
            ).splitlines()[-1]
        )

    def original_manifest(self):
        manifest = self.workspace / "Cargo.toml"
        for index, argument in enumerate(self.arguments):
            if argument == "--":
                break
            if argument == "--manifest-path":
                manifest = Path(self.arguments[index + 1])
            elif argument.startswith("--manifest-path="):
                manifest = Path(argument.split("=", 1)[1])
        return manifest.absolute()

    def original_metadata(self, *, locate=False):
        manifest = self.original_manifest()
        env = self.staging_environment()
        return json.loads(
            subprocess.check_output(
                [
                    self.spec["cargo"],
                    *(
                        ["locate-project", "--workspace"]
                        if locate
                        else [
                            "metadata",
                            "--format-version",
                            "1",
                            "--all-features",
                            "--locked",
                        ]
                    ),
                    "--manifest-path",
                    str(manifest),
                ],
                cwd=self.workspace,
                env=env,
                text=True,
            )
        )

    def mapped_command(self, roots):
        arguments = list(self.arguments)
        for index, argument in enumerate(arguments):
            if argument == "--":
                break
            if argument in {"--target-dir", "--build-dir"} or argument.startswith(
                ("--target-dir=", "--build-dir=")
            ):
                raise ValueError(
                    "dyndrv owns its target/build directories; use its persistent workspace target"
                )
            if argument == "--manifest-path":
                arguments[index + 1] = self.map_path(
                    Path(arguments[index + 1]).absolute(), roots
                )
            elif argument.startswith("--manifest-path="):
                arguments[index] = "--manifest-path=" + self.map_path(
                    Path(argument.split("=", 1)[1]).absolute(), roots
                )
        return arguments

    @staticmethod
    def map_path(path, roots):
        for root in sorted(roots, key=lambda root: len(root["original"]), reverse=True):
            try:
                return str(Path(root["logical"]) / path.relative_to(root["original"]))
            except ValueError:
                pass
        raise ValueError(f"Path is outside Cargo's discovered roots: {path}")

    def run_namespace(self, config, command, *, capture=False):
        invocation = [
            self.spec["python"],
            str(Path(__file__).resolve()),
            "namespace",
            str(config),
            "--",
            *command,
        ]
        process = subprocess.Popen(
            invocation,
            env=self.environment,
            stdout=subprocess.PIPE if capture else None,
            text=True,
            start_new_session=True,
            pass_fds=getattr(self, "lock_fds", ()),
        )
        try:
            output, _ = process.communicate()
        except BaseException:
            os.killpg(process.pid, signal.SIGTERM)
            try:
                process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()
            raise
        if process.returncode:
            raise subprocess.CalledProcessError(process.returncode, invocation)
        return output

    def install_binaries(self, metadata, directory, target, namespace_config):
        run(
            [
                str(Path(self.spec["nix"]).with_name("nix-store")),
                "--realise",
                str(self.bootstrap),
                "--add-root",
                str(directory / "bootstrap-root"),
                "--indirect",
            ],
            env=self.client,
            stdout=subprocess.DEVNULL,
        )
        names = {
            item["name"]
            for package in metadata["packages"]
            if package["id"] in metadata["workspace_members"]
            for item in package["targets"]
            if "bin" in item["kind"]
        }
        config = json.loads(namespace_config.read_text())
        config["cwd"] = None
        config["environment"] = dict.fromkeys(
            self.workspace_names, str(self.logical / "workspace")
        )
        launch_config = directory / "launch.json"
        write_json(launch_config, config)
        record = directory / "installed.json"
        installed = json.loads(record.read_text()) if record.exists() else {}
        for name in sorted(names):
            for binary in sorted(
                {*target.glob(f"*/{name}"), *target.glob(f"*/*/{name}")}
            ):
                relative = binary.relative_to(target)
                if not binary.is_file() or not os.access(binary, os.X_OK):
                    continue
                if any(
                    part in {"build", "deps", "examples", ".dyndrv-delivered"}
                    for part in relative.parts
                ):
                    continue
                retained = directory / "installed" / relative
                retained.parent.mkdir(parents=True, exist_ok=True)
                if not retained.is_file() or not filecmp.cmp(
                    binary, retained, shallow=False
                ):
                    temporary = retained.with_name(
                        retained.name + "." + uuid.uuid4().hex
                    )
                    shutil.copy2(binary, temporary)
                    temporary.replace(retained)
                destination = Path(metadata["workspace_root"]) / "target" / relative
                body = (
                    f"#!{self.bootstrap}/tools/bin/bash\n"
                    "# cargo-dyndrv launcher; build resources live in its source namespace.\n"
                    "set -euo pipefail\n"
                    f"exec 9> {shlex.quote(str(directory / 'workspace.lock'))}\n"
                    f"{shlex.quote(str(self.bootstrap / 'tools/bin/flock'))} -s 9\n"
                    "exec "
                    + shlex.join(
                        [
                            self.spec["python"],
                            str(Path(__file__).resolve()),
                            "namespace",
                            str(launch_config),
                            "--",
                            str(retained),
                        ]
                    )
                    + ' "$@"\n'
                )
                destination.parent.mkdir(parents=True, exist_ok=True)
                profile = relative.parts[-2]
                with locked(
                    Path(metadata["workspace_root"])
                    / "target/nix"
                    / f".install-{profile}.lock"
                ):
                    if (
                        not destination.is_file()
                        or destination.open("rb").read(len(body.encode()) + 1)
                        != body.encode()
                    ):
                        temporary = destination.with_name(
                            "." + destination.name + "." + uuid.uuid4().hex
                        )
                        temporary.write_text(body)
                        temporary.chmod(0o755)
                        temporary.replace(destination)
                installed[str(destination)] = body
        write_json(record, installed)

    def retain_failed(self, session, target, owner):
        references = set()
        for path in session.rglob("*.nix-result.json"):
            result = json.loads(path.read_text())
            if result.get("compilerSucceeded"):
                references.add(result["output"])
        # Metadata has its own content-addressed owner, separate from a compiler
        # result. Keep every backing mapping until the fork can validate a full
        # successful snapshot; Cargo may consider these partial products fresh.
        for path in (target / ".dyndrv-delivered").glob("*.json"):
            receipt = json.loads(path.read_text())
            if receipt.get("version") != 5:
                raise ValueError(f"Unsupported dyndrv artifact receipt: {path}")
            for backing in itertools.chain(
                receipt["files"].values(), receipt["trees"].values()
            ):
                path = Path(backing)
                if (
                    not path.is_relative_to("/nix/store")
                    or ".." in path.parts
                    or len(path.parts) < 4
                ):
                    raise ValueError("Artifact backing is outside the immutable store")
                references.add(str(Path(*path.parts[:4])))
        for path in (target / ".dyndrv-delivered/incremental").glob("*.json"):
            receipt = json.loads(path.read_text())
            if receipt.get("version") != 1:
                raise ValueError(f"Unsupported incremental artifact receipt: {path}")
            backing = Path(receipt["input"])
            if (
                not backing.is_relative_to("/nix/store")
                or ".." in backing.parts
                or len(backing.parts) < 4
            ):
                raise ValueError("Incremental backing is outside the immutable store")
            references.add(str(Path(*backing.parts[:4])))
        self.daemon.retain(references, owner)

    def run_post_commands(self, namespace_config, layout):
        for hook in self.spec.get("post_commands", {}).get(self.action, []):
            self.run_namespace(
                namespace_config,
                [
                    argument.replace("{workspace}", layout["workspace"]).replace(
                        "{bootstrap}", str(self.bootstrap)
                    )
                    for argument in hook
                ]
                + self.mapped_command(layout["roots"]),
            )

    def run(self):
        timings = RunTimings()
        actions = {
            "build",
            "run",
            "check",
            "clippy",
            "test",
            "list",
            "doc",
            "fmt",
            "clean",
            "gc",
            "stop",
        }
        if self.action not in actions | self.spec.get("action_executables", {}).keys():
            raise ValueError(f"dyndrv does not support {self.action}")
        if self.action == "stop":
            self.daemon.stop()
            return
        if self.action == "gc":
            if self.daemon.mode == "system":
                raise ValueError(
                    "System store collection is administrator-managed; clean only your workspace roots"
                )
            with locked(self.state / "daemon.lock"):
                self.daemon.require_idle()
                self.daemon.start()
                run(
                    [self.spec["nix"], "store", "gc", "--store", self.daemon.url],
                    env=self.daemon.client_environment,
                )
            return
        if self.action == "fmt":
            run(
                [self.spec["cargo"], *self.arguments],
                cwd=self.workspace,
                env=self.staging_environment(),
            )
            return
        with timings.phase("original_metadata"):
            metadata = self.original_metadata(locate=self.action == "clean")
        workspace = (
            Path(metadata["root"]).parent
            if self.action == "clean"
            else Path(metadata["workspace_root"])
        )
        policy = hashlib.sha256(
            json.dumps(
                {"store": self.daemon.mode, "logical": str(self.logical)},
                sort_keys=True,
            ).encode()
            + b"\0".join(
                path.read_bytes()
                for path in (
                    self.bootstrap / "environment.json",
                    Path(__file__).resolve(),
                    Path(__file__).with_name("sources.py").resolve(),
                )
            )
        ).hexdigest()[:16]
        identity = hashlib.sha256(str(workspace).encode()).hexdigest()[:16]
        owner = identity + "/" + policy
        directory = self.state / "workspaces" / identity / policy
        directory.mkdir(parents=True, exist_ok=True)
        with (
            locked(directory / "workspace.lock") as workspace_lock,
            contextlib.ExitStack() as cleanup,
        ):
            in_progress = directory / "in-progress.json"
            if self.action == "clean":
                shutil.rmtree(directory / "stage" / "target", ignore_errors=True)
                shutil.rmtree(self.state / "roots" / owner, ignore_errors=True)
                record = directory / "installed.json"
                if record.exists():
                    for name, body in json.loads(record.read_text()).items():
                        destination = Path(name)
                        if (
                            destination.is_file()
                            and destination.open("rb").read(len(body.encode()) + 1)
                            == body.encode()
                        ):
                            destination.unlink()
                    record.unlink()
                shutil.rmtree(directory / "installed", ignore_errors=True)
                (directory / "bootstrap-root").unlink(missing_ok=True)
                in_progress.unlink(missing_ok=True)
                return
            if in_progress.exists():
                raise ValueError(
                    "dyndrv target has an interrupted command; run the application's dyndrv clean action before retrying"
                )
            stage = directory / "stage"
            target = stage / "target"
            session = directory / "sessions" / uuid.uuid4().hex
            record = {
                "session": str(session),
                "status": 1,
                "workspace": str(workspace),
                "target": str(target),
                "logical": str(self.logical),
            }
            cleanup.callback(timings.write, directory / "last-run.json", record)
            stage.mkdir(exist_ok=True)
            with timings.phase("vendor_input"):
                vendor = self.vendor_input(workspace / "Cargo.lock")
            original_home = Path(
                os.environ.get("CARGO_HOME", str(Path.home() / ".cargo"))
            )
            directories = [original_home]
            for parent in reversed(workspace.parents):
                directories.append(parent / ".cargo")
            # Cargo prefers its legacy filename when both names are present.
            candidates = [
                directory
                / ("config" if (directory / "config").is_file() else "config.toml")
                for directory in directories
            ]
            candidates = [path for path in candidates if path.is_file()]
            source_layout = SourceLayout(
                workspace,
                stage,
                self.logical,
                self.spec["cargo"],
                env=self.staging_environment(),
                configurations=candidates,
            )
            with timings.phase("source_layout"):
                layout = source_layout.prepare(metadata)
            cargo_home = stage / "cargo-home"
            cargo_home.mkdir(exist_ok=True)
            configuration = {}

            def merge(target, update):
                for name, value in update.items():
                    if isinstance(value, dict) and isinstance(target.get(name), dict):
                        merge(target[name], value)
                    else:
                        target[name] = value

            for path in candidates:
                if path.is_file():
                    value = tomllib.loads(
                        source_layout.configuration(
                            path, path.read_bytes(), layout["roots"]
                        ).decode()
                    )
                    value.get("registry", {}).pop("token", None)
                    for registry in value.get("registries", {}).values():
                        registry.pop("token", None)
                    merge(configuration, value)
            with timings.phase("vendor_layout"):
                vendor_configuration, vendor_packages = self.vendor_layout(
                    vendor, stage
                )
            merge(configuration, vendor_configuration)
            merged = tomli_w.dumps(configuration)
            if (
                not (cargo_home / "config.toml").exists()
                or (cargo_home / "config.toml").read_text() != merged
            ):
                (cargo_home / "config.toml").write_text(merged)
            target.mkdir(exist_ok=True)
            if (target / ".dyndrv-delivered/rejected").exists():
                raise ValueError(
                    "dyndrv target was rejected; run the application's dyndrv clean action before retrying"
                )
            (stage / "tmp").mkdir(exist_ok=True)
            (stage / "home").mkdir(exist_ok=True)
            self.environment.update(
                {
                    "HOME": str(self.logical / "home"),
                    "TMPDIR": str(self.logical / "tmp"),
                    "CARGO_HOME": str(self.logical / "cargo-home"),
                    "CARGO_TARGET_DIR": str(self.logical / "target"),
                    "CARGO_BUILD_BUILD_DIR": str(self.logical / "target"),
                    "NIX_CONFIG": "experimental-features = nix-command ca-derivations\n",
                    "NIX_CONF_DIR": str(self.daemon.state / "config"),
                    "NIX_USER_CONF_FILES": "",
                    **dict.fromkeys(self.workspace_names, layout["workspace"]),
                }
            )
            runtime_names = set(self.runtime_names)
            if self.action == "run":
                runner = stage / "runtime-runner.py"
                runner.write_text(
                    "import os,sys\nos.chdir(os.environ['CARGO_DYNDRV_ORIGINAL_CWD'])\nos.execvpe(sys.argv[1],sys.argv[1:],os.environ)\n"
                )
                self.environment["CARGO_DYNDRV_ORIGINAL_CWD"] = str(self.workspace)
                host = (
                    subprocess.check_output(
                        [self.environment["RUSTC"], "-vV"], text=True
                    )
                    .split("host: ")[1]
                    .splitlines()[0]
                )
                runner_name = (
                    "CARGO_TARGET_" + host.upper().replace("-", "_") + "_RUNNER"
                )
                self.environment[runner_name] = (
                    self.spec["python"] + " " + str(self.logical / runner.name)
                )
                runtime_names.add(runner_name)
            session.parent.mkdir(exist_ok=True)
            namespace_config = directory / "namespace.json"
            write_json(
                namespace_config,
                {
                    "logical": str(self.logical),
                    "stage": str(stage),
                    "store": str(self.daemon.store_root / "nix/store"),
                    "cwd": layout["workspace"],
                    "packages": vendor_packages,
                },
            )
            timings.execution_started = time.monotonic()
            status = 1
            with self.daemon.lease() as lease:
                # A hard-killed parent must not unlock a live Cargo child.
                self.lock_fds = (workspace_lock.fileno(), lease.fileno())
                environment_inputs = {
                    name: re.findall(r"/nix/store/[a-z0-9]{32}-[^/\s:;\"']+", value)
                    for name, value in self.environment.items()
                    if name not in self.private_names
                }
                runtime_inputs = sorted(
                    {path for paths in environment_inputs.values() for path in paths}
                )
                extra_inputs = sorted(
                    {
                        path
                        for name, paths in environment_inputs.items()
                        if self.compiler_environment_name(name, runtime_names)
                        for path in paths
                    }
                )
                # Runtime tools remain available in the namespace and rooted for
                # launchers, but must not enter immutable compiler identities.
                runtime_inputs = sorted(
                    {str(self.bootstrap), str(vendor), *runtime_inputs}
                )
                with timings.phase("provision"):
                    self.daemon.provision(runtime_inputs, owner)
                # Checks/tests also replace the stage's vendor index. Existing
                # launchers use that same stage, so refresh their package mounts
                # once the new runtime inputs are available.
                launch_config = directory / "launch.json"
                if launch_config.is_file():
                    launch = json.loads(launch_config.read_text())
                    launch["packages"] = vendor_packages
                    write_json(launch_config, launch)
                with timings.phase("in_namespace_metadata"):
                    prepared = json.loads(
                        self.run_namespace(
                            namespace_config,
                            [
                                self.spec["cargo"],
                                "metadata",
                                "--offline",
                                "--locked",
                                "--all-features",
                                "--format-version",
                                "1",
                            ],
                            capture=True,
                        )
                    )
                with timings.phase("package_sources"):
                    packages = set(layout["package_sources"])
                    for package in prepared["packages"]:
                        if package["source"] is not None:
                            packages.add(
                                str(Path(package["manifest_path"]).parent.resolve())
                            )
                    packages = self.source_roots(packages)
                omitted = sorted(
                    name
                    for name in self.environment.keys() | runtime_names
                    if not self.compiler_environment_name(name, runtime_names)
                )
                # The empty prefix admits Cargo and build-script compiler inputs.
                # Explicit omissions take precedence in the patched driver.
                backend = {
                    "nix": self.spec["nix"],
                    "socket": str(self.daemon.socket),
                    "real_store": str(self.daemon.store_root),
                    "store_mode": self.daemon.mode,
                    "source": layout["workspace"],
                    "source_overlay": self.source_overlay,
                    "target": str(self.logical / "target"),
                    "inputs": sorted({*self.spec["inputs"], *extra_inputs}),
                    "package_sources": sorted(packages),
                    "package_store_inputs": self.package_store_inputs(
                        vendor_packages, packages
                    ),
                    "incremental": True,
                    "incremental_observer": os.environ.get(
                        "CARGO_DYNDRV_OBSERVE_INCREMENTAL"
                    )
                    == "1",
                    "environment_names": [],
                    "environment_prefixes": [""],
                    "layout_environment_names": sorted(self.layout_names),
                    "omitted_environment_names": omitted,
                }
                if self.daemon.mode == "system":
                    roots = self.state / "roots" / owner
                    private_directory(roots)
                    backend["gc_roots"] = str(roots)
                configuration = directory / "backend.json"
                write_json(configuration, backend)
                command = [
                    self.spec["driver"],
                    "nix-cargo",
                    "--directory",
                    str(session),
                    "--cargo",
                    self.spec.get("action_executables", {}).get(
                        self.action,
                        self.spec["nextest"]
                        if self.arguments[:1] == ["nextest"]
                        else self.spec["cargo"],
                    ),
                    "--rustdoc",
                    self.spec["rustdoc"],
                    "--nix-config",
                    str(configuration),
                    "--",
                    *self.mapped_command(layout["roots"]),
                ]
                print(f"dyndrv session: {session}", file=sys.stderr)
                write_json(in_progress, {"session": str(session)})
                try:
                    with timings.phase("driver"):
                        self.run_namespace(namespace_config, command)
                    with timings.phase("post_commands"):
                        self.run_post_commands(namespace_config, layout)
                    references = directory / ("references-" + session.name + ".json")
                    with timings.phase("snapshot_references"):
                        self.run_namespace(
                            namespace_config,
                            [
                                self.spec["driver"],
                                "snapshot-references",
                                "--directory",
                                str(session),
                                "--output",
                                str(references),
                            ],
                        )
                    with timings.phase("retain"):
                        self.daemon.retain(
                            sorted(
                                set(json.loads(references.read_text())["storePaths"])
                                | set(runtime_inputs)
                            ),
                            owner,
                            replace=True,
                        )
                    in_progress.unlink(missing_ok=True)
                    if self.action in {"build", "run"}:
                        with timings.phase("install_binaries"):
                            self.install_binaries(
                                metadata, directory, target, namespace_config
                            )
                    status = 0
                except subprocess.CalledProcessError as error:
                    status = error.returncode
                    try:
                        with timings.phase("retain"):
                            self.retain_failed(session, target, owner)
                        in_progress.unlink(missing_ok=True)
                    except (
                        OSError,
                        RuntimeError,
                        subprocess.CalledProcessError,
                        ValueError,
                        KeyError,
                        TypeError,
                    ) as failure:
                        rejected = target / ".dyndrv-delivered/rejected"
                        rejected.parent.mkdir(parents=True, exist_ok=True)
                        rejected.write_text(
                            "Could not retain partial compiler outputs\n"
                        )
                        print(
                            f"dyndrv cache retention failed: {failure}", file=sys.stderr
                        )
                    raise
                except BaseException as error:
                    if isinstance(error, SystemExit):
                        status = error.code if isinstance(error.code, int) else 1
                    rejected = target / ".dyndrv-delivered/rejected"
                    rejected.parent.mkdir(parents=True, exist_ok=True)
                    if not rejected.exists():
                        rejected.write_text(
                            "Worker interrupted before target ownership was retained\n"
                        )
                    print(
                        "dyndrv target requires the application's dyndrv clean action before reuse",
                        file=sys.stderr,
                    )
                    raise
                finally:
                    record.update(
                        status=status,
                        seconds=time.monotonic() - timings.execution_started,
                    )
                    if self.action == "doc" and status == 0:
                        docs = target / "doc"
                        if docs.exists():
                            shutil.copytree(
                                docs, workspace / "target/doc", dirs_exist_ok=True
                            )
                    reports = stage / "workspace/target/nextest"
                    if reports.exists():
                        destination = workspace / "target/test-reports/dyndrv"
                        shutil.copytree(reports, destination, dirs_exist_ok=True)
            return 0


def main():
    if len(sys.argv) > 1 and sys.argv[1] == "namespace":
        configuration = json.loads(Path(sys.argv[2]).read_text())
        if sys.argv[3] != "--":
            raise ValueError("namespace command requires --")
        namespace(configuration, sys.argv[4:])
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bootstrap", type=Path, required=True)
    parser.add_argument("--workspace", type=Path, required=True)
    parser.add_argument("--action", required=True)
    parser.add_argument("arguments", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    if not args.arguments or args.arguments[0] != "--":
        parser.error("Pass the Cargo command after --")
    os.umask(0o077)

    def interrupted(signum, _frame):
        raise SystemExit(128 + signum)

    for signum in (signal.SIGINT, signal.SIGTERM):
        signal.signal(signum, interrupted)
    Worker(args.bootstrap, args.workspace, args.action, args.arguments[1:]).run()


if __name__ == "__main__":
    try:
        main()
    except subprocess.CalledProcessError as error:
        sys.exit(error.returncode if error.returncode >= 0 else 128 - error.returncode)
    except (OSError, ValueError, RuntimeError) as error:
        print(f"error: {error}", file=sys.stderr)
        sys.exit(2)
