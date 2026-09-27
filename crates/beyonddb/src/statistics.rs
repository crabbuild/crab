//! Logical item statistics and generation-checked table snapshots.

use std::collections::{BTreeMap, BTreeSet};

use crab_cell_runtime::registry::{Command, CommandContext, CommandResult, Query, QueryContext};
use extenddb_core::types::Item;
use serde::{Deserialize, Serialize};

use crate::table::statement;
use crate::{Error, Json, Result, SqlValue};

pub(crate) fn item_bytes(item: &Item) -> Result<i64> {
    i64::try_from(extenddb_core::types::item_size_bytes(item))
        .map_err(|_| Error::Command("item statistics size overflow"))
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ItemStatistics {
    pub count: i64,
    pub bytes: i64,
}

impl ItemStatistics {
    pub fn add(&mut self, other: &Self) -> Result<()> {
        self.count = self
            .count
            .checked_add(other.count)
            .ok_or(Error::Command("item count overflow"))?;
        self.bytes = self
            .bytes
            .checked_add(other.bytes)
            .ok_or(Error::Command("item bytes overflow"))?;
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TableStatistics {
    pub base: ItemStatistics,
    pub local: BTreeMap<String, ItemStatistics>,
    pub global: BTreeMap<String, ItemStatistics>,
}

fn decode_pair(row: &[SqlValue]) -> Result<ItemStatistics> {
    match row {
        [SqlValue::Integer(count), SqlValue::Integer(bytes)] if *count >= 0 && *bytes >= 0 => {
            Ok(ItemStatistics {
                count: *count,
                bytes: *bytes,
            })
        }
        _ => Err(Error::Command("invalid item statistics")),
    }
}

fn local_statistics(context: &QueryContext<'_>, table_id: &str) -> Result<TableStatistics> {
    let rows = context.sql(&statement(
        "SELECT index_name, item_count, item_bytes FROM ddb_local_index_statistics WHERE table_id = ?1",
        vec![SqlValue::Text(table_id.into())],
    ))?;
    let mut statistics = TableStatistics::default();
    for row in &rows[0].rows {
        let Some(SqlValue::Text(name)) = row.first() else {
            return Err(Error::Command("invalid statistics index name"));
        };
        let values = decode_pair(&row[1..])?;
        if name.is_empty() {
            statistics.base = values;
        } else {
            statistics.local.insert(name.clone(), values);
        }
    }
    Ok(statistics)
}

pub(crate) struct ReadAccountStatistics;
impl Query for ReadAccountStatistics {
    const MODULE: &'static str = crate::MODULE;
    const ID: u32 = 36;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<String>;
    type Output = Json<TableStatistics>;
    fn execute(
        context: &mut QueryContext<'_>,
        Json(table_id): Self::Input,
    ) -> Result<Self::Output> {
        Ok(Json(local_statistics(context, &table_id)?))
    }
}

pub(crate) struct ReadPartitionStatistics;
impl Query for ReadPartitionStatistics {
    const MODULE: &'static str = crate::DATA_MODULE;
    const ID: u32 = 14;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<String>;
    type Output = Json<TableStatistics>;
    fn execute(
        context: &mut QueryContext<'_>,
        Json(table_id): Self::Input,
    ) -> Result<Self::Output> {
        let mut statistics = local_statistics(context, &table_id)?;
        let rows = context.sql(&statement(
            "SELECT item_count, logical_bytes FROM ddb_partition_usage WHERE singleton = 1",
            vec![],
        ))?;
        statistics.base = decode_pair(
            rows[0]
                .rows
                .first()
                .ok_or(Error::Command("missing partition statistics"))?,
        )?;
        Ok(Json(statistics))
    }
}

pub(crate) struct ReadGlobalIndexStatistics;
impl Query for ReadGlobalIndexStatistics {
    const MODULE: &'static str = crate::global_index::MODULE;
    const ID: u32 = 7;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<()>;
    type Output = Json<ItemStatistics>;
    fn execute(context: &mut QueryContext<'_>, _: Self::Input) -> Result<Self::Output> {
        let rows = context.sql(&statement(
            "SELECT item_count, item_bytes FROM ddb_global_index_statistics WHERE singleton = 1",
            vec![],
        ))?;
        Ok(Json(decode_pair(
            rows[0]
                .rows
                .first()
                .ok_or(Error::Command("missing index statistics"))?,
        )?))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct StatisticsSnapshot {
    pub table_id: String,
    pub sampled_at: i64,
    pub base_epoch: Option<u64>,
    pub index_generations: BTreeSet<String>,
    pub statistics: TableStatistics,
}

pub(crate) struct PublishStatistics;
impl Command for PublishStatistics {
    const MODULE: &'static str = crate::MODULE;
    const ID: u32 = 31;
    const CODEC_VERSION: u32 = 2;
    type Input = Json<StatisticsSnapshot>;
    type Output = Json<bool>;
    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(input): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        let rows = context.sql(&statement(
            "SELECT record FROM ddb_live_tables WHERE table_id = ?1",
            vec![SqlValue::Text(input.table_id.clone())],
        ))?;
        let Some(table) = crate::table::decode_table(&rows[0])? else {
            return Ok(CommandResult::Rejected(Json(false)));
        };
        let rows = context.sql(&statement(
            "SELECT route_epoch FROM ddb_routes WHERE table_id = ?1",
            vec![SqlValue::Text(input.table_id.clone())],
        ))?;
        let epoch = rows[0]
            .rows
            .first()
            .map(|row| parse_epoch(row))
            .transpose()?;
        // The sample follows one directory generation. A cutover invalidates
        // the whole sample, preventing old-source and child double counting.
        if epoch != input.base_epoch
            || table.global_secondary_indexes.len() != input.index_generations.len()
        {
            return Ok(CommandResult::Rejected(Json(false)));
        }
        for index in &table.global_secondary_indexes {
            let rows = context.sql(&statement(
                "SELECT 1 FROM ddb_global_index_routes WHERE table_id = ?1 AND retired = 0 AND initial_fingerprint IS NOT NULL",
                vec![SqlValue::Text(index.id.clone())],
            ))?;
            // Index samples validate membership in each directory leaf and cross
            // disjoint immutable intervals. This commit checks the live index
            // generations; it cannot validate remote leaf versions atomically.
            if rows[0].rows.is_empty() || !input.index_generations.contains(&index.id) {
                return Ok(CommandResult::Rejected(Json(false)));
            }
        }
        context.sql(&statement(
            "INSERT INTO ddb_table_statistics (table_id, sampled_at, statistics) VALUES (?1, ?2, ?3) \
             ON CONFLICT(table_id) DO UPDATE SET sampled_at = excluded.sampled_at, statistics = excluded.statistics \
             WHERE excluded.sampled_at >= ddb_table_statistics.sampled_at",
            vec![SqlValue::Text(input.table_id), SqlValue::Integer(input.sampled_at), SqlValue::Blob(serde_json::to_vec(&input.statistics)?)],
        ))?;
        Ok(CommandResult::Success(Json(true)))
    }
}

fn parse_epoch(row: &[SqlValue]) -> Result<u64> {
    let [SqlValue::Text(epoch)] = row else {
        return Err(Error::Command("invalid statistics route epoch"));
    };
    epoch
        .parse()
        .map_err(|_| Error::Command("invalid statistics route epoch"))
}

pub(crate) struct ReadTableStatistics;
impl Query for ReadTableStatistics {
    const MODULE: &'static str = crate::MODULE;
    const ID: u32 = 37;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<String>;
    type Output = Json<TableStatistics>;
    fn execute(
        context: &mut QueryContext<'_>,
        Json(table_id): Self::Input,
    ) -> Result<Self::Output> {
        let rows = context.sql(&statement(
            "SELECT statistics FROM ddb_table_statistics WHERE table_id = ?1",
            vec![SqlValue::Text(table_id)],
        ))?;
        match rows[0].rows.first().map(Vec::as_slice) {
            None => Ok(Json(TableStatistics::default())),
            Some([SqlValue::Blob(bytes)]) => Ok(Json(serde_json::from_slice(bytes)?)),
            _ => Err(Error::Command("invalid table statistics snapshot")),
        }
    }
}
