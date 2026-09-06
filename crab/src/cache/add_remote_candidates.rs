//! Persistent advisory cache for proof-backed remote chunk candidates.
//!
//! Entries are scoped to one bucket and global content prefix. They only
//! accelerate `crab add`; push revalidates every placement and origin proof
//! before adopting it.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lru::LruCache;
use rusqlite::{Connection, params, params_from_iter};

use crate::core::error::{CrabError, Result};
use crab_staging::push_plan::ExistingChunkCandidate;
use crab_xet::hash::MerkleHash;
use crab_xet::xorb::format::XorbRef;

const SCHEMA_VERSION: i64 = 1;
const MEMORY_CAPACITY: usize = 65_536;
const MAX_PERSISTENT_ENTRIES: i64 = 2_000_000;
const LOOKUP_BATCH_SIZE: usize = 512;
const PRUNE_EVERY_WRITES: u64 = 64;
const NEGATIVE_TABLE: &str = "remote_candidate_misses_v1";
const NEGATIVE_TTL: Duration = Duration::from_secs(5 * 60);

/// A bounded bucket-scoped cache of proof-backed candidates.
pub(crate) struct AddRemoteCandidateCache {
    connection: Mutex<Connection>,
    memory: Mutex<LruCache<MerkleHash, Option<ExistingChunkCandidate>>>,
    writes_since_prune: std::sync::atomic::AtomicU64,
}

