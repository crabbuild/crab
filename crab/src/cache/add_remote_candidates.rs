//! Persistent advisory cache for proof-backed remote chunk candidates.
//!
//! Entries are scoped to one bucket and global content prefix. They only
//! accelerate `crab add`; push revalidates every placement and origin proof
//! before adopting it.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

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

/// A bounded bucket-scoped cache of proof-backed candidates.
pub(crate) struct AddRemoteCandidateCache {
    connection: Mutex<Connection>,
    memory: Mutex<LruCache<MerkleHash, Option<ExistingChunkCandidate>>>,
}

impl AddRemoteCandidateCache {
    /// Open or create the cache. Cache failures are handled by the caller as
    /// an advisory miss; they must never make `crab add` fail.
    pub(crate) fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(CrabError::Io)?;
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
                     PRAGMA user_version = 1;
                     COMMIT;",
                )
                .map_err(|error| database_error("initialize", error))?;
        } else if version != SCHEMA_VERSION {
            return Err(CrabError::Internal(format!(
                "unsupported add remote candidate cache schema {version}; expected v{SCHEMA_VERSION}"
            )));
        }
        let capacity = NonZeroUsize::new(MEMORY_CAPACITY).unwrap_or(NonZeroUsize::MIN);
        Ok(Self {
            connection: Mutex::new(connection),
            memory: Mutex::new(LruCache::new(capacity)),
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

    pub(crate) fn load_persistent(
        &self,
        hashes: &[MerkleHash],
    ) -> Result<HashMap<MerkleHash, ExistingChunkCandidate>> {
        if hashes.is_empty() {
            return Ok(HashMap::new());
        }
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
                                    xorb_hash,
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
                out.insert(chunk_hash, candidate);
            }
        }
        Ok(out)
    }

    pub(crate) fn persist(&self, entries: &[(MerkleHash, ExistingChunkCandidate)]) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| CrabError::Internal("add remote candidate database poisoned".into()))?;
        let transaction = connection
            .transaction()
            .map_err(|error| database_error("begin update", error))?;
        {
            let mut statement = transaction
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
                         origin_proof_id = excluded.origin_proof_id",
                )
                .map_err(|error| database_error("prepare update", error))?;
            for (chunk_hash, candidate) in entries {
                let chunk_hash = <[u8; 32]>::from(*chunk_hash);
                let xorb_hash = <[u8; 32]>::from(candidate.xorb_ref.xorb_hash);
                statement
                    .execute(params![
                        chunk_hash.as_slice(),
                        xorb_hash.as_slice(),
                        i64::from(candidate.xorb_ref.chunk_index),
                        i64::from(candidate.xorb_ref.uncompressed_size),
                        candidate.placement_id.as_slice(),
                        candidate.origin_proof_id.as_slice(),
                    ])
                    .map_err(|error| database_error("write update", error))?;
            }
        }
        transaction
            .commit()
            .map_err(|error| database_error("commit update", error))?;
        let count: i64 = connection
            .query_row("SELECT COUNT(*) FROM remote_candidates_v1", [], |row| {
                row.get(0)
            })
            .map_err(|error| database_error("count entries", error))?;
        if count > MAX_PERSISTENT_ENTRIES {
            let overflow = count - MAX_PERSISTENT_ENTRIES;
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
        cache.persist(&[(hash, candidate(7))]).expect("persist");
        let loaded = cache.load_persistent(&[hash]).expect("load");
        assert_eq!(loaded.get(&hash), Some(&candidate(7)));
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
}
