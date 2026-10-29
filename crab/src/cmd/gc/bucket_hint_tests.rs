use super::*;
use crab_metadata::capsule_protocol::{
    CapsuleChunkIndex, PointerCatalog, ShardCatalogEntry, XorbCatalogEntry, XorbChunkEntry,
    write_capsule_chunk_candidates,
};
use crab_metadata::receipts::OriginReceipt;
use std::collections::BTreeSet;

async fn publish_hints(store: &Store, repo: &str) -> (MerkleHash, [u8; 32]) {
    let layout = crab_storage::StoreLayout::new(store.as_storage().clone(), repo.to_owned());
    let xorb = MerkleHash::from([3; 4]);
    let chunk = MerkleHash::from([1; 4]);
    let mut catalog = PointerCatalog::new();
    catalog
        .insert_xorb(
            xorb.hex(),
            XorbCatalogEntry::new(
                80,
                "4".repeat(64),
                vec![XorbChunkEntry::new(chunk.hex(), 7)],
            ),
        )
        .unwrap();
    catalog
        .insert_shard("2".repeat(64), ShardCatalogEntry::new(10, vec![xorb.hex()]))
        .unwrap();
    let origin = OriginReceipt::new(
        "canonical-origin".to_owned(),
        layout.xorb_path(&xorb).to_string(),
        xorb.into(),
        [5; 32],
        80,
        Some("etag".to_owned()),
        None,
    );
    let cancel = CancellationToken::new();
    let fence = crate::maintenance::GcGlobalWriterLease::acquire(store, GLOBAL_PREFIX, &cancel)
        .await
        .unwrap();
    let result = write_capsule_chunk_candidates(
        &layout,
        &catalog,
        &HashMap::from([(xorb, origin)]),
        &|| cancel.is_cancelled(),
    )
    .await;
    fence.release().await.unwrap();
    result.unwrap();
    let candidates = CapsuleChunkIndex::new(&layout)
        .candidates(layout.store(), &[chunk], 1024 * 1024)
        .await
        .unwrap();
    (
        chunk,
        candidates.placements[&<[u8; 32]>::from(chunk)].placement_id(),
    )
}

fn args(force: bool) -> BucketGcArgs {
    BucketGcArgs {
        bucket: "isolated-hint-test".to_owned(),
        dry_run: false,
        grace_period: Duration::from_secs(3600),
        force,
        yes: true,
        list_concurrency: 4,
        list_profile: GcListProfile::Adaptive,
        delete_concurrency: 4,
        resume_run_id: None,
    }
}

#[tokio::test]
async fn hint_gc_preserves_live_pages_and_honors_grace() {
    for force in [false, true] {
        let store = memory_store();
        seed_registry(&store, &RefRegistry::default()).await;
        let (hash, old_id) = publish_hints(&store, "first").await;
        publish_hints(&store, "second").await;
        let layout = crab_storage::StoreLayout::new(store.as_storage().clone(), String::new());
        let (_, live) = CapsuleChunkIndex::new(&layout)
            .objects(layout.store(), 1024 * 1024)
            .await
            .unwrap();
        let live = live
            .into_iter()
            .map(|(path, _)| path.to_string())
            .collect::<BTreeSet<_>>();
        let orphan = Bytes::from_static(b"unpublished hint page");
        let identity = blake3::hash(&orphan).to_hex().to_string();
        store
            .put(
                &canonical_global_content_path(CAPSULE_CHUNK_PAGE_KIND, &identity),
                orphan,
            )
            .await
            .unwrap();
        let before = store
            .list_prefix(&layout.capsule_chunk_pages_prefix())
            .await
            .unwrap()
            .into_iter()
            .map(|meta| meta.location.to_string())
            .collect::<BTreeSet<_>>();
        assert!(before.len() > live.len());
        let outcome = run_bucket_gc(
            &args(force),
            &store,
            &HashSet::new(),
            &HashSet::new(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        let after = store
            .list_prefix(&layout.capsule_chunk_pages_prefix())
            .await
            .unwrap()
            .into_iter()
            .map(|meta| meta.location.to_string())
            .collect::<BTreeSet<_>>();
        assert_eq!(after, if force { live } else { before.clone() });
        assert_eq!(
            outcome.derived_index_objects_deleted,
            (before.len() - after.len()) as u64
        );
        assert_eq!(
            outcome.to_summary().derived_index_objects_deleted,
            outcome.derived_index_objects_deleted
        );
        let exact = CapsuleChunkIndex::new(&layout)
            .candidates_by_id(layout.store(), &[(hash, old_id)], 1024 * 1024)
            .await
            .unwrap();
        assert_eq!(
            exact.placements[&<[u8; 32]>::from(hash)].placement_id(),
            old_id
        );
    }
}

#[tokio::test]
async fn missing_hint_page_aborts_sweep_before_payload_deletion() {
    let store = memory_store();
    seed_registry(&store, &RefRegistry::default()).await;
    publish_hints(&store, "first").await;
    let layout = crab_storage::StoreLayout::new(store.as_storage().clone(), String::new());
    let (_, pages) = CapsuleChunkIndex::new(&layout)
        .objects(layout.store(), 1024 * 1024)
        .await
        .unwrap();
    store.delete(&pages[0].0).await.unwrap();
    let orphan = canonical_global_content_path("xorbs", &"a".repeat(64));
    store
        .put(&orphan, Bytes::from_static(b"unreferenced payload"))
        .await
        .unwrap();
    assert!(
        run_bucket_gc(
            &args(true),
            &store,
            &HashSet::new(),
            &HashSet::new(),
            &CancellationToken::new()
        )
        .await
        .is_err()
    );
    assert!(store.get_with_etag(&orphan).await.is_ok());
}

#[tokio::test]
async fn hint_root_changes_invalidate_bucket_root_identity() {
    let store = memory_store();
    seed_registry(&store, &RefRegistry::default()).await;
    let registry = load_ref_registry(&store, false).await.unwrap();
    publish_hints(&store, "first").await;
    let before = bucket_root_snapshot_streaming(&store, &registry, &HashSet::new())
        .await
        .unwrap();
    publish_hints(&store, "second").await;
    let after = bucket_root_snapshot_streaming(&store, &registry, &HashSet::new())
        .await
        .unwrap();
    assert_ne!(before.root_identity, after.root_identity);
}
