use std::{path::Path, sync::Arc};

use crab_cell_app::CompiledApplication;
use crab_cell_host::CellNodeBuilder;
use crab_cell_runtime::CellStorageLayout;
use crab_cell_runtime::{
    ApplicationIdentity, CatalogEntry, CatalogProof, CatalogRole, CellAuthority, CellCatalog,
    CellHandle, CellModule, CellReplica, CellTarget, ControlState, IncarnationId, Owner, Registry,
    ReleaseState, ReleaseStore, SessionId, SqlWorkerPool,
};
use uuid::Uuid;

use super::{
    REPOSITORY_MIGRATION, REPOSITORY_NAMESPACE, RepositoryModule, repository_replica_limits,
};
use crate::catalog::RepositoryApplicationState;
use crate::{Config, Error, Result};

const INITIALIZE_MAILBOX_BYTES: usize = 16 * 1024 * 1024;

pub(crate) async fn initialize_repository(config: &Config, repository: Uuid) -> Result<()> {
    let catalog = crate::catalog::CatalogStore::from_config(config)?;
    let (document, _) = catalog.load().await?;
    let record = document
        .repositories
        .iter()
        .find(|record| record.id == repository)
        .ok_or(crate::catalog::CatalogError::NotFound)?;
    let startup = super::verify_startup_release(config).await?;
    require_ready_release(&startup.layout, startup.identity, &startup.registry).await?;
    if record.application == RepositoryApplicationState::CellReady {
        return verify_repository_cells(
            &startup.layout,
            startup.identity,
            [(repository, record.application)],
        )
        .await;
    }
    initialize_repository_at(
        &startup.layout,
        startup.identity,
        &startup.registry,
        &startup.application,
        &config.cells.data_dir,
        config.cells.local_disk_limit_bytes,
        config.cells.peer_advertise.to_string(),
        repository,
    )
    .await?;
    catalog.mark_cell_ready(repository).await?;
    Ok(())
}

pub(crate) async fn initialize_repository_at(
    layout: &CellStorageLayout,
    identity: ApplicationIdentity,
    registry: &Registry,
    application: &Arc<CompiledApplication>,
    data_dir: &Path,
    local_disk_limit_bytes: u64,
    endpoint: String,
    repository: Uuid,
) -> Result<()> {
    let target = CellTarget::new(
        identity.tenant(),
        identity.application(),
        REPOSITORY_NAMESPACE,
        repository.as_bytes(),
    )?;
    let (proof, authority) = provision_repository(layout, identity, registry, &target).await?;
    let session = SessionId::from_bytes(Uuid::now_v7().into_bytes());
    let owner = Owner { session, endpoint };
    let observed = match authority.load(target.cell_id()).await? {
        Some(observed) => observed,
        None => {
            authority
                .create_initial(
                    &proof,
                    IncarnationId::from_bytes(Uuid::now_v7().into_bytes()),
                    owner.clone(),
                )
                .await?
        }
    };
    std::fs::create_dir_all(data_dir)?;
    let directory = tempfile::Builder::new()
        .prefix("crab-cell-repository-init-")
        .tempdir_in(data_dir)?;
    let budget = crate::server::CellRuntimeBudget::from_resources(crate::peer::local_resources(
        data_dir,
        local_disk_limit_bytes,
    )?)?;
    let local_disk = budget.local_disk();
    let cell_node = CellNodeBuilder::new(Arc::clone(application))
        .with_runtime(SqlWorkerPool::new(1, 1)?, INITIALIZE_MAILBOX_BYTES)
        .with_replica_host(budget.replica_host(local_disk, directory.path().to_owned()))
        .with_session(session)
        .build_unleased_for_maintenance()?;
    let runtime = cell_node.runtime();
    let replica = CellReplica::new(
        layout.clone(),
        *target.cell_id().as_bytes(),
        *observed.value().incarnation.as_bytes(),
        repository_replica_limits(),
    )
    .map_err(crab_cell_runtime::Error::from)?;
    let destination = directory.path().join(format!("{}.sqlite", Uuid::now_v7()));
    let result: Result<()> = async {
        let handle = match (observed.value().state, observed.value().root.is_some()) {
            (ControlState::Recovering, false) => {
                let repository_bytes = repository.into_bytes();
                let initialize = move |transaction: &rusqlite::Transaction<'_>| {
                    transaction.execute_batch(REPOSITORY_MIGRATION)?;
                    transaction.execute(
                        "INSERT INTO repository_identity(singleton, repository_uuid) VALUES (1, ?1)",
                        [repository_bytes.as_slice()],
                    )?;
                    Ok(())
                };
                if observed.value().owner.as_ref() == Some(&owner) {
                    runtime
                        .bootstrap(
                            proof,
                            replica,
                            authority.clone(),
                            observed,
                            destination,
                            initialize,
                        )
                        .await?
                } else {
                    return Err(Error::Config(
                        "offline repository initialization cannot fence an existing Cell owner",
                    ));
                }
            }
            (ControlState::Idle, true) => {
                runtime
                    .acquire_idle_restored(
                        proof,
                        replica,
                        authority.clone(),
                        observed,
                        destination,
                        owner.clone(),
                    )
                    .await?
            }
            (ControlState::Recovering | ControlState::Serving, true) => {
                return Err(Error::Config(
                    "offline repository initialization cannot take over an owned Cell",
                ));
            }
            (ControlState::Tombstoned, _) => {
                return Err(Error::Config("repository Cell is tombstoned"));
            }
            _ => {
                return Err(Error::Config(
                    "repository Cell control cannot be initialized or restored",
                ));
            }
        };
        verify_repository_identity(&handle, repository).await?;
        handle.drain().await?;
        let control = authority
            .load(target.cell_id())
            .await?
            .ok_or(Error::Config("initialized repository Cell lost its control"))?;
        if control.value().state != ControlState::Idle || control.value().owner.is_some() {
            return Err(Error::Config(
                "initialized repository Cell did not release to idle",
            ));
        }
        Ok(())
    }
    .await;
    let shutdown = cell_node.shutdown().await;
    match (result, shutdown) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), _) => Err(error),
        (Ok(()), Err(error)) => Err(error.into()),
    }
}

