//! ExtendDB table operations routed to account Cells.

mod admission;
mod data;
mod recovery;
mod remaining;
mod transaction;
mod transaction_read;
mod transaction_transport;

use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use crab_cell_runtime::client::{CellClient, InvocationError};
use crab_cell_runtime::identity::{CellTarget, RequestId, TenantId};
use crab_cell_runtime::{MutationIdentity, partition_for_shard};
use extenddb_core::limits::LimitsConfig;
use extenddb_core::types::{
    BillingMode, BillingModeSummary, CreateTableInput, DeleteTableInput, DescribeTableInput,
    IndexInfo, ListTablesInput as DdbListTablesInput, ListTablesOutput,
    ProvisionedThroughputDescription, TableDescription, TableKeyInfo, TableStatus,
    UpdateTableInput,
};
use extenddb_storage::error::StorageError;
use extenddb_storage::{BoxedFuture, TableEngine};

use super::{
    APPLICATION, ActivateTableRoute, ActivateTableRouteOutcome, CreateTable, CreateTableOutcome,
    DeleteTable, DeleteTableOutcome, DescribeTable, DescribeTableById, Json, ListTables,
    ListTablesInput, ListTablesOutcome, NAMESPACE, PartitionSpec, ReadRoutePage, RoutePageInput,
    RoutePageOutcome, TableRecord, TableRoute, TableSpec, TableUpdate, UpdateTable,
    UpdateTableOutcome, account_target,
};

/// Installs an initial table's data Cells before its route becomes visible.
pub trait InitialPartitionProvisioner: Send + Sync {
    /// Return the installed, published partition contracts for a new table.
    fn provision<'a>(
        &'a self,
        account_id: &'a str,
        table: &'a TableRecord,
    ) -> BoxedFuture<'a, Result<Vec<PartitionSpec>, StorageError>>;
}

/// Admits a discoverable coordinator before any transaction record is written.
pub trait CoordinatorProvisioner: Send + Sync {
    /// Ensure the shard is published and registered through the account owner.
    fn ensure<'a>(
        &'a self,
        client: &'a CellClient,
        account_id: &'a str,
        routing_key: &'a [u8],
    ) -> BoxedFuture<'a, Result<(), StorageError>>;
}

/// ExtendDB table backend over already-provisioned and routable account Cells.
pub struct CellStorage {
    client: CellClient,
    region: String,
    initial_partitions: Option<Arc<dyn InitialPartitionProvisioner>>,
    coordinators: Option<Arc<dyn CoordinatorProvisioner>>,
}

impl CellStorage {
    pub(crate) fn client(&self) -> &CellClient {
        &self.client
    }

    /// Binds ExtendDB operations to a Cell client and AWS region.
    ///
    /// The server must provision and activate each account Cell before routing
    /// its requests through this backend.
    pub fn new(client: CellClient, region: impl Into<String>) -> Self {
        Self {
            client,
            region: region.into(),
            initial_partitions: None,
            coordinators: None,
        }
    }

    /// Enables transaction writes through durably registered coordinator shards.
    #[must_use]
    pub fn with_transaction_coordinators(
        mut self,
        provisioner: Arc<dyn CoordinatorProvisioner>,
    ) -> Self {
        self.coordinators = Some(provisioner);
        self
    }

    /// Requires initial data Cell provisioning before a table is active.
    #[must_use]
    pub fn with_initial_partitions(
        mut self,
        provisioner: Arc<dyn InitialPartitionProvisioner>,
    ) -> Self {
        self.initial_partitions = Some(provisioner);
        self
    }
}

