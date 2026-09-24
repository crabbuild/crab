//! Blob primitive: content-addressed parts with lease-based uploads.
use std::collections::BTreeSet;

use bytes::Bytes;
use crab_storage::{
    GLOBAL_PREFIX, StorageError, Store, content_hash_from_path, global_content_path,
    global_content_prefix,
};
use futures_util::StreamExt;
use object_store::path::Path as ObjectPath;
use rusqlite::{Connection, OptionalExtension, Transaction};

use crate::{Error, Result};

mod api;
mod sql;
mod store;
#[cfg(test)]
mod tests;

pub use api::{BlobCommand, BlobModule, BlobNamespace, BlobQueryCommand, register_blob};
pub use store::{BlobArtifactStore, BlobGarbageCollectionReport};

use sql::*;
use store::part_digest;

const BLOB_SCHEMA: &str = include_str!("../migrations/blob.sql");
pub(crate) const BLOB_TABLE: &str = "blob_uploads";
pub(crate) const MAX_BLOB_PART_BYTES: usize = 256 * 1024;
pub(crate) const MAX_BLOB_READ_BYTES: u32 = 512 * 1024;
pub(crate) const MAX_BLOB_PARTS: u32 = 4_096;
const MAX_BLOB_BYTES: u64 = MAX_BLOB_PART_BYTES as u64 * MAX_BLOB_PARTS as u64;
const MAX_KEY_BYTES: usize = 1_024;
const MAX_METADATA_BYTES: usize = 8 * 1_024;
const MAX_CONTENT_TYPE_BYTES: usize = 256;
const MIN_UPLOAD_LIFETIME_MS: i64 = 60_000;
const MAX_UPLOAD_LIFETIME_MS: i64 = 7 * 24 * 60 * 60 * 1_000;
const BLOB_PART_KIND: &str = "blob-parts";
const MAX_BLOB_READ_PARTS: usize = 8;
const MAX_BLOB_GC_DELETIONS: u32 = 128;

#[derive(Clone, Copy)]
struct BlobMutationTimes {
    now_ms: i64,
    issued_at_ms: i64,
}

/// Conditional publication rule captured when an upload begins.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlobCondition {
    Any,
    Missing,
    Etag([u8; 32]),
}

/// One bounded mutation against a blob namespace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlobMutation {
    Begin {
        key: Vec<u8>,
        upload_id: [u8; 16],
        condition: BlobCondition,
        content_type: Option<String>,
        metadata: Vec<u8>,
        expires_at_ms: i64,
    },
    PutPart {
        key: Vec<u8>,
        upload_id: [u8; 16],
        part_number: u32,
        payload: Vec<u8>,
    },
    /// Stores a reference to a content-addressed object-store part.
    ///
    /// This variant is emitted by [`BlobNamespace`] after it uploads the
    /// payload. Applications should use [`BlobMutation::PutPart`] instead.
    #[doc(hidden)]
    PutPartRef {
        key: Vec<u8>,
        upload_id: [u8; 16],
        part_number: u32,
        digest: [u8; 32],
        size: u32,
    },
    Complete {
        key: Vec<u8>,
        upload_id: [u8; 16],
        part_count: u32,
    },
    Abort {
        key: Vec<u8>,
        upload_id: [u8; 16],
    },
    Delete {
        key: Vec<u8>,
        condition: BlobCondition,
    },
}

/// Business result of a blob mutation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlobMutationOutcome {
    Begun,
    PartStored { digest: [u8; 32] },
    Committed { etag: [u8; 32], size: u64 },
    Aborted,
    Deleted,
    NotFound,
    Conflict,
}

/// Immutable metadata for one published blob.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlobMetadata {
    pub key: Vec<u8>,
    pub etag: [u8; 32],
    pub size: u64,
    pub part_count: u32,
    pub content_type: Option<String>,
    pub metadata: Vec<u8>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// One bounded blob read result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlobRead {
    pub metadata: BlobMetadata,
    pub offset: u64,
    pub bytes: Vec<u8>,
    pub(crate) parts: Vec<BlobPart>,
    pub(crate) end: u64,
}

