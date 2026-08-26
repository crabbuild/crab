//! Advisory file locking backed by object storage.
//!
//! Manages file-level advisory locks in cloud object storage using
//! compare-and-swap (CAS) operations to prevent race conditions.
//! Lock records are stored as JSON at `{prefix}/{namespace}/{path-hash}`.
//!
//! Two namespaces are used:
//! - `locks/files` — native crab locks (`crab lock/unlock`)
//! - `lfs/locks`   — Git LFS compatibility (`crab lfs lock/unlock`)

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use futures_util::StreamExt;
use object_store::path::Path;
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::core::error::{CrabError, Result};
use crate::storage::store::Store;

/// JSON payload stored in the lock file in object storage.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LockRecord {
    /// The repository-relative path of the locked file.
    pub path: String,
    /// Identity of the lock owner (e.g. email or username).
    pub owner: String,
    /// Unix timestamp (seconds) when the lock was created.
    pub locked_at: u64,
    /// Unique lock identifier.
    pub id: String,
    /// Optional Unix timestamp (seconds) when the lock expires.
    /// `None` means the lock never expires.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    /// Unix timestamp when this record was released through a CAS transition.
    /// Released records are tombstones and are not returned as active locks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub released_at: Option<u64>,
}

/// Advisory file lock manager backed by object storage.
///
/// Lock records live at `{prefix}/{namespace}/{blake3-hash-of-path}`.
/// Creation uses `PutMode::Create` (via [`Store::put`]) for atomicity;
/// release reads the record and uses an etag-checked CAS tombstone so a
/// stale unlock cannot delete a newer lock.
///
/// Use [`LockManager::native`] for `crab lock/unlock` (stores at
/// `locks/files/`) or [`LockManager::lfs`] for LFS compatibility
/// (stores at `lfs/locks/`).
pub struct LockManager {
    store: Store,
    prefix: String,
    /// The sub-path between prefix and the hash, e.g. "locks/files" or "lfs/locks".
    namespace: String,
}

impl LockManager {
    /// Creates a new lock manager with a custom namespace.
    ///
    /// Prefer [`LockManager::native`] or [`LockManager::lfs`] for
    /// standard use cases.
    #[must_use]
    pub fn new(store: Store, prefix: &str, namespace: &str) -> Self {
        Self {
            store,
            prefix: prefix.to_string(),
            namespace: namespace.to_string(),
        }
    }

    /// Creates a lock manager for native crab locks.
    ///
    /// Stores lock records at `{prefix}/locks/files/{hash}`.
    #[must_use]
    pub fn native(store: Store, prefix: &str) -> Self {
        Self::new(store, prefix, "locks/files")
    }

    /// Creates a lock manager for Git LFS compatibility.
    ///
    /// Stores lock records at `{prefix}/lfs/locks/{hash}`.
    #[must_use]
    pub fn lfs(store: Store, prefix: &str) -> Self {
        Self::new(store, prefix, "lfs/locks")
    }

    /// Creates an advisory lock for `path` owned by `owner`.
    ///
    /// If the path is already locked by the same owner, returns the
    /// existing lock record. If locked by a different owner with a
    /// non-expired lock, returns [`CrabError::LfsLockConflict`].
    /// If locked by a different owner with an expired lock, the old
    /// lock is replaced atomically.
    ///
    /// Uses CAS (create-if-not-exists via `PutMode::Create`) for mutual
    /// exclusion between concurrent lock requests.
    ///
    /// # Errors
    ///
    /// Returns [`CrabError::LfsLockConflict`] if the file is locked
    /// by another owner. Propagates storage errors.
    pub async fn lock(&self, path: &str, owner: &str) -> Result<LockRecord> {
        self.lock_with_expiry(path, owner, None).await
    }

