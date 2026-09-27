//! Routed capacity sweeps and recoverable data-range splits.

use std::{sync::Arc, time::Duration};

use crab_cell_host::CellNodeTaskGroup;
use crab_cell_runtime::{
    cell::catalog::CellCatalog,
    client::{CellClient, InvocationError, ReadPolicy},
};
use extenddb_storage::{BoxedFuture, error::StorageError};
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;

use super::{CapacityCursor, CellInitialPartitionProvisioner, provision_error, split_plan};
use crate::backend::{cell_error, mutation_identity};
use crate::split::split_contract;
use crate::{
    BeginSplit, BeginSplitOutcome, CellSplitController, DescribeTable, Json, ListTables,
    ListTablesInput, ListTablesOutcome, PartitionState, PartitionUsage, PublishedPartitionInput,
    PublishedPartitionOutcome, ReadGlobalIndexRoutePage, ReadPartitionState,
    ReadPublishedPartition, ReadRoutePage, ReadSourceSplitPlan, ReadSplitPlan, ReadSplitRoute,
    RoutePageInput, RoutePageOutcome, SplitPlan, SplitRouteState, account_target, data_target,
};

impl CellInitialPartitionProvisioner {
    /// Inspect one base/index range or resume its pending split; return whether a split completed.
    ///
    /// `None` starts a new pass. Once a range is selected, the cursor advances
    /// even if its split fails; its durable plan remains for the next pass.
    /// Discovery failures preserve the cursor. Reads use current owners.
    pub async fn reconcile_account_capacity(
        &self,
        account_id: &str,
        client: CellClient,
        max_database_bytes: u64,
        cursor: &mut Option<CapacityCursor>,
    ) -> Result<bool, StorageError> {
        if max_database_bytes == 0 {
            return Err(StorageError::Validation(
                "database byte split threshold must be positive".into(),
            ));
        }
        let account = account_target(account_id).map_err(provision_error)?;
        let client = client.with_read_policy(ReadPolicy::CurrentOwner);
        let (name, after_lower, index) = if let Some(cursor) = cursor.as_ref()
            && (cursor.after_lower.is_some() || cursor.index.is_some())
        {
            (cursor.table_name.clone(), cursor.after_lower, cursor.index)
        } else {
            let page = client
                .query::<ListTables>(
                    &account,
                    None,
                    Json(ListTablesInput {
                        limit: 1,
                        exclusive_start: cursor.as_ref().map(|cursor| cursor.table_name.clone()),
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
                *cursor = None;
                return Ok(false);
            };
            (name, None, None)
        };
        let Some(table) = client
            .query::<DescribeTable>(&account, None, Json(name.clone()))
            .await
            .map_err(cell_error)?
            .output
            .0
        else {
            *cursor = Some(CapacityCursor {
                table_name: name,
                after_lower: None,
                index: None,
            });
            return Ok(false);
        };
        let index_record = match index {
            Some(position) => match table.global_secondary_indexes.get(position) {
                Some(index) => Some(index),
                None => {
                    *cursor = Some(CapacityCursor {
                        table_name: name,
                        after_lower: None,
                        index: None,
                    });
                    return Ok(false);
                }
            },
            None => None,
        };
        let input = Json(RoutePageInput {
            table_id: index_record.map_or_else(|| table.id.clone(), |index| index.id.clone()),
            start_hash: None,
            after_lower,
            expected_epoch: None,
        });
        let page = if index_record.is_some() {
            client
                .query::<ReadGlobalIndexRoutePage>(&account, None, input)
                .await
        } else {
            client.query::<ReadRoutePage>(&account, None, input).await
        }
        .map_err(cell_error)?
        .output
        .0;
        let (partitions, has_more) = match page {
            RoutePageOutcome::Page {
                partitions,
                has_more,
                ..
            } => (partitions, has_more),
            RoutePageOutcome::Unrouted if index.is_some() => (Vec::new(), false),
            RoutePageOutcome::Unrouted => {
                // Do not resize indexes while CreateTable is still publishing
                // its initial base route and may retry index installation.
                *cursor = Some(CapacityCursor {
                    table_name: name,
                    after_lower: None,
                    index: None,
                });
                return Ok(false);
            }
            RoutePageOutcome::Changed => {
                return Err(StorageError::Transient(
                    "capacity route changed; retry".into(),
                ));
            }
        };
        let partition = partitions.first();
        let more = partitions.len() > 1 || has_more;
        let next_index = if more {
            index
        } else {
            let next = index.map_or(0, |index| index + 1);
            (next < table.global_secondary_indexes.len()).then_some(next)
        };
        // Durable source/child reservations retain retry state. Advance before
        // attempting a range so unavailable owners cannot starve other ranges.
        *cursor = Some(CapacityCursor {
            table_name: name,
            after_lower: if more {
                partition.map(|partition| partition.lower)
            } else {
                None
            },
            index: next_index,
        });
        let Some(partition) = partition else {
            return Ok(false);
        };
        if let Some(index) = index_record {
            self.split_global_index_if_over_database_bytes(
                account_id,
                client,
                &index.id,
                partition.partition_id,
                max_database_bytes,
            )
            .await
            .map(|plan| plan.is_some())
        } else {
            self.split_if_over_database_bytes(
                account_id,
                client,
                &table.id,
                partition.partition_id,
                max_database_bytes,
            )
            .await
            .map(|plan| plan.is_some())
        }
    }

    /// Repeat bounded account sweeps until cancellation or an actionable error.
    pub async fn run_account_capacity_loop(
        &self,
        account_id: &str,
        client: CellClient,
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
                    client.clone(),
                    max_database_bytes,
                    &mut cursor,
                )
                .await;
            match result {
                Ok(_) => {}
                // The cursor advances before work; durable plans retain retries.
                // Fleet pressure or an indivisible range must not terminate
                // healthy serving while other ranges can still make progress.
                Err(StorageError::Transient(error) | StorageError::LimitExceeded(error)) => {
                    tracing::warn!(account_id, %error, "capacity sweep deferred");
                }
                Err(error) => return Err(error),
            }
        }
    }

