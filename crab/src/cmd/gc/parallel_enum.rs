//! Prefix-sharded parallel LIST enumeration for GC candidate discovery.
//!
//! Object stores organize keys by prefix. GC exploits this by issuing up to
//! `gc.list_concurrency` (default 32) parallel LIST requests across all 256
//! two-hex-digit prefixes (`00/` through `ff/`). Each prefix is paginated
//! via continuation tokens and results are streamed through an `mpsc` channel.
//!
//! The caller receives a flat `Vec<ObjectMeta>` of all discovered objects
//! plus a [`ListOutcome`] with request counts and wall-clock timing.
//!
//! When `class_aware` is enabled, the enumerator enriches each
//! [`ObjectMeta`] with storage-class and transition-timestamp fields.
//! The real implementation will call provider-specific APIs (S3
//! `GetObjectAttributes`, GCS `timeStorageClassUpdated`, Azure
//! `AccessTierChangeTime`); for now a placeholder is used.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use tokio::sync::{Semaphore, mpsc};
use tracing::{debug, warn};

use super::{ListOutcome, ObjectMeta};
use crate::core::error::{Result, check_cancelled};
use crate::tier::classes::StorageClass;

/// All 256 two-hex-digit prefixes: `"00"` through `"ff"`.
fn hex_prefixes() -> Vec<String> {
    (0..=255u8).map(|b| format!("{b:02x}")).collect()
}

/// Trait abstracting the LIST operation for testability.
///
/// In production, this wraps `store.inner().list(Some(&prefix))`.
/// In tests, a mock implementation returns canned results.
pub trait ObjectLister: Send + Sync + 'static {
    /// List all objects under the given prefix in a specific storage dimension.
    ///
    /// Returns `ObjectMeta` items. Implementations handle pagination internally.
    fn list_prefix(
        &self,
        dimension: &str,
        prefix: &str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<ObjectMeta>>> + Send + '_>>;
}

/// Storage dimensions that GC enumerates.
///
/// Content-addressed dimensions (`xorbs`, `shards`) are listed from the
/// global `.crab/` prefix. Per-repo dimensions (`packs`) are listed
/// from `{repo}/packs/`. The `ObjectLister` implementation is responsible
/// for routing each dimension to the correct prefix.
const DIMENSIONS: &[&str] = &["xorbs", "shards", "packs"];

