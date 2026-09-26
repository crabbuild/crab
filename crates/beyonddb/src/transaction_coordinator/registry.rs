//! Account-owned discovery of coordinator shards with published transactions.

use crab_cell_runtime::registry::{Command, CommandContext, CommandResult, Query, QueryContext};
use serde::{Deserialize, Serialize};

use super::SHARDS;
use crate::table::statement;
use crate::{Error, Json, MODULE, Result, SqlValue, account_target};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RegisterCoordinatorShardInput {
    pub account_id: String,
    pub shard: u32,
}

/// Publish shard discoverability before writing its first coordinator record.
pub struct RegisterCoordinatorShard;

impl Command for RegisterCoordinatorShard {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 20;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<RegisterCoordinatorShardInput>;
    type Output = Json<()>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(input): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        if account_target(&input.account_id)? != *context.target() {
            return Err(Error::Identity(
                "coordinator shard reached the wrong account",
            ));
        }
        if input.shard >= SHARDS {
            return Err(Error::Command("invalid coordinator shard"));
        }
        context.sql(&statement(
            "INSERT OR IGNORE INTO ddb_coordinator_shards (shard) VALUES (?1)",
            vec![SqlValue::Integer(i64::from(input.shard))],
        ))?;
        Ok(CommandResult::Success(Json(())))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ListCoordinatorShardsInput {
    pub account_id: String,
    pub after: Option<u32>,
    pub limit: u8,
}

/// Page registered coordinator shard numbers from one account Cell.
pub struct ListCoordinatorShards;

impl Query for ListCoordinatorShards {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 24;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<ListCoordinatorShardsInput>;
    type Output = Json<Vec<u32>>;

    fn execute(context: &mut QueryContext<'_>, Json(input): Self::Input) -> Result<Self::Output> {
        if account_target(&input.account_id)?.cell_id() != context.cell_id() {
            return Err(Error::Identity(
                "coordinator listing reached the wrong account",
            ));
        }
        if input.limit == 0 || input.limit > 100 {
            return Err(Error::Command("invalid coordinator shard page limit"));
        }
        let rows = context.sql(&statement(
            "SELECT shard FROM ddb_coordinator_shards WHERE shard > ?1 ORDER BY shard LIMIT ?2",
            vec![
                SqlValue::Integer(input.after.map_or(-1, i64::from)),
                SqlValue::Integer(i64::from(input.limit)),
            ],
        ))?;
        let mut shards = Vec::with_capacity(rows[0].rows.len());
        for row in &rows[0].rows {
            let [SqlValue::Integer(shard)] = row.as_slice() else {
                return Err(Error::Command("invalid coordinator shard row"));
            };
            let shard = u32::try_from(*shard)
                .ok()
                .filter(|shard| *shard < SHARDS)
                .ok_or(Error::Command("invalid coordinator shard"))?;
            shards.push(shard);
        }
        Ok(Json(shards))
    }
}

/// Test shard registration without publishing a receipt on every transaction.
pub struct ReadCoordinatorRegistration;

impl Query for ReadCoordinatorRegistration {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 26;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<RegisterCoordinatorShardInput>;
    type Output = Json<bool>;

    fn execute(context: &mut QueryContext<'_>, Json(input): Self::Input) -> Result<Self::Output> {
        if account_target(&input.account_id)?.cell_id() != context.cell_id()
            || input.shard >= SHARDS
        {
            return Err(Error::Identity("invalid coordinator registration lookup"));
        }
        let rows = context.sql(&statement(
            "SELECT 1 FROM ddb_coordinator_shards WHERE shard = ?1",
            vec![SqlValue::Integer(i64::from(input.shard))],
        ))?;
        Ok(Json(!rows[0].rows.is_empty()))
    }
}
