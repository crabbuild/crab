//! Integration tests for plot configuration (Task 4.5) and
//! conditional stages (Task 4.6).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]

use std::fs;
use std::path::{Path, PathBuf};
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

/// Write a `crab.yaml` to the given directory.
fn write_yaml(dir: &Path, content: &str) {
    fs::write(dir.join("crab.yaml"), content).unwrap();
}

// ─── Task 4.5: Plot configuration tests ───────────────────────────────

#[test]
fn plot_config_simple_path_validates() {
    let tmp = TempDir::new().unwrap();
    write_yaml(
        tmp.path(),
        r#"
plots:
  - metrics/loss.csv
stages:
  train:
    cmd: "echo train"
"#,
    );
    let (status, _stdout, _stderr) = run_validate(tmp.path());
    assert!(status.success(), "simple plot path should validate");
}

#[test]
fn plot_config_structured_validates() {
    let tmp = TempDir::new().unwrap();
    write_yaml(
        tmp.path(),
        r#"
plots:
  - metrics/loss.csv:
      x: epoch
      y: [train_loss, val_loss]
      title: "Training Loss"
  - metrics/roc.json:
      x: fpr
      y: tpr
      template: linear
stages:
  train:
    cmd: "echo train"
"#,
    );
    let (status, _stdout, stderr) = run_validate(tmp.path());
    assert!(
        status.success(),
        "structured plot config should validate; stderr={stderr}"
    );
}

#[test]
fn plot_config_mixed_simple_and_structured_validates() {
    let tmp = TempDir::new().unwrap();
    write_yaml(
        tmp.path(),
        r#"
plots:
  - metrics/simple.csv
  - metrics/loss.csv:
      x: epoch
      y: [train_loss, val_loss]
      title: "Training Loss"
stages:
  train:
    cmd: "echo train"
"#,
    );
    let (status, _stdout, stderr) = run_validate(tmp.path());
    assert!(
        status.success(),
        "mixed plot config should validate; stderr={stderr}"
    );
}

#[test]
fn plot_config_parses_correctly_via_library() {
    let yaml = r#"
plots:
  - metrics/loss.csv:
      x: epoch
      y: [train_loss, val_loss]
      title: "Training Loss"
  - metrics/roc.json:
      x: fpr
      y: tpr
      template: linear
  - metrics/simple.csv
stages:
  train:
    cmd: "echo train"
"#;
    let wf = crab_workflow::parse_yaml(yaml).expect("should parse");

    // Simple plots list should contain all paths.
    assert_eq!(wf.plots.len(), 3);

    // Structured configs should have 2 entries.
    assert_eq!(wf.plot_configs.len(), 2);

    let loss = &wf.plot_configs[0];
    assert_eq!(loss.path.to_str().unwrap(), "metrics/loss.csv");
    assert_eq!(loss.x.as_deref(), Some("epoch"));
    assert_eq!(loss.y, vec!["train_loss", "val_loss"]);
    assert_eq!(loss.title.as_deref(), Some("Training Loss"));
    assert!(loss.template.is_none());

    let roc = &wf.plot_configs[1];
    assert_eq!(roc.path.to_str().unwrap(), "metrics/roc.json");
    assert_eq!(roc.x.as_deref(), Some("fpr"));
    assert_eq!(roc.y, vec!["tpr"]);
    assert!(roc.title.is_none());
    assert_eq!(roc.template.as_deref(), Some("linear"));
}

#[test]
fn dvc_multi_source_plot_config_expands_to_path_configs() {
    let yaml = r#"
plots:
  - train_val_test:
      y:
        train.csv: [train_acc, val_acc]
        test.csv: test_acc
      x: epoch
      title: Accuracy
      template: linear
stages:
  train:
    cmd: "echo train"
"#;
    let wf = crab_workflow::parse_yaml(yaml).expect("should parse DVC multi-source plot");

    assert_eq!(
        wf.plots,
        vec![PathBuf::from("train.csv"), PathBuf::from("test.csv")]
    );
    assert_eq!(wf.plot_configs.len(), 2);

    let train = &wf.plot_configs[0];
    assert_eq!(train.path, PathBuf::from("train.csv"));
    assert_eq!(train.x.as_deref(), Some("epoch"));
    assert_eq!(train.y, vec!["train_acc", "val_acc"]);
    assert_eq!(train.title.as_deref(), Some("Accuracy"));
    assert_eq!(train.template.as_deref(), Some("linear"));

    let test = &wf.plot_configs[1];
    assert_eq!(test.path, PathBuf::from("test.csv"));
    assert_eq!(test.x.as_deref(), Some("epoch"));
    assert_eq!(test.y, vec!["test_acc"]);
}

