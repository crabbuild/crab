use super::*;

#[tokio::test(flavor = "multi_thread")]
async fn preparation_cancellation_and_known_commit_keep_distinct_outcomes() {
    use crab_remote::prepare::{CapsuleCommitOutcome, Error};
    for existing in [false, true] {
        for late in [false, true] {
            let hooked = Arc::new(crab_storage::test_support::CountingObjectStore::new(
                Arc::new(object_store::memory::InMemory::new()),
            ));
            let layout = crab_storage::StoreLayout::new(
                crab_storage::Store::new(hooked.clone()),
                "repositories/cancel".to_owned(),
            );
            crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
                .await
                .unwrap();
            let (directory, content) = content_fixture().await;
            source_commit(directory.path(), &content);
            let initial_tip = String::from_utf8(git(directory.path(), &["rev-parse", "HEAD"]))
                .unwrap()
                .trim()
                .to_owned();
            let (prepared, view, runtime, leases) =
                prepare(&layout, directory.path(), Some(content)).await;
            let artifacts = prepared
                .upload_capsule(&view, proof_limits(), &CancellationToken::new())
                .await
                .unwrap();
            let (artifacts, runtime, leases, previous) = if existing {
                let initial = artifacts
                    .commit(
                        None,
                        std::time::Duration::from_secs(60),
                        &CancellationToken::new(),
                    )
                    .await
                    .unwrap();
                assert!(matches!(initial, CapsuleCommitOutcome::Committed { .. }));
                leases.release_after_commit().await;
                runtime.shutdown().await;
                std::fs::write(directory.path().join("next.txt"), b"next version").unwrap();
                git(directory.path(), &["add", "next.txt"]);
                git(directory.path(), &["commit", "-m", "next"]);
                let (prepared, view, runtime, leases) =
                    prepare(&layout, directory.path(), None).await;
                let artifacts = prepared
                    .upload_capsule(&view, proof_limits(), &CancellationToken::new())
                    .await
                    .unwrap();
                (artifacts, runtime, leases, Some(initial_tip))
            } else {
                (artifacts, runtime, leases, None)
            };
            let tip = String::from_utf8(git(directory.path(), &["rev-parse", "HEAD"]))
                .unwrap()
                .trim()
                .to_owned();
            let cancel = CancellationToken::new();
            let trigger = if late {
                layout.capsule_ref_heads_prefix()
            } else {
                layout.repo_path("v2/capsules")
            };
            hooked.cancel_after_put(trigger, cancel.clone());
            let result = artifacts
                .commit(None, std::time::Duration::from_secs(60), &cancel)
                .await;
            if result.is_ok() {
                leases.release_after_commit().await;
            } else {
                leases.release().await;
            }
            runtime.shutdown().await;
            assert!(cancel.is_cancelled(), "existing={existing}, late={late}");
            let refs = crab_read::capsule_protocol::read_visible_refs(&layout)
                .await
                .unwrap();
            if late {
                assert!(
                    matches!(result, Ok(CapsuleCommitOutcome::Committed { .. })),
                    "existing={existing}: {result:?}"
                );
                assert_eq!(refs.get("refs/heads/main"), Some(&tip));
            } else {
                assert!(
                    matches!(result, Err(Error::Write(crab_write::WriteError::Cancelled))),
                    "existing={existing}: {result:?}"
                );
                assert_eq!(refs.get("refs/heads/main"), previous.as_ref());
            }
            let new_cancel = CancellationToken::new();
            let successor = crab_remote::publication::acquire_leases(
                layout.store(),
                &layout,
                ["refs/heads/main".to_owned()],
                crab_remote::publication::LeaseOptions::renewing(std::time::Duration::from_secs(
                    60,
                )),
                &new_cancel,
            )
            .await
            .unwrap();
            successor.release().await;
        }
    }
}