    /// Retain the account's split loop in the node's serving task group.
    ///
    /// Node shutdown cancels the loop before draining Cell owners.
    pub fn install_account_capacity_loop(
        self: &Arc<Self>,
        tasks: &CellNodeTaskGroup,
        account_id: String,
        client: CellClient,
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
                        client.clone(),
                        max_database_bytes,
                        interval,
                        &cancellation,
                    )
                    .await
            })
            .map_err(provision_error)
    }

    /// Split one oversized range or resume the table's first pending source plan.
    ///
    /// A serving control loop should repeat this call while the table is active.
    pub async fn reconcile_table_capacity(
        &self,
        account_id: &str,
        client: CellClient,
        table_id: &str,
        max_database_bytes: u64,
    ) -> Result<Option<SplitPlan>, StorageError> {
        if max_database_bytes == 0 {
            return Err(StorageError::Validation(
                "database byte split threshold must be positive".into(),
            ));
        }
        let account = account_target(account_id).map_err(provision_error)?;
        let client = client.with_read_policy(ReadPolicy::CurrentOwner);
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
                    client.clone(),
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
                        client.clone(),
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

    /// Split a range once its occupied SQLite pages exceed `max_database_bytes`.
    ///
    /// The routed client must reach the source and restore idle Cells. The
    /// caller must repeat this check as part of its capacity loop. A pending
    /// split is resumed even if its source no longer crosses the threshold.
    pub async fn split_if_over_database_bytes(
        &self,
        account_id: &str,
        client: CellClient,
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
        let client = client.with_read_policy(ReadPolicy::CurrentOwner);
        if client
            .query::<ReadSourceSplitPlan>(
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
            .0
            .is_some()
        {
            return self
                .split_partition(account_id, client.clone(), table_id, source_partition_id)
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
                .split_partition(account_id, client.clone(), table_id, source_partition_id)
                .await
                .map(Some);
        }
        let target =
            data_target(account_id, table_id, &source_partition_id).map_err(provision_error)?;
        let usage = client
            .query::<PartitionUsage>(&target, None, Json(()))
            .await
            .map_err(cell_error)?
            .output
            .0;
        if usage.database_bytes <= max_database_bytes {
            return Ok(None);
        }
        self.split_partition(account_id, client.clone(), table_id, source_partition_id)
            .await
            .map(Some)
    }

    /// Plan and finish one additional data Cell range split.
    ///
    /// The source range is unavailable between its seal and the route switch.
    /// A retry resumes a durable plan for the same source after interruption.
    pub fn split_partition<'a>(
        &'a self,
        account_id: &'a str,
        client: CellClient,
        table_id: &'a str,
        source_partition_id: [u8; 16],
    ) -> BoxedFuture<'a, Result<SplitPlan, StorageError>> {
        // Split recovery nests admission and replay futures. Keep that state off
        // the capacity caller's stack, including retries of completed splits.
        Box::pin(async move {
            let account = account_target(account_id).map_err(provision_error)?;
            let client = client.with_read_policy(ReadPolicy::CurrentOwner);
            if let Some(plan) = client
                .query::<ReadSourceSplitPlan>(
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
                .0
            {
                self.resume_split(account_id, client.clone(), &plan).await?;
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
                        .completed_split(
                            account_id,
                            table_id,
                            source_partition_id,
                            &client,
                            &account,
                        )
                        .await?
                        .ok_or_else(|| {
                            StorageError::Validation(
                                "split source is absent from table route".into(),
                            )
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
            self.resume_split(account_id, client.clone(), &plan).await?;
            Ok(plan)
        })
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
        if catalog
            .lookup(target.cell_id())
            .await
            .map_err(provision_error)?
            .is_none()
        {
            return Ok(None);
        }
        let status = client
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
    /// The client must reach the account, source, and published children,
    /// restoring idle owners when necessary. New children use configured fleet
    /// placement; retries preserve existing owners and the recorded plan.
    pub async fn resume_split(
        &self,
        account_id: &str,
        client: CellClient,
        plan: &SplitPlan,
    ) -> Result<(), StorageError> {
        let (source, children, _) = split_contract(plan)?;
        let account = account_target(account_id).map_err(provision_error)?;
        let client = client.with_read_policy(ReadPolicy::CurrentOwner);
        let pending = client
            .query::<ReadSourceSplitPlan>(
                &account,
                None,
                Json(PublishedPartitionInput {
                    table_id: source.table.id.clone(),
                    partition_id: source.partition_id,
                }),
            )
            .await
            .map_err(cell_error)?
            .output
            .0;
        let route_state = client
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
        if catalog
            .lookup(source_target.cell_id())
            .await
            .map_err(provision_error)?
            .is_none()
        {
            return Err(StorageError::Transient(
                "split source is not cataloged".into(),
            ));
        }
        for spec in children {
            let target = data_target(account_id, &spec.table.id, &spec.partition_id)
                .map_err(provision_error)?;
            self.provision_range(&target, &client).await?;
        }
        CellSplitController::new(client)
            .resume(account_id, plan)
            .await
    }
}
