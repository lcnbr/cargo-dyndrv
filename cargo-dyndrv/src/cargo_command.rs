//! Shared caller policy for unit-graph and package-metadata discovery.
use crate::package_metadata::Metadata;
use cargo_metadata::MetadataCommand;
use std::{
    path::Path,
    process::{Command, Output, Stdio},
};

pub(crate) struct CargoInvocation<'a> {
    cargo_path: &'a Path,
    caller_args: &'a [String],
}

impl<'a> CargoInvocation<'a> {
    pub(crate) fn new(cargo_path: &'a Path, caller_args: &'a [String]) -> Self {
        Self {
            cargo_path,
            caller_args,
        }
    }

    pub(crate) fn unit_graph_command(&self) -> Command {
        let mut command = Command::new(self.cargo_path);
        command
            .args(self.caller_args)
            .args(["-Z", "unstable-options", "--unit-graph"])
            .stderr(Stdio::inherit());
        command
    }

    pub(crate) fn metadata(&self) -> Result<Metadata, cargo_metadata::Error> {
        Self::execute_metadata(&mut self.metadata_command().cargo_command())
    }

    pub(crate) fn metadata_command(&self) -> MetadataCommand {
        let mut command = MetadataCommand::new();
        command.cargo_path(self.cargo_path);
        // Preserve the existing separated manifest-option behavior. The
        // preceding graph command remains Cargo's authoritative CLI parser.
        let mut index = 0;
        while index + 1 < self.caller_args.len() {
            if matches!(self.caller_args[index].as_str(), "-m" | "--manifest-path") {
                command.manifest_path(self.caller_args[index + 1].clone());
                index += 1;
            }
            index += 1;
        }
        // These are zero-argument global flags in the pinned Cargo CLI.
        // Preserve exact spelling/order/duplicates. Do not infer settings from
        // environment/config or interpret rustc/rustdoc arguments after '--'.
        let policy = self
            .caller_args
            .iter()
            .take_while(|arg| arg.as_str() != "--")
            .filter(|arg| matches!(arg.as_str(), "--locked" | "--offline" | "--frozen"))
            .cloned()
            .collect::<Vec<_>>();
        command.other_options(policy);
        command
    }

    // Keep the existing nonverbose MetadataCommand::exec process/output behavior,
    // retaining the selected raw JSON bytes before typed versions lose spelling.
    fn execute_metadata(command: &mut Command) -> Result<Metadata, cargo_metadata::Error> {
        Self::decode_metadata(command.output()?)
    }

