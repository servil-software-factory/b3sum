//! Unified file-end, descriptor-identity, and bounded-read state machines for `b3sum` (#468).
//!
//! Ordinary reading paths in `b3sum` share three invariants enforced here
//! (memory-mapped paths hash the mapped extent, and `--outboard`'s non-mapped
//! fallback still buffers the whole input):
//! 1. **Only a waiting read returning 0 ends a file.** A non-blocking cache
//!    probe (`RWF_NOWAIT`) returning `0` or an error (such as `EAGAIN` or
//!    sandbox `EPERM`) never ends a file; a real blocking read error (`EIO`,
//!    etc.) is propagated rather than swallowed as a cache miss.
//! 2. **Initialized prefix length lives in `Vec::len()`.** Buffers grow safely
//!    without callers carrying an unchecked raw offset, and small-hint regular
//!    files promote to incremental streaming once they exceed [`SMALL_MEM_CAP`]
//!    so a virtual or growing file reporting a tiny size cannot exhaust memory.
//! 3. **Descriptor identity is validated by the OS (`fstat` / `stat`), never
//!    by lexical path equality alone.** Relative paths under `/dev` and `/proc`
//!    (including from `/`) are nominated lexically and confirmed via `stat`;
//!    only genuine consumable streams (FIFOs/pipes, Unix sockets, terminals —
//!    not block devices or cloning char devices) reuse fd 0 directly; on macOS,
//!    verified `fd(4)` regular-file aliases (`/dev/stdin`, `/dev/fd/0`) hash
//!    fd 0 from byte 0 via positioned `pread` or `mmap` without moving fd 0's
//!    shared offset.

use std::fs::File;
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

use std::os::fd::{AsFd, AsRawFd, BorrowedFd, RawFd};

/// Ceiling for reading a small-hinted file into a single contiguous `Vec<u8>`.
/// Files whose reads pass this bound promote to incremental streaming hashing.
pub(crate) const SMALL_MEM_CAP: usize = 4 << 20;

/// Device and inode pair identifying an open Unix file description's target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct StreamId {
    pub(crate) dev: u64,
    pub(crate) ino: u64,
}

/// File semantics relevant to descriptor aliasing and offset ownership.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FileKind {
    /// Regular file (`S_IFREG`).
    Regular,
    /// Consumable non-seekable stream: pipe/FIFO (`S_IFIFO`), socket
    /// (`S_IFSOCK`), or a terminal (`S_IFCHR` where `isatty(fd) == 1`).
    /// Excludes seekable block devices (`S_IFBLK`) and cloning devices such as
    /// `/dev/ptmx`, whose named open yields an independent description.
    Stream,
    /// Any other special file (directories, block devices, non-tty char devices).
    Other,
}

/// Metadata queried directly from an open file descriptor via `fstat` (needs
/// no spare file descriptor).
#[derive(Clone, Copy, Debug)]
pub(crate) struct FdMeta {
    pub(crate) id: StreamId,
    pub(crate) kind: FileKind,
    pub(crate) mode_type: libc::mode_t,
    pub(crate) len: u64,
}

impl FdMeta {
    #[inline]
    pub(crate) fn is_file(self) -> bool {
        self.kind == FileKind::Regular
    }
}

/// Whether `rdev` is Linux's cloning PTY master multiplexor `/dev/ptmx` (5, 2).
#[cfg(target_os = "linux")]
#[inline]
fn is_ptmx_rdev(rdev: u64) -> bool {
    rdev == libc::makedev(5, 2)
}

/// Whether `fd` is a non-cloning terminal whose pathname reopens the same
/// stream (excludes cloning PTY masters such as `/dev/ptmx` / `posix_openpt`).
fn is_reopenable_tty_fd(fd: RawFd, rdev: libc::dev_t) -> bool {
    // Sound: `isatty` queries an open fd without touching memory.
    if unsafe { libc::isatty(fd) } != 1 {
        return false;
    }
    #[cfg(target_os = "linux")]
    if is_ptmx_rdev(rdev as u64) {
        return false;
    }
    #[cfg(not(target_os = "linux"))]
    let _ = rdev;
    // A real terminal or PTY slave has a tty device name (`/dev/pts/N`,
    // `/dev/ttysNNN`, etc.), whereas a cloning PTY master has none.
    let mut name = [0 as libc::c_char; 256];
    // Sound: `name` is valid writable storage of length `name.len()`.
    unsafe { libc::ttyname_r(fd, name.as_mut_ptr(), name.len()) == 0 }
}

