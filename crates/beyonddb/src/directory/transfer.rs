//! Leaf-owned base and index transfer contracts and membership publication.

use crate::table::statement;
use crate::{Error, Json, Result, RoutePagePartition, SqlBatch, SqlResultSet, SqlValue};
use crate::{GlobalIndexPartitionSpec, GlobalIndexSplitPlan, PartitionSpec, SplitPlan};
use crab_cell_runtime::registry::{Command, CommandContext, CommandResult, Query, QueryContext};
use serde::{Deserialize, Serialize};

/// Address a range participant within the directory leaf selected by its logical bound.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DirectoryPartitionInput {
    pub table_id: String,
    pub partition_id: [u8; 16],
}

fn visible(sql: impl FnOnce(&SqlBatch) -> Result<Vec<SqlResultSet>>, table: &str) -> Result<bool> {
    Ok(crate::directory::state(sql)?.is_some_and(|state| {
        state.spec.table_id == table
            && matches!(
                state.mode,
                crate::DirectoryMode::Leaf | crate::DirectoryMode::Frozen(_)
            )
    }))
}

/// Read one range from its current metadata leaf.
pub struct ReadDirectoryRange;
impl Query for ReadDirectoryRange {
    const MODULE: &'static str = crate::directory::MODULE;
    const ID: u32 = 4;
    const CODEC_VERSION: u32 = 2;
    type Input = Json<DirectoryPartitionInput>;
    type Output = Json<Option<RoutePagePartition>>;
    fn execute(context: &mut QueryContext<'_>, Json(input): Self::Input) -> Result<Self::Output> {
        if !visible(|batch| context.sql(batch), &input.table_id)? {
            return Ok(Json(None));
        }
        let rows = context.sql(&statement(
            "SELECT record FROM ddb_directory_ranges WHERE partition_id = ?1",
            vec![SqlValue::Blob(input.partition_id.to_vec())],
        ))?;
        match rows[0].rows.first().map(Vec::as_slice) {
            None => Ok(Json(None)),
            Some([SqlValue::Blob(bytes)]) => Ok(Json(Some(serde_json::from_slice(bytes)?))),
            _ => Err(Error::Command("invalid directory member")),
        }
    }
}

fn read_plan(
    sql: impl Fn(&SqlBatch) -> Result<Vec<SqlResultSet>>,
    input: &DirectoryPartitionInput,
) -> Result<Option<DirectoryTransfer>> {
    if !visible(&sql, &input.table_id)? {
        return Ok(None);
    }
    let rows = sql(&statement(
        "SELECT p.plan FROM ddb_directory_members m JOIN ddb_directory_transfers p ON p.lower_bound = m.lower_bound WHERE m.partition_id = ?1",
        vec![SqlValue::Blob(input.partition_id.to_vec())],
    ))?;
    match rows[0].rows.first().map(Vec::as_slice) {
        None => Ok(None),
        Some([SqlValue::Blob(bytes)]) => Ok(Some(serde_json::from_slice(bytes)?)),
        _ => Err(Error::Command("invalid directory transfer")),
    }
}

/// Discover an unfinished transfer through its source or either child in one leaf.
pub struct ReadDirectoryTransfer;
impl Query for ReadDirectoryTransfer {
    const MODULE: &'static str = crate::directory::MODULE;
    const ID: u32 = 5;
    const CODEC_VERSION: u32 = 2;
    type Input = Json<DirectoryPartitionInput>;
    type Output = Json<Option<DirectoryTransfer>>;
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

/// Full immutable contract retained with a leaf's compact participant reservation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum DirectoryTransfer {
    Base(Box<SplitPlan>),
    GlobalIndex(Box<GlobalIndexSplitPlan>),
}

impl From<SplitPlan> for DirectoryTransfer {
    fn from(plan: SplitPlan) -> Self {
        Self::Base(Box::new(plan))
    }
}

