//! Integration tests for `crab run --explain-miss` — field-by-field
//! diff between current resolved stage inputs and the lockfile's
//! recorded values.
//!
//! Drives the real `crab` binary and inspects the structured JSON
//! output to verify that changed deps, params, env vars, and cmd
//! produce the expected diff entries.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]

use std::fs;
use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_crab")
}

/// Run a stage without structured output, just for exit status.
fn run_stage(repo: &Path, args: &[&str]) -> std::process::ExitStatus {
    Command::new(bin())
        .current_dir(repo)
        .env("CRAB_WORKFLOW_ENABLED", "1")
        .args(args)
        .status()
        .expect("crab run should spawn")
}

/// Run with --explain-miss and --json, returning the parsed envelope.
fn run_explain_miss_json(repo: &Path, extra_args: &[&str]) -> serde_json::Value {
    let mut args = vec!["run", "--explain-miss", "--json"];
    args.extend_from_slice(extra_args);
    let output = Command::new(bin())
        .current_dir(repo)
        .env("CRAB_WORKFLOW_ENABLED", "1")
        .args(&args)
        .output()
        .expect("crab run should spawn");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    // --explain-miss emits the explain envelope first, then the
    // stage result envelope. We want the first one.
    let lines: Vec<&str> = stdout.trim().lines().collect();
    assert!(
        !lines.is_empty(),
        "expected at least one JSON line from --explain-miss --json; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_str(lines[0])
        .unwrap_or_else(|e| panic!("parse explain-miss envelope failed: {e}; line={}", lines[0]))
}

/// Missing lockfile entry reports "never run" with current hash.
#[test]
fn explain_miss_never_run_reports_reason() {
    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join("input.txt"), b"hello").unwrap();

    let envelope = run_explain_miss_json(
        tmp.path(),
        &[
            "--name",
            "process",
            "--deps",
            "input.txt",
            "--outs",
            "output.txt",
            "--",
            "/bin/cp",
            "input.txt",
            "output.txt",
        ],
    );

    // The envelope should have schema "workflow.explain_miss"
    assert_eq!(envelope["schema"], "workflow.explain_miss");

    let data = &envelope["data"]["data"];
    assert_eq!(data["stage"], "process");
    assert_eq!(data["reason"], "never run");
    assert!(
        data["stage_hash_current"]
            .as_str()
            .unwrap()
            .starts_with("b3:")
    );
    assert!(data["stage_hash_lockfile"].is_null());
    assert_eq!(data["diffs"], serde_json::json!([]));
}

/// Changed dep file shows category: "dep", key: path, old/new hashes.
#[test]
fn explain_miss_changed_dep_shows_diff() {
    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join("data.csv"), b"original content").unwrap();

    // First run: creates the lockfile entry.
    let status = run_stage(
        tmp.path(),
        &[
            "run",
            "--name",
            "train",
            "--deps",
            "data.csv",
            "--outs",
            "model.bin",
            "--",
            "/bin/cp",
            "data.csv",
            "model.bin",
        ],
    );
    assert!(status.success(), "first run should succeed");

    // Verify lockfile was created.
    assert!(
        tmp.path().join("crab.lock").exists(),
        "lockfile should exist after first run"
    );

    // Modify the dep file to trigger a cache miss.
    fs::write(tmp.path().join("data.csv"), b"modified content").unwrap();

    // Run with --explain-miss to get the diff.
    let envelope = run_explain_miss_json(
        tmp.path(),
        &[
            "--name",
            "train",
            "--deps",
            "data.csv",
            "--outs",
            "model.bin",
            "--",
            "/bin/cp",
            "data.csv",
            "model.bin",
        ],
    );

    assert_eq!(envelope["schema"], "workflow.explain_miss");
    let data = &envelope["data"]["data"];
    assert_eq!(data["stage"], "train");
    assert!(
        data["stage_hash_current"]
            .as_str()
            .unwrap()
            .starts_with("b3:")
    );
    assert!(
        data["stage_hash_lockfile"]
            .as_str()
            .unwrap()
            .starts_with("b3:")
    );
    assert_ne!(data["stage_hash_current"], data["stage_hash_lockfile"]);

    let diffs = data["diffs"].as_array().expect("diffs should be an array");
    assert!(!diffs.is_empty(), "should have at least one diff entry");

    // Find the dep diff entry.
    let dep_diff = diffs
        .iter()
        .find(|d| d["category"] == "dep" && d["key"] == "data.csv")
        .expect("should have a dep diff for data.csv");

    assert_eq!(dep_diff["category"], "dep");
    assert_eq!(dep_diff["key"], "data.csv");
    assert!(dep_diff["old"].as_str().unwrap().starts_with("b3:"));
    assert!(dep_diff["new"].as_str().unwrap().starts_with("b3:"));
    assert_ne!(dep_diff["old"], dep_diff["new"]);
}

