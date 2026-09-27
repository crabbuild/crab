//! Account-local split intent, participant reservation, and atomic publication.

use crab_cell_runtime::registry::{Command, CommandContext, CommandResult, Query, QueryContext};
use serde::{Deserialize, Serialize};

use super::{GlobalIndexPartitionSpec, GlobalIndexSplitPlan};
use crate::routing::{decode_page_partition, parse_epoch};
use crate::table::statement;
use crate::{
    Error, Json, Result, RoutePagePartition, SplitRouteState, SqlBatch, SqlResultSet, SqlValue,
};

/// Address a range within one immutable global-index generation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GlobalIndexPartitionInput {
    pub index_id: String,
    pub partition_id: [u8; 16],
}

/// Published range and directory generation used to plan its replacement.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GlobalIndexPartitionRoute {
    pub directory_epoch: u64,
    pub partition: RoutePagePartition,
}

/// Read one published index range without scanning the directory.
pub struct ReadPublishedGlobalIndexPartition;
impl Query for ReadPublishedGlobalIndexPartition {
    const MODULE: &'static str = crate::MODULE;
    const ID: u32 = 35;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<GlobalIndexPartitionInput>;
    type Output = Json<Option<GlobalIndexPartitionRoute>>;
    fn execute(context: &mut QueryContext<'_>, Json(input): Self::Input) -> Result<Self::Output> {
        let rows = context.sql(&statement("SELECT p.partition_id, p.lower_bound, p.upper_bound, p.epoch, r.route_epoch FROM ddb_global_index_partitions p JOIN ddb_global_index_routes r ON r.table_id = p.table_id WHERE p.table_id = ?1 AND p.partition_id = ?2", vec![SqlValue::Text(input.index_id), SqlValue::Blob(input.partition_id.to_vec())]))?;
        let Some(row) = rows[0].rows.first() else {
            return Ok(Json(None));
        };
        let Some(SqlValue::Text(epoch)) = row.get(4) else {
            return Err(Error::Command("invalid index route epoch"));
        };
        Ok(Json(Some(GlobalIndexPartitionRoute {
            directory_epoch: parse_epoch(epoch)?,
            partition: decode_page_partition(&row[..4].to_vec())?,
        })))
    }
}

fn read_plan(
    sql: impl Fn(&SqlBatch) -> Result<Vec<SqlResultSet>>,
    input: &GlobalIndexPartitionInput,
) -> Result<Option<GlobalIndexSplitPlan>> {
    let rows = sql(&statement(
        "SELECT p.plan FROM ddb_global_index_split_members m JOIN ddb_global_index_split_plans p ON p.table_id = m.table_id AND p.source_partition_id = m.source_partition_id WHERE m.table_id = ?1 AND m.partition_id = ?2",
        vec![
            SqlValue::Text(input.index_id.clone()),
            SqlValue::Blob(input.partition_id.to_vec()),
        ],
    ))?;
    match rows[0].rows.first().map(Vec::as_slice) {
        None => Ok(None),
        Some([SqlValue::Blob(bytes)]) => Ok(Some(serde_json::from_slice(bytes)?)),
        _ => Err(Error::Command("invalid index split plan")),
    }
}

/// Discover a split through its source or either child, including after cutover.
pub struct ReadGlobalIndexSplitPlan;
impl Query for ReadGlobalIndexSplitPlan {
    const MODULE: &'static str = crate::MODULE;
    const ID: u32 = 33;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<GlobalIndexPartitionInput>;
    type Output = Json<Option<GlobalIndexSplitPlan>>;
    fn execute(context: &mut QueryContext<'_>, Json(input): Self::Input) -> Result<Self::Output> {
        Ok(Json(read_plan(|batch| context.sql(batch), &input)?))
    }
}

fn range_row(spec: &GlobalIndexPartitionSpec) -> Vec<SqlValue> {
    vec![
        SqlValue::Blob(spec.partition_id.to_vec()),
        SqlValue::Blob(spec.lower.unwrap_or([0; 16]).to_vec()),
        SqlValue::Blob(
            spec.upper
                .map_or_else(|| vec![0xff; 17], |upper| upper.to_vec()),
        ),
        SqlValue::Text(spec.epoch.to_string()),
    ]
}