impl AddRemoteCandidateCache {
    /// Open or create the cache. Cache failures are handled by the caller as
    /// an advisory miss; they must never make `crab add` fail.
    pub(crate) fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            crab_cache::ensure_private_cache_directory(parent).map_err(|error| {
                CrabError::Internal(format!(
                    "prepare private add remote candidate cache directory {}: {error}",
                    parent.display()
                ))
            })?;
        }
        let connection = Connection::open(path).map_err(|error| database_error("open", error))?;
        connection
            .busy_timeout(Duration::from_millis(250))
            .map_err(|error| database_error("configure timeout", error))?;
        connection
            .execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")
            .map_err(|error| database_error("configure", error))?;
        let version = connection
            .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
            .map_err(|error| database_error("read schema version", error))?;
        if version == 0 {
            connection
                .execute_batch(
                    "BEGIN IMMEDIATE;
                     CREATE TABLE remote_candidates_v1 (
                         chunk_hash BLOB PRIMARY KEY NOT NULL CHECK(length(chunk_hash) = 32),
                         xorb_hash BLOB NOT NULL CHECK(length(xorb_hash) = 32),
                         chunk_index INTEGER NOT NULL CHECK(chunk_index >= 0),
                         uncompressed_size INTEGER NOT NULL CHECK(uncompressed_size >= 0),
                         placement_id BLOB NOT NULL CHECK(length(placement_id) = 32),
                         origin_proof_id BLOB NOT NULL CHECK(length(origin_proof_id) = 32)
                     ) WITHOUT ROWID;
                     CREATE TABLE remote_candidate_misses_v1 (
                         chunk_hash BLOB PRIMARY KEY NOT NULL CHECK(length(chunk_hash) = 32),
                         observed_at INTEGER NOT NULL CHECK(observed_at >= 0)
                     ) WITHOUT ROWID;
                     PRAGMA user_version = 1;
                     COMMIT;",
                )
                .map_err(|error| database_error("initialize", error))?;
        } else if version != SCHEMA_VERSION {
            return Err(CrabError::Internal(format!(
                "unsupported add remote candidate cache schema {version}; expected v{SCHEMA_VERSION}"
            )));
        }
        connection
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS remote_candidate_misses_v1 (
                     chunk_hash BLOB PRIMARY KEY NOT NULL CHECK(length(chunk_hash) = 32),
                     observed_at INTEGER NOT NULL CHECK(observed_at >= 0)
                 ) WITHOUT ROWID;",
            )
            .map_err(|error| database_error("initialize negative entries", error))?;
        ensure_negative_timestamp_column(&connection)?;
        let capacity = NonZeroUsize::new(MEMORY_CAPACITY).unwrap_or(NonZeroUsize::MIN);
        Ok(Self {
            connection: Mutex::new(connection),
            memory: Mutex::new(LruCache::new(capacity)),
            writes_since_prune: std::sync::atomic::AtomicU64::new(0),
        })
    }

    pub(crate) fn memory_get(
        &self,
        hash: &MerkleHash,
    ) -> Result<Option<Option<ExistingChunkCandidate>>> {
        let mut memory = self
            .memory
            .lock()
            .map_err(|_| CrabError::Internal("add remote candidate cache poisoned".into()))?;
        Ok(memory.get(hash).copied())
    }

    pub(crate) fn memory_get_batch(
        &self,
        hashes: &[MerkleHash],
    ) -> Result<HashMap<MerkleHash, Option<ExistingChunkCandidate>>> {
        if hashes.is_empty() {
            return Ok(HashMap::new());
        }
        let mut memory = self
            .memory
            .lock()
            .map_err(|_| CrabError::Internal("add remote candidate cache poisoned".into()))?;
        let mut out = HashMap::with_capacity(hashes.len());
        for hash in hashes {
            if let Some(candidate) = memory.get(hash).copied() {
                out.insert(*hash, candidate);
            }
        }
        Ok(out)
    }

    pub(crate) fn memory_insert(
        &self,
        hash: MerkleHash,
        candidate: Option<ExistingChunkCandidate>,
    ) -> Result<()> {
        let mut memory = self
            .memory
            .lock()
            .map_err(|_| CrabError::Internal("add remote candidate cache poisoned".into()))?;
        memory.put(hash, candidate);
        Ok(())
    }

    pub(crate) fn memory_insert_batch(
        &self,
        entries: &[(MerkleHash, Option<ExistingChunkCandidate>)],
    ) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let mut memory = self
            .memory
            .lock()
            .map_err(|_| CrabError::Internal("add remote candidate cache poisoned".into()))?;
        for (hash, candidate) in entries {
            memory.put(*hash, *candidate);
        }
        Ok(())
    }

    pub(crate) fn load_persistent(
        &self,
        hashes: &[MerkleHash],
    ) -> Result<HashMap<MerkleHash, Option<ExistingChunkCandidate>>> {
        if hashes.is_empty() {
            return Ok(HashMap::new());
        }
        let now = current_unix_timestamp()?;
        let ttl = i64::try_from(NEGATIVE_TTL.as_secs())
            .map_err(|_| CrabError::Internal("negative cache TTL exceeds sqlite range".into()))?;
        let cutoff = now.saturating_sub(ttl);
        let connection = self
            .connection
            .lock()
            .map_err(|_| CrabError::Internal("add remote candidate database poisoned".into()))?;
        let mut out = HashMap::with_capacity(hashes.len());
        for batch in hashes.chunks(LOOKUP_BATCH_SIZE) {
            let placeholders = std::iter::repeat_n("?", batch.len())
                .collect::<Vec<_>>()
                .join(",");
            let query = format!(
                "SELECT chunk_hash, xorb_hash, chunk_index, uncompressed_size,
                        placement_id, origin_proof_id
                 FROM remote_candidates_v1 WHERE chunk_hash IN ({placeholders})"
            );
            let values = batch
                .iter()
                .map(|hash| <[u8; 32]>::from(*hash).to_vec())
                .collect::<Vec<_>>();
            let mut statement = connection
                .prepare(&query)
                .map_err(|error| database_error("prepare lookup", error))?;
            let rows = statement
                .query_map(
                    params_from_iter(values.iter().map(|value| value.as_slice())),
                    |row| {
                        let chunk_hash = decode_hash(row.get(0)?)?;
                        let xorb_hash = decode_hash(row.get(1)?)?;
                        let chunk_index = row.get::<_, i64>(2)?;
                        let uncompressed_size = row.get::<_, i64>(3)?;
                        let placement_id = decode_hash(row.get(4)?)?;
                        let origin_proof_id = decode_hash(row.get(5)?)?;
                        let chunk_index = u32::try_from(chunk_index).map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                2,
                                rusqlite::types::Type::Integer,
                                Box::new(error),
                            )
                        })?;
                        let uncompressed_size =
                            u32::try_from(uncompressed_size).map_err(|error| {
                                rusqlite::Error::FromSqlConversionFailure(
                                    3,
                                    rusqlite::types::Type::Integer,
                                    Box::new(error),
                                )
                            })?;
                        Ok((
                            chunk_hash,
                            ExistingChunkCandidate {
                                xorb_ref: XorbRef {
                                    xorb_hash: xorb_hash.into(),
                                    chunk_index,
                                    uncompressed_size,
                                },
                                placement_id,
                                origin_proof_id,
                            },
                        ))
                    },
                )
                .map_err(|error| database_error("lookup", error))?;
            for row in rows {
                let (chunk_hash, candidate) =
                    row.map_err(|error| database_error("decode lookup", error))?;
                out.insert(chunk_hash.into(), Some(candidate));
            }

            let negative_query = format!(
                "SELECT chunk_hash, observed_at FROM {NEGATIVE_TABLE}
                 WHERE chunk_hash IN ({placeholders})"
            );
            let mut statement = connection
                .prepare(&negative_query)
                .map_err(|error| database_error("prepare negative lookup", error))?;
            let rows = statement
                .query_map(
                    params_from_iter(values.iter().map(|value| value.as_slice())),
                    |row| Ok((decode_hash(row.get(0)?)?, row.get::<_, i64>(1)?)),
                )
                .map_err(|error| database_error("negative lookup", error))?;
            let mut expired = Vec::new();
            for row in rows {
                let (chunk_hash, observed_at) =
                    row.map_err(|error| database_error("decode negative lookup", error))?;
                if observed_at >= cutoff && observed_at <= now {
                    out.entry(chunk_hash.into()).or_insert(None);
                } else {
                    expired.push(chunk_hash);
                }
            }
            if !expired.is_empty() {
                let mut statement = connection
                    .prepare_cached(&format!(
                        "DELETE FROM {NEGATIVE_TABLE} WHERE chunk_hash = ?1"
                    ))
                    .map_err(|error| database_error("prepare expired negative delete", error))?;
                for chunk_hash in expired {
                    statement
                        .execute(params![chunk_hash.as_slice()])
                        .map_err(|error| database_error("delete expired negative", error))?;
                }
            }
        }
        Ok(out)
    }

    pub(crate) fn persist_results(
        &self,
        entries: &[(MerkleHash, Option<ExistingChunkCandidate>)],
    ) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let observed_at = current_unix_timestamp()?;
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| CrabError::Internal("add remote candidate database poisoned".into()))?;
        let transaction = connection
            .transaction()
            .map_err(|error| database_error("begin update", error))?;
        {
            let mut positive_statement = transaction
                .prepare_cached(
                    "INSERT INTO remote_candidates_v1
                         (chunk_hash, xorb_hash, chunk_index, uncompressed_size,
                          placement_id, origin_proof_id)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT(chunk_hash) DO UPDATE SET
                         xorb_hash = excluded.xorb_hash,
                         chunk_index = excluded.chunk_index,
                         uncompressed_size = excluded.uncompressed_size,
                         placement_id = excluded.placement_id,
                         origin_proof_id = excluded.origin_proof_id
                     WHERE remote_candidates_v1.xorb_hash != excluded.xorb_hash
                        OR remote_candidates_v1.chunk_index != excluded.chunk_index
                        OR remote_candidates_v1.uncompressed_size != excluded.uncompressed_size
                        OR remote_candidates_v1.placement_id != excluded.placement_id
                        OR remote_candidates_v1.origin_proof_id != excluded.origin_proof_id",
                )
                .map_err(|error| database_error("prepare update", error))?;
            let negative_table = NEGATIVE_TABLE;
            let mut negative_statement = transaction
                .prepare_cached(&format!(
                    "INSERT INTO {negative_table} (chunk_hash, observed_at) VALUES (?1, ?2)
                     ON CONFLICT(chunk_hash) DO UPDATE SET observed_at = excluded.observed_at"
                ))
                .map_err(|error| database_error("prepare negative update", error))?;
            let mut delete_negative_statement = transaction
                .prepare_cached(&format!(
                    "DELETE FROM {negative_table} WHERE chunk_hash = ?1"
                ))
                .map_err(|error| database_error("prepare negative delete", error))?;
            let mut delete_positive_statement = transaction
                .prepare_cached("DELETE FROM remote_candidates_v1 WHERE chunk_hash = ?1")
                .map_err(|error| database_error("prepare positive delete", error))?;
            for (chunk_hash, candidate) in entries {
                let chunk_hash = <[u8; 32]>::from(*chunk_hash);
                if let Some(candidate) = candidate {
                    let xorb_hash = <[u8; 32]>::from(candidate.xorb_ref.xorb_hash);
                    positive_statement
                        .execute(params![
                            chunk_hash.as_slice(),
                            xorb_hash.as_slice(),
                            i64::from(candidate.xorb_ref.chunk_index),
                            i64::from(candidate.xorb_ref.uncompressed_size),
                            candidate.placement_id.as_slice(),
                            candidate.origin_proof_id.as_slice(),
                        ])
                        .map_err(|error| database_error("write update", error))?;
                    delete_negative_statement
                        .execute(params![chunk_hash.as_slice()])
                        .map_err(|error| database_error("delete negative update", error))?;
                } else {
                    delete_positive_statement
                        .execute(params![chunk_hash.as_slice()])
                        .map_err(|error| database_error("delete positive update", error))?;
                    negative_statement
                        .execute(params![chunk_hash.as_slice(), observed_at])
                        .map_err(|error| database_error("write negative update", error))?;
                }
            }
        }
        transaction
            .commit()
            .map_err(|error| database_error("commit update", error))?;
        let writes = self
            .writes_since_prune
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        if !writes.is_multiple_of(PRUNE_EVERY_WRITES) {
            return Ok(());
        }
        let positive_count: i64 = connection
            .query_row("SELECT COUNT(*) FROM remote_candidates_v1", [], |row| {
                row.get(0)
            })
            .map_err(|error| database_error("count entries", error))?;
        let negative_count: i64 = connection
            .query_row(
                &format!("SELECT COUNT(*) FROM {NEGATIVE_TABLE}"),
                [],
                |row| row.get(0),
            )
            .map_err(|error| database_error("count negative entries", error))?;
        let mut overflow = positive_count
            .saturating_add(negative_count)
            .saturating_sub(MAX_PERSISTENT_ENTRIES);
        if overflow > 0 {
            let negative_eviction = overflow.min(negative_count);
            if negative_eviction > 0 {
                connection
                    .execute(
                        &format!(
                            "DELETE FROM {NEGATIVE_TABLE}
                             WHERE chunk_hash IN (
                                 SELECT chunk_hash FROM {NEGATIVE_TABLE}
                                 ORDER BY chunk_hash LIMIT ?1
                             )"
                        ),
                        params![negative_eviction],
                    )
                    .map_err(|error| database_error("evict negative entries", error))?;
                overflow -= negative_eviction;
            }
            if overflow > 0 {
                connection
                    .execute(
                        "DELETE FROM remote_candidates_v1
                         WHERE chunk_hash IN (
                             SELECT chunk_hash FROM remote_candidates_v1
                             ORDER BY chunk_hash LIMIT ?1
                         )",
                        params![overflow],
                    )
                    .map_err(|error| database_error("evict entries", error))?;
            }
        }
        Ok(())
    }
}

