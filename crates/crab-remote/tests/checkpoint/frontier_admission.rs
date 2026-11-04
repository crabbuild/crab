use super::*;
use object_store::ObjectStoreExt;

#[tokio::test]
async fn checkpointed_ref_heads_do_not_repeat_body_gets() {
    let (layout, observations) = empty_fixture().await;
    let layout = StoreLayout::with_global_prefix(
        layout.store().clone().with_target_identity([1; 32]),
        layout.repo_prefix().to_owned(),
        layout.global_prefix().to_owned(),
    );
    let names = (0..64)
        .map(|index| format!("stable-{index}"))
        .collect::<Vec<_>>();
    let blobs = names
        .iter()
        .map(|name| (name.as_str(), name.as_bytes()))
        .collect::<Vec<_>>();
    publish_blobs(&layout, &blobs).await;
    let checkpointed = checkpoint(&layout).await;
    let root = crab_write::capsule_protocol::open_root(&layout)
        .await
        .unwrap();
    observations.2.reset();
    let capture = crab_read::capsule_protocol::CapsuleRepositoryCapture::from_root(layout, root)
        .await
        .unwrap();
    let ref_body_gets = observations
        .2
        .requests()
        .iter()
        .filter(|request| request.location.contains("/v2/refs/"))
        .count();
    assert_eq!(
        (capture.ref_view().unwrap().refs().clone(), ref_body_gets),
        (checkpointed.refs().clone(), 0),
        "stable checkpointed refs must retain their values without one body GET per head"
    );
}

