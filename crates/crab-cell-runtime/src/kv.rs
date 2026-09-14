use std::collections::HashSet;

use rusqlite::{Connection, OptionalExtension, Transaction, types::Value};

use crate::{Error, Result};

const KV_SCHEMA: &str = include_str!("migrations/kv.sql");
const MAX_SCOPE_BYTES: usize = 1_024;
const MAX_KEY_BYTES: usize = 1_024;
const MAX_VALUE_BYTES: usize = 65_536;
const MAX_ATOMIC_ITEMS: usize = 128;
const MAX_OPERATION_BYTES: usize = 1 << 20;
const MAX_LIST_ITEMS: usize = 1_000;
const MAX_PAGE_BYTES: usize = 1 << 20;
const VERSION_BYTES: usize = 28;
const CLEANUP_ITEMS: usize = 128;

type RawEntry = (Vec<u8>, Vec<u8>, Vec<u8>, Option<i64>);

/// One condition checked against the logical live value before any KV write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KvCondition {
    Absent,
    Version([u8; VERSION_BYTES]),
}

/// One key precondition in a scoped atomic operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KvCheck {
    pub key: Vec<u8>,
    pub condition: KvCondition,
}

/// One ordered mutation in a scoped atomic operation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KvMutation {
    Put {
        key: Vec<u8>,
        value: Vec<u8>,
        expires_at_ms: Option<i64>,
    },
    Delete {
        key: Vec<u8>,
    },
}

impl KvMutation {
    fn key(&self) -> &[u8] {
        match self {
            Self::Put { key, .. } | Self::Delete { key } => key,
        }
    }
}

/// All checks and ordered writes applied by one runtime command transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KvAtomicRequest {
    pub scope: Vec<u8>,
    pub checks: Vec<KvCheck>,
    pub mutations: Vec<KvMutation>,
}

/// Result for one applied mutation, preserving request order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KvMutationResult {
    pub key: Vec<u8>,
    pub version: Option<[u8; VERSION_BYTES]>,
    pub deleted: bool,
}

/// Business outcome produced inside the runtime's application savepoint.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum KvAtomicOutcome {
    Applied(Vec<KvMutationResult>),
    PreconditionFailed { key: Vec<u8> },
}

/// One live KV entry returned by get or list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KvEntry {
    pub key: Vec<u8>,
    pub value: Vec<u8>,
    pub version: [u8; VERSION_BYTES],
    pub expires_at_ms: Option<i64>,
}

/// One bounded, current-read page within a single scope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KvPage {
    pub entries: Vec<KvEntry>,
    pub next_after: Option<Vec<u8>>,
}

/// Installs the exact version-one KV schema inside bootstrap or migration SQL.
pub fn install_kv_schema(transaction: &Transaction<'_>) -> Result<()> {
    transaction.execute_batch(KV_SCHEMA)?;
    Ok(())
}

/// Checks and mutates one scope atomically inside the caller's runtime command.
pub fn kv_atomic(
    transaction: &Transaction<'_>,
    now_ms: i64,
    request: &KvAtomicRequest,
) -> Result<KvAtomicOutcome> {
    validate_atomic(now_ms, request)?;
    let (incarnation, prior_sequence) = transaction.query_row(
        "SELECT incarnation, commit_sequence FROM sys_meta WHERE singleton = 1",
        [],
        |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, i64>(1)?)),
    )?;
    let incarnation: [u8; 16] = incarnation
        .try_into()
        .map_err(|_| Error::Command("invalid KV runtime incarnation"))?;
    let sequence = prior_sequence
        .checked_add(1)
        .filter(|value| *value > 0)
        .ok_or(Error::Command("KV commit sequence overflow"))?;

    for check in &request.checks {
        let version = live_version(transaction, &request.scope, &check.key, now_ms)?;
        let satisfied = match (&check.condition, version) {
            (KvCondition::Absent, None) => true,
            (KvCondition::Version(expected), Some(actual)) => expected.as_slice() == actual,
            _ => false,
        };
        if !satisfied {
            return Ok(KvAtomicOutcome::PreconditionFailed {
                key: check.key.clone(),
            });
        }
    }

    let mut results = Vec::with_capacity(request.mutations.len());
    for (ordinal, mutation) in request.mutations.iter().enumerate() {
        match mutation {
            KvMutation::Put {
                key,
                value,
                expires_at_ms,
            } => {
                let ordinal =
                    u32::try_from(ordinal).map_err(|_| Error::Command("too many KV mutations"))?;
                let version = kv_version(incarnation, sequence, ordinal)?;
                transaction.execute(
                    "INSERT INTO kv_entries(scope, key, version, value, expires_at_ms) VALUES (?1, ?2, ?3, ?4, ?5) ON CONFLICT(scope, key) DO UPDATE SET version = excluded.version, value = excluded.value, expires_at_ms = excluded.expires_at_ms",
                    (
                        request.scope.as_slice(),
                        key.as_slice(),
                        version.as_slice(),
                        value.as_slice(),
                        expires_at_ms,
                    ),
                )?;
                results.push(KvMutationResult {
                    key: key.clone(),
                    version: Some(version),
                    deleted: false,
                });
            }
            KvMutation::Delete { key } => {
                transaction.execute(
                    "DELETE FROM kv_entries WHERE scope = ?1 AND key = ?2",
                    (request.scope.as_slice(), key.as_slice()),
                )?;
                results.push(KvMutationResult {
                    key: key.clone(),
                    version: None,
                    deleted: true,
                });
            }
        }
    }
    Ok(KvAtomicOutcome::Applied(results))
}

