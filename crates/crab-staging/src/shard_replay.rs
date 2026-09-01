//! Disk-backed, bounded replay of file and chunk records from one Xet shard.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crab_xet::hash::{HashedWrite, MerkleHash};

use crate::error::{Result, StagingError};
use crate::recipe::{ChunkingPolicyId, RecipeRecorder};

/// Default number of replay rows returned by one SQLite read.
pub const REPLAY_BATCH_ENTRIES: usize = 1_000;

/// One reconstructed file-index row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayedFileEntry {
    pub id: i64,
    pub file_hash: MerkleHash,
    pub recipe_hash: [u8; 32],
}

/// One reconstructed chunk-index row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayedChunkEntry {
    pub id: i64,
    pub chunk_hash: MerkleHash,
    pub xorb_hash: MerkleHash,
    pub chunk_index: u32,
    pub uncompressed_size: u32,
}

/// A temporary SQLite replay index whose memory use is independent of shard term count.
pub struct ShardReplaySpool {
    _workspace: tempfile::TempDir,
    path: PathBuf,
    pub file_entries: u64,
    pub chunk_entries: u64,
}

impl ShardReplaySpool {
    /// Verify and replay a shard into a temporary workspace beneath `workspace_root`.
    pub fn from_reader_in(
        mut reader: impl Read,
        workspace_root: &Path,
        expected_hash: MerkleHash,
        include_file_index: bool,
        include_chunk_index: bool,
    ) -> Result<Self> {
        let workspace = tempfile::Builder::new()
            .prefix("crab-shard-replay-")
            .tempdir_in(workspace_root)?;
        let source_path = workspace.path().join("shard.mdb");
        let spool_path = workspace.path().join("replay.sqlite");
        let mut source = File::create(&source_path)?;
        let mut hashed = HashedWrite::new(&mut source);
        std::io::copy(&mut reader, &mut hashed)?;
        let actual_hash = hashed.hash();
        drop(hashed);
        source.flush()?;
        if actual_hash != expected_hash {
            return Err(StagingError::ShardReplayCorrupt {
                reason: "shard body hash mismatch during replay".to_owned(),
            });
        }
        drop(source);

        let (file_entries, parsed_chunks) = build_spool(
            &source_path,
            &spool_path,
            include_file_index,
            include_chunk_index,
        )?;
        Ok(Self {
            _workspace: workspace,
            path: spool_path,
            file_entries,
            chunk_entries: if include_chunk_index {
                parsed_chunks
            } else {
                0
            },
        })
    }

    /// Return the next bounded batch of file rows after `after_id`.
    pub fn file_batch(&self, after_id: i64, limit: usize) -> Result<Vec<ReplayedFileEntry>> {
        let connection = open_read_only(&self.path)?;
        let mut query = connection.prepare(
            "SELECT id, file_hash, recipe_hash FROM files
             WHERE id > ?1 ORDER BY id LIMIT ?2",
        )?;
        let rows = query.query_map(rusqlite::params![after_id, limit as i64], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Vec<u8>>(2)?,
            ))
        })?;
        rows.map(|row| {
            let (id, file_hash, recipe_hash) = row?;
            let recipe_hash = recipe_hash.try_into().map_err(|value: Vec<u8>| {
                StagingError::ShardReplayCorrupt {
                    reason: format!(
                        "recipe hash has {} bytes in shard replay spool, expected 32",
                        value.len()
                    ),
                }
            })?;
            Ok(ReplayedFileEntry {
                id,
                file_hash: parse_hash(&file_hash, "file hash")?,
                recipe_hash,
            })
        })
        .collect()
    }

    /// Return the next bounded batch of chunk rows after `after_id`.
    pub fn chunk_batch(&self, after_id: i64, limit: usize) -> Result<Vec<ReplayedChunkEntry>> {
        let connection = open_read_only(&self.path)?;
        let mut query = connection.prepare(
            "SELECT id, chunk_hash, xorb_hash, chunk_index, size FROM chunks
             WHERE id > ?1 ORDER BY id LIMIT ?2",
        )?;
        let rows = query.query_map(rusqlite::params![after_id, limit as i64], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?;
        rows.map(|row| {
            let (id, chunk_hash, xorb_hash, chunk_index, size) = row?;
            Ok(ReplayedChunkEntry {
                id,
                chunk_hash: parse_hash(&chunk_hash, "chunk hash")?,
                xorb_hash: parse_hash(&xorb_hash, "xorb hash")?,
                chunk_index: u32::try_from(chunk_index).map_err(|error| {
                    StagingError::ShardReplayCorrupt {
                        reason: format!("invalid chunk index in shard replay spool: {error}"),
                    }
                })?,
                uncompressed_size: u32::try_from(size).map_err(|error| {
                    StagingError::ShardReplayCorrupt {
                        reason: format!("invalid chunk size in shard replay spool: {error}"),
                    }
                })?,
            })
        })
        .collect()
    }
}

