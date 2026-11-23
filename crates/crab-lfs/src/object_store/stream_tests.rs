use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use async_trait::async_trait;
use futures_util::{TryStreamExt, stream::BoxStream};
use object_store::ObjectStoreExt as _;
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
    StalledBody,
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
            if matches!(self.fault, Fault::StalledBody) {
                let partial = futures_util::stream::iter([Ok(Bytes::from_static(b"veri"))]);
                result.payload = GetResultPayload::Stream(
                    partial.chain(futures_util::stream::pending()).boxed(),
                );
            }
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

async fn edge_fixture(
    origin: Arc<DeliveryStore>,
    responses: Vec<(Option<Range<u64>>, String, Bytes)>,
    limits: Option<&str>,
) -> (LfsObjectStore, tokio::task::JoinHandle<()>) {
    use crab_storage::cdn::{CdnAccess, CdnReadFamily, CdnRoute, CdnTransport};
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, BufReader};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let transport = CdnTransport::loopback_fixture(listener.local_addr().unwrap()).unwrap();
    let target = [4; 32];
    let mut route = CdnRoute::new(
        target,
        "stream".into(),
        None,
        CdnAccess::Public,
        transport,
        Duration::from_secs(10),
    );
    if let Some(columns) = limits {
        route = route
            .with_loopback_read_limits(CdnReadFamily::Lfs, columns)
            .unwrap();
    }
    let store = Store::new(origin)
        .with_target_identity(target)
        .with_cdn_route(route)
        .unwrap();
    let server = tokio::spawn(async move {
        for (range, headers, body) in responses {
            let (stream, _) = listener.accept().await.unwrap();
            let mut stream = BufReader::new(stream);
            let mut request = String::new();
            while !request.ends_with("\r\n\r\n") {
                assert_ne!(stream.read_line(&mut request).await.unwrap(), 0);
            }
            let request = request.to_ascii_lowercase();
            let headers_sent: Vec<_> = request
                .lines()
                .filter_map(|line| line.strip_prefix("range: "))
                .collect();
            let expected: Vec<_> = range
                .into_iter()
                .map(|range| format!("bytes={}-{}", range.start, range.end - 1))
                .collect();
            assert_eq!(headers_sent, expected);
            stream.write_all(headers.as_bytes()).await.unwrap();
            stream.write_all(&body).await.unwrap();
        }
    });
    (LfsObjectStore::new(store, "stream"), server)
}

fn edge_partial(range: Range<u64>, size: u64, bytes: Bytes) -> (Option<Range<u64>>, String, Bytes) {
    let end = range.end - 1;
    let start = range.start;
    let len = range.end - start;
    (
        Some(range),
        format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{end}/{size}\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n"
        ),
        bytes,
    )
}

#[tokio::test]
async fn cdn_stream_validates_edge_bytes_without_proving_the_native_version() {
    let (_, origin, oid) = fixture(Fault::CorruptBody).await;
    let response = edge_partial(0..8, 8, Bytes::from_static(b"verified"));
    let (lfs, edge) = edge_fixture(origin.clone(), vec![response], None).await;
    let session = LfsReadSession::new(LfsReadCache::default());
    let (range, stream) = lfs
        .stream_content(&oid, 8, None, Some(&session))
        .await
        .unwrap();
    assert_eq!(range, 0..8);
    assert_eq!(
        stream.try_collect::<Vec<_>>().await.unwrap().concat(),
        b"verified"
    );
    assert_eq!(origin.reads.load(Ordering::Relaxed), 0);
    edge.await.unwrap();
    let partial = lfs
        .stream_content(&oid, 8, Some(1..4), Some(&session))
        .await;
    assert!(matches!(partial, Err(LfsError::ObjectCorrupt { .. })));
    assert_eq!(origin.bodies.load(Ordering::Relaxed), 1);
    session.close().await;
}