/// Refuse record-oriented sockets (non-`SOCK_STREAM` sockets such as
/// `SOCK_SEQPACKET` / `SOCK_DGRAM`) whose short reads silently discard record
/// tails instead of behaving like byte streams (#515). Fails closed if
/// `getsockopt(SO_TYPE)` fails or returns an unexpected length.
pub(crate) fn ensure_byte_stream_fd(fd: RawFd, mode_type: libc::mode_t) -> crate::err::Result<()> {
    if mode_type == libc::S_IFSOCK {
        let mut sock_type: libc::c_int = 0;
        let want_len = libc::socklen_t::try_from(core::mem::size_of::<libc::c_int>()).expect("c_int size fits in socklen_t");
        let mut len = want_len;
        // Sound: `sock_type` and `len` are valid writable stack variables.
        let rc = unsafe { libc::getsockopt(fd, libc::SOL_SOCKET, libc::SO_TYPE, (&mut sock_type as *mut libc::c_int).cast(), &mut len) };
        if rc < 0 {
            return Err(io::Error::last_os_error().into());
        }
        if len != want_len {
            crate::bail!("could not determine socket type for descriptor");
        }
        if sock_type != libc::SOCK_STREAM {
            crate::bail!("record-oriented socket input is not supported; use a byte-stream socket or pipe");
        }
    }
    Ok(())
}

/// Stat `fd` and verify it is safe to read as a byte stream, failing closed on
/// either `fstat` or `getsockopt(SO_TYPE)` errors.
pub(crate) fn validate_byte_stream_fd(fd: RawFd) -> crate::err::Result<FdMeta> {
    let meta = fstat_fd(fd)?;
    ensure_byte_stream_fd(fd, meta.mode_type)?;
    Ok(meta)
}

/// Best-effort non-blocking write of a short fatal diagnostic (`<= 512` bytes)
/// to fd 2 without locking `std::io::stderr()`, allocating, or mutating fd 2's
/// open-file flags. Async-signal-safe: uses only `fcntl`, `fstat`, `open`,
/// `write`, `send`, and `close`.
///
/// Checks that fd 2 is actually open for writing (`F_GETFL` access mode is not
/// `O_RDONLY`), then classifies fd 2 with `fstat(2)`:
/// - `S_IFREG`: writes directly to fd 2 (preserves the caller's file offset and
///   `O_APPEND` mode, and regular-file writes never wait on a pipe reader).
/// - `S_IFSOCK`: uses per-call `send(2, ..., MSG_DONTWAIT)` (atomic non-blocking
///   without mutating fd flags).
/// - `S_IFIFO` (pipes/FIFOs have no seek offset and no `TOSTOP`/`SIGTTOU`): on
///   Linux, reopens `/proc/self/fd/2` with `O_WRONLY | O_NONBLOCK | O_CLOEXEC`
///   for an independent non-blocking open file description that never blocks or
///   races peer writers; if unavailable (or on non-Linux), omits the diagnostic.
/// - Everything else (including `S_IFCHR` devices/terminals that may have seek
///   offsets or `TOSTOP`): omits the diagnostic so fatal termination
///   (`_exit`/`abort`) is never delayed or stopped by stderr.
pub(crate) fn write_fatal_nonblocking(msg: &[u8]) {
    if msg.is_empty() {
        return;
    }
    let msg = &msg[..msg.len().min(512)];
    // Sound: `fcntl(F_GETFL)` is async-signal-safe; refuse to write or reopen
    // a read-only fd 2 (e.g. read end of a pipe passed as stderr).
    let fl = unsafe { libc::fcntl(2, libc::F_GETFL) };
    if fl < 0 || (fl & libc::O_ACCMODE) == libc::O_RDONLY {
        return;
    }
    let mut st = core::mem::MaybeUninit::<libc::stat>::zeroed();
    // Sound: `st` is valid writable storage for `libc::stat`; `fstat` is async-signal-safe.
    if unsafe { libc::fstat(2, st.as_mut_ptr()) } < 0 {
        return;
    }
    // Sound: `fstat` returned 0 and initialized `st`.
    let mode_type = unsafe { st.assume_init() }.st_mode & libc::S_IFMT;
    match mode_type {
        libc::S_IFREG => {
            // Sound: `msg` is valid readable memory; writing directly to fd 2
            // preserves its offset and `O_APPEND`, and regular files do not
            // wait on a reader. `write` is async-signal-safe.
            let _ = unsafe { libc::write(2, msg.as_ptr().cast(), msg.len()) };
        }
        libc::S_IFSOCK => {
            #[cfg(target_os = "linux")]
            let flags = libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL;
            #[cfg(not(target_os = "linux"))]
            let flags = libc::MSG_DONTWAIT;
            // Sound: `msg` is valid readable memory; `send` with `MSG_DONTWAIT`
            // never blocks and is async-signal-safe.
            let _ = unsafe { libc::send(2, msg.as_ptr().cast(), msg.len(), flags) };
        }
        libc::S_IFIFO => {
            #[cfg(target_os = "linux")]
            {
                const PROC_STDERR: &[u8] = b"/proc/self/fd/2\0";
                // Sound: NUL-terminated static path on a verified-writable
                // pipe/FIFO (no seek offset, no `TOSTOP`/`SIGTTOU`); `open`,
                // `write`, `close` are async-signal-safe and `O_NONBLOCK` lives
                // on the new open file description without mutating fd 2 flags.
                let nb_fd = unsafe { libc::open(PROC_STDERR.as_ptr().cast(), libc::O_WRONLY | libc::O_NONBLOCK | libc::O_CLOEXEC) };
                if nb_fd >= 0 {
                    let _ = unsafe { libc::write(nb_fd, msg.as_ptr().cast(), msg.len()) };
                    let _ = unsafe { libc::close(nb_fd) };
                }
            }
        }
        _ => {}
    }
}

