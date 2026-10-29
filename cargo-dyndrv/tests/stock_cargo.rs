//! Process-boundary controls for the unmodified-Cargo driver. Real Cargo coverage
//! is measured separately; these isolate argv, IO, descriptors and failure status.
use std::env;
use std::ffi::OsString;
use std::fs;
use std::io::ErrorKind;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs as unix_fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = env::temp_dir().join(format!(
            "dyndrv-stock-process-{}-{}",
            process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let result = Self(root);
        result.script(
            "cargo",
            "case \"$1\" in\nrustc) shift; exec \"$RUSTC_WRAPPER\" \"$@\";;\nrustdoc) shift; exec \"$RUSTDOC\" \"$@\";;\n*) exit 97;;\nesac\n",
        );
        result
    }
    fn script(&self, name: &str, body: &str) -> PathBuf {
        let path = self.0.join(name);
        fs::write(&path, format!("#!/bin/sh\n{body}")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        path
    }
    fn capture(&self, name: &str, rustdoc: &Path) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_cargo-dyndrv"));
        command
            .env_clear()
            .env(
                "PATH",
                env::var_os("PATH").expect("test process must supply a tool PATH"),
            )
            .args(["capture-cargo", "--directory"])
            .arg(self.0.join(name))
            .arg("--cargo")
            .arg(self.0.join("cargo"))
            .arg("--rustdoc")
            .arg(rustdoc)
            .arg("--")
            .current_dir(&self.0);
        command
    }
    fn record(&self, name: &str) -> serde_json::Value {
        let entries = fs::read_dir(self.0.join(name).join("invocations"))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(entries.len(), 1);
        serde_json::from_slice(&fs::read(entries[0].path()).unwrap()).unwrap()
    }

    fn nix_config(&self) -> (UnixListener, serde_json::Value) {
        fs::set_permissions(&self.0, fs::Permissions::from_mode(0o700)).unwrap();
        let socket = self.0.join("daemon.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let source = self.0.join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("lib.rs"), "pub fn source() {}\n").unwrap();
        // These fixtures never execute the Nix CLI. Reuse a declared immutable
        // executable so their no-preparation checks also work in the Nix sandbox.
        let nix = PathBuf::from(env!("BUILD_WRAP")).canonicalize().unwrap();
        let config = serde_json::json!({
            "nix": nix, "socket": socket, "real_store": self.0.join("store"),
            "source": source, "target": self.0.join("target"),
            "inputs": [nix], "environment_names": [], "omitted_environment_names": [],
        });
        (listener, config)
    }

    fn nix_cargo(&self, name: &str, config: &serde_json::Value, rustdoc: &Path) -> Command {
        let path = self.0.join(format!("{name}-config.json"));
        fs::write(&path, serde_json::to_vec(config).unwrap()).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_cargo-dyndrv"));
        command
            .env_clear()
            .env("PATH", env::var_os("PATH").unwrap())
            .args(["nix-cargo", "--directory"])
            .arg(self.0.join(name))
            .arg("--cargo")
            .arg(self.0.join("cargo"))
            .arg("--rustdoc")
            .arg(rustdoc)
            .arg("--nix-config")
            .arg(path)
            .arg("--")
            .current_dir(&self.0);
        command
    }

    fn assert_unprepared(&self, name: &str, listener: &UnixListener) {
        assert_eq!(listener.accept().unwrap_err().kind(), ErrorKind::WouldBlock);
        let root = self.0.join(name);
        for filename in [
            "prepare.lock",
            "prepare-started.json",
            "prepare.json",
            "compiler-inputs.json",
        ] {
            assert!(!root.join(filename).exists(), "unexpected {filename}");
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn compiler_preserves_native_arguments_streams_and_private_inheritance() {
    let fixture = Fixture::new();
    let compiler = fixture.script(
        "compiler",
        "printf '%s\\000' \"$@\"; printf '%s' \"$PRIVATE_TEST_VALUE\"; cat; printf diagnostic >&2\n",
    );
    let argument = OsString::from_vec(b"space newline\nnonutf8\xff".to_vec());
    let input = fixture.0.join("stdin");
    fs::write(&input, b"input-stream").unwrap();
    let mut direct = Command::new(&compiler);
    direct
        .env_clear()
        .env(
            "PATH",
            env::var_os("PATH").expect("test process must supply a tool PATH"),
        )
        .env("PRIVATE_TEST_VALUE", "private-value-never-recorded")
        .arg(&argument)
        .stdin(fs::File::open(&input).unwrap());
    let expected = direct.output().unwrap();
    let actual = fixture
        .capture("capture", &compiler)
        .args(["rustc"])
        .arg(&compiler)
        .arg(&argument)
        .env("PRIVATE_TEST_VALUE", "private-value-never-recorded")
        .stdin(fs::File::open(&input).unwrap())
        .output()
        .unwrap();
    assert!(actual.status.success());
    assert_eq!(actual.stdout, expected.stdout);
    assert_eq!(actual.stderr, expected.stderr);
    let record = fixture.record("capture");
    assert_eq!(
        record["arguments"],
        serde_json::json!([argument.into_vec()])
    );
    assert!(
        !serde_json::to_string(&record)
            .unwrap()
            .contains("private-value-never-recorded")
    );
}

#[test]
fn rustdoc_dispatch_does_not_insert_a_compiler_argument() {
    let fixture = Fixture::new();
    let rustdoc = fixture.script("rustdoc", "printf '%s\\000' \"$@\"\n");
    let actual = fixture
        .capture("doc", &rustdoc)
        .args(["rustdoc", "--test", "a source.rs"])
        .output()
        .unwrap();
    assert!(actual.status.success());
    assert_eq!(actual.stdout, b"--test\0a source.rs\0");
    assert_eq!(fixture.record("doc")["role"], "rustdoc");
}

#[test]
fn child_exit_codes_and_signals_are_preserved() {
    let fixture = Fixture::new();
    for (name, body, code, signal) in [
        ("exit23", "exit 23\n", Some(23), None),
        ("signal", "kill -TERM $$\n", None, Some(15)),
    ] {
        let compiler = fixture.script(name, body);
        let status = fixture
            .capture(&format!("capture-{name}"), &compiler)
            .arg("rustc")
            .arg(&compiler)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert_eq!(status.code(), code);
        assert_eq!(status.signal(), signal);
        fixture.record(&format!("capture-{name}"));
    }
}

#[test]
fn inherited_descriptor_and_jobserver_word_survive_exec() {
    let fixture = Fixture::new();
    fs::write(fixture.0.join("descriptor-input"), b"inherited-descriptor").unwrap();
    fixture.script(
        "cargo",
        "exec 3<descriptor-input\nexport CARGO_MAKEFLAGS='--jobserver-auth=3,4'\nshift\nexec \"$RUSTC_WRAPPER\" \"$@\"\n",
    );
    let compiler = fixture.script("compiler", "cat <&3; printf '%s' \"$CARGO_MAKEFLAGS\"\n");
    let actual = fixture
        .capture("descriptor", &compiler)
        .arg("rustc")
        .arg(&compiler)
        .output()
        .unwrap();
    assert!(actual.status.success());
    assert_eq!(actual.stdout, b"inherited-descriptor--jobserver-auth=3,4");
    assert_eq!(
        fixture.record("descriptor")["jobserver"],
        serde_json::json!(b"--jobserver-auth=3,4".to_vec())
    );
}

#[test]
fn conflicting_wrapper_and_existing_capture_are_rejected() {
    let fixture = Fixture::new();
    let compiler = fixture.script("compiler", "exit 0\n");
    let occupied = fixture.0.join("occupied");
    fs::create_dir(&occupied).unwrap();
    fs::write(occupied.join("sentinel"), b"unchanged").unwrap();
    for name in ["occupied", "conflict"] {
        let mut command = fixture.capture(name, &compiler);
        command.arg("rustc").arg(&compiler);
        if name == "conflict" {
            command.env("RUSTC_WRAPPER", "/existing/wrapper");
        }
        let result = command.output().unwrap();
        assert!(!result.status.success());
    }
    assert_eq!(fs::read(occupied.join("sentinel")).unwrap(), b"unchanged");
    assert!(!fixture.0.join("conflict").exists());
}

#[test]
fn valid_noop_queries_and_native_documentation_never_prepare_compiler_inputs() {
    let fixture = Fixture::new();
    let (listener, config) = fixture.nix_config();
    let compiler = fixture.script("rustc", "printf native\n");
    let rustdoc = fixture.script("rustdoc", "printf native-doc\n");
    fixture.script("cargo", "case \"$1\" in\nnoop) exit 0;;\nrustc) shift; exec \"$RUSTC_WRAPPER\" \"$@\";;\nrustdoc) shift; exec \"$RUSTDOC\" \"$@\";;\nesac\n");
    for (name, args) in [
        ("noop", vec!["noop"]),
        ("version", vec!["rustc", compiler.to_str().unwrap(), "-vV"]),
        (
            "print",
            vec![
                "rustc",
                compiler.to_str().unwrap(),
                "--print",
                "cfg",
                "--target",
                "fixture",
            ],
        ),
        ("doc", vec!["rustdoc", "lib.rs"]),
        ("doctest", vec!["rustdoc", "--test", "lib.rs"]),
    ] {
        let result = fixture
            .nix_cargo(name, &config, &rustdoc)
            .args(args)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        fixture.assert_unprepared(name, &listener);
        let completion: serde_json::Value = serde_json::from_slice(
            &fs::read(fixture.0.join(name).join("cargo-result.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            completion,
            serde_json::json!({"version":2, "status":0, "nix_invocations":0})
        );
    }
}

#[test]
fn invalid_policy_and_rejected_targets_fail_before_cargo_without_preparation() {
    let fixture = Fixture::new();
    let (listener, config) = fixture.nix_config();
    let rustdoc = fixture.script("rustdoc", "exit 0\n");
    fixture.script("cargo", "touch cargo-started\nexit 0\n");
    let public_roots = fixture.0.join("public-roots");
    fs::create_dir(&public_roots).unwrap();
    fs::set_permissions(&public_roots, fs::Permissions::from_mode(0o755)).unwrap();
    let roots_link = fixture.0.join("roots-link");
    unix_fs::symlink(&fixture.0, &roots_link).unwrap();
    for (name, field, value) in [
        ("private-target", "private_target", serde_json::json!(true)),
        (
            "store-links",
            "artifact_delivery",
            serde_json::json!("system-store-rlib-links"),
        ),
        ("no-inputs", "inputs", serde_json::json!([])),
        (
            "missing-root-policy",
            "gc_roots",
            serde_json::json!(fixture.0.join("missing-roots")),
        ),
        (
            "public-root-policy",
            "gc_roots",
            serde_json::json!(public_roots),
        ),
        ("symlink-roots", "gc_roots", serde_json::json!(roots_link)),
        (
            "jobserver",
            "environment_names",
            serde_json::json!(["CARGO_MAKEFLAGS"]),
        ),
        (
            "overlap",
            "package_sources",
            serde_json::json!([config["source"]]),
        ),
        ("rejected", "source_overlay", serde_json::json!(false)),
    ] {
        let mut invalid = config.clone();
        invalid[field] = value;
        if name == "rejected" {
            fs::create_dir_all(fixture.0.join("target/.dyndrv-delivered")).unwrap();
            fs::write(
                fixture.0.join("target/.dyndrv-delivered/rejected"),
                b"rejected\n",
            )
            .unwrap();
        }
        let result = fixture
            .nix_cargo(name, &invalid, &rustdoc)
            .arg("noop")
            .output()
            .unwrap();
        assert!(!result.status.success(), "invalid {name} was accepted");
        assert!(!fixture.0.join("cargo-started").exists());
        fixture.assert_unprepared(name, &listener);
    }
}

#[test]
fn swallowed_lazy_initialization_failure_is_counted_and_rejects_the_target() {
    let fixture = Fixture::new();
    let (listener, config) = fixture.nix_config();
    let compiler = fixture.script("rustc", "exit 0\n");
    fixture.script("cargo", "session=${RUSTC_WRAPPER%/bin/*}\nprintf 1 >\"$session/prepare-started.json\"\n\"$RUSTC_WRAPPER\" \"$1\" source.rs\nexit 0\n");
    let result = fixture
        .nix_cargo("swallowed", &config, &compiler)
        .arg(&compiler)
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("incomplete Nix compiler invocation"));
    assert_eq!(listener.accept().unwrap_err().kind(), ErrorKind::WouldBlock);
    let root = fixture.0.join("swallowed");
    let completion: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("cargo-result.json")).unwrap()).unwrap();
    assert_eq!(
        completion,
        serde_json::json!({"version":2, "status":256, "nix_invocations":1})
    );
    assert!(!root.join("compiler-inputs.json").exists());
    assert!(
        fixture
            .0
            .join("target/.dyndrv-delivered/rejected")
            .is_file()
    );
    let rejected = fixture
        .nix_cargo("rejected-after-swallow", &config, &compiler)
        .arg("noop")
        .output()
        .unwrap();
    assert!(!rejected.status.success());
    fixture.assert_unprepared("rejected-after-swallow", &listener);
}