fn route_state(
    plan: &GlobalIndexSplitPlan,
    sql: impl Fn(&SqlBatch) -> Result<Vec<SqlResultSet>>,
) -> Result<(SplitRouteState, Option<u64>)> {
    let rows = sql(&statement(
        "SELECT r.route_epoch, t.record FROM ddb_global_index_routes r JOIN ddb_live_tables t ON t.table_id = r.base_table_id WHERE r.table_id = ?1",
        vec![SqlValue::Text(plan.source.index.id.clone())],
    ))?;
    let Some(row) = rows[0].rows.first() else {
        return Ok((SplitRouteState::Unrouted, None));
    };
    let [SqlValue::Text(epoch), SqlValue::Blob(record)] = row.as_slice() else {
        return Err(Error::Command("invalid index directory"));
    };
    let epoch = parse_epoch(epoch)?;
    let table: crate::TableRecord = serde_json::from_slice(record)?;
    let source = &plan.source;
    // Billing and protection metadata can change during a transfer. Only the
    // immutable key contract and index generation determine its ownership.
    if !plan.valid()
        || table.id != source.table.id
        || table.key_schema != source.table.key_schema
        || table.attribute_definitions != source.table.attribute_definitions
        || !table.global_secondary_indexes.contains(&source.index)
    {
        return Ok((SplitRouteState::Changed, Some(epoch)));
    }
    let rows = sql(&statement(
        "SELECT partition_id, lower_bound, upper_bound, epoch FROM ddb_global_index_partitions WHERE table_id = ?1 AND partition_id IN (?2, ?3, ?4)",
        vec![
            SqlValue::Text(source.index.id.clone()),
            SqlValue::Blob(source.partition_id.to_vec()),
            SqlValue::Blob(plan.children[0].partition_id.to_vec()),
            SqlValue::Blob(plan.children[1].partition_id.to_vec()),
        ],
    ))?;
    let rows = &rows[0].rows;
    let state = if epoch >= plan.expected_epoch && rows == &[range_row(source)] {
        SplitRouteState::Before
    } else if epoch > plan.expected_epoch
        && rows.len() == 2
        && plan
            .children
            .iter()
            .all(|child| rows.contains(&range_row(child)))
    {
        SplitRouteState::After
    } else {
        SplitRouteState::Changed
    };
    Ok((state, Some(epoch)))
}

/// Compare the exact source/child range rows with one immutable split plan.
pub struct ReadGlobalIndexSplitRoute;
impl Query for ReadGlobalIndexSplitRoute {
    const MODULE: &'static str = crate::MODULE;
    const ID: u32 = 34;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<GlobalIndexSplitPlan>;
    type Output = Json<SplitRouteState>;
    fn execute(context: &mut QueryContext<'_>, Json(plan): Self::Input) -> Result<Self::Output> {
        Ok(Json(route_state(&plan, |batch| context.sql(batch))?.0))
    }
}

fn source_input(plan: &GlobalIndexSplitPlan) -> GlobalIndexPartitionInput {
    GlobalIndexPartitionInput {
        index_id: plan.source.index.id.clone(),
        partition_id: plan.source.partition_id,
    }
}

