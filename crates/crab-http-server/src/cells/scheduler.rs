use std::time::Duration;

use crab_cell_runtime::{
    ApplicationIdentity, CatalogRole, CellAuthority, CellCatalog, DueCellScan, EffectRunOutcome,
    EffectSource, EffectSupervisor, InvocationError, MaintenanceTickCommand,
    MaintenanceTickOutcome, MaintenanceTickRequest, MutationIdentity, NodeDirectory, RequestId,
    SessionId, preferred_scanner,
};
use crab_storage::CellStorageLayout;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{REPOSITORY_NAMESPACE, RepositoryCellRouter, RepositoryModule};

const SCAN_INTERVAL: Duration = Duration::from_secs(1);
const MAX_LIVE_NODES: usize = 10_000;
const MAX_DUE_PER_CYCLE: usize = 128;
const EFFECT_LEASE_MS: u32 = 30_000;

/// Fleet-assigned scanner for repository Cell maintenance and effects.
pub(crate) struct RepositoryCellScheduler {
    catalog: CellCatalog,
    authority: CellAuthority,
    directory: NodeDirectory,
    router: RepositoryCellRouter,
    session: SessionId,
}

impl RepositoryCellScheduler {
    pub(crate) fn new(
        identity: ApplicationIdentity,
        layout: CellStorageLayout,
        directory: NodeDirectory,
        router: RepositoryCellRouter,
        session: SessionId,
    ) -> Self {
        Self {
            catalog: CellCatalog::new(layout.clone(), identity.tenant()),
            authority: CellAuthority::new(layout),
            directory,
            router,
            session,
        }
    }

    pub(crate) async fn run(self, cancellation: CancellationToken) -> crate::Result<()> {
        loop {
            if cancellation.is_cancelled() {
                return Ok(());
            }
            if let Err(error) = self.scan_once().await {
                tracing::warn!(error = %error, "repository Cell scheduler scan failed");
            }
            tokio::select! {
                () = cancellation.cancelled() => return Ok(()),
                () = tokio::time::sleep(SCAN_INTERVAL) => {}
            }
        }
    }

    async fn scan_once(&self) -> crate::Result<()> {
        let now_ms = super::unix_now_ms()?;
        let nodes = self
            .directory
            .live(now_ms, MAX_LIVE_NODES)
            .await?
            .into_iter()
            .map(|node| node.session())
            .collect::<Vec<_>>();
        let mut remaining = MAX_DUE_PER_CYCLE;
        for shard in 0_u8..=u8::MAX {
            if remaining == 0 || preferred_scanner(shard, &nodes)? != Some(self.session) {
                continue;
            }
            let mut scan = DueCellScan::new(&self.catalog, self.authority.clone(), shard).await?;
            while remaining != 0 {
                let Some(batch) = scan.next_batch(now_ms).await? else {
                    break;
                };
                for due in batch.into_iter().take(remaining) {
                    remaining -= 1;
                    if let Err(error) = self.process(due).await {
                        tracing::warn!(error = %error, "repository Cell scheduler item failed");
                    }
                }
            }
        }
        Ok(())
    }

    async fn process(&self, due: crab_cell_runtime::DueCell) -> crate::Result<()> {
        let entry = due.catalog().entry();
        if entry.namespace() != REPOSITORY_NAMESPACE || entry.role() != CatalogRole::Repository {
            return Ok(());
        }
        let repository = Uuid::from_bytes(entry.partition().try_into().map_err(|_| {
            crab_cell_runtime::Error::Catalog("repository partition is not a UUID")
        })?);
        let expected_commit_sequence = due
            .control()
            .value()
            .root
            .as_ref()
            .ok_or(crab_cell_runtime::Error::CellNotActive)?
            .commit_sequence;
        let scheduled = self.router.route_scheduler(repository).await?;
        let release_after = scheduled.should_release();
        let result = self
            .process_cell(scheduled.cell, expected_commit_sequence)
            .await;
        if !release_after {
            return result;
        }
        result.and(self.router.drain_local(repository).await)
    }

