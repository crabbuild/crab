use crate::catalog::CatalogStore;
use crate::repository::MaintenanceSchedule;
use crate::repository::MaintenanceTrigger;
use crate::repository::Repository;
use crate::server::Server;
use crate::server::http::router;
use crate::test_support::server::{close, fixture, fixture_with_store, fixture_without_cells};
use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::http::StatusCode;
use bytes::Bytes;
use crab_metadata::capsule_protocol::{
    Capsule, CapsuleGitPack, CapsuleRefEdit, CapsuleSection, CapsuleSectionKind,
    CapsuleTransaction, CapsuleVisibilityDelta,
};
use crab_metadata::git_visibility::GitVisibilityEdit;
use crab_remote_git::RemoteGitRuntime;
use crab_remote_git::RepositoryOptions;
use crab_storage::Store;
use crab_storage::StoreLayout;
use object_store::path::Path as ObjectPath;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use crate::test_support::git::history as git_history;

mod browse;

mod probes;

fn repository(server: &Server) -> Arc<Repository> {
    server
        .repositories
        .get(&("team".into(), "repo".into()))
        .unwrap()
}

#[tokio::test]
async fn unchanged_browse_refresh_reads_only_mutable_control_metadata() {
    let server = fixture_without_cells().await;
    let reads = Arc::new(AtomicUsize::new(0));
    {
        let mut current = server.repositories.current.write().unwrap();
        let repo = Arc::get_mut(current.by_name.values_mut().next().unwrap()).unwrap();
        let observed = Arc::clone(&reads);
        repo.store = repo
            .store
            .clone()
            .with_read_request_observer(Arc::new(move |kind| {
                if kind == crab_storage::StorageReadKind::Get {
                    observed.fetch_add(1, Ordering::Relaxed);
                }
            }));
        repo.layout = StoreLayout::new(repo.store.clone(), "maintenance".into());
    }
    let repo = repository(&server);
    let history = git_history(1);
    let root = crab_write::capsule_protocol::open_root(&repo.layout)
        .await
        .unwrap();
    publish_next(
        &repo,
        root,
        None,
        history.oids[0].clone(),
        Some(history.pack),
    )
    .await;
    repo.open(&server, &CancellationToken::new()).await.unwrap();
    repo.pinned.lock().await.as_mut().unwrap().checked =
        Some(Instant::now() - Duration::from_secs(3));
    reads.store(0, Ordering::Relaxed);

    repo.open(&server, &CancellationToken::new()).await.unwrap();

    // One root, one ref head, and the bounded derived-index pointer; immutable
    // capsule/pack bodies must not be read again for an unchanged state.
    assert_eq!(reads.load(Ordering::Relaxed), 3);
    close(&server).await;
}

#[tokio::test]
async fn browse_refresh_observes_ref_publication_without_a_root_change() {
    let server = fixture_without_cells().await;
    let repo = repository(&server);
    let history = git_history(2);
    let root = crab_write::capsule_protocol::open_root(&repo.layout)
        .await
        .unwrap();
    let (root, old) = publish_next(
        &repo,
        root,
        None,
        history.oids[0].clone(),
        Some(history.pack),
    )
    .await;
    repo.open(&server, &CancellationToken::new()).await.unwrap();
    let root_etag = root.etag().clone();
    publish_next(&repo, root, Some(old), history.oids[1].clone(), None).await;
    assert_eq!(
        crab_write::capsule_protocol::open_root(&repo.layout)
            .await
            .unwrap()
            .etag(),
        &root_etag
    );
    repo.pinned.lock().await.as_mut().unwrap().checked =
        Some(Instant::now() - Duration::from_secs(3));

    let current = repo.open(&server, &CancellationToken::new()).await.unwrap();

    assert_eq!(
        current.git.refs().head.as_ref().unwrap().target.to_string(),
        history.oids[1]
    );
    close(&server).await;
}

