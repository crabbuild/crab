//! Ordered local-index reads fenced by the base item's transaction intents.

use crab_cell_runtime::registry::{Query, QueryContext};
use extenddb_core::types::{KeyType, extract_key, item_size_bytes};

use crate::item_storage::StoredItem;
use crate::partition::key::{index_key, partition_key_bytes};
use crate::partition::query::index_bounds;
use crate::table::{TableRecord, decode_table, statement};
use crate::{Error, Json, PartitionQueryInput, PartitionQueryOutcome, Result, SqlValue};

pub(crate) struct QueryAccountIndex;

impl Query for QueryAccountIndex {
    const MODULE: &'static str = crate::MODULE;
    const ID: u32 = 28;
    const CODEC_VERSION: u32 = 1;
    type Input = Json<PartitionQueryInput>;
    type Output = Json<PartitionQueryOutcome>;

    fn execute(context: &mut QueryContext<'_>, Json(input): Self::Input) -> Result<Self::Output> {
        let rows = context.sql(&statement(
            "SELECT t.record FROM ddb_tables t LEFT JOIN ddb_routes r ON t.table_id = r.table_id WHERE t.table_id = ?1 AND r.table_id IS NULL",
            vec![SqlValue::Text(input.table_id.clone())],
        ))?;
        let Some(table) = decode_table(&rows[0])? else {
            return Ok(Json(PartitionQueryOutcome::NotInstalled));
        };
        query(context, &table, input, false)
    }
}

pub(crate) fn query(
    context: &QueryContext<'_>,
    table: &TableRecord,
    input: PartitionQueryInput,
    routed: bool,
) -> Result<Json<PartitionQueryOutcome>> {
    let Some(index) = table
        .local_secondary_indexes
        .iter()
        .find(|index| Some(&index.index_name) == input.index_name.as_ref())
    else {
        return Ok(Json(PartitionQueryOutcome::InvalidKey));
    };
    if input.limit == 0 {
        return Ok(Json(PartitionQueryOutcome::InvalidLimit));
    }
    let hash_schema: Vec<_> = table
        .key_schema
        .iter()
        .filter(|key| key.key_type == KeyType::Hash)
        .cloned()
        .collect();
    if extenddb_core::validation::validate_key_only(
        &input.partition_key,
        &hash_schema,
        &table.attribute_definitions,
    )
    .is_err()
    {
        return Ok(Json(PartitionQueryOutcome::InvalidKey));
    }
    let range = index
        .key_schema
        .iter()
        .find(|key| key.key_type == KeyType::Range);
    if !input.extra_range_equals.is_empty()
        || input.sort.as_ref().is_some_and(|predicate| {
            range.map(|key| key.attribute_name.as_str()) != Some(predicate.attribute())
        })
    {
        return Ok(Json(PartitionQueryOutcome::InvalidCondition));
    }
    let bounds = match input.sort.as_ref().map(index_bounds).transpose() {
        Ok(bounds) => bounds.unwrap_or_default(),
        Err(_) => return Ok(Json(PartitionQueryOutcome::InvalidCondition)),
    };
    let partition = partition_key_bytes(&input.partition_key, &table.key_schema)?;
    let mut cursor = if let Some(key) = &input.exclusive_start_key {
        if !super::valid_cursor(key, table, index)
            || partition_key_bytes(key, &table.key_schema)? != partition
        {
            return Ok(Json(PartitionQueryOutcome::InvalidKey));
        }
        Some((
            index_key(key, &index.key_schema)?.1,
            index_key(key, &table.key_schema)?.1,
            crate::items::item_key(key, &table.key_schema)?,
        ))
    } else {
        None
    };
    // Base-sort lock ranges cannot fence an LSI sort-key move or absent create.
    // Fence the HASH group; account locks conservatively fence their table.
    let lock_query = if routed {
        statement(
            "SELECT transaction_id FROM ddb_partition_transaction_locks WHERE partition_key = ?1 AND write_lock = 1 LIMIT 1",
            vec![SqlValue::Blob(partition.clone())],
        )
    } else {
        statement(
            "SELECT transaction_id FROM ddb_account_transaction_locks WHERE table_id = ?1 AND write_lock = 1 LIMIT 1",
            vec![SqlValue::Text(table.id.clone())],
        )
    };
    let locks = context.sql(&lock_query)?;
    if let Some(conflict) = crate::participant::read_conflict(context, &locks[0])? {
        return Ok(Json(PartitionQueryOutcome::Conflict(conflict)));
    }
    let schema = super::key_schema(table, index);
    let limit = input.limit.min(10_000) as usize;
    let direction = if input.forward { "ASC" } else { "DESC" };
    let comparison = if input.forward { ">" } else { "<" };
    let mut items = Vec::new();
    let mut last = None;
    let mut bytes = 0_usize;
    loop {
        let mut sql = String::from(
            "SELECT item_key, sort_key, base_sort_key FROM ddb_local_index_items WHERE table_id = ? AND index_name = ? AND partition_key = ?",
        );
        let mut parameters = vec![
            SqlValue::Text(table.id.clone()),
            SqlValue::Text(index.index_name.clone()),
            SqlValue::Blob(partition.clone()),
        ];
        for (op, bound) in &bounds {
            sql.push_str(&format!(" AND sort_key {op} ?"));
            parameters.push(SqlValue::Blob(bound.clone()));
        }
        if let Some((sort, base_sort, key)) = &cursor {
            sql.push_str(&format!(
                " AND (sort_key, base_sort_key, item_key) {comparison} (?, ?, ?)"
            ));
            parameters.extend([
                SqlValue::Blob(sort.clone()),
                SqlValue::Blob(base_sort.clone()),
                SqlValue::Blob(key.clone()),
            ]);
        }
        sql.push_str(&format!(" ORDER BY sort_key {direction}, base_sort_key {direction}, item_key {direction} LIMIT 64"));
        let rows = context.sql(&statement(&sql, parameters))?;
        for row in &rows[0].rows {
            let [
                SqlValue::Blob(key),
                SqlValue::Blob(sort),
                SqlValue::Blob(base_sort),
            ] = row.as_slice()
            else {
                return Err(Error::Command("invalid local index row"));
            };
            let source = if routed {
                StoredItem::Partition(key)
            } else {
                StoredItem::Account {
                    table_id: &table.id,
                    key,
                }
            };
            let item = source
                .read(|batch| context.sql(batch))?
                .ok_or(Error::Command("local index entry has no base item"))?;
            let size = serde_json::to_vec(&item)?.len().max(item_size_bytes(&item));
            let next_bytes = bytes.saturating_add(size).saturating_add(128);
            if items.len() >= limit || (!items.is_empty() && next_bytes > 900_000) {
                return Ok(Json(PartitionQueryOutcome::Page {
                    items,
                    last_evaluated_key: last,
                }));
            }
            bytes = next_bytes;
            last = Some(extract_key(&item, &schema));
            items.push(item);
            cursor = Some((sort.clone(), base_sort.clone(), key.clone()));
        }
        if rows[0].rows.len() < 64 {
            break;
        }
    }
    Ok(Json(PartitionQueryOutcome::Page {
        items,
        last_evaluated_key: None,
    }))
}
