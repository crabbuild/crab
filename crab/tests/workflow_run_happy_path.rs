//! Integration tests for `crab run` — happy-path cache semantics
//! and orphan-sidecar sweep.
//!
//! Drives the real `crab` binary via `Command::new(env!("CARGO_BIN_EXE_crab"))`
//! and reads the SQLite run journal plus the per-run log-file layout
//! to confirm a cache hit short-circuits before the child process is
//! ever spawned. Modifying a dep should invalidate the cache and
//! trigger a miss (Running transition, fresh per-stage log file).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use rusqlite::Connection;
use tempfile::TempDir;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_crab")
}

/// Run `crab run --name copy --deps a.txt --outs b.txt -- /bin/cp
/// a.txt b.txt` with `--json` so the caller can introspect whether
/// the run was a cache hit.
fn run_copy_stage_json(repo: &Path) -> (std::process::ExitStatus, serde_json::Value) {
    let output = Command::new(bin())
        .current_dir(repo)
        .env("CRAB_WORKFLOW_ENABLED", "1")
        .args([
            "run", "--name", "copy", "--json", "--deps", "a.txt", "--outs", "b.txt", "--",
            "/bin/cp", "a.txt", "b.txt",
        ])
        .output()
        .expect("crab run should spawn");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let envelope: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("parse --json envelope failed: {e}; stdout={stdout:?}"));
    (output.status, envelope)
}

/// Run the same stage without structured output, for tests that only
/// care about exit status + side effects.
fn run_copy_stage(repo: &Path) -> std::process::ExitStatus {
    Command::new(bin())
        .current_dir(repo)
        .env("CRAB_WORKFLOW_ENABLED", "1")
        .args([
            "run", "--name", "copy", "--deps", "a.txt", "--outs", "b.txt", "--", "/bin/cp",
            "a.txt", "b.txt",
        ])
        .status()
        .expect("crab run should spawn")
}

fn run_summary_stage_cache_hit(envelope: &serde_json::Value, stage_name: &str) -> bool {
    assert_eq!(envelope["schema"], "workflow.run");
    let stages = envelope["data"]["stages"]
        .as_array()
        .expect("workflow.run data.stages array");
    let stage = stages
        .iter()
        .find(|stage| stage["stage_name"] == stage_name)
        .unwrap_or_else(|| panic!("stage {stage_name:?} missing from workflow.run summary"));
    stage["cache_hit"]
        .as_bool()
        .expect("stage cache_hit boolean")
}

