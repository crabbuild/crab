use super::*;
use object_store::memory::InMemory;
use std::collections::BTreeMap;
use std::io::Write as _;
use std::process::{Command, Stdio};

fn hash(seed: u8) -> MerkleHash {
    MerkleHash::from([seed; 32])
}

fn xorb_info(xorb_hash: MerkleHash, chunks: &[(MerkleHash, u32)]) -> Arc<MDBXorbInfo> {
    let mut offset = 0;
    let entries = chunks
        .iter()
        .map(|(chunk_hash, size)| {
            let entry = XorbChunkSequenceEntry::new(*chunk_hash, *size, offset);
            offset += *size;
            entry
        })
        .collect::<Vec<_>>();
    Arc::new(MDBXorbInfo {
        metadata: XorbChunkSequenceHeader::new(xorb_hash, entries.len(), offset),
        chunks: entries,
    })
}

fn file_info(file_hash: MerkleHash, xorb_hash: MerkleHash, size: u32) -> MDBFileInfo {
    MDBFileInfo {
        metadata: crab_xet::shard::FileDataSequenceHeader::new(file_hash, 1, false, false),
        segments: vec![FileDataSequenceEntry::new(xorb_hash, size, 0, 1)],
        verification: Vec::new(),
        metadata_ext: None,
    }
}

async fn put_xorb(
    store: &Store,
    router: &StoreLayout,
    chunks: &[crab_xet::xorb::format::Chunk],
) -> crab_xet::xorb::builder::XorbResult {
    let mut builder = crab_xet::xorb::builder::XorbBuilder::new();
    for chunk in chunks {
        builder
            .push(chunk, crab_xet::xorb::builder::RunId(0))
            .unwrap();
    }
    let xorb = builder.finalize().unwrap().remove(0);
    store
        .put(
            &router.xorb_path(&xorb.hash),
            Bytes::from(xorb.bytes.clone()),
        )
        .await
        .unwrap();
    xorb
}

#[tokio::test]
async fn executor_shared_destinations_preserve_every_source_chunk() {
    use crate::optimize::xorbs::executor::{ExecutorConfig, execute};
    use crate::optimize::xorbs::profile::Profile;
    use crab_xet::xorb::format::Chunk;

    let store = Store::new(Arc::new(InMemory::new()));
    let router = StoreLayout::new(store.clone(), "org/shared-rewrite".to_owned());
    let shared = Chunk::new(Bytes::from(vec![1_u8; 1024]));
    let a = Chunk::new(Bytes::from(vec![2_u8; 1024]));
    let b = Chunk::new(Bytes::from(vec![3_u8; 1024]));
    let sources = [
        put_xorb(&store, &router, &[a, shared.clone()]).await,
        put_xorb(&store, &router, &[shared, b]).await,
    ];
    let directory = tempfile::tempdir().unwrap();
    let journal = OptimizeXorbsJournal::open(&directory.path().join("journal.db")).unwrap();
    let profile = Profile::code();
    journal
        .start_run("shared-rewrite", &profile.to_json())
        .unwrap();
    for source in &sources {
        journal
            .insert_source("shared-rewrite", &source.hash.hex())
            .unwrap();
    }
    let cancel = CancellationToken::new();
    execute(
        &journal,
        "shared-rewrite",
        &profile,
        &ExecutorConfig::default(),
        &cancel,
        &store,
        &router,
        None,
    )
    .await
    .unwrap();
    let (mapping, _, _) = build_mapping(&journal, "shared-rewrite").unwrap();
    assert_eq!(
        mapping[&sources[0].hash.hex()],
        mapping[&sources[1].hash.hex()]
    );
    let loaded = load_mapping(&store, &router, &mapping, &cancel)
        .await
        .unwrap();
    assert_eq!(loaded.destination_infos.len(), 1);
    assert_eq!(
        loaded
            .destination_infos
            .values()
            .next()
            .unwrap()
            .chunks
            .len(),
        3
    );
    for source in sources {
        let parser = XorbParser::parse(Bytes::from(source.bytes)).unwrap();
        let placement = &loaded.sources[&source.hash];
        for (index, chunk) in placement.chunks.iter().enumerate() {
            let target = placement.refs[&chunk.hash];
            let (destination, _) = load_xorb(&store, &router, target.xorb_hash).await.unwrap();
            assert_eq!(
                destination.get_chunk(target.chunk_index).unwrap().data,
                parser.get_chunk(index as u32).unwrap().data,
            );
        }
    }
}

