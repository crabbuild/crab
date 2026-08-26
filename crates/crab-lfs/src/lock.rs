//! Git LFS file-lock records backed by object storage.
//!
//! The lock record format is shared by the CLI and HTTP LFS service. Records
//! live at `{prefix}/lfs/locks/{blake3(path)}` and are released with a CAS
//! tombstone so a stale unlock cannot remove a newer lock for the same path.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use futures_util::StreamExt;
use object_store::path::Path;
use serde::{Deserialize, Serialize};
use tracing::debug;

use crab_storage::{StorageError, Store};

/// Result alias for LFS lock operations.
pub type LockResult<T> = std::result::Result<T, LfsLockError>;

/// Errors raised by the shared LFS lock store.
#[derive(thiserror::Error, Debug)]
pub enum LfsLockError {
    /// A non-expired lock is held by another owner.
    #[error("LFS lock conflict for {path}; held by {owner}")]
    Conflict { path: String, owner: String },

    /// The requested lock does not exist or is already released.
    #[error("LFS lock not found: {path}")]
    NotFound { path: String },

    /// An unlock supplied a lock ID that no longer names the current holder.
    #[error("LFS lock ID does not match the current lock for {path}")]
    IdMismatch { path: String },

    /// A stored lock record could not be decoded safely.
    #[error("invalid LFS lock record at {path}: {reason}")]
    Corrupt { path: String, reason: String },

    /// Object-store operation failed.
    #[error(transparent)]
    Storage(#[from] StorageError),

    /// Lock record serialization failed.
    #[error("LFS lock serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
}

/// JSON record stored for one repository-relative file lock.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LockRecord {
    /// Repository-relative file path.
    pub path: String,
    /// Authenticated identity that owns the lock.
    pub owner: String,
    /// Unix timestamp in seconds when the lock was acquired.
    pub locked_at: u64,
    /// Stable lock identifier returned to Git LFS clients.
    pub id: String,
    /// Optional Unix expiry timestamp. `None` means no expiry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    /// Unix timestamp when this record was released, if it is a tombstone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub released_at: Option<u64>,
}

/// Shared object-store implementation for native and HTTP-compatible LFS
/// locking. The namespace is normally `lfs/locks`; native Crab locks use
/// `locks/files` through the same implementation.
pub struct LfsLockManager {
    store: Store,
    prefix: String,
    namespace: String,
}

impl LfsLockManager {
    /// Creates a manager with an explicit object-store namespace.
    #[must_use]
    pub fn new(store: Store, prefix: &str, namespace: &str) -> Self {
        Self {
            store,
            prefix: prefix.trim_matches('/').to_owned(),
            namespace: namespace.trim_matches('/').to_owned(),
        }
    }

    /// Creates a manager for Git LFS-compatible lock records.
    #[must_use]
    pub fn lfs(store: Store, prefix: &str) -> Self {
        Self::new(store, prefix, "lfs/locks")
    }

    /// Creates a manager for Crab's native file-lock records.
    #[must_use]
    pub fn native(store: Store, prefix: &str) -> Self {
        Self::new(store, prefix, "locks/files")
    }

    /// Acquires a non-expiring lock, or returns the existing lock for the same
    /// owner.
    pub async fn lock(&self, path: &str, owner: &str) -> LockResult<LockRecord> {
        self.lock_with_expiry(path, owner, None).await
    }

