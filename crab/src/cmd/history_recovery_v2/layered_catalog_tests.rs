use super::*;

async fn xet_catalog_fixture(
    layout: &CapsuleStoreLayout<crab_storage::Store>,
    parts: &[&[u8]],
    file_hash: [u8; 32],
) -> (
    PointerCatalog,
    crab_types::pointer::Pointer,
    Vec<(object_store::path::Path, Bytes)>,
) {
    use crab_metadata::capsule_protocol::{
        FileCatalogEntry, ShardCatalogEntry, XorbCatalogEntry, XorbChunkEntry,
    };
    use crab_xet::{
        hash::MerkleHash,
        shard::{ShardWriter, file_info_from_placements, xorb_info_from_placements},
        xorb::{
            builder::{RunId, XorbBuilder},
            format::Chunk,
        },
    };
    let size = parts.iter().map(|part| part.len() as u64).sum();
    let chunks = parts
        .iter()
        .map(|part| Chunk::new(Bytes::copy_from_slice(part)))
        .collect::<Vec<_>>();
    let mut builder = XorbBuilder::new();
    for chunk in &chunks {
        builder.push(chunk, RunId(0)).unwrap();
    }
    let xorb = builder.finalize().unwrap().pop().unwrap();
    let file_hash = MerkleHash::from(file_hash);
    let placements = xorb
        .placements
        .iter()
        .cloned()
        .map(|p| (p.chunk_hash, p))
        .collect();
    let mut writer = ShardWriter::new();
    writer
        .add_xorb(Arc::new(
            xorb_info_from_placements(xorb.hash, &xorb.placements).unwrap(),
        ))
        .unwrap();
    writer
        .add_file(
            file_info_from_placements(
                file_hash,
                &chunks.iter().map(|chunk| chunk.hash).collect::<Vec<_>>(),
                &placements,
            )
            .unwrap(),
        )
        .unwrap();
    let (shard, shard_hash) = writer.finalize().unwrap();
    let bodies = vec![
        (layout.xorb_path(&xorb.hash), xorb.bytes),
        (layout.shard_path(&shard_hash), Bytes::from(shard)),
    ];
    let mut catalog = PointerCatalog::new();
    catalog
        .insert_xorb(
            xorb.hash.hex(),
            XorbCatalogEntry::new(
                bodies[0].1.len() as u64,
                blake3::hash(&bodies[0].1).to_hex().to_string(),
                chunks
                    .iter()
                    .zip(parts)
                    .map(|(chunk, part)| XorbChunkEntry::new(chunk.hash.hex(), part.len() as u32))
                    .collect(),
            ),
        )
        .unwrap();
    catalog
        .insert_shard(
            shard_hash.hex(),
            ShardCatalogEntry::new(bodies[1].1.len() as u64, vec![xorb.hash.hex()]),
        )
        .unwrap();
    catalog
        .insert_file(
            file_hash.hex(),
            FileCatalogEntry::new(size, shard_hash.hex()),
        )
        .unwrap();
    for (path, body) in &bodies {
        layout.store().put(path, body.clone()).await.unwrap();
    }
    let pointer = crab_types::pointer::Pointer {
        file_hash: file_hash.into(),
        size,
        shard_hint: None,
    };
    (catalog, pointer, bodies)
}

async fn xet_history_fixture() -> (
    CapsuleStoreLayout<crab_storage::Store>,
    RootSnapshot,
    Vec<(object_store::path::Path, Bytes)>,
) {
    let layout = empty_fixture().await;
    let content = b"historical xet payload";
    let (catalog, pointer, bodies) =
        xet_catalog_fixture(&layout, &[content], *blake3::hash(content).as_bytes()).await;
    publish_blob(&layout, "refs/tags/xet", &pointer.serialize(), catalog).await;
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
    (layout, root, bodies)
}

#[tokio::test]
async fn historical_catalog_accounting_counts_distinct_encoded_bodies() {
    let (layout, root, bodies) = xet_history_fixture().await;
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
    let pages = verified
        .checkpoint
        .catalog_objects(&layout, LIMIT)
        .await
        .unwrap();
    let expected_bytes = segment.bytes().len() as u64
        + verified.checkpoint.bytes().len() as u64
        + pages.iter().map(|(_, bytes)| *bytes).sum::<u64>()
        + verified
            .checkpoint
            .sources()
            .iter()
            .map(|source| source.object_size())
            .sum::<u64>()
        + bodies
            .iter()
            .map(|(_, body)| body.len() as u64)
            .sum::<u64>();
    assert_eq!(
        (
            verified.verification.shards,
            verified.verification.xorbs,
            verified.verification.dependency_objects,
            verified.verification.dependency_bytes
        ),
        (
            1,
            1,
            2 + pages.len() as u64
                + verified.checkpoint.sources().len() as u64
                + bodies.len() as u64,
            expected_bytes
        )
    );
}

