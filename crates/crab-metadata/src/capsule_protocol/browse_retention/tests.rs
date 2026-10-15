use std::sync::Arc;

use object_store::memory::InMemory;

use super::*;

fn fixture() -> StoreLayout<Store> {
    StoreLayout::new(
        Store::new(Arc::new(InMemory::new())),
        "browse-retention".into(),
    )
}

fn indexes(byte: char) -> BrowseIndexes {
    BrowseIndexes::new(byte.to_string().repeat(64), "b".repeat(64), None).unwrap()
}

#[cfg(feature = "remote-index")]
async fn integrity_fixture() -> (StoreLayout<Store>, Vec<object_store::path::Path>) {
    use super::super::{
        Capsule, CapsuleGitPack, CapsuleRefEdit, CapsuleRun, CapsuleTransaction, PackLayer,
        PointerCatalog,
    };
    use crate::git_object_locator::{
        GitObjectLocation, GitObjectLocator, GitObjectMetadata, GitPackInventoryEntry,
        PagedGitObjectIndexWrite,
    };
    use crate::path_state::{PathStateInput, append_path_state, upload_path_state};
    use crate::split_commit_graph::{
        CommitGraphInput, append_paged_commit_graph, load_split_commit_graph,
        upload_split_commit_graph,
    };

    let layout = fixture();
    let mut manifest = crate::manifests::Manifest::default_for_repo("refs/heads/main");
    manifest.generation = 1;
    let pack_id = crab_xet::hash::MerkleHash::from([3; 32]);
    let inventory = [GitPackInventoryEntry {
        pack_id,
        object_count: 1,
        pack_size: 42,
    }];
    manifest.pack_index_hash = crate::manifests::compact_pack_index(
        1,
        &[crate::manifests::PackManifestEntry {
            pack_id: pack_id.to_string(),
            size: 42,
            content_hash: pack_id.to_string(),
            ref_tips: Vec::new(),
            object_count: 1,
        }],
    )
    .unwrap()
    .0;
    manifest.git_validation_digest = "b".repeat(64);
    let graph = append_paged_commit_graph(
        None,
        1,
        manifest.pack_index_hash.clone(),
        manifest.git_validation_digest.clone(),
        &[[1; 20]],
        vec![CommitGraphInput {
            oid: [1; 20],
            tree_oid: [2; 20],
            commit_time: 1,
            parents: Vec::new(),
        }],
    )
    .unwrap()
    .unwrap();
    upload_split_commit_graph(layout.store(), &layout, &graph)
        .await
        .unwrap();
    let graph_hash = graph.descriptor_hash.clone();
    let mut paths = vec![layout.repo_path(&graph.layers[0].reference.path)];
    let graph = load_split_commit_graph(layout.store(), &layout, &graph_hash, 1024 * 1024)
        .await
        .unwrap();
    let path_state = append_path_state(
        None,
        &graph,
        vec![PathStateInput {
            oid: [1; 20],
            first_parent: None,
            author: Vec::new(),
            author_seconds: 1,
            message: Vec::new(),
            mutations: Vec::new(),
        }],
    )
    .unwrap();
    upload_path_state(layout.store(), &layout, &path_state, 1024 * 1024)
        .await
        .unwrap();
    paths.push(layout.repo_path(&path_state.layers[0].reference.path));
    let entries = [(
        [1; 20],
        GitObjectLocator {
            ordinal: 0,
            pack_id,
            location: GitObjectLocation {
                pack_offset: 12,
                entry_len: 10,
                crc32: 0,
            },
            metadata: GitObjectMetadata::default(),
        },
    )];
    let physical =
        PagedGitObjectIndexWrite::build(&"a".repeat(64), &manifest, &inventory, &entries).unwrap();
    let hash = physical.upload(layout.store(), &layout).await.unwrap();
    paths.push(
        crate::git_object_locator::PagedGitObjectIndex::retained_paths(
            layout.store(),
            &layout,
            &hash,
            &"a".repeat(64),
            1024 * 1024,
        )
        .await
        .unwrap()[1]
            .clone(),
    );
    let indexes = BrowseIndexes::new(
        "a".repeat(64),
        graph_hash.clone(),
        Some(path_state.descriptor_hash),
    )
    .unwrap()
    .with_object_index(hash)
    .unwrap();
    let transaction = CapsuleTransaction::new(
        &"c".repeat(64),
        vec![CapsuleRefEdit::new(
            "refs/heads/main",
            None,
            Some("1".repeat(40)),
            None,
        )],
    )
    .unwrap();
    let run =
        CapsuleRun::leaf(Capsule::build(&transaction, Vec::new(), Vec::new()).unwrap()).unwrap();
    let run_path = layout.capsule_path(run.hash());
    layout
        .store()
        .put(&run_path, run.bytes().clone())
        .await
        .unwrap();
    paths.push(run_path);
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
    let pack = CapsuleGitPack::new(
        Bytes::from(vec![0x42; 1024]),
        Bytes::from_static(b"index"),
        Bytes::from_static(b"reverse"),
        Bytes::from_static(b"locator"),
        "3".repeat(40),
        1,
    )
    .unwrap();
    let layer = PackLayer::build(&pack).unwrap();
    let layer_path = layout.capsule_pack_layer_path(layer.hash());
    layout
        .store()
        .put(&layer_path, layer.bytes().clone())
        .await
        .unwrap();
    paths.push(layer_path);
    let mut catalog = PointerCatalog::new();
    catalog
        .insert_shard(
            "a".repeat(64),
            super::super::ShardCatalogEntry::new(1, Vec::new()),
        )
        .unwrap();
    catalog
        .insert_file(
            "b".repeat(64),
            super::super::FileCatalogEntry::new(1, "a".repeat(64)),
        )
        .unwrap();
    let checkpoint = LayeredCheckpoint::build(
        1,
        &"c".repeat(64),
        vec![layer.source_descriptor().unwrap()],
        catalog,
        None,
    )
    .unwrap();
    checkpoint.publish_catalog(&layout).await.unwrap();
    paths.push(layout.capsule_catalog_path(checkpoint.catalog().unwrap().hash()));
    let checkpoint_path = layout.capsule_checkpoint_path(checkpoint.hash());
    layout
        .store()
        .put(&checkpoint_path, checkpoint.bytes().clone())
        .await
        .unwrap();
    paths.push(checkpoint_path);
    retain_browse_indexes(&layout, &indexes, 1, Some(&checkpoint), &[pointer])
        .await
        .unwrap();
    // The old bodies now belong only to retained capture lifetime, not the
    // latest readiness or any authoritative checkpoint-history chain.
    let latest = BrowseIndexes::new("d".repeat(64), graph_hash, None).unwrap();
    retain_browse_indexes(&layout, &latest, 1, None, &[])
        .await
        .unwrap();
    layout
        .store()
        .create_strict_with_etag(
            &layout.capsule_browse_indexes_path(),
            latest.encode().unwrap(),
        )
        .await
        .unwrap();
    (layout, paths)
}

