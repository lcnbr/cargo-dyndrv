"""Private worker lifecycle regressions, without starting Nix or compiling."""

import json
import os
import select
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
import tomllib
import unittest
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from unittest import mock

import tomli_w

REPOSITORY = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPOSITORY / "worker"))
import worker


class WorkerPolicyTests(unittest.TestCase):
    def test_source_overlay_defaults_to_false_and_accepts_booleans(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for option, expected in (
                ({}, False),
                ({"source_overlay": False}, False),
                ({"source_overlay": True}, True),
            ):
                with self.subTest(option=option):
                    (root / "environment.json").write_text(
                        json.dumps({"environment": {}, **option})
                    )
                    with (
                        mock.patch.dict(
                            os.environ, {"CARGO_DYNDRV_STORE": "private"}, clear=True
                        ),
                        mock.patch.object(worker, "Daemon"),
                        mock.patch.object(worker, "private_directory"),
                    ):
                        runner = worker.Worker(root, root, "check", ["check"])
                    self.assertIs(runner.source_overlay, expected)

    def test_source_overlay_rejects_non_boolean_values_before_store_access(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for value in (None, "false", "true", 0, 1, 0.0, [], {}):
                with self.subTest(value=value):
                    (root / "environment.json").write_text(
                        json.dumps({"environment": {}, "source_overlay": value})
                    )
                    with (
                        mock.patch.object(worker, "Daemon") as daemon,
                        mock.patch.object(worker, "private_directory") as directory,
                        self.assertRaisesRegex(
                            ValueError, "source_overlay must be a boolean"
                        ),
                    ):
                        worker.Worker(root, root, "check", ["check"])
                    daemon.assert_not_called()
                    directory.assert_not_called()

    def test_source_roots_preserve_lexical_ancestry_and_raw_strings(self):
        for packages, expected in (
            (
                ["/a/b/child", "/a/b", "/a/bc", "/a/bc/deep", "/a/beta", "/a/b"],
                {"/a/b", "/a/bc", "/a/beta"},
            ),
            (["/raw/./root/", "/other"], {"/raw/./root/", "/other"}),
            (["/a/b/../child", "/a/b"], {"/a/b"}),
            (["/equal", "/equal/", "/separate"], {"/separate"}),
        ):
            with self.subTest(packages=packages):
                self.assertEqual(worker.Worker.source_roots(packages), expected)

    def test_application_policy_forwards_runtime_values_but_excludes_private_build_inputs(
        self,
    ):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "environment.json").write_text(
                json.dumps(
                    {
                        "environment": {},
                        "runtime_path": "/tool/bin",
                        "vendor_expression": "configured vendor expression",
                        "nix": "/nix/store/fixture-nix/bin/nix",
                        "private_environment_names": ["PRIVATE_TOKEN"],
                        "runtime_environment_names": ["NEXTEST_WORKSPACE_ROOT"],
                        "runtime_environment_prefixes": ["APP_RUNTIME_"],
                        "workspace_environment_names": [
                            "NEXTEST_WORKSPACE_ROOT",
                            "INSTA_WORKSPACE_ROOT",
                            "APP_RUNTIME_WORKSPACE",
                        ],
                        "forward_environment_names": [
                            "APP_COMPILER_FLAG",
                            "APP_RUNTIME_WORKSPACE",
                        ],
                    }
                )
            )
            environment = {
                "CARGO_DYNDRV_STATE": str(root / "state"),
                "CARGO_DYNDRV_STORE": "private",
                "PRIVATE_TOKEN": "runtime-only",
                "APP_RUNTIME_VALUE": "runtime",
                "APP_COMPILER_FLAG": "compiler",
                "INSTA_WORKSPACE_ROOT": "/caller/physical/workspace",
                "NEXTEST_WORKSPACE_ROOT": "/caller/physical/workspace",
                "APP_RUNTIME_WORKSPACE": "/caller/physical/workspace",
                "INSTA_UPDATE": "always",
                "UNRELATED_VALUE": "ignored",
            }
            with (
                mock.patch.dict(os.environ, environment, clear=True),
                mock.patch.object(worker, "Daemon"),
                mock.patch.object(worker, "private_directory"),
            ):
                runner = worker.Worker(root, root, "check", ["check"])
                self.assertNotIn("PRIVATE_TOKEN", runner.staging_environment())
            self.assertEqual(runner.environment["PRIVATE_TOKEN"], "runtime-only")
            self.assertEqual(runner.environment["APP_RUNTIME_VALUE"], "runtime")
            self.assertEqual(runner.environment["APP_COMPILER_FLAG"], "compiler")
            self.assertNotIn("UNRELATED_VALUE", runner.environment)
            self.assertEqual(
                runner.layout_names, {"INSTA_WORKSPACE_ROOT", "NEXTEST_WORKSPACE_ROOT"}
            )
            for name in runner.workspace_names:
                self.assertNotIn(name, runner.environment)
                self.assertTrue(
                    runner.compiler_environment_name(name, runner.runtime_names)
                )
            for name in ("PRIVATE_TOKEN", "APP_RUNTIME_VALUE", "INSTA_UPDATE", "HOME"):
                self.assertFalse(
                    runner.compiler_environment_name(name, runner.runtime_names)
                )
            self.assertTrue(
                runner.compiler_environment_name(
                    "APP_COMPILER_FLAG", runner.runtime_names
                )
            )
            self.assertIn("PRIVATE_TOKEN", runner.runtime_names)
            self.assertNotIn("PRIVATE_TOKEN", runner.client)
            with mock.patch.object(
                worker.subprocess,
                "check_output",
                return_value="/nix/store/fixture-vendor\n",
            ) as execute:
                runner.vendor_input(root / "Cargo.lock")
            self.assertNotIn("PRIVATE_TOKEN", execute.call_args.kwargs["env"])
            self.assertEqual(
                execute.call_args.kwargs["env"]["CARGO_DYNDRV_CARGO_LOCK"],
                str(root / "Cargo.lock"),
            )
            specification = json.loads((root / "environment.json").read_text())
            specification["private_environment_names"].append("INSTA_WORKSPACE_ROOT")
            (root / "environment.json").write_text(json.dumps(specification))
            with self.assertRaisesRegex(
                ValueError, "cannot be private.*INSTA_WORKSPACE_ROOT"
            ):
                worker.Worker(root, root, "check", ["check"])

    def test_phase_timings_include_failed_work_and_preserve_execution_seconds(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "last-run.json"
            with mock.patch.object(
                worker.time, "monotonic", side_effect=[10, 11, 14, 15, 17, 20, 22]
            ):
                timings = worker.RunTimings()
                with (
                    self.assertRaisesRegex(ValueError, "failed"),
                    timings.phase("vendor_input"),
                ):
                    raise ValueError("failed")
                with timings.phase("vendor_input"):
                    pass
                timings.write(path, {"status": 1, "seconds": 2})
                report = json.loads(path.read_text())
                self.assertEqual(report["phase_seconds"], {"vendor_input": 5})
                self.assertEqual(report["total_seconds"], 10)
                self.assertEqual(report["seconds"], 2)
                timings.write(path, {"status": 1})
                self.assertEqual(json.loads(path.read_text())["seconds"], 0)

    def test_post_commands_map_context_and_manifest_and_preserve_failure(self):
        runner = worker.Worker.__new__(worker.Worker)
        runner.bootstrap = Path("/nix/store/test-bootstrap")
        runner.action = "package"
        runner.arguments = [
            "build",
            "--manifest-path",
            "/original/Cargo.toml",
            "--out",
            "target/wheels",
        ]
        runner.spec = {
            "post_commands": {
                "package": [
                    ["export", "{workspace}/assets", "{bootstrap}", "{literal}"]
                ]
            }
        }
        runner.run_namespace = mock.Mock()
        layout = {
            "workspace": "/build/source",
            "roots": [{"original": "/original", "logical": "/build/source"}],
        }
        runner.run_post_commands(Path("namespace.json"), layout)
        runner.run_namespace.assert_called_once_with(
            Path("namespace.json"),
            [
                "export",
                "/build/source/assets",
                "/nix/store/test-bootstrap",
                "{literal}",
                "build",
                "--manifest-path",
                "/build/source/Cargo.toml",
                "--out",
                "target/wheels",
            ],
        )
        runner.run_namespace.side_effect = subprocess.CalledProcessError(17, "export")
        with self.assertRaises(subprocess.CalledProcessError) as failure:
            runner.run_post_commands(Path("namespace.json"), layout)
        self.assertEqual(failure.exception.returncode, 17)


class VendorInputTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="dyndrv-vendor-input-")
        self.addCleanup(temporary.cleanup)
        self.directory = Path(temporary.name)
        self.lock = self.directory / "Cargo.lock"
        self.lock.write_text("version = 3\n")
        self.runner = worker.Worker.__new__(worker.Worker)
        self.runner.state = self.directory / "state"
        self.runner.state.mkdir(mode=0o700)
        self.runner.spec = {
            "vendor_lockfile_only": True,
            "vendor_expression": "immutable lockfile-only provider",
            "nix": "/nix/store/" + "0" * 32 + "-nix/bin/nix",
        }
        self.runner.client = {"NIX_REMOTE": "daemon", "HOST_VALUE": "not-persisted"}
        self.vendor = "/nix/store/" + "1" * 32 + "-vendor"
        self.build = self.enterContext(
            mock.patch.object(
                worker.subprocess, "check_output", return_value=self.vendor + "\n"
            )
        )
        self.validity = self.enterContext(
            mock.patch.object(
                worker.subprocess, "run", return_value=mock.Mock(returncode=0)
            )
        )

    def test_cache_hit_uses_host_store_and_persists_only_result_identity(self):
        self.assertEqual(self.runner.vendor_input(self.lock), Path(self.vendor))
        self.assertEqual(self.runner.vendor_input(self.lock), Path(self.vendor))
        self.build.assert_called_once()
        self.validity.assert_called_once_with(
            [self.runner.spec["nix"], "path-info", self.vendor],
            env=self.runner.client,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        )
        cache = next((self.runner.state / "vendor-inputs").glob("*.json"))
        self.assertEqual(
            json.loads(cache.read_text()),
            {"version": 1, "key": cache.stem, "path": self.vendor},
        )
        self.assertNotIn("not-persisted", cache.read_text())
        self.assertFalse((self.runner.state / "roots").exists())

    def test_content_expression_and_tool_change_keys_but_lock_path_does_not(self):
        self.runner.vendor_input(self.lock)
        relocated = self.directory / "relocated.lock"
        relocated.write_bytes(self.lock.read_bytes())
        self.runner.vendor_input(relocated)
        self.assertEqual(self.build.call_count, 1)
        stamp = self.lock.stat()
        self.lock.write_text("version = 4\n")
        os.utime(self.lock, ns=(stamp.st_atime_ns, stamp.st_mtime_ns))
        self.runner.vendor_input(self.lock)
        self.runner.spec["vendor_expression"] += " changed"
        self.runner.vendor_input(self.lock)
        self.runner.spec["nix"] += "-changed"
        self.runner.vendor_input(self.lock)
        self.assertEqual(self.build.call_count, 4)
        self.assertEqual(
            len(list((self.runner.state / "vendor-inputs").glob("*.json"))), 4
        )

    def test_profile_changes_after_keying_do_not_change_the_executed_binary(self):
        first, second = (self.directory / name for name in ("nix-a", "nix-b"))
        first.touch()
        second.touch()
        profile = self.directory / "profile-nix"
        profile.symlink_to(first)
        self.runner.spec["nix"] = str(profile)
        original_locked = worker.locked

        def changed_profile(path):
            profile.unlink()
            profile.symlink_to(second)
            return original_locked(path)

        with mock.patch.object(worker, "locked", side_effect=changed_profile):
            self.runner.vendor_input(self.lock)
        self.assertEqual(self.build.call_args.args[0][0], str(first))
        profile.unlink()
        profile.symlink_to(first)
        self.runner.vendor_input(self.lock)
        self.assertEqual(self.validity.call_args.args[0][0], str(first))
        self.build.assert_called_once()

    def test_malformed_entries_rebuild_without_querying_untrusted_paths(self):
        self.runner.vendor_input(self.lock)
        cache = next((self.runner.state / "vendor-inputs").glob("*.json"))
        entry = json.loads(cache.read_text())
        invalid = [
            b"not JSON",
            b"\xff",
            b"null",
            b"[]",
            *(
                json.dumps(entry | change).encode()
                for change in (
                    {"version": 0},
                    {"key": "wrong"},
                    {"path": None},
                    {"path": "/tmp/vendor"},
                    {"path": self.vendor + "/subdirectory"},
                    {"path": "/nix/store/" + "e" * 32 + "-vendor"},
                )
            ),
        ]
        for contents in invalid:
            with self.subTest(contents=contents):
                cache.write_bytes(contents)
                self.build.reset_mock()
                self.validity.reset_mock()
                self.assertEqual(self.runner.vendor_input(self.lock), Path(self.vendor))
                self.build.assert_called_once()
                self.validity.assert_not_called()

    def test_collected_store_path_rebuilds(self):
        self.runner.vendor_input(self.lock)
        self.build.reset_mock()
        self.validity.return_value.returncode = 1
        self.assertEqual(self.runner.vendor_input(self.lock), Path(self.vendor))
        self.build.assert_called_once()
        self.validity.assert_called_once()

    def test_opt_out_preserves_impure_builds_without_reading_the_lock(self):
        self.lock.unlink()
        for enabled in (False, None):
            with self.subTest(enabled=enabled):
                self.runner.spec.pop("vendor_lockfile_only", None)
                if enabled is not None:
                    self.runner.spec["vendor_lockfile_only"] = enabled
                self.build.reset_mock()
                self.runner.vendor_input(self.lock)
                self.runner.vendor_input(self.lock)
                self.assertEqual(self.build.call_count, 2)
        self.validity.assert_not_called()
        self.assertFalse((self.runner.state / "vendor-inputs").exists())

    def test_cache_build_uses_lock_snapshot_and_cleans_up_after_failure(self):
        original = self.lock.read_bytes()
        snapshots = []

        def build(*arguments, **kwargs):
            snapshot = Path(kwargs["env"]["CARGO_DYNDRV_CARGO_LOCK"])
            snapshots.append(snapshot)
            self.lock.write_text("version = 4\n")
            self.assertEqual(snapshot.name, "Cargo.lock")
            self.assertEqual(snapshot.read_bytes(), original)
            return self.vendor + "\n"

        self.build.side_effect = build
        self.runner.vendor_input(self.lock)
        self.assertFalse(snapshots[0].parent.exists())
        self.build.side_effect = subprocess.CalledProcessError(7, "nix")
        with self.assertRaises(subprocess.CalledProcessError):
            self.runner.vendor_input(self.lock)
        directory = self.runner.state / "vendor-inputs"
        self.assertFalse(any(path.is_dir() for path in directory.iterdir()))
        self.assertEqual(len(list(directory.glob("*.json"))), 1)
        self.lock.write_bytes(original)
        self.assertEqual(self.runner.vendor_input(self.lock), Path(self.vendor))
        self.assertEqual(self.build.call_count, 2)

    def test_cross_workspace_clients_serialize_one_cache_key(self):
        other = worker.Worker.__new__(worker.Worker)
        other.__dict__.update(self.runner.__dict__)
        self.runner.workspace = self.directory / "first-workspace"
        other.workspace = self.directory / "second-workspace"

        def build(*arguments, **kwargs):
            time.sleep(0.02)
            return self.vendor + "\n"

        self.build.side_effect = build
        with ThreadPoolExecutor(max_workers=2) as pool:
            results = list(
                pool.map(
                    lambda runner: runner.vendor_input(self.lock), (self.runner, other)
                )
            )
        self.assertEqual(results, [Path(self.vendor), Path(self.vendor)])
        self.build.assert_called_once()
        self.validity.assert_called_once()


class VendorLayoutTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="dyndrv-vendor-")
        self.addCleanup(temporary.cleanup)
        self.directory = Path(temporary.name)
        self.runner = worker.Worker.__new__(worker.Worker)
        self.runner.logical = Path("/build/cargo-dyndrv")
        self.stage = self.directory / "stage"

    def package(self, name):
        package = self.directory / "packages" / name
        package.mkdir(parents=True)
        (package / "Cargo.toml").write_text(
            '[package]\nname="probe"\nversion="1.0.0"\n'
        )
        return package

    def vendor(self, name, packages):
        vendor = self.directory / name
        registry = vendor / "registry"
        registry.mkdir(parents=True)
        for package_name, package in packages.items():
            (registry / package_name).symlink_to(package, target_is_directory=True)
        configuration = {
            "source": {
                "crates-io": {"replace-with": "registry"},
                "registry": {"directory": str(registry)},
            }
        }
        (vendor / "config.toml").write_text(tomli_w.dumps(configuration))
        return vendor

    def test_lockfile_changes_preserve_existing_package_paths_and_prune_removed_entries(
        self,
    ):
        selected = self.package("selected")
        unused = self.package("unused")
        first = self.vendor("first", {"probe-1.0.0": selected})
        second = self.vendor(
            "second", {"probe-1.0.0": selected, "unused-1.0.0": unused}
        )
        original = (first / "config.toml").read_bytes()
        configuration, packages = self.runner.vendor_layout(first, self.stage)
        changed_configuration, changed_packages = self.runner.vendor_layout(
            second, self.stage
        )
        self.assertEqual(configuration, changed_configuration)
        self.assertEqual(
            packages, [p for p in changed_packages if p["source"] == str(selected)]
        )
        for package in changed_packages:
            mountpoint = self.stage / Path(package["target"]).relative_to(
                self.runner.logical
            )
            self.assertTrue(mountpoint.is_dir())
            self.assertFalse(mountpoint.is_symlink())
            self.assertEqual(list(mountpoint.iterdir()), [])
        self.runner.vendor_layout(first, self.stage)
        self.assertFalse(any(self.stage.rglob("unused-1.0.0")))
        self.assertEqual((first / "config.toml").read_bytes(), original)

    def test_changed_package_bytes_keep_logical_path_but_change_mounted_source(self):
        first = self.vendor("first", {"probe-1.0.0": self.package("original")})
        changed = self.package("changed")
        (changed / "Cargo.toml").write_text(
            '[package]\nname="probe"\nversion="1.0.0"\n# changed\n'
        )
        second = self.vendor("second", {"probe-1.0.0": changed})
        _, before = self.runner.vendor_layout(first, self.stage)
        _, after = self.runner.vendor_layout(second, self.stage)
        self.assertEqual(before[0]["target"], after[0]["target"])
        self.assertNotEqual(before[0]["source"], after[0]["source"])

    def test_distinct_cargo_sources_keep_same_named_packages_disjoint(self):
        vendor = self.vendor("first", {"probe-1.0.0": self.package("registry")})
        git = self.vendor("git", {"probe-1.0.0": self.package("git")})
        configuration = tomllib.loads((vendor / "config.toml").read_text())
        configuration["source"]["git"] = {"directory": str(git / "registry")}
        (vendor / "config.toml").write_text(tomli_w.dumps(configuration))
        _, packages = self.runner.vendor_layout(vendor, self.stage)
        self.assertEqual(len(packages), 2)
        self.assertNotEqual(packages[0]["target"], packages[1]["target"])

    def test_store_inputs_select_only_whole_mounted_package_roots(self):
        sources = [
            "/nix/store/whole-package",
            "/nix/store/git-checkout/package",
            "/nix/store/unselected-package",
            str(self.directory / "mutable-package"),
        ]
        packages = [
            {"source": source, "target": str(self.runner.logical / f"vendor/{index}")}
            for index, source in enumerate(sources)
        ]
        selected = {
            package["target"] for index, package in enumerate(packages) if index != 2
        }
        self.assertEqual(
            self.runner.package_store_inputs(packages, selected),
            {packages[0]["target"]: sources[0]},
        )


