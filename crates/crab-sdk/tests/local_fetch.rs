#![cfg(feature = "local")]

use std::error::Error as _;
use std::path::PathBuf;

use crab_sdk::local::{CloneOptions, FetchDepth, FetchOptions};
use crab_sdk::{ErrorKind, RepositoryLocator};

#[path = "local_support/direct.rs"]
mod direct_support;
mod local_support;
use direct_support::direct_remote;
use local_support::{client, commit, git_path, run};

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

#[tokio::test(flavor = "multi_thread")]
async fn clone_uses_explicit_tools_without_helper_path() {
    let scratch = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let commits = direct_remote(store.path(), "repository", 1).await;
    let sdk = client(store.path(), scratch.path());
    let locator = RepositoryLocator::new("repository").unwrap();
    let commit = &commits[0];
    let destination = scratch.path().join("checkout with spaces");

    let local_handle = sdk
        .clone_local(locator, &destination, CloneOptions::default())
        .await
        .unwrap_or_else(|error| panic!("{}", failure(&error)));
    let local = local_handle.local().unwrap();

    assert_eq!(
        run(&git_path(), &destination, &["rev-parse", "HEAD"]),
        *commit
    );
    run(&git_path(), &destination, &["fsck", "--no-dangling"]);
    assert_eq!(local.path(), destination.canonicalize().unwrap());
    sdk.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn clone_preserves_existing_destination() {
    let scratch = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    direct_remote(store.path(), "repository", 1).await;
    let sdk = client(store.path(), scratch.path());
    let locator = RepositoryLocator::new("repository").unwrap();
    let destination = scratch.path().join("existing");
    std::fs::create_dir(&destination).unwrap();
    std::fs::write(destination.join("marker"), b"unchanged").unwrap();

    let error = match sdk
        .clone_local(locator, &destination, CloneOptions::default())
        .await
    {
        Ok(_) => panic!("clone replaced an existing destination"),
        Err(error) => error,
    };

    assert_eq!(error.kind(), crab_sdk::ErrorKind::Conflict);
    assert_eq!(
        std::fs::read(destination.join("marker")).unwrap(),
        b"unchanged"
    );
    assert_eq!(std::fs::read_dir(&destination).unwrap().count(), 1);
    sdk.close().await.unwrap();
}

async fn unsupported_mutation_preserves_bytes(key: &str, value: &str, name: &str) {
    let scratch = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    direct_remote(store.path(), "repository", 1).await;
    let sdk = client(store.path(), scratch.path());
    let destination = scratch.path().join(name);
    let local_handle = sdk
        .clone_local(
            RepositoryLocator::new("repository").unwrap(),
            &destination,
            CloneOptions::default(),
        )
        .await
        .unwrap();
    let local = local_handle.local().unwrap();
    let git = git_path();
    run(&git, &destination, &["config", key, value]);
    std::fs::write(destination.join("retained.txt"), b"retained\n").unwrap();
    let before = run(&git, &destination, &["status", "--porcelain=v2"]);

    let error = local
        .stage(vec!["retained.txt".into()])
        .await
        .err()
        .unwrap();

    assert_eq!(error.kind(), ErrorKind::UnsupportedCapability);
    assert_eq!(
        std::fs::read(destination.join("retained.txt")).unwrap(),
        b"retained\n"
    );
    assert_eq!(
        run(&git, &destination, &["status", "--porcelain=v2"]),
        before
    );
    sdk.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn partial_clone_rejected_before_mutation() {
    unsupported_mutation_preserves_bytes(
        "remote.origin.promisor",
        "true",
        "partial-clone-checkout",
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn unsupported_worktree_mutation_preserves_bytes() {
    unsupported_mutation_preserves_bytes("core.sparseCheckout", "true", "sparse-checkout").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn configure_local_uses_exact_crab_path() {
    let scratch = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let repository = scratch.path().join("repository");
    std::fs::create_dir(&repository).unwrap();
    let git = git_path();
    run(&git, &repository, &["init", "-b", "main"]);
    std::fs::write(
        repository.join(".gitattributes"),
        b"*.bin filter=crab diff=crab merge=crab -text\n",
    )
    .unwrap();
    commit(&git, &repository, "file.txt", "main");
    let sdk = client(store.path(), scratch.path());
    let error = match sdk.open(crab_sdk::OpenOptions::local(&repository)).await {
        Ok(_) => panic!("repository unexpectedly opened before filter setup"),
        Err(error) => error,
    };
    assert_eq!(error.kind(), crab_sdk::ErrorKind::LocalSetupRequired);
    sdk.configure_local(&repository).await.unwrap();
    sdk.open(crab_sdk::OpenOptions::local(&repository))
        .await
        .unwrap();
    let process = run(
        &git,
        &repository,
        &["config", "--local", "--get", "filter.crab.process"],
    );
    assert!(process.contains("'"));
    assert!(process.ends_with(" filter-process"));
    sdk.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn linked_worktree_preserves_sibling() {
    let scratch = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let repository = scratch.path().join("repository with spaces");
    std::fs::create_dir(&repository).unwrap();
    let git = git_path();
    run(&git, &repository, &["init", "-b", "main"]);
    let main = commit(&git, &repository, "file.txt", "main");
    let sibling = scratch.path().join("linked sibling");
    run(
        &git,
        &repository,
        &[
            "worktree",
            "add",
            "-b",
            "sibling",
            sibling.to_str().unwrap(),
        ],
    );
    let sdk = client(store.path(), scratch.path());
    let local_handle = sdk
        .open(crab_sdk::OpenOptions::local(&sibling))
        .await
        .unwrap();
    let local = local_handle.local().unwrap();
    assert_eq!(local.path(), sibling.canonicalize().unwrap());
    assert_ne!(local.path(), repository.canonicalize().unwrap());
    assert_eq!(run(&git, &repository, &["rev-parse", "HEAD"]), main);
    assert!(local.common_directory().ends_with(".git"));
    let snapshot = local.snapshot("HEAD").await.unwrap();
    assert_eq!(snapshot.commit_id().to_string(), main);
    assert_eq!(snapshot.history(10).await.unwrap().len(), 1);
    sdk.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn fetch_recovers_each_metadata_boundary() {
    let scratch = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let git = git_path();
    let commits = direct_remote(store.path(), "repository", 4).await;
    let sdk = client(store.path(), scratch.path());
    let before_fetch_head = format!("{}\t\tbranch 'main' of old\n", commits[2]);
    let after_fetch_head = format!("{}\t\tbranch 'main' of crab-sdk:repository\n", commits[3]);
    for boundary in 0..6 {
        let checkout = scratch.path().join(format!("checkout-{boundary}"));
        let local_handle = sdk
            .clone_local(
                RepositoryLocator::new("repository").unwrap(),
                &checkout,
                CloneOptions::default(),
            )
            .await
            .unwrap_or_else(|error| panic!("{}", failure(&error)));
        let git_dir = PathBuf::from(run(
            &git,
            &checkout,
            &["rev-parse", "--path-format=absolute", "--git-dir"],
        ));
        run(
            &git,
            &checkout,
            &["update-ref", "refs/remotes/origin/main", &commits[2]],
        );
        std::fs::write(git_dir.join("shallow"), format!("{}\n", commits[2])).unwrap();
        std::fs::write(git_dir.join("FETCH_HEAD"), &before_fetch_head).unwrap();
        let intent = serde_json::json!({
            "version": 1,
            "refs": {
                "refs/remotes/origin/main": {
                    "before": commits[2],
                    "after": commits[3]
                }
            },
            "remote_head_name": "refs/remotes/origin/HEAD",
            "remote_head": {
                "before": "refs/remotes/origin/main",
                "after": "refs/remotes/origin/main"
            },
            "shallow": {
                "before": format!("{}\n", commits[2]),
                "after": format!("{}\n", commits[1])
            },
            "fetch_head": {
                "before": before_fetch_head,
                "after": after_fetch_head
            }
        });
        let intent_path = git_dir.join("crab-sdk-fetch-intent-v1");
        let intent_bytes = serde_json::to_vec(&intent).unwrap();
        std::fs::write(&intent_path, &intent_bytes).unwrap();
        if boundary >= 1 {
            run(
                &git,
                &checkout,
                &["update-ref", "refs/remotes/origin/main", &commits[3]],
            );
        }
        if boundary == 2 {
            std::fs::write(git_dir.join("shallow.lock"), format!("{}\n", commits[1])).unwrap();
        }
        if boundary >= 3 {
            std::fs::write(git_dir.join("shallow"), format!("{}\n", commits[1])).unwrap();
        }
        if boundary == 4 {
            std::fs::write(git_dir.join("FETCH_HEAD.lock"), &after_fetch_head).unwrap();
        }
        if boundary >= 5 {
            std::fs::write(git_dir.join("FETCH_HEAD"), &after_fetch_head).unwrap();
        }
        drop(local_handle);
        let local_handle = sdk
            .open(crab_sdk::OpenOptions::local(&checkout))
            .await
            .unwrap_or_else(|error| panic!("boundary {boundary}: {}", failure(&error)));
        let local = local_handle.local().unwrap();
        assert_eq!(std::fs::read(&intent_path).unwrap(), intent_bytes);
        local
            .fetch(FetchOptions::default())
            .await
            .unwrap_or_else(|error| panic!("boundary {boundary}: {}", failure(&error)));
        assert_eq!(
            run(&git, &checkout, &["rev-parse", "refs/remotes/origin/main"]),
            commits[3]
        );
        assert_eq!(
            std::fs::read_to_string(git_dir.join("shallow")).unwrap(),
            format!("{}\n", commits[1])
        );
        assert_eq!(
            std::fs::read_to_string(git_dir.join("FETCH_HEAD")).unwrap(),
            after_fetch_head
        );
        assert!(!intent_path.exists());
    }
    sdk.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn open_rejects_missing_fetch_object_without_mutation() {
    let scratch = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let git = git_path();
    let commits = direct_remote(store.path(), "repository", 1).await;
    let sdk = client(store.path(), scratch.path());
    let checkout = scratch.path().join("checkout");
    let local_handle = sdk
        .clone_local(
            RepositoryLocator::new("repository").unwrap(),
            &checkout,
            CloneOptions::default(),
        )
        .await
        .unwrap();
    let git_dir = PathBuf::from(run(
        &git,
        &checkout,
        &["rev-parse", "--path-format=absolute", "--git-dir"],
    ));
    let missing = "1111111111111111111111111111111111111111";
    let expected = commits[0].clone();
    let intent = serde_json::to_vec(&serde_json::json!({
        "version": 1,
        "refs": {
            "refs/remotes/origin/main": {
                "before": expected.clone(),
                "after": missing,
            }
        },
        "remote_head_name": "refs/remotes/origin/HEAD",
        "remote_head": {
            "before": "refs/remotes/origin/main",
            "after": "refs/remotes/origin/main",
        },
        "shallow": null,
        "fetch_head": {
            "before": null,
            "after": null,
        },
    }))
    .unwrap();
    let intent_path = git_dir.join("crab-sdk-fetch-intent-v1");
    std::fs::write(&intent_path, &intent).unwrap();
    drop(local_handle);

    let error = match sdk.open(crab_sdk::OpenOptions::local(&checkout)).await {
        Ok(_) => panic!("repository opened with a missing recovery object"),
        Err(error) => error,
    };

    assert_eq!(error.kind(), ErrorKind::Corruption);
    assert_eq!(std::fs::read(&intent_path).unwrap(), intent);
    assert_eq!(
        run(&git, &checkout, &["rev-parse", "refs/remotes/origin/main"]),
        expected
    );
    sdk.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn shallow_deepen_unshallow_round_trip() {
    let scratch = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let git = git_path();
    direct_remote(store.path(), "repository", 4).await;
    let checkout = scratch.path().join("checkout");
    let sdk = client(store.path(), scratch.path());
    let local_handle = sdk
        .clone_local(
            RepositoryLocator::new("repository").unwrap(),
            &checkout,
            CloneOptions::default()
                .with_branch("main")
                .unwrap()
                .with_depth(1)
                .unwrap(),
        )
        .await
        .unwrap_or_else(|error| panic!("{}", failure(&error)));
    let local = local_handle.local().unwrap();
    assert_eq!(run(&git, &checkout, &["rev-list", "--count", "HEAD"]), "1");
    let deepened = local
        .fetch(
            FetchOptions::default()
                .with_depth(FetchDepth::Deepen(2))
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(deepened.is_shallow());
    assert_eq!(run(&git, &checkout, &["rev-list", "--count", "HEAD"]), "3");
    let complete = local
        .fetch(
            FetchOptions::default()
                .tags(true)
                .with_depth(FetchDepth::Unshallow)
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(!complete.is_shallow());
    assert_eq!(run(&git, &checkout, &["rev-list", "--count", "HEAD"]), "4");
    assert!(!run(&git, &checkout, &["tag", "--list", "retained"]).is_empty());

    run(
        &git,
        &checkout,
        &["update-ref", "refs/remotes/origin/obsolete", "HEAD"],
    );
    run(
        &git,
        &checkout,
        &["update-ref", "refs/tags/obsolete", "HEAD"],
    );
    local
        .fetch(FetchOptions::default().tags(true).prune(true))
        .await
        .unwrap_or_else(|error| panic!("{}", failure(&error)));
    assert!(run(&git, &checkout, &["tag", "--list", "obsolete"]).is_empty());
    assert!(
        std::process::Command::new(&git)
            .current_dir(&checkout)
            .args(["show-ref", "--verify", "refs/remotes/origin/obsolete"])
            .status()
            .unwrap()
            .code()
            .is_some_and(|code| code != 0)
    );
    sdk.close().await.unwrap();
}
