use std::path::Path;

#[test]
fn test_parse_check_line() {
    blake3::initialize();
    // =========================
    // ===== Success Cases =====
    // =========================

    // the basic case
    let parsed = crate::parse_check_line("0909090909090909090909090909090909090909090909090909090909090909  foo").unwrap();
    assert_eq!(parsed.expected_hash, blake3::Hash::from([0x09; 32]));
    assert!(parsed.escaped_display.is_none());
    assert_eq!(parsed.display_str(), "foo");
    assert_eq!(parsed.file_path, Path::new("foo"));

    // regular whitespace
    let parsed = crate::parse_check_line("fafafafafafafafafafafafafafafafafafafafafafafafafafafafafafafafa   \t\r\n\n\r \t\r\n\n\r").unwrap();
    assert_eq!(parsed.expected_hash, blake3::Hash::from([0xfa; 32]));
    assert!(parsed.escaped_display.is_none());
    assert_eq!(parsed.display_str(), " \t\r\n\n\r \t");
    assert_eq!(parsed.file_path, Path::new(" \t\r\n\n\r \t"));

    // path is one space
    let parsed = crate::parse_check_line("4242424242424242424242424242424242424242424242424242424242424242   ").unwrap();
    assert_eq!(parsed.expected_hash, blake3::Hash::from([0x42; 32]));
    assert!(parsed.escaped_display.is_none());
    assert_eq!(parsed.display_str(), " ");
    assert_eq!(parsed.file_path, Path::new(" "));

    // *Unescaped* backslashes. Note that this line does *not* start with a
    // backslash, so something like "\" + "n" is interpreted as *two*
    // characters.
    let parsed = crate::parse_check_line("4343434343434343434343434343434343434343434343434343434343434343  fo\\a\\no").unwrap();
    assert_eq!(parsed.expected_hash, blake3::Hash::from([0x43; 32]));
    assert!(parsed.escaped_display.is_none());
    assert_eq!(parsed.display_str(), "fo\\a\\no");
    assert_eq!(parsed.file_path, Path::new("fo\\a\\no"));

    // escaped newlines
    let parsed = crate::parse_check_line("\\4444444444444444444444444444444444444444444444444444444444444444  fo\\r\\n\\n\\ro").unwrap();
    assert_eq!(parsed.expected_hash, blake3::Hash::from([0x44; 32]));
    assert_eq!(parsed.escaped_display.as_deref(), Some("\\fo\\r\\n\\n\\ro"));
    assert_eq!(parsed.display_str(), "\\fo\\r\\n\\n\\ro");
    assert_eq!(parsed.file_path, Path::new("fo\r\n\n\ro"));

    // Escaped newline and backslash.
    let parsed = crate::parse_check_line("\\4545454545454545454545454545454545454545454545454545454545454545  fo\\n\\\\o").unwrap();
    assert_eq!(parsed.expected_hash, blake3::Hash::from([0x45; 32]));
    assert_eq!(parsed.escaped_display.as_deref(), Some("\\fo\\n\\\\o"));
    assert_eq!(parsed.display_str(), "\\fo\\n\\\\o");
    assert_eq!(parsed.file_path, Path::new("fo\n\\o"));

    // non-ASCII path
    let parsed = crate::parse_check_line("4646464646464646464646464646464646464646464646464646464646464646  否认").unwrap();
    assert_eq!(parsed.expected_hash, blake3::Hash::from([0x46; 32]));
    assert!(parsed.escaped_display.is_none());
    assert_eq!(parsed.display_str(), "否认");
    assert_eq!(parsed.file_path, Path::new("否认"));

    // untagged separator "  " in the file name
    let parsed = crate::parse_check_line("4747474747474747474747474747474747474747474747474747474747474747  foo  bar").unwrap();
    assert_eq!(parsed.expected_hash, blake3::Hash::from([0x47; 32]));
    assert!(parsed.escaped_display.is_none());
    assert_eq!(parsed.display_str(), "foo  bar");
    assert_eq!(parsed.file_path, Path::new("foo  bar"));

    // tagged separator ") = " in the file name
    let parsed = crate::parse_check_line("BLAKE3 (foo) = bar) = 4848484848484848484848484848484848484848484848484848484848484848").unwrap();
    assert_eq!(parsed.expected_hash, blake3::Hash::from([0x48; 32]));
    assert!(parsed.escaped_display.is_none());
    assert_eq!(parsed.display_str(), "foo) = bar");
    assert_eq!(parsed.file_path, Path::new("foo) = bar"));

    // =========================
    // ===== Failure Cases =====
    // =========================

    // too short
    crate::parse_check_line("").unwrap_err();
    crate::parse_check_line("0").unwrap_err();
    crate::parse_check_line("00").unwrap_err();
    crate::parse_check_line("0000000000000000000000000000000000000000000000000000000000000000").unwrap_err();
    crate::parse_check_line("0000000000000000000000000000000000000000000000000000000000000000  ").unwrap_err();

    // not enough spaces
    crate::parse_check_line("0000000000000000000000000000000000000000000000000000000000000000 foo").unwrap_err();

    // capital letter hex
    crate::parse_check_line("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA  foo").unwrap_err();

    // non-hex hex
    crate::parse_check_line("xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx  foo").unwrap_err();

    // non-ASCII hex
    crate::parse_check_line("你好, 我叫杰克. 认识你很高兴. 要不要吃个香蕉?  foo").unwrap_err();

    // 64 UTF-8 bytes ending in a 2-byte scalar (63 chars): must return Err, never panic
    let utf8_tail = format!("{}é", "0".repeat(62));
    assert_eq!(utf8_tail.len(), 2 * blake3::OUT_LEN);
    crate::parse_check_line(&format!("{utf8_tail}  foo")).unwrap_err();
    crate::parse_check_line(&format!("BLAKE3 (foo) = {utf8_tail}")).unwrap_err();

    // invalid escape sequence
    crate::parse_check_line("\\0000000000000000000000000000000000000000000000000000000000000000  fo\\o").unwrap_err();

    // truncated escape sequence
    crate::parse_check_line("\\0000000000000000000000000000000000000000000000000000000000000000  foo\\").unwrap_err();

    // null char
    crate::parse_check_line("0000000000000000000000000000000000000000000000000000000000000000  fo\0o").unwrap_err();

    // Unicode replacement char
    crate::parse_check_line("0000000000000000000000000000000000000000000000000000000000000000  fo�o").unwrap_err();
}

