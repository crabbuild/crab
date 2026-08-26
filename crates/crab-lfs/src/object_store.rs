//! LFS object storage in cloud object storage.
//!
//! Stores and retrieves LFS objects keyed by their SHA-256 OID using a
//! two-level fan-out directory layout (`{prefix}/lfs/objects/{aa}/{bb}/{oid}`).
//! Verifies integrity on upload, accepts matching objects idempotently, and
//! conditionally repairs corrupt objects.

use std::ops::Range;
use std::path::Path as StdPath;
use std::pin::Pin;

use bytes::Bytes;
use futures_util::{Stream, StreamExt};
use object_store::path::Path;
use object_store::{MultipartUpload, ObjectMeta, ObjectStoreExt, PutPayload};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crab_git::lfs_pointer::hex_encode;
use crab_storage::{ETag, StorageError, Store};

/// Result alias for LFS object storage operations.
pub type Result<T> = std::result::Result<T, LfsError>;

/// Backpressured LFS object stream with storage failures mapped to the LFS
/// error surface.
pub type LfsByteStream = Pin<Box<dyn Stream<Item = Result<Bytes>> + Send + 'static>>;

/// Errors raised by LFS object storage operations.
#[derive(thiserror::Error, Debug)]
pub enum LfsError {
    /// The object bytes do not match the declared SHA-256 OID.
    #[error("LFS object corrupt: oid {oid}")]
    ObjectCorrupt { oid: String },

    /// The requested LFS object was not found.
    #[error("LFS object missing: oid {oid}")]
    ObjectMissing { oid: String },

    /// Local file I/O failed while streaming an object.
    #[error("LFS object I/O error: {source}")]
    Io {
        #[from]
        #[source]
        source: std::io::Error,
    },

    /// Underlying object-store transport failed.
    #[error(transparent)]
    Storage {
        #[from]
        source: StorageError,
    },
}

/// Part size for streaming multipart uploads. 8 MiB sits above S3's
/// 5 MiB minimum for all parts except the last, and under the
/// 10_000-part ceiling for any realistic LFS object (8 MiB × 10k ≈ 80
/// GiB). Matches the part size the xorb upload path uses so we don't
/// proliferate bespoke tuning knobs for each upload surface.
const STREAM_PART_SIZE: usize = 8 * 1024 * 1024;

/// Maximum number of parts in flight simultaneously during a streaming
/// upload. Bounds peak memory to `STREAM_PART_SIZE * MAX_IN_FLIGHT_PARTS`
/// — 32 MiB at defaults — regardless of file size. Matches the xorb
/// uploader's bound.
const MAX_IN_FLIGHT_PARTS: usize = 4;

/// Size of the read buffer used when streaming a file into part
/// accumulators. Sized to match the part size so a single read tops up
/// one part without an extra copy in the common case.
const FILE_READ_BUF: usize = STREAM_PART_SIZE;
const RECEIPT_MAGIC: &[u8] = b"crab-lfs-receipt\0\x01";
const RECEIPT_VERIFIER: &str = "crab-lfs/1";
const MAX_RECEIPT_FIELD_SIZE: usize = 4 * 1024;
const MAX_RECEIPT_SIZE: u64 = 16 * 1024;

enum ExistingObject {
    Missing,
    Valid(u64),
    Corrupt(ETag),
}

struct VerificationReceipt {
    oid: [u8; 32],
    size: u64,
    object_path: String,
    e_tag: Option<String>,
    version: Option<String>,
    verifier: String,
}

/// LFS object storage backed by a cloud [`Store`].
///
/// Each object is addressed by its 32-byte SHA-256 OID and stored at a
/// two-level fan-out path: `{prefix}/lfs/objects/{aa}/{bb}/{full_oid}`.
pub struct LfsObjectStore {
    store: Store,
    prefix: String,
    primary_fallback: Option<(Store, String)>,
}

impl LfsObjectStore {
    /// Creates a new LFS object store rooted at `prefix`.
    pub fn new(store: Store, prefix: &str) -> Self {
        Self {
            store,
            prefix: prefix.to_owned(),
            primary_fallback: None,
        }
    }

    /// Creates a read store with a primary fallback for stale or failing replicas.
    pub fn new_with_primary_fallback(
        store: Store,
        prefix: &str,
        fallback_store: Store,
        fallback_prefix: &str,
    ) -> Self {
        Self {
            store,
            prefix: prefix.to_owned(),
            primary_fallback: Some((fallback_store, fallback_prefix.to_owned())),
        }
    }

    /// Borrows the underlying [`Store`] for operations not covered by
    /// the `LfsObjectStore` API (e.g., range requests for download resume).
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Returns the object store prefix for this LFS store.
    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// Returns the object store [`Path`] for the given OID.
    ///
    /// Public so callers (e.g., the transfer agent) can issue range
    /// requests directly against the store for download resume.
    pub fn object_path_for(&self, oid: &[u8; 32]) -> Path {
        self.object_path(oid)
    }

    /// Returns the object store [`Path`] for `oid` under `prefix`.
    ///
    /// Use this when the caller already owns the store handle separately, such
    /// as SDK presign paths that need the LFS layout without constructing a
    /// temporary [`LfsObjectStore`].
    #[must_use]
    pub fn object_path_for_prefix(prefix: &str, oid: &[u8; 32]) -> Path {
        Self::object_path_at(prefix, oid)
    }

    /// Reads object metadata, retrying a configured primary fallback when the
    /// selected replica is stale or unavailable.
    pub async fn head(&self, oid: &[u8; 32]) -> Result<ObjectMeta> {
        let path = self.object_path(oid);
        match self.store.head(&path).await {
            Ok(meta) => Ok(meta),
            Err(error) => {
                let Some((fallback_store, fallback_prefix)) = self.primary_fallback.as_ref() else {
                    return Err(error.into());
                };
                tracing::debug!(
                    oid = %hex_encode(oid),
                    error = %error,
                    "LFS metadata read from selected remote failed; retrying primary"
                );
                fallback_store
                    .head(&Self::object_path_at(fallback_prefix, oid))
                    .await
                    .map_err(Into::into)
            }
        }
    }

    /// Verifies an object before opening a backpressured stream for an HTTP
    /// response. Range reads are checked against the complete SHA-256 object
    /// first, so a corrupt immutable key is never served as a successful
    /// transfer.
    pub async fn get_stream(
        &self,
        oid: &[u8; 32],
        expected_size: u64,
        range: Option<Range<u64>>,
    ) -> Result<(ObjectMeta, Range<u64>, LfsByteStream)> {
        match Self::get_verified_stream_at(
            &self.store,
            &self.prefix,
            oid,
            expected_size,
            range.clone(),
        )
        .await
        {
            Ok(result) => Ok(result),
            Err(error) => {
                let Some((fallback_store, fallback_prefix)) = self.primary_fallback.as_ref() else {
                    return Err(error);
                };
                tracing::debug!(
                    oid = %hex_encode(oid),
                    error = %error,
                    "LFS stream from selected remote failed integrity or availability checks; retrying primary"
                );
                Self::get_verified_stream_at(
                    fallback_store,
                    fallback_prefix,
                    oid,
                    expected_size,
                    range,
                )
                .await
            }
        }
    }

