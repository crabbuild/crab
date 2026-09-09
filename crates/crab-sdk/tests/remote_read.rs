#![cfg(feature = "remote")]

pub mod support;

use std::sync::Arc;

use crab_metadata::git_object_locator::{
    GitLocatorCoverage, GitObjectLocatorWriter, GitPackLocatorRecord,
};
use crab_metadata::manifest_store::{create_manifest, upload_segmented_bulk};
use crab_metadata::manifests::{
    BulkData, Manifest, PackManifestEntry, compact_pack_index, compact_shard_index,
};
use crab_sdk::{Client, DirectStoreOptions, ErrorKind, RepositoryLocator};
use crab_storage::{Store, StoreLayout};
use crab_xet::hash::MerkleHash;
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

async fn publish_indexing_fixture(root: &std::path::Path, coverage: Option<u64>) -> Store {
    let backend = Arc::new(object_store::local::LocalFileSystem::new_with_prefix(root).unwrap());
    let store = Store::new(backend.clone());
    let layout = StoreLayout::new(store.clone(), "repository".to_owned());
    let pack_id = crab_xet::hash::compute_data_hash(b"pending pack");
    let tip = "1111111111111111111111111111111111111111";
    let pack = PackManifestEntry {
        pack_id: pack_id.to_string(),
        content_hash: pack_id.to_string(),
        size: 128,
        object_count: 1,
        ref_tips: vec![tip.to_owned()],
    };
    let (pack_index_hash, _, pack_index) = compact_pack_index(1, &[pack]).unwrap();
    let (shard_index_hash, _, shard_index) = compact_shard_index(1, &[]).unwrap();
    upload_segmented_bulk(
        &store,
        &layout,
        &BulkData {
            pack_index,
            shard_index,
        },
    )
    .await
    .unwrap();
    let mut manifest = Manifest::default_for_repo("refs/heads/main");
    manifest.generation = 2;
    manifest
        .refs
        .insert("refs/heads/main".to_owned(), tip.to_owned());
    manifest.pack_index_hash = pack_index_hash;
    manifest.shard_index_hash = shard_index_hash;
    manifest.seal_git_validation();
    create_manifest(&store, &layout, &manifest).await.unwrap();
    if let Some(generation) = coverage {
        let hash = MerkleHash::from_hex(&manifest.pack_index_hash).unwrap();
        let mut writer = GitObjectLocatorWriter::open(backend, "repository")
            .await
            .unwrap();
        writer
            .bind_packs(&[GitPackLocatorRecord {
                pack_id,
                committed_generation: 1,
                pack_index_hash: hash,
                object_count: 1,
                pack_size: 128,
            }])
            .await
            .unwrap();
        writer
            .set_coverage(GitLocatorCoverage {
                generation,
                pack_index_hash: hash,
            })
            .await
            .unwrap();
        writer.close().await.unwrap();
    }
    store
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn read_only_open_never_repairs() {
    for coverage in [None, Some(1)] {
        let directory = tempfile::tempdir().unwrap();
        let store = publish_indexing_fixture(directory.path(), coverage).await;
        // Catalog readiness must fail before fetching pack data or starting a
        // repair writer. No pack object is needed to diagnose this boundary.
        let before = stored_bytes(&store).await;
        let client = Client::builder()
            .direct_store(DirectStoreOptions::filesystem(directory.path()).unwrap())
            .build()
            .unwrap();
        let error = client
            .open_remote(RepositoryLocator::new("repository").unwrap())
            .await
            .err()
            .unwrap();
        client.close().await.unwrap();
        assert_eq!(
            (error.kind(), stored_bytes(&store).await),
            (ErrorKind::Indexing, before),
            "catalog coverage: {coverage:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn locator_acquisition_obeys_operation_limits() {
    let directory = tempfile::tempdir().unwrap();
    publish_indexing_fixture(directory.path(), Some(2)).await;
    let client = Client::builder()
        .direct_store(DirectStoreOptions::filesystem(directory.path()).unwrap())
        .build()
        .unwrap();
    let repository = client
        .open_remote(RepositoryLocator::new("repository").unwrap())
        .await
        .unwrap();
    for limits in [
        crab_sdk::ReadLimits {
            max_storage_requests: 1,
            ..Default::default()
        },
        crab_sdk::ReadLimits {
            max_fetched_bytes: 1,
            ..Default::default()
        },
    ] {
        let options = crab_sdk::OperationOptions::default()
            .with_limits(limits)
            .unwrap()
            .with_timeout(std::time::Duration::from_secs(1))
            .unwrap();
        let error = repository
            .snapshot(crab_sdk::Revision::branch("main").unwrap())
            .with_options(options)
            .await
            .err()
            .unwrap();
        assert_eq!(error.kind(), crab_sdk::ErrorKind::LimitExceeded);
    }
    client.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "publishes fixtures under an existing dedicated CRAB_SDK_INDEXING_FIXTURE_DIR"]
async fn publish_indexing_qualification_fixtures() {
    let root = std::path::PathBuf::from(std::env::var_os("CRAB_SDK_INDEXING_FIXTURE_DIR").unwrap());
    assert!(root.is_absolute() && root.is_dir());
    for (name, coverage) in [("absent", None), ("stale", Some(1))] {
        let directory = root.join(name);
        std::fs::create_dir(&directory).unwrap();
        publish_indexing_fixture(&directory, coverage).await;
    }
}

#[tokio::test]
async fn snapshot_stays_pinned() {
    let mut fixture = support::read_fixture::ReadFixture::new().await;
    let old_commit = fixture.snapshot.commit().await.unwrap().id;
    let refreshed = fixture.advance().await;
    let current = refreshed
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
            crab_sdk::PageRequest::new(10, None).unwrap(),
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
        crab_sdk::ReadOptions::default(),
        crab_sdk::ReadOptions::default().with_range(0..1).unwrap(),
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
        .archive(crab_sdk::ContentMode::Hydrated)
        .await
        .unwrap();
    let mut extension_entry = false;
    let error = loop {
        match archive.next().await {
            Ok(Some(crab_sdk::ArchiveEvent::Entry { entry, .. })) => {
                extension_entry = entry.path == path;
            }
            Ok(Some(crab_sdk::ArchiveEvent::Data(_) | crab_sdk::ArchiveEvent::EndEntry)) => {
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
