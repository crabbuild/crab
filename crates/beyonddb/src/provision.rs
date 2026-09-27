//! Initial table data Cell admission through the existing Cell runtime.

mod capacity;
mod global_indexes;
mod ranges;
mod residency;
mod transactions;

use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crab_cell_app::CompiledApplication;
use crab_cell_runtime::Error as CellError;
use crab_cell_runtime::cell::actor::{CellHandle, CellRuntime};
use crab_cell_runtime::cell::catalog::{CatalogEntry, CatalogProof, CatalogRole, CellCatalog};
use crab_cell_runtime::client::{CellClient, InvocationError};
use crab_cell_runtime::control::{ControlState, Owner, authority::CellAuthority};
use crab_cell_runtime::identity::{CellTarget, IncarnationId, SessionId};
use crab_cell_runtime::ltx::{CellStorageLayout, Limits};
use crab_cell_runtime::node::NodeDirectory;
use crab_cell_runtime::recovery::manifest::RecoveryManifestStore;
use crab_ltx::{CellReplica, rusqlite};
use extenddb_storage::BoxedFuture;
use extenddb_storage::error::StorageError;

use crate::backend::{InitialPartitionProvisioner, cell_error, mutation_identity};
use crate::{
    DATA_MODULE, DescribeTable, InstallPartition, InstallPartitionOutcome, Json, ListTables,
    ListTablesInput, ListTablesOutcome, PartitionInstall, PartitionSpec, ReadRoutePage,
    RegisterCoordinatorShard, RegisterCoordinatorShardInput, RoutePageInput, RoutePageOutcome,
    SplitPlan, TableRecord, account_target, coordinator_target, credential_target, data_target,
    initialize_account, initialize_coordinator, initialize_credentials, initialize_partition,
};

/// Position in an account capacity sweep.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapacityCursor {
    /// Table being inspected; absent range and index cursors resume after it.
    pub table_name: String,
    /// Lower bound of the last range inspected in this table.
    pub after_lower: Option<[u8; 16]>,
    /// Index position being inspected, or the base table when absent.
    pub index: Option<usize>,
}

/// Admits initial data Cells per table on a leased or private Cell runtime.
pub struct CellInitialPartitionProvisioner {
    runtime: CellRuntime,
    application: Arc<CompiledApplication>,
    layout: CellStorageLayout,
    session: SessionId,
    endpoint: String,
    directory: PathBuf,
    initial_partition_count: u16,
    transaction_recovery: transactions::CoordinatorRecovery,
    admission: tokio::sync::Mutex<()>,
    peers: Option<Arc<crate::BeyonddbPeers>>,
}

impl CellInitialPartitionProvisioner {
    /// Bind the application's runtime, object store, owner identity and local disk.
    ///
    /// Serving callers must have installed the node lease before provisioning.
    /// The directory must be private to this node and writable.
    pub fn new(
        runtime: CellRuntime,
        application: Arc<CompiledApplication>,
        layout: CellStorageLayout,
        session: SessionId,
        endpoint: String,
        directory: PathBuf,
    ) -> Result<Self, StorageError> {
        std::fs::create_dir_all(&directory)
            .map_err(|error| StorageError::Connection(error.to_string()))?;
        Ok(Self {
            runtime,
            application,
            layout,
            session,
            endpoint,
            directory,
            initial_partition_count: 1,
            transaction_recovery: Default::default(),
            admission: Default::default(),
            peers: None,
        })
    }

    /// Place new data/index ranges using this node's shared peer context.
    ///
    /// The context must use the same runtime, layout, and boot session.
    pub fn with_peers(mut self, peers: Arc<crate::BeyonddbPeers>) -> Self {
        self.peers = Some(peers);
        self
    }

    /// Start each new table with a power-of-two number of independently owned ranges.
    ///
    /// Accepted counts are 1 through 256. The count must remain stable while
    /// retrying an interrupted CreateTable, because range IDs are deterministic.
    pub fn with_initial_partition_count(mut self, count: u16) -> Result<Self, StorageError> {
        if count == 0 || count > 256 || !count.is_power_of_two() {
            return Err(StorageError::Validation(
                "initial partition count must be a power of two between 1 and 256".into(),
            ));
        }
        self.initial_partition_count = count;
        Ok(self)
    }

