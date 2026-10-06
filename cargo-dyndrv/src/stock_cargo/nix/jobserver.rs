//! Transport Cargo's existing pipe, without allocating tokens or a second pool.
//! The private same-user daemon must mount the configured directory at ROOT.
//! Socket names bind immutable requests; host paths and FD numbers are transport.
use std::{
    ffi::OsStr,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    mem::{self, MaybeUninit},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::{
            fs::{FileTypeExt, MetadataExt, OpenOptionsExt},
            net::{UnixListener, UnixStream},
            process::CommandExt,
        },
    },
    path::{Path, PathBuf},
    process::Command,
    thread::{self, JoinHandle},
};

pub(super) const ROOT: &str = "/dyndrv-jobserver";

pub(super) struct Pipe {
    fds: [OwnedFd; 2],
}

impl Pipe {
    fn validate(&self) -> io::Result<()> {
        let mut identity = None;
        for (fd, access) in self.fds.iter().zip([libc::O_RDONLY, libc::O_WRONLY]) {
            let mut info = MaybeUninit::<libc::stat>::uninit();
            // SAFETY: the owned FD is live and fstat initializes info on success.
            if unsafe { libc::fstat(fd.as_raw_fd(), info.as_mut_ptr()) } != 0 {
                return Err(io::Error::last_os_error());
            }
            // SAFETY: fstat succeeded above.
            let info = unsafe { info.assume_init() };
            // SAFETY: F_GETFL only reads flags from the live FD.
            let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
            if flags < 0 {
                return Err(io::Error::last_os_error());
            }
            let pair = (info.st_dev, info.st_ino);
            if info.st_mode & libc::S_IFMT != libc::S_IFIFO
                || flags & libc::O_ACCMODE != access
                || identity.is_some_and(|old| old != pair)
            {
                return Err(io::Error::other(
                    "jobserver requires matching read/write pipe ends",
                ));
            }
            identity = Some(pair);
        }
        Ok(())
    }

    fn from_flags(flags: &OsStr) -> io::Result<Self> {
        let flags = flags
            .to_str()
            .ok_or_else(|| io::Error::other("non-UTF8 Cargo jobserver flags"))?;
        let mut pair = None;
        for option in flags.split_ascii_whitespace() {
            if let Some(value) = option
                .strip_prefix("--jobserver-auth=")
                .or_else(|| option.strip_prefix("--jobserver-fds="))
            {
                let (read, write) = value
                    .split_once(',')
                    .ok_or_else(|| io::Error::other("Cargo jobserver must use pipe descriptors"))?;
                let parse = |value: &str| {
                    value
                        .parse::<RawFd>()
                        .ok()
                        .filter(|fd| *fd >= 3)
                        .ok_or_else(|| io::Error::other("invalid Cargo jobserver descriptor"))
                };
                let current = [parse(read)?, parse(write)?];
                if current[0] == current[1] || pair.is_some_and(|old| old != current) {
                    return Err(io::Error::other("conflicting Cargo jobserver descriptors"));
                }
                pair = Some(current);
            }
        }
        let pair = pair.ok_or_else(|| io::Error::other("Cargo jobserver pipe is missing"))?;
        let duplicate = |fd| {
            // SAFETY: fcntl either duplicates the inherited FD or reports EBADF.
            let new = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
            if new < 0 {
                Err(io::Error::last_os_error())
            } else {
                // SAFETY: this call uniquely owns the newly duplicated FD.
                Ok(unsafe { OwnedFd::from_raw_fd(new) })
            }
        };
        let pipe = Self {
            fds: [duplicate(pair[0])?, duplicate(pair[1])?],
        };
        pipe.validate()?;
        Ok(pipe)
    }

