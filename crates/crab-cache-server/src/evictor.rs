//! Background weighted-LRU evictor for cache storage management.
//!
//! Spawns a tokio task that periodically checks cache usage against the
//! high-water mark and evicts down to the low-water mark. The eviction
//! logic itself lives on [`CacheStore::evict_to_budget`] — this module
//! provides the background task wrapper.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;
use tracing::{debug, info, warn};

use crate::cache_store::CacheStore;

/// Handle for the background evictor task.
///
/// Dropping the handle does not cancel the task — call [`EvictorHandle::shutdown`]
/// or drop the `Arc<CacheStore>` to stop it.
pub struct EvictorHandle {
    /// Notify the evictor to run immediately (e.g. after a large write).
    notify: Arc<Notify>,
    /// Join handle for the background task.
    join: tokio::task::JoinHandle<()>,
}

impl EvictorHandle {
    /// Signal the evictor to check immediately rather than waiting for the
    /// next poll interval.
    pub fn nudge(&self) {
        self.notify.notify_one();
    }

    /// Get a clone of the notify handle for sharing with other components.
    pub fn notify_handle(&self) -> Arc<Notify> {
        Arc::clone(&self.notify)
    }

    /// Shut down the evictor task and wait for it to finish.
    pub async fn shutdown(self) {
        self.join.abort();
        let _ = self.join.await;
    }
}

/// Spawn the background evictor task.
///
/// The task wakes every `poll_interval` (or immediately when nudged) and
/// runs eviction if `current_bytes > max_bytes * high_water_ratio`.
pub fn start_evictor_task(
    cache_store: Arc<CacheStore>,
    high_water_ratio: f64,
    low_water_ratio: f64,
    poll_interval: Duration,
) -> EvictorHandle {
    let notify = Arc::new(Notify::new());
    let notify_clone = Arc::clone(&notify);

    let join = tokio::spawn(async move {
        info!(
            high_water_ratio,
            low_water_ratio,
            poll_secs = poll_interval.as_secs(),
            "evictor task started"
        );

        loop {
            // Wait for either the poll interval or an explicit nudge.
            tokio::select! {
                () = tokio::time::sleep(poll_interval) => {}
                () = notify_clone.notified() => {}
            }

            let high_water = (cache_store.max_bytes() as f64 * high_water_ratio) as u64;
            let current = cache_store.current_bytes();

            if current <= high_water {
                debug!(
                    current,
                    high_water, "below high-water mark, skipping eviction"
                );
                continue;
            }

            debug!(
                current,
                high_water, "above high-water mark, running eviction"
            );

            match cache_store.evict_to_budget(high_water_ratio, low_water_ratio) {
                Ok(stats) => {
                    if stats.evicted_count > 0 {
                        info!(
                            evicted_count = stats.evicted_count,
                            evicted_bytes = stats.evicted_bytes,
                            current_bytes = cache_store.current_bytes(),
                            "eviction run complete"
                        );
                    }
                }
                Err(e) => {
                    warn!(error = %e, "eviction run failed");
                }
            }
        }
    });

    EvictorHandle { notify, join }
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]
mod tests {
    use crate::cache_store::{CacheStore, ObjectType, ServerObjectKey};
    use crate::db::{CACHE_DB_FILE, CacheDb};
    use bytes::Bytes;

    fn test_store(max_bytes: u64) -> CacheStore {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let db = CacheDb::open_or_create(&root.join(CACHE_DB_FILE)).unwrap();
        let conn = db.connect().unwrap();
        std::mem::forget(dir);
        CacheStore::open(root, max_bytes, conn).unwrap()
    }

    fn put_object(store: &CacheStore, data: &[u8], object_type: ObjectType) -> ServerObjectKey {
        let bytes = Bytes::copy_from_slice(data);
        let hash = blake3::hash(&bytes);
        let hash_hex = hash.to_hex().to_string();
        let key = ServerObjectKey {
            bucket: "b".to_string(),
            repo_path: "r".to_string(),
            object_type,
            hash: hash_hex,
        };
        store.put(&key, bytes, hash.as_bytes()).unwrap();
        key
    }

