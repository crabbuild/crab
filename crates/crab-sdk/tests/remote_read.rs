#![cfg(feature = "remote")]

pub mod support;

use std::sync::Arc;

use crab_sdk::ErrorKind;
use crab_storage::Store;
use futures_util::TryStreamExt as _;
use object_store::ObjectStoreExt as _;

async fn stored_bytes(store: &Store) -> Vec<(String, bytes::Bytes)> {
    let mut objects = store
        .inner()
        .list(None)
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    objects.sort_by(|a, b| a.location.cmp(&b.location));
    let mut contents = Vec::new();
    for object in objects {
        let bytes = store
            .inner()
            .get(&object.location)
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        contents.push((object.location.to_string(), bytes));
    }
    contents
}

#[test]
fn sha256_rejected_before_mutation() {
    let directory = tempfile::tempdir().unwrap();
    let before = std::fs::read_dir(directory.path()).unwrap().count();
    let error = crab_sdk::ObjectId::from_hex(&"a".repeat(64)).err().unwrap();

    assert_eq!(error.kind(), ErrorKind::UnsupportedCapability);
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_only_open_never_repairs() {
    let fixture = support::read_fixture::ReadFixture::new().await;
    let storage = fixture.directory.path().join("storage");
    let store = Store::new(Arc::new(
        object_store::local::LocalFileSystem::new_with_prefix(&storage).unwrap(),
    ));
    let before = stored_bytes(&store).await;
    let repository = fixture
        .repository
        .remote()
        .unwrap()
        .refresh()
        .await
        .unwrap();
    repository
        .remote()
        .unwrap()
        .snapshot(crab_sdk::Revision::branch("main").unwrap())
        .await
        .unwrap();
    fixture.client.close().await.unwrap();
    assert_eq!(stored_bytes(&store).await, before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot_reads_obey_operation_limits() {
    let fixture = support::read_fixture::ReadFixture::new().await;
    let client = &fixture.client;
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
        let options = crab_sdk::operation::Options::default()
            .with_limits(limits)
            .unwrap()
            .with_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        let error = fixture
            .snapshot
            .read_blob(fixture.path.clone())
            .with_options(crab_sdk::operation::ReadOptions::default().with_operation(options))
            .await
            .err()
            .unwrap();
        assert_eq!(error.kind(), crab_sdk::ErrorKind::LimitExceeded);
    }
    client.close().await.unwrap();
}

#[tokio::test]
async fn snapshot_stays_pinned() {
    let mut fixture = support::read_fixture::ReadFixture::new().await;
    let old_commit = fixture.snapshot.commit().await.unwrap().id;
    let refreshed = fixture.advance().await;
    let current = refreshed
        .remote()
        .unwrap()
        .snapshot(crab_sdk::Revision::branch("main").unwrap())
        .await
        .unwrap();
    assert_ne!(current.commit().await.unwrap().id, old_commit);
    assert_eq!(
        fixture
            .snapshot
            .read_blob(fixture.path.clone())
            .await
            .unwrap(),
        fixture.original
    );
    assert_eq!(
        current.read_blob(fixture.path.clone()).await.unwrap(),
        fixture.updated
    );
    fixture.client.close().await.unwrap();
}

#[cfg(feature = "content")]
#[tokio::test]
async fn raw_and_hydrated_bytes_are_distinct() {
    let fixture = support::read_fixture::ReadFixture::new().await;
    let path = crab_sdk::GitPath::new(b"z-lfs".to_vec()).unwrap();
    let raw = fixture.snapshot.read_blob(path.clone()).await.unwrap();
    assert_eq!(raw, fixture.pointer);
    let mut stream = fixture.snapshot.open_file(path).await.unwrap();
    let mut hydrated = Vec::new();
    while let Some(bytes) = stream.next().await.unwrap() {
        hydrated.extend_from_slice(&bytes);
    }
    stream.close().await.unwrap();
    assert_eq!(hydrated, fixture.updated);
    assert_ne!(hydrated.as_slice(), raw.as_ref());
    fixture.client.close().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn byte_paths_round_trip() {
    let fixture = support::read_fixture::ReadFixture::new().await;
    let page = fixture
        .snapshot
        .tree(
            crab_sdk::GitPath::root(),
            crab_sdk::remote::PageRequest::new(10, None).unwrap(),
        )
        .await
        .unwrap();
    assert!(
        page.items
            .iter()
            .any(|entry| entry.path.as_bytes() == b"file-\xff.bin")
    );
    assert_eq!(
        fixture
            .snapshot
            .read_blob(fixture.path.clone())
            .await
            .unwrap(),
        fixture.original
    );
    fixture.client.close().await.unwrap();
}

#[cfg(feature = "content")]
#[tokio::test]
async fn lfs_extensions_never_deliver_untransformed_hydrated_bytes() {
    let fixture = support::read_fixture::ReadFixture::new().await;
    let path = crab_sdk::GitPath::new(b"z-lfs-extension".to_vec()).unwrap();
    let raw = fixture.snapshot.read_blob(path.clone()).await.unwrap();
    assert_eq!(
        crab_git::LfsPointer::parse(&raw).unwrap().extensions.len(),
        1
    );
    for options in [
        crab_sdk::operation::ReadOptions::default(),
        crab_sdk::operation::ReadOptions::default()
            .with_range(0..1)
            .unwrap(),
    ] {
        let error = match fixture
            .snapshot
            .open_file(path.clone())
            .with_options(options)
            .await
        {
            Err(error) => error,
            Ok(_) => panic!("LFS extension unexpectedly accepted for hydration"),
        };
        assert_eq!(error.kind(), ErrorKind::UnsupportedCapability);
    }
    let mut archive = fixture
        .snapshot
        .archive(crab_sdk::remote::ContentMode::Hydrated)
        .await
        .unwrap();
    let mut extension_entry = false;
    let error = loop {
        match archive.next().await {
            Ok(Some(crab_sdk::remote::ArchiveEvent::Entry { entry, .. })) => {
                extension_entry = entry.path == path;
            }
            Ok(Some(
                crab_sdk::remote::ArchiveEvent::Data(_) | crab_sdk::remote::ArchiveEvent::EndEntry,
            )) => {
                assert!(
                    !extension_entry,
                    "extension entry delivered hydrated content"
                );
            }
            Ok(Some(_)) => {}
            Ok(None) => panic!("archive accepted unsupported LFS extension"),
            Err(error) => break error,
        }
    };
    assert!(extension_entry);
    assert_eq!(error.kind(), ErrorKind::UnsupportedCapability);
    archive.close().await.unwrap();
    fixture.client.close().await.unwrap();
}

#[cfg(feature = "write")]
#[tokio::test(flavor = "multi_thread")]
async fn indexed_reads_outlive_the_opening_admission() {
    let fixture = support::read_fixture::ReadFixture::new().await;
    let layout = crab_storage::StoreLayout::new(fixture.store.clone(), "repository".to_owned());
    let runtime = Arc::new(crab_remote_git::RemoteGitRuntime::default());
    let ready = crab_remote::browse_indexes::ensure(
        &layout,
        &crab_remote_git::RepositoryIdentity::new("fixture", "repository", 1).unwrap(),
        runtime.clone(),
        Default::default(),
        16 * 1024 * 1024,
        &tokio_util::sync::CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(ready.object_index_hash().is_some() && !ready.commit_graph_hash().is_empty());
    // Service callers can reuse a captured view after its request completes.
    // A new opening must use its own admission, not a retired capture token.
    let cancellation = tokio_util::sync::CancellationToken::new();
    let admitted = crab_storage::StoreLayout::new(
        layout.store().clone().with_read_admission(
            runtime
                .read_admission(Default::default(), cancellation.clone())
                .unwrap(),
        ),
        layout.repo_prefix().to_owned(),
    );
    let root = crab_metadata::capsule_protocol::load_root(&admitted)
        .await
        .unwrap();
    let view = crab_read::capsule_protocol::open_git_view_from_root(
        &admitted,
        root,
        crab_read::capsule_protocol::CapsuleReadLimits {
            max_capsule_bytes: 16 * 1024 * 1024,
            max_frontier_bytes: 16 * 1024 * 1024,
        },
    )
    .await
    .unwrap()
    .with_browse_indexes(Some(ready.clone()));
    cancellation.cancel();
    view.git_repository_from_store(
        layout.clone(),
        crab_remote_git::RepositoryIdentity::new("fixture", "repository", 1).unwrap(),
        runtime.clone(),
        Default::default(),
        16 * 1024 * 1024,
        &tokio_util::sync::CancellationToken::new(),
        None,
    )
    .await
    .unwrap();
    runtime.shutdown().await;
    support::copy_published_objects(
        &fixture.store,
        &fixture.directory.path().join("storage"),
        None,
        &mut Default::default(),
    )
    .await;
    let repository = fixture
        .repository
        .remote()
        .unwrap()
        .refresh()
        .await
        .unwrap();
    let snapshot = repository
        .remote()
        .unwrap()
        .snapshot(crab_sdk::Revision::branch("main").unwrap())
        .await
        .unwrap();
    let bytes = snapshot.read_blob(fixture.path.clone()).await.unwrap();
    let descriptor =
        layout.bulk_manifest_path("git-object-index", ready.object_index_hash().unwrap());
    std::fs::write(
        fixture
            .directory
            .path()
            .join("storage")
            .join(descriptor.as_ref()),
        b"corrupt current descriptor",
    )
    .unwrap();
    let error = repository.remote().unwrap().refresh().await.err().unwrap();
    fixture.client.close().await.unwrap();
    assert_eq!(
        (bytes, error.kind()),
        (fixture.original, ErrorKind::Corruption)
    );
}