/// One object-store part intersecting a bounded Blob range.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BlobPart {
    digest: [u8; 32],
    offset: u64,
    size: u32,
}

/// One lexicographically ordered page of blob metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlobPage {
    pub objects: Vec<BlobMetadata>,
    pub next: Option<Vec<u8>>,
}

/// One bounded read against a blob namespace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlobQuery {
    Head {
        key: Vec<u8>,
    },
    Read {
        key: Vec<u8>,
        offset: u64,
        limit: u32,
    },
    List {
        prefix: Vec<u8>,
        after: Option<Vec<u8>>,
        limit: u32,
    },
}

/// Result shape for a blob query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlobQueryResult {
    Head(Option<BlobMetadata>),
    Read(Option<BlobRead>),
    List(BlobPage),
}

/// Installs the current Blob metadata and manifest schema.
pub fn install_blob_schema(transaction: &Transaction<'_>) -> Result<()> {
    transaction.execute_batch(BLOB_SCHEMA)?;
    Ok(())
}

/// Applies one multipart or conditional blob mutation atomically.
pub fn blob_mutate(
    transaction: &Transaction<'_>,
    now_ms: i64,
    issued_at_ms: i64,
    mutation: &BlobMutation,
) -> Result<BlobMutationOutcome> {
    validate_now(now_ms)?;
    validate_now(issued_at_ms)?;
    match mutation {
        BlobMutation::Begin {
            key,
            upload_id,
            condition,
            content_type,
            metadata,
            expires_at_ms,
        } => begin_upload(
            transaction,
            BlobMutationTimes {
                now_ms,
                issued_at_ms,
            },
            key,
            *upload_id,
            *condition,
            content_type.as_deref(),
            metadata,
            *expires_at_ms,
        ),
        BlobMutation::PutPart { .. } => Err(Error::Command(
            "blob part payload must be uploaded to object store",
        )),
        BlobMutation::PutPartRef {
            key,
            upload_id,
            part_number,
            digest,
            size,
        } => put_part_ref(
            transaction,
            now_ms,
            key,
            *upload_id,
            *part_number,
            *digest,
            *size,
        ),
        BlobMutation::Complete {
            key,
            upload_id,
            part_count,
        } => complete_upload(transaction, now_ms, key, *upload_id, *part_count),
        BlobMutation::Abort { key, upload_id } => abort_upload(transaction, key, *upload_id),
        BlobMutation::Delete { key, condition } => delete_blob(transaction, key, *condition),
    }
}

/// Executes one bounded, integrity-checked blob query.
pub fn blob_query(connection: &Connection, query: &BlobQuery) -> Result<BlobQueryResult> {
    match query {
        BlobQuery::Head { key } => Ok(BlobQueryResult::Head(blob_metadata(connection, key)?)),
        BlobQuery::Read { key, offset, limit } => Ok(BlobQueryResult::Read(read_blob(
            connection, key, *offset, *limit,
        )?)),
        BlobQuery::List {
            prefix,
            after,
            limit,
        } => Ok(BlobQueryResult::List(list_blobs(
            connection,
            prefix,
            after.as_deref(),
            *limit,
        )?)),
    }
}

/// Removes bounded abandoned uploads that are not referenced by published objects.
pub fn blob_cleanup_expired(
    transaction: &Transaction<'_>,
    now_ms: i64,
    limit: usize,
) -> Result<usize> {
    validate_now(now_ms)?;
    if limit > 128 {
        return Err(Error::Command("blob cleanup limit exceeds 128"));
    }
    transaction
        .execute(
            "DELETE FROM blob_uploads WHERE upload_id IN (SELECT u.upload_id FROM blob_uploads u INDEXED BY blob_upload_expiry WHERE u.expires_at_ms <= ?1 AND NOT EXISTS (SELECT 1 FROM blob_objects o WHERE o.upload_id = u.upload_id) ORDER BY u.expires_at_ms, u.upload_id LIMIT ?2)",
            (now_ms, limit as i64),
        )
        .map_err(Into::into)
}

fn validate_now(now_ms: i64) -> Result<()> {
    if now_ms < 0 {
        return Err(Error::Command("negative blob logical time"));
    }
    Ok(())
}