#[tokio::test]
async fn historical_checkpoint_proof_rejects_corrupt_catalog_payload_bodies() {
    let (layout, root, bodies) = xet_history_fixture().await;
    let chain = history_chain(&layout, &root).await.unwrap();
    let segment = &chain[0];
    for (path, original) in bodies {
        let mut corrupt = original.to_vec();
        corrupt[0] ^= 1;
        layout
            .store()
            .inner()
            .put(&path, Bytes::from(corrupt).into())
            .await
            .unwrap();
        let result = verify_history(
            &layout,
            &root,
            segment.checkpoint().covered_generation(),
            Some(segment.hash()),
            &CancellationToken::new(),
        )
        .await;
        layout
            .store()
            .inner()
            .put(&path, original.into())
            .await
            .unwrap();
        assert!(
            result.is_err(),
            "accepted corrupt historical payload {path}"
        );
    }
}

#[tokio::test]
async fn restore_keeps_newer_recipes_and_newer_only_content() {
    let (layout, old_root, _) = xet_history_fixture().await;
    let chain = history_chain(&layout, &old_root).await.unwrap();
    let historical = &chain[0];
    let content = b"historical xet payload";
    let (mut incoming, pointer, _) = xet_catalog_fixture(
        &layout,
        &[&content[..5], &content[5..]],
        *blake3::hash(content).as_bytes(),
    )
    .await;
    let original_key = crab_xet::hash::MerkleHash::from(pointer.file_hash).hex();
    let replacement = incoming.files()[&original_key].clone();
    let other_content = b"content introduced after the restored checkpoint";
    let (other, other_pointer, _) = xet_catalog_fixture(
        &layout,
        &[other_content],
        *blake3::hash(other_content).as_bytes(),
    )
    .await;
    let other_key = crab_xet::hash::MerkleHash::from(other_pointer.file_hash).hex();
    let other_entry = other.files()[&other_key].clone();
    incoming.apply(&other).unwrap();
    publish_blob(&layout, "refs/tags/newer", b"newer git object", incoming).await;
    let store = Store::from_storage(layout.store().clone());
    let router = StoreLayout::new(store.clone(), layout.repo_prefix().to_owned());
    restore_history(
        &store,
        &router,
        &layout,
        &HistoryRestoreArgs {
            generation: historical.checkpoint().covered_generation(),
            digest: Some(historical.hash().to_owned()),
            apply: true,
            json: false,
        },
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let root = crab_write::capsule_protocol::open_root(&layout)
        .await
        .unwrap();
    let checkpoint = crab_metadata::capsule_protocol::load_layered_checkpoint(
        &layout,
        root.record().root().checkpoint().unwrap(),
        LIMIT,
    )
    .await
    .unwrap();
    let selected = checkpoint
        .pointer_catalog_lookup(&layout, LIMIT)
        .unwrap()
        .files(layout.store(), &[original_key.clone(), other_key.clone()])
        .await
        .unwrap();
    assert_eq!(
        (
            root.record().root().refs(),
            selected.files().get(&original_key),
            selected.files().get(&other_key)
        ),
        (historical.refs(), Some(&replacement), Some(&other_entry))
    );
}

#[tokio::test]
async fn restore_rejects_current_winning_recipe_with_wrong_bytes_before_publication() {
    let (layout, old_root, _) = xet_history_fixture().await;
    let chain = history_chain(&layout, &old_root).await.unwrap();
    let historical = &chain[0];
    let claim = *blake3::hash(b"historical xet payload").as_bytes();
    let (incoming, _, _) = xet_catalog_fixture(&layout, &[b"historical Xet payload"], claim).await;
    publish_blob(&layout, "refs/tags/newer", b"newer git object", incoming).await;
    let before = crab_write::capsule_protocol::open_root(&layout)
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
            generation: historical.checkpoint().covered_generation(),
            digest: Some(historical.hash().to_owned()),
            apply: true,
            json: false,
        },
        &cancel,
    )
    .await;
    let error = result.unwrap_err();
    assert!(
        matches!(&error, CrabError::HashMismatch { requested, actual }
        if *requested == crab_xet::hash::MerkleHash::from(claim).hex()
            && *actual == blake3::hash(b"historical Xet payload").to_hex().as_str()),
        "unexpected error: {error}"
    );
    let after = crab_write::capsule_protocol::open_root(&layout)
        .await
        .unwrap();
    assert_eq!(after.record().digest(), before.record().digest());
    let lease = crate::maintenance::GcSweepLease::acquire(&store, router.repo_prefix(), &cancel)
        .await
        .unwrap();
    lease.release().await.unwrap();
}
