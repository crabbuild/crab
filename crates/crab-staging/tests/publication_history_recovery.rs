//! Committed history ownership must outlive rollback of its native index intent.

#![expect(clippy::unwrap_used, reason = "test assertions")]

use std::path::Path;
use std::sync::Arc;

use crab_staging::stream::{StreamStageProgress, StreamStageXorbBuilder, stage_reader_streaming};
use crab_staging::{
    PublicationIntentEntry, StagingArea, StagingAreaReadOnly, StagingError, StagingPushSnapshot,
};
use crab_xet::hash::MerkleHash;
use tokio_util::sync::CancellationToken;

#[tokio::test(flavor = "multi_thread")]
async fn committed_history_preserves_only_selected_intent_recipes_through_rollback() {
    let temp = tempfile::tempdir().unwrap();
    let staging = StagingArea::open(temp.path().to_owned()).await.unwrap();
    let mut staged = Vec::new();
    for (path, byte) in [("committed", 0x71), ("uncommitted", 0x72)] {
        staged.push(
            stage_reader_streaming(
                &mut std::io::Cursor::new(vec![byte; 2 * 1024 * 1024]),
                Path::new(path),
                &staging,
                StreamStageProgress {
                    xorb_builder: Some(StreamStageXorbBuilder::new(
                        1,
                        crab_xet::xorb::builder::XorbBuilder::new,
                    )),
                    ..Default::default()
                },
                &CancellationToken::new(),
            )
            .await
            .unwrap(),
        );
    }
    let entries = staged
        .iter()
        .enumerate()
        .map(|(index, staged)| PublicationIntentEntry {
            batch_id: staged.batch_id.clone(),
            path: if index == 0 {
                "committed"
            } else {
                "uncommitted"
            }
            .into(),
            expected_pointer_oid: format!("{}", index + 1).repeat(40),
            previous_index_state: "0".repeat(64),
        })
        .collect::<Vec<_>>();
    let intent = staging.create_publication_intent(&entries).unwrap();
    let recipe_hash = MerkleHash::from(staged[0].recipe.hash());
    let invalid = staging.preserve_publication_history(
        &intent,
        &[recipe_hash, MerkleHash::from([0xff; 32])],
        &CancellationToken::new(),
    );
    assert!(invalid.is_err() && staging.lifecycle_health().unwrap().path_heads == 0);
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(matches!(
        staging.preserve_publication_history(&intent, &[recipe_hash], &cancelled),
        Err(StagingError::Cancelled)
    ));
    for _ in 0..2 {
        staging
            .preserve_publication_history(&intent, &[recipe_hash], &CancellationToken::new())
            .unwrap();
    }
    let health = staging.lifecycle_health().unwrap();
    assert!(
        health.path_heads == 1
            && health.path_leases == 3
            && health.payloads == 0
            && health.current_head_prepared_bytes > 0,
        "{health:?}"
    );
    staging.rollback_publication_intent(&intent).unwrap();
    assert!(
        staging
            .published_recipe_for_file(&staged[0].recipe.file_hash())
            .unwrap()
            == Some(staged[0].recipe.clone())
            && staging
                .chunks_for_file(&staged[0].recipe.file_hash())
                .unwrap()
                .len()
                == 16
            && staging
                .chunks_for_file(&staged[1].recipe.file_hash())
                .unwrap()
                .is_empty()
    );
    staging.close().await.unwrap();
    let reader = Arc::new(
        StagingAreaReadOnly::open(temp.path().to_owned())
            .await
            .unwrap(),
    );
    reader
        .verify_file_reconstruction(&staged[0].recipe.file_hash(), staged[0].size)
        .await
        .unwrap();
    let prepared = reader.prepared_xorbs_for_recipes(&[recipe_hash]).unwrap();
    let bodies = prepared
        .values()
        .flatten()
        .map(|xorb| {
            crab_staging::push_plan::prepared_xorb_path(
                temp.path(),
                &MerkleHash::from_hex(&xorb.hash).unwrap(),
            )
        })
        .collect::<Vec<_>>();
    assert!(!bodies.is_empty() && bodies.iter().all(|path| path.is_file()));
    let snapshot =
        StagingPushSnapshot::begin(Arc::clone(&reader), "history-push".to_owned()).unwrap();
    snapshot
        .capture(std::slice::from_ref(&staged[0].recipe))
        .unwrap();
    snapshot.commit().unwrap();
    let health = reader.lifecycle_health().unwrap();
    assert!(health.path_heads == 0 && health.path_leases == 0 && health.payloads == 0);
    assert!(bodies.iter().all(|path| !path.try_exists().unwrap()));
}
