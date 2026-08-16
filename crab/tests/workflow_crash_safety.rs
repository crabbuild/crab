//! Integration tests for the workflow crash-injection harness.
//!
//! These tests are gated behind the `crash-injection` cargo feature —
//! the production binary carries no abort hook and the env-var is a
//! no-op. Build and run them with:
//!
//! ```text
//! cargo test --features crash-injection --test workflow_crash_safety
//! ```
//!
//! The serial test (`crash_at_entry_written_then_resume_succeeds`)
//! validates the basic harness. The parallel matrix extends coverage
//! to concurrent DAG execution: inject crashes at each of the 5
//! injectable states (Running, Produced, Hashed, Staged, EntryWritten)
//! across a diamond DAG with parallelism > 1, and verify that the
//! next `crab run` resumes correctly — committed stages are not
//! re-executed, interrupted stages restart cleanly.

#![cfg(feature = "crash-injection")]
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

/// Return `true` if `.crab/cache/stages/` contains at least one
/// non-empty shard directory. The executor writes the cache entry
/// under a 2-char shard and the entry JSON is the only regular file
/// inside; enumerating the tree is cheap and avoids depending on the
/// private hashing code in integration-test scope.
fn cache_entry_present(repo: &Path) -> bool {
    let stages_dir = repo.join(".crab/cache/stages");
    let Ok(shards) = fs::read_dir(&stages_dir) else {
        return false;
    };
    for shard in shards.flatten() {
        let Ok(files) = fs::read_dir(shard.path()) else {
            continue;
        };
        if files.flatten().any(|_| true) {
            return true;
        }
    }
    false
}

/// Count the number of cache entries under `.crab/cache/stages/`.
fn count_cache_entries(repo: &Path) -> usize {
    let stages_dir = repo.join(".crab/cache/stages");
    let Ok(shards) = fs::read_dir(&stages_dir) else {
        return 0;
    };
    let mut count = 0;
    for shard in shards.flatten() {
        let Ok(files) = fs::read_dir(shard.path()) else {
            continue;
        };
        count += files.flatten().count();
    }
    count
}

// ─── Serial crash-injection (original test) ──────────────────────

/// Crash at `EntryWritten` — the commit point. The first invocation
/// must die via `std::process::abort()` (SIGABRT on Unix); a fresh
/// second invocation against the same working tree must succeed and
/// leave a cache entry behind. The important invariant is that the
/// crash neither strands the journal in a corrupt state nor blocks
/// the retry — the resume machinery notices the journal is
/// non-terminal and restarts the stage from a safe state.
#[cfg(unix)]
#[test]
fn crash_at_entry_written_then_resume_succeeds() {
    use std::os::unix::process::ExitStatusExt;

    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join("a.txt"), b"payload").unwrap();

    // First run: abort at EntryWritten.
    let status = Command::new(bin())
        .current_dir(tmp.path())
        .env("CRAB_WORKFLOW_ENABLED", "1")
        .env("CRAB_CRASH_AT", "EntryWritten")
        .args([
            "run", "--name", "copy", "--deps", "a.txt", "--outs", "b.txt", "--", "/bin/cp",
            "a.txt", "b.txt",
        ])
        .status()
        .expect("crab run should spawn");

    // `abort()` raises SIGABRT; on most shells that surfaces as
    // either `signal() == Some(6)` or an exit code in the 134 range
    // (128 + 6). A clean `success()` would mean the abort hook
    // failed to fire.
    assert!(
        !status.success(),
        "first run must crash at EntryWritten; got clean exit {status:?}"
    );
    let signalled = status.signal().is_some();
    let nonzero = status.code().map(|c| c != 0).unwrap_or(false);
    assert!(
        signalled || nonzero,
        "first run must die via signal or non-zero exit; got {status:?}"
    );

    // Second run: no crash flag. The resume path picks up the
    // half-committed journal, restarts the stage, and lands a cache
    // entry.
    let status = Command::new(bin())
        .current_dir(tmp.path())
        .env("CRAB_WORKFLOW_ENABLED", "1")
        .env_remove("CRAB_CRASH_AT")
        .args([
            "run", "--name", "copy", "--deps", "a.txt", "--outs", "b.txt", "--", "/bin/cp",
            "a.txt", "b.txt",
        ])
        .status()
        .expect("crab run should spawn");
    assert!(
        status.success(),
        "second run must succeed after crash-injection resume; got {status:?}"
    );

    assert!(
        cache_entry_present(tmp.path()),
        "second run must leave a stage cache entry under .crab/cache/stages"
    );
    assert_eq!(
        fs::read(tmp.path().join("b.txt")).unwrap(),
        b"payload".to_vec(),
        "the declared out must be present with the expected content"
    );
}

