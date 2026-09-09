//! Object-version races at LFS verification and serving boundaries.
#![allow(clippy::unwrap_used, clippy::panic, reason = "test assertions")]

use async_trait::async_trait;
use bytes::Bytes;
use crab_lfs::{LfsError, LfsObjectStore};
use crab_storage::Store;
use futures_util::{TryStreamExt, stream::BoxStream};
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    ObjectStoreExt, PutMultipartOptions, PutOptions, PutPayload, PutResult, memory::InMemory,
    path::Path,
};
use sha2::{Digest, Sha256};
use std::{
    fmt,
    io::Write,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

#[derive(Debug, Clone, Copy)]
enum Validator {
    Etag,
    Version,
    Weak,
    Missing,
}

#[derive(Debug)]
struct RacingStore {
    inner: Arc<InMemory>,
    path: Path,
    replace_on_head: Option<bool>,
    validator: Validator,
    reads: AtomicUsize,
}
impl fmt::Display for RacingStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("lfs-racing-store")
    }
}
#[async_trait]
impl ObjectStore for RacingStore {
    async fn put_opts(
        &self,
        path: &Path,
        payload: PutPayload,
        opts: PutOptions,
    ) -> object_store::Result<PutResult> {
        self.inner.put_opts(path, payload, opts).await
    }
    async fn put_multipart_opts(
        &self,
        path: &Path,
        opts: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        self.inner.put_multipart_opts(path, opts).await
    }
    async fn get_opts(&self, path: &Path, opts: GetOptions) -> object_store::Result<GetResult> {
        if *path == self.path
            && Some(opts.head) == self.replace_on_head
            && self.reads.fetch_add(1, Ordering::Relaxed) == 1
        {
            self.inner
                .put(path, Bytes::from_static(b"wrong").into())
                .await?;
        }
        let mut result = self.inner.get_opts(path, opts).await?;
        match self.validator {
            Validator::Etag => {}
            Validator::Version => {
                result.meta.version = result.meta.e_tag.take();
            }
            Validator::Weak => {
                result.meta.e_tag = result.meta.e_tag.map(|tag| format!("W/{tag}"));
            }
            Validator::Missing => {
                result.meta.e_tag = None;
                result.meta.version = None;
            }
        }
        Ok(result)
    }
    fn delete_stream(
        &self,
        paths: BoxStream<'static, object_store::Result<Path>>,
    ) -> BoxStream<'static, object_store::Result<Path>> {
        self.inner.delete_stream(paths)
    }
    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.inner.list(prefix)
    }
    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> object_store::Result<ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }
    async fn copy_opts(
        &self,
        from: &Path,
        to: &Path,
        opts: CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, opts).await
    }
}
fn fixture(
    replace_on_head: Option<bool>,
    validator: Validator,
) -> (Arc<InMemory>, LfsObjectStore, [u8; 32], Path) {
    let oid = Sha256::digest(b"hello").into();
    let path = LfsObjectStore::object_path_for_prefix("repo", &oid);
    let inner = Arc::new(InMemory::new());
    let store = Store::new(Arc::new(RacingStore {
        inner: inner.clone(),
        path: path.clone(),
        replace_on_head,
        validator,
        reads: AtomicUsize::new(0),
    }));
    (inner, LfsObjectStore::new(store, "repo"), oid, path)
}

#[tokio::test]
async fn range_stream_rejects_replacement_between_verification_and_serving() {
    let (inner, lfs, oid, path) = fixture(Some(false), Validator::Etag);
    inner
        .put(&path, Bytes::from_static(b"hello").into())
        .await
        .unwrap();
    let result = lfs.get_stream(&oid, 5, Some(1..4)).await;
    assert!(matches!(
        result,
        Err(LfsError::Storage {
            source: crab_storage::StorageError::StateConflict { .. }
        })
    ));
}

#[tokio::test]
async fn upload_cannot_certify_a_later_head_as_verified_content() {
    for streamed in [false, true] {
        let (_, lfs, oid, _) = fixture(Some(true), Validator::Etag);
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(b"hello").unwrap();
        file.flush().unwrap();
        if streamed {
            lfs.put_stream_with_size(&oid, Some(5), file.path())
                .await
                .unwrap();
        } else {
            lfs.put(&oid, Bytes::from_static(b"hello")).await.unwrap();
        }
        assert!(
            matches!(
                lfs.verify_size(&oid, 5).await,
                Err(LfsError::ObjectCorrupt { .. })
            ),
            "streamed={streamed}: replacement must be hashed, not accepted through a receipt"
        );
    }
}

