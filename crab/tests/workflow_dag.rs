//! Integration tests for `crab workflow dag`.
//!
//! Exercises the CLI end-to-end: scaffold a repo with a multi-stage
//! `crab.yaml`, run `crab workflow dag` in each format, and
//! assert the output shape. The JSON envelope is the strongest
//! contract — it pins down stage order and edge list — so we drive
//! most assertions off of it.

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

/// Scaffold a repo with a three-stage linear pipeline
/// (`clean → train → report`). Each stage declares a single out and
/// each downstream stage depends on the previous stage's out, so
/// the DAG has a deterministic topological order.
fn scaffold_linear_repo(root: &Path) {
    fs::write(
        root.join("crab.yaml"),
        "stages:\n  \
            clean:\n    cmd: \"true\"\n    deps:\n      - raw.csv\n    outs:\n      - clean.csv\n  \
            train:\n    cmd: \"true\"\n    deps:\n      - clean.csv\n    outs:\n      - model.pkl\n  \
            report:\n    cmd: \"true\"\n    deps:\n      - model.pkl\n    outs:\n      - report.html\n",
    )
    .unwrap();
    fs::write(root.join("raw.csv"), b"id,value\n1,42\n").unwrap();

    fs::create_dir_all(root.join(".crab")).unwrap();
    fs::write(
        root.join(".crab").join("config.toml"),
        "[workflow]\nenabled = true\n",
    )
    .unwrap();
}

fn run_dag(repo: &Path, extra: &[&str]) -> (std::process::Output, String) {
    let mut cmd = Command::new(bin());
    cmd.current_dir(repo).args(["workflow", "dag"]);
    for a in extra {
        cmd.arg(a);
    }
    let output = cmd.output().expect("crab workflow dag should spawn");
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    (output, stderr)
}

#[test]
fn ascii_output_lists_stages_in_topological_order() {
    let tmp = TempDir::new().unwrap();
    scaffold_linear_repo(tmp.path());

    let (output, stderr) = run_dag(tmp.path(), &[]);
    assert!(
        output.status.success(),
        "dag exited {}: stderr={stderr}",
        output.status
    );
    let stdout = String::from_utf8_lossy(&output.stdout);

    // Stage names appear in the expected topological order.
    let clean_pos = stdout.find("clean").expect("clean in ascii");
    let train_pos = stdout.find("train").expect("train in ascii");
    let report_pos = stdout.find("report").expect("report in ascii");
    assert!(
        clean_pos < train_pos && train_pos < report_pos,
        "stages out of topo order:\n{stdout}"
    );
    // Source stage carries the no-deps placeholder.
    assert!(
        stdout.contains("└─ (no deps)"),
        "source marker missing:\n{stdout}"
    );
    // Downstream stages list their direct producer.
    assert!(stdout.contains("└─ clean"), "clean → train edge missing");
    assert!(stdout.contains("└─ train"), "train → report edge missing");
}

#[test]
fn mermaid_output_is_parseable_graph_td_block() {
    let tmp = TempDir::new().unwrap();
    scaffold_linear_repo(tmp.path());

    let (output, stderr) = run_dag(tmp.path(), &["--format", "mermaid"]);
    assert!(
        output.status.success(),
        "dag exited {}: stderr={stderr}",
        output.status
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.starts_with("graph TD\n"),
        "missing header:\n{stdout}"
    );
    // One collision-safe node ID per stage label.
    assert!(stdout.contains("    node1[\"clean\"]\n"));
    assert!(stdout.contains("    node2[\"train\"]\n"));
    assert!(stdout.contains("    node3[\"report\"]\n"));
    // Edges in producer → consumer order.
    assert!(stdout.contains("    node1 --> node2\n"));
    assert!(stdout.contains("    node2 --> node3\n"));
}

#[test]
fn json_envelope_pins_down_stage_and_edge_order() {
    let tmp = TempDir::new().unwrap();
    scaffold_linear_repo(tmp.path());

    let (output, stderr) = run_dag(tmp.path(), &["--json"]);
    assert!(
        output.status.success(),
        "dag exited {}: stderr={stderr}",
        output.status
    );
    let envelope: Value =
        serde_json::from_slice(&output.stdout).expect("dag --json must parse as JSON");
    assert_eq!(envelope["schema"], "workflow.dag");
    let stages: Vec<String> = envelope["data"]["stages"]
        .as_array()
        .expect("stages array")
        .iter()
        .map(|v| v["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(stages, vec!["clean", "train", "report"]);

    // Non-expanded stages should have expanded: false.
    for stage_val in envelope["data"]["stages"].as_array().unwrap() {
        assert_eq!(
            stage_val["expanded"].as_bool(),
            Some(false),
            "non-expanded stage should have expanded: false"
        );
    }

    let edges: Vec<(String, String)> = envelope["data"]["edges"]
        .as_array()
        .expect("edges array")
        .iter()
        .map(|e| {
            (
                e["from"].as_str().unwrap().to_owned(),
                e["to"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        edges,
        vec![
            ("clean".to_owned(), "train".to_owned()),
            ("train".to_owned(), "report".to_owned()),
        ]
    );
}

#[test]
fn fails_when_workflow_disabled() {
    // A repo without `[workflow] enabled = true` should surface the
    // same gating error as every other workflow subcommand.
    let tmp = TempDir::new().unwrap();
    fs::write(
        tmp.path().join("crab.yaml"),
        "stages:\n  clean:\n    cmd: \"true\"\n",
    )
    .unwrap();

    let (output, _) = run_dag(tmp.path(), &[]);
    assert!(
        !output.status.success(),
        "dag should fail when workflow layer is disabled"
    );
}