impl TableEngine for CellStorage {
    fn create_table(
        &self,
        account_id: &str,
        mut input: CreateTableInput,
    ) -> BoxedFuture<'_, Result<TableDescription, StorageError>> {
        let account_id = account_id.to_owned();
        Box::pin(async move {
            input.resolve_table_throughput_mode();
            if input
                .global_secondary_indexes
                .as_ref()
                .is_some_and(|v| !v.is_empty())
                || input
                    .local_secondary_indexes
                    .as_ref()
                    .is_some_and(|v| !v.is_empty())
                || input.vector_indexes.as_ref().is_some_and(|v| !v.is_empty())
                || input.stream_specification.is_some()
                || input.sse_specification.is_some()
                || input.table_class.is_some()
                || input.on_demand_throughput.is_some()
            {
                return Err(unsupported(
                    "table indexes, streams, SSE, class, or on-demand ceilings",
                ));
            }
            extenddb_core::validation::validate_create_table(&input, &LimitsConfig::default())
                .map_err(|error| StorageError::Validation(error.to_string()))?;
            let target = target(&account_id)?;
            let name = input.table_name.clone();
            let spec = TableSpec {
                table_name: input.table_name,
                key_schema: input.key_schema,
                attribute_definitions: input.attribute_definitions,
                billing_mode: input.billing_mode.unwrap_or(BillingMode::Provisioned),
                provisioned_throughput: input.provisioned_throughput,
                deletion_protection_enabled: input.deletion_protection_enabled.unwrap_or(false),
                initial_tags: input.tags.unwrap_or_default(),
                resource_arn: Some(extenddb_storage::util::table_arn(
                    &self.region,
                    &account_id,
                    &name,
                )),
            };
            let submitted = spec.clone();
            let record = match self
                .client
                .command::<CreateTable>(&target, mutation_identity()?, Json(spec))
                .await
            {
                Ok(committed) => match committed.output.0 {
                    CreateTableOutcome::Created(record) => record,
                    _ => {
                        return Err(StorageError::Internal(
                            "unexpected successful create result".into(),
                        ));
                    }
                },
                Err(InvocationError::Rejected(committed)) => match committed.output.0 {
                    CreateTableOutcome::AlreadyExists => {
                        if self.initial_partitions.is_none() {
                            return Err(StorageError::TableAlreadyExists(name));
                        }
                        let existing = self.record(&account_id, &name).await?;
                        if !submitted.matches_record(&existing)
                            || self.route_active_for(&account_id, &existing.id).await?
                        {
                            return Err(StorageError::TableAlreadyExists(name));
                        }
                        existing
                    }
                    CreateTableOutcome::InvalidSchema => {
                        return Err(StorageError::Validation("invalid table schema".into()));
                    }
                    _ => {
                        return Err(StorageError::Internal(
                            "unexpected rejected create result".into(),
                        ));
                    }
                },
                Err(error) => return Err(cell_error(error)),
            };
            if let Some(provisioner) = &self.initial_partitions {
                let partitions = provisioner.provision(&account_id, &record).await?;
                let route = TableRoute {
                    table_id: record.id.clone(),
                    epoch: 1,
                    partitions,
                };
                match self
                    .client
                    .command::<ActivateTableRoute>(&target, mutation_identity()?, Json(route))
                    .await
                {
                    Ok(committed) if committed.output.0 == ActivateTableRouteOutcome::Activated => {
                    }
                    Ok(_) => {
                        return Err(StorageError::Internal(
                            "unexpected successful route activation".into(),
                        ));
                    }
                    Err(InvocationError::Rejected(committed)) => {
                        return Err(match committed.output.0 {
                            ActivateTableRouteOutcome::TableNotFound => {
                                StorageError::TableNotFound(record.table_name.clone())
                            }
                            ActivateTableRouteOutcome::AlreadyActive => StorageError::Transient(
                                "table route changed during creation".into(),
                            ),
                            ActivateTableRouteOutcome::TransactionConflict => {
                                StorageError::Transient("table has prepared transactions".into())
                            }
                            ActivateTableRouteOutcome::TableNotEmpty => {
                                StorageError::TableNotActive(record.table_name.clone())
                            }
                            ActivateTableRouteOutcome::InvalidRoute => StorageError::Internal(
                                "provisioner returned an invalid table route".into(),
                            ),
                            ActivateTableRouteOutcome::Activated => StorageError::Internal(
                                "unexpected rejected route activation".into(),
                            ),
                        });
                    }
                    Err(error) => return Err(cell_error(error)),
                }
            }
            Ok(description(
                record,
                &account_id,
                &self.region,
                TableStatus::Active,
            ))
        })
    }

    fn delete_table(
        &self,
        account_id: &str,
        input: DeleteTableInput,
    ) -> BoxedFuture<'_, Result<TableDescription, StorageError>> {
        let account_id = account_id.to_owned();
        Box::pin(async move {
            let target = target(&account_id)?;
            let name = input.table_name;
            let record = match self
                .client
                .command::<DeleteTable>(&target, mutation_identity()?, Json(name.clone()))
                .await
            {
                Ok(committed) => match committed.output.0 {
                    DeleteTableOutcome::Deleted(record) => record,
                    _ => {
                        return Err(StorageError::Internal(
                            "unexpected successful delete result".into(),
                        ));
                    }
                },
                Err(InvocationError::Rejected(committed)) => match committed.output.0 {
                    DeleteTableOutcome::TableNotFound => {
                        return Err(StorageError::TableNotFound(name));
                    }
                    DeleteTableOutcome::TransactionConflict => {
                        return Err(StorageError::Transient(
                            "table has prepared transactions".into(),
                        ));
                    }
                    DeleteTableOutcome::DeletionProtected => {
                        return Err(StorageError::DeletionProtected(name));
                    }
                    _ => {
                        return Err(StorageError::Internal(
                            "unexpected rejected delete result".into(),
                        ));
                    }
                },
                Err(error) => return Err(cell_error(error)),
            };
            Ok(description(
                record,
                &account_id,
                &self.region,
                TableStatus::Deleting,
            ))
        })
    }

    fn describe_table(
        &self,
        account_id: &str,
        input: DescribeTableInput,
    ) -> BoxedFuture<'_, Result<TableDescription, StorageError>> {
        let account_id = account_id.to_owned();
        Box::pin(async move {
            let record = self.record(&account_id, &input.table_name).await?;
            let status = if self.initial_partitions.is_some()
                && !self.route_active_for(&account_id, &record.id).await?
            {
                TableStatus::Creating
            } else {
                TableStatus::Active
            };
            Ok(description(record, &account_id, &self.region, status))
        })
    }

    fn list_tables(
        &self,
        account_id: &str,
        input: DdbListTablesInput,
    ) -> BoxedFuture<'_, Result<ListTablesOutput, StorageError>> {
        let account_id = account_id.to_owned();
        Box::pin(async move {
            let target = target(&account_id)?;
            let output = self
                .client
                .query::<ListTables>(
                    &target,
                    None,
                    Json(ListTablesInput {
                        limit: i64::from(input.limit.unwrap_or(100)),
                        exclusive_start: input.exclusive_start_table_name,
                    }),
                )
                .await
                .map_err(cell_error)?;
            match output.output.0 {
                ListTablesOutcome::Page(page) => Ok(ListTablesOutput {
                    table_names: page.names,
                    last_evaluated_table_name: page.last_evaluated,
                }),
                ListTablesOutcome::InvalidLimit => Err(StorageError::Validation(
                    "table listing limit must be 1..=100".into(),
                )),
            }
        })
    }

    fn update_table(
        &self,
        account_id: &str,
        mut input: UpdateTableInput,
    ) -> BoxedFuture<'_, Result<TableDescription, StorageError>> {
        let account_id = account_id.to_owned();
        Box::pin(async move {
            input.resolve_table_throughput_mode();
            if input
                .global_secondary_index_updates
                .as_ref()
                .is_some_and(|v| !v.is_empty())
                || input.attribute_definitions.is_some()
                || input.stream_specification.is_some()
                || input.table_class.is_some()
                || input.on_demand_throughput.is_some()
                || input
                    .vector_index_updates
                    .as_ref()
                    .is_some_and(|v| !v.is_empty())
            {
                return Err(unsupported(
                    "index, stream, class, or on-demand ceiling updates",
                ));
            }
            let target = target(&account_id)?;
            let name = input.table_name.clone();
            let update = TableUpdate {
                table_name: input.table_name,
                billing_mode: input.billing_mode,
                provisioned_throughput: input.provisioned_throughput,
                deletion_protection_enabled: input.deletion_protection_enabled,
            };
            let record = match self
                .client
                .command::<UpdateTable>(&target, mutation_identity()?, Json(update))
                .await
            {
                Ok(committed) => match committed.output.0 {
                    UpdateTableOutcome::Updated(record) => record,
                    _ => {
                        return Err(StorageError::Internal(
                            "unexpected successful update result".into(),
                        ));
                    }
                },
                Err(InvocationError::Rejected(committed)) => match committed.output.0 {
                    UpdateTableOutcome::TableNotFound => {
                        return Err(StorageError::TableNotFound(name));
                    }
                    UpdateTableOutcome::InvalidUpdate => {
                        return Err(StorageError::Validation(
                            "invalid table billing update".into(),
                        ));
                    }
                    _ => {
                        return Err(StorageError::Internal(
                            "unexpected rejected update result".into(),
                        ));
                    }
                },
                Err(error) => return Err(cell_error(error)),
            };
            Ok(description(
                record,
                &account_id,
                &self.region,
                TableStatus::Active,
            ))
        })
    }

    fn table_key_info(
        &self,
        account_id: &str,
        table_name: &str,
    ) -> BoxedFuture<'_, Result<TableKeyInfo, StorageError>> {
        let account_id = account_id.to_owned();
        let table_name = table_name.to_owned();
        Box::pin(async move {
            let record = self.record(&account_id, &table_name).await?;
            if self.initial_partitions.is_some()
                && !self.route_active_for(&account_id, &record.id).await?
            {
                return Err(StorageError::TableNotActive(table_name));
            }
            Ok(TableKeyInfo {
                table_name: record.table_name,
                account_id,
                table_id: record.id,
                base_key_schema: record.key_schema.clone(),
                key_schema: record.key_schema,
                attribute_definitions: record.attribute_definitions,
                ..TableKeyInfo::default()
            })
        })
    }

    fn index_info(
        &self,
        account_id: &str,
        table_name: &str,
        index_name: &str,
    ) -> BoxedFuture<'_, Result<IndexInfo, StorageError>> {
        let account_id = account_id.to_owned();
        let table_name = table_name.to_owned();
        let index_name = index_name.to_owned();
        Box::pin(async move {
            self.record(&account_id, &table_name).await?;
            Err(StorageError::IndexNotFound(index_name))
        })
    }

    fn index_info_by_table_id(
        &self,
        table_id: &str,
        index_name: &str,
    ) -> BoxedFuture<'_, Result<IndexInfo, StorageError>> {
        let table_id = table_id.to_owned();
        let index_name = index_name.to_owned();
        Box::pin(async move {
            let target = target_from_table_id(&table_id)?;
            let found = self
                .client
                .query::<DescribeTableById>(&target, None, Json(table_id.clone()))
                .await
                .map_err(cell_error)?;
            if found.output.0.is_none() {
                return Err(StorageError::TableNotFound(table_id));
            }
            Err(StorageError::IndexNotFound(index_name))
        })
    }
}

