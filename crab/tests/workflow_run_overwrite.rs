//! Integration tests for `crab run` — cache-hit overwrite policy (R12).
//!
//! Three scenarios:
//! - Cache hit with a matching file on disk → no-op (mtime unchanged).
//! - Cache hit with a mismatching file under `--no-overwrite` →
//!   `StageOverwriteConflict` (exit code 5).
//! - Cache hit with uncommitted git changes → currently deferred
//!   (the git-dirty probe in `cmd::run::inspect_existing` is
//!   stubbed to `false` for phase 1; re-enable once a real
//!   gitoxide check lands).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::SystemTime;

use tempfile::TempDir;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_crab")
}

/// Exit code for `StageOverwriteConflict`. Maps to the "I/O / lock
/// conflict" bucket (5) per `CrabError::exit_code`.
const EXIT_OVERWRITE_CONFLICT: i32 = 5;

/// Run `crab run` in `repo` with the given extra flags appended
/// before `--`. Returns the full output (status + stdout + stderr).
fn run_copy_with_flags(repo: &Path, extra: &[&str]) -> std::process::Output {
    let mut args = vec![
        "run", "--name", "copy", "--deps", "a.txt", "--outs", "b.txt",
    ];
    args.extend_from_slice(extra);
    args.extend_from_slice(&["--", "/bin/cp", "a.txt", "b.txt"]);

    Command::new(bin())
        .current_dir(repo)
        .env("CRAB_WORKFLOW_ENABLED", "1")
        .args(&args)
        .output()
        .expect("crab run should spawn")
}