impl From<GlobalIndexSplitPlan> for DirectoryTransfer {
    fn from(plan: GlobalIndexSplitPlan) -> Self {
        Self::GlobalIndex(Box::new(plan))
    }
}

impl TryFrom<DirectoryTransfer> for GlobalIndexSplitPlan {
    type Error = Error;
    fn try_from(plan: DirectoryTransfer) -> Result<Self> {
        match plan {
            DirectoryTransfer::GlobalIndex(plan) => Ok(*plan),
            DirectoryTransfer::Base(_) => Err(Error::Command("expected index transfer")),
        }
    }
}

impl SplitPlan {
    pub(crate) fn directory_change(&self) -> crate::DirectoryChange {
        let range = |spec: &PartitionSpec| RoutePagePartition {
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

impl DirectoryTransfer {
    fn table_id(&self) -> &str {
        match self {
            Self::Base(plan) => &plan.source.table.id,
            Self::GlobalIndex(plan) => &plan.source.index.id,
        }
    }

    fn valid(&self) -> bool {
        match self {
            Self::Base(plan) => plan.valid_for(&plan.source.table),
            Self::GlobalIndex(plan) => plan.valid(),
        }
    }

    fn directory_change(&self) -> crate::DirectoryChange {
        match self {
            Self::Base(plan) => plan.directory_change(),
            Self::GlobalIndex(plan) => plan.directory_change(),
        }
    }

    fn source_input(&self) -> DirectoryPartitionInput {
        DirectoryPartitionInput {
            table_id: self.table_id().into(),
            partition_id: self.directory_change().source.partition_id,
        }
    }
}

/// Reserve the exact transfer in the leaf that owns its source interval.
pub struct BeginDirectoryTransfer;
impl Command for BeginDirectoryTransfer {
    const MODULE: &'static str = crate::directory::MODULE;
    const ID: u32 = 10;
    const CODEC_VERSION: u32 = 2;
    type Input = Json<DirectoryTransfer>;
    type Output = Json<bool>;
    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(plan): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        if !plan.valid() || !visible(|batch| context.sql(batch), plan.table_id())? {
            return Ok(CommandResult::Rejected(Json(false)));
        }
        if let Some(existing) = read_plan(|batch| context.sql(batch), &plan.source_input())? {
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
                "INSERT INTO ddb_directory_transfers VALUES (?1, ?2)",
                vec![
                    SqlValue::Blob(plan.directory_change().source.lower.to_vec()),
                    SqlValue::Blob(serde_json::to_vec(&plan)?),
                ],
            ))?;
        }
        Ok(result)
    }
}

/// Replace the reserved source after the controller verifies both durable copies.
pub struct PublishDirectoryTransfer;
impl Command for PublishDirectoryTransfer {
    const MODULE: &'static str = crate::directory::MODULE;
    const ID: u32 = 11;
    const CODEC_VERSION: u32 = 2;
    type Input = Json<DirectoryTransfer>;
    type Output = Json<bool>;
    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(plan): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        if read_plan(|batch| context.sql(batch), &plan.source_input())?.as_ref() != Some(&plan) {
            return Ok(CommandResult::Rejected(Json(false)));
        }
        crate::PublishDirectoryChange::execute(context, Json(plan.directory_change()))
    }
}

/// Remove the full transfer and participant reservations after both children open.
pub struct FinishDirectoryTransfer;
impl Command for FinishDirectoryTransfer {
    const MODULE: &'static str = crate::directory::MODULE;
    const ID: u32 = 12;
    const CODEC_VERSION: u32 = 2;
    type Input = Json<DirectoryTransfer>;
    type Output = Json<bool>;
    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(plan): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        if !visible(|batch| context.sql(batch), plan.table_id())?
            || read_plan(|batch| context.sql(batch), &plan.source_input())?
                .as_ref()
                .is_some_and(|existing| existing != &plan)
        {
            return Ok(CommandResult::Rejected(Json(false)));
        }
        crate::FinishDirectoryChange::execute(context, Json(plan.directory_change()))
    }
}
