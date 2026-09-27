//! Indexed account reads for split planning and publication recovery.

use crab_cell_runtime::registry::{Command, CommandContext, CommandResult, Query, QueryContext};
use serde::{Deserialize, Serialize};

use super::{SplitPlan, decode_page_partition, insert_route_partition, parse_epoch};
use crate::table::{TableRecord, decode_table, statement};
use crate::{Json, MODULE, PartitionSpec, Result, SqlBatch, SqlResultSet, SqlValue};

/// Identify one published range by its data Cell partition ID.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PublishedPartitionInput {
    /// Immutable table identity.
    pub table_id: String,
    /// Data Cell partition identity.
    pub partition_id: [u8; 16],
}

/// Published state of one data Cell partition ID.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum PublishedPartitionOutcome {
    /// The table has no active route.
    Unrouted,
    /// The route exists but this partition no longer owns a range.
    Missing,
    /// The partition's immutable range and the current directory epoch.
    Published {
        route_epoch: u64,
        spec: Box<PartitionSpec>,
    },
}

/// Read one indexed owner row and the route epoch.
pub struct ReadPublishedPartition;

impl Query for ReadPublishedPartition {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 16;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<PublishedPartitionInput>;
    type Output = Json<PublishedPartitionOutcome>;

    fn execute(context: &mut QueryContext<'_>, Json(input): Self::Input) -> Result<Self::Output> {
        let Some((epoch, table)) = route_head(&input.table_id, |batch| context.sql(batch))? else {
            return Ok(Json(PublishedPartitionOutcome::Unrouted));
        };
        let rows = context.sql(&statement(
            "SELECT partition_id, lower_bound, upper_bound, epoch \
             FROM ddb_route_partitions WHERE table_id = ?1 AND partition_id = ?2",
            vec![
                SqlValue::Text(input.table_id),
                SqlValue::Blob(input.partition_id.to_vec()),
            ],
        ))?;
        let Some(row) = rows[0].rows.first() else {
            return Ok(Json(PublishedPartitionOutcome::Missing));
        };
        Ok(Json(PublishedPartitionOutcome::Published {
            route_epoch: epoch,
            spec: Box::new(partition_spec(row, &table)?),
        }))
    }
}

/// Position of one durable split relative to the indexed route.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SplitRouteState {
    /// The table has no active route.
    Unrouted,
    /// The source is published and neither child is published.
    Before,
    /// The children are published and the source is absent.
    After,
    /// The directory no longer matches this plan.
    Changed,
}

/// Compare one compact split plan with the published directory atomically.
pub struct ReadSplitRoute;

impl Query for ReadSplitRoute {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 17;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<SplitPlan>;
    type Output = Json<SplitRouteState>;

    fn execute(context: &mut QueryContext<'_>, Json(plan): Self::Input) -> Result<Self::Output> {
        Ok(Json(split_route_state(&plan, |batch| context.sql(batch))?))
    }
}

fn split_route_state(
    plan: &SplitPlan,
    sql: impl Fn(&SqlBatch) -> Result<Vec<SqlResultSet>>,
) -> Result<SplitRouteState> {
    let table_id = &plan.source.table.id;
    let Some((epoch, table)) = route_head(table_id, &sql)? else {
        return Ok(SplitRouteState::Unrouted);
    };
    if table != plan.source.table || !plan.valid_for(&table) {
        return Ok(SplitRouteState::Changed);
    }
    let rows = sql(&statement(
        "SELECT partition_id, lower_bound, upper_bound, epoch \
         FROM ddb_route_partitions WHERE table_id = ?1 \
         AND partition_id IN (?2, ?3, ?4)",
        vec![
            SqlValue::Text(table_id.clone()),
            SqlValue::Blob(plan.source.partition_id.to_vec()),
            SqlValue::Blob(plan.children[0].partition_id.to_vec()),
            SqlValue::Blob(plan.children[1].partition_id.to_vec()),
        ],
    ))?;
    let mut source = false;
    let mut children = [false; 2];
    for row in &rows[0].rows {
        let spec = partition_spec(row, &table)?;
        if spec.partition_id == plan.source.partition_id {
            source = spec == plan.source;
        } else if spec.partition_id == plan.children[0].partition_id {
            children[0] = spec == plan.children[0];
        } else if spec.partition_id == plan.children[1].partition_id {
            children[1] = spec == plan.children[1];
        }
    }
    // The exact source/child identities fence this split. A newer directory
    // epoch can belong to an unrelated range and must not strand a sealed source.
    let state = if epoch >= plan.expected_epoch && source && rows[0].rows.len() == 1 {
        SplitRouteState::Before
    } else if plan.next_epoch().is_some_and(|next| epoch >= next)
        && !source
        && children == [true, true]
        && rows[0].rows.len() == 2
    {
        SplitRouteState::After
    } else {
        SplitRouteState::Changed
    };
    Ok(state)
}

