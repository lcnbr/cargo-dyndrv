"""Local Cargo source staging, using disposable Git repositories and real Cargo."""

import json
import os
import shutil
import subprocess
import sys
import tempfile
import tomllib
import unittest
from pathlib import Path
from unittest import mock

import tomli_w

REPOSITORY = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(REPOSITORY / "worker"))
from sources import SourceLayout


class SourceLayoutTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="dyndrv source's ")
        self.addCleanup(temporary.cleanup)
        self.directory = Path(temporary.name)
        self.workspace = self.directory / "main workspace"
        self.external = self.directory / "local fork's workspace"
        self.stage = self.directory / "stage"
        self.logical = self.stage
        self.cargo = shutil.which("cargo")
        if not self.cargo:
            self.fail("Cargo must be supplied by the Nix test environment")
        self.env = dict(
            os.environ,
            CARGO_HOME=str(self.directory / "cargo-home"),
            HOME=str(self.directory / "home"),
            XDG_CONFIG_HOME=str(self.directory / "config"),
        )
        self.repository(self.workspace)
        self.package(self.workspace, "application")
        self.document(self.workspace / "Cargo.toml", workspace={"resolver": "2"})

    def command(self, *arguments, cwd=None):
        result = subprocess.run(
            arguments,
            cwd=cwd or self.workspace,
            env=self.env,
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        return result.stdout

    def repository(self, path, *arguments):
        path.mkdir(parents=True, exist_ok=True)
        self.command(
            "git", "init", "--quiet", *arguments, str(path), cwd=self.directory
        )

    @staticmethod
    def write(path, content):
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content)

    def document(self, path, **tables):
        document = tomllib.loads(path.read_text()) if path.exists() else {}
        document.update(tables)
        self.write(path, tomli_w.dumps(document))

    def package(self, path, name, version="1.0.0"):
        self.document(
            path / "Cargo.toml",
            package={"name": name, "version": version, "edition": "2021"},
        )
        self.write(path / "src/lib.rs", "pub fn value() -> u32 { 1 }\n")

    def metadata(self, workspace=None):
        return json.loads(
            self.command(
                self.cargo,
                "metadata",
                "--offline",
                "--format-version",
                "1",
                "--all-features",
                cwd=workspace,
            )
        )

    def layout(self, workspace=None, configurations=()):
        return SourceLayout(
            workspace or self.workspace,
            self.stage,
            self.logical,
            self.cargo,
            self.env,
            configurations=configurations,
        )

    def stage_state(self):
        return {
            path.relative_to(self.stage): (path.read_bytes(), path.stat().st_mtime_ns)
            for path in self.stage.rglob("*")
            if path.is_file()
        }

    def test_result_module_names_are_sources_while_nix_output_links_are_excluded(self):
        self.write(self.workspace / "src/results.rs", "pub fn value() -> u32 { 7 }\n")
        self.write(
            self.workspace / "src/lib.rs",
            "mod results;\npub fn value() -> u32 { results::value() }\n",
        )
        for name in ("result", "result-2"):
            (self.workspace / name).symlink_to("/nix/store/missing-output")
        prepared = self.layout().prepare(self.metadata())
        workspace = Path(prepared["workspace"])
        self.assertTrue((workspace / "src/results.rs").is_file())
        for name in ("result", "result-2"):
            self.assertFalse((workspace / name).is_symlink())
        self.env["CARGO_TARGET_DIR"] = str(self.directory / "build-target")
        self.env["CARGO_BUILD_BUILD_DIR"] = self.env["CARGO_TARGET_DIR"]
        self.command(self.cargo, "check", "--locked", "--offline", cwd=workspace)

    def patch_workspace(self):
        self.repository(self.external)
        member = self.external / "crates/local"
        self.package(member, "local")
        self.document(
            member / "Cargo.toml",
            package={
                "name": "local",
                "version": {"workspace": True},
                "edition": "2021",
            },
        )
        self.document(
            self.external / "Cargo.toml",
            workspace={"members": ["crates/local"], "package": {"version": "1.0.0"}},
        )
        self.document(
            self.workspace / "Cargo.toml",
            dependencies={"local": "1.0.0"},
            patch={
                "crates-io": {
                    "local": {"path": "../" + self.external.name + "/crates/local"}
                }
            },
        )
        return member

    def test_dirty_patch_new_module_and_whole_workspace_inheritance(self):
        member = self.patch_workspace()
        self.command("git", "add", ".", cwd=self.external)
        self.write(member / "src/lib.rs", "mod added; pub use added::value;\n")
        self.write(member / "src/added.rs", "pub fn value() -> u32 { 2 }\n")
        self.write(self.external / "assets/template.txt", "build-time asset")
        original = self.metadata()
        result = self.layout().prepare(original)
        staged = Path(result["roots"][1]["physical"])
        self.assertEqual(
            (staged / "crates/local/src/added.rs").read_text(),
            "pub fn value() -> u32 { 2 }\n",
        )
        self.assertEqual(
            (staged / "assets/template.txt").read_text(), "build-time asset"
        )
        self.assertEqual(
            tomllib.loads((staged / "Cargo.toml").read_text())["workspace"]["package"][
                "version"
            ],
            "1.0.0",
        )
        prepared = self.metadata(Path(result["workspace"]))
        self.assertEqual(
            sorted((p["name"], p["version"]) for p in prepared["packages"]),
            sorted((p["name"], p["version"]) for p in original["packages"]),
        )
        self.assertEqual(result["manifest"], str(self.stage / "workspace/Cargo.toml"))

    def test_dependency_tables_map_absolute_and_parent_relative_paths_only(self):
        member = self.patch_workspace()
        metadata = self.metadata()
        absolute = {"path": str(member)}
        relative = {"path": "../" + self.external.name + "/crates/local"}
        manifest = self.workspace / "Cargo.toml"
        self.document(
            manifest,
            dependencies={"local": absolute},
            **{
                "dev-dependencies": {"local": relative},
                "build-dependencies": {"local": absolute},
                "target": {"cfg(unix)": {"dependencies": {"local": relative}}},
                "workspace": {"dependencies": {"local": absolute}},
                "replace": {"local:1.0.0": relative},
                "package": {
                    "name": "application",
                    "version": "1.0.0",
                    "metadata": {
                        "path": str(member),
                        "dependencies": {"path": "literal"},
                    },
                },
                "lib": {"path": "src/lib.rs"},
            },
        )
        result = self.layout().prepare(metadata)
        document = tomllib.loads(Path(result["manifest"]).read_text())
        mapped = result["package_sources"][0] + "/crates/local"
        for table in ("dependencies", "dev-dependencies", "build-dependencies"):
            self.assertEqual(document[table]["local"]["path"], mapped)
        self.assertEqual(document["workspace"]["dependencies"]["local"]["path"], mapped)
        self.assertEqual(
            document["target"]["cfg(unix)"]["dependencies"]["local"]["path"], mapped
        )
        self.assertEqual(document["replace"]["local:1.0.0"]["path"], mapped)
        self.assertEqual(document["patch"]["crates-io"]["local"]["path"], mapped)
        self.assertEqual(document["package"]["metadata"]["path"], str(member))
        self.assertEqual(
            document["package"]["metadata"]["dependencies"]["path"], "literal"
        )
        self.assertEqual(document["lib"]["path"], "src/lib.rs")

    def test_absolute_target_and_inherited_package_paths_are_mapped(self):
        metadata = self.metadata()
        self.write(self.workspace / "README.txt", "readme")
        self.write(self.workspace / "LICENSE", "license")
        self.document(
            self.workspace / "Cargo.toml",
            workspace={
                "package": {
                    "readme": str(self.workspace / "README.txt"),
                    "license-file": str(self.workspace / "LICENSE"),
                }
            },
            lib={"path": str(self.workspace / "src/lib.rs")},
        )
        result = self.layout().prepare(metadata)
        document = tomllib.loads(Path(result["manifest"]).read_text())
        self.assertEqual(
            document["workspace"]["package"]["readme"],
            result["workspace"] + "/README.txt",
        )
        self.assertEqual(
            document["workspace"]["package"]["license-file"],
            result["workspace"] + "/LICENSE",
        )
        self.assertEqual(document["lib"]["path"], result["workspace"] + "/src/lib.rs")

    def test_member_addition_rename_and_removal_prune_stale_files(self):
        self.document(
            self.workspace / "Cargo.toml",
            workspace={"members": ["members/*"], "resolver": "2"},
        )
        first = self.workspace / "members/first"
        self.package(first, "first")
        self.layout().prepare(self.metadata())
        first.rename(self.workspace / "members/renamed")
        self.package(self.workspace / "members/added", "added")
        self.layout().prepare(self.metadata())
        self.assertFalse((self.stage / "workspace/members/first").exists())
        self.assertTrue((self.stage / "workspace/members/renamed/src/lib.rs").exists())
        shutil.rmtree(self.workspace / "members/renamed")
        self.layout().prepare(self.metadata())
        self.assertFalse((self.stage / "workspace/members/renamed").exists())
        self.assertTrue((self.stage / "workspace/members/added/src/lib.rs").exists())

    def test_external_mapping_survives_physical_directory_rename(self):
        member = self.patch_workspace()
        first = self.layout().prepare(self.metadata())
        moved = self.directory / "a completely different name"
        self.external.rename(moved)
        self.document(
            self.workspace / "Cargo.toml",
            patch={
                "crates-io": {
                    "local": {"path": str(moved / member.relative_to(self.external))}
                }
            },
        )
        second = self.layout().prepare(self.metadata())
        self.assertEqual(first["package_sources"], second["package_sources"])
        self.document(self.workspace / "Cargo.toml", dependencies={}, patch={})
        self.layout().prepare(self.metadata())
        self.assertFalse(Path(second["roots"][1]["physical"]).exists())

    def test_unused_patch_and_external_members_discover_dependencies_to_fixed_point(
        self,
    ):
        member = self.patch_workspace()
        spare = self.external / "crates/spare"
        self.package(spare, "spare")
        third = self.directory / "additional declared root"
        self.repository(third)
        self.package(third, "additional")
        self.write(third / "assets/new", "new external source")
        self.document(
            spare / "Cargo.toml",
            dependencies={"additional": {"workspace": True}},
        )
        self.document(
            self.external / "Cargo.toml",
            workspace={
                "members": ["crates/local", "crates/spare"],
                "package": {"version": "1.0.0"},
                "dependencies": {"additional": {"path": str(third)}},
            },
        )
        self.document(self.workspace / "Cargo.toml", dependencies={})
        metadata = self.metadata()
        self.assertEqual([p["name"] for p in metadata["packages"]], ["application"])
        result = self.layout().prepare(metadata)
        roots = {
            Path(root["original"]): Path(root["physical"]) for root in result["roots"]
        }
        self.assertEqual(set(roots), {self.workspace, self.external, third})
        self.assertTrue((roots[third] / "assets/new").is_file())
        self.assertTrue(
            (
                roots[self.external] / member.relative_to(self.external) / "src/lib.rs"
            ).is_file()
        )
        staged_spare = roots[self.external] / spare.relative_to(self.external)
        document = tomllib.loads((roots[self.external] / "Cargo.toml").read_text())
        self.assertEqual(
            document["workspace"]["dependencies"]["additional"]["path"],
            str(roots[third]),
        )
        self.metadata(Path(result["workspace"]))
        self.metadata(staged_spare)

    def test_external_identity_uses_all_members_when_selected_member_changes(self):
        self.patch_workspace()
        spare = self.external / "crates/spare"
        self.package(spare, "spare")
        self.document(
            self.external / "Cargo.toml",
            workspace={
                "members": ["crates/local", "crates/spare"],
                "package": {"version": "1.0.0"},
            },
        )
        first = self.layout().prepare(self.metadata())
        self.document(
            self.workspace / "Cargo.toml",
            dependencies={"spare": {"path": str(spare)}},
            patch={},
        )
        second = self.layout().prepare(self.metadata())
        self.assertEqual(first["package_sources"], second["package_sources"])

    def test_unused_replacement_root_is_discovered_without_resolving_versions(self):
        self.repository(self.external)
        self.package(self.external, "unused")
        self.document(
            self.workspace / "Cargo.toml",
            replace={"unused:1.0.0": {"path": str(self.external)}},
        )
        metadata = self.metadata()
        self.assertEqual([p["name"] for p in metadata["packages"]], ["application"])
        result = self.layout().prepare(metadata)
        document = tomllib.loads(Path(result["manifest"]).read_text())
        self.assertEqual(
            document["replace"]["unused:1.0.0"]["path"], result["package_sources"][0]
        )
        self.metadata(Path(result["workspace"]))

    def test_cyclic_unused_patch_declarations_terminate(self):
        self.repository(self.external)
        self.package(self.external, "first")
        other = self.directory / "second fork"
        self.repository(other)
        self.package(other, "second")
        for root, name, dependency in (
            (self.workspace, "first", self.external),
            (self.external, "second", other),
            (other, "first", self.external),
        ):
            self.document(
                root / "Cargo.toml",
                patch={"crates-io": {name: {"path": str(dependency)}}},
            )
        layout = self.layout()
        with mock.patch.object(layout, "_run", wraps=layout._run) as run:
            result = layout.prepare(self.metadata())
        self.assertEqual(len(result["roots"]), 3)
        self.assertEqual(
            sum(call.args[0][1] == "locate-project" for call in run.call_args_list), 2
        )

    def test_cargo_configuration_paths_map_and_keep_mtimes(self):
        self.repository(self.external)
        self.package(self.external, "unused")
        for name in ("config", "config.toml"):
            with self.subTest(name=name):
                source = self.workspace / ".cargo" / name
                self.document(
                    source,
                    paths=[str(self.external)],
                    patch={
                        "crates-io": {"unused": {"path": "../" + self.external.name}}
                    },
                    build={"rustflags": ["--cfg=configuration_test"]},
                )
                metadata = self.metadata()
                self.assertEqual(
                    [p["name"] for p in metadata["packages"]], ["application"]
                )
                layout = self.layout()
                result = layout.prepare(metadata)
                staged = Path(result["workspace"]) / ".cargo" / name
                document = tomllib.loads(staged.read_text())
                mapped = result["package_sources"][0]
                self.assertEqual(document["paths"], [mapped])
                self.assertEqual(
                    document["patch"]["crates-io"]["unused"]["path"], mapped
                )
                self.assertEqual(
                    document["build"]["rustflags"], ["--cfg=configuration_test"]
                )
                timestamp = staged.stat().st_mtime_ns
                self.assertEqual(timestamp, source.stat().st_mtime_ns)
                os.utime(source, ns=(timestamp + 100, timestamp + 100))
                layout.prepare(metadata)
                self.assertEqual(staged.stat().st_mtime_ns, timestamp)
                self.document(
                    source, build={"rustflags": ["--cfg=changed_configuration"]}
                )
                os.utime(source, ns=(timestamp, timestamp))
                layout.prepare(metadata)
                changed = staged.stat().st_mtime_ns
                self.assertGreater(changed, timestamp)
                layout.prepare(metadata)
                self.assertEqual(staged.stat().st_mtime_ns, changed)
                self.metadata(Path(result["workspace"]))
                source.unlink()

    def test_config_paths_array_alone_discovers_an_unused_root(self):
        self.repository(self.external)
        self.package(self.external, "unused")
        self.document(
            self.workspace / ".cargo/config.toml",
            paths=["../" + self.external.name],
        )
        metadata = self.metadata()
        self.assertEqual([p["name"] for p in metadata["packages"]], ["application"])
        result = self.layout().prepare(metadata)
        document = tomllib.loads(
            (Path(result["workspace"]) / ".cargo/config.toml").read_text()
        )
        self.assertEqual(document["paths"], result["package_sources"])

    def test_safe_cargo_directory_symlink_discovers_config_paths_from_alias_origin(
        self,
    ):
        self.repository(self.external)
        self.package(self.external, "unused")
        self.document(
            self.workspace / "config-data/nested/config.toml",
            patch={"crates-io": {"unused": {"path": "../" + self.external.name}}},
        )
        (self.workspace / ".cargo").symlink_to(
            "config-data/nested", target_is_directory=True
        )
        metadata = self.metadata()
        self.assertEqual([p["name"] for p in metadata["packages"]], ["application"])
        result = self.layout().prepare(metadata)
        source = Path(result["workspace"]) / ".cargo/config.toml"
        self.assertFalse(source.parent.is_symlink())
        document = tomllib.loads(source.read_text())
        self.assertEqual(
            document["patch"]["crates-io"]["unused"]["path"],
            result["package_sources"][0],
        )
        self.metadata(Path(result["workspace"]))

    def test_public_configuration_uses_config_origin_and_logical_paths(self):
        self.patch_workspace()
        self.logical = self.directory / "stable namespace"
        layout = self.layout()
        result = layout.prepare(self.metadata())
        source = self.directory / "ancestor/.cargo/config.toml"
        content = tomli_w.dumps(
            {
                "paths": ["../" + self.external.name + "/crates/local"],
                "patch": {
                    "crates-io": {
                        "local": {"path": str(self.external / "crates/local")}
                    }
                },
            }
        ).encode()
        # Relative config paths start above .cargo, including caller-supplied configs.
        document = tomllib.loads(
            layout.configuration(source, content, result["roots"]).decode()
        )
        mapped = result["package_sources"][0] + "/crates/local"
        self.assertEqual(document["paths"], [mapped])
        self.assertEqual(document["patch"]["crates-io"]["local"]["path"], mapped)
        self.assertFalse(self.logical.exists())

    def test_caller_supplied_unused_ancestor_and_global_patches_seed_discovery(self):
        self.patch_workspace()
        self.document(self.workspace / "Cargo.toml", dependencies={}, patch={})
        for name in (".cargo/config.toml", "cargo-home/config.toml"):
            with self.subTest(name=name):
                source = self.directory / name
                self.document(
                    source,
                    patch={
                        "crates-io": {
                            "local": {"path": self.external.name + "/crates/local"}
                        }
                    },
                )
                original = source.read_bytes()
                metadata = self.metadata()
                self.assertEqual(
                    [p["name"] for p in metadata["packages"]], ["application"]
                )
                layout = self.layout(configurations=[source])
                result = layout.prepare(metadata)
                self.assertEqual(len(result["package_sources"]), 1)
                mapped = result["package_sources"][0] + "/crates/local"
                document = tomllib.loads(
                    layout.configuration(source, original, result["roots"]).decode()
                )
                self.assertEqual(
                    document["patch"]["crates-io"]["local"]["path"], mapped
                )
                self.assertEqual(source.read_bytes(), original)
                self.assertFalse((Path(result["workspace"]) / ".cargo").exists())
                external = Path(result["roots"][1]["physical"])
                self.assertEqual(
                    tomllib.loads((external / "Cargo.toml").read_text())["workspace"][
                        "package"
                    ]["version"],
                    "1.0.0",
                )
                source.unlink()

    def test_caller_supplied_paths_and_replace_seed_unused_roots(self):
        self.repository(self.external)
        self.package(self.external, "unused")
        source = self.directory / "cargo-home/config.toml"
        for document in (
            {"paths": [self.external.name]},
            {"replace": {"unused:1.0.0": {"path": self.external.name}}},
        ):
            with self.subTest(document=document):
                self.write(source, tomli_w.dumps(document))
                metadata = self.metadata()
                self.assertEqual(
                    [p["name"] for p in metadata["packages"]], ["application"]
                )
                layout = self.layout(configurations=(source,))
                result = layout.prepare(metadata)
                mapped = result["package_sources"][0]
                rewritten = tomllib.loads(
                    layout.configuration(
                        source, source.read_bytes(), result["roots"]
                    ).decode()
                )
                if "paths" in document:
                    self.assertEqual(rewritten["paths"], [mapped])
                else:
                    self.assertEqual(
                        rewritten["replace"]["unused:1.0.0"]["path"], mapped
                    )

    def test_caller_supplied_config_keeps_ignored_and_excluded_root_rejections(self):
        metadata = self.metadata()
        self.layout().prepare(metadata)
        self.write(self.stage / "workspace/retained", "old stage")
        before = self.stage_state()
        self.repository(self.external)
        source = self.directory / "cargo-home/config.toml"
        self.write(self.external / ".gitignore", "ignored/\n")
        for name in ("ignored", "target/helper"):
            with self.subTest(name=name):
                root = self.external / name
                self.package(root, "unused")
                self.document(root / "Cargo.toml", workspace={})
                self.document(
                    source, patch={"crates-io": {"unused": {"path": str(root)}}}
                )
                with self.assertRaisesRegex(
                    ValueError, "excluded or missing|excluded repository path"
                ):
                    self.layout(configurations=(source,)).prepare(metadata)
                self.assertEqual(self.stage_state(), before)

    def test_source_and_rewritten_manifest_mtimes_are_persistent(self):
        member = self.patch_workspace()
        metadata = self.metadata()
        first = self.layout().prepare(metadata)
        paths = [self.workspace / "Cargo.toml", member / "src/lib.rs"]
        mapped = [
            Path(first["manifest"]),
            Path(first["package_sources"][0]) / "crates/local/src/lib.rs",
        ]
        timestamps = [p.stat().st_mtime_ns for p in paths]
        self.assertEqual([p.stat().st_mtime_ns for p in mapped], timestamps)
        self.layout().prepare(metadata)
        self.assertEqual([p.stat().st_mtime_ns for p in mapped], timestamps)

    def test_same_mtime_changed_bytes_bump_once_including_rewritten_manifest(self):
        self.patch_workspace()
        metadata = self.metadata()
        first = self.layout().prepare(metadata)
        source = self.workspace / "src/lib.rs"
        manifest = self.workspace / "Cargo.toml"
        originals = [source.stat(), manifest.stat()]
        self.write(source, "pub fn value() -> u32 { 9 }\n")
        self.document(manifest, features={"new-feature": []})
        for path, previous in zip((source, manifest), originals, strict=True):
            os.utime(path, ns=(previous.st_atime_ns, previous.st_mtime_ns))
        self.layout().prepare(metadata)
        outputs = [Path(first["workspace"]) / "src/lib.rs", Path(first["manifest"])]
        timestamps = [p.stat().st_mtime_ns for p in outputs]
        for actual, original in zip(timestamps, originals, strict=True):
            self.assertGreater(actual, original.st_mtime_ns)
        self.layout().prepare(metadata)
        self.assertEqual([p.stat().st_mtime_ns for p in outputs], timestamps)
        self.assertIn("new-feature", Path(first["manifest"]).read_text())

    def test_inventory_filters_secrets_state_and_caches_but_keeps_tracked_ignored_files(
        self,
    ):
        self.write(self.workspace / ".gitignore", "ignored*\n*.secret\n")
        self.write(self.workspace / "ignored-tracked", "useful tracked asset")
        self.command("git", "add", "-f", "ignored-tracked")
        excluded = [
            ".env",
            ".env.local",
            ".netrc",
            ".cargo/credentials.toml",
            "target/output",
            ".venv/bin/python",
            "result-link",
            ".jj/state",
            "__pycache__/mod.pyc",
            ".ruff_cache/value",
            "ignored.secret",
        ]
        for name in excluded:
            self.write(self.workspace / name, "excluded")
        # A .jj directory marks a JJ workspace; use Git's real marker here.
        shutil.rmtree(self.workspace / ".jj")
        self.write(
            self.workspace / "tests/resources/source.rs", "legitimate fixture source"
        )
        result = self.layout().prepare(self.metadata())
        root = Path(result["workspace"])
        for name in excluded:
            self.assertFalse((root / name).exists(), name)
        self.assertFalse((root / ".git").exists())
        self.assertEqual((root / "ignored-tracked").read_text(), "useful tracked asset")
        self.assertTrue((root / "tests/resources/source.rs").exists())

    def test_safe_file_and_directory_symlinks_copy_only_selected_children(self):
        self.write(self.workspace / "assets/value", "asset")
        self.write(self.workspace / "assets/.env", "secret")
        self.write(self.workspace / ".gitignore", "assets/ignored\n")
        self.write(self.workspace / "assets/ignored", "secret")
        (self.workspace / "file-alias").symlink_to("assets/value")
        (self.workspace / "directory-alias").symlink_to(
            "assets", target_is_directory=True
        )
        result = self.layout().prepare(self.metadata())
        root = Path(result["workspace"])
        self.assertEqual((root / "file-alias").read_text(), "asset")
        self.assertFalse((root / "file-alias").is_symlink())
        self.assertEqual((root / "directory-alias/value").read_text(), "asset")
        self.assertFalse((root / "directory-alias/.env").exists())
        self.assertFalse((root / "directory-alias/ignored").exists())

    def test_symlink_escapes_cycles_and_secret_aliases_fail_without_stage_writes(self):
        metadata = self.metadata()
        outside = self.directory / "outside-secret"
        self.write(outside, "secret")
        self.write(self.workspace / ".env", "secret")
        self.write(self.workspace / ".gitignore", "ignored\n")
        self.write(self.workspace / "ignored", "secret")
        link = self.workspace / "alias"
        for target in (str(outside), ".env", "ignored", ".git/config", ".", "alias"):
            with self.subTest(target=target):
                link.symlink_to(target)
                try:
                    with self.assertRaises(ValueError):
                        self.layout().prepare(metadata)
                    self.assertFalse(self.stage.exists())
                finally:
                    link.unlink()

    def test_parent_symlink_cannot_expose_ignored_files(self):
        tracked = self.workspace / "tracked/value"
        self.write(tracked, "public")
        self.command("git", "add", "tracked/value")
        self.write(self.workspace / ".gitignore", "ignored/\n")
        self.write(self.workspace / "ignored/value", "secret")
        shutil.rmtree(tracked.parent)
        tracked.parent.symlink_to("ignored", target_is_directory=True)
        with self.assertRaisesRegex(ValueError, "VCS-ignored"):
            self.layout().prepare(self.metadata())
        self.assertFalse(self.stage.exists())

    def test_ancestor_vcs_and_gitfile_workspaces(self):
        metadata = self.metadata()
        parent = self.directory / "repository ancestor"
        self.repository(parent, "--separate-git-dir", str(self.directory / "git-state"))
        nested = parent / "nested workspace"
        self.workspace.rename(nested)
        shutil.rmtree(nested / ".git")
        self.write(parent / "outside", "outside workspace")
        metadata["workspace_root"] = str(nested)
        metadata["packages"][0]["manifest_path"] = str(nested / "Cargo.toml")
        result = self.layout(nested).prepare(metadata)
        self.assertTrue((Path(result["workspace"]) / "src/lib.rs").exists())
        self.assertFalse((Path(result["workspace"]) / "outside").exists())
        self.assertFalse((Path(result["workspace"]) / ".git").exists())

    def test_discovery_skips_payload_resolution_but_full_plan_preserves_preflight(self):
        metadata = self.metadata()
        payload = self.workspace / "assets/payload"
        self.write(payload, "ordinary source bytes")
        self.command("git", "add", "assets/payload")
        layout = self.layout()
        layout.prepare(metadata)
        self.write(self.stage / "workspace/retained", "retain until preflight passes")
        before = self.stage_state()
        original_resolve = Path.resolve

        def resolve(path, *arguments, **options):
            if path == payload:
                self.fail("discovery resolved an ordinary payload")
            return original_resolve(path, *arguments, **options)

        with mock.patch.object(Path, "resolve", resolve):
            layout._discover(metadata)
        self.assertEqual(self.stage_state(), before)
        outside = self.directory / "outside-payload"
        self.write(outside, "outside declared roots")
        for case, error in (
            ("fifo", "special source"),
            ("escape", "escapes declared roots"),
        ):
            with self.subTest(case=case):
                payload.unlink()
                if case == "fifo":
                    os.mkfifo(payload)
                else:
                    payload.symlink_to(outside)
                with self.assertRaisesRegex(ValueError, error):
                    layout.prepare(metadata)
                self.assertEqual(self.stage_state(), before)

    def test_all_manifests_and_special_files_are_validated_before_stale_deletion(self):
        metadata = self.metadata()
        self.layout().prepare(metadata)
        retained = self.stage / "workspace/retained"
        self.write(retained, "old stage must survive failed preflight")
        invalid = self.workspace / "fixtures/Cargo.toml"
        self.write(invalid, "invalid = [")
        with self.assertRaises(tomllib.TOMLDecodeError):
            self.layout().prepare(metadata)
        self.assertTrue(retained.exists())
        self.command("git", "add", "fixtures/Cargo.toml")
        invalid.unlink()
        os.mkfifo(invalid)
        with self.assertRaisesRegex(ValueError, "special source"):
            self.layout().prepare(metadata)
        self.assertTrue(retained.exists())
        invalid.unlink()
        source = self.workspace / "special"
        self.write(source, "initial regular file")
        self.command("git", "add", "special")
        source.unlink()
        os.mkfifo(source)
        with self.assertRaisesRegex(ValueError, "special source"):
            self.layout().prepare(metadata)
        self.assertTrue(retained.exists())

    def test_no_vcs_and_destination_symlinks_fail_without_mutation(self):
        metadata = self.metadata()
        shutil.rmtree(self.workspace / ".git")
        with self.assertRaisesRegex(
            ValueError, "without JJ or Git|not a git repository"
        ):
            self.layout().prepare(metadata)
        self.assertFalse(self.stage.exists())
        self.repository(self.workspace)
        self.stage.mkdir()
        (self.stage / "workspace").symlink_to(self.workspace, target_is_directory=True)
        with self.assertRaisesRegex(ValueError, "staged file or symlink"):
            self.layout().prepare(metadata)
        self.assertEqual(
            (self.workspace / "src/lib.rs").read_text(), "pub fn value() -> u32 { 1 }\n"
        )

    def test_nested_repository_inventory_is_retained_in_parent_mapping(self):
        nested = self.workspace / "vendor/nested"
        self.repository(nested)
        self.package(nested, "nested")
        self.document(nested / "Cargo.toml", workspace={})
        self.write(self.workspace / ".gitignore", "vendor/\n")
        self.write(nested / "assets/template", "nested workspace asset")
        self.document(
            self.workspace / "Cargo.toml",
            dependencies={"nested": {"path": "vendor/nested"}},
            workspace={"exclude": ["vendor/nested"], "resolver": "2"},
        )
        result = self.layout().prepare(self.metadata())
        self.assertEqual(result["package_sources"], [])
        self.assertEqual(len(result["roots"]), 1)
        self.assertEqual(
            (Path(result["workspace"]) / "vendor/nested/assets/template").read_text(),
            "nested workspace asset",
        )
        self.assertFalse((Path(result["workspace"]) / "vendor/nested/.git").exists())
        self.metadata(Path(result["workspace"]))

    def test_unused_patch_in_nested_ignored_repository_keeps_parent_mapping(self):
        nested = self.workspace / "vendor/unused"
        self.repository(nested)
        self.package(nested, "unused")
        self.document(nested / "Cargo.toml", workspace={})
        self.write(nested / "assets/new", "new nested asset")
        self.write(self.workspace / ".gitignore", "vendor/\n")
        self.document(
            self.workspace / "Cargo.toml",
            patch={"crates-io": {"unused": {"path": "vendor/unused"}}},
        )
        metadata = self.metadata()
        self.assertEqual([p["name"] for p in metadata["packages"]], ["application"])
        result = self.layout().prepare(metadata)
        self.assertEqual(result["package_sources"], [])
        staged = Path(result["workspace"]) / "vendor/unused"
        self.assertEqual((staged / "assets/new").read_text(), "new nested asset")
        self.metadata(Path(result["workspace"]))

    def test_nested_submodule_boundaries_keep_assets_and_remove_every_gitlink(self):
        outer = self.workspace / "outer"
        inner = outer / "inner"
        self.repository(outer)
        self.repository(inner)
        self.package(inner, "deep")
        self.document(inner / "Cargo.toml", workspace={})
        self.write(outer / "assets/outer", "outer repository asset")
        self.write(inner / "assets/inner", "inner repository asset")
        # Gitlinks may reference commits that are absent from the parent store.
        # No fixture commits are needed to reproduce nested submodule inventory.
        for root, name in ((self.workspace, "outer"), (outer, "inner")):
            self.command(
                "git",
                "update-index",
                "--add",
                "--cacheinfo",
                f"160000,{'a' * 40},{name}",
                cwd=root,
            )
        self.document(
            self.workspace / "Cargo.toml",
            dependencies={"deep": {"path": "outer/inner"}},
            workspace={"exclude": ["outer"], "resolver": "2"},
        )
        result = self.layout().prepare(self.metadata())
        self.assertEqual(result["package_sources"], [])
        staged = Path(result["workspace"]) / "outer"
        self.assertEqual(
            (staged / "assets/outer").read_text(), "outer repository asset"
        )
        self.assertEqual(
            (staged / "inner/assets/inner").read_text(), "inner repository asset"
        )
        self.metadata(Path(result["workspace"]))

    def test_every_local_package_manifest_must_survive_inventory_before_mutation(self):
        metadata = self.metadata()
        self.layout().prepare(metadata)
        self.write(self.stage / "workspace/retained", "previous stage")
        excluded = self.workspace / "target/excluded"
        self.package(excluded, "excluded")
        metadata["packages"].append(
            {
                "name": "excluded",
                "version": "1.0.0",
                "source": None,
                "manifest_path": str(excluded / "Cargo.toml"),
            }
        )
        with self.assertRaisesRegex(ValueError, "local package manifest is excluded"):
            self.layout().prepare(metadata)
        self.assertTrue((self.stage / "workspace/retained").exists())

    @unittest.skipUnless(shutil.which("jj"), "JJ is not installed")
    def test_jj_inventory_does_not_auto_track_new_excluded_sentinels(self):
        metadata = self.metadata()
        self.command("jj", "git", "init", "--colocate", str(self.workspace))
        self.command("jj", "config", "set", "--repo", "snapshot.auto-track", "none()")
        excluded = (
            ".env",
            ".netrc",
            "nested/.env.local",
            "nested/credentials.toml",
            "nested/cache.pyc",
            "nested/result-cache/value",
        )
        for name in excluded:
            self.write(self.workspace / name, "harmless inventory sentinel\n")
        self.write(self.workspace / "src/added.rs", "pub fn added() {}\n")
        result = self.layout().prepare(metadata)
        tracked = set(
            self.command("jj", "--ignore-working-copy", "file", "list").splitlines()
        )
        self.assertIn("src/added.rs", tracked)
        self.assertTrue((Path(result["workspace"]) / "src/added.rs").is_file())
        for name in excluded:
            self.assertNotIn(name, tracked)
            self.assertFalse((Path(result["workspace"]) / name).exists())
        self.assertEqual(
            self.command(
                "jj", "--ignore-working-copy", "config", "get", "snapshot.auto-track"
            ).strip(),
            "none()",
        )

    @unittest.skipUnless(shutil.which("jj"), "JJ is not installed")
    def test_jj_includes_new_large_files_despite_user_snapshot_configuration(self):
        metadata = self.metadata()
        self.command("jj", "git", "init", "--colocate", str(self.workspace))
        self.command("jj", "config", "set", "--repo", "snapshot.auto-track", "none()")
        self.command("jj", "config", "set", "--repo", "snapshot.max-new-file-size", "1")
        self.write(self.workspace / "large-new-asset", "x" * (1024 * 1024 + 1))
        result = self.layout().prepare(metadata)
        self.assertEqual(
            (Path(result["workspace"]) / "large-new-asset").stat().st_size,
            1024 * 1024 + 1,
        )

    def test_readonly_sources_update_and_worker_state_outside_roots_survives(self):
        source = self.workspace / "src/lib.rs"
        source.chmod(0o444)
        metadata = self.metadata()
        self.layout().prepare(metadata)
        self.write(self.stage / "worker-state", "worker owns this")
        source.chmod(0o644)
        self.write(source, "pub fn value() -> u32 { 2 }\n")
        source.chmod(0o444)
        self.layout().prepare(metadata)
        staged = self.stage / "workspace/src/lib.rs"
        self.assertIn("{ 2 }", staged.read_text())
        self.assertEqual(staged.stat().st_mode & 0o777, 0o444)
        self.assertEqual((self.stage / "worker-state").read_text(), "worker owns this")
        self.assertFalse(list(self.stage.rglob(".dyndrv-source-*")))

    def test_ambiguous_external_identities_are_rejected(self):
        self.patch_workspace()
        metadata = self.metadata()
        other = self.directory / "same identity"
        self.repository(other)
        self.package(other, "local")
        metadata["packages"].append(
            {
                "name": "local",
                "version": "1.0.0",
                "source": None,
                "manifest_path": str(other / "Cargo.toml"),
            }
        )
        with self.assertRaisesRegex(ValueError, "ambiguous"):
            self.layout().prepare(metadata)
        self.assertFalse(self.stage.exists())

    def test_logical_namespace_and_requested_member_manifest_are_separate_from_stage(
        self,
    ):
        member = self.workspace / "member"
        self.package(member, "member")
        self.document(self.workspace / "Cargo.toml", workspace={"members": ["member"]})
        self.logical = self.directory / "namespace that does not exist"
        metadata = self.metadata()
        metadata["packages"].extend(
            {"source": source, "manifest_path": "/unused/manifest/Cargo.toml"}
            for source in (
                "registry+https://example.invalid/index",
                "git+https://example.invalid/repo",
            )
        )
        result = self.layout(member).prepare(metadata)
        self.assertEqual(result["workspace"], str(self.logical / "workspace"))
        self.assertEqual(
            result["manifest"], str(self.logical / "workspace/member/Cargo.toml")
        )
        self.assertEqual(result["roots"][0]["physical"], str(self.stage / "workspace"))
        self.assertTrue((self.stage / "workspace/member/Cargo.toml").exists())
        self.assertFalse(self.logical.exists())

    def test_apply_uses_preflight_bytes_and_replaces_existing_hardlinks(self):
        metadata = self.metadata()
        source = self.workspace / "src/lib.rs"
        layout = self.layout()
        original_inventory = layout._stage_inventory

        def mutate_after_preflight(roots):
            self.write(source, "new source after preflight\n")
            return original_inventory(roots)

        with mock.patch.object(
            layout, "_stage_inventory", side_effect=mutate_after_preflight
        ):
            result = layout.prepare(metadata)
        staged = Path(result["workspace"]) / "src/lib.rs"
        self.assertEqual(staged.read_text(), "pub fn value() -> u32 { 1 }\n")
        outside = self.directory / "outside-hardlink"
        os.link(staged, outside)
        self.layout().prepare(metadata)
        self.assertEqual(outside.read_text(), "pub fn value() -> u32 { 1 }\n")
        self.assertEqual(staged.read_text(), "new source after preflight\n")

    def test_missing_invalid_and_non_vcs_unused_roots_preserve_stage(self):
        metadata = self.metadata()
        self.layout().prepare(metadata)
        retained = self.stage / "workspace/retained"
        self.write(retained, "old stage")
        before = self.stage_state()
        outside = self.directory / "not-in-metadata"
        for case in ("missing", "invalid", "non-vcs", "symlink-escape"):
            with self.subTest(case=case):
                if case == "invalid":
                    self.write(outside / "Cargo.toml", "invalid = [")
                elif case == "non-vcs":
                    (outside / "Cargo.toml").unlink()
                    self.package(outside, "unused")
                elif case == "symlink-escape":
                    self.repository(outside)
                    secret = self.directory / "outside-secret"
                    self.write(secret, "secret")
                    (outside / "escape").symlink_to(secret)
                self.document(
                    self.workspace / "Cargo.toml",
                    patch={"crates-io": {"unused": {"path": str(outside)}}},
                )
                with self.assertRaises(ValueError):
                    self.layout().prepare(metadata)
                self.assertEqual(self.stage_state(), before)

    def test_external_root_cannot_hide_an_excluded_ancestor_repository_path(self):
        metadata = self.metadata()
        self.layout().prepare(metadata)
        self.write(self.stage / "workspace/retained", "old stage")
        before = self.stage_state()
        self.repository(self.external)
        excluded = self.external / "target/helper"
        self.package(excluded, "unused")
        self.document(excluded / "Cargo.toml", workspace={})
        self.document(
            self.workspace / "Cargo.toml",
            patch={"crates-io": {"unused": {"path": str(excluded)}}},
        )
        with self.assertRaisesRegex(ValueError, "excluded repository path"):
            self.layout().prepare(metadata)
        self.assertEqual(self.stage_state(), before)

    def test_external_root_cannot_capture_main_or_stage(self):
        enclosing = self.directory / "enclosing workspace"
        enclosing.mkdir()
        moved = enclosing / self.workspace.name
        self.workspace.rename(moved)
        self.workspace = moved
        outsider = enclosing / "outsider"
        self.package(outsider, "unused")
        self.document(
            enclosing / "Cargo.toml",
            workspace={"members": ["outsider"], "exclude": [self.workspace.name]},
        )
        metadata = self.metadata()
        self.layout().prepare(metadata)
        self.write(self.stage / "workspace/retained", "old stage")
        before = self.stage_state()
        self.document(
            self.workspace / "Cargo.toml",
            patch={"crates-io": {"unused": {"path": "../outsider"}}},
        )
        with self.assertRaisesRegex(ValueError, "source root overlaps another root"):
            self.layout().prepare(metadata)
        self.assertEqual(self.stage_state(), before)
        # A separately declared root must not absorb the worker-owned stage.
        self.repository(self.external)
        self.package(self.external, "unused")
        self.stage = self.external / "stage"
        self.write(self.stage / "workspace/retained", "old stage")
        before = self.stage_state()
        self.document(
            self.workspace / "Cargo.toml",
            patch={"crates-io": {"unused": {"path": str(self.external)}}},
        )
        with self.assertRaisesRegex(ValueError, "stage overlaps source root"):
            self.layout().prepare(metadata)
        self.assertEqual(self.stage_state(), before)

    def test_external_member_outside_discovered_workspace_is_rejected(self):
        member = self.patch_workspace()
        located = json.dumps(
            {"root": str(self.directory / "unrelated/Cargo.toml")}
        ).encode()
        layout = self.layout()
        with (
            mock.patch.object(layout, "_run", return_value=located),
            self.assertRaisesRegex(ValueError, "outside its workspace"),
        ):
            layout.prepare(
                {
                    "workspace_root": str(self.workspace),
                    "packages": [
                        {
                            "name": "local",
                            "version": "1.0.0",
                            "source": None,
                            "manifest_path": str(member / "Cargo.toml"),
                        }
                    ],
                }
            )
        self.assertFalse(self.stage.exists())

    def test_legacy_cargo_tables_rewrite_paths_with_real_edition_2021_metadata(self):
        member = self.patch_workspace()
        manifest = self.workspace / "Cargo.toml"
        document = tomllib.loads(manifest.read_text())
        document["project"] = document.pop("package")
        document["project"]["readme"] = str(self.workspace / "README.txt")
        document["dependencies"] = {}
        document["dev_dependencies"] = {"local": {"path": str(member)}}
        document["target"] = {
            "cfg(unix)": {
                "build_dependencies": {
                    "local": {"path": "../" + self.external.name + "/crates/local"}
                }
            }
        }
        self.write(self.workspace / "README.txt", "readme")
        manifest.write_text(tomli_w.dumps(document))
        result = self.layout().prepare(self.metadata())
        staged = tomllib.loads(Path(result["manifest"]).read_text())
        mapped = result["package_sources"][0] + "/crates/local"
        self.assertEqual(staged["dev_dependencies"]["local"]["path"], mapped)
        self.assertEqual(
            staged["target"]["cfg(unix)"]["build_dependencies"]["local"]["path"],
            mapped,
        )
        self.assertEqual(
            staged["project"]["readme"], result["workspace"] + "/README.txt"
        )
        self.metadata(Path(result["workspace"]))


if __name__ == "__main__":
    unittest.main()
