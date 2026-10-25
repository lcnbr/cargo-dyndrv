//! Each rustc-selected incremental child is an immutable input/output pair.
//! Never recover missing local state by requesting its previous producer.
use super::{CompilerOutput, Prepared};
use color_eyre::eyre::{self, Context, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    fs::{self, File, OpenOptions},
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
};

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct State {
    pub directory: PathBuf,
    pub input: Option<PathBuf>,
    pub unit: String,
    pub observe: bool,
    child: Option<String>,
}

// Research-only observation. Keep old objects open so their inode numbers cannot
// be recycled; require a new session path and unchanged bytes after compilation.
// Production restoration does not pay this file hashing/descriptor overhead.
pub(super) struct Products {
    files: BTreeMap<(u64, u64), (File, PathBuf, String)>,
    paths: BTreeSet<PathBuf>,
}

impl Products {
    fn paths(root: &Path, files: &mut Vec<PathBuf>) -> eyre::Result<()> {
        for entry in fs::read_dir(root)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                Self::paths(&entry.path(), files)?;
            } else if kind.is_file() {
                let name = entry.file_name();
                if name.as_bytes().ends_with(b".o") || name.as_bytes().ends_with(b".pre-lto.bc") {
                    files.push(entry.path());
                }
            } else {
                bail!("incremental observer encountered an unsupported entry");
            }
        }
        Ok(())
    }

    pub(super) fn capture(root: &Path) -> eyre::Result<Self> {
        let mut paths = Vec::new();
        Self::paths(root, &mut paths)?;
        let previous_paths = paths.iter().cloned().collect();
        let mut files = BTreeMap::new();
        for path in paths {
            let file = File::open(&path)?;
            let info = file.metadata()?;
            let digest = Prepared::digest(&fs::read(&path)?);
            files.insert((info.dev(), info.ino()), (file, path, digest));
        }
        Ok(Self {
            files,
            paths: previous_paths,
        })
    }

    fn reused(&self, root: &Path) -> eyre::Result<Vec<serde_json::Value>> {
        let mut paths = Vec::new();
        Self::paths(root, &mut paths)?;
        let mut reused = Vec::new();
        for path in paths {
            let info = path.metadata()?;
            if let Some((_held_file, previous, digest)) = self.files.get(&(info.dev(), info.ino()))
                && !self.paths.contains(&path)
                && previous.parent() != path.parent()
                && *digest == Prepared::digest(&fs::read(&path)?)
            {
                reused.push(serde_json::json!({"before":previous,"after":path,"sha256":digest,"bytes":info.len()}));
            }
        }
        Ok(reused)
    }

    pub(super) fn report(self, root: &Path) -> eyre::Result<()> {
        eprintln!(
            "DYNDRV_STOCK_INCREMENTAL_REUSE {}",
            serde_json::json!({
                "directory":root,"inputProducts":self.files.len(),"reusedProducts":self.reused(root)?,
                "method":"New session paths hard-linked to held pre-compile object descriptors with identical complete bytes"
            })
        );
        Ok(())
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    version: u32,
    directory: PathBuf,
    input: PathBuf,
    unit: String,
    child: String,
}

impl Receipt {
    fn read(path: &Path, backend: &Prepared) -> eyre::Result<Self> {
        if !path.symlink_metadata()?.is_file() {
            bail!("incremental receipt must be regular");
        }
        let receipt: Self = serde_json::from_slice(&fs::read(path)?)?;
        Prepared::scratch(&receipt.directory)?;
        if receipt.version != 1
            || path.file_name() != Some(OsStr::new(&format!("{}.json", receipt.unit)))
            || !State::valid_child(&receipt.child)
            || !receipt.directory.starts_with(&backend.config.target)
            || receipt.directory.file_name() != Some(OsStr::new("incremental"))
            || crate::source::resolve_target_directory(&receipt.directory)? != receipt.directory
        {
            bail!("incremental receipt ownership mismatch");
        }
        Ok(receipt)
    }

    fn physical(&self, backend: &Prepared) -> eyre::Result<PathBuf> {
        State::validate_input(&self.input)?;
        let physical = backend.physical(&self.input)?;
        if State::produced_child(&physical)?.as_ref() != Some(&self.child) {
            bail!("incremental input child mismatch");
        }
        Ok(physical.join(&self.child))
    }
}

