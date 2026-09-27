//! A base mutation and its index work publish through the same Cell command.

use crab_cell_runtime::registry::{
    Command, CommandContext, CommandResult, OperationDescriptor, Query, QueryContext,
};
use extenddb_core::types::Item;
use serde::{Deserialize, Serialize};

use super::ProjectionVersion;
use crate::item_storage::StoredValue;
use crate::table::{TableRecord, statement};
use crate::{Error, Json, Result, SqlValue};

pub(crate) const SCHEMA: &str = include_str!("../global_index_outbox_schema.sql");
const CHUNK_BYTES: u32 = 128 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct IndexChange {
    pub table: TableRecord,
    pub version: ProjectionVersion,
    pub old: Option<Item>,
    pub new: Option<Item>,
}

/// Identity and length of one immutable pending index projection.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IndexChangeHeader {
    pub id: [u8; 32],
    pub bytes: u32,
}

/// Outcome of one projection attempt across all indexes.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum IndexChangeDelivery {
    Applied([u8; 32]),
    Deferred([u8; 32]),
}

/// Read a bounded part of a previously observed immutable index change.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IndexChangeChunk {
    pub id: [u8; 32],
    pub offset: u32,
}

pub(crate) const fn chunk_operation(id: u32) -> OperationDescriptor {
    OperationDescriptor {
        input_limit: 4096,
        output_limit: 1024 * 1024,
        ..crate::operation(id)
    }
}

pub(crate) fn enqueue(
    context: &CommandContext<'_, '_>,
    table: &TableRecord,
    key: &[u8],
    source_epoch: u64,
    old: Option<Item>,
    new: Option<Item>,
) -> Result<Option<[u8; 32]>> {
    if table.global_secondary_indexes.iter().all(|index| {
        old.as_ref().and_then(|item| index.project(table, item))
            == new.as_ref().and_then(|item| index.project(table, item))
    }) {
        return Ok(None);
    }
    let mut hash = blake3::Hasher::new();
    hash.update(b"beyonddb.index-change.v1\0");
    hash.update(context.cell_id().as_bytes());
    hash.update(&context.sequence().to_be_bytes());
    hash.update(table.id.as_bytes());
    hash.update(key);
    let id = *hash.finalize().as_bytes();
    context.sql(&statement(
        "INSERT INTO ddb_index_changes (id, table_id, sequence, attempt_sequence, item) VALUES (?1, ?2, ?3, ?3, X'')",
        vec![
            SqlValue::Blob(id.to_vec()),
            SqlValue::Text(table.id.clone()),
            SqlValue::Blob(context.sequence().to_be_bytes().to_vec()),
        ],
    ))?;
    StoredValue::IndexChange(&id).write(
        context,
        &IndexChange {
            table: table.clone(),
            version: ProjectionVersion {
                source_epoch,
                sequence: context.sequence(),
            },
            old,
            new,
        },
    )?;
    Ok(Some(id))
}

pub(crate) fn old_bytes(table: &TableRecord, old: Option<&Item>) -> Result<u64> {
    if table.global_secondary_indexes.is_empty() {
        return Ok(0);
    }
    Ok(old
        .map(serde_json::to_vec)
        .transpose()?
        .map_or(0, |bytes| bytes.len() as u64))
}

pub(crate) fn reserve(
    capacity: &mut crate::secondary_index::Capacity,
    table: &TableRecord,
    old_bytes: u64,
    new: Option<&Item>,
) -> Result<()> {
    if table.global_secondary_indexes.is_empty() {
        return Ok(());
    }
    // Journal row, identity index, ordered work index, and blob allocation must
    // fit after COMMIT. Reserve old/new images once, independent of index count.
    capacity.edits += 8;
    let new_bytes = new
        .map(serde_json::to_vec)
        .transpose()?
        .map_or(0, |bytes| bytes.len() as u64);
    capacity.overflow_bytes +=
        2 * (old_bytes + new_bytes + serde_json::to_vec(table)?.len() as u64 + 1024);
    Ok(())
}

pub(crate) fn pending(context: &CommandContext<'_, '_>) -> Result<bool> {
    Ok(!context.sql(&statement(
        "SELECT 1 FROM ddb_index_changes LIMIT 1",
        vec![],
    ))?[0]
        .rows
        .is_empty())
}