#[tokio::test]
async fn cdn_stream_denial_stops_before_native_or_primary_io() {
    let (_, origin, oid) = fixture(Fault::None).await;
    let (primary, primary_origin, _) = fixture(Fault::None).await;
    let forbidden = (
        Some(0..8),
        "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n".into(),
        Bytes::new(),
    );
    let (lfs, edge) = edge_fixture(origin.clone(), vec![forbidden], None).await;
    let lfs =
        LfsObjectStore::new_with_primary_fallback(lfs.store, "stream", primary.store, "stream");
    let Err(error) = lfs.stream_content(&oid, 8, None, None).await else {
        panic!("edge authorization failure must stop the download");
    };
    assert!(matches!(
        crab_storage::store::cdn_read_rejection(&error),
        Some(StorageError::Forbidden { .. })
    ));
    assert_eq!(origin.reads.load(Ordering::Relaxed), 0);
    assert_eq!(primary_origin.reads.load(Ordering::Relaxed), 0);
    edge.await.unwrap();
}

#[tokio::test]
async fn cdn_stream_origin_restart_keeps_the_native_partial_reader() {
    let (_, origin, oid) = fixture(Fault::None).await;
    let unavailable = (
        Some(0..8),
        "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n".into(),
        Bytes::new(),
    );
    let (lfs, edge) = edge_fixture(origin.clone(), vec![unavailable], None).await;
    let (_, partial) = lfs.stream_content(&oid, 8, Some(1..4), None).await.unwrap();
    assert_eq!(
        partial.try_collect::<Vec<_>>().await.unwrap().concat(),
        b"eri"
    );
    assert!(!edge.is_finished());
    origin.bodies.store(0, Ordering::Relaxed);
    let (_, full) = lfs.stream_content(&oid, 8, Some(0..8), None).await.unwrap();
    assert_eq!(
        full.try_collect::<Vec<_>>().await.unwrap().concat(),
        b"verified"
    );
    assert_eq!(origin.bodies.load(Ordering::Relaxed), 1);
    edge.await.unwrap();
}

#[tokio::test]
async fn cdn_stream_withholds_corrupt_final_range_after_delivering_the_first_range() {
    const RANGE_BUFFER_BYTES: u64 = 8 * 1024 * 1024;
    let prefix = Bytes::from(vec![b'a'; RANGE_BUFFER_BYTES as usize]);
    let mut hash = Sha256::new();
    hash.update(&prefix);
    hash.update(b"z");
    let oid = hash.finalize().into();
    let (_, origin, _) = fixture(Fault::None).await;
    let size = RANGE_BUFFER_BYTES + 1;
    let first = edge_partial(0..RANGE_BUFFER_BYTES, size, prefix.clone());
    let last = edge_partial(RANGE_BUFFER_BYTES..size, size, Bytes::from_static(b"x"));
    let (lfs, edge) = edge_fixture(origin.clone(), vec![first, last], None).await;
    let (_, mut stream) = lfs.stream_content(&oid, size, None, None).await.unwrap();
    assert_eq!(stream.try_next().await.unwrap().unwrap(), prefix);
    assert!(matches!(
        stream.try_next().await,
        Err(LfsError::ObjectCorrupt { .. })
    ));
    assert_eq!(origin.reads.load(Ordering::Relaxed), 0);
    edge.await.unwrap();
}

#[tokio::test]
async fn cdn_stream_splits_all_requests_at_the_qualified_range_size() {
    let (_, origin, oid) = fixture(Fault::None).await;
    let responses = vec![
        edge_partial(0..3, 8, Bytes::from_static(b"ver")),
        edge_partial(3..6, 8, Bytes::from_static(b"ifi")),
        edge_partial(6..8, 8, Bytes::from_static(b"ed")),
    ];
    let (lfs, edge) = edge_fixture(origin.clone(), responses, Some("0 8 3 10000")).await;
    let (range, stream) = lfs.stream_content(&oid, 8, None, None).await.unwrap();
    assert_eq!(range, 0..8);
    assert_eq!(
        stream.try_collect::<Vec<_>>().await.unwrap().concat(),
        b"verified"
    );
    assert_eq!(origin.reads.load(Ordering::Relaxed), 0);
    edge.await.unwrap();
}

