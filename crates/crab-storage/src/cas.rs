//! Compare-and-swap update loop for JSON objects.
//!
//! The CAS loop is storage-owned because it composes object-store reads,
//! conditional creates, conditional updates, retry backoff, and storage-domain
//! error classification. Higher domains provide the JSON payload type and
//! mutation function.

use std::time::Duration;

use bytes::Bytes;
use object_store::path::Path;
use rand::Rng;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::error::{Result, StorageError};
use crate::store::Store;

/// Default maximum CAS attempts before returning `StateConflict`.
pub const DEFAULT_MAX_ATTEMPTS: u32 = 10;

/// Maximum JSON object size loaded or written by the default CAS loop.
///
/// CAS objects include shared ref registries and other coordination metadata.
/// Keeping this limit in the storage layer prevents a malformed or unbounded
/// coordination object from turning every CAS caller into an arbitrary-memory
/// read. Callers with a smaller domain-specific limit should use
/// [`cas_update_bounded`].
pub const MAX_CAS_OBJECT_BYTES: u64 = 512 * 1024 * 1024;

const CAS_BACKOFF_BASE: Duration = Duration::from_millis(50);
const CAS_BACKOFF_CAP: Duration = Duration::from_millis(500);

/// Updates one JSON object with a load, mutate, conditional-write loop.
///
/// Missing objects start from `T::default()` and use a conditional create.
/// Existing objects use the ETag returned by the read and a conditional update.
///
/// # Errors
///
/// Returns [`StorageError::StateConflict`] if all attempts are exhausted.
/// Returns [`StorageError::CorruptObject`] when the existing object is not
/// valid JSON for `T`.
pub async fn cas_update<T, F>(store: &Store, path: &str, max_attempts: u32, mutate: F) -> Result<T>
where
    T: Serialize + for<'de> Deserialize<'de> + Default + Clone,
    F: Fn(&mut T),
{
    cas_update_bounded(store, path, max_attempts, MAX_CAS_OBJECT_BYTES, mutate).await
}

/// Updates one JSON object with an explicit read/write size ceiling.
///
/// Existing objects are rejected before their body is consumed when the
/// provider advertises a size above `max_bytes`. Newly serialized values are
/// checked before the conditional write as well, so a successful update can
/// never create an object the next CAS attempt would refuse to read.
///
/// # Errors
///
/// Returns [`StorageError::CorruptObject`] when an existing or newly serialized
/// object exceeds `max_bytes`.
pub async fn cas_update_bounded<T, F>(
    store: &Store,
    path: &str,
    max_attempts: u32,
    max_bytes: u64,
    mutate: F,
) -> Result<T>
where
    T: Serialize + for<'de> Deserialize<'de> + Default + Clone,
    F: Fn(&mut T),
{
    let obj_path = Path::from(path);
    let max = if max_attempts == 0 {
        DEFAULT_MAX_ATTEMPTS
    } else {
        max_attempts
    };

    for attempt in 0..max {
        let (mut value, etag) = match store.get_with_etag_bounded(&obj_path, max_bytes).await {
            Ok((body, etag)) => {
                let parsed: T = serde_json::from_slice(&body).map_err(|source| {
                    StorageError::CorruptObject {
                        path: path.to_owned(),
                        reason: format!("invalid JSON: {source}"),
                    }
                })?;
                (parsed, Some(etag))
            }
            Err(StorageError::NotFound { .. }) => (T::default(), None),
            Err(error) => return Err(error),
        };

        mutate(&mut value);
        let new_body = serde_json::to_vec(&value)
            .map_err(|source| StorageError::Internal(format!("CAS serialize: {source}")))?;
        let new_size = u64::try_from(new_body.len()).unwrap_or(u64::MAX);
        if new_size > max_bytes {
            return Err(StorageError::CorruptObject {
                path: path.to_owned(),
                reason: format!(
                    "serialized CAS object is {new_size} bytes; bounded update supports at most {max_bytes} bytes"
                ),
            });
        }
        let new_bytes = Bytes::from(new_body);

        let write_result = match etag {
            Some(tag) => store.update(&obj_path, new_bytes, tag).await.map(|_| ()),
            None => store.create_strict(&obj_path, new_bytes).await,
        };

        match write_result {
            Ok(()) => {
                debug!(path = %path, attempt, "CAS update succeeded");
                return Ok(value);
            }
            Err(StorageError::StateConflict { .. }) => {
                if attempt + 1 < max {
                    let delay = cas_jitter(attempt);
                    debug!(path = %path, attempt, delay_ms = delay.as_millis(), "CAS conflict, retrying");
                    tokio::time::sleep(delay).await;
                } else {
                    warn!(path = %path, attempts = max, "CAS update exhausted all attempts");
                }
            }
            Err(error) => return Err(error),
        }
    }

    Err(StorageError::StateConflict {
        path: path.to_owned(),
    })
}

