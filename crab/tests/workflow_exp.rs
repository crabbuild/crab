//! Integration tests for `crab exp` — run / show / diff / ls /
//! promote / apply / gc.
//!
//! Drives the real `crab` binary via
//! `Command::new(env!("CARGO_BIN_EXE_crab"))` against a scratch
//! git repo that ships a one-stage `crab.yaml` + `params.yaml`.
//! The tests assert on the `.crab/workflow/exp/<uuid>.meta.json`
//! blobs and (for `promote`) on `git rev-parse <branch>`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use serde_json::Value;
use tempfile::TempDir;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_crab")
}

/// Run `git` with a clean environment (no GIT_DIR / GIT_WORK_TREE
/// inherited from the test harness) scoped to `repo`.
fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(repo)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "git {args:?} failed");
}

/// Capture stdout of a `git` invocation as a trimmed string.
fn git_output(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?} failed: {out:?}");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// Produce a scratch repo with:
/// - workflow enabled via `.crab/config.toml`
/// - a `params.yaml` with `model.lr: 0.001`
/// - a `crab.yaml` with a single `copy` stage that copies
///   `params.yaml` to `out.txt`, with `params: [model.lr]` so the
///   override drives a stage-hash change
/// - an initial commit covering the above
fn init_scratch_repo() -> TempDir {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();

    git(repo, &["init", "--initial-branch=main"]);
    git(repo, &["config", "user.email", "t@test.com"]);
    git(repo, &["config", "user.name", "Test"]);
    git(repo, &["config", "commit.gpgsign", "false"]);

    // Workflow enablement lives in the local config file so every
    // `crab` invocation in the tests honors it without
    // environment-variable noise.
    fs::create_dir_all(repo.join(".crab")).unwrap();
    fs::write(
        repo.join(".crab/config.toml"),
        "[workflow]\nenabled = true\n",
    )
    .unwrap();

    fs::write(repo.join("params.yaml"), "model:\n  lr: 0.001\n").unwrap();
    fs::write(
        repo.join("crab.yaml"),
        concat!(
            "params:\n",
            "  - params.yaml\n",
            "stages:\n",
            "  copy:\n",
            "    cmd: \"cp params.yaml out.txt\"\n",
            "    deps:\n",
            "      - params.yaml\n",
            "    outs:\n",
            "      - out.txt\n",
            "    params:\n",
            "      - model.lr\n",
        ),
    )
    .unwrap();

    // Both the config and the yaml need to be committed so HEAD
    // captures them — the exp worktree checkouts from HEAD, not
    // from the working tree.
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-m", "initial"]);

    tmp
}

/// Invoke `crab exp <subcmd>` with the extra argv and parse
/// stdout as a JSON envelope. Returns the envelope's `.data` node
/// (not the outer envelope — tests care about the payload).
fn run_exp_json(repo: &Path, subcmd: &str, extra: &[&str]) -> Value {
    let mut args = vec!["exp", subcmd, "--json"];
    args.extend(extra.iter().copied());
    run_crab_json(repo, &args)
}

/// Invoke `crab <args>` and parse stdout as a JSON envelope. Returns
/// the envelope's `.data` node.
fn run_crab_json(repo: &Path, args: &[&str]) -> Value {
    let output = Command::new(bin())
        .current_dir(repo)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .output()
        .expect("crab should spawn");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "crab {args:?} failed: status={:?} stdout={stdout:?} stderr={stderr:?}",
        output.status
    );
    let envelope: Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("parse crab {args:?} envelope failed: {e}; stdout={stdout:?}"));
    envelope["data"].clone()
}

#[test]
fn run_push_false_output_still_reuses_local_cache() {
    let tmp = init_scratch_repo();
    let repo = tmp.path();
    fs::write(
        repo.join("crab.yaml"),
        concat!(
            "stages:\n",
            "  localonly:\n",
            "    cmd: \"if [ -f marker ]; then exit 99; fi; touch marker; printf x > local-only.txt\"\n",
            "    outs:\n",
            "      - path: local-only.txt\n",
            "        push: false\n",
        ),
    )
    .unwrap();

    run_crab_json(repo, &["run", "--json"]);
    assert_eq!(
        fs::read_to_string(repo.join("local-only.txt")).unwrap(),
        "x"
    );
    assert!(repo.join("marker").exists());

    fs::remove_file(repo.join("local-only.txt")).unwrap();
    run_crab_json(repo, &["run", "--json"]);
    assert_eq!(
        fs::read_to_string(repo.join("local-only.txt")).unwrap(),
        "x"
    );
}

#[test]
fn run_directory_output_replays_from_local_cache_after_delete() {
    let tmp = init_scratch_repo();
    let repo = tmp.path();
    fs::write(
        repo.join("crab.yaml"),
        concat!(
            "stages:\n",
            "  tree:\n",
            "    cmd: \"if [ -f marker-dir ]; then exit 99; fi; touch marker-dir; mkdir -p artifacts/nested artifacts/empty; printf a > artifacts/nested/a.txt; printf b > artifacts/b.txt\"\n",
            "    outs:\n",
            "      - path: artifacts\n",
            "        kind: directory\n",
        ),
    )
    .unwrap();

    run_crab_json(repo, &["run", "--json"]);
    assert_eq!(
        fs::read_to_string(repo.join("artifacts/nested/a.txt")).unwrap(),
        "a"
    );
    assert_eq!(
        fs::read_to_string(repo.join("artifacts/b.txt")).unwrap(),
        "b"
    );
    assert!(repo.join("artifacts/empty").is_dir());

    fs::remove_dir_all(repo.join("artifacts")).unwrap();
    run_crab_json(repo, &["run", "--json"]);
    assert_eq!(
        fs::read_to_string(repo.join("artifacts/nested/a.txt")).unwrap(),
        "a"
    );
    assert_eq!(
        fs::read_to_string(repo.join("artifacts/b.txt")).unwrap(),
        "b"
    );
    assert!(repo.join("artifacts/empty").is_dir());
}

fn yaml_str<'a>(value: &'a serde_yaml::Value, path: &[&str]) -> Option<&'a str> {
    let mut cursor = value;
    for segment in path {
        let serde_yaml::Value::Mapping(map) = cursor else {
            return None;
        };
        cursor = map.get(serde_yaml::Value::String((*segment).to_owned()))?;
    }
    cursor.as_str()
}

