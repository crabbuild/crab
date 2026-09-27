//! Read projected images without fetching base-table attributes.

use crab_cell_runtime::registry::{Query, QueryContext};
use extenddb_core::types::{Item, KeyType, extract_key, item_size_bytes};

use super::{MODULE, read_spec};
use crate::item_storage::StoredValue;
use crate::partition::key::{index_key, partition_key_bytes, sort_component, sort_prefix};
use crate::table::statement;
use crate::{
    Error, Json, PartitionQueryInput, PartitionQueryOutcome, PartitionScanInput,
    PartitionScanOutcome, Result, SortComparison, SortPredicate, SqlValue,
};

/// Read one global-index HASH group in index sort-key order.
pub struct GlobalIndexQuery;

impl Query for GlobalIndexQuery {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 2;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<PartitionQueryInput>;
    type Output = Json<PartitionQueryOutcome>;

    fn execute(context: &mut QueryContext<'_>, Json(input): Self::Input) -> Result<Self::Output> {
        use PartitionQueryOutcome as Outcome;
        let Some(spec) = read_spec(|batch| context.sql(batch))? else {
            return Ok(Json(Outcome::NotInstalled));
        };
        if input.table_id != spec.index.id
            || input.epoch != spec.epoch
            || !super::transfer::serving(|batch| context.sql(batch))?
        {
            return Ok(Json(Outcome::StaleRoute));
        }
        if input.limit == 0 {
            return Ok(Json(Outcome::InvalidLimit));
        }
        let schema = &spec.index.specification.key_schema;
        let hash_schema: Vec<_> = schema
            .iter()
            .filter(|key| key.key_type == KeyType::Hash)
            .cloned()
            .collect();
        if extenddb_core::validation::validate_key_only(
            &input.partition_key,
            &hash_schema,
            &spec.table.attribute_definitions,
        )
        .is_err()
        {
            return Ok(Json(Outcome::InvalidKey));
        }
        if !spec.contains(&input.partition_key)? {
            return Ok(Json(Outcome::WrongPartition));
        }
        let bounds = match bounds(&input, schema, &spec.table.attribute_definitions) {
            Ok(bounds) => bounds,
            Err(_) => return Ok(Json(Outcome::InvalidCondition)),
        };
        let partition = partition_key_bytes(&input.partition_key, schema)?;
        let keys = spec.index.key_schema(&spec.table);
        let mut cursor = if let Some(key) = &input.exclusive_start_key {
            if !spec.valid_key(key) || partition_key_bytes(key, schema)? != partition {
                return Ok(Json(Outcome::InvalidKey));
            }
            Some((
                index_key(key, schema)?.1,
                crate::items::item_key(key, &keys)?,
            ))
        } else {
            None
        };
        let order = if input.forward { "ASC" } else { "DESC" };
        let comparison = if input.forward { ">" } else { "<" };
        let mut items = Vec::new();
        let mut bytes = 0_usize;
        let mut last = None;
        loop {
            let mut sql = String::from(
                "SELECT item_key, sort_key FROM ddb_global_index_items WHERE item IS NOT NULL AND partition_key = ?",
            );
            let mut parameters = vec![SqlValue::Blob(partition.clone())];
            for (operator, bound) in &bounds {
                sql.push_str(&format!(" AND sort_key {operator} ?"));
                parameters.push(SqlValue::Blob(bound.clone()));
            }
            if let Some((sort, key)) = &cursor {
                sql.push_str(&format!(" AND (sort_key, item_key) {comparison} (?, ?)"));
                parameters.extend([SqlValue::Blob(sort.clone()), SqlValue::Blob(key.clone())]);
            }
            sql.push_str(&format!(
                " ORDER BY sort_key {order}, item_key {order} LIMIT 64"
            ));
            let rows = context.sql(&statement(&sql, parameters))?;
            for row in &rows[0].rows {
                let [SqlValue::Blob(key), SqlValue::Blob(sort)] = row.as_slice() else {
                    return Err(Error::Command("invalid global-index query row"));
                };
                let item = StoredValue::GlobalIndex(key)
                    .read(|batch| context.sql(batch))?
                    .ok_or(Error::Command("global-index row lost its image"))?;
                let next = bytes
                    .saturating_add(serde_json::to_vec(&item)?.len().max(item_size_bytes(&item)))
                    .saturating_add(128);
                if items.len() >= input.limit.min(10_000) as usize
                    || (!items.is_empty() && next > 900_000)
                {
                    return Ok(Json(Outcome::Page {
                        items,
                        last_evaluated_key: last,
                    }));
                }
                bytes = next;
                last = Some(extract_key(&item, &keys));
                items.push(item);
                cursor = Some((sort.clone(), key.clone()));
            }
            if rows[0].rows.len() < 64 {
                break;
            }
        }
        Ok(Json(Outcome::Page {
            items,
            last_evaluated_key: None,
        }))
    }
}

