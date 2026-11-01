//! Checkpoint snapshots must preserve data without deep-copying namespace values.
use super::*;
use crate::mutation::tests::capsule_fixture;

#[tokio::test]
async fn scheduled_checkpoint_shares_immutable_values_and_preserves_its_generation() {
    let (repository, runtime, cancel) = capsule_fixture().await;
    let scheduler = Arc::new(CheckpointScheduler::new(cancel, Metrics::new().unwrap()));
    let _blocked = Arc::clone(&scheduler.permits)
        .acquire_many_owned(CHECKPOINT_PUBLICATION_CONCURRENCY as u32)
        .await
        .unwrap();
    let oid = ObjectId::empty_blob(gix_hash::Kind::Sha1);
    let mut manifest = attributes::Manifest::empty_complete();
    for path in ["namespace/keep", "namespace/remove", "namespace/untouched"] {
        manifest.update(
            path.to_owned(),
            Some(attributes::ObjectAttributes::new(
                oid,
                "original".to_owned(),
                4,
                123,
                attributes::PutAttributes {
                    metadata: BTreeMap::from([("nested".to_owned(), "retained".to_owned())]),
                    ..Default::default()
                },
            )),
        );
    }
    scheduler.schedule(&repository, "refs/heads/main", oid, &manifest);
    // This current-thread runtime cannot poll the queued owner until we await.
    let snapshot = scheduler.refs.lock().unwrap()["repo\0refs/heads/main"]
        .pending
        .as_ref()
        .unwrap()
        .manifest
        .clone();
    let shared_before = std::ptr::eq(
        manifest.object("namespace/keep", oid).unwrap(),
        snapshot.object("namespace/keep", oid).unwrap(),
    );
    manifest.update(
        "namespace/keep".to_owned(),
        Some(attributes::ObjectAttributes::new(
            oid,
            "replacement".to_owned(),
            7,
            456,
            Default::default(),
        )),
    );
    manifest.update("namespace/remove".to_owned(), None);
    assert_eq!(
        snapshot.object("namespace/keep", oid).unwrap().etag,
        "original"
    );
    assert_eq!(
        snapshot.object("namespace/keep", oid).unwrap().metadata["nested"],
        "retained"
    );
    assert!(snapshot.object("namespace/remove", oid).is_some());
    assert_eq!(
        manifest.object("namespace/keep", oid).unwrap().etag,
        "replacement"
    );
    assert!(manifest.object("namespace/remove", oid).is_none());
    assert!(std::ptr::eq(
        manifest.object("namespace/untouched", oid).unwrap(),
        snapshot.object("namespace/untouched", oid).unwrap(),
    ));
    scheduler.shutdown().await;
    assert_eq!(
        scheduler.manifest_bytes.available_permits(),
        MAX_WARM_BRANCH_STATE_BYTES
    );
    runtime.shutdown().await;
    assert!(
        shared_before,
        "scheduling a checkpoint must share immutable values instead of deep-copying every object"
    );
}