#[tokio::test]
async fn cdn_full_only_policy_serves_streams_and_files_with_whole_gets() {
    let (_, origin, oid) = fixture(Fault::None).await;
    let response = (
        None,
        "HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\n".into(),
        Bytes::from_static(b"verified"),
    );
    let (lfs, edge) = edge_fixture(
        origin.clone(),
        vec![response.clone(), response],
        Some("8 0 0 10000"),
    )
    .await;
    let (range, stream) = lfs.stream_content(&oid, 8, None, None).await.unwrap();
    assert_eq!(range, 0..8);
    assert_eq!(
        stream.try_collect::<Vec<_>>().await.unwrap().concat(),
        b"verified"
    );
    assert_eq!(origin.reads.load(Ordering::Relaxed), 0);

    let directory = tempfile::tempdir().unwrap();
    let destination = directory.path().join("object");
    lfs.download_to_file(&oid, 8, &destination).await.unwrap();
    assert_eq!(std::fs::read(destination).unwrap(), b"verified");
    assert_eq!(origin.reads.load(Ordering::Relaxed), 0);
    edge.await.unwrap();
}

#[tokio::test]
async fn cdn_origin_restart_respects_deadline_and_drains_partial_file() {
    use crab_storage::cdn::{CdnAccess, CdnRoute, CdnTransport};
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, BufReader};

    let (_, origin, oid) = fixture(Fault::StalledBody).await;
    let (primary, primary_fixture, _) = fixture(Fault::None).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let transport = CdnTransport::loopback_fixture(listener.local_addr().unwrap()).unwrap();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut stream = BufReader::new(stream);
        let mut request = String::new();
        while !request.ends_with("\r\n\r\n") {
            assert_ne!(stream.read_line(&mut request).await.unwrap(), 0);
        }
        stream
            .write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n")
            .await
            .unwrap();
    });
    let target = [4; 32];
    let selected = Store::new(origin.clone())
        .with_target_identity(target)
        .with_cdn_route(CdnRoute::new(
            target,
            "stream".into(),
            None,
            CdnAccess::Public,
            transport,
            Duration::from_secs(1),
        ))
        .unwrap();
    let lfs =
        LfsObjectStore::new_with_primary_fallback(selected, "stream", primary.store, "stream");
    let directory = tempfile::tempdir().unwrap();
    let destination = directory.path().join("private-download");
    let error = tokio::time::timeout(
        Duration::from_secs(5),
        lfs.download_to_file(&oid, 8, &destination),
    )
    .await
    .expect("selected origin must stop on the CDN attempt deadline")
    .unwrap_err();
    assert!(matches!(
        crab_storage::store::cdn_read_rejection(&error),
        Some(StorageError::Io { source }) if source.kind() == std::io::ErrorKind::TimedOut
    ));
    server.await.unwrap();
    assert_eq!(origin.bodies.load(Ordering::Relaxed), 1);
    assert_eq!(primary_fixture.reads.load(Ordering::Relaxed), 0);
    assert_eq!(std::fs::read(destination).unwrap(), b"veri");
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
async fn streamed_upload_receipt_reuse_requires_a_bound_transport() {
    for bound in [false, true] {
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
        let transport = Store::new(store.clone());
        let transport = if bound {
            transport.with_target_identity([1; 32])
        } else {
            transport
        };
        let lfs = LfsObjectStore::new(transport, "stream");
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), b"verified").unwrap();

        lfs.put_stream_with_size(&oid, Some(8), file.path())
            .await
            .unwrap();
        let result = collect(&lfs, &oid, Some(1..4)).await.unwrap();

        assert_eq!(
            (result, store.bodies.load(Ordering::Relaxed)),
            (b"eri".to_vec(), if bound { 1 } else { 2 })
        );
    }
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

