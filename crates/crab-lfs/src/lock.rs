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

const MAX_LOCK_READ_CONCURRENCY: usize = 32;

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
        self.lock_with_expiry_mode(path, owner, expires_in, false)
            .await
    }

    /// Acquires a lock and reports an existing lock as a conflict, including
    /// when the current owner matches. This is the exclusive HTTP API
    /// contract; the native CLI keeps the idempotent `lock` behavior.
    pub async fn lock_exclusive(&self, path: &str, owner: &str) -> LockResult<LockRecord> {
        self.lock_with_expiry_mode(path, owner, None, true).await
    }

    async fn lock_with_expiry_mode(
        &self,
        path: &str,
        owner: &str,
        expires_in: Option<Duration>,
        same_owner_is_conflict: bool,
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
                if existing.owner == owner && !same_owner_is_conflict {
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
        self.force_unlock_with_id_inner(path, None).await
    }

    /// Releases a lock regardless of its owner while requiring the current
    /// record to have `lock_id`. The ID check is part of the same read/CAS
    /// sequence, so a stale force-unlock cannot release a replacement lock.
    pub async fn force_unlock_with_id(&self, path: &str, lock_id: &str) -> LockResult<LockRecord> {
        self.force_unlock_with_id_inner(path, Some(lock_id)).await
    }

    async fn force_unlock_with_id_inner(
        &self,
        path: &str,
        lock_id: Option<&str>,
    ) -> LockResult<LockRecord> {
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
        if lock_id.is_some_and(|expected| expected != record.id) {
            return Err(LfsLockError::IdMismatch {
                path: path.to_owned(),
            });
        }
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

    /// Lists a bounded, ID-sorted page of active locks.
    ///
    /// `limit` bounds the number of records retained while the object-store
    /// listing is scanned. The returned vector may contain fewer records when
    /// the filters do not match; callers can request one extra record to
    /// determine whether a next page exists.
    pub async fn list_page(
        &self,
        path: Option<&str>,
        id: Option<&str>,
        cursor: Option<&str>,
        limit: usize,
    ) -> LockResult<Vec<LockRecord>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let prefix = Path::from(self.namespace_path());
        let mut records = Vec::with_capacity(limit);
        let mut stream =
            self.store
                .inner()
                .list(Some(&prefix))
                .map(|result| async {
                    let object = result.map_err(|error| {
                        LfsLockError::Storage(crab_storage::map_object_store_error(
                            error,
                            prefix.as_ref(),
                        ))
                    })?;
                    match self.store.get_with_etag(&object.location).await {
                        Ok((body, _)) => Ok::<Option<LockRecord>, LfsLockError>(Some(
                            decode_record(&object.location, &body)?,
                        )),
                        Err(StorageError::NotFound { .. }) => Ok(None),
                        Err(error) => Err(LfsLockError::Storage(error)),
                    }
                })
                .buffered(MAX_LOCK_READ_CONCURRENCY);
        while let Some(result) = stream.next().await {
            let Some(record) = result? else {
                continue;
            };
            if is_released(&record)
                || is_expired(&record)
                || path.is_some_and(|expected| expected != record.path)
                || id.is_some_and(|expected| expected != record.id)
                || cursor.is_some_and(|value| record.id.as_str() <= value)
            {
                continue;
            }
            if id.is_some() {
                return Ok(vec![record]);
            }
            insert_page_record(&mut records, record, limit);
        }
        Ok(records)
    }

    /// Finds an active lock by its public ID.
    pub async fn find_by_id(&self, id: &str) -> LockResult<LockRecord> {
        self.list_page(None, Some(id), None, 1)
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| LfsLockError::NotFound {
                path: format!("lock id {id}"),
            })
    }

    /// Finds an active lock for a repository-relative path.
    pub async fn find_by_path(&self, path: &str) -> LockResult<LockRecord> {
        let object_path = Path::from(self.lock_path(path));
        let (body, _) =
            self.store
                .get_with_etag(&object_path)
                .await
                .map_err(|error| match error {
                    StorageError::NotFound { .. } => LfsLockError::NotFound {
                        path: path.to_owned(),
                    },
                    other => other.into(),
                })?;
        let record = decode_record(&object_path, &body)?;
        if record.path != path || is_released(&record) || is_expired(&record) {
            return Err(LfsLockError::NotFound {
                path: path.to_owned(),
            });
        }
        Ok(record)
    }

    /// Verifies all stored records and returns malformed object keys.
    pub async fn verify_locks(&self) -> LockResult<Vec<String>> {
        let prefix = Path::from(self.namespace_path());
        let mut invalid = Vec::new();
        let mut stream = self
            .store
            .inner()
            .list(Some(&prefix))
            .map(|result| async {
                let object = result.map_err(|error| {
                    LfsLockError::Storage(crab_storage::map_object_store_error(
                        error,
                        prefix.as_ref(),
                    ))
                })?;
                match self.store.get_with_etag(&object.location).await {
                    Ok((body, _)) => Ok::<Option<String>, LfsLockError>(
                        serde_json::from_slice::<LockRecord>(&body)
                            .is_err()
                            .then(|| object.location.to_string()),
                    ),
                    Err(StorageError::NotFound { .. }) => Ok(None),
                    Err(error) => Err(LfsLockError::Storage(error)),
                }
            })
            .buffered(MAX_LOCK_READ_CONCURRENCY);
        while let Some(result) = stream.next().await {
            if let Some(location) = result? {
                invalid.push(location);
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
        let mut stream = futures_util::stream::iter(paths.iter().map(|path| async move {
            let object_path = Path::from(self.lock_path(path));
            match self.store.get_with_etag(&object_path).await {
                Ok((body, _)) => {
                    let record = decode_record(&object_path, &body)?;
                    Ok::<Option<LockRecord>, LfsLockError>(
                        (!is_expired(&record) && !is_released(&record) && record.owner != owner)
                            .then_some(record),
                    )
                }
                Err(StorageError::NotFound { .. }) => Ok(None),
                Err(error) => Err(LfsLockError::Storage(error)),
            }
        }))
        .buffered(MAX_LOCK_READ_CONCURRENCY);
        while let Some(result) = stream.next().await {
            if let Some(record) = result? {
                conflicts.push(record);
            }
        }
        Ok(conflicts)
    }

    async fn list_records(&self, prefix: &Path) -> LockResult<Vec<LockRecord>> {
        let mut records = Vec::new();
        let mut stream =
            self.store
                .inner()
                .list(Some(prefix))
                .map(|result| async {
                    let object = result.map_err(|error| {
                        LfsLockError::Storage(crab_storage::map_object_store_error(
                            error,
                            prefix.as_ref(),
                        ))
                    })?;
                    match self.store.get_with_etag(&object.location).await {
                        Ok((body, _)) => Ok::<Option<LockRecord>, LfsLockError>(Some(
                            decode_record(&object.location, &body)?,
                        )),
                        Err(StorageError::NotFound { .. }) => Ok(None),
                        Err(error) => Err(LfsLockError::Storage(error)),
                    }
                })
                .buffered(MAX_LOCK_READ_CONCURRENCY);
        while let Some(result) = stream.next().await {
            if let Some(record) = result?
                && !is_released(&record)
            {
                records.push(record);
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

fn insert_page_record(records: &mut Vec<LockRecord>, record: LockRecord, limit: usize) {
    let index = records
        .binary_search_by(|current| current.id.cmp(&record.id))
        .unwrap_or_else(|index| index);
    records.insert(index, record);
    if records.len() > limit {
        records.pop();
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
#[expect(clippy::unwrap_used)]
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
            manager.lock_exclusive("model.bin", "alice").await,
            Err(LfsLockError::Conflict { owner, .. }) if owner == "alice"
        ));
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
    async fn force_unlock_id_mismatch_preserves_replacement_lock() {
        let manager = manager();
        let first = manager.lock("model.bin", "alice").await.unwrap();
        manager
            .unlock_with_id("model.bin", "alice", Some(&first.id))
            .await
            .unwrap();
        let replacement = manager.lock("model.bin", "bob").await.unwrap();

        assert!(matches!(
            manager.force_unlock_with_id("model.bin", &first.id).await,
            Err(LfsLockError::IdMismatch { .. })
        ));
        assert_eq!(
            manager.find_by_path("model.bin").await.unwrap(),
            replacement
        );
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