impl State {
    pub(super) fn from_args(
        args: &[OsString],
        target: &Path,
        output: &CompilerOutput,
    ) -> eyre::Result<Option<Self>> {
        let mut remaining = args.iter();
        let mut directory = None;
        while let Some(arg) = remaining.next() {
            let option = if arg == "-C" || arg == "--codegen" {
                remaining.next().map(|arg| arg.as_bytes())
            } else {
                arg.as_bytes()
                    .strip_prefix(b"-C")
                    .or_else(|| arg.as_bytes().strip_prefix(b"--codegen="))
            };
            if let Some(value) = option.and_then(|option| option.strip_prefix(b"incremental=")) {
                if value.is_empty() || directory.is_some() {
                    bail!("incremental output requires one nonempty directory");
                }
                directory = Some(PathBuf::from(OsString::from_vec(value.to_vec())));
            }
        }
        let Some(directory) = directory else {
            return Ok(None);
        };
        Prepared::scratch(&directory)?;
        if !directory.starts_with(target)
            || directory.file_name() != Some(OsStr::new("incremental"))
            || crate::source::resolve_target_directory(&directory)? != directory
        {
            bail!("incremental root must be a normalized directory inside target");
        }
        let CompilerOutput::Directory(artifacts) = output else {
            bail!("incremental compilation requires directory artifact output");
        };
        if directory.starts_with(artifacts) || artifacts.starts_with(&directory) {
            bail!("incremental state and compiler artifacts must be disjoint");
        }
        Ok(Some(Self {
            directory,
            input: None,
            unit: Prepared::digest(&serde_json::to_vec(
                &args.iter().map(|a| a.as_bytes()).collect::<Vec<_>>(),
            )?),
            child: None,
            observe: false,
        }))
    }

    fn receipt(&self, backend: &Prepared) -> PathBuf {
        backend
            .config
            .target
            .join(".dyndrv-delivered/incremental")
            .join(format!("{}.json", self.unit))
    }

    fn valid_child(child: &str) -> bool {
        let mut parts = Path::new(child).components();
        matches!(parts.next(), Some(std::path::Component::Normal(_))) && parts.next().is_none()
    }

    fn produced_child(root: &Path) -> eyre::Result<Option<String>> {
        source_wrap::validate_tree(root)?;
        let mut entries = fs::read_dir(root)?;
        let Some(entry) = entries.next() else {
            return Ok(None);
        };
        let entry = entry?;
        if !entry.file_type()?.is_dir() || entries.next().is_some() {
            bail!("one compiler must own at most one rustc incremental child");
        }
        Ok(Some(entry.file_name().into_string().map_err(|_| {
            eyre::eyre!("non-UTF8 rustc incremental child")
        })?))
    }

    fn validate_input(input: &Path) -> eyre::Result<()> {
        let root = Prepared::store_path(input)?
            .to_absolute_path(&harmonia_store_path::StoreDir::default());
        if input != root.join("incremental") {
            bail!("incremental input must belong to a compiler result");
        }
        Ok(())
    }

    // mtime and writable bits are intentionally transport properties. File bytes,
    // types, executable bits and the complete directory inventory remain bound.
    fn verify(input: &Path, local: &Path) -> eyre::Result<()> {
        let expected = fs::symlink_metadata(input)?;
        let actual = fs::symlink_metadata(local)?;
        if expected.file_type() != actual.file_type()
            || expected.permissions().mode() & 0o111 != actual.permissions().mode() & 0o111
        {
            bail!("incremental state type or executable mode changed");
        }
        if expected.is_file() {
            if !source_wrap::same_contents(input, local)? {
                bail!("incremental state bytes changed");
            }
        } else if expected.is_dir() {
            let names = |path: &Path| -> std::io::Result<BTreeSet<OsString>> {
                fs::read_dir(path)?
                    .map(|entry| entry.map(|entry| entry.file_name()))
                    .collect()
            };
            let entries = names(input)?;
            if entries != names(local)? {
                bail!("incremental state entries changed");
            }
            for entry in entries {
                Self::verify(&input.join(&entry), &local.join(entry))?;
            }
        } else {
            bail!("incremental state contains a symlink or special file");
        }
        Ok(())
    }

