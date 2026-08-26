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

use crate::core::error::{CrabError, Result};
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
}

impl BatchResolver {
    /// Creates a new batch resolver.
    ///
    /// - `remote_store`: the remote LFS object store for upload/download.
    /// - `local_lfs_dir`: path to the local `.git/lfs` directory where
    ///   objects are cached on disk.
    /// - `config`: LFS configuration controlling concurrency and behavior.
    pub fn new(
        remote_store: Arc<LfsObjectStore>,
        local_lfs_dir: PathBuf,
        config: LfsConfig,
    ) -> Self {
        Self {
            remote_store,
            local_lfs_dir,
            config,
        }
    }

    /// Identifies LFS pointers whose objects are missing from the remote store.
    ///
    /// In a full implementation this walks the commits being pushed to
    /// collect LFS pointers. For now it accepts pre-collected pointers
    /// (the git object walking will be wired up via gitoxide later).
    ///
    /// Returns only the pointers whose OIDs are absent from the remote.
    /// Existence checks run concurrently, bounded by `config.concurrent_transfers`.
    pub async fn find_missing_for_push(&self, pointers: &[LfsPointer]) -> Result<Vec<LfsPointer>> {
        if pointers.is_empty() {
            return Ok(Vec::new());
        }

        let mut pointer_by_oid: HashMap<[u8; 32], LfsPointer> =
            HashMap::with_capacity(pointers.len());
        for pointer in pointers {
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

        let coordinator = TransferCoordinator::new((&self.config).into());
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
    /// file paths before checking local presence. In a full implementation
    /// this walks reachable commits to collect pointers; for now it accepts
    /// pre-collected `(path, pointer)` pairs.
    ///
    /// Returns only the pointers whose OIDs are absent from local storage.
    pub fn find_missing_for_fetch(
        &self,
        entries: &[(String, LfsPointer)],
        include: Option<&PatternFilter>,
        exclude: Option<&PatternFilter>,
    ) -> Result<Vec<LfsPointer>> {
        let mut missing = Vec::new();
        for (path, pointer) in entries {
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
            if !self.local_object_exists(pointer)? {
                missing.push(pointer.clone());
            }
        }
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
        if pointers.is_empty() {
            return Ok(());
        }

        let coordinator = TransferCoordinator::new((&self.config).into());
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
        if pointers.is_empty() {
            return Ok(());
        }

        let coordinator = TransferCoordinator::new((&self.config).into());
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
mod tests {
    use std::sync::Arc;

    use bytes::Bytes;
    use object_store::memory::InMemory;
    use sha2::{Digest, Sha256};

    use super::*;
    use crab_storage::{RetryPolicy, Store};

    fn test_lfs_store() -> LfsObjectStore {
        let inner: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
        let policy = RetryPolicy {
            max_attempts: 2,
            base: std::time::Duration::from_millis(1),
            cap: std::time::Duration::from_millis(5),
        };
        let store = Store::with_retry(inner, policy);
        LfsObjectStore::new(store, "repo")
    }

    fn sha256_oid(data: &[u8]) -> [u8; 32] {
        let hash = Sha256::digest(data);
        let mut oid = [0u8; 32];
        oid.copy_from_slice(&hash);
        oid
    }

    fn make_pointer(data: &[u8]) -> LfsPointer {
        LfsPointer {
            oid: sha256_oid(data),
            size: data.len() as u64,
            extensions: Vec::new(),
        }
    }

    fn test_config() -> LfsConfig {
        LfsConfig {
            concurrent_transfers: 4,
            ..LfsConfig::default()
        }
    }

    #[tokio::test]
    async fn find_missing_for_push_returns_absent_objects() {
        let store = Arc::new(test_lfs_store());
        let dir = tempfile::tempdir().unwrap();
        let resolver =
            BatchResolver::new(Arc::clone(&store), dir.path().to_path_buf(), test_config());

        let data_a = b"object-a";
        let data_b = b"object-b";
        let ptr_a = make_pointer(data_a);
        let ptr_b = make_pointer(data_b);

        // Upload object A to remote so it's present.
        store
            .put(&ptr_a.oid, Bytes::from(data_a.to_vec()))
            .await
            .unwrap();

        let missing = resolver
            .find_missing_for_push(&[ptr_a.clone(), ptr_b.clone()])
            .await
            .unwrap();

        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].oid, ptr_b.oid);
    }

    #[tokio::test]
    async fn find_missing_for_fetch_applies_include_filter() {
        let store = Arc::new(test_lfs_store());
        let dir = tempfile::tempdir().unwrap();
        let resolver =
            BatchResolver::new(Arc::clone(&store), dir.path().to_path_buf(), test_config());

        let ptr = make_pointer(b"some-data");
        let entries = vec![
            ("models/large.bin".to_string(), ptr.clone()),
            ("docs/readme.md".to_string(), ptr.clone()),
        ];

        let include = PatternFilter::new("*.bin").unwrap();
        let missing = resolver
            .find_missing_for_fetch(&entries, Some(&include), None)
            .unwrap();

        assert_eq!(missing.len(), 1);
    }

    #[tokio::test]
    async fn find_missing_for_fetch_applies_exclude_filter() {
        let store = Arc::new(test_lfs_store());
        let dir = tempfile::tempdir().unwrap();
        let resolver =
            BatchResolver::new(Arc::clone(&store), dir.path().to_path_buf(), test_config());

        let ptr = make_pointer(b"some-data");
        let entries = vec![
            ("models/large.bin".to_string(), ptr.clone()),
            ("docs/readme.md".to_string(), ptr.clone()),
        ];

        let exclude = PatternFilter::new("*.md").unwrap();
        let missing = resolver
            .find_missing_for_fetch(&entries, None, Some(&exclude))
            .unwrap();

        // Both point to the same OID, but readme.md is excluded.
        // Since the OID is the same and not local, only the .bin entry passes.
        assert_eq!(missing.len(), 1);
    }

    #[tokio::test]
    async fn find_missing_for_fetch_skips_locally_present() {
        let store = Arc::new(test_lfs_store());
        let dir = tempfile::tempdir().unwrap();
        let resolver =
            BatchResolver::new(Arc::clone(&store), dir.path().to_path_buf(), test_config());

        let data = b"local-object";
        let ptr = make_pointer(data);

        // Write the object to local storage.
        let local_path = local_object_path_for(dir.path(), &ptr.oid);
        std::fs::create_dir_all(local_path.parent().unwrap()).unwrap();
        std::fs::write(&local_path, data).unwrap();

        let entries = vec![("file.bin".to_string(), ptr)];
        let missing = resolver
            .find_missing_for_fetch(&entries, None, None)
            .unwrap();

        assert!(missing.is_empty());
    }

    #[tokio::test]
    async fn find_missing_for_fetch_refetches_corrupt_local_object() {
        let store = Arc::new(test_lfs_store());
        let dir = tempfile::tempdir().unwrap();
        let resolver =
            BatchResolver::new(Arc::clone(&store), dir.path().to_path_buf(), test_config());
        let ptr = make_pointer(b"valid-content");
        let local_path = local_object_path_for(dir.path(), &ptr.oid);
        std::fs::create_dir_all(local_path.parent().unwrap()).unwrap();
        std::fs::write(local_path, b"corrupt").unwrap();

        let missing = resolver
            .find_missing_for_fetch(&[("file.bin".to_owned(), ptr.clone())], None, None)
            .unwrap();

        assert_eq!(missing, vec![ptr]);
    }

    #[tokio::test]
    async fn upload_missing_transfers_objects() {
        let store = Arc::new(test_lfs_store());
        let dir = tempfile::tempdir().unwrap();
        let resolver =
            BatchResolver::new(Arc::clone(&store), dir.path().to_path_buf(), test_config());

        let data = b"upload-me";
        let ptr = make_pointer(data);

        // Write the object to local storage so upload can read it.
        let local_path = local_object_path_for(dir.path(), &ptr.oid);
        std::fs::create_dir_all(local_path.parent().unwrap()).unwrap();
        std::fs::write(&local_path, data).unwrap();

        resolver.upload_missing(&[ptr.clone()]).await.unwrap();

        // Verify it's now on the remote.
        assert!(store.exists(&ptr.oid).await.unwrap());
        let downloaded = store.get(&ptr.oid).await.unwrap();
        assert_eq!(&downloaded[..], data);
    }

    #[tokio::test]
    async fn upload_missing_skips_already_present() {
        let store = Arc::new(test_lfs_store());
        let dir = tempfile::tempdir().unwrap();
        let resolver =
            BatchResolver::new(Arc::clone(&store), dir.path().to_path_buf(), test_config());

        let data = b"already-there";
        let ptr = make_pointer(data);

        // Pre-upload to remote.
        store
            .put(&ptr.oid, Bytes::from(data.to_vec()))
            .await
            .unwrap();

        // No local file needed — the upload should be skipped entirely.
        resolver.upload_missing(&[ptr]).await.unwrap();
    }

    #[tokio::test]
    async fn download_missing_transfers_objects() {
        let store = Arc::new(test_lfs_store());
        let dir = tempfile::tempdir().unwrap();
        let resolver =
            BatchResolver::new(Arc::clone(&store), dir.path().to_path_buf(), test_config());

        let data = b"download-me";
        let ptr = make_pointer(data);

        // Upload to remote so download can fetch it.
        store
            .put(&ptr.oid, Bytes::from(data.to_vec()))
            .await
            .unwrap();

        resolver.download_missing(&[ptr.clone()]).await.unwrap();

        // Verify it's now in local storage.
        let local_path = local_object_path_for(dir.path(), &ptr.oid);
        assert!(local_path.is_file());
        let content = std::fs::read(&local_path).unwrap();
        assert_eq!(&content[..], data);
    }

    #[tokio::test]
    async fn download_missing_skips_already_present() {
        let store = Arc::new(test_lfs_store());
        let dir = tempfile::tempdir().unwrap();
        let resolver =
            BatchResolver::new(Arc::clone(&store), dir.path().to_path_buf(), test_config());

        let data = b"already-local";
        let ptr = make_pointer(data);

        // Write to local storage.
        let local_path = local_object_path_for(dir.path(), &ptr.oid);
        std::fs::create_dir_all(local_path.parent().unwrap()).unwrap();
        std::fs::write(&local_path, data).unwrap();

        // No remote object needed — the download should be skipped.
        resolver.download_missing(&[ptr]).await.unwrap();
    }

    #[tokio::test]
    async fn download_objects_refetch_overwrites_present_object() {
        let store = Arc::new(test_lfs_store());
        let dir = tempfile::tempdir().unwrap();
        let resolver =
            BatchResolver::new(Arc::clone(&store), dir.path().to_path_buf(), test_config());

        let data = b"remote-content";
        let ptr = make_pointer(data);
        store
            .put(&ptr.oid, Bytes::from(data.to_vec()))
            .await
            .unwrap();

        let local_path = local_object_path_for(dir.path(), &ptr.oid);
        std::fs::create_dir_all(local_path.parent().unwrap()).unwrap();
        std::fs::write(&local_path, b"stale").unwrap();

        resolver
            .download_objects(&[ptr.clone()], true)
            .await
            .unwrap();

        let content = std::fs::read(&local_path).unwrap();
        assert_eq!(&content[..], data);
    }

    #[tokio::test]
    async fn upload_missing_empty_list_is_noop() {
        let store = Arc::new(test_lfs_store());
        let dir = tempfile::tempdir().unwrap();
        let resolver =
            BatchResolver::new(Arc::clone(&store), dir.path().to_path_buf(), test_config());

        resolver.upload_missing(&[]).await.unwrap();
    }

    #[tokio::test]
    async fn download_missing_empty_list_is_noop() {
        let store = Arc::new(test_lfs_store());
        let dir = tempfile::tempdir().unwrap();
        let resolver =
            BatchResolver::new(Arc::clone(&store), dir.path().to_path_buf(), test_config());

        resolver.download_missing(&[]).await.unwrap();
    }

    #[tokio::test]
    async fn pattern_filter_comma_separated() {
        let filter = PatternFilter::new("*.bin, *.dat").unwrap();
        assert!(filter.matches("model.bin"));
        assert!(filter.matches("data.dat"));
        assert!(!filter.matches("readme.md"));
    }

    #[tokio::test]
    async fn upload_download_round_trip() {
        let inner: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
        let policy = RetryPolicy {
            max_attempts: 2,
            base: std::time::Duration::from_millis(1),
            cap: std::time::Duration::from_millis(5),
        };

        let store = Arc::new(LfsObjectStore::new(
            Store::with_retry(Arc::clone(&inner), policy.clone()),
            "repo",
        ));

        let upload_dir = tempfile::tempdir().unwrap();
        let download_dir = tempfile::tempdir().unwrap();

        let data = b"round-trip-content";
        let ptr = make_pointer(data);

        // Write to upload-side local storage.
        let upload_path = local_object_path_for(upload_dir.path(), &ptr.oid);
        std::fs::create_dir_all(upload_path.parent().unwrap()).unwrap();
        std::fs::write(&upload_path, data).unwrap();

        // Upload.
        let uploader = BatchResolver::new(
            Arc::clone(&store),
            upload_dir.path().to_path_buf(),
            test_config(),
        );
        uploader.upload_missing(&[ptr.clone()]).await.unwrap();

        // Download to a different local dir.
        let downloader = BatchResolver::new(
            Arc::clone(&store),
            download_dir.path().to_path_buf(),
            test_config(),
        );
        downloader.download_missing(&[ptr.clone()]).await.unwrap();

        let downloaded_path = local_object_path_for(download_dir.path(), &ptr.oid);
        let content = std::fs::read(&downloaded_path).unwrap();
        assert_eq!(&content[..], data);
    }

    #[test]
    fn local_object_path_format() {
        let dir = PathBuf::from("/tmp/lfs");
        let mut oid = [0u8; 32];
        oid[0] = 0xab;
        oid[1] = 0xcd;
        let path = local_object_path_for(&dir, &oid);
        let path_str = path.to_string_lossy();
        let hex = hex_encode(&oid);
        assert!(path_str.contains("/objects/ab/cd/"));
        assert!(path_str.ends_with(&hex));
    }
}