/// Enumerate the journal directories under `.crab/workflow/runs`.
/// Returns `(run_id, directory_path)` tuples, stable-sorted by
/// run_id (UUIDv7, so chronological).
fn journal_dirs(repo: &Path) -> Vec<(String, PathBuf)> {
    let runs_dir = repo.join(".crab/workflow/runs");
    let mut out: Vec<(String, PathBuf)> = fs::read_dir(&runs_dir)
        .map(|rd| {
            rd.filter_map(Result::ok)
                .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
                .collect()
        })
        .unwrap_or_default();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Count `stage_runs` rows in a journal that landed in the `Running`
/// state. On a miss the row transitions through Running → Produced →
/// … → Committed, overwriting the state column each time — so we
/// can't detect "ever was Running" from the state column alone.
/// Instead we look for the per-stage log file, which the supervisor
/// creates only when the child is actually spawned.
fn supervisor_log_exists(run_dir: &Path, stage_name: &str) -> bool {
    run_dir.join(format!("stage-{stage_name}.log")).exists()
}

/// Sanity: the journal opens, and the single stage row reached
/// `Committed` (tag 11). We don't rely on the state-tag column for
/// cache-hit vs miss — see `supervisor_log_exists`.
fn assert_stage_committed(run_dir: &Path, stage_name: &str) {
    let journal = run_dir.join("journal.db");
    let conn = Connection::open(&journal).expect("open journal.db");
    let state: i64 = conn
        .query_row(
            "SELECT state FROM stage_runs WHERE stage_name = ?1",
            rusqlite::params![stage_name],
            |r| r.get(0),
        )
        .expect("stage row exists");
    // `Committed` is tag 10 per `StageState::sql_tag`. Keeping this
    // as a magic number rather than reaching into the crate API so
    // the test stays an integration test.
    assert_eq!(state, 10, "stage should be Committed, got tag {state}");
}

/// Full trajectory for R1: first run is a miss, second run with
/// identical inputs is a cache hit (no supervisor log), third run
/// after modifying `a.txt` is a miss again.
#[test]
fn cache_hit_on_second_run_then_miss_after_dep_change() {
    let tmp = TempDir::new().unwrap();

    // First run: miss. cp executes, b.txt produced.
    fs::write(tmp.path().join("a.txt"), b"payload-v1").unwrap();
    let (status, envelope) = run_copy_stage_json(tmp.path());
    assert!(status.success(), "first run should succeed: {status:?}");
    assert_eq!(envelope["data"]["cache_hit"], false);
    assert_eq!(
        fs::read(tmp.path().join("b.txt")).unwrap(),
        b"payload-v1".to_vec()
    );

    let after_first = journal_dirs(tmp.path());
    assert_eq!(after_first.len(), 1, "one journal after first run");
    let first_dir = &after_first[0].1;
    assert_stage_committed(first_dir, "copy");
    assert!(
        supervisor_log_exists(first_dir, "copy"),
        "first run (miss) MUST have a per-stage log",
    );

    // Second run: same inputs → cache hit. No supervisor log for
    // the new journal (executor short-circuits before spawning).
    let (status, envelope) = run_copy_stage_json(tmp.path());
    assert!(
        status.success(),
        "second run (cache hit) should succeed: {status:?}"
    );
    assert_eq!(envelope["data"]["cache_hit"], true);
    assert_eq!(
        fs::read(tmp.path().join("b.txt")).unwrap(),
        b"payload-v1".to_vec(),
    );

    let after_second = journal_dirs(tmp.path());
    assert_eq!(after_second.len(), 2, "two journals after second run");
    let first_run_id = &after_first[0].0;
    let (_second_run_id, second_dir) = after_second
        .iter()
        .find(|(rid, _)| rid != first_run_id)
        .expect("second journal must exist");
    assert_stage_committed(second_dir, "copy");
    assert!(
        !supervisor_log_exists(second_dir, "copy"),
        "cache-hit run MUST NOT spawn a child (no supervisor log)",
    );

    // Third run: modify the dep → stage_hash changes → miss.
    fs::write(tmp.path().join("a.txt"), b"payload-v2").unwrap();
    let (status, envelope) = run_copy_stage_json(tmp.path());
    assert!(status.success(), "third run should succeed: {status:?}");
    assert_eq!(envelope["data"]["cache_hit"], false);
    assert_eq!(
        fs::read(tmp.path().join("b.txt")).unwrap(),
        b"payload-v2".to_vec()
    );

    let after_third = journal_dirs(tmp.path());
    assert_eq!(after_third.len(), 3, "three journals after third run");
    let second_run_id = &after_second
        .iter()
        .find(|(rid, _)| rid != first_run_id)
        .expect("second journal")
        .0;
    let (_third_run_id, third_dir) = after_third
        .iter()
        .find(|(rid, _)| rid != first_run_id && rid != second_run_id)
        .expect("third journal must exist");
    assert_stage_committed(third_dir, "copy");
    assert!(
        supervisor_log_exists(third_dir, "copy"),
        "miss after dep change MUST have a per-stage log",
    );
}

#[test]
fn repro_alias_runs_yaml_stage_with_dvc_flags() {
    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join("a.txt"), b"payload-v1").unwrap();
    fs::write(
        tmp.path().join("crab.yaml"),
        concat!(
            "stages:\n",
            "  copy:\n",
            "    cmd: cp a.txt b.txt\n",
            "    deps:\n",
            "      - a.txt\n",
            "    outs:\n",
            "      - b.txt\n",
        ),
    )
    .unwrap();

    let output = Command::new(bin())
        .current_dir(tmp.path())
        .env("CRAB_WORKFLOW_ENABLED", "1")
        .args(["repro", "--json", "--no-run-cache", "copy"])
        .output()
        .expect("crab repro should spawn");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "crab repro should succeed: stdout={stdout:?} stderr={stderr:?}"
    );
    assert_eq!(
        fs::read(tmp.path().join("b.txt")).unwrap(),
        b"payload-v1".to_vec()
    );
    let envelope: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("parse --json envelope failed: {e}; stdout={stdout:?}"));
    assert_eq!(envelope["schema"], "workflow.run");
}

