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
            .with_http_connector(crate::transport_read_admission::ReadAdmissionConnector::default())
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
    let task = tokio::spawn(async move { store.put(&Path::from("objects/large"), body(5)).await });
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
    let start = std::time::Instant::now();
    store
        .inner()
        .put_opts(&path, payload, PutMode::Create.into())
        .await
        .unwrap();
    let upload_s = start.elapsed().as_secs_f64();
    let start = std::time::Instant::now();
    let mut stream = store.inner().get(&path).await.unwrap().into_stream();
    let mut hash = blake3::Hasher::new();
    let mut received = 0;
    while let Some(bytes) = stream.next().await {
        let bytes = bytes.unwrap();
        received += bytes.len();
        hash.update(&bytes);
    }
    assert_eq!((received, hash.finalize()), (size, expected.finalize()));
    let download_s = start.elapsed().as_secs_f64();
    let replacement: PutPayload = (0..MULTIPART_THRESHOLD)
        .step_by(UPLOAD_FRAME_BYTES)
        .map(|_| Bytes::from(vec![0x7a; UPLOAD_FRAME_BYTES]))
        .collect();
    assert!(matches!(
        store
            .inner()
            .put_opts(&path, replacement, PutMode::Create.into())
            .await,
        Err(object_store::Error::Precondition { .. } | object_store::Error::AlreadyExists { .. })
    ));
    assert!(matches!(
        store
            .create_strict(&path, Bytes::from_static(b"different"))
            .await,
        Err(crate::StorageError::StateConflict { .. })
    ));
    assert_eq!(store.head(&path).await.unwrap().size, size as u64);
    println!(
        "{}",
        serde_json::json!({"key":path.as_ref(), "bytes":size,
        "upload_s":upload_s, "download_s":download_s, "blake3_matched":true,
        "conflicting_replacement_rejected":true})
    );
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
