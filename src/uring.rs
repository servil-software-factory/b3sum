//! Minimal Linux `io_uring` wrapper over `libc` (`SYS_io_uring_setup`,
//! `SYS_io_uring_register` for `IORING_OP_READ` capability probing,
//! `SYS_io_uring_enter`, and the ring mappings). Zero external dependencies.

use core::cell::Cell;
use core::ptr::NonNull;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::ptr;
use std::sync::atomic::{AtomicU32, Ordering};

const IORING_OP_READ: u8 = 22;
const IORING_REGISTER_PROBE: libc::c_uint = 8;
const IO_URING_OP_SUPPORTED: u16 = 1 << 0;
const PROBE_OPS: usize = 32;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct IoUringProbeOp {
    op: u8,
    resv: u8,
    flags: u16,
    resv2: u32,
}

#[repr(C)]
struct IoUringProbe {
    last_op: u8,
    ops_len: u8,
    resv: u16,
    resv2: [u32; 3],
    ops: [IoUringProbeOp; PROBE_OPS],
}

const IORING_OFF_SQ_RING: i64 = 0;
const IORING_OFF_CQ_RING: i64 = 0x800_0000;
const IORING_OFF_SQES: i64 = 0x1000_0000;

const IORING_FEAT_SINGLE_MMAP: u32 = 1 << 0;
const IORING_ENTER_GETEVENTS: u32 = 1 << 0;

