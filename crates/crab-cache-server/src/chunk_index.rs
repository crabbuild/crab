//! Persistent chunk-to-xorb index for org-level cross-repo dedup.
//!
//! Maps chunk hashes to their xorb location (xorb hash, chunk index, length)
//! using SQLite.
//!
//! Populated automatically when shards are cached. Queried by clients
//! during push to skip uploading chunks that already exist.

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use bytes::Bytes;
use crab_xet::shard::ShardReader;
use crab_xet::xorb::format::MerkleHash;
use rusqlite::{Connection, OptionalExtension, params};
use tracing::warn;

use crate::db::map_sqlite_err;
#[cfg(test)]
use crate::db::{CACHE_DB_FILE, CacheDb};
use crate::error::{CacheServiceError, Result};

const VAL_LEN: usize = 40;

/// Persistent chunk-to-xorb index backed by SQLite.
pub struct ChunkIndex {
    conn: Mutex<Connection>,
}

/// Location of a chunk within a xorb.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkLocation {
    pub xorb_hash: [u8; 32],
    pub chunk_index: u32,
    pub length: u32,
}

/// Result of a batch dedup query, partitioning input indices into known/unknown.
#[derive(Debug)]
pub struct DedupResult {
    /// Chunks found in the index, with their original index and location.
    pub known: Vec<(usize, ChunkLocation)>,
    /// Indices of chunks not found in the index.
    pub unknown: Vec<usize>,
}

impl ChunkIndex {
    /// Open a chunk index using the given SQLite connection.
    pub fn open(conn: Connection) -> Result<Self> {
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// Batch lookup: returns a `DedupResult` partitioning input indices into
    /// known (with their xorb location) and unknown.
    pub fn query_batch(&self, hashes: &[[u8; 32]]) -> Result<DedupResult> {
        let conn = self.connection()?;
        let mut stmt = conn
            .prepare("SELECT xorb_ref FROM chunk_index WHERE chunk_hash = ?1")
            .map_err(map_sqlite_err)?;
        let mut known = Vec::new();
        let mut unknown = Vec::new();

        for (i, hash) in hashes.iter().enumerate() {
            match stmt
                .query_row(params![hash.as_slice()], |row| row.get::<_, Vec<u8>>(0))
                .optional()
                .map_err(map_sqlite_err)?
            {
                Some(val) => {
                    if let Some(location) = decode_value(&val) {
                        known.push((i, location));
                    } else {
                        // Corrupt entry: treat as unknown.
                        warn!(
                            index = i,
                            "chunk_index entry has invalid length, treating as unknown"
                        );
                        unknown.push(i);
                    }
                }
                None => unknown.push(i),
            }
        }

        Ok(DedupResult { known, unknown })
    }

    /// Parse production shard data and insert chunk-to-xorb mappings.
    ///
    /// The accepted format is the serialized `MDBShardInfo` used by the Crab CLI.
    ///
    /// Returns the number of entries ingested.
    pub fn ingest_shard(&self, shard_data: &[u8]) -> Result<u64> {
        if shard_data.is_empty() {
            return Ok(0);
        }

        let entries = parse_production_shard_entries(shard_data)?;

        self.insert_entries(&entries)?;

        let ingested = u64::try_from(entries.len()).map_err(|_| {
            CacheServiceError::InternalError("shard has too many entries to count".into())
        })?;
        tracing::debug!(entries = ingested, "ingested shard into chunk index");
        Ok(ingested)
    }

    fn insert_entries(&self, entries: &[ShardIndexEntry]) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }

