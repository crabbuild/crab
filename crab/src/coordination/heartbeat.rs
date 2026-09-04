//! Background task that periodically extends a push lock's TTL.
//!
//! While an operation is in progress, [`LockHeartbeat`] delegates renewal
//! to the shared lock owner. Lost or released ownership and exhausted
//! bounded retries cancel the operation via a [`CancellationToken`].
//!
use std::time::Duration;

use crab_coordination::PushLock;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error};

use crate::storage::store::Store;

/// Minimum heartbeat interval (seconds).
const MIN_INTERVAL_SECS: u64 = 10;

/// Background task that periodically extends a push lock's TTL.
///
/// Spawn via [`LockHeartbeat::spawn`]; stop via [`LockHeartbeat::stop`].
/// Lost or released ownership and exhausted bounded retries cancel the
/// associated operation through `push_cancel`.
pub struct LockHeartbeat {
    cancel: CancellationToken,
    handle: Option<JoinHandle<()>>,
}

impl LockHeartbeat {
    /// Spawn a heartbeat task for the given lock.
    ///
    /// `interval` is clamped to `[10, ttl - 10]` seconds. The heartbeat
    /// runs until explicitly stopped or until it detects the lock has
    /// been stolen, deleted, or released, or renewal fails.
    pub fn spawn(
        store: Store,
        lock_path: String,
        holder: String,
        ttl: Duration,
        interval: Duration,
        push_cancel: CancellationToken,
    ) -> Self {
        Self::spawn_with_stop(
            store,
            lock_path,
            holder,
            ttl,
            interval,
            push_cancel.clone(),
            push_cancel,
        )
    }

