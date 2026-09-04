//! Batch resolver for LFS object transfers.
//!
//! Identifies which LFS objects are missing locally or remotely by walking
//! commit history and comparing against the object store. Drives concurrent
//! upload and download of missing objects.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

#[cfg(not(feature = "gix-pathmatch"))]
use globset::{Glob, GlobSet, GlobSetBuilder};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::core::error::{CrabError, Result, check_cancelled};
use crate::lfs::config::LfsConfig;
use crate::lfs::coordinator::{
    TransferCoordinator, TransferDirection, TransferOutcome, TransferRequest,
};
use crab_git::lfs_pointer::{LfsPointer, hex_encode};
use crab_lfs::LfsObjectStore;

/// Batch resolver for LFS push and fetch operations.
///
/// Determines which LFS objects need to be transferred by comparing
/// local and remote state, then drives concurrent uploads or downloads
/// bounded by the configured concurrency limit.
pub struct BatchResolver {
    remote_store: Arc<LfsObjectStore>,
    local_lfs_dir: PathBuf,
    config: LfsConfig,
    cancellation: CancellationToken,
}

impl BatchResolver {
    /// Creates a new batch resolver.
    ///
    /// - `remote_store`: the remote LFS object store for upload/download.
    /// - `local_lfs_dir`: path to the local `.git/lfs` directory where
    ///   objects are cached on disk.
    /// - `config`: LFS configuration controlling concurrency and behavior.
    /// - `cancel`: caller cancellation, observed by every batch operation.
    pub fn new(
        remote_store: Arc<LfsObjectStore>,
        local_lfs_dir: PathBuf,
        config: LfsConfig,
        cancel: &CancellationToken,
    ) -> Self {
        Self {
            remote_store,
            local_lfs_dir,
            config,
            cancellation: cancel.clone(),
        }
    }

    /// Identifies LFS pointers whose objects are missing from the remote store.
    ///
    /// Accepts pointers collected by the caller's Git discovery operation.
    /// Returns pointers whose remote objects are missing or corrupt.
    /// Existence checks run concurrently, bounded by `config.concurrent_transfers`.
    pub async fn find_missing_for_push(&self, pointers: &[LfsPointer]) -> Result<Vec<LfsPointer>> {
        check_cancelled(&self.cancellation)?;
        if pointers.is_empty() {
            return Ok(Vec::new());
        }

        let mut pointer_by_oid: HashMap<[u8; 32], LfsPointer> =
            HashMap::with_capacity(pointers.len());
        for pointer in pointers {
            check_cancelled(&self.cancellation)?;
            if let Some(existing) = pointer_by_oid.get(&pointer.oid)
                && existing.size != pointer.size
            {
                return Err(CrabError::LfsObjectCorrupt {
                    oid: hex_encode(&pointer.oid),
                });
            }
            pointer_by_oid
                .entry(pointer.oid)
                .or_insert_with(|| pointer.clone());
        }

        let coordinator = TransferCoordinator::new((&self.config).into(), &self.cancellation);
        let missing = Arc::new(Mutex::new(Vec::new()));
        let missing_for_operation = Arc::clone(&missing);
        let store = Arc::clone(&self.remote_store);
        let requests = pointer_by_oid.values().map(transfer_request);
        coordinator
            .execute(
                TransferDirection::Upload,
                requests,
                move |request, cancel| {
                    let store = Arc::clone(&store);
                    let missing = Arc::clone(&missing_for_operation);
                    async move {
                        if cancel.is_cancelled() {
                            return Err(CrabError::Cancelled);
                        }
                        match store.verify_size(&request.oid, request.size).await {
                            Ok(()) => Ok(TransferOutcome::AlreadyValid),
                            Err(
                                crab_lfs::LfsError::ObjectMissing { .. }
                                | crab_lfs::LfsError::ObjectCorrupt { .. },
                            ) => {
                                missing.lock().await.push(request);
                                Ok(TransferOutcome::Skipped)
                            }
                            Err(error) => Err(CrabError::from(error)),
                        }
                    }
                },
            )
            .await?;

        let mut missing = missing.lock().await;
        let requests = std::mem::take(&mut *missing);
        Ok(requests
            .into_iter()
            .filter_map(|request| pointer_by_oid.get(&request.oid).cloned())
            .collect())
    }

