//! Account-owned table TTL configuration.

use crab_cell_runtime::registry::{Command, CommandContext, CommandResult, Query, QueryContext};
use serde::{Deserialize, Serialize};

use crate::table::{command_table, query_table, statement};
use crate::{Json, MODULE, Result, SqlValue};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UpdateTtlInput {
    pub table_name: String,
    pub attribute_name: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum UpdateTtlOutcome {
    Updated,
    TableNotFound,
    InvalidAttribute,
}

/// Commit one table's TTL setting in its account Cell.
pub struct UpdateTtl;

impl Command for UpdateTtl {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 17;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<UpdateTtlInput>;
    type Output = Json<UpdateTtlOutcome>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(input): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        let Some(table) = command_table(context, &input.table_name)? else {
            return Ok(CommandResult::Rejected(Json(
                UpdateTtlOutcome::TableNotFound,
            )));
        };
        if input
            .attribute_name
            .as_ref()
            .is_some_and(|name| !valid_attribute(name))
        {
            return Ok(CommandResult::Rejected(Json(
                UpdateTtlOutcome::InvalidAttribute,
            )));
        }
        match input.attribute_name {
            Some(name) => {
                context.sql(&statement(
                    "INSERT INTO ddb_table_ttl (table_id, attribute_name) VALUES (?1, ?2) \
                     ON CONFLICT(table_id) DO UPDATE SET attribute_name = excluded.attribute_name, \
                     sweep_after = NULL",
                    vec![SqlValue::Text(table.id), SqlValue::Text(name)],
                ))?;
            }
            None => {
                context.sql(&statement(
                    "DELETE FROM ddb_table_ttl WHERE table_id = ?1",
                    vec![SqlValue::Text(table.id)],
                ))?;
            }
        }
        Ok(CommandResult::Success(Json(UpdateTtlOutcome::Updated)))
    }
}

