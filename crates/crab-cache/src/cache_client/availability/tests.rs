use std::net::SocketAddr;
use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
use std::time::Duration;

use futures_util::StreamExt as _;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::*;
use crate::CacheServiceAuth;
use crate::cache_client::{CacheClient, build_cache_service_http_client};

struct Endpoint {
    address: SocketAddr,
    requests: Arc<AtomicUsize>,
    status: Arc<AtomicU16>,
    task: tokio::task::JoinHandle<()>,
}

impl Endpoint {
    async fn start(status: u16, stalled_headers: bool, stalled_body: bool) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let status = Arc::new(AtomicU16::new(status));
        let request_count = Arc::clone(&requests);
        let response_status = Arc::clone(&status);
        let task = tokio::spawn(async move {
            let mut workers = tokio::task::JoinSet::new();
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let requests = Arc::clone(&request_count);
                let status = Arc::clone(&response_status);
                workers.spawn(async move {
                    let mut request = [0; 4096];
                    if socket.read(&mut request).await.unwrap_or(0) == 0 {
                        return;
                    }
                    requests.fetch_add(1, Ordering::SeqCst);
                    if stalled_headers {
                        std::future::pending::<()>().await;
                    }
                    let code = status.load(Ordering::SeqCst);
                    let length = if stalled_body { 100 } else { 7 };
                    let headers = format!("HTTP/1.1 {code} test\r\nContent-Length: {length}\r\nConnection: close\r\n\r\nhealthy");
                    let _ = socket.write_all(headers.as_bytes()).await;
                    if stalled_body {
                        std::future::pending::<()>().await;
                    }
                });
                while workers.try_join_next().is_some() {}
            }
        });
        Self {
            address,
            requests,
            status,
            task,
        }
    }

    fn client(&self) -> CacheClient {
        CacheClient::new(
            &format!("http://{}", self.address),
            &CacheServiceAuth::None,
            None,
            None,
            None,
        )
        .unwrap()
    }

    fn count(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }

    async fn stop(self) {
        self.task.abort();
        let _ = self.task.await;
    }
}

fn advance_recovery(client: &CacheClient) {
    // Pure state tests below check the real cooldown. Network tests advance
    // that clock edge explicitly instead of sleeping for thirty seconds.
    client.availability.state.lock().unwrap().retry_at = Some(Instant::now());
}

#[test]
fn cooldown_has_one_probe_and_cancelled_probe_can_retry() {
    let availability = Arc::new(Availability::default());
    let now = Instant::now();
    availability.begin(now).unwrap().failed();
    assert!(availability.begin(now).is_err());
    assert!(availability.begin(now + REQUEST_TIMEOUT / 2).is_err());
    let future = now + REQUEST_TIMEOUT + Duration::from_secs(1);
    let probe = availability.begin(future).unwrap();
    assert!(availability.begin(future).is_err());
    drop(probe);
    assert!(availability.begin(Instant::now()).is_err());
    availability.begin(future).unwrap().succeeded();
    assert!(availability.begin(Instant::now()).is_ok());
}

#[test]
fn stale_inflight_completions_cannot_clear_or_extend_failure() {
    let availability = Arc::new(Availability::default());
    let now = Instant::now();
    let mut failed = availability.begin(now).unwrap();
    let mut old_success = availability.begin(now).unwrap();
    let mut old_failure = availability.begin(now).unwrap();
    failed.failed();
    let retry_at = availability.state.lock().unwrap().retry_at;
    old_success.succeeded();
    old_failure.failed();
    assert_eq!(availability.state.lock().unwrap().retry_at, retry_at);
    assert!(availability.begin(now).is_err());
}

#[tokio::test]
async fn transient_failure_suppresses_every_client_entry_point_across_clones() {
    let endpoint = Endpoint::start(503, false, false).await;
    let client = endpoint.client();
    assert!(client.get("object").await.is_err());
    let clone = client.clone();
    assert!(clone.get_bounded("other", 1024).await.is_err());
    assert!(clone.get_stream("other").await.is_err());
    assert!(clone.head("other").await.is_err());
    assert!(clone.get_range("other", 0..1).await.is_err());
    assert!(clone.put("other", bytes::Bytes::new()).await.is_err());
    assert!(clone.dedup_query("repo", &[[0; 32]]).await.is_err());
    assert!(clone.capabilities().await.is_err());
    assert!(!clone.is_healthy().await);
    let directory = tempfile::tempdir().unwrap();
    assert!(
        clone
            .download_to_path("other", &directory.path().join("object"))
            .await
            .is_err()
    );
    assert_eq!(endpoint.count(), 1);
    // Separately constructed clients must not share state across credentials,
    // tenants, or configuration changes merely because their URL matches.
    assert!(endpoint.client().get("other").await.is_err());
    assert_eq!(endpoint.count(), 2);
    endpoint.stop().await;
}

