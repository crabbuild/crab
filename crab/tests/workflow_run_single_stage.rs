//! Integration tests for `crab run` — single-stage mode.
//!
//! Drives the real `crab` binary via `Command::new(env!("CARGO_BIN_EXE_crab"))`
//! so this is an end-to-end happy-path smoke test of CLI parsing, the
//! workflow feature flag, stage hashing, journal writes, and cache
//! materialization.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]

use std::fs;
use std::process::Command;

use tempfile::TempDir;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_crab")
}

/// Happy path: copy a dep to an out via `crab run` and verify the out
/// file exists with the expected bytes.
#[test]
fn run_single_stage_copies_dep_to_out() {
    let tmp = TempDir::new().unwrap();

    // Seed a dep.
    fs::write(tmp.path().join("a.txt"), b"payload").unwrap();

    // Run `crab run` with workflow enabled via env var.
    let status = Command::new(bin())
        .current_dir(tmp.path())
        .env("CRAB_WORKFLOW_ENABLED", "1")
        .args([
            "run", "--name", "copy", "--deps", "a.txt", "--outs", "b.txt", "--", "/bin/cp",
            "a.txt", "b.txt",
        ])
        .status()
        .expect("crab run should spawn");
    assert!(status.success(), "crab run failed: {status:?}");

    // Verify the out file was produced.
    let out = fs::read(tmp.path().join("b.txt")).expect("b.txt must exist");
    assert_eq!(out, b"payload".to_vec());

    // Verify the stage cache landed on disk. The hex sharding follows
    // `stages/{2-char}/<hex>.json` from `cache::entry_path`.
    let stages_dir = tmp.path().join(".crab/cache/stages");
    assert!(
        stages_dir.exists(),
        "stage cache dir must be created: {}",
        stages_dir.display()
    );

    // Verify the run journal landed on disk.
    let runs_dir = tmp.path().join(".crab/workflow/runs");
    assert!(
        runs_dir.exists(),
        "run journal dir must be created: {}",
        runs_dir.display()
    );
    let first_run = fs::read_dir(&runs_dir)
        .unwrap()
        .next()
        .expect("at least one run recorded")
        .unwrap();
    assert!(first_run.path().join("journal.db").exists());
}

/// An explicit environment opt-out keeps the workflow command inert.
#[test]
fn run_fails_with_workflow_disabled() {
    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join("a.txt"), b"x").unwrap();

    let output = Command::new(bin())
        .current_dir(tmp.path())
        // The shipped default is enabled; exercise the explicit opt-out.
        .env("CRAB_WORKFLOW_ENABLED", "0")
        .args([
            "run",
            "--name",
            "copy",
            "--deps",
            "a.txt",
            "--outs",
            "b.txt",
            "--",
            "/bin/true",
        ])
        .output()
        .expect("crab run should spawn");

    assert!(
        !output.status.success(),
        "expected failure when workflow disabled: {output:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("workflow feature is disabled") || stderr.contains("CRAB-E0231"),
        "expected WorkflowDisabled error, got stderr: {stderr}"
    );
}

/// `--dry-run` prints a plan and does not execute the command.
#[test]
fn run_dry_run_does_not_execute() {
    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join("a.txt"), b"x").unwrap();

    let output = Command::new(bin())
        .current_dir(tmp.path())
        .env("CRAB_WORKFLOW_ENABLED", "1")
        .args([
            "run",
            "--name",
            "copy",
            "--deps",
            "a.txt",
            "--outs",
            "b.txt",
            "--dry-run",
            "--json",
            "--",
            "/bin/cp",
            "a.txt",
            "b.txt",
        ])
        .output()
        .expect("crab run should spawn");

    assert!(
        output.status.success(),
        "dry-run should succeed: {output:?}"
    );

    // The JSON envelope must contain the stage hash.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value =
        serde_json::from_str(stdout.trim()).expect("dry-run JSON must parse");
    assert_eq!(json["schema"], "workflow.plan");
    assert!(json["data"]["stage_hash"].is_string());

    // b.txt must still be absent.
    assert!(
        !tmp.path().join("b.txt").exists(),
        "dry-run must not execute the command"
    );
}