#[test]
fn test_filepath_to_string() {
    blake3::initialize();
    let output = crate::filepath_to_string(Path::new("foo"));
    assert_eq!(output.filepath_string, "foo");
    assert!(matches!(output.filepath_string, std::borrow::Cow::Borrowed("foo")));
    assert!(!output.is_escaped);

    let output = crate::filepath_to_string(Path::new("f\\ \t\r\noo"));
    assert_eq!(output.filepath_string, "f\\\\ \t\\r\\noo");
    assert!(output.is_escaped);
}

/// A file is read to where its reads end, whatever length its size said:
/// the same bytes when it has grown since, when it has shrunk, and when
/// the size was right, each from a reader that gives a few bytes a read.
#[test]
fn test_a_files_size_does_not_decide_where_it_ends() {
    let dir = crate::tempfile::tempdir().unwrap();
    let all: Vec<u8> = (0..1000u32).map(|i| (i * 7) as u8).collect();
    for len in [0usize, 1, 999, 1000] {
        let path = dir.path().join(format!("file-{len}.bin"));
        std::fs::write(&path, &all[..len]).unwrap();
        for says in [0, 1, len.saturating_sub(1), len, len + 1, 5000] {
            // Fresh Vec each iteration so `says < len` genuinely starts with
            // capacity `says + 1` and exercises buffer growth to EOF (#424).
            let mut bytes = Vec::new();
            let mut file = std::fs::File::open(&path).unwrap();
            let eof = crate::file_end::read_sequential_capped_seeded(&mut file, says, 2000, &mut bytes).unwrap();
            assert!(eof, "len={len} <= cap 2000 must reach EOF (says={says})");
            assert_eq!(bytes, &all[..len], "len={len} says={says}");
        }
    }
    let mut bytes = vec![0xEE; 5];
    // When actual input exceeds `cap`, `read_sequential_capped_seeded` stops at
    // `cap` (`Ok(false)`) and leaves the unread tail in `file` for streaming; at
    // `cap == 0`, returns `Ok(false)` without consuming any byte.
    let path = dir.path().join("full.bin");
    std::fs::write(&path, &all).unwrap();
    let mut file = std::fs::File::open(&path).unwrap();
    assert!(!crate::file_end::read_sequential_capped_seeded(&mut file, 10, 0, &mut bytes).unwrap());
    assert!(bytes.is_empty());
    let eof = crate::file_end::read_sequential_capped_seeded(&mut file, 10, 256, &mut bytes).unwrap();
    assert!(!eof, "input longer than cap must promote to streaming");
    assert_eq!(bytes, &all[..256]);
    let mut tail = Vec::new();
    std::io::Read::read_to_end(&mut file, &mut tail).unwrap();
    assert_eq!(tail, &all[256..]);
}

/// `--no-mmap` maps no file, on each path that maps one without it: the
/// one-file hash, the extended output, the many-file run and the
/// outboard. The same hashes either way. (BLAKE3 #432: the option was
/// accepted and did nothing.)
#[cfg(target_os = "linux")]
#[test]
fn test_no_mmap_maps_no_file() {
    blake3::initialize();
    let dir = crate::tempfile::tempdir().unwrap();
    let bytes: Vec<u8> = (0..crate::MAP_LEN + 4096).map(|i| (i * 31) as u8).collect();
    let paths: Vec<_> = (0..2).map(|i| dir.path().join(format!("file-{i}"))).collect();
    for path in &paths {
        std::fs::write(path, &bytes).unwrap();
    }
    let want = blake3::convenience::hash(&bytes);
    let stdin_ctx = crate::file_end::StdinContext::current();
    let run = |args: &crate::Args| {
        use std::sync::atomic::Ordering;
        let before = crate::MAPPED.load(Ordering::Relaxed);
        assert_eq!(crate::hash_open_file(args, std::fs::File::open(&paths[0]).unwrap()).unwrap(), want);
        let one = crate::MAPPED.load(Ordering::Relaxed);
        let mut out = [0u8; 32];
        crate::hash_path(args, &paths[0], &stdin_ctx).unwrap().fill(&mut out);
        assert_eq!(out, *want.as_bytes());
        let xof = crate::MAPPED.load(Ordering::Relaxed);
        crate::linux_pipe::hash_paths(args, &paths, None, &mut |_, hash| {
            assert_eq!(hash.unwrap(), want);
            true
        });
        let many = crate::MAPPED.load(Ordering::Relaxed);
        let (mut rooms, mut nodes) = (crate::outboard::Rooms::new(), Vec::new());
        assert_eq!(crate::outboard_of(args, &paths[0], &stdin_ctx, 1, &mut rooms, &mut nodes).unwrap(), want);
        let outboard = crate::MAPPED.load(Ordering::Relaxed);
        [one - before, xof - one, many - xof, outboard - many]
    };
    let mut args = crate::Args::for_test_plain(paths.clone());
    let mapped = run(&args);
    assert!(mapped.iter().all(|&n| n >= 1), "without the option each path maps: {mapped:?}");
    args.no_mmap = true;
    assert_eq!(run(&args), [0; 4]);
}

/// One thread, and no ring to be had (a sandbox that refuses io_uring): a
/// cold file of POOL_START_LEN or more is read in pieces. It was read
/// whole into memory, as a shorter file still is, so a file larger than
/// the memory the process may have ended it.
#[test]
#[cfg(target_os = "linux")]
fn test_a_cold_long_file_on_one_thread_without_a_ring_is_read_in_pieces() {
    blake3::initialize();
    crate::THREADS.set(std::num::NonZeroUsize::MIN).expect("this test's process sets the thread count first");
    crate::uring::REFUSED.store(true, std::sync::atomic::Ordering::Relaxed);
    crate::linux_pipe::set_force_miss(true);
    let dir = crate::tempfile::tempdir().unwrap();
    let bytes: Vec<u8> = (0..crate::POOL_START_LEN + 12_345).map(|i| (i.wrapping_mul(37) ^ (i >> 10)) as u8).collect();
    for (len, in_pieces) in [(crate::POOL_START_LEN - 1, 0), (crate::POOL_START_LEN, 1), (bytes.len(), 1)] {
        let path = dir.path().join(format!("file-{len}"));
        std::fs::write(&path, &bytes[..len]).unwrap();
        let args = crate::Args::for_test_plain(vec![path.clone()]);
        let before = crate::STREAMED.with(|streamed| streamed.get());
        assert_eq!(crate::hash_open_file(&args, std::fs::File::open(&path).unwrap()).unwrap(), blake3::convenience::hash(&bytes[..len]), "len={len}");
        assert_eq!(crate::STREAMED.with(|streamed| streamed.get()) - before, in_pieces, "len={len}");
    }
}