#[tokio::test]
#[cfg(feature = "remote-index")]
async fn integrity_checks_every_retained_body_after_readiness_replacement() {
    let (layout, paths) = integrity_fixture().await;
    verify_retained_browse_indexes(&layout, 1024 * 1024)
        .await
        .unwrap();
    for path in paths {
        let original = layout.store().get_with_etag(&path).await.unwrap().0;
        for missing in [false, true] {
            if missing {
                layout.store().delete(&path).await.unwrap();
            } else {
                layout
                    .store()
                    .put_overwrite(&path, Bytes::from_static(b"corrupt"))
                    .await
                    .unwrap();
            }
            assert!(
                verify_retained_browse_indexes(&layout, 1024 * 1024)
                    .await
                    .is_err(),
                "missing={missing}, path={path}"
            );
            layout
                .store()
                .put_overwrite(&path, original.clone())
                .await
                .unwrap();
        }
    }
    verify_retained_browse_indexes(&layout, 1024 * 1024)
        .await
        .unwrap();
}

#[tokio::test]
#[cfg(feature = "remote-index")]
async fn verified_capture_distinguishes_control_movement_from_corruption() {
    for readiness in [false, true] {
        let layout = fixture();
        let capture = verify_retained_browse_indexes(&layout, 1024 * 1024)
            .await
            .unwrap();
        assert!(capture.is_current(&layout).await.unwrap());
        let path = if readiness {
            let path = layout.capsule_browse_indexes_path();
            layout
                .store()
                .put(&path, indexes('a').encode().unwrap())
                .await
                .unwrap();
            path
        } else {
            retain_browse_indexes(&layout, &indexes('a'), 1, None, &[])
                .await
                .unwrap();
            layout.browse_retained_head_path()
        };
        assert!(!capture.is_current(&layout).await.unwrap());
        layout
            .store()
            .put_overwrite(&path, Bytes::from_static(b"corrupt"))
            .await
            .unwrap();
        assert!(capture.is_current(&layout).await.is_err());
    }
}

#[tokio::test]
#[cfg(feature = "remote-index")]
async fn advertised_readiness_requires_authenticated_lifetime_ownership() {
    let layout = fixture();
    layout
        .store()
        .create_strict_with_etag(
            &layout.capsule_browse_indexes_path(),
            indexes('a').encode().unwrap(),
        )
        .await
        .unwrap();
    assert!(
        verify_retained_browse_indexes(&layout, 1024 * 1024)
            .await
            .is_err()
    );
}