fn route_head(
    table_id: &str,
    sql: impl Fn(&SqlBatch) -> Result<Vec<SqlResultSet>>,
) -> Result<Option<(u64, TableRecord)>> {
    let rows = sql(&statement(
        "SELECT route_epoch, route_table FROM ddb_routes WHERE table_id = ?1",
        vec![SqlValue::Text(table_id.to_owned())],
    ))?;
    let Some(row) = rows[0].rows.first() else {
        return Ok(None);
    };
    let [SqlValue::Text(epoch), SqlValue::Blob(table)] = row.as_slice() else {
        return Err(crate::Error::Command("invalid route metadata"));
    };
    let table: TableRecord = serde_json::from_slice(table)?;
    if table.id != table_id {
        return Err(crate::Error::Command("invalid route table identity"));
    }
    Ok(Some((parse_epoch(epoch)?, table)))
}

fn partition_spec(row: &Vec<SqlValue>, table: &TableRecord) -> Result<PartitionSpec> {
    let range = decode_page_partition(row)?;
    Ok(PartitionSpec {
        table: table.clone(),
        partition_id: range.partition_id,
        lower: (range.lower != [0; 16]).then_some(range.lower),
        upper: range.upper,
        epoch: range.epoch,
    })
}

/// Outcome of recording one split plan.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum BeginSplitOutcome {
    /// This exact plan is durable.
    Planned,
    /// The table does not exist.
    TableNotFound,
    /// The table has no active route.
    RouteNotFound,
    /// The proposed route is not a valid one-range split.
    InvalidPlan,
    /// Another split plan reserves this source or one of its children.
    Conflict,
}

/// Record a single-range split before provisioning or copying its children.
pub struct BeginSplit;

impl Command for BeginSplit {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 11;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<SplitPlan>;
    type Output = Json<BeginSplitOutcome>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(plan): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        let table_id = &plan.source.table.id;
        let table_rows = context.sql(&statement(
            "SELECT record FROM ddb_tables WHERE table_id = ?1",
            vec![SqlValue::Text(table_id.clone())],
        ))?;
        let Some(table) = decode_table(&table_rows[0])? else {
            return Ok(CommandResult::Rejected(Json(
                BeginSplitOutcome::TableNotFound,
            )));
        };
        let state = split_route_state(&plan, |batch| context.sql(batch))?;
        if state == SplitRouteState::Unrouted {
            return Ok(CommandResult::Rejected(Json(
                BeginSplitOutcome::RouteNotFound,
            )));
        }
        if !plan.valid_for(&table) || state != SplitRouteState::Before {
            return Ok(CommandResult::Rejected(Json(
                BeginSplitOutcome::InvalidPlan,
            )));
        }
        if let Some(existing) =
            read_partition_plan(&plan.source.table.id, plan.source.partition_id, |batch| {
                context.sql(batch)
            })?
        {
            return Ok(if existing == plan {
                CommandResult::Success(Json(BeginSplitOutcome::Planned))
            } else {
                CommandResult::Rejected(Json(BeginSplitOutcome::Conflict))
            });
        }
        for child in &plan.children {
            if read_partition_plan(table_id, child.partition_id, |batch| context.sql(batch))?
                .is_some()
            {
                return Ok(CommandResult::Rejected(Json(BeginSplitOutcome::Conflict)));
            }
        }
        context.sql(&statement(
            "INSERT INTO ddb_split_plans (table_id, source_partition_id, plan) VALUES (?1, ?2, ?3)",
            vec![
                SqlValue::Text(table_id.clone()),
                SqlValue::Blob(plan.source.partition_id.to_vec()),
                SqlValue::Blob(serde_json::to_vec(&plan)?),
            ],
        ))?;
        for spec in [&plan.source, &plan.children[0], &plan.children[1]] {
            context.sql(&statement(
                "INSERT INTO ddb_split_members (table_id, partition_id, source_partition_id) VALUES (?1, ?2, ?3)",
                vec![SqlValue::Text(table_id.clone()), SqlValue::Blob(spec.partition_id.to_vec()), SqlValue::Blob(plan.source.partition_id.to_vec())],
            ))?;
        }
        Ok(CommandResult::Success(Json(BeginSplitOutcome::Planned)))
    }
}

