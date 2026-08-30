//! Integration tests for `crab workflow status`.
//!
//! Exercises the CLI end-to-end with a minimal crab.yaml + dep on
//! disk, an empty lockfile, and a modified-dep scenario. Verifies
//! the three main state buckets — up-to-date, stale, never-run —
//! through the structured `--json` envelope.

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

/// Build a repo scaffold with a crab.yaml declaring one stage.
fn scaffold_repo(root: &Path) {
    fs::write(
        root.join("crab.yaml"),
        "stages:\n  build:\n    cmd: \"cp a.txt b.txt\"\n    deps:\n      - a.txt\n    outs:\n      - b.txt\n    env: empty\n",
    )
    .unwrap();
    fs::write(root.join("a.txt"), b"hello").unwrap();

    // Workflow layer opt-in. The gate lives in `.crab/local.toml`
    // and mirrors the other integration tests in this directory.
    fs::create_dir_all(root.join(".crab")).unwrap();
    fs::write(
        root.join(".crab").join("config.toml"),
        "[workflow]\nenabled = true\n",
    )
    .unwrap();
}

fn scaffold_pipeline_repo(root: &Path) {
    fs::write(
        root.join("crab.yaml"),
        "stages:\n  clean:\n    cmd: \"cp raw.txt clean.txt\"\n    deps:\n      - raw.txt\n    outs:\n      - clean.txt\n    env: empty\n  train:\n    cmd: \"cp clean.txt model.txt\"\n    deps:\n      - clean.txt\n    outs:\n      - model.txt\n    env: empty\n",
    )
    .unwrap();
    fs::write(root.join("raw.txt"), b"raw").unwrap();
    fs::write(root.join("clean.txt"), b"clean").unwrap();
    fs::create_dir_all(root.join(".crab")).unwrap();
    fs::write(
        root.join(".crab").join("config.toml"),
        "[workflow]\nenabled = true\n",
    )
    .unwrap();
}

