#![expect(clippy::unwrap_used, reason = "test assertions")]

use super::*;
use bytes::Bytes;
use crab_metadata::capsule_protocol::{
    Capsule, CapsuleGitPack, CapsuleRefEdit, CapsuleSection, CapsuleSectionKind,
    CapsuleTransaction, CapsuleVisibilityDelta,
};
use crab_metadata::git_visibility::GitVisibilityEdit;
use object_store::ObjectStoreExt;
use std::sync::Arc;

const LIMIT: u64 = 8 * 1024 * 1024;

async fn publish_blob(
    layout: &CapsuleStoreLayout<crab_storage::Store>,
    name: &str,
    body: &[u8],
) -> String {
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
    let scratch = tempfile::tempdir().unwrap();
    let path = scratch.path().join("source.pack");
    std::fs::write(&path, &bytes).unwrap();
    let indexed = crab_git::pack::install_pack_file_from_path(
        &scratch.path().join("indexed"),
        &path,
        blake3::hash(&bytes).to_hex().as_ref(),
        LIMIT,
        true,
    )
    .unwrap();
    let checksum = gix_hash::ObjectId::from_hex(indexed.git_sha1.as_bytes()).unwrap();
    let kinds = crab_git::pack_locator::encode_pack_kind_metadata(checksum, &[kind]).unwrap();
    let pack = CapsuleGitPack::new(
        Bytes::from(bytes),
        Bytes::from(std::fs::read(indexed.idx_path).unwrap()),
        Bytes::from(std::fs::read(indexed.rev_path).unwrap()),
        Bytes::from(kinds),
        indexed.git_sha1,
        1,
    )
    .unwrap();
    publish_pack(layout, name, pack, &oid).await;
    oid
}

