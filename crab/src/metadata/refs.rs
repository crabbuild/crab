//! Git ref storage backed by object store.
//!
//! Refs are stored as individual objects under `refs/{name}` in the
//! repository's object store prefix. Each object contains the hex SHA
//! string. CAS semantics are enforced via `PutMode::Create` for new
//! refs and `PutMode::Update(ETag)` for updates.

use bytes::Bytes;
use object_store::path::Path;
use tracing::debug;

use crate::core::error::{CrabError, Result};
use crate::storage::store::{ETag, Store};

/// A git ref entry with its name, SHA, and optional CAS token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefEntry {
    /// Ref name (e.g. `heads/main`, `tags/v1.0`).
    pub name: String,
    /// Hex-encoded commit SHA.
    pub sha: String,
    /// CAS token for conditional updates. `None` on list results from
    /// backends that don't populate per-object `ETag`s in listings.
    pub etag: Option<ETag>,
}

/// Trait for ref storage operations.
///
/// Implementations must enforce CAS semantics: `create` fails if the
/// ref already exists, `update` fails if the stored SHA doesn't match
/// `expected_sha`.
pub trait RefStore: Send + Sync {
    /// List all refs under the store's prefix.
    fn list(&self) -> impl std::future::Future<Output = Result<Vec<RefEntry>>> + Send;

    /// Get a single ref by name. Returns `None` if the ref doesn't exist.
    fn get(&self, name: &str)
    -> impl std::future::Future<Output = Result<Option<RefEntry>>> + Send;

    /// Create a new ref. Fails with `RefAlreadyExists` if it already exists.
    fn create(&self, name: &str, sha: &str)
    -> impl std::future::Future<Output = Result<()>> + Send;

    /// Update an existing ref. Fails with `CasConflict` if the stored
    /// SHA doesn't match `expected_sha`.
    fn update(
        &self,
        name: &str,
        expected_sha: &str,
        new_sha: &str,
    ) -> impl std::future::Future<Output = Result<()>> + Send;

    /// Delete a ref. Fails with `NotFound` if it doesn't exist.
    fn delete(&self, name: &str) -> impl std::future::Future<Output = Result<()>> + Send;
}

/// S3-backed ref store implementation.
///
/// Refs are stored at `{repo_prefix}/refs/{name}` as plain-text hex SHA
/// strings. The `Store` wrapper handles retry and error mapping.
pub struct ObjectStoreRefStore {
    store: Store,
    repo_prefix: String,
}

impl ObjectStoreRefStore {
    /// Create a new ref store rooted at `prefix` in the given object store.
    #[must_use]
    pub fn new(store: Store, prefix: String) -> Self {
        Self {
            store,
            repo_prefix: prefix,
        }
    }

    /// Build the object-store path for a ref name.
    fn ref_path(&self, name: &str) -> Path {
        Path::from(format!("{}/refs/{name}", self.repo_prefix))
    }
}

impl RefStore for ObjectStoreRefStore {
    async fn list(&self) -> Result<Vec<RefEntry>> {
        use futures_util::StreamExt;

        let prefix = Path::from(format!("{}/refs", self.repo_prefix));
        let stream = self.store.inner().list(Some(&prefix));
        let objects: Vec<_> = stream
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<std::result::Result<Vec<_>, _>>()
            .map_err(|e| {
                CrabError::from(crab_storage::map_object_store_error(e, prefix.as_ref()))
            })?;

        let mut entries = Vec::with_capacity(objects.len());
        for meta in objects {
            let full = meta.location.as_ref();
            // Strip the `{repo_prefix}/refs/` portion to get the ref name.
            let ref_prefix = format!("{}/refs/", self.repo_prefix);
            let name = full.strip_prefix(&ref_prefix).unwrap_or(full);
            // Read the SHA from the object body.
            match self.store.get_with_etag(&meta.location).await {
                Ok((body, etag)) => {
                    let sha = String::from_utf8_lossy(&body).trim().to_string();
                    entries.push(RefEntry {
                        name: name.to_string(),
                        sha,
                        etag: Some(etag),
                    });
                }
                Err(CrabError::NotFound { .. }) => {
                    // Race: listed but deleted before we could read.
                    debug!(ref_name = %name, "ref disappeared between list and get");
                }
                Err(e) => return Err(e),
            }
        }
        Ok(entries)
    }

