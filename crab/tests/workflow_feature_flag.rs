//! Integration tests for the `[workflow] enabled` feature flag.
//!
//! Covers the disable → re-enable life cycle that a cautious operator
//! runs when they want to opt out of the workflow layer mid-project
//! and later opt back in:
//!
//! 1. With workflow disabled, `crab run` refuses with the
//!    `WorkflowDisabled` error and leaves no workflow state on disk.
//! 2. Stock commands (`crab status`) keep working while the flag
//!    is off — they never touch `.crab/workflow/` and never gate on
//!    the flag.
//! 3. Re-enabling the flag lets `crab run` proceed again, picks up
//!    the existing stage cache, and replays from it.
//!
//! The tests drive the real `crab` binary (`CARGO_BIN_EXE_crab`)
//! and use the TOML config file + explicit `env_remove` on the
//! `CRAB_WORKFLOW_ENABLED` override so the config's `enabled` value
//! is what actually flips behavior — otherwise the env override from
//! the surrounding test harness could silently mask a regression.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use tempfile::TempDir;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_crab")
}

/// Write `.crab/local.toml` with the requested `[workflow] enabled`
/// value. Any prior file at that path is replaced.
fn write_workflow_config(repo: &Path, enabled: bool) {
    let cfg_dir = repo.join(".crab");
    fs::create_dir_all(&cfg_dir).unwrap();
    let body = format!("[workflow]\nenabled = {enabled}\n");
    fs::write(cfg_dir.join("local.toml"), body).unwrap();
}

/// Invoke `crab run` for the canonical single-stage `cp a.txt b.txt`
/// fixture. The env override is explicitly cleared so the TOML config
/// drives the feature-flag decision — the whole point of this suite.
fn crab_run(repo: &Path) -> Output {
    Command::new(bin())
        .current_dir(repo)
        .env_remove("CRAB_WORKFLOW_ENABLED")
        .args([
            "run", "--name", "copy", "--deps", "a.txt", "--outs", "b.txt", "--", "/bin/cp",
            "a.txt", "b.txt",
        ])
        .output()
        .expect("crab run should spawn")
}

/// Invoke the stock hydration-oriented `crab status` with `--json`.
/// Same env-override rule as `crab_run`: TOML wins.
fn crab_status(repo: &Path) -> Output {
    Command::new(bin())
        .current_dir(repo)
        .env_remove("CRAB_WORKFLOW_ENABLED")
        .args(["status", "--json"])
        .output()
        .expect("crab status should spawn")
}

/// Seed a minimal fixture: a dep file plus workflow config at the
/// requested enabled state.
fn scaffold(repo: &Path, enabled: bool) {
    fs::write(repo.join("a.txt"), b"payload-v1").unwrap();
    write_workflow_config(repo, enabled);
}

/// With workflow disabled via TOML, `crab run` fails cleanly with
/// the `WorkflowDisabled` error (CRAB-E0231) and produces no
/// workflow state on disk.
#[test]
fn workflow_disabled_run_errors_cleanly() {
    let tmp = TempDir::new().unwrap();
    scaffold(tmp.path(), false);

    let out = crab_run(tmp.path());
    assert!(
        !out.status.success(),
        "run must fail when workflow is disabled: status={:?}, stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("CRAB-E0231") || stderr.contains("workflow feature is disabled"),
        "expected WorkflowDisabled diagnostic; stderr={stderr}"
    );

    // No side effects: the refusal happens before any journal is
    // created, so `.crab/workflow/` must not exist.
    assert!(
        !tmp.path().join(".crab/workflow").exists(),
        "disabled run must not create .crab/workflow state"
    );
    assert!(
        !tmp.path().join("b.txt").exists(),
        "disabled run must not materialize outs"
    );
}