/// Changed env var shows category: "env", key: variable name.
/// Note: env var values don't participate in the stage hash (only
/// the allowlist names do), so we also change a dep to trigger the
/// cache miss, then verify the env diff is reported.
#[test]
fn explain_miss_changed_env_shows_diff() {
    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join("input.txt"), b"data-v1").unwrap();

    // First run with env var set to one value.
    let output = Command::new(bin())
        .current_dir(tmp.path())
        .env("CRAB_WORKFLOW_ENABLED", "1")
        .env("CUDA_VISIBLE_DEVICES", "0")
        .args([
            "run",
            "--name",
            "train",
            "--deps",
            "input.txt",
            "--outs",
            "output.txt",
            "--env",
            "CUDA_VISIBLE_DEVICES",
            "--",
            "/bin/cp",
            "input.txt",
            "output.txt",
        ])
        .output()
        .expect("crab run should spawn");
    assert!(
        output.status.success(),
        "first run should succeed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify lockfile was created.
    assert!(
        tmp.path().join("crab.lock").exists(),
        "lockfile should exist after first run"
    );

    // Modify the dep file AND change the env var value to trigger a
    // cache miss (dep change causes the hash to differ) and show the
    // env diff.
    fs::write(tmp.path().join("input.txt"), b"data-v2").unwrap();

    // Second run with different dep content AND different env var value.
    let output = Command::new(bin())
        .current_dir(tmp.path())
        .env("CRAB_WORKFLOW_ENABLED", "1")
        .env("CUDA_VISIBLE_DEVICES", "1")
        .args([
            "run",
            "--explain-miss",
            "--json",
            "--name",
            "train",
            "--deps",
            "input.txt",
            "--outs",
            "output.txt",
            "--env",
            "CUDA_VISIBLE_DEVICES",
            "--",
            "/bin/cp",
            "input.txt",
            "output.txt",
        ])
        .output()
        .expect("crab run should spawn");

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let lines: Vec<&str> = stdout.trim().lines().collect();
    assert!(
        !lines.is_empty(),
        "expected JSON output; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let envelope: serde_json::Value = serde_json::from_str(lines[0])
        .unwrap_or_else(|e| panic!("parse failed: {e}; line={}", lines[0]));

    assert_eq!(envelope["schema"], "workflow.explain_miss");
    let data = &envelope["data"]["data"];
    let diffs = data["diffs"].as_array().expect("diffs should be an array");

    // Should have both a dep diff and an env diff.
    let env_diff = diffs
        .iter()
        .find(|d| d["category"] == "env" && d["key"] == "CUDA_VISIBLE_DEVICES")
        .expect("should have an env diff for CUDA_VISIBLE_DEVICES");

    assert_eq!(env_diff["category"], "env");
    assert_eq!(env_diff["key"], "CUDA_VISIBLE_DEVICES");
}

/// Changed cmd shows category: "cmd".
#[test]
fn explain_miss_changed_cmd_shows_diff() {
    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join("input.txt"), b"data").unwrap();

    // First run with one command.
    let status = run_stage(
        tmp.path(),
        &[
            "run",
            "--name",
            "process",
            "--deps",
            "input.txt",
            "--outs",
            "output.txt",
            "--",
            "/bin/cp",
            "input.txt",
            "output.txt",
        ],
    );
    assert!(status.success(), "first run should succeed");

    // Second run with a different command — triggers cache miss.
    let envelope = run_explain_miss_json(
        tmp.path(),
        &[
            "--name",
            "process",
            "--deps",
            "input.txt",
            "--outs",
            "output.txt",
            "--",
            "/bin/cat",
            "input.txt",
        ],
    );

    assert_eq!(envelope["schema"], "workflow.explain_miss");
    let data = &envelope["data"]["data"];
    let diffs = data["diffs"].as_array().expect("diffs should be an array");

    let cmd_diff = diffs
        .iter()
        .find(|d| d["category"] == "cmd")
        .expect("should have a cmd diff");

    assert_eq!(cmd_diff["category"], "cmd");
    assert_eq!(cmd_diff["key"], "cmd");
    assert!(cmd_diff["old"].as_str().is_some());
    assert!(cmd_diff["new"].as_str().is_some());
    assert_ne!(cmd_diff["old"], cmd_diff["new"]);
}

/// JSON output conforms to `workflow.explain_miss` schema structure.
#[test]
fn explain_miss_json_conforms_to_schema() {
    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join("a.txt"), b"v1").unwrap();

    // First run to populate lockfile.
    let status = run_stage(
        tmp.path(),
        &[
            "run", "--name", "copy", "--deps", "a.txt", "--outs", "b.txt", "--", "/bin/cp",
            "a.txt", "b.txt",
        ],
    );
    assert!(status.success());

    // Modify dep to trigger miss.
    fs::write(tmp.path().join("a.txt"), b"v2").unwrap();

    let envelope = run_explain_miss_json(
        tmp.path(),
        &[
            "--name", "copy", "--deps", "a.txt", "--outs", "b.txt", "--", "/bin/cp", "a.txt",
            "b.txt",
        ],
    );

    // Verify schema structure.
    assert_eq!(envelope["schema"], "workflow.explain_miss");
    assert!(envelope["version"].is_string());
    assert!(envelope["data"].is_object());

    let data = &envelope["data"]["data"];
    assert!(data["stage"].is_string());
    assert!(data["stage_hash_current"].is_string());
    assert!(data["stage_hash_lockfile"].is_string());
    assert!(data["diffs"].is_array());

    // Each diff entry has the required fields.
    for diff in data["diffs"].as_array().unwrap() {
        assert!(diff["category"].is_string(), "diff missing category");
        assert!(diff["key"].is_string(), "diff missing key");
        // old and new are optional (skip_serializing_if = None)
        // but when present they should be strings.
        if let Some(old) = diff.get("old") {
            if !old.is_null() {
                assert!(old.is_string(), "old should be a string when present");
            }
        }
        if let Some(new) = diff.get("new") {
            if !new.is_null() {
                assert!(new.is_string(), "new should be a string when present");
            }
        }
    }
}