/// Format a short fatal diagnostic into a fixed stack buffer (no heap
/// allocation, no `stderr` mutex) and emit it via [`write_fatal_nonblocking`].
#[cfg(target_os = "linux")]
pub(crate) fn write_fatal_fmt(args: core::fmt::Arguments<'_>) {
    struct StackBuf {
        buf: [u8; 256],
        len: usize,
    }
    impl core::fmt::Write for StackBuf {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            let room = self.buf.len() - self.len;
            let take = s.len().min(room);
            self.buf[self.len..self.len + take].copy_from_slice(&s.as_bytes()[..take]);
            self.len += take;
            Ok(())
        }
    }
    let mut out = StackBuf { buf: [0u8; 256], len: 0 };
    let _ = core::fmt::Write::write_fmt(&mut out, args);
    write_fatal_nonblocking(&out.buf[..out.len]);
}

#[cfg(test)]
thread_local! {
    pub(crate) static FSTAT_CALLS: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
}

/// Query `(dev, ino, kind, len)` directly on `fd` using `fstat`, without
/// duplicating the descriptor.
pub(crate) fn fstat_fd(fd: RawFd) -> io::Result<FdMeta> {
    #[cfg(test)]
    FSTAT_CALLS.with(|c| c.set(c.get() + 1));
    let mut st = core::mem::MaybeUninit::<libc::stat>::zeroed();
    // Sound: `st` is valid writable storage for `libc::stat`.
    let rc = unsafe { libc::fstat(fd, st.as_mut_ptr()) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    // Sound: `fstat` returned 0 and initialized `st`.
    let st = unsafe { st.assume_init() };
    let mode_type = st.st_mode & libc::S_IFMT;
    let kind = match mode_type {
        libc::S_IFREG => FileKind::Regular,
        libc::S_IFIFO | libc::S_IFSOCK => FileKind::Stream,
        libc::S_IFCHR if is_reopenable_tty_fd(fd, st.st_rdev) => FileKind::Stream,
        _ => FileKind::Other,
    };
    Ok(FdMeta { id: StreamId { dev: st.st_dev as u64, ino: st.st_ino as u64 }, kind, mode_type, len: st.st_size.max(0) as u64 })
}

/// Snapshot of process standard input and working directory used to classify
/// operands and checkfile entries identically without repeated `getcwd` calls.
#[derive(Clone, Debug)]
pub(crate) struct StdinContext {
    pub(crate) stdin: Option<FdMeta>,
    cwd: Option<PathBuf>,
}

/// How an operand or checkfile entry relates to standard input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InputRole {
    /// Literal `-`.
    StdinDash,
    /// Another pathname under `/dev` or `/proc` (absolute or relative) whose
    /// OS `stat` confirms it names the same consumable stdin stream (pipe,
    /// FIFO, Unix socket, or terminal). Must read from fd 0 directly rather
    /// than reopening the pathname.
    StdinStreamAlias,
    /// macOS `/dev/stdin` or `/dev/fd/<N>` (for any descriptor `N`) when OS
    /// lookup confirms the path resolves to Darwin's `fd(4)` entry for a
    /// regular file: opened via the pathname (preserving OS lookup errors and
    /// permissions) and read from offset 0 via positioned `pread` or `mmap`
    /// without moving the underlying open-file description's shared cursor.
    #[cfg(target_os = "macos")]
    DarwinRegularDevFd,
    /// Ordinary pathname opened normally.
    Path,
}

