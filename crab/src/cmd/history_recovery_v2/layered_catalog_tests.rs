use super::*;

async fn xet_history_fixture() -> (
    CapsuleStoreLayout<crab_storage::Store>,
    RootSnapshot,
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
    let layout = empty_fixture().await;
    let content = Bytes::from_static(b"historical xet payload");
    let chunk = Chunk::new(content.clone());
    let mut builder = XorbBuilder::new();
    builder.push(&chunk, RunId(0)).unwrap();
    let xorb = builder.finalize().unwrap().pop().unwrap();
    let file_hash = MerkleHash::from(*blake3::hash(&content).as_bytes());
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
        .add_file(file_info_from_placements(file_hash, &[chunk.hash], &placements).unwrap())
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
                vec![XorbChunkEntry::new(chunk.hash.hex(), content.len() as u32)],
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
            FileCatalogEntry::new(content.len() as u64, shard_hash.hex()),
        )
        .unwrap();
    for (path, body) in &bodies {
        layout.store().put(path, body.clone()).await.unwrap();
    }
    let pointer = crab_types::pointer::Pointer {
        file_hash: file_hash.into(),
        size: content.len() as u64,
        shard_hint: None,
    };
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
