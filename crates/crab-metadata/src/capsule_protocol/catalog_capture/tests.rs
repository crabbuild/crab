use super::*;
use crate::capsule_protocol::{
    Capsule, CapsulePointer, CapsuleRefEdit, CapsuleRefHead, CapsuleRun, CapsuleSection,
    CapsuleSectionKind, CapsuleTransaction, CapsuleVisibilityDelta, RepositoryRoot, RootRecord,
    capsule_ref_name_key, create_root,
};
#[cfg(feature = "file-index-reader")]
use crate::file_index_lookup::FileIndexLookupSession;
use crate::git_visibility::GitVisibilityEdit;
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};

#[tokio::test]
async fn current_catalog_does_not_recapture_another_repository_or_epoch() {
    for field in ["repository_id", "ref_epoch"] {
        let layout = StoreLayout::new(
            Store::new(Arc::new(object_store::memory::InMemory::new())),
            "repositories/current-authority".to_owned(),
        );
        let initial = RootRecord::encode(
            RepositoryRoot::initial(&"1".repeat(64), "refs/heads/main").unwrap(),
        )
        .unwrap();
        let root = create_root(&layout, initial.clone()).await.unwrap();
        let mut replacement = serde_json::to_value(initial.root()).unwrap();
        replacement[field] = serde_json::Value::String("2".repeat(64));
        let replacement = RootRecord::encode(serde_json::from_value(replacement).unwrap()).unwrap();
        layout
            .store()
            .put_overwrite(&layout.capsule_root_path(), replacement.bytes().clone())
            .await
            .unwrap();
        assert!(matches!(
            load_current_pointer_catalog_lookup(&layout, &root).await,
            Err(MetadataError::CapsuleContract {
                record: "current catalog",
                ..
            })
        ));
        assert!(matches!(
            load_current_publication_catalog(&layout, &root).await,
            Err(MetadataError::CapsuleContract {
                record: "current catalog",
                ..
            })
        ));
    }
}

async fn publication_catalog_fixture(
    edited_ref: &str,
) -> (
    PublicationCatalogLookup,
    Arc<object_store::throttle::ThrottledStore<object_store::memory::InMemory>>,
) {
    use crate::capsule_protocol::{
        FileCatalogEntry, ShardCatalogEntry, XorbCatalogEntry, XorbChunkEntry,
    };
    let origin = Arc::new(object_store::throttle::ThrottledStore::new(
        object_store::memory::InMemory::new(),
        object_store::throttle::ThrottleConfig::default(),
    ));
    let layout = StoreLayout::new(Store::new(origin.clone()), "catalog-discovery".to_owned());
    let root = create_root(
        &layout,
        RootRecord::encode(RepositoryRoot::initial(&"1".repeat(64), "refs/heads/main").unwrap())
            .unwrap(),
    )
    .await
    .unwrap();
    let file = "2".repeat(64);
    let shard = "3".repeat(64);
    let xorb = "4".repeat(64);
    let chunk = "5".repeat(64);
    let mut catalog = PointerCatalog::new();
    catalog
        .insert_file(file.clone(), FileCatalogEntry::new(7, shard.clone()))
        .unwrap();
    catalog
        .insert_shard(shard, ShardCatalogEntry::new(80, vec![xorb.clone()]))
        .unwrap();
    catalog
        .insert_xorb(
            xorb.clone(),
            XorbCatalogEntry::new(
                99,
                "6".repeat(64),
                vec![XorbChunkEntry::new(chunk.clone(), 7)],
            ),
        )
        .unwrap();
    let oid = "1".repeat(40);
    let transaction = CapsuleTransaction::new(
        root.record().digest(),
        vec![CapsuleRefEdit::new(
            edited_ref,
            None,
            Some(oid.clone()),
            None,
        )],
    )
    .unwrap();
    let visibility = CapsuleVisibilityDelta::new(BTreeMap::from([(
        edited_ref.to_owned(),
        GitVisibilityEdit::from_replacement_objects(None, oid.clone(), vec![oid.clone()]),
    )]))
    .unwrap();
    let run = CapsuleRun::leaf(
        Capsule::build(
            &transaction,
            Vec::new(),
            vec![
                CapsuleSection::new(
                    CapsuleSectionKind::VisibilityDelta,
                    visibility.encode().unwrap(),
                ),
                CapsuleSection::new(
                    CapsuleSectionKind::CatalogDelta,
                    catalog.encode_delta().unwrap(),
                ),
            ],
        )
        .unwrap(),
    )
    .unwrap();
    layout
        .store()
        .create_strict(&layout.capsule_path(run.hash()), run.bytes().clone())
        .await
        .unwrap();
    let pointer = CapsulePointer::new(
        run.hash(),
        run.bytes().len() as u64,
        run.control_offset(),
        run.control_size(),
        run.footer_hash(),
        run.level(),
        run.transaction_ids(),
        run.newest_base_root_digest(),
    )
    .unwrap();
    // A valid catalog index cannot authorize reuse when its containing
    // transaction disagrees with the captured ref head.
    let head = CapsuleRefHead::from_root(
        "refs/heads/main",
        root.record().root().ref_epoch().to_owned(),
        None,
        None,
    )
    .unwrap();
    let state = head
        .successor_state(
            &BTreeSet::new(),
            Some(oid),
            None,
            transaction.id().unwrap(),
            vec![pointer],
        )
        .unwrap();
    layout
        .store()
        .create_strict(
            &layout.capsule_ref_head_path(&capsule_ref_name_key("refs/heads/main")),
            head.commit(state).unwrap().encode().unwrap(),
        )
        .await
        .unwrap();
    (
        load_current_publication_catalog(&layout, &root)
            .await
            .unwrap(),
        origin,
    )
}

