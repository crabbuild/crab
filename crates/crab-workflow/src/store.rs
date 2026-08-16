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

    /// Writes a content-addressed object if absent.
    pub async fn put(&self, path: &Path, bytes: Bytes) -> Result<()> {
        self.inner.put(path, bytes).await.map_err(Into::into)
    }

    /// Fetches object metadata without reading its body.
    pub async fn head(&self, path: &Path) -> Result<ObjectMeta> {
        self.inner.head(path).await.map_err(Into::into)
    }

    /// Reads an object together with its compare-and-swap token.
    pub async fn get_with_etag(&self, path: &Path) -> Result<(Bytes, crab_storage::ETag)> {
        self.inner.get_with_etag(path).await.map_err(Into::into)
    }
}
