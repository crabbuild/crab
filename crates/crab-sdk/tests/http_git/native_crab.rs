use std::error::Error as _;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};

use crab_sdk::local::{
    CloneOptions, CommitOptions, FetchDepth, FetchOptions, Options, PullOptions, PushOptions,
    PushOutcome, PushRefspec, Tools,
};
use crab_sdk::remote::EntryMode;
use crab_sdk::remote::write::{
    CommitIdentity, CommitOptions as RemoteCommitOptions, FileEdit, MutationOutcome,
};
use crab_sdk::storage::DirectStoreOptions;
use crab_sdk::{
    Client, ErrorKind, GitPath, ObjectId, OpenOptions, RepositoryLocator, Revision, WritePolicy,
};
use futures_util::{FutureExt as _, TryStreamExt as _};
use object_store::ObjectStoreExt as _;
use sha2::Digest as _;

use super::local_support::{commit, git_path, run};

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires the native Crab HTTP server, real Git and compatible Crab"]
async fn round_trip() {
    let url = std::env::var("CRAB_SDK_TEST_HTTP_URL").expect("native HTTP fixture URL");
    let crab = PathBuf::from(
        std::env::var_os("CRAB_SDK_TEST_CRAB_BIN").expect("compatible Crab executable"),
    );
    let scratch = tempfile::tempdir().unwrap();
    let store = scratch.path().join("local-store");
    std::fs::create_dir(&store).unwrap();
    let git = git_path();
    let sdk = Client::builder()
        .direct_store(DirectStoreOptions::filesystem(&store).unwrap())
        .local(Options::new(Tools::new(&git, &crab).unwrap()))
        .build()
        .unwrap();
    let bucket = std::env::var("QUALIFICATION_BUCKET").unwrap();
    let content = Client::builder()
        .direct_store(DirectStoreOptions::s3_from_env(&bucket).unwrap())
        .local(Options::new(Tools::new(&git, &crab).unwrap()))
        .build()
        .unwrap();
    let result = AssertUnwindSafe(async {
        let base = exercise(&sdk, &git, scratch.path(), &url).await;
        hydrated_clone(&content, &git, scratch.path(), &url, base).await
    })
    .catch_unwind()
    .await;
    let close = sdk.close().await;
    let content_close = content.close().await;
    let commit = match result {
        Ok(commit) => commit,
        Err(panic) => std::panic::resume_unwind(panic),
    };
    close.unwrap();
    content_close.unwrap();
    println!("native-http-sdk-commit={commit}");
}