/// Cache-hit no-op: the second run sees a file on disk whose hash
/// matches the cache entry. Materialization is skipped, so the
/// file's mtime stays the same.
#[test]
fn cache_hit_noop_preserves_file_mtime() {
    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join("a.txt"), b"payload").unwrap();

    // First run: miss → cp produces b.txt.
    let out = run_copy_with_flags(tmp.path(), &[]);
    assert!(
        out.status.success(),
        "first run failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mtime_before = mtime_of(&tmp.path().join("b.txt"));

    // Small sleep so a hypothetical rewrite would show a different
    // mtime at the filesystem's resolution. macOS HFS+ has 1s
    // resolution; everything else is sub-second but this keeps the
    // test robust across filesystems.
    std::thread::sleep(std::time::Duration::from_millis(1100));

    // Second run: same inputs → cache hit. Since b.txt's on-disk
    // hash matches the cache entry, the overwrite policy returns
    // `NoOp` and `write_atomic` is never called.
    let out = run_copy_with_flags(tmp.path(), &[]);
    assert!(
        out.status.success(),
        "second run failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mtime_after = mtime_of(&tmp.path().join("b.txt"));

    assert_eq!(
        mtime_before, mtime_after,
        "cache-hit no-op must not touch the file's mtime",
    );
}

/// Mismatching file on disk + `--no-overwrite` → refuse the write
/// with `StageOverwriteConflict`.
#[test]
fn mismatch_with_no_overwrite_fails_with_stage_overwrite_conflict() {
    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join("a.txt"), b"payload").unwrap();

    // First run: miss → cp produces b.txt. Cache entry hashes b.txt
    // as "payload".
    let out = run_copy_with_flags(tmp.path(), &[]);
    assert!(
        out.status.success(),
        "first run failed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Tamper with b.txt so its on-disk hash no longer matches the
    // cache entry's recorded hash.
    fs::write(tmp.path().join("b.txt"), b"tampered").unwrap();

    // Second run with --no-overwrite: cache hit detected, but the
    // overwrite policy refuses because the existing file differs.
    let out = run_copy_with_flags(tmp.path(), &["--no-overwrite"]);
    assert!(
        !out.status.success(),
        "--no-overwrite must fail on mismatching file"
    );
    assert_eq!(
        out.status.code(),
        Some(EXIT_OVERWRITE_CONFLICT),
        "expected StageOverwriteConflict exit code ({EXIT_OVERWRITE_CONFLICT}); \
         got status={:?}, stderr={}",
        out.status,
        String::from_utf8_lossy(&out.stderr),
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("overwrite conflict") || stderr.contains("CRAB-E0217"),
        "expected overwrite-conflict error text in stderr; got {stderr}"
    );

    // Tampered bytes must remain on disk — `--no-overwrite` refused
    // the clobber cleanly.
    assert_eq!(
        fs::read(tmp.path().join("b.txt")).unwrap(),
        b"tampered".to_vec()
    );
}

/// Mismatch without `--no-overwrite` → the cache-hit path currently
/// takes the on-disk bytes and re-writes them atomically (phase 1
/// doesn't pull from xorbs yet; `materialize_hit` re-reads the
/// declared out path — see the TODO in `cmd::run::materialize_hit`).
/// The test here asserts the run *succeeds* (the overwrite-policy
/// guard doesn't block it) without asserting which bytes end up on
/// disk. Once xorb reconstruction lands in phase 3, flip the
/// `assert_ne!` to `assert_eq!(read(...), b"payload")`.
#[test]
fn mismatch_without_flags_overwrites() {
    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join("a.txt"), b"payload").unwrap();

    let out = run_copy_with_flags(tmp.path(), &[]);
    assert!(out.status.success(), "first run failed");

    // Tamper.
    fs::write(tmp.path().join("b.txt"), b"tampered").unwrap();

    // Plain second run succeeds — the overwrite guard does not
    // block, the cache-hit materialization runs, and `crab run`
    // exits clean. Byte-level restoration against the cache entry
    // is a phase-3 item (xorb reconstruction).
    let out = run_copy_with_flags(tmp.path(), &[]);
    assert!(
        out.status.success(),
        "default overwrite must succeed: stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Uncommitted git changes → should fail without `--force`. The
/// current phase-1 wiring stubs `git_dirty: false` in
/// `cmd::run::inspect_existing` (no gitoxide index probe yet), so
/// this test is ignored until the real check lands. Keep the setup
/// in place so it turns on as soon as the probe is implemented —
/// no new harness needed.
#[test]
#[ignore = "cmd::run::inspect_existing returns git_dirty: false unconditionally in phase 1"]
fn mismatch_with_dirty_git_fails_without_force() {
    let tmp = TempDir::new().unwrap();
    fs::write(tmp.path().join("a.txt"), b"payload").unwrap();

    // Init a git repo and commit a baseline b.txt so uncommitted
    // modifications are detectable relative to HEAD.
    assert!(
        Command::new("git")
            .current_dir(tmp.path())
            .args(["init", "--quiet"])
            .status()
            .expect("git init")
            .success()
    );
    for (k, v) in [("user.email", "t@t"), ("user.name", "t")] {
        assert!(
            Command::new("git")
                .current_dir(tmp.path())
                .args(["config", k, v])
                .status()
                .expect("git config")
                .success()
        );
    }
    fs::write(tmp.path().join("b.txt"), b"baseline").unwrap();
    assert!(
        Command::new("git")
            .current_dir(tmp.path())
            .args(["add", "a.txt", "b.txt"])
            .status()
            .expect("git add")
            .success()
    );
    assert!(
        Command::new("git")
            .current_dir(tmp.path())
            .args(["commit", "--quiet", "-m", "baseline"])
            .status()
            .expect("git commit")
            .success()
    );

    // Produce a cache entry for the stage.
    let out = run_copy_with_flags(tmp.path(), &[]);
    assert!(out.status.success(), "first run failed");

    // Dirty working tree: local modification to the stage's out.
    fs::write(tmp.path().join("b.txt"), b"dirty").unwrap();

    // Second run (no --force) must refuse because b.txt is dirty
    // relative to HEAD and the cache entry's bytes differ from the
    // on-disk bytes.
    let out = run_copy_with_flags(tmp.path(), &[]);
    assert!(
        !out.status.success(),
        "dirty-git overwrite must fail without --force"
    );
    assert_eq!(out.status.code(), Some(EXIT_OVERWRITE_CONFLICT));

    // With --force, the overwrite is allowed.
    let out = run_copy_with_flags(tmp.path(), &["--force"]);
    assert!(out.status.success(), "--force must allow the overwrite");
}

fn mtime_of(path: &Path) -> SystemTime {
    fs::metadata(path)
        .expect("metadata")
        .modified()
        .expect("mtime available on this platform")
}