    /// Hold this lock through compiler completion and host delivery. Cargo normally
    /// serializes the unit; reject accidental concurrent owners instead of waiting.
    pub(super) fn bind(&mut self, backend: &Prepared) -> eyre::Result<File> {
        let receipt = self.receipt(backend);
        let parent = receipt.parent().unwrap();
        fs::create_dir_all(parent)?;
        if parent.canonicalize()? != parent {
            bail!("incremental receipt path traverses symlinks");
        }
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(receipt.with_extension("lock"))?;
        lock.try_lock()
            .wrap_err("incremental directory already has a live owner")?;
        if !lock.metadata()?.is_file() {
            bail!("incremental lock must be regular");
        }
        self.input = None;
        self.child = None;
        // Cargo passes a shared root. Only inspect this invocation's recorded
        // rustc-selected child; sibling units may be compiling concurrently.
        if self.directory.try_exists()? && receipt.try_exists()? {
            let prior = Receipt::read(&receipt, backend)?;
            if prior.directory != self.directory || prior.unit != self.unit {
                bail!("incremental receipt ownership mismatch");
            }
            let local = self.directory.join(&prior.child);
            // A missing child is an explicit cold/stripped state, even if its
            // previous immutable owner is unavailable. Never request producer A.
            if local.symlink_metadata().is_ok() {
                Self::verify(&prior.physical(backend)?, &local)?;
                self.input = Some(prior.input);
                self.child = Some(prior.child);
            }
        }
        Ok(lock)
    }

