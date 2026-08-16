//! Integration tests for P7: side effects and `on_cache_hit` execution.
//!
//! Exercises the `on_cache_hit` hook wiring: a stage with
//! `side_effects: true` and `on_cache_hit: <cmd>` fires the hook on
//! cache hits (second run), transitions to `Failed` on non-zero exit,
//! and emits a warning when `on_cache_hit` is absent.

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

/// Scaffold a repo with a stage that has `side_effects: true` and
/// `on_cache_hit: "echo hit >> log"`. The stage command writes a
/// file so we can verify cache hits.
fn scaffold_side_effects_repo(root: &Path) {
    let yaml = format!(
        r#"stages:
  notify:
    cmd: "echo first_run > {root}/output.txt"
    deps:
      - input.txt
    outs:
      - output.txt
    side_effects: true
    on_cache_hit: "echo hit >> {root}/log.txt"
"#,
        root = root.to_string_lossy(),
    );
    fs::write(root.join("crab.yaml"), yaml).unwrap();
    fs::write(root.join("input.txt"), b"payload").unwrap();

    fs::create_dir_all(root.join(".crab")).unwrap();
    fs::write(
        root.join(".crab").join("config.toml"),
        "[workflow]\nenabled = true\n",
    )
    .unwrap();
}

/// Scaffold a repo with a stage that has `side_effects: true` and
/// `on_cache_hit` that exits non-zero.
fn scaffold_failing_hook_repo(root: &Path) {
    let yaml = format!(
        r#"stages:
  notify:
    cmd: "echo first_run > {root}/output.txt"
    deps:
      - input.txt
    outs:
      - output.txt
    side_effects: true
    on_cache_hit: "exit 42"
"#,
        root = root.to_string_lossy(),
    );
    fs::write(root.join("crab.yaml"), yaml).unwrap();
    fs::write(root.join("input.txt"), b"payload").unwrap();

    fs::create_dir_all(root.join(".crab")).unwrap();
    fs::write(
        root.join(".crab").join("config.toml"),
        "[workflow]\nenabled = true\n",
    )
    .unwrap();
}

/// Scaffold a repo with a stage that has `side_effects: true` but
/// NO `on_cache_hit` — should emit a warning.
fn scaffold_no_hook_repo(root: &Path) {
    let yaml = format!(
        r#"stages:
  notify:
    cmd: "echo first_run > {root}/output.txt"
    deps:
      - input.txt
    outs:
      - output.txt
    side_effects: true
"#,
        root = root.to_string_lossy(),
    );
    fs::write(root.join("crab.yaml"), yaml).unwrap();
    fs::write(root.join("input.txt"), b"payload").unwrap();

    fs::create_dir_all(root.join(".crab")).unwrap();
    fs::write(
        root.join(".crab").join("config.toml"),
        "[workflow]\nenabled = true\n",
    )
    .unwrap();
}

/// First run (miss path) should NOT fire the on_cache_hit hook.
/// Second run (cache hit) should fire the hook and append to log.
#[test]
fn on_cache_hit_fires_on_second_run() {
    let tmp = TempDir::new().unwrap();
    scaffold_side_effects_repo(tmp.path());

    // First run: miss path — hook should NOT fire.
    let output = Command::new(bin())
        .current_dir(tmp.path())
        .env("CRAB_WORKFLOW_ENABLED", "1")
        .args(["run", "notify"])
        .output()
        .expect("crab run should spawn");

    assert!(
        output.status.success(),
        "first run should succeed: stderr={}",
        String::from_utf8_lossy(&output.stderr),
    );

    // Verify the output file was created by the stage command.
    assert!(
        tmp.path().join("output.txt").exists(),
        "output.txt should exist after first run"
    );

    // The log file should NOT exist after the first run (hook
    // doesn't fire on miss path).
    assert!(
        !tmp.path().join("log.txt").exists(),
        "log.txt should NOT exist after first run (miss path)"
    );

    // Second run: cache hit — hook should fire.
    let output = Command::new(bin())
        .current_dir(tmp.path())
        .env("CRAB_WORKFLOW_ENABLED", "1")
        .args(["run", "notify"])
        .output()
        .expect("crab run should spawn");

    assert!(
        output.status.success(),
        "second run should succeed: stderr={}",
        String::from_utf8_lossy(&output.stderr),
    );

    // The log file should now exist with "hit" appended.
    let log_content = fs::read_to_string(tmp.path().join("log.txt"))
        .expect("log.txt should exist after second run");
    assert!(
        log_content.contains("hit"),
        "log.txt should contain 'hit' from on_cache_hit hook, got: {log_content:?}"
    );
}

