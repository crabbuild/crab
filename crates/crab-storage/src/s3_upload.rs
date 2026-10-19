//! Bounded, retryable S3 multipart uploads and atomic immutable completion.
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use crate::multipart::UPLOAD_FRAME_BYTES;
use async_trait::async_trait;
use bytes::Bytes;
use futures_util::StreamExt as _;
use futures_util::stream::{BoxStream, FuturesUnordered};
use object_store::aws::AmazonS3;
use object_store::client::HttpRequest;
use object_store::multipart::{MultipartStore, PartId};
use object_store::path::Path;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMode, PutMultipartOptions, PutOptions, PutPayload, PutResult,
};
use tokio::sync::Semaphore;

const MULTIPART_THRESHOLD: usize = 32 * 1024 * 1024;
const PART_BYTES: usize = 16 * 1024 * 1024;
const MAX_PARTS: usize = 10_000;
const MAX_PART_BYTES: u64 = 5 * 1024 * 1024 * 1024;
const IN_FLIGHT: usize = 4;

tokio::task_local! {
    static CREATE_COMPLETION: ();
}

pub(crate) fn condition_completion(request: &mut HttpRequest) {
    // object_store 0.14.1 adds this header after signing in its conditional-copy
    // implementation too. Scope only completion: parts and ordinary overwrite
    // uploads must retain their own semantics, even in concurrent tasks.
    if CREATE_COMPLETION.try_with(|()| ()).is_ok()
        && request.method() == "POST"
        && request.uri().query().is_some_and(|query| {
            url::form_urlencoded::parse(query.as_bytes()).any(|(key, _)| key == "uploadId")
        })
    {
        request.headers_mut().insert(
            "if-none-match",
            hyper::header::HeaderValue::from_static("*"),
        );
    }
}

#[derive(Debug)]
pub(crate) struct S3Store {
    inner: Arc<AmazonS3>,
    parts: Arc<Semaphore>,
    conditional_create: bool,
}

impl std::fmt::Display for S3Store {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.inner.fmt(formatter)
    }
}

impl S3Store {
    pub(crate) fn new(inner: Arc<AmazonS3>, conditional_create: bool) -> Self {
        Self {
            inner,
            parts: Arc::new(Semaphore::new(IN_FLIGHT)),
            conditional_create,
        }
    }

    async fn create(
        &self,
        path: &Path,
        payload: PutPayload,
        opts: PutOptions,
    ) -> object_store::Result<PutResult> {
        let part_size = PART_BYTES.max(payload.content_length().div_ceil(MAX_PARTS));
        if part_size as u64 > MAX_PART_BYTES {
            return Err(object_store::Error::NotSupported {
                source: "immutable multipart object exceeds S3 part limits".into(),
            });
        }
        let options = PutMultipartOptions {
            tags: opts.tags,
            attributes: opts.attributes,
            extensions: opts.extensions,
        };
        let mut upload = self.put_multipart_opts(path, options).await?;
        let result = Self::upload_parts(&mut *upload, path, payload, part_size).await;
        if result.is_err()
            && let Err(error) = upload.abort().await
        {
            tracing::warn!(%error, "failed to abort immutable multipart upload");
        }
        result
    }

