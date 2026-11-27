//! Tests for standalone `build_version.rs` (M16, M17): clean/dirty Git
//! checkouts, linked worktrees, zero `.git/objects` pollution on dirty trees,
//! release commits, and `.cargo_vcs_info.json` validation.

#[path = "../build_version.rs"]
mod build_version;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

struct TempDir(PathBuf);

impl TempDir {
    fn new(tag: &str) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("b3sum-build-ver-{tag}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn list_object_files(objects_dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir(objects_dir) else {
        return found;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Ok(sub) = fs::read_dir(&path) {
                for s in sub.flatten() {
                    found.push(s.path());
                }
            }
        } else {
            found.push(path);
        }
    }
    found.sort();
    found
}

fn init_repo(dir: &Path) -> String {
    git(dir, &["init", "-q", "--initial-branch=main"]);
    git(dir, &["config", "user.name", "Test"]);
    git(dir, &["config", "user.email", "test@example.invalid"]);
    git(dir, &["config", "commit.gpgsign", "false"]);
    fs::write(dir.join("Cargo.toml"), "[package]\nname = \"b3sums\"\nversion = \"0.11.0+dev\"\n").unwrap();
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-qm", "initial"]);
    git(dir, &["rev-parse", "HEAD"])
}

#[test]
fn git_checkout_clean_dirty_no_object_pollution_worktree_and_release() {
    let root = TempDir::new("git");
    let repo = root.path().join("repo");
    let scratch = root.path().join("scratch");
    fs::create_dir_all(&repo).unwrap();
    fs::create_dir_all(&scratch).unwrap();
    let head = init_repo(&repo);

    // 1. Clean dev commit.
    let (ver, _watched) = build_version::build_version(&repo, "0.11.0+dev", &scratch);
    assert_eq!(ver, format!("0.11.0+dev.{head}"));

    // 2. Dirty untracked file must mark `.dirty.<tree>` WITHOUT writing ANY
    //    object into the repository's `.git/objects` (M17), and the repo root
    //    directory itself must be watched so newly added root files invalidate
    //    a cached clean build (F8).
    let (clean_ver, clean_watched) = build_version::build_version(&repo, "0.11.0+dev", &scratch);
    assert_eq!(clean_ver, format!("0.11.0+dev.{head}"));
    assert!(clean_watched.contains(&repo), "repo root directory must be watched so new root files trigger rerun");
    let objects_before = list_object_files(&repo.join(".git/objects"));
    let untracked = repo.join("untracked secret.txt");
    fs::write(&untracked, b"never write me to .git/objects").unwrap();
    let (dirty_ver, dirty_watched) = build_version::build_version(&repo, "0.11.0+dev", &scratch);
    let objects_after = list_object_files(&repo.join(".git/objects"));
    assert_eq!(objects_before, objects_after, "build_version must not write uncommitted files into .git/objects (M17)");
    assert!(dirty_ver.starts_with(&format!("0.11.0+dev.{head}.dirty.")), "got {dirty_ver}");
    assert!(dirty_watched.contains(&untracked), "untracked file with spaces must be watched via NUL-delimited ls-files (M17)");
    fs::remove_file(&untracked).unwrap();

    // 2b. A checkout path containing ':' must still quote its alternate object
    //     directory cleanly and report clean (not `.dirty` from missing objects).
    let colon_repo = root.path().join("repo:with:colon");
    fs::create_dir_all(&colon_repo).unwrap();
    let colon_head = init_repo(&colon_repo);
    let (colon_ver, _) = build_version::build_version(&colon_repo, "0.11.0+dev", &scratch);
    assert_eq!(colon_ver, format!("0.11.0+dev.{colon_head}"), "colon in repo path must not split GIT_ALTERNATE_OBJECT_DIRECTORIES");

    // 3. Linked worktree resolves clean commit properly (M16).
    let wt = root.path().join("linked-wt");
    git(&repo, &["worktree", "add", "-q", "--detach", wt.to_str().unwrap(), "HEAD"]);
    let (wt_ver, _) = build_version::build_version(&wt, "0.11.0+dev", &scratch);
    assert_eq!(wt_ver, format!("0.11.0+dev.{head}"));

    // 4. Stamped release version commit returns bare release version when clean.
    let rel_version = format!("0.11.0+{head}");
    fs::write(repo.join("Cargo.toml"), format!("[package]\nname = \"b3sums\"\nversion = \"{rel_version}\"\n")).unwrap();
    git(&repo, &["commit", "-qam", "release stamp"]);
    let (rel_ver, _) = build_version::build_version(&repo, &rel_version, &scratch);
    assert_eq!(rel_ver, rel_version);
}

