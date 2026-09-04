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

/// Composition adapter between local staging durability and storage transport.
pub struct MultipartJournal(Arc<Mutex<crab_staging::MultipartRegistry>>);

impl MultipartJournal {
    #[must_use]
    pub fn new(registry: crab_staging::MultipartRegistry) -> Self {
        Self(Arc::new(Mutex::new(registry)))
    }

    async fn call<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut crab_staging::MultipartRegistry) -> crab_staging::Result<T> + Send + 'static,
    {
        let registry = Arc::clone(&self.0);
        // SQLite FULL commits and lock waits must not stall the async workers
        // responsible for provider I/O and lease heartbeats.
        tokio::task::spawn_blocking(move || {
            let mut registry = registry.lock().map_err(|_| {
                crab_staging::StagingError::Internal("multipart journal lock poisoned".into())
            })?;
            f(&mut registry)
        })
        .await
        .map_err(|error| CrabError::Io(std::io::Error::other(error)))?
        .map_err(CrabError::from)
    }

    pub async fn find_abandoned(
        &self,
        now: std::time::SystemTime,
        grace: Duration,
    ) -> crate::core::error::Result<Vec<crab_staging::AbandonedUpload>> {
        self.call(move |registry| registry.find_abandoned(now, grace))
            .await
    }

    pub async fn claim_abandoned(
        &self,
        entry_id: &str,
        expected_revision: i64,
        owner_token: &str,
        now: i64,
        lease_duration: Duration,
    ) -> crate::core::error::Result<Option<crab_staging::AbandonedClaim>> {
        let entry_id = entry_id.to_owned();
        let owner_token = owner_token.to_owned();
        self.call(move |registry| {
            registry.claim_abandoned(
                &entry_id,
                expected_revision,
                &owner_token,
                now,
                lease_duration,
            )
        })
        .await
    }

    pub async fn renew_repair(
        &self,
        lease: &crab_staging::MultipartLease,
        now: i64,
        lease_duration: Duration,
    ) -> crate::core::error::Result<bool> {
        let lease = lease.clone();
        self.call(move |registry| registry.renew(&lease, now, lease_duration))
            .await
    }

    pub async fn abandon_repair(
        &self,
        lease: &crab_staging::MultipartLease,
        now: i64,
    ) -> crate::core::error::Result<bool> {
        let lease = lease.clone();
        self.call(move |registry| registry.abandon_owned(&lease, now))
            .await
    }

    pub async fn release_repair(
        &self,
        lease: &crab_staging::MultipartLease,
        now: i64,
    ) -> crate::core::error::Result<bool> {
        let lease = lease.clone();
        self.call(move |registry| registry.release_owned(&lease, now))
            .await
    }
}

#[async_trait::async_trait]
impl crab_storage::multipart::MultipartJournal for MultipartJournal {
    async fn claim(
        &self,
        target: &crab_storage::multipart::MultipartTarget,
        payload_hash: &[u8],
        expected_hash: &[u8; 32],
        size: u64,
        part_size: usize,
        owner_token: &str,
        now_unix_seconds: i64,
        lease_duration: Duration,
    ) -> crab_storage::multipart::JournalResult<crab_storage::multipart::JournalClaimOutcome> {
        let target = crab_staging::MultipartTarget {
            provider: target.provider.clone(),
            host: target.host.clone(),
            container: target.container.clone(),
            key: target.key.clone(),
        };
        let payload_hash = payload_hash.to_vec();
        let expected_hash = *expected_hash;
        let owner_token = owner_token.to_owned();
        let outcome = self
            .call(move |registry| {
                registry.claim(
                    &target,
                    &payload_hash,
                    &expected_hash,
                    size,
                    part_size,
                    &owner_token,
                    now_unix_seconds,
                    lease_duration,
                )
            })
            .await
            .map_err(journal_error)?;
        match outcome {
            crab_staging::ClaimOutcome::Busy => {
                Ok(crab_storage::multipart::JournalClaimOutcome::Busy)
            }
            crab_staging::ClaimOutcome::Acquired(claim) => {
                let parts = claim
                    .completed_parts
                    .into_iter()
                    .map(|part| {
                        Ok(crab_storage::multipart::JournalPart {
                            part_idx: usize::try_from(part.part_number).map_err(|_| {
                                Box::new(crab_staging::StagingError::Internal(
                                    "multipart part index is negative".to_owned(),
                                ))
                                    as crab_storage::multipart::JournalError
                            })?,
                            content_id: part.etag,
                            size: u64::try_from(part.size).map_err(|_| {
                                Box::new(crab_staging::StagingError::Internal(
                                    "multipart part size is negative".to_owned(),
                                ))
                                    as crab_storage::multipart::JournalError
                            })?,
                        })
                    })
                    .collect::<crab_storage::multipart::JournalResult<Vec<_>>>()?;
                Ok(crab_storage::multipart::JournalClaimOutcome::Acquired(
                    crab_storage::multipart::JournalClaim {
                        lease: crab_storage::multipart::JournalLease {
                            entry_id: claim.lease.entry_id,
                            owner_token: claim.lease.owner_token,
                        },
                        upload_id: claim.upload_id,
                        payload_hash: claim.payload_hash,
                        expected_hash: claim.expected_hash,
                        size: claim.size,
                        part_size: claim.part_size,
                        parts,
                    },
                ))
            }
        }
    }