async fn exercise(sdk: &Client, git: &Path, scratch: &Path, url: &str) -> ObjectId {
    let seed = scratch.join("seed");
    std::fs::create_dir(&seed).unwrap();
    run(git, &seed, &["init", "--initial-branch=main"]);
    commit(git, &seed, "README.md", "native seed");
    run(git, &seed, &["push", url, "main:main"]);
    let locator = RepositoryLocator::http(url).unwrap();
    let checkout = scratch.join("checkout");
    let repository = sdk
        .clone_local(locator.clone(), &checkout, CloneOptions::default())
        .await
        .unwrap();
    let observer_path = scratch.join("observer");
    let observer = sdk
        .clone_local(locator.clone(), &observer_path, CloneOptions::default())
        .await
        .unwrap();
    let local = repository.local().unwrap();
    assert!(local.status().await.unwrap().is_clean());
    std::fs::write(checkout.join("README.md"), "native SDK update\n").unwrap();
    local.stage(vec!["README.md".into()]).await.unwrap();
    let identity =
        CommitIdentity::new("SDK qualification", "sdk@example.invalid", 1_700_000_000, 0).unwrap();
    let target = local
        .commit(
            CommitOptions::new(identity.clone(), identity, b"native SDK HTTP\n".to_vec()).unwrap(),
        )
        .await
        .unwrap();
    let pushed = local
        .prepare_push(PushOptions::current_branch())
        .await
        .unwrap()
        .execute()
        .await
        .unwrap();
    assert!(matches!(pushed, PushOutcome::TransportCommitted { .. }));
    assert!(local.status().await.unwrap().is_clean());

    let observer = observer.local().unwrap();
    observer.fetch(FetchOptions::default()).await.unwrap();
    observer
        .pull(PullOptions::fast_forward_only())
        .await
        .unwrap();
    assert_eq!(observer.status().await.unwrap().head(), Some(target));
    assert_eq!(
        std::fs::read(observer_path.join("README.md")).unwrap(),
        b"native SDK update\n"
    );

    let shallow_path = scratch.join("shallow");
    let shallow = sdk
        .clone_local(
            locator.clone(),
            &shallow_path,
            CloneOptions::default().with_depth(1).unwrap(),
        )
        .await
        .unwrap();
    assert!(shallow_path.join(".git/shallow").is_file());
    shallow
        .local()
        .unwrap()
        .fetch(
            FetchOptions::default()
                .with_depth(FetchDepth::Unshallow)
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(!shallow_path.join(".git/shallow").exists());

    let branches = ["refs/heads/sdk-http", "refs/tags/sdk-http"];
    let created = local
        .prepare_push(
            PushOptions::current_branch()
                .with_refspecs(
                    branches
                        .iter()
                        .map(|name| PushRefspec::update("refs/heads/main", name).unwrap())
                        .collect(),
                )
                .unwrap(),
        )
        .await
        .unwrap()
        .execute()
        .await
        .unwrap();
    assert!(matches!(created, PushOutcome::TransportCommitted { .. }));
    let refs = run(
        git,
        &checkout,
        &["ls-remote", url, branches[0], branches[1]],
    );
    assert_eq!(refs.lines().count(), 2);
    let deleted = local
        .prepare_push(
            PushOptions::current_branch()
                .with_refspecs(
                    branches
                        .iter()
                        .map(|name| PushRefspec::delete(name).unwrap())
                        .collect(),
                )
                .unwrap(),
        )
        .await
        .unwrap()
        .execute()
        .await
        .unwrap();
    assert!(matches!(deleted, PushOutcome::TransportCommitted { .. }));
    assert!(
        run(
            git,
            &checkout,
            &["ls-remote", url, branches[0], branches[1]]
        )
        .is_empty()
    );

    run(git, &checkout, &["checkout", "--orphan", "replacement"]);
    run(git, &checkout, &["rm", "-rf", "."]);
    commit(git, &checkout, "replacement.txt", "replacement");
    let force = local
        .prepare_push(
            PushOptions::current_branch()
                .with_refspecs(vec![
                    PushRefspec::update("refs/heads/replacement", "refs/heads/main").unwrap(),
                ])
                .unwrap()
                .with_policy(WritePolicy::ForceWithLease),
        )
        .await
        .unwrap()
        .execute()
        .await
        .unwrap_err();
    assert_eq!(force.kind(), ErrorKind::Conflict);

    let independent = scratch.join("independent");
    let fresh = sdk
        .clone_local(locator, &independent, CloneOptions::default().eager())
        .await
        .unwrap();
    assert_eq!(
        fresh.local().unwrap().status().await.unwrap().head(),
        Some(target)
    );
    assert_eq!(
        std::fs::read(independent.join("README.md")).unwrap(),
        b"native SDK update\n"
    );
    run(git, &independent, &["fsck", "--strict"]);
    target
}

async fn hydrated_clone(
    sdk: &Client,
    git: &Path,
    scratch: &Path,
    url: &str,
    base: ObjectId,
) -> ObjectId {
    let bucket = std::env::var("QUALIFICATION_BUCKET").unwrap();
    let prefix = std::env::var("QUALIFICATION_PREFIX").unwrap();
    let mut bytes = (0..256 * 1024)
        .map(|index: usize| (index.wrapping_mul(73) ^ (index >> 8)) as u8)
        .collect::<Vec<_>>();
    // Per-fixture bytes keep an earlier verified cache from hiding a missing xorb.
    bytes.extend_from_slice(prefix.as_bytes());
    let lfs_bytes = bytes.iter().map(|byte| byte ^ 0x5a).collect::<Vec<_>>();
    let oid = sha2::Sha256::digest(&lfs_bytes).into();
    let store =
        crab_storage::build_static_env_store(&bucket, crab_storage::StorageProviderKind::S3)
            .unwrap();
    let layout = crab_storage::StoreLayout::new(store.clone(), prefix.clone());
    let xorb_prefix = crab_storage::layout::global_content_prefix(layout.global_prefix(), "xorbs");
    let existing_xorbs = store
        .inner()
        .list(Some(&xorb_prefix))
        .map_ok(|object| object.location)
        .try_collect::<std::collections::HashSet<_>>()
        .await
        .unwrap();
    crab_lfs::LfsObjectStore::new(store.clone(), &prefix)
        .put(&oid, lfs_bytes.clone().into())
        .await
        .unwrap();
    let pointer = crab_git::LfsPointer {
        oid,
        size: bytes.len() as u64,
        extensions: Vec::new(),
    }
    .serialize();
    let attributes = b"*.bin filter=crab diff=crab merge=crab -text\n*.lfs filter=lfs diff=lfs merge=lfs -text\n".to_vec();
    let project =
        format!("version = 1\n[remote]\nurl = \"crab://{bucket}/{prefix}\"\n").into_bytes();
    let identity =
        CommitIdentity::new("SDK qualification", "sdk@example.invalid", 1_700_000_000, 0).unwrap();
    let prepared = sdk
        .open(OpenOptions::remote(
            RepositoryLocator::new(&prefix).unwrap(),
        ))
        .await
        .unwrap()
        .remote()
        .unwrap()
        .prepare_commit(
            RemoteCommitOptions::new(
                base,
                "refs/heads/main",
                Some(base),
                identity.clone(),
                identity,
                b"native HTTP content\n".to_vec(),
            )
            .unwrap(),
            vec![
                FileEdit::git(
                    GitPath::new(".gitattributes").unwrap(),
                    EntryMode::Regular,
                    attributes.len() as u64,
                    std::io::Cursor::new(attributes),
                )
                .unwrap(),
                FileEdit::git(
                    GitPath::new("crab.toml").unwrap(),
                    EntryMode::Regular,
                    project.len() as u64,
                    std::io::Cursor::new(project),
                )
                .unwrap(),
                FileEdit::hydrated(
                    GitPath::new("a.bin").unwrap(),
                    EntryMode::Regular,
                    bytes.len() as u64,
                    std::io::Cursor::new(bytes.clone()),
                )
                .unwrap(),
                FileEdit::git(
                    GitPath::new("b.lfs").unwrap(),
                    EntryMode::Regular,
                    pointer.len() as u64,
                    std::io::Cursor::new(pointer.clone()),
                )
                .unwrap(),
            ],
            scratch.to_owned(),
        )
        .await
        .unwrap();
    let target = prepared.commit_id().unwrap();
    assert!(matches!(
        prepared.execute().await.unwrap(),
        MutationOutcome::Committed { .. }
    ));
    let uploaded = store
        .inner()
        .list(Some(&xorb_prefix))
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    let mut removed = Vec::new();
    for object in uploaded {
        if existing_xorbs.contains(&object.location) {
            continue;
        }
        let body = store
            .inner()
            .get(&object.location)
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        store.delete(&object.location).await.unwrap();
        removed.push((object.location, body));
    }
    assert!(!removed.is_empty(), "fixture must publish a fresh xorb");
    let lazy_path = scratch.join("lazy-content-http");
    let lazy = sdk
        .clone_local(
            RepositoryLocator::http(url).unwrap(),
            &lazy_path,
            CloneOptions::default().lazy(),
        )
        .await;
    let failed_path = scratch.join("missing-content-http");
    let failed = sdk
        .clone_local(
            RepositoryLocator::http(url).unwrap(),
            &failed_path,
            CloneOptions::default().eager(),
        )
        .await;
    for (path, body) in removed {
        store.put(&path, body).await.unwrap();
    }
    let lazy = lazy.unwrap_or_else(|error| panic!("lazy native HTTP clone: {}", failure(&error)));
    let raw = sdk
        .open(OpenOptions::remote(
            RepositoryLocator::new(&prefix).unwrap(),
        ))
        .await
        .unwrap()
        .remote()
        .unwrap()
        .snapshot(Revision::branch("main").unwrap())
        .await
        .unwrap()
        .read_blob(GitPath::new("a.bin").unwrap())
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(lazy_path.join("a.bin")).unwrap(),
        raw.as_ref()
    );
    assert_eq!(std::fs::read(lazy_path.join("b.lfs")).unwrap(), pointer);
    assert!(lazy.local().unwrap().status().await.unwrap().is_clean());
    assert!(failed.is_err(), "missing content must fail an eager clone");
    assert!(
        !failed_path.exists(),
        "failed clone must not publish a destination"
    );
    let checkout = scratch.join("hydrated-http");
    let cloned = sdk
        .clone_local(
            RepositoryLocator::http(url).unwrap(),
            &checkout,
            CloneOptions::default().eager(),
        )
        .await
        .unwrap();
    assert!(
        std::fs::read(checkout.join("a.bin")).unwrap() == bytes,
        "eager HTTP clone must materialize Crab content"
    );
    assert!(
        std::fs::read(checkout.join("b.lfs")).unwrap() == lfs_bytes,
        "eager HTTP clone must materialize LFS content"
    );
    assert!(cloned.local().unwrap().status().await.unwrap().is_clean());
    run(git, &checkout, &["fsck", "--strict"]);
    let identity =
        CommitIdentity::new("SDK qualification", "sdk@example.invalid", 1_700_000_000, 0).unwrap();
    let committed = sdk
        .open(OpenOptions::remote(
            RepositoryLocator::new(&prefix).unwrap(),
        ))
        .await
        .unwrap()
        .remote()
        .unwrap()
        .prepare_commit(
            RemoteCommitOptions::new(
                target,
                "refs/heads/without-project-config",
                None,
                identity.clone(),
                identity,
                b"content without project configuration\n".to_vec(),
            )
            .unwrap(),
            vec![FileEdit::delete(GitPath::new("crab.toml").unwrap()).unwrap()],
            scratch.to_owned(),
        )
        .await
        .unwrap()
        .execute()
        .await
        .unwrap();
    assert!(matches!(committed, MutationOutcome::Committed { .. }));
    let checkout = scratch.join("hydrated-direct-without-config");
    let cloned = sdk
        .clone_local(
            RepositoryLocator::new(&prefix).unwrap(),
            &checkout,
            CloneOptions::default()
                .with_branch("without-project-config")
                .unwrap()
                .eager(),
        )
        .await
        .unwrap();
    assert!(std::fs::read(checkout.join("a.bin")).unwrap() == bytes);
    assert!(std::fs::read(checkout.join("b.lfs")).unwrap() == lfs_bytes);
    assert!(!checkout.join("crab.toml").exists());
    assert!(cloned.local().unwrap().status().await.unwrap().is_clean());
    let mut updated_lfs = lfs_bytes;
    updated_lfs.extend_from_slice(b"local LFS update\n");
    // Exercise multiple real multipart parts in preparation, upload and recovery.
    updated_lfs.resize(16 * 1024 * 1024, 0x6b);
    run(
        git,
        &checkout,
        &["config", "lfs.storage", "cache directory's payloads"],
    );
    std::fs::write(checkout.join("b.lfs"), &updated_lfs).unwrap();
    let local = cloned.local().unwrap();
    run(
        git,
        &checkout,
        &[
            "config",
            "lfs.extension.fixture.clean",
            "/usr/bin/touch extension-ran",
        ],
    );
    let rejected = local.stage(vec!["b.lfs".into()]).await.unwrap_err();
    assert_eq!(rejected.kind(), ErrorKind::UnsupportedCapability);
    assert!(!checkout.join("extension-ran").exists());
    run(
        git,
        &checkout,
        &["config", "--remove-section", "lfs.extension.fixture"],
    );
    local.stage(vec!["b.lfs".into()]).await.unwrap();
    let identity =
        CommitIdentity::new("SDK qualification", "sdk@example.invalid", 1_700_000_000, 0).unwrap();
    local
        .commit(
            CommitOptions::new(identity.clone(), identity, b"local LFS update\n".to_vec()).unwrap(),
        )
        .await
        .unwrap();
    let before = store
        .inner()
        .list(None)
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    let dry_run = local
        .prepare_push(PushOptions::current_branch().dry_run(true))
        .await
        .unwrap()
        .execute()
        .await
        .unwrap();
    assert!(matches!(dry_run, PushOutcome::DryRun { .. }));
    let after = store
        .inner()
        .list(None)
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    assert_eq!(before, after, "LFS dry-run must create no remote objects");
    let prepared = local
        .prepare_push(PushOptions::current_branch())
        .await
        .unwrap();
    let token = prepared.recovery_token().to_json().unwrap();
    drop(prepared);
    let updated_oid = sha2::Sha256::digest(&updated_lfs).into();
    let storage = checkout.join(".git/cache directory's payloads");
    std::fs::remove_file(crab_lfs::local::object_path(&storage, &updated_oid)).unwrap();
    assert!(matches!(
        sdk.resume_local_push(
            crab_sdk::local::PushRecoveryToken::from_json(&token).unwrap(),
            scratch.to_owned()
        )
        .await
        .unwrap(),
        PushOutcome::Committed { .. }
    ));
    let verified = scratch.join("local-lfs-independent");
    sdk.clone_local(
        RepositoryLocator::new(&prefix).unwrap(),
        &verified,
        CloneOptions::default()
            .with_branch("without-project-config")
            .unwrap()
            .eager(),
    )
    .await
    .unwrap();
    assert!(std::fs::read(verified.join("b.lfs")).unwrap() == updated_lfs);
    target
}

fn failure(error: &crab_sdk::Error) -> String {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(error) = source {
        message.push_str(": ");
        message.push_str(&error.to_string());
        source = error.source();
    }
    message
}
