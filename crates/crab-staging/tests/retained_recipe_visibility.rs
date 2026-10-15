//! Public occurrence enumeration must not depend on Git publication state.

use crab_staging::stream::{StreamStageProgress, StreamStageXorbBuilder, stage_reader_streaming};
use crab_staging::{StagingArea, StagingAreaReadOnly};
use crab_xet::chunker::GearChunker;
use crab_xet::hash::MerkleHash;
use std::path::Path;
use tokio_util::sync::CancellationToken;

#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::unwrap_used, reason = "test assertions")]
async fn prepared_occurrences_are_visible_to_both_handles_before_publication() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("staging");
    let bytes = vec![0x71; 2 * 1024 * 1024];
    let mut chunker = GearChunker::new();
    let mut chunks = chunker.feed(&bytes);
    chunks.extend(chunker.finalize());
    let expected = chunks
        .into_iter()
        .map(|chunk| {
            (
                MerkleHash::from(<[u8; 32]>::from(chunk.hash)),
                chunk.data.len() as u64,
            )
        })
        .collect::<Vec<_>>();
    assert!(expected.len() > 1 && expected.windows(2).any(|pair| pair[0] == pair[1]));
    let staging = StagingArea::open(root.clone()).await.unwrap();
    let staged = stage_reader_streaming(
        &mut std::io::Cursor::new(&bytes),
        Path::new("model.bin"),
        &staging,
        StreamStageProgress {
            xorb_builder: Some(StreamStageXorbBuilder::new(
                1,
                crab_xet::xorb::builder::XorbBuilder::new,
            )),
            ..StreamStageProgress::default()
        },
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let hash = staged.recipe.file_hash();
    let hashes = expected.iter().map(|(hash, _)| *hash).collect::<Vec<_>>();
    assert!(staging.published_recipe_for_file(&hash).unwrap().is_none());
    assert_eq!(staging.chunks_for_file(&hash).unwrap(), hashes);
    assert_eq!(staging.chunks_for_file_with_sizes(&hash).unwrap(), expected);
    staging.close().await.unwrap();
    {
        let shared = StagingAreaReadOnly::open(root.clone()).await.unwrap();
        assert!(shared.published_recipe_for_file(&hash).unwrap().is_none());
        assert_eq!(shared.chunks_for_file(&hash).unwrap(), hashes);
        assert_eq!(shared.chunks_for_file_with_sizes(&hash).unwrap(), expected);
    }
    let staging = StagingArea::open(root.clone()).await.unwrap();
    staging
        .delegate_publication(&[(&staged.batch_id, Path::new("model.bin"))])
        .unwrap();
    assert!(staging.published_recipe_for_file(&hash).unwrap().is_none());
    assert_eq!(staging.chunks_for_file(&hash).unwrap(), hashes);
    assert_eq!(staging.chunks_for_file_with_sizes(&hash).unwrap(), expected);
    staging.close().await.unwrap();
    let shared = StagingAreaReadOnly::open(root.clone()).await.unwrap();
    assert!(shared.published_recipe_for_file(&hash).unwrap().is_none());
    assert_eq!(shared.chunks_for_file(&hash).unwrap(), hashes);
    assert_eq!(shared.chunks_for_file_with_sizes(&hash).unwrap(), expected);
    shared
        .create_push_snapshot("captured-pointer", std::slice::from_ref(&staged.recipe))
        .unwrap();
    drop(shared);
    let staging = StagingArea::open(root.clone()).await.unwrap();
    staging.rollback_batch(&staged.batch_id).unwrap();
    assert!(staging.published_recipe_for_file(&hash).unwrap().is_none());
    assert_eq!(staging.chunks_for_file(&hash).unwrap(), hashes);
    assert_eq!(staging.chunks_for_file_with_sizes(&hash).unwrap(), expected);
    staging.close().await.unwrap();
    let shared = StagingAreaReadOnly::open(root).await.unwrap();
    assert_eq!(shared.chunks_for_file(&hash).unwrap(), hashes);
    assert_eq!(shared.chunks_for_file_with_sizes(&hash).unwrap(), expected);
    shared
        .discard_open_push_snapshot("captured-pointer")
        .await
        .unwrap();
    assert!(
        shared.chunks_for_file(&hash).unwrap().is_empty()
            && shared.chunks_for_file_with_sizes(&hash).unwrap().is_empty()
    );
}