const IORING_SETUP_CQSIZE: u32 = 1 << 3;
const IORING_SETUP_COOP_TASKRUN: u32 = 1 << 8;
const IORING_SETUP_SINGLE_ISSUER: u32 = 1 << 12;
const IORING_SETUP_DEFER_TASKRUN: u32 = 1 << 13;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct IoSqrOffsets {
    head: u32,
    tail: u32,
    ring_mask: u32,
    ring_entries: u32,
    flags: u32,
    dropped: u32,
    array: u32,
    resv1: u32,
    user_addr: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct IoCqrOffsets {
    head: u32,
    tail: u32,
    ring_mask: u32,
    ring_entries: u32,
    overflow: u32,
    cqes: u32,
    flags: u32,
    resv1: u32,
    user_addr: u64,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct IoUringParams {
    sq_entries: u32,
    cq_entries: u32,
    flags: u32,
    sq_thread_cpu: u32,
    sq_thread_idle: u32,
    features: u32,
    wq_fd: u32,
    resv: [u32; 3],
    sq_off: IoSqrOffsets,
    cq_off: IoCqrOffsets,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct IoUringSqe {
    opcode: u8,
    flags: u8,
    ioprio: u16,
    fd: i32,
    off: u64,
    addr: u64,
    len: u32,
    rw_flags: u32,
    user_data: u64,
    buf_group: u16,
    personality: u16,
    splice_fd_in: i32,
    pad2: [u64; 2],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct IoUringCqe {
    pub user_data: u64,
    pub res: i32,
    pub flags: u32,
}

// Compile-time verification of the hand-written Linux io_uring UAPI layouts (M7).
const _: () = {
    assert!(size_of::<IoSqrOffsets>() == 40);
    assert!(size_of::<IoCqrOffsets>() == 40);
    assert!(size_of::<IoUringParams>() == 120);
    assert!(size_of::<IoUringSqe>() == 64);
    assert!(size_of::<IoUringCqe>() == 16);
    assert!(core::mem::offset_of!(IoUringSqe, off) == 8);
    assert!(core::mem::offset_of!(IoUringSqe, addr) == 16);
    assert!(core::mem::offset_of!(IoUringSqe, len) == 24);
    assert!(core::mem::offset_of!(IoUringSqe, user_data) == 32);
    assert!(core::mem::offset_of!(IoUringCqe, res) == 8);
};

/// Owning guard for one `io_uring` `mmap` region: guarantees a non-null,
/// non-`MAP_FAILED` base address and unmaps once on drop (M7).
struct RingMap {
    ptr: NonNull<libc::c_void>,
    len: usize,
}

impl RingMap {
    fn map(fd: &OwnedFd, len: usize, offset: libc::off_t) -> io::Result<Self> {
        assert!(len > 0, "io_uring mapping length must be positive");
        // Sound: mapping `len` bytes of the owned `io_uring` descriptor at the
        // kernel-defined ring offset; kernel chooses the address.
        let raw = unsafe { libc::mmap(ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd.as_raw_fd(), offset) };
        if raw == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let Some(ptr) = NonNull::new(raw) else {
            // Sound: `raw` was just returned as a valid mapping of `len` bytes.
            unsafe { libc::munmap(raw, len) };
            return Err(io::Error::new(io::ErrorKind::InvalidData, "mmap returned null address"));
        };
        Ok(Self { ptr, len })
    }

    #[inline]
    fn as_ptr(&self) -> *mut libc::c_void {
        self.ptr.as_ptr()
    }
}

impl Drop for RingMap {
    fn drop(&mut self) {
        // Sound: `self.ptr` and `self.len` came from a successful `mmap` in
        // `RingMap::map`, unmapped once here.
        unsafe { libc::munmap(self.ptr.as_ptr(), self.len) };
    }
}

pub(crate) struct Uring {
    // Field drop order (after `Drop::drop` verifies `in_flight == 0`):
    // unmap SQEs, optional separate CQ ring, SQ ring, then close `fd`.
    _sqes_map: RingMap,
    _cq_map: Option<RingMap>,
    _sq_map: RingMap,
    fd: OwnedFd,
    ring_flags: u32,

    sqes_ptr: *mut IoUringSqe,
    sq_head: *const AtomicU32,
    sq_tail: *const AtomicU32,
    sq_mask: u32,
    sq_entries: u32,
    local_sq_tail: u32,
    unsubmitted: u32,

    cq_head: *const AtomicU32,
    cq_tail: *const AtomicU32,
    cq_mask: u32,
    cqes: *const IoUringCqe,
    /// Reads queued and not yet reaped: the kernel may still write their
    /// buffers. The ring is not dropped or set aside while there are any.
    in_flight: u32,
}

// `Uring` intentionally does NOT implement `Send` or `Sync`: its raw pointers
// keep it `!Send + !Sync`, enforcing `IORING_SETUP_SINGLE_ISSUER` thread
// affinity at compile time.

thread_local! {
    /// Per-thread cached `Uring`: `IORING_SETUP_SINGLE_ISSUER` binds each ring
    /// to the `tid` that created it, so reuse stays strictly thread-local.
    static LOCAL_URING: Cell<Option<Uring>> = const { Cell::new(None) };
}

impl Uring {
    /// Acquire this thread's cached `Uring` with at least `entries` SQEs, or create one.
    pub(crate) fn acquire(entries: u32) -> io::Result<Self> {
        if let Some(ring) = LOCAL_URING.with(|c| c.take()) {
            if ring.sq_entries >= entries {
                return Ok(ring);
            }
            drop(ring);
        }
        Self::new(entries)
    }

    /// Cache an idle `Uring` on the thread that created it so subsequent
    /// `--check` batches on this thread reuse it without `io_uring_setup`.
    pub(crate) fn release(self) {
        assert_eq!(self.in_flight, 0, "a ring is set aside with no read in flight");
        LOCAL_URING.with(|c| {
            c.set(Some(self));
        });
    }

    pub(crate) fn new(entries: u32) -> io::Result<Self> {
        #[cfg(test)]
        if REFUSED.load(Ordering::Relaxed) {
            return Err(io::Error::from_raw_os_error(libc::ENOSYS));
        }
        let try_flags = [
            IORING_SETUP_CQSIZE | IORING_SETUP_COOP_TASKRUN | IORING_SETUP_SINGLE_ISSUER | IORING_SETUP_DEFER_TASKRUN,
            IORING_SETUP_CQSIZE | IORING_SETUP_COOP_TASKRUN,
            0,
        ];
        let mut params = IoUringParams::default();
        let mut ring_fd: Option<OwnedFd> = None;
        let mut chosen_flags = 0u32;
        let mut last_err = io::Error::from_raw_os_error(libc::ENOSYS);

        for &flags in &try_flags {
            params = IoUringParams { cq_entries: if flags & IORING_SETUP_CQSIZE != 0 { entries * 2 } else { 0 }, flags, ..IoUringParams::default() };
            // Sound: kernel initializes `params` and returns a new cloexec fd or -1.
            let rc = unsafe { libc::syscall(libc::SYS_io_uring_setup, entries, &mut params as *mut IoUringParams) };
            if rc >= 0 {
                let raw_fd = RawFd::try_from(rc).map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "io_uring_setup fd out of range"))?;
                // Sound: `raw_fd` is a freshly created `io_uring` fd owned solely here.
                ring_fd = Some(unsafe { OwnedFd::from_raw_fd(raw_fd) });
                chosen_flags = flags;
                break;
            }
            last_err = io::Error::last_os_error();
        }
        let fd = match ring_fd {
            Some(fd) => fd,
            None => return Err(last_err),
        };

        // Verify the kernel actually supports `IORING_OP_READ` (introduced in
        // Linux 5.6 alongside `IORING_REGISTER_PROBE`, whereas `io_uring_setup`
        // dates to 5.1) before selecting the ring backend.
        if !probe_op_read(fd.as_raw_fd()) {
            return Err(io::Error::from_raw_os_error(libc::EOPNOTSUPP));
        }

        let sq_size = params.sq_off.array as usize + (params.sq_entries as usize) * size_of::<u32>();
        let cq_size = params.cq_off.cqes as usize + (params.cq_entries as usize) * size_of::<IoUringCqe>();
        let single_mmap = (params.features & IORING_FEAT_SINGLE_MMAP) != 0;
        let sq_ring_len = if single_mmap { sq_size.max(cq_size) } else { sq_size };

        let sq_map = RingMap::map(&fd, sq_ring_len, IORING_OFF_SQ_RING)?;
        let cq_map = if single_mmap { None } else { Some(RingMap::map(&fd, cq_size, IORING_OFF_CQ_RING)?) };
        let sqes_len = (params.sq_entries as usize) * size_of::<IoUringSqe>();
        let sqes_map = RingMap::map(&fd, sqes_len, IORING_OFF_SQES)?;

        let sq_base = sq_map.as_ptr().cast::<u8>();
        let cq_base = cq_map.as_ref().unwrap_or(&sq_map).as_ptr().cast::<u8>();
        let sqes_ptr = sqes_map.as_ptr().cast::<IoUringSqe>();

        // Sound: each offset is one the kernel gave for this ring (`params`),
        // inside the mapping of the length it also gave; the heads and tails
        // are u32s the kernel reads and writes atomically, as AtomicU32 does,
        // and the SQ indirection array has `params.sq_entries` u32 slots
        // initialized once to the identity permutation `0..sq_entries` (L4).
        let (sq_head, sq_tail, sq_mask, cq_head, cq_tail, cq_mask, cqes, local_sq_tail) = unsafe {
            let sq_head = sq_base.add(params.sq_off.head as usize).cast::<AtomicU32>();
            let sq_tail = sq_base.add(params.sq_off.tail as usize).cast::<AtomicU32>();
            let sq_mask = *sq_base.add(params.sq_off.ring_mask as usize).cast::<u32>();
            let sq_array = sq_base.add(params.sq_off.array as usize).cast::<u32>();
            for i in 0..params.sq_entries {
                *sq_array.add(i as usize) = i;
            }
            let cq_head = cq_base.add(params.cq_off.head as usize).cast::<AtomicU32>();
            let cq_tail = cq_base.add(params.cq_off.tail as usize).cast::<AtomicU32>();
            let cq_mask = *cq_base.add(params.cq_off.ring_mask as usize).cast::<u32>();
            let cqes = cq_base.add(params.cq_off.cqes as usize).cast::<IoUringCqe>();
            let local_sq_tail = (*sq_tail).load(Ordering::Relaxed);
            (sq_head, sq_tail, sq_mask, cq_head, cq_tail, cq_mask, cqes, local_sq_tail)
        };

        Ok(Self {
            _sqes_map: sqes_map,
            _cq_map: cq_map,
            _sq_map: sq_map,
            fd,
            ring_flags: chosen_flags,
            sqes_ptr,
            sq_head,
            sq_tail,
            sq_mask,
            sq_entries: params.sq_entries,
            local_sq_tail,
            unsubmitted: 0,
            cq_head,
            cq_tail,
            cq_mask,
            cqes,
            in_flight: 0,
        })
    }

    #[inline]
    pub(crate) fn sq_space_left(&self) -> u32 {
        // Sound: `sq_head` points into the ring's mapping, which lives as long
        // as the Uring.
        let head = unsafe { (*self.sq_head).load(Ordering::Acquire) };
        self.sq_entries - (self.local_sq_tail.wrapping_sub(head))
    }

    #[inline]
    pub(crate) fn unsubmitted(&self) -> u32 {
        self.unsubmitted
    }

    /// Queue an `IORING_OP_READ` SQE for a buffer with `room` bytes left:
    /// it asks for all of them, up to [`READ_MAX`] ([`read_len`]). Caller
    /// must ensure `buf[..room]` stays valid and exclusively writable by
    /// the kernel until the matching CQE is reaped.
    #[inline]
    pub(crate) unsafe fn push_read(&mut self, fd: RawFd, buf: *mut u8, room: usize, offset: u64, user_data: u64) -> bool {
        if self.sq_space_left() == 0 {
            return false;
        }
        let len = read_len(room);
        let idx = (self.local_sq_tail & self.sq_mask) as usize;
        // Sound: `idx` is below `sq_entries` (masked), so the write is inside
        // the ring's SQE array (the SQ indirection array was initialized once
        // to the identity permutation in `Uring::new`), and the kernel reads
        // an SQE only after `sq_tail` advances in `submit_and_wait`.
        unsafe {
            *self.sqes_ptr.add(idx) = IoUringSqe {
                opcode: IORING_OP_READ,
                flags: 0,
                ioprio: 0,
                fd,
                off: offset,
                addr: buf as usize as u64,
                len,
                rw_flags: 0,
                user_data,
                buf_group: 0,
                personality: 0,
                splice_fd_in: 0,
                pad2: [0; 2],
            };
        }
        self.local_sq_tail = self.local_sq_tail.wrapping_add(1);
        self.unsubmitted += 1;
        self.in_flight += 1;
        true
    }

    /// Submit all queued SQEs and wait for at least `min_complete` CQEs.
    /// A failed enter stops the process: some reads may already have been
    /// submitted, so unwinding could free buffers the kernel still writes.
    /// A read's error CQE, in contrast, has finished using that read's buffer.
    pub(crate) fn submit_and_wait(&mut self, min_complete: u32) {
        assert!(min_complete <= self.in_flight, "submit_and_wait cannot wait for {min_complete} completions with only {} reads in flight", self.in_flight);
        if self.unsubmitted > 0 {
            // Sound: into the ring's mapping, as sq_space_left's.
            unsafe { (*self.sq_tail).store(self.local_sq_tail, Ordering::Release) };
        }
        let to_submit = self.unsubmitted;
        self.unsubmitted = 0;

        let need_getevents = min_complete > 0 || (self.ring_flags & IORING_SETUP_DEFER_TASKRUN) != 0;
        if to_submit == 0 && min_complete == 0 && !need_getevents {
            return;
        }
        let enter_flags = if need_getevents { IORING_ENTER_GETEVENTS } else { 0 };
        // Sound: `self.sq_head` points into this ring's live SQ mapping.
        if let Err(reason) = run_submit_loop(
            to_submit,
            min_complete,
            self.local_sq_tail,
            || unsafe { (*self.sq_head).load(Ordering::Acquire) },
            |rem| enter(self.fd.as_raw_fd(), rem, min_complete, enter_flags),
        ) {
            abort_submit_progress(reason);
        }
    }

    #[inline]
    pub(crate) fn pop_cqe(&mut self) -> Option<IoUringCqe> {
        // Sound: into the ring's mapping. Only this thread moves the head.
        let head = unsafe { (*self.cq_head).load(Ordering::Relaxed) };
        let tail = unsafe { (*self.cq_tail).load(Ordering::Acquire) };
        if head == tail {
            return None;
        }
        let idx = (head & self.cq_mask) as usize;
        // Sound: `idx` is masked to the CQEs' count, the kernel finished
        // writing the CQE before advancing `cq_tail`, and only this thread
        // advances `cq_head` inside the live CQ mapping.
        let cqe = unsafe {
            let cqe = *self.cqes.add(idx);
            (*self.cq_head).store(head.wrapping_add(1), Ordering::Release);
            cqe
        };
        self.in_flight -= 1;
        #[cfg(test)]
        let cqe = spoiled(cqe);
        Some(cqe)
    }
}

/// Drive the `io_uring_enter` submission loop until all queued SQEs are
/// submitted (or a wait-only enter completes), enforcing forward progress on
/// every iteration (M31).
pub(crate) fn run_submit_loop(
    mut to_submit: u32,
    min_complete: u32,
    local_sq_tail: u32,
    mut read_sq_head: impl FnMut() -> u32,
    mut do_enter: impl FnMut(u32) -> Option<u32>,
) -> Result<(), &'static str> {
    loop {
        let sq_head_before = read_sq_head();
        if let Some(submitted) = do_enter(to_submit) {
            if to_submit == 0 {
                if submitted != 0 {
                    return Err("io_uring_enter reported submissions with none queued");
                }
                return Ok(());
            }
            let sq_head_after = read_sq_head();
            match reconcile_submit_progress(to_submit, submitted, sq_head_before, sq_head_after, local_sq_tail) {
                Ok(0) => return Ok(()),
                Ok(remaining) => to_submit = remaining,
                Err(reason) => return Err(reason),
            }
        } else {
            // Interrupted: the kernel may have consumed some SQEs.
            let head = read_sq_head();
            to_submit = local_sq_tail.wrapping_sub(head);
            if min_complete == 0 && to_submit == 0 {
                return Ok(());
            }
        }
    }
}

/// Reconcile remaining SQEs after a non-negative `io_uring_enter` return,
/// rejecting contradictory over-counts and unexplained zero-progress loops
/// (M31).
pub(crate) fn reconcile_submit_progress(
    to_submit: u32,
    submitted: u32,
    sq_head_before: u32,
    sq_head_after: u32,
    local_sq_tail: u32,
) -> Result<u32, &'static str> {
    if submitted > to_submit {
        return Err("io_uring_enter reported more submissions than queued");
    }
    if to_submit == 0 {
        return Ok(0);
    }
    if submitted > 0 {
        return Ok(to_submit - submitted);
    }
    // `submitted == 0` with `to_submit > 0`: allow progress only if the kernel
    // visibly advanced `sq_head` toward `local_sq_tail` so remaining shrank.
    if sq_head_after != sq_head_before {
        let remaining = local_sq_tail.wrapping_sub(sq_head_after);
        if remaining < to_submit {
            return Ok(remaining);
        }
    }
    Err("io_uring_enter made zero submission progress with pending SQEs")
}

