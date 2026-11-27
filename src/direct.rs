//! A cold file read past the page cache (Linux, O_DIRECT), several reads
//! at once, each thread hashing the piece it has just read.
//!
//! A drive answers several reads at once faster than one after another,
//! and a direct read skips the page cache's copy: on a Ryzen 9700X's NVMe
//! drives a cold 1 GiB file is read at 10 GB/s this way (16 threads; a
//! ring at the same depth reads no faster), at 7.6 GB/s through the cache
//! by a ring, at 5 GB/s through a map. The page cache does not hold the
//! file afterwards: a large set of files hashed once is the common case,
//! and is too large to cache (Zooko, November 14, 2026).

use blake3::throughput::{Cv, TreeHasher, level_of};

/// A cold file is read in pieces of this many bytes, each by one direct
/// read: 128 groups, so a piece starts on a group and its groups'
/// chaining values are a run of the file's.
pub(crate) const PIECE: usize = 2 << 20;

/// A file of at least this many bytes that the page cache does not hold
/// is read directly.
pub(crate) const DIRECT_MIN: u64 = 2 * PIECE as u64;

#[repr(align(4096))]
struct AlignedPiece {
    _bytes: [u8; PIECE],
}
const _: () = assert!(core::mem::size_of::<AlignedPiece>() == PIECE && core::mem::align_of::<AlignedPiece>() == 4096);

/// A buffer a direct read can fill: PIECE bytes, 4096-byte aligned.
pub(crate) struct Piece(Box<core::mem::MaybeUninit<AlignedPiece>>);

impl Piece {
    pub(crate) fn new() -> Piece {
        // Not zeroed: zeroing would touch every page (512 page faults, 0.7 ms
        // on a Ryzen 7700X), and only bytes that a read has filled are ever read.
        Piece(Box::new_uninit())
    }

    #[inline]
    fn as_mut_ptr(&mut self) -> *mut u8 {
        self.0.as_mut_ptr().cast()
    }
}

std::thread_local! {
    /// How many files this thread has set out to hash directly.
    #[cfg(test)]
    pub(crate) static HASHED: core::cell::Cell<usize> = const { core::cell::Cell::new(0) };
    /// The tests' seam: set, this thread takes every file for cold (a
    /// file just written is in the cache).
    #[cfg(test)]
    pub(crate) static TAKE_FOR_COLD: core::cell::Cell<bool> = const { core::cell::Cell::new(false) };
}

/// Whether `file`, of `len` bytes, is one to read directly: DIRECT_MIN
/// bytes or more, and its first byte not in the page cache (a read that
/// may not wait says; a file system without such reads, as a tmpfs, says
/// something else: its files are in memory).
pub(crate) fn is_for(file: &std::fs::File, len: u64) -> bool {
    use std::os::fd::AsRawFd;
    if len < DIRECT_MIN {
        return false;
    }
    let mut byte = 0u8;
    let iov = libc::iovec { iov_base: (&raw mut byte).cast(), iov_len: 1 };
    // Sound: one iovec over one writable byte.
    let got = unsafe { libc::preadv2(file.as_raw_fd(), &iov, 1, 0, libc::RWF_NOWAIT) };
    let cold = got == -1 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock;
    #[cfg(test)]
    let cold = cold || TAKE_FOR_COLD.with(|c| c.get()) || crate::linux_pipe::force_miss();
    cold
}

/// Why an `O_DIRECT` read or hash attempt did not complete directly.
///
/// Separating `Unavailable` (reopen failed or `EINVAL` alignment/filesystem
/// refusal) from `Changed` (the file shrank during reads or grew past `len`
/// at the EOF probe) and `Err` (real I/O error) ensures callers never fall back
/// to a stale `mmap` after observing that the file's extent changed, never
/// fail a readable file merely because `O_DIRECT` is unsupported, and never
/// swallow real read errors.
#[derive(Debug)]
pub(crate) enum Failure {
    Unavailable,
    Changed,
    Err(std::io::Error),
}

impl Failure {
    fn priority(&self) -> u8 {
        match self {
            Failure::Unavailable => 1,
            Failure::Changed => 2,
            Failure::Err(_) => 3,
        }
    }
}

#[cfg(test)]
std::thread_local! {
    /// Test-only hook invoked right before the final 1-byte EOF probe in
    /// `direct::read` so tests can deterministically grow the file after all
    /// `O_DIRECT` piece reads complete.
    pub(crate) static BEFORE_EOF_PROBE: std::cell::RefCell<Option<Box<dyn FnMut()>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) static CLAIMED_PIECES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