    /// Identifies LFS pointers whose objects are missing locally.
    ///
    /// Applies optional include/exclude glob filters to the associated
    /// file paths before checking local presence. The caller supplies
    /// `(path, pointer)` pairs from its Git discovery operation.
    ///
    /// Returns only the pointers whose OIDs are absent from local storage.
    pub fn find_missing_for_fetch(
        &self,
        entries: &[(String, LfsPointer)],
        include: Option<&PatternFilter>,
        exclude: Option<&PatternFilter>,
    ) -> Result<Vec<LfsPointer>> {
        check_cancelled(&self.cancellation)?;
        let mut missing = Vec::new();
        let mut seen_sizes = HashMap::new();
        for (path, pointer) in entries {
            check_cancelled(&self.cancellation)?;
            if let Some(inc) = include
                && !inc.matches(path)
            {
                continue;
            }
            if let Some(exc) = exclude
                && exc.matches(path)
            {
                continue;
            }
            if let Some(existing_size) = seen_sizes.get(&pointer.oid) {
                if *existing_size != pointer.size {
                    return Err(CrabError::LfsObjectCorrupt {
                        oid: hex_encode(&pointer.oid),
                    });
                }
                continue;
            }
            seen_sizes.insert(pointer.oid, pointer.size);
            if !self.local_object_exists(pointer)? {
                missing.push(pointer.clone());
            }
        }
        check_cancelled(&self.cancellation)?;
        Ok(missing)
    }

    /// Uploads missing LFS objects to the remote store concurrently.
    ///
    /// Reads each object from local LFS storage and uploads it to the
    /// remote. Objects already present on the remote are skipped.
    /// Concurrency is bounded by `config.concurrent_transfers`, and
    /// further reduced when `transfer_max_bandwidth` is set.
    ///
    /// # Errors
    ///
    /// Returns the first upload error encountered after admitted transfers
    /// have drained; subsequent errors are not returned individually.
    pub async fn upload_missing(&self, pointers: &[LfsPointer]) -> Result<()> {
        check_cancelled(&self.cancellation)?;
        if pointers.is_empty() {
            return Ok(());
        }

        let coordinator = TransferCoordinator::new((&self.config).into(), &self.cancellation);
        let store = Arc::clone(&self.remote_store);
        let local_dir = self.local_lfs_dir.clone();
        coordinator
            .execute(
                TransferDirection::Upload,
                pointers.iter().map(transfer_request),
                move |request, cancel| {
                    let store = Arc::clone(&store);
                    let local_path = local_object_path_for(&local_dir, &request.oid);
                    async move {
                        if cancel.is_cancelled() {
                            return Err(CrabError::Cancelled);
                        }
                        store
                            .put_stream_with_size(&request.oid, Some(request.size), &local_path)
                            .await
                            .map_err(CrabError::from)?;
                        Ok(TransferOutcome::Transferred)
                    }
                },
            )
            .await
            .map(|_| ())
    }

    /// Downloads missing LFS objects from the remote store concurrently.
    ///
    /// Fetches each object from the remote and writes it to local LFS
    /// storage. Objects already present locally are skipped.
    /// Concurrency is bounded by `config.concurrent_transfers`, and
    /// further reduced when `transfer_max_bandwidth` is set.
    ///
    /// # Errors
    ///
    /// Returns the first download error encountered unless
    /// `config.skip_download_errors` is set, in which case failed transfers
    /// are counted as skipped and the operation continues.
    pub async fn download_missing(&self, pointers: &[LfsPointer]) -> Result<()> {
        self.download_objects(pointers, false).await
    }