    /// Acquires a lock with an optional duration from the current time.
    pub async fn lock_with_expiry(
        &self,
        path: &str,
        owner: &str,
        expires_in: Option<Duration>,
    ) -> LockResult<LockRecord> {
        let key = self.lock_path(path);
        let object_path = Path::from(key.as_str());
        let now = unix_now();
        let record = LockRecord {
            path: path.to_owned(),
            owner: owner.to_owned(),
            locked_at: now,
            id: generate_lock_id(path, owner),
            expires_at: expires_in.map(|duration| now.saturating_add(duration.as_secs())),
            released_at: None,
        };
        let body = serde_json::to_vec(&record)?;

        match self.store.put(&object_path, Bytes::from(body)).await {
            Ok(()) => {
                debug!(path = %path, owner = %owner, "LFS lock acquired");
                Ok(record)
            }
            Err(StorageError::StateConflict { .. }) => {
                let (existing_body, etag) = self.store.get_with_etag(&object_path).await?;
                let existing = decode_record(&object_path, &existing_body)?;
                if is_expired(&existing) || is_released(&existing) {
                    let new_body = serde_json::to_vec(&record)?;
                    self.store
                        .update(&object_path, Bytes::from(new_body), etag)
                        .await?;
                    return Ok(record);
                }
                if existing.owner == owner {
                    return Ok(existing);
                }
                Err(LfsLockError::Conflict {
                    path: path.to_owned(),
                    owner: existing.owner,
                })
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Releases a lock after validating the owner and, when supplied, ID.
    pub async fn unlock_with_id(
        &self,
        path: &str,
        owner: &str,
        lock_id: Option<&str>,
    ) -> LockResult<LockRecord> {
        let object_path = Path::from(self.lock_path(path));
        let (body, etag) =
            self.store
                .get_with_etag(&object_path)
                .await
                .map_err(|error| match error {
                    StorageError::NotFound { .. } => LfsLockError::NotFound {
                        path: path.to_owned(),
                    },
                    other => other.into(),
                })?;
        let existing = decode_record(&object_path, &body)?;
        if is_released(&existing) {
            return Err(LfsLockError::NotFound {
                path: path.to_owned(),
            });
        }
        if existing.owner != owner {
            return Err(LfsLockError::Conflict {
                path: path.to_owned(),
                owner: existing.owner,
            });
        }
        if lock_id.is_some_and(|expected| expected != existing.id) {
            return Err(LfsLockError::IdMismatch {
                path: path.to_owned(),
            });
        }

        let mut released = existing;
        released.released_at = Some(unix_now());
        let released_body = serde_json::to_vec(&released)?;
        self.store
            .update(&object_path, Bytes::from(released_body), etag)
            .await?;
        Ok(released)
    }

    /// Releases a lock regardless of its owner. Missing and already released
    /// locks are treated as successful idempotent deletes.
    pub async fn force_unlock(&self, path: &str) -> LockResult<LockRecord> {
        let object_path = Path::from(self.lock_path(path));
        let (body, etag) = match self.store.get_with_etag(&object_path).await {
            Ok(result) => result,
            Err(StorageError::NotFound { .. }) => {
                return Err(LfsLockError::NotFound {
                    path: path.to_owned(),
                });
            }
            Err(error) => return Err(error.into()),
        };
        let mut record = decode_record(&object_path, &body)?;
        if is_released(&record) {
            return Ok(record);
        }
        record.released_at = Some(unix_now());
        let released_body = serde_json::to_vec(&record)?;
        self.store
            .update(&object_path, Bytes::from(released_body), etag)
            .await?;
        Ok(record)
    }

    /// Lists active, non-expired locks in this repository.
    pub async fn list(&self) -> LockResult<Vec<LockRecord>> {
        let prefix = Path::from(self.namespace_path());
        let records = self.list_records(&prefix).await?;
        Ok(records
            .into_iter()
            .filter(|record| !is_expired(record))
            .collect())
    }

    /// Lists all valid non-tombstone records, including expired locks.
    pub async fn list_all(&self) -> LockResult<Vec<LockRecord>> {
        let prefix = Path::from(self.namespace_path());
        self.list_records(&prefix).await
    }

    /// Finds an active lock by its public ID.
    pub async fn find_by_id(&self, id: &str) -> LockResult<LockRecord> {
        self.list()
            .await?
            .into_iter()
            .find(|record| record.id == id)
            .ok_or_else(|| LfsLockError::NotFound {
                path: format!("lock id {id}"),
            })
    }

    /// Finds an active lock for a repository-relative path.
    pub async fn find_by_path(&self, path: &str) -> LockResult<LockRecord> {
        self.list()
            .await?
            .into_iter()
            .find(|record| record.path == path)
            .ok_or_else(|| LfsLockError::NotFound {
                path: path.to_owned(),
            })
    }

    /// Verifies all stored records and returns malformed object keys.
    pub async fn verify_locks(&self) -> LockResult<Vec<String>> {
        let prefix = Path::from(self.namespace_path());
        let stream = self.store.inner().list(Some(&prefix));
        let objects = stream
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| {
                LfsLockError::Storage(crab_storage::map_object_store_error(error, prefix.as_ref()))
            })?;
        let mut invalid = Vec::new();
        for object in objects {
            match self.store.get_with_etag(&object.location).await {
                Ok((body, _)) if serde_json::from_slice::<LockRecord>(&body).is_ok() => {}
                Ok(_) => invalid.push(object.location.to_string()),
                Err(StorageError::NotFound { .. }) => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(invalid)
    }

    /// Returns active locks held by owners other than `owner` for `paths`.
    pub async fn check_conflicts(
        &self,
        paths: &[String],
        owner: &str,
    ) -> LockResult<Vec<LockRecord>> {
        let mut conflicts = Vec::new();
        for path in paths {
            let object_path = Path::from(self.lock_path(path));
            match self.store.get_with_etag(&object_path).await {
                Ok((body, _)) => {
                    let record = decode_record(&object_path, &body)?;
                    if !is_expired(&record) && !is_released(&record) && record.owner != owner {
                        conflicts.push(record);
                    }
                }
                Err(StorageError::NotFound { .. }) => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(conflicts)
    }

    async fn list_records(&self, prefix: &Path) -> LockResult<Vec<LockRecord>> {
        let stream = self.store.inner().list(Some(prefix));
        let objects = stream
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|error| {
                LfsLockError::Storage(crab_storage::map_object_store_error(error, prefix.as_ref()))
            })?;
        let mut records = Vec::new();
        for object in objects {
            match self.store.get_with_etag(&object.location).await {
                Ok((body, _)) => {
                    let record = decode_record(&object.location, &body)?;
                    if !is_released(&record) {
                        records.push(record);
                    }
                }
                Err(StorageError::NotFound { .. }) => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(records)
    }

    fn namespace_path(&self) -> String {
        if self.prefix.is_empty() {
            self.namespace.clone()
        } else {
            format!("{}/{}", self.prefix, self.namespace)
        }
    }

    fn lock_path(&self, path: &str) -> String {
        let hash = blake3::hash(path.as_bytes()).to_hex();
        format!("{}/{hash}", self.namespace_path())
    }
}

fn decode_record(path: &Path, body: &[u8]) -> LockResult<LockRecord> {
    serde_json::from_slice(body).map_err(|error| LfsLockError::Corrupt {
        path: path.to_string(),
        reason: error.to_string(),
    })
}

fn is_expired(record: &LockRecord) -> bool {
    record
        .expires_at
        .is_some_and(|expires_at| unix_now() >= expires_at)
}

fn is_released(record: &LockRecord) -> bool {
    record.released_at.is_some()
}

fn generate_lock_id(path: &str, owner: &str) -> String {
    let now = unix_now();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .subsec_nanos();
    let pid = std::process::id();
    let hash = blake3::hash(format!("{path}:{owner}:{now}:{nanos}:{pid}").as_bytes());
    let hex = hash.to_hex();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

#[cfg(test)]
#[expect(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use object_store::memory::InMemory;
    use std::sync::Arc;

    fn manager() -> LfsLockManager {
        LfsLockManager::lfs(Store::new(Arc::new(InMemory::new())), "repo")
    }

    #[tokio::test]
    async fn lock_cycle_and_conflict_are_cas_backed() {
        let manager = manager();
        let first = manager.lock("model.bin", "alice").await.unwrap();
        assert_eq!(manager.lock("model.bin", "alice").await.unwrap(), first);
        assert!(matches!(
            manager.lock("model.bin", "bob").await,
            Err(LfsLockError::Conflict { owner, .. }) if owner == "alice"
        ));
        manager
            .unlock_with_id("model.bin", "alice", Some(&first.id))
            .await
            .unwrap();
        assert!(manager.lock("model.bin", "bob").await.is_ok());
    }

    #[tokio::test]
    async fn list_filters_released_records() {
        let manager = manager();
        let record = manager.lock("model.bin", "alice").await.unwrap();
        manager
            .unlock_with_id("model.bin", "alice", Some(&record.id))
            .await
            .unwrap();
        assert!(manager.list().await.unwrap().is_empty());
    }
}
