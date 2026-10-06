//! Opt-in private generated tree owner. This module materializes explicitly
//! supplied inputs; it does not discover Cargo edges or authenticate Nix pairs.
use super::{Materializer, copy_tree_contents, invalid, validate_tree, verify_unchanged};
use std::{
    collections::BTreeSet,
    ffi::OsString,
    fs::{self, File, FileTimes},
    io,
    os::unix::{
        ffi::OsStrExt,
        fs::{MetadataExt, PermissionsExt},
    },
    path::{Component, Path, PathBuf},
    process::{Command, ExitCode, ExitStatus},
    time::{Duration, SystemTime},
};

#[derive(Debug)]
struct Input {
    id: String,
    out: PathBuf,
    flags: PathBuf,
}
#[derive(Debug)]
struct Producer {
    id: String,
    out: PathBuf,
    flags: PathBuf,
}
#[derive(Debug)]
struct Config {
    source: PathBuf,
    inputs: Vec<Input>,
    producer: Option<Producer>,
    owner: Option<String>,
    command: Vec<OsString>,
}

fn id(value: &std::ffi::OsStr) -> io::Result<String> {
    let value = value
        .to_str()
        .ok_or_else(|| invalid("generated unit ID must be UTF-8"))?;
    if value.len() != 16
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(invalid(
            "generated unit ID must be 16 lowercase hex characters",
        ));
    }
    Ok(value.to_owned())
}
fn parse(args: &[OsString]) -> io::Result<Config> {
    let mut source = None;
    let mut inputs = Vec::new();
    let mut producer = None;
    let mut owner = None;
    let mut at = 0;
    while at < args.len() && args[at] != "--" {
        match args[at].to_str() {
            Some("--source") if source.is_none() && at + 1 < args.len() => {
                source = Some(PathBuf::from(&args[at + 1]));
                at += 2;
            }
            Some("--input") if at + 3 < args.len() => {
                inputs.push(Input {
                    id: id(&args[at + 1])?,
                    out: args[at + 2].clone().into(),
                    flags: args[at + 3].clone().into(),
                });
                at += 4;
            }
            Some("--producer") if producer.is_none() && at + 3 < args.len() => {
                producer = Some(Producer {
                    id: id(&args[at + 1])?,
                    out: args[at + 2].clone().into(),
                    flags: args[at + 3].clone().into(),
                });
                at += 4;
            }
            Some("--owner") if owner.is_none() && at + 1 < args.len() => {
                owner = Some(id(&args[at + 1])?);
                at += 2;
            }
            _ => {
                return Err(invalid(
                    "invalid, incomplete or repeated generated-v1 option",
                ));
            }
        }
    }
    if at == args.len() || at + 1 == args.len() || (owner.is_some() && producer.is_some()) {
        return Err(invalid(
            "generated-v1 requires -- COMMAND and exclusive --owner/--producer",
        ));
    }
    Ok(Config {
        source: source.ok_or_else(|| invalid("--source required"))?,
        inputs,
        producer,
        owner,
        command: args[at + 1..].to_vec(),
    })
}

