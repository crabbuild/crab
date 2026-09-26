//! Local index keys share the base item's command, locks, and Cell ownership.

mod read;
pub(crate) use read::{QueryAccountIndex, query};

use crab_cell_runtime::registry::CommandContext;
use extenddb_core::types::{Item, KeySchemaElement, LsiInput, extract_key};
use serde::{Deserialize, Serialize};

use crate::items::{item_key, valid_key};
use crate::partition::key::index_key;
use crate::table::{TableRecord, statement};
use crate::{Result, SqlValue};

pub(crate) const SCHEMA: &str = include_str!("secondary_index_schema.sql");

struct Entry {
    name: String,
    partition: Vec<u8>,
    sort: Vec<u8>,
    base_sort: Vec<u8>,
}

fn entries(table: &TableRecord, item: &Item) -> Result<Vec<Entry>> {
    table
        .local_secondary_indexes
        .iter()
        .filter(|index| {
            index
                .key_schema
                .iter()
                .all(|key| item.contains_key(&key.attribute_name))
        })
        .map(|index| {
            let (partition, sort) = index_key(item, &index.key_schema)?;
            let (_, base_sort) = index_key(item, &table.key_schema)?;
            Ok(Entry {
                name: index.index_name.clone(),
                partition,
                sort,
                base_sort,
            })
        })
        .collect()
}

pub(crate) fn delete(
    context: &CommandContext<'_, '_>,
    table: &TableRecord,
    key: &[u8],
) -> Result<()> {
    // Index names are immutable. Use the full primary-key prefix for each delete;
    // filtering table+item alone would scan every entry between those columns.
    for index in &table.local_secondary_indexes {
        context.sql(&statement(
            "DELETE FROM ddb_local_index_items WHERE table_id = ?1 AND index_name = ?2 AND item_key = ?3",
            vec![SqlValue::Text(table.id.clone()), SqlValue::Text(index.index_name.clone()), SqlValue::Blob(key.to_vec())],
        ))?;
    }
    Ok(())
}

pub(crate) fn write(
    context: &CommandContext<'_, '_>,
    table: &TableRecord,
    key: &[u8],
    item: &Item,
) -> Result<()> {
    delete(context, table, key)?;
    for entry in entries(table, item)? {
        context.sql(&statement(
            "INSERT INTO ddb_local_index_items (table_id, index_name, item_key, partition_key, sort_key, base_sort_key) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            vec![SqlValue::Text(table.id.clone()), SqlValue::Text(entry.name), SqlValue::Blob(key.to_vec()), SqlValue::Blob(entry.partition), SqlValue::Blob(entry.sort), SqlValue::Blob(entry.base_sort)],
        ))?;
    }
    Ok(())
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Capacity {
    pub edits: u64,
    pub overflow_bytes: u64,
}

pub(crate) fn capacity(table: &TableRecord, item: Option<&Item>) -> Result<Capacity> {
    // A replacement deletes and inserts in two B-trees. Reserve overflow bytes
    // separately: the same base/HASH keys can appear in all five local indexes.
    let mut capacity = Capacity {
        edits: table.local_secondary_indexes.len() as u64 * 4,
        overflow_bytes: 0,
    };
    if let Some(item) = item {
        let key = item_key(item, &table.key_schema)?;
        for entry in entries(table, item)? {
            let bytes = table.id.len()
                + entry.name.len()
                + key.len()
                + entry.partition.len()
                + entry.sort.len()
                + entry.base_sort.len()
                + 128;
            capacity.overflow_bytes += bytes as u64 * 2;
        }
    }
    Ok(capacity)
}

pub(crate) fn valid_item(item: &Item, table: &TableRecord) -> bool {
    let indexes: Vec<_> = table
        .local_secondary_indexes
        .iter()
        .map(|index| extenddb_core::validation::IndexKeyRef {
            index_name: &index.index_name,
            key_schema: &index.key_schema,
        })
        .collect();
    // DynamoDB's item limit includes every corresponding LSI projection. ALL
    // is the only admitted projection until the engine can plan base-table fetches.
    let present = table
        .local_secondary_indexes
        .iter()
        .filter(|index| {
            index
                .key_schema
                .iter()
                .all(|key| item.contains_key(&key.attribute_name))
        })
        .count();
    if extenddb_core::types::item_size_bytes(item) * (1 + present)
        > extenddb_core::limits::LimitsConfig::default().max_item_size_bytes
    {
        return false;
    }
    extenddb_core::validation::validate_index_keys(item, &indexes, &table.attribute_definitions)
        .is_ok()
        && table.local_secondary_indexes.iter().all(|index| {
            extenddb_core::validation::validate_key_sizes(
                item,
                &index.key_schema,
                &extenddb_core::limits::LimitsConfig::default(),
            )
            .is_ok()
        })
}

pub(crate) fn key_schema(table: &TableRecord, index: &LsiInput) -> Vec<KeySchemaElement> {
    let mut schema = table.key_schema.clone();
    for key in &index.key_schema {
        if !schema
            .iter()
            .any(|existing| existing.attribute_name == key.attribute_name)
        {
            schema.push(key.clone());
        }
    }
    schema
}

pub(crate) fn valid_cursor(key: &Item, table: &TableRecord, index: &LsiInput) -> bool {
    let schema = key_schema(table, index);
    key.len() == schema.len()
        && schema
            .iter()
            .all(|attr| key.contains_key(&attr.attribute_name))
        && valid_key(&extract_key(key, &table.key_schema), table)
        && extenddb_core::validation::validate_item_keys(
            key,
            &index.key_schema,
            &table.attribute_definitions,
        )
        .is_ok()
        && extenddb_core::validation::validate_key_sizes(
            key,
            &index.key_schema,
            &extenddb_core::limits::LimitsConfig::default(),
        )
        .is_ok()
}

pub(crate) fn scan_keys(
    context: &crab_cell_runtime::registry::QueryContext<'_>,
    table_id: &str,
    index_name: Option<&str>,
    cursor: &[u8],
    routed: bool,
    limit: u32,
) -> Result<Vec<Vec<SqlValue>>> {
    let (source, predicate, parameters) = if let Some(name) = index_name {
        (
            "ddb_local_index_items",
            "table_id = ?1 AND index_name = ?2 AND item_key > ?3",
            vec![
                SqlValue::Text(table_id.into()),
                SqlValue::Text(name.into()),
                SqlValue::Blob(cursor.to_vec()),
            ],
        )
    } else if routed {
        (
            "ddb_partition_items",
            "item_key > ?1",
            vec![SqlValue::Blob(cursor.to_vec())],
        )
    } else {
        (
            "ddb_items",
            "table_id = ?1 AND item_key > ?2",
            vec![
                SqlValue::Text(table_id.into()),
                SqlValue::Blob(cursor.to_vec()),
            ],
        )
    };
    let sql =
        format!("SELECT item_key FROM {source} WHERE {predicate} ORDER BY item_key LIMIT {limit}");
    context
        .sql(&statement(&sql, parameters))?
        .into_iter()
        .next()
        .map(|result| result.rows)
        .ok_or(crate::Error::Command("scan statement returned no result"))
}