/// Reserve one source and both children before any index Cell is fenced.
pub struct BeginGlobalIndexSplit;
impl Command for BeginGlobalIndexSplit {
    const MODULE: &'static str = crate::MODULE;
    const ID: u32 = 27;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<GlobalIndexSplitPlan>;
    type Output = Json<bool>;
    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(plan): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        if route_state(&plan, |batch| context.sql(batch))?.0 != SplitRouteState::Before {
            return Ok(CommandResult::Rejected(Json(false)));
        }
        let input = source_input(&plan);
        if let Some(existing) = read_plan(|batch| context.sql(batch), &input)? {
            return Ok(if existing == plan {
                CommandResult::Success(Json(true))
            } else {
                CommandResult::Rejected(Json(false))
            });
        }
        for child in &plan.children {
            if read_plan(
                |batch| context.sql(batch),
                &GlobalIndexPartitionInput {
                    index_id: input.index_id.clone(),
                    partition_id: child.partition_id,
                },
            )?
            .is_some()
            {
                return Ok(CommandResult::Rejected(Json(false)));
            }
        }
        context.sql(&statement("INSERT INTO ddb_global_index_split_plans (table_id, source_partition_id, plan) VALUES (?1, ?2, ?3)", vec![SqlValue::Text(input.index_id.clone()), SqlValue::Blob(input.partition_id.to_vec()), SqlValue::Blob(serde_json::to_vec(&plan)?)]))?;
        for spec in [&plan.source, &plan.children[0], &plan.children[1]] {
            context.sql(&statement("INSERT INTO ddb_global_index_split_members (table_id, partition_id, source_partition_id) VALUES (?1, ?2, ?3)", vec![SqlValue::Text(input.index_id.clone()), SqlValue::Blob(spec.partition_id.to_vec()), SqlValue::Blob(input.partition_id.to_vec())]))?;
        }
        Ok(CommandResult::Success(Json(true)))
    }
}

/// Publish verified children atomically, retaining the plan until both are open.
///
/// The trusted controller must verify the sealed source and activated children first.
pub struct CommitGlobalIndexSplit;
impl Command for CommitGlobalIndexSplit {
    const MODULE: &'static str = crate::MODULE;
    const ID: u32 = 28;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<GlobalIndexSplitPlan>;
    type Output = Json<bool>;
    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(plan): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        let input = source_input(&plan);
        let existing = read_plan(|batch| context.sql(batch), &input)?;
        let (state, epoch) = route_state(&plan, |batch| context.sql(batch))?;
        if state == SplitRouteState::After
            && existing.as_ref().is_none_or(|existing| existing == &plan)
        {
            return Ok(CommandResult::Success(Json(true)));
        }
        if existing.as_ref() != Some(&plan) || state != SplitRouteState::Before {
            return Ok(CommandResult::Rejected(Json(false)));
        }
        let next = epoch
            .and_then(|epoch| epoch.checked_add(1))
            .ok_or(Error::Command("index directory epoch exhausted"))?;
        context.sql(&statement(
            "UPDATE ddb_global_index_routes SET route_epoch = ?2 WHERE table_id = ?1",
            vec![
                SqlValue::Text(input.index_id.clone()),
                SqlValue::Text(next.to_string()),
            ],
        ))?;
        let removed = context.sql(&statement(
            "DELETE FROM ddb_global_index_partitions WHERE table_id = ?1 AND partition_id = ?2",
            vec![
                SqlValue::Text(input.index_id.clone()),
                SqlValue::Blob(input.partition_id.to_vec()),
            ],
        ))?;
        if removed[0].rows_affected != 1 {
            return Err(Error::Command("index split source disappeared"));
        }
        for child in &plan.children {
            let mut row = vec![SqlValue::Text(input.index_id.clone())];
            row.extend(range_row(child));
            context.sql(&statement("INSERT INTO ddb_global_index_partitions (table_id, partition_id, lower_bound, upper_bound, epoch) VALUES (?1, ?2, ?3, ?4, ?5)", row))?;
        }
        Ok(CommandResult::Success(Json(true)))
    }
}

/// Retire a published plan after the trusted controller verifies both children open.
pub struct FinishGlobalIndexSplit;
impl Command for FinishGlobalIndexSplit {
    const MODULE: &'static str = crate::MODULE;
    const ID: u32 = 29;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<GlobalIndexSplitPlan>;
    type Output = Json<bool>;
    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(plan): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        let input = source_input(&plan);
        if route_state(&plan, |batch| context.sql(batch))?.0 != SplitRouteState::After
            || read_plan(|batch| context.sql(batch), &input)?
                .as_ref()
                .is_some_and(|existing| existing != &plan)
        {
            return Ok(CommandResult::Rejected(Json(false)));
        }
        context.sql(&statement("DELETE FROM ddb_global_index_split_plans WHERE table_id = ?1 AND source_partition_id = ?2", vec![SqlValue::Text(input.index_id), SqlValue::Blob(input.partition_id.to_vec())]))?;
        Ok(CommandResult::Success(Json(true)))
    }
}
