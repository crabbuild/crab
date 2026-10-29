use super::*;
use crab_metadata::manifests::ShardList;
use object_store::memory::InMemory;
use std::io::Write as _;
use std::process::{Command, Stdio};
use std::sync::Arc;

fn memory_store() -> Store {
    Store::new(Arc::new(InMemory::new()))
}

fn git_pack_fixture() -> (String, crab_metadata::capsule_protocol::CapsuleGitPack) {
    let workspace = tempfile::tempdir().unwrap();
    let git_dir = workspace.path().join("repository.git");
    assert!(
        Command::new("git")
            .args(["init", "--bare", "--quiet"])
            .arg(&git_dir)
            .status()
            .unwrap()
            .success()
    );
    let mut hash = Command::new("git")
        .arg("--git-dir")
        .arg(&git_dir)
        .args(["hash-object", "-t", "tree", "-w", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    hash.stdin.take().unwrap().write_all(b"").unwrap();
    let tree = String::from_utf8(hash.wait_with_output().unwrap().stdout)
        .unwrap()
        .trim()
        .to_owned();
    let mut commit = Command::new("git")
        .arg("--git-dir")
        .arg(&git_dir)
        .args(["commit-tree", &tree])
        .env("GIT_AUTHOR_NAME", "Crab Test")
        .env("GIT_AUTHOR_EMAIL", "crab@example.invalid")
        .env("GIT_AUTHOR_DATE", "@1 +0000")
        .env("GIT_COMMITTER_NAME", "Crab Test")
        .env("GIT_COMMITTER_EMAIL", "crab@example.invalid")
        .env("GIT_COMMITTER_DATE", "@1 +0000")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    commit.stdin.take().unwrap().write_all(b"commit\n").unwrap();
    let tip = String::from_utf8(commit.wait_with_output().unwrap().stdout)
        .unwrap()
        .trim()
        .to_owned();
    assert!(
        Command::new("git")
            .arg("--git-dir")
            .arg(&git_dir)
            .args(["update-ref", "refs/heads/main", &tip])
            .status()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("git")
            .arg("--git-dir")
            .arg(&git_dir)
            .args(["repack", "-a", "-d", "--depth=64"])
            .status()
            .unwrap()
            .success()
    );
    let source_pack = std::fs::read_dir(git_dir.join("objects/pack"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.extension()
                .is_some_and(|extension| extension == "pack")
        })
        .unwrap();
    let pack_bytes = std::fs::read(&source_pack).unwrap();
    let canonical_id = blake3::hash(&pack_bytes).to_hex().to_string();
    let installed_dir = workspace.path().join("installed");
    std::fs::create_dir_all(&installed_dir).unwrap();
    let installed = crab_git::pack::install_pack_file_from_path(
        &installed_dir,
        &source_pack,
        &canonical_id,
        MAX_CAPSULE_BYTES,
        true,
    )
    .unwrap();
    let mut locations = crab_git::pack_locator::PackLocationIter::open(
        &installed.idx_path,
        &installed.rev_path,
        pack_bytes.len() as u64,
    )
    .unwrap();
    let object_count = locations.object_count();
    let object_ids = locations
        .by_ref()
        .map(|location| location.unwrap().oid)
        .collect::<Vec<_>>();
    let kinds = crab_git::pack::object_kinds_from_git_dir(&git_dir, &object_ids).unwrap();
    let ordered_kinds = object_ids
        .iter()
        .map(|oid| *kinds.get(oid).unwrap())
        .collect::<Vec<_>>();
    let checksum = gix_hash::ObjectId::from_hex(installed.git_sha1.as_bytes()).unwrap();
    let locator =
        crab_git::pack_locator::encode_pack_kind_metadata(checksum, &ordered_kinds).unwrap();
    let pack = crab_metadata::capsule_protocol::CapsuleGitPack::new(
        Bytes::from(pack_bytes),
        Bytes::from(std::fs::read(&installed.idx_path).unwrap()),
        Bytes::from(std::fs::read(&installed.rev_path).unwrap()),
        Bytes::from(locator),
        installed.git_sha1,
        object_count,
    )
    .unwrap();
    (tip, pack)
}

struct XetShardFixture {
    content_size: u64,
    file_hash: MerkleHash,
    xorb_hash: MerkleHash,
    xorb_body: Bytes,
    chunk_hash: MerkleHash,
    chunk_size: u32,
    shard_hash: MerkleHash,
    shard_body: Bytes,
}

fn xet_file_shard(byte: u8, bloom: bool) -> XetShardFixture {
    use crab_xet::shard::{
        FileDataSequenceEntry, FileDataSequenceHeader, MDBFileInfo, MDBXorbInfo, ShardWriter,
        XorbChunkSequenceEntry, XorbChunkSequenceHeader,
    };
    use crab_xet::xorb::builder::{RunId, XorbBuilder};
    use crab_xet::xorb::format::Chunk;

    let content = Bytes::from(vec![byte; 1024]);
    let chunk = Chunk::new(content.clone());
    let mut builder = XorbBuilder::new();
    builder.push(&chunk, RunId(0)).unwrap();
    let xorb = builder.finalize().unwrap().remove(0);
    let mut writer = ShardWriter::new();
    writer
        .add_xorb(Arc::new(MDBXorbInfo {
            metadata: XorbChunkSequenceHeader::new(xorb.hash, 1, content.len()),
            chunks: vec![XorbChunkSequenceEntry::new(
                chunk.hash,
                content.len() as u32,
                0,
            )],
        }))
        .unwrap();
    writer
        .add_file(MDBFileInfo {
            metadata: FileDataSequenceHeader::new(chunk.hash, 1, false, false),
            segments: vec![FileDataSequenceEntry::new(
                xorb.hash,
                content.len() as u32,
                0,
                1,
            )],
            verification: Vec::new(),
            metadata_ext: None,
        })
        .unwrap();
    let (bytes, hash) = if bloom {
        writer
            .finalize_with_bloom(&[chunk.hash], &[chunk.hash])
            .unwrap()
    } else {
        writer.finalize().unwrap()
    };
    XetShardFixture {
        content_size: content.len() as u64,
        file_hash: chunk.hash,
        xorb_hash: xorb.hash,
        xorb_body: xorb.bytes,
        chunk_hash: chunk.hash,
        chunk_size: content.len() as u32,
        shard_hash: hash,
        shard_body: Bytes::from(bytes),
    }
}

#[tokio::test]
async fn downloaded_shards_load_plain_and_bloom_objects_as_native_xet_bodies() {
    for bloom in [false, true] {
        let fixture = xet_file_shard(55, bloom);
        let store = memory_store();
        let layout =
            crab_storage::StoreLayout::new(store.as_storage().clone(), "compact-format".to_owned());
        layout
            .store()
            .put(
                &layout.shard_path(&fixture.shard_hash),
                fixture.shard_body.clone(),
            )
            .await
            .unwrap();
        let target = tempfile::tempdir().unwrap();
        download_shards(
            &store,
            &layout,
            &[fixture.shard_hash.hex()],
            target.path(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        let shards = MDBShardFile::load_all_valid(target.path(), &new_shard_file_cache()).unwrap();
        let mut actual = Vec::new();
        shards[0].read_into_buffer(&mut actual).unwrap();
        assert_eq!(
            actual,
            crab_xet::shard_parse::strip_bloom_trailer(&fixture.shard_body)
        );
    }
}

#[tokio::test]
async fn downloaded_bloom_shards_authenticate_trailer_before_creating_native_files() {
    let fixture = xet_file_shard(55, true);
    let store = memory_store();
    let layout = crab_storage::StoreLayout::new(
        store.as_storage().clone(),
        "compact-corrupt-bloom".to_owned(),
    );
    let mut corrupt = fixture.shard_body.to_vec();
    let index = corrupt.len() - 13;
    corrupt[index] ^= 1;
    layout
        .store()
        .put(
            &layout.shard_path(&fixture.shard_hash),
            Bytes::from(corrupt),
        )
        .await
        .unwrap();
    let target = tempfile::tempdir().unwrap();
    let result = download_shards(
        &store,
        &layout,
        &[fixture.shard_hash.hex()],
        target.path(),
        &CancellationToken::new(),
    )
    .await;
    assert!(
        matches!(result, Err(CrabError::CorruptObject { .. }))
            && std::fs::read_dir(target.path()).unwrap().next().is_none()
    );
}

#[test]
fn parse_size_mib() {
    assert_eq!(parse_size_str("100MiB").unwrap(), 100 * 1024 * 1024);
}

#[test]
fn parse_size_mb() {
    assert_eq!(parse_size_str("50MB").unwrap(), 50_000_000);
}

#[test]
fn parse_size_gib() {
    assert_eq!(parse_size_str("1GiB").unwrap(), 1024 * 1024 * 1024);
}

#[test]
fn parse_size_bare_number() {
    assert_eq!(parse_size_str("4096").unwrap(), 4096);
}

#[test]
fn parse_size_invalid_errors() {
    assert!(parse_size_str("notanumber").is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn compact_empty_capsule_repository_is_noop() {
    for dry_run in [true, false] {
        let store = memory_store();
        let layout = crab_storage::StoreLayout::new(
            store.as_storage().clone(),
            "compact-empty-capsule".into(),
        );
        crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
            .await
            .unwrap();
        let result = run_compact(
            &CompactArgs {
                repo: "compact-empty-capsule".into(),
                bucket: "test-bucket".into(),
                dry_run,
                max_shard_size: DEFAULT_MAX_SHARD_SIZE,
            },
            &store,
        )
        .await
        .unwrap();
        assert_eq!(
            (
                result.dry_run,
                result.source_shards,
                result.compacted_shards
            ),
            (dry_run, 0, 0)
        );
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn compact_requires_capsule_root_before_reading_or_mutating_legacy_shards() {
    for dry_run in [false, true] {
        let observed = Arc::new(crab_storage::test_support::CountingObjectStore::new(
            Arc::new(InMemory::new()),
        ));
        let store = Store::new(observed.clone());
        let layout = crab_storage::StoreLayout::new(
            store.as_storage().clone(),
            "compact-root-required".to_owned(),
        );
        let fixtures = [xet_file_shard(71, false), xet_file_shard(72, true)];
        for fixture in &fixtures {
            layout
                .store()
                .put(
                    &layout.shard_path(&fixture.shard_hash),
                    fixture.shard_body.clone(),
                )
                .await
                .unwrap();
            layout
                .store()
                .put(
                    &layout.xorb_path(&fixture.xorb_hash),
                    fixture.xorb_body.clone(),
                )
                .await
                .unwrap();
        }
        let list_path = layout.repo_path("manifests/shard-list");
        let list = ShardList {
            generation: 1,
            entries: fixtures
                .iter()
                .map(|fixture| fixture.shard_hash.hex())
                .collect(),
        };
        store
            .put(&list_path, Bytes::from(serde_json::to_vec(&list).unwrap()))
            .await
            .unwrap();
        observed.reset();
        let result = run_compact(
            &CompactArgs {
                repo: "compact-root-required".into(),
                bucket: "test-bucket".into(),
                dry_run,
                max_shard_size: DEFAULT_MAX_SHARD_SIZE,
            },
            &store,
        )
        .await;
        let missing_root = matches!(result, Err(CrabError::NotFound { ref path }) if path == &layout.capsule_root_path().to_string());
        let read_list = observed
            .requests()
            .iter()
            .any(|request| request.location == list_path.as_ref());
        assert_eq!(
            (
                missing_root,
                observed.put_requests(),
                observed.multipart_starts(),
                read_list
            ),
            (true, 0, 0, false),
            "dry_run={dry_run}"
        );
    }
}

#[tokio::test]
async fn corrupt_capsule_root_never_falls_back_to_legacy_shard_list() {
    let store = memory_store();
    let repo = "org/corrupt-capsule-compact";
    let layout = crab_storage::StoreLayout::new(store.as_storage().clone(), repo.to_owned());
    store
        .put(
            &layout.capsule_root_path(),
            Bytes::from_static(b"corrupt root"),
        )
        .await
        .unwrap();
    let body = serde_json::to_vec(&ShardList {
        generation: 1,
        entries: vec![MerkleHash::from([1_u64; 4]).hex()],
    })
    .unwrap();
    store
        .put(&layout.repo_path("manifests/shard-list"), Bytes::from(body))
        .await
        .unwrap();

    let result = run_compact(
        &CompactArgs {
            repo: repo.to_owned(),
            bucket: "test-bucket".to_owned(),
            dry_run: true,
            max_shard_size: DEFAULT_MAX_SHARD_SIZE,
        },
        &store,
    )
    .await;

    assert!(result.is_err());
}

#[tokio::test]
async fn capsule_dry_run_uses_authenticated_catalog_without_legacy_shard_list() {
    let store = memory_store();
    let layout = crab_storage::StoreLayout::new(
        store.as_storage().clone(),
        "org/capsule-compact".to_owned(),
    );
    let root =
        crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
            .await
            .unwrap();
    let xorb = MerkleHash::from([1_u64; 4]).hex();
    let shard = MerkleHash::from([2_u64; 4]).hex();
    let file = MerkleHash::from([3_u64; 4]).hex();
    let mut catalog = crab_metadata::capsule_protocol::PointerCatalog::new();
    catalog
        .insert_xorb(
            xorb.clone(),
            crab_metadata::capsule_protocol::XorbCatalogEntry::new(
                1,
                "4".repeat(64),
                vec![crab_metadata::capsule_protocol::XorbChunkEntry::new(
                    "5".repeat(64),
                    1,
                )],
            ),
        )
        .unwrap();
    catalog
        .insert_shard(
            shard.clone(),
            crab_metadata::capsule_protocol::ShardCatalogEntry::new(1, vec![xorb]),
        )
        .unwrap();
    catalog
        .insert_file(
            file,
            crab_metadata::capsule_protocol::FileCatalogEntry::new(1, shard),
        )
        .unwrap();
    let pack = crab_metadata::capsule_protocol::CapsuleGitPack::new(
        Bytes::from_static(b"pack"),
        Bytes::from_static(b"index"),
        Bytes::from_static(b"reverse"),
        Bytes::from_static(b"locator"),
        "6".repeat(40),
        1,
    )
    .unwrap();
    let layer = crab_metadata::capsule_protocol::PackLayer::build(&pack).unwrap();
    layout
        .store()
        .put(
            &layout.capsule_pack_layer_path(layer.hash()),
            layer.bytes().clone(),
        )
        .await
        .unwrap();
    let checkpoint = crab_metadata::capsule_protocol::LayeredCheckpoint::build(
        root.record().root().generation(),
        root.record().digest(),
        vec![layer.source_descriptor().unwrap()],
        catalog,
        None,
    )
    .unwrap();
    let transaction_id = "7".repeat(64);
    let capsule = crab_metadata::capsule_protocol::CapsulePointer::new(
        "8".repeat(64),
        2,
        1,
        1,
        "0".repeat(64),
        0,
        vec![transaction_id.clone()],
        root.record().digest(),
    )
    .unwrap();
    crab_write::capsule_protocol::publish_ref_layered_checkpoint(
        &layout,
        root,
        &checkpoint,
        BTreeMap::from([("refs/heads/main".to_owned(), "9".repeat(40))]),
        BTreeMap::new(),
        BTreeMap::from([("refs/heads/main".to_owned(), transaction_id)]),
        vec![capsule],
    )
    .await
    .unwrap();

    let outcome = run_compact(
        &CompactArgs {
            repo: "org/capsule-compact".to_owned(),
            bucket: "test-bucket".to_owned(),
            dry_run: true,
            max_shard_size: DEFAULT_MAX_SHARD_SIZE,
        },
        &store,
    )
    .await
    .unwrap();

    assert!(outcome.dry_run);
    assert_eq!(outcome.source_shards, 1);
    assert!(matches!(
        store
            .get_with_etag(&layout.repo_path("manifests/shard-list"))
            .await,
        Err(CrabError::NotFound { .. })
    ));
}

#[tokio::test]
async fn capsule_compaction_publishes_verified_checkpoint_without_legacy_metadata() {
    for (cancel_at_commit, missing_files) in [(false, false), (true, false), (false, true)] {
        let observed = Arc::new(crab_storage::test_support::CountingObjectStore::new(
            Arc::new(InMemory::new()),
        ));
        let store =
            Store::new(observed.clone()).with_storage_scope(crab_types::storage::StorageScope {
                repo_prefix: "scoped/capsule-compact-apply".to_owned(),
                global_prefix: "scoped/capsule-compact-apply/.crab".to_owned(),
                source_repo: "org/capsule-compact-apply".to_owned(),
                scope_hash: "a".repeat(64),
            });
        let layout = crab_storage::StoreLayout::new(
            store.as_storage().clone(),
            "org/capsule-compact-apply".to_owned(),
        );
        let root =
            crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
                .await
                .unwrap();
        let fixture_a = xet_file_shard(11, true);
        let fixture_b = xet_file_shard(12, false);
        for fixture in [&fixture_a, &fixture_b] {
            layout
                .store()
                .put(
                    &layout.xorb_path(&fixture.xorb_hash),
                    fixture.xorb_body.clone(),
                )
                .await
                .unwrap();
            layout
                .store()
                .put(
                    &layout.shard_path(&fixture.shard_hash),
                    fixture.shard_body.clone(),
                )
                .await
                .unwrap();
        }
        let mut catalog = crab_metadata::capsule_protocol::PointerCatalog::new();
        for (index, fixture) in [&fixture_a, &fixture_b].into_iter().enumerate() {
            catalog
                .insert_xorb(
                    fixture.xorb_hash.hex(),
                    crab_metadata::capsule_protocol::XorbCatalogEntry::new(
                        fixture.xorb_body.len() as u64,
                        blake3::hash(&fixture.xorb_body).to_hex().to_string(),
                        vec![crab_metadata::capsule_protocol::XorbChunkEntry::new(
                            fixture.chunk_hash.hex(),
                            fixture.chunk_size,
                        )],
                    ),
                )
                .unwrap();
            catalog
                .insert_shard(
                    fixture.shard_hash.hex(),
                    crab_metadata::capsule_protocol::ShardCatalogEntry::new(
                        fixture.shard_body.len() as u64,
                        vec![fixture.xorb_hash.hex()],
                    ),
                )
                .unwrap();
            catalog
                .insert_file(
                    if missing_files {
                        format!("{:064x}", index + 1)
                    } else {
                        fixture.file_hash.hex()
                    },
                    crab_metadata::capsule_protocol::FileCatalogEntry::new(
                        fixture.content_size,
                        fixture.shard_hash.hex(),
                    ),
                )
                .unwrap();
        }
        let (tip, pack) = git_pack_fixture();
        let transaction = crab_metadata::capsule_protocol::CapsuleTransaction::new(
            root.record().digest(),
            vec![crab_metadata::capsule_protocol::CapsuleRefEdit::new(
                "refs/heads/main",
                None,
                Some(tip.clone()),
                None,
            )],
        )
        .unwrap();
        let visibility =
            crab_metadata::capsule_protocol::CapsuleVisibilityDelta::new(BTreeMap::from([(
                "refs/heads/main".to_owned(),
                crab_metadata::git_visibility::GitVisibilityEdit::from_replacement_objects(
                    None,
                    tip.clone(),
                    vec![tip],
                ),
            )]))
            .unwrap();
        let capsule = crab_metadata::capsule_protocol::Capsule::build(
            &transaction,
            vec![pack],
            vec![
                crab_metadata::capsule_protocol::CapsuleSection::new(
                    crab_metadata::capsule_protocol::CapsuleSectionKind::CatalogDelta,
                    catalog.encode_delta().unwrap(),
                ),
                crab_metadata::capsule_protocol::CapsuleSection::new(
                    crab_metadata::capsule_protocol::CapsuleSectionKind::VisibilityDelta,
                    visibility.encode().unwrap(),
                ),
            ],
        )
        .unwrap();
        crab_write::capsule_protocol::publish(&layout, root, &transaction, &capsule)
            .await
            .unwrap();

        let before = crab_metadata::capsule_protocol::load_root(&layout)
            .await
            .unwrap()
            .record()
            .digest()
            .to_owned();
        let cancel = CancellationToken::new();
        if cancel_at_commit {
            observed.cancel_after_put(layout.capsule_root_path(), cancel.clone());
        }
        let outcome = run_compact_with_cancel(
            &CompactArgs {
                repo: "org/capsule-compact-apply".to_owned(),
                bucket: "test-bucket".to_owned(),
                dry_run: false,
                max_shard_size: 1024 * 1024,
            },
            &store,
            &cancel,
        )
        .await;
        if missing_files {
            assert!(
                matches!(outcome, Err(CrabError::CorruptObject { .. })),
                "missing catalog files must fail: {outcome:?}"
            );
            assert_eq!(
                crab_metadata::capsule_protocol::load_root(&layout)
                    .await
                    .unwrap()
                    .record()
                    .digest(),
                before
            );
            continue;
        }
        let outcome = outcome.unwrap();
        assert_eq!(cancel.is_cancelled(), cancel_at_commit);

        assert_eq!(outcome.source_shards, 2);
        assert_eq!(outcome.compacted_shards, 1);
        let view = crab_read::capsule_protocol::open_view(
            &layout,
            crab_read::capsule_protocol::CapsuleReadLimits {
                max_capsule_bytes: MAX_CAPSULE_BYTES,
                max_frontier_bytes: MAX_CAPSULE_BYTES,
            },
        )
        .await
        .unwrap();
        let compacted = view.pointer_catalog().await.unwrap();
        assert_eq!(compacted.shards().len(), 1);
        assert_eq!(
            compacted
                .files()
                .get(&fixture_a.file_hash.hex())
                .unwrap()
                .shard_hash(),
            compacted
                .files()
                .get(&fixture_b.file_hash.hex())
                .unwrap()
                .shard_hash()
        );
        assert!(!compacted.shards().contains_key(&fixture_a.shard_hash.hex()));
        assert!(!compacted.shards().contains_key(&fixture_b.shard_hash.hex()));
        assert_eq!(compacted.xorbs().len(), 2);
        assert!(view.root().root().checkpoint().is_some());
        assert!(matches!(
            store
                .get_with_etag(&layout.repo_path("manifests/shard-list"))
                .await,
            Err(CrabError::NotFound { .. })
        ));
    }
}

#[test]
fn capsule_compaction_remaps_every_file_and_prunes_old_shards() {
    let old_shard_a = MerkleHash::from([1_u64; 4]).hex();
    let old_shard_b = MerkleHash::from([2_u64; 4]).hex();
    let new_shard = MerkleHash::from([3_u64; 4]);
    let file_a = MerkleHash::from([4_u64; 4]).hex();
    let file_b = MerkleHash::from([5_u64; 4]).hex();
    let xorb_a = MerkleHash::from([6_u64; 4]).hex();
    let xorb_b = MerkleHash::from([7_u64; 4]).hex();
    let mut current = crab_metadata::capsule_protocol::PointerCatalog::new();
    for (xorb, chunk, digest) in [
        (&xorb_a, "8".repeat(64), "9".repeat(64)),
        (&xorb_b, "a".repeat(64), "b".repeat(64)),
    ] {
        current
            .insert_xorb(
                xorb.clone(),
                crab_metadata::capsule_protocol::XorbCatalogEntry::new(
                    10,
                    digest,
                    vec![crab_metadata::capsule_protocol::XorbChunkEntry::new(
                        chunk, 10,
                    )],
                ),
            )
            .unwrap();
    }
    current
        .insert_shard(
            old_shard_a.clone(),
            crab_metadata::capsule_protocol::ShardCatalogEntry::new(10, vec![xorb_a.clone()]),
        )
        .unwrap();
    current
        .insert_shard(
            old_shard_b.clone(),
            crab_metadata::capsule_protocol::ShardCatalogEntry::new(10, vec![xorb_b.clone()]),
        )
        .unwrap();
    current
        .insert_file(
            file_a.clone(),
            crab_metadata::capsule_protocol::FileCatalogEntry::new(10, old_shard_a.clone()),
        )
        .unwrap();
    current
        .insert_file(
            file_b.clone(),
            crab_metadata::capsule_protocol::FileCatalogEntry::new(20, old_shard_b.clone()),
        )
        .unwrap();
    let compacted = [PreparedCompactedShard {
        hash: new_shard,
        body: Bytes::from_static(b"compacted"),
        files: vec![file_a.clone(), file_b.clone()],
        xorbs: vec![xorb_a.clone(), xorb_b.clone()],
    }];

    let replacement = compacted_pointer_catalog(&current, &compacted).unwrap();

    assert_eq!(replacement.shards().len(), 1);
    assert!(!replacement.shards().contains_key(&old_shard_a));
    assert!(!replacement.shards().contains_key(&old_shard_b));
    assert_eq!(
        replacement.files().get(&file_a).unwrap().shard_hash(),
        new_shard.hex()
    );
    assert_eq!(
        replacement.files().get(&file_b).unwrap().shard_hash(),
        new_shard.hex()
    );
    assert_eq!(replacement.xorbs().len(), 2);
}

#[test]
fn capsule_filter_keeps_only_authenticated_files_and_dependencies() {
    use crab_xet::shard::ShardWriter;

    let retained = xet_file_shard(31, false);
    let foreign = xet_file_shard(32, false);
    let mut writer = ShardWriter::new();
    for fixture in [&retained, &foreign] {
        let parsed = MDBMinimalShard::from_reader(
            &mut std::io::Cursor::new(&fixture.shard_body),
            true,
            true,
        )
        .unwrap();
        writer
            .add_xorb(Arc::new(crab_xet::shard::MDBXorbInfo::from(
                parsed.xorb(0).unwrap(),
            )))
            .unwrap();
        writer
            .add_file(crab_xet::shard::MDBFileInfo::from(parsed.file(0).unwrap()))
            .unwrap();
    }
    let (body, _) = writer.finalize().unwrap();
    let source_dir = tempfile::tempdir().unwrap();
    let output_dir = tempfile::tempdir().unwrap();
    let shard_file = MDBShardFile::write_out_from_reader(
        source_dir.path(),
        &mut std::io::Cursor::new(body),
        &new_shard_file_cache(),
    )
    .unwrap();

    let filtered = select_catalog_files(
        &[shard_file],
        output_dir.path(),
        &BTreeSet::from([retained.file_hash.hex()]),
    )
    .unwrap();

    assert_eq!(filtered.len(), 1);
    let mut body = Vec::new();
    filtered[0].read_into_buffer(&mut body).unwrap();
    let parsed = MDBMinimalShard::from_reader(&mut std::io::Cursor::new(body), true, true).unwrap();
    assert_eq!(parsed.num_files(), 1);
    assert_eq!(parsed.file(0).unwrap().file_hash(), retained.file_hash);
    assert_eq!(parsed.num_xorb(), 1);
    assert_eq!(parsed.xorb(0).unwrap().xorb_hash(), retained.xorb_hash);
}
