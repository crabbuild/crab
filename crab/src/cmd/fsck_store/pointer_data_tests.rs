#![expect(clippy::unwrap_used, reason = "test assertions")]

use super::*;
use bytes::Bytes;
use crab_metadata::capsule_protocol::{
    Capsule, CapsuleGitPack, CapsuleRefEdit, CapsuleSection, CapsuleSectionKind,
    CapsuleTransaction, CapsuleVisibilityDelta, FileCatalogEntry, PointerCatalog,
    ShardCatalogEntry,
};
use crab_metadata::git_visibility::GitVisibilityEdit;
use sha2::{Digest, Sha256};

async fn publish_pointer(
    layout: &crab_storage::StoreLayout<crab_storage::Store>,
    root: crab_metadata::capsule_protocol::RootSnapshot,
    body: &[u8],
    delta: &PointerCatalog,
) -> crab_metadata::capsule_protocol::RootSnapshot {
    let kind = gix_object::Kind::Blob;
    let oid = crab_remote::objects::object_id(kind, body)
        .unwrap()
        .to_string();
    let mut bytes = Vec::new();
    crab_git::pack_writer::write_pack(
        &mut bytes,
        std::iter::once(Ok((kind, body.len() as u64, body))),
        1024 * 1024,
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
        1024 * 1024,
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
    let name = "refs/tags/payload";
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
        vec![
            CapsuleSection::new(
                CapsuleSectionKind::VisibilityDelta,
                visibility.encode().unwrap(),
            ),
            CapsuleSection::new(
                CapsuleSectionKind::CatalogDelta,
                delta.encode_delta().unwrap(),
            ),
        ],
    )
    .unwrap();
    crab_write::capsule_protocol::publish(layout, root, &transaction, &capsule)
        .await
        .unwrap()
}

async fn pointer_checker(case: &str) -> StoreChecker {
    let (store, prefix, root, xorb_hash, shard_hash) =
        super::tests::capsule_checker_fixture().await;
    let router = StoreLayout::new(store.clone(), prefix.clone());
    let layout = crab_storage::StoreLayout::with_global_prefix(
        store.as_storage().clone(),
        router.repo_prefix().to_owned(),
        router.global_prefix().to_owned(),
    );
    let mut pointer = Pointer {
        file_hash: blake3::hash(&[0x61; 1024]).into(),
        size: 1024,
        shard_hint: None,
    };
    let mut delta = PointerCatalog::new();
    match case {
        "missing" => pointer.file_hash = [42; 32],
        "size" => pointer.size += 1,
        "hash" => {
            let (body, _) = store
                .get_with_etag_bounded(&router.shard_path(&shard_hash), MAX_FSCK_SHARD_BYTES)
                .await
                .unwrap();
            let reader = ShardReader::from_bytes(body, shard_hash);
            let mut recipe = reader
                .get_file_info(&MerkleHash::from(pointer.file_hash))
                .unwrap()
                .unwrap();
            pointer.file_hash = [42; 32];
            recipe.metadata.file_hash = MerkleHash::from(pointer.file_hash);
            let mut shard = crab_xet::shard::ShardWriter::new();
            shard
                .add_xorb(Arc::new(reader.get_xorb_info(&xorb_hash).unwrap().unwrap()))
                .unwrap();
            shard.add_file(recipe).unwrap();
            let (body, hash) = shard.finalize().unwrap();
            delta
                .insert_shard(
                    hash.hex(),
                    ShardCatalogEntry::new(body.len() as u64, vec![xorb_hash.hex()]),
                )
                .unwrap();
            delta
                .insert_file(
                    MerkleHash::from(pointer.file_hash).hex(),
                    FileCatalogEntry::new(pointer.size, hash.hex()),
                )
                .unwrap();
            store
                .put(&router.shard_path(&hash), Bytes::from(body))
                .await
                .unwrap();
        }
        _ => {}
    }
    let body = if case.starts_with("lfs-") {
        let lfs = crab_git::lfs_pointer::LfsPointer {
            oid: Sha256::digest(b"hello").into(),
            size: 5,
            extensions: Vec::new(),
        };
        if case != "lfs-missing" {
            let path = crab_lfs::LfsObjectStore::new(layout.store().clone(), layout.repo_prefix())
                .object_path_for(&lfs.oid);
            let body = if case == "lfs-corrupt" {
                b"wrong"
            } else {
                b"hello"
            };
            layout
                .store()
                .put(&path, Bytes::copy_from_slice(body))
                .await
                .unwrap();
        }
        lfs.serialize()
    } else {
        pointer.serialize()
    };
    let root = publish_pointer(&layout, root, &body, &delta).await;
    StoreChecker::for_capsule_repository(store, prefix, root)
        .await
        .unwrap()
}

#[tokio::test]
async fn full_fsck_proves_reachable_crab_and_lfs_payloads() {
    let mut outcomes = Vec::new();
    for case in [
        "valid",
        "missing",
        "size",
        "hash",
        "lfs-valid",
        "lfs-missing",
        "lfs-corrupt",
    ] {
        let checker = Box::pin(pointer_checker(case)).await;
        let (_, outcome) = crate::cmd::fsck::run_fsck(
            &crate::cmd::fsck::FsckArgs::default(),
            &checker,
            &crate::cmd::fsck::NullRepairer,
            &CancellationToken::new(),
            Duration::from_hours(1),
            None,
        )
        .await
        .unwrap();
        outcomes.push((case, outcome.errors == 0));
    }
    assert_eq!(
        outcomes,
        vec![
            ("valid", true),
            ("missing", false),
            ("size", false),
            ("hash", false),
            ("lfs-valid", true),
            ("lfs-missing", false),
            ("lfs-corrupt", false)
        ]
    );
}

#[tokio::test]
async fn fsck_retains_one_git_database_through_data_checks_then_releases_it() {
    let checker = Box::pin(pointer_checker("valid")).await;
    checker.check_git_objects().await.unwrap();
    let state = checker.capsule.as_ref().unwrap();
    let original = state.git_workspace.get().unwrap().path().to_owned();
    let clean = checker.check_data_chain().await.unwrap().is_empty();
    let reused = state.git_workspace.get().unwrap().path() == original;
    drop(checker);
    assert!(clean && reused && !original.exists());
}

#[tokio::test]
async fn data_checks_prove_pointer_coverage_without_a_prior_git_phase() {
    let checker = Box::pin(pointer_checker("missing")).await;
    assert!(matches!(checker.check_data_chain().await,
        Err(CrabError::CorruptObject { reason, .. }) if reason.contains("absent from the catalog")
    ));
}
