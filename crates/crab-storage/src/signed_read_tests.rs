use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bytes::Bytes;
use object_store::memory::InMemory;
use object_store::path::Path;
use object_store::signer::Signer;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use crate::{
    ReadAdmission, StorageError, StorageObservation, StorageObserver, StorageOperation, Store,
};

#[derive(Debug, Default)]
struct SigningProbe(AtomicU64);

#[async_trait::async_trait]
impl Signer for SigningProbe {
    async fn signed_url(
        &self,
        _: reqwest::Method,
        _: &Path,
        _: Duration,
    ) -> object_store::Result<url::Url> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(object_store::Error::NotSupported {
            source: "unexpected direct transport".into(),
        })
    }
}

#[derive(Default)]
struct ReadPolicy {
    cancel: CancellationToken,
    reject: bool,
    requests: AtomicU64,
    bytes: AtomicU64,
    observed_ranges: AtomicU64,
    observed_bytes: AtomicU64,
}

#[derive(Debug, thiserror::Error)]
#[error("read denied by caller")]
struct Denied;

#[async_trait::async_trait]
impl ReadAdmission for ReadPolicy {
    fn cancellation(&self) -> &CancellationToken {
        &self.cancel
    }

    async fn request(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.requests.fetch_add(1, Ordering::SeqCst);
        if self.reject {
            return Err(Box::new(Denied));
        }
        Ok(())
    }

    async fn bytes(&self, bytes: u64) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.bytes.fetch_add(bytes, Ordering::SeqCst);
        Ok(())
    }
}

impl StorageObserver for ReadPolicy {
    fn started(&self, _: StorageOperation) {}

    fn finished(&self, observation: StorageObservation) {
        if observation.operation == StorageOperation::Range {
            self.observed_ranges.fetch_add(1, Ordering::SeqCst);
            self.observed_bytes
                .fetch_add(observation.bytes_read, Ordering::SeqCst);
        }
    }
}

const LARGE_RANGE: u64 = 8 * 1024 * 1024;

#[derive(Debug)]
struct EndpointSigner(url::Url);

#[async_trait::async_trait]
impl Signer for EndpointSigner {
    async fn signed_url(
        &self,
        _: reqwest::Method,
        _: &Path,
        _: Duration,
    ) -> object_store::Result<url::Url> {
        Ok(self.0.clone())
    }
}

async fn signed_range_endpoint(
    content_range: Option<&str>,
    body: Bytes,
    requested_range: &str,
) -> (Store, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/object", listener.local_addr().unwrap());
    let content_range = content_range
        .map(|value| format!("Content-Range: {value}\r\n"))
        .unwrap_or_default();
    let headers = format!(
        "HTTP/1.1 206 Partial Content\r\n{content_range}Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let requested_range = format!("range: {requested_range}\r\n");
    let server = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                assert!(request.len() < 16 * 1024);
                request.push(stream.read_u8().await.unwrap());
            }
            assert!(
                String::from_utf8(request)
                    .unwrap()
                    .to_ascii_lowercase()
                    .contains(&requested_range)
            );
            stream.write_all(headers.as_bytes()).await.unwrap();
            // A rejected header can close the connection before its body is sent.
            let _ = stream.write_all(&body).await;
        }
    });
    let store = Store::new(Arc::new(InMemory::new()))
        .with_signer(Arc::new(EndpointSigner(url.parse().unwrap())))
        .with_retry_policy(crate::RetryPolicy {
            max_attempts: 1,
            base: Duration::ZERO,
            cap: Duration::ZERO,
        });
    (store, server)
}

#[tokio::test]
async fn signed_file_reads_require_the_requested_content_range() {
    for header in [
        None,
        Some("bytes 0-7/16"),
        Some("bytes 4-10/16"),
        Some("bytes 4-11/8"),
        Some("bytes 4-18446744073709551615/16"),
        Some("bytes 4-11/*"),
        Some("invalid"),
        Some("bytes 4-11/16\r\nContent-Range: bytes 0-7/16"),
    ] {
        let body = Bytes::from_static(b"abcdefgh");
        let (store, server) = signed_range_endpoint(header, body.clone(), "bytes=4-11").await;
        let directory = tempfile::tempdir().unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            store.try_download_signed_ranges_to_path(
                &Path::from("object"),
                &directory.path().join("pack"),
                16,
                blake3::hash(&body).to_hex().as_str(),
                4..8,
                8..12,
                &CancellationToken::new(),
            ),
        )
        .await;
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
        assert!(
            matches!(result.unwrap(), Err(StorageError::CorruptObject { .. })),
            "accepted invalid Content-Range: {header:?}"
        );
    }
}

