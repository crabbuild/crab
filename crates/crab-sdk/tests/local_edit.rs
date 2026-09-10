#![cfg(feature = "local")]

use crab_sdk::local::{
    CheckoutOptions, CommitOptions as LocalCommitOptions, ExecutionPolicy as LocalExecutionPolicy,
    PushOptions, PushOutcome as LocalPushOutcome,
};
use crab_sdk::remote::write::CommitIdentity;
use crab_sdk::{ErrorKind, RepositoryLocator, RepositoryMode};

mod local_support;
use local_support::{client, client_with_policy, commit, git_path, run};

fn install_failing_checkout_hook(repository: &std::path::Path) {
    let hook = repository.join(".git/hooks/post-checkout");
    std::fs::write(&hook, b"#!/bin/sh\nprintf ran > sdk-hook-ran\nexit 41\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn dry_run_creates_no_remote_objects() {
    let scratch = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let sdk = client(store.path(), scratch.path());
    let locator = RepositoryLocator::new("repository").unwrap();
    sdk.initialize_remote(locator, "refs/heads/main")
        .await
        .unwrap();
    let checkout = scratch.path().join("checkout");
    std::fs::create_dir(&checkout).unwrap();
    let git = git_path();
    run(&git, &checkout, &["init", "--initial-branch=main"]);
    run(
        &git,
        &checkout,
        &["remote", "add", "origin", "crab-sdk:repository"],
    );
    commit(&git, &checkout, "next.txt", "next");
    let repository = sdk
        .open(crab_sdk::OpenOptions::local(&checkout))
        .await
        .unwrap();
    assert_eq!(repository.mode(), RepositoryMode::Local);
    let interface_inventory = directory_inventory(store.path());
    assert_eq!(
        repository.remote().err().unwrap().kind(),
        ErrorKind::UnsupportedCapability
    );
    assert_eq!(directory_inventory(store.path()), interface_inventory);
    let local = repository.local().unwrap();
    let before = directory_inventory(store.path());

    let prepared = local
        .prepare_push(
            PushOptions::default()
                .with_destination("refs/heads/main")
                .unwrap()
                .dry_run(true),
        )
        .await
        .unwrap();
    let outcome = prepared.execute().await.unwrap();

    assert!(matches!(outcome, LocalPushOutcome::DryRun { .. }));
    assert_eq!(directory_inventory(store.path()), before);
    sdk.close().await.unwrap();
}

fn directory_inventory(root: &std::path::Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
    fn visit(
        root: &std::path::Path,
        directory: &std::path::Path,
        files: &mut Vec<(std::path::PathBuf, Vec<u8>)>,
    ) {
        let mut entries = std::fs::read_dir(directory)
            .unwrap()
            .collect::<std::io::Result<Vec<_>>>()
            .unwrap();
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            if entry.file_type().unwrap().is_dir() {
                visit(root, &entry.path(), files);
            } else {
                files.push((
                    entry.path().strip_prefix(root).unwrap().to_owned(),
                    std::fs::read(entry.path()).unwrap(),
                ));
            }
        }
    }
    let mut files = Vec::new();
    visit(root, root, &mut files);
    files
}

#[tokio::test(flavor = "multi_thread")]
async fn hydrated_status_is_clean() {
    let scratch = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let repository = scratch.path().join("repository");
    std::fs::create_dir(&repository).unwrap();
    let git = git_path();
    run(&git, &repository, &["init", "-b", "main"]);
    commit(&git, &repository, "file.txt", "initial");
    let sdk = client(store.path(), scratch.path());
    let repository_handle = sdk
        .open(crab_sdk::OpenOptions::local(&repository))
        .await
        .unwrap();
    let local = repository_handle.local().unwrap();
    assert!(local.status().await.unwrap().is_clean());
    sdk.close().await.unwrap();
}