#[tokio::test]
#[cfg(feature = "remote-index")]
async fn retained_physical_index_must_match_its_graph_and_state() {
    for field in [
        "state_digest",
        "generation",
        "pack_index_hash",
        "git_validation_digest",
    ] {
        let (layout, _) = integrity_fixture().await;
        let records = load_retained_browse_indexes(&layout).await.unwrap();
        let indexes = records[1].indexes();
        let path =
            layout.bulk_manifest_path("git-object-index", indexes.object_index_hash().unwrap());
        let bytes = layout.store().get_with_etag(&path).await.unwrap().0;
        let mut descriptor: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        descriptor[field] = if field == "generation" {
            serde_json::json!(2)
        } else {
            serde_json::json!("e".repeat(64))
        };
        let bytes = Bytes::from(serde_json::to_vec(&descriptor).unwrap());
        let hash = blake3::hash(&bytes).to_hex().to_string();
        layout
            .store()
            .put(&layout.bulk_manifest_path("git-object-index", &hash), bytes)
            .await
            .unwrap();
        let indexes = indexes.clone().with_object_index(hash).unwrap();
        retain_browse_indexes(&layout, &indexes, 1, None, &[])
            .await
            .unwrap();
        assert!(
            verify_retained_browse_indexes(&layout, 1024 * 1024)
                .await
                .is_err(),
            "field={field}"
        );
    }
}

#[tokio::test]
async fn unchanged_capture_does_not_grow_retention_or_rewrite_its_head() {
    let layout = fixture();
    retain_browse_indexes(&layout, &indexes('a'), 1, None, &[])
        .await
        .unwrap();
    let before = layout
        .store()
        .get_with_etag_bounded(&layout.browse_retained_head_path(), 4096)
        .await
        .unwrap();
    retain_browse_indexes(&layout, &indexes('a'), 1, None, &[])
        .await
        .unwrap();
    let after = layout
        .store()
        .get_with_etag_bounded(&layout.browse_retained_head_path(), 4096)
        .await
        .unwrap();
    assert_eq!(before, after);
}

#[tokio::test]
async fn replacement_retains_prior_coverage_even_without_query_readiness() {
    let layout = fixture();
    retain_browse_indexes(&layout, &indexes('a'), 1, None, &[])
        .await
        .unwrap();
    retain_browse_indexes(&layout, &indexes('c'), 2, None, &[])
        .await
        .unwrap();
    let roots = load_retained_browse_indexes(&layout).await.unwrap();
    assert_eq!(
        roots
            .iter()
            .map(|root| root.indexes().state_digest())
            .collect::<Vec<_>>(),
        vec!["c".repeat(64), "a".repeat(64)]
    );
}

#[tokio::test]
async fn explicit_pruning_keeps_current_readiness_and_relinks_retained_generations() {
    let layout = fixture();
    for (generation, byte) in [(1, 'a'), (2, 'c'), (3, 'd'), (4, 'e')] {
        retain_browse_indexes(&layout, &indexes(byte), generation, None, &[])
            .await
            .unwrap();
    }
    // A lifetime publication may precede a failed readiness CAS. Preserve the
    // older advertised root even when it is below the explicit pruning floor.
    layout
        .store()
        .create_strict_with_etag(
            &layout.capsule_browse_indexes_path(),
            indexes('a').encode().unwrap(),
        )
        .await
        .unwrap();
    prune_retained_browse_indexes(&layout, 3).await.unwrap();
    let roots = load_retained_browse_indexes(&layout).await.unwrap();
    assert_eq!(
        roots
            .iter()
            .map(RetainedBrowseIndexes::generation)
            .collect::<Vec<_>>(),
        vec![4, 3, 1]
    );
}

#[tokio::test]
async fn corrupt_or_missing_retained_roots_fail_closed() {
    for missing in [false, true] {
        let layout = fixture();
        retain_browse_indexes(&layout, &indexes('a'), 1, None, &[])
            .await
            .unwrap();
        let roots = load_retained_browse_indexes(&layout).await.unwrap();
        let path = layout.browse_retained_root_path(roots[0].hash());
        if missing {
            layout.store().delete(&path).await.unwrap();
        } else {
            layout
                .store()
                .put_overwrite(&path, Bytes::from_static(b"corrupt"))
                .await
                .unwrap();
        }
        assert!(load_retained_browse_indexes(&layout).await.is_err());
    }
}

