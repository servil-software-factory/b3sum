//! b3sum: BLAKE3's command-line hasher.
//!
//! On Linux, multi-file runs and uncached large files stream through
//! `io_uring` and `blake3::throughput::Pipeline` (`linux_pipe`):
//! - Warm small files (`<= 16 KiB`) read via `preadv2(RWF_NOWAIT)` into a
//!   core-local 16-slot `Block` arena and hash locally via `hash_each`.
//! - Cold small/medium files (`< 4 MiB`) stream asynchronously through
//!   `io_uring` into recycled buffers (promoting to streaming hashing if a
//!   small-hinted file grows past 4 MiB).
//! - In the multi-file warm mapped path (when `--no-mmap` is off, `mmap`
//!   succeeds, and sub-4 MiB files are not already claimed by an active cold
//!   `io_uring` ring), cached files (`>= 512 KiB`, first page resident via
//!   `mincore`) are mapped without copying into rooms: `512 KiB..4 MiB` hash
//!   directly via `hash_slice_32`, while `>= 4 MiB` transfer `Mmap` ownership
//!   through `WarmLargePipe` (`Pipeline::owned`); completed maps are held up
//!   to `HOLD_PAGES` until exit to avoid `munmap` TLB shootdowns.
//! - Cold large files (`>= 4 MiB`, not resident in page cache) stream
//!   directly from storage into 64 KiB `Pipeline::room()`s via batched
//!   `IORING_OP_READ`s, overlapping NVMe DMA with all-core subtree hashing.
//! - One cold file of 4 MiB or more, with more than one thread, is read
//!   past the page cache (`O_DIRECT`), 2 MiB a read on up to 16 threads
//!   (`direct`); the cache does not hold the file afterwards.
//!
//! Single small files read into a bounded `Vec` (promoting to incremental
//! streaming above 4 MiB) with zero `mmap` or ring setup, and standard
//! 32-byte digests format on the stack into a 32 KiB buffered writer without
//! cloning a 26 KiB `Hasher` per file. Unified file-end rules and descriptor
//! identity checks live in `file_end`.

mod args;
#[cfg(test)]
#[path = "test_bao_ref.rs"]
mod bao_ref;
mod err;
mod mmap;
#[cfg(test)]
#[path = "test_temp.rs"]
mod tempfile;
use args::{Command, KeyMode, ParseOutcome};
use std::cmp;
use std::fs::File;
use std::io;
use std::io::prelude::*;
use std::path::{Path, PathBuf};

#[cfg(test)]
mod unit_tests;

const NAME: &str = "b3sum";

#[cfg(target_os = "linux")]
mod direct;
mod file_end;
#[cfg(target_os = "linux")]
mod linux_pipe;
#[cfg(target_os = "linux")]
mod numa;
mod outboard;
#[cfg(target_os = "linux")]
mod uring;

use file_end::StreamId;
use file_end::{HashRead, InputRole, SMALL_MEM_CAP, StdinContext};

/// Best-effort diagnostic line on stderr: never panics on write errors such as
/// `ENOSPC` or `EBADF`, so normal status-1 failures keep their exit status
/// instead of aborting (a closed pipe on stderr still follows default `SIGPIPE`).
fn eprint_line(args: std::fmt::Arguments<'_>) {
    let mut stderr = io::stderr().lock();
    let _ = stderr.write_fmt(args);
    let _ = stderr.write_all(b"\n");
}

macro_rules! eprintln_safe {
    ($($arg:tt)*) => { eprint_line(format_args!($($arg)*)) };
}

struct Args {
    file_args: Vec<PathBuf>,
    command: Command,
    num_threads: Option<std::num::NonZeroUsize>,
    no_mmap: bool,
    mode: blake3::Mode<'static>,
    base_hasher: blake3::convenience::Hasher,
}

impl Args {
    fn parse() -> err::Result<Self> {
        let parsed = match args::parse_from(std::env::args_os()) {
            Ok(ParseOutcome::Parsed(parsed)) => parsed,
            Ok(ParseOutcome::Help) => {
                let mut stdout = io::stdout().lock();
                if let Err(e) = stdout.write_all(args::HELP_TEXT.as_bytes()).and_then(|()| stdout.flush()) {
                    let _ = writeln!(io::stderr().lock(), "{NAME}: {e}");
                    std::process::exit(1);
                }
                std::process::exit(0);
            }
            Ok(ParseOutcome::Version) => {
                let mut stdout = io::stdout().lock();
                if let Err(e) = writeln!(stdout, "b3sums {}", env!("B3SUM_BUILD_VERSION")).and_then(|()| stdout.flush()) {
                    let _ = writeln!(io::stderr().lock(), "{NAME}: {e}");
                    std::process::exit(1);
                }
                std::process::exit(0);
            }
            Err(msg) => {
                let mut stderr = io::stderr().lock();
                let _ = writeln!(stderr, "error: {msg}\n\nUsage: b3sum [OPTIONS] [FILE]...\n\nFor more information, try '--help'.");
                let _ = stderr.flush();
                std::process::exit(2);
            }
        };
        let file_args = parsed.files;
        if let Command::Hash { format, seek, length } = parsed.command {
            if format == args::OutputFormat::Raw && file_args.len() > 1 {
                bail!("Only one filename can be provided when using --raw");
            }
            // BLAKE3's output is 2^64 - 1 bytes long. A request past its end
            // was a panic for text, and for --raw fewer bytes than asked for,
            // with success.
            if seek.checked_add(length).is_none() {
                bail!("--seek {} and --length {} ask for output past its end, at 2^64 - 1 bytes", seek, length);
            }
        }
        // Crash-only: leak the single key or context string once for `'static` Mode.
        let mode: blake3::Mode<'static> = match parsed.key_mode {
            KeyMode::Keyed => {
                let key: &'static [u8; blake3::KEY_LEN] = Box::leak(Box::new(read_key_from_stdin()?));
                blake3::Mode::Keyed(key)
            }
            KeyMode::DeriveKey(context) => {
                let ctx: &'static str = Box::leak(context.into_boxed_str());
                blake3::Mode::DeriveKey(ctx)
            }
            KeyMode::Plain => blake3::Mode::Plain,
        };
        let base_hasher = blake3::convenience::Hasher::new(mode, None);
        Ok(Self { file_args, command: parsed.command, num_threads: parsed.num_threads, no_mmap: parsed.no_mmap, mode, base_hasher })
    }

    #[cfg(test)]
    pub(crate) fn for_test_plain(file_args: Vec<PathBuf>) -> Self {
        Self {
            file_args,
            command: Command::Hash { length: blake3::OUT_LEN as u64, seek: 0, format: args::OutputFormat::Text(args::TextFormat::Gnu) },
            num_threads: None,
            no_mmap: false,
            mode: blake3::Mode::Plain,
            base_hasher: blake3::convenience::Hasher::new(blake3::Mode::Plain, None),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test_plain_threads(file_args: Vec<PathBuf>, num_threads: usize) -> Self {
        let mut args = Self::for_test_plain(file_args);
        args.num_threads = std::num::NonZeroUsize::new(num_threads);
        args
    }

    #[inline(always)]
    fn is_standard_32(&self) -> bool {
        match self.command {
            Command::Hash { seek, length, .. } => seek == 0 && length <= blake3::OUT_LEN as u64,
            Command::Check { .. } | Command::Outboard { .. } => true,
        }
    }

    fn num_threads(&self) -> Option<std::num::NonZeroUsize> {
        self.num_threads
    }

    fn check(&self) -> bool {
        matches!(self.command, Command::Check { .. })
    }

    fn outboard(&self) -> Option<&Path> {
        match &self.command {
            Command::Outboard { dir, .. } => Some(dir.as_path()),
            _ => None,
        }
    }

    fn raw(&self) -> bool {
        matches!(self.command, Command::Hash { format: args::OutputFormat::Raw, .. })
    }

    fn text_format(&self) -> args::TextFormat {
        match self.command {
            Command::Hash { format: args::OutputFormat::Text(tf), .. } | Command::Outboard { format: tf, .. } => tf,
            Command::Hash { format: args::OutputFormat::Raw, .. } | Command::Check { .. } => args::TextFormat::Gnu,
        }
    }

    fn len(&self) -> u64 {
        match self.command {
            Command::Hash { length, .. } => length,
            Command::Check { .. } | Command::Outboard { .. } => blake3::OUT_LEN as u64,
        }
    }

    fn seek(&self) -> u64 {
        match self.command {
            Command::Hash { seek, .. } => seek,
            Command::Check { .. } | Command::Outboard { .. } => 0,
        }
    }

    /// The map of `file`, unless `--no-mmap` asked for none or the file
    /// cannot be mapped: the one place b3sum maps a file to hash it. A
    /// mapped file must not be modified in place while being hashed (see
    /// `README.md`; pass `--no-mmap` for live or untrusted files). A
    /// shortened file stops the process when its lost pages are read
    /// (`explain_sigbus`). (BLAKE3 #432: the option was accepted and did
    /// nothing.)
    pub(crate) fn map(&self, file: &impl std::os::fd::AsRawFd) -> Option<mmap::Mmap> {
        if self.no_mmap {
            return None;
        }
        #[cfg(test)]
        MAPPED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // Sound: the mapping is read-only and the CLI documents that files
        // must not be modified concurrently while hashed (pass `--no-mmap`
        // for mutable inputs; truncation triggers `explain_sigbus`).
        unsafe { mmap::Mmap::map(file) }.ok()
    }

    fn keyed(&self) -> bool {
        matches!(self.mode, blake3::Mode::Keyed(_))
    }

    fn quiet(&self) -> bool {
        matches!(self.command, Command::Check { quiet: true })
    }
}

/// Buffered writer over stdout: flushes each completed text line immediately
/// when stdout is a terminal, and buffers up to 32 KiB when redirected to a
/// pipe or file so multi-file runs make one `write` syscall per 32 KiB while
/// still hitting `SIGPIPE` on closed pipes (`test_reader_gone`). Tracks
/// partial writes and poisons itself on fatal errors so already-delivered
/// prefixes are never duplicated on later flushes.
pub(crate) struct OutBuf {
    buf: Vec<u8>,
    poisoned: bool,
    flush_each_line: bool,
}

const OUT_BUF_CAP: usize = 32 * 1024;

impl OutBuf {
    fn new() -> Self {
        use std::io::IsTerminal as _;
        Self { buf: Vec::with_capacity(OUT_BUF_CAP), poisoned: false, flush_each_line: io::stdout().is_terminal() }
    }

    #[inline]
    fn is_line_flushed(&self) -> bool {
        self.flush_each_line
    }

    #[inline]
    fn finish_line(&mut self) -> io::Result<()> {
        if self.flush_each_line {
            self.flush()?;
        }
        Ok(())
    }

    #[inline]
    fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    #[inline]
    fn write_bytes(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.poisoned {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "stdout previously failed"));
        }
        if self.buf.len() + bytes.len() > OUT_BUF_CAP {
            self.flush()?;
            if bytes.len() >= OUT_BUF_CAP {
                let (_, res) = write_all_stdout(bytes);
                if res.is_err() {
                    self.poisoned = true;
                }
                return res;
            }
        }
        self.buf.extend_from_slice(bytes);
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        if self.poisoned {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "stdout previously failed"));
        }
        if !self.buf.is_empty() {
            let (_, res) = write_all_stdout(&self.buf);
            self.buf.clear();
            if let Err(e) = res {
                self.poisoned = true;
                return Err(e);
            }
        }
        Ok(())
    }
}

