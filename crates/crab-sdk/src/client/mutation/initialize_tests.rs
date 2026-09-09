use std::{sync::Arc, time::Duration};

use bytes::Bytes;

use super::{tests::memory_client, *};

#[tokio::test]
async fn filesystem_initialization_needs_only_conditional_create_support() {
    let root = tempfile::tempdir().unwrap();
    let client = Client::builder()
        .direct_store(crate::DirectStoreOptions::filesystem(root.path()).unwrap())
        .build()
        .unwrap();
    let locator = RepositoryLocator::new("repository").unwrap();
    client
        .initialize_remote(locator.clone(), "refs/heads/main")
        .await
        .unwrap();
    let repo = client.open_remote(locator).await.unwrap();
    assert_eq!(repo.refs().await.unwrap().head(), Some("refs/heads/main"));
    assert!(
        !repo
            .capabilities()
            .contains(&crate::RepositoryCapability::UpdateRefs)
    );
    client.close().await.unwrap();
}

#[tokio::test]
async fn initialization_is_lazy_and_adopts_roots_without_changing_head() {
    let client = memory_client();
    let locator = RepositoryLocator::new("repository").unwrap();
    let layout = crab_storage::StoreLayout::new(
        client.0.store.clone(),
        locator.direct_prefix().unwrap().to_owned(),
    );
    drop(client.initialize_remote(locator.clone(), "refs/heads/main"));
    assert!(
        client
            .0
            .store
            .list_prefix(&layout.repo_path(""))
            .await
            .unwrap()
            .is_empty()
    );
    client
        .initialize_remote(locator.clone(), "refs/heads/main")
        .await
        .unwrap();
    client
        .initialize_remote(locator.clone(), "refs/heads/other")
        .await
        .unwrap();
    let refs = client
        .open_remote(locator)
        .await
        .unwrap()
        .refs()
        .await
        .unwrap();
    assert_eq!(refs.head(), Some("refs/heads/main"));
    assert!(refs.entries().is_empty());
    client.close().await.unwrap();
}

#[tokio::test]
async fn initialization_rejects_invalid_heads_without_writes() {
    let client = memory_client();
    let locator = RepositoryLocator::new("repository").unwrap();
    for head in ["main", "refs/tags/v1", "refs/heads/", "refs/heads/a..b"] {
        let error = client
            .initialize_remote(locator.clone(), head)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidInput, "{head}");
    }
    let layout = crab_storage::StoreLayout::new(
        client.0.store.clone(),
        locator.direct_prefix().unwrap().to_owned(),
    );
    assert!(
        client
            .0
            .store
            .list_prefix(&layout.repo_path(""))
            .await
            .unwrap()
            .is_empty()
    );
    client.close().await.unwrap();
}

#[tokio::test]
async fn initialization_preserves_incompatible_prefixes() {
    for (path, bytes) in [
        ("orphan", b"existing".as_slice()),
        ("layout", b"invalid".as_slice()),
    ] {
        let client = memory_client();
        let locator = RepositoryLocator::new("repository").unwrap();
        let layout = crab_storage::StoreLayout::new(
            client.0.store.clone(),
            locator.direct_prefix().unwrap().to_owned(),
        );
        let path = if path == "layout" {
            layout.layout_descriptor_path()
        } else {
            layout.repo_path(path)
        };
        client
            .0
            .store
            .put(&path, Bytes::copy_from_slice(bytes))
            .await
            .unwrap();
        let error = client
            .initialize_remote(locator, "refs/heads/main")
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Corruption);
        let objects = client
            .0
            .store
            .list_prefix(&layout.repo_path(""))
            .await
            .unwrap();
        assert_eq!(objects.len(), 1);
        assert_eq!(client.0.store.get_with_etag(&path).await.unwrap().0, bytes);
        client.close().await.unwrap();
    }
}

#[tokio::test]
async fn initialization_adopts_an_interrupted_descriptor_create() {
    let client = memory_client();
    let locator = RepositoryLocator::new("repository").unwrap();
    let layout = crab_storage::StoreLayout::new(
        client.0.store.clone(),
        locator.direct_prefix().unwrap().to_owned(),
    );
    crab_metadata::layout_descriptor::ensure_canonical_layout(&client.0.store, &layout)
        .await
        .unwrap();
    client
        .initialize_remote(locator.clone(), "refs/heads/main")
        .await
        .unwrap();
    let refs = client
        .open_remote(locator)
        .await
        .unwrap()
        .refs()
        .await
        .unwrap();
    assert_eq!(refs.head(), Some("refs/heads/main"));
    client.close().await.unwrap();
}

#[tokio::test]
async fn initialization_deadline_drains_a_stalled_read() {
    let mut client = memory_client();
    let delayed = object_store::throttle::ThrottledStore::new(
        client.0.store.inner().clone(),
        object_store::throttle::ThrottleConfig {
            wait_get_per_call: Duration::from_secs(60),
            ..Default::default()
        },
    );
    Arc::get_mut(&mut client.0).unwrap().store = crab_storage::Store::new(Arc::new(delayed));
    let locator = RepositoryLocator::new("repository").unwrap();
    let options = OperationOptions::default()
        .with_timeout(Duration::from_millis(10))
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(1), async {
        client
            .initialize_remote(locator, "refs/heads/main")
            .with_options(options)
            .await
    })
    .await;
    assert!(matches!(result, Ok(Err(error)) if error.kind() == ErrorKind::Timeout));
    tokio::time::timeout(Duration::from_secs(1), client.close())
        .await
        .unwrap()
        .unwrap();
}