fn run_status(repo: &Path) -> Value {
    let output = Command::new(bin())
        .current_dir(repo)
        .args(["workflow", "status", "--json"])
        .output()
        .expect("crab workflow status should spawn");
    assert!(
        output.status.success(),
        "status exited {}: stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("status output must parse as JSON")
}

fn run_top_level_workflow_status(repo: &Path) -> Value {
    run_top_level_workflow_status_args(repo, &[])
}

fn run_top_level_workflow_status_args(repo: &Path, args: &[&str]) -> Value {
    let mut command_args = vec!["status", "--workflow", "--json"];
    command_args.extend(args);
    let output = Command::new(bin())
        .current_dir(repo)
        .args(command_args)
        .output()
        .expect("crab status --workflow should spawn");
    assert!(
        output.status.success(),
        "status --workflow exited {}: stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("status --workflow output must parse as JSON")
}

fn run_top_level_workflow_why(repo: &Path, stage: &str) -> Value {
    let output = Command::new(bin())
        .current_dir(repo)
        .args(["status", "--workflow", "--json", "--why", stage])
        .output()
        .expect("crab status --workflow --why should spawn");
    assert!(
        output.status.success(),
        "status --workflow --why exited {}: stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout)
        .expect("status --workflow --why output must parse as JSON")
}

fn run_why(repo: &Path, stage: &str) -> Value {
    let output = Command::new(bin())
        .current_dir(repo)
        .args(["workflow", "status", "--json", "--why", stage])
        .output()
        .expect("crab workflow status --why should spawn");
    assert!(
        output.status.success(),
        "status --why exited {}: stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("status --why output must parse as JSON")
}

#[test]
fn top_level_status_workflow_reports_workflow_state() {
    let tmp = TempDir::new().unwrap();
    scaffold_repo(tmp.path());

    let json = run_top_level_workflow_status(tmp.path());
    assert_eq!(json["schema"], "workflow.status");
    let stages = json["data"]["stages"].as_array().expect("stages array");
    assert_eq!(stages.len(), 1);
    assert_eq!(stages[0]["stage"], "build");
    assert_eq!(stages[0]["state"], "never_run");
}

#[test]
fn top_level_status_workflow_why_uses_workflow_status_explainer() {
    let tmp = TempDir::new().unwrap();
    scaffold_repo(tmp.path());

    let json = run_top_level_workflow_why(tmp.path(), "build");
    assert_eq!(json["schema"], "workflow.status");
    assert_eq!(json["data"]["stage"], "build");
    assert_eq!(json["data"]["up_to_date"], false);
    assert!(json["data"].get("lockfile_stage_hash").is_none());
}

#[test]
fn top_level_status_workflow_accepts_stage_target() {
    let tmp = TempDir::new().unwrap();
    scaffold_pipeline_repo(tmp.path());

    let json = run_top_level_workflow_status_args(tmp.path(), &["train"]);
    let stages = json["data"]["stages"].as_array().expect("stages array");
    assert_eq!(stages.len(), 1);
    assert_eq!(stages[0]["stage"], "train");
}

#[test]
fn top_level_status_workflow_accepts_output_path_target() {
    let tmp = TempDir::new().unwrap();
    scaffold_pipeline_repo(tmp.path());

    let json = run_top_level_workflow_status_args(tmp.path(), &["model.txt"]);
    let stages = json["data"]["stages"].as_array().expect("stages array");
    assert_eq!(stages.len(), 1);
    assert_eq!(stages[0]["stage"], "train");
}

#[test]
fn top_level_status_workflow_with_deps_includes_upstream() {
    let tmp = TempDir::new().unwrap();
    scaffold_pipeline_repo(tmp.path());

    let json = run_top_level_workflow_status_args(tmp.path(), &["--with-deps", "model.txt"]);
    let stages = json["data"]["stages"].as_array().expect("stages array");
    let names: Vec<&str> = stages
        .iter()
        .map(|stage| stage["stage"].as_str().expect("stage name"))
        .collect();
    assert_eq!(names, ["clean", "train"]);
}

#[test]
fn reports_never_run_without_lockfile() {
    let tmp = TempDir::new().unwrap();
    scaffold_repo(tmp.path());

    let json = run_status(tmp.path());
    assert_eq!(json["schema"], "workflow.status");
    let stages = json["data"]["stages"].as_array().expect("stages array");
    assert_eq!(stages.len(), 1);
    assert_eq!(stages[0]["stage"], "build");
    assert_eq!(stages[0]["state"], "never_run");
    assert!(stages[0]["lockfile_stage_hash"].is_null());
    assert!(!stages[0]["stage_hash"].is_null());
}

#[test]
fn reports_stale_after_dep_modification() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    scaffold_repo(root);

    // Seed a lockfile that matches the current dep hash — stage
    // reports up-to-date, then we mutate the dep and rerun to
    // observe the stale transition.
    let status_before = run_status(root);
    let stages_before = status_before["data"]["stages"].as_array().unwrap();
    assert_eq!(stages_before[0]["state"], "never_run");

    // Trigger a `crab run` to materialize `crab.lock`. Use the
    // single-stage-from-yaml mode so the test doesn't depend on
    // lockfile-writing internals.
    let run = Command::new(bin())
        .current_dir(root)
        .args(["run", "build"])
        .output()
        .expect("crab run should spawn");
    assert!(
        run.status.success(),
        "run failed: {}",
        String::from_utf8_lossy(&run.stderr)
    );
    assert!(root.join("crab.lock").exists(), "lockfile must be written");

    let status_after_run = run_status(root);
    let stages_after = status_after_run["data"]["stages"].as_array().unwrap();
    assert_eq!(stages_after[0]["state"], "up_to_date");

    // Mutate the dep — status should now report stale-dep.
    fs::write(root.join("a.txt"), b"changed").unwrap();
    let status_stale = run_status(root);
    let stages_stale = status_stale["data"]["stages"].as_array().unwrap();
    assert_eq!(stages_stale[0]["state"], "stale");
    assert_eq!(stages_stale[0]["reason"], "dep");
    assert_eq!(stages_stale[0]["changed_key"], "a.txt");
}

#[test]
fn why_payload_details_dep_diff() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    scaffold_repo(root);

    let run = Command::new(bin())
        .current_dir(root)
        .args(["run", "build"])
        .output()
        .expect("crab run should spawn");
    assert!(run.status.success());

    // Change the dep so `--why` has something to diff against.
    fs::write(root.join("a.txt"), b"changed").unwrap();

    let json = run_why(root, "build");
    assert_eq!(json["schema"], "workflow.status");
    let data = &json["data"];
    assert_eq!(data["stage"], "build");
    assert_eq!(data["up_to_date"], false);
    let diffs = data["diffs"].as_array().expect("diffs array");
    let dep_diff = diffs
        .iter()
        .find(|d| d["category"] == "dep" && d["key"] == "a.txt")
        .expect("dep diff entry");
    assert!(dep_diff["current"].is_string());
    assert!(dep_diff["lockfile"].is_string());
    assert_ne!(dep_diff["current"], dep_diff["lockfile"]);
}

#[test]
fn reports_stale_after_param_modification() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    fs::write(
        root.join("crab.yaml"),
        "params:\n  - params.yaml\nstages:\n  train:\n    cmd: \"cp params.yaml result.txt\"\n    params:\n      - model.lr\n    outs:\n      - result.txt\n    env: empty\n",
    )
    .unwrap();
    fs::write(root.join("params.yaml"), b"model:\n  lr: 0.01\n").unwrap();
    fs::create_dir_all(root.join(".crab")).unwrap();
    fs::write(
        root.join(".crab").join("config.toml"),
        "[workflow]\nenabled = true\n",
    )
    .unwrap();

    let run = Command::new(bin())
        .current_dir(root)
        .args(["run", "train"])
        .output()
        .expect("crab run should spawn");
    assert!(
        run.status.success(),
        "run failed: {}",
        String::from_utf8_lossy(&run.stderr)
    );

    let status_after_run = run_status(root);
    let stages_after = status_after_run["data"]["stages"].as_array().unwrap();
    assert_eq!(stages_after[0]["state"], "up_to_date");

    fs::write(root.join("params.yaml"), b"model:\n  lr: 0.02\n").unwrap();

    let status_stale = run_status(root);
    let stages_stale = status_stale["data"]["stages"].as_array().unwrap();
    assert_eq!(stages_stale[0]["state"], "stale");
    assert_eq!(stages_stale[0]["reason"], "param");
    assert_eq!(stages_stale[0]["changed_key"], "model.lr");

    let why = run_why(root, "train");
    let param_diff = why["data"]["diffs"]
        .as_array()
        .expect("diffs array")
        .iter()
        .find(|d| d["category"] == "param" && d["key"] == "model.lr")
        .expect("param diff entry");
    assert_eq!(param_diff["current"], "0.02");
    assert_eq!(param_diff["lockfile"], "0.01");
}

#[test]
fn reports_stale_after_wdir_param_modification() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    let training = root.join("training");
    fs::create_dir_all(&training).unwrap();

    fs::write(
        root.join("crab.yaml"),
        "stages:\n  train:\n    cmd: \"cp params.yaml result.txt\"\n    wdir: training\n    params:\n      - model.lr\n    outs:\n      - result.txt\n    env: empty\n",
    )
    .unwrap();
    fs::write(root.join("params.yaml"), b"model:\n  lr: 9.99\n").unwrap();
    fs::write(training.join("params.yaml"), b"model:\n  lr: 0.01\n").unwrap();
    fs::create_dir_all(root.join(".crab")).unwrap();
    fs::write(
        root.join(".crab").join("config.toml"),
        "[workflow]\nenabled = true\n",
    )
    .unwrap();

    let run = Command::new(bin())
        .current_dir(root)
        .args(["run", "train"])
        .output()
        .expect("crab run should spawn");
    assert!(
        run.status.success(),
        "run failed: {}",
        String::from_utf8_lossy(&run.stderr)
    );

    let status_after_run = run_status(root);
    let stages_after = status_after_run["data"]["stages"].as_array().unwrap();
    assert_eq!(stages_after[0]["state"], "up_to_date");

    fs::write(training.join("params.yaml"), b"model:\n  lr: 0.02\n").unwrap();

    let status_stale = run_status(root);
    let stages_stale = status_stale["data"]["stages"].as_array().unwrap();
    assert_eq!(stages_stale[0]["state"], "stale");
    assert_eq!(stages_stale[0]["reason"], "param");
    assert_eq!(stages_stale[0]["changed_key"], "model.lr");

    let why = run_why(root, "train");
    let param_diff = why["data"]["diffs"]
        .as_array()
        .expect("diffs array")
        .iter()
        .find(|d| d["category"] == "param" && d["key"] == "model.lr")
        .expect("param diff entry");
    assert_eq!(param_diff["current"], "0.02");
    assert_eq!(param_diff["lockfile"], "0.01");
}

