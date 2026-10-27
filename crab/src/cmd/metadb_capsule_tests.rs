use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bytes::Bytes;
use crab_metadata::capsule_protocol::{
    Capsule, CapsuleGitPack, CapsuleRefEdit, CapsuleSection, CapsuleSectionKind,
    CapsuleTransaction, CapsuleVisibilityDelta,
};
use crab_metadata::git_visibility::GitVisibilityEdit;
use crab_storage::{StorageObservation, StorageObserver, StorageOperation, Store, StoreLayout};
use object_store::{ObjectStoreExt, memory::InMemory};
use tokio_util::sync::CancellationToken;

use super::{CrabError, OutputMode, diagnose_capsule, run_capsule_rebuild_in};

const LIMIT: u64 = 8 * 1024 * 1024;

#[derive(Default)]
struct ObjectRequests(AtomicUsize);

impl StorageObserver for ObjectRequests {
    fn started(&self, _operation: StorageOperation) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }

    fn finished(&self, _observation: StorageObservation) {}
}

async fn blob_repository(body: &[u8]) -> StoreLayout<Store> {
    let layout = StoreLayout::new(
        Store::new(Arc::new(InMemory::new())),
        "org/metadata-integrity".to_owned(),
    );
    let root =
        crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
            .await
            .unwrap();
    let kind = gix_object::Kind::Blob;
    let oid = crab_remote::objects::object_id(kind, body)
        .unwrap()
        .to_string();
    let mut bytes = Vec::new();
    crab_git::pack_writer::write_pack(
        &mut bytes,
        std::iter::once(Ok((kind, body.len() as u64, body))),
        LIMIT,
        || false,
    )
    .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source.pack");
    std::fs::write(&source, &bytes).unwrap();
    let indexed = crab_git::pack::install_pack_file_from_path(
        &directory.path().join("indexed"),
        &source,
        blake3::hash(&bytes).to_hex().as_ref(),
        LIMIT,
        true,
    )
    .unwrap();
    let checksum = gix_hash::ObjectId::from_hex(indexed.git_sha1.as_bytes()).unwrap();
    let pack = CapsuleGitPack::new(
        Bytes::from(bytes),
        Bytes::from(std::fs::read(indexed.idx_path).unwrap()),
        Bytes::from(std::fs::read(indexed.rev_path).unwrap()),
        Bytes::from(crab_git::pack_locator::encode_pack_kind_metadata(checksum, &[kind]).unwrap()),
        indexed.git_sha1,
        1,
    )
    .unwrap();
    let name = "refs/tags/content";
    let transaction = CapsuleTransaction::new(
        root.record().digest(),
        vec![CapsuleRefEdit::new(name, None, Some(oid.clone()), None)],
    )
    .unwrap();
    let visibility = CapsuleVisibilityDelta::new(BTreeMap::from([(
        name.to_owned(),
        GitVisibilityEdit::from_replacement_objects(None, oid.clone(), vec![oid]),
    )]))
    .unwrap();
    let capsule = Capsule::build(
        &transaction,
        vec![pack],
        vec![CapsuleSection::new(
            CapsuleSectionKind::VisibilityDelta,
            visibility.encode().unwrap(),
        )],
    )
    .unwrap();
    crab_write::capsule_protocol::publish(&layout, root, &transaction, &capsule)
        .await
        .unwrap();
    layout
}

#[tokio::test]
async fn layered_diagnosis_and_rebuild_verify_nonempty_repository_without_rewriting_it() {
    let layout = blob_repository(&[0xa5; 4096]).await;
    let cancel = CancellationToken::new();
    assert!(
        crab_remote::checkpoint::publish_capsule_checkpoint(&layout, 0, LIMIT, &cancel)
            .await
            .unwrap()
            .published
    );
    let root = crab_write::capsule_protocol::open_root(&layout)
        .await
        .unwrap();
    let before = root.record().digest().to_owned();
    assert_eq!(root.record().root().checkpoint().unwrap().format(), 9);
    let diagnosis = diagnose_capsule(&layout, root.clone(), true, &cancel)
        .await
        .unwrap();
    let deep = diagnosis.deep_integrity.unwrap();
    assert!(deep.git_closure_verified);
    assert_eq!(deep.git_packs, 1);
    assert_eq!(deep.pointer_objects_read, 0);
    run_capsule_rebuild_in(
        &layout,
        root,
        layout.repo_prefix(),
        OutputMode::Text,
        &cancel,
    )
    .await
    .unwrap();
    let after = crab_write::capsule_protocol::open_root(&layout)
        .await
        .unwrap();
    assert_eq!(after.record().digest(), before);
    assert!(matches!(
        layout.store().head(&layout.manifest_path()).await,
        Err(crab_storage::StorageError::NotFound { .. })
    ));
}