    /// Provision or reacquire an account Cell on this node.
    ///
    /// An existing Cell is acquired only after its authority record is idle
    /// with a published root; an active owner is never bootstrapped again.
    pub async fn admit_account(&self, account_id: &str) -> Result<CellHandle, StorageError> {
        let target = account_target(account_id).map_err(provision_error)?;
        self.admit_module(&target, crate::MODULE, initialize_account)
            .await
    }

    /// Provision or reacquire the shard that authenticates an access key.
    pub async fn admit_credential(&self, access_key_id: &str) -> Result<CellHandle, StorageError> {
        let target = credential_target(access_key_id).map_err(provision_error)?;
        self.admit_module(&target, crate::credentials::MODULE, initialize_credentials)
            .await
    }

    /// Provision and register the account-scoped shard for one transaction key.
    ///
    /// The account must be locally owned or idle. Registration publishes before
    /// the handle is returned, so later transaction records are discoverable.
    pub async fn admit_coordinator(
        &self,
        account_id: &str,
        routing_key: &[u8],
    ) -> Result<CellHandle, StorageError> {
        let target = coordinator_target(account_id, routing_key).map_err(provision_error)?;
        let account = account_target(account_id).map_err(provision_error)?;
        let account_handle = self.admit_account(account_id).await?;
        let handle = self
            .admit_module(
                &target,
                crate::transaction_coordinator::MODULE,
                initialize_coordinator,
            )
            .await?;
        let shard_bytes: [u8; 4] = target
            .partition()
            .try_into()
            .map_err(|_| StorageError::Internal("invalid coordinator partition".into()))?;
        let client = CellClient::local(self.application.registry(), account_handle);
        client
            .command::<RegisterCoordinatorShard>(
                &account,
                mutation_identity()?,
                Json(RegisterCoordinatorShardInput {
                    account_id: account_id.to_owned(),
                    shard: u32::from_be_bytes(shard_bytes),
                }),
            )
            .await
            .map_err(cell_error)?;
        Ok(handle)
    }

    /// Admit a configured account or recover its published root after a crash.
    pub async fn recover_owned_account(
        &self,
        account_id: &str,
        nodes: &NodeDirectory,
    ) -> Result<CellHandle, StorageError> {
        let target = account_target(account_id).map_err(provision_error)?;
        self.recover_owned(&target, crate::MODULE, nodes, initialize_account)
            .await
    }

    /// Admit a configured credential shard or recover it after a crash.
    pub async fn recover_owned_credential(
        &self,
        access_key_id: &str,
        nodes: &NodeDirectory,
    ) -> Result<CellHandle, StorageError> {
        let target = credential_target(access_key_id).map_err(provision_error)?;
        self.recover_owned(
            &target,
            crate::credentials::MODULE,
            nodes,
            initialize_credentials,
        )
        .await
    }

    /// Recover a published coordinator shard after its former owner expires.
    pub async fn recover_owned_coordinator(
        &self,
        account_id: &str,
        routing_key: &[u8],
        nodes: &NodeDirectory,
    ) -> Result<CellHandle, StorageError> {
        let target = coordinator_target(account_id, routing_key).map_err(provision_error)?;
        self.recover_owned(
            &target,
            crate::transaction_coordinator::MODULE,
            nodes,
            initialize_coordinator,
        )
        .await
    }

