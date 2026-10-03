use super::*;
use object_store::ObjectStoreExt;

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
    crab_write::capsule_protocol::publish(&layout, root, &transaction, &capsule)
        .await
        .unwrap();
    let complete = view(&layout).await;
    assert!(
        complete
            .git_visibility_index()
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
