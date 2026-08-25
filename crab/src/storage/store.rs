//! Compatibility Adapter for the storage-domain `Store`.
//!
//! The implementation lives in `crab-storage`; this module preserves the
//! existing CLI-facing `CrabError` Interface while callers migrate to the
//! storage-domain `StorageError` Interface.

use std::ops::Range;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use bytes::Bytes;
use object_store::path::Path;
use object_store::{MultipartUpload, ObjectMeta, ObjectStore};
use tokio::io::AsyncReadExt;

use crate::core::error::{CrabError, Result};

pub use crate::git::url::Cloud;
pub use crab_storage::{BucketIdentity, ETag, StagedWrite};

/// Product adapter between the staging registry and storage transport.
///
/// The adapter lives at this composition boundary so `crab-staging` stays
/// independent of `crab-storage`. SQLite calls are short local transactions;
/// the mutex supplies the `Sync` contract required by concurrent push tasks.
pub struct MultipartJournal(Mutex<crab_staging::MultipartRegistry>);

impl MultipartJournal {
    #[must_use]
    pub fn new(registry: crab_staging::MultipartRegistry) -> Self {
        Self(Mutex::new(registry))
    }

    fn lock(
        &self,
    ) -> std::result::Result<
        std::sync::MutexGuard<'_, crab_staging::MultipartRegistry>,
        crab_storage::multipart::JournalError,
    > {
        self.0.lock().map_err(|_| {
            Box::new(crab_staging::StagingError::Internal(
                "multipart journal lock poisoned".to_owned(),
            )) as crab_storage::multipart::JournalError
        })
    }

    pub fn find_abandoned(
        &self,
        now: std::time::SystemTime,
        grace: Duration,
    ) -> std::result::Result<Vec<crab_staging::AbandonedUpload>, crab_staging::StagingError> {
        self.0
            .lock()
            .map_err(|_| crab_staging::StagingError::Internal("journal lock poisoned".into()))?
            .find_abandoned(now, grace)
    }

    pub fn abort_if_tracked(
        &self,
        upload_id: &str,
    ) -> std::result::Result<bool, crab_staging::StagingError> {
        self.0
            .lock()
            .map_err(|_| crab_staging::StagingError::Internal("journal lock poisoned".into()))?
            .abort_if_tracked(upload_id)
    }

    fn map_staging(error: crab_staging::StagingError) -> crab_storage::multipart::JournalError {
        Box::new(error)
    }
}

impl crab_storage::multipart::MultipartJournal for MultipartJournal {
    fn begin(
        &self,
        payload_hash: &[u8],
        bucket: &str,
        key: &str,
        upload_id: &str,
    ) -> crab_storage::multipart::JournalResult<bool> {
        self.lock()?
            .begin(payload_hash, bucket, key, upload_id)
            .map_err(Self::map_staging)
    }

    fn record_part(
        &self,
        upload_id: &str,
        part_idx: usize,
        content_id: &str,
        size: u64,
    ) -> crab_storage::multipart::JournalResult<()> {
        let part_idx = i64::try_from(part_idx).map_err(|_| {
            Box::new(crab_staging::StagingError::Internal(
                "multipart part index overflow".to_owned(),
            )) as crab_storage::multipart::JournalError
        })?;
        let size = i64::try_from(size).map_err(|_| {
            Box::new(crab_staging::StagingError::Internal(
                "multipart part size overflow".to_owned(),
            )) as crab_storage::multipart::JournalError
        })?;
        self.lock()?
            .record_part(upload_id, part_idx, content_id, size)
            .map_err(Self::map_staging)
    }

    fn complete(&self, upload_id: &str) -> crab_storage::multipart::JournalResult<()> {
        self.lock()?.complete(upload_id).map_err(Self::map_staging)
    }

    fn abort_stale(&self, upload_id: &str) -> crab_storage::multipart::JournalResult<()> {
        self.lock()?
            .abort_stale(upload_id)
            .map_err(Self::map_staging)
    }

