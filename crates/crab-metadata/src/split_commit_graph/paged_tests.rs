use super::super::paged::decode_layer;
use super::*;
use object_store::ObjectStoreExt;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

fn oid(index: u32) -> [u8; 20] {
    let mut oid = [7; 20];
    oid[..4].copy_from_slice(&index.to_be_bytes());
    oid
}

fn inputs(start: u32, count: u32) -> Vec<CommitGraphInput> {
    (start..start + count)
        .map(|index| CommitGraphInput {
            oid: oid(index),
            tree_oid: oid(index + 100_000),
            commit_time: i64::from(index),
            parents: if index == 0 {
                Vec::new()
            } else {
                vec![oid(index - 1)]
            },
        })
        .collect()
}

async fn fixture(count: u32) -> (Store, StoreLayout<Store>, CommitGraphWrite, Arc<AtomicU64>) {
    let observed = Arc::new(AtomicU64::new(0));
    let counter = Arc::clone(&observed);
    let store = Store::new(Arc::new(object_store::memory::InMemory::new()))
        .with_read_byte_observer(Arc::new(move |bytes| {
            counter.fetch_add(bytes, Ordering::Relaxed);
        }));
    let layout = StoreLayout::new(store.clone(), "paged-graph".to_owned());
    let write = append_paged_commit_graph(
        None,
        1,
        "a".repeat(64),
        "b".repeat(64),
        &[oid(count - 1)],
        inputs(0, count),
    )
    .unwrap()
    .unwrap();
    upload_split_commit_graph(&store, &layout, &write)
        .await
        .unwrap();
    observed.store(0, Ordering::Relaxed);
    (store, layout, write, observed)
}

