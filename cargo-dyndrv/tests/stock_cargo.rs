//! Process-boundary controls for the unmodified-Cargo driver. Real Cargo coverage
//! is measured separately; these isolate argv, IO, descriptors and failure status.
use std::{
    ffi::OsString,
    fs,
    os::unix::{ffi::OsStringExt, fs::PermissionsExt, process::ExitStatusExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{SystemTime, UNIX_EPOCH},
};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let root = std::env::temp_dir().join(format!(
            "dyndrv-stock-process-{}-{}",
            std::process::id(),
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
                std::env::var_os("PATH").expect("test process must supply a tool PATH"),
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
            std::env::var_os("PATH").expect("test process must supply a tool PATH"),
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