    async fn recover_owned(
        &self,
        target: &CellTarget,
        module: &'static str,
        nodes: &NodeDirectory,
        initialize: for<'a> fn(&rusqlite::Transaction<'a>) -> crab_cell_runtime::Result<()>,
    ) -> Result<CellHandle, StorageError> {
        let observed = CellAuthority::new(self.layout.clone())
            .load(target.cell_id())
            .await
            .map_err(provision_error)?;
        if let Some(former) = observed
            .as_ref()
            .and_then(|control| control.value().owner.as_ref())
            .filter(|owner| owner.session != self.session)
        {
            wait_for_expired(nodes, former.session).await?;
            let proof = self.cataloged(target, module).await?;
            return self
                .takeover_expired(target, proof, nodes, initialize)
                .await;
        }
        self.admit_module(target, module, initialize).await
    }

    async fn recover_discovered_owner(
        &self,
        target: &CellTarget,
        module: &'static str,
        initialize: for<'a> fn(
            &crab_ltx::rusqlite::Transaction<'a>,
        ) -> crab_cell_runtime::Result<()>,
        nodes: &NodeDirectory,
    ) -> Result<bool, StorageError> {
        let observed = CellAuthority::new(self.layout.clone())
            .load(target.cell_id())
            .await
            .map_err(provision_error)?
            .ok_or_else(|| StorageError::Transient("discovered Cell has no authority".into()))?;
        let former = observed
            .value()
            .owner
            .as_ref()
            .map(|owner| (owner.session, owner.endpoint.clone()));
        match former {
            Some((session, _)) if session == self.session => {
                // Ownership can outlive a canceled activation. Discovery must
                // restore the actor before its caller starts resolving work.
                let proof = self.cataloged(target, module).await?;
                if self
                    .runtime
                    .local_handle(proof.clone(), &observed)
                    .await
                    .map_err(provision_error)?
                    .is_none()
                {
                    self.admit_initialized(target, proof, initialize).await?;
                }
            }
            Some((session, endpoint)) => {
                if endpoint == self.endpoint {
                    wait_for_expired(nodes, session).await?;
                } else if nodes
                    .is_live(session, lease_time_ms()?)
                    .await
                    .map_err(provision_error)?
                {
                    return Ok(false);
                }
                // Discovery can select a new endpoint after the old lease expires.
                // Takeover rechecks the exact session and Cell authority so a
                // renewal or competing successor cannot be overwritten.
                let proof = self.cataloged(target, module).await?;
                self.takeover_expired(target, proof, nodes, initialize)
                    .await?;
            }
            None if observed.value().root.is_some() => {
                let proof = self.cataloged(target, module).await?;
                self.admit_initialized(target, proof, initialize).await?;
            }
            _ => return Ok(false),
        }
        self.track_coordinator(target)?;
        Ok(true)
    }

    pub(crate) async fn recover_projection_owner(
        &self,
        target: &CellTarget,
        nodes: &NodeDirectory,
    ) -> Result<(), StorageError> {
        type Initialize = for<'a> fn(&rusqlite::Transaction<'a>) -> crab_cell_runtime::Result<()>;
        let (module, initialize): (&'static str, Initialize) = match target.namespace() {
            crate::NAMESPACE => (crate::MODULE, initialize_account),
            crate::DATA_NAMESPACE => (DATA_MODULE, initialize_partition),
            crate::global_index::NAMESPACE => {
                (crate::global_index::MODULE, crate::initialize_global_index)
            }
            _ => {
                return Err(StorageError::Validation(
                    "invalid projection owner namespace".into(),
                ));
            }
        };
        self.recover_discovered_owner(target, module, initialize, nodes)
            .await?;
        Ok(())
    }

    /// Recover idle or expired routed ranges for a configured account.
    ///
    /// The caller selects this node as the account's recovery owner. Live remote
    /// owners remain in place; local capacity and fenced takeover still gate admission.
    pub async fn recover_registered_partitions(
        &self,
        account_id: &str,
        account_handle: CellHandle,
        nodes: &NodeDirectory,
    ) -> Result<(), StorageError> {
        let account = account_target(account_id).map_err(provision_error)?;
        let client = CellClient::local(self.application.registry(), account_handle);
        let mut after_table = None;
        loop {
            let page = client
                .query::<ListTables>(
                    &account,
                    None,
                    Json(ListTablesInput {
                        limit: 100,
                        exclusive_start: after_table,
                    }),
                )
                .await
                .map_err(cell_error)?
                .output
                .0;
            let ListTablesOutcome::Page(page) = page else {
                return Err(StorageError::Internal("invalid recovery table page".into()));
            };
            for name in page.names {
                let Some(table) = client
                    .query::<DescribeTable>(&account, None, Json(name))
                    .await
                    .map_err(cell_error)?
                    .output
                    .0
                else {
                    continue;
                };
                self.recover_routed_table(account_id, &account, &client, &table.id, nodes, None)
                    .await?;
                for index in &table.global_secondary_indexes {
                    self.recover_routed_table(
                        account_id,
                        &account,
                        &client,
                        &table.id,
                        nodes,
                        Some(index),
                    )
                    .await?;
                }
            }
            let Some(next) = page.last_evaluated else {
                return Ok(());
            };
            after_table = Some(next);
        }
    }

    async fn recover_routed_table(
        &self,
        account_id: &str,
        account: &CellTarget,
        client: &CellClient,
        table_id: &str,
        nodes: &NodeDirectory,
        index: Option<&crate::GlobalIndexRecord>,
    ) -> Result<(), StorageError> {
        let table_id = index.map_or(table_id, |index| index.id.as_str());
        let mut after_lower = None;
        let mut expected_epoch = None;
        loop {
            let input = Json(RoutePageInput {
                table_id: table_id.to_owned(),
                start_hash: None,
                after_lower,
                expected_epoch,
            });
            let page = if index.is_some() {
                client
                    .query::<crate::ReadGlobalIndexRoutePage>(account, None, input)
                    .await
            } else {
                client.query::<ReadRoutePage>(account, None, input).await
            }
            .map_err(cell_error)?
            .output
            .0;
            let (epoch, partitions, has_more) = match page {
                RoutePageOutcome::Unrouted => return Ok(()),
                RoutePageOutcome::Changed => {
                    return Err(StorageError::Transient(
                        "table route changed during recovery".into(),
                    ));
                }
                RoutePageOutcome::Page {
                    epoch,
                    partitions,
                    has_more,
                } => (epoch, partitions, has_more),
            };
            after_lower = partitions.last().map(|partition| partition.lower);
            for partition in partitions {
                if index.is_some() {
                    let target =
                        crate::global_index_target(account_id, table_id, &partition.partition_id)
                            .map_err(provision_error)?;
                    self.recover_discovered_owner(
                        &target,
                        crate::global_index::MODULE,
                        crate::initialize_global_index,
                        nodes,
                    )
                    .await?;
                } else {
                    let target = data_target(account_id, table_id, &partition.partition_id)
                        .map_err(provision_error)?;
                    self.recover_discovered_owner(
                        &target,
                        DATA_MODULE,
                        initialize_partition,
                        nodes,
                    )
                    .await?;
                }
            }
            if !has_more {
                return Ok(());
            }
            expected_epoch = Some(epoch);
        }
    }

    /// Recover an account Cell after its previous node lease expires.
    ///
    /// The caller must select this node as the replacement owner. An active
    /// node log must first be recovered by the fleet coordinator.
    pub async fn takeover_expired_account(
        &self,
        account_id: &str,
        nodes: &NodeDirectory,
    ) -> Result<CellHandle, StorageError> {
        let target = account_target(account_id).map_err(provision_error)?;
        let proof = self.cataloged(&target, crate::MODULE).await?;
        self.takeover_expired(&target, proof, nodes, initialize_account)
            .await
    }

    /// Recover an access-key shard after its previous node lease expires.
    ///
    /// The caller must select this node as the replacement owner. An active
    /// node log must first be recovered by the fleet coordinator.
    pub async fn takeover_expired_credential(
        &self,
        access_key_id: &str,
        nodes: &NodeDirectory,
    ) -> Result<CellHandle, StorageError> {
        let target = credential_target(access_key_id).map_err(provision_error)?;
        let proof = self.cataloged(&target, crate::credentials::MODULE).await?;
        self.takeover_expired(&target, proof, nodes, initialize_credentials)
            .await
    }

    /// Reacquire one cataloged data range after its prior owner released it.
    ///
    /// The caller must first select this node as the new owner from the
    /// published table route. This never creates a new data Cell or changes
    /// its installed range contract.
    pub async fn admit_existing_partition(
        &self,
        account_id: &str,
        table_id: &str,
        partition_id: &[u8; 16],
    ) -> Result<CellHandle, StorageError> {
        let target = data_target(account_id, table_id, partition_id).map_err(provision_error)?;
        let proof = self.cataloged(&target, DATA_MODULE).await?;
        self.admit(&target, proof).await
    }

    /// Reacquire a cataloged range after its serving node lease has expired.
    ///
    /// The caller must select this node as the replacement owner. The node
    /// directory fences the exact expired boot session before Cell authority
    /// can move. An active node log requires fleet recovery first.
    pub async fn takeover_expired_partition(
        &self,
        account_id: &str,
        table_id: &str,
        partition_id: &[u8; 16],
        nodes: &NodeDirectory,
    ) -> Result<CellHandle, StorageError> {
        let target = data_target(account_id, table_id, partition_id).map_err(provision_error)?;
        let proof = self.cataloged(&target, DATA_MODULE).await?;
        self.takeover_expired(&target, proof, nodes, initialize_partition)
            .await
    }

    async fn takeover_expired(
        &self,
        target: &CellTarget,
        proof: CatalogProof,
        nodes: &NodeDirectory,
        initialize: for<'a> fn(&rusqlite::Transaction<'a>) -> crab_cell_runtime::Result<()>,
    ) -> Result<CellHandle, StorageError> {
        let _admission = self.admission.lock().await;
        self.reclaim_coordinator_capacity().await?;
        let authority = CellAuthority::new(self.layout.clone());
        let observed = authority
            .load(target.cell_id())
            .await
            .map_err(provision_error)?
            .ok_or_else(|| StorageError::Transient("Cell has no authority".into()))?;
        let former = observed
            .value()
            .owner
            .as_ref()
            .ok_or_else(|| StorageError::Transient("Cell has no serving owner".into()))?;
        if former.session == self.session {
            return Err(StorageError::Transient(
                "Cell cannot be taken over from this owner state".into(),
            ));
        }
        let now_ms = lease_time_ms()?;
        let takeover = match nodes
            .takeover_proof(former.session, self.session, now_ms)
            .await
            .map_err(provision_error)?
        {
            Some(proof) => proof,
            None => nodes
                .claim_expired_for_takeover(former.session, self.session, now_ms)
                .await
                .map_err(provision_error)?,
        };
        let replica = CellReplica::new(
            self.layout.clone(),
            *target.cell_id().as_bytes(),
            *observed.value().incarnation.as_bytes(),
            Limits::default(),
        )
        .map_err(|error| StorageError::Transient(error.to_string()))?;
        let destination = self.activation_destination(target)?;
        let owner = Owner {
            session: self.session,
            endpoint: self.endpoint.clone(),
        };
        let handle = if observed.value().root.is_none() {
            self.runtime
                .takeover_unpublished(
                    proof,
                    replica,
                    authority,
                    observed,
                    takeover,
                    destination,
                    owner,
                    initialize,
                )
                .await
        } else {
            self.runtime
                .takeover_restored(
                    proof,
                    replica,
                    authority,
                    observed,
                    takeover,
                    RecoveryManifestStore::new(self.layout.clone(), Limits::default())
                        .with_recovery_scratch(self.directory.clone()),
                    destination,
                    owner,
                )
                .await
        }
        .map_err(provision_error)?;
        self.track_coordinator(target)?;
        Ok(handle)
    }

    fn activation_destination(&self, target: &CellTarget) -> Result<PathBuf, StorageError> {
        // Resume lookup consumes records in the destination directory. Isolate
        // Cells and give each activation a fresh path: another Cell's release
        // must not invalidate this Cell's local resume image.
        let directory = self.directory.join(
            blake3::Hash::from_bytes(*target.cell_id().as_bytes())
                .to_hex()
                .as_str(),
        );
        std::fs::create_dir_all(&directory)
            .map_err(|error| StorageError::Connection(error.to_string()))?;
        Ok(directory.join(format!("{}.sqlite", uuid::Uuid::now_v7())))
    }

    async fn cataloged(
        &self,
        target: &CellTarget,
        module: &'static str,
    ) -> Result<CatalogProof, StorageError> {
        let proof = CellCatalog::new(self.layout.clone(), target.tenant())
            .lookup(target.cell_id())
            .await
            .map_err(provision_error)?
            .ok_or_else(|| StorageError::Transient("Cell is not cataloged".into()))?;
        let code = self
            .application
            .registry()
            .module_code(module)
            .ok_or_else(|| StorageError::Internal("Cell module is not compiled".into()))?;
        if proof.entry().namespace() != target.namespace()
            || proof.entry().partition() != target.partition()
            || proof.entry().role() != CatalogRole::Sql
            || proof.entry().initial_code() != code
            || proof.entry().initial_schema() != 1
        {
            return Err(StorageError::Internal("Cell catalog collision".into()));
        }
        Ok(proof)
    }

    async fn admit_module(
        &self,
        target: &CellTarget,
        module: &'static str,
        initialize: for<'a> fn(&rusqlite::Transaction<'a>) -> crab_cell_runtime::Result<()>,
    ) -> Result<CellHandle, StorageError> {
        let code = self
            .application
            .registry()
            .module_code(module)
            .ok_or_else(|| StorageError::Internal("Cell module is not compiled".into()))?;
        let proof = CellCatalog::new(self.layout.clone(), target.tenant())
            .provision(
                CatalogEntry::new(target, CatalogRole::Sql, code, 1).map_err(provision_error)?,
            )
            .await
            .map_err(provision_error)?;
        self.admit_initialized(target, proof, initialize).await
    }

    async fn admit(
        &self,
        target: &CellTarget,
        proof: CatalogProof,
    ) -> Result<CellHandle, StorageError> {
        self.admit_initialized(target, proof, initialize_partition)
            .await
    }

    async fn admit_initialized(
        &self,
        target: &CellTarget,
        proof: CatalogProof,
        initialize: for<'a> fn(&rusqlite::Transaction<'a>) -> crab_cell_runtime::Result<()>,
    ) -> Result<CellHandle, StorageError> {
        // Serialize local activation/reclamation. Authority CAS still decides
        // ownership against other nodes; this guard never fences peers.
        let _admission = self.admission.lock().await;
        let authority = CellAuthority::new(self.layout.clone());
        let owner = Owner {
            session: self.session,
            endpoint: self.endpoint.clone(),
        };
        let observed = match authority
            .load(target.cell_id())
            .await
            .map_err(provision_error)?
        {
            Some(observed) => observed,
            None => {
                let incarnation = IncarnationId::from_bytes(*uuid::Uuid::now_v7().as_bytes());
                match authority
                    .create_initial(&proof, incarnation, owner.clone())
                    .await
                {
                    Ok(observed) => observed,
                    Err(CellError::CellAlreadyActive) => authority
                        .load(target.cell_id())
                        .await
                        .map_err(provision_error)?
                        .ok_or_else(|| {
                            StorageError::Transient("concurrent Cell admission is pending".into())
                        })?,
                    Err(error) => return Err(provision_error(error)),
                }
            }
        };
        if let Some(handle) = self
            .runtime
            .local_handle(proof.clone(), &observed)
            .await
            .map_err(provision_error)?
        {
            self.track_coordinator(target)?;
            return Ok(handle);
        }
        self.reclaim_coordinator_capacity().await?;
        let control = observed.value();
        if control.root.is_some()
            && match control.state {
                ControlState::Idle => control.owner.is_none(),
                ControlState::Recovering => control
                    .owner
                    .as_ref()
                    .is_some_and(|owner| owner.session == self.session),
                _ => false,
            }
        {
            return self
                .activate_published(target, proof, observed)
                .await
                .map_err(provision_error);
        }
        let replica = CellReplica::new(
            self.layout.clone(),
            *target.cell_id().as_bytes(),
            *observed.value().incarnation.as_bytes(),
            Limits::default(),
        )
        .map_err(|error| StorageError::Transient(error.to_string()))?;
        let destination = self.activation_destination(target)?;
        let handle = match observed.value().state {
            ControlState::Recovering
                if observed.value().root.is_none()
                    && observed.value().owner.as_ref().map(|owner| owner.session)
                        == Some(self.session) =>
            {
                self.runtime
                    .bootstrap(proof, replica, authority, observed, destination, initialize)
                    .await
                    .map_err(provision_error)
            }
            _ => Err(StorageError::Transient(
                "Cell has another owner or is still activating".into(),
            )),
        }?;
        self.track_coordinator(target)?;
        Ok(handle)
    }
}