/// Hook non-zero exit transitions stage to `Failed`; cache entry
/// remains valid for subsequent runs.
#[test]
fn hook_nonzero_exit_transitions_to_failed() {
    let tmp = TempDir::new().unwrap();
    scaffold_failing_hook_repo(tmp.path());

    // First run: miss path — succeeds.
    let output = Command::new(bin())
        .current_dir(tmp.path())
        .env("CRAB_WORKFLOW_ENABLED", "1")
        .args(["run", "notify"])
        .output()
        .expect("crab run should spawn");

    assert!(
        output.status.success(),
        "first run should succeed: stderr={}",
        String::from_utf8_lossy(&output.stderr),
    );

    // Second run: cache hit — hook exits 42, stage should fail.
    let output = Command::new(bin())
        .current_dir(tmp.path())
        .env("CRAB_WORKFLOW_ENABLED", "1")
        .args(["run", "notify"])
        .output()
        .expect("crab run should spawn");

    assert!(
        !output.status.success(),
        "second run should fail due to hook non-zero exit"
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("on_cache_hit hook failed") || stderr.contains("E0239"),
        "stderr should mention hook failure, got: {stderr}"
    );
}

/// Stage with `side_effects: true` but no `on_cache_hit` emits
/// warning and sets `side_effects_skipped: true` in structured output.
#[test]
fn side_effects_without_hook_emits_warning_and_skipped_flag() {
    let tmp = TempDir::new().unwrap();
    scaffold_no_hook_repo(tmp.path());

    // First run: miss path.
    let output = Command::new(bin())
        .current_dir(tmp.path())
        .env("CRAB_WORKFLOW_ENABLED", "1")
        .args(["run", "notify"])
        .output()
        .expect("crab run should spawn");

    assert!(
        output.status.success(),
        "first run should succeed: stderr={}",
        String::from_utf8_lossy(&output.stderr),
    );

    // Second run: cache hit — should emit warning about skipped
    // side effects.
    let output = Command::new(bin())
        .current_dir(tmp.path())
        .env("CRAB_WORKFLOW_ENABLED", "1")
        .env("CRAB_LOG", "warn")
        .args(["run", "--json", "notify"])
        .output()
        .expect("crab run should spawn");

    assert!(
        output.status.success(),
        "second run should succeed: stderr={}",
        String::from_utf8_lossy(&output.stderr),
    );

    // Check stderr for the warning about skipped side effects.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("side_effects") || stderr.contains("side effects"),
        "stderr should warn about skipped side effects, got: {stderr}"
    );

    // Check structured output for side_effects_skipped: true.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let json: serde_json::Value = serde_json::from_str(&stdout)
        .unwrap_or_else(|e| panic!("stdout should be valid JSON: {e}\nstdout: {stdout}"));
    assert_eq!(json["schema"], "workflow.run");
    let stages = json["data"]["stages"]
        .as_array()
        .expect("workflow.run data.stages array");
    let data = stages
        .iter()
        .find(|stage| stage["stage_name"] == "notify")
        .expect("notify stage result");
    assert_eq!(
        data.get("side_effects_skipped"),
        Some(&serde_json::Value::Bool(true)),
        "structured output should have notify.side_effects_skipped: true, got: {stdout}"
    );
}

/// Hook does NOT fire during retry attempts (retries are miss paths
/// by definition).
#[test]
fn hook_does_not_fire_during_retry() {
    let tmp = TempDir::new().unwrap();

    // Create a stage that fails on first attempt, succeeds on second,
    // with side_effects and on_cache_hit. The hook should NOT fire
    // because retries are miss paths.
    let script = tmp.path().join("run.sh");
    fs::write(
        &script,
        format!(
            r#"#!/bin/sh
COUNTER_FILE="{root}/counter.txt"
if [ ! -f "$COUNTER_FILE" ]; then
    echo "1" > "$COUNTER_FILE"
    exit 1
fi
echo "done" > "{root}/output.txt"
exit 0
"#,
            root = tmp.path().to_string_lossy(),
        ),
    )
    .unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    }

    let yaml = format!(
        r#"stages:
  flaky:
    cmd: "{script}"
    deps:
      - input.txt
    outs:
      - output.txt
    retry:
      max_attempts: 3
      on_exit_codes: [1]
      initial_backoff: "10ms"
      max_backoff: "1s"
      backoff_multiplier: 2.0
    side_effects: true
    on_cache_hit: "echo hit >> {root}/hook_log.txt"
"#,
        script = script.to_string_lossy(),
        root = tmp.path().to_string_lossy(),
    );
    fs::write(tmp.path().join("crab.yaml"), yaml).unwrap();
    fs::write(tmp.path().join("input.txt"), b"retry-payload").unwrap();

    fs::create_dir_all(tmp.path().join(".crab")).unwrap();
    fs::write(
        tmp.path().join(".crab").join("config.toml"),
        "[workflow]\nenabled = true\n",
    )
    .unwrap();

    // First run: retries and succeeds on attempt 2 (miss path).
    // Hook should NOT fire.
    let output = Command::new(bin())
        .current_dir(tmp.path())
        .env("CRAB_WORKFLOW_ENABLED", "1")
        .args(["run", "flaky"])
        .output()
        .expect("crab run should spawn");

    assert!(
        output.status.success(),
        "first run should succeed after retry: stderr={}",
        String::from_utf8_lossy(&output.stderr),
    );

    // Hook log should NOT exist — retries are miss paths.
    assert!(
        !tmp.path().join("hook_log.txt").exists(),
        "hook_log.txt should NOT exist after first run (miss path with retries)"
    );
}
