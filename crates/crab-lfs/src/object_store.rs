//! LFS object storage in cloud object storage.
//!
//! Stores and retrieves LFS objects keyed by their SHA-256 OID using a
//! two-level fan-out directory layout (`{prefix}/lfs/objects/{aa}/{bb}/{oid}`).
//! Verifies integrity on upload and supports idempotent puts.

use std::path::Path as StdPath;

use bytes::Bytes;
use object_store::path::Path;
use object_store::{MultipartUpload, ObjectStoreExt, PutPayload};
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;

use crab_git::lfs_pointer::hex_encode;
use crab_storage::{StorageError, Store};

/// Result alias for LFS object storage operations.
pub type Result<T> = std::result::Result<T, LfsError>;

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
    /// Idempotent: if an object with the same OID already exists, the upload
    /// is skipped and the call succeeds.
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

        // Skip upload if the object already exists (idempotent put).
        if self.exists_at_path(&path).await? {
            return Ok(());
        }

        // The underlying Store.put uses PutMode::Create with idempotent
        // conflict handling, so a race between the exists check and the
        // put is harmless — the second writer sees CasConflict and the
        // Store resolves it by comparing content hashes.
        self.store.put(&path, bytes).await.map_err(Into::into)
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
    /// Idempotent: if an object already exists at the target path the
    /// upload short-circuits, just like [`Self::put`].
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
        let path = self.object_path(oid);

        // Idempotent short-circuit — same HEAD check as `put`. Saves a
        // full file read and multipart round-trip when the object is
        // already present.
        if self.exists_at_path(&path).await? {
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
        let hash_result = stream_file_parts(&mut file, &mut *upload, oid, file_path, &path).await;

        match hash_result {
            Ok(()) => {
                upload.complete().await.map_err(|e| {
                    LfsError::from(crab_storage::map_object_store_error(e, path.as_ref()))
                })?;
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
    if actual.as_slice() != expected_oid {
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
        // put followed by put_stream on the same OID: the stream path
        // must short-circuit via the HEAD check and not re-upload.
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
