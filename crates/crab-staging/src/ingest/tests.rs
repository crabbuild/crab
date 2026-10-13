use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use super::*;
use crate::stream::{
    stage_file_streaming, stage_file_streaming_as, stage_file_streaming_with_hooks,
};
use crab_xet::xorb::builder::{CompressionPolicy, FixedCompression, XorbResult};
use crab_xet::xorb::format::CompressionScheme;

struct BatchRemoteLookup {
    first_only: bool,
}

#[derive(Default)]
struct PreparedReadCount(AtomicU64);

impl crate::metrics::StagingMetrics for PreparedReadCount {
    fn inc_prepared_source_xorb_open(&self) {
        self.0.fetch_add(1, Relaxed);
    }
}

struct MalformedRemoteLookup;

#[async_trait::async_trait]
impl ExistingChunkLookup for MalformedRemoteLookup {
    async fn lookup_existing_candidates(
        &self,
        _chunks: &[(MerkleHash, u64)],
    ) -> Result<Vec<Option<ExistingChunkCandidate>>> {
        Ok(Vec::new())
    }
}

#[async_trait::async_trait]
impl ExistingChunkLookup for BatchRemoteLookup {
    async fn lookup_existing_candidates(
        &self,
        chunks: &[(MerkleHash, u64)],
    ) -> Result<Vec<Option<ExistingChunkCandidate>>> {
        Ok(chunks
            .iter()
            .enumerate()
            .map(|(index, (chunk_hash, size))| {
                (!self.first_only || index == 0).then(|| {
                    let mut proof = <[u8; 32]>::from(*chunk_hash);
                    proof[0] ^= 0xA5;
                    ExistingChunkCandidate {
                        xorb_ref: crab_xet::xorb::format::XorbRef {
                            xorb_hash: MerkleHash::from(proof),
                            chunk_index: u32::from_le_bytes(proof[..4].try_into().unwrap()),
                            uncompressed_size: (*size).try_into().unwrap(),
                        },
                        placement_id: *blake3::hash(&proof).as_bytes(),
                        origin_proof_id: *blake3::hash(&proof).as_bytes(),
                    }
                })
            })
            .collect())
    }
}

fn write_pattern_file(path: &Path, bytes: usize) {
    write_pattern_file_with_salt(path, bytes, 0);
}

fn write_pattern_file_with_salt(path: &Path, bytes: usize, salt: u64) {
    let mut file = std::fs::File::create(path).unwrap();
    let mut buf = vec![0u8; 1024 * 1024];
    let mut written = 0usize;
    while written < bytes {
        let n = (bytes - written).min(buf.len());
        for (i, byte) in buf[..n].iter_mut().enumerate() {
            let offset = (written + i) as u64;
            *byte = offset
                .wrapping_mul(2_654_435_761)
                .wrapping_add(salt.wrapping_mul(1_103_515_245))
                .wrapping_shr(11) as u8;
        }
        file.write_all(&buf[..n]).unwrap();
        written += n;
    }
}

fn direct_hash_and_chunks(data: &[u8]) -> ([u8; 32], Vec<MerkleHash>) {
    let hash = *blake3::hash(data).as_bytes();
    let mut chunker = GearChunker::new();
    let mut chunks = Vec::new();
    for block in data.chunks(READ_BUF_SIZE) {
        chunks.extend(chunker.feed(block));
    }
    if let Some(last) = chunker.finalize() {
        chunks.push(last);
    }
    (
        hash,
        chunks
            .into_iter()
            .map(|c| MerkleHash::from(<[u8; 32]>::from(c.hash)))
            .collect(),
    )
}

fn multi_batch_fixture_bytes() -> usize {
    usize::try_from(STAGE_BATCH_TARGET_BYTES)
        .unwrap()
        .saturating_add(READ_BUF_SIZE * 2)
}

fn recipe_pairs(staging: &StagingArea, result: &StreamStageResult) -> Vec<(MerkleHash, u64)> {
    let mut pairs = Vec::new();
    let mut next = 0u64;
    while next < result.recipe.chunk_count() {
        let page = staging.recipe_page(&result.recipe, next).unwrap();
        pairs.extend(
            page.chunks
                .iter()
                .map(|chunk| (chunk.chunk_hash, chunk.len)),
        );
        next = page.next_occurrence();
    }
    pairs
}

fn recipe_remote_chunks(
    staging: &StagingArea,
    recipe: &crate::recipe::FileRecipe,
) -> Vec<(MerkleHash, crate::push_plan::ExistingChunkCandidate)> {
    let mut chunks = Vec::new();
    let mut next = 0u64;
    while next < recipe.chunk_count() {
        let page = staging
            .recipe_remote_chunk_page(recipe, next)
            .expect("remote authority page");
        assert!(page.len() <= crate::recipe::RECIPE_PAGE_ENTRIES);
        chunks.extend(page);
        next += crate::recipe::RECIPE_PAGE_ENTRIES as u64;
    }
    chunks
}

#[test]
fn direct_xorb_staging_uses_bounded_remote_batches() {
    assert_eq!(stage_batch_target_bytes(false), STAGE_BATCH_TARGET_BYTES);
    assert_eq!(
        stage_batch_target_bytes(true),
        DIRECT_XORB_STAGE_BATCH_TARGET_BYTES
    );
    let lengths = vec![1024 * 1024; 64];
    assert_eq!(
        batch_flush_prefix_from_lengths(
            lengths.iter().copied(),
            DIRECT_XORB_STAGE_BATCH_TARGET_BYTES,
        ),
        (64, DIRECT_XORB_STAGE_BATCH_TARGET_BYTES)
    );
}

#[test]
fn phase_timings_convert_after_accumulating_submillisecond_work() {
    let mut timings = StreamStageTimingAccumulator::default();
    timings.chunking = timings.chunking.saturating_add(Duration::from_micros(600));
    timings.chunking = timings.chunking.saturating_add(Duration::from_micros(600));

    assert_eq!(timings.finish().chunking_duration_ms, 1);
}

#[tokio::test]
async fn xorb_builder_admission_is_shared_and_cancel_safe() {
    let builders = StreamStageXorbBuilder::new(1, XorbBuilder::new);
    let cancel = CancellationToken::new();
    let (first_permit, _first_builder) = builders.build(&cancel).await.unwrap();

    assert_eq!(builders.available_permits(), 0);

    let waiting_cancel = CancellationToken::new();
    waiting_cancel.cancel();
    let error = match builders.build(&waiting_cancel).await {
        Ok(_) => panic!("cancelled admission unexpectedly built a xorb builder"),
        Err(error) => error,
    };
    assert!(matches!(error, CrabError::Cancelled));

    drop(first_permit);
    let (_second_permit, _second_builder) = builders.build(&cancel).await.unwrap();
}