fn decode_hash(value: Vec<u8>) -> rusqlite::Result<[u8; 32]> {
    value.try_into().map_err(|value: Vec<u8>| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Blob,
            Box::new(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("expected 32-byte hash, got {} bytes", value.len()),
            )),
        )
    })
}

fn database_error(operation: &str, error: rusqlite::Error) -> CrabError {
    CrabError::Internal(format!("{operation} add remote candidate cache: {error}"))
}

fn ensure_negative_timestamp_column(connection: &Connection) -> Result<()> {
    let mut statement = connection
        .prepare("PRAGMA table_info(remote_candidate_misses_v1)")
        .map_err(|error| database_error("inspect negative schema", error))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(|error| database_error("inspect negative columns", error))?;
    let column_names = columns
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| database_error("decode negative schema", error))?;
    drop(statement);
    let has_timestamp = column_names.iter().any(|name| name == "observed_at");
    if has_timestamp {
        return Ok(());
    }
    connection
        .execute(
            "ALTER TABLE remote_candidate_misses_v1
             ADD COLUMN observed_at INTEGER NOT NULL DEFAULT 0",
            [],
        )
        .map_err(|error| database_error("upgrade negative schema", error))?;
    Ok(())
}

fn current_unix_timestamp() -> Result<i64> {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| CrabError::Internal(format!("read add cache clock: {error}")))?
        .as_secs();
    i64::try_from(seconds)
        .map_err(|_| CrabError::Internal("add cache clock exceeds sqlite integer range".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn candidate(seed: u8) -> ExistingChunkCandidate {
        ExistingChunkCandidate {
            xorb_ref: XorbRef {
                xorb_hash: MerkleHash::from([seed; 32]),
                chunk_index: u32::from(seed),
                uncompressed_size: 4096,
            },
            placement_id: [seed.wrapping_add(1); 32],
            origin_proof_id: [seed.wrapping_add(2); 32],
        }
    }

    #[test]
    fn persistent_candidates_round_trip() {
        let dir = tempdir().expect("tempdir");
        let path = dir.path().join("remote-candidates.sqlite");
        let cache = AddRemoteCandidateCache::open(&path).expect("open");
        let hash = MerkleHash::from([7; 32]);
        cache
            .persist_results(&[(hash, Some(candidate(7)))])
            .expect("persist");
        cache
            .persist_results(&[(hash, Some(candidate(8)))])
            .expect("refresh");
        let loaded = cache.load_persistent(&[hash]).expect("load");
        assert_eq!(loaded.get(&hash), Some(&Some(candidate(8))));
    }

    #[test]
    fn persistent_negative_entries_round_trip_and_refresh() {
        let dir = tempdir().expect("tempdir");
        let cache = AddRemoteCandidateCache::open(&dir.path().join("cache.sqlite")).expect("open");
        let hash = MerkleHash::from([4; 32]);
        cache
            .persist_results(&[(hash, None)])
            .expect("persist negative");
        assert_eq!(
            cache.load_persistent(&[hash]).expect("load").get(&hash),
            Some(&None)
        );

        cache
            .persist_results(&[(hash, Some(candidate(4)))])
            .expect("refresh positive");
        assert_eq!(
            cache.load_persistent(&[hash]).expect("load").get(&hash),
            Some(&Some(candidate(4)))
        );
    }

    #[test]
    fn expired_negative_entries_are_not_reused() {
        let dir = tempdir().expect("tempdir");
        let cache = AddRemoteCandidateCache::open(&dir.path().join("cache.sqlite")).expect("open");
        let hash = MerkleHash::from([5; 32]);
        cache
            .persist_results(&[(hash, None)])
            .expect("persist negative");
        cache
            .connection
            .lock()
            .expect("database lock")
            .execute(
                "UPDATE remote_candidate_misses_v1 SET observed_at = 0 WHERE chunk_hash = ?1",
                params![<[u8; 32]>::from(hash).as_slice()],
            )
            .expect("age negative");

        assert!(
            !cache
                .load_persistent(&[hash])
                .expect("load")
                .contains_key(&hash)
        );
        let remaining: i64 = cache
            .connection
            .lock()
            .expect("database lock")
            .query_row(
                "SELECT COUNT(*) FROM remote_candidate_misses_v1 WHERE chunk_hash = ?1",
                params![<[u8; 32]>::from(hash).as_slice()],
                |row| row.get(0),
            )
            .expect("count expired");
        assert_eq!(remaining, 0);
    }

    #[test]
    fn memory_cache_distinguishes_negative_entries() {
        let dir = tempdir().expect("tempdir");
        let cache = AddRemoteCandidateCache::open(&dir.path().join("cache.sqlite")).expect("open");
        let hash = MerkleHash::from([3; 32]);
        assert_eq!(cache.memory_get(&hash).expect("lookup"), None);
        cache.memory_insert(hash, None).expect("insert");
        assert_eq!(cache.memory_get(&hash).expect("lookup"), Some(None));
    }

    #[test]
    fn memory_batch_cache_preserves_positive_negative_and_misses() {
        let dir = tempdir().expect("tempdir");
        let cache = AddRemoteCandidateCache::open(&dir.path().join("cache.sqlite")).expect("open");
        let positive_hash = MerkleHash::from([8; 32]);
        let negative_hash = MerkleHash::from([9; 32]);
        let missing_hash = MerkleHash::from([10; 32]);
        cache
            .memory_insert_batch(&[(positive_hash, Some(candidate(8))), (negative_hash, None)])
            .expect("insert batch");

        let loaded = cache
            .memory_get_batch(&[positive_hash, negative_hash, missing_hash])
            .expect("lookup batch");
        assert_eq!(loaded.get(&positive_hash), Some(&Some(candidate(8))));
        assert_eq!(loaded.get(&negative_hash), Some(&None));
        assert!(!loaded.contains_key(&missing_hash));
    }

    #[test]
    fn persistent_lookup_batches_large_requests() {
        let dir = tempdir().expect("tempdir");
        let cache = AddRemoteCandidateCache::open(&dir.path().join("cache.sqlite")).expect("open");
        let entries = (0..600u16)
            .map(|seed| {
                let mut bytes = [0; 32];
                bytes[..2].copy_from_slice(&seed.to_le_bytes());
                (MerkleHash::from(bytes), candidate((seed % 256) as u8))
            })
            .collect::<Vec<_>>();
        let entries = entries
            .into_iter()
            .map(|(hash, candidate)| (hash, Some(candidate)))
            .collect::<Vec<_>>();
        cache.persist_results(&entries).expect("persist");
        let hashes = entries.iter().map(|(hash, _)| *hash).collect::<Vec<_>>();
        assert_eq!(cache.load_persistent(&hashes).expect("load").len(), 600);
    }
}