#[test]
fn nondeterministic_inline_stage_runs_on_second_invocation() {
    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join("a.txt"), b"payload").unwrap();
    let marker = tmp.path().join("marker.log");
    let script = format!(
        "cp a.txt b.txt && printf 'run\\n' >> '{}'",
        marker.display()
    );

    for attempt in 1..=2 {
        let output = Command::new(bin())
            .current_dir(tmp.path())
            .env("CRAB_WORKFLOW_ENABLED", "1")
            .args([
                "run",
                "--name",
                "poll",
                "--json",
                "--nondeterministic",
                "--deps",
                "a.txt",
                "--outs",
                "b.txt",
                "--",
                "/bin/sh",
                "-c",
                &script,
            ])
            .output()
            .expect("crab run should spawn");
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(
            output.status.success(),
            "attempt {attempt} failed: stdout={stdout:?} stderr={stderr:?}"
        );
        let envelope: serde_json::Value = serde_json::from_str(stdout.trim())
            .unwrap_or_else(|e| panic!("parse --json failed: {e}; stdout={stdout:?}"));
        assert_eq!(envelope["data"]["cache_hit"], false);
    }

    assert_eq!(fs::read_to_string(marker).unwrap(), "run\nrun\n");
}

#[test]
fn dvc_cmd_list_runs_commands_in_order() {
    let tmp = TempDir::new().unwrap();
    let yaml = r#"
stages:
  multi:
    cmd:
      - "printf first > marker.txt"
      - "printf second > out.txt"
    outs:
      - out.txt
"#;
    fs::write(tmp.path().join("crab.yaml"), yaml).unwrap();

    let output = Command::new(bin())
        .current_dir(tmp.path())
        .env("CRAB_WORKFLOW_ENABLED", "1")
        .args(["run", "multi"])
        .output()
        .expect("crab run should spawn");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "cmd list run failed: stdout={stdout:?} stderr={stderr:?}"
    );
    assert_eq!(
        fs::read_to_string(tmp.path().join("marker.txt")).unwrap(),
        "first"
    );
    assert_eq!(
        fs::read_to_string(tmp.path().join("out.txt")).unwrap(),
        "second"
    );
}

#[test]
fn dvc_cmd_list_stops_after_first_failure() {
    let tmp = TempDir::new().unwrap();
    let yaml = r#"
stages:
  multi:
    cmd:
      - "printf first > marker.txt"
      - "exit 7"
      - "printf never > out.txt"
    outs:
      - out.txt
"#;
    fs::write(tmp.path().join("crab.yaml"), yaml).unwrap();

    let output = Command::new(bin())
        .current_dir(tmp.path())
        .env("CRAB_WORKFLOW_ENABLED", "1")
        .args(["run", "multi"])
        .output()
        .expect("crab run should spawn");
    assert!(
        !output.status.success(),
        "cmd list with failing middle command must fail"
    );
    assert_eq!(
        fs::read_to_string(tmp.path().join("marker.txt")).unwrap(),
        "first"
    );
    assert!(
        !tmp.path().join("out.txt").exists(),
        "command after a failing list entry must not run"
    );
}

#[test]
fn dvc_path_key_out_settings_drive_cache_policy() {
    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join("a.txt"), b"payload").unwrap();
    let yaml = r#"
stages:
  build:
    cmd: "cp a.txt out.txt && printf 'run\n' >> marker.txt"
    deps:
      - a.txt
    outs:
      - out.txt:
          cache: false
