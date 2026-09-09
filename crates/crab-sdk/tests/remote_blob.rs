#![cfg(feature = "remote")]

#[path = "remote_blob/archive.rs"]
mod archive;
#[cfg(feature = "content")]
#[path = "remote_blob/crab.rs"]
mod crab;
#[cfg(feature = "content")]
#[path = "remote_blob/large.rs"]
mod large;
#[cfg(feature = "content")]
#[path = "remote_blob/lfs.rs"]
mod lfs;
#[path = "remote_blob/ranges.rs"]
mod ranges;

pub mod support;
use support::{copy_published_objects, git, publish_catalog};

use std::process::Command;
use std::sync::Arc;

use bytes::Bytes;
use crab_metadata::git_object_locator::{
    GitLocatorCoverage, GitObjectLocation, GitObjectLocatorEntry, GitPackLocatorRecord,
};
use crab_metadata::manifest_store::{
    create_manifest, read_manifest, upload_segmented_bulk, write_manifest_cas,
};
use crab_metadata::manifests::{
    BulkData, Manifest, PackManifestEntry, compact_pack_index, compact_shard_index,
};
use crab_sdk::{Client, DirectStoreOptions, GitPath, ReadOptions, RepositoryLocator, Revision};
use crab_storage::{Store, StoreLayout};
use crab_xet::hash::MerkleHash;
use futures_util::TryStreamExt;
use sha2::Digest as _;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn raw_blob_reads_survive_source_removal_and_keep_snapshots_pinned() {
    verify_native_reads(None).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires an empty dedicated CRAB_SDK_TEST_S3_BUCKET and S3 credentials"]
async fn s3_pack_reads_survive_source_removal_and_keep_snapshots_pinned() {
    let bucket = std::env::var("CRAB_SDK_TEST_S3_BUCKET").unwrap();
    verify_native_reads(Some(bucket)).await;
}

async fn verify_native_reads(bucket: Option<String>) {
    let mut published = std::collections::HashSet::new();
    let cloud = bucket.as_ref().map(|bucket| {
        crab_storage::build_static_env_store(bucket, crab_storage::StorageProviderKind::S3).unwrap()
    });
    if let Some(cloud) = &cloud {
        assert!(
            cloud
                .inner()
                .list(None)
                .try_collect::<Vec<_>>()
                .await
                .unwrap()
                .is_empty(),
            "use an empty dedicated qualification bucket"
        );
    }
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("source");
    let storage = directory.path().join("storage");
    std::fs::create_dir(&source).unwrap();
    std::fs::create_dir(&storage).unwrap();
    git(&source, &["init", "--initial-branch=main"]);
    #[cfg(unix)]
    let (filename, file_path) = {
        use std::os::unix::ffi::OsStringExt;
        let bytes = b"file-\xff.bin".to_vec();
        (bytes.clone(), std::ffi::OsString::from_vec(bytes))
    };
    #[cfg(not(unix))]
    let (filename, file_path) = (b"file.bin".to_vec(), std::ffi::OsString::from("file.bin"));
    let first_bytes = b"first committed bytes\0\xff\n";
    static SECOND_BYTES: [u8; 192 * 1024] = {
        let mut bytes = [0; 192 * 1024];
        let mut index = 0;
        while index < bytes.len() {
            bytes[index] = (index.wrapping_mul(73) ^ (index >> 8)) as u8;
            index += 1;
        }
        bytes
    };
    let second_bytes = SECOND_BYTES.as_slice();
    std::fs::write(source.join("file.bin"), first_bytes).unwrap();
    std::fs::write(source.join("second.txt"), b"another tree entry\n").unwrap();
    git(&source, &["add", "second.txt"]);
    let blob = git(&source, &["hash-object", "-w", "file.bin"]);
    git(
        &source,
        &[
            std::ffi::OsStr::new("update-index"),
            std::ffi::OsStr::new("--add"),
            std::ffi::OsStr::new("--cacheinfo"),
            std::ffi::OsStr::new("100644"),
            std::ffi::OsStr::new(&blob),
            file_path.as_os_str(),
        ],
    );
    git(&source, &["commit", "-m", "first"]);
    let first = git(&source, &["rev-parse", "HEAD"]);
    std::fs::write(source.join("file.bin"), second_bytes).unwrap();
    let blob = git(&source, &["hash-object", "-w", "file.bin"]);
    git(
        &source,
        &[
            std::ffi::OsStr::new("update-index"),
            std::ffi::OsStr::new("--add"),
            std::ffi::OsStr::new("--cacheinfo"),
            std::ffi::OsStr::new("100644"),
            std::ffi::OsStr::new(&blob),
            file_path.as_os_str(),
        ],
    );
    std::fs::write(source.join("second.txt"), b"another tree entry\nnew line\n").unwrap();
    git(&source, &["add", "second.txt"]);
    #[cfg(feature = "content")]
    let crab_fixture = crab::Fixture::new(second_bytes);
    #[cfg(feature = "content")]
    let crab_pointer = crab_fixture.pointer.serialize();
    #[cfg(not(feature = "content"))]
    let crab_pointer = b"version https://crab.dev/spec/v1\nfile-hash 0000000000000000000000000000000000000000000000000000000000000000\nsize 32\n".to_vec();
    #[cfg(feature = "content")]
    let crab_no_hint = crab_types::pointer::Pointer {
        file_hash: crab_fixture.pointer.file_hash,
        size: crab_fixture.pointer.size,
        shard_hint: None,
    }
    .serialize();
    #[cfg(not(feature = "content"))]
    let crab_no_hint = crab_pointer.clone();
    #[cfg(feature = "content")]
    let crab_stale_hint = crab_types::pointer::Pointer {
        file_hash: crab_fixture.pointer.file_hash,
        size: crab_fixture.pointer.size,
        shard_hint: Some([0; 32]),
    }
    .serialize();
    #[cfg(not(feature = "content"))]
    let crab_stale_hint = crab_pointer.clone();
    let lfs_oid: [u8; 32] = sha2::Sha256::digest(second_bytes).into();
    let lfs_pointer = crab_git::LfsPointer {
        oid: lfs_oid,
        size: second_bytes.len() as u64,
        extensions: Vec::new(),
    }
    .serialize();
    std::fs::write(source.join("z-crab"), &crab_pointer).unwrap();
    std::fs::write(source.join("z-lfs"), &lfs_pointer).unwrap();
    std::fs::write(source.join("z-crab-no-hint"), &crab_no_hint).unwrap();
    std::fs::write(source.join("z-crab-stale-hint"), &crab_stale_hint).unwrap();
    git(
        &source,
        &[
            "add",
            "z-crab",
            "z-crab-no-hint",
            "z-crab-stale-hint",
            "z-lfs",
        ],
    );
    git(&source, &["commit", "-m", "second"]);
    let second = git(&source, &["rev-parse", "HEAD"]);
    let base = directory.path().join("fixture");
    let hash = git(
        &source,
        &[
            "pack-objects",
            "--all",
            "--index-version=2",
            base.to_str().unwrap(),
        ],
    );
    let pack = directory.path().join(format!("fixture-{hash}.pack"));
    let index = pack.with_extension("idx");
    let reverse = pack.with_extension("rev");
    crab_git::write_pack_reverse_index(&index, &reverse).unwrap();
    let pack_bytes = std::fs::read(&pack).unwrap();
    let pack_size = pack_bytes.len() as u64;
    let pack_id = MerkleHash::from_hex(blake3::hash(&pack_bytes).to_hex().as_str()).unwrap();
    let locations = crab_git::PackLocationIter::open(&index, &reverse, pack_size)
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let object_count = locations.len() as u64;
    let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
    let layout = StoreLayout::new(store.clone(), "repository".to_owned());
    #[cfg(feature = "content")]
    crab_fixture.publish(&store, &layout).await;
    #[cfg(feature = "content")]
    {
        let lfs = crab_lfs::LfsObjectStore::new(store.clone(), "repository");
        store
            .put(
                &lfs.object_path_for(&lfs_oid),
                Bytes::from_static(second_bytes),
            )
            .await
            .unwrap();
    }

    store
        .put(&layout.pack_path(&pack_id), Bytes::from(pack_bytes))
        .await
        .unwrap();
    store
        .put(
            &layout.pack_index_path(&pack_id),
            Bytes::from(std::fs::read(index).unwrap()),
        )
        .await
        .unwrap();
    store
        .put(
            &layout.pack_reverse_index_path(&pack_id),
            Bytes::from(std::fs::read(reverse).unwrap()),
        )
        .await
        .unwrap();
    let (pack_index_hash, _, pack_index) = compact_pack_index(
        1,
        &[PackManifestEntry {
            pack_id: pack_id.to_string(),
            size: pack_size,
            content_hash: pack_id.to_string(),
            ref_tips: vec![second.clone()],
            object_count,
        }],
    )
    .unwrap();
    #[cfg(feature = "content")]
    let shards = vec![crab_fixture.shard.0.hex()];
    #[cfg(not(feature = "content"))]
    let shards = vec![];
    let (shard_index_hash, _, shard_index) = compact_shard_index(1, &shards).unwrap();
    upload_segmented_bulk(
        &store,
        &layout,
        &BulkData {
            shard_index,
            pack_index,
        },
    )
    .await
    .unwrap();
    let mut manifest = Manifest::default_for_repo("refs/heads/main");
    manifest.generation = 1;
    manifest
        .refs
        .insert("refs/heads/main".to_owned(), first.clone());
    manifest.pack_index_hash = pack_index_hash;
    manifest.shard_index_hash = shard_index_hash;
    manifest.seal_git_validation();
    create_manifest(&store, &layout, &manifest).await.unwrap();
    let mut coverage = GitLocatorCoverage {
        generation: 1,
        pack_index_hash: MerkleHash::from_hex(&manifest.pack_index_hash).unwrap(),
    };
    let record = GitPackLocatorRecord {
        pack_id,
        committed_generation: 1,
        pack_index_hash: coverage.pack_index_hash,
        object_count,
        pack_size,
    };
    let entries = locations
        .into_iter()
        .map(|entry| GitObjectLocatorEntry {
            oid: entry.oid.as_bytes().try_into().unwrap(),
            location: GitObjectLocation {
                pack_offset: entry.pack_offset,
                entry_len: entry.entry_len,
                crc32: entry.crc32,
            },
            metadata: Default::default(),
        })
        .collect::<Vec<_>>();
    publish_catalog(&store, coverage, record, &entries).await;
    copy_published_objects(&store, &storage, cloud.as_ref(), &mut published).await;
    std::fs::remove_dir_all(source).unwrap();
    std::fs::remove_file(pack).unwrap();

    let options = match &bucket {
        Some(bucket) => DirectStoreOptions::s3_from_env(bucket).unwrap(),
        None => DirectStoreOptions::filesystem(&storage).unwrap(),
    };
    #[cfg(feature = "content")]
    let fixture_store = cloud.clone().unwrap_or_else(|| {
        Store::new(Arc::new(
            object_store::local::LocalFileSystem::new_with_prefix(&storage).unwrap(),
        ))
    });
    let builder = Client::builder().direct_store(options.clone());
    #[cfg(feature = "content")]
    let builder = {
        let cache = directory.path().join("content-cache");
        std::fs::create_dir(&cache).unwrap();
        builder.content_cache(crab_sdk::ContentCache::new(&cache, 4 * 1024 * 1024).unwrap())
    };
    let client = builder.build().unwrap();
    let repository = client
        .open_remote(RepositoryLocator::new("repository").unwrap())
        .await
        .unwrap();
    let snapshot = repository
        .snapshot(Revision::branch("main").unwrap())
        .await
        .unwrap();
    let path = GitPath::new(filename.clone()).unwrap();
    let commit = snapshot.commit().await.unwrap();
    assert_eq!(
        (commit.id.to_string(), commit.message.as_ref()),
        (first.clone(), b"first\n".as_slice())
    );
    let page = snapshot
        .tree(
            GitPath::root(),
            crab_sdk::PageRequest::new(1, None).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(page.items[0].path.as_bytes(), filename);
    let continuation = page.next.unwrap();
    let tail = snapshot
        .tree(
            GitPath::root(),
            crab_sdk::PageRequest::new(1, Some(continuation)).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(tail.items[0].path.as_bytes(), b"second.txt");
    assert!(tail.next.is_none());

    assert_eq!(
        repository.refs().await.unwrap().entries()[0]
            .target()
            .to_string(),
        first
    );
    let hidden_commit = Revision::commit(crab_sdk::ObjectId::from_hex(&second).unwrap());
    assert_eq!(
        repository
            .snapshot(hidden_commit)
            .await
            .err()
            .unwrap()
            .kind(),
        crab_sdk::ErrorKind::NotFound
    );
    assert_eq!(
        snapshot.read_blob(path.clone()).await.unwrap().as_ref(),
        first_bytes
    );

    let (_, etag) = read_manifest(&store, &layout).await.unwrap();
    manifest.generation = 2;
    manifest
        .refs
        .insert("refs/heads/main".to_owned(), second.clone());
    manifest.seal_git_validation();
    write_manifest_cas(&store, &layout, &manifest, &etag)
        .await
        .unwrap();
    coverage.generation = 2;
    publish_catalog(&store, coverage, record, &entries).await;
    copy_published_objects(&store, &storage, cloud.as_ref(), &mut published).await;
    let refreshed = repository.refresh().await.unwrap();
    assert_eq!(
        refreshed.refs().await.unwrap().entries()[0]
            .target()
            .to_string(),
        second
    );
    assert_eq!(
        repository.refs().await.unwrap().entries()[0]
            .target()
            .to_string(),
        first
    );
    let current = refreshed
        .snapshot(Revision::branch("main").unwrap())
        .await
        .unwrap();
    assert_eq!(current.commit_id().unwrap().to_string(), second);
    assert_eq!(
        current.read_blob(path.clone()).await.unwrap().as_ref(),
        second_bytes
    );
    let base = Revision::commit(crab_sdk::ObjectId::from_hex(&first).unwrap());
    let binary = current.diff(base.clone(), path.clone()).await.unwrap();
    assert_eq!(binary.classification, crab_sdk::DiffClassification::Binary);
    assert!(binary.hunks.is_empty());
    let text_path = GitPath::new(b"second.txt".to_vec()).unwrap();
    let text = current.diff(base, text_path.clone()).await.unwrap();
    assert_eq!(text.classification, crab_sdk::DiffClassification::Text);
    assert_eq!(text.hunks.len(), 1);
    let hunk = &text.hunks[0];
    assert_eq!(
        (
            hunk.old_start,
            hunk.old_lines,
            hunk.new_start,
            hunk.new_lines
        ),
        (1, 1, 1, 2)
    );
    assert_eq!(
        hunk.bytes.as_ref(),
        b"-another tree entry\n+another tree entry\n+new line\n"
    );
    assert_eq!(
        snapshot
            .diff(
                Revision::commit(crab_sdk::ObjectId::from_hex(&second).unwrap()),
                text_path.clone()
            )
            .await
            .unwrap_err()
            .kind(),
        crab_sdk::ErrorKind::NotFound,
    );
    let blame = current.blame(text_path).await.unwrap();
    let attribution = blame
        .ranges
        .iter()
        .map(|range| {
            (
                range.start,
                range.lines,
                range.commit.id.to_string(),
                range.source_path.as_bytes(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        attribution,
        vec![
            (1, 1, first.clone(), b"second.txt".as_slice()),
            (2, 1, second.clone(), b"second.txt".as_slice())
        ]
    );
    assert_eq!(
        current.blame(path.clone()).await.unwrap_err().kind(),
        crab_sdk::ErrorKind::UnsupportedCapability
    );
    assert_eq!(snapshot.commit_id().unwrap().to_string(), first);
    assert_eq!(
        snapshot.read_blob(path).await.unwrap().as_ref(),
        first_bytes
    );
    let archived = archive::collect(&current, crab_sdk::ContentMode::Git).await;
    assert_eq!(
        archived,
        vec![
            (filename.clone(), Bytes::from_static(second_bytes)),
            (
                b"second.txt".to_vec(),
                Bytes::from_static(b"another tree entry\nnew line\n")
            ),
            (b"z-crab".to_vec(), Bytes::copy_from_slice(&crab_pointer)),
            (
                b"z-crab-no-hint".to_vec(),
                Bytes::copy_from_slice(&crab_no_hint)
            ),
            (
                b"z-crab-stale-hint".to_vec(),
                Bytes::copy_from_slice(&crab_stale_hint)
            ),
            (b"z-lfs".to_vec(), Bytes::copy_from_slice(&lfs_pointer)),
        ]
    );
    for advance in [false, true] {
        let mut archive = current.archive(crab_sdk::ContentMode::Git).await.unwrap();
        if advance {
            assert!(archive.next().await.unwrap().is_some());
        }
        tokio::time::timeout(std::time::Duration::from_secs(2), archive.close())
            .await
            .unwrap()
            .unwrap();
    }
    #[cfg(not(feature = "content"))]
    assert_eq!(
        current
            .archive(crab_sdk::ContentMode::Hydrated)
            .await
            .err()
            .unwrap()
            .kind(),
        crab_sdk::ErrorKind::UnsupportedCapability
    );
    #[cfg(feature = "content")]
    {
        let hydrated = archive::collect(&current, crab_sdk::ContentMode::Hydrated).await;
        let expected = archived
            .into_iter()
            .map(|(path, bytes)| {
                let bytes = if [
                    b"z-crab".as_slice(),
                    b"z-crab-no-hint",
                    b"z-crab-stale-hint",
                    b"z-lfs",
                ]
                .contains(&path.as_slice())
                {
                    Bytes::copy_from_slice(second_bytes)
                } else {
                    bytes
                };
                (path, bytes)
            })
            .collect::<Vec<_>>();
        assert_eq!(hydrated, expected);
        archive::logical_limit(&current, second_bytes.len()).await;
        archive::close_during_hydration(&current).await;
    }

    let mut expired = current
        .archive(crab_sdk::ContentMode::Git)
        .with_options(
            ReadOptions::default()
                .with_timeout(std::time::Duration::from_millis(200))
                .unwrap(),
        )
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    let terminal = loop {
        match expired.next().await {
            Ok(Some(_)) => continue,
            Ok(None) => panic!("idle archive deadline was not enforced"),
            Err(error) => break error,
        }
    };
    assert_eq!(terminal.kind(), crab_sdk::ErrorKind::Timeout);
    expired.close().await.unwrap();
    let mut content = current
        .open_file(GitPath::new(filename.clone()).unwrap())
        .await
        .unwrap();
    let mut delivered = Vec::new();
    while let Some(bytes) = content.next().await.unwrap() {
        assert!(bytes.len() <= 64 * 1024);
        delivered.extend_from_slice(&bytes);
    }
    assert_eq!(delivered, second_bytes);
    content.close().await.unwrap();
    for advance in [false, true] {
        let mut content = current
            .open_file(GitPath::new(filename.clone()).unwrap())
            .await
            .unwrap();
        if advance {
            assert!(content.next().await.unwrap().is_some());
        }
        tokio::time::timeout(std::time::Duration::from_secs(2), content.close())
            .await
            .unwrap()
            .unwrap();
    }
    let mut expired = current
        .open_file(GitPath::new(filename.clone()).unwrap())
        .with_options(
            ReadOptions::default()
                .with_timeout(std::time::Duration::from_millis(200))
                .unwrap(),
        )
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    let terminal = loop {
        match expired.next().await {
            Ok(Some(_)) => continue,
            Ok(None) => panic!("idle content deadline was not enforced"),
            Err(error) => break error,
        }
    };
    assert_eq!(terminal.kind(), crab_sdk::ErrorKind::Timeout);
    expired.close().await.unwrap();
    for (name, pointer) in [
        (b"z-crab".as_slice(), crab_pointer.as_slice()),
        (b"z-crab-no-hint".as_slice(), crab_no_hint.as_slice()),
        (b"z-crab-stale-hint".as_slice(), crab_stale_hint.as_slice()),
        (b"z-lfs".as_slice(), lfs_pointer.as_slice()),
    ] {
        let path = GitPath::new(name.to_vec()).unwrap();
        assert_eq!(
            current.read_blob(path.clone()).await.unwrap().as_ref(),
            pointer
        );
        if !cfg!(feature = "content") {
            assert_eq!(
                current.open_file(path).await.err().unwrap().kind(),
                crab_sdk::ErrorKind::UnsupportedCapability
            );
        }
    }
    ranges::verify(&current, &filename, second_bytes).await;
    ranges::verify_limits(&current, &filename).await;
    ranges::verify_controls(&current, &filename).await;
    ranges::verify_progress(&current, &filename).await;
    #[cfg(feature = "content")]
    lfs::verify(&current, &fixture_store, lfs_oid, second_bytes).await;
    #[cfg(feature = "content")]
    crab::verify(&current, second_bytes).await;
    #[cfg(feature = "content")]
    crab::verify_limits(options.clone(), &directory.path().join("limits-cache")).await;
    #[cfg(feature = "content")]
    crab_fixture
        .verify_corruption(
            &fixture_store,
            options,
            &layout,
            &directory.path().join("corrupt-cache"),
        )
        .await;

    let history = current
        .history(
            crab_sdk::HistoryTraversal::AllParents,
            crab_sdk::PageRequest::new(1, None).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(history.items[0].id.to_string(), second);
    let cursor = history.next.unwrap();
    let parent = current
        .history(
            crab_sdk::HistoryTraversal::AllParents,
            crab_sdk::PageRequest::new(1, Some(cursor.clone())).unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(parent.items[0].id.to_string(), first);
    assert!(parent.next.is_none());

    let (_, etag) = read_manifest(&store, &layout).await.unwrap();
    manifest.generation = 3;
    manifest.shard_index_hash.clear();
    manifest.seal_git_validation();
    write_manifest_cas(&store, &layout, &manifest, &etag)
        .await
        .unwrap();
    coverage.generation = 3;
    publish_catalog(&store, coverage, record, &entries).await;
    copy_published_objects(&store, &storage, cloud.as_ref(), &mut published).await;
    let same_commit = refreshed
        .refresh()
        .await
        .unwrap()
        .snapshot(Revision::branch("main").unwrap())
        .await
        .unwrap();
    assert_eq!(
        same_commit.commit_id().unwrap(),
        current.commit_id().unwrap()
    );
    #[cfg(feature = "content")]
    {
        crab::verify_pinned_lookup(&current, second_bytes).await;
        crab::verify_lookup_missing(&same_commit).await;
    }
    let invalid = same_commit
        .history(
            crab_sdk::HistoryTraversal::AllParents,
            crab_sdk::PageRequest::new(1, Some(cursor)).unwrap(),
        )
        .await
        .unwrap_err();
    assert_eq!(invalid.kind(), crab_sdk::ErrorKind::InvalidInput);
    let archive = current.archive(crab_sdk::ContentMode::Git).await.unwrap();
    drop(archive);
    let archive = current.archive(crab_sdk::ContentMode::Git).await.unwrap();
    let content = current
        .open_file(GitPath::new(filename.clone()).unwrap())
        .await
        .unwrap();
    #[cfg(feature = "content")]
    let lfs = current
        .open_file(GitPath::new(b"z-lfs".to_vec()).unwrap())
        .await
        .unwrap();
    #[cfg(feature = "content")]
    let crab = current
        .open_file(GitPath::new(b"z-crab".to_vec()).unwrap())
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), client.close())
        .await
        .unwrap()
        .unwrap();
    archive.close().await.unwrap();
    content.close().await.unwrap();
    #[cfg(feature = "content")]
    lfs.close().await.unwrap();
    #[cfg(feature = "content")]
    crab.close().await.unwrap();
    if let (Some(bucket), Some(examples)) = (&bucket, std::env::var_os("CRAB_SDK_TEST_EXAMPLES")) {
        let examples = std::path::PathBuf::from(examples);
        let expected = b"another tree entry\nnew line\n";
        let read_executable = examples.join(format!("remote_read{}", std::env::consts::EXE_SUFFIX));
        if let Some(guard) = std::env::var_os("CRAB_SDK_TEST_READ_GUARD") {
            let cases = directory.path().join("read-cases.json");
            let raw = serde_json::json!({
                "args": [bucket, "repository", "main", "second.txt", "git"],
                "expected_stdout": format!(
                    "commit={second} bytes={} blake3={}\n",
                    expected.len(),
                    blake3::hash(expected)
                )
            });
            #[cfg(feature = "content")]
            let hydrated = {
                let cache = directory.path().join("example-cache");
                std::fs::create_dir(&cache).unwrap();
                ["z-crab", "z-lfs"].map(|path| {
                    serde_json::json!({
                        "args": [bucket, "repository", "main", path, "hydrated", cache],
                        "expected_stdout": format!(
                            "commit={second} bytes={} blake3={}\n",
                            second_bytes.len(),
                            blake3::hash(second_bytes)
                        )
                    })
                })
            };
            #[cfg(feature = "content")]
            let inputs = std::iter::once(raw).chain(hydrated).collect::<Vec<_>>();
            #[cfg(not(feature = "content"))]
            let inputs = vec![raw];
            std::fs::write(&cases, serde_json::to_vec(&inputs).unwrap()).unwrap();
            let result = Command::new(
                std::env::var_os("CRAB_SDK_TEST_PYTHON").unwrap_or_else(|| "python3".into()),
            )
            .arg(guard)
            .arg(&read_executable)
            .arg(std::env::var("CRAB_SDK_TEST_S3_ENDPOINT").unwrap())
            .arg(cases)
            .output()
            .unwrap();
            assert!(
                result.status.success(),
                "read-only guard failed: {}",
                String::from_utf8_lossy(&result.stderr)
            );
            let report = std::env::var_os("CRAB_SDK_TEST_READ_GUARD_REPORT").unwrap();
            std::fs::write(report, result.stdout).unwrap();
        } else {
            let read = Command::new(&read_executable)
                .args([bucket, "repository", "main", "second.txt", "git"])
                .output()
                .unwrap();
            assert!(
                read.status.success(),
                "example failed: {}",
                String::from_utf8_lossy(&read.stderr)
            );
            assert_eq!(
                String::from_utf8(read.stdout).unwrap(),
                format!(
                    "commit={second} bytes={} blake3={}\n",
                    expected.len(),
                    blake3::hash(expected)
                )
            );
        }
        #[cfg(feature = "content")]
        if std::env::var_os("CRAB_SDK_TEST_READ_GUARD").is_none() {
            let cache = directory.path().join("example-cache");
            std::fs::create_dir(&cache).unwrap();
            for path in ["z-crab", "z-lfs"] {
                let read = Command::new(
                    examples.join(format!("remote_read{}", std::env::consts::EXE_SUFFIX)),
                )
                .args([bucket, "repository", "main", path, "hydrated"])
                .arg(&cache)
                .output()
                .unwrap();
                assert!(
                    read.status.success(),
                    "hydrated example failed: {}",
                    String::from_utf8_lossy(&read.stderr)
                );
                assert_eq!(
                    String::from_utf8(read.stdout).unwrap(),
                    format!(
                        "commit={second} bytes={} blake3={}\n",
                        second_bytes.len(),
                        blake3::hash(second_bytes)
                    )
                );
            }
        }
        let archive =
            Command::new(examples.join(format!("remote_archive{}", std::env::consts::EXE_SUFFIX)))
                .args([bucket, "repository", "main"])
                .output()
                .unwrap();
        assert!(
            archive.status.success(),
            "example failed: {}",
            String::from_utf8_lossy(&archive.stderr)
        );
        assert!(
            String::from_utf8(archive.stdout)
                .unwrap()
                .contains(&format!("{:?} {}\n", b"second.txt", expected.len()))
        );
    }
    if let Some(cloud) = cloud {
        let objects = cloud
            .inner()
            .list(None)
            .try_collect::<Vec<_>>()
            .await
            .unwrap();
        assert!(
            objects
                .iter()
                .all(|object| published.contains(&object.location)),
            "SDK left an object not published by the fixture"
        );
        // Older catalog checkpoints may leave the publisher's final inventory.
        // Cleanup owns every copied generation, not only its last view.
        for path in published {
            cloud.delete(&path).await.unwrap();
        }
        assert!(
            cloud
                .inner()
                .list(None)
                .try_collect::<Vec<_>>()
                .await
                .unwrap()
                .is_empty()
        );
    }
}
