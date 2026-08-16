//! Integration tests for the retry loop wired in `cmd/run.rs`.
//!
//! Exercises the retry policy end-to-end: a stage that fails on the
//! first attempt with a qualifying exit code retries and succeeds on
//! the second attempt. Verifies journal rows, JSONL retry events,
//! exhausted-retry error surfacing, and backoff policy adherence.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]

use std::fs;
use std::path::Path;
use std::process::Command;

use rusqlite::Connection;
use serde_json::Value;
use tempfile::TempDir;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_crab")
}

/// Scaffold a repo with a single stage that uses a counter file to
/// fail on the first attempt (exit 1) and succeed on the second.
/// The retry policy allows up to 3 attempts on exit code 1.
fn scaffold_retry_repo(root: &Path) {
    // The stage script reads a counter file, increments it, and
    // exits 1 if the counter is 1 (first attempt). On attempt 2+
    // it succeeds by copying the dep to the out.
    let script = root.join("run.sh");
    fs::write(
        &script,
        r#"#!/bin/sh
COUNTER_FILE="$1/counter.txt"
DEP="$1/input.txt"
OUT="$1/output.txt"

if [ ! -f "$COUNTER_FILE" ]; then
    echo "1" > "$COUNTER_FILE"
    exit 1
fi

COUNT=$(cat "$COUNTER_FILE")
COUNT=$((COUNT + 1))
echo "$COUNT" > "$COUNTER_FILE"
cp "$DEP" "$OUT"
exit 0
"#,
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
    cmd: "{script} {root}"
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
"#,
        script = script.to_string_lossy(),
        root = root.to_string_lossy(),
    );
    fs::write(root.join("crab.yaml"), yaml).unwrap();
    fs::write(root.join("input.txt"), b"retry-payload").unwrap();

    fs::create_dir_all(root.join(".crab")).unwrap();
    fs::write(
        root.join(".crab").join("config.toml"),
        "[workflow]\nenabled = true\n",
    )
    .unwrap();
}

/// Scaffold a repo with a stage that always fails (exit 1) to test
/// retry exhaustion.
fn scaffold_exhausted_repo(root: &Path) {
    let script = root.join("always_fail.sh");
    fs::write(&script, "#!/bin/sh\nexit 1\n").unwrap();

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    }

    let yaml = format!(
        r#"stages:
  doomed:
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
"#,
        script = script.to_string_lossy(),
    );
    fs::write(root.join("crab.yaml"), yaml).unwrap();
    fs::write(root.join("input.txt"), b"doomed-payload").unwrap();

    fs::create_dir_all(root.join(".crab")).unwrap();
    fs::write(
        root.join(".crab").join("config.toml"),
        "[workflow]\nenabled = true\n",
    )
    .unwrap();
}