impl StdinContext {
    pub(crate) fn current() -> Self {
        let stdin = fstat_fd(0).ok();
        let cwd = std::env::current_dir().ok();
        Self { stdin, cwd }
    }

    /// The `(dev, ino)` of stdin if and only if stdin is a consumable stream.
    pub(crate) fn stdin_stream_id(&self) -> Option<StreamId> {
        self.stdin.filter(|m| m.kind == FileKind::Stream).map(|m| m.id)
    }

    /// Fast lexical nomination: whether walking `path` from `cwd` either lands
    /// under `/dev` or `/proc` or crosses a `..` from inside a subdirectory of
    /// `/dev` or `/proc` (such as `/dev/fd/../../self/fd/0`, where `/dev/fd`
    /// symlinks to `/proc/self/fd`). Ordinary paths with a local directory
    /// named `dev` or `proc` (e.g. `dev/../fifo` under `/tmp/work`) are never
    /// nominated. This ONLY filters which operands get an OS `stat` check.
    fn might_be_dev_or_proc(&self, path: &Path) -> bool {
        let mut cur = if path.is_absolute() {
            PathBuf::from("/")
        } else {
            let Some(cwd) = self.cwd.as_deref() else {
                return false;
            };
            cwd.to_path_buf()
        };
        let is_under = |p: &Path| p.starts_with("/dev") || p.starts_with("/proc");
        let is_inside_subdir = |p: &Path| is_under(p) && p != Path::new("/dev") && p != Path::new("/proc");
        for comp in path.components() {
            match comp {
                Component::Prefix(_) => {}
                Component::RootDir => {
                    cur.clear();
                    cur.push("/");
                }
                Component::CurDir => {}
                Component::ParentDir => {
                    if is_inside_subdir(&cur) {
                        return true;
                    }
                    if cur.parent().is_some() {
                        cur.pop();
                    }
                }
                Component::Normal(part) => cur.push(part),
            }
        }
        is_under(&cur)
    }

    /// Classify `path` consistently across hashing operands, `--outboard`,
    /// XOF mode, and `--check` entries. Every alias decision is verified by
    /// the OS against the original operand (never by lexical text alone).
    pub(crate) fn classify(&self, path: &Path) -> InputRole {
        if crate::is_stdin(path) {
            return InputRole::StdinDash;
        }
        if self.might_be_dev_or_proc(path) {
            if let Some(stdin) = self.stdin
                && stdin.kind == FileKind::Stream
                && (path_matches_fd_meta(path, stdin) || {
                    #[cfg(target_os = "macos")]
                    {
                        darwin_dev_fd_number(path) == Some(0)
                    }
                    #[cfg(not(target_os = "macos"))]
                    {
                        false
                    }
                })
            {
                return InputRole::StdinStreamAlias;
            }
            #[cfg(target_os = "macos")]
            if is_verified_darwin_regular_dev_fd(path) {
                return InputRole::DarwinRegularDevFd;
            }
        }
        InputRole::Path
    }
}

