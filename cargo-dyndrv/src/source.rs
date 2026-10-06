//! Workspace sources retain their layout while mutable Cargo state is snapshotted separately.
use cargo_metadata::camino::Utf8Path;
use color_eyre::eyre::{self, Context};
use nix_nar::{FileSystem, FileSystemMetadata, NativeFileSystem};
use std::{
    collections::BTreeSet,
    fs, io,
    path::{Component, Path, PathBuf},
};

pub const LOGICAL_ROOT: &str = "/build/source";

pub struct WorkspaceSource {
    pub root: PathBuf,
    compiler_state: BTreeSet<PathBuf>,
}

pub struct PackageSource {
    pub manifest: PathBuf,
    pub directory: PathBuf,
    pub compiler_input: PathBuf,
}

/// A temporary checkout whose top-level sources link to the declared workspace.
/// Validate the live links, but encode the already captured immutable source.
pub(crate) struct WorkspaceAlias {
    root: PathBuf,
    immutable: PathBuf,
    markers: BTreeSet<String>,
    local: BTreeSet<String>,
}

impl WorkspaceAlias {
    pub(crate) fn new(
        root: &Path,
        source: &Path,
        target: &Path,
        immutable: &Path,
    ) -> eyre::Result<Self> {
        if root.canonicalize()? != root
            || source.starts_with(root)
            || root == target
            || (root.starts_with(source) && !root.starts_with(target))
        {
            eyre::bail!("workspace mirror must be normalized and outside captured source state");
        }
        let mut expected = NativeFileSystem {}
            .read_dir(
                Utf8Path::from_path(immutable)
                    .ok_or_else(|| eyre::eyre!("non-UTF8 immutable source"))?,
            )?
            .into_iter()
            .collect::<BTreeSet<_>>();
        expected.remove("target");
        let mut markers = BTreeSet::new();
        let mut local = BTreeSet::new();
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| eyre::eyre!("non-UTF8 workspace mirror entry"))?;
            let kind = entry.file_type()?;
            if name == "target" && kind.is_dir() {
                // This checkout's runtime output is not a compiler source.
                continue;
            }
            if matches!(name.as_str(), ".git" | ".jj")
                && kind.is_dir()
                && fs::read_dir(entry.path())?.next().is_none()
            {
                markers.insert(name);
                continue;
            }
            if expected.remove(&name) {
                if kind.is_symlink() && fs::read_link(entry.path())? == source.join(&name) {
                    continue;
                }
                eyre::bail!("workspace mirror contains an unbound source entry: {name}");
            }
            // Nested tools may create auxiliary inputs beside the source links.
            // Bind their actual bytes; never silently omit runtime-created files.
            if matches!(name.as_str(), ".git" | ".jj" | "target")
                || (!kind.is_file() && !kind.is_dir())
            {
                eyre::bail!("workspace mirror contains unsupported local state: {name}");
            }
            if kind.is_dir() {
                source_wrap::validate_tree(&entry.path())?;
            }
            local.insert(name);
        }
        if !expected.is_empty() {
            eyre::bail!("workspace mirror omits declared source entries");
        }
        Ok(Self {
            root: root.to_path_buf(),
            immutable: immutable.to_path_buf(),
            markers,
            local,
        })
    }

    fn input(&self, path: &Utf8Path) -> io::Result<PathBuf> {
        let relative = path
            .as_std_path()
            .strip_prefix(&self.root)
            .map_err(|_| io::Error::other("path escapes workspace mirror"))?;
        if relative
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
        {
            return Err(io::Error::other("workspace mirror path is not normalized"));
        }
        let local = relative
            .iter()
            .next()
            .is_some_and(|name| name.to_str().is_some_and(|name| self.local.contains(name)));
        Ok(if local { &self.root } else { &self.immutable }.join(relative))
    }

    fn marker(&self, path: &Utf8Path) -> bool {
        path.parent().map(Utf8Path::as_std_path) == Some(self.root.as_path())
            && path
                .file_name()
                .is_some_and(|name| self.markers.contains(name))
    }

    pub(crate) fn nar(&self) -> eyre::Result<Vec<u8>> {
        let mut encoder = nix_nar::Encoder::new_with_filesystem(&self.root, self)?;
        let mut bytes = Vec::new();
        io::copy(&mut encoder, &mut bytes)?;
        Ok(bytes)
    }
}

impl FileSystem for &WorkspaceAlias {
    type File = fs::File;

