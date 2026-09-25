//! Initial table data Cell admission through the existing Cell runtime.

use std::{
    path::PathBuf,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crab_cell_app::{ApplicationHandle, CompiledApplication};
use crab_cell_host::CellNodeTaskGroup;
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
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;

use crate::backend::{InitialPartitionProvisioner, cell_error, mutation_identity};
use crate::split::split_contract;
use crate::{
    BeginSplit, BeginSplitOutcome, Beyonddb, CellSplitController, DATA_MODULE, DescribeTable,
    InstallPartition, InstallPartitionOutcome, Json, ListTables, ListTablesInput,
    ListTablesOutcome, PartitionInstall, PartitionSpec, PartitionState, PartitionUsage,
    PublishedPartitionInput, PublishedPartitionOutcome, ReadPartitionState, ReadPublishedPartition,
    ReadRoutePage, ReadSplitPlan, ReadSplitRoute, RoutePageInput, RoutePageOutcome, SplitPlan,
    SplitRouteState, TableRecord, account_target, credential_target, data_target,
    initialize_account, initialize_credentials, initialize_partition,
};

/// Position in an account capacity sweep.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapacityCursor {
    /// Table being inspected; no range cursor resumes after this table.
    pub table_name: String,
    /// Lower bound of the last range inspected in this table.
    pub after_lower: Option<[u8; 16]>,
}

