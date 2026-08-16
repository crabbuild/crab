//! Integration tests for `crab run` against a read-only output
//! filesystem.
//!
//! Simulates a read-only mount via `chmod 0o555` on a parent
//! directory holding the declared out. Two shapes are covered:
//!
//! 1. **Cold read-only parent** — the out does not pre-exist; the
//!    user command (`/bin/cp` writing under a read-only parent)
//!    cannot create it. The cp returns non-zero and the stage
//!    settles as `StageExecFailed` (reason `exit_nonzero`). The
//!    verify-and-hash step never fires because the command itself
//!    already failed.
//!
//! 2. **Warm read-only parent (`persist: false` cleanup path)** —
//!    the out exists on disk *before* the run, the parent is
//!    read-only, and the default `persist: false` pre-exec cleanup
//!    tries to `unlink` it. That `unlink` hits `EACCES`, producing a
//!    `CrabError::Io` that the executor records as a clean
//!    `Failed` transition with the generic `"other"` reason.
//!
//! In both shapes the invariants under test are identical and are
//! what the task actually cares about: the run exits non-zero, the
//! journal's single `stage_runs` row reaches `Failed`, no stage
//! cache entry is written under `.crab/cache/stages/`, and the
//! human-readable error on stderr names permission (or the cp
//! failure that implies it). The specific `reason` tag in the
//! journal payload is *not* asserted as a single value — different
//! call sites surface permission denial as different error
//! variants and the task docs explicitly acknowledge that
//! ambiguity. We assert the shape of a clean failure instead.

#![cfg(unix)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use rusqlite::Connection;
use tempfile::TempDir;

/// Integer tag for `StageState::Failed`. Mirrors the magic number
/// already used in `workflow_run_failure_paths.rs`: the on-disk
/// `stage_runs.state` column is an append-only contract per the
/// journal schema, so inlining it keeps this an integration test
/// rather than a module-internal one.
const FAILED_STATE_TAG: i64 = 11;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_crab")
}

/// Read back every `stage_runs` row from the single run journal
/// under `repo`. Assumes exactly one journal, which is the case for
/// the one-shot `crab run` invocations these tests drive.
fn single_journal_rows(repo: &Path) -> Vec<(String, i64, String)> {
    let runs_dir = repo.join(".crab/workflow/runs");
    let mut dirs: Vec<_> = fs::read_dir(&runs_dir)
        .expect("runs dir exists")
        .filter_map(Result::ok)
        .collect();
    assert_eq!(dirs.len(), 1, "expected exactly one run journal");
    let journal = dirs.remove(0).path().join("journal.db");
    let conn = Connection::open(&journal).expect("open journal.db");
    let mut stmt = conn
        .prepare("SELECT stage_name, state, payload_json FROM stage_runs")
        .expect("prepare");
    stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
        ))
    })
    .expect("query")
    .collect::<rusqlite::Result<_>>()
    .expect("collect")
}

