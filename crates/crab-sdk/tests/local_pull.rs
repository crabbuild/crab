#![cfg(feature = "local")]

use crab_sdk::local::{
    CloneOptions, HydrationState, IntegrationId, IntegrationKind, PullOptions, PullOutcome,
};
use crab_sdk::{ErrorKind, RepositoryLocator};

#[path = "local_support/direct.rs"]
mod direct_support;
mod local_support;
use direct_support::direct_remote;
use local_support::{client, commit, git_path, run};

#[test]
fn persisted_integration_ids_require_canonical_sdk_uuid() {
    for value in [
        "",
        "550e8400-e29b-41d4-a716-446655440000",
        "01890F3E-7B8A-7ABC-8DEF-0123456789AB",
    ] {
        assert_eq!(
            IntegrationId::from_string(value).unwrap_err().kind(),
            ErrorKind::InvalidInput,
            "accepted {value:?}",
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn direct_pull_reuses_sdk_fetch_transport() {
    let scratch = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let commits = direct_remote(store.path(), "repository", 2).await;
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
    let local = local_handle.local().unwrap();
    let git = git_path();
    run(&git, &checkout, &["reset", "--hard", &commits[0]]);
    run(
        &git,
        &checkout,
        &["update-ref", "refs/remotes/origin/main", &commits[0]],
    );

    let outcome = local
        .pull(PullOptions::fast_forward_only().hydrate(false))
        .await
        .unwrap();

    assert!(matches!(outcome, PullOutcome::Updated { .. }));
    assert_eq!(run(&git, &checkout, &["rev-parse", "HEAD"]), commits[1]);
    sdk.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn open_preserves_stale_integration_until_the_next_pull() {
    let fixture = fixture();
    let git_dir = std::path::PathBuf::from(run(
        &fixture.git,
        &fixture.checkout,
        &["rev-parse", "--path-format=absolute", "--git-dir"],
    ));
    let head = run(&fixture.git, &fixture.checkout, &["rev-parse", "HEAD"]);
    let intent = serde_json::json!({
        "version": 1,
        "id": "01890f3e-7b8a-7abc-8def-0123456789ab",
        "kind": "merge",
        "fetched": head,
        "tracked_status_hash": null,
    });
    let intent = serde_json::to_vec(&intent).unwrap();
    let intent_path = git_dir.join("crab-sdk-integration-v1.json");
    std::fs::write(&intent_path, &intent).unwrap();
    let sdk = client(fixture.store.path(), fixture.scratch.path());

    let local_handle = sdk
        .open(crab_sdk::OpenOptions::local(&fixture.checkout))
        .await
        .unwrap();
    let local = local_handle.local().unwrap();

    assert_eq!(std::fs::read(&intent_path).unwrap(), intent);
    assert!(matches!(
        local
            .pull(PullOptions::fast_forward_only().hydrate(false))
            .await
            .unwrap(),
        PullOutcome::UpToDate { .. }
    ));
    assert!(!intent_path.exists());
    sdk.close().await.unwrap();
}

struct Fixture {
    scratch: tempfile::TempDir,
    store: tempfile::TempDir,
    source: std::path::PathBuf,
    checkout: std::path::PathBuf,
    git: std::path::PathBuf,
}

fn fixture() -> Fixture {
    let scratch = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let git = git_path();
    let remote = scratch.path().join("remote.git");
    run(
        &git,
        scratch.path(),
        &["init", "--bare", remote.to_str().unwrap()],
    );
    let source = scratch.path().join("source");
    run(
        &git,
        scratch.path(),
        &["clone", remote.to_str().unwrap(), source.to_str().unwrap()],
    );
    run(&git, &source, &["switch", "-c", "main"]);
    commit(&git, &source, "shared.txt", "initial");
    run(&git, &source, &["push", "-u", "origin", "main"]);
    let checkout = scratch.path().join("checkout");
    run(
        &git,
        scratch.path(),
        &[
            "clone",
            "--branch",
            "main",
            remote.to_str().unwrap(),
            checkout.to_str().unwrap(),
        ],
    );
    run(&git, &checkout, &["config", "user.name", "SDK pull test"]);
    run(
        &git,
        &checkout,
        &["config", "user.email", "sdk@example.invalid"],
    );
    Fixture {
        scratch,
        store,
        source,
        checkout,
        git,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn ff_only_preserves_diverged_head() {
    let fixture = self::fixture();
    let before = commit(&fixture.git, &fixture.checkout, "local.txt", "local");
    commit(&fixture.git, &fixture.source, "remote.txt", "remote");
    run(&fixture.git, &fixture.source, &["push", "origin", "main"]);
    let sdk = client(fixture.store.path(), fixture.scratch.path());
    let local_handle = sdk
        .open(crab_sdk::OpenOptions::local(&fixture.checkout))
        .await
        .unwrap();
    let local = local_handle.local().unwrap();
    let error = local
        .pull(PullOptions::fast_forward_only())
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Conflict);
    assert_eq!(
        run(&fixture.git, &fixture.checkout, &["rev-parse", "HEAD"]),
        before
    );
    sdk.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn merge_and_rebase_resume_conflicts() {
    let fixture = fixture();
    commit(&fixture.git, &fixture.checkout, "shared.txt", "local");
    commit(&fixture.git, &fixture.source, "shared.txt", "remote");
    run(&fixture.git, &fixture.source, &["push", "origin", "main"]);
    let sdk = client(fixture.store.path(), fixture.scratch.path());
    let local_handle = sdk
        .open(crab_sdk::OpenOptions::local(&fixture.checkout))
        .await
        .unwrap();
    let local = local_handle.local().unwrap();
    let PullOutcome::Conflict(conflict) = local
        .pull(PullOptions::merge().hydrate(false))
        .await
        .unwrap()
    else {
        panic!("divergent content did not conflict")
    };
    assert_eq!(conflict.kind(), IntegrationKind::Merge);
    assert_eq!(conflict.paths(), &[std::path::PathBuf::from("shared.txt")]);
    let persisted_integration = conflict.id().as_str().to_owned();
    let git_dir = std::path::PathBuf::from(run(
        &fixture.git,
        &fixture.checkout,
        &["rev-parse", "--path-format=absolute", "--git-dir"],
    ));
    let intent_path = git_dir.join("crab-sdk-integration-v1.json");
    let intent = std::fs::read(&intent_path).unwrap();
    assert_eq!(
        local
            .pull(PullOptions::merge().hydrate(false))
            .await
            .unwrap_err()
            .kind(),
        ErrorKind::Conflict
    );
    assert_eq!(std::fs::read(&intent_path).unwrap(), intent);
    drop(local_handle);
    sdk.close().await.unwrap();
    let sdk = client(fixture.store.path(), fixture.scratch.path());
    let local_handle = sdk
        .open(crab_sdk::OpenOptions::local(&fixture.checkout))
        .await
        .unwrap();
    let local = local_handle.local().unwrap();
    std::fs::write(fixture.checkout.join("shared.txt"), b"resolved\n").unwrap();
    local.stage(vec!["shared.txt".into()]).await.unwrap();
    let result = local
        .continue_integration(IntegrationId::from_string(&persisted_integration).unwrap())
        .await
        .unwrap();
    assert!(matches!(result, PullOutcome::Updated { .. }));
    assert_eq!(
        run(
            &fixture.git,
            &fixture.checkout,
            &["rev-list", "--parents", "-n", "1", "HEAD"]
        )
        .split_whitespace()
        .count(),
        3
    );
    sdk.close().await.unwrap();

    let fixture = self::fixture();
    commit(
        &fixture.git,
        &fixture.checkout,
        "shared.txt",
        "local rebase",
    );
    commit(&fixture.git, &fixture.source, "shared.txt", "remote rebase");
    run(&fixture.git, &fixture.source, &["push", "origin", "main"]);
    let sdk = client(fixture.store.path(), fixture.scratch.path());
    let local_handle = sdk
        .open(crab_sdk::OpenOptions::local(&fixture.checkout))
        .await
        .unwrap();
    let local = local_handle.local().unwrap();
    let PullOutcome::Conflict(conflict) = local
        .pull(PullOptions::rebase().hydrate(false))
        .await
        .unwrap()
    else {
        panic!("divergent rebase did not conflict")
    };
    assert_eq!(conflict.kind(), IntegrationKind::Rebase);
    let integration = conflict.id().clone();
    std::fs::write(fixture.checkout.join("shared.txt"), b"resolved rebase\n").unwrap();
    local.stage(vec!["shared.txt".into()]).await.unwrap();
    assert!(matches!(
        local.continue_integration(integration).await.unwrap(),
        PullOutcome::Updated { .. }
    ));
    assert_eq!(
        run(
            &fixture.git,
            &fixture.checkout,
            &["rev-list", "--parents", "-n", "1", "HEAD"]
        )
        .split_whitespace()
        .count(),
        2
    );
    sdk.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn abort_preserves_later_user_edits() {
    let fixture = fixture();
    commit(&fixture.git, &fixture.checkout, "shared.txt", "local");
    commit(&fixture.git, &fixture.source, "shared.txt", "remote");
    run(&fixture.git, &fixture.source, &["push", "origin", "main"]);
    let before = run(&fixture.git, &fixture.checkout, &["rev-parse", "HEAD"]);
    let sdk = client(fixture.store.path(), fixture.scratch.path());
    let local_handle = sdk
        .open(crab_sdk::OpenOptions::local(&fixture.checkout))
        .await
        .unwrap();
    let local = local_handle.local().unwrap();
    let PullOutcome::Conflict(conflict) = local
        .pull(PullOptions::rebase().hydrate(false))
        .await
        .unwrap()
    else {
        panic!("rebase did not conflict")
    };
    let integration = conflict.id().clone();
    std::fs::write(fixture.checkout.join("later.txt"), b"later user edit\n").unwrap();
    local.abort_integration(integration).await.unwrap();
    assert_eq!(
        run(&fixture.git, &fixture.checkout, &["rev-parse", "HEAD"]),
        before
    );
    assert_eq!(
        std::fs::read(fixture.checkout.join("later.txt")).unwrap(),
        b"later user edit\n"
    );
    sdk.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn abort_rejects_later_tracked_edits_that_git_would_overwrite() {
    let fixture = fixture();
    commit(&fixture.git, &fixture.checkout, "shared.txt", "local");
    commit(&fixture.git, &fixture.source, "shared.txt", "remote");
    run(&fixture.git, &fixture.source, &["push", "origin", "main"]);
    let sdk = client(fixture.store.path(), fixture.scratch.path());
    let local_handle = sdk
        .open(crab_sdk::OpenOptions::local(&fixture.checkout))
        .await
        .unwrap();
    let local = local_handle.local().unwrap();
    let PullOutcome::Conflict(conflict) = local
        .pull(PullOptions::rebase().hydrate(false))
        .await
        .unwrap()
    else {
        panic!("rebase did not conflict")
    };
    let integration = conflict.id().clone();
    std::fs::write(fixture.checkout.join("shared.txt"), b"later tracked edit\n").unwrap();
    let error = local.abort_integration(integration).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Conflict);
    assert_eq!(
        std::fs::read(fixture.checkout.join("shared.txt")).unwrap(),
        b"later tracked edit\n"
    );
    sdk.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn hydration_failure_reports_new_head() {
    let fixture = fixture();
    let remote = commit(&fixture.git, &fixture.source, "remote.txt", "remote");
    run(&fixture.git, &fixture.source, &["push", "origin", "main"]);
    std::fs::write(
        fixture.checkout.join("crab.toml"),
        b"version = 1\n[remote]\nurl = \"crab://bucket/repository\"\n",
    )
    .unwrap();
    let sdk = client(fixture.store.path(), fixture.scratch.path());
    let local_handle = sdk
        .open(crab_sdk::OpenOptions::local(&fixture.checkout))
        .await
        .unwrap();
    let local = local_handle.local().unwrap();
    let result = local.pull(PullOptions::fast_forward_only()).await.unwrap();
    assert!(
        matches!(
            result,
            PullOutcome::Updated {
                head,
                hydration: HydrationState::Failed(_),
            } if head.to_string() == remote
        ),
        "unexpected pull outcome: {result:?}"
    );
    sdk.close().await.unwrap();
}
