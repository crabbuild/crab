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
    /// Publishes regardless of the current object.
    Any,
    /// Publishes only when no object exists at the key.
    Missing,
    /// Publishes only while the object's ETag still matches.
    Etag([u8; 32]),
}

/// One bounded mutation against a blob namespace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlobMutation {
    /// Opens a bounded multipart upload.
    Begin {
        /// Blob key the upload targets.
        key: Vec<u8>,
        /// Client-chosen upload identity.
        upload_id: [u8; 16],
        /// Publication rule checked when the upload completes.
        condition: BlobCondition,
        /// Content type recorded when the object is published.
        content_type: Option<String>,
        /// Opaque application metadata recorded with the object.
        metadata: Vec<u8>,
        /// Logical time after which an unfinished upload is abandoned.
        expires_at_ms: i64,
    },
    /// Stores one part of an open upload.
    PutPart {
        /// Blob key the upload targets.
        key: Vec<u8>,
        /// Upload the part belongs to.
        upload_id: [u8; 16],
        /// One-based part number.
        part_number: u32,
        /// Part bytes, bounded by `MAX_BLOB_PART_BYTES`.
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
    /// Completes an open upload and publishes the object.
    Complete {
        /// Blob key the upload targets.
        key: Vec<u8>,
        /// Upload to complete.
        upload_id: [u8; 16],
        /// Number of parts the finished object must contain.
        part_count: u32,
    },
    /// Discards an open upload without publishing.
    Abort {
        /// Blob key the upload targets.
        key: Vec<u8>,
        /// Upload to discard.
        upload_id: [u8; 16],
    },
    /// Removes a published object when its condition matches.
    Delete {
        /// Blob key to remove.
        key: Vec<u8>,
        /// Rule checked before the object is removed.
        condition: BlobCondition,
    },
}

/// Business result of a blob mutation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlobMutationOutcome {
    /// The upload is open and accepts parts.
    Begun,
    /// The part was stored.
    PartStored {
        /// Digest the part is stored under.
        digest: [u8; 32],
    },
    /// The object was published.
    Committed {
        /// ETag of the published bytes.
        etag: [u8; 32],
        /// Total object size in bytes.
        size: u64,
    },
    /// The open upload was discarded.
    Aborted,
    /// The object was removed.
    Deleted,
    /// No object or upload matched the request.
    NotFound,
    /// The condition did not match the current object.
    Conflict,
}

/// Immutable metadata for one published blob.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlobMetadata {
    /// Blob key.
    pub key: Vec<u8>,
    /// ETag of the published bytes.
    pub etag: [u8; 32],
    /// Total object size in bytes.
    pub size: u64,
    /// Number of parts the object was assembled from.
    pub part_count: u32,
    /// Content type recorded at completion.
    pub content_type: Option<String>,
    /// Opaque application metadata recorded at completion.
    pub metadata: Vec<u8>,
    /// Logical time the object was first published.
    pub created_at_ms: i64,
    /// Logical time the object was last replaced.
    pub updated_at_ms: i64,
}

/// One bounded blob read result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlobRead {
    /// Metadata of the object the range was read from.
    pub metadata: BlobMetadata,
    /// Byte offset the returned range starts at.
    pub offset: u64,
    /// Bytes of the requested range.
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
    /// Object metadata in lexicographic key order.
    pub objects: Vec<BlobMetadata>,
    /// Key to continue from when the page filled its limit.
    pub next: Option<Vec<u8>>,
}

/// One bounded read against a blob namespace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlobQuery {
    /// Reads one object's metadata.
    Head {
        /// Blob key to look up.
        key: Vec<u8>,
    },
    /// Reads a bounded byte range of one object.
    Read {
        /// Blob key to read.
        key: Vec<u8>,
        /// First byte of the range.
        offset: u64,
        /// Maximum bytes to return.
        limit: u32,
    },
    /// Lists object metadata in key order.
    List {
        /// Key prefix to list.
        prefix: Vec<u8>,
        /// Key to continue after, from a previous page.
        after: Option<Vec<u8>>,
        /// Maximum objects to return.
        limit: u32,
    },
}

/// Result shape for a blob query.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlobQueryResult {
    /// Metadata for the key, absent when no object exists.
    Head(Option<BlobMetadata>),
    /// The bounded range, absent when no object exists.
    Read(Option<BlobRead>),
    /// One page of object metadata.
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