#[test]
fn write_all_with_stops_on_write_zero_and_reports_partial_progress() {
    let mut calls = 0usize;
    let (written, res) = crate::write_all_with(b"abcdef", |_| {
        calls += 1;
        Ok(0)
    });
    assert_eq!(calls, 1);
    assert_eq!(written, 0);
    assert_eq!(res.unwrap_err().kind(), std::io::ErrorKind::WriteZero);

    // Partial write followed by EAGAIN reports exact bytes already delivered.
    let mut step = 0usize;
    let (written, res) = crate::write_all_with(b"abcdef", |chunk| {
        step += 1;
        match step {
            1 => Ok(2.min(chunk.len())),
            2 => Err(std::io::Error::from(std::io::ErrorKind::Interrupted)),
            3 => Ok(1.min(chunk.len())),
            _ => Err(std::io::Error::from(std::io::ErrorKind::WouldBlock)),
        }
    });
    assert_eq!(written, 3);
    assert_eq!(res.unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
}

#[test]
fn read_check_line_bounded_caps_memory_and_isolates_bad_utf8() {
    let mut input = Vec::new();
    input.extend_from_slice(b"line1\n");
    input.extend_from_slice(&vec![b'a'; 200]);
    input.push(b'\n');
    input.extend_from_slice(b"bad-\xff-utf8\n");
    input.extend_from_slice(b"line4\n");
    let mut reader = std::io::BufReader::with_capacity(16, &input[..]);
    let mut raw = Vec::new();

    assert_eq!(crate::read_check_line_bounded(&mut reader, &mut raw, 64).unwrap().unwrap().unwrap(), "line1\n");
    let err_long = crate::read_check_line_bounded(&mut reader, &mut raw, 64).unwrap().unwrap().unwrap_err();
    assert!(err_long.to_string().contains("maximum length"), "{err_long}");
    assert!(raw.len() <= 64, "must not allocate past max_bytes");
    let err_utf8 = crate::read_check_line_bounded(&mut reader, &mut raw, 64).unwrap().unwrap().unwrap_err();
    assert!(err_utf8.to_string().contains("Invalid UTF-8"), "{err_utf8}");
    assert_eq!(crate::read_check_line_bounded(&mut reader, &mut raw, 64).unwrap().unwrap().unwrap(), "line4\n");
    assert!(crate::read_check_line_bounded(&mut reader, &mut raw, 64).unwrap().is_none());
}

#[test]
fn cap_requested_threads_bounds_extreme_values() {
    let capped = crate::cap_requested_threads(std::num::NonZeroUsize::new(usize::MAX).unwrap());
    assert!(capped.get() >= 1 && capped.get() <= crate::MAX_THREADS);
    let one = crate::cap_requested_threads(std::num::NonZeroUsize::MIN);
    assert_eq!(one.get(), 1);
}

#[test]
fn read_check_line_bounded_retries_interrupted() {
    use std::io::Read;
    struct InterruptOnce<'a> {
        inner: &'a [u8],
        interrupted: bool,
    }
    impl Read for InterruptOnce<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if !self.interrupted {
                self.interrupted = true;
                return Err(std::io::Error::from(std::io::ErrorKind::Interrupted));
            }
            self.inner.read(buf)
        }
    }
    let mut reader = std::io::BufReader::new(InterruptOnce { inner: b"ok-after-eintr\n", interrupted: false });
    let mut raw = Vec::new();
    let line = crate::read_check_line_bounded(&mut reader, &mut raw, 64).unwrap().unwrap().unwrap();
    assert_eq!(line, "ok-after-eintr\n");
}

#[test]
fn direct_permits_enforce_shared_budget() {
    let total = crate::threads().get();
    if total < 3 {
        return;
    }
    let file_threads = total - 2;
    let _guard = crate::FileThreadsGuard::enter(file_threads);
    let a = crate::DirectPermits::acquire(10);
    assert_eq!(a.extra, 2, "first lease takes only the shared spare permits");
    let b = crate::DirectPermits::acquire(10);
    assert_eq!(b.extra, 0, "concurrent caller gets 0 extra while spare permits are leased");
    drop(a);
    let c = crate::DirectPermits::acquire(1);
    assert_eq!(c.extra, 1, "permits return to the shared pool on drop");
}

#[test]
fn direct_permits_subtract_existing_pool_workers_and_pool_threads_respects_leases() {
    let total = crate::threads().get();
    if total < 4 {
        return;
    }
    // Save/restore simulated pool_threads around this accounting unit test.
    let prev_pool = {
        let mut b = crate::lock_budget();
        core::mem::replace(&mut b.pool_threads, 3)
    };
    let _guard = crate::FileThreadsGuard::enter(1);
    // total budget T, file_threads = 1, pool_background = 2 -> spare = T - 3.
    let expected_spare = total.saturating_sub(3);
    let leased = crate::DirectPermits::acquire(total);
    assert_eq!(leased.extra, expected_spare, "direct permits must subtract existing pool background workers");
    // While `leased` holds `expected_spare` permits, pool_threads() must account for them.
    let p_while_leased = crate::pool_threads().get();
    assert!(
        1 + leased.extra + p_while_leased.saturating_sub(1) <= total,
        "file_workers(1) + leased_direct({}) + pool_bg({}) must not exceed total({})",
        leased.extra,
        p_while_leased.saturating_sub(1),
        total
    );
    drop(leased);
    crate::lock_budget().pool_threads = prev_pool;
}

#[cfg(target_os = "linux")]
#[test]
fn cloning_ptmx_is_not_classified_as_stream_alias() {
    use std::os::fd::FromRawFd;
    // Open a fresh PTY master via posix_openpt (same dev/ino as /dev/ptmx).
    // Sound: standard POSIX call returning a new fd or -1.
    let fd = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY) };
    if fd < 0 {
        return;
    }
    // Sound: `fd` is a newly opened owned descriptor.
    let _owned = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
    let meta = crate::file_end::fstat_fd(fd).expect("fstat PTY master");
    assert_eq!(meta.kind, crate::file_end::FileKind::Other, "cloning PTY master must not be classified as FileKind::Stream");
    assert!(!crate::file_end::path_matches_stream(std::path::Path::new("/dev/ptmx"), meta.id), "/dev/ptmx must never match as a reopenable stream alias");
}

