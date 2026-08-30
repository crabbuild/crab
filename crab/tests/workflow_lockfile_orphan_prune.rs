//! Integration test for stale-lockfile-entry cleanup (R5).
//!
//! Scenario:
//! 1. Scaffold a repo with a two-stage `crab.yaml` (`stage_a`,
//!    `stage_b`) and run `crab run` — the lockfile records both
//!    stages.
//! 2. Remove `stage_b` from `crab.yaml` and run `crab run` again.
//! 3. The second run succeeds (orphan pruning is a `warn!`, never an
//!    error) and `crab.lock` now contains `stage_a` only.
//!
//! Mirrors the `prune_and_save_lockfile` path in `cmd::run` and the
//! `Lockfile::prune_stages_not_in` helper in `workflow::lockfile`.

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

/// Scaffold a repo with a two-stage pipeline:
/// - `stage_a` copies `a.txt → b.txt`.
/// - `stage_b` copies `b.txt → c.txt` (consumes stage_a's out).
///
/// The dep chain is incidental — the test only needs both stages to
/// land in the lockfile on the first run.
fn scaffold_two_stage_repo(root: &Path) {
    let yaml = "stages:\n  \
            stage_a:\n    cmd: \"/bin/cp a.txt b.txt\"\n    deps:\n      - a.txt\n    outs:\n      - b.txt\n  \
            stage_b:\n    cmd: \"/bin/cp b.txt c.txt\"\n    deps:\n      - b.txt\n    outs:\n      - c.txt\n";
    fs::write(root.join("crab.yaml"), yaml).unwrap();
    fs::write(root.join("a.txt"), b"payload").unwrap();

    fs::create_dir_all(root.join(".crab")).unwrap();
    fs::write(
        root.join(".crab").join("local.toml"),
        "[workflow]\nenabled = true\n",
    )
    .unwrap();
}

/// Overwrite `crab.yaml` with a single-stage workflow, dropping
/// `stage_b` so the next `crab run` prunes its lockfile entry.
fn drop_stage_b(root: &Path) {
    let yaml = "stages:\n  \
            stage_a:\n    cmd: \"/bin/cp a.txt b.txt\"\n    deps:\n      - a.txt\n    outs:\n      - b.txt\n";
    fs::write(root.join("crab.yaml"), yaml).unwrap();
}

/// Run `crab run` in DAG mode and return the full `Output`. The
/// caller decides whether to assert on stdout, stderr, or status.
fn run_dag(repo: &Path) -> std::process::Output {
    Command::new(bin())
        .current_dir(repo)
        .env("CRAB_WORKFLOW_ENABLED", "1")
        .args(["run"])
        .output()
        .expect("crab run should spawn")
}

/// Read `crab.lock` as a UTF-8 string. The file is written
/// canonically by the workflow layer, so a substring match on the
/// stage name key is a stable way to check presence in the integration
/// boundary.
fn read_lockfile(repo: &Path) -> String {
    fs::read_to_string(repo.join("crab.lock"))
        .expect("crab.lock should exist after a successful run")
}

/// Canonical YAML indents stage-name keys by two spaces under
/// `stages:`. Matching the full line prefix avoids false positives
/// on paths or cmd strings that happen to contain the stage name.
fn lockfile_contains_stage(lockfile: &str, name: &str) -> bool {
    let needle = format!("\n  {name}:\n");
    lockfile.contains(&needle)
}

/// End-to-end: remove a stage from `crab.yaml`, re-run, observe
/// the lockfile entry is gone and the remaining stage survives.
#[test]
fn removing_stage_from_yaml_prunes_its_lockfile_entry() {
    let tmp = TempDir::new().unwrap();
    scaffold_two_stage_repo(tmp.path());

    // First run populates the lockfile with both stages.
    let first = run_dag(tmp.path());
    assert!(
        first.status.success(),
        "first run exited {}: stderr={}",
        first.status,
        String::from_utf8_lossy(&first.stderr),
    );
    let lockfile_before = read_lockfile(tmp.path());
    assert!(
        lockfile_contains_stage(&lockfile_before, "stage_a"),
        "stage_a missing from lockfile after first run:\n{lockfile_before}",
    );
    assert!(
        lockfile_contains_stage(&lockfile_before, "stage_b"),
        "stage_b missing from lockfile after first run:\n{lockfile_before}",
    );

    // Drop stage_b from crab.yaml; the lockfile still carries its
    // orphan entry until the next run rewrites the file.
    drop_stage_b(tmp.path());

    let second = run_dag(tmp.path());
    assert!(
        second.status.success(),
        "second run exited {}: stderr={}",
        second.status,
        String::from_utf8_lossy(&second.stderr),
    );

    let lockfile_after = read_lockfile(tmp.path());
    assert!(
        lockfile_contains_stage(&lockfile_after, "stage_a"),
        "stage_a was unexpectedly pruned:\n{lockfile_after}",
    );
    assert!(
        !lockfile_contains_stage(&lockfile_after, "stage_b"),
        "stage_b entry should have been pruned from lockfile:\n{lockfile_after}",
    );
}
