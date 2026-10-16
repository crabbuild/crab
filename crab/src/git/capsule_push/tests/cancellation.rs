use super::*;

#[tokio::test]
async fn cancellation_during_preparation_does_not_publish_direct_or_coordinated_refs() {
    for coordinated in [false, true] {
        let hooked = Arc::new(crab_storage::test_support::CountingObjectStore::new(
            Arc::new(InMemory::new()),
        ));
        let layout = crab_storage::StoreLayout::new(
            crab_storage::Store::new(hooked.clone()),
            "repos/cancel".to_owned(),
        );
        let base =
            crab_write::capsule_protocol::initialize(&layout, &"1".repeat(64), "refs/heads/main")
                .await
                .unwrap();
        let transaction = crab_metadata::capsule_protocol::CapsuleTransaction::new(
            base.record().digest(),
            vec![crab_metadata::capsule_protocol::CapsuleRefEdit::new(
                "refs/heads/main",
                None,
                Some("2".repeat(40)),
                None,
            )],
        )
        .unwrap();
        let capsule =
            crab_metadata::capsule_protocol::Capsule::build(&transaction, Vec::new(), Vec::new())
                .unwrap();
        let coordinator = Arc::new(crab_coordination::InMemoryWriteCoordinator::new());
        let config = if coordinated {
            PushConfig {
                active_active_replication: Some(active_active_replication()),
                active_active_writer: Some("east".to_owned()),
                active_active_coordinator: Some(
                    crate::git::push::ActiveActiveWriteCoordinator::new(coordinator.clone()),
                ),
                ..PushConfig::default()
            }
        } else {
            PushConfig::default()
        };
        let cancel = CancellationToken::new();
        hooked.cancel_after_put(layout.repo_path("v2/capsules"), cancel.clone());
        let result = publish_capsule(
            &config,
            &layout,
            base,
            &transaction,
            &capsule,
            &[],
            &BTreeMap::new(),
            &cancel,
        )
        .await;
        assert!(
            matches!(result, Err(CrabError::Cancelled)),
            "coordinated={coordinated}"
        );
        assert!(
            crab_read::capsule_protocol::read_visible_refs(&layout)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            coordinator
                .repair_snapshot()
                .await
                .unwrap()
                .materialization_gaps
                .is_empty()
        );
    }
}
