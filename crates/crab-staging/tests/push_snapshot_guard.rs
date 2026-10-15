//! Reader fences and exact lease cleanup across push completion and cancellation.

#![expect(clippy::unwrap_used, reason = "test assertions")]

use std::path::Path;
use std::sync::Arc;

use crab_staging::stream::{StreamStageProgress, stage_reader_streaming};
use crab_staging::{StagingArea, StagingAreaReadOnly, StagingPushSnapshot};
use tokio_util::sync::CancellationToken;

async fn fixture(
    root: &Path,
    native_owner: bool,
) -> (
    Arc<StagingAreaReadOnly>,
    crab_staging::stream::StreamStageResult,
    Option<crab_staging::StagingBatchId>,
) {
    let staging = StagingArea::open(root.to_owned()).await.unwrap();
    let staged = stage_reader_streaming(
        &mut std::io::Cursor::new(b"shared fixture payload"),
        Path::new("model.bin"),
        &staging,
        StreamStageProgress::default(),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    staging
        .delegate_publication(&[(&staged.batch_id, Path::new("model.bin"))])
        .unwrap();
    let native = if native_owner {
        let batch = staging.create_batch().unwrap();
        staging
            .record_verified_recipe_lease(&batch, Path::new("native.bin"), &staged.recipe)
            .unwrap();
        Some(batch)
    } else {
        None
    };
    staging.close().await.unwrap();
    (
        Arc::new(StagingAreaReadOnly::open(root.to_owned()).await.unwrap()),
        staged,
        native,
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn dropped_snapshot_keeps_delegated_payload_and_clears_its_reader() {
    let temp = tempfile::tempdir().unwrap();
    let (staging, staged, _) = fixture(temp.path(), false).await;
    let snapshot = StagingPushSnapshot::begin(Arc::clone(&staging), "failed".to_owned()).unwrap();
    snapshot
        .capture(std::slice::from_ref(&staged.recipe))
        .unwrap();
    drop(snapshot);
    let health = staging.lifecycle_health().unwrap();
    assert!(
        health.open_push_snapshots == 0
            && health.path_leases == 1
            && health.payloads == 1
            && staging.list_inflight().unwrap().is_empty()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn committed_snapshot_preserves_active_native_owner_of_shared_payload() {
    let temp = tempfile::tempdir().unwrap();
    let (staging, staged, native) = fixture(temp.path(), true).await;
    let snapshot =
        StagingPushSnapshot::begin(Arc::clone(&staging), "committed".to_owned()).unwrap();
    snapshot
        .capture(std::slice::from_ref(&staged.recipe))
        .unwrap();
    snapshot.commit().unwrap();
    let health = staging.lifecycle_health().unwrap();
    assert!(
        health.path_leases == 1
            && health.payloads == 1
            && health.open_push_snapshots == 0
            && health.committed_push_snapshots == 0
            && staging.list_inflight().unwrap().is_empty()
    );
    drop(staging);
    let writer = StagingArea::open(temp.path().to_owned()).await.unwrap();
    writer.rollback_batch(&native.unwrap()).unwrap();
    assert!(
        writer
            .chunks_for_file(&staged.recipe.file_hash())
            .unwrap()
            .is_empty()
    );
    writer.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn sibling_reader_defers_retirement_until_exclusive_maintenance() {
    let temp = tempfile::tempdir().unwrap();
    let (staging, staged, _) = fixture(temp.path(), false).await;
    let first = StagingPushSnapshot::begin(Arc::clone(&staging), "first".to_owned()).unwrap();
    first.capture(std::slice::from_ref(&staged.recipe)).unwrap();
    let second = StagingPushSnapshot::begin(Arc::clone(&staging), "second".to_owned()).unwrap();
    second
        .capture(std::slice::from_ref(&staged.recipe))
        .unwrap();
    first.commit().unwrap();
    let health = staging.lifecycle_health().unwrap();
    assert!(
        health.committed_push_snapshots == 1
            && health.open_push_snapshots == 1
            && health.payloads == 1
    );
    drop(second);
    drop(staging);
    let writer = StagingArea::open(temp.path().to_owned()).await.unwrap();
    writer.clean().unwrap();
    let health = writer.lifecycle_health().unwrap();
    assert!(
        health.path_leases == 0
            && health.payloads == 0
            && health.open_push_snapshots == 0
            && health.committed_push_snapshots == 0
            && writer.list_inflight().unwrap().is_empty()
    );
    writer.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn task_abort_drops_snapshot_without_retiring_unpublished_lease() {
    let temp = tempfile::tempdir().unwrap();
    let (staging, staged, _) = fixture(temp.path(), false).await;
    let reader = Arc::clone(&staging);
    let (ready, received) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let snapshot = StagingPushSnapshot::begin(reader, "aborted".to_owned()).unwrap();
        snapshot
            .capture(std::slice::from_ref(&staged.recipe))
            .unwrap();
        ready.send(()).unwrap();
        std::future::pending::<()>().await;
        drop(snapshot);
    });
    received.await.unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let health = staging.lifecycle_health().unwrap();
    assert!(
        health.path_leases == 1
            && health.open_push_snapshots == 0
            && health.payloads == 1
            && staging.list_inflight().unwrap().is_empty()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn local_commit_failure_releases_marker_and_preserves_original_lease() {
    let temp = tempfile::tempdir().unwrap();
    let (staging, staged, _) = fixture(temp.path(), false).await;
    let snapshot =
        StagingPushSnapshot::begin(Arc::clone(&staging), "missing-journal".to_owned()).unwrap();
    snapshot
        .capture(std::slice::from_ref(&staged.recipe))
        .unwrap();
    staging.remove_push_snapshot("missing-journal").unwrap();
    assert!(snapshot.commit().is_err());
    let health = staging.lifecycle_health().unwrap();
    assert!(
        health.path_leases == 1
            && health.payloads == 1
            && staging.list_inflight().unwrap().is_empty()
    );
}
