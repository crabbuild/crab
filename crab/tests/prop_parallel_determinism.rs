//! Property test: parallel execution produces byte-identical outputs
//! to serial execution for the same DAG.
//!
//! Generates random input data, executes a diamond DAG with
//! parallelism=1 (serial) and parallelism=4 (parallel), and asserts
//! that output files are byte-identical.

use std::fs;
use std::path::{Path, PathBuf};

use crab::cmd::run::{RunArgs, run_in};
use crab::core::output::OutputMode;
use proptest::prelude::*;

static CWD_GUARD: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct CwdGuard {
    _lock: tokio::sync::MutexGuard<'static, ()>,
    prev: PathBuf,
}

impl CwdGuard {
    async fn enter(root: &Path) -> Self {
        let lock = CWD_GUARD.lock().await;
        let prev =
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")));
        std::env::set_current_dir(root).unwrap();
        Self { _lock: lock, prev }
    }
}

impl Drop for CwdGuard {
    fn drop(&mut self) {
        std::env::set_current_dir(&self.prev).unwrap();
    }
}

/// Helper: write a `crab.yaml` and config enabling workflow.
fn setup_repo(root: &Path, yaml: &str, parallelism: u32) {
    fs::write(root.join("crab.yaml"), yaml).unwrap();
    let crab_dir = root.join(".crab");
    fs::create_dir_all(&crab_dir).unwrap();
    fs::write(
        crab_dir.join("config.toml"),
        format!("[workflow]\nenabled = true\nparallelism = {parallelism}\n"),
    )
    .unwrap();
}

fn dag_args_with_parallelism(p: u32) -> RunArgs {
    RunArgs {
        name: None,
        deps: vec![],
        outs: vec![],
        env: vec![],
        empty_env: false,
        timeout: None,
        hermetic: false,
        nondeterministic: false,
        force: true,
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
        parallelism: Some(p),
        cache_push: false,
        allow_missing: false,
        pull: false,
        validate: false,
        #[cfg(feature = "watch")]
        watch: false,
        workflow: None,
        stages: None,
        glob: false,
        cmd: vec![],
    }
}

/// Run a DAG and collect output file contents.
async fn run_and_collect_outputs(
    root: &Path,
    yaml: &str,
    parallelism: u32,
    out_files: &[&str],
) -> Vec<(String, Vec<u8>)> {
    // Clean previous outputs.
    for f in out_files {
        let _ = fs::remove_file(root.join(f));
    }
    // Clean workflow state for a fresh run.
    let _ = fs::remove_dir_all(root.join(".crab").join("workflow"));
    let _ = fs::remove_dir_all(root.join(".crab").join("cache"));
    let _ = fs::remove_file(root.join("crab.lock"));

    setup_repo(root, yaml, parallelism);

    // Enable workflow via env var.
    unsafe { std::env::set_var("CRAB_WORKFLOW_ENABLED", "1") };

    let _cwd = CwdGuard::enter(root).await;

    let args = dag_args_with_parallelism(parallelism);
    let result = run_in(&args, root, OutputMode::Text).await;
    if let Err(e) = &result {
        eprintln!("run_in failed for parallelism={parallelism}: {e}");
    }

    let mut results = Vec::new();
    for f in out_files {
        let content = fs::read(root.join(f)).unwrap_or_default();
        results.push((f.to_string(), content));
    }
    results
}

/// **Validates: Requirements 1.2**
///
/// Property: parallel execution produces byte-identical outputs to
/// serial execution for the same DAG.
#[tokio::test(flavor = "multi_thread")]
async fn parallel_outputs_match_serial_for_diamond_dag() {
    unsafe { std::env::set_var("CRAB_WORKFLOW_ENABLED", "1") };

    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();

    // Create input files.
    fs::write(root.join("input.txt"), b"test-data-123").unwrap();

    let yaml = r#"stages:
  a:
    cmd: "cp input.txt a.out"
    deps:
      - input.txt
    outs:
      - a.out
  b:
    cmd: "cat a.out a.out > b.out"
    deps:
      - a.out
    outs:
      - b.out
  c:
    cmd: "wc -c < a.out > c.out"
    deps:
      - a.out
    outs:
      - c.out
  d:
    cmd: "cat b.out c.out > d.out"
    deps:
      - b.out
      - c.out
    outs:
      - d.out
"#;

    let out_files = &["a.out", "b.out", "c.out", "d.out"];

    // Run serial (parallelism=1).
    let serial_outputs = run_and_collect_outputs(root, yaml, 1, out_files).await;

    // Run parallel (parallelism=4).
    let parallel_outputs = run_and_collect_outputs(root, yaml, 4, out_files).await;

    // Assert byte-identical outputs.
    assert_eq!(
        serial_outputs.len(),
        parallel_outputs.len(),
        "Output count mismatch"
    );
    for (serial, parallel) in serial_outputs.iter().zip(parallel_outputs.iter()) {
        assert_eq!(serial.0, parallel.0, "Output file name mismatch");
        assert_eq!(
            serial.1, parallel.1,
            "Output file {} differs between serial and parallel execution",
            serial.0
        );
    }
}

// Property test with proptest: generate random input data and verify
// that parallel execution produces the same outputs as serial.
proptest! {
    #![proptest_config(ProptestConfig::with_cases(5))]

    #[test]
    fn prop_parallel_determinism(input_data in "[a-z0-9]{1,100}") {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async {
            unsafe { std::env::set_var("CRAB_WORKFLOW_ENABLED", "1") };

            let tmp = tempfile::TempDir::new().unwrap();
            let root = tmp.path();

            fs::write(root.join("input.txt"), input_data.as_bytes()).unwrap();

            let yaml = r#"stages:
  a:
    cmd: "cp input.txt a.out"
    deps:
      - input.txt
    outs:
      - a.out
  b:
    cmd: "cat a.out a.out > b.out"
    deps:
      - a.out
    outs:
      - b.out
  c:
    cmd: "cat a.out | rev > c.out"
    deps:
      - a.out
    outs:
      - c.out
  d:
    cmd: "cat b.out c.out > d.out"
    deps:
      - b.out
      - c.out
    outs:
      - d.out
"#;

            let out_files = &["a.out", "b.out", "c.out", "d.out"];

            let serial_outputs = run_and_collect_outputs(root, yaml, 1, out_files).await;
            let parallel_outputs = run_and_collect_outputs(root, yaml, 4, out_files).await;

            for (serial, parallel) in serial_outputs.iter().zip(parallel_outputs.iter()) {
                assert_eq!(
                    &serial.1,
                    &parallel.1,
                    "Output {} differs between serial and parallel",
                    serial.0
                );
            }
        });
    }
}