#[test]
fn dvc_plot_x_mapping_applies_per_source_path() {
    let yaml = r#"
plots:
  - roc_vs_prc:
      y:
        precision_recall.json: precision
        roc.json: tpr
      x:
        precision_recall.json: recall
        roc.json: fpr
stages:
  train:
    cmd: "echo train"
"#;
    let wf = crab_workflow::parse_yaml(yaml).expect("should parse DVC x mapping plot");

    assert_eq!(wf.plot_configs.len(), 2);
    assert_eq!(
        wf.plot_configs[0].path,
        PathBuf::from("precision_recall.json")
    );
    assert_eq!(wf.plot_configs[0].x.as_deref(), Some("recall"));
    assert_eq!(wf.plot_configs[0].y, vec!["precision"]);
    assert_eq!(wf.plot_configs[1].path, PathBuf::from("roc.json"));
    assert_eq!(wf.plot_configs[1].x.as_deref(), Some("fpr"));
    assert_eq!(wf.plot_configs[1].y, vec!["tpr"]);
}

#[test]
fn stage_level_dvc_plot_configs_lower_to_plot_paths() {
    let yaml = r#"
stages:
  train:
    cmd: "echo train"
    plots:
      - train_val_test:
          y:
            train.csv: [train_acc, val_acc]
            test.csv: test_acc
          x: epoch
"#;
    let wf = crab_workflow::parse_yaml(yaml).expect("should parse stage plot configs");
    let stage = wf
        .stages
        .get(&crab::workflow::StageName::parse("train").unwrap())
        .expect("train stage");

    assert_eq!(
        stage.plots,
        vec![PathBuf::from("train.csv"), PathBuf::from("test.csv")]
    );
}

#[test]
fn dvc_stage_metric_settings_lower_to_metric_path_and_output_policy() {
    let yaml = r#"
stages:
  train:
    cmd: "echo train"
    metrics:
      - metrics/train.json:
          cache: false
          persist: true
          push: false
"#;
    let wf = crab_workflow::parse_yaml(yaml).expect("should parse DVC metric settings");
    let stage = wf
        .stages
        .get(&crab::workflow::StageName::parse("train").unwrap())
        .expect("train stage");

    assert_eq!(stage.metrics, vec![PathBuf::from("metrics/train.json")]);
    assert_eq!(stage.outs.len(), 1);
    assert_eq!(stage.outs[0].path, PathBuf::from("metrics/train.json"));
    assert!(!stage.outs[0].cache);
    assert!(stage.outs[0].persist);
    assert!(!stage.outs[0].push);
}

// ─── Task 4.6: Conditional stage tests ─────────────────────────────────

#[test]
fn condition_env_validates() {
    let tmp = TempDir::new().unwrap();
    write_yaml(
        tmp.path(),
        r#"
stages:
  deploy:
    cmd: "echo deploy"
    condition:
      env: CI
"#,
    );
    let (status, _stdout, stderr) = run_validate(tmp.path());
    assert!(
        status.success(),
        "condition env should validate; stderr={stderr}"
    );
}

#[test]
fn condition_file_exists_validates() {
    let tmp = TempDir::new().unwrap();
    write_yaml(
        tmp.path(),
        r#"
stages:
  deploy:
    cmd: "echo deploy"
    condition:
      file_exists: config/prod.yaml
"#,
    );
    let (status, _stdout, stderr) = run_validate(tmp.path());
    assert!(
        status.success(),
        "condition file_exists should validate; stderr={stderr}"
    );
}

#[test]
fn condition_expr_validates() {
    let tmp = TempDir::new().unwrap();
    write_yaml(
        tmp.path(),
        r#"
stages:
  deploy:
    cmd: "echo deploy"
    condition:
      expr: "production == 'production'"
"#,
    );
    let (status, _stdout, stderr) = run_validate(tmp.path());
    assert!(
        status.success(),
        "condition expr should validate; stderr={stderr}"
    );
}