    async fn get(&self, name: &str) -> Result<Option<RefEntry>> {
        let path = self.ref_path(name);
        match self.store.get_with_etag(&path).await {
            Ok((body, etag)) => {
                let sha = String::from_utf8_lossy(&body).trim().to_string();
                Ok(Some(RefEntry {
                    name: name.to_string(),
                    sha,
                    etag: Some(etag),
                }))
            }
            Err(CrabError::NotFound { .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    async fn create(&self, name: &str, sha: &str) -> Result<()> {
        let path = self.ref_path(name);
        let body = Bytes::from(sha.to_string());
        self.store
            .create_strict(&path, body)
            .await
            .map_err(|e| match e {
                CrabError::CasConflict { .. } => CrabError::RefAlreadyExists {
                    name: name.to_string(),
                },
                other => other,
            })
    }

    async fn update(&self, name: &str, expected_sha: &str, new_sha: &str) -> Result<()> {
        let path = self.ref_path(name);
        // Read current value + ETag.
        let (body, etag) = self.store.get_with_etag(&path).await?;
        let current_sha = String::from_utf8_lossy(&body).trim().to_string();

        if current_sha != expected_sha {
            return Err(CrabError::NonFastForward {
                ref_name: name.to_string(),
                have: current_sha,
                want: expected_sha.to_string(),
            });
        }

        let new_body = Bytes::from(new_sha.to_string());
        self.store.update(&path, new_body, etag).await?;
        Ok(())
    }

    async fn delete(&self, name: &str) -> Result<()> {
        let path = self.ref_path(name);
        self.store.delete(&path).await
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use object_store::memory::InMemory;
    use std::sync::Arc;

    fn test_ref_store() -> ObjectStoreRefStore {
        let inner: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
        let store = Store::new(inner);
        ObjectStoreRefStore::new(store, "repo".to_string())
    }

    #[tokio::test]
    async fn create_and_get_ref() {
        let rs = test_ref_store();
        rs.create("heads/main", "abc123").await.unwrap();

        let entry = rs
            .get("heads/main")
            .await
            .unwrap()
            .expect("ref should exist");
        assert_eq!(entry.name, "heads/main");
        assert_eq!(entry.sha, "abc123");
        assert!(entry.etag.is_some());
    }

    #[tokio::test]
    async fn get_missing_ref_returns_none() {
        let rs = test_ref_store();
        assert!(rs.get("heads/nonexistent").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn create_duplicate_ref_fails() {
        let rs = test_ref_store();
        rs.create("heads/main", "abc123").await.unwrap();
        let err = rs.create("heads/main", "def456").await.unwrap_err();
        assert!(
            matches!(err, CrabError::RefAlreadyExists { .. }),
            "expected RefAlreadyExists, got {err:?}"
        );
    }

    #[tokio::test]
    async fn update_ref_with_correct_sha() {
        let rs = test_ref_store();
        rs.create("heads/main", "abc123").await.unwrap();
        rs.update("heads/main", "abc123", "def456").await.unwrap();

        let entry = rs.get("heads/main").await.unwrap().unwrap();
        assert_eq!(entry.sha, "def456");
    }

    #[tokio::test]
    async fn update_ref_with_wrong_sha_fails() {
        let rs = test_ref_store();
        rs.create("heads/main", "abc123").await.unwrap();
        let err = rs
            .update("heads/main", "wrong", "def456")
            .await
            .unwrap_err();
        assert!(
            matches!(err, CrabError::NonFastForward { .. }),
            "expected NonFastForward, got {err:?}"
        );
    }

    #[tokio::test]
    async fn delete_ref() {
        let rs = test_ref_store();
        rs.create("heads/main", "abc123").await.unwrap();
        rs.delete("heads/main").await.unwrap();
        assert!(rs.get("heads/main").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn list_refs() {
        let rs = test_ref_store();
        rs.create("heads/main", "aaa").await.unwrap();
        rs.create("heads/dev", "bbb").await.unwrap();
        rs.create("tags/v1", "ccc").await.unwrap();

        let mut refs = rs.list().await.unwrap();
        refs.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(refs.len(), 3);
        assert_eq!(refs[0].name, "heads/dev");
        assert_eq!(refs[1].name, "heads/main");
        assert_eq!(refs[2].name, "tags/v1");
    }
}
