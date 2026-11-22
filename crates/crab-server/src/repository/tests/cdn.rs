use axum::{
    extract::{Path, State},
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
    routing::get,
};
use crab_remote_git::RepositoryIdentity;
use crab_storage::{
    cdn::{CdnAccess, CdnConsumer, CdnProfile, CdnRoute, CdnTransport},
    test_support::{CountingObjectStore, ObjectReadKind},
};
use object_store::{ObjectStoreExt as _, memory::InMemory};
use sha2::{Digest as _, Sha256};

use super::*;

#[tokio::test]
async fn ordinary_http_reads_use_verified_edge_bodies_and_keep_native_controls() {
    let raw = Arc::new(InMemory::new());
    let native = Arc::new(CountingObjectStore::new(raw.clone()));
    let edge = Arc::new(CountingObjectStore::new(raw));
    let target = [3; 32];
    let origin = Store::new(native.clone())
        .with_target_identity(target)
        .with_bucket_identity(crab_storage::BucketIdentity::new(
            crab_storage::StorageProviderKind::S3,
            "fixture-bucket",
            "fixture-bucket",
        ));
    let server = fixture_with_store(origin.clone()).await;
    let repo = repository(&server);
    let git_body = b"approved git bytes";
    let mut root = crab_write::capsule_protocol::open_root(&repo.layout)
        .await
        .unwrap();
    let mut previous = None;
    // Fill the member budget so maintenance must publish a physical layer.
    // One ref-local pack would remain inside its original capsule run.
    for sequence in 0..8 {
        let history = crate::test_support::git::history_with_blob(&[sequence]);
        let (next, tip) = publish_next(
            &repo,
            root,
            previous,
            history.oids[0].clone(),
            Some(history.pack),
        )
        .await;
        root = next;
        previous = Some(tip);
    }
    let history = crate::test_support::git::history_with_blob(git_body);
    publish_next(
        &repo,
        root,
        previous,
        history.oids[0].clone(),
        Some(history.pack),
    )
    .await;
    crab_remote::checkpoint::maintain_capsule_repository(
        &repo.layout,
        1,
        16 * 1024 * 1024,
        &server.cancellation,
    )
    .await
    .unwrap();
    let pack_layers = origin
        .list_prefix(&repo.layout.repo_path("v2/pack-layers"))
        .await
        .unwrap();
    assert!(!pack_layers.is_empty());
    let lfs_body = Bytes::from_static(b"approved lfs bytes");
    let oid = Sha256::digest(&lfs_body).into();
    let lfs = crab_lfs::LfsObjectStore::new(origin.clone(), repo.layout.repo_prefix());
    lfs.put(&oid, lfs_body.clone()).await.unwrap();
    let lfs_path = lfs.object_path_for(&oid);
    let lfs_url = format!(
        "/git/team/repo.git/info/lfs/objects/{}?size={}",
        crab_git::lfs_pointer::hex_encode(&oid),
        lfs_body.len(),
    );
    let git_url = "/api/repos/team/repo/blob?rev=main&path=asset";
    let baseline = read(&server, git_url, None).await;
    assert_eq!(baseline.status(), StatusCode::OK);
    let git_etag = baseline.headers()[header::ETAG].clone();
    assert_eq!(
        axum::body::to_bytes(baseline.into_body(), 1024)
            .await
            .unwrap(),
        &git_body[..]
    );

    for object in &pack_layers {
        native.block_body_reads_for(&object.location);
    }
    native.block_body_reads_for(&lfs_path);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let transport = CdnTransport::loopback_fixture(listener.local_addr().unwrap()).unwrap();
    let edge_app = Router::new()
        .route("/{*key}", get(serve_object))
        .with_state(edge.clone());
    let edge_stop = CancellationToken::new();
    let stop = edge_stop.clone();
    let edge_task = tokio::spawn(async move {
        axum::serve(listener, edge_app)
            .with_graceful_shutdown(stop.cancelled_owned())
            .await
            .unwrap();
    });
    let read_store = origin
        .with_cdn_route(
            CdnRoute::new(
                target,
                repo.layout.repo_prefix().to_owned(),
                None,
                CdnAccess::Public,
                transport,
                Duration::from_secs(5),
            )
            .for_consumer(CdnConsumer::Server, CdnProfile::Direct),
        )
        .unwrap();
    install_layout(
        &server,
        &repo,
        Some(StoreLayout::new(
            read_store,
            repo.layout.repo_prefix().to_owned(),
        )),
    );

    let response = read(&server, git_url, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::ETAG], git_etag);
    assert_eq!(
        axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap(),
        &git_body[..]
    );
    assert!(edge.requests().iter().any(|request| {
        pack_layers
            .iter()
            .any(|object| object.location.as_ref() == request.location)
    }));

    let response = read(&server, &lfs_url, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    let lfs_etag = response.headers()[header::ETAG].clone();
    assert_eq!(
        axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap(),
        lfs_body
    );
    assert!(
        edge.requests()
            .iter()
            .any(|request| request.location == lfs_path.as_ref())
    );

    edge.reset();
    native.unblock_body_reads_for(&lfs_path);
    let response = read(
        &server,
        &lfs_url,
        Some((header::RANGE, "bytes=0-7".parse().unwrap())),
    )
    .await;
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.headers()[header::ETAG], lfs_etag);
    assert_eq!(
        response.headers()[header::CONTENT_RANGE],
        format!("bytes 0-7/{}", lfs_body.len())
    );
    assert_eq!(
        axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap(),
        &lfs_body[..8]
    );
    assert!(edge.requests().is_empty());

    for object in &pack_layers {
        native.unblock_body_reads_for(&object.location);
    }
    native.reset();
    install_layout(&server, &repo, None);
    let response = read(&server, git_url, None).await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()[header::ETAG], git_etag);
    assert_eq!(
        axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap(),
        &git_body[..]
    );
    assert!(native.requests().iter().any(|request| {
        request.kind != ObjectReadKind::Head
            && pack_layers
                .iter()
                .any(|object| object.location.as_ref() == request.location)
    }));
    assert!(edge.requests().is_empty());
    close(&server).await;
    edge_stop.cancel();
    edge_task.await.unwrap();
}