#[tokio::test]
async fn failed_executor_page_resumes_with_complete_destination_coverage() {
    use crate::optimize::xorbs::executor::{ExecutorConfig, execute};
    use crate::optimize::xorbs::profile::Profile;
    use crab_xet::xorb::format::Chunk;

    let store = Store::new(Arc::new(InMemory::new()));
    let router = StoreLayout::new(store.clone(), "org/rewrite-resume".to_owned());
    let shared = Chunk::new(Bytes::from(vec![11_u8; 1024]));
    let sources = [
        put_xorb(
            &store,
            &router,
            &[Chunk::new(Bytes::from(vec![12_u8; 1024])), shared.clone()],
        )
        .await,
        put_xorb(
            &store,
            &router,
            &[shared, Chunk::new(Bytes::from(vec![13_u8; 1024]))],
        )
        .await,
    ];
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("journal.db");
    let profile = Profile::code();
    {
        let journal = OptimizeXorbsJournal::open(&path).unwrap();
        journal
            .start_run("rewrite-resume", &profile.to_json())
            .unwrap();
        for source in &sources {
            journal
                .insert_source("rewrite-resume", &source.hash.hex())
                .unwrap();
        }
    }
    let last_source = sources
        .iter()
        .map(|source| source.hash.hex())
        .max()
        .unwrap();
    {
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute_batch(&format!(
                "CREATE TRIGGER fail_completion BEFORE UPDATE OF status ON sources
             WHEN NEW.status = 'done' AND NEW.src_xorb = '{last_source}'
             BEGIN SELECT RAISE(ABORT, 'injected source completion failure'); END;"
            ))
            .unwrap();
    }
    let cancel = CancellationToken::new();
    let journal = OptimizeXorbsJournal::open(&path).unwrap();
    let result = execute(
        &journal,
        "rewrite-resume",
        &profile,
        &ExecutorConfig::default(),
        &cancel,
        &store,
        &router,
        None,
    )
    .await;
    let error = result.unwrap_err();
    let CrabError::Io(error) = error else {
        panic!("journal failures must preserve the SQLite source");
    };
    assert!(
        std::error::Error::source(error.get_ref().unwrap())
            .unwrap()
            .is::<rusqlite::Error>()
    );
    let counts = journal.count_by_status("rewrite-resume").unwrap();
    assert_eq!((counts.done, counts.pending), (0, 2));
    drop(journal);
    {
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection
            .execute_batch("DROP TRIGGER fail_completion;")
            .unwrap();
    }
    let journal = OptimizeXorbsJournal::open(&path).unwrap();
    execute(
        &journal,
        "rewrite-resume",
        &profile,
        &ExecutorConfig::default(),
        &cancel,
        &store,
        &router,
        None,
    )
    .await
    .unwrap();
    let (mapping, _, _) = build_mapping(&journal, "rewrite-resume").unwrap();
    let loaded = load_mapping(&store, &router, &mapping, &cancel)
        .await
        .unwrap();
    assert_eq!(loaded.sources.len(), 2);
    assert_eq!(loaded.destination_infos.len(), 1);
}

