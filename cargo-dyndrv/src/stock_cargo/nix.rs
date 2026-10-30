//! Execute Cargo's actual compiler command and deliver owned metadata before
//! forwarding its readiness notification. Shared incremental state is separate.
use crate::{
    bound_graph,
    source::{WorkspaceAlias, WorkspaceSource},
    store,
    tools::Tool,
    util::IntoBytes,
};
mod incremental;
mod jobserver;
mod metadata;
use color_eyre::eyre::{self, Context, bail};
use harmonia_store_content_address::ContentAddressMethodAlgorithm;
use harmonia_store_derivation::{
    derivation::{Derivation, DerivationOutput},
    derived_path::SingleDerivedPath,
    placeholder::Placeholder,
};
use harmonia_store_path::{StoreDir, StorePath, StorePathName};
use harmonia_store_remote::{DaemonStore, HandshakeDaemonStore};
use harmonia_utils_hash::{Algorithm::SHA256, HashFormat};
use libc::{
    AT_EACCESS, AT_FDCWD, EACCES, EROFS, O_CLOEXEC, O_NOFOLLOW, O_NONBLOCK, ST_RDONLY, W_OK,
};
use serde::{Deserialize, Serialize};
use source_wrap::{Verifier, same_contents};
use std::borrow::Cow;
use std::ffi::CString;
use std::fs::{File, Metadata, Permissions};
use std::io;
use std::io::Error as IoError;
use std::iter;
use std::mem::MaybeUninit;
use std::os::unix::fs as unix_fs;
use std::process;
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::{OsStr, OsString},
    fs::{self, OpenOptions},
    io::{BufRead, BufReader, ErrorKind, Read, Write},
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
        process::ExitStatusExt,
    },
    path::{Component, Path, PathBuf},
    process::{Command, ExitStatus, Stdio},
    str::FromStr,
    time::Instant,
};

