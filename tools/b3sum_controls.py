#!/usr/bin/env python3
"""Each of b3sum's protections has a test that fails without it.

    python3 tools/b3sum_controls.py

A protection here is a check or a rule in b3sum's code that a bug once
lacked. For each, this stages a private temporary copy of the crate
outside the developer's checkout (never modifying the working tree in
place — M22), verifies the exact named test passes on the baseline,
applies the single planted fault to the staged copy, runs the exact
named test under `cargo nextest` with a bounded timeout, and requires
that exact test to fail. A protection whose test still passes, fails to
build, or hits a timeout/infrastructure error exits 1 naming it.

b3sum is outside the mutation gate (BLAKE3's tools/mutants), which is slow on it
(BLAKE3 #386); this is its bounded stand-in, for the protections named.
"""

import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
STAGE_ENTRIES = (
    "Cargo.toml",
    "Cargo.lock",
    "README.md",
    "build.rs",
    "build_version.rs",
    "src",
    "tests",
    "tools",
    ".config",
)
CONTROL_TIMEOUT_SECS = 180

READ_TO_END = """            let next_len = (bytes.len() * 2).max(4096).min(cap);
            bytes.try_reserve(next_len - bytes.len()).map_err(|_| io::Error::from(io::ErrorKind::OutOfMemory))?;
            bytes.resize(next_len, 0);"""
READ_TO_END_NOT = """            bytes.truncate(got);
            return Ok(true);"""

