//! Integration tests for `crab params diff` and
//! `crab metrics diff`.
//!
//! Drives the real `crab` binary against a tempdir git repo with
//! two branches whose `params.yaml` / `metrics.json` differ, and
//! checks the CLI output. Uses `std::process::Command` to run
//! `git` for setup because constructing commits via gitoxide is
//! unnecessary overhead for a test harness — production code paths
//! use gitoxide for blob reads.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]

use std::path::Path;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_crab")
}

fn git_available() -> bool {
    Command::new("git")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(repo)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed");
}

fn git_init(repo: &Path) {
    git(repo, &["init", "--initial-branch=main"]);
    git(repo, &["config", "user.email", "t@test.com"]);
    git(repo, &["config", "user.name", "Test"]);
    git(repo, &["config", "commit.gpgsign", "false"]);
}

/// Commit `{params.yaml with {model: {lr: 0.01}, epochs: 5}}` on
/// branch `a`, then check out `b` and commit
/// `{model: {lr: 0.02}, epochs: 5, dropout: 0.3}`.
fn setup_two_branch_repo(repo: &Path) {
    git_init(repo);

    std::fs::write(repo.join("params.yaml"), b"model:\n  lr: 0.01\nepochs: 5\n").unwrap();
    git(repo, &["add", "params.yaml"]);
    git(repo, &["commit", "-m", "init params on main"]);
    git(repo, &["checkout", "-b", "a"]);

    git(repo, &["checkout", "main"]);
    git(repo, &["checkout", "-b", "b"]);
    std::fs::write(
        repo.join("params.yaml"),
        b"model:\n  lr: 0.02\nepochs: 5\ndropout: 0.3\n",
    )
    .unwrap();
    git(repo, &["commit", "-am", "bump params on b"]);
}

#[test]
fn params_diff_table_snapshot() {
    if !git_available() {
        eprintln!("skipping: git not on PATH");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    setup_two_branch_repo(repo);

    let output = Command::new(bin())
        .current_dir(repo)
        .args(["params", "diff", "a", "b"])
        .output()
        .expect("crab params diff");
    assert!(
        output.status.success(),
        "params diff failed: {:?}\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("path"), "expected path column: {stdout}");
    assert!(stdout.contains("param"), "expected param column: {stdout}");
    assert!(
        stdout.contains("params.yaml"),
        "expected params path: {stdout}"
    );
    assert!(stdout.contains("dropout"), "expected added param: {stdout}");
    assert!(
        stdout.contains("model.lr"),
        "expected changed param: {stdout}"
    );
    assert!(
        !stdout.contains("epochs"),
        "unchanged params should be hidden without --all: {stdout}"
    );
}

#[test]
fn params_diff_json_envelope() {
    if !git_available() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    setup_two_branch_repo(repo);

    let output = Command::new(bin())
        .current_dir(repo)
        .args(["params", "diff", "a", "b", "--json"])
        .output()
        .expect("crab params diff --json");
    assert!(output.status.success(), "params diff --json failed");

    let stdout = String::from_utf8(output.stdout).unwrap();
    let envelope: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("envelope parse: {e}; stdout={stdout}"));

    assert_eq!(envelope["schema"], "params.diff");
    assert_eq!(envelope["version"], "1.0");
    assert_eq!(envelope["data"]["ref_a"], "a");
    assert_eq!(envelope["data"]["ref_b"], "b");
    // `dropout` is present only in b → added.
    assert!(envelope["data"]["added"]["params.yaml"]["dropout"].is_number());
    // `model.lr` changed.
    assert!(envelope["data"]["changed"]["params.yaml"]["model.lr"].is_object());
    // `epochs` unchanged — must NOT appear in added/removed/changed.
    assert!(
        envelope["data"]["added"]["params.yaml"]
            .get("epochs")
            .is_none()
    );
    assert!(
        envelope["data"]["removed"]
            .get("params.yaml")
            .and_then(|path| path.get("epochs"))
            .is_none()
    );
    assert!(
        envelope["data"]["changed"]["params.yaml"]
            .get("epochs")
            .is_none()
    );
}

fn setup_metrics_two_branch_repo(repo: &Path) {
    git_init(repo);
    std::fs::write(
        repo.join("metrics.json"),
        br#"{"accuracy": 0.80, "loss": 0.50, "f1": 0.75}"#,
    )
    .unwrap();
    git(repo, &["add", "metrics.json"]);
    git(repo, &["commit", "-m", "baseline metrics"]);
    git(repo, &["checkout", "-b", "improved"]);
    std::fs::write(
        repo.join("metrics.json"),
        br#"{"accuracy": 0.85, "loss": 0.40, "f1": 0.78}"#,
    )
    .unwrap();
    git(repo, &["commit", "-am", "improved metrics"]);
}

#[test]
fn metrics_diff_table_snapshot() {
    if !git_available() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    setup_metrics_two_branch_repo(repo);

    let output = Command::new(bin())
        .current_dir(repo)
        .args(["metrics", "diff", "main", "improved"])
        .output()
        .expect("crab metrics diff");
    assert!(
        output.status.success(),
        "metrics diff failed: {:?}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("path"), "expected path column: {stdout}");
    assert!(
        stdout.contains("metric"),
        "expected metric column: {stdout}"
    );
    assert!(
        stdout.contains("metrics.json"),
        "expected metrics path: {stdout}"
    );
    assert!(
        stdout.contains("accuracy"),
        "expected accuracy row: {stdout}"
    );
    assert!(stdout.contains("loss"), "expected loss row: {stdout}");
    assert!(stdout.contains("+6.25%"), "expected percent gain: {stdout}");
    assert!(stdout.contains("-20%"), "expected percent drop: {stdout}");
}

#[test]
fn metrics_diff_pr_comment_marks_gains_and_regressions() {
    if !git_available() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    setup_metrics_two_branch_repo(repo);

    let output = Command::new(bin())
        .current_dir(repo)
        .args([
            "metrics",
            "diff",
            "main",
            "improved",
            "--format",
            "pr-comment",
        ])
        .output()
        .expect("crab metrics diff --format pr-comment");
    assert!(output.status.success(), "metrics diff pr-comment failed");

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("- + `metrics.json:accuracy`"),
        "expected positive delta marker in pr-comment output: {stdout}"
    );
    assert!(
        stdout.contains("- - `metrics.json:loss`"),
        "expected negative delta marker in pr-comment output: {stdout}"
    );
    assert!(
        stdout.contains("accuracy"),
        "expected accuracy metric: {stdout}"
    );
}
