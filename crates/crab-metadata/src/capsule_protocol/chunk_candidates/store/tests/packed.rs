use super::*;

#[tokio::test]
async fn incremental_publications_do_not_rewrite_unchanged_catalog_rows() {
    let observations = Arc::new(Observations::default());
    let layout = StoreLayout::new(
        Store::new(Arc::new(object_store::memory::InMemory::new()))
            .with_storage_observer(observations.clone()),
        "source".to_owned(),
    );
    let (mut catalog, mut origins, _) = fixture(3000, 3);
    write_capsule_chunk_candidates(&layout, &catalog, &origins, &|| false)
        .await
        .unwrap();
    let (delta, more_origins, _) = fixture(1, 5);
    for (hash, entry) in delta.xorbs() {
        catalog.insert_xorb(hash.clone(), entry.clone()).unwrap();
    }
    catalog
        .insert_shard(
            "3".repeat(64),
            delta.shards().values().next().unwrap().clone(),
        )
        .unwrap();
    origins.extend(more_origins);
    observations.0.lock().unwrap().clear();
    write_capsule_chunk_candidates(&layout, &catalog, &origins, &|| false)
        .await
        .unwrap();
    let written = observations
        .0
        .lock()
        .unwrap()
        .iter()
        .map(|observation| observation.bytes_written)
        .sum::<u64>();
    assert!(
        written < 16 * 1024,
        "incremental write used {written} bytes"
    );
}

#[tokio::test]
async fn mixed_size_compaction_preserves_newer_heads_in_unmerged_runs() {
    let layout = layout();
    let (large, origins, hashes) = fixture(3000, 3);
    write_capsule_chunk_candidates(&layout, &large, &origins, &|| false)
        .await
        .unwrap();
    let newer = StoreLayout::new(layout.store().clone(), "newest".to_owned());
    let (small, origins, _) = fixture_from_hashes(5, vec![hashes[0]]);
    write_capsule_chunk_candidates(&newer, &small, &origins, &|| false)
        .await
        .unwrap();
    let (unrelated, origins, _) = fixture(3000, 7);
    write_capsule_chunk_candidates(&layout, &unrelated, &origins, &|| false)
        .await
        .unwrap();
    let result = CapsuleChunkIndex::new(&layout)
        .candidates(layout.store(), &hashes[..1], 1024 * 1024)
        .await
        .unwrap();
    assert!(
        result.placements.len() == 1
            && result.anchors.len() == 1
            && result
                .anchors
                .values()
                .all(|anchor| anchor.source_repo_prefix == "newest")
    );
}

