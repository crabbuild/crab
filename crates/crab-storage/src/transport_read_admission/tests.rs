use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::{TryStreamExt as _, stream::BoxStream};
use object_store::client::HttpRequestBody;
use object_store::path::Path;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    ObjectStoreExt, PutMultipartOptions, PutOptions, PutPayload, PutResult,
};
use tokio_util::sync::CancellationToken;

use super::*;

#[derive(Default)]
struct CountingAdmission {
    requests: AtomicU64,
    bytes: AtomicU64,
    received: AtomicU64,
    cancellation: CancellationToken,
}

#[derive(Debug, thiserror::Error)]
#[error("request limit exhausted")]
struct Exhausted;

struct RequestLimit {
    remaining: AtomicU64,
    cancellation: CancellationToken,
}

struct ByteRejection {
    requests: AtomicU64,
    received: AtomicU64,
    cancellation: CancellationToken,
}

#[async_trait::async_trait]
impl ReadAdmission for RequestLimit {
    fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    async fn request(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.remaining
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .map(|_| ())
            .map_err(|_| Box::new(Exhausted) as _)
    }

    async fn bytes(&self, _bytes: u64) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl ReadAdmission for ByteRejection {
    fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    async fn request(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.requests.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    async fn bytes(&self, _bytes: u64) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        Err(Box::new(Exhausted))
    }

    async fn received(&self, bytes: u64) {
        self.received.fetch_add(bytes, Ordering::Relaxed);
    }
}

#[async_trait::async_trait]
impl ReadAdmission for CountingAdmission {
    fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    async fn request(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.requests.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    async fn bytes(&self, bytes: u64) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
        Ok(())
    }

    async fn received(&self, bytes: u64) {
        self.received.fetch_add(bytes, Ordering::Relaxed);
    }
}

#[tokio::test]
async fn response_rejection_stops_retry_admission() {
    let admission = Arc::new(ByteRejection {
        requests: AtomicU64::new(0),
        received: AtomicU64::new(0),
        cancellation: CancellationToken::new(),
    });
    let context = TransportReadContext::new(admission.clone());

    let first = context.admit_bytes(4).await.expect_err("body rejection");
    let retry = context
        .admit_request()
        .await
        .expect_err("latched rejection");

    assert!(crate::read_rejection(&object_rejected(first)).is_some());
    assert!(crate::read_rejection(&object_rejected(retry)).is_some());
    assert_eq!(admission.requests.load(Ordering::Relaxed), 0);
}

#[derive(Debug)]
struct StaticService {
    calls: Arc<AtomicU64>,
}

#[async_trait::async_trait]
impl HttpService for StaticService {
    async fn call(&self, _request: HttpRequest) -> Result<HttpResponse, HttpError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Ok(HttpResponse::new(Bytes::from_static(b"body").into()))
    }
}

fn request() -> HttpRequest {
    let mut request = HttpRequest::new(HttpRequestBody::empty());
    *request.uri_mut() = "https://storage.invalid/object".parse().unwrap();
    request
}

fn object_store_error(error: HttpError) -> object_store::Error {
    object_store::Error::Generic {
        store: "transport admission test",
        source: Box::new(error),
    }
}

struct PartialBody {
    first: bool,
    cancel: Option<CancellationToken>,
    fail: bool,
}

impl Body for PartialBody {
    type Data = Bytes;
    type Error = HttpError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, HttpError>>> {
        if !self.first {
            self.first = true;
            if let Some(cancel) = &self.cancel {
                cancel.cancel();
            }
            return Poll::Ready(Some(Ok(Frame::data(Bytes::from_static(b"part")))));
        }
        if self.fail {
            return Poll::Ready(Some(Err(HttpError::new(
                HttpErrorKind::Unknown,
                std::io::Error::other("fixture response failure"),
            ))));
        }
        Poll::Pending
    }
}

