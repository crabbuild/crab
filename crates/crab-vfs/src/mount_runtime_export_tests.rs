use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn export_uses_the_captured_base_instead_of_reopening_the_mounted_path() {
    let root = tempfile::tempdir().unwrap();
    let source = source_repo(root.path());
    std::fs::create_dir_all(source.join("archive")).unwrap();
    std::fs::write(source.join("archive/child.txt"), b"captured child").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("../base.txt", source.join("archive/link")).unwrap();
    git(&source, ["add", "."]);
    git(&source, ["commit", "-m", "base directory"]);
    let config = PipelineConfig {
        source: source.display().to_string(),
        git_dir: source.join(".git"),
        git_metadata: GitMetadataOwnership::Borrowed,
        ref_name: Some("refs/heads/main".into()),
        read_only: false,
        cache_dir: root.path().join("cache"),
        cancel_token: CancellationToken::new(),
    };
    let output = MountPipelineBuilder::new(config.clone()).execute().unwrap();
    output.engine.rename("base.txt", "moved.txt").await.unwrap();
    output
        .engine
        .rename("archive", "moved-archive")
        .await
        .unwrap();
    let mountpoint = root.path().join("view");
    std::fs::create_dir(&mountpoint).unwrap();
    std::fs::write(mountpoint.join("moved.txt"), b"different generation").unwrap();
    let destination = root.path().join("export");
    let lease = output.engine.open_read("moved.txt").unwrap();
    export_overlay_runtime(
        Arc::clone(&output.engine),
        output.overlay.clone().unwrap(),
        config.cache_dir,
        mountpoint,
        destination.clone(),
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read(destination.join("moved.txt")).unwrap(),
        b"base"
    );
    assert_eq!(
        std::fs::read(destination.join("moved-archive/child.txt")).unwrap(),
        b"captured child"
    );
    #[cfg(unix)]
    assert_eq!(
        std::fs::read_link(destination.join("moved-archive/link")).unwrap(),
        Path::new("../base.txt")
    );
    assert_eq!(
        output.engine.read_at(&lease, 0, 4).await.unwrap().as_ref(),
        b"base"
    );
    output.engine.shutdown_background().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn export_reads_unpublished_pointer_content_without_an_origin() {
    let root = tempfile::tempdir().unwrap();
    let source = source_repo(root.path());
    let config = PipelineConfig {
        source: source.display().to_string(),
        git_dir: source.join(".git"),
        git_metadata: GitMetadataOwnership::Borrowed,
        ref_name: Some("refs/heads/main".into()),
        read_only: false,
        cache_dir: root.path().join("cache"),
        cancel_token: CancellationToken::new(),
    };
    let mut output = MountPipelineBuilder::new(config.clone()).execute().unwrap();
    let content = (0..3 * 1024 * 1024)
        .map(|i| (i % 251) as u8)
        .collect::<Vec<_>>();
    output.engine.create("local.bin", 0o100_644).await.unwrap();
    output.engine.write("local.bin", 0, &content).await.unwrap();
    let reset = output.engine.begin_overlay_reset().await;
    let mut prepared = output.clone();
    commit_mount_runtime(
        &mut prepared,
        &config,
        &reset,
        "local pointer".into(),
        false,
    )
    .await
    .unwrap();
    drop(reset);
    output = prepared;
    output
        .engine
        .rename("local.bin", "moved.bin")
        .await
        .unwrap();
    let view = root.path().join("view");
    std::fs::create_dir(&view).unwrap();
    let destination = root.path().join("export");
    export_overlay_runtime(
        Arc::clone(&output.engine),
        output.overlay.clone().unwrap(),
        config.cache_dir,
        view,
        destination.clone(),
    )
    .await
    .unwrap();
    let exported = std::fs::read(destination.join("moved.bin")).unwrap();
    output.engine.shutdown_background().await;
    assert_eq!(blake3::hash(&exported), blake3::hash(&content));
    let staging = crab_staging::StagingAreaReadOnly::open(
        crate::repository::staging_root_for_git_dir(&config.git_dir).unwrap(),
    )
    .await
    .unwrap();
    assert!(staging.list_inflight().unwrap().is_empty());
}

#[expect(
    clippy::panic,
    reason = "release origin waiters before reporting failed test setup"
)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cancelled_export_waiter_retains_mutation_exclusion_until_copy_finishes() {
    struct Gate {
        first: std::sync::atomic::AtomicBool,
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }
    #[async_trait::async_trait]
    impl crab_read::XorbAvailability for Gate {
        async fn ensure_available(
            &self,
            _path: &object_store::path::Path,
        ) -> crab_read::Result<()> {
            if self.first.swap(false, std::sync::atomic::Ordering::AcqRel) {
                self.entered.notify_one();
                self.release.notified().await;
            }
            Ok(())
        }
    }
    let root = tempfile::tempdir().unwrap();
    let source = source_repo(root.path());
    let content = bytes::Bytes::from(
        (0..2 * 1024 * 1024)
            .map(|i| (i % 251) as u8)
            .collect::<Vec<_>>(),
    );
    let stored =
        crate::test_support::StoredPointer::new(&root.path().join("origin"), content.clone())
            .await
            .with_catalog()
            .await;
    let gate = Arc::new(Gate {
        first: std::sync::atomic::AtomicBool::new(true),
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let mut context = stored.context;
    context.hydrator = Arc::new(
        context
            .hydrator
            .as_ref()
            .clone()
            .with_availability(gate.clone()),
    );
    std::fs::write(source.join("model.bin"), stored.pointer.serialize()).unwrap();
    git(&source, ["add", "model.bin"]);
    git(&source, ["commit", "-m", "remote pointer"]);
    let config = PipelineConfig {
        source: source.display().to_string(),
        git_dir: source.join(".git"),
        git_metadata: GitMetadataOwnership::Borrowed,
        ref_name: Some("refs/heads/main".into()),
        read_only: false,
        cache_dir: root.path().join("cache"),
        cancel_token: CancellationToken::new(),
    };
    let output = MountPipelineBuilder::new(config.clone())
        .with_read_context(context)
        .execute()
        .unwrap();
    output
        .engine
        .rename("model.bin", "moved.bin")
        .await
        .unwrap();
    let view = root.path().join("view");
    std::fs::create_dir(&view).unwrap();
    let destination = root.path().join("export");
    let caller = tokio::spawn(export_overlay_runtime(
        Arc::clone(&output.engine),
        output.overlay.clone().unwrap(),
        config.cache_dir,
        view,
        destination.clone(),
    ));
    let entered =
        tokio::time::timeout(std::time::Duration::from_secs(5), gate.entered.notified()).await;
    if entered.is_err() {
        gate.release.notify_one();
        let _ = caller.await;
        output.engine.shutdown_background().await;
        panic!("export did not reach controlled origin read");
    }
    caller.abort();
    let _ = caller.await;
    let engine = Arc::clone(&output.engine);
    let mut mutation = tokio::spawn(async move { engine.create("after.txt", 0o100_644).await });
    let early = tokio::time::timeout(std::time::Duration::from_millis(100), &mut mutation).await;
    let excluded = early.is_err();
    gate.release.notify_one();
    match early {
        Ok(result) => {
            result.unwrap().unwrap();
        }
        Err(_) => {
            mutation.await.unwrap().unwrap();
        }
    }
    output.engine.shutdown_background().await;
    assert!(
        excluded,
        "caller cancellation released the export mutation gate"
    );
    assert_eq!(
        blake3::hash(&std::fs::read(destination.join("moved.bin")).unwrap()),
        blake3::hash(&content)
    );
}
