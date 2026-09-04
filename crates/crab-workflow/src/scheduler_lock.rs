//! Workflow scheduler lock — serializes `crab run` invocations
//! against the same repo.
//!
//! Design §"Concurrency model": one `crab run` at a time per repo
//! via file-lock at `.crab/workflow/.lock`. Losers honor
//! `--lock-timeout` (default 600s, R24) or `--no-wait`. The lock file
//! records the holder's PID so the loser can render a
//! [`CrabError::WorkflowLockTimeout { held_by, waited_ms }`] that
//! points at the process to kill or wait on.
//!
//! This is a thin cousin of the staging-area lock at `crab-staging`:
//! same PID on-disk diagnostic, but simpler: there's a single
//! exclusive holder, there's no reader/writer split, and the
//! lifetime of the lock matches the lifetime of a `crab run`
//! invocation.
//!
//! The lock file itself lives at `{workflow_root}/.lock`, where
//! `workflow_root` is `{repo_root}/.crab/workflow`. The parent
//! directory is created on demand so the first `crab run` in a
//! fresh repo doesn't fail on a missing directory.
//!
//! # Drop behavior
//!
//! On drop the guard explicitly unlocks before closing its handle,
//! so a descriptor inherited by a concurrent fork cannot prolong ownership.
//! The lockfile is deliberately
//! retained so a waiter cannot acquire the inode and then have its
//! pathname unlinked by the previous holder. The next holder
//! overwrites the diagnostic PID before returning.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fs4::fs_std::FileExt as LockFileExt;

use tracing::{debug, warn};

use crate::{Result, WorkflowError as CrabError};

/// Name of the lockfile inside the workflow root.
const LOCKFILE_NAME: &str = ".lock";

/// Lower bound on the per-attempt backoff. Small enough that a lock
/// released immediately after we started waiting gets picked up
/// within one OS scheduler tick.
const POLL_INITIAL: Duration = Duration::from_millis(25);

/// Cap on the per-attempt backoff. Keeps responsiveness when the
/// holder releases after a long wait — a 600-second default timeout
/// shouldn't mean the waiter sleeps for 20s past the release event.
const POLL_MAX: Duration = Duration::from_millis(500);

/// Backoff growth factor. Standard 2x exponential, capped at
/// [`POLL_MAX`].
const POLL_MULTIPLIER: u32 = 2;

/// RAII guard around an acquired workflow scheduler lock.
///
/// Holding this value means the current process is the sole
/// scheduler running against the target `workflow_root`. Dropping
/// it releases the advisory lock before closing the file descriptor
/// while retaining the lockfile for the next holder to reuse.
///
/// Does NOT implement `Clone` or `Copy` — the lock is exclusive by
/// construction.
#[must_use = "dropping the lock releases it; bind to a variable to hold the lock"]
#[derive(Debug)]
pub struct SchedulerLock {
    /// Held for the lifetime of the guard. Dropping the guard releases
    /// the flock; we keep it private so callers can't accidentally
    /// drop it independently of the guard.
    file: Option<File>,
    path: PathBuf,
}

impl SchedulerLock {
    /// Path of the lockfile this guard holds. Useful for diagnostics
    /// and tests.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Acquire the scheduler lock at `{workflow_root}/.lock`, waiting
    /// up to `timeout` for a currently-held lock to release.
    ///
    /// Passing [`Duration::ZERO`] is equivalent to [`try_acquire`]
    /// returning a `WorkflowLockTimeout` on contention — `--no-wait`
    /// routes through here with a zero timeout.
    ///
    /// On success the parent directory exists, the lockfile at
    /// `{workflow_root}/.lock` contains the current PID (as ASCII
    /// digits, no trailing newline), and the returned guard holds
    /// the flock.
    ///
    /// # Errors
    ///
    /// Returns [`CrabError::WorkflowLockTimeout`] when another
    /// process still holds the lock after `timeout` elapses. The
    /// `held_by` field carries the holder's PID parsed from the
    /// lockfile (or `None` when the file is missing, empty, or
    /// otherwise unreadable). `waited_ms` is the actual wall-clock
    /// wait time, not the budgeted timeout.
    ///
    /// Returns [`CrabError::Io`] for other filesystem failures
    /// (permission denied, ENOSPC, etc.).
    pub fn acquire(workflow_root: &Path, timeout: Duration) -> Result<Self> {
        std::fs::create_dir_all(workflow_root).map_err(CrabError::Io)?;
        let path = workflow_root.join(LOCKFILE_NAME);
        let start = Instant::now();
        let mut delay = POLL_INITIAL;

        loop {
            let file = open_lockfile(&path)?;
            match try_flock_exclusive(&file) {
                Ok(()) => {
                    write_pid(&file, &path);
                    debug!(
                        path = %path.display(),
                        elapsed_ms = start.elapsed().as_millis() as u64,
                        "workflow scheduler lock acquired"
                    );
                    return Ok(Self {
                        file: Some(file),
                        path,
                    });
                }
                Err(e) if is_would_block(&e) => {
                    let elapsed = start.elapsed();
                    if elapsed >= timeout {
                        let held_by = read_holder_pid(&path);
                        let waited_ms = elapsed.as_millis().min(u128::from(u64::MAX)) as u64;
                        warn!(
                            path = %path.display(),
                            ?held_by,
                            waited_ms,
                            "workflow scheduler lock timeout"
                        );
                        return Err(CrabError::WorkflowLockTimeout { held_by, waited_ms });
                    }
                    // Drop the unfcocked handle before sleeping so we
                    // don't hold an extra fd across the sleep. A fresh
                    // open on the next iteration still sees the same
                    // inode the holder's fd points at.
                    drop(file);
                    let remaining = timeout.saturating_sub(elapsed);
                    let nap = delay.min(remaining);
                    std::thread::sleep(nap);
                    delay = (delay * POLL_MULTIPLIER).min(POLL_MAX);
                }
                Err(e) => return Err(CrabError::Io(e)),
            }
        }
    }