#[tokio::test]
async fn legacy_partial_page_resume_preserves_unrelated_completed_mapping() {
    use crate::optimize::xorbs::executor::{ExecutorConfig, execute};
    use crate::optimize::xorbs::profile::Profile;
    use crab_xet::xorb::format::Chunk;

    for prior_state in ["partial", "already-resumed", "complete"] {
        let store = Store::new(Arc::new(InMemory::new()));
        let router = StoreLayout::new(store.clone(), "org/legacy-resume".to_owned());
        let a = Chunk::new(Bytes::from(vec![21_u8; 1024]));
        let shared = Chunk::new(Bytes::from(vec![22_u8; 1024]));
        let b = Chunk::new(Bytes::from(vec![23_u8; 1024]));
        let c = Chunk::new(Bytes::from(vec![24_u8; 1024]));
        let d = Chunk::new(Bytes::from(vec![25_u8; 1024]));
        let sources = [
            put_xorb(&store, &router, &[a.clone(), shared.clone()]).await,
            put_xorb(&store, &router, &[shared.clone(), b.clone()]).await,
            put_xorb(&store, &router, &[c.clone(), d.clone()]).await,
        ];
        let shared_destination = put_xorb(&store, &router, &[a, shared, b]).await;
        let unrelated_destination = put_xorb(&store, &router, &[d, c]).await;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("journal.db");
        let profile = Profile::code();
        let unrelated_mapping = serde_json::to_string(&[unrelated_destination.hash.hex()]).unwrap();
        {
            let journal = OptimizeXorbsJournal::open(&path).unwrap();
            journal
                .start_run("legacy-resume", &profile.to_json())
                .unwrap();
            for source in &sources {
                journal
                    .insert_source("legacy-resume", &source.hash.hex())
                    .unwrap();
            }
            // The released writer committed these rows individually. Its last
            // page can be partial even though the shared body is fully uploaded.
            journal
                .update_source_status(
                    "legacy-resume",
                    &sources[0].hash.hex(),
                    SourceStatus::Done,
                    Some(&serde_json::to_string(&[shared_destination.hash.hex()]).unwrap()),
                )
                .unwrap();
            journal
                .update_source_status(
                    "legacy-resume",
                    &sources[2].hash.hex(),
                    SourceStatus::Done,
                    Some(&unrelated_mapping),
                )
                .unwrap();
        }
        {
            let connection = rusqlite::Connection::open(&path).unwrap();
            connection
                .execute(
                    "UPDATE runs SET schema_ver = 1 WHERE run_id = 'legacy-resume'",
                    [],
                )
                .unwrap();
        }
        let journal = OptimizeXorbsJournal::open(&path).unwrap();
        let cancel = CancellationToken::new();
        if prior_state == "already-resumed" {
            execute(
                &journal,
                "legacy-resume",
                &profile,
                &ExecutorConfig::default(),
                &cancel,
                &store,
                &router,
                None,
            )
            .await
            .unwrap();
        } else if prior_state == "complete" {
            journal
                .update_source_status(
                    "legacy-resume",
                    &sources[1].hash.hex(),
                    SourceStatus::Done,
                    Some(&serde_json::to_string(&[shared_destination.hash.hex()]).unwrap()),
                )
                .unwrap();
        }
        let run = journal.active_run().unwrap().unwrap();
        prepare_resume(&journal, &run, &store, &router, &cancel)
            .await
            .unwrap();
        drop(journal);
        let journal = OptimizeXorbsJournal::open(&path).unwrap();
        let run = journal.active_run().unwrap().unwrap();
        assert_eq!(
            run.schema_ver,
            crate::optimize::xorbs::journal::SCHEMA_VERSION
        );
        // Reopening after migration must not replay already repaired work.
        prepare_resume(&journal, &run, &store, &router, &cancel)
            .await
            .unwrap();
        execute(
            &journal,
            "legacy-resume",
            &profile,
            &ExecutorConfig::default(),
            &cancel,
            &store,
            &router,
            None,
        )
        .await
        .unwrap();
        let (mapping, _, _) = build_mapping(&journal, "legacy-resume").unwrap();
        let loaded = load_mapping(&store, &router, &mapping, &cancel)
            .await
            .unwrap();
        assert_eq!(
            mapping[&sources[2].hash.hex()],
            vec![unrelated_destination.hash.hex()]
        );
        for source in sources {
            let original = XorbParser::parse(Bytes::from(source.bytes)).unwrap();
            for (index, chunk) in loaded.sources[&source.hash].chunks.iter().enumerate() {
                let target = loaded.sources[&source.hash].refs[&chunk.hash];
                let (destination, _) = load_xorb(&store, &router, target.xorb_hash).await.unwrap();
                assert_eq!(
                    destination.get_chunk(target.chunk_index).unwrap().data,
                    original.get_chunk(index as u32).unwrap().data
                );
            }
        }
    }
}