    #[test]
    fn evict_to_budget_reduces_below_low_water() {
        // max_bytes = 100, high water = 95, low water = 90.
        let store = test_store(100);

        // Insert objects totaling > 95 bytes.
        // Each unique data blob gets a unique hash.
        for i in 0u8..10 {
            let data = vec![i; 10];
            put_object(&store, &data, ObjectType::Xorb);
        }
        assert_eq!(store.current_bytes(), 100);

        let stats = store.evict_to_budget(0.95, 0.90).unwrap();
        assert!(stats.evicted_count > 0);
        assert!(
            store.current_bytes() <= 90,
            "should be at or below low-water mark"
        );
    }

    #[test]
    fn evict_to_budget_noop_when_below_high_water() {
        let store = test_store(1000);
        put_object(&store, b"small", ObjectType::Xorb);
        assert_eq!(store.current_bytes(), 5);

        let stats = store.evict_to_budget(0.95, 0.90).unwrap();
        assert_eq!(stats.evicted_count, 0);
        assert_eq!(stats.evicted_bytes, 0);
    }

    #[test]
    fn eviction_prefers_xorbs_over_shards() {
        // max_bytes = 100, fill with 50 bytes xorb + 50 bytes shard.
        let store = test_store(100);

        // Insert both at the same logical time — type weight should break ties.
        // Insert xorb first, then shard. Both get similar timestamps.
        let xorb_data = vec![0xBB; 50];
        let _xorb_key = put_object(&store, &xorb_data, ObjectType::Xorb);

        let shard_data = vec![0xAA; 50];
        let shard_key = put_object(&store, &shard_data, ObjectType::Shard);

        assert_eq!(store.current_bytes(), 100);

        // Touch the shard to give it a newer last_access, ensuring the xorb
        // is strictly older. Even without this, type weight (xorb=0 < shard=3)
        // should break ties, but the explicit touch makes the test robust
        // against sub-millisecond insertion timing.
        let _ = store.get(&shard_key).unwrap();

        // Evict down to 90 bytes — should evict the xorb (older + lower weight).
        let stats = store.evict_to_budget(0.95, 0.90).unwrap();
        assert!(stats.evicted_count >= 1);

        // The shard should still be readable.
        let shard_bytes = store.get(&shard_key).unwrap();
        assert!(shard_bytes.is_some(), "shard should survive eviction");
    }

    #[test]
    fn remove_object_frees_bytes() {
        let store = test_store(1000);
        let data = vec![0u8; 42];
        let bytes = Bytes::copy_from_slice(&data);
        let hash = blake3::hash(&bytes);
        let hash_hex = hash.to_hex().to_string();
        let key = ServerObjectKey {
            bucket: "b".to_string(),
            repo_path: "r".to_string(),
            object_type: ObjectType::Xorb,
            hash: hash_hex,
        };
        store.put(&key, bytes, hash.as_bytes()).unwrap();
        assert_eq!(store.current_bytes(), 42);

        let mut meta_key = [0u8; 33];
        meta_key[0] = ObjectType::Xorb.as_u8();
        meta_key[1..].copy_from_slice(hash.as_bytes());

        let freed = store.remove_object(&meta_key).unwrap();
        assert_eq!(freed, 42);
        assert_eq!(store.current_bytes(), 0);

        // Object should be gone.
        assert!(store.get(&key).unwrap().is_none());
    }

    #[test]
    fn remove_object_missing_returns_zero() {
        let store = test_store(1000);
        let meta_key = [0u8; 33];
        let freed = store.remove_object(&meta_key).unwrap();
        assert_eq!(freed, 0);
    }

    #[test]
    fn remaining_objects_readable_after_eviction() {
        let store = test_store(100);

        // Insert 10 objects of 10 bytes each = 100 bytes total.
        let mut keys = Vec::new();
        for i in 0u8..10 {
            let data = vec![i; 10];
            let key = put_object(&store, &data, ObjectType::Xorb);
            keys.push((key, data));
        }

        // Evict down to 90 bytes.
        store.evict_to_budget(0.95, 0.90).unwrap();

        // All remaining objects should be readable and correct.
        for (key, data) in &keys {
            if let Some(got) = store.get(key).unwrap() {
                assert_eq!(got.as_ref(), data.as_slice());
            }
        }
    }
}
