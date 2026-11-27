//! The build version: the crate's version with what names this build's code
//! (Zooko, November 9, 2026: as much as fits in the version). The source
//! version, in Cargo.toml, is a release's `X.Y.Z+<its code commit>` on the
//! release commit alone, and `X.Y.Z+dev` on the commits after it. A build of
//! the release commit is named by that version alone. Any other build appends
//! its commit, and a dirty tree the Git tree id of its tracked files, staged
//! index entries, and non-ignored untracked files under tracked subdirectories
//! (the exact input set Cargo watches incrementally without recursively
//! watching `target/`; release scripts build from clean `HEAD` clones):
//!
//!     0.8.0+b3c5efc7…              the release commit, clean
//!     0.8.0+dev.<commit>           a later commit, clean
//!     0.8.0+dev.<commit>.dirty.<tree id>
//!
//! (semver build metadata: dot-separated ASCII letters, digits and hyphens.)
//! The commit and tree come from the checkout's Git where this crate is the
//! top of a work tree (a clone, a worktree, Cargo's checkout of a git
//! dependency), else from the crate's `.cargo_vcs_info.json` (a package: its
//! full 40-hex commit, and `.dirty` without a tree where it was packaged
//! dirty), else nothing is appended. Shared by `build.rs` and
//! `tests/build_version.rs`.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Cargo's marker in its checkout of a git dependency: no change of ours.
const CARGO_MARKER: &str = ".cargo-ok";

fn git_bytes_with_stdin(dir: &Path, args: &[&str], index: Option<&Path>, objects: Option<(&Path, &OsStr)>, stdin_bytes: Option<&[u8]>) -> Option<Vec<u8>> {
    use std::io::Write;
    use std::process::Stdio;
    let mut command = Command::new("git");
    command.arg("-C").arg(dir).args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    if let Some(index) = index {
        command.env("GIT_INDEX_FILE", index);
    }
    if let Some((private_obj, alternates)) = objects {
        // Redirect all newly written Git objects (dirty blobs and synthetic
        // trees) into private build scratch while reading existing objects
        // read-only from the resolved object store + inherited alternates (M17).
        command.env("GIT_OBJECT_DIRECTORY", private_obj);
        command.env("GIT_ALTERNATE_OBJECT_DIRECTORIES", alternates);
    }
    if let Some(input) = stdin_bytes {
        command.stdin(Stdio::piped());
        let mut child = command.spawn().ok()?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(input).ok()?;
        }
        let out = child.wait_with_output().ok()?;
        return out.status.success().then_some(out.stdout);
    }
    let out = command.output().ok()?;
    out.status.success().then_some(out.stdout)
}

fn git_bytes(dir: &Path, args: &[&str], index: Option<&Path>, objects: Option<(&Path, &OsStr)>) -> Option<Vec<u8>> {
    git_bytes_with_stdin(dir, args, index, objects, None)
}

fn git_path(dir: &Path, args: &[&str]) -> Option<PathBuf> {
    let mut bytes = git_bytes(dir, args, None, None)?;
    if bytes.ends_with(b"\n") {
        bytes.pop();
    }
    let p = PathBuf::from(OsString::from_vec(bytes));
    Some(if p.is_absolute() { p } else { dir.join(p) })
}

fn git_ascii(dir: &Path, args: &[&str], index: Option<&Path>, objects: Option<(&Path, &OsStr)>) -> Option<String> {
    let bytes = git_bytes(dir, args, index, objects)?;
    String::from_utf8(bytes).ok().map(|s| s.trim().to_owned())
}