    /// Returns the object store path for the given OID.
    ///
    /// Layout: `{prefix}/lfs/objects/{oid[0:2]}/{oid[2:4]}/{oid}`
    /// where each segment is lowercase hex.
    fn object_path(&self, oid: &[u8; 32]) -> Path {
        Self::object_path_at(&self.prefix, oid)
    }

    fn object_path_at(prefix: &str, oid: &[u8; 32]) -> Path {
        let hex = hex_encode(oid);
        let prefix = prefix.trim_matches('/');
        let path_str = if prefix.is_empty() {
            format!("lfs/objects/{}/{}/{}", &hex[..2], &hex[2..4], hex)
        } else {
            format!("{prefix}/lfs/objects/{}/{}/{}", &hex[..2], &hex[2..4], hex)
        };
        Path::from(path_str)
    }

    /// Uploads an LFS object after verifying its SHA-256 matches the declared OID.
    ///
    /// A matching existing object is accepted without a write. A corrupt
    /// object at the same path is replaced conditionally and reverified.
    ///
    /// # Errors
    ///
    /// Returns [`LfsError::ObjectCorrupt`] if the SHA-256 of `bytes`
    /// does not match `oid`.
    pub async fn put(&self, oid: &[u8; 32], bytes: Bytes) -> Result<()> {
        // Verify content integrity before touching the store.
        let actual = Sha256::digest(&bytes);
        if actual.as_slice() != oid {
            return Err(LfsError::ObjectCorrupt {
                oid: hex_encode(oid),
            });
        }

        let path = self.object_path(oid);

        match self.inspect_existing(&path, oid).await? {
            ExistingObject::Valid(_) => {
                self.record_verification_receipt(oid).await;
                return Ok(());
            }
            ExistingObject::Corrupt(etag) => {
                return self.replace_corrupt(&path, oid, bytes, etag).await;
            }
            ExistingObject::Missing => {}
        }

        // The underlying Store.put uses PutMode::Create with idempotent
        // conflict handling, so a race between the exists check and the
        // put is harmless — the second writer sees CasConflict and the
        // Store resolves it by comparing content hashes.
        self.store.put(&path, bytes).await.map_err(LfsError::from)?;
        self.record_verification_receipt(oid).await;
        Ok(())
    }

    /// Streaming upload: read the local file in bounded chunks, hash
    /// incrementally, and push parts to the object store via
    /// [`object_store::MultipartUpload`].
    ///
    /// This is the large-object counterpart of [`Self::put`]. Where
    /// `put` materializes the entire payload in memory (unavoidable
    /// for its `Bytes` contract), `put_stream` caps peak memory at
    /// [`STREAM_PART_SIZE`] × [`MAX_IN_FLIGHT_PARTS`] (~32 MiB at
    /// defaults) regardless of the source file's size. A 50 GiB LFS
    /// object now uploads without OOMing.
    ///
    /// Integrity is verified in one pass: the SHA-256 hasher consumes
    /// every byte as it leaves the file, and the final digest is
    /// compared against the declared `oid`. A mismatch aborts the
    /// multipart upload before any part-complete PUT is issued, so no
    /// partial object lands on the remote.
    ///
    /// A matching existing object short-circuits. A corrupt object at the
    /// target path is replaced only after the local stream has been verified;
    /// the immutable content-addressed key makes a racing valid replacement
    /// logically equivalent.
    ///
    /// # Errors
    ///
    /// - [`LfsError::ObjectCorrupt`] — file bytes don't hash to
    ///   the declared `oid`. The in-flight multipart upload is
    ///   [`MultipartUpload::abort`]ed before the error surfaces so S3
    ///   doesn't accumulate orphan parts.
    /// - [`LfsError::Io`] — local file read failed.
    /// - [`LfsError::Storage`] — underlying object-store failure after retries.
    pub async fn put_stream(&self, oid: &[u8; 32], file_path: &StdPath) -> Result<()> {
        self.put_stream_with_size(oid, None, file_path).await
    }

    /// Streams and verifies an LFS object with an expected pointer size.
    ///
    /// The size check is performed during the same pass as SHA-256 hashing,
    /// so a pointer with a conflicting declared size cannot publish bytes.
    pub async fn put_stream_with_size(
        &self,
        oid: &[u8; 32],
        expected_size: Option<u64>,
        file_path: &StdPath,
    ) -> Result<()> {
        let path = self.object_path(oid);

        if let ExistingObject::Valid(actual_size) = self.inspect_existing(&path, oid).await? {
            if expected_size.is_some_and(|expected| expected != actual_size) {
                return Err(LfsError::ObjectCorrupt {
                    oid: hex_encode(oid),
                });
            }
            self.record_verification_receipt(oid).await;
            return Ok(());
        }

        let mut file = tokio::fs::File::open(file_path)
            .await
            .map_err(|e| annotate_io_error(e, file_path))?;

        // Begin the multipart upload. A failure here short-circuits
        // before we read any file bytes, so there's nothing to clean
        // up on the remote.
        let inner = self.store.inner();
        let mut upload = inner
            .put_multipart(&path)
            .await
            .map_err(|e| LfsError::from(crab_storage::map_object_store_error(e, path.as_ref())))?;

        // Drive the upload with a bounded FuturesUnordered so we always
        // keep `MAX_IN_FLIGHT_PARTS` in flight without unbounded growth.
        // The hasher runs on the read side, inline with the buffer
        // accumulation, so the whole pipeline is one pass over the
        // file bytes.
        let hash_result = stream_file_parts(
            &mut file,
            &mut *upload,
            oid,
            expected_size,
            file_path,
            &path,
        )
        .await;

        match hash_result {
            Ok(()) => {
                upload.complete().await.map_err(|e| {
                    LfsError::from(crab_storage::map_object_store_error(e, path.as_ref()))
                })?;
                self.record_verification_receipt(oid).await;
                Ok(())
            }
            Err(e) => {
                // Abort is best-effort — we already have a failure to
                // surface. Log the abort outcome at debug since it
                // doesn't change the user-visible result.
                if let Err(abort_err) = upload.abort().await {
                    tracing::debug!(
                        path = %path,
                        error = %abort_err,
                        "multipart abort after upload failure also failed"
                    );
                }
                Err(e)
            }
        }
    }

    /// Downloads the raw bytes of an LFS object.
    ///
    /// # Errors
    ///
    /// Returns [`LfsError::ObjectMissing`] if the object does not exist.
    pub async fn get(&self, oid: &[u8; 32]) -> Result<Bytes> {
        match Self::get_from(&self.store, &self.prefix, oid).await {
            Ok(bytes) => Ok(bytes),
            Err(e) => {
                let Some((fallback_store, fallback_prefix)) = self.primary_fallback.as_ref() else {
                    return Err(e);
                };
                tracing::debug!(
                    oid = %hex_encode(oid),
                    error = %e,
                    "LFS read from selected remote failed; retrying primary"
                );
                Self::get_from(fallback_store, fallback_prefix, oid).await
            }
        }
    }

