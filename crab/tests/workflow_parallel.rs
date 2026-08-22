//! Integration tests for the parallel DAG scheduler.
//!
//! Validates that independent stages run concurrently, that
//! `parallelism = 1` produces serial behavior, and that JSONL
//! output from concurrent stages is well-formed.

use std::fs;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crab::cmd::run::{RunArgs, run_in};
use crab::core::output::OutputMode;

// Serialize tests that touch env vars.
static ENV_GUARD: Mutex<()> = Mutex::new(());

/// RAII guard that enables the workflow layer via env var.
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

/// Helper: write a `crab.yaml` and ensure .crab dir exists.
fn setup_repo(root: &Path, yaml: &str, parallelism: u32) {
    fs::write(root.join("crab.yaml"), yaml).unwrap();
    let crab_dir = root.join(".crab");
    fs::create_dir_all(&crab_dir).unwrap();
    // Set parallelism via env var since Config::resolve_local()
    // doesn't read from the test's temp directory.
    unsafe {
        std::env::set_var("CRAB_WORKFLOW_PARALLELISM", parallelism.to_string());
    }
}

/// Build RunArgs for DAG mode (no positional, no inline flags).
fn dag_args() -> RunArgs {
    RunArgs {
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
        #[cfg(feature = "watch")]
        watch: false,
        workflow: None,
        stages: None,
        glob: false,
        cmd: vec![],
    }
}

/// Diamond DAG: A → {B, C} → D
/// B and C each sleep for 500ms. With parallelism=2, total time
/// should be ~500ms (not ~1000ms).
#[tokio::test(flavor = "multi_thread")]
async fn diamond_dag_parallel_execution() {
    let (_lock, _guard) = EnabledGuard::new();
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();

    // Create input file for stage A.
    fs::write(root.join("input.txt"), b"hello").unwrap();

    let yaml = r#"stages:
  a:
    cmd: "cp input.txt a.out"
    deps:
      - input.txt
    outs:
      - a.out
  b:
    cmd: "sleep 0.5 && cp a.out b.out"
    deps:
      - a.out
    outs:
      - b.out
  c:
    cmd: "sleep 0.5 && cp a.out c.out"
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

    setup_repo(root, yaml, 2);

    let prev_cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(root).unwrap();

    let start = Instant::now();
    let mut args = dag_args();
    args.parallelism = Some(2);
    let result = run_in(&args, root, OutputMode::Text).await;
    let elapsed = start.elapsed();

    std::env::set_current_dir(&prev_cwd).unwrap();

    assert!(result.is_ok(), "DAG run failed: {:?}", result.err());

    // With parallelism=2, B and C run concurrently. Total time
    // should be significantly less than 1000ms (serial would be
    // ~1000ms for B+C). Allow some overhead.
    assert!(
        elapsed < Duration::from_millis(900),
        "Expected parallel execution (< 900ms), got {:?}",
        elapsed
    );

    // Verify outputs exist.
    assert!(root.join("a.out").exists());
    assert!(root.join("b.out").exists());
    assert!(root.join("c.out").exists());
    assert!(root.join("d.out").exists());

    // Verify D's output is the concatenation of B and C.
    let d_content = fs::read_to_string(root.join("d.out")).unwrap();
    assert_eq!(d_content, "hellohello");
}

/// With parallelism=1, stages execute serially (same as old behavior).
#[tokio::test(flavor = "multi_thread")]
async fn parallelism_one_is_serial() {
    let (_lock, _guard) = EnabledGuard::new();
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();

    fs::write(root.join("input.txt"), b"data").unwrap();

    let yaml = r#"stages:
  a:
    cmd: "cp input.txt a.out"
    deps:
      - input.txt
    outs:
      - a.out
  b:
    cmd: "sleep 0.3 && cp a.out b.out"
    deps:
      - a.out
    outs:
      - b.out
  c:
    cmd: "sleep 0.3 && cp a.out c.out"
    deps:
      - a.out
    outs:
      - c.out
"#;

    setup_repo(root, yaml, 1);

    let prev_cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(root).unwrap();

    let start = Instant::now();
    let mut args = dag_args();
    args.parallelism = Some(1);
    let result = run_in(&args, root, OutputMode::Text).await;
    let elapsed = start.elapsed();

    std::env::set_current_dir(&prev_cwd).unwrap();

    assert!(result.is_ok(), "DAG run failed: {:?}", result.err());

    // With parallelism=1, B and C run serially. Total time should
    // be >= 600ms (300ms + 300ms).
    assert!(
        elapsed >= Duration::from_millis(550),
        "Expected serial execution (>= 550ms), got {:?}",
        elapsed
    );

    assert!(root.join("b.out").exists());
    assert!(root.join("c.out").exists());
}