fn is_full_hex_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Encode `real_objects` as one Git alternate entry (double-quoting per Git's
/// `GIT_ALTERNATE_OBJECT_DIRECTORIES` syntax if it contains `:`, `"`, or `\`)
/// and append any inherited `GIT_ALTERNATE_OBJECT_DIRECTORIES` value.
fn build_alternates_env(real_objects: &Path) -> OsString {
    let raw = real_objects.as_os_str().as_bytes();
    let mut out = Vec::with_capacity(raw.len() + 8);
    if raw.iter().any(|&b| matches!(b, b':' | b'"' | b'\\')) {
        out.push(b'"');
        for &b in raw {
            if b == b'"' || b == b'\\' {
                out.push(b'\\');
            }
            out.push(b);
        }
        out.push(b'"');
    } else {
        out.extend_from_slice(raw);
    }
    if let Some(existing) = std::env::var_os("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        && !existing.is_empty()
    {
        out.push(b':');
        out.extend_from_slice(existing.as_bytes());
    }
    OsString::from_vec(out)
}

/// The build version of this standalone crate at `krate` (the repository
/// root), whose source version is `version`, and the paths whose change
/// changes it (for rerun-if-changed).
pub fn build_version(krate: &Path, version: &str, scratch: &Path) -> (String, Vec<PathBuf>) {
    let package = || (from_package_info(krate, version), vec![krate.join(".cargo_vcs_info.json")]);
    let dir = krate;
    // Query each path individually as raw OS bytes so newlines or non-UTF-8
    // checkout paths cannot corrupt multi-line parsing.
    let Some(top) = git_path(dir, &["rev-parse", "--show-toplevel"]) else {
        return package();
    };
    if top.canonicalize().ok() != dir.canonicalize().ok() {
        return package();
    }
    let (Some(git_dir), Some(common), Some(real_objects), Some(commit), Some(head_tree)) = (
        git_path(dir, &["rev-parse", "--git-dir"]),
        git_path(dir, &["rev-parse", "--git-common-dir"]),
        git_path(dir, &["rev-parse", "--git-path", "objects"]),
        git_ascii(dir, &["rev-parse", "HEAD"], None, None),
        git_ascii(dir, &["rev-parse", "HEAD^{tree}"], None, None),
    ) else {
        return package();
    };
    let alternates = build_alternates_env(&real_objects);
    // The working tree as Git would commit it, through an index and object
    // directory of our own inside `scratch` so uncommitted files are never
    // written into the repository's `.git/objects` (M17).
    let index = scratch.join("build-version-index");
    let private_objects = scratch.join("build-version-objects");
    let _ = std::fs::remove_file(&index);
    let _ = std::fs::remove_dir_all(&private_objects);
    let _ = std::fs::create_dir_all(&private_objects);
    let objs = Some((private_objects.as_path(), alternates.as_os_str()));
    // Always populate the scratch index from `HEAD` via `read-tree` instead of
    // copying `.git/index` so `assume-unchanged` / `skip-worktree` flags cannot
    // hide modified source bytes as clean (M1). Hash the exact input set that
    // `watched()` invalidates in Cargo (R3 M3): every tracked file (via
    // `add -u`), every existing path currently staged in the real index (incl.
    // staged root files and force-added ignored files), and every non-ignored
    // untracked file under tracked top-level subdirectories (e.g. `src/`,
    // `tests/`, `tools/`), excluding untracked scratch files at the repo root
    // that Cargo cannot watch without recursively watching `target/`.
    let cached_raw = git_bytes(dir, &["ls-files", "-z", "--cached"], None, None).unwrap_or_default();
    let mut cached_existing = Vec::new();
    let mut tracked_top_dirs: BTreeSet<Vec<u8>> = BTreeSet::new();
    for raw in cached_raw.split(|&b| b == 0).filter(|s| !s.is_empty()) {
        if raw == CARGO_MARKER.as_bytes() {
            continue;
        }
        if let Some(slash) = raw.iter().position(|&b| b == b'/') {
            tracked_top_dirs.insert(raw[..slash].to_vec());
        }
        if dir.join(OsStr::from_bytes(raw)).symlink_metadata().is_ok() {
            cached_existing.extend_from_slice(raw);
            cached_existing.push(0);
        }
    }
    let existing_top_dirs: Vec<OsString> = tracked_top_dirs
        .iter()
        .filter_map(|d| {
            let os = OsStr::from_bytes(d);
            dir.join(os).is_dir().then(|| os.to_os_string())
        })
        .collect();
    let tree = git_ascii(dir, &["read-tree", "HEAD"], Some(&index), objs)
        .and_then(|_| git_ascii(dir, &["add", "-u", "--", "."], Some(&index), objs))
        .and_then(|_| {
            if cached_existing.is_empty() {
                Some(String::new())
            } else {
                git_bytes_with_stdin(dir, &["add", "-f", "--pathspec-from-file=-", "--pathspec-file-nul"], Some(&index), objs, Some(&cached_existing))
                    .map(|_| String::new())
            }
        })
        .and_then(|_| {
            if existing_top_dirs.is_empty() {
                Some(String::new())
            } else {
                let mut pathspec_in = Vec::new();
                for d in &existing_top_dirs {
                    pathspec_in.extend_from_slice(d.as_bytes());
                    pathspec_in.push(0);
                }
                git_bytes_with_stdin(dir, &["add", "-A", "--pathspec-from-file=-", "--pathspec-file-nul"], Some(&index), objs, Some(&pathspec_in))
                    .map(|_| String::new())
            }
        })
        .and_then(|_| git_ascii(dir, &["write-tree"], Some(&index), objs));
    let _ = std::fs::remove_file(&index);
    let _ = std::fs::remove_dir_all(&private_objects);
    let clean = tree.as_deref() == Some(head_tree.as_str());
    let release = version
        .split_once('+')
        .is_some_and(|(_, named)| is_full_hex_sha(named) && git_ascii(dir, &["rev-parse", "HEAD^"], None, None).as_deref() == Some(named));
    let name = match (clean, release) {
        (true, true) => version.to_owned(),
        (true, false) => format!("{version}.{commit}"),
        (false, _) => format!("{version}.{commit}.dirty{}", tree.map_or(String::new(), |t| format!(".{t}"))),
    };
    (name, watched(dir, &git_dir, &common, &cached_raw, &existing_top_dirs))
}

/// Minimal strict RFC 8259 JSON parser for `.cargo_vcs_info.json` (M16):
/// validates the entire document with no trailing garbage and returns typed
/// values so unrelated top-level keys cannot shadow `git.sha1` / `git.dirty`.
#[derive(Debug)]
enum JsonVal {
    Other,
    Bool(bool),
    Str(String),
    Object(Vec<(String, JsonVal)>),
}

struct JsonParser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> JsonParser<'a> {
    fn skip_ws(&mut self) {
        while let Some(&b) = self.bytes.get(self.pos) {
            if matches!(b, b' ' | b'\t' | b'\r' | b'\n') {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn parse_str(&mut self) -> Option<String> {
        if *self.bytes.get(self.pos)? != b'"' {
            return None;
        }
        self.pos += 1;
        let mut out = String::new();
        while let Some(&b) = self.bytes.get(self.pos) {
            self.pos += 1;
            match b {
                b'"' => return Some(out),
                b'\\' => {
                    let esc = *self.bytes.get(self.pos)?;
                    self.pos += 1;
                    match esc {
                        b'"' | b'\\' | b'/' => out.push(esc as char),
                        b'b' => out.push('\u{0008}'),
                        b'f' => out.push('\u{000C}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hex_bytes = self.bytes.get(self.pos..self.pos + 4)?;
                            if !hex_bytes.iter().all(u8::is_ascii_hexdigit) {
                                return None;
                            }
                            let hex = std::str::from_utf8(hex_bytes).ok()?;
                            let code = u16::from_str_radix(hex, 16).ok()?;
                            self.pos += 4;
                            if (0xD800..=0xDBFF).contains(&code) {
                                // UTF-16 high surrogate: must be followed by `\uXXXX` low surrogate (M3).
                                if self.bytes.get(self.pos..self.pos + 2) != Some(b"\\u") {
                                    return None;
                                }
                                self.pos += 2;
                                let low_bytes = self.bytes.get(self.pos..self.pos + 4)?;
                                if !low_bytes.iter().all(u8::is_ascii_hexdigit) {
                                    return None;
                                }
                                let low_hex = std::str::from_utf8(low_bytes).ok()?;
                                let low = u16::from_str_radix(low_hex, 16).ok()?;
                                if !(0xDC00..=0xDFFF).contains(&low) {
                                    return None;
                                }
                                self.pos += 4;
                                let scalar = 0x10000 + (((u32::from(code) - 0xD800) << 10) | (u32::from(low) - 0xDC00));
                                out.push(char::from_u32(scalar)?);
                            } else if (0xDC00..=0xDFFF).contains(&code) {
                                return None;
                            } else {
                                out.push(char::from_u32(u32::from(code))?);
                            }
                        }
                        _ => return None,
                    }
                }
                0x00..=0x1F => return None,
                0x20..=0x7F => out.push(b as char),
                _ => {
                    // Decode only the 2..=4 bytes of this single UTF-8 scalar (O(1) per char; M3).
                    let start = self.pos - 1;
                    let width = match b {
                        0xC2..=0xDF => 2,
                        0xE0..=0xEF => 3,
                        0xF0..=0xF4 => 4,
                        _ => return None,
                    };
                    let slice = std::str::from_utf8(self.bytes.get(start..start + width)?).ok()?;
                    let ch = slice.chars().next()?;
                    self.pos = start + width;
                    out.push(ch);
                }
            }
        }
        None
    }

    fn parse_value(&mut self) -> Option<JsonVal> {
        self.skip_ws();
        match *self.bytes.get(self.pos)? {
            b'"' => Some(JsonVal::Str(self.parse_str()?)),
            b'{' => {
                self.pos += 1;
                self.skip_ws();
                let mut pairs = Vec::new();
                if self.bytes.get(self.pos) == Some(&b'}') {
                    self.pos += 1;
                    return Some(JsonVal::Object(pairs));
                }
                loop {
                    self.skip_ws();
                    let k = self.parse_str()?;
                    self.skip_ws();
                    if *self.bytes.get(self.pos)? != b':' {
                        return None;
                    }
                    self.pos += 1;
                    let v = self.parse_value()?;
                    pairs.push((k, v));
                    self.skip_ws();
                    match *self.bytes.get(self.pos)? {
                        b',' => self.pos += 1,
                        b'}' => {
                            self.pos += 1;
                            return Some(JsonVal::Object(pairs));
                        }
                        _ => return None,
                    }
                }
            }
            b'[' => {
                self.pos += 1;
                self.skip_ws();
                if self.bytes.get(self.pos) == Some(&b']') {
                    self.pos += 1;
                    return Some(JsonVal::Other);
                }
                loop {
                    let _ = self.parse_value()?;
                    self.skip_ws();
                    match *self.bytes.get(self.pos)? {
                        b',' => self.pos += 1,
                        b']' => {
                            self.pos += 1;
                            return Some(JsonVal::Other);
                        }
                        _ => return None,
                    }
                }
            }
            b't' if self.bytes.get(self.pos..self.pos + 4) == Some(b"true") => {
                self.pos += 4;
                Some(JsonVal::Bool(true))
            }
            b'f' if self.bytes.get(self.pos..self.pos + 5) == Some(b"false") => {
                self.pos += 5;
                Some(JsonVal::Bool(false))
            }
            b'n' if self.bytes.get(self.pos..self.pos + 4) == Some(b"null") => {
                self.pos += 4;
                Some(JsonVal::Other)
            }
            b'-' | b'0'..=b'9' => {
                if self.bytes[self.pos] == b'-' {
                    self.pos += 1;
                }
                match *self.bytes.get(self.pos)? {
                    b'0' => self.pos += 1,
                    b'1'..=b'9' => {
                        self.pos += 1;
                        while self.bytes.get(self.pos).is_some_and(u8::is_ascii_digit) {
                            self.pos += 1;
                        }
                    }
                    _ => return None,
                }
                if self.bytes.get(self.pos) == Some(&b'.') {
                    self.pos += 1;
                    if !self.bytes.get(self.pos).is_some_and(u8::is_ascii_digit) {
                        return None;
                    }
                    while self.bytes.get(self.pos).is_some_and(u8::is_ascii_digit) {
                        self.pos += 1;
                    }
                }
                if matches!(self.bytes.get(self.pos), Some(b'e' | b'E')) {
                    self.pos += 1;
                    if matches!(self.bytes.get(self.pos), Some(b'+' | b'-')) {
                        self.pos += 1;
                    }
                    if !self.bytes.get(self.pos).is_some_and(u8::is_ascii_digit) {
                        return None;
                    }
                    while self.bytes.get(self.pos).is_some_and(u8::is_ascii_digit) {
                        self.pos += 1;
                    }
                }
                Some(JsonVal::Other)
            }
            _ => None,
        }
    }
}

fn parse_json(text: &str) -> Option<JsonVal> {
    let mut parser = JsonParser { bytes: text.as_bytes(), pos: 0 };
    let val = parser.parse_value()?;
    parser.skip_ws();
    (parser.pos == parser.bytes.len()).then_some(val)
}

fn obj_get<'a>(val: &'a JsonVal, key: &str) -> Option<&'a JsonVal> {
    let JsonVal::Object(pairs) = val else { return None };
    pairs.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v)
}

/// A package's build: its validated 40-hex `git.sha1` and optional `git.dirty`
/// flag as `cargo package` wrote them (M16, M3). Missing `.cargo_vcs_info.json`
/// (`ErrorKind::NotFound`) returns the bare manifest version; a *present* file
/// that is unreadable, non-UTF-8, malformed, or truncated returns
/// `{version}.unverified` so corrupt provenance is never mistaken for clean.
fn from_package_info(dir: &Path, version: &str) -> String {
    let unverified = || format!("{version}.unverified");
    let raw = match std::fs::read(dir.join(".cargo_vcs_info.json")) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return version.to_owned(),
        Err(_) => return unverified(),
    };
    let Ok(info) = String::from_utf8(raw) else {
        return unverified();
    };
    let Some(root) = parse_json(&info) else {
        return unverified();
    };
    let Some(git_obj) = obj_get(&root, "git") else {
        return unverified();
    };
    let Some(JsonVal::Str(commit)) = obj_get(git_obj, "sha1") else {
        return unverified();
    };
    if !is_full_hex_sha(commit) {
        return unverified();
    }
    let dirty = match obj_get(git_obj, "dirty") {
        None => false,
        Some(JsonVal::Bool(b)) => *b,
        Some(_) => return unverified(),
    };
    if version.ends_with("+dev") || dirty { format!("{version}.{commit}{}", if dirty { ".dirty" } else { "" }) } else { version.to_owned() }
}

