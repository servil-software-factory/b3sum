//! Tests for standalone `build_version.rs` (M16, M17): clean/dirty Git
//! checkouts, linked worktrees, zero `.git/objects` pollution on dirty trees,
//! release commits, and `.cargo_vcs_info.json` validation.

#[path = "../build_version.rs"]
mod build_version;
#[path = "../src/test_temp.rs"]
mod tempfile;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

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
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("repo");
    let scratch = root.path().join("scratch");
    fs::create_dir_all(&repo).unwrap();
    fs::create_dir_all(&scratch).unwrap();
    let head = init_repo(&repo);

    // 1. Clean dev commit: repo root itself must NOT be in watched (avoids
    //    Cargo recursively scanning target/ and .git/ on every build; M2),
    //    while source subdirs like `src` ARE watched.
    let (clean_ver, clean_watched) = build_version::build_version(&repo, "0.11.0+dev", &scratch);
    assert_eq!(clean_ver, format!("0.11.0+dev.{head}"));
    assert!(!clean_watched.contains(&repo), "repo root must not be watched directly (Cargo would scan target/ and .git/; M2)");
    assert!(clean_watched.contains(&repo.join("src")), "source subdirectory must be watched");

    // 1b. `git update-index --assume-unchanged` must NEVER hide modified bytes
    //     as clean (M1).
    git(&repo, &["update-index", "--assume-unchanged", "src/main.rs"]);
    fs::write(repo.join("src/main.rs"), "fn main() { println!(\"changed\"); }\n").unwrap();
    let (assumed_dirty, _) = build_version::build_version(&repo, "0.11.0+dev", &scratch);
    assert!(assumed_dirty.starts_with(&format!("0.11.0+dev.{head}.dirty.")), "assume-unchanged edit must report dirty (M1), got {assumed_dirty}");
    git(&repo, &["update-index", "--no-assume-unchanged", "src/main.rs"]);
    fs::write(repo.join("src/main.rs"), "fn main() {}\n").unwrap();

    // 1c. Force-added ignored file (`git add -f`) in the real index must also
    //     mark the tree dirty even though HEAD does not track it (M1).
    fs::write(repo.join(".gitignore"), "ignored.rs\n").unwrap();
    git(&repo, &["add", ".gitignore"]);
    git(&repo, &["commit", "-qm", "add gitignore"]);
    let head = git(&repo, &["rev-parse", "HEAD"]);
    fs::write(repo.join("ignored.rs"), "pub fn extra() {}\n").unwrap();
    git(&repo, &["add", "-f", "ignored.rs"]);
    let (forced_dirty, _) = build_version::build_version(&repo, "0.11.0+dev", &scratch);
    assert!(forced_dirty.starts_with(&format!("0.11.0+dev.{head}.dirty.")), "force-added ignored file must report dirty (M1), got {forced_dirty}");
    git(&repo, &["rm", "-q", "--cached", "ignored.rs"]);
    fs::remove_file(repo.join("ignored.rs")).unwrap();

    // 2. Dirty untracked file inside a watched source subdirectory must mark
    //    `.dirty.<tree>` WITHOUT writing ANY object into `.git/objects` (M17),
    //    whereas an unstaged untracked root file stays outside the incremental
    //    stamp contract until staged (R3 M3).
    let objects_before = list_object_files(&repo.join(".git/objects"));
    let root_scratch = repo.join("root-scratch.txt");
    fs::write(&root_scratch, b"unstaged root note").unwrap();
    let (root_scratch_ver, _) = build_version::build_version(&repo, "0.11.0+dev", &scratch);
    assert_eq!(root_scratch_ver, format!("0.11.0+dev.{head}"), "unstaged untracked root file must match Cargo's incremental watch set (R3 M3)");
    fs::remove_file(&root_scratch).unwrap();

    let untracked = repo.join("src/untracked secret.txt");
    fs::write(&untracked, b"never write me to .git/objects").unwrap();
    let (dirty_ver, dirty_watched) = build_version::build_version(&repo, "0.11.0+dev", &scratch);
    let objects_after = list_object_files(&repo.join(".git/objects"));
    assert_eq!(objects_before, objects_after, "build_version must not write uncommitted files into .git/objects (M17)");
    assert!(dirty_ver.starts_with(&format!("0.11.0+dev.{head}.dirty.")), "got {dirty_ver}");
    assert!(dirty_watched.contains(&untracked), "untracked file with spaces in subdir must be watched via NUL-delimited ls-files (M17)");
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
    let dir = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let sha = "0123456789abcdef0123456789abcdef01234567";

    // Missing VCS info returns bare manifest version.
    let (v, _) = build_version::build_version(dir.path(), "0.11.0+dev", scratch.path());
    assert_eq!(v, "0.11.0+dev");

    // Clean packaged dev build appends full 40-hex SHA, even alongside escaped
    // non-BMP UTF-16 surrogate pairs in an unrelated JSON string (M3).
    fs::write(dir.path().join(".cargo_vcs_info.json"), format!("{{\"note\":\"\\uD83D\\uDE00\",\"git\":{{\"sha1\":\"{sha}\"}},\"path_in_vcs\":\"\"}}")).unwrap();
    let (v, _) = build_version::build_version(dir.path(), "0.11.0+dev", scratch.path());
    assert_eq!(v, format!("0.11.0+dev.{sha}"));

    // Dirty packaged release build appends `.sha.dirty`.
    fs::write(dir.path().join(".cargo_vcs_info.json"), format!("{{\"git\":{{\"sha1\":\"{sha}\",\"dirty\":true}},\"path_in_vcs\":\"\"}}")).unwrap();
    let (v, _) = build_version::build_version(dir.path(), &format!("0.11.0+{sha}"), scratch.path());
    assert_eq!(v, format!("0.11.0+{sha}.{sha}.dirty"));

    // Truncated / malformed SHA, truncated JSON, and trailing garbage report
    // `.unverified` rather than looking like clean release provenance (M3/M16).
    fs::write(dir.path().join(".cargo_vcs_info.json"), "{\"git\":{\"sha1\":\"deadbeef\"},\"path_in_vcs\":\"\"}").unwrap();
    let (v, _) = build_version::build_version(dir.path(), "0.11.0+dev", scratch.path());
    assert_eq!(v, "0.11.0+dev.unverified");

    // Truncated JSON after sha1 must be flagged unverified.
    fs::write(dir.path().join(".cargo_vcs_info.json"), format!("{{\"git\":{{\"sha1\":\"{sha}\"")).unwrap();
    let (v, _) = build_version::build_version(dir.path(), "0.11.0+dev", scratch.path());
    assert_eq!(v, "0.11.0+dev.unverified");

    // Trailing garbage after closing brace must be flagged unverified.
    fs::write(dir.path().join(".cargo_vcs_info.json"), format!("{{\"git\":{{\"sha1\":\"{sha}\"}}}} garbage")).unwrap();
    let (v, _) = build_version::build_version(dir.path(), "0.11.0+dev", scratch.path());
    assert_eq!(v, "0.11.0+dev.unverified");

    // Top-level `"dirty": false` before `"git": {"dirty": true}` must NOT mask git.dirty!
    fs::write(dir.path().join(".cargo_vcs_info.json"), format!("{{\"dirty\":false,\"git\":{{\"sha1\":\"{sha}\",\"dirty\":true}}}}")).unwrap();
    let (v, _) = build_version::build_version(dir.path(), &format!("0.11.0+{sha}"), scratch.path());
    assert_eq!(v, format!("0.11.0+{sha}.{sha}.dirty"));

    // Invalid UTF-8 bytes in a present `.cargo_vcs_info.json` must report
    // `.unverified`, NOT look like a missing file / clean provenance (M3).
    fs::write(dir.path().join(".cargo_vcs_info.json"), [0xFFu8, 0xFEu8]).unwrap();
    let (v, _) = build_version::build_version(dir.path(), &format!("0.11.0+{sha}"), scratch.path());
    assert_eq!(v, format!("0.11.0+{sha}.unverified"));

    // Leading `+` in a `\u` escape is not valid JSON and must report `.unverified` (M3).
    fs::write(dir.path().join(".cargo_vcs_info.json"), format!("{{\"note\":\"\\u+041\",\"git\":{{\"sha1\":\"{sha}\"}}}}")).unwrap();
    let (v, _) = build_version::build_version(dir.path(), &format!("0.11.0+{sha}"), scratch.path());
    assert_eq!(v, format!("0.11.0+{sha}.unverified"));
}

