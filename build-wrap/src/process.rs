use std::{
    ffi::OsString,
    path::PathBuf,
    process::{Command, Stdio},
};

/// The terminal child command, after the outer environment wrappers have run.
/// Its environment remains inherited except for this wrapper's OUT_DIR setting.
pub(crate) struct ScriptProcess {
    executable: PathBuf,
    cwd: PathBuf,
    out_dir: PathBuf,
    args: Vec<OsString>,
}

impl ScriptProcess {
    pub(crate) fn new(
        executable: PathBuf,
        cwd: PathBuf,
        out_dir: PathBuf,
        args: impl Iterator<Item = OsString>,
    ) -> Self {
        Self {
            executable,
            cwd,
            out_dir,
            args: args.collect(),
        }
    }

    pub(crate) fn command(&self) -> Command {
        let mut command = Command::new(&self.executable);
        command
            .args(&self.args)
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .current_dir(&self.cwd)
            .env("OUT_DIR", &self.out_dir);
        command
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::BTreeMap, fs};

    #[test]
    fn observation_does_not_create_outputs_or_spawn() {
        let root =
            std::env::temp_dir().join(format!("uncreated-build-wrap-{}", std::process::id()));
        assert!(!root.exists());
        let invocation = ScriptProcess::new(
            root.join("script"),
            root.join("cwd"),
            root.join("out"),
            [OsString::from("one argument"), OsString::from("two")].into_iter(),
        );
        let command = invocation.command();
        assert_eq!(command.get_program(), root.join("script"));
        assert_eq!(command.get_current_dir(), Some(root.join("cwd").as_path()));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            ["one argument", "two"]
        );
        assert_eq!(
            command.get_envs().collect::<Vec<_>>(),
            [(
                std::ffi::OsStr::new("OUT_DIR"),
                Some(root.join("out").as_os_str())
            )]
        );
        assert!(!root.exists());
    }

    #[cfg(unix)]
    #[test]
    fn argument_bytes_are_not_stringified() {
        use std::os::unix::ffi::OsStringExt;
        let argument = OsString::from_vec(b"equals= newline\n nonutf8\xff".to_vec());
        let invocation = ScriptProcess::new(
            "script".into(),
            ".".into(),
            "out".into(),
            [argument.clone()].into_iter(),
        );
        assert_eq!(
            invocation.command().get_args().collect::<Vec<_>>(),
            [&argument]
        );
    }

    #[cfg(unix)]
    #[test]
    fn dump_child_environment() {
        use std::os::unix::ffi::OsStrExt;
        if std::env::var_os("BUILD_WRAP_DUMP_ENVIRONMENT").is_none() {
            return;
        }
        let mut bytes = Vec::new();
        for (key, value) in std::env::vars_os() {
            bytes.extend_from_slice(key.as_bytes());
            bytes.push(0);
            bytes.extend_from_slice(value.as_bytes());
            bytes.push(0);
        }
        let output = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
        fs::write(output.join("environment.nul"), bytes).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn terminal_child_preserves_inheritance_and_environment_bytes() {
        use std::os::unix::ffi::OsStringExt;
        let root = std::env::temp_dir().join(format!(
            "build-wrap-env-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        let raw = OsString::from_vec(b"value=with\nnewline\xff".to_vec());
        let invocation = ScriptProcess::new(
            std::env::current_exe().unwrap(),
            root.clone(),
            root.clone(),
            [
                "--exact",
                "process::tests::dump_child_environment",
                "--nocapture",
            ]
            .into_iter()
            .map(OsString::from),
        );
        let mut command = invocation.command();
        // These represent already explicit outer-wrapper operations. This test
        // does not claim the production derivation supplies Cargo's environment.
        command
            .env("BUILD_WRAP_DUMP_ENVIRONMENT", "1")
            .env("BUILD_WRAP_RAW", &raw)
            .env("BUILD_WRAP_EMPTY", "")
            .env("CARGO_ENCODED_RUSTFLAGS", "--cfg\x1fone value")
            .env_remove("RUSTFLAGS");
        let overlay = command
            .get_envs()
            .map(|(key, value)| (key.to_owned(), value.map(OsString::from)))
            .collect::<BTreeMap<_, _>>();
        let inherited = std::env::vars_os().collect::<BTreeMap<_, _>>();
        let result = command.output().unwrap();
        assert!(result.status.success(), "{:?}", result.status);
        let bytes = fs::read(root.join("environment.nul")).unwrap();
        let fields = bytes
            .strip_suffix(&[0])
            .unwrap()
            .split(|byte| *byte == 0)
            .collect::<Vec<_>>();
        assert_eq!(fields.len() % 2, 0);
        let mut child = BTreeMap::new();
        for pair in fields.as_chunks::<2>().0 {
            assert!(
                child
                    .insert(
                        OsString::from_vec(pair[0].to_vec()),
                        OsString::from_vec(pair[1].to_vec())
                    )
                    .is_none()
            );
        }
        for (key, value) in &overlay {
            assert_eq!(child.get(key), value.as_ref(), "{key:?}");
        }
        for (key, value) in &inherited {
            if !overlay.contains_key(key) {
                assert_eq!(child.get(key), Some(value), "{key:?}");
            }
        }
        assert!(
            child
                .keys()
                .all(|key| overlay.contains_key(key) || inherited.contains_key(key))
        );
        assert_eq!(child[&OsString::from("BUILD_WRAP_RAW")], raw);
        assert_eq!(child[&OsString::from("BUILD_WRAP_EMPTY")], "");
        assert!(!child.contains_key(&OsString::from("RUSTFLAGS")));
        fs::remove_dir_all(root).unwrap();
    }
}