/// Bounded progress from one account capacity sweep.
#[derive(Clone, Debug, PartialEq)]
pub struct CapacitySweep {
    /// Resume at this position on the next sweep, or restart at the beginning.
    pub cursor: Option<CapacityCursor>,
    /// Source and children of one completed split, if any.
    pub split: Option<SplitPlan>,
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
        })
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
        self.takeover_expired(&target, proof, nodes).await
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
        self.takeover_expired(&target, proof, nodes).await
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
        self.takeover_expired(&target, proof, nodes).await
    }

    async fn takeover_expired(
        &self,
        target: &CellTarget,
        proof: CatalogProof,
        nodes: &NodeDirectory,
    ) -> Result<CellHandle, StorageError> {
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
        if former.session == self.session || observed.value().root.is_none() {
            return Err(StorageError::Transient(
                "Cell cannot be taken over from this owner state".into(),
            ));
        }
        let now_ms = i64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(|_| StorageError::Internal("system clock predates Unix epoch".into()))?
                .as_millis(),
        )
        .map_err(|_| StorageError::Internal("system clock exceeds lease range".into()))?;
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
        let destination = self.directory.join(format!(
            "{}.sqlite",
            blake3::Hash::from_bytes(*target.cell_id().as_bytes()).to_hex()
        ));
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
                Owner {
                    session: self.session,
                    endpoint: self.endpoint.clone(),
                },
            )
            .await
            .map_err(provision_error)
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

    /// Inspect one table range or resume one pending split.
    ///
    /// Pass the returned cursor back into the next call; `None` starts a new pass.
    pub async fn reconcile_account_capacity(
        &self,
        account_id: &str,
        account_handle: CellHandle,
        max_database_bytes: u64,
        cursor: Option<&CapacityCursor>,
    ) -> Result<CapacitySweep, StorageError> {
        if max_database_bytes == 0 {
            return Err(StorageError::Validation(
                "database byte split threshold must be positive".into(),
            ));
        }
        let account = account_target(account_id).map_err(provision_error)?;
        let client = CellClient::local(self.application.registry(), account_handle.clone());
        let (name, after_lower) = if let Some(cursor) = cursor
            && cursor.after_lower.is_some()
        {
            (cursor.table_name.clone(), cursor.after_lower)
        } else {
            let page = client
                .query::<ListTables>(
                    &account,
                    None,
                    Json(ListTablesInput {
                        limit: 1,
                        exclusive_start: cursor.map(|cursor| cursor.table_name.clone()),
                    }),
                )
                .await
                .map_err(cell_error)?
                .output
                .0;
            let ListTablesOutcome::Page(page) = page else {
                return Err(StorageError::Internal("invalid capacity table page".into()));
            };
            let Some(name) = page.names.into_iter().next() else {
                return Ok(CapacitySweep {
                    cursor: None,
                    split: None,
                });
            };
            (name, None)
        };
        let Some(table) = client
            .query::<DescribeTable>(&account, None, Json(name.clone()))
            .await
            .map_err(cell_error)?
            .output
            .0
        else {
            return Ok(CapacitySweep {
                cursor: Some(CapacityCursor {
                    table_name: name,
                    after_lower: None,
                }),
                split: None,
            });
        };
        let page = client
            .query::<ReadRoutePage>(
                &account,
                None,
                Json(RoutePageInput {
                    table_id: table.id.clone(),
                    start_hash: None,
                    after_lower,
                    expected_epoch: None,
                }),
            )
            .await
            .map_err(cell_error)?
            .output
            .0;
        let (partitions, has_more) = match page {
            RoutePageOutcome::Page {
                partitions,
                has_more,
                ..
            } => (partitions, has_more),
            RoutePageOutcome::Unrouted => {
                return Ok(CapacitySweep {
                    cursor: Some(CapacityCursor {
                        table_name: name,
                        after_lower: None,
                    }),
                    split: None,
                });
            }
            RoutePageOutcome::Changed => {
                return Err(StorageError::Transient(
                    "capacity route changed; retry".into(),
                ));
            }
        };
        let pending = client
            .query::<ReadSplitPlan>(&account, None, Json(table.id.clone()))
            .await
            .map_err(cell_error)?
            .output
            .0;
        let partition = partitions.first();
        if partition.is_none() && pending.is_none() {
            return Ok(CapacitySweep {
                cursor: Some(CapacityCursor {
                    table_name: name,
                    after_lower: None,
                }),
                split: None,
            });
        }
        let split = if let Some(plan) = pending {
            Some(
                self.split_partition(
                    account_id,
                    account_handle,
                    &table.id,
                    plan.source.partition_id,
                )
                .await?,
            )
        } else {
            let Some(partition) = partition else {
                return Err(StorageError::Internal("capacity range disappeared".into()));
            };
            self.split_if_over_database_bytes(
                account_id,
                account_handle,
                &table.id,
                partition.partition_id,
                max_database_bytes,
            )
            .await?
        };
        let next_after_lower = if partitions.len() > 1 || has_more {
            partition.map(|partition| partition.lower)
        } else {
            None
        };
        Ok(CapacitySweep {
            cursor: Some(CapacityCursor {
                table_name: name,
                after_lower: next_after_lower,
            }),
            split,
        })
    }

    /// Repeat bounded account sweeps until cancellation or an actionable error.
    pub async fn run_account_capacity_loop(
        &self,
        account_id: &str,
        account_handle: CellHandle,
        max_database_bytes: u64,
        interval: Duration,
        cancellation: &CancellationToken,
    ) -> Result<(), StorageError> {
        if interval.is_zero() || max_database_bytes == 0 {
            return Err(StorageError::Validation(
                "capacity interval and database byte threshold must be positive".into(),
            ));
        }
        let mut ticks = tokio::time::interval(interval);
        ticks.set_missed_tick_behavior(MissedTickBehavior::Skip);
        let mut cursor = None;
        loop {
            tokio::select! {
                () = cancellation.cancelled() => return Ok(()),
                _ = ticks.tick() => {}
            }
            let result = self
                .reconcile_account_capacity(
                    account_id,
                    account_handle.clone(),
                    max_database_bytes,
                    cursor.as_ref(),
                )
                .await?;
            cursor = result.cursor;
        }
    }

    /// Retain the account's split loop in the node's serving task group.
    ///
    /// Node shutdown cancels the loop before draining Cell owners.
    pub fn install_account_capacity_loop(
        self: &Arc<Self>,
        tasks: &CellNodeTaskGroup,
        account_id: String,
        account_handle: CellHandle,
        max_database_bytes: u64,
        interval: Duration,
    ) -> Result<(), StorageError> {
        if interval.is_zero() || max_database_bytes == 0 {
            return Err(StorageError::Validation(
                "capacity interval and database byte threshold must be positive".into(),
            ));
        }
        let provisioner = Arc::clone(self);
        let cancellation = tasks.cancellation_token();
        tasks
            .spawn(async move {
                provisioner
                    .run_account_capacity_loop(
                        &account_id,
                        account_handle,
                        max_database_bytes,
                        interval,
                        &cancellation,
                    )
                    .await
            })
            .map_err(provision_error)
    }

    /// Split one oversized range or resume the table's durable split plan.
    ///
    /// A serving control loop should repeat this call while the table is active.
    pub async fn reconcile_table_capacity(
        &self,
        account_id: &str,
        account_handle: CellHandle,
        table_id: &str,
        max_database_bytes: u64,
    ) -> Result<Option<SplitPlan>, StorageError> {
        if max_database_bytes == 0 {
            return Err(StorageError::Validation(
                "database byte split threshold must be positive".into(),
            ));
        }
        let account = account_target(account_id).map_err(provision_error)?;
        let client = CellClient::local(self.application.registry(), account_handle.clone());
        if let Some(plan) = client
            .query::<ReadSplitPlan>(&account, None, Json(table_id.to_owned()))
            .await
            .map_err(cell_error)?
            .output
            .0
        {
            return self
                .split_partition(
                    account_id,
                    account_handle,
                    table_id,
                    plan.source.partition_id,
                )
                .await
                .map(Some);
        }
        let mut after_lower = None;
        let mut expected_epoch = None;
        loop {
            let page = client
                .query::<ReadRoutePage>(
                    &account,
                    None,
                    Json(RoutePageInput {
                        table_id: table_id.to_owned(),
                        start_hash: None,
                        after_lower,
                        expected_epoch,
                    }),
                )
                .await
                .map_err(cell_error)?
                .output
                .0;
            let (epoch, partitions, has_more) = match page {
                RoutePageOutcome::Page {
                    epoch,
                    partitions,
                    has_more,
                } => (epoch, partitions, has_more),
                RoutePageOutcome::Unrouted => {
                    return Err(StorageError::TableNotActive(table_id.to_owned()));
                }
                RoutePageOutcome::Changed => {
                    return Err(StorageError::Transient(
                        "capacity route changed; retry".into(),
                    ));
                }
            };
            let last_lower = partitions.last().map(|partition| partition.lower);
            for partition in partitions {
                if let Some(plan) = self
                    .split_if_over_database_bytes(
                        account_id,
                        account_handle.clone(),
                        table_id,
                        partition.partition_id,
                        max_database_bytes,
                    )
                    .await?
                {
                    return Ok(Some(plan));
                }
            }
            if !has_more {
                return Ok(None);
            }
            after_lower = last_lower;
            expected_epoch = Some(epoch);
        }
    }

    /// Split a range once its SQLite database image exceeds `max_database_bytes`.
    ///
    /// The caller must repeat this check as part of its capacity loop. A pending
    /// split is resumed even if its source no longer crosses the threshold.
    pub async fn split_if_over_database_bytes(
        &self,
        account_id: &str,
        account_handle: CellHandle,
        table_id: &str,
        source_partition_id: [u8; 16],
        max_database_bytes: u64,
    ) -> Result<Option<SplitPlan>, StorageError> {
        if max_database_bytes == 0 {
            return Err(StorageError::Validation(
                "database byte split threshold must be positive".into(),
            ));
        }
        let account = account_target(account_id).map_err(provision_error)?;
        let client = CellClient::local(self.application.registry(), account_handle.clone());
        if client
            .query::<ReadSplitPlan>(&account, None, Json(table_id.to_owned()))
            .await
            .map_err(cell_error)?
            .output
            .0
            .is_some()
        {
            return self
                .split_partition(account_id, account_handle, table_id, source_partition_id)
                .await
                .map(Some);
        }
        let published = client
            .query::<ReadPublishedPartition>(
                &account,
                None,
                Json(PublishedPartitionInput {
                    table_id: table_id.to_owned(),
                    partition_id: source_partition_id,
                }),
            )
            .await
            .map_err(cell_error)?
            .output
            .0;
        if !matches!(published, PublishedPartitionOutcome::Published { .. }) {
            return self
                .split_partition(account_id, account_handle, table_id, source_partition_id)
                .await
                .map(Some);
        }
        let target =
            data_target(account_id, table_id, &source_partition_id).map_err(provision_error)?;
        let proof = CellCatalog::new(self.layout.clone(), target.tenant())
            .lookup(target.cell_id())
            .await
            .map_err(provision_error)?
            .ok_or_else(|| StorageError::Transient("split source is not cataloged".into()))?;
        let handle = self.admit(&target, proof).await?;
        let usage = CellClient::local(self.application.registry(), handle)
            .query::<PartitionUsage>(&target, None, Json(()))
            .await
            .map_err(cell_error)?
            .output
            .0;
        if usage.database_bytes <= max_database_bytes {
            return Ok(None);
        }
        self.split_partition(account_id, account_handle, table_id, source_partition_id)
            .await
            .map(Some)
    }

    /// Plan and finish one additional data Cell range split.
    ///
    /// The source range is unavailable between its seal and the route switch.
    /// A retry resumes a durable plan for the same source after interruption.
    pub async fn split_partition(
        &self,
        account_id: &str,
        account_handle: CellHandle,
        table_id: &str,
        source_partition_id: [u8; 16],
    ) -> Result<SplitPlan, StorageError> {
        let account = account_target(account_id).map_err(provision_error)?;
        let client = CellClient::local(self.application.registry(), account_handle.clone());
        if let Some(plan) = client
            .query::<ReadSplitPlan>(&account, None, Json(table_id.to_owned()))
            .await
            .map_err(cell_error)?
            .output
            .0
        {
            if plan.source.partition_id != source_partition_id {
                return Err(StorageError::Transient(
                    "another partition split is pending for this table".into(),
                ));
            }
            self.resume_split(account_id, account_handle, &plan).await?;
            return Ok(plan);
        }
        let published = client
            .query::<ReadPublishedPartition>(
                &account,
                None,
                Json(PublishedPartitionInput {
                    table_id: table_id.to_owned(),
                    partition_id: source_partition_id,
                }),
            )
            .await
            .map_err(cell_error)?
            .output
            .0;
        let (route_epoch, source) = match published {
            PublishedPartitionOutcome::Published { route_epoch, spec } => (route_epoch, spec),
            PublishedPartitionOutcome::Unrouted => {
                return Err(StorageError::TableNotActive(table_id.to_owned()));
            }
            PublishedPartitionOutcome::Missing => {
                return self
                    .completed_split(account_id, table_id, source_partition_id, &client, &account)
                    .await?
                    .ok_or_else(|| {
                        StorageError::Validation("split source is absent from table route".into())
                    });
            }
        };
        let plan = split_plan(&source, route_epoch)?;
        match client
            .command::<BeginSplit>(&account, mutation_identity()?, Json(plan.clone()))
            .await
        {
            Ok(committed) if committed.output.0 == BeginSplitOutcome::Planned => {}
            Ok(_) => {
                return Err(StorageError::Internal(
                    "unexpected successful split plan result".into(),
                ));
            }
            Err(InvocationError::Rejected(committed)) => {
                return Err(match committed.output.0 {
                    BeginSplitOutcome::TableNotFound => {
                        StorageError::TableNotFound(table_id.to_owned())
                    }
                    BeginSplitOutcome::RouteNotFound => {
                        StorageError::TableNotActive(table_id.to_owned())
                    }
                    BeginSplitOutcome::InvalidPlan | BeginSplitOutcome::Conflict => {
                        StorageError::Transient("split plan or route changed; retry".into())
                    }
                    BeginSplitOutcome::Planned => {
                        StorageError::Internal("unexpected rejected split plan result".into())
                    }
                });
            }
            Err(error) => return Err(cell_error(error)),
        }
        self.resume_split(account_id, account_handle, &plan).await?;
        Ok(plan)
    }

    async fn completed_split(
        &self,
        account_id: &str,
        table_id: &str,
        source_partition_id: [u8; 16],
        client: &CellClient,
        account: &crab_cell_runtime::identity::CellTarget,
    ) -> Result<Option<SplitPlan>, StorageError> {
        let target =
            data_target(account_id, table_id, &source_partition_id).map_err(provision_error)?;
        let catalog = CellCatalog::new(self.layout.clone(), target.tenant());
        let Some(proof) = catalog
            .lookup(target.cell_id())
            .await
            .map_err(provision_error)?
        else {
            return Ok(None);
        };
        let handle = self.admit(&target, proof).await?;
        let status = CellClient::local(self.application.registry(), handle)
            .query::<ReadPartitionState>(&target, None, Json(()))
            .await
            .map_err(cell_error)?
            .output
            .0;
        let Some(status) = status else {
            return Ok(None);
        };
        let PartitionState::Sealed(seal) = status.state else {
            return Ok(None);
        };
        let source = status.spec;
        if seal.table_id != table_id
            || seal.source_partition_id != source_partition_id
            || seal.epoch != source.epoch
            || seal.source_lower != source.lower
            || seal.source_upper != source.upper
        {
            return Ok(None);
        }
        let Some(expected_epoch) = seal.next_epoch.checked_sub(1) else {
            return Ok(None);
        };
        let mut left = source.clone();
        left.partition_id = seal.left_partition_id;
        left.upper = Some(seal.boundary);
        left.epoch = seal.next_epoch;
        let mut right = source.clone();
        right.partition_id = seal.right_partition_id;
        right.lower = Some(seal.boundary);
        right.epoch = seal.next_epoch;
        let plan = SplitPlan {
            source,
            children: [left, right],
            expected_epoch,
        };
        let state = client
            .query::<ReadSplitRoute>(account, None, Json(plan.clone()))
            .await
            .map_err(cell_error)?
            .output
            .0;
        Ok((state == SplitRouteState::After).then_some(plan))
    }

    /// Admit both split children and resume a durable table split.
    ///
    /// The account handle must belong to this leased node. An interrupted
    /// attempt can call this again after owner recovery with the same plan.
    pub async fn resume_split(
        &self,
        account_id: &str,
        account_handle: CellHandle,
        plan: &SplitPlan,
    ) -> Result<(), StorageError> {
        let (source, children, _) = split_contract(plan)?;
        let account = account_target(account_id).map_err(provision_error)?;
        let registry = self.application.registry();
        let account_client = CellClient::local(Arc::clone(&registry), account_handle.clone());
        let pending = account_client
            .query::<ReadSplitPlan>(&account, None, Json(source.table.id.clone()))
            .await
            .map_err(cell_error)?
            .output
            .0;
        let route_state = account_client
            .query::<ReadSplitRoute>(&account, None, Json(plan.clone()))
            .await
            .map_err(cell_error)?
            .output
            .0;
        if !((pending.as_ref() == Some(plan) && route_state == SplitRouteState::Before)
            || (pending.is_none() && route_state == SplitRouteState::After))
        {
            return Err(StorageError::Transient(
                "split plan or published route changed".into(),
            ));
        }
        let source_target = data_target(account_id, &source.table.id, &source.partition_id)
            .map_err(provision_error)?;
        let catalog = CellCatalog::new(self.layout.clone(), source_target.tenant());
        let source_proof = catalog
            .lookup(source_target.cell_id())
            .await
            .map_err(provision_error)?
            .ok_or_else(|| StorageError::Transient("split source is not cataloged".into()))?;
        let source_handle = self.admit(&source_target, source_proof).await?;
        let code = registry
            .module_code(DATA_MODULE)
            .ok_or_else(|| StorageError::Internal("data Cell module is not compiled".into()))?;
        let mut handles = vec![account_handle, source_handle];
        for spec in children {
            let target = data_target(account_id, &spec.table.id, &spec.partition_id)
                .map_err(provision_error)?;
            let proof = catalog
                .provision(
                    CatalogEntry::new(&target, CatalogRole::Sql, code, 1)
                        .map_err(provision_error)?,
                )
                .await
                .map_err(provision_error)?;
            handles.push(self.admit(&target, proof).await?);
        }
        let client = CellClient::local_many(registry, handles).map_err(provision_error)?;
        CellSplitController::new(client)
            .resume(account_id, plan)
            .await
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
            return Ok(handle);
        }
        let replica = CellReplica::new(
            self.layout.clone(),
            *target.cell_id().as_bytes(),
            *observed.value().incarnation.as_bytes(),
            Limits::default(),
        )
        .map_err(|error| StorageError::Transient(error.to_string()))?;
        let destination = self.directory.join(format!(
            "{}.sqlite",
            blake3::Hash::from_bytes(*target.cell_id().as_bytes()).to_hex()
        ));
        match observed.value().state {
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
            ControlState::Idle if observed.value().root.is_some() => self
                .runtime
                .acquire_idle_restored(proof, replica, authority, observed, destination, owner)
                .await
                .map_err(provision_error),
            _ => Err(StorageError::Transient(
                "Cell has another owner or is still activating".into(),
            )),
        }
    }
}

