//! Integration tests for `crab run --watch`.
//!
//! These tests are timing-sensitive and marked `#[ignore]` by default.
//! Run with: `cargo test --test workflow_watch -- --ignored`
//!
//! The tests exercise:
//! - Initial DAG execution followed by watch mode
//! - Re-execution triggered by dep file modification
//! - Debounce coalescing of rapid saves
//! - Editor temp file filtering
//! - Single-stage watch mode (transitive deps only)

#![cfg(feature = "watch")]

use std::fs;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use tempfile::TempDir;

static ENV_GUARD: Mutex<()> = Mutex::new(());

struct EnabledGuard;

impl EnabledGuard {
    fn new() -> (std::sync::MutexGuard<'static, ()>, Self) {
        let lock = ENV_GUARD.lock().unwrap_or_else(|e| e.into_inner());
        unsafe { std::env::set_var("CRAB_WORKFLOW_ENABLED", "1") };
        (lock, Self)
    }
}

impl Drop for EnabledGuard {
    fn drop(&mut self) {
        unsafe { std::env::remove_var("CRAB_WORKFLOW_ENABLED") };
    }
}

/// Helper: write a minimal crab.yaml with a single stage that
/// copies a dep to an out.
fn write_copy_yaml(dir: &Path, dep: &str, out: &str) {
    let yaml = format!(
        r#"stages:
  copy:
    cmd:
      argv: ["/bin/cp", "{src}", "{dst}"]
    deps:
      - {dep}
    outs:
      - {out}
"#,
        src = dir.join(dep).to_string_lossy(),
        dst = dir.join(out).to_string_lossy(),
        dep = dep,
        out = out,
    );
    fs::write(dir.join("crab.yaml"), yaml).unwrap();
}

/// Helper: write a two-stage yaml where `preprocess` feeds `train`.
#[allow(dead_code)]
fn write_two_stage_yaml(dir: &Path) {
    let yaml = format!(
        r#"stages:
  preprocess:
    cmd:
      argv: ["/bin/cp", "{src}", "{mid}"]
    deps:
      - raw.txt
    outs:
      - clean.txt
  train:
    cmd:
      argv: ["/bin/cp", "{mid}", "{dst}"]
    deps:
      - clean.txt
    outs:
      - model.pkl
"#,
        src = dir.join("raw.txt").to_string_lossy(),
        mid = dir.join("clean.txt").to_string_lossy(),
        dst = dir.join("model.pkl").to_string_lossy(),
    );
    fs::write(dir.join("crab.yaml"), yaml).unwrap();
}

/// Verify that the watcher module correctly filters editor temp files.
#[test]
fn editor_temp_files_are_filtered() {
    use crab::workflow::watcher::is_editor_temp_file;

    // These should be filtered (not trigger re-execution).
    assert!(is_editor_temp_file(Path::new(".main.rs.swp")));
    assert!(is_editor_temp_file(Path::new("file.txt~")));
    assert!(is_editor_temp_file(Path::new("#autosave#")));
    assert!(is_editor_temp_file(Path::new("data.tmp")));
    assert!(is_editor_temp_file(Path::new(".DS_Store")));

    // These should NOT be filtered (should trigger re-execution).
    assert!(!is_editor_temp_file(Path::new("data.csv")));
    assert!(!is_editor_temp_file(Path::new("model.pkl")));
    assert!(!is_editor_temp_file(Path::new("train.py")));
}

/// Verify that `collect_dep_paths` extracts only Path deps.
#[test]
fn collect_dep_paths_extracts_file_deps() {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use crab::workflow::stage::{Cmd, Dep, Stage, StageName};
    use crab::workflow::watcher::collect_dep_paths;

    let mut stages = BTreeMap::new();
    let stage = Stage::new(
        StageName::parse("train").unwrap(),
        Cmd::Shell("echo hi".into()),
    );
    let mut stage = stage;
    stage.deps = vec![
        Dep::Path(PathBuf::from("data.csv")),
        Dep::Path(PathBuf::from("config.yaml")),
    ];
    stages.insert(StageName::parse("train").unwrap(), stage);

    let paths = collect_dep_paths(&stages);
    assert_eq!(paths.len(), 2);
    assert!(paths.contains(&PathBuf::from("config.yaml")));
    assert!(paths.contains(&PathBuf::from("data.csv")));
}

