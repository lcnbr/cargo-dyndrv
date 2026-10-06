//! Drive unmodified Cargo at its process boundary. Capture mode observes native
//! execution; Nix mode binds actual commands to immutable compiler inputs. Cargo
//! retains command construction, scheduling, build scripts and target ownership.
mod nix;
use color_eyre::eyre::{self, Context, bail};
use serde::{Deserialize, Serialize};
use std::{
    ffi::{OsStr, OsString},
    fs::{self, DirBuilder, OpenOptions},
    io::Write,
    os::unix::{
        ffi::{OsStrExt, OsStringExt},
        fs::{DirBuilderExt, OpenOptionsExt, symlink},
        process::{CommandExt, ExitStatusExt},
    },
    path::{Path, PathBuf},
    process::{Command, ExitStatus},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

const RUSTC_WRAPPER: &str = "dyndrv-stock-rustc";
const RUSTDOC_WRAPPER: &str = "dyndrv-stock-rustdoc";

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Session {
    version: u32,
    rustdoc: Vec<u8>,
    #[serde(default)]
    nix: Option<nix::Prepared>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Completion {
    version: u32,
    status: i32,
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
            version: 1,
            rustdoc: rustdoc.as_os_str().as_bytes().to_vec(),
            nix: config.map(nix::Prepared::prepare).transpose()?,
        };
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(root.join("session.json"))?;
        serde_json::to_writer(&mut file, &session)?;
        file.write_all(b"\n")?;
        let executable = std::env::current_exe()?;
        for name in [RUSTC_WRAPPER, RUSTDOC_WRAPPER] {
            symlink(&executable, root.join("bin").join(name))?;
        }
        Ok(session)
    }

    fn execute(wrapper: &Path, arguments: &[OsString]) -> eyre::Result<()> {
        let root = wrapper
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| eyre::eyre!("wrapper lacks a capture directory"))?;
        let session: Self = serde_json::from_slice(&fs::read(root.join("session.json"))?)?;
        if session.version != 1 {
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
        let native_doctest = session.nix.is_some() && Self::is_doctest(role, arguments);
        let mut environment_names: Vec<_> = std::env::vars_os()
            .map(|(name, _)| name.as_bytes().to_vec())
            .collect();
        environment_names.sort();
        let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
        let observation = serde_json::json!({
            "version": 1,
            "role": role,
            "pid": std::process::id(),
            "capturedUnixNanoseconds": timestamp.to_string(),
            "program": program.as_bytes(),
            "arguments": arguments.iter().map(|arg| arg.as_bytes()).collect::<Vec<_>>(),
            "cwd": std::env::current_dir()?.as_os_str().as_bytes(),
            // Do not serialize arbitrary environment values (including licenses).
            // Effective-environment replay requires a separate explicit contract.
            "environmentNames": environment_names,
            "jobserver": std::env::var_os("CARGO_MAKEFLAGS").map(|value| value.as_bytes().to_vec()),
            "execution": if native_doctest { "native-doctest" } else if session.nix.is_some() { "nix-request-or-native-probe" } else { "native-exec-not-nix" },
        });
        let destination = root
            .join("invocations")
            .join(format!("{}-{timestamp}.json", std::process::id()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&destination)?;
        serde_json::to_writer(&mut file, &observation)?;
        file.write_all(b"\n")?;
        drop(file);
        if native_doctest {
            // Stable rustdoc has no compiler-wrapper hook. Keep its compilation
            // and runtime together, execute on every call, and measure this
            // explicitly native stage without serializing private runtime env.
            let started = Instant::now();
            let status = Command::new(program).args(arguments).status()?;
            fs::write(
                destination.with_extension("native-result.json"),
                serde_json::to_vec(&serde_json::json!({
                    "execution": "native-doctest",
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
            && !nix::Prepared::is_probe(program, arguments)
        {
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
        let status = command.status().wrap_err("executing unmodified Cargo")?;
        if status.success() {
            for entry in fs::read_dir(root.join("invocations"))? {
                let path = entry?.path();
                if path.extension() != Some(OsStr::new("json")) {
                    continue;
                }
                let record: serde_json::Value = serde_json::from_slice(&fs::read(&path)?)?;
                if record.get("version") != Some(&serde_json::json!(1)) {
                    continue; // Nix delivery receipts have no observation version.
                }
                let arguments: Vec<Vec<u8>> = serde_json::from_value(record["arguments"].clone())?;
                let arguments: Vec<_> = arguments.into_iter().map(OsString::from_vec).collect();
                let program =
                    OsString::from_vec(serde_json::from_value(record["program"].clone())?);
                let (extension, expected): (&str, &[u8]) =
                    if Self::is_doctest(record["role"].as_str().unwrap_or_default(), &arguments) {
                        ("native-complete", b"executed\n")
                    } else {
                        ("nix-complete", b"delivered\n")
                    };
                if !nix::Prepared::is_probe(&program, &arguments)
                    && fs::read(path.with_extension(extension)).ok().as_deref() != Some(expected)
                {
                    if let Some(backend) = &self.nix {
                        backend.reject_incomplete()?;
                    }
                    bail!(
                        "Cargo succeeded after an incomplete Nix compiler invocation: {}; a build script may have swallowed a backend error",
                        path.display()
                    );
                }
            }
        }
        nix::Prepared::write_json(
            &root.join("cargo-result.json"),
            &Completion {
                version: 1,
                status: status.into_raw(),
            },
        )?;
        Ok(status)
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
        if session.version != 1 || completion.version != 1 || completion.status != 0 {
            bail!("snapshot references require a successfully completed Cargo session");
        }
        let backend = session
            .nix
            .ok_or_else(|| eyre::eyre!("snapshot references require Nix artifact ownership"))?;
        // Reuse the completed session's owner. Do not prepare another source
        // import, contact a daemon or realize an absent backing output.
        nix::Prepared::write_json(Path::new(destination), &backend.snapshot_references()?)
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
        std::process::exit(status.code().unwrap_or(1));
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
        return Some(nix::Prepared::build(&arguments[2..]));
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
    if std::env::var_os("RUSTC_WRAPPER").is_some_and(|value| !value.is_empty()) {
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

    #[test]
    fn cargo_success_requires_every_compilation_to_complete() {
        let root = std::env::temp_dir().join(format!(
            "dyndrv-cargo-completion-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(root.join("invocations")).unwrap();
        let record = root.join("invocations/1-1.json");
        fs::write(
            &record,
            serde_json::to_vec(&serde_json::json!({
                "version": 1, "program": b"rustc".as_slice(), "arguments": [b"probe.rs".as_slice()]
            }))
            .unwrap(),
        )
        .unwrap();
        let mut success = Command::new("/bin/sh");
        success.args(["-c", "exit 0"]);
        let session = Session {
            version: 1,
            rustdoc: vec![],
            nix: None,
        };
        assert!(session.run_cargo(&root, &mut success).is_err());
        fs::write(record.with_extension("nix-complete"), b"delivered\n").unwrap();
        assert!(session.run_cargo(&root, &mut success).unwrap().success());
        let completed: Completion =
            serde_json::from_slice(&fs::read(root.join("cargo-result.json")).unwrap()).unwrap();
        assert_eq!(completed.status, 0);
        assert!(session.run_cargo(&root, &mut success).is_err());
        fs::remove_file(root.join("cargo-result.json")).unwrap();
        fs::remove_file(record.with_extension("nix-complete")).unwrap();
        fs::write(
            &record,
            serde_json::to_vec(&serde_json::json!({
                "version": 1, "program": b"rustc".as_slice(), "arguments": [b"-vV".as_slice()]
            }))
            .unwrap(),
        )
        .unwrap();
        assert!(session.run_cargo(&root, &mut success).unwrap().success());
        fs::remove_file(root.join("cargo-result.json")).unwrap();
        fs::write(
            &record,
            serde_json::to_vec(&serde_json::json!({
                "version": 1, "role": "rustdoc", "program": b"rustdoc".as_slice(),
                "arguments": [b"--test".as_slice(), b"lib.rs".as_slice()]
            }))
            .unwrap(),
        )
        .unwrap();
        // Cached compiler delivery cannot stand in for actual doctest execution.
        fs::write(record.with_extension("nix-complete"), b"delivered\n").unwrap();
        assert!(session.run_cargo(&root, &mut success).is_err());
        fs::write(record.with_extension("native-complete"), b"executed\n").unwrap();
        assert!(session.run_cargo(&root, &mut success).unwrap().success());
        fs::remove_file(root.join("cargo-result.json")).unwrap();
        let mut failed = Command::new("/bin/sh");
        failed.args(["-c", "exit 23"]);
        assert_eq!(
            session.run_cargo(&root, &mut failed).unwrap().code(),
            Some(23)
        );
        fs::remove_dir_all(root).unwrap();
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
        nix::Prepared::write_json(
            &root.join("session.json"),
            &Session {
                version: 1,
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
            nix::Prepared::write_json(
                &root.join("cargo-result.json"),
                &Completion { version: 1, status },
            )
            .unwrap();
            assert!(Session::snapshot_references(&arguments).is_err());
            assert!(!output.exists());
            fs::remove_file(root.join("cargo-result.json")).unwrap();
        }
        fs::remove_dir_all(root).unwrap();
    }
}