#[tokio::test]
async fn backwards_publication_preserves_the_lifetime_head() {
    let layout = fixture();
    retain_browse_indexes(&layout, &indexes('a'), 2, None, &[])
        .await
        .unwrap();
    let before = layout
        .store()
        .get_with_etag_bounded(&layout.browse_retained_head_path(), 4096)
        .await
        .unwrap();
    assert!(
        retain_browse_indexes(&layout, &indexes('c'), 1, None, &[])
            .await
            .is_err()
    );
    assert_eq!(
        layout
            .store()
            .get_with_etag_bounded(&layout.browse_retained_head_path(), 4096)
            .await
            .unwrap(),
        before
    );
}

#[test]
fn retention_head_bounds_are_admitted_before_publishing() {
    for (roots, bytes) in [
        (MAX_HISTORY_CHAIN_SEGMENTS, 1),
        (1, MAX_HISTORY_CHAIN_BYTES),
    ] {
        let old = Head {
            version: 1,
            hash: "a".repeat(64),
            roots,
            bytes,
        };
        assert!(Head::append(Some(&old), &Bytes::from_static(b"new root")).is_err());
    }
}

#[tokio::test]
async fn an_expired_publisher_cannot_replace_a_successor_lifetime_head() {
    let layout = fixture();
    retain_browse_indexes(&layout, &indexes('a'), 1, None, &[])
        .await
        .unwrap();
    let (old, etag) = load_head(&layout).await.unwrap().unwrap();
    retain_browse_indexes(&layout, &indexes('c'), 2, None, &[])
        .await
        .unwrap();
    let result = publish_head(&layout, &old, Some(etag)).await;
    assert!(matches!(
        result,
        Err(MetadataError::Storage {
            source: crab_storage::StorageError::StateConflict { .. }
        })
    ));
}

#[tokio::test]
async fn captures_keep_ref_only_visibility_sources_independently_of_current_packaging() {
    use crate::capsule_protocol::{Capsule, CapsuleRefEdit, CapsuleRun, CapsuleTransaction};
    let layout = fixture();
    let transaction = CapsuleTransaction::new(
        &"a".repeat(64),
        vec![CapsuleRefEdit::new(
            "refs/heads/main",
            None,
            Some("1".repeat(40)),
            None,
        )],
    )
    .unwrap();
    let capsule = Capsule::build(&transaction, Vec::new(), Vec::new()).unwrap();
    let run = CapsuleRun::leaf(capsule).unwrap();
    let source = CapsulePointer::new(
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
    retain_browse_indexes(&layout, &indexes('a'), 1, None, &[source])
        .await
        .unwrap();
    retain_browse_indexes(&layout, &indexes('c'), 2, None, &[])
        .await
        .unwrap();
    let roots = load_retained_browse_indexes(&layout).await.unwrap();
    assert_eq!(
        roots[1].source_paths(&layout),
        vec![layout.capsule_path(run.hash())]
    );
}

#[tokio::test]
async fn retrying_completed_pruning_does_not_rewrite_the_lifetime_head() {
    let layout = fixture();
    for (generation, byte) in [(1, 'a'), (2, 'c'), (3, 'd')] {
        retain_browse_indexes(&layout, &indexes(byte), generation, None, &[])
            .await
            .unwrap();
    }
    prune_retained_browse_indexes(&layout, 2).await.unwrap();
    let before = layout
        .store()
        .get_with_etag_bounded(&layout.browse_retained_head_path(), 4096)
        .await
        .unwrap();
    prune_retained_browse_indexes(&layout, 2).await.unwrap();
    assert_eq!(
        layout
            .store()
            .get_with_etag_bounded(&layout.browse_retained_head_path(), 4096)
            .await
            .unwrap(),
        before
    );
}

#[tokio::test]
#[cfg(feature = "remote-index")]
async fn authenticated_retention_cannot_omit_checkpoint_dependencies() {
    for corruption in ["catalog-omitted", "catalog-replaced", "source-omitted"] {
        let (layout, _) = integrity_fixture().await;
        let mut records = load_retained_browse_indexes(&layout).await.unwrap();
        let mut payload = records.pop().unwrap().payload;
        payload.previous = None;
        match corruption {
            "catalog-omitted" => payload.catalog = None,
            "catalog-replaced" => payload.catalog = Some("f".repeat(64)),
            _ => payload.sources.clear(),
        }
        // Rehash the malformed record: raw object integrity alone cannot
        // prove that its dependency list owns the authenticated checkpoint.
        let bytes = payload.encode().unwrap();
        let head = Head::append(None, &bytes).unwrap();
        layout
            .store()
            .put_overwrite(&layout.browse_retained_root_path(&head.hash), bytes)
            .await
            .unwrap();
        layout
            .store()
            .put_overwrite(
                &layout.browse_retained_head_path(),
                Bytes::from(serde_json::to_vec(&head).unwrap()),
            )
            .await
            .unwrap();
        assert!(
            load_retained_browse_indexes(&layout).await.is_err(),
            "{corruption}"
        );
    }
}
