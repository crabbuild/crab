use rusqlite::{Connection, OptionalExtension, Transaction};

use crate::{Error, Result};

mod api;
#[cfg(test)]
mod tests;

pub use api::{BlobCommand, BlobModule, BlobNamespace, BlobQueryCommand, register_blob};

const BLOB_SCHEMA: &str = include_str!("migrations/blob.sql");
pub(crate) const MAX_BLOB_PART_BYTES: usize = 256 * 1024;
pub(crate) const MAX_BLOB_READ_BYTES: u32 = 512 * 1024;
pub(crate) const MAX_BLOB_PARTS: u32 = 4_096;
const MAX_BLOB_BYTES: u64 = MAX_BLOB_PART_BYTES as u64 * MAX_BLOB_PARTS as u64;
const MAX_KEY_BYTES: usize = 1_024;
const MAX_METADATA_BYTES: usize = 8 * 1_024;
const MAX_CONTENT_TYPE_BYTES: usize = 256;
const MIN_UPLOAD_LIFETIME_MS: i64 = 60_000;
const MAX_UPLOAD_LIFETIME_MS: i64 = 7 * 24 * 60 * 60 * 1_000;

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

/// Installs the exact version-one Blob schema.
pub fn install_blob_schema(transaction: &Transaction<'_>) -> Result<()> {
    transaction.execute_batch(BLOB_SCHEMA)?;
    Ok(())
}

/// Applies one multipart or conditional blob mutation atomically.
pub fn blob_mutate(
    transaction: &Transaction<'_>,
    now_ms: i64,
    mutation: &BlobMutation,
) -> Result<BlobMutationOutcome> {
    validate_now(now_ms)?;
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
            now_ms,
            key,
            *upload_id,
            *condition,
            content_type.as_deref(),
            metadata,
            *expires_at_ms,
        ),
        BlobMutation::PutPart {
            key,
            upload_id,
            part_number,
            payload,
        } => put_part(transaction, now_ms, key, *upload_id, *part_number, payload),
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

