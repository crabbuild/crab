use super::*;

struct Catalog {
    next: std::sync::RwLock<Arc<crab_read::ShardHydrator>>,
    fail: std::sync::atomic::AtomicBool,
}

#[async_trait::async_trait]
impl crate::MountCatalogCapture for Catalog {
    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn capture(&self) -> Result<Arc<crab_read::ShardHydrator>> {
        if self.fail.load(std::sync::atomic::Ordering::Acquire) {
            return Err(std::io::Error::other("injected catalog capture failure").into());
        }
        Ok(Arc::clone(&self.next.read().unwrap()))
    }
}

#[cfg(feature = "fuse")]
#[tokio::test(flavor = "multi_thread")]
async fn published_open_pointer_uses_current_catalog_after_staging_retirement() {
    let root = tempfile::tempdir().unwrap();
    let source = source_repo(root.path());
    let old = crate::test_support::StoredPointer::new(
        &root.path().join("old-origin"),
        bytes::Bytes::from_static(b"previous catalog content"),
    )
    .await
    .with_catalog()
    .await;
    let content = bytes::Bytes::from_static(b"unpublished pointer content");
    let published = crate::test_support::StoredPointer::new(
        &root.path().join("published-origin"),
        content.clone(),
    )
    .await
    .with_catalog()
    .await;
    let catalog = Arc::new(Catalog {
        next: std::sync::RwLock::new(Arc::clone(&old.context.hydrator)),
        fail: std::sync::atomic::AtomicBool::new(false),
    });
    let mut context = old.context;
    context.catalog = catalog.clone();
    let config = PipelineConfig {
        source: source.display().to_string(),
        git_dir: source.join(".git"),
        git_metadata: GitMetadataOwnership::Borrowed,
        ref_name: Some("refs/heads/main".into()),
        read_only: false,
        cache_dir: root.path().join("cache"),
        cancel_token: CancellationToken::new(),
    };
    let mut output = MountPipelineBuilder::new(config.clone())
        .with_read_context(context)
        .execute()
        .unwrap();
    let engine = Arc::clone(&output.engine);
    engine.create("kept.bin", 0o100_644).await.unwrap();
    engine.write("kept.bin", 0, &content).await.unwrap();
    let mut held = engine.open_file("kept.bin", true, true).await.unwrap();
    let reset = engine.begin_overlay_reset().await;
    let committed =
        commit_mount_runtime(&mut output, &config, &reset, "local pointer".into(), false)
            .await
            .unwrap();
    drop(reset);

    // A confirmed push can retire local recipes before installing its new
    // catalog. An open inode must resume identical content through that view.
    let staging = Arc::new(
        crab_staging::StagingAreaReadOnly::open(
            crate::repository::staging_root_for_git_dir(&config.git_dir).unwrap(),
        )
        .await
        .unwrap(),
    );
    let hash = crab_xet::hash::MerkleHash::from(published.pointer.file_hash);
    let recipe = staging.published_recipe_for_file(&hash).unwrap().unwrap();
    let push =
        crab_staging::StagingPushSnapshot::begin(staging.clone(), "confirmed".into()).unwrap();
    push.capture(&[recipe]).unwrap();
    push.commit().unwrap();
    assert!(staging.published_recipe_for_file(&hash).unwrap().is_none());
    drop(staging);
    *catalog.next.write().unwrap() = Arc::clone(&published.context.hydrator);
    let reset = engine.begin_overlay_reset().await;
    adopt_published_snapshot(
        &mut output,
        &config,
        committed.commit_oid.as_deref().unwrap(),
        "refs/heads/main",
    )
    .await
    .unwrap();
    drop(reset);
    assert_eq!(engine.read("kept.bin", 0, 100).await.unwrap(), content);
    let written = engine
        .write_open_file(&mut held, Some("kept.bin"), 0, b"AFTER")
        .await;
    let visible = engine.read("kept.bin", 0, 100).await;
    drop(held);
    engine.shutdown_background().await;
    assert_eq!(
        (written.unwrap(), visible.unwrap()),
        (5, bytes::Bytes::from_static(b"AFTERlished pointer content"))
    );
}