/// Write `bytes` using `write_once`, advancing past short writes, retrying
/// `Interrupted`, and reporting `WriteZero` if `write_once` returns `Ok(0)`.
/// Returns `(bytes_written, result)` so callers never re-emit written prefixes.
fn write_all_with(mut bytes: &[u8], mut write_once: impl FnMut(&[u8]) -> io::Result<usize>) -> (usize, io::Result<()>) {
    let mut written = 0usize;
    while !bytes.is_empty() {
        match write_once(bytes) {
            Ok(0) => {
                return (written, Err(io::Error::new(io::ErrorKind::WriteZero, "failed to write whole buffer to stdout")));
            }
            Ok(n) => {
                let n = n.min(bytes.len());
                written += n;
                bytes = &bytes[n..];
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return (written, Err(e)),
        }
    }
    (written, Ok(()))
}

fn write_all_stdout(bytes: &[u8]) -> (usize, io::Result<()>) {
    write_all_with(bytes, |chunk| {
        // Sound: a write of `chunk`, a slice held here, to standard output.
        let rc = unsafe { libc::write(1, chunk.as_ptr().cast(), chunk.len()) };
        if rc < 0 { Err(io::Error::last_os_error()) } else { Ok(rc as usize) }
    })
}

#[inline]
fn encode_hex_stack(src: &[u8], dst: &mut [u8]) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for (i, &b) in src.iter().enumerate() {
        dst[2 * i] = HEX[(b >> 4) as usize];
        dst[2 * i + 1] = HEX[(b & 0x0f) as usize];
    }
}

#[inline]
pub(crate) fn hash_slice_32(mode: blake3::Mode<'_>, bytes: &[u8]) -> blake3::Hash {
    if use_pool_for(bytes.len()) {
        let mut hasher = blake3::convenience::Hasher::new(mode, Some(bytes.len() as u64));
        blake3::throughput::update_multithreaded(&mut hasher, bytes);
        hasher.finalize()
    } else {
        hash_slice_local(mode, bytes)
    }
}

/// Hashes on the calling thread, whether or not the pool is running.
pub(crate) fn hash_slice_local(mode: blake3::Mode<'_>, bytes: &[u8]) -> blake3::Hash {
    if matches!(mode, blake3::Mode::Plain) {
        blake3::convenience::hash(bytes)
    } else {
        let mut hasher = blake3::convenience::Hasher::new(mode, Some(bytes.len() as u64));
        hasher.update(bytes);
        hasher.finalize()
    }
}

/// Whether `path` is the operand that stands for standard input: `-`
/// itself. (Paths compare by their parts, so `-/` and `-/.` were taken
/// for it too, and a file of that name was never opened.)
pub(crate) fn is_stdin(path: &Path) -> bool {
    path.as_os_str() == "-"
}

/// Whether standard input is named more than once (either literally `-` or
/// by a stream alias under `/dev` or `/proc`, including relative spellings
/// resolved against the working directory). A stream is read once in operand
/// order rather than split across file worker threads.
pub(crate) fn stdin_named_twice(paths: &[PathBuf], stdin_ctx: &StdinContext) -> bool {
    let mut stream_refs = 0usize;
    #[cfg(target_os = "macos")]
    let mut dash_refs = 0usize;
    #[cfg(target_os = "macos")]
    let mut darwin_dev_fd_refs = 0usize;
    for path in paths {
        match stdin_ctx.classify(path) {
            InputRole::StdinDash => {
                #[cfg(target_os = "macos")]
                {
                    dash_refs += 1;
                }
                stream_refs += 1;
            }
            InputRole::StdinStreamAlias => {
                stream_refs += 1;
            }
            #[cfg(target_os = "macos")]
            InputRole::DarwinRegularDevFd => {
                darwin_dev_fd_refs += 1;
            }
            InputRole::Path => {}
        }
    }
    #[cfg(target_os = "macos")]
    if dash_refs > 0 && darwin_dev_fd_refs > 0 {
        return true;
    }
    stream_refs > 1
}

/// Hash a regular-file Darwin `/dev/fd/<N>` (or `/dev/stdin`) operand from
/// offset 0 via `mmap` or positioned `pread`, after opening `path` normally to
/// preserve OS lookup errors and permissions, without moving the underlying
/// open-file description's shared offset (#515).
#[cfg(target_os = "macos")]
fn hash_darwin_regular_dev_fd_hash(args: &Args, path: &Path) -> err::Result<blake3::Hash> {
    use std::os::fd::{AsFd, AsRawFd};
    let file = File::open(path)?;
    let meta = file_end::fstat_fd(file.as_raw_fd())?;
    file_end::ensure_byte_stream_fd(file.as_raw_fd(), meta.mode_type)?;
    if meta.len < MAP_LEN as u64 {
        let mut buf = Vec::with_capacity((usize::try_from(meta.len).expect("len < MAP_LEN") + 1).max(4096).min(SMALL_MEM_CAP + 1));
        let mut reader = file_end::PreadReader::new(file.as_fd(), 0);
        let mut chunk = [0u8; 64 * 1024];
        loop {
            let n = reader.read(&mut chunk)?;
            if n == 0 {
                return Ok(hash_slice_32(args.mode, &buf));
            }
            if buf.len() + n > SMALL_MEM_CAP {
                let mut hasher = args.base_hasher.clone();
                update(&mut hasher, &buf);
                update(&mut hasher, &chunk[..n]);
                update_from(&mut hasher, reader)?;
                return Ok(hasher.finalize());
            }
            buf.extend_from_slice(&chunk[..n]);
        }
    }
    if let Ok(len) = usize::try_from(meta.len)
        && len >= MAP_LEN
        && let Some(map) = args.map(&file)
    {
        if resident(&map) {
            let h = hash_slice_32(args.mode, &map);
            hold(map);
            return Ok(h);
        }
        drop(map);
    }
    let mut hasher = args.base_hasher.clone();
    update_from(&mut hasher, file_end::PreadReader::new(file.as_fd(), 0))?;
    Ok(hasher.finalize())
}

/// Fast standard-mode (`seek == 0 && len <= 32`) single-path hasher.
pub(crate) fn hash_path_hash(args: &Args, path: &Path, stdin_ctx: &StdinContext, forbidden: Option<StreamId>) -> err::Result<blake3::Hash> {
    match stdin_ctx.classify(path) {
        InputRole::StdinDash => {
            if args.keyed() {
                bail!("Cannot open `-` in keyed mode");
            }
            if let Some(forbid) = forbidden
                && stdin_ctx.stdin_stream_id() == Some(forbid)
            {
                bail!("standard input is the checkfile; it cannot also be a file to check");
            }
            // `b3sum < file`: a regular file at offset 0 is hashed as a named
            // file; if hashing did not move the descriptor cursor (mmap/pread/
            // direct/ring), advance fd 0 to the exact byte count hashed and
            // propagate any seek failure.
            if let Some((file, meta)) = stdin_file() {
                let hr = hash_open_file_with_meta(args, file, meta)?;
                if !hr.cursor_moved && hr.bytes_hashed > 0 {
                    // Sound: `lseek` on open descriptor 0 touches no memory.
                    let rc = unsafe { libc::lseek(0, hr.bytes_hashed as libc::off_t, libc::SEEK_SET) };
                    if rc < 0 {
                        return Err(io::Error::last_os_error().into());
                    }
                }
                return Ok(hr.hash);
            }
            file_end::validate_byte_stream_fd(0)?;
            let mut hasher = args.base_hasher.clone();
            update_from(&mut hasher, io::stdin())?;
            Ok(hasher.finalize())
        }
        InputRole::StdinStreamAlias => {
            if args.keyed() {
                bail!("Cannot open `-` in keyed mode");
            }
            if let Some(forbid) = forbidden
                && stdin_ctx.stdin_stream_id() == Some(forbid)
            {
                bail!("standard input is the checkfile; it cannot also be a file to check");
            }
            // Reuse fd 0 directly instead of reopening `/dev/stdin` (which
            // fails with ENXIO on inherited Unix sockets and can block
            // forever on a closed named FIFO).
            file_end::validate_byte_stream_fd(0)?;
            let mut hasher = args.base_hasher.clone();
            update_from(&mut hasher, io::stdin())?;
            Ok(hasher.finalize())
        }
        #[cfg(target_os = "macos")]
        InputRole::DarwinRegularDevFd => hash_darwin_regular_dev_fd_hash(args, path),
        InputRole::Path => {
            let (file, meta) = file_end::open_checked(path, forbidden)?;
            Ok(hash_open_file_with_meta(args, file, meta)?.hash)
        }
    }
}

/// Standard input as a file, where it is a regular file read from its
/// start (`b3sum < file`): the same file, its position shared (the
/// caller moves it past the consumed bytes once hashed). Uses `fstat(0)`
/// and `lseek(0, 0, SEEK_CUR)` first so no spare descriptor is touched
/// unless needed, returning the already-queried [`file_end::FdMeta`] (M5).
fn stdin_file() -> Option<(File, file_end::FdMeta)> {
    use std::os::fd::AsFd;
    let meta = file_end::fstat_fd(0).ok()?;
    if !meta.is_file() {
        return None;
    }
    // Sound: querying current offset of open descriptor 0 touches no memory.
    let pos = unsafe { libc::lseek(0, 0, libc::SEEK_CUR) };
    if pos != 0 {
        return None;
    }
    Some((File::from(io::stdin().as_fd().try_clone_to_owned().ok()?), meta))
}

/// Stream an already-opened `file` sequentially to actual EOF without creating
/// a memory mapping (used after an `O_DIRECT` read observes that the file's
/// length changed, so a stale mapping is never hashed), using the already-
/// queried `fd_meta` to avoid a redundant `fstat` (M5).
#[cfg(target_os = "linux")]
pub(crate) fn hash_open_file_streaming_with_meta(args: &Args, file: File, fd_meta: file_end::FdMeta) -> err::Result<blake3::Hash> {
    use std::os::fd::AsRawFd;
    file_end::ensure_byte_stream_fd(file.as_raw_fd(), fd_meta.mode_type)?;
    let mut hasher = args.base_hasher.clone();
    update_from(&mut hasher, file)?;
    Ok(hasher.finalize())
}

/// Test convenience wrapper for hashing an open file without pre-queried metadata.
#[cfg(test)]
pub(crate) fn hash_open_file(args: &Args, file: File) -> err::Result<blake3::Hash> {
    Ok(hash_open_file_ex(args, file)?.hash)
}

/// Hash an already-opened `file`, querying `fstat` once and forwarding to
/// [`hash_open_file_with_meta`].
#[cfg(test)]
pub(crate) fn hash_open_file_ex(args: &Args, file: File) -> err::Result<HashRead> {
    use std::os::fd::AsRawFd;
    let meta = file_end::fstat_fd(file.as_raw_fd())?;
    hash_open_file_with_meta(args, file, meta)
}

