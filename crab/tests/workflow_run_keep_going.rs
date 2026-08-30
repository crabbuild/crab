//! Integration tests for partial DAG success (`--keep-going`,
//! `--ignore-errors`).
//!
//! Exercises the diamond DAG `A → {B, C} → D` where B fails:
//! - `--keep-going`: C succeeds, D is `NotStarted`, exit code 1.
//! - Default (no flag): B fails, D never starts, exit code 1.
//! - `--ignore-errors`: D is attempted even though B failed.
//! - Lockfile contains C's entry but not B or D.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]

use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::Value;
use tempfile::TempDir;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_crab")
}

/// Scaffold a diamond DAG: `A → {B, C} → D`.
///
/// - Stage A: copies `input.txt` → `a_out.txt` (always succeeds).
/// - Stage B: runs a script that exits 1 (always fails).
/// - Stage C: copies `a_out.txt` → `c_out.txt` (always succeeds).
/// - Stage D: copies `b_out.txt` + `c_out.txt` → `d_out.txt`
///   (depends on both B and C outputs).
///
/// The DAG edges are inferred from dep/out overlap:
///   A produces `a_out.txt`
///   B depends on `a_out.txt`, produces `b_out.txt`
///   C depends on `a_out.txt`, produces `c_out.txt`
///   D depends on `b_out.txt` and `c_out.txt`, produces `d_out.txt`
fn scaffold_diamond_repo(root: &Path) {
    // Script that always fails with exit code 1.
    let fail_script = root.join("fail.sh");
    fs::write(&fail_script, "#!/bin/sh\nexit 1\n").unwrap();

    // Script for stage D that concatenates both inputs.
    let concat_script = root.join("concat.sh");
    fs::write(
        &concat_script,
        "#!/bin/sh\ncat b_out.txt c_out.txt > d_out.txt\n",
    )
    .unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&fail_script, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(&concat_script, fs::Permissions::from_mode(0o755)).unwrap();
    }

    let yaml = format!(
        r#"stages:
  stage_a:
    cmd: "/bin/cp input.txt a_out.txt"
    deps:
      - input.txt
    outs:
      - a_out.txt
  stage_b:
    cmd: "{fail_script}"
    deps:
      - a_out.txt
    outs:
      - b_out.txt
  stage_c:
    cmd: "/bin/cp a_out.txt c_out.txt"
    deps:
      - a_out.txt
    outs:
      - c_out.txt
  stage_d:
    cmd: "{concat_script}"
    deps:
      - b_out.txt
      - c_out.txt
    outs:
      - d_out.txt
"#,
        fail_script = fail_script.to_string_lossy(),
        concat_script = concat_script.to_string_lossy(),
    );
    fs::write(root.join("crab.yaml"), yaml).unwrap();
    fs::write(root.join("input.txt"), b"diamond-test\n").unwrap();

    fs::create_dir_all(root.join(".crab")).unwrap();
    fs::write(
        root.join(".crab").join("local.toml"),
        "[workflow]\nenabled = true\n",
    )
    .unwrap();
}

/// Run `crab run` in DAG mode with optional extra args.
fn run_dag(repo: &Path, extra: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(bin());
    cmd.current_dir(repo).env("CRAB_WORKFLOW_ENABLED", "1");
    cmd.arg("run");
    for a in extra {
        cmd.arg(a);
    }
    cmd.output().expect("crab run should spawn")
}

/// Check whether `crab.lock` contains a given stage name as a
/// top-level key under `stages:`.
fn lockfile_contains_stage(lockfile: &str, name: &str) -> bool {
    let needle = format!("\n  {name}:\n");
    lockfile.contains(&needle)
}

/// Parse multi-line JSON output and find the envelope with the given
/// schema. DAG mode `--json` emits multiple JSON objects (one per
/// line): per-stage events, the `workflow.run` summary, and the
/// error envelope. This helper finds the one we care about.
fn find_json_envelope(stdout: &str, schema: &str) -> Option<Value> {
    stdout.lines().find_map(|line| {
        let v: Value = serde_json::from_str(line).ok()?;
        if v["schema"].as_str() == Some(schema) {
            Some(v)
        } else {
            None
        }
    })
}

// ─── Tests ───────────────────────────────────────────────────────