fn lease_time_ms() -> Result<i64, StorageError> {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| StorageError::Internal("system clock predates Unix epoch".into()))?
            .as_millis(),
    )
    .map_err(|_| StorageError::Internal("system clock exceeds lease range".into()))
}

async fn wait_for_expired(nodes: &NodeDirectory, former: SessionId) -> Result<(), StorageError> {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let now_ms = lease_time_ms()?;
            if !nodes
                .is_live(former, now_ms)
                .await
                .map_err(provision_error)?
            {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    })
    .await
    .map_err(|_| StorageError::Transient("previous Cell owner remains live".into()))?
}

fn split_boundary(
    lower: Option<[u8; 16]>,
    upper: Option<[u8; 16]>,
) -> Result<[u8; 16], StorageError> {
    let lower = u128::from_be_bytes(lower.unwrap_or([0; 16]));
    let upper = upper.map(u128::from_be_bytes);
    let midpoint = match upper {
        Some(upper) => upper
            .checked_sub(lower)
            .and_then(|width| lower.checked_add(width / 2)),
        None => lower.checked_add((u128::MAX - lower) / 2 + 1),
    }
    .filter(|middle| *middle > lower && upper.is_none_or(|upper| *middle < upper))
    .ok_or_else(|| StorageError::LimitExceeded("partition range cannot be split further".into()))?;
    Ok(midpoint.to_be_bytes())
}