/// Scan one global-index range in complete index/base-key order.
pub struct GlobalIndexScan;

impl Query for GlobalIndexScan {
    const MODULE: &'static str = MODULE;
    const ID: u32 = 3;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<PartitionScanInput>;
    type Output = Json<PartitionScanOutcome>;

    fn execute(context: &mut QueryContext<'_>, Json(input): Self::Input) -> Result<Self::Output> {
        use PartitionScanOutcome as Outcome;
        let Some(spec) = read_spec(|batch| context.sql(batch))? else {
            return Ok(Json(Outcome::NotInstalled));
        };
        if input.table_id != spec.index.id
            || input.epoch != spec.epoch
            || !super::transfer::serving(|batch| context.sql(batch))?
        {
            return Ok(Json(Outcome::StaleRoute));
        }
        if input.limit == Some(0) {
            return Ok(Json(Outcome::InvalidLimit));
        }
        let schema = spec.index.key_schema(&spec.table);
        let mut cursor = match &input.exclusive_start_key {
            Some(key) if spec.valid_key(key) => crate::items::item_key(key, &schema)?,
            Some(_) => return Ok(Json(Outcome::InvalidKey)),
            None => Vec::new(),
        };
        let mut items: Vec<Item> = Vec::new();
        let mut bytes = 0_usize;
        let mut last = None;
        loop {
            let rows = context.sql(&statement("SELECT item_key FROM ddb_global_index_items WHERE item IS NOT NULL AND item_key > ?1 ORDER BY item_key LIMIT 64", vec![SqlValue::Blob(cursor.clone())]))?;
            for row in &rows[0].rows {
                let [SqlValue::Blob(key)] = row.as_slice() else {
                    return Err(Error::Command("invalid global-index scan row"));
                };
                let item = StoredValue::GlobalIndex(key)
                    .read(|batch| context.sql(batch))?
                    .ok_or(Error::Command("global-index row lost its image"))?;
                let next = bytes
                    .saturating_add(serde_json::to_vec(&item)?.len().max(item_size_bytes(&item)))
                    .saturating_add(128);
                if items.len() >= input.limit.unwrap_or(10_000).min(10_000) as usize
                    || (!items.is_empty() && next > 900_000)
                {
                    return Ok(Json(Outcome::Page {
                        items,
                        last_evaluated_key: last,
                    }));
                }
                bytes = next;
                last = Some(extract_key(&item, &schema));
                items.push(item);
                cursor = key.clone();
            }
            if rows[0].rows.len() < 64 {
                break;
            }
        }
        Ok(Json(Outcome::Page {
            items,
            last_evaluated_key: None,
        }))
    }
}

fn upper(mut prefix: Vec<u8>) -> Result<Vec<u8>> {
    while let Some(last) = prefix.last_mut() {
        if *last < u8::MAX {
            *last += 1;
            return Ok(prefix);
        }
        prefix.pop();
    }
    Err(Error::Command("global-index prefix has no upper bound"))
}