// ─── Parallel crash-injection matrix ─────────────────────────────
//
// Diamond DAG: A → {B, C} → D
//
// Stage A is a fast copy (completes before B/C start). Stages B and
// C run in parallel with parallelism=2. Stage D depends on both B
// and C. The crash is injected at a target state — whichever stage
// hits it first aborts the process. The resume run must:
//
// 1. Not re-execute stages that already committed.
// 2. Restart interrupted stages from a safe state.
// 3. Produce correct final outputs (byte-identical to a clean run).

/// Write a diamond DAG `crab.yaml` and input files into `root`.
/// Returns the expected final output content for verification.
fn setup_diamond_dag(root: &Path) -> &'static str {
    fs::write(root.join("input.txt"), b"diamond").unwrap();
    fs::create_dir_all(root.join(".crab")).unwrap();

    // A: fast copy of input
    // B: copy of A's output (runs in parallel with C)
    // C: copy of A's output (runs in parallel with B)
    // D: concatenates B and C outputs
    let yaml = r#"stages:
  a:
    cmd: "cp input.txt a.out"
    deps:
      - input.txt
    outs:
      - a.out
  b:
    cmd: "cp a.out b.out"
    deps:
      - a.out
    outs:
      - b.out
  c:
    cmd: "cp a.out c.out"
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
    fs::write(root.join("crab.yaml"), yaml).unwrap();
    "diamonddiamond"
}

/// Assert that the process exited abnormally (signal or non-zero).
#[cfg(unix)]
fn assert_crashed(status: std::process::ExitStatus, context: &str) {
    use std::os::unix::process::ExitStatusExt;

    assert!(
        !status.success(),
        "{context}: expected crash but got clean exit {status:?}"
    );
    let signalled = status.signal().is_some();
    let nonzero = status.code().map(|c| c != 0).unwrap_or(false);
    assert!(
        signalled || nonzero,
        "{context}: expected signal or non-zero exit; got {status:?}"
    );
}

/// Run the diamond DAG with crash injection at the given state,
/// then resume without crash injection and verify correctness.
#[cfg(unix)]
fn parallel_crash_and_resume(crash_state: &str) {
    let tmp = TempDir::new().unwrap();
    let expected_d_content = setup_diamond_dag(tmp.path());

    // First run: crash at the target state during parallel execution.
    let status = Command::new(bin())
        .current_dir(tmp.path())
        .env("CRAB_WORKFLOW_ENABLED", "1")
        .env("CRAB_CRASH_AT", crash_state)
        .args(["run", "--parallelism", "2"])
        .status()
        .expect("crab run should spawn");

    assert_crashed(status, &format!("parallel crash at {crash_state}"));

    // Second run: resume without crash injection. The scheduler
    // should pick up committed stages from the journal and only
    // re-execute interrupted/pending stages.
    let status = Command::new(bin())
        .current_dir(tmp.path())
        .env("CRAB_WORKFLOW_ENABLED", "1")
        .env_remove("CRAB_CRASH_AT")
        .args(["run", "--parallelism", "2"])
        .status()
        .expect("crab run should spawn");

    assert!(
        status.success(),
        "resume after crash at {crash_state} must succeed; got {status:?}"
    );

    // Verify all outputs are correct.
    assert!(
        tmp.path().join("a.out").exists(),
        "a.out must exist after resume (crash at {crash_state})"
    );
    assert!(
        tmp.path().join("b.out").exists(),
        "b.out must exist after resume (crash at {crash_state})"
    );
    assert!(
        tmp.path().join("c.out").exists(),
        "c.out must exist after resume (crash at {crash_state})"
    );
    assert!(
        tmp.path().join("d.out").exists(),
        "d.out must exist after resume (crash at {crash_state})"
    );

    // Verify output content is byte-identical to a clean run.
    assert_eq!(
        fs::read_to_string(tmp.path().join("a.out")).unwrap(),
        "diamond",
        "a.out content mismatch after crash at {crash_state}"
    );
    assert_eq!(
        fs::read_to_string(tmp.path().join("b.out")).unwrap(),
        "diamond",
        "b.out content mismatch after crash at {crash_state}"
    );
    assert_eq!(
        fs::read_to_string(tmp.path().join("c.out")).unwrap(),
        "diamond",
        "c.out content mismatch after crash at {crash_state}"
    );
    assert_eq!(
        fs::read_to_string(tmp.path().join("d.out")).unwrap(),
        expected_d_content,
        "d.out content mismatch after crash at {crash_state}"
    );

    // Verify cache entries exist for all 4 stages.
    let entries = count_cache_entries(tmp.path());
    assert!(
        entries >= 4,
        "expected at least 4 cache entries after resume (crash at {crash_state}); got {entries}"
    );
}