#[derive(Clone, Copy, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
enum StoreMode {
    #[default]
    Private,
    System,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum ArtifactDelivery {
    #[default]
    Copy,
    SystemStoreRlibLinks,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum Representation {
    Copy,
    StoreLink,
}

impl Representation {
    // The caller separately authenticates ownership and immutable store backing.
    // This check never treats an equal-content replacement as the recorded link.
    fn validate_contents(
        self,
        target: &Path,
        input: &Path,
        verifier: &mut Verifier,
    ) -> eyre::Result<()> {
        let info = target.symlink_metadata()?;
        match self {
            Self::Copy => {
                if !info.is_file() || !verifier.same_contents(target, input)? {
                    bail!("existing compiler output differs from its immutable receipt");
                }
            }
            Self::StoreLink => {
                if !info.file_type().is_symlink()
                    || fs::read_link(target)?.as_os_str() != input.as_os_str()
                    || !input.symlink_metadata()?.is_file()
                {
                    bail!("existing compiler output differs from its exact store link");
                }
            }
        }
        Ok(())
    }

    fn publish(self, input: &Path, target: &Path, owner: &Path) -> eyre::Result<()> {
        let temporary = target.with_extension(format!("dyndrv-{}.tmp", process::id()));
        let staged = match self {
            Self::Copy => Some(
                OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&temporary)?,
            ),
            Self::StoreLink => {
                unix_fs::symlink(owner, &temporary)?;
                None
            }
        };
        let result = (|| -> eyre::Result<()> {
            if let Some(mut staged) = staged {
                io::copy(&mut File::open(input)?, &mut staged)?;
                staged.set_permissions(Permissions::from_mode(
                    input.metadata()?.permissions().mode() | 0o200,
                ))?;
            }
            fs::rename(&temporary, target)?;
            Ok(())
        })();
        if result.is_err() {
            // Exclusive creation above succeeded: only our staging node is ours.
            let _ = fs::remove_file(temporary);
        }
        result
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Config {
    nix: PathBuf,
    socket: PathBuf,
    // The physical store mount is transport only; logical /nix/store paths are
    // the immutable inputs. Private local-store tests use their own store root.
    real_store: PathBuf,
    #[serde(default)]
    store_mode: StoreMode,
    // Delivery is a client policy, never part of the immutable compiler request.
    #[serde(default)]
    artifact_delivery: ArtifactDelivery,
    #[serde(default)]
    gc_roots: Option<PathBuf>,
    source: PathBuf,
    #[serde(default)]
    package_sources: Vec<PathBuf>,
    #[serde(default)]
    package_store_inputs: BTreeMap<PathBuf, PathBuf>,
    #[serde(default)]
    layout_environment_names: BTreeSet<String>,
    target: PathBuf,
    #[serde(default)]
    incremental: bool,
    #[serde(default)]
    incremental_observer: bool,
    // Experimental source staging; part of every immutable compiler recipe.
    #[serde(default)]
    source_overlay: bool,
    // Experimental empty-target bind mount; requires source_overlay. Both
    // settings are serialized into the immutable invocation recipe.
    #[serde(default)]
    private_target: bool,
    // Experimental same-user local transport. The private daemon must mount
    // this directory at jobserver::ROOT; it is not an immutable compiler input.
    #[serde(default)]
    jobserver_socket_directory: Option<PathBuf>,
    inputs: Vec<PathBuf>,
    environment_names: BTreeSet<String>,
    #[serde(default)]
    environment_prefixes: Vec<String>,
    omitted_environment_names: BTreeSet<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Backend {
    config: Config,
    // Ownership/audit operations need only configuration. Compiler inputs exist
    // only after the session's single, atomically published preparation.
    #[serde(skip)]
    compiler_inputs: Option<CompilerInputs>,
}

#[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct CompilerInputs {
    trees: BTreeMap<PathBuf, PathBuf>,
    builder_store: PathBuf,
}

impl CompilerInputs {
    fn in_session(
        session: &Path,
        prepare: impl FnOnce() -> eyre::Result<Self>,
    ) -> eyre::Result<Self> {
        let directory = jobserver::Broker::directory(session)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(O_NOFOLLOW | O_CLOEXEC | O_NONBLOCK)
            .open(session.join("prepare.lock"))?;
        let info = lock.metadata()?;
        if !info.is_file()
            || info.uid() != directory.metadata()?.uid()
            || info.nlink() != 1
            || info.mode() & 0o077 != 0
        {
            bail!("compiler preparation lock must be a private, owned regular file");
        }
        // Intentionally hold the process-shared lock through capture and import:
        // every compiler must use the same complete source view, exactly once.
        // Keep its inode after release so concurrent waiters cannot split owners.
        lock.lock()?;
        let published = session.join("compiler-inputs.json");
        match published.symlink_metadata() {
            Ok(info) => {
                if !info.is_file() || info.uid() != directory.metadata()?.uid() {
                    bail!("published compiler inputs must be an owned regular file");
                }
                return Ok(serde_json::from_slice(&fs::read(published)?)?);
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        // Exclusive creation rejects a failed or interrupted initializer instead
        // of quietly capturing a different source view on a later wrapper call.
        Backend::write_json(&session.join("prepare-started.json"), &process::id())
            .wrap_err("compiler session initialization was already attempted but not published")?;
        let inputs = prepare()?;
        let staged = session.join("compiler-inputs.tmp");
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&staged)?;
        let result = (|| {
            serde_json::to_writer(&mut file, &inputs)?;
            file.write_all(b"\n")?;
            fs::rename(&staged, &published)?;
            Ok(inputs)
        })();
        if result.is_err() {
            let _ = fs::remove_file(staged);
        }
        result
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Invocation {
    trees: BTreeMap<PathBuf, PathBuf>,
    source: PathBuf,
    target: PathBuf,
    cwd: PathBuf,
    output: CompilerOutput,
    incremental: Option<incremental::State>,
    jobserver: bool,
    source_overlay: bool,
    #[serde(default)]
    private_target: bool,
    program: Vec<u8>,
    arguments: Vec<Vec<u8>>,
    stdin: Option<Vec<u8>>,
    environment: Vec<(Vec<u8>, Vec<u8>)>,
    // Original Cargo path -> immutable Nix input, including transitive files.
    dependencies: BTreeMap<PathBuf, PathBuf>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Delivered {
    version: u32,
    // Early metadata has no exit status. A completed negative compiler probe
    // still owns its dep-info/objects, but cannot supply a completed link input.
    compiler_status: Option<i32>,
    owned: BTreeMap<PathBuf, Representation>,
    files: BTreeMap<PathBuf, PathBuf>,
    // Complete source images needed when a downstream compiler expands macros.
    trees: BTreeMap<PathBuf, PathBuf>,
}

impl Delivered {
    fn select_representations(&mut self, config: &Config, output: &CompilerOutput) {
        if config.artifact_delivery == ArtifactDelivery::SystemStoreRlibLinks
            && matches!(config.store_mode, StoreMode::System)
            && self.compiler_status == Some(0)
            && matches!(output, CompilerOutput::Directory(_))
        {
            // Keep selection separate from mutation: sibling metadata must be
            // owned Copy in this completed result, not a transitive input.
            let links: Vec<_> = self
                .owned
                .keys()
                .filter(|path| {
                    path.parent() == output.directory()
                        && self.rlib_link_eligible(path, &config.target)
                })
                .cloned()
                .collect();
            for path in links {
                self.owned.insert(path, Representation::StoreLink);
            }
        }
    }

    fn validate(&self, path: &Path, config: &Config) -> eyre::Result<()> {
        if self.version != 6
            || !self.owned.contains_key(path)
            || !self.owned.keys().all(|path| self.files.contains_key(path))
        {
            bail!("artifact receipt version or ownership mismatch");
        }
        for (target, representation) in &self.owned {
            if *representation == Representation::StoreLink
                && (config.artifact_delivery != ArtifactDelivery::SystemStoreRlibLinks
                    || !matches!(config.store_mode, StoreMode::System)
                    || self.compiler_status != Some(0)
                    || !self.rlib_link_eligible(target, &config.target))
            {
                bail!("artifact receipt claims an ineligible store link");
            }
        }
        Ok(())
    }

    fn rlib_link_eligible(&self, path: &Path, target: &Path) -> bool {
        let Ok(relative) = path.strip_prefix(target) else {
            return false;
        };
        let components: Vec<_> = relative.components().collect();
        let ordinary = matches!(components.as_slice(),
            [Component::Normal(_), Component::Normal(deps), Component::Normal(_)]
                | [Component::Normal(_), Component::Normal(_), Component::Normal(deps), Component::Normal(_)]
                if *deps == OsStr::new("deps"));
        ordinary
            && path.components().collect::<PathBuf>().as_os_str() == path.as_os_str()
            && path.extension() == Some(OsStr::new("rlib"))
            && self.owned.get(&path.with_extension("rmeta")) == Some(&Representation::Copy)
            && self.files.contains_key(&path.with_extension("rmeta"))
            && !self
                .trees
                .keys()
                .any(|root| root.starts_with(target) && path.starts_with(root))
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SnapshotReferences {
    version: u32,
    target: PathBuf,
    store_paths: BTreeSet<PathBuf>,
    owned_files: usize,
    incremental_children: usize,
    seconds: f64,
}

// Own the child until its pipes are drained and it is reaped. In particular,
// an early metadata decoder/import error must cancel the live Nix client.
struct CapturedProcess {
    child: std::process::Child,
    stdout: Option<std::thread::JoinHandle<std::io::Result<Vec<u8>>>>,
    stdin: Option<std::thread::JoinHandle<std::io::Result<()>>>,
    stderr: BufReader<std::process::ChildStderr>,
    bytes: Vec<u8>,
}

impl CapturedProcess {
    fn spawn(command: &mut Command, input: Option<Vec<u8>>) -> std::io::Result<Self> {
        let mut child = command
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let stdin = input.map(|input| {
            let mut pipe = child.stdin.take().unwrap();
            std::thread::spawn(move || pipe.write_all(&input))
        });
        let mut stdout = child.stdout.take().unwrap();
        let stdout = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            stdout.read_to_end(&mut bytes)?;
            Ok(bytes)
        });
        let stderr = BufReader::new(child.stderr.take().unwrap());
        Ok(Self {
            child,
            stdout: Some(stdout),
            stdin,
            stderr,
            bytes: Vec::new(),
        })
    }

    fn line(&mut self) -> std::io::Result<&[u8]> {
        let offset = self.bytes.len();
        self.stderr.read_until(b'\n', &mut self.bytes)?;
        Ok(&self.bytes[offset..])
    }

    fn finish(mut self) -> std::io::Result<std::process::Output> {
        let status = self.child.wait()?;
        let stdout = self
            .stdout
            .take()
            .unwrap()
            .join()
            .map_err(|_| std::io::Error::other("compiler stdout reader panicked"))??;
        if let Some(writer) = self.stdin.take() {
            match writer
                .join()
                .map_err(|_| std::io::Error::other("compiler stdin writer panicked"))?
            {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => {}
                Err(error) => return Err(error),
            }
        }
        Ok(std::process::Output {
            status,
            stdout,
            stderr: std::mem::take(&mut self.bytes),
        })
    }
}

impl Drop for CapturedProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
enum CompilerOutput {
    Directory(PathBuf),
    File(PathBuf),
    Stdout,
}

impl CompilerOutput {
    fn from_args(args: &[OsString], target: &Path) -> eyre::Result<Self> {
        let mut output = None;
        let mut remaining = args.iter();
        while let Some(arg) = remaining.next() {
            let candidate = if arg == "--out-dir" {
                Self::Directory(
                    remaining
                        .next()
                        .ok_or_else(|| eyre::eyre!("missing output directory"))?
                        .into(),
                )
            } else if arg == "-o" {
                match remaining
                    .next()
                    .ok_or_else(|| eyre::eyre!("missing output file"))?
                {
                    path if path == "-" => Self::Stdout,
                    path => Self::File(path.into()),
                }
            } else {
                continue;
            };
            if output.replace(candidate).is_some() {
                bail!("multiple compiler output locations are not supported");
            }
        }
        let output =
            output.ok_or_else(|| eyre::eyre!("compiler lacks an explicit output location"))?;
        output.validate(target)?;
        // The observed rustix probes emit metadata only. Other explicit emit
        // layouts need their own artifact contract, including side outputs.
        if !matches!(output, Self::Directory(_))
            && args
                .iter()
                .filter(|arg| arg.as_bytes().starts_with(b"--emit"))
                .map(OsString::as_os_str)
                .collect::<Vec<_>>()
                != [OsStr::new("--emit=metadata")]
        {
            bail!("explicit file/stdout output currently requires --emit=metadata");
        }
        Ok(output)
    }

    fn validate(&self, target: &Path) -> eyre::Result<()> {
        let path = match self {
            Self::Directory(path) | Self::File(path) => path,
            Self::Stdout => return Ok(()),
        };
        Backend::scratch(path)?;
        if !path.starts_with(target) || path == target {
            bail!("compiler output must be inside the Cargo target");
        }
        Ok(())
    }

    fn directory(&self) -> Option<&Path> {
        match self {
            Self::Directory(path) => Some(path),
            Self::File(path) => path.parent(),
            Self::Stdout => None,
        }
    }

    fn writable<'a>(&'a self, root: &Path, cwd: &'a Path) -> Option<source_wrap::Writable<'a>> {
        match self {
            Self::Directory(path) if path.starts_with(root) => {
                Some(source_wrap::Writable::Directory(path))
            }
            Self::File(path) if path.starts_with(root) => Some(source_wrap::Writable::File(path)),
            Self::Stdout if cwd.starts_with(root) => {
                Some(source_wrap::Writable::ScratchDirectory(cwd))
            }
            _ => None,
        }
    }
}

impl Invocation {
    fn writable<'a>(&'a self, root: &Path) -> Vec<source_wrap::Writable<'a>> {
        let mut writable = Vec::new();
        if self.target.starts_with(root) && self.target != root {
            // The source image has an empty target placeholder. Generated trees
            // are materialized beneath it and verified independently below.
            writable.push(if self.private_target {
                source_wrap::Writable::EmptyDirectory(&self.target)
            } else {
                source_wrap::Writable::Directory(&self.target)
            });
            if matches!(self.output, CompilerOutput::Stdout)
                && self.cwd.starts_with(root)
                && !self.cwd.starts_with(&self.target)
            {
                writable.push(source_wrap::Writable::ScratchDirectory(&self.cwd));
            }
        } else if let Some(output) = self.output.writable(root, &self.cwd) {
            writable.push(output);
        }
        writable
    }
}

impl Backend {
    fn configured_socket(config: &Config) -> eyre::Result<PathBuf> {
        match config.store_mode {
            StoreMode::Private => {
                if config.artifact_delivery != ArtifactDelivery::Copy {
                    bail!("store-link delivery requires the administered system store");
                }
                bound_graph::socket(&config.socket)
            }
            StoreMode::System => {
                let path = config.socket.canonicalize()?;
                let info = config.socket.symlink_metadata()?;
                let parent = path
                    .parent()
                    .ok_or_else(|| eyre::eyre!("daemon socket has no parent"))?
                    .metadata()?;
                let Some(roots) = &config.gc_roots else {
                    bail!("system mode requires durable GC roots");
                };
                let directory = jobserver::Broker::directory(roots)?;
                let uid = directory.metadata()?.uid();
                if path != Path::new("/nix/var/nix/daemon-socket/socket")
                    || config.real_store != Path::new("/")
                    || !info.file_type().is_socket()
                    || parent.uid() != info.uid()
                    || parent.uid() == uid
                    || parent.mode() & 0o022 != 0
                {
                    bail!(
                        "system mode requires the administrator-owned system daemon and durable GC roots"
                    );
                }
                if config.artifact_delivery == ArtifactDelivery::SystemStoreRlibLinks {
                    if config.socket.as_os_str() != path.as_os_str() {
                        bail!("store-link daemon socket must have its canonical spelling");
                    }
                    for ancestor in path.ancestors().skip(1) {
                        if !Self::protected_store_entry(ancestor, info.uid())?.is_dir() {
                            bail!("system daemon ancestor is not a real directory");
                        }
                    }
                    for ancestor in Path::new("/nix/store").ancestors() {
                        if !Self::protected_store_entry(ancestor, info.uid())?.is_dir() {
                            bail!("system store ancestor is not a real directory");
                        }
                    }
                    Self::readonly_store_mount(Path::new("/nix/store"))?;
                }
                Ok(path)
            }
        }
    }

    // std has no effective-access or mount-flags query. These small FFI calls
    // check the live administered namespace, never a serialized trust flag.
    fn protected_store_entry(path: &Path, admin: u32) -> eyre::Result<Metadata> {
        let info = path.symlink_metadata()?;
        if info.file_type().is_symlink() || info.uid() != admin {
            bail!(
                "system store path is not owned by its administrator: {}",
                path.display()
            );
        }
        let name = CString::new(path.as_os_str().as_bytes())?;
        // SAFETY: name is a live NUL-terminated string; faccessat only queries
        // the named path and AT_EACCESS selects the caller's effective IDs.
        if unsafe { libc::faccessat(AT_FDCWD, name.as_ptr(), W_OK, AT_EACCESS) } == 0 {
            bail!(
                "system store path is writable by the caller: {}",
                path.display()
            );
        }
        let error = IoError::last_os_error();
        if !matches!(error.raw_os_error(), Some(EACCES | EROFS)) {
            return Err(error.into());
        }
        Ok(info)
    }

    fn readonly_store_mount(path: &Path) -> eyre::Result<()> {
        let name = CString::new(path.as_os_str().as_bytes())?;
        let mut stats = MaybeUninit::uninit();
        // SAFETY: name remains NUL-terminated and live; stats is correctly
        // aligned writable storage. statvfs initializes it on success only.
        if unsafe { libc::statvfs(name.as_ptr(), stats.as_mut_ptr()) } != 0 {
            return Err(IoError::last_os_error().into());
        }
        // SAFETY: the successful statvfs call initialized all fields above.
        if unsafe { stats.assume_init() }.f_flag & ST_RDONLY == 0 {
            bail!("store-link delivery requires a read-only system store mount");
        }
        Ok(())
    }

    fn validate_store_backing(&self, input: &Path) -> eyre::Result<()> {
        let physical = self.physical(input)?;
        if self.config.artifact_delivery != ArtifactDelivery::SystemStoreRlibLinks
            || physical.as_os_str() != input.as_os_str()
            || input.components().collect::<PathBuf>().as_os_str() != input.as_os_str()
        {
            bail!("store-link backing requires a normalized system store path");
        }
        let socket = Self::configured_socket(&self.config)?;
        let admin = socket.symlink_metadata()?.uid();
        let store = Path::new("/nix/store");
        let device = store.symlink_metadata()?.dev();
        for path in input.ancestors().take_while(|path| *path != store) {
            let info = Self::protected_store_entry(path, admin)?;
            if info.dev() != device
                || info.mode() & 0o222 != 0
                || (path == input && !info.is_file())
                || (path != input && !info.is_dir())
            {
                bail!("store-link backing is not a protected immutable regular file");
            }
            Self::readonly_store_mount(path)?;
        }
        Ok(())
    }

    async fn pin<T: DaemonStore>(&self, daemon: &mut T, input: &Path) -> eyre::Result<()> {
        let Some(directory) = &self.config.gc_roots else {
            return Ok(());
        };
        let _directory = jobserver::Broker::directory(directory)?;
        let store_path = Self::store_path(input)?;
        daemon.add_temp_root(&store_path).await?;
        let owner = store_path.to_absolute_path(&StoreDir::default());
        let root = directory.join(
            owner
                .file_name()
                .expect("validated store paths have a basename"),
        );
        match unix_fs::symlink(&owner, &root) {
            Ok(()) => {}
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                if fs::read_link(&root)? != owner {
                    bail!("GC root already names a different immutable owner");
                }
            }
            Err(error) => return Err(error.into()),
        }
        daemon.add_indirect_root(&root.into_bytes()).await?;
        Ok(())
    }

    // Stock Cargo nests its workspace wrapper inside RUSTC_WRAPPER. Keep the
    // entire driver command intact; inspect the inner arguments only for queries.
    fn compiler<'a>(
        program: &'a OsStr,
        args: &'a [OsString],
    ) -> Option<(&'a OsStr, &'a [OsString])> {
        match Path::new(program).file_name()?.as_bytes() {
            b"rustc" => Some((program, args)),
            b"clippy-driver" => {
                let (rustc, args) = args.split_first()?;
                (Path::new(rustc).file_name() == Some(OsStr::new("rustc")))
                    .then_some((rustc.as_os_str(), args))
            }
            _ => None,
        }
    }

    pub(super) fn is_probe(program: &OsStr, args: &[OsString]) -> bool {
        if Path::new(program).file_name() == Some(OsStr::new("rustdoc")) {
            return args == [OsStr::new("-vV")];
        }
        let Some((_, args)) = Self::compiler(program, args) else {
            return false;
        };
        if args == [OsStr::new("-vV")]
            || args == [OsStr::new("-V")]
            || args == [OsStr::new("--version")]
            || args == [OsStr::new("--version"), OsStr::new("--verbose")]
        {
            return true;
        }
        // Maturin queries the target cfg before invoking Cargo. Match only this
        // read-only argument shape; compiler actions still require delivery.
        if let [print, cfg, target, _triple] = args
            && print == "--print"
            && cfg == "cfg"
            && target == "--target"
        {
            return true;
        }
        let permitted = [
            b"file-names".as_slice(),
            b"sysroot",
            b"split-debuginfo",
            b"crate-name",
            b"cfg",
        ];
        args.first().is_some_and(|arg| arg == "-")
            && args.iter().any(|arg| arg == "--print=file-names")
            && args
                .iter()
                .filter_map(|arg| arg.as_bytes().strip_prefix(b"--print="))
                .all(|value| permitted.contains(&value))
            && !args
                .iter()
                .any(|arg| arg == "--emit" || arg.as_bytes().starts_with(b"--emit="))
    }

    fn digest(bytes: &[u8]) -> String {
        SHA256.digest(bytes).as_base16().as_bare().to_string()
    }

    fn store_path(path: &Path) -> eyre::Result<StorePath> {
        let part = path
            .strip_prefix("/nix/store")?
            .components()
            .next()
            .ok_or_else(|| eyre::eyre!("missing store path"))?;
        let Component::Normal(part) = part else {
            bail!("invalid store path")
        };
        StorePath::from_base_path(
            part.to_str()
                .ok_or_else(|| eyre::eyre!("non-UTF8 store path"))?,
        )
        .map_err(Into::into)
    }

    fn scratch(path: &Path) -> eyre::Result<()> {
        if !path.is_absolute()
            || !["/tmp", "/build"].iter().any(|root| path.starts_with(root))
            || path
                .components()
                .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
        {
            bail!(
                "initial Nix bridge requires normalized absolute scratch paths under /tmp or /build"
            )
        }
        Ok(())
    }

    fn physical(&self, path: &Path) -> eyre::Result<PathBuf> {
        Self::store_path(path)?;
        if path
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
        {
            bail!("immutable artifact path must be normalized");
        }
        Ok(self.config.real_store.join(path.strip_prefix("/")?))
    }

    pub(super) fn write_json(path: &Path, value: &impl Serialize) -> eyre::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)?;
        serde_json::to_writer(&mut file, value)?;
        file.write_all(b"\n")?;
        Ok(())
    }

    fn bound_store_root(source: &Path, input: &Path) -> eyre::Result<Option<StorePath>> {
        for path in [source, input] {
            if !path.is_absolute()
                || path
                    .components()
                    .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
            {
                bail!("immutable package binding paths must be normalized and absolute");
            }
        }
        let root = Self::store_path(input)?;
        if root.to_absolute_path(&StoreDir::default()).as_os_str() != input.as_os_str() {
            return Ok(None);
        }
        let source = fs::metadata(source)?;
        let input = fs::symlink_metadata(input)?;
        Ok((source.is_dir()
            && input.is_dir()
            && source.dev() == input.dev()
            && source.ino() == input.ino())
        .then_some(root))
    }

    pub(super) fn configure(path: &Path) -> eyre::Result<Self> {
        let config: Config = serde_json::from_slice(&fs::read(path)?)?;
        if config.private_target && !config.source_overlay {
            bail!("private target storage requires source overlays");
        }
        Self::configured_socket(&config)?;
        if let Some(directory) = &config.gc_roots {
            jobserver::Broker::directory(directory)?;
        }
        let store_dir = StoreDir::default();
        Tool::bound(&store_dir, &config.nix)?;
        Self::scratch(&config.source)?;
        Self::scratch(&config.target)?;
        if config.source.starts_with(&config.target)
            || !config.real_store.is_absolute()
            || !config
                .environment_names
                .is_disjoint(&config.omitted_environment_names)
            || config.environment_names.contains("CARGO_MAKEFLAGS")
        {
            bail!(
                "invalid source/target, store mount or environment policy; raw jobserver flags cannot be serialized"
            )
        }
        if let Some(directory) = &config.jobserver_socket_directory {
            Self::scratch(directory)?;
            if [&config.source, &config.target]
                .into_iter()
                .chain(&config.package_sources)
                .any(|tree| directory.starts_with(tree) || tree.starts_with(directory))
            {
                bail!("jobserver transport must be disjoint from compiler source and target trees");
            }
            jobserver::Broker::directory(directory)?;
        }
        fs::create_dir_all(&config.target)?;
        if config.target.canonicalize()? != config.target {
            bail!("target directory must not traverse symlinks");
        }
        let state = config.target.join(".dyndrv-delivered");
        fs::create_dir_all(&state)?;
        if !state.symlink_metadata()?.is_dir() {
            bail!("artifact state must be a real directory");
        }
        if state.join("rejected").try_exists()? {
            bail!(
                "Cargo target contains artifacts from a rejected backend run; restore a verified target or use an empty target"
            );
        }
        let references: BTreeSet<_> = config
            .inputs
            .iter()
            .map(|path| Self::store_path(path))
            .collect::<eyre::Result<_>>()?;
        if references.is_empty() {
            bail!("explicit compiler and builder runtime inputs required")
        }
        let mut roots = BTreeSet::new();
        for source in iter::once(&config.source).chain(&config.package_sources) {
            if source.starts_with(&config.target)
                || roots
                    .iter()
                    .any(|root: &&PathBuf| root.starts_with(source) || source.starts_with(root))
            {
                bail!("source roots must be disjoint and cannot be inside target");
            }
            if !source.is_dir() {
                bail!(
                    "compiler source root must be a directory: {}",
                    source.display()
                );
            }
            roots.insert(source);
        }
        Ok(Self {
            config,
            compiler_inputs: None,
        })
    }

    pub(super) fn prepare(mut self, session: &Path) -> eyre::Result<Self> {
        self.compiler_inputs = Some(CompilerInputs::in_session(session, || {
            self.prepare_inputs(&session.join("prepare.json"))
        })?);
        Ok(self)
    }

    fn compiler_inputs(&self) -> eyre::Result<&CompilerInputs> {
        self.compiler_inputs
            .as_ref()
            .ok_or_else(|| eyre::eyre!("compiler inputs have not been prepared"))
    }

    #[tokio::main(flavor = "current_thread")]
    async fn prepare_inputs(&self, evidence: &Path) -> eyre::Result<CompilerInputs> {
        let started = Instant::now();
        let config = &self.config;
        Self::configured_socket(config)?;
        let store_dir = StoreDir::default();
        let references = config
            .inputs
            .iter()
            .map(|path| Self::store_path(path))
            .collect::<eyre::Result<BTreeSet<_>>>()?;
        let handshake_started = Instant::now();
        let mut daemon = harmonia_store_remote::DaemonClientBuilder::new()
            .set_store_dir(&store_dir)
            .build_unix(&config.socket)
            .await?
            .handshake()
            .await?;
        let handshake_seconds = handshake_started.elapsed().as_secs_f64();
        let mut trees = BTreeMap::new();
        let mut tree_times = Vec::new();
        for source in iter::once(&config.source).chain(&config.package_sources) {
            let tree_started = Instant::now();
            // Staging precedes Cargo; this complete immutable view is fixed at
            // the first compiler. Native build-script outputs under target are
            // excluded here and captured per invocation from OUT_DIR/-L below.
            // Namespace package mounts can have scratch paths while retaining
            // the exact directory identity of a registered immutable store root.
            // Subdirectories and lookalike trees still take the validated NAR route.
            let store_input = config
                .package_store_inputs
                .get(source)
                .filter(|_| source != &config.source)
                .and_then(|input| Self::bound_store_root(source, input).ok().flatten());
            let mut method = "store-path";
            let mut nar_bytes = 0;
            let input = if source.starts_with("/nix/store") {
                Self::input_tree(&mut daemon, source).await?
            } else if let Some(input) = store_input
                && daemon.is_valid_path(&input).await?
            {
                method = "store-root";
                input.to_absolute_path(&store_dir)
            } else {
                method = "nar";
                let source = WorkspaceSource::new(source, source, &config.target)?;
                let nar = source.nar()?;
                nar_bytes = nar.len();
                store::add_prepared_nar(&mut daemon, &nar)
                    .await?
                    .to_absolute_path(&store_dir)
            };
            tree_times.push(serde_json::json!({
                "logical": source,
                "method": method,
                "narBytes": nar_bytes,
                "seconds": tree_started.elapsed().as_secs_f64(),
            }));
            trees.insert(source.clone(), input);
        }
        let builder_started = Instant::now();
        let builder_nar = store::source_nar(&std::env::current_exe()?)?;
        let builder_store = daemon
            .add_ca_to_store(
                "stock-cargo-compiler-builder",
                ContentAddressMethodAlgorithm::NixArchive(SHA256),
                &references,
                false,
                builder_nar.as_slice(),
            )
            .await?
            .path
            .to_absolute_path(&store_dir);
        let builder_seconds = builder_started.elapsed().as_secs_f64();
        let prepared = CompilerInputs {
            trees,
            builder_store,
        };
        let guard_started = Instant::now();
        if config.gc_roots.is_some() {
            let references = prepared
                .trees
                .values()
                .chain([&prepared.builder_store])
                .map(|path| Self::store_path(path))
                .collect::<eyre::Result<BTreeSet<_>>>()?;
            let guard = daemon
                .add_ca_to_store(
                    "stock-cargo-session-inputs",
                    ContentAddressMethodAlgorithm::Text,
                    &references,
                    false,
                    &b"compiler session inputs"[..],
                )
                .await?
                .path
                .to_absolute_path(&store_dir);
            self.pin(&mut daemon, &guard).await?;
        }
        Self::write_json(
            evidence,
            &serde_json::json!({
                "seconds": started.elapsed().as_secs_f64(),
                "handshakeSeconds": handshake_seconds,
                "trees": tree_times,
                "builderImportSeconds": builder_seconds,
                "builderNarBytes": builder_nar.len(),
                "guardSeconds": guard_started.elapsed().as_secs_f64(),
            }),
        )?;
        Ok(prepared)
    }

    pub(super) fn reject_incomplete(&self) -> eyre::Result<()> {
        fs::write(
            self.config.target.join(".dyndrv-delivered/rejected"),
            b"Cargo swallowed an incomplete Nix compiler invocation\n",
        )?;
        Ok(())
    }

    async fn input_tree<T: DaemonStore>(daemon: &mut T, path: &Path) -> eyre::Result<PathBuf> {
        if path.starts_with("/nix/store") {
            Self::store_path(path)?;
            if !path.is_dir() {
                bail!("immutable input tree is not a directory");
            }
            return Ok(path.to_path_buf());
        }
        Self::scratch(path)?;
        source_wrap::validate_tree(path)?;
        let nar = store::source_nar(path)?;
        Ok(store::add_prepared_nar(daemon, &nar)
            .await?
            .to_absolute_path(&StoreDir::default()))
    }

    fn input_directories(args: &[OsString]) -> eyre::Result<BTreeSet<PathBuf>> {
        let mut directories = BTreeSet::new();
        if let Some(path) = std::env::var_os("OUT_DIR") {
            directories.insert(path.into());
        }
        let mut remaining = args.iter();
        while let Some(arg) = remaining.next() {
            let search = if arg == "-L" {
                Some(
                    remaining
                        .next()
                        .ok_or_else(|| eyre::eyre!("missing library search directory"))?
                        .as_bytes(),
                )
            } else {
                arg.as_bytes().strip_prefix(b"-L")
            };
            if let Some(search) = search {
                if search.starts_with(b"dependency=") {
                    continue;
                }
                let path = search
                    .iter()
                    .position(|c| *c == b'=')
                    .map_or(search, |at| &search[at + 1..]);
                directories.insert(OsString::from_vec(path.to_vec()).into());
            }
        }
        Ok(directories)
    }

    fn receipt(&self, path: &Path) -> PathBuf {
        self.config.target.join(".dyndrv-delivered").join(format!(
            "{}.json",
            Self::digest(path.as_os_str().as_bytes())
        ))
    }

    fn delivered(&self, path: &Path) -> eyre::Result<Delivered> {
        let file = self.receipt(path);
        let directory = file.parent().expect("receipt has an ownership directory");
        if directory.canonicalize()?.as_os_str() != directory.as_os_str() {
            bail!("artifact ownership directory must not traverse symlinks");
        }
        if !file.symlink_metadata()?.is_file() {
            bail!("artifact receipt must be a regular file");
        }
        let receipt: Delivered = serde_json::from_slice(&fs::read(file)?)?;
        receipt.validate(path, &self.config)?;
        Ok(receipt)
    }

    pub(super) fn snapshot_references(&self) -> eyre::Result<SnapshotReferences> {
        let started = Instant::now();
        if self.config.artifact_delivery == ArtifactDelivery::SystemStoreRlibLinks {
            Self::configured_socket(&self.config)?;
        }
        Self::scratch(&self.config.target)?;
        let directory = self.config.target.join(".dyndrv-delivered");
        if directory.canonicalize()? != directory || !self.config.real_store.is_absolute() {
            bail!("snapshot ownership directory or physical store is not normalized");
        }
        let mut verifier = Verifier::default();
        let (mut references, incremental_children, _leases) =
            incremental::State::snapshot_references(self, &mut verifier)?;
        let mut owners = BTreeMap::new();
        // Overlapping transitive maps may repeat immutable paths. Cache only
        // exact backing paths for this snapshot, with files and trees separate.
        // Mutable targets and their complete byte comparisons are never cached.
        let mut validated_files = BTreeSet::new();
        let mut validated_trees = BTreeSet::new();
        for entry in fs::read_dir(&directory)? {
            let entry = entry?;
            let path = entry.path();
            if path.file_name() == Some(OsStr::new("incremental")) && entry.file_type()?.is_dir() {
                continue;
            }
            if !entry.file_type()?.is_file() || path.extension() != Some(OsStr::new("json")) {
                bail!("snapshot contains rejected, unfinished or unknown artifact state");
            }
            let receipt: Delivered = serde_json::from_slice(&fs::read(&path)?)?;
            let keys: Vec<_> = receipt
                .owned
                .keys()
                .filter(|p| self.receipt(p) == path)
                .collect();
            let [key] = keys.as_slice() else {
                bail!("artifact receipt filename does not identify one owned file");
            };
            receipt.validate(key, &self.config)?;
            for input in receipt.trees.values() {
                if !validated_trees.contains(input) {
                    if !self.physical(input)?.is_dir() {
                        bail!("snapshot dependency source lacks immutable backing");
                    }
                    references
                        .insert(Self::store_path(input)?.to_absolute_path(&StoreDir::default()));
                    validated_trees.insert(input.clone());
                }
            }
            self.validate_target_parent(key)?;
            match key.symlink_metadata() {
                Ok(_) => {
                    if receipt.compiler_status.is_none() {
                        bail!("snapshot artifact is incomplete");
                    }
                    self.validate_receipt_contents(
                        key,
                        &receipt,
                        &receipt.files[*key],
                        &mut verifier,
                    )?;
                    owners.insert((*key).clone(), receipt.files[*key].clone());
                }
                // Build scripts may remove their positive/negative probe files.
                // Keep the immutable references named by those retained receipts,
                // without restoring deleted files or asserting they still exist.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    if receipt.owned[*key] == Representation::StoreLink {
                        self.validate_store_backing(&receipt.files[*key])?;
                    }
                }
                Err(error) => return Err(error.into()),
            }
            // Transitive mappings may describe earlier compiler inputs. Retain
            // those immutable references without pretending they all describe
            // the current mutable target. Each current key is checked above.
            for (target, input) in &receipt.files {
                Self::scratch(target)?;
                if !target.starts_with(&self.config.target) {
                    bail!("snapshot dependency is outside target or lacks immutable backing");
                }
                if !validated_files.contains(input) {
                    if !self.physical(input)?.symlink_metadata()?.is_file() {
                        bail!("snapshot dependency is outside target or lacks immutable backing");
                    }
                    references
                        .insert(Self::store_path(input)?.to_absolute_path(&StoreDir::default()));
                    validated_files.insert(input.clone());
                }
            }
        }
        Ok(SnapshotReferences {
            version: 1,
            target: self.config.target.clone(),
            store_paths: references,
            owned_files: owners.len(),
            incremental_children,
            seconds: started.elapsed().as_secs_f64(),
        })
    }

    fn previous_objects(&self, artifact: &Path) -> eyre::Result<BTreeMap<PathBuf, PathBuf>> {
        if !self.receipt(artifact).try_exists()? {
            return Ok(BTreeMap::new());
        }
        let receipt = self.delivered(artifact)?;
        Ok(receipt
            .owned
            .into_keys()
            .filter(|path| path.as_os_str().as_bytes().ends_with(b".rcgu.o"))
            .map(|path| {
                let owner = receipt.files[&path].clone();
                (path, owner)
            })
            .collect())
    }

    fn validate_replacement(&self, target: &Path) -> eyre::Result<()> {
        self.validate_target_parent(target)?;
        match target.symlink_metadata() {
            Ok(_) => {
                let old = self
                    .delivered(target)
                    .wrap_err("existing compiler output is not owned by dyndrv")?;
                self.validate_receipt_contents(
                    target,
                    &old,
                    &old.files[target],
                    &mut Verifier::default(),
                )?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        Ok(())
    }

    fn validate_target_parent(&self, target: &Path) -> eyre::Result<()> {
        Self::scratch(target)?;
        let parent = target
            .parent()
            .ok_or_else(|| eyre::eyre!("artifact has no parent"))?;
        if !target.starts_with(&self.config.target)
            || target == self.config.target
            || target.components().collect::<PathBuf>().as_os_str() != target.as_os_str()
        {
            bail!("artifact must have a normalized target path and real parent directories");
        }
        // Build scripts may remove entire probe directories. Inspect every
        // remaining ancestor so a dangling directory link cannot hide a leaf.
        for ancestor in parent.ancestors() {
            match ancestor.symlink_metadata() {
                Ok(info) if info.is_dir() => {}
                Err(error) if error.kind() == ErrorKind::NotFound => {}
                Ok(_) => bail!("artifact parent is not a real directory"),
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }

    // Reusing parsed metadata still validates the current node. Only a recorded,
    // exact store link with live immutable backing can omit the byte comparison.
    fn validate_receipt_contents(
        &self,
        target: &Path,
        receipt: &Delivered,
        input: &Path,
        verifier: &mut Verifier,
    ) -> eyre::Result<()> {
        receipt.validate(target, &self.config)?;
        self.validate_target_parent(target)?;
        let representation = receipt.owned[target];
        if representation == Representation::StoreLink {
            if receipt.files[target].as_os_str() != input.as_os_str() {
                bail!("current artifact receipt differs from merged dependency owner");
            }
            self.validate_store_backing(input)?;
        }
        representation.validate_contents(target, &self.physical(input)?, verifier)
    }

    fn publish_receipt(&self, target: &Path, delivered: &Delivered) -> eyre::Result<()> {
        let receipt = self.receipt(target);
        let staged = receipt.with_extension(format!("{}.tmp", std::process::id()));
        Self::write_json(&staged, delivered)?;
        fs::rename(staged, receipt)?;
        Ok(())
    }

    async fn deliver_metadata<T: DaemonStore>(
        &self,
        daemon: &mut T,
        invocation: &Invocation,
        artifact: &metadata::Artifact,
    ) -> eyre::Result<PathBuf> {
        let target = &artifact.path;
        let directory = target.parent().unwrap();
        if directory.canonicalize()? != directory {
            bail!("metadata output directory traverses symlinks")
        }
        self.validate_replacement(target)?;
        let temporary =
            target.with_extension(format!("dyndrv-metadata-{}.tmp", std::process::id()));
        let mut staged = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        staged.write_all(&artifact.bytes)?;
        drop(staged);
        let result = async {
            let owner = self.metadata_owner(daemon, &temporary, invocation).await?;
            // A linked archive exposes the store's fixed mtime. Refresh its
            // copied metadata before announcing readiness so Cargo can observe
            // a rebuild even when only bundled native code changed. Consumers
            // may start now, so completion must preserve this timestamp.
            if self.config.artifact_delivery != ArtifactDelivery::SystemStoreRlibLinks
                && target.try_exists()?
                && fs::read(target)? == artifact.bytes
            {
                fs::remove_file(&temporary)?;
            } else {
                fs::rename(&temporary, target)?;
            }
            let mut files = Self::metadata_files(invocation.dependencies.clone());
            files.insert(target.clone(), owner.clone());
            self.publish_receipt(
                target,
                &Delivered {
                    version: 6,
                    compiler_status: None,
                    owned: BTreeMap::from([(target.clone(), Representation::Copy)]),
                    files,
                    trees: invocation.trees.clone(),
                },
            )?;
            Ok(owner)
        }
        .await;
        if result.is_err() {
            let _ = fs::remove_file(temporary);
        }
        result
    }

    fn dependencies(
        &self,
        args: &[OsString],
        trees: &mut BTreeMap<PathBuf, PathBuf>,
    ) -> eyre::Result<BTreeMap<PathBuf, PathBuf>> {
        let mut dependencies = BTreeMap::new();
        let mut source_trees = BTreeMap::new();
        let mut retain_sources = |sources: BTreeMap<PathBuf, PathBuf>| -> eyre::Result<()> {
            for (root, input) in sources {
                // A Cargo-fresh macro can still read newly edited data. Prefer
                // this session's declared trees and freshly captured inputs to
                // its producer's snapshot. Other generated sources keep their
                // immutable owner, which the target's GC roots retain.
                let (root, input) = self
                    .compiler_inputs()?
                    .trees
                    .iter()
                    .chain(trees.iter())
                    .find(|(current, _)| {
                        root.starts_with(current)
                            && (!root.starts_with(&self.config.target)
                                || current.starts_with(&self.config.target))
                    })
                    .unwrap_or((&root, &input));
                if let Some(previous) = source_trees.insert(root.clone(), input.clone())
                    && previous != *input
                {
                    bail!("conflicting dependency source owners");
                }
            }
            Ok(())
        };
        let mut metadata = BTreeSet::new();
        let mut expanded = BTreeSet::new();
        let linking = Self::links_dependencies(args);
        let mut remaining = args.iter();
        while let Some(arg) = remaining.next() {
            if arg == "--extern" {
                let value = remaining
                    .next()
                    .ok_or_else(|| eyre::eyre!("missing extern input"))?
                    .as_bytes();
                // Cargo supplies the toolchain's proc_macro crate without an
                // artifact path. Its sysroot is already a declared rustc input.
                if value == b"proc_macro" {
                    continue;
                }
                let index = value
                    .iter()
                    .position(|b| *b == b'=')
                    .ok_or_else(|| eyre::eyre!("extern without an explicit artifact"))?;
                let path = PathBuf::from(OsString::from_vec(value[index + 1..].to_vec()));
                let receipt = self
                    .delivered(&path)
                    .wrap_err("dependency has no immutable delivery receipt; no native fallback")?;
                if linking && receipt.compiler_status != Some(0) {
                    bail!("link dependency is not completely delivered");
                }
                retain_sources(receipt.trees)?;
                let files = if !linking && path.extension() == Some(OsStr::new("rmeta")) {
                    Self::metadata_files(receipt.files)
                } else {
                    receipt.files
                };
                Self::merge_dependencies(&mut dependencies, files, &mut metadata)?;
            }
        }
        // A library may finish while an upstream producer is still linking.
        // Its early metadata closure is sufficient for another library, but a
        // final linker needs the upstream archives. Cargo owns that readiness
        // decision; refresh receipts here without starting another build graph.
        while linking && let Some(path) = metadata.pop_first() {
            if expanded.insert(path.clone()) {
                let receipt = self.delivered(&path)?;
                if receipt.compiler_status != Some(0) {
                    bail!("transitive link dependency is not completely delivered");
                }
                retain_sources(receipt.trees)?;
                Self::merge_dependencies(&mut dependencies, receipt.files, &mut metadata)?;
            }
        }
        // Direct dependencies carry overlapping transitive receipts. Merge and
        // reject conflicting owners before reading each distinct artifact once.
        for (target, immutable) in &dependencies {
            if !target.starts_with(&self.config.target) {
                bail!("delivered dependency differs from immutable input");
            }
            match self.receipt(target).symlink_metadata() {
                Ok(_) => {
                    let receipt = self.delivered(target)?;
                    self.validate_receipt_contents(
                        target,
                        &receipt,
                        immutable,
                        &mut Verifier::default(),
                    )?;
                }
                // Transitive-only files and Cargo aliases have no current
                // ownership claim. They still require complete byte equality.
                Err(error) if error.kind() == ErrorKind::NotFound => {
                    if !same_contents(target, &self.physical(immutable)?)? {
                        bail!("delivered dependency differs from immutable input");
                    }
                }
                Err(error) => return Err(error.into()),
            }
        }
        trees.extend(source_trees);
        // Generated roots can overlap across producers. Materialization creates
        // directories exclusively, so drop a child only when the ancestor's
        // immutable image contains exactly that subtree. A workspace's empty
        // target placeholder never covers generated sources beneath it.
        let mut redundant = Vec::new();
        for (root, input) in trees.iter() {
            let ancestor = root.ancestors().skip(1).find_map(|parent| {
                trees
                    .get(parent)
                    .filter(|_| {
                        !root.starts_with(&self.config.target)
                            || parent.starts_with(&self.config.target)
                    })
                    .map(|input| (parent, input))
            });
            if let Some((parent, owner)) = ancestor {
                let subtree = owner.join(root.strip_prefix(parent)?);
                if subtree != *input
                    && store::source_nar(&self.physical(&subtree)?)?
                        != store::source_nar(&self.physical(input)?)?
                {
                    bail!("overlapping dependency source images disagree");
                }
                redundant.push(root.clone());
            }
        }
        for root in redundant {
            trees.remove(&root);
        }
        Ok(dependencies)
    }

    fn links_dependencies(args: &[OsString]) -> bool {
        let mut remaining = args.iter();
        let mut library = false;
        let mut metadata_only = false;
        while let Some(arg) = remaining.next() {
            let value = if arg == "--crate-type" {
                remaining.next().map(|value| value.as_bytes())
            } else {
                arg.as_bytes().strip_prefix(b"--crate-type=")
            };
            if let Some(types) = value {
                if types
                    .split(|byte| *byte == b',')
                    .any(|kind| kind != b"lib" && kind != b"rlib")
                {
                    return true;
                }
                library = true;
            }
            if arg == "--test" {
                return true;
            }
            if let Some(emit) = arg.as_bytes().strip_prefix(b"--emit=") {
                metadata_only = !emit.split(|byte| *byte == b',').any(|kind| kind == b"link");
            }
        }
        !library && !metadata_only
    }

    fn metadata_files(mut files: BTreeMap<PathBuf, PathBuf>) -> BTreeMap<PathBuf, PathBuf> {
        // Keep proc-macro/dynamic libraries and archives without a metadata
        // counterpart. Available sibling .rlib/.d files must not change a
        // metadata consumer's input identity depending on producer timing.
        let metadata: BTreeSet<_> = files
            .keys()
            .filter(|path| path.extension() == Some(OsStr::new("rmeta")))
            .map(|path| path.with_extension("rlib"))
            .collect();
        files.retain(|path, _| {
            path.extension() != Some(OsStr::new("d")) && !metadata.contains(path)
        });
        files
    }

    fn merge_dependencies(
        dependencies: &mut BTreeMap<PathBuf, PathBuf>,
        files: BTreeMap<PathBuf, PathBuf>,
        metadata: &mut BTreeSet<PathBuf>,
    ) -> eyre::Result<()> {
        for (target, immutable) in files {
            if target.extension() == Some(OsStr::new("rmeta")) {
                metadata.insert(target.clone());
            }
            if dependencies
                .insert(target, immutable.clone())
                .is_some_and(|old| old != immutable)
            {
                bail!("conflicting dependency owners");
            }
        }
        Ok(())
    }

    async fn metadata_owner<T: DaemonStore>(
        &self,
        daemon: &mut T,
        path: &Path,
        invocation: &Invocation,
    ) -> eyre::Result<PathBuf> {
        let bytes = fs::read(path)?;
        let candidates = self
            .config
            .inputs
            .iter()
            .chain(invocation.trees.values())
            .chain(invocation.dependencies.values())
            .chain(
                invocation
                    .incremental
                    .iter()
                    .filter_map(|state| state.input.as_ref()),
            )
            .map(|path| Self::store_path(path))
            .collect::<eyre::Result<BTreeSet<_>>>()?;
        // Retain the fixed compiler/native closure and any declared source or
        // produced input whose store hash occurs in the metadata. Unrelated
        // source revisions and sibling artifacts do not affect this owner.
        let mut references = self
            .config
            .inputs
            .iter()
            .map(|path| Self::store_path(path))
            .collect::<eyre::Result<BTreeSet<_>>>()?;
        references.extend(Self::metadata_references(&bytes, candidates));
        let owner = daemon
            .add_ca_to_store(
                "stock-cargo-metadata",
                ContentAddressMethodAlgorithm::NixArchive(SHA256),
                &references,
                false,
                store::source_nar(path)?.as_slice(),
            )
            .await?
            .path
            .to_absolute_path(&StoreDir::default());
        self.pin(daemon, &owner).await?;
        Ok(owner)
    }

    fn metadata_references(bytes: &[u8], candidates: BTreeSet<StorePath>) -> BTreeSet<StorePath> {
        let mut by_hash = BTreeMap::<Vec<u8>, BTreeSet<StorePath>>::new();
        for path in candidates {
            let absolute = path.to_absolute_path(&StoreDir::default());
            by_hash
                .entry(absolute.file_name().unwrap().as_bytes()[..32].to_vec())
                .or_default()
                .insert(path);
        }
        let alphabet = b"0123456789abcdfghijklmnpqrsvwxyz"
            .iter()
            .fold(0u128, |mask, byte| mask | (1u128 << byte));
        let mut references = BTreeSet::new();
        let mut run = 0usize;
        // Scan once, including bare hashes as Nix does. Most binary bytes end
        // a base32 run, avoiding a full metadata scan per transitive input.
        for (offset, byte) in bytes.iter().copied().enumerate() {
            run = if byte < 128 && alphabet & (1u128 << byte) != 0 {
                run + 1
            } else {
                0
            };
            if run >= 32
                && let Some(paths) = by_hash.get(&bytes[offset + 1 - 32..=offset])
            {
                references.extend(paths.iter().cloned());
            }
        }
        references
    }

    // Descriptive layout values remain compiler inputs without granting reads
    // across source roots. Explicit forwarded path inputs still declare roots.
    fn source_environment<'a>(
        environment: &'a [(Vec<u8>, Vec<u8>)],
        layout_names: &'a BTreeSet<String>,
    ) -> impl Iterator<Item = (&'a [u8], &'a [u8])> {
        environment
            .iter()
            .filter(move |(name, _)| {
                !layout_names
                    .iter()
                    .any(|layout| layout.as_bytes() == name.as_slice())
            })
            .map(|(name, value)| (name.as_slice(), value.as_slice()))
    }

    // Select whole declared roots, never compiler dep-info files: proc macros
    // can read any file within a retained root. Dependency receipts add their
    // source owners after capturing this invocation's generated inputs.
    // The prepared map stays complete for workspace-mirror capture in execute.
    fn source_trees<'a>(
        trees: &BTreeMap<PathBuf, PathBuf>,
        cwd: &Path,
        arguments: impl IntoIterator<Item = &'a [u8]>,
        environment: impl IntoIterator<Item = (&'a [u8], &'a [u8])>,
    ) -> BTreeMap<PathBuf, PathBuf> {
        let mut retained: BTreeSet<_> = trees.keys().filter(|root| cwd.starts_with(root)).collect();
        if retained.is_empty() {
            // Mirrors and generated Cargo projects are validated/captured later.
            // Without a declared owner, relative paths have no known root scope.
            return trees.clone();
        }
        // Nix's reserved builder directory is infrastructure, not a declaration
        // of all sources beneath /build. Keep its value in the actual compiler
        // environment and request identity; only source selection ignores it.
        let values = arguments.into_iter().chain(
            environment
                .into_iter()
                .filter_map(|(name, value)| (name != b"NIX_BUILD_TOP").then_some(value)),
        );
        for value in values {
            if value
                .split(|byte| *byte == b'/' || Self::source_path_boundary(*byte))
                .any(|part| part == b"." || part == b"..")
            {
                // Lexically resolving traversal can change symlink semantics.
                // Keep the layout intact when a value might escape its owner.
                return trees.clone();
            }
            // Repeated slashes are equivalent path spelling, including in values
            // containing flags or path lists. Leave all other bytes untouched.
            let value = if value.windows(2).any(|part| part == b"//") {
                let mut normalized = Vec::with_capacity(value.len());
                for byte in value {
                    if *byte != b'/' || normalized.last() != Some(&b'/') {
                        normalized.push(*byte);
                    }
                }
                Cow::Owned(normalized)
            } else {
                Cow::Borrowed(value)
            };
            retained.extend(
                trees
                    .keys()
                    .filter(|root| Self::source_reference(&value, root)),
            );
        }
        // Preparation rejects overlapping declared roots. Preserve that rule,
        // but also make selection safe for nested maps: a parent image and all
        // of its separately supplied descendants must remain together.
        loop {
            let previous = retained.len();
            for root in trees.keys() {
                if retained
                    .iter()
                    .any(|other| root.starts_with(other) || other.starts_with(root))
                {
                    retained.insert(root);
                }
            }
            if retained.len() == previous {
                break;
            }
        }
        trees
            .iter()
            .filter(|(root, _)| retained.contains(root))
            .map(|(root, input)| (root.to_path_buf(), input.to_path_buf()))
            .collect()
    }

    fn source_path_boundary(byte: u8) -> bool {
        byte.is_ascii_whitespace()
            || matches!(
                byte,
                b'=' | b':'
                    | b';'
                    | b','
                    | b'\''
                    | b'"'
                    | b'['
                    | b']'
                    | b'('
                    | b')'
                    | b'{'
                    | b'}'
                    | b'@'
                    | 0
                    | 0x1f
            )
    }

    fn source_reference(value: &[u8], root: &Path) -> bool {
        root.ancestors().any(|ancestor| {
            let path = ancestor.as_os_str().as_bytes();
            value.windows(path.len()).enumerate().any(|(offset, part)| {
                part == path
                    // A slash within another path is not a reference to `/`.
                    && (path != b"/" || offset == 0 || Self::source_path_boundary(value[offset - 1]))
                    && value.get(offset + path.len()).is_none_or(|next| {
                        (*next == b'/'
                            && (ancestor == root
                                || value.get(offset + path.len() + 1)
                                    .is_none_or(|byte| Self::source_path_boundary(*byte))))
                            || Self::source_path_boundary(*next)
                    })
            })
        })
    }

    fn invocation(
        &self,
        program: &OsStr,
        args: &[OsString],
        role: &str,
    ) -> eyre::Result<Invocation> {
        if role != "rustc-wrapper" {
            bail!(
                "Nix compiler bridge requires Cargo's compiler-wrapper boundary; only doctest rustdoc actions run natively"
            )
        }
        let (rustc, _) = Self::compiler(program, args)
            .ok_or_else(|| eyre::eyre!("unsupported Cargo compiler driver chain"))?;
        Tool::bound(&StoreDir::default(), Path::new(program))?;
        Tool::bound(&StoreDir::default(), Path::new(rustc))?;
        let cwd = std::env::current_dir()?;
        let output = CompilerOutput::from_args(args, &self.config.target)?;
        let mut incremental = incremental::State::from_args(args, &self.config.target, &output)?;
        if incremental.is_some() && !self.config.incremental {
            bail!("incremental state requires the experimental immutable owner setting");
        }
        if let Some(state) = &mut incremental {
            state.observe = self.config.incremental_observer;
        }
        let mut environment = Vec::new();
        for (name, value) in std::env::vars_os() {
            let public_name = name
                .to_str()
                .ok_or_else(|| eyre::eyre!("environment name is not declared UTF-8"))?;
            if self.config.omitted_environment_names.contains(public_name) {
                continue;
            }
            if self.config.environment_names.contains(public_name)
                || self
                    .config
                    .environment_prefixes
                    .iter()
                    .any(|prefix| public_name.starts_with(prefix))
            {
                environment.push((name.as_bytes().to_vec(), value.as_bytes().to_vec()));
            } else if !self.config.omitted_environment_names.contains(public_name) {
                bail!("compiler environment name requires an explicit policy: {public_name}")
            }
        }
        environment.sort();
        let trees = Self::source_trees(
            &self.compiler_inputs()?.trees,
            &cwd,
            args.iter().map(|value| value.as_bytes()),
            // Layout values still reach rustc and determine its request. They
            // describe the namespace without declaring access to every source
            // root at that path; actual cross-root reads need explicit inputs.
            Self::source_environment(&environment, &self.config.layout_environment_names),
        );
        Ok(Invocation {
            trees,
            source: self.config.source.clone(),
            target: self.config.target.clone(),
            cwd,
            output,
            incremental,
            jobserver: self.config.jobserver_socket_directory.is_some(),
            source_overlay: self.config.source_overlay,
            private_target: self.config.private_target,
            program: program.as_bytes().to_vec(),
            arguments: args.iter().map(|a| a.as_bytes().to_vec()).collect(),
            stdin: if args
                .iter()
                .enumerate()
                .any(|(index, arg)| arg == "-" && (index == 0 || args[index - 1] != "-o"))
            {
                let mut input = Vec::new();
                std::io::stdin().read_to_end(&mut input)?;
                Some(input)
            } else {
                None
            },
            environment,
            dependencies: BTreeMap::new(),
        })
    }

    #[tokio::main(flavor = "current_thread")]
    pub(super) async fn execute(
        &self,
        observation: &Path,
        program: &OsStr,
        args: &[OsString],
        role: &str,
    ) -> eyre::Result<ExitStatus> {
        let started = Instant::now();
        let inputs = self.compiler_inputs()?;
        Self::configured_socket(&self.config)?;
        let mut invocation = self.invocation(program, args, role)?;
        let _incremental_lock = invocation
            .incremental
            .as_mut()
            .map(|state| state.bind(self))
            .transpose()?;
        let store_dir = StoreDir::default();
        let mut daemon = harmonia_store_remote::DaemonClientBuilder::new()
            .set_store_dir(&store_dir)
            .build_unix(&self.config.socket)
            .await?
            .handshake()
            .await?;
        if !invocation.trees.keys().any(|root| {
            invocation.cwd.starts_with(root)
                && (!invocation.cwd.starts_with(&self.config.target)
                    || root.starts_with(&self.config.target))
        }) && (!invocation.cwd.starts_with(&self.config.target)
            || invocation.cwd.join("Cargo.toml").is_symlink())
        {
            Self::scratch(&invocation.cwd)?;
            let input = &inputs.trees[&self.config.source];
            let alias = WorkspaceAlias::new(
                &invocation.cwd,
                &self.config.source,
                &self.config.target,
                &self.physical(input)?,
            )
            .wrap_err("compiler cwd is not a verified workspace mirror")?;
            let input = store::add_prepared_nar(&mut daemon, &alias.nar()?)
                .await?
                .to_absolute_path(&store_dir);
            self.pin(&mut daemon, &input).await?;
            invocation.trees.insert(invocation.cwd.clone(), input);
        }
        let mut directories = Self::input_directories(args)?;
        // Nested Cargo (including trybuild) creates an actual package in target.
        // Capture that source at the compiler boundary, separately from artifacts.
        if invocation.cwd.starts_with(&self.config.target)
            && !invocation.trees.contains_key(&invocation.cwd)
        {
            let manifest =
                PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").ok_or_else(|| {
                    eyre::eyre!("generated Cargo project lacks a manifest directory")
                })?);
            Self::scratch(&manifest)?;
            if manifest == self.config.target
                || !manifest.starts_with(&self.config.target)
                || !invocation.cwd.starts_with(&manifest)
                || !manifest.join("Cargo.toml").is_file()
            {
                bail!("generated compiler cwd must belong to a Cargo project inside target");
            }
            directories.insert(manifest);
        }
        for directory in directories {
            // A workspace snapshot excludes target; its prefix alone does not
            // establish that generated input bytes were included.
            if invocation.trees.keys().any(|root| {
                directory.starts_with(root)
                    && (!directory.starts_with(&self.config.target)
                        || root.starts_with(&self.config.target))
            }) {
                continue;
            }
            if !directory.starts_with(&self.config.target) && !directory.starts_with("/nix/store") {
                bail!(
                    "library/generated input must belong to Cargo target, a declared source or Nix store"
                );
            }
            let input = Self::input_tree(&mut daemon, &directory).await?;
            self.pin(&mut daemon, &input).await?;
            invocation.trees.insert(directory, input);
        }
        // Capture this invocation's explicit generated inputs first, so a
        // dependency snapshot cannot hide their current contents.
        invocation.dependencies = self.dependencies(args, &mut invocation.trees)?;
        let request = serde_json::to_vec(&invocation)?;
        let request_key = Self::digest(&request);
        let mut references = self
            .config
            .inputs
            .iter()
            .chain([&inputs.builder_store])
            .chain(invocation.trees.values())
            .map(|p| Self::store_path(p))
            .collect::<eyre::Result<BTreeSet<_>>>()?;
        references.insert(Self::store_path(Path::new(program))?);
        let (rustc, _) = Self::compiler(program, args)
            .ok_or_else(|| eyre::eyre!("unsupported Cargo compiler driver chain"))?;
        references.insert(Self::store_path(Path::new(rustc))?);
        for path in invocation.dependencies.values() {
            references.insert(Self::store_path(path)?);
        }
        if let Some(input) = invocation
            .incremental
            .as_ref()
            .and_then(|state| state.input.as_ref())
        {
            references.insert(Self::store_path(input)?);
        }
        let request_path = daemon
            .add_ca_to_store(
                "stock-cargo-invocation.json",
                ContentAddressMethodAlgorithm::Text,
                &references,
                false,
                request.as_slice(),
            )
            .await?
            .path;
        references.insert(request_path.clone());
        self.pin(&mut daemon, &request_path.to_absolute_path(&store_dir))
            .await?;
        let name = format!("stock-cargo-{}", &request_key[..24]);
        let drv = Derivation {
            name: StorePathName::from_str(&name)?,
            outputs: BTreeMap::from([(
                store::OUTPUT_OUT.clone(),
                DerivationOutput::CAFloating(ContentAddressMethodAlgorithm::NixArchive(SHA256)),
            )]),
            inputs: references
                .into_iter()
                .map(SingleDerivedPath::Opaque)
                .collect(),
            platform: store::PLATFORM.clone(),
            builder: inputs.builder_store.to_path_buf().into_bytes(),
            args: vec![
                "stock-compile".into(),
                request_path.to_absolute_path(&store_dir).into_bytes(),
            ],
            env: BTreeMap::from([(
                "out".into(),
                Placeholder::standard_output(&store::OUTPUT_OUT).into_bytes(),
            )]),
            structured_attrs: None,
        };
        let drv_path = store::add_drv_to_store(&mut daemon, &store_dir, drv, &name)
            .await?
            .to_absolute_path(&store_dir);
        self.pin(&mut daemon, &drv_path).await?;
        let submitted = started.elapsed().as_secs_f64();
        let mut command = Command::new(&self.config.nix);
        command
            .args(["build", "--offline", "--store"])
            .arg(format!("unix://{}", self.config.socket.display()))
            .args([
                "--option",
                "substitute",
                "false",
                "--json",
                "--log-format",
                "internal-json",
                "--print-build-logs",
            ])
            .arg(format!("{}^out", drv_path.display()));
        if let Some(directory) = &self.config.gc_roots {
            command
                .arg("--out-link")
                .arg(directory.join(format!("result-{request_key}")));
        } else {
            command.arg("--no-link");
        }
        let mut log = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(observation.with_extension("nix-stderr"))?;
        let jobserver = self
            .config
            .jobserver_socket_directory
            .as_ref()
            .map(|directory| jobserver::Broker::from_cargo(directory, &request_key))
            .transpose()?;
        let mut process = CapturedProcess::spawn(&mut command, None)?;
        let mut metadata = metadata::Receiver::default();
        let mut early = BTreeMap::new();
        let mut previous_objects = BTreeMap::new();
        let mut notifications = BTreeSet::new();
        let mut metadata_delivery_seconds = 0.0;
        let mut metadata_bytes = 0;
        let mut metadata_ready = Vec::new();
        let streamed = async {
            loop {
                let line = process.line()?;
                if line.is_empty() {
                    break;
                }
                log.write_all(line)?;
                let Some(event) = line.strip_prefix(b"@nix ") else {
                    continue;
                };
                let event: serde_json::Value = serde_json::from_slice(event)?;
                if let Some(fields) = event["fields"].as_array() {
                    for field in fields {
                        let Some(frame) = field
                            .as_str()
                            .and_then(|line| line.strip_prefix(metadata::PREFIX))
                        else {
                            continue;
                        };
                        if let Some(artifact) = metadata.accept(frame, &invocation.output)? {
                            previous_objects.extend(self.previous_objects(&artifact.path)?);
                            let delivery = Instant::now();
                            let owner = self
                                .deliver_metadata(&mut daemon, &invocation, &artifact)
                                .await?;
                            metadata_delivery_seconds += delivery.elapsed().as_secs_f64();
                            metadata_bytes += artifact.bytes.len();
                            metadata_ready.push(serde_json::json!({
                                "path":artifact.path, "bytes":artifact.bytes.len(),
                                "offsetSeconds":started.elapsed().as_secs_f64(),
                                "monotonicSeconds":Self::monotonic_seconds()?
                            }));
                            early.insert(artifact.path, owner);
                            notifications.insert(artifact.notification.clone());
                            // Bytes and their immutable ownership are visible
                            // before stock Cargo receives metadata readiness.
                            let mut stderr = std::io::stderr().lock();
                            stderr.write_all(&artifact.notification)?;
                            stderr.flush()?;
                        }
                    }
                }
            }
            metadata.finish()
        }
        .await;
        streamed?;
        let built = process.finish()?;
        fs::write(observation.with_extension("nix-stdout"), &built.stdout)?;
        if !built.status.success() {
            std::io::stderr().write_all(&built.stderr)?;
            bail!(
                "Nix compiler build failed with {}; no artifacts delivered",
                built.status
            )
        }
        let jobserver_connected = jobserver.map(jobserver::Broker::finish).transpose()?;
        let values: serde_json::Value = serde_json::from_slice(&built.stdout)?;
        let output = PathBuf::from(
            values[0]["outputs"]["out"]
                .as_str()
                .ok_or_else(|| eyre::eyre!("Nix did not return one compiler output"))?,
        );
        let physical = self.physical(&output)?;
        let raw_status: i32 = serde_json::from_slice(&fs::read(physical.join("status.json"))?)?;
        let status = ExitStatus::from_raw(raw_status);
        let build_finished = started.elapsed().as_secs_f64();
        if let Some(directory) = invocation.output.directory()
            && directory.canonicalize()? != directory
        {
            bail!("compiler output directory must not traverse symlinks");
        }
        let mut files = invocation.dependencies.clone();
        let mut owned = Vec::new();
        // Validate every replacement before changing any target artifact. Cargo
        // may already have removed stale files, but unrelated files are not ours.
        for entry in fs::read_dir(physical.join("artifacts"))? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                bail!("compiler output must contain regular artifacts");
            }
            let directory = invocation
                .output
                .directory()
                .ok_or_else(|| eyre::eyre!("stdout-only compiler returned file artifacts"))?;
            let target = directory.join(entry.file_name());
            if let CompilerOutput::File(expected) = &invocation.output
                && &target != expected
            {
                bail!("explicit compiler output contains an unexpected artifact");
            }
            self.validate_replacement(&target)?;
            previous_objects.extend(self.previous_objects(&target)?);
            let input = if target.extension() == Some(OsStr::new("rmeta")) {
                if let Some(owner) = early.get(&target) {
                    if !same_contents(&entry.path(), &self.physical(owner)?)? {
                        bail!("compiler changed metadata after publishing it");
                    }
                    owner.clone()
                } else {
                    let delivery = Instant::now();
                    let owner = self
                        .metadata_owner(&mut daemon, &entry.path(), &invocation)
                        .await?;
                    metadata_delivery_seconds += delivery.elapsed().as_secs_f64();
                    metadata_bytes += entry.metadata()?.len() as usize;
                    owner
                }
            } else {
                output.join("artifacts").join(entry.file_name())
            };
            files.insert(target.clone(), input);
            owned.push((entry.path(), target));
        }
        if early
            .keys()
            .any(|target| !owned.iter().any(|(_, path)| path == target))
        {
            bail!("complete compiler result omitted previously delivered metadata");
        }
        let owned_paths: BTreeSet<_> = owned.iter().map(|(_, target)| target.clone()).collect();
        let mut stale_objects = Vec::new();
        if status.success() {
            // A failed rustc can leave codegen objects that a successful native
            // retry removes while archiving. The fresh Nix builder cannot see
            // those old files. Reconcile only that compiler's owned temporary
            // objects, retaining any it still emits (including save-temps).
            for (target, owner) in previous_objects {
                if owned_paths.contains(&target) {
                    continue;
                }
                if target.parent() != invocation.output.directory() {
                    bail!("previous compiler object escapes its output directory");
                }
                self.validate_replacement(&target)?;
                if self.delivered(&target)?.files[&target] != owner {
                    bail!("previous compiler object changed ownership");
                }
                stale_objects.push(target);
            }
        }
        let mut delivered = Delivered {
            version: 6,
            compiler_status: Some(raw_status),
            owned: owned_paths
                .into_iter()
                .map(|path| (path, Representation::Copy))
                .collect(),
            files,
            trees: invocation.trees.clone(),
        };
        delivered.select_representations(&self.config, &invocation.output);
        if delivered
            .owned
            .values()
            .any(|value| *value == Representation::StoreLink)
        {
            // Result out-links already retain this package; also use the normal
            // durable pin lifecycle before exposing any backing through a link.
            self.pin(&mut daemon, &output).await?;
            for (target, representation) in &delivered.owned {
                if *representation == Representation::StoreLink {
                    self.validate_store_backing(&delivered.files[target])?;
                }
            }
        }
        for (input, target) in &owned {
            if target.extension() == Some(OsStr::new("rmeta"))
                && (early.contains_key(target)
                    || delivered.owned.get(&target.with_extension("rlib"))
                        != Some(&Representation::StoreLink))
                && target.try_exists()?
                && same_contents(input, target)?
            {
                // Early metadata may already have been consumed. Replacing
                // identical bytes would change mtime and dirty those consumers.
                // Without early delivery, a linked archive needs fresh metadata
                // here instead: neither output otherwise records this rebuild.
                continue;
            }
            delivered.owned[target].publish(input, target, &delivered.files[target])?;
        }
        for target in stale_objects {
            match fs::remove_file(&target) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            fs::remove_file(self.receipt(&target))?;
        }
        // Every advertised file must exist before any complete receipt is
        // visible to a consumer refreshing a transitive link closure.
        for (_, target) in &owned {
            self.publish_receipt(target, &delivered)?;
        }
        if let Some(state) = &invocation.incremental {
            state.deliver(self, &output)?;
        }
        let delivery_finished = started.elapsed().as_secs_f64();
        Self::write_json(
            &observation.with_extension("nix-result.json"),
            &serde_json::json!({
                "derivation": drv_path, "output": output, "submitSeconds": submitted,
                "buildSeconds": build_finished - submitted, "deliverySeconds": delivery_finished - build_finished,
                "secondsBeforeFinalCompilerMessages": delivery_finished,
                "metadataDeliverySeconds": metadata_delivery_seconds,
                "metadataBytes": metadata_bytes,
                "earlyMetadataFiles": early.len(),
                "metadataReady": metadata_ready,
                "compilerStatus": raw_status,
                "compilerSucceeded": status.success(),
                "incremental": invocation.incremental,
                "jobserverPolicy": if invocation.jobserver { "Cargo slot held; original Cargo pipe through a private same-user broker" } else { "Cargo slot held; builder-local rustc without inherited jobserver FDs" },
                "jobserverConnected": jobserver_connected,
                "pipelining": "metadata readiness follows verified early delivery; other artifacts follow compiler completion"
            }),
        )?;
        std::io::stdout().write_all(&fs::read(physical.join("stdout"))?)?;
        let stderr = fs::read(physical.join("stderr"))?;
        for line in stderr.split_inclusive(|byte| *byte == b'\n') {
            if !notifications.remove(line) {
                std::io::stderr().write_all(line)?;
            }
        }
        if !notifications.is_empty() {
            bail!("complete compiler result omitted an early notification")
        }
        Ok(status)
    }

    fn children_cpu() -> std::io::Result<(f64, f64)> {
        let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
        // SAFETY: getrusage writes a complete rusage on success; the allocation
        // remains live and correctly aligned throughout the call.
        if unsafe { libc::getrusage(libc::RUSAGE_CHILDREN, usage.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: the successful call above initialized the complete value.
        let usage = unsafe { usage.assume_init() };
        let seconds = |value: libc::timeval| value.tv_sec as f64 + value.tv_usec as f64 / 1e6;
        Ok((seconds(usage.ru_utime), seconds(usage.ru_stime)))
    }

    fn monotonic_seconds() -> std::io::Result<f64> {
        let mut time = std::mem::MaybeUninit::<libc::timespec>::uninit();
        // SAFETY: a successful clock_gettime initializes the live, aligned timespec.
        if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, time.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: the call above succeeded.
        let time = unsafe { time.assume_init() };
        Ok(time.tv_sec as f64 + time.tv_nsec as f64 / 1e9)
    }

    fn compiler_output(
        command: &mut Command,
        input: Option<Vec<u8>>,
        mut observe: impl FnMut(&[u8]) -> std::io::Result<()>,
    ) -> std::io::Result<std::process::Output> {
        // Drain compiler output while feeding input: neither pipe may block
        // the other. A compiler rejecting its options may close stdin early.
        let mut process = CapturedProcess::spawn(command, input)?;
        loop {
            let line = process.line()?;
            if line.is_empty() {
                break;
            }
            observe(line)?;
        }
        process.finish()
    }

    pub(super) fn build(arguments: &[OsString]) -> eyre::Result<()> {
        let [request] = arguments else {
            bail!("stock-compile requires one immutable invocation")
        };
        Self::store_path(Path::new(request))?;
        if std::env::current_dir()? != Path::new("/build") {
            bail!("stock-compile requires a private Nix /build")
        }
        let output = PathBuf::from(
            std::env::var_os("out").ok_or_else(|| eyre::eyre!("missing Nix output"))?,
        );
        Self::store_path(&output)?;
        let request_bytes = fs::read(request)?;
        let mut invocation: Invocation = serde_json::from_slice(&request_bytes)?;
        if invocation.private_target && !invocation.source_overlay {
            bail!("private target storage requires source overlays");
        }
        Self::scratch(&invocation.source)?;
        Self::scratch(&invocation.target)?;
        invocation.output.validate(&invocation.target)?;
        let arguments: Vec<_> = invocation
            .arguments
            .iter()
            .map(|a| OsString::from_vec(a.clone()))
            .collect();
        let expected_incremental =
            incremental::State::from_args(&arguments, &invocation.target, &invocation.output)?;
        if expected_incremental
            .as_ref()
            .map(|state| (&state.directory, &state.unit))
            != invocation
                .incremental
                .as_ref()
                .map(|state| (&state.directory, &state.unit))
        {
            bail!("incremental declaration does not match compiler arguments");
        }
        if !invocation
            .trees
            .keys()
            .any(|source| invocation.cwd.starts_with(source))
            || invocation.source.starts_with(&invocation.target)
        {
            bail!("invalid invocation layout")
        }
        let input_started = Instant::now();
        let mut materializer = if invocation.source_overlay {
            source_wrap::Materializer::overlay(Path::new("/build/.dyndrv-source-overlays"))?
        } else {
            source_wrap::Materializer::Copy
        };
        for (destination, input) in &invocation.trees {
            Self::store_path(input)?;
            if destination != input {
                Self::scratch(destination)?;
                fs::create_dir_all(destination.parent().unwrap())?;
                materializer.materialize(input, destination, &invocation.writable(destination))?;
            }
        }
        let trees_ready = input_started.elapsed().as_secs_f64();
        if let Some(state) = &invocation.incremental {
            if invocation
                .dependencies
                .keys()
                .any(|path| path.starts_with(&state.directory))
            {
                bail!("incremental state overlaps compiler dependencies");
            }
            state.restore()?;
        }
        let products = invocation
            .incremental
            .as_ref()
            .filter(|state| state.observe)
            .map(|state| incremental::Products::capture(&state.directory))
            .transpose()?;
        let incremental_ready = input_started.elapsed().as_secs_f64();
        if let Some(directory) = invocation.output.directory() {
            fs::create_dir_all(directory)?;
        }
        for (target, input) in &invocation.dependencies {
            if !target.starts_with(&invocation.target) {
                bail!("dependency destination escapes target")
            }
            Self::store_path(input)?;
            fs::create_dir_all(target.parent().unwrap())?;
            if target.symlink_metadata().is_ok() {
                bail!("duplicate dependency destination")
            }
            fs::copy(input, target)?;
        }
        eprintln!(
            "DYNDRV_STOCK_INPUTS_READY {}",
            serde_json::json!({
                "treeSeconds": trees_ready,
                "incrementalSeconds": incremental_ready - trees_ready,
                "incremental": invocation.incremental,
                "dependencySeconds": input_started.elapsed().as_secs_f64() - incremental_ready,
                "trees": invocation.trees,
                "dependencyFiles": invocation.dependencies.len()
            })
        );
        let program = OsStr::from_bytes(&invocation.program);
        Self::store_path(Path::new(&program))?;
        let env = invocation
            .environment
            .iter()
            .map(|(k, v)| (OsStr::from_bytes(k), OsStr::from_bytes(v)));
        let mut command = Command::new(program);
        command
            .args(invocation.arguments.iter().map(|a| OsStr::from_bytes(a)))
            .env_clear()
            .envs(env)
            .current_dir(&invocation.cwd)
            .stdin(Stdio::null());
        let jobserver = invocation
            .jobserver
            .then(|| jobserver::Pipe::receive(&Self::digest(&request_bytes)))
            .transpose()?;
        if let Some(pipe) = &jobserver {
            pipe.configure(&mut command);
        }
        eprintln!(
            "DYNDRV_STOCK_COMPILER_START {}",
            serde_json::json!({
                "monotonicSeconds": Self::monotonic_seconds()?,
                "program": command.get_program().as_bytes(),
                "arguments": command.get_args().map(OsStrExt::as_bytes).collect::<Vec<_>>(),
                "cwd": command.get_current_dir(),
                "environmentNames": command.get_envs().map(|(name, _)| name.as_bytes()).collect::<Vec<_>>()
            })
        );
        // This builder reaps one compiler. Its waited-child CPU includes
        // descendants the compiler reaped (such as linkers), but not this
        // builder's staging, validation, publication or Nix queue time.
        let (user_before, system_before) = Self::children_cpu()?;
        let compiler_started = Instant::now();
        let result = Self::compiler_output(&mut command, invocation.stdin.take(), |line| {
            metadata::Artifact::emit(line, &invocation.output, &mut std::io::stderr().lock())
                .map_err(|error| std::io::Error::other(error.to_string()))
        })?;
        let compiler_seconds = compiler_started.elapsed().as_secs_f64();
        let (user_after, system_after) = Self::children_cpu()?;
        eprintln!(
            "DYNDRV_STOCK_COMPILER_FINISH {}",
            serde_json::json!({
                "monotonicSeconds": Self::monotonic_seconds()?,
                "seconds": compiler_seconds,
                "userSeconds": user_after - user_before,
                "systemSeconds": system_after - system_before,
                "cpuAccounting": "waited compiler and its reaped descendants; excludes builder and Nix service",
                "exitCode": result.status.code()
            })
        );
        let publication_started = Instant::now();
        if let Some(products) = products {
            products.report(&invocation.incremental.as_ref().unwrap().directory)?;
        }
        let source_verification_started = Instant::now();
        let mut verifier = Verifier::default();
        for (destination, input) in &invocation.trees {
            if destination != input {
                verifier.verify_unchanged(input, destination, &invocation.writable(destination))?;
            }
        }
        let source_verification_seconds = source_verification_started.elapsed().as_secs_f64();
        let dependency_verification_started = Instant::now();
        for (target, input) in &invocation.dependencies {
            if !verifier.same_contents(target, input)? {
                bail!("compiler changed dependency input")
            }
        }
        let dependency_verification_seconds =
            dependency_verification_started.elapsed().as_secs_f64();
        let artifact_publication_started = Instant::now();
        // Nix may mount /tmp and /build on different filesystems. Keep the
        // hard-link staging inside the writable output, including when its
        // parent is a read-only generated input tree.
        let stage = invocation
            .output
            .directory()
            .unwrap_or(Path::new("/build"))
            .join(".dyndrv-result");
        fs::create_dir(&stage)?;
        let artifacts = match &invocation.output {
            CompilerOutput::Directory(directory) => fs::read_dir(directory)?
                .map(|entry| entry.map(|entry| entry.path()))
                .collect::<std::io::Result<Vec<_>>>()?,
            CompilerOutput::File(path) if path.try_exists()? => vec![path.clone()],
            CompilerOutput::File(_) | CompilerOutput::Stdout => Vec::new(),
        };
        for artifact in artifacts {
            if artifact != stage && !invocation.dependencies.contains_key(&artifact) {
                if !artifact.symlink_metadata()?.is_file() {
                    bail!("unsupported compiler output directory or special file")
                }
                fs::hard_link(&artifact, stage.join(artifact.file_name().unwrap()))?;
            }
        }
        fs::create_dir(&output)?;
        compile_wrap::publish(&stage, &output.join("artifacts"))?;
        let artifact_publication_seconds = artifact_publication_started.elapsed().as_secs_f64();
        let incremental_publication_started = Instant::now();
        if let Some(state) = &invocation.incremental {
            state.publish(&output)?;
        }
        let incremental_publication_seconds =
            incremental_publication_started.elapsed().as_secs_f64();
        fs::write(output.join("stdout"), result.stdout)?;
        fs::write(output.join("stderr"), result.stderr)?;
        // A failed compiler result is data, not a failed Nix transport. Cargo
        // and build scripts receive the actual status after artifact delivery.
        Self::write_json(&output.join("status.json"), &result.status.into_raw())?;
        eprintln!(
            "DYNDRV_STOCK_OUTPUT_PUBLISHED {}",
            serde_json::json!({
                "verificationAndPublicationSeconds": publication_started.elapsed().as_secs_f64(),
                "sourceVerificationSeconds": source_verification_seconds,
                "dependencyVerificationSeconds": dependency_verification_seconds,
                "artifactPublicationSeconds": artifact_publication_seconds,
                "incrementalPublicationSeconds": incremental_publication_seconds
            })
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Completion, Session};
    use super::*;
    use std::env;
    use std::fs::{DirBuilder, FileTimes};
    use std::os::unix::fs::DirBuilderExt;
    use std::process;
    use std::thread;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    struct ArtifactFixture {
        root: PathBuf,
        prepared: Backend,
    }

    impl ArtifactFixture {
        fn new() -> Self {
            let root = env::temp_dir().join(format!(
                "dyndrv-artifacts-{}-{}",
                process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            let target = root.join("target");
            fs::create_dir_all(target.join(".dyndrv-delivered")).unwrap();
            let config = serde_json::from_value(serde_json::json!({
                "nix": "", "socket": "", "real_store": root.join("store"),
                "source": root.join("source"), "target": target,
                "inputs": [], "environment_names": [], "omitted_environment_names": []
            }))
            .unwrap();
            Self {
                root,
                prepared: Backend {
                    config,
                    compiler_inputs: None,
                },
            }
        }

        fn receipt(&self, directory: &str) -> Delivered {
            let mut receipt = Delivered {
                version: 6,
                compiler_status: Some(0),
                owned: BTreeMap::new(),
                files: BTreeMap::new(),
                trees: BTreeMap::new(),
            };
            for name in [
                "libfixture.rlib",
                "libfixture.rmeta",
                "libfixture.so",
                "fixture.d",
                "fixture",
                "fixture.rcgu.o",
            ] {
                let target = self.prepared.config.target.join(directory).join(name);
                let input =
                    Path::new("/nix/store/00000000000000000000000000000000-fixture").join(name);
                let physical = self.prepared.physical(&input).unwrap();
                fs::create_dir_all(physical.parent().unwrap()).unwrap();
                fs::create_dir_all(target.parent().unwrap()).unwrap();
                fs::write(&physical, name).unwrap();
                fs::write(&target, name).unwrap();
                receipt.owned.insert(target.clone(), Representation::Copy);
                receipt.files.insert(target, input);
            }
            receipt
        }
    }

    impl Drop for ArtifactFixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.root).unwrap();
        }
    }

    #[test]
    fn session_preparation_child() {
        let Some(session) = env::var_os("DYNDRV_TEST_PREPARATION_SESSION") else {
            return;
        };
        let session = Path::new(&session);
        let inputs = CompilerInputs::in_session(session, || {
            assert!(!session.join("compiler-inputs.json").exists());
            // Exclusive evidence makes a second initializer fail the test,
            // even if both would otherwise publish equal compiler inputs.
            Backend::write_json(&session.join("capture-owner.json"), &process::id())?;
            if env::var_os("DYNDRV_TEST_PREPARATION_EXIT").is_some() {
                process::exit(23); // Simulate interruption without running Rust destructors.
            }
            thread::sleep(Duration::from_millis(40));
            Ok(serde_json::from_slice(&fs::read(
                session.join("requested.json"),
            )?)?)
        })
        .unwrap();
        assert_eq!(
            serde_json::to_vec(&inputs).unwrap(),
            fs::read(session.join("expected.json")).unwrap()
        );
    }

    #[test]
    fn concurrent_compiler_processes_share_one_complete_preparation() {
        let fixture = ArtifactFixture::new();
        let session = fixture.root.join("session");
        DirBuilder::new().mode(0o700).create(&session).unwrap();
        let inputs = CompilerInputs {
            trees: BTreeMap::from([(
                fixture.prepared.config.source.to_path_buf(),
                PathBuf::from("/nix/store/00000000000000000000000000000000-source"),
            )]),
            builder_store: PathBuf::from(env!("BUILD_WRAP")),
        };
        Backend::write_json(&session.join("requested.json"), &inputs).unwrap();
        fs::write(
            session.join("expected.json"),
            serde_json::to_vec(&inputs).unwrap(),
        )
        .unwrap();
        let children: Vec<_> = (0..4)
            .map(|_| {
                Command::new(env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "stock_cargo::nix::tests::session_preparation_child",
                        "--nocapture",
                    ])
                    .env("DYNDRV_TEST_PREPARATION_SESSION", &session)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .unwrap()
            })
            .collect();
        for child in children {
            let result = child.wait_with_output().unwrap();
            assert!(
                result.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&result.stdout),
                String::from_utf8_lossy(&result.stderr)
            );
        }
        assert!(session.join("capture-owner.json").is_file());
        let published: CompilerInputs =
            serde_json::from_slice(&fs::read(session.join("compiler-inputs.json")).unwrap())
                .unwrap();
        assert_eq!(published, inputs);
        assert!(!session.join("compiler-inputs.tmp").exists());
    }

    #[test]
    fn failed_or_unpublished_initialization_is_never_retried() {
        let fixture = ArtifactFixture::new();
        for failure in ["capture", "publication"] {
            let session = fixture.root.join(failure);
            DirBuilder::new().mode(0o700).create(&session).unwrap();
            if failure == "publication" {
                fs::write(
                    session.join("compiler-inputs.tmp"),
                    b"unfinished publication",
                )
                .unwrap();
            }
            assert!(
                CompilerInputs::in_session(&session, || {
                    if failure == "capture" {
                        bail!("injected source capture failure");
                    }
                    Ok(CompilerInputs {
                        trees: BTreeMap::new(),
                        builder_store: PathBuf::from(env!("BUILD_WRAP")),
                    })
                })
                .is_err()
            );
            assert!(!session.join("compiler-inputs.json").exists());
            assert!(
                CompilerInputs::in_session(&session, || panic!("must not prepare again")).is_err()
            );
            if failure == "publication" {
                assert_eq!(
                    fs::read(session.join("compiler-inputs.tmp")).unwrap(),
                    b"unfinished publication"
                );
            }
        }
    }

    #[test]
    fn interrupted_initializer_releases_the_lock_but_never_recaptures() {
        let fixture = ArtifactFixture::new();
        let session = fixture.root.join("session");
        DirBuilder::new().mode(0o700).create(&session).unwrap();
        let result = Command::new(env::current_exe().unwrap())
            .args([
                "--exact",
                "stock_cargo::nix::tests::session_preparation_child",
            ])
            .env("DYNDRV_TEST_PREPARATION_SESSION", &session)
            .env("DYNDRV_TEST_PREPARATION_EXIT", "1")
            .output()
            .unwrap();
        assert_eq!(result.status.code(), Some(23));
        assert!(session.join("capture-owner.json").is_file());
        assert!(!session.join("compiler-inputs.json").exists());
        assert!(
            CompilerInputs::in_session(&session, || panic!(
                "must not recapture interrupted session"
            ))
            .is_err()
        );
    }

    #[test]
    fn preparation_rejects_symlinked_shared_or_nonregular_lock_files() {
        let fixture = ArtifactFixture::new();
        let sentinel = fixture.root.join("sentinel");
        fs::write(&sentinel, b"untouched").unwrap();
        for kind in ["symlink", "directory", "hardlink", "public"] {
            let session = fixture.root.join(kind);
            DirBuilder::new().mode(0o700).create(&session).unwrap();
            let lock = session.join("prepare.lock");
            match kind {
                "symlink" => unix_fs::symlink(&sentinel, &lock).unwrap(),
                "directory" => fs::create_dir(&lock).unwrap(),
                "hardlink" => fs::hard_link(&sentinel, &lock).unwrap(),
                "public" => {
                    fs::write(&lock, b"").unwrap();
                    fs::set_permissions(&lock, Permissions::from_mode(0o666)).unwrap();
                }
                _ => unreachable!(),
            }
            assert!(
                CompilerInputs::in_session(&session, || panic!("unsafe lock accepted")).is_err()
            );
        }
        assert_eq!(fs::read(sentinel).unwrap(), b"untouched");
    }

    #[test]
    fn lazy_source_view_freezes_all_roots_at_first_compiler_and_excludes_out_dir() {
        let fixture = ArtifactFixture::new();
        let session = fixture.root.join("session");
        DirBuilder::new().mode(0o700).create(&session).unwrap();
        let source = fixture.root.join("source");
        let package = fixture.root.join("package");
        let target = source.join("target");
        fs::create_dir_all(target.join("build/out")).unwrap();
        fs::create_dir(&package).unwrap();
        let source_file = source.join("input.txt");
        fs::write(&source_file, b"staged original").unwrap();
        fs::write(package.join("asset.txt"), b"current macro asset").unwrap();
        let times =
            FileTimes::new().set_modified(source_file.metadata().unwrap().modified().unwrap());
        // Native writes outside OUT_DIR are unsupported and timing-dependent:
        // a cached script can write before capture; later writes do not recapture.
        fs::write(&source_file, b"before compiler").unwrap();
        File::options()
            .write(true)
            .open(&source_file)
            .unwrap()
            .set_times(times)
            .unwrap();
        fs::write(
            target.join("build/out/generated"),
            b"generated before compiler",
        )
        .unwrap();
        let inputs = CompilerInputs::in_session(&session, || {
            let trees = [&source, &package]
                .into_iter()
                .map(|root| {
                    let nar = WorkspaceSource::new(root, root, &target)?.nar()?;
                    Ok((root.to_path_buf(), PathBuf::from(Backend::digest(&nar))))
                })
                .collect::<eyre::Result<_>>()?;
            Ok(CompilerInputs {
                trees,
                builder_store: PathBuf::from(env!("BUILD_WRAP")),
            })
        })
        .unwrap();
        assert_eq!(inputs.trees.len(), 2);
        let source_digest = |root: &Path| {
            PathBuf::from(Backend::digest(
                &WorkspaceSource::new(root, root, &target)
                    .unwrap()
                    .nar()
                    .unwrap(),
            ))
        };
        assert_eq!(inputs.trees[&source], source_digest(&source));
        assert_eq!(inputs.trees[&package], source_digest(&package));
        fs::write(target.join("build/out/generated"), b"new generated bytes").unwrap();
        assert_eq!(
            inputs.trees[&source],
            source_digest(&source),
            "OUT_DIR is captured separately per invocation"
        );
        fs::write(&source_file, b"after compiler!").unwrap();
        assert_ne!(inputs.trees[&source], source_digest(&source));
        assert_eq!(
            inputs,
            CompilerInputs::in_session(&session, || panic!("source view must stay fixed")).unwrap()
        );
    }

    #[test]
    fn explicit_snapshot_audits_existing_artifacts_without_prepared_compiler_inputs() {
        let fixture = ArtifactFixture::new();
        let receipt = fixture.receipt("debug/deps");
        for target in receipt.owned.keys() {
            fixture.prepared.publish_receipt(target, &receipt).unwrap();
        }
        assert!(fixture.prepared.compiler_inputs.is_none());
        let root = fixture.root.join("session");
        fs::create_dir(&root).unwrap();
        Backend::write_json(
            &root.join("session.json"),
            &serde_json::json!({
                "version": 2, "rustdoc": [], "nix": fixture.prepared,
            }),
        )
        .unwrap();
        Backend::write_json(
            &root.join("cargo-result.json"),
            &Completion {
                version: 2,
                status: 0,
                nix_invocations: 0,
            },
        )
        .unwrap();
        let output = root.join("references.json");
        let arguments = [
            "--directory".into(),
            root.to_path_buf().into_os_string(),
            "--output".into(),
            output.to_path_buf().into_os_string(),
        ];
        Session::snapshot_references(&arguments).unwrap();
        let references: serde_json::Value =
            serde_json::from_slice(&fs::read(&output).unwrap()).unwrap();
        assert_eq!(references["ownedFiles"], receipt.owned.len());
        assert_eq!(references["storePaths"].as_array().unwrap().len(), 1);
        assert!(!root.join("compiler-inputs.json").exists());
        assert!(!root.join("prepare-started.json").exists());
        fs::remove_file(&output).unwrap();
        fs::write(receipt.owned.keys().next().unwrap(), b"corrupt target").unwrap();
        assert!(Session::snapshot_references(&arguments).is_err());
        assert!(!output.exists());
    }

    #[test]
    fn artifact_delivery_defaults_and_receipts_have_explicit_representations() {
        let mut fixture = ArtifactFixture::new();
        let config = &mut fixture.prepared.config;
        assert_eq!(config.artifact_delivery, ArtifactDelivery::Copy);
        let mut encoded = serde_json::to_value(&*config).unwrap();
        assert_eq!(encoded["artifact_delivery"], "copy");
        encoded["artifact_delivery"] = "system-store-links".into();
        assert!(serde_json::from_value::<Config>(encoded).is_err());
        config.artifact_delivery = ArtifactDelivery::SystemStoreRlibLinks;
        assert!(Backend::configured_socket(config).is_err());
        // Snapshot reload must rerun the live gate even with no owned links.
        assert!(fixture.prepared.snapshot_references().is_err());
        fixture.prepared.config.store_mode = StoreMode::System;
        let mut receipt = fixture.receipt("debug/deps");
        let target = fixture
            .prepared
            .config
            .target
            .join("debug/deps/libfixture.rlib");
        receipt
            .owned
            .insert(target.clone(), Representation::StoreLink);
        receipt.validate(&target, &fixture.prepared.config).unwrap();
        let encoded = serde_json::to_value(&receipt).unwrap();
        assert_eq!(encoded["version"], 6);
        assert_eq!(encoded["owned"][target.to_str().unwrap()], "store-link");
        assert_eq!(
            encoded["owned"][target.with_extension("rmeta").to_str().unwrap()],
            "copy"
        );
        let mut invalid = encoded;
        invalid["owned"][target.to_str().unwrap()] = "symlink".into();
        assert!(serde_json::from_value::<Delivered>(invalid).is_err());
        for status in [None, Some(1 << 8)] {
            receipt.compiler_status = status;
            assert!(receipt.validate(&target, &fixture.prepared.config).is_err());
        }
        receipt.compiler_status = Some(0);
        receipt.version = 5;
        assert!(receipt.validate(&target, &fixture.prepared.config).is_err());
        receipt.version = 6;
        assert!(
            receipt
                .validate(&target.with_extension("unknown"), &fixture.prepared.config)
                .is_err()
        );
        receipt.files.remove(&target.with_extension("rmeta"));
        assert!(receipt.validate(&target, &fixture.prepared.config).is_err());
    }

    #[test]
    fn store_link_selection_requires_completed_ordinary_rlib_and_owned_copied_metadata() {
        let mut fixture = ArtifactFixture::new();
        fixture.prepared.config.artifact_delivery = ArtifactDelivery::SystemStoreRlibLinks;
        fixture.prepared.config.store_mode = StoreMode::System;
        for directory in [
            "debug/deps",
            "x86_64-unknown-linux-gnu/release/deps",
            "deps",
            "debug/build/probe/out",
            "nested/target/debug/deps",
        ] {
            let mut receipt = fixture.receipt(directory);
            let output = CompilerOutput::Directory(fixture.prepared.config.target.join(directory));
            receipt.select_representations(&fixture.prepared.config, &output);
            let eligible = matches!(
                directory,
                "debug/deps" | "x86_64-unknown-linux-gnu/release/deps"
            );
            for (path, representation) in &receipt.owned {
                assert_eq!(
                    *representation == Representation::StoreLink,
                    eligible && path.extension() == Some(OsStr::new("rlib")),
                    "{}",
                    path.display()
                );
            }
        }
        let directory = fixture.prepared.config.target.join("debug/deps");
        let target = directory.join("libfixture.rlib");
        for output in [CompilerOutput::File(target.clone()), CompilerOutput::Stdout] {
            let mut receipt = fixture.receipt("debug/deps");
            receipt.select_representations(&fixture.prepared.config, &output);
            assert_eq!(receipt.owned[&target], Representation::Copy);
        }
        let output = CompilerOutput::Directory(directory.clone());
        for status in [None, Some(1 << 8)] {
            let mut receipt = fixture.receipt("debug/deps");
            receipt.compiler_status = status;
            receipt.select_representations(&fixture.prepared.config, &output);
            assert_eq!(receipt.owned[&target], Representation::Copy);
        }
        let mut receipt = fixture.receipt("debug/deps");
        receipt.owned.remove(&target.with_extension("rmeta"));
        receipt.select_representations(&fixture.prepared.config, &output);
        assert_eq!(
            receipt.owned[&target],
            Representation::Copy,
            "transitive metadata is not an owned companion"
        );
        let mut receipt = fixture.receipt("debug/deps");
        receipt
            .trees
            .insert(directory, PathBuf::from("generated-owner"));
        receipt.select_representations(&fixture.prepared.config, &output);
        assert_eq!(receipt.owned[&target], Representation::Copy);
        let mut receipt = fixture.receipt("debug/deps");
        fixture.prepared.config.artifact_delivery = ArtifactDelivery::Copy;
        receipt.select_representations(&fixture.prepared.config, &output);
        assert_eq!(receipt.owned[&target], Representation::Copy);
    }

    #[test]
    fn exact_link_identity_and_atomic_staging_reject_substitutions() {
        let fixture = ArtifactFixture::new();
        let receipt = fixture.receipt("debug/deps");
        let target = fixture
            .prepared
            .config
            .target
            .join("debug/deps/libfixture.rlib");
        let input = fixture.prepared.physical(&receipt.files[&target]).unwrap();
        let original = input.metadata().unwrap();
        let validate = || {
            Representation::StoreLink.validate_contents(&target, &input, &mut Verifier::default())
        };
        assert!(
            validate().is_err(),
            "equal regular bytes must not authenticate a link"
        );
        Representation::StoreLink
            .publish(&input, &target, &input)
            .unwrap();
        validate().unwrap();
        assert_eq!(
            fs::read_link(&target).unwrap().as_os_str(),
            input.as_os_str()
        );
        let other = input.with_extension("other");
        fs::copy(&input, &other).unwrap();
        for spelling in [
            other,
            input.parent().unwrap().join("./libfixture.rlib"),
            PathBuf::from("libfixture.rlib"),
            input.with_extension("missing"),
        ] {
            fs::remove_file(&target).unwrap();
            unix_fs::symlink(&spelling, &target).unwrap();
            assert!(validate().is_err(), "{}", spelling.display());
        }
        Representation::StoreLink
            .publish(&input, &target, &input)
            .unwrap();
        validate().unwrap();
        assert!(
            Representation::Copy
                .validate_contents(&target, &input, &mut Verifier::default())
                .is_err()
        );
        let temporary = target.with_extension(format!("dyndrv-{}.tmp", process::id()));
        fs::write(&temporary, "other operation").unwrap();
        assert!(
            Representation::StoreLink
                .publish(&input, &target, &input)
                .is_err()
        );
        assert_eq!(fs::read(&temporary).unwrap(), b"other operation");
        fs::remove_file(&temporary).unwrap();
        fs::remove_file(&target).unwrap();
        fs::create_dir(&target).unwrap();
        assert!(
            Representation::StoreLink
                .publish(&input, &target, &input)
                .is_err()
        );
        assert!(temporary.symlink_metadata().is_err());
        assert_eq!(input.metadata().unwrap().mtime(), original.mtime());
        assert_eq!(input.metadata().unwrap().mode(), original.mode());
        fs::remove_dir(&target).unwrap();
        unix_fs::symlink(&input, &target).unwrap();
        fs::remove_file(&input).unwrap();
        assert!(validate().is_err(), "dangling expected backing must fail");
        // Structural fixtures do not certify /tmp as an administered store.
        assert!(
            fixture
                .prepared
                .validate_store_backing(&receipt.files[&target])
                .is_err()
        );
    }

    #[test]
    fn copied_dependencies_keep_full_merged_byte_checks_across_owner_changes() {
        let mut fixture = ArtifactFixture::new();
        let original = fixture.receipt("debug/deps");
        let directory = fixture.prepared.config.target.join("debug/deps");
        let archive = directory.join("libfixture.rlib");
        let direct = directory.join("libfixture.so");
        fixture
            .prepared
            .publish_receipt(&direct, &original)
            .unwrap();
        let new_owner = original.files[&archive].with_extension("new-owner");
        fs::copy(
            fixture
                .prepared
                .physical(&original.files[&archive])
                .unwrap(),
            fixture.prepared.physical(&new_owner).unwrap(),
        )
        .unwrap();
        let mut current = Delivered {
            version: 6,
            compiler_status: Some(0),
            owned: BTreeMap::from([(archive.clone(), Representation::Copy)]),
            files: BTreeMap::from([(archive.clone(), new_owner)]),
            trees: BTreeMap::new(),
        };
        fixture
            .prepared
            .publish_receipt(&archive, &current)
            .unwrap();
        let args = [
            OsString::from("--crate-type=lib"),
            OsString::from("--extern"),
            format!("dep={}", direct.display()).into(),
        ];
        assert_eq!(
            fixture
                .prepared
                .dependencies(&args, &mut BTreeMap::new())
                .unwrap(),
            original.files
        );
        let bytes = fs::read(&archive).unwrap();
        let mut changed = bytes.clone();
        changed[0] ^= 1;
        fs::write(&archive, changed).unwrap();
        assert!(
            fixture
                .prepared
                .dependencies(&args, &mut BTreeMap::new())
                .is_err()
        );
        fs::write(&archive, bytes).unwrap();
        current.version = 5;
        fixture
            .prepared
            .publish_receipt(&archive, &current)
            .unwrap();
        assert!(
            fixture
                .prepared
                .dependencies(&args, &mut BTreeMap::new())
                .is_err()
        );
        // Unlike Copy, a StoreLink cannot authenticate a different merged owner,
        // even if both backings contain the same bytes.
        current.version = 6;
        let metadata = archive.with_extension("rmeta");
        current.owned.insert(metadata.clone(), Representation::Copy);
        current
            .files
            .insert(metadata.clone(), original.files[&metadata].clone());
        current
            .owned
            .insert(archive.clone(), Representation::StoreLink);
        fixture.prepared.config.artifact_delivery = ArtifactDelivery::SystemStoreRlibLinks;
        fixture.prepared.config.store_mode = StoreMode::System;
        fixture
            .prepared
            .publish_receipt(&archive, &current)
            .unwrap();
        let error = fixture
            .prepared
            .dependencies(&args, &mut BTreeMap::new())
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("differs from merged dependency owner")
        );
    }

    #[test]
    fn absent_probe_directories_retain_backing_but_symlinked_ancestors_and_receipts_fail() {
        let fixture = ArtifactFixture::new();
        let mut receipt = fixture.receipt("debug/build/probe/out");
        receipt.compiler_status = Some(1 << 8);
        for target in receipt.owned.keys() {
            fixture.prepared.publish_receipt(target, &receipt).unwrap();
        }
        let present = fixture.prepared.snapshot_references().unwrap();
        let directory = fixture.prepared.config.target.join("debug/build/probe");
        fs::remove_dir_all(&directory).unwrap();
        let absent = fixture.prepared.snapshot_references().unwrap();
        assert_eq!(absent.owned_files, 0);
        assert_eq!(absent.store_paths, present.store_paths);
        // A dangling ancestor link must not be mistaken for a removed probe.
        unix_fs::symlink(fixture.root.join("missing"), &directory).unwrap();
        assert!(fixture.prepared.snapshot_references().is_err());
        fs::remove_file(&directory).unwrap();
        let state = fixture.prepared.config.target.join(".dyndrv-delivered");
        let moved = fixture.root.join("moved-receipts");
        fs::rename(&state, &moved).unwrap();
        unix_fs::symlink(&moved, &state).unwrap();
        assert!(fixture.prepared.snapshot_references().is_err());
        assert!(
            fixture
                .prepared
                .delivered(receipt.owned.keys().next().unwrap())
                .is_err()
        );
        fs::remove_file(&state).unwrap();
        fs::rename(&moved, &state).unwrap();
        let target = receipt.owned.keys().next().unwrap();
        fs::remove_file(fixture.prepared.physical(&receipt.files[target]).unwrap()).unwrap();
        assert!(fixture.prepared.snapshot_references().is_err());
    }

    #[test]
    fn bound_package_inputs_require_the_same_directory_and_a_whole_store_root() {
        let store_dir = StoreDir::default();
        let input = Backend::store_path(Path::new(env!("BUILD_WRAP")))
            .unwrap()
            .to_absolute_path(&store_dir);
        assert!(Backend::bound_store_root(&input, &input).unwrap().is_some());
        assert!(
            Backend::bound_store_root(&input, &input.join("bin"))
                .unwrap()
                .is_none()
        );
        assert!(Backend::bound_store_root(&input, &input.join("../other")).is_err());

        let root = env::temp_dir().join(format!(
            "dyndrv-store-binding-{}-{}",
            process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        // A declared store path alone must not authenticate an unrelated mount.
        assert!(Backend::bound_store_root(&root, &input).unwrap().is_none());
        let alias = root.join("alias");
        unix_fs::symlink(&input, &alias).unwrap();
        assert!(Backend::bound_store_root(&alias, &input).unwrap().is_some());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn source_scope_prunes_disjoint_local_and_packaged_roots() {
        let trees: BTreeMap<_, _> = [
            ("/build/workspace", "workspace-input"),
            ("/build/local", "local-input"),
            ("/build/vendor/registry/crate-1.0", "vendor-input"),
            ("/nix/store/packaged", "packaged-input"),
        ]
        .map(|(root, input)| (PathBuf::from(root), PathBuf::from(input)))
        .into();
        for (root, input) in &trees {
            let selected = Backend::source_trees(&trees, &root.join("src"), [], []);
            assert_eq!(selected.len(), 1, "owner: {}", root.display());
            assert_eq!(selected.get(root), Some(input));
        }
        // Selection must leave the complete prepared map available for fallback.
        assert_eq!(trees.len(), 4);
    }

    #[test]
    fn source_scope_distinguishes_nix_build_top_from_declared_build_roots() {
        let trees: BTreeMap<_, _> = [
            ("/build/cargo-dyndrv/workspace", "workspace-input"),
            ("/build/cargo-dyndrv/sources/local", "local-input"),
            (
                "/build/cargo-dyndrv/vendor/registry/pkg-1.0",
                "vendor-input",
            ),
        ]
        .map(|(root, input)| (PathBuf::from(root), PathBuf::from(input)))
        .into();
        let infrastructure = [(b"NIX_BUILD_TOP".as_slice(), b"/build".as_slice())];
        for (root, input) in &trees {
            let selected = Backend::source_trees(&trees, root, [], infrastructure);
            assert_eq!(selected, BTreeMap::from([(root.clone(), input.clone())]));
        }
        let cwd = Path::new("/build/cargo-dyndrv/sources/local");
        let environment = [
            infrastructure[0],
            (b"DATA_ROOT".as_slice(), b"/build".as_slice()),
        ];
        assert_eq!(Backend::source_trees(&trees, cwd, [], environment), trees);
        assert_eq!(
            Backend::source_trees(&trees, cwd, [b"--data=/build".as_slice()], infrastructure),
            trees,
        );
    }

    #[test]
    fn layout_environment_does_not_bind_the_workspace_to_registry_compilers() {
        let mut trees = BTreeMap::from([
            (
                PathBuf::from("/build/workspace"),
                PathBuf::from("workspace-v1"),
            ),
            (PathBuf::from("/build/vendor/pkg"), PathBuf::from("package")),
        ]);
        let cwd = Path::new("/build/vendor/pkg");
        let mut environment = vec![(
            b"INSTA_WORKSPACE_ROOT".to_vec(),
            b"/build/workspace".to_vec(),
        )];
        let layouts = BTreeSet::from(["INSTA_WORKSPACE_ROOT".to_owned()]);
        let selected = Backend::source_trees(
            &trees,
            cwd,
            [],
            Backend::source_environment(&environment, &layouts),
        );
        assert_eq!(selected.len(), 1);
        trees.insert(
            PathBuf::from("/build/workspace"),
            PathBuf::from("workspace-v2"),
        );
        assert_eq!(
            Backend::source_trees(
                &trees,
                cwd,
                [],
                Backend::source_environment(&environment, &layouts),
            ),
            selected,
        );
        environment.push((b"DATA_ROOT".to_vec(), b"/build/workspace".to_vec()));
        assert_eq!(
            Backend::source_trees(
                &trees,
                cwd,
                [],
                Backend::source_environment(&environment, &layouts),
            ),
            trees,
        );
    }

    #[test]
    fn source_scope_keeps_argument_and_effective_environment_references() {
        let trees: BTreeMap<_, _> = [
            ("/build/workspace", "workspace-input"),
            ("/build/external", "external-input"),
            ("/build/unrelated", "unrelated-input"),
        ]
        .map(|(root, input)| (PathBuf::from(root), PathBuf::from(input)))
        .into();
        let cwd = Path::new("/build/workspace");
        for value in [
            b"/build/external".as_slice(),
            b"/build/external/", // A trailing slash must not mean `/`.
            b"-Lnative=/build/external/lib",
            b"-L/build/external/lib",
            b"--extern=helper=/build/external/helper.rlib",
            b"link-args=-Wl,-rpath,/build/external/lib",
            b"@/build/external/response-file",
            b"/unrelated:/build/external/data:/another",
            b"\"/build/external/data\"",
            b"\xff/build/external/\xfe",
            b"/build//external/data",
        ] {
            let selected = Backend::source_trees(&trees, cwd, [value], []);
            assert_eq!(selected.len(), 2, "reference: {value:?}");
            assert_eq!(
                selected.get(Path::new("/build/external")),
                trees.get(Path::new("/build/external"))
            );
        }
        let arguments = [
            OsString::from("src/lib.rs"),
            OsString::from("--cfg=feature=demo"),
        ];
        // This is the effective (already policy-filtered) environment, matching
        // invocation's input. Omitted ambient values are deliberately absent.
        let environment = [(
            b"RESOURCE_ROOT".as_slice(),
            b"/build/external/assets".as_slice(),
        )];
        let selected = Backend::source_trees(
            &trees,
            cwd,
            arguments.iter().map(|value| value.as_bytes()),
            environment,
        );
        assert!(selected.contains_key(Path::new("/build/external")));
        assert!(!selected.contains_key(Path::new("/build/unrelated")));
    }

    #[test]
    fn source_scope_respects_prefix_boundaries_and_explicit_ancestors() {
        let trees: BTreeMap<_, _> = [
            ("/build/workspace", "workspace-input"),
            ("/build/external/pkg", "package-input"),
            ("/build/external/pkg-other", "other-input"),
            ("/build/elsewhere", "unrelated-input"),
        ]
        .map(|(root, input)| (PathBuf::from(root), PathBuf::from(input)))
        .into();
        let cwd = Path::new("/build/workspace");
        let selected = Backend::source_trees(
            &trees,
            cwd,
            [b"/build/external/pkg-other/src/lib.rs".as_slice()],
            [],
        );
        assert_eq!(selected.len(), 2);
        assert!(!selected.contains_key(Path::new("/build/external/pkg")));
        for value in [b"BASE=/build/external".as_slice(), b"BASE=/build/external/"] {
            let selected = Backend::source_trees(&trees, cwd, [value], []);
            assert_eq!(selected.len(), 3, "ancestor: {value:?}");
            assert!(!selected.contains_key(Path::new("/build/elsewhere")));
        }
    }

    #[test]
    fn source_scope_preserves_non_utf8_and_delimited_root_names() {
        let root = PathBuf::from(OsString::from_vec(b"/build/external =\xff".to_vec()));
        let trees = BTreeMap::from([
            (
                PathBuf::from("/build/workspace"),
                PathBuf::from("workspace-input"),
            ),
            (root.clone(), PathBuf::from("external-input")),
            (
                PathBuf::from("/build/unrelated"),
                PathBuf::from("unrelated-input"),
            ),
        ]);
        let selected = Backend::source_trees(
            &trees,
            Path::new("/build/workspace"),
            [b"--data=/build/external =\xff/file".as_slice()],
            [],
        );
        assert_eq!(selected.len(), 2);
        assert_eq!(selected.get(&root), trees.get(&root));
    }

    #[test]
    fn source_scope_keeps_layout_for_traversal_and_unknown_owners() {
        let trees: BTreeMap<_, _> = [
            ("/build/workspace", "workspace-input"),
            ("/build/external", "external-input"),
            ("/build/unrelated", "unrelated-input"),
        ]
        .map(|(root, input)| (PathBuf::from(root), PathBuf::from(input)))
        .into();
        for value in [
            b"../external/data".as_slice(),
            b"BASE=..",
            b"/build/workspace/../external/data",
            b"/build/./external",
        ] {
            assert_eq!(
                Backend::source_trees(&trees, Path::new("/build/workspace"), [value], []),
                trees
            );
        }
        for cwd in ["/build/mirror", "/build/target/generated", "/build"] {
            assert_eq!(Backend::source_trees(&trees, Path::new(cwd), [], []), trees);
        }
    }

    #[test]
    fn source_scope_keeps_complete_overlapping_groups() {
        // Preparation still rejects nested declarations. Exercise selection's
        // defensive closure independently, including a separately captured
        // generated tree: source snapshots do not contain target contents.
        let trees: BTreeMap<_, _> = [
            ("/build/workspace", "workspace-input"),
            ("/build/workspace/nested", "nested-input"),
            ("/build/workspace/target/generated", "generated-input"),
            ("/build/workspace/sibling", "sibling-input"),
            ("/build/unrelated", "unrelated-input"),
        ]
        .map(|(root, input)| (PathBuf::from(root), PathBuf::from(input)))
        .into();
        let selected =
            Backend::source_trees(&trees, Path::new("/build/workspace/nested/src"), [], []);
        assert_eq!(selected.len(), 4);
        assert_eq!(
            selected.get(Path::new("/build/workspace/target/generated")),
            trees.get(Path::new("/build/workspace/target/generated"))
        );
        assert!(!selected.contains_key(Path::new("/build/unrelated")));
    }

    #[test]
    fn source_scope_invalidation_tracks_every_retained_root_file() {
        let root = env::temp_dir().join(format!(
            "dyndrv-source-scope-{}-{}",
            process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        let owner = root.join("owner");
        let unrelated = root.join("unrelated");
        fs::create_dir_all(owner.join("assets")).unwrap();
        fs::create_dir_all(&unrelated).unwrap();
        fs::write(owner.join("lib.rs"), b"pub fn function() {}").unwrap();
        fs::write(owner.join("assets/proc-macro-data"), b"first").unwrap();
        fs::write(unrelated.join("lib.rs"), b"first").unwrap();
        let snapshot = || {
            [&owner, &unrelated]
                .into_iter()
                .map(|path| {
                    let nar = store::source_nar(path).unwrap();
                    (path.to_path_buf(), PathBuf::from(Backend::digest(&nar)))
                })
                .collect::<BTreeMap<_, _>>()
        };
        let before = Backend::source_trees(&snapshot(), &owner, [], []);
        fs::write(unrelated.join("lib.rs"), b"second").unwrap();
        assert_eq!(Backend::source_trees(&snapshot(), &owner, [], []), before);
        // A non-Rust asset, absent from Cargo/dep-info, still owns the request.
        fs::write(owner.join("assets/proc-macro-data"), b"second").unwrap();
        let changed = Backend::source_trees(&snapshot(), &owner, [], []);
        assert_ne!(changed, before);
        let referenced =
            Backend::source_trees(&snapshot(), &owner, [unrelated.as_os_str().as_bytes()], []);
        fs::write(unrelated.join("lib.rs"), b"third").unwrap();
        assert_ne!(
            Backend::source_trees(&snapshot(), &owner, [unrelated.as_os_str().as_bytes()], []),
            referenced
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn maturin_cfg_is_a_probe_but_compilation_is_not() {
        let args = ["--print", "cfg", "--target", "x86_64-unknown-linux-gnu"].map(OsString::from);
        assert!(Backend::is_probe(OsStr::new("rustc"), &args));
        let mut compilation = args.to_vec();
        compilation.push("input.rs".into());
        assert!(!Backend::is_probe(OsStr::new("rustc"), &compilation));
    }

    #[test]
    fn private_target_policy_is_explicit_and_only_selects_the_workspace_placeholder() {
        let source = PathBuf::from("/build/source");
        let target = source.join("target");
        let generated = target.join("generated");
        let mut invocation = Invocation {
            trees: BTreeMap::new(),
            source: source.clone(),
            target: target.clone(),
            cwd: source.clone(),
            output: CompilerOutput::Stdout,
            incremental: None,
            jobserver: false,
            source_overlay: true,
            private_target: false,
            program: Vec::new(),
            arguments: Vec::new(),
            stdin: None,
            environment: Vec::new(),
            dependencies: BTreeMap::new(),
        };
        let copying = serde_json::to_vec(&invocation).unwrap();
        let request = serde_json::to_value(&invocation).unwrap();
        assert!(request.get("artifact_delivery").is_none());
        assert!(matches!(
            invocation.writable(&source).as_slice(),
            [source_wrap::Writable::Directory(path), source_wrap::Writable::ScratchDirectory(cwd)]
                if **path == target && **cwd == source
        ));
        invocation.private_target = true;
        let private = serde_json::to_vec(&invocation).unwrap();
        assert_ne!(Backend::digest(&copying), Backend::digest(&private));
        assert!(
            serde_json::from_slice::<Invocation>(&private)
                .unwrap()
                .private_target
        );
        assert!(matches!(
            invocation.writable(&source).as_slice(),
            [source_wrap::Writable::EmptyDirectory(path), source_wrap::Writable::ScratchDirectory(cwd)]
                if **path == target && **cwd == source
        ));
        invocation.output = CompilerOutput::Directory(generated.join("outputs"));
        assert!(matches!(
            invocation.writable(&generated).as_slice(),
            [source_wrap::Writable::Directory(path)] if path.starts_with(&generated)
        ));
        invocation.output = CompilerOutput::File(generated.join("output"));
        assert!(matches!(
            invocation.writable(&generated).as_slice(),
            [source_wrap::Writable::File(_)]
        ));
        invocation.target = PathBuf::from("/build/external-target");
        invocation.output = CompilerOutput::Directory(invocation.target.join("deps"));
        assert!(invocation.writable(&source).is_empty());
    }

    #[test]
    fn metadata_reference_scan_retains_declared_bare_hashes_and_colliding_names() {
        let path = |name: &str| Backend::store_path(Path::new(name)).unwrap();
        let first = path("/nix/store/00000000000000000000000000000000-first");
        let same_hash = path("/nix/store/00000000000000000000000000000000-second");
        let absent = path("/nix/store/11111111111111111111111111111111-absent");
        let candidates = BTreeSet::from([first.clone(), same_hash.clone(), absent]);
        let mut bytes = vec![255; 20];
        bytes.extend_from_slice(b"x00000000000000000000000000000000x");
        bytes.push(0);
        bytes.extend_from_slice(b"111111111111111111111111111111");
        assert_eq!(
            Backend::metadata_references(&bytes, candidates),
            BTreeSet::from([first, same_hash])
        );
    }

    #[test]
    fn metadata_closures_ignore_completion_timing_but_linkers_refresh_archives() {
        let root = std::env::temp_dir().join(format!(
            "dyndrv-metadata-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let target = root.join("target");
        fs::create_dir_all(target.join(".dyndrv-delivered")).unwrap();
        let mut backend = Backend {
            config: Config {
                nix: PathBuf::new(),
                socket: PathBuf::new(),
                real_store: root.join("store"),
                store_mode: Default::default(),
                artifact_delivery: ArtifactDelivery::Copy,
                gc_roots: None,
                source: root.join("source"),
                package_sources: Vec::new(),
                package_store_inputs: BTreeMap::new(),
                layout_environment_names: BTreeSet::new(),
                target: target.clone(),
                inputs: Vec::new(),
                environment_names: BTreeSet::new(),
                environment_prefixes: Vec::new(),
                omitted_environment_names: BTreeSet::new(),
                incremental: false,
                incremental_observer: false,
                source_overlay: false,
                private_target: false,
                jobserver_socket_directory: None,
            },
            compiler_inputs: None,
        };
        let names = [
            "libleaf.rmeta",
            "libleaf.rlib",
            "leaf.d",
            "libmiddle.rmeta",
            "libmiddle.rlib",
        ];
        let files: BTreeMap<_, _> = names
            .into_iter()
            .map(|name| {
                let local = target.join(name);
                let immutable =
                    Path::new("/nix/store/00000000000000000000000000000000-fixture").join(name);
                let physical = backend.physical(&immutable).unwrap();
                fs::create_dir_all(physical.parent().unwrap()).unwrap();
                fs::write(&physical, name).unwrap();
                fs::write(&local, name).unwrap();
                (local, immutable)
            })
            .collect();
        let subset = |names: &[&str]| {
            names
                .iter()
                .map(|name| {
                    let path = target.join(name);
                    (path.clone(), files[&path].clone())
                })
                .collect::<BTreeMap<_, _>>()
        };
        let declared = root.join("source");
        let generated = target.join("generated");
        let old_source = PathBuf::from("/nix/store/11111111111111111111111111111111-source");
        let old_generated = PathBuf::from("/nix/store/22222222222222222222222222222222-generated");
        let current_source = PathBuf::from("/nix/store/33333333333333333333333333333333-source");
        let current_generated =
            PathBuf::from("/nix/store/44444444444444444444444444444444-generated");
        for input in [
            &old_source,
            &old_generated,
            &current_source,
            &current_generated,
        ] {
            fs::create_dir_all(backend.physical(input).unwrap()).unwrap();
        }
        backend.compiler_inputs = Some(CompilerInputs {
            trees: BTreeMap::from([(declared.clone(), current_source.clone())]),
            builder_store: PathBuf::from(env!("BUILD_WRAP")),
        });
        fs::create_dir(backend.physical(&old_source).unwrap().join("data")).unwrap();
        fs::create_dir(backend.physical(&old_generated).unwrap().join("nested")).unwrap();
        let sources = BTreeMap::from([
            (declared.clone(), old_source.clone()),
            (declared.join("data"), old_source.join("data")),
            (generated.clone(), old_generated.clone()),
            (generated.join("nested"), old_generated.join("nested")),
        ]);
        let receipt = |names: &[&str], compiler_status, contents: BTreeMap<PathBuf, PathBuf>| {
            for name in names {
                fs::write(
                    backend.receipt(&target.join(name)),
                    serde_json::to_vec(&Delivered {
                        version: 6,
                        trees: sources.clone(),
                        compiler_status,
                        owned: names
                            .iter()
                            .map(|name| (target.join(name), Representation::Copy))
                            .collect(),
                        files: contents.clone(),
                    })
                    .unwrap(),
                )
                .unwrap();
            }
        };
        let args = |kind: &str, name: &str| {
            vec![
                OsString::from("--crate-type"),
                kind.into(),
                "--extern".into(),
                format!("dep={}", target.join(name).display()).into(),
            ]
        };
        receipt(&["libleaf.rmeta"], None, subset(&["libleaf.rmeta"]));
        let library = args("rlib", "libleaf.rmeta");
        let mut early_trees = BTreeMap::new();
        let early = backend.dependencies(&library, &mut early_trees).unwrap();
        assert_eq!(
            early_trees,
            BTreeMap::from([
                (declared.clone(), current_source.clone()),
                (generated.clone(), old_generated.clone()),
            ])
        );
        // Explicitly captured generated inputs must not be replaced by the
        // dependency's older snapshot at the same logical location.
        let mut generated_trees = BTreeMap::from([(generated.clone(), current_generated.clone())]);
        backend
            .dependencies(&library, &mut generated_trees)
            .unwrap();
        assert_eq!(generated_trees[&generated], current_generated);
        assert!(
            backend
                .dependencies(&args("bin", "libleaf.rmeta"), &mut BTreeMap::new())
                .is_err()
        );
        // Middle completed using only leaf metadata. Its immutable receipt
        // cannot retrospectively contain the then-unfinished leaf archive.
        receipt(
            &["libmiddle.rmeta", "libmiddle.rlib"],
            Some(0),
            subset(&["libleaf.rmeta", "libmiddle.rmeta", "libmiddle.rlib"]),
        );
        assert!(
            backend
                .dependencies(&args("bin", "libmiddle.rlib"), &mut BTreeMap::new())
                .is_err()
        );
        receipt(
            &["libleaf.rmeta", "libleaf.rlib", "leaf.d"],
            Some(0),
            subset(&["libleaf.rmeta", "libleaf.rlib", "leaf.d"]),
        );
        assert_eq!(
            backend
                .dependencies(&library, &mut BTreeMap::new())
                .unwrap(),
            early
        );
        assert_eq!(
            backend
                .dependencies(&args("bin", "libmiddle.rlib"), &mut BTreeMap::new())
                .unwrap(),
            files
        );
        assert_eq!(
            backend
                .dependencies(&args("lib", "libmiddle.rmeta"), &mut BTreeMap::new())
                .unwrap(),
            subset(&["libleaf.rmeta", "libmiddle.rmeta"])
        );
        let archive = target.join("libleaf.rlib");
        let parsed = backend.delivered(&archive).unwrap();
        let mut verifier = Verifier::default();
        backend
            .validate_receipt_contents(&archive, &parsed, &parsed.files[&archive], &mut verifier)
            .unwrap();
        let mut bytes = fs::read(&archive).unwrap();
        bytes[0] ^= 1;
        fs::write(&archive, &bytes).unwrap();
        assert!(
            backend
                .validate_receipt_contents(
                    &archive,
                    &parsed,
                    &parsed.files[&archive],
                    &mut verifier
                )
                .is_err()
        );
        assert!(
            backend
                .dependencies(&args("bin", "libmiddle.rlib"), &mut BTreeMap::new())
                .is_err()
        );
        assert_eq!(
            backend
                .dependencies(&library, &mut BTreeMap::new())
                .unwrap(),
            early
        );
        assert!(backend.snapshot_references().is_err());
        bytes[0] ^= 1;
        fs::write(&archive, &bytes).unwrap();
        let snapshot = backend.snapshot_references().unwrap();
        assert_eq!(snapshot.owned_files, 5);
        assert_eq!(snapshot.incremental_children, 0);
        assert_eq!(
            snapshot.store_paths,
            BTreeSet::from([
                PathBuf::from("/nix/store/00000000000000000000000000000000-fixture"),
                old_source,
                old_generated,
            ])
        );
        let mut invalid = backend.delivered(&archive).unwrap();
        invalid.version = 4;
        assert!(invalid.validate(&archive, &backend.config).is_err());
        backend.publish_receipt(&archive, &invalid).unwrap();
        assert!(backend.validate_replacement(&archive).is_err());
        assert!(backend.snapshot_references().is_err());
        invalid.version = 6;
        // Keep this key mapped while removing another owned file's mapping.
        invalid.files.remove(&target.join("leaf.d")).unwrap();
        assert!(invalid.files.contains_key(&archive));
        assert!(invalid.validate(&archive, &backend.config).is_err());
        backend.publish_receipt(&archive, &invalid).unwrap();
        assert!(backend.validate_replacement(&archive).is_err());
        assert!(backend.snapshot_references().is_err());
        receipt(
            &["libleaf.rmeta", "libleaf.rlib", "leaf.d"],
            Some(0),
            subset(&["libleaf.rmeta", "libleaf.rlib", "leaf.d"]),
        );
        // Expected compile-fail tests retain dep-info from finished rustc
        // failures. Snapshot ownership is distinct from link readiness.
        receipt(&["leaf.d"], None, subset(&["leaf.d"]));
        assert!(backend.snapshot_references().is_err());
        receipt(&["leaf.d"], Some(1 << 8), subset(&["leaf.d"]));
        let failed_probe = backend.snapshot_references().unwrap();
        assert_eq!(failed_probe.owned_files, snapshot.owned_files);
        assert_eq!(failed_probe.store_paths, snapshot.store_paths);
        assert!(
            backend
                .dependencies(&args("bin", "leaf.d"), &mut BTreeMap::new())
                .is_err()
        );
        fs::write(target.join("leaf.d"), "changed failed-probe artifact").unwrap();
        assert!(backend.snapshot_references().is_err());
        fs::write(target.join("leaf.d"), "leaf.d").unwrap();
        receipt(&["leaf.d"], Some(0), subset(&["leaf.d"]));
        receipt(&["libleaf.rmeta"], None, subset(&["libleaf.rmeta"]));
        assert!(backend.snapshot_references().is_err());
        fs::remove_file(target.join("libleaf.rmeta")).unwrap();
        let deleted_probe = backend.snapshot_references().unwrap();
        assert_eq!(deleted_probe.owned_files, 4);
        assert_eq!(deleted_probe.store_paths, snapshot.store_paths);
        fs::write(target.join("libleaf.rmeta"), "libleaf.rmeta").unwrap();
        receipt(
            &["libleaf.rmeta", "libleaf.rlib", "leaf.d"],
            Some(0),
            subset(&["libleaf.rmeta", "libleaf.rlib", "leaf.d"]),
        );
        backend.reject_incomplete().unwrap();
        assert!(backend.snapshot_references().is_err());
        fs::remove_file(target.join(".dyndrv-delivered/rejected")).unwrap();
        // A transitive-only file must be checked even after a sibling in its
        // store root has passed. It has neither a current target nor a receipt.
        let transitive = target.join("zz-transitive.rlib");
        let input = files[&archive].parent().unwrap().join("zz-transitive.rlib");
        let physical = backend.physical(&input).unwrap();
        fs::write(&physical, b"transitive-only").unwrap();
        let mut retained = backend.delivered(&archive).unwrap();
        retained.files.insert(transitive, input);
        backend.publish_receipt(&archive, &retained).unwrap();
        let complete = backend.snapshot_references().unwrap();
        assert_eq!(complete.owned_files, snapshot.owned_files);
        assert_eq!(complete.store_paths, snapshot.store_paths);
        fs::remove_file(&physical).unwrap();
        assert!(backend.snapshot_references().is_err());
        receipt(
            &["libleaf.rmeta", "libleaf.rlib", "leaf.d"],
            Some(0),
            subset(&["libleaf.rmeta", "libleaf.rlib", "leaf.d"]),
        );
        fs::remove_file(
            backend
                .physical(&files[&target.join("libleaf.rlib")])
                .unwrap(),
        )
        .unwrap();
        assert!(backend.snapshot_references().is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn linked_outputs_require_complete_dependencies() {
        let links = |args: &[&str]| {
            Backend::links_dependencies(&args.iter().map(OsString::from).collect::<Vec<_>>())
        };
        assert!(!links(&[
            "--crate-type=lib",
            "--emit=dep-info,metadata,link"
        ]));
        assert!(!links(&["--crate-type", "rlib"]));
        assert!(!links(&["--emit=metadata"]));
        for args in [
            vec!["--crate-type=lib,cdylib"],
            vec!["--crate-type=proc-macro"],
            vec!["--crate-type=staticlib"],
            vec!["--crate-type=lib", "--test"],
            vec!["--crate-type=bin"],
            vec![],
        ] {
            assert!(links(&args), "{args:?}");
        }
    }

    #[test]
    fn overlapping_receipts_preserve_transitive_validation_and_reject_conflicting_owners() {
        let root = std::env::temp_dir().join(format!(
            "dyndrv-dependencies-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let target = root.join("target");
        fs::create_dir_all(target.join(".dyndrv-delivered")).unwrap();
        let backend = Backend {
            config: Config {
                nix: PathBuf::new(),
                socket: PathBuf::new(),
                real_store: root.join("store"),
                store_mode: Default::default(),
                artifact_delivery: ArtifactDelivery::Copy,
                gc_roots: None,
                source: root.join("source"),
                package_sources: Vec::new(),
                package_store_inputs: BTreeMap::new(),
                layout_environment_names: BTreeSet::new(),
                target: target.clone(),
                inputs: Vec::new(),
                environment_names: BTreeSet::new(),
                environment_prefixes: Vec::new(),
                omitted_environment_names: BTreeSet::new(),
                incremental: false,
                incremental_observer: false,
                source_overlay: false,
                private_target: false,
                jobserver_socket_directory: None,
            },
            compiler_inputs: None,
        };
        let immutable_root = Path::new("/nix/store/00000000000000000000000000000000-fixture");
        let mut inputs = BTreeMap::new();
        for name in ["left.rlib", "right.rlib", "shared.rlib"] {
            let local = target.join(name);
            let immutable = immutable_root.join(name);
            let physical = backend.physical(&immutable).unwrap();
            fs::create_dir_all(physical.parent().unwrap()).unwrap();
            fs::write(&physical, name).unwrap();
            fs::write(&local, name).unwrap();
            inputs.insert(local, immutable);
        }
        let shared = target.join("shared.rlib");
        let write_receipt = |name: &str, shared_owner: &Path| {
            let local = target.join(name);
            let files = BTreeMap::from([
                (local.clone(), inputs[&local].clone()),
                (shared.clone(), shared_owner.to_path_buf()),
            ]);
            fs::write(
                backend.receipt(&local),
                serde_json::to_vec(&Delivered {
                    version: 6,
                    trees: BTreeMap::new(),
                    compiler_status: Some(0),
                    owned: BTreeMap::from([(local.clone(), Representation::Copy)]),
                    files,
                })
                .unwrap(),
            )
            .unwrap();
        };
        write_receipt("left.rlib", &inputs[&shared]);
        write_receipt("right.rlib", &inputs[&shared]);
        let args = [
            OsString::from("--extern"),
            format!("left={}", target.join("left.rlib").display()).into(),
            OsString::from("--extern"),
            format!("right={}", target.join("right.rlib").display()).into(),
            OsString::from("--extern"),
            OsString::from("proc_macro"),
        ];
        assert_eq!(
            backend.dependencies(&args, &mut BTreeMap::new()).unwrap(),
            inputs
        );
        fs::write(&shared, "changed transitive input").unwrap();
        assert!(
            backend
                .dependencies(&args, &mut BTreeMap::new())
                .unwrap_err()
                .to_string()
                .contains("differs from immutable input")
        );
        fs::write(&shared, "shared.rlib").unwrap();
        assert_eq!(
            backend.dependencies(&args, &mut BTreeMap::new()).unwrap(),
            inputs
        );
        write_receipt("right.rlib", &immutable_root.join("conflicting-owner.rlib"));
        assert!(
            backend
                .dependencies(&args, &mut BTreeMap::new())
                .unwrap_err()
                .to_string()
                .contains("conflicting dependency owners")
        );
        write_receipt("right.rlib", &inputs[&shared]);
        fs::remove_file(&shared).unwrap();
        assert!(backend.dependencies(&args, &mut BTreeMap::new()).is_err());
        let local = target.join("left.rlib");
        let own_object = target.join("left.rcgu.o");
        let dependency_object = target.join("shared.rcgu.o");
        let object_owner = immutable_root.join("left.rcgu.o");
        backend
            .publish_receipt(
                &local,
                &Delivered {
                    version: 6,
                    trees: BTreeMap::new(),
                    compiler_status: Some(1 << 8),
                    owned: BTreeMap::from([
                        (local.clone(), Representation::Copy),
                        (own_object.clone(), Representation::Copy),
                    ]),
                    files: BTreeMap::from([
                        (local.clone(), inputs[&local].clone()),
                        (own_object.clone(), object_owner.clone()),
                        (dependency_object, immutable_root.join("shared.rcgu.o")),
                    ]),
                },
            )
            .unwrap();
        assert_eq!(
            backend.previous_objects(&local).unwrap(),
            BTreeMap::from([(own_object, object_owner)])
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn explicit_probe_outputs_require_one_owned_location_and_metadata_only() {
        let parse = |args: &[&str]| {
            CompilerOutput::from_args(
                &args.iter().map(OsString::from).collect::<Vec<_>>(),
                Path::new("/tmp/target"),
            )
        };
        assert!(matches!(
            parse(&["--emit=metadata", "-o", "-"]).unwrap(),
            CompilerOutput::Stdout
        ));
        assert!(matches!(
            parse(&["--emit=metadata", "-o", "/tmp/target/out/probe"]).unwrap(),
            CompilerOutput::File(_)
        ));
        assert!(matches!(
            parse(&["--out-dir", "/tmp/target/deps"]).unwrap(),
            CompilerOutput::Directory(_)
        ));
        for args in [
            vec!["--emit=metadata", "-o"],
            vec!["--emit=metadata", "-o", "relative"],
            vec!["--emit=metadata", "-o", "/tmp/elsewhere"],
            vec!["--emit=metadata", "-o", "/tmp/target/../escape"],
            vec!["--emit=metadata", "-o", "/tmp/target"],
            vec!["--emit=metadata", "-o", "-", "--out-dir", "/tmp/target/out"],
            vec!["--emit=metadata", "-o", "-", "-o", "-"],
            vec!["--emit=metadata,link", "-o", "-"],
            vec!["--emit=metadata", "--emit=metadata=/tmp/other", "-o", "-"],
            vec!["--crate-type=rlib", "-o", "-"],
        ] {
            assert!(parse(&args).is_err(), "{args:?}");
        }
    }
    #[test]
    fn only_noncompiling_queries_bypass_the_builder() {
        let probe = |args: &[&str]| {
            Backend::is_probe(
                OsStr::new("rustc"),
                &args.iter().map(OsString::from).collect::<Vec<_>>(),
            )
        };
        assert!(probe(&["-vV"]));
        assert!(probe(&["-V"]));
        assert!(probe(&["--version"]));
        assert!(probe(&["--version", "--verbose"]));
        assert!(!probe(&["--version", "input.rs"]));
        assert!(probe(&[
            "-",
            "--crate-name",
            "___",
            "--print=file-names",
            "--print=cfg"
        ]));
        assert!(!probe(&[
            "input.custom",
            "--crate-name",
            "actual",
            "--out-dir",
            "/tmp/target"
        ]));
        assert!(!probe(&["-", "--print=file-names", "--print=link-args"]));
        assert!(!probe(&["-", "--print=file-names", "--emit=link"]));
        assert!(!probe(&["--test", "input.rs"]));
    }

    #[test]
    fn clippy_queries_require_the_literal_rustc_driver_chain() {
        let probe = |program: &str, args: &[&str]| {
            Backend::is_probe(
                OsStr::new(program),
                &args.iter().map(OsString::from).collect::<Vec<_>>(),
            )
        };
        assert!(probe("clippy-driver", &["/tool/bin/rustc", "-vV"]));
        assert!(probe(
            "clippy-driver",
            &["/tool/bin/rustc", "-", "--print=file-names", "--print=cfg"]
        ));
        assert!(!probe(
            "clippy-driver",
            &[
                "/tool/bin/rustc",
                "-",
                "--print=file-names",
                "--emit=metadata"
            ]
        ));
        assert!(!probe("clippy-driver", &["/tool/bin/rustc", "input.rs"]));
        assert!(!probe("clippy-driver", &["-vV"]));
        assert!(!probe("clippy-driver", &["/tool/bin/other", "-vV"]));
        assert!(!probe("other", &["-vV"]));
        assert!(probe("rustdoc", &["-vV"]));
        assert!(!probe("rustdoc", &["--test", "input.rs"]));
    }

    #[test]
    fn compiler_stdin_preserves_bytes_without_output_pipe_deadlock() {
        let input = vec![b'x'; 262_144];
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "head -c 131072 /dev/zero >&2; cat"]);
        let result =
            Backend::compiler_output(&mut command, Some(input.clone()), |_| Ok(())).unwrap();
        assert!(result.status.success());
        assert_eq!(result.stdout, input);
        assert_eq!(result.stderr, vec![0; 131_072]);
        let mut early_exit = Command::new("/bin/sh");
        early_exit.args(["-c", "exit 23"]);
        let result =
            Backend::compiler_output(&mut early_exit, Some(vec![0; 1_048_576]), |_| Ok(()))
                .unwrap();
        assert_eq!(result.status.code(), Some(23));
    }

    #[test]
    fn abandoning_a_stream_kills_and_reaps_its_child() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "printf 'ready\\n' >&2; exec sleep 30"]);
        let mut process = CapturedProcess::spawn(&mut command, None).unwrap();
        let pid = process.child.id();
        assert_eq!(process.line().unwrap(), b"ready\n");
        drop(process);
        let mut status = 0;
        // SAFETY: status is a valid writeable integer; only our recorded child
        // PID is queried, and WNOHANG prevents waiting for any other process.
        assert_eq!(
            unsafe { libc::waitpid(pid as i32, &mut status, libc::WNOHANG) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }
}