    /// Spawn a heartbeat whose lifetime is independent from operation
    /// cancellation.
    ///
    /// `push_cancel` is still cancelled when the lock is stolen or lost, but
    /// cancelling it does not stop the heartbeat. The separate `stop_cancel`
    /// token is used by maintenance owners that must keep renewing while a
    /// cancelled blocking operation unwinds and releases its lock.
    pub fn spawn_with_stop(
        store: Store,
        lock_path: String,
        holder: String,
        ttl: Duration,
        interval: Duration,
        push_cancel: CancellationToken,
        stop_cancel: CancellationToken,
    ) -> Self {
        let interval = clamp_interval(interval, ttl);
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        let push_cancel_clone = push_cancel.clone();
        let stop_cancel_clone = stop_cancel.clone();

        let handle = tokio::spawn(async move {
            heartbeat_loop(
                store,
                lock_path,
                holder,
                ttl,
                interval,
                push_cancel_clone,
                stop_cancel_clone,
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
        // Join the current bounded renewal before the caller releases its
        // lock; dropping an in-flight request leaves its result uncertain.
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
    stop_cancel: CancellationToken,
    self_cancel: CancellationToken,
) {
    loop {
        // Sleep for the heartbeat interval, but wake early if either
        // the push or the heartbeat itself is cancelled.
        tokio::select! {
            () = tokio::time::sleep(interval) => {}
            () = self_cancel.cancelled() => {
                debug!(lock_path = %lock_path, "heartbeat stopped");
                return;
            }
            () = stop_cancel.cancelled() => {
                debug!(lock_path = %lock_path, "heartbeat stop requested");
                return;
            }
        }

        // After waking, check cancellation before doing any I/O.
        if self_cancel.is_cancelled() || stop_cancel.is_cancelled() {
            return;
        }

        match PushLock::renew_if_holder(store.inner(), &lock_path, &holder, ttl).await {
            Ok(()) => {
                debug!(lock_path = %lock_path, "heartbeat extended lock TTL");
            }
            Err(error) => {
                error!(
                    lock_path = %lock_path,
                    %error,
                    "push lock renewal failed, cancelling operation"
                );
                push_cancel.cancel();
                return;
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use crab_coordination::{PushLockPayload as LockPayload, unix_now};
    use object_store::memory::InMemory;
    use object_store::path::Path;
    use std::sync::Arc;

    fn memory_store() -> Store {
        let inner: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
        Store::new(inner)
    }

    async fn put_lock(store: &Store, path: &str, holder: &str, ttl_secs: u64) {
        let payload = LockPayload {
            holder: holder.to_owned(),
            expires_at: unix_now() + ttl_secs,
            lease_secs: ttl_secs,
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
            lease_secs: 300,
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
    async fn released_claim_cancels_heartbeat_without_resurrection() {
        let store = memory_store();
        let lock_path = "repo/locks/refs/heads/main/lock";
        let holder = "original-holder";
        let payload = LockPayload::released(holder);
        store
            .put(
                &Path::from(lock_path),
                Bytes::from(serde_json::to_vec(&payload).unwrap()),
            )
            .await
            .unwrap();
        let before = store.get_with_etag(&Path::from(lock_path)).await.unwrap();
        let cancel = CancellationToken::new();
        let heartbeat = LockHeartbeat::spawn(
            store.clone(),
            lock_path.to_owned(),
            holder.to_owned(),
            Duration::from_secs(300),
            Duration::from_secs(10),
            cancel.clone(),
        );
        tokio::time::sleep(Duration::from_secs(15)).await;
        heartbeat.stop().await;
        let after = store.get_with_etag(&Path::from(lock_path)).await.unwrap();

        assert!(
            cancel.is_cancelled(),
            "released ownership must stop the operation"
        );
        assert_eq!(after, before, "heartbeat must not rewrite a released claim");
    }

    #[tokio::test]
    #[ignore = "requires AWS_BUCKET, AWS_ENDPOINT_URL and credentials for a writable S3-compatible test bucket"]
    async fn s3_released_claim_rejects_cached_and_heartbeat_renewal() {
        let inner = object_store::aws::AmazonS3Builder::from_env()
            .with_bucket_name(std::env::var("AWS_BUCKET").unwrap())
            .with_endpoint(std::env::var("AWS_ENDPOINT_URL").unwrap())
            .with_allow_http(true)
            .build()
            .unwrap();
        let store = Store::new(Arc::new(inner));
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let prefix = format!(
            "qualification/lease-renewal-{}-{timestamp}",
            std::process::id()
        );
        println!("retained qualification prefix: {prefix}");
        let ttl = Duration::from_secs(60);
        let mut lock = PushLock::acquire_ref(store.inner(), &prefix, "refs/heads/main", ttl)
            .await
            .unwrap();
        PushLock::release_ref_if_holder(store.inner(), &prefix, "refs/heads/main", lock.holder())
            .await
            .unwrap();
        let path = Path::from(lock.path());
        let before = store.get_with_etag(&path).await.unwrap();
        let cached_result = lock.renew().await;
        let cancel = CancellationToken::new();
        let heartbeat = LockHeartbeat::spawn(
            store.clone(),
            lock.path().to_owned(),
            lock.holder().to_owned(),
            ttl,
            Duration::from_secs(10),
            cancel.clone(),
        );
        let cancelled = tokio::time::timeout(Duration::from_secs(15), cancel.cancelled()).await;
        heartbeat.stop().await;
        let after = store.get_with_etag(&path).await.unwrap();
        lock.release().await.unwrap();

        assert!(
            cached_result.is_err(),
            "released claim must reject cached renewal"
        );
        assert!(
            cancelled.is_ok(),
            "heartbeat must cancel after observing release"
        );
        assert_eq!(
            after, before,
            "neither renewal may rewrite the released claim"
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

    #[tokio::test(start_paused = true)]
    async fn independent_stop_keeps_heartbeat_after_operation_cancel() {
        let store = memory_store();
        let lock_path = "repo/locks/refs/heads/main/lock";
        let holder = "test-holder";
        let ttl = Duration::from_secs(300);

        put_lock(&store, lock_path, holder, 300).await;
        let (before, _) = store.get_with_etag(&Path::from(lock_path)).await.unwrap();
        let before: LockPayload = serde_json::from_slice(&before).unwrap();

        let push_cancel = CancellationToken::new();
        let stop_cancel = CancellationToken::new();
        let heartbeat = LockHeartbeat::spawn_with_stop(
            store.clone(),
            lock_path.to_owned(),
            holder.to_owned(),
            ttl,
            Duration::from_secs(10),
            push_cancel.clone(),
            stop_cancel.clone(),
        );

        push_cancel.cancel();
        tokio::time::sleep(Duration::from_secs(15)).await;

        let (body, _) = store.get_with_etag(&Path::from(lock_path)).await.unwrap();
        let after: LockPayload = serde_json::from_slice(&body).unwrap();
        heartbeat.stop().await;

        assert!(!stop_cancel.is_cancelled());
        assert!(after.expires_at >= before.expires_at);
    }
}