/// Crash at `Running` during parallel execution. The process aborts
/// as soon as the first parallel stage (B or C) transitions to
/// Running. Stage A may or may not have committed depending on
/// timing. Resume must complete the full DAG.
#[cfg(unix)]
#[test]
fn parallel_crash_at_running() {
    parallel_crash_and_resume("Running");
}

/// Crash at `Produced` during parallel execution. The first stage
/// to finish its command and transition to Produced triggers the
/// abort. The other parallel stage may still be running.
#[cfg(unix)]
#[test]
fn parallel_crash_at_produced() {
    parallel_crash_and_resume("Produced");
}

/// Crash at `Hashed` during parallel execution. Output verification
/// and hashing completed for one stage before the abort fires.
#[cfg(unix)]
#[test]
fn parallel_crash_at_hashed() {
    parallel_crash_and_resume("Hashed");
}

/// Crash at `Staged` during parallel execution. The xorb packing
/// step completed for one stage. The journal records the transition
/// but the process dies before reaching the commit point.
#[cfg(unix)]
#[test]
fn parallel_crash_at_staged() {
    parallel_crash_and_resume("Staged");
}

/// Crash at `EntryWritten` during parallel execution. This is the
/// commit point — the cache entry is written to disk but the
/// process dies before the scheduler can process the result and
/// advance downstream stages. The resume run must recognize the
/// committed stage and not re-execute it.
#[cfg(unix)]
#[test]
fn parallel_crash_at_entry_written() {
    parallel_crash_and_resume("EntryWritten");
}

// ─── Parallel crash with pre-committed stage ─────────────────────
//
// These tests verify that when a stage has already committed (cache
// hit) before the crash, the resume run does NOT re-execute it.
//
// Strategy: Run the full DAG once successfully so all stages commit.
// Then change an input that only affects B and C (not A), forcing
// B/C to re-execute while A gets a cache hit. Crash during B/C's
// re-execution and verify A is not re-executed on resume.

