use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use futures_util::{TryStreamExt, stream::BoxStream};
use object_store::{
    CopyOptions, GetOptions, GetResult, GetResultPayload, ListResult, MultipartUpload, ObjectStore,
    PutMultipartOptions, PutOptions, PutResult, memory::InMemory,
};

use super::*;

#[derive(Debug, Clone, Copy)]
enum Fault {
    None,
    ChangedVersion,
    CorruptBody,
    ShortBody,
    LongBody,
    WrongRange,
    NoValidator,
    EmptyValidator,
    WeakValidator,
}

#[derive(Debug)]
struct DeliveryStore {
    inner: InMemory,
    object_path: Path,
    fault: Fault,
    bodies: AtomicUsize,
    delivery_read: AtomicUsize,
    reads: AtomicUsize,
    writes: AtomicUsize,
}

impl std::fmt::Display for DeliveryStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("delivery fault fixture")
    }
}

#[async_trait]
impl ObjectStore for DeliveryStore {
    async fn put_opts(
        &self,
        path: &Path,
        payload: PutPayload,
        options: PutOptions,
    ) -> object_store::Result<PutResult> {
        self.writes.fetch_add(1, Ordering::Relaxed);
        self.inner.put_opts(path, payload, options).await
    }

    async fn put_multipart_opts(
        &self,
        path: &Path,
        options: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        self.writes.fetch_add(1, Ordering::Relaxed);
        self.inner.put_multipart_opts(path, options).await
    }

    async fn get_opts(&self, path: &Path, options: GetOptions) -> object_store::Result<GetResult> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        let body = !options.head && path == &self.object_path;
        let delivery = body
            && self.bodies.fetch_add(1, Ordering::Relaxed) + 1
                == self.delivery_read.load(Ordering::Relaxed);
        let mut result = self.inner.get_opts(path, options).await?;
        if path == &self.object_path {
            match self.fault {
                Fault::NoValidator => {
                    result.meta.e_tag = None;
                    result.meta.version = None;
                }
                Fault::WeakValidator => {
                    result.meta.e_tag = Some("W/weak".to_owned());
                    result.meta.version = None;
                }
                Fault::EmptyValidator => {
                    result.meta.e_tag = Some(String::new());
                    result.meta.version = Some(String::new());
                }
                _ => {}
            }
        }
        if delivery {
            let replacement = match self.fault {
                Fault::ChangedVersion => {
                    result.meta.e_tag = Some("changed".to_owned());
                    None
                }
                Fault::WrongRange => {
                    result.range.start += 1;
                    None
                }
                Fault::CorruptBody => Some(b"tampered".as_slice()),
                Fault::ShortBody => Some(b"verifie".as_slice()),
                Fault::LongBody => Some(b"verified!".as_slice()),
                _ => None,
            };
            if let Some(bytes) = replacement {
                result.payload = GetResultPayload::Stream(
                    futures_util::stream::iter([Ok(Bytes::from_static(bytes))]).boxed(),
                );
            }
        }
        Ok(result)
    }

    fn delete_stream(
        &self,
        paths: BoxStream<'static, object_store::Result<Path>>,
    ) -> BoxStream<'static, object_store::Result<Path>> {
        self.writes.fetch_add(1, Ordering::Relaxed);
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
        options: CopyOptions,
    ) -> object_store::Result<()> {
        self.writes.fetch_add(1, Ordering::Relaxed);
        self.inner.copy_opts(from, to, options).await
    }
}

async fn fixture(fault: Fault) -> (LfsObjectStore, Arc<DeliveryStore>, [u8; 32]) {
    let oid = Sha256::digest(b"verified").into();
    let store = Arc::new(DeliveryStore {
        inner: InMemory::new(),
        object_path: LfsObjectStore::object_path_for_prefix("stream", &oid),
        fault,
        bodies: AtomicUsize::new(0),
        delivery_read: AtomicUsize::new(1),
        reads: AtomicUsize::new(0),
        writes: AtomicUsize::new(0),
    });
    store
        .inner
        .put(&store.object_path, Bytes::from_static(b"verified").into())
        .await
        .unwrap();
    (
        LfsObjectStore::new(Store::new(store.clone()), "stream"),
        store,
        oid,
    )
}

async fn collect(
    lfs: &LfsObjectStore,
    oid: &[u8; 32],
    range: Option<Range<u64>>,
) -> Result<Vec<u8>> {
    let (_, _, stream) = lfs.get_stream(oid, 8, range).await?;
    let chunks: Vec<Bytes> = stream.try_collect().await?;
    Ok(chunks.concat())
}

#[tokio::test]
async fn full_and_range_streams_never_write_receipts() {
    for range in [None, Some(1..4), Some(8..8)] {
        let (lfs, store, oid) = fixture(Fault::None).await;
        let expected = range.clone().unwrap_or(0..8);
        let result = collect(&lfs, &oid, range).await.unwrap();
        assert_eq!(
            (result, store.writes.load(Ordering::Relaxed)),
            (
                b"verified"[expected.start as usize..expected.end as usize].to_vec(),
                0
            )
        );
    }
}

