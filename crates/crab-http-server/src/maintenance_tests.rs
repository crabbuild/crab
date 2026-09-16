use super::*;
use axum::body::Body;
use crab_metadata::capsule_protocol::{
    Capsule, CapsuleGitPack, CapsuleRefEdit, CapsuleSection, CapsuleSectionKind,
    CapsuleTransaction, CapsuleVisibilityDelta,
};
use crab_metadata::git_visibility::GitVisibilityEdit;
use tower::ServiceExt;

use crate::test_git::history as git_history;

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
    crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
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
                integrity: crate::integrity::Status::default(),
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
async fn integrity_proof_is_reported_separately_from_readiness() {
    let server = fixture_without_cells().await;
    let management = management_router(Arc::clone(&server));
    let pending = management
        .clone()
        .oneshot(
            Request::builder()
                .uri("/integrityz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(pending.status(), StatusCode::ACCEPTED);
    let repository = server
        .repositories
        .get(&("team".into(), "repo".into()))
        .unwrap();
    crate::integrity::scrub(
        &repository,
        Arc::clone(&server.maintenance_admission),
        &server.cancellation,
    )
    .await
    .unwrap();

    let complete = management
        .clone()
        .oneshot(
            Request::builder()
                .uri("/integrityz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(complete.status(), StatusCode::OK);
    let body = http_body_util::BodyExt::collect(complete.into_body())
        .await
        .unwrap()
        .to_bytes();
    let report: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(report["status"], "complete");
    assert_eq!(report["repositories"][0]["proof"]["state"], "complete");
    assert!(
        report["repositories"][0]["proof"]["last_complete"]["state_digest"]
            .as_str()
            .is_some_and(|digest| digest.len() == 64)
    );
    let readiness = management
        .oneshot(
            Request::builder()
                .uri("/readyz")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(readiness.status(), StatusCode::SERVICE_UNAVAILABLE);
    close(&server).await;
}

fn capsule(
    transaction: &CapsuleTransaction,
    old: Option<String>,
    new: String,
    pack: Option<CapsuleGitPack>,
) -> Capsule {
    let visibility = CapsuleVisibilityDelta::new(BTreeMap::from([(
        "refs/heads/main".to_owned(),
        GitVisibilityEdit::from_replacement_objects(old, new.clone(), vec![new]),
    )]))
    .unwrap();
    Capsule::build(
        transaction,
        pack.into_iter().collect(),
        vec![CapsuleSection::new(
            CapsuleSectionKind::VisibilityDelta,
            visibility.encode().unwrap(),
        )],
    )
    .unwrap()
}

async fn publish_next(
    repo: &Repository,
    base: crab_write::capsule_protocol::RootSnapshot,
    old: Option<String>,
    new: String,
    pack: Option<CapsuleGitPack>,
) -> (crab_write::capsule_protocol::RootSnapshot, String) {
    let transaction = CapsuleTransaction::new(
        base.record().digest(),
        vec![CapsuleRefEdit::new(
            "refs/heads/main",
            old.clone(),
            Some(new.clone()),
            None,
        )],
    )
    .unwrap();
    let base = crab_write::capsule_protocol::publish(
        &repo.layout,
        base,
        &transaction,
        &capsule(&transaction, old, new.clone(), pack),
    )
    .await
    .unwrap();
    (base, new)
}

#[tokio::test]
async fn checkpoint_bounds_ref_frontier_and_next_push_starts_fresh() {
    let history = git_history(33);
    let server = fixture().await;
    let repo = repository(&server);
    let mut base = crab_write::capsule_protocol::open_root(&repo.layout)
        .await
        .unwrap();
    let mut old = None;
    for sequence in 0..32 {
        let result = publish_next(
            &repo,
            base,
            old,
            history.oids[sequence].clone(),
            (sequence == 0).then(|| history.pack.clone()),
        )
        .await;
        base = result.0;
        let new = result.1;
        old = Some(new);
    }

    crate::maintenance::run(
        repo.layout.clone(),
        Arc::clone(&server.maintenance_admission),
        CancellationToken::new(),
    )
    .await
    .unwrap();

    let checkpoint = crab_write::capsule_protocol::open_root(&repo.layout)
        .await
        .unwrap();
    assert!(checkpoint.record().root().checkpoint().is_some());
    assert_eq!(
        checkpoint
            .record()
            .root()
            .compacted_ref_transactions()
            .len(),
        1
    );
    let new = history.oids[32].clone();
    let transaction = CapsuleTransaction::new(
        checkpoint.record().digest(),
        vec![CapsuleRefEdit::new(
            "refs/heads/main",
            old.clone(),
            Some(new.clone()),
            None,
        )],
    )
    .unwrap();
    crab_write::capsule_protocol::publish(
        &repo.layout,
        checkpoint,
        &transaction,
        &capsule(&transaction, old, new, None),
    )
    .await
    .unwrap();

    let path =
        repo.layout
            .capsule_ref_head_path(&crab_metadata::capsule_protocol::capsule_ref_name_key(
                "refs/heads/main",
            ));
    let (body, _) = repo.store.get_with_etag(&path).await.unwrap();
    let head = crab_metadata::capsule_protocol::CapsuleRefHead::decode(&body).unwrap();
    assert_eq!(
        head.visible(&std::collections::BTreeSet::new())
            .frontier()
            .iter()
            .map(crab_metadata::capsule_protocol::CapsulePointer::capsule_count)
            .sum::<u32>(),
        1
    );
    close(&server).await;
}

#[tokio::test]
async fn lagging_checkpoint_preserves_concurrent_ref_suffix() {
    let history = git_history(35);
    let server = fixture().await;
    let repo = repository(&server);
    let mut base = crab_write::capsule_protocol::open_root(&repo.layout)
        .await
        .unwrap();
    let mut old = None;
    for sequence in 0..32 {
        let result = publish_next(
            &repo,
            base,
            old,
            history.oids[sequence].clone(),
            (sequence == 0).then(|| history.pack.clone()),
        )
        .await;
        base = result.0;
        old = Some(result.1);
    }

    let captured = crab_read::capsule_protocol::open_view(
        &repo.layout,
        crab_read::capsule_protocol::CapsuleReadLimits {
            max_capsule_bytes: 2 * 1024 * 1024 * 1024,
            max_frontier_bytes: 2 * 1024 * 1024 * 1024,
        },
    )
    .await
    .unwrap();
    let checkpoint = crab_metadata::capsule_protocol::Checkpoint::build_with_catalogs(
        captured.root().root().generation(),
        captured.root().digest(),
        captured.checkpoint_git_packs().unwrap(),
        captured.pointer_catalog().unwrap(),
        Some(
            crab_metadata::capsule_protocol::CapsuleVisibilitySnapshot::from_index(
                &captured.git_visibility_index().unwrap(),
            )
            .unwrap(),
        ),
    )
    .unwrap();

    for sequence in 32..34 {
        let result = publish_next(&repo, base, old, history.oids[sequence].clone(), None).await;
        base = result.0;
        old = Some(result.1);
    }
    base = crab_write::capsule_protocol::publish_ref_checkpoint(
        &repo.layout,
        captured.root_snapshot().clone(),
        &checkpoint,
        captured.refs().clone(),
        captured.peeled_refs().clone(),
        captured.visible_ref_transactions().clone(),
        captured.capsule_run_pointers().to_vec(),
    )
    .await
    .unwrap();

    let result = publish_next(&repo, base, old, history.oids[34].clone(), None).await;
    assert_eq!(result.1, history.oids[34]);
    let view = crab_read::capsule_protocol::open_view(
        &repo.layout,
        crab_read::capsule_protocol::CapsuleReadLimits {
            max_capsule_bytes: 2 * 1024 * 1024 * 1024,
            max_frontier_bytes: 2 * 1024 * 1024 * 1024,
        },
    )
    .await
    .unwrap();
    assert_eq!(view.refs().get("refs/heads/main"), Some(&history.oids[34]));
    assert_eq!(
        view.capsule_run_pointers()
            .iter()
            .map(crab_metadata::capsule_protocol::CapsulePointer::capsule_count)
            .sum::<u32>(),
        3
    );
    assert_eq!(view.ref_capsule_count("refs/heads/main"), 3);
    close(&server).await;
}

#[tokio::test]
async fn foreground_checkpoint_preserves_headroom_before_the_hard_bound() {
    let history = git_history(57);
    let server = fixture().await;
    let repo = repository(&server);
    let mut base = crab_write::capsule_protocol::open_root(&repo.layout)
        .await
        .unwrap();
    let mut old = None;
    for sequence in 0..crate::maintenance::FOREGROUND_CAPSULE_THRESHOLD as usize {
        let result = publish_next(
            &repo,
            base,
            old,
            history.oids[sequence].clone(),
            (sequence == 0).then(|| history.pack.clone()),
        )
        .await;
        base = result.0;
        old = Some(result.1);
    }
    let before = repo.open_view().await.unwrap();
    assert_eq!(
        before.ref_capsule_count("refs/heads/main"),
        crate::maintenance::FOREGROUND_CAPSULE_THRESHOLD
    );

    repo.checkpoint_now(&server, &CancellationToken::new())
        .await
        .unwrap();
    let checkpoint = repo.open_view().await.unwrap();
    assert_eq!(checkpoint.ref_capsule_count("refs/heads/main"), 0);
    let result = publish_next(
        &repo,
        checkpoint.root_snapshot().clone(),
        old,
        history.oids[56].clone(),
        None,
    )
    .await;
    let after = repo.open_view().await.unwrap();
    assert_eq!(after.refs().get("refs/heads/main"), Some(&result.1));
    assert_eq!(after.ref_capsule_count("refs/heads/main"), 1);
    close(&server).await;
}
