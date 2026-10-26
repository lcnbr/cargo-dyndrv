//! Source materialization with an explicitly owned compiler output file or subtree.
use std::fs::{self, Permissions};
#[cfg(target_os = "linux")]
use std::io::Result as IoResult;
use std::io::{self, Read};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

pub mod generated;
#[cfg(target_os = "linux")]
mod overlay;

#[derive(Clone, Copy)]
pub enum Writable<'a> {
    Directory(&'a Path),
    /// An empty input placeholder. Overlays stage it on private ordinary storage.
    EmptyDirectory(&'a Path),
    File(&'a Path),
    /// Temporary children are allowed during execution; none may remain.
    ScratchDirectory(&'a Path),
}

impl<'a> Writable<'a> {
    fn path(self) -> &'a Path {
        match self {
            Self::Directory(path)
            | Self::EmptyDirectory(path)
            | Self::File(path)
            | Self::ScratchDirectory(path) => path,
        }
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::other(message)
}

pub fn validate_tree(root: &Path) -> io::Result<()> {
    if !fs::symlink_metadata(root)?.is_dir() {
        return Err(invalid("workspace source must be a directory"));
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        if kind.is_dir() {
            validate_tree(&entry.path())?;
        } else if !kind.is_file() {
            return Err(invalid(
                "workspace source contains a symlink or special file",
            ));
        }
    }
    Ok(())
}

fn readonly_mode(path: &Path) -> io::Result<u32> {
    Ok(fs::symlink_metadata(path)?.permissions().mode() & 0o777 & !0o222)
}

/// Compare complete file bytes without allocations proportional to artifact size.
pub fn same_contents(left: &Path, right: &Path) -> io::Result<bool> {
    let mut left = fs::File::open(left)?;
    let mut right = fs::File::open(right)?;
    let mut remaining = left.metadata()?.len();
    if remaining != right.metadata()?.len() {
        return Ok(false);
    }
    let mut a = vec![0; remaining.clamp(1, 64 * 1024) as usize];
    let mut b = vec![0; a.len()];
    while remaining > 0 {
        let count = remaining.min(a.len() as u64) as usize;
        left.read_exact(&mut a[..count])?;
        right.read_exact(&mut b[..count])?;
        if a[..count] != b[..count] {
            return Ok(false);
        }
        remaining -= count as u64;
    }
    // Growth during comparison must not turn an equal prefix into acceptance;
    // read_exact above also rejects truncation before the advertised length.
    Ok(left.read(&mut a[..1])? == 0 && right.read(&mut b[..1])? == 0)
}

/// Source staging changes independently of the post-execution validation.
pub enum Materializer {
    Copy,
    #[cfg(target_os = "linux")]
    Overlay(overlay::Mounts),
}

impl Materializer {
    pub fn overlay(root: &Path) -> io::Result<Self> {
        #[cfg(target_os = "linux")]
        {
            overlay::Mounts::new(root).map(Self::Overlay)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = root;
            Err(io::Error::other("source overlays require Linux"))
        }
    }

    pub fn materialize(
        &mut self,
        source: &Path,
        destination: &Path,
        writable: &[Writable<'_>],
    ) -> io::Result<()> {
        validate_tree(source)?;
        let parent = destination
            .parent()
            .ok_or_else(|| invalid("source destination needs a parent"))?
            .canonicalize()?;
        let destination = parent.join(
            destination
                .file_name()
                .ok_or_else(|| invalid("source destination needs a name"))?,
        );
        let source = source.canonicalize()?;
        if source.starts_with(&destination) || destination.starts_with(&source) {
            return Err(invalid("source and destination must be disjoint"));
        }
        validate_output(&source, &destination, writable)?;
        match self {
            Self::Copy => copy_tree(&source, &destination)?,
            #[cfg(target_os = "linux")]
            Self::Overlay(mounts) => {
                mounts.materialize(&source, &destination, writable)?;
                fs::set_permissions(
                    &destination,
                    fs::Permissions::from_mode(readonly_mode(&source)?),
                )?;
                for output in writable {
                    if let Writable::EmptyDirectory(output) = output {
                        mounts.empty_directory(output)?;
                    }
                }
            }
        }
        for output in writable {
            match output {
                Writable::Directory(output) | Writable::EmptyDirectory(output) => {
                    make_writable(output)?;
                }
                Writable::File(output) => {
                    let parent = output.parent().unwrap();
                    fs::set_permissions(
                        parent,
                        fs::Permissions::from_mode(readonly_mode(parent)? | 0o200),
                    )?;
                    if output.try_exists()? {
                        make_writable(output)?;
                    }
                }
                Writable::ScratchDirectory(directory) => fs::set_permissions(
                    directory,
                    fs::Permissions::from_mode(readonly_mode(directory)? | 0o200),
                )?,
            }
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn prepare_upper(
        source: &Path,
        destination: &Path,
        upper: &Path,
        writable: &[Writable<'_>],
    ) -> IoResult<()> {
        // Lower inodes can belong to an unmapped store owner. Prepare owned
        // copies before mounting; chmod cannot make those lower inodes ours.
        // Copy each directory output whole, preserving its existing inputs.
        if writable
            .iter()
            .any(|output| matches!(output, Writable::Directory(path) if *path == destination))
        {
            return copy_tree_contents(source, upper);
        }
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            let target = destination.join(entry.file_name());
            if !writable
                .iter()
                .any(|output| output.path().starts_with(&target))
            {
                continue;
            }
            let upper_target = upper.join(entry.file_name());
            if entry.file_type()?.is_dir() {
                fs::create_dir(&upper_target)?;
                Self::prepare_upper(&entry.path(), &target, &upper_target, writable)?;
            } else {
                fs::copy(entry.path(), &upper_target)?;
                fs::set_permissions(
                    &upper_target,
                    Permissions::from_mode(readonly_mode(&entry.path())?),
                )?;
            }
        }
        // Ancestors and scratch directories need owned metadata, not writable
        // siblings. The shared permission pass applies the exact allowances.
        fs::set_permissions(upper, Permissions::from_mode(readonly_mode(source)?))
    }
}

fn validate_output(source: &Path, destination: &Path, writable: &[Writable<'_>]) -> io::Result<()> {
    for (index, &output) in writable.iter().enumerate() {
        let relative = output
            .path()
            .strip_prefix(destination)
            .map_err(|_| invalid("compiler output must be inside the materialized tree"))?;
        if relative
            .components()
            .any(|part| !matches!(part, std::path::Component::Normal(_)))
        {
            return Err(invalid("compiler output must use normal path components"));
        }
        let original = source.join(relative);
        match output {
            Writable::Directory(_)
            | Writable::EmptyDirectory(_)
            | Writable::ScratchDirectory(_)
                if !fs::symlink_metadata(&original)?.is_dir() =>
            {
                return Err(invalid(
                    "compiler output must name an existing directory in the input tree",
                ));
            }
            Writable::EmptyDirectory(_) => {
                if relative.as_os_str().is_empty() || fs::read_dir(&original)?.next().is_some() {
                    return Err(invalid(
                        "private output must name an empty input subdirectory",
                    ));
                }
                if writable.iter().enumerate().any(|(other_index, other)| {
                    matches!(other, Writable::Directory(_) | Writable::EmptyDirectory(_))
                        && index != other_index
                        && (other.path().starts_with(output.path())
                            || output.path().starts_with(other.path()))
                }) {
                    return Err(invalid("private output directories must not overlap"));
                }
            }
            Writable::File(_) => {
                if relative.as_os_str().is_empty()
                    || !fs::symlink_metadata(original.parent().unwrap())?.is_dir()
                {
                    return Err(invalid(
                        "compiler file output needs an existing parent directory",
                    ));
                }
                match fs::symlink_metadata(original) {
                    Ok(info) if !info.is_file() => {
                        return Err(invalid("compiler file output must be regular"));
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                    _ => {}
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn make_writable(path: &Path) -> io::Result<()> {
    let info = fs::symlink_metadata(path)?;
    fs::set_permissions(
        path,
        fs::Permissions::from_mode(info.permissions().mode() | 0o200),
    )?;
    if info.is_dir() {
        for entry in fs::read_dir(path)? {
            make_writable(&entry?.path())?;
        }
    }
    Ok(())
}

fn copy_tree(source: &Path, destination: &Path) -> io::Result<()> {
    // Exclusive creation also rejects an existing directory or dangling symlink.
    fs::create_dir(destination)?;
    copy_tree_contents(source, destination)
}

// Destination must have been exclusively created by this owner. The generated
// export owner reserves both public roots before copying either payload.
fn copy_tree_contents(source: &Path, destination: &Path) -> io::Result<()> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let target = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target)?;
            fs::set_permissions(
                &target,
                fs::Permissions::from_mode(readonly_mode(&entry.path())?),
            )?;
        }
    }
    fs::set_permissions(
        destination,
        fs::Permissions::from_mode(readonly_mode(source)?),
    )
}

/// A script that changes permissions to write sources is also unsupported.
/// Verify after execution, before the outer builder can return success.
pub fn verify_unchanged(
    source: &Path,
    destination: &Path,
    writable: &[Writable<'_>],
) -> io::Result<()> {
    validate_output(source, destination, writable)?;
    verify_tree(source, destination, writable)
}

fn verify_tree(source: &Path, destination: &Path, writable: &[Writable<'_>]) -> io::Result<()> {
    if writable.iter().any(|output| {
        matches!(output, Writable::Directory(path) | Writable::EmptyDirectory(path)
                if *path == destination)
    }) {
        return validate_tree(destination);
    }
    let actual = fs::symlink_metadata(destination)?;
    let expected = fs::symlink_metadata(source)?;
    let mut expected_mode = readonly_mode(source)?;
    if writable
        .iter()
        .any(|output| matches!(output, Writable::ScratchDirectory(path) if *path == destination))
    {
        expected_mode |= 0o200;
    }
    let writable_files: std::collections::BTreeSet<_> = writable
        .iter()
        .filter_map(|output| match output {
            Writable::File(path) if path.parent() == Some(destination) => {
                Some(path.file_name().unwrap())
            }
            _ => None,
        })
        .collect();
    if !writable_files.is_empty() {
        expected_mode |= 0o200;
    }
    if actual.file_type() != expected.file_type()
        || actual.permissions().mode() & 0o777 != expected_mode
    {
        return Err(invalid(
            "workspace source type or mode changed during execution",
        ));
    }
    if expected.is_file() {
        if !same_contents(source, destination)? {
            return Err(invalid("workspace source bytes changed during execution"));
        }
    } else if expected.is_dir() {
        let names = |path: &Path| -> io::Result<std::collections::BTreeSet<_>> {
            fs::read_dir(path)?
                .filter_map(|entry| match entry {
                    Ok(entry) if writable_files.contains(entry.file_name().as_os_str()) => None,
                    other => Some(other.map(|entry| entry.file_name())),
                })
                .collect()
        };
        if names(source)? != names(destination)? {
            return Err(invalid("workspace source entries changed during execution"));
        }
        for name in &writable_files {
            match fs::symlink_metadata(destination.join(name)) {
                Ok(info) if !info.is_file() => {
                    return Err(invalid("compiler file output must remain regular"));
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
                _ => {}
            }
        }
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            if writable_files.contains(entry.file_name().as_os_str()) {
                continue;
            }
            verify_tree(
                &entry.path(),
                &destination.join(entry.file_name()),
                writable,
            )?;
        }
    } else {
        return Err(invalid("workspace source contains an unsupported entry"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{os::unix::fs::symlink, time::SystemTime};

    fn fixture() -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "source-wrap-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&root).unwrap();
        fs::create_dir(root.join("source")).unwrap();
        fs::write(root.join("source/input"), b"input").unwrap();
        root
    }

    #[test]
    fn overlay_rejects_state_outside_private_build_before_creating_it() {
        let root = fixture();
        let state = root.join("overlay");
        assert!(Materializer::overlay(&state).is_err());
        assert!(!state.exists());
        assert_eq!(fs::read(root.join("source/input")).unwrap(), b"input");
        fs::remove_dir_all(root).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn overlay_upper_preserves_declared_inputs_without_copying_unowned_siblings() {
        let root = fixture();
        let source = root.join("source");
        fs::create_dir_all(source.join("generated/full/nested")).unwrap();
        fs::create_dir_all(source.join("generated/full/undeclared/deep")).unwrap();
        fs::create_dir(source.join("generated/empty")).unwrap();
        fs::create_dir(source.join("scratch")).unwrap();
        fs::write(source.join("generated/full/nested/input"), b"nested input").unwrap();
        fs::write(
            source.join("generated/full/undeclared/deep/input"),
            b"whole subtree input",
        )
        .unwrap();
        fs::write(source.join("generated/existing"), b"previous output").unwrap();
        fs::write(source.join("generated/private.rs"), b"private input").unwrap();
        fs::write(source.join("scratch/input"), b"scratch input").unwrap();
        fs::set_permissions(source.join("generated"), Permissions::from_mode(0o751)).unwrap();
        let immutable = root.join("immutable");
        copy_tree(&source, &immutable).unwrap();
        for reversed in [false, true] {
            let upper = root.join(if reversed { "reversed-upper" } else { "upper" });
            fs::create_dir(&upper).unwrap();
            let full = upper.join("generated/full");
            let nested = full.join("nested");
            let nested_file = nested.join("input");
            let empty = upper.join("generated/empty");
            let existing = upper.join("generated/existing");
            let absent = upper.join("generated/absent");
            let scratch = upper.join("scratch");
            let mut writable = [
                Writable::Directory(&nested),
                Writable::File(&nested_file),
                Writable::ScratchDirectory(&nested),
                Writable::Directory(&full),
                Writable::Directory(&empty),
                Writable::File(&existing),
                Writable::File(&absent),
                Writable::ScratchDirectory(&scratch),
            ];
            if reversed {
                writable.reverse();
            }
            validate_output(&immutable, &upper, &writable).unwrap();
            Materializer::prepare_upper(&immutable, &upper, &upper, &writable).unwrap();
            assert_eq!(fs::read(&nested_file).unwrap(), b"nested input");
            assert_eq!(
                fs::read(full.join("undeclared/deep/input")).unwrap(),
                b"whole subtree input"
            );
            assert_eq!(fs::read(&existing).unwrap(), b"previous output");
            assert!(empty.is_dir());
            assert!(fs::read_dir(&empty).unwrap().next().is_none());
            for relative in [
                "input",
                "generated/private.rs",
                "scratch/input",
                "generated/absent",
            ] {
                assert!(!upper.join(relative).exists(), "{relative}");
            }
            for relative in [
                "",
                "generated",
                "generated/full",
                "generated/full/nested",
                "generated/full/nested/input",
                "generated/full/undeclared",
                "generated/full/undeclared/deep",
                "generated/full/undeclared/deep/input",
                "generated/empty",
                "generated/existing",
                "scratch",
            ] {
                assert_eq!(
                    fs::symlink_metadata(upper.join(relative))
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o777,
                    readonly_mode(&immutable.join(relative)).unwrap(),
                    "{relative}"
                );
            }
            verify_unchanged(&source, &immutable, &[]).unwrap();
            make_writable(&upper).unwrap();
        }
        make_writable(&immutable).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn byte_comparison_checks_empty_files_lengths_and_late_chunk_differences() {
        let root = fixture();
        let left = root.join("source/input");
        let right = root.join("other");
        for length in [0, 1, 65536, 2 * 65536 + 3] {
            let bytes: Vec<_> = (0..length).map(|i| (i % 251) as u8).collect();
            fs::write(&left, &bytes).unwrap();
            fs::write(&right, &bytes).unwrap();
            assert!(same_contents(&left, &right).unwrap());
            if length > 0 {
                let mut changed = bytes.clone();
                changed[length - 1] ^= 1;
                fs::write(&right, changed).unwrap();
                assert!(!same_contents(&left, &right).unwrap());
            }
            let mut longer = bytes;
            longer.push(0);
            fs::write(&right, longer).unwrap();
            assert!(!same_contents(&left, &right).unwrap());
        }
        fs::remove_file(&right).unwrap();
        assert!(same_contents(&left, &right).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn target_overlay_and_stdout_scratch_keep_source_and_generated_inputs_checked() {
        let root = fixture();
        let source = root.join("source");
        fs::create_dir(source.join("target")).unwrap();
        let generated = root.join("generated");
        fs::create_dir(&generated).unwrap();
        fs::write(generated.join("input.rs"), "generated source").unwrap();
        let copy = root.join("copy");
        let target = copy.join("target");
        let writable = [
            Writable::Directory(&target),
            Writable::ScratchDirectory(&copy),
        ];
        Materializer::Copy
            .materialize(&source, &copy, &writable)
            .unwrap();
        let generated_copy = target.join("project");
        Materializer::Copy
            .materialize(&generated, &generated_copy, &[])
            .unwrap();
        fs::write(target.join("output"), "artifact").unwrap();
        fs::create_dir(copy.join("temporary-rmeta")).unwrap();
        assert!(verify_unchanged(&source, &copy, &writable).is_err());
        fs::remove_dir(copy.join("temporary-rmeta")).unwrap();
        verify_unchanged(&source, &copy, &writable).unwrap();
        verify_unchanged(&generated, &generated_copy, &[]).unwrap();
        make_writable(&generated_copy).unwrap();
        fs::write(generated_copy.join("input.rs"), "mutated").unwrap();
        assert!(verify_unchanged(&generated, &generated_copy, &[]).is_err());
        make_writable(&copy).unwrap();
        fs::write(copy.join("input"), "mutated source").unwrap();
        assert!(verify_unchanged(&source, &copy, &writable).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn empty_targets_preserve_generated_inputs_and_stdout_scratch_validation() {
        let root = fixture();
        let source = root.join("source");
        let destination = root.join("copy");
        for name in ["target", "second-target"] {
            fs::create_dir(source.join(name)).unwrap();
        }
        let target = destination.join("target");
        let second = destination.join("second-target");
        let writable = [
            Writable::EmptyDirectory(&target),
            Writable::EmptyDirectory(&second),
            Writable::ScratchDirectory(&destination),
        ];
        Materializer::Copy
            .materialize(&source, &destination, &writable)
            .unwrap();
        let generated = root.join("generated");
        fs::create_dir(&generated).unwrap();
        fs::write(generated.join("input.rs"), b"generated").unwrap();
        let generated_copy = target.join("project");
        Materializer::Copy
            .materialize(&generated, &generated_copy, &[])
            .unwrap();
        fs::write(second.join("output"), b"artifact").unwrap();
        verify_unchanged(&source, &destination, &writable).unwrap();
        verify_unchanged(&generated, &generated_copy, &[]).unwrap();
        fs::write(destination.join("temporary"), b"scratch").unwrap();
        assert!(verify_unchanged(&source, &destination, &writable).is_err());
        fs::remove_file(destination.join("temporary")).unwrap();
        std::os::unix::fs::symlink("../input", second.join("link")).unwrap();
        assert!(verify_unchanged(&source, &destination, &writable).is_err());
        fs::remove_file(second.join("link")).unwrap();
        make_writable(&generated_copy).unwrap();
        fs::write(generated_copy.join("input.rs"), b"changed").unwrap();
        assert!(verify_unchanged(&generated, &generated_copy, &[]).is_err());
        make_writable(&destination).unwrap();
        fs::write(destination.join("input"), b"changed").unwrap();
        assert!(verify_unchanged(&source, &destination, &writable).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn private_targets_reject_nonempty_missing_aliased_or_overlapping_slots() {
        let root = fixture();
        let source = root.join("source");
        let destination = root.join("copy");
        fs::create_dir(source.join("target")).unwrap();
        fs::create_dir(source.join("generated")).unwrap();
        fs::write(source.join("generated/input.rs"), b"generated").unwrap();
        for name in ["", "generated", "input", "missing", "../outside"] {
            let output = destination.join(name);
            assert!(
                Materializer::Copy
                    .materialize(&source, &destination, &[Writable::EmptyDirectory(&output)])
                    .is_err(),
                "{name}"
            );
            assert!(!destination.exists());
        }
        let target = destination.join("target");
        for other in [
            Writable::EmptyDirectory(&target),
            Writable::Directory(&destination),
        ] {
            assert!(
                Materializer::Copy
                    .materialize(
                        &source,
                        &destination,
                        &[Writable::EmptyDirectory(&target), other],
                    )
                    .is_err()
            );
            assert!(!destination.exists());
        }
        fs::remove_dir(source.join("target")).unwrap();
        symlink("generated", source.join("target")).unwrap();
        assert!(
            Materializer::Copy
                .materialize(&source, &destination, &[Writable::EmptyDirectory(&target)])
                .is_err()
        );
        assert!(!destination.exists());
        fs::remove_file(source.join("target")).unwrap();
        fs::create_dir(source.join("target")).unwrap();
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("existing"), b"preserved").unwrap();
        assert!(
            Materializer::Copy
                .materialize(&source, &destination, &[Writable::EmptyDirectory(&target)])
                .is_err()
        );
        assert_eq!(
            fs::read(destination.join("existing")).unwrap(),
            b"preserved"
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn compiler_output_is_writable_without_unprotecting_generated_inputs() {
        let root = fixture();
        let source = root.join("source");
        let copy = root.join("copy");
        fs::create_dir(source.join("probe")).unwrap();
        fs::write(source.join("probe/old"), b"old").unwrap();
        let output = copy.join("probe");
        Materializer::Copy
            .materialize(&source, &copy, &[Writable::Directory(&output)])
            .unwrap();
        fs::write(output.join("old"), b"replacement").unwrap();
        fs::write(output.join("new"), b"new").unwrap();
        verify_unchanged(&source, &copy, &[Writable::Directory(&output)]).unwrap();
        assert_eq!(
            fs::metadata(copy.join("input"))
                .unwrap()
                .permissions()
                .mode()
                & 0o222,
            0
        );
        fs::set_permissions(copy.join("input"), fs::Permissions::from_mode(0o644)).unwrap();
        fs::write(copy.join("input"), b"changed").unwrap();
        assert!(verify_unchanged(&source, &copy, &[Writable::Directory(&output)]).is_err());
        fs::write(copy.join("input"), b"input").unwrap();
        fs::set_permissions(
            copy.join("input"),
            fs::Permissions::from_mode(readonly_mode(&source.join("input")).unwrap()),
        )
        .unwrap();
        verify_unchanged(&source, &copy, &[Writable::Directory(&output)]).unwrap();
        symlink("../input", output.join("link")).unwrap();
        assert!(verify_unchanged(&source, &copy, &[Writable::Directory(&output)]).is_err());
        fs::remove_file(output.join("link")).unwrap();
        make_writable(&copy).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn writable_output_must_be_an_existing_owned_directory() {
        let root = fixture();
        let source = root.join("source");
        let copy = root.join("copy");
        for output in [
            root.join("outside"),
            copy.join("input"),
            copy.join("missing"),
            copy.join("../source"),
        ] {
            assert!(
                Materializer::Copy
                    .materialize(&source, &copy, &[Writable::Directory(&output)])
                    .is_err()
            );
            assert!(!copy.exists());
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn file_output_preserves_siblings_and_allows_creation_replacement_and_removal() {
        for existing in [false, true] {
            let root = fixture();
            let source = root.join("source");
            let copy = root.join("copy");
            if existing {
                fs::write(source.join("output"), b"previous").unwrap();
            }
            let output = copy.join("output");
            let writable = &[Writable::File(&output)][..];
            Materializer::Copy
                .materialize(&source, &copy, writable)
                .unwrap();
            fs::write(&output, b"compiler result").unwrap();
            verify_unchanged(&source, &copy, writable).unwrap();
            fs::remove_file(&output).unwrap();
            verify_unchanged(&source, &copy, writable).unwrap();
            fs::write(copy.join("unexpected"), b"extra").unwrap();
            assert!(verify_unchanged(&source, &copy, writable).is_err());
            fs::remove_file(copy.join("unexpected")).unwrap();
            symlink("input", &output).unwrap();
            assert!(verify_unchanged(&source, &copy, writable).is_err());
            fs::remove_file(&output).unwrap();
            fs::create_dir(&output).unwrap();
            assert!(verify_unchanged(&source, &copy, writable).is_err());
            fs::remove_dir(&output).unwrap();
            assert_eq!(
                fs::metadata(copy.join("input"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o222,
                0
            );
            fs::set_permissions(copy.join("input"), fs::Permissions::from_mode(0o644)).unwrap();
            fs::write(copy.join("input"), b"corrupted sibling").unwrap();
            assert!(verify_unchanged(&source, &copy, writable).is_err());
            make_writable(&copy).unwrap();
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn file_output_requires_regular_file_and_existing_parent_inside_tree() {
        let root = fixture();
        let source = root.join("source");
        let copy = root.join("copy");
        fs::create_dir(source.join("directory")).unwrap();
        for path in [
            root.join("outside"),
            copy.clone(),
            copy.join("directory"),
            copy.join("missing/file"),
            copy.join("../source/input"),
        ] {
            assert!(
                Materializer::Copy
                    .materialize(&source, &copy, &[Writable::File(&path)])
                    .is_err()
            );
            assert!(!copy.exists());
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn temporary_compiler_entries_must_be_removed_without_changing_inputs() {
        let root = fixture();
        let source = root.join("source");
        let copy = root.join("copy");
        let writable = &[Writable::ScratchDirectory(&copy)][..];
        Materializer::Copy
            .materialize(&source, &copy, writable)
            .unwrap();
        fs::create_dir(copy.join("rmeta-temporary")).unwrap();
        fs::write(copy.join("rmeta-temporary/output"), b"metadata").unwrap();
        assert!(verify_unchanged(&source, &copy, writable).is_err());
        fs::remove_dir_all(copy.join("rmeta-temporary")).unwrap();
        verify_unchanged(&source, &copy, writable).unwrap();
        fs::remove_file(copy.join("input")).unwrap();
        assert!(verify_unchanged(&source, &copy, writable).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn copy_is_readonly_and_detects_bytes_modes_and_added_entries() {
        let root = fixture();
        let source = root.join("source");
        let copy = root.join("copy");
        Materializer::Copy.materialize(&source, &copy, &[]).unwrap();
        verify_unchanged(&source, &copy, &[]).unwrap();
        assert_eq!(
            fs::metadata(copy.join("input"))
                .unwrap()
                .permissions()
                .mode()
                & 0o222,
            0
        );
        fs::set_permissions(copy.join("input"), fs::Permissions::from_mode(0o644)).unwrap();
        assert!(verify_unchanged(&source, &copy, &[]).is_err());
        fs::write(copy.join("input"), b"other").unwrap();
        fs::set_permissions(
            copy.join("input"),
            fs::Permissions::from_mode(readonly_mode(&source.join("input")).unwrap()),
        )
        .unwrap();
        assert!(verify_unchanged(&source, &copy, &[]).is_err());
        fs::set_permissions(copy.join("input"), fs::Permissions::from_mode(0o644)).unwrap();
        fs::write(copy.join("input"), b"input").unwrap();
        fs::set_permissions(
            copy.join("input"),
            fs::Permissions::from_mode(readonly_mode(&source.join("input")).unwrap()),
        )
        .unwrap();
        verify_unchanged(&source, &copy, &[]).unwrap();
        fs::set_permissions(&copy, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(copy.join("extra"), b"extra").unwrap();
        fs::set_permissions(
            &copy,
            fs::Permissions::from_mode(readonly_mode(&source).unwrap()),
        )
        .unwrap();
        assert!(verify_unchanged(&source, &copy, &[]).is_err());
        fs::set_permissions(&copy, fs::Permissions::from_mode(0o755)).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unsafe_sources_and_existing_or_nested_destination_reject() {
        let root = fixture();
        let source = root.join("source");
        assert!(
            Materializer::Copy
                .materialize(&source, &source.join("nested"), &[])
                .is_err()
        );
        assert!(
            Materializer::Copy
                .materialize(&source, &source, &[])
                .is_err()
        );
        fs::create_dir(root.join("copy")).unwrap();
        assert!(
            Materializer::Copy
                .materialize(&source, &root.join("copy"), &[])
                .is_err()
        );
        symlink("input", source.join("link")).unwrap();
        assert!(validate_tree(&source).is_err());
        assert!(
            Materializer::Copy
                .materialize(&source, &root.join("new-copy"), &[])
                .is_err()
        );
        assert!(!root.join("new-copy").exists());
        fs::remove_dir_all(root).unwrap();
    }
}
