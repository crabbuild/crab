//! Integration tests for `crab run --jsonl` in DAG mode.
//!
//! Scaffold a two-stage DAG (`step1 → step2`), run `crab run`
//! with `--jsonl`, and assert the stream carries the expected
//! sequence of `workflow.stage.*` events per stage followed by a
//! terminal `workflow.run` summary line.

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

/// Scaffold a repo with a two-stage linear pipeline:
/// `step1` copies `a.txt → b.txt`, `step2` copies `b.txt → c.txt`.
/// `step2` depends on `step1`'s out so the DAG has a deterministic
/// topological order.
fn scaffold_two_stage_repo(root: &Path) {
    fs::write(
        root.join("crab.yaml"),
        "stages:\n  \
            step1:\n    cmd: \"/bin/cp a.txt b.txt\"\n    deps:\n      - a.txt\n    outs:\n      - b.txt\n  \
            step2:\n    cmd: \"/bin/cp b.txt c.txt\"\n    deps:\n      - b.txt\n    outs:\n      - c.txt\n",
    )
    .unwrap();
    fs::write(root.join("a.txt"), b"payload").unwrap();

    fs::create_dir_all(root.join(".crab")).unwrap();
    fs::write(
        root.join(".crab").join("local.toml"),
        "[workflow]\nenabled = true\n",
    )
    .unwrap();
}

/// Parse one JSONL line into a `(schema, event_type)` pair plus the
/// full value so assertions can inspect payload fields.
fn parse_line(line: &str) -> (String, String, Value) {
    let v: Value = serde_json::from_str(line)
        .unwrap_or_else(|e| panic!("jsonl line is not valid JSON: {e}; line={line}"));
    let schema = v["schema"].as_str().expect("schema field").to_owned();
    let event_type = v["type"].as_str().expect("type field").to_owned();
    (schema, event_type, v)
}