#[tokio::test]
async fn streamed_upload_receipt_avoids_full_reverification_for_ranges() {
    let oid = Sha256::digest(b"verified").into();
    let store = Arc::new(DeliveryStore {
        inner: InMemory::new(),
        object_path: LfsObjectStore::object_path_for_prefix("stream", &oid),
        fault: Fault::None,
        bodies: AtomicUsize::new(0),
        delivery_read: AtomicUsize::new(1),
        reads: AtomicUsize::new(0),
        writes: AtomicUsize::new(0),
    });
    let lfs = LfsObjectStore::new(Store::new(store.clone()), "stream");
    let file = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(file.path(), b"verified").unwrap();

    lfs.put_stream_with_size(&oid, Some(8), file.path())
        .await
        .unwrap();
    let result = collect(&lfs, &oid, Some(1..4)).await.unwrap();

    assert_eq!(
        (result, store.bodies.load(Ordering::Relaxed)),
        (b"eri".to_vec(), 1)
    );
}

#[tokio::test]
async fn changed_range_delivery_version_is_a_conflict_before_bytes_are_exposed() {
    for range in [Some(1..4), Some(0..7)] {
        let (lfs, store, oid) = fixture(Fault::ChangedVersion).await;
        store.delivery_read.store(2, Ordering::Relaxed);
        assert!(matches!(
            collect(&lfs, &oid, range).await,
            Err(LfsError::Storage {
                source: StorageError::StateConflict { .. }
            })
        ));
    }
}

#[tokio::test]
async fn full_streams_verify_in_one_body_request() {
    let (lfs, store, oid) = fixture(Fault::None).await;
    let result = collect(&lfs, &oid, None).await.unwrap();
    assert_eq!(
        (result, store.bodies.load(Ordering::Relaxed)),
        (b"verified".to_vec(), 1)
    );
}

#[tokio::test]
async fn invalid_terminal_bytes_are_withheld_from_content_length_consumers() {
    for fault in [Fault::CorruptBody, Fault::LongBody, Fault::ShortBody] {
        let (lfs, _, oid) = fixture(fault).await;
        let (_, _, mut stream) = lfs.get_stream(&oid, 8, None).await.unwrap();
        let mut delivered = 0;
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(bytes) => delivered += bytes.len(),
                Err(_) => break,
            }
        }
        assert!(
            delivered < 8,
            "{fault:?}: corrupt transfer filled Content-Length"
        );
    }
}

#[tokio::test]
async fn delivered_corruption_and_incomplete_framing_never_reach_successful_eof() {
    for fault in [
        Fault::CorruptBody,
        Fault::ShortBody,
        Fault::LongBody,
        Fault::WrongRange,
    ] {
        let (lfs, store, oid) = fixture(fault).await;
        let error = collect(&lfs, &oid, None).await.unwrap_err();
        // Store validates response framing before LFS hashes delivered bytes.
        // Preserve that original corruption source instead of erasing its path.
        match fault {
            Fault::CorruptBody => assert!(
                matches!(error, LfsError::ObjectCorrupt { oid: actual } if actual == hex_encode(&oid)),
                "{fault:?}"
            ),
            _ => assert!(
                matches!(error, LfsError::Storage { source: StorageError::CorruptObject { path, .. } }
                    if path == store.object_path.to_string()),
                "{fault:?}"
            ),
        }
    }
}

#[tokio::test]
async fn range_streams_require_strong_version_proof_but_full_hashing_does_not() {
    for fault in [
        Fault::NoValidator,
        Fault::EmptyValidator,
        Fault::WeakValidator,
    ] {
        let (lfs, _, oid) = fixture(fault).await;
        let error = collect(&lfs, &oid, Some(1..4)).await.unwrap_err();
        assert!(
            matches!(error, LfsError::Io { source } if source.kind() == std::io::ErrorKind::Unsupported)
        );
        let (lfs, _, oid) = fixture(fault).await;
        assert_eq!(collect(&lfs, &oid, None).await.unwrap(), b"verified");
    }
}

#[tokio::test]
async fn invalid_ranges_fail_before_storage_reads() {
    for range in [Range { start: 4, end: 3 }, 0..9, 9..9] {
        let (lfs, store, oid) = fixture(Fault::None).await;
        let error = collect(&lfs, &oid, Some(range)).await.unwrap_err();
        assert_eq!(
            (
                matches!(error, LfsError::Io { source } if source.kind() == std::io::ErrorKind::InvalidInput),
                store.reads.load(Ordering::Relaxed)
            ),
            (true, 0)
        );
    }
}

#[tokio::test]
async fn read_session_tracks_delivery_and_range_preverification_jobs() {
    let store = Store::new(Arc::new(InMemory::new()));
    let lfs = LfsObjectStore::new(store.clone(), "tracked");
    let oid: [u8; 32] = Sha256::digest(b"abcdef").into();
    store
        .put(&lfs.object_path_for(&oid), Bytes::from_static(b"abcdef"))
        .await
        .unwrap();
    let session = LfsReadSession::default();
    let (_, _, mut stream) = lfs
        .get_stream_with_session(&oid, 6, None, &session)
        .await
        .unwrap();
    session.close().await;
    assert!(
        matches!(stream.next().await, Some(Err(LfsError::Io { source })) if source.kind() == std::io::ErrorKind::BrokenPipe)
    );
    drop(stream);
    let partial = lfs
        .get_stream_with_session(&oid, 6, Some(1..3), &session)
        .await;
    assert!(
        matches!(partial, Err(LfsError::Io { source }) if source.kind() == std::io::ErrorKind::BrokenPipe)
    );
}