    async fn upload_parts(
        upload: &mut dyn MultipartUpload,
        path: &Path,
        payload: PutPayload,
        part_size: usize,
    ) -> object_store::Result<PutResult> {
        let mut pending = FuturesUnordered::new();
        let mut buffers = Vec::new();
        let mut length = 0;
        for bytes in payload {
            let mut offset = 0;
            while offset < bytes.len() {
                let take = (part_size - length)
                    .min(UPLOAD_FRAME_BYTES)
                    .min(bytes.len() - offset);
                buffers.push(bytes.slice(offset..offset + take));
                offset += take;
                length += take;
                if length == part_size {
                    if pending.len() == IN_FLIGHT
                        && let Some(result) = pending.next().await
                    {
                        result?;
                    }
                    pending
                        .push(upload.put_part(std::mem::take(&mut buffers).into_iter().collect()));
                    length = 0;
                }
            }
        }
        if !buffers.is_empty() {
            if pending.len() == IN_FLIGHT
                && let Some(result) = pending.next().await
            {
                result?;
            }
            pending.push(upload.put_part(buffers.into_iter().collect()));
        }
        while let Some(result) = pending.next().await {
            result?;
        }
        let result = CREATE_COMPLETION.scope((), upload.complete()).await;
        match result {
            // Normalize only completion's occupied-key response. Part failures
            // must not masquerade as a successful competing publication.
            Err(
                source @ (object_store::Error::Precondition { .. }
                | object_store::Error::NotModified { .. }),
            ) => Err(object_store::Error::AlreadyExists {
                path: path.to_string(),
                source: Box::new(source),
            }),
            // S3 requires a new upload after completion's HTTP 409. The
            // dependency maps it to AlreadyExists; unlike HTTP 412 this does
            // not prove an occupied key that logical-create callers can read.
            Err(source @ object_store::Error::AlreadyExists { .. }) => {
                Err(object_store::Error::Generic {
                    store: "S3 multipart create",
                    source: Box::new(crate::StorageError::NetworkTransient { source }),
                })
            }
            other => other,
        }
    }
}

struct S3Upload {
    state: Arc<UploadState>,
    next_index: usize,
    active: bool,
}

#[derive(Debug)]
struct UploadState {
    inner: Arc<AmazonS3>,
    path: Path,
    id: String,
    completed: Mutex<BTreeMap<usize, PartId>>,
    permits: Arc<Semaphore>,
    runtime: tokio::runtime::Handle,
}

impl std::fmt::Debug for S3Upload {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("S3Upload")
            .field("next_index", &self.next_index)
            .finish_non_exhaustive()
    }
}

impl UploadState {
    async fn part(&self, index: usize, payload: PutPayload) -> object_store::Result<PartId> {
        let _permit =
            self.permits
                .acquire()
                .await
                .map_err(|source| object_store::Error::Generic {
                    store: "S3 multipart",
                    source: Box::new(source),
                })?;
        // Retrying high-level put_part would allocate a new index and could
        // duplicate bytes. Retry the same immutable payload and explicit index.
        crate::retry(&crate::RetryPolicy::DEFAULT, || {
            let payload = payload.clone();
            async move {
                MultipartStore::put_part(&*self.inner, &self.path, &self.id, index, payload)
                    .await
                    .map_err(|source| crate::map_object_store_error(source, self.path.as_ref()))
            }
        })
        .await
        .map_err(|source| match source {
            crate::StorageError::ObjectStore { source, .. }
            | crate::StorageError::NetworkTransient { source } => source,
            source => object_store::Error::Generic {
                store: "S3 multipart",
                source: Box::new(source),
            },
        })
    }
}

#[async_trait]
impl MultipartUpload for S3Upload {
    fn put_part(&mut self, payload: PutPayload) -> object_store::UploadPart {
        let index = self.next_index;
        self.next_index += 1;
        let state = Arc::clone(&self.state);
        Box::pin(async move {
            let part = state.part(index, payload).await?;
            state
                .completed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(index, part);
            Ok(())
        })
    }

    async fn complete(&mut self) -> object_store::Result<PutResult> {
        let parts = {
            let completed = self
                .state
                .completed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if completed.len() != self.next_index {
                return Err(object_store::Error::Generic {
                    store: "S3 multipart",
                    source: "multipart parts are incomplete".into(),
                });
            }
            completed.values().cloned().collect()
        };
        let result = MultipartStore::complete_multipart(
            &*self.state.inner,
            &self.state.path,
            &self.state.id,
            parts,
        )
        .await;
        if result.is_ok() {
            self.active = false;
        }
        result
    }