/// Verify via OS resolution that `path` resolves to a Darwin `fd(4)` entry
/// (`/dev/fd/<N>` or a symlink in `/dev` targeting `fd/<N>`, for any open
/// descriptor `N`), preserving trailing-slash errors, directory-symlink `..`
/// semantics, and OS lookup errors, and return `N`.
#[cfg(target_os = "macos")]
fn darwin_dev_fd_number(path: &Path) -> Option<i32> {
    use std::os::unix::ffi::OsStrExt;
    fn parse_fd_number(name: &std::ffi::OsStr) -> Option<i32> {
        let bytes = name.as_bytes();
        if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
            return None;
        }
        std::str::from_utf8(bytes).ok()?.parse::<i32>().ok()
    }
    if path.as_os_str().as_bytes().ends_with(b"/") {
        return None;
    }
    // Verify the OS itself can stat `path` (preserving ENOTDIR/ENOENT for
    // invalid traversals like `/dev/fd/0/../../stdin`).
    let _ = std::fs::metadata(path).ok()?;
    // Resolve the parent directory via the OS kernel (`canonicalize`) so
    // symlinks and `..` follow kernel rules rather than lexical cancellation.
    let file_name = path.file_name()?;
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let real_parent = std::fs::canonicalize(parent).ok()?;
    if real_parent == Path::new("/dev/fd") {
        return parse_fd_number(file_name);
    }
    if real_parent == Path::new("/dev") {
        let entry = real_parent.join(file_name);
        if let Ok(target) = std::fs::read_link(&entry) {
            let rel = target.strip_prefix("/dev/fd").or_else(|_| target.strip_prefix("fd"));
            if let Ok(rest) = rel
                && let Some(fd_name) = rest.file_name()
                && rest.components().count() == 1
            {
                return parse_fd_number(fd_name);
            }
        }
    }
    None
}

/// Verify via OS resolution that `path` resolves to a Darwin `fd(4)` entry
/// referring to a regular file.
#[cfg(target_os = "macos")]
fn is_verified_darwin_regular_dev_fd(path: &Path) -> bool {
    darwin_dev_fd_number(path).is_some() && std::fs::metadata(path).is_ok_and(|meta| meta.is_file())
}

/// Shared OS `stat` helper for pathname-to-identity checks (L3): rejects
/// trailing slashes and Linux cloning PTY master devices (`/dev/ptmx`),
/// returning `(S_IFMT file-type bits, StreamId)` on match.
fn stat_path_identity(path: &Path) -> Option<(u32, StreamId)> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    if path.as_os_str().as_bytes().ends_with(b"/") {
        return None;
    }
    let meta = std::fs::metadata(path).ok()?;
    let mode_type = meta.mode() & u32::from(libc::S_IFMT);
    #[cfg(target_os = "linux")]
    if mode_type == u32::from(libc::S_IFCHR) && is_ptmx_rdev(meta.rdev()) {
        return None;
    }
    Some((mode_type, StreamId { dev: meta.dev(), ino: meta.ino() }))
}

/// Whether `path` currently resolves via OS `stat` to the exact same `(dev, ino)`
/// AND file type (`S_IFMT`) as `target` (preserving symlinks, `..`, and trailing
/// directory requirements), excluding cloning PTY master devices.
pub(crate) fn path_matches_fd_meta(path: &Path, target: FdMeta) -> bool {
    stat_path_identity(path).is_some_and(|(mode_type, id)| mode_type == u32::from(target.mode_type) && id == target.id)
}

/// Whether `path` currently resolves via OS `stat` to a stream matching `stream`
/// (excluding cloning PTY master devices).
pub(crate) fn path_matches_stream(path: &Path, stream: StreamId) -> bool {
    stat_path_identity(path).is_some_and(|(mode_type, id)| {
        (mode_type == u32::from(libc::S_IFIFO) || mode_type == u32::from(libc::S_IFSOCK) || mode_type == u32::from(libc::S_IFCHR)) && id == stream
    })
}

/// Open `path` and immediately check its `fstat` identity against an optional
/// forbidden consumable stream (e.g., the checkfile's own pipe/FIFO). Checks
/// `stat(path)` first so rejecting the checkfile's own FIFO does not block
/// waiting for another writer, then re-verifies `fstat(fd)` after open to
/// close pathname-swap TOCTOU races.
pub(crate) fn open_checked(path: &Path, forbidden: Option<StreamId>) -> crate::err::Result<(File, FdMeta)> {
    open_checked_with(path, forbidden, |p| File::open(p))
}