impl CellStorage {
    async fn route_active_for(
        &self,
        account_id: &str,
        table_id: &str,
    ) -> Result<bool, StorageError> {
        let account = target(account_id)?;
        let result = self
            .client
            .query::<ReadRoutePage>(
                &account,
                None,
                Json(RoutePageInput {
                    table_id: table_id.to_owned(),
                    start_hash: None,
                    after_lower: None,
                    expected_epoch: None,
                }),
            )
            .await
            .map_err(cell_error)?
            .output
            .0;
        match result {
            RoutePageOutcome::Unrouted => Ok(false),
            RoutePageOutcome::Page { .. } => Ok(true),
            RoutePageOutcome::Changed => {
                Err(StorageError::Transient("table route changed; retry".into()))
            }
        }
    }

    async fn record(&self, account_id: &str, name: &str) -> Result<TableRecord, StorageError> {
        let target = target(account_id)?;
        self.client
            .query::<DescribeTable>(&target, None, Json(name.to_owned()))
            .await
            .map_err(cell_error)?
            .output
            .0
            .ok_or_else(|| StorageError::TableNotFound(name.to_owned()))
    }
}

fn description(
    record: TableRecord,
    account_id: &str,
    region: &str,
    status: TableStatus,
) -> TableDescription {
    let (read_capacity_units, write_capacity_units) = record
        .provisioned_throughput
        .as_ref()
        .map_or((0, 0), |capacity| {
            (capacity.read_capacity_units, capacity.write_capacity_units)
        });
    let billing_mode_summary = record
        .pay_per_request_since_ms
        .map(|since| BillingModeSummary {
            billing_mode: BillingMode::PayPerRequest,
            last_update_to_pay_per_request_date_time: Some(since as f64 / 1_000.0),
        });
    TableDescription {
        table_name: record.table_name.clone(),
        key_schema: record.key_schema,
        attribute_definitions: record.attribute_definitions,
        table_status: status,
        creation_date_time: record.created_at_ms as f64 / 1_000.0,
        table_arn: extenddb_storage::util::table_arn(region, account_id, &record.table_name),
        table_id: record.id,
        provisioned_throughput: ProvisionedThroughputDescription {
            read_capacity_units,
            write_capacity_units,
            ..ProvisionedThroughputDescription::default()
        },
        billing_mode_summary,
        deletion_protection_enabled: record.deletion_protection_enabled,
        ..TableDescription::default()
    }
}

