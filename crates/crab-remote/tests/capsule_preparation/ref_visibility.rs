use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn acknowledged_branch_survives_source_deletion() {
    branch_survives_source_deletion(false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn prepared_branch_survives_source_deletion_before_commit() {
    branch_survives_source_deletion(true).await;
}

async fn branch_survives_source_deletion(delete_before_commit: bool) {
    use crab_metadata::capsule_protocol::{CapsuleRefEdit, CapsuleTransaction};
    use crab_remote::prepare::CapsuleCommitOutcome;
    let cancel = CancellationToken::new();
    let (directory, content) = content_fixture().await;
    source_commit(directory.path(), &content);
    let (layout, _) = layout().await;
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
    let pointer = &head.visible(&active).frontier()[0];
    let (_, controls) =
        crab_metadata::capsule_protocol::load_capsule_run_git_control(&layout, pointer)
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
