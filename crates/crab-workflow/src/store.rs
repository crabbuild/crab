//! Workflow-facing object-store adapter.

use std::sync::Arc;

use bytes::Bytes;
use object_store::path::Path;
use object_store::{ObjectMeta, ObjectStore};

use crate::Result;

/// Object store used for remote workflow cache manifests and artifacts.
///
/// The adapter keeps storage-domain classification inside `crab-workflow` so
/// cache callers receive the same workflow error vocabulary for local and
/// remote operations.
#[derive(Clone)]
pub struct WorkflowStore {
    inner: crab_storage::Store,
}

impl WorkflowStore {
    /// Wraps an object store with Crab's default retry policy.
    #[must_use]
    pub fn new(inner: Arc<dyn ObjectStore>) -> Self {
        Self {
            inner: crab_storage::Store::new(inner),
        }
    }

    /// Wraps an existing storage-domain store.
    #[must_use]
    pub fn from_storage(inner: crab_storage::Store) -> Self {
        Self { inner }
    }

    /// Borrow the storage-domain facade for operations that need streaming
    /// reads, multipart file uploads, or explicit CAS/delete semantics.
    ///
    /// Workflow policy remains in this crate; exposing the already-configured
    /// facade avoids constructing a second provider client in CLI commands.
    #[must_use]
    pub fn as_storage(&self) -> &crab_storage::Store {
        &self.inner
    }

    /// Writes a content-addressed object if absent.
    pub async fn put(&self, path: &Path, bytes: Bytes) -> Result<()> {
        self.inner.put(path, bytes).await.map_err(Into::into)
    }

    /// Creates a coordination object only when the path is absent.
    ///
    /// Unlike [`Self::put`], this preserves an existing-object conflict even
    /// when the backend does not implement conditional create correctly.
    pub async fn create_strict(&self, path: &Path, bytes: Bytes) -> Result<()> {
        self.inner
            .create_strict(path, bytes)
            .await
            .map_err(Into::into)
    }

    /// Fetches object metadata without reading its body.
    pub async fn head(&self, path: &Path) -> Result<ObjectMeta> {
        self.inner.head(path).await.map_err(Into::into)
    }

    /// Reads an object together with its compare-and-swap token.
    pub async fn get_with_etag(&self, path: &Path) -> Result<(Bytes, crab_storage::ETag)> {
        self.inner.get_with_etag(path).await.map_err(Into::into)
    }

    /// Delete one remote workflow object.
    pub async fn delete(&self, path: &Path) -> Result<()> {
        self.inner.delete(path).await.map_err(Into::into)
    }

    /// List objects below a remote workflow prefix.
    pub async fn list_prefix(&self, prefix: &Path) -> Result<Vec<ObjectMeta>> {
        self.inner.list_prefix(prefix).await.map_err(Into::into)
    }
}
