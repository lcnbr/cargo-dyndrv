"""Private worker lifecycle regressions, without starting Nix or compiling."""

import contextlib
import json
import os
import select
import shutil
import signal
import socket
import stat
import subprocess
import sys
import tempfile
import time
import tomllib
import unittest
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from types import SimpleNamespace
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


class ArtifactDeliveryTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="dyndrv-artifact-policy-")
        self.addCleanup(temporary.cleanup)
        self.directory = Path(temporary.name)
        self.state = self.directory / "state"
        self.spec = {
            "environment": {},
            "runtime_path": "/tool/bin",
            "nix": "/tool/bin/nix",
            "cargo": "/tool/bin/cargo",
            "driver": "/tool/bin/cargo-dyndrv",
            "rustdoc": "/tool/bin/rustdoc",
            "inputs": ["/nix/store/compiler-input"],
        }
        config = self.directory / "store.json"
        worker.write_json(config, {"version": 1, "store_mode": "system"})
        self.enterContext(
            mock.patch.dict(
                os.environ,
                {
                    "CARGO_DYNDRV_STATE": str(self.state),
                    "CARGO_DYNDRV_STORE_CONFIG": str(config),
                    "CARGO_HOME": str(self.directory / "cargo-home"),
                },
                clear=True,
            )
        )
        self.host = {
            Path("/nix"): SimpleNamespace(st_mode=stat.S_IFDIR | 0o755, st_uid=0),
            Path("/nix/store"): SimpleNamespace(
                st_mode=stat.S_IFDIR | 0o1775, st_uid=0, st_gid=30000
            ),
            Path("/build/cargo-dyndrv"): SimpleNamespace(
                st_mode=stat.S_IFDIR | 0o755, st_uid=0
            ),
        }
        lstat = Path.lstat
        self.enterContext(
            mock.patch.object(
                Path,
                "lstat",
                autospec=True,
                side_effect=lambda path: (
                    self.host[path] if path in self.host else lstat(path)
                ),
            )
        )
        resolve = Path.resolve
        self.canonical = {path: path for path in self.host}
        self.enterContext(
            mock.patch.object(
                Path,
                "resolve",
                autospec=True,
                side_effect=lambda path, **options: (
                    self.canonical[path]
                    if path in self.canonical
                    else resolve(path, **options)
                ),
            )
        )
        self.access = self.enterContext(
            mock.patch.object(worker.os, "access", return_value=False)
        )
        self.mount = self.enterContext(
            mock.patch.object(
                worker.os,
                "statvfs",
                return_value=SimpleNamespace(f_flag=os.ST_RDONLY | os.ST_NOSUID),
            )
        )
        self.daemon = self.enterContext(mock.patch.object(worker, "Daemon"))
        self.private_directory = self.enterContext(
            mock.patch.object(worker, "private_directory")
        )

    def make_worker(self):
        worker.write_json(self.directory / "environment.json", self.spec)
        return worker.Worker(self.directory, self.directory, "check", ["check"])

    def test_copy_default_and_explicit_policy_skip_the_host_gate(self):
        for mode in ("private", "system"):
            for option in ({}, {"artifact_delivery": "copy"}):
                with self.subTest(mode=mode, option=option):
                    self.spec.pop("artifact_delivery", None)
                    self.spec.update(option)
                    with mock.patch.dict(os.environ, {"CARGO_DYNDRV_STORE": mode}):
                        runner = self.make_worker()
                    self.assertEqual(runner.artifact_delivery, "copy")
        self.access.assert_not_called()
        self.mount.assert_not_called()

    def test_invalid_policy_is_rejected_before_store_configuration_or_state(self):
        for value in (None, "system-store-links", "copy ", "", True, 0, [], {}):
            with self.subTest(value=value):
                self.spec["artifact_delivery"] = value
                with (
                    mock.patch.object(Path, "is_file") as store_configuration,
                    self.assertRaisesRegex(ValueError, "artifact_delivery must be"),
                ):
                    self.make_worker()
                store_configuration.assert_not_called()
        self.daemon.assert_not_called()
        self.private_directory.assert_not_called()
        self.access.assert_not_called()
        self.mount.assert_not_called()
        self.assertFalse(self.state.exists())

    def test_link_policy_refuses_private_store_before_daemon_or_state(self):
        self.spec["artifact_delivery"] = "system-store-rlib-links"
        with (
            mock.patch.dict(os.environ, {"CARGO_DYNDRV_STORE": "private"}),
            self.assertRaisesRegex(ValueError, "requires the system store"),
        ):
            self.make_worker()
        self.daemon.assert_not_called()
        self.private_directory.assert_not_called()
        self.access.assert_not_called()
        self.mount.assert_not_called()
        self.assertFalse(self.state.exists())

    def test_link_policy_accepts_protected_admin_group_writable_store(self):
        self.spec["artifact_delivery"] = "system-store-rlib-links"
        runner = self.make_worker()
        self.assertEqual(runner.artifact_delivery, "system-store-rlib-links")
        self.assertEqual(self.daemon.call_args.kwargs, {"mode": "system"})
        self.assertEqual(
            self.access.call_args_list,
            [
                mock.call(path, os.W_OK, effective_ids=True)
                for path in (Path("/nix"), Path("/nix/store"))
            ],
        )
        self.mount.assert_called_once_with("/nix/store")

    def test_link_policy_refuses_untrusted_host_directories_before_state(self):
        self.spec["artifact_delivery"] = "system-store-rlib-links"
        for path in (Path("/nix"), Path("/nix/store")):
            for change in (
                {"st_uid": 1001},
                {"st_uid": 65534},
                {"st_mode": stat.S_IFLNK | 0o777},
                {"st_mode": stat.S_IFREG | 0o555},
            ):
                with self.subTest(path=path, change=change):
                    info = SimpleNamespace(**(vars(self.host[path]) | change))
                    with (
                        mock.patch.dict(self.host, {path: info}),
                        self.assertRaisesRegex(ValueError, "administrator-owned"),
                    ):
                        self.make_worker()
            with (
                self.subTest(path=path, change="noncanonical"),
                mock.patch.dict(self.canonical, {path: Path("/elsewhere")}),
                self.assertRaisesRegex(ValueError, "canonical"),
            ):
                self.make_worker()
            with self.subTest(path=path, change="writable"):
                self.access.side_effect = (
                    lambda candidate, *args, expected=path, **kwargs: (
                        candidate == expected
                    )
                )
                with self.assertRaisesRegex(ValueError, "not writable by the caller"):
                    self.make_worker()
                self.access.side_effect = None
        self.daemon.assert_not_called()
        self.private_directory.assert_not_called()
        self.assertFalse(self.state.exists())

    def test_link_policy_requires_read_only_store_mount(self):
        self.spec["artifact_delivery"] = "system-store-rlib-links"
        self.mount.return_value.f_flag = 0
        with self.assertRaisesRegex(ValueError, "read-only /nix/store mount"):
            self.make_worker()
        self.daemon.assert_not_called()
        self.private_directory.assert_not_called()
        self.assertFalse(self.state.exists())

    def test_delivery_policy_separates_targets_without_changing_compiler_inputs(self):
        daemon = self.daemon.return_value
        daemon.mode = "system"
        daemon.state = self.state
        daemon.store_root = Path("/")
        daemon.socket = Path("/nix/var/nix/daemon-socket/socket")
        layout = {
            "workspace": "/build/cargo-dyndrv/workspace",
            "roots": [],
            "package_sources": ["/build/cargo-dyndrv/workspace"],
        }
        deliveries = []
        for policy in ("copy", "system-store-rlib-links", "copy"):
            self.spec["artifact_delivery"] = policy
            runner = self.make_worker()

            def execute(
                config, command, *, capture=False, policy=policy, runner=runner
            ):
                if command[1] == "metadata":
                    return json.dumps({"packages": []})
                if command[1] == "snapshot-references":
                    worker.write_json(Path(command[-1]), {"storePaths": []})
                else:
                    self.assertEqual(command[1], "nix-cargo")
                    worker.write_json(
                        Path(command[3]) / "cargo-result.json",
                        {"version": 2, "status": 0, "nix_invocations": 0},
                    )
                    backend = json.loads((config.parent / "backend.json").read_text())
                    self.assertEqual(backend.pop("artifact_delivery"), policy)
                    roots = Path(backend.pop("gc_roots"))
                    self.assertEqual(roots.name, config.parent.name)
                    namespace = json.loads(config.read_text())
                    self.assertEqual(namespace["store"], "/nix/store")
                    deliveries.append(
                        (namespace["stage"], backend, runner.environment.copy())
                    )

            with (
                mock.patch.object(
                    runner,
                    "original_metadata",
                    return_value={"workspace_root": str(self.directory)},
                ),
                mock.patch.object(
                    runner, "vendor_input", return_value=Path("/nix/store/vendor")
                ),
                mock.patch.object(runner, "vendor_layout", return_value=({}, [])),
                mock.patch.object(worker, "SourceLayout") as sources,
                mock.patch.object(runner, "run_namespace", side_effect=execute),
            ):
                sources.return_value.prepare.return_value = layout
                self.assertEqual(runner.run(), 0)
        self.assertNotEqual(deliveries[0][0], deliveries[1][0])
        self.assertEqual(deliveries[0][1:], deliveries[1][1:])
        self.assertEqual(deliveries[0], deliveries[2])


class WorkerRetentionTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="dyndrv-retention-")
        self.addCleanup(temporary.cleanup)
        self.directory = Path(temporary.name)
        self.bootstrap = self.directory / "bootstrap"
        self.bootstrap.mkdir()
        (self.bootstrap / "environment.json").write_text("{}")
        self.workspace = self.directory / "workspace"
        self.workspace.mkdir()
        self.state = self.directory / "state"
        self.daemon = mock.Mock(
            mode="system",
            state=self.state,
            store_root=Path("/"),
            socket=Path("/daemon/socket"),
        )

        @contextlib.contextmanager
        def lease():
            with worker.locked(self.state / "test-lease") as stream:
                yield stream

        self.daemon.lease.side_effect = lease
        self.daemon.retain.side_effect = self.retain
        self.daemon.provision.side_effect = lambda paths, owner: self.retain(
            paths, owner
        )
        self.metadata = self.enterContext(
            mock.patch.object(worker.Worker, "original_metadata")
        )
        self.metadata.side_effect = lambda **kwargs: {
            "workspace_root": str(self.workspace),
            "root": str(self.workspace / "Cargo.toml"),
        }
        self.vendor = self.enterContext(
            mock.patch.object(
                worker.Worker, "vendor_input", return_value=Path("/nix/store/vendor")
            )
        )
        self.enterContext(
            mock.patch.object(worker.Worker, "vendor_layout", return_value=({}, []))
        )
        self.sources = self.enterContext(mock.patch.object(worker, "SourceLayout"))
        self.sources.return_value.prepare.return_value = {
            "workspace": "/logical/workspace",
            "roots": [],
            "package_sources": ["/logical/workspace"],
        }
        self.namespace = self.enterContext(
            mock.patch.object(worker.Worker, "run_namespace")
        )
        self.namespace.side_effect = self.execute
        self.enterContext(
            mock.patch.dict(
                os.environ, {"CARGO_HOME": str(self.directory / "home")}, clear=True
            )
        )
        self.completion = {"version": 2, "status": 0, "nix_invocations": 0}
        self.driver_failure = None
        self.during_driver = lambda: None
        self.snapshot_failure = None
        self.snapshots = 0
        self.hooks = {}
        self.extra_runtime = {}

    def retain(self, references, owner, *, replace=False):
        roots = self.state / "roots" / owner
        roots.mkdir(parents=True, exist_ok=True, mode=0o700)
        names = set()
        for reference in references:
            root = roots / Path(reference).name
            names.add(root.name)
            if not root.is_symlink():
                root.symlink_to(reference)
        if replace:
            for root in roots.iterdir():
                if root.name not in names:
                    root.unlink()

    def execute(self, config, command, **kwargs):
        if command[1] == "metadata":
            return json.dumps({"packages": []})
        if command[1] == "snapshot-references":
            self.snapshots += 1
            for lock in (config.parent / "workspace.lock", self.state / "test-lease"):
                with (
                    self.assertRaises(BlockingIOError),
                    worker.locked(lock, blocking=False),
                ):
                    self.fail("snapshot must keep both locks")
            if self.snapshot_failure:
                raise self.snapshot_failure
            worker.write_json(
                Path(command[-1]), {"storePaths": ["/nix/store/compiler-output"]}
            )
        elif command[1] == "nix-cargo":
            target = config.parent / "stage/target"
            (target / ".dyndrv-delivered").mkdir(exist_ok=True)
            self.during_driver()
            if self.completion is not None:
                worker.write_json(
                    Path(command[3]) / "cargo-result.json", self.completion
                )
            if self.driver_failure:
                raise self.driver_failure
        else:
            self.assertEqual(command[0], "post-hook")

    def run_worker(self, action="check"):
        runner = worker.Worker.__new__(worker.Worker)
        runner.bootstrap = self.bootstrap
        runner.workspace = self.workspace
        runner.state = self.state
        runner.daemon = self.daemon
        runner.logical = Path("/logical")
        runner.action = action
        runner.arguments = [action] if action not in {"clean", "audit"} else []
        runner.spec = {
            "environment": {},
            "runtime_path": "/tools",
            "cargo": "/tools/cargo",
            "driver": "/tools/driver",
            "rustdoc": "/tools/rustdoc",
            "nix": "/tools/nix",
            "inputs": ["/nix/store/compiler-input"],
            "post_commands": self.hooks,
        }
        runner.environment = dict(self.extra_runtime)
        runner.workspace_names = runner.layout_names = runner.runtime_names = (
            runner.private_names
        ) = set()
        runner.runtime_prefixes = ()
        runner.source_overlay = False
        runner.artifact_delivery = "copy"
        return runner.run()

    @property
    def policy(self):
        return next((self.state / "workspaces").glob("*/*"))

    @property
    def report(self):
        return json.loads((self.policy / "last-run.json").read_text())

    @property
    def checkpoint(self):
        return self.policy / "retention.json"

    def test_first_command_reconciles_then_warm_reuse_preserves_existing_roots(self):
        def create_runtime():
            (self.policy / "stage/target/runtime").write_text(str(self.snapshots))

        self.during_driver = create_runtime
        self.run_worker()
        self.assertEqual(self.report["ownership_validation"], "full")
        roots = next((self.state / "roots").glob("*/*"))
        before = {path.name: path.lstat() for path in roots.iterdir()}
        self.daemon.retain.reset_mock()
        self.run_worker()
        self.assertEqual(self.snapshots, 1)
        self.assertEqual(self.report["ownership_validation"], "retained")
        self.assertEqual(self.report["nix_invocations"], 0)
        self.assertNotIn("snapshot_references", self.report["phase_seconds"])
        self.assertNotIn("retain", self.report["phase_seconds"])
        self.daemon.retain.assert_not_called()
        self.assertEqual(before, {path.name: path.lstat() for path in roots.iterdir()})
        self.assertEqual(self.sources.return_value.prepare.call_count, 2)
        self.assertEqual(self.daemon.provision.call_count, 2)

    def test_real_invocations_without_final_results_and_hooks_require_full_audit(self):
        self.run_worker()
        # Completed negative compilers have no successful final result to scan.
        self.completion["nix_invocations"] = 1
        self.run_worker()
        self.assertEqual(self.report["ownership_validation"], "full")
        self.completion["nix_invocations"] = 0
        self.hooks["check"] = [["post-hook", "argument"]]
        self.run_worker()
        self.assertEqual(self.snapshots, 3)
        self.hooks.clear()
        self.hooks["test"] = [["post-hook", "argument"]]
        self.run_worker()
        self.assertEqual(self.report["ownership_validation"], "retained")

    def test_missing_or_malformed_completion_never_reuses_retention(self):
        for completion in (
            None,
            {},
            {"version": 1, "status": 0, "nix_invocations": 0},
            *(
                {"version": 2, "status": 0, "nix_invocations": value}
                for value in (-1, True, 0.0, None)
            ),
            {"version": 2, "status": 256, "nix_invocations": 1},
        ):
            with self.subTest(completion=completion):
                self.run_worker("clean")
                self.completion = {"version": 2, "status": 0, "nix_invocations": 0}
                self.run_worker()
                self.completion = completion
                with self.assertRaisesRegex(
                    ValueError, "successful v2 Cargo completion"
                ):
                    self.run_worker()
                self.assertFalse(self.checkpoint.exists())
                self.assertNotEqual(self.report["status"], 0)
                with self.assertRaisesRegex(ValueError, "interrupted"):
                    self.run_worker("audit")

    def test_missing_checkpoint_replaced_directories_and_root_changes_reconcile(self):
        for name in (
            "checkpoint",
            "target",
            "ownership",
            "roots",
            "missing_root",
            "missing_roots",
            "added_runtime",
        ):
            with self.subTest(name=name):
                self.run_worker()
                before = self.snapshots
                identity = json.loads(self.checkpoint.read_text())
                if name == "checkpoint":
                    self.checkpoint.unlink()
                elif name == "missing_root":
                    (Path(identity["roots"]["path"]) / "compiler-output").unlink()
                elif name == "missing_roots":
                    shutil.rmtree(Path(identity["roots"]["path"]))
                elif name == "added_runtime":
                    self.extra_runtime["RUNTIME_INPUT"] = (
                        "/nix/store/" + "a" * 32 + "-extra"
                    )
                else:
                    path = Path(identity[name]["path"])
                    saved = path.with_name(path.name + "-saved")
                    path.rename(saved)
                    shutil.copytree(saved, path, symlinks=True)
                    shutil.rmtree(saved)
                self.run_worker()
                self.assertEqual(self.snapshots, before + 1)
                self.assertEqual(self.report["ownership_validation"], "full")

    def test_identity_change_during_delivery_free_command_requires_full_audit(self):
        self.run_worker()
        roots = Path(json.loads(self.checkpoint.read_text())["roots"]["path"])
        self.during_driver = lambda: (roots / "compiler-output").unlink()
        self.run_worker()
        self.assertEqual(self.snapshots, 2)

    def test_failure_invalidates_checkpoint_and_retains_backing_then_clean_removes_it(
        self,
    ):
        self.run_worker()
        self.driver_failure = subprocess.CalledProcessError(17, "driver")
        with self.assertRaises(subprocess.CalledProcessError) as failure:
            self.run_worker()
        self.assertEqual(failure.exception.returncode, 17)
        self.assertFalse(self.checkpoint.exists())
        self.assertFalse((self.policy / "in-progress.json").exists())
        self.daemon.retain.assert_called_with(set(), "/".join(self.policy.parts[-2:]))
        with self.assertRaisesRegex(ValueError, "successful prior session"):
            self.run_worker("audit")
        self.driver_failure = None
        self.run_worker()
        self.assertEqual(self.report["ownership_validation"], "full")
        self.assertEqual(self.snapshots, 2)
        session = Path(self.report["session"])
        self.run_worker("clean")
        self.assertFalse(self.checkpoint.exists())
        self.assertTrue(session.exists())

    def test_audit_uses_successful_stored_session_without_staging_and_keeps_extra_roots(
        self,
    ):
        self.run_worker()
        session = self.report["session"]
        roots = Path(json.loads(self.checkpoint.read_text())["roots"]["path"])
        (roots / "older-runtime").symlink_to("/nix/store/older-runtime")
        for operation in (
            self.namespace,
            self.metadata,
            self.vendor,
            self.sources,
            self.daemon.provision,
        ):
            operation.reset_mock()
        self.run_worker("audit")
        self.metadata.assert_called_once_with(locate=True)
        self.namespace.assert_called_once()
        self.assertEqual(
            self.namespace.call_args.args[1][1:4],
            ["snapshot-references", "--directory", session],
        )
        self.vendor.assert_not_called()
        self.sources.assert_not_called()
        self.daemon.provision.assert_not_called()
        self.assertTrue((roots / "older-runtime").is_symlink())
        self.assertEqual(self.report["action"], "audit")
        self.assertEqual(self.report["ownership_validation"], "full")
        self.assertTrue(self.checkpoint.exists())
        self.assertNotIn("driver", self.report["phase_seconds"])

    def test_unused_corruption_is_left_for_explicit_audit_and_audit_failure_preserves_roots(
        self,
    ):
        self.run_worker()
        roots = Path(json.loads(self.checkpoint.read_text())["roots"]["path"])
        before = {path.name: path.lstat() for path in roots.iterdir()}
        self.snapshot_failure = subprocess.CalledProcessError(2, "corrupt payload")
        self.run_worker()
        self.assertEqual(self.report["ownership_validation"], "retained")
        self.daemon.retain.reset_mock()
        with self.assertRaises(subprocess.CalledProcessError):
            self.run_worker("audit")
        self.assertFalse(self.checkpoint.exists())
        self.assertEqual(self.report["status"], 2)
        self.daemon.retain.assert_not_called()
        self.assertEqual(before, {path.name: path.lstat() for path in roots.iterdir()})

    def test_audit_requires_successful_session_and_refuses_rejected_targets(self):
        with self.assertRaisesRegex(ValueError, "successful prior session"):
            self.run_worker("audit")
        self.run_worker()
        (self.policy / "stage/target/.dyndrv-delivered/rejected").touch()
        with self.assertRaisesRegex(ValueError, "target was rejected"):
            self.run_worker("audit")
        self.assertFalse(self.checkpoint.exists())


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

    def test_retention_rejects_missing_backing_before_realizing_or_pruning(self):
        roots = self.daemon.state / "roots/workspace/policy"
        roots.mkdir(parents=True)
        old = roots / "old-output"
        old.symlink_to("/nix/store/old-output")
        missing = "/nix/store/missing-output"
        with (
            mock.patch.object(
                worker, "run", side_effect=subprocess.CalledProcessError(1, "path-info")
            ) as execute,
            self.assertRaises(subprocess.CalledProcessError),
        ):
            self.daemon.retain([missing], "workspace/policy", replace=True)
        execute.assert_called_once()
        self.assertEqual(execute.call_args.args[0][1], "path-info")
        self.assertEqual(os.readlink(old), "/nix/store/old-output")

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
                "version": 6,
                "owned": {"artifact": "store-link"},
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
        for representation in ("copy", "store-link"):
            with self.subTest(representation=representation):
                path = target / ".dyndrv-delivered/dependency.json"
                receipt = json.loads(path.read_text())
                receipt["owned"]["artifact"] = representation
                worker.write_json(path, receipt)
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

    def test_failed_build_rejects_old_artifact_schema_before_retaining_roots(self):
        runner = worker.Worker.__new__(worker.Worker)
        runner.daemon = self.daemon
        target = self.directory / "target"
        worker.write_json(
            target / ".dyndrv-delivered/dependency.json",
            {
                "version": 5,
                "owned": ["artifact"],
                "files": {"artifact": "/nix/store/artifact-owner/artifact"},
                "trees": {},
            },
        )
        with (
            mock.patch.object(self.daemon, "retain") as retain,
            self.assertRaisesRegex(ValueError, "Unsupported dyndrv artifact receipt"),
        ):
            runner.retain_failed(self.directory / "session", target, "workspace/policy")
        retain.assert_not_called()

    def test_failed_build_rejects_dependency_source_outside_store(self):
        runner = worker.Worker.__new__(worker.Worker)
        runner.daemon = self.daemon
        target = self.directory / "target"
        worker.write_json(
            target / ".dyndrv-delivered/dependency.json",
            {
                "version": 6,
                "owned": {},
                "files": {},
                "trees": {"source": "/tmp/mutable-source"},
            },
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