#[tokio::test]
async fn mapping_rejects_uncovered_or_ambiguous_destination_chunks() {
    use crab_xet::xorb::format::Chunk;

    let store = Store::new(Arc::new(InMemory::new()));
    let router = StoreLayout::new(store.clone(), "org/invalid-rewrite".to_owned());
    let a = Chunk::new(Bytes::from(vec![4_u8; 1024]));
    let b = Chunk::new(Bytes::from(vec![5_u8; 1024]));
    let foreign = Chunk::new(Bytes::from(vec![6_u8; 1024]));
    let source_a = put_xorb(&store, &router, std::slice::from_ref(&a)).await;
    let source_b = put_xorb(&store, &router, std::slice::from_ref(&b)).await;
    let merged = put_xorb(&store, &router, &[a.clone(), b.clone()]).await;
    let extra = put_xorb(&store, &router, &[a, b, foreign]).await;
    for (case, a_destinations, b_destinations) in [
        ("foreign chunk", vec![extra.hash], vec![extra.hash]),
        (
            "source not mapped to destination",
            vec![merged.hash],
            vec![source_b.hash],
        ),
        (
            "missing source chunk",
            vec![source_b.hash],
            vec![source_b.hash],
        ),
        (
            "ambiguous placement",
            vec![source_a.hash, merged.hash],
            vec![merged.hash],
        ),
    ] {
        let mapping = HashMap::from([
            (
                source_a.hash.hex(),
                a_destinations.iter().map(MerkleHash::hex).collect(),
            ),
            (
                source_b.hash.hex(),
                b_destinations.iter().map(MerkleHash::hex).collect(),
            ),
        ]);
        assert!(
            matches!(
                load_mapping(&store, &router, &mapping, &CancellationToken::new()).await,
                Err(CrabError::CorruptObject { .. })
            ),
            "{case} must fail closed",
        );
    }
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

#[test]
fn cas_repeat_noop_when_first_succeeded() {
    let outcome = ReconcileOutcome {
        entries_updated: 10,
        entries_unchanged: 5,
        shards_uploaded: 1,
        shard_bytes: 256,
        cas_first_attempt: true,
        cas_attempts: 1,
    };
    assert!(is_cas_repeat_noop(&outcome));
}

#[test]
fn cas_repeat_not_noop_when_first_failed() {
    let outcome = ReconcileOutcome {
        entries_updated: 10,
        entries_unchanged: 5,
        shards_uploaded: 1,
        shard_bytes: 256,
        cas_first_attempt: false,
        cas_attempts: 3,
    };
    assert!(!is_cas_repeat_noop(&outcome));
}

#[tokio::test]
async fn finalize_requires_capsule_root_before_reading_any_mapping() {
    for has_mapping in [false, true] {
        let workspace = tempfile::tempdir().unwrap();
        let journal = OptimizeXorbsJournal::open(&workspace.path().join("journal.db")).unwrap();
        journal.start_run("root-required", "{}").unwrap();
        if has_mapping {
            let source = hash(51).hex();
            journal.insert_source("root-required", &source).unwrap();
            journal
                .update_source_status(
                    "root-required",
                    &source,
                    SourceStatus::Done,
                    Some(&serde_json::to_string(&[hash(52).hex()]).unwrap()),
                )
                .unwrap();
        }
        let observed = Arc::new(crab_storage::test_support::CountingObjectStore::new(
            Arc::new(InMemory::new()),
        ));
        let store = Store::new(observed.clone());
        let router = StoreLayout::new(store.clone(), "org/root-required".to_owned());
        let layout = crab_storage::StoreLayout::new(
            store.as_storage().clone(),
            router.repo_prefix().to_owned(),
        );
        let result = finalize(
            &journal,
            "root-required",
            &store,
            &router,
            &CancellationToken::new(),
        )
        .await;
        let missing_root = matches!(result, Err(CrabError::NotFound { ref path }) if path == &layout.capsule_root_path().to_string());
        let only_root_read = observed
            .requests()
            .iter()
            .all(|request| request.location == layout.capsule_root_path().as_ref());
        assert_eq!(
            (missing_root, only_root_read, observed.put_requests()),
            (true, true, 0),
            "has_mapping={has_mapping}"
        );
    }
}

#[tokio::test]
async fn finalize_with_empty_dest_lists_counts_zero_updates() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.db");
    let journal = OptimizeXorbsJournal::open(&path).unwrap();

    journal.start_run("test-empty", "{}").unwrap();
    journal.insert_source("test-empty", "xorb-aaa").unwrap();
    journal
        .update_source_status("test-empty", "xorb-aaa", SourceStatus::Done, Some("[]"))
        .unwrap();

    let store = Store::new(Arc::new(InMemory::new()));
    let router = StoreLayout::new(store.clone(), "org/empty-mappings".to_owned());
    let layout =
        crab_storage::StoreLayout::new(store.as_storage().clone(), router.repo_prefix().to_owned());
    crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
        .await
        .unwrap();
    let outcome = finalize(
        &journal,
        "test-empty",
        &store,
        &router,
        &CancellationToken::new(),
    )
    .await
    .unwrap();

    assert_eq!(outcome.entries_updated, 0);
    assert_eq!(outcome.entries_unchanged, 0);
    assert_eq!(outcome.shards_uploaded, 0);
}