"#;
    fs::write(tmp.path().join("crab.yaml"), yaml).unwrap();

    for attempt in 1..=2 {
        let output = Command::new(bin())
            .current_dir(tmp.path())
            .env("CRAB_WORKFLOW_ENABLED", "1")
            .args(["run", "build"])
            .output()
            .expect("crab run should spawn");
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(
            output.status.success(),
            "attempt {attempt} failed: stdout={stdout:?} stderr={stderr:?}"
        );
    }

    assert_eq!(
        fs::read_to_string(tmp.path().join("marker.txt")).unwrap(),
        "run\nrun\n"
    );
    assert_eq!(
        fs::read(tmp.path().join("out.txt")).unwrap(),
        b"payload".to_vec()
    );
}

#[test]
fn workflow_run_records_declared_metric_hash_in_lockfile() {
    let tmp = TempDir::new().unwrap();
    let yaml = r#"
stages:
  train:
    cmd: "mkdir -p metrics && printf '{\"accuracy\":0.9}\n' > metrics/train.json"
    metrics:
      - metrics/train.json
"#;
    fs::write(tmp.path().join("crab.yaml"), yaml).unwrap();

    let output = Command::new(bin())
        .current_dir(tmp.path())
        .env("CRAB_WORKFLOW_ENABLED", "1")
        .args(["run", "train"])
        .output()
        .expect("crab run should spawn");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "workflow run failed: stdout={stdout:?} stderr={stderr:?}"
    );

    let lock = fs::read_to_string(tmp.path().join("crab.lock")).unwrap();
    assert!(lock.contains("- hash: \"b3:"), "lockfile={lock}");
    assert!(
        lock.contains("path: \"metrics/train.json\""),
        "lockfile={lock}"
    );
}

#[test]
fn workflow_run_records_declared_plot_hash_in_lockfile() {
    let tmp = TempDir::new().unwrap();
    let yaml = r#"
stages:
  train:
    cmd: "mkdir -p plots && printf 'epoch,loss\n1,0.5\n' > plots/loss.csv"
    plots:
      - plots/loss.csv
"#;
    fs::write(tmp.path().join("crab.yaml"), yaml).unwrap();

    let output = Command::new(bin())
        .current_dir(tmp.path())
        .env("CRAB_WORKFLOW_ENABLED", "1")
        .args(["run", "train"])
        .output()
        .expect("crab run should spawn");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "workflow run failed: stdout={stdout:?} stderr={stderr:?}"
    );

    let lock = fs::read_to_string(tmp.path().join("crab.lock")).unwrap();
    assert!(
        lock.contains("    plots:\n    - hash: \"b3:"),
        "lockfile={lock}"
    );
    assert!(lock.contains("path: \"plots/loss.csv\""), "lockfile={lock}");
}

#[test]
fn cache_hit_materializes_metric_and_plot_artifacts() {
    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join("input.txt"), b"payload").unwrap();
    let yaml = r#"
stages:
  report:
    cmd: "mkdir -p metrics plots && cp input.txt metrics/report.json && cp input.txt plots/loss.csv"
    deps:
      - input.txt
    metrics:
      - metrics/report.json
    plots:
      - plots/loss.csv
"#;
    fs::write(tmp.path().join("crab.yaml"), yaml).unwrap();

    let first = Command::new(bin())
        .current_dir(tmp.path())
        .env("CRAB_WORKFLOW_ENABLED", "1")
        .args(["run", "--json", "report"])
        .output()
        .expect("crab run should spawn");
    let first_stdout = String::from_utf8_lossy(&first.stdout).into_owned();
    let first_stderr = String::from_utf8_lossy(&first.stderr).into_owned();
    assert!(
        first.status.success(),
        "first run failed: stdout={first_stdout:?} stderr={first_stderr:?}"
    );
    let first_json: serde_json::Value = serde_json::from_str(first_stdout.trim())
        .unwrap_or_else(|e| panic!("parse first --json failed: {e}; stdout={first_stdout:?}"));
    assert!(!run_summary_stage_cache_hit(&first_json, "report"));

    fs::remove_file(tmp.path().join("metrics/report.json")).unwrap();
    fs::remove_file(tmp.path().join("plots/loss.csv")).unwrap();

    let second = Command::new(bin())
        .current_dir(tmp.path())
        .env("CRAB_WORKFLOW_ENABLED", "1")
        .args(["run", "--json", "report"])
        .output()
        .expect("crab run should spawn");
    let second_stdout = String::from_utf8_lossy(&second.stdout).into_owned();
    let second_stderr = String::from_utf8_lossy(&second.stderr).into_owned();
    assert!(
        second.status.success(),
        "second run failed: stdout={second_stdout:?} stderr={second_stderr:?}"
    );
    let second_json: serde_json::Value = serde_json::from_str(second_stdout.trim())
        .unwrap_or_else(|e| panic!("parse second --json failed: {e}; stdout={second_stdout:?}"));
    assert!(run_summary_stage_cache_hit(&second_json, "report"));

    assert_eq!(
        fs::read(tmp.path().join("metrics/report.json")).unwrap(),
        b"payload".to_vec()
    );
    assert_eq!(
        fs::read(tmp.path().join("plots/loss.csv")).unwrap(),
        b"payload".to_vec()
    );
}

