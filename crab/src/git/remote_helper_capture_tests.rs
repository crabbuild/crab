use super::tests::{GitEnvCwdGuard, capsule_history_fixture, git_object_exists, run_git};
use super::*;

struct RefAdvance {
    store: crate::storage::store::Store,
    router: StoreLayout,
    head_path: object_store::path::Path,
    before: bytes::Bytes,
    after: bytes::Bytes,
    advertised: String,
    concurrent: String,
    advertised_blob: String,
    concurrent_blob: String,
}

async fn advancing_ref(checkpoint: bool) -> RefAdvance {
    let (source, store, router, _, _) = Box::pin(capsule_history_fixture()).await;
    let layout = crab_storage::StoreLayout::with_global_prefix(
        store.as_storage().clone(),
        router.repo_prefix().to_owned(),
        router.global_prefix().to_owned(),
    );
    let head_path = layout.capsule_ref_head_path(
        &crab_metadata::capsule_protocol::capsule_ref_name_key("refs/heads/main"),
    );
    let mut states = Vec::new();
    let cancel = tokio_util::sync::CancellationToken::new();
    for label in ["advertised", "concurrent"] {
        std::fs::write(source.path().join("history.txt"), label.as_bytes()).unwrap();
        run_git(source.path(), &["add", "history.txt"]);
        run_git(source.path(), &["commit", "-qm", label]);
        let git_dir = source.path().join(".git");
        {
            let _guard = GitEnvCwdGuard::set(source.path(), &git_dir, source.path());
            let (result, _) = Box::pin(crate::git::capsule_push::run(
                &PushConfig {
                    git_dir: Some(git_dir.clone()),
                    ..Default::default()
                },
                &[PushSpec {
                    force: false,
                    src: "HEAD".to_owned(),
                    dst: "refs/heads/main".to_owned(),
                }],
                &store,
                &router,
                None,
                &[],
                None,
                None,
                None,
                &cancel,
            ))
            .await
            .unwrap();
            assert!(result.all_ok());
        }
        if checkpoint && label == "advertised" {
            assert!(
                Box::pin(crab_remote::checkpoint::publish_capsule_checkpoint(
                    &layout,
                    1,
                    64 * 1024 * 1024,
                    &cancel,
                ))
                .await
                .unwrap()
                .published
            );
        }
        let head = store.get_with_etag(&head_path).await.unwrap().0;
        let root = crab_write::capsule_protocol::open_root(&layout)
            .await
            .unwrap();
        let oid = String::from_utf8(run_git(source.path(), &["rev-parse", "HEAD"]))
            .unwrap()
            .trim()
            .to_owned();
        let blob = String::from_utf8(run_git(source.path(), &["rev-parse", "HEAD:history.txt"]))
            .unwrap()
            .trim()
            .to_owned();
        states.push((head, root.record().digest().to_owned(), oid, blob));
    }
    let (after, after_root, concurrent, concurrent_blob) = states.pop().unwrap();
    let (before, before_root, advertised, advertised_blob) = states.pop().unwrap();
    assert_eq!(before_root, after_root);
    store
        .put_overwrite(&head_path, before.clone())
        .await
        .unwrap();
    RefAdvance {
        store,
        router,
        head_path,
        before,
        after,
        advertised,
        concurrent,
        advertised_blob,
        concurrent_blob,
    }
}

async fn request(
    batch: Batch,
    options: &HelperOptions,
    fixture: &RefAdvance,
    cache: &mut SessionCache,
) -> Result<Vec<u8>> {
    let mut output = Vec::new();
    Box::pin(dispatch_batch(
        &batch,
        options,
        &mut output,
        Some(&fixture.store),
        &PushStaging::Missing,
        fixture.router.repo_prefix(),
        cache,
        "origin",
        None,
        OutputMode::Text,
        None,
        None,
        None,
        None,
        &tokio_util::sync::CancellationToken::new(),
    ))
    .await?;
    Ok(output)
}