/// A two-stage DAG run emits the per-stage event sequence for each
/// stage plus a terminal `workflow.run` summary.
#[test]
fn two_stage_dag_jsonl_emits_events_and_run_summary() {
    let tmp = TempDir::new().unwrap();
    scaffold_two_stage_repo(tmp.path());

    let output = Command::new(bin())
        .current_dir(tmp.path())
        .env("CRAB_WORKFLOW_ENABLED", "1")
        .args(["run", "--jsonl"])
        .output()
        .expect("crab run --jsonl should spawn");
    assert!(
        output.status.success(),
        "crab run exited {}: stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );

    // Both stages ran end-to-end.
    assert_eq!(
        fs::read(tmp.path().join("b.txt")).unwrap(),
        b"payload".to_vec(),
    );
    assert_eq!(
        fs::read(tmp.path().join("c.txt")).unwrap(),
        b"payload".to_vec(),
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert!(
        !lines.is_empty(),
        "jsonl output must be non-empty: stderr={}",
        String::from_utf8_lossy(&output.stderr),
    );

    // Every line is valid JSON with `schema` and `type` fields.
    let parsed: Vec<(String, String, Value)> = lines.iter().map(|l| parse_line(l)).collect();

    // Sequence of `schema` values for miss-path stages (both stages
    // run from scratch on the first invocation). The exact per-stage
    // block is: started → cache_checked → produced → hashed →
    // committed. We check the per-stage ordering is preserved in the
    // output.
    let expected_stage_sequence = [
        "workflow.stage.started",
        "workflow.stage.cache_checked",
        "workflow.stage.produced",
        "workflow.stage.hashed",
        "workflow.stage.committed",
    ];

    let step1_schemas: Vec<&str> = parsed
        .iter()
        .filter(|(_, _, v)| v["data"]["stage"] == "step1")
        .map(|(s, _, _)| s.as_str())
        .collect();
    assert_eq!(
        step1_schemas, expected_stage_sequence,
        "step1 event sequence mismatch; full stream:\n{stdout}",
    );

    let step2_schemas: Vec<&str> = parsed
        .iter()
        .filter(|(_, _, v)| v["data"]["stage"] == "step2")
        .map(|(s, _, _)| s.as_str())
        .collect();
    assert_eq!(
        step2_schemas, expected_stage_sequence,
        "step2 event sequence mismatch; full stream:\n{stdout}",
    );

    // The terminal line is `workflow.run` with type `result`.
    let (terminal_schema, terminal_type, terminal_value) =
        parsed.last().expect("at least one line").clone();
    assert_eq!(terminal_schema, "workflow.run");
    assert_eq!(terminal_type, "result");

    // Both stages landed in the `succeeded` bin; `failed` and
    // `not_started` are empty.
    let succeeded: Vec<&str> = terminal_value["data"]["succeeded"]
        .as_array()
        .expect("succeeded array")
        .iter()
        .map(|x| x.as_str().unwrap())
        .collect();
    assert_eq!(succeeded, vec!["step1", "step2"]);
    assert!(
        terminal_value["data"]["failed"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    assert!(
        terminal_value["data"]["not_started"]
            .as_array()
            .unwrap()
            .is_empty(),
    );

    // Per-stage records preserve topological order and carry the
    // cache_hit / duration fields consumers expect.
    let stages = terminal_value["data"]["stages"]
        .as_array()
        .expect("stages array");
    assert_eq!(stages.len(), 2, "two successful stages");
    assert_eq!(stages[0]["stage_name"], "step1");
    assert_eq!(stages[0]["cache_hit"], false);
    assert_eq!(stages[1]["stage_name"], "step2");
    assert_eq!(stages[1]["cache_hit"], false);

    // duration_ms is a non-negative integer.
    assert!(
        terminal_value["data"]["duration_ms"].as_u64().is_some(),
        "duration_ms must be present: {terminal_value}",
    );

    // Cache-checked events report miss source as `"none"` on the
    // first run; consumers rely on this to distinguish fresh work
    // from re-materialization.
    let cache_events: Vec<&Value> = parsed
        .iter()
        .filter(|(s, _, _)| s == "workflow.stage.cache_checked")
        .map(|(_, _, v)| v)
        .collect();
    assert_eq!(cache_events.len(), 2);
    for ev in cache_events {
        assert_eq!(ev["data"]["hit"], false);
        assert_eq!(ev["data"]["hit_source"], "none");
    }
}

/// Running a DAG twice with unchanged inputs should produce cache
/// hits on the second run: `produced` / `hashed` events disappear
/// because the executor short-circuits, but `started` /
/// `cache_checked` / `committed` still fire.
#[test]
fn second_run_cache_hits_skip_produced_and_hashed_events() {
    let tmp = TempDir::new().unwrap();
    scaffold_two_stage_repo(tmp.path());

    // First run: miss path. Ignored here; its purpose is priming
    // the cache for the second run.
    let first = Command::new(bin())
        .current_dir(tmp.path())
        .env("CRAB_WORKFLOW_ENABLED", "1")
        .args(["run"])
        .status()
        .expect("first crab run should spawn");
    assert!(first.success(), "first run must succeed: {first:?}");

    // Second run: same inputs → cache hit on both stages.
    let output = Command::new(bin())
        .current_dir(tmp.path())
        .env("CRAB_WORKFLOW_ENABLED", "1")
        .args(["run", "--jsonl"])
        .output()
        .expect("second crab run should spawn");
    assert!(
        output.status.success(),
        "second run exited {}: stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr),
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let parsed: Vec<(String, String, Value)> = stdout.lines().map(|l| parse_line(l)).collect();

    // Cache hit path emits: started → cache_checked → committed
    // (no produced / hashed). This mirrors the state machine: on a
    // hit we never enter `Running`, so the stream skips straight
    // to the terminal `committed` transition.
    let cache_hit_sequence = [
        "workflow.stage.started",
        "workflow.stage.cache_checked",
        "workflow.stage.committed",
    ];
    for stage in ["step1", "step2"] {
        let schemas: Vec<&str> = parsed
            .iter()
            .filter(|(_, _, v)| v["data"]["stage"] == stage)
            .map(|(s, _, _)| s.as_str())
            .collect();
        assert_eq!(
            schemas, cache_hit_sequence,
            "{stage} hit-path event sequence mismatch; full stream:\n{stdout}",
        );
    }

    // Every cache_checked event reports a local hit.
    let cache_events: Vec<&Value> = parsed
        .iter()
        .filter(|(s, _, _)| s == "workflow.stage.cache_checked")
        .map(|(_, _, v)| v)
        .collect();
    assert_eq!(cache_events.len(), 2);
    for ev in cache_events {
        assert_eq!(ev["data"]["hit"], true);
        assert_eq!(ev["data"]["hit_source"], "local");
    }

    // Terminal summary still reports both stages in `succeeded`
    // with `cache_hit = true`.
    let (terminal_schema, terminal_type, terminal_value) = parsed.last().unwrap().clone();
    assert_eq!(terminal_schema, "workflow.run");
    assert_eq!(terminal_type, "result");
    let stages = terminal_value["data"]["stages"].as_array().unwrap();
    assert_eq!(stages.len(), 2);
    for stage in stages {
        assert_eq!(stage["cache_hit"], true);
    }
}