#[test]
fn wdir_stage_resolves_paths_and_replays_cache_from_repo_relative_entry() {
    let tmp = TempDir::new().unwrap();
    let training = tmp.path().join("training");
    fs::create_dir_all(&training).unwrap();
    fs::write(training.join("data.csv"), b"payload").unwrap();
    let yaml = r#"
stages:
  train:
    cmd: "cp data.csv model.pkl && printf 'run\n' >> marker.txt"
    wdir: training
    deps:
      - data.csv
    outs:
      - model.pkl
"#;
    fs::write(tmp.path().join("crab.yaml"), yaml).unwrap();

    for (attempt, expected_hit) in [(1, false), (2, true)] {
        if attempt == 2 {
            fs::remove_file(training.join("model.pkl")).unwrap();
        }

        let output = Command::new(bin())
            .current_dir(tmp.path())
            .env("CRAB_WORKFLOW_ENABLED", "1")
            .args(["run", "--json", "train"])
            .output()
            .expect("crab run should spawn");
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(
            output.status.success(),
            "attempt {attempt} failed: stdout={stdout:?} stderr={stderr:?}"
        );
        let envelope: serde_json::Value = serde_json::from_str(stdout.trim())
            .unwrap_or_else(|e| panic!("parse --json failed: {e}; stdout={stdout:?}"));
        assert_eq!(
            run_summary_stage_cache_hit(&envelope, "train"),
            expected_hit
        );
        assert_eq!(
            fs::read(training.join("model.pkl")).unwrap(),
            b"payload".to_vec()
        );
    }

    assert_eq!(
        fs::read_to_string(training.join("marker.txt")).unwrap(),
        "run\n"
    );
    let lockfile = fs::read_to_string(tmp.path().join("crab.lock")).unwrap();
    assert!(lockfile.contains("\"training/data.csv\""), "{lockfile}");
    assert!(lockfile.contains("\"training/model.pkl\""), "{lockfile}");
}

#[test]
fn wdir_stage_params_default_to_stage_directory() {
    let tmp = TempDir::new().unwrap();
    let training = tmp.path().join("training");
    fs::create_dir_all(&training).unwrap();
    fs::write(training.join("data.csv"), b"payload").unwrap();
    fs::write(training.join("params.yaml"), b"model:\n  lr: 0.01\n").unwrap();
    fs::write(tmp.path().join("params.yaml"), b"model:\n  lr: 9.99\n").unwrap();
    let yaml = r#"
stages:
  train:
    cmd: "cp data.csv model.pkl && printf 'run\n' >> marker.txt"
    wdir: training
    deps:
      - data.csv
    params:
      - model.lr
    outs:
      - model.pkl
"#;
    fs::write(tmp.path().join("crab.yaml"), yaml).unwrap();

    for attempt in 1..=2 {
        let output = Command::new(bin())
            .current_dir(tmp.path())
            .env("CRAB_WORKFLOW_ENABLED", "1")
            .args(["run", "--json", "train"])
            .output()
            .expect("crab run should spawn");
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(
            output.status.success(),
            "attempt {attempt} failed: stdout={stdout:?} stderr={stderr:?}"
        );
        let envelope: serde_json::Value = serde_json::from_str(stdout.trim())
            .unwrap_or_else(|e| panic!("parse --json failed: {e}; stdout={stdout:?}"));
        assert!(!run_summary_stage_cache_hit(&envelope, "train"));

        if attempt == 1 {
            fs::write(training.join("params.yaml"), b"model:\n  lr: 0.02\n").unwrap();
        }
    }

    assert_eq!(
        fs::read_to_string(training.join("marker.txt")).unwrap(),
        "run\nrun\n"
    );
    let lockfile = fs::read_to_string(tmp.path().join("crab.lock")).unwrap();
    assert!(lockfile.contains("model.lr"), "{lockfile}");
    assert!(lockfile.contains("\"0.02\""), "{lockfile}");
}

