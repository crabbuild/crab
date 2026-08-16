//! Integration tests for `crab run --validate`.
//!
//! Exercises the validation layers: unknown YAML keys, invalid
//! timeout values, self-loops, duplicate outs, and the happy path.

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

/// Run `crab run --validate` in the given directory and return
/// (exit status, stdout, stderr).
fn run_validate(repo: &Path) -> (std::process::ExitStatus, String, String) {
    let output = Command::new(bin())
        .current_dir(repo)
        .env("CRAB_WORKFLOW_ENABLED", "1")
        .args(["run", "--validate"])
        .output()
        .expect("crab run --validate should spawn");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    (output.status, stdout, stderr)
}

fn parse_validate_data(stdout: &str) -> serde_json::Value {
    let envelope: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("parse stdout failed: {e}; stdout={stdout:?}"));
    assert_eq!(envelope["schema"], "workflow.validate");
    envelope["data"].clone()
}

fn parse_validate_errors(stdout: &str) -> Vec<serde_json::Value> {
    serde_json::from_value(parse_validate_data(stdout))
        .unwrap_or_else(|e| panic!("parse validation errors failed: {e}; stdout={stdout:?}"))
}

/// Write a `crab.yaml` to the given directory.
fn write_yaml(dir: &Path, content: &str) {
    fs::write(dir.join("crab.yaml"), content).unwrap();
}

#[test]
fn valid_yaml_exits_zero_with_valid_true() {
    let tmp = TempDir::new().unwrap();
    write_yaml(
        tmp.path(),
        r#"
stages:
  clean:
    cmd: "echo clean"
    deps:
      - input.txt
    outs:
      - output.txt
"#,
    );
    // Create the dep file so discovery works.
    fs::write(tmp.path().join("input.txt"), b"data").unwrap();

    let (status, stdout, _stderr) = run_validate(tmp.path());
    assert!(
        status.success(),
        "expected exit 0 for valid yaml, got {:?}\nstdout: {stdout}",
        status.code()
    );
    let parsed = parse_validate_data(&stdout);
    assert_eq!(parsed["valid"], true);
}

#[test]
fn unknown_top_level_key_exits_two() {
    let tmp = TempDir::new().unwrap();
    write_yaml(
        tmp.path(),
        r#"
bogus_key: true
stages:
  clean:
    cmd: "echo clean"
"#,
    );

    let (status, stdout, _stderr) = run_validate(tmp.path());
    assert_eq!(
        status.code(),
        Some(2),
        "expected exit 2 for unknown key\nstdout: {stdout}"
    );
    let errors = parse_validate_errors(&stdout);
    assert!(!errors.is_empty(), "expected at least one error");
    // The error should mention the unknown key.
    let first = &errors[0];
    assert_eq!(first["kind"], "WorkflowYamlUnknownKey");
    let msg = first["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("bogus_key") || msg.contains("unknown field"),
        "error message should mention the unknown key: {msg}"
    );
}

#[test]
fn unknown_stage_field_exits_two() {
    let tmp = TempDir::new().unwrap();
    write_yaml(
        tmp.path(),
        r#"
stages:
  clean:
    cmd: "echo clean"
    bogus_field: true
"#,
    );

    let (status, stdout, _stderr) = run_validate(tmp.path());
    assert_eq!(
        status.code(),
        Some(2),
        "expected exit 2 for unknown stage field\nstdout: {stdout}"
    );
    let errors = parse_validate_errors(&stdout);
    assert!(!errors.is_empty());
    let first = &errors[0];
    assert_eq!(first["kind"], "WorkflowYamlUnknownKey");
}

#[test]
fn self_loop_dep_equals_own_out_exits_two() {
    let tmp = TempDir::new().unwrap();
    write_yaml(
        tmp.path(),
        r#"
stages:
  train:
    cmd: "echo train"
    deps:
      - model.bin
    outs:
      - model.bin
"#,
    );

    let (status, stdout, _stderr) = run_validate(tmp.path());
    assert_eq!(
        status.code(),
        Some(2),
        "expected exit 2 for self-loop\nstdout: {stdout}"
    );
    let errors = parse_validate_errors(&stdout);
    assert!(!errors.is_empty());
    // Should report a WorkflowCycle (self-loop).
    let has_cycle = errors.iter().any(|e| e["kind"] == "WorkflowCycle");
    assert!(
        has_cycle,
        "expected a WorkflowCycle error for self-loop, got: {errors:?}"
    );
}