#[test]
fn build_mapping_extracts_src_to_dest() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("journal.db");
    let journal = OptimizeXorbsJournal::open(&path).unwrap();

    journal.start_run("map-test", "{}").unwrap();
    journal.insert_source("map-test", "src-a").unwrap();
    journal.insert_source("map-test", "src-b").unwrap();
    journal.insert_source("map-test", "src-c").unwrap();

    journal
        .update_source_status(
            "map-test",
            "src-a",
            SourceStatus::Done,
            Some(r#"["d1","d2"]"#),
        )
        .unwrap();
    journal
        .update_source_status("map-test", "src-b", SourceStatus::Done, Some("[]"))
        .unwrap();
    journal
        .mark_corrupt("map-test", "src-c", "hash", "bad")
        .unwrap();

    let (mapping, updated, unchanged) = build_mapping(&journal, "map-test").unwrap();

    assert_eq!(mapping.len(), 1);
    assert_eq!(mapping["src-a"], vec!["d1", "d2"]);
    assert_eq!(updated, 1);
    assert_eq!(unchanged, 1);
}

#[test]
fn rewrite_coalesces_deduplicated_destination_chunks() {
    let source_hash = hash(1);
    let destination_hash = hash(2);
    let chunk_a = hash(3);
    let chunk_b = hash(4);
    let source = SourcePlacement {
        chunks: vec![
            SourceChunk {
                hash: chunk_a,
                size: 4,
            },
            SourceChunk {
                hash: chunk_b,
                size: 8,
            },
            SourceChunk {
                hash: chunk_a,
                size: 4,
            },
        ],
        refs: HashMap::from([
            (
                chunk_a,
                XorbRef {
                    xorb_hash: destination_hash,
                    chunk_index: 0,
                    uncompressed_size: 4,
                },
            ),
            (
                chunk_b,
                XorbRef {
                    xorb_hash: destination_hash,
                    chunk_index: 1,
                    uncompressed_size: 8,
                },
            ),
        ]),
    };
    let mapping = LoadedMapping {
        sources: HashMap::from([(source_hash, source)]),
        source_catalog: HashMap::new(),
        destination_infos: HashMap::new(),
        destination_catalog: HashMap::new(),
    };
    let file = MDBFileInfo {
        metadata: crab_xet::shard::FileDataSequenceHeader::new(hash(5), 1, false, false),
        segments: vec![FileDataSequenceEntry::new(source_hash, 16, 0, 3)],
        verification: Vec::new(),
        metadata_ext: None,
    };

    let (rewritten, changed) = rewrite_file_info(&file, &mapping).unwrap();

    assert!(changed);
    assert_eq!(rewritten.metadata.num_entries, 2);
    assert_eq!(rewritten.segments[0].xorb_hash, destination_hash);
    assert_eq!(rewritten.segments[0].chunk_index_start, 0);
    assert_eq!(rewritten.segments[0].chunk_index_end, 2);
    assert_eq!(rewritten.segments[0].unpacked_segment_bytes, 12);
    assert_eq!(rewritten.segments[1].chunk_index_start, 0);
    assert_eq!(rewritten.segments[1].chunk_index_end, 1);
    assert_eq!(rewritten.segments[1].unpacked_segment_bytes, 4);
}

#[test]
fn rewritten_shard_drops_source_xorb_metadata() {
    let source_hash = hash(10);
    let destination_hash = hash(11);
    let chunk_hash = hash(12);
    let file_hash = hash(13);
    let source_info = xorb_info(source_hash, &[(chunk_hash, 16)]);
    let destination_info = xorb_info(destination_hash, &[(chunk_hash, 16)]);
    let mut writer = ShardWriter::new();
    writer.add_xorb(source_info).unwrap();
    writer
        .add_file(file_info(file_hash, source_hash, 16))
        .unwrap();
    let (body, old_hash) = writer.finalize().unwrap();

    let mapping = LoadedMapping {
        sources: HashMap::from([(
            source_hash,
            SourcePlacement {
                chunks: vec![SourceChunk {
                    hash: chunk_hash,
                    size: 16,
                }],
                refs: HashMap::from([(
                    chunk_hash,
                    XorbRef {
                        xorb_hash: destination_hash,
                        chunk_index: 0,
                        uncompressed_size: 16,
                    },
                )]),
            },
        )]),
        source_catalog: HashMap::new(),
        destination_infos: HashMap::from([(destination_hash, destination_info)]),
        destination_catalog: HashMap::new(),
    };

    let rewrite = rewrite_shard(
        &Bytes::from(body),
        old_hash,
        &mapping,
        &HashSet::from([file_hash]),
    )
    .unwrap()
    .unwrap();
    let reader = ShardReader::from_bytes(rewrite.bytes, rewrite.new_hash);

    assert!(reader.get_xorb_info(&source_hash).unwrap().is_none());
    assert!(reader.get_xorb_info(&destination_hash).unwrap().is_some());
    assert_eq!(
        reader.get_file_info(&file_hash).unwrap().unwrap().segments[0].xorb_hash,
        destination_hash
    );
}