fn build_spool(
    source_path: &Path,
    spool_path: &Path,
    include_file_index: bool,
    include_chunk_index: bool,
) -> Result<(u64, u64)> {
    let mut connection = rusqlite::Connection::open(spool_path)?;
    connection.execute_batch(
        "PRAGMA journal_mode = OFF;
         PRAGMA synchronous = OFF;
         CREATE TABLE chunks (
             id INTEGER PRIMARY KEY,
             xorb_hash TEXT NOT NULL,
             chunk_index INTEGER NOT NULL,
             chunk_hash TEXT NOT NULL,
             size INTEGER NOT NULL,
             UNIQUE (xorb_hash, chunk_index)
         );
         CREATE TABLE files (
             id INTEGER PRIMARY KEY,
             file_hash TEXT NOT NULL,
             recipe_hash BLOB NOT NULL
         );",
    )?;

    let needs_chunks = include_file_index || include_chunk_index;
    let chunk_entries = if needs_chunks {
        replay_chunks(&mut connection, source_path)?
    } else {
        0
    };
    let file_entries = if include_file_index {
        replay_files(&mut connection, source_path)?
    } else {
        0
    };
    Ok((file_entries, chunk_entries))
}

fn replay_chunks(connection: &mut rusqlite::Connection, source_path: &Path) -> Result<u64> {
    let transaction = connection.transaction()?;
    let mut insert = transaction.prepare_cached(
        "INSERT INTO chunks (xorb_hash, chunk_index, chunk_hash, size)
         VALUES (?1, ?2, ?3, ?4)",
    )?;
    let mut source = File::open(source_path)?;
    let count = crab_xet::shard_parse::visit_xorb_chunks_from_reader(
        &mut source,
        |xorb_hash, chunk_index, chunk_hash, size| {
            insert
                .execute(rusqlite::params![
                    xorb_hash.hex(),
                    i64::from(chunk_index),
                    chunk_hash.hex(),
                    i64::from(size),
                ])
                .map(|_| ())
                .map_err(std::io::Error::other)
        },
    )
    .map_err(map_parse_error)?;
    drop(insert);
    transaction.commit()?;
    Ok(count)
}

