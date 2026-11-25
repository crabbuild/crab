use super::{
    Definition, Field, FieldKind, Item, Layout, Result, SqlValue, Transaction, result_text,
    result_u64_from_row,
};
use crate::cells::Error;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU32;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(try_from = "String", into = "String")]
pub(crate) enum GroupBy {
    Status,
    IssueType,
    Field(Uuid),
}

impl GroupBy {
    pub(super) fn board_column(self, value: &str, statuses: &[String], fields: &[Field]) -> bool {
        match self {
            Self::Status => statuses.iter().any(|status| status == value),
            Self::IssueType => {
                matches!(value, "none" | "other")
                    || Uuid::parse_str(value)
                        .is_ok_and(|id| !id.is_nil() && id.to_string() == value)
            }
            Self::Field(id) => fields.iter().any(|field| {
                field.id == id
                    && matches!(&field.definition, FieldKind::Select { options }
                        if value.is_empty() || options.iter().any(|option| option == value))
            }),
        }
    }
}

pub(super) type ColumnLimits = BTreeMap<GroupBy, BTreeMap<String, NonZeroU32>>;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ColumnLimit {
    view: Uuid,
    group_by: GroupBy,
    value: String,
    limit: Option<NonZeroU32>,
}

impl ColumnLimit {
    pub(crate) fn apply(
        &self,
        definition: &mut Definition,
    ) -> std::result::Result<(), &'static str> {
        let view = definition
            .views
            .iter_mut()
            .find(|view| view.id == self.view && view.layout == Layout::Board)
            .ok_or("Choose a saved board view")?;
        if !self
            .group_by
            .board_column(&self.value, &definition.statuses, &definition.fields)
        {
            return Err("This board column is unavailable");
        }
        if let Some(limit) = self.limit {
            view.column_limits
                .entry(self.group_by)
                .or_default()
                .insert(self.value.clone(), limit);
        } else if let Some(columns) = view.column_limits.get_mut(&self.group_by) {
            columns.remove(&self.value);
            if columns.is_empty() {
                view.column_limits.remove(&self.group_by);
            }
        }
        if !definition.valid() {
            return Err("This Project exceeds the saved settings size limit");
        }
        Ok(())
    }
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