#[tokio::test]
async fn origin_receipts_survive_partial_drop_transport_failure_and_cancel_after_read() {
    for outcome in ["drop", "failure", "cancel"] {
        let admission = Arc::new(CountingAdmission::default());
        let inner = HttpResponseBody::new(PartialBody {
            first: false,
            cancel: (outcome == "cancel").then(|| admission.cancellation.clone()),
            fail: outcome == "failure",
        });
        let context = TransportReadContext::new(admission.clone());
        let mut stream =
            HttpResponseBody::new(AdmissionBody::new(inner, vec![context])).bytes_stream();
        if outcome == "cancel" {
            assert!(stream.next().await.unwrap().is_err());
        } else {
            assert_eq!(stream.next().await.unwrap().unwrap(), b"part"[..]);
            if outcome == "failure" {
                assert!(stream.next().await.unwrap().is_err());
            }
        }
        drop(stream);
        assert_eq!(admission.received.load(Ordering::Relaxed), 4, "{outcome}");
    }
}

#[tokio::test]
async fn rejected_origin_bytes_are_observed_before_budget_admission() {
    let admission = Arc::new(ByteRejection {
        requests: AtomicU64::new(0),
        received: AtomicU64::new(0),
        cancellation: CancellationToken::new(),
    });
    let context = TransportReadContext::new(admission.clone());
    let body = HttpResponseBody::new(AdmissionBody::new(
        Bytes::from_static(b"body").into(),
        vec![context],
    ));
    assert!(body.bytes().await.is_err());
    assert_eq!(admission.received.load(Ordering::Relaxed), 4);
}

#[tokio::test]
async fn local_object_body_reservations_never_become_origin_receipts() {
    let inner = object_store::memory::InMemory::new();
    let path = Path::from("local");
    inner
        .put(&path, Bytes::from_static(b"body").into())
        .await
        .unwrap();
    let admission = Arc::new(CountingAdmission::default());
    let store = crate::Store::new(Arc::new(inner)).with_read_admission(admission.clone());
    assert_eq!(store.get_with_etag(&path).await.unwrap().0, b"body"[..]);
    assert_eq!(admission.bytes.load(Ordering::Relaxed), 4);
    assert_eq!(admission.received.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn external_transport_receipts_observe_consumed_bytes_even_after_cancellation() {
    let admission = Arc::new(CountingAdmission::default());
    let context = TransportReadContext::new(admission.clone());
    let receipt = scope(context, super::request()).await.unwrap().unwrap();
    admission.cancellation.cancel();
    assert!(receipt.bytes(4).await.is_err());
    assert_eq!(admission.received.load(Ordering::Relaxed), 4);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn s3_transport_counts_repeated_bytes_from_a_resumed_superset_response() {
    use std::io::{Read, Write};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = std::thread::spawn(move || {
        for (attempt, body) in [b"comp".as_slice(), b"complete".as_slice()]
            .into_iter()
            .enumerate()
        {
            let deadline = Instant::now() + Duration::from_secs(5);
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(connection) => break connection,
                    Err(error)
                        if error.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        std::thread::sleep(Duration::from_millis(5))
                    }
                    Err(error) => panic!("fixture connection did not arrive: {error}"),
                }
            };
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            let mut byte = [0];
            while !request.ends_with(b"\r\n\r\n") {
                assert_eq!(stream.read(&mut byte).unwrap(), 1);
                request.push(byte[0]);
                assert!(request.len() < 16 * 1024);
            }
            let status = if attempt == 0 {
                "200 OK"
            } else {
                assert!(
                    String::from_utf8_lossy(&request)
                        .to_ascii_lowercase()
                        .contains("range: bytes=4-7\r\n")
                );
                "206 Partial Content\r\nContent-Range: bytes 0-7/8"
            };
            write!(stream, "HTTP/1.1 {status}\r\nContent-Length: 8\r\nETag: \"fixture\"\r\nLast-Modified: Thu, 01 Jan 1970 00:00:00 GMT\r\nConnection: close\r\n\r\n").unwrap();
            stream.write_all(body).unwrap();
            // Keep the first frame separate from the premature EOF, so this
            // proves bytes consumed before failure rather than a failed header.
            if body.len() == 4 {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
    });
    let admission = Arc::new(CountingAdmission::default());
    let (progress, mut observations) = crate::ReadProgress::channel();
    let store = crate::build_explicit_store(
        "fixture",
        crate::ObjectStoreCredentials::Aws {
            access_key_id: "fixture-access".into(),
            secret_access_key: "fixture-secret".into(),
            session_token: None,
            region: "us-east-1".into(),
        },
        Some(&endpoint),
        true,
    )
    .unwrap()
    .with_retry_policy(crate::RetryPolicy {
        max_attempts: 2,
        base: std::time::Duration::from_millis(1),
        cap: std::time::Duration::from_millis(1),
    })
    .with_read_admission(progress.admission(admission.clone()));
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        store.get_with_etag(&Path::from("body")),
    )
    .await
    .unwrap();
    server.join().unwrap();
    assert_eq!(result.unwrap().0, b"complete"[..]);
    assert_eq!(admission.received.load(Ordering::Relaxed), 12);
    assert_eq!(progress.received_bytes(), 12);
    drop(store);
    drop(progress);
    assert_eq!(observations.next().await, Some(12));
    assert_eq!(observations.next().await, None);
}

#[derive(Debug)]
struct ConnectorBackedStore {
    inner: object_store::memory::InMemory,
    client: HttpClient,
    list_entries: Vec<ObjectMeta>,
}

impl std::fmt::Display for ConnectorBackedStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("ConnectorBackedStore")
    }
}