#[cold]
fn abort_submit_progress(reason: &str) -> ! {
    crate::file_end::write_fatal_fmt(format_args!("b3sum: {reason}; stopping before pending reads' buffers can be released\n"));
    std::process::abort()
}

/// The most one read is asked for: less than the 4 GiB that a request's
/// 32-bit length can say.
pub(crate) const READ_MAX: usize = 1 << 30;

/// The length a read is asked for, where its buffer has `room` bytes
/// left: all of them, up to [`READ_MAX`]. A buffer of gigabytes (a file
/// that reported a small size and has more, or that grew) can have 4 GiB
/// of room and more, and a request's length is 32 bits: the room cast to
/// them asked for its low bits alone, so for exactly 4 GiB of room it
/// asked for no byte, the read gave none, and no byte is how a file's end
/// is known. The hash printed was then of the file's first 4 GiB. So the
/// room comes here as it is, and no caller narrows it. A request is for
/// at least one byte, in every build: nothing else may end a file.
fn read_len(room: usize) -> u32 {
    assert!(room > 0, "a read is asked for at least one byte");
    u32::try_from(room.min(READ_MAX)).expect("clamped to READ_MAX")
}

/// Probe whether `ring_fd` supports `IORING_OP_READ`.
/// `IoUringProbe.ops_len` MUST be 0 on input (the kernel validates it is
/// zeroed and writes the returned op count there; capacity is passed via `nr_args`).
fn probe_op_read(ring_fd: RawFd) -> bool {
    #[cfg(test)]
    if REFUSE_OP_READ.load(Ordering::Relaxed) {
        return false;
    }
    let mut probe = IoUringProbe { last_op: 0, ops_len: 0, resv: 0, resv2: [0; 3], ops: [IoUringProbeOp::default(); PROBE_OPS] };
    // Sound: `probe` is zeroed on input with `PROBE_OPS` trailing
    // `io_uring_probe_op` entries matching the `nr_args` count passed.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_io_uring_register,
            ring_fd,
            IORING_REGISTER_PROBE,
            (&raw mut probe).cast::<libc::c_void>(),
            libc::c_uint::try_from(PROBE_OPS).expect("PROBE_OPS fits c_uint"),
        )
    };
    if rc < 0 {
        return false;
    }
    (probe.ops_len as usize) > (IORING_OP_READ as usize)
        && (probe.last_op as usize) >= (IORING_OP_READ as usize)
        && (probe.ops[IORING_OP_READ as usize].flags & IO_URING_OP_SUPPORTED) != 0
}

