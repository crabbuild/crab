//! Integration tests for `crab workflow journal`.
//!
//! Drives the CLI binary end-to-end for the `show`, `ls`, and `gc`
//! subcommands. The tests seed journals via `crab run` so the
//! on-disk layout matches what a real user would produce — nothing
//! is faked at the SQLite layer.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_crab")
}

/// Write the minimal config that opts into the workflow layer so
/// `crab run` doesn't bail with `WorkflowDisabled`.
fn enable_workflow(repo: &Path) {
    fs::create_dir_all(repo.join(".crab")).unwrap();
    fs::write(
        repo.join(".crab").join("local.toml"),
        "[workflow]\nenabled = true\n",
    )
    .unwrap();
}

/// Execute `crab <args>` in `repo`. Returns the full `Output`
/// struct so callers can assert on stdout, stderr, and exit code.
fn run(repo: &Path, args: &[&str]) -> Output {
    let mut cmd = Command::new(bin());
    cmd.current_dir(repo)
        .env("CRAB_WORKFLOW_ENABLED", "1")
        .args(args);
    cmd.output().expect("crab should spawn")
}

/// Seed a single-stage journal by running `crab run` with an
/// inline copy command. `stage_name` is used for the `--name` flag
/// so each seeded run produces a distinct stage_hash; callers that
/// seed multiple runs in the same test pass different names.
/// Returns the single run_id written under `.crab/workflow/runs/`.
fn seed_one_run(repo: &Path, stage_name: &str, src: &str, dst: &str) -> String {
    fs::write(repo.join(src), stage_name.as_bytes()).unwrap();
    let output = run(
        repo,
        &[
            "run",
            "--name",
            stage_name,
            "--deps",
            src,
            "--outs",
            dst,
            "--",
            "sh",
            "-c",
            &format!("cp {src} {dst}"),
        ],
    );
    assert!(
        output.status.success(),
        "seed run for '{stage_name}' failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    // Return the most recent run_id — the runs directory for this
    // repo only contains journals we seeded ourselves.
    latest_run_id(repo)
}

fn latest_run_id(repo: &Path) -> String {
    let mut entries: Vec<PathBuf> = fs::read_dir(runs_dir(repo))
        .expect("runs dir should exist")
        .map(|e| e.unwrap().path())
        .collect();
    // UUIDv7 sorts chronologically, so the lexicographically-largest
    // name is the most recent run.
    entries.sort();
    entries
        .pop()
        .expect("at least one run_id directory")
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

fn runs_dir(repo: &Path) -> PathBuf {
    repo.join(".crab").join("workflow").join("runs")
}

#[test]
fn ls_on_fresh_repo_reports_no_journals() {
    let tmp = TempDir::new().unwrap();
    enable_workflow(tmp.path());

    let output = run(tmp.path(), &["workflow", "journal", "ls", "--json"]);
    assert!(
        output.status.success(),
        "ls failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let v: Value = serde_json::from_slice(&output.stdout).unwrap();
    let journals = v.pointer("/data/journals").unwrap().as_array().unwrap();
    assert!(journals.is_empty(), "expected empty journals, got: {v:#}",);
}

#[test]
fn ls_lists_seeded_journal_with_success_outcome() {
    let tmp = TempDir::new().unwrap();
    enable_workflow(tmp.path());
    let run_id = seed_one_run(tmp.path(), "copy", "a.txt", "b.txt");

    let output = run(tmp.path(), &["workflow", "journal", "ls", "--json"]);
    assert!(output.status.success());
    let v: Value = serde_json::from_slice(&output.stdout).unwrap();
    let journals = v.pointer("/data/journals").unwrap().as_array().unwrap();
    let mine = journals
        .iter()
        .find(|j| j.get("run_id").and_then(Value::as_str) == Some(run_id.as_str()))
        .expect("seeded run_id should be listed");
    // A clean `crab run` ends with a Success outcome once the
    // scheduler lock is released and `mark_run_outcome` runs.
    let outcome = mine.get("outcome").and_then(Value::as_str).unwrap();
    assert!(
        outcome == "success" || outcome == "in_flight",
        "unexpected outcome: {outcome}"
    );
}

#[test]
fn show_emits_stage_rows_for_seeded_run() {
    let tmp = TempDir::new().unwrap();
    enable_workflow(tmp.path());
    let run_id = seed_one_run(tmp.path(), "copy", "a.txt", "b.txt");

    let output = run(
        tmp.path(),
        &["workflow", "journal", "show", &run_id, "--json"],
    );
    assert!(
        output.status.success(),
        "show failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let v: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        v.pointer("/data/run_id").and_then(Value::as_str),
        Some(run_id.as_str())
    );
    let stages = v.pointer("/data/stages").unwrap().as_array().unwrap();
    assert!(!stages.is_empty(), "expected at least one stage row");
    // Single-stage runs walk from Resolving through Committed; the
    // final recorded state for a successful run is Committed.
    let states: Vec<&str> = stages
        .iter()
        .filter_map(|s| s.get("state").and_then(Value::as_str))
        .collect();
    assert!(
        states.iter().any(|s| *s == "Committed"),
        "expected Committed state in trajectory: {states:?}"
    );
}

#[test]
fn show_missing_run_id_exits_nonzero() {
    let tmp = TempDir::new().unwrap();
    enable_workflow(tmp.path());
    let phantom = "00000000-0000-0000-0000-000000000001";

    let output = run(
        tmp.path(),
        &["workflow", "journal", "show", phantom, "--json"],
    );
    assert!(
        !output.status.success(),
        "show on missing run_id should fail; stdout={}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn gc_keep_one_removes_older_terminal_runs() {
    let tmp = TempDir::new().unwrap();
    enable_workflow(tmp.path());

    // Seed two runs with distinct names so their stage hashes
    // differ and both produce fresh journals.
    let _id1 = seed_one_run(tmp.path(), "first", "a.txt", "a.out");

    // Small sleep so the second UUIDv7 sorts strictly after the
    // first when we come to compare.
    std::thread::sleep(std::time::Duration::from_millis(20));

    let _id2 = seed_one_run(tmp.path(), "second", "b.txt", "b.out");

    // Count seeded run_id directories.
    let before: Vec<PathBuf> = fs::read_dir(runs_dir(tmp.path()))
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    assert!(
        before.len() >= 2,
        "expected at least two run directories, got {}",
        before.len()
    );

    // Dry-run first: nothing removed on disk.
    let dry = run(
        tmp.path(),
        &[
            "workflow",
            "journal",
            "gc",
            "--keep",
            "1",
            "--dry-run",
            "--json",
        ],
    );
    assert!(dry.status.success());
    let after_dry: usize = fs::read_dir(runs_dir(tmp.path())).unwrap().count();
    assert_eq!(after_dry, before.len(), "--dry-run should not delete");

    // Real gc: keep only the newest terminal run.
    let gc = run(
        tmp.path(),
        &["workflow", "journal", "gc", "--keep", "1", "--json"],
    );
    assert!(
        gc.status.success(),
        "gc failed: stderr={}",
        String::from_utf8_lossy(&gc.stderr)
    );
    let v: Value = serde_json::from_slice(&gc.stdout).unwrap();
    let removed = v.pointer("/data/removed").unwrap().as_array().unwrap();
    let kept = v.pointer("/data/kept").unwrap().as_array().unwrap();
    assert_eq!(kept.len(), 1, "should keep exactly one: {v:#}");
    assert!(!removed.is_empty(), "should have removed older journals");

    // Filesystem matches the report.
    let after: usize = fs::read_dir(runs_dir(tmp.path())).unwrap().count();
    assert_eq!(after, kept.len(), "disk count should match kept count",);
}