#[tokio::test]
async fn signed_large_ranges_validate_offsets_before_returning_bytes() {
    for valid in [false, true] {
        let body = Bytes::from(vec![0x5a; LARGE_RANGE as usize]);
        let start = if valid { 4 } else { 0 };
        let header = format!(
            "bytes {start}-{}/{}",
            start + LARGE_RANGE - 1,
            LARGE_RANGE + 4
        );
        let request = format!("bytes=4-{}", LARGE_RANGE + 3);
        let (store, server) = signed_range_endpoint(Some(&header), body.clone(), &request).await;
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            store.range_get(&Path::from("object"), 4..LARGE_RANGE + 4),
        )
        .await;
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
        let result = result.unwrap();
        if valid {
            assert_eq!(result.unwrap(), body);
        } else {
            assert!(matches!(result, Err(StorageError::CorruptObject { .. })));
        }
    }
}

#[tokio::test]
async fn signed_file_reads_preserve_nonzero_offsets_and_hashes() {
    let body = Bytes::from_static(b"abcdefgh");
    let (store, server) =
        signed_range_endpoint(Some("bytes 4-11/16"), body.clone(), "bytes=4-11").await;
    let directory = tempfile::tempdir().unwrap();
    let destination = directory.path().join("pack");
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        store.try_download_signed_ranges_to_path(
            &Path::from("object"),
            &destination,
            16,
            blake3::hash(&body).to_hex().as_str(),
            4..8,
            8..12,
            &CancellationToken::new(),
        ),
    )
    .await;
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
    let (sidecars, hash) = result.unwrap().unwrap().unwrap();
    assert_eq!(std::fs::read(destination).unwrap(), b"abcd");
    assert_eq!(sidecars, b"efgh"[..]);
    assert_eq!(hash, blake3::hash(b"abcd"));
}

#[tokio::test]
async fn signed_file_cancellation_stops_headers_and_bodies_before_returning() {
    for coalesced in [true, false] {
        for send_body_prefix in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/object", listener.local_addr().unwrap());
            let count = if coalesced { 1 } else { 2 };
            let (ready, mut requests) = tokio::sync::mpsc::channel(count);
            let server = tokio::spawn(async move {
                let mut connections = tokio::task::JoinSet::new();
                for _ in 0..count {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let ready = ready.clone();
                    connections.spawn(async move {
                        let mut request = Vec::new();
                        while !request.ends_with(b"\r\n\r\n") {
                            assert!(request.len() < 16 * 1024);
                            request.push(stream.read_u8().await.unwrap());
                        }
                        if send_body_prefix {
                            let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
                            let range = request.lines().find_map(|line| line.strip_prefix("range: bytes=")).unwrap();
                            let (start, end) = range.split_once('-').unwrap();
                            let length = end.parse::<u64>().unwrap() - start.parse::<u64>().unwrap() + 1;
                            let headers = format!("HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {range}/12\r\nContent-Length: {length}\r\nConnection: close\r\n\r\nab");
                            stream.write_all(headers.as_bytes()).await.unwrap();
                        }
                        ready.send(()).await.unwrap();
                        // Cancellation must close every in-flight source read, not
                        // return while a detached worker still owns this connection.
                        let mut trailing = Vec::new();
                        let _ = stream.read_to_end(&mut trailing).await;
                    });
                }
                while let Some(result) = connections.join_next().await {
                    result.unwrap();
                }
            });
            let store = Store::new(Arc::new(InMemory::new()))
                .with_signer(Arc::new(EndpointSigner(url.parse().unwrap())));
            let directory = tempfile::tempdir().unwrap();
            let destination = directory.path().join("pack");
            let cancel = CancellationToken::new();
            let task_cancel = cancel.clone();
            let mut download = tokio::spawn(async move {
                store
                    .try_download_signed_ranges_to_path(
                        &Path::from("object"),
                        &destination,
                        12,
                        &"a".repeat(64),
                        0..4,
                        if coalesced { 4..8 } else { 8..12 },
                        &task_cancel,
                    )
                    .await
            });
            for _ in 0..count {
                tokio::time::timeout(Duration::from_secs(5), requests.recv())
                    .await
                    .unwrap()
                    .unwrap();
            }
            cancel.cancel();
            let result = tokio::time::timeout(Duration::from_secs(2), &mut download).await;
            if result.is_err() {
                download.abort();
                let _ = download.await;
            }
            let mut server = server;
            let closed = tokio::time::timeout(Duration::from_secs(2), &mut server).await;
            if closed.is_err() {
                server.abort();
                let _ = server.await;
            }
            assert!(
                matches!(result, Ok(Ok(Err(StorageError::Cancelled)))),
                "coalesced={coalesced}, body_prefix={send_body_prefix}: {result:?}"
            );
            assert!(
                closed.is_ok(),
                "source connection remained live after cancellation"
            );
            directory.close().unwrap();
        }
    }
}