/// Stock commands must keep working regardless of the workflow flag.
/// We pick `crab status` because it's a stock command the user
/// typically runs while evaluating the workflow layer, and because
/// it doesn't need a real git repo — which keeps the test focused on
/// the feature-flag contract rather than git setup.
#[test]
fn workflow_disabled_status_still_works() {
    let tmp = TempDir::new().unwrap();
    scaffold(tmp.path(), false);

    let out = crab_status(tmp.path());
    assert!(
        out.status.success(),
        "stock `crab status` must work with workflow disabled: \
         status={:?}, stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );

    // The status envelope is the usual `schema: status` JSON — parsing
    // it confirms the command ran through its normal path rather than
    // short-circuiting on the workflow gate.
    let stdout = String::from_utf8_lossy(&out.stdout);
    let envelope: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("parse status envelope failed: {e}; stdout={stdout:?}"));
    assert_eq!(envelope["schema"], "status");
}

/// Full toggle life cycle: enabled → run populates the lockfile →
/// disabled → stock commands still pass → re-enabled → next `run`
/// reads the existing lockfile and replays from cache without
/// re-executing the child.
#[test]
fn reenabling_workflow_reads_existing_lockfile() {
    let tmp = TempDir::new().unwrap();
    scaffold(tmp.path(), true);

    // Phase 1: enabled — first run populates the stage cache and
    // the lockfile lives under the workflow layer's in-memory
    // representation (single-stage mode doesn't emit crab.lock,
    // but it does emit a cache entry under .crab/cache/stages/).
    let first = crab_run(tmp.path());
    assert!(
        first.status.success(),
        "first run (enabled) must succeed: stderr={}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(
        fs::read(tmp.path().join("b.txt")).unwrap(),
        b"payload-v1".to_vec()
    );
    let stages_dir = tmp.path().join(".crab/cache/stages");
    assert!(
        stages_dir.exists(),
        "stage cache must be seeded after enabled run"
    );
    let runs_dir = tmp.path().join(".crab/workflow/runs");
    let first_run_count = fs::read_dir(&runs_dir).unwrap().count();
    assert!(first_run_count >= 1, "one journal after first run");

    // Phase 2: disabled — the flag flips, stock commands keep
    // working, `crab run` refuses, the workflow state on disk is
    // preserved untouched.
    write_workflow_config(tmp.path(), false);
    let status = crab_status(tmp.path());
    assert!(
        status.status.success(),
        "status must keep working while workflow is disabled: stderr={}",
        String::from_utf8_lossy(&status.stderr)
    );
    let disabled = crab_run(tmp.path());
    assert!(
        !disabled.status.success(),
        "run must refuse while workflow is disabled"
    );
    assert!(
        stages_dir.exists(),
        "disabling workflow must not delete pre-existing stage cache"
    );
    let disabled_run_count = fs::read_dir(&runs_dir).unwrap().count();
    assert_eq!(
        disabled_run_count, first_run_count,
        "disabled run must not create a new journal directory"
    );

    // Phase 3: re-enabled — run sees the pre-existing cache entry
    // and materializes from it. We assert the cache hit by checking
    // that no new per-stage supervisor log was created on this run
    // (the executor only spawns the child on a miss).
    write_workflow_config(tmp.path(), true);
    let second = crab_run(tmp.path());
    assert!(
        second.status.success(),
        "re-enabled run must succeed: stderr={}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(
        fs::read(tmp.path().join("b.txt")).unwrap(),
        b"payload-v1".to_vec(),
        "cache-hit materialization must produce the original bytes"
    );

    let second_run_count = fs::read_dir(&runs_dir).unwrap().count();
    assert!(
        second_run_count > first_run_count,
        "re-enabled run must create a new journal"
    );

    // Locate the newest run directory and confirm no supervisor log
    // was emitted — proof the cache hit short-circuited before any
    // child process was spawned.
    let newest_run = fs::read_dir(&runs_dir)
        .unwrap()
        .filter_map(Result::ok)
        .max_by_key(|e| e.file_name())
        .expect("at least one run dir");
    let log_path = newest_run.path().join("stage-copy.log");
    assert!(
        !log_path.exists(),
        "re-enabled cache-hit run MUST NOT spawn the child (no supervisor log expected at {})",
        log_path.display()
    );
}