pub(crate) static INJECT_EIO_PIECE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(usize::MAX);

#[cfg(test)]
pub(crate) static INJECT_EIO_RECORDED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Read `file` directly, PIECE bytes at a time, across `threads` threads
/// (this one using `mine`, the rest owning a piece each). For each piece
/// `i`, calls `each` with the piece's bytes and its `per` slots of `outs`.
/// Returns `Err(Failure::Unavailable)` if `O_DIRECT` cannot be opened or is
/// refused (`EINVAL`/`EOPNOTSUPP`), `Err(Failure::Changed)` if the file shrank
/// or grew past `len`, and `Err(Failure::Err)` on a real I/O error.
pub(crate) fn read<T: Send>(
    file: &std::fs::File,
    len: usize,
    threads: usize,
    mine: &mut Piece,
    outs: &mut [T],
    per: usize,
    each: impl Fn(usize, &[u8], &mut [T]) + Sync,
) -> Result<(), Failure> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    assert!(per > 0, "direct::read requires at least one output slot per piece");
    let expected_pieces = len.div_ceil(PIECE);
    assert!(expected_pieces > 0, "direct::read requires a non-empty length");
    assert_eq!(outs.chunks_mut(per).len(), expected_pieces, "direct::read output slice must cover every piece");
    let opened = file;
    // Open a new description of this very file, with independent flags and
    // offset. Reopening its old pathname could hash a different file after
    // a rename; changing flags on a dup would change the caller's too.
    // Without procfs, the caller uses its original descriptor another way.
    let direct = format!("/proc/self/fd/{}", file.as_raw_fd());
    let Ok(file) = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_DIRECT).open(direct) else {
        return Err(Failure::Unavailable);
    };
    let threads = threads.min(len.div_ceil(PIECE)).max(1);
    struct Shared<'a, T> {
        pieces: core::iter::Enumerate<core::slice::ChunksMut<'a, T>>,
        failure: Option<Failure>,
    }
    let shared = std::sync::Mutex::new(Shared { pieces: outs.chunks_mut(per).enumerate(), failure: None });
    let record_failure = |fail: Failure| {
        let mut s = shared.lock().unwrap();
        if s.failure.as_ref().is_none_or(|cur| fail.priority() > cur.priority()) {
            s.failure = Some(fail);
        }
    };
    let work = |buffer: &mut Piece| {
        loop {
            let next_piece = {
                let mut s = shared.lock().unwrap();
                if s.failure.is_some() {
                    return;
                }
                s.pieces.next()
            };
            let Some((i, outs)) = next_piece else { return };
            #[cfg(test)]
            CLAIMED_PIECES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            #[cfg(test)]
            if i == INJECT_EIO_PIECE.load(std::sync::atomic::Ordering::Relaxed) {
                record_failure(Failure::Err(std::io::Error::from_raw_os_error(libc::EIO)));
                INJECT_EIO_RECORDED.store(true, std::sync::atomic::Ordering::Release);
                return;
            }
            let want = PIECE.min(len - i * PIECE);
            let got = loop {
                // Sound: the buffer holds PIECE writable bytes. A piece
                // starts on a multiple of PIECE, so of the drive's blocks.
                let rc = unsafe { libc::pread(file.as_raw_fd(), buffer.as_mut_ptr().cast(), PIECE, (i * PIECE) as libc::off_t) };
                if rc == -1 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                break rc;
            };
            if got < 0 {
                let err = std::io::Error::last_os_error();
                if matches!(err.raw_os_error(), Some(libc::EINVAL | libc::EOPNOTSUPP)) {
                    record_failure(Failure::Unavailable);
                } else {
                    record_failure(Failure::Err(err));
                }
                return;
            }
            if got != want as isize {
                record_failure(Failure::Changed);
                return;
            }
            // Sound: `want` initialized bytes of the buffer.
            each(i, unsafe { core::slice::from_raw_parts(buffer.as_mut_ptr(), want) }, outs);
        }
    };
    if threads == 1 {
        work(mine);
    } else {
        std::thread::scope(|scope| {
            for _ in 1..threads {
                scope.spawn(|| work(&mut Piece::new()));
            }
            work(mine);
        });
    }
    if let Some(fail) = shared.into_inner().unwrap().failure {
        return Err(fail);
    }
    #[cfg(test)]
    BEFORE_EOF_PROBE.with(|slot| {
        if let Some(mut f) = slot.borrow_mut().take() {
            f();
        }
    });
    // The reads above found `len` bytes; one read more says there are no
    // others. A last piece that is whole was asked for no byte more than
    // it has, so its read does not say the file ends there (BLAKE3 #424:
    // a file that had grown past a multiple of PIECE gave the hash of its
    // first `len` bytes, with no sign of the rest). A last piece that is
    // short was one read that stopped, and a read may stop before a
    // file's end. The read is of one byte at `len`, of the file as the
    // caller opened it: a direct read starts on a block, and `len` need
    // not.
    let mut byte = 0u8;
    loop {
        // Sound: one writable byte.
        let more = unsafe { libc::pread(opened.as_raw_fd(), (&raw mut byte).cast(), 1, len as libc::off_t) };
        if more == -1 && std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
            continue;
        }
        return if more == 0 {
            Ok(())
        } else if more > 0 {
            Err(Failure::Changed)
        } else {
            Err(Failure::Err(std::io::Error::last_os_error()))
        };
    }
}