#[tokio::test]
async fn catalog_discovery_hits_require_complete_ref_reconciliation() {
    let (lookup, _) = publication_catalog_fixture("refs/heads/other").await;
    assert!(
        lookup
            .files(&["f".repeat(64)])
            .await
            .unwrap()
            .files()
            .is_empty()
    );
    assert!(
        lookup
            .chunks(&["e".repeat(64)])
            .await
            .unwrap()
            .xorbs()
            .is_empty()
    );
    assert!(lookup.authority.get().is_none());
    assert!(lookup.files(&["2".repeat(64)]).await.is_err());
    assert!(lookup.chunks(&["5".repeat(64)]).await.is_err());
    assert!(lookup.xorbs(&["4".repeat(64)]).await.is_err());
}

#[tokio::test(start_paused = true)]
async fn cancelled_publication_proof_can_retry_and_serve_concurrent_queries() {
    let (lookup, origin) = publication_catalog_fixture("refs/heads/main").await;
    let files = ["2".repeat(64)];
    let xorbs = ["4".repeat(64)];
    let mut work = PageWork::new(MAX_CATALOG_BYTES, 0);
    // Warm discovery so cancellation interrupts reconciliation, not its probe.
    assert!(
        lookup
            .probe
            .has_rows(lookup.layout.store(), Kind::CatalogFiles, &files, &mut work)
            .await
            .unwrap()
    );
    origin.config_mut(|config| config.wait_get_per_call = std::time::Duration::from_secs(1));
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(1), lookup.files(&files))
            .await
            .is_err()
    );
    assert!(lookup.authority.get().is_none());
    origin.config_mut(|config| config.wait_get_per_call = std::time::Duration::ZERO);
    let (files, xorbs) = tokio::try_join!(lookup.files(&files), lookup.xorbs(&xorbs)).unwrap();
    assert_eq!(
        (
            files.files().len(),
            xorbs.xorbs().len(),
            lookup.authority.get().is_some()
        ),
        (1, 1, true)
    );
}