/// Hash an already-opened `file` using its already-queried `fd_meta` (M5),
/// returning the digest along with exact byte count and whether the file
/// descriptor's cursor was advanced.
pub(crate) fn hash_open_file_with_meta(args: &Args, mut file: File, fd_meta: file_end::FdMeta) -> err::Result<HashRead> {
    {
        use std::os::fd::AsRawFd;
        file_end::ensure_byte_stream_fd(file.as_raw_fd(), fd_meta.mode_type)?;
    }
    if fd_meta.is_file() {
        let size = fd_meta.len;
        if size < MAP_LEN as u64 {
            let cap = (usize::try_from(size).expect("size < MAP_LEN") + 1).max(4096).min(SMALL_MEM_CAP + 1);
            let mut buf = Vec::with_capacity(cap);
            let reached_eof = file_end::read_sequential_capped(&mut file, &mut buf, SMALL_MEM_CAP)?;
            if reached_eof {
                return Ok(HashRead { hash: hash_slice_32(args.mode, &buf), bytes_hashed: buf.len() as u64, cursor_moved: true });
            }
            return Ok(file_end::finish_hash_streaming_from_prefix(args.mode, &buf, &mut file, true)?);
        }

        if let Ok(len) = usize::try_from(size)
            && len >= MAP_LEN
            && let Some(map) = args.map(&file)
        {
            let warm = resident(&map);
            // The tests' seam: a file just written is in the cache.
            #[cfg(all(test, target_os = "linux"))]
            let warm = warm && !linux_pipe::force_miss();
            if warm {
                // Release the open file descriptor once the mapping is known
                // warm so tight FD limits (e.g. RLIMIT_NOFILE=4) still have a
                // spare FD if `hash_slice_32` triggers lazy NUMA sysfs reads.
                drop(file);
                let h = hash_slice_32(args.mode, &map);
                let map_len = map.len() as u64;
                hold(map);
                return Ok(HashRead { hash: h, bytes_hashed: map_len, cursor_moved: false });
            }
            #[cfg(target_os = "linux")]
            if threads().get() == 1 {
                drop(map);
                if size < POOL_START_LEN as u64 {
                    let mut buf = Vec::with_capacity((len + 1).min(SMALL_MEM_CAP + 1));
                    let reached_eof = file_end::read_sequential_capped(&mut file, &mut buf, SMALL_MEM_CAP)?;
                    if reached_eof {
                        return Ok(HashRead { hash: hash_slice_32(args.mode, &buf), bytes_hashed: buf.len() as u64, cursor_moved: true });
                    }
                    return Ok(file_end::finish_hash_streaming_from_prefix(args.mode, &buf, &mut file, true)?);
                }
                if let Some(res) = linux_pipe::hash_single_cold_large_file_1t(args, &file, size) {
                    let (hash, bytes_hashed) = res?;
                    return Ok(HashRead { hash, bytes_hashed, cursor_moved: false });
                }
                // No ring to read it through (a sandbox that refuses
                // io_uring, a kernel without IORING_OP_READ): stream below.
            } else {
                // A cold file of 4 MiB or more: read past the page cache,
                // several reads at once, each thread hashing its piece
                // (direct.rs). Under `--num-threads`, concurrent file workers
                // lease extra direct threads from one shared spare permit pool
                // so total active threads stay within `--num-threads`.
                let want_total = if args.num_threads().is_some() { pool_threads().get() } else { threads().get() }.min(DIRECT_THREADS);
                let permits = DirectPermits::acquire(want_total.saturating_sub(1));
                let direct_threads = 1 + permits.extra;
                if direct::is_for(&file, size) {
                    match direct::hash(args.mode, &file, len, direct_threads, &mut direct::Piece::new()) {
                        Ok(h) => {
                            drop(permits);
                            drop(map);
                            return Ok(HashRead { hash: h, bytes_hashed: len as u64, cursor_moved: false });
                        }
                        Err(direct::Failure::Unavailable) => {
                            // O_DIRECT could not be opened or was refused (`EINVAL`) on this
                            // fd/fs: fall through to the existing mapped multi-core path below.
                        }
                        Err(direct::Failure::Changed) => {
                            // The direct reader observed the file grew or shrank: discard
                            // the stale mapping and stream the open descriptor to actual EOF.
                            drop(permits);
                            drop(map);
                            let mut hasher = args.base_hasher.clone();
                            let bytes_hashed = update_from_counted(&mut hasher, file)?;
                            return Ok(HashRead { hash: hasher.finalize(), bytes_hashed, cursor_moved: true });
                        }
                        Err(direct::Failure::Err(e)) => {
                            drop(permits);
                            drop(map);
                            return Err(e.into());
                        }
                    }
                }
                drop(permits);
                // Multi-threaded single cold file (`>= 512 KiB`): start the pool
                // only if the budget allows `> 1` threads so a temporary width of 1
                // does not freeze the global singleton at 1 (M27).
                let _ = try_init_parallel_pool();
                let h = hash_slice_32(args.mode, &map);
                let map_len = map.len() as u64;
                hold(map);
                return Ok(HashRead { hash: h, bytes_hashed: map_len, cursor_moved: false });
            }
            #[cfg(not(target_os = "linux"))]
            drop(map);
        }
    }

    let mut hasher = args.base_hasher.clone();
    let bytes_hashed = update_from_counted(&mut hasher, file)?;
    Ok(HashRead { hash: hasher.finalize(), bytes_hashed, cursor_moved: true })
}

fn hash_path(args: &Args, path: &Path, stdin_ctx: &StdinContext) -> err::Result<blake3::OutputReader> {
    let mut hasher = args.base_hasher.clone();
    match stdin_ctx.classify(path) {
        InputRole::StdinDash | InputRole::StdinStreamAlias => {
            if args.keyed() {
                bail!("Cannot open `-` in keyed mode");
            }
            file_end::validate_byte_stream_fd(0)?;
            update_from(&mut hasher, io::stdin())?;
        }
        #[cfg(target_os = "macos")]
        InputRole::DarwinRegularDevFd => {
            use std::os::fd::{AsFd, AsRawFd};
            let file = File::open(path)?;
            let meta = file_end::validate_byte_stream_fd(file.as_raw_fd())?;
            if meta.len >= MAP_LEN as u64
                && let Some(map) = args.map(&file)
                && resident(&map)
            {
                update(&mut hasher, &map);
                hold(map);
            } else {
                update_from(&mut hasher, file_end::PreadReader::new(file.as_fd(), 0))?;
            }
        }
        InputRole::Path => {
            let mut file = File::open(path)?;
            let fd_meta = {
                use std::os::fd::AsRawFd;
                file_end::validate_byte_stream_fd(file.as_raw_fd())?
            };
            if fd_meta.is_file() && fd_meta.len < 64 * 1024 {
                let mut stack_buf = [0u8; 64 * 1024];
                let (n, eof) = fill(&mut file, &mut stack_buf)?;
                update(&mut hasher, &stack_buf[..n]);
                if !eof {
                    update_from(&mut hasher, file)?;
                }
            } else if let Some(map) = mapped(args, &file) {
                update(&mut hasher, &map);
                hold(map);
            } else {
                update_from(&mut hasher, file)?;
            }
        }
    }
    let mut output_reader = hasher.finalize_xof();
    output_reader.set_position(args.seek());
    Ok(output_reader)
}

const HOLD_PAGES: u64 = 1 << 18;
static HELD_PAGES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Cached OS virtual memory page size, validated positive once (L5).
pub(crate) fn page_size() -> usize {
    static PAGE_SIZE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *PAGE_SIZE.get_or_init(|| {
        // Sound: `sysconf(_SC_PAGESIZE)` queries an immutable OS constant.
        let ps = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        usize::try_from(ps).ok().filter(|&n| n > 0).expect("OS page size must be positive")
    })
}

pub(crate) fn hold(map: mmap::Mmap) {
    use std::sync::atomic::Ordering;
    let page = page_size() as u64;
    let pages = (map.len() as u64).div_ceil(page);
    if HELD_PAGES.fetch_add(pages, Ordering::Relaxed) + pages <= HOLD_PAGES {
        std::mem::forget(map);
    } else {
        HELD_PAGES.fetch_sub(pages, Ordering::Relaxed);
    }
}

/// Reserve all but `remaining_pages` of `HOLD_PAGES` in tests without mapping
/// a multi-GiB sparse filler (`RLIMIT_AS`-safe on 4 KiB and 64 KiB kernels; #568).
#[cfg(test)]
pub(crate) fn set_hold_budget_remaining_for_test(remaining_pages: u64) {
    assert!(remaining_pages <= HOLD_PAGES);
    HELD_PAGES.store(HOLD_PAGES - remaining_pages, std::sync::atomic::Ordering::Relaxed);
}

/// The files this process has mapped (`Args::map`). Atomic so multi-threaded
/// `linux_pipe::hash_paths` counts maps across all worker threads (#504).
#[cfg(test)]
pub(crate) static MAPPED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn mapped(args: &Args, file: &File) -> Option<mmap::Mmap> {
    let len = usize::try_from(file.metadata().ok()?.len()).ok()?;
    if len < MAP_LEN {
        return None;
    }
    let map = args.map(file)?;
    if cfg!(target_os = "linux") {
        return Some(map);
    }
    resident(&map).then_some(map)
}

pub(crate) fn resident(map: &mmap::Mmap) -> bool {
    let mut vec = 0u8;
    // Sound: one page of the map (a map is at least one byte here, the
    // callers') and one byte for the answer.
    let rc = unsafe { libc::mincore(map.as_ptr() as *mut libc::c_void, 1, (&mut vec as *mut u8).cast()) };
    rc == 0 && vec & 1 == 1
}

pub(crate) const MAP_LEN: usize = POOL_LEN;

const POOL_LEN: usize = 512 << 10;
/// Starting the thread pool in a one-shot CLI process costs ~15-25 us per worker
/// thread (~0.3-2 ms), which exceeds the whole hash time below ~4 MiB; once
/// started, inputs from `POOL_LEN` up use it.
const POOL_START_LEN: usize = 4 << 20;

/// The threads that read one cold file directly, each a read at a time:
/// a drive's answer grows little past this many reads at once (a Ryzen
/// 9700X's: 5.7 GB/s at 2, 8.8 at 4, 10.3 at 8, 10.7 at 16).
#[cfg(target_os = "linux")]
pub(crate) const DIRECT_THREADS: usize = 16;

/// Hard ceiling on effective `--num-threads` so absurd values (e.g. `usize::MAX`)
/// are bounded cleanly as an upper limit instead of aborting in pool allocation.
const MAX_THREADS: usize = 1024;

static POOL_ONCE: std::sync::Once = std::sync::Once::new();
/// Actual width of the initialized global pool (0 before initialization),
/// distinct from prospective [`pool_threads()`] budget (M27).
static ACTUAL_POOL_THREADS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

struct ThreadBudget {
    file_threads: usize,
    pool_threads: usize,
    leased_direct: usize,
    strict: bool,
}

static THREAD_BUDGET: std::sync::Mutex<ThreadBudget> =
    std::sync::Mutex::new(ThreadBudget { file_threads: 1, pool_threads: 0, leased_direct: 0, strict: false });

fn lock_budget() -> std::sync::MutexGuard<'static, ThreadBudget> {
    THREAD_BUDGET.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(any(target_os = "linux", test))]
pub(crate) fn init_pool_once(n: usize) {
    let _ = init_pool_inner(n, true);
}

/// Start the global thread pool only when the budget currently allows more than
/// one thread, reserving the width atomically under `THREAD_BUDGET` so a
/// temporary width of 1 (during a strict file batch or direct-I/O lease) leaves
/// the singleton uninitialized for later parallel work (M27).
fn try_init_parallel_pool() -> bool {
    init_pool_inner(MAX_THREADS, false) > 1
}

fn init_pool_inner(n: usize, allow_single: bool) -> usize {
    let actual = actual_pool_threads();
    if actual != 0 {
        return actual;
    }
    {
        let mut b = lock_budget();
        if b.pool_threads == 0 {
            let leased = if b.strict { b.leased_direct } else { 0 };
            let limit =
                if b.strict || !allow_single { threads().get().saturating_sub(b.file_threads.saturating_add(leased)).saturating_add(1) } else { MAX_THREADS };
            let chosen = n.min(limit).clamp(1, MAX_THREADS);
            if !allow_single && chosen <= 1 {
                return 0;
            }
            b.pool_threads = chosen;
        }
    }
    POOL_ONCE.call_once(|| {
        let chosen = lock_budget().pool_threads;
        blake3::initialize_multithreaded(std::num::NonZeroUsize::new(chosen));
        ACTUAL_POOL_THREADS.store(chosen, std::sync::atomic::Ordering::Release);
    });
    actual_pool_threads()
}

/// Actual initialized width of the global thread pool (0 if not yet started; M27).
pub(crate) fn actual_pool_threads() -> usize {
    ACTUAL_POOL_THREADS.load(std::sync::atomic::Ordering::Acquire)
}

/// Cap on file-worker threads for a batch, respecting any background worker
/// threads already started in the global pool so total simultaneous hashing
/// threads stay within `--num-threads` across multiple batches/checkfiles.
pub(crate) fn max_file_threads(total: usize) -> usize {
    let bg_workers = lock_budget().pool_threads.saturating_sub(1);
    total.saturating_sub(bg_workers).max(1)
}

