"""Prepare VCS-selected local Cargo sources for the worker's stable namespace.

Cargo metadata owns dependency resolution. The caller owns the stage directory
and its lock, and supplies any Cargo configuration outside the source roots.
Explicit Cargo path declarations extend the local source roots to a fixed
point, including unused patches and other external-workspace members.
"""

import hashlib
import itertools
import json
import os
import stat
import subprocess
import tempfile
import time
import tomllib
from pathlib import Path

import tomli_w


class SourceLayout:
    _EXCLUDED_NAMES = (
        ".git",
        ".jj",
        "target",
        ".venv",
        "__pycache__",
        ".env",
        ".netrc",
        "credentials",
        "credentials.toml",
        ".direnv",
        ".pytest_cache",
        ".mypy_cache",
        ".ruff_cache",
        "result",
    )
    _EXCLUDED_PREFIXES = ("result-", ".env.")
    _EXCLUDED_SUFFIXES = (".pyc", ".pyo")

    def __init__(
        self,
        workspace: Path,
        stage: Path,
        logical: Path,
        cargo: str,
        env: dict | None = None,
        configurations=(),
    ):
        self.workspace = workspace.resolve()
        self.stage = Path(os.path.abspath(stage))
        self.logical = Path(os.path.abspath(logical))
        self.cargo = cargo
        self.env = env
        self.configurations = tuple(
            Path(os.path.abspath(path)) for path in configurations
        )

    def _run(self, arguments, cwd):
        result = subprocess.run(
            arguments, cwd=cwd, env=self.env, capture_output=True, check=False
        )
        if result.returncode:
            raise ValueError(
                f"{arguments[0]} failed while preparing sources in {cwd}: "
                + result.stderr.decode(errors="replace").strip()
            )
        return result.stdout

    @classmethod
    def _excluded(cls, path):
        return any(
            part in cls._EXCLUDED_NAMES
            or part.startswith(cls._EXCLUDED_PREFIXES)
            or part.endswith(cls._EXCLUDED_SUFFIXES)
            for part in path.parts
        )

    def _inventory(self, root):
        for repository in (root, *root.parents):
            if (repository / ".jj").is_dir():
                patterns = (
                    *self._EXCLUDED_NAMES,
                    *(prefix + "*" for prefix in self._EXCLUDED_PREFIXES),
                    *("*" + suffix for suffix in self._EXCLUDED_SUFFIXES),
                )
                auto_track = (
                    "all() ~ ("
                    + " | ".join(
                        "root-prefix-glob:" + json.dumps("**/" + pattern)
                        for pattern in patterns
                    )
                    + ")"
                )
                command = [
                    "jj",
                    "--config",
                    "snapshot.max-new-file-size=0",
                    "--config",
                    "snapshot.auto-track=" + json.dumps(auto_track),
                    "file",
                    "list",
                    "--no-pager",
                    "--color",
                    "never",
                    "-T",
                    'path ++ "\\0"',
                ]
                break
            if (repository / ".git").exists():
                command = [
                    "git",
                    "ls-files",
                    "--cached",
                    "--others",
                    "--exclude-standard",
                    "-z",
                ]
                break
        else:
            raise ValueError(f"cannot inventory source root without JJ or Git: {root}")
        if self._excluded(root.relative_to(repository)):
            raise ValueError(f"source root is in an excluded repository path: {root}")
        paths = set()
        for name in self._run(command, repository).split(b"\0"):
            if not name:
                continue
            relative = Path(os.fsdecode(name))
            if relative.is_absolute() or ".." in relative.parts:
                raise ValueError(f"VCS path escapes source repository: {relative}")
            path = repository / relative
            # Deleted tracked files remain in Git's index.
            if (
                path.is_relative_to(root)
                and not self._excluded(path.relative_to(root))
                and (path.exists() or path.is_symlink())
            ):
                paths.add(path)
        return paths

    def _discover(self, metadata):
        main = Path(metadata["workspace_root"]).resolve()
        if not self.workspace.is_relative_to(main):
            raise ValueError(f"requested workspace {self.workspace} is outside {main}")
        configurations = set(self.configurations)
        for source in configurations:
            if not source.is_file():
                raise ValueError(
                    f"Cargo configuration is missing or not a regular file: {source}"
                )
        manifests = {
            Path(package["manifest_path"]).resolve()
            for package in metadata["packages"]
            if package.get("source") is None
        }
        roots = {main}
        workspaces = {}
        located = {manifest for manifest in manifests if manifest.is_relative_to(main)}
        inspected = set()
        while True:
            for manifest in sorted(manifests - located):
                if not manifest.is_file():
                    raise ValueError(f"local Cargo manifest is missing: {manifest}")
                location = json.loads(
                    self._run(
                        [
                            self.cargo,
                            "locate-project",
                            "--workspace",
                            "--manifest-path",
                            str(manifest),
                            "--message-format",
                            "json",
                        ],
                        manifest.parent,
                    )
                )
                root = Path(location["root"]).resolve().parent
                if not manifest.is_relative_to(root):
                    raise ValueError(
                        f"Cargo located {manifest} outside its workspace {root}"
                    )
                if self.stage.is_relative_to(root) or root.is_relative_to(self.stage):
                    raise ValueError(f"stage overlaps source root: {root}")
                if not any(root.is_relative_to(owner) for owner in roots):
                    if any(owner.is_relative_to(root) for owner in roots):
                        raise ValueError(f"source root overlaps another root: {root}")
                    roots.add(root)
                if root != main and root not in workspaces:
                    workspace = json.loads(
                        self._run(
                            [
                                self.cargo,
                                "metadata",
                                "--no-deps",
                                "--offline",
                                "--format-version",
                                "1",
                                "--manifest-path",
                                str(root / "Cargo.toml"),
                            ],
                            root,
                        )
                    )
                    if Path(workspace["workspace_root"]).resolve() != root:
                        raise ValueError(f"Cargo workspace root changed: {root}")
                    for package in workspace["packages"]:
                        member = Path(package["manifest_path"]).resolve()
                        if not member.is_relative_to(root):
                            raise ValueError(
                                f"Cargo located {member} outside its workspace {root}"
                            )
                    workspaces[root] = workspace["packages"]
                    manifests.update(
                        Path(package["manifest_path"]).resolve()
                        for package in workspace["packages"]
                        if package.get("source") is None
                    )
                located.add(manifest)

            inventories = self._inventories(roots, manifests)
            sources = (
                (source, resolved)
                for _, source, resolved in self._source_files(
                    inventories, discovering=True
                )
            )
            for source, resolved in itertools.chain(
                ((source, source) for source in self.configurations), sources
            ):
                if source in inspected:
                    continue
                configuration = source in configurations or (
                    source.name in {"config", "config.toml"}
                    and source.parent.name == ".cargo"
                )
                if source.name != "Cargo.toml" and not configuration:
                    continue
                document = tomllib.loads(resolved.read_text())
                base = source.parent.parent if configuration else source.parent
                paths = [
                    dependency["path"]
                    for dependency in self._path_dependencies(document)
                ]
                if configuration:
                    paths.extend(document.get("paths", []))
                for path in paths:
                    manifest = (base / path / "Cargo.toml").resolve()
                    if not manifest.is_file():
                        raise ValueError(f"local Cargo manifest is missing: {manifest}")
                    manifests.add(manifest)
                inspected.add(source)
            # New declarations and Cargo-reported members are each visited once;
            # cycles stop when the manifest set reaches this fixed point.
            if manifests <= located:
                break

        mapped_roots = [(main, Path("workspace"))]
        identities = {}
        for root in sorted(roots - {main}):
            identity = sorted(
                {
                    (package["name"], package["version"])
                    for package in workspaces[root]
                    if Path(package["manifest_path"]).resolve().is_relative_to(root)
                }
            )
            if not identity:
                raise ValueError(f"Cargo source root has no local packages: {root}")
            stable = hashlib.sha256(json.dumps(identity).encode()).hexdigest()[:24]
            if stable in identities:
                raise ValueError(
                    f"ambiguous local source identity: {root} and {identities[stable]}"
                )
            identities[stable] = root
            mapped_roots.append((root, Path("sources") / stable))
        return (
            [mapped_roots[0], *sorted(mapped_roots[1:], key=lambda root: str(root[1]))],
            manifests,
        )

    @staticmethod
    def _path_dependencies(document):
        tables = [document, document.get("workspace", {})]
        tables.extend(document.get("target", {}).values())
        paths = list(document.get("patch", {}).values())
        paths.append(document.get("replace", {}))
        for table in tables:
            for kind in (
                "dependencies",
                "dev-dependencies",
                "build-dependencies",
                "dev_dependencies",
                "build_dependencies",
            ):
                paths.append(table.get(kind, {}))
        for table in paths:
            for dependency in table.values():
                if isinstance(dependency, dict) and "path" in dependency:
                    yield dependency

    def _mapped(self, path, roots):
        path = path.resolve()
        for original, relative in sorted(roots, key=lambda root: -len(root[0].parts)):
            if path.is_relative_to(original):
                return self.logical / relative / path.relative_to(original)
        raise ValueError(
            f"Cargo path is outside the resolved local source roots: {path}"
        )

    def _manifest(self, source, content, roots):
        document = tomllib.loads(content.decode())
        changed = False

        for dependency in self._path_dependencies(document):
            original = dependency["path"]
            mapped = str(self._mapped(source.parent / original, roots))
            if original != mapped:
                dependency["path"] = mapped
                changed = True

        def absolute(table, fields):
            nonlocal changed
            for field in fields:
                value = table.get(field)
                if isinstance(value, str) and Path(value).is_absolute():
                    mapped = str(self._mapped(Path(value), roots))
                    if value != mapped:
                        table[field] = mapped
                        changed = True

        package_fields = (
            "build",
            "readme",
            "license-file",
            "license_file",
            "workspace",
        )
        absolute(document.get("package", {}), package_fields)
        absolute(document.get("project", {}), package_fields)
        absolute(document.get("workspace", {}).get("package", {}), package_fields)
        absolute(document.get("lib", {}), ("path",))
        for kind in ("bin", "example", "test", "bench"):
            for target in document.get(kind, []):
                absolute(target, ("path",))
        return tomli_w.dumps(document).encode() if changed else content

    def configuration(self, source, content, root_specs):
        """Map Cargo configuration paths through its resolved source roots."""
        roots = [
            (Path(root["original"]), Path(root["logical"]).relative_to(self.logical))
            for root in root_specs
        ]
        return self._configuration(source, content, roots)

    def _configuration(self, source, content, roots):
        base = source.parent.parent
        result = self._manifest(base / "Cargo.toml", content, roots)
        document = tomllib.loads(result.decode())
        if "paths" in document:
            document["paths"] = [
                str(self._mapped(base / path, roots)) for path in document["paths"]
            ]
            result = tomli_w.dumps(document).encode()
        return result

    def _inventories(self, roots, manifests):
        inventories = {root: self._inventory(root) for root in roots}
        # A dependency's repository can be nested inside an ignored directory
        # of another source root (including Git submodules). Keep its inventory
        # even though its physical tree shares the enclosing root's mapping.
        repositories = {}
        for manifest in manifests:
            owner = next(
                root
                for root in sorted(roots, key=lambda root: -len(root.parts))
                if manifest.is_relative_to(root)
            )
            for directory in (manifest.parent, *manifest.parent.parents):
                if directory == owner:
                    break
                if (
                    (directory / ".jj").is_dir() or (directory / ".git").exists()
                ) and not self._excluded(directory.relative_to(owner)):
                    repositories[directory] = owner
        # Outer inventories can contain inner gitlinks. Expand them in order so
        # each link is removed after its enclosing repository is inventoried.
        for directory in sorted(repositories, key=lambda path: len(path.parts)):
            owner = repositories[directory]
            inventories[owner].discard(directory)
            inventories[owner].update(self._inventory(directory))
        return inventories

    def _source_files(self, inventories, discovering=False):
        selected = set().union(*inventories.values())

        def expand(source, ancestors):
            try:
                resolved = source.resolve(strict=True)
            except (OSError, RuntimeError) as error:
                raise ValueError(
                    f"broken or cyclic source symlink: {source}"
                ) from error
            owners = [root for root in inventories if resolved.is_relative_to(root)]
            if not owners:
                # Explicit declarations may introduce this target's root on the
                # next discovery pass. The final plan always requires ownership.
                if discovering:
                    return
                raise ValueError(f"source symlink escapes declared roots: {source}")
            if any(self._excluded(resolved.relative_to(root)) for root in owners):
                raise ValueError(f"source symlink targets an excluded path: {source}")
            mode = resolved.stat().st_mode
            if stat.S_ISDIR(mode):
                if not source.is_symlink():
                    raise ValueError(f"unsupported directory VCS entry: {source}")
                if resolved in ancestors:
                    raise ValueError(f"cyclic source directory symlink: {source}")
                for child in sorted(selected):
                    if child.is_relative_to(resolved):
                        yield from expand(
                            source / child.relative_to(resolved),
                            ancestors | {resolved},
                        )
                return
            if not stat.S_ISREG(mode):
                raise ValueError(f"unsupported special source file: {source}")
            if resolved != source and resolved not in selected:
                raise ValueError(f"source symlink targets a VCS-ignored file: {source}")
            yield source, resolved

        for root, inventory in inventories.items():
            for source in sorted(inventory):
                for alias, resolved in expand(source, {root}):
                    yield root, alias, resolved

    def _plan(self, roots, manifests):
        inventories = self._inventories([root for root, _ in roots], manifests)
        relative_roots = dict(roots)
        files = {}
        for root, source, resolved in self._source_files(inventories):
            destination = relative_roots[root] / source.relative_to(root)
            content = resolved.read_bytes()
            if (
                destination.name in {"config", "config.toml"}
                and destination.parent.name == ".cargo"
            ):
                content = self._configuration(source, content, roots)
            elif destination.name == "Cargo.toml":
                content = self._manifest(source, content, roots)
            files[destination] = (content, resolved.stat())
        for root, relative in roots:
            if relative / "Cargo.toml" not in files:
                raise ValueError(
                    f"source workspace manifest is excluded or missing: {root}"
                )
        for path in files:
            if any(parent in files for parent in path.parents):
                raise ValueError(f"conflicting staged file and directory: {path}")
        return files

    def _stage_inventory(self, roots):
        for ancestor in (self.stage, *self.stage.parents):
            if ancestor.is_symlink():
                raise ValueError(f"stage path traverses a symlink: {ancestor}")
            if ancestor.exists() and not ancestor.is_dir():
                raise ValueError(f"stage path traverses a regular file: {ancestor}")
        for root, _ in roots:
            if self.stage.is_relative_to(root) or root.is_relative_to(self.stage):
                raise ValueError(f"stage overlaps source root: {root}")
        files, directories = [], []

        def visit(path):
            mode = path.lstat().st_mode
            if stat.S_ISDIR(mode):
                directories.append(path)
                for child in path.iterdir():
                    visit(child)
            elif stat.S_ISREG(mode):
                files.append(path)
            else:
                raise ValueError(f"unsupported staged file or symlink: {path}")

        for name in ("workspace", "sources"):
            path = self.stage / name
            if path.exists() or path.is_symlink():
                visit(path)
        return files, directories

    def prepare(self, metadata: dict) -> dict:
        roots, manifests = self._discover(metadata)
        files = self._plan(roots, manifests)
        old_files, old_directories = self._stage_inventory(roots)
        manifest = self._mapped(self.workspace / "Cargo.toml", roots)
        for source in manifests:
            mapped = self._mapped(source, roots)
            if mapped.relative_to(self.logical) not in files:
                raise ValueError(
                    f"local package manifest is excluded or missing: {mapped}"
                )
        if not any(self.logical / relative == manifest for relative in files):
            raise ValueError(
                f"requested manifest is excluded or missing: {self.workspace}"
            )

        # All source and destination validation precedes the first stage mutation.
        expected = {self.stage / path for path in files}
        for path in old_files:
            if path not in expected:
                path.unlink()
        needed = {parent for path in expected for parent in path.parents}
        for path in sorted(old_directories, key=lambda path: -len(path.parts)):
            if path not in needed:
                path.rmdir()
        for relative, (content, source_stat) in files.items():
            destination = self.stage / relative
            previous = destination.stat() if destination.exists() else None
            same_content = previous is not None and destination.read_bytes() == content
            if same_content and stat.S_IMODE(previous.st_mode) == stat.S_IMODE(
                source_stat.st_mode
            ):
                continue
            destination.parent.mkdir(parents=True, exist_ok=True)
            modified = previous.st_mtime_ns if same_content else source_stat.st_mtime_ns
            if previous and not same_content and modified <= previous.st_mtime_ns:
                # Cargo freshness must notice changed bytes even after an editor
                # or checkout restores the original source timestamp.
                modified = max(time.time_ns(), previous.st_mtime_ns + 1)
            temporary = None
            try:
                with tempfile.NamedTemporaryFile(
                    dir=destination.parent, prefix=".dyndrv-source-", delete=False
                ) as output:
                    temporary = Path(output.name)
                    output.write(content)
                    output.flush()
                    os.fchmod(output.fileno(), stat.S_IMODE(source_stat.st_mode))
                    os.utime(output.fileno(), ns=(source_stat.st_atime_ns, modified))
                temporary.replace(destination)
            finally:
                if temporary is not None:
                    temporary.unlink(missing_ok=True)
        return {
            "workspace": str(self.logical / "workspace"),
            "package_sources": [
                str(self.logical / relative) for _, relative in roots[1:]
            ],
            "roots": [
                {
                    "original": str(original),
                    "physical": str(self.stage / relative),
                    "logical": str(self.logical / relative),
                }
                for original, relative in roots
            ],
            "manifest": str(manifest),
        }