fn split_plan(source: &PartitionSpec, route_epoch: u64) -> Result<SplitPlan, StorageError> {
    let lower = u128::from_be_bytes(source.lower.unwrap_or([0; 16]));
    let upper = source.upper.map(u128::from_be_bytes);
    let midpoint = match upper {
        Some(upper) => upper
            .checked_sub(lower)
            .and_then(|width| lower.checked_add(width / 2)),
        None => lower.checked_add((u128::MAX - lower) / 2 + 1),
    }
    .filter(|middle| *middle > lower && upper.is_none_or(|upper| *middle < upper))
    .ok_or_else(|| StorageError::LimitExceeded("partition range cannot be split further".into()))?;
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
    let boundary = midpoint.to_be_bytes();
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
    fn provision<'a>(
        &'a self,
        account_id: &'a str,
        table: &'a TableRecord,
    ) -> BoxedFuture<'a, Result<Vec<PartitionSpec>, StorageError>> {
        Box::pin(async move {
            let registry = self.application.registry();
            let code = registry
                .module_code(DATA_MODULE)
                .ok_or_else(|| StorageError::Internal("data Cell module is not compiled".into()))?;
            let mut partitions = Vec::with_capacity(usize::from(self.initial_partition_count));
            for index in 0..self.initial_partition_count {
                let spec = initial_partition(table, self.initial_partition_count, index)?;
                let target = data_target(account_id, &table.id, &spec.partition_id)
                    .map_err(provision_error)?;
                let catalog = CellCatalog::new(self.layout.clone(), target.tenant());
                let proof = catalog
                    .provision(
                        CatalogEntry::new(&target, CatalogRole::Sql, code, 1)
                            .map_err(provision_error)?,
                    )
                    .await
                    .map_err(provision_error)?;
                let handle = self.admit(&target, proof).await?;
                let client = CellClient::local(Arc::clone(&registry), handle);
                let application = ApplicationHandle::<Beyonddb>::new(
                    client,
                    Arc::clone(&self.application),
                    target.tenant(),
                    target.application(),
                )
                .map_err(provision_error)?;
                match application
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
