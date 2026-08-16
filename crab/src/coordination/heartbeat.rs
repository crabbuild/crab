//! Background task that periodically extends a push lock's TTL.
//!
//! While a push is in progress, the [`LockHeartbeat`] reads the lock
//! payload, verifies the holder matches, and writes a new `expires_at`
//! via ETag-based CAS. If the lock is stolen, deleted, or unreachable
//! after one retry, the heartbeat cancels the push via a
//! [`CancellationToken`].
//!
use std::time::Duration;

use bytes::Bytes;
use object_store::path::Path;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, warn};

use crate::core::error::CrabError;
use crate::storage::retry::{RetryClass, retry_class};
use crate::storage::store::Store;
use crab_coordination::{PushLockPayload as LockPayload, unix_now};

/// Minimum heartbeat interval (seconds).
const MIN_INTERVAL_SECS: u64 = 10;

/// Background task that periodically extends a push lock's TTL.
///
/// Spawn via [`LockHeartbeat::spawn`]; stop via [`LockHeartbeat::stop`].
/// If the lock is stolen, deleted, or a CAS conflict occurs, the
/// heartbeat cancels the associated push through `push_cancel`.
pub struct LockHeartbeat {
    cancel: CancellationToken,
    handle: Option<JoinHandle<()>>,
}

impl LockHeartbeat {
    /// Spawn a heartbeat task for the given lock.
    ///
    /// `interval` is clamped to `[10, ttl - 10]` seconds. The heartbeat
    /// runs until explicitly stopped or until it detects the lock has
    /// been stolen/deleted.
    pub fn spawn(
        store: Store,
        lock_path: String,
        holder: String,
        ttl: Duration,
        interval: Duration,
        push_cancel: CancellationToken,
    ) -> Self {
        let interval = clamp_interval(interval, ttl);
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        let push_cancel_clone = push_cancel.clone();

        let handle = tokio::spawn(async move {
            heartbeat_loop(
                store,
                lock_path,
                holder,
                ttl,
                interval,
                push_cancel_clone,
                cancel_clone,
            )
            .await;
        });

        Self {
            cancel,
            handle: Some(handle),
        }
    }

    /// Stop the heartbeat task and wait for it to exit.
    pub async fn stop(mut self) {
        self.cancel.cancel();
        // The task checks the cancel token on each iteration, so it
        // will exit promptly. Ignore join errors (task panicked or was
        // already finished).
        if let Some(handle) = self.handle.take() {
            let _ = handle.await;
        }
    }

    /// Fire the cancellation token without awaiting the task.
    ///
    /// Safe to call from a synchronous `Drop` impl to stop the heartbeat
    /// from extending the lock during unwind. The background task will
    /// notice the cancellation at its next poll and exit.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }
}

impl Drop for LockHeartbeat {
    fn drop(&mut self) {
        // Ensure the background task stops extending the lock when the
        // pipeline is torn down, including on panic. If `stop()` was
        // already called, the handle is already taken and this is a
        // no-op beyond the idempotent cancel.
        self.cancel.cancel();
        if let Some(handle) = self.handle.take() {
            handle.abort();
        }
    }
}

/// Clamp the heartbeat interval to `[10, ttl - 10]` seconds.
///
/// If `ttl` is too small for a meaningful range (≤ 20s), the interval
/// is set to `MIN_INTERVAL_SECS`.
fn clamp_interval(interval: Duration, ttl: Duration) -> Duration {
    let ttl_secs = ttl.as_secs();
    let max_secs = ttl_secs.saturating_sub(MIN_INTERVAL_SECS);
    let clamped = interval
        .as_secs()
        .max(MIN_INTERVAL_SECS)
        .min(max_secs.max(MIN_INTERVAL_SECS));
    Duration::from_secs(clamped)
}