async fn straddling_frontier() -> (StoreLayout<Store>, Arc<Observations>) {
    let (layout, observations) = empty_fixture().await;
    let name = "refs/tags/changing";
    let mut previous = None;
    let mut captured = None;
    for version in 0..4 {
        let root = crab_write::capsule_protocol::open_root(&layout)
            .await
            .unwrap();
        let (oid, pack) = blob_pack(format!("version {version}").as_bytes());
        let transaction = CapsuleTransaction::new(
            root.record().digest(),
            vec![CapsuleRefEdit::new(
                name,
                previous.clone(),
                Some(oid.clone()),
                None,
            )],
        )
        .unwrap();
        let visibility = CapsuleVisibilityDelta::new(BTreeMap::from([(
            name.to_owned(),
            GitVisibilityEdit::from_replacement_objects(
                previous.clone(),
                oid.clone(),
                vec![oid.clone()],
            ),
        )]))
        .unwrap();
        let capsule = Capsule::build(
            &transaction,
            vec![pack],
            vec![CapsuleSection::new(
                CapsuleSectionKind::VisibilityDelta,
                visibility.encode().unwrap(),
            )],
        )
        .unwrap();
        crab_write::capsule_protocol::publish(&layout, root, &transaction, capsule)
            .await
            .unwrap();
        previous = Some(oid);
        if version == 1 {
            captured = Some(view(&layout).await);
        }
    }
    // The fourth publication rolls all four transactions into one run before
    // the older capture checkpoints only the first two transactions.
    crab_remote::checkpoint::publish_capsule_checkpoint_from_view(
        &layout,
        &captured.unwrap(),
        0,
        LIMIT,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    (layout, observations)
}

#[tokio::test]
async fn checkpoint_threshold_ignores_covered_run_prefix() {
    let (layout, _) = straddling_frontier().await;
    let outcome = crab_remote::checkpoint::publish_capsule_checkpoint(
        &layout,
        3,
        LIMIT,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(
        !outcome.published,
        "two pending transactions must not meet a three-capsule threshold"
    );
}

#[tokio::test]
async fn captured_views_count_only_uncheckpointed_transactions() {
    let (straddling, _) = straddling_frontier().await;
    let (shared, _) = empty_fixture().await;
    publish_blobs(&shared, &[("first", b"first"), ("second", b"second")]).await;
    for (layout, expected) in [(straddling, 2), (shared, 1)] {
        let root = crab_write::capsule_protocol::open_root(&layout)
            .await
            .unwrap();
        let capture = crab_read::capsule_protocol::CapsuleRepositoryCapture::from_root(
            layout.clone(),
            root.clone(),
        )
        .await
        .unwrap();
        let views = [
            view(&layout).await,
            capture.metadata_view(LIMITS).await.unwrap(),
            capture.owned_git_metadata_view(LIMITS).await.unwrap(),
            capture.layered_control_view(LIMITS).await.unwrap(),
            capture.tip_bound_control_view(LIMITS).await.unwrap(),
            crab_read::capsule_protocol::open_git_view_from_root(&layout, root.clone(), LIMITS)
                .await
                .unwrap(),
        ];
        let counts = views
            .iter()
            .map(|view| view.capsule_count().unwrap())
            .chain([
                crab_read::capsule_protocol::read_activity_from_root(&layout, &root)
                    .await
                    .unwrap()
                    .capsule_count(),
                capture.capsule_count().unwrap(),
                u64::try_from(
                    crab_metadata::capsule_protocol::load_pointer_catalog_lookup_from_root(
                        &layout, &root,
                    )
                    .await
                    .unwrap()
                    .pending_capsule_count(),
                )
                .unwrap(),
            ])
            .collect::<Vec<_>>();
        assert_eq!(counts, vec![expected; 9]);
    }
}

#[tokio::test]
async fn below_threshold_checkpoint_keeps_immutable_metadata_cold() {
    let (layout, observations) = straddling_frontier().await;
    observations.2.reset();
    crab_remote::checkpoint::publish_capsule_checkpoint(
        &layout,
        5,
        LIMIT,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(
        observations.2.requests().iter().all(|request| {
            !request.location.contains("/v2/capsules/")
                && !request.location.contains("/v2/checkpoints/")
        }),
        "below-threshold admission needs only the captured root and ref heads"
    );
}

#[tokio::test]
async fn checkpoint_admits_member_heavy_sources_below_the_source_count_limit() {
    let (layout, _) = empty_fixture().await;
    let (oid, pack) = blob_pack(b"retained through footer admission");
    let mut packs = vec![pack];
    for index in 1_u32..512 {
        packs.push(blob_pack(&index.to_be_bytes()).1);
    }
    // Keep runs below the 32-run compaction trigger while exhausting footer bytes.
    for index in 0..31 {
        let root = crab_write::capsule_protocol::open_root(&layout)
            .await
            .unwrap();
        let name = format!("refs/tags/alias-{index}");
        let transaction = CapsuleTransaction::new(
            root.record().digest(),
            vec![CapsuleRefEdit::new(&name, None, Some(oid.clone()), None)],
        )
        .unwrap();
        let visibility = CapsuleVisibilityDelta::new(BTreeMap::from([(
            name,
            GitVisibilityEdit::from_replacement_objects(None, oid.clone(), vec![oid.clone()]),
        )]))
        .unwrap();
        let capsule = Capsule::build(
            &transaction,
            packs.clone(),
            vec![CapsuleSection::new(
                CapsuleSectionKind::VisibilityDelta,
                visibility.encode().unwrap(),
            )],
        )
        .unwrap();
        crab_write::capsule_protocol::publish(&layout, root, &transaction, capsule)
            .await
            .unwrap();
    }
    let maximum = 64 * LIMIT;
    let limits = CapsuleReadLimits {
        max_capsule_bytes: maximum,
        max_frontier_bytes: maximum,
    };
    let root = crab_write::capsule_protocol::open_root(&layout)
        .await
        .unwrap();
    let before =
        crab_read::capsule_protocol::open_view_from_root_with_metadata(&layout, root, limits)
            .await
            .unwrap();
    assert_eq!(before.capsule_run_sources().len(), 31);
    assert_eq!(
        before
            .capsule_run_sources()
            .iter()
            .map(|source| source.members().len())
            .sum::<usize>(),
        31 * 512,
    );
    assert!(
        serde_json::to_vec(before.capsule_run_sources())
            .unwrap()
            .len()
            > 8 * 1024 * 1024
    );
    let outcome = crab_remote::checkpoint::publish_capsule_checkpoint_from_view(
        &layout,
        &before,
        0,
        maximum,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(outcome.published && outcome.pack_bytes_read > 0);
    let root = crab_write::capsule_protocol::open_root(&layout)
        .await
        .unwrap();
    let after =
        crab_read::capsule_protocol::open_view_from_root_with_metadata(&layout, root, limits)
            .await
            .unwrap();
    assert_eq!(after.refs(), before.refs());
    assert_eq!(
        after.layered_checkpoint().unwrap().sources()[0],
        before.capsule_run_sources()[0],
        "footer admission must retain the older source prefix",
    );
    let directory = tempfile::tempdir().unwrap();
    crab_git::initialize_bare_git_dir(directory.path()).unwrap();
    // The retained aliases exceed the small fixtures' 8 MiB clone budget.
    // Keep the same admission used to construct this larger repository.
    let installed = crab_read::capsule_protocol::install_git_packs_from_store(
        &after,
        &layout,
        directory.path(),
        maximum,
        None,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    verify_installed_git_blobs(
        &after,
        directory.path(),
        &installed,
        &[("alias-0", b"retained through footer admission")],
    );
}

#[tokio::test]
async fn complete_checkpoint_open_defers_pack_sidecars_until_object_read() {
    let (layout, observations) = empty_fixture().await;
    for name in ["first", "second", "third", "fourth"] {
        publish_blob(&layout, name).await;
    }
    let captured = checkpoint(&layout).await;
    let checkpoint = captured.layered_checkpoint().unwrap();
    assert!(!checkpoint.is_control_only());
    let oid = crab_remote::objects::object_id(gix_object::Kind::Blob, b"first").unwrap();
    let visibility = checkpoint.visibility_ordinal_snapshot().unwrap().unwrap();
    let position = visibility
        .objects()
        .binary_search(&oid.as_bytes().try_into().unwrap())
        .unwrap();
    let member = visibility.member_admission().unwrap()[position];
    let selected = &checkpoint.sources()[usize::from(member.source_index())];
    let unrelated = checkpoint
        .sources()
        .iter()
        .filter(|source| source.object_hash() != selected.object_hash())
        .map(|source| layout.capsule_path(source.object_hash()))
        .collect::<Vec<_>>();
    assert!(!unrelated.is_empty());
    for path in &unrelated {
        observations.2.block_body_reads_for(path);
    }
    let runtime = Arc::new(crab_remote_git::RemoteGitRuntime::default());
    let cancel = CancellationToken::new();
    observations.2.reset();
    let repository = captured
        .git_repository_from_store(
            layout.clone(),
            crab_remote_git::RepositoryIdentity::new("memory", "complete-placement", 1).unwrap(),
            runtime.clone(),
            crab_remote_git::RepositoryOptions::default(),
            LIMIT,
            &cancel,
            None,
        )
        .await
        .unwrap();
    assert!(observations.2.requests().is_empty());
    let operation = repository
        .operation(crab_remote_git::OperationKind::Repository, &cancel)
        .await
        .unwrap();
    let result = operation.read_objects(&[oid]).await;
    let objects = operation.finish(result).await.unwrap();
    assert_eq!(objects[0].data.as_ref(), b"first");
    runtime.shutdown().await;
}

#[tokio::test]
async fn paged_checkpoint_placement_keeps_unrelated_pack_indexes_cold() {
    let (layout, observations) = empty_fixture().await;
    for name in ["first", "second", "third", "fourth"] {
        publish_blob(&layout, name).await;
    }
    let complete = checkpoint(&layout).await;
    let checkpoint = complete.layered_checkpoint().unwrap();
    let visibility = checkpoint.visibility_ordinal_snapshot().unwrap().unwrap();
    publish_blob(&layout, "frontier").await;
    let root = crab_write::capsule_protocol::open_root(&layout)
        .await
        .unwrap();
    let captured = crab_read::capsule_protocol::open_git_view_from_root(&layout, root, LIMITS)
        .await
        .unwrap();
    let runtime = Arc::new(
        crab_remote_git::RemoteGitRuntime::new(
            crab_remote_git::RuntimeOptions {
                max_pack_index_cache_entries: 0,
                ..Default::default()
            },
            Arc::new(crab_remote_git::NoopMetrics),
        )
        .unwrap(),
    );
    let cancel = CancellationToken::new();
    let repository = captured
        .git_repository_from_store(
            layout.clone(),
            crab_remote_git::RepositoryIdentity::new("memory", "paged-placement", 1).unwrap(),
            runtime.clone(),
            crab_remote_git::RepositoryOptions::default(),
            LIMIT,
            &cancel,
            None,
        )
        .await
        .unwrap();
    for name in ["first", "second", "third", "fourth"] {
        let oid = crab_remote::objects::object_id(gix_object::Kind::Blob, name.as_bytes()).unwrap();
        let position = visibility
            .objects()
            .binary_search(&oid.as_bytes().try_into().unwrap())
            .unwrap();
        let member = visibility.member_admission().unwrap()[position];
        let selected = &checkpoint.sources()[usize::from(member.source_index())];
        let unrelated = checkpoint
            .sources()
            .iter()
            .filter(|source| source.object_hash() != selected.object_hash())
            .map(|source| layout.capsule_path(source.object_hash()).to_string())
            .collect::<Vec<_>>();
        let operation = repository
            .operation(crab_remote_git::OperationKind::Repository, &cancel)
            .await
            .unwrap();
        observations.2.reset();
        let result = operation.read_objects(&[oid]).await;
        let objects = operation.finish(result).await.unwrap();
        assert_eq!(objects[0].data.as_ref(), name.as_bytes());
        assert!(
            observations
                .2
                .requests()
                .iter()
                .all(|read| !unrelated.contains(&read.location)),
            "checkpoint member admission must exclude unrelated source indexes"
        );
    }
    runtime.shutdown().await;
}

#[tokio::test]
async fn frontier_delta_base_placement_does_not_probe_unrelated_member_indexes() {
    let (layout, observations) = empty_fixture().await;
    publish_blob(&layout, "seed").await;
    checkpoint(&layout).await;

    // A full "hello world" blob followed by a REF_DELTA for "hello world!".
    // Only the latter is visible; its physical dependency is not a wanted object.
    let body = Bytes::from_static(
        b"PACK\x00\x00\x00\x02\x00\x00\x00\x02\x3b\x78\x9c\xcb\x48\xcd\xc9\xc9\x57\x28\xcf\x2f\xca\x49\x01\x00\x1a\x0b\x04\x5d\x76\x95\xd0\x9f\x2b\x10\x15\x93\x47\xee\xce\x71\x39\x9a\x7e\x2e\x90\x7e\xa3\xdf\x4f\x78\x9c\xe3\xe6\x99\xc0\xcd\xa8\x08\x00\x03\x08\x00\xd5\xc8\xb3\x79\x68\x41\x27\xb1\xee\x2c\xcb\x94\x62\xcf\xbd\x36\x4e\x23\xc8\x62\xf1",
    );
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("delta.pack");
    std::fs::write(&path, &body).unwrap();
    // Native Git accepts self-contained REF deltas; the gix index writer only
    // accepts OFS deltas. Preserve this wire shape so the base is looked up by OID.
    crab_git::initialize_bare_git_dir(directory.path()).unwrap();
    let indexed = std::process::Command::new("git")
        .arg("--git-dir")
        .arg(directory.path())
        .args(["index-pack", "--strict"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(
        indexed.status.success(),
        "{}",
        String::from_utf8_lossy(&indexed.stderr)
    );
    let index_path = path.with_extension("idx");
    let reverse_path = path.with_extension("rev");
    crab_git::pack_locator::write_pack_reverse_index(&index_path, &reverse_path).unwrap();
    let checksum = crab_git::pack::verify_pack_index_file(&index_path).unwrap();
    let kinds = crab_git::pack_locator::encode_pack_kind_metadata(
        gix_hash::ObjectId::from_hex(checksum.as_bytes()).unwrap(),
        &[gix_object::Kind::Blob; 2],
    )
    .unwrap();
    let index = std::fs::read(index_path).unwrap();
    // The authenticated run control retains pooled indexes. Only the two
    // physical entries, excluding the pack header/trailer, consume this
    // operation's origin-byte budget.
    let byte_budget = body.len() as u64 - 32;
    let pack = CapsuleGitPack::new(
        body,
        Bytes::from(index),
        Bytes::from(std::fs::read(reverse_path).unwrap()),
        Bytes::from(kinds),
        checksum,
        2,
    )
    .unwrap();
    let base = crab_remote::objects::object_id(gix_object::Kind::Blob, b"hello world").unwrap();
    let target = crab_remote::objects::object_id(gix_object::Kind::Blob, b"hello world!").unwrap();
    let (other_oid, other_pack) = blob_pack(b"unrelated");
    let root = crab_write::capsule_protocol::open_root(&layout)
        .await
        .unwrap();
    let refs = [
        ("refs/tags/delta", target.to_string()),
        ("refs/tags/other", other_oid),
    ];
    let transaction = CapsuleTransaction::new(
        root.record().digest(),
        refs.iter()
            .map(|(name, oid)| CapsuleRefEdit::new(*name, None, Some(oid.clone()), None))
            .collect(),
    )
    .unwrap();
    let visibility = CapsuleVisibilityDelta::new(
        refs.iter()
            .map(|(name, oid)| {
                (
                    (*name).to_owned(),
                    GitVisibilityEdit::from_replacement_objects(
                        None,
                        oid.clone(),
                        vec![oid.clone()],
                    ),
                )
            })
            .collect(),
    )
    .unwrap();
    let capsule = Capsule::build(
        &transaction,
        vec![pack, other_pack],
        vec![CapsuleSection::new(
            CapsuleSectionKind::VisibilityDelta,
            visibility.encode().unwrap(),
        )],
    )
    .unwrap();
    crab_write::capsule_protocol::publish(&layout, root, &transaction, capsule)
        .await
        .unwrap();
    let complete = view(&layout).await;
    assert!(
        complete
            .git_visibility_index()
            .await
            .unwrap()
            .ref_closures()
            .values()
            .all(|objects| !objects.contains(&base.to_string()))
    );
    // Keep the source lazy so the operation's fetched-byte limit covers the
    // physical REF_DELTA base and rejects a missing or corrupt origin entry.
    let view = crab_read::capsule_protocol::open_view_from_root_with_control(
        &layout,
        complete.root_snapshot().clone(),
        LIMITS,
    )
    .await
    .unwrap();
    let base_bytes: [u8; 20] = base.as_bytes().try_into().unwrap();
    assert!(!view.frontier_object_admission().contains_key(&base_bytes));
    let source = &view.capsule_run_sources()[0];
    assert!(view.capsule_run_member_oids()[source.object_hash()][0].contains(&base_bytes));

    for scenario in ["valid", "byte-budget", "corrupt-base"] {
        if scenario == "corrupt-base" {
            let path = layout.capsule_path(source.object_hash());
            let (original, _) = layout.store().get_with_etag(&path).await.unwrap();
            let mut corrupt = original.to_vec();
            corrupt[source.members()[0].pack().offset() as usize + 13] ^= 1;
            layout
                .store()
                .inner()
                .put(&path, Bytes::from(corrupt).into())
                .await
                .unwrap();
        }
        let runtime = Arc::new(
            crab_remote_git::RemoteGitRuntime::new(
                crab_remote_git::RuntimeOptions {
                    // No retained index can conceal a missing physical location hint.
                    max_pack_index_cache_entries: 0,
                    ..Default::default()
                },
                Arc::new(crab_remote_git::NoopMetrics),
            )
            .unwrap(),
        );
        let cancel = CancellationToken::new();
        let repository = view
            .git_repository_from_store(
                layout.clone(),
                crab_remote_git::RepositoryIdentity::new("fixture", "physical-base", 1).unwrap(),
                runtime.clone(),
                crab_remote_git::RepositoryOptions::default(),
                LIMIT,
                &cancel,
                None,
            )
            .await
            .unwrap();
        let operation = repository
            .operation_with_limits(
                crab_remote_git::OperationKind::UploadPack,
                &cancel,
                crab_remote_git::OperationLimits {
                    max_fetched_bytes: byte_budget - u64::from(scenario == "byte-budget"),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        observations.0.lock().unwrap().clear();
        let result = operation.read_objects(&[target]).await;
        let result = operation.finish(result).await;
        runtime.shutdown().await;
        if scenario == "valid" {
            let objects = result.expect("physical base lookup must fit the exact member budget");
            assert_eq!(objects.len(), 1);
            assert_eq!(objects[0].oid, target);
            assert_eq!(objects[0].data.as_ref(), b"hello world!");
            assert_eq!(
                observations
                    .0
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|read| read.bytes_read)
                    .sum::<u64>(),
                byte_budget
            );
        } else {
            let mut error = result.as_ref().unwrap_err();
            while let crab_remote_git::Error::SharedRead { source } = error {
                error = source.as_ref();
            }
            match scenario {
                "byte-budget" => assert!(matches!(
                    error,
                    crab_remote_git::Error::LimitExceeded {
                        limit: "fetched bytes",
                        ..
                    }
                )),
                "corrupt-base" => assert!(matches!(
                    error,
                    crab_remote_git::Error::PackedEntryCrcMismatch { oid } if *oid == base
                )),
                _ => unreachable!(),
            }
        }
    }
}