async fn capture_reads(
    file_data_bytes: usize,
    objects_per_ref: usize,
    history_len: usize,
    delay: std::time::Duration,
    absence_probe: bool,
) -> (usize, usize, u64, std::time::Duration) {
    let reads = Arc::new(AtomicUsize::new(0));
    let bytes = Arc::new(AtomicU64::new(0));
    let observed = reads.clone();
    let observed_bytes = bytes.clone();
    let origin = Arc::new(object_store::throttle::ThrottledStore::new(
        object_store::memory::InMemory::new(),
        object_store::throttle::ThrottleConfig::default(),
    ));
    let store = Store::new(origin.clone())
        .with_read_request_observer(Arc::new(move |_| {
            observed.fetch_add(1, Ordering::SeqCst);
        }))
        .with_read_byte_observer(Arc::new(move |count| {
            observed_bytes.fetch_add(count, Ordering::SeqCst);
        }));
    let layout = StoreLayout::new(store, "catalog-leaf-opening".to_owned());
    let root = create_root(
        &layout,
        RootRecord::encode(RepositoryRoot::initial(&"1".repeat(64), "refs/heads/main").unwrap())
            .unwrap(),
    )
    .await
    .unwrap();
    let count = 64;
    for index in 1..=count {
        let name = format!("refs/heads/agent-{index:04}");
        let last_object = index * objects_per_ref;
        let mut oid = format!("{last_object:040x}");
        let objects = (last_object - objects_per_ref + 1..=last_object)
            .map(|object| format!("{object:040x}"))
            .collect();
        let transaction = CapsuleTransaction::new(
            root.record().digest(),
            vec![CapsuleRefEdit::new(&name, None, Some(oid.clone()), None)],
        )
        .unwrap();
        let visibility = CapsuleVisibilityDelta::new(BTreeMap::from([(
            name.clone(),
            GitVisibilityEdit::from_replacement_objects(None, oid.clone(), objects),
        )]))
        .unwrap();
        let mut sections = vec![CapsuleSection::new(
            CapsuleSectionKind::VisibilityDelta,
            visibility.encode().unwrap(),
        )];
        if file_data_bytes > 0 {
            sections.push(CapsuleSection::new(
                CapsuleSectionKind::FileData,
                bytes::Bytes::from(vec![17; file_data_bytes]),
            ));
        }
        let first =
            CapsuleRun::leaf(Capsule::build(&transaction, Vec::new(), sections).unwrap()).unwrap();
        let mut leaves = vec![first];
        for update in 1..history_len {
            let next = format!(
                "{:040x}",
                count * objects_per_ref + index * history_len + update
            );
            let transaction = CapsuleTransaction::new(
                root.record().digest(),
                vec![CapsuleRefEdit::new(
                    &name,
                    Some(oid.clone()),
                    Some(next.clone()),
                    None,
                )],
            )
            .unwrap();
            let visibility = CapsuleVisibilityDelta::new(BTreeMap::from([(
                name.clone(),
                GitVisibilityEdit::from_delta_objects(
                    Some(oid),
                    next.clone(),
                    vec![next.clone()],
                    Vec::new(),
                ),
            )]))
            .unwrap();
            leaves.push(
                CapsuleRun::leaf(
                    Capsule::build(
                        &transaction,
                        Vec::new(),
                        vec![CapsuleSection::new(
                            CapsuleSectionKind::VisibilityDelta,
                            visibility.encode().unwrap(),
                        )],
                    )
                    .unwrap(),
                )
                .unwrap(),
            );
            oid = next;
        }
        let run = if leaves.len() == 1 {
            leaves.pop().unwrap()
        } else {
            CapsuleRun::compact(leaves).unwrap()
        };
        layout
            .store()
            .create_strict(&layout.capsule_path(run.hash()), run.bytes().clone())
            .await
            .unwrap();
        let pointer = CapsulePointer::new(
            run.hash(),
            run.bytes().len() as u64,
            run.control_offset(),
            run.control_size(),
            run.footer_hash(),
            run.level(),
            run.transaction_ids(),
            run.newest_base_root_digest(),
        )
        .unwrap();
        let head = CapsuleRefHead::from_root(
            &name,
            root.record().root().ref_epoch().to_owned(),
            None,
            None,
        )
        .unwrap();
        let state = head
            .successor_state(
                &BTreeSet::new(),
                Some(oid),
                None,
                run.transaction_ids().last().unwrap().clone(),
                vec![pointer],
            )
            .unwrap();
        layout
            .store()
            .create_strict(
                &layout.capsule_ref_head_path(&capsule_ref_name_key(&name)),
                head.commit(state).unwrap().encode().unwrap(),
            )
            .await
            .unwrap();
    }
    reads.store(0, Ordering::SeqCst);
    bytes.store(0, Ordering::SeqCst);
    origin.config_mut(|config| config.wait_get_per_call = delay);
    let started = tokio::time::Instant::now();
    fn send<F: std::future::Future + Send>(future: F) -> F {
        future
    }
    if absence_probe {
        let lookup = send(load_current_publication_catalog(&layout, &root))
            .await
            .unwrap();
        assert_eq!(lookup.pending_capsule_count(), count);
        assert!(
            send(lookup.files(&["f".repeat(64)]))
                .await
                .unwrap()
                .files()
                .is_empty()
        );
        assert!(
            send(lookup.chunks(&["e".repeat(64)]))
                .await
                .unwrap()
                .xorbs()
                .is_empty()
        );
        assert!(
            send(lookup.xorbs(&["d".repeat(64)]))
                .await
                .unwrap()
                .xorbs()
                .is_empty()
        );
        assert!(lookup.authority.get().is_none());
    } else {
        let lookup = send(load_pointer_catalog_lookup_from_root(&layout, &root))
            .await
            .unwrap();
        assert!(
            lookup
                .files(layout.store(), &[])
                .await
                .unwrap()
                .files()
                .is_empty()
        );
    }
    let observed = reads.load(Ordering::SeqCst);
    assert!(bytes.load(Ordering::SeqCst) <= (count * 16 * 1024) as u64);
    (
        count,
        observed,
        bytes.load(Ordering::SeqCst),
        started.elapsed(),
    )
}