#[test]
fn capsule_catalog_replaces_shard_and_prunes_source_xorb() {
    use crab_metadata::capsule_protocol::{
        FileCatalogEntry, PointerCatalog, ShardCatalogEntry, XorbCatalogEntry, XorbChunkEntry,
    };

    let old_shard = hash(20);
    let new_shard = hash(21);
    let source_xorb = hash(22);
    let destination_xorb = hash(23);
    let file = hash(24);
    let chunk = hash(25);
    let source_entry = XorbCatalogEntry::new(
        10,
        hash(26).hex(),
        vec![XorbChunkEntry::new(chunk.hex(), 10)],
    );
    let destination_entry = XorbCatalogEntry::new(
        9,
        hash(27).hex(),
        vec![XorbChunkEntry::new(chunk.hex(), 10)],
    );
    let mut current = PointerCatalog::new();
    current
        .insert_xorb(source_xorb.hex(), source_entry)
        .unwrap();
    current
        .insert_shard(
            old_shard.hex(),
            ShardCatalogEntry::new(20, vec![source_xorb.hex()]),
        )
        .unwrap();
    current
        .insert_file(file.hex(), FileCatalogEntry::new(10, old_shard.hex()))
        .unwrap();
    let mapping = LoadedMapping {
        sources: HashMap::new(),
        source_catalog: HashMap::new(),
        destination_infos: HashMap::new(),
        destination_catalog: HashMap::from([(destination_xorb, destination_entry.clone())]),
    };
    let plan = ReconcilePlan {
        replacements: vec![ShardRewrite {
            new_hash: new_shard,
            bytes: Bytes::from_static(b"replacement"),
            file_entries: vec![FileIndexEntry {
                file_hash: file,
                recipe_hash: [0; 32],
                shard_hash: new_shard,
            }],
            xorbs: vec![destination_xorb],
        }],
        replaced_sources: HashSet::from([old_shard]),
        final_shards: vec![new_shard],
        file_entries: vec![FileIndexEntry {
            file_hash: file,
            recipe_hash: [0; 32],
            shard_hash: new_shard,
        }],
    };

    let replacement = capsule_replacement_catalog(&current, &plan, &mapping).unwrap();

    assert_eq!(
        replacement.files()[&file.hex()].shard_hash(),
        new_shard.hex()
    );
    assert!(replacement.shards().contains_key(&new_shard.hex()));
    assert!(!replacement.shards().contains_key(&old_shard.hex()));
    assert_eq!(
        replacement.xorbs()[&destination_xorb.hex()],
        destination_entry
    );
    assert!(!replacement.xorbs().contains_key(&source_xorb.hex()));
}