#[tokio::test]
async fn prepared_xorb_ownership_is_globally_bounded() {
    let builders = StreamStageXorbBuilder::new(2, XorbBuilder::new);
    let first = builders
        .acquire_materialization(&CancellationToken::new())
        .await
        .unwrap();
    let waiting_builders = builders.clone();
    let waiting = tokio::spawn(async move {
        waiting_builders
            .acquire_materialization(&CancellationToken::new())
            .await
    });

    tokio::task::yield_now().await;
    assert!(!waiting.is_finished());
    assert_eq!(builders.available_materialization_permits(), 0);

    drop(first);
    let second = waiting.await.unwrap().unwrap();
    assert_eq!(builders.available_materialization_permits(), 0);
    drop(second);
    assert_eq!(builders.available_materialization_permits(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn direct_stream_partial_overlap_has_one_canonical_prepared_placement() {
    let dir = tempfile::tempdir().unwrap();
    let shared_path = dir.path().join("shared.bin");
    let first_prefix_path = dir.path().join("first-prefix.bin");
    let second_prefix_path = dir.path().join("second-prefix.bin");
    write_pattern_file_with_salt(&shared_path, 3 * 1024 * 1024, 17);
    write_pattern_file_with_salt(&first_prefix_path, 1024 * 1024, 29);
    write_pattern_file_with_salt(&second_prefix_path, 1024 * 1024, 41);
    let shared = std::fs::read(shared_path).unwrap();
    let mut first_bytes = std::fs::read(first_prefix_path).unwrap();
    first_bytes.extend_from_slice(&shared);
    let mut second_bytes = std::fs::read(second_prefix_path).unwrap();
    second_bytes.extend_from_slice(&shared);
    let first_path = dir.path().join("first.bin");
    let second_path = dir.path().join("second.bin");
    std::fs::write(&first_path, &first_bytes).unwrap();
    std::fs::write(&second_path, &second_bytes).unwrap();

    let staging = StagingArea::open(dir.path().join(".crab/staging"))
        .await
        .unwrap();
    let preparation = staging.create_add_preparation().unwrap();
    let builders =
        StreamStageXorbBuilder::new(2, XorbBuilder::new).bind_preparation(preparation.clone());
    let cancel = CancellationToken::new();
    let first = stage_file_streaming(
        &first_path,
        dir.path(),
        &staging,
        StreamStageProgress {
            xorb_builder: Some(builders.clone()),
            ..StreamStageProgress::default()
        },
        &cancel,
    );
    let second = stage_file_streaming(
        &second_path,
        dir.path(),
        &staging,
        StreamStageProgress {
            xorb_builder: Some(builders),
            ..StreamStageProgress::default()
        },
        &cancel,
    );
    let (first, second) = tokio::join!(first, second);
    let first = first.unwrap();
    let second = second.unwrap();
    staging.finalize_add_preparation(&preparation).unwrap();
    staging.mark_batch_published(&first.batch_id).unwrap();
    staging.mark_batch_published(&second.batch_id).unwrap();

    let first_recipe = recipe_pairs(&staging, &first);
    let second_recipe = recipe_pairs(&staging, &second);
    let first_hashes = first_recipe
        .iter()
        .map(|(hash, _)| *hash)
        .collect::<std::collections::HashSet<_>>();
    let second_hashes = second_recipe
        .iter()
        .map(|(hash, _)| *hash)
        .collect::<std::collections::HashSet<_>>();
    let shared_hashes = first_hashes
        .intersection(&second_hashes)
        .copied()
        .collect::<std::collections::HashSet<_>>();
    assert!(
        shared_hashes.len() > 8,
        "fixture must share multiple CDC chunks"
    );

    let first_plan = staging
        .load_file_push_plan(&MerkleHash::from(first.file_hash))
        .await
        .unwrap()
        .unwrap();
    let second_plan = staging
        .load_file_push_plan(&MerkleHash::from(second.file_hash))
        .await
        .unwrap()
        .unwrap();
    let placement_map = |plan: &crate::push_plan::FilePushPlan| {
        plan.prepared_xorbs
            .iter()
            .flat_map(|xorb| {
                xorb.placements.iter().map(|placement| {
                    (
                        MerkleHash::from_hex(&placement.chunk_hash).unwrap(),
                        MerkleHash::from_hex(&placement.xorb_hash).unwrap(),
                    )
                })
            })
            .collect::<std::collections::HashMap<_, _>>()
    };
    let first_placements = placement_map(&first_plan);
    let second_placements = placement_map(&second_plan);
    for chunk_hash in shared_hashes {
        assert_eq!(
            first_placements.get(&chunk_hash),
            second_placements.get(&chunk_hash),
            "shared chunk must resolve to one canonical prepared xorb"
        );
    }
    for (result, expected) in [(&first, &first_bytes), (&second, &second_bytes)] {
        let hashes = recipe_pairs(&staging, result)
            .into_iter()
            .map(|(hash, _)| hash)
            .collect::<Vec<_>>();
        let reconstructed = staging
            .get_chunks_batch(&hashes)
            .await
            .unwrap()
            .into_iter()
            .flat_map(|(_, bytes)| bytes)
            .collect::<Vec<_>>();
        assert_eq!(&reconstructed, expected);
    }
}

#[tokio::test]
async fn concurrent_prepared_xorb_writers_preserve_file_order() {
    fn results(salt: u8) -> Vec<XorbResult> {
        let policy =
            Arc::new(FixedCompression::new(CompressionScheme::None)) as Arc<dyn CompressionPolicy>;
        let mut builder = XorbBuilder::with_policy(policy)
            .with_size_bounds(8, 8)
            .with_max_overshoot(0);
        for byte in 1_u8..=3 {
            let value = byte.wrapping_add(salt);
            builder
                .push(
                    &Chunk {
                        hash: MerkleHash::from([value; 32]),
                        data: Bytes::from(vec![value; 8]),
                    },
                    RunId(0),
                )
                .unwrap();
        }
        builder.finalize().unwrap()
    }

    let dir = tempfile::tempdir().unwrap();
    let builders = StreamStageXorbBuilder::new(2, XorbBuilder::new);
    let first_results = results(0);
    let second_results = results(10);
    let first_hashes = first_results
        .iter()
        .map(|result| result.hash)
        .collect::<Vec<_>>();
    let second_hashes = second_results
        .iter()
        .map(|result| result.hash)
        .collect::<Vec<_>>();
    let mut first = StreamPreparedXorbWriter::spawn(
        dir.path().to_path_buf(),
        MerkleHash::from([40; 32]),
        &builders,
        StreamStageHooks::default(),
    );
    let mut second = StreamPreparedXorbWriter::spawn(
        dir.path().to_path_buf(),
        MerkleHash::from([41; 32]),
        &builders,
        StreamStageHooks::default(),
    );
    let cancel = CancellationToken::new();
    for (left, right) in first_results.into_iter().zip(second_results) {
        let left_permit = builders.acquire_materialization(&cancel).await.unwrap();
        first.submit_reserved(left, left_permit).await.unwrap();
        let right_permit = builders.acquire_materialization(&cancel).await.unwrap();
        second.submit_reserved(right, right_permit).await.unwrap();
    }
    let first = first.finish().await.unwrap();
    let second = second.finish().await.unwrap();

    assert_eq!(
        first
            .xorbs
            .iter()
            .map(|prepared| prepared.hash)
            .collect::<Vec<_>>(),
        first_hashes
    );
    assert_eq!(
        second
            .xorbs
            .iter()
            .map(|prepared| prepared.hash)
            .collect::<Vec<_>>(),
        second_hashes
    );
    for prepared in [&first, &second] {
        for (sequence, prepared) in prepared.xorbs.iter().enumerate() {
            assert!(prepared.payload_path.exists());
            assert!(
                prepared
                    .payload_path
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(&format!("{sequence:016x}-"))
            );
        }
    }
}

#[test]
fn batch_flush_prefix_uses_byte_cap_before_chunk_cap() {
    let half_target = usize::try_from(STAGE_BATCH_TARGET_BYTES / 2).unwrap();
    let (count, bytes) = batch_flush_prefix_from_lengths(
        [half_target, half_target, READ_BUF_SIZE],
        STAGE_BATCH_TARGET_BYTES,
    );

    assert_eq!(count, 2);
    assert_eq!(bytes, STAGE_BATCH_TARGET_BYTES);
}

#[test]
fn batch_flush_prefix_uses_chunk_cap_for_small_chunks() {
    let (count, bytes) = batch_flush_prefix_from_lengths(
        std::iter::repeat_n(1usize, STAGE_BATCH_CHUNKS + 1),
        STAGE_BATCH_TARGET_BYTES,
    );

    assert_eq!(count, STAGE_BATCH_CHUNKS);
    assert_eq!(bytes, STAGE_BATCH_CHUNKS as u64);
}

#[tokio::test]
async fn empty_file_produces_empty_hash_and_zero_chunks() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("empty.bin");
    std::fs::write(&path, []).unwrap();
    let staging = StagingArea::open(dir.path().join(".crab/staging"))
        .await
        .unwrap();

    let result = stage_file_streaming(
        &path,
        dir.path(),
        &staging,
        StreamStageProgress::default(),
        &CancellationToken::new(),
    )
    .await
    .unwrap();

    assert_eq!(result.size, 0);
    assert_eq!(result.file_hash, *blake3::hash(b"").as_bytes());
    assert_eq!(result.chunks, 0);
    assert_eq!(result.recipe.chunk_count(), 0);
    let file_hash = MerkleHash::from(result.file_hash);
    assert!(staging.chunks_for_file(&file_hash).unwrap().is_empty());
}

#[tokio::test]
async fn fragmented_input_preserves_native_recipe_without_payload_rewrite() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("native.bin");
    write_pattern_file(&path, 4 * 1024 * 1024 + 17);
    let bytes = Bytes::from(std::fs::read(&path).unwrap());
    let staging = Arc::new(StagingArea::open(dir.path().join("staging")).await.unwrap());
    let progress = StreamStageProgress {
        xorb_builder: Some(StreamStageXorbBuilder::new(1, XorbBuilder::new)),
        ..Default::default()
    };
    let cancel = CancellationToken::new();
    let native = stage_file_streaming(&path, dir.path(), &staging, progress.clone(), &cancel)
        .await
        .unwrap();
    let mut input =
        IngestSession::begin(&staging, Path::new("stream.bin"), progress, &cancel, None)
            .await
            .unwrap();
    let mut offset = 0;
    let mut random = 0x9351_73abu64;
    while offset < bytes.len() {
        random = random
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        let end = (offset + (random as usize % 131_072) + 1).min(bytes.len());
        input.feed(bytes.slice(offset..end)).await.unwrap();
        offset = end;
    }
    let streamed = input.finish_input().await.unwrap().seal().await.unwrap();
    assert_eq!(streamed.recipe, native.recipe);
    assert!(streamed.prepared_xorbs.is_empty());
    assert_eq!(
        recipe_pairs(&staging, &streamed),
        recipe_pairs(&staging, &native)
    );
    assert_eq!(
        tokio::task::spawn_blocking({
            let staging = Arc::clone(&staging);
            move || staging.stats().unwrap()
        })
        .await
        .unwrap()
        .current_segment_bytes,
        0
    );
}

#[tokio::test]
async fn segment_consumers_reuse_prepared_authority_for_changed_content() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("model.bin");
    let original = (0..129u64)
        .flat_map(|counter| {
            let mut block = vec![0u8; 32 * 1024];
            blake3::Hasher::new()
                .update(&counter.to_le_bytes())
                .finalize_xof()
                .fill(&mut block);
            block
        })
        .collect::<Vec<_>>();
    std::fs::write(&path, original).unwrap();
    let staging = Arc::new(StagingArea::open(dir.path().join("staging")).await.unwrap());
    let cancel = CancellationToken::new();
    let original = stage_file_streaming(
        &path,
        dir.path(),
        &staging,
        StreamStageProgress {
            xorb_builder: Some(StreamStageXorbBuilder::new(1, XorbBuilder::new)),
            ..Default::default()
        },
        &cancel,
    )
    .await
    .unwrap();
    let mut changed = std::fs::read(&path).unwrap();
    let middle = changed.len() / 2;
    changed[middle] ^= 0x3f;
    std::fs::write(&path, &changed).unwrap();
    let retained_staging = Arc::clone(&staging);
    let hooks = StreamStageHooks {
        after_batch_flush: Some(Arc::new(move || {
            retained_staging.rollback_batch(&original.batch_id).unwrap();
        })),
        ..Default::default()
    };
    let staged = stage_file_streaming_with_hooks(
        &path,
        dir.path(),
        &staging,
        StreamStageProgress::default(),
        &cancel,
        hooks,
    )
    .await
    .unwrap();
    let bytes_written = tokio::task::spawn_blocking({
        let staging = Arc::clone(&staging);
        move || staging.stats().unwrap().current_segment_bytes
    })
    .await
    .unwrap();
    assert!(
        bytes_written < 1024 * 1024,
        "shared prepared content was rewritten into {bytes_written} segment bytes"
    );
    let hashes = recipe_pairs(&staging, &staged)
        .into_iter()
        .map(|(hash, _)| hash)
        .collect::<Vec<_>>();
    let reconstructed = staging
        .get_chunks_batch(&hashes)
        .await
        .unwrap()
        .into_iter()
        .flat_map(|(_, bytes)| bytes.to_vec())
        .collect::<Vec<_>>();
    assert_eq!(reconstructed, changed);
}

