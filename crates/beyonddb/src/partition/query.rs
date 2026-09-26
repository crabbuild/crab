//! Ordered base-table Query within the HASH key's owning Cell.

use extenddb_core::types::{AttributeValue, Item, KeyType, extract_key, item_size_bytes};
use serde::{Deserialize, Serialize};

use super::key::{partition_key_bytes, sort_component, sort_prefix};
use super::{
    AccessState, DATA_MODULE, Json, Query, QueryContext, Result, SqlValue, data_key_hash,
    decode_spec, query_access,
};
use crate::Error;
use crate::items::{item_key, valid_key};
use crate::table::statement;

/// One normalized comparison on a table RANGE key.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SortComparison {
    Eq,
    Lt,
    Le,
    Gt,
    Ge,
}

/// A normalized first RANGE-key predicate.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SortPredicate {
    Compare {
        attribute: String,
        op: SortComparison,
        value: AttributeValue,
    },
    Between {
        attribute: String,
        low: AttributeValue,
        high: AttributeValue,
    },
    BeginsWith {
        attribute: String,
        prefix: AttributeValue,
    },
}

impl SortPredicate {
    fn attribute(&self) -> &str {
        match self {
            Self::Compare { attribute, .. }
            | Self::Between { attribute, .. }
            | Self::BeginsWith { attribute, .. } => attribute,
        }
    }

    fn matches(&self, item: &Item) -> Result<bool> {
        let Some(value) = item.get(self.attribute()) else {
            return Ok(false);
        };
        match self {
            Self::Compare {
                op, value: bound, ..
            } => {
                let actual = sort_component(value)?;
                let bound = sort_component(bound)?;
                Ok(match op {
                    SortComparison::Eq => actual == bound,
                    SortComparison::Lt => actual < bound,
                    SortComparison::Le => actual <= bound,
                    SortComparison::Gt => actual > bound,
                    SortComparison::Ge => actual >= bound,
                })
            }
            Self::Between { low, high, .. } => {
                let actual = sort_component(value)?;
                Ok(actual >= sort_component(low)? && actual <= sort_component(high)?)
            }
            Self::BeginsWith { prefix, .. } => Ok(match (value, prefix) {
                (AttributeValue::S(value), AttributeValue::S(prefix)) => value.starts_with(prefix),
                (AttributeValue::B(value), AttributeValue::B(prefix)) => value.starts_with(prefix),
                _ => false,
            }),
        }
    }
}

/// One bounded query against a HASH key's owner Cell.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PartitionQueryInput {
    pub table_id: String,
    pub epoch: u64,
    pub partition_key: Item,
    pub sort: Option<SortPredicate>,
    pub extra_range_equals: Vec<(String, AttributeValue)>,
    pub forward: bool,
    pub limit: u32,
    pub exclusive_start_key: Option<Item>,
}

/// Result of an ordered data Cell query.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum PartitionQueryOutcome {
    Page {
        items: Vec<Item>,
        last_evaluated_key: Option<Item>,
    },
    NotInstalled,
    StaleRoute,
    Sealed,
    NotReady,
    WrongPartition,
    InvalidKey,
    InvalidCondition,
    InvalidLimit,
    /// A key in the requested range has an unresolved transaction intent.
    Conflict,
}

/// Read matching items in RANGE-key order from one data Cell.
pub struct PartitionQuery;

impl Query for PartitionQuery {
    const MODULE: &'static str = DATA_MODULE;
    const ID: u32 = 5;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<PartitionQueryInput>;
    type Output = Json<PartitionQueryOutcome>;