async fn advertised_fetch_survives_advance(checkpoint: bool) {
    let fixture = Box::pin(advancing_ref(checkpoint)).await;
    let target = tempfile::tempdir().unwrap();
    run_git(target.path(), &["init", "-q"]);
    let git_dir = target.path().join(".git");
    let _guard = GitEnvCwdGuard::set(target.path(), &git_dir, target.path());
    let mut cache = SessionCache::new(Default::default());
    let mut options = HelperOptions::default();
    if checkpoint {
        Box::pin(request(Batch::Capabilities, &options, &fixture, &mut cache))
            .await
            .unwrap();
        options.fetch_options.depth = Some(1);
    }
    let listed = Box::pin(request(
        Batch::List { for_push: false },
        &options,
        &fixture,
        &mut cache,
    ))
    .await
    .unwrap();
    assert!(String::from_utf8_lossy(&listed).contains(&fixture.advertised));
    fixture
        .store
        .put_overwrite(&fixture.head_path, fixture.after.clone())
        .await
        .unwrap();
    let entries = vec![FetchEntry {
        sha: fixture.advertised.clone(),
        ref_name: "refs/heads/main".to_owned(),
    }];
    let fetched = Box::pin(request(
        Batch::Fetch(entries),
        &options,
        &fixture,
        &mut cache,
    ))
    .await;
    cache.git_runtime.shutdown().await;
    assert!(
        fetched.is_ok(),
        "advertised classic snapshot changed: {fetched:?}"
    );
    assert!(git_object_exists(target.path(), &fixture.advertised));
    assert!(!git_object_exists(target.path(), &fixture.concurrent));
    assert_eq!(
        run_git(
            target.path(),
            &["cat-file", "blob", &fixture.advertised_blob]
        ),
        b"advertised"
    );
    run_git(target.path(), &["fsck", "--strict", "--full"]);
    let repeated = Box::pin(request(
        Batch::List { for_push: false },
        &options,
        &fixture,
        &mut cache,
    ))
    .await
    .unwrap();
    assert_eq!(repeated, listed);
    assert_ne!(fixture.before, fixture.after);
}

#[tokio::test]
async fn classic_uncheckpointed_fetch_keeps_advertised_heads() {
    Box::pin(advertised_fetch_survives_advance(false)).await;
}

#[tokio::test]
async fn classic_shallow_promotion_keeps_advertised_heads() {
    Box::pin(advertised_fetch_survives_advance(true)).await;
}

#[tokio::test]
async fn classic_promisor_rejects_objects_published_after_discovery() {
    let fixture = Box::pin(advancing_ref(false)).await;
    let target = tempfile::tempdir().unwrap();
    run_git(target.path(), &["init", "-q"]);
    let git_dir = target.path().join(".git");
    let _guard = GitEnvCwdGuard::set(target.path(), &git_dir, target.path());
    let mut cache = SessionCache::new(Default::default());
    let options = HelperOptions {
        filter_requested: true,
        ..Default::default()
    };
    Box::pin(request(
        Batch::List { for_push: false },
        &options,
        &fixture,
        &mut cache,
    ))
    .await
    .unwrap();
    fixture
        .store
        .put_overwrite(&fixture.head_path, fixture.after.clone())
        .await
        .unwrap();
    let entry = |oid: &str| {
        Batch::Fetch(vec![FetchEntry {
            sha: oid.to_owned(),
            ref_name: oid.to_owned(),
        }])
    };
    Box::pin(request(
        entry(&fixture.advertised_blob),
        &options,
        &fixture,
        &mut cache,
    ))
    .await
    .unwrap();
    let later = Box::pin(request(
        entry(&fixture.concurrent_blob),
        &options,
        &fixture,
        &mut cache,
    ))
    .await;
    cache.git_runtime.shutdown().await;
    assert!(matches!(later, Err(CrabError::Protocol(message))
        if message == "requested object is outside the visible generation"));
    assert_eq!(
        run_git(
            target.path(),
            &["cat-file", "blob", &fixture.advertised_blob]
        ),
        b"advertised"
    );
    assert!(!git_object_exists(target.path(), &fixture.concurrent_blob));
}

#[tokio::test]
async fn empty_capability_probe_does_not_freeze_future_discovery() {
    let fixture = Box::pin(advancing_ref(false)).await;
    let layout = crab_storage::StoreLayout::with_global_prefix(
        fixture.store.as_storage().clone(),
        fixture.router.repo_prefix().to_owned(),
        fixture.router.global_prefix().to_owned(),
    );
    let root = crab_write::capsule_protocol::open_root(&layout)
        .await
        .unwrap();
    let (heads, _) =
        crab_metadata::capsule_protocol::capture_ref_heads(&layout, root.record().root())
            .await
            .unwrap();
    let mut saved = Vec::new();
    for head in heads {
        let path = layout.capsule_ref_head_path(
            &crab_metadata::capsule_protocol::capsule_ref_name_key(head.ref_name()),
        );
        let bytes = fixture.store.get_with_etag(&path).await.unwrap().0;
        fixture.store.delete(&path).await.unwrap();
        saved.push((path, bytes));
    }
    let target = tempfile::tempdir().unwrap();
    run_git(target.path(), &["init", "-q"]);
    let git_dir = target.path().join(".git");
    let _guard = GitEnvCwdGuard::set(target.path(), &git_dir, target.path());
    let mut cache = SessionCache::new(Default::default());
    let options = HelperOptions::default();
    Box::pin(request(Batch::Capabilities, &options, &fixture, &mut cache))
        .await
        .unwrap();
    for (path, bytes) in saved {
        fixture.store.put_overwrite(&path, bytes).await.unwrap();
    }
    let listed = Box::pin(request(
        Batch::List { for_push: false },
        &options,
        &fixture,
        &mut cache,
    ))
    .await
    .unwrap();
    cache.git_runtime.shutdown().await;
    assert!(String::from_utf8_lossy(&listed).contains(&fixture.advertised));
}