    fn metadata(&self, path: &Utf8Path) -> io::Result<FileSystemMetadata> {
        if self.marker(path) {
            return Ok(FileSystemMetadata::Directory);
        }
        let input = self.input(path)?;
        match (NativeFileSystem {}).metadata(Utf8Path::from_path(&input).unwrap())? {
            info @ (FileSystemMetadata::Directory | FileSystemMetadata::File { .. }) => Ok(info),
            _ => Err(io::Error::other(
                "workspace mirror contains a symlink or special file",
            )),
        }
    }

    fn read_dir(&self, path: &Utf8Path) -> io::Result<Vec<String>> {
        if self.marker(path) {
            return Ok(Vec::new());
        }
        let input = self.input(path)?;
        let mut names = NativeFileSystem {}.read_dir(Utf8Path::from_path(&input).unwrap())?;
        if path.as_std_path() == self.root {
            names.extend(self.markers.iter().cloned());
            names.extend(self.local.iter().cloned());
            names.sort();
        }
        Ok(names)
    }

    fn open(&self, path: &Utf8Path) -> io::Result<Self::File> {
        fs::File::open(self.input(path)?)
    }
}

impl WorkspaceSource {
    pub fn new(explicit_root: &Path, cargo_root: &Path, target_dir: &Path) -> eyre::Result<Self> {
        let root = explicit_root
            .canonicalize()
            .wrap_err("workspace source root")?;
        if root != cargo_root.canonicalize()? {
            eyre::bail!("explicit workspace source must match Cargo workspace_root");
        }
        if !root.is_dir()
            || resolve_target_directory(target_dir)? != target_dir
            || root.starts_with(target_dir)
        {
            eyre::bail!(
                "target must be normalized, must not traverse symlinks, and cannot contain the source"
            );
        }
        for name in [".git", ".jj"] {
            if root.join(name).symlink_metadata().is_ok() {
                eyre::bail!("workspace source contains VCS state: {name}");
            }
        }
        // Nextest also writes root/target reports when Cargo's target is external.
        // Empty directory placeholders keep the source identity stable before and
        // after either owner creates state, without copying artifacts or reports.
        let mut compiler_state = BTreeSet::from([root.join("target")]);
        if target_dir.starts_with(&root) {
            compiler_state.insert(target_dir.to_path_buf());
        }
        let source = Self {
            root,
            compiler_state,
        };
        source.validate(
            Utf8Path::from_path(&source.root).ok_or_else(|| eyre::eyre!("non-UTF8 source root"))?,
        )?;
        Ok(source)
    }

    fn validate(&self, path: &Utf8Path) -> io::Result<()> {
        if matches!(
            FileSystem::metadata(&self, path)?,
            FileSystemMetadata::Directory
        ) {
            for name in FileSystem::read_dir(&self, path)? {
                self.validate(&path.join(name))?;
            }
        }
        Ok(())
    }

    pub fn nar(&self) -> eyre::Result<Vec<u8>> {
        let mut encoder = nix_nar::Encoder::new_with_filesystem(&self.root, self)?;
        let mut bytes = Vec::new();
        io::copy(&mut encoder, &mut bytes)?;
        Ok(bytes)
    }

    pub fn package(&self, manifest: &Path, compiler_input: &Path) -> eyre::Result<PackageSource> {
        let relative = |path: &Path| -> eyre::Result<PathBuf> {
            let canonical = path.canonicalize()?;
            if self
                .compiler_state
                .iter()
                .any(|state| canonical.starts_with(state))
            {
                eyre::bail!("package source belongs to excluded compiler state");
            }
            Ok(canonical
                .strip_prefix(&self.root)
                .wrap_err("local package or target escapes workspace source")?
                .to_path_buf())
        };
        let manifest = relative(manifest)?;
        let directory = manifest
            .parent()
            .ok_or_else(|| eyre::eyre!("manifest has no parent"))?
            .to_path_buf();
        Ok(PackageSource {
            manifest,
            directory,
            compiler_input: relative(compiler_input)?,
        })
    }
}

// Use the NAR encoder's filesystem view, so filtering requires no intermediate
// source copy and never walks or reads the mutable target payload.
impl FileSystem for &WorkspaceSource {
    type File = fs::File;

