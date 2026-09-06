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
const PERSISTENT_UNION_LOOKUP_BATCH: usize = LOOKUP_BATCH_SIZE / 2;
const PERSIST_DEDUP_BATCH_SIZE: usize = MEMORY_CAPACITY;
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
        let mut out = HashMap::with_capacity(hashes.len().min(MEMORY_CAPACITY));
        for hash in hashes {
            if let Some(candidate) = memory.get(hash).copied() {
                out.insert(*hash, candidate);
            }
        }
        Ok(out)
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
        let mut out = HashMap::with_capacity(hashes.len().min(MEMORY_CAPACITY));
        // The UNION repeats every hash list, so keep total bind variables below
        // SQLite's default 999-variable limit.
        for batch in hashes.chunks(PERSISTENT_UNION_LOOKUP_BATCH) {
            let placeholders = std::iter::repeat_n("?", batch.len())
                .collect::<Vec<_>>()
                .join(",");
            let values = batch
                .iter()
                .map(|hash| <[u8; 32]>::from(*hash))
                .collect::<Vec<_>>();
            let query = format!(
                "SELECT chunk_hash, xorb_hash, chunk_index, uncompressed_size,
                        placement_id, origin_proof_id, 1 AS is_positive, NULL AS observed_at
                 FROM remote_candidates_v1 WHERE chunk_hash IN ({placeholders})
                 UNION ALL
                 SELECT chunk_hash, NULL, NULL, NULL, NULL, NULL, 0 AS is_positive, observed_at
                 FROM {NEGATIVE_TABLE} WHERE chunk_hash IN ({placeholders})"
            );
            let mut statement = connection
                .prepare_cached(&query)
                .map_err(|error| database_error("prepare lookup", error))?;
            let rows = statement
                .query_map(
                    params_from_iter(
                        values
                            .iter()
                            .chain(values.iter())
                            .map(|value| value.as_slice()),
                    ),
                    |row| {
                        let chunk_hash = decode_hash(row.get(0)?)?;
                        let is_positive: bool = row.get(6)?;
                        let candidate = if is_positive {
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
                            Some(ExistingChunkCandidate {
                                xorb_ref: XorbRef {
                                    xorb_hash: xorb_hash.into(),
                                    chunk_index,
                                    uncompressed_size,
                                },
                                placement_id,
                                origin_proof_id,
                            })
                        } else {
                            None
                        };
                        Ok((chunk_hash, candidate, row.get::<_, Option<i64>>(7)?))
                    },
                )
                .map_err(|error| database_error("lookup", error))?;
            let mut expired = Vec::new();
            for row in rows {
                let (chunk_hash, candidate, observed_at) =
                    row.map_err(|error| database_error("decode lookup", error))?;
                if let Some(candidate) = candidate {
                    out.insert(chunk_hash.into(), Some(candidate));
                } else if let Some(observed_at) = observed_at {
                    if observed_at >= cutoff && observed_at <= now {
                        out.entry(chunk_hash.into()).or_insert(None);
                    } else {
                        expired.push(chunk_hash);
                    }
                } else {
                    return Err(CrabError::Internal(
                        "add remote candidate cache returned an invalid row".into(),
                    ));
                }
            }
            if !expired.is_empty() {
                for expired_batch in expired.chunks(LOOKUP_BATCH_SIZE) {
                    let placeholders = std::iter::repeat_n("?", expired_batch.len())
                        .collect::<Vec<_>>()
                        .join(",");
                    let query = format!(
                        "DELETE FROM {NEGATIVE_TABLE} WHERE chunk_hash IN ({placeholders})"
                    );
                    connection
                        .execute(
                            &query,
                            params_from_iter(expired_batch.iter().map(|hash| hash.as_slice())),
                        )
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
        if entries.len() > PERSIST_DEDUP_BATCH_SIZE {
            // The cache is advisory, so page oversized writes. Processing
            // pages in input order preserves last-result-wins semantics while
            // avoiding a repository-sized deduplication map.
            for batch in entries.chunks(PERSIST_DEDUP_BATCH_SIZE) {
                self.persist_results(batch)?;
            }
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
        let mut latest = HashMap::with_capacity(entries.len());
        for (chunk_hash, candidate) in entries {
            latest.insert(<[u8; 32]>::from(*chunk_hash), *candidate);
        }
        let mut positive_entries = Vec::new();
        let mut negative_hashes = Vec::new();
        for (chunk_hash, candidate) in latest {
            if let Some(candidate) = candidate {
                positive_entries.push((chunk_hash, candidate));
            } else {
                negative_hashes.push(chunk_hash);
            }
        }
        let positive_hashes = positive_entries
            .iter()
            .map(|(chunk_hash, _)| *chunk_hash)
            .collect::<Vec<_>>();
        for batch in positive_hashes.chunks(LOOKUP_BATCH_SIZE) {
            let placeholders = std::iter::repeat_n("?", batch.len())
                .collect::<Vec<_>>()
                .join(",");
            let query =
                format!("DELETE FROM {NEGATIVE_TABLE} WHERE chunk_hash IN ({placeholders})");
            transaction
                .execute(
                    &query,
                    params_from_iter(batch.iter().map(|hash| hash.as_slice())),
                )
                .map_err(|error| database_error("delete stale negative entries", error))?;
        }
        for batch in negative_hashes.chunks(LOOKUP_BATCH_SIZE) {
            let placeholders = std::iter::repeat_n("?", batch.len())
                .collect::<Vec<_>>()
                .join(",");
            let query =
                format!("DELETE FROM remote_candidates_v1 WHERE chunk_hash IN ({placeholders})");
            transaction
                .execute(
                    &query,
                    params_from_iter(batch.iter().map(|hash| hash.as_slice())),
                )
                .map_err(|error| database_error("delete stale positive entries", error))?;
        }
        // Six parameters per row keep positive batches below SQLite's default limit.
        const POSITIVE_WRITE_BATCH: usize = 128;
        for batch in positive_entries.chunks(POSITIVE_WRITE_BATCH) {
            let values_sql = std::iter::repeat_n("(?,?,?,?,?,?)", batch.len())
                .collect::<Vec<_>>()
                .join(",");
            let query = format!(
                "INSERT INTO remote_candidates_v1
                     (chunk_hash, xorb_hash, chunk_index, uncompressed_size,
                      placement_id, origin_proof_id)
                 VALUES {values_sql}
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
                    OR remote_candidates_v1.origin_proof_id != excluded.origin_proof_id"
            );
            let mut statement = transaction
                .prepare_cached(&query)
                .map_err(|error| database_error("prepare update batch", error))?;
            let chunk_hashes = batch
                .iter()
                .map(|(chunk_hash, _)| chunk_hash.as_slice())
                .collect::<Vec<_>>();
            let xorb_hashes = batch
                .iter()
                .map(|(_, candidate)| <[u8; 32]>::from(candidate.xorb_ref.xorb_hash))
                .collect::<Vec<_>>();
            let indices = batch
                .iter()
                .map(|(_, candidate)| i64::from(candidate.xorb_ref.chunk_index))
                .collect::<Vec<_>>();
            let sizes = batch
                .iter()
                .map(|(_, candidate)| i64::from(candidate.xorb_ref.uncompressed_size))
                .collect::<Vec<_>>();
            let placement_ids = batch
                .iter()
                .map(|(_, candidate)| candidate.placement_id)
                .collect::<Vec<_>>();
            let origin_proof_ids = batch
                .iter()
                .map(|(_, candidate)| candidate.origin_proof_id)
                .collect::<Vec<_>>();
            let xorb_slices = xorb_hashes
                .iter()
                .map(|hash| hash.as_slice())
                .collect::<Vec<_>>();
            let placement_slices = placement_ids
                .iter()
                .map(|hash| hash.as_slice())
                .collect::<Vec<_>>();
            let origin_proof_slices = origin_proof_ids
                .iter()
                .map(|hash| hash.as_slice())
                .collect::<Vec<_>>();
            let mut values: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(batch.len() * 6);
            for index in 0..batch.len() {
                values.push(&chunk_hashes[index]);
                values.push(&xorb_slices[index]);
                values.push(&indices[index]);
                values.push(&sizes[index]);
                values.push(&placement_slices[index]);
                values.push(&origin_proof_slices[index]);
            }
            statement
                .execute(params_from_iter(values))
                .map_err(|error| database_error("write update batch", error))?;
        }

        // Two parameters per row keep negative batches below SQLite's default limit.
        const NEGATIVE_WRITE_BATCH: usize = 256;
        for batch in negative_hashes.chunks(NEGATIVE_WRITE_BATCH) {
            let values_sql = std::iter::repeat_n("(?,?)", batch.len())
                .collect::<Vec<_>>()
                .join(",");
            let query = format!(
                "INSERT INTO remote_candidate_misses_v1 (chunk_hash, observed_at)
                 VALUES {values_sql}
                 ON CONFLICT(chunk_hash) DO UPDATE SET observed_at = excluded.observed_at"
            );
            let mut statement = transaction
                .prepare_cached(&query)
                .map_err(|error| database_error("prepare negative update batch", error))?;
            let hash_slices = batch.iter().map(|hash| hash.as_slice()).collect::<Vec<_>>();
            let mut values: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(batch.len() * 2);
            for hash in &hash_slices {
                values.push(hash);
                values.push(&observed_at);
            }
            statement
                .execute(params_from_iter(values))
                .map_err(|error| database_error("write negative update batch", error))?;
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
    fn persistent_duplicate_results_keep_input_order() {
        let dir = tempdir().expect("tempdir");
        let cache = AddRemoteCandidateCache::open(&dir.path().join("cache.sqlite")).expect("open");
        let hash = MerkleHash::from([6; 32]);

        cache
            .persist_results(&[(hash, Some(candidate(6))), (hash, None)])
            .expect("persist negative last");
        assert_eq!(
            cache.load_persistent(&[hash]).expect("load").get(&hash),
            Some(&None)
        );

        cache
            .persist_results(&[(hash, None), (hash, Some(candidate(6)))])
            .expect("persist positive last");
        assert_eq!(
            cache.load_persistent(&[hash]).expect("load").get(&hash),
            Some(&Some(candidate(6)))
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
        assert!(
            !cache
                .memory_get_batch(&[hash])
                .expect("lookup")
                .contains_key(&hash)
        );
        cache.memory_insert_batch(&[(hash, None)]).expect("insert");
        assert_eq!(
            cache.memory_get_batch(&[hash]).expect("lookup").get(&hash),
            Some(&None)
        );
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

    #[test]
    fn persistent_negative_writes_batch_large_requests() {
        let dir = tempdir().expect("tempdir");
        let cache = AddRemoteCandidateCache::open(&dir.path().join("cache.sqlite")).expect("open");
        let entries = (0..600u16)
            .map(|seed| {
                let mut bytes = [0; 32];
                bytes[..2].copy_from_slice(&seed.to_le_bytes());
                (MerkleHash::from(bytes), None)
            })
            .collect::<Vec<_>>();
        cache.persist_results(&entries).expect("persist negatives");
        let hashes = entries.iter().map(|(hash, _)| *hash).collect::<Vec<_>>();
        let loaded = cache.load_persistent(&hashes).expect("load negatives");
        assert_eq!(loaded.len(), 600);
        assert!(loaded.values().all(Option::is_none));
    }
}
