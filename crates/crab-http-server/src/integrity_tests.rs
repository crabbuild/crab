use bytes::Bytes;
use sha2::Digest as _;

use super::*;

#[tokio::test]
async fn scrub_reports_dependency_loss_without_erasing_the_last_complete_proof() {
    let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
    let layout = StoreLayout::new(store.clone(), "integrity".into());
    let content = Bytes::from_static(b"content");
    let oid: [u8; 32] = sha2::Sha256::digest(&content).into();
    let pointer = crab_git::LfsPointer {
        oid,
        size: content.len() as u64,
        extensions: Vec::new(),
    };
    crate::test_git::publish_blob(&layout, &pointer.serialize()).await;
    let lfs = crab_lfs::LfsObjectStore::new(store, layout.repo_prefix());
    lfs.put(&oid, content).await.unwrap();
    let status = Status::default();
    let admission = Arc::new(Semaphore::new(1));
    let cancellation = CancellationToken::new();

    scrub_target(&layout, &status, Arc::clone(&admission), &cancellation)
        .await
        .unwrap();
    let complete = status.snapshot();
    assert_eq!(complete.state, State::Complete);
    assert_eq!(
        complete
            .last_complete
            .as_ref()
            .unwrap()
            .reachable_lfs_objects,
        1
    );

    lfs.delete(&oid).await.unwrap();
    assert!(matches!(
        scrub_target(&layout, &status, admission, &cancellation).await,
        Err(Error::Read(crab_read::ReadError::Lfs(
            crab_lfs::LfsError::ObjectMissing { .. }
        )))
    ));
    let failed = status.snapshot();
    assert_eq!(failed.state, State::Failed);
    assert!(failed.error.is_some());
    assert_eq!(failed.last_complete, complete.last_complete);
}

#[tokio::test]
async fn deployment_report_prevents_duplicate_scrubs_and_expires_fail_closed() {
    let mut server = crate::server::maintenance_tests::fixture_without_cells().await;
    let repository = server
        .repositories
        .get(&("team".into(), "repo".into()))
        .unwrap();
    let store = repository.store.clone();
    let layout = repository.layout.clone();
    let content = Bytes::from_static(b"deployment content");
    let oid: [u8; 32] = sha2::Sha256::digest(&content).into();
    let pointer = crab_git::LfsPointer {
        oid,
        size: content.len() as u64,
        extensions: Vec::new(),
    };
    crate::test_git::publish_blob_at_current(&layout, &pointer.serialize()).await;
    let lfs = crab_lfs::LfsObjectStore::new(store.clone(), layout.repo_prefix());
    lfs.put(&oid, content).await.unwrap();
    drop(repository);
    let root = crate::storage_root::StorageRoot::memory(store, "integrity-server");
    Arc::get_mut(&mut server).unwrap().catalog =
        Some(crate::catalog::CatalogStore::new(root.clone()));
    let mut locks = crab_coordination::PushLockAcquireContext::new(root.store.inner().clone());

    assert_eq!(
        coordinated_cycle(&server, &root, &mut locks).await.unwrap(),
        SCRUB_INTERVAL
    );
    let repository = server
        .repositories
        .get(&("team".into(), "repo".into()))
        .unwrap();
    assert_eq!(repository.integrity.snapshot().state, State::Complete);
    let (body, stale_etag) = read_report(&root).await.unwrap().unwrap();
    let report = decode_report(&body).unwrap();
    store_report(&root, &report, Some(stale_etag.clone()))
        .await
        .unwrap();
    assert!(matches!(
        store_report(&root, &report, Some(stale_etag)).await,
        Err(Error::Storage(
            crab_storage::StorageError::StateConflict { .. }
        ))
    ));
    lfs.delete(&oid).await.unwrap();
    repository.integrity.replace(Status::default().snapshot());

    assert!(coordinated_cycle(&server, &root, &mut locks).await.unwrap() > REPORT_RETRY_INTERVAL);
    assert_eq!(repository.integrity.snapshot().state, State::Complete);
    let (body, etag) = read_report(&root).await.unwrap().unwrap();
    let mut stale = decode_report(&body).unwrap();
    stale.generated_at_unix = 0;
    store_report(&root, &stale, Some(etag)).await.unwrap();
    let held = crab_coordination::PushLock::acquire_internal(
        root.store.inner(),
        &root.prefix,
        SCRUB_RESOURCE,
        crab_coordination::DEFAULT_PUSH_LOCK_TTL,
    )
    .await
    .unwrap();
    let mut contender = crab_coordination::PushLockAcquireContext::new(root.store.inner().clone());
    assert_eq!(
        coordinated_cycle(&server, &root, &mut contender)
            .await
            .unwrap(),
        REPORT_RETRY_INTERVAL
    );
    assert_eq!(repository.integrity.snapshot().state, State::Complete);
    held.release().await.unwrap();

    assert_eq!(
        coordinated_cycle(&server, &root, &mut contender)
            .await
            .unwrap(),
        SCRUB_INTERVAL
    );
    assert_eq!(repository.integrity.snapshot().state, State::Failed);
    assert_eq!(
        load_report(&root).await.unwrap().unwrap().repositories[0]
            .proof
            .state,
        State::Failed
    );
    crate::server::maintenance_tests::close(&server).await;
}
