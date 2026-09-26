//! Indexed expiry candidates and resumable TTL backfill within one data Cell.

use crab_cell_runtime::primitives::sql::SqlResultSet;
use crab_cell_runtime::registry::{Command, CommandContext, CommandResult, Query, QueryContext};
use extenddb_core::types::{AttributeValue, Item};
use serde::{Deserialize, Serialize};

use super::{
    AccessState, DATA_MODULE, Json, Result, SqlValue, command_access, decode_spec, query_access,
};
use crate::table::statement;
use crate::{Error, ttl::valid_attribute};

#[derive(Clone, Debug, PartialEq)]
struct TtlState {
    attribute: Option<String>,
    generation: i64,
    cursor: Option<Vec<u8>>,
    ready: bool,
}

fn decode_state(rows: &SqlResultSet) -> Result<Option<TtlState>> {
    let Some(row) = rows.rows.first() else {
        return Ok(None);
    };
    let [
        attribute,
        SqlValue::Integer(generation),
        cursor,
        SqlValue::Integer(ready),
    ] = row.as_slice()
    else {
        return Err(Error::Command("invalid partition TTL state"));
    };
    let attribute = match attribute {
        SqlValue::Text(attribute) => Some(attribute.clone()),
        SqlValue::Null => None,
        _ => return Err(Error::Command("invalid partition TTL attribute")),
    };
    let cursor = match cursor {
        SqlValue::Blob(cursor) => Some(cursor.clone()),
        SqlValue::Null => None,
        _ => return Err(Error::Command("invalid partition TTL cursor")),
    };
    if *generation < 1 || !matches!(ready, 0 | 1) {
        return Err(Error::Command("invalid partition TTL generation"));
    }
    Ok(Some(TtlState {
        attribute,
        generation: *generation,
        cursor,
        ready: *ready == 1,
    }))
}

fn state_sql() -> &'static str {
    "SELECT attribute_name, generation, cursor, ready FROM ddb_partition_ttl WHERE singleton = 1"
}

fn item_epoch(item: &Item, attribute: Option<&str>) -> Option<i64> {
    let AttributeValue::N(value) = item.get(attribute?)? else {
        return None;
    };
    value.parse::<i64>().ok().filter(|epoch| *epoch > 0)
}