    fn execute(context: &mut QueryContext<'_>, Json(input): Self::Input) -> Result<Self::Output> {
        let rows = context.sql(&statement(
            "SELECT spec FROM ddb_partition WHERE singleton = 1",
            vec![],
        ))?;
        let Some(spec) = decode_spec(&rows[0])? else {
            return Ok(Json(PartitionQueryOutcome::NotInstalled));
        };
        if spec.table.id != input.table_id || spec.epoch != input.epoch {
            return Ok(Json(PartitionQueryOutcome::StaleRoute));
        }
        match query_access(context)? {
            AccessState::Serving => {}
            AccessState::Sealed => return Ok(Json(PartitionQueryOutcome::Sealed)),
            AccessState::Importing => return Ok(Json(PartitionQueryOutcome::NotReady)),
        }
        if input.limit == 0 {
            return Ok(Json(PartitionQueryOutcome::InvalidLimit));
        }
        let hash_attributes: Vec<_> = spec
            .table
            .key_schema
            .iter()
            .filter(|element| element.key_type == KeyType::Hash)
            .collect();
        if input.partition_key.len() != hash_attributes.len()
            || hash_attributes
                .iter()
                .any(|element| !input.partition_key.contains_key(&element.attribute_name))
        {
            return Ok(Json(PartitionQueryOutcome::InvalidKey));
        }
        let range_attributes: Vec<_> = spec
            .table
            .key_schema
            .iter()
            .filter(|element| element.key_type == KeyType::Range)
            .collect();
        if input.sort.as_ref().is_some_and(|sort| {
            range_attributes
                .first()
                .map(|element| element.attribute_name.as_str())
                != Some(sort.attribute())
        }) || input.extra_range_equals.iter().any(|(name, _)| {
            !range_attributes
                .iter()
                .skip(1)
                .any(|element| &element.attribute_name == name)
        }) {
            return Ok(Json(PartitionQueryOutcome::InvalidCondition));
        }
        if !spec.contains(data_key_hash(
            &spec.table.id,
            &input.partition_key,
            &spec.table.key_schema,
        )?) {
            return Ok(Json(PartitionQueryOutcome::WrongPartition));
        }
        let partition_key = partition_key_bytes(&input.partition_key, &spec.table.key_schema)?;
        let mut cursor = match &input.exclusive_start_key {
            Some(start) if valid_key(start, &spec.table) => {
                if partition_key_bytes(start, &spec.table.key_schema)? != partition_key {
                    return Ok(Json(PartitionQueryOutcome::InvalidKey));
                }
                Some((
                    super::key::index_key(start, &spec.table.key_schema)?.1,
                    item_key(start, &spec.table.key_schema)?,
                ))
            }
            Some(_) => return Ok(Json(PartitionQueryOutcome::InvalidKey)),
            None => None,
        };
        let mut items = Vec::new();
        let mut last_returned = None;
        let mut bytes = 0_usize;
        let limit = input.limit.min(10_000) as usize;
        let bounds = if range_attributes.len() == 1 {
            match input.sort.as_ref().map(index_bounds).transpose() {
                Ok(Some(bounds)) => bounds,
                Ok(None) => Vec::new(),
                Err(_) => return Ok(Json(PartitionQueryOutcome::InvalidCondition)),
            }
        } else {
            Vec::new()
        };
        // Probe the intent index itself: a prepared insert has no live row.
        // Multi-attribute RANGE predicates conservatively fence the HASH group.
        let (predicate, parameters) =
            range_predicate(&partition_key, &bounds, &cursor, input.forward);
        let locks = context.sql(&statement(
            &format!("SELECT 1 FROM ddb_partition_transaction_locks {predicate} AND write_lock = 1 LIMIT 1"),
            parameters,
        ))?;
        if !locks[0].rows.is_empty() {
            return Ok(Json(PartitionQueryOutcome::Conflict));
        }
        loop {
            let (predicate, parameters) =
                range_predicate(&partition_key, &bounds, &cursor, input.forward);
            let mut sql =
                format!("SELECT item_key, sort_key, item FROM ddb_partition_items {predicate}");
            let order = if input.forward { "ASC" } else { "DESC" };
            sql.push_str(&format!(
                " ORDER BY sort_key {order}, item_key {order} LIMIT 64"
            ));
            let rows = context.sql(&statement(&sql, parameters))?;
            let page = &rows[0].rows;
            if page.is_empty() {
                break;
            }
            for row in page {
                let [
                    SqlValue::Blob(key),
                    SqlValue::Blob(sort),
                    SqlValue::Blob(image),
                ] = row.as_slice()
                else {
                    return Err(Error::Command("invalid partition query row"));
                };
                let item: Item = serde_json::from_slice(image)?;
                if matches_item(&input, &item)? {
                    if items.len() >= limit {
                        return Ok(Json(PartitionQueryOutcome::Page {
                            items,
                            last_evaluated_key: last_returned,
                        }));
                    }
                    let encoded_bytes = image.len().max(item_size_bytes(&item));
                    let next_bytes = bytes.saturating_add(encoded_bytes).saturating_add(128);
                    if !items.is_empty() && next_bytes > 900_000 {
                        return Ok(Json(PartitionQueryOutcome::Page {
                            items,
                            last_evaluated_key: last_returned,
                        }));
                    }
                    bytes = next_bytes;
                    last_returned = Some(extract_key(&item, &spec.table.key_schema));
                    items.push(item);
                }
                cursor = Some((sort.clone(), key.clone()));
            }
            if page.len() < 64 {
                break;
            }
        }
        Ok(Json(PartitionQueryOutcome::Page {
            items,
            last_evaluated_key: None,
        }))
    }
}

