use super::*;
use std::time::Duration;

use crate::server::{
    MaintenanceSchedule, MaintenanceTrigger, Repository, Server, maintenance_tests,
};
use crab_metadata::capsule_protocol::{
    BrowseIndexes, Capsule, CapsuleRefEdit, CapsuleTransaction, load_root,
};

async fn indexed_fixture() -> (Arc<Server>, Arc<Repository>) {
    let server = maintenance_tests::fixture_without_cells().await;
    let repository = server
        .repositories
        .get(&("team".into(), "repo".into()))
        .unwrap();
    crate::test_git::publish_blob_at_current(&repository.layout, b"source probe fixture\n").await;
    crab_remote::browse_indexes::ensure(
        &repository.layout,
        &repository.identity,
        Arc::clone(&server.runtime),
        server.options,
        2 * 1024 * 1024 * 1024,
        &server.cancellation,
    )
    .await
    .unwrap();
    (server, repository)
}

#[tokio::test]
async fn source_probe_observes_ref_only_publication_and_same_state_index_changes() {
    let (server, repository) = indexed_fixture().await;
    let first = current_source(&repository, &server.cancellation)
        .await
        .unwrap();
    let root = load_root(&repository.layout).await.unwrap();
    let root_etag = root.etag().clone();
    let view = open_view(&repository.layout).await.unwrap();
    let oid = view.refs()["refs/heads/main"].clone();
    let transaction = CapsuleTransaction::new(
        root.record().digest(),
        vec![CapsuleRefEdit::new(
            "refs/tags/probe",
            None,
            Some(oid),
            None,
        )],
    )
    .unwrap();
    let capsule = Capsule::build(&transaction, Vec::new(), Vec::new()).unwrap();
    crab_write::capsule_protocol::publish(&repository.layout, root, &transaction, &capsule)
        .await
        .unwrap();
    assert_eq!(
        load_root(&repository.layout).await.unwrap().etag(),
        &root_etag
    );
    repository
        .projection_source
        .lock()
        .await
        .as_mut()
        .unwrap()
        .checked -= Duration::from_secs(3);
    let changed = current_source(&repository, &server.cancellation)
        .await
        .unwrap();
    assert_ne!(first.source_token, changed.source_token);
    assert!(changed.path_state_hash.is_none());
    crab_remote::browse_indexes::ensure(
        &repository.layout,
        &repository.identity,
        Arc::clone(&server.runtime),
        server.options,
        2 * 1024 * 1024 * 1024,
        &server.cancellation,
    )
    .await
    .unwrap();
    repository
        .projection_source
        .lock()
        .await
        .as_mut()
        .unwrap()
        .checked -= Duration::from_secs(3);
    let complete = current_source(&repository, &server.cancellation)
        .await
        .unwrap();
    assert_eq!(changed.journal_state_digest, complete.journal_state_digest);
    assert!(complete.path_state_hash.is_some());
    let partial = BrowseIndexes::new(
        complete.journal_state_digest.clone(),
        complete.commit_graph_hash.clone().unwrap(),
        None,
    )
    .unwrap();
    repository
        .store
        .put_overwrite(
            &repository.layout.capsule_browse_indexes_path(),
            partial.encode().unwrap(),
        )
        .await
        .unwrap();
    repository
        .projection_source
        .lock()
        .await
        .as_mut()
        .unwrap()
        .checked -= Duration::from_secs(3);
    let partial = current_source(&repository, &server.cancellation)
        .await
        .unwrap();
    maintenance_tests::close(&server).await;
    assert_eq!(
        (partial.journal_state_digest, partial.path_state_hash),
        (complete.journal_state_digest, None)
    );
}

#[tokio::test]
async fn source_probe_revalidates_corruption_after_invalidation_or_scrub_expiry() {
    for invalidate in [false, true] {
        let (server, repository) = indexed_fixture().await;
        let source = current_source(&repository, &server.cancellation)
            .await
            .unwrap();
        let path = repository
            .layout
            .bulk_manifest_path("path-state", source.path_state_hash.as_deref().unwrap());
        repository.store.delete(&path).await.unwrap();
        if invalidate {
            repository.invalidate().await;
        } else {
            repository
                .projection_source
                .lock()
                .await
                .as_mut()
                .unwrap()
                .verified -= crate::integrity::SCRUB_INTERVAL;
        }
        let result = current_source(&repository, &server.cancellation).await;
        maintenance_tests::close(&server).await;
        assert!(
            matches!(
                result,
                Err(crate::Error::Remote(crab_remote_git::Error::Corrupt {
                    stage: crab_remote_git::CorruptionStage::PathState
                }))
            ),
            "invalidate={invalidate}"
        );
    }
}

#[tokio::test]
async fn source_probe_honors_cancellation_and_preserves_invalid_record_errors() {
    let (server, repository) = indexed_fixture().await;
    current_source(&repository, &server.cancellation)
        .await
        .unwrap();
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(matches!(
        current_source(&repository, &cancelled).await,
        Err(crate::Error::Remote(crab_remote_git::Error::Cancelled))
    ));
    repository
        .store
        .put_overwrite(
            &repository.layout.capsule_browse_indexes_path(),
            bytes::Bytes::from_static(b"invalid"),
        )
        .await
        .unwrap();
    repository
        .projection_source
        .lock()
        .await
        .as_mut()
        .unwrap()
        .checked -= Duration::from_secs(3);
    let result = current_source(&repository, &server.cancellation).await;
    maintenance_tests::close(&server).await;
    assert!(matches!(result, Err(crate::Error::BrowseIndexes(_))));
}

#[tokio::test]
async fn completed_maintenance_reuses_exact_source_until_repair() {
    let (server, repository) = indexed_fixture().await;
    let source = current_source(&repository, &server.cancellation)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if repository
                .schedule_maintenance(&server, MaintenanceTrigger::Poll(&source))
                .await
                .unwrap()
                == MaintenanceSchedule::Completed
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        repository
            .schedule_maintenance(&server, MaintenanceTrigger::Poll(&source))
            .await
            .unwrap(),
        MaintenanceSchedule::Completed
    );
    assert_eq!(
        repository
            .schedule_maintenance(&server, MaintenanceTrigger::Repair)
            .await
            .unwrap(),
        MaintenanceSchedule::Started
    );
    maintenance_tests::close(&server).await;
}

#[tokio::test]
async fn contended_generation_owner_cannot_prove_completed_maintenance() {
    let (server, repository) = indexed_fixture().await;
    let lease = crab_coordination::PushLock::acquire_internal(
        repository.store.inner(),
        repository.layout.repo_prefix(),
        crab_coordination::GIT_GENERATION_OWNER_RESOURCE,
        Duration::from_secs(60),
    )
    .await
    .unwrap();
    let ready = crate::maintenance::run(
        repository.layout.clone(),
        Arc::clone(&server.maintenance_admission),
        None,
        server.cancellation.clone(),
        crate::maintenance::Pass::BackgroundMaintenance,
        Some(crate::maintenance::ProjectionContext {
            repository_id: repository.id,
            router: None,
            metrics: server.metrics.clone(),
            identity: repository.identity.clone(),
            runtime: Arc::clone(&server.runtime),
            options: server.options,
        }),
    )
    .await
    .unwrap();
    lease.release().await.unwrap();
    maintenance_tests::close(&server).await;
    assert!(ready.is_none());
}