#[test]
fn record_oriented_sockets_and_packet_pipes_are_rejected() {
    blake3::initialize();
    use std::os::fd::FromRawFd;
    // SOCK_STREAM must be accepted (assert socketpair succeeds on supported Unix targets).
    let mut sv = [-1 as libc::c_int; 2];
    // Sound: `sv` is a valid 2-element stack array.
    assert_eq!(unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, sv.as_mut_ptr()) }, 0, "socketpair(AF_UNIX, SOCK_STREAM) failed");
    {
        let _a = unsafe { std::os::fd::OwnedFd::from_raw_fd(sv[0]) };
        let _b = unsafe { std::os::fd::OwnedFd::from_raw_fd(sv[1]) };
        let meta = crate::file_end::fstat_fd(sv[0]).unwrap();
        assert!(crate::file_end::ensure_byte_stream_fd(sv[0], meta.mode_type).is_ok());
    }
    // SOCK_DGRAM (and on Linux SOCK_SEQPACKET) must be rejected cleanly.
    let mut dv = [-1 as libc::c_int; 2];
    assert_eq!(unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, dv.as_mut_ptr()) }, 0, "socketpair(AF_UNIX, SOCK_DGRAM) failed");
    {
        let _a = unsafe { std::os::fd::OwnedFd::from_raw_fd(dv[0]) };
        let _b = unsafe { std::os::fd::OwnedFd::from_raw_fd(dv[1]) };
        let meta = crate::file_end::fstat_fd(dv[0]).unwrap();
        assert!(crate::file_end::ensure_byte_stream_fd(dv[0], meta.mode_type).is_err());
    }
    #[cfg(target_os = "linux")]
    {
        let mut qv = [-1 as libc::c_int; 2];
        assert_eq!(unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_SEQPACKET, 0, qv.as_mut_ptr()) }, 0, "socketpair(AF_UNIX, SOCK_SEQPACKET) failed");
        let _a = unsafe { std::os::fd::OwnedFd::from_raw_fd(qv[0]) };
        let _b = unsafe { std::os::fd::OwnedFd::from_raw_fd(qv[1]) };
        let meta = crate::file_end::fstat_fd(qv[0]).unwrap();
        assert!(crate::file_end::ensure_byte_stream_fd(qv[0], meta.mode_type).is_err());
        // Linux packet-mode pipe (`pipe2(O_DIRECT)`): a full 4096-byte packet must
        // be read completely by streaming `outboard_from_reader` (not truncated at 32 bytes).
        let mut pv = [-1 as libc::c_int; 2];
        if unsafe { libc::pipe2(pv.as_mut_ptr(), libc::O_DIRECT) } == 0 {
            let mut r = unsafe { std::fs::File::from_raw_fd(pv[0]) };
            let mut w = unsafe { std::fs::File::from_raw_fd(pv[1]) };
            let payload: Vec<u8> = (0..4096usize).map(|i| (i & 0xff) as u8).collect();
            use std::io::Write;
            w.write_all(&payload).unwrap();
            drop(w);
            let mut cvs = Vec::new();
            let mut out = Vec::new();
            let got = crate::outboard::outboard_from_reader(blake3::Mode::Plain, &mut r, &mut cvs, &mut out).unwrap();
            assert_eq!(got, blake3::convenience::hash(&payload), "outboard_from_reader must preserve full PIPE_BUF packet-mode pipe writes");
            assert!(out.is_empty(), "4096-byte payload is <= 1 chunk group");
        }
        // Packet-mode pipe across a 4 MiB piece boundary (`fill()`): 1 byte +
        // 1024 x 4096-byte packets must never truncate the final byte.
        let mut pv2 = [-1 as libc::c_int; 2];
        if unsafe { libc::pipe2(pv2.as_mut_ptr(), libc::O_DIRECT) } == 0 {
            let r = unsafe { std::fs::File::from_raw_fd(pv2[0]) };
            let mut w = unsafe { std::fs::File::from_raw_fd(pv2[1]) };
            let writer = std::thread::spawn(move || {
                use std::io::Write;
                w.write_all(&[0x5Au8]).unwrap();
                let pkt = [0xA5u8; 4096];
                for _ in 0..1024 {
                    w.write_all(&pkt).unwrap();
                }
            });
            let mut hasher = {
                blake3::initialize();
                blake3::convenience::Hasher::new(blake3::Mode::Plain, None)
            };
            crate::update_from(&mut hasher, r).unwrap();
            writer.join().unwrap();
            let mut expected = blake3::convenience::Hasher::new(blake3::Mode::Plain, None);
            expected.update(&[0x5Au8]);
            expected.update(&vec![0xA5u8; 1024 * 4096]);
            assert_eq!(hasher.finalize(), expected.finalize(), "fill() must preserve full 4 KiB packets across piece boundaries");
        }
        // `hash_open_file_ex` must also reject an owned SOCK_DGRAM socket.
        let mut dv2 = [-1 as libc::c_int; 2];
        if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_DGRAM, 0, dv2.as_mut_ptr()) } == 0 {
            let f = unsafe { std::fs::File::from_raw_fd(dv2[0]) };
            let _peer = unsafe { std::os::fd::OwnedFd::from_raw_fd(dv2[1]) };
            let args = crate::Args::for_test_plain(Vec::new());
            assert!(crate::hash_open_file_ex(&args, f).is_err(), "hash_open_file_ex must reject SOCK_DGRAM");
        }
    }
    // `fill` must never issue a read request smaller than `min_packet_read_len()`
    // once at least one byte is buffered (protects 16 KiB / 64 KiB page Linux kernels).
    {
        struct MinAskReader {
            min_len: usize,
            calls: usize,
        }
        impl std::io::Read for MinAskReader {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                assert!(buf.len() >= self.min_len, "fill issued short read of {} < {}", buf.len(), self.min_len);
                self.calls += 1;
                if self.calls == 1 {
                    buf[0] = 0x42;
                    Ok(1)
                } else {
                    Ok(0)
                }
            }
        }
        let min_len = crate::min_packet_read_len();
        let mut buf = vec![0u8; min_len];
        let mut r = MinAskReader { min_len, calls: 0 };
        let (n, eof) = crate::fill(&mut r, &mut buf).unwrap();
        assert_eq!((n, eof), (1, false));
        assert_eq!(r.calls, 1);
    }
    // On macOS, `/dev/fd/<N>` for a nonzero regular descriptor positioned past
    // offset 0 must hash the full file from offset 0 without moving the fd's cursor.
    #[cfg(target_os = "macos")]
    {
        use std::io::{Seek, SeekFrom};
        use std::os::fd::AsRawFd;
        let dir = crate::tempfile::tempdir().unwrap();
        let path = dir.path().join("abc.txt");
        std::fs::write(&path, b"abc").unwrap();
        let mut f = std::fs::File::open(&path).unwrap();
        f.seek(SeekFrom::Start(1)).unwrap();
        let fd_path = std::path::PathBuf::from(format!("/dev/fd/{}", f.as_raw_fd()));
        let stdin_ctx = crate::file_end::StdinContext::current();
        assert_eq!(stdin_ctx.classify(&fd_path), crate::file_end::InputRole::DarwinRegularDevFd);
        let args = crate::Args::for_test_plain(Vec::new());
        let got = crate::hash_path_hash(&args, &fd_path, &stdin_ctx, None).unwrap();
        blake3::initialize();
        assert_eq!(got, blake3::convenience::hash(b"abc"), "/dev/fd/N must hash full regular file from offset 0");
        assert_eq!(f.stream_position().unwrap(), 1, "/dev/fd/N must not advance the underlying fd cursor");
    }
}