/// Core heartbeat loop. Runs until cancelled or until the lock is lost.
async fn heartbeat_loop(
    store: Store,
    lock_path: String,
    holder: String,
    ttl: Duration,
    interval: Duration,
    push_cancel: CancellationToken,
    self_cancel: CancellationToken,
) {
    let obj_path = Path::from(lock_path.as_str());

    loop {
        // Sleep for the heartbeat interval, but wake early if either
        // the push or the heartbeat itself is cancelled.
        tokio::select! {
            () = tokio::time::sleep(interval) => {}
            () = self_cancel.cancelled() => {
                debug!(lock_path = %lock_path, "heartbeat stopped");
                return;
            }
            () = push_cancel.cancelled() => {
                debug!(lock_path = %lock_path, "push cancelled, heartbeat exiting");
                return;
            }
        }

        // After waking, check cancellation before doing any I/O.
        if self_cancel.is_cancelled() || push_cancel.is_cancelled() {
            return;
        }

        match try_extend(&store, &obj_path, &holder, ttl).await {
            Ok(()) => {
                debug!(lock_path = %lock_path, "heartbeat extended lock TTL");
            }
            Err(HeartbeatFailure::LockStolen { actual_holder }) => {
                warn!(
                    lock_path = %lock_path,
                    actual_holder = %actual_holder,
                    "lock stolen by another holder, cancelling push"
                );
                push_cancel.cancel();
                return;
            }
            Err(HeartbeatFailure::LockDeleted) => {
                warn!(lock_path = %lock_path, "lock deleted, cancelling push");
                push_cancel.cancel();
                return;
            }
            Err(HeartbeatFailure::CasConflict) => {
                warn!(
                    lock_path = %lock_path,
                    "CAS conflict during heartbeat, lock may be stolen — cancelling push"
                );
                push_cancel.cancel();
                return;
            }
            Err(HeartbeatFailure::Transient(err)) => {
                warn!(
                    lock_path = %lock_path,
                    error = %err,
                    "heartbeat transient error, retrying once"
                );
                tokio::time::sleep(Duration::from_secs(1)).await;

                match try_extend(&store, &obj_path, &holder, ttl).await {
                    Ok(()) => {
                        debug!(lock_path = %lock_path, "heartbeat retry succeeded");
                    }
                    Err(retry_err) => {
                        error!(
                            lock_path = %lock_path,
                            error = ?retry_err,
                            "heartbeat retry failed, cancelling push"
                        );
                        push_cancel.cancel();
                        return;
                    }
                }
            }
            Err(HeartbeatFailure::Fatal(err)) => {
                error!(
                    lock_path = %lock_path,
                    error = %err,
                    "heartbeat fatal error, cancelling push"
                );
                push_cancel.cancel();
                return;
            }
        }
    }
}

/// Internal failure modes for a single heartbeat attempt.
#[derive(Debug)]
enum HeartbeatFailure {
    /// Lock holder doesn't match — someone else owns the lock.
    LockStolen { actual_holder: String },
    /// Lock file was deleted.
    LockDeleted,
    /// ETag-based CAS failed — lock was modified between read and write.
    CasConflict,
    /// Transient storage error — worth retrying once.
    Transient(CrabError),
    /// Non-transient, non-CAS error — give up.
    Fatal(CrabError),
}

/// Attempt a single heartbeat: read lock, verify holder, extend TTL.
async fn try_extend(
    store: &Store,
    obj_path: &Path,
    holder: &str,
    ttl: Duration,
) -> std::result::Result<(), HeartbeatFailure> {
    // Read current lock payload with ETag.
    let (body, etag) = match store.get_with_etag(obj_path).await {
        Ok(pair) => pair,
        Err(CrabError::NotFound { .. }) => {
            return Err(HeartbeatFailure::LockDeleted);
        }
        Err(e) => {
            return Err(classify_heartbeat_error(e));
        }
    };

    // Deserialize and verify holder.
    let payload: LockPayload = serde_json::from_slice(&body).map_err(|e| {
        HeartbeatFailure::Fatal(CrabError::Internal(format!(
            "heartbeat: malformed lock payload: {e}"
        )))
    })?;

    if payload.holder != holder {
        return Err(HeartbeatFailure::LockStolen {
            actual_holder: payload.holder,
        });
    }

    // Write new expires_at via CAS.
    let new_payload = LockPayload {
        holder: holder.to_owned(),
        expires_at: unix_now() + ttl.as_secs(),
    };
    let new_body = serde_json::to_vec(&new_payload).map_err(|e| {
        HeartbeatFailure::Fatal(CrabError::Internal(format!(
            "heartbeat: serialize lock payload: {e}"
        )))
    })?;

    match store.update(obj_path, Bytes::from(new_body), etag).await {
        Ok(_) => Ok(()),
        Err(CrabError::CasConflict { .. }) => Err(HeartbeatFailure::CasConflict),
        Err(e) => Err(classify_heartbeat_error(e)),
    }
}