    /// Creates an advisory lock with an optional expiration duration.
    ///
    /// `expires_in` is the duration from now until the lock expires.
    /// `None` means the lock never expires.
    pub async fn lock_with_expiry(
        &self,
        path: &str,
        owner: &str,
        expires_in: Option<Duration>,
    ) -> Result<LockRecord> {
        let key = self.lock_path(path);
        let obj_path = Path::from(key.as_str());

        let now = unix_now();
        let expires_at = expires_in.map(|d| now.saturating_add(d.as_secs()));

        let record = LockRecord {
            path: path.to_string(),
            owner: owner.to_string(),
            locked_at: now,
            id: generate_lock_id(path, owner),
            expires_at,
            released_at: None,
        };

        let body = serde_json::to_vec(&record)
            .map_err(|e| CrabError::Internal(format!("lock serialize: {e}")))?;

        match self.store.put(&obj_path, Bytes::from(body)).await {
            Ok(()) => {
                debug!(path = %path, owner = %owner, expires = ?expires_at, "LFS lock acquired");
                Ok(record)
            }
            Err(CrabError::CasConflict { .. }) => {
                // Object already exists — check if expired or same owner.
                let (existing_body, etag) = self.store.get_with_etag(&obj_path).await?;
                let existing: LockRecord = serde_json::from_slice(&existing_body)
                    .map_err(|e| CrabError::Internal(format!("lock deserialize: {e}")))?;

                // If the existing lock is expired, replace it.
                if Self::is_expired(&existing) || Self::is_released(&existing) {
                    debug!(path = %path, "replacing expired lock held by {}", existing.owner);
                    let new_body = serde_json::to_vec(&record)
                        .map_err(|e| CrabError::Internal(format!("lock serialize: {e}")))?;
                    // The inspected expired record must still be current.
                    // A delete/create gap could erase a racing new owner.
                    self.store
                        .update(&obj_path, Bytes::from(new_body), etag)
                        .await?;
                    return Ok(record);
                }

                if existing.owner == owner {
                    debug!(path = %path, owner = %owner, "LFS lock already held by same owner");
                    Ok(existing)
                } else {
                    Err(CrabError::LfsLockConflict {
                        path: path.to_string(),
                        owner: existing.owner,
                    })
                }
            }
            Err(e) => Err(e),
        }
    }

    /// Removes the advisory lock for `path`, verifying that `owner` matches.
    ///
    /// # Errors
    ///
    /// Returns [`CrabError::LfsLockConflict`] if the lock is held by
    /// a different owner. Returns [`CrabError::NotFound`] if no lock
    /// exists for the path.
    pub async fn unlock(&self, path: &str, owner: &str) -> Result<()> {
        self.unlock_with_id(path, owner, None).await
    }

    /// Releases a lock through a holder-checked compare-and-swap transition.
    ///
    /// When `lock_id` is supplied it must match the current record as well as
    /// the owner. The record becomes a tombstone instead of being deleted, so
    /// a stale unlock cannot erase a newer lock that reused the same key.
    pub async fn unlock_with_id(
        &self,
        path: &str,
        owner: &str,
        lock_id: Option<&str>,
    ) -> Result<()> {
        let key = self.lock_path(path);
        let obj_path = Path::from(key.as_str());

        let (body, etag) = self.store.get_with_etag(&obj_path).await?;
        let existing: LockRecord = serde_json::from_slice(&body)
            .map_err(|e| CrabError::Internal(format!("lock deserialize: {e}")))?;

        if Self::is_released(&existing) {
            return Err(CrabError::NotFound {
                path: format!("LFS lock for {path}"),
            });
        }
        if existing.owner != owner {
            return Err(CrabError::LfsLockConflict {
                path: path.to_string(),
                owner: existing.owner,
            });
        }
        if lock_id.is_some_and(|lock_id| lock_id != existing.id) {
            return Err(CrabError::Configuration {
                key: "lfs unlock".to_owned(),
                origin: format!("lock ID does not match the current lock for {path}"),
            });
        }

        let mut released = existing;
        released.released_at = Some(unix_now());
        let body = serde_json::to_vec(&released)
            .map_err(|e| CrabError::Internal(format!("lock serialize: {e}")))?;
        self.store
            .update(&obj_path, Bytes::from(body), etag)
            .await?;
        debug!(path = %path, owner = %owner, "LFS lock released");
        Ok(())
    }