#[cfg(feature = "fuse")]
#[tokio::test(flavor = "multi_thread")]
async fn unpublished_base_pointer_handle_survives_staging_retirement() {
    let root = tempfile::tempdir().unwrap();
    let source = source_repo(root.path());
    let old = crate::test_support::StoredPointer::new(
        &root.path().join("old-origin"),
        bytes::Bytes::from_static(b"previous catalog content"),
    )
    .await
    .with_catalog()
    .await;
    let content = bytes::Bytes::from_static(b"unpublished pointer content");
    let published = crate::test_support::StoredPointer::new(
        &root.path().join("published-origin"),
        content.clone(),
    )
    .await
    .with_catalog()
    .await;
    let catalog = Arc::new(Catalog {
        next: std::sync::RwLock::new(Arc::clone(&old.context.hydrator)),
        fail: std::sync::atomic::AtomicBool::new(false),
    });
    let mut context = old.context;
    context.catalog = catalog.clone();
    let config = PipelineConfig {
        source: source.display().to_string(),
        git_dir: source.join(".git"),
        git_metadata: GitMetadataOwnership::Borrowed,
        ref_name: Some("refs/heads/main".into()),
        read_only: false,
        cache_dir: root.path().join("cache"),
        cancel_token: CancellationToken::new(),
    };
    let mut output = MountPipelineBuilder::new(config.clone())
        .with_read_context(context)
        .execute()
        .unwrap();
    let engine = Arc::clone(&output.engine);
    engine.create("kept.bin", 0o100_644).await.unwrap();
    engine.write("kept.bin", 0, &content).await.unwrap();
    let reset = engine.begin_overlay_reset().await;
    let committed =
        commit_mount_runtime(&mut output, &config, &reset, "local pointer".into(), false)
            .await
            .unwrap();
    drop(reset);
    let mut held = engine.open_file("kept.bin", true, true).await.unwrap();

    // A confirmed push can retire local recipes before installing its new
    // catalog. An open inode must resume identical content through that view.
    let staging = Arc::new(
        crab_staging::StagingAreaReadOnly::open(
            crate::repository::staging_root_for_git_dir(&config.git_dir).unwrap(),
        )
        .await
        .unwrap(),
    );
    let hash = crab_xet::hash::MerkleHash::from(published.pointer.file_hash);
    let recipe = staging.published_recipe_for_file(&hash).unwrap().unwrap();
    let push =
        crab_staging::StagingPushSnapshot::begin(staging.clone(), "confirmed".into()).unwrap();
    push.capture(&[recipe]).unwrap();
    push.commit().unwrap();
    assert!(staging.published_recipe_for_file(&hash).unwrap().is_none());
    drop(staging);
    *catalog.next.write().unwrap() = Arc::clone(&published.context.hydrator);
    let reset = engine.begin_overlay_reset().await;
    adopt_published_snapshot(
        &mut output,
        &config,
        committed.commit_oid.as_deref().unwrap(),
        "refs/heads/main",
    )
    .await
    .unwrap();
    drop(reset);
    let retained = engine.read_open_file(&held, 0, 100).await;
    assert_eq!(engine.read("kept.bin", 0, 100).await.unwrap(), content);
    let written = engine
        .write_open_file(&mut held, Some("kept.bin"), 0, b"AFTER")
        .await;
    let visible = engine.read("kept.bin", 0, 100).await;
    drop(held);
    engine.shutdown_background().await;
    assert_eq!(
        (retained.unwrap(), written.unwrap(), visible.unwrap()),
        (
            content,
            5,
            bytes::Bytes::from_static(b"AFTERlished pointer content")
        )
    );
}