@unittest.skipUnless(sys.platform == "linux", "The dyndrv worker requires Linux")
class WorkerLifecycleTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="dyndrv worker's ")
        self.addCleanup(temporary.cleanup)
        self.directory = Path(temporary.name)
        self.python = str(Path(sys.executable).resolve())
        # Startup is mocked; no Nix executable or daemon is needed.
        self.daemon = worker.Daemon(self.directory / "state", {"nix": self.python})
        self.addCleanup(shutil.rmtree, self.daemon.transport)
        self.record = self.daemon.state / "daemon.json"

    def daemon_record(self, *, stale=False, ignore_term=False):
        script = "import sys; print('ready', flush=True); sys.stdin.buffer.read()"
        if ignore_term:
            script = (
                "import signal, sys; "
                "signal.signal(signal.SIGTERM, signal.SIG_IGN); "
                "print('ready', flush=True); sys.stdin.buffer.read()"
            )
        command = [
            self.python,
            "-c",
            script,
            "--store",
            str(self.directory / "live-store"),
        ]
        process = subprocess.Popen(
            command,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            start_new_session=True,
        )

        def reap():
            try:
                process.communicate(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.communicate(timeout=5)

        self.addCleanup(reap)
        readable, _, _ = select.select([process.stdout], [], [], 5)
        self.assertTrue(readable, "child did not finish process setup")
        self.assertEqual(process.stdout.readline(), b"ready\n")
        if stale:
            # The executable and every other argument still match the live PID.
            command[-1] = str(self.directory / "previous-store")
        record = {"pid": process.pid, "command": command}
        worker.write_json(self.record, record)
        with socket.socket(socket.AF_UNIX) as transport:
            transport.bind(str(self.daemon.socket))
        self.assertIsNone(process.poll())
        return process, record

    def test_stop_clears_stale_pid_without_signalling_the_live_process(self):
        process, _ = self.daemon_record(stale=True)
        with mock.patch.object(worker.os, "killpg") as kill:
            self.daemon.stop()
        kill.assert_not_called()
        self.assertIsNone(process.poll())
        self.assertFalse(self.record.exists())
        self.assertFalse(self.daemon.socket.exists())

    def test_system_store_cannot_be_stopped_or_collected_by_a_workspace(self):
        system = worker.Daemon(
            self.directory / "system-client", {"nix": self.python}, mode="system"
        )
        self.assertEqual(system.store_root, Path("/"))
        self.assertEqual(system.socket, Path("/nix/var/nix/daemon-socket/socket"))
        runner = worker.Worker.__new__(worker.Worker)
        runner.daemon = system
        runner.action = "gc"
        runner.spec = {}
        with mock.patch.object(worker.subprocess, "run") as execute:
            with self.assertRaisesRegex(ValueError, "administrator-managed"):
                system.stop()
            with self.assertRaisesRegex(ValueError, "administrator-managed"):
                runner.run()
            execute.assert_not_called()

    def test_system_store_provisions_roots_without_copying_tools(self):
        system = worker.Daemon(
            self.directory / "system-client", {"nix": self.python}, mode="system"
        )
        inputs = ["/nix/store/fixture-input"]
        with (
            mock.patch.object(system, "retain") as retain,
            mock.patch.object(worker.subprocess, "run") as execute,
        ):
            system.provision(inputs, "workspace/policy")
        retain.assert_called_once_with(inputs, "workspace/policy")
        execute.assert_not_called()

    def test_failed_build_retains_artifacts_dependency_sources_and_incremental_state(
        self,
    ):
        runner = worker.Worker.__new__(worker.Worker)
        runner.daemon = self.daemon
        target = self.directory / "target"
        session = self.directory / "session"
        artifact = "/nix/store/artifact-owner/artifacts/libdependency.rlib"
        source = "/nix/store/source-owner"
        incremental = "/nix/store/incremental-owner"
        worker.write_json(
            target / ".dyndrv-delivered/dependency.json",
            {
                "version": 5,
                "files": {"artifact": artifact},
                "trees": {"source": source},
            },
        )
        worker.write_json(
            target / ".dyndrv-delivered/incremental/dependency.json",
            {"version": 1, "input": incremental},
        )
        worker.write_json(
            session / "completed.nix-result.json",
            {"compilerSucceeded": True, "output": "/nix/store/completed-owner"},
        )
        with mock.patch.object(self.daemon, "retain") as retain:
            runner.retain_failed(session, target, "workspace/policy")
        retain.assert_called_once_with(
            {
                "/nix/store/artifact-owner",
                source,
                incremental,
                "/nix/store/completed-owner",
            },
            "workspace/policy",
        )

    def test_failed_build_rejects_dependency_source_outside_store(self):
        runner = worker.Worker.__new__(worker.Worker)
        runner.daemon = self.daemon
        target = self.directory / "target"
        worker.write_json(
            target / ".dyndrv-delivered/dependency.json",
            {"version": 5, "files": {}, "trees": {"source": "/tmp/mutable-source"}},
        )
        with (
            mock.patch.object(self.daemon, "retain") as retain,
            self.assertRaisesRegex(ValueError, "outside the immutable store"),
        ):
            runner.retain_failed(self.directory / "session", target, "workspace/policy")
        retain.assert_not_called()

    def test_start_replaces_stale_pid_and_socket_before_launch(self):
        process, _ = self.daemon_record(stale=True)
        replacement = mock.Mock(pid=process.pid)
        replacement.poll.return_value = None

        def launch(*arguments, **options):
            self.assertFalse(self.record.exists())
            self.assertFalse(self.daemon.socket.exists())
            return replacement

        with (
            mock.patch.object(self.daemon, "healthy", side_effect=[False, True]),
            mock.patch.object(worker.subprocess, "Popen", side_effect=launch) as start,
            mock.patch.object(worker.os, "killpg") as kill,
        ):
            self.daemon.start()
        start.assert_called_once()
        self.assertEqual(
            json.loads(self.record.read_text()),
            {"pid": replacement.pid, "command": start.call_args.args[0]},
        )
        kill.assert_not_called()
        self.assertIsNone(process.poll())

    def test_start_preserves_matching_unhealthy_daemon_and_refuses_duplicate(self):
        process, record = self.daemon_record()
        with (
            mock.patch.object(self.daemon, "healthy", return_value=False),
            mock.patch.object(worker.subprocess, "Popen") as start,
            mock.patch.object(worker.os, "killpg") as kill,
            self.assertRaisesRegex(RuntimeError, "recorded daemon is not responding"),
        ):
            self.daemon.start()
        start.assert_not_called()
        kill.assert_not_called()
        self.assertEqual(json.loads(self.record.read_text()), record)
        self.assertTrue(self.daemon.socket.exists())
        self.assertIsNone(process.poll())

    def test_stop_terminates_and_clears_the_matching_owned_process(self):
        process, _ = self.daemon_record()
        killpg = os.killpg

        def terminate(pid, operation):
            self.assertEqual(pid, process.pid)
            self.assertEqual(operation, signal.SIGTERM)
            killpg(pid, operation)
            process.communicate(timeout=5)

        with mock.patch.object(worker.os, "killpg", side_effect=terminate) as kill:
            self.daemon.stop()
        kill.assert_called_once_with(process.pid, signal.SIGTERM)
        self.assertEqual(process.returncode, -signal.SIGTERM)
        self.assertFalse(self.record.exists())
        self.assertFalse(self.daemon.socket.exists())

    def test_stop_force_kills_unhealthy_owned_process_that_ignores_term(self):
        process, _ = self.daemon_record(ignore_term=True)
        killpg = os.killpg
        monotonic = time.monotonic
        first_wait = iter((0, 11))

        def terminate(pid, operation):
            self.assertEqual(pid, process.pid)
            killpg(pid, operation)
            if operation == signal.SIGTERM:
                with self.assertRaises(subprocess.TimeoutExpired):
                    process.wait(timeout=0.05)

        with (
            mock.patch.object(self.daemon, "healthy", return_value=False),
            mock.patch.object(worker.os, "killpg", side_effect=terminate) as kill,
            mock.patch.object(worker, "time", wraps=time) as clock,
        ):
            # Expire only the TERM grace period; the KILL wait uses real time.
            clock.monotonic.side_effect = lambda: next(first_wait, monotonic())
            self.daemon.stop()
        self.assertEqual(
            kill.call_args_list,
            [
                mock.call(process.pid, signal.SIGTERM),
                mock.call(process.pid, signal.SIGKILL),
            ],
        )
        self.assertEqual(process.wait(timeout=5), -signal.SIGKILL)
        self.assertFalse(self.record.exists())
        self.assertFalse(self.daemon.socket.exists())

    def test_namespace_child_retains_locks_after_parent_handles_close(self):
        namespace = self.directory / "namespace substitute.py"
        namespace.write_text(
            "import os, sys\n"
            "assert sys.argv[1] == 'namespace' and sys.argv[3] == '--'\n"
            "os.execvpe(sys.argv[4], sys.argv[4:], os.environ)\n"
        )
        child = self.directory / "lock holder.py"
        child.write_text(
            "import json, os, sys, time\n"
            "from pathlib import Path\n"
            "ready, release, *descriptors = sys.argv[1:]\n"
            "identities = []\n"
            "for descriptor in descriptors:\n"
            "    info = os.fstat(int(descriptor))\n"
            "    identities.append([info.st_dev, info.st_ino])\n"
            "temporary = Path(ready).with_suffix('.tmp')\n"
            "temporary.write_text(json.dumps(identities))\n"
            "temporary.replace(ready)\n"
            "deadline = time.monotonic() + 10\n"
            "while not Path(release).exists():\n"
            "    if time.monotonic() >= deadline:\n"
            "        raise RuntimeError('test did not release the lock holder')\n"
            "    time.sleep(0.01)\n"
        )
        runner = worker.Worker.__new__(worker.Worker)
        runner.spec = {"python": self.python}
        runner.environment = {"PATH": os.defpath}
        ready = self.directory / "ready.json"
        release = self.directory / "release"
        workspace_lock = self.directory / "workspace.lock"
        with (
            mock.patch.object(self.daemon, "start"),
            worker.locked(workspace_lock) as workspace,
            self.daemon.lease() as lease,
            ThreadPoolExecutor(max_workers=1) as executor,
            mock.patch.object(worker, "__file__", str(namespace)),
        ):
            runner.lock_fds = (workspace.fileno(), lease.fileno())
            identities = [
                [info.st_dev, info.st_ino] for info in map(os.fstat, runner.lock_fds)
            ]
            locks = (workspace_lock, Path(lease.name))
            future = executor.submit(
                runner.run_namespace,
                self.directory / "namespace.json",
                [
                    self.python,
                    str(child),
                    str(ready),
                    str(release),
                    *map(str, runner.lock_fds),
                ],
                capture=True,
            )
            try:
                deadline = time.monotonic() + 5
                while not ready.exists():
                    if future.done():
                        future.result()
                        self.fail("namespace child exited before reporting its locks")
                    self.assertLess(time.monotonic(), deadline, "child did not start")
                    time.sleep(0.01)
                self.assertEqual(json.loads(ready.read_text()), identities)
                workspace.close()
                lease.close()
                for path in locks:
                    with (
                        self.assertRaises(BlockingIOError),
                        worker.locked(path, blocking=False),
                    ):
                        pass
                with self.assertRaisesRegex(RuntimeError, "still using the daemon"):
                    self.daemon.require_idle()
            finally:
                release.touch()
                future.result(timeout=15)
            for path in locks:
                with worker.locked(path, blocking=False):
                    pass
            self.daemon.require_idle()
            self.assertFalse(locks[1].exists())


if __name__ == "__main__":
    unittest.main()