/// For the tests: no ring can be made, as where a sandbox refuses
/// io_uring.
#[cfg(test)]
pub(crate) static REFUSED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// For the tests: `io_uring_setup` succeeds, but `IORING_OP_READ` is reported
/// unsupported (as on Linux 5.1..5.5 or restricted rings).
#[cfg(test)]
pub(crate) static REFUSE_OP_READ: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// For the tests: what becomes of the completions [`Uring::pop_cqe`]
/// hands out. 1: every other read that gave more than one byte gave half
/// as many (a short read; the bytes past them are in the buffer and are
/// not to be used). 2: every other read was interrupted. The reads'
/// callers must come to the same bytes either way. [`SPOILED`] counts the
/// completions so changed.
#[cfg(test)]
pub(crate) static SPOIL: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
#[cfg(test)]
pub(crate) static SPOILED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
fn spoiled(mut cqe: IoUringCqe) -> IoUringCqe {
    static SEEN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let this_one = || SEEN.fetch_add(1, Ordering::Relaxed) % 2 == 0;
    let before = cqe.res;
    match SPOIL.load(Ordering::Relaxed) {
        1 if cqe.res > 1 && this_one() => cqe.res /= 2,
        2 if cqe.res >= 0 && this_one() => cqe.res = -libc::EINTR,
        _ => {}
    }
    if cqe.res != before {
        SPOILED.fetch_add(1, Ordering::Relaxed);
    }
    cqe
}