#[cfg(feature = "fuse")]
#[tokio::test(flavor = "multi_thread")]
async fn failed_post_commit_promotion_preserves_descriptor_and_retries() {
    for upgrade in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let source = source_repo(root.path());
        let old = crate::test_support::StoredPointer::new(
            &root.path().join("old-origin"),
            bytes::Bytes::from_static(b"previous catalog content"),
        )
        .await
        .with_catalog()
        .await;
        let content = bytes::Bytes::from_static(b"unpublished pointer content");
        let published = crate::test_support::StoredPointer::new(
            &root.path().join("published-origin"),
            content.clone(),
        )
        .await
        .with_catalog()
        .await;
        let catalog = Arc::new(Catalog {
            next: std::sync::RwLock::new(Arc::clone(&old.context.hydrator)),
            fail: std::sync::atomic::AtomicBool::new(false),
        });
        let mut context = old.context;
        context.catalog = catalog.clone();
        let config = PipelineConfig {
            source: source.display().to_string(),
            git_dir: source.join(".git"),
            git_metadata: GitMetadataOwnership::Borrowed,
            ref_name: Some("refs/heads/main".into()),
            read_only: false,
            cache_dir: root.path().join("cache"),
            cancel_token: CancellationToken::new(),
        };
        let mut output = MountPipelineBuilder::new(config.clone())
            .with_read_context(context)
            .execute()
            .unwrap();
        let engine = Arc::clone(&output.engine);
        engine.create("kept.bin", 0o100_644).await.unwrap();
        engine.write("kept.bin", 0, &content).await.unwrap();
        let mut held = engine.open_file("kept.bin", true, !upgrade).await.unwrap();
        let reset = engine.begin_overlay_reset().await;
        let committed =
            commit_mount_runtime(&mut output, &config, &reset, "local pointer".into(), false)
                .await
                .unwrap();
        drop(reset);
        let staging = Arc::new(
            crab_staging::StagingAreaReadOnly::open(
                crate::repository::staging_root_for_git_dir(&config.git_dir).unwrap(),
            )
            .await
            .unwrap(),
        );
        let hash = crab_xet::hash::MerkleHash::from(published.pointer.file_hash);
        let recipe = staging.published_recipe_for_file(&hash).unwrap().unwrap();
        let push =
            crab_staging::StagingPushSnapshot::begin(staging.clone(), "confirmed".into()).unwrap();
        push.capture(&[recipe]).unwrap();
        push.commit().unwrap();
        drop(staging);
        // Retirement can precede catalog adoption. Failed COW must leave the
        // original native descriptor usable and its publication binding retryable.
        let failed = if upgrade {
            held.enable_access(&engine, Some("kept.bin"), true, true)
                .await
                .map(|_| 0)
        } else {
            engine
                .write_open_file(&mut held, Some("kept.bin"), 0, b"AFTER")
                .await
        };
        let retained = engine.read_open_file(&held, 0, 100).await;
        *catalog.next.write().unwrap() = Arc::clone(&published.context.hydrator);
        let reset = engine.begin_overlay_reset().await;
        adopt_published_snapshot(
            &mut output,
            &config,
            committed.commit_oid.as_deref().unwrap(),
            "refs/heads/main",
        )
        .await
        .unwrap();
        drop(reset);
        if upgrade {
            held.enable_access(&engine, Some("kept.bin"), true, true)
                .await
                .unwrap();
        }
        let retried = engine
            .write_open_file(&mut held, Some("kept.bin"), 0, b"AFTER")
            .await;
        let visible = engine.read("kept.bin", 0, 100).await;
        drop(held);
        engine.shutdown_background().await;
        assert_eq!(
            (
                failed.is_err(),
                retained.unwrap(),
                retried.unwrap(),
                visible.unwrap()
            ),
            (
                true,
                content,
                5,
                bytes::Bytes::from_static(b"AFTERlished pointer content")
            )
        );
    }
}

