use super::{crab_dir_from_journal_path, enumerate_sources, normalize_output_class};
use crate::core::error::CrabError;
use crate::storage::{Store, StoreLayout};
use bytes::Bytes;
use object_store::memory::InMemory;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[test]
fn nested_journal_path_resolves_crab_directory() {
    let path = std::path::Path::new("/repo/.crab/optimize/xorbs/journal.db");

    assert_eq!(
        crab_dir_from_journal_path(path).unwrap(),
        std::path::Path::new("/repo/.crab")
    );
}

#[test]
fn output_classes_are_canonicalized_for_each_provider() {
    use crate::tier::provider::Provider;

    assert_eq!(
        normalize_output_class(Provider::S3, "standard-ia").unwrap(),
        "STANDARD_IA"
    );
    assert_eq!(
        normalize_output_class(Provider::Gcs, "nearline").unwrap(),
        "NEARLINE"
    );
    assert_eq!(
        normalize_output_class(Provider::Azure, "standard").unwrap(),
        "Hot"
    );
}

#[test]
fn output_class_rejects_cross_provider_values() {
    let error =
        normalize_output_class(crate::tier::provider::Provider::Gcs, "STANDARD_IA").unwrap_err();

    assert!(matches!(error, CrabError::Configuration { .. }));
}

#[tokio::test]
async fn empty_capsule_inventory_reuses_its_admitted_root_without_global_fallback() {
    let observed = Arc::new(crab_storage::test_support::CountingObjectStore::new(
        Arc::new(InMemory::new()),
    ));
    let store = Store::new(observed.clone());
    let router = StoreLayout::new(store.clone(), "org/root-required".to_owned());
    let layout =
        crab_storage::StoreLayout::new(store.as_storage().clone(), router.repo_prefix().to_owned());
    let foreign = crab_xet::hash::MerkleHash::from([11_u64; 4]);
    let path = layout.xorb_path(&foreign);
    store
        .put(&path, Bytes::from_static(b"foreign"))
        .await
        .unwrap();
    let root =
        crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
            .await
            .unwrap();
    // Model an out-of-band provider mutation after admission. Ordinary Store
    // writes correctly reject overwriting an existing object without CAS.
    object_store::ObjectStoreExt::put(
        store.inner().as_ref(),
        &layout.capsule_root_path(),
        Bytes::from_static(b"changed after admission").into(),
    )
    .await
    .unwrap();
    observed.reset();
    let sources = enumerate_sources(&store, &router, root, &CancellationToken::new())
        .await
        .unwrap();
    let touched_foreign = observed
        .requests()
        .iter()
        .any(|request| request.location == path.as_ref());
    let reread_root = observed
        .requests()
        .iter()
        .any(|request| request.location == layout.capsule_root_path().as_ref());
    assert_eq!(
        (
            sources.len(),
            touched_foreign,
            reread_root,
            observed.put_requests()
        ),
        (0, false, false, 0)
    );
}

#[tokio::test]
async fn capsule_source_enumeration_ignores_foreign_global_xorbs() {
    let store = Store::new(Arc::new(InMemory::new()));
    let router = StoreLayout::new(store.clone(), "org/repo".to_owned());
    let layout =
        crab_storage::StoreLayout::new(store.as_storage().clone(), router.repo_prefix().to_owned());
    let root =
        crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
            .await
            .unwrap();
    let selected = crab_xet::hash::MerkleHash::from([2_u64; 4]);
    let foreign = crab_xet::hash::MerkleHash::from([3_u64; 4]);
    let shard = crab_xet::hash::MerkleHash::from([4_u64; 4]);
    let file = crab_xet::hash::MerkleHash::from([5_u64; 4]);
    store
        .put(
            &layout.xorb_path(&selected),
            Bytes::from_static(b"selected"),
        )
        .await
        .unwrap();
    store
        .put(&layout.xorb_path(&foreign), Bytes::from_static(b"foreign"))
        .await
        .unwrap();
    let mut catalog = crab_metadata::capsule_protocol::PointerCatalog::new();
    catalog
        .insert_xorb(
            selected.hex(),
            crab_metadata::capsule_protocol::XorbCatalogEntry::new(
                8,
                "6".repeat(64),
                vec![crab_metadata::capsule_protocol::XorbChunkEntry::new(
                    "7".repeat(64),
                    8,
                )],
            ),
        )
        .unwrap();
    catalog
        .insert_shard(
            shard.hex(),
            crab_metadata::capsule_protocol::ShardCatalogEntry::new(1, vec![selected.hex()]),
        )
        .unwrap();
    catalog
        .insert_file(
            file.hex(),
            crab_metadata::capsule_protocol::FileCatalogEntry::new(8, shard.hex()),
        )
        .unwrap();
    let pack = crab_metadata::capsule_protocol::CapsuleGitPack::new(
        Bytes::from_static(b"pack"),
        Bytes::from_static(b"index"),
        Bytes::from_static(b"reverse"),
        Bytes::from_static(b"locator"),
        "8".repeat(40),
        1,
    )
    .unwrap();
    let tip = "9".repeat(40);
    let transaction = crab_metadata::capsule_protocol::CapsuleTransaction::new(
        root.record().digest(),
        vec![crab_metadata::capsule_protocol::CapsuleRefEdit::new(
            "refs/heads/main",
            None,
            Some(tip.clone()),
            None,
        )],
    )
    .unwrap();
    let visibility = crab_metadata::capsule_protocol::CapsuleVisibilityDelta::new(
        std::collections::BTreeMap::from([(
            "refs/heads/main".to_owned(),
            crab_metadata::git_visibility::GitVisibilityEdit::from_replacement_objects(
                None,
                tip.clone(),
                vec![tip],
            ),
        )]),
    )
    .unwrap();
    let capsule = crab_metadata::capsule_protocol::Capsule::build(
        &transaction,
        vec![pack],
        vec![
            crab_metadata::capsule_protocol::CapsuleSection::new(
                crab_metadata::capsule_protocol::CapsuleSectionKind::CatalogDelta,
                catalog.encode_delta().unwrap(),
            ),
            crab_metadata::capsule_protocol::CapsuleSection::new(
                crab_metadata::capsule_protocol::CapsuleSectionKind::VisibilityDelta,
                visibility.encode().unwrap(),
            ),
        ],
    )
    .unwrap();
    crab_write::capsule_protocol::publish(&layout, root, &transaction, &capsule)
        .await
        .unwrap();

    let admitted = crab_metadata::capsule_protocol::load_root(&layout)
        .await
        .unwrap();
    let sources = enumerate_sources(&store, &router, admitted, &CancellationToken::new())
        .await
        .unwrap();

    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0].hash, selected.hex());
}

#[tokio::test]
async fn corrupt_capsule_root_never_falls_back_to_global_xorb_listing() {
    let store = Store::new(Arc::new(InMemory::new()));
    let router = StoreLayout::new(store.clone(), "org/corrupt".to_owned());
    let layout =
        crab_storage::StoreLayout::new(store.as_storage().clone(), router.repo_prefix().to_owned());
    store
        .put(
            &layout.capsule_root_path(),
            Bytes::from_static(b"corrupt root"),
        )
        .await
        .unwrap();
    let foreign = crab_xet::hash::MerkleHash::from([10_u64; 4]);
    store
        .put(&layout.xorb_path(&foreign), Bytes::from_static(b"foreign"))
        .await
        .unwrap();

    let result = crate::auth::open_repository_root(
        &store,
        router.repo_prefix(),
        "crab://bucket/org/corrupt",
    )
    .await;

    assert!(result.is_err());
}