fn checked_path(path: &Path) -> io::Result<()> {
    if !path.is_absolute()
        || path
            .as_os_str()
            .as_bytes()
            .split(|b| *b == b'/')
            .any(|part| part == b"." || part == b"..")
        || path
            .components()
            .any(|p| !matches!(p, Component::RootDir | Component::Normal(_)))
    {
        return Err(invalid(
            "generated paths must be absolute without dot components",
        ));
    }
    // canonicalize alone would accept symlink aliases. Reject every existing
    // ancestor explicitly, including an ancestor of an absent output.
    let mut prefix = PathBuf::new();
    for part in path.components() {
        prefix.push(part);
        match fs::symlink_metadata(&prefix) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(invalid("symlink in generated path or ancestor"));
            }
            Ok(meta) if prefix != path && !meta.is_dir() => {
                return Err(invalid("non-directory generated path ancestor"));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}
fn absent(path: &Path) -> io::Result<()> {
    checked_path(path)?;
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
        Ok(_) => Err(invalid("generated destination already exists")),
    }
}
fn overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}
fn generated(root: &Path, id: &str) -> PathBuf {
    root.join("generated/v1").join(id).join("out")
}
fn flags(root: &Path, id: &str) -> PathBuf {
    root.join("generated-flags/v1").join(id)
}
fn preflight(config: &Config, root: &Path) -> io::Result<()> {
    checked_path(root)?;
    if !fs::symlink_metadata(root)?.is_dir() {
        return Err(invalid("private build root must be a directory"));
    }
    for suffix in ["source", "generated", "generated-flags"] {
        absent(&root.join(suffix))?;
    }
    let mut ids = BTreeSet::new();
    let mut trees = vec![config.source.as_path()];
    for input in &config.inputs {
        id(input.id.as_ref())?;
        if !ids.insert(input.id.as_str()) {
            return Err(invalid("duplicate generated input identity"));
        }
        trees.extend([input.out.as_path(), input.flags.as_path()]);
    }
    if let Some(owner) = &config.owner
        && !ids.contains(owner.as_str())
    {
        return Err(invalid(
            "compiler owner must identify a declared generated input",
        ));
    }
    if let Some(producer) = &config.producer {
        id(producer.id.as_ref())?;
        if ids.contains(producer.id.as_str()) {
            return Err(invalid("producer cannot be its own generated input"));
        }
    }
    for (i, tree) in trees.iter().enumerate() {
        checked_path(tree)?;
        validate_tree(tree)?;
        if overlap(tree, root) || trees[..i].iter().any(|other| overlap(tree, other)) {
            return Err(invalid(
                "overlapping source, generated inputs or private root",
            ));
        }
    }
    if let Some(producer) = &config.producer {
        for output in [&producer.out, &producer.flags] {
            absent(output)?;
            let parent = output
                .parent()
                .ok_or_else(|| invalid("public output requires a parent"))?;
            if !fs::symlink_metadata(parent)?.is_dir()
                || overlap(output, root)
                || trees.iter().any(|tree| overlap(output, tree))
            {
                return Err(invalid(
                    "public generated output overlaps inputs/private root or has no parent",
                ));
            }
        }
        if overlap(&producer.out, &producer.flags) {
            return Err(invalid("public generated outputs overlap"));
        }
    }
    Ok(())
}
fn epoch() -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(1)
}
fn normalize_times(path: &Path) -> io::Result<()> {
    if fs::symlink_metadata(path)?.is_dir() {
        for entry in fs::read_dir(path)? {
            normalize_times(&entry?.path())?;
        }
    }
    File::open(path)?.set_times(FileTimes::new().set_accessed(epoch()).set_modified(epoch()))
}
fn verify_times(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.modified()? != epoch() {
        return Err(invalid("generated materialization timestamp changed"));
    }
    if metadata.is_dir() {
        for entry in fs::read_dir(path)? {
            verify_times(&entry?.path())?;
        }
    }
    Ok(())
}
fn copy_generated(source: &Path, destination: &Path) -> io::Result<()> {
    Materializer::Copy.materialize(source, destination, &[])?;
    normalize_times(destination)?;
    verify_unchanged(source, destination, &[])?;
    verify_times(destination)
}
fn remove_owned(path: &Path, dev: u64, ino: u64) -> io::Result<()> {
    let meta = fs::symlink_metadata(path)?;
    if !meta.is_dir() || meta.dev() != dev || meta.ino() != ino {
        return Err(invalid("refusing cleanup of replaced generated output"));
    }
    fn writable_dirs(path: &Path) -> io::Result<()> {
        let meta = fs::symlink_metadata(path)?;
        if meta.is_dir() {
            fs::set_permissions(
                path,
                fs::Permissions::from_mode(meta.permissions().mode() | 0o700),
            )?;
            for entry in fs::read_dir(path)? {
                writable_dirs(&entry?.path())?;
            }
        }
        Ok(())
    }
    writable_dirs(path)?;
    fs::remove_dir_all(path)
}
fn publish(config: &Producer, root: &Path) -> io::Result<()> {
    let source_out = generated(root, &config.id);
    let source_flags = flags(root, &config.id);
    for source in [&source_out, &source_flags] {
        checked_path(source)?;
        validate_tree(source)?;
    }
    // Recheck after the child: it may have changed an otherwise valid ancestor.
    // A preexisting path created by the child remains unowned and is not deleted.
    for output in [&config.out, &config.flags] {
        absent(output)?;
    }
    let mut owned = Vec::new();
    let result = (|| {
        // Reserve each root exclusively; a failed reservation never makes that
        // path ours to delete. Record inode identity before any recursive copy.
        for output in [&config.out, &config.flags] {
            fs::create_dir(output)?;
            let meta = fs::symlink_metadata(output)?;
            owned.push((output, meta.dev(), meta.ino()));
        }
        for (source, output) in [(&source_out, &config.out), (&source_flags, &config.flags)] {
            copy_tree_contents(source, output)?;
            normalize_times(output)?;
            verify_unchanged(source, output, &[])?;
            verify_times(output)?;
        }
        Ok(())
    })();
    if let Err(error) = result {
        for (path, dev, ino) in owned {
            if let Err(cleanup) = remove_owned(path, dev, ino) {
                return Err(io::Error::other(format!(
                    "generated export failed: {error}; owned-output cleanup failed: {cleanup}"
                )));
            }
        }
        return Err(error);
    }
    Ok(())
}
fn execute(config: &Config, root: &Path) -> io::Result<ExitStatus> {
    preflight(config, root)?;
    let source = root.join("source");
    Materializer::Copy.materialize(&config.source, &source, &[])?;
    fs::create_dir(root.join("generated"))?;
    fs::create_dir(root.join("generated/v1"))?;
    fs::create_dir(root.join("generated-flags"))?;
    fs::create_dir(root.join("generated-flags/v1"))?;
    for input in &config.inputs {
        fs::create_dir(root.join("generated/v1").join(&input.id))?;
        copy_generated(&input.out, &generated(root, &input.id))?;
        copy_generated(&input.flags, &flags(root, &input.id))?;
    }
    if let Some(producer) = &config.producer {
        fs::create_dir(root.join("generated/v1").join(&producer.id))?;
    }
    let mut command = Command::new(&config.command[0]);
    command
        .args(&config.command[1..])
        .current_dir(&source)
        .env_remove("OUT_DIR");
    // Public Nix output paths must not leak into admitted generator recipes.
    command.env_remove("out").env_remove("flags");
    if let Some(owner) = config
        .producer
        .as_ref()
        .map(|p| &p.id)
        .or(config.owner.as_ref())
    {
        command.env("OUT_DIR", generated(root, owner));
    }
    let status = command.status()?;
    checked_path(&source)?;
    verify_unchanged(&config.source, &source, &[])?;
    for input in &config.inputs {
        for (original, copy) in [
            (&input.out, generated(root, &input.id)),
            (&input.flags, flags(root, &input.id)),
        ] {
            checked_path(&copy)?;
            verify_unchanged(original, &copy, &[])?;
            verify_times(&copy)?;
        }
    }
    if status.success()
        && let Some(producer) = &config.producer
    {
        publish(producer, root)?;
    }
    Ok(status)
}