#[cfg(feature = "fuse")]
#[tokio::test(flavor = "multi_thread")]
async fn failed_commit_adoption_preserves_open_content_and_retries_same_commit() {
    let root = tempfile::tempdir().unwrap();
    let source = source_repo(root.path());
    let stored = crate::test_support::StoredPointer::new(
        &root.path().join("origin"),
        bytes::Bytes::from_static(b"catalog content"),
    )
    .await
    .with_catalog()
    .await;
    let catalog = Arc::new(Catalog {
        next: std::sync::RwLock::new(Arc::clone(&stored.context.hydrator)),
        fail: std::sync::atomic::AtomicBool::new(true),
    });
    let mut context = stored.context.clone();
    context.catalog = catalog.clone();
    let config = PipelineConfig {
        source: source.display().to_string(),
        git_dir: source.join(".git"),
        git_metadata: GitMetadataOwnership::Borrowed,
        ref_name: Some("refs/heads/main".into()),
        read_only: false,
        cache_dir: root.path().join("cache"),
        cancel_token: CancellationToken::new(),
    };
    let mut output = MountPipelineBuilder::new(config.clone())
        .with_read_context(context)
        .execute()
        .unwrap();
    let engine = Arc::clone(&output.engine);
    engine.create("kept.txt", 0o100_644).await.unwrap();
    engine.write("kept.txt", 0, b"before").await.unwrap();
    let mut held = engine.open_file("kept.txt", true, true).await.unwrap();
    let initial = output.resolver.generation();
    let reset = engine.begin_overlay_reset().await;
    let failed = commit_mount_runtime(&mut output, &config, &reset, "capture fails".into(), false)
        .await
        .is_err();
    drop(reset);
    let preserved = (
        output.resolver.generation(),
        engine.read("kept.txt", 0, 100).await.unwrap(),
    );
    let committed = git_stdout(&source, ["rev-parse", "main"]);
    catalog
        .fail
        .store(false, std::sync::atomic::Ordering::Release);
    let reset = engine.begin_overlay_reset().await;
    let retried =
        commit_mount_runtime(&mut output, &config, &reset, "retry adoption".into(), false)
            .await
            .unwrap();
    drop(reset);
    engine
        .write_open_file(&mut held, Some("kept.txt"), 0, b"AFTER!")
        .await
        .unwrap();
    let visible = engine.read("kept.txt", 0, 100).await.unwrap();
    drop(held);
    engine.shutdown_background().await;
    assert_eq!(
        (
            failed,
            preserved,
            retried.commit_oid,
            visible,
            git_stdout(&source, ["rev-list", "--count", "main"])
        ),
        (
            true,
            (initial, bytes::Bytes::from_static(b"before")),
            Some(committed),
            bytes::Bytes::from_static(b"AFTER!"),
            "2".into()
        )
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn refresh_installs_captured_hintless_reader_only_after_successful_capture() {
    let root = tempfile::tempdir().unwrap();
    let source = source_repo(root.path());
    let old = crate::test_support::StoredPointer::new(
        &root.path().join("old-cache"),
        bytes::Bytes::from_static(b"old captured content"),
    )
    .await
    .with_catalog()
    .await;
    let new = crate::test_support::StoredPointer::new(
        &root.path().join("new-cache"),
        bytes::Bytes::from_static(b"new captured bytes"),
    )
    .await
    .with_catalog()
    .await;
    assert!(
        old.context
            .hydrator
            .reconstruct_from_pointer(&new.pointer.serialize())
            .await
            .is_err()
    );
    std::fs::write(source.join("model.bin"), old.pointer.serialize()).unwrap();
    git(&source, ["add", "model.bin"]);
    git(&source, ["commit", "-m", "old pointer"]);
    let catalog = Arc::new(Catalog {
        next: std::sync::RwLock::new(Arc::clone(&new.context.hydrator)),
        fail: std::sync::atomic::AtomicBool::new(true),
    });
    let mut context = old.context;
    context.catalog = catalog.clone();
    let config = PipelineConfig {
        source: source.display().to_string(),
        git_dir: source.join(".git"),
        git_metadata: GitMetadataOwnership::Borrowed,
        ref_name: Some("refs/heads/main".into()),
        read_only: true,
        cache_dir: root.path().join("mount-cache"),
        cancel_token: CancellationToken::new(),
    };
    let mut output = MountPipelineBuilder::new(config.clone())
        .with_read_context(context)
        .execute()
        .unwrap();
    let initial_generation = output.resolver.generation();
    assert_eq!(
        output.engine.read("model.bin", 0, 100).await.unwrap(),
        b"old captured content".as_slice()
    );
    std::fs::write(source.join("model.bin"), new.pointer.serialize()).unwrap();
    git(&source, ["add", "model.bin"]);
    git(&source, ["commit", "-m", "new pointer"]);
    assert!(
        refresh_mount_runtime(&mut output, &config, root.path())
            .await
            .is_err()
    );
    assert_eq!(output.resolver.generation(), initial_generation);
    assert_eq!(
        output.engine.read("model.bin", 0, 100).await.unwrap(),
        b"old captured content".as_slice()
    );
    catalog
        .fail
        .store(false, std::sync::atomic::Ordering::Release);
    refresh_mount_runtime(&mut output, &config, root.path())
        .await
        .unwrap();
    assert_eq!(
        output.engine.read("model.bin", 0, 100).await.unwrap(),
        b"new captured bytes".as_slice()
    );
    output.engine.shutdown_background().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn existing_auto_refresh_follows_the_ref_selected_by_manual_switch() {
    let root = tempfile::tempdir().unwrap();
    let source = source_repo(root.path());
    git(&source, ["checkout", "-b", "other"]);
    std::fs::write(source.join("base.txt"), b"other").unwrap();
    git(&source, ["add", "base.txt"]);
    git(&source, ["commit", "-m", "other branch"]);
    git(&source, ["checkout", "main"]);
    let mut config = PipelineConfig {
        source: source.display().to_string(),
        git_dir: source.join(".git"),
        git_metadata: GitMetadataOwnership::Borrowed,
        ref_name: Some("refs/heads/main".into()),
        read_only: false,
        cache_dir: root.path().join("cache"),
        cancel_token: CancellationToken::new(),
    };
    let mut output = MountPipelineBuilder::new(config.clone()).execute().unwrap();
    let refresh = crate::pipeline::spawn_refresh_loop(
        &output,
        &config,
        &root.path().join("mount"),
        std::time::Duration::from_millis(10),
    )
    .unwrap();
    switch_mount_runtime(&mut output, &mut config, root.path(), "other")
        .await
        .unwrap();
    git(&source, ["checkout", "other"]);
    std::fs::write(source.join("base.txt"), b"advanced other").unwrap();
    git(&source, ["add", "base.txt"]);
    git(&source, ["commit", "-m", "advance selected branch"]);
    let expected = git_stdout(&source, ["rev-parse", "HEAD"]);
    git(&source, ["checkout", "main"]);
    let observed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while output.resolver.head().as_ref().map(|head| &head.0) != Some(&expected) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;
    config.cancel_token.cancel();
    refresh.await.unwrap();
    let bytes = output.engine.read("base.txt", 0, 100).await;
    output.engine.shutdown_background().await;
    assert!(
        observed.is_ok(),
        "existing watcher did not adopt the selected branch"
    );
    assert_eq!(bytes.unwrap(), b"advanced other".as_slice());
}

#[tokio::test(flavor = "multi_thread")]
async fn inflight_pointer_read_keeps_its_captured_catalog_during_refresh() -> Result<()> {
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
    let old = crate::test_support::StoredPointer::new(
        &root.path().join("old-cache"),
        bytes::Bytes::from_static(b"old captured content"),
    )
    .await
    .with_catalog()
    .await;
    let new = crate::test_support::StoredPointer::new(
        &root.path().join("new-cache"),
        bytes::Bytes::from_static(b"new captured bytes"),
    )
    .await
    .with_catalog()
    .await;
    let gate = Arc::new(Gate {
        first: std::sync::atomic::AtomicBool::new(true),
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let mut context = old.context;
    context.hydrator = Arc::new(
        context
            .hydrator
            .as_ref()
            .clone()
            .with_availability(gate.clone()),
    );
    context.catalog = new.context.catalog;
    std::fs::write(source.join("model.bin"), old.pointer.serialize()).unwrap();
    git(&source, ["add", "model.bin"]);
    git(&source, ["commit", "-m", "old pointer"]);
    let config = PipelineConfig {
        source: source.display().to_string(),
        git_dir: source.join(".git"),
        git_metadata: GitMetadataOwnership::Borrowed,
        ref_name: Some("refs/heads/main".into()),
        read_only: true,
        cache_dir: root.path().join("cache"),
        cancel_token: CancellationToken::new(),
    };
    let mut output = MountPipelineBuilder::new(config.clone())
        .with_read_context(context)
        .execute()
        .unwrap();
    let engine = Arc::clone(&output.engine);
    let lease = engine.open_read("model.bin").unwrap();
    let read = tokio::spawn(async move { engine.read_at(&lease, 0, 100).await });
    let entered =
        tokio::time::timeout(std::time::Duration::from_secs(5), gate.entered.notified()).await;
    if entered.is_err() {
        read.abort();
        let _ = read.await;
        output.engine.shutdown_background().await;
        return Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "old read did not reach origin admission",
        )
        .into());
    }
    std::fs::write(source.join("model.bin"), new.pointer.serialize()).unwrap();
    git(&source, ["add", "model.bin"]);
    git(&source, ["commit", "-m", "new pointer"]);
    let refreshed = refresh_mount_runtime(&mut output, &config, root.path()).await;
    gate.release.notify_one();
    let old_bytes = read.await.unwrap();
    let new_bytes = output.engine.read("model.bin", 0, 100).await;
    output.engine.shutdown_background().await;
    refreshed.unwrap();
    assert_eq!(
        (old_bytes.unwrap(), new_bytes.unwrap()),
        (
            bytes::Bytes::from_static(b"old captured content"),
            bytes::Bytes::from_static(b"new captured bytes")
        )
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn short_branch_auto_refresh_preserves_the_selected_commit_on_remount() {
    let root = tempfile::tempdir().unwrap();
    let source = source_repo(root.path());
    let mut config = PipelineConfig {
        source: source.display().to_string(),
        git_dir: root.path().join("cache/.git"),
        git_metadata: GitMetadataOwnership::Mount,
        ref_name: Some("main".into()),
        read_only: false,
        cache_dir: root.path().join("cache"),
        cancel_token: CancellationToken::new(),
    };
    let output = MountPipelineBuilder::new(config.clone()).execute().unwrap();
    let refresh = crate::pipeline::spawn_refresh_loop(
        &output,
        &config,
        &root.path().join("mount"),
        std::time::Duration::from_millis(10),
    )
    .unwrap();
    std::fs::write(source.join("base.txt"), b"advanced main").unwrap();
    git(&source, ["add", "base.txt"]);
    git(&source, ["commit", "-m", "advance main"]);
    let expected = git_stdout(&source, ["rev-parse", "HEAD"]);
    let observed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while output.resolver.head().as_ref().map(|head| &head.0) != Some(&expected) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;
    config.cancel_token.cancel();
    refresh.await.unwrap();
    output.engine.shutdown_background().await;
    let local_head = git_dir_stdout(&config.git_dir, ["rev-parse", "refs/heads/main"]);
    drop(output);
    config.cancel_token = CancellationToken::new();
    let remount = MountPipelineBuilder::new(config).execute().unwrap();
    let bytes = remount.engine.read("base.txt", 0, 100).await;
    remount.engine.shutdown_background().await;
    observed.unwrap();
    assert_eq!(
        (local_head, bytes.unwrap()),
        (expected, bytes::Bytes::from_static(b"advanced main")),
    );
}
