use super::*;
use axum::body::Body;
use crab_coordination::{
    CoordinationError, GIT_GENERATION_OWNER_RESOURCE, GIT_MANIFEST_RESOURCE, GcFenceLease,
    PushLock, internal_lock_path,
};
use crab_metadata::{manifest_store, ref_journal::RefJournalEdit};
use crab_write::WriteError;
use http_body_util::BodyExt;
use tower::ServiceExt;

const TTL: Duration = Duration::from_secs(60);

struct UnavailableRoundTrip;

impl crab_cell_runtime::PeerRoundTrip for UnavailableRoundTrip {
    fn send(
        &self,
        _target: crab_cell_runtime::CellTarget,
        _request: Vec<u8>,
        _remaining_ms: u32,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = crab_cell_runtime::Result<Vec<u8>>> + Send + 'static>,
    > {
        Box::pin(async { Err(crab_cell_runtime::Error::CellNotActive) })
    }
}

async fn fixture_without_cells() -> Arc<Server> {
    let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
    let admission_store = store.clone();
    let layout = StoreLayout::new(store.clone(), "maintenance".into());
    crab_write::initialize::initialize_repository(&store, &layout, "refs/heads/main")
        .await
        .unwrap();
    Arc::new(Server {
        repositories: BTreeMap::from([(
            ("team".into(), "repo".into()),
            Repository {
                id: uuid::Uuid::from_bytes([1; 16]),
                config: RepositoryConfig {
                    owner: "team".into(),
                    name: "repo".into(),
                    bucket: "memory".into(),
                    prefix: "maintenance".into(),
                    default_branch: "main".into(),
                    description: String::new(),
                    members: vec![],
                    protected_branches: vec![],
                },
                identity: RepositoryIdentity::new("memory", "maintenance", 1).unwrap(),
                store,
                layout,
                pinned: Mutex::new(None),
                maintenance: Mutex::new(None),
            },
        )])
        .into(),
        runtime: Arc::new(RemoteGitRuntime::default()),
        cell_runtime: start_test_cell_runtime(),
        repository_cells: None,
        peer_receiver: None,
        follower_store: None,
        node_log_transport: None,
        options: RepositoryOptions::default(),
        cursor_key: [0; 32],
        admission: Semaphore::new(16),
        transfer_admission: crate::transfer_admission::TransferAdmission::new(
            admission_store,
            "test/.crab/http-server/v1/admission".into(),
            4,
        ),
        local_staging: crate::local_disk::LocalStaging::for_test(),
        app_admission: Semaphore::new(8),
        maintenance_admission: Arc::new(Semaphore::new(2)),
        cancellation: CancellationToken::new(),
        receives: tokio_util::task::TaskTracker::new(),
        auth: None,
        catalog: None,
        catalog_healthy: AtomicBool::new(false),
        node_healthy: AtomicBool::new(false),
        scheduler_status: crate::cells::SchedulerStatus::new(crate::cells::unix_now_ms().unwrap())
            .unwrap(),
        cell_capacity: super::test_cell_capacity_report(),
        metrics: crate::metrics::Metrics::new().unwrap(),
    })
}

pub(super) async fn fixture() -> Arc<Server> {
    static CELL_DIRS: std::sync::OnceLock<std::sync::Mutex<Vec<tempfile::TempDir>>> =
        std::sync::OnceLock::new();

    let mut server = fixture_without_cells().await;
    server.cell_runtime.shutdown().await.unwrap();
    let repository = server
        .repositories
        .get(&("team".into(), "repo".into()))
        .unwrap();
    let identity = crab_cell_runtime::ApplicationIdentity::new(
        crab_cell_runtime::TenantId::from_bytes([31; 16]),
        crab_cell_runtime::ApplicationId::from_bytes([32; 16]),
    );
    let layout = crab_storage::CellStorageLayout::new(
        repository.store.clone(),
        object_store::path::Path::from("maintenance-test-cells"),
        *identity.application().as_bytes(),
    );
    let registry = Arc::new(crate::cells::compiled_registry().unwrap());
    crate::cells::bootstrap_release_at(
        &layout,
        identity,
        &registry,
        &format!("sha256:{}", "b".repeat(64)),
    )
    .await
    .unwrap();
    let cell_dir = tempfile::TempDir::new().unwrap();
    crate::cells::initialize_repository_at(
        &layout,
        identity,
        &registry,
        cell_dir.path(),
        32 * 1024 * 1024 * 1024,
        "https://initializer.test:8081".into(),
        repository.id,
    )
    .await
    .unwrap();
    let cell_session = crab_cell_runtime::SessionId::from_bytes([33; 16]);
    let cell_runtime = crab_cell_runtime::CellRuntime::new(
        crab_cell_runtime::SqlWorkerPool::new(1, 16).unwrap(),
        16 * 1024 * 1024,
        cell_session,
    )
    .unwrap();
    let router = crate::cells::RepositoryCellRouter::new(
        identity,
        layout.clone(),
        Arc::clone(&registry),
        cell_runtime.clone(),
        crate::cells::RepositoryCellPeer::new(
            crab_cell_runtime::NodeDirectory::new(
                layout,
                crab_cell_runtime::Digest::from_bytes([34; 32]),
                crab_cell_runtime::Digest::from_bytes([35; 32]),
                registry.release_digest(),
            ),
            Arc::new(crab_cell_runtime::PeerSigner::new(
                cell_session,
                registry.release_digest(),
                ed25519_dalek::SigningKey::from_bytes(&[36; 32]),
            )),
            Arc::new(UnavailableRoundTrip),
            crab_cell_runtime::Owner {
                session: cell_session,
                endpoint: "https://server.test:8081".into(),
            },
        ),
        cell_dir.path().to_path_buf(),
    )
    .unwrap();
    CELL_DIRS
        .get_or_init(|| std::sync::Mutex::new(Vec::new()))
        .lock()
        .unwrap()
        .push(cell_dir);
    let mutable = Arc::get_mut(&mut server).unwrap();
    mutable.cell_runtime = cell_runtime;
    mutable.repository_cells = Some(router);
    server
}

fn repository(server: &Server) -> Arc<Repository> {
    server
        .repositories
        .get(&("team".into(), "repo".into()))
        .unwrap()
}

pub(super) async fn commit_without_proof(repo: &Repository) -> PushLock {
    let lease = PushLock::acquire_ref(
        repo.store.inner(),
        repo.layout.repo_prefix(),
        "refs/heads/main",
        TTL,
    )
    .await
    .unwrap();
    let snapshot = manifest_store::read_repository_snapshot(&repo.store, &repo.layout)
        .await
        .unwrap();
    crab_write::journal::commit_edits(
        &repo.store,
        &repo.layout,
        &snapshot,
        vec![RefJournalEdit {
            ref_name: "refs/heads/main".into(),
            old_oid: None,
            new_oid: Some("a".repeat(40)),
            peeled_oid: None,
            lock_holder: Some(lease.holder().to_owned()),
            visibility_evidence_hash: None,
        }],
        None,
        vec![],
        vec![],
        crab_write::journal::CommitOptions::new(TTL, &tokio_util::sync::CancellationToken::new()),
    )
    .await
    .unwrap();
    lease
}

async fn assert_released(repo: &Repository) {
    let owner = PushLock::acquire_internal(
        repo.store.inner(),
        repo.layout.repo_prefix(),
        GIT_GENERATION_OWNER_RESOURCE,
        TTL,
    )
    .await
    .unwrap();
    owner.release().await.unwrap();
    for domain in [repo.layout.global_prefix(), repo.layout.repo_prefix()] {
        let sweep = GcFenceLease::acquire_sweep(repo.store.inner(), domain, TTL)
            .await
            .unwrap();
        sweep.release().await.unwrap();
    }
}

async fn close(server: &Server) {
    server.cancellation.cancel();
    server.finish_maintenance().await.unwrap();
    server.shutdown_runtimes().await.unwrap();
}

fn enable_catalog_readiness(server: &mut Arc<Server>) {
    let store = repository(server).store.clone();
    let server = Arc::get_mut(server).unwrap();
    server.catalog = Some(CatalogStore::new(crate::storage_root::StorageRoot::memory(
        store.clone(),
        "catalog",
    )));
    let identity = crab_cell_runtime::ApplicationIdentity::new(
        crab_cell_runtime::TenantId::from_bytes([1; 16]),
        crab_cell_runtime::ApplicationId::from_bytes([2; 16]),
    );
    let layout = crab_storage::CellStorageLayout::new(
        store,
        object_store::path::Path::from("catalog"),
        *identity.application().as_bytes(),
    );
    let registry = Arc::new(crate::cells::compiled_registry().unwrap());
    let resolver =
        crate::peer::LocalCellResolver::new(layout.clone(), identity, server.cell_runtime.clone());
    let releases = crab_cell_runtime::ReleaseStore::new(layout.clone(), identity).unwrap();
    server.peer_receiver = Some(crate::peer::PeerReceiver::new(
        crab_cell_runtime::NodeId::from_bytes([9; 16]),
        crab_cell_runtime::SessionId::from_bytes([9; 16]),
        crab_cell_runtime::NodeDirectory::new(
            layout,
            crab_cell_runtime::Digest::from_bytes([3; 32]),
            crab_cell_runtime::Digest::from_bytes([4; 32]),
            registry.release_digest(),
        ),
        registry,
        releases,
        resolver,
        Arc::new(UnavailableRoundTrip),
    ));
    server.catalog_healthy.store(true, Ordering::Release);
    server.node_healthy.store(true, Ordering::Release);
    server
        .scheduler_status
        .mark_completed(crate::cells::unix_now_ms().unwrap());
}

#[tokio::test]
async fn readiness_opens_every_repository_before_admitting_traffic() {
    let mut server = fixture().await;
    enable_catalog_readiness(&mut server);

    let response = management_router(Arc::clone(&server))
        .oneshot(
            Request::builder()
                .uri("/readyz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    close(&server).await;
}

#[tokio::test]
async fn readiness_rejects_a_server_that_is_draining() {
    let mut server = fixture().await;
    enable_catalog_readiness(&mut server);
    server.cancellation.cancel();

    let response = management_router(Arc::clone(&server))
        .oneshot(
            Request::builder()
                .uri("/readyz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok()),
        Some("5")
    );
    close(&server).await;
}

#[tokio::test]
async fn readiness_rejects_a_draining_cell_runtime() {
    let mut server = fixture().await;
    enable_catalog_readiness(&mut server);
    server.shutdown_runtimes().await.unwrap();

    let response = management_router(Arc::clone(&server))
        .oneshot(
            Request::builder()
                .uri("/readyz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok()),
        Some("5")
    );
}

#[tokio::test]
async fn readiness_rejects_a_stalled_cell_scheduler() {
    let mut server = fixture().await;
    enable_catalog_readiness(&mut server);
    let now_ms = crate::cells::unix_now_ms().unwrap();
    let stalled = crate::cells::SchedulerStatus::new(now_ms - 15_000).unwrap();
    stalled.mark_completed(now_ms - 15_000);
    Arc::get_mut(&mut server).unwrap().scheduler_status = stalled;

    let response = management_router(Arc::clone(&server))
        .oneshot(
            Request::builder()
                .uri("/readyz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    close(&server).await;
}

#[tokio::test]
async fn readiness_requires_the_first_cell_scheduler_cycle() {
    let mut server = fixture().await;
    enable_catalog_readiness(&mut server);
    Arc::get_mut(&mut server).unwrap().scheduler_status =
        crate::cells::SchedulerStatus::new(crate::cells::unix_now_ms().unwrap()).unwrap();

    let response = management_router(Arc::clone(&server))
        .oneshot(
            Request::builder()
                .uri("/readyz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    close(&server).await;
}

#[tokio::test]
async fn readiness_rejects_a_repository_that_still_needs_indexing() {
    let mut server = fixture().await;
    enable_catalog_readiness(&mut server);
    let repo = repository(&server);
    let lease = commit_without_proof(&repo).await;

    let response = management_router(Arc::clone(&server))
        .oneshot(
            Request::builder()
                .uri("/readyz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        response
            .headers()
            .get("retry-after")
            .and_then(|value| value.to_str().ok()),
        Some("5")
    );
    assert_released(&repo).await;
    lease.release().await.unwrap();
    close(&server).await;
}

#[tokio::test]
async fn expired_browser_cache_observes_journal_and_reports_missing_proof_without_rollback() {
    let server = fixture().await;
    let repo = repository(&server);
    repo.open(&server, &CancellationToken::new()).await.unwrap();
    repo.pinned.lock().await.as_mut().unwrap().0 = Instant::now() - Duration::from_secs(3);
    let before = manifest_store::read_manifest(&repo.store, &repo.layout)
        .await
        .unwrap()
        .1;
    let lease = commit_without_proof(&repo).await;
    assert_eq!(
        before,
        manifest_store::read_manifest(&repo.store, &repo.layout)
            .await
            .unwrap()
            .1
    );

    let response = router(Arc::clone(&server))
        .oneshot(
            Request::builder()
                .uri("/api/repos/team/repo/refs")
                .header("host", "localhost:8788")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["error"]["code"], "indexing_failed");
    let after = manifest_store::read_repository_snapshot(&repo.store, &repo.layout)
        .await
        .unwrap();
    assert_eq!(after.manifest.refs["refs/heads/main"], "a".repeat(40));
    assert!(after.journal.transactions.is_empty());
    assert_released(&repo).await;
    lease.release().await.unwrap();
    close(&server).await;
}

#[tokio::test]
async fn another_generation_owner_keeps_publication_authority() {
    let server = fixture().await;
    let repo = repository(&server);
    let lease = commit_without_proof(&repo).await;
    let mut owner = PushLock::acquire_internal(
        repo.store.inner(),
        repo.layout.repo_prefix(),
        GIT_GENERATION_OWNER_RESOURCE,
        TTL,
    )
    .await
    .unwrap();
    let before = manifest_store::read_manifest(&repo.store, &repo.layout)
        .await
        .unwrap();
    let result = repo
        .open_current(&server, server.options, &CancellationToken::new())
        .await;
    assert!(matches!(
        result,
        Err(crate::Error::Remote(
            crab_remote_git::Error::RepositoryIndexing { .. }
        ))
    ));
    assert_eq!(
        before,
        manifest_store::read_manifest(&repo.store, &repo.layout)
            .await
            .unwrap()
    );
    owner.renew().await.unwrap();
    owner.release().await.unwrap();
    lease.release().await.unwrap();
    close(&server).await;
}

#[tokio::test]
async fn gc_sweep_blocks_publication_and_releases_preceding_leases() {
    for global in [true, false] {
        let server = fixture().await;
        let repo = repository(&server);
        let lease = commit_without_proof(&repo).await;
        let domain = if global {
            repo.layout.global_prefix()
        } else {
            repo.layout.repo_prefix()
        };
        let sweep = GcFenceLease::acquire_sweep(repo.store.inner(), domain, TTL)
            .await
            .unwrap();
        let before = manifest_store::read_manifest(&repo.store, &repo.layout)
            .await
            .unwrap();
        let result = repo
            .open_current(&server, server.options, &CancellationToken::new())
            .await;
        assert!(matches!(
            result,
            Err(crate::Error::Maintenance(WriteError::Coordination(
                CoordinationError::GcFenceHeld { .. }
            )))
        ));
        assert_eq!(
            before,
            manifest_store::read_manifest(&repo.store, &repo.layout)
                .await
                .unwrap()
        );
        sweep.renew().await.unwrap();
        sweep.release().await.unwrap();
        assert_released(&repo).await;
        lease.release().await.unwrap();
        close(&server).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn disconnected_reader_retains_publication_until_retry_or_shutdown_drains_it() {
    for shutdown in [false, true] {
        let server = fixture().await;
        let repo = repository(&server);
        let lease = commit_without_proof(&repo).await;
        let manifest = PushLock::acquire_internal(
            repo.store.inner(),
            repo.layout.repo_prefix(),
            GIT_MANIFEST_RESOURCE,
            TTL,
        )
        .await
        .unwrap();
        let cancel = CancellationToken::new();
        let request_server = Arc::clone(&server);
        let request_cancel = cancel.clone();
        let request = tokio::spawn(async move {
            repository(&request_server)
                .open_current(&request_server, request_server.options, &request_cancel)
                .await
        });
        let owner_path =
            internal_lock_path(repo.layout.repo_prefix(), GIT_GENERATION_OWNER_RESOURCE).unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            while repo.store.head(&owner_path.as_str().into()).await.is_err() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        cancel.cancel();
        assert!(matches!(
            request.await.unwrap(),
            Err(crate::Error::Remote(crab_remote_git::Error::Cancelled))
        ));
        assert!(
            repo.maintenance
                .lock()
                .await
                .as_ref()
                .is_some_and(|task| !task.is_finished())
        );
        assert_eq!(server.maintenance_admission.available_permits(), 1);
        if shutdown {
            tokio::time::timeout(Duration::from_secs(5), close(&server))
                .await
                .unwrap();
            manifest.release().await.unwrap();
        } else {
            manifest.release().await.unwrap();
            let result = repo
                .open_current(&server, server.options, &CancellationToken::new())
                .await;
            assert!(matches!(
                result,
                Err(crate::Error::Maintenance(
                    WriteError::VisibilityUnavailable { .. }
                ))
            ));
            close(&server).await;
        }
        assert_eq!(server.maintenance_admission.available_permits(), 2);
        assert!(repo.maintenance.lock().await.is_none());
        assert_released(&repo).await;
        lease.release().await.unwrap();
    }
}