/// Outcome of atomically replacing a published route with its planned split.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum CommitSplitOutcome {
    /// The planned route is published; unfinished opening retains the plan.
    Committed,
    /// The table or its published route no longer exists.
    RouteNotFound,
    /// No matching split plan is durable for this source range.
    PlanNotFound,
    /// The durable plan differs from the submitted plan.
    PlanMismatch,
    /// The published route changed since the split was planned.
    RouteChanged,
}

/// Publish a split after a trusted coordinator verifies both activated children.
///
/// This account-local compare-and-swap does not inspect other Cells. The caller
/// must verify the sealed source and both child activations before invoking it.
pub struct CommitSplit;

impl Command for CommitSplit {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 12;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<SplitPlan>;
    type Output = Json<CommitSplitOutcome>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(plan): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        let table_id = &plan.source.table.id;
        let table_rows = context.sql(&statement(
            "SELECT record FROM ddb_tables WHERE table_id = ?1",
            vec![SqlValue::Text(table_id.clone())],
        ))?;
        if decode_table(&table_rows[0])?.is_none() {
            return Ok(CommandResult::Rejected(Json(
                CommitSplitOutcome::RouteNotFound,
            )));
        }
        let state = split_route_state(&plan, |batch| context.sql(batch))?;
        if state == SplitRouteState::Unrouted {
            return Ok(CommandResult::Rejected(Json(
                CommitSplitOutcome::RouteNotFound,
            )));
        }
        let plan_rows = context.sql(&statement(
            "SELECT plan FROM ddb_split_plans WHERE table_id = ?1 AND source_partition_id = ?2",
            vec![
                SqlValue::Text(table_id.clone()),
                SqlValue::Blob(plan.source.partition_id.to_vec()),
            ],
        ))?;
        let Some(durable) = decode_plan(&plan_rows[0])? else {
            return Ok(if state == SplitRouteState::After {
                CommandResult::Success(Json(CommitSplitOutcome::Committed))
            } else {
                CommandResult::Rejected(Json(CommitSplitOutcome::PlanNotFound))
            });
        };
        if durable != plan {
            return Ok(CommandResult::Rejected(Json(
                CommitSplitOutcome::PlanMismatch,
            )));
        }
        if state == SplitRouteState::After {
            return Ok(CommandResult::Success(Json(CommitSplitOutcome::Committed)));
        }
        if state != SplitRouteState::Before {
            return Ok(CommandResult::Rejected(Json(
                CommitSplitOutcome::RouteChanged,
            )));
        }
        // Unrelated sources can publish between planning and this commit. Advance
        // the current directory epoch while retaining the planned child epochs;
        // paged readers must detect every publication, including concurrent splits.
        let (epoch, _) = route_head(table_id, |batch| context.sql(batch))?
            .ok_or(crate::Error::Command("split route disappeared"))?;
        let next_epoch = epoch
            .checked_add(1)
            .ok_or(crate::Error::Command("split epoch exhausted"))?;
        context.sql(&statement(
            "UPDATE ddb_routes SET route_epoch = ?2 WHERE table_id = ?1",
            vec![
                SqlValue::Text(table_id.clone()),
                SqlValue::Text(next_epoch.to_string()),
            ],
        ))?;
        let removed = context.sql(&statement(
            "DELETE FROM ddb_route_partitions WHERE table_id = ?1 AND partition_id = ?2",
            vec![
                SqlValue::Text(table_id.clone()),
                SqlValue::Blob(plan.source.partition_id.to_vec()),
            ],
        ))?;
        if removed[0].rows_affected != 1 {
            return Err(crate::Error::Command("split source route row is missing"));
        }
        for partition in &plan.children {
            insert_route_partition(context, partition)?;
        }
        Ok(CommandResult::Success(Json(CommitSplitOutcome::Committed)))
    }
}

