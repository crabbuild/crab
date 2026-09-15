use std::{path::PathBuf, sync::Arc};

use crab_cell_runtime::{
    ApplicationIdentity, CatalogProof, CellAuthority, CellCatalog, CellClient, CellReplica,
    CellRuntime, CellTarget, ControlState, Owner, PeerPrincipal, PeerRoundTrip, PeerSigner,
    Registry, ReplicaLimits, VersionedControl,
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
    signer: Arc<PeerSigner>,
    round_trip: Arc<dyn PeerRoundTrip>,
    owner: Owner,
    session_dir: PathBuf,
    activation: Arc<[Mutex<()>]>,
}

pub(crate) struct RepositoryCell {
    pub(crate) target: CellTarget,
    pub(crate) client: CellClient,
}

impl RepositoryCellRouter {
    pub(crate) fn new(
        identity: ApplicationIdentity,
        layout: CellStorageLayout,
        registry: Arc<Registry>,
        runtime: CellRuntime,
        signer: Arc<PeerSigner>,
        round_trip: Arc<dyn PeerRoundTrip>,
        owner: Owner,
        session_dir: PathBuf,
    ) -> crate::Result<Self> {
        if !session_dir.is_absolute() || owner.endpoint.is_empty() {
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
            signer,
            round_trip,
            owner,
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
        if let Some(routed) = self.route_existing(&target, principal, action).await? {
            return Ok(routed);
        }

        let shard = activation_shard(&target);
        let _activation = self.activation[shard].lock().await;
        if let Some(routed) = self.route_existing(&target, principal, action).await? {
            return Ok(routed);
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
        self.activate_or_route(target, proof, observed, principal, action)
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
        principal: &Identity,
        action: &'static str,
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
        if owner.session != self.owner.session {
            return Ok(Some(self.peer(target.clone(), principal, action)));
        }
        if owner != &self.owner {
            return Err(crab_cell_runtime::Error::Fenced.into());
        }
        Ok(self
            .runtime
            .local_handle(proof, &control)
            .await?
            .map(|handle| RepositoryCell {
                target: target.clone(),
                client: CellClient::local(Arc::clone(&self.registry), handle),
            }))
    }

    async fn activate_or_route(
        &self,
        target: CellTarget,
        proof: CatalogProof,
        observed: VersionedControl,
        principal: &Identity,
        action: &'static str,
    ) -> crate::Result<RepositoryCell> {
        if observed.value().state == ControlState::Tombstoned {
            return Err(crab_cell_runtime::Error::CellNotActive.into());
        }
        if let Some(owner) = observed.value().owner.as_ref()
            && owner.session != self.owner.session
        {
            return Ok(self.peer(target, principal, action));
        }
        if observed
            .value()
            .owner
            .as_ref()
            .is_some_and(|owner| owner != &self.owner)
        {
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
                        self.owner.clone(),
                    )
                    .await?
            }
            ControlState::Recovering | ControlState::Serving => {
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
            ControlState::Tombstoned => {
                return Err(crab_cell_runtime::Error::CellNotActive.into());
            }
        };
        Ok(RepositoryCell {
            target,
            client: CellClient::local(Arc::clone(&self.registry), handle),
        })
    }

    fn peer(
        &self,
        target: CellTarget,
        principal: &Identity,
        action: &'static str,
    ) -> RepositoryCell {
        RepositoryCell {
            target,
            client: CellClient::peer(
                Arc::clone(&self.registry),
                Arc::clone(&self.signer),
                PeerPrincipal {
                    issuer: principal.issuer.clone(),
                    subject: principal.subject.clone(),
                    actions: vec![action.to_owned()],
                },
                Arc::clone(&self.round_trip),
            ),
        }
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
        TenantId,
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
    async fn route_reuses_and_restores_explicit_repository_cell() {
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
            layout,
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
            layout,
            Arc::clone(&registry),
            runtime,
            Arc::new(PeerSigner::new(
                session,
                registry.release_digest(),
                SigningKey::from_bytes(&[7; 32]),
            )),
            Arc::new(UnavailablePeer),
            owner(session),
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