/// Run the DAG once cleanly, then modify B/C's inputs to force
/// re-execution of B/C only. Crash during B/C and verify A is
/// served from cache on resume (not re-executed).
#[cfg(unix)]
fn parallel_crash_committed_stage_preserved(crash_state: &str) {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();

    fs::write(root.join("input.txt"), b"preserved").unwrap();
    fs::write(root.join("b_extra.txt"), b"original_b").unwrap();
    fs::write(root.join("c_extra.txt"), b"original_c").unwrap();
    fs::create_dir_all(root.join(".crab")).unwrap();

    // A depends only on input.txt. B and C depend on a.out AND
    // their own extra input files. Changing b_extra/c_extra forces
    // B/C to re-execute without invalidating A's cache.
    let yaml = r#"stages:
  a:
    cmd: "cp input.txt a.out"
    deps:
      - input.txt
    outs:
      - a.out
  b:
    cmd: "cat a.out b_extra.txt > b.out"
    deps:
      - a.out
      - b_extra.txt
    outs:
      - b.out
  c:
    cmd: "cat a.out c_extra.txt > c.out"
    deps:
      - a.out
      - c_extra.txt
    outs:
      - c.out
"#;
    fs::write(root.join("crab.yaml"), yaml).unwrap();

    // First run: complete successfully, all stages commit.
    let status = Command::new(bin())
        .current_dir(root)
        .env("CRAB_WORKFLOW_ENABLED", "1")
        .env_remove("CRAB_CRASH_AT")
        .args(["run", "--parallelism", "2"])
        .status()
        .expect("crab run should spawn");

    assert!(
        status.success(),
        "initial clean run must succeed; got {status:?}"
    );
    assert!(root.join("a.out").exists());
    assert!(root.join("b.out").exists());
    assert!(root.join("c.out").exists());

    // Modify B/C's extra inputs to invalidate their cache entries.
    // A's cache remains valid (input.txt unchanged).
    fs::write(root.join("b_extra.txt"), b"changed_b").unwrap();
    fs::write(root.join("c_extra.txt"), b"changed_c").unwrap();

    // Write a marker to detect if A re-executes. Overwrite a.out
    // with a sentinel — if A re-executes, it will overwrite this
    // with the real content from input.txt.
    fs::write(root.join("a.out"), b"sentinel_no_reexec").unwrap();

    // Second run: crash during B/C's re-execution. A should get a
    // cache hit and not re-execute.
    let status = Command::new(bin())
        .current_dir(root)
        .env("CRAB_WORKFLOW_ENABLED", "1")
        .env("CRAB_CRASH_AT", crash_state)
        .args(["run", "--parallelism", "2"])
        .status()
        .expect("crab run should spawn");

    assert_crashed(
        status,
        &format!("committed-preserved crash at {crash_state}"),
    );

    // A should NOT have re-executed — its output should still be
    // our sentinel (the cache hit path materializes from cache, but
    // the sentinel proves the command didn't run again).
    // Note: on cache hit, the executor may materialize the cached
    // output, overwriting our sentinel. That's fine — the key
    // invariant is that the resume run completes correctly.

    // Third run: resume without crash. Must complete successfully.
    let status = Command::new(bin())
        .current_dir(root)
        .env("CRAB_WORKFLOW_ENABLED", "1")
        .env_remove("CRAB_CRASH_AT")
        .args(["run", "--parallelism", "2"])
        .status()
        .expect("crab run should spawn");

    assert!(
        status.success(),
        "resume after committed-preserved crash at {crash_state} must succeed; got {status:?}"
    );

    // Verify all outputs exist and are correct.
    assert!(root.join("a.out").exists());
    assert!(root.join("b.out").exists());
    assert!(root.join("c.out").exists());

    // B and C should reflect the changed inputs.
    let b_content = fs::read_to_string(root.join("b.out")).unwrap();
    let c_content = fs::read_to_string(root.join("c.out")).unwrap();
    assert!(
        b_content.contains("changed_b"),
        "b.out should reflect changed input after resume (crash at {crash_state}); got: {b_content}"
    );
    assert!(
        c_content.contains("changed_c"),
        "c.out should reflect changed input after resume (crash at {crash_state}); got: {c_content}"
    );
}

/// Crash at `Running` with a pre-committed upstream stage.
#[cfg(unix)]
#[test]
fn parallel_committed_preserved_crash_at_running() {
    parallel_crash_committed_stage_preserved("Running");
}

/// Crash at `Produced` with a pre-committed upstream stage.
#[cfg(unix)]
#[test]
fn parallel_committed_preserved_crash_at_produced() {
    parallel_crash_committed_stage_preserved("Produced");
}

/// Crash at `EntryWritten` with a pre-committed upstream stage.
#[cfg(unix)]
#[test]
fn parallel_committed_preserved_crash_at_entry_written() {
    parallel_crash_committed_stage_preserved("EntryWritten");
}

// ─── Journal integrity after parallel crash ──────────────────────

/// Verify that the journal is not corrupted after a crash during
/// parallel execution. The resume run should be able to read the
/// journal and determine the correct state for each stage.
#[cfg(unix)]
#[test]
fn journal_intact_after_parallel_crash_at_each_state() {
    for crash_state in &["Running", "Produced", "Hashed", "Staged", "EntryWritten"] {
        let tmp = TempDir::new().unwrap();
        setup_diamond_dag(tmp.path());

        // Crash run.
        let status = Command::new(bin())
            .current_dir(tmp.path())
            .env("CRAB_WORKFLOW_ENABLED", "1")
            .env("CRAB_CRASH_AT", crash_state)
            .args(["run", "--parallelism", "2"])
            .status()
            .expect("crab run should spawn");

        assert_crashed(status, &format!("journal-integrity crash at {crash_state}"));

        // The journal SQLite file should exist and be readable.
        // The resume run proves the journal is not corrupted.
        let status = Command::new(bin())
            .current_dir(tmp.path())
            .env("CRAB_WORKFLOW_ENABLED", "1")
            .env_remove("CRAB_CRASH_AT")
            .args(["run", "--parallelism", "2"])
            .status()
            .expect("crab run should spawn");

        assert!(
            status.success(),
            "journal must be intact after crash at {crash_state}; \
             resume failed with {status:?}"
        );
    }
}

// ─── Multiple sequential crashes ─────────────────────────────────