#[tokio::test]
async fn unwrapped_reads_keep_signed_acceleration() {
    let signer = Arc::new(SigningProbe::default());
    let store = Store::new(Arc::new(InMemory::new())).with_signer(signer.clone());
    assert!(
        store
            .range_get(&Path::from("pack"), 0..LARGE_RANGE)
            .await
            .is_err()
    );
    assert_eq!(signer.0.load(Ordering::SeqCst), 1);
    let directory = tempfile::tempdir().unwrap();
    assert!(
        store
            .try_download_signed_ranges_to_path(
                &Path::from("source"),
                &directory.path().join("pack"),
                8,
                &"a".repeat(64),
                0..4,
                4..8,
                &CancellationToken::new(),
            )
            .await
            .is_err()
    );
    assert_eq!(signer.0.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn separate_signed_ranges_preserve_bytes_and_drain_on_sibling_failure() {
    for reject_sidecar in [false, true] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/object", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                connections.spawn(async move {
                    let mut request = Vec::new();
                    while !request.ends_with(b"\r\n\r\n") {
                        assert!(request.len() < 16 * 1024);
                        request.push(stream.read_u8().await.unwrap());
                    }
                    let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
                    let sidecar = request.contains("range: bytes=12-15\r\n");
                    assert!(sidecar || request.contains("range: bytes=4-7\r\n"));
                    if sidecar && reject_sidecar {
                        stream.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
                    } else {
                        let (range, body) = if sidecar { ("12-15", "wxyz") } else { ("4-7", "abcd") };
                        let body = if reject_sidecar { "ab" } else { body };
                        let response = format!("HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {range}/16\r\nContent-Length: 4\r\nConnection: close\r\n\r\n{body}");
                        // A sibling can reject before this response is written.
                        let _ = stream.write_all(response.as_bytes()).await;
                        if reject_sidecar {
                            let _ = stream.read_to_end(&mut Vec::new()).await;
                        }
                    }
                });
            }
            while let Some(result) = connections.join_next().await {
                result.unwrap();
            }
        });
        let store = Store::new(Arc::new(InMemory::new()))
            .with_signer(Arc::new(EndpointSigner(url.parse().unwrap())));
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("pack");
        let result = tokio::time::timeout(
            Duration::from_secs(2),
            store.try_download_signed_ranges_to_path(
                &Path::from("object"),
                &destination,
                16,
                &"a".repeat(64),
                4..8,
                12..16,
                &CancellationToken::new(),
            ),
        )
        .await;
        let mut server = server;
        let stopped = tokio::time::timeout(Duration::from_secs(2), &mut server).await;
        if stopped.is_err() {
            server.abort();
            let _ = server.await;
        }
        let result = result.unwrap();
        if reject_sidecar {
            assert!(matches!(result, Err(StorageError::Forbidden { .. })));
        } else {
            let (sidecars, hash) = result.unwrap().unwrap();
            assert_eq!(
                (sidecars, hash),
                (Bytes::from_static(b"wxyz"), blake3::hash(b"abcd"))
            );
            assert_eq!(std::fs::read(&destination).unwrap(), b"abcd");
        }
        assert!(stopped.is_ok(), "sibling source read did not stop");
        directory.close().unwrap();
    }
}

#[tokio::test]
async fn signed_capability_cannot_bypass_read_rejection_or_cancellation() {
    for cancelled in [false, true] {
        let signer = Arc::new(SigningProbe::default());
        let policy = Arc::new(ReadPolicy {
            reject: true,
            ..Default::default()
        });
        if cancelled {
            policy.cancel.cancel();
        }
        // Attaching a signer later must not re-enable bypass of an existing policy.
        let store = Store::new(Arc::new(InMemory::new()))
            .with_read_admission(policy.clone())
            .with_signer(signer.clone());
        let result = store.range_get(&Path::from("pack"), 0..LARGE_RANGE).await;
        let Err(StorageError::ReadRejected { source }) = result else {
            panic!("expected typed read rejection, got {result:?}");
        };
        if cancelled {
            assert!(matches!(
                source.downcast_ref::<StorageError>(),
                Some(StorageError::Cancelled)
            ));
        } else {
            assert!(source.downcast_ref::<Denied>().is_some());
        }
        assert_eq!(signer.0.load(Ordering::SeqCst), 0);
        assert_eq!(
            policy.requests.load(Ordering::SeqCst),
            u64::from(!cancelled)
        );
    }
}