    /// Checks whether an LFS object exists without downloading its body.
    pub async fn exists(&self, oid: &[u8; 32]) -> Result<bool> {
        let path = self.object_path(oid);
        match self.exists_at_path(&path).await {
            Ok(true) => Ok(true),
            Ok(false) => {
                let Some((fallback_store, fallback_prefix)) = self.primary_fallback.as_ref() else {
                    return Ok(false);
                };
                let path = Self::object_path_at(fallback_prefix, oid);
                Self::exists_at(fallback_store, &path).await
            }
            Err(e) => {
                let Some((fallback_store, fallback_prefix)) = self.primary_fallback.as_ref() else {
                    return Err(e);
                };
                tracing::debug!(
                    oid = %hex_encode(oid),
                    error = %e,
                    "LFS HEAD from selected remote failed; retrying primary"
                );
                let path = Self::object_path_at(fallback_prefix, oid);
                Self::exists_at(fallback_store, &path).await
            }
        }
    }

    /// Downloads an LFS object and re-hashes it to verify integrity.
    ///
    /// # Errors
    ///
    /// Returns [`LfsError::ObjectCorrupt`] if the stored bytes'
    /// SHA-256 does not match the OID.
    /// Returns [`LfsError::ObjectMissing`] if the object does not exist.
    pub async fn verify(&self, oid: &[u8; 32]) -> Result<Bytes> {
        let bytes = self.get(oid).await?;
        let actual = Sha256::digest(&bytes);
        if actual.as_slice() != oid {
            return Err(LfsError::ObjectCorrupt {
                oid: hex_encode(oid),
            });
        }
        Ok(bytes)
    }

    /// Verifies an LFS object without retaining its body in memory.
    pub async fn verify_size(&self, oid: &[u8; 32], expected_size: u64) -> Result<()> {
        match Self::verify_size_at(&self.store, &self.prefix, oid, expected_size).await {
            Ok(()) => {
                Self::record_verification_receipt_at(&self.store, &self.prefix, oid).await;
                Ok(())
            }
            Err(error) => {
                let Some((fallback_store, fallback_prefix)) = self.primary_fallback.as_ref() else {
                    return Err(error);
                };
                tracing::debug!(
                    oid = %hex_encode(oid),
                    error = %error,
                    "LFS verification from selected remote failed; retrying primary"
                );
                match Self::verify_size_at(fallback_store, fallback_prefix, oid, expected_size)
                    .await
                {
                    Ok(()) => {
                        Self::record_verification_receipt_at(fallback_store, fallback_prefix, oid)
                            .await;
                        Ok(())
                    }
                    Err(error) => Err(error),
                }
            }
        }
    }

    /// Streams an LFS object to a local file while verifying its size and
    /// SHA-256 digest.
    ///
    /// The destination is truncated before each selected-source attempt. A
    /// replica read that is missing, corrupt, or unavailable is retried from
    /// the configured primary fallback; local I/O errors are returned without
    /// hiding them behind a remote retry.
    pub async fn download_to_file(
        &self,
        oid: &[u8; 32],
        expected_size: u64,
        destination: &StdPath,
    ) -> Result<()> {
        match Self::download_from_to_file(
            &self.store,
            &self.prefix,
            oid,
            expected_size,
            destination,
        )
        .await
        {
            Ok(()) => Ok(()),
            Err(error @ LfsError::Io { .. }) => Err(error),
            Err(error) => {
                let Some((fallback_store, fallback_prefix)) = self.primary_fallback.as_ref() else {
                    return Err(error);
                };
                tracing::debug!(
                    oid = %hex_encode(oid),
                    error = %error,
                    "LFS streaming read from selected remote failed; retrying primary"
                );
                Self::download_from_to_file(
                    fallback_store,
                    fallback_prefix,
                    oid,
                    expected_size,
                    destination,
                )
                .await
            }
        }
    }

    /// Deletes an LFS object from the store.
    ///
    /// Behavior on missing objects depends on the backend: some backends
    /// treat delete as idempotent, others return an error. When the
    /// backend reports `NotFound`, this method maps it to
    /// [`LfsError::ObjectMissing`].
    pub async fn delete(&self, oid: &[u8; 32]) -> Result<()> {
        let path = self.object_path(oid);
        self.store.delete(&path).await.map_err(|e| match e {
            StorageError::NotFound { .. } => LfsError::ObjectMissing {
                oid: hex_encode(oid),
            },
            other => other.into(),
        })
    }

    /// HEAD check on a path — returns `true` if the object exists.
    async fn exists_at_path(&self, path: &Path) -> Result<bool> {
        Self::exists_at(&self.store, path).await
    }