/// Inner implementation of [`open_checked`] parameterized on the open call so
/// tests can inject a post-open descriptor swap after the pre-open path check
/// passes, proving the post-open `fstat` guard fails closed on its own (M13).
pub(crate) fn open_checked_with(path: &Path, forbidden: Option<StreamId>, open: impl FnOnce(&Path) -> io::Result<File>) -> crate::err::Result<(File, FdMeta)> {
    if let Some(forbid) = forbidden
        && path_matches_stream(path, forbid)
    {
        crate::bail!("this checkfile is a stream, and cannot also be a file to check");
    }
    let file = open(path)?;
    let meta = fstat_fd(file.as_raw_fd())?;
    if let Some(forbid) = forbidden
        && meta.kind == FileKind::Stream
        && meta.id == forbid
    {
        crate::bail!("this checkfile is a stream, and cannot also be a file to check");
    }
    Ok((file, meta))
}

/// Outcome of hashing an open descriptor, carrying exact byte count and
/// whether a cursor-consuming read path ran.
#[derive(Clone, Copy, Debug)]
pub(crate) struct HashRead {
    pub(crate) hash: blake3::Hash,
    pub(crate) bytes_hashed: u64,
    pub(crate) cursor_moved: bool,
}

/// Result of a Linux page-cache probe followed by waiting positional reads.
#[cfg(target_os = "linux")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CacheRead {
    /// Waiting read returned 0: EOF reached with `usize` bytes total in buffer.
    Eof(usize),
    /// Optional non-blocking probe unavailable (`EAGAIN`/`EPERM`/`EOPNOTSUPP`/...)
    /// or buffer full before EOF: fall back to waiting reads on the open fd.
    NeedMore(usize),
    /// Descriptor does not support positional reads (`ESPIPE`); fall back to sequential `read`.
    NonSeekable(usize),
}

/// Run the single rule for cached positional reads:
/// 1. The first read asks the page cache alone (`RWF_NOWAIT`).
/// 2. A non-blocking 0-byte reply does NOT mean EOF (Linux 5.9/5.10 bug): it
///    switches to waiting reads (`flags == 0`).
/// 3. Any non-`EINTR`, non-`ESPIPE` failure of the optional `RWF_NOWAIT` probe
///    (including `EAGAIN`, `EOPNOTSUPP`, `ENOSYS`, or sandbox `EPERM`) returns
///    `NeedMore` so the caller performs the real waiting read on the open fd.
/// 4. On waiting reads (`flags == 0`), `ESPIPE` returns `NonSeekable`, while
///    **any real blocking error (`EIO`, etc.) is returned as `Err`, never
///    swallowed as a cache miss.**
/// 5. Only a waiting read (`flags == 0`) returning `0` returns `CacheRead::Eof`.
#[cfg(target_os = "linux")]
#[inline]
pub(crate) fn cached_then_plain(dst_cap: usize, mut at: usize, mut read: impl FnMut(usize, libc::c_int) -> isize) -> io::Result<CacheRead> {
    let mut flags = libc::RWF_NOWAIT;
    loop {
        if at == dst_cap {
            return Ok(CacheRead::NeedMore(at));
        }
        let rc = read(at, flags);
        if rc < 0 {
            let err = io::Error::last_os_error();
            let raw = err.raw_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            if raw == Some(libc::ESPIPE) {
                return Ok(CacheRead::NonSeekable(at));
            }
            if flags != 0 {
                return Ok(CacheRead::NeedMore(at));
            }
            return Err(err);
        }
        if rc == 0 && flags == 0 {
            return Ok(CacheRead::Eof(at));
        }
        at += rc as usize;
        flags = 0;
    }
}

#[derive(Clone, Copy)]
enum CapReadKind {
    Sequential,
    #[cfg(target_os = "linux")]
    Positioned,
}

/// Shared capped growth loop for raw `read`/`pread` syscalls without exposing
/// uninitialized memory to safe `Read` implementations (M11).
fn read_capped_with(fd: RawFd, kind: CapReadKind, buf: &mut Vec<u8>, cap: usize) -> io::Result<bool> {
    loop {
        if buf.len() >= cap {
            return Ok(false);
        }
        if buf.len() == buf.capacity() {
            let grow = buf.len().max(4096).min(cap - buf.len());
            buf.reserve(grow);
        }
        let avail = (buf.capacity() - buf.len()).min(cap - buf.len());
        let len = buf.len();
        let rc = match kind {
            CapReadKind::Sequential => {
                // Sound: `buf.as_mut_ptr().add(len)` points to `avail` valid writable
                // bytes inside `buf`'s allocation; `libc::read` writes at most `avail` bytes.
                unsafe { libc::read(fd, buf.as_mut_ptr().add(len).cast(), avail) }
            }
            #[cfg(target_os = "linux")]
            CapReadKind::Positioned => {
                let off = libc::off_t::try_from(len).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "file offset overflow"))?;
                // Sound: `buf.as_mut_ptr().add(len)` points to `avail` valid writable
                // bytes inside `buf`'s allocation; `libc::pread` writes at most `avail` bytes.
                unsafe { libc::pread(fd, buf.as_mut_ptr().add(len).cast(), avail, off) }
            }
        };
        if rc == 0 {
            return Ok(true);
        }
        if rc < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        let n = rc as usize;
        assert!(n <= avail, "kernel read did not exceed requested length");
        // Sound: the kernel `read`/`pread` syscall initialized `n` bytes starting at `len`.
        unsafe { buf.set_len(len + n) };
    }
}