fn install_layout(server: &Server, repo: &Repository, layout: Option<StoreLayout<Store>>) {
    use crate::repository::catalog::RepositoryIndex;

    let version = server.repositories.version() + 1;
    let repository = Repository::new(
        repo.id,
        repo.config.clone(),
        repo.layout.clone(),
        RepositoryIdentity::new("native-fixture", repo.layout.repo_prefix(), version).unwrap(),
    )
    .with_read_layout(layout.map(|layout| {
        let identity =
            RepositoryIdentity::new("cdn-fixture", layout.repo_prefix(), version).unwrap();
        (layout, identity)
    }));
    server.repositories.replace(RepositoryIndex::new(
        version,
        BTreeMap::from([(
            (repo.config.owner.clone(), repo.config.name.clone()),
            Arc::new(repository),
        )]),
    ));
}

async fn read(
    server: &Arc<Server>,
    path: &str,
    header: Option<(header::HeaderName, header::HeaderValue)>,
) -> Response {
    let mut request = Request::builder()
        .uri(path)
        .header(header::HOST, "localhost:8788");
    if let Some((name, value)) = header {
        request = request.header(name, value);
    }
    router(server.clone())
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

async fn serve_object(
    State(store): State<Arc<CountingObjectStore>>,
    Path(key): Path<String>,
    headers: HeaderMap,
) -> Response {
    let body = store
        .get(&ObjectPath::from(key))
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    let Some(range) = headers.get(header::RANGE) else {
        return body.into_response();
    };
    let (start, end) = range
        .to_str()
        .unwrap()
        .strip_prefix("bytes=")
        .unwrap()
        .split_once('-')
        .unwrap();
    let (start, end) = (
        start.parse::<usize>().unwrap(),
        end.parse::<usize>().unwrap(),
    );
    (
        StatusCode::PARTIAL_CONTENT,
        [(
            header::CONTENT_RANGE,
            format!("bytes {start}-{end}/{}", body.len()),
        )],
        body.slice(start..=end),
    )
        .into_response()
}