    async fn exists_at(store: &Store, path: &Path) -> Result<bool> {
        match store.head(path).await {
            Ok(_) => Ok(true),
            Err(StorageError::NotFound { .. }) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    async fn get_from(store: &Store, prefix: &str, oid: &[u8; 32]) -> Result<Bytes> {
        let path = Self::object_path_at(prefix, oid);
        match store.get_with_etag(&path).await {
            Ok((bytes, _etag)) => Ok(bytes),
            Err(StorageError::NotFound { .. }) => Err(LfsError::ObjectMissing {
                oid: hex_encode(oid),
            }),
            Err(e) => Err(e.into()),
        }
    }

    async fn verify_size_at(
        store: &Store,
        prefix: &str,
        oid: &[u8; 32],
        expected_size: u64,
    ) -> Result<()> {
        let path = Self::object_path_at(prefix, oid);
        match Self::inspect_existing_at(store, prefix, &path, oid).await? {
            ExistingObject::Valid(size) => {
                if size == expected_size {
                    Ok(())
                } else {
                    Err(LfsError::ObjectCorrupt {
                        oid: hex_encode(oid),
                    })
                }
            }
            ExistingObject::Missing => Err(LfsError::ObjectMissing {
                oid: hex_encode(oid),
            }),
            ExistingObject::Corrupt(_) => Err(LfsError::ObjectCorrupt {
                oid: hex_encode(oid),
            }),
        }
    }

    async fn get_verified_stream_at(
        store: &Store,
        prefix: &str,
        oid: &[u8; 32],
        expected_size: u64,
        range: Option<Range<u64>>,
    ) -> Result<(ObjectMeta, Range<u64>, LfsByteStream)> {
        Self::verify_size_at(store, prefix, oid, expected_size).await?;
        Self::record_verification_receipt_at(store, prefix, oid).await;
        let path = Self::object_path_at(prefix, oid);
        let (meta, result_range, stream) = store
            .get_stream(&path, range)
            .await
            .map_err(LfsError::from)?;
        if meta.size != expected_size
            || result_range.start > result_range.end
            || result_range.end > meta.size
        {
            return Err(LfsError::ObjectCorrupt {
                oid: hex_encode(oid),
            });
        }
        let stream = stream.map(|chunk| chunk.map_err(Into::into)).boxed();
        Ok((meta, result_range, stream))
    }

    async fn download_from_to_file(
        store: &Store,
        prefix: &str,
        oid: &[u8; 32],
        expected_size: u64,
        destination: &StdPath,
    ) -> Result<()> {
        if let Some(parent) = destination.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let path = Self::object_path_at(prefix, oid);
        let (meta, range, mut stream) = match store.get_stream(&path, None).await {
            Ok(result) => result,
            Err(StorageError::NotFound { .. }) => {
                return Err(LfsError::ObjectMissing {
                    oid: hex_encode(oid),
                });
            }
            Err(error) => return Err(error.into()),
        };
        if range.start != 0 || range.end != meta.size || meta.size != expected_size {
            return Err(LfsError::ObjectCorrupt {
                oid: hex_encode(oid),
            });
        }

        let mut file = tokio::fs::File::create(destination).await?;
        let mut hasher = Sha256::new();
        let mut actual_size = 0u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            actual_size = actual_size.checked_add(chunk.len() as u64).ok_or_else(|| {
                StorageError::CorruptObject {
                    path: path.to_string(),
                    reason: "LFS object size overflow while downloading".to_owned(),
                }
            })?;
            hasher.update(&chunk);
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        file.sync_all().await?;

        if actual_size != expected_size || hasher.finalize().as_slice() != oid {
            return Err(LfsError::ObjectCorrupt {
                oid: hex_encode(oid),
            });
        }
        Ok(())
    }

    /// Replaces a corrupt same-key object only if the inspected version is
    /// still current, then re-verifies the winning bytes.
    async fn replace_corrupt(
        &self,
        path: &Path,
        oid: &[u8; 32],
        bytes: Bytes,
        etag: ETag,
    ) -> Result<()> {
        if !object_matches(oid, &bytes) {
            return Err(LfsError::ObjectCorrupt {
                oid: hex_encode(oid),
            });
        }

        if let Err(update_error) = self.store.update(path, bytes, etag).await {
            // A racing repair is successful only when its winning bytes are
            // valid; otherwise preserve the conditional-write failure.
            if self.verify_at_path(path, oid).await.is_ok() {
                self.record_verification_receipt(oid).await;
                return Ok(());
            }
            return Err(update_error.into());
        }

        self.verify_at_path(path, oid).await?;
        self.record_verification_receipt(oid).await;
        Ok(())
    }

    async fn record_verification_receipt(&self, oid: &[u8; 32]) {
        Self::record_verification_receipt_at(&self.store, &self.prefix, oid).await;
    }

    async fn record_verification_receipt_at(store: &Store, prefix: &str, oid: &[u8; 32]) {
        let object_path = Self::object_path_at(prefix, oid);
        let meta = match store.head(&object_path).await {
            Ok(meta) => meta,
            Err(error) => {
                tracing::debug!(
                    oid = %hex_encode(oid),
                    error = %error,
                    "could not read LFS object metadata for verification receipt"
                );
                return;
            }
        };
        if meta.e_tag.is_none() && meta.version.is_none() {
            // A receipt without a provider validator cannot prove that the
            // bytes observed later are the bytes verified here.
            return;
        }

        let receipt = VerificationReceipt {
            oid: *oid,
            size: meta.size,
            object_path: object_path.to_string(),
            e_tag: meta.e_tag.clone(),
            version: meta.version.clone(),
            verifier: RECEIPT_VERIFIER.to_owned(),
        };
        let Ok(body) = encode_receipt(&receipt) else {
            tracing::debug!(
                oid = %hex_encode(oid),
                "could not encode LFS verification receipt"
            );
            return;
        };
        let receipt_path = receipt_path_at(prefix, oid);
        if let Err(error) = store.put_overwrite(&receipt_path, Bytes::from(body)).await {
            // Receipts accelerate future presence checks; losing one is safe
            // because the next operation falls back to streamed hashing.
            tracing::debug!(
                oid = %hex_encode(oid),
                error = %error,
                "could not persist LFS verification receipt"
            );
        }
    }

    async fn verify_at_path(&self, path: &Path, oid: &[u8; 32]) -> Result<()> {
        match self.inspect_existing(path, oid).await? {
            ExistingObject::Valid(_) => Ok(()),
            ExistingObject::Missing => Err(LfsError::ObjectMissing {
                oid: hex_encode(oid),
            }),
            ExistingObject::Corrupt(_) => Err(LfsError::ObjectCorrupt {
                oid: hex_encode(oid),
            }),
        }
    }

    async fn inspect_existing(&self, path: &Path, oid: &[u8; 32]) -> Result<ExistingObject> {
        Self::inspect_existing_at(&self.store, &self.prefix, path, oid).await
    }

    async fn inspect_existing_at(
        store: &Store,
        prefix: &str,
        path: &Path,
        oid: &[u8; 32],
    ) -> Result<ExistingObject> {
        let meta = match store.head(path).await {
            Ok(meta) => meta,
            Err(StorageError::NotFound { .. }) => return Ok(ExistingObject::Missing),
            Err(error) => return Err(error.into()),
        };
        if receipt_matches(store, prefix, path, oid, &meta).await {
            return Ok(ExistingObject::Valid(meta.size));
        }

        let (meta, range, mut stream) = match store.get_stream(path, None).await {
            Ok(result) => result,
            Err(StorageError::NotFound { .. }) => return Ok(ExistingObject::Missing),
            Err(error) => return Err(error.into()),
        };
        let etag = ETag {
            e_tag: meta.e_tag.clone(),
            version: meta.version.clone(),
        };
        let mut hasher = Sha256::new();
        let mut actual_size = 0u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            hasher.update(&chunk);
            actual_size = actual_size.checked_add(chunk.len() as u64).ok_or_else(|| {
                StorageError::CorruptObject {
                    path: path.to_string(),
                    reason: "object size overflow while verifying LFS content".to_owned(),
                }
            })?;
        }
        let expected_size = range.end.saturating_sub(range.start);
        if range.start != 0 || expected_size != meta.size || actual_size != expected_size {
            return Err(StorageError::CorruptObject {
                path: path.to_string(),
                reason: format!(
                    "incomplete LFS object body: expected {} bytes, read {actual_size}",
                    meta.size
                ),
            }
            .into());
        }
        if hasher.finalize().as_slice() == oid {
            Ok(ExistingObject::Valid(meta.size))
        } else {
            Ok(ExistingObject::Corrupt(etag))
        }
    }
}

fn receipt_path_at(prefix: &str, oid: &[u8; 32]) -> Path {
    let hex = hex_encode(oid);
    let prefix = prefix.trim_matches('/');
    let path = if prefix.is_empty() {
        format!("lfs/receipts/{}/{}/{}.bin", &hex[..2], &hex[2..4], hex)
    } else {
        format!(
            "{prefix}/lfs/receipts/{}/{}/{}.bin",
            &hex[..2],
            &hex[2..4],
            hex
        )
    };
    Path::from(path)
}

async fn receipt_matches(
    store: &Store,
    prefix: &str,
    object_path: &Path,
    oid: &[u8; 32],
    meta: &ObjectMeta,
) -> bool {
    if meta.e_tag.is_none() && meta.version.is_none() {
        return false;
    }
    let receipt_path = receipt_path_at(prefix, oid);
    let (body, _) = match store
        .get_with_etag_bounded(&receipt_path, MAX_RECEIPT_SIZE)
        .await
    {
        Ok(result) => result,
        Err(error) => {
            tracing::debug!(
                oid = %hex_encode(oid),
                error = %error,
                "LFS verification receipt unavailable; hashing object body"
            );
            return false;
        }
    };
    let Some(receipt) = decode_receipt(&body) else {
        tracing::debug!(
            oid = %hex_encode(oid),
            "LFS verification receipt malformed; hashing object body"
        );
        return false;
    };
    receipt.oid == *oid
        && receipt.size == meta.size
        && receipt.object_path == object_path.to_string()
        && receipt.e_tag == meta.e_tag
        && receipt.version == meta.version
        && receipt.verifier == RECEIPT_VERIFIER
}

fn encode_receipt(receipt: &VerificationReceipt) -> Result<Vec<u8>> {
    let mut body = Vec::with_capacity(256);
    body.extend_from_slice(RECEIPT_MAGIC);
    body.extend_from_slice(&receipt.oid);
    body.extend_from_slice(&receipt.size.to_be_bytes());
    append_receipt_field(&mut body, receipt.object_path.as_bytes())?;
    append_receipt_option(&mut body, receipt.e_tag.as_deref())?;
    append_receipt_option(&mut body, receipt.version.as_deref())?;
    append_receipt_field(&mut body, receipt.verifier.as_bytes())?;
    Ok(body)
}

fn append_receipt_field(body: &mut Vec<u8>, value: &[u8]) -> Result<()> {
    let length = u32::try_from(value.len()).map_err(|_| LfsError::Storage {
        source: StorageError::CorruptObject {
            path: "lfs/receipts".to_owned(),
            reason: "verification receipt field is too large".to_owned(),
        },
    })?;
    body.extend_from_slice(&length.to_be_bytes());
    body.extend_from_slice(value);
    Ok(())
}

fn append_receipt_option(body: &mut Vec<u8>, value: Option<&str>) -> Result<()> {
    match value {
        Some(value) => {
            body.push(1);
            append_receipt_field(body, value.as_bytes())?;
        }
        None => body.push(0),
    }
    Ok(())
}

fn decode_receipt(body: &[u8]) -> Option<VerificationReceipt> {
    let mut cursor = 0;
    take_receipt_bytes(body, &mut cursor, RECEIPT_MAGIC.len())
        .filter(|magic| *magic == RECEIPT_MAGIC)?;
    let oid = take_receipt_bytes(body, &mut cursor, 32)?;
    let mut oid_bytes = [0u8; 32];
    oid_bytes.copy_from_slice(oid);
    let size_bytes = take_receipt_bytes(body, &mut cursor, 8)?;
    let size = u64::from_be_bytes(size_bytes.try_into().ok()?);
    let object_path = receipt_string(body, &mut cursor)?;
    let e_tag = receipt_option(body, &mut cursor)?;
    let version = receipt_option(body, &mut cursor)?;
    let verifier = receipt_string(body, &mut cursor)?;
    if cursor != body.len() {
        return None;
    }
    Some(VerificationReceipt {
        oid: oid_bytes,
        size,
        object_path,
        e_tag,
        version,
        verifier,
    })
}

fn receipt_string(body: &[u8], cursor: &mut usize) -> Option<String> {
    let value = receipt_field(body, cursor)?;
    String::from_utf8(value.to_owned()).ok()
}

fn receipt_option(body: &[u8], cursor: &mut usize) -> Option<Option<String>> {
    let present = *take_receipt_bytes(body, cursor, 1)?.first()?;
    match present {
        0 => Some(None),
        1 => receipt_string(body, cursor).map(Some),
        _ => None,
    }
}

fn receipt_field<'a>(body: &'a [u8], cursor: &mut usize) -> Option<&'a [u8]> {
    let length = take_receipt_bytes(body, cursor, 4)?;
    let length = u32::from_be_bytes(length.try_into().ok()?) as usize;
    if length > MAX_RECEIPT_FIELD_SIZE {
        return None;
    }
    take_receipt_bytes(body, cursor, length)
}