    fn metadata(&self, path: &Utf8Path) -> io::Result<FileSystemMetadata> {
        let placeholder = self
            .compiler_state
            .iter()
            .any(|state| state.starts_with(path));
        match (NativeFileSystem {}).metadata(path) {
            Ok(FileSystemMetadata::Directory) => Ok(FileSystemMetadata::Directory),
            Ok(info @ FileSystemMetadata::File { .. }) if !placeholder => Ok(info),
            Err(error) if error.kind() == io::ErrorKind::NotFound && placeholder => {
                Ok(FileSystemMetadata::Directory)
            }
            Err(error) => Err(error),
            _ => Err(io::Error::other(
                "workspace source contains a symlink, special file or non-directory target ancestor",
            )),
        }
    }

    fn read_dir(&self, path: &Utf8Path) -> io::Result<Vec<String>> {
        let mut names = if self
            .compiler_state
            .iter()
            .any(|state| path.starts_with(state))
            || !path.exists()
        {
            BTreeSet::new()
        } else {
            NativeFileSystem {}.read_dir(path)?.into_iter().collect()
        };
        for state in &self.compiler_state {
            if let Ok(relative) = state.strip_prefix(path)
                && let Some(Component::Normal(name)) = relative.components().next()
            {
                names.insert(
                    name.to_str()
                        .ok_or_else(|| io::Error::other("non-UTF8 target component"))?
                        .to_owned(),
                );
            }
        }
        Ok(names.into_iter().collect())
    }

    fn open(&self, path: &Utf8Path) -> io::Result<Self::File> {
        NativeFileSystem {}.open(path)
    }
}