async fn publish_pack(
    layout: &CapsuleStoreLayout<crab_storage::Store>,
    name: &str,
    pack: CapsuleGitPack,
    oid: &str,
) {
    let root = crab_write::capsule_protocol::open_root(layout)
        .await
        .unwrap();
    let transaction = CapsuleTransaction::new(
        root.record().digest(),
        vec![CapsuleRefEdit::new(name, None, Some(oid.to_owned()), None)],
    )
    .unwrap();
    let visibility = CapsuleVisibilityDelta::new(BTreeMap::from([(
        name.to_owned(),
        GitVisibilityEdit::from_replacement_objects(None, oid.to_owned(), vec![oid.to_owned()]),
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
    crab_write::capsule_protocol::publish(layout, root, &transaction, &capsule)
        .await
        .unwrap();
}

async fn empty_fixture() -> CapsuleStoreLayout<crab_storage::Store> {
    let layout = CapsuleStoreLayout::new(
        crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new())),
        "layered-history".to_owned(),
    );
    crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
        .await
        .unwrap();
    layout
}

async fn fixture() -> (CapsuleStoreLayout<crab_storage::Store>, RootSnapshot) {
    let layout = empty_fixture().await;
    for (name, body) in [
        ("refs/tags/first", b"first version".as_slice()),
        ("refs/tags/second", b"second version".as_slice()),
    ] {
        publish_blob(&layout, name, body).await;
        assert!(
            crab_remote::checkpoint::publish_capsule_checkpoint(
                &layout,
                1,
                LIMIT,
                &CancellationToken::new(),
            )
            .await
            .unwrap()
            .published
        );
    }
    let root = crab_write::capsule_protocol::open_root(&layout)
        .await
        .unwrap();
    (layout, root)
}

#[tokio::test]
async fn layered_history_verification_preserves_historical_refs_and_bytes() {
    let (layout, root) = fixture().await;
    let chain = history_chain(&layout, &root).await.unwrap();
    let oldest = chain.last().unwrap();
    assert_eq!(oldest.checkpoint().format(), 5);

    let verified = verify_history(
        &layout,
        &root,
        oldest.checkpoint().covered_generation(),
        Some(oldest.hash()),
        &CancellationToken::new(),
    )
    .await
    .unwrap();

    assert_eq!(verified.verification.refs, 1);
    let git_dir = verified._workspace.path().join("repository.git");
    let refs = Command::new("git")
        .arg("--git-dir")
        .arg(&git_dir)
        .args(["for-each-ref", "--format=%(refname)"])
        .output()
        .unwrap();
    assert!(refs.status.success());
    assert_eq!(refs.stdout, b"refs/tags/first\n");
    let object = Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .args(["cat-file", "blob", "refs/tags/first"])
        .output()
        .unwrap();
    assert!(object.status.success());
    assert_eq!(object.stdout, b"first version");
}

#[tokio::test]
async fn historical_thin_member_is_repaired_against_its_retained_base() {
    use crab_git::incoming_pack::{ExternalDeltaBase, IncomingPack, ReceiveLimits};
    use std::sync::atomic::AtomicBool;

    let layout = empty_fixture().await;
    let base = vec![b'a'; 32 * 1024];
    let base_oid = publish_blob(&layout, "refs/tags/base", &base).await;
    let base_oid = gix_hash::ObjectId::from_hex(base_oid.as_bytes()).unwrap();
    let mut target = base.clone();
    target[1024] = b'b';
    let kind = gix_object::Kind::Blob;
    let oid = crab_remote::objects::object_id(kind, &target).unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let incoming = IncomingPack::from_generated_objects(
        [(kind, target.clone())],
        scratch.path(),
        ReceiveLimits {
            max_pack_bytes: LIMIT,
            max_objects: 2,
            max_object_bytes: LIMIT as usize,
            max_inflated_bytes: LIMIT,
            max_delta_depth: 8,
        },
        || false,
    )
    .unwrap();
    let thin = incoming
        .prepare_with_external_delta_bases(
            scratch.path(),
            LIMIT,
            &AtomicBool::new(false),
            &BTreeMap::from([(oid, base_oid)]),
            &BTreeMap::from([(oid, ExternalDeltaBase::new(base_oid, kind, base, 0))]),
            8,
            LIMIT as usize,
        )
        .unwrap()
        .unwrap();
    assert_eq!(thin.external_delta_bases(), &[base_oid]);
    let pack = CapsuleGitPack::new_with_external_delta_bases(
        Bytes::from(std::fs::read(thin.pack_path()).unwrap()),
        Bytes::from(std::fs::read(thin.index_path()).unwrap()),
        Bytes::from(std::fs::read(thin.reverse_path()).unwrap()),
        Bytes::from(std::fs::read(thin.kinds_path()).unwrap()),
        thin.git_sha1().to_string(),
        u64::from(thin.object_count()),
        vec![base_oid.to_string()],
    )
    .unwrap();
    publish_pack(&layout, "refs/tags/target", pack, &oid.to_string()).await;
    assert!(
        crab_remote::checkpoint::publish_capsule_checkpoint(
            &layout,
            1,
            LIMIT,
            &CancellationToken::new(),
        )
        .await
        .unwrap()
        .published
    );
    let root = crab_write::capsule_protocol::open_root(&layout)
        .await
        .unwrap();
    let chain = history_chain(&layout, &root).await.unwrap();
    let segment = &chain[0];
    let verified = verify_history(
        &layout,
        &root,
        segment.checkpoint().covered_generation(),
        Some(segment.hash()),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let output = Command::new("git")
        .arg("--git-dir")
        .arg(verified._workspace.path().join("repository.git"))
        .args(["cat-file", "blob", "refs/tags/target"])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, target);
}

#[tokio::test]
async fn layered_restore_preserves_sources_and_allows_new_epoch_publication() {
    let (layout, root) = fixture().await;
    let chain = history_chain(&layout, &root).await.unwrap();
    let oldest = chain.last().unwrap();
    let historical =
        crab_metadata::capsule_protocol::load_layered_checkpoint(&layout, oldest.checkpoint())
            .await
            .unwrap();
    let store = Store::from_storage(layout.store().clone());
    let router = StoreLayout::new(store.clone(), layout.repo_prefix().to_owned());
    let cancel = CancellationToken::new();
    let result = restore_history(
        &store,
        &router,
        &layout,
        &HistoryRestoreArgs {
            generation: oldest.checkpoint().covered_generation(),
            digest: Some(oldest.hash().to_owned()),
            apply: true,
            json: false,
        },
        &cancel,
    )
    .await
    .unwrap();
    assert!(result.applied);
    let restored = crab_write::capsule_protocol::open_root(&layout)
        .await
        .unwrap();
    assert_eq!(restored.record().root().refs(), oldest.refs());
    assert_ne!(
        restored.record().root().ref_epoch(),
        root.record().root().ref_epoch()
    );
    assert!(restored.record().root().gc_fence().is_none());
    let checkpoint = crab_metadata::capsule_protocol::load_layered_checkpoint(
        &layout,
        restored.record().root().checkpoint().unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(checkpoint.sources(), historical.sources());
    let retained = history_chain(&layout, &restored).await.unwrap();
    let before_restore = &retained[0];
    assert!(before_restore.capsule_runs().is_empty());
    assert_eq!(before_restore.refs(), root.record().root().refs());
    let retained_proof = verify_history(
        &layout,
        &restored,
        before_restore.checkpoint().covered_generation(),
        Some(before_restore.hash()),
        &cancel,
    )
    .await
    .unwrap();
    assert_eq!(retained_proof.verification.refs, 2);

    let new_oid = publish_blob(&layout, "refs/tags/after-restore", b"after restore").await;
    let view = crab_read::capsule_protocol::open_view(
        &layout,
        crab_read::capsule_protocol::CapsuleReadLimits {
            max_capsule_bytes: LIMIT,
            max_frontier_bytes: LIMIT,
        },
    )
    .await
    .unwrap();
    let mut expected = oldest.refs().clone();
    expected.insert("refs/tags/after-restore".to_owned(), new_oid);
    assert_eq!(view.refs(), &expected);
    let directory = tempfile::tempdir().unwrap();
    crab_git::initialize_bare_git_dir(directory.path()).unwrap();
    crab_read::capsule_protocol::install_git_packs_from_store(
        &view,
        &layout,
        directory.path(),
        LIMIT,
        None,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    for (name, bytes) in [
        ("refs/tags/first", b"first version".as_slice()),
        ("refs/tags/after-restore", b"after restore".as_slice()),
    ] {
        let output = Command::new("git")
            .arg("--git-dir")
            .arg(directory.path())
            .args(["cat-file", "blob", &view.refs()[name]])
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, bytes);
    }
}

#[tokio::test]
async fn corrupt_layered_history_cannot_restore_or_leak_the_sweep_lease() {
    let (layout, root) = fixture().await;
    let chain = history_chain(&layout, &root).await.unwrap();
    let oldest = chain.last().unwrap();
    let checkpoint =
        crab_metadata::capsule_protocol::load_layered_checkpoint(&layout, oldest.checkpoint())
            .await
            .unwrap();
    let source = &checkpoint.sources()[0];
    let path = layout.capsule_path(source.object_hash());
    let (body, _) = layout.store().get_with_etag(&path).await.unwrap();
    let mut corrupt = body.to_vec();
    corrupt[source.members()[0].pack().offset() as usize + 12] ^= 1;
    layout
        .store()
        .inner()
        .put(&path, Bytes::from(corrupt).into())
        .await
        .unwrap();
    let store = Store::from_storage(layout.store().clone());
    let router = StoreLayout::new(store.clone(), layout.repo_prefix().to_owned());
    let cancel = CancellationToken::new();
    let result = restore_history(
        &store,
        &router,
        &layout,
        &HistoryRestoreArgs {
            generation: oldest.checkpoint().covered_generation(),
            digest: Some(oldest.hash().to_owned()),
            apply: true,
            json: false,
        },
        &cancel,
    )
    .await;
    assert!(result.is_err());
    let unchanged = crab_write::capsule_protocol::open_root(&layout)
        .await
        .unwrap();
    assert_eq!(unchanged.record().digest(), root.record().digest());
    let lease = crate::maintenance::GcSweepLease::acquire(&store, router.repo_prefix(), &cancel)
        .await
        .unwrap();
    lease.release().await.unwrap();
}

#[tokio::test]
async fn history_verification_rejects_unavailable_external_pointer_content() {
    let crab = crab_types::pointer::Pointer {
        file_hash: [0x42; 32],
        size: 1024,
        shard_hint: None,
    }
    .serialize();
    let lfs = crab_git::LfsPointer {
        oid: [0x42; 32],
        size: 1024,
        extensions: Vec::new(),
    }
    .serialize();
    for (is_crab, bytes) in [(true, crab), (false, lfs)] {
        let layout = empty_fixture().await;
        publish_blob(&layout, "refs/tags/pointer", &bytes).await;
        assert!(
            crab_remote::checkpoint::publish_capsule_checkpoint(
                &layout,
                1,
                LIMIT,
                &CancellationToken::new(),
            )
            .await
            .unwrap()
            .published
        );
        let root = crab_write::capsule_protocol::open_root(&layout)
            .await
            .unwrap();
        let chain = history_chain(&layout, &root).await.unwrap();
        let segment = &chain[0];
        let error = verify_history(
            &layout,
            &root,
            segment.checkpoint().covered_generation(),
            Some(segment.hash()),
            &CancellationToken::new(),
        )
        .await
        .err()
        .unwrap();
        if is_crab {
            assert!(matches!(error, CrabError::CorruptObject { .. }));
        } else {
            assert!(matches!(error, CrabError::LfsObjectMissing { .. }));
        }
        let unchanged = crab_write::capsule_protocol::open_root(&layout)
            .await
            .unwrap();
        assert_eq!(unchanged.record().digest(), root.record().digest());
    }
}

#[tokio::test]
async fn history_verification_counts_and_hashes_reachable_lfs_content() {
    use sha2::{Digest, Sha256};

    let layout = empty_fixture().await;
    let content = Bytes::from_static(b"historical external content");
    let oid: [u8; 32] = Sha256::digest(&content).into();
    let lfs = crab_lfs::LfsObjectStore::new(layout.store().clone(), layout.repo_prefix());
    lfs.put(&oid, content.clone()).await.unwrap();
    let pointer = crab_git::LfsPointer {
        oid,
        size: content.len() as u64,
        extensions: Vec::new(),
    };
    publish_blob(&layout, "refs/tags/lfs", &pointer.serialize()).await;
    assert!(
        crab_remote::checkpoint::publish_capsule_checkpoint(
            &layout,
            1,
            LIMIT,
            &CancellationToken::new(),
        )
        .await
        .unwrap()
        .published
    );
    let root = crab_write::capsule_protocol::open_root(&layout)
        .await
        .unwrap();
    let chain = history_chain(&layout, &root).await.unwrap();
    let segment = &chain[0];
    let verified = verify_history(
        &layout,
        &root,
        segment.checkpoint().covered_generation(),
        Some(segment.hash()),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(verified.verification.dependency_objects, 4);
    assert_eq!(
        verified.verification.dependency_bytes,
        segment.bytes().len() as u64
            + verified.checkpoint.bytes().len() as u64
            + verified.checkpoint.sources()[0].object_size()
            + content.len() as u64
    );
    let mut corrupt = content.to_vec();
    corrupt[0] ^= 1;
    layout
        .store()
        .inner()
        .put(&lfs.object_path_for(&oid), Bytes::from(corrupt).into())
        .await
        .unwrap();
    assert!(
        verify_history(
            &layout,
            &root,
            segment.checkpoint().covered_generation(),
            Some(segment.hash()),
            &CancellationToken::new(),
        )
        .await
        .is_err()
    );
}