/// Read the first pending source plan in ID order for bounded table recovery.
pub struct ReadSplitPlan;

impl Query for ReadSplitPlan {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 12;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<String>;
    type Output = Json<Option<SplitPlan>>;

    fn execute(
        context: &mut QueryContext<'_>,
        Json(table_id): Self::Input,
    ) -> Result<Self::Output> {
        Ok(Json(decode_plan(
            &context.sql(&statement(
                "SELECT plan FROM ddb_split_plans WHERE table_id = ?1 ORDER BY source_partition_id LIMIT 1",
                vec![SqlValue::Text(table_id)],
            ))?[0],
        )?))
    }
}

/// Read a pending split through its source or either replacement partition.
pub struct ReadPartitionSplitPlan;

impl Query for ReadPartitionSplitPlan {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 32;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<PublishedPartitionInput>;
    type Output = Json<Option<SplitPlan>>;

    fn execute(context: &mut QueryContext<'_>, Json(input): Self::Input) -> Result<Self::Output> {
        Ok(Json(read_partition_plan(
            &input.table_id,
            input.partition_id,
            |batch| context.sql(batch),
        )?))
    }
}

fn read_partition_plan(
    table_id: &str,
    partition_id: [u8; 16],
    sql: impl Fn(&SqlBatch) -> Result<Vec<SqlResultSet>>,
) -> Result<Option<SplitPlan>> {
    decode_plan(
        &sql(&statement(
            "SELECT p.plan FROM ddb_split_members m JOIN ddb_split_plans p ON p.table_id = m.table_id AND p.source_partition_id = m.source_partition_id WHERE m.table_id = ?1 AND m.partition_id = ?2",
            vec![
                SqlValue::Text(table_id.into()),
                SqlValue::Blob(partition_id.to_vec()),
            ],
        ))?[0],
    )
}

/// Retire a published plan after the trusted controller verifies both children open.
pub struct FinishSplit;

impl Command for FinishSplit {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 30;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<SplitPlan>;
    type Output = Json<bool>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(plan): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        let table_id = &plan.source.table.id;
        if split_route_state(&plan, |batch| context.sql(batch))? != SplitRouteState::After
            || read_partition_plan(table_id, plan.source.partition_id, |batch| {
                context.sql(batch)
            })?
            .as_ref()
            .is_some_and(|existing| existing != &plan)
        {
            return Ok(CommandResult::Rejected(Json(false)));
        }
        context.sql(&statement(
            "DELETE FROM ddb_split_plans WHERE table_id = ?1 AND source_partition_id = ?2",
            vec![
                SqlValue::Text(table_id.clone()),
                SqlValue::Blob(plan.source.partition_id.to_vec()),
            ],
        ))?;
        Ok(CommandResult::Success(Json(true)))
    }
}

fn decode_plan(rows: &SqlResultSet) -> Result<Option<SplitPlan>> {
    let Some(row) = rows.rows.first() else {
        return Ok(None);
    };
    let [SqlValue::Blob(bytes)] = row.as_slice() else {
        return Err(crate::Error::Command("invalid split plan row"));
    };
    Ok(Some(serde_json::from_slice(bytes)?))
}
