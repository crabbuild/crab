use super::*;

pub(super) fn install_process_lease(node: &crab_cell_host::CellNode) {
    let cancellation = CancellationToken::new();
    let tasks = node
        .install_task_group(cancellation.clone(), CancellationToken::new())
        .expect("process task group");
    let lease = NodeLeaseGuard::new(0, 60_000).expect("process lease");
    node.install_node_lease(lease.clone())
        .expect("process readiness");
    // The test's static fleet advertisement does not renew the local guard.
    // Keep it live while the restored process performs slow object-store reads.
    tasks
        .spawn(async move {
            loop {
                tokio::select! {
                    () = cancellation.cancelled() => return Ok::<(), crab_cell_runtime::Error>(()),
                    () = tokio::time::sleep(Duration::from_secs(20)) => lease.renew(0, 60_000)?,
                }
            }
        })
        .expect("process lease renewal task");
}

pub(super) async fn restore_cells(
    node: &crab_cell_host::CellNode,
    layout: &CellStorageLayout,
    tenant: TenantId,
    application: ApplicationId,
    session: SessionId,
    fenced: Option<&crab_cell_runtime::node::FencedNodeSession>,
    directory: &FilePath,
    role: &str,
) -> Vec<crab_cell_runtime::cell::actor::CellHandle> {
    let authority = CellAuthority::new(layout.clone());
    let mut restored_cells = Vec::new();
    for (namespace, module, incarnation_byte) in [
        (fixture::SQL_NAMESPACE, fixture::SQL_MODULE, 40_u8),
        (fixture::KV_NAMESPACE, fixture::KV_MODULE, 41_u8),
        (fixture::BLOB_NAMESPACE, fixture::BLOB_MODULE, 42_u8),
        (fixture::QUEUE_NAMESPACE, fixture::QUEUE_MODULE, 43_u8),
        (fixture::CRON_NAMESPACE, fixture::CRON_MODULE, 45_u8),
        (fixture::WORKFLOW_NAMESPACE, fixture::WORKFLOW_MODULE, 46_u8),
    ] {
        let target = CellTarget::new(tenant, application, namespace, &partition_for_shard(0))
            .expect("restored target");
        let observed = authority
            .load(target.cell_id())
            .await
            .expect("prior authority")
            .expect("prior owner");
        let prior_root = observed.value().root.clone().expect("published root");
        let prior_epoch = observed.value().epoch;
        let proof = CellCatalog::new(layout.clone(), tenant)
            .lookup(target.cell_id())
            .await
            .expect("prior catalog")
            .expect("provisioned Cell");
        let replica = CellReplica::new(
            layout.clone(),
            *target.cell_id().as_bytes(),
            *IncarnationId::from_bytes([incarnation_byte; 16]).as_bytes(),
            ReplicaLimits::default(),
        )
        .expect("restored replica");
        let destination = directory.join(format!("{module}-{role}.sqlite"));
        let owner = Owner {
            session,
            endpoint: format!("https://public-{role}.internal:8081"),
        };
        let restored = if let Some(fenced) = fenced {
            node.runtime()
                .takeover_restored(
                    proof,
                    replica,
                    authority.clone(),
                    observed,
                    fenced.direct_takeover().expect("direct takeover proof"),
                    RecoveryManifestStore::new(layout.clone(), ReplicaLimits::default()),
                    destination,
                    owner,
                )
                .await
                .expect("exact-root takeover")
        } else {
            assert_eq!(
                observed.value().state,
                crab_cell_runtime::control::ControlState::Idle
            );
            assert!(observed.value().owner.is_none());
            node.runtime()
                .acquire_idle_restored(
                    proof,
                    replica,
                    authority.clone(),
                    observed,
                    destination,
                    owner,
                )
                .await
                .expect("exact-root idle restore")
        };
        let current = authority
            .load(target.cell_id())
            .await
            .expect("restored authority")
            .expect("restored owner");
        assert!(current.value().epoch > prior_epoch);
        assert_eq!(current.value().root.as_ref(), Some(&prior_root));
        assert_eq!(
            current.value().owner.as_ref().map(|owner| owner.session),
            Some(session)
        );
        restored_cells.push(restored);
    }
    restored_cells
}
