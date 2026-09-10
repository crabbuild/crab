#![cfg(feature = "remote")]

use std::sync::Arc;

use crab_sdk::storage::DirectStoreOptions;
use crab_sdk::{Client, ErrorKind, RepositoryLocator, RepositoryMode, Revision};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn public_client_opens_empty_repository_and_closes_admission_for_all_handles() {
    let directory = tempfile::tempdir().unwrap();
    let store = crab_storage::Store::new(Arc::new(
        object_store::local::LocalFileSystem::new_with_prefix(directory.path()).unwrap(),
    ));
    let layout = crab_storage::StoreLayout::new(store.clone(), "repository".to_owned());
    let manifest = crab_metadata::manifests::Manifest::default_for_repo("refs/heads/main");
    crab_metadata::manifest_store::create_manifest(&store, &layout, &manifest)
        .await
        .unwrap();
    let client = Client::builder()
        .direct_store(DirectStoreOptions::filesystem(directory.path()).unwrap())
        .build()
        .unwrap();
    let cancellation = crab_sdk::operation::Cancellation::default();
    cancellation.cancel();
    let controls = crab_sdk::operation::Options::default().with_cancellation(cancellation);
    assert_eq!(
        client
            .open(crab_sdk::OpenOptions::remote(
                RepositoryLocator::new("repository").unwrap()
            ))
            .with_options(controls)
            .await
            .err()
            .unwrap()
            .kind(),
        ErrorKind::Cancelled
    );
    let repository = client
        .open(crab_sdk::OpenOptions::remote(
            RepositoryLocator::new("repository").unwrap(),
        ))
        .await
        .unwrap();
    assert_eq!(repository.mode(), RepositoryMode::Remote);
    #[cfg(feature = "local")]
    assert_eq!(
        repository.local().err().unwrap().kind(),
        ErrorKind::UnsupportedCapability
    );
    let remote = repository.remote().unwrap();
    let mut expected_capabilities = vec![crab_sdk::remote::Capability::ReadGit];
    if cfg!(feature = "content") {
        expected_capabilities.push(crab_sdk::remote::Capability::ReadContent);
    }
    assert_eq!(remote.capabilities(), expected_capabilities);
    let failure = remote
        .snapshot(Revision::branch("main").unwrap())
        .await
        .err()
        .unwrap();
    assert_eq!(failure.kind(), ErrorKind::NotFound);
    let limited = crab_sdk::operation::Options::default()
        .with_limits(crab_sdk::operation::ReadLimits {
            max_response_bytes: 1,
            ..Default::default()
        })
        .unwrap();
    assert_eq!(
        remote
            .refs()
            .with_options(limited)
            .await
            .unwrap_err()
            .kind(),
        ErrorKind::LimitExceeded
    );
    let refs = remote.refs().await.unwrap();
    assert_eq!(
        (refs.head(), refs.entries().len()),
        (Some("refs/heads/main"), 0)
    );
    let expired = crab_sdk::operation::Options::default().with_deadline(std::time::Instant::now());
    assert_eq!(
        remote
            .refs()
            .with_options(expired.clone())
            .await
            .unwrap_err()
            .kind(),
        ErrorKind::Timeout
    );
    assert_eq!(
        remote
            .refresh()
            .with_options(expired.clone())
            .await
            .err()
            .unwrap()
            .kind(),
        ErrorKind::Timeout
    );
    assert_eq!(
        remote
            .snapshot(Revision::branch("main").unwrap())
            .with_options(expired)
            .await
            .err()
            .unwrap()
            .kind(),
        ErrorKind::Timeout
    );
    client.close().await.unwrap();
    assert_eq!(remote.capabilities(), expected_capabilities);
    let failure = remote.refresh().await.err().unwrap();
    assert_eq!(failure.kind(), ErrorKind::Cancelled);
    assert_eq!(
        remote.refs().await.unwrap_err().kind(),
        ErrorKind::Cancelled
    );
    client.close().await.unwrap();
}

#[test]
fn explicit_store_and_repository_paths_reject_ambiguous_inputs() {
    assert!(DirectStoreOptions::filesystem(std::path::Path::new("relative")).is_err());
    for prefix in [
        "", "/root", "../repo", "a//b", "a/./b", "a/../b", "a\\b", "a\0b",
    ] {
        assert!(RepositoryLocator::new(prefix).is_err(), "{prefix:?}");
    }
}

#[tokio::test]
async fn repository_open_charges_listing_and_manifest_reads() {
    let directory = tempfile::tempdir().unwrap();
    let store = crab_storage::Store::new(Arc::new(
        object_store::local::LocalFileSystem::new_with_prefix(directory.path()).unwrap(),
    ));
    let layout = crab_storage::StoreLayout::new(store.clone(), "repository".to_owned());
    crab_metadata::manifest_store::create_manifest(
        &store,
        &layout,
        &crab_metadata::manifests::Manifest::default_for_repo("refs/heads/main"),
    )
    .await
    .unwrap();
    for limits in [
        crab_sdk::operation::ReadLimits {
            max_storage_requests: 1,
            ..Default::default()
        },
        crab_sdk::operation::ReadLimits {
            max_fetched_bytes: 1,
            ..Default::default()
        },
    ] {
        let client = Client::builder()
            .direct_store(DirectStoreOptions::filesystem(directory.path()).unwrap())
            .build()
            .unwrap();
        let options = crab_sdk::operation::Options::default()
            .with_limits(limits)
            .unwrap();
        let result = client
            .open(crab_sdk::OpenOptions::remote(
                RepositoryLocator::new("repository").unwrap(),
            ))
            .with_options(options)
            .await;
        let repository = client
            .open(crab_sdk::OpenOptions::remote(
                RepositoryLocator::new("repository").unwrap(),
            ))
            .await
            .unwrap();
        let remote = repository.remote().unwrap();
        assert_eq!(remote.refs().await.unwrap().head(), Some("refs/heads/main"));
        client.close().await.unwrap();
        assert_eq!(result.err().unwrap().kind(), ErrorKind::LimitExceeded);
    }
}