#[test]
fn reports_stale_after_python_param_modification() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    fs::write(
        root.join("crab.yaml"),
        "params:\n  - params.py\nstages:\n  train:\n    cmd: \"cp params.py result.txt\"\n    params:\n      - model.lr\n    outs:\n      - result.txt\n    env: empty\n",
    )
    .unwrap();
    fs::write(root.join("params.py"), b"model = {'lr': 0.01}\n").unwrap();
    fs::create_dir_all(root.join(".crab")).unwrap();
    fs::write(
        root.join(".crab").join("config.toml"),
        "[workflow]\nenabled = true\n",
    )
    .unwrap();

    let run = Command::new(bin())
        .current_dir(root)
        .args(["run", "train"])
        .output()
        .expect("crab run should spawn");
    assert!(
        run.status.success(),
        "run failed: {}",
        String::from_utf8_lossy(&run.stderr)
    );

    let status_after_run = run_status(root);
    let stages_after = status_after_run["data"]["stages"].as_array().unwrap();
    assert_eq!(stages_after[0]["state"], "up_to_date");

    fs::write(root.join("params.py"), b"model = {'lr': 0.02}\n").unwrap();

    let status_stale = run_status(root);
    let stages_stale = status_stale["data"]["stages"].as_array().unwrap();
    assert_eq!(stages_stale[0]["state"], "stale");
    assert_eq!(stages_stale[0]["reason"], "param");
    assert_eq!(stages_stale[0]["changed_key"], "model.lr");

    let why = run_why(root, "train");
    let param_diff = why["data"]["diffs"]
        .as_array()
        .expect("diffs array")
        .iter()
        .find(|d| d["category"] == "param" && d["key"] == "model.lr")
        .expect("python param diff entry");
    assert_eq!(param_diff["current"], "0.02");
    assert_eq!(param_diff["lockfile"], "0.01");
}