# (the protection, its file, the text to find once, what replaces it, exact nextest test name)
CONTROLS = [
    (
        "--no-mmap maps no file (#432)",
        "src/main.rs",
        "        if self.no_mmap {\n            return None;\n        }\n",
        "",
        "unit_tests::test_no_mmap_maps_no_file",
    ),
    (
        "a checkfile with no line is a failure (#407)",
        "src/main.rs",
        '    ensure!(saw_line, "{}: no checksum entries", path.display());\n',
        "    let _ = saw_line;\n",
        "test_empty_checkfile_is_failure",
    ),
    (
        "an output error is a failure (#406)",
        "src/main.rs",
        "            Err(e) => return (written, Err(e)),",
        "            Err(_) => return (bytes.len(), Ok(())),",
        "test_output_error_is_failure",
    ),
    (
        "a direct read reads once more for the file's end (#424)",
        "src/direct.rs",
        "        return if more == 0 {\n            Ok(())\n        } else if more > 0 {\n            Err(Failure::Changed)\n        } else {\n            Err(Failure::Err(std::io::Error::last_os_error()))\n        };",
        "        return Ok(());",
        "direct::test::a_file_read_directly_hashes_as_its_bytes_do",
    ),
    (
        "growing file during O_DIRECT discards stale mmap and streams to EOF (H2)",
        "src/main.rs",
        "                        Err(direct::Failure::Changed) => {\n                            // The direct reader observed the file grew or shrank: discard\n                            // the stale mapping and stream the open descriptor to actual EOF.\n                            drop(permits);\n                            drop(map);\n                            let mut hasher = args.base_hasher.clone();\n                            let bytes_hashed = update_from_counted(&mut hasher, file)?;\n                            return Ok(HashRead { hash: hasher.finalize(), bytes_hashed, cursor_moved: true });\n                        }",
        "                        Err(direct::Failure::Changed) => {}",
        "unit_tests::hash_open_file_ex_streams_full_file_when_file_grows_during_direct_read",
    ),
    (
        "direct-read workers stop claiming pieces after another worker fails (M29)",
        "src/direct.rs",
        "                if s.failure.is_some() {\n                    return;\n                }",
        "                if false {\n                    return;\n                }",
        "unit_tests::direct_read_workers_stop_claiming_pieces_after_error",
    ),
    (
        "failed getsockopt(SO_TYPE) fails closed rather than hashing unknown socket (H3)",
        "src/file_end.rs",
        "        if rc < 0 {\n            return Err(io::Error::last_os_error().into());\n        }",
        "        if rc < 0 {\n            return Ok(());\n        }",
        "unit_tests::validate_and_ensure_byte_stream_fd_fail_closed_on_os_error",
    ),
    (
        "post-open fstat rejects descriptor swapped to forbidden checkfile stream (M13)",
        "src/file_end.rs",
        "    if let Some(forbid) = forbidden\n        && meta.kind == FileKind::Stream\n        && meta.id == forbid\n    {\n        crate::bail!(\"this checkfile is a stream, and cannot also be a file to check\");\n    }",
        "    let _ = (&meta, forbidden);",
        "unit_tests::post_open_forbidden_stream_guard_rejects_swapped_descriptor",
    ),
    (
        "checkfile batching strictly enforces retained path byte budget (H4)",
        "src/main.rs",
        "        if !entries.is_empty() && retained_bytes.saturating_add(rec_bytes) > max_batch_bytes {",
        "        if false {",
        "unit_tests::collect_check_batch_enforces_retained_byte_budget",
    ),
    (
        "SIGBUS exits promptly without blocking on undrained full stderr pipe (H5)",
        "src/main.rs",
        "            file_end::write_fatal_nonblocking(MESSAGE);",
        "            let _ = unsafe { libc::write(2, MESSAGE.as_ptr().cast(), MESSAGE.len()) };",
        "unit_tests::sigbus_exits_promptly_with_full_undrained_stderr_pipe_and_writes_when_writable",
    ),
    (
        "a read for a known size goes on to where a read gives no byte (#424)",
        "src/main.rs",
        READ_TO_END,
        READ_TO_END_NOT,
        "unit_tests::test_a_files_size_does_not_decide_where_it_ends",
    ),
    (
        "a batched small file is held to its slot before the copy",
        "src/linux_pipe.rs",
        '        assert!(byte_len <= WARM_SLOT_BLOCKS * 64, "a batched file fits its slot");\n',
        "",
        "linux_pipe::test::a_file_beyond_its_slot_is_refused_before_the_copy",
    ),
    (
        "a buffer is sized by what was read, not by the file's size (#440)",
        "src/linux_pipe.rs",
        "    let need = hint.saturating_add(1).max(prefix.len().saturating_add(4096)).min(SMALL_MEM_CAP + 1);",
        "    let need = hint.saturating_add(1).max(4096).min(SMALL_MEM_CAP + 1);",
        "linux_pipe::test::a_buffer_holds_what_was_read_whatever_the_size_said",
    ),
    (
        "growing the descriptor table lands on no open descriptor (#441)",
        "src/linux_pipe.rs",
        "                let high = libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, target);",
        "                let high = libc::dup2(fd, target);",
        "linux_pipe::test::growing_the_descriptor_table_closes_no_descriptor",
    ),
    (
        "a ring dropped with reads in flight stops the process (#442)",
        "src/uring.rs",
        "        if self.in_flight != 0 {\n            crate::file_end::write_fatal_fmt(",
        "        if false {\n            crate::file_end::write_fatal_fmt(",
        "uring::test::a_ring_dropped_with_a_read_in_flight_stops_the_process",
    ),
    (
        "a failed io_uring_enter stops the process (#409)",
        "src/uring.rs",
        "    crate::file_end::write_fatal_fmt(format_args!(\"b3sum: io_uring_enter failed (errno {errno}); stopping before pending reads' buffers can be released\\n\"));\n    std::process::abort()",
        "    crate::file_end::write_fatal_fmt(format_args!(\"b3sum: io_uring_enter failed (errno {errno}); stopping before pending reads' buffers can be released\\n\"));\n    None",
        "uring::test::failed_enter_stops_before_unwinding",
    ),
    (
        "io_uring submit loop rejects over-count and zero-progress stalls (M31)",
        "src/uring.rs",
        "            let sq_head_after = read_sq_head();\n            match reconcile_submit_progress(to_submit, submitted, sq_head_before, sq_head_after, local_sq_tail) {\n                Ok(0) => return Ok(()),\n                Ok(remaining) => to_submit = remaining,\n                Err(reason) => return Err(reason),\n            }",
        "            let _ = sq_head_before;\n            to_submit = to_submit.saturating_sub(submitted);\n            if to_submit == 0 {\n                return Ok(());\n            }",
        "uring::test::reconcile_submit_progress_rejects_overcount_and_zero_progress",
    ),
    (
        "the file opened is the file hashed where there is no io_uring",
        "src/linux_pipe.rs",
        "        } else {\n            let res = if direct_changed {\n                crate::hash_open_file_streaming_with_meta(args, file, meta)\n            } else {\n                crate::hash_open_file_with_meta(args, file, meta).map(|hr| hr.hash)\n            };\n            sink.put(i, res);\n        }\n",
        "        } else {\n            drop(file);\n            let ctx = crate::file_end::StdinContext::current();\n            sink.put(i, crate::hash_path_hash(args, &paths[i], &ctx, forbidden));\n        }\n",
        "linux_pipe::test::without_a_ring_the_file_opened_is_the_file_hashed",
    ),
]

