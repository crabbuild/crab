//! Workflow-facing object-store adapter.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use crab_coordination::{CoordinationError, GcFenceHeartbeat, GcFenceLease};
use object_store::path::Path;
use object_store::{ObjectMeta, ObjectStore};
use tokio_util::sync::CancellationToken;

use crate::{Result, WorkflowError};

const GC_FENCE_TTL: Duration = crab_coordination::DEFAULT_GC_FENCE_TTL;

/// Object store used for remote workflow cache manifests and artifacts.
///
/// The adapter keeps storage-domain classification inside `crab-workflow` so
/// cache callers receive the same workflow error vocabulary for local and
/// remote operations.
#[derive(Clone)]
pub struct WorkflowStore {
    inner: crab_storage::Store,
}

struct WorkflowFenceLease {
    lease: GcFenceLease,
    heartbeat: GcFenceHeartbeat,
}

impl WorkflowFenceLease {
    async fn release(self) -> Result<()> {
        self.heartbeat.stop().await;
        self.lease.release().await.map_err(WorkflowError::GcFence)
    }
}

/// Shared GC writer admission covering the global and repository workflow roots.
pub struct WorkflowGcWriter {
    global: WorkflowFenceLease,
    repo: WorkflowFenceLease,
    cancel: CancellationToken,
}

impl WorkflowGcWriter {
    async fn acquire(store: &WorkflowStore, prefix: &str) -> Result<Self> {
        let cancel = CancellationToken::new();
        let router = crab_storage::StoreLayout::new(store.inner.clone(), prefix.to_owned());
        let global =
            GcFenceLease::acquire_writer(store.inner.inner(), router.global_prefix(), GC_FENCE_TTL)
                .await
                .map_err(WorkflowError::GcFence)?;
        let global_heartbeat = GcFenceHeartbeat::spawn(&global, cancel.clone(), GC_FENCE_TTL / 3);
        let repo = match GcFenceLease::acquire_writer(
            store.inner.inner(),
            router.repo_prefix(),
            GC_FENCE_TTL,
        )
        .await
        .map_err(WorkflowError::GcFence)
        {
            Ok(repo) => repo,
            Err(error) => {
                global_heartbeat.stop().await;
                let _ = global.release().await;
                return Err(error);
            }
        };
        let repo_heartbeat = GcFenceHeartbeat::spawn(&repo, cancel.clone(), GC_FENCE_TTL / 3);
        Ok(Self {
            global: WorkflowFenceLease {
                lease: global,
                heartbeat: global_heartbeat,
            },
            repo: WorkflowFenceLease {
                lease: repo,
                heartbeat: repo_heartbeat,
            },
            cancel,
        })
    }

    pub(crate) fn cancellation(&self) -> CancellationToken {
        self.cancel.clone()
    }

    pub(crate) fn lease_lost_error(prefix: &str) -> WorkflowError {
        WorkflowError::GcFence(CoordinationError::GcFenceLost {
            domain: prefix.to_owned(),
            holder: "workflow-heartbeat".to_owned(),
        })
    }

    /// Release repository admission before global admission.
    pub async fn release(self) -> Result<()> {
        let repo_result = self.repo.release().await;
        let global_result = self.global.release().await;
        match repo_result {
            Err(error) => Err(error),
            Ok(()) => global_result,
        }
    }
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

    /// Acquire shared admission while publishing remote workflow objects.
    pub async fn acquire_gc_writer(&self, prefix: &str) -> Result<WorkflowGcWriter> {
        WorkflowGcWriter::acquire(self, prefix).await
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