// Canonicalize existing components, including symlink parents, while allowing a
// not-yet-created target directory. Resolve .. after its preceding component.
pub(crate) fn resolve_target_directory(path: &Path) -> eyre::Result<PathBuf> {
    if !path.is_absolute() {
        eyre::bail!("Cargo target directory must be absolute");
    }
    let mut resolved = PathBuf::new();
    for part in path.components() {
        match part {
            Component::RootDir => resolved.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            Component::Normal(name) => {
                resolved.push(name);
                match resolved.symlink_metadata() {
                    Ok(_) => resolved = resolved.canonicalize()?,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.into()),
                }
            }
            Component::Prefix(_) => eyre::bail!("unsupported target path prefix"),
        }
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, time::SystemTime};
    #[test]
    fn workspace_mirrors_bind_only_declared_sources_and_empty_vcs_markers() {
        let root = std::env::temp_dir().join(format!(
            "dyndrv-mirror-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let source = root.join("source");
        let target = root.join("target");
        let immutable = root.join("immutable");
        let mirror = root.join("mirror");
        fs::create_dir_all(source.join("src")).unwrap();
        fs::write(source.join("Cargo.toml"), "original manifest").unwrap();
        fs::write(source.join("src/lib.rs"), "original source").unwrap();
        let nar = WorkspaceSource::new(&source, &source, &target)
            .unwrap()
            .nar()
            .unwrap();
        nix_nar::Decoder::new(nar.as_slice())
            .unwrap()
            .unpack(&immutable)
            .unwrap();
        fs::create_dir(&mirror).unwrap();
        for name in ["Cargo.toml", "src"] {
            std::os::unix::fs::symlink(source.join(name), mirror.join(name)).unwrap();
        }
        let make_alias = || WorkspaceAlias::new(&mirror, &source, &target, &immutable);
        assert_eq!(make_alias().unwrap().nar().unwrap(), nar);
        // Live source changes cannot become undeclared bytes in the alias input.
        fs::write(source.join("src/lib.rs"), "later source").unwrap();
        fs::create_dir(mirror.join("target")).unwrap();
        fs::write(mirror.join("target/runtime"), "excluded runtime output").unwrap();
        assert_eq!(make_alias().unwrap().nar().unwrap(), nar);
        for marker in [".git", ".jj"] {
            fs::create_dir(mirror.join(marker)).unwrap();
        }
        let alias = make_alias().unwrap();
        let decoded = root.join("decoded");
        nix_nar::Decoder::new(alias.nar().unwrap().as_slice())
            .unwrap()
            .unpack(&decoded)
            .unwrap();
        assert_eq!(
            fs::read(decoded.join("src/lib.rs")).unwrap(),
            b"original source"
        );
        for empty in ["target", ".git", ".jj"] {
            assert_eq!(fs::read_dir(decoded.join(empty)).unwrap().count(), 0);
        }
        assert!(
            alias
                .input(Utf8Path::from_path(&mirror.join("../source")).unwrap())
                .is_err()
        );
        assert!(alias.input(Utf8Path::from_path(&source).unwrap()).is_err());
        fs::write(mirror.join(".jj/private"), "VCS state").unwrap();
        assert!(make_alias().is_err());
        fs::remove_file(mirror.join(".jj/private")).unwrap();
        let before = make_alias().unwrap().nar().unwrap();
        fs::write(mirror.join("extra"), "auxiliary compiler input").unwrap();
        let with_extra = make_alias().unwrap().nar().unwrap();
        assert_ne!(with_extra, before);
        let extra_copy = root.join("extra-copy");
        nix_nar::Decoder::new(with_extra.as_slice())
            .unwrap()
            .unpack(&extra_copy)
            .unwrap();
        assert_eq!(
            fs::read(extra_copy.join("extra")).unwrap(),
            b"auxiliary compiler input"
        );
        fs::remove_file(mirror.join("extra")).unwrap();
        std::os::unix::fs::symlink(&source, mirror.join("extra")).unwrap();
        assert!(make_alias().is_err());
        fs::remove_file(mirror.join("extra")).unwrap();
        fs::create_dir(mirror.join("extra")).unwrap();
        fs::write(mirror.join("extra/input"), "nested input").unwrap();
        assert!(make_alias().unwrap().nar().is_ok());
        std::os::unix::fs::symlink(&source, mirror.join("extra/link")).unwrap();
        assert!(make_alias().is_err());
        fs::remove_dir_all(mirror.join("extra")).unwrap();
        fs::remove_file(mirror.join("src")).unwrap();
        assert!(make_alias().is_err());
        std::os::unix::fs::symlink(immutable.join("src"), mirror.join("src")).unwrap();
        assert!(make_alias().is_err());
        fs::remove_file(mirror.join("src")).unwrap();
        fs::create_dir(mirror.join("src")).unwrap();
        assert!(make_alias().is_err());
        fs::remove_dir(mirror.join("src")).unwrap();
        std::os::unix::fs::symlink(source.join("src"), mirror.join("src")).unwrap();
        assert!(make_alias().is_ok());
        fs::remove_dir_all(mirror.join("target")).unwrap();
        std::os::unix::fs::symlink(&source, mirror.join("target")).unwrap();
        assert!(make_alias().is_err());
        std::os::unix::fs::symlink(&mirror, root.join("mirror-link")).unwrap();
        assert!(
            WorkspaceAlias::new(&root.join("mirror-link"), &source, &target, &immutable).is_err()
        );
        assert!(WorkspaceAlias::new(&source, &source, &target, &immutable).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn workspace_mirrors_inside_target_bind_the_immutable_source() {
        let temporary = std::env::temp_dir().join(format!(
            "dyndrv-target-mirror-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let source = temporary.join("source");
        fs::create_dir_all(source.join("src")).unwrap();
        fs::write(source.join("Cargo.toml"), "original manifest").unwrap();
        fs::write(source.join("src/lib.rs"), "original source").unwrap();
        for name in ["target", "build-state"] {
            let target = source.join(name);
            let immutable = temporary.join(format!("immutable-{name}"));
            let nar = WorkspaceSource::new(&source, &source, &target)
                .unwrap()
                .nar()
                .unwrap();
            nix_nar::Decoder::new(nar.as_slice())
                .unwrap()
                .unpack(&immutable)
                .unwrap();
            let mirror = target.join("temporary/checkout");
            fs::create_dir_all(&mirror).unwrap();
            for entry in fs::read_dir(&source).unwrap() {
                let entry = entry.unwrap();
                if entry.file_name() != "target" {
                    std::os::unix::fs::symlink(entry.path(), mirror.join(entry.file_name()))
                        .unwrap();
                }
            }
            let alias = WorkspaceAlias::new(&mirror, &source, &target, &immutable).unwrap();
            assert_eq!(alias.nar().unwrap(), nar);
            assert!(
                WorkspaceAlias::new(&mirror, &source, &source.join("other"), &immutable).is_err()
            );
            assert!(WorkspaceAlias::new(&target, &source, &target, &immutable).is_err());
            fs::remove_file(mirror.join("src")).unwrap();
            std::os::unix::fs::symlink(&immutable, mirror.join("src")).unwrap();
            assert!(WorkspaceAlias::new(&mirror, &source, &target, &immutable).is_err());
            fs::remove_dir_all(target).unwrap();
        }
        fs::remove_dir_all(temporary).unwrap();
    }

    #[test]
    fn snapshots_exclude_target_payload_but_preserve_layout_and_source_identity() {
        let root = std::env::temp_dir().join(format!(
            "dyndrv-snapshot-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let source = root.join("source");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("Cargo.toml"), "source manifest").unwrap();
        let target = source.join("build/cargo-target");
        let clean = WorkspaceSource::new(&source, &source, &target)
            .unwrap()
            .nar()
            .unwrap();
        fs::create_dir_all(&target).unwrap();
        fs::create_dir(source.join("target")).unwrap();
        fs::write(source.join("target/report"), "runtime report").unwrap();
        fs::write(
            target.join("private-state"),
            "compiler state must not enter source",
        )
        .unwrap();
        std::os::unix::fs::symlink("/unavailable", target.join("ignored-link")).unwrap();
        let layout = WorkspaceSource::new(&source, &source, &target).unwrap();
        assert_eq!(layout.nar().unwrap(), clean);
        let copy = root.join("decoded");
        nix_nar::Decoder::new(clean.as_slice())
            .unwrap()
            .unpack(&copy)
            .unwrap();
        assert!(copy.join("target").is_dir());
        assert!(copy.join("build/cargo-target").is_dir());
        assert_eq!(fs::read_dir(copy.join("target")).unwrap().count(), 0);
        assert_eq!(
            fs::read_dir(copy.join("build/cargo-target"))
                .unwrap()
                .count(),
            0
        );
        assert_eq!(
            fs::read(copy.join("Cargo.toml")).unwrap(),
            b"source manifest"
        );
        fs::write(source.join("Cargo.toml"), "changed source").unwrap();
        assert_ne!(layout.nar().unwrap(), clean);
        assert!(
            layout
                .package(&target.join("private-state"), &source.join("Cargo.toml"))
                .is_err()
        );
        std::os::unix::fs::symlink("Cargo.toml", source.join("source-link")).unwrap();
        assert!(WorkspaceSource::new(&source, &source, &target).is_err());
        fs::remove_file(source.join("source-link")).unwrap();
        fs::remove_dir_all(source.join("target")).unwrap();
        std::os::unix::fs::symlink(&target, source.join("target")).unwrap();
        assert!(WorkspaceSource::new(&source, &source, &target).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn mapping_uses_workspace_relative_paths_and_rejects_escapes_and_state() {
        let root = std::env::temp_dir().join(format!(
            "dyndrv-layout-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let source = root.join("source");
        fs::create_dir_all(source.join("crates/a")).unwrap();
        fs::write(source.join("crates/a/Cargo.toml"), "").unwrap();
        fs::write(source.join("shared.rs"), "").unwrap();
        fs::write(root.join("outside.rs"), "").unwrap();
        let layout = WorkspaceSource::new(&source, &source, &root.join("target")).unwrap();
        let package = layout
            .package(
                &source.join("crates/a/Cargo.toml"),
                &source.join("shared.rs"),
            )
            .unwrap();
        assert_eq!(package.directory, Path::new("crates/a"));
        assert_eq!(package.compiler_input, Path::new("shared.rs"));
        assert!(
            layout
                .package(
                    &source.join("crates/a/Cargo.toml"),
                    &root.join("outside.rs")
                )
                .is_err()
        );
        assert!(WorkspaceSource::new(&root, &source, &root.join("target")).is_err());
        assert!(WorkspaceSource::new(&source, &source, &source.join("target")).is_ok());
        assert!(WorkspaceSource::new(&source, &source, &source).is_err());
        std::os::unix::fs::symlink(&source, root.join("source-alias")).unwrap();
        assert!(
            WorkspaceSource::new(&source, &source, &root.join("source-alias/missing/target"))
                .is_err()
        );
        fs::create_dir(root.join("outside")).unwrap();
        assert!(
            WorkspaceSource::new(
                &source,
                &source,
                &root.join("outside/../source/missing/target")
            )
            .is_err()
        );
        assert!(
            WorkspaceSource::new(
                &source,
                &source,
                &root.join("source-alias/missing/../target")
            )
            .is_err()
        );
        assert!(
            WorkspaceSource::new(&source, &source, &root.join("outside/missing/target")).is_ok()
        );
        fs::create_dir(source.join(".jj")).unwrap();
        assert!(WorkspaceSource::new(&source, &source, &root.join("target")).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