#[test]
fn single_small_file_skips_thread_discovery_and_numa_confinement() {
    blake3::initialize();
    let dir = crate::tempfile::tempdir().unwrap();
    let path = dir.path().join("small_16k.bin");
    let data = vec![0x5Au8; 16 * 1024];
    std::fs::write(&path, &data).unwrap();
    let args = crate::Args::for_test_plain(vec![path]);
    let stdin_ctx = crate::file_end::StdinContext::current();
    crate::configure_threads(&args);
    let mut out = crate::OutBuf::new();
    let failed = crate::hash_inputs(&args, &stdin_ctx, &mut out);
    assert_eq!(failed, 0);
    assert!(crate::THREADS.get().is_none(), "single 16 KiB file must not trigger thread discovery (#469)");
    #[cfg(target_os = "linux")]
    assert_eq!(crate::CONFINEMENT_CALLS.load(std::sync::atomic::Ordering::Relaxed), 0, "single 16 KiB file must not invoke NUMA confinement (#469)");
    assert!(crate::threads().get() >= 1);
    #[cfg(target_os = "linux")]
    assert_eq!(crate::CONFINEMENT_CALLS.load(std::sync::atomic::Ordering::Relaxed), 1);
}

#[test]
fn multi_file_path_triggers_numa_confinement_once() {
    blake3::initialize();
    let dir = crate::tempfile::tempdir().unwrap();
    let p1 = dir.path().join("a.bin");
    let p2 = dir.path().join("b.bin");
    std::fs::write(&p1, b"alpha").unwrap();
    std::fs::write(&p2, b"beta").unwrap();
    let args = crate::Args::for_test_plain(vec![p1, p2]);
    let stdin_ctx = crate::file_end::StdinContext::current();
    crate::configure_threads(&args);
    let mut out = crate::OutBuf::new();
    let failed = crate::hash_inputs(&args, &stdin_ctx, &mut out);
    assert_eq!(failed, 0);
    assert!(crate::THREADS.get().is_some());
    #[cfg(target_os = "linux")]
    assert_eq!(
        crate::CONFINEMENT_CALLS.load(std::sync::atomic::Ordering::Relaxed),
        1,
        "multi-file hashing must confine to one NUMA node once before spawning workers"
    );
}

#[test]
fn num_threads_one_skips_confinement() {
    blake3::initialize();
    let args = crate::Args::for_test_plain_threads(Vec::new(), 1);
    crate::configure_threads(&args);
    assert_eq!(crate::threads().get(), 1);
    #[cfg(target_os = "linux")]
    assert_eq!(crate::CONFINEMENT_CALLS.load(std::sync::atomic::Ordering::Relaxed), 0);
}

#[test]
fn num_threads_two_confines_eagerly() {
    blake3::initialize();
    let args = crate::Args::for_test_plain_threads(Vec::new(), 2);
    crate::configure_threads(&args);
    assert_eq!(crate::threads().get(), 2);
    #[cfg(target_os = "linux")]
    assert_eq!(crate::CONFINEMENT_CALLS.load(std::sync::atomic::Ordering::Relaxed), 1);
}

#[test]
fn validate_and_ensure_byte_stream_fd_fail_closed_on_os_error() {
    // failing `getsockopt(SO_TYPE)` on a socket descriptor must return an OS
    // error, never treat the failure as permission to hash the descriptor.
    let err = crate::file_end::ensure_byte_stream_fd(-1, libc::S_IFSOCK).unwrap_err();
    assert!(!err.to_string().is_empty());
    // Failing `fstat` must likewise fail closed.
    let err = crate::file_end::validate_byte_stream_fd(-1).unwrap_err();
    assert!(!err.to_string().is_empty());
}

#[test]
fn collect_check_batch_enforces_retained_byte_budget() {
    // a manifest with many long lines must flush once retained path bytes
    // reach `max_batch_bytes`, rather than buffering `max_entries` huge paths.
    let digest = "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262";
    let mut manifest = Vec::new();
    let count = 10usize;
    for i in 0..count {
        let name = format!("path_{i:02}_{}", "a".repeat(90));
        manifest.extend_from_slice(format!("{digest}  {name}\n").as_bytes());
    }
    let mut reader = std::io::BufReader::new(&manifest[..]);
    let mut raw = Vec::new();
    let mut pending = None;
    let mut entries = Vec::new();
    let mut batch_paths = Vec::new();
    let stdin_ctx = crate::file_end::StdinContext::current();
    let never_own = |_: &std::path::Path| false;

    let mut batches = 0usize;
    let mut total_entries = 0usize;
    let mut eof = false;
    while !eof {
        eof = crate::collect_check_batch(
            &mut reader,
            &mut raw,
            &mut pending,
            &mut entries,
            &mut batch_paths,
            100, // high entry count cap so byte budget decides
            256, // tight retained byte budget (strict cap: at most 2 * 98 = 196 <= 256 bytes!)
            1024,
            &stdin_ctx,
            &never_own,
        )
        .unwrap();
        if !entries.is_empty() {
            batches += 1;
            total_entries += entries.len();
            let batch_bytes: usize = batch_paths.iter().map(|p| p.as_os_str().len()).sum();
            assert!(batch_bytes <= 256, "batch retained {batch_bytes} bytes, exceeding strict 256-byte budget");
            assert!(batch_paths.len() <= 2, "strict 256-byte budget fits at most 2 98-byte paths, got {}", batch_paths.len());
        }
    }
    assert_eq!(total_entries, count);
    assert_eq!(batches, 5, "10 x 98-byte paths under 256-byte cap must yield 5 batches of 2");
}