#[tokio::test]
async fn signed_capability_keeps_large_range_admission_and_observation() {
    for admission in [false, true] {
        let signer = Arc::new(SigningProbe::default());
        let policy = Arc::new(ReadPolicy::default());
        let mut store = Store::new(Arc::new(InMemory::new())).with_signer(signer.clone());
        if admission {
            store = store.with_read_admission(policy.clone());
        } else {
            store = store.with_storage_observer(policy.clone());
        }
        let path = Path::from("pack");
        let data = Bytes::from(vec![0x5a; LARGE_RANGE as usize]);
        store.put(&path, data.clone()).await.unwrap();
        assert_eq!(
            store
                .clone()
                .range_get(&path, 0..LARGE_RANGE)
                .await
                .unwrap(),
            data
        );
        assert_eq!(signer.0.load(Ordering::SeqCst), 0);
        if admission {
            assert_eq!(policy.requests.load(Ordering::SeqCst), 1);
            assert_eq!(policy.bytes.load(Ordering::SeqCst), LARGE_RANGE);
        } else {
            assert_eq!(policy.observed_ranges.load(Ordering::SeqCst), 1);
            assert_eq!(policy.observed_bytes.load(Ordering::SeqCst), LARGE_RANGE);
        }
    }
}

#[tokio::test]
async fn signed_file_acceleration_declines_wrapped_reads_without_creating_files() {
    for admission in [false, true] {
        let signer = Arc::new(SigningProbe::default());
        let policy = Arc::new(ReadPolicy::default());
        let mut store = Store::new(Arc::new(InMemory::new()));
        if admission {
            store = store.with_read_admission(policy);
        } else {
            store = store.with_storage_observer(policy);
        }
        let store = store.with_signer(signer.clone());
        let directory = tempfile::tempdir().unwrap();
        let destination = directory.path().join("pack");
        let result = store
            .try_download_signed_ranges_to_path(
                &Path::from("source"),
                &destination,
                8,
                &"a".repeat(64),
                0..4,
                4..8,
                &CancellationToken::new(),
            )
            .await
            .unwrap();
        assert!(result.is_none());
        assert!(!destination.exists());
        assert_eq!(signer.0.load(Ordering::SeqCst), 0);
        // Presigning remains available to explicit callers; only direct reads decline.
        assert!(
            store
                .signed_url(&Path::from("source"), Duration::from_secs(1))
                .await
                .is_err()
        );
        assert_eq!(signer.0.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
#[ignore = "requires CRAB_E2E_FLAT_BUCKET and ambient writable S3 credentials"]
async fn live_signed_reads_preserve_admission_and_verified_bytes() {
    let bucket = std::env::var("CRAB_E2E_FLAT_BUCKET").expect("explicit scratch bucket required");
    let store = crate::build_static_env_store(&bucket, crate::StorageProviderKind::S3)
        .expect("build test store");
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = Path::from(format!("signed-read-qualification/{nonce}/payload"));
    let data = Bytes::from(vec![0x5a; LARGE_RANGE as usize + 4]);
    assert!(store.create_strict(&path, data.clone()).await.is_ok());

    let directory = tempfile::tempdir().unwrap();
    let destination = directory.path().join("pack");
    let result = store
        .try_download_signed_ranges_to_path(
            &path,
            &destination,
            data.len() as u64,
            blake3::hash(&data).to_hex().as_str(),
            0..LARGE_RANGE,
            LARGE_RANGE..LARGE_RANGE + 4,
            &CancellationToken::new(),
        )
        .await;
    assert!(result.is_ok(), "unwrapped signed download failed");
    let (sidecars, hash) = result.unwrap().expect("S3 signed download available");
    assert_eq!(sidecars, data.slice(LARGE_RANGE as usize..));
    assert_eq!(hash, blake3::hash(&data[..LARGE_RANGE as usize]));
    assert_eq!(
        std::fs::read(&destination).unwrap(),
        data[..LARGE_RANGE as usize]
    );

    let rejected = Arc::new(ReadPolicy {
        reject: true,
        ..Default::default()
    });
    let result = store
        .clone()
        .with_read_admission(rejected.clone())
        .range_get(&path, 0..LARGE_RANGE)
        .await;
    assert!(matches!(result, Err(StorageError::ReadRejected { .. })));
    assert_eq!(rejected.requests.load(Ordering::SeqCst), 1);
    assert_eq!(rejected.bytes.load(Ordering::SeqCst), 0);

    let accepted = Arc::new(ReadPolicy::default());
    let result = store
        .clone()
        .with_read_admission(accepted.clone())
        .with_storage_observer(accepted.clone())
        .range_get(&path, 0..LARGE_RANGE)
        .await;
    assert!(result.is_ok(), "admitted provider range failed");
    assert_eq!(result.unwrap(), data.slice(..LARGE_RANGE as usize));
    assert_eq!(accepted.requests.load(Ordering::SeqCst), 1);
    assert_eq!(accepted.bytes.load(Ordering::SeqCst), LARGE_RANGE);
    assert_eq!(accepted.observed_ranges.load(Ordering::SeqCst), 1);
    assert_eq!(accepted.observed_bytes.load(Ordering::SeqCst), LARGE_RANGE);
    assert!(store.delete(&path).await.is_ok());
}
