//! Leaf-owned index split intents and exact membership publication.

use super::{GlobalIndexPartitionSpec, GlobalIndexSplitPlan};
use crate::table::statement;
use crate::{Error, Json, Result, RoutePagePartition, SqlBatch, SqlResultSet, SqlValue};
use crab_cell_runtime::registry::{Command, CommandContext, CommandResult, Query, QueryContext};
use serde::{Deserialize, Serialize};

/// Address an index participant within the directory leaf selected by its logical bound.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GlobalIndexPartitionInput {
    pub index_id: String,
    pub partition_id: [u8; 16],
}

fn visible(sql: impl FnOnce(&SqlBatch) -> Result<Vec<SqlResultSet>>, index: &str) -> Result<bool> {
    Ok(crate::directory::state(sql)?.is_some_and(|state| {
        state.spec.table_id == index
            && matches!(
                state.mode,
                crate::DirectoryMode::Leaf | crate::DirectoryMode::Frozen(_)
            )
    }))
}

/// Read one range from its current metadata leaf.
pub struct ReadPublishedGlobalIndexPartition;
impl Query for ReadPublishedGlobalIndexPartition {
    const MODULE: &'static str = crate::directory::MODULE;
    const ID: u32 = 4;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<GlobalIndexPartitionInput>;
    type Output = Json<Option<RoutePagePartition>>;
    fn execute(context: &mut QueryContext<'_>, Json(input): Self::Input) -> Result<Self::Output> {
        if !visible(|batch| context.sql(batch), &input.index_id)? {
            return Ok(Json(None));
        }
        let rows = context.sql(&statement(
            "SELECT record FROM ddb_directory_ranges WHERE partition_id = ?1",
            vec![SqlValue::Blob(input.partition_id.to_vec())],
        ))?;
        match rows[0].rows.first().map(Vec::as_slice) {
            None => Ok(Json(None)),
            Some([SqlValue::Blob(bytes)]) => Ok(Json(Some(serde_json::from_slice(bytes)?))),
            _ => Err(Error::Command("invalid index directory member")),
        }
    }
}

fn read_plan(
    sql: impl Fn(&SqlBatch) -> Result<Vec<SqlResultSet>>,
    input: &GlobalIndexPartitionInput,
) -> Result<Option<GlobalIndexSplitPlan>> {
    if !visible(&sql, &input.index_id)? {
        return Ok(None);
    }
    let rows = sql(&statement(
        "SELECT p.plan FROM ddb_directory_members m JOIN ddb_directory_index_changes p ON p.lower_bound = m.lower_bound WHERE m.partition_id = ?1",
        vec![SqlValue::Blob(input.partition_id.to_vec())],
    ))?;
    match rows[0].rows.first().map(Vec::as_slice) {
        None => Ok(None),
        Some([SqlValue::Blob(bytes)]) => Ok(Some(serde_json::from_slice(bytes)?)),
        _ => Err(Error::Command("invalid index split plan")),
    }
}

/// Discover an unfinished transfer through its source or either child in one leaf.
pub struct ReadGlobalIndexSplitPlan;
impl Query for ReadGlobalIndexSplitPlan {
    const MODULE: &'static str = crate::directory::MODULE;
    const ID: u32 = 5;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<GlobalIndexPartitionInput>;
    type Output = Json<Option<GlobalIndexSplitPlan>>;
    fn execute(context: &mut QueryContext<'_>, Json(input): Self::Input) -> Result<Self::Output> {
        Ok(Json(read_plan(|batch| context.sql(batch), &input)?))
    }
}

impl GlobalIndexSplitPlan {
    pub(crate) fn directory_change(&self) -> crate::DirectoryChange {
        let range = |spec: &GlobalIndexPartitionSpec| RoutePagePartition {
            partition_id: spec.partition_id,
            lower: spec.lower.unwrap_or([0; 16]),
            upper: spec.upper,
            epoch: spec.epoch,
        };
        crate::DirectoryChange {
            source: range(&self.source),
            children: self.children.each_ref().map(range),
        }
    }
}

fn source_input(plan: &GlobalIndexSplitPlan) -> GlobalIndexPartitionInput {
    GlobalIndexPartitionInput {
        index_id: plan.source.index.id.clone(),
        partition_id: plan.source.partition_id,
    }
}

/// Reserve the exact transfer in the leaf that owns its source interval.
pub struct BeginGlobalIndexSplit;
impl Command for BeginGlobalIndexSplit {
    const MODULE: &'static str = crate::directory::MODULE;
    const ID: u32 = 10;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<GlobalIndexSplitPlan>;
    type Output = Json<bool>;
    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(plan): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        if !plan.valid() || !visible(|batch| context.sql(batch), &plan.source.index.id)? {
            return Ok(CommandResult::Rejected(Json(false)));
        }
        if let Some(existing) = read_plan(|batch| context.sql(batch), &source_input(&plan))? {
            return Ok(if existing == plan {
                CommandResult::Success(Json(true))
            } else {
                CommandResult::Rejected(Json(false))
            });
        }
        let result = crate::BeginDirectoryChange::execute(context, Json(plan.directory_change()))?;
        if matches!(result, CommandResult::Success(_)) {
            // Compact reservations fence metadata movement. Keep the full source
            // contract beside them so recovery never rebuilds a plan from a later
            // table description or needs a missing participant to identify it.
            context.sql(&statement(
                "INSERT INTO ddb_directory_index_changes VALUES (?1, ?2)",
                vec![
                    SqlValue::Blob(plan.source.lower.unwrap_or([0; 16]).to_vec()),
                    SqlValue::Blob(serde_json::to_vec(&plan)?),
                ],
            ))?;
        }
        Ok(result)
    }
}

/// Replace the reserved source after the controller verifies both durable copies.
pub struct CommitGlobalIndexSplit;
impl Command for CommitGlobalIndexSplit {
    const MODULE: &'static str = crate::directory::MODULE;
    const ID: u32 = 11;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<GlobalIndexSplitPlan>;
    type Output = Json<bool>;
    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(plan): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        if read_plan(|batch| context.sql(batch), &source_input(&plan))?.as_ref() != Some(&plan) {
            return Ok(CommandResult::Rejected(Json(false)));
        }
        crate::PublishDirectoryChange::execute(context, Json(plan.directory_change()))
    }
}

/// Remove the full transfer and participant reservations after both children open.
pub struct FinishGlobalIndexSplit;
impl Command for FinishGlobalIndexSplit {
    const MODULE: &'static str = crate::directory::MODULE;
    const ID: u32 = 12;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<GlobalIndexSplitPlan>;
    type Output = Json<bool>;
    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(plan): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        if !visible(|batch| context.sql(batch), &plan.source.index.id)?
            || read_plan(|batch| context.sql(batch), &source_input(&plan))?
                .as_ref()
                .is_some_and(|existing| existing != &plan)
        {
            return Ok(CommandResult::Rejected(Json(false)));
        }
        crate::FinishDirectoryChange::execute(context, Json(plan.directory_change()))
    }
}