/// Map a storage error into the appropriate heartbeat failure category.
fn classify_heartbeat_error(err: CrabError) -> HeartbeatFailure {
    match retry_class(&err) {
        RetryClass::Transient | RetryClass::Throttled { .. } => HeartbeatFailure::Transient(err),
        _ => HeartbeatFailure::Fatal(err),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use object_store::memory::InMemory;
    use std::sync::Arc;

    fn memory_store() -> Store {
        let inner: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
        Store::new(inner)
    }

    async fn put_lock(store: &Store, path: &str, holder: &str, ttl_secs: u64) {
        let payload = LockPayload {
            holder: holder.to_owned(),
            expires_at: unix_now() + ttl_secs,
        };
        let body = serde_json::to_vec(&payload).unwrap();
        store
            .put(&Path::from(path), Bytes::from(body))
            .await
            .unwrap();
    }

    #[test]
    fn clamp_interval_within_range() {
        let ttl = Duration::from_secs(300);
        let interval = Duration::from_secs(100);
        let clamped = clamp_interval(interval, ttl);
        assert_eq!(clamped, Duration::from_secs(100));
    }

    #[test]
    fn clamp_interval_below_minimum() {
        let ttl = Duration::from_secs(300);
        let interval = Duration::from_secs(5);
        let clamped = clamp_interval(interval, ttl);
        assert_eq!(clamped, Duration::from_secs(10));
    }

    #[test]
    fn clamp_interval_above_maximum() {
        let ttl = Duration::from_secs(300);
        let interval = Duration::from_secs(295);
        let clamped = clamp_interval(interval, ttl);
        assert_eq!(clamped, Duration::from_secs(290));
    }

    #[test]
    fn clamp_interval_small_ttl() {
        // TTL of 15s: max = 5, but min = 10, so clamped to 10.
        let ttl = Duration::from_secs(15);
        let interval = Duration::from_secs(100);
        let clamped = clamp_interval(interval, ttl);
        assert_eq!(clamped, Duration::from_secs(10));
    }

    #[tokio::test(start_paused = true)]
    async fn heartbeat_extends_ttl() {
        let store = memory_store();
        let lock_path = "repo/locks/refs/heads/main/lock";
        let holder = "test-holder";
        let ttl = Duration::from_secs(300);

        put_lock(&store, lock_path, holder, 300).await;

        let push_cancel = CancellationToken::new();
        let heartbeat = LockHeartbeat::spawn(
            store.clone(),
            lock_path.to_owned(),
            holder.to_owned(),
            ttl,
            Duration::from_secs(10),
            push_cancel.clone(),
        );

        // Give the heartbeat time to run one cycle (interval is clamped to 10s).
        tokio::time::sleep(Duration::from_secs(15)).await;

        // Stop and verify the lock still has our holder.
        heartbeat.stop().await;

        let (body, _) = store.get_with_etag(&Path::from(lock_path)).await.unwrap();
        let payload: LockPayload = serde_json::from_slice(&body).unwrap();
        assert_eq!(payload.holder, holder);
        assert!(!push_cancel.is_cancelled());
    }

    #[tokio::test(start_paused = true)]
    async fn heartbeat_detects_stolen_lock() {
        let store = memory_store();
        let lock_path = "repo/locks/refs/heads/main/lock";
        let holder = "original-holder";
        let ttl = Duration::from_secs(300);

        // Write lock with a different holder.
        let thief_payload = LockPayload {
            holder: "thief".to_owned(),
            expires_at: unix_now() + 300,
        };
        let body = serde_json::to_vec(&thief_payload).unwrap();
        store
            .put(&Path::from(lock_path), Bytes::from(body))
            .await
            .unwrap();

        let push_cancel = CancellationToken::new();
        let heartbeat = LockHeartbeat::spawn(
            store.clone(),
            lock_path.to_owned(),
            holder.to_owned(),
            ttl,
            Duration::from_secs(10),
            push_cancel.clone(),
        );

        // Wait for the heartbeat to detect the mismatch (interval is 10s).
        tokio::time::sleep(Duration::from_secs(15)).await;
        heartbeat.stop().await;

        assert!(
            push_cancel.is_cancelled(),
            "push should be cancelled on stolen lock"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn heartbeat_detects_deleted_lock() {
        let store = memory_store();
        let lock_path = "repo/locks/refs/heads/main/lock";
        let holder = "test-holder";
        let ttl = Duration::from_secs(300);

        // Don't create the lock file — heartbeat should detect NotFound.
        let push_cancel = CancellationToken::new();
        let heartbeat = LockHeartbeat::spawn(
            store.clone(),
            lock_path.to_owned(),
            holder.to_owned(),
            ttl,
            Duration::from_secs(10),
            push_cancel.clone(),
        );

        tokio::time::sleep(Duration::from_secs(15)).await;
        heartbeat.stop().await;

        assert!(
            push_cancel.is_cancelled(),
            "push should be cancelled on deleted lock"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn stop_cancels_heartbeat_cleanly() {
        let store = memory_store();
        let lock_path = "repo/locks/refs/heads/main/lock";
        let holder = "test-holder";
        let ttl = Duration::from_secs(300);

        put_lock(&store, lock_path, holder, 300).await;

        let push_cancel = CancellationToken::new();
        let heartbeat = LockHeartbeat::spawn(
            store.clone(),
            lock_path.to_owned(),
            holder.to_owned(),
            ttl,
            Duration::from_secs(10),
            push_cancel.clone(),
        );

        // Stop immediately — should not cancel the push.
        heartbeat.stop().await;
        assert!(!push_cancel.is_cancelled());
    }
}