/// None means interrupted; every other error is fatal. Keep this decision
/// at the syscall, before any caller can release an in-flight buffer.
#[inline]
fn enter(fd: RawFd, to_submit: u32, min_complete: u32, flags: u32) -> Option<u32> {
    // Sound: a system call with plain values; no signal mask is passed.
    let rc = unsafe { libc::syscall(libc::SYS_io_uring_enter, fd, to_submit, min_complete, flags, ptr::null::<libc::sigset_t>(), size_of::<libc::sigset_t>()) };
    if rc >= 0 {
        return Some(u32::try_from(rc).expect("non-negative enter count fits u32"));
    }
    let err = io::Error::last_os_error();
    if err.kind() == io::ErrorKind::Interrupted {
        return None;
    }
    let errno = err.raw_os_error().unwrap_or(0);
    crate::file_end::write_fatal_fmt(format_args!("b3sum: io_uring_enter failed (errno {errno}); stopping before pending reads' buffers can be released\n"));
    std::process::abort()
}

impl Drop for Uring {
    fn drop(&mut self) {
        // Closing a ring does not wait for its reads: the kernel may write
        // a queued read's buffer after this returns, and whoever drops the
        // ring frees those buffers next. So a ring dropped with a read in
        // flight (a return or a panic past it) stops the process here.
        if self.in_flight != 0 {
            crate::file_end::write_fatal_fmt(format_args!(
                "b3sum: an io_uring was dropped with {} reads in flight; stopping before their buffers can be released\n",
                self.in_flight
            ));
            std::process::abort()
        }
        // Owned mappings (`_sqes_map`, `_cq_map`, `_sq_map`) and `fd` drop
        // automatically in field order right after this guard returns.
    }
}