#[tokio::test]
async fn prepared_verification_and_changed_replay_preserve_all_input_bytes() {
    for change in [
        "exact",
        "beginning",
        "end",
        "truncate",
        "extend",
        "many_boundaries",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("model.bin");
        write_pattern_file(
            &path,
            if change == "many_boundaries" {
                multi_batch_fixture_bytes()
            } else {
                4 * 1024 * 1024 + 17
            },
        );
        let original = std::fs::read(&path).unwrap();
        let mut incoming = original.clone();
        match change {
            "beginning" => incoming[0] ^= 0x3f,
            "end" | "many_boundaries" => *incoming.last_mut().unwrap() ^= 0x3f,
            "truncate" => {
                incoming.truncate(incoming.len() - 8193);
            }
            "extend" => incoming.extend_from_slice(b"appended\0content\n"),
            _ => {}
        }
        let reads = Arc::new(PreparedReadCount::default());
        let mut staging = StagingArea::open(dir.path().join("staging")).await.unwrap();
        staging.set_metrics(Arc::clone(&reads));
        let staging = Arc::new(staging);
        let progress = StreamStageProgress {
            xorb_builder: Some(StreamStageXorbBuilder::new(1, XorbBuilder::new)),
            ..Default::default()
        };
        let cancel = CancellationToken::new();
        let native = stage_file_streaming(&path, dir.path(), &staging, progress.clone(), &cancel)
            .await
            .unwrap();
        let chunks_done = Arc::new(AtomicU64::new(0));
        let mut input = IngestSession::begin(
            &staging,
            Path::new("model.bin"),
            StreamStageProgress {
                chunks_done: Some(Arc::clone(&chunks_done)),
                ..progress
            },
            &cancel,
            Some(native.recipe.clone()),
        )
        .await
        .unwrap();
        // The incoming operation must retain recipe pages and payloads even
        // when the original unpublished owner is rolled back during input.
        staging.rollback_batch(&native.batch_id).unwrap();
        for block in incoming.chunks(31_337) {
            input.feed(Bytes::copy_from_slice(block)).await.unwrap();
        }
        let completed = input.finish_input().await.unwrap();
        assert_eq!(
            completed.file_hash(),
            *blake3::hash(&incoming).as_bytes(),
            "{change}"
        );
        assert!(
            reads.0.load(Relaxed) <= 1,
            "{change} reread the verified prefix"
        );
        let staged = completed.seal().await.unwrap();
        let pairs = recipe_pairs(&staging, &staged);
        let hashes = pairs.iter().map(|(hash, _)| *hash).collect::<Vec<_>>();
        assert_eq!(
            hashes,
            direct_hash_and_chunks(&incoming).1,
            "{change} CDC recipe"
        );
        let reconstructed = staging
            .get_chunks_batch(&hashes)
            .await
            .unwrap()
            .into_iter()
            .flat_map(|(_, bytes)| bytes.to_vec())
            .collect::<Vec<_>>();
        assert_eq!(reconstructed, incoming, "{change}");
        assert_eq!(
            tokio::task::spawn_blocking({
                let staging = Arc::clone(&staging);
                move || staging.stats().unwrap()
            })
            .await
            .unwrap()
            .current_segment_bytes,
            0,
            "{change}"
        );
        if change == "exact" {
            assert_eq!(chunks_done.load(Ordering::Relaxed), 0);
            assert!(staged.prepared_xorbs.is_empty());
        } else {
            assert!(chunks_done.load(Ordering::Relaxed) > 0, "{change}");
        }
    }
}