#[test]
fn package_vcs_info_validates_full_sha_and_dirty_flag() {
    let dir = TempDir::new("pkg");
    let scratch = TempDir::new("pkg-scratch");
    let sha = "0123456789abcdef0123456789abcdef01234567";

    // Missing VCS info returns bare manifest version.
    let (v, _) = build_version::build_version(dir.path(), "0.11.0+dev", scratch.path());
    assert_eq!(v, "0.11.0+dev");

    // Clean packaged dev build appends full 40-hex SHA.
    fs::write(dir.path().join(".cargo_vcs_info.json"), format!("{{\"git\":{{\"sha1\":\"{sha}\"}},\"path_in_vcs\":\"\"}}")).unwrap();
    let (v, _) = build_version::build_version(dir.path(), "0.11.0+dev", scratch.path());
    assert_eq!(v, format!("0.11.0+dev.{sha}"));

    // Dirty packaged release build appends `.sha.dirty`.
    fs::write(dir.path().join(".cargo_vcs_info.json"), format!("{{\"git\":{{\"sha1\":\"{sha}\",\"dirty\":true}},\"path_in_vcs\":\"\"}}")).unwrap();
    let (v, _) = build_version::build_version(dir.path(), &format!("0.11.0+{sha}"), scratch.path());
    assert_eq!(v, format!("0.11.0+{sha}.{sha}.dirty"));

    // Truncated / malformed SHA, truncated JSON, trailing garbage, and top-level
    // `dirty` shadowing `git.dirty` are all handled strictly (M16).
    fs::write(dir.path().join(".cargo_vcs_info.json"), "{\"git\":{\"sha1\":\"deadbeef\"},\"path_in_vcs\":\"\"}").unwrap();
    let (v, _) = build_version::build_version(dir.path(), "0.11.0+dev", scratch.path());
    assert_eq!(v, "0.11.0+dev");

    // Truncated JSON after sha1 must be rejected.
    fs::write(dir.path().join(".cargo_vcs_info.json"), format!("{{\"git\":{{\"sha1\":\"{sha}\"")).unwrap();
    let (v, _) = build_version::build_version(dir.path(), "0.11.0+dev", scratch.path());
    assert_eq!(v, "0.11.0+dev");

    // Trailing garbage after closing brace must be rejected.
    fs::write(dir.path().join(".cargo_vcs_info.json"), format!("{{\"git\":{{\"sha1\":\"{sha}\"}}}} garbage")).unwrap();
    let (v, _) = build_version::build_version(dir.path(), "0.11.0+dev", scratch.path());
    assert_eq!(v, "0.11.0+dev");

    // Top-level `"dirty": false` before `"git": {"dirty": true}` must NOT mask git.dirty!
    fs::write(dir.path().join(".cargo_vcs_info.json"), format!("{{\"dirty\":false,\"git\":{{\"sha1\":\"{sha}\",\"dirty\":true}}}}")).unwrap();
    let (v, _) = build_version::build_version(dir.path(), &format!("0.11.0+{sha}"), scratch.path());
    assert_eq!(v, format!("0.11.0+{sha}.{sha}.dirty"));
}