#[test]
fn default_params_yaml_templates_drive_stage_hash() {
    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join("input.txt"), b"payload").unwrap();
    fs::write(
        tmp.path().join("params.yaml"),
        b"input: input.txt\noutput: output.txt\nmarker: marker.txt\nmessage: first\n",
    )
    .unwrap();
    let yaml = r#"
stages:
  build:
    cmd: "cp ${input} ${output} && printf '${message}\n' >> ${marker}"
    deps:
      - ${input}
    outs:
      - ${output}
"#;
    fs::write(tmp.path().join("crab.yaml"), yaml).unwrap();

    for (attempt, expected_hit) in [(1, false), (2, true)] {
        if attempt == 2 {
            fs::remove_file(tmp.path().join("output.txt")).unwrap();
        }
        let output = Command::new(bin())
            .current_dir(tmp.path())
            .env("CRAB_WORKFLOW_ENABLED", "1")
            .args(["run", "--json", "build"])
            .output()
            .expect("crab run should spawn");
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        assert!(
            output.status.success(),
            "attempt {attempt} failed: stdout={stdout:?} stderr={stderr:?}"
        );
        let envelope: serde_json::Value = serde_json::from_str(stdout.trim())
            .unwrap_or_else(|e| panic!("parse --json failed: {e}; stdout={stdout:?}"));
        assert_eq!(
            run_summary_stage_cache_hit(&envelope, "build"),
            expected_hit
        );
        assert_eq!(fs::read(tmp.path().join("output.txt")).unwrap(), b"payload");
    }

    fs::write(
        tmp.path().join("params.yaml"),
        b"input: input.txt\noutput: output.txt\nmarker: marker.txt\nmessage: second\n",
    )
    .unwrap();
    let output = Command::new(bin())
        .current_dir(tmp.path())
        .env("CRAB_WORKFLOW_ENABLED", "1")
        .args(["run", "--json", "build"])
        .output()
        .expect("crab run should spawn");
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    assert!(
        output.status.success(),
        "param change run failed: stdout={stdout:?} stderr={stderr:?}"
    );
    let envelope: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("parse --json failed: {e}; stdout={stdout:?}"));
    assert!(!run_summary_stage_cache_hit(&envelope, "build"));
    assert_eq!(
        fs::read_to_string(tmp.path().join("marker.txt")).unwrap(),
        "first\nsecond\n"
    );
}

/// Orphan-sidecar sweep: pre-create a `.crab.tmp.<uuid>` at a
/// declared out path; after a run the sweep must remove it. The
/// UUID here belongs to no in-flight journal, so it is orphan by
/// definition.
#[test]
fn orphan_sidecar_at_declared_out_path_is_swept() {
    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join("a.txt"), b"payload").unwrap();

    let orphan = tmp
        .path()
        .join("b.txt.crab.tmp.01234567-0123-7000-8000-000000000000");
    fs::write(&orphan, b"orphan-bytes").unwrap();
    assert!(orphan.exists(), "sanity: sidecar seeded");

    let status = run_copy_stage(tmp.path());
    assert!(status.success(), "crab run should succeed: {status:?}");

    assert!(
        !orphan.exists(),
        "orphan sidecar should be swept after the run: {}",
        orphan.display()
    );
    assert_eq!(
        fs::read(tmp.path().join("b.txt")).unwrap(),
        b"payload".to_vec()
    );
}