#[tokio::test]
async fn hint_root_rejects_retired_shape_and_unsupported_versions() {
    let layout = layout();
    let (catalog, origins, _) = fixture(1, 3);
    write_capsule_chunk_candidates(&layout, &catalog, &origins, &|| false)
        .await
        .unwrap();
    let body = layout
        .store()
        .get_with_etag(&layout.capsule_chunk_index_path())
        .await
        .unwrap()
        .0;
    let root = Root::decode(&body).unwrap();
    assert_eq!(root.version, 1);
    assert!(Root::decode(br#"{"version":1,"records":null}"#).is_err());
    for version in [0, 2, u32::MAX] {
        let mut changed = root.clone();
        changed.version = version;
        assert!(Root::decode(&changed.encode().unwrap()).is_err());
    }
}

#[tokio::test]
async fn corrupt_packed_run_blocks_compaction_and_gc() {
    let layout = layout();
    let (catalog, origins, _) = fixture(3000, 3);
    write_capsule_chunk_candidates(&layout, &catalog, &origins, &|| false)
        .await
        .unwrap();
    let index = CapsuleChunkIndex::new(&layout);
    let (root, _) = index
        .load(layout.store(), &mut PageWork::new(1024 * 1024, 0))
        .await
        .unwrap()
        .unwrap();
    let run = root.levels.iter().flatten().next().unwrap();
    let path = run.path(&index.prefix);
    let mut bytes = layout
        .store()
        .get_with_etag(&path)
        .await
        .unwrap()
        .0
        .to_vec();
    bytes[0] ^= 1;
    layout
        .store()
        .put_overwrite(&path, Bytes::from(bytes))
        .await
        .unwrap();
    assert!(
        index
            .objects(layout.store(), 16 * 1024 * 1024)
            .await
            .is_err()
    );
    let (delta, origins, _) = fixture(3000, 5);
    assert!(
        write_capsule_chunk_candidates(&layout, &delta, &origins, &|| false)
            .await
            .is_err()
    );
    assert_eq!(
        index
            .load(layout.store(), &mut PageWork::new(1024 * 1024, 0))
            .await
            .unwrap()
            .unwrap()
            .0
            .sequence,
        root.sequence
    );
}

#[tokio::test]
async fn root_validation_rejects_wrong_tiers_and_future_sequences() {
    let layout = layout();
    let (catalog, origins, hashes) = fixture(1, 3);
    write_capsule_chunk_candidates(&layout, &catalog, &origins, &|| false)
        .await
        .unwrap();
    let index = CapsuleChunkIndex::new(&layout);
    let (mut root, _) = index
        .load(layout.store(), &mut PageWork::new(1024 * 1024, 0))
        .await
        .unwrap()
        .unwrap();
    let tier = root.levels.iter().position(Option::is_some).unwrap();
    let run = root.levels[tier].clone().unwrap();
    root.levels.push(Some(run));
    assert!(Root::decode(&root.encode().unwrap()).is_err());
    root.levels.pop();
    let mut work = PageWork::new(1024 * 1024, 0);
    let run = root.levels.iter().flatten().next().unwrap();
    let mut rows = run
        .read(layout.store(), &index.prefix, root.sequence, &mut work)
        .await
        .unwrap()
        .into_iter()
        .collect::<BTreeMap<_, _>>();
    for value in rows.values_mut() {
        value[..8].copy_from_slice(&2_u64.to_le_bytes());
    }
    let (run, body) = Run::build(&rows).unwrap();
    layout
        .store()
        .put_if_absent_verified(&run.path(&index.prefix), body)
        .await
        .unwrap();
    root.levels[tier] = Some(run);
    layout
        .store()
        .put_overwrite(&index.root, root.encode().unwrap())
        .await
        .unwrap();
    assert!(
        index
            .candidates(layout.store(), &hashes, 1024 * 1024)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn immutable_identity_conflicts_do_not_advance_the_root() {
    let layout = layout();
    let (catalog, origins, _) = fixture(1, 3);
    write_capsule_chunk_candidates(&layout, &catalog, &origins, &|| false)
        .await
        .unwrap();
    let index = CapsuleChunkIndex::new(&layout);
    let (mut root, _) = index
        .load(layout.store(), &mut PageWork::new(1024 * 1024, 0))
        .await
        .unwrap()
        .unwrap();
    let run = root.levels.iter().flatten().next().unwrap();
    let mut rows = run
        .read(
            layout.store(),
            &index.prefix,
            root.sequence,
            &mut PageWork::new(1024 * 1024, 0),
        )
        .await
        .unwrap()
        .into_iter()
        .collect::<BTreeMap<_, _>>();
    // Whitespace preserves the logical anchor digest, but immutable bytes must
    // still match exactly when an existing identity is republished.
    for (key, value) in &mut rows {
        if key[0] == 0x07 {
            value.insert(8, b' ');
        }
    }
    let (run, body) = Run::build(&rows).unwrap();
    layout
        .store()
        .put_if_absent_verified(&run.path(&index.prefix), body)
        .await
        .unwrap();
    let mut levels = vec![None; run.level() + 1];
    let level = run.level();
    levels[level] = Some(run);
    root.levels = levels;
    layout
        .store()
        .put_overwrite(&index.root, root.encode().unwrap())
        .await
        .unwrap();
    assert!(
        write_capsule_chunk_candidates(&layout, &catalog, &origins, &|| false)
            .await
            .is_err()
    );
    assert_eq!(
        index
            .load(layout.store(), &mut PageWork::new(1024 * 1024, 0))
            .await
            .unwrap()
            .unwrap()
            .0
            .sequence,
        root.sequence
    );
}

#[tokio::test]
async fn authenticated_filters_have_no_false_negatives_for_scattered_heads_or_ids() {
    let layout = layout();
    let hashes = (0..5000_u64)
        .map(|index| MerkleHash::from(*blake3::hash(&index.to_le_bytes()).as_bytes()))
        .collect::<Vec<_>>();
    let (catalog, origins, hashes) = fixture_from_hashes(11, hashes);
    write_capsule_chunk_candidates(&layout, &catalog, &origins, &|| false)
        .await
        .unwrap();
    let index = CapsuleChunkIndex::new(&layout);
    let heads = index
        .candidates(layout.store(), &hashes, 16 * 1024 * 1024)
        .await
        .unwrap();
    let ids = heads
        .placements
        .values()
        .map(|p| (MerkleHash::from(p.chunk_hash), p.placement_id()))
        .collect::<Vec<_>>();
    let exact = index
        .candidates_by_id(layout.store(), &ids, 16 * 1024 * 1024)
        .await
        .unwrap();
    assert_eq!(heads.placements, exact.placements);
    assert_eq!(heads.placements.len(), hashes.len());
}

#[tokio::test]
async fn damaged_filter_is_an_error_instead_of_a_negative_hint() {
    let layout = layout();
    let (catalog, origins, hashes) = fixture(1, 3);
    write_capsule_chunk_candidates(&layout, &catalog, &origins, &|| false)
        .await
        .unwrap();
    let index = CapsuleChunkIndex::new(&layout);
    let (root, _) = index
        .load(layout.store(), &mut PageWork::new(1024 * 1024, 0))
        .await
        .unwrap()
        .unwrap();
    let run = root.levels.iter().flatten().next().unwrap();
    let path = run.path(&index.prefix);
    let mut bytes = layout
        .store()
        .get_with_etag(&path)
        .await
        .unwrap()
        .0
        .to_vec();
    bytes[run.filter.offset as usize + 22] ^= 1;
    layout
        .store()
        .put_overwrite(&path, Bytes::from(bytes))
        .await
        .unwrap();
    assert!(
        index
            .candidates(layout.store(), &hashes, 1024 * 1024)
            .await
            .is_err()
    );
}