#[test]
fn reports_stale_after_file_scoped_param_modification() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    fs::write(
        root.join("crab.yaml"),
        "stages:\n  train:\n    cmd: \"cp custom.yaml result.txt\"\n    params:\n      - custom.yaml:\n          - model.lr\n    outs:\n      - result.txt\n    env: empty\n",
    )
    .unwrap();
    fs::write(root.join("custom.yaml"), b"model:\n  lr: 0.01\n").unwrap();
    fs::create_dir_all(root.join(".crab")).unwrap();
    fs::write(
        root.join(".crab").join("config.toml"),
        "[workflow]\nenabled = true\n",
    )
    .unwrap();

    let run = Command::new(bin())
        .current_dir(root)
        .args(["run", "train"])
        .output()
        .expect("crab run should spawn");
    assert!(
        run.status.success(),
        "run failed: {}",
        String::from_utf8_lossy(&run.stderr)
    );

    fs::write(root.join("custom.yaml"), b"model:\n  lr: 0.02\n").unwrap();

    let status_stale = run_status(root);
    let stages_stale = status_stale["data"]["stages"].as_array().unwrap();
    assert_eq!(stages_stale[0]["state"], "stale");
    assert_eq!(stages_stale[0]["reason"], "param");
    assert_eq!(stages_stale[0]["changed_key"], "custom.yaml:model.lr");

    let why = run_why(root, "train");
    let param_diff = why["data"]["diffs"]
        .as_array()
        .expect("diffs array")
        .iter()
        .find(|d| d["category"] == "param" && d["key"] == "custom.yaml:model.lr")
        .expect("file-scoped param diff entry");
    assert_eq!(param_diff["current"], "0.02");
    assert_eq!(param_diff["lockfile"], "0.01");
}
