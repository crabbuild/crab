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

use super::{CrabError, DbSelector, OutputMode, diagnose_capsule, run_capsule_rebuild_in};

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
    assert_eq!(root.record().root().checkpoint().unwrap().format(), 5);
    let diagnosis = diagnose_capsule(&layout, root.clone(), DbSelector::Both, true, &cancel)
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
        DbSelector::Both,
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
        DbSelector::Both,
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
    )
    .await
    .unwrap();
    let missing = layout.capsule_path(checkpoint.sources()[0].object_hash());
    layout.store().delete(&missing).await.unwrap();
    let diagnosis = diagnose_capsule(&layout, root.clone(), DbSelector::Both, true, &cancel).await;
    assert!(matches!(
        diagnosis,
        Err(CrabError::NotFound { path }) if path == missing.as_ref()
    ));
    let rebuild = run_capsule_rebuild_in(
        &layout,
        root,
        layout.repo_prefix(),
        DbSelector::Both,
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
        diagnose_capsule(&layout, root.clone(), DbSelector::Both, true, &cancel).await,
        Err(CrabError::CorruptObject { .. })
    ));
    assert!(matches!(
        run_capsule_rebuild_in(
            &layout,
            root,
            layout.repo_prefix(),
            DbSelector::Both,
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