fn replay_files(connection: &mut rusqlite::Connection, source_path: &Path) -> Result<u64> {
    let transaction = connection.transaction()?;
    let mut insert =
        transaction.prepare_cached("INSERT INTO files (file_hash, recipe_hash) VALUES (?1, ?2)")?;
    let mut source = File::open(source_path)?;
    let count = crab_xet::shard_parse::visit_file_entries_from_reader(
        &mut source,
        |file_hash, terms| {
            let mut recorder = RecipeRecorder::new(ChunkingPolicyId::XetGearV1_64KiB);
            for term in terms {
                if term.chunk_index_start > term.chunk_index_end {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "file {} has reversed chunk range {}..{}",
                            file_hash.hex(),
                            term.chunk_index_start,
                            term.chunk_index_end
                        ),
                    ));
                }
                let mut selected_bytes = 0_u64;
                let mut selected_chunks = 0_u64;
                {
                    let mut query = transaction
                        .prepare_cached(
                            "SELECT chunk_hash, size FROM chunks
                             WHERE xorb_hash = ?1 AND chunk_index >= ?2 AND chunk_index < ?3
                             ORDER BY chunk_index",
                        )
                        .map_err(std::io::Error::other)?;
                    let mut rows = query
                        .query(rusqlite::params![
                            term.xorb_hash.hex(),
                            i64::from(term.chunk_index_start),
                            i64::from(term.chunk_index_end),
                        ])
                        .map_err(std::io::Error::other)?;
                    while let Some(row) = rows.next().map_err(std::io::Error::other)? {
                        let chunk_hash: String = row.get(0).map_err(std::io::Error::other)?;
                        let size: i64 = row.get(1).map_err(std::io::Error::other)?;
                        let size = u64::try_from(size).map_err(std::io::Error::other)?;
                        recorder
                            .record(
                                MerkleHash::from_hex(&chunk_hash).map_err(std::io::Error::other)?,
                                size,
                            )
                            .map_err(std::io::Error::other)?;
                        selected_bytes = selected_bytes.checked_add(size).ok_or_else(|| {
                            std::io::Error::other("recipe byte count overflow")
                        })?;
                        selected_chunks = selected_chunks.checked_add(1).ok_or_else(|| {
                            std::io::Error::other("recipe chunk count overflow")
                        })?;
                    }
                }
                let expected_chunks = u64::from(term.chunk_index_end - term.chunk_index_start);
                if selected_chunks != expected_chunks
                    || selected_bytes != u64::from(term.unpacked_segment_bytes)
                {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!(
                            "file {} term {}..{} resolved to {selected_chunks} chunks/{selected_bytes} bytes, expected {expected_chunks} chunks/{} bytes",
                            file_hash.hex(),
                            term.chunk_index_start,
                            term.chunk_index_end,
                            term.unpacked_segment_bytes
                        ),
                    ));
                }
            }
            let file_size = recorder.recorded_bytes();
            let recipe = recorder
                .seal(file_hash, file_size)
                .map_err(std::io::Error::other)?;
            insert
                .execute(rusqlite::params![file_hash.hex(), recipe.hash().as_slice()])
                .map(|_| ())
                .map_err(std::io::Error::other)
        },
    )
    .map_err(map_parse_error)?;
    drop(insert);
    transaction.commit()?;
    Ok(count)
}

fn open_read_only(path: &Path) -> Result<rusqlite::Connection> {
    Ok(rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?)
}

fn parse_hash(value: &str, field: &str) -> Result<MerkleHash> {
    MerkleHash::from_hex(value).map_err(|error| StagingError::ShardReplayCorrupt {
        reason: format!("invalid {field} in shard replay spool: {error}"),
    })
}

fn map_parse_error(error: crab_xet::error::XetError) -> StagingError {
    match error {
        crab_xet::error::XetError::ShardReplayIo { source, .. } => StagingError::Io(source),
        other => StagingError::ShardReplayCorrupt {
            reason: format!("failed to replay shard entries: {other}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::sync::Arc;

    use crab_xet::shard::{
        FileDataSequenceEntry, FileDataSequenceHeader, MDBFileInfo, MDBXorbInfo, ShardWriter,
        XorbChunkSequenceEntry, XorbChunkSequenceHeader,
    };

    use super::*;

    fn hash(byte: u8) -> MerkleHash {
        MerkleHash::from([byte; 32])
    }

    #[test]
    fn replay_spool_preserves_file_recipe_and_chunk_rows() -> Result<()> {
        let chunks = vec![
            XorbChunkSequenceEntry::new(hash(1), 3, 0),
            XorbChunkSequenceEntry::new(hash(2), 5, 3),
        ];
        let xorb = Arc::new(MDBXorbInfo {
            metadata: XorbChunkSequenceHeader::new(hash(3), 2, 8),
            chunks,
        });
        let file = MDBFileInfo {
            metadata: FileDataSequenceHeader::new(hash(4), 1, false, false),
            segments: vec![FileDataSequenceEntry::new(hash(3), 8, 0, 2)],
            verification: Vec::new(),
            metadata_ext: None,
        };
        let mut writer = ShardWriter::new();
        writer.add_xorb(xorb)?;
        writer.add_file(file)?;
        let (bytes, expected_hash) = writer.finalize()?;
        let workspace = tempfile::tempdir()?;
        let spool = ShardReplaySpool::from_reader_in(
            Cursor::new(bytes),
            workspace.path(),
            expected_hash,
            true,
            true,
        )?;

        let files = spool.file_batch(0, REPLAY_BATCH_ENTRIES)?;
        let chunks = spool.chunk_batch(0, REPLAY_BATCH_ENTRIES)?;
        assert_eq!(files.len(), 1);
        assert_eq!(chunks.len(), 2);
        assert_eq!(files[0].file_hash, hash(4));
        assert_eq!(chunks[1].chunk_hash, hash(2));
        Ok(())
    }
}