fn begin_upload(
    transaction: &Transaction<'_>,
    now_ms: i64,
    key: &[u8],
    upload_id: [u8; 16],
    condition: BlobCondition,
    content_type: Option<&str>,
    metadata: &[u8],
    expires_at_ms: i64,
) -> Result<BlobMutationOutcome> {
    validate_key(key)?;
    validate_metadata(content_type, metadata)?;
    let lifetime = expires_at_ms
        .checked_sub(now_ms)
        .ok_or(Error::Command("blob upload expiry overflow"))?;
    if !(MIN_UPLOAD_LIFETIME_MS..=MAX_UPLOAD_LIFETIME_MS).contains(&lifetime) {
        return Err(Error::Command(
            "blob upload lifetime must be between one minute and seven days",
        ));
    }
    let digest = begin_digest(key, condition, content_type, metadata, expires_at_ms);
    let existing = transaction
        .query_row(
            "SELECT request_digest FROM blob_uploads WHERE upload_id = ?1",
            [upload_id.as_slice()],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()?;
    if let Some(existing) = existing {
        return Ok(if existing.as_slice() == digest {
            BlobMutationOutcome::Begun
        } else {
            BlobMutationOutcome::Conflict
        });
    }
    let (condition_code, expected_etag) = encode_condition(condition);
    transaction.execute(
        "INSERT INTO blob_uploads(upload_id, object_key, request_digest, condition, expected_etag, content_type, metadata, created_at_ms, expires_at_ms, completed, etag, size, part_count) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0, NULL, 0, 0)",
        (
            upload_id.as_slice(),
            key,
            digest.as_slice(),
            condition_code,
            expected_etag.as_ref().map(<[u8; 32]>::as_slice),
            content_type,
            metadata,
            now_ms,
            expires_at_ms,
        ),
    )?;
    Ok(BlobMutationOutcome::Begun)
}

fn put_part(
    transaction: &Transaction<'_>,
    now_ms: i64,
    key: &[u8],
    upload_id: [u8; 16],
    part_number: u32,
    payload: &[u8],
) -> Result<BlobMutationOutcome> {
    validate_key(key)?;
    if !(1..=MAX_BLOB_PARTS).contains(&part_number) {
        return Err(Error::Command("blob part number must be in 1..=4096"));
    }
    if payload.len() > MAX_BLOB_PART_BYTES {
        return Err(Error::Command("blob part exceeds 256 KiB"));
    }
    let upload = upload_state(transaction, upload_id)?;
    let Some((stored_key, completed, expires_at_ms)) = upload else {
        return Ok(BlobMutationOutcome::NotFound);
    };
    if stored_key != key || completed || expires_at_ms <= now_ms {
        return Ok(BlobMutationOutcome::Conflict);
    }
    let digest = part_digest(payload);
    let existing = transaction
        .query_row(
            "SELECT digest FROM blob_parts WHERE upload_id = ?1 AND part_number = ?2",
            (upload_id.as_slice(), i64::from(part_number)),
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()?;
    if let Some(existing) = existing {
        return Ok(if existing.as_slice() == digest {
            BlobMutationOutcome::PartStored { digest }
        } else {
            BlobMutationOutcome::Conflict
        });
    }
    transaction.execute(
        "INSERT INTO blob_parts(upload_id, part_number, digest, payload, byte_offset) VALUES (?1, ?2, ?3, ?4, NULL)",
        (
            upload_id.as_slice(),
            i64::from(part_number),
            digest.as_slice(),
            payload,
        ),
    )?;
    Ok(BlobMutationOutcome::PartStored { digest })
}

fn complete_upload(
    transaction: &Transaction<'_>,
    now_ms: i64,
    key: &[u8],
    upload_id: [u8; 16],
    part_count: u32,
) -> Result<BlobMutationOutcome> {
    validate_key(key)?;
    if !(1..=MAX_BLOB_PARTS).contains(&part_count) {
        return Err(Error::Command(
            "blob completion part count must be in 1..=4096",
        ));
    }
    let upload = transaction
        .query_row(
            "SELECT object_key, condition, expected_etag, content_type, metadata, created_at_ms, expires_at_ms, completed, etag, size, part_count FROM blob_uploads WHERE upload_id = ?1",
            [upload_id.as_slice()],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?, row.get::<_, i64>(1)?,
                    row.get::<_, Option<Vec<u8>>>(2)?, row.get::<_, Option<String>>(3)?,
                    row.get::<_, Vec<u8>>(4)?, row.get::<_, i64>(5)?, row.get::<_, i64>(6)?,
                    row.get::<_, i64>(7)?, row.get::<_, Option<Vec<u8>>>(8)?,
                    row.get::<_, i64>(9)?, row.get::<_, i64>(10)?,
                ))
            },
        )
        .optional()?;
    let Some((
        stored_key,
        condition,
        expected,
        content_type,
        metadata,
        created_at_ms,
        expires_at_ms,
        completed,
        stored_etag,
        stored_size,
        stored_parts,
    )) = upload
    else {
        return Ok(BlobMutationOutcome::NotFound);
    };
    if stored_key != key {
        return Ok(BlobMutationOutcome::Conflict);
    }
    if completed != 0 {
        let etag = exact_etag(stored_etag)?;
        return Ok(if stored_parts == i64::from(part_count) {
            BlobMutationOutcome::Committed {
                etag,
                size: nonnegative_u64(stored_size, "invalid stored blob size")?,
            }
        } else {
            BlobMutationOutcome::Conflict
        });
    }
    if expires_at_ms <= now_ms {
        return Ok(BlobMutationOutcome::Conflict);
    }
    let condition = decode_condition(condition, expected)?;
    if !condition_matches(transaction, key, condition)? {
        return Ok(BlobMutationOutcome::Conflict);
    }
    let mut statement = transaction.prepare(
        "SELECT part_number, digest, length(payload) FROM blob_parts WHERE upload_id = ?1 ORDER BY part_number",
    )?;
    let rows = statement.query_map([upload_id.as_slice()], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, Vec<u8>>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;
    let mut parts = Vec::with_capacity(part_count as usize);
    for row in rows {
        parts.push(row?);
    }
    drop(statement);
    if parts.len() != part_count as usize {
        return Ok(BlobMutationOutcome::Conflict);
    }
    let mut manifest = blake3::Hasher::new();
    manifest.update(b"crab.blob.v1\0");
    let mut offset = 0_u64;
    for (index, (number, digest, payload_len)) in parts.iter().enumerate() {
        if *number != (index + 1) as i64 || digest.len() != 32 || *payload_len < 0 {
            return Err(Error::Command("invalid stored blob part"));
        }
        manifest.update(&number.to_be_bytes());
        let payload_len = u32::try_from(*payload_len)
            .map_err(|_| Error::Command("invalid stored blob part length"))?;
        manifest.update(&payload_len.to_be_bytes());
        manifest.update(digest);
        transaction.execute(
            "UPDATE blob_parts SET byte_offset = ?1 WHERE upload_id = ?2 AND part_number = ?3 AND byte_offset IS NULL",
            (offset as i64, upload_id.as_slice(), *number),
        )?;
        offset = offset
            .checked_add(u64::from(payload_len))
            .ok_or(Error::Command("blob size overflow"))?;
    }
    if offset > MAX_BLOB_BYTES {
        return Err(Error::Command("blob exceeds one GiB"));
    }
    let etag = *manifest.finalize().as_bytes();
    let prior_upload = transaction
        .query_row(
            "SELECT upload_id FROM blob_objects WHERE object_key = ?1",
            [key],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()?;
    transaction.execute(
        "INSERT INTO blob_objects(object_key, upload_id, etag, size, part_count, content_type, metadata, created_at_ms, updated_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) ON CONFLICT(object_key) DO UPDATE SET upload_id = excluded.upload_id, etag = excluded.etag, size = excluded.size, part_count = excluded.part_count, content_type = excluded.content_type, metadata = excluded.metadata, updated_at_ms = excluded.updated_at_ms",
        (
            key, upload_id.as_slice(), etag.as_slice(), offset as i64,
            i64::from(part_count), content_type, metadata, created_at_ms, now_ms,
        ),
    )?;
    transaction.execute(
        "UPDATE blob_uploads SET completed = 1, etag = ?1, size = ?2, part_count = ?3 WHERE upload_id = ?4 AND completed = 0",
        (etag.as_slice(), offset as i64, i64::from(part_count), upload_id.as_slice()),
    )?;
    if let Some(prior) = prior_upload.filter(|prior| prior.as_slice() != upload_id) {
        transaction.execute("DELETE FROM blob_uploads WHERE upload_id = ?1", [prior])?;
    }
    Ok(BlobMutationOutcome::Committed { etag, size: offset })
}

fn abort_upload(
    transaction: &Transaction<'_>,
    key: &[u8],
    upload_id: [u8; 16],
) -> Result<BlobMutationOutcome> {
    validate_key(key)?;
    let changed = transaction.execute(
        "DELETE FROM blob_uploads WHERE upload_id = ?1 AND object_key = ?2 AND completed = 0",
        (upload_id.as_slice(), key),
    )?;
    Ok(if changed == 1 {
        BlobMutationOutcome::Aborted
    } else {
        BlobMutationOutcome::NotFound
    })
}

fn delete_blob(
    transaction: &Transaction<'_>,
    key: &[u8],
    condition: BlobCondition,
) -> Result<BlobMutationOutcome> {
    validate_key(key)?;
    if !condition_matches(transaction, key, condition)? {
        return Ok(BlobMutationOutcome::Conflict);
    }
    let upload = transaction
        .query_row(
            "SELECT upload_id FROM blob_objects WHERE object_key = ?1",
            [key],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()?;
    let Some(upload) = upload else {
        return Ok(BlobMutationOutcome::NotFound);
    };
    transaction.execute("DELETE FROM blob_objects WHERE object_key = ?1", [key])?;
    transaction.execute("DELETE FROM blob_uploads WHERE upload_id = ?1", [upload])?;
    Ok(BlobMutationOutcome::Deleted)
}

fn read_blob(
    connection: &Connection,
    key: &[u8],
    offset: u64,
    limit: u32,
) -> Result<Option<BlobRead>> {
    let Some(metadata) = blob_metadata(connection, key)? else {
        return Ok(None);
    };
    if limit > MAX_BLOB_READ_BYTES {
        return Err(Error::Command("blob read exceeds 512 KiB"));
    }
    if offset >= metadata.size || limit == 0 {
        return Ok(Some(BlobRead {
            metadata,
            offset,
            bytes: Vec::new(),
        }));
    }
    let end = offset.saturating_add(u64::from(limit)).min(metadata.size);
    let upload_id = connection.query_row(
        "SELECT upload_id FROM blob_objects WHERE object_key = ?1",
        [key],
        |row| row.get::<_, Vec<u8>>(0),
    )?;
    let mut statement = connection.prepare(
        "SELECT byte_offset, digest, payload FROM blob_parts WHERE upload_id = ?1 AND byte_offset < ?2 AND byte_offset + length(payload) > ?3 ORDER BY part_number",
    )?;
    let rows = statement.query_map((upload_id, end as i64, offset as i64), |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, Vec<u8>>(1)?,
            row.get::<_, Vec<u8>>(2)?,
        ))
    })?;
    let mut bytes = Vec::with_capacity((end - offset) as usize);
    for row in rows {
        let (part_offset, digest, payload) = row?;
        if part_offset < 0 || digest.as_slice() != part_digest(&payload) {
            return Err(Error::Command("blob part integrity check failed"));
        }
        let part_offset = part_offset as u64;
        let start = offset.saturating_sub(part_offset) as usize;
        let take_end = (end.saturating_sub(part_offset) as usize).min(payload.len());
        bytes.extend_from_slice(&payload[start..take_end]);
    }
    if bytes.len() != (end - offset) as usize {
        return Err(Error::Command("blob range is incomplete"));
    }
    Ok(Some(BlobRead {
        metadata,
        offset,
        bytes,
    }))
}