    fn same_user(peer: &UnixStream) -> io::Result<()> {
        let mut credentials = MaybeUninit::<libc::ucred>::uninit();
        let mut size = mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: the live socket, output buffer and length have their required types.
        if unsafe {
            libc::getsockopt(
                peer.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                credentials.as_mut_ptr().cast(),
                &mut size,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: getsockopt succeeded and Linux returns a ucred for SO_PEERCRED.
        if size as usize != mem::size_of::<libc::ucred>()
            || unsafe { credentials.assume_init().uid != libc::geteuid() }
        {
            return Err(io::Error::other(
                "jobserver requires a private same-user Nix builder",
            ));
        }
        Ok(())
    }

    fn send(&self, peer: &UnixStream) -> io::Result<()> {
        let mut byte = b'P';
        let mut vector = libc::iovec {
            iov_base: (&mut byte as *mut u8).cast(),
            iov_len: 1,
        };
        let mut control = [0usize; 8];
        // SAFETY: an all-zero msghdr is valid before assigning its pointer fields.
        let mut message: libc::msghdr = unsafe { mem::zeroed() };
        message.msg_iov = &mut vector;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        let raw = self.fds.each_ref().map(AsRawFd::as_raw_fd);
        // SAFETY: control is aligned and large enough for one header and two FDs.
        unsafe {
            message.msg_controllen = libc::CMSG_SPACE(mem::size_of_val(&raw) as u32) as usize;
            let header = libc::CMSG_FIRSTHDR(&message);
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (*header).cmsg_len = libc::CMSG_LEN(mem::size_of_val(&raw) as u32) as usize;
            std::ptr::copy_nonoverlapping(raw.as_ptr(), libc::CMSG_DATA(header).cast(), raw.len());
            if libc::sendmsg(peer.as_raw_fd(), &message, libc::MSG_NOSIGNAL) != 1 {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }

    fn receive_from(path: &Path, key: &str) -> io::Result<Self> {
        let mut peer = UnixStream::connect(path)?;
        Self::same_user(&peer)?;
        peer.write_all(key.as_bytes())?;
        let mut byte = 0u8;
        let mut vector = libc::iovec {
            iov_base: (&mut byte as *mut u8).cast(),
            iov_len: 1,
        };
        let mut control = [0usize; 8];
        // SAFETY: an all-zero msghdr is valid before assigning its pointer fields.
        let mut message: libc::msghdr = unsafe { mem::zeroed() };
        message.msg_iov = &mut vector;
        message.msg_iovlen = 1;
        message.msg_control = control.as_mut_ptr().cast();
        message.msg_controllen = mem::size_of_val(&control);
        // SAFETY: all buffers are live, correctly sized and aligned through recvmsg.
        let count =
            unsafe { libc::recvmsg(peer.as_raw_fd(), &mut message, libc::MSG_CMSG_CLOEXEC) };
        if count < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut fds = Vec::new();
        // SAFETY: recvmsg initialized the ancillary headers. Immediately own every
        // received FD, including malformed/excess responses, so rejection closes it.
        unsafe {
            let mut header = libc::CMSG_FIRSTHDR(&message);
            while !header.is_null() {
                if (*header).cmsg_level == libc::SOL_SOCKET
                    && (*header).cmsg_type == libc::SCM_RIGHTS
                {
                    let bytes = (*header)
                        .cmsg_len
                        .saturating_sub(libc::CMSG_LEN(0) as usize);
                    for i in 0..bytes / mem::size_of::<RawFd>() {
                        fds.push(OwnedFd::from_raw_fd(
                            *libc::CMSG_DATA(header).cast::<RawFd>().add(i),
                        ));
                    }
                }
                header = libc::CMSG_NXTHDR(&message, header);
            }
        }
        if count != 1 || byte != b'P' || message.msg_flags & libc::MSG_CTRUNC != 0 || fds.len() != 2
        {
            return Err(io::Error::other("invalid jobserver descriptor response"));
        }
        let pipe = Self {
            fds: fds.try_into().unwrap(),
        };
        pipe.validate()?;
        Ok(pipe)
    }

    pub(super) fn receive(key: &str) -> io::Result<Self> {
        Self::receive_from(&Path::new(ROOT).join(key), key)
    }

    pub(super) fn configure(&self, command: &mut Command) {
        let [read, write] = self.fds.each_ref().map(AsRawFd::as_raw_fd);
        command.env(
            "CARGO_MAKEFLAGS",
            format!("-j --jobserver-fds={read},{write} --jobserver-auth={read},{write}"),
        );
        // SAFETY: this child-only callback uses only async-signal-safe fcntl.
        // Pipe remains live through spawn; the parent's CLOEXEC bits stay intact.
        unsafe {
            command.pre_exec(move || {
                for fd in [read, write] {
                    if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
    }
}

pub(super) struct Broker {
    _directory: File,
    _owner: File,
    path: PathBuf,
    stop: UnixStream,
    thread: Option<JoinHandle<io::Result<bool>>>,
}

impl Broker {
    pub(super) fn directory(path: &Path) -> io::Result<File> {
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path)?;
        let info = directory.metadata()?;
        // SAFETY: geteuid takes no arguments and has no memory effects.
        if !path.is_absolute()
            || path.canonicalize()? != path
            || info.uid() != unsafe { libc::geteuid() }
            || info.mode() & 0o077 != 0
        {
            return Err(io::Error::other(
                "jobserver directory must be normalized, owned and private",
            ));
        }
        Ok(directory)
    }

    fn start(directory: &Path, key: &str, pipe: Pipe) -> io::Result<Self> {
        if key.len() != 64 || !key.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(io::Error::other(
                "jobserver socket must bind a request digest",
            ));
        }
        let directory = Self::directory(directory)?;
        // Resolving through a held directory FD avoids Unix socket path-length
        // limits without changing cwd or binding an abstract/global namespace.
        let path = PathBuf::from(format!("/proc/self/fd/{}/{key}", directory.as_raw_fd()));
        // Keep the lock inode between requests. Unlinking it on normal exit
        // would let concurrent openers lock different inodes for the same key.
        // The kernel releases this ownership even on SIGKILL, when Drop cannot
        // remove the socket. Only a new exclusive owner may remove that socket.
        let owner = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(path.with_extension("lock"))?;
        let info = owner.metadata()?;
        if !info.is_file()
            || info.nlink() != 1
            || info.uid() != directory.metadata()?.uid()
            || info.mode() & 0o077 != 0
        {
            return Err(io::Error::other("invalid jobserver ownership file"));
        }
        // SAFETY: owner is live; nonblocking flock never waits for another build.
        if unsafe { libc::flock(owner.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(io::Error::last_os_error());
        }
        match fs::symlink_metadata(&path) {
            Ok(info)
                if info.file_type().is_socket() && info.uid() == directory.metadata()?.uid() =>
            {
                fs::remove_file(&path)?;
            }
            Ok(_) => return Err(io::Error::other("unexpected jobserver endpoint file")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let listener = UnixListener::bind(&path)?;
        let (stop, cancel) = UnixStream::pair().inspect_err(|_| {
            let _ = fs::remove_file(&path);
        })?;
        let mut broker = Self {
            _directory: directory,
            _owner: owner,
            path,
            stop,
            thread: None,
        };
        listener.set_nonblocking(true)?;
        let key = key.to_owned();
        broker.thread =
            Some(thread::Builder::new().spawn(move || Self::serve(listener, cancel, &key, pipe))?);
        Ok(broker)
    }

    pub(super) fn from_cargo(directory: &Path, key: &str) -> io::Result<Self> {
        let flags = std::env::var_os("CARGO_MAKEFLAGS")
            .ok_or_else(|| io::Error::other("Cargo did not provide its jobserver"))?;
        Self::start(directory, key, Pipe::from_flags(&flags)?)
    }

    fn ready(fd: RawFd, cancel: &UnixStream) -> io::Result<bool> {
        let mut events = [
            libc::pollfd {
                fd: cancel.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        loop {
            // SAFETY: events contains two initialized pollfd values for live FDs.
            if unsafe { libc::poll(events.as_mut_ptr(), events.len() as _, -1) } < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if events[0].revents != 0 {
                return Ok(false);
            }
            if events[1].revents != 0 {
                return Ok(true);
            }
        }
    }

    fn serve(
        listener: UnixListener,
        cancel: UnixStream,
        key: &str,
        pipe: Pipe,
    ) -> io::Result<bool> {
        loop {
            if !Self::ready(listener.as_raw_fd(), &cancel)? {
                return Ok(false);
            }
            let (mut peer, _) = match listener.accept() {
                Ok(peer) => peer,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                Err(error) => return Err(error),
            };
            Pipe::same_user(&peer)?;
            peer.set_nonblocking(true)?;
            let mut request = [0u8; 64];
            let mut filled = 0;
            while filled < request.len() {
                if !Self::ready(peer.as_raw_fd(), &cancel)? {
                    return Ok(false);
                }
                match peer.read(&mut request[filled..]) {
                    Ok(0) => return Err(io::Error::other("incomplete jobserver request")),
                    Ok(count) => filled += count,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
                    Err(error) => return Err(error),
                }
            }
            if request.as_slice() != key.as_bytes() {
                return Err(io::Error::other("jobserver caller request does not match"));
            }
            pipe.send(&peer)?;
            return Ok(true);
        }
    }

    pub(super) fn finish(mut self) -> io::Result<bool> {
        self.stop.write_all(&[0]).ok();
        self.thread
            .take()
            .unwrap()
            .join()
            .map_err(|_| io::Error::other("jobserver transport thread panicked"))?
    }
}

impl Drop for Broker {
    fn drop(&mut self) {
        self.stop.write_all(&[0]).ok();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        // The directory FD remains owned until unlink finishes. A failed bind
        // never constructs Broker and therefore cannot unlink another owner.
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs::DirBuilder,
        os::unix::fs::{DirBuilderExt, PermissionsExt},
        time::{SystemTime, UNIX_EPOCH},
    };

    const KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn root() -> PathBuf {
        let root = std::env::temp_dir().canonicalize().unwrap().join(format!(
            "dyndrv-jobserver-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        DirBuilder::new().mode(0o700).create(&root).unwrap();
        root
    }

    fn pipe() -> Pipe {
        let mut fds = [-1; 2];
        // SAFETY: pipe2 initializes both FD slots on success; this helper owns them.
        assert_eq!(
            unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) },
            0
        );
        Pipe {
            fds: fds.map(|fd| unsafe { OwnedFd::from_raw_fd(fd) }),
        }
    }

    fn flags(pipe: &Pipe) -> String {
        let [read, write] = pipe.fds.each_ref().map(AsRawFd::as_raw_fd);
        format!("-j --jobserver-fds={read},{write} --jobserver-auth={read},{write}")
    }

    #[test]
    fn cargo_flags_require_two_matching_pipe_ends() {
        let original = pipe();
        Pipe::from_flags(OsStr::new(&flags(&original))).unwrap();
        let [read, write] = original.fds.each_ref().map(AsRawFd::as_raw_fd);
        for value in [
            "".into(),
            "-j8".into(),
            "--jobserver-auth=fifo:/tmp/other".into(),
            "--jobserver-auth=-1,-2".into(),
            format!("--jobserver-auth=0,{write}"),
            format!("--jobserver-auth={read},{read}"),
            format!("--jobserver-auth={write},{read}"),
            format!("{} --jobserver-auth={write},{read}", flags(&original)),
        ] {
            assert!(Pipe::from_flags(OsStr::new(&value)).is_err(), "{value}");
        }
        let other = pipe();
        assert!(
            Pipe::from_flags(OsStr::new(&format!(
                "--jobserver-auth={read},{}",
                other.fds[1].as_raw_fd()
            )))
            .is_err()
        );
    }

    #[test]
    fn transport_conserves_tokens_and_leaves_shared_flags_unchanged() {
        let root = root();
        let original = pipe();
        let flags_before = original
            .fds
            .each_ref()
            .map(|fd| unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) });
        let mut writer = File::from(original.fds[1].try_clone().unwrap());
        writer.write_all(b"x").unwrap();
        let broker = Broker::start(
            &root,
            KEY,
            Pipe::from_flags(OsStr::new(&flags(&original))).unwrap(),
        )
        .unwrap();
        let received = Pipe::receive_from(&broker.path, KEY).unwrap();
        assert!(broker.finish().unwrap());
        let mut reader = File::from(received.fds[0].try_clone().unwrap());
        let mut byte = [0];
        reader.read_exact(&mut byte).unwrap();
        assert_eq!(&byte, b"x");
        let mut original_reader = File::from(original.fds[0].try_clone().unwrap());
        assert_eq!(
            original_reader.read(&mut byte).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        File::from(received.fds[1].try_clone().unwrap())
            .write_all(b"x")
            .unwrap();
        original_reader.read_exact(&mut byte).unwrap();
        assert_eq!(&byte, b"x");
        assert_eq!(
            flags_before,
            original
                .fds
                .each_ref()
                .map(|fd| unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) })
        );
        assert!(!root.join(KEY).exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn independent_compilers_share_the_same_token_pool() {
        let root = root();
        let original = pipe();
        File::from(original.fds[1].try_clone().unwrap())
            .write_all(b"xy")
            .unwrap();
        let mut clients = Vec::new();
        for key in [KEY.to_owned(), "a".repeat(64)] {
            let broker = Broker::start(
                &root,
                &key,
                Pipe::from_flags(OsStr::new(&flags(&original))).unwrap(),
            )
            .unwrap();
            clients.push(Pipe::receive_from(&broker.path, &key).unwrap());
            assert!(broker.finish().unwrap());
        }
        for (client, expected) in clients.iter().zip(b"xy") {
            let mut byte = [0];
            File::from(client.fds[0].try_clone().unwrap())
                .read_exact(&mut byte)
                .unwrap();
            assert_eq!(byte[0], *expected);
        }
        let mut reader = File::from(original.fds[0].try_clone().unwrap());
        assert_eq!(
            reader.read(&mut [0]).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        for client in clients {
            File::from(client.fds[1].try_clone().unwrap())
                .write_all(b"x")
                .unwrap();
        }
        assert_eq!(reader.read(&mut [0; 3]).unwrap(), 2);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn absent_or_partial_connections_cancel_without_consuming_tokens() {
        for partial in [false, true] {
            let root = root();
            let original = pipe();
            File::from(original.fds[1].try_clone().unwrap())
                .write_all(b"x")
                .unwrap();
            let broker = Broker::start(
                &root,
                KEY,
                Pipe::from_flags(OsStr::new(&flags(&original))).unwrap(),
            )
            .unwrap();
            let peer = partial.then(|| {
                let mut peer = UnixStream::connect(&broker.path).unwrap();
                peer.write_all(b"partial").unwrap();
                peer
            });
            assert!(!broker.finish().unwrap());
            drop(peer);
            let mut byte = [0];
            File::from(original.fds[0].try_clone().unwrap())
                .read_exact(&mut byte)
                .unwrap();
            assert_eq!(&byte, b"x");
            assert!(!root.join(KEY).exists());
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn wrong_request_cannot_receive_descriptors() {
        let root = root();
        let broker = Broker::start(&root, KEY, pipe()).unwrap();
        assert!(Pipe::receive_from(&broker.path, &"f".repeat(64)).is_err());
        assert!(broker.finish().is_err());
        assert!(!root.join(KEY).exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn bind_collision_preserves_the_original_owner_and_supports_long_host_paths() {
        let root = root();
        let long = root.join("x".repeat(100));
        DirBuilder::new().mode(0o700).create(&long).unwrap();
        let broker = Broker::start(&long, KEY, pipe()).unwrap();
        assert!(Broker::start(&long, KEY, pipe()).is_err());
        Pipe::receive_from(&broker.path, KEY).unwrap();
        assert!(broker.finish().unwrap());
        assert!(!long.join(KEY).exists());
        fs::set_permissions(&long, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(Broker::start(&long, KEY, pipe()).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    // Run in its own process so SIGKILL tests the kernel's ownership cleanup.
    #[test]
    fn killed_broker_child() {
        let Some(root) = std::env::var_os("DYNDRV_TEST_KILLED_BROKER_ROOT") else {
            return;
        };
        let _broker = Broker::start(Path::new(&root), KEY, pipe()).unwrap();
        println!("broker-ready");
        loop {
            thread::park();
        }
    }

    #[test]
    fn killed_owner_does_not_block_a_later_compiler() {
        use std::io::BufRead;
        use std::process::Stdio;
        let root = root();
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "stock_cargo::nix::jobserver::tests::killed_broker_child",
                "--nocapture",
            ])
            .env("DYNDRV_TEST_KILLED_BROKER_ROOT", &root)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut output = io::BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        loop {
            assert_ne!(
                output.read_line(&mut line).unwrap(),
                0,
                "child exited before binding"
            );
            if line.contains("broker-ready") {
                break;
            }
            line.clear();
        }
        child.kill().unwrap();
        assert!(!child.wait().unwrap().success());
        assert!(root.join(KEY).exists(), "SIGKILL left no recovery case");
        let broker = Broker::start(&root, KEY, pipe()).unwrap();
        Pipe::receive_from(&broker.path, KEY).unwrap();
        assert!(broker.finish().unwrap());
        assert!(!root.join(KEY).exists());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn recovery_preserves_unexpected_files_and_reuses_the_ownership_inode() {
        let root = root();
        let endpoint = root.join(KEY);
        fs::write(&endpoint, "unrelated").unwrap();
        assert!(Broker::start(&root, KEY, pipe()).is_err());
        assert_eq!(fs::read_to_string(&endpoint).unwrap(), "unrelated");
        fs::remove_file(&endpoint).unwrap();
        std::os::unix::fs::symlink("missing", &endpoint).unwrap();
        assert!(Broker::start(&root, KEY, pipe()).is_err());
        assert!(
            endpoint
                .symlink_metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
        fs::remove_file(&endpoint).unwrap();
        let lock = root.join(format!("{KEY}.lock"));
        let inode = lock.metadata().unwrap().ino();
        for _ in 0..2 {
            let broker = Broker::start(&root, KEY, pipe()).unwrap();
            assert!(!broker.finish().unwrap());
            assert_eq!(lock.metadata().unwrap().ino(), inode);
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn child_inherits_received_descriptors_without_changing_parent_cloexec() {
        let root = root();
        let broker = Broker::start(&root, KEY, pipe()).unwrap();
        let received = Pipe::receive_from(&broker.path, KEY).unwrap();
        assert!(broker.finish().unwrap());
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "test -p /proc/self/fd/$1 && test -p /proc/self/fd/$2 && test -n \"$CARGO_MAKEFLAGS\"", "jobserver"])
            .args(received.fds.iter().map(|fd| fd.as_raw_fd().to_string()));
        received.configure(&mut command);
        assert!(command.status().unwrap().success());
        for fd in &received.fds {
            assert_ne!(
                unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
                0
            );
        }
        fs::remove_dir_all(root).unwrap();
    }
}