fn split_plan(source: &PartitionSpec, route_epoch: u64) -> Result<SplitPlan, StorageError> {
    let boundary = split_boundary(source.lower, source.upper)?;
    let next_epoch = route_epoch
        .checked_add(1)
        .ok_or_else(|| StorageError::LimitExceeded("table route epoch exhausted".into()))?;
    let fresh_id = |other: Option<[u8; 16]>| loop {
        let candidate = *uuid::Uuid::now_v7().as_bytes();
        if other != Some(candidate) && source.partition_id != candidate {
            break candidate;
        }
    };
    let left_id = fresh_id(None);
    let right_id = fresh_id(Some(left_id));
    let mut left = source.clone();
    left.partition_id = left_id;
    left.upper = Some(boundary);
    left.epoch = next_epoch;
    let mut right = source.clone();
    right.partition_id = right_id;
    right.lower = Some(boundary);
    right.epoch = next_epoch;
    Ok(SplitPlan {
        source: source.clone(),
        children: [left, right],
        expected_epoch: route_epoch,
    })
}

impl InitialPartitionProvisioner for CellInitialPartitionProvisioner {
    fn provision_global_index<'a>(
        &'a self,
        client: &'a CellClient,
        account_id: &'a str,
        table: &'a TableRecord,
        index: &'a crate::GlobalIndexRecord,
    ) -> BoxedFuture<'a, Result<Vec<crate::GlobalIndexPartitionSpec>, StorageError>> {
        Box::pin(async move {
            let account = account_target(account_id).map_err(provision_error)?;
            let mut partitions = Vec::with_capacity(usize::from(self.initial_partition_count));
            for ordinal in 0..self.initial_partition_count {
                self.reclaim_retired_ranges(client, &account, None).await?;
                let range = initial_partition(table, self.initial_partition_count, ordinal)?;
                let spec = crate::GlobalIndexPartitionSpec {
                    table: table.clone(),
                    index: index.clone(),
                    partition_id: range.partition_id,
                    lower: range.lower,
                    upper: range.upper,
                    epoch: range.epoch,
                };
                let target = crate::global_index_target(account_id, &index.id, &spec.partition_id)
                    .map_err(provision_error)?;
                let client = self.provision_range(&target, client).await?;
                match client
                    .command::<crate::InstallGlobalIndexPartition>(
                        &target,
                        mutation_identity()?,
                        Json(spec.clone()),
                    )
                    .await
                {
                    Ok(result) if result.output.0 => partitions.push(spec),
                    Ok(_) | Err(InvocationError::Rejected(_)) => {
                        return Err(StorageError::Internal(
                            "global-index range installation rejected".into(),
                        ));
                    }
                    Err(error) => return Err(cell_error(error)),
                }
            }
            Ok(partitions)
        })
    }

    fn provision<'a>(
        &'a self,
        client: &'a CellClient,
        account_id: &'a str,
        table: &'a TableRecord,
    ) -> BoxedFuture<'a, Result<Vec<PartitionSpec>, StorageError>> {
        Box::pin(async move {
            let account = account_target(account_id).map_err(provision_error)?;
            let mut partitions = Vec::with_capacity(usize::from(self.initial_partition_count));
            for index in 0..self.initial_partition_count {
                self.reclaim_retired_ranges(client, &account, None).await?;
                let spec = initial_partition(table, self.initial_partition_count, index)?;
                let target = data_target(account_id, &table.id, &spec.partition_id)
                    .map_err(provision_error)?;
                let client = self.provision_range(&target, client).await?;
                match client
                    .command::<InstallPartition>(
                        &target,
                        mutation_identity()?,
                        Json(PartitionInstall::Serving(spec.clone())),
                    )
                    .await
                {
                    Ok(committed) if committed.output.0 == InstallPartitionOutcome::Installed => {
                        partitions.push(spec);
                    }
                    Ok(_) => {
                        return Err(StorageError::Internal(
                            "unexpected successful partition install".into(),
                        ));
                    }
                    Err(InvocationError::Rejected(committed)) => {
                        return Err(StorageError::Internal(format!(
                            "partition install rejected: {:?}",
                            committed.output.0
                        )));
                    }
                    Err(error) => return Err(cell_error(error)),
                }
            }
            Ok(partitions)
        })
    }
}