#[test]
fn sigbus_exits_promptly_with_full_undrained_stderr_pipe_and_writes_when_writable() {
    // fatal SIGBUS termination must neither block on an undrained full
    // stderr pipe nor stall in `pause()`, while still emitting the explanation
    // when stderr is a writable file (and on Linux, a writable pipe/tty via
    // `/proc/self/fd/2` with independent `O_NONBLOCK`).
    use std::os::fd::FromRawFd;
    const CHILD: &str = "B3SUM_TEST_SIGBUS_HANDLER";
    if std::env::var_os(CHILD).is_some() {
        crate::explain_sigbus();
        unsafe { libc::raise(libc::SIGBUS) };
        std::process::exit(0);
    }

    // Subcase A: append-mode regular file (`2>>job.log`) preserves existing
    // bytes at offset 0 and appends the explanation on every OS.
    let dir = crate::tempfile::tempdir().unwrap();
    let err_path = dir.path().join("sigbus.err");
    std::fs::write(&err_path, b"EXISTING_LOG_PREFIX\n").unwrap();
    let err_file = std::fs::OpenOptions::new().append(true).open(&err_path).unwrap();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "unit_tests::sigbus_exits_promptly_with_full_undrained_stderr_pipe_and_writes_when_writable", "--nocapture"])
        .env(CHILD, "1")
        .stderr(std::process::Stdio::from(err_file))
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(1), "expected exit 1 on SIGBUS, got {status:?}");
    let err_text = std::fs::read_to_string(&err_path).unwrap();
    assert!(err_text.starts_with("EXISTING_LOG_PREFIX\n"), "fatal diagnostic must not overwrite existing log prefix, got {err_text:?}");
    assert!(err_text.contains("bus error while reading a mapped file"), "expected SIGBUS diagnostic appended to regular-file stderr, got {err_text:?}");

    // On Linux (when `/proc/self/fd/2` is reopenable for writing), a writable
    // pipe also receives the explanation via independent `O_NONBLOCK`.
    #[cfg(target_os = "linux")]
    if std::fs::OpenOptions::new().write(true).open("/proc/self/fd/2").is_ok() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "unit_tests::sigbus_exits_promptly_with_full_undrained_stderr_pipe_and_writes_when_writable", "--nocapture"])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(1), "expected exit 1 on SIGBUS pipe, got {:?}", output.status);
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("bus error while reading a mapped file"),
            "expected SIGBUS diagnostic on writable Linux pipe, got {:?}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Subcase B: completely stuffed, undrained stderr pipe whose reader stays
    // open must still exit 1 promptly (bounded 2s deadline so regressions fail fast).
    let mut ends = [0i32; 2];
    assert_eq!(unsafe { libc::pipe(ends.as_mut_ptr()) }, 0);
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
        .args(["--exact", "unit_tests::sigbus_exits_promptly_with_full_undrained_stderr_pipe_and_writes_when_writable", "--nocapture"])
        .env(CHILD, "1")
        .stderr(std::process::Stdio::from(stderr_file))
        .spawn()
        .unwrap();
    // 8 s parent deadline (documented in `.config/nextest.toml`) so the SIGBUS
    // planted-fault control fails cleanly instead of hitting nextest's 60 s cap.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            unsafe { libc::close(ends[0]) };
            panic!("child hung on full undrained stderr pipe during SIGBUS");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    unsafe { libc::close(ends[0]) };
    assert_eq!(status.code(), Some(1), "expected immediate exit 1 with full stderr, got {status:?}");
}

#[cfg(target_os = "linux")]
#[test]
fn hash_open_file_ex_streams_full_file_when_file_grows_during_direct_read() {
    // if `direct::hash` observes the file grew (positive final EOF probe),
    // `hash_open_file_ex` must discard its stale mmap and stream the descriptor
    // to actual EOF instead of returning the hash of the earlier prefix.
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    blake3::initialize();
    let dir = crate::tempfile::tempdir().unwrap();
    let path = dir.path().join("growing-direct.bin");
    let initial_len = 2 * crate::direct::PIECE;
    let initial: Vec<u8> = (0..initial_len).map(|i| (i.wrapping_mul(17) ^ (i >> 9)) as u8).collect();
    let suffix = b"GROWN_AFTER_DIRECT_READ_TAIL_BYTES";
    std::fs::write(&path, &initial).unwrap();
    if std::fs::OpenOptions::new().read(true).custom_flags(libc::O_DIRECT).open(&path).is_err() {
        eprintln!("no O_DIRECT in {}", dir.path().display());
        return;
    }

    let args = crate::Args::for_test_plain_threads(Vec::new(), 2);
    crate::configure_threads(&args);
    let file = std::fs::File::open(&path).unwrap();

    let grow_path = path.clone();
    crate::direct::BEFORE_EOF_PROBE.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move || {
            let mut f = std::fs::OpenOptions::new().append(true).open(&grow_path).unwrap();
            f.write_all(suffix).unwrap();
            f.flush().unwrap();
        }));
    });
    let before_direct = crate::direct::HASHED.with(std::cell::Cell::get);
    crate::linux_pipe::set_force_miss(true);
    crate::direct::TAKE_FOR_COLD.with(|c| c.set(true));
    let res = crate::hash_open_file_ex(&args, file);
    crate::direct::TAKE_FOR_COLD.with(|c| c.set(false));
    crate::linux_pipe::set_force_miss(false);
    crate::direct::BEFORE_EOF_PROBE.with(|slot| {
        slot.borrow_mut().take();
    });
    assert!(crate::direct::HASHED.with(std::cell::Cell::get) > before_direct, "test must actually enter direct::hash before growing the file");

    let hr = res.unwrap();
    let mut full = initial;
    full.extend_from_slice(suffix);
    let want = blake3::convenience::hash_with(blake3::Mode::Plain, &full);
    assert_eq!(hr.bytes_hashed, full.len() as u64, "must hash the grown file to real EOF, not stale mmap prefix");
    assert_eq!(hr.hash, want, "must return full grown digest, not stale mmap prefix");
}

