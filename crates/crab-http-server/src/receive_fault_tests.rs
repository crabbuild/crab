use super::*;
use axum::body::Body;
use futures_util::stream::BoxStream;
use http_body_util::BodyExt;
use object_store::{
    CopyOptions, GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
    PutMultipartOptions, PutOptions, PutPayload, PutResult, path::Path,
};
use tower::ServiceExt;

#[derive(Clone, Copy, Debug)]
enum Fault {
    LostMarkerReply,
    RejectedMarker,
    CancelAfterHead,
    CancelAfterMarker,
}

#[derive(Debug)]
struct FaultStore {
    inner: Arc<dyn ObjectStore>,
    marker_prefix: String,
    head_path: String,
    cancel: CancellationToken,
    fault: Fault,
}

impl std::fmt::Display for FaultStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("receive-fault-store")
    }
}

fn disconnected() -> object_store::Error {
    object_store::Error::Generic {
        store: "receive-fault",
        source: Box::new(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "lost reply",
        )),
    }
}

#[async_trait::async_trait]
impl ObjectStore for FaultStore {
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        options: PutOptions,
    ) -> object_store::Result<PutResult> {
        let marker = location.as_ref().starts_with(&self.marker_prefix);
        if marker && matches!(self.fault, Fault::RejectedMarker) {
            return Err(disconnected());
        }
        let result = self.inner.put_opts(location, payload, options).await?;
        if marker && matches!(self.fault, Fault::LostMarkerReply) {
            return Err(disconnected());
        }
        if (marker && matches!(self.fault, Fault::CancelAfterMarker))
            || (location.as_ref() == self.head_path && matches!(self.fault, Fault::CancelAfterHead))
        {
            self.cancel.cancel();
        }
        Ok(result)
    }

    async fn get_opts(
        &self,
        location: &Path,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        self.inner.get_opts(location, options).await
    }
    async fn put_multipart_opts(
        &self,
        location: &Path,
        options: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        self.inner.put_multipart_opts(location, options).await
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

async fn body() -> (Vec<u8>, String) {
    use super::receive_tests::{git, success};
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path();
    success(
        path,
        &["init", "--initial-branch=main", "--object-format=sha1", "."],
    )
    .await;
    std::fs::write(path.join("README.md"), "fault qualification\n").unwrap();
    success(path, &["add", "README.md"]).await;
    success(path, &["commit", "-m", "fault qualification"]).await;
    let oid = success(path, &["rev-parse", "HEAD"]).await;
    let pack = git(path, &["pack-objects", "--stdout", "--all"]).await;
    assert!(pack.status.success());
    let command = format!(
        "{} {oid} refs/heads/main\0report-status atomic\n",
        "0".repeat(40)
    );
    let mut body = format!("{:04x}{command}0000", command.len() + 4).into_bytes();
    body.extend_from_slice(&pack.stdout);
    // The HTTP publisher sees only the wire request, never the client's Git files.
    directory.close().unwrap();
    (body, oid)
}

const FAULTS: [Fault; 4] = [
    Fault::LostMarkerReply,
    Fault::RejectedMarker,
    Fault::CancelAfterHead,
    Fault::CancelAfterMarker,
];

#[tokio::test(flavor = "multi_thread")]
async fn receive_faults_preserve_exact_commit_outcomes_and_repair_on_restart() {
    let (body, oid) = body().await;
    for fault in FAULTS {
        exercise(maintenance_tests::fixture().await, fault, &body, &oid).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires fresh isolated RustFS prefixes and environment credentials"]
async fn receive_faults_rustfs() {
    let bucket = std::env::var("QUALIFICATION_BUCKET").unwrap();
    let prefix = std::env::var("QUALIFICATION_PREFIX").unwrap();
    assert!(prefix.starts_with("qualification/http-receive-"));
    let store = build_static_env_store(&bucket, StorageProviderKind::S3).unwrap();
    let domain = format!("{prefix}/draining-writer");
    let writer = crab_coordination::GcFenceLease::acquire_writer(
        store.inner(),
        &domain,
        Duration::from_secs(3),
    )
    .await
    .unwrap();
    let cancel = CancellationToken::new();
    let heartbeat =
        crab_coordination::GcFenceHeartbeat::spawn(&writer, cancel.clone(), Duration::from_secs(1));
    cancel.cancel();
    tokio::time::sleep(Duration::from_secs(4)).await;
    let other = crab_coordination::GcFenceLease::acquire_writer(
        store.inner(),
        &domain,
        Duration::from_secs(30),
    )
    .await
    .unwrap();
    heartbeat.stop().await;
    writer.release().await.unwrap();
    other.release().await.unwrap();
    let sweep = crab_coordination::GcFenceLease::acquire_sweep(
        store.inner(),
        &domain,
        Duration::from_secs(30),
    )
    .await
    .unwrap();
    sweep.release().await.unwrap();
    let (body, oid) = body().await;
    for (index, fault) in FAULTS.into_iter().enumerate() {
        let mut server = maintenance_tests::fixture().await;
        let repo = Arc::get_mut(&mut server)
            .unwrap()
            .repositories
            .get_mut(&("team".into(), "repo".into()))
            .unwrap();
        let prefix = format!("{prefix}/{index}");
        repo.layout = StoreLayout::new(store.clone(), prefix.clone());
        repo.store = store.clone();
        repo.config.bucket = bucket.clone();
        repo.config.prefix = prefix.clone();
        repo.identity = RepositoryIdentity::new(format!("s3:{bucket}"), prefix, 1).unwrap();
        crab_write::initialize::initialize_repository(&repo.store, &repo.layout, "refs/heads/main")
            .await
            .unwrap();
        exercise(server, fault, &body, &oid).await;
    }
}

async fn exercise(mut server: Arc<Server>, fault: Fault, body: &[u8], oid: &str) {
    let cancel = server.cancellation.clone();
    let repo = Arc::get_mut(&mut server)
        .unwrap()
        .repositories
        .get_mut(&("team".into(), "repo".into()))
        .unwrap();
    let origin = repo.store.clone();
    let origin_layout = repo.layout.clone();
    let config = repo.config.clone();
    let identity = repo.identity.clone();
    let faulty = Store::with_retry(
        Arc::new(FaultStore {
            inner: Arc::clone(origin.inner()),
            marker_prefix: format!("{}/", repo.layout.ref_journal_active_prefix()),
            head_path: repo
                .layout
                .ref_journal_head_path(&crab_metadata::ref_journal::ref_name_hash(
                    "refs/heads/main",
                ))
                .to_string(),
            cancel,
            fault,
        }),
        crab_storage::RetryPolicy {
            max_attempts: 1,
            base: Duration::ZERO,
            cap: Duration::ZERO,
        },
    );
    repo.store = faulty.clone();
    repo.layout = StoreLayout::new(faulty, repo.config.prefix.clone());
    let response = router(Arc::clone(&server))
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/git/team/repo.git/git-receive-pack")
                .header("host", "localhost:8788")
                .header("content-type", "application/x-git-receive-pack-request")
                .body(Body::from(body.to_vec()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let response = response.into_body().collect().await.unwrap().to_bytes();
    if matches!(fault, Fault::LostMarkerReply) {
        assert_eq!(status, StatusCode::OK);
        assert!(String::from_utf8_lossy(&response).contains("ok refs/heads/main"));
    } else {
        assert!(!status.is_success(), "{fault:?}");
    }
    assert!(!String::from_utf8_lossy(&response).contains("ng refs/heads/main"));
    server.cancellation.cancel();
    server.receives.close();
    server.receives.wait().await;
    server.finish_maintenance().await.unwrap();
    server.runtime.shutdown().await;
    let snapshot = crab_metadata::manifest_store::read_repository_snapshot(&origin, &origin_layout)
        .await
        .unwrap();
    let committed = matches!(fault, Fault::LostMarkerReply | Fault::CancelAfterMarker);
    assert_eq!(
        snapshot
            .journal
            .refs
            .get("refs/heads/main")
            .map(String::as_str),
        committed.then_some(oid),
        "{fault:?}"
    );
    for head in crab_metadata::ref_journal::list_ref_heads(&origin, &origin_layout)
        .await
        .unwrap()
    {
        // An attempted marker with no conclusive readback retains recovery
        // evidence. A new lease holder may replace it on an explicit retry.
        assert_eq!(
            head.head.prepared_transaction.is_some(),
            matches!(fault, Fault::RejectedMarker),
            "{fault:?}"
        );
    }
    for domain in [origin_layout.global_prefix(), origin_layout.repo_prefix()] {
        let sweep = crab_coordination::GcFenceLease::acquire_sweep(
            origin.inner(),
            domain,
            Duration::from_secs(30),
        )
        .await
        .unwrap();
        sweep.release().await.unwrap();
    }
    let mut restarted = maintenance_tests::fixture().await;
    let repo = Arc::get_mut(&mut restarted)
        .unwrap()
        .repositories
        .get_mut(&("team".into(), "repo".into()))
        .unwrap();
    repo.store = origin.clone();
    repo.layout = origin_layout.clone();
    repo.config = config;
    repo.identity = identity;
    let response = router(Arc::clone(&restarted))
        .oneshot(
            Request::builder()
                .uri("/api/repos/team/repo/refs")
                .header("host", "localhost:8788")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK, "{fault:?}");
    let value: serde_json::Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(
        value["refs"].as_array().unwrap().len(),
        usize::from(committed),
        "{fault:?}"
    );
    if committed {
        assert_eq!(value["refs"][0]["oid"], oid);
    }
    if !committed {
        let retry = router(Arc::clone(&restarted))
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/git/team/repo.git/git-receive-pack")
                    .header("host", "localhost:8788")
                    .header("content-type", "application/x-git-receive-pack-request")
                    .body(Body::from(body.to_vec()))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(retry.status(), StatusCode::OK, "retry after {fault:?}");
        let snapshot =
            crab_metadata::manifest_store::read_repository_snapshot(&origin, &origin_layout)
                .await
                .unwrap();
        assert_eq!(
            snapshot
                .journal
                .refs
                .get("refs/heads/main")
                .map(String::as_str),
            Some(oid)
        );
        for head in crab_metadata::ref_journal::list_ref_heads(&origin, &origin_layout)
            .await
            .unwrap()
        {
            assert!(head.head.prepared_transaction.is_none());
        }
    }
    let blob = router(Arc::clone(&restarted))
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/api/repos/team/repo/blob?rev={oid}&path=README.md"
                ))
                .header("host", "localhost:8788")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(blob.status(), StatusCode::OK, "{fault:?}");
    assert_eq!(
        blob.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .as_ref(),
        b"fault qualification\n"
    );
    restarted.cancellation.cancel();
    restarted.finish_maintenance().await.unwrap();
    restarted.runtime.shutdown().await;
}