fn take_receipt_bytes<'a>(body: &'a [u8], cursor: &mut usize, length: usize) -> Option<&'a [u8]> {
    let end = cursor.checked_add(length)?;
    let value = body.get(*cursor..end)?;
    *cursor = end;
    Some(value)
}

fn object_matches(oid: &[u8; 32], bytes: &[u8]) -> bool {
    Sha256::digest(bytes).as_slice() == oid
}

/// Read `file` in [`FILE_READ_BUF`]-sized chunks, accumulate into
/// [`STREAM_PART_SIZE`] parts, and push each part to `upload` with at
/// most [`MAX_IN_FLIGHT_PARTS`] in flight. Every byte is fed to a
/// SHA-256 hasher as it leaves the file; the final digest is compared
/// against `oid` before returning success.
///
/// This function owns the read loop and the in-flight part queue
/// exclusively so the surrounding put_stream can abort the upload on
/// any error without fighting the borrow checker for &mut MultipartUpload.
async fn stream_file_parts(
    file: &mut tokio::fs::File,
    upload: &mut dyn MultipartUpload,
    expected_oid: &[u8; 32],
    expected_size: Option<u64>,
    file_path: &StdPath,
    remote_path: &Path,
) -> Result<()> {
    use futures_util::stream::{FuturesUnordered, StreamExt};

    let mut hasher = Sha256::new();
    let mut pending: FuturesUnordered<
        std::pin::Pin<Box<dyn std::future::Future<Output = object_store::Result<()>> + Send>>,
    > = FuturesUnordered::new();

    // `buf` is the currently-assembling part; we flush it as a part
    // whenever it reaches STREAM_PART_SIZE. Pre-allocated to avoid
    // reallocation during the common full-part case.
    let mut buf: Vec<u8> = Vec::with_capacity(STREAM_PART_SIZE);
    // Scratch buffer for reads from the file. Sized the same as the
    // part size so a single read in the best case produces a complete
    // part without any partial accumulation.
    let mut read_buf = vec![0u8; FILE_READ_BUF];
    let mut total_bytes_read: u64 = 0;

    loop {
        let n = file
            .read(&mut read_buf)
            .await
            .map_err(|e| annotate_io_error(e, file_path))?;

        if n == 0 {
            // EOF. Flush whatever remains in `buf` as the final part.
            if !buf.is_empty() {
                dispatch_part(upload, &mut buf, &mut pending)?;
            }
            break;
        }

        total_bytes_read += n as u64;
        hasher.update(&read_buf[..n]);
        buf.extend_from_slice(&read_buf[..n]);

        // Drain complete parts out of `buf` while it's large enough.
        // Multiple loop iterations handle the (rare) case where a
        // single read delivered more than one part's worth of bytes.
        while buf.len() >= STREAM_PART_SIZE {
            // Backpressure: if we're at the concurrency ceiling, wait
            // for one in-flight part to complete before dispatching a
            // new one. This bounds peak memory to the part size times
            // MAX_IN_FLIGHT_PARTS regardless of file size.
            if pending.len() >= MAX_IN_FLIGHT_PARTS
                && let Some(result) = pending.next().await
            {
                result.map_err(|e| {
                    LfsError::from(crab_storage::map_object_store_error(
                        e,
                        remote_path.as_ref(),
                    ))
                })?;
            }

            // Peel one STREAM_PART_SIZE chunk off the front of `buf`
            // and dispatch it. `split_off` + swap keeps the remainder
            // (if any) in `buf` for the next iteration without an
            // extra copy.
            let mut part = buf;
            let tail = part.split_off(STREAM_PART_SIZE);
            buf = tail;
            dispatch_part_owned(upload, part, &mut pending)?;
        }
    }

    // Drain remaining in-flight parts. Any failure aborts the whole
    // upload — the caller will call MultipartUpload::abort.
    while let Some(result) = pending.next().await {
        result.map_err(|e| {
            LfsError::from(crab_storage::map_object_store_error(
                e,
                remote_path.as_ref(),
            ))
        })?;
    }

    // SHA-256 verification runs AFTER all parts have been accepted by
    // the remote but BEFORE CompleteMultipartUpload. A mismatch here
    // surfaces as an aborted multipart — never a live, hash-mismatched
    // object on S3.
    let actual = hasher.finalize();
    if expected_size.is_some_and(|expected| expected != total_bytes_read)
        || actual.as_slice() != expected_oid
    {
        tracing::warn!(
            path = %remote_path,
            bytes_read = total_bytes_read,
            "LFS streaming upload: computed SHA-256 differs from declared OID"
        );
        return Err(LfsError::ObjectCorrupt {
            oid: hex_encode(expected_oid),
        });
    }

    tracing::debug!(
        path = %remote_path,
        bytes = total_bytes_read,
        "LFS streaming upload: hash verified, ready to complete"
    );

    Ok(())
}

