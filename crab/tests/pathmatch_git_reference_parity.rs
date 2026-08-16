//! Git-reference parity for the consolidated pathspec engine.
//!
//! Runs `git ls-files -- '*.bin'` against a fixture repo and compares
//! the set against what the `core::pathmatch::PatternFilter` matches
//! over the same file list. The point is to make the "matches git's
//! behavior" user story executable: if a user hits a case where
//! `crab add '*.bin'` and `git ls-files -- '*.bin'` disagree, a
//! regression here is the bug.

#![cfg(feature = "gix-pathmatch")]

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use std::process::Command;

use crab::core::pathmatch::build_filter;

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn init_fixture(dir: &Path, files: &[&str]) {
    let status = Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .current_dir(dir)
        .status()
        .expect("git init");
    assert!(status.success(), "git init failed");

    // Isolate from the user's global git config to avoid "Please tell me
    // who you are" and to keep filter=crab-style attributes inert.
    let _ = Command::new("git")
        .args(["config", "user.email", "test@example.com"])
        .current_dir(dir)
        .status();
    let _ = Command::new("git")
        .args(["config", "user.name", "Test"])
        .current_dir(dir)
        .status();

    for rel in files {
        let p = dir.join(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&p, b"x").unwrap();
    }

    // Stage & commit so `git ls-files` returns something deterministic.
    let status = Command::new("git")
        .args(["add", "-A"])
        .current_dir(dir)
        .status()
        .expect("git add");
    assert!(status.success(), "git add failed");
    let status = Command::new("git")
        .args(["commit", "-q", "-m", "fixture"])
        .current_dir(dir)
        .status()
        .expect("git commit");
    assert!(status.success(), "git commit failed");
}

fn git_ls_files(dir: &Path, pathspec: &str) -> BTreeSet<String> {
    let out = Command::new("git")
        .args(["ls-files", "--", pathspec])
        .current_dir(dir)
        .output()
        .expect("git ls-files");
    assert!(out.status.success(), "git ls-files failed");
    String::from_utf8(out.stdout)
        .expect("utf8 output")
        .lines()
        .map(|s| s.to_owned())
        .collect()
}

#[test]
fn filter_matches_git_ls_files_for_extension_glob() {
    if !git_available() {
        eprintln!("git not on PATH, skipping");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let files = [
        "a.bin",
        "dir/b.bin",
        "deep/nested/c.bin",
        "readme.md",
        "docs/notes.txt",
    ];
    init_fixture(root, &files);

    let git_set = git_ls_files(root, "*.bin");
    let filter = build_filter(&["*.bin".to_owned()], &[]).expect("build filter");

    let via_filter: BTreeSet<String> = files
        .iter()
        .filter(|p| filter.matches(p))
        .map(|s| (*s).to_owned())
        .collect();

    assert_eq!(
        via_filter, git_set,
        "crab pathspec filter diverged from `git ls-files -- '*.bin'`"
    );
}

#[test]
fn filter_matches_git_ls_files_for_directory_prefix() {
    if !git_available() {
        eprintln!("git not on PATH, skipping");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let files = [
        "models/v1/a.bin",
        "models/config.json",
        "data/models/b.bin",
        "readme.md",
    ];
    init_fixture(root, &files);

    let git_set = git_ls_files(root, "models/**");
    let filter = build_filter(&["models/**".to_owned()], &[]).expect("build filter");

    let via_filter: BTreeSet<String> = files
        .iter()
        .filter(|p| filter.matches(p))
        .map(|s| (*s).to_owned())
        .collect();

    assert_eq!(
        via_filter, git_set,
        "crab pathspec filter diverged from `git ls-files -- 'models/**'`"
    );
}
