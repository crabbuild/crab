use super::*;
use crate::engine::OverlayWriter;
use crate::pipeline::MountPipelineBuilder;
use crate::source::MountSource;
use tokio_util::sync::CancellationToken;

fn git<const N: usize>(repo: &Path, args: [&str; N]) {
    let _git_env = crate::test_support::GIT_DIR_MUTEX
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} in {}: {}",
        repo.display(),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn git_in<const N: usize>(dir: &Path, args: [&str; N]) {
    let _git_env = crate::test_support::GIT_DIR_MUTEX
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .status()
        .unwrap();
    assert!(status.success());
}

fn git_stdout<const N: usize>(repo: &Path, args: [&str; N]) -> String {
    let _git_env = crate::test_support::GIT_DIR_MUTEX
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn git_dir_stdout<const N: usize>(git_dir: &Path, args: [&str; N]) -> String {
    let _git_env = crate::test_support::GIT_DIR_MUTEX
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let output = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .output()
        .unwrap();
    assert!(output.status.success());
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

#[test]
fn switch_normalizes_branch_names() {
    assert_eq!(normalized_fetch_ref("main").unwrap(), "refs/heads/main");
    assert_eq!(
        normalized_fetch_ref("refs/tags/v1").unwrap(),
        "refs/tags/v1"
    );
}

fn source_repo(root: &Path) -> std::path::PathBuf {
    let source = root.join("source");
    std::fs::create_dir_all(&source).unwrap();
    git(&source, ["init", "-b", "main"]);
    git(&source, ["config", "user.email", "audit@localhost"]);
    git(&source, ["config", "user.name", "VFS audit"]);
    std::fs::write(source.join("base.txt"), b"base").unwrap();
    std::fs::write(source.join(".gitattributes"), b"*.bin filter=crab\n").unwrap();
    git(&source, ["add", "."]);
    git(&source, ["commit", "-m", "base"]);
    source
}

#[cfg(feature = "fuse")]
#[tokio::test(flavor = "multi_thread")]
async fn commit_keeps_open_inode_linked_for_later_writes() {
    let root = tempfile::tempdir().unwrap();
    let source = source_repo(root.path());
    let config = PipelineConfig {
        source: source.display().to_string(),
        git_dir: source.join(".git"),
        git_metadata: crate::pipeline::GitMetadataOwnership::Borrowed,
        ref_name: Some("refs/heads/main".into()),
        read_only: false,
        cache_dir: root.path().join("cache"),
        cancel_token: CancellationToken::new(),
    };
    let mut output = MountPipelineBuilder::new(config.clone()).execute().unwrap();
    let engine = Arc::clone(&output.engine);
    engine.create("kept.txt", 0o100_644).await.unwrap();
    engine.write("kept.txt", 0, b"before").await.unwrap();
    let mut held = engine.open_file("kept.txt", true, true).await.unwrap();
    for expected in [b"AFTER!", b"THIRD!", b"FOURTH"] {
        let reset = engine.begin_overlay_reset().await;
        commit_mount_runtime(
            &mut output,
            &config,
            &reset,
            "keep open inode".into(),
            false,
        )
        .await
        .unwrap();
        drop(reset);
        engine
            .write_open_file(&mut held, Some("kept.txt"), 0, expected)
            .await
            .unwrap();
        engine.sync_open_file(&held, false).unwrap();
        assert_eq!(
            (
                engine.read_open_file(&held, 0, 100).await.unwrap(),
                engine.read("kept.txt", 0, 100).await.unwrap()
            ),
            (
                bytes::Bytes::copy_from_slice(expected),
                bytes::Bytes::copy_from_slice(expected)
            )
        );
    }
    drop(held);
    engine.shutdown_background().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn initial_pipeline_honors_requested_ref() {
    let root = tempfile::tempdir().unwrap();
    let source = source_repo(root.path());
    git(&source, ["checkout", "-b", "other"]);
    std::fs::write(source.join("other.txt"), b"other").unwrap();
    git(&source, ["add", "."]);
    git(&source, ["commit", "-m", "other"]);
    let requested_oid = git_stdout(&source, ["rev-parse", "HEAD"]);
    git(&source, ["checkout", "main"]);
    let config = PipelineConfig {
        source: source.display().to_string(),
        git_dir: source.join(".git"),
        git_metadata: crate::pipeline::GitMetadataOwnership::Borrowed,
        ref_name: Some("refs/heads/other".into()),
        read_only: true,
        cache_dir: root.path().join("cache"),
        cancel_token: CancellationToken::new(),
    };
    let output = MountPipelineBuilder::new(config).execute().unwrap();
    output.hydration.shutdown().await;
    assert_eq!(output.head_oid, requested_oid);
}

#[tokio::test(flavor = "multi_thread")]
async fn borrowed_mount_with_remote_data_origin_preserves_staged_index() {
    let root = tempfile::tempdir().unwrap();
    let source = source_repo(root.path());
    std::fs::write(source.join("base.txt"), b"staged change").unwrap();
    git(&source, ["add", "base.txt"]);
    let before = git_stdout(&source, ["rev-parse", ":base.txt"]);
    let config = PipelineConfig {
        source: "crab://bucket/read-origin".into(),
        git_dir: source.join(".git"),
        git_metadata: crate::pipeline::GitMetadataOwnership::Borrowed,
        ref_name: Some("refs/heads/main".into()),
        read_only: true,
        cache_dir: root.path().join("cache"),
        cancel_token: CancellationToken::new(),
    };
    let output = MountPipelineBuilder::new(config).execute().unwrap();
    output.hydration.shutdown().await;
    assert_eq!(
        git_stdout(&source, ["rev-parse", ":base.txt"]),
        before,
        "read-only mount rewrote the source repository's staged version"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn borrowed_remote_data_origin_auto_refresh_preserves_source_index() {
    let root = tempfile::tempdir().unwrap();
    let source = source_repo(root.path());
    let config = PipelineConfig {
        source: "crab://bucket/read-origin".into(),
        git_dir: source.join(".git"),
        git_metadata: GitMetadataOwnership::Borrowed,
        ref_name: Some("refs/heads/main".into()),
        read_only: false,
        cache_dir: root.path().join("cache"),
        cancel_token: CancellationToken::new(),
    };
    let output = MountPipelineBuilder::new(config.clone()).execute().unwrap();
    std::fs::write(source.join("base.txt"), b"advanced").unwrap();
    git(&source, ["add", "base.txt"]);
    git(&source, ["commit", "-m", "source advance"]);
    std::fs::write(source.join("base.txt"), b"staged after advance").unwrap();
    git(&source, ["add", "base.txt"]);
    let before = std::fs::read(config.git_dir.join("index")).unwrap();
    let expected_oid = git_stdout(&source, ["rev-parse", "HEAD"]);
    let handle = crate::pipeline::spawn_refresh_loop(
        &output,
        &config,
        &root.path().join("mount"),
        std::time::Duration::from_millis(10),
    )
    .unwrap();
    let refreshed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if output.snapshot.head_oid().unwrap().as_deref() == Some(&expected_oid)
                && output.snapshot.current_generation().unwrap()
                    == Some(output.resolver.generation())
            {
                return output.engine.read("base.txt", 0, 1024).await;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;
    config.cancel_token.cancel();
    handle.await.unwrap();
    output.hydration.shutdown().await;
    assert_eq!(
        (
            refreshed.unwrap().unwrap(),
            std::fs::read(config.git_dir.join("index")).unwrap()
        ),
        (bytes::Bytes::from_static(b"advanced"), before)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn owned_clone_of_local_source_fetches_refs_and_refreshes_only_its_index() {
    let root = tempfile::tempdir().unwrap();
    let source = source_repo(root.path());
    let mut config = PipelineConfig {
        source: source.display().to_string(),
        git_dir: root.path().join("mount.git"),
        git_metadata: GitMetadataOwnership::Mount,
        ref_name: Some("refs/heads/main".into()),
        read_only: false,
        cache_dir: root.path().join("cache"),
        cancel_token: CancellationToken::new(),
    };
    let mut output = MountPipelineBuilder::new(config.clone()).execute().unwrap();
    git(&source, ["checkout", "-b", "other"]);
    std::fs::write(source.join("other.txt"), b"other").unwrap();
    git(&source, ["add", "other.txt"]);
    git(&source, ["commit", "-m", "other"]);
    let other_oid = git_stdout(&source, ["rev-parse", "HEAD"]);
    let other_tree = git_stdout(&source, ["rev-parse", "HEAD^{tree}"]);
    git(&source, ["checkout", "main"]);
    std::fs::write(source.join("base.txt"), b"unrelated staged source data").unwrap();
    git(&source, ["add", "base.txt"]);
    let before = std::fs::read(source.join(".git/index")).unwrap();
    switch_mount_runtime(&mut output, &mut config, root.path(), "other")
        .await
        .unwrap();

    // Advance only the source ref: its worktree/index and the cached clone
    // stay untouched, so the mount must fetch rather than watch its own HEAD.
    let advanced_oid = git_stdout(
        &source,
        [
            "commit-tree",
            &other_tree,
            "-p",
            &other_oid,
            "-m",
            "advance other",
        ],
    );
    git(&source, ["update-ref", "refs/heads/other", &advanced_oid]);
    let handle = crate::pipeline::spawn_refresh_loop(
        &output,
        &config,
        &root.path().join("mount"),
        std::time::Duration::from_millis(10),
    )
    .unwrap();
    let refreshed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if output.snapshot.head_oid().unwrap().as_deref() == Some(&advanced_oid)
                && output.snapshot.current_generation().unwrap()
                    == Some(output.resolver.generation())
            {
                return output.engine.read("other.txt", 0, 1024).await;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;
    config.cancel_token.cancel();
    handle.await.unwrap();
    output.hydration.shutdown().await;
    assert_eq!(
        (
            refreshed.unwrap().unwrap(),
            std::fs::read(source.join(".git/index")).unwrap(),
            git_dir_stdout(&config.git_dir, ["rev-parse", "refs/heads/other"]),
            git_dir_stdout(&config.git_dir, ["rev-parse", ":other.txt"]),
        ),
        (
            bytes::Bytes::from_static(b"other"),
            before,
            advanced_oid,
            git_stdout(&source, ["rev-parse", "refs/heads/other:other.txt"]),
        )
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn local_refresh_and_switch_preserve_source_state() {
    for kind in ["worktree", "bare", "linked"] {
        for read_only in [true, false] {
            for remote_data_origin in [false, true] {
                let root = tempfile::tempdir().unwrap();
                let source = source_repo(root.path());
                let main_oid = git_stdout(&source, ["rev-parse", "HEAD"]);
                git(&source, ["checkout", "-b", "other"]);
                std::fs::write(source.join("other.txt"), b"other").unwrap();
                git(&source, ["add", "other.txt"]);
                git(&source, ["commit", "-m", "other"]);
                let other_oid = git_stdout(&source, ["rev-parse", "HEAD"]);
                let staged_oid = git_stdout(&source, ["rev-parse", "HEAD:other.txt"]);
                git(&source, ["checkout", "main"]);
                let mount_source = match kind {
                    "bare" => {
                        let bare = root.path().join("bare.git");
                        git(
                            root.path(),
                            [
                                "clone",
                                "--bare",
                                source.to_str().unwrap(),
                                bare.to_str().unwrap(),
                            ],
                        );
                        git(&bare, ["read-tree", "main"]);
                        bare
                    }
                    "linked" => {
                        let linked = root.path().join("linked");
                        git(
                            &source,
                            [
                                "worktree",
                                "add",
                                "-b",
                                "linked",
                                linked.to_str().unwrap(),
                                "main",
                            ],
                        );
                        linked
                    }
                    _ => source,
                };
                let cacheinfo = format!("100644,{staged_oid},base.txt");
                git(&mount_source, ["update-index", "--cacheinfo", &cacheinfo]);
                let git_dir = MountSource::validate_local(&mount_source).unwrap();
                let before = (
                    std::fs::read(git_dir.join("index")).unwrap(),
                    std::fs::read(git_dir.join("HEAD")).unwrap(),
                );
                let mut config = PipelineConfig {
                    source: if remote_data_origin {
                        "crab://bucket/read-origin".into()
                    } else {
                        mount_source.display().to_string()
                    },
                    git_dir: git_dir.clone(),
                    git_metadata: crate::pipeline::GitMetadataOwnership::Borrowed,
                    ref_name: Some("refs/heads/main".into()),
                    read_only,
                    cache_dir: root.path().join("cache"),
                    cancel_token: CancellationToken::new(),
                };
                let mut output = MountPipelineBuilder::new(config.clone()).execute().unwrap();
                let initial_oid = output.head_oid.clone();
                let mountpoint = root.path().join("mount");
                refresh_mount_runtime(&mut output, &config, &mountpoint)
                    .await
                    .unwrap();
                let switched = switch_mount_runtime(&mut output, &mut config, &mountpoint, "other")
                    .await
                    .unwrap();
                let refreshed = refresh_mount_runtime(&mut output, &config, &mountpoint)
                    .await
                    .unwrap();
                adopt_published_snapshot(&mut output, &config, &other_oid, "refs/heads/other")
                    .await
                    .unwrap();
                let content = output.engine.read("other.txt", 0, 100).await;
                output.hydration.shutdown().await;
                let after = (
                    std::fs::read(git_dir.join("index")).unwrap(),
                    std::fs::read(git_dir.join("HEAD")).unwrap(),
                );
                assert_eq!(
                    (
                        initial_oid,
                        switched.head_oid,
                        refreshed.head_oid,
                        after,
                        content.unwrap()
                    ),
                    (
                        main_oid,
                        other_oid.clone(),
                        other_oid,
                        before,
                        bytes::Bytes::from_static(b"other")
                    ),
                    "source={kind}, read_only={read_only}, remote_data_origin={remote_data_origin}"
                );
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn audit_local_pointer_commit_remains_readable() {
    let root = tempfile::tempdir().unwrap();
    let source = source_repo(root.path());
    let stored = crate::test_support::StoredPointer::new(
        &root.path().join("shared"),
        bytes::Bytes::from_static(b"remote base"),
    )
    .await;
    let config = PipelineConfig {
        source: source.display().to_string(),
        git_dir: source.join(".git"),
        git_metadata: crate::pipeline::GitMetadataOwnership::Borrowed,
        ref_name: Some("refs/heads/main".into()),
        read_only: false,
        cache_dir: root.path().join("cache"),
        cancel_token: CancellationToken::new(),
    };
    let mut output = MountPipelineBuilder::new(config.clone())
        .with_read_context(stored.context)
        .execute()
        .unwrap();
    output.engine.create("new.bin", 0o100_644).await.unwrap();
    for (revision, expected) in [
        ("first", b"new local bytes".as_slice()),
        ("second", b"NEW local bytes".as_slice()),
    ] {
        output.engine.write("new.bin", 0, expected).await.unwrap();
        let engine = output.engine.clone();
        let reset = engine.begin_overlay_reset().await;
        commit_mount_runtime(
            &mut output,
            &config,
            &reset,
            format!("local pointer {revision}"),
            false,
        )
        .await
        .unwrap();
        drop(reset);
        git(&source, ["branch", revision]);
        assert_eq!(
            output.engine.read("new.bin", 0, 1024).await.unwrap(),
            expected
        );
        assert_eq!(
            output
                .hydration
                .read_stats_snapshot()
                .read_window_remote_fetches,
            0
        );
        output.hydration.shutdown().await;
        drop(output);
        // No remote context or warm read windows can conceal lost staging.
        output = MountPipelineBuilder::new(config.clone()).execute().unwrap();
        assert_eq!(
            output.engine.read("new.bin", 0, 1024).await.unwrap(),
            expected
        );
        assert_eq!(
            output
                .hydration
                .read_stats_snapshot()
                .read_window_remote_fetches,
            0
        );
    }
    let mut config = config;
    switch_mount_runtime(&mut output, &mut config, root.path(), "first")
        .await
        .unwrap();
    let older = output.engine.read("new.bin", 0, 1024).await;
    output.hydration.shutdown().await;
    assert_eq!(older.unwrap(), b"new local bytes".as_slice());
}

#[test]
fn resolve_refresh_head_fetches_remote_tracked_ref_into_mount_cache() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source");
    let remote = dir.path().join("remote.git");
    let mount_git = dir.path().join("mount.git");

    std::fs::create_dir_all(&source).unwrap();
    git(&source, ["init", "-b", "main"]);
    git(
        &source,
        ["config", "user.email", "mount-runtime-test@crab.local"],
    );
    git(&source, ["config", "user.name", "mount runtime test"]);
    std::fs::write(source.join("file.txt"), "base").unwrap();
    git(&source, ["add", "file.txt"]);
    git(&source, ["commit", "-m", "base"]);
    git_in(
        dir.path(),
        [
            "clone",
            "--bare",
            source.to_str().unwrap(),
            remote.to_str().unwrap(),
        ],
    );
    git_in(
        dir.path(),
        [
            "clone",
            "--bare",
            "--filter=blob:none",
            remote.to_str().unwrap(),
            mount_git.to_str().unwrap(),
        ],
    );

    let old_head = git_dir_stdout(&mount_git, ["rev-parse", "refs/heads/main"]);
    std::fs::write(source.join("file.txt"), "updated").unwrap();
    git(&source, ["add", "file.txt"]);
    git(&source, ["commit", "-m", "update"]);
    git(&source, ["push", remote.to_str().unwrap(), "main"]);

    let (head_oid, head_ref) = resolve_refresh_head(
        &mount_git,
        GitMetadataOwnership::Mount,
        Some("refs/heads/main"),
    )
    .unwrap();
    let new_head = git_stdout(&source, ["rev-parse", "HEAD"]);

    assert_ne!(old_head, new_head);
    assert_eq!(head_ref, "refs/heads/main");
    assert_eq!(head_oid, new_head);
    assert_eq!(
        git_dir_stdout(&mount_git, ["rev-parse", "refs/heads/main"]),
        new_head
    );
}

#[tokio::test]
async fn committed_generations_are_adopted_before_pruning_live_resolver() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source");
    let cache = dir.path().join("cache");
    std::fs::create_dir_all(&source).unwrap();
    git(&source, ["init", "-b", "main"]);
    git(
        &source,
        ["config", "user.email", "mount-runtime-test@crab.local"],
    );
    git(&source, ["config", "user.name", "mount runtime test"]);
    std::fs::write(source.join("base.txt"), "base").unwrap();
    git(&source, ["add", "base.txt"]);
    git(&source, ["commit", "-m", "base"]);

    let config = PipelineConfig {
        source: source.display().to_string(),
        git_dir: source.join(".git"),
        git_metadata: crate::pipeline::GitMetadataOwnership::Borrowed,
        ref_name: Some("refs/heads/main".to_owned()),
        read_only: false,
        cache_dir: cache.clone(),
        cancel_token: CancellationToken::new(),
    };
    let mut output = MountPipelineBuilder::new(config.clone()).execute().unwrap();
    let overlay = std::sync::Arc::clone(output.overlay.as_ref().unwrap());

    for (path, content, message) in [
        ("first.txt", b"first".as_slice(), "first mounted commit"),
        ("second.txt", b"second".as_slice(), "second mounted commit"),
    ] {
        overlay.create_file(path, 0o100_644).unwrap();
        overlay.write_file(path, 0, content).unwrap();
        let engine = std::sync::Arc::clone(&output.engine);
        let reset = engine.begin_overlay_reset().await;
        commit_mount_runtime(&mut output, &config, &reset, message.to_owned(), false)
            .await
            .unwrap();
    }

    assert_eq!(output.resolver.generation(), 3);
    assert!(output.snapshot.get_node(1, "base.txt").unwrap().is_none());
    let names = output
        .resolver
        .readdir("")
        .unwrap()
        .into_iter()
        .map(|entry| entry.name)
        .collect::<Vec<_>>();
    assert_eq!(names, ["base.txt", "first.txt", "second.txt"]);
}

#[path = "mount_runtime_catalog_tests.rs"]
mod catalog;

#[tokio::test(flavor = "multi_thread")]
async fn inspection_preserves_open_read_leases() {
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
    let output = MountPipelineBuilder::new(config).execute().unwrap();
    output.engine.write("base.txt", 0, b"edit").await.unwrap();
    let lease = output.engine.open_read("base.txt").unwrap();
    let diff = inspect_overlay_runtime(
        Arc::clone(&output.engine),
        output.overlay.clone().unwrap(),
        root.path().join("inactive-view"),
    )
    .await
    .unwrap();
    let visible = output.engine.read_at(&lease, 0, 4).await;
    output.engine.shutdown_background().await;
    assert_eq!(diff.changes[0].path, "base.txt");
    assert_eq!(visible.unwrap().as_ref(), b"edit");
}