/// Wraps an I/O error with the source path so callers can report which
/// local file the upload was reading.
fn annotate_io_error(source: std::io::Error, file_path: &StdPath) -> LfsError {
    let wrapped = std::io::Error::new(
        source.kind(),
        format!("{} (reading {})", source, file_path.display()),
    );
    LfsError::Io { source: wrapped }
}

/// Dispatch the accumulated buffer as a new part, leaving `buf` empty
/// and ready to accept more bytes. Used when `buf` is moved in its
/// entirety (EOF with a partial final part).
fn dispatch_part(
    upload: &mut dyn MultipartUpload,
    buf: &mut Vec<u8>,
    pending: &mut futures_util::stream::FuturesUnordered<
        std::pin::Pin<Box<dyn std::future::Future<Output = object_store::Result<()>> + Send>>,
    >,
) -> Result<()> {
    let part_bytes = std::mem::take(buf);
    dispatch_part_owned(upload, part_bytes, pending)
}

/// Dispatch a caller-owned `Vec<u8>` as a new part. Zero-copies into
/// `Bytes` via `Bytes::from(Vec<u8>)` so the allocation travels with
/// the in-flight future.
fn dispatch_part_owned(
    upload: &mut dyn MultipartUpload,
    part_bytes: Vec<u8>,
    pending: &mut futures_util::stream::FuturesUnordered<
        std::pin::Pin<Box<dyn std::future::Future<Output = object_store::Result<()>> + Send>>,
    >,
) -> Result<()> {
    let payload: PutPayload = Bytes::from(part_bytes).into();
    let fut = upload.put_part(payload);
    pending.push(Box::pin(fut));
    Ok(())
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]
mod tests {
    use std::io::Write as _;
    use std::sync::Arc;

    use bytes::Bytes;
    use object_store::memory::InMemory;
    use sha2::{Digest, Sha256};

    use super::*;
    use crab_storage::RetryPolicy;

    fn test_store() -> LfsObjectStore {
        LfsObjectStore::new(test_base_store(), "repo")
    }

    fn test_base_store() -> Store {
        let inner: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
        let policy = RetryPolicy {
            max_attempts: 2,
            base: std::time::Duration::from_millis(1),
            cap: std::time::Duration::from_millis(5),
        };
        Store::with_retry(inner, policy)
    }

    fn sha256_oid(data: &[u8]) -> [u8; 32] {
        let hash = Sha256::digest(data);
        let mut oid = [0u8; 32];
        oid.copy_from_slice(&hash);
        oid
    }

    #[test]
    fn object_path_format() {
        let store = test_store();
        // OID where first two bytes are 0xab, 0xcd
        let mut oid = [0u8; 32];
        oid[0] = 0xab;
        oid[1] = 0xcd;
        let path = store.object_path(&oid);
        let path_str = path.to_string();
        let hex = hex_encode(&oid);
        assert!(path_str.starts_with("repo/lfs/objects/ab/cd/"));
        assert!(path_str.ends_with(&hex));
    }

    #[test]
    fn object_path_for_prefix_matches_store_layout() {
        let store = test_store();
        let mut oid = [0u8; 32];
        oid[0] = 0xab;
        oid[1] = 0xcd;

        assert_eq!(
            LfsObjectStore::object_path_for_prefix("repo", &oid),
            store.object_path_for(&oid)
        );
    }

    #[test]
    fn object_path_for_empty_prefix_has_no_leading_slash() {
        let mut oid = [0u8; 32];
        oid[0] = 0xab;
        oid[1] = 0xcd;

        assert_eq!(
            LfsObjectStore::object_path_for_prefix("", &oid).as_ref(),
            "lfs/objects/ab/cd/abcd000000000000000000000000000000000000000000000000000000000000"
        );
    }

    #[test]
    fn verification_receipt_round_trip_is_versioned_and_exact() {
        let oid = [0x42u8; 32];
        let receipt = VerificationReceipt {
            oid,
            size: 17,
            object_path: "repo/lfs/objects/42/42/object".to_owned(),
            e_tag: Some("etag-value".to_owned()),
            version: Some("version-value".to_owned()),
            verifier: RECEIPT_VERIFIER.to_owned(),
        };

        let encoded = encode_receipt(&receipt).unwrap();
        assert_eq!(&encoded[..RECEIPT_MAGIC.len()], RECEIPT_MAGIC);
        assert_eq!(decode_receipt(&encoded).map(|_| ()), Some(()));

        let mut trailing = encoded;
        trailing.push(0);
        assert!(decode_receipt(&trailing).is_none());
    }

    #[tokio::test]
    async fn put_get_roundtrip() {
        let store = test_store();
        let data = b"hello LFS world";
        let oid = sha256_oid(data);
        let bytes = Bytes::from_static(data);

        store.put(&oid, bytes.clone()).await.unwrap();
        let got = store.get(&oid).await.unwrap();
        assert_eq!(got, bytes);
    }

    #[tokio::test]
    async fn put_rejects_mismatched_hash() {
        let store = test_store();
        let wrong_oid = [0xffu8; 32];
        let bytes = Bytes::from_static(b"content");

        let err = store
            .put(&wrong_oid, bytes)
            .await
            .expect_err("mismatched hash must be rejected");
        assert!(matches!(err, LfsError::ObjectCorrupt { .. }));
    }

    #[tokio::test]
    async fn put_is_idempotent() {
        let store = test_store();
        let data = b"idempotent content";
        let oid = sha256_oid(data);
        let bytes = Bytes::from(data.to_vec());

        store.put(&oid, bytes.clone()).await.unwrap();
        store.put(&oid, bytes).await.unwrap();
    }