/// Stage with retry policy retries on exit code 1 and succeeds on
/// attempt 2.
#[test]
fn retry_succeeds_on_second_attempt() {
    let tmp = TempDir::new().unwrap();
    scaffold_retry_repo(tmp.path());

    let output = Command::new(bin())
        .current_dir(tmp.path())
        .env("CRAB_WORKFLOW_ENABLED", "1")
        .args(["run", "flaky"])
        .output()
        .expect("crab run should spawn");

    assert!(
        output.status.success(),
        "run should succeed on retry: status={}, stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );

    // The output file should exist with the correct content.
    assert_eq!(
        fs::read(tmp.path().join("output.txt")).unwrap(),
        b"retry-payload".to_vec(),
        "output should match input after successful retry"
    );
}

/// Journal contains rows for attempt 1 (Failed) and attempt 2
/// (Committed).
#[test]
fn journal_records_both_attempts() {
    let tmp = TempDir::new().unwrap();
    scaffold_retry_repo(tmp.path());

    let output = Command::new(bin())
        .current_dir(tmp.path())
        .env("CRAB_WORKFLOW_ENABLED", "1")
        .args(["run", "flaky"])
        .output()
        .expect("crab run should spawn");

    assert!(
        output.status.success(),
        "run should succeed: stderr={}",
        String::from_utf8_lossy(&output.stderr),
    );

    // Find the journal directory.
    let runs_dir = tmp.path().join(".crab/workflow/runs");
    let entries: Vec<_> = fs::read_dir(&runs_dir)
        .expect("runs dir exists")
        .filter_map(Result::ok)
        .collect();
    assert_eq!(entries.len(), 1, "one run journal expected");

    let journal_path = entries[0].path().join("journal.db");
    let conn = Connection::open(&journal_path).expect("open journal");

    // Query all stage_runs rows for the 'flaky' stage.
    let mut stmt = conn
        .prepare(
            "SELECT attempt, state FROM stage_runs WHERE stage_name = 'flaky' ORDER BY attempt",
        )
        .unwrap();
    let rows: Vec<(u32, i64)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .filter_map(Result::ok)
        .collect();

    assert!(
        rows.len() >= 2,
        "expected at least 2 attempt rows, got {}: {:?}",
        rows.len(),
        rows
    );

    // Attempt 1 should be in Failed state (tag 11).
    let (attempt_1, state_1) = rows[0];
    assert_eq!(attempt_1, 1);
    // Failed state tag is 11 in the journal schema.
    assert_eq!(
        state_1, 11,
        "attempt 1 should be Failed (tag 11), got {state_1}"
    );

    // Attempt 2 should be in Committed state (tag 10).
    let (attempt_2, state_2) = rows[1];
    assert_eq!(attempt_2, 2);
    assert_eq!(
        state_2, 10,
        "attempt 2 should be Committed (tag 10), got {state_2}"
    );
}

/// `--jsonl` emits `workflow.stage.retry` events with attempt number
/// and backoff duration.
#[test]
fn jsonl_emits_retry_events() {
    let tmp = TempDir::new().unwrap();
    scaffold_retry_repo(tmp.path());

    let output = Command::new(bin())
        .current_dir(tmp.path())
        .env("CRAB_WORKFLOW_ENABLED", "1")
        .args(["run", "--jsonl", "flaky"])
        .output()
        .expect("crab run --jsonl should spawn");

    assert!(
        output.status.success(),
        "run should succeed: stderr={}",
        String::from_utf8_lossy(&output.stderr),
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();

    // Find the retry event line.
    let retry_lines: Vec<Value> = lines
        .iter()
        .filter_map(|line| {
            let v: Value = serde_json::from_str(line).ok()?;
            if v["schema"].as_str() == Some("workflow.stage.retry") {
                Some(v)
            } else {
                None
            }
        })
        .collect();

    assert!(
        !retry_lines.is_empty(),
        "expected at least one workflow.stage.retry event in JSONL output; lines={:?}",
        lines
    );

    let retry_event = &retry_lines[0];
    assert_eq!(retry_event["data"]["stage"], "flaky");
    assert_eq!(retry_event["data"]["attempt"], 1);
    assert!(
        retry_event["data"]["backoff_ms"].as_u64().unwrap() > 0,
        "backoff_ms should be positive"
    );
    assert_eq!(retry_event["data"]["reason"], "exit_nonzero");
    assert_eq!(retry_event["data"]["exhausted"], false);
}

/// Exhausted retries surface the last attempt's error in structured
/// output with `exhausted: true` semantics (the error variant
/// carries the attempt count).
#[test]
fn exhausted_retries_surface_error() {
    let tmp = TempDir::new().unwrap();
    scaffold_exhausted_repo(tmp.path());

    let output = Command::new(bin())
        .current_dir(tmp.path())
        .env("CRAB_WORKFLOW_ENABLED", "1")
        .args(["run", "--json", "doomed"])
        .output()
        .expect("crab run should spawn");

    assert!(
        !output.status.success(),
        "run should fail when retries exhausted"
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    // The structured error output should mention retry exhaustion.
    // The error envelope carries the stage name and attempt count.
    if !stdout.is_empty() {
        let envelope: Value = serde_json::from_str(stdout.trim()).unwrap_or(Value::Null);
        if envelope != Value::Null {
            // Check for the exhausted error in the error envelope.
            let error_msg = envelope["error"]["message"].as_str().unwrap_or("");
            assert!(
                error_msg.contains("exhausted") || error_msg.contains("retry"),
                "error message should mention retry exhaustion: {error_msg}"
            );
        }
    }

    // Also verify via stderr that the error mentions retry exhaustion.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("exhausted") || stderr.contains("retry") || stderr.contains("3 attempts"),
        "stderr should mention retry exhaustion: {stderr}"
    );
}

/// Retry backoff respects `initial_backoff`, `max_backoff`, and
/// `backoff_multiplier` from the policy. We verify this by checking
/// the backoff_ms field in the JSONL retry event.
#[test]
fn retry_backoff_respects_policy() {
    let tmp = TempDir::new().unwrap();

    // Create a stage that fails twice then succeeds, with a known
    // backoff policy.
    let script = tmp.path().join("fail_twice.sh");
    fs::write(
        &script,
        format!(
            r#"#!/bin/sh
COUNTER_FILE="{}/counter2.txt"
DEP="{}/input.txt"
OUT="{}/output.txt"

if [ ! -f "$COUNTER_FILE" ]; then
    echo "1" > "$COUNTER_FILE"
    exit 1
fi

COUNT=$(cat "$COUNTER_FILE")
if [ "$COUNT" -lt 2 ]; then
    COUNT=$((COUNT + 1))
    echo "$COUNT" > "$COUNTER_FILE"
    exit 1
fi

COUNT=$((COUNT + 1))
echo "$COUNT" > "$COUNTER_FILE"
cp "$DEP" "$OUT"
exit 0
"#,
            tmp.path().to_string_lossy(),
            tmp.path().to_string_lossy(),
            tmp.path().to_string_lossy(),
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
  backoff_test:
    cmd: "{script}"
    deps:
      - input.txt
    outs:
      - output.txt
    retry:
      max_attempts: 5
      on_exit_codes: [1]
      initial_backoff: "50ms"
      max_backoff: "5s"
      backoff_multiplier: 3.0
"#,
        script = script.to_string_lossy(),
    );
    fs::write(tmp.path().join("crab.yaml"), yaml).unwrap();
    fs::write(tmp.path().join("input.txt"), b"backoff-test").unwrap();

    fs::create_dir_all(tmp.path().join(".crab")).unwrap();
    fs::write(
        tmp.path().join(".crab").join("config.toml"),
        "[workflow]\nenabled = true\n",
    )
    .unwrap();

    let output = Command::new(bin())
        .current_dir(tmp.path())
        .env("CRAB_WORKFLOW_ENABLED", "1")
        .args(["run", "--jsonl", "backoff_test"])
        .output()
        .expect("crab run --jsonl should spawn");

    assert!(
        output.status.success(),
        "run should succeed after retries: stderr={}",
        String::from_utf8_lossy(&output.stderr),
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let retry_events: Vec<Value> = stdout
        .lines()
        .filter_map(|line| {
            let v: Value = serde_json::from_str(line).ok()?;
            if v["schema"].as_str() == Some("workflow.stage.retry") {
                Some(v)
            } else {
                None
            }
        })
        .collect();

    // Should have 2 retry events (attempt 1 fails, attempt 2 fails,
    // attempt 3 succeeds).
    assert_eq!(
        retry_events.len(),
        2,
        "expected 2 retry events, got {}: {:?}",
        retry_events.len(),
        retry_events
    );

    // First retry: backoff should be initial_backoff = 50ms
    // (attempt 1 → backoff = 50 * 3^0 = 50ms).
    let backoff_1 = retry_events[0]["data"]["backoff_ms"].as_u64().unwrap();
    assert_eq!(backoff_1, 50, "first backoff should be 50ms (initial)");

    // Second retry: backoff should be 50 * 3^1 = 150ms.
    let backoff_2 = retry_events[1]["data"]["backoff_ms"].as_u64().unwrap();
    assert_eq!(backoff_2, 150, "second backoff should be 150ms (50 * 3)");
}
