use super::*;
use crab_metadata::capsule_protocol::{
    CapsuleChunkIndex, PointerCatalog, ShardCatalogEntry, XorbCatalogEntry, XorbChunkEntry,
    write_capsule_chunk_candidates,
};
use crab_metadata::receipts::OriginReceipt;

#[derive(Clone, Copy, Debug)]
enum Scope {
    Issued,
    Explicit,
    Shared,
}

async fn fixture(
    scope: Scope,
    inner: Arc<dyn ObjectStore>,
    repo: &str,
) -> (Arc<AgedObjects>, Store, StoreLayout<crab_storage::Store>) {
    let backend = Arc::new(AgedObjects {
        inner,
        deletes: None,
        paths: Mutex::new(HashSet::new()),
        modified: SystemTime::now() - Duration::from_secs(7200),
        list_calls: Mutex::new(Vec::new()),
    });
    let mut store = Store::new(backend.clone());
    let global = if matches!(scope, Scope::Shared) {
        ".crab".to_owned()
    } else {
        format!("{repo}/.crab")
    };
    if matches!(scope, Scope::Issued) {
        store = store.with_storage_scope(crab_types::storage::StorageScope {
            repo_prefix: repo.to_owned(),
            global_prefix: global.clone(),
            source_repo: "org/source".to_owned(),
            scope_hash: "1".repeat(64),
        });
    }
    let layout =
        StoreLayout::with_global_prefix(store.as_storage().clone(), repo.to_owned(), global);
    crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
        .await
        .unwrap();
    (backend, store, layout)
}

async fn publish_hints(
    layout: &StoreLayout<crab_storage::Store>,
    seed: u64,
) -> (MerkleHash, [u8; 32]) {
    let xorb = MerkleHash::from([seed; 4]);
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
    let leases = crate::maintenance::GcWriterLeases::acquire(
        &Store::from_storage(layout.store().clone()),
        layout.global_prefix(),
        layout.repo_prefix(),
        &cancel,
    )
    .await
    .unwrap();
    let result =
        write_capsule_chunk_candidates(layout, &catalog, &HashMap::from([(xorb, origin)]), &|| {
            false
        })
        .await;
    leases.release().await.unwrap();
    result.unwrap();
    let candidates = CapsuleChunkIndex::new(layout)
        .candidates(layout.store(), &[chunk], LIMIT)
        .await
        .unwrap();
    (
        chunk,
        candidates.placements[&<[u8; 32]>::from(chunk)].placement_id(),
    )
}

async fn sweep(
    store: &Store,
    layout: &StoreLayout<crab_storage::Store>,
    force: bool,
) -> crate::core::error::Result<super::super::GcOutcome> {
    let router = crate::storage::StoreLayout::with_global_prefix(
        store.clone(),
        layout.repo_prefix().to_owned(),
        layout.global_prefix().to_owned(),
    );
    super::super::run_repo_remote_gc(
        &GcArgs {
            force,
            yes: true,
            ..GcArgs::default()
        },
        store,
        &router,
        &HashSet::new(),
        &CancellationToken::new(),
        Duration::from_secs(3600),
        None,
    )
    .await
}