    /// Releases the advisory lock for `path` regardless of owner.
    ///
    /// # Errors
    ///
    /// Propagates storage errors. `NotFound` is treated as success
    /// (lock already gone).
    pub async fn force_unlock(&self, path: &str) -> Result<()> {
        let key = self.lock_path(path);
        let obj_path = Path::from(key.as_str());

        let (body, etag) = match self.store.get_with_etag(&obj_path).await {
            Ok(result) => result,
            Err(CrabError::NotFound { .. }) => {
                debug!(path = %path, "LFS lock already gone on force-unlock");
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        let mut released: LockRecord = serde_json::from_slice(&body)
            .map_err(|e| CrabError::Internal(format!("lock deserialize: {e}")))?;
        if Self::is_released(&released) {
            return Ok(());
        }
        released.released_at = Some(unix_now());
        let body = serde_json::to_vec(&released)
            .map_err(|e| CrabError::Internal(format!("lock serialize: {e}")))?;
        self.store
            .update(&obj_path, Bytes::from(body), etag)
            .await?;
        debug!(path = %path, "LFS lock force-released");
        Ok(())
    }

    /// Lists all active (non-expired) lock records under this prefix.
    ///
    /// Expired locks are skipped. To list all locks including expired,
    /// use [`Self::list_all`].
    ///
    /// # Errors
    ///
    /// Propagates storage errors from listing or reading individual
    /// lock records.
    pub async fn list(&self) -> Result<Vec<LockRecord>> {
        let all = self.list_all().await?;
        Ok(all.into_iter().filter(|r| !Self::is_expired(r)).collect())
    }

    /// Lists all lock records including expired ones.
    ///
    /// # Errors
    ///
    /// Propagates storage errors from listing or reading individual
    /// lock records.
    pub async fn list_all(&self) -> Result<Vec<LockRecord>> {
        let locks_prefix = Path::from(format!("{}/{}", self.prefix, self.namespace));
        let stream = self.store.inner().list(Some(&locks_prefix));
        let objects: Vec<_> = stream
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| {
                CrabError::from(crab_storage::map_object_store_error(
                    e,
                    locks_prefix.as_ref(),
                ))
            })?;

        let mut records = Vec::new();
        for meta in objects {
            match self.store.get_with_etag(&meta.location).await {
                Ok((body, _)) => {
                    if let Ok(record) = serde_json::from_slice::<LockRecord>(&body)
                        && !Self::is_released(&record)
                    {
                        records.push(record);
                    } else {
                        debug!(key = %meta.location, "lock record JSON deserialization failed");
                    }
                }
                Err(CrabError::NotFound { .. }) => {
                    // Disappeared between list and get — skip.
                }
                Err(e) => return Err(e),
            }
        }

        Ok(records)
    }

    /// Verifies all lock records by re-reading and validating JSON integrity.
    ///
    /// Returns a list of invalid lock keys that could not be deserialized.
    /// Intact locks pass verification silently.
    ///
    /// # Errors
    ///
    /// Propagates storage errors from listing or reading.
    pub async fn verify_locks(&self) -> Result<Vec<String>> {
        let locks_prefix = Path::from(format!("{}/{}", self.prefix, self.namespace));
        let stream = self.store.inner().list(Some(&locks_prefix));
        let objects: Vec<_> = stream
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| {
                CrabError::from(crab_storage::map_object_store_error(
                    e,
                    locks_prefix.as_ref(),
                ))
            })?;

        let mut invalid = Vec::new();
        for meta in objects {
            match self.store.get_with_etag(&meta.location).await {
                Ok((body, _)) => {
                    if serde_json::from_slice::<LockRecord>(&body).is_err() {
                        invalid.push(meta.location.to_string());
                    }
                }
                Err(CrabError::NotFound { .. }) => {
                    // Disappeared — skip.
                }
                Err(e) => return Err(e),
            }
        }

        Ok(invalid)
    }

    /// Checks whether a lock record has expired.
    fn is_expired(record: &LockRecord) -> bool {
        match record.expires_at {
            Some(exp) => unix_now() >= exp,
            None => false,
        }
    }

    fn is_released(record: &LockRecord) -> bool {
        record.released_at.is_some()
    }

    /// Returns locks held by other owners on the given paths.
    ///
    /// For each path, checks whether a lock exists and is owned by
    /// someone other than `owner`. Paths that are unlocked or locked
    /// by `owner` are excluded from the result.
    ///
    /// # Errors
    ///
    /// Propagates storage errors.
    pub async fn check_conflicts(&self, paths: &[String], owner: &str) -> Result<Vec<LockRecord>> {
        let mut conflicts = Vec::new();

        for path in paths {
            let key = self.lock_path(path);
            let obj_path = Path::from(key.as_str());

            match self.store.get_with_etag(&obj_path).await {
                Ok((body, _)) => {
                    let record = serde_json::from_slice::<LockRecord>(&body).map_err(|error| {
                        CrabError::CorruptObject {
                            path: key,
                            reason: format!("invalid LFS lock record: {error}"),
                        }
                    })?;
                    if !Self::is_expired(&record)
                        && !Self::is_released(&record)
                        && record.owner != owner
                    {
                        conflicts.push(record);
                    }
                }
                Err(CrabError::NotFound { .. }) => {
                    // No lock on this path — no conflict.
                }
                Err(e) => return Err(e),
            }
        }

        Ok(conflicts)
    }

