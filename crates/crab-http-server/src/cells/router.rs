use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use cellule_runtime::CellStorageLayout;
use cellule_runtime::{
    ACTIVE_CELL_NATIVE_BYTES, ACTIVE_CELL_PAGE_CACHE_BYTES, ApplicationIdentity, CatalogProof,
    CatalogRole, CellAuthority, CellCatalog, CellClient, CellDescription, CellHandle, CellReplica,
    CellRuntime, CellTarget, CellTransferDemand, ControlState, EffectPeerClient,
    MAX_ACTIVITY_PAYLOAD_BYTES, MigrationPeerClient, NodeByteReservation, NodeDirectory,
    NodeJobReservation, Owner, PeerOperation, PeerPrincipal, PeerRoundTrip, PeerSigner,
    PersistedWorkInventory, PlacementObservation, PlacementPlanner, Registry, ReleaseState,
    ReleaseStore, VersionedControl, peer_wire,
};
use tokio::sync::{Mutex, OwnedRwLockReadGuard, OwnedRwLockWriteGuard, RwLock};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{REPOSITORY_NAMESPACE, repository_replica_limits};
use crate::auth::Identity;

const ACTIVATION_SHARDS: usize = 4096;
const REBALANCE_INTERVAL: Duration = Duration::from_secs(15);
const REBALANCE_IDLE_MS: i64 = 60_000;

#[derive(Clone, Copy)]
struct RebalanceEvidence {
    generation: u64,
    first_seen_ms: i64,
    last_sample_ms: i64,
    samples: u8,
}

#[derive(Default)]
struct RebalanceProgress {
    released: usize,
    activated: usize,
}

#[derive(Clone)]
pub(crate) struct RepositoryCellRouter {
    identity: ApplicationIdentity,
    layout: CellStorageLayout,
    registry: Arc<Registry>,
    catalog: CellCatalog,
    authority: CellAuthority,
    runtime: CellRuntime,
    peer: RepositoryCellPeer,
    placement: PlacementPlanner,
    session_dir: PathBuf,
    recovery_artifacts: Option<Arc<super::RecoveryArtifactRegistry>>,
    activation: Arc<[Mutex<()>]>,
    operation: Arc<[Arc<RwLock<()>>]>,
    rebalance_evidence: Arc<Mutex<HashMap<cellule_runtime::CellId, RebalanceEvidence>>>,
}

#[derive(Clone)]
pub(crate) struct RepositoryCellPeer {
    directory: NodeDirectory,
    signer: Arc<PeerSigner>,
    round_trip: Arc<dyn PeerRoundTrip>,
    owner: Owner,
}

pub(crate) struct RepositoryCell {
    pub(crate) target: CellTarget,
    pub(crate) client: CellClient,
    handle: Option<CellHandle>,
    // Keep routing and the subsequent Cell operation in one lifecycle window.
    // Without this guard, an eager drain can race a second request and return
    // CellDraining or make activation observe CellAlreadyActive.
    _operation: Option<OwnedRwLockReadGuard<()>>,
}

pub(crate) struct ScheduledRepositoryCell {
    pub(crate) cell: RepositoryCell,
    release_after: bool,
}

impl ScheduledRepositoryCell {
    pub(crate) fn should_release(&self) -> bool {
        self.release_after
    }
}

