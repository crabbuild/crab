use std::{path::PathBuf, sync::Arc};

use crab_cell_runtime::{
    ApplicationIdentity, CatalogProof, CellAuthority, CellCatalog, CellClient, CellDescription,
    CellHandle, CellReplica, CellRuntime, CellTarget, ControlState, EffectPeerClient,
    MAX_ACTIVITY_PAYLOAD_BYTES, MigrationPeerClient, NodeByteReservation, NodeDirectory, Owner,
    PeerPrincipal, PeerRoundTrip, PeerSigner, PersistedWorkInventory, Registry, ReleaseState,
    ReleaseStore, ReplicaLimits, VersionedControl,
};
use crab_storage::CellStorageLayout;
use tokio::sync::Mutex;
use uuid::Uuid;

use super::REPOSITORY_NAMESPACE;
use crate::auth::Identity;

const ACTIVATION_SHARDS: usize = 4096;

#[derive(Clone)]
pub(crate) struct RepositoryCellRouter {
    identity: ApplicationIdentity,
    layout: CellStorageLayout,
    registry: Arc<Registry>,
    catalog: CellCatalog,
    authority: CellAuthority,
    runtime: CellRuntime,
    peer: RepositoryCellPeer,
    session_dir: PathBuf,
    activation: Arc<[Mutex<()>]>,
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
            session_dir,
            activation: (0..ACTIVATION_SHARDS)
                .map(|_| Mutex::new(()))
                .collect::<Vec<_>>()
                .into(),
        })
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

    pub(crate) async fn route_runtime(
        &self,
        target: CellTarget,
        principal: PeerPrincipal,
    ) -> crate::Result<ScheduledRepositoryCell> {
        if target.tenant() != self.identity.tenant()
            || target.application() != self.identity.application()
        {
            return Err(crab_cell_runtime::Error::PeerAuthorization(
                "runtime target belongs to another application",
            )
            .into());
        }
        self.route_target(target, principal).await
    }

    pub(crate) fn registry(&self) -> Arc<Registry> {
        Arc::clone(&self.registry)
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
            return Err(crab_cell_runtime::Error::PeerAuthorization(
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
                .ok_or(crab_cell_runtime::Error::CellNotActive)?;
            let control = self
                .authority
                .load(target.cell_id())
                .await?
                .ok_or(crab_cell_runtime::Error::CellNotActive)?;
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
                .ok_or(crab_cell_runtime::Error::Registry(
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
            match scheduled.cell.handle {
                Some(handle)
                    if handle.code() == plan.from_code()
                        && handle.schema() == plan.from_schema() =>
                {
                    handle.migrate(plan, super::unix_now_ms()?).await?;
                }
                Some(handle)
                    if handle.code() == plan.to_code() && handle.schema() >= plan.to_schema() => {}
                Some(_) => return Err(crab_cell_runtime::Error::Fenced.into()),
                None => {
                    self.migration_peer_client()
                        .migrate(target.clone(), expected, plan, super::unix_now_ms()?)
                        .await?;
                }
            }
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
            return Err(crab_cell_runtime::Error::PeerAuthorization(
                "persisted-work target belongs to another application",
            )
            .into());
        }
        let role = self
            .catalog
            .lookup(target.cell_id())
            .await?
            .ok_or(crab_cell_runtime::Error::CellNotActive)?
            .entry()
            .role();
        let scheduled = self
            .route_runtime(
                target.clone(),
                self.runtime_principal(&["cell.release.inspect"]),
            )
            .await?;
        let inventory: crate::Result<PersistedWorkInventory> = match scheduled.cell.handle.as_ref()
        {
            Some(handle) => handle
                .persisted_work_inventory(role)
                .await
                .map_err(Into::into),
            None => Err(crab_cell_runtime::Error::CellNotActive.into()),
        };
        let drained = if scheduled.should_release() {
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
            .ok_or(crab_cell_runtime::Error::Release(
                "release is unavailable during Cell migration",
            ))?;
        if !matches!(
            release.record().state(),
            ReleaseState::Activating | ReleaseState::Maintenance
        ) || release.record().desired() != Some(self.registry.release_digest())
        {
            return Err(crab_cell_runtime::Error::Release(
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

    async fn route_target(
        &self,
        target: CellTarget,
        principal: PeerPrincipal,
    ) -> crate::Result<ScheduledRepositoryCell> {
        if let Some(routed) = self.route_existing(&target, &principal).await? {
            return Ok(ScheduledRepositoryCell {
                cell: routed,
                release_after: false,
            });
        }

        let shard = activation_shard(&target);
        let _activation = self.activation[shard].lock().await;
        if let Some(routed) = self.route_existing(&target, &principal).await? {
            return Ok(ScheduledRepositoryCell {
                cell: routed,
                release_after: false,
            });
        }

        let proof = self
            .catalog
            .lookup(target.cell_id())
            .await?
            .ok_or(crab_cell_runtime::Error::CellNotActive)?;
        let observed = self
            .authority
            .load(target.cell_id())
            .await?
            .ok_or(crab_cell_runtime::Error::CellNotActive)?;
        if observed.value().root.is_none() {
            return Err(crab_cell_runtime::Error::CellNotActive.into());
        }
        self.activate_or_route(target, proof, observed, &principal)
            .await
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
        let Some(proof) = self.catalog.lookup(target.cell_id()).await? else {
            return Ok(None);
        };
        let Some(control) = self.authority.load(target.cell_id()).await? else {
            return Ok(None);
        };
        if control.value().root.is_none() {
            return Err(crab_cell_runtime::Error::CellNotActive.into());
        }
        if control.value().state == ControlState::Tombstoned {
            return Err(crab_cell_runtime::Error::CellNotActive.into());
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
            return Err(crab_cell_runtime::Error::Fenced.into());
        }
        Ok(self
            .runtime
            .local_handle(proof, &control)
            .await?
            .map(|handle| RepositoryCell {
                target: target.clone(),
                client: CellClient::local(Arc::clone(&self.registry), handle.clone()),
                handle: Some(handle),
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
            return Err(crab_cell_runtime::Error::CellNotActive.into());
        }
        let remote_owner = observed
            .value()
            .owner
            .as_ref()
            .filter(|owner| owner.session != self.peer.owner.session);
        if let Some(owner) = remote_owner
            && self.remote_owner_is_live(owner).await?
        {
            return Ok(ScheduledRepositoryCell {
                cell: self.peer(target, principal.clone()),
                release_after: false,
            });
        }
        let takeover = remote_owner.is_some();
        if observed.value().owner.as_ref().is_some_and(|owner| {
            owner.session == self.peer.owner.session && owner != &self.peer.owner
        }) {
            return Err(crab_cell_runtime::Error::Fenced.into());
        }

        let replica = CellReplica::new(
            self.layout.clone(),
            *target.cell_id().as_bytes(),
            *observed.value().incarnation.as_bytes(),
            ReplicaLimits::default(),
        )
        .map_err(crab_cell_runtime::Error::from)?;
        let destination = self.activation_path(&target).await?;
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
                if takeover {
                    self.runtime
                        .takeover_restored(
                            proof,
                            replica,
                            self.authority.clone(),
                            observed,
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
                            destination,
                        )
                        .await?
                }
            }
            ControlState::Tombstoned => {
                return Err(crab_cell_runtime::Error::CellNotActive.into());
            }
        };
        Ok(ScheduledRepositoryCell {
            cell: RepositoryCell {
                target,
                client: CellClient::local(Arc::clone(&self.registry), handle.clone()),
                handle: Some(handle),
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
        let proof = self
            .catalog
            .lookup(target.cell_id())
            .await?
            .ok_or(crab_cell_runtime::Error::CellNotActive)?;
        let control = self
            .authority
            .load(target.cell_id())
            .await?
            .ok_or(crab_cell_runtime::Error::CellNotActive)?;
        let handle = self
            .runtime
            .local_handle(proof, &control)
            .await?
            .ok_or(crab_cell_runtime::Error::CellNotActive)?;
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
    ) {
        return Err(crab_cell_runtime::Error::PeerAuthorization(
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
    use std::{future::Future, pin::Pin, time::UNIX_EPOCH};

    use crab_cell_runtime::{
        ApplicationId, IncarnationId, MutationIdentity, RequestId, SessionId, SqlWorkerPool,
        TenantId, Transition,
    };
    use crab_storage::Store;
    use ed25519_dalek::SigningKey;
    use object_store::{memory::InMemory, path::Path as ObjectPath};

    use super::*;
    use crate::cells::{
        REPOSITORY_MIGRATION, bootstrap_release_at,
        repository::{CreateIssue, CreateIssueInput, GetIssue, RepositoryAuthor},
    };

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
    async fn route_reuses_restores_idle_and_takes_over_stale_owner() {
        let identity = ApplicationIdentity::new(
            TenantId::from_bytes([1; 16]),
            ApplicationId::from_bytes([2; 16]),
        );
        let layout = CellStorageLayout::new(
            Store::new(Arc::new(InMemory::new())),
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
                    ReplicaLimits::default(),
                )
                .unwrap(),
                authority,
                observed,
                first_dir.path().join("bootstrap.sqlite"),
                move |transaction| {
                    transaction.execute_batch(REPOSITORY_MIGRATION)?;
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
        let reused = first
            .route(repository, &principal, "repository.read")
            .await
            .unwrap();
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
        let stale = idle.value().takeover(owner(stale_session)).unwrap();
        authority
            .transition(&idle, stale, Transition::Takeover)
            .await
            .unwrap();
        let third_session = SessionId::from_bytes([11; 16]);
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
    async fn route_refuses_to_initialize_an_uncataloged_repository_cell() {
        let identity = ApplicationIdentity::new(
            TenantId::from_bytes([11; 16]),
            ApplicationId::from_bytes([12; 16]),
        );
        let layout = CellStorageLayout::new(
            Store::new(Arc::new(InMemory::new())),
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
            Err(crate::Error::Cell(crab_cell_runtime::Error::CellNotActive))
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
            Store::new(Arc::new(InMemory::new())),
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
            Err(crate::Error::Cell(crab_cell_runtime::Error::Capacity(
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
                    crab_cell_runtime::Digest::from_bytes([21; 32]),
                    crab_cell_runtime::Digest::from_bytes([22; 32]),
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