pub(super) fn write_values(
    context: &mut CommandContext<'_, '_>,
    item: &Item,
) -> Result<(SqlValue, SqlValue)> {
    let rows = context.sql(&statement(state_sql(), vec![]))?;
    let state = decode_state(&rows[0])?;
    let generation = state.as_ref().map_or(0, |state| state.generation);
    let epoch = state
        .as_ref()
        .and_then(|state| item_epoch(item, state.attribute.as_deref()));
    Ok((
        SqlValue::Integer(generation),
        epoch.map_or(SqlValue::Null, SqlValue::Integer),
    ))
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConfigurePartitionTtlInput {
    pub table_id: String,
    pub epoch: u64,
    pub attribute_name: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ConfigurePartitionTtlOutcome {
    Configured { ready: bool },
    NotInstalled,
    StaleRoute,
    NotReady,
    InvalidAttribute,
}

/// Set the indexed TTL generation without rewriting the existing item set.
pub struct ConfigurePartitionTtl;

impl Command for ConfigurePartitionTtl {
    const MODULE: &'static str = DATA_MODULE;
    const ID: u32 = 10;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<ConfigurePartitionTtlInput>;
    type Output = Json<ConfigurePartitionTtlOutcome>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(input): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        let rows = context.sql(&statement(
            "SELECT spec FROM ddb_partition WHERE singleton = 1",
            vec![],
        ))?;
        let Some(spec) = decode_spec(&rows[0])? else {
            return Ok(CommandResult::Rejected(Json(
                ConfigurePartitionTtlOutcome::NotInstalled,
            )));
        };
        if spec.table.id != input.table_id || spec.epoch != input.epoch {
            return Ok(CommandResult::Rejected(Json(
                ConfigurePartitionTtlOutcome::StaleRoute,
            )));
        }
        if !matches!(command_access(context)?, AccessState::Serving) {
            return Ok(CommandResult::Rejected(Json(
                ConfigurePartitionTtlOutcome::NotReady,
            )));
        }
        if input
            .attribute_name
            .as_ref()
            .is_some_and(|name| !valid_attribute(name))
        {
            return Ok(CommandResult::Rejected(Json(
                ConfigurePartitionTtlOutcome::InvalidAttribute,
            )));
        }
        let rows = context.sql(&statement(state_sql(), vec![]))?;
        let current = decode_state(&rows[0])?;
        if let Some(state) = &current
            && state.attribute == input.attribute_name
        {
            return Ok(CommandResult::Success(Json(
                ConfigurePartitionTtlOutcome::Configured { ready: state.ready },
            )));
        }
        let generation = current.map_or(1, |state| state.generation.saturating_add(1));
        if generation == i64::MAX {
            return Err(Error::Command("partition TTL generation exhausted"));
        }
        let ready = input.attribute_name.is_none();
        context.sql(&statement(
            "INSERT INTO ddb_partition_ttl (singleton, attribute_name, generation, cursor, ready) \
             VALUES (1, ?1, ?2, NULL, ?3) ON CONFLICT(singleton) DO UPDATE SET \
             attribute_name = excluded.attribute_name, generation = excluded.generation, \
             cursor = NULL, ready = excluded.ready",
            vec![
                input.attribute_name.map_or(SqlValue::Null, SqlValue::Text),
                SqlValue::Integer(generation),
                SqlValue::Integer(i64::from(ready)),
            ],
        ))?;
        Ok(CommandResult::Success(Json(
            ConfigurePartitionTtlOutcome::Configured { ready },
        )))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PartitionTtlInput {
    pub table_id: String,
    pub epoch: u64,
    pub attribute_name: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ReadPartitionTtlInput {
    pub table_id: String,
    pub epoch: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ReadPartitionTtlOutcome {
    State {
        attribute_name: Option<String>,
        ready: bool,
    },
    Missing,
    StaleRoute,
    NotReady,
}

/// Inspect a serving partition's TTL index without advancing its Cell log.
pub struct ReadPartitionTtl;

impl Query for ReadPartitionTtl {
    const MODULE: &'static str = DATA_MODULE;
    const ID: u32 = 9;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<ReadPartitionTtlInput>;
    type Output = Json<ReadPartitionTtlOutcome>;

    fn execute(context: &mut QueryContext<'_>, Json(input): Self::Input) -> Result<Self::Output> {
        let rows = context.sql(&statement(
            "SELECT spec FROM ddb_partition WHERE singleton = 1",
            vec![],
        ))?;
        let Some(spec) = decode_spec(&rows[0])? else {
            return Ok(Json(ReadPartitionTtlOutcome::StaleRoute));
        };
        if spec.table.id != input.table_id || spec.epoch != input.epoch {
            return Ok(Json(ReadPartitionTtlOutcome::StaleRoute));
        }
        if !matches!(query_access(context)?, AccessState::Serving) {
            return Ok(Json(ReadPartitionTtlOutcome::NotReady));
        }
        let rows = context.sql(&statement(state_sql(), vec![]))?;
        match decode_state(&rows[0])? {
            Some(state) => Ok(Json(ReadPartitionTtlOutcome::State {
                attribute_name: state.attribute,
                ready: state.ready,
            })),
            None => Ok(Json(ReadPartitionTtlOutcome::Missing)),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum BackfillPartitionTtlOutcome {
    Ready,
    Progress,
    StaleRoute,
    NotReady,
}

/// Fill the stable expiry index in bounded, resumable Cell commands.
pub struct BackfillPartitionTtl;

impl Command for BackfillPartitionTtl {
    const MODULE: &'static str = DATA_MODULE;
    const ID: u32 = 11;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<PartitionTtlInput>;
    type Output = Json<BackfillPartitionTtlOutcome>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(input): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        let rows = context.sql(&statement(
            "SELECT spec FROM ddb_partition WHERE singleton = 1",
            vec![],
        ))?;
        let Some(spec) = decode_spec(&rows[0])? else {
            return Ok(CommandResult::Rejected(Json(
                BackfillPartitionTtlOutcome::StaleRoute,
            )));
        };
        if spec.table.id != input.table_id || spec.epoch != input.epoch {
            return Ok(CommandResult::Rejected(Json(
                BackfillPartitionTtlOutcome::StaleRoute,
            )));
        }
        if !matches!(command_access(context)?, AccessState::Serving) {
            return Ok(CommandResult::Rejected(Json(
                BackfillPartitionTtlOutcome::NotReady,
            )));
        }
        let rows = context.sql(&statement(state_sql(), vec![]))?;
        let Some(mut state) = decode_state(&rows[0])? else {
            return Ok(CommandResult::Rejected(Json(
                BackfillPartitionTtlOutcome::NotReady,
            )));
        };
        if state.attribute.as_deref() != Some(&input.attribute_name) {
            return Ok(CommandResult::Rejected(Json(
                BackfillPartitionTtlOutcome::NotReady,
            )));
        }
        if state.ready {
            return Ok(CommandResult::Success(Json(
                BackfillPartitionTtlOutcome::Ready,
            )));
        }
        for _ in 0..32 {
            let rows = context.sql(&statement(
                "SELECT item_key FROM ddb_partition_items \
                 WHERE (?1 IS NULL OR item_key > ?1) ORDER BY item_key LIMIT 2",
                vec![state.cursor.clone().map_or(SqlValue::Null, SqlValue::Blob)],
            ))?;
            if rows[0].rows.is_empty() {
                context.sql(&statement(
                    "UPDATE ddb_partition_ttl SET cursor = NULL, ready = 1 WHERE singleton = 1",
                    vec![],
                ))?;
                return Ok(CommandResult::Success(Json(
                    BackfillPartitionTtlOutcome::Ready,
                )));
            }
            for row in &rows[0].rows {
                let [SqlValue::Blob(key)] = row.as_slice() else {
                    return Err(Error::Command("invalid TTL backfill item"));
                };
                let item = crate::item_storage::StoredItem::Partition(key)
                    .read(|batch| context.sql(batch))?
                    .ok_or(Error::Command("TTL key has no item"))?;
                let epoch = item_epoch(&item, state.attribute.as_deref());
                context.sql(&statement(
                    "UPDATE ddb_partition_items SET ttl_generation = ?1, ttl_epoch = ?2 \
                     WHERE item_key = ?3",
                    vec![
                        SqlValue::Integer(state.generation),
                        epoch.map_or(SqlValue::Null, SqlValue::Integer),
                        SqlValue::Blob(key.clone()),
                    ],
                ))?;
                state.cursor = Some(key.clone());
            }
        }
        context.sql(&statement(
            "UPDATE ddb_partition_ttl SET cursor = ?1 WHERE singleton = 1",
            vec![state.cursor.map_or(SqlValue::Null, SqlValue::Blob)],
        ))?;
        Ok(CommandResult::Success(Json(
            BackfillPartitionTtlOutcome::Progress,
        )))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExpiredPartitionInput {
    pub table_id: String,
    pub epoch: u64,
    pub attribute_name: String,
    pub cutoff_epoch: i64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ExpiredPartitionOutcome {
    Items(Vec<Item>),
    StaleRoute,
    NotReady,
}

/// Read two indexed expiry candidates from one serving partition.
pub struct ReadExpiredPartition;

impl Query for ReadExpiredPartition {
    const MODULE: &'static str = DATA_MODULE;
    const ID: u32 = 8;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<ExpiredPartitionInput>;
    type Output = Json<ExpiredPartitionOutcome>;

    fn execute(context: &mut QueryContext<'_>, Json(input): Self::Input) -> Result<Self::Output> {
        let rows = context.sql(&statement(
            "SELECT spec FROM ddb_partition WHERE singleton = 1",
            vec![],
        ))?;
        let Some(spec) = decode_spec(&rows[0])? else {
            return Ok(Json(ExpiredPartitionOutcome::StaleRoute));
        };
        if spec.table.id != input.table_id || spec.epoch != input.epoch {
            return Ok(Json(ExpiredPartitionOutcome::StaleRoute));
        }
        if !matches!(query_access(context)?, AccessState::Serving) {
            return Ok(Json(ExpiredPartitionOutcome::NotReady));
        }
        let rows = context.sql(&statement(state_sql(), vec![]))?;
        let Some(state) = decode_state(&rows[0])? else {
            return Ok(Json(ExpiredPartitionOutcome::NotReady));
        };
        if !state.ready || state.attribute.as_deref() != Some(&input.attribute_name) {
            return Ok(Json(ExpiredPartitionOutcome::NotReady));
        }
        // Shared and exclusive locks both prevent deletion. Excluding them
        // before LIMIT lets later expired items progress while a decision is
        // pending; the delete command still fences locks acquired after this read.
        let rows = context.sql(&statement(
            "SELECT i.item_key FROM ddb_partition_items i WHERE i.ttl_generation = ?1 \
             AND i.ttl_epoch BETWEEN 1 AND ?2 AND NOT EXISTS \
             (SELECT 1 FROM ddb_partition_transaction_locks l WHERE l.item_key = i.item_key) \
             ORDER BY i.ttl_epoch, i.item_key LIMIT 2",
            vec![
                SqlValue::Integer(state.generation),
                SqlValue::Integer(input.cutoff_epoch),
            ],
        ))?;
        let mut items = Vec::with_capacity(rows[0].rows.len());
        let mut bytes = 0;
        for row in &rows[0].rows {
            let [SqlValue::Blob(key)] = row.as_slice() else {
                return Err(Error::Command("invalid expired item row"));
            };
            let item = crate::item_storage::StoredItem::Partition(key)
                .read(|batch| context.sql(batch))?
                .ok_or(Error::Command("expired key has no item"))?;
            let encoded = serde_json::to_vec(&item)?.len();
            if !items.is_empty() && bytes + encoded > 900_000 {
                break;
            }
            bytes += encoded;
            items.push(item);
        }
        Ok(Json(ExpiredPartitionOutcome::Items(items)))
    }
}