#[tokio::test]
async fn retained_remote_proofs_skip_only_unchanged_bound_object_bodies() {
    let (lfs, raw, oid) = fixture(Fault::None).await;
    let lfs = LfsObjectStore::new(lfs.store.with_target_identity([1; 32]), "stream");
    let cache = crate::LfsReadCache::default();
    for (range, expected_bodies) in [(1..4, 2), (3..5, 3)] {
        let session = LfsReadSession::new(cache.clone());
        let (_, _, stream) = lfs
            .get_stream_with_session(&oid, 8, Some(range.clone()), &session)
            .await
            .unwrap();
        let bytes = stream
            .try_fold(Vec::new(), |mut all, bytes| async move {
                all.extend_from_slice(&bytes);
                Ok(all)
            })
            .await
            .unwrap();
        session.close().await;
        assert_eq!(
            (
                bytes,
                raw.bodies.load(Ordering::Relaxed),
                raw.writes.load(Ordering::Relaxed)
            ),
            (
                b"verified"[range.start as usize..range.end as usize].to_vec(),
                expected_bodies,
                0
            )
        );
    }
    raw.inner
        .put(&raw.object_path, Bytes::from_static(b"corrupt!").into())
        .await
        .unwrap();
    let session = LfsReadSession::new(cache.clone());
    let corrupt = lfs
        .get_stream_with_session(&oid, 8, Some(0..1), &session)
        .await;
    session.close().await;
    assert!(matches!(corrupt, Err(LfsError::ObjectCorrupt { .. })));
    raw.inner
        .put(&raw.object_path, Bytes::from_static(b"verified").into())
        .await
        .unwrap();
    let session = LfsReadSession::new(cache);
    let (_, _, stream) = lfs
        .get_stream_with_session(&oid, 8, Some(0..1), &session)
        .await
        .unwrap();
    let bytes = stream.try_collect::<Vec<_>>().await.unwrap();
    session.close().await;
    assert_eq!(bytes.concat(), b"v");
}

#[tokio::test]
async fn whole_delivery_retains_only_its_verified_response_for_later_ranges() {
    let (lfs, raw, oid) = fixture(Fault::None).await;
    let lfs = LfsObjectStore::new(lfs.store.with_target_identity([1; 32]), "stream");
    let cache = crate::LfsReadCache::default();
    for range in [None, Some(2..4)] {
        let session = LfsReadSession::new(cache.clone());
        let (_, _, stream) = lfs
            .get_stream_with_session(&oid, 8, range, &session)
            .await
            .unwrap();
        let _ = stream.try_collect::<Vec<_>>().await.unwrap();
        session.close().await;
    }
    assert_eq!(
        (
            raw.bodies.load(Ordering::Relaxed),
            raw.writes.load(Ordering::Relaxed)
        ),
        (2, 0)
    );
}

#[tokio::test]
async fn coincident_validators_cannot_reuse_proof_across_store_incarnations() {
    let (first, _, oid) = fixture(Fault::None).await;
    let first = LfsObjectStore::new(first.store.with_target_identity([1; 32]), "stream");
    let cache = crate::LfsReadCache::default();
    let session = LfsReadSession::new(cache.clone());
    let (_, _, stream) = first
        .get_stream_with_session(&oid, 8, Some(0..1), &session)
        .await
        .unwrap();
    let _ = stream.try_collect::<Vec<_>>().await.unwrap();
    session.close().await;
    let raw = Arc::new(InMemory::new());
    raw.put(
        &first.object_path(&oid),
        Bytes::from_static(b"corrupt!").into(),
    )
    .await
    .unwrap();
    let other = LfsObjectStore::new(Store::new(raw).with_target_identity([1; 32]), "stream");
    assert_eq!(
        first
            .store
            .head(&first.object_path(&oid))
            .await
            .unwrap()
            .e_tag,
        other
            .store
            .head(&other.object_path(&oid))
            .await
            .unwrap()
            .e_tag
    );
    let session = LfsReadSession::new(cache);
    let result = other
        .get_stream_with_session(&oid, 8, Some(0..1), &session)
        .await;
    session.close().await;
    assert!(matches!(result, Err(LfsError::ObjectCorrupt { .. })));
}