impl RepositoryCellRouter {
    pub(crate) fn new(
        identity: ApplicationIdentity,
        layout: CellStorageLayout,
        registry: Arc<Registry>,
        runtime: CellRuntime,
        peer: RepositoryCellPeer,
        session_dir: PathBuf,
    ) -> crate::Result<Self> {
        if !session_dir.is_absolute() || peer.owner.endpoint.is_empty() {
            return Err(crate::Error::Config(
                "repository Cell routing requires an absolute session directory and endpoint",
            ));
        }
        Ok(Self {
            identity,
            catalog: CellCatalog::new(layout.clone(), identity.tenant()),
            authority: CellAuthority::new(layout.clone()),
            layout,
            registry,
            runtime,
            peer,
            placement: PlacementPlanner::default(),
            session_dir,
            recovery_artifacts: None,
            activation: (0..ACTIVATION_SHARDS)
                .map(|_| Mutex::new(()))
                .collect::<Vec<_>>()
                .into(),
            operation: (0..ACTIVATION_SHARDS)
                .map(|_| Arc::new(RwLock::new(())))
                .collect::<Vec<_>>()
                .into(),
            rebalance_evidence: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// Runs the private, bounded owner movement loop while server admission is live.
    pub(crate) async fn run_rebalance(&self, cancellation: CancellationToken) -> crate::Result<()> {
        let mut tick = tokio::time::interval(REBALANCE_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                () = cancellation.cancelled() => return Ok(()),
                _ = tick.tick() => {
                    match self.rebalance_once().await {
                        Ok(progress) if progress.released > 0 => {
                            tracing::info!(released = progress.released, activated = progress.activated, "Cell rebalance tick completed");
                        }
                        Err(error) => tracing::warn!(error = %error, "Cell rebalance tick failed"),
                        Ok(_) => {}
                    }
                }
            }
        }
    }

    async fn rebalance_once(&self) -> crate::Result<RebalanceProgress> {
        let now_ms = super::unix_now_ms()?;
        self.rebalance_once_at(now_ms).await
    }

    async fn rebalance_once_at(&self, now_ms: i64) -> crate::Result<RebalanceProgress> {
        let live = self.peer.directory.live(now_ms, 1_024).await?;
        let observations = live
            .iter()
            .filter_map(|node| {
                PlacementObservation::from_signed_advertisement(node, now_ms, false).ok()
            })
            .collect::<Vec<_>>();
        let Some(source) = observations
            .iter()
            .find(|node| node.session == self.peer.owner.session)
        else {
            return Ok(RebalanceProgress::default());
        };
        let candidates = self.runtime.idle_transfer_candidates().await?;
        let active = candidates
            .iter()
            .map(|(cell, _, _, _)| *cell)
            .collect::<HashSet<_>>();
        let mut evidence = self.rebalance_evidence.lock().await;
        evidence.retain(|cell, _| active.contains(cell));
        let demands = candidates
            .into_iter()
            .filter_map(|(cell, generation, last_used_ms, role)| {
                if role != CatalogRole::Application {
                    return None;
                }
                if last_used_ms < 0
                    || (!source.draining && now_ms.saturating_sub(last_used_ms) < REBALANCE_IDLE_MS)
                {
                    return None;
                }
                let observed = evidence.entry(cell).or_insert(RebalanceEvidence {
                    generation,
                    first_seen_ms: now_ms,
                    last_sample_ms: source.observed_at_ms,
                    samples: 1,
                });
                if observed.generation != generation {
                    *observed = RebalanceEvidence {
                        generation,
                        first_seen_ms: now_ms,
                        last_sample_ms: source.observed_at_ms,
                        samples: 1,
                    };
                }
                if observed.last_sample_ms != source.observed_at_ms {
                    observed.last_sample_ms = source.observed_at_ms;
                    observed.samples = observed.samples.saturating_add(1);
                }
                Some(CellTransferDemand {
                    cell,
                    source: self.peer.owner.session,
                    generation,
                    memory_bytes: ACTIVE_CELL_NATIVE_BYTES + ACTIVE_CELL_PAGE_CACHE_BYTES,
                    disk_bytes: repository_replica_limits().max_plan_bytes,
                    job_credits: 2,
                    resident_since_ms: observed.first_seen_ms,
                    last_moved_at_ms: None,
                    stable_observations: observed.samples,
                    settled: true,
                })
            })
            .collect::<Vec<_>>();
        drop(evidence);
        let mut progress = RebalanceProgress::default();
        for intent in self
            .placement
            .plan_transfers(now_ms, &observations, &demands)?
        {
            let Some(node) = live
                .iter()
                .find(|node| node.session() == intent.destination)
            else {
                continue;
            };
            let Some(proof) = self.catalog.lookup(intent.cell).await? else {
                continue;
            };
            if proof.entry().role() != CatalogRole::Application
                || proof.entry().namespace() != REPOSITORY_NAMESPACE
            {
                continue;
            }
            let target = CellTarget::new(
                self.identity.tenant(),
                self.identity.application(),
                proof.entry().namespace(),
                proof.entry().partition(),
            )?;
            let _operation = Arc::clone(&self.operation[activation_shard(&target)])
                .write_owned()
                .await;
            let still_idle = self
                .runtime
                .idle_transfer_candidates()
                .await?
                .into_iter()
                .any(|(cell, generation, last_used_ms, role)| {
                    cell == intent.cell
                        && generation == intent.generation
                        && role == CatalogRole::Application
                        && (source.draining
                            || now_ms.saturating_sub(last_used_ms) >= REBALANCE_IDLE_MS)
                });
            if !still_idle {
                continue;
            }
            if self
                .runtime
                .release_idle_cell(intent.cell, intent.source, intent.generation)
                .await
                .is_err()
            {
                continue;
            }
            progress.released += 1;
            if let Err(error) = self
                .peer
                .activate_remote(
                    target,
                    node.clone(),
                    &self.runtime_principal(&["cell.activate"]),
                    now_ms,
                )
                .await
            {
                tracing::warn!(cell = ?intent.cell, error = %error, "Cell released but receiver activation failed");
            } else {
                progress.activated += 1;
            }
        }
        Ok(progress)
    }

    pub(crate) async fn route(
        &self,
        repository: Uuid,
        principal: &Identity,
        action: &'static str,
    ) -> crate::Result<RepositoryCell> {
        validate_action(action)?;
        let target = CellTarget::new(
            self.identity.tenant(),
            self.identity.application(),
            REPOSITORY_NAMESPACE,
            repository.as_bytes(),
        )?;
        self.route_target(
            target,
            PeerPrincipal {
                issuer: principal.issuer.clone(),
                subject: principal.subject.clone(),
                actions: vec![action.to_owned()],
            },
        )
        .await
        .map(|scheduled| scheduled.cell)
    }

    pub(crate) async fn route_scheduler_target(
        &self,
        target: CellTarget,
    ) -> crate::Result<ScheduledRepositoryCell> {
        self.route_runtime(
            target,
            self.runtime_principal(&[
                "cell.activity.source",
                "cell.effect.source",
                "cell.scheduler.tick",
            ]),
        )
        .await
    }

    pub(crate) async fn route_projection(
        &self,
        repository: Uuid,
    ) -> crate::Result<ScheduledRepositoryCell> {
        let target = CellTarget::new(
            self.identity.tenant(),
            self.identity.application(),
            REPOSITORY_NAMESPACE,
            repository.as_bytes(),
        )?;
        self.route_runtime(target, self.runtime_principal(&["repository.projection"]))
            .await
    }

    pub(crate) async fn route_runtime(
        &self,
        target: CellTarget,
        principal: PeerPrincipal,
    ) -> crate::Result<ScheduledRepositoryCell> {
        if target.tenant() != self.identity.tenant()
            || target.application() != self.identity.application()
        {
            return Err(cellule_runtime::Error::PeerAuthorization(
                "runtime target belongs to another application",
            )
            .into());
        }
        self.route_target(target, principal).await
    }

    /// Activates a target on this node without consulting the fleet planner.
    ///
    /// A peer that was selected by the ingress planner uses this bounded seam
    /// so a second node cannot recursively choose a third destination. The
    /// normal authority CAS and actor admission still decide whether activation
    /// succeeds.
    pub(crate) async fn activate_local_target(
        &self,
        target: CellTarget,
        principal: PeerPrincipal,
    ) -> crate::Result<ScheduledRepositoryCell> {
        if target.namespace() != REPOSITORY_NAMESPACE {
            return Err(cellule_runtime::Error::PeerAuthorization(
                "peer activation targets an unsupported namespace",
            )
            .into());
        }
        let scheduled = self.route_target_inner(target, principal, false).await?;
        if scheduled.cell.handle.is_none() {
            return Err(cellule_runtime::Error::CellNotActive.into());
        }
        Ok(scheduled)
    }

    pub(crate) fn registry(&self) -> Arc<Registry> {
        Arc::clone(&self.registry)
    }

    pub(crate) fn recovery_scratch_directory(&self) -> PathBuf {
        self.session_dir.clone()
    }

    pub(crate) fn with_recovery_artifacts(
        mut self,
        registry: Arc<super::RecoveryArtifactRegistry>,
    ) -> Self {
        self.recovery_artifacts = Some(registry);
        self
    }

    pub(crate) fn recovery_artifacts(&self) -> Option<Arc<super::RecoveryArtifactRegistry>> {
        self.recovery_artifacts.clone()
    }

    pub(crate) fn recovery_disk_budget(&self) -> cellule_runtime::DiskBudget {
        self.runtime.local_disk_budget()
    }

    fn recovery_manifest_store(&self, scratch: PathBuf) -> cellule_runtime::RecoveryManifestStore {
        let store = cellule_runtime::RecoveryManifestStore::new(
            self.layout.clone(),
            repository_replica_limits(),
        )
        .with_recovery_disk(self.runtime.local_disk_budget())
        .with_recovery_scratch(scratch);
        self.recovery_artifacts
            .as_ref()
            .map_or(store.clone(), |artifacts| {
                store.with_recovery_artifacts(artifacts.clone())
            })
    }

    pub(crate) fn effect_peer_client(&self) -> EffectPeerClient {
        EffectPeerClient::new(
            Arc::clone(&self.peer.signer),
            self.runtime_principal(&["cell.effect.deliver", "cell.effect.resolve"]),
            Arc::clone(&self.peer.round_trip),
        )
    }

    pub(crate) async fn migrate_target(&self, target: CellTarget) -> crate::Result<()> {
        if target.tenant() != self.identity.tenant()
            || target.application() != self.identity.application()
        {
            return Err(cellule_runtime::Error::PeerAuthorization(
                "migration target belongs to another application",
            )
            .into());
        }
        loop {
            self.require_migration_release().await?;
            let proof = self
                .catalog
                .lookup(target.cell_id())
                .await?
                .ok_or(cellule_runtime::Error::CellNotActive)?;
            let control = self
                .authority
                .load(target.cell_id())
                .await?
                .ok_or(cellule_runtime::Error::CellNotActive)?;
            if control.value().state == ControlState::Tombstoned {
                return Ok(());
            }
            if self.registry.is_current_cell(
                proof.entry().namespace(),
                proof.entry().role(),
                control.value().code,
                control.value().schema,
            ) {
                return Ok(());
            }
            let plan = self
                .registry
                .next_migration(
                    proof.entry().namespace(),
                    control.value().code,
                    control.value().schema,
                )?
                .ok_or(cellule_runtime::Error::Registry(
                    "cataloged Cell has no migration to the current release",
                ))?;
            let expected = CellDescription {
                cell: target.cell_id(),
                incarnation: control.value().incarnation,
                code: control.value().code,
                schema: control.value().schema,
            };
            let scheduled = self
                .route_runtime(
                    target.clone(),
                    self.runtime_principal(&["cell.release.migrate"]),
                )
                .await?;
            let release_after = scheduled.should_release();
            match scheduled.cell.handle.as_ref() {
                Some(handle)
                    if handle.code() == plan.from_code()
                        && handle.schema() == plan.from_schema() =>
                {
                    handle.migrate(plan, super::unix_now_ms()?).await?;
                }
                Some(handle)
                    if handle.code() == plan.to_code() && handle.schema() >= plan.to_schema() => {}
                Some(_) => return Err(cellule_runtime::Error::Fenced.into()),
                None => {
                    self.migration_peer_client()
                        .migrate(target.clone(), expected, plan, super::unix_now_ms()?)
                        .await?;
                }
            }
            drop(scheduled.cell);
            if release_after {
                self.drain_local_target(&target).await?;
            }
        }
    }

    pub(crate) async fn persisted_work_target(
        &self,
        target: CellTarget,
    ) -> crate::Result<PersistedWorkInventory> {
        if target.tenant() != self.identity.tenant()
            || target.application() != self.identity.application()
        {
            return Err(cellule_runtime::Error::PeerAuthorization(
                "persisted-work target belongs to another application",
            )
            .into());
        }
        let role = self
            .catalog
            .lookup(target.cell_id())
            .await?
            .ok_or(cellule_runtime::Error::CellNotActive)?
            .entry()
            .role();
        let scheduled = self
            .route_runtime(
                target.clone(),
                self.runtime_principal(&["cell.release.inspect"]),
            )
            .await?;
        let release_after = scheduled.should_release();
        let inventory: crate::Result<PersistedWorkInventory> = match scheduled.cell.handle.as_ref()
        {
            Some(handle) => handle
                .persisted_work_inventory(role)
                .await
                .map_err(Into::into),
            None => Err(cellule_runtime::Error::CellNotActive.into()),
        };
        drop(scheduled.cell);
        let drained = if release_after {
            self.drain_local_target(&target).await
        } else {
            Ok(())
        };
        match (inventory, drained) {
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
            (Ok(inventory), Ok(())) => Ok(inventory),
        }
    }

    fn migration_peer_client(&self) -> MigrationPeerClient {
        MigrationPeerClient::new(
            Arc::clone(&self.peer.signer),
            self.runtime_principal(&["cell.release.migrate"]),
            Arc::clone(&self.peer.round_trip),
        )
    }

    async fn require_migration_release(&self) -> crate::Result<()> {
        let release = ReleaseStore::new(self.layout.clone(), self.identity)?
            .load()
            .await?
            .ok_or(cellule_runtime::Error::Release(
                "release is unavailable during Cell migration",
            ))?;
        if !matches!(
            release.record().state(),
            ReleaseState::Activating | ReleaseState::Maintenance
        ) || release.record().desired() != Some(self.registry.release_digest())
        {
            return Err(cellule_runtime::Error::Release(
                "Cell migration requires the compiled release to be activating or in maintenance",
            )
            .into());
        }
        Ok(())
    }

    pub(crate) fn reserve_activity_payloads(&self) -> crate::Result<NodeByteReservation> {
        self.runtime
            .try_reserve_node_bytes(2 * MAX_ACTIVITY_PAYLOAD_BYTES)
            .map_err(Into::into)
    }

    pub(crate) fn reserve_primitive_job(&self) -> crate::Result<Option<NodeJobReservation>> {
        self.runtime.try_reserve_worker_job().map_err(Into::into)
    }

    async fn route_target(
        &self,
        target: CellTarget,
        principal: PeerPrincipal,
    ) -> crate::Result<ScheduledRepositoryCell> {
        self.route_target_inner(target, principal, true).await
    }

    async fn route_target_inner(
        &self,
        target: CellTarget,
        principal: PeerPrincipal,
        use_placement: bool,
    ) -> crate::Result<ScheduledRepositoryCell> {
        let shard = activation_shard(&target);
        let operation = Arc::clone(&self.operation[shard]).read_owned().await;
        if let Some(routed) = self.route_existing(&target, &principal).await? {
            return Ok(ScheduledRepositoryCell {
                cell: RepositoryCell {
                    _operation: Some(operation),
                    ..routed
                },
                release_after: false,
            });
        }

        drop(operation);
        let _activation = self.activation[shard].lock().await;
        let operation = Arc::clone(&self.operation[shard]).read_owned().await;
        if let Some(routed) = self.route_existing(&target, &principal).await? {
            return Ok(ScheduledRepositoryCell {
                cell: RepositoryCell {
                    _operation: Some(operation),
                    ..routed
                },
                release_after: false,
            });
        }

        let proof = self
            .catalog
            .lookup(target.cell_id())
            .await?
            .ok_or(cellule_runtime::Error::CellNotActive)?;
        let observed = self
            .authority
            .load(target.cell_id())
            .await?
            .ok_or(cellule_runtime::Error::CellNotActive)?;
        if observed.value().root.is_none() {
            return Err(cellule_runtime::Error::CellNotActive.into());
        }
        if use_placement
            && self
                .activate_preferred_node(&target, &observed, &principal)
                .await?
        {
            if let Some(routed) = self.route_existing(&target, &principal).await? {
                return Ok(ScheduledRepositoryCell {
                    cell: RepositoryCell {
                        _operation: Some(operation),
                        ..routed
                    },
                    release_after: false,
                });
            }
            return Err(cellule_runtime::Error::CellNotActive.into());
        }
        let mut routed = self
            .activate_or_route(target, proof, observed, &principal)
            .await?;
        routed.cell._operation = Some(operation);
        Ok(routed)
    }

    async fn activate_preferred_node(
        &self,
        target: &CellTarget,
        observed: &VersionedControl,
        principal: &PeerPrincipal,
    ) -> crate::Result<bool> {
        if observed
            .value()
            .owner
            .as_ref()
            .is_some_and(|owner| owner.session == self.peer.owner.session)
        {
            return Ok(false);
        }
        let now_ms = super::unix_now_ms()?;
        let node = if let Some(owner) = observed.value().owner.as_ref()
            && let Some(node) = self
                .peer
                .directory
                .preferred_recovery_node(owner.session, now_ms)
                .await?
        {
            node
        } else {
            let Some(score) = self
                .peer
                .directory
                .choose_advertised_placement(&self.placement, target.cell_id(), now_ms, 1_024)
                .await?
            else {
                // A fully legacy fleet has no placement contract yet. Preserve
                // ordinary local acquisition until the rollout has one signed
                // observation to consume; mixed fleets never select legacy nodes.
                return Ok(false);
            };
            self.peer
                .directory
                .load(score.session, now_ms)
                .await?
                .ok_or(cellule_runtime::Error::CellNotActive)?
                .advertisement()
                .clone()
        };
        if node.session() == self.peer.owner.session {
            return Ok(false);
        }
        match self
            .peer
            .activate_remote(target.clone(), node, principal, now_ms)
            .await
        {
            Ok(()) => Ok(true),
            Err(crate::Error::Cell(
                error @ (cellule_runtime::Error::CellNotActive
                | cellule_runtime::Error::Deadline
                | cellule_runtime::Error::PeerTransport { .. }
                | cellule_runtime::Error::PeerTransportUnknown { .. }),
            )) => {
                // Placement is advisory. A stale or unreachable destination must
                // not turn a cold request into an outage; local authority CAS
                // remains the fail-closed acquisition path.
                tracing::debug!(error = %error, "preferred Cell activation was unavailable");
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }

    pub(crate) async fn verify_repositories(
        &self,
        repositories: impl IntoIterator<Item = (Uuid, crate::catalog::RepositoryApplicationState)>,
    ) -> crate::Result<()> {
        super::verify_repository_cells(&self.layout, self.identity, repositories).await
    }

    async fn route_existing(
        &self,
        target: &CellTarget,
        principal: &PeerPrincipal,
    ) -> crate::Result<Option<RepositoryCell>> {
        if let Some(handle) = self
            .runtime
            .resident_handle(target, CatalogRole::Application)
            .await?
        {
            return Ok(Some(RepositoryCell {
                target: target.clone(),
                client: CellClient::local(Arc::clone(&self.registry), handle.clone()),
                handle: Some(handle),
                _operation: None,
            }));
        }
        let Some(proof) = self.catalog.lookup(target.cell_id()).await? else {
            return Ok(None);
        };
        let Some(control) = self.authority.load(target.cell_id()).await? else {
            return Ok(None);
        };
        if control.value().root.is_none() {
            return Err(cellule_runtime::Error::CellNotActive.into());
        }
        if control.value().state == ControlState::Tombstoned {
            return Err(cellule_runtime::Error::CellNotActive.into());
        }
        let Some(owner) = control.value().owner.as_ref() else {
            return Ok(None);
        };
        if owner.session != self.peer.owner.session {
            // Only a missing or canonically expired session can begin takeover.
            // Corrupt or foreign directory state must fail closed.
            return if self.remote_owner_is_live(owner).await? {
                Ok(Some(self.peer(target.clone(), principal.clone())))
            } else {
                Ok(None)
            };
        }
        if owner != &self.peer.owner {
            return Err(cellule_runtime::Error::Fenced.into());
        }
        Ok(self
            .runtime
            .local_handle(proof, &control)
            .await?
            .map(|handle| RepositoryCell {
                target: target.clone(),
                client: CellClient::local(Arc::clone(&self.registry), handle.clone()),
                handle: Some(handle),
                _operation: None,
            }))
    }

    async fn activate_or_route(
        &self,
        target: CellTarget,
        proof: CatalogProof,
        observed: VersionedControl,
        principal: &PeerPrincipal,
    ) -> crate::Result<ScheduledRepositoryCell> {
        if observed.value().state == ControlState::Tombstoned {
            return Err(cellule_runtime::Error::CellNotActive.into());
        }
        let remote_owner = observed
            .value()
            .owner
            .as_ref()
            .filter(|owner| owner.session != self.peer.owner.session);
        let takeover = if let Some(owner) = remote_owner {
            if self.remote_owner_is_live(owner).await? {
                return Ok(ScheduledRepositoryCell {
                    cell: self.peer(target, principal.clone()),
                    release_after: false,
                });
            }
            let now_ms = super::unix_now_ms()?;
            match self
                .peer
                .directory
                .takeover_proof(owner.session, self.peer.owner.session, now_ms)
                .await?
            {
                Some(proof) => Some(proof),
                None => Some(
                    self.peer
                        .directory
                        .claim_expired_for_takeover(owner.session, self.peer.owner.session, now_ms)
                        .await?,
                ),
            }
        } else {
            None
        };
        if observed.value().owner.as_ref().is_some_and(|owner| {
            owner.session == self.peer.owner.session && owner != &self.peer.owner
        }) {
            return Err(cellule_runtime::Error::Fenced.into());
        }

        let replica = CellReplica::new(
            self.layout.clone(),
            *target.cell_id().as_bytes(),
            *observed.value().incarnation.as_bytes(),
            repository_replica_limits(),
        )
        .map_err(cellule_runtime::Error::from)?;
        let destination = self.activation_path(&target).await?;
        let recovery_scratch = destination
            .parent()
            .ok_or(crate::Error::Config(
                "Cell activation destination has no parent",
            ))?
            .to_owned();
        let handle = match observed.value().state {
            ControlState::Idle => {
                self.runtime
                    .acquire_idle_restored(
                        proof,
                        replica,
                        self.authority.clone(),
                        observed,
                        destination,
                        self.peer.owner.clone(),
                    )
                    .await?
            }
            ControlState::Recovering | ControlState::Serving => {
                if let Some(takeover) = takeover {
                    self.runtime
                        .takeover_restored(
                            proof,
                            replica,
                            self.authority.clone(),
                            observed,
                            takeover,
                            self.recovery_manifest_store(recovery_scratch.clone()),
                            destination,
                            self.peer.owner.clone(),
                        )
                        .await?
                } else {
                    self.runtime
                        .activate_restored(
                            proof,
                            replica,
                            self.authority.clone(),
                            observed,
                            self.recovery_manifest_store(recovery_scratch),
                            destination,
                        )
                        .await?
                }
            }
            ControlState::Tombstoned => {
                return Err(cellule_runtime::Error::CellNotActive.into());
            }
        };
        Ok(ScheduledRepositoryCell {
            cell: RepositoryCell {
                target,
                client: CellClient::local(Arc::clone(&self.registry), handle.clone()),
                handle: Some(handle),
                _operation: None,
            },
            release_after: true,
        })
    }

    fn peer(&self, target: CellTarget, principal: PeerPrincipal) -> RepositoryCell {
        RepositoryCell {
            target,
            client: CellClient::peer(
                Arc::clone(&self.registry),
                Arc::clone(&self.peer.signer),
                principal,
                Arc::clone(&self.peer.round_trip),
            ),
            handle: None,
            _operation: None,
        }
    }

    fn runtime_principal(&self, actions: &[&str]) -> PeerPrincipal {
        PeerPrincipal {
            issuer: format!(
                "crab-runtime:{}",
                encode_hex(self.peer.directory.fleet().as_bytes())
            ),
            subject: encode_hex(self.peer.owner.session.as_bytes()),
            actions: actions.iter().map(|action| (*action).to_owned()).collect(),
        }
    }

    async fn remote_owner_is_live(&self, owner: &Owner) -> crate::Result<bool> {
        Ok(self
            .peer
            .directory
            .is_live(owner.session, super::unix_now_ms()?)
            .await?)
    }

    async fn activation_path(&self, target: &CellTarget) -> crate::Result<PathBuf> {
        let directory = self
            .session_dir
            .join(encode_hex(target.cell_id().as_bytes()));
        tokio::fs::create_dir_all(&directory).await?;
        Ok(directory.join(format!("{}.sqlite", Uuid::now_v7())))
    }

    #[cfg(test)]
    pub(crate) async fn drain_local(&self, repository: Uuid) -> crate::Result<()> {
        let target = CellTarget::new(
            self.identity.tenant(),
            self.identity.application(),
            REPOSITORY_NAMESPACE,
            repository.as_bytes(),
        )?;
        self.drain_local_target(&target).await
    }

    pub(crate) async fn drain_local_target(&self, target: &CellTarget) -> crate::Result<()> {
        let _operation: OwnedRwLockWriteGuard<()> =
            Arc::clone(&self.operation[activation_shard(target)])
                .write_owned()
                .await;
        let proof = self
            .catalog
            .lookup(target.cell_id())
            .await?
            .ok_or(cellule_runtime::Error::CellNotActive)?;
        let control = self
            .authority
            .load(target.cell_id())
            .await?
            .ok_or(cellule_runtime::Error::CellNotActive)?;
        let handle = self
            .runtime
            .local_handle(proof, &control)
            .await?
            .ok_or(cellule_runtime::Error::CellNotActive)?;
        handle.drain().await.map_err(Into::into)
    }
}

impl RepositoryCellPeer {
    pub(crate) fn new(
        directory: NodeDirectory,
        signer: Arc<PeerSigner>,
        round_trip: Arc<dyn PeerRoundTrip>,
        owner: Owner,
    ) -> Self {
        Self {
            directory,
            signer,
            round_trip,
            owner,
        }
    }

    async fn activate_remote(
        &self,
        target: CellTarget,
        node: cellule_runtime::NodeAdvertisement,
        _principal: &PeerPrincipal,
        now_ms: i64,
    ) -> crate::Result<()> {
        // Verification subtracts transit time from the signed lifetime. Leave
        // room beyond the 30-second request deadline or every remote hint fails.
        let expires_at_ms = now_ms
            .checked_add(60_000)
            .ok_or(cellule_runtime::Error::Peer("activation deadline overflow"))?;
        let principal = PeerPrincipal {
            issuer: format!(
                "crab-runtime:{}",
                encode_hex(self.directory.fleet().as_bytes())
            ),
            subject: encode_hex(self.owner.session.as_bytes()),
            actions: vec!["cell.activate".to_owned()],
        };
        let request = self.signer.sign(
            principal,
            now_ms,
            expires_at_ms,
            30_000,
            PeerOperation::Read(peer_wire::ReadRequest {
                target: Some(peer_target(&target)),
                timeout_ms: 30_000,
                minimum: None,
                operation: Some(peer_wire::read_request::Operation::Describe(true)),
            }),
        )?;
        let reply = self
            .round_trip
            .send_to_node(target.clone(), node, request, 30_000)
            .await?;
        let reply = cellule_runtime::decode_peer_reply(&reply)?;
        match reply.outcome {
            Some(peer_wire::peer_reply::Outcome::Read(read)) => match read.result {
                Some(peer_wire::read_reply::Result::Description(description))
                    if description.cell_id.as_slice() == target.cell_id().as_bytes() =>
                {
                    Ok(())
                }
                _ => Err(cellule_runtime::Error::Peer(
                    "preferred node did not activate the requested Cell",
                )
                .into()),
            },
            Some(peer_wire::peer_reply::Outcome::Error(error)) => {
                let _ = error;
                Err(cellule_runtime::Error::Peer("preferred node rejected activation").into())
            }
            _ => Err(cellule_runtime::Error::Peer(
                "preferred node returned an unexpected activation reply",
            )
            .into()),
        }
    }
}

fn peer_target(target: &CellTarget) -> peer_wire::Target {
    peer_wire::Target {
        tenant_id: target.tenant().as_bytes().to_vec(),
        application_id: target.application().as_bytes().to_vec(),
        namespace_id: target.namespace().as_bytes().to_vec(),
        partition: target.partition().to_vec(),
    }
}

fn activation_shard(target: &CellTarget) -> usize {
    let cell = target.cell_id();
    let bytes = cell.as_bytes();
    ((usize::from(bytes[0]) << 4) | usize::from(bytes[1] >> 4)) % ACTIVATION_SHARDS
}

fn validate_action(action: &str) -> crate::Result<()> {
    if !matches!(
        action,
        "repository.read"
            | "repository.issue.create"
            | "repository.comment.create"
            | "repository.issue.update"
            | "repository.comment.update"
            | "repository.label.create"
            | "repository.label.update"
            | "repository.label.delete"
            | "repository.status.create"
            | "repository.check.create"
            | "repository.check.update"
            | "repository.settings.protections"
            | "repository.settings.lifecycle"
            | "repository.pull.create"
            | "repository.pull.update"
            | "repository.pull.comment"
            | "repository.pull.review"
            | "repository.pull.review.thread"
            | "repository.pull.merge"
            | "repository.release.create"
            | "repository.release.update"
            | "repository.release.asset"
    ) {
        return Err(cellule_runtime::Error::PeerAuthorization(
            "repository route requested an unknown action",
        )
        .into());
    }
    Ok(())
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        pin::Pin,
        sync::{Arc, Mutex},
        time::UNIX_EPOCH,
    };

    use cellule_runtime::{
        ApplicationId, IncarnationId, MutationIdentity, NodeAdvertisement, NodeCapacity,
        PeerVerifier, RequestId, SessionId, SqlWorkerPool, TenantId, Transition,
    };
    use crab_storage::{StorageReadKind, Store};
    use ed25519_dalek::SigningKey;
    use object_store::{memory::InMemory, path::Path as ObjectPath};

    use super::*;
    use crate::cells::{
        bootstrap_release_at, initialize_repository_schema,
        repository::{CreateIssue, CreateIssueInput, GetIssue, RepositoryAuthor},
    };

    struct UnavailablePeer;

    struct ActivatingPeer {
        destination: RepositoryCellRouter,
        source: SessionId,
        release: cellule_runtime::Digest,
        verifying_key: ed25519_dalek::VerifyingKey,
        now_ms: i64,
    }

    impl PeerRoundTrip for ActivatingPeer {
        fn send(
            &self,
            _target: CellTarget,
            _request: Vec<u8>,
            _remaining_ms: u32,
        ) -> Pin<Box<dyn Future<Output = cellule_runtime::Result<Vec<u8>>> + Send + 'static>>
        {
            Box::pin(async { Err(cellule_runtime::Error::CellNotActive) })
        }

        fn send_to_node(
            &self,
            target: CellTarget,
            _node: NodeAdvertisement,
            request: Vec<u8>,
            _remaining_ms: u32,
        ) -> Pin<Box<dyn Future<Output = cellule_runtime::Result<Vec<u8>>> + Send + 'static>>
        {
            let verified = PeerVerifier::new(self.source, self.release, self.verifying_key)
                .verify(&request, self.now_ms)
                .is_ok();
            let destination = self.destination.clone();
            Box::pin(async move {
                if !verified {
                    return Err(cellule_runtime::Error::PeerAuthorization(
                        "invalid activation",
                    ));
                }
                let scheduled = destination
                    .activate_local_target(
                        target.clone(),
                        destination.runtime_principal(&["cell.activate"]),
                    )
                    .await
                    .map_err(|_| cellule_runtime::Error::CellNotActive)?;
                let handle = scheduled
                    .cell
                    .handle
                    .as_ref()
                    .ok_or(cellule_runtime::Error::CellNotActive)?;
                cellule_runtime::encode_peer_reply(&peer_wire::PeerReply {
                    outcome: Some(peer_wire::peer_reply::Outcome::Read(peer_wire::ReadReply {
                        receipt: None,
                        result: Some(peer_wire::read_reply::Result::Description(
                            peer_wire::CellDescription {
                                cell_id: target.cell_id().as_bytes().to_vec(),
                                incarnation: handle.incarnation().as_bytes().to_vec(),
                                code: handle.code().as_bytes().to_vec(),
                                schema: handle.schema(),
                            },
                        )),
                    })),
                })
            })
        }
    }

    struct DelayedActivationPeer {
        session: SessionId,
        release: cellule_runtime::Digest,
        key: ed25519_dalek::VerifyingKey,
        received_at_ms: i64,
    }

    impl PeerRoundTrip for DelayedActivationPeer {
        fn send(
            &self,
            _target: CellTarget,
            _request: Vec<u8>,
            _remaining_ms: u32,
        ) -> Pin<Box<dyn Future<Output = cellule_runtime::Result<Vec<u8>>> + Send + 'static>>
        {
            Box::pin(async { Err(cellule_runtime::Error::CellNotActive) })
        }

        fn send_to_node(
            &self,
            _target: CellTarget,
            _node: NodeAdvertisement,
            request: Vec<u8>,
            remaining_ms: u32,
        ) -> Pin<Box<dyn Future<Output = cellule_runtime::Result<Vec<u8>>> + Send + 'static>>
        {
            let verified = remaining_ms == 30_000
                && PeerVerifier::new(self.session, self.release, self.key)
                    .verify(&request, self.received_at_ms)
                    .is_ok();
            Box::pin(async move {
                if !verified {
                    return Err(cellule_runtime::Error::Peer(
                        "activation authorization expired in transit",
                    ));
                }
                Err(cellule_runtime::Error::Control(
                    "activation reached enrolled peer",
                ))
            })
        }
    }

    #[tokio::test]
    async fn remote_activation_authorization_survives_transit() {
        let session = SessionId::from_bytes([1; 16]);
        let successor = SessionId::from_bytes([2; 16]);
        let release = cellule_runtime::Digest::from_bytes([3; 32]);
        let fleet = cellule_runtime::Digest::from_bytes([4; 32]);
        let image = cellule_runtime::Digest::from_bytes([5; 32]);
        let key = SigningKey::from_bytes(&[6; 32]);
        let directory = NodeDirectory::new(
            CellStorageLayout::new(
                Store::new(Arc::new(InMemory::new())).for_cellule().unwrap(),
                ObjectPath::from("activation-transit"),
                [7; 16],
            ),
            fleet,
            image,
            release,
        );
        let peer = RepositoryCellPeer::new(
            directory,
            Arc::new(PeerSigner::new(session, release, key.clone())),
            Arc::new(DelayedActivationPeer {
                session,
                release,
                key: key.verifying_key(),
                received_at_ms: 1_001,
            }),
            owner(session),
        );
        let node = NodeAdvertisement::sign(
            cellule_runtime::NodeId::from_bytes(*successor.as_bytes()),
            successor,
            owner(successor).endpoint,
            fleet,
            cellule_runtime::Digest::from_bytes([8; 32]),
            image,
            release,
            &SigningKey::from_bytes(&[9; 32]),
            1,
            1_000,
            11_000,
            vec![cellule_runtime::Digest::from_bytes([10; 32])],
            vec![1],
            cellule_runtime::NodeFailureDomain::default(),
            NodeCapacity {
                free_memory_bytes: 1,
                free_disk_bytes: 1,
                job_credits: 1,
                ..NodeCapacity::default()
            },
        )
        .unwrap();
        let target = CellTarget::new(
            TenantId::from_bytes([11; 16]),
            ApplicationId::from_bytes([7; 16]),
            REPOSITORY_NAMESPACE,
            &[12; 16],
        )
        .unwrap();
        let error = peer
            .activate_remote(
                target,
                node,
                &PeerPrincipal {
                    issuer: "urn:crab:local".into(),
                    subject: "operator".into(),
                    actions: vec!["repository.issue.create".into()],
                },
                1_000,
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            crate::Error::Cell(cellule_runtime::Error::Control(
                "activation reached enrolled peer"
            ))
        ));
    }

    impl PeerRoundTrip for UnavailablePeer {
        fn send(
            &self,
            _target: CellTarget,
            _request: Vec<u8>,
            _remaining_ms: u32,
        ) -> Pin<Box<dyn Future<Output = cellule_runtime::Result<Vec<u8>>> + Send + 'static>>
        {
            Box::pin(async { Err(cellule_runtime::Error::CellNotActive) })
        }
    }

    #[tokio::test]
    async fn route_reuses_restores_idle_and_takes_over_stale_owner() {
        let identity = ApplicationIdentity::new(
            TenantId::from_bytes([1; 16]),
            ApplicationId::from_bytes([2; 16]),
        );
        let reads = Arc::new(Mutex::new(Vec::<StorageReadKind>::new()));
        let observed_reads = Arc::clone(&reads);
        let layout = CellStorageLayout::new(
            Store::new(Arc::new(InMemory::new()))
                .with_read_request_observer(Arc::new(move |kind| {
                    observed_reads
                        .lock()
                        .expect("read observer lock")
                        .push(kind)
                }))
                .for_cellule()
                .unwrap(),
            ObjectPath::from("repository-router"),
            *identity.application().as_bytes(),
        );
        let registry = Arc::new(crate::cells::compiled_registry().unwrap());
        bootstrap_release_at(
            &layout,
            identity,
            &registry,
            &format!("sha256:{}", "a".repeat(64)),
        )
        .await
        .unwrap();
        let principal = Identity {
            issuer: "https://crab.build".into(),
            subject: "user-1".into(),
            name: "Crab User".into(),
        };
        let repository = Uuid::from_bytes([3; 16]);
        let first_session = SessionId::from_bytes([4; 16]);
        let first_runtime = CellRuntime::new(
            SqlWorkerPool::new(1, 10).unwrap(),
            16 * 1024 * 1024,
            first_session,
        )
        .unwrap();
        let first_dir = tempfile::TempDir::new().unwrap();
        let target = CellTarget::new(
            identity.tenant(),
            identity.application(),
            REPOSITORY_NAMESPACE,
            repository.as_bytes(),
        )
        .unwrap();
        let (proof, authority) =
            crate::cells::provision_repository(&layout, identity, &registry, &target)
                .await
                .unwrap();
        let observed = authority
            .create_initial(
                &proof,
                IncarnationId::from_bytes([9; 16]),
                owner(first_session),
            )
            .await
            .unwrap();
        let repository_bytes = repository.into_bytes();
        first_runtime
            .bootstrap(
                proof,
                CellReplica::new(
                    layout.clone(),
                    *target.cell_id().as_bytes(),
                    *observed.value().incarnation.as_bytes(),
                    cellule_runtime::ReplicaLimits::default(),
                )
                .unwrap(),
                authority,
                observed,
                first_dir.path().join("bootstrap.sqlite"),
                move |transaction| {
                    initialize_repository_schema(transaction)?;
                    transaction.execute(
                        "INSERT INTO repository_identity(singleton, repository_uuid) VALUES (1, ?1)",
                        [repository_bytes.as_slice()],
                    )?;
                    Ok(())
                },
            )
            .await
            .unwrap();
        let first = router(
            identity,
            layout.clone(),
            Arc::clone(&registry),
            first_runtime.clone(),
            first_session,
            first_dir.path().to_path_buf(),
        );

        let routed = first
            .route(repository, &principal, "repository.issue.create")
            .await
            .unwrap();
        let created = routed
            .client
            .command::<CreateIssue>(
                &routed.target,
                mutation(5),
                CreateIssueInput {
                    submission_id: [8; 16],
                    author: RepositoryAuthor {
                        issuer: principal.issuer.clone(),
                        subject: principal.subject.clone(),
                        name: principal.name.clone(),
                    },
                    title: "Routed issue".into(),
                    body: "published through the repository router".into(),
                },
            )
            .await
            .unwrap();
        let crate::cells::repository::CreateIssueOutcome::Created(created_issue) = &created.output
        else {
            panic!("successful issue command returned a rejection outcome");
        };
        reads.lock().unwrap().clear();
        let reused = first
            .route(repository, &principal, "repository.read")
            .await
            .unwrap();
        assert!(reads.lock().unwrap().is_empty());
        assert_eq!(
            reused
                .client
                .query::<GetIssue>(&reused.target, Some(created.receipt), created_issue.number)
                .await
                .unwrap()
                .output,
            Some(created_issue.as_ref().clone())
        );
        first_runtime.shutdown().await.unwrap();

        let second_session = SessionId::from_bytes([6; 16]);
        let second_runtime = CellRuntime::new(
            SqlWorkerPool::new(1, 10).unwrap(),
            16 * 1024 * 1024,
            second_session,
        )
        .unwrap();
        let second_dir = tempfile::TempDir::new().unwrap();
        let second = router(
            identity,
            layout.clone(),
            Arc::clone(&registry),
            second_runtime.clone(),
            second_session,
            second_dir.path().to_path_buf(),
        );
        let restored = second
            .route(repository, &principal, "repository.read")
            .await
            .unwrap();
        assert_eq!(
            restored
                .client
                .query::<GetIssue>(
                    &restored.target,
                    Some(created.receipt),
                    created_issue.number,
                )
                .await
                .unwrap()
                .output,
            Some(created_issue.as_ref().clone())
        );
        second_runtime.shutdown().await.unwrap();

        let authority = CellAuthority::new(layout.clone());
        let idle = authority.load(target.cell_id()).await.unwrap().unwrap();
        let stale_session = SessionId::from_bytes([10; 16]);
        let now_ms = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        )
        .unwrap();
        let stale_issued_at_ms = now_ms - 20_000;
        let directory = NodeDirectory::new(
            layout.clone(),
            cellule_runtime::Digest::from_bytes([21; 32]),
            cellule_runtime::Digest::from_bytes([22; 32]),
            registry.release_digest(),
        );
        directory
            .create(
                NodeAdvertisement::sign(
                    cellule_runtime::NodeId::from_bytes(*stale_session.as_bytes()),
                    stale_session,
                    owner(stale_session).endpoint,
                    cellule_runtime::Digest::from_bytes([21; 32]),
                    cellule_runtime::Digest::from_bytes([23; 32]),
                    cellule_runtime::Digest::from_bytes([22; 32]),
                    registry.release_digest(),
                    &SigningKey::from_bytes(&[10; 32]),
                    1,
                    stale_issued_at_ms,
                    stale_issued_at_ms + 15_000,
                    registry.module_digests(),
                    vec![1],
                    cellule_runtime::NodeFailureDomain::default(),
                    NodeCapacity {
                        free_memory_bytes: 1,
                        free_disk_bytes: 1,
                        job_credits: 1,
                        ..NodeCapacity::default()
                    },
                )
                .unwrap(),
                stale_issued_at_ms,
            )
            .await
            .unwrap();
        let stale = idle.value().takeover(owner(stale_session)).unwrap();
        authority
            .transition(&idle, stale, Transition::Takeover)
            .await
            .unwrap();
        let third_session = SessionId::from_bytes([11; 16]);
        directory
            .create(
                NodeAdvertisement::sign(
                    cellule_runtime::NodeId::from_bytes(*third_session.as_bytes()),
                    third_session,
                    owner(third_session).endpoint,
                    cellule_runtime::Digest::from_bytes([21; 32]),
                    cellule_runtime::Digest::from_bytes([24; 32]),
                    cellule_runtime::Digest::from_bytes([22; 32]),
                    registry.release_digest(),
                    &SigningKey::from_bytes(&[11; 32]),
                    1,
                    now_ms,
                    now_ms + 15_000,
                    registry.module_digests(),
                    vec![1],
                    cellule_runtime::NodeFailureDomain::default(),
                    NodeCapacity {
                        free_memory_bytes: 1,
                        free_disk_bytes: 1,
                        job_credits: 1,
                        ..NodeCapacity::default()
                    },
                )
                .unwrap(),
                now_ms,
            )
            .await
            .unwrap();
        let third_runtime = CellRuntime::new(
            SqlWorkerPool::new(1, 10).unwrap(),
            16 * 1024 * 1024,
            third_session,
        )
        .unwrap();
        let third_dir = tempfile::TempDir::new().unwrap();
        let third = router(
            identity,
            layout,
            Arc::clone(&registry),
            third_runtime.clone(),
            third_session,
            third_dir.path().to_path_buf(),
        );

        let recovered = third
            .route(repository, &principal, "repository.read")
            .await
            .unwrap();
        assert_eq!(
            recovered
                .client
                .query::<GetIssue>(
                    &recovered.target,
                    Some(created.receipt),
                    created_issue.number,
                )
                .await
                .unwrap()
                .output,
            Some(created_issue.as_ref().clone())
        );
        let owned = authority.load(target.cell_id()).await.unwrap().unwrap();
        assert_eq!(owned.value().owner.as_ref(), Some(&owner(third_session)));
        third_runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn fleet_rebalance_releases_settled_cell_and_restores_its_result() {
        let identity = ApplicationIdentity::new(
            TenantId::from_bytes([41; 16]),
            ApplicationId::from_bytes([42; 16]),
        );
        let layout = CellStorageLayout::new(
            Store::new(Arc::new(InMemory::new())).for_cellule().unwrap(),
            ObjectPath::from("fleet-rebalance"),
            *identity.application().as_bytes(),
        );
        let registry = Arc::new(crate::cells::compiled_registry().unwrap());
        bootstrap_release_at(
            &layout,
            identity,
            &registry,
            &format!("sha256:{}", "c".repeat(64)),
        )
        .await
        .unwrap();
        let source = SessionId::from_bytes([43; 16]);
        let receiver = SessionId::from_bytes([44; 16]);
        let source_runtime =
            CellRuntime::new(SqlWorkerPool::new(1, 4).unwrap(), 16 * 1024 * 1024, source).unwrap();
        let receiver_runtime = CellRuntime::new(
            SqlWorkerPool::new(1, 4).unwrap(),
            16 * 1024 * 1024,
            receiver,
        )
        .unwrap();
        let source_dir = tempfile::TempDir::new().unwrap();
        let receiver_dir = tempfile::TempDir::new().unwrap();
        let target = CellTarget::new(
            identity.tenant(),
            identity.application(),
            REPOSITORY_NAMESPACE,
            &[45; 16],
        )
        .unwrap();
        let (proof, authority) =
            crate::cells::provision_repository(&layout, identity, &registry, &target)
                .await
                .unwrap();
        let observed = authority
            .create_initial(&proof, IncarnationId::from_bytes([46; 16]), owner(source))
            .await
            .unwrap();
        let handle = source_runtime
            .bootstrap(
                proof,
                CellReplica::new(
                    layout.clone(),
                    *target.cell_id().as_bytes(),
                    *observed.value().incarnation.as_bytes(),
                    repository_replica_limits(),
                )
                .unwrap(),
                authority.clone(),
                observed,
                source_dir.path().join("rebalance.sqlite"),
                |transaction| initialize_repository_schema(transaction).map_err(Into::into),
            )
            .await
            .unwrap();
        let mutation = mutation(47);
        let digest = cellule_runtime::Digest::from_bytes([48; 32]);
        let issued_at_ms = mutation.issued_at_ms;
        handle
            .execute(mutation, digest, issued_at_ms, 64, 64, |transaction| {
                transaction.execute_batch("CREATE TABLE transfer_state(value INTEGER NOT NULL); INSERT INTO transfer_state(value) VALUES (7)")?;
                Ok(cellule_runtime::HandlerOutcome::Success(Vec::new()))
            })
            .await
            .unwrap();
        let destination = router(
            identity,
            layout.clone(),
            Arc::clone(&registry),
            receiver_runtime.clone(),
            receiver,
            receiver_dir.path().to_path_buf(),
        );
        let fleet = cellule_runtime::Digest::from_bytes([21; 32]);
        let image = cellule_runtime::Digest::from_bytes([22; 32]);
        let directory = NodeDirectory::new(layout.clone(), fleet, image, registry.release_digest());
        let now_ms = crate::cells::unix_now_ms().unwrap() + 65_000;
        for (session, free_memory_bytes, free_disk_bytes, active_cells) in [
            (source, 1024 * 1024, 7 * 1024 * 1024 * 1024, 1),
            (receiver, 16 * 1024 * 1024, 10 * 1024 * 1024 * 1024, 0),
        ] {
            let key = SigningKey::from_bytes(&[*session.as_bytes().first().unwrap(); 32]);
            let advertisement = NodeAdvertisement::sign(
                cellule_runtime::NodeId::from_bytes(*session.as_bytes()),
                session,
                owner(session).endpoint,
                fleet,
                cellule_runtime::Digest::from_bytes([49; 32]),
                image,
                registry.release_digest(),
                &key,
                1,
                now_ms,
                now_ms + 15_000,
                registry.module_digests(),
                vec![1],
                cellule_runtime::NodeFailureDomain::default(),
                NodeCapacity {
                    free_memory_bytes,
                    free_disk_bytes,
                    job_credits: 10,
                    ..NodeCapacity::default()
                },
            )
            .unwrap()
            .with_placement_capacity(
                cellule_runtime::NodePlacementCapacity {
                    memory_capacity_bytes: 16 * 1024 * 1024,
                    disk_capacity_bytes: 10 * 1024 * 1024 * 1024,
                    active_cells,
                    max_active_cells: 10,
                    running_jobs: 0,
                    job_capacity: 10,
                    publication_backlog: 0,
                    hydration_backlog: 0,
                    primitive_backlog: 0,
                }
                .validated()
                .unwrap(),
                &key,
            )
            .unwrap();
            directory.create(advertisement, now_ms).await.unwrap();
        }
        let source_key = SigningKey::from_bytes(&[43; 32]);
        let source_router = RepositoryCellRouter::new(
            identity,
            layout.clone(),
            Arc::clone(&registry),
            source_runtime.clone(),
            RepositoryCellPeer::new(
                directory,
                Arc::new(PeerSigner::new(
                    source,
                    registry.release_digest(),
                    source_key.clone(),
                )),
                Arc::new(ActivatingPeer {
                    destination: destination.clone(),
                    source,
                    release: registry.release_digest(),
                    verifying_key: source_key.verifying_key(),
                    now_ms,
                }),
                owner(source),
            ),
            source_dir.path().to_path_buf(),
        )
        .unwrap();
        let generation = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some((_, generation, _, _)) = source_runtime
                    .idle_transfer_candidates()
                    .await
                    .unwrap()
                    .first()
                {
                    break *generation;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        source_router.rebalance_evidence.lock().await.insert(
            target.cell_id(),
            RebalanceEvidence {
                generation: generation.saturating_sub(1),
                first_seen_ms: now_ms - 65_000,
                last_sample_ms: now_ms - 1,
                samples: 2,
            },
        );
        let cold = source_router.rebalance_once_at(now_ms).await.unwrap();
        assert_eq!(cold.released, 0);
        source_router.rebalance_evidence.lock().await.insert(
            target.cell_id(),
            RebalanceEvidence {
                generation,
                first_seen_ms: now_ms - 65_000,
                last_sample_ms: now_ms - 1,
                samples: 2,
            },
        );
        let progress = source_router.rebalance_once_at(now_ms).await.unwrap();
        assert_eq!((progress.released, progress.activated), (1, 1));
        assert_eq!(source_runtime.stats().active_cells(), 0);
        assert_eq!(receiver_runtime.stats().active_cells(), 1);
        let owned = authority.load(target.cell_id()).await.unwrap().unwrap();
        assert_eq!(owned.value().owner.as_ref(), Some(&owner(receiver)));
        let proof = destination
            .catalog
            .lookup(target.cell_id())
            .await
            .unwrap()
            .unwrap();
        let successor = receiver_runtime
            .local_handle(proof, &owned)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            successor
                .query(64, 64, |connection| {
                    let value =
                        connection.query_row("SELECT value FROM transfer_state", [], |row| {
                            row.get::<_, i64>(0)
                        })?;
                    Ok(value.to_be_bytes().to_vec())
                })
                .await
                .unwrap(),
            7_i64.to_be_bytes()
        );
        successor.drain().await.unwrap();
        receiver_runtime.shutdown().await.unwrap();
        source_runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn route_refuses_to_initialize_an_uncataloged_repository_cell() {
        let identity = ApplicationIdentity::new(
            TenantId::from_bytes([11; 16]),
            ApplicationId::from_bytes([12; 16]),
        );
        let layout = CellStorageLayout::new(
            Store::new(Arc::new(InMemory::new())).for_cellule().unwrap(),
            ObjectPath::from("repository-router-missing"),
            *identity.application().as_bytes(),
        );
        let registry = Arc::new(crate::cells::compiled_registry().unwrap());
        bootstrap_release_at(
            &layout,
            identity,
            &registry,
            &format!("sha256:{}", "b".repeat(64)),
        )
        .await
        .unwrap();
        let session = SessionId::from_bytes([13; 16]);
        let runtime = CellRuntime::new(
            SqlWorkerPool::new(1, 10).unwrap(),
            16 * 1024 * 1024,
            session,
        )
        .unwrap();
        let directory = tempfile::TempDir::new().unwrap();
        let router = router(
            identity,
            layout,
            registry,
            runtime.clone(),
            session,
            directory.path().to_path_buf(),
        );
        let principal = Identity {
            issuer: "https://crab.build".into(),
            subject: "user-1".into(),
            name: "Crab User".into(),
        };

        let result = router
            .route(Uuid::from_bytes([14; 16]), &principal, "repository.read")
            .await;

        assert!(matches!(
            result,
            Err(crate::Error::Cell(cellule_runtime::Error::CellNotActive))
        ));
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn activity_payload_reservation_is_bounded_and_reusable() {
        let identity = ApplicationIdentity::new(
            TenantId::from_bytes([31; 16]),
            ApplicationId::from_bytes([32; 16]),
        );
        let layout = CellStorageLayout::new(
            Store::new(Arc::new(InMemory::new())).for_cellule().unwrap(),
            ObjectPath::from("repository-router-activity-admission"),
            *identity.application().as_bytes(),
        );
        let registry = Arc::new(crate::cells::compiled_registry().unwrap());
        let session = SessionId::from_bytes([33; 16]);
        let runtime = CellRuntime::new(
            SqlWorkerPool::new(1, 1).unwrap(),
            2 * MAX_ACTIVITY_PAYLOAD_BYTES,
            session,
        )
        .unwrap();
        let directory = tempfile::TempDir::new().unwrap();
        let router = router(
            identity,
            layout,
            registry,
            runtime.clone(),
            session,
            directory.path().to_path_buf(),
        );
        let held = router.reserve_activity_payloads().unwrap();

        assert!(matches!(
            router.reserve_activity_payloads(),
            Err(crate::Error::Cell(cellule_runtime::Error::Capacity(
                "node retained bytes"
            )))
        ));
        drop(held);
        let released = router.reserve_activity_payloads().unwrap();
        drop(released);

        runtime.shutdown().await.unwrap();
    }

    fn router(
        identity: ApplicationIdentity,
        layout: CellStorageLayout,
        registry: Arc<Registry>,
        runtime: CellRuntime,
        session: SessionId,
        session_dir: PathBuf,
    ) -> RepositoryCellRouter {
        RepositoryCellRouter::new(
            identity,
            layout.clone(),
            Arc::clone(&registry),
            runtime,
            RepositoryCellPeer::new(
                NodeDirectory::new(
                    layout,
                    cellule_runtime::Digest::from_bytes([21; 32]),
                    cellule_runtime::Digest::from_bytes([22; 32]),
                    registry.release_digest(),
                ),
                Arc::new(PeerSigner::new(
                    session,
                    registry.release_digest(),
                    SigningKey::from_bytes(&[7; 32]),
                )),
                Arc::new(UnavailablePeer),
                owner(session),
            ),
            session_dir,
        )
        .unwrap()
    }

    fn owner(session: SessionId) -> Owner {
        Owner {
            session,
            endpoint: format!("https://{}.internal:8081", encode_hex(session.as_bytes())),
        }
    }

    fn mutation(byte: u8) -> MutationIdentity {
        let now_ms = i64::try_from(
            std::time::SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        )
        .unwrap();
        MutationIdentity {
            request_id: RequestId::from_bytes([byte; 16]),
            issued_at_ms: now_ms,
            expires_at_ms: now_ms + 60_000,
        }
    }
}
