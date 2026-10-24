#![cfg(feature = "remote")]

use std::sync::Arc;

use crab_metadata::capsule_protocol::{RepositoryRoot, RootRecord, create_root};
use crab_sdk::storage::DirectStoreOptions;
use crab_sdk::{Client, ErrorKind, OpenOptions, RepositoryLocator};
use crab_storage::{Store, StoreLayout};

async fn fixture() -> (tempfile::TempDir, Store, StoreLayout<Store>, Client) {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::new(Arc::new(
        object_store::local::LocalFileSystem::new_with_prefix(directory.path()).unwrap(),
    ));
    let layout = StoreLayout::new(store.clone(), "repository".to_owned());
    let client = Client::builder()
        .direct_store(DirectStoreOptions::filesystem(directory.path()).unwrap())
        .build()
        .unwrap();
    (directory, store, layout, client)
}

async fn native_root(layout: &StoreLayout<Store>) {
    let root = RepositoryRoot::initial(&"a".repeat(64), "refs/heads/native").unwrap();
    create_root(layout, RootRecord::encode(root).unwrap())
        .await
        .unwrap();
}

async fn legacy_manifest(store: &Store, layout: &StoreLayout<Store>) {
    crab_metadata::manifest_store::create_manifest(
        store,
        layout,
        &crab_metadata::manifests::Manifest::default_for_repo("refs/heads/legacy"),
    )
    .await
    .unwrap();
}

fn open() -> OpenOptions {
    OpenOptions::remote(RepositoryLocator::new("repository").unwrap())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_repository_opens_without_legacy_metadata() {
    let (_directory, _store, layout, client) = fixture().await;
    native_root(&layout).await;
    let repository = client.open(open()).await.unwrap();
    let references = repository.remote().unwrap().refs().await.unwrap();
    client.close().await.unwrap();
    assert_eq!(references.head(), Some("refs/heads/native"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_authority_supersedes_legacy_metadata() {
    let (_directory, store, layout, client) = fixture().await;
    legacy_manifest(&store, &layout).await;
    native_root(&layout).await;
    let repository = client.open(open()).await.unwrap();
    let references = repository.remote().unwrap().refs().await.unwrap();
    client.close().await.unwrap();
    assert_eq!(references.head(), Some("refs/heads/native"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn corrupt_native_authority_never_reads_legacy_metadata() {
    let (_directory, store, layout, client) = fixture().await;
    legacy_manifest(&store, &layout).await;
    store
        .put_overwrite(&layout.capsule_root_path(), b"corrupt root".to_vec().into())
        .await
        .unwrap();
    let result = client.open(open()).await;
    client.close().await.unwrap();
    assert_eq!(result.err().unwrap().kind(), ErrorKind::Corruption);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_only_repository_is_not_opened() {
    let (_directory, store, layout, client) = fixture().await;
    legacy_manifest(&store, &layout).await;
    let result = client.open(open()).await;
    client.close().await.unwrap();
    assert_eq!(result.err().unwrap().kind(), ErrorKind::NotFound);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_capture_obeys_request_and_byte_limits() {
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
        let (_directory, _store, layout, client) = fixture().await;
        native_root(&layout).await;
        let controls = crab_sdk::operation::Options::default()
            .with_limits(limits)
            .unwrap();
        let result = client.open(open()).with_options(controls).await;
        client.close().await.unwrap();
        assert_eq!(
            result.err().unwrap().kind(),
            ErrorKind::LimitExceeded,
            "{limits:?}"
        );
    }
}