/// Read into `buf` (preserving `buf[..buf.len()]` as the initialized prefix)
/// sequentially up to `cap`. Returns `Ok(true)` if EOF was reached within
/// `cap`, or `Ok(false)` when `buf.len() >= cap` and the caller must promote
/// to incremental streaming hashing.
pub(crate) fn read_sequential_capped(file: &mut File, buf: &mut Vec<u8>, cap: usize) -> io::Result<bool> {
    read_capped_with(file.as_raw_fd(), CapReadKind::Sequential, buf, cap)
}

/// Read the rest of `file` starting at `buf.len()` using `pread` up to `cap`,
/// falling back to sequential `read` on `ESPIPE` when `buf.is_empty()`.
/// Returns `Ok((reached_eof, used_sequential))`.
#[cfg(target_os = "linux")]
pub(crate) fn pread_rest_capped(file: &mut File, buf: &mut Vec<u8>, cap: usize) -> io::Result<(bool, bool)> {
    match read_capped_with(file.as_raw_fd(), CapReadKind::Positioned, buf, cap) {
        Ok(eof) => Ok((eof, false)),
        Err(e) if e.raw_os_error() == Some(libc::ESPIPE) && buf.is_empty() => Ok((read_sequential_capped(file, buf, cap)?, true)),
        Err(e) => Err(e),
    }
}

/// Query the current byte offset of a seekable descriptor, propagating OS
/// errors (`ESPIPE`, `EBADF`, etc.) instead of substituting zero (M2).
#[cfg(any(target_os = "macos", test))]
pub(crate) fn current_fd_offset(fd: BorrowedFd<'_>) -> io::Result<u64> {
    // Sound: `lseek(SEEK_CUR)` queries the open file description's current
    // offset without reading or writing caller memory.
    let rc = unsafe { libc::lseek(fd.as_raw_fd(), 0, libc::SEEK_CUR) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    u64::try_from(rc).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "negative file offset"))
}

/// Set the byte offset of a seekable descriptor to `offset`, rejecting
/// `off_t` overflow and propagating OS errors (M2).
#[cfg(any(target_os = "macos", test))]
pub(crate) fn set_fd_offset(fd: BorrowedFd<'_>, offset: u64) -> io::Result<()> {
    let off = libc::off_t::try_from(offset).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "file offset exceeds off_t"))?;
    // Sound: `lseek(SEEK_SET)` updates the open file description's offset
    // without touching caller memory.
    let rc = unsafe { libc::lseek(fd.as_raw_fd(), off, libc::SEEK_SET) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Read from a borrowed Unix descriptor at `*offset` via `pread`, advancing
/// `*offset` without modifying the open file description's shared cursor.
pub(crate) fn pread_at(fd: BorrowedFd<'_>, offset: &mut u64, buf: &mut [u8]) -> io::Result<usize> {
    if buf.is_empty() {
        return Ok(0);
    }
    let off = libc::off_t::try_from(*offset).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "pread offset exceeds off_t"))?;
    loop {
        // Sound: `fd` is borrowed live for this call, and `buf` is a live
        // exclusive mutable slice for the kernel to fill.
        let rc = unsafe { libc::pread(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), off) };
        if rc < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        let n = rc as usize;
        assert!(n <= buf.len(), "kernel pread did not exceed requested length");
        *offset = offset.checked_add(n as u64).ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "pread offset overflow"))?;
        return Ok(n);
    }
}

/// A lifetime-bound `Read` adapter that reads sequentially from a fixed
/// starting offset on a borrowed Unix descriptor using `pread`, never
/// modifying the underlying open file description's cursor (M6).
pub(crate) struct PreadReader<'a> {
    fd: BorrowedFd<'a>,
    offset: u64,
}