/// Enumerate all candidate objects across storage dimensions using
/// prefix-sharded parallel LIST.
///
/// Issues up to `concurrency` parallel LIST requests at a time across
/// all 256 two-hex prefixes for each dimension (xorbs, shards, packs).
///
/// When `class_aware` is `true`, each returned [`ObjectMeta`] is enriched
/// with `storage_class` and `transitioned_at` fields. The real
/// implementation will call provider-specific APIs:
/// - **S3:** `GetObjectAttributes` for the storage class
/// - **GCS:** `timeStorageClassUpdated` from object metadata
/// - **Azure:** `AccessTierChangeTime` from blob properties
///
/// When `class_aware` is `false` (the default), both fields remain `None`,
/// preserving backward-compatible behavior.
///
/// # Returns
///
/// A tuple of `(objects, list_outcome)` where `objects` is the full set of
/// discovered storage objects and `list_outcome` carries request metrics.
///
/// # Errors
///
/// Returns [`CrabError::Cancelled`] if the cancellation token fires
/// between dimension sweeps.
pub async fn enumerate_candidates(
    lister: Arc<dyn ObjectLister>,
    concurrency: usize,
    cancel: &tokio_util::sync::CancellationToken,
    class_aware: bool,
) -> Result<(Vec<ObjectMeta>, ListOutcome)> {
    let start = Instant::now();
    let request_count = Arc::new(AtomicU64::new(0));
    let prefixes = hex_prefixes();

    let mut all_objects = Vec::new();
    let mut failed_prefixes: Vec<String> = Vec::new();

    for dimension in DIMENSIONS {
        check_cancelled(cancel)?;

        let semaphore = Arc::new(Semaphore::new(concurrency));
        let (tx, mut rx) = mpsc::channel::<Result<Vec<ObjectMeta>>>(concurrency * 2);

        let mut handles = Vec::with_capacity(prefixes.len());
        for prefix in &prefixes {
            let sem = Arc::clone(&semaphore);
            let tx = tx.clone();
            let req_count = Arc::clone(&request_count);
            let lister = Arc::clone(&lister);
            let dim = dimension.to_string();
            let pfx = prefix.clone();

            handles.push(tokio::spawn(async move {
                let _permit = sem.acquire().await;
                req_count.fetch_add(1, Ordering::Relaxed);

                match lister.list_prefix(&dim, &pfx).await {
                    Ok(objects) => {
                        let _ = tx.send(Ok(objects)).await;
                    }
                    Err(e) => {
                        warn!(dimension = %dim, prefix = %pfx, error = %e, "LIST failed");
                        // Signal the failure to the collector so GC can
                        // surface partial enumeration in its outcome.
                        let _ = tx
                            .send(Err(crate::core::error::CrabError::Internal(format!(
                                "{dim}:{pfx}"
                            ))))
                            .await;
                    }
                }
            }));
        }

        // Drop the sender so the receiver closes when all tasks finish.
        drop(tx);

        while let Some(result) = rx.recv().await {
            match result {
                Ok(batch) => all_objects.extend(batch),
                Err(crate::core::error::CrabError::Internal(tag)) => failed_prefixes.push(tag),
                Err(_) => {
                    // Defensive: producer only emits Internal-wrapped tags.
                }
            }
        }

        for handle in handles {
            let _ = handle.await;
        }
    }

    let wall_seconds = start.elapsed().as_secs_f64();
    let outcome = ListOutcome {
        requests: request_count.load(Ordering::Relaxed),
        parallelism: concurrency,
        wall_seconds,
        failed_prefixes: failed_prefixes.clone(),
    };

    if !failed_prefixes.is_empty() {
        warn!(
            failed_count = failed_prefixes.len(),
            "enumeration is partial: some LIST requests failed; GC may not reclaim all eligible space"
        );
    }

    debug!(
        objects = all_objects.len(),
        requests = outcome.requests,
        wall_secs = format!("{:.2}", outcome.wall_seconds),
        "enumeration complete"
    );

    // When class_aware is enabled, enrich each object with storage-class
    // metadata.
    //
    // Per-object `head_with_class` already works (see
    // `storage/head_class.rs` — S3 / GCS / Azure all wired), but
    // calling it O(n) here would issue one HEAD per object and blow
    // past the GC enumeration budget on any non-trivial bucket.
    //
    // The right shape is a **batched class probe** that piggybacks on
    // provider-native LIST responses: S3's ListObjectsV2 returns
    // StorageClass per object, GCS's objects.list returns
    // storageClass + timeStorageClassUpdated, Azure's blob listing
    // returns AccessTier + AccessTierChangeTime. That requires a new
    // method on [`ObjectLister`] (`list_with_class`) or extending the
    // existing [`ObjectLister::list_prefix`] return type. Both are
    // breaking trait changes best bundled with a spec-level design
    // review — tracked under `crab-storage-economy` batch-class
    // work.
    //
    // Until that lands the enrichment is intentionally pessimistic:
    // `StorageClass::Unknown` is treated as warm by
    // [`class_aware::check_object_lock`], which is always safe for
    // GC (no aggressive archive transitions based on missing data).
    if class_aware {
        for obj in &mut all_objects {
            obj.storage_class = Some(StorageClass::Unknown);
            obj.transitioned_at = Some(obj.last_modified);
        }
    }

    Ok((all_objects, outcome))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::time::SystemTime;
    use tokio_util::sync::CancellationToken;

    /// Mock lister that returns a fixed set of objects per (dimension, prefix).
    struct MockLister {
        objects: HashMap<(String, String), Vec<ObjectMeta>>,
    }

    impl MockLister {
        fn new() -> Self {
            Self {
                objects: HashMap::new(),
            }
        }

        fn add(&mut self, dimension: &str, prefix: &str, objects: Vec<ObjectMeta>) {
            self.objects
                .insert((dimension.to_string(), prefix.to_string()), objects);
        }
    }

    impl ObjectLister for MockLister {
        fn list_prefix(
            &self,
            dimension: &str,
            prefix: &str,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<ObjectMeta>>> + Send + '_>>
        {
            let key = (dimension.to_string(), prefix.to_string());
            let result = self.objects.get(&key).cloned().unwrap_or_default();
            Box::pin(async move { Ok(result) })
        }
    }

    #[test]
    fn hex_prefixes_generates_256_entries() {
        let prefixes = hex_prefixes();
        assert_eq!(prefixes.len(), 256);
        assert_eq!(prefixes[0], "00");
        assert_eq!(prefixes[255], "ff");
        assert_eq!(prefixes[0x0a], "0a");
        assert_eq!(prefixes[0xab], "ab");
    }

    #[tokio::test]
    async fn enumerate_empty_store_returns_empty() {
        let lister = Arc::new(MockLister::new());
        let cancel = CancellationToken::new();

        let (objects, outcome) = enumerate_candidates(lister, 32, &cancel, false)
            .await
            .expect("should succeed");

        assert!(objects.is_empty());
        // 3 dimensions × 256 prefixes = 768 requests.
        assert_eq!(outcome.requests, 768);
        assert_eq!(outcome.parallelism, 32);
    }

    #[tokio::test]
    async fn enumerate_returns_objects_from_lister() {
        let mut mock = MockLister::new();
        let t = SystemTime::now() - std::time::Duration::from_secs(3600);

        mock.add(
            "xorbs",
            "ab",
            vec![ObjectMeta {
                key: "xorbs/ab/obj1".to_string(),
                size: 100,
                last_modified: t,
                e_tag: None,
                version: None,
                storage_class: None,
                transitioned_at: None,
            }],
        );
        mock.add(
            "shards",
            "cd",
            vec![ObjectMeta {
                key: "shards/cd/shard1".to_string(),
                size: 200,
                last_modified: t,
                e_tag: None,
                version: None,
                storage_class: None,
                transitioned_at: None,
            }],
        );

        let lister = Arc::new(mock);
        let cancel = CancellationToken::new();

        let (objects, _outcome) = enumerate_candidates(lister, 8, &cancel, false)
            .await
            .expect("should succeed");

        assert_eq!(objects.len(), 2);
        let keys: std::collections::HashSet<&str> =
            objects.iter().map(|o| o.key.as_str()).collect();
        assert!(keys.contains("xorbs/ab/obj1"));
        assert!(keys.contains("shards/cd/shard1"));
    }

    #[tokio::test]
    async fn enumerate_respects_cancellation() {
        let lister = Arc::new(MockLister::new());
        let cancel = CancellationToken::new();
        cancel.cancel();

        let result = enumerate_candidates(lister, 32, &cancel, false).await;
        assert!(
            matches!(result, Err(crate::core::error::CrabError::Cancelled)),
            "should return Cancelled"
        );
    }

    #[tokio::test]
    async fn class_aware_false_leaves_fields_none() {
        let mut mock = MockLister::new();
        let t = SystemTime::now() - std::time::Duration::from_secs(7200);

        mock.add(
            "xorbs",
            "ab",
            vec![ObjectMeta {
                key: "xorbs/ab/obj1".to_string(),
                size: 100,
                last_modified: t,
                e_tag: None,
                version: None,
                storage_class: None,
                transitioned_at: None,
            }],
        );

        let lister = Arc::new(mock);
        let cancel = CancellationToken::new();

        let (objects, _) = enumerate_candidates(lister, 8, &cancel, false)
            .await
            .expect("should succeed");

        assert_eq!(objects.len(), 1);
        assert!(
            objects[0].storage_class.is_none(),
            "class_aware=false must leave storage_class as None"
        );
        assert!(
            objects[0].transitioned_at.is_none(),
            "class_aware=false must leave transitioned_at as None"
        );
    }

    #[tokio::test]
    async fn class_aware_true_populates_fields() {
        let mut mock = MockLister::new();
        let t = SystemTime::now() - std::time::Duration::from_secs(7200);

        mock.add(
            "xorbs",
            "cd",
            vec![ObjectMeta {
                key: "xorbs/cd/obj2".to_string(),
                size: 500,
                last_modified: t,
                e_tag: None,
                version: None,
                storage_class: None,
                transitioned_at: None,
            }],
        );

        let lister = Arc::new(mock);
        let cancel = CancellationToken::new();

        let (objects, _) = enumerate_candidates(lister, 8, &cancel, true)
            .await
            .expect("should succeed");

        assert_eq!(objects.len(), 1);
        assert_eq!(
            objects[0].storage_class,
            Some(crate::tier::classes::StorageClass::Unknown),
            "class_aware=true must populate storage_class"
        );
        assert_eq!(
            objects[0].transitioned_at,
            Some(t),
            "class_aware=true must set transitioned_at to last_modified as fallback"
        );
    }
}