/// Invoke `crab <args>` and return `(status, stdout, stderr)`
/// without assuming success.
fn run_crab_raw(repo: &Path, args: &[&str]) -> (std::process::ExitStatus, String, String) {
    let output = Command::new(bin())
        .current_dir(repo)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .output()
        .expect("crab should spawn");
    (
        output.status,
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

/// Invoke `crab exp <subcmd>` and return `(status, stdout, stderr)`
/// without assuming success. Used by tests that expect failure.
fn run_exp_raw(
    repo: &Path,
    subcmd: &str,
    extra: &[&str],
) -> (std::process::ExitStatus, String, String) {
    let mut args = vec!["exp", subcmd];
    args.extend(extra.iter().copied());
    let output = Command::new(bin())
        .current_dir(repo)
        .args(&args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .output()
        .expect("crab exp should spawn");
    (
        output.status,
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
    )
}

#[test]
fn exp_run_creates_metadata_file_and_succeeds() {
    let tmp = init_scratch_repo();
    let repo = tmp.path();

    let payload = run_exp_json(repo, "run", &["--name", "baseline-run"]);
    let exp_id = payload["exp_id"].as_str().unwrap().to_owned();
    assert_eq!(payload["status"], "success");
    assert_eq!(payload["name"], "baseline-run");
    assert!(!payload["base_commit"].as_str().unwrap().is_empty());
    // The stages map must include the single `copy` stage with a
    // non-empty hash — we don't pin the hash value because it
    // depends on the git commit OIDs + params.yaml bytes which
    // drift with every test run.
    assert!(
        payload["stages"]["copy"]
            .as_str()
            .is_some_and(|h| !h.is_empty()),
        "stages.copy hash missing: {payload}",
    );

    // The metadata blob is at the expected path.
    let meta_path = repo
        .join(".crab/workflow/exp")
        .join(format!("{exp_id}.meta.json"));
    assert!(
        meta_path.exists(),
        "metadata file not at {}",
        meta_path.display()
    );
    let meta: Value = serde_json::from_slice(&fs::read(&meta_path).unwrap()).unwrap();
    assert_eq!(meta["exp_id"], exp_id);
    assert_eq!(meta["name"], "baseline-run");
    assert_eq!(
        meta["schema_version"],
        crab::workflow::EXPERIMENT_METADATA_SCHEMA_VERSION
    );
    assert!(
        repo.join(".crab/workflow/exp")
            .join(format!("{exp_id}.workspace"))
            .is_dir(),
        "successful experiments should capture an apply workspace snapshot"
    );
}

#[test]
fn exp_run_with_override_hits_different_stage_hash() {
    let tmp = init_scratch_repo();
    let repo = tmp.path();

    let payload_a = run_exp_json(repo, "run", &["--set", "model.lr=0.001"]);
    let payload_b = run_exp_json(repo, "run", &["--set", "model.lr=0.002"]);

    let hash_a = payload_a["stages"]["copy"].as_str().unwrap();
    let hash_b = payload_b["stages"]["copy"].as_str().unwrap();
    assert_ne!(
        hash_a, hash_b,
        "different --set values must yield different stage hashes"
    );
}

#[test]
fn exp_run_queue_and_run_all_use_dvc_spellings() {
    let tmp = init_scratch_repo();
    let repo = tmp.path();

    let queued = run_exp_json(
        repo,
        "run",
        &["--queue", "-S", "model.lr=0.004,0.005", "--name", "sweep"],
    );
    assert_eq!(queued["queued_count"], 2);

    let status = run_exp_json(repo, "status", &[]);
    assert_eq!(status["pending"], 2);
    assert_eq!(status["total"], 2);

    let started = run_exp_json(repo, "run", &["--run-all", "-j", "2"]);
    assert_eq!(started["processed"], 2);
    assert_eq!(started["succeeded"], 2);
    assert_eq!(started["failed"], 0);

    let ls = run_exp_json(repo, "ls", &[]);
    let experiments = ls["experiments"].as_array().unwrap();
    assert_eq!(experiments.len(), 2);

    let mut names = experiments
        .iter()
        .map(|experiment| experiment["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    names.sort_unstable();
    assert_eq!(names, vec!["sweep-1", "sweep-2"]);

    let mut params = experiments
        .iter()
        .map(|experiment| experiment["params"]["model.lr"].as_str().unwrap())
        .collect::<Vec<_>>();
    params.sort_unstable();
    assert_eq!(params, vec!["0.004", "0.005"]);
}

#[test]
fn exp_run_all_runs_after_previous_stop_signal() {
    let tmp = init_scratch_repo();
    let repo = tmp.path();

    let stopped = run_exp_json(repo, "stop", &[]);
    assert_eq!(stopped["signaled"], true);
    assert!(repo.join(".crab/exp-queue/.stop").exists());

    let queued = run_exp_json(
        repo,
        "run",
        &["--queue", "-S", "model.lr=0.014,0.015", "--name", "restart"],
    );
    assert_eq!(queued["queued_count"], 2);

    let started = run_exp_json(repo, "run", &["--run-all", "-j", "2"]);
    assert_eq!(started["processed"], 2);
    assert_eq!(started["succeeded"], 2);
    assert_eq!(started["failed"], 0);
    assert!(!repo.join(".crab/exp-queue/.stop").exists());
}

#[test]
fn queue_remove_success_uses_dvc_spelling_without_deleting_experiment() {
    let tmp = init_scratch_repo();
    let repo = tmp.path();

    let queued = run_exp_json(
        repo,
        "run",
        &[
            "--queue",
            "-S",
            "model.lr=0.006",
            "--name",
            "queued-success",
        ],
    );
    let exp_id = queued["experiment_ids"][0].as_str().unwrap();

    let started = run_crab_json(repo, &["queue", "start", "--json"]);
    assert_eq!(started["processed"], 1);
    assert_eq!(started["succeeded"], 1);

    let removed = run_crab_json(repo, &["queue", "remove", "--json", "--success"]);
    assert_eq!(removed["removed"], serde_json::json!([exp_id]));
    assert_eq!(removed["skipped_running"].as_array().unwrap().len(), 0);

    let status = run_crab_json(repo, &["queue", "status", "--json"]);
    assert_eq!(status["total"], 0);

    let show = run_exp_json(repo, "show", &[exp_id]);
    assert_eq!(show["metadata"]["exp_id"], exp_id);
    assert_eq!(show["metadata"]["name"], "queued-success");
}

#[test]
fn queue_logs_returns_completed_stage_output() {
    let tmp = init_scratch_repo();
    let repo = tmp.path();
    fs::write(
        repo.join("crab.yaml"),
        concat!(
            "params:\n",
            "  - params.yaml\n",
            "stages:\n",
            "  copy:\n",
            "    cmd: \"printf 'queue-log-line\\n' && cp params.yaml out.txt\"\n",
            "    deps:\n",
            "      - params.yaml\n",
            "    outs:\n",
            "      - out.txt\n",
            "    params:\n",
            "      - model.lr\n",
        ),
    )
    .unwrap();
    git(repo, &["add", "crab.yaml"]);
    git(repo, &["commit", "-m", "emit queue logs"]);

    let queued = run_exp_json(repo, "run", &["--queue", "-S", "model.lr=0.008"]);
    let exp_id = queued["experiment_ids"][0].as_str().unwrap();

    let started = run_crab_json(repo, &["queue", "start", "--json"]);
    assert_eq!(started["succeeded"], 1);

    let logs = run_crab_json(repo, &["queue", "logs", "--json", exp_id]);
    assert_eq!(logs["id"], exp_id);
    assert!(
        logs["contents"]
            .as_str()
            .is_some_and(|contents| contents.contains("queue-log-line")),
        "queue logs should include stage stdout: {logs}",
    );

    run_crab_json(repo, &["queue", "remove", "--json", "--success"]);
    assert!(
        !repo
            .join(".crab/exp-queue/logs")
            .join(format!("{exp_id}.log"))
            .exists(),
        "queue remove should delete associated task logs"
    );

    let (status, stdout, stderr) = run_crab_raw(repo, &["queue", "logs", "--json", exp_id]);
    assert!(
        !status.success(),
        "queue logs for removed task must fail: stdout={stdout:?} stderr={stderr:?}",
    );
}

#[test]
fn queue_kill_interrupts_running_task_and_marks_failed() {
    let tmp = init_scratch_repo();
    let repo = tmp.path();
    fs::write(
        repo.join("crab.yaml"),
        concat!(
            "params:\n",
            "  - params.yaml\n",
            "stages:\n",
            "  copy:\n",
            "    cmd: \"printf 'queue-kill-started\\n'; sleep 30; printf 'queue-kill-done\\n'; cp params.yaml out.txt\"\n",
            "    deps:\n",
            "      - params.yaml\n",
            "    outs:\n",
            "      - out.txt\n",
            "    params:\n",
            "      - model.lr\n",
        ),
    )
    .unwrap();
    git(repo, &["add", "crab.yaml"]);
    git(repo, &["commit", "-m", "make queued task long-running"]);

    let queued = run_exp_json(repo, "run", &["--queue", "-S", "model.lr=0.009"]);
    let exp_id = queued["experiment_ids"][0].as_str().unwrap();

    let mut starter = Command::new(bin())
        .current_dir(repo)
        .args(["queue", "start", "--json"])
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("queue start should spawn");

    let mut saw_running = false;
    for _ in 0..300 {
        let status = run_crab_json(repo, &["queue", "status", "--json"]);
        if status["running"] == 1 {
            saw_running = true;
            break;
        }
        if starter.try_wait().unwrap().is_some() {
            let output = starter.wait_with_output().unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            panic!(
                "queue start exited before task was running: stdout={stdout:?} stderr={stderr:?}"
            );
        }
        thread::sleep(Duration::from_millis(100));
    }
    if !saw_running {
        let _ = starter.kill();
        let output = starter.wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        panic!("queue task did not become running: stdout={stdout:?} stderr={stderr:?}");
    }

    let killed = run_crab_json(repo, &["queue", "kill", "--json", "--force", exp_id]);
    assert_eq!(killed["killed"], serde_json::json!([exp_id]));
    assert_eq!(killed["force"], true);

    let mut exited = false;
    for _ in 0..300 {
        if starter.try_wait().unwrap().is_some() {
            exited = true;
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    if !exited {
        let _ = starter.kill();
    }
    let output = starter.wait_with_output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "queue start should report failed task without failing itself: status={:?} stdout={stdout:?} stderr={stderr:?}",
        output.status,
    );
    let envelope: Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("parse queue start envelope failed: {e}; stdout={stdout:?}"));
    let started = &envelope["data"];
    assert_eq!(started["processed"], 1);
    assert_eq!(started["succeeded"], 0);
    assert_eq!(started["failed"], 1);
    assert_eq!(started["failed_ids"], serde_json::json!([exp_id]));

    let status = run_crab_json(repo, &["queue", "status", "--json"]);
    assert_eq!(status["running"], 0);
    assert_eq!(status["failed"], 1);

    let logs = run_crab_json(repo, &["queue", "logs", "--json", exp_id]);
    let contents = logs["contents"].as_str().unwrap();
    assert!(
        contents.contains("queue-kill-started"),
        "killed task logs should preserve output before interruption: {logs}",
    );
    assert!(
        !contents.contains("queue-kill-done"),
        "force-killed task must not run commands after the sleep: {logs}",
    );
}

#[test]
fn plots_templates_lists_and_dumps_dvc_style_templates() {
    let tmp = init_scratch_repo();
    let repo = tmp.path();
    fs::create_dir_all(repo.join(".dvc/plots")).unwrap();
    fs::write(
        repo.join(".dvc/plots/custom.json"),
        r#"{"$schema":"https://vega.github.io/schema/vega-lite/v5.json","data":{"values":"<DVC_METRIC_DATA>"}}"#,
    )
    .unwrap();

    let listed = run_crab_json(repo, &["plots", "templates", "--json"]);
    let templates = listed["templates"].as_array().unwrap();
    assert!(
        templates
            .iter()
            .any(|template| { template["name"] == "linear" && template["source"] == "builtin" })
    );
    assert!(
        templates
            .iter()
            .any(|template| { template["name"] == "custom" && template["source"] == "local" })
    );

    let (status, stdout, stderr) = run_crab_raw(repo, &["plots", "templates", "linear"]);
    assert!(
        status.success(),
        "plots templates linear failed: stdout={stdout:?} stderr={stderr:?}",
    );
    let spec: Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("parse template spec failed: {e}; stdout={stdout:?}"));
    assert_eq!(spec["title"], "<DVC_METRIC_TITLE>");
    assert_eq!(spec["data"]["values"], "<DVC_METRIC_DATA>");
}

#[test]
fn metrics_plot_embeds_image_target_in_html() {
    let tmp = init_scratch_repo();
    let repo = tmp.path();
    fs::create_dir_all(repo.join("plots")).unwrap();
    fs::write(
        repo.join("plots/confusion.svg"),
        r#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16"><text x="1" y="12">ok</text></svg>"#,
    )
    .unwrap();

    let (status, stdout, stderr) = run_crab_raw(
        repo,
        &[
            "metrics",
            "plot",
            "plots/confusion.svg",
            "--format",
            "html",
            "--output",
            "image-plots.html",
        ],
    );
    assert!(
        status.success(),
        "metrics plot image failed: stdout={stdout:?} stderr={stderr:?}",
    );

    let html = fs::read_to_string(repo.join("image-plots.html")).unwrap();
    assert!(html.contains(r#""kind": "image""#));
    assert!(html.contains("plots/confusion.svg"));
    assert!(html.contains("data:image/svg+xml;base64,"));
}

#[test]
fn exp_run_composes_hydra_config_groups_before_param_overrides() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();

    git(repo, &["init", "--initial-branch=main"]);
    git(repo, &["config", "user.email", "t@test.com"]);
    git(repo, &["config", "user.name", "Test"]);
    git(repo, &["config", "commit.gpgsign", "false"]);

    fs::create_dir_all(repo.join(".crab")).unwrap();
    fs::write(
        repo.join(".crab/config.toml"),
        "[workflow]\nenabled = true\n\n[hydra]\nenabled = true\n",
    )
    .unwrap();
    fs::create_dir_all(repo.join("conf/train/model")).unwrap();
    fs::create_dir_all(repo.join("conf/train/optimizer")).unwrap();
    fs::write(
        repo.join("conf/config.yaml"),
        "defaults:\n  - train/model: resnet\n  - train/optimizer: sgd\n",
    )
    .unwrap();
    fs::write(
        repo.join("conf/train/model/resnet.yaml"),
        "name: ResNet\nsize: 50\n",
    )
    .unwrap();
    fs::write(
        repo.join("conf/train/model/efficientnet.yaml"),
        "name: EfficientNet\nsize: b0\n",
    )
    .unwrap();
    fs::write(
        repo.join("conf/train/optimizer/sgd.yaml"),
        "name: SGD\nlr: 0.001\n",
    )
    .unwrap();
    fs::write(
        repo.join("crab.yaml"),
        concat!(
            "params:\n",
            "  - params.yaml\n",
            "stages:\n",
            "  train:\n",
            "    cmd: \"cp params.yaml out.txt\"\n",
            "    deps:\n",
            "      - params.yaml\n",
            "    outs:\n",
            "      - out.txt\n",
            "    params:\n",
            "      - train.model.name\n",
            "      - train.optimizer.lr\n",
        ),
    )
    .unwrap();
    git(repo, &["add", "-f", ".crab/config.toml"]);
    git(repo, &["add", "conf", "crab.yaml"]);
    git(repo, &["commit", "-m", "hydra workflow"]);

    let run = run_exp_json(
        repo,
        "run",
        &[
            "--set-param",
            "train/model=efficientnet",
            "--set-param",
            "train.optimizer.lr=0.02",
            "--name",
            "hydra-choice",
        ],
    );
    let exp_id = run["exp_id"].as_str().unwrap();
    assert_eq!(run["status"], "success");

    let apply = run_exp_json(repo, "apply", &[exp_id]);
    assert!(
        apply["applied"]
            .as_array()
            .unwrap()
            .iter()
            .any(|path| path.as_str() == Some("params.yaml")),
        "hydra experiment should apply composed params.yaml: {apply}",
    );

    let params = fs::read_to_string(repo.join("params.yaml")).unwrap();
    assert!(params.contains("name: EfficientNet"));
    assert!(params.contains("size: b0"));
    assert!(params.contains("lr: 0.02"));
    assert_eq!(fs::read_to_string(repo.join("out.txt")).unwrap(), params);
}

#[test]
fn exp_run_composes_hydra_nested_defaults_and_package_overrides() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();

    git(repo, &["init", "--initial-branch=main"]);
    git(repo, &["config", "user.email", "t@test.com"]);
    git(repo, &["config", "user.name", "Test"]);
    git(repo, &["config", "commit.gpgsign", "false"]);

    fs::create_dir_all(repo.join(".crab")).unwrap();
    fs::write(
        repo.join(".crab/config.toml"),
        "[workflow]\nenabled = true\n\n[hydra]\nenabled = true\n",
    )
    .unwrap();
    fs::create_dir_all(repo.join("conf/server/db")).unwrap();
    fs::create_dir_all(repo.join("conf/train/model")).unwrap();
    fs::create_dir_all(repo.join("conf/train/optimizer")).unwrap();
    fs::create_dir_all(repo.join("conf/augment")).unwrap();
    fs::write(
        repo.join("conf/config.yaml"),
        concat!(
            "defaults:\n",
            "  - server/apache@src\n",
            "  - server/apache@dst\n",
            "  - server/db@srcdb: mysql\n",
            "  - server/db@dstdb: mysql\n",
            "  - train/model: resnet\n",
            "  - override train/model: resnet101\n",
            "  - train/optimizer: null\n",
            "  - augment: [flip, crop]\n",
        ),
    )
    .unwrap();
    fs::write(
        repo.join("conf/server/apache.yaml"),
        "defaults:\n  - db: mysql\nname: apache\n",
    )
    .unwrap();
    fs::write(repo.join("conf/server/db/mysql.yaml"), "engine: mysql\n").unwrap();
    fs::write(repo.join("conf/server/db/sqlite.yaml"), "engine: sqlite\n").unwrap();
    fs::write(repo.join("conf/train/model/resnet.yaml"), "name: ResNet\n").unwrap();
    fs::write(
        repo.join("conf/train/model/resnet101.yaml"),
        "name: ResNet101\n",
    )
    .unwrap();
    fs::write(
        repo.join("conf/train/model/efficientnet.yaml"),
        "name: EfficientNet\n",
    )
    .unwrap();
    fs::write(
        repo.join("conf/train/optimizer/adam.yaml"),
        "name: Adam\nlr: 0.001\n",
    )
    .unwrap();
    fs::write(repo.join("conf/augment/flip.yaml"), "flip: true\n").unwrap();
    fs::write(repo.join("conf/augment/crop.yaml"), "crop: 224\n").unwrap();
    fs::write(
        repo.join("crab.yaml"),
        concat!(
            "params:\n",
            "  - params.yaml\n",
            "stages:\n",
            "  train:\n",
            "    cmd: \"cp params.yaml out.txt\"\n",
            "    deps:\n",
            "      - params.yaml\n",
            "    outs:\n",
            "      - out.txt\n",
            "    params:\n",
            "      - src.db.engine\n",
            "      - dst.db.engine\n",
            "      - srcdb.engine\n",
            "      - dstdb.engine\n",
            "      - train.model.name\n",
            "      - train.optimizer.name\n",
            "      - augment.flip\n",
            "      - augment.crop\n",
        ),
    )
    .unwrap();
    git(repo, &["add", "-f", ".crab/config.toml"]);
    git(repo, &["add", "conf", "crab.yaml"]);
    git(repo, &["commit", "-m", "hydra nested workflow"]);

    let run = run_exp_json(
        repo,
        "run",
        &[
            "-S",
            "server/db@srcdb=sqlite",
            "-S",
            "train/model=efficientnet",
            "-S",
            "train/optimizer=adam",
            "--name",
            "hydra-nested",
        ],
    );
    let exp_id = run["exp_id"].as_str().unwrap();
    assert_eq!(run["status"], "success");

    run_exp_json(repo, "apply", &[exp_id]);

    let params = fs::read_to_string(repo.join("params.yaml")).unwrap();
    let params: serde_yaml::Value = serde_yaml::from_str(&params).unwrap();
    assert_eq!(yaml_str(&params, &["src", "db", "engine"]), Some("mysql"));
    assert_eq!(yaml_str(&params, &["dst", "db", "engine"]), Some("mysql"));
    assert_eq!(yaml_str(&params, &["srcdb", "engine"]), Some("sqlite"));
    assert_eq!(yaml_str(&params, &["dstdb", "engine"]), Some("mysql"));
    assert_eq!(
        yaml_str(&params, &["train", "model", "name"]),
        Some("EfficientNet")
    );
    assert_eq!(
        yaml_str(&params, &["train", "optimizer", "name"]),
        Some("Adam")
    );
    assert_eq!(params["augment"]["flip"].as_bool(), Some(true));
    assert_eq!(params["augment"]["crop"].as_i64(), Some(224));
}

#[test]
fn exp_show_returns_metadata() {
    let tmp = init_scratch_repo();
    let repo = tmp.path();

    let run = run_exp_json(repo, "run", &[]);
    let exp_id = run["exp_id"].as_str().unwrap().to_owned();

    let show = run_exp_json(repo, "show", &[&exp_id]);
    assert_eq!(show["metadata"]["exp_id"], exp_id);
    assert!(show["metadata"]["base_commit"].as_str().is_some());
}

#[test]
fn exp_show_accepts_unambiguous_id_prefix() {
    let tmp = init_scratch_repo();
    let repo = tmp.path();

    let run = run_exp_json(repo, "run", &[]);
    let exp_id = run["exp_id"].as_str().unwrap().to_owned();
    let prefix = &exp_id[..12];

    let show = run_exp_json(repo, "show", &[prefix]);
    assert_eq!(show["metadata"]["exp_id"], exp_id);
}

#[test]
fn exp_show_without_id_lists_recent_experiments() {
    let tmp = init_scratch_repo();
    let repo = tmp.path();

    let first = run_exp_json(repo, "run", &["--set-param", "model.lr=0.001"]);
    std::thread::sleep(std::time::Duration::from_millis(5));
    let second = run_exp_json(repo, "run", &["--set-param", "model.lr=0.002"]);

    let show = run_exp_json(repo, "show", &["--all", "--num", "1"]);
    let experiments = show["experiments"].as_array().unwrap();
    assert_eq!(experiments.len(), 1);
    assert_eq!(experiments[0]["id"], second["exp_id"]);
    assert_ne!(experiments[0]["id"], first["exp_id"]);
}

#[test]
fn exp_show_renders_markdown_sorted_by_param() {
    let tmp = init_scratch_repo();
    let repo = tmp.path();

    run_exp_json(repo, "run", &["--set-param", "model.lr=0.002"]);
    std::thread::sleep(std::time::Duration::from_millis(5));
    run_exp_json(repo, "run", &["--set-param", "model.lr=0.001"]);

    let (status, stdout, stderr) = run_exp_raw(
        repo,
        "show",
        &["--md", "--sort-by", "model.lr", "--sort-order", "asc"],
    );

    assert!(
        status.success(),
        "exp show --md failed: status={status:?} stdout={stdout:?} stderr={stderr:?}",
    );
    assert!(
        stdout.contains(
            "| EXP_ID | NAME | MESSAGE | STARTED_AT | STAGES | STATUS | BASE_COMMIT | PARAMS | METRICS |"
        )
    );
    let first = stdout
        .find("model.lr=0.001")
        .expect("sorted markdown should include smaller param");
    let second = stdout
        .find("model.lr=0.002")
        .expect("sorted markdown should include larger param");
    assert!(first < second, "ascending model.lr order wrong:\n{stdout}");
}

#[test]
fn exp_show_filters_markdown_param_columns() {
    let tmp = init_scratch_repo();
    let repo = tmp.path();

    run_exp_json(repo, "run", &["--set-param", "model.lr=0.002"]);
    std::thread::sleep(std::time::Duration::from_millis(5));
    run_exp_json(repo, "run", &["--set-param", "model.lr=0.001"]);

    let (drop_status, drop_stdout, drop_stderr) =
        run_exp_raw(repo, "show", &["--md", "--only-changed", "--drop", "model"]);
    assert!(
        drop_status.success(),
        "exp show --drop failed: status={drop_status:?} stdout={drop_stdout:?} stderr={drop_stderr:?}",
    );
    assert!(
        !drop_stdout.contains("model.lr="),
        "--drop should remove the matching changed param:\n{drop_stdout}",
    );

    let (keep_status, keep_stdout, keep_stderr) = run_exp_raw(
        repo,
        "show",
        &[
            "--md",
            "--only-changed",
            "--drop",
            "model",
            "--keep",
            "model\\.lr",
        ],
    );
    assert!(
        keep_status.success(),
        "exp show --keep failed: status={keep_status:?} stdout={keep_stdout:?} stderr={keep_stderr:?}",
    );
    assert!(
        keep_stdout.contains("model.lr=0.001") && keep_stdout.contains("model.lr=0.002"),
        "--keep should preserve the matching changed param:\n{keep_stdout}",
    );
}

#[test]
fn exp_show_missing_id_returns_not_found() {
    let tmp = init_scratch_repo();
    let repo = tmp.path();

    // Mint a valid UUIDv7-shaped string that won't exist on disk.
    let made_up_id = "01931b9e-4b3c-7b2a-b9f0-0123456789ab";
    let (status, stdout, stderr) = run_exp_raw(repo, "show", &[made_up_id]);
    assert!(!status.success(), "exp show missing id must fail");
    let combined = format!("{stdout}\n{stderr}");
    assert!(
        combined.contains("E0222") || combined.contains("experiment not found"),
        "missing-id error should mention ExperimentNotFound/E0222; got:\n{combined}",
    );
}

#[test]
fn exp_ls_returns_experiments_in_reverse_chronological_order() {
    let tmp = init_scratch_repo();
    let repo = tmp.path();

    let mut ids = Vec::new();
    for i in 0..3 {
        let v = format!("0.00{}", i + 1);
        let payload = run_exp_json(repo, "run", &["--set", &format!("model.lr={v}")]);
        ids.push(payload["exp_id"].as_str().unwrap().to_owned());
        // Nudge the clock so the next UUIDv7 sorts strictly after.
        std::thread::sleep(std::time::Duration::from_millis(5));
    }

    let ls = run_exp_json(repo, "ls", &[]);
    let experiments = ls["experiments"].as_array().unwrap();
    assert_eq!(experiments.len(), 3);
    // Newest (last created) first.
    assert_eq!(experiments[0]["id"], ids[2]);
    assert_eq!(experiments[1]["id"], ids[1]);
    assert_eq!(experiments[2]["id"], ids[0]);

    let list = run_exp_json(repo, "list", &["--limit", "1"]);
    assert_eq!(list["experiments"].as_array().unwrap().len(), 1);
}

#[test]
fn exp_ls_respects_limit() {
    let tmp = init_scratch_repo();
    let repo = tmp.path();

    for i in 0..3 {
        let v = format!("0.00{}", i + 1);
        run_exp_json(repo, "run", &["--set", &format!("model.lr={v}")]);
        std::thread::sleep(std::time::Duration::from_millis(5));
    }

    let ls = run_exp_json(repo, "ls", &["--limit", "2"]);
    assert_eq!(ls["experiments"].as_array().unwrap().len(), 2);
}

#[test]
fn exp_promote_creates_branch_with_experiment_snapshot() {
    let tmp = init_scratch_repo();
    let repo = tmp.path();

    let run = run_exp_json(repo, "run", &[]);
    let exp_id = run["exp_id"].as_str().unwrap().to_owned();
    let base_commit = run["base_commit"].as_str().unwrap().to_owned();

    let promote = run_exp_json(repo, "promote", &[&exp_id, "--branch", "feat-x"]);
    assert_eq!(promote["branch"], "feat-x");
    assert_ne!(promote["commit"], base_commit);

    // Verify the branch contains the experiment snapshot and has
    // the experiment baseline as its parent.
    let rev = git_output(repo, &["rev-parse", "feat-x"]);
    assert_eq!(promote["commit"], rev);
    let parent = git_output(repo, &["rev-parse", "feat-x^"]);
    assert_eq!(parent, base_commit);
    let out = git_output(repo, &["show", "feat-x:out.txt"]);
    assert_eq!(out, "model:\n  lr: 0.001");

    let default_branch = format!("exp-{}", &exp_id[..12]);
    let branch = run_exp_json(repo, "branch", &[&exp_id]);
    assert_eq!(branch["branch"], default_branch);
    let rev = git_output(repo, &["rev-parse", &default_branch]);
    assert_eq!(branch["commit"], rev);
    let parent = git_output(repo, &["rev-parse", &format!("{default_branch}^")]);
    assert_eq!(parent, base_commit);
}

#[test]
fn exp_promote_accepts_unambiguous_id_prefix() {
    let tmp = init_scratch_repo();
    let repo = tmp.path();

    let run = run_exp_json(repo, "run", &[]);
    let exp_id = run["exp_id"].as_str().unwrap().to_owned();
    let prefix = &exp_id[..12];
    let base_commit = run["base_commit"].as_str().unwrap().to_owned();

    let promote = run_exp_json(repo, "promote", &[prefix, "short-id"]);
    assert_eq!(promote["exp_id"], exp_id);
    assert_eq!(promote["branch"], "short-id");
    let parent = git_output(repo, &["rev-parse", "short-id^"]);
    assert_eq!(parent, base_commit);
}

#[test]
fn exp_apply_restores_experiment_workspace_snapshot() {
    let tmp = init_scratch_repo();
    let repo = tmp.path();

    let run = run_exp_json(repo, "run", &["--set-param", "model.lr=0.007"]);
    let exp_id = run["exp_id"].as_str().unwrap();

    fs::write(repo.join("params.yaml"), "model:\n  lr: 9.999\n").unwrap();
    fs::write(repo.join("out.txt"), "stale output\n").unwrap();

    let apply = run_exp_json(repo, "apply", &[&exp_id[..12]]);
    assert_eq!(apply["exp_id"], exp_id);
    let applied = apply["applied"].as_array().unwrap();
    assert!(
        applied
            .iter()
            .any(|path| path.as_str() == Some("params.yaml")),
        "apply payload should include params.yaml: {apply}",
    );
    assert!(
        applied.iter().any(|path| path.as_str() == Some("out.txt")),
        "apply payload should include out.txt: {apply}",
    );

    assert_eq!(
        fs::read_to_string(repo.join("params.yaml")).unwrap(),
        "model:\n  lr: 0.007\n"
    );
    assert_eq!(
        fs::read_to_string(repo.join("out.txt")).unwrap(),
        "model:\n  lr: 0.007\n"
    );
}

#[test]
fn exp_apply_removes_files_deleted_by_experiment() {
    let tmp = init_scratch_repo();
    let repo = tmp.path();

    fs::write(repo.join("tracked.txt"), "keep until experiment\n").unwrap();
    fs::write(
        repo.join("crab.yaml"),
        concat!(
            "params:\n",
            "  - params.yaml\n",
            "stages:\n",
            "  copy:\n",
            "    cmd: \"rm tracked.txt && cp params.yaml out.txt\"\n",
            "    deps:\n",
            "      - params.yaml\n",
            "      - tracked.txt\n",
            "    outs:\n",
            "      - out.txt\n",
            "    params:\n",
            "      - model.lr\n",
        ),
    )
    .unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-m", "delete tracked in experiment"]);

    let run = run_exp_json(repo, "run", &[]);
    let exp_id = run["exp_id"].as_str().unwrap();
    fs::write(repo.join("tracked.txt"), "local conflicting edit\n").unwrap();

    let apply = run_exp_json(repo, "apply", &[exp_id]);
    assert!(
        apply["deleted"]
            .as_array()
            .unwrap()
            .iter()
            .any(|path| path.as_str() == Some("tracked.txt")),
        "apply payload should include tracked deletion: {apply}",
    );
    assert!(
        !repo.join("tracked.txt").exists(),
        "exp apply should remove tracked.txt because the experiment deleted it"
    );
}

#[test]
fn exp_save_captures_current_workspace_without_running() {
    let tmp = init_scratch_repo();
    let repo = tmp.path();

    fs::write(repo.join("tracked.txt"), "tracked baseline\n").unwrap();
    git(repo, &["add", "tracked.txt"]);
    git(repo, &["commit", "-m", "add tracked file"]);

    fs::write(repo.join("params.yaml"), "model:\n  lr: 0.123\n").unwrap();
    fs::write(repo.join("notes.txt"), "untracked note\n").unwrap();
    fs::remove_file(repo.join("tracked.txt")).unwrap();

    let save = run_exp_json(
        repo,
        "save",
        &["--name", "manual-snapshot", "-I", "notes.txt"],
    );
    let exp_id = save["exp_id"].as_str().unwrap();
    assert_eq!(save["status"], "saved");
    assert_eq!(save["name"], "manual-snapshot");
    assert_eq!(save["stages"].as_object().unwrap().len(), 0);

    let show = run_exp_json(repo, "show", &[exp_id]);
    assert_eq!(show["metadata"]["name"], "manual-snapshot");

    git(repo, &["checkout", "--", "params.yaml", "tracked.txt"]);
    fs::remove_file(repo.join("notes.txt")).unwrap();
    assert_eq!(
        fs::read_to_string(repo.join("params.yaml")).unwrap(),
        "model:\n  lr: 0.001\n"
    );
    assert!(repo.join("tracked.txt").exists());
    assert!(!repo.join("notes.txt").exists());

    let apply = run_exp_json(repo, "apply", &[exp_id]);
    assert!(
        apply["applied"]
            .as_array()
            .unwrap()
            .iter()
            .any(|path| path.as_str() == Some("params.yaml")),
        "saved experiment should restore modified params.yaml: {apply}",
    );
    assert!(
        apply["applied"]
            .as_array()
            .unwrap()
            .iter()
            .any(|path| path.as_str() == Some("notes.txt")),
        "saved experiment should restore untracked notes.txt: {apply}",
    );
    assert!(
        apply["deleted"]
            .as_array()
            .unwrap()
            .iter()
            .any(|path| path.as_str() == Some("tracked.txt")),
        "saved experiment should remember tracked.txt deletion: {apply}",
    );
    assert_eq!(
        fs::read_to_string(repo.join("params.yaml")).unwrap(),
        "model:\n  lr: 0.123\n"
    );
    assert_eq!(
        fs::read_to_string(repo.join("notes.txt")).unwrap(),
        "untracked note\n"
    );
    assert!(!repo.join("tracked.txt").exists());
}

#[test]
fn exp_rename_updates_name_and_force_allows_duplicate_label() {
    let tmp = init_scratch_repo();
    let repo = tmp.path();

    let first = run_exp_json(repo, "run", &["--name", "old-name"]);
    std::thread::sleep(std::time::Duration::from_millis(5));
    let second = run_exp_json(repo, "run", &["--name", "taken-name"]);
    let first_id = first["exp_id"].as_str().unwrap();
    let second_id = second["exp_id"].as_str().unwrap();

    let rename = run_exp_json(repo, "rename", &[first_id, "winner"]);
    assert_eq!(rename["old_name"], "old-name");
    assert_eq!(rename["new_name"], "winner");

    let show = run_exp_json(repo, "show", &[first_id]);
    assert_eq!(show["metadata"]["name"], "winner");

    let (status, stdout, stderr) = run_exp_raw(repo, "rename", &[second_id, "winner"]);
    assert!(
        !status.success(),
        "duplicate rename should fail: stdout={stdout:?} stderr={stderr:?}"
    );

    let forced = run_exp_json(repo, "rename", &[second_id, "winner", "--force"]);
    assert_eq!(forced["new_name"], "winner");
}

#[test]
fn exp_remove_deletes_selected_experiment_by_prefix() {
    let tmp = init_scratch_repo();
    let repo = tmp.path();

    let first = run_exp_json(repo, "run", &["--set-param", "model.lr=0.001"]);
    std::thread::sleep(std::time::Duration::from_millis(5));
    let second = run_exp_json(repo, "run", &["--set-param", "model.lr=0.002"]);
    let first_id = first["exp_id"].as_str().unwrap();
    let second_id = second["exp_id"].as_str().unwrap();

    let remove = run_exp_json(repo, "remove", &[&first_id[..12]]);
    let removed = remove["removed"].as_array().unwrap();
    let kept = remove["kept"].as_array().unwrap();
    assert_eq!(removed.len(), 1);
    assert_eq!(removed[0], first_id);
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0], second_id);

    let (status, _, _) = run_exp_raw(repo, "show", &[first_id]);
    assert!(!status.success(), "removed experiment should be gone");
    assert!(
        !repo
            .join(".crab/workflow/exp")
            .join(format!("{first_id}.workspace"))
            .exists(),
        "removed experiment snapshot should be gone"
    );
    let show_second = run_exp_json(repo, "show", &[second_id]);
    assert_eq!(show_second["metadata"]["exp_id"], second_id);
}

#[test]
fn exp_rm_keep_preserves_selected_experiment() {
    let tmp = init_scratch_repo();
    let repo = tmp.path();

    let first = run_exp_json(repo, "run", &["--set-param", "model.lr=0.001"]);
    std::thread::sleep(std::time::Duration::from_millis(5));
    let second = run_exp_json(repo, "run", &["--set-param", "model.lr=0.002"]);
    std::thread::sleep(std::time::Duration::from_millis(5));
    let third = run_exp_json(repo, "run", &["--set-param", "model.lr=0.003"]);
    let first_id = first["exp_id"].as_str().unwrap();
    let second_id = second["exp_id"].as_str().unwrap();
    let third_id = third["exp_id"].as_str().unwrap();

    let remove = run_exp_json(repo, "rm", &["--keep", &second_id[..12]]);
    let removed = remove["removed"].as_array().unwrap();
    assert_eq!(removed.len(), 2);
    assert!(removed.iter().any(|id| id.as_str() == Some(first_id)));
    assert!(removed.iter().any(|id| id.as_str() == Some(third_id)));
    assert_eq!(remove["kept"], serde_json::json!([second_id]));

    let remaining = run_exp_json(repo, "ls", &[]);
    let experiments = remaining["experiments"].as_array().unwrap();
    assert_eq!(experiments.len(), 1);
    assert_eq!(experiments[0]["id"], second_id);
}

#[test]
fn exp_gc_keeps_newest_n_and_removes_rest() {
    let tmp = init_scratch_repo();
    let repo = tmp.path();

    let mut ids = Vec::new();
    for i in 0..5 {
        let v = format!("0.0{:02}", i + 1);
        let payload = run_exp_json(repo, "run", &["--set", &format!("model.lr={v}")]);
        ids.push(payload["exp_id"].as_str().unwrap().to_owned());
        std::thread::sleep(std::time::Duration::from_millis(5));
    }

    let gc = run_exp_json(repo, "gc", &["--keep", "2"]);
    assert_eq!(gc["keep"], 2);
    assert_eq!(gc["kept"].as_array().unwrap().len(), 2);
    assert_eq!(gc["removed"].as_array().unwrap().len(), 3);

    // The two newest remain on disk; the three oldest are gone.
    let exp_dir = repo.join(".crab/workflow/exp");
    for (idx, id) in ids.iter().enumerate() {
        let meta = exp_dir.join(format!("{id}.meta.json"));
        if idx >= 3 {
            assert!(meta.exists(), "expected kept metadata for {id}");
        } else {
            assert!(!meta.exists(), "expected removed metadata for {id}");
        }
    }
}

#[test]
fn exp_diff_reports_differences() {
    let tmp = init_scratch_repo();
    let repo = tmp.path();

    let payload_a = run_exp_json(repo, "run", &["--set", "model.lr=0.001"]);
    let payload_b = run_exp_json(repo, "run", &["--set", "model.lr=0.002"]);
    let id_a = payload_a["exp_id"].as_str().unwrap();
    let id_b = payload_b["exp_id"].as_str().unwrap();

    let diff = run_exp_json(repo, "diff", &[id_a, id_b]);
    assert_eq!(diff["id_a"], id_a);
    assert_eq!(diff["id_b"], id_b);

    // The `model.lr` override differs — must appear in
    // `params_changed`.
    let changed = &diff["params_changed"]["model.lr"];
    assert!(
        changed.is_array(),
        "params_changed.model.lr missing: {diff}"
    );
    let arr = changed.as_array().unwrap();
    assert_eq!(arr[0], "0.001");
    assert_eq!(arr[1], "0.002");

    // Stage hash must also differ because the override feeds into
    // the stage's params set.
    assert!(
        diff["stages_changed"]["copy"].is_array(),
        "stages_changed.copy missing: {diff}",
    );
}

#[test]
fn exp_diff_accepts_unambiguous_id_prefixes() {
    let tmp = init_scratch_repo();
    let repo = tmp.path();

    let payload_a = run_exp_json(repo, "run", &["--set-param", "model.lr=0.001"]);
    std::thread::sleep(std::time::Duration::from_millis(5));
    let payload_b = run_exp_json(repo, "run", &["--set-param", "model.lr=0.002"]);
    let id_a = payload_a["exp_id"].as_str().unwrap();
    let id_b = payload_b["exp_id"].as_str().unwrap();

    let diff = run_exp_json(repo, "diff", &[&id_a[..12], &id_b[..12]]);
    assert_eq!(diff["id_a"], id_a);
    assert_eq!(diff["id_b"], id_b);
    assert!(diff["params_changed"]["model.lr"].is_array());
}

#[test]
fn exp_diff_renders_markdown_report_with_prefixes() {
    let tmp = init_scratch_repo();
    let repo = tmp.path();

    let payload_a = run_exp_json(repo, "run", &["--set-param", "model.lr=0.001"]);
    std::thread::sleep(std::time::Duration::from_millis(5));
    let payload_b = run_exp_json(repo, "run", &["--set-param", "model.lr=0.002"]);
    let id_a = payload_a["exp_id"].as_str().unwrap();
    let id_b = payload_b["exp_id"].as_str().unwrap();

    let (status, stdout, stderr) = run_exp_raw(
        repo,
        "diff",
        &[&id_a[..12], &id_b[..12], "--md", "--precision", "3"],
    );

    assert!(
        status.success(),
        "exp diff --md failed: status={status:?} stdout={stdout:?} stderr={stderr:?}",
    );
    assert!(stdout.contains("# Experiment diff"));
    assert!(stdout.contains("| Change | Key | Old | New |"));
    assert!(stdout.contains("| changed | model.lr | 0.001 | 0.002 |"));
    assert!(stdout.contains("## Stages"));
}

/// Regression: `collect_local_workflow_live_set` is the interface
/// bucket GC consults to decide which workflow stage entries and
/// experiment metadata blobs are still reachable from this host.
///
/// After three `exp run` calls the live set must include all
/// three experiment IDs plus the union of their stage hashes.
/// After `exp gc --keep 1` removes two of them, the live set must
/// shrink to exactly the surviving experiment's ID + its stage
/// hashes — this is what makes the two GC'd experiments' backing
/// stage entries eligible for remote deletion on the next GC
/// cycle.
#[test]
fn exp_gc_shrinks_workflow_live_set() {
    use crab::workflow::collect_local_workflow_live_set;

    let tmp = init_scratch_repo();
    let repo = tmp.path();

    // Run three experiments with distinct params so each produces
    // a different stage hash. Nudge the clock between runs so
    // their UUIDv7 IDs sort strictly.
    let mut id_to_stage: Vec<(String, String)> = Vec::new();
    for i in 0..3 {
        let v = format!("0.00{}", i + 1);
        let payload = run_exp_json(repo, "run", &["--set", &format!("model.lr={v}")]);
        let id = payload["exp_id"].as_str().unwrap().to_owned();
        let stage_hash = payload["stages"]["copy"]
            .as_str()
            .expect("stage hash missing in exp run payload")
            .to_owned();
        id_to_stage.push((id, stage_hash));
        std::thread::sleep(std::time::Duration::from_millis(5));
    }

    let live = collect_local_workflow_live_set(repo).expect("walker ok");
    assert_eq!(
        live.experiment_ids.len(),
        3,
        "three experiments should contribute three live ids",
    );
    for (id, stage_hash) in &id_to_stage {
        let parsed = id.parse().expect("valid uuidv7 id");
        assert!(
            live.experiment_ids.contains(&parsed),
            "live set missing experiment {id}",
        );
        assert!(
            live.stage_hashes.contains(stage_hash),
            "live set missing stage hash for {id}",
        );
    }

    // GC keeps only the newest. `cmd exp gc` returns the kept/
    // removed lists in the payload, but the source of truth for
    // this test is what the live-set walker sees on disk
    // afterward — that is the signal the remote GC walker will
    // actually consume.
    let gc_payload = run_exp_json(repo, "gc", &["--keep", "1"]);
    assert_eq!(gc_payload["keep"], 1);
    assert_eq!(gc_payload["kept"].as_array().unwrap().len(), 1);
    assert_eq!(gc_payload["removed"].as_array().unwrap().len(), 2);

    let live_after = collect_local_workflow_live_set(repo).expect("walker ok");
    assert_eq!(
        live_after.experiment_ids.len(),
        1,
        "one experiment should remain after gc --keep 1",
    );

    // The surviving experiment is the newest one — index 2 in our
    // creation order (UUIDv7 sorts chronologically).
    let (survivor_id, survivor_stage) = id_to_stage.last().unwrap();
    let survivor_parsed = survivor_id.parse().expect("valid uuidv7 id");
    assert!(
        live_after.experiment_ids.contains(&survivor_parsed),
        "newest experiment should survive gc",
    );
    assert_eq!(
        live_after.stage_hashes.len(),
        1,
        "only the survivor's stage hashes should remain",
    );
    assert!(
        live_after.stage_hashes.contains(survivor_stage),
        "survivor's stage hash missing from post-gc live set",
    );
    // The removed experiments' stage hashes are gone — they'd now
    // be eligible for remote deletion (subject to the remote
    // grace period).
    for (id, stage_hash) in &id_to_stage[..2] {
        let parsed = id.parse().expect("valid uuidv7 id");
        assert!(
            !live_after.experiment_ids.contains(&parsed),
            "gc'd experiment {id} should be absent from live set",
        );
        assert!(
            !live_after.stage_hashes.contains(stage_hash),
            "gc'd experiment {id}'s stage hash should be absent from live set",
        );
    }
}

/// Regression for UUIDv7 sort order under clock skew.
///
/// Two experiments A and B are written with UUIDv7 ids in
/// chronological order (A first, B second), but their
/// `started_at` wall-clock timestamps are deliberately inverted
/// — A carries a future-dated `started_at`, B carries a
/// past-dated one. This simulates what happens when the host
/// clock jumps (NTP correction, resumed-from-sleep, manual
/// `date` set), so the UUIDv7 timestamp and the captured
/// `started_at` disagree.
///
/// The test writes `.meta.json` blobs directly to disk rather
/// than going through `crab exp run`, because `exp run` uses
/// `now_rfc3339_millis()` for `started_at` — we need to control
/// both fields independently to exercise the skew.
///
/// Assertion: `crab exp ls --json` surfaces B before A because
/// B's UUID is newer. A sort by `started_at` would invert that.
#[test]
fn exp_ls_orders_by_uuidv7_not_wall_clock_started_at() {
    use std::collections::BTreeMap;

    use crab::workflow::{EXPERIMENT_METADATA_SCHEMA_VERSION, ExperimentId, ExperimentMetadata};

    let tmp = init_scratch_repo();
    let repo = tmp.path();

    // Mint A first, sleep, then B — UUIDv7's millisecond-
    // resolution timestamp guarantees `a_id < b_id` lexically.
    let a_id = ExperimentId::new_v7();
    std::thread::sleep(std::time::Duration::from_millis(5));
    let b_id = ExperimentId::new_v7();
    assert!(
        a_id.to_string() < b_id.to_string(),
        "UUIDv7 lex order must follow creation order: a={a_id}, b={b_id}",
    );

    // A gets the *later* wall-clock timestamp; B gets the
    // *earlier* one. If `exp ls` sorts by `started_at` we'd see
    // [A, B]; if it sorts by UUID (correct) we see [B, A].
    let make = |id: ExperimentId, started_at: &str| ExperimentMetadata {
        schema_version: EXPERIMENT_METADATA_SCHEMA_VERSION,
        exp_id: id,
        base_commit: "0".repeat(40),
        queue_commit: None,
        name: None,
        message: None,
        param_overrides: BTreeMap::new(),
        stages: BTreeMap::new(),
        metrics: BTreeMap::new(),
        cli_args: Vec::new(),
        host_fingerprint: "test-host".into(),
        started_at: started_at.to_owned(),
        ended_at: None,
        status: "completed".to_owned(),
    };

    let meta_a = make(a_id, "2099-12-31T23:59:59.000Z");
    let meta_b = make(b_id, "2020-01-01T00:00:00.000Z");

    let exp_dir = repo.join(".crab/workflow/exp");
    fs::create_dir_all(&exp_dir).unwrap();
    fs::write(
        exp_dir.join(format!("{a_id}.meta.json")),
        meta_a.canonical_json().unwrap(),
    )
    .unwrap();
    fs::write(
        exp_dir.join(format!("{b_id}.meta.json")),
        meta_b.canonical_json().unwrap(),
    )
    .unwrap();

    let ls = run_exp_json(repo, "ls", &[]);
    let experiments = ls["experiments"].as_array().unwrap();
    assert_eq!(
        experiments.len(),
        2,
        "both meta blobs should be surfaced: {ls}",
    );

    // B (newer UUID) first, A (older UUID) second — even though
    // B's `started_at` is nearly 80 years earlier than A's.
    assert_eq!(
        experiments[0]["id"],
        b_id.to_string(),
        "newer UUID must sort first regardless of started_at",
    );
    assert_eq!(experiments[1]["id"], a_id.to_string());

    // The inversion is real: the first experiment in the list
    // carries the *earlier* wall-clock timestamp. If the sort key
    // ever switches to `started_at`, this assertion flips and the
    // test fails loudly.
    assert_eq!(
        experiments[0]["started_at"], "2020-01-01T00:00:00.000Z",
        "sort key must be UUID, not started_at",
    );
    assert_eq!(experiments[1]["started_at"], "2099-12-31T23:59:59.000Z");
}