    /// Try to acquire the lock without blocking.
    ///
    /// Returns `Ok(Some(guard))` when the lock was free,
    /// `Ok(None)` when another process holds it, or `Err` on
    /// filesystem failure. Unlike [`acquire`], `try_acquire` never
    /// returns `WorkflowLockTimeout` — contention is reported via
    /// the `None` variant so the caller can branch without pattern
    /// matching on a specific error.
    ///
    /// # Errors
    ///
    /// Returns [`CrabError::Io`] on permission / disk / parent
    /// directory failures. Does NOT return any lock-timeout error.
    pub fn try_acquire(workflow_root: &Path) -> Result<Option<Self>> {
        std::fs::create_dir_all(workflow_root).map_err(CrabError::Io)?;
        let path = workflow_root.join(LOCKFILE_NAME);
        let file = open_lockfile(&path)?;
        match try_flock_exclusive(&file) {
            Ok(()) => {
                write_pid(&file, &path);
                Ok(Some(Self {
                    file: Some(file),
                    path,
                }))
            }
            Err(e) if is_would_block(&e) => Ok(None),
            Err(e) => Err(CrabError::Io(e)),
        }
    }
}

impl Drop for SchedulerLock {
    fn drop(&mut self) {
        // Remove the Windows diagnostic sidecar while this guard still
        // owns the lock. Removing it after closing the handle could race
        // with the next holder writing its PID into the same sidecar.
        #[cfg(windows)]
        let pid_path = pid_path(&self.path);
        #[cfg(windows)]
        remove_pid_sidecar(&pid_path, &self.path);

        // A concurrent fork may retain the open-file description until exec.
        // Release this guard's ownership explicitly; closing only its copy can
        // otherwise leave a completed workflow blocking the next invocation.
        if let Some(file) = self.file.take()
            && let Err(error) = LockFileExt::unlock(&file)
        {
            warn!(path = %self.path.display(), %error, "workflow scheduler unlock failed");
        }
    }
}

// --- Internal helpers ---

/// Open (or create) the lockfile with the permissions the flock
/// family expects: read+write, truncate-free (so the holder's PID
/// survives a racing `try_acquire` on a stale file), create-if-
/// missing.
fn open_lockfile(path: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(CrabError::Io)
}

/// Write `std::process::id()` into the lockfile. Best-effort: the
/// lock itself is what guarantees mutual exclusion, the PID is
/// purely for diagnostic messaging on timeout.
fn write_pid(file: &File, _path: &Path) {
    // `set_len(0)` + seek(0) guarantees a clean rewrite even when
    // the file previously held a longer PID (e.g., 99999 → 42).
    let pid = std::process::id();
    let _ = (&*file).flush();
    let _ = file.set_len(0);
    // The Seek / Write impls on &File require mutability so we grab
    // a short-lived handle via (&mut &File) via a local rebind.
    let mut handle: &File = file;
    let _ = handle.seek(SeekFrom::Start(0));
    let _ = handle.write_all(pid.to_string().as_bytes());
    let _ = (&mut &*file).flush();
    // fsync for durability — another process reading the PID
    // shouldn't race against an uncommitted write. `sync_all`
    // failures are non-fatal; worst case the reader falls back to
    // `held_by: None`.
    let _ = file.sync_all();

    // Windows denies reads through a second handle while LockFileEx
    // protects the lockfile. Keep the diagnostic PID in an unlocked
    // sidecar there so waiters can still report the holder.
    #[cfg(windows)]
    {
        let _ = std::fs::write(pid_path(_path), pid.to_string());
    }
}