    pub(super) fn snapshot_references(
        backend: &Prepared,
    ) -> eyre::Result<(BTreeSet<PathBuf>, usize, Vec<File>)> {
        let directory = backend.config.target.join(".dyndrv-delivered/incremental");
        if !directory.try_exists()? {
            return Ok((BTreeSet::new(), 0, Vec::new()));
        }
        if directory.canonicalize()? != directory {
            bail!("incremental ownership directory traverses symlinks");
        }
        let mut references = BTreeSet::new();
        let mut children = BTreeSet::new();
        let mut leases = Vec::new();
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let path = entry.path();
            if !entry.file_type()?.is_file() {
                bail!("incremental snapshot metadata must be regular files");
            }
            if path.extension() == Some(OsStr::new("lock")) {
                if entry.metadata()?.len() != 0 {
                    bail!("incremental coordination lock is not empty");
                }
                let lease = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .custom_flags(libc::O_NOFOLLOW)
                    .open(path)?;
                lease
                    .try_lock()
                    .wrap_err("incremental snapshot has a live compiler owner")?;
                leases.push(lease);
            } else if path.extension() != Some(OsStr::new("json")) {
                bail!("incremental snapshot contains unfinished or unknown metadata");
            }
        }
        for entry in fs::read_dir(&directory)? {
            let path = entry?.path();
            if path.extension() != Some(OsStr::new("json")) {
                continue;
            }
            if !path.with_extension("lock").symlink_metadata()?.is_file() {
                bail!("incremental receipt lacks its coordination lock");
            }
            let prior = Receipt::read(&path, backend)?;
            let physical = prior.physical(backend)?;
            let local = prior.directory.join(&prior.child);
            if local.symlink_metadata().is_ok() {
                Self::verify(&physical, &local)?;
                children.insert(local);
            }
            // Retained receipts themselves name their previous immutable input,
            // including after a deliberate strip. Export must retain that input
            // or fail; this operation never requests a producer to recover it.
            references.insert(
                Prepared::store_path(&prior.input)?
                    .to_absolute_path(&harmonia_store_path::StoreDir::default()),
            );
        }
        Ok((references, children.len(), leases))
    }

    pub(super) fn restore(&self) -> eyre::Result<()> {
        fs::create_dir_all(self.directory.parent().unwrap())?;
        if let Some(input) = &self.input {
            Self::validate_input(input)?;
            if Self::produced_child(input)? != self.child {
                bail!("incremental restore child mismatch");
            }
            source_wrap::Materializer::Copy.materialize(
                input,
                &self.directory,
                &[source_wrap::Writable::Directory(&self.directory)],
            )?;
        } else {
            fs::create_dir(&self.directory)?;
        }
        Ok(())
    }

    pub(super) fn publish(&self, output: &Path) -> eyre::Result<()> {
        let child = Self::produced_child(&self.directory)?;
        if self.child.is_some() && child != self.child {
            bail!("compiler changed its incremental child identity");
        }
        compile_wrap::publish(&self.directory, &output.join("incremental"))?;
        Ok(())
    }

    pub(super) fn deliver(&self, backend: &Prepared, output: &Path) -> eyre::Result<()> {
        let input = output.join("incremental");
        let physical = backend.physical(&input)?;
        let Some(child) = Self::produced_child(&physical)? else {
            return Ok(());
        };
        let destination = self.directory.join(&child);
        // Recheck only this unit before replacement, without locking siblings.
        if let Some(previous) = &self.input {
            if self.child.as_ref() != Some(&child) {
                bail!("incremental child ownership changed");
            }
            Self::verify(&backend.physical(previous)?.join(&child), &destination)?;
        } else if destination.symlink_metadata().is_ok() {
            bail!("refusing to replace an unowned incremental child");
        }
        let parent = &self.directory;
        fs::create_dir_all(parent)?;
        if parent.canonicalize()? != *parent {
            bail!("incremental destination traverses symlinks");
        }
        let stem = format!(".dyndrv-{}-{}", self.unit, std::process::id());
        let staged = parent.join(format!("{stem}.new"));
        let backup = parent.join(format!("{stem}.old"));
        if staged.symlink_metadata().is_ok() || backup.symlink_metadata().is_ok() {
            bail!("incremental staging is already occupied");
        }
        source_wrap::Materializer::Copy.materialize(
            &physical.join(&child),
            &staged,
            &[source_wrap::Writable::Directory(&staged)],
        )?;
        let existed = destination.try_exists()?;
        if existed {
            fs::rename(&destination, &backup)?;
        }
        if let Err(error) = fs::rename(&staged, &destination) {
            if existed {
                fs::rename(&backup, &destination)?;
            }
            return Err(error.into());
        }
        if existed {
            fs::remove_dir_all(backup)?;
        }
        let receipt = self.receipt(backend);
        let temporary = receipt.with_extension(format!("{}.tmp", std::process::id()));
        Prepared::write_json(
            &temporary,
            &Receipt {
                version: 1,
                directory: self.directory.clone(),
                input,
                child,
                unit: self.unit.clone(),
            },
        )?;
        fs::rename(temporary, receipt)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::Config;
    use super::*;
    use std::{collections::BTreeMap, time::SystemTime};

    fn root() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "dyndrv-incremental-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&path).unwrap();
        path
    }

    #[test]
    fn product_observer_requires_a_new_hard_link_and_unchanged_bytes() {
        let root = root();
        let old = root.join("old");
        let new = root.join("new");
        fs::create_dir(&old).unwrap();
        fs::create_dir(&new).unwrap();
        fs::write(old.join("reused.o"), "cached object").unwrap();
        let earlier = root.join("earlier");
        fs::create_dir(&earlier).unwrap();
        fs::hard_link(old.join("reused.o"), earlier.join("already-reused.o")).unwrap();
        let products = Products::capture(&root).unwrap();
        assert!(products.reused(&root).unwrap().is_empty());
        fs::copy(old.join("reused.o"), new.join("copied.o")).unwrap();
        assert!(products.reused(&root).unwrap().is_empty());
        fs::hard_link(old.join("reused.o"), new.join("reused.o")).unwrap();
        fs::remove_file(old.join("reused.o")).unwrap();
        assert_eq!(products.reused(&root).unwrap().len(), 1);
        fs::write(new.join("reused.o"), "changed object").unwrap();
        assert!(products.reused(&root).unwrap().is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn compiler_options_bind_one_disjoint_normalized_cargo_unit() {
        let root = root();
        let target = root.join("target");
        fs::create_dir(&target).unwrap();
        let directory = target.join("ci/incremental");
        let output = CompilerOutput::Directory(target.join("ci/deps"));
        let value = format!("incremental={}", directory.display());
        for args in [
            vec!["-C".into(), value.clone().into()],
            vec![format!("-C{value}").into()],
            vec!["--codegen".into(), value.clone().into()],
            vec![format!("--codegen={value}").into()],
        ] {
            assert_eq!(
                State::from_args(&args, &target, &output)
                    .unwrap()
                    .unwrap()
                    .directory,
                directory
            );
        }
        assert!(
            State::from_args(&["-Ccodegen-units=16".into()], &target, &output)
                .unwrap()
                .is_none()
        );
        for value in [
            String::new(),
            "relative".into(),
            target.to_string_lossy().into_owned(),
            root.join("outside/incremental")
                .to_string_lossy()
                .into_owned(),
            target
                .join("ci/incremental/../unit")
                .to_string_lossy()
                .into_owned(),
        ] {
            assert!(
                State::from_args(&[format!("-Cincremental={value}").into()], &target, &output)
                    .is_err()
            );
        }
        let args = vec![format!("-C{value}").into()];
        assert!(
            State::from_args(
                &args,
                &target,
                &CompilerOutput::Directory(directory.clone())
            )
            .is_err()
        );
        assert!(State::from_args(&args, &target, &CompilerOutput::Stdout).is_err());
        assert!(State::from_args(&[args[0].clone(), args[0].clone()], &target, &output).is_err());
        fs::create_dir_all(target.join("ci")).unwrap();
        std::os::unix::fs::symlink(&root, &directory).unwrap();
        assert!(State::from_args(&args, &target, &output).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn restored_state_requires_immutable_ownership_but_absent_state_never_recovers_a() {
        let root = root();
        let target = root.join("target");
        fs::create_dir(&target).unwrap();
        let backend = Prepared {
            config: Config {
                nix: PathBuf::new(),
                socket: PathBuf::new(),
                real_store: root.join("store"),
                store_mode: Default::default(),
                gc_roots: None,
                source: root.join("source"),
                package_sources: Vec::new(),
                package_store_inputs: BTreeMap::new(),
                layout_environment_names: BTreeSet::new(),
                target: target.clone(),
                incremental: true,
                incremental_observer: false,
                source_overlay: false,
                private_target: false,
                jobserver_socket_directory: None,
                inputs: Vec::new(),
                environment_names: BTreeSet::new(),
                environment_prefixes: Vec::new(),
                omitted_environment_names: BTreeSet::new(),
            },
            trees: BTreeMap::new(),
            builder_store: PathBuf::new(),
        };
        let directory = target.join("ci/incremental");
        let output = Path::new("/nix/store/00000000000000000000000000000000-result");
        let physical = backend.physical(output).unwrap();
        fs::create_dir_all(physical.join("incremental/rustc-selected-child/session")).unwrap();
        fs::write(
            physical.join("incremental/rustc-selected-child/session/object.o"),
            "owned object",
        )
        .unwrap();
        let make_state = || State {
            directory: directory.clone(),
            input: None,
            unit: "unit-hash".to_string(),
            child: None,
            observe: false,
        };
        let mut cold = make_state();
        let lease = cold.bind(&backend).unwrap();
        assert!(cold.input.is_none());
        assert!(make_state().bind(&backend).is_err());
        cold.deliver(&backend, output).unwrap();
        assert!(State::snapshot_references(&backend).is_err());
        drop(lease);
        let (references, count, leases) = State::snapshot_references(&backend).unwrap();
        assert_eq!(references, BTreeSet::from([output.to_path_buf()]));
        assert_eq!(count, 1);
        assert!(make_state().bind(&backend).is_err());
        drop(leases);
        let mut warm = make_state();
        let lease = warm.bind(&backend).unwrap();
        assert_eq!(
            warm.input.as_deref(),
            Some(output.join("incremental").as_path())
        );
        drop(lease);
        let object = directory.join("rustc-selected-child/session/object.o");
        fs::write(&object, "tampered").unwrap();
        assert!(make_state().bind(&backend).is_err());
        assert!(State::snapshot_references(&backend).is_err());
        fs::write(&object, "owned object").unwrap();
        fs::write(directory.join("rustc-selected-child/unexpected"), "unbound").unwrap();
        assert!(make_state().bind(&backend).is_err());
        fs::remove_file(directory.join("rustc-selected-child/unexpected")).unwrap();
        fs::remove_file(&object).unwrap();
        std::os::unix::fs::symlink(
            physical.join("incremental/rustc-selected-child/session/object.o"),
            &object,
        )
        .unwrap();
        assert!(make_state().bind(&backend).is_err());
        fs::remove_file(&object).unwrap();
        fs::write(&object, "owned object").unwrap();
        fs::rename(&physical, root.join("missing-backing-output")).unwrap();
        assert!(make_state().bind(&backend).is_err());
        assert!(State::snapshot_references(&backend).is_err());
        // The same missing backing output must never matter after stripping.
        fs::remove_dir_all(&directory).unwrap();
        let mut stripped = make_state();
        let lease = stripped.bind(&backend).unwrap();
        assert!(stripped.input.is_none());
        drop(lease);
        fs::create_dir(&directory).unwrap();
        let lease = stripped.bind(&backend).unwrap();
        assert!(stripped.input.is_none());
        drop(lease);
        fs::rename(root.join("missing-backing-output"), &physical).unwrap();
        let (references, count, leases) = State::snapshot_references(&backend).unwrap();
        assert_eq!(references, BTreeSet::from([output.to_path_buf()]));
        assert_eq!(count, 0);
        drop(leases);
        fs::remove_file(stripped.receipt(&backend)).unwrap();
        fs::create_dir(directory.join("rustc-selected-child")).unwrap();
        let mut unowned = make_state();
        let lease = unowned.bind(&backend).unwrap();
        assert!(unowned.input.is_none());
        assert!(unowned.deliver(&backend, output).is_err());
        drop(lease);
        fs::remove_dir_all(root).unwrap();
    }
}
