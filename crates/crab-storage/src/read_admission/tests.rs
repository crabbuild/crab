use super::*;
use crate::test_support::CountingObjectStore;
use crate::{RetryClass, RetryPolicy, StorageError, Store, map_object_store_error, retry_class};
use futures_util::TryStreamExt;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

#[derive(Debug, thiserror::Error)]
#[error("test admission exhausted")]
struct Exhausted;

struct Budget {
    requests: AtomicU64,
    bytes: AtomicU64,
    cancellation: tokio_util::sync::CancellationToken,
}

impl Budget {
    fn new(requests: u64, bytes: u64) -> Arc<Self> {
        Arc::new(Self {
            requests: AtomicU64::new(requests),
            bytes: AtomicU64::new(bytes),
            cancellation: tokio_util::sync::CancellationToken::new(),
        })
    }
}

fn charge(
    counter: &AtomicU64,
    amount: u64,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    counter
        .try_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
            left.checked_sub(amount)
        })
        .map(|_| ())
        .map_err(|_| Box::new(Exhausted) as _)
}

#[async_trait::async_trait]
impl ReadAdmission for Budget {
    fn cancellation(&self) -> &tokio_util::sync::CancellationToken {
        &self.cancellation
    }

    async fn request(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        charge(&self.requests, 1)
    }
    async fn bytes(&self, amount: u64) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        charge(&self.bytes, amount)
    }
}

fn exhausted(error: StorageError) {
    assert_eq!(retry_class(&error), RetryClass::Fatal);
    match error {
        StorageError::ReadRejected { source } => {
            assert!(source.downcast_ref::<Exhausted>().is_some())
        }
        other => panic!("unexpected failure: {other:?}"),
    }
}

#[tokio::test]
async fn provider_body_retries_retain_request_and_byte_admission() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    for (truncate, requests, bytes, succeeds) in [
        (false, 10, 1, false),
        (true, 1, 4, false),
        (true, 2, 4, true),
    ] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let calls = Arc::new(AtomicUsize::new(0));
        let observed = calls.clone();
        let server = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                while !request.ends_with(b"\r\n\r\n") {
                    assert!(request.len() < 16 * 1024);
                    request.push(socket.read_u8().await.unwrap());
                }
                let attempt = observed.fetch_add(1, Ordering::SeqCst);
                let (status, range, body) = if attempt > 0 && truncate {
                    assert!(String::from_utf8(request).unwrap().contains("bytes=2-3"));
                    (
                        "206 Partial Content",
                        "Content-Range: bytes 2-3/4\r\n",
                        "cd",
                    )
                } else {
                    ("200 OK", "", "abcd")
                };
                let headers = format!(
                    "HTTP/1.1 {status}\r\n{range}Content-Length: {}\r\nETag: \"pinned\"\r\nLast-Modified: Thu, 01 Jan 1970 00:00:00 GMT\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                socket.write_all(headers.as_bytes()).await.unwrap();
                let body = if truncate && attempt == 0 {
                    &body[..2]
                } else {
                    body
                };
                let _ = socket.write_all(body.as_bytes()).await;
            }
        });
        let budget = Budget::new(requests, bytes);
        let store = crate::build_explicit_store(
            "bucket",
            crate::ObjectStoreCredentials::Aws {
                access_key_id: "test".into(),
                secret_access_key: "test".into(),
                session_token: None,
                region: "us-east-1".into(),
            },
            Some(&endpoint),
            true,
        )
        .unwrap()
        .with_read_admission(budget.clone());
        let result = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let (_, _, stream) = store.get_stream(&Path::from("object"), None).await?;
            stream.try_collect::<Vec<_>>().await
        })
        .await;
        server.abort();
        let _ = server.await;
        let result = result.unwrap();
        if succeeds {
            assert_eq!(result.unwrap().concat(), b"abcd");
        } else {
            let error = result.unwrap_err();
            assert_eq!(retry_class(&error), RetryClass::Fatal);
            let mut cause: &(dyn std::error::Error + 'static) = &error;
            while !cause.is::<Exhausted>() {
                cause = cause.source().expect("typed admission rejection retained");
            }
        }
        assert_eq!(calls.load(Ordering::SeqCst), if succeeds { 2 } else { 1 });
    }
}

