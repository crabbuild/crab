//! Serializes workflow schedulers with an exclusive advisory file lock.
//!
//! The caller supplies a workflow root; the guard locks its `.lock` file and
//! creates missing parent directories. `acquire` yields during contention;
//! `try_acquire` reports contention immediately. Timeout policy belongs to the
//! caller, including the CLI's `--lock-timeout` and `--no-wait` options.
//!
//! The holder PID is a best-effort diagnostic, not proof of ownership. Windows
//! waiters read an unlocked `.lock.pid` sidecar because the locked file cannot
//! be read through a second handle. Unix waiters read the lockfile itself.
//!
//! Drop removes the Windows sidecar while still holding the lock, then explicitly
//! unlocks before closing the descriptor. The lockfile remains: unlinking it could
//! let a new caller lock a different inode while a waiter still holds the old one.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use fs4::fs_std::FileExt as LockFileExt;

use tracing::{debug, warn};

use crate::{Result, WorkflowError};

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
    /// Passing [`Duration::ZERO`] is equivalent to [`Self::try_acquire`]
    /// returning a `WorkflowLockTimeout` on contention — `--no-wait`
    /// routes through here with a zero timeout.
    ///
    /// Contention waits use Tokio timers; dropping this future cancels waiting
    /// without leaving a background waiter. Filesystem attempts and best-effort
    /// PID writes remain synchronous. Call within a Tokio runtime with time
    /// enabled. On success, retain the returned guard for the protected work.
    ///
    /// # Errors
    ///
    /// Returns [`WorkflowError::WorkflowLockTimeout`] when another
    /// process still holds the lock after `timeout` elapses. The
    /// `held_by` field carries the holder's PID parsed from the
    /// lockfile (or `None` when the file is missing, empty, or
    /// otherwise unreadable). `waited_ms` is the actual wall-clock
    /// wait time, not the budgeted timeout.
    ///
    /// Returns [`WorkflowError::Io`] for other filesystem failures
    /// (permission denied, ENOSPC, etc.).
    pub async fn acquire(workflow_root: &Path, timeout: Duration) -> Result<Self> {
        std::fs::create_dir_all(workflow_root).map_err(WorkflowError::Io)?;
        let path = workflow_root.join(LOCKFILE_NAME);
        let start = Instant::now();
        let mut delay = POLL_INITIAL;

        loop {
            let file = open_lockfile(&path)?;
            match LockFileExt::try_lock_exclusive(&file) {
                Ok(true) => {
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
                Ok(false) => {
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
                        return Err(WorkflowError::WorkflowLockTimeout { held_by, waited_ms });
                    }
                    // No descriptor is needed during backoff. The retained
                    // lockfile keeps the next attempt on the holder's inode.
                    drop(file);
                    let remaining = timeout.saturating_sub(elapsed);
                    let nap = delay.min(remaining);
                    tokio::time::sleep(nap).await;
                    delay = (delay * POLL_MULTIPLIER).min(POLL_MAX);
                }
                Err(e) => return Err(WorkflowError::Io(e)),
            }
        }
    }

    /// Try to acquire the lock without blocking.
    ///
    /// Returns `Ok(Some(guard))` when the lock was free,
    /// `Ok(None)` when another process holds it, or `Err` on
    /// filesystem failure. Unlike [`Self::acquire`], `try_acquire` never
    /// returns `WorkflowLockTimeout` — contention is reported via
    /// the `None` variant so the caller can branch without pattern
    /// matching on a specific error.
    ///
    /// # Errors
    ///
    /// Returns [`WorkflowError::Io`] on permission / disk / parent
    /// directory failures. Does NOT return any lock-timeout error.
    pub fn try_acquire(workflow_root: &Path) -> Result<Option<Self>> {
        std::fs::create_dir_all(workflow_root).map_err(WorkflowError::Io)?;
        let path = workflow_root.join(LOCKFILE_NAME);
        let file = open_lockfile(&path)?;
        match LockFileExt::try_lock_exclusive(&file) {
            Ok(true) => {
                write_pid(&file, &path);
                Ok(Some(Self {
                    file: Some(file),
                    path,
                }))
            }
            Ok(false) => Ok(None),
            Err(e) => Err(WorkflowError::Io(e)),
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
        .map_err(WorkflowError::Io)
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
    // Persist the diagnostic best-effort. PID durability and read visibility
    // do not establish ownership; the advisory lock does.
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn waiting_allows_the_holder_future_to_release() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("workflow");
        let holder = SchedulerLock::try_acquire(&root).unwrap().unwrap();
        let (acquired, ()) = tokio::join!(
            biased;
            async { SchedulerLock::acquire(&root, Duration::from_millis(100)).await },
            async {
                tokio::time::sleep(Duration::from_millis(1)).await;
                drop(holder);
            }
        );
        assert!(acquired.is_ok(), "lock waiting starved the holder future");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn cancelled_wait_does_not_retain_lock_ownership() {
        let temp = TempDir::new().unwrap();
        let root = temp.path().join("workflow");
        let holder = SchedulerLock::try_acquire(&root).unwrap().unwrap();
        let cancelled = tokio::time::timeout(
            Duration::from_millis(10),
            SchedulerLock::acquire(&root, Duration::from_secs(5)),
        )
        .await;
        assert!(cancelled.is_err());
        drop(holder);
        let next = SchedulerLock::try_acquire(&root).unwrap();
        assert!(next.is_some(), "cancelled waiter retained lock ownership");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn acquire_succeeds_on_free_lock() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("workflow");
        let guard = SchedulerLock::acquire(&root, Duration::from_millis(100))
            .await
            .unwrap();
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

    #[tokio::test(flavor = "multi_thread")]
    async fn drop_releases_lock_and_retains_diagnostic_file() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("workflow");
        let path = {
            let guard = SchedulerLock::acquire(&root, Duration::ZERO).await.unwrap();
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
        let _next = SchedulerLock::acquire(&root, Duration::ZERO).await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn drop_releases_lock_with_a_duplicated_descriptor() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("workflow");
        let guard = SchedulerLock::acquire(&root, Duration::ZERO).await.unwrap();
        // A concurrent fork can retain this open-file description until exec,
        // even though the descriptor is close-on-exec. Model it without timing.
        let duplicate = guard.file.as_ref().unwrap().try_clone().unwrap();
        drop(guard);
        let next = SchedulerLock::acquire(&root, Duration::ZERO).await.unwrap();
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
            let _guard = SchedulerLock::acquire(&root_a, Duration::ZERO)
                .await
                .unwrap();
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
            let _guard = SchedulerLock::acquire(&root_a, Duration::ZERO)
                .await
                .unwrap();
            acquired_a.notify_one();
            release_a.notified().await;
        });

        acquired.notified().await;

        let root_b = root.clone();
        let timeout = Duration::from_millis(500);
        let started = Instant::now();
        let err = SchedulerLock::acquire(&root_b, timeout)
            .await
            .expect_err("acquire must time out while A holds the lock");
        let elapsed = started.elapsed();

        match err {
            WorkflowError::WorkflowLockTimeout { held_by, waited_ms } => {
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
                let _guard = SchedulerLock::acquire(&root_a, Duration::ZERO)
                    .await
                    .unwrap();
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
        let task_b =
            tokio::spawn(
                async move { SchedulerLock::acquire(&root_b, Duration::from_secs(5)).await },
            );

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
            let _guard = SchedulerLock::acquire(&root_a, Duration::ZERO)
                .await
                .unwrap();
            acquired_a.notify_one();
            release_a.notified().await;
        });

        acquired.notified().await;

        let root_b = root.clone();
        let started = Instant::now();
        let err = SchedulerLock::acquire(&root_b, Duration::ZERO)
            .await
            .expect_err("no-wait must fail fast");
        let elapsed = started.elapsed();

        assert!(
            matches!(err, WorkflowError::WorkflowLockTimeout { .. }),
            "wrong variant: {err}"
        );
        assert!(
            elapsed < Duration::from_millis(250),
            "no-wait path must not sleep; took {elapsed:?}"
        );

        release.notify_one();
        task_a.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn acquire_creates_missing_parent_directory() {
        let tmp = TempDir::new().unwrap();
        // Workflow root two levels deep — neither exists yet.
        let root = tmp.path().join("a").join("b").join("workflow");
        assert!(!root.exists());
        let _guard = SchedulerLock::acquire(&root, Duration::ZERO).await.unwrap();
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
