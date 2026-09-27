//! Account generation anchors and traversal of independently owned index directories.

use crab_cell_runtime::registry::{Command, CommandContext, CommandResult};
use serde::{Deserialize, Serialize};

use super::GlobalIndexRecord;
use crate::table::{decode_table, statement};
use crate::{Json, Result, RoutePagePartition, SqlValue, TableRecord};

/// Initial index directory published after all of its ranges are installed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GlobalIndexRoute {
    pub table: TableRecord,
    pub index: GlobalIndexRecord,
    pub partitions: Vec<RoutePagePartition>,
    pub receipt: crate::DirectoryCopyReceipt,
}

/// Publish the first contiguous directory of one global-index generation.
pub struct ActivateGlobalIndexRoute;

impl Command for ActivateGlobalIndexRoute {
    const MODULE: &'static str = crate::MODULE;
    const ID: u32 = 25;
    const CODEC_VERSION: u32 = 2;
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
        let spec = crate::DirectorySpec::root(input.index.id.clone());
        let target = crate::directory::target_for_tenant(context.target().tenant(), &spec)?;
        let fingerprint = crate::directory::fingerprint(&spec, &input.partitions)?;
        if input.receipt.cell_id != *target.cell_id().as_bytes()
            || input.receipt.sequence == 0
            || input.receipt.sequence > i64::MAX as u64
            || input.receipt.fingerprint != fingerprint
        {
            return Ok(CommandResult::Rejected(Json(false)));
        }
        let existing = context.sql(&statement(
            "SELECT initial_fingerprint FROM ddb_directory_roots WHERE table_id = ?1",
            vec![SqlValue::Text(input.index.id.clone())],
        ))?;
        match existing[0].rows.first().map(Vec::as_slice) {
            Some([SqlValue::Null]) => {}
            Some([SqlValue::Blob(previous)]) if previous.as_slice() == fingerprint => {
                return Ok(CommandResult::Success(Json(true)));
            }
            _ => return Ok(CommandResult::Rejected(Json(false))),
        }
        context.sql(&statement("UPDATE ddb_directory_roots SET initial_fingerprint = ?3 WHERE table_id = ?1 AND base_table_id = ?2", vec![SqlValue::Text(input.index.id), SqlValue::Text(table.id), SqlValue::Blob(fingerprint.to_vec())]))?;
        Ok(CommandResult::Success(Json(true)))
    }
}
