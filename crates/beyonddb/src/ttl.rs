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
                     ON CONFLICT(table_id) DO UPDATE SET attribute_name = excluded.attribute_name",
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