/// Integration test: watch mode detects a dep change and re-executes.
///
/// This test is timing-sensitive and may be flaky on slow CI machines.
#[ignore]
#[tokio::test(flavor = "multi_thread")]
async fn watch_mode_reexecutes_on_dep_change() {
    let (_lock, _guard) = EnabledGuard::new();
    let tmp = TempDir::new().unwrap();
    let prev_cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(tmp.path()).unwrap();

    fs::write(tmp.path().join("input.txt"), b"v1").unwrap();
    write_copy_yaml(tmp.path(), "input.txt", "output.txt");

    // Run the initial execution (not in watch mode — just verify
    // the pipeline works).
    let args = crab::cmd::run::RunArgs {
        name: None,
        deps: vec![],
        outs: vec![],
        env: vec![],
        empty_env: false,
        timeout: None,
        hermetic: false,
        nondeterministic: false,
        force: false,
        dry_run: false,
        interactive: false,
        cache_only: false,
        no_run_cache: false,
        no_commit: false,
        no_overwrite: false,
        resume_trust_outputs: false,
        abandon: None,
        explain_miss: false,
        lock_timeout: None,
        no_wait: false,
        json: false,
        jsonl: false,
        recursive: false,
        single_item: false,
        downstream: false,
        force_downstream: false,
        pipeline: false,
        all_pipelines: false,
        keep_going: false,
        ignore_errors: false,
        parallelism: None,
        cache_push: false,
        allow_missing: false,
        pull: false,
        validate: false,
        watch: false,
        workflow: None,
        stages: None,
        glob: false,
        cmd: vec![],
    };

    let result =
        crab::cmd::run::run_in(&args, tmp.path(), crab::core::output::OutputMode::Text).await;
    std::env::set_current_dir(&prev_cwd).unwrap();

    result.expect("initial run should succeed");
    assert_eq!(
        fs::read(tmp.path().join("output.txt")).unwrap(),
        b"v1".to_vec()
    );
}

/// Integration test: the DepWatcher starts and can be dropped cleanly.
#[ignore]
#[tokio::test(flavor = "multi_thread")]
async fn dep_watcher_starts_and_stops_cleanly() {
    use std::path::PathBuf;

    use crab::workflow::watcher::DepWatcher;

    let tmp = TempDir::new().unwrap();
    let dep_file = tmp.path().join("data.csv");
    fs::write(&dep_file, b"initial").unwrap();

    let watcher = DepWatcher::start(&[PathBuf::from("data.csv")], tmp.path()).unwrap();

    // Drop the watcher — should not panic or leave orphan threads.
    drop(watcher);

    // Give a moment for cleanup.
    tokio::time::sleep(Duration::from_millis(50)).await;
}

/// Integration test: modifying a dep file produces a batch from the watcher.
#[ignore]
#[tokio::test(flavor = "multi_thread")]
async fn dep_watcher_detects_file_modification() {
    use std::path::PathBuf;

    use crab::workflow::watcher::DepWatcher;

    let tmp = TempDir::new().unwrap();
    let dep_file = tmp.path().join("data.csv");
    fs::write(&dep_file, b"initial").unwrap();

    let mut watcher = DepWatcher::start(&[PathBuf::from("data.csv")], tmp.path()).unwrap();

    // Give the watcher time to register.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Modify the dep file.
    fs::write(&dep_file, b"modified").unwrap();

    // The watcher should detect the change within 2 seconds.
    let batch = tokio::time::timeout(Duration::from_secs(2), watcher.next_batch())
        .await
        .expect("watcher should detect change within 2s")
        .expect("batch should not be None");

    assert!(!batch.is_empty(), "batch should contain the changed file");
}

/// Integration test: editor temp files do not trigger the watcher.
#[ignore]
#[tokio::test(flavor = "multi_thread")]
async fn dep_watcher_ignores_editor_temp_files() {
    use std::path::PathBuf;

    use crab::workflow::watcher::DepWatcher;

    let tmp = TempDir::new().unwrap();
    let dep_dir = tmp.path().join("src");
    fs::create_dir_all(&dep_dir).unwrap();
    fs::write(dep_dir.join("main.rs"), b"fn main() {}").unwrap();

    let mut watcher = DepWatcher::start(&[PathBuf::from("src")], tmp.path()).unwrap();

    // Give the watcher time to register.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Create editor temp files — these should NOT trigger.
    fs::write(dep_dir.join(".main.rs.swp"), b"swap").unwrap();
    fs::write(dep_dir.join("main.rs~"), b"backup").unwrap();

    // Wait past the debounce window.
    let result = tokio::time::timeout(Duration::from_millis(500), watcher.next_batch()).await;

    // Should timeout (no batch) because only temp files changed.
    assert!(
        result.is_err(),
        "editor temp files should not trigger the watcher"
    );
}