    async fn abort(&mut self) -> object_store::Result<()> {
        let result =
            MultipartStore::abort_multipart(&*self.state.inner, &self.state.path, &self.state.id)
                .await;
        if result.is_ok() || matches!(result, Err(object_store::Error::NotFound { .. })) {
            self.active = false;
        }
        result
    }
}

impl Drop for S3Upload {
    fn drop(&mut self) {
        // Ordinary and immutable upload cancellation share abort ownership.
        // Returned failures await abort before their caller retries the upload.
        if self.active {
            let state = Arc::clone(&self.state);
            self.state.runtime.spawn(async move {
                if let Err(error) =
                    MultipartStore::abort_multipart(&*state.inner, &state.path, &state.id).await
                {
                    tracing::warn!(%error, "failed to abort cancelled multipart upload");
                }
            });
        }
    }
}

#[async_trait]
impl ObjectStore for S3Store {
    async fn put_opts(
        &self,
        path: &Path,
        payload: PutPayload,
        opts: PutOptions,
    ) -> object_store::Result<PutResult> {
        if self.conditional_create
            && opts.mode == PutMode::Create
            && payload.content_length() >= MULTIPART_THRESHOLD
        {
            return self.create(path, payload, opts).await;
        }
        self.inner.put_opts(path, payload, opts).await
    }

    async fn put_multipart_opts(
        &self,
        path: &Path,
        opts: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        let runtime = tokio::runtime::Handle::try_current().map_err(|source| {
            object_store::Error::Generic {
                store: "S3 multipart",
                source: Box::new(source),
            }
        })?;
        let id = MultipartStore::create_multipart_opts(&*self.inner, path, opts).await?;
        Ok(Box::new(S3Upload {
            state: Arc::new(UploadState {
                inner: Arc::clone(&self.inner),
                path: path.clone(),
                id,
                completed: Mutex::new(BTreeMap::new()),
                permits: Arc::clone(&self.parts),
                runtime,
            }),
            next_index: 0,
            active: true,
        }))
    }

    async fn get_opts(&self, path: &Path, opts: GetOptions) -> object_store::Result<GetResult> {
        self.inner.get_opts(path, opts).await
    }

    async fn get_ranges(
        &self,
        path: &Path,
        ranges: &[std::ops::Range<u64>],
    ) -> object_store::Result<Vec<Bytes>> {
        self.inner.get_ranges(path, ranges).await
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, object_store::Result<Path>>,
    ) -> BoxStream<'static, object_store::Result<Path>> {
        self.inner.delete_stream(locations)
    }

    fn list(&self, prefix: Option<&Path>) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.inner.list(prefix)
    }

    fn list_with_offset(
        &self,
        prefix: Option<&Path>,
        offset: &Path,
    ) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.inner.list_with_offset(prefix, offset)
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

#[cfg(test)]
mod tests {
    use super::*;
    use object_store::ObjectStoreExt as _;
    use object_store::aws::AmazonS3Builder;
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    #[derive(Default)]
    struct State {
        next: usize,
        sessions: BTreeMap<String, BTreeMap<usize, Vec<u8>>>,
        object: Option<Vec<u8>>,
        conditions: Vec<bool>,
        part_lengths: Vec<usize>,
        aborts: usize,
        lose_completion: bool,
        fail_part: bool,
        hold_parts: bool,
        lose_part: bool,
        conflict_completion: bool,
    }

    struct Endpoint {
        state: Arc<Mutex<State>>,
        task: tokio::task::JoinHandle<()>,
        store: crate::Store,
    }