    fn resumable(
        &self,
        payload_hash: &[u8],
        bucket: &str,
        key: &str,
    ) -> crab_storage::multipart::JournalResult<Option<crab_storage::multipart::ResumeInfo>> {
        let info = self
            .lock()?
            .resumable(payload_hash, bucket, key)
            .map_err(Self::map_staging)?;
        info.map(|info| {
            let parts = info
                .completed_parts
                .into_iter()
                .map(|part| {
                    Ok(crab_storage::multipart::JournalPart {
                        part_idx: usize::try_from(part.part_number).map_err(|_| {
                            Box::new(crab_staging::StagingError::Internal(
                                "multipart part index is negative".to_owned(),
                            )) as crab_storage::multipart::JournalError
                        })?,
                        content_id: part.etag,
                        size: u64::try_from(part.size).map_err(|_| {
                            Box::new(crab_staging::StagingError::Internal(
                                "multipart part size is negative".to_owned(),
                            )) as crab_storage::multipart::JournalError
                        })?,
                    })
                })
                .collect::<crab_storage::multipart::JournalResult<Vec<_>>>()?;
            Ok(crab_storage::multipart::ResumeInfo {
                upload_id: info.upload_id,
                parts,
            })
        })
        .transpose()
    }
}

/// CAS-aware facade over an `object_store::ObjectStore`.
#[derive(Clone)]
pub struct Store {
    inner: crab_storage::Store,
}

impl crab_storage::StorageScopeProvider for Store {
    fn storage_scope(&self) -> Option<&crab_types::storage::StorageScope> {
        self.storage_scope()
    }
}

impl Store {
    #[must_use]
    pub fn new(inner: Arc<dyn ObjectStore>) -> Self {
        Self {
            inner: crab_storage::Store::new(inner),
        }
    }

    #[must_use]
    pub fn with_retry(inner: Arc<dyn ObjectStore>, retry: crab_storage::RetryPolicy) -> Self {
        Self {
            inner: crab_storage::Store::with_retry(inner, retry),
        }
    }

    #[must_use]
    pub fn from_storage(inner: crab_storage::Store) -> Self {
        Self { inner }
    }

    #[must_use]
    pub fn into_storage(self) -> crab_storage::Store {
        self.inner
    }

    #[must_use]
    pub fn as_storage(&self) -> &crab_storage::Store {
        &self.inner
    }

    #[must_use]
    pub fn with_bucket_identity(mut self, identity: BucketIdentity) -> Self {
        self.inner = self.inner.with_bucket_identity(identity);
        self
    }

    #[must_use]
    pub fn with_signer(mut self, signer: Arc<dyn object_store::signer::Signer>) -> Self {
        self.inner = self.inner.with_signer(signer);
        self
    }

    #[must_use]
    pub fn with_multipart(
        mut self,
        multipart: Arc<dyn object_store::multipart::MultipartStore>,
    ) -> Self {
        self.inner = self.inner.with_multipart(multipart);
        self
    }

    /// Resumable multipart upload with part-level journaling. Returns
    /// whether a previously recorded session was resumed. See
    /// [`crab_storage::Store::put_multipart_file_resumable_retry`].
    #[allow(clippy::too_many_arguments)]
    pub async fn put_multipart_file_resumable_retry(
        &self,
        path: &Path,
        file_path: &std::path::Path,
        size: u64,
        expected_hash: [u8; 32],
        payload_hash: &[u8],
        part_size: usize,
        cancel: &tokio_util::sync::CancellationToken,
        on_part_done: Option<&(dyn Fn(u64) + Send + Sync)>,
        journal: Option<&dyn crab_storage::multipart::MultipartJournal>,
    ) -> crab_storage::Result<bool> {
        self.inner
            .put_multipart_file_resumable_retry(
                path,
                file_path,
                size,
                expected_hash,
                payload_hash,
                part_size,
                cancel,
                on_part_done,
                journal,
            )
            .await
    }

    pub async fn abort_multipart(&self, path: &Path, upload_id: &str) -> Result<bool> {
        self.inner
            .abort_multipart(path, upload_id)
            .await
            .map_err(CrabError::from)
    }