#[inline]
fn use_pool_for(len: usize) -> bool {
    let actual = actual_pool_threads();
    if actual != 0 {
        len >= POOL_LEN && actual > 1
    } else if len >= POOL_START_LEN {
        try_init_parallel_pool()
    } else {
        false
    }
}

fn cap_requested_threads(requested: std::num::NonZeroUsize) -> std::num::NonZeroUsize {
    let avail = std::thread::available_parallelism().map_or(64, |n| n.get());
    let cap = avail.saturating_mul(4).clamp(64, MAX_THREADS);
    std::num::NonZeroUsize::new(requested.get().min(cap).max(1)).expect("capped thread count is nonzero")
}

#[cfg(test)]
thread_local! {
    /// For the tests: the inputs this thread read in pieces ([`update_from`]).
    pub(crate) static STREAMED: core::cell::Cell<u32> = const { core::cell::Cell::new(0) };
}

static THREADS: std::sync::OnceLock<std::num::NonZeroUsize> = std::sync::OnceLock::new();

#[cfg(all(test, target_os = "linux"))]
pub(crate) static CONFINEMENT_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[cfg(target_os = "linux")]
fn confine_cli_once() {
    static CONFINED: std::sync::Once = std::sync::Once::new();
    CONFINED.call_once(|| {
        #[cfg(test)]
        CONFINEMENT_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        numa::confine_cli_to_one_node();
    });
}

/// Apply `--num-threads` before hashing begins. Runs with `--num-threads > 1`
/// confine to one Linux memory node eagerly before `available_parallelism()`
/// caps the request; `--num-threads 1` and default runs defer confinement to
/// the first `threads()` demand (so single-threaded paths that never ask for
/// workers skip `/sys/devices/system` NUMA discovery: BLAKE3 #469).
pub(crate) fn configure_threads(args: &Args) {
    if let Some(requested) = args.num_threads() {
        #[cfg(target_os = "linux")]
        if requested.get() > 1 {
            confine_cli_once();
        }
        THREADS.set(cap_requested_threads(requested)).expect("main sets the threads first");
        lock_budget().strict = true;
    }
}

pub(crate) fn threads() -> std::num::NonZeroUsize {
    *THREADS.get_or_init(|| {
        // Confine lazily on the first default `threads()` demand so single-
        // threaded paths that never query worker count skip per-process
        // `/sys/devices/system` NUMA discovery on multi-socket Linux hosts (#469).
        #[cfg(target_os = "linux")]
        confine_cli_once();
        std::thread::available_parallelism().unwrap_or(std::num::NonZeroUsize::MIN)
    })
}

/// Scoped accounting for active file-worker threads under `--num-threads`.
/// Shares one mutex-serialized hashing thread budget across file workers,
/// global pool background workers, and `direct::hash` helper threads (#515).
pub(crate) struct FileThreadsGuard {
    prev_file: usize,
    prev_strict: bool,
}

impl FileThreadsGuard {
    pub(crate) fn enter(file_threads: usize) -> Self {
        let capped = file_threads.clamp(1, threads().get());
        let mut b = lock_budget();
        let prev_file = core::mem::replace(&mut b.file_threads, capped);
        let prev_strict = core::mem::replace(&mut b.strict, true);
        Self { prev_file, prev_strict }
    }
}

impl Drop for FileThreadsGuard {
    fn drop(&mut self) {
        let mut b = lock_budget();
        b.strict = self.prev_strict;
        b.file_threads = self.prev_file;
    }
}

/// Shared lease of extra helper threads for `direct::hash` under strict
/// `--num-threads` budgeting, serialized atomically with pool initialization.
#[cfg(any(target_os = "linux", test))]
pub(crate) struct DirectPermits {
    pub(crate) extra: usize,
    leased: bool,
}

#[cfg(any(target_os = "linux", test))]
impl DirectPermits {
    pub(crate) fn acquire(want_extra: usize) -> Self {
        if want_extra == 0 {
            return Self { extra: 0, leased: false };
        }
        let mut b = lock_budget();
        if !b.strict {
            return Self { extra: want_extra, leased: false };
        }
        let used = b.file_threads.saturating_add(b.pool_threads.saturating_sub(1)).saturating_add(b.leased_direct);
        let take = threads().get().saturating_sub(used).min(want_extra);
        if take == 0 {
            return Self { extra: 0, leased: false };
        }
        b.leased_direct += take;
        Self { extra: take, leased: true }
    }
}

#[cfg(any(target_os = "linux", test))]
impl Drop for DirectPermits {
    fn drop(&mut self) {
        if self.leased && self.extra > 0 {
            let mut b = lock_budget();
            b.leased_direct = b.leased_direct.saturating_sub(self.extra);
        }
    }
}

#[cfg(any(target_os = "linux", test))]
pub(crate) fn pool_threads() -> std::num::NonZeroUsize {
    let b = lock_budget();
    let leased_direct = if b.strict { b.leased_direct } else { 0 };
    std::num::NonZeroUsize::new(threads().get().saturating_sub(b.file_threads.saturating_add(leased_direct)).saturating_add(1)).expect("at least one thread")
}

pub(crate) fn update(hasher: &mut blake3::convenience::Hasher, input: &[u8]) {
    if use_pool_for(input.len()) {
        blake3::throughput::update_multithreaded(hasher, input);
    } else {
        hasher.update(input);
    }
}

const STREAM_PIECE: usize = 4 << 20;

const _: () = {
    assert!(STREAM_PIECE == file_end::SMALL_MEM_CAP);
    assert!(POOL_START_LEN == STREAM_PIECE);
    #[cfg(target_os = "linux")]
    assert!(direct::DIRECT_MIN >= STREAM_PIECE as u64);
};

struct StreamBuffers {
    first: Vec<u8>,
    /// Allocated lazily only on the multi-threaded overlap branch, so
    /// single-threaded runs and streams that finish within the first 4 MiB
    /// piece never allocate a second 4 MiB buffer.
    second: Option<Vec<u8>>,
}

thread_local! {
    static BUFFERS: std::cell::RefCell<StreamBuffers> = std::cell::RefCell::new(StreamBuffers { first: vec![0; STREAM_PIECE], second: None });
}

/// Minimum read request length so Linux `pipe2(O_DIRECT)` packet-mode pipes
/// (which buffer up to one kernel `PAGE_SIZE` per packet in `fs/pipe.c` and
/// discard any unread tail on a short `read`) never silently truncate a packet
/// on 4 KiB, 16 KiB, or 64 KiB page kernels (#515, L5).
pub(crate) fn min_packet_read_len() -> usize {
    #[cfg(target_os = "linux")]
    {
        page_size().max(4096)
    }
    #[cfg(not(target_os = "linux"))]
    {
        4096
    }
}

/// Fill `buffer` from `reader`, returning `(bytes_read, reached_eof)`.
/// Keeps every read request `>= min_packet_read_len()` (at least one kernel
/// page on Linux) when filling a piece buffer so `pipe2(O_DIRECT)` packet-mode
/// pipes never discard a packet tail on a short trailing read (#515);
/// `reached_eof` is true only when `reader.read` actually returns `Ok(0)`.
fn fill(reader: &mut impl Read, buffer: &mut [u8]) -> io::Result<(usize, bool)> {
    let min_read = min_packet_read_len();
    let mut filled = 0;
    while filled < buffer.len() {
        if filled > 0 && buffer.len() >= min_read && buffer.len() - filled < min_read {
            return Ok((filled, false));
        }
        match reader.read(&mut buffer[filled..]) {
            Ok(0) => return Ok((filled, true)),
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok((filled, false))
}

fn update_from(hasher: &mut blake3::convenience::Hasher, reader: impl Read + Send) -> io::Result<()> {
    update_from_counted(hasher, reader).map(|_| ())
}

pub(crate) fn update_from_counted(hasher: &mut blake3::convenience::Hasher, mut reader: impl Read + Send) -> io::Result<u64> {
    #[cfg(test)]
    STREAMED.with(|streamed| streamed.set(streamed.get() + 1));
    BUFFERS.with_borrow_mut(|bufs| {
        let first = &mut bufs.first;
        let (mut len, mut eof) = fill(&mut reader, first)?;
        let mut total = len as u64;
        if eof {
            update(hasher, &first[..len]);
            return Ok(total);
        }
        if threads().get() == 1 {
            loop {
                update(hasher, &first[..len]);
                if eof {
                    return Ok(total);
                }
                let next = fill(&mut reader, first)?;
                len = next.0;
                eof = next.1;
                total += len as u64;
            }
        }
        let second = bufs.second.get_or_insert_with(|| vec![0; STREAM_PIECE]);
        std::thread::scope(|scope| {
            let (free_tx, free_rx) = std::sync::mpsc::sync_channel::<&mut [u8]>(1);
            let (full_tx, full_rx) = std::sync::mpsc::sync_channel::<(&mut [u8], io::Result<(usize, bool)>)>(1);
            let reader = &mut reader;
            scope.spawn(move || {
                for buffer in free_rx {
                    let read = fill(reader, buffer);
                    let last = !matches!(read, Ok((_, false)));
                    if full_tx.send((buffer, read)).is_err() || last {
                        break;
                    }
                }
            });
            free_tx.send(&mut second[..]).expect("the reader waits for a buffer");
            let mut current: &mut [u8] = &mut first[..];
            let mut cur_len = len;
            loop {
                update(hasher, &current[..cur_len]);
                let (next, read) = full_rx.recv().expect("the reader sends every piece");
                let (next_len, next_eof) = read?;
                total += next_len as u64;
                if next_eof {
                    update(hasher, &next[..next_len]);
                    return Ok(total);
                }
                free_tx.send(current).expect("the reader waits for a buffer");
                current = next;
                cur_len = next_len;
            }
        })
    })
}

fn write_hash_text(hash: &blake3::Hash, args: &Args, out: &mut OutBuf) -> err::Result<()> {
    let take = usize::try_from(args.len()).expect("standard 32-byte mode len <= 32");
    let mut hex_buf = [0u8; 2 * blake3::OUT_LEN];
    encode_hex_stack(&hash.as_bytes()[..take], &mut hex_buf[..2 * take]);
    out.write_bytes(&hex_buf[..2 * take])?;
    Ok(())
}

/// Shared GNU/BSD checkfile line formatter accepting any digest-writing
/// callback (`write_hash_text` or `write_hex_output`), preserving the zero-
/// allocation path for plain UTF-8 names (L2).
fn write_formatted_line(path: &Path, args: &Args, out: &mut OutBuf, write_digest: impl FnOnce(&mut OutBuf) -> err::Result<()>) -> err::Result<()> {
    match args.text_format() {
        args::TextFormat::DigestOnly => {
            write_digest(out)?;
            out.write_bytes(b"\n")?;
            return out.finish_line().map_err(Into::into);
        }
        args::TextFormat::Bsd => {
            let FilepathString { filepath_string, is_escaped } = filepath_to_string(path);
            if is_escaped {
                out.write_bytes(b"\\")?;
            }
            out.write_bytes(b"BLAKE3 (")?;
            out.write_bytes(filepath_string.as_bytes())?;
            out.write_bytes(b") = ")?;
            write_digest(out)?;
        }
        args::TextFormat::Gnu => {
            let FilepathString { filepath_string, is_escaped } = filepath_to_string(path);
            if is_escaped {
                out.write_bytes(b"\\")?;
            }
            write_digest(out)?;
            out.write_bytes(b"  ")?;
            out.write_bytes(filepath_string.as_bytes())?;
        }
    }
    out.write_bytes(b"\n")?;
    out.finish_line()?;
    Ok(())
}

pub(crate) fn print_hash_output(hash: blake3::Hash, path: &Path, args: &Args, out: &mut OutBuf) -> err::Result<()> {
    let take = usize::try_from(args.len()).expect("standard 32-byte mode len <= 32");
    if args.raw() {
        out.write_bytes(&hash.as_bytes()[..take])?;
        return Ok(());
    }
    write_formatted_line(path, args, out, |out| write_hash_text(&hash, args, out))
}

fn write_hex_output(mut output: blake3::OutputReader, args: &Args, out: &mut OutBuf) -> err::Result<()> {
    let mut len = args.len();
    let mut block = [0u8; blake3::BLOCK_LEN];
    let mut hex_buf = [0u8; 2 * blake3::BLOCK_LEN];
    while len > 0 {
        // Only the bytes asked for: the output's last bytes are no whole
        // block, and filling one past its end is a panic.
        let take_bytes = usize::try_from(cmp::min(len, block.len() as u64)).expect("bounded by block.len()");
        output.fill(&mut block[..take_bytes]);
        encode_hex_stack(&block[..take_bytes], &mut hex_buf[..2 * take_bytes]);
        out.write_bytes(&hex_buf[..2 * take_bytes])?;
        len -= take_bytes as u64;
    }
    Ok(())
}

fn write_raw_output(mut output: blake3::OutputReader, args: &Args, out: &mut OutBuf) -> err::Result<()> {
    let mut len = args.len();
    let mut block = [0u8; 4096];
    while len > 0 {
        let take_bytes = usize::try_from(cmp::min(len, block.len() as u64)).expect("bounded by block.len()");
        output.fill(&mut block[..take_bytes]);
        out.write_bytes(&block[..take_bytes])?;
        len -= take_bytes as u64;
    }
    Ok(())
}

fn print_output(output: blake3::OutputReader, path: &Path, args: &Args, out: &mut OutBuf) -> err::Result<()> {
    if args.raw() {
        write_raw_output(output, args, out)?;
        return Ok(());
    }
    write_formatted_line(path, args, out, |out| write_hex_output(output, args, out))
}

fn read_key_from_stdin() -> err::Result<[u8; blake3::KEY_LEN]> {
    file_end::validate_byte_stream_fd(0)?;
    let mut bytes = Vec::with_capacity(blake3::KEY_LEN + 1);
    let n = std::io::stdin().lock().take(blake3::KEY_LEN as u64 + 1).read_to_end(&mut bytes)?;
    if n < blake3::KEY_LEN {
        bail!("expected {} key bytes from stdin, found {}", blake3::KEY_LEN, n,)
    } else if n > blake3::KEY_LEN {
        bail!("read more than {} key bytes from stdin", blake3::KEY_LEN)
    } else {
        Ok(bytes[..blake3::KEY_LEN].try_into().unwrap())
    }
}

struct FilepathString<'a> {
    filepath_string: std::borrow::Cow<'a, str>,
    is_escaped: bool,
}

fn filepath_to_string(filepath: &Path) -> FilepathString<'_> {
    let unicode_cow = filepath.to_string_lossy();
    if !unicode_cow.as_bytes().iter().any(|&b| matches!(b, b'\\' | b'\n' | b'\r')) {
        return FilepathString { filepath_string: unicode_cow, is_escaped: false };
    }
    let mut escaped = String::with_capacity(unicode_cow.len() + 8);
    for ch in unicode_cow.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            _ => escaped.push(ch),
        }
    }
    FilepathString { filepath_string: std::borrow::Cow::Owned(escaped), is_escaped: true }
}