    impl Drop for Endpoint {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    impl Endpoint {
        async fn new() -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}", listener.local_addr().unwrap());
            let state = Arc::new(Mutex::new(State::default()));
            let serving = Arc::clone(&state);
            let task = tokio::spawn(async move {
                loop {
                    let (socket, _) = listener.accept().await.unwrap();
                    let state = Arc::clone(&serving);
                    tokio::spawn(serve(socket, state));
                }
            });
            let s3 = AmazonS3Builder::new()
                .with_bucket_name("bucket")
                .with_access_key_id("test")
                .with_secret_access_key("test")
                .with_region("us-east-1")
                .with_endpoint(url)
                .with_allow_http(true)
                .with_retry(object_store::RetryConfig {
                    max_retries: 0,
                    ..Default::default()
                })
                .with_http_connector(
                    crate::transport_read_admission::ReadAdmissionConnector::default(),
                )
                .build()
                .unwrap();
            let store = crate::Store::with_retry(
                Arc::new(S3Store::new(Arc::new(s3), true)),
                crate::RetryPolicy {
                    max_attempts: 2,
                    base: std::time::Duration::from_millis(1),
                    ..crate::RetryPolicy::DEFAULT
                },
            );
            Self { state, task, store }
        }
    }

    async fn serve(mut socket: tokio::net::TcpStream, state: Arc<Mutex<State>>) {
        let mut request = Vec::new();
        let mut chunk = [0; 8192];
        let header_end = loop {
            let count = socket.read(&mut chunk).await.unwrap();
            if count == 0 {
                return;
            }
            request.extend_from_slice(&chunk[..count]);
            if let Some(end) = request.windows(4).position(|v| v == b"\r\n\r\n") {
                break end + 4;
            }
        };
        let headers = String::from_utf8(request[..header_end].to_vec()).unwrap();
        let first = headers.lines().next().unwrap();
        let mut words = first.split_whitespace();
        let method = words.next().unwrap();
        let uri = words.next().unwrap();
        let length = headers
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
            .map_or(0, |(_, value)| value.trim().parse::<usize>().unwrap());
        while request.len() - header_end < length {
            let count = socket.read(&mut chunk).await.unwrap();
            if count == 0 {
                return;
            }
            request.extend_from_slice(&chunk[..count]);
        }
        let query = uri.split_once('?').map_or("", |(_, q)| q);
        let parameters: BTreeMap<_, _> = url::form_urlencoded::parse(query.as_bytes())
            .into_owned()
            .collect();
        let conditional = headers
            .lines()
            .any(|line| line.eq_ignore_ascii_case("if-none-match: *"));
        let (status, body, hold) = {
            let mut state = state.lock().unwrap();
            match (method, parameters.get("uploadId")) {
                ("POST", None) => {
                    assert!(!conditional);
                    state.next += 1;
                    let id = state.next.to_string();
                    state.sessions.insert(id.clone(), BTreeMap::new());
                    (200, format!("<InitiateMultipartUploadResult><UploadId>{id}</UploadId></InitiateMultipartUploadResult>").into_bytes(), false)
                }
                ("PUT", Some(id)) => {
                    assert!(!conditional);
                    state.part_lengths.push(length);
                    let part = parameters["partNumber"].parse().unwrap();
                    if let Some(parts) = state.sessions.get_mut(id) {
                        parts.insert(part, request[header_end..].to_vec());
                        if state.lose_part {
                            state.lose_part = false;
                            return;
                        }
                        (
                            if state.fail_part { 400 } else { 200 },
                            Vec::new(),
                            state.hold_parts,
                        )
                    } else {
                        // An abort can reach the server before another part's body.
                        // S3 rejects that upload ID; the fixture must not panic.
                        (
                            404,
                            b"<Error><Code>NoSuchUpload</Code></Error>".to_vec(),
                            false,
                        )
                    }
                }
                ("POST", Some(id)) => {
                    state.conditions.push(conditional);
                    if state.conflict_completion {
                        state.conflict_completion = false;
                        (
                            409,
                            b"<Error><Code>ConditionalRequestConflict</Code></Error>".to_vec(),
                            false,
                        )
                    } else if conditional && state.object.is_some() {
                        (
                            412,
                            b"<Error><Code>PreconditionFailed</Code></Error>".to_vec(),
                            false,
                        )
                    } else {
                        let parts = state.sessions.remove(id).unwrap();
                        let xml = String::from_utf8_lossy(&request[header_end..]);
                        let mut position = 0;
                        for part in parts.values() {
                            let hash = blake3::hash(part).to_hex().to_string();
                            position += xml[position..]
                                .find(&hash)
                                .expect("completion retains ordered part validators")
                                + hash.len();
                        }
                        state.object = Some(parts.into_values().flatten().collect());
                        if state.lose_completion {
                            state.lose_completion = false;
                            return;
                        }
                        (200, b"<CompleteMultipartUploadResult><ETag>\"object\"</ETag></CompleteMultipartUploadResult>".to_vec(), false)
                    }
                }
                ("DELETE", Some(id)) => {
                    assert!(!conditional);
                    state.sessions.remove(id);
                    state.aborts += 1;
                    (204, Vec::new(), false)
                }
                ("GET", None) => {
                    assert!(!conditional);
                    (200, state.object.clone().unwrap(), false)
                }
                ("PUT", None) => {
                    if conditional && state.object.is_some() {
                        (412, Vec::new(), false)
                    } else {
                        state.object = Some(request[header_end..].to_vec());
                        (200, Vec::new(), false)
                    }
                }
                other => panic!("unexpected request {other:?}"),
            }
        };
        if hold {
            std::future::pending::<()>().await;
        }
        let etag = blake3::hash(&request[header_end..]).to_hex().to_string();
        if parameters.get("partNumber").is_some_and(|part| part == "1") {
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        }
        let response = format!(
            "HTTP/1.1 {status} OK\r\nContent-Length: {}\r\nETag: \"{etag}\"\r\nLast-Modified: Wed, 01 Oct 2025 00:00:00 GMT\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = socket.write_all(response.as_bytes()).await;
        let _ = socket.write_all(&body).await;
    }

    fn body(value: u8) -> Bytes {
        let mut bytes = vec![0; MULTIPART_THRESHOLD + 3];
        for (index, chunk) in bytes.chunks_mut(256 * 1024).enumerate() {
            chunk.fill(value.wrapping_add(index as u8));
        }
        Bytes::from(bytes)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn multipart_create_is_atomic_and_identical_writers_are_idempotent() {
        let endpoint = Endpoint::new().await;
        let path = Path::from("objects/large");
        let bytes = body(7);
        let (first, second) = tokio::join!(
            endpoint.store.put_if_absent_verified(&path, bytes.clone()),
            endpoint.store.put_if_absent_verified(&path, bytes.clone()),
        );
        assert_ne!(first.unwrap(), second.unwrap());
        let state = endpoint.state.lock().unwrap();
        assert_eq!(state.object.as_deref(), Some(bytes.as_ref()));
        assert!(state.conditions.iter().all(|condition| *condition));
        assert!(
            state
                .part_lengths
                .iter()
                .all(|length| *length <= PART_BYTES)
        );
        assert!(state.sessions.is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn occupied_key_is_never_replaced_by_different_content() {
        let endpoint = Endpoint::new().await;
        let path = Path::from("objects/large");
        let original = body(1);
        endpoint.store.put(&path, original.clone()).await.unwrap();
        assert!(matches!(
            endpoint.store.put(&path, body(2)).await,
            Err(crate::StorageError::StateConflict { .. })
        ));
        assert_eq!(
            endpoint.state.lock().unwrap().object.as_deref(),
            Some(original.as_ref())
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn occupied_multipart_create_preserves_object_store_error_contract() {
        let endpoint = Endpoint::new().await;
        let path = Path::from("objects/large");
        endpoint.store.put(&path, body(1)).await.unwrap();
        let result = endpoint
            .store
            .inner()
            .put_opts(&path, body(2).into(), PutMode::Create.into())
            .await;
        assert!(matches!(
            result,
            Err(object_store::Error::AlreadyExists { .. })
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn lost_completion_response_recovers_without_replacing_object() {
        let endpoint = Endpoint::new().await;
        endpoint.state.lock().unwrap().lose_completion = true;
        let bytes = body(3);
        endpoint
            .store
            .put_if_absent_verified(&Path::from("objects/large"), bytes.clone())
            .await
            .unwrap();
        assert_eq!(
            endpoint.state.lock().unwrap().object.as_deref(),
            Some(bytes.as_ref())
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn part_failure_aborts_before_returning() {
        let endpoint = Endpoint::new().await;
        endpoint.state.lock().unwrap().fail_part = true;
        assert!(
            endpoint
                .store
                .put(&Path::from("objects/large"), body(4))
                .await
                .is_err()
        );
        let state = endpoint.state.lock().unwrap();
        assert!(state.object.is_none());
        assert!(state.sessions.is_empty());
        assert!(state.aborts > 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancelled_upload_aborts_without_publication() {
        let endpoint = Endpoint::new().await;
        endpoint.state.lock().unwrap().hold_parts = true;
        let store = endpoint.store.clone();
        let task =
            tokio::spawn(async move { store.put(&Path::from("objects/large"), body(5)).await });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if !endpoint.state.lock().unwrap().part_lengths.is_empty() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        task.abort();
        let _ = task.await;
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if endpoint.state.lock().unwrap().aborts > 0 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(endpoint.state.lock().unwrap().object.is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn overwrite_upload_and_small_create_keep_their_contracts() {
        let endpoint = Endpoint::new().await;
        let path = Path::from("objects/small");
        endpoint
            .store
            .create_strict(&path, Bytes::from_static(b"small"))
            .await
            .unwrap();
        endpoint.store.put_overwrite(&path, body(6)).await.unwrap();
        assert_eq!(endpoint.state.lock().unwrap().next, 0);
        let mut upload = endpoint.store.inner().put_multipart(&path).await.unwrap();
        upload
            .put_part(Bytes::from_static(b"replacement").into())
            .await
            .unwrap();
        upload.complete().await.unwrap();
        assert_eq!(endpoint.state.lock().unwrap().conditions, [false]);
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore = "requires a dedicated live S3 bucket and uploads at least 256 MiB"]
    async fn live_large_immutable_create_preserves_bytes_and_rejects_replacement() {
        let bucket = std::env::var("CRAB_TEST_BUCKET").unwrap();
        let size: usize =
            std::env::var("CRAB_TEST_OBJECT_MIB").map_or(256, |v| v.parse().unwrap()) * 1024 * 1024;
        let store = crate::build_static_env_store(&bucket, crate::StorageProviderKind::S3).unwrap();
        let path = Path::from(format!(
            "transport-probe/fixed-native-multipart-{}-{size}",
            rand::random::<u64>()
        ));
        let bytes = Bytes::from(vec![0x6d; UPLOAD_FRAME_BYTES]);
        let payload: PutPayload = (0..size)
            .step_by(UPLOAD_FRAME_BYTES)
            .map(|offset| bytes.slice(..UPLOAD_FRAME_BYTES.min(size - offset)))
            .collect();
        let mut expected = blake3::Hasher::new();
        for part in &payload {
            expected.update(part);
        }
        store
            .inner()
            .put_opts(&path, payload, PutMode::Create.into())
            .await
            .unwrap();
        let mut stream = store.inner().get(&path).await.unwrap().into_stream();
        let mut hash = blake3::Hasher::new();
        let mut received = 0;
        while let Some(bytes) = stream.next().await {
            let bytes = bytes.unwrap();
            received += bytes.len();
            hash.update(&bytes);
        }
        assert_eq!((received, hash.finalize()), (size, expected.finalize()));
        let replacement: PutPayload = (0..MULTIPART_THRESHOLD)
            .step_by(UPLOAD_FRAME_BYTES)
            .map(|_| Bytes::from(vec![0x7a; UPLOAD_FRAME_BYTES]))
            .collect();
        assert!(matches!(
            store
                .inner()
                .put_opts(&path, replacement, PutMode::Create.into())
                .await,
            Err(object_store::Error::Precondition { .. }
                | object_store::Error::AlreadyExists { .. })
        ));
        assert!(matches!(
            store
                .create_strict(&path, Bytes::from_static(b"different"))
                .await,
            Err(crate::StorageError::StateConflict { .. })
        ));
        assert_eq!(store.head(&path).await.unwrap().size, size as u64);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn lost_part_response_retries_the_same_index() {
        let endpoint = Endpoint::new().await;
        endpoint.state.lock().unwrap().lose_part = true;
        let bytes = body(8);
        endpoint
            .store
            .put_if_absent_verified(&Path::from("objects/large"), bytes.clone())
            .await
            .unwrap();
        let state = endpoint.state.lock().unwrap();
        assert_eq!(state.object.as_deref(), Some(bytes.as_ref()));
        assert_eq!(state.next, 1);
        assert_eq!(state.part_lengths.len(), 4);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn logical_immutable_conflict_returns_existing_encoding() {
        let endpoint = Endpoint::new().await;
        let path = Path::from("objects/logical");
        let original = body(9);
        endpoint
            .store
            .create_or_read_immutable(&path, original.clone(), original.len() as u64)
            .await
            .unwrap();
        let result = endpoint
            .store
            .create_or_read_immutable(&path, body(10), original.len() as u64)
            .await
            .unwrap();
        assert!(
            matches!(result, crate::ImmutableCreateOutcome::Existing { bytes, .. } if bytes == original)
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn multipart_create_records_the_staged_write() {
        let endpoint = Endpoint::new().await;
        let store = endpoint
            .store
            .clone()
            .with_staging_writes("uploads/attempt".to_owned());
        let bytes = body(11);
        let path = Path::from("objects/staged");
        store
            .put_if_absent_verified(&path, bytes.clone())
            .await
            .unwrap();
        let writes = store.staged_writes();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].canonical_key, path.as_ref());
        assert!(writes[0].staged_key.starts_with("uploads/attempt/"));
        assert_eq!(writes[0].size, bytes.len() as u64);
        assert_eq!(writes[0].blake3, blake3::hash(&bytes).to_hex().as_str());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn completion_conflict_restarts_before_logical_create_reads_existing() {
        let endpoint = Endpoint::new().await;
        endpoint.state.lock().unwrap().conflict_completion = true;
        let bytes = body(12);
        let result = endpoint
            .store
            .create_or_read_immutable(
                &Path::from("objects/logical"),
                bytes.clone(),
                bytes.len() as u64,
            )
            .await
            .unwrap();
        assert!(matches!(result, crate::ImmutableCreateOutcome::Created(_)));
        let state = endpoint.state.lock().unwrap();
        assert_eq!(state.next, 2);
        assert_eq!(state.aborts, 1);
        assert_eq!(state.object.as_deref(), Some(bytes.as_ref()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ordinary_multipart_retries_uncertain_parts_without_restarting() {
        let endpoint = Endpoint::new().await;
        endpoint.state.lock().unwrap().lose_part = true;
        let bytes = body(13);
        endpoint
            .store
            .put_multipart_retry(
                &Path::from("objects/cache"),
                bytes.clone(),
                PART_BYTES,
                &tokio_util::sync::CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        let state = endpoint.state.lock().unwrap();
        assert_eq!(state.object.as_deref(), Some(bytes.as_ref()));
        assert_eq!(state.next, 1);
        assert_eq!(state.conditions, [false]);
        assert_eq!(state.part_lengths.len(), 4);
    }
}