#[async_trait::async_trait]
impl ObjectStore for ConnectorBackedStore {
    async fn get_opts(&self, path: &Path, options: GetOptions) -> object_store::Result<GetResult> {
        let response = self
            .client
            .execute(request())
            .await
            .map_err(object_store_error)?;
        let (parts, body) = response.into_parts();
        body.bytes().await.map_err(object_store_error)?;
        let mut result = self.inner.get_opts(path, options).await?;
        result.extensions = parts.extensions;
        Ok(result)
    }

    async fn put_opts(
        &self,
        path: &Path,
        payload: PutPayload,
        options: PutOptions,
    ) -> object_store::Result<PutResult> {
        self.inner.put_opts(path, payload, options).await
    }

    async fn put_multipart_opts(
        &self,
        path: &Path,
        options: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        self.inner.put_multipart_opts(path, options).await
    }

    fn delete_stream(
        &self,
        paths: BoxStream<'static, object_store::Result<Path>>,
    ) -> BoxStream<'static, object_store::Result<Path>> {
        self.inner.delete_stream(paths)
    }

    fn list(&self, _prefix: Option<&Path>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        let entries = self.list_entries.clone();
        let client = self.client.clone();
        futures_util::stream::try_unfold(
            (0_usize, entries, client),
            |(index, entries, client)| async move {
                let Some(entry) = entries.get(index).cloned() else {
                    return Ok(None);
                };
                let response = client
                    .execute(request())
                    .await
                    .map_err(object_store_error)?;
                response
                    .into_body()
                    .bytes()
                    .await
                    .map_err(object_store_error)?;
                Ok(Some((entry, (index + 1, entries, client))))
            },
        )
        .boxed()
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
        self.inner.copy_opts(from, to, options).await
    }
}

#[tokio::test]
async fn connector_consumes_prepaid_request_then_charges_retries_and_response_bytes() {
    let admission = Arc::new(CountingAdmission::default());
    admission.request().await.unwrap();
    let context = TransportReadContext::new(admission.clone());
    let calls = Arc::new(AtomicU64::new(0));
    let service = ReadAdmissionService(HttpClient::new(StaticService {
        calls: calls.clone(),
    }));

    let first = scope(context.clone(), service.call(request()))
        .await
        .unwrap();
    assert!(first.extensions().get::<TransportReadAccounted>().is_some());
    assert_eq!(first.into_body().bytes().await.unwrap(), "body");
    let second = scope(context, service.call(request())).await.unwrap();
    assert_eq!(second.into_body().bytes().await.unwrap(), "body");

    assert_eq!(calls.load(Ordering::Relaxed), 2);
    assert_eq!(admission.requests.load(Ordering::Relaxed), 2);
    assert_eq!(admission.bytes.load(Ordering::Relaxed), 8);
    assert_eq!(admission.received.load(Ordering::Relaxed), 8);
}