#[tokio::test]
async fn blocked_durability_checkpoint_prevents_sealing_and_allows_retry() {
    let dir = tempfile::tempdir().unwrap();
    let staging = StagingArea::open(dir.path().join("staging")).await.unwrap();
    crate::lock_index(&staging.index)
        .unwrap()
        .connection()
        .busy_timeout(Duration::ZERO)
        .unwrap();
    let reader = rusqlite::Connection::open(staging.root().join("index.db")).unwrap();
    reader
        .execute_batch("BEGIN; SELECT COUNT(*) FROM staging_meta;")
        .unwrap();
    let bytes = Bytes::from_static(b"durable content\0with binary bytes\n");
    let progress = StreamStageProgress {
        xorb_builder: Some(StreamStageXorbBuilder::new(1, XorbBuilder::new)),
        ..Default::default()
    };
    let cancel = CancellationToken::new();
    let mut input = IngestSession::begin(
        &staging,
        Path::new("content.bin"),
        progress.clone(),
        &cancel,
        None,
    )
    .await
    .unwrap();
    input.feed(bytes.clone()).await.unwrap();
    let completed = input.finish_input().await.unwrap();
    let error = completed.seal().await.unwrap_err();
    assert!(
        matches!(error, CrabError::Io(ref cause) if cause.kind() == std::io::ErrorKind::WouldBlock)
    );
    assert!(
        staging
            .unpublished_local_recipe_for_path(Path::new("content.bin"))
            .unwrap()
            .is_none()
    );
    reader.execute_batch("ROLLBACK;").unwrap();
    let mut retry =
        IngestSession::begin(&staging, Path::new("content.bin"), progress, &cancel, None)
            .await
            .unwrap();
    retry.feed(bytes.clone()).await.unwrap();
    let staged = retry.finish_input().await.unwrap().seal().await.unwrap();
    let hashes = recipe_pairs(&staging, &staged)
        .into_iter()
        .map(|(hash, _)| hash)
        .collect::<Vec<_>>();
    let reconstructed = staging.get_chunks_batch(&hashes).await.unwrap();
    assert_eq!(reconstructed[0].1, bytes);
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn staging_rejects_symlink_without_recording_target_bytes() {
    use std::os::unix::fs::symlink;

    let dir = tempfile::tempdir().unwrap();
    let outside = dir.path().join("outside.bin");
    let link = dir.path().join("link.bin");
    std::fs::write(&outside, b"outside-worktree-secret").unwrap();
    symlink(&outside, &link).unwrap();
    let staging = StagingArea::open(dir.path().join(".crab/staging"))
        .await
        .unwrap();

    let err = stage_file_streaming(
        &link,
        dir.path(),
        &staging,
        StreamStageProgress::default(),
        &CancellationToken::new(),
    )
    .await
    .unwrap_err();

    assert!(matches!(err, CrabError::Configuration { .. }));
    let stats = tokio::task::block_in_place(|| staging.stats()).unwrap();
    assert_eq!(stats.file_count, 0);
    assert_eq!(stats.chunk_count, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn path_replacement_during_stream_never_publishes_replacement() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("replace.bin");
    let displaced = dir.path().join("displaced.bin");
    let fixture_bytes = multi_batch_fixture_bytes();
    write_pattern_file(&path, fixture_bytes);
    let staging = StagingArea::open(dir.path().join(".crab/staging"))
        .await
        .unwrap();

    let path_for_hook = path.clone();
    let displaced_for_hook = displaced.clone();
    let replaced = Arc::new(AtomicBool::new(false));
    let replaced_for_hook = Arc::clone(&replaced);
    let hooks = StreamStageHooks {
        after_batch_flush: Some(Arc::new(move || {
            if !replaced_for_hook.swap(true, Ordering::SeqCst) {
                std::fs::rename(&path_for_hook, &displaced_for_hook).unwrap();
                std::fs::write(&path_for_hook, b"replacement bytes").unwrap();
            }
        })),
        ..StreamStageHooks::default()
    };

    let err = stage_file_streaming_with_hooks(
        &path,
        dir.path(),
        &staging,
        StreamStageProgress::default(),
        &CancellationToken::new(),
        hooks,
    )
    .await
    .unwrap_err();

    assert!(replaced.load(Ordering::SeqCst));
    assert!(matches!(err, CrabError::FileChangedDuringStaging { .. }));
    let stats = tokio::task::block_in_place(|| staging.stats()).unwrap();
    assert_eq!(stats.file_count, 0);
    assert_eq!(stats.chunk_count, 0);
}

#[tokio::test]
async fn streaming_matches_direct_chunking_for_medium_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("medium.bin");
    write_pattern_file(&path, 2 * 1024 * 1024);
    let data = std::fs::read(&path).unwrap();
    let (expected_hash, expected_chunks) = direct_hash_and_chunks(&data);
    let staging = StagingArea::open(dir.path().join(".crab/staging"))
        .await
        .unwrap();

    let result = stage_file_streaming(
        &path,
        dir.path(),
        &staging,
        StreamStageProgress::default(),
        &CancellationToken::new(),
    )
    .await
    .unwrap();

    assert_eq!(result.file_hash, expected_hash);
    assert_eq!(result.size, data.len() as u64);
    assert_eq!(result.chunks, expected_chunks.len());
    let result_pairs = recipe_pairs(&staging, &result);
    let result_chunk_hashes: Vec<_> = result_pairs.iter().map(|(hash, _)| *hash).collect();
    assert_eq!(result_chunk_hashes, expected_chunks);
    let staged = staging
        .chunks_for_file(&MerkleHash::from(result.file_hash))
        .unwrap();
    assert_eq!(staged, expected_chunks);
    let staged_with_sizes = staging
        .chunks_for_file_with_sizes(&MerkleHash::from(result.file_hash))
        .unwrap();
    assert_eq!(result_pairs, staged_with_sizes);
}

#[tokio::test(flavor = "multi_thread")]
async fn rolling_back_one_shared_recipe_batch_preserves_other_lease() {
    let dir = tempfile::tempdir().unwrap();
    let first_path = dir.path().join("first.bin");
    let second_path = dir.path().join("second.bin");
    write_pattern_file(&first_path, 2 * 1024 * 1024);
    std::fs::copy(&first_path, &second_path).unwrap();
    let staging = StagingArea::open(dir.path().join(".crab/staging"))
        .await
        .unwrap();

    let first = stage_file_streaming(
        &first_path,
        dir.path(),
        &staging,
        StreamStageProgress::default(),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let second = stage_file_streaming(
        &second_path,
        dir.path(),
        &staging,
        StreamStageProgress::default(),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(first.file_hash, second.file_hash);
    assert_ne!(first.batch_id, second.batch_id);

    staging.rollback_batch(&second.batch_id).unwrap();
    let file_hash = MerkleHash::from(first.file_hash);
    assert_eq!(
        staging.chunks_for_file(&file_hash).unwrap(),
        recipe_pairs(&staging, &first)
            .iter()
            .map(|(chunk_hash, _)| *chunk_hash)
            .collect::<Vec<_>>()
    );

    staging.rollback_batch(&first.batch_id).unwrap();
    assert!(staging.chunks_for_file(&file_hash).unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn committed_retirement_preserves_same_recipe_owned_by_open_batch() {
    let dir = tempfile::tempdir().unwrap();
    let first_path = dir.path().join("published.bin");
    let second_path = dir.path().join("open.bin");
    write_pattern_file(&first_path, 2 * 1024 * 1024);
    std::fs::copy(&first_path, &second_path).unwrap();
    let root = dir.path().join(".crab/staging");
    let staging = StagingArea::open(root.clone()).await.unwrap();
    let first = stage_file_streaming(
        &first_path,
        dir.path(),
        &staging,
        StreamStageProgress::default(),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let second = stage_file_streaming(
        &second_path,
        dir.path(),
        &staging,
        StreamStageProgress::default(),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    staging.mark_batch_published(&first.batch_id).unwrap();
    staging.close().await.unwrap();

    let reader = crate::StagingAreaReadOnly::open(root).await.unwrap();
    let file_hash = MerkleHash::from(first.file_hash);
    reader
        .create_push_snapshot("push-first", std::slice::from_ref(&first.recipe))
        .unwrap();
    reader.commit_push_snapshot("push-first").unwrap();
    let retained = reader.retire_push_snapshot("push-first").await.unwrap();
    assert!(retained.is_empty());
    assert!(!reader.chunks_for_file(&file_hash).unwrap().is_empty());
    reader.remove_push_snapshot("push-first").unwrap();

    reader.mark_batch_published(&second.batch_id).unwrap();
    reader
        .create_push_snapshot("push-second", std::slice::from_ref(&second.recipe))
        .unwrap();
    reader.commit_push_snapshot("push-second").unwrap();
    let retired = reader.retire_push_snapshot("push-second").await.unwrap();
    assert_eq!(retired.len(), 1);
    assert!(retired[0].rows_deleted > 0);
    assert!(reader.chunks_for_file(&file_hash).unwrap().is_empty());
    reader.remove_push_snapshot("push-second").unwrap();
}

#[tokio::test]
async fn streaming_as_stages_source_outside_repo_root() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    let cache = dir.path().join("cache");
    std::fs::create_dir(&repo).unwrap();
    std::fs::create_dir(&cache).unwrap();
    let source = cache.join("overlay-backing.bin");
    write_pattern_file(&source, 2 * 1024 * 1024);
    let data = std::fs::read(&source).unwrap();
    let (expected_hash, expected_chunks) = direct_hash_and_chunks(&data);
    let staging = StagingArea::open(dir.path().join(".crab/staging"))
        .await
        .unwrap();

    let result = stage_file_streaming_as(
        &source,
        &repo,
        Path::new("models/model.bin"),
        &staging,
        StreamStageProgress::default(),
        &CancellationToken::new(),
    )
    .await
    .unwrap();

    assert_eq!(result.abs_path, source);
    assert_eq!(result.file_hash, expected_hash);
    assert_eq!(result.size, data.len() as u64);
    assert_eq!(result.chunks, expected_chunks.len());
    let staged = staging
        .chunks_for_file(&MerkleHash::from(result.file_hash))
        .unwrap();
    assert_eq!(staged, expected_chunks);
}

#[tokio::test]
async fn streaming_prepares_xorbs_from_verified_chunks() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("prepared.bin");
    write_pattern_file(&path, 2 * 1024 * 1024);
    let source = std::fs::read(&path).unwrap();
    let staging = StagingArea::open(dir.path().join(".crab/staging"))
        .await
        .unwrap();

    let result = stage_file_streaming(
        &path,
        dir.path(),
        &staging,
        StreamStageProgress {
            xorb_builder: Some(StreamStageXorbBuilder::new(1, XorbBuilder::new)),
            ..StreamStageProgress::default()
        },
        &CancellationToken::new(),
    )
    .await
    .unwrap();

    assert!(!result.prepared_xorbs.is_empty());
    let prepared_chunks: Vec<_> = result
        .prepared_xorbs
        .iter()
        .flat_map(|prepared| {
            prepared
                .placements
                .iter()
                .map(|placement| (placement.chunk_hash, u64::from(placement.uncompressed_size)))
        })
        .collect();
    let result_pairs = recipe_pairs(&staging, &result);
    let unique_staged_chunks: std::collections::HashSet<_> = result_pairs.iter().copied().collect();
    let unique_prepared_chunks: std::collections::HashSet<_> =
        prepared_chunks.iter().copied().collect();

    assert_eq!(unique_prepared_chunks, unique_staged_chunks);
    let file = staging
        .list_files()
        .unwrap()
        .into_iter()
        .find(|file| file.file_hash == result.file_hash)
        .unwrap();
    assert_eq!(file.committed_chunks + file.pending_chunks, 0);
    staging
        .mark_batch_published(&result.batch_id)
        .expect("publish staged recipe");
    let plan = staging
        .load_file_push_plan(&MerkleHash::from(result.file_hash))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(plan.chunk_count, result.recipe.chunk_count());
    assert_eq!(plan.sequence_hash().unwrap(), result.recipe.sequence_hash());
    let hashes = result_pairs
        .iter()
        .map(|(hash, _)| *hash)
        .collect::<Vec<_>>();
    let reconstructed = staging
        .get_chunks_batch(&hashes)
        .await
        .unwrap()
        .into_iter()
        .flat_map(|(_, bytes)| bytes)
        .collect::<Vec<_>>();
    assert_eq!(reconstructed, source);
    for prepared in &result.prepared_xorbs {
        assert!(!prepared.payload_path.exists());
        let payload = std::fs::read(crate::push_plan::prepared_xorb_path(
            staging.root(),
            &prepared.hash,
        ))
        .unwrap();
        assert_eq!(prepared.bytes, payload.len() as u64);
        assert_eq!(
            prepared.payload_hash,
            blake3::hash(&payload).to_hex().to_string()
        );
        assert!(
            prepared
                .placements
                .iter()
                .all(|placement| placement.xorb_hash == prepared.hash)
        );
    }
    let staging_root = staging.root().to_path_buf();
    staging.close().await.unwrap();

    let reopened = crate::StagingAreaReadOnly::open(staging_root)
        .await
        .unwrap();
    assert_eq!(
        reopened
            .chunks_for_file_with_sizes(&MerkleHash::from(result.file_hash))
            .unwrap(),
        result_pairs
    );
    let reconstructed = reopened
        .get_chunks_batch(&hashes)
        .await
        .unwrap()
        .into_iter()
        .flat_map(|(_, bytes)| bytes)
        .collect::<Vec<_>>();
    assert_eq!(reconstructed, source);
}

#[tokio::test]
async fn proven_remote_chunks_create_no_local_payload_copy() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("remote-only.bin");
    write_pattern_file(&path, 2 * 1024 * 1024);
    let staging = StagingArea::open(dir.path().join(".crab/staging"))
        .await
        .unwrap();

    let result = stage_file_streaming(
        &path,
        dir.path(),
        &staging,
        StreamStageProgress {
            existing_lookup: Some(Arc::new(BatchRemoteLookup { first_only: false })),
            ..StreamStageProgress::default()
        },
        &CancellationToken::new(),
    )
    .await
    .unwrap();

    assert!(result.prepared_xorbs.is_empty());
    let file = staging
        .list_files()
        .unwrap()
        .into_iter()
        .find(|file| file.file_hash == result.file_hash)
        .unwrap();
    assert_eq!(file.committed_chunks + file.pending_chunks, 0);
    staging.mark_batch_published(&result.batch_id).unwrap();
    let plan = staging
        .load_file_push_plan(&MerkleHash::from(result.file_hash))
        .await
        .unwrap()
        .unwrap();
    let unique_recipe_chunks = recipe_pairs(&staging, &result)
        .into_iter()
        .map(|(hash, _)| hash)
        .collect::<std::collections::HashSet<_>>();
    assert!(plan.existing.is_empty());
    let indexed_remote_chunks = recipe_remote_chunks(&staging, &result.recipe)
        .into_iter()
        .map(|(hash, _)| hash)
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(indexed_remote_chunks, unique_recipe_chunks);
    assert!(plan.prepared_xorbs.is_empty());
}

#[tokio::test]
async fn restaging_identical_recipe_with_remote_authority_reuses_canonical_recipe() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let source = repo.join("source.bin");
    write_pattern_file(&source, 2 * 1024 * 1024);
    let staging = StagingArea::open(dir.path().join(".crab/staging"))
        .await
        .unwrap();

    let first = stage_file_streaming_as(
        &source,
        &repo,
        Path::new("models/original.bin"),
        &staging,
        StreamStageProgress::default(),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    staging.mark_batch_published(&first.batch_id).unwrap();

    let renamed = stage_file_streaming_as(
        &source,
        &repo,
        Path::new("models/renamed.bin"),
        &staging,
        StreamStageProgress {
            existing_lookup: Some(Arc::new(BatchRemoteLookup { first_only: false })),
            ..StreamStageProgress::default()
        },
        &CancellationToken::new(),
    )
    .await
    .unwrap();

    assert_eq!(renamed.recipe, first.recipe);
    staging.mark_batch_published(&renamed.batch_id).unwrap();
    assert_eq!(
        staging
            .published_recipe_for_file(&MerkleHash::from(renamed.file_hash))
            .unwrap(),
        Some(first.recipe)
    );
}

#[tokio::test]
async fn mixed_remote_batch_packs_only_unknown_chunks() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("mixed.bin");
    write_pattern_file(&path, 4 * 1024 * 1024);
    let staging = StagingArea::open(dir.path().join(".crab/staging"))
        .await
        .unwrap();

    let result = stage_file_streaming(
        &path,
        dir.path(),
        &staging,
        StreamStageProgress {
            xorb_builder: Some(StreamStageXorbBuilder::new(1, XorbBuilder::new)),
            existing_lookup: Some(Arc::new(BatchRemoteLookup { first_only: true })),
            ..StreamStageProgress::default()
        },
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    staging.mark_batch_published(&result.batch_id).unwrap();
    let plan = staging
        .load_file_push_plan(&MerkleHash::from(result.file_hash))
        .await
        .unwrap()
        .unwrap();
    assert!(plan.existing.is_empty());
    let existing = recipe_remote_chunks(&staging, &result.recipe)
        .into_iter()
        .map(|(hash, _candidate)| hash)
        .collect::<std::collections::HashSet<_>>();
    let prepared = plan
        .prepared_xorbs
        .iter()
        .flat_map(|xorb| xorb.placements.iter())
        .map(|placement| MerkleHash::from_hex(&placement.chunk_hash).unwrap())
        .collect::<std::collections::HashSet<_>>();
    let recipe = recipe_pairs(&staging, &result)
        .into_iter()
        .map(|(hash, _)| hash)
        .collect::<std::collections::HashSet<_>>();

    assert!(!existing.is_empty());
    assert!(!prepared.is_empty());
    assert!(existing.is_disjoint(&prepared));
    assert_eq!(
        existing
            .union(&prepared)
            .copied()
            .collect::<std::collections::HashSet<_>>(),
        recipe
    );
    let file = staging
        .list_files()
        .unwrap()
        .into_iter()
        .find(|file| file.file_hash == result.file_hash)
        .unwrap();
    assert_eq!(file.committed_chunks + file.pending_chunks, 0);
}

#[tokio::test]
async fn malformed_remote_lookup_conservatively_packs_every_chunk() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("malformed-lookup.bin");
    write_pattern_file(&path, 2 * 1024 * 1024);
    let staging = StagingArea::open(dir.path().join(".crab/staging"))
        .await
        .unwrap();

    let result = stage_file_streaming(
        &path,
        dir.path(),
        &staging,
        StreamStageProgress {
            xorb_builder: Some(StreamStageXorbBuilder::new(1, XorbBuilder::new)),
            existing_lookup: Some(Arc::new(MalformedRemoteLookup)),
            ..StreamStageProgress::default()
        },
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    staging.mark_batch_published(&result.batch_id).unwrap();
    let plan = staging
        .load_file_push_plan(&MerkleHash::from(result.file_hash))
        .await
        .unwrap()
        .unwrap();

    assert!(plan.existing.is_empty());
    let prepared = plan
        .prepared_xorbs
        .iter()
        .flat_map(|xorb| xorb.placements.iter())
        .map(|placement| placement.chunk_hash.as_str())
        .collect::<std::collections::HashSet<_>>();
    let recipe = recipe_pairs(&staging, &result)
        .into_iter()
        .map(|(hash, _)| hash.hex())
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(prepared, recipe.iter().map(String::as_str).collect());
}

#[tokio::test(flavor = "multi_thread")]
async fn restaging_path_reclaims_prepared_payload_after_snapshot_release() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path().join("repo");
    std::fs::create_dir(&repo).unwrap();
    let first_source = dir.path().join("first.bin");
    let second_source = dir.path().join("second.bin");
    write_pattern_file(&first_source, 2 * 1024 * 1024);
    write_pattern_file(&second_source, 3 * 1024 * 1024);
    let staging_root = dir.path().join(".crab/staging");
    let logical_path = Path::new("models/model.bin");

    let staging = StagingArea::open(staging_root.clone()).await.unwrap();
    let first = stage_file_streaming_as(
        &first_source,
        &repo,
        logical_path,
        &staging,
        StreamStageProgress {
            xorb_builder: Some(StreamStageXorbBuilder::new(1, XorbBuilder::new)),
            ..StreamStageProgress::default()
        },
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    staging.mark_batch_published(&first.batch_id).unwrap();
    let first_hash = MerkleHash::from(first.file_hash);
    let prepared_paths = first
        .prepared_xorbs
        .iter()
        .map(|prepared| crate::push_plan::prepared_xorb_path(staging.root(), &prepared.hash))
        .collect::<Vec<_>>();
    assert!(prepared_paths.iter().all(|path| path.is_file()));
    staging.close().await.unwrap();

    let reader = crate::StagingAreaReadOnly::open(staging_root.clone())
        .await
        .unwrap();
    reader
        .create_push_snapshot("push-first", std::slice::from_ref(&first.recipe))
        .unwrap();
    drop(reader);

    let staging = StagingArea::open(staging_root.clone()).await.unwrap();
    let second = stage_file_streaming_as(
        &second_source,
        &repo,
        logical_path,
        &staging,
        StreamStageProgress::default(),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    staging.mark_batch_published(&second.batch_id).unwrap();
    assert!(prepared_paths.iter().all(|path| path.is_file()));
    assert!(
        staging
            .published_recipe_for_file(&first_hash)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        staging
            .published_recipe_for_file(&MerkleHash::from(second.file_hash))
            .unwrap(),
        Some(second.recipe.clone())
    );
    let second_prepared = staging
        .load_file_push_plan(&MerkleHash::from(second.file_hash))
        .await
        .unwrap()
        .unwrap()
        .prepared_xorbs
        .into_iter()
        .map(|xorb| xorb.hash().unwrap())
        .collect::<std::collections::HashSet<_>>();
    staging.close().await.unwrap();

    let reader = crate::StagingAreaReadOnly::open(staging_root)
        .await
        .unwrap();
    let retired = reader
        .discard_open_push_snapshot("push-first")
        .await
        .unwrap();
    assert_eq!(retired.len(), 1);
    for prepared in &first.prepared_xorbs {
        let path = crate::push_plan::prepared_xorb_path(reader.root(), &prepared.hash);
        assert_eq!(
            path.exists(),
            second_prepared.contains(&prepared.hash),
            "restage must retain only content-addressed payloads leased by the new recipe"
        );
    }
    let second_chunks = reader
        .chunks_for_file(&MerkleHash::from(second.file_hash))
        .unwrap();
    assert_eq!(second_chunks.len(), second.chunks);
}

#[tokio::test]
async fn prepared_xorb_writer_failure_publishes_no_authority() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("write-failure.bin");
    write_pattern_file(&path, multi_batch_fixture_bytes());
    let staging_root = dir.path().join(".crab/staging");
    let staging = StagingArea::open(staging_root.clone()).await.unwrap();
    let hooks = StreamStageHooks {
        before_prepared_xorb_write: Some(Arc::new(|_| {
            Err(CrabError::Internal(
                "injected prepared xorb failure".to_owned(),
            ))
        })),
        ..StreamStageHooks::default()
    };

    let error = stage_file_streaming_with_hooks(
        &path,
        dir.path(),
        &staging,
        StreamStageProgress {
            xorb_builder: Some(StreamStageXorbBuilder::new(1, XorbBuilder::new)),
            ..StreamStageProgress::default()
        },
        &CancellationToken::new(),
        hooks,
    )
    .await
    .unwrap_err();

    assert!(
        error.to_string().contains("injected prepared xorb failure"),
        "got {error:?}"
    );
    assert!(staging.list_files().unwrap().is_empty());
    assert!(
        std::fs::read_dir(staging_root.join("stream-prepared-xorbs"))
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(true)
    );
    assert!(!staging_root.join("push-plans/payloads").exists());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn prepared_xorb_promotion_is_serialized_per_preparation() {
    let dir = tempfile::tempdir().unwrap();
    let first_path = dir.path().join("first.bin");
    let second_path = dir.path().join("second.bin");
    write_pattern_file_with_salt(&first_path, 2 * 1024 * 1024, 17);
    write_pattern_file_with_salt(&second_path, 2 * 1024 * 1024, 41);
    let staging = StagingArea::open(dir.path().join(".crab/staging"))
        .await
        .unwrap();
    let preparation = staging.create_add_preparation().unwrap();
    let builders =
        StreamStageXorbBuilder::new(2, XorbBuilder::new).bind_preparation(preparation.clone());
    let active = Arc::new(AtomicU64::new(0));
    let peak = Arc::new(AtomicU64::new(0));
    let hook_active = Arc::clone(&active);
    let hook_peak = Arc::clone(&peak);
    let hooks = StreamStageHooks {
        before_prepared_xorb_promote: Some(Arc::new(move |_| {
            let concurrent = hook_active.fetch_add(1, Ordering::SeqCst) + 1;
            hook_peak.fetch_max(concurrent, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(200));
            hook_active.fetch_sub(1, Ordering::SeqCst);
            Ok(())
        })),
        ..StreamStageHooks::default()
    };
    let progress = StreamStageProgress {
        xorb_builder: Some(builders),
        ..StreamStageProgress::default()
    };
    let cancel = CancellationToken::new();
    let first = stage_file_streaming_with_hooks(
        &first_path,
        dir.path(),
        &staging,
        progress.clone(),
        &cancel,
        hooks.clone(),
    );
    let second = stage_file_streaming_with_hooks(
        &second_path,
        dir.path(),
        &staging,
        progress,
        &cancel,
        hooks,
    );
    let (first, second) = tokio::join!(first, second);

    first.unwrap();
    second.unwrap();
    assert_eq!(peak.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn progress_counts_streaming_and_chunking_in_one_pass() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("progress.bin");
    write_pattern_file(&path, 2 * 1024 * 1024);
    let data_len = std::fs::metadata(&path).unwrap().len();
    let staging = StagingArea::open(dir.path().join(".crab/staging"))
        .await
        .unwrap();
    let stream_bytes = Arc::new(AtomicU64::new(0));
    let chunk_bytes = Arc::new(AtomicU64::new(0));
    let chunks = Arc::new(AtomicU64::new(0));

    let result = stage_file_streaming(
        &path,
        dir.path(),
        &staging,
        StreamStageProgress {
            bytes_done: Some(Arc::clone(&stream_bytes)),
            chunk_bytes_done: Some(Arc::clone(&chunk_bytes)),
            chunks_done: Some(Arc::clone(&chunks)),
            xorb_builder: None,
            existing_lookup: None,
        },
        &CancellationToken::new(),
    )
    .await
    .unwrap();

    assert_eq!(stream_bytes.load(Ordering::Relaxed), data_len);
    assert_eq!(chunk_bytes.load(Ordering::Relaxed), data_len);
    assert_eq!(chunks.load(Ordering::Relaxed), result.chunks as u64);
}

#[tokio::test]
async fn multi_batch_file_stages_contiguous_offsets() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("large.bin");
    write_pattern_file(&path, multi_batch_fixture_bytes());
    let data = std::fs::read(&path).unwrap();
    let (_, expected_chunks) = direct_hash_and_chunks(&data);
    assert!(
        expected_chunks.len() > STAGE_BATCH_CHUNKS || data.len() as u64 > STAGE_BATCH_TARGET_BYTES,
        "fixture must span multiple staging batches"
    );
    let staging = StagingArea::open(dir.path().join(".crab/staging"))
        .await
        .unwrap();

    let result = stage_file_streaming(
        &path,
        dir.path(),
        &staging,
        StreamStageProgress::default(),
        &CancellationToken::new(),
    )
    .await
    .unwrap();

    let staged = staging
        .chunks_for_file(&MerkleHash::from(result.file_hash))
        .unwrap();
    assert_eq!(staged.len(), expected_chunks.len());
    assert_eq!(staged, expected_chunks);
    let staged_with_sizes = staging
        .chunks_for_file_with_sizes(&MerkleHash::from(result.file_hash))
        .unwrap();
    assert_eq!(recipe_pairs(&staging, &result), staged_with_sizes);
}

#[tokio::test]
async fn changed_file_during_stream_fails_and_retires_provisional_rows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("changing.bin");
    let fixture_bytes = multi_batch_fixture_bytes();
    write_pattern_file(&path, fixture_bytes);
    let original = std::fs::read(&path).unwrap();
    let original_hash = *blake3::hash(&original).as_bytes();
    let staging = StagingArea::open(dir.path().join(".crab/staging"))
        .await
        .unwrap();

    let path_for_hook = path.clone();
    let did_mutate = Arc::new(AtomicBool::new(false));
    let did_mutate_for_hook = Arc::clone(&did_mutate);
    let hooks = StreamStageHooks {
        after_batch_flush: Some(Arc::new(move || {
            if !did_mutate_for_hook.swap(true, Ordering::SeqCst) {
                write_pattern_file_with_salt(&path_for_hook, fixture_bytes, 7);
            }
        })),
        ..StreamStageHooks::default()
    };

    let err = stage_file_streaming_with_hooks(
        &path,
        dir.path(),
        &staging,
        StreamStageProgress::default(),
        &CancellationToken::new(),
        hooks,
    )
    .await
    .unwrap_err();

    assert!(did_mutate.load(Ordering::SeqCst));
    let mutated = std::fs::read(&path).unwrap();
    let mutated_hash = *blake3::hash(&mutated).as_bytes();
    match err {
        CrabError::FileChangedDuringStaging {
            path: err_path,
            first_hash,
            second_hash,
            first_size,
            second_size,
        } => {
            assert!(err_path.ends_with("changing.bin"));
            assert_ne!(first_hash, MerkleHash::from(original_hash).hex());
            assert_eq!(second_hash, MerkleHash::from(mutated_hash).hex());
            assert_eq!(first_size, original.len() as u64);
            assert_eq!(second_size, mutated.len() as u64);
        }
        other => panic!("expected FileChangedDuringStaging, got {other:?}"),
    }

    assert!(
        staging.list_files().unwrap().is_empty(),
        "provisional rows and file records must be retired after validation fails"
    );
    assert!(
        staging
            .chunks_for_file(&MerkleHash::from(mutated_hash))
            .unwrap()
            .is_empty(),
        "changed file must not be adopted after validation fails"
    );
}

#[tokio::test]
async fn writable_reopen_rolls_back_every_batch_in_unfinished_preparation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("unfinished.bin");
    write_pattern_file(&path, 2 * 1024 * 1024);
    let staging_root = dir.path().join(".crab/staging");
    let staging = StagingArea::open(staging_root.clone()).await.unwrap();
    let preparation = staging.create_add_preparation().unwrap();
    let result = stage_file_streaming(
        &path,
        dir.path(),
        &staging,
        StreamStageProgress {
            xorb_builder: Some(
                StreamStageXorbBuilder::new(1, XorbBuilder::new).bind_preparation(preparation),
            ),
            ..StreamStageProgress::default()
        },
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let payloads = result
        .prepared_xorbs
        .iter()
        .map(|prepared| crate::push_plan::prepared_xorb_path(&staging_root, &prepared.hash))
        .collect::<Vec<_>>();
    assert!(payloads.iter().all(|path| path.is_file()));
    staging.close().await.unwrap();

    let recovered = StagingArea::open(staging_root).await.unwrap();

    assert!(recovered.list_files().unwrap().is_empty());
    assert!(payloads.iter().all(|path| !path.exists()));
    assert_eq!(
        recovered
            .lifecycle_health()
            .unwrap()
            .open_batches_without_publication,
        0
    );
}

#[tokio::test]
async fn cancellation_after_batch_flush_retires_partial_rows() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cancel.bin");
    write_pattern_file(&path, multi_batch_fixture_bytes());
    let data = std::fs::read(&path).unwrap();
    let file_hash = *blake3::hash(&data).as_bytes();
    let (_, expected_chunks) = direct_hash_and_chunks(&data);
    assert!(
        expected_chunks.len() > STAGE_BATCH_CHUNKS || data.len() as u64 > STAGE_BATCH_TARGET_BYTES,
        "fixture must flush at least one full staging batch"
    );
    let staging = StagingArea::open(dir.path().join(".crab/staging"))
        .await
        .unwrap();
    let cancel = CancellationToken::new();
    let cancel_for_hook = cancel.clone();
    let did_cancel = Arc::new(AtomicBool::new(false));
    let did_cancel_for_hook = Arc::clone(&did_cancel);
    let hooks = StreamStageHooks {
        after_batch_flush: Some(Arc::new(move || {
            if !did_cancel_for_hook.swap(true, Ordering::SeqCst) {
                cancel_for_hook.cancel();
            }
        })),
        ..StreamStageHooks::default()
    };

    let err = stage_file_streaming_with_hooks(
        &path,
        dir.path(),
        &staging,
        StreamStageProgress::default(),
        &cancel,
        hooks,
    )
    .await
    .unwrap_err();

    assert!(matches!(err, CrabError::Cancelled), "got {err:?}");
    assert!(did_cancel.load(Ordering::SeqCst));
    assert!(
        staging
            .chunks_for_file(&MerkleHash::from(file_hash))
            .unwrap()
            .is_empty(),
        "partial rows staged before cancellation must be retired"
    );
}

#[tokio::test]
async fn cancellation_discards_partial_direct_xorb_authority() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("cancel-direct.bin");
    write_pattern_file(&path, multi_batch_fixture_bytes());
    let staging_root = dir.path().join(".crab/staging");
    let staging = StagingArea::open(staging_root.clone()).await.unwrap();
    let cancel = CancellationToken::new();
    let cancel_for_hook = cancel.clone();
    let hooks = StreamStageHooks {
        after_batch_flush: Some(Arc::new(move || cancel_for_hook.cancel())),
        ..StreamStageHooks::default()
    };

    let error = stage_file_streaming_with_hooks(
        &path,
        dir.path(),
        &staging,
        StreamStageProgress {
            xorb_builder: Some(StreamStageXorbBuilder::new(1, XorbBuilder::new)),
            ..StreamStageProgress::default()
        },
        &cancel,
        hooks,
    )
    .await
    .unwrap_err();

    assert!(matches!(error, CrabError::Cancelled), "got {error:?}");
    assert!(staging.list_files().unwrap().is_empty());
    assert!(
        std::fs::read_dir(staging_root.join("stream-prepared-xorbs"))
            .map(|mut entries| entries.next().is_none())
            .unwrap_or(true)
    );
    assert!(!staging_root.join("push-plans/payloads").exists());
}

#[tokio::test]
#[ignore = "manual: stages 128 MiB and measures RSS; run with --ignored"]
async fn streaming_stage_bounded_rss_on_128mib_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rss.bin");
    write_pattern_file(&path, 128 * 1024 * 1024);
    let baseline = memory_stats::memory_stats()
        .map(|s| s.physical_mem)
        .unwrap_or(0);
    let staging = StagingArea::open(dir.path().join(".crab/staging"))
        .await
        .unwrap();

    let result = stage_file_streaming(
        &path,
        dir.path(),
        &staging,
        StreamStageProgress::default(),
        &CancellationToken::new(),
    )
    .await
    .unwrap();

    let peak = memory_stats::memory_stats()
        .map(|s| s.physical_mem)
        .unwrap_or(0);
    let delta = peak.saturating_sub(baseline);
    assert_eq!(result.size, 128 * 1024 * 1024);
    if baseline > 0 {
        assert!(
            delta < 96 * 1024 * 1024,
            "RSS delta {delta} exceeded bounded streaming budget"
        );
    }
}

#[tokio::test]
async fn tagged_layout_upgrade_preserves_staged_only_content_and_ownership() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("only.bin");
    write_pattern_file(&path, 1024 * 1024 + 17);
    let bytes = std::fs::read(&path).unwrap();
    let root = dir.path().join("staging");
    let staging = StagingArea::open(root.clone()).await.unwrap();
    let staged = stage_file_streaming(
        &path,
        dir.path(),
        &staging,
        StreamStageProgress {
            xorb_builder: Some(StreamStageXorbBuilder::new(1, XorbBuilder::new)),
            ..Default::default()
        },
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    staging.mark_batch_published(&staged.batch_id).unwrap();
    staging.close().await.unwrap();
    std::fs::remove_file(path).unwrap();
    // The tagged v1 contract has the same tables and payload formats, without
    // the two ownership indexes and with the v1 writer-admission marker.
    let database = rusqlite::Connection::open(root.join("index.db")).unwrap();
    database
        .execute_batch(
            "DROP INDEX chunk_payloads_by_segment;
        DROP INDEX recording_terms_by_chunk;
        UPDATE staging_meta SET value = '1' WHERE key = 'layout_version';",
        )
        .unwrap();
    drop(database);
    assert!(StagingArea::open(root.clone()).await.is_err());
    StagingArea::upgrade_layout(root.clone()).await.unwrap();
    StagingArea::upgrade_layout(root.clone()).await.unwrap();
    let staging = StagingArea::open(root).await.unwrap();
    let hashes = staging
        .chunks_for_file(&MerkleHash::from(staged.file_hash))
        .unwrap();
    let reconstructed = staging
        .get_chunks_batch(&hashes)
        .await
        .unwrap()
        .into_iter()
        .flat_map(|(_, data)| data.to_vec())
        .collect::<Vec<_>>();
    assert_eq!(reconstructed, bytes);
    staging.close().await.unwrap();
}

#[tokio::test]
async fn layout_upgrade_refuses_active_owner_and_unknown_schema() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("staging");
    let staging = StagingArea::open(root.clone()).await.unwrap();
    assert!(matches!(
        StagingArea::upgrade_layout(root.clone()).await,
        Err(CrabError::StagingLocked { .. })
    ));
    staging.close().await.unwrap();
    let database = rusqlite::Connection::open(root.join("index.db")).unwrap();
    database
        .execute_batch(
            "CREATE TABLE unexpected (value TEXT);
        UPDATE staging_meta SET value = '1' WHERE key = 'layout_version';",
        )
        .unwrap();
    assert!(StagingArea::upgrade_layout(root).await.is_err());
    let version: String = database
        .query_row(
            "SELECT value FROM staging_meta WHERE key = 'layout_version'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(version, "1");
}

#[tokio::test]
async fn published_prepared_candidate_verifies_without_cdc() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("published.bin");
    write_pattern_file(&path, 1024 * 1024 + 17);
    let staging = StagingArea::open(dir.path().join("staging")).await.unwrap();
    let progress = StreamStageProgress {
        xorb_builder: Some(StreamStageXorbBuilder::new(1, XorbBuilder::new)),
        ..Default::default()
    };
    let cancel = CancellationToken::new();
    let original = stage_file_streaming(&path, dir.path(), &staging, progress.clone(), &cancel)
        .await
        .unwrap();
    staging.mark_batch_published(&original.batch_id).unwrap();
    assert!(
        staging
            .unpublished_local_recipe_for_path(Path::new("published.bin"))
            .unwrap()
            .is_none()
    );
    let candidate = staging
        .local_recipe_for_path(Path::new("published.bin"))
        .unwrap();
    let chunks_done = Arc::new(AtomicU64::new(0));
    let mut input = IngestSession::begin(
        &staging,
        Path::new("published.bin"),
        StreamStageProgress {
            chunks_done: Some(Arc::clone(&chunks_done)),
            ..progress
        },
        &cancel,
        candidate,
    )
    .await
    .unwrap();
    for block in std::fs::read(path).unwrap().chunks(31_337) {
        input.feed(Bytes::copy_from_slice(block)).await.unwrap();
    }
    let result = input.finish_input().await.unwrap().seal().await.unwrap();
    assert_eq!(chunks_done.load(Relaxed), 0);
    assert_eq!(result.recipe, original.recipe);
}