/// What a commit, a checkout, or an edit to the stamped input set changes:
/// HEAD, the index, the reflog, the refs, every tracked/staged file, every
/// tracked top-level subdirectory (watched recursively by Cargo so new
/// untracked files or nested subdirs trigger a rerun), and existing untracked
/// files inside those tracked subdirectories — never the repo root itself
/// (which would make Cargo recursively scan `target/` and `.git/` on every
/// no-op build; M2, R3 M3).
fn watched(dir: &Path, git_dir: &Path, common: &Path, cached_raw: &[u8], top_dirs: &[OsString]) -> Vec<PathBuf> {
    let mut set = BTreeSet::new();
    for p in [git_dir.join("HEAD"), git_dir.join("index"), git_dir.join("logs/HEAD"), common.join("refs"), common.join("packed-refs")] {
        if p.exists() {
            set.insert(p);
        }
    }
    for raw in cached_raw.split(|&b| b == 0).filter(|s| !s.is_empty()) {
        if raw == CARGO_MARKER.as_bytes() {
            continue;
        }
        let full = dir.join(OsStr::from_bytes(raw));
        if full.symlink_metadata().is_ok() {
            set.insert(full);
        }
    }
    for top in top_dirs {
        let top_path = dir.join(top);
        if top_path.is_dir() {
            set.insert(top_path);
        }
    }
    if !top_dirs.is_empty()
        && let Some(untracked) = git_bytes(dir, &["ls-files", "-z", "--others", "--exclude-standard"], None, None)
    {
        for raw in untracked.split(|&b| b == 0).filter(|s| !s.is_empty()) {
            let Some(slash) = raw.iter().position(|&b| b == b'/') else {
                continue;
            };
            let prefix = OsStr::from_bytes(&raw[..slash]);
            if top_dirs.iter().any(|d| d.as_os_str() == prefix) {
                let full = dir.join(OsStr::from_bytes(raw));
                if full.symlink_metadata().is_ok() {
                    set.insert(full);
                }
            }
        }
    }
    set.into_iter().collect()
}