#[tokio::test]
async fn small_ref_leaf_capture_coalesces_metadata_only() {
    let (count, observed, _, _) = capture_reads(0, 1, 1, std::time::Duration::ZERO, false).await;
    assert_eq!(
        observed,
        3 * count,
        "one head and two metadata reads per ref"
    );
}

#[tokio::test]
async fn large_ref_leaves_keep_payloads_cold_and_coalesce_metadata() {
    let (count, observed, _, _) =
        capture_reads(64 * 1024, 1, 1, std::time::Duration::ZERO, false).await;
    assert_eq!(
        observed,
        3 * count,
        "one head and two metadata reads per ref"
    );
}

#[tokio::test]
async fn catalog_capture_keeps_small_leaf_payloads_cold() {
    let (count, _, baseline, _) = capture_reads(0, 1, 1, std::time::Duration::ZERO, false).await;
    let (_, _, with_payload, _) =
        capture_reads(8 * 1024, 1, 1, std::time::Duration::ZERO, false).await;
    assert!(
        with_payload <= baseline + (count * 256) as u64,
        "payload changed control traffic from {baseline} to {with_payload} bytes"
    );
}

#[tokio::test(start_paused = true)]
async fn leaf_proof_latency_is_bounded_by_concurrent_waves() {
    let delay = std::time::Duration::from_millis(8);
    let (_, _, _, elapsed) = capture_reads(0, 1, 1, delay, false).await;
    assert!(
        elapsed <= delay * 16,
        "capture serialized proof reads: {elapsed:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn paged_new_tip_proofs_follow_concurrent_capture_waves() {
    let delay = std::time::Duration::from_millis(8);
    for (objects, history) in [(4096, 1), (1, 4)] {
        let (_, _, _, elapsed) = capture_reads(0, objects, history, delay, false).await;
        assert!(
            elapsed <= delay * 16,
            "capture serialized {objects}-object/{history}-edit tip proofs: {elapsed:?}"
        );
    }
}

#[tokio::test]
async fn paged_tip_capture_reuses_authenticated_inline_roots() {
    let (count, reads, _, _) = capture_reads(0, 4096, 1, std::time::Duration::ZERO, false).await;
    assert_eq!(
        reads,
        count * 4,
        "head, run, scalar with inline root, and tip leaf"
    );
}

#[tokio::test]
async fn absent_publication_keys_do_not_open_visibility_pages() {
    let (count, reads, _, _) = capture_reads(0, 4096, 1, std::time::Duration::ZERO, true).await;
    assert!(
        reads <= 2 * count + 1,
        "absence probe opened visibility: {reads} reads for {count} refs"
    );
}

#[cfg(feature = "file-index-reader")]
#[tokio::test]
async fn current_catalog_opening_reads_follow_compacted_runs() {
    let mut observations = Vec::new();
    // Metadata-only history keeps the catalog empty. One physical run
    // isolates visibility replay from run fan-out and payload lookup.
    for count in [2, 8, 32] {
        let reads = Arc::new(AtomicUsize::new(0));
        let observed = reads.clone();
        let store = Store::new(Arc::new(object_store::memory::InMemory::new()))
            .with_read_request_observer(Arc::new(move |_| {
                observed.fetch_add(1, Ordering::SeqCst);
            }));
        let layout = StoreLayout::new(store, "catalog-opening".to_owned());
        let root = create_root(
            &layout,
            RootRecord::encode(
                RepositoryRoot::initial(&"1".repeat(64), "refs/heads/main").unwrap(),
            )
            .unwrap(),
        )
        .await
        .unwrap();
        let mut leaves = Vec::new();
        let mut previous = None;
        for version in 1..=count {
            let oid = format!("{version:040x}");
            let transaction = CapsuleTransaction::new(
                root.record().digest(),
                vec![CapsuleRefEdit::new(
                    "refs/heads/main",
                    previous.clone(),
                    Some(oid.clone()),
                    None,
                )],
            )
            .unwrap();
            let visibility = CapsuleVisibilityDelta::new(BTreeMap::from([(
                "refs/heads/main".to_owned(),
                GitVisibilityEdit::from_delta_objects(
                    previous,
                    oid.clone(),
                    vec![oid.clone()],
                    Vec::new(),
                ),
            )]))
            .unwrap();
            let capsule = Capsule::build(
                &transaction,
                Vec::new(),
                vec![CapsuleSection::new(
                    CapsuleSectionKind::VisibilityDelta,
                    visibility.encode().unwrap(),
                )],
            )
            .unwrap();
            leaves.push(CapsuleRun::leaf(capsule).unwrap());
            previous = Some(oid);
        }
        let run = CapsuleRun::compact(leaves).unwrap();
        layout
            .store()
            .create_strict(&layout.capsule_path(run.hash()), run.bytes().clone())
            .await
            .unwrap();
        let pointer = CapsulePointer::new(
            run.hash(),
            run.bytes().len() as u64,
            run.control_offset(),
            run.control_size(),
            run.footer_hash(),
            run.level(),
            run.transaction_ids(),
            run.newest_base_root_digest(),
        )
        .unwrap();
        let head = CapsuleRefHead::from_root(
            "refs/heads/main",
            root.record().root().ref_epoch().to_owned(),
            None,
            None,
        )
        .unwrap();
        let state = head
            .successor_state(
                &BTreeSet::new(),
                previous,
                None,
                run.transaction_ids().last().unwrap().clone(),
                vec![pointer],
            )
            .unwrap();
        layout
            .store()
            .create_strict(
                &layout.capsule_ref_head_path(&capsule_ref_name_key("refs/heads/main")),
                head.commit(state).unwrap().encode().unwrap(),
            )
            .await
            .unwrap();
        reads.store(0, Ordering::SeqCst);
        let session = FileIndexLookupSession::open_for_storage(layout.store(), "catalog-opening")
            .await
            .unwrap();
        observations.push((count, reads.load(Ordering::SeqCst)));
        session.close().await.unwrap();
    }
    assert!(
        observations.windows(2).all(|pair| pair[0].1 == pair[1].1),
        "provider reads should follow compacted proof blocks rather than logical edits: {observations:?}"
    );
}