#[tokio::test]
async fn rebuild_rejects_missing_reachable_file_before_publishing_checkpoint() {
    let pointer = crab_types::pointer::Pointer {
        file_hash: [7; 32],
        size: 4096,
        shard_hint: None,
    };
    let layout = blob_repository(&pointer.serialize()).await;
    let root = crab_write::capsule_protocol::open_root(&layout)
        .await
        .unwrap();
    let before = root.record().digest().to_owned();
    assert!(root.record().root().checkpoint().is_none());
    let result = run_capsule_rebuild_in(
        &layout,
        root,
        layout.repo_prefix(),
        OutputMode::Text,
        &CancellationToken::new(),
    )
    .await;
    assert!(matches!(
        result,
        Err(CrabError::CorruptObject { reason, .. }) if reason.contains("absent from the catalog")
    ));
    let after = crab_write::capsule_protocol::open_root(&layout)
        .await
        .unwrap();
    assert_eq!(after.record().digest(), before);
}

#[tokio::test]
async fn layered_metadata_verification_preserves_missing_source_failure() {
    let layout = blob_repository(b"missing source fixture").await;
    let cancel = CancellationToken::new();
    crab_remote::checkpoint::publish_capsule_checkpoint(&layout, 0, LIMIT, &cancel)
        .await
        .unwrap();
    let root = crab_write::capsule_protocol::open_root(&layout)
        .await
        .unwrap();
    let before = root.record().digest().to_owned();
    let checkpoint = crab_metadata::capsule_protocol::load_layered_checkpoint(
        &layout,
        root.record().root().checkpoint().unwrap(),
        u64::MAX,
    )
    .await
    .unwrap();
    let missing = layout.capsule_path(checkpoint.sources()[0].object_hash());
    layout.store().delete(&missing).await.unwrap();
    let diagnosis = diagnose_capsule(&layout, root.clone(), true, &cancel).await;
    assert!(matches!(
        diagnosis,
        Err(CrabError::NotFound { path }) if path == missing.as_ref()
    ));
    let rebuild = run_capsule_rebuild_in(
        &layout,
        root,
        layout.repo_prefix(),
        OutputMode::Text,
        &cancel,
    )
    .await;
    assert!(matches!(
        rebuild,
        Err(CrabError::NotFound { path }) if path == missing.as_ref()
    ));
    let after = crab_write::capsule_protocol::open_root(&layout)
        .await
        .unwrap();
    assert_eq!(after.record().digest(), before);
}

#[tokio::test]
async fn layered_metadata_verification_rejects_corruption_outside_git_pack_ranges() {
    let layout = blob_repository(b"intact Git bytes inside a corrupt capsule").await;
    let cancel = CancellationToken::new();
    crab_remote::checkpoint::publish_capsule_checkpoint(&layout, 0, LIMIT, &cancel)
        .await
        .unwrap();
    let root = crab_write::capsule_protocol::open_root(&layout)
        .await
        .unwrap();
    let before = root.record().digest().to_owned();
    let checkpoint = crab_metadata::capsule_protocol::load_layered_checkpoint(
        &layout,
        root.record().root().checkpoint().unwrap(),
        u64::MAX,
    )
    .await
    .unwrap();
    let source = &checkpoint.sources()[0];
    for member in source.members() {
        for range in [
            member.pack(),
            member.index(),
            member.reverse_index(),
            member.locator(),
        ] {
            assert!(range.offset() > 0);
        }
    }
    let path = layout.capsule_path(source.object_hash());
    let mut body = layout
        .store()
        .get_with_etag(&path)
        .await
        .unwrap()
        .0
        .to_vec();
    body[0] ^= 1;
    // Bypass the storage owner's immutable-create protection to simulate bit rot.
    layout
        .store()
        .inner()
        .put(&path, Bytes::from(body).into())
        .await
        .unwrap();

    // Range intake still succeeds: deep verification must additionally reject
    // corruption in the immutable source's framing, not just its Git members.
    let database = tempfile::tempdir().unwrap();
    crab_git::initialize_bare_git_dir(database.path()).unwrap();
    crab_read::capsule_protocol::install_layered_checkpoint(
        &checkpoint,
        &layout,
        database.path(),
        LIMIT,
    )
    .await
    .unwrap();
    assert!(matches!(
        diagnose_capsule(&layout, root.clone(), true, &cancel).await,
        Err(CrabError::CorruptObject { .. })
    ));
    assert!(matches!(
        run_capsule_rebuild_in(
            &layout,
            root,
            layout.repo_prefix(),
            OutputMode::Text,
            &cancel,
        )
        .await,
        Err(CrabError::CorruptObject { .. })
    ));
    let after = crab_write::capsule_protocol::open_root(&layout)
        .await
        .unwrap();
    assert_eq!(after.record().digest(), before);
}