#[cfg(target_os = "linux")]
#[test]
fn direct_read_workers_stop_claiming_pieces_after_error() {
    // once one worker records an error on piece 0 while 11 valid pieces
    // remain, peer workers must stop claiming remaining pieces instead of
    // reading the rest of the file (counts claims BEFORE `pread`).
    use std::os::unix::fs::OpenOptionsExt;
    use std::sync::atomic::Ordering;

    let dir = crate::tempfile::tempdir().unwrap();
    let path = dir.path().join("cancel-direct.bin");
    let pieces = 12usize;
    std::fs::write(&path, vec![0x5au8; pieces * crate::direct::PIECE]).unwrap();
    if std::fs::OpenOptions::new().read(true).custom_flags(libc::O_DIRECT).open(&path).is_err() {
        eprintln!("no O_DIRECT in {}", dir.path().display());
        return;
    }
    let file = std::fs::File::open(&path).unwrap();
    crate::direct::CLAIMED_PIECES.store(0, Ordering::SeqCst);
    crate::direct::INJECT_EIO_RECORDED.store(false, Ordering::SeqCst);
    crate::direct::INJECT_EIO_PIECE.store(0, Ordering::SeqCst);
    let mut outs = vec![0u8; pieces];
    let mut mine = crate::direct::Piece::new();
    let res = crate::direct::read(&file, pieces * crate::direct::PIECE, 2, &mut mine, &mut outs, 1, |_, _, _| {
        // Deterministically wait until piece 0's worker has recorded its error
        // before looping back to claim the next piece (no scheduler race).
        while !crate::direct::INJECT_EIO_RECORDED.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
    });
    crate::direct::INJECT_EIO_PIECE.store(usize::MAX, Ordering::SeqCst);
    assert!(matches!(res, Err(crate::direct::Failure::Err(ref e)) if e.raw_os_error() == Some(libc::EIO)));
    let claimed = crate::direct::CLAIMED_PIECES.load(Ordering::SeqCst);
    assert!(claimed <= 2, "at most piece 0 (failing) and one in-flight peer piece may be claimed, got {claimed} of {pieces}");
}

#[test]
fn checked_fd_offset_helpers_propagate_espipe_and_overflow() {
    use std::io::Read;
    use std::os::fd::{AsFd, FromRawFd};

    // Regular file: roundtrip offset query, set, and lifetime-bound PreadReader.
    let dir = crate::tempfile::tempdir().unwrap();
    let path = dir.path().join("seekable.bin");
    std::fs::write(&path, b"0123456789abcdef").unwrap();
    let f = std::fs::File::open(&path).unwrap();
    assert_eq!(crate::file_end::current_fd_offset(f.as_fd()).unwrap(), 0);
    crate::file_end::set_fd_offset(f.as_fd(), 6).unwrap();
    assert_eq!(crate::file_end::current_fd_offset(f.as_fd()).unwrap(), 6);
    let mut pr = crate::file_end::PreadReader::new(f.as_fd(), 4);
    let mut chunk = [0u8; 4];
    pr.read_exact(&mut chunk).unwrap();
    assert_eq!(&chunk, b"4567");
    // Positioned pread must not move the file description's cursor.
    assert_eq!(crate::file_end::current_fd_offset(f.as_fd()).unwrap(), 6);
    // Overflowing `off_t` (u64::MAX) must fail cleanly as InvalidInput.
    let err = crate::file_end::set_fd_offset(f.as_fd(), u64::MAX).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    let mut pr_over = crate::file_end::PreadReader::new(f.as_fd(), u64::MAX);
    let err2 = pr_over.read(&mut chunk).unwrap_err();
    assert_eq!(err2.kind(), std::io::ErrorKind::InvalidInput);

    // Non-seekable pipe: current_fd_offset and set_fd_offset must propagate ESPIPE
    // rather than substituting 0.
    let mut ends = [0i32; 2];
    assert_eq!(unsafe { libc::pipe(ends.as_mut_ptr()) }, 0);
    let r = unsafe { std::fs::File::from_raw_fd(ends[0]) };
    let w = unsafe { std::fs::File::from_raw_fd(ends[1]) };
    assert_eq!(crate::file_end::current_fd_offset(r.as_fd()).unwrap_err().raw_os_error(), Some(libc::ESPIPE));
    assert_eq!(crate::file_end::set_fd_offset(r.as_fd(), 0).unwrap_err().raw_os_error(), Some(libc::ESPIPE));
    drop((r, w));
}

#[test]
fn post_open_forbidden_stream_guard_rejects_swapped_descriptor() {
    // `ordinary` passes the pre-open `path_matches_stream` check, then
    // the injected open returns an open descriptor for the forbidden FIFO.
    // Removing only the post-open `fstat` guard in `open_checked_with` makes
    // this test fail.
    use std::os::fd::AsRawFd;
    let dir = crate::tempfile::tempdir().unwrap();
    let ordinary = dir.path().join("ordinary.txt");
    std::fs::write(&ordinary, b"harmless").unwrap();
    let fifo = dir.path().join("manifest.fifo");
    assert!(std::process::Command::new("mkfifo").arg(&fifo).status().unwrap().success());
    let _rw = std::fs::OpenOptions::new().read(true).write(true).open(&fifo).unwrap();
    let forbid = crate::file_end::fstat_fd(_rw.as_raw_fd()).unwrap().id;
    assert!(!crate::file_end::path_matches_stream(&ordinary, forbid), "pre-open path check must NOT match ordinary file");

    let err = crate::file_end::open_checked_with(&ordinary, Some(forbid), |_| std::fs::File::open(&fifo)).unwrap_err();
    assert!(err.to_string().contains("checkfile is a stream"), "post-open fstat guard must reject swapped stream, got {err}");
}