fn blob_metadata(connection: &Connection, key: &[u8]) -> Result<Option<BlobMetadata>> {
    validate_key(key)?;
    connection
        .query_row(
            "SELECT etag, size, part_count, content_type, metadata, created_at_ms, updated_at_ms FROM blob_objects WHERE object_key = ?1",
            [key],
            |row| decode_metadata(key.to_vec(), row),
        )
        .optional()
        .map_err(Into::into)
}

fn list_blobs(
    connection: &Connection,
    prefix: &[u8],
    after: Option<&[u8]>,
    limit: u32,
) -> Result<BlobPage> {
    if prefix.len() > MAX_KEY_BYTES || after.is_some_and(|value| value.len() > MAX_KEY_BYTES) {
        return Err(Error::Command("blob list key exceeds 1024 bytes"));
    }
    if !(1..=128).contains(&limit) {
        return Err(Error::Command("blob list limit must be in 1..=128"));
    }
    let after = after.unwrap_or_default();
    let candidates = if let Some(upper) = prefix_successor(prefix) {
        let mut statement = connection.prepare(
            "SELECT object_key, etag, size, part_count, content_type, metadata, created_at_ms, updated_at_ms FROM blob_objects WHERE object_key > ?1 AND object_key >= ?2 AND object_key < ?3 ORDER BY object_key LIMIT ?4",
        )?;
        statement
            .query_map((after, prefix, upper, i64::from(limit) + 1), |row| {
                let key = row.get::<_, Vec<u8>>(0)?;
                decode_metadata(key, row)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
    } else {
        let mut statement = connection.prepare(
            "SELECT object_key, etag, size, part_count, content_type, metadata, created_at_ms, updated_at_ms FROM blob_objects WHERE object_key > ?1 AND object_key >= ?2 ORDER BY object_key LIMIT ?3",
        )?;
        statement
            .query_map((after, prefix, i64::from(limit) + 1), |row| {
                let key = row.get::<_, Vec<u8>>(0)?;
                decode_metadata(key, row)
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
    };
    let mut objects = Vec::with_capacity(limit as usize);
    let mut bytes = 0_usize;
    let mut truncated = false;
    for object in candidates {
        let object_bytes = object
            .key
            .len()
            .checked_add(object.metadata.len())
            .and_then(|value| {
                value.checked_add(object.content_type.as_ref().map_or(0, String::len))
            })
            .and_then(|value| value.checked_add(96))
            .ok_or(Error::Command("blob list byte count overflow"))?;
        if objects.len() == limit as usize || bytes.saturating_add(object_bytes) > 512 * 1024 {
            truncated = true;
            break;
        }
        bytes += object_bytes;
        objects.push(object);
    }
    let next = truncated
        .then(|| objects.last().map(|object| object.key.clone()))
        .flatten();
    Ok(BlobPage { objects, next })
}

fn prefix_successor(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut upper = prefix.to_vec();
    let index = upper.iter().rposition(|byte| *byte != u8::MAX)?;
    upper[index] += 1;
    upper.truncate(index + 1);
    Some(upper)
}

fn decode_metadata(key: Vec<u8>, row: &rusqlite::Row<'_>) -> rusqlite::Result<BlobMetadata> {
    let offset = usize::from(row.as_ref().column_count() == 8);
    let etag = row.get::<_, Vec<u8>>(offset)?;
    let etag: [u8; 32] = etag.try_into().map_err(|_| rusqlite::Error::InvalidQuery)?;
    let size = row.get::<_, i64>(offset + 1)?;
    let part_count = row.get::<_, i64>(offset + 2)?;
    Ok(BlobMetadata {
        key,
        etag,
        size: u64::try_from(size).map_err(|_| rusqlite::Error::InvalidQuery)?,
        part_count: u32::try_from(part_count).map_err(|_| rusqlite::Error::InvalidQuery)?,
        content_type: row.get(offset + 3)?,
        metadata: row.get(offset + 4)?,
        created_at_ms: row.get(offset + 5)?,
        updated_at_ms: row.get(offset + 6)?,
    })
}

fn upload_state(
    transaction: &Transaction<'_>,
    upload_id: [u8; 16],
) -> Result<Option<(Vec<u8>, bool, i64)>> {
    transaction
        .query_row(
            "SELECT object_key, completed, expires_at_ms FROM blob_uploads WHERE upload_id = ?1",
            [upload_id.as_slice()],
            |row| Ok((row.get(0)?, row.get::<_, i64>(1)? != 0, row.get(2)?)),
        )
        .optional()
        .map_err(Into::into)
}

fn condition_matches(
    transaction: &Transaction<'_>,
    key: &[u8],
    condition: BlobCondition,
) -> Result<bool> {
    let current = transaction
        .query_row(
            "SELECT etag FROM blob_objects WHERE object_key = ?1",
            [key],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()?;
    Ok(match condition {
        BlobCondition::Any => true,
        BlobCondition::Missing => current.is_none(),
        BlobCondition::Etag(expected) => current.as_deref() == Some(expected.as_slice()),
    })
}

fn encode_condition(condition: BlobCondition) -> (i64, Option<[u8; 32]>) {
    match condition {
        BlobCondition::Any => (0, None),
        BlobCondition::Missing => (1, None),
        BlobCondition::Etag(etag) => (2, Some(etag)),
    }
}

fn decode_condition(code: i64, etag: Option<Vec<u8>>) -> Result<BlobCondition> {
    match (code, etag) {
        (0, None) => Ok(BlobCondition::Any),
        (1, None) => Ok(BlobCondition::Missing),
        (2, Some(etag)) => {
            Ok(BlobCondition::Etag(etag.try_into().map_err(|_| {
                Error::Command("invalid stored blob condition")
            })?))
        }
        _ => Err(Error::Command("invalid stored blob condition")),
    }
}

fn begin_digest(
    key: &[u8],
    condition: BlobCondition,
    content_type: Option<&str>,
    metadata: &[u8],
    expires_at_ms: i64,
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab.blob-upload.v1\0");
    hasher.update(key);
    let (code, etag) = encode_condition(condition);
    hasher.update(&code.to_be_bytes());
    if let Some(etag) = etag {
        hasher.update(&etag);
    }
    if let Some(content_type) = content_type {
        hasher.update(content_type.as_bytes());
    }
    hasher.update(metadata);
    hasher.update(&expires_at_ms.to_be_bytes());
    *hasher.finalize().as_bytes()
}

fn part_digest(payload: &[u8]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab.blob-part.v1\0");
    hasher.update(payload);
    *hasher.finalize().as_bytes()
}

fn validate_key(key: &[u8]) -> Result<()> {
    if key.is_empty() || key.len() > MAX_KEY_BYTES {
        return Err(Error::Command("blob key must be 1..=1024 bytes"));
    }
    Ok(())
}

fn validate_metadata(content_type: Option<&str>, metadata: &[u8]) -> Result<()> {
    if metadata.len() > MAX_METADATA_BYTES
        || content_type
            .is_some_and(|value| value.is_empty() || value.len() > MAX_CONTENT_TYPE_BYTES)
    {
        return Err(Error::Command("blob metadata exceeds limits"));
    }
    Ok(())
}

fn exact_etag(value: Option<Vec<u8>>) -> Result<[u8; 32]> {
    value
        .ok_or(Error::Command("completed blob upload lacks ETag"))?
        .try_into()
        .map_err(|_| Error::Command("invalid stored blob ETag"))
}

fn nonnegative_u64(value: i64, message: &'static str) -> Result<u64> {
    u64::try_from(value).map_err(|_| Error::Command(message))
}

fn validate_now(now_ms: i64) -> Result<()> {
    if now_ms < 0 {
        return Err(Error::Command("negative blob logical time"));
    }
    Ok(())
}