fn hex_half_byte(b: u8) -> err::Result<u8> {
    match b {
        b'0'..=b'9' => Ok(b - b'0'),
        b'a'..=b'f' => Ok(b - b'a' + 10),
        _ => bail!("Invalid hex"),
    }
}

fn check_for_invalid_characters(utf8_path: &str) -> err::Result<()> {
    if utf8_path.contains('\0') {
        bail!("Null character in path");
    }
    if utf8_path.contains('\u{FFFD}') {
        bail!("Unicode replacement character in path");
    }
    Ok(())
}

fn unescape(mut path: &str) -> err::Result<String> {
    let mut unescaped = String::with_capacity(path.len());
    while let Some(i) = path.find('\\') {
        ensure!(i < path.len() - 1, "Invalid backslash escape");
        unescaped.push_str(&path[..i]);
        match path[i + 1..].chars().next().unwrap() {
            'n' => unescaped.push('\n'),
            'r' => unescaped.push('\r'),
            '\\' => unescaped.push('\\'),
            _ => bail!("Invalid backslash escape"),
        }
        path = &path[i + 2..];
    }
    unescaped.push_str(path);
    unescaped.shrink_to_fit();
    Ok(unescaped)
}

#[derive(Debug)]
struct ParsedCheckLine {
    /// Original escaped display text (`\...`), allocated ONLY when the line was
    /// escaped. Ordinary unescaped paths display directly from `file_path` so
    /// each path is allocated once.
    escaped_display: Option<String>,
    file_path: PathBuf,
    expected_hash: blake3::Hash,
}

impl ParsedCheckLine {
    fn display_str(&self) -> &str {
        self.escaped_display.as_deref().unwrap_or_else(|| self.file_path.to_str().expect("checkfile paths are valid UTF-8"))
    }

    fn retained_bytes(&self) -> usize {
        self.file_path.capacity() + self.escaped_display.as_ref().map_or(0, String::capacity)
    }
}

fn split_untagged_check_line(line_after_slash: &str) -> Option<(&str, &str)> {
    line_after_slash.split_once("  ")
}

fn split_tagged_check_line(line_after_slash: &str) -> Option<(&str, &str)> {
    let prefix = "BLAKE3 (";
    if !line_after_slash.starts_with(prefix) {
        return None;
    }
    line_after_slash[prefix.len()..].rsplit_once(") = ")
}

fn parse_check_line(mut line: &str) -> err::Result<ParsedCheckLine> {
    line = line.trim_end_matches(['\r', '\n']);
    let Some(first) = line.chars().next() else {
        bail!("Empty line");
    };
    let line_after_slash;
    let is_escaped;
    if first == '\\' {
        is_escaped = true;
        line_after_slash = &line[1..];
    } else {
        is_escaped = false;
        line_after_slash = line;
    }

    let hash_text;
    let file_str;
    // The tagged form first: it is told by its start and its last
    // `) = `, and its name may hold two spaces, which is how the other
    // form is cut (a file named `a  b` made a line of --tag that --check
    // called an invalid hash).
    if let Some((left, right)) = split_tagged_check_line(line_after_slash) {
        file_str = left;
        hash_text = right;
    } else if let Some((left, right)) = split_untagged_check_line(line_after_slash) {
        hash_text = left;
        file_str = right;
    } else {
        bail!("Invalid check line format");
    }

    let mut hash_bytes = [0; blake3::OUT_LEN];
    let hex_bytes = hash_text.as_bytes();
    ensure!(hex_bytes.len() == 2 * blake3::OUT_LEN, "Invalid hash length");
    for (byte, pair) in hash_bytes.iter_mut().zip(hex_bytes.chunks_exact(2)) {
        *byte = 16 * hex_half_byte(pair[0])? + hex_half_byte(pair[1])?;
    }
    let expected_hash: blake3::Hash = hash_bytes.into();

    let (file_path_string, escaped_display) = if is_escaped { (unescape(file_str)?, Some("\\".to_string() + file_str)) } else { (file_str.to_string(), None) };
    ensure!(!file_path_string.is_empty(), "empty file path");
    check_for_invalid_characters(&file_path_string)?;

    Ok(ParsedCheckLine { escaped_display, file_path: file_path_string.into(), expected_hash })
}

/// Hash `paths` in standard 32-byte mode (`seek == 0 && len <= 32`) across
/// cores (using `linux_pipe` on Linux and worker threads elsewhere), invoking
/// `on_ordered(idx, result)` in exact `0..paths.len()` order.
fn hash_paths_32(
    args: &Args,
    paths: &[PathBuf],
    stdin_ctx: &StdinContext,
    forbidden: Option<StreamId>,
    on_ordered: &mut dyn FnMut(usize, err::Result<blake3::Hash>) -> bool,
) {
    if paths.is_empty() {
        return;
    }
    if paths.len() == 1 {
        let _ = on_ordered(0, hash_path_hash(args, &paths[0], stdin_ctx, forbidden));
        return;
    }
    if stdin_named_twice(paths, stdin_ctx) {
        for (i, path) in paths.iter().enumerate() {
            if !on_ordered(i, hash_path_hash(args, path, stdin_ctx, forbidden)) {
                break;
            }
        }
        return;
    }

    #[cfg(target_os = "linux")]
    if linux_pipe::applies_to_paths(args, paths, stdin_ctx) {
        linux_pipe::hash_paths(args, paths, forbidden, on_ordered);
        return;
    }

    let total = threads().get();
    let max_workers = if args.num_threads().is_some() { max_file_threads(total) } else { total };
    let threads = max_workers.min(paths.len());
    let _file_threads_guard = args.num_threads().map(|_| FileThreadsGuard::enter(threads));
    run_ordered_collector(paths.len(), threads, |i| hash_path_hash(args, &paths[i], stdin_ctx, forbidden), on_ordered);
}

/// Indexed out-of-order result buffer that drains a contiguous prefix in order
/// and stops immediately on a rejected emission (L10).
pub(crate) struct OrderedCollector<T> {
    results: Vec<Option<T>>,
    emitted: usize,
    stopped: bool,
}

impl<T> OrderedCollector<T> {
    pub(crate) fn new(count: usize) -> Self {
        Self { results: (0..count).map(|_| None).collect(), emitted: 0, stopped: false }
    }

    /// Record `item` at `index` and emit every contiguous ready item in order.
    /// Returns `false` if `emit` declined an item (invoking `on_stop` once).
    pub(crate) fn insert_and_drain(&mut self, index: usize, item: T, emit: &mut (impl FnMut(usize, T) -> bool + ?Sized), on_stop: impl FnOnce()) -> bool {
        self.results[index] = Some(item);
        while self.emitted < self.results.len() {
            let Some(ready) = self.results[self.emitted].take() else { break };
            if !emit(self.emitted, ready) {
                self.stopped = true;
                on_stop();
                return false;
            }
            self.emitted += 1;
        }
        !self.stopped
    }

    pub(crate) fn finish(self, context: &str) {
        assert!(
            self.stopped || self.emitted == self.results.len(),
            "{context} returned after {}/{} items without cancellation",
            self.emitted,
            self.results.len()
        );
    }
}

/// Shared parallel ordered collector for generic plain and extended-output
/// multi-file runs (M3): workers claim indices via `next`, send results to the
/// main collector, and emit a contiguous prefix in order. When `on_ordered`
/// returns `false`, one close path publishes `next.fetch_max(count, SeqCst)`
/// and drops `done_rx` before joining workers so helpers stop claiming new
/// operands immediately. Asserts `emitted == count` on normal completion (M30).
pub(crate) fn run_ordered_collector<O: Send>(count: usize, threads: usize, work: impl Fn(usize) -> O + Sync, on_ordered: impl FnMut(usize, O) -> bool) {
    run_ordered_collector_inner(count, threads, work, on_ordered, |_| {});
}

