#![cfg(unix)]

use super::*;

use crab_cell_runtime::{
    ApplicationId, ApplicationIdentity, CellAuthority, Digest, NodeDirectory, SessionId, TenantId,
};
use crab_storage::CellStorageLayout;
use object_store::{memory::InMemory, path::Path as ObjectPath};
use serde_json::Value;

use crate::{
    auth::Identity,
    peer::{LocalCellResolver, NodePublisher, PeerHttpRoundTrip, PeerReceiver},
    peer_tls::{LoadedPeerTls, PeerTlsIdentity, tests::IdentityFiles},
};

struct UnavailablePeer;

impl PeerRoundTrip for UnavailablePeer {
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

#[tokio::test(flavor = "multi_thread")]
async fn public_collaboration_requests_reach_remote_owner_over_mtls_and_publish_ltx() {
    let store = Store::new(Arc::new(InMemory::new()));
    let repository = repository(store.clone()).await;
    let repository_id = repository.id;
    let identity = ApplicationIdentity::new(
        TenantId::from_bytes([2; 16]),
        ApplicationId::from_bytes([3; 16]),
    );
    let cell_layout = CellStorageLayout::new(
        store.clone(),
        ObjectPath::from("remote-owner-cells"),
        *identity.application().as_bytes(),
    );
    let registry = Arc::new(crate::cells::compiled_registry().unwrap());
    crate::cells::bootstrap_release_at(
        &cell_layout,
        identity,
        &registry,
        &format!("sha256:{}", "a".repeat(64)),
    )
    .await
    .unwrap();
    let initialize_dir = tempfile::TempDir::new().unwrap();
    crate::cells::initialize_repository_at(
        &cell_layout,
        identity,
        &registry,
        initialize_dir.path(),
        "https://localhost:1".into(),
        repository_id,
    )
    .await
    .unwrap();

    let management_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let management_endpoint = format!(
        "https://localhost:{}",
        management_listener.local_addr().unwrap().port()
    );
    let identity_files = IdentityFiles::generate();
    let peer_tls = Arc::new(
        LoadedPeerTls::load(&identity_files.config(url::Url::parse(&management_endpoint).unwrap()))
            .unwrap(),
    );
    let image = Digest::from_bytes([6; 32]);
    let directory = NodeDirectory::new(
        cell_layout.clone(),
        peer_tls.fleet(),
        image,
        registry.release_digest(),
    );
    let ingress_session = SessionId::from_bytes([7; 16]);
    let owner_session = SessionId::from_bytes([8; 16]);
    let ingress_dir = tempfile::TempDir::new().unwrap();
    let owner_dir = tempfile::TempDir::new().unwrap();
    let ingress_publisher = NodePublisher::new(
        directory.clone(),
        peer_tls.signing_key().clone(),
        ingress_session,
        "https://localhost:2".into(),
        peer_tls.fleet(),
        peer_tls.certificate(),
        image,
        registry.release_digest(),
        registry.module_digests(),
        ingress_dir.path().to_path_buf(),
        crate::cells::SchedulerStatus::new(crate::cells::unix_now_ms().unwrap()).unwrap(),
    )
    .unwrap();
    let owner_publisher = NodePublisher::new(
        directory.clone(),
        peer_tls.signing_key().clone(),
        owner_session,
        management_endpoint.clone(),
        peer_tls.fleet(),
        peer_tls.certificate(),
        image,
        registry.release_digest(),
        registry.module_digests(),
        owner_dir.path().to_path_buf(),
        crate::cells::SchedulerStatus::new(crate::cells::unix_now_ms().unwrap()).unwrap(),
    )
    .unwrap();
    ingress_publisher.publish_initial().await.unwrap();
    owner_publisher.publish_initial().await.unwrap();

    let owner_runtime = runtime(owner_session);
    let owner_router = crate::cells::RepositoryCellRouter::new(
        identity,
        cell_layout.clone(),
        Arc::clone(&registry),
        owner_runtime.clone(),
        crate::cells::RepositoryCellPeer::new(
            directory.clone(),
            Arc::new(crab_cell_runtime::PeerSigner::new(
                owner_session,
                registry.release_digest(),
                peer_tls.signing_key().clone(),
            )),
            Arc::new(UnavailablePeer),
            crab_cell_runtime::Owner {
                session: owner_session,
                endpoint: management_endpoint,
            },
        ),
        owner_publisher.session_dir(),
    )
    .unwrap();
    let local_operator = Identity {
        issuer: "urn:crab:local".into(),
        subject: "operator".into(),
        name: "Local operator".into(),
    };
    owner_router
        .route(repository_id, &local_operator, "repository.read")
        .await
        .unwrap();

    let authority = CellAuthority::new(cell_layout.clone());
    let target = crab_cell_runtime::CellTarget::new(
        identity.tenant(),
        identity.application(),
        crate::cells::REPOSITORY_NAMESPACE,
        repository_id.as_bytes(),
    )
    .unwrap();
    let root_before = authority
        .load(target.cell_id())
        .await
        .unwrap()
        .unwrap()
        .value()
        .root
        .clone();

    let owner_server = server(
        Arc::clone(&repository),
        store.clone(),
        owner_runtime.clone(),
        None,
        Some(PeerReceiver::new(
            directory.clone(),
            Arc::clone(&registry),
            crab_cell_runtime::ReleaseStore::new(cell_layout.clone(), identity).unwrap(),
            LocalCellResolver::new(cell_layout.clone(), identity, owner_runtime.clone()),
            Arc::new(UnavailablePeer),
        )),
    );
    let management = management_router(Arc::clone(&owner_server));
    let (management_stop, management_done) = tokio::sync::oneshot::channel();
    let management_tls = Arc::clone(&peer_tls);
    let management_task = tokio::spawn(async move {
        axum::serve(
            management_tls.listener(management_listener),
            management.into_make_service_with_connect_info::<PeerTlsIdentity>(),
        )
        .with_graceful_shutdown(async move {
            let _ = management_done.await;
        })
        .await
        .unwrap();
    });

    let ingress_runtime = runtime(ingress_session);
    let round_trip: Arc<dyn PeerRoundTrip> = Arc::new(PeerHttpRoundTrip::new(
        identity,
        authority.clone(),
        directory.clone(),
        peer_tls.client_identity(),
        ingress_session,
    ));
    let ingress_router = crate::cells::RepositoryCellRouter::new(
        identity,
        cell_layout.clone(),
        Arc::clone(&registry),
        ingress_runtime.clone(),
        crate::cells::RepositoryCellPeer::new(
            directory.clone(),
            Arc::new(crab_cell_runtime::PeerSigner::new(
                ingress_session,
                registry.release_digest(),
                peer_tls.signing_key().clone(),
            )),
            round_trip,
            crab_cell_runtime::Owner {
                session: ingress_session,
                endpoint: "https://localhost:2".into(),
            },
        ),
        ingress_publisher.session_dir(),
    )
    .unwrap();
    let ingress_server = server(
        repository,
        store,
        ingress_runtime.clone(),
        Some(ingress_router),
        None,
    );
    let public_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let public_origin = format!("http://{}", public_listener.local_addr().unwrap());
    let public_app = router(Arc::clone(&ingress_server));
    let (public_stop, public_done) = tokio::sync::oneshot::channel();
    let public_task = tokio::spawn(async move {
        axum::serve(public_listener, public_app)
            .with_graceful_shutdown(async move {
                let _ = public_done.await;
            })
            .await
            .unwrap();
    });

    let client = reqwest::Client::new();
    let created = client
        .post(format!("{public_origin}/api/repos/team/repo/issues"))
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(
            serde_json::json!({
                "request_id": "00000000-0000-4000-8000-000000000001",
                "title": "Remote Cell",
                "body": "Written on the owner node"
            })
            .to_string(),
        )
        .send()
        .await
        .unwrap();
    let created_status = created.status();
    let created_bytes = created.bytes().await.unwrap();
    assert_eq!(
        created_status,
        StatusCode::CREATED,
        "{}",
        String::from_utf8_lossy(&created_bytes)
    );
    let created: Value = serde_json::from_slice(&created_bytes).unwrap();
    assert_eq!(created["number"], 1);
    let label = client
        .post(format!("{public_origin}/api/repos/team/repo/labels"))
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(
            serde_json::json!({
                "request_id": "00000000-0000-4000-8000-000000000002",
                "name": "remote",
                "color": "123abc",
                "description": "Created on the owner node"
            })
            .to_string(),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(label.status(), StatusCode::CREATED);
    let label: Value = serde_json::from_slice(&label.bytes().await.unwrap()).unwrap();
    assert_eq!(label["id"], 1);
    let assigned = client
        .patch(format!("{public_origin}/api/repos/team/repo/issues/1"))
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(serde_json::json!({"version":1,"label_ids":[1]}).to_string())
        .send()
        .await
        .unwrap();
    assert_eq!(assigned.status(), StatusCode::OK);
    let assigned: Value = serde_json::from_slice(&assigned.bytes().await.unwrap()).unwrap();
    assert_eq!(assigned["labels"][0]["name"], "remote");
    let listed = client
        .get(format!(
            "{public_origin}/api/repos/team/repo/issues?state=all"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(listed.status(), StatusCode::OK);
    let listed: Value = serde_json::from_slice(&listed.bytes().await.unwrap()).unwrap();
    assert_eq!(listed["items"][0]["title"], "Remote Cell");
    assert_eq!(listed["items"][0]["labels"][0]["id"], 1);
    let root_after = authority
        .load(target.cell_id())
        .await
        .unwrap()
        .unwrap()
        .value()
        .root
        .clone();
    assert_ne!(root_after, root_before);

    public_stop.send(()).unwrap();
    management_stop.send(()).unwrap();
    public_task.await.unwrap();
    management_task.await.unwrap();
    ingress_server.shutdown_runtimes().await.unwrap();
    owner_server.shutdown_runtimes().await.unwrap();
}

async fn repository(store: Store) -> Arc<Repository> {
    let layout = StoreLayout::new(store.clone(), "remote-owner".into());
    crab_write::initialize::initialize_repository(&store, &layout, "refs/heads/main")
        .await
        .unwrap();
    Arc::new(Repository {
        id: Uuid::from_bytes([1; 16]),
        config: RepositoryConfig {
            owner: "team".into(),
            name: "repo".into(),
            bucket: "memory".into(),
            prefix: "remote-owner".into(),
            default_branch: "main".into(),
            description: String::new(),
            members: Vec::new(),
            protected_branches: Vec::new(),
        },
        identity: RepositoryIdentity::new("memory", "remote-owner", 1).unwrap(),
        store,
        layout,
        protections: RwLock::new(BranchProtections::configured(&[])),
        lifecycle: RwLock::new(RepositoryLifecycle::active()),
        pinned: Mutex::new(None),
        maintenance: Mutex::new(None),
    })
}

fn runtime(session: SessionId) -> CellRuntime {
    CellRuntime::new(
        SqlWorkerPool::new(1, 16).unwrap(),
        16 * 1024 * 1024,
        session,
    )
    .unwrap()
}

fn server(
    repository: Arc<Repository>,
    store: Store,
    cell_runtime: CellRuntime,
    repository_cells: Option<crate::cells::RepositoryCellRouter>,
    peer_receiver: Option<PeerReceiver>,
) -> Arc<Server> {
    Arc::new(Server {
        repositories: BTreeMap::from([(("team".into(), "repo".into()), repository)]).into(),
        runtime: Arc::new(RemoteGitRuntime::default()),
        cell_runtime,
        repository_cells,
        peer_receiver,
        options: RepositoryOptions::default(),
        cursor_key: [0; 32],
        admission: Semaphore::new(16),
        transfer_admission: TransferAdmission::new(
            store,
            "remote-owner/.crab/http-server/v1/admission".into(),
            4,
        ),
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
        metrics: crate::metrics::Metrics::new().unwrap(),
    })
}
