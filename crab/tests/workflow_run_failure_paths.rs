//! Integration tests for `crab run` — failure paths.
//!
//! Covers:
//! - Non-zero exit from the user command (journal `Failed`, no
//!   cache entry, lockfile unchanged).
//! - SIGINT propagation from parent Crab to the child process
//!   (Unix only).
//! - `--timeout` → SIGTERM → SIGKILL escalation with a
//!   `StageExecTimeout` journal payload.
//! - Read-only parent directory surfacing as a clean stage failure
//!   on platforms where the user command can't write to the declared
//!   out path. The Linux-loopback disk-full path stays `#[ignore]`d
//!   until a CI fixture is in place.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use rusqlite::Connection;
use tempfile::TempDir;

/// Integer tag for `StageState::Failed`. Kept as a magic number so
/// this integration test doesn't reach into the crate's private
/// workflow module; the on-disk tag is stable per
/// `StageState::sql_tag` doc comments (Failed = 11, Aborted = 12).
const FAILED_STATE_TAG: i64 = 11;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_crab")
}

/// Read the final `(state, payload_json)` for every row in the
/// single run journal under `repo`. Assumes exactly one journal.
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

/// Non-zero exit: journal row settles in `Failed`, no cache entry
/// lands on disk. Lockfile unchanged (there is none — phase 1
/// doesn't produce `crab.lock` yet, so we verify its absence).
#[test]
fn nonzero_exit_leaves_failed_journal_and_no_cache_entry() {
    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join("a.txt"), b"x").unwrap();

    let status = Command::new(bin())
        .current_dir(tmp.path())
        .env("CRAB_WORKFLOW_ENABLED", "1")
        .args([
            "run", "--name", "fail", "--deps", "a.txt", "--outs", "b.txt", "--", "/bin/sh", "-c",
            "exit 42",
        ])
        .status()
        .expect("crab run should spawn");
    assert!(
        !status.success(),
        "crab must fail when user cmd exits non-zero"
    );

    let rows = single_journal_rows(tmp.path());
    assert_eq!(rows.len(), 1, "exactly one stage row");
    let (stage, state, payload) = &rows[0];
    assert_eq!(stage, "fail");
    assert_eq!(
        *state, FAILED_STATE_TAG,
        "expected Failed state (tag {FAILED_STATE_TAG}), got {state}; payload={payload}"
    );
    // Failure payload carries the exit code classification.
    assert!(
        payload.contains("exit_nonzero"),
        "expected exit_nonzero in payload, got {payload}"
    );

    // No stage cache entry created.
    let stages_dir = tmp.path().join(".crab/cache/stages");
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

    // Phase-1 lockfile is not yet written by `crab run`, so its
    // absence is the expected invariant.
    assert!(
        !tmp.path().join("crab.lock").exists(),
        "lockfile must not be created on failure"
    );
}

/// Timeout escalation: `--timeout 1s` with `/bin/sleep` → supervisor
/// sends SIGTERM after 1s, escalates to SIGKILL after
/// `graceful_shutdown_timeout`. Journal records `StageExecTimeout`
/// in the failure payload.
///
/// Child is invoked via `Cmd::Argv` so there is no intermediate
/// shell. A `/bin/sh -c "..."` indirection spawns an orphan
/// `sleep` that keeps the pipes open after `sh` is killed — the
/// supervisor's stdout pump then blocks on EOF for the full child
/// lifetime, masking the escalation. Direct argv avoids that.
#[test]
fn timeout_escalates_and_journal_records_timed_out() {
    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join("a.txt"), b"x").unwrap();

    let start = Instant::now();
    let status = Command::new(bin())
        .current_dir(tmp.path())
        .env("CRAB_WORKFLOW_ENABLED", "1")
        // Drive the graceful-shutdown window down to 1s so the test
        // completes quickly — default is 10s, which is pointless
        // latency for a test that just wants to see the escalation
        // fire.
        .env("CRAB_WORKFLOW_GRACEFUL_SHUTDOWN_TIMEOUT_SECS", "1")
        .args([
            "run",
            "--name",
            "slow",
            "--deps",
            "a.txt",
            "--outs",
            "b.txt",
            "--timeout",
            "1s",
            "--",
            "/bin/sleep",
            "60",
        ])
        .status()
        .expect("crab run should spawn");

    let elapsed = start.elapsed();
    assert!(
        !status.success(),
        "timeout run must fail; elapsed={elapsed:?}"
    );
    // Must complete in well under the child's 60-second sleep — the
    // timeout (1s) + graceful window (1s) should land the SIGKILL
    // well within 15 seconds per R18 design bounds.
    assert!(
        elapsed < Duration::from_secs(15),
        "timeout escalation should fire fast; took {elapsed:?}"
    );

    let rows = single_journal_rows(tmp.path());
    let (stage, state, payload) = &rows[0];
    assert_eq!(stage, "slow");
    assert_eq!(
        *state, FAILED_STATE_TAG,
        "expected Failed; payload={payload}"
    );
    assert!(
        payload.contains("timeout") && payload.contains("\"timed_out\":true"),
        "expected timed_out:true in payload, got {payload}"
    );
}

