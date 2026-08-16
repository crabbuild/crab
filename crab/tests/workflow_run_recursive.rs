//! Integration tests for workflow discovery (R2).
//!
//! Covers the two CLI-visible behaviors:
//!
//! - Default (Root) mode rejects a repo that contains nested
//!   `crab.yaml` files with `WorkflowDiscoveryAmbiguous`.
//! - `--recursive` merges every discovered yaml, prefixing nested
//!   stage names with their containing directory joined by dots,
//!   and the DAG runs both sets end to end.

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

/// Default discovery mode refuses a repo with a root + nested yaml.
/// The error surfaces as `CRAB-E0204` (WorkflowDiscoveryAmbiguous)
/// so consumers reading structured output see a stable code.
#[test]
fn default_mode_rejects_nested_crab_yaml() {
    let tmp = TempDir::new().unwrap();

    // Root yaml: minimal stage so the parse is well-formed.
    fs::write(
        tmp.path().join("crab.yaml"),
        "stages:\n  root_stage:\n    cmd:\n      argv: [\"/usr/bin/true\"]\n",
    )
    .unwrap();
    // Nested yaml: ditto.
    fs::create_dir_all(tmp.path().join("data")).unwrap();
    fs::write(
        tmp.path().join("data/crab.yaml"),
        "stages:\n  clean:\n    cmd:\n      argv: [\"/usr/bin/true\"]\n",
    )
    .unwrap();

    let output = Command::new(bin())
        .current_dir(tmp.path())
        .env("CRAB_WORKFLOW_ENABLED", "1")
        .args(["run"])
        .output()
        .expect("crab run should spawn");

    assert!(
        !output.status.success(),
        "expected failure under default discover mode: {output:?}"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("CRAB-E0204") || stderr.contains("ambiguous workflow discovery"),
        "expected WorkflowDiscoveryAmbiguous error, got stderr: {stderr}"
    );
}

/// `--recursive` merges every `crab.yaml` into one DAG. A nested
/// stage's cmd runs successfully and its out — written relative to
/// the nested directory — appears on disk at the rewritten,
/// repo-relative path.
#[test]
fn recursive_mode_merges_nested_yaml_and_runs_dag() {
    let tmp = TempDir::new().unwrap();

    // Root yaml: stage that copies a repo-root input to a
    // repo-root output.
    fs::write(tmp.path().join("root_input.txt"), b"root-payload").unwrap();
    let root_yaml = format!(
        "stages:\n  root_stage:\n    cmd:\n      argv: [\"/bin/cp\", \"{src}\", \"{dst}\"]\n    deps:\n      - root_input.txt\n    outs:\n      - root_output.txt\n",
        src = tmp.path().join("root_input.txt").to_string_lossy(),
        dst = tmp.path().join("root_output.txt").to_string_lossy(),
    );
    fs::write(tmp.path().join("crab.yaml"), root_yaml).unwrap();

    // Nested yaml under data/: its dep and out paths are relative
    // to `data/`, but the merged workflow rewrites them to
    // repo-relative form (`data/raw.csv`, `data/clean.out`).
    fs::create_dir_all(tmp.path().join("data")).unwrap();
    fs::write(tmp.path().join("data/raw.csv"), b"nested-payload").unwrap();
    let nested_yaml = format!(
        "stages:\n  clean:\n    cmd:\n      argv: [\"/bin/cp\", \"{src}\", \"{dst}\"]\n    deps:\n      - raw.csv\n    outs:\n      - clean.out\n",
        src = tmp.path().join("data/raw.csv").to_string_lossy(),
        dst = tmp.path().join("data/clean.out").to_string_lossy(),
    );
    fs::write(tmp.path().join("data/crab.yaml"), nested_yaml).unwrap();

    let output = Command::new(bin())
        .current_dir(tmp.path())
        .env("CRAB_WORKFLOW_ENABLED", "1")
        .args(["run", "--recursive"])
        .output()
        .expect("crab run should spawn");

    assert!(
        output.status.success(),
        "recursive DAG run should succeed. stdout: {} stderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // Both stages' outs must be on disk at the rewritten repo-
    // relative paths.
    assert_eq!(
        fs::read(tmp.path().join("root_output.txt")).unwrap(),
        b"root-payload".to_vec(),
        "root stage produced its out"
    );
    assert_eq!(
        fs::read(tmp.path().join("data/clean.out")).unwrap(),
        b"nested-payload".to_vec(),
        "nested stage produced its out under the rewritten path"
    );

    // Lockfile entries must cover both stages' effective names.
    let lock = fs::read_to_string(tmp.path().join("crab.lock"))
        .expect("crab.lock written after recursive run");
    assert!(
        lock.contains("root_stage"),
        "lockfile missing root_stage: {lock}"
    );
    assert!(
        lock.contains("data.clean"),
        "lockfile missing nested data.clean: {lock}"
    );
}

/// `CRAB_WORKFLOW_DISCOVER=recursive` alone (without `--recursive`)
/// opts in via config so ops who set it system-wide don't have to
/// pass the flag every time.
#[test]
fn env_var_discover_recursive_opts_in_without_flag() {
    let tmp = TempDir::new().unwrap();

    // Minimal stages using cmds that exist on every unix we target.
    // `/usr/bin/true` is present on macOS and Linux; `/bin/true`
    // ships on Linux but not macOS. Argv form bypasses the shell so
    // there's no PATH resolution surprise.
    fs::write(
        tmp.path().join("crab.yaml"),
        "stages:\n  root_stage:\n    cmd:\n      argv: [\"/usr/bin/true\"]\n",
    )
    .unwrap();
    fs::create_dir_all(tmp.path().join("data")).unwrap();
    fs::write(
        tmp.path().join("data/crab.yaml"),
        "stages:\n  clean:\n    cmd:\n      argv: [\"/usr/bin/true\"]\n",
    )
    .unwrap();

    let output = Command::new(bin())
        .current_dir(tmp.path())
        .env("CRAB_WORKFLOW_ENABLED", "1")
        .env("CRAB_WORKFLOW_DISCOVER", "recursive")
        .args(["run"])
        .output()
        .expect("crab run should spawn");

    // With the env-var opt-in, recursive discovery is active and the
    // run succeeds even though both yamls exist. We only assert
    // success here — the deeper merging behavior is covered by
    // `recursive_mode_merges_nested_yaml_and_runs_dag`.
    assert!(
        output.status.success(),
        "config-opt-in recursive mode should succeed. stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