fn target(account_id: &str) -> Result<CellTarget, StorageError> {
    account_target(account_id).map_err(|error| StorageError::Validation(error.to_string()))
}

fn target_from_table_id(table_id: &str) -> Result<CellTarget, StorageError> {
    let hash = blake3::Hash::from_hex(table_id)
        .map_err(|_| StorageError::TableNotFound(table_id.to_owned()))?;
    let mut tenant = [0_u8; 16];
    tenant.copy_from_slice(&hash.as_bytes()[..16]);
    CellTarget::new(
        TenantId::from_bytes(tenant),
        APPLICATION,
        NAMESPACE,
        &partition_for_shard(0),
    )
    .map_err(|error| StorageError::Internal(error.to_string()))
}

pub(crate) fn mutation_identity() -> Result<MutationIdentity, StorageError> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| StorageError::Internal(error.to_string()))?;
    let now_ms = i64::try_from(elapsed.as_millis())
        .map_err(|error| StorageError::Internal(error.to_string()))?;
    Ok(MutationIdentity {
        request_id: RequestId::from_bytes(*uuid::Uuid::now_v7().as_bytes()),
        issued_at_ms: now_ms,
        expires_at_ms: now_ms.saturating_add(60_000),
    })
}

pub(crate) fn cell_error<T>(error: InvocationError<T>) -> StorageError {
    match error {
        InvocationError::Pending(_) | InvocationError::NotStarted(_) => {
            StorageError::Transient(error.to_string())
        }
        InvocationError::InvalidPublishedResult { .. } | InvocationError::Rejected(_) => {
            StorageError::Internal(error.to_string())
        }
    }
}

fn unsupported(feature: &str) -> StorageError {
    StorageError::Unsupported(feature.to_owned())
}