/// Keep-going mode: when B fails, C (independent) still completes,
/// and D (downstream of B) is not started.
#[tokio::test(flavor = "multi_thread")]
async fn parallel_keep_going() {
    let (_lock, _guard) = EnabledGuard::new();
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();

    fs::write(root.join("input.txt"), b"data").unwrap();
    fs::write(root.join("c-input.txt"), b"c-data").unwrap();

    let yaml = r#"stages:
  a:
    cmd: "cp input.txt a.out"
    deps:
      - input.txt
    outs:
      - a.out
  b:
    cmd:
      argv: ["/bin/sh", "-c", "exit 1"]
    deps:
      - a.out
    outs:
      - b.out
  c:
    cmd: "cp c-input.txt c.out"
    deps:
      - c-input.txt
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

    setup_repo(root, yaml, 2);

    let prev_cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(root).unwrap();

    let mut args = dag_args();
    args.keep_going = true;
    args.parallelism = Some(2);
    let result = run_in(&args, root, OutputMode::Text).await;

    std::env::set_current_dir(&prev_cwd).unwrap();

    // Overall run should fail (B failed).
    assert!(result.is_err());

    // C should have completed (independent branch).
    assert!(
        root.join("c.out").exists(),
        "Independent branch C should complete under --keep-going"
    );

    // D should NOT have run (downstream of failed B).
    assert!(
        !root.join("d.out").exists(),
        "Downstream of failed stage should not run"
    );
}

/// Two GPU stages with parallelism=4 but only 1 GPU: run serially.
/// Both stages declare `resources: { gpu: 1 }`. With only 1 GPU
/// available, they cannot overlap despite the parallelism cap being 4.
#[tokio::test(flavor = "multi_thread")]
async fn gpu_stages_run_serially_with_one_gpu() {
    let (_lock, _guard) = EnabledGuard::new();
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();

    fs::write(root.join("input.txt"), b"data").unwrap();

    let yaml = r#"stages:
  a:
    cmd: "cp input.txt a.out"
    deps:
      - input.txt
    outs:
      - a.out
  gpu_b:
    cmd: "sleep 0.4 && cp a.out gpu_b.out"
    deps:
      - a.out
    outs:
      - gpu_b.out
    resources:
      gpu: 1
  gpu_c:
    cmd: "sleep 0.4 && cp a.out gpu_c.out"
    deps:
      - a.out
    outs:
      - gpu_c.out
    resources:
      gpu: 1
"#;

    setup_repo(root, yaml, 4);

    let prev_cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(root).unwrap();

    // Set GPU count to 1 so only one GPU stage can run at a time.
    unsafe { std::env::set_var("CRAB_GPU_COUNT", "1") };

    let start = Instant::now();
    let mut args = dag_args();
    args.parallelism = Some(4);
    let result = run_in(&args, root, OutputMode::Text).await;
    let elapsed = start.elapsed();

    std::env::set_current_dir(&prev_cwd).unwrap();
    unsafe { std::env::remove_var("CRAB_GPU_COUNT") };

    assert!(result.is_ok(), "DAG run failed: {:?}", result.err());

    // With only 1 GPU, gpu_b and gpu_c must run serially.
    // Total time should be >= 800ms (400ms + 400ms).
    assert!(
        elapsed >= Duration::from_millis(700),
        "Expected serial GPU execution (>= 700ms), got {:?}",
        elapsed
    );

    assert!(root.join("gpu_b.out").exists());
    assert!(root.join("gpu_c.out").exists());
}