/// After a successful inline single-stage run, `crab.lock` must
/// exist and contain the committed stage entry in canonical form.
#[test]
fn lockfile_written_after_inline_single_stage_run() {
    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join("a.txt"), b"lockfile-test").unwrap();

    let status = run_copy_stage(tmp.path());
    assert!(status.success(), "crab run should succeed: {status:?}");

    let lockfile_path = tmp.path().join("crab.lock");
    assert!(
        lockfile_path.exists(),
        "crab.lock must exist after a successful inline run"
    );

    let content = fs::read_to_string(&lockfile_path).unwrap();

    // Canonical form checks:
    // 1. Contains the stage name as a YAML key
    assert!(
        content.contains("  copy:\n"),
        "lockfile must contain the stage name as a key: {content}"
    );

    // 2. String values are double-quoted (canonical form per R5)
    assert!(
        content.contains("\"crab.stage.v1\""),
        "lockfile must use double-quoted strings for hash algo: {content}"
    );

    // 3. Contains a b3: prefixed hash for the stage
    assert!(
        content.contains("\"b3:"),
        "lockfile must contain b3-prefixed hashes: {content}"
    );

    // 4. Contains dep entry for a.txt
    assert!(
        content.contains("\"a.txt\""),
        "lockfile must contain the dep path: {content}"
    );

    // 5. Contains out entry for b.txt
    assert!(
        content.contains("\"b.txt\""),
        "lockfile must contain the out path: {content}"
    );

    // 6. Top-level keys are sorted: crab_hash_algo < schema_version < stages
    let algo_pos = content.find("crab_hash_algo").unwrap();
    let schema_pos = content.find("schema_version").unwrap();
    let stages_pos = content.find("stages").unwrap();
    assert!(
        algo_pos < schema_pos && schema_pos < stages_pos,
        "top-level keys must be sorted: algo@{algo_pos}, schema@{schema_pos}, stages@{stages_pos}"
    );
}

/// A second run with the same inputs should still produce a valid
/// lockfile (upsert is idempotent). Modifying the dep should update
/// the lockfile with the new hash.
#[test]
fn lockfile_updated_on_dep_change() {
    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join("a.txt"), b"version-1").unwrap();

    let status = run_copy_stage(tmp.path());
    assert!(status.success());

    let lockfile_v1 = fs::read_to_string(tmp.path().join("crab.lock")).unwrap();

    // Modify dep and re-run
    fs::write(tmp.path().join("a.txt"), b"version-2").unwrap();
    let status = run_copy_stage(tmp.path());
    assert!(status.success());

    let lockfile_v2 = fs::read_to_string(tmp.path().join("crab.lock")).unwrap();

    // The lockfile should have changed because the dep hash changed
    assert_ne!(
        lockfile_v1, lockfile_v2,
        "lockfile must update when dep content changes"
    );

    // Both versions should be valid canonical form
    assert!(lockfile_v2.contains("  copy:\n"));
    assert!(lockfile_v2.contains("\"b3:"));
}

/// Lockfile preserves entries from prior runs of other stages.
/// Running stage "copy" then stage "copy2" should leave both in
/// the lockfile.
#[test]
fn lockfile_preserves_entries_from_other_stages() {
    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join("a.txt"), b"shared-dep").unwrap();

    // Run first stage: "copy"
    let status = run_copy_stage(tmp.path());
    assert!(status.success());

    let lockfile_after_first = fs::read_to_string(tmp.path().join("crab.lock")).unwrap();
    assert!(lockfile_after_first.contains("  copy:\n"));

    // Run a different stage: "copy2" with different out
    let status = Command::new(bin())
        .current_dir(tmp.path())
        .env("CRAB_WORKFLOW_ENABLED", "1")
        .args([
            "run", "--name", "copy2", "--deps", "a.txt", "--outs", "c.txt", "--", "/bin/cp",
            "a.txt", "c.txt",
        ])
        .status()
        .expect("crab run should spawn");
    assert!(status.success());

    let lockfile_after_second = fs::read_to_string(tmp.path().join("crab.lock")).unwrap();

    // Both stages should be present
    assert!(
        lockfile_after_second.contains("  copy:\n"),
        "lockfile must preserve the first stage entry"
    );
    assert!(
        lockfile_after_second.contains("  copy2:\n"),
        "lockfile must contain the second stage entry"
    );
}