#[tokio::test]
async fn full_stream_without_a_validator_hashes_delivered_bytes() {
    let (inner, lfs, oid, path) = fixture(None, Validator::Missing);
    inner
        .put(&path, Bytes::from_static(b"hello").into())
        .await
        .unwrap();
    let (_, _, stream) = lfs.get_stream(&oid, 5, None).await.unwrap();
    let bytes = stream
        .try_fold(Vec::new(), |mut bytes, chunk| async move {
            bytes.extend_from_slice(&chunk);
            Ok(bytes)
        })
        .await
        .unwrap();
    assert_eq!(bytes, b"hello");
}

#[tokio::test]
async fn verified_streams_support_etags_versions_and_primary_fallback() {
    for validator in [Validator::Etag, Validator::Version] {
        let (inner, lfs, oid, path) = fixture(None, validator);
        inner
            .put(&path, Bytes::from_static(b"hello").into())
            .await
            .unwrap();
        lfs.verify_size(&oid, 5).await.unwrap();
        lfs.verify_size(&oid, 5).await.unwrap();
        let (_, range, stream) = lfs.get_stream(&oid, 5, Some(1..4)).await.unwrap();
        let bytes = stream
            .try_fold(Vec::new(), |mut bytes, chunk| async move {
                bytes.extend_from_slice(&chunk);
                Ok(bytes)
            })
            .await
            .unwrap();
        assert_eq!(
            (range, bytes.as_slice()),
            (1..4, &b"ell"[..]),
            "{validator:?}"
        );
    }
    let (inner, replica, oid, path) = fixture(Some(false), Validator::Version);
    inner
        .put(&path, Bytes::from_static(b"hello").into())
        .await
        .unwrap();
    let primary = Store::new(Arc::new(InMemory::new()));
    primary
        .put(&path, Bytes::from_static(b"hello"))
        .await
        .unwrap();
    let lfs =
        LfsObjectStore::new_with_primary_fallback(replica.store().clone(), "repo", primary, "repo");
    let (_, _, stream) = lfs.get_stream(&oid, 5, None).await.unwrap();
    assert_eq!(
        stream
            .try_fold(Vec::new(), |mut bytes, chunk| async move {
                bytes.extend_from_slice(&chunk);
                Ok(bytes)
            })
            .await
            .unwrap()
            .as_slice(),
        b"hello"
    );
}

#[tokio::test]
async fn weak_etag_supports_verified_full_streaming() {
    let (inner, lfs, oid, path) = fixture(None, Validator::Weak);
    inner
        .put(&path, Bytes::from_static(b"hello").into())
        .await
        .unwrap();
    let (_, _, stream) = lfs.get_stream(&oid, 5, None).await.unwrap();
    let bytes = stream
        .try_fold(Vec::new(), |mut bytes, chunk| async move {
            bytes.extend_from_slice(&chunk);
            Ok(bytes)
        })
        .await
        .unwrap();
    assert_eq!(bytes, b"hello");
    let dir = tempfile::tempdir().unwrap();
    let destination = dir.path().join("download");
    lfs.download_to_file(&oid, 5, &destination).await.unwrap();
    assert_eq!(std::fs::read(destination).unwrap(), b"hello");
}

#[tokio::test]
async fn local_object_store_supports_verified_streaming() {
    let dir = tempfile::tempdir().unwrap();
    let backend = object_store::local::LocalFileSystem::new_with_prefix(dir.path()).unwrap();
    let lfs = LfsObjectStore::new(Store::new(Arc::new(backend)), "repo");
    let oid = Sha256::digest(b"hello").into();
    lfs.put(&oid, Bytes::from_static(b"hello")).await.unwrap();
    let (_, _, stream) = lfs.get_stream(&oid, 5, None).await.unwrap();
    let bytes = stream
        .try_fold(Vec::new(), |mut bytes, chunk| async move {
            bytes.extend_from_slice(&chunk);
            Ok(bytes)
        })
        .await
        .unwrap();
    assert_eq!(bytes, b"hello");
}