#[test]
fn capsule_rewrite_strips_foreign_files_from_shared_shard() {
    let source_xorb = hash(30);
    let destination_xorb = hash(31);
    let foreign_xorb = hash(32);
    let selected_file = hash(33);
    let foreign_file = hash(34);
    let selected_chunk = hash(35);
    let foreign_chunk = hash(36);
    let mut writer = ShardWriter::new();
    writer
        .add_xorb(xorb_info(source_xorb, &[(selected_chunk, 10)]))
        .unwrap();
    writer
        .add_xorb(xorb_info(foreign_xorb, &[(foreign_chunk, 12)]))
        .unwrap();
    writer
        .add_file(file_info(selected_file, source_xorb, 10))
        .unwrap();
    writer
        .add_file(file_info(foreign_file, foreign_xorb, 12))
        .unwrap();
    let (body, old_hash) = writer.finalize().unwrap();
    let mapping = LoadedMapping {
        sources: HashMap::from([(
            source_xorb,
            SourcePlacement {
                chunks: vec![SourceChunk {
                    hash: selected_chunk,
                    size: 10,
                }],
                refs: HashMap::from([(
                    selected_chunk,
                    XorbRef {
                        xorb_hash: destination_xorb,
                        chunk_index: 0,
                        uncompressed_size: 10,
                    },
                )]),
            },
        )]),
        source_catalog: HashMap::new(),
        destination_infos: HashMap::from([(
            destination_xorb,
            xorb_info(destination_xorb, &[(selected_chunk, 10)]),
        )]),
        destination_catalog: HashMap::new(),
    };

    let rewrite = rewrite_shard(
        &Bytes::from(body),
        old_hash,
        &mapping,
        &HashSet::from([selected_file]),
    )
    .unwrap()
    .unwrap();
    let reader = ShardReader::from_bytes(rewrite.bytes, rewrite.new_hash);

    assert!(reader.get_file_info(&selected_file).unwrap().is_some());
    assert!(reader.get_file_info(&foreign_file).unwrap().is_none());
    assert!(reader.get_xorb_info(&destination_xorb).unwrap().is_some());
    assert!(reader.get_xorb_info(&foreign_xorb).unwrap().is_none());
}