/// Stages without `resources:` default to `{ cpu: 1 }` and run
/// concurrently when parallelism allows.
#[tokio::test(flavor = "multi_thread")]
async fn stages_without_resources_default_to_cpu_one() {
    let (_lock, _guard) = EnabledGuard::new();
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();

    fs::write(root.join("input.txt"), b"data").unwrap();

    // Two stages with no resources declared — should default to
    // cpu: 1 and run concurrently with parallelism=4.
    let yaml = r#"stages:
  a:
    cmd: "cp input.txt a.out"
    deps:
      - input.txt
    outs:
      - a.out
  b:
    cmd: "printf started > b.started; while [ ! -f release ]; do sleep 0.01; done; cp a.out b.out"
    deps:
      - a.out
    outs:
      - b.out
  c:
    cmd: "printf started > c.started; while [ ! -f release ]; do sleep 0.01; done; cp a.out c.out"
    deps:
      - a.out
    outs:
      - c.out
"#;

    setup_repo(root, yaml, 4);

    let prev_cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(root).unwrap();

    let mut args = dag_args();
    args.parallelism = Some(4);

    let b_started = root.join("b.started");
    let c_started = root.join("c.started");
    let release = root.join("release");
    // A release barrier proves overlap without relying on a loaded runner's
    // wall-clock scheduling margin.
    let mut run = Box::pin(run_in(&args, root, OutputMode::Text));
    let mut marker_wait = Box::pin(async {
        loop {
            if b_started.is_file() && c_started.is_file() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    });
    let mut marker_timeout = Box::pin(tokio::time::sleep(Duration::from_secs(2)));
    let mut completed = None;
    let both_started = tokio::select! {
        result = &mut run => {
            completed = Some(result);
            false
        }
        () = &mut marker_wait => true,
        () = &mut marker_timeout => false,
    };
    fs::write(&release, b"release").unwrap();
    let result = match completed {
        Some(result) => result,
        None => run.await,
    };

    std::env::set_current_dir(&prev_cwd).unwrap();

    assert!(result.is_ok(), "DAG run failed: {:?}", result.err());

    assert!(
        both_started,
        "both default-resource stages must start before the release barrier"
    );

    assert!(root.join("b.out").exists());
    assert!(root.join("c.out").exists());
}

/// Resources exceeding machine capacity: warn! emitted, stage runs anyway.
#[tokio::test(flavor = "multi_thread")]
async fn resources_exceeding_capacity_still_runs() {
    let (_lock, _guard) = EnabledGuard::new();
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();

    fs::write(root.join("input.txt"), b"data").unwrap();

    // Stage declares gpu: 4 but machine has 0 GPUs — should warn
    // and run anyway.
    let yaml = r#"stages:
  greedy:
    cmd: "cp input.txt greedy.out"
    deps:
      - input.txt
    outs:
      - greedy.out
    resources:
      gpu: 4
      cpu: 1
"#;

    setup_repo(root, yaml, 4);

    let prev_cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(root).unwrap();

    // Ensure no GPUs are available.
    unsafe { std::env::set_var("CRAB_GPU_COUNT", "0") };

    let mut args = dag_args();
    args.parallelism = Some(4);
    let result = run_in(&args, root, OutputMode::Text).await;

    std::env::set_current_dir(&prev_cwd).unwrap();
    unsafe { std::env::remove_var("CRAB_GPU_COUNT") };

    // Stage should still succeed despite exceeding capacity.
    assert!(result.is_ok(), "DAG run failed: {:?}", result.err());
    assert!(root.join("greedy.out").exists());
}

/// Stage with `resources: { gpu: 1 }` waits for GPU slot before starting.
/// Verifies that a GPU stage only starts after a prior GPU stage completes.
#[tokio::test(flavor = "multi_thread")]
async fn gpu_stage_waits_for_slot() {
    let (_lock, _guard) = EnabledGuard::new();
    let tmp = tempfile::TempDir::new().unwrap();
    let root = tmp.path();

    fs::write(root.join("input.txt"), b"data").unwrap();

    // Linear chain: a → gpu_b → gpu_c. Both GPU stages need 1 GPU.
    // With 1 GPU, gpu_c must wait for gpu_b to finish.
    let yaml = r#"stages:
  a:
    cmd: "cp input.txt a.out"
    deps:
      - input.txt
    outs:
      - a.out
  gpu_b:
    cmd: "sleep 0.3 && cp a.out gpu_b.out"
    deps:
      - a.out
    outs:
      - gpu_b.out
    resources:
      gpu: 1
  gpu_c:
    cmd: "cp gpu_b.out gpu_c.out"
    deps:
      - gpu_b.out
    outs:
      - gpu_c.out
    resources:
      gpu: 1
"#;

    setup_repo(root, yaml, 4);

    let prev_cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(root).unwrap();

    unsafe { std::env::set_var("CRAB_GPU_COUNT", "1") };

    let mut args = dag_args();
    args.parallelism = Some(4);
    let result = run_in(&args, root, OutputMode::Text).await;

    std::env::set_current_dir(&prev_cwd).unwrap();
    unsafe { std::env::remove_var("CRAB_GPU_COUNT") };

    assert!(result.is_ok(), "DAG run failed: {:?}", result.err());
    assert!(root.join("gpu_b.out").exists());
    assert!(root.join("gpu_c.out").exists());

    // Verify output correctness — gpu_c should contain the same
    // data as gpu_b (which is a copy of a.out).
    let content = fs::read_to_string(root.join("gpu_c.out")).unwrap();
    assert_eq!(content, "data");
}