    fn decode_metadata(output: Output) -> Result<Metadata, cargo_metadata::Error> {
        if !output.status.success() {
            return Err(cargo_metadata::Error::CargoMetadata {
                stderr: String::from_utf8(output.stderr)?,
            });
        }
        let stdout = std::str::from_utf8(&output.stdout)?
            .lines()
            .find(|line| line.starts_with('{'))
            .ok_or(cargo_metadata::Error::NoJson)?;
        Ok(Metadata::from_slice(stdout.as_bytes())?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        ffi::OsString,
        fs,
        path::PathBuf,
        time::{Instant, SystemTime, UNIX_EPOCH},
    };

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn argv(command: &Command) -> Vec<OsString> {
        command.get_args().map(OsString::from).collect()
    }

    #[test]
    fn graph_arguments_and_unset_process_options_stay_identical() {
        for values in [
            vec!["build"],
            vec!["--offline", "build", "-m", "a path/Cargo.toml", "--locked"],
            vec!["rustc", "--frozen", "--", "-C", "opt-level=1"],
        ] {
            let caller = args(&values);
            let invocation = CargoInvocation::new(Path::new("/fixture/cargo"), &caller);
            let actual = invocation.unit_graph_command();
            let mut expected = caller.clone();
            expected.extend(args(&["-Z", "unstable-options", "--unit-graph"]));
            assert_eq!(actual.get_program(), "/fixture/cargo");
            assert_eq!(
                argv(&actual),
                expected.into_iter().map(OsString::from).collect::<Vec<_>>()
            );
            assert_eq!(actual.get_current_dir(), None);
            assert_eq!(actual.get_envs().count(), 0);
        }
    }

    #[test]
    fn metadata_preserves_literal_policy_order_and_manifest_selection() {
        for policy in [
            vec![],
            vec!["--locked"],
            vec!["--offline"],
            vec!["--frozen"],
            vec!["--offline", "--locked", "--frozen"],
            vec!["--locked", "--locked", "--offline"],
        ] {
            for manifest_flag in ["-m", "--manifest-path"] {
                let mut caller = args(&["build", manifest_flag, "a path/Cargo.toml"]);
                caller.extend(args(&policy));
                let invocation = CargoInvocation::new(Path::new("/fixture/cargo"), &caller);
                let command = invocation.metadata_command().cargo_command();
                let mut expected = args(&[
                    "metadata",
                    "--format-version",
                    "1",
                    "--manifest-path",
                    "a path/Cargo.toml",
                ]);
                expected.extend(args(&policy));
                assert_eq!(command.get_program(), "/fixture/cargo");
                assert_eq!(
                    argv(&command),
                    expected.into_iter().map(OsString::from).collect::<Vec<_>>()
                );
                assert_eq!(command.get_current_dir(), None);
                assert_eq!(command.get_envs().count(), 0);
            }
        }
    }

    #[test]
    fn policy_projection_respects_separator_and_exact_tokens() {
        let caller = args(&[
            "--offline",
            "rustc",
            "--locked",
            "--config=key=--frozen",
            "--locked=false",
            "--",
            "--frozen",
            "--offline",
        ]);
        let invocation = CargoInvocation::new(Path::new("/fixture/cargo"), &caller);
        assert_eq!(
            argv(&invocation.metadata_command().cargo_command()),
            args(&["metadata", "--format-version", "1", "--offline", "--locked"])
                .into_iter()
                .map(OsString::from)
                .collect::<Vec<_>>()
        );
    }

    struct Fixture {
        root: PathBuf,
        preserve: bool,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let directory = std::env::var_os("CARGO_DYNDRV_POLICY_TEST_ROOT");
            let preserve = directory.is_some();
            let parent = directory
                .map(PathBuf::from)
                .unwrap_or_else(std::env::temp_dir);
            let root = parent.join(format!(
                "cargo-dyndrv-policy-{name}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir(&root).unwrap();
            Self { root, preserve }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            if !self.preserve {
                let _ = fs::remove_dir_all(&self.root);
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn actual_graph_and_metadata_children_receive_complete_arguments() {
        use std::os::unix::{ffi::OsStrExt, fs::PermissionsExt};
        let fixture = Fixture::new("actual-argv");
        let cargo = fixture.root.join("cargo");
        fs::write(&cargo, r#"#!/bin/sh
case "$1" in
  metadata)
    printf '%s\000' "$@" > "${0%/*}/metadata.argv"
    printf 'metadata\n' >> "${0%/*}/query-count"
    printf '%s\n' '{"packages":[{"name":"short","version":"0.1.0","id":"short","manifest_path":"/fixture/short/Cargo.toml","targets":[],"dependencies":[],"features":{},"rust_version":"1.85"},{"name":"patch","version":"0.1.0","id":"patch","manifest_path":"/fixture/patch/Cargo.toml","targets":[],"dependencies":[],"features":{},"rust_version":"1.85.0"}],"workspace_members":[],"workspace_default_members":[],"resolve":null,"target_directory":"/fixture/target","version":1,"workspace_root":"/fixture","metadata":null}'
    ;;
  *)
    printf '%s\000' "$@" > "${0%/*}/graph.argv"
    printf '%s\n' '{"version":1,"units":[],"roots":[]}'
    ;;
esac
"#).unwrap();
        fs::set_permissions(&cargo, fs::Permissions::from_mode(0o700)).unwrap();
        let caller = args(&[
            "build",
            "--manifest-path",
            "a path/Cargo.toml",
            "--locked",
            "--offline",
            "--frozen",
        ]);
        let invocation = CargoInvocation::new(&cargo, &caller);
        assert_eq!(crate::unit_graph::discover(&invocation).unwrap().version, 1);
        let records = invocation.metadata().unwrap().packages;
        assert_eq!(
            records
                .iter()
                .map(|p| p.rust_version_env())
                .collect::<Vec<_>>(),
            ["1.85", "1.85.0"]
        );
        assert_eq!(
            records[0].package.rust_version,
            records[1].package.rust_version
        );
        assert_eq!(
            fs::read(fixture.root.join("query-count")).unwrap(),
            b"metadata\n"
        );
        let mut expected_graph = caller;
        expected_graph.extend(args(&["-Z", "unstable-options", "--unit-graph"]));
        let expected_metadata = args(&[
            "metadata",
            "--format-version",
            "1",
            "--manifest-path",
            "a path/Cargo.toml",
            "--locked",
            "--offline",
            "--frozen",
        ]);
        for (name, expected) in [("graph", expected_graph), ("metadata", expected_metadata)] {
            let received = fs::read(fixture.root.join(format!("{name}.argv"))).unwrap();
            let fields = received
                .strip_suffix(&[0])
                .unwrap()
                .split(|byte| *byte == 0)
                .collect::<Vec<_>>();
            assert_eq!(
                fields,
                expected
                    .iter()
                    .map(|value| std::ffi::OsStr::new(value).as_bytes())
                    .collect::<Vec<_>>()
            );
        }
    }

    const LOCK: &str = "# Semantically current, deliberately noncanonical fixture lock.\nversion = 4\n[[package]]\nname = \"policy_probe\"\nversion = \"0.1.0\"\n";

    #[test]
    fn real_metadata_obeys_lock_and_offline_policies() {
        let cargo = PathBuf::from(
            std::env::var_os("CARGO").expect("Cargo provides its own executable to tests"),
        );
        for (name, policy, has_lock, expect_success, unchanged) in [
            (
                "locked-current",
                vec!["--locked", "--offline"],
                true,
                true,
                true,
            ),
            ("frozen-current", vec!["--frozen"], true, true, true),
            (
                "offline-formatting-control",
                vec!["--offline"],
                true,
                true,
                false,
            ),
            (
                "locked-missing",
                vec!["--locked", "--offline"],
                false,
                false,
                true,
            ),
        ] {
            let fixture = Fixture::new(name);
            fs::write(fixture.root.join("Cargo.toml"), "[package]\nname = \"policy_probe\"\nversion = \"0.1.0\"\nedition = \"2024\"\n[workspace]\n").unwrap();
            fs::create_dir(fixture.root.join("src")).unwrap();
            fs::write(fixture.root.join("src/lib.rs"), "pub fn fixture() {}\n").unwrap();
            let lock = fixture.root.join("Cargo.lock");
            if has_lock {
                fs::write(&lock, LOCK).unwrap();
                fs::write(fixture.root.join("before-lock.txt"), LOCK).unwrap();
            }
            let mut caller = args(&["build", "--manifest-path"]);
            caller.push(fixture.root.join("Cargo.toml").to_str().unwrap().to_owned());
            caller.extend(args(&policy));
            let invocation = CargoInvocation::new(&cargo, &caller);
            let mut metadata = invocation.metadata_command();
            // A controlled Cargo home prevents tests from touching caller caches.
            fs::create_dir(fixture.root.join("cargo-home")).unwrap();
            metadata.env("CARGO_HOME", fixture.root.join("cargo-home"));
            let mut command = metadata.cargo_command();
            let start = Instant::now();
            let result = CargoInvocation::execute_metadata(&mut command);
            let after = fs::read(&lock).ok();
            let receipt = serde_json::json!({"case":name,"program":command.get_program().to_str().unwrap(),"argv":command.get_args().map(|v|v.to_str().unwrap()).collect::<Vec<_>>(),"seconds":start.elapsed().as_secs_f64(),"success":result.is_ok(),"error":result.as_ref().err().map(ToString::to_string),"lockPresentBefore":has_lock,"lockPresentAfter":after.is_some(),"lockBytesUnchanged":after.as_deref()==has_lock.then_some(LOCK.as_bytes()),"noNixBuilder":true});
            fs::write(
                fixture.root.join("receipt.json"),
                serde_json::to_vec_pretty(&receipt).unwrap(),
            )
            .unwrap();
            eprintln!("CARGO_POLICY_CONTROL {receipt}");
            assert_eq!(result.is_ok(), expect_success, "{name}: {result:?}");
            if has_lock {
                let after = after.unwrap();
                assert_eq!(after == LOCK.as_bytes(), unchanged, "{name}");
                if !unchanged {
                    let text = String::from_utf8(after).unwrap();
                    assert!(text.starts_with("# This file is automatically @generated by Cargo."));
                    assert_eq!(
                        result.unwrap().packages[0].package.name.to_string(),
                        "policy_probe"
                    );
                }
            } else {
                assert!(after.is_none(), "{name}");
                assert!(result.unwrap_err().to_string().contains("--locked"));
            }
        }
    }
    #[cfg(unix)]
    fn output(status: i32, stdout: &[u8], stderr: &[u8]) -> Output {
        use std::os::unix::process::ExitStatusExt;
        Output {
            status: std::process::ExitStatus::from_raw(status << 8),
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
        }
    }

    #[cfg(unix)]
    #[test]
    fn metadata_status_utf8_and_json_errors_keep_library_precedence() {
        use cargo_metadata::Error;
        let err = CargoInvocation::decode_metadata(output(1, b"\xff", b"specific failure\n"))
            .unwrap_err();
        assert!(matches!(&err, Error::CargoMetadata{stderr} if stderr=="specific failure\n"));
        assert_eq!(
            err.to_string(),
            "`cargo metadata` exited with an error: specific failure\n"
        );
        assert!(matches!(
            CargoInvocation::decode_metadata(output(1, b"{}", b"\xff")),
            Err(Error::ErrUtf8(_))
        ));
        assert!(matches!(
            CargoInvocation::decode_metadata(output(0, b"{}\n\xff", b"")),
            Err(Error::Utf8(_))
        ));
        let err = CargoInvocation::decode_metadata(output(0, b"logs only\n {}\n", b"ignored"))
            .unwrap_err();
        assert!(matches!(err, Error::NoJson));
        assert_eq!(err.to_string(), Error::NoJson.to_string());
        for invalid in [
            "{",
            "{}",
            "{\"packages\":42}",
            "{\"packages\":[],\"packages\":[]}",
        ] {
            let expected = MetadataCommand::parse(invalid).unwrap_err();
            let actual =
                CargoInvocation::decode_metadata(output(0, invalid.as_bytes(), b"ignored"))
                    .unwrap_err();
            assert!(matches!(actual, Error::Json(_)));
            assert_eq!(actual.to_string(), expected.to_string());
        }
        let err = CargoInvocation::execute_metadata(&mut Command::new(
            "/nonexistent/cargo-dyndrv-fixture/cargo",
        ))
        .unwrap_err();
        assert!(matches!(err, Error::Io(_)));
    }

    #[cfg(unix)]
    #[test]
    fn selected_live_line_and_complete_bound_bytes_preserve_same_records() {
        let document = serde_json::json!({"packages":[{"name":"p","version":"0.1.0","id":"p","manifest_path":"/fixture/Cargo.toml","targets":[],"dependencies":[],"features":{},"rust_version":"1.85.0"}],"workspace_members":["p"],"workspace_root":"/fixture","target_directory":"/fixture/target","version":1,"resolve":null});
        let compact = serde_json::to_string(&document).unwrap();
        let live = format!("leading log\n{compact}\n{{invalid second JSON line\n");
        let live = CargoInvocation::decode_metadata(output(
            0,
            live.as_bytes(),
            b"ignored successful stderr\xff",
        ))
        .unwrap();
        let bound = Metadata::from_slice(&serde_json::to_vec_pretty(&document).unwrap()).unwrap();
        assert_eq!(live.workspace_root, bound.workspace_root);
        assert_eq!(live.target_directory, bound.target_directory);
        assert_eq!(live.workspace_members, bound.workspace_members);
        assert_eq!(live.packages.len(), 1);
        assert_eq!(live.packages[0].package, bound.packages[0].package);
        assert_eq!(live.packages[0].rust_version_env(), "1.85.0");
        assert_eq!(
            live.packages[0].rust_version_env(),
            bound.packages[0].rust_version_env()
        );
    }
}