#[tokio::test]
async fn listing_shapes_cannot_bypass_request_admission() {
    let inner = Arc::new(object_store::memory::InMemory::new());
    let store = Store::new(inner).with_read_admission(Budget::new(0, 0));
    for result in [
        store
            .inner()
            .list(None)
            .try_collect::<Vec<_>>()
            .await
            .map(|_| ()),
        store
            .inner()
            .list_with_offset(None, &Path::from("offset"))
            .try_collect::<Vec<_>>()
            .await
            .map(|_| ()),
        store.inner().list_with_delimiter(None).await.map(|_| ()),
    ] {
        exhausted(map_object_store_error(result.unwrap_err(), "listing"));
    }
}

#[tokio::test]
async fn listing_admission_is_lazy_and_charges_once_per_invocation() {
    let inner = Arc::new(object_store::memory::InMemory::new());
    for path in ["first", "second"] {
        inner
            .put(&Path::from(path), Bytes::from_static(b"body").into())
            .await
            .unwrap();
    }
    let budget = Budget::new(1, 0);
    let store = Store::new(inner).with_read_admission(budget.clone());
    let listing = store.inner().list(None);
    assert_eq!(budget.requests.load(Ordering::SeqCst), 1);
    assert_eq!(listing.try_collect::<Vec<_>>().await.unwrap().len(), 2);
    assert_eq!(budget.requests.load(Ordering::SeqCst), 0);
    assert_eq!(budget.bytes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn admission_stops_retries_before_another_backend_request() {
    let inner = Arc::new(CountingObjectStore::new(Arc::new(
        object_store::memory::InMemory::new(),
    )));
    inner.set_body_reads_enabled(false);
    let store = Store::new(inner.clone())
        .with_read_admission(Budget::new(2, 100))
        .with_retry_policy(RetryPolicy {
            max_attempts: 5,
            base: std::time::Duration::ZERO,
            cap: std::time::Duration::ZERO,
        });
    exhausted(
        store
            .get_with_etag(&Path::from("object"))
            .await
            .unwrap_err(),
    );
    assert_eq!(inner.counts().body_requests(), 2);
}

#[tokio::test]
async fn raw_and_routed_reads_share_admission_without_charging_heads_as_bodies() {
    let inner = Arc::new(object_store::memory::InMemory::new());
    inner
        .put(&Path::from("object"), Bytes::from_static(b"abcd").into())
        .await
        .unwrap();
    inner
        .put(
            &Path::from("routed/object"),
            Bytes::from_static(b"abcd").into(),
        )
        .await
        .unwrap();
    let budget = Budget::new(3, 8);
    let store = Store::new(inner.clone())
        .with_read_routes(vec![("routed".into(), inner)])
        .with_read_admission(budget.clone());
    store.head(&Path::from("object")).await.unwrap();
    assert_eq!(
        store
            .inner()
            .get(&Path::from("object"))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap(),
        b"abcd"[..]
    );
    assert_eq!(
        store
            .range_get(&Path::from("routed/object"), 1..3)
            .await
            .unwrap(),
        b"bc"[..]
    );
    exhausted(
        store
            .get_with_etag(&Path::from("object"))
            .await
            .unwrap_err(),
    );
    assert_eq!(budget.bytes.load(Ordering::SeqCst), 2);
}

#[derive(Debug)]
struct BodyStore {
    inner: object_store::memory::InMemory,
    body: Bytes,
    polls: Arc<AtomicUsize>,
    pending: Option<(bool, Arc<tokio::sync::Notify>, Arc<AtomicUsize>)>,
    transport_rejection: bool,
}
impl fmt::Display for BodyStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("BodyStore")
    }
}
#[async_trait::async_trait]
impl ObjectStore for BodyStore {
    async fn get_opts(&self, path: &Path, options: GetOptions) -> object_store::Result<GetResult> {
        if let Some((true, entered, dropped)) = &self.pending {
            let _guard = ReadDrop(dropped.clone());
            entered.notify_one();
            futures_util::future::pending::<()>().await;
        }
        let mut result = self.inner.get_opts(path, options).await?;
        let polls = self.polls.clone();
        let body = self.body.clone();
        let pending = self.pending.clone();
        let transport_rejection = self.transport_rejection;
        if transport_rejection {
            result
                .extensions
                .insert(crate::transport_read_admission::TransportReadAccounted);
        }
        result.payload = GetResultPayload::Stream(
            futures_util::stream::once(async move {
                polls.fetch_add(1, Ordering::SeqCst);
                if let Some((false, entered, dropped)) = pending {
                    let _guard = ReadDrop(dropped);
                    entered.notify_one();
                    futures_util::future::pending::<()>().await;
                }
                if transport_rejection {
                    return Err(object_store::Error::Generic {
                        store: "provider stream",
                        source: Box::new(StorageError::ReadRejected {
                            source: Box::new(Exhausted),
                        }),
                    });
                }
                Ok(body)
            })
            .boxed(),
        );
        Ok(result)
    }
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
    fn delete_stream(
        &self,
        paths: BoxStream<'static, object_store::Result<Path>>,
    ) -> BoxStream<'static, object_store::Result<Path>> {
        self.inner.delete_stream(paths)
    }
    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        if let Some((_, entered, dropped)) = self.pending.clone() {
            return futures_util::stream::once(async move {
                let _guard = ReadDrop(dropped);
                entered.notify_one();
                futures_util::future::pending().await
            })
            .boxed();
        }
        self.inner.list(prefix)
    }
    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> object_store::Result<ListResult> {
        if let Some((_, entered, dropped)) = &self.pending {
            let _guard = ReadDrop(dropped.clone());
            entered.notify_one();
            futures_util::future::pending::<()>().await;
        }
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

#[tokio::test]
async fn cancellation_drops_pending_listing_work_for_every_shape() {
    for shape in 0..3 {
        let entered = Arc::new(tokio::sync::Notify::new());
        let dropped = Arc::new(AtomicUsize::new(0));
        let inner = BodyStore {
            inner: object_store::memory::InMemory::new(),
            body: Bytes::new(),
            polls: Arc::new(AtomicUsize::new(0)),
            pending: Some((false, entered.clone(), dropped.clone())),
            transport_rejection: false,
        };
        let budget = Budget::new(10, 0);
        let store = Store::new(Arc::new(inner)).with_read_admission(budget.clone());
        let task = tokio::spawn(async move {
            match shape {
                0 => store
                    .inner()
                    .list(None)
                    .try_collect::<Vec<_>>()
                    .await
                    .map(|_| ()),
                1 => store
                    .inner()
                    .list_with_offset(None, &Path::from("offset"))
                    .try_collect::<Vec<_>>()
                    .await
                    .map(|_| ()),
                _ => store.inner().list_with_delimiter(None).await.map(|_| ()),
            }
        });
        tokio::time::timeout(std::time::Duration::from_secs(1), entered.notified())
            .await
            .unwrap();
        budget.cancellation.cancel();
        let error = tokio::time::timeout(std::time::Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap_err();
        let error = map_object_store_error(error, "listing");
        assert!(matches!(error, StorageError::ReadRejected { source }
            if matches!(source.downcast_ref::<StorageError>(), Some(StorageError::Cancelled))));
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert_eq!(budget.requests.load(Ordering::SeqCst), 9);
    }
}

async fn body_store(body: &'static [u8], budget: Arc<Budget>) -> (Store, Arc<AtomicUsize>) {
    let polls = Arc::new(AtomicUsize::new(0));
    let inner = BodyStore {
        inner: object_store::memory::InMemory::new(),
        body: Bytes::from_static(body),
        polls: polls.clone(),
        pending: None,
        transport_rejection: false,
    };
    inner
        .put(&Path::from("object"), Bytes::from_static(b"abcd").into())
        .await
        .unwrap();
    (
        Store::new(Arc::new(inner)).with_read_admission(budget),
        polls,
    )
}

#[tokio::test]
async fn byte_rejection_never_polls_the_response_payload() {
    let (store, polls) = body_store(b"abcd", Budget::new(10, 3)).await;
    exhausted(
        store
            .get_with_etag(&Path::from("object"))
            .await
            .unwrap_err(),
    );
    assert_eq!(polls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn transport_body_admission_rejection_is_terminal() {
    let inner = BodyStore {
        inner: object_store::memory::InMemory::new(),
        body: Bytes::new(),
        polls: Arc::new(AtomicUsize::new(0)),
        pending: None,
        transport_rejection: true,
    };
    inner
        .put(&Path::from("object"), Bytes::from_static(b"body").into())
        .await
        .unwrap();
    let store = Store::new(Arc::new(inner)).with_read_admission(Budget::new(1, 0));

    let error = store
        .inner()
        .get(&Path::from("object"))
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap_err();

    assert!(matches!(error, object_store::Error::NotSupported { .. }));
    assert!(crate::read_rejection(&error).is_some());
}

#[tokio::test]
async fn response_framing_rejects_short_and_oversized_payloads() {
    for body in [b"abc".as_slice(), b"abcde".as_slice()] {
        let (store, _) = body_store(body, Budget::new(10, 100)).await;
        let error = store
            .inner()
            .get(&Path::from("object"))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap_err();
        // SlateDB buffers ranges inside its retry loop and retries Generic
        // forever. Framing failures must cross that boundary as terminal.
        assert!(matches!(error, object_store::Error::NotSupported { .. }));
        let error = map_object_store_error(error, "object");
        assert!(matches!(error, StorageError::CorruptObject { .. }));
        assert_eq!(retry_class(&error), RetryClass::FatalAfterOneRetry);
    }
}

#[tokio::test]
async fn framing_corruption_retries_once_and_charges_both_attempts() {
    let budget = Budget::new(10, 100);
    let (store, _) = body_store(b"abc", budget.clone()).await;
    let error = store
        .get_with_etag(&Path::from("object"))
        .await
        .unwrap_err();
    assert!(matches!(error, StorageError::CorruptObject { .. }));
    assert_eq!(budget.requests.load(Ordering::SeqCst), 8);
    assert_eq!(budget.bytes.load(Ordering::SeqCst), 92);
}

#[test]
fn nested_io_admission_errors_stay_fatal_and_preserve_the_source() {
    let error = object_store::Error::Generic {
        store: "cache",
        source: Box::new(std::io::Error::other(StorageError::ReadRejected {
            source: Box::new(Exhausted),
        })),
    };
    let error = map_object_store_error(error, "object");
    assert_eq!(retry_class(&error), RetryClass::Fatal);
    assert!(crate::read_rejection(&error).is_some());
}

#[test]
fn admission_failure_cannot_be_reclassified_by_auth_message_heuristics() {
    let error = rejected("expired token budget".into());
    assert!(crate::classify_auth_error(&error).is_none());
    assert_eq!(
        retry_class(&map_object_store_error(error, "object")),
        RetryClass::Fatal
    );
}

#[tokio::test]
async fn coalesced_ranges_charge_the_complete_transport_span_once() {
    let inner = Arc::new(object_store::memory::InMemory::new());
    let path = Path::from("object");
    inner
        .put(&path, Bytes::from_static(b"abcd").into())
        .await
        .unwrap();
    let budget = Budget::new(1, 4);
    let store = Store::new(inner).with_read_admission(budget.clone());
    let ranges = store
        .inner()
        .get_ranges(&path, &[0..1, 3..4])
        .await
        .unwrap();
    assert_eq!(
        ranges,
        vec![Bytes::from_static(b"a"), Bytes::from_static(b"d")]
    );
    assert_eq!(budget.requests.load(Ordering::SeqCst), 0);
    assert_eq!(budget.bytes.load(Ordering::SeqCst), 0);
}

struct ReadDrop(Arc<AtomicUsize>);
impl Drop for ReadDrop {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn cancellation_releases_pending_headers_and_body_without_retry() {
    for headers in [true, false] {
        let entered = Arc::new(tokio::sync::Notify::new());
        let dropped = Arc::new(AtomicUsize::new(0));
        let inner = BodyStore {
            inner: object_store::memory::InMemory::new(),
            body: Bytes::from_static(b"abcd"),
            polls: Arc::new(AtomicUsize::new(0)),
            pending: Some((headers, entered.clone(), dropped.clone())),
            transport_rejection: false,
        };
        let path = Path::from("object");
        inner
            .put(&path, Bytes::from_static(b"abcd").into())
            .await
            .unwrap();
        let budget = Budget::new(10, 100);
        let store = Store::new(Arc::new(inner)).with_read_admission(budget.clone());
        let task = tokio::spawn(async move { store.get_with_etag(&path).await });
        tokio::time::timeout(std::time::Duration::from_secs(1), entered.notified())
            .await
            .unwrap();
        budget.cancellation.cancel();
        let error = tokio::time::timeout(std::time::Duration::from_secs(1), task)
            .await
            .expect("pending transport must stop on cancellation")
            .unwrap()
            .unwrap_err();
        assert_eq!(retry_class(&error), RetryClass::Fatal);
        assert!(
            matches!(error, StorageError::ReadRejected { source } if matches!(source.downcast_ref::<StorageError>(), Some(StorageError::Cancelled)))
        );
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert_eq!(budget.requests.load(Ordering::SeqCst), 9);
    }
}