pub(crate) fn valid_attribute(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ReadTtlOutcome {
    TableNotFound,
    Disabled,
    Enabled(String),
}

/// Read one table's durable TTL setting.
pub struct ReadTtl;

impl Query for ReadTtl {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 20;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<String>;
    type Output = Json<ReadTtlOutcome>;

    fn execute(context: &mut QueryContext<'_>, Json(name): Self::Input) -> Result<Self::Output> {
        let Some(table) = query_table(context, &name)? else {
            return Ok(Json(ReadTtlOutcome::TableNotFound));
        };
        let rows = context.sql(&statement(
            "SELECT attribute_name FROM ddb_table_ttl WHERE table_id = ?1",
            vec![SqlValue::Text(table.id)],
        ))?;
        match rows[0].rows.first().map(Vec::as_slice) {
            Some([SqlValue::Text(attribute)]) => {
                Ok(Json(ReadTtlOutcome::Enabled(attribute.clone())))
            }
            Some(_) => Err(crate::Error::Command("invalid TTL row")),
            None => Ok(Json(ReadTtlOutcome::Disabled)),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TtlTablePage {
    pub tables: Vec<(String, String)>,
    pub last_evaluated: Option<String>,
}

/// Page through enabled TTL tables in one account.
pub struct ListTtlTables;

impl Query for ListTtlTables {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 21;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<Option<String>>;
    type Output = Json<TtlTablePage>;

    fn execute(context: &mut QueryContext<'_>, Json(after): Self::Input) -> Result<Self::Output> {
        let rows = context.sql(&statement(
            "SELECT t.table_name, ttl.attribute_name FROM ddb_table_ttl ttl \
             JOIN ddb_tables t ON t.table_id = ttl.table_id \
             WHERE (?1 IS NULL OR t.table_name > ?1) ORDER BY t.table_name LIMIT 101",
            vec![after.map_or(SqlValue::Null, SqlValue::Text)],
        ))?;
        let mut tables = Vec::with_capacity(rows[0].rows.len());
        for row in &rows[0].rows {
            let [SqlValue::Text(name), SqlValue::Text(attribute)] = row.as_slice() else {
                return Err(crate::Error::Command("invalid TTL table row"));
            };
            tables.push((name.clone(), attribute.clone()));
        }
        let last_evaluated = if tables.len() > 100 {
            tables.pop();
            tables.last().map(|(name, _)| name.clone())
        } else {
            None
        };
        Ok(Json(TtlTablePage {
            tables,
            last_evaluated,
        }))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TtlSweepState {
    pub table_id: String,
    pub attribute_name: String,
    pub after_lower: Option<[u8; 16]>,
}

fn sweep_state(
    rows: &crab_cell_runtime::primitives::sql::SqlResultSet,
) -> Result<Option<TtlSweepState>> {
    let Some(row) = rows.rows.first() else {
        return Ok(None);
    };
    let [
        SqlValue::Text(table_id),
        SqlValue::Text(attribute_name),
        after,
    ] = row.as_slice()
    else {
        return Err(crate::Error::Command("invalid TTL sweep row"));
    };
    let after_lower = match after {
        SqlValue::Null => None,
        SqlValue::Blob(bytes) => Some(
            bytes
                .as_slice()
                .try_into()
                .map_err(|_| crate::Error::Command("invalid TTL sweep cursor"))?,
        ),
        _ => return Err(crate::Error::Command("invalid TTL sweep cursor")),
    };
    Ok(Some(TtlSweepState {
        table_id: table_id.clone(),
        attribute_name: attribute_name.clone(),
        after_lower,
    }))
}

fn sweep_sql() -> &'static str {
    "SELECT ttl.table_id, ttl.attribute_name, ttl.sweep_after FROM ddb_table_ttl ttl \
     JOIN ddb_tables t ON t.table_id = ttl.table_id WHERE t.table_name = ?1"
}

/// Read one table's durable TTL sweep position.
pub struct ReadTtlSweep;

impl Query for ReadTtlSweep {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 22;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<String>;
    type Output = Json<Option<TtlSweepState>>;

    fn execute(context: &mut QueryContext<'_>, Json(name): Self::Input) -> Result<Self::Output> {
        let rows = context.sql(&statement(sweep_sql(), vec![SqlValue::Text(name)]))?;
        Ok(Json(sweep_state(&rows[0])?))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AdvanceTtlSweepInput {
    pub table_name: String,
    pub table_id: String,
    pub attribute_name: String,
    pub expected_after: Option<[u8; 16]>,
    pub next_after: Option<[u8; 16]>,
}

/// Move one table's sweep position after its route page was processed.
pub struct AdvanceTtlSweep;

impl Command for AdvanceTtlSweep {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 18;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<AdvanceTtlSweepInput>;
    type Output = Json<bool>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(input): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        let rows = context.sql(&statement(
            sweep_sql(),
            vec![SqlValue::Text(input.table_name.clone())],
        ))?;
        let Some(state) = sweep_state(&rows[0])? else {
            return Ok(CommandResult::Rejected(Json(false)));
        };
        if state.table_id != input.table_id
            || state.attribute_name != input.attribute_name
            || state.after_lower != input.expected_after
        {
            return Ok(CommandResult::Rejected(Json(false)));
        }
        context.sql(&statement(
            "UPDATE ddb_table_ttl SET sweep_after = ?1 WHERE table_id = ?2",
            vec![
                input
                    .next_after
                    .map_or(SqlValue::Null, |value| SqlValue::Blob(value.to_vec())),
                SqlValue::Text(input.table_id),
            ],
        ))?;
        Ok(CommandResult::Success(Json(true)))
    }
}

fn schedule_sql() -> &'static str {
    "SELECT last_table FROM ddb_ttl_schedule WHERE singleton = 1"
}

fn schedule_cursor(
    rows: &crab_cell_runtime::primitives::sql::SqlResultSet,
) -> Result<Option<String>> {
    match rows.rows.first().map(Vec::as_slice) {
        Some([SqlValue::Null]) => Ok(None),
        Some([SqlValue::Text(name)]) => Ok(Some(name.clone())),
        _ => Err(crate::Error::Command("invalid TTL schedule cursor")),
    }
}

/// Read the next account table position for bounded TTL sweeps.
pub struct ReadTtlSchedule;

impl Query for ReadTtlSchedule {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 23;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<()>;
    type Output = Json<Option<String>>;

    fn execute(context: &mut QueryContext<'_>, Json(()): Self::Input) -> Result<Self::Output> {
        let rows = context.sql(&statement(schedule_sql(), vec![]))?;
        Ok(Json(schedule_cursor(&rows[0])?))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AdvanceTtlScheduleInput {
    pub expected_last: Option<String>,
    pub next_last: Option<String>,
}

/// Advance the account's table sweep position after its bounded batch.
pub struct AdvanceTtlSchedule;

impl Command for AdvanceTtlSchedule {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 19;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<AdvanceTtlScheduleInput>;
    type Output = Json<bool>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(input): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        let rows = context.sql(&statement(schedule_sql(), vec![]))?;
        if schedule_cursor(&rows[0])? != input.expected_last {
            return Ok(CommandResult::Rejected(Json(false)));
        }
        context.sql(&statement(
            "UPDATE ddb_ttl_schedule SET last_table = ?1 WHERE singleton = 1",
            vec![input.next_last.map_or(SqlValue::Null, SqlValue::Text)],
        ))?;
        Ok(CommandResult::Success(Json(true)))
    }
}