/// Parse the PID stored in the lockfile. Returns `None` when the
/// file is missing, unreadable, or doesn't contain a valid u32.
fn read_holder_pid(path: &Path) -> Option<u32> {
    #[cfg(windows)]
    if let Some(pid) = read_pid_file(&pid_path(path)) {
        return Some(pid);
    }
    read_pid_file(path)
}

fn read_pid_file(path: &Path) -> Option<u32> {
    let mut file = File::open(path).ok()?;
    let mut buf = String::new();
    file.read_to_string(&mut buf).ok()?;
    buf.trim().parse().ok()
}

#[cfg(windows)]
fn pid_path(path: &Path) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("lock");
    path.with_file_name(format!("{file_name}.pid"))
}

#[cfg(windows)]
fn remove_pid_sidecar(path: &Path, lock_path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            debug!(
                path = %lock_path.display(),
                error = %e,
                "best-effort lock PID removal failed"
            );
        }
    }
}

/// Attempt a non-blocking exclusive file lock on `file`. The `fs4`
/// adapter uses `flock` on Unix and `LockFileEx` on Windows, keeping
/// the scheduler lock contract identical across native runners.
fn try_flock_exclusive(file: &File) -> std::io::Result<()> {
    if LockFileExt::try_lock_exclusive(file)? {
        Ok(())
    } else {
        Err(std::io::Error::from(std::io::ErrorKind::WouldBlock))
    }
}

