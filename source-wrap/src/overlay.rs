//! Private Linux mounts owned by one single-threaded compiler builder.
use std::{
    ffi::{CString, c_char, c_int, c_ulong, c_void},
    fs, io,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
};

// Keep the standalone source helper std-only, as required by makeHelper.
unsafe extern "C" {
    fn unshare(flags: c_int) -> c_int;
    fn getuid() -> u32;
    fn getgid() -> u32;
    fn mount(
        source: *const c_char,
        target: *const c_char,
        filesystem: *const c_char,
        flags: c_ulong,
        data: *const c_void,
    ) -> c_int;
}

const CLONE_NEWUSER: c_int = 0x10000000;
const CLONE_NEWNS: c_int = 0x00020000;
const MS_BIND: c_ulong = 4096;
const MS_REC: c_ulong = 16384;
const MS_PRIVATE: c_ulong = 262144;

pub struct Mounts {
    root: PathBuf,
    next: usize,
}

impl Mounts {
    pub(super) fn new(root: &Path) -> io::Result<Self> {
        if std::env::current_dir()? != Path::new("/build")
            || root.parent() != Some(Path::new("/build"))
            || root.file_name().is_none()
        {
            return Err(io::Error::other("overlay requires a private /build owner"));
        }
        if fs::read_dir("/proc/self/task")?.count() != 1 {
            return Err(io::Error::other("overlay owner must be single-threaded"));
        }
        // Exclusive ownership precedes any namespace change. Never mount over
        // a pre-existing directory or enter a shared mount namespace.
        fs::create_dir(root)?;
        // SAFETY: these calls have no pointer arguments. unshare changes only
        // this single-threaded builder's user and mount namespaces.
        let (uid, gid) = unsafe { (getuid(), getgid()) };
        if unsafe { unshare(CLONE_NEWUSER | CLONE_NEWNS) } != 0 {
            return Err(io::Error::last_os_error());
        }
        fs::write("/proc/self/uid_map", format!("{uid} {uid} 1\n"))?;
        fs::write("/proc/self/setgroups", "deny\n")?;
        fs::write("/proc/self/gid_map", format!("{gid} {gid} 1\n"))?;
        // MS_REC | MS_PRIVATE prevents propagation to any ancestor namespace.
        // SAFETY: every non-null pointer refers to a live NUL-terminated string.
        if unsafe {
            mount(
                std::ptr::null(),
                c"/".as_ptr(),
                std::ptr::null(),
                MS_REC | MS_PRIVATE,
                std::ptr::null(),
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            root: root.into(),
            next: 0,
        })
    }

    pub(super) fn materialize(&mut self, source: &Path, destination: &Path) -> io::Result<()> {
        if !source.starts_with("/nix/store")
            || source == Path::new("/nix/store")
            || !["/build", "/tmp"]
                .iter()
                .any(|root| destination.starts_with(root) && destination != Path::new(root))
        {
            return Err(io::Error::other(
                "overlay requires an immutable store input and private /build or /tmp destination",
            ));
        }
        if source.starts_with(&self.root)
            || self.root.starts_with(source)
            || destination.starts_with(&self.root)
            || self.root.starts_with(destination)
        {
            return Err(io::Error::other(
                "overlay state overlaps source or destination",
            ));
        }
        let state = self.root.join(self.next.to_string());
        self.next += 1;
        fs::create_dir(&state)?;
        let upper = state.join("upper");
        let work = state.join("work");
        fs::create_dir(&upper)?;
        fs::create_dir(&work)?;
        // mount(2)'s option list uses commas and colons as delimiters. Reject
        // ambiguous paths instead of allowing them to select another input.
        let mut options = Vec::new();
        for (prefix, path) in [
            (b"lowerdir=".as_slice(), source),
            (b",upperdir=", &upper),
            (b",workdir=", &work),
        ] {
            let bytes = path.as_os_str().as_bytes();
            if bytes.iter().any(|b| matches!(b, b',' | b':' | b'\\' | 0)) {
                return Err(io::Error::other("unsupported overlay path delimiter"));
            }
            options.extend_from_slice(prefix);
            options.extend_from_slice(bytes);
        }
        options.extend_from_slice(b",userxattr");
        let options = CString::new(options)?;
        let target = CString::new(destination.as_os_str().as_bytes())?;
        fs::create_dir(destination)?;
        // SAFETY: the option and target buffers live for the synchronous call.
        // The private namespace was established by new; the lower input is
        // immutable, and exclusively owned upper/work directories hold writes.
        if unsafe {
            mount(
                c"overlay".as_ptr(),
                target.as_ptr(),
                c"overlay".as_ptr(),
                0,
                options.as_ptr().cast(),
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub(super) fn empty_directory(&mut self, destination: &Path) -> io::Result<()> {
        // Materializer validated an empty immutable placeholder before mounting
        // its source tree. Check the live slot too: never hide generated inputs.
        let info = fs::symlink_metadata(destination)?;
        if !info.is_dir() || fs::read_dir(destination)?.next().is_some() {
            return Err(io::Error::other(
                "private output slot is not an empty directory",
            ));
        }
        let storage = self.root.join(self.next.to_string());
        self.next += 1;
        fs::create_dir(&storage)?;
        fs::set_permissions(&storage, info.permissions())?;
        let source = CString::new(storage.as_os_str().as_bytes())?;
        let target = CString::new(destination.as_os_str().as_bytes())?;
        // SAFETY: both buffers live for this synchronous call. The empty source
        // is exclusively owned, and new established a private mount namespace.
        // The bind keeps compiler paths intact while dependency copies and
        // output links use the ordinary /build filesystem, outside overlayfs.
        if unsafe {
            mount(
                source.as_ptr(),
                target.as_ptr(),
                std::ptr::null(),
                MS_BIND,
                std::ptr::null(),
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}