#[cfg(unix)]
#[tokio::test(flavor = "multi_thread")]
async fn prefetch_passes_exact_paths_as_include_patterns() {
    let scratch = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let repository = scratch.path().join("repository");
    std::fs::create_dir(&repository).unwrap();
    let git = git_path();
    run(&git, &repository, &["init", "-b", "main"]);
    commit(&git, &repository, "file.txt", "initial");
    std::fs::write(scratch.path().join("crab fixture.enable-fetch"), b"").unwrap();
    let sdk = client(store.path(), scratch.path());
    let repository_handle = sdk
        .open(crab_sdk::OpenOptions::local(&repository))
        .await
        .unwrap();

    repository_handle
        .local()
        .unwrap()
        .prefetch_content(vec!["models/a.bin".into(), "space name.bin".into()])
        .await
        .unwrap();

    assert_eq!(
        std::fs::read_to_string(scratch.path().join("crab fixture.fetch")).unwrap(),
        "--json\n--include\nmodels/a.bin\n--include\nspace name.bin\n"
    );
    sdk.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn stage_commit_push_round_trip() {
    let scratch = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let repository = scratch.path().join("repository");
    std::fs::create_dir(&repository).unwrap();
    let git = git_path();
    run(&git, &repository, &["init", "-b", "main"]);
    commit(&git, &repository, "file.txt", "initial");
    std::fs::write(repository.join("file.txt"), b"changed\n").unwrap();
    std::fs::write(repository.join("new file.txt"), b"new\n").unwrap();

    let sdk = client(store.path(), scratch.path());
    let repository_handle = sdk
        .open(crab_sdk::OpenOptions::local(&repository))
        .await
        .unwrap();
    let local = repository_handle.local().unwrap();
    let changed = local.status().await.unwrap();
    assert_eq!(changed.entries().len(), 2);
    assert!(changed.entries().iter().any(|entry| entry.is_untracked()));
    local
        .stage(vec!["file.txt".into(), "new file.txt".into()])
        .await
        .unwrap();
    assert!(
        local
            .status()
            .await
            .unwrap()
            .entries()
            .iter()
            .all(|entry| entry.is_staged())
    );
    let author =
        CommitIdentity::new("SDK author", "sdk@example.invalid", 1_700_000_000, -420).unwrap();
    let oid = local
        .commit(LocalCommitOptions::new(author.clone(), author, b"SDK commit\n".to_vec()).unwrap())
        .await
        .unwrap();
    assert_eq!(
        run(&git, &repository, &["rev-parse", "HEAD"]),
        oid.to_string()
    );
    assert!(local.status().await.unwrap().is_clean());
    sdk.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn dirty_checkout_preserves_bytes() {
    let scratch = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let repository = scratch.path().join("repository");
    std::fs::create_dir(&repository).unwrap();
    let git = git_path();
    run(&git, &repository, &["init", "-b", "main"]);
    commit(&git, &repository, "file.txt", "main");
    run(&git, &repository, &["checkout", "-b", "other"]);
    commit(&git, &repository, "file.txt", "other");
    run(&git, &repository, &["checkout", "main"]);
    std::fs::write(repository.join("file.txt"), b"user bytes\n").unwrap();

    let sdk = client(store.path(), scratch.path());
    let repository_handle = sdk
        .open(crab_sdk::OpenOptions::local(&repository))
        .await
        .unwrap();
    let local = repository_handle.local().unwrap();
    let error = local
        .checkout("other", CheckoutOptions::default())
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Conflict);
    assert_eq!(
        std::fs::read(repository.join("file.txt")).unwrap(),
        b"user bytes\n"
    );
    assert_eq!(
        run(&git, &repository, &["branch", "--show-current"]),
        "main"
    );
    sdk.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn checkout_requires_explicit_hook_trust_and_reports_hook_failure() {
    let scratch = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let repository = scratch.path().join("repository");
    std::fs::create_dir(&repository).unwrap();
    let git = git_path();
    run(&git, &repository, &["init", "-b", "main"]);
    commit(&git, &repository, "file.txt", "main");
    run(&git, &repository, &["checkout", "-b", "other"]);
    run(&git, &repository, &["checkout", "main"]);
    install_failing_checkout_hook(&repository);
    let sdk = client(store.path(), scratch.path());
    let repository_handle = sdk
        .open(crab_sdk::OpenOptions::local(&repository))
        .await
        .unwrap();
    let local = repository_handle.local().unwrap();

    local
        .checkout("other", CheckoutOptions::default())
        .await
        .unwrap();
    assert!(!repository.join("sdk-hook-ran").exists());
    sdk.close().await.unwrap();
    let trusted_sdk =
        client_with_policy(store.path(), scratch.path(), LocalExecutionPolicy::Trusted);
    let trusted_handle = trusted_sdk
        .open(crab_sdk::OpenOptions::local(&repository))
        .await
        .unwrap();
    let trusted = trusted_handle.local().unwrap();
    let error = trusted
        .checkout("main", CheckoutOptions::default())
        .await
        .unwrap_err();

    assert_eq!(error.kind(), ErrorKind::Conflict);
    assert_eq!(
        std::fs::read(repository.join("sdk-hook-ran")).unwrap(),
        b"ran"
    );
    assert_eq!(
        run(&git, &repository, &["branch", "--show-current"]),
        "main"
    );
    trusted_sdk.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn stage_disables_unrecognized_executable_filters_without_changing_bytes() {
    let scratch = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let repository = scratch.path().join("repository");
    std::fs::create_dir(&repository).unwrap();
    let git = git_path();
    run(&git, &repository, &["init", "-b", "main"]);
    commit(&git, &repository, "README.md", "initial");
    std::fs::write(
        repository.join(".gitattributes"),
        "*.txt filter=untrusted\n",
    )
    .unwrap();
    run(
        &git,
        &repository,
        &[
            "config",
            "filter.untrusted.clean",
            "printf executed > driver-ran; while IFS= read -r line; do printf '%s\\n' \"$line\"; done",
        ],
    );
    std::fs::write(repository.join("payload.txt"), b"original bytes\n").unwrap();
    let sdk = client(store.path(), scratch.path());
    let repository_handle = sdk
        .open(crab_sdk::OpenOptions::local(&repository))
        .await
        .unwrap();
    let local = repository_handle.local().unwrap();

    local.stage(vec!["payload.txt".into()]).await.unwrap();

    assert!(!repository.join("driver-ran").exists());
    assert_eq!(
        run(&git, &repository, &["show", ":payload.txt"]),
        "original bytes"
    );
    sdk.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn cli_and_sdk_share_staging() {
    // The native Crab path is covered by the RustFS local qualification. This
    // local Git fixture proves the SDK leaves a canonical index consumable by Git.
    let scratch = tempfile::tempdir().unwrap();
    let store = tempfile::tempdir().unwrap();
    let repository = scratch.path().join("repository");
    std::fs::create_dir(&repository).unwrap();
    let git = git_path();
    run(&git, &repository, &["init", "-b", "main"]);
    commit(&git, &repository, "file.txt", "initial");
    std::fs::write(repository.join("file.txt"), b"staged by SDK\n").unwrap();
    let sdk = client(store.path(), scratch.path());
    let repository_handle = sdk
        .open(crab_sdk::OpenOptions::local(&repository))
        .await
        .unwrap();
    let local = repository_handle.local().unwrap();
    local.stage(vec!["file.txt".into()]).await.unwrap();
    assert_eq!(
        run(&git, &repository, &["diff", "--cached", "--name-only"]),
        "file.txt"
    );
    sdk.close().await.unwrap();
}