#[tokio::test]
async fn scoped_read_routes_cannot_reuse_the_canonical_transport_proof() {
    let (first, _, oid) = fixture(Fault::None).await;
    let first = LfsObjectStore::new(first.store.with_target_identity([1; 32]), "stream");
    let cache = crate::LfsReadCache::default();
    let session = LfsReadSession::new(cache.clone());
    let (_, _, stream) = first
        .get_stream_with_session(&oid, 8, Some(0..1), &session)
        .await
        .unwrap();
    let _ = stream.try_collect::<Vec<_>>().await.unwrap();
    session.close().await;
    let raw = Arc::new(InMemory::new());
    raw.put(
        &first.object_path(&oid),
        Bytes::from_static(b"corrupt!").into(),
    )
    .await
    .unwrap();
    let store = first
        .store
        .clone()
        .with_read_routes(vec![("stream/lfs".to_owned(), raw)]);
    assert_eq!(
        first
            .store
            .head(&first.object_path(&oid))
            .await
            .unwrap()
            .e_tag,
        store.head(&first.object_path(&oid)).await.unwrap().e_tag
    );
    let routed = LfsObjectStore::new(store, "stream");
    let session = LfsReadSession::new(cache);
    let result = routed
        .get_stream_with_session(&oid, 8, Some(0..1), &session)
        .await;
    session.close().await;
    assert!(matches!(result, Err(LfsError::ObjectCorrupt { .. })));
}

#[tokio::test]
async fn canonical_receipts_cannot_certify_bytes_from_a_different_read_route() {
    let (canonical, _, oid) = fixture(Fault::None).await;
    let canonical = LfsObjectStore::new(canonical.store.with_target_identity([1; 32]), "stream");
    canonical.verify_size(&oid, 8).await.unwrap();
    let raw = Arc::new(InMemory::new());
    raw.put(
        &canonical.object_path(&oid),
        Bytes::from_static(b"corrupt!").into(),
    )
    .await
    .unwrap();
    let routed_store = canonical
        .store
        .clone()
        .with_read_routes(vec![("stream/lfs/objects".to_owned(), raw)]);
    assert_eq!(
        canonical
            .store
            .head(&canonical.object_path(&oid))
            .await
            .unwrap()
            .e_tag,
        routed_store
            .head(&canonical.object_path(&oid))
            .await
            .unwrap()
            .e_tag
    );
    let routed = LfsObjectStore::new(routed_store, "stream");
    let result = collect(&routed, &oid, Some(0..1)).await;
    assert!(
        matches!(&result, Err(LfsError::ObjectCorrupt { .. })),
        "{result:?}"
    );
}

#[tokio::test]
async fn receipts_cannot_cross_canonical_transport_identities() {
    let (first, _, oid) = fixture(Fault::None).await;
    let first = LfsObjectStore::new(first.store.with_target_identity([1; 32]), "stream");
    first.verify_size(&oid, 8).await.unwrap();
    let receipt_path = receipt_path_at("stream", &oid);
    let (receipt, _) = first
        .store
        .get_with_etag_bounded(&receipt_path, MAX_RECEIPT_SIZE)
        .await
        .unwrap();
    let raw = Arc::new(InMemory::new());
    raw.put(
        &first.object_path(&oid),
        Bytes::from_static(b"corrupt!").into(),
    )
    .await
    .unwrap();
    raw.put(&receipt_path, receipt.into()).await.unwrap();
    let store = Store::new(raw).with_target_identity([2; 32]);
    let expected = first.store.head(&first.object_path(&oid)).await.unwrap();
    let other = store.head(&first.object_path(&oid)).await.unwrap();
    assert_eq!(
        (expected.size, expected.e_tag, expected.version),
        (other.size, other.e_tag, other.version)
    );
    let other = LfsObjectStore::new(store, "stream");
    assert!(matches!(
        collect(&other, &oid, Some(0..1)).await,
        Err(LfsError::ObjectCorrupt { .. })
    ));
}

#[tokio::test]
async fn declared_size_mismatch_rejects_ranges_before_opening_the_payload() {
    let (lfs, raw, oid) = fixture(Fault::None).await;
    let result = lfs.get_stream(&oid, 4, Some(0..1)).await;
    assert!(matches!(result, Err(LfsError::ObjectCorrupt { .. })));
    assert_eq!(raw.bodies.load(Ordering::Relaxed), 0);
}
