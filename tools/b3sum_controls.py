#!/usr/bin/env python3
"""Each of b3sum's protections has a test that fails without it.

    python3 tools/b3sum_controls.py

A protection here is a check or a rule in b3sum's code that a bug once
lacked. For each, this takes the protection out with one edit, runs the
test that is there for it, and requires that test to fail; then the file
is put back. A protection whose test still passes is not protected
against being lost: this exits 1 naming it. The tests must also pass on
the code as it is.

b3sum is outside the mutation gate (tools/mutants), which is slow on it
(BLAKE3 #386); this is its bounded stand-in, for the protections named.
A build of b3sum's tests per protection: about four minutes in all.
"""

import os
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

READ_TO_END = "    file.read_to_end(bytes)?;\n    Ok(())\n}\n\n/// The hash of the input at `path`"
READ_TO_END_NOT = "    bytes.truncate(says);\n    Ok(())\n}\n\n/// The hash of the input at `path`"

# (the protection, its file, the text to find once, what replaces it, the test)
CONTROLS = [
    (
        "--no-mmap maps no file (#432)",
        "src/main.rs",
        "        if self.inner.no_mmap {\n            return None;\n        }\n",
        "",
        "test_no_mmap_maps_no_file",
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
        "a_file_read_directly_hashes_as_its_bytes_do",
    ),
    (
        "growing file during O_DIRECT discards stale mmap and streams to EOF (H2)",
        "src/main.rs",
        "                        Err(direct::Failure::Changed) => {\n                            // The direct reader observed the file grew or shrank: discard\n                            // the stale mapping and stream the open descriptor to actual EOF.\n                            drop(permits);\n                            drop(map);\n                            let mut hasher = args.base_hasher.clone();\n                            let bytes_hashed = update_from_counted(&mut hasher, file)?;\n                            return Ok(HashRead { hash: hasher.finalize(), bytes_hashed, cursor_moved: true });\n                        }",
        "                        Err(direct::Failure::Changed) => {}",
        "hash_open_file_ex_streams_full_file_when_file_grows_during_direct_read",
    ),
    (
        "direct-read workers stop claiming pieces after another worker fails (M29)",
        "src/direct.rs",
        "                if s.failure.is_some() {\n                    return;\n                }",
        "                if false {\n                    return;\n                }",
        "direct_read_workers_stop_claiming_pieces_after_error",
    ),
    (
        "failed getsockopt(SO_TYPE) fails closed rather than hashing unknown socket (H3)",
        "src/file_end.rs",
        "        if rc < 0 {\n            return Err(io::Error::last_os_error().into());\n        }",
        "        if rc < 0 {\n            return Ok(());\n        }",
        "validate_and_ensure_byte_stream_fd_fail_closed_on_os_error",
    ),
    (
        "checkfile batching strictly enforces retained path byte budget (H4)",
        "src/main.rs",
        "        if !entries.is_empty() && retained_bytes.saturating_add(rec_bytes) > max_batch_bytes {",
        "        if false {",
        "collect_check_batch_enforces_retained_byte_budget",
    ),
    (
        "SIGBUS exits promptly without blocking on undrained full stderr pipe (H5)",
        "src/main.rs",
        "            file_end::write_fatal_nonblocking(MESSAGE);",
        "            let _ = unsafe { libc::write(2, MESSAGE.as_ptr().cast(), MESSAGE.len()) };",
        "sigbus_exits_promptly_with_full_undrained_stderr_pipe_and_writes_when_writable",
    ),
    (
        "a read for a known size goes on to where a read gives no byte (#424)",
        "src/main.rs",
        READ_TO_END,
        READ_TO_END_NOT,
        "test_a_files_size_does_not_decide_where_it_ends",
    ),
    (
        "a batched small file is held to its slot before the copy",
        "src/linux_pipe.rs",
        '        assert!(byte_len <= WARM_SLOT_BLOCKS * 64, "a batched file fits its slot");\n',
        "",
        "a_file_beyond_its_slot_is_refused_before_the_copy",
    ),
    (
        "a buffer is sized by what was read, not by the file's size (#440)",
        "src/linux_pipe.rs",
        "    let need = hint.saturating_add(1).max(prefix.len().saturating_add(4096)).min(SMALL_MEM_CAP + 1);",
        "    let need = hint.saturating_add(1).max(4096).min(SMALL_MEM_CAP + 1);",
        "a_buffer_holds_what_was_read_whatever_the_size_said",
    ),
    (
        "growing the descriptor table lands on no open descriptor (#441)",
        "src/linux_pipe.rs",
        "                let high = libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, target);",
        "                let high = libc::dup2(fd, target);",
        "growing_the_descriptor_table_closes_no_descriptor",
    ),
    (
        "a ring dropped with reads in flight stops the process (#442)",
        "src/uring.rs",
        "        if self.in_flight != 0 {\n            crate::file_end::write_fatal_fmt(",
        "        if false {\n            crate::file_end::write_fatal_fmt(",
        "a_ring_dropped_with_a_read_in_flight_stops_the_process",
    ),
    (
        "a failed io_uring_enter stops the process (#409)",
        "src/uring.rs",
        "stopping before pending reads' buffers can be released\\n\"));\n    std::process::abort()",
        "stopping before pending reads' buffers can be released\\n\"));\n    None",
        "failed_enter_stops_before_unwinding",
    ),
    (
        "the file opened is the file hashed where there is no io_uring",
        "src/linux_pipe.rs",
        "        } else {\n            let res = if direct_changed { crate::hash_open_file_streaming(args, file) } else { crate::hash_open_file(args, file) };\n            sink.put(i, res);\n        }\n",
        "        } else {\n            drop(file);\n            let ctx = crate::file_end::StdinContext::current();\n            sink.put(i, crate::hash_path_hash(args, &paths[i], &ctx, forbidden));\n        }\n",
        "without_a_ring_the_file_opened_is_the_file_hashed",
    ),
]


def test_passes(test):
    run = subprocess.run(["cargo", "test", "-q", "--release", "--locked", "--manifest-path", "Cargo.toml", "--", test], cwd=ROOT, capture_output=True, text=True)
    ran = [line for line in run.stdout.splitlines() if line.startswith("test result:")]
    if run.returncode != 0 and not ran and "running " not in run.stdout:
        return None, run.stdout[-1500:] + run.stderr[-1500:]
    if run.returncode == 0 and not any(" 1 passed" in line for line in ran):
        sys.exit(f"b3sum_controls: no test named {test} ran: update this script")
    return run.returncode == 0, ""


def main():
    bad = []
    for name, path, find, replace, test in CONTROLS:
        full = os.path.join(ROOT, path)
        with open(full) as f:
            original = f.read()
        if original.count(find) != 1:
            sys.exit(f"b3sum_controls: the place of '{name}' is not in {path} once: update this script")
        passes, why = test_passes(test)
        if passes is not True:
            sys.exit(f"b3sum_controls: {test} does not pass on the code as it is\n{why}")
        try:
            with open(full, "w") as f:
                f.write(original.replace(find, replace))
            passes, why = test_passes(test)
        finally:
            with open(full, "w") as f:
                f.write(original)
        verdict = "fails without it" if passes is False else "does not build without it" if passes is None else "STILL PASSES without it"
        print(f"{verdict:26}  {name}  ({test})", flush=True)
        if passes is not False:
            bad.append(name)
            if why:
                print(why)
    if bad:
        sys.exit(f"b3sum_controls: {len(bad)} protections have no test that fails without them")
    print(f"b3sum_controls: each of {len(CONTROLS)} protections has a test that fails without it")


if __name__ == "__main__":
    main()