fn initial_partition(
    table: &TableRecord,
    count: u16,
    index: u16,
) -> Result<PartitionSpec, StorageError> {
    let mut partition_id = [0_u8; 16];
    partition_id[15] = u8::try_from(index)
        .map_err(|_| StorageError::Internal("invalid initial partition index".into()))?;
    let boundary = |ordinal: u16| -> Result<[u8; 16], StorageError> {
        let mut hash = [0_u8; 16];
        hash[0] = u8::try_from(ordinal * (256 / count))
            .map_err(|_| StorageError::Internal("invalid initial range boundary".into()))?;
        Ok(hash)
    };
    Ok(PartitionSpec {
        table: table.clone(),
        partition_id,
        lower: (index > 0).then(|| boundary(index)).transpose()?,
        upper: (index + 1 < count)
            .then(|| boundary(index + 1))
            .transpose()?,
        epoch: 1,
    })
}

fn provision_error(error: CellError) -> StorageError {
    match error {
        CellError::CatalogFull | CellError::Capacity(_) => {
            StorageError::LimitExceeded(error.to_string())
        }
        CellError::CatalogCollision
        | CellError::Identity(_)
        | CellError::Registry(_)
        | CellError::Release(_) => StorageError::Internal(error.to_string()),
        _ => StorageError::Transient(error.to_string()),
    }
}