#[cfg(test)]
mod test {
    use super::Uring;
    use std::os::fd::AsRawFd;

    /// On a host where raw `io_uring_setup` succeeds and an independent raw
    /// `IORING_REGISTER_PROBE` reports `IORING_OP_READ` supported, `Uring::new`
    /// must succeed (so an always-false `probe_op_read` fails this test!), while
    /// `REFUSE_OP_READ` causes `Uring::new` to fail with `EOPNOTSUPP` (L10).
    #[test]
    fn uring_probe_accepts_op_read_and_refuse_seam_rejects_it() {
        let mut params = super::IoUringParams::default();
        let raw_fd = unsafe { libc::syscall(libc::SYS_io_uring_setup, 8u32, &raw mut params) };
        if raw_fd < 0 {
            return;
        }
        // Independent kernel probe (not calling production `probe_op_read`).
        let mut probe = super::IoUringProbe { last_op: 0, ops_len: 0, resv: 0, resv2: [0; 3], ops: [super::IoUringProbeOp::default(); super::PROBE_OPS] };
        let rc = unsafe {
            libc::syscall(
                libc::SYS_io_uring_register,
                raw_fd as std::os::fd::RawFd,
                super::IORING_REGISTER_PROBE,
                (&raw mut probe).cast::<libc::c_void>(),
                super::PROBE_OPS as libc::c_uint,
            )
        };
        let op_read_ok = rc == 0
            && (probe.ops_len as usize) > (super::IORING_OP_READ as usize)
            && (probe.last_op as usize) >= (super::IORING_OP_READ as usize)
            && (probe.ops[super::IORING_OP_READ as usize].flags & super::IO_URING_OP_SUPPORTED) != 0;
        unsafe { libc::close(raw_fd as std::os::fd::RawFd) };
        if !op_read_ok {
            // Older Linux 5.1..5.5 or restricted rings: production probe must decline.
            assert_eq!(Uring::new(8).err().and_then(|e| e.raw_os_error()), Some(libc::EOPNOTSUPP));
            return;
        }
        assert!(Uring::new(8).is_ok(), "probe must accept IORING_OP_READ on a capable kernel");
        super::REFUSE_OP_READ.store(true, std::sync::atomic::Ordering::Relaxed);
        let err = Uring::new(8).err().expect("REFUSE_OP_READ must reject ring setup");
        super::REFUSE_OP_READ.store(false, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(err.raw_os_error(), Some(libc::EOPNOTSUPP));
    }

    /// A ring's read is asked for at least one byte and at most what its
    /// 32-bit length can say, whatever room the buffer has: at 4 GiB of
    /// room and at its multiples the cast length was 0, which reads like
    /// the file's end.
    #[test]
    fn a_rings_read_asks_for_bytes_whatever_the_room() {
        for spare in [1, 4096, super::READ_MAX - 1, super::READ_MAX, super::READ_MAX + 1, (1 << 32) - 1, 1 << 32, (1 << 32) + 1, 1 << 33, 3 << 32, usize::MAX] {
            let len = super::read_len(spare) as usize;
            assert!(0 < len && len <= spare && len <= super::READ_MAX, "{spare}: {len}");
            assert_eq!(len, spare.min(super::READ_MAX), "{spare}");
        }
    }

    /// Through a ring: a read whose buffer has 4 GiB of room gets the
    /// file's bytes. (The room is address space alone, and the file has
    /// 100 bytes; where the system will not lend 4 GiB of addresses, this
    /// says nothing.)
    #[test]
    fn a_read_with_four_gib_of_room_gets_the_files_bytes() {
        let Ok(mut ring) = Uring::new(8) else { return };
        let mut buffer: Vec<u8> = Vec::new();
        if buffer.try_reserve_exact(1 << 32).is_err() {
            eprintln!("no 4 GiB of address space to try this with");
            return;
        }
        let dir = crate::tempfile::tempdir().unwrap();
        let path = dir.path().join("f");
        std::fs::write(&path, [7u8; 100]).unwrap();
        let file = std::fs::File::open(&path).unwrap();
        // Sound: the buffer has the 4 GiB of room said, and is not touched
        // until the read's completion is taken, below.
        assert!(unsafe { ring.push_read(file.as_raw_fd(), buffer.as_mut_ptr(), 1 << 32, 0, 5) });
        ring.submit_and_wait(1);
        let cqe = ring.pop_cqe().unwrap();
        assert_eq!((cqe.user_data, cqe.res), (5, 100));
    }

    /// A ring dropped while a read is in flight stops the process: the
    /// read here is of a pipe no one writes, so it stays in flight.
    #[test]
    fn a_ring_dropped_with_a_read_in_flight_stops_the_process() {
        use std::os::unix::process::ExitStatusExt;
        const CHILD: &str = "B3SUM_TEST_RING_DROPPED";
        if std::env::var_os(CHILD).is_some() {
            let Ok(mut ring) = Uring::new(8) else { std::process::exit(77) };
            let mut ends = [0i32; 2];
            assert_eq!(unsafe { libc::pipe(ends.as_mut_ptr()) }, 0);
            let mut buffer = [0u8; 64];
            assert!(unsafe { ring.push_read(ends[0], buffer.as_mut_ptr(), 64, 0, 1) });
            ring.submit_and_wait(0);
            drop(ring);
            std::process::exit(0);
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "uring::test::a_ring_dropped_with_a_read_in_flight_stops_the_process", "--nocapture"])
            .env(CHILD, "1")
            .output()
            .unwrap();
        if output.status.code() == Some(77) {
            eprintln!("no io_uring here");
            return;
        }
        assert_eq!(output.status.signal(), Some(libc::SIGABRT), "{:?}: {:?}", output.status, output.stderr);
        assert!(String::from_utf8_lossy(&output.stderr).contains("dropped with 1 reads in flight"));
    }