impl<'a> PreadReader<'a> {
    pub(crate) fn new(fd: BorrowedFd<'a>, offset: u64) -> Self {
        Self { fd, offset }
    }
}

impl Read for PreadReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        pread_at(self.fd, &mut self.offset, buf)
    }
}

/// Hash `prefix` followed by the remainder of `file` starting at `prefix.len()`
/// via positioned `pread` (or sequentially if `sequential` is true), keeping
/// memory bounded regardless of reported size.
pub(crate) fn finish_hash_streaming_from_prefix(mode: blake3::Mode<'_>, prefix: &[u8], file: &mut File, sequential: bool) -> io::Result<HashRead> {
    let mut hasher = blake3::convenience::Hasher::new(mode, None);
    crate::update(&mut hasher, prefix);
    if sequential {
        let extra = crate::update_from_counted(&mut hasher, file)?;
        Ok(HashRead { hash: hasher.finalize(), bytes_hashed: prefix.len() as u64 + extra, cursor_moved: true })
    } else {
        let mut reader = PreadReader::new(file.as_fd(), prefix.len() as u64);
        let extra = crate::update_from_counted(&mut hasher, &mut reader)?;
        Ok(HashRead { hash: hasher.finalize(), bytes_hashed: prefix.len() as u64 + extra, cursor_moved: false })
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn root_cwd_nominates_relative_dev_and_proc_candidates() {
        {
            let ctx = StdinContext { stdin: None, cwd: Some(PathBuf::from("/")) };
            assert!(ctx.might_be_dev_or_proc(Path::new("dev/stdin")));
            assert!(ctx.might_be_dev_or_proc(Path::new("proc/self/fd/0")));
            assert!(ctx.might_be_dev_or_proc(Path::new("/dev/fd/../../self/fd/0")));
            assert!(ctx.might_be_dev_or_proc(Path::new("/tmp/../dev/fd/../../self/fd/0")));
            assert!(!ctx.might_be_dev_or_proc(Path::new("tmp/file")));

            // Ordinary directory containing a subdirectory named `dev` or `proc`:
            // `dev/../fifo` must NEVER be nominated as a `/dev` or `/proc` alias (#515).
            let work = StdinContext { stdin: None, cwd: Some(PathBuf::from("/tmp/work")) };
            assert!(!work.might_be_dev_or_proc(Path::new("dev/../fifo")));
            assert!(!work.might_be_dev_or_proc(Path::new("proc/../fifo")));
            assert!(!work.might_be_dev_or_proc(Path::new("/tmp/work/dev/../fifo")));
            assert!(work.might_be_dev_or_proc(Path::new("../../dev/fd/0")));

            // Escaping `/dev` via `..` lands outside `/dev` and must not nominate.
            let in_dev = StdinContext { stdin: None, cwd: Some(PathBuf::from("/dev")) };
            assert!(!in_dev.might_be_dev_or_proc(Path::new("../tmp/work/fifo")));
            assert!(in_dev.might_be_dev_or_proc(Path::new("stdin")));

            // Traversing `..` from inside `/dev/fd` can cross the `/proc/self/fd` symlink.
            let in_dev_fd = StdinContext { stdin: None, cwd: Some(PathBuf::from("/dev/fd")) };
            assert!(in_dev_fd.might_be_dev_or_proc(Path::new("../../self/fd/0")));
        }
    }

    #[test]
    fn small_read_capped_promotes_above_cap_without_oom() {
        let dir = crate::tempfile::tempdir().unwrap();
        let path = dir.path().join("big-virtual");
        let data: Vec<u8> = (0..100_000usize).map(|i| (i * 17) as u8).collect();
        std::fs::write(&path, &data).unwrap();
        let mut file = File::open(&path).unwrap();
        let mut buf = Vec::with_capacity(64);
        let reached_eof = read_sequential_capped(&mut file, &mut buf, 4096).unwrap();
        assert!(!reached_eof, "must stop at cap and promote");
        assert_eq!(buf.len(), 4096);
        assert_eq!(&buf[..], &data[..4096]);
        let mut rest = Vec::new();
        file.read_to_end(&mut rest).unwrap();
        assert_eq!(&rest[..], &data[4096..]);
    }
}
