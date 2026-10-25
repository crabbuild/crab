use std::collections::BTreeMap;
use std::sync::Arc;

use crab_sdk::operation::{ReadLimits, ReadOptions};
use crab_sdk::remote::{ArchiveEvent, ContentMode, EntryMode, PageRequest};
use crab_sdk::storage::DirectStoreOptions;
use crab_sdk::{Client, ErrorKind, GitPath, OpenOptions, RepositoryLocator, Revision};
use crab_storage::{Store, StoreLayout};

use super::support::{capsule_fixture::CapsuleFixture, copy_published_objects, git};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ten_thousand_files_paginate_and_archive_without_source() {
    verify(None).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires an isolated writable S3 or RustFS qualification bucket"]
async fn s3_ten_thousand_files_paginate_and_archive_without_source() {
    verify(Some(std::env::var("CRAB_SDK_TEST_S3_BUCKET").unwrap())).await;
}

async fn verify(bucket: Option<String>) {
    const FILES: usize = 10_000;
    // Identical blobs isolate path/tree scale from content diversity.
    const PAYLOAD: [u8; 4096] = [b'q'; 4096];
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source");
    let storage = directory.path().join("storage");
    std::fs::create_dir_all(source.join("ordinary")).unwrap();
    std::fs::create_dir(&storage).unwrap();
    git(&source, &["init", "--initial-branch=main"]);
    for index in 0..FILES {
        std::fs::write(source.join(format!("ordinary/{index:05}.txt")), PAYLOAD).unwrap();
    }
    git(&source, &["add", "ordinary"]);
    git(&source, &["commit", "-m", "large ordinary tree"]);
    let commit = git(&source, &["rev-parse", "HEAD"]);
    let pack_base = directory.path().join("fixture");
    let hash = git(
        &source,
        &[
            "pack-objects",
            "--all",
            "--index-version=2",
            pack_base.to_str().unwrap(),
        ],
    );
    let pack = directory.path().join(format!("fixture-{hash}.pack"));
    let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
    let prefix = format!(
        "qualification/sdk-tree-{}",
        directory.path().file_name().unwrap().to_str().unwrap()
    );
    let layout = StoreLayout::new(store.clone(), prefix.clone());
    let mut native =
        CapsuleFixture::new(layout, &source, &pack, std::slice::from_ref(&commit)).await;
    native
        .publish(
            BTreeMap::from([("refs/heads/main".to_owned(), commit)]),
            Default::default(),
        )
        .await;
    let cloud = bucket.as_ref().map(|bucket| {
        crab_storage::build_static_env_store(bucket, crab_storage::StorageProviderKind::S3).unwrap()
    });
    let mut published = Default::default();
    copy_published_objects(&store, &storage, cloud.as_ref(), &mut published).await;
    std::fs::remove_dir_all(&source).unwrap();
    std::fs::remove_file(pack).unwrap();
    let options = match &bucket {
        Some(bucket) => DirectStoreOptions::s3_from_env(bucket).unwrap(),
        None => DirectStoreOptions::filesystem(&storage).unwrap(),
    };
    let client = Client::builder().direct_store(options).build().unwrap();
    let repository = client
        .open(OpenOptions::remote(
            RepositoryLocator::new(&prefix).unwrap(),
        ))
        .await
        .unwrap();
    let snapshot = repository
        .remote()
        .unwrap()
        .snapshot(Revision::branch("main").unwrap())
        .await
        .unwrap();
    let mut cursor = None;
    let mut seen = 0;
    loop {
        let page = snapshot
            .tree(
                GitPath::new("ordinary").unwrap(),
                PageRequest::new(137, cursor).unwrap(),
            )
            .await
            .unwrap();
        assert!(!page.items.is_empty());
        assert!(page.items.len() <= 137);
        for entry in page.items {
            assert_eq!(
                entry.path.as_bytes(),
                format!("ordinary/{seen:05}.txt").as_bytes()
            );
            assert_eq!(entry.mode, EntryMode::Regular);
            seen += 1;
        }
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(seen, FILES);

    // Archive budgets include each repeated blob visit and each entry's depth.
    // Large traversals must opt in without weakening the default finite limits.
    let options = ReadOptions::default()
        .with_limits(ReadLimits {
            max_logical_objects: 20_000,
            max_depth: 30_000,
            ..Default::default()
        })
        .unwrap();
    for (expect_limit, limits) in [(true, ReadOptions::default()), (false, options)] {
        let mut stream = snapshot
            .archive(ContentMode::Git)
            .with_options(limits)
            .await
            .unwrap();
        let mut files = 0;
        let mut pending = None;
        let mut bytes = Vec::new();
        let terminal = loop {
            match stream.next().await {
                Ok(Some(ArchiveEvent::Entry { entry, .. })) => {
                    assert!(pending.is_none());
                    if entry.mode == EntryMode::Tree {
                        assert_eq!(entry.path.as_bytes(), b"ordinary");
                    } else {
                        assert_eq!(
                            entry.path.as_bytes(),
                            format!("ordinary/{files:05}.txt").as_bytes()
                        );
                    }
                    pending = Some(entry.mode);
                }
                Ok(Some(ArchiveEvent::Data(part))) => bytes.extend_from_slice(&part),
                Ok(Some(ArchiveEvent::EndEntry)) => {
                    if pending.take().unwrap() == EntryMode::Regular {
                        assert_eq!(bytes, PAYLOAD);
                        files += 1;
                    }
                    bytes.clear();
                }
                Ok(Some(_)) => panic!("unexpected archive event"),
                Ok(None) => break Ok(()),
                Err(error) => break Err(error.kind()),
            }
        };
        stream.close().await.unwrap();
        if expect_limit {
            assert_eq!(terminal, Err(ErrorKind::LimitExceeded));
        } else {
            terminal.unwrap();
            assert_eq!(files, FILES);
        }
    }
    client.close().await.unwrap();
    if let Some(cloud) = cloud {
        for path in published {
            cloud.delete(&path).await.unwrap();
        }
    }
}
