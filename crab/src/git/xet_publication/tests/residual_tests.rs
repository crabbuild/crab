use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn mixed_prepared_xorb_uploads_only_unresolved_chunks() {
    use crab_staging::stream::{
        StreamStageProgress, StreamStageXorbBuilder, stage_reader_streaming,
    };
    use std::sync::atomic::{AtomicU64, Ordering};

    #[derive(Default)]
    struct Reads(AtomicU64);
    impl crab_staging::StagingMetrics for Reads {
        fn inc_prepared_source_xorb_open(&self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
    let root = tempfile::tempdir().unwrap();
    let staging = crab_staging::StagingArea::open(root.path().to_owned())
        .await
        .unwrap();
    let mut bytes = vec![0; 32 * 1024 * 1024];
    blake3::Hasher::new()
        .update(b"mixed prepared publication")
        .finalize_xof()
        .fill(&mut bytes);
    let staged = stage_reader_streaming(
        &mut std::io::Cursor::new(&bytes),
        Path::new("model.bin"),
        &staging,
        StreamStageProgress {
            xorb_builder: Some(StreamStageXorbBuilder::new(1, || {
                XorbBuilder::with_policy(Arc::new(FixedCompression::new(CompressionScheme::None))
                    as Arc<dyn CompressionPolicy>)
            })),
            ..Default::default()
        },
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    staging
        .delegate_publication(&[(&staged.batch_id, Path::new("model.bin"))])
        .unwrap();
    staging.close().await.unwrap();
    let mut staging = StagingAreaReadOnly::open(root.path().to_owned())
        .await
        .unwrap();
    let reads = Arc::new(Reads::default());
    staging.set_metrics(reads.clone());
    let staging = Arc::new(staging);
    let plans = staging
        .prepared_xorbs_for_recipes(&[MerkleHash::from(staged.recipe.hash())])
        .unwrap();
    let plans = &plans[&MerkleHash::from(staged.recipe.hash())];
    assert_eq!(plans.len(), 1);
    let original = XorbParser::parse(Bytes::from(
        std::fs::read(crab_staging::push_plan::prepared_xorb_path(
            staging.root(),
            &plans[0].hash().unwrap(),
        ))
        .unwrap(),
    ))
    .unwrap();
    let count = plans[0].placements.len();
    assert!(
        count > 257,
        "fixture must span the old 256-chunk read batches"
    );
    // Exercise both a small edit and an almost entirely new recipe. The
    // latter must not reopen the same prepared body for every read batch.
    for committed_count in [count - 1, 1] {
        let mut builder =
            XorbBuilder::with_policy(Arc::new(FixedCompression::new(CompressionScheme::None))
                as Arc<dyn CompressionPolicy>);
        for index in 0..committed_count {
            builder
                .push(&original.get_chunk(index as u32).unwrap(), RunId(0))
                .unwrap();
        }
        let mut results = builder.finalize().unwrap();
        assert_eq!(results.len(), 1);
        let committed = results.remove(0);
        let entry = catalog_xorb_entry(
            committed.bytes.len() as u64,
            blake3::hash(&committed.bytes).to_hex().to_string(),
            &committed.placements,
        )
        .unwrap();
        let mut catalog = PointerCatalog::new();
        catalog.insert_xorb(committed.hash.hex(), entry).unwrap();
        // Only the selected new shard is reconstructed below. This old
        // binding supplies the authenticated reverse-chunk lookup fixture.
        let shard = "a".repeat(64);
        catalog
            .insert_shard(
                shard.clone(),
                ShardCatalogEntry::new(1, vec![committed.hash.hex()]),
            )
            .unwrap();
        catalog
            .insert_file("b".repeat(64), FileCatalogEntry::new(1, shard))
            .unwrap();
        let (layout, base) = publish_catalog(catalog.encode().unwrap()).await;
        layout
            .store()
            .create_strict(&layout.xorb_path(&committed.hash), committed.bytes)
            .await
            .unwrap();
        reads.0.store(0, Ordering::Relaxed);
        let pointer = Pointer {
            file_hash: staged.file_hash,
            size: staged.size,
            shard_hint: None,
        };
        let (delta, stats) = prepare_delta(
            &layout,
            &base,
            std::slice::from_ref(&pointer),
            Some(&staging),
            None,
            None,
            &Default::default(),
            1,
            false,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        crab_read::verify_catalog_file_recipe(
            &layout,
            &pointer,
            &delta.files()[&MerkleHash::from(staged.file_hash).hex()],
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        let expected = plans[0]
            .placements
            .iter()
            .skip(committed_count)
            .map(|placement| placement.chunk_hash.clone())
            .collect::<HashSet<_>>();
        let actual = delta
            .xorbs()
            .values()
            .flat_map(|entry| entry.chunks())
            .map(|chunk| chunk.hash().to_owned())
            .collect::<HashSet<_>>();
        eprintln!(
            "committed={committed_count}/{count}, uploaded_bytes={}, prepared_reads={}",
            stats.xorb_bytes_uploaded,
            reads.0.load(Ordering::Relaxed)
        );
        assert_eq!(
            (actual, reads.0.load(Ordering::Relaxed)),
            (expected, 1),
            "publication must upload only missing chunks and open their source once"
        );
    }
}
