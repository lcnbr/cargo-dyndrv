//! Publish compiler artifacts only after compiling in a fresh builder-local directory.
use std::{
    ffi::OsString,
    fs, io,
    path::{Component, Path},
    process::{Command, ExitStatus},
};

pub fn publish(source: &Path, destination: &Path) -> io::Result<()> {
    fs::create_dir(destination)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        let output = destination.join(entry.file_name());
        if kind.is_dir() {
            publish(&entry.path(), &output)?;
        } else if kind.is_file() {
            fs::copy(entry.path(), output)?;
        } else {
            return Err(io::Error::other(
                "compiler output contains a non-regular entry",
            ));
        }
    }
    fs::set_permissions(destination, fs::metadata(source)?.permissions())
}

pub fn compile(staging: &Path, output: &Path, argv: &[OsString]) -> io::Result<ExitStatus> {
    if !staging.is_absolute()
        || !output.is_absolute()
        || staging
            .components()
            .chain(output.components())
            .any(|c| c == Component::ParentDir)
        || staging.starts_with(output)
        || output.starts_with(staging)
    {
        return Err(io::Error::other(
            "disjoint absolute staging and output paths without parent components required",
        ));
    }
    // Reject a previous result, including dangling symlinks, before executing.
    match fs::symlink_metadata(output) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
        Ok(_) => return Err(io::Error::other("public compiler output already exists")),
    }
    let (program, args) = argv
        .split_first()
        .ok_or_else(|| io::Error::other("compiler command required"))?;
    fs::create_dir(staging)?;
    // Inherit cwd, environment and stdio. env-wrap and rustc arguments are opaque.
    let status = Command::new(program).args(args).status()?;
    if status.success()
        && let Err(error) = publish(staging, output)
    {
        let _ = fs::remove_dir_all(output);
        return Err(error);
    }
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{os::unix::fs::PermissionsExt, time::SystemTime};

    fn fixture() -> std::path::PathBuf {
        let nonce = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("compile-wrap-{}-{nonce}", std::process::id()));
        fs::create_dir(&root).unwrap();
        root
    }

    fn shell(body: &str, staging: &Path) -> Vec<OsString> {
        vec![
            "/bin/sh".into(),
            "-c".into(),
            body.into(),
            "compiler".into(),
            staging.into(),
        ]
    }

    #[test]
    fn successful_outputs_keep_bytes_names_and_modes() {
        let root = fixture();
        let stage = root.join("staging");
        let out = root.join("public");
        let status = compile(&stage, &out, &shell("mkdir \"$1/nested\"; printf artifact > \"$1/nested/libx.rlib\"; chmod 751 \"$1/nested/libx.rlib\"", &stage)).unwrap();
        assert!(status.success());
        assert_eq!(fs::read(out.join("nested/libx.rlib")).unwrap(), b"artifact");
        assert_eq!(
            fs::metadata(out.join("nested/libx.rlib"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o751
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn failures_and_signals_never_publish() {
        for script in ["printf partial > \"$1/partial\"; exit 23", "kill -TERM $$"] {
            let root = fixture();
            let stage = root.join("staging");
            let out = root.join("public");
            let status = compile(&stage, &out, &shell(script, &stage)).unwrap();
            assert!(!status.success());
            assert!(!out.exists());
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn existing_staging_or_output_is_not_touched() {
        for occupied in ["staging", "public"] {
            let root = fixture();
            fs::create_dir(root.join(occupied)).unwrap();
            fs::write(root.join(occupied).join("sentinel"), "unchanged").unwrap();
            assert!(
                compile(
                    &root.join("staging"),
                    &root.join("public"),
                    &shell("exit 0", &root)
                )
                .is_err()
            );
            assert_eq!(
                fs::read(root.join(occupied).join("sentinel")).unwrap(),
                b"unchanged"
            );
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn overlapping_and_parent_component_paths_fail_before_compilation() {
        let root = fixture();
        for (stage, out) in [
            (root.join("stage"), root.join("stage/public")),
            (root.join("public/stage"), root.join("public")),
            (root.join("stage"), root.join("stage")),
            (root.join("stage"), root.join("other/../stage/public")),
        ] {
            assert!(compile(&stage, &out, &shell("exit 0", &root)).is_err());
            assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unsupported_entry_does_not_leave_partial_public_output() {
        let root = fixture();
        let stage = root.join("staging");
        let out = root.join("public");
        assert!(compile(&stage, &out, &shell("ln -s missing \"$1/link\"", &stage)).is_err());
        assert!(!out.exists());
        fs::remove_dir_all(root).unwrap();
    }
}
