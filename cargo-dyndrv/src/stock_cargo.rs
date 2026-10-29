//! Drive unmodified Cargo at its process boundary. Capture mode observes native
//! execution; Nix mode binds actual commands to immutable compiler inputs. Cargo
//! retains command construction, scheduling, build scripts and target ownership.
mod nix;
use self::nix::Backend;
use color_eyre::eyre;
use color_eyre::eyre::{Context, bail};
use serde::{Deserialize, Serialize};
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::fs::{DirBuilder, OpenOptions};
use std::io::Write;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs as unix_fs;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process;
use std::process::{Command, ExitStatus};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const RUSTC_WRAPPER: &str = "dyndrv-stock-rustc";
const RUSTDOC_WRAPPER: &str = "dyndrv-stock-rustdoc";

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Session {
    version: u32,
    rustdoc: Vec<u8>,
    #[serde(default)]
    nix: Option<Backend>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Completion {
    version: u32,
    status: i32,
    nix_invocations: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
enum Execution {
    NativeDoctest,
    NativeRustdoc,
    NativeProbe,
    NixCompiler,
    NativeExecNotNix,
}

impl Execution {
    fn completion(self) -> Option<(&'static str, &'static [u8])> {
        match self {
            Self::NixCompiler => Some(("nix-complete", b"delivered\n")),
            Self::NativeDoctest | Self::NativeRustdoc => Some(("native-complete", b"executed\n")),
            Self::NativeProbe | Self::NativeExecNotNix => None,
        }
    }
}

impl Session {
    fn create(root: &Path, rustdoc: &Path, config: Option<&Path>) -> eyre::Result<Self> {
        if !root.is_absolute() || !rustdoc.is_absolute() || !rustdoc.is_file() {
            bail!("capture directory and rustdoc executable must be absolute");
        }
        // Exclusive creation prevents overwriting prior evidence or mixing owners.
        DirBuilder::new().mode(0o700).create(root)?;
        DirBuilder::new().mode(0o700).create(root.join("bin"))?;
        DirBuilder::new()
            .mode(0o700)
            .create(root.join("invocations"))?;
        let session = Self {
            version: 2,
            rustdoc: rustdoc.as_os_str().as_bytes().to_vec(),
            nix: config.map(Backend::configure).transpose()?,
        };
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(root.join("session.json"))?;
        serde_json::to_writer(&mut file, &session)?;
        file.write_all(b"\n")?;
        let executable = env::current_exe()?;
        for name in [RUSTC_WRAPPER, RUSTDOC_WRAPPER] {
            unix_fs::symlink(&executable, root.join("bin").join(name))?;
        }
        Ok(session)
    }

    fn execute(wrapper: &Path, arguments: &[OsString]) -> eyre::Result<()> {
        let root = wrapper
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| eyre::eyre!("wrapper lacks a capture directory"))?;
        let session: Self = serde_json::from_slice(&fs::read(root.join("session.json"))?)?;
        if session.version != 2 {
            bail!("unsupported stock-Cargo capture version");
        }
        let rustdoc = OsString::from_vec(session.rustdoc);
        let (program, arguments, role) = if wrapper.file_name() == Some(OsStr::new(RUSTC_WRAPPER)) {
            let (program, arguments) = arguments
                .split_first()
                .ok_or_else(|| eyre::eyre!("Cargo did not supply a compiler executable"))?;
            (program, arguments, "rustc-wrapper")
        } else {
            (&rustdoc, arguments, "rustdoc")
        };
        let native_rustdoc = session.nix.is_some() && role == "rustdoc";
        let execution = if native_rustdoc {
            if Self::is_doctest(role, arguments) {
                Execution::NativeDoctest
            } else {
                Execution::NativeRustdoc
            }
        } else if session.nix.is_some() {
            if Backend::is_probe(program, arguments) {
                Execution::NativeProbe
            } else {
                Execution::NixCompiler
            }
        } else {
            Execution::NativeExecNotNix
        };
        let mut environment_names: Vec<_> = env::vars_os()
            .map(|(name, _)| name.as_bytes().to_vec())
            .collect();
        environment_names.sort();
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let observation = serde_json::json!({
            "version": 1,
            "role": role,
            "pid": process::id(),
            "capturedUnixNanoseconds": timestamp.to_string(),
            "program": program.as_bytes(),
            "arguments": arguments.iter().map(|arg| arg.as_bytes()).collect::<Vec<_>>(),
            "cwd": env::current_dir()?.as_os_str().as_bytes(),
            // Do not serialize arbitrary environment values (including licenses).
            // Effective-environment replay requires a separate explicit contract.
            "environmentNames": environment_names,
            "jobserver": env::var_os("CARGO_MAKEFLAGS").map(|value| value.as_bytes().to_vec()),
            "execution": execution,
        });
        let destination = root
            .join("invocations")
            .join(format!("{}-{timestamp}.json", process::id()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&destination)?;
        serde_json::to_writer(&mut file, &observation)?;
        file.write_all(b"\n")?;
        drop(file);
        if native_rustdoc {
            // Rustdoc runs as the caller, like build scripts and doctests. Its
            // dependencies still cross the immutable compiler boundary; record
            // native execution separately from Nix compiler delivery.
            let started = Instant::now();
            let status = Command::new(program).args(arguments).status()?;
            fs::write(
                destination.with_extension("native-result.json"),
                serde_json::to_vec(&serde_json::json!({
                    "execution": execution,
                    "status": status.into_raw(),
                    "seconds": started.elapsed().as_secs_f64(),
                }))?,
            )?;
            if status.success() {
                fs::write(destination.with_extension("native-complete"), b"executed\n")?;
            }
            Self::exit(status);
        }
        if let Some(backend) = session.nix
            && execution == Execution::NixCompiler
        {
            let backend = backend.prepare(root)?;
            let status = backend.execute(&destination, program, arguments, role)?;
            // Build scripts may swallow wrapper failures as negative capability
            // probes. Only completed delivery permits the outer Cargo success.
            fs::write(destination.with_extension("nix-complete"), b"delivered\n")?;
            Self::exit(status);
        }
        // exec preserves Cargo's descriptors, stdin, streaming output, signals,
        // cwd and environment. It adds no shell, output buffering or child waiter.
        let error = Command::new(program).args(arguments).exec();
        Err(error).wrap_err("executing Cargo's supplied compiler command")
    }

    fn is_doctest(role: &str, arguments: &[OsString]) -> bool {
        role == "rustdoc" && arguments.iter().any(|arg| arg == "--test")
    }

    fn run_cargo(&self, root: &Path, command: &mut Command) -> eyre::Result<ExitStatus> {
        if root.join("cargo-result.json").try_exists()? {
            bail!("a completed Cargo session cannot be reused");
        }
        let status = command.status();
        let mut nix_invocations = 0;
        let mut incomplete = None;
        for entry in fs::read_dir(root.join("invocations"))? {
            let path = entry?.path();
            if path.extension() != Some(OsStr::new("json")) {
                continue;
            }
            let record: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
            if record.get("version") != Some(&serde_json::json!(1)) {
                continue; // Nix delivery receipts have no observation version.
            }
            let execution = Execution::deserialize(&record["execution"])?;
            if execution == Execution::NixCompiler {
                nix_invocations += 1;
            }
            if status.as_ref().is_ok_and(ExitStatus::success)
                && let Some((extension, expected)) = execution.completion()
                && fs::read(path.with_extension(extension)).ok().as_deref() != Some(expected)
            {
                incomplete.get_or_insert(path);
            }
        }
        if incomplete.is_some()
            && let Some(backend) = &self.nix
        {
            backend.reject_incomplete()?;
        }
        Backend::write_json(
            &root.join("cargo-result.json"),
            &Completion {
                version: 2,
                status: if incomplete.is_some() {
                    1 << 8
                } else {
                    status.as_ref().map_or(1 << 8, |status| status.into_raw())
                },
                nix_invocations,
            },
        )?;
        if let Some(path) = incomplete {
            bail!(
                "Cargo succeeded after an incomplete Nix compiler invocation: {}; a build script may have swallowed a backend error",
                path.display()
            );
        }
        status.wrap_err("executing unmodified Cargo")
    }

    fn snapshot_references(arguments: &[OsString]) -> eyre::Result<()> {
        let [directory, root, output, destination] = arguments else {
            bail!(
                "usage: snapshot-references --directory COMPLETED_NIX_SESSION --output NEW_JSON_FILE"
            );
        };
        if directory != "--directory" || output != "--output" {
            bail!("snapshot-references requires --directory and --output");
        }
        let root = Path::new(root);
        if !root.is_absolute() || root.canonicalize()? != root {
            bail!("snapshot session must be a normalized absolute directory");
        }
        for name in ["session.json", "cargo-result.json"] {
            if !root.join(name).symlink_metadata()?.is_file() {
                bail!("snapshot session records must be regular files");
            }
        }
        let session: Self = serde_json::from_slice(&fs::read(root.join("session.json"))?)?;
        let completion: Completion =
            serde_json::from_slice(&fs::read(root.join("cargo-result.json"))?)?;
        if session.version != 2 || completion.version != 2 || completion.status != 0 {
            bail!("snapshot references require a successfully completed Cargo session");
        }
        let backend = session
            .nix
            .ok_or_else(|| eyre::eyre!("snapshot references require Nix artifact ownership"))?;
        // Reuse the completed session's owner. Do not prepare another source
        // import, contact a daemon or realize an absent backing output.
        Backend::write_json(Path::new(destination), &backend.snapshot_references()?)
    }

    fn exit(status: ExitStatus) -> ! {
        if let Some(signal) = status.signal() {
            // Preserve the child's signal status after the supervising wait.
            // SAFETY: reset and raise a signal reported by waitpid in this process.
            unsafe {
                libc::signal(signal, libc::SIG_DFL);
                libc::raise(signal);
            }
        }
        process::exit(status.code().unwrap_or(1));
    }
}

/// Dispatch before UTF-8 CLI parsing: compiler arguments may contain native bytes.
pub(crate) fn dispatch(arguments: &[OsString]) -> Option<eyre::Result<()>> {
    let executable = Path::new(arguments.first()?);
    if matches!(
        executable.file_name().and_then(OsStr::to_str),
        Some(RUSTC_WRAPPER | RUSTDOC_WRAPPER)
    ) {
        return Some(Session::execute(executable, &arguments[1..]));
    }
    if arguments.get(1).is_some_and(|arg| arg == "stock-compile") {
        return Some(Backend::build(&arguments[2..]));
    }
    if arguments
        .get(1)
        .is_some_and(|arg| arg == "snapshot-references")
    {
        return Some(Session::snapshot_references(&arguments[2..]));
    }
    if arguments
        .get(1)
        .is_some_and(|arg| arg == "capture-cargo" || arg == "nix-cargo")
    {
        return Some(capture(&arguments[2..], arguments[1] == "nix-cargo"));
    }
    None
}

fn capture(arguments: &[OsString], use_nix: bool) -> eyre::Result<()> {
    let mut directory = None;
    let mut cargo = None;
    let mut rustdoc = None;
    let mut nix_config = None;
    let mut remaining = arguments;
    while remaining.first().is_some_and(|arg| arg != "--") {
        let [name, value, rest @ ..] = remaining else {
            bail!("capture option requires a value");
        };
        let slot = match name.to_str() {
            Some("--directory") => &mut directory,
            Some("--cargo") => &mut cargo,
            Some("--rustdoc") => &mut rustdoc,
            Some("--nix-config") => &mut nix_config,
            _ => bail!("unknown capture option"),
        };
        if slot.replace(PathBuf::from(value)).is_some() {
            bail!("repeated capture option");
        }
        remaining = rest;
    }
    let (Some(directory), Some(cargo), Some(rustdoc), [separator, caller @ ..]) =
        (directory, cargo, rustdoc, remaining)
    else {
        bail!(
            "usage: capture-cargo --directory NEW_ABSOLUTE_DIRECTORY --cargo STOCK_CARGO --rustdoc STOCK_RUSTDOC -- CARGO_ARGUMENTS"
        );
    };
    if separator != "--" || caller.is_empty() || !cargo.is_absolute() || !cargo.is_file() {
        bail!("an absolute Cargo executable and Cargo arguments after -- are required");
    }
    if env::var_os("RUSTC_WRAPPER").is_some_and(|value| !value.is_empty()) {
        bail!("capture-cargo requires exclusive ownership of RUSTC_WRAPPER");
    }
    if use_nix != nix_config.is_some() {
        bail!("nix-cargo requires --nix-config; capture-cargo does not accept it");
    }
    let session = Session::create(&directory, &rustdoc, nix_config.as_deref())?;
    let mut command = Command::new(cargo);
    command
        .args(caller)
        .env("RUSTC_WRAPPER", directory.join("bin").join(RUSTC_WRAPPER))
        .env("RUSTDOC", directory.join("bin").join(RUSTDOC_WRAPPER));
    if use_nix {
        Session::exit(session.run_cargo(&directory, &mut command)?);
    }
    let error = command.exec();
    Err(error).wrap_err("executing unmodified Cargo")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let root = env::temp_dir().join(format!(
                "dyndrv-cargo-completion-{}-{}",
                process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir_all(root.join("invocations")).unwrap();
            Self(root)
        }

        fn record(&self, name: &str, execution: Execution) -> PathBuf {
            let path = self.0.join("invocations").join(format!("{name}.json"));
            Backend::write_json(
                &path,
                &serde_json::json!({
                    "version": 1, "execution": execution,
                }),
            )
            .unwrap();
            path
        }

        fn run(&self, status: u8) -> eyre::Result<ExitStatus> {
            let mut command = Command::new("/bin/sh");
            command.args(["-c", &format!("exit {status}")]);
            Session {
                version: 2,
                rustdoc: vec![],
                nix: None,
            }
            .run_cargo(&self.0, &mut command)
        }

        fn completion(&self) -> Completion {
            serde_json::from_slice(&fs::read(self.0.join("cargo-result.json")).unwrap()).unwrap()
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn completion_counts_actual_immutable_invocations_including_negative_probes() {
        let fixture = Fixture::new();
        let positive = fixture.record("positive", Execution::NixCompiler);
        let negative = fixture.record("negative", Execution::NixCompiler);
        fixture.record("version-probe", Execution::NativeProbe);
        for path in [&positive, &negative] {
            fs::write(path.with_extension("nix-complete"), b"delivered\n").unwrap();
        }
        // A completed compiler error is a real immutable invocation. Result
        // files alone cannot distinguish it from early/unfinished metadata.
        Backend::write_json(
            &negative.with_extension("nix-result.json"),
            &serde_json::json!({"status": 1 << 8}),
        )
        .unwrap();
        for (name, execution) in [
            ("doc", Execution::NativeRustdoc),
            ("doctest", Execution::NativeDoctest),
        ] {
            let path = fixture.record(name, execution);
            fs::write(path.with_extension("native-complete"), b"executed\n").unwrap();
        }
        assert!(fixture.run(0).unwrap().success());
        let completion = fixture.completion();
        assert_eq!(completion.version, 2);
        assert_eq!(completion.status, 0);
        assert_eq!(completion.nix_invocations, 2);
        assert!(
            fixture.run(0).is_err(),
            "completed sessions cannot be reused"
        );
    }

    #[test]
    fn swallowed_preparation_and_early_metadata_failures_remain_incomplete() {
        for metadata in [false, true] {
            let fixture = Fixture::new();
            let path = fixture.record("incomplete", Execution::NixCompiler);
            if metadata {
                // Neither an early result nor a native completion establishes
                // completed immutable compiler delivery.
                Backend::write_json(
                    &path.with_extension("nix-result.json"),
                    &serde_json::json!({"metadata": true}),
                )
                .unwrap();
                fs::write(path.with_extension("native-complete"), b"executed\n").unwrap();
            }
            assert!(fixture.run(0).is_err());
            let completion = fixture.completion();
            assert_ne!(completion.status, 0);
            assert_eq!(completion.nix_invocations, 1);
        }
    }

    #[test]
    fn native_completion_and_failed_cargo_keep_distinct_counts() {
        for execution in [Execution::NativeRustdoc, Execution::NativeDoctest] {
            let fixture = Fixture::new();
            let path = fixture.record("native", execution);
            fs::write(path.with_extension("nix-complete"), b"delivered\n").unwrap();
            assert!(fixture.run(0).is_err());
            assert_eq!(fixture.completion().nix_invocations, 0);
        }
        let fixture = Fixture::new();
        fixture.record("incomplete", Execution::NixCompiler);
        assert_eq!(fixture.run(23).unwrap().code(), Some(23));
        assert_eq!(fixture.completion().status, 23 << 8);
        assert_eq!(fixture.completion().nix_invocations, 1);
    }

    #[test]
    fn cargo_spawn_failure_records_an_unsuccessful_empty_session() {
        let fixture = Fixture::new();
        let session = Session {
            version: 2,
            rustdoc: vec![],
            nix: None,
        };
        let mut command = Command::new(fixture.0.join("absent-cargo"));
        assert!(session.run_cargo(&fixture.0, &mut command).is_err());
        assert_eq!(fixture.completion().status, 1 << 8);
        assert_eq!(fixture.completion().nix_invocations, 0);
    }

    #[test]
    fn snapshot_references_reject_unfinished_failed_and_native_sessions() {
        let root = std::env::temp_dir().join(format!(
            "dyndrv-snapshot-session-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        Backend::write_json(
            &root.join("session.json"),
            &Session {
                version: 2,
                rustdoc: vec![],
                nix: None,
            },
        )
        .unwrap();
        let output = root.join("references.json");
        let arguments = [
            "--directory".into(),
            root.clone().into_os_string(),
            "--output".into(),
            output.clone().into_os_string(),
        ];
        assert!(Session::snapshot_references(&arguments).is_err());
        for status in [256, 0] {
            Backend::write_json(
                &root.join("cargo-result.json"),
                &Completion {
                    version: 2,
                    status,
                    nix_invocations: 0,
                },
            )
            .unwrap();
            assert!(Session::snapshot_references(&arguments).is_err());
            assert!(!output.exists());
            fs::remove_file(root.join("cargo-result.json")).unwrap();
        }
        fs::remove_dir_all(root).unwrap();
    }
}
