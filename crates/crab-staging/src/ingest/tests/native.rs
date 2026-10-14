use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn native_descriptor_reuse_uses_repository_path_and_rewinds_input() {
    use crate::stream::stage_file_streaming_from_descriptor_as;
    use std::io::{Seek, SeekFrom};

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("temporary-content");
    let repo_path = Path::new("models/content.bin");
    write_pattern_file(&path, READ_BUF_SIZE + 17);
    let staging = StagingArea::open(dir.path().join("staging")).await.unwrap();
    let cancel = CancellationToken::new();
    let progress = StreamStageProgress {
        xorb_builder: Some(StreamStageXorbBuilder::new(1, XorbBuilder::new)),
        ..Default::default()
    };
    let original = stage_file_streaming_as(
        &path,
        dir.path(),
        repo_path,
        &staging,
        progress.clone(),
        &cancel,
    )
    .await
    .unwrap();
    let mut descriptor = std::fs::File::open(&path).unwrap();
    descriptor.seek(SeekFrom::End(0)).unwrap();
    let chunks_done = Arc::new(AtomicU64::new(0));
    let result = stage_file_streaming_from_descriptor_as(
        descriptor,
        &path,
        dir.path(),
        repo_path,
        &staging,
        StreamStageProgress {
            chunks_done: Some(Arc::clone(&chunks_done)),
            ..progress
        },
        &cancel,
    )
    .await
    .unwrap();
    assert_eq!(result.recipe, original.recipe);
    assert_eq!(chunks_done.load(Relaxed), 0);
    assert!(result.prepared_xorbs.is_empty());
    staging.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn native_restaging_verifies_retained_recipes_without_materialization() {
    for authority in ["segment", "prepared", "published"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("model.bin");
        write_pattern_file(&path, 4 * READ_BUF_SIZE + 17);
        let original_bytes = std::fs::read(&path).unwrap();
        let staging = StagingArea::open(dir.path().join("staging")).await.unwrap();
        let progress = StreamStageProgress {
            xorb_builder: (authority != "segment")
                .then(|| StreamStageXorbBuilder::new(1, XorbBuilder::new)),
            ..Default::default()
        };
        let cancel = CancellationToken::new();
        let original = stage_file_streaming(&path, dir.path(), &staging, progress.clone(), &cancel)
            .await
            .unwrap();
        if authority == "published" {
            staging.mark_batch_published(&original.batch_id).unwrap();
        }
        let before = tokio::task::block_in_place(|| staging.stats())
            .unwrap()
            .current_segment_bytes;
        crate::lock_index(&staging.index)
            .unwrap()
            .connection()
            .execute_batch(
                "CREATE TEMP TRIGGER forbid_recipe_rebuild BEFORE INSERT ON recipe_occurrences
             BEGIN SELECT RAISE(ABORT, 'exact restaging rebuilt recipe occurrences'); END;",
            )
            .unwrap();
        let stat = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(stat + Duration::from_secs(2))
            .unwrap();
        let chunks_done = Arc::new(AtomicU64::new(0));
        let bytes_done = Arc::new(AtomicU64::new(0));
        let forbidden = StreamStageHooks {
            after_batch_flush: Some(Arc::new(|| panic!("exact input classified authority"))),
            before_prepared_xorb_write: Some(Arc::new(|_| panic!("exact input wrote payload"))),
            ..Default::default()
        };
        let repeated = stage_file_streaming_with_hooks(
            &path,
            dir.path(),
            &staging,
            StreamStageProgress {
                chunks_done: Some(Arc::clone(&chunks_done)),
                bytes_done: Some(Arc::clone(&bytes_done)),
                ..progress
            },
            &cancel,
            forbidden,
        )
        .await
        .unwrap();
        assert_eq!(chunks_done.load(Relaxed), 0, "{authority}");
        assert_eq!(bytes_done.load(Relaxed), original_bytes.len() as u64);
        assert_eq!(repeated.recipe, original.recipe);
        assert_eq!(repeated.timings.chunking_duration_ms, 0);
        assert_eq!(
            tokio::task::block_in_place(|| staging.stats())
                .unwrap()
                .current_segment_bytes,
            before
        );
        assert!(repeated.prepared_xorbs.is_empty());
        if authority != "published" {
            staging.rollback_batch(&original.batch_id).unwrap();
        }
        staging.mark_batch_published(&repeated.batch_id).unwrap();
        staging.close().await.unwrap();
        let reopened = StagingArea::open(dir.path().join("staging")).await.unwrap();
        let hashes = recipe_pairs(&reopened, &repeated)
            .into_iter()
            .map(|(hash, _)| hash)
            .collect::<Vec<_>>();
        let reconstructed = reopened
            .get_chunks_batch(&hashes)
            .await
            .unwrap()
            .into_iter()
            .flat_map(|(_, bytes)| bytes.to_vec())
            .collect::<Vec<_>>();
        assert_eq!(reconstructed, original_bytes, "{authority}");
        reopened.close().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn native_changed_recipe_preserves_cdc_and_reuses_verified_prefix() {
    for change in ["beginning", "tail", "truncate", "extend"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("model.bin");
        write_pattern_file(&path, 4 * READ_BUF_SIZE + 17);
        let mut incoming = std::fs::read(&path).unwrap();
        let reads = Arc::new(PreparedReadCount::default());
        let mut staging = StagingArea::open(dir.path().join("staging")).await.unwrap();
        staging.set_metrics(Arc::clone(&reads));
        let progress = StreamStageProgress {
            xorb_builder: Some(StreamStageXorbBuilder::new(1, XorbBuilder::new)),
            ..Default::default()
        };
        let cancel = CancellationToken::new();
        let original = stage_file_streaming(&path, dir.path(), &staging, progress.clone(), &cancel)
            .await
            .unwrap();
        staging.mark_batch_published(&original.batch_id).unwrap();
        match change {
            "beginning" => incoming[0] ^= 0x3f,
            "tail" => *incoming.last_mut().unwrap() ^= 0x3f,
            "truncate" => incoming.truncate(incoming.len() - 8193),
            "extend" => incoming.extend_from_slice(b"appended\0content\n"),
            _ => unreachable!(),
        }
        std::fs::write(&path, &incoming).unwrap();
        let chunks_done = Arc::new(AtomicU64::new(0));
        let staged = stage_file_streaming(
            &path,
            dir.path(),
            &staging,
            StreamStageProgress {
                chunks_done: Some(Arc::clone(&chunks_done)),
                ..progress
            },
            &cancel,
        )
        .await
        .unwrap();
        let (hash, expected_chunks) = direct_hash_and_chunks(&incoming);
        let chunks = recipe_pairs(&staging, &staged)
            .into_iter()
            .map(|(hash, _)| hash)
            .collect::<Vec<_>>();
        assert_eq!(staged.file_hash, hash, "{change}");
        assert_eq!(chunks, expected_chunks, "{change}");
        assert!(reads.0.load(Relaxed) <= 1, "{change} reread the prefix");
        if change == "tail" {
            assert!(chunks_done.load(Relaxed) < staged.chunks as u64 / 2);
        }
        let reconstructed = staging
            .get_chunks_batch(&chunks)
            .await
            .unwrap()
            .into_iter()
            .flat_map(|(_, bytes)| bytes.to_vec())
            .collect::<Vec<_>>();
        assert_eq!(reconstructed, incoming, "{change}");
        staging.close().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn native_verification_failure_preserves_original_owner() {
    for failure in ["mutation", "replacement", "cancellation"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("model.bin");
        write_pattern_file(&path, 2 * READ_BUF_SIZE + 17);
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
        let changed = Arc::new(AtomicBool::new(false));
        let hook_path = path.clone();
        let hook_cancel = cancel.clone();
        let hooks = StreamStageHooks {
            after_input_read: Some(Arc::new(move || {
                if changed.swap(true, Ordering::SeqCst) {
                    return;
                }
                match failure {
                    "mutation" => {
                        // Mutate already-consumed bytes: verification can match
                        // the old recipe, but source validation must still reject.
                        let mut file = std::fs::OpenOptions::new()
                            .write(true)
                            .open(&hook_path)
                            .unwrap();
                        file.write_all(b"changed").unwrap();
                        file.set_modified(std::time::SystemTime::now() + Duration::from_secs(2))
                            .unwrap();
                    }
                    "replacement" => {
                        std::fs::rename(&hook_path, hook_path.with_extension("old")).unwrap();
                        std::fs::write(&hook_path, b"replacement").unwrap();
                    }
                    "cancellation" => hook_cancel.cancel(),
                    _ => unreachable!(),
                }
            })),
            ..Default::default()
        };
        let error =
            stage_file_streaming_with_hooks(&path, dir.path(), &staging, progress, &cancel, hooks)
                .await
                .unwrap_err();
        match failure {
            "cancellation" => assert!(matches!(error, CrabError::Cancelled)),
            _ => assert!(matches!(error, CrabError::FileChangedDuringStaging { .. })),
        }
        assert_eq!(
            staging
                .lifecycle_health()
                .unwrap()
                .open_batches_without_publication,
            0
        );
        assert_eq!(
            staging
                .local_recipe_for_path(Path::new("model.bin"))
                .unwrap(),
            Some(original.recipe)
        );
        staging.close().await.unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn native_verification_survives_concurrent_original_owner_rollback() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("model.bin");
    write_pattern_file(&path, 2 * READ_BUF_SIZE + 17);
    let original_bytes = std::fs::read(&path).unwrap();
    let staging = Arc::new(StagingArea::open(dir.path().join("staging")).await.unwrap());
    let progress = StreamStageProgress {
        xorb_builder: Some(StreamStageXorbBuilder::new(1, XorbBuilder::new)),
        ..Default::default()
    };
    let cancel = CancellationToken::new();
    let original = stage_file_streaming(&path, dir.path(), &staging, progress.clone(), &cancel)
        .await
        .unwrap();
    let (start, started) = std::sync::mpsc::sync_channel(0);
    let (finish, finished) = std::sync::mpsc::sync_channel(0);
    let finished = std::sync::Mutex::new(finished);
    let retire_staging = Arc::clone(&staging);
    let original_batch = original.batch_id.clone();
    let retire = tokio::task::spawn_blocking(move || {
        started.recv().unwrap();
        let result = retire_staging.rollback_batch(&original_batch);
        finish.send(()).unwrap();
        result
    });
    let once = AtomicBool::new(false);
    let hooks = StreamStageHooks {
        after_input_read: Some(Arc::new(move || {
            if !once.swap(true, Ordering::SeqCst) {
                // Dropping the hook on an early staging error disconnects the
                // worker instead of leaving runtime shutdown waiting on a barrier.
                start.send(()).unwrap();
                finished.lock().unwrap().recv().unwrap();
            }
        })),
        ..Default::default()
    };
    let result =
        stage_file_streaming_with_hooks(&path, dir.path(), &staging, progress, &cancel, hooks)
            .await
            .unwrap();
    retire.await.unwrap().unwrap();
    assert_eq!(result.recipe, original.recipe);
    assert_eq!(result.timings.chunking_duration_ms, 0);
    let hashes = recipe_pairs(&staging, &result)
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
    assert_eq!(reconstructed, original_bytes);
    Arc::try_unwrap(staging)
        .ok()
        .unwrap()
        .close()
        .await
        .unwrap();
}