    async fn bind_upload(
        &self,
        lease: &crab_storage::multipart::JournalLease,
        upload_id: &str,
        now_unix_seconds: i64,
        lease_duration: Duration,
    ) -> crab_storage::multipart::JournalResult<bool> {
        let lease = staging_lease(lease);
        let upload_id = upload_id.to_owned();
        self.call(move |registry| {
            registry.bind_upload(&lease, &upload_id, now_unix_seconds, lease_duration)
        })
        .await
        .map_err(journal_error)
    }

    async fn renew(
        &self,
        lease: &crab_storage::multipart::JournalLease,
        now_unix_seconds: i64,
        lease_duration: Duration,
    ) -> crab_storage::multipart::JournalResult<bool> {
        let lease = staging_lease(lease);
        self.call(move |registry| registry.renew(&lease, now_unix_seconds, lease_duration))
            .await
            .map_err(journal_error)
    }

    async fn record_part(
        &self,
        lease: &crab_storage::multipart::JournalLease,
        part: &crab_storage::multipart::JournalPart,
        now_unix_seconds: i64,
        lease_duration: Duration,
    ) -> crab_storage::multipart::JournalResult<bool> {
        let part = crab_staging::CompletedPart {
            part_number: i64::try_from(part.part_idx).map_err(|_| {
                Box::new(crab_staging::StagingError::Internal(
                    "multipart part index exceeds SQLite range".to_owned(),
                )) as crab_storage::multipart::JournalError
            })?,
            etag: part.content_id.clone(),
            size: i64::try_from(part.size).map_err(|_| {
                Box::new(crab_staging::StagingError::Internal(
                    "multipart part size exceeds SQLite range".to_owned(),
                )) as crab_storage::multipart::JournalError
            })?,
        };
        let lease = staging_lease(lease);
        self.call(move |registry| {
            registry.record_part(&lease, &part, now_unix_seconds, lease_duration)
        })
        .await
        .map_err(journal_error)
    }

    async fn reset_owned(
        &self,
        lease: &crab_storage::multipart::JournalLease,
        payload_hash: &[u8],
        expected_hash: &[u8; 32],
        size: u64,
        part_size: usize,
        now_unix_seconds: i64,
        lease_duration: Duration,
    ) -> crab_storage::multipart::JournalResult<bool> {
        let lease = staging_lease(lease);
        let payload_hash = payload_hash.to_vec();
        let expected_hash = *expected_hash;
        self.call(move |registry| {
            registry.reset_owned(
                &lease,
                &payload_hash,
                &expected_hash,
                size,
                part_size,
                now_unix_seconds,
                lease_duration,
            )
        })
        .await
        .map_err(journal_error)
    }

    async fn complete_owned(
        &self,
        lease: &crab_storage::multipart::JournalLease,
        now_unix_seconds: i64,
    ) -> crab_storage::multipart::JournalResult<bool> {
        let lease = staging_lease(lease);
        self.call(move |registry| registry.complete_owned(&lease, now_unix_seconds))
            .await
            .map_err(journal_error)
    }

    async fn abandon_owned(
        &self,
        lease: &crab_storage::multipart::JournalLease,
        now_unix_seconds: i64,
    ) -> crab_storage::multipart::JournalResult<bool> {
        let lease = staging_lease(lease);
        self.call(move |registry| registry.abandon_owned(&lease, now_unix_seconds))
            .await
            .map_err(journal_error)
    }

    async fn release_owned(
        &self,
        lease: &crab_storage::multipart::JournalLease,
        now_unix_seconds: i64,
    ) -> crab_storage::multipart::JournalResult<bool> {
        let lease = staging_lease(lease);
        self.call(move |registry| registry.release_owned(&lease, now_unix_seconds))
            .await
            .map_err(journal_error)
    }
}

fn journal_error(error: CrabError) -> crab_storage::multipart::JournalError {
    Box::new(error)
}

fn staging_lease(lease: &crab_storage::multipart::JournalLease) -> crab_staging::MultipartLease {
    crab_staging::MultipartLease {
        entry_id: lease.entry_id.clone(),
        owner_token: lease.owner_token.clone(),
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
    pub fn with_target_identity(mut self, identity: [u8; 32]) -> Self {
        self.inner = self.inner.with_target_identity(identity);
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
        identity: BucketIdentity,
    ) -> Self {
        self.inner = self.inner.with_multipart(multipart, identity);
        self
    }

    #[must_use]
    pub fn has_resumable_multipart(&self) -> bool {
        self.inner.has_resumable_multipart()
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

    pub async fn verify_written_size_and_hash(
        &self,
        path: &Path,
        expected_size: u64,
        expected_hash: &[u8; 32],
    ) -> Result<()> {
        self.inner
            .verify_written_size_and_hash(path, expected_size, expected_hash)
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

    #[allow(clippy::too_many_arguments)]
    pub async fn put_multipart_file_resumable(
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
    ) -> Result<crab_storage::multipart::ResumableUploadOutcome> {
        self.inner
            .put_multipart_file_resumable(
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
            .map_err(CrabError::from)
    }

    #[must_use]
    pub fn multipart_target(
        &self,
        path: &Path,
    ) -> Option<crab_storage::multipart::MultipartTarget> {
        self.inner.multipart_target(path)
    }

    pub async fn abort_explicit_multipart(&self, path: &Path, upload_id: &str) -> Result<()> {
        self.inner
            .abort_explicit_multipart(path, upload_id)
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