/// The hash of the open `file`, of `len` bytes (`len > PIECE`, two pieces or
/// more), read directly: each piece's subtree hashed by the thread that read
/// it, on its own core's kernels, then the pieces' parents a layer at a time.
/// Propagates [`Failure`] as [`read`] says.
pub(crate) fn hash(mode: blake3::Mode, file: &std::fs::File, len: usize, threads: usize, mine: &mut Piece) -> Result<blake3::Hash, Failure> {
    assert!(len > PIECE, "two pieces or more");
    #[cfg(test)]
    HASHED.with(|h| h.set(h.get() + 1));
    // Checked when this compiles: a piece is a whole subtree's size.
    const LEVEL: u32 = level_of(PIECE as u64);
    let tree = TreeHasher::new(mode);
    let pieces = len.div_ceil(PIECE);
    // A slot a piece, as `read` shares them out. The last piece is the
    // message's last subtree, a value of its own kind: its slot stays unset.
    let mut cvs = vec![Cv::unset(); pieces];
    let last = std::sync::OnceLock::new();
    {
        let tree = tree.clone().on_this_core();
        read(file, len, threads, mine, &mut cvs, 1, |i, bytes, cv| {
            if i + 1 == pieces {
                last.set(tree.last_cv((i << LEVEL) as u64, bytes)).expect("one thread reads the last piece");
            } else {
                cv[0] = tree.subtree_cv(LEVEL, i as u64, bytes);
            }
        })?;
    }
    let mut last = last.into_inner().expect("the last piece was read");
    cvs.truncate(pieces - 1);
    // A layer's whole values pair up. An odd one out is the last
    // subtree's left sibling.
    let mut next = vec![Cv::unset(); cvs.len() / 2];
    while cvs.len() > 1 {
        let pairs = cvs.len() / 2;
        tree.parent_cvs(&cvs[..2 * pairs], &mut next[..pairs]);
        if cvs.len() % 2 == 1 {
            last = tree.last_parent_cv(&cvs[2 * pairs], &last);
        }
        cvs.truncate(pairs);
        cvs.copy_from_slice(&next[..pairs]);
    }
    Ok(tree.root_hash(&cvs[0], &last, len as u64))
}

#[cfg(test)]
mod test {
    use super::*;
    use std::os::unix::fs::OpenOptionsExt;