#[tokio::test]
async fn graph_point_queries_preserve_duplicates_without_reading_whole_layers() {
    let (store, layout, write, observed) = fixture(10_000).await;
    let reader = CommitGraphReader::load(&store, &layout, &write.descriptor_hash, 512 * 1024)
        .await
        .unwrap();
    assert_eq!(
        observed.load(Ordering::Relaxed),
        write.descriptor_bytes.len() as u64
    );
    let requested = [oid(9_999), oid(0), oid(9_999), oid(20_000)];
    assert_eq!(
        reader.ordinals(&store, &requested).await.unwrap(),
        vec![Some(9_999), Some(0), Some(9_999), None]
    );
    let records = reader
        .records(&store, &[9_999, 0, 9_999, 10_000])
        .await
        .unwrap();
    assert_eq!(
        records
            .iter()
            .map(|record| record.as_ref().map(|record| record.oid))
            .collect::<Vec<_>>(),
        vec![Some(oid(9_999)), Some(oid(0)), Some(oid(9_999)), None]
    );
    assert!(observed.load(Ordering::Relaxed) < write.layers[0].bytes.len() as u64 / 2);
    observed.store(0, Ordering::Relaxed);
    reader.ordinals(&store, &requested).await.unwrap();
    reader
        .records(&store, &[9_999, 0, 9_999, 10_000])
        .await
        .unwrap();
    assert_eq!(observed.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn full_graph_validation_preserves_ordinals_across_paged_compaction_and_rebind() {
    let (store, layout, initial, _) = fixture(8).await;
    let base = load_split_commit_graph(
        &store,
        &layout,
        &initial.descriptor_hash,
        DEFAULT_MAX_SPLIT_COMMIT_GRAPH_BYTES,
    )
    .await
    .unwrap();
    let first_digest = base.ordinal_digest();
    let write = append_paged_commit_graph(
        Some(base),
        2,
        "a".repeat(64),
        "b".repeat(64),
        &[oid(15)],
        inputs(8, 8),
    )
    .unwrap()
    .unwrap();
    upload_split_commit_graph(&store, &layout, &write)
        .await
        .unwrap();
    let graph = load_split_commit_graph(
        &store,
        &layout,
        &write.descriptor_hash,
        DEFAULT_MAX_SPLIT_COMMIT_GRAPH_BYTES,
    )
    .await
    .unwrap();
    assert_eq!(graph.descriptor.layers.len(), 1);
    assert_ne!(graph.ordinal_digest(), first_digest);
    let rebind = rebind_split_commit_graph(&graph, 3, "c".repeat(64), "d".repeat(64)).unwrap();
    let descriptor = decode_commit_graph_descriptor(&rebind.descriptor_bytes, "rebind").unwrap();
    assert_eq!(
        descriptor.commit_ordinal_digest.as_deref(),
        Some(graph.ordinal_digest().as_str())
    );
    let reader = CommitGraphReader::load(
        &store,
        &layout,
        &write.descriptor_hash,
        DEFAULT_MAX_SPLIT_COMMIT_GRAPH_BYTES,
    )
    .await
    .unwrap();
    let requested = (0..16).collect::<Vec<_>>();
    let actual = reader.records(&store, &requested).await.unwrap();
    assert_eq!(
        actual,
        requested
            .iter()
            .map(|ordinal| graph.record(*ordinal).cloned())
            .collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn graph_query_budget_applies_before_io_and_to_warm_pages() {
    let (store, layout, write, observed) = fixture(1_000).await;
    let mut reader = CommitGraphReader::load(
        &store,
        &layout,
        &write.descriptor_hash,
        DEFAULT_MAX_SPLIT_COMMIT_GRAPH_BYTES,
    )
    .await
    .unwrap();
    reader.records(&store, &[999]).await.unwrap();
    reader.max_bytes = reader.descriptor_bytes;
    observed.store(0, Ordering::Relaxed);
    assert!(reader.records(&store, &[999]).await.is_err());
    assert_eq!(observed.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn missing_or_corrupt_graph_pages_are_errors_including_out_of_range_oid_misses() {
    let (store, layout, write, _) = fixture(1_000).await;
    let path = layout.repo_path(&write.layers[0].reference.path);
    let root = &write.layers[0].reference.pages.as_ref().unwrap().oids;
    let mut bytes = write.layers[0].bytes.clone();
    bytes[root.offset as usize] ^= 1;
    store
        .inner()
        .put(&path, bytes::Bytes::from(bytes).into())
        .await
        .unwrap();
    let reader = CommitGraphReader::load(
        &store,
        &layout,
        &write.descriptor_hash,
        DEFAULT_MAX_SPLIT_COMMIT_GRAPH_BYTES,
    )
    .await
    .unwrap();
    assert!(reader.ordinals(&store, &[oid(20_000)]).await.is_err());
    store.inner().delete(&path).await.unwrap();
    assert!(reader.records(&store, &[0]).await.is_err());
}

#[tokio::test]
async fn full_validation_rejects_oid_directory_disagreement_and_positional_digest_tampering() {
    let (store, layout, write, _) = fixture(3).await;
    let graph = load_split_commit_graph(
        &store,
        &layout,
        &write.descriptor_hash,
        DEFAULT_MAX_SPLIT_COMMIT_GRAPH_BYTES,
    )
    .await
    .unwrap();
    let mut descriptor = graph.descriptor.clone();
    descriptor.commit_ordinal_digest = Some("f".repeat(64));
    assert!(SplitCommitGraph::new(descriptor, graph.layers.clone()).is_err());
    let reference = &write.layers[0].reference;
    let mut roots = reference.pages.clone().unwrap();
    let mut bytes = write.layers[0].bytes.clone();
    // Re-sign a wrong ordinal to test semantic consistency rather than hashing.
    let start = roots.oids.offset as usize;
    bytes[start + 14 + 20 + 4..start + 14 + 20 + 8].copy_from_slice(&2u32.to_le_bytes());
    roots.oids.hash = *blake3::hash(&bytes[start..start + roots.oids.bytes as usize]).as_bytes();
    assert!(decode_layer(&bytes, reference, &roots, "wrong-directory").is_err());
}

#[tokio::test]
async fn paged_append_reuses_unchanged_layers_and_migrates_only_derived_eager_graphs() {
    let (store, layout, initial, _) = fixture(8).await;
    let base = load_split_commit_graph(
        &store,
        &layout,
        &initial.descriptor_hash,
        DEFAULT_MAX_SPLIT_COMMIT_GRAPH_BYTES,
    )
    .await
    .unwrap();
    let old_layer = base.descriptor.layers[0].clone();
    let write = append_paged_commit_graph(
        Some(base),
        2,
        "a".repeat(64),
        "b".repeat(64),
        &[oid(8)],
        inputs(8, 1),
    )
    .unwrap()
    .unwrap();
    let descriptor = decode_commit_graph_descriptor(&write.descriptor_bytes, "append").unwrap();
    assert_eq!((&descriptor.layers[0], write.layers.len()), (&old_layer, 1));
    let eager = append_split_commit_graph(
        None,
        1,
        "a".repeat(64),
        "b".repeat(64),
        &[oid(0)],
        inputs(0, 1),
    )
    .unwrap()
    .unwrap();
    upload_split_commit_graph(&store, &layout, &eager)
        .await
        .unwrap();
    assert!(
        CommitGraphReader::load(
            &store,
            &layout,
            &eager.descriptor_hash,
            DEFAULT_MAX_SPLIT_COMMIT_GRAPH_BYTES
        )
        .await
        .is_err()
    );
    let base = load_split_commit_graph(
        &store,
        &layout,
        &eager.descriptor_hash,
        DEFAULT_MAX_SPLIT_COMMIT_GRAPH_BYTES,
    )
    .await
    .unwrap();
    let converted = append_paged_commit_graph(
        Some(base),
        1,
        "a".repeat(64),
        "b".repeat(64),
        &[oid(0)],
        vec![],
    )
    .unwrap()
    .unwrap();
    upload_split_commit_graph(&store, &layout, &converted)
        .await
        .unwrap();
    let reader = CommitGraphReader::load(
        &store,
        &layout,
        &converted.descriptor_hash,
        DEFAULT_MAX_SPLIT_COMMIT_GRAPH_BYTES,
    )
    .await
    .unwrap();
    assert_eq!(
        reader.ordinals(&store, &[oid(0)]).await.unwrap(),
        vec![Some(0)]
    );
}

#[tokio::test]
async fn cancelled_graph_range_reads_leave_the_reader_reusable() {
    use object_store::throttle::{ThrottleConfig, ThrottledStore};
    let (store, layout, write, _) = fixture(1_000).await;
    let throttled = Arc::new(ThrottledStore::new(
        store.inner().clone(),
        ThrottleConfig::default(),
    ));
    let store = Store::new(throttled.clone());
    let reader = CommitGraphReader::load(
        &store,
        &layout,
        &write.descriptor_hash,
        DEFAULT_MAX_SPLIT_COMMIT_GRAPH_BYTES,
    )
    .await
    .unwrap();
    reader.records(&store, &[0]).await.unwrap();
    throttled.config_mut(|config| config.wait_get_per_call = std::time::Duration::from_secs(60));
    {
        let query = reader.records(&store, &[999]);
        futures_util::pin_mut!(query);
        assert!(futures_util::poll!(query).is_pending());
    }
    throttled.config_mut(|config| config.wait_get_per_call = std::time::Duration::ZERO);
    assert_eq!(
        reader.records(&store, &[999]).await.unwrap()[0]
            .as_ref()
            .unwrap()
            .oid,
        oid(999)
    );
}

#[tokio::test]
async fn decreasing_append_sizes_cannot_grow_the_layer_directory_without_bound() {
    let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
    let layout = StoreLayout::new(store.clone(), "graph-carries".to_owned());
    let mut graph = None;
    let mut count = 0;
    for batch in (1..=40).rev() {
        let write = append_paged_commit_graph(
            graph,
            1,
            "a".repeat(64),
            "b".repeat(64),
            &[oid(count + batch - 1)],
            inputs(count, batch),
        )
        .unwrap()
        .unwrap();
        upload_split_commit_graph(&store, &layout, &write)
            .await
            .unwrap();
        graph = Some(
            load_split_commit_graph(
                &store,
                &layout,
                &write.descriptor_hash,
                DEFAULT_MAX_SPLIT_COMMIT_GRAPH_BYTES,
            )
            .await
            .unwrap(),
        );
        count += batch;
        assert!(graph.as_ref().unwrap().descriptor.layers.len() <= MAX_PAGED_LAYERS);
    }
    let graph = graph.unwrap();
    assert_eq!(
        (0..count)
            .map(|ordinal| graph.record(ordinal).unwrap().oid)
            .collect::<Vec<_>>(),
        (0..count).map(oid).collect::<Vec<_>>()
    );
}

proptest::proptest! {
    #[test]
    fn paged_graph_matches_eager_graph_for_branching_parent_orders(selectors in proptest::collection::vec(0u8..8, 1..40)) {
        let additions = selectors.iter().enumerate().map(|(index, selector)| {
            let mut parents = Vec::new();
            if index > 0 && selector & 1 != 0 { parents.push(oid(index as u32 - 1)) }
            if index > 1 && selector & 2 != 0 { parents.push(oid(index as u32 - 2)) }
            if selector & 4 != 0 { parents.reverse() }
            CommitGraphInput { oid: oid(index as u32), tree_oid: oid(index as u32 + 1000), commit_time: index as i64, parents }
        }).collect::<Vec<_>>();
        let roots = additions.iter().map(|input| input.oid).collect::<Vec<_>>();
        let eager = append_split_commit_graph(None, 1, "a".repeat(64), "b".repeat(64), &roots, additions.clone()).unwrap().unwrap();
        let paged = append_paged_commit_graph(None, 1, "a".repeat(64), "b".repeat(64), &roots, additions).unwrap().unwrap();
        let eager_layers = eager.layers.iter().map(|object| decode_commit_graph_layer(&object.bytes, "eager").unwrap()).collect::<Vec<_>>();
        let paged_layers = paged.layers.iter().map(|object| decode_layer(&object.bytes, &object.reference, object.reference.pages.as_ref().unwrap(), "paged").unwrap().graph).collect::<Vec<_>>();
        proptest::prop_assert_eq!(&paged_layers, &eager_layers);
    }
}

#[tokio::test]
async fn ancestry_membership_includes_both_merge_parents_but_excludes_retained_other_history() {
    let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
    let layout = StoreLayout::new(store.clone(), "branch-ancestry".to_owned());
    let mut additions = inputs(0, 5);
    additions[2].parents = vec![oid(0)];
    additions[3].parents = vec![oid(1), oid(2)];
    additions[4].parents.clear();
    let write = append_paged_commit_graph(
        None,
        1,
        "a".repeat(64),
        "b".repeat(64),
        &[oid(3)],
        additions,
    )
    .unwrap()
    .unwrap();
    upload_split_commit_graph(&store, &layout, &write)
        .await
        .unwrap();
    let reader = CommitGraphReader::load(
        &store,
        &layout,
        &write.descriptor_hash,
        DEFAULT_MAX_SPLIT_COMMIT_GRAPH_BYTES,
    )
    .await
    .unwrap();
    let queries = [
        (
            3,
            vec![0, 1, 2, 3, 4, 2],
            vec![true, true, true, true, false, true],
        ),
        (1, vec![0, 1, 2, 3], vec![true, true, false, false]),
        (
            4,
            vec![0, 1, 2, 3, 4],
            vec![false, false, false, false, true],
        ),
    ];
    for (root, candidates, expected) in queries {
        assert_eq!(
            reader
                .ancestry_membership(&store, root, &candidates)
                .await
                .unwrap(),
            expected
        );
    }
}

#[tokio::test]
async fn ancestry_lookup_reads_dependency_pages_and_charges_warm_queries() {
    let (store, layout, write, observed) = fixture(10_000).await;
    let mut reader = CommitGraphReader::load(&store, &layout, &write.descriptor_hash, 512 * 1024)
        .await
        .unwrap();
    observed.store(0, Ordering::Relaxed);
    let candidates = [0, 1, 5_000, 9_999, 10_000, 5_000];
    assert_eq!(
        reader
            .ancestry_membership(&store, 9_999, &candidates)
            .await
            .unwrap(),
        vec![true, true, true, true, false, true]
    );
    assert!(observed.load(Ordering::Relaxed) < 512 * 1024);
    observed.store(0, Ordering::Relaxed);
    reader
        .ancestry_membership(&store, 9_999, &candidates)
        .await
        .unwrap();
    assert_eq!(observed.load(Ordering::Relaxed), 0);
    reader.max_bytes = reader.descriptor_bytes;
    assert!(
        reader
            .ancestry_membership(&store, 9_999, &candidates)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn missing_ancestry_nodes_cannot_become_a_negative_membership_answer() {
    let (store, layout, write, _) = fixture(100).await;
    let reader = CommitGraphReader::load(
        &store,
        &layout,
        &write.descriptor_hash,
        DEFAULT_MAX_SPLIT_COMMIT_GRAPH_BYTES,
    )
    .await
    .unwrap();
    let roots = write.layers[0].reference.pages.as_ref().unwrap();
    let mut bytes = write.layers[0].bytes.clone();
    bytes[roots.ancestry_nodes.offset as usize] ^= 1;
    store
        .inner()
        .put(
            &layout.repo_path(&write.layers[0].reference.path),
            bytes::Bytes::from(bytes).into(),
        )
        .await
        .unwrap();
    assert!(
        reader
            .ancestry_membership(&store, 99, &[200])
            .await
            .is_err()
    );
}

#[tokio::test]
async fn full_validation_rejects_a_resigned_set_that_grants_a_non_ancestor() {
    use super::super::ancestry::Reference;
    use crate::pages::{self, Kind, Writer};
    let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
    let layout = StoreLayout::new(store.clone(), "forged-ancestry".to_owned());
    let mut additions = inputs(0, 4);
    additions[2].parents = vec![oid(0)];
    let mut write = append_paged_commit_graph(
        None,
        1,
        "a".repeat(64),
        "b".repeat(64),
        &[oid(3)],
        additions,
    )
    .unwrap()
    .unwrap();
    let object = &mut write.layers[0];
    let roots = object.reference.pages.as_ref().unwrap();
    let scan = |root: &crate::PageRef, kind| {
        pages::scan(
            &object.bytes,
            root,
            kind,
            "original",
            &mut Default::default(),
        )
        .unwrap()
    };
    let records = scan(&roots.records, Kind::CommitRecords);
    let oids = scan(&roots.oids, Kind::CommitOids);
    let mut ancestry_roots = scan(&roots.ancestry_roots, Kind::CommitAncestryRoots);
    let old = Reference::decode(&ancestry_roots[3].1, "old").unwrap();
    let fake = vec![0, 2, 0, 0, 0, 0];
    let reference = Reference {
        hash: *blake3::hash(&fake).as_bytes(),
        start: 0,
        height: 2,
    };
    ancestry_roots[3].1 = reference.encode();
    let mut nodes = scan(&roots.ancestry_nodes, Kind::CommitAncestryNodes)
        .into_iter()
        .collect::<BTreeMap<_, _>>();
    nodes.remove(old.hash.as_slice());
    nodes.insert(reference.hash.to_vec(), fake);
    let mut writer = Writer::default();
    let pages = CommitGraphLayerPages {
        records: writer.tree(Kind::CommitRecords, records).unwrap(),
        oids: writer.tree(Kind::CommitOids, oids).unwrap(),
        ancestry_roots: writer
            .tree(Kind::CommitAncestryRoots, ancestry_roots)
            .unwrap(),
        ancestry_nodes: writer
            .tree(Kind::CommitAncestryNodes, nodes.into_iter().collect())
            .unwrap(),
    };
    object.bytes = writer.bytes;
    object.reference.hash = blake3::hash(&object.bytes).to_hex().to_string();
    object.reference.path = commit_graph_layer_path(&object.reference.hash);
    object.reference.bytes = object.bytes.len() as u64;
    object.reference.pages = Some(pages);
    let mut descriptor =
        decode_commit_graph_descriptor(&write.descriptor_bytes, "descriptor").unwrap();
    descriptor.layers[0] = object.reference.clone();
    write.descriptor_bytes = encode_commit_graph_descriptor(&descriptor).unwrap();
    write.descriptor_hash = blake3::hash(&write.descriptor_bytes).to_hex().to_string();
    upload_split_commit_graph(&store, &layout, &write)
        .await
        .unwrap();
    let error = load_split_commit_graph(
        &store,
        &layout,
        &write.descriptor_hash,
        DEFAULT_MAX_SPLIT_COMMIT_GRAPH_BYTES,
    )
    .await
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("ancestry coverage differs from the verified DAG")
    );
}