    async fn process_cell(
        &self,
        cell: super::RepositoryCell,
        expected_commit_sequence: u64,
    ) -> crate::Result<()> {
        let tick = cell
            .client
            .command::<MaintenanceTickCommand<RepositoryModule>>(
                &cell.target,
                mutation_identity()?,
                MaintenanceTickRequest {
                    expected_commit_sequence,
                },
            )
            .await;
        let processed = match tick {
            Ok(committed) => match committed.output {
                MaintenanceTickOutcome::Applied { processed } => processed,
                MaintenanceTickOutcome::Stale => return Ok(()),
            },
            Err(InvocationError::Rejected(_)) => return Ok(()),
            Err(error) => {
                tracing::warn!(error = %error, "repository Cell Tick was not resolved");
                return Ok(());
            }
        };
        if processed != 0 {
            return Ok(());
        }
        let supervisor = EffectSupervisor::<RepositoryModule>::new(
            EffectSource::new(cell.client, cell.target),
            self.router.effect_peer_client(),
            EFFECT_LEASE_MS,
        )?;
        match supervisor.run_once().await {
            Ok(EffectRunOutcome::Idle { .. })
            | Ok(EffectRunOutcome::Delivered { .. })
            | Ok(EffectRunOutcome::Retrying { .. })
            | Ok(EffectRunOutcome::Failed { .. })
            | Ok(EffectRunOutcome::LeaseLost { .. }) => Ok(()),
            Err(error) => {
                tracing::warn!(error = %error, "repository Cell effect was not resolved");
                Ok(())
            }
        }
    }
}

fn mutation_identity() -> crate::Result<MutationIdentity> {
    let now_ms = super::unix_now_ms()?;
    Ok(MutationIdentity {
        request_id: RequestId::from_bytes(Uuid::now_v7().into_bytes()),
        issued_at_ms: now_ms,
        expires_at_ms: now_ms
            .checked_add(60_000)
            .ok_or(crab_cell_runtime::Error::Command(
                "scheduler request expiry overflow",
            ))?,
    })
}

#[cfg(test)]
mod tests {
    use std::{future::Future, pin::Pin, sync::Arc};

    use crab_cell_runtime::{
        ApplicationId, CellReplica, CellRuntime, CellTarget, Digest, IncarnationId,
        NodeAdvertisement, NodeCapacity, Owner, PeerRoundTrip, PeerSigner, ReplicaLimits,
        SqlWorkerPool, TenantId,
    };
    use crab_storage::{CellStorageLayout, Store};
    use ed25519_dalek::SigningKey;
    use object_store::{memory::InMemory, path::Path};

    use super::*;
    use crate::cells::{REPOSITORY_MIGRATION, bootstrap_release_at, provision_repository};

    struct UnavailablePeer;

    impl PeerRoundTrip for UnavailablePeer {
        fn send(
            &self,
            _target: CellTarget,
            _request: Vec<u8>,
            _remaining_ms: u32,
        ) -> Pin<Box<dyn Future<Output = crab_cell_runtime::Result<Vec<u8>>> + Send + 'static>>
        {
            Box::pin(async { Err(crab_cell_runtime::Error::CellNotActive) })
        }
    }