    /// Fatal abort must not block even when fd 2 is a completely full,
    /// undrained pipe whose reader is still open.
    #[test]
    fn failed_enter_aborts_even_with_full_undrained_stderr_pipe() {
        use std::os::fd::FromRawFd;
        use std::os::unix::process::ExitStatusExt;
        const CHILD: &str = "B3SUM_TEST_FAILED_ENTER_FULL_STDERR";
        if std::env::var_os(CHILD).is_some() {
            super::enter(-1, 1, 0, 0);
            std::process::exit(0);
        }
        let mut ends = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(ends.as_mut_ptr()) }, 0);
        // Set write end non-blocking solely to stuff the pipe completely full,
        // then restore blocking mode before handing it to the child as stderr.
        let flags = unsafe { libc::fcntl(ends[1], libc::F_GETFL) };
        assert!(flags >= 0);
        assert_eq!(unsafe { libc::fcntl(ends[1], libc::F_SETFL, flags | libc::O_NONBLOCK) }, 0);
        let chunk = [b'x'; 4096];
        loop {
            let n = unsafe { libc::write(ends[1], chunk.as_ptr().cast(), chunk.len()) };
            if n <= 0 {
                break;
            }
        }
        assert_eq!(unsafe { libc::fcntl(ends[1], libc::F_SETFL, flags) }, 0);
        let stderr_file = unsafe { std::fs::File::from_raw_fd(ends[1]) };
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "uring::test::failed_enter_aborts_even_with_full_undrained_stderr_pipe", "--nocapture"])
            .env(CHILD, "1")
            .stderr(std::process::Stdio::from(stderr_file))
            .spawn()
            .unwrap();
        // Hold the read end open without ever reading from it; child must abort
        // promptly (8 s parent deadline documented in `.config/nextest.toml`).
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if std::time::Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                unsafe { libc::close(ends[0]) };
                panic!("child hung on full undrained stderr pipe during io_uring_enter abort");
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        unsafe { libc::close(ends[0]) };
        assert_eq!(status.signal(), Some(libc::SIGABRT), "expected immediate SIGABRT with full stderr, got {status:?}");
    }

    /// A real failed syscall must abort, even inside catch_unwind. This
    /// needs no io_uring permission: an invalid fd (or a denied syscall)
    /// deterministically takes the production error path.
    #[test]
    fn failed_enter_stops_before_unwinding() {
        use std::os::unix::process::ExitStatusExt;
        const CHILD: &str = "B3SUM_TEST_FAILED_ENTER";
        if std::env::var_os(CHILD).is_some() {
            let _ = std::panic::catch_unwind(|| super::enter(-1, 1, 0, 0));
            std::process::exit(0);
        }
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "uring::test::failed_enter_stops_before_unwinding", "--nocapture"])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert_eq!(output.status.signal(), Some(libc::SIGABRT), "{:?}: {:?}", output.status, output.stderr);
        assert!(String::from_utf8_lossy(&output.stderr).contains("stopping before pending reads' buffers can be released"));
    }

    #[test]
    fn uring_reads_offsets_saturation_and_error_cqes() {
        blake3::initialize();
        let Ok(mut ring) = Uring::new(8) else { return };
        assert!(ring.pop_cqe().is_none());
        assert_eq!(ring.unsubmitted(), 0);
        let cap = ring.sq_space_left();
        assert!(cap >= 8);

        let dir = crate::tempfile::tempdir().unwrap();
        let p = dir.path().join("f0");
        let data: Vec<u8> = (0..10_000usize).map(|k| (k * 13 + 7) as u8).collect();
        std::fs::write(&p, &data).unwrap();
        let f = std::fs::File::open(&p).unwrap();

        // 1. Non-zero offset read + saturation up to `cap` (`push_read` returns false when full!)
        let mut bufs: Vec<Vec<u8>> = (0..cap as usize).map(|_| vec![0u8; 256]).collect();
        for (i, b) in bufs.iter_mut().enumerate() {
            let off = (i * 111) as u64;
            assert!(unsafe { ring.push_read(f.as_raw_fd(), b.as_mut_ptr(), 256, off, i as u64) });
            assert_eq!(ring.unsubmitted(), (i + 1) as u32);
        }
        assert_eq!(ring.sq_space_left(), 0);
        let mut extra = [0u8; 16];
        assert!(!unsafe { ring.push_read(f.as_raw_fd(), extra.as_mut_ptr(), 16, 0, 999) }, "ring must report full");

        ring.submit_and_wait(cap);
        assert_eq!(ring.unsubmitted(), 0);
        assert_eq!(ring.sq_space_left(), cap);

        let mut seen = vec![false; cap as usize];
        while let Some(cqe) = ring.pop_cqe() {
            let i = cqe.user_data as usize;
            assert_eq!(cqe.res, 256);
            let off = i * 111;
            assert_eq!(&bufs[i][..], &data[off..off + 256]);
            seen[i] = true;
        }
        assert!(seen.into_iter().all(|x| x));
        assert!(ring.pop_cqe().is_none());

        // 2. Fault injection: invalid fd `-1` produces `-EBADF` CQE without corrupting ring state,
        // and `acquire` / `release` recycles the idle ring cleanly.
        assert!(unsafe { ring.push_read(-1, extra.as_mut_ptr(), 16, 0, 0xDEAD_BEEF) });
        ring.submit_and_wait(1);
        let err_cqe = ring.pop_cqe().expect("error CQE");
        assert_eq!(err_cqe.user_data, 0xDEAD_BEEF);
        assert_eq!(err_cqe.res, -libc::EBADF);
        ring.submit_and_wait(0);

        let saved_fd = ring.fd.as_raw_fd();
        ring.release();
        let reused = Uring::acquire(8).unwrap();
        assert_eq!(reused.fd.as_raw_fd(), saved_fd);
    }

    #[test]
    fn reconcile_submit_progress_rejects_overcount_and_zero_progress() {
        // Normal full & partial progress through the actual production submit loop.
        assert_eq!(
            super::run_submit_loop(
                0,
                1,
                10,
                || 10,
                |rem| {
                    assert_eq!(rem, 0);
                    Some(0)
                }
            ),
            Ok(())
        );
        let calls = core::cell::Cell::new(0u32);
        assert_eq!(
            super::run_submit_loop(
                4,
                0,
                14,
                || 10 + calls.get(),
                |rem| {
                    let c = calls.get() + 1;
                    calls.set(c);
                    if c == 1 {
                        assert_eq!(rem, 4);
                        Some(1)
                    } else {
                        assert_eq!(rem, 3);
                        Some(3)
                    }
                },
            ),
            Ok(())
        );
        // Zero return allowed ONLY when kernel visibly advanced sq_head.
        let head = core::cell::Cell::new(10u32);
        let mut step = 0u32;
        assert_eq!(
            super::run_submit_loop(
                4,
                0,
                14,
                || head.get(),
                |_| {
                    step += 1;
                    if step == 1 {
                        head.set(12);
                        Some(0)
                    } else {
                        head.set(14);
                        Some(2)
                    }
                },
            ),
            Ok(())
        );
        // Wrap-around sq_head advancement works cleanly.
        assert_eq!(super::reconcile_submit_progress(4, 0, u32::MAX - 1, 1, 2), Ok(1));
        // Over-count rejected by the loop rather than silently saturating to 0.
        assert!(super::run_submit_loop(2, 0, 12, || 10, |_| Some(3)).is_err());
        assert!(super::run_submit_loop(0, 1, 10, || 10, |_| Some(1)).is_err());
        // Zero progress with pending SQEs and unmoved sq_head rejected immediately
        // instead of looping forever.
        let mut stall_calls = 0u32;
        assert!(
            super::run_submit_loop(
                2,
                0,
                12,
                || 10,
                |_| {
                    stall_calls += 1;
                    assert!(stall_calls <= 2, "submit loop must fail stop instead of spinning");
                    Some(0)
                },
            )
            .is_err()
        );
        // Even if sq_head changed to something not shrinking remaining, reject.
        assert!(super::reconcile_submit_progress(2, 0, 10, 9, 12).is_err());
    }
}