    #[tokio::test]
    async fn put_conditionally_repairs_corrupt_existing_object() {
        let store = test_store();
        let data = Bytes::from_static(b"correct object");
        let oid = sha256_oid(&data);
        let path = store.object_path_for(&oid);
        store
            .store()
            .inner()
            .put(&path, Bytes::from_static(b"corrupt").into())
            .await
            .unwrap();

        store.put(&oid, data.clone()).await.unwrap();

        assert_eq!(store.verify(&oid).await.unwrap(), data);
    }

    #[tokio::test]
    async fn get_missing_returns_lfs_object_missing() {
        let store = test_store();
        let oid = [0x42u8; 32];

        let err = store
            .get(&oid)
            .await
            .expect_err("missing object must error");
        assert!(matches!(err, LfsError::ObjectMissing { .. }));
    }

    #[tokio::test]
    async fn get_uses_primary_fallback_after_selected_remote_miss() {
        let selected = test_base_store();
        let primary = test_base_store();
        let primary_lfs = LfsObjectStore::new(primary.clone(), "repo");
        let data = Bytes::from_static(b"primary fallback LFS object");
        let oid = sha256_oid(&data);
        primary_lfs.put(&oid, data.clone()).await.unwrap();

        let store = LfsObjectStore::new_with_primary_fallback(selected, "repo", primary, "repo");

        let got = store.get(&oid).await.unwrap();
        assert_eq!(got, data);
    }

    #[tokio::test]
    async fn get_stream_uses_primary_fallback_after_selected_remote_corruption() {
        let selected = test_base_store();
        let primary = test_base_store();
        let primary_lfs = LfsObjectStore::new(primary.clone(), "repo");
        let data = Bytes::from_static(b"primary stream integrity fallback");
        let oid = sha256_oid(&data);
        primary_lfs.put(&oid, data.clone()).await.unwrap();

        let selected_lfs = LfsObjectStore::new(selected.clone(), "repo");
        let selected_path = selected_lfs.object_path_for(&oid);
        selected
            .inner()
            .put(&selected_path, Bytes::from(vec![b'x'; data.len()]).into())
            .await
            .unwrap();

        let store = LfsObjectStore::new_with_primary_fallback(selected, "repo", primary, "repo");
        let (_, range, mut stream) = store
            .get_stream(&oid, data.len() as u64, None)
            .await
            .unwrap();
        let mut got = Vec::new();
        while let Some(chunk) = stream.next().await {
            got.extend_from_slice(&chunk.unwrap());
        }

        assert_eq!(range, 0..data.len() as u64);
        assert_eq!(got, data);
    }

    #[tokio::test]
    async fn exists_returns_false_for_missing() {
        let store = test_store();
        let oid = [0x42u8; 32];
        assert!(!store.exists(&oid).await.unwrap());
    }

    #[tokio::test]
    async fn exists_returns_true_after_put() {
        let store = test_store();
        let data = b"exists check";
        let oid = sha256_oid(data);

        store.put(&oid, Bytes::from(data.to_vec())).await.unwrap();
        assert!(store.exists(&oid).await.unwrap());
    }

    #[tokio::test]
    async fn verify_succeeds_for_valid_object() {
        let store = test_store();
        let data = b"verify me";
        let oid = sha256_oid(data);
        let bytes = Bytes::from(data.to_vec());

        store.put(&oid, bytes.clone()).await.unwrap();
        let got = store.verify(&oid).await.unwrap();
        assert_eq!(got, bytes);
    }

    #[tokio::test]
    async fn verify_size_backfills_validator_receipt_for_external_object() {
        let raw_store = test_base_store();
        let store = LfsObjectStore::new(raw_store.clone(), "repo");
        let data = Bytes::from_static(b"external object");
        let oid = sha256_oid(&data);
        let object_path = store.object_path_for(&oid);
        raw_store
            .inner()
            .put(&object_path, data.clone().into())
            .await
            .unwrap();

        store.verify_size(&oid, data.len() as u64).await.unwrap();

        let receipt_path = receipt_path_at("repo", &oid);
        assert!(raw_store.head(&receipt_path).await.is_ok());
    }

    #[tokio::test]
    async fn download_to_file_streams_and_verifies_object() {
        let store = test_store();
        let data = b"stream this object to disk";
        let oid = sha256_oid(data);
        store.put(&oid, Bytes::from_static(data)).await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("nested").join("object");

        store
            .download_to_file(&oid, data.len() as u64, &destination)
            .await
            .unwrap();

        assert_eq!(std::fs::read(destination).unwrap(), data);
    }

    #[tokio::test]
    async fn download_to_file_rejects_wrong_declared_size() {
        let store = test_store();
        let data = b"size matters";
        let oid = sha256_oid(data);
        store.put(&oid, Bytes::from_static(data)).await.unwrap();
        let dir = tempfile::tempdir().unwrap();

        let error = store
            .download_to_file(&oid, data.len() as u64 + 1, &dir.path().join("object"))
            .await
            .unwrap_err();

        assert!(matches!(error, LfsError::ObjectCorrupt { .. }));
    }

    #[tokio::test]
    async fn download_to_file_falls_back_after_replica_corruption() {
        let selected = test_base_store();
        let primary = test_base_store();
        let primary_lfs = LfsObjectStore::new(primary.clone(), "repo");
        let data = Bytes::from_static(b"primary streaming fallback");
        let oid = sha256_oid(&data);
        primary_lfs.put(&oid, data.clone()).await.unwrap();

        let selected_lfs = LfsObjectStore::new(selected.clone(), "repo");
        let selected_path = selected_lfs.object_path_for(&oid);
        selected
            .inner()
            .put(&selected_path, Bytes::from_static(b"corrupt").into())
            .await
            .unwrap();
        let store = LfsObjectStore::new_with_primary_fallback(selected, "repo", primary, "repo");
        let dir = tempfile::tempdir().unwrap();
        let destination = dir.path().join("object");

        store
            .download_to_file(&oid, data.len() as u64, &destination)
            .await
            .unwrap();

        assert_eq!(std::fs::read(destination).unwrap(), data);
    }

    #[tokio::test]
    async fn verify_missing_returns_lfs_object_missing() {
        let store = test_store();
        let oid = [0x42u8; 32];

        let err = store
            .verify(&oid)
            .await
            .expect_err("missing object must error");
        assert!(matches!(err, LfsError::ObjectMissing { .. }));
    }

    #[tokio::test]
    async fn delete_removes_object() {
        let store = test_store();
        let data = b"delete me";
        let oid = sha256_oid(data);

        store.put(&oid, Bytes::from(data.to_vec())).await.unwrap();
        store.delete(&oid).await.unwrap();
        assert!(!store.exists(&oid).await.unwrap());
    }

    #[tokio::test]
    async fn delete_missing_is_backend_dependent() {
        let store = test_store();
        let oid = [0x42u8; 32];

        // InMemory backend treats delete as idempotent — no error for
        // missing keys. Real backends (S3) may return NotFound which
        // gets mapped to LfsError::ObjectMissing.
        let result = store.delete(&oid).await;
        assert!(result.is_ok());
    }