/// Diamond DAG with `--keep-going`: B fails, C succeeds, D is
/// `NotStarted` with reason `upstream_failed`. Exit code 1.
#[test]
fn keep_going_diamond_b_fails_c_succeeds_d_not_started() {
    let tmp = TempDir::new().unwrap();
    scaffold_diamond_repo(tmp.path());

    let output = run_dag(tmp.path(), &["--keep-going", "--json"]);

    assert!(
        !output.status.success(),
        "DAG with a failed stage must exit non-zero"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let envelope = find_json_envelope(&stdout, "workflow.run")
        .expect("expected a workflow.run envelope in --json output");

    let data = &envelope["data"];

    // A and C should be in succeeded.
    let succeeded: Vec<&str> = data["succeeded"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(
        succeeded.contains(&"stage_a"),
        "stage_a should succeed: {succeeded:?}"
    );
    assert!(
        succeeded.contains(&"stage_c"),
        "stage_c should succeed: {succeeded:?}"
    );

    // B should be in failed.
    let failed: Vec<&str> = data["failed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(
        failed.contains(&"stage_b"),
        "stage_b should be in failed: {failed:?}"
    );

    // D should be in not_started.
    let not_started: Vec<&str> = data["not_started"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert!(
        not_started.contains(&"stage_d"),
        "stage_d should be not_started: {not_started:?}"
    );

    // C's output file should exist (it ran successfully).
    assert!(
        tmp.path().join("c_out.txt").exists(),
        "c_out.txt should exist after stage_c succeeds"
    );

    // D's output file should NOT exist (it was never started).
    assert!(
        !tmp.path().join("d_out.txt").exists(),
        "d_out.txt should not exist since stage_d was not started"
    );
}

/// Without `--keep-going` (default): B fails, D never starts,
/// exit code 1. C may or may not run depending on scheduling
/// order, but D must never start.
#[test]
fn default_mode_b_fails_d_never_starts_exit_code_1() {
    let tmp = TempDir::new().unwrap();
    scaffold_diamond_repo(tmp.path());

    let output = run_dag(tmp.path(), &[]);

    assert!(
        !output.status.success(),
        "DAG with a failed stage must exit non-zero (exit code 1)"
    );

    // D's output file should NOT exist.
    assert!(
        !tmp.path().join("d_out.txt").exists(),
        "d_out.txt should not exist in default mode when B fails"
    );
}

/// `--ignore-errors`: D is attempted even though B failed. D will
/// fail at execution time because `b_out.txt` doesn't exist (B
/// never produced it), but the important thing is that D was
/// *attempted* — it should NOT be in `not_started`.
#[test]
fn ignore_errors_d_is_attempted_despite_b_failure() {
    let tmp = TempDir::new().unwrap();
    scaffold_diamond_repo(tmp.path());

    let output = run_dag(tmp.path(), &["--ignore-errors", "--json"]);

    assert!(
        !output.status.success(),
        "DAG should still fail overall (B failed, D likely fails too)"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let envelope = find_json_envelope(&stdout, "workflow.run")
        .expect("expected a workflow.run envelope in --json output");

    let data = &envelope["data"];

    // D should NOT be in not_started — it was attempted.
    let empty = vec![];
    let not_started: Vec<&str> = data["not_started"]
        .as_array()
        .unwrap_or(&empty)
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(
        !not_started.contains(&"stage_d"),
        "stage_d should NOT be in not_started under --ignore-errors: {not_started:?}"
    );

    // D should be in either failed (dep resolution error or
    // execution error) or succeeded (unlikely but possible if
    // the script handles missing input gracefully).
    let failed: Vec<&str> = data["failed"]
        .as_array()
        .unwrap_or(&empty)
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    let succeeded: Vec<&str> = data["succeeded"]
        .as_array()
        .unwrap_or(&empty)
        .iter()
        .filter_map(|v| v.as_str())
        .collect();
    assert!(
        failed.contains(&"stage_d") || succeeded.contains(&"stage_d"),
        "stage_d should have been attempted (in failed or succeeded): \
         failed={failed:?}, succeeded={succeeded:?}"
    );
}

/// Lockfile contains C's entry (committed) but not B or D after a
/// `--keep-going` run where B fails.
#[test]
fn lockfile_has_c_but_not_b_or_d_after_keep_going() {
    let tmp = TempDir::new().unwrap();
    scaffold_diamond_repo(tmp.path());

    let output = run_dag(tmp.path(), &["--keep-going"]);

    assert!(
        !output.status.success(),
        "DAG with a failed stage must exit non-zero"
    );

    let lockfile_path = tmp.path().join("crab.lock");
    assert!(
        lockfile_path.exists(),
        "crab.lock must exist after a partial DAG success"
    );

    let lockfile = fs::read_to_string(&lockfile_path).unwrap();

    // A succeeded, so it should be in the lockfile.
    assert!(
        lockfile_contains_stage(&lockfile, "stage_a"),
        "stage_a should be in lockfile (it committed): {lockfile}"
    );

    // C succeeded, so it should be in the lockfile.
    assert!(
        lockfile_contains_stage(&lockfile, "stage_c"),
        "stage_c should be in lockfile (it committed): {lockfile}"
    );

    // B failed, so it should NOT be in the lockfile.
    assert!(
        !lockfile_contains_stage(&lockfile, "stage_b"),
        "stage_b should NOT be in lockfile (it failed): {lockfile}"
    );

    // D was not started, so it should NOT be in the lockfile.
    assert!(
        !lockfile_contains_stage(&lockfile, "stage_d"),
        "stage_d should NOT be in lockfile (it was not started): {lockfile}"
    );
}

/// Structured output includes per-stage disposition with reasons
/// via JSONL stream.
#[test]
fn jsonl_emits_not_started_events_with_reasons() {
    let tmp = TempDir::new().unwrap();
    scaffold_diamond_repo(tmp.path());

    let output = run_dag(tmp.path(), &["--keep-going", "--jsonl"]);

    assert!(
        !output.status.success(),
        "DAG with a failed stage must exit non-zero"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();

    // Find the not_started event for stage_d.
    let not_started_events: Vec<Value> = lines
        .iter()
        .filter_map(|line| {
            let v: Value = serde_json::from_str(line).ok()?;
            if v["schema"].as_str() == Some("workflow.stage.not_started") {
                Some(v)
            } else {
                None
            }
        })
        .collect();

    assert!(
        !not_started_events.is_empty(),
        "expected at least one workflow.stage.not_started event; lines={lines:?}"
    );

    // Find the event for stage_d specifically.
    let d_event = not_started_events
        .iter()
        .find(|v| v["data"]["stage"].as_str() == Some("stage_d"));
    assert!(
        d_event.is_some(),
        "expected a not_started event for stage_d: {not_started_events:?}"
    );

    let d_event = d_event.unwrap();
    assert_eq!(
        d_event["data"]["reason"].as_str(),
        Some("upstream_failed"),
        "stage_d not_started reason should be 'upstream_failed'"
    );
}