/// Reads one logically live value from a scope at the supplied logical time.
pub fn kv_get(
    connection: &Connection,
    scope: &[u8],
    key: &[u8],
    now_ms: i64,
) -> Result<Option<KvEntry>> {
    validate_scope(scope)?;
    validate_key(key)?;
    validate_now(now_ms)?;
    let row = connection
        .query_row(
            "SELECT key, value, version, expires_at_ms FROM kv_entries WHERE scope = ?1 AND key = ?2 AND (expires_at_ms IS NULL OR expires_at_ms > ?3)",
            (scope, key, now_ms),
            decode_entry,
        )
        .optional()?;
    row.map(validate_entry).transpose()
}

/// Lists a bounded binary-ordered page within one scope and prefix.
pub fn kv_list(
    connection: &Connection,
    scope: &[u8],
    prefix: &[u8],
    after_key: Option<&[u8]>,
    limit: usize,
    now_ms: i64,
) -> Result<KvPage> {
    validate_scope(scope)?;
    if prefix.len() > MAX_KEY_BYTES {
        return Err(Error::Command("KV prefix exceeds 1024 bytes"));
    }
    if let Some(after_key) = after_key {
        validate_key(after_key)?;
    }
    if !(1..=MAX_LIST_ITEMS).contains(&limit) {
        return Err(Error::Command("KV list limit must be in 1..=1000"));
    }
    validate_now(now_ms)?;

    let mut sql = String::from(
        "SELECT key, value, version, expires_at_ms FROM kv_entries WHERE scope = ?1 AND (expires_at_ms IS NULL OR expires_at_ms > ?2)",
    );
    let mut parameters = vec![Value::Blob(scope.to_vec()), Value::Integer(now_ms)];
    if let Some(after_key) = after_key {
        parameters.push(Value::Blob(after_key.to_vec()));
        sql.push_str(&format!(" AND key > ?{}", parameters.len()));
    }
    if !prefix.is_empty() {
        parameters.push(Value::Blob(prefix.to_vec()));
        sql.push_str(&format!(" AND key >= ?{}", parameters.len()));
        if let Some(end) = prefix_successor(prefix) {
            parameters.push(Value::Blob(end));
            sql.push_str(&format!(" AND key < ?{}", parameters.len()));
        }
    }
    parameters.push(Value::Integer(
        i64::try_from(limit + 1).map_err(|_| Error::Command("KV list limit overflow"))?,
    ));
    sql.push_str(&format!(" ORDER BY key LIMIT ?{}", parameters.len()));

    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map(rusqlite::params_from_iter(parameters), decode_entry)?;
    let mut entries = Vec::with_capacity(limit.min(16));
    let mut page_bytes = 0_usize;
    let mut has_more = false;
    for row in rows {
        let entry = validate_entry(row?)?;
        let entry_bytes = entry
            .key
            .len()
            .checked_add(entry.value.len())
            .and_then(|bytes| bytes.checked_add(VERSION_BYTES + 64))
            .ok_or(Error::Command("KV page byte count overflow"))?;
        if entries.len() == limit
            || page_bytes
                .checked_add(entry_bytes)
                .is_none_or(|bytes| bytes > MAX_PAGE_BYTES)
        {
            has_more = true;
            break;
        }
        page_bytes += entry_bytes;
        entries.push(entry);
    }
    let next_after = if has_more {
        entries.last().map(|entry| entry.key.clone())
    } else {
        None
    };
    Ok(KvPage {
        entries,
        next_after,
    })
}

/// Deletes at most 128 physically expired entries in one internal command.
pub fn kv_cleanup_expired(transaction: &Transaction<'_>, now_ms: i64) -> Result<usize> {
    validate_now(now_ms)?;
    let changed = transaction.execute(
        "DELETE FROM kv_entries WHERE (scope, key) IN (SELECT scope, key FROM kv_entries INDEXED BY kv_expiry WHERE expires_at_ms IS NOT NULL AND expires_at_ms <= ?1 ORDER BY expires_at_ms, scope, key LIMIT ?2)",
        (now_ms, CLEANUP_ITEMS as i64),
    )?;
    Ok(changed)
}