    // ---- streaming upload (put_stream) ----

    /// Build a file of the requested size whose contents are the
    /// repeating byte `fill`. Returns the temp file handle so the
    /// caller controls cleanup; the path is accessible via
    /// `.path()` for as long as the handle is alive.
    fn temp_file_of_size(size: usize, fill: u8) -> (tempfile::NamedTempFile, [u8; 32]) {
        use std::io::Write as _;
        let mut tmp = tempfile::NamedTempFile::new().expect("create tempfile");
        // Write in 1 MiB chunks so we don't hold size bytes in memory
        // at once during test-fixture setup for big-file tests.
        const CHUNK: usize = 1024 * 1024;
        let buf = vec![fill; CHUNK.min(size)];
        let mut remaining = size;
        let mut hasher = Sha256::new();
        while remaining > 0 {
            let n = remaining.min(CHUNK);
            tmp.write_all(&buf[..n]).expect("write tempfile");
            hasher.update(&buf[..n]);
            remaining -= n;
        }
        tmp.flush().expect("flush tempfile");
        let mut oid = [0u8; 32];
        oid.copy_from_slice(&hasher.finalize());
        (tmp, oid)
    }

    #[tokio::test]
    async fn put_stream_round_trip_small() {
        // Even though the streaming path is designed for large files,
        // it must also be correct for small files — transfer_agent
        // routes based on declared size, but put_stream itself should
        // never care.
        let store = test_store();
        let (tmp, oid) = temp_file_of_size(1024, 0x7a);

        store
            .put_stream(&oid, tmp.path())
            .await
            .expect("small put_stream succeeds");

        let got = store.get(&oid).await.expect("download");
        assert_eq!(got.len(), 1024);
        assert_eq!(Sha256::digest(&got).as_slice(), oid.as_slice());
    }

    #[tokio::test]
    async fn put_stream_round_trip_multipart() {
        // Size > STREAM_PART_SIZE forces at least two parts, exercising
        // the in-flight-part concurrency path. Kept at ~18 MiB so the
        // test stays fast; the bound-checking logic doesn't care about
        // absolute size, only that multiple parts are emitted.
        let store = test_store();
        let size = STREAM_PART_SIZE * 2 + 128; // 3 parts: 8, 8, 128 B
        let (tmp, oid) = temp_file_of_size(size, 0x55);

        store
            .put_stream(&oid, tmp.path())
            .await
            .expect("multi-part put_stream succeeds");

        let got = store.get(&oid).await.expect("download");
        assert_eq!(got.len(), size);
        // Full hash verification on the round-trip — proves bytes survived
        // the split/assemble across multiple parts unchanged.
        assert_eq!(Sha256::digest(&got).as_slice(), oid.as_slice());
    }

    #[tokio::test]
    async fn put_stream_detects_corruption_and_aborts() {
        // Lie about the OID: the file hashes to something else entirely.
        // The streaming upload should detect the mismatch AFTER parts
        // have been pushed but BEFORE CompleteMultipartUpload, aborting
        // the upload so no object lands on the remote.
        let store = test_store();
        let (tmp, _real_oid) = temp_file_of_size(STREAM_PART_SIZE + 1, 0x33);
        let fake_oid = [0x11u8; 32]; // won't match any real content

        let err = store
            .put_stream(&fake_oid, tmp.path())
            .await
            .expect_err("hash mismatch must error");
        assert!(
            matches!(err, LfsError::ObjectCorrupt { .. }),
            "expected ObjectCorrupt, got {err:?}"
        );

        // Critical: the object must NOT be visible on the remote. A
        // completed multipart would leave a live object whose hash
        // didn't match the declared OID — silently corrupting the LFS
        // store. The abort path prevents this.
        assert!(
            !store.exists(&fake_oid).await.unwrap(),
            "aborted multipart must not leave a live object"
        );
    }

    #[tokio::test]
    async fn put_stream_is_idempotent_on_existing_object() {
        // put followed by put_stream on the same OID: verifying the existing
        // bytes must accept them without replacing the object.
        let store = test_store();
        let data = b"idempotent streaming";
        let oid = sha256_oid(data);

        // First, put via the small-object path.
        store
            .put(&oid, Bytes::from(data.to_vec()))
            .await
            .expect("initial put");

        // Now write the same bytes to a file and call put_stream. It
        // must succeed without issuing a multipart upload.
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut tmp, data).unwrap();
        tmp.flush().unwrap();

        store
            .put_stream(&oid, tmp.path())
            .await
            .expect("idempotent put_stream succeeds");

        // And the content is still what we originally wrote.
        let got = store.get(&oid).await.unwrap();
        assert_eq!(got.as_ref(), data);
    }

    #[tokio::test]
    async fn put_stream_rejects_pointer_size_mismatch_on_existing_object() {
        let store = test_store();
        let data = b"size-bound existing object";
        let oid = sha256_oid(data);
        store.put(&oid, Bytes::from_static(data)).await.unwrap();
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut tmp, data).unwrap();
        tmp.flush().unwrap();

        let error = store
            .put_stream_with_size(&oid, Some(data.len() as u64 + 1), tmp.path())
            .await
            .unwrap_err();

        assert!(matches!(error, LfsError::ObjectCorrupt { .. }));
    }

    #[tokio::test]
    async fn put_stream_conditionally_repairs_corrupt_existing_object() {
        let store = test_store();
        let data = b"correct streamed object";
        let oid = sha256_oid(data);
        let path = store.object_path_for(&oid);
        store
            .store()
            .inner()
            .put(&path, Bytes::from_static(b"corrupt").into())
            .await
            .unwrap();
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut tmp, data).unwrap();
        tmp.flush().unwrap();

        store.put_stream(&oid, tmp.path()).await.unwrap();

        assert_eq!(store.verify(&oid).await.unwrap().as_ref(), data);
    }

    #[tokio::test]
    async fn put_stream_handles_zero_byte_file() {
        // Zero-byte files are a genuine LFS case: `touch empty.bin`
        // followed by `git add` tracks it through LFS. The streaming
        // path must not emit a zero-byte final part (S3 rejects that
        // in the middle of a multipart, and for the last part it's
        // legal but wasteful) and must still verify the hash.
        let store = test_store();
        let (tmp, oid) = temp_file_of_size(0, 0);

        store
            .put_stream(&oid, tmp.path())
            .await
            .expect("zero-byte streaming succeeds");

        assert!(store.exists(&oid).await.unwrap());
        let got = store.get(&oid).await.unwrap();
        assert_eq!(got.len(), 0);
    }

    #[tokio::test]
    async fn put_stream_part_boundary_exact() {
        // Exactly one part's worth — exercises the "flush final part"
        // path without the "partial final part" code path.
        let store = test_store();
        let (tmp, oid) = temp_file_of_size(STREAM_PART_SIZE, 0x42);

        store
            .put_stream(&oid, tmp.path())
            .await
            .expect("single-part exact streaming succeeds");

        let got = store.get(&oid).await.unwrap();
        assert_eq!(got.len(), STREAM_PART_SIZE);
        assert_eq!(Sha256::digest(&got).as_slice(), oid.as_slice());
    }
}