fn bounds(
    input: &PartitionQueryInput,
    schema: &[extenddb_core::types::KeySchemaElement],
    definitions: &[extenddb_core::types::AttributeDefinition],
) -> Result<Vec<(&'static str, Vec<u8>)>> {
    let ranges: Vec<_> = schema
        .iter()
        .filter(|key| key.key_type == KeyType::Range)
        .collect();
    let mut predicates = std::collections::BTreeMap::new();
    for (name, value) in &input.extra_range_equals {
        if predicates
            .insert(
                name.as_str(),
                SortPredicate::Compare {
                    attribute: name.clone(),
                    op: SortComparison::Eq,
                    value: value.clone(),
                },
            )
            .is_some()
        {
            return Err(Error::Command("duplicate global-index sort predicate"));
        }
    }
    if let Some(predicate) = &input.sort
        && predicates
            .insert(predicate.attribute(), predicate.clone())
            .is_some()
    {
        return Err(Error::Command("duplicate global-index sort predicate"));
    }
    let mut prefix = Vec::new();
    for range in ranges {
        let Some(predicate) = predicates.remove(range.attribute_name.as_str()) else {
            break;
        };
        let expected = definitions
            .iter()
            .find(|definition| definition.attribute_name == range.attribute_name)
            .ok_or(Error::Command(
                "global-index sort attribute has no definition",
            ))?
            .attribute_type;
        let values: &[&extenddb_core::types::AttributeValue] = match &predicate {
            SortPredicate::Compare { value, .. } => &[value],
            SortPredicate::Between { low, high, .. } => &[low, high],
            SortPredicate::BeginsWith { prefix, .. } => &[prefix],
        };
        if values.iter().any(|value| {
            !matches!(
                (expected, value),
                (
                    extenddb_core::types::ScalarAttributeType::S,
                    extenddb_core::types::AttributeValue::S(_)
                ) | (
                    extenddb_core::types::ScalarAttributeType::N,
                    extenddb_core::types::AttributeValue::N(_)
                ) | (
                    extenddb_core::types::ScalarAttributeType::B,
                    extenddb_core::types::AttributeValue::B(_)
                )
            )
        }) {
            return Err(Error::Command("global-index sort operand type mismatch"));
        }
        if let SortPredicate::Compare {
            op: SortComparison::Eq,
            value,
            ..
        } = &predicate
        {
            prefix.extend(sort_component(value)?);
            continue;
        }
        if !predicates.is_empty() {
            return Err(Error::Command(
                "range comparison must be the final sort condition",
            ));
        }
        let mut result = Vec::new();
        if !prefix.is_empty() {
            result.extend([(">=", prefix.clone()), ("<", upper(prefix.clone())?)]);
        }
        let with = |value: Vec<u8>| [prefix.clone(), value].concat();
        match predicate {
            SortPredicate::Compare { op, value, .. } => {
                let value = with(sort_component(&value)?);
                result.push(match op {
                    SortComparison::Lt => ("<", value),
                    SortComparison::Le => ("<", upper(value)?),
                    SortComparison::Gt => (">=", upper(value)?),
                    SortComparison::Ge => (">=", value),
                    SortComparison::Eq => {
                        return Err(Error::Command("unconsumed equality predicate"));
                    }
                });
            }
            SortPredicate::Between { low, high, .. } => result.extend([
                (">=", with(sort_component(&low)?)),
                ("<", upper(with(sort_component(&high)?))?),
            ]),
            SortPredicate::BeginsWith { prefix: value, .. } => {
                let low = with(sort_prefix(&value)?);
                result.push((">=", low.clone()));
                // Empty and all-0xff binary prefixes extend to the end of this
                // HASH group; they have no finite exclusive upper bound.
                if let Ok(high) = upper(low) {
                    result.push(("<", high));
                }
            }
        }
        return Ok(result);
    }
    if !predicates.is_empty() {
        return Err(Error::Command(
            "global-index sort conditions are not a leading prefix",
        ));
    }
    if prefix.is_empty() {
        Ok(Vec::new())
    } else {
        Ok(vec![(">=", prefix.clone()), ("<", upper(prefix)?)])
    }
}