/// Updates one JSON object using the default CAS attempt budget.
///
/// # Errors
///
/// Returns [`StorageError::StateConflict`] if all attempts are exhausted.
pub async fn cas_update_default<T, F>(store: &Store, path: &str, mutate: F) -> Result<T>
where
    T: Serialize + for<'de> Deserialize<'de> + Default + Clone,
    F: Fn(&mut T),
{
    cas_update(store, path, DEFAULT_MAX_ATTEMPTS, mutate).await
}

fn cas_jitter(attempt: u32) -> Duration {
    let shift = 1u32.checked_shl(attempt).unwrap_or(u32::MAX);
    let exp = CAS_BACKOFF_BASE.saturating_mul(shift);
    let bound = exp.min(CAS_BACKOFF_CAP);
    let bound_nanos = u64::try_from(bound.as_nanos()).unwrap_or(u64::MAX);
    if bound_nanos == 0 {
        return Duration::ZERO;
    }
    let pick = rand::rng().random_range(0..=bound_nanos);
    Duration::from_nanos(pick)
}

#[cfg(test)]
#[expect(clippy::unwrap_used, clippy::expect_used, reason = "test assertions")]
mod tests {
    use std::sync::Arc;

    use object_store::memory::InMemory;
    use serde::{Deserialize, Serialize};

    use super::*;

    #[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
    struct TestManifest {
        generation: u64,
        entries: Vec<String>,
    }

    fn memory_store() -> Store {
        let inner: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
        Store::new(inner)
    }

    #[tokio::test]
    async fn cas_creates_new_json_object() {
        let store = memory_store();
        let result = cas_update::<TestManifest, _>(&store, "repo/shard-list", 5, |manifest| {
            manifest.generation += 1;
            manifest.entries.push("shard-aaa".to_owned());
        })
        .await
        .unwrap();

        assert_eq!(result.generation, 1);
        assert_eq!(result.entries, vec!["shard-aaa"]);
    }

    #[tokio::test]
    async fn cas_updates_existing_json_object() {
        let store = memory_store();

        let _: TestManifest =
            cas_update::<TestManifest, _>(&store, "repo/shard-list", 5, |manifest| {
                manifest.generation += 1;
                manifest.entries.push("shard-aaa".to_owned());
            })
            .await
            .unwrap();

        let result = cas_update::<TestManifest, _>(&store, "repo/shard-list", 5, |manifest| {
            manifest.generation += 1;
            manifest.entries.push("shard-bbb".to_owned());
        })
        .await
        .unwrap();

        assert_eq!(result.generation, 2);
        assert_eq!(result.entries.len(), 2);
    }

    #[tokio::test]
    async fn bounded_cas_rejects_oversized_existing_object() {
        let store = memory_store();
        let path = Path::from("repo/oversized-cas");
        store
            .put(&path, Bytes::from_static(b"0123456789"))
            .await
            .unwrap();

        let error = cas_update_bounded::<TestManifest, _>(&store, path.as_ref(), 3, 9, |_| {})
            .await
            .expect_err("bounded CAS must reject an oversized existing object");

        assert!(matches!(error, StorageError::CorruptObject { .. }));
    }

    #[tokio::test]
    async fn bounded_cas_rejects_oversized_serialized_update() {
        let store = memory_store();
        let path = "repo/oversized-cas-write";

        let error = cas_update_bounded::<TestManifest, _>(&store, path, 3, 8, |manifest| {
            manifest.entries.push("0123456789".to_owned())
        })
        .await
        .expect_err("bounded CAS must reject an oversized serialized update");

        assert!(matches!(error, StorageError::CorruptObject { .. }));
        assert!(matches!(
            store.get_with_etag(&Path::from(path)).await,
            Err(StorageError::NotFound { .. })
        ));
    }

    #[tokio::test]
    async fn cas_jitter_bounded_by_cap() {
        for attempt in 0..20 {
            let delay = cas_jitter(attempt);
            assert!(
                delay <= CAS_BACKOFF_CAP,
                "attempt {attempt}: {delay:?} exceeds cap"
            );
        }
    }
}
