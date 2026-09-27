//! Account-owned discovery of independently partitioned global indexes.

use crab_cell_runtime::registry::{Command, CommandContext, CommandResult, Query, QueryContext};
use serde::{Deserialize, Serialize};

use super::GlobalIndexRecord;
use crate::table::{decode_table, statement};
use crate::{
    Json, Result, RoutePageInput, RoutePageOutcome, RoutePagePartition, SqlValue, TableRecord,
};

/// Initial index directory published after all of its ranges are installed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GlobalIndexRoute {
    pub table: TableRecord,
    pub index: GlobalIndexRecord,
    pub partitions: Vec<RoutePagePartition>,
}

/// Publish the first contiguous directory of one global-index generation.
pub struct ActivateGlobalIndexRoute;

impl Command for ActivateGlobalIndexRoute {
    const MODULE: &'static str = crate::MODULE;
    const ID: u32 = 25;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<GlobalIndexRoute>;
    type Output = Json<bool>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(input): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        let rows = context.sql(&statement(
            "SELECT record FROM ddb_live_tables WHERE table_id = ?1",
            vec![SqlValue::Text(input.table.id.clone())],
        ))?;
        let Some(table) = decode_table(&rows[0])? else {
            return Ok(CommandResult::Rejected(Json(false)));
        };
        if table != input.table
            || !table.global_secondary_indexes.contains(&input.index)
            || input.partitions.is_empty()
        {
            return Ok(CommandResult::Rejected(Json(false)));
        }
        let mut lower = Some([0; 16]);
        let mut ids = std::collections::HashSet::new();
        for range in &input.partitions {
            if range.epoch != 1
                || lower != Some(range.lower)
                || range.upper.is_some_and(|upper| upper <= range.lower)
                || !ids.insert(range.partition_id)
            {
                return Ok(CommandResult::Rejected(Json(false)));
            }
            lower = range.upper;
        }
        if lower.is_some() {
            return Ok(CommandResult::Rejected(Json(false)));
        }
        let existing = context.sql(&statement(
            "SELECT route_epoch FROM ddb_global_index_routes WHERE table_id = ?1",
            vec![SqlValue::Text(input.index.id.clone())],
        ))?;
        if !existing[0].rows.is_empty() {
            let matches = crate::routing::route_partitions_match(
                context,
                "ddb_global_index_partitions",
                &input.index.id,
                input.partitions.into_iter(),
            )?;
            return Ok(if matches {
                CommandResult::Success(Json(true))
            } else {
                CommandResult::Rejected(Json(false))
            });
        }
        context.sql(&statement("INSERT INTO ddb_global_index_routes (table_id, base_table_id, route_epoch) VALUES (?1, ?2, '1')", vec![SqlValue::Text(input.index.id.clone()), SqlValue::Text(table.id)]))?;
        for range in input.partitions {
            context.sql(&statement("INSERT INTO ddb_global_index_partitions (table_id, partition_id, lower_bound, upper_bound, epoch) VALUES (?1, ?2, ?3, ?4, '1')", vec![
                SqlValue::Text(input.index.id.clone()), SqlValue::Blob(range.partition_id.to_vec()),
                SqlValue::Blob(range.lower.to_vec()),
                SqlValue::Blob(range.upper.map_or_else(|| vec![0xff; 17], |upper| upper.to_vec())),
            ]))?;
        }
        Ok(CommandResult::Success(Json(true)))
    }
}

/// Page an index directory with the same epoch and coverage checks as base routes.
pub struct ReadGlobalIndexRoutePage;

impl Query for ReadGlobalIndexRoutePage {
    const MODULE: &'static str = crate::MODULE;
    const ID: u32 = 29;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<RoutePageInput>;
    type Output = Json<RoutePageOutcome>;

    fn execute(context: &mut QueryContext<'_>, Json(input): Self::Input) -> Result<Self::Output> {
        crate::routing::read_route_page(
            context,
            input,
            "ddb_global_index_routes",
            "ddb_global_index_partitions",
        )
    }
}