#[test]
fn incremental_cargo_build_agrees_with_fresh_build_across_root_and_subdir_changes() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let root = tempfile::tempdir().unwrap();
    let repo = root.path().join("fixture");
    let target_inc = root.path().join("target-inc");
    fs::create_dir_all(repo.join("src")).unwrap();
    fs::write(repo.join("Cargo.toml"), "[package]\nname = \"stamp_fixture\"\nversion = \"0.11.0+dev\"\nedition = \"2024\"\n").unwrap();
    fs::copy(manifest_dir.join("build.rs"), repo.join("build.rs")).unwrap();
    fs::copy(manifest_dir.join("build_version.rs"), repo.join("build_version.rs")).unwrap();
    fs::write(repo.join("src/main.rs"), "fn main() { print!(\"{}\", env!(\"B3SUM_BUILD_VERSION\")); }\n").unwrap();
    git(&repo, &["init", "-q", "--initial-branch=main"]);
    git(&repo, &["config", "user.name", "Test"]);
    git(&repo, &["config", "user.email", "test@example.invalid"]);
    git(&repo, &["config", "commit.gpgsign", "false"]);
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "initial"]);
    let head = git(&repo, &["rev-parse", "HEAD"]);

    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let build_and_read = |target_dir: &Path| -> String {
        let out = Command::new(&cargo).args(["build", "--quiet", "--offline"]).current_dir(&repo).env("CARGO_TARGET_DIR", target_dir).output().unwrap();
        assert!(out.status.success(), "cargo build failed: {}", String::from_utf8_lossy(&out.stderr));
        let bin = target_dir.join("debug/stamp_fixture");
        let run = Command::new(&bin).output().unwrap();
        assert!(run.status.success());
        String::from_utf8(run.stdout).unwrap()
    };
    let assert_inc_matches_fresh = |label: &str| -> String {
        let inc = build_and_read(&target_inc);
        let fresh_dir = tempfile::tempdir().unwrap();
        let fresh = build_and_read(fresh_dir.path());
        assert_eq!(inc, fresh, "incremental vs fresh mismatch ({label})");
        inc
    };

    // 1. Clean checkout.
    let v1 = assert_inc_matches_fresh("clean");
    assert_eq!(v1, format!("0.11.0+dev.{head}"));

    // 2. Unstaged untracked root scratch file: both incremental and fresh stay clean (R3 M3).
    let scratch_root = repo.join("scratch-note.txt");
    fs::write(&scratch_root, "local root scratch\n").unwrap();
    let v2 = assert_inc_matches_fresh("unstaged root file");
    assert_eq!(v2, format!("0.11.0+dev.{head}"));

    // 3. Staging that root file updates `.git/index`, so incremental flips to dirty immediately.
    git(&repo, &["add", "scratch-note.txt"]);
    let v3 = assert_inc_matches_fresh("staged root file");
    assert!(v3.starts_with(&format!("0.11.0+dev.{head}.dirty.")), "got {v3}");
    git(&repo, &["rm", "-q", "--cached", "scratch-note.txt"]);
    fs::remove_file(&scratch_root).unwrap();
    assert_eq!(assert_inc_matches_fresh("restored clean"), format!("0.11.0+dev.{head}"));

    // 4. Adding an untracked file inside watched `src/` flips incremental to dirty immediately.
    let sub_extra = repo.join("src/extra.rs");
    fs::write(&sub_extra, "pub fn extra() {}\n").unwrap();
    let v4 = assert_inc_matches_fresh("untracked subdir file");
    assert!(v4.starts_with(&format!("0.11.0+dev.{head}.dirty.")), "got {v4}");
    fs::remove_file(&sub_extra).unwrap();

    // 5. Editing a tracked root file (`Cargo.toml`) flips incremental to dirty immediately.
    let orig_toml = fs::read_to_string(repo.join("Cargo.toml")).unwrap();
    fs::write(repo.join("Cargo.toml"), format!("{orig_toml}# tracked root edit\n")).unwrap();
    let v5 = assert_inc_matches_fresh("edited tracked root file");
    assert!(v5.starts_with(&format!("0.11.0+dev.{head}.dirty.")), "got {v5}");
    fs::write(repo.join("Cargo.toml"), &orig_toml).unwrap();

    // 6. Exported foreign `GIT_DIR` must NOT spoof `build_version`'s commit.
    let foreign = root.path().join("foreign");
    fs::create_dir_all(&foreign).unwrap();
    let foreign_head = init_repo(&foreign);
    assert_ne!(foreign_head, head);
    let foreign_target = tempfile::tempdir().unwrap();
    let out = Command::new(&cargo)
        .args(["build", "--quiet", "--offline"])
        .current_dir(&repo)
        .env("CARGO_TARGET_DIR", foreign_target.path())
        .env("GIT_DIR", foreign.join(".git"))
        .output()
        .unwrap();
    assert!(out.status.success(), "cargo build with foreign GIT_DIR failed: {}", String::from_utf8_lossy(&out.stderr));
    let run = Command::new(foreign_target.path().join("debug/stamp_fixture")).output().unwrap();
    assert!(run.status.success());
    assert_eq!(String::from_utf8(run.stdout).unwrap(), format!("0.11.0+dev.{head}"), "foreign GIT_DIR must not spoof build_version");
}