fn validate_atomic(now_ms: i64, request: &KvAtomicRequest) -> Result<()> {
    validate_now(now_ms)?;
    validate_scope(&request.scope)?;
    let item_count = request
        .checks
        .len()
        .checked_add(request.mutations.len())
        .ok_or(Error::Command("KV atomic item count overflow"))?;
    if item_count == 0 || item_count > MAX_ATOMIC_ITEMS {
        return Err(Error::Command("KV atomic requires 1..=128 items"));
    }
    let mut operation_bytes = request.scope.len();
    for check in &request.checks {
        validate_key(&check.key)?;
        operation_bytes = operation_bytes
            .checked_add(check.key.len() + VERSION_BYTES + 8)
            .ok_or(Error::Command("KV atomic byte count overflow"))?;
    }
    let mut mutation_keys = HashSet::with_capacity(request.mutations.len());
    for mutation in &request.mutations {
        let key = mutation.key();
        validate_key(key)?;
        if !mutation_keys.insert(key) {
            return Err(Error::Command("duplicate KV mutation key"));
        }
        operation_bytes = operation_bytes
            .checked_add(key.len() + 16)
            .ok_or(Error::Command("KV atomic byte count overflow"))?;
        if let KvMutation::Put {
            value,
            expires_at_ms,
            ..
        } = mutation
        {
            if value.len() > MAX_VALUE_BYTES {
                return Err(Error::Command("KV value exceeds 65536 bytes"));
            }
            operation_bytes = operation_bytes
                .checked_add(value.len())
                .ok_or(Error::Command("KV atomic byte count overflow"))?;
            if expires_at_ms.is_some_and(|expiry| expiry <= now_ms) {
                return Err(Error::Command("KV expiry must be after logical time"));
            }
        }
    }
    if operation_bytes > MAX_OPERATION_BYTES {
        return Err(Error::Command("KV atomic operation exceeds 1 MiB"));
    }
    Ok(())
}

fn validate_scope(scope: &[u8]) -> Result<()> {
    if scope.len() > MAX_SCOPE_BYTES {
        return Err(Error::Command("KV scope exceeds 1024 bytes"));
    }
    Ok(())
}

fn validate_key(key: &[u8]) -> Result<()> {
    if key.is_empty() || key.len() > MAX_KEY_BYTES {
        return Err(Error::Command("KV key must contain 1..=1024 bytes"));
    }
    Ok(())
}

fn validate_now(now_ms: i64) -> Result<()> {
    if now_ms < 0 {
        return Err(Error::Command("negative KV logical time"));
    }
    Ok(())
}

fn live_version(
    transaction: &Transaction<'_>,
    scope: &[u8],
    key: &[u8],
    now_ms: i64,
) -> Result<Option<Vec<u8>>> {
    transaction
        .query_row(
            "SELECT version FROM kv_entries WHERE scope = ?1 AND key = ?2 AND (expires_at_ms IS NULL OR expires_at_ms > ?3)",
            (scope, key, now_ms),
            |row| row.get(0),
        )
        .optional()
        .map_err(Error::from)
}

fn kv_version(incarnation: [u8; 16], sequence: i64, ordinal: u32) -> Result<[u8; VERSION_BYTES]> {
    let sequence = u64::try_from(sequence).map_err(|_| Error::Command("invalid KV sequence"))?;
    let mut version = [0; VERSION_BYTES];
    version[..16].copy_from_slice(&incarnation);
    version[16..24].copy_from_slice(&sequence.to_be_bytes());
    version[24..].copy_from_slice(&ordinal.to_be_bytes());
    Ok(version)
}

fn decode_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawEntry> {
    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
}

fn validate_entry((key, value, version, expires_at_ms): RawEntry) -> Result<KvEntry> {
    validate_key(&key)?;
    if value.len() > MAX_VALUE_BYTES {
        return Err(Error::Command("stored KV value exceeds limit"));
    }
    let version = version
        .try_into()
        .map_err(|_| Error::Command("stored KV version has invalid length"))?;
    Ok(KvEntry {
        key,
        value,
        version,
        expires_at_ms,
    })
}

fn prefix_successor(prefix: &[u8]) -> Option<Vec<u8>> {
    let index = prefix.iter().rposition(|byte| *byte != u8::MAX)?;
    let mut end = prefix[..=index].to_vec();
    end[index] += 1;
    Some(end)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_kv_migration_matches_normative_contract() {
        assert_eq!(
            KV_SCHEMA,
            include_str!("../../../crab/docs/architecture/platform/contracts/kv.sql")
        );
    }

    #[test]
    fn all_ff_prefix_has_no_exclusive_successor() {
        assert_eq!(prefix_successor(&[]), None);
        assert_eq!(prefix_successor(&[0xff, 0xff]), None);
        assert_eq!(prefix_successor(&[0x12, 0xff]), Some(vec![0x13]));
    }
}