fn range_predicate(
    partition_key: &[u8],
    bounds: &[(&str, Vec<u8>)],
    cursor: &Option<(Vec<u8>, Vec<u8>)>,
    forward: bool,
) -> (String, Vec<SqlValue>) {
    let mut sql = String::from("WHERE partition_key = ?");
    let mut parameters = vec![SqlValue::Blob(partition_key.to_vec())];
    for (operator, bound) in bounds {
        sql.push_str(" AND sort_key ");
        sql.push_str(operator);
        sql.push_str(" ?");
        parameters.push(SqlValue::Blob(bound.clone()));
    }
    if let Some((sort, key)) = cursor {
        let cmp = if forward { ">" } else { "<" };
        sql.push_str(&format!(" AND (sort_key, item_key) {cmp} (?, ?)"));
        parameters.extend([SqlValue::Blob(sort.clone()), SqlValue::Blob(key.clone())]);
    }
    (sql, parameters)
}

fn index_bounds(predicate: &SortPredicate) -> Result<Vec<(&'static str, Vec<u8>)>> {
    match predicate {
        SortPredicate::Compare { op, value, .. } => {
            let operator = match op {
                SortComparison::Eq => "=",
                SortComparison::Lt => "<",
                SortComparison::Le => "<=",
                SortComparison::Gt => ">",
                SortComparison::Ge => ">=",
            };
            Ok(vec![(operator, sort_component(value)?)])
        }
        SortPredicate::Between { low, high, .. } => Ok(vec![
            (">=", sort_component(low)?),
            ("<=", sort_component(high)?),
        ]),
        SortPredicate::BeginsWith { prefix, .. } => {
            let lower = sort_prefix(prefix)?;
            let mut bounds = vec![(">=", lower.clone())];
            if let Some(upper) = prefix_upper_bound(lower) {
                bounds.push(("<", upper));
            }
            Ok(bounds)
        }
    }
}

fn prefix_upper_bound(mut prefix: Vec<u8>) -> Option<Vec<u8>> {
    while let Some(last) = prefix.last_mut() {
        if *last < u8::MAX {
            *last += 1;
            return Some(prefix);
        }
        prefix.pop();
    }
    None
}

fn matches_item(input: &PartitionQueryInput, item: &Item) -> Result<bool> {
    if let Some(sort) = &input.sort
        && !sort.matches(item)?
    {
        return Ok(false);
    }
    for (name, expected) in &input.extra_range_equals {
        let Some(actual) = item.get(name) else {
            return Ok(false);
        };
        if sort_component(actual)? != sort_component(expected)? {
            return Ok(false);
        }
    }
    Ok(true)
}