#[tokio::test]
async fn browse_refresh_does_not_hide_new_invalid_indexes_behind_cached_state() {
    let server = fixture_without_cells().await;
    let repo = repository(&server);
    let history = git_history(1);
    let root = crab_write::capsule_protocol::open_root(&repo.layout)
        .await
        .unwrap();
    publish_next(
        &repo,
        root,
        None,
        history.oids[0].clone(),
        Some(history.pack),
    )
    .await;
    repo.open(&server, &CancellationToken::new()).await.unwrap();
    repo.store
        .put_overwrite(
            &repo.layout.capsule_browse_indexes_path(),
            Bytes::from_static(b"invalid"),
        )
        .await
        .unwrap();
    repo.pinned.lock().await.as_mut().unwrap().checked =
        Some(Instant::now() - Duration::from_secs(3));

    let result = repo.open(&server, &CancellationToken::new()).await;

    assert!(matches!(result, Err(crate::Error::BrowseIndexes(_))));
    close(&server).await;
}

#[tokio::test]
async fn capsule_browse_builder_does_not_repeat_raw_history_authorization() {
    let mut request_counts = Vec::new();
    for count in [64, 128, 256] {
        let server = fixture_without_cells().await;
        let reads = Arc::new(AtomicUsize::new(0));
        {
            let mut current = server.repositories.current.write().unwrap();
            let repo = Arc::get_mut(current.by_name.values_mut().next().unwrap()).unwrap();
            let observed = Arc::clone(&reads);
            repo.store = repo
                .store
                .clone()
                .with_read_request_observer(Arc::new(move |kind| {
                    if matches!(
                        kind,
                        crab_storage::StorageReadKind::Get | crab_storage::StorageReadKind::Range
                    ) {
                        observed.fetch_add(1, Ordering::Relaxed);
                    }
                }));
            repo.layout = StoreLayout::new(repo.store.clone(), "maintenance".into());
        }
        let repo = repository(&server);
        let history = git_history(count);
        let root = crab_write::capsule_protocol::open_root(&repo.layout)
            .await
            .unwrap();
        publish_next(
            &repo,
            root,
            None,
            history.oids[count - 1].clone(),
            Some(history.pack),
        )
        .await;
        // Graph construction reads verified commits in bounded batches. Path
        // comparison must use that graph, rather than spending a history walk
        // budget again to authorize each old commit and its first parent.
        let options = RepositoryOptions::new(
            server.options.object_limits(),
            crab_remote_git::OperationLimits {
                max_history_commits: 1,
                ..server.options.operation_limits()
            },
        )
        .unwrap();
        reads.store(0, Ordering::Relaxed);
        let result = crab_remote::browse_indexes::ensure(
            &repo.layout,
            &repo.identity,
            Arc::clone(&server.runtime),
            options,
            2 * 1024 * 1024 * 1024,
            &CancellationToken::new(),
        )
        .await;
        close(&server).await;
        eprintln!(
            "capsule browse builder: {count} commits, {} GET/range reads",
            reads.load(Ordering::Relaxed)
        );
        assert!(result.is_ok(), "{count} commits: {result:?}");
        request_counts.push(reads.load(Ordering::Relaxed));
    }
    assert!(
        request_counts.windows(2).all(|pair| pair[1] <= pair[0] * 2),
        "origin work grows faster than history: {request_counts:?}"
    );
}

