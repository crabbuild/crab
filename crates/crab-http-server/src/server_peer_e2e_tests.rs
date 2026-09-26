#![cfg(unix)]

use super::*;

use crab_cell_runtime::cell::application::ApplicationIdentity;
use crab_cell_runtime::client::CellDescription;
use crab_cell_runtime::control::authority::CellAuthority;
use crab_cell_runtime::identity::{ApplicationId, Digest, SessionId, TenantId};
use crab_cell_runtime::ltx::CellStorageLayout;
use crab_cell_runtime::node::NodeDirectory;
use crab_cell_runtime::peer::{PeerPrincipal, PeerReplicaResolver, ReplicaPeerClient};
use crab_storage::{ObjectStoreCredentials, build_explicit_store};
use object_store::{memory::InMemory, path::Path as ObjectPath};
use serde_json::Value;

use crate::{
    auth::Identity,
    peer::{LocalCellResolver, NodePublisher, PeerHttpRoundTrip, PeerReceiver},
    peer_tls::{LoadedPeerTls, PeerTlsIdentity, tests::IdentityFiles},
};

struct UnavailablePeer;

async fn json_request(
    client: &reqwest::Client,
    method: reqwest::Method,
    url: impl reqwest::IntoUrl,
    body: Value,
) -> (StatusCode, Value) {
    let response = client
        .request(method, url)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(body.to_string())
        .send()
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.bytes().await.unwrap();
    let value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| panic!("{status}: {}", String::from_utf8_lossy(&bytes)));
    (status, value)
}