fn peek(context: &QueryContext<'_>, table_id: String) -> Result<Json<Option<IndexChangeHeader>>> {
    let rows = context.sql(&statement("SELECT id, length(item) FROM ddb_index_changes WHERE table_id = ?1 ORDER BY attempt_sequence, sequence, id LIMIT 1", vec![SqlValue::Text(table_id)]))?;
    let Some(row) = rows[0].rows.first() else {
        return Ok(Json(None));
    };
    let [SqlValue::Blob(id), SqlValue::Integer(bytes)] = row.as_slice() else {
        return Err(Error::Command("invalid index change header"));
    };
    Ok(Json(Some(IndexChangeHeader {
        id: id
            .as_slice()
            .try_into()
            .map_err(|_| Error::Command("invalid index change identity"))?,
        bytes: u32::try_from(*bytes).map_err(|_| Error::Command("invalid index change size"))?,
    })))
}

fn chunk(context: &QueryContext<'_>, input: IndexChangeChunk) -> Result<Json<Option<Vec<u8>>>> {
    let rows = context.sql(&statement(
        &format!("SELECT substr(item, ?2, {CHUNK_BYTES}) FROM ddb_index_changes WHERE id = ?1"),
        vec![
            SqlValue::Blob(input.id.to_vec()),
            SqlValue::Integer(i64::from(input.offset) + 1),
        ],
    ))?;
    match rows[0].rows.first().map(Vec::as_slice) {
        None => Ok(Json(None)),
        Some([SqlValue::Blob(bytes)]) => Ok(Json(Some(bytes.clone()))),
        _ => Err(Error::Command("invalid index change chunk")),
    }
}

fn record_delivery(
    context: &CommandContext<'_, '_>,
    delivery: IndexChangeDelivery,
) -> Result<CommandResult<Json<()>>> {
    match delivery {
        IndexChangeDelivery::Applied(id) => {
            context.sql(&statement(
                "DELETE FROM ddb_index_changes WHERE id = ?1",
                vec![SqlValue::Blob(id.to_vec())],
            ))?;
        }
        IndexChangeDelivery::Deferred(id) => {
            // Move failed work behind already-enqueued changes. The same source
            // commit sequence orders new work and retries, preventing either from
            // starving; immutable projection versions still fence late delivery.
            context.sql(&statement(
                "UPDATE ddb_index_changes SET attempt_sequence = ?2 WHERE id = ?1",
                vec![
                    SqlValue::Blob(id.to_vec()),
                    SqlValue::Blob(context.sequence().to_be_bytes().to_vec()),
                ],
            ))?;
        }
    }
    Ok(CommandResult::Success(Json(())))
}

macro_rules! handlers {
    ($peek:ident, $chunk:ident, $ack:ident, $module:expr, $peek_id:expr, $chunk_id:expr, $ack_id:expr) => {
        /// Observe the next immutable projection journal entry for a base table.
        pub struct $peek;
        impl Query for $peek {
            const MODULE: &'static str = $module;
            const ID: u32 = $peek_id;
            const CODEC_VERSION: u32 = 1;
            type Input = Json<String>;
            type Output = Json<Option<IndexChangeHeader>>;
            fn execute(
                context: &mut QueryContext<'_>,
                Json(table): Self::Input,
            ) -> Result<Self::Output> {
                peek(context, table)
            }
        }
        /// Read part of an immutable projection journal entry.
        pub struct $chunk;
        impl Query for $chunk {
            const MODULE: &'static str = $module;
            const ID: u32 = $chunk_id;
            const CODEC_VERSION: u32 = 1;
            type Input = Json<IndexChangeChunk>;
            type Output = Json<Option<Vec<u8>>>;
            fn execute(
                context: &mut QueryContext<'_>,
                Json(input): Self::Input,
            ) -> Result<Self::Output> {
                chunk(context, input)
            }
        }
        /// Retire delivered work or defer a failed attempt without losing its journal.
        pub struct $ack;
        impl Command for $ack {
            const MODULE: &'static str = $module;
            const ID: u32 = $ack_id;
            const CODEC_VERSION: u32 = 2;
            type Input = Json<IndexChangeDelivery>;
            type Output = Json<()>;
            fn execute(
                context: &mut CommandContext<'_, '_>,
                Json(delivery): Self::Input,
            ) -> Result<CommandResult<Self::Output>> {
                record_delivery(context, delivery)
            }
        }
    };
}

handlers!(
    ReadAccountIndexChange,
    ReadAccountIndexChangeChunk,
    RecordAccountIndexDelivery,
    crate::MODULE,
    30,
    31,
    26
);
handlers!(
    ReadPartitionIndexChange,
    ReadPartitionIndexChangeChunk,
    RecordPartitionIndexDelivery,
    crate::DATA_MODULE,
    12,
    13,
    15
);
