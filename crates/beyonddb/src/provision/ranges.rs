//! Range and directory placement with fenced recovery after interrupted admission.

use crab_cell_runtime::{
    Error, Result,
    cell::{
        actor::CellHandle,
        catalog::{CatalogEntry, CatalogRole, CellCatalog},
    },
    client::CellClient,
    control::authority::CellAuthority,
    identity::CellTarget,
    node::NodeDirectory,
};
use crab_ltx::rusqlite;
use extenddb_storage::error::StorageError;

use super::{CellInitialPartitionProvisioner, provision_error};

type Initializer = for<'a> fn(&rusqlite::Transaction<'a>) -> Result<()>;

fn range_module(target: &CellTarget) -> Result<(&'static str, Initializer)> {
    match target.namespace() {
        crate::DATA_NAMESPACE => Ok((crate::DATA_MODULE, crate::initialize_partition)),
        crate::global_index::NAMESPACE => {
            Ok((crate::global_index::MODULE, crate::initialize_global_index))
        }
        crate::directory::NAMESPACE => Ok((crate::directory::MODULE, crate::initialize_directory)),
        _ => Err(Error::PeerAuthorization(
            "bootstrap requires a data, index or directory Cell",
        )),
    }
}

impl CellInitialPartitionProvisioner {
    pub(super) async fn provision_range(
        &self,
        target: &CellTarget,
        client: &CellClient,
    ) -> std::result::Result<CellClient, StorageError> {
        let (module, initialize) = range_module(target).map_err(provision_error)?;
        let code = self
            .application
            .registry()
            .module_code(module)
            .ok_or_else(|| StorageError::Internal("range module is not compiled".into()))?;
        let proof = CellCatalog::new(self.layout.clone(), target.tenant())
            .provision(
                CatalogEntry::new(target, CatalogRole::Sql, code, 1).map_err(provision_error)?,
            )
            .await
            .map_err(provision_error)?;
        let observed = CellAuthority::new(self.layout.clone())
            .load(target.cell_id())
            .await
            .map_err(provision_error)?;
        let Some(peers) = &self.peers else {
            if observed.as_ref().is_some_and(|control| {
                control.value().root.is_some()
                    && control
                        .value()
                        .owner
                        .as_ref()
                        .is_some_and(|owner| owner.session != self.session)
            }) {
                return Ok(client.clone());
            }
            let handle = self.admit_initialized(target, proof, initialize).await?;
            return Ok(CellClient::local(self.application.registry(), handle));
        };
        let owner = observed
            .as_ref()
            .and_then(|control| control.value().owner.as_ref())
            .map(|owner| owner.session);
        let unpublished = observed
            .as_ref()
            .is_none_or(|control| control.value().root.is_none());
        let expired = if let Some(session) = owner {
            !peers
                .directory()
                .is_live(session, super::lease_time_ms()?)
                .await
                .map_err(provision_error)?
        } else {
            false
        };
        // Route publication can follow root publication. An expired initial
        // owner must be recoverable even before account routes discover it.
        if (unpublished || expired)
            && peers
                .provision_local(target, owner)
                .await
                .map_err(provision_error)?
        {
            self.admit_range(target, peers.directory())
                .await
                .map_err(provision_error)?;
        }
        Ok(client.clone())
    }

    pub(crate) async fn admit_range(
        &self,
        target: &CellTarget,
        nodes: &NodeDirectory,
    ) -> Result<CellHandle> {
        let (module, initialize) = range_module(target)?;
        // The authenticated receiver never catalogs a caller-supplied Cell.
        // Match the persisted identity, module digest, role, and schema first.
        let proof = self
            .cataloged(target, module)
            .await
            .map_err(admission_error)?;
        let observed = CellAuthority::new(self.layout.clone())
            .load(target.cell_id())
            .await?;
        if let Some(control) = observed {
            if let Some(handle) = self.runtime.local_handle(proof.clone(), &control).await? {
                return Ok(handle);
            }
            if let Some(owner) = control
                .value()
                .owner
                .as_ref()
                .filter(|owner| owner.session != self.session)
            {
                if nodes
                    .is_live(
                        owner.session,
                        super::lease_time_ms().map_err(admission_error)?,
                    )
                    .await?
                {
                    return Err(Error::CellNotActive);
                }
                // The runtime rechecks authority under the expired-session
                // proof. Published roots restore; only rootless claims bootstrap.
                return self
                    .takeover_expired(target, proof, nodes, initialize)
                    .await
                    .map_err(admission_error);
            }
            if control.value().root.is_some() {
                return self
                    .restore_idle(target, proof, control)
                    .await?
                    .ok_or(Error::CellNotActive);
            }
        }
        self.admit_initialized(target, proof, initialize)
            .await
            .map_err(admission_error)
    }
}

fn admission_error(source: StorageError) -> Error {
    Error::PeerTransport {
        context: "BeyondDB range admission",
        source: Box::new(source),
    }
}
