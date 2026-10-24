use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};

use crab_sdk::local::{
    CloneOptions, CommitOptions, FetchDepth, FetchOptions, Options, PullOptions, PushOptions,
    PushOutcome, PushRefspec, Tools,
};
use crab_sdk::remote::write::CommitIdentity;
use crab_sdk::storage::DirectStoreOptions;
use crab_sdk::{Client, ErrorKind, ObjectId, RepositoryLocator, WritePolicy};
use futures_util::FutureExt as _;

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
        .local(Options::new(Tools::new(&git, crab).unwrap()))
        .build()
        .unwrap();
    let result = AssertUnwindSafe(exercise(&sdk, &git, scratch.path(), &url))
        .catch_unwind()
        .await;
    let close = sdk.close().await;
    let commit = match result {
        Ok(commit) => commit,
        Err(panic) => std::panic::resume_unwind(panic),
    };
    close.unwrap();
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
        .clone_local(locator, &independent, CloneOptions::default())
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
