use super::*;
use crate::recipe::{ChunkingPolicyId, FileRecipe};
use crate::{StagingArea, StagingMetrics, stream};
use std::sync::atomic::{AtomicU64, Ordering};

async fn segmented() -> (tempfile::TempDir, Vec<u8>, MerkleHash) {
    let root = tempfile::tempdir().unwrap();
    let chunks = (0..600u32)
        .map(|i| {
            let data = (i % 17).to_le_bytes().repeat(13);
            (crab_xet::hash::compute_data_hash(&data), data)
        })
        .collect::<Vec<_>>();
    let content = chunks
        .iter()
        .flat_map(|(_, data)| data.iter().copied())
        .collect::<Vec<_>>();
    let hash = MerkleHash::from(*blake3::hash(&content).as_bytes());
    let staging = StagingArea::open(root.path().to_owned()).await.unwrap();
    staging
        .pre_register_file(&hash, content.len() as u64)
        .unwrap();
    staging
        .stage_chunks_batch(
            &chunks
                .iter()
                .map(|(hash, data)| (hash, data.as_slice()))
                .collect::<Vec<_>>(),
            &hash,
            0,
        )
        .await
        .unwrap();
    let recipe = FileRecipe::from_staged_chunks(
        ChunkingPolicyId::XetGearV1_64KiB,
        hash,
        content.len() as u64,
        &chunks
            .iter()
            .map(|(hash, data)| (*hash, data.len() as u64))
            .collect::<Vec<_>>(),
    )
    .unwrap();
    let batch = staging.create_batch().unwrap();
    staging
        .record_verified_recipe_lease(&batch, Path::new("file.bin"), &recipe)
        .unwrap();
    staging.flush_pending().await.unwrap();
    staging.mark_batch_published(&batch).unwrap();
    staging.close().await.unwrap();
    (root, content, hash)
}

#[tokio::test]
async fn lifetime_pin_preserves_reads_through_retirement_and_exclusive_maintenance() {
    for prepared_source in [false, true] {
        let (root, content, hash) = if prepared_source {
            prepared().await
        } else {
            segmented().await
        };
        let staging_root = if prepared_source {
            root.path().join("staging")
        } else {
            root.path().to_owned()
        };
        if !prepared_source {
            let writer = StagingArea::open(staging_root.clone()).await.unwrap();
            let dead = vec![0x77; 8192];
            let dead_hash = crab_xet::hash::compute_data_hash(&dead);
            writer
                .pre_register_file(&dead_hash, dead.len() as u64)
                .unwrap();
            writer
                .stage_chunks_batch(&[(&dead_hash, dead.as_slice())], &dead_hash, 0)
                .await
                .unwrap();
            {
                let mut segment = writer.writer.lock().await;
                crate::seal_current_segment_blocking(
                    writer.root(),
                    &writer.index,
                    &writer.cfg,
                    None,
                    &mut segment,
                )
                .unwrap();
            }
            writer.unregister_file(&dead_hash).unwrap();
            writer.close().await.unwrap();
        }
        let reader = StagedFileReader::open(staging_root.clone(), hash, content.len() as u64)
            .await
            .unwrap()
            .unwrap();
        let recipe = reader.recipe.clone();
        let pin = reader.into_pin().unwrap();
        // A namespace-local diagnostic PID is not liveness authority. The
        // marker's live OS lock must protect ownership during maintenance.
        std::fs::write(
            crate::read_pin_marker_path(&staging_root, &pin.id),
            format!("pid=4294967295\nid={}\n", pin.id),
        )
        .unwrap();
        let staging = Arc::new(
            StagingAreaReadOnly::open(staging_root.clone())
                .await
                .unwrap(),
        );
        let push = StagingPushSnapshot::begin(staging.clone(), "confirmed".into()).unwrap();
        push.capture(&[recipe]).unwrap();
        push.commit().unwrap();
        let published = staging.published_recipe_for_file(&hash).unwrap();
        drop(staging);
        let writer = StagingArea::open(staging_root.clone()).await.unwrap();
        writer.clean().unwrap();
        let compacted = writer.compact().await.unwrap();
        writer.close().await.unwrap();
        let reader = StagedFileReader::open(staging_root.clone(), hash, content.len() as u64)
            .await
            .unwrap()
            .unwrap();
        let actual = reader
            .read_range(0..content.len() as u64, &CancellationToken::new())
            .await
            .unwrap();
        drop(reader);
        drop(pin);
        let writer = StagingArea::open(staging_root.clone()).await.unwrap();
        writer.clean().unwrap();
        let remaining = writer.list_inflight().unwrap();
        writer.close().await.unwrap();
        let snapshot_count: i64 = rusqlite::Connection::open(staging_root.join("index.db"))
            .unwrap()
            .query_row("SELECT count(*) FROM push_snapshots", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            (
                published.is_none(),
                actual.as_ref(),
                prepared_source || compacted.chunks_moved > 0,
                remaining.is_empty(),
                snapshot_count
            ),
            (true, content.as_slice(), true, true, 0),
            "prepared_source={prepared_source}"
        );
    }
}