pub(crate) fn run_ordered_collector_inner<O: Send>(
    count: usize,
    threads: usize,
    work: impl Fn(usize) -> O + Sync,
    mut on_ordered: impl FnMut(usize, O) -> bool,
    mut after_cancel: impl FnMut(usize),
) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let next = AtomicUsize::new(0);
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        for _ in 1..threads {
            if next.load(Ordering::Acquire) >= count {
                break;
            }
            let (next, done_tx, work) = (&next, done_tx.clone(), &work);
            scope.spawn(move || {
                loop {
                    let i = next.fetch_add(1, Ordering::AcqRel);
                    if i >= count {
                        break;
                    }
                    let output = work(i);
                    if done_tx.send((i, output)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(done_tx);

        let mut collector = OrderedCollector::new(count);
        let mut take = |i: usize, output: O| -> bool {
            collector.insert_and_drain(i, output, &mut on_ordered, || {
                let claimed_before_close = next.fetch_max(count, Ordering::SeqCst).min(count);
                after_cancel(claimed_before_close);
            })
        };
        let mut keep_going = true;
        loop {
            let i = next.fetch_add(1, Ordering::AcqRel);
            if i >= count {
                break;
            }
            if !take(i, work(i)) {
                keep_going = false;
                break;
            }
            while let Ok((wi, wout)) = done_rx.try_recv() {
                if !take(wi, wout) {
                    keep_going = false;
                    break;
                }
            }
            if !keep_going {
                break;
            }
        }
        if keep_going {
            while let Ok((i, output)) = done_rx.recv() {
                if !take(i, output) {
                    break;
                }
            }
        }
        drop(done_rx);
        collector.finish("ordered collector");
    });
}

/// The outboards' files are created by this many threads, apart from the
/// ones that hash: creating a file takes its directory's lock, so more
/// threads than a few only wait for one another, and the hashing threads
/// would wait with them.
const OUTBOARD_WRITERS: usize = 2;

/// `--outboard DIR`: each file's hash, printed as ever, and its outboard
/// written to DIR/<hash>.obao4 (outboard.rs).
///
/// The files are shared out among threads started once for the run, each
/// with rooms of its own for a file's bytes and chaining values; where
/// there are fewer files than threads, each file takes several. A thread
/// hands each outboard to the writers and takes the next file. The lines
/// are printed in the files' order, once all are done. One file is hashed
/// and written by the caller's thread alone.
fn outboard_inputs(args: &Args, dir: &Path, stdin_ctx: &StdinContext, out: &mut OutBuf) -> u64 {
    use std::sync::{Mutex, mpsc};
    let paths = &args.file_args;
    // Before any file is hashed: a run of short files alone would write
    // nothing, and say nothing of a directory that is not there.
    if !dir.is_dir() {
        eprintln_safe!("{}: {}: not a directory", NAME, dir.to_string_lossy());
        return paths.len().max(1) as u64;
    }
    let total = threads().get();
    // On an interactive terminal, finish writing and printing each operand
    // before opening the next so progress appears immediately and a later FIFO
    // cannot hold back earlier lines (M4).
    if out.is_line_flushed() {
        let mut rooms = outboard::Rooms::new();
        let mut nodes = Vec::new();
        let mut failed = 0u64;
        for path in paths {
            nodes.clear();
            let res = outboard_of(args, path, stdin_ctx, total, &mut rooms, &mut nodes).and_then(|hash| {
                if !nodes.is_empty() {
                    outboard::write_in(dir, &hash, &nodes)?;
                }
                print_hash_output(hash, path, args, out)
            });
            if let Err(e) = res {
                failed = failed.saturating_add(1);
                let _ = out.flush();
                eprintln_safe!("{}: {}: {}", NAME, path.to_string_lossy(), e);
                if out.is_poisoned() {
                    break;
                }
            }
        }
        return failed;
    }
    let max_workers = if args.num_threads().is_some() { max_file_threads(total) } else { total };
    let workers = if stdin_named_twice(paths, stdin_ctx) { 1 } else { max_workers.min(paths.len()) };
    let in_a_file = (total / workers.max(1)).max(1);
    let next = std::sync::atomic::AtomicUsize::new(0);
    let done = Mutex::new(Vec::<(usize, err::Result<blake3::Hash>)>::new());
    // To the writers: a file's index, its hash, its outboard; no more
    // waiting than a few a writer, so that outboards are not made faster
    // than they are written. Back from them: the outboards' buffers, for
    // the next files.
    type Made = (usize, blake3::Hash, Vec<u8>);
    let (to_writers, made) = mpsc::sync_channel::<Made>(8 * OUTBOARD_WRITERS);
    let (to_workers, spare) = mpsc::channel::<Vec<u8>>();
    let (made, spare) = (Mutex::new(made), Mutex::new(spare));
    let unwritten = Mutex::new(Vec::<(usize, io::Error)>::new());
    let write = |(i, hash, nodes): Made| {
        if let Err(e) = outboard::write_in(dir, &hash, &nodes) {
            unwritten.lock().unwrap().push((i, e));
        }
        // No worker may be left to take it.
        let _ = to_workers.send(nodes);
    };
    let work = |hand: &dyn Fn(Made)| {
        let mut rooms = outboard::Rooms::new();
        let mut mine = Vec::new();
        let mut nodes = Vec::new();
        loop {
            let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let Some(path) = paths.get(i) else { break };
            nodes.clear();
            let hashed = outboard_of(args, path, stdin_ctx, in_a_file, &mut rooms, &mut nodes);
            // An empty outboard (a file of one chunk group or less) is no
            // file: there is nothing in it to read, and creating a file
            // costs more than hashing a short one.
            if let Ok(hash) = &hashed
                && !nodes.is_empty()
            {
                hand((i, *hash, core::mem::take(&mut nodes)));
                nodes = spare.lock().unwrap().try_recv().unwrap_or_default();
            }
            mine.push((i, hashed));
        }
        done.lock().unwrap().append(&mut mine);
    };
    if workers <= 1 {
        work(&write);
    } else {
        std::thread::scope(|scope| {
            for _ in 0..OUTBOARD_WRITERS {
                scope.spawn(|| {
                    loop {
                        // The lock is let go before the file is written.
                        let next = made.lock().unwrap().recv();
                        let Ok(next) = next else { break };
                        write(next);
                    }
                });
            }
            let work = &work;
            for _ in 1..workers {
                let to_writers = to_writers.clone();
                scope.spawn(move || work(&|made| to_writers.send(made).expect("the writers run until every sender is done")));
            }
            work(&|made| to_writers.send(made).expect("the writers run until every sender is done"));
            // The writers end when the channel does: when every thread's
            // sender is gone.
            drop(to_writers);
        });
    }
    let unwritten = unwritten.into_inner().unwrap();
    let mut results: Vec<Option<err::Result<blake3::Hash>>> = (0..paths.len()).map(|_| None).collect();
    for (i, result) in done.into_inner().unwrap() {
        results[i] = Some(result);
    }
    for (i, e) in unwritten {
        results[i] = Some(Err(e.into()));
    }
    let mut failed = 0u64;
    for (path, result) in paths.iter().zip(results) {
        let result = result.expect("every file has its result");
        if let Err(e) = result.and_then(|hash| print_hash_output(hash, path, args, out)) {
            failed = failed.saturating_add(1);
            let _ = out.flush();
            eprintln_safe!("{}: {}: {}", NAME, path.to_string_lossy(), e);
            if out.is_poisoned() {
                break;
            }
        }
    }
    failed
}

/// Read up to `cap` bytes from `file` (fallibly allocating at most `cap`,
/// seeded from `says + 1`), returning `Ok(true)` if actual EOF (`read == 0`)
/// was observed within `cap`, or `Ok(false)` when `file` reaches `cap` without
/// EOF so the caller streams the prefix plus remaining bytes without truncating
/// at `says` (#424, M12). At `cap == 0`, returns `Ok(false)` with `bytes` empty
/// and `file` untouched.
pub(crate) fn read_bounded_to_eof(file: &mut impl Read, says: usize, cap: usize, bytes: &mut Vec<u8>) -> io::Result<bool> {
    bytes.clear();
    if cap == 0 {
        return Ok(false);
    }
    let initial = says.saturating_add(1).min(cap);
    bytes.try_reserve(initial).map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
    bytes.resize(initial, 0);
    let mut got = 0usize;
    loop {
        if got == bytes.len() {
            if got >= cap {
                return Ok(false);
            }
            let next_len = (bytes.len() * 2).max(4096).min(cap);
            bytes.try_reserve(next_len - bytes.len()).map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
            bytes.resize(next_len, 0);
        }
        match file.read(&mut bytes[got..]) {
            Ok(0) => {
                bytes.truncate(got);
                return Ok(true);
            }
            Ok(n) => got += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

/// The hash of the input at `path` (`-`: standard input), and its
/// outboard appended to `nodes`. A regular file under MAP_LEN is read into the
/// bounded thread room (promoting to streaming if it grows past MAP_LEN); a
/// longer cold file is read directly on Linux; any other regular file is mapped
/// where it can be (`hold`); stdin, pipes, and non-mapped fallbacks stream in
/// 16 KiB chunk groups without buffering the entire input in memory (M12).
fn outboard_of(
    args: &Args,
    path: &Path,
    stdin_ctx: &StdinContext,
    threads: usize,
    rooms: &mut outboard::Rooms,
    nodes: &mut Vec<u8>,
) -> err::Result<blake3::Hash> {
    let mode = args.mode;
    let bytes = &mut rooms.bytes;
    bytes.clear();
    match stdin_ctx.classify(path) {
        InputRole::StdinDash | InputRole::StdinStreamAlias => {
            // The key was standard input, read to its end: no message is
            // left there, and its tree would be the empty message's.
            if matches!(mode, blake3::Mode::Keyed(_)) {
                bail!("Cannot open `-` in keyed mode");
            }
            file_end::validate_byte_stream_fd(0)?;
            return Ok(outboard::outboard_from_reader(mode, &mut io::stdin().lock(), &mut rooms.cvs, nodes)?);
        }
        #[cfg(target_os = "macos")]
        InputRole::DarwinRegularDevFd => {
            use std::os::fd::{AsFd, AsRawFd};
            let file = File::open(path)?;
            let fd = file.as_raw_fd();
            let meta = file_end::validate_byte_stream_fd(fd)?;
            if meta.len >= MAP_LEN as u64
                && let Some(map) = args.map(&file)
            {
                let hash = outboard::outboard(mode, &map, threads, &mut rooms.cvs, nodes);
                hold(map);
                return Ok(hash);
            }
            return Ok(outboard::outboard_from_reader(mode, &mut file_end::PreadReader::new(file.as_fd(), 0), &mut rooms.cvs, nodes)?);
        }
        InputRole::Path => {}
    }
    let mut file = File::open(path)?;
    let fd_meta = {
        use std::os::fd::AsRawFd;
        file_end::validate_byte_stream_fd(file.as_raw_fd())?
    };
    if fd_meta.is_file() && fd_meta.len < MAP_LEN as u64 {
        if read_bounded_to_eof(&mut file, usize::try_from(fd_meta.len).expect("len < MAP_LEN"), MAP_LEN, bytes)? {
            return Ok(outboard::outboard(mode, bytes, threads, &mut rooms.cvs, nodes));
        }
        let mut combined = io::Cursor::new(&bytes[..]).chain(&mut file);
        return Ok(outboard::outboard_from_reader(mode, &mut combined, &mut rooms.cvs, nodes)?);
    }
    #[cfg(target_os = "linux")]
    let mut skip_map_after_change = false;
    #[cfg(target_os = "linux")]
    if fd_meta.is_file() {
        match outboard::outboard_direct(mode, &file, fd_meta.len, threads, &mut rooms.piece, &mut rooms.cvs, nodes) {
            Ok(hash) => return Ok(hash),
            Err(direct::Failure::Unavailable) => {}
            Err(direct::Failure::Changed) => skip_map_after_change = true,
            Err(direct::Failure::Err(e)) => return Err(e.into()),
        }
    }
    #[cfg(not(target_os = "linux"))]
    let skip_map_after_change = false;
    if fd_meta.is_file()
        && fd_meta.len > 0
        && !skip_map_after_change
        && let Some(map) = args.map(&file)
    {
        let hash = outboard::outboard(mode, &map, threads, &mut rooms.cvs, nodes);
        hold(map);
        return Ok(hash);
    }
    Ok(outboard::outboard_from_reader(mode, &mut file, &mut rooms.cvs, nodes)?)
}

fn hash_inputs(args: &Args, stdin_ctx: &StdinContext, out: &mut OutBuf) -> u64 {
    if let Some(dir) = args.outboard() {
        return outboard_inputs(args, dir, stdin_ctx, out);
    }
    if out.is_line_flushed() {
        // Interactive terminal output: hash and flush each operand in order
        // before opening the next so a later blocking FIFO/stream never holds
        // back earlier ready hashes (while each large file still uses all
        // hashing threads). Redirected runs stay on the batched pipelines (#465).
        let mut failed = 0u64;
        for path in &args.file_args {
            let res = if args.is_standard_32() {
                hash_path_hash(args, path, stdin_ctx, None).and_then(|h| print_hash_output(h, path, args, out))
            } else {
                hash_path(args, path, stdin_ctx).and_then(|o| print_output(o, path, args, out))
            };
            if let Err(e) = res {
                failed = failed.saturating_add(1);
                let _ = out.flush();
                eprintln_safe!("{}: {}: {}", NAME, path.to_string_lossy(), e);
                if out.is_poisoned() {
                    break;
                }
            }
        }
        return failed;
    }
    if args.is_standard_32() {
        let mut failed = 0u64;
        hash_paths_32(args, &args.file_args, stdin_ctx, None, &mut |idx, res| {
            let path = &args.file_args[idx];
            if let Err(e) = res.and_then(|h| print_hash_output(h, path, args, out)) {
                failed = failed.saturating_add(1);
                let _ = out.flush();
                eprintln_safe!("{}: {}: {}", NAME, path.to_string_lossy(), e);
                if out.is_poisoned() {
                    return false;
                }
            }
            true
        });
        return failed;
    }

    let total = threads().get();
    let max_workers = if args.num_threads().is_some() { max_file_threads(total) } else { total };
    let worker_threads = if args.file_args.len() == 1 || stdin_named_twice(&args.file_args, stdin_ctx) { 1 } else { max_workers.min(args.file_args.len()) };
    let _file_threads_guard = args.num_threads().map(|_| FileThreadsGuard::enter(worker_threads));
    let mut failed = 0u64;
    // On one worker thread there is no reordering problem: stream operands
    // directly without reserving a result slot per path (M8).
    if worker_threads <= 1 {
        for path in &args.file_args {
            if let Err(e) = hash_path(args, path, stdin_ctx).and_then(|output| print_output(output, path, args, out)) {
                failed = failed.saturating_add(1);
                let _ = out.flush();
                eprintln_safe!("{}: {}: {}", NAME, path.to_string_lossy(), e);
                if out.is_poisoned() {
                    break;
                }
            }
        }
        return failed;
    }
    run_ordered_collector(
        args.file_args.len(),
        worker_threads,
        |i| hash_path(args, &args.file_args[i], stdin_ctx),
        |idx, res| {
            let path = &args.file_args[idx];
            if let Err(e) = res.and_then(|output| print_output(output, path, args, out)) {
                failed = failed.saturating_add(1);
                let _ = out.flush();
                eprintln_safe!("{}: {}: {}", NAME, path.to_string_lossy(), e);
                if out.is_poisoned() {
                    return false;
                }
            }
            true
        },
    );
    failed
}

fn emit_checked_result(display: &str, expected_hash: blake3::Hash, res: err::Result<blake3::Hash>, args: &Args, out: &mut OutBuf) -> err::Result<bool> {
    let found_hash = match res {
        Ok(h) => h,
        Err(e) => {
            out.write_bytes(format!("{}: FAILED ({})\n", display, e).as_bytes())?;
            out.finish_line()?;
            return Ok(false);
        }
    };
    // Constant-time comparison (`blake3::Hash::eq`).
    if expected_hash == found_hash {
        if !args.quiet() {
            out.write_bytes(display.as_bytes())?;
            out.write_bytes(b": OK\n")?;
            out.finish_line()?;
        }
        Ok(true)
    } else {
        out.write_bytes(display.as_bytes())?;
        out.write_bytes(b": FAILED\n")?;
        out.finish_line()?;
        Ok(false)
    }
}

fn emit_checked_entry(parsed: ParsedCheckLine, res: err::Result<blake3::Hash>, args: &Args, out: &mut OutBuf) -> err::Result<bool> {
    emit_checked_result(parsed.display_str(), parsed.expected_hash, res, args, out)
}

/// Hard byte cap on one `--check` line (1 MiB, well above any OS path limit +
/// escapes + digest) so pointing `--check` at a giant newline-free file cannot
/// exhaust memory before reporting a parse error.
const MAX_CHECK_LINE_BYTES: usize = 1 << 20;

/// Hard byte budget on retained path/display payload across one `--check` batch
/// (8 MiB), so a manifest of up to `CHECK_CHUNK` long lines cannot retain
/// gigabytes of paths before hashing and draining.
const MAX_CHECK_BATCH_BYTES: usize = 8 << 20;

enum CheckBatchEntry {
    ParseErr(err::Error),
    Own(ParsedCheckLine),
    Stdin(ParsedCheckLine),
    /// Ordinary batchable file path: the `PathBuf` lives once in `batch_paths[batch_idx]`
    /// (zero duplicate `PathBuf` clone); `escaped_display` is `Some` only when the
    /// manifest line used backslash escaping so unescaped paths store one copy total.
    File {
        escaped_display: Option<String>,
        expected_hash: blake3::Hash,
        batch_idx: usize,
    },
}

/// Read one line from `reader` with a hard byte bound `max_bytes`, draining
/// any oversized remainder up to `\n` or EOF without allocating, and borrowing
/// validated UTF-8 directly from `raw`.
fn read_check_line_bounded<'a>(reader: &mut impl BufRead, raw: &'a mut Vec<u8>, max_bytes: usize) -> io::Result<Option<Result<&'a str, err::Error>>> {
    raw.clear();
    let mut too_long = false;
    let mut saw_any = false;
    loop {
        let avail = match reader.fill_buf() {
            Ok(buf) => buf,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if avail.is_empty() {
            break;
        }
        saw_any = true;
        if let Some(pos) = avail.iter().position(|&b| b == b'\n') {
            let take = pos + 1;
            if !too_long {
                if raw.len() + take > max_bytes {
                    too_long = true;
                } else {
                    raw.extend_from_slice(&avail[..take]);
                }
            }
            reader.consume(take);
            break;
        }
        let n = avail.len();
        if !too_long {
            if raw.len() + n > max_bytes {
                too_long = true;
            } else {
                raw.extend_from_slice(avail);
            }
        }
        reader.consume(n);
    }
    if !saw_any {
        return Ok(None);
    }
    if too_long {
        return Ok(Some(Err(format_err!("Check line exceeds maximum length"))));
    }
    match std::str::from_utf8(raw) {
        Ok(s) => Ok(Some(Ok(s))),
        Err(_) => Ok(Some(Err(format_err!("Invalid UTF-8 in check line")))),
    }
}

/// One classified record awaiting placement in a check batch.
enum PendingCheckRecord {
    ParseErr(err::Error),
    Own(ParsedCheckLine),
    Stdin(ParsedCheckLine),
    File(ParsedCheckLine),
}

impl PendingCheckRecord {
    fn retained_bytes(&self) -> usize {
        match self {
            PendingCheckRecord::ParseErr(_) => 64,
            PendingCheckRecord::Own(p) | PendingCheckRecord::Stdin(p) | PendingCheckRecord::File(p) => p.retained_bytes(),
        }
    }
}

/// Collect up to `max_entries` check entries whose total retained path/display
/// bytes stay strictly within `max_batch_bytes` (without overshooting on a
/// second entry; an oversized record is stashed in `pending` for the next batch,
/// while a single record when `entries.is_empty()` is always accepted for
/// forward progress). Returns `Ok(true)` when EOF was reached AND `pending` is
/// empty.
fn collect_check_batch(
    reader: &mut impl BufRead,
    raw_line: &mut Vec<u8>,
    pending: &mut Option<PendingCheckRecord>,
    entries: &mut Vec<CheckBatchEntry>,
    batch_paths: &mut Vec<PathBuf>,
    max_entries: usize,
    max_batch_bytes: usize,
    max_line_bytes: usize,
    stdin_ctx: &StdinContext,
    is_the_checkfile: &impl Fn(&Path) -> bool,
) -> io::Result<bool> {
    entries.clear();
    batch_paths.clear();
    let mut retained_bytes: usize = 0;
    while entries.len() < max_entries {
        let rec = match pending.take() {
            Some(r) => r,
            None => {
                let Some(line_res) = read_check_line_bounded(reader, raw_line, max_line_bytes)? else {
                    return Ok(true);
                };
                match line_res.and_then(parse_check_line) {
                    Err(e) => PendingCheckRecord::ParseErr(e),
                    Ok(parsed) if is_the_checkfile(&parsed.file_path) => PendingCheckRecord::Own(parsed),
                    Ok(parsed) if matches!(stdin_ctx.classify(&parsed.file_path), InputRole::StdinDash | InputRole::StdinStreamAlias) => {
                        PendingCheckRecord::Stdin(parsed)
                    }
                    Ok(parsed) => PendingCheckRecord::File(parsed),
                }
            }
        };
        let rec_bytes = rec.retained_bytes();
        if !entries.is_empty() && retained_bytes.saturating_add(rec_bytes) > max_batch_bytes {
            *pending = Some(rec);
            return Ok(false);
        }
        retained_bytes = retained_bytes.saturating_add(rec_bytes);
        match rec {
            PendingCheckRecord::ParseErr(e) => entries.push(CheckBatchEntry::ParseErr(e)),
            PendingCheckRecord::Own(parsed) => entries.push(CheckBatchEntry::Own(parsed)),
            PendingCheckRecord::Stdin(parsed) => entries.push(CheckBatchEntry::Stdin(parsed)),
            PendingCheckRecord::File(parsed) => {
                let batch_idx = batch_paths.len();
                batch_paths.push(parsed.file_path);
                entries.push(CheckBatchEntry::File { escaped_display: parsed.escaped_display, expected_hash: parsed.expected_hash, batch_idx });
            }
        }
    }
    Ok(false)
}

/// Owning reader for a checkfile source, binding every positioned `pread`
/// reader to the lifetime of its open descriptor instead of a raw fd (M6).
enum CheckfileReader<'a> {
    File(File),
    Stdin(io::StdinLock<'a>),
    #[cfg(target_os = "macos")]
    PreadFile {
        file: File,
        offset: u64,
    },
    #[cfg(target_os = "macos")]
    PreadStdin {
        stdin: io::StdinLock<'a>,
        offset: u64,
    },
}

impl Read for CheckfileReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        #[cfg(target_os = "macos")]
        use std::os::fd::AsFd;
        match self {
            CheckfileReader::File(f) => f.read(buf),
            CheckfileReader::Stdin(s) => s.read(buf),
            #[cfg(target_os = "macos")]
            CheckfileReader::PreadFile { file, offset } => file_end::pread_at(file.as_fd(), offset, buf),
            #[cfg(target_os = "macos")]
            CheckfileReader::PreadStdin { stdin, offset } => file_end::pread_at(stdin.as_fd(), offset, buf),
        }
    }
}

fn check_one_checkfile(path: &Path, args: &Args, stdin_ctx: &StdinContext, files_failed: &mut u64, out: &mut OutBuf) -> err::Result<()> {
    #[cfg(target_os = "macos")]
    use std::os::fd::AsFd;
    const CHECK_CHUNK: usize = 4096;

    let stdin = io::stdin();
    let own: Option<StreamId>;
    let checkfile_is_stdin;
    let reader: CheckfileReader<'_> = match stdin_ctx.classify(path) {
        InputRole::StdinDash => {
            checkfile_is_stdin = true;
            let stdin_lock = stdin.lock();
            let meta = file_end::validate_byte_stream_fd(0)?;
            own = (meta.kind == file_end::FileKind::Stream).then_some(meta.id);
            #[cfg(target_os = "macos")]
            if meta.kind == file_end::FileKind::Regular {
                let start = file_end::current_fd_offset(stdin_lock.as_fd())?;
                CheckfileReader::PreadStdin { stdin: stdin_lock, offset: start }
            } else {
                CheckfileReader::Stdin(stdin_lock)
            }
            #[cfg(not(target_os = "macos"))]
            {
                let _ = &meta;
                CheckfileReader::Stdin(stdin_lock)
            }
        }
        InputRole::StdinStreamAlias => {
            checkfile_is_stdin = true;
            let stdin_lock = stdin.lock();
            let meta = file_end::validate_byte_stream_fd(0)?;
            own = (meta.kind == file_end::FileKind::Stream).then_some(meta.id);
            CheckfileReader::Stdin(stdin_lock)
        }
        #[cfg(target_os = "macos")]
        InputRole::DarwinRegularDevFd => {
            use std::os::fd::AsRawFd;
            checkfile_is_stdin = false;
            own = None;
            let file = File::open(path)?;
            let _ = file_end::validate_byte_stream_fd(file.as_raw_fd())?;
            CheckfileReader::PreadFile { file, offset: 0 }
        }
        InputRole::Path => {
            use std::os::fd::AsRawFd;
            let file = File::open(path)?;
            let meta = file_end::validate_byte_stream_fd(file.as_raw_fd())?;
            own = (meta.kind == file_end::FileKind::Stream).then_some(meta.id);
            checkfile_is_stdin = own.is_some() && own == stdin_ctx.stdin_stream_id();
            #[cfg(target_os = "macos")]
            if meta.kind == file_end::FileKind::Regular {
                // On macOS, opening `/dev/fd/N` shares the underlying open
                // file description offset (`fd(4)`). Reading a regular
                // checkfile via positioned `pread` prevents any `-` or
                // `/dev/fd/M` entry from advancing the parser cursor behind
                // its back (#515).
                let start = file_end::current_fd_offset(file.as_fd())?;
                CheckfileReader::PreadFile { file, offset: start }
            } else {
                CheckfileReader::File(file)
            }
            #[cfg(not(target_os = "macos"))]
            {
                CheckfileReader::File(file)
            }
        }
    };
    let check_buf_cap = min_packet_read_len().max(64 * 1024);
    let mut bufreader = io::BufReader::with_capacity(check_buf_cap, reader);

    // The checkfile's own stream, by any name (few checkfiles are
    // streams: for those every entry is looked at, AND the actual opened
    // descriptor is checked again against `own` before any bytes are read).
    let is_the_checkfile = |entry: &Path| {
        match stdin_ctx.classify(entry) {
            InputRole::StdinDash => return checkfile_is_stdin,
            InputRole::StdinStreamAlias => return checkfile_is_stdin,
            _ => {}
        }
        if own.is_some_and(|own| file_end::path_matches_stream(entry, own)) {
            return true;
        }
        false
    };

    let mut raw_line = Vec::new();
    let mut pending = None;
    let mut entries = Vec::new();
    let mut batch_paths = Vec::new();
    let mut eof = false;
    let mut saw_line = false;

    // On an interactive terminal, check and emit one entry at a time so
    // progress appears immediately and a later blocking FIFO or slow manifest
    // cannot hold back earlier results (M4). Keep full batching for redirected
    // output.
    let batch_limit = if out.is_line_flushed() { 1 } else { CHECK_CHUNK };

    let loop_res: err::Result<()> = (|| {
        while !eof {
            eof = collect_check_batch(
                &mut bufreader,
                &mut raw_line,
                &mut pending,
                &mut entries,
                &mut batch_paths,
                batch_limit,
                MAX_CHECK_BATCH_BYTES,
                MAX_CHECK_LINE_BYTES,
                stdin_ctx,
                &is_the_checkfile,
            )?;
            if entries.is_empty() {
                break;
            }
            saw_line = true;

            let mut batch_results: Vec<Option<err::Result<blake3::Hash>>> = (0..batch_paths.len()).map(|_| None).collect();
            hash_paths_32(args, &batch_paths, stdin_ctx, own, &mut |idx, res| {
                batch_results[idx] = Some(res);
                true
            });

            for entry in entries.drain(..) {
                let ok = match entry {
                    CheckBatchEntry::ParseErr(e) => {
                        let _ = out.flush();
                        eprintln_safe!("{}: {}", NAME, e);
                        false
                    }
                    CheckBatchEntry::Own(parsed) => {
                        let what = if checkfile_is_stdin { "standard input is the checkfile; it" } else { "this checkfile is a stream, and" };
                        emit_checked_entry(parsed, Err(format_err!("{what} cannot also be a file to check")), args, out)?
                    }
                    CheckBatchEntry::Stdin(parsed) => {
                        let res = hash_path_hash(args, &parsed.file_path, stdin_ctx, own);
                        emit_checked_entry(parsed, res, args, out)?
                    }
                    CheckBatchEntry::File { escaped_display, expected_hash, batch_idx } => {
                        let res = batch_results[batch_idx].take().expect("every batch path hashed");
                        let display = escaped_display.as_deref().unwrap_or_else(|| batch_paths[batch_idx].to_str().expect("checkfile paths are valid UTF-8"));
                        emit_checked_result(display, expected_hash, res, args, out)?
                    }
                };
                if !ok {
                    *files_failed = files_failed.saturating_add(1);
                }
            }
        }
        Ok(())
    })();
    // Advance fd 0's shared offset to match all bytes consumed from the
    // checkfile even when the loop terminated early on an I/O or write error (M2).
    #[cfg(target_os = "macos")]
    let seek_res = {
        let buffered = bufreader.buffer().len() as u64;
        if let CheckfileReader::PreadStdin { stdin, offset } = bufreader.into_inner() {
            file_end::set_fd_offset(stdin.as_fd(), offset.saturating_sub(buffered))
        } else {
            Ok(())
        }
    };
    loop_res?;
    #[cfg(target_os = "macos")]
    seek_res?;
    ensure!(saw_line, "{}: no checksum entries", path.display());
    Ok(())
}

fn explain_sigbus() {
    static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    extern "C" fn on_sigbus(_: libc::c_int) {
        if !SAID.swap(true, std::sync::atomic::Ordering::SeqCst) {
            const MESSAGE: &[u8] = b"b3sum: a bus error while reading a mapped file; most likely another program truncated it, so no digest of it would be right. Hash the files again once nothing writes to them.\n";
            file_end::write_fatal_nonblocking(MESSAGE);
        }
        // Sound: `_exit` is async-signal-safe; terminate immediately without
        // blocking on stderr or waiting in `pause()` for another handler.
        unsafe { libc::_exit(1) };
    }
    let handler: extern "C" fn(libc::c_int) = on_sigbus;
    // Sound: the handler makes only calls a signal handler may make, and
    // SIGBUS is this program's to handle.
    unsafe { libc::signal(libc::SIGBUS, handler as libc::sighandler_t) };
}

fn main() -> err::Result<()> {
    // Sound: before any thread starts; a broken pipe then ends b3sum, as it
    // does other tools.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    explain_sigbus();
    blake3::initialize();
    let args = Args::parse()?;
    configure_threads(&args);
    let stdin_ctx = StdinContext::current();
    let mut out = OutBuf::new();
    let mut files_failed = 0u64;
    if args.check() {
        for path in &args.file_args {
            if let Err(e) = check_one_checkfile(path, &args, &stdin_ctx, &mut files_failed, &mut out) {
                let _ = out.flush();
                return Err(e);
            }
        }
    } else {
        files_failed = hash_inputs(&args, &stdin_ctx, &mut out);
    }
    out.flush()?;
    if args.check() && files_failed > 0 {
        eprintln_safe!("{}: WARNING: {} computed checksum{} did NOT match", NAME, files_failed, if files_failed == 1 { "" } else { "s" },);
    }
    let status = if files_failed > 0 { 1 } else { 0 };
    use std::io::Write;
    let _ = std::io::stderr().flush();
    // Sound: ends the process; standard output and error were flushed above.
    unsafe { libc::_exit(status) }
}

#[cfg(test)]
mod test {
    include!("../tools/require_nextest.rs");
    use crate::args::{Command, ParseOutcome, parse_from};

    #[test]
    fn test_args_basics_and_conflicts() {
        blake3::initialize();
        assert_eq!(parse_from(["b3sum", "--help"]), Ok(ParseOutcome::Help));
        assert_eq!(parse_from(["b3sum", "-h"]), Ok(ParseOutcome::Help));
        assert_eq!(parse_from(["b3sum", "--version"]), Ok(ParseOutcome::Version));
        assert_eq!(parse_from(["b3sum", "-V"]), Ok(ParseOutcome::Version));

        let Ok(ParseOutcome::Parsed(p)) = parse_from(["b3sum", "-l64", "--seek=8", "--num-threads", "2", "--", "-weird"]) else {
            panic!("expected parsed");
        };
        assert_eq!(p.command, Command::Hash { length: 64, seek: 8, format: crate::args::OutputFormat::Text(crate::args::TextFormat::Gnu) });
        assert_eq!(p.num_threads.unwrap().get(), 2);
        assert_eq!(p.files, vec![std::path::PathBuf::from("-weird")]);

        // Conflicts & requirements
        assert!(parse_from(["b3sum", "--keyed"]).is_err());
        assert!(parse_from(["b3sum", "--keyed", "--derive-key", "ctx", "f"]).is_err());
        assert!(parse_from(["b3sum", "--quiet"]).is_err());
        assert!(parse_from(["b3sum", "--check", "--seek", "0"]).is_err());
        assert!(parse_from(["b3sum", "--check", "--length", "32"]).is_err());
        assert!(parse_from(["b3sum", "--check", "--raw"]).is_err());
        assert!(parse_from(["b3sum", "--check", "--tag"]).is_err());
        assert!(parse_from(["b3sum", "--raw", "--tag"]).is_err());
        assert!(parse_from(["b3sum", "--tag", "--raw"]).is_err());
        assert!(parse_from(["b3sum", "--check", "--no-names"]).is_err());
        assert!(parse_from(["b3sum", "--check", "--keyed", "f"]).is_err());
        assert!(parse_from(["b3sum", "--check", "--derive-key", "x"]).is_err());
        assert!(parse_from(["b3sum", "--check", "--outboard", "d"]).is_err());
        assert!(parse_from(["b3sum", "--outboard", "d", "--length", "32"]).is_err());
        assert!(parse_from(["b3sum", "--outboard", "d", "--seek", "0"]).is_err());
        assert!(parse_from(["b3sum", "--outboard", "d", "--raw"]).is_err());
        assert!(parse_from(["b3sum", "--outboard", "d", "--derive-key", "x"]).is_err());
        // Check + quiet IS allowed; outboard + keyed + file IS allowed
        assert!(matches!(parse_from(["b3sum", "-c", "--quiet"]), Ok(ParseOutcome::Parsed(_))));
        assert!(matches!(parse_from(["b3sum", "--outboard", "d", "--keyed", "f"]), Ok(ParseOutcome::Parsed(_))));
        // Duplicate flags rejected
        assert!(parse_from(["b3sum", "--raw", "--raw"]).is_err());
        assert!(parse_from(["b3sum", "-l", "16", "--length", "32"]).is_err());
        // Missing / invalid values
        assert!(parse_from(["b3sum", "--length"]).is_err());
        assert!(parse_from(["b3sum", "--num-threads", "0"]).is_err());
        assert!(parse_from(["b3sum", "--unknown"]).is_err());
    }

    #[test]
    fn test_outbuf_buffers_when_not_line_flushed() {
        // Under cargo-nextest stdout is redirected to a pipe (not a terminal):
        // OutBuf must keep line bytes buffered across finish_line() until
        // OUT_BUF_CAP or explicit flush(), preserving piped batching (#465).
        let mut out = crate::OutBuf { buf: Vec::with_capacity(crate::OUT_BUF_CAP), poisoned: false, flush_each_line: false };
        assert!(!out.is_line_flushed());
        out.write_bytes(b"abc  file\n").unwrap();
        out.finish_line().unwrap();
        assert_eq!(out.buf.as_slice(), b"abc  file\n", "redirected OutBuf must retain bytes across finish_line");
        // With line flushing enabled, finish_line drains the buffer immediately.
        out.flush_each_line = true;
        out.finish_line().unwrap();
        assert!(out.buf.is_empty(), "terminal OutBuf must drain on finish_line");
    }
}
