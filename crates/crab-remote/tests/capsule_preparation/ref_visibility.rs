use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn acknowledged_branch_survives_source_deletion() {
    branch_survives_source_deletion(layout().await.0, false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn prepared_branch_survives_source_deletion_before_commit() {
    branch_survives_source_deletion(layout().await.0, true).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires CRAB_E2E_FLAT_BUCKET and explicit scratch S3 credentials"]
async fn live_branch_survives_source_deletion_and_source_removal() {
    let bucket = std::env::var("CRAB_E2E_FLAT_BUCKET").expect("explicit scratch bucket required");
    let store =
        crab_storage::build_static_env_store(&bucket, crab_storage::StorageProviderKind::S3)
            .unwrap();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    for before_commit in [false, true] {
        let prefix = format!("ref-lifetime/{nonce}/{before_commit}");
        let layout = crab_storage::StoreLayout::with_global_prefix(
            store.clone(),
            format!("{prefix}/repo"),
            format!("{prefix}/global"),
        );
        crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
            .await
            .unwrap();
        branch_survives_source_deletion(layout, before_commit).await;
    }
}

async fn branch_survives_source_deletion(
    layout: crab_storage::StoreLayout<crab_storage::Store>,
    delete_before_commit: bool,
) {
    use crab_metadata::capsule_protocol::{CapsuleRefEdit, CapsuleTransaction};
    use crab_remote::prepare::CapsuleCommitOutcome;
    let cancel = CancellationToken::new();
    let (directory, content) = content_fixture().await;
    source_commit(directory.path(), &content);
    let (prepared, view, runtime, leases) = prepare(&layout, directory.path(), Some(content)).await;
    let artifacts = prepared
        .upload_capsule(&view, proof_limits(), &cancel)
        .await
        .unwrap();
    let committed = artifacts
        .commit(None, std::time::Duration::from_secs(60), &cancel)
        .await;
    leases.release_after_commit().await;
    runtime.shutdown().await;
    assert!(matches!(
        committed.unwrap(),
        CapsuleCommitOutcome::Committed { .. }
    ));
    let tip = gix_hash::ObjectId::from_hex(
        String::from_utf8(git(directory.path(), &["rev-parse", "HEAD"]))
            .unwrap()
            .trim()
            .as_bytes(),
    )
    .unwrap();
    let mut expected_objects = String::from_utf8(git(
        directory.path(),
        &["rev-list", "--objects", "--no-object-names", "HEAD"],
    ))
    .unwrap()
    .lines()
    .map(str::to_owned)
    .collect::<Vec<_>>();
    expected_objects.sort_unstable();
    let expected_bodies = expected_objects
        .iter()
        .map(|oid| {
            let kind = String::from_utf8(git(directory.path(), &["cat-file", "-t", oid])).unwrap();
            (
                gix_hash::ObjectId::from_hex(oid.as_bytes()).unwrap(),
                git(directory.path(), &["cat-file", kind.trim(), oid]),
            )
        })
        .collect::<Vec<_>>();
    let crab_git::pointer_detect::PointerKind::Crab(pointer) = crab_git::pointer_detect::classify(
        &std::fs::read(directory.path().join("model.bin")).unwrap(),
    ) else {
        panic!("fixture pointer");
    };
    let borrowed = "refs/heads/borrowed";
    let (prepared, view, runtime, leases) =
        prepare_update(&layout, directory.path(), None, borrowed, Some(tip)).await;
    let edit = CapsuleRefEdit::new(
        borrowed,
        None,
        Some(tip.to_string()),
        prepared
            .plan()
            .peeled()
            .get(borrowed)
            .map(ToString::to_string),
    );
    let (plan, expected_borrow) = (0..256)
        .find_map(|i| {
            let plan = blake3::hash(format!("borrow-lifetime:{i}").as_bytes())
                .to_hex()
                .to_string();
            let transaction = CapsuleTransaction::for_plan(
                view.root_snapshot().record().digest(),
                &plan,
                vec![edit.clone()],
            )
            .unwrap();
            let id = transaction.id().unwrap();
            (id.as_bytes()[0] >= b'8').then_some((plan, id))
        })
        .unwrap();
    let artifacts = prepared
        .upload_capsule(&view, proof_limits(), &cancel)
        .await
        .unwrap();
    if delete_before_commit {
        delete_source(&layout, directory.path(), tip, &expected_borrow).await;
    }
    let committed = artifacts
        .commit(Some(&plan), std::time::Duration::from_secs(60), &cancel)
        .await;
    leases.release_after_commit().await;
    runtime.shutdown().await;
    let CapsuleCommitOutcome::Committed { transaction_id } = committed.unwrap() else {
        panic!("borrowed ref publication must have a proven terminal outcome");
    };
    assert_eq!(transaction_id, expected_borrow);
    let root = crab_write::capsule_protocol::open_root(&layout)
        .await
        .unwrap();
    let (heads, active) =
        crab_metadata::capsule_protocol::capture_ref_heads(&layout, root.record().root())
            .await
            .unwrap();
    let head = heads
        .iter()
        .find(|head| head.ref_name() == borrowed)
        .unwrap();
    let capsule_pointer = &head.visible(&active).frontier()[0];
    let (_, controls) =
        crab_metadata::capsule_protocol::load_capsule_run_git_control(&layout, capsule_pointer)
            .await
            .unwrap();
    let control = controls[0]
        .materialize_visibility(layout.store())
        .await
        .unwrap();
    let evidence = &control.visibility_delta().unwrap().unwrap().edits()[borrowed];
    assert_eq!(
        (evidence.replaces, &evidence.old_oid, &evidence.added),
        (true, &None, &expected_objects),
    );
    if !delete_before_commit {
        delete_source(&layout, directory.path(), tip, &expected_borrow).await;
    }
    let root = crab_write::capsule_protocol::open_root(&layout)
        .await
        .unwrap();
    std::fs::remove_dir_all(directory.path()).unwrap();
    let limits = crab_read::capsule_protocol::CapsuleReadLimits {
        max_capsule_bytes: LIMIT,
        max_frontier_bytes: LIMIT,
    };
    for mode in ["full", "control", "paged"] {
        let view = match mode {
            "full" => {
                crab_read::capsule_protocol::open_view_from_root(&layout, root.clone(), limits)
                    .await
            }
            "control" => {
                crab_read::capsule_protocol::open_view_from_root_with_metadata(
                    &layout,
                    root.clone(),
                    limits,
                )
                .await
            }
            _ => {
                crab_read::capsule_protocol::open_git_view_from_root(&layout, root.clone(), limits)
                    .await
            }
        }
        .unwrap_or_else(|error| panic!("acknowledged state must reopen through {mode}: {error}"));
        let index = view.git_visibility_index().await.unwrap();
        assert_eq!(
            (
                view.refs().get(borrowed).map(String::as_str),
                view.refs().contains_key("refs/heads/main"),
                index.objects_for_ref(borrowed)
            ),
            (
                Some(tip.to_string().as_str()),
                false,
                Some(expected_objects.clone())
            ),
        );
        let runtime = Arc::new(crab_remote_git::RemoteGitRuntime::default());
        let repository = view
            .git_repository_from_store(
                layout.clone(),
                crab_remote_git::RepositoryIdentity::new("qualification", "branch", 1).unwrap(),
                runtime.clone(),
                crab_remote_git::RepositoryOptions::default(),
                LIMIT,
                &cancel,
                None,
            )
            .await
            .unwrap();
        let operation = repository
            .operation(crab_remote_git::OperationKind::Repository, &cancel)
            .await
            .unwrap();
        let oids = expected_bodies
            .iter()
            .map(|(oid, _)| *oid)
            .collect::<Vec<_>>();
        let result = operation.read_objects(&oids).await;
        let result = operation.finish(result).await;
        runtime.shutdown().await;
        assert_eq!(
            result
                .unwrap()
                .into_iter()
                .map(|object| (object.oid, object.data.to_vec()))
                .collect::<Vec<_>>(),
            expected_bodies,
            "cold {mode} object bytes after source removal",
        );
        let catalog = view.pointer_catalog().await.unwrap();
        let file_hash = crab_xet::hash::merkle_hex_from_bytes(&pointer.file_hash);
        let shard = MerkleHash::from_hex(catalog.files()[&file_hash].shard_hash()).unwrap();
        crab_read::pointer_proof::verify_crab_pointer(
            &layout,
            &pointer,
            shard,
            proof_limits().content,
            &cancel,
        )
        .await
        .unwrap();
    }
}

async fn delete_source(
    layout: &crab_storage::StoreLayout<crab_storage::Store>,
    directory: &std::path::Path,
    tip: gix_hash::ObjectId,
    expected_borrow: &str,
) {
    use crab_metadata::capsule_protocol::{CapsuleRefEdit, CapsuleTransaction};
    use crab_remote::prepare::CapsuleCommitOutcome;
    let cancel = CancellationToken::new();
    let (prepared, view, runtime, leases) =
        prepare_update(layout, directory, None, "refs/heads/main", None).await;
    let edit = CapsuleRefEdit::new("refs/heads/main", Some(tip.to_string()), None, None);
    // Reviewed plans provide stable transaction identities without a runtime
    // nonce hook. Force deletion ahead of borrowing in canonical ID order.
    let (plan, expected_delete) = (0..256)
        .find_map(|i| {
            let plan = blake3::hash(format!("delete-lifetime:{i}").as_bytes())
                .to_hex()
                .to_string();
            let transaction = CapsuleTransaction::for_plan(
                view.root_snapshot().record().digest(),
                &plan,
                vec![edit.clone()],
            )
            .unwrap();
            let id = transaction.id().unwrap();
            (id.as_str() < expected_borrow).then_some((plan, id))
        })
        .unwrap();
    let artifacts = prepared
        .upload_capsule(&view, proof_limits(), &cancel)
        .await
        .unwrap();
    let committed = artifacts
        .commit(Some(&plan), std::time::Duration::from_secs(60), &cancel)
        .await;
    leases.release_after_commit().await;
    runtime.shutdown().await;
    let CapsuleCommitOutcome::Committed { transaction_id } = committed.unwrap() else {
        panic!("source deletion must have a proven terminal outcome");
    };
    assert_eq!(transaction_id, expected_delete);
}