#[test]
fn run_ordered_collector_emits_in_order_and_stops_claiming_on_cancel() {
    use std::sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    // 1. Normal completion emits all indices in strict 0..N order.
    let mut got = Vec::new();
    crate::run_ordered_collector(
        32,
        4,
        |i| i * 3 + 1,
        |i, v| {
            got.push((i, v));
            true
        },
    );
    assert_eq!(got, (0..32).map(|i| (i, i * 3 + 1)).collect::<Vec<_>>());

    // 2. Cancellation on index 0 publishes `next >= count` strictly BEFORE
    // helpers are released from the gate (`after_cancel(claimed_before_close)`),
    // proving EVERY index executed by any thread was claimed strictly BEFORE
    // `next.fetch_max(count, SeqCst)` (`max_index_executed < claimed_before_close`)
    // — 100% scheduling-independent.
    let main_tid = std::thread::current().id();
    let arrived = Arc::new((Mutex::new(0usize), Condvar::new()));
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let timed_out = Arc::new(AtomicBool::new(false));
    let max_index_plus_one = AtomicUsize::new(0);
    let claimed_before_close = AtomicUsize::new(0);
    let mut emitted = Vec::new();
    crate::run_ordered_collector_inner(
        128,
        4,
        |i| {
            max_index_plus_one.fetch_max(i + 1, Ordering::SeqCst);
            if i > 0 && std::thread::current().id() != main_tid {
                let (lock, cvar) = &*arrived;
                *lock.lock().unwrap() += 1;
                cvar.notify_all();
                let (glock, gcvar) = &*gate;
                let g = glock.lock().unwrap();
                let (_g, res) = gcvar.wait_timeout_while(g, std::time::Duration::from_secs(5), |done| !*done).unwrap();
                if res.timed_out() {
                    timed_out.store(true, Ordering::SeqCst);
                }
            }
            i
        },
        |i, v| {
            emitted.push((i, v));
            let (lock, cvar) = &*arrived;
            let g = lock.lock().unwrap();
            let (_g, res) = cvar.wait_timeout_while(g, std::time::Duration::from_secs(5), |n| *n == 0).unwrap();
            assert!(!res.timed_out(), "at least one helper must reach the gate before cancelling");
            false
        },
        |prev_next| {
            // Runs strictly AFTER `next.fetch_max(count, SeqCst)` returns `prev_next`.
            claimed_before_close.store(prev_next, Ordering::SeqCst);
            let (glock, gcvar) = &*gate;
            *glock.lock().unwrap() = true;
            gcvar.notify_all();
        },
    );
    assert!(!timed_out.load(Ordering::SeqCst), "collector cancellation gate must not time out");
    assert_eq!(emitted, vec![(0, 0)]);
    let max_seen = max_index_plus_one.load(Ordering::SeqCst);
    let pre_close = claimed_before_close.load(Ordering::SeqCst);
    assert!(
        max_seen <= pre_close && pre_close < 128,
        "no index >= claimed_before_close may be claimed after cancel (max_seen={max_seen}, claimed_before_close={pre_close}; M3)"
    );
}

#[test]
fn single_named_file_uses_one_fstat_call() {
    blake3::initialize();
    let dir = crate::tempfile::tempdir().unwrap();
    let small = dir.path().join("small.bin");
    std::fs::write(&small, b"hello single fstat").unwrap();
    let args = crate::Args::for_test_plain_threads(vec![small.clone()], 1);
    let stdin_ctx = crate::file_end::StdinContext::current();

    let before = crate::file_end::FSTAT_CALLS.with(|c| c.get());
    let h = crate::hash_path_hash(&args, &small, &stdin_ctx, None).unwrap();
    let after = crate::file_end::FSTAT_CALLS.with(|c| c.get());
    assert_eq!(h, blake3::convenience::hash(b"hello single fstat"));
    assert_eq!(after - before, 1, "single named file must reuse open_checked's fstat without redundant fstat calls (M5)");
}

#[test]
fn streaming_small_input_does_not_allocate_second_buffer() {
    blake3::initialize();
    let second_allocated = std::thread::spawn(|| {
        let mut hasher = blake3::convenience::Hasher::new(blake3::Mode::Plain, None);
        let data = vec![0xABu8; 4096];
        let n = crate::update_from_counted(&mut hasher, std::io::Cursor::new(&data)).unwrap();
        assert_eq!(n, 4096);
        assert_eq!(hasher.finalize(), blake3::convenience::hash(&data));
        crate::BUFFERS.with_borrow(|b| b.second.is_some())
    })
    .join()
    .unwrap();
    assert!(!second_allocated, "streaming <= 1 piece must not eagerly allocate a second 4 MiB buffer (M11)");
}

#[test]
fn use_pool_for_respects_actual_pool_width_vs_prospective_budget() {
    // Under a fully occupied FileThreadsGuard, prospective `pool_threads()` is 1
    // so `use_pool_for` must not attempt to start a 1-thread singleton.
    let total = crate::threads().get();
    let _guard = crate::FileThreadsGuard::enter(total);
    assert_eq!(crate::pool_threads().get(), 1);
    if crate::actual_pool_threads() == 0 {
        assert!(!crate::use_pool_for(crate::POOL_START_LEN * 2), "must not initialize a 1-thread pool singleton");
        assert_eq!(crate::actual_pool_threads(), 0, "pool must remain uninitialized while width is 1");
    } else if crate::actual_pool_threads() == 1 {
        assert!(!crate::use_pool_for(crate::POOL_START_LEN * 2), "a 1-thread pool must never be used as multithreaded");
    }
}

#[cfg(target_os = "linux")]
#[test]
fn cold_ring_temporary_width_one_does_not_freeze_global_pool_at_one() {
    // Two >= 4 MiB cold files under `--num-threads 2` allocate 2 file readers
    // (`cold_pool_workers = 1`); the cold ring must fall back to local
    // streaming instead of freezing `ACTUAL_POOL_THREADS` at 1, leaving a
    // subsequent lone large file free to initialize the pool at width 2.
    blake3::initialize();
    let dir = crate::tempfile::tempdir().unwrap();
    let a = dir.path().join("a.bin");
    let b = dir.path().join("b.bin");
    let bytes = vec![0x42u8; 4 * 1024 * 1024 + 17];
    std::fs::write(&a, &bytes).unwrap();
    std::fs::write(&b, &bytes).unwrap();

    let files = vec![a, b];
    let args = crate::Args::for_test_plain_threads(files.clone(), 2);
    crate::linux_pipe::set_force_miss(true);
    let mut got = Vec::new();
    crate::linux_pipe::hash_paths(&args, &files, None, &mut |i, res| {
        got.push((i, res.unwrap()));
        true
    });
    crate::linux_pipe::set_force_miss(false);
    assert_eq!(got.len(), 2);
    let want = blake3::convenience::hash(&bytes);
    assert_eq!(got[0].1, want);
    assert_eq!(got[1].1, want);
    assert_ne!(crate::actual_pool_threads(), 1, "temporary width-1 cold-ring batch must not freeze global pool singleton at 1");
}