    #[must_use]
    pub fn with_storage_scope(mut self, scope: crab_types::storage::StorageScope) -> Self {
        self.inner = self.inner.with_storage_scope(scope);
        self
    }

    #[must_use]
    pub fn storage_scope(&self) -> Option<&crab_types::storage::StorageScope> {
        self.inner.storage_scope()
    }

    #[must_use]
    pub fn with_read_routes(mut self, routes: Vec<(String, Arc<dyn ObjectStore>)>) -> Self {
        self.inner = self.inner.with_read_routes(routes);
        self
    }

    #[must_use]
    pub fn with_read_byte_observer(mut self, observer: Arc<dyn Fn(u64) + Send + Sync>) -> Self {
        self.inner = self.inner.with_read_byte_observer(observer);
        self
    }

    #[must_use]
    pub fn with_staging_writes(mut self, upload_prefix: String) -> Self {
        self.inner = self.inner.with_staging_writes(upload_prefix);
        self
    }

    #[must_use]
    pub fn with_staging_write_store(
        mut self,
        upload_prefix: String,
        write_inner: Arc<dyn ObjectStore>,
    ) -> Self {
        self.inner = self
            .inner
            .with_staging_write_store(upload_prefix, write_inner);
        self
    }

    #[must_use]
    pub fn staging_write_prefix(&self) -> Option<&str> {
        self.inner.staging_write_prefix()
    }

    #[must_use]
    pub fn staged_writes(&self) -> Vec<StagedWrite> {
        self.inner.staged_writes()
    }

    pub async fn flush_staged_writes(&self, max_concurrency: usize) -> Result<Vec<StagedWrite>> {
        Ok(self.inner.flush_staged_writes(max_concurrency).await?)
    }

    pub async fn flush_staging_object(&self, path: &Path, expected_size: u64) -> Result<()> {
        Ok(self.inner.flush_staging_object(path, expected_size).await?)
    }

    #[must_use]
    pub fn bucket_identity(&self) -> BucketIdentity {
        self.inner.bucket_identity()
    }

    #[must_use]
    pub fn inner(&self) -> &Arc<dyn ObjectStore> {
        self.inner.inner()
    }

    pub async fn put(&self, path: &Path, bytes: Bytes) -> Result<()> {
        self.inner.put(path, bytes).await.map_err(CrabError::from)
    }

    pub async fn create_strict(&self, path: &Path, bytes: Bytes) -> Result<()> {
        self.inner
            .create_strict(path, bytes)
            .await
            .map_err(CrabError::from)
    }

    pub async fn create_strict_with_etag(&self, path: &Path, bytes: Bytes) -> Result<ETag> {
        self.inner
            .create_strict_with_etag(path, bytes)
            .await
            .map_err(CrabError::from)
    }

    pub async fn put_overwrite(&self, path: &Path, bytes: Bytes) -> Result<()> {
        self.inner
            .put_overwrite(path, bytes)
            .await
            .map_err(CrabError::from)
    }

    pub async fn update(&self, path: &Path, bytes: Bytes, etag: ETag) -> Result<ETag> {
        self.inner
            .update(path, bytes, etag)
            .await
            .map_err(CrabError::from)
    }

    pub async fn put_exact(&self, path: &Path, bytes: Bytes) -> Result<()> {
        self.inner
            .put_exact(path, bytes)
            .await
            .map_err(CrabError::from)
    }

    pub async fn get_with_etag(&self, path: &Path) -> Result<(Bytes, ETag)> {
        self.inner
            .get_with_etag(path)
            .await
            .map_err(CrabError::from)
    }

    pub async fn get_with_etag_bounded(
        &self,
        path: &Path,
        max_bytes: u64,
    ) -> Result<(Bytes, ETag)> {
        self.inner
            .get_with_etag_bounded(path, max_bytes)
            .await
            .map_err(CrabError::from)
    }