fn is_would_block(err: &std::io::Error) -> bool {
    err.kind() == std::io::ErrorKind::WouldBlock
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tempfile::TempDir;
    use tokio::sync::{Notify, oneshot};

    #[test]
    fn acquire_succeeds_on_free_lock() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("workflow");
        let guard = SchedulerLock::acquire(&root, Duration::from_millis(100)).unwrap();
        assert!(guard.path().exists());
        assert_eq!(guard.path().file_name().unwrap(), LOCKFILE_NAME);
        let pid = read_holder_pid(guard.path()).expect("pid recorded");
        assert_eq!(pid, std::process::id());
    }

    #[test]
    fn try_acquire_returns_some_on_free_lock() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("workflow");
        let guard = SchedulerLock::try_acquire(&root).unwrap();
        assert!(guard.is_some());
    }

    #[test]
    fn drop_releases_lock_and_retains_diagnostic_file() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("workflow");
        let path = {
            let guard = SchedulerLock::acquire(&root, Duration::ZERO).unwrap();
            assert!(guard.path().exists());
            guard.path().to_path_buf()
        };
        assert!(path.exists(), "lockfile should remain for the next holder");
        #[cfg(windows)]
        assert!(
            !pid_path(&path).exists(),
            "lock PID sidecar should be removed after drop"
        );

        // Re-acquire: should succeed.
        let _next = SchedulerLock::acquire(&root, Duration::ZERO).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn drop_releases_lock_with_a_duplicated_descriptor() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("workflow");
        let guard = SchedulerLock::acquire(&root, Duration::ZERO).unwrap();
        // A concurrent fork can retain this open-file description until exec,
        // even though the descriptor is close-on-exec. Model it without timing.
        let duplicate = guard.file.as_ref().unwrap().try_clone().unwrap();
        drop(guard);
        let next = SchedulerLock::acquire(&root, Duration::ZERO).unwrap();
        drop(duplicate);
        assert!(SchedulerLock::try_acquire(&root).unwrap().is_none());
        drop(next);
    }

    // --- Two-tokio-task contention tests ---
    //
    // All integration-style concurrency tests live within a single
    // process (same PID), so we assert `held_by.is_some()` rather
    // than checking for a specific PID value; the holder and waiter
    // share `std::process::id()`.

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn task_b_try_acquire_is_none_while_task_a_holds() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("workflow");

        let acquired = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let (done_tx, done_rx) = oneshot::channel();

        let acquired_a = acquired.clone();
        let release_a = release.clone();
        let root_a = root.clone();
        let task_a = tokio::spawn(async move {
            let _guard = SchedulerLock::acquire(&root_a, Duration::ZERO).unwrap();
            acquired_a.notify_one();
            release_a.notified().await;
            let _ = done_tx.send(());
        });

        // Wait until task A reports it owns the lock.
        acquired.notified().await;

        let root_b = root.clone();
        let outcome = tokio::task::spawn_blocking(move || SchedulerLock::try_acquire(&root_b))
            .await
            .unwrap()
            .unwrap();
        assert!(
            outcome.is_none(),
            "try_acquire must report None while task A holds the lock"
        );

        release.notify_one();
        done_rx.await.unwrap();
        task_a.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn task_b_acquire_times_out_with_holder_pid() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("workflow");

        let acquired = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());

        let acquired_a = acquired.clone();
        let release_a = release.clone();
        let root_a = root.clone();
        let task_a = tokio::spawn(async move {
            let _guard = SchedulerLock::acquire(&root_a, Duration::ZERO).unwrap();
            acquired_a.notify_one();
            release_a.notified().await;
        });

        acquired.notified().await;

        let root_b = root.clone();
        let timeout = Duration::from_millis(500);
        let started = Instant::now();
        let err = tokio::task::spawn_blocking(move || SchedulerLock::acquire(&root_b, timeout))
            .await
            .unwrap()
            .expect_err("acquire must time out while A holds the lock");
        let elapsed = started.elapsed();

        match err {
            CrabError::WorkflowLockTimeout { held_by, waited_ms } => {
                // held_by is present — task A wrote its PID (same as
                // ours in-process) before handing off. We don't
                // assert the exact value because all threads share a
                // PID; only its presence matters for the diagnostic.
                assert!(held_by.is_some(), "held_by must carry the holder PID");
                assert_eq!(
                    held_by.unwrap(),
                    std::process::id(),
                    "same-process PID matches std::process::id()"
                );
                // waited_ms is within an order of magnitude of the
                // timeout — timing assertions are loose to avoid CI
                // flakiness on slow runners.
                assert!(
                    waited_ms >= 400,
                    "waited_ms should be >= timeout, got {waited_ms}"
                );
                assert!(
                    elapsed >= timeout,
                    "wall-clock wait must be at least the timeout budget"
                );
            }
            other => panic!("expected WorkflowLockTimeout, got {other}"),
        }

        release.notify_one();
        task_a.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn task_b_can_acquire_after_task_a_releases() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("workflow");

        let acquired = Arc::new(Notify::new());
        let released = Arc::new(Notify::new());

        let acquired_a = acquired.clone();
        let released_a = released.clone();
        let root_a = root.clone();
        let task_a = tokio::spawn(async move {
            {
                let _guard = SchedulerLock::acquire(&root_a, Duration::ZERO).unwrap();
                acquired_a.notify_one();
                // Hold briefly then drop by leaving scope.
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            released_a.notify_one();
        });

        acquired.notified().await;

        // Start the waiter with a generous timeout — it should
        // succeed after task A's 100ms hold + drop.
        let root_b = root.clone();
        let task_b = tokio::task::spawn_blocking(move || {
            SchedulerLock::acquire(&root_b, Duration::from_secs(5))
        });

        // Wait for task A to finish, then check that task B's
        // acquisition completed successfully.
        task_a.await.unwrap();
        released.notified().await;
        let guard_b = task_b
            .await
            .unwrap()
            .expect("task B must eventually acquire");
        assert!(guard_b.path().exists());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn no_wait_zero_timeout_fails_fast_when_held() {
        // `--no-wait` plumbing passes `Duration::ZERO`: the call
        // must return WorkflowLockTimeout without sleeping when the
        // lock is already held.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("workflow");

        let acquired = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());

        let acquired_a = acquired.clone();
        let release_a = release.clone();
        let root_a = root.clone();
        let task_a = tokio::spawn(async move {
            let _guard = SchedulerLock::acquire(&root_a, Duration::ZERO).unwrap();
            acquired_a.notify_one();
            release_a.notified().await;
        });

        acquired.notified().await;

        let root_b = root.clone();
        let started = Instant::now();
        let err =
            tokio::task::spawn_blocking(move || SchedulerLock::acquire(&root_b, Duration::ZERO))
                .await
                .unwrap()
                .expect_err("no-wait must fail fast");
        let elapsed = started.elapsed();

        assert!(
            matches!(err, CrabError::WorkflowLockTimeout { .. }),
            "wrong variant: {err}"
        );
        assert!(
            elapsed < Duration::from_millis(250),
            "no-wait path must not sleep; took {elapsed:?}"
        );

        release.notify_one();
        task_a.await.unwrap();
    }

    #[test]
    fn acquire_creates_missing_parent_directory() {
        let tmp = TempDir::new().unwrap();
        // Workflow root two levels deep — neither exists yet.
        let root = tmp.path().join("a").join("b").join("workflow");
        assert!(!root.exists());
        let _guard = SchedulerLock::acquire(&root, Duration::ZERO).unwrap();
        assert!(root.is_dir());
    }

    #[test]
    fn read_holder_pid_returns_none_for_missing_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("missing.lock");
        assert_eq!(read_holder_pid(&path), None);
    }

    #[test]
    fn read_holder_pid_returns_none_for_empty_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("empty.lock");
        std::fs::write(&path, b"").unwrap();
        assert_eq!(read_holder_pid(&path), None);
    }

    #[test]
    fn read_holder_pid_returns_none_for_garbage() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("garbage.lock");
        std::fs::write(&path, b"not-a-number\n").unwrap();
        assert_eq!(read_holder_pid(&path), None);
    }
}