async fn json_get(client: &reqwest::Client, url: impl reqwest::IntoUrl) -> (StatusCode, Value) {
    let response = client.get(url).send().await.unwrap();
    let status = response.status();
    let bytes = response.bytes().await.unwrap();
    let value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| panic!("{status}: {}", String::from_utf8_lossy(&bytes)));
    (status, value)
}

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
    public_collaboration_remote_owner(
        Store::new(Arc::new(InMemory::new())),
        "memory",
        "remote-owner",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires an isolated pre-created RustFS bucket, prefix and explicit test credentials"]
async fn rustfs_public_collaboration_reaches_remote_owner_and_publishes_ltx() {
    let required = |name| std::env::var(name).unwrap_or_else(|_| panic!("missing {name}"));
    let bucket = required("CRAB_HTTP_CELL_TEST_BUCKET");
    let store = build_explicit_store(
        &bucket,
        ObjectStoreCredentials::Aws {
            access_key_id: required("AWS_ACCESS_KEY_ID"),
            secret_access_key: required("AWS_SECRET_ACCESS_KEY"),
            session_token: None,
            region: "us-east-1".into(),
        },
        Some(&required("CRAB_HTTP_CELL_TEST_ENDPOINT")),
        true,
    )
    .unwrap();
    public_collaboration_remote_owner(store, &bucket, &required("CRAB_HTTP_CELL_TEST_PREFIX"))
        .await;
}

async fn public_collaboration_remote_owner(store: Store, bucket: &str, root: &str) {
    let repository_prefix = format!("{root}/repository");
    let repository = repository(store.clone(), bucket, repository_prefix).await;
    let repository_id = repository.id;
    let identity = ApplicationIdentity::new(
        TenantId::from_bytes([2; 16]),
        ApplicationId::from_bytes([3; 16]),
    );
    let cell_layout = CellStorageLayout::new(
        store.clone(),
        ObjectPath::from(format!("{root}/cells")),
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
    let application = crate::cells::compiled_application().unwrap();
    crate::cells::initialize_repository_at(
        &cell_layout,
        identity,
        &registry,
        &application,
        initialize_dir.path(),
        32 * 1024 * 1024 * 1024,
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
    let ingress_management_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let ingress_management_endpoint = format!(
        "https://localhost:{}",
        ingress_management_listener.local_addr().unwrap().port()
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
    let ingress_publisher = Arc::new(
        NodePublisher::new(
            directory.clone(),
            peer_tls.signing_key().clone(),
            ingress_session,
            ingress_management_endpoint.clone(),
            crab_cell_runtime::node::NodeFailureDomain::default(),
            peer_tls.fleet(),
            peer_tls.certificate(),
            image,
            registry.release_digest(),
            registry.module_digests(),
            ingress_dir.path().to_path_buf(),
            32 * 1024 * 1024 * 1024,
            crate::cells::SchedulerStatus::new(crate::cells::unix_now_ms().unwrap()).unwrap(),
        )
        .unwrap(),
    );
    let owner_publisher = Arc::new(
        NodePublisher::new(
            directory.clone(),
            peer_tls.signing_key().clone(),
            owner_session,
            management_endpoint.clone(),
            crab_cell_runtime::node::NodeFailureDomain::default(),
            peer_tls.fleet(),
            peer_tls.certificate(),
            image,
            registry.release_digest(),
            registry.module_digests(),
            owner_dir.path().to_path_buf(),
            32 * 1024 * 1024 * 1024,
            crate::cells::SchedulerStatus::new(crate::cells::unix_now_ms().unwrap()).unwrap(),
        )
        .unwrap(),
    );
    let owner_node = owner_publisher.node();
    ingress_publisher.publish_initial().await.unwrap();
    owner_publisher.publish_initial().await.unwrap();
    let ingress_session_dir = ingress_publisher.session_dir();
    let owner_session_dir = owner_publisher.session_dir();

    let owner_runtime = runtime(owner_session);
    let owner_router = crate::cells::RepositoryCellRouter::new(
        identity,
        cell_layout.clone(),
        Arc::clone(&registry),
        owner_runtime.clone(),
        crate::cells::RepositoryCellPeer::new(
            directory.clone(),
            Arc::new(crab_cell_runtime::peer::PeerSigner::new(
                owner_session,
                registry.release_digest(),
                peer_tls.signing_key().clone(),
            )),
            Arc::new(PeerHttpRoundTrip::new(
                identity,
                CellAuthority::new(cell_layout.clone()),
                directory.clone(),
                peer_tls.client_identity(),
                owner_session,
            )),
            crab_cell_runtime::control::Owner {
                session: owner_session,
                endpoint: management_endpoint,
            },
        ),
        owner_session_dir,
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

    let owner_read_replicas = crate::cells::ReadReplicaManager::new(
        owner_runtime.clone(),
        Arc::clone(&registry),
        cell_layout.clone(),
        directory.clone(),
        owner_session,
        owner_dir.path().join("read-replicas"),
    );

    let owner_server = server(
        Arc::clone(&repository),
        store.clone(),
        owner_runtime.clone(),
        Some(owner_router.clone()),
        Some(PeerReceiver::new(
            owner_node,
            owner_session,
            directory.clone(),
            Arc::clone(&registry),
            Arc::new(
                crab_cell_runtime::recovery::release::ReleaseStore::new(
                    cell_layout.clone(),
                    identity,
                )
                .unwrap(),
            ),
            LocalCellResolver::new(cell_layout.clone(), identity, owner_runtime.clone()),
            Arc::new(UnavailablePeer),
            Some(owner_read_replicas.clone()),
        )),
    );
    let owner_heartbeat_stop = CancellationToken::new();
    let owner_heartbeat_task = tokio::spawn(
        Arc::clone(&owner_publisher)
            .run_shared(Arc::clone(&owner_server), owner_heartbeat_stop.clone()),
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

    let ingress_runtime = CellRuntime::new(
        SqlWorkerPool::new(1, 128).unwrap(),
        16 * 1024 * 1024,
        ingress_session,
    )
    .unwrap();
    let reader = crate::cells::ReadReplicaManager::new(
        ingress_runtime.clone(),
        Arc::clone(&registry),
        cell_layout.clone(),
        directory.clone(),
        ingress_session,
        ingress_dir.path().join("read-replicas"),
    );
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
            Arc::new(crab_cell_runtime::peer::PeerSigner::new(
                ingress_session,
                registry.release_digest(),
                peer_tls.signing_key().clone(),
            )),
            Arc::clone(&round_trip),
            crab_cell_runtime::control::Owner {
                session: ingress_session,
                endpoint: ingress_management_endpoint.clone(),
            },
        ),
        ingress_session_dir,
    )
    .unwrap()
    .with_read_replicas(Some(reader.clone()));
    let ingress_server = server(
        repository,
        store,
        ingress_runtime.clone(),
        Some(ingress_router),
        Some(PeerReceiver::new(
            ingress_publisher.node(),
            ingress_session,
            directory.clone(),
            Arc::clone(&registry),
            Arc::new(
                crab_cell_runtime::recovery::release::ReleaseStore::new(
                    cell_layout.clone(),
                    identity,
                )
                .unwrap(),
            ),
            LocalCellResolver::new(cell_layout.clone(), identity, ingress_runtime.clone()),
            round_trip,
            Some(reader.clone()),
        )),
    );
    let ingress_management = management_router(Arc::clone(&ingress_server));
    let (ingress_management_stop, ingress_management_done) = tokio::sync::oneshot::channel();
    let ingress_management_tls = Arc::clone(&peer_tls);
    let ingress_management_task = tokio::spawn(async move {
        axum::serve(
            ingress_management_tls.listener(ingress_management_listener),
            ingress_management.into_make_service_with_connect_info::<PeerTlsIdentity>(),
        )
        .with_graceful_shutdown(async move {
            let _ = ingress_management_done.await;
        })
        .await
        .unwrap();
    });
    let ingress_heartbeat_stop = CancellationToken::new();
    let ingress_heartbeat_task = tokio::spawn(
        Arc::clone(&ingress_publisher)
            .run_shared(Arc::clone(&ingress_server), ingress_heartbeat_stop.clone()),
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

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3 * 60))
        .build()
        .unwrap();
    let source = tempfile::TempDir::new().unwrap();
    let source_path = source.path();
    let git_url = format!("{public_origin}/git/team/repo.git");
    crate::server::receive_tests::success(
        source_path,
        &["init", "--initial-branch=main", "--object-format=sha1", "."],
    )
    .await;
    std::fs::write(source_path.join("README.md"), "base\n").unwrap();
    crate::server::receive_tests::success(source_path, &["add", "README.md"]).await;
    crate::server::receive_tests::success(source_path, &["commit", "-m", "base"]).await;
    let base_oid = crate::server::receive_tests::success(source_path, &["rev-parse", "HEAD"]).await;
    crate::server::receive_tests::success(source_path, &["push", &git_url, "main"]).await;
    crate::server::receive_tests::success(source_path, &["checkout", "-b", "feature"]).await;
    std::fs::write(source_path.join("README.md"), "base\nfeature\n").unwrap();
    crate::server::receive_tests::success(source_path, &["commit", "-am", "feature"]).await;
    let feature_oid =
        crate::server::receive_tests::success(source_path, &["rev-parse", "HEAD"]).await;
    crate::server::receive_tests::success(source_path, &["push", &git_url, "feature"]).await;
    eprintln!("qualified native Git main and feature pushes");

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
    eprintln!("qualified issue and label mutations");
    let comment = json_request(
        &client,
        reqwest::Method::POST,
        format!("{public_origin}/api/repos/team/repo/issues/1/comments"),
        serde_json::json!({
            "request_id": "00000000-0000-4000-8000-000000000004",
            "body": "Durable issue comment"
        }),
    )
    .await;
    assert_eq!(comment.0, StatusCode::CREATED);
    assert_eq!(comment.1["number"], 1);
    eprintln!("qualified issue comment mutation");
    let status = json_request(
        &client,
        reqwest::Method::POST,
        format!("{public_origin}/api/repos/team/repo/statuses/{feature_oid}"),
        serde_json::json!({
            "request_id": "00000000-0000-4000-8000-000000000005",
            "context": "e2e/runtime",
            "state": "success",
            "description": "Remote owner published the status",
            "target_url": format!("{public_origin}/team/repo")
        }),
    )
    .await;
    assert_eq!(status.0, StatusCode::CREATED);
    assert_eq!(status.1["context"], "e2e/runtime");
    eprintln!("qualified commit status mutation");
    let check = json_request(
        &client,
        reqwest::Method::POST,
        format!("{public_origin}/api/repos/team/repo/check-runs"),
        serde_json::json!({
            "request_id": "00000000-0000-4000-8000-000000000006",
            "head_sha": feature_oid,
            "name": "e2e/runtime",
            "status": "completed",
            "conclusion": "success",
            "details_url": format!("{public_origin}/team/repo"),
            "output": {
                "title": "Runtime matrix passed",
                "summary": "Published through the remote Cell owner"
            }
        }),
    )
    .await;
    assert_eq!(check.0, StatusCode::CREATED);
    assert_eq!(check.1["conclusion"], "success");
    eprintln!("qualified check run mutation");
    let pull = json_request(
        &client,
        reqwest::Method::POST,
        format!("{public_origin}/api/repos/team/repo/pulls"),
        serde_json::json!({
            "request_id": "00000000-0000-4000-8000-000000000007",
            "title": "Remote Cell pull",
            "body": "Durable pull request",
            "base_ref": "refs/heads/main",
            "head_ref": "refs/heads/feature"
        }),
    )
    .await;
    assert_eq!(pull.0, StatusCode::CREATED);
    assert_eq!(pull.1["number"], 1);
    assert_eq!(pull.1["base_oid"], base_oid);
    assert_eq!(pull.1["head_oid"], feature_oid);
    eprintln!("qualified pull request mutation");
    let review_thread = json_request(
        &client,
        reqwest::Method::POST,
        format!("{public_origin}/api/repos/team/repo/pulls/1/threads"),
        serde_json::json!({
            "request_id": "00000000-0000-4000-8000-000000000010",
            "body": "Remote inline review",
            "suggested_text": "base\nreviewed\n",
            "base_oid": base_oid,
            "head_oid": feature_oid,
            "path_hex": "524541444d452e6d64",
            "side": "new",
            "start_line": 2,
            "end_line": 2
        }),
    )
    .await;
    assert_eq!(review_thread.0, StatusCode::CREATED);
    assert_eq!(review_thread.1["number"], 1);
    assert_eq!(review_thread.1["path_hex"], "524541444d452e6d64");
    assert_eq!(review_thread.1["current"], true);
    let review_reply = json_request(
        &client,
        reqwest::Method::POST,
        format!("{public_origin}/api/repos/team/repo/pulls/1/threads/1/replies"),
        serde_json::json!({
            "request_id": "00000000-0000-4000-8000-000000000011",
            "body": "I will address this in the next push"
        }),
    )
    .await;
    assert_eq!(review_reply.0, StatusCode::CREATED);
    assert_eq!(review_reply.1["number"], 1);
    let resolved_thread = json_request(
        &client,
        reqwest::Method::PATCH,
        format!("{public_origin}/api/repos/team/repo/pulls/1/threads/1"),
        serde_json::json!({"version": review_thread.1["version"], "resolved": true}),
    )
    .await;
    assert_eq!(resolved_thread.0, StatusCode::OK);
    assert_eq!(resolved_thread.1["resolved"], true);
    assert_eq!(resolved_thread.1["resolved_by"], "Local operator");
    eprintln!("qualified inline review thread, reply, and resolution");
    let pull_comment = json_request(
        &client,
        reqwest::Method::POST,
        format!("{public_origin}/api/repos/team/repo/pulls/1/comments"),
        serde_json::json!({
            "request_id": "00000000-0000-4000-8000-000000000008",
            "body": "Durable pull comment"
        }),
    )
    .await;
    assert_eq!(pull_comment.0, StatusCode::CREATED);
    assert_eq!(pull_comment.1["number"], 1);
    eprintln!("qualified pull comment mutation");
    let release = json_request(
        &client,
        reqwest::Method::POST,
        format!("{public_origin}/api/repos/team/repo/releases"),
        serde_json::json!({
            "request_id": "00000000-0000-4000-8000-000000000009",
            "tag_name": "e2e-runtime",
            "target_oid": feature_oid,
            "title": "Runtime qualification",
            "body": "Durable release and native Git tag",
            "prerelease": true,
            "draft": false
        }),
    )
    .await;
    assert_eq!(release.0, StatusCode::CREATED);
    assert_eq!(release.1["number"], 1);
    assert_eq!(release.1["tag_name"], "e2e-runtime");
    eprintln!("qualified release and Git tag mutation");
    let protections = json_request(
        &client,
        reqwest::Method::PUT,
        format!("{public_origin}/api/repos/team/repo/settings/branch-protections"),
        serde_json::json!({
            "expected_version": 0,
            "rules": [{
                "branch": "main",
                "required_approvals": 0,
                "required_checks": ["e2e/runtime"]
            }]
        }),
    )
    .await;
    assert_eq!(protections.0, StatusCode::OK);
    assert_eq!(protections.1["version"], 1);
    eprintln!("qualified branch protection mutation");
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

    let current = authority.load(target.cell_id()).await.unwrap().unwrap();
    let readers_url = format!("{public_origin}/api/repos/team/repo/settings/read-replicas");
    let initial_target = json_get(&client, readers_url.as_str()).await;
    assert_eq!(initial_target.0, StatusCode::OK);
    assert_eq!(initial_target.1["desired_readers"], 0);
    let one_reader = json_request(
        &client,
        reqwest::Method::PUT,
        readers_url.as_str(),
        serde_json::json!({"expected_revision":0,"desired_readers":1}),
    )
    .await;
    assert_eq!(one_reader.0, StatusCode::ACCEPTED);
    assert_eq!(one_reader.1["revision"], 1);
    let target_status = json_get(&client, readers_url.as_str()).await;
    assert_eq!(target_status.1["convergence"], "ready");
    assert_eq!(target_status.1["readiness"]["ready_readers"], 1);
    let remote_status = owner_router
        .read_replica_status(&target, &owner_read_replicas)
        .await
        .unwrap();
    assert_eq!(remote_status.ready_readers, 1);
    let ready = reader
        .resolve(target.clone())
        .await
        .unwrap()
        .receipt()
        .await;
    let observed = reader
        .resolve(target.clone())
        .await
        .unwrap()
        .query::<crate::cells::repository::GetIssue>(Some(ready), 1)
        .await
        .unwrap();
    assert_eq!(observed.output.unwrap().title, "Remote Cell");
    let replica_url = format!("{public_origin}/api/repos/team/repo/issues/1?read=replica");
    let replica_response = client.get(&replica_url).send().await.unwrap();
    let replica_status = replica_response.status();
    let replica_headers = replica_response.headers().clone();
    let replica_bytes = replica_response.bytes().await.unwrap();
    assert_eq!(
        replica_status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&replica_bytes)
    );
    assert_eq!(
        replica_headers
            .get("x-crab-cell-reader")
            .unwrap()
            .to_str()
            .unwrap()
            .len(),
        32
    );
    let incarnation = replica_headers
        .get("x-crab-cell-incarnation")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let sequence = replica_headers
        .get("x-crab-cell-sequence")
        .unwrap()
        .to_str()
        .unwrap()
        .parse::<u64>()
        .unwrap();
    let replica_body: Value = serde_json::from_slice(&replica_bytes).unwrap();
    assert_eq!(replica_body["title"], "Remote Cell");
    assert_eq!(replica_body["labels"][0]["id"], 1);
    let behind_url = format!(
        "{replica_url}&after_incarnation={incarnation}&after_sequence={}",
        sequence + 1
    );
    let behind = json_get(&client, behind_url.as_str()).await;
    assert_eq!(behind.0, StatusCode::CONFLICT);
    assert_eq!(behind.1["error"]["code"], "replica_behind");
    let selected_reader = directory
        .load(ingress_session, crate::cells::unix_now_ms().unwrap())
        .await
        .unwrap()
        .unwrap()
        .advertisement()
        .clone();
    let replica_peer = ReplicaPeerClient::new(
        Arc::clone(&registry),
        Arc::new(crab_cell_runtime::peer::PeerSigner::new(
            owner_session,
            registry.release_digest(),
            peer_tls.signing_key().clone(),
        )),
        PeerPrincipal {
            issuer: local_operator.issuer.clone(),
            subject: local_operator.subject.clone(),
            actions: vec!["repository.read".into()],
        },
        Arc::new(PeerHttpRoundTrip::new(
            identity,
            authority.clone(),
            directory.clone(),
            peer_tls.client_identity(),
            owner_session,
        )),
    );
    let exact = CellDescription {
        cell: target.cell_id(),
        incarnation: current.value().incarnation,
        code: current.value().code,
        schema: current.value().schema,
    };
    let remote = replica_peer
        .query::<crate::cells::repository::GetIssue>(
            &target,
            selected_reader.clone(),
            exact,
            Some(ready),
            1,
        )
        .await
        .unwrap();
    assert_eq!(remote.output.unwrap().title, "Remote Cell");

    use futures_util::StreamExt as _;
    let concurrent = futures_util::stream::iter(0..32)
        .map(|_| {
            replica_peer.query::<crate::cells::repository::GetIssue>(
                &target,
                selected_reader.clone(),
                exact,
                Some(ready),
                1,
            )
        })
        .buffer_unordered(8)
        .collect::<Vec<_>>()
        .await;
    assert!(
        concurrent.iter().all(std::result::Result::is_ok),
        "concurrent replica reads: {concurrent:?}"
    );

    let stale_update = json_request(
        &client,
        reqwest::Method::PUT,
        readers_url.as_str(),
        serde_json::json!({"expected_revision":0,"desired_readers":0}),
    )
    .await;
    assert_eq!(stale_update.0, StatusCode::CONFLICT);
    let two_readers = json_request(
        &client,
        reqwest::Method::PUT,
        readers_url.as_str(),
        serde_json::json!({"expected_revision":1,"desired_readers":2}),
    )
    .await;
    assert_eq!(two_readers.0, StatusCode::ACCEPTED);
    let shortfall = json_get(&client, readers_url.as_str()).await;
    assert_eq!(shortfall.1["convergence"], "shortfall");
    assert_eq!(shortfall.1["readiness"]["selected_readers"], 1);
    let zero_readers = json_request(
        &client,
        reqwest::Method::PUT,
        readers_url.as_str(),
        serde_json::json!({"expected_revision":2,"desired_readers":0}),
    )
    .await;
    assert_eq!(zero_readers.0, StatusCode::ACCEPTED);
    let stop_readers = CancellationToken::new();
    let running_reader = reader.clone();
    let running_stop = stop_readers.clone();
    let reader_task = tokio::spawn(async move { running_reader.run(running_stop).await });
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if reader.resolve(target.clone()).await.is_err() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    stop_readers.cancel();
    reader_task.await.unwrap().unwrap();
    assert_eq!(ingress_runtime.stats().file_descriptors(), 0);
    let unavailable = json_get(&client, replica_url.as_str()).await;
    assert_eq!(unavailable.0, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(unavailable.1["error"]["code"], "replica_unavailable");
    let restored_target = json_request(
        &client,
        reqwest::Method::PUT,
        readers_url.as_str(),
        serde_json::json!({"expected_revision":3,"desired_readers":1}),
    )
    .await;
    assert_eq!(restored_target.0, StatusCode::ACCEPTED);
    assert!(reader.resolve(target.clone()).await.is_ok());

    management_stop.send(()).unwrap();
    management_task.await.unwrap();
    owner_heartbeat_stop.cancel();
    owner_heartbeat_task.await.unwrap().unwrap();
    let stale_owner = authority.load(target.cell_id()).await.unwrap().unwrap();
    let stale_reader = reader.resolve(target.clone()).await.unwrap();
    let (warm, ready) = reader.status(target.clone()).await.unwrap();
    assert!(!ready);
    assert_eq!(warm.incarnation, stale_owner.value().incarnation);
    assert!(matches!(
        stale_reader
            .query::<crate::cells::repository::GetIssue>(None, 1)
            .await,
        Err(crab_cell_runtime::Error::Fenced)
    ));
    std::fs::remove_dir_all(owner_dir.path()).unwrap();

    let restored = client
        .get(format!(
            "{public_origin}/api/repos/team/repo/issues?state=all"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(restored.status(), StatusCode::OK);
    let promoted = authority.load(target.cell_id()).await.unwrap().unwrap();
    assert_eq!(
        promoted.value().owner.as_ref().unwrap().session,
        ingress_session
    );
    assert!(promoted.value().epoch > stale_owner.value().epoch);
    assert!(reader.resolve(target.clone()).await.is_err());
    assert!(matches!(
        stale_reader
            .query::<crate::cells::repository::GetIssue>(None, 1)
            .await,
        Err(crab_cell_runtime::Error::Fenced)
    ));
    drop(stale_reader);

    let restored: Value = serde_json::from_slice(&restored.bytes().await.unwrap()).unwrap();
    assert_eq!(restored["items"][0]["title"], "Remote Cell");
    assert_eq!(restored["items"][0]["labels"][0]["name"], "remote");
    let (comments_status, comments) = json_get(
        &client,
        format!("{public_origin}/api/repos/team/repo/issues/1/comments"),
    )
    .await;
    assert_eq!(comments_status, StatusCode::OK);
    assert_eq!(comments["items"][0]["body"], "Durable issue comment");
    let (statuses_status, statuses) = json_get(
        &client,
        format!("{public_origin}/api/repos/team/repo/commits/{feature_oid}/status"),
    )
    .await;
    assert_eq!(statuses_status, StatusCode::OK);
    assert_eq!(statuses["state"], "success");
    assert_eq!(statuses["statuses"][0]["context"], "e2e/runtime");
    let (checks_status, checks) = json_get(
        &client,
        format!("{public_origin}/api/repos/team/repo/commits/{feature_oid}/check-runs"),
    )
    .await;
    assert_eq!(checks_status, StatusCode::OK);
    assert_eq!(checks["items"][0]["name"], "e2e/runtime");
    assert_eq!(checks["items"][0]["conclusion"], "success");
    let (restored_pull_status, restored_pull) = json_get(
        &client,
        format!("{public_origin}/api/repos/team/repo/pulls/1"),
    )
    .await;
    assert_eq!(restored_pull_status, StatusCode::OK);
    assert_eq!(restored_pull["title"], "Remote Cell pull");
    assert_eq!(restored_pull["head_oid"], feature_oid);
    assert_eq!(restored_pull["merge_requirements"]["protected"], true);
    assert_eq!(restored_pull["merge_requirements"]["satisfied"], true);
    let (restored_thread_status, restored_thread) = json_get(
        &client,
        format!("{public_origin}/api/repos/team/repo/pulls/1/threads/1"),
    )
    .await;
    assert_eq!(restored_thread_status, StatusCode::OK);
    assert_eq!(restored_thread["path_hex"], "524541444d452e6d64");
    assert_eq!(restored_thread["start_line"], 2);
    assert_eq!(restored_thread["end_line"], 2);
    assert_eq!(restored_thread["resolved"], true);
    assert_eq!(restored_thread["resolved_by"], "Local operator");
    let (restored_replies_status, restored_replies) = json_get(
        &client,
        format!("{public_origin}/api/repos/team/repo/pulls/1/threads/1/replies"),
    )
    .await;
    assert_eq!(restored_replies_status, StatusCode::OK);
    assert_eq!(
        restored_replies["items"][0]["body"],
        "I will address this in the next push"
    );
    eprintln!("qualified restored inline review conversation");
    let (pull_comments_status, pull_comments) = json_get(
        &client,
        format!("{public_origin}/api/repos/team/repo/pulls/1/comments"),
    )
    .await;
    assert_eq!(pull_comments_status, StatusCode::OK);
    assert_eq!(pull_comments["items"][0]["body"], "Durable pull comment");
    let (releases_status, releases) = json_get(
        &client,
        format!("{public_origin}/api/repos/team/repo/releases"),
    )
    .await;
    assert_eq!(releases_status, StatusCode::OK);
    assert_eq!(releases["items"][0]["tag_name"], "e2e-runtime");
    assert_eq!(releases["items"][0]["target_oid"], feature_oid);
    let (catalog_status, catalog) = json_get(&client, format!("{public_origin}/api/repos")).await;
    assert_eq!(catalog_status, StatusCode::OK);
    assert_eq!(catalog["repositories"][0]["protection_version"], 1);
    assert_eq!(
        catalog["repositories"][0]["protected_branches"][0]["required_checks"][0],
        "e2e/runtime"
    );
    eprintln!("qualified restored collaboration matrix");
    let taken_over = authority.load(target.cell_id()).await.unwrap().unwrap();
    // Idle compaction may replace the manifest after the last mutation.
    // Takeover must preserve the exact authority root observed at owner death,
    // while that root still identifies the acknowledged database position.
    assert_eq!(taken_over.value().root, stale_owner.value().root);
    let acknowledged = root_after.as_ref().unwrap();
    let recovered = taken_over.value().root.as_ref().unwrap();
    assert_eq!(
        (
            recovered.txid,
            recovered.checksum,
            recovered.commit_sequence
        ),
        (
            acknowledged.txid,
            acknowledged.checksum,
            acknowledged.commit_sequence
        )
    );
    assert_eq!(
        taken_over.value().owner.as_ref().unwrap().session,
        ingress_session
    );

    let continued = client
        .post(format!("{public_origin}/api/repos/team/repo/issues"))
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(
            serde_json::json!({
                "request_id": "00000000-0000-4000-8000-000000000003",
                "title": "Recovered Cell",
                "body": "Published by the successor node"
            })
            .to_string(),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(continued.status(), StatusCode::CREATED);
    let continued: Value = serde_json::from_slice(&continued.bytes().await.unwrap()).unwrap();
    assert_eq!(continued["number"], 2);
    let continued_control = authority.load(target.cell_id()).await.unwrap().unwrap();
    assert!(
        continued_control
            .value()
            .root
            .as_ref()
            .unwrap()
            .commit_sequence
            > taken_over.value().root.as_ref().unwrap().commit_sequence
    );

    let clone = tempfile::TempDir::new().unwrap();
    crate::server::receive_tests::success(
        clone.path(),
        &["clone", "--branch", "feature", &git_url, "."],
    )
    .await;
    assert_eq!(
        crate::server::receive_tests::success(clone.path(), &["rev-parse", "HEAD"]).await,
        feature_oid
    );
    assert_eq!(
        std::fs::read_to_string(clone.path().join("README.md")).unwrap(),
        "base\nfeature\n"
    );
    assert_eq!(
        crate::server::receive_tests::success(
            clone.path(),
            &["ls-remote", &git_url, "refs/tags/e2e-runtime"],
        )
        .await,
        format!("{feature_oid}\trefs/tags/e2e-runtime")
    );
    eprintln!("qualified clone and tag reads after takeover");
    let repository = ingress_server
        .repositories
        .get(&("team".into(), "repo".into()))
        .unwrap();
    assert!(
        repository
            .store
            .list_prefix(&repository.layout.repo_path("app/v1"))
            .await
            .unwrap()
            .is_empty()
    );

    public_stop.send(()).unwrap();
    public_task.await.unwrap();
    ingress_management_stop.send(()).unwrap();
    ingress_management_task.await.unwrap();
    ingress_heartbeat_stop.cancel();
    ingress_heartbeat_task.await.unwrap().unwrap();
    ingress_server.receives.close();
    ingress_server.receives.wait().await;
    ingress_server.shutdown_runtimes().await.unwrap();
    match owner_server.shutdown_runtimes().await {
        Ok(()) | Err(crate::Error::Cell(crab_cell_runtime::Error::Fenced)) => {}
        Err(error) => panic!("unexpected stale-owner shutdown result: {error}"),
    }
    let released = authority.load(target.cell_id()).await.unwrap().unwrap();
    assert_eq!(
        released.value().state,
        crab_cell_runtime::control::ControlState::Idle
    );
    assert!(released.value().owner.is_none());
    assert!(
        released.value().root.as_ref().unwrap().commit_sequence
            >= root_after.as_ref().unwrap().commit_sequence
    );
}

async fn repository(store: Store, bucket: &str, prefix: String) -> Arc<Repository> {
    let layout = StoreLayout::new(store.clone(), prefix.clone());
    crab_write::initialize::initialize_repository(&store, &layout, "refs/heads/main")
        .await
        .unwrap();
    Arc::new(Repository {
        id: Uuid::from_bytes([1; 16]),
        config: RepositoryConfig {
            owner: "team".into(),
            name: "repo".into(),
            bucket: bucket.into(),
            prefix: prefix.clone(),
            default_branch: "main".into(),
            description: String::new(),
            members: Vec::new(),
            protected_branches: Vec::new(),
        },
        identity: RepositoryIdentity::new(bucket, prefix.as_str(), 1).unwrap(),
        store,
        layout,
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
        cell_node: None,
        repository_cells,
        peer_receiver,
        follower_store: None,
        node_log_transport: None,
        options: RepositoryOptions::default(),
        cursor_key: [0; 32],
        admission: Semaphore::new(16),
        transfer_admission: TransferAdmission::new(
            store,
            "remote-owner/.crab/http-server/v1/admission".into(),
            4,
        ),
        local_staging: crate::local_disk::LocalStaging::for_test(),
        app_admission: Semaphore::new(8),
        maintenance_admission: Arc::new(Semaphore::new(2)),
        cancellation: CancellationToken::new(),
        receives: tokio_util::task::TaskTracker::new(),
        auth: None,
        git_import: None,
        catalog: None,
        catalog_healthy: AtomicBool::new(false),
        node_healthy: AtomicBool::new(true),
        scheduler_status: crate::cells::SchedulerStatus::new(crate::cells::unix_now_ms().unwrap())
            .unwrap(),
        cell_capacity: super::test_cell_capacity_report(),
        metrics: crate::metrics::Metrics::new(&[]).unwrap(),
    })
}