#[tokio::test]
async fn request_local_errors_do_not_disable_the_endpoint() {
    for status in [400, 401, 403, 404, 416, 429, 500, 502, 503, 504, 507] {
        let endpoint = Endpoint::start(status, false, false).await;
        let client = endpoint.client();
        assert!(client.get("first").await.is_err());
        assert!(client.get("second").await.is_err());
        let expected = if matches!(status, 429 | 502 | 503) {
            1
        } else {
            2
        };
        assert_eq!(endpoint.count(), expected, "HTTP {status}");
        endpoint.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn response_body_owns_recovery_and_drop_does_not_strand_it() {
    let endpoint = Endpoint::start(503, false, false).await;
    let client = endpoint.client();
    assert!(client.get("object").await.is_err());
    endpoint.status.store(200, Ordering::SeqCst);
    advance_recovery(&client);
    let response = client.get_stream("object").await.unwrap().unwrap();
    let mut contenders = tokio::task::JoinSet::new();
    for _ in 0..32 {
        let clone = client.clone();
        contenders.spawn(async move { clone.get("other").await });
    }
    while let Some(result) = contenders.join_next().await {
        assert!(result.unwrap().is_err());
    }
    assert_eq!(endpoint.count(), 2);
    drop(response);
    assert!(client.get("other").await.is_err());
    advance_recovery(&client);
    let mut response = client
        .get_stream("object")
        .await
        .unwrap()
        .unwrap()
        .into_stream()
        .boxed();
    while let Some(chunk) = response.next().await {
        assert!(chunk.is_ok());
    }
    assert_eq!(client.get("other").await.unwrap(), b"healthy"[..]);
    endpoint.stop().await;
}

#[tokio::test]
async fn header_and_body_timeouts_preserve_sources_and_suppress_followups() {
    for stalled_body in [false, true] {
        let endpoint = Endpoint::start(200, !stalled_body, stalled_body).await;
        let mut client = endpoint.client();
        client.client =
            build_cache_service_http_client(Duration::from_millis(100), None, None, None).unwrap();
        let error = if stalled_body {
            let response = client.get_stream("object").await.unwrap().unwrap();
            let results: Vec<_> = response.into_stream().collect().await;
            results.into_iter().find_map(Result::err).unwrap()
        } else {
            client.get("object").await.unwrap_err()
        };
        assert!(matches!(error, CacheError::ServiceRequestTimeout { .. }));
        assert!(std::error::Error::source(&error).is_some());
        assert!(client.get("other").await.is_err());
        assert_eq!(endpoint.count(), 1);
        endpoint.stop().await;
    }
}

#[tokio::test]
async fn cancelled_header_probe_releases_admission_for_later_recovery() {
    let endpoint = Endpoint::start(200, true, false).await;
    let mut client = endpoint.client();
    client.client =
        build_cache_service_http_client(Duration::from_millis(100), None, None, None).unwrap();
    assert!(client.get("first").await.is_err());
    client.client = build_cache_service_http_client(REQUEST_TIMEOUT, None, None, None).unwrap();
    advance_recovery(&client);
    let clone = client.clone();
    let worker = tokio::spawn(async move { clone.get("probe").await });
    tokio::time::timeout(Duration::from_secs(2), async {
        while endpoint.count() < 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    worker.abort();
    assert!(worker.await.unwrap_err().is_cancelled());
    assert!(client.get("still-deferred").await.is_err());
    assert_eq!(endpoint.count(), 2);
    advance_recovery(&client);
    client.client =
        build_cache_service_http_client(Duration::from_millis(100), None, None, None).unwrap();
    assert!(client.get("next-probe").await.is_err());
    assert_eq!(endpoint.count(), 3);
    endpoint.stop().await;
}