#[test]
fn condition_multiple_fields_rejected() {
    let yaml = r#"
stages:
  deploy:
    cmd: "echo deploy"
    condition:
      env: CI
      file_exists: config/prod.yaml
"#;
    let err = crab_workflow::parse_yaml(yaml).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("condition") || msg.contains("exactly one"),
        "expected condition error, got: {msg}"
    );
}

#[test]
fn condition_env_evaluates_correctly() {
    use crab::workflow::stage::StageCondition;

    let tmp = TempDir::new().unwrap();

    // Env var not set → false.
    unsafe { std::env::remove_var("CRAB_TEST_COND_VAR") };
    let cond = StageCondition::Env("CRAB_TEST_COND_VAR".to_owned());
    assert!(!cond.evaluate(tmp.path()));

    // Env var set to empty → false.
    unsafe { std::env::set_var("CRAB_TEST_COND_VAR", "") };
    assert!(!cond.evaluate(tmp.path()));

    // Env var set to non-empty → true.
    unsafe { std::env::set_var("CRAB_TEST_COND_VAR", "1") };
    assert!(cond.evaluate(tmp.path()));

    // Clean up.
    unsafe { std::env::remove_var("CRAB_TEST_COND_VAR") };
}

#[test]
fn condition_file_exists_evaluates_correctly() {
    use crab::workflow::stage::StageCondition;
    use std::path::PathBuf;

    let tmp = TempDir::new().unwrap();

    // File does not exist → false.
    let cond = StageCondition::FileExists(PathBuf::from("marker.txt"));
    assert!(!cond.evaluate(tmp.path()));

    // Create the file → true.
    fs::write(tmp.path().join("marker.txt"), b"present").unwrap();
    assert!(cond.evaluate(tmp.path()));
}

#[test]
fn condition_expr_equality_evaluates_correctly() {
    use crab::workflow::stage::StageCondition;

    let tmp = TempDir::new().unwrap();

    // Equal → true.
    let cond = StageCondition::Expr("production == 'production'".to_owned());
    assert!(cond.evaluate(tmp.path()));

    // Not equal → false.
    let cond = StageCondition::Expr("staging == 'production'".to_owned());
    assert!(!cond.evaluate(tmp.path()));

    // != operator: different → true.
    let cond = StageCondition::Expr("staging != 'production'".to_owned());
    assert!(cond.evaluate(tmp.path()));

    // != operator: same → false.
    let cond = StageCondition::Expr("production != 'production'".to_owned());
    assert!(!cond.evaluate(tmp.path()));
}

#[test]
fn condition_parses_from_yaml() {
    use crab::workflow::stage::StageCondition;

    let yaml = r#"
stages:
  deploy:
    cmd: "echo deploy"
    condition:
      env: CI
  build:
    cmd: "echo build"
    condition:
      file_exists: Makefile
  release:
    cmd: "echo release"
    condition:
      expr: "production == 'production'"
"#;
    let wf = crab_workflow::parse_yaml(yaml).expect("should parse");

    let deploy = wf
        .stages
        .get(&crab::workflow::StageName::parse("deploy").unwrap())
        .unwrap();
    assert!(matches!(
        deploy.condition,
        Some(StageCondition::Env(ref v)) if v == "CI"
    ));

    let build = wf
        .stages
        .get(&crab::workflow::StageName::parse("build").unwrap())
        .unwrap();
    assert!(matches!(
        build.condition,
        Some(StageCondition::FileExists(ref p)) if p.to_str() == Some("Makefile")
    ));

    let release = wf
        .stages
        .get(&crab::workflow::StageName::parse("release").unwrap())
        .unwrap();
    assert!(matches!(
        release.condition,
        Some(StageCondition::Expr(ref e)) if e == "production == 'production'"
    ));
}

#[test]
fn stage_without_condition_has_none() {
    let yaml = r#"
stages:
  train:
    cmd: "echo train"
"#;
    let wf = crab_workflow::parse_yaml(yaml).expect("should parse");
    let train = wf
        .stages
        .get(&crab::workflow::StageName::parse("train").unwrap())
        .unwrap();
    assert!(train.condition.is_none());
}