#[test]
fn invalid_timeout_value_exits_two() {
    let tmp = TempDir::new().unwrap();
    write_yaml(
        tmp.path(),
        r#"
stages:
  train:
    cmd: "echo train"
    timeout: "banana"
"#,
    );

    let (status, stdout, _stderr) = run_validate(tmp.path());
    assert_eq!(
        status.code(),
        Some(2),
        "expected exit 2 for invalid timeout\nstdout: {stdout}"
    );
    let errors = parse_validate_errors(&stdout);
    assert!(!errors.is_empty());
    // The error should mention the invalid timeout.
    let msg = errors[0]["message"].as_str().unwrap_or("");
    assert!(
        msg.contains("timeout") || msg.contains("banana") || msg.contains("duration"),
        "error message should mention timeout issue: {msg}"
    );
}

#[test]
fn duplicate_outs_across_stages_exits_two() {
    let tmp = TempDir::new().unwrap();
    write_yaml(
        tmp.path(),
        r#"
stages:
  clean:
    cmd: "echo clean"
    outs:
      - shared.txt
  transform:
    cmd: "echo transform"
    outs:
      - shared.txt
"#,
    );

    let (status, stdout, _stderr) = run_validate(tmp.path());
    assert_eq!(
        status.code(),
        Some(2),
        "expected exit 2 for duplicate outs\nstdout: {stdout}"
    );
    let errors = parse_validate_errors(&stdout);
    assert!(!errors.is_empty());
    let has_dup = errors
        .iter()
        .any(|e| e["kind"] == "WorkflowDuplicateOutput");
    assert!(
        has_dup,
        "expected a WorkflowDuplicateOutput error, got: {errors:?}"
    );
}

#[test]
fn invalid_stage_name_exits_two() {
    let tmp = TempDir::new().unwrap();
    write_yaml(
        tmp.path(),
        r#"
stages:
  "has spaces":
    cmd: "echo bad"
"#,
    );

    let (status, stdout, _stderr) = run_validate(tmp.path());
    assert_eq!(
        status.code(),
        Some(2),
        "expected exit 2 for invalid stage name\nstdout: {stdout}"
    );
    let errors = parse_validate_errors(&stdout);
    assert!(!errors.is_empty());
}

#[test]
fn reports_all_errors_not_fail_fast() {
    let tmp = TempDir::new().unwrap();
    // This yaml has a self-loop AND duplicate outs — both should be reported.
    write_yaml(
        tmp.path(),
        r#"
stages:
  clean:
    cmd: "echo clean"
    deps:
      - output.txt
    outs:
      - output.txt
      - shared.txt
  transform:
    cmd: "echo transform"
    outs:
      - shared.txt
"#,
    );

    let (status, stdout, _stderr) = run_validate(tmp.path());
    assert_eq!(status.code(), Some(2), "expected exit 2\nstdout: {stdout}");
    let errors = parse_validate_errors(&stdout);
    // Should have at least 2 errors: self-loop + duplicate out.
    assert!(
        errors.len() >= 2,
        "expected at least 2 errors (self-loop + duplicate out), got {}: {errors:?}",
        errors.len()
    );
}

#[test]
fn deny_unknown_fields_on_retry_block() {
    let tmp = TempDir::new().unwrap();
    write_yaml(
        tmp.path(),
        r#"
stages:
  train:
    cmd: "echo train"
    retry:
      max_attempts: 3
      bogus_retry_field: true
"#,
    );

    let (status, stdout, _stderr) = run_validate(tmp.path());
    assert_eq!(
        status.code(),
        Some(2),
        "expected exit 2 for unknown retry field\nstdout: {stdout}"
    );
    let errors = parse_validate_errors(&stdout);
    assert!(!errors.is_empty());
    assert_eq!(errors[0]["kind"], "WorkflowYamlUnknownKey");
}