    pub async fn download_to_path(&self, path: &Path, dest: &std::path::Path) -> Result<u64> {
        self.inner
            .download_to_path(path, dest)
            .await
            .map_err(CrabError::from)
    }

    pub async fn download_to_path_bounded(
        &self,
        path: &Path,
        dest: &std::path::Path,
        max_bytes: u64,
    ) -> Result<u64> {
        self.inner
            .download_to_path_bounded(path, dest, max_bytes)
            .await
            .map_err(CrabError::from)
    }

    pub async fn stream_to_writer<W>(&self, path: &Path, writer: &mut W) -> Result<u64>
    where
        W: std::io::Write + ?Sized,
    {
        self.inner
            .stream_to_writer(path, writer)
            .await
            .map_err(CrabError::from)
    }

    pub async fn verify(&self, path: &Path, expected_hash: &[u8; 32]) -> Result<Bytes> {
        self.inner
            .verify(path, expected_hash)
            .await
            .map_err(CrabError::from)
    }

    pub async fn verify_size_and_hash(
        &self,
        path: &Path,
        expected_size: u64,
        expected_hash: &[u8; 32],
    ) -> Result<()> {
        self.inner
            .verify_size_and_hash(path, expected_size, expected_hash)
            .await
            .map_err(CrabError::from)
    }

    pub async fn head(&self, path: &Path) -> Result<ObjectMeta> {
        self.inner.head(path).await.map_err(CrabError::from)
    }

    pub async fn range_get(&self, path: &Path, range: Range<u64>) -> Result<Bytes> {
        self.inner
            .range_get(path, range)
            .await
            .map_err(CrabError::from)
    }

    pub async fn delete(&self, path: &Path) -> Result<()> {
        self.inner.delete(path).await.map_err(CrabError::from)
    }

    pub async fn copy(&self, from: &Path, to: &Path) -> Result<()> {
        self.inner.copy(from, to).await.map_err(CrabError::from)
    }

    pub async fn copy_if_not_exists(&self, from: &Path, to: &Path) -> Result<()> {
        self.inner
            .copy_if_not_exists(from, to)
            .await
            .map_err(CrabError::from)
    }

    pub async fn promote_staged_content_addressed_object(
        &self,
        staged: &Path,
        canonical: &Path,
        expected_hash: [u8; 32],
        expected_size: u64,
    ) -> Result<bool> {
        self.inner
            .promote_staged_content_addressed_object(
                staged,
                canonical,
                expected_hash,
                expected_size,
            )
            .await
            .map_err(CrabError::from)
    }

    pub async fn create_multipart_upload(&self, path: &Path) -> Result<Box<dyn MultipartUpload>> {
        self.inner
            .create_multipart_upload(path)
            .await
            .map_err(CrabError::from)
    }

    pub async fn delete_prefix(&self, prefix: &Path) -> Result<u64> {
        self.inner
            .delete_prefix(prefix)
            .await
            .map_err(CrabError::from)
    }

    pub async fn list_prefix(&self, prefix: &Path) -> Result<Vec<ObjectMeta>> {
        self.inner
            .list_prefix(prefix)
            .await
            .map_err(CrabError::from)
    }

    pub async fn put_multipart_retry(
        &self,
        path: &Path,
        data: Bytes,
        part_size: usize,
        cancel: &tokio_util::sync::CancellationToken,
        on_part_done: Option<&(dyn Fn(u64) + Send + Sync)>,
    ) -> Result<()> {
        self.inner
            .put_multipart_retry(path, data, part_size, cancel, on_part_done)
            .await
            .map_err(CrabError::from)
    }