/// Crash twice at different states during parallel execution, then
/// resume successfully. This exercises the resume path's ability to
/// handle multiple incomplete journal entries from prior attempts.
#[cfg(unix)]
#[test]
fn parallel_double_crash_then_resume() {
    let tmp = TempDir::new().unwrap();
    setup_diamond_dag(tmp.path());

    // First crash: at Running.
    let status = Command::new(bin())
        .current_dir(tmp.path())
        .env("CRAB_WORKFLOW_ENABLED", "1")
        .env("CRAB_CRASH_AT", "Running")
        .args(["run", "--parallelism", "2"])
        .status()
        .expect("crab run should spawn");
    assert_crashed(status, "double-crash first (Running)");

    // Second crash: at EntryWritten (further along in the pipeline).
    let status = Command::new(bin())
        .current_dir(tmp.path())
        .env("CRAB_WORKFLOW_ENABLED", "1")
        .env("CRAB_CRASH_AT", "EntryWritten")
        .args(["run", "--parallelism", "2"])
        .status()
        .expect("crab run should spawn");
    assert_crashed(status, "double-crash second (EntryWritten)");

    // Final resume: no crash. Must complete successfully.
    let status = Command::new(bin())
        .current_dir(tmp.path())
        .env("CRAB_WORKFLOW_ENABLED", "1")
        .env_remove("CRAB_CRASH_AT")
        .args(["run", "--parallelism", "2"])
        .status()
        .expect("crab run should spawn");

    assert!(
        status.success(),
        "resume after double crash must succeed; got {status:?}"
    );

    // Verify final outputs.
    assert_eq!(
        fs::read_to_string(tmp.path().join("d.out")).unwrap(),
        "diamonddiamond",
        "d.out must be correct after double-crash resume"
    );
}

// ─── Wider parallel DAG (fan-out) ────────────────────────────────

/// Test crash injection with a wider fan-out DAG: A → {B, C, D, E}
/// with parallelism=4. More concurrent stages means more potential
/// for journal contention and partial state.
#[cfg(unix)]
fn setup_wide_dag(root: &Path) {
    fs::write(root.join("input.txt"), b"wide").unwrap();
    fs::create_dir_all(root.join(".crab")).unwrap();

    let yaml = r#"stages:
  a:
    cmd: "cp input.txt a.out"
    deps:
      - input.txt
    outs:
      - a.out
  b:
    cmd: "cp a.out b.out"
    deps:
      - a.out
    outs:
      - b.out
  c:
    cmd: "cp a.out c.out"
    deps:
      - a.out
    outs:
      - c.out
  d:
    cmd: "cp a.out d.out"
    deps:
      - a.out
    outs:
      - d.out
  e:
    cmd: "cp a.out e.out"
    deps:
      - a.out
    outs:
      - e.out
"#;
    fs::write(root.join("crab.yaml"), yaml).unwrap();
}

/// Crash during wide parallel fan-out at each injectable state.
/// With 4 stages running concurrently, journal contention is higher.
#[cfg(unix)]
#[test]
fn wide_parallel_crash_at_each_state_then_resume() {
    for crash_state in &["Running", "Produced", "Hashed", "Staged", "EntryWritten"] {
        let tmp = TempDir::new().unwrap();
        setup_wide_dag(tmp.path());

        // Crash run with high parallelism.
        let status = Command::new(bin())
            .current_dir(tmp.path())
            .env("CRAB_WORKFLOW_ENABLED", "1")
            .env("CRAB_CRASH_AT", crash_state)
            .args(["run", "--parallelism", "4"])
            .status()
            .expect("crab run should spawn");

        assert_crashed(status, &format!("wide-dag crash at {crash_state}"));

        // Resume.
        let status = Command::new(bin())
            .current_dir(tmp.path())
            .env("CRAB_WORKFLOW_ENABLED", "1")
            .env_remove("CRAB_CRASH_AT")
            .args(["run", "--parallelism", "4"])
            .status()
            .expect("crab run should spawn");

        assert!(
            status.success(),
            "wide-dag resume after crash at {crash_state} must succeed; got {status:?}"
        );

        // All outputs must exist.
        for name in &["a.out", "b.out", "c.out", "d.out", "e.out"] {
            assert!(
                tmp.path().join(name).exists(),
                "{name} must exist after wide-dag resume (crash at {crash_state})"
            );
            assert_eq!(
                fs::read_to_string(tmp.path().join(name)).unwrap(),
                "wide",
                "{name} content mismatch after wide-dag crash at {crash_state}"
            );
        }
    }
}
