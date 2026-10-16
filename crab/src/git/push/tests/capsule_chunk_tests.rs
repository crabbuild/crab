use super::*;

struct FirstCandidateOnly(Arc<AddRemoteChunkClassifier>);

#[async_trait::async_trait]
impl crab_staging::push_plan::ExistingChunkLookup for FirstCandidateOnly {
    async fn lookup_existing_candidates(
        &self,
        chunks: &[(MerkleHash, u64)],
    ) -> crab_staging::Result<Vec<Option<crab_staging::push_plan::ExistingChunkCandidate>>> {
        let candidates = crab_staging::push_plan::ExistingChunkLookup::lookup_existing_candidates(
            self.0.as_ref(),
            chunks,
        )
        .await?;
        Ok(candidates
            .into_iter()
            .enumerate()
            .map(
                |(index, candidate)| {
                    if index == 0 { candidate } else { None }
                },
            )
            .collect())
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn capsule_push_exposes_committed_chunks_to_cold_cross_repository_add() {
    use crab_staging::push_plan::ExistingChunkLookup;
    use crab_staging::stream::{StreamStageProgress, stage_reader_streaming};

    let _git_env = CleanGitEnvGuard::new();
    let content = test_deterministic_bytes(0x4cab, 2 * 1024 * 1024);
    let source = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .args(args)
            .current_dir(source.path())
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.invalid")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.invalid")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "-b", "main"]);
    let staging_root = source.path().join("staging");
    let staging = crab_staging::StagingArea::open(staging_root.clone())
        .await
        .unwrap();
    let staged = stage_reader_streaming(
        &mut std::io::Cursor::new(content.as_slice()),
        std::path::Path::new("model.bin"),
        &staging,
        StreamStageProgress::default(),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let chunks = staging
        .recipe_page(&staged.recipe, 0)
        .unwrap()
        .chunks
        .into_iter()
        .map(|chunk| (chunk.chunk_hash, chunk.len))
        .collect::<Vec<_>>();
    staging
        .delegate_publication(&[(&staged.batch_id, std::path::Path::new("model.bin"))])
        .unwrap();
    let pointer = crab_types::pointer::Pointer {
        file_hash: staged.file_hash,
        size: staged.size,
        shard_hint: None,
    };
    std::fs::write(source.path().join("model.bin"), pointer.serialize()).unwrap();
    git(&["add", "model.bin"]);
    git(&["commit", "-m", "shared payload"]);
    staging.close().await.unwrap();
    let staging = Arc::new(
        crab_staging::StagingAreaReadOnly::open(staging_root)
            .await
            .unwrap(),
    );
    let reads = Arc::new(RangeCountingStore::new(Arc::new(AtomicUsize::new(0))));
    let store = Store::new(reads.clone());
    let source_router = StoreLayout::new(store.clone(), "repos/source".to_owned());
    let layout =
        crab_storage::StoreLayout::new(store.as_storage().clone(), "repos/source".to_owned());
    crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
        .await
        .unwrap();
    let config = PushConfig {
        git_dir: Some(source.path().join(".git")),
        ..Default::default()
    };
    let (result, _) = crate::git::capsule_push::run(
        &config,
        &[crate::git::remote_helper::PushSpec {
            force: false,
            src: "refs/heads/main".to_owned(),
            dst: "refs/heads/main".to_owned(),
        }],
        &store,
        &source_router,
        None,
        &[],
        Some(&staging),
        None,
        None,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(result.all_ok(), "source publication failed: {result:?}");
    drop(staging);

    let router = StoreLayout::new(store.clone(), "repos/consumer".to_owned());
    let pipeline = PushPipeline::new(
        PushConfig::default(),
        Vec::new(),
        Some(store.clone()),
        None,
        None,
        router.repo_prefix().to_owned(),
        router.clone(),
        None,
        CancellationToken::new(),
        None,
    );
    let cache_root = tempfile::tempdir().unwrap();
    let mut metadata = crate::core::config::MetaDbTomlConfig::default();
    metadata.chunk_index.local_path = Some(cache_root.path().join("chunks.sqlite"));
    pipeline.install_metadb(build_push_metadb_guard(
        &store, &router, None, &metadata, true,
    ));
    // No advisory caches or source-repository state can manufacture a hit.
    // This exercises the real publisher-to-classifier authority boundary.
    let classifier = Arc::new(AddRemoteChunkClassifier {
        pipeline,
        remote_chunk_index: tokio::sync::OnceCell::new(),
        candidate_cache: None,
        candidate_cache_hits: std::sync::atomic::AtomicU64::new(0),
        candidate_cache_misses: std::sync::atomic::AtomicU64::new(0),
    });
    let candidates = classifier.lookup_existing_candidates(&chunks).await;
    let consumer_staging_root = cache_root.path().join("consumer-staging");
    let consumer_staging = crab_staging::StagingArea::open(consumer_staging_root.clone())
        .await
        .unwrap();
    let remote_staged = stage_reader_streaming(
        &mut std::io::Cursor::new(content.as_slice()),
        std::path::Path::new("model.bin"),
        &consumer_staging,
        StreamStageProgress {
            existing_lookup: Some(classifier.clone()),
            ..Default::default()
        },
        &CancellationToken::new(),
    )
    .await;
    let partial_root = cache_root.path().join("partial-staging");
    let partial_staging = crab_staging::StagingArea::open(partial_root.clone())
        .await
        .unwrap();
    let partial_staged = stage_reader_streaming(
        &mut std::io::Cursor::new(content.as_slice()),
        std::path::Path::new("model.bin"),
        &partial_staging,
        StreamStageProgress {
            existing_lookup: Some(Arc::new(FirstCandidateOnly(classifier.clone()))),
            ..Default::default()
        },
        &CancellationToken::new(),
    )
    .await;
    classifier.close().await;
    let partial_staged = partial_staged.unwrap();
    partial_staging
        .delegate_publication(&[(&partial_staged.batch_id, std::path::Path::new("model.bin"))])
        .unwrap();
    partial_staging.close().await.unwrap();
    let remote_staged = remote_staged.unwrap();
    consumer_staging
        .delegate_publication(&[(&remote_staged.batch_id, std::path::Path::new("model.bin"))])
        .unwrap();
    consumer_staging.close().await.unwrap();
    let candidates = candidates.unwrap();
    assert!(
        !candidates.is_empty() && candidates.iter().all(Option::is_some),
        "committed source chunks must be reusable from a cold consumer: {candidates:?}"
    );

    let planned = chunks
        .iter()
        .zip(&candidates)
        .filter_map(|((hash, _), candidate)| candidate.map(|candidate| (*hash, candidate)))
        .collect::<HashMap<_, _>>();
    let root = crab_metadata::capsule_protocol::load_root(&layout)
        .await
        .unwrap();
    let catalog = crab_metadata::capsule_protocol::load_pointer_catalog_from_root(&layout, &root)
        .await
        .unwrap();
    let mut origins = HashMap::new();
    for candidate in planned.values() {
        let path = layout.xorb_path(&candidate.xorb_ref.xorb_hash);
        let (bytes, identity) = layout.store().get_with_etag(&path).await.unwrap();
        let parser = crab_xet::xorb::parser::XorbParser::parse(bytes.clone()).unwrap();
        let mut origin = crab_metadata::receipts::OriginReceipt::new(
            "canonical-origin".to_owned(),
            path.to_string(),
            candidate.xorb_ref.xorb_hash.into(),
            parser.payload_digest(),
            bytes.len() as u64,
            identity.e_tag,
            identity.version,
        );
        // Change the head's proof identity while retaining the actual committed source.
        origin.proven_at_unix_secs += 10;
        origins.insert(candidate.xorb_ref.xorb_hash, origin);
    }
    crab_metadata::capsule_protocol::write_capsule_chunk_candidates(
        &layout,
        &catalog,
        &origins,
        &|| false,
    )
    .await
    .unwrap();
    let consumer_layout =
        crab_storage::StoreLayout::new(store.as_storage().clone(), "repos/consumer".to_owned());
    let verified = revalidate_staged_remote_chunks(
        &metadata,
        &consumer_layout,
        &planned,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        verified.candidates.len(),
        planned.len(),
        "old exact proof survives a newer shared head"
    );

    for field in ["placement", "origin", "position"] {
        let mut forged = planned.clone();
        for candidate in forged.values_mut() {
            match field {
                "placement" => candidate.placement_id = [9; 32],
                "origin" => candidate.origin_proof_id = [9; 32],
                "position" => candidate.xorb_ref.chunk_index += 1,
                _ => unreachable!(),
            }
        }
        assert!(
            revalidate_staged_remote_chunks(
                &metadata,
                &consumer_layout,
                &forged,
                &CancellationToken::new()
            )
            .await
            .unwrap()
            .candidates
            .is_empty(),
            "accepted forged {field}"
        );
    }

    let consumer_base = crab_write::capsule_protocol::initialize(
        &consumer_layout,
        &"2".repeat(64),
        "refs/heads/main",
    )
    .await
    .unwrap();
    let consumer_staging = Arc::new(
        crab_staging::StagingAreaReadOnly::open(consumer_staging_root)
            .await
            .unwrap(),
    );
    reads.full_xorb_gets.store(0, Ordering::Relaxed);
    let (delta, stats) = crate::git::xet_publication::prepare_delta(
        &consumer_layout,
        &consumer_base,
        &[crab_types::pointer::Pointer {
            file_hash: remote_staged.file_hash,
            size: remote_staged.size,
            shard_hint: None,
        }],
        Some(&consumer_staging),
        None,
        None,
        &metadata,
        1,
        true,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        (delta.files().len(), stats.xorbs_uploaded),
        (1, 0),
        "remote-only staging must publish without repacking"
    );
    assert_eq!(
        reads.full_xorb_gets.load(Ordering::Relaxed),
        0,
        "an exact, current source/origin proof must not download the reused xorb body"
    );
    drop(consumer_staging);
    let partial_staging = Arc::new(
        crab_staging::StagingAreaReadOnly::open(partial_root)
            .await
            .unwrap(),
    );
    reads.full_xorb_gets.store(0, Ordering::Relaxed);
    let (_, partial_stats) = crate::git::xet_publication::prepare_delta(
        &consumer_layout,
        &consumer_base,
        &[crab_types::pointer::Pointer {
            file_hash: partial_staged.file_hash,
            size: partial_staged.size,
            shard_hint: None,
        }],
        Some(&partial_staging),
        None,
        None,
        &metadata,
        1,
        true,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        (
            partial_stats.xorbs_uploaded,
            reads.full_xorb_gets.load(Ordering::Relaxed)
        ),
        (0, 0),
        "partial add-time hints must reuse the authenticated requested closure without packing or downloading"
    );
    drop(partial_staging);

    let source_key = layout.capsule_root_path();
    let (source_bytes, _) = layout.store().get_with_etag(&source_key).await.unwrap();
    layout.store().delete(&source_key).await.unwrap();
    let unrooted = revalidate_staged_remote_chunks(
        &metadata,
        &consumer_layout,
        &planned,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    layout.store().put(&source_key, source_bytes).await.unwrap();
    assert!(
        unrooted.candidates.is_empty(),
        "physical bytes alone cannot replace a source root"
    );

    for candidate in planned.values() {
        let key = layout.xorb_path(&candidate.xorb_ref.xorb_hash);
        let (bytes, previous) = layout.store().get_with_etag(&key).await.unwrap();
        // Bypass the immutable adapter only to inject a provider-side replacement.
        let replaced = layout
            .store()
            .inner()
            .put(&key, bytes.into())
            .await
            .unwrap();
        assert_ne!(
            (previous.e_tag, previous.version),
            (replaced.e_tag, replaced.version),
            "the fault must actually change the provider identity"
        );
    }
    assert!(
        revalidate_staged_remote_chunks(
            &metadata,
            &consumer_layout,
            &planned,
            &CancellationToken::new()
        )
        .await
        .unwrap()
        .candidates
        .is_empty(),
        "a replacement identity cannot reuse the original proof"
    );
    let verified_refs = verified.into_refs();
    let cached = classifier
        .pipeline
        .verify_xorb_refs_with_committed_receipts(&verified_refs, &origins, true)
        .await
        .unwrap();
    assert!(
        cached.is_empty(),
        "warm proofs cannot certify the replacement identity"
    );

    // A fresh attestation for one occurrence must not certify another
    // occurrence's stale provider identity in the same immutable xorb.
    let mut fresh_origins = HashMap::new();
    for hash in planned
        .values()
        .map(|candidate| candidate.xorb_ref.xorb_hash)
        .collect::<HashSet<_>>()
    {
        let key = layout.xorb_path(&hash);
        let (bytes, identity) = layout.store().get_with_etag(&key).await.unwrap();
        let parser = crab_xet::xorb::parser::XorbParser::parse(bytes.clone()).unwrap();
        parser.verify_payload_digest().unwrap();
        parser.verify_all_chunks().unwrap();
        fresh_origins.insert(
            hash,
            crab_metadata::receipts::OriginReceipt::new(
                "canonical-origin".to_owned(),
                key.to_string(),
                hash.into(),
                parser.payload_digest(),
                bytes.len() as u64,
                identity.e_tag,
                identity.version,
            ),
        );
    }
    crab_metadata::capsule_protocol::write_capsule_chunk_candidates(
        &layout,
        &catalog,
        &fresh_origins,
        &|| false,
    )
    .await
    .unwrap();
    let index = crab_metadata::capsule_protocol::CapsuleChunkIndex::new(&layout);
    let hashes = planned.keys().copied().collect::<Vec<_>>();
    let fresh = classifier
        .pipeline
        .lookup_capsule_chunks_for_add(&hashes, &index)
        .await;
    let fresh = fresh.unwrap();
    assert_eq!(
        fresh.len(),
        planned.len(),
        "replacement proof fixture must be independently valid"
    );
    assert!(
        fresh.len() > 1,
        "mixed-origin coverage needs distinct chunks in one xorb"
    );
    let mut mixed = planned.clone();
    let (hash, candidate) = fresh.into_iter().next().unwrap();
    mixed.insert(hash, candidate);
    assert!(
        matches!(
            revalidate_staged_remote_chunks(
                &metadata,
                &consumer_layout,
                &mixed,
                &CancellationToken::new(),
            )
            .await,
            Err(CrabError::CorruptObject { .. })
        ),
        "conflicting immutable-xorb identities must not certify one another"
    );
}