SUMMARY_RE = re.compile(r"^\s*Summary \[[^\]]+\]\s+(\d+)\s+tests?\s+run:\s+(\d+)\s+passed(?:,\s+(\d+)\s+failed)?", re.MULTILINE)
NEXTEST_TEST_FAIL_EXIT = 100


def ensure_tempdir_outside_root():
    real_root = os.path.realpath(ROOT)
    real_tmp = os.path.realpath(tempfile.gettempdir())
    if os.path.commonpath([real_root, real_tmp]) == real_root:
        sys.exit(f"b3sum_controls: temporary directory {real_tmp} is inside checkout {real_root}; set TMPDIR outside the checkout")


def stage_tree(stage_root):
    for entry in STAGE_ENTRIES:
        src = os.path.join(ROOT, entry)
        dst = os.path.join(stage_root, entry)
        if os.path.isdir(src):
            shutil.copytree(src, dst)
        elif os.path.isfile(src):
            shutil.copy2(src, dst)


def run_named_test(stage_root, target_dir, test):
    env = os.environ.copy()
    env["CARGO_TARGET_DIR"] = target_dir
    cmd = [
        "cargo",
        "nextest",
        "run",
        "--locked",
        "--release",
        "--no-fail-fast",
        "--status-level",
        "all",
        "--failure-output",
        "immediate",
        "-E",
        f"test(={test})",
    ]
    proc = subprocess.Popen(
        cmd,
        cwd=stage_root,
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        start_new_session=True,
    )
    try:
        stdout, stderr = proc.communicate(timeout=CONTROL_TIMEOUT_SECS)
    except subprocess.TimeoutExpired:
        try:
            os.killpg(proc.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        proc.communicate()
        return "timeout", f"timed out after {CONTROL_TIMEOUT_SECS}s running {test}"

    # nextest prints runner status & final Summary lines to stderr.
    matches = list(SUMMARY_RE.finditer(stderr))
    if not matches:
        combined = stdout + "\n" + stderr
        return "build_error", combined[-2000:]
    m = matches[-1]
    ran, passed, failed = int(m.group(1)), int(m.group(2)), int(m.group(3) or 0)
    if ran != 1:
        return "missing", f"expected exactly 1 test matching test(={test}), ran {ran}\n{stderr[-1500:]}"
    if proc.returncode == 0 and passed == 1 and failed == 0:
        return "pass", ""
    fail_line_re = re.compile(rf"^\s*(?:FAIL|ABORT)\s+\[[^\]]+\].*\b{re.escape(test)}\s*$", re.MULTILINE)
    if proc.returncode == NEXTEST_TEST_FAIL_EXIT and passed == 0 and failed == 1 and fail_line_re.search(stderr):
        return "fail", ""
    return "infra_error", f"unexpected nextest exit={proc.returncode} ran={ran} passed={passed} failed={failed}\n{stderr[-1500:]}"


def main():
    ensure_tempdir_outside_root()
    if shutil.which("cargo-nextest") is None:
        probe = subprocess.run(["cargo", "nextest", "--version"], capture_output=True, text=True)
        if probe.returncode != 0:
            sys.exit("b3sum_controls: cargo-nextest is required (install via `sh tools/ci-tools.sh nextest`)")

    bad = []
    with tempfile.TemporaryDirectory(prefix="b3sum-controls-") as tmp:
        stage_root = os.path.join(tmp, "src")
        target_dir = os.path.join(tmp, "target")
        os.makedirs(stage_root)
        stage_tree(stage_root)

        # Verify every anchor matches cleanly in the staged copy before compiling.
        originals = {}
        for name, path, find, _replace, _test in CONTROLS:
            full = os.path.join(stage_root, path)
            if path not in originals:
                with open(full) as f:
                    originals[path] = f.read()
            if originals[path].count(find) != 1:
                sys.exit(f"b3sum_controls: the place of '{name}' is not in {path} once: update this script")

        # Verify every named control test passes in ONE clean nextest invocation
        # on the untouched staged tree before planting individual faults (L5).
        baseline_tests = list(dict.fromkeys(test for _, _, _, _, test in CONTROLS))
        filter_expr = " | ".join(f"test(={t})" for t in baseline_tests)
        env = os.environ.copy()
        env["CARGO_TARGET_DIR"] = target_dir
        try:
            proc = subprocess.run(
                [
                    "cargo",
                    "nextest",
                    "run",
                    "--locked",
                    "--release",
                    "--no-fail-fast",
                    "--status-level",
                    "pass",
                    "--final-status-level",
                    "fail",
                    "-E",
                    filter_expr,
                ],
                cwd=stage_root,
                env=env,
                capture_output=True,
                text=True,
                timeout=CONTROL_TIMEOUT_SECS,
            )
        except subprocess.TimeoutExpired:
            sys.exit(f"b3sum_controls: baseline suite timed out after {CONTROL_TIMEOUT_SECS}s")
        m = SUMMARY_RE.search(proc.stderr)
        if not m or proc.returncode != 0 or int(m.group(1)) != len(baseline_tests) or int(m.group(2)) != len(baseline_tests) or int(m.group(3) or 0) != 0:
            sys.exit(f"b3sum_controls: baseline verification failed (exit={proc.returncode}, expected {len(baseline_tests)} passes)\n{proc.stderr[-2000:]}")

        for name, path, find, replace, test in CONTROLS:
            full = os.path.join(stage_root, path)
            original = originals[path]

            try:
                with open(full, "w") as f:
                    f.write(original.replace(find, replace, 1))
                status, why = run_named_test(stage_root, target_dir, test)
            finally:
                with open(full, "w") as f:
                    f.write(original)

            labels = {
                "fail": "fails without it",
                "pass": "STILL PASSES without it",
                "build_error": "does not build without it",
                "timeout": "TIMED OUT without it",
                "missing": "TEST MISSING without it",
                "infra_error": "INFRA ERROR without it",
            }
            verdict = labels[status]
            print(f"{verdict:26}  {name}  ({test})", flush=True)
            if status != "fail":
                if why:
                    print(why, file=sys.stderr)
                bad.append(name)

    if bad:
        sys.exit(f"b3sum_controls: {len(bad)} protection(s) not proved: {', '.join(bad)}")
    print(f"b3sum_controls: all {len(CONTROLS)} protections proved (staged outside source tree)")


if __name__ == "__main__":
    main()