#[tokio::test]
async fn paged_ranges_preserve_repeated_occurrences_and_boundaries() {
    let (root, content, hash) = segmented().await;
    let reader = StagedFileReader::open(root.path().to_owned(), hash, content.len() as u64)
        .await
        .unwrap()
        .unwrap();
    for range in [
        1..99,
        26_500..26_800,
        content.len() - 7..content.len(),
        content.len()..content.len(),
        0..content.len(),
    ] {
        assert_eq!(
            reader
                .read_range(
                    range.start as u64..range.end as u64,
                    &CancellationToken::new()
                )
                .await
                .unwrap(),
            &content[range]
        );
    }
}

#[tokio::test]
async fn file_reconstruction_reorders_coalesced_chunks_before_hash_verification() {
    let (root, content, hash) = segmented().await;
    let reader = StagedFileReader::open(root.path().to_owned(), hash, content.len() as u64)
        .await
        .unwrap()
        .unwrap();
    let dest = root.path().join("output");
    reader
        .reconstruct_to_path(&dest, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(std::fs::read(dest).unwrap(), content);
}

#[tokio::test]
async fn sibling_readers_release_temporary_snapshots_without_retirement() {
    let (root, content, hash) = segmented().await;
    let first = StagedFileReader::open(root.path().to_owned(), hash, content.len() as u64)
        .await
        .unwrap()
        .unwrap();
    let second = StagedFileReader::open(root.path().to_owned(), hash, content.len() as u64)
        .await
        .unwrap()
        .unwrap();
    drop(first);
    let count: i64 = rusqlite::Connection::open(root.path().join("index.db"))
        .unwrap()
        .query_row("SELECT count(*) FROM push_snapshots", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);
    drop(second);
    let db = rusqlite::Connection::open(root.path().join("index.db")).unwrap();
    let count: i64 = db
        .query_row("SELECT count(*) FROM push_snapshots", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 0);
}

#[tokio::test]
async fn active_reader_defers_push_retirement_until_its_bytes_are_consumed() {
    let (root, content, hash) = segmented().await;
    let reader = StagedFileReader::open(root.path().to_owned(), hash, content.len() as u64)
        .await
        .unwrap()
        .unwrap();
    let staging = Arc::new(
        StagingAreaReadOnly::open(root.path().to_owned())
            .await
            .unwrap(),
    );
    let push = StagingPushSnapshot::begin(staging.clone(), "published".into()).unwrap();
    push.capture(std::slice::from_ref(&reader.recipe)).unwrap();
    push.commit().unwrap();
    let bytes = reader
        .read_range(0..content.len() as u64, &CancellationToken::new())
        .await
        .unwrap();
    drop(reader);
    staging.retire_push_snapshot("published").await.unwrap();
    assert_eq!(
        (bytes, staging.published_recipe_for_file(&hash).unwrap()),
        (Bytes::from(content), None)
    );
}

#[derive(Default)]
struct Opens(AtomicU64);
impl StagingMetrics for Opens {
    fn inc_prepared_source_xorb_open(&self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

async fn prepared() -> (tempfile::TempDir, Vec<u8>, MerkleHash) {
    let root = tempfile::tempdir().unwrap();
    let mut random = 7u32;
    let content = (0..2 * 1024 * 1024)
        .map(|_| {
            random = random.wrapping_mul(1664525).wrapping_add(1013904223);
            (random >> 24) as u8
        })
        .collect::<Vec<_>>();
    let path = root.path().join("file.bin");
    std::fs::write(&path, &content).unwrap();
    let staging = StagingArea::open(root.path().join("staging"))
        .await
        .unwrap();
    let staged = stream::stage_file_streaming(
        &path,
        root.path(),
        &staging,
        stream::StreamStageProgress {
            xorb_builder: Some(stream::StreamStageXorbBuilder::new(
                1,
                crab_xet::xorb::builder::XorbBuilder::new,
            )),
            ..Default::default()
        },
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    staging.mark_batch_published(&staged.batch_id).unwrap();
    staging.close().await.unwrap();
    (root, content, MerkleHash::from(staged.file_hash))
}

#[tokio::test]
async fn prepared_range_opens_one_source_for_many_chunks() {
    let (root, content, hash) = prepared().await;
    let metrics = Arc::new(Opens::default());
    let mut staging = StagingAreaReadOnly::open(root.path().join("staging"))
        .await
        .unwrap();
    staging.set_metrics(metrics.clone());
    let reader = StagedFileReader::capture(Arc::new(staging), hash, content.len() as u64)
        .unwrap()
        .unwrap();
    let actual = reader
        .read_range(1..content.len() as u64 - 1, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        (actual.as_ref(), metrics.0.load(Ordering::Relaxed)),
        (&content[1..content.len() - 1], 1)
    );
}

#[tokio::test]
async fn prepared_corruption_after_capture_is_never_returned() {
    let (root, content, hash) = prepared().await;
    let reader = StagedFileReader::open(root.path().join("staging"), hash, content.len() as u64)
        .await
        .unwrap()
        .unwrap();
    let sources = reader
        .staging
        .prepared_xorbs_for_recipes(&[MerkleHash::from(reader.recipe.hash())])
        .unwrap();
    for source in sources.values().flatten() {
        let path = crate::push_plan::prepared_xorb_path(
            reader.staging.root(),
            &MerkleHash::from_hex(&source.hash).unwrap(),
        );
        let mut bytes = std::fs::read(&path).unwrap();
        bytes[0] ^= 1;
        std::fs::write(path, bytes).unwrap();
    }
    assert!(
        reader
            .read_range(0..1024, &CancellationToken::new())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn cancelled_read_releases_its_snapshot_on_drop() {
    let (root, content, hash) = segmented().await;
    let reader = StagedFileReader::open(root.path().to_owned(), hash, content.len() as u64)
        .await
        .unwrap()
        .unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let result = reader.read_range(0..1, &cancel).await;
    drop(reader);
    let staging = StagingAreaReadOnly::open(root.path().to_owned())
        .await
        .unwrap();
    assert!(
        matches!(result, Err(StagingError::Cancelled))
            && staging.list_inflight().unwrap().is_empty()
    );
}

#[tokio::test]
async fn history_publication_preserves_earlier_owners() {
    for same_content in [false, true] {
        let (root, original, hash) = prepared().await;
        let content = if same_content {
            original.clone()
        } else {
            b"replacement bytes".to_vec()
        };
        let file = root.path().join("file.bin");
        std::fs::write(&file, &content).unwrap();
        let staging = StagingArea::open(root.path().join("staging"))
            .await
            .unwrap();
        let next = stream::stage_file_streaming(
            &file,
            root.path(),
            &staging,
            Default::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        staging.close().await.unwrap();
        let staging = StagingAreaReadOnly::open(root.path().join("staging"))
            .await
            .unwrap();
        staging
            .mark_batch_history_published(&next.batch_id)
            .unwrap();
        drop(staging);
        let reader =
            StagedFileReader::open(root.path().join("staging"), hash, original.len() as u64)
                .await
                .unwrap()
                .unwrap();
        assert_eq!(
            reader
                .read_range(0..original.len() as u64, &CancellationToken::new())
                .await
                .unwrap()
                .as_ref(),
            original
        );
    }
}