    /// A file read directly hashes as its bytes do: at two pieces, an odd
    /// count of pieces, a ragged last piece and a last piece of one byte;
    /// on one thread and on several; plain and keyed. A length that the
    /// file's pieces do not bear returns `Err(Failure::Changed)`. (Where the
    /// test's directory has no direct reads, as a tmpfs before Linux 6.6, there
    /// is nothing to compare.)
    #[test]
    fn a_file_read_directly_hashes_as_its_bytes_do() {
        blake3::initialize();
        let dir = crate::tempfile::tempdir().unwrap();
        let all: Vec<u8> = (0..7 * PIECE + 12345).map(|i: usize| (i.wrapping_mul(37) ^ (i >> 11)) as u8).collect();
        let key = [0x33u8; 32];
        let mut piece = Piece::new();
        for len in [PIECE + 1, 2 * PIECE, 2 * PIECE + 1, 3 * PIECE, 4 * PIECE - 1, 5 * PIECE + 4096, all.len()] {
            let path = dir.path().join(format!("file-{len}"));
            std::fs::write(&path, &all[..len]).unwrap();
            if std::fs::OpenOptions::new().read(true).custom_flags(libc::O_DIRECT).open(&path).is_err() {
                eprintln!("no direct reads in {}", dir.path().display());
                return;
            }
            let file = std::fs::File::open(&path).unwrap();
            for mode in [blake3::Mode::Plain, blake3::Mode::Keyed(&key)] {
                let want = blake3::convenience::hash_with(mode, &all[..len]);
                for threads in [1, 2, 5] {
                    assert!(matches!(hash(mode, &file, len, threads, &mut piece), Ok(h) if h == want), "len={len} threads={threads}");
                }
                // Any other length, one that ends a piece too: the file's
                // end is where its reads say (`Err(Failure::Changed)`, distinct
                // from `Err(Failure::Unavailable)`).
                for wrong in [len + 1, len - 1, len + PIECE, len - PIECE, len.next_multiple_of(PIECE) - PIECE] {
                    if wrong == len || wrong <= PIECE {
                        continue;
                    }
                    assert!(matches!(hash(mode, &file, wrong, 2, &mut piece), Err(Failure::Changed)), "len={len} read as {wrong}");
                }
            }
        }
        // What is read directly: a cold file of DIRECT_MIN bytes or more.
        let path = dir.path().join(format!("file-{}", all.len()));
        let file = std::fs::File::open(&path).unwrap();
        assert!(!is_for(&file, all.len() as u64), "just written: the cache holds it");
        TAKE_FOR_COLD.with(|c| c.set(true));
        assert!(is_for(&file, DIRECT_MIN) && !is_for(&file, DIRECT_MIN - 1));
        TAKE_FOR_COLD.with(|c| c.set(false));
    }

    /// Replacing a pathname after open must not switch which file is
    /// hashed. Rename is the synchronization: no racing threads or sleeps.
    #[test]
    fn direct_reads_keep_the_open_file_when_its_path_is_replaced() {
        blake3::initialize();
        if crate::threads().get() == 1 {
            return;
        }
        let dir = crate::tempfile::tempdir().unwrap();
        let path = dir.path().join("input");
        let original = vec![0x35u8; DIRECT_MIN as usize];
        std::fs::write(&path, &original).unwrap();
        if std::fs::OpenOptions::new().read(true).custom_flags(libc::O_DIRECT).open(&path).is_err() {
            return;
        }
        let file = std::fs::File::open(&path).unwrap();
        std::fs::rename(&path, dir.path().join("moved")).unwrap();
        std::fs::write(&path, vec![0x97u8; original.len()]).unwrap();
        let args = crate::Args::for_test_plain(vec![path.clone()]);
        crate::linux_pipe::set_force_miss(true);
        let got = crate::hash_open_file(&args, file).unwrap();
        crate::linux_pipe::set_force_miss(false);
        assert_eq!(got, blake3::convenience::hash(&original), "hash the open file, not a replacement at its old name");
    }

    /// b3sum's one-file path reads a cold file of DIRECT_MIN or more
    /// directly, and no other: with every file taken for cold (the engine's
    /// seam), a file of DIRECT_MIN goes this way and one a byte shorter
    /// does not, and both hash as their bytes do.
    #[test]
    fn the_one_file_path_reads_a_cold_long_file_directly() {
        blake3::initialize();
        let dir = crate::tempfile::tempdir().unwrap();
        let all: Vec<u8> = (0..DIRECT_MIN as usize + 4096).map(|i: usize| (i.wrapping_mul(41) ^ (i >> 9)) as u8).collect();
        let probe = dir.path().join("probe");
        std::fs::write(&probe, &all[..4096]).unwrap();
        if std::fs::OpenOptions::new().read(true).custom_flags(libc::O_DIRECT).open(&probe).is_err() || crate::threads().get() == 1 {
            return;
        }
        crate::linux_pipe::set_force_miss(true);
        for (len, direct) in [(DIRECT_MIN as usize - 1, 0), (DIRECT_MIN as usize, 1), (all.len(), 1)] {
            let path = dir.path().join(format!("file-{len}"));
            std::fs::write(&path, &all[..len]).unwrap();
            let args = crate::Args::for_test_plain(vec![path.clone()]);
            HASHED.with(|h| h.set(0));
            assert_eq!(
                crate::hash_path_hash(&args, &path, &crate::file_end::StdinContext::current(), None).unwrap(),
                blake3::convenience::hash(&all[..len]),
                "len={len}"
            );
            assert_eq!(HASHED.with(|h| h.get()), direct, "len={len}");
        }
        crate::linux_pipe::set_force_miss(false);
    }
}