pub fn run(args: &[OsString]) -> io::Result<ExitCode> {
    if std::env::current_dir()? != Path::new("/build") {
        return Err(invalid("generated-v1 requires a private /build cwd"));
    }
    let config = parse(args)?;
    let status = execute(&config, Path::new("/build"))?;
    match status.code() {
        Some(code) => Ok(ExitCode::from(code as u8)),
        None => Err(io::Error::other(format!(
            "generated command terminated with {status}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    const A: &str = "000000000000000a";
    const B: &str = "000000000000000b";
    struct Fixture {
        root: PathBuf,
    }
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "generated-wrap-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir(&root).unwrap();
            for name in ["build", "src", "input-out", "input-flags"] {
                fs::create_dir(root.join(name)).unwrap();
            }
            fs::write(root.join("src/input"), b"source").unwrap();
            fs::write(root.join("input-out/data"), b"generated").unwrap();
            fs::write(
                root.join("input-flags/metadata"),
                b"DEP_A_PATH=logical-path-only\n",
            )
            .unwrap();
            Self { root }
        }
        fn build(&self) -> PathBuf {
            self.root.join("build")
        }
        fn config(&self, body: &str) -> Config {
            Config {
                source: self.root.join("src"),
                inputs: vec![Input {
                    id: A.into(),
                    out: self.root.join("input-out"),
                    flags: self.root.join("input-flags"),
                }],
                producer: None,
                owner: Some(A.into()),
                command: vec!["/bin/sh".into(), "-c".into(), body.into()],
            }
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let m = fs::symlink_metadata(&self.root).unwrap();
            remove_owned(&self.root, m.dev(), m.ino()).unwrap();
        }
    }
    #[test]
    fn predecessor_copies_are_disjoint_readonly_and_have_exact_file_and_directory_mtimes() {
        let f = Fixture::new();
        let original = f.root.join("input-out/data");
        fs::set_permissions(&original, fs::Permissions::from_mode(0o751)).unwrap();
        File::open(&original)
            .unwrap()
            .set_times(
                FileTimes::new().set_modified(
                    SystemTime::UNIX_EPOCH + Duration::new(1_790_000_000, 987_654_321),
                ),
            )
            .unwrap();
        let c = f.config("test -f \"$OUT_DIR/data\" && test -f input");
        assert!(execute(&c, &f.build()).unwrap().success());
        for copy in [generated(&f.build(), A), flags(&f.build(), A)] {
            verify_times(&copy).unwrap();
            assert_eq!(fs::metadata(&copy).unwrap().mtime_nsec(), 0);
        }
        let copy = generated(&f.build(), A).join("data");
        assert_eq!(fs::read(&copy).unwrap(), b"generated");
        assert_ne!(
            fs::metadata(&original).unwrap().ino(),
            fs::metadata(&copy).unwrap().ino()
        );
        assert_eq!(
            fs::metadata(&copy).unwrap().permissions().mode() & 0o777,
            0o551
        );
        assert_eq!(fs::metadata(&copy).unwrap().mtime(), 1);
        assert_eq!(fs::metadata(&copy).unwrap().mtime_nsec(), 0);
        assert_eq!(fs::metadata(&original).unwrap().mtime_nsec(), 987_654_321);
    }
    #[test]
    fn producer_publishes_both_outputs_only_after_success() {
        let f = Fixture::new();
        let mut c=f.config("mkdir \"$OUT_DIR\" \"$1\"; printf '%s' \"$OUT_DIR\" > \"$OUT_DIR/path\"; printf flags > \"$1/env\"; chmod 751 \"$OUT_DIR/path\"");
        c.owner = None;
        c.producer = Some(Producer {
            id: B.into(),
            out: f.root.join("public-out"),
            flags: f.root.join("public-flags"),
        });
        c.command
            .extend(["script".into(), flags(&f.build(), B).into_os_string()]);
        assert!(execute(&c, &f.build()).unwrap().success());
        assert_eq!(
            fs::read_to_string(f.root.join("public-out/path")).unwrap(),
            generated(&f.build(), B).to_str().unwrap()
        );
        assert_eq!(fs::read(f.root.join("public-flags/env")).unwrap(), b"flags");
        verify_times(&f.root.join("public-out")).unwrap();
        verify_times(&f.root.join("public-flags")).unwrap();
        assert_eq!(
            fs::metadata(f.root.join("public-out/path"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o551
        );
    }
    #[test]
    fn failed_or_invalid_generator_never_publishes() {
        for body in [
            "mkdir \"$OUT_DIR\" \"$1\"; printf partial > \"$OUT_DIR/data\"; exit 9",
            "mkdir \"$OUT_DIR\" \"$1\"; ln -s /unowned \"$OUT_DIR/link\"",
        ] {
            let f = Fixture::new();
            let mut c = f.config(body);
            c.owner = None;
            c.producer = Some(Producer {
                id: B.into(),
                out: f.root.join("public-out"),
                flags: f.root.join("public-flags"),
            });
            c.command
                .extend(["script".into(), flags(&f.build(), B).into_os_string()]);
            assert!(!execute(&c, &f.build()).is_ok_and(|status| status.success()));
            assert!(!f.root.join("public-out").exists());
            assert!(!f.root.join("public-flags").exists());
        }
    }
    #[test]
    fn mutated_source_predecessor_or_timestamp_is_rejected() {
        for body in [
            "chmod u+w input; printf changed > input",
            "chmod u+w \"$OUT_DIR/data\"; printf changed > \"$OUT_DIR/data\"",
            "touch \"$OUT_DIR/data\"",
        ] {
            let f = Fixture::new();
            let c = f.config(body);
            assert!(execute(&c, &f.build()).is_err());
        }
    }
    #[test]
    fn preflight_rejects_missing_pairs_stale_paths_aliases_symlinks_and_overlaps_before_mutation() {
        for case in 0..12 {
            let f = Fixture::new();
            let mut c = f.config("exit 0");
            match case {
                0 => c.inputs[0].out = f.root.join("missing"),
                1 => c.inputs[0].flags = f.root.join("missing"),
                2 => c.inputs.push(Input {
                    id: A.into(),
                    out: f.root.join("input-out"),
                    flags: f.root.join("input-flags"),
                }),
                3 => {
                    c.owner = None;
                    c.producer = Some(Producer {
                        id: A.into(),
                        out: f.root.join("public-out"),
                        flags: f.root.join("public-flags"),
                    });
                }
                4 => c.owner = Some(B.into()),
                5 => c.source = f.root.clone(),
                6 => {
                    symlink(f.root.join("src"), f.root.join("alias")).unwrap();
                    c.source = f.root.join("alias");
                }
                7 => {
                    symlink(f.root.join("input-out"), f.root.join("alias")).unwrap();
                    c.inputs[0].out = f.root.join("alias/data");
                }
                8 => {
                    symlink("data", f.root.join("input-out/link")).unwrap();
                }
                9 => {
                    c.owner = None;
                    c.producer = Some(Producer {
                        id: B.into(),
                        out: f.root.join("public"),
                        flags: f.root.join("public/nested"),
                    });
                }
                10 => {
                    c.owner = None;
                    c.producer = Some(Producer {
                        id: B.into(),
                        out: f.root.join("src/output"),
                        flags: f.root.join("public-flags"),
                    });
                }
                11 => c.source = f.root.join("src/./"),
                _ => unreachable!(),
            }
            assert!(preflight(&c, &f.build()).is_err(), "case {case}");
            assert!(!f.build().join("source").exists());
            assert!(!f.build().join("generated").exists());
        }
        for suffix in ["source", "generated", "generated-flags"] {
            let f = Fixture::new();
            fs::create_dir(f.build().join(suffix)).unwrap();
            assert!(preflight(&f.config("exit 0"), &f.build()).is_err());
        }
    }
    #[test]
    fn failed_second_output_reservation_preserves_unowned_path() {
        let f = Fixture::new();
        fs::create_dir_all(generated(&f.build(), B)).unwrap();
        fs::create_dir_all(flags(&f.build(), B)).unwrap();
        fs::write(generated(&f.build(), B).join("data"), b"payload").unwrap();
        let producer = Producer {
            id: B.into(),
            out: f.root.join("public-out"),
            flags: f.root.join("preexisting"),
        };
        fs::create_dir(&producer.flags).unwrap();
        fs::write(producer.flags.join("keep"), b"untouched").unwrap();
        assert!(publish(&producer, &f.build()).is_err());
        assert!(!producer.out.exists());
        assert_eq!(fs::read(producer.flags.join("keep")).unwrap(), b"untouched");
    }
    #[test]
    fn replaced_owned_root_is_never_removed() {
        let f = Fixture::new();
        let path = f.root.join("owned");
        fs::create_dir(&path).unwrap();
        let old = fs::metadata(&path).unwrap();
        fs::rename(&path, f.root.join("retained-original")).unwrap();
        fs::create_dir(&path).unwrap();
        fs::write(path.join("keep"), b"untouched").unwrap();
        assert!(remove_owned(&path, old.dev(), old.ino()).is_err());
        assert_eq!(fs::read(path.join("keep")).unwrap(), b"untouched");
    }
    #[test]
    fn changed_generated_or_public_ancestors_are_rejected_after_child() {
        for own in [false, true] {
            let f = Fixture::new();
            let mut c =
                f.config("mkdir \"$OUT_DIR\" \"$1\"; mv \"$2\" \"$3\"; ln -s \"$3\" \"$2\"");
            c.owner = None;
            c.producer = Some(Producer {
                id: B.into(),
                out: f.root.join("public-out"),
                flags: f.root.join("public-flags"),
            });
            let selected = if own { B } else { A };
            c.command.extend([
                "script".into(),
                flags(&f.build(), B).into_os_string(),
                f.build()
                    .join("generated/v1")
                    .join(selected)
                    .into_os_string(),
                f.build().join("retained-generated").into_os_string(),
            ]);
            assert!(execute(&c, &f.build()).is_err());
            assert!(!f.root.join("public-out").exists());
            assert!(!f.root.join("public-flags").exists());
        }
        let f = Fixture::new();
        let parent = f.root.join("public-parent");
        fs::create_dir(&parent).unwrap();
        let mut c = f.config("mkdir \"$OUT_DIR\" \"$1\"; rmdir \"$2\"; ln -s \"$3\" \"$2\"");
        c.owner = None;
        c.producer = Some(Producer {
            id: B.into(),
            out: parent.join("out"),
            flags: parent.join("flags"),
        });
        c.command.extend([
            "script".into(),
            flags(&f.build(), B).into_os_string(),
            parent.into_os_string(),
            f.root.join("input-out").into_os_string(),
        ]);
        assert!(execute(&c, &f.build()).is_err());
        assert!(!f.root.join("input-out/out").exists());
        assert!(!f.root.join("input-out/flags").exists());
    }

    #[test]
    fn parser_requires_explicit_identity_roles_and_command() {
        let args = |values: &[&str]| values.iter().map(OsString::from).collect::<Vec<_>>();
        assert!(
            parse(&args(&[
                "--source",
                "/source",
                "--input",
                A,
                "/out",
                "/flags",
                "--owner",
                A,
                "--",
                "/bin/true"
            ]))
            .is_ok()
        );
        for values in [
            vec!["--source", "/source", "--"],
            vec![
                "--source",
                "/source",
                "--input",
                A,
                "/out",
                "--",
                "/bin/true",
            ],
            vec![
                "--source",
                "/source",
                "--owner",
                A,
                "--producer",
                B,
                "/out",
                "/flags",
                "--",
                "/bin/true",
            ],
            vec![
                "--source",
                "/source",
                "--input",
                "../escape",
                "/out",
                "/flags",
                "--",
                "/bin/true",
            ],
        ] {
            assert!(parse(&args(&values)).is_err());
        }
    }
}