    /// Computes the object store path for a lock record.
    fn lock_path(&self, path: &str) -> String {
        let hash = blake3::hash(path.as_bytes());
        let hex = hash.to_hex();
        format!("{}/{}/{hex}", self.prefix, self.namespace)
    }
}

/// Generates a unique lock ID from path, owner, timestamp, nanoseconds, and PID.
fn generate_lock_id(path: &str, owner: &str) -> String {
    let now = unix_now();
    // Include nanosecond precision and process ID for uniqueness
    // within the same second across concurrent processes.
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .subsec_nanos();
    let pid = std::process::id();
    let input = format!("{path}:{owner}:{now}:{nanos}:{pid}");
    let hash = blake3::hash(input.as_bytes());
    let hex = hash.to_hex();
    // Format as UUID-like: 8-4-4-4-12
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32],
    )
}

/// Current Unix timestamp in seconds.
fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
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

    fn lock_manager(store: &Store) -> LockManager {
        LockManager::native(store.clone(), "repo")
    }

    #[tokio::test]
    async fn lock_creates_record() {
        let store = memory_store();
        let mgr = lock_manager(&store);

        let record = mgr.lock("models/large.bin", "alice").await.unwrap();
        assert_eq!(record.path, "models/large.bin");
        assert_eq!(record.owner, "alice");
        assert!(!record.id.is_empty());
        assert!(record.locked_at > 0);
    }

    #[tokio::test]
    async fn lock_same_owner_returns_existing() {
        let store = memory_store();
        let mgr = lock_manager(&store);

        let first = mgr.lock("models/large.bin", "alice").await.unwrap();
        let second = mgr.lock("models/large.bin", "alice").await.unwrap();
        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn lock_different_owner_conflicts() {
        let store = memory_store();
        let mgr = lock_manager(&store);

        mgr.lock("models/large.bin", "alice").await.unwrap();
        let err = mgr
            .lock("models/large.bin", "bob")
            .await
            .expect_err("should conflict");
        assert!(
            matches!(err, CrabError::LfsLockConflict { ref owner, .. } if owner == "alice"),
            "expected LfsLockConflict from alice, got {err:?}"
        );
    }

    #[tokio::test]
    async fn unlock_by_owner_succeeds() {
        let store = memory_store();
        let mgr = lock_manager(&store);

        mgr.lock("models/large.bin", "alice").await.unwrap();
        mgr.unlock("models/large.bin", "alice").await.unwrap();

        // Lock should be gone.
        let locks = mgr.list().await.unwrap();
        assert!(locks.is_empty());
    }

    #[tokio::test]
    async fn unlock_by_wrong_owner_fails() {
        let store = memory_store();
        let mgr = lock_manager(&store);

        mgr.lock("models/large.bin", "alice").await.unwrap();
        let err = mgr
            .unlock("models/large.bin", "bob")
            .await
            .expect_err("wrong owner should fail");
        assert!(
            matches!(err, CrabError::LfsLockConflict { ref owner, .. } if owner == "alice"),
            "expected LfsLockConflict, got {err:?}"
        );
    }

    #[tokio::test]
    async fn force_unlock_removes_regardless_of_owner() {
        let store = memory_store();
        let mgr = lock_manager(&store);

        mgr.lock("models/large.bin", "alice").await.unwrap();
        mgr.force_unlock("models/large.bin").await.unwrap();

        let locks = mgr.list().await.unwrap();
        assert!(locks.is_empty());
    }

    #[tokio::test]
    async fn force_unlock_nonexistent_is_ok() {
        let store = memory_store();
        let mgr = lock_manager(&store);

        mgr.force_unlock("no/such/file").await.unwrap();
    }

    #[tokio::test]
    async fn list_returns_all_locks() {
        let store = memory_store();
        let mgr = lock_manager(&store);

        mgr.lock("a.bin", "alice").await.unwrap();
        mgr.lock("b.bin", "bob").await.unwrap();
        mgr.lock("c.bin", "alice").await.unwrap();

        let mut locks = mgr.list().await.unwrap();
        locks.sort_by(|a, b| a.path.cmp(&b.path));

        assert_eq!(locks.len(), 3);
        assert_eq!(locks[0].path, "a.bin");
        assert_eq!(locks[1].path, "b.bin");
        assert_eq!(locks[2].path, "c.bin");
    }

    #[tokio::test]
    async fn check_conflicts_finds_other_owners() {
        let store = memory_store();
        let mgr = lock_manager(&store);

        mgr.lock("a.bin", "alice").await.unwrap();
        mgr.lock("b.bin", "bob").await.unwrap();
        mgr.lock("c.bin", "alice").await.unwrap();

        let paths = vec![
            "a.bin".to_string(),
            "b.bin".to_string(),
            "c.bin".to_string(),
            "d.bin".to_string(), // not locked
        ];

        let conflicts = mgr.check_conflicts(&paths, "alice").await.unwrap();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].path, "b.bin");
        assert_eq!(conflicts[0].owner, "bob");
    }

    #[tokio::test]
    async fn check_conflicts_empty_when_all_owned() {
        let store = memory_store();
        let mgr = lock_manager(&store);

        mgr.lock("a.bin", "alice").await.unwrap();
        mgr.lock("b.bin", "alice").await.unwrap();

        let paths = vec!["a.bin".to_string(), "b.bin".to_string()];
        let conflicts = mgr.check_conflicts(&paths, "alice").await.unwrap();
        assert!(conflicts.is_empty());
    }

    #[tokio::test]
    async fn expired_lock_is_replaced_atomically() {
        let store = memory_store();
        let mgr = lock_manager(&store);
        mgr.lock_with_expiry("model.bin", "alice", Some(Duration::ZERO))
            .await
            .unwrap();

        let replacement = mgr.lock("model.bin", "bob").await.unwrap();

        assert_eq!(replacement.owner, "bob");
        assert_eq!(mgr.list().await.unwrap(), vec![replacement]);
    }

    #[tokio::test]
    async fn expired_lock_is_not_a_push_conflict() {
        let store = memory_store();
        let mgr = lock_manager(&store);
        mgr.lock_with_expiry("model.bin", "alice", Some(Duration::ZERO))
            .await
            .unwrap();

        let conflicts = mgr
            .check_conflicts(&["model.bin".to_owned()], "bob")
            .await
            .unwrap();

        assert!(conflicts.is_empty());
    }

    #[tokio::test]
    async fn missing_identity_does_not_bypass_active_lock() {
        let store = memory_store();
        let mgr = lock_manager(&store);
        mgr.lock("model.bin", "alice").await.unwrap();

        let conflicts = mgr
            .check_conflicts(&["model.bin".to_owned()], "")
            .await
            .unwrap();

        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].owner, "alice");
    }

    #[tokio::test]
    async fn corrupt_lock_record_fails_conflict_check_closed() {
        let store = memory_store();
        let mgr = lock_manager(&store);
        let path = object_store::path::Path::from(mgr.lock_path("model.bin"));
        store
            .put(&path, Bytes::from_static(b"invalid"))
            .await
            .unwrap();

        let error = mgr
            .check_conflicts(&["model.bin".to_owned()], "bob")
            .await
            .unwrap_err();

        assert!(matches!(error, CrabError::CorruptObject { .. }));
    }

    #[tokio::test]
    async fn lock_path_uses_blake3_hash() {
        let mgr = LockManager::native(memory_store(), "repo");
        let key = mgr.lock_path("models/large.bin");
        let expected_hash = blake3::hash(b"models/large.bin").to_hex();
        assert_eq!(key, format!("repo/locks/files/{expected_hash}"));
    }

    #[tokio::test]
    async fn lfs_lock_path_uses_lfs_namespace() {
        let mgr = LockManager::lfs(memory_store(), "repo");
        let key = mgr.lock_path("models/large.bin");
        let expected_hash = blake3::hash(b"models/large.bin").to_hex();
        assert_eq!(key, format!("repo/lfs/locks/{expected_hash}"));
    }

    #[tokio::test]
    async fn lock_unlock_lock_cycle() {
        let store = memory_store();
        let mgr = lock_manager(&store);

        mgr.lock("file.bin", "alice").await.unwrap();
        mgr.unlock("file.bin", "alice").await.unwrap();

        // Should be able to re-lock after unlock.
        let record = mgr.lock("file.bin", "bob").await.unwrap();
        assert_eq!(record.owner, "bob");
    }

    #[tokio::test]
    async fn stale_unlock_id_cannot_release_replacement_lock() {
        let store = memory_store();
        let mgr = lock_manager(&store);

        let first = mgr.lock("file.bin", "alice").await.unwrap();
        mgr.unlock_with_id("file.bin", "alice", Some(&first.id))
            .await
            .unwrap();
        let replacement = mgr.lock("file.bin", "bob").await.unwrap();

        let error = mgr
            .unlock_with_id("file.bin", "alice", Some(&first.id))
            .await
            .expect_err("stale owner and ID must not release replacement");
        assert!(matches!(error, CrabError::LfsLockConflict { .. }));
        assert_eq!(mgr.list().await.unwrap(), vec![replacement]);
    }
}