#[tokio::test]
async fn browse_refresh_attaches_new_indexes_without_a_ref_change() {
    let server = fixture_without_cells().await;
    let repo = repository(&server);
    let history = git_history(1);
    let root = crab_write::capsule_protocol::open_root(&repo.layout)
        .await
        .unwrap();
    publish_next(
        &repo,
        root,
        None,
        history.oids[0].clone(),
        Some(history.pack),
    )
    .await;
    repo.open(&server, &CancellationToken::new()).await.unwrap();
    crab_remote::browse_indexes::ensure(
        &repo.layout,
        &repo.identity,
        Arc::clone(&server.runtime),
        server.options,
        2 * 1024 * 1024 * 1024,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    repo.pinned.lock().await.as_mut().unwrap().checked =
        Some(Instant::now() - Duration::from_secs(3));

    let current = repo.open(&server, &CancellationToken::new()).await.unwrap();

    assert!(current.git.commit_graph_available());
    close(&server).await;
}

fn enable_catalog_readiness(server: &mut Arc<Server>) {
    let store = repository(server).store.clone();
    let server = Arc::get_mut(server).unwrap();
    server.catalog = Some(CatalogStore::new(crate::storage_root::StorageRoot::memory(
        store.clone(),
        "catalog",
    )));
    server.catalog_healthy.store(true, Ordering::Release);
    server.node_healthy.store(true, Ordering::Release);
}

#[tokio::test]
async fn readiness_opens_every_repository_before_admitting_traffic() {
    let mut server = fixture().await;
    enable_catalog_readiness(&mut server);

    let response = crate::server::management::router(Arc::clone(&server), Router::new())
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

    let response = crate::server::management::router(Arc::clone(&server), Router::new())
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

    let response = crate::server::management::router(Arc::clone(&server), Router::new())
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
async fn integrity_proof_is_reported_separately_from_readiness() {
    let server = fixture_without_cells().await;
    let management = crate::server::management::router(Arc::clone(&server), Router::new());
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

    crate::repository::maintenance::run(
        repo.layout.clone(),
        Arc::clone(&server.maintenance_admission),
        None,
        CancellationToken::new(),
        crate::repository::maintenance::Pass::BackgroundMaintenance,
        None,
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
    for sequence in 32..34 {
        let result = publish_next(&repo, base, old, history.oids[sequence].clone(), None).await;
        base = result.0;
        old = Some(result.1);
    }
    let checkpointed = crab_remote::checkpoint::publish_capsule_checkpoint_from_view(
        &repo.layout,
        &captured,
        1,
        2 * 1024 * 1024 * 1024,
        &tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(checkpointed.published);
    base = crab_write::capsule_protocol::open_root(&repo.layout)
        .await
        .unwrap();
    assert_eq!(base.record().root().refs(), captured.refs());
    assert_eq!(
        base.record().root().compacted_ref_transactions(),
        captured.visible_ref_transactions()
    );

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
    assert!(view.layered_checkpoint().is_some());
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
    for sequence in 0..crate::repository::maintenance::FOREGROUND_CAPSULE_THRESHOLD as usize {
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
        crate::repository::maintenance::FOREGROUND_CAPSULE_THRESHOLD
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

#[derive(Default)]
struct ReadBytes(std::sync::atomic::AtomicU64);

impl crab_storage::StorageObserver for ReadBytes {
    fn started(&self, _: crab_storage::StorageOperation) {}

    fn finished(&self, observation: crab_storage::StorageObservation) {
        self.0.fetch_add(observation.bytes_read, Ordering::Relaxed);
    }
}

#[tokio::test]
async fn browse_revalidation_keeps_unchanged_capsule_payloads_cached() {
    let observed = Arc::new(ReadBytes::default());
    let store = Store::new(Arc::new(object_store::memory::InMemory::new()))
        .with_storage_observer(observed.clone());
    let server = fixture_with_store(store).await;
    let repo = repository(&server);
    let body = (0_u32..32 * 1024)
        .map(|index| blake3::hash(&index.to_le_bytes()).as_bytes()[0])
        .collect::<Vec<_>>();
    let history = crate::test_support::git::history_with_blob(&body);
    let root = crab_write::capsule_protocol::open_root(&repo.layout)
        .await
        .unwrap();
    publish_next(
        &repo,
        root,
        None,
        history.oids[0].clone(),
        Some(history.pack),
    )
    .await;
    repo.open(&server, &server.cancellation).await.unwrap();
    repo.pinned.lock().await.as_mut().unwrap().checked =
        Some(Instant::now() - Duration::from_secs(3));
    observed.0.store(0, Ordering::Relaxed);

    repo.open(&server, &server.cancellation).await.unwrap();
    let revalidated_bytes = observed.0.load(Ordering::Relaxed);
    repo.invalidate().await;
    observed.0.store(0, Ordering::Relaxed);
    repo.open(&server, &server.cancellation).await.unwrap();
    let invalidated_bytes = observed.0.load(Ordering::Relaxed);
    close(&server).await;
    assert!(
        revalidated_bytes < body.len() as u64 && invalidated_bytes < body.len() as u64,
        "unchanged open reread {revalidated_bytes} bytes after expiry and {invalidated_bytes} after invalidation"
    );
}

#[tokio::test]
async fn browse_open_leaves_git_pack_bodies_in_the_origin() {
    let observed = Arc::new(ReadBytes::default());
    let store = Store::new(Arc::new(object_store::memory::InMemory::new()))
        .with_storage_observer(observed.clone());
    let server = fixture_with_store(store).await;
    let repo = repository(&server);
    let body = (0_u32..32 * 1024)
        .map(|index| blake3::hash(&index.to_le_bytes()).as_bytes()[0])
        .collect::<Vec<_>>();
    let history = crate::test_support::git::history_with_blob(&body);
    let root = crab_write::capsule_protocol::open_root(&repo.layout)
        .await
        .unwrap();
    publish_next(
        &repo,
        root,
        None,
        history.oids[0].clone(),
        Some(history.pack),
    )
    .await;
    observed.0.store(0, Ordering::Relaxed);

    let opened = repo.open(&server, &server.cancellation).await.unwrap();
    let open_bytes = observed.0.load(Ordering::Relaxed);
    let operation = opened
        .git
        .operation(
            crab_remote_git::OperationKind::Content,
            &server.cancellation,
        )
        .await
        .unwrap();
    let content = async {
        let snapshot = opened
            .git
            .snapshot(
                &crab_remote_git::Revision::parse("refs/heads/main")?,
                &operation,
            )
            .await?;
        snapshot
            .read_blob(
                &crab_remote_git::GitPath::new(b"asset".to_vec())?,
                &operation,
            )
            .await
    }
    .await;
    let content = operation.finish(content).await.unwrap();
    close(&server).await;
    assert!(open_bytes < body.len() as u64 && content.bytes.as_ref() == body.as_slice());
}

#[tokio::test]
async fn attribution_schedules_repair_for_a_corrupt_pinned_descriptor() {
    let server = fixture().await;
    let repo = repository(&server);
    let history = git_history(2);
    let root = crab_write::capsule_protocol::open_root(&repo.layout)
        .await
        .unwrap();
    publish_next(
        &repo,
        root,
        None,
        history.oids[1].clone(),
        Some(history.pack),
    )
    .await;
    crab_remote::browse_indexes::ensure(
        &repo.layout,
        &repo.identity,
        Arc::clone(&server.runtime),
        server.options,
        2 * 1024 * 1024 * 1024,
        &server.cancellation,
    )
    .await
    .unwrap();
    let opened = repo.open(&server, &server.cancellation).await.unwrap();
    let hash = opened.source.path_state_hash.as_deref().unwrap();
    repo.store
        .put_overwrite(
            &repo.layout.bulk_manifest_path("path-state", hash),
            Bytes::from_static(b"{"),
        )
        .await
        .unwrap();

    let response = router(Arc::clone(&server))
        .oneshot(
            Request::builder()
                .uri("/api/repos/team/repo/tree-attribution")
                .header("host", "localhost:8788")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let repair_scheduled = repo.maintenance.lock().await.worker.is_some();
    close(&server).await;
    assert_eq!(
        (response.status(), repair_scheduled),
        (StatusCode::SERVICE_UNAVAILABLE, true)
    );
}