    /// Downloads LFS objects from the remote store.
    ///
    /// When `force` is true, existing local objects are downloaded again and
    /// overwritten after remote integrity verification.
    pub async fn download_objects(&self, pointers: &[LfsPointer], force: bool) -> Result<()> {
        check_cancelled(&self.cancellation)?;
        if pointers.is_empty() {
            return Ok(());
        }

        let coordinator = TransferCoordinator::new((&self.config).into(), &self.cancellation);
        let store = Arc::clone(&self.remote_store);
        let local_dir = self.local_lfs_dir.clone();
        coordinator
            .execute(
                TransferDirection::Download,
                pointers.iter().map(transfer_request),
                move |request, cancel| {
                    let store = Arc::clone(&store);
                    let local_dir = local_dir.clone();
                    async move {
                        if cancel.is_cancelled() {
                            return Err(CrabError::Cancelled);
                        }
                        if !force
                            && crate::lfs::cache::is_valid(&local_dir, &request.oid, request.size)?
                        {
                            tracing::debug!(
                                oid = %hex_encode(&request.oid),
                                "object already local, skipping download",
                            );
                            return Ok(TransferOutcome::AlreadyValid);
                        }

                        let temp = crate::lfs::cache::new_temp_path(&local_dir)?;
                        let temp_path: PathBuf = temp.to_path_buf();
                        store
                            .download_to_file(&request.oid, request.size, &temp_path)
                            .await
                            .map_err(CrabError::from)?;
                        check_cancelled(&cancel)?;
                        crate::lfs::cache::install_verified_temp_path(
                            &local_dir,
                            &request.oid,
                            request.size,
                            temp,
                        )?;
                        Ok(TransferOutcome::Transferred)
                    }
                },
            )
            .await
            .map(|_| ())
    }

    /// Checks whether an LFS object exists in local storage.
    fn local_object_exists(&self, pointer: &LfsPointer) -> Result<bool> {
        crate::lfs::cache::is_valid(&self.local_lfs_dir, &pointer.oid, pointer.size)
    }
}

fn transfer_request(pointer: &LfsPointer) -> TransferRequest {
    TransferRequest {
        oid: pointer.oid,
        size: pointer.size,
    }
}

// ---------------------------------------------------------------------------
// Pattern filter
// ---------------------------------------------------------------------------

/// Compiled include filter over comma-separated glob patterns.
///
/// Under `gix-pathmatch`, this wraps a consolidated
/// [`core::pathmatch::PatternFilter`] so LFS batch filtering follows
/// the same pathspec semantics as `crab add` / `hydrate`. Off the
/// flag, it falls back to the legacy `globset`-backed implementation.
pub struct PatternFilter {
    #[cfg(feature = "gix-pathmatch")]
    inner: crate::core::pathmatch::PatternFilter,
    #[cfg(not(feature = "gix-pathmatch"))]
    set: GlobSet,
}

impl PatternFilter {
    /// Compiles a comma-separated list of glob patterns into a filter.
    ///
    /// # Errors
    ///
    /// Returns [`CrabError::InvalidPattern`] if any pattern is malformed.
    pub fn new(patterns: &str) -> Result<Self> {
        let parts: Vec<String> = patterns
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned)
            .collect();

        #[cfg(feature = "gix-pathmatch")]
        {
            let inner = crate::core::pathmatch::build_filter(&parts, &[])?;
            Ok(Self { inner })
        }
        #[cfg(not(feature = "gix-pathmatch"))]
        {
            let mut builder = GlobSetBuilder::new();
            for pat in &parts {
                builder.add(Glob::new(pat)?);
            }
            let set = builder.build()?;
            Ok(Self { set })
        }
    }

    /// Returns `true` if the path matches any pattern in this filter.
    pub fn matches(&self, path: &str) -> bool {
        #[cfg(feature = "gix-pathmatch")]
        {
            self.inner.matches(path)
        }
        #[cfg(not(feature = "gix-pathmatch"))]
        {
            self.set.is_match(path)
        }
    }
}

/// Compute the local filesystem path for an LFS object.
///
/// Layout: `{lfs_dir}/objects/{oid[0:2]}/{oid[2:4]}/{oid}`
fn local_object_path_for(lfs_dir: &Path, oid: &[u8; 32]) -> PathBuf {
    let hex = hex_encode(oid);
    lfs_dir
        .join("objects")
        .join(&hex[..2])
        .join(&hex[2..4])
        .join(&hex)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]
mod tests;