#[tokio::test]
async fn nested_contexts_charge_every_policy_without_double_charging_first_request() {
    let outer = Arc::new(CountingAdmission::default());
    let inner = Arc::new(CountingAdmission::default());
    outer.request().await.unwrap();
    inner.request().await.unwrap();
    let outer_context = TransportReadContext::new(outer.clone());
    let inner_context = TransportReadContext::new(inner.clone());
    let service = ReadAdmissionService(HttpClient::new(StaticService {
        calls: Arc::new(AtomicU64::new(0)),
    }));

    let response = scope(outer_context, scope(inner_context, service.call(request())))
        .await
        .unwrap();
    assert_eq!(response.into_body().bytes().await.unwrap(), "body");

    assert_eq!(outer.requests.load(Ordering::Relaxed), 1);
    assert_eq!(inner.requests.load(Ordering::Relaxed), 1);
    assert_eq!(outer.bytes.load(Ordering::Relaxed), 4);
    assert_eq!(inner.bytes.load(Ordering::Relaxed), 4);
    assert_eq!(outer.received.load(Ordering::Relaxed), 4);
    assert_eq!(inner.received.load(Ordering::Relaxed), 4);
}

#[tokio::test]
async fn admitted_store_counts_connector_gets_and_each_listing_page() {
    let inner = object_store::memory::InMemory::new();
    for path in ["first", "second"] {
        inner
            .put(&Path::from(path), Bytes::from_static(b"body").into())
            .await
            .unwrap();
    }
    let list_entries = inner.list(None).try_collect::<Vec<_>>().await.unwrap();
    let calls = Arc::new(AtomicU64::new(0));
    let service = ReadAdmissionService(HttpClient::new(StaticService {
        calls: calls.clone(),
    }));
    let admission = Arc::new(CountingAdmission::default());
    let store = crate::Store::new(Arc::new(ConnectorBackedStore {
        inner,
        client: HttpClient::new(service),
        list_entries,
    }))
    .with_read_admission(admission.clone());

    assert_eq!(
        store
            .inner()
            .get(&Path::from("first"))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap(),
        "body"
    );
    assert_eq!(
        store
            .inner()
            .list(None)
            .try_collect::<Vec<_>>()
            .await
            .unwrap()
            .len(),
        2
    );

    assert_eq!(calls.load(Ordering::Relaxed), 3);
    assert_eq!(admission.requests.load(Ordering::Relaxed), 3);
    assert_eq!(admission.bytes.load(Ordering::Relaxed), 12);
    assert_eq!(admission.received.load(Ordering::Relaxed), 12);
}

#[tokio::test]
async fn listing_page_rejection_stops_before_the_next_http_request() {
    let inner = object_store::memory::InMemory::new();
    for path in ["first", "second"] {
        inner
            .put(&Path::from(path), Bytes::from_static(b"body").into())
            .await
            .unwrap();
    }
    let list_entries = inner.list(None).try_collect::<Vec<_>>().await.unwrap();
    let calls = Arc::new(AtomicU64::new(0));
    let service = ReadAdmissionService(HttpClient::new(StaticService {
        calls: calls.clone(),
    }));
    let admission = Arc::new(RequestLimit {
        remaining: AtomicU64::new(1),
        cancellation: CancellationToken::new(),
    });
    let store = crate::Store::new(Arc::new(ConnectorBackedStore {
        inner,
        client: HttpClient::new(service),
        list_entries,
    }))
    .with_read_admission(admission);

    let error = store
        .inner()
        .list(None)
        .try_collect::<Vec<_>>()
        .await
        .unwrap_err();

    assert!(crate::read_rejection(&error).is_some());
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}