#[tokio::test]
async fn scoped_hint_gc_requires_issued_ownership_and_preserves_live_exact_ids_and_grace() {
    for scope in [Scope::Issued, Scope::Explicit, Scope::Shared] {
        for force in [false, true] {
            let state = fixture(scope, Arc::new(InMemory::new()), "org/scoped-hints").await;
            assert_hint_gc(scope, force, state).await;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires CRAB_E2E_FLAT_BUCKET and explicit scratch S3 credentials"]
async fn live_scoped_hint_gc_preserves_live_exact_ids_and_grace() {
    let bucket = std::env::var("CRAB_E2E_FLAT_BUCKET").unwrap();
    let store =
        crab_storage::build_static_env_store(&bucket, crab_storage::StorageProviderKind::S3)
            .unwrap();
    for force in [false, true] {
        let prefix = format!("scoped-hint-gc/{}", uuid::Uuid::now_v7());
        let state = fixture(Scope::Issued, store.inner().clone(), &prefix).await;
        assert_hint_gc(Scope::Issued, force, state).await;
    }
}

async fn assert_hint_gc(
    scope: Scope,
    force: bool,
    state: (Arc<AgedObjects>, Store, StoreLayout<crab_storage::Store>),
) {
    let (backend, store, layout) = state;
    let (chunk, old_id) = publish_hints(&layout, 3).await;
    publish_hints(&layout, 4).await;
    let (_, live) = CapsuleChunkIndex::new(&layout)
        .objects(layout.store(), LIMIT)
        .await
        .unwrap();
    let live = live
        .into_iter()
        .map(|(path, _)| path)
        .collect::<HashSet<_>>();
    let before = store
        .list_prefix(&layout.capsule_chunk_pages_prefix())
        .await
        .unwrap();
    let before = before
        .into_iter()
        .map(|meta| meta.location)
        .collect::<HashSet<_>>();
    assert!(
        before.len() > live.len(),
        "fixture must have superseded pages"
    );
    backend.paths.lock().unwrap().extend(before.iter().cloned());
    let fresh = Path::from(format!(
        "{}/aa/{}",
        layout.capsule_chunk_pages_prefix(),
        "a".repeat(64)
    ));
    let unrelated = Path::from(format!(
        "other/.crab/capsule-chunk-pages/bb/{}",
        "b".repeat(64)
    ));
    for path in [&fresh, &unrelated] {
        store
            .put(path, Bytes::from_static(b"unpublished page"))
            .await
            .unwrap();
    }
    backend.paths.lock().unwrap().insert(unrelated.clone());
    let outcome = sweep(&store, &layout, force).await.unwrap();
    let after = store
        .list_prefix(&layout.capsule_chunk_pages_prefix())
        .await
        .unwrap();
    let after = after
        .into_iter()
        .map(|meta| meta.location)
        .collect::<HashSet<_>>();
    let mut expected = if matches!(scope, Scope::Issued) {
        live
    } else {
        before.clone()
    };
    expected.insert(fresh);
    assert_eq!(after, expected, "{scope:?}, force={force}");
    assert_eq!(
        outcome.derived_index_objects_deleted,
        (before.len() + 1 - after.len()) as u64
    );
    assert!(store.head(&unrelated).await.is_ok());
    let candidates = CapsuleChunkIndex::new(&layout)
        .candidates_by_id(layout.store(), &[(chunk, old_id)], LIMIT)
        .await
        .unwrap();
    assert_eq!(
        candidates.placements[&<[u8; 32]>::from(chunk)].placement_id(),
        old_id
    );
}

#[tokio::test]
async fn broken_scoped_hint_closure_aborts_before_any_sweep_and_releases_admission() {
    for missing in [true, false] {
        let (backend, store, layout) =
            fixture(Scope::Issued, Arc::new(InMemory::new()), "org/scoped-hints").await;
        publish_hints(&layout, 3).await;
        let (_, pages) = CapsuleChunkIndex::new(&layout)
            .objects(layout.store(), LIMIT)
            .await
            .unwrap();
        let page = &pages[0].0;
        if missing {
            store.delete(page).await.unwrap();
        } else {
            // The product store rejects immutable-key replacement. Inject a
            // provider fault below that guard so GC must detect the damage.
            backend
                .inner
                .put_opts(
                    page,
                    Bytes::from_static(b"damaged page").into(),
                    PutOptions::default(),
                )
                .await
                .unwrap();
        }
        let orphan = layout.capsule_path(&"f".repeat(64));
        store
            .put(&orphan, Bytes::from_static(b"unreferenced capsule"))
            .await
            .unwrap();
        backend.paths.lock().unwrap().insert(orphan.clone());
        assert!(
            sweep(&store, &layout, false).await.is_err(),
            "missing={missing}"
        );
        assert!(store.head(&orphan).await.is_ok());
        for domain in [layout.global_prefix(), layout.repo_prefix()] {
            let writer = crab_coordination::GcFenceLease::acquire_writer(
                store.inner(),
                domain,
                Duration::from_secs(60),
            )
            .await
            .unwrap();
            writer.release().await.unwrap();
        }
        let root = crab_write::capsule_protocol::open_root(&layout)
            .await
            .unwrap();
        assert!(root.record().root().gc_fence().is_none());
    }
}

#[tokio::test]
async fn scoped_hint_gc_respects_both_writer_domains_and_releases_failed_admission() {
    for global in [true, false] {
        let (_, store, layout) =
            fixture(Scope::Issued, Arc::new(InMemory::new()), "org/scoped-hints").await;
        let domain = if global {
            layout.global_prefix()
        } else {
            layout.repo_prefix()
        };
        let writer = crab_coordination::GcFenceLease::acquire_writer(
            store.inner(),
            domain,
            Duration::from_secs(60),
        )
        .await
        .unwrap();
        assert!(matches!(
            sweep(&store, &layout, false).await,
            Err(crate::core::error::CrabError::PushLockHeld { .. })
        ));
        let root = crab_write::capsule_protocol::open_root(&layout)
            .await
            .unwrap();
        assert!(root.record().root().gc_fence().is_none());
        writer.release().await.unwrap();
        sweep(&store, &layout, false).await.unwrap();
    }
}