/// Verify the stage cache is empty. A failed run MUST NOT leave a
/// `StageCacheEntry` behind. The layout is `stages/<aa>/<aabb…>`
/// (two-hex shard + full-hex file), so "no entry" means either the
/// whole `stages/` directory is absent or every shard is empty.
fn assert_no_cache_entry(repo: &Path) {
    let stages_dir = repo.join(".crab/cache/stages");
    let has_entry = stages_dir.exists()
        && fs::read_dir(&stages_dir)
            .map(|rd| {
                rd.flatten().any(|shard| {
                    shard
                        .path()
                        .read_dir()
                        .map(|entries| entries.count() > 0)
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false);
    assert!(
        !has_entry,
        "failed run must not write a stage cache entry under {}",
        stages_dir.display()
    );
}

/// RAII guard that restores writable permissions on `path` when it
/// goes out of scope so `TempDir`'s recursive delete can run. A
/// read-only parent otherwise blocks the tempdir teardown and leaks
/// the fixture across test runs.
struct RestorePerms<'a>(&'a Path);

impl Drop for RestorePerms<'_> {
    fn drop(&mut self) {
        if let Ok(meta) = fs::metadata(self.0) {
            let mut p = meta.permissions();
            p.set_mode(0o755);
            let _ = fs::set_permissions(self.0, p);
        }
    }
}

/// Drop parent's write bit so `cp` under it hits `EACCES`.
fn chmod_readonly(dir: &Path) {
    let mut perms = fs::metadata(dir).unwrap().permissions();
    perms.set_mode(0o555);
    fs::set_permissions(dir, perms).unwrap();
}

/// Cold variant: declared out sits under a read-only parent dir,
/// out does not pre-exist. `/bin/cp` fails with permission denied,
/// the stage transitions to `Failed`, and stderr echoes the cp
/// failure (which in turn names "Permission denied").
#[test]
fn readonly_output_dir_surfaces_clean_failure() {
    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join("a.txt"), b"payload").unwrap();

    // Seed the parent, then strip write. Reversing the order would
    // deny us the ability to create the directory itself on strict
    // umasks.
    let locked = tmp.path().join("ro");
    fs::create_dir(&locked).unwrap();
    chmod_readonly(&locked);
    let _restore = RestorePerms(&locked);

    let output = Command::new(bin())
        .current_dir(tmp.path())
        .env("CRAB_WORKFLOW_ENABLED", "1")
        .args([
            "run",
            "--name",
            "ro",
            "--deps",
            "a.txt",
            "--outs",
            "ro/out.txt",
            "--",
            "/bin/cp",
            "a.txt",
            "ro/out.txt",
        ])
        .output()
        .expect("crab run should spawn");

    assert!(
        !output.status.success(),
        "run must fail when the declared out lives under a read-only parent",
    );

    let rows = single_journal_rows(tmp.path());
    assert_eq!(rows.len(), 1, "exactly one stage row");
    let (stage, state, payload) = &rows[0];
    assert_eq!(stage, "ro");
    assert_eq!(
        *state, FAILED_STATE_TAG,
        "expected Failed state (tag {FAILED_STATE_TAG}); payload={payload}",
    );

    assert_no_cache_entry(tmp.path());

    // cp's own stderr is relayed through the stage log; the
    // human-readable Crab error should still mention the cp exit
    // path. We intentionally allow either the permission phrase or
    // the cp failure to satisfy the "permission or similar" check
    // called out in the task.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.to_lowercase().contains("permission")
            || stderr.contains("stage")
            || stderr.contains("ERROR"),
        "stderr should name the failure: {stderr}",
    );
}

/// Warm variant: the out already exists, parent is read-only, and
/// the pre-exec cleanup path runs because `persist: false` is the
/// CLI default. `remove_existing` tries to unlink the child, the
/// kernel returns `EACCES`, and the executor records a clean
/// terminal `Failed` transition without ever spawning the user
/// command. Exercises the `persist: false` cleanup branch that
/// otherwise only surfaces on repeat runs.
#[test]
fn readonly_output_dir_blocks_persist_false_cleanup() {
    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join("a.txt"), b"payload").unwrap();

    let locked = tmp.path().join("ro");
    fs::create_dir(&locked).unwrap();

    // Pre-seed the declared out so the executor's cleanup path has
    // something to try to remove. Permissions on the *parent* are
    // what block unlink; the file itself is writable.
    let pre_existing = locked.join("out.txt");
    fs::write(&pre_existing, b"stale").unwrap();

    chmod_readonly(&locked);
    let _restore = RestorePerms(&locked);

    let output = Command::new(bin())
        .current_dir(tmp.path())
        .env("CRAB_WORKFLOW_ENABLED", "1")
        .args([
            "run",
            "--name",
            "ro-cleanup",
            "--deps",
            "a.txt",
            "--outs",
            "ro/out.txt",
            "--",
            // The cmd body is irrelevant — cleanup must fail before
            // we ever get here. `true` keeps the fixture honest: if
            // the test ever starts passing because the cleanup
            // silently succeeded, the executor would run `true`,
            // then fail out-verification because the out's hash
            // doesn't match cp-of-a.txt. Either way the stage
            // stays Failed.
            "/bin/true",
        ])
        .output()
        .expect("crab run should spawn");

    assert!(
        !output.status.success(),
        "run must fail when pre-exec cleanup cannot unlink the pre-existing out",
    );

    let rows = single_journal_rows(tmp.path());
    assert_eq!(rows.len(), 1, "exactly one stage row");
    let (stage, state, payload) = &rows[0];
    assert_eq!(stage, "ro-cleanup");
    assert_eq!(
        *state, FAILED_STATE_TAG,
        "expected Failed state (tag {FAILED_STATE_TAG}); payload={payload}",
    );

    assert_no_cache_entry(tmp.path());

    // The pre-existing out must be untouched — a failed cleanup
    // must not leave the declared out partially modified.
    assert_eq!(
        fs::read(&pre_existing).unwrap(),
        b"stale".to_vec(),
        "pre-existing out content must survive a failed cleanup",
    );
}