    /// Verify a bounded in-memory object with its Xet data hash before a
    /// retryable multipart upload.
    pub async fn put_multipart_retry_with_xet_hash(
        &self,
        path: &Path,
        data: Bytes,
        expected_hash: [u8; 32],
        part_size: usize,
        cancel: &tokio_util::sync::CancellationToken,
        on_part_done: Option<&(dyn Fn(u64) + Send + Sync)>,
    ) -> Result<()> {
        let actual_hash: [u8; 32] = crab_xet::hash::compute_data_hash(&data).into();
        if actual_hash != expected_hash {
            return Err(CrabError::CorruptObject {
                path: path.to_string(),
                reason: format!(
                    "in-memory Xet data hash {} does not match expected {}",
                    crab_xet::hash::merkle_hex_from_bytes(&actual_hash),
                    crab_xet::hash::merkle_hex_from_bytes(&expected_hash)
                ),
            });
        }
        self.inner
            .put_multipart_retry(path, data, part_size, cancel, on_part_done)
            .await
            .map_err(CrabError::from)
    }

    pub async fn put_multipart_file_retry(
        &self,
        path: &Path,
        file_path: &std::path::Path,
        size: u64,
        expected_hash: [u8; 32],
        part_size: usize,
        cancel: &tokio_util::sync::CancellationToken,
        on_part_done: Option<&(dyn Fn(u64) + Send + Sync)>,
    ) -> Result<()> {
        self.inner
            .put_multipart_file_retry(
                path,
                file_path,
                size,
                expected_hash,
                part_size,
                cancel,
                on_part_done,
            )
            .await
            .map_err(CrabError::from)
    }

    /// Verify a local file with the Xet data hash before uploading it as a
    /// bounded, retryable multipart object.
    pub async fn put_multipart_file_retry_with_xet_hash(
        &self,
        path: &Path,
        file_path: &std::path::Path,
        size: u64,
        expected_hash: [u8; 32],
        part_size: usize,
        cancel: &tokio_util::sync::CancellationToken,
        on_part_done: Option<&(dyn Fn(u64) + Send + Sync)>,
    ) -> Result<()> {
        let metadata = tokio::fs::metadata(file_path)
            .await
            .map_err(CrabError::Io)?;
        if metadata.len() != size {
            return Err(CrabError::CorruptObject {
                path: file_path.display().to_string(),
                reason: format!(
                    "local file has {} bytes; upload expects {size}",
                    metadata.len()
                ),
            });
        }

        let mut file = tokio::fs::File::open(file_path)
            .await
            .map_err(CrabError::Io)?;
        let mut xet_hasher = crab_xet::hash::HashedWrite::new(std::io::sink());
        let mut blake3_hasher = blake3::Hasher::new();
        let mut remaining = size;
        let mut buffer = vec![0u8; part_size.max(1).min(1024 * 1024)];
        while remaining > 0 {
            if cancel.is_cancelled() {
                return Err(CrabError::Cancelled);
            }
            let want = remaining.min(buffer.len() as u64) as usize;
            file.read_exact(&mut buffer[..want])
                .await
                .map_err(CrabError::Io)?;
            std::io::Write::write_all(&mut xet_hasher, &buffer[..want]).map_err(CrabError::Io)?;
            blake3_hasher.update(&buffer[..want]);
            remaining -= want as u64;
        }
        let actual_hash: [u8; 32] = xet_hasher.hash().into();
        if actual_hash != expected_hash {
            return Err(CrabError::CorruptObject {
                path: file_path.display().to_string(),
                reason: format!(
                    "local Xet data hash {} does not match expected {}",
                    crab_xet::hash::merkle_hex_from_bytes(&actual_hash),
                    crab_xet::hash::merkle_hex_from_bytes(&expected_hash)
                ),
            });
        }

        self.inner
            .put_multipart_file_retry(
                path,
                file_path,
                size,
                *blake3_hasher.finalize().as_bytes(),
                part_size,
                cancel,
                on_part_done,
            )
            .await
            .map_err(CrabError::from)
    }

    pub async fn signed_url(&self, path: &Path, expires_in: Duration) -> Result<url::Url> {
        self.inner
            .signed_url(path, expires_in)
            .await
            .map_err(CrabError::from)
    }
}

impl From<crab_storage::Store> for Store {
    fn from(inner: crab_storage::Store) -> Self {
        Self::from_storage(inner)
    }
}

impl From<Store> for crab_storage::Store {
    fn from(store: Store) -> Self {
        store.into_storage()
    }
}