#[tokio::test]
async fn capsule_finalize_publishes_rewritten_xorbs_without_legacy_manifest() {
    for cancel_at_commit in [false, true] {
        use crab_metadata::capsule_protocol::{
            Capsule, CapsuleRefEdit, CapsuleSection, CapsuleSectionKind, CapsuleTransaction,
            CapsuleVisibilityDelta, FileCatalogEntry, PointerCatalog, ShardCatalogEntry,
            XorbCatalogEntry, XorbChunkEntry,
        };
        use crab_xet::xorb::builder::{RunId, XorbBuilder};
        use crab_xet::xorb::format::Chunk;

        let observed = Arc::new(crab_storage::test_support::CountingObjectStore::new(
            Arc::new(InMemory::new()),
        ));
        let store = Store::new(observed.clone());
        let router = StoreLayout::new(store.clone(), "org/optimize-v2".to_owned());
        let layout = crab_storage::StoreLayout::new(
            store.as_storage().clone(),
            router.repo_prefix().to_owned(),
        );
        let root =
            crab_write::capsule_protocol::initialize(&layout, &hash(40).hex(), "refs/heads/main")
                .await
                .unwrap();

        let chunk_a = Chunk::new(Bytes::from(vec![41_u8; 1024]));
        let chunk_b = Chunk::new(Bytes::from(vec![42_u8; 1024]));
        let mut source_builder = XorbBuilder::new();
        source_builder.push(&chunk_a, RunId(0)).unwrap();
        source_builder.push(&chunk_b, RunId(0)).unwrap();
        let mut source_results = source_builder.finalize().unwrap();
        assert_eq!(source_results.len(), 1);
        let source = source_results.remove(0);
        let mut destination_a_builder = XorbBuilder::new();
        destination_a_builder.push(&chunk_a, RunId(0)).unwrap();
        let destination_a = destination_a_builder.finalize().unwrap().remove(0);
        let mut destination_b_builder = XorbBuilder::new();
        destination_b_builder.push(&chunk_b, RunId(0)).unwrap();
        let destination_b = destination_b_builder.finalize().unwrap().remove(0);
        assert_ne!(source.hash, destination_a.hash);
        assert_ne!(source.hash, destination_b.hash);

        let file_hash = hash(43);
        let source_info = xorb_info(source.hash, &[(chunk_a.hash, 1024), (chunk_b.hash, 1024)]);
        let mut shard_writer = ShardWriter::new();
        shard_writer.add_xorb(source_info).unwrap();
        shard_writer
            .add_file(MDBFileInfo {
                metadata: crab_xet::shard::FileDataSequenceHeader::new(file_hash, 1, false, false),
                segments: vec![FileDataSequenceEntry::new(source.hash, 2048, 0, 2)],
                verification: Vec::new(),
                metadata_ext: None,
            })
            .unwrap();
        let (shard_body, shard_hash) = shard_writer.finalize().unwrap();
        for (xorb_hash, body) in [
            (source.hash, source.bytes.clone()),
            (destination_a.hash, destination_a.bytes.clone()),
            (destination_b.hash, destination_b.bytes.clone()),
        ] {
            store
                .put(&layout.xorb_path(&xorb_hash), Bytes::from(body))
                .await
                .unwrap();
        }
        store
            .put(
                &layout.shard_path(&shard_hash),
                Bytes::from(shard_body.clone()),
            )
            .await
            .unwrap();

        let mut catalog = PointerCatalog::new();
        catalog
            .insert_xorb(
                source.hash.hex(),
                XorbCatalogEntry::new(
                    source.bytes.len() as u64,
                    blake3::hash(&source.bytes).to_hex().to_string(),
                    vec![
                        XorbChunkEntry::new(chunk_a.hash.hex(), 1024),
                        XorbChunkEntry::new(chunk_b.hash.hex(), 1024),
                    ],
                ),
            )
            .unwrap();
        catalog
            .insert_shard(
                shard_hash.hex(),
                ShardCatalogEntry::new(shard_body.len() as u64, vec![source.hash.hex()]),
            )
            .unwrap();
        catalog
            .insert_file(
                file_hash.hex(),
                FileCatalogEntry::new(2048, shard_hash.hex()),
            )
            .unwrap();

        let (tip, pack) = git_pack_fixture();
        let transaction = CapsuleTransaction::new(
            root.record().digest(),
            vec![CapsuleRefEdit::new(
                "refs/heads/main",
                None,
                Some(tip.clone()),
                None,
            )],
        )
        .unwrap();
        let visibility = CapsuleVisibilityDelta::new(BTreeMap::from([(
            "refs/heads/main".to_owned(),
            crab_metadata::git_visibility::GitVisibilityEdit::from_replacement_objects(
                None,
                tip.clone(),
                vec![tip],
            ),
        )]))
        .unwrap();
        let capsule = Capsule::build(
            &transaction,
            vec![pack],
            vec![
                CapsuleSection::new(
                    CapsuleSectionKind::CatalogDelta,
                    catalog.encode_delta().unwrap(),
                ),
                CapsuleSection::new(
                    CapsuleSectionKind::VisibilityDelta,
                    visibility.encode().unwrap(),
                ),
            ],
        )
        .unwrap();
        crab_write::capsule_protocol::publish(&layout, root, &transaction, &capsule)
            .await
            .unwrap();

        let journal_dir = tempfile::tempdir().unwrap();
        let journal = OptimizeXorbsJournal::open(&journal_dir.path().join("journal.db")).unwrap();
        journal.start_run("capsule-finalize", "{}").unwrap();
        journal
            .insert_source("capsule-finalize", &source.hash.hex())
            .unwrap();
        journal
            .update_source_status(
                "capsule-finalize",
                &source.hash.hex(),
                SourceStatus::Done,
                Some(
                    &serde_json::to_string(&vec![
                        destination_a.hash.hex(),
                        destination_b.hash.hex(),
                    ])
                    .unwrap(),
                ),
            )
            .unwrap();

        let cancel = CancellationToken::new();
        if cancel_at_commit {
            observed.cancel_after_put(layout.capsule_root_path(), cancel.clone());
        }
        let outcome = finalize(&journal, "capsule-finalize", &store, &router, &cancel)
            .await
            .unwrap();

        assert_eq!(cancel.is_cancelled(), cancel_at_commit);
        assert_eq!(outcome.entries_updated, 1);
        assert_eq!(outcome.shards_uploaded, 1);
        let view = crab_read::capsule_protocol::open_view(
            &layout,
            crab_read::capsule_protocol::CapsuleReadLimits {
                max_capsule_bytes: MAX_CAPSULE_BYTES,
                max_frontier_bytes: MAX_CAPSULE_BYTES,
            },
        )
        .await
        .unwrap();
        let replacement = view.pointer_catalog().await.unwrap();
        assert!(!replacement.xorbs().contains_key(&source.hash.hex()));
        assert!(replacement.xorbs().contains_key(&destination_a.hash.hex()));
        assert!(replacement.xorbs().contains_key(&destination_b.hash.hex()));
        assert_ne!(
            replacement.files()[&file_hash.hex()].shard_hash(),
            shard_hash.hex()
        );
        crab_read::verify_capsule_pointer_catalog_objects(&layout, &replacement)
            .await
            .unwrap();
        assert!(matches!(
            store.get_with_etag(&router.manifest_path()).await,
            Err(CrabError::NotFound { .. })
        ));
    }
}