    #[tokio::test]
    async fn scan_routes_an_idle_due_cell_and_publishes_tick_progress() {
        let identity = ApplicationIdentity::new(
            TenantId::from_bytes([1; 16]),
            ApplicationId::from_bytes([2; 16]),
        );
        let layout = CellStorageLayout::new(
            Store::new(Arc::new(InMemory::new())),
            Path::from("repository-scheduler"),
            *identity.application().as_bytes(),
        );
        let registry = Arc::new(crate::cells::compiled_registry().unwrap());
        let image = Digest::from_bytes([12; 32]);
        bootstrap_release_at(
            &layout,
            identity,
            &registry,
            &format!("sha256:{}", "0c".repeat(32)),
        )
        .await
        .unwrap();
        let repository = Uuid::from_bytes([3; 16]);
        let target = CellTarget::new(
            identity.tenant(),
            identity.application(),
            REPOSITORY_NAMESPACE,
            repository.as_bytes(),
        )
        .unwrap();
        let (proof, authority) = provision_repository(&layout, identity, &registry, &target)
            .await
            .unwrap();
        let session = SessionId::from_bytes([4; 16]);
        let endpoint = "https://localhost:8789".to_owned();
        let owner = Owner {
            session,
            endpoint: endpoint.clone(),
        };
        let observed = authority
            .create_initial(&proof, IncarnationId::from_bytes([5; 16]), owner.clone())
            .await
            .unwrap();
        let runtime =
            CellRuntime::new(SqlWorkerPool::new(1, 4).unwrap(), 16 * 1024 * 1024, session).unwrap();
        let replica = CellReplica::new(
            layout.clone(),
            *target.cell_id().as_bytes(),
            *observed.value().incarnation.as_bytes(),
            ReplicaLimits::default(),
        )
        .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let sqlite = directory.path().join("bootstrap.sqlite");
        let repository_bytes = repository.into_bytes();
        let handle = runtime
            .bootstrap(
                proof,
                replica,
                authority.clone(),
                observed,
                sqlite,
                move |transaction| {
                    transaction.execute_batch(REPOSITORY_MIGRATION)?;
                    transaction.execute(
                        "INSERT INTO repository_identity(singleton, repository_uuid) VALUES (1, ?1)",
                        [repository_bytes.as_slice()],
                    )?;
                    transaction.execute(
                        "INSERT INTO sys_inbox VALUES (?1, ?2, 1, X'', 0, 1, 1)",
                        ([9_u8; 32].as_slice(), [8_u8; 32].as_slice()),
                    )?;
                    Ok(())
                },
            )
            .await
            .unwrap();
        handle.drain().await.unwrap();
        let before = authority.load(target.cell_id()).await.unwrap().unwrap();
        assert_eq!(before.value().root.as_ref().unwrap().commit_sequence, 0);
        assert_eq!(before.value().next_due_ms, Some(1));

        let fleet = Digest::from_bytes([10; 32]);
        let certificate = Digest::from_bytes([11; 32]);
        let node_directory =
            NodeDirectory::new(layout.clone(), fleet, image, registry.release_digest());
        let key = SigningKey::from_bytes(&[6; 32]);
        let now_ms = super::super::unix_now_ms().unwrap();
        node_directory
            .create(
                NodeAdvertisement::sign(
                    session,
                    endpoint,
                    fleet,
                    certificate,
                    image,
                    registry.release_digest(),
                    &key,
                    1,
                    now_ms,
                    now_ms + 15_000,
                    registry.module_digests(),
                    vec![1],
                    NodeCapacity {
                        free_memory_bytes: 1024 * 1024 * 1024,
                        free_disk_bytes: 1024 * 1024 * 1024,
                        job_credits: 1,
                    },
                )
                .unwrap(),
                now_ms,
            )
            .await
            .unwrap();
        let router = RepositoryCellRouter::new(
            identity,
            layout.clone(),
            Arc::clone(&registry),
            runtime.clone(),
            super::super::RepositoryCellPeer::new(
                node_directory.clone(),
                Arc::new(PeerSigner::new(session, registry.release_digest(), key)),
                Arc::new(UnavailablePeer),
                owner,
            ),
            directory.path().join("session"),
        )
        .unwrap();
        RepositoryCellScheduler::new(identity, layout, node_directory, router, session)
            .scan_once()
            .await
            .unwrap();

        let after = authority.load(target.cell_id()).await.unwrap().unwrap();
        assert_eq!(after.value().root.as_ref().unwrap().commit_sequence, 1);
        assert!(after.value().next_due_ms.is_some_and(|due| due > now_ms));
        assert_eq!(after.value().state, crab_cell_runtime::ControlState::Idle);
        runtime.shutdown().await.unwrap();
    }
}