        let mut conn = self.connection()?;
        let tx = conn.transaction().map_err(map_sqlite_err)?;
        {
            let mut stmt = tx
                .prepare(
                    "INSERT OR REPLACE INTO chunk_index (chunk_hash, xorb_ref)
                     VALUES (?1, ?2)",
                )
                .map_err(map_sqlite_err)?;

            for entry in entries {
                let value = encode_value(&entry.xorb_hash, entry.chunk_index, entry.length);
                stmt.execute(params![entry.chunk_hash.as_slice(), value.as_slice()])
                    .map_err(map_sqlite_err)?;
            }
        }
        tx.commit().map_err(map_sqlite_err)?;
        Ok(())
    }

    /// Rebuild the chunk index by scanning a directory for shard files and
    /// re-ingesting each one.
    ///
    /// Returns the total number of entries ingested across all shards.
    pub fn rebuild_from_shards(&self, shard_dir: &Path) -> Result<u64> {
        let mut stack: Vec<PathBuf> = match std::fs::read_dir(shard_dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::debug!(dir = %shard_dir.display(), "shard directory not found, nothing to rebuild");
                return Ok(0);
            }
            Err(e) => {
                return Err(CacheServiceError::InternalError(
                    format!("failed to read shard dir {}: {e}", shard_dir.display()).into(),
                ));
            }
        }
        .filter_map(|entry| match entry {
            Ok(entry) => Some(entry.path()),
            Err(e) => {
                warn!(dir = %shard_dir.display(), error = %e, "failed to read shard dir entry, skipping");
                None
            }
        })
        .collect();

        let mut total: u64 = 0;
        while let Some(path) = stack.pop() {
            if path.is_dir() {
                match std::fs::read_dir(&path) {
                    Ok(entries) => {
                        for entry in entries {
                            match entry {
                                Ok(entry) => stack.push(entry.path()),
                                Err(e) => {
                                    warn!(dir = %path.display(), error = %e, "failed to read shard dir entry, skipping");
                                }
                            }
                        }
                    }
                    Err(e) => {
                        warn!(dir = %path.display(), error = %e, "failed to read shard subdir, skipping");
                    }
                }
                continue;
            }

            if !path.is_file() {
                continue;
            }

            let data = match std::fs::read(&path) {
                Ok(data) => data,
                Err(e) => {
                    warn!(path = %path.display(), error = %e, "failed to read shard file, skipping");
                    continue;
                }
            };

            match self.ingest_shard(&data) {
                Ok(n) => {
                    total = total.checked_add(n).ok_or_else(|| {
                        CacheServiceError::InternalError(
                            "rebuilt chunk index entry count overflowed".into(),
                        )
                    })?;
                }
                Err(e) => {
                    warn!(path = %path.display(), error = %e, "failed to ingest shard, skipping");
                }
            }
        }

        tracing::debug!(total_entries = total, dir = %shard_dir.display(), "rebuilt chunk index from shards");
        Ok(total)
    }

    /// Number of chunk-to-xorb mappings in the index.
    pub fn len(&self) -> Result<u64> {
        let conn = self.connection()?;
        let count: i64 = conn
            .query_row("SELECT COUNT(1) FROM chunk_index", [], |row| row.get(0))
            .map_err(map_sqlite_err)?;
        Ok(count as u64)
    }

    /// Whether the index is empty.
    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }

    #[cfg(test)]
    fn stub() -> Result<Self> {
        let dir = tempfile::tempdir().map_err(|e| {
            CacheServiceError::InternalError(
                format!("failed to create temp dir for stub ChunkIndex: {e}").into(),
            )
        })?;
        let db = CacheDb::open_or_create(&dir.path().join(CACHE_DB_FILE))?;
        let index = Self::open(db.connect()?)?;

        // The SQLite connection must outlive the temporary directory for the
        // test index. Leaking here keeps the backing store valid until process exit.
        std::mem::forget(dir);

        Ok(index)
    }

    fn connection(&self) -> Result<MutexGuard<'_, Connection>> {
        self.conn.lock().map_err(|_| {
            CacheServiceError::InternalError("cache service chunk index connection poisoned".into())
        })
    }
}

#[derive(Debug, Clone)]
struct ShardIndexEntry {
    chunk_hash: [u8; 32],
    xorb_hash: [u8; 32],
    chunk_index: u32,
    length: u32,
}

fn parse_production_shard_entries(shard_data: &[u8]) -> Result<Vec<ShardIndexEntry>> {
    let shard_hash = MerkleHash::from(*blake3::hash(shard_data).as_bytes());
    let reader = ShardReader::from_bytes(Bytes::copy_from_slice(shard_data), shard_hash);
    let shard = reader.shard_info_public().map_err(|e| {
        CacheServiceError::InternalError(format!("failed to parse production shard: {e}").into())
    })?;

    let mut cursor = Cursor::new(reader.v1_data());
    let xorb_blocks = shard.read_all_xorb_blocks_full(&mut cursor).map_err(|e| {
        CacheServiceError::InternalError(format!("failed to read shard xorb blocks: {e}").into())
    })?;

    let entry_count = xorb_blocks.iter().map(|xorb| xorb.chunks.len()).sum();
    let mut entries = Vec::with_capacity(entry_count);

    for xorb in xorb_blocks {
        let xorb_hash: [u8; 32] = xorb.metadata.xorb_hash.into();
        for (chunk_index, chunk) in xorb.chunks.into_iter().enumerate() {
            let chunk_index = u32::try_from(chunk_index).map_err(|_| {
                CacheServiceError::InternalError("shard xorb has too many chunks".into())
            })?;
            entries.push(ShardIndexEntry {
                chunk_hash: chunk.chunk_hash.into(),
                xorb_hash,
                chunk_index,
                length: chunk.unpacked_segment_bytes,
            });
        }
    }

    Ok(entries)
}

fn encode_value(xorb_hash: &[u8; 32], chunk_index: u32, length: u32) -> [u8; VAL_LEN] {
    let mut val = [0u8; VAL_LEN];
    val[..32].copy_from_slice(xorb_hash);
    val[32..36].copy_from_slice(&chunk_index.to_le_bytes());
    val[36..40].copy_from_slice(&length.to_le_bytes());
    val
}

