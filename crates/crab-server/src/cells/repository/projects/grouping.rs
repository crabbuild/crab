use super::{
    Definition, FieldKind, Item, Result, SqlValue, Transaction, result_text, result_u64_from_row,
};
use crate::cells::Error;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(try_from = "String", into = "String")]
pub(crate) enum GroupBy {
    Status,
    IssueType,
    Field(Uuid),
}

impl TryFrom<String> for GroupBy {
    type Error = &'static str;

    fn try_from(value: String) -> std::result::Result<Self, Self::Error> {
        match value.as_str() {
            "status" => Ok(Self::Status),
            "issue_type" => Ok(Self::IssueType),
            field => field
                .strip_prefix("field:")
                .and_then(|id| Uuid::parse_str(id).ok())
                .filter(|id| !id.is_nil())
                .map(Self::Field)
                .ok_or("Invalid Project grouping field"),
        }
    }
}

impl From<GroupBy> for String {
    fn from(value: GroupBy) -> Self {
        match value {
            GroupBy::Status => "status".into(),
            GroupBy::IssueType => "issue_type".into(),
            GroupBy::Field(id) => format!("field:{id}"),
        }
    }
}

pub(crate) fn field_counts<'a>(
    context: &Transaction<'_>,
    definition: &Definition,
    field: Uuid,
    values_path: &str,
    predicate: &str,
    arguments: &[SqlValue],
    page: impl Iterator<Item = &'a Item>,
) -> Result<BTreeMap<String, u64>> {
    let field = definition
        .fields
        .iter()
        .find(|candidate| candidate.id == field)
        .ok_or(Error::Invalid("Unknown Project grouping field"))?;
    let path = SqlValue::Text(format!(r#"{values_path}."{}""#, field.id));
    const KEY: &str = "COALESCE(json_extract(p.data, ?), '')";
    let mut bindings = vec![path.clone()];
    bindings.extend_from_slice(arguments);
    let mut condition = String::new();
    if !matches!(field.definition, FieldKind::Select { .. }) {
        // Free-form groups only need counts for values present on this page.
        // A project with distinct values keeps the response bounded by page size.
        let values: BTreeSet<&str> = page
            .map(|item| item.values.get(&field.id).map_or("", String::as_str))
            .collect();
        condition = format!(" AND {KEY} IN (SELECT value FROM json_each(?))");
        bindings.extend([path, SqlValue::Text(serde_json::to_string(&values)?)]);
    }
    context
        .query(
            &format!("SELECT {KEY},COUNT(*) {predicate}{condition} GROUP BY 1"),
            &bindings,
        )?
        .rows
        .iter()
        .map(|row| Ok((result_text(row, 0)?, result_u64_from_row(row, 1)?)))
        .collect()
}