#[tokio::test]
async fn dependency_proof_admits_whole_source_bytes_before_object_reads() {
    let layout = blob_repository(b"whole source byte admission").await;
    let cancel = CancellationToken::new();
    crab_remote::checkpoint::publish_capsule_checkpoint(&layout, 0, LIMIT, &cancel)
        .await
        .unwrap();
    let view = crab_read::capsule_protocol::open_view(
        &layout,
        crab_read::capsule_protocol::CapsuleReadLimits {
            max_capsule_bytes: LIMIT,
            max_frontier_bytes: LIMIT,
        },
    )
    .await
    .unwrap();
    let maximum = view
        .layered_checkpoint()
        .unwrap()
        .sources()
        .iter()
        .map(|source| source.object_size())
        .sum::<u64>()
        - 1;
    let requests = Arc::new(ObjectRequests::default());
    let observed = StoreLayout::new(
        layout
            .store()
            .clone()
            .with_storage_observer(requests.clone()),
        layout.repo_prefix().to_owned(),
    );
    let proof = crab_read::capsule_protocol::verify_reachable_dependencies(
        &observed,
        &view,
        crab_read::capsule_protocol::CapsuleDependencyLimits {
            max_git_bytes: maximum,
            pointer_scan: crate::cmd::fsck_store::CAPSULE_GIT_SCAN_LIMITS,
        },
        &cancel,
    )
    .await;
    assert!(
        matches!(&proof, Err(crab_read::ReadError::CapsuleReadLimit {
        resource: "layered source bodies", maximum: actual,
    }) if *actual == maximum),
        "{proof:?}"
    );
    assert_eq!(requests.0.load(Ordering::Relaxed), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn rebuild_releases_both_fences_for_valid_missing_and_corrupt_authority() {
    use futures_util::TryStreamExt;
    for state in ["valid", "missing", "corrupt"] {
        let inner: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
        let counted = Arc::new(crab_storage::test_support::CountingObjectStore::new(inner));
        let store = crate::storage::Store::new(counted.clone());
        let router = crate::storage::StoreLayout::new(store, format!("rebuild-{state}"));
        let layout = StoreLayout::new(
            router.store().as_storage().clone(),
            router.repo_prefix().to_owned(),
        );
        match state {
            "valid" => {
                crab_write::capsule_protocol::initialize(
                    &layout,
                    &"1".repeat(64),
                    "refs/heads/main",
                )
                .await
                .unwrap();
            }
            "corrupt" => {
                layout
                    .store()
                    .put(
                        &layout.capsule_root_path(),
                        Bytes::from_static(b"corrupt root"),
                    )
                    .await
                    .unwrap();
            }
            _ => {}
        }
        counted.reset();
        let result =
            super::run_rebuild_in(&router, OutputMode::Text, &CancellationToken::new()).await;
        match state {
            "valid" => result.unwrap(),
            "missing" => assert!(matches!(result, Err(CrabError::NotFound { .. }))),
            "corrupt" => assert!(matches!(result, Err(CrabError::CorruptObject { .. }))),
            _ => unreachable!(),
        }
        assert!(counted.requests().iter().all(
            |request| !request.location.contains("file_index_db")
                && !request.location.contains("chunk_index_db")
                && !request.location.ends_with("manifest.json")
        ));
        let objects = router
            .store()
            .inner()
            .list(None)
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        assert!(
            objects
                .iter()
                .all(|object| !object.location.as_ref().contains("file_index_db")
                    && !object.location.as_ref().contains("chunk_index_db"))
        );
        let lock_path = crab_coordination::internal_lock_path(
            router.repo_prefix(),
            crab_coordination::REPOSITORY_MAINTENANCE_RESOURCE,
        )
        .unwrap();
        let body = router
            .store()
            .inner()
            .get(&object_store::path::Path::from(lock_path))
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        let lock: crab_coordination::PushLockPayload = serde_json::from_slice(&body).unwrap();
        assert!(lock.is_released(), "{state}");
        for domain in [router.global_prefix(), router.repo_prefix()] {
            let sweep = crab_coordination::GcFenceLease::acquire_sweep(
                router.store().inner(),
                domain,
                std::time::Duration::from_secs(60),
            )
            .await
            .unwrap();
            sweep.release().await.unwrap();
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn owner_requires_capsule_authority_before_claiming_lease_and_releases_success() {
    for state in ["valid", "missing", "corrupt"] {
        let inner: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
        let counted = Arc::new(crab_storage::test_support::CountingObjectStore::new(inner));
        let router = crate::storage::StoreLayout::new(
            crate::storage::Store::new(counted.clone()),
            format!("owner-{state}"),
        );
        let layout = StoreLayout::new(
            router.store().as_storage().clone(),
            router.repo_prefix().to_owned(),
        );
        match state {
            "valid" => {
                crab_write::capsule_protocol::initialize(
                    &layout,
                    &"1".repeat(64),
                    "refs/heads/main",
                )
                .await
                .unwrap();
            }
            "corrupt" => {
                layout
                    .store()
                    .put(
                        &layout.capsule_root_path(),
                        Bytes::from_static(b"corrupt root"),
                    )
                    .await
                    .unwrap();
            }
            _ => {}
        }
        counted.reset();
        let result = super::run_generation_owner_in(
            &router,
            true,
            30,
            false,
            std::time::Duration::from_secs(60),
            &CancellationToken::new(),
        )
        .await;
        match state {
            "valid" => {
                result.unwrap();
                let next = crab_coordination::PushLock::acquire_internal(
                    router.store().inner(),
                    router.repo_prefix(),
                    crab_coordination::GIT_GENERATION_OWNER_RESOURCE,
                    std::time::Duration::from_secs(60),
                )
                .await
                .unwrap();
                next.release().await.unwrap();
            }
            "missing" => assert!(matches!(result, Err(CrabError::NotFound { .. }))),
            "corrupt" => assert!(matches!(result, Err(CrabError::CorruptObject { .. }))),
            _ => unreachable!(),
        }
        if state != "valid" {
            assert_eq!(counted.put_requests(), 0, "{state}");
            assert!(
                counted
                    .requests()
                    .iter()
                    .all(|request| request.location == layout.capsule_root_path().as_ref())
            );
        }
    }
}

#[test]
fn cache_diagnosis_rejects_corruption_without_replacing_bytes() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("cache.sqlite");
    let bytes = b"this is not SQLite; diagnosis must preserve it";
    std::fs::write(&path, bytes).unwrap();
    assert!(super::cache_stats_for(&path).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
}

#[test]
fn owner_storage_throttle_remains_retryable_with_provider_source() {
    let error = super::map_capsule_owner_error(crab_remote::checkpoint::CheckpointError::Storage(
        crab_storage::StorageError::Throttled {
            retry_after: Some(std::time::Duration::from_secs(7)),
            source: Some(object_store::Error::Generic {
                store: "fixture",
                source: std::io::Error::other("provider throttle").into(),
            }),
        },
    ));
    assert!(super::generation_owner_once_retryable(&error));
    assert!(
        matches!(&error, CrabError::Throttled { retry_after: Some(delay), .. } if *delay == std::time::Duration::from_secs(7))
    );
    let source = std::error::Error::source(&error).unwrap();
    assert!(source.to_string().contains("provider throttle"));
}

#[tokio::test(flavor = "multi_thread")]
async fn rebuild_does_not_capture_authority_before_maintenance_admission() {
    let counted = Arc::new(crab_storage::test_support::CountingObjectStore::new(
        Arc::new(InMemory::new()),
    ));
    let router = crate::storage::StoreLayout::new(
        crate::storage::Store::new(counted.clone()),
        "rebuild-fence-order".to_owned(),
    );
    let layout = StoreLayout::new(
        router.store().as_storage().clone(),
        router.repo_prefix().to_owned(),
    );
    crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
        .await
        .unwrap();
    let cancel = CancellationToken::new();
    let blocker = crate::maintenance::RepositoryMaintenanceLease::acquire(
        router.store(),
        router.global_prefix(),
        router.repo_prefix(),
        &cancel,
    )
    .await
    .unwrap();
    counted.reset();
    let result = super::run_rebuild_in(&router, OutputMode::Text, &cancel).await;
    assert!(matches!(result, Err(CrabError::PushLockHeld { .. })));
    assert!(
        counted
            .requests()
            .iter()
            .all(|request| request.location != layout.capsule_root_path().as_ref())
    );
    layout
        .store()
        .put_overwrite(
            &layout.capsule_root_path(),
            Bytes::from_static(b"changed to corrupt authority while admission blocked"),
        )
        .await
        .unwrap();
    blocker.release().await.unwrap();
    let result = super::run_rebuild_in(&router, OutputMode::Text, &cancel).await;
    assert!(matches!(result, Err(CrabError::CorruptObject { .. })));
}