fn decode_value(val: &[u8]) -> Option<ChunkLocation> {
    if val.len() != VAL_LEN {
        return None;
    }

    let mut xorb_hash = [0u8; 32];
    xorb_hash.copy_from_slice(&val[..32]);
    let mut chunk_index = [0u8; 4];
    let mut length = [0u8; 4];
    chunk_index.copy_from_slice(&val[32..36]);
    length.copy_from_slice(&val[36..40]);

    Some(ChunkLocation {
        xorb_hash,
        chunk_index: u32::from_le_bytes(chunk_index),
        length: u32::from_le_bytes(length),
    })
}

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crab_xet::shard::ShardWriter;
    use crab_xet::shard::{MDBXorbInfo, XorbChunkSequenceEntry, XorbChunkSequenceHeader};

    /// Create a deterministic 32-byte hash from a seed byte.
    fn hash_from_seed(seed: u8) -> [u8; 32] {
        [seed; 32]
    }

    fn test_index() -> ChunkIndex {
        ChunkIndex::stub().unwrap()
    }

    fn make_production_shard_with_xorb(xorb_hash: [u8; 32], chunks: &[([u8; 32], u32)]) -> Vec<u8> {
        let xorb_hash = MerkleHash::from(xorb_hash);
        let mut byte_offset = 0u32;
        let entries: Vec<XorbChunkSequenceEntry> = chunks
            .iter()
            .map(|(chunk_hash, length)| {
                let entry = XorbChunkSequenceEntry::new(
                    MerkleHash::from(*chunk_hash),
                    *length,
                    byte_offset,
                );
                byte_offset = byte_offset.checked_add(*length).unwrap();
                entry
            })
            .collect();

        let xorb = Arc::new(MDBXorbInfo {
            metadata: XorbChunkSequenceHeader::new(xorb_hash, chunks.len(), byte_offset),
            chunks: entries,
        });

        let mut writer = ShardWriter::new();
        writer.add_xorb(xorb).unwrap();
        let (bytes, _) = writer.finalize().unwrap();
        bytes
    }

    fn make_production_shard() -> (Vec<u8>, [u8; 32], [u8; 32], [u8; 32]) {
        let xorb = hash_from_seed(9);
        let chunk_a = hash_from_seed(1);
        let chunk_b = hash_from_seed(2);
        let bytes = make_production_shard_with_xorb(xorb, &[(chunk_a, 100), (chunk_b, 200)]);

        (bytes, chunk_a, chunk_b, xorb)
    }

    #[test]
    fn insert_and_query_round_trip() {
        let idx = test_index();
        let chunk = hash_from_seed(1);
        let xorb = hash_from_seed(0xAA);

        let shard_data = make_production_shard_with_xorb(xorb, &[(chunk, 65536)]);
        let ingested = idx.ingest_shard(&shard_data).unwrap();
        assert_eq!(ingested, 1);

        let result = idx.query_batch(&[chunk]).unwrap();
        assert_eq!(result.known.len(), 1);
        assert!(result.unknown.is_empty());

        let (i, loc) = &result.known[0];
        assert_eq!(*i, 0);
        assert_eq!(loc.xorb_hash, xorb);
        assert_eq!(loc.chunk_index, 0);
        assert_eq!(loc.length, 65536);
    }

    #[test]
    fn ingest_production_shard_round_trip() {
        let idx = test_index();
        let (shard_data, chunk_a, chunk_b, xorb) = make_production_shard();

        let ingested = idx.ingest_shard(&shard_data).unwrap();
        assert_eq!(ingested, 2);

        let result = idx.query_batch(&[chunk_a, chunk_b]).unwrap();
        assert_eq!(result.known.len(), 2);
        assert!(result.unknown.is_empty());
        assert_eq!(result.known[0].1.xorb_hash, xorb);
        assert_eq!(result.known[0].1.chunk_index, 0);
        assert_eq!(result.known[0].1.length, 100);
        assert_eq!(result.known[1].1.xorb_hash, xorb);
        assert_eq!(result.known[1].1.chunk_index, 1);
        assert_eq!(result.known[1].1.length, 200);
    }

    #[test]
    fn query_unknown_hashes() {
        let idx = test_index();
        let unknown1 = hash_from_seed(0xFF);
        let unknown2 = hash_from_seed(0xFE);

        let result = idx.query_batch(&[unknown1, unknown2]).unwrap();
        assert!(result.known.is_empty());
        assert_eq!(result.unknown, vec![0, 1]);
    }

    #[test]
    fn batch_query_mixed_known_unknown() {
        let idx = test_index();
        let chunk_a = hash_from_seed(1);
        let chunk_b = hash_from_seed(2);
        let chunk_unknown = hash_from_seed(99);
        let xorb = hash_from_seed(0xBB);

        let shard_data = make_production_shard_with_xorb(xorb, &[(chunk_a, 100), (chunk_b, 200)]);
        idx.ingest_shard(&shard_data).unwrap();

        let result = idx.query_batch(&[chunk_a, chunk_unknown, chunk_b]).unwrap();

        // Known: indices 0 and 2.
        assert_eq!(result.known.len(), 2);
        assert_eq!(result.known[0].0, 0);
        assert_eq!(result.known[0].1.chunk_index, 0);
        assert_eq!(result.known[0].1.length, 100);
        assert_eq!(result.known[1].0, 2);
        assert_eq!(result.known[1].1.chunk_index, 1);
        assert_eq!(result.known[1].1.length, 200);

        // Unknown: index 1.
        assert_eq!(result.unknown, vec![1]);
    }

    #[test]
    fn len_accuracy() {
        let idx = test_index();
        assert_eq!(idx.len().unwrap(), 0);

        let chunks: Vec<([u8; 32], u32)> =
            (0..5u8).map(|seed| (hash_from_seed(seed), 64)).collect();
        let shard_data = make_production_shard_with_xorb(hash_from_seed(0xCC), &chunks);
        idx.ingest_shard(&shard_data).unwrap();
        assert_eq!(idx.len().unwrap(), 5);
    }

    #[test]
    fn ingest_empty_shard() {
        let idx = test_index();
        let ingested = idx.ingest_shard(&[]).unwrap();
        assert_eq!(ingested, 0);
        assert_eq!(idx.len().unwrap(), 0);
    }

    #[test]
    fn ingest_rejects_non_production_shard_bytes() {
        let idx = test_index();
        let mut invalid_legacy_fixture = vec![0u8; 72];
        invalid_legacy_fixture[..32].copy_from_slice(&hash_from_seed(1));
        invalid_legacy_fixture[32..64].copy_from_slice(&hash_from_seed(0xAA));

        let err = idx
            .ingest_shard(&invalid_legacy_fixture)
            .expect_err("legacy fixture bytes must not ingest");

        assert!(err.to_string().contains("failed to parse production shard"));
        assert_eq!(idx.len().unwrap(), 0);
    }

    #[test]
    fn rebuild_from_shards_directory() {
        let dir = tempfile::tempdir().unwrap();
        let shard_dir = dir.path();

        // Write two shard files using the service cache layout: shards/{prefix}/{hash}.
        let shard1 =
            make_production_shard_with_xorb(hash_from_seed(0xAA), &[(hash_from_seed(1), 100)]);
        std::fs::create_dir_all(shard_dir.join("aa")).unwrap();
        std::fs::write(shard_dir.join("aa").join("shard1"), &shard1).unwrap();

        let shard2 = make_production_shard_with_xorb(
            hash_from_seed(0xBB),
            &[(hash_from_seed(2), 200), (hash_from_seed(3), 300)],
        );
        std::fs::create_dir_all(shard_dir.join("bb")).unwrap();
        std::fs::write(shard_dir.join("bb").join("shard2"), &shard2).unwrap();

        let idx = test_index();
        let total = idx.rebuild_from_shards(shard_dir).unwrap();
        assert_eq!(total, 3);
        assert_eq!(idx.len().unwrap(), 3);

        // Verify all three chunks are queryable.
        let result = idx
            .query_batch(&[hash_from_seed(1), hash_from_seed(2), hash_from_seed(3)])
            .unwrap();
        assert_eq!(result.known.len(), 3);
        assert!(result.unknown.is_empty());
    }

    #[test]
    fn rebuild_from_nonexistent_directory() {
        let idx = test_index();
        let total = idx
            .rebuild_from_shards(Path::new("/tmp/nonexistent_shard_dir_12345"))
            .unwrap();
        assert_eq!(total, 0);
    }

    #[test]
    fn ingest_overwrites_existing_entry() {
        let idx = test_index();
        let chunk = hash_from_seed(1);
        let xorb_a = hash_from_seed(0xAA);
        let xorb_b = hash_from_seed(0xBB);

        idx.ingest_shard(&make_production_shard_with_xorb(xorb_a, &[(chunk, 100)]))
            .unwrap();
        idx.ingest_shard(&make_production_shard_with_xorb(xorb_b, &[(chunk, 300)]))
            .unwrap();

        // Should reflect the latest write.
        let result = idx.query_batch(&[chunk]).unwrap();
        assert_eq!(result.known.len(), 1);
        assert_eq!(result.known[0].1.xorb_hash, xorb_b);
        assert_eq!(result.known[0].1.chunk_index, 0);
        assert_eq!(result.known[0].1.length, 300);

        // Still only one entry.
        assert_eq!(idx.len().unwrap(), 1);
    }
}
