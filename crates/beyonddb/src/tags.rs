//! Table resource tags committed with account Cell commands.

use crab_cell_runtime::registry::{Command, CommandContext, CommandResult, Query, QueryContext};
use extenddb_core::types::Tag;
use serde::{Deserialize, Serialize};

use crate::table::{command_table, query_table, statement};
use crate::{Error, Json, MODULE, Result, SqlBatch, SqlStatement, SqlValue, account_target};

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct TagRequest {
    pub account_id: String,
    pub table_name: String,
    pub resource_arn: String,
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) enum TagChange {
    Put(Tag),
    Remove(String),
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct UpdateTagsInput {
    pub request: TagRequest,
    pub changes: Vec<TagChange>,
}

pub(crate) struct UpdateTags;

impl Command for UpdateTags {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 15;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<UpdateTagsInput>;
    type Output = Json<bool>;

    fn execute(
        context: &mut CommandContext<'_, '_>,
        Json(input): Self::Input,
    ) -> Result<CommandResult<Self::Output>> {
        validate_request(&input.request)?;
        if account_target(&input.request.account_id)? != *context.target() {
            return Err(Error::Identity("resource tags reached the wrong account"));
        }
        let Some(table) = command_table(context, &input.request.table_name)? else {
            return Ok(CommandResult::Rejected(Json(false)));
        };
        let statements = input
            .changes
            .into_iter()
            .map(|change| match change {
                TagChange::Put(tag) => SqlStatement {
                    sql: "INSERT INTO ddb_table_tags (table_id, resource_arn, tag_key, tag_value) \
                          VALUES (?1, ?2, ?3, ?4) ON CONFLICT (table_id, resource_arn, tag_key) \
                          DO UPDATE SET tag_value = excluded.tag_value"
                        .into(),
                    parameters: vec![
                        SqlValue::Text(table.id.clone()),
                        SqlValue::Text(input.request.resource_arn.clone()),
                        SqlValue::Text(tag.key),
                        SqlValue::Text(tag.value),
                    ],
                },
                TagChange::Remove(key) => SqlStatement {
                    sql: "DELETE FROM ddb_table_tags WHERE table_id = ?1 AND resource_arn = ?2 AND tag_key = ?3".into(),
                    parameters: vec![
                        SqlValue::Text(table.id.clone()),
                        SqlValue::Text(input.request.resource_arn.clone()),
                        SqlValue::Text(key),
                    ],
                },
            })
            .collect::<Vec<_>>();
        if !statements.is_empty() {
            context.sql(&SqlBatch { statements })?;
        }
        Ok(CommandResult::Success(Json(true)))
    }
}

pub(crate) struct ReadTags;

impl Query for ReadTags {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 18;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<TagRequest>;
    type Output = Json<Option<Vec<Tag>>>;

    fn execute(context: &mut QueryContext<'_>, Json(request): Self::Input) -> Result<Self::Output> {
        validate_request(&request)?;
        if account_target(&request.account_id)?.cell_id() != context.cell_id() {
            return Err(Error::Identity("resource tags reached the wrong account"));
        }
        let Some(table) = query_table(context, &request.table_name)? else {
            return Ok(Json(None));
        };
        let result = context.sql(&statement(
            "SELECT tag_key, tag_value FROM ddb_table_tags \
             WHERE table_id = ?1 AND resource_arn = ?2 ORDER BY tag_key",
            vec![
                SqlValue::Text(table.id),
                SqlValue::Text(request.resource_arn),
            ],
        ))?;
        let mut tags = Vec::with_capacity(result[0].rows.len());
        for row in &result[0].rows {
            let [SqlValue::Text(key), SqlValue::Text(value)] = row.as_slice() else {
                return Err(Error::Command("invalid resource tag row"));
            };
            tags.push(Tag {
                key: key.clone(),
                value: value.clone(),
            });
        }
        Ok(Json(Some(tags)))
    }
}

pub(crate) fn parse_table_arn(arn: &str) -> Option<(&str, &str, &str)> {
    let rest = arn.strip_prefix("arn:aws:dynamodb:")?;
    let (region, rest) = rest.split_once(':')?;
    let (account_id, resource) = rest.split_once(':')?;
    let table_name = resource.strip_prefix("table/")?.split('/').next()?;
    if region.is_empty() || account_id.is_empty() || table_name.is_empty() {
        return None;
    }
    Some((region, account_id, table_name))
}

fn validate_request(request: &TagRequest) -> Result<()> {
    let Some((_, account_id, table_name)) = parse_table_arn(&request.resource_arn) else {
        return Err(Error::Identity("invalid table resource ARN"));
    };
    if account_id != request.account_id || table_name != request.table_name {
        return Err(Error::Identity("resource ARN does not match table"));
    }
    Ok(())
}