async fn verify_repository_identity(handle: &CellHandle, repository: Uuid) -> Result<()> {
    let observed = handle
        .query(32, 16, |connection| {
            connection
                .query_row(
                    "SELECT repository_uuid FROM repository_identity WHERE singleton = 1",
                    [],
                    |row| row.get::<_, Vec<u8>>(0),
                )
                .map_err(crab_cell_runtime::Error::from)
        })
        .await?;
    if observed.as_slice() != repository.as_bytes() {
        return Err(Error::Config(
            "repository Cell identity differs from the catalog repository",
        ));
    }
    Ok(())
}

async fn require_ready_release(
    layout: &CellStorageLayout,
    identity: ApplicationIdentity,
    registry: &Registry,
) -> Result<()> {
    let release = ReleaseStore::new(layout.clone(), identity)?
        .load()
        .await?
        .ok_or(Error::Config("Cell application release is not activated"))?;
    if release.record().state() != ReleaseState::Ready
        || release.record().current() != Some(registry.release_digest())
    {
        return Err(Error::Config(
            "repository creation requires this binary's ready Cell release",
        ));
    }
    Ok(())
}

pub(crate) async fn verify_repository_cells(
    layout: &CellStorageLayout,
    identity: ApplicationIdentity,
    repositories: impl IntoIterator<Item = (Uuid, RepositoryApplicationState)>,
) -> Result<()> {
    let catalog = CellCatalog::new(layout.clone(), identity.tenant());
    let authority = CellAuthority::new(layout.clone());
    for (repository, state) in repositories {
        if state != RepositoryApplicationState::CellReady {
            return Err(Error::Config(
                "cataloged repository application has not completed Cell initialization",
            ));
        }
        let target = CellTarget::new(
            identity.tenant(),
            identity.application(),
            REPOSITORY_NAMESPACE,
            repository.as_bytes(),
        )?;
        catalog
            .lookup(target.cell_id())
            .await?
            .ok_or(Error::Config(
                "cataloged repository has not been initialized into a Cell",
            ))?;
        let control = authority
            .load(target.cell_id())
            .await?
            .ok_or(Error::Config("cataloged repository Cell has no control"))?;
        if control.value().root.is_none() || control.value().state == ControlState::Tombstoned {
            return Err(Error::Config(
                "cataloged repository Cell has no published application root",
            ));
        }
    }
    Ok(())
}

pub(crate) async fn provision_repository(
    layout: &CellStorageLayout,
    identity: ApplicationIdentity,
    registry: &Registry,
    target: &CellTarget,
) -> Result<(CatalogProof, CellAuthority)> {
    let catalog = CellCatalog::new(layout.clone(), identity.tenant());
    let code =
        registry
            .module_code(RepositoryModule::NAME)
            .ok_or(crab_cell_runtime::Error::Registry(
                "repository module is not registered",
            ))?;
    let proof = ReleaseStore::new(layout.clone(), identity)?
        .provision(
            &catalog,
            registry,
            CatalogEntry::new(target, CatalogRole::Repository, code, 1)?,
        )
        .await?;
    Ok((proof, CellAuthority::new(layout.clone())))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crab_cell_runtime::{ApplicationId, CellAuthority, CellTarget, ControlState, TenantId};
    use crab_storage::Store;
    use object_store::{memory::InMemory, path::Path as ObjectPath};

    use super::*;

    #[tokio::test]
    async fn publishes_and_verifies_an_exact_retry() {
        let identity = ApplicationIdentity::new(
            TenantId::from_bytes([31; 16]),
            ApplicationId::from_bytes([32; 16]),
        );
        let layout = CellStorageLayout::new(
            Store::new(Arc::new(InMemory::new())),
            ObjectPath::from("repository-initializer"),
            *identity.application().as_bytes(),
        );
        let registry = super::super::compiled_registry().unwrap();
        super::super::bootstrap_release_at(
            &layout,
            identity,
            &registry,
            &format!("sha256:{}", "a".repeat(64)),
        )
        .await
        .unwrap();
        let repository = Uuid::from_bytes([33; 16]);
        let local = tempfile::TempDir::new().unwrap();
        let application = super::super::compiled_application().unwrap();

        for _ in 0..2 {
            initialize_repository_at(
                &layout,
                identity,
                &registry,
                &application,
                local.path(),
                32 * 1024 * 1024 * 1024,
                "https://initializer.internal:8081".into(),
                repository,
            )
            .await
            .unwrap();
        }

        verify_repository_cells(
            &layout,
            identity,
            [(repository, RepositoryApplicationState::CellReady)],
        )
        .await
        .unwrap();
        let target = CellTarget::new(
            identity.tenant(),
            identity.application(),
            REPOSITORY_NAMESPACE,
            repository.as_bytes(),
        )
        .unwrap();
        let control = CellAuthority::new(layout)
            .load(target.cell_id())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(control.value().state, ControlState::Idle);
        assert!(control.value().root.is_some());
        assert!(control.value().owner.is_none());
    }
}