/// SIGINT propagation: spawn a long-sleeping child, send SIGINT to
/// the crab process, expect crab to forward the signal to the
/// child and reap it cleanly. The journal row lands in `Failed`
/// with a signal payload.
///
/// R18 asks for an `Aborted` outcome on SIGINT; the current phase-1
/// wiring in `cmd/run.rs` marks every error-path run as `Failure`
/// (and every stage as `Failed`). That refinement is phase-3 work;
/// what phase 1 guarantees, and what this test asserts, is clean
/// signal forwarding to the child and a well-formed Failed
/// transition on the stage row.
#[cfg(unix)]
#[test]
fn sigint_is_forwarded_to_child_and_run_terminates() {
    use std::os::unix::process::ExitStatusExt;

    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join("a.txt"), b"x").unwrap();

    let mut child = Command::new(bin())
        .current_dir(tmp.path())
        .env("CRAB_WORKFLOW_ENABLED", "1")
        // Graceful-shutdown window small enough to keep the test
        // snappy if SIGINT relay somehow fails to reap the child.
        .env("CRAB_WORKFLOW_GRACEFUL_SHUTDOWN_TIMEOUT_SECS", "1")
        .args([
            "run", "--name", "sleeper", "--deps", "a.txt", "--outs", "b.txt", "--", "/bin/sh",
            "-c", "sleep 30",
        ])
        .spawn()
        .expect("crab run spawn");

    // Wait for the supervisor to start the child. The per-stage log
    // file is the cleanest signal: the supervisor opens it
    // immediately after spawning the child, and a cache-hit path
    // never creates it.
    let runs_dir = tmp.path().join(".crab/workflow/runs");
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if find_stage_log(&runs_dir, "sleeper").is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        find_stage_log(&runs_dir, "sleeper").is_some(),
        "supervisor did not start within 10s",
    );

    // Send SIGINT to the crab process.
    let pid = nix::unistd::Pid::from_raw(child.id() as i32);
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGINT)
        .expect("kill(SIGINT) should succeed");

    // Crab should exit within the graceful-shutdown window + a
    // modest buffer. Use wait() with a soft deadline via a loop
    // rather than blocking forever — if signal relay is broken we
    // want a clean test failure, not a hang.
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => {
                if Instant::now() > deadline {
                    let _ = child.kill();
                    panic!("crab did not exit within 10s of SIGINT");
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => panic!("try_wait failed: {e}"),
        }
    };

    assert!(
        !status.success(),
        "crab must exit non-zero after SIGINT; got {status:?} (signal={:?})",
        status.signal()
    );

    // Journal: the stage row should be Failed with a signal-related
    // payload. Resolve the race where the journal write may still
    // be in flight when the process exits by polling briefly.
    let deadline = Instant::now() + Duration::from_secs(5);
    let rows = loop {
        let rows = single_journal_rows(tmp.path());
        if rows.first().map(|(_, state, _)| *state) == Some(FAILED_STATE_TAG) {
            break rows;
        }
        if Instant::now() > deadline {
            break rows;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let (stage, state, payload) = &rows[0];
    assert_eq!(stage, "sleeper");
    assert_eq!(
        *state, FAILED_STATE_TAG,
        "expected Failed after SIGINT; payload={payload}"
    );
    assert!(
        payload.contains("signal"),
        "expected 'signal' reason in payload, got {payload}"
    );
}

/// Locate `stage-<name>.log` under any subdirectory of
/// `.crab/workflow/runs/`. Returns the first match.
fn find_stage_log(runs_dir: &Path, stage_name: &str) -> Option<std::path::PathBuf> {
    let entries = fs::read_dir(runs_dir).ok()?;
    let target = format!("stage-{stage_name}.log");
    for entry in entries.flatten() {
        let candidate = entry.path().join(&target);
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// Read-only parent directory: the declared out sits under a parent
/// directory with mode `0o555`, so the user command can't create it
/// and Crab surfaces a clean failure. The on-disk invariant is the
/// important part — the stage row must land in `Failed`, and no
/// cache entry must be written. The specific error variant depends
/// on whether the failure fires during `/bin/sh -c 'echo > out'`
/// (surfaces as `StageExecFailed`, classified `exit_nonzero`) or
/// during the out-verification step that follows a successful
/// command (surfaces as `StageOutMalformed`, classified
/// `out_malformed`). Both are clean terminal failures — assert the
/// shape, not the specific reason.
///
/// This is the portable variant of the disk-full test. The Linux
/// loopback-FS variant that actually exercises ENOSPC / EDQUOT via
/// `map_fs_err` stays `#[ignore]`d until CI carries a fixture.
#[cfg(unix)]
#[test]
fn readonly_parent_dir_surfaces_io_error() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join("a.txt"), b"x").unwrap();

    // Declared out lives inside a parent directory we can read +
    // traverse but not write to. Create the directory first, then
    // drop write permission — reversing the order would deny us
    // access to seed the dir itself on strict umasks.
    let locked = tmp.path().join("locked");
    fs::create_dir(&locked).unwrap();
    let mut perms = fs::metadata(&locked).unwrap().permissions();
    perms.set_mode(0o555);
    fs::set_permissions(&locked, perms).unwrap();

    // Restore write permissions on teardown so `TempDir`'s
    // recursive-delete can clean up. Using a scope guard keeps the
    // cleanup reliable even if an assertion panics.
    struct RestorePerms<'a>(&'a Path);
    impl<'a> Drop for RestorePerms<'a> {
        fn drop(&mut self) {
            if let Ok(meta) = fs::metadata(self.0) {
                let mut p = meta.permissions();
                p.set_mode(0o755);
                let _ = fs::set_permissions(self.0, p);
            }
        }
    }
    let _restore = RestorePerms(&locked);

    let status = Command::new(bin())
        .current_dir(tmp.path())
        .env("CRAB_WORKFLOW_ENABLED", "1")
        .args([
            "run",
            "--name",
            "readonly",
            "--deps",
            "a.txt",
            "--outs",
            "locked/out.txt",
            "--",
            "/bin/sh",
            "-c",
            "echo x > locked/out.txt",
        ])
        .status()
        .expect("crab run should spawn");
    assert!(
        !status.success(),
        "run must fail when the declared out cannot be written"
    );

    let rows = single_journal_rows(tmp.path());
    assert_eq!(rows.len(), 1, "exactly one stage row");
    let (stage, state, payload) = &rows[0];
    assert_eq!(stage, "readonly");
    assert_eq!(
        *state, FAILED_STATE_TAG,
        "expected Failed state (tag {FAILED_STATE_TAG}); payload={payload}"
    );

    // No stage cache entry on any failure path.
    let stages_dir = tmp.path().join(".crab/cache/stages");
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

// ---------------------------------------------------------------------------
// Deferred tasks: ignored stubs with pointers at the blockers.
// ---------------------------------------------------------------------------

/// Task 1.24 — disk-full coverage. Needs a Linux-only loopback FS or
/// a tmpfs quota; not portable to macOS dev machines. Defer until
/// the Linux CI lane adds a fixture for this.
#[test]
#[ignore = "requires Linux loopback FS or tmpfs quota fixture"]
fn disk_full_produces_stage_disk_full() {
    // Intentionally empty. See task 1.24.
}
