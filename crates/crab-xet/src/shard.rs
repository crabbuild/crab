//! Thin wrappers around xet-core's `MDBInMemoryShard` for building and
//! reading shards, plus a one-shard-per-push session with 100 MiB splitting.
//!
//! ## Footer versioning
//!
//! v1 shards use xet-core's standard footer (72 bytes). v2 shards append a
//! bloom filter section after the shard data and write an 8-byte
//! `bloom_offset` trailer followed by a 4-byte magic `SH02` at the very end.
//! Readers detect v2 by checking the last 4 bytes; absence of the magic means
//! v1 (backward-compatible).

use std::io::{Cursor, Seek, SeekFrom};
use std::mem::size_of;
use std::sync::Arc;

use bytes::Bytes;
use tracing::debug;

use xet_core_structures::merklehash::{HashedWrite, MerkleHash};
pub use xet_core_structures::metadata_shard::file_structs::{
    FileDataSequenceEntry, FileDataSequenceHeader, MDBFileInfo,
};
pub use xet_core_structures::metadata_shard::session_directory::merge_shards;
pub use xet_core_structures::metadata_shard::set_operations::shard_set_union;
pub use xet_core_structures::metadata_shard::shard_file_handle::MDBShardFile;
pub use xet_core_structures::metadata_shard::shard_format::MDBShardInfo;
use xet_core_structures::metadata_shard::shard_in_memory::MDBInMemoryShard;
pub use xet_core_structures::metadata_shard::streaming_shard::MDBMinimalShard;
pub use xet_core_structures::metadata_shard::xorb_structs::{
    MDBXorbInfo, XorbChunkSequenceEntry, XorbChunkSequenceHeader,
};

use crate::error::{Result, XetError};
use crate::shard_bloom::ShardBloom;

/// 100 MiB soft cap for shard splitting.
const SHARD_SIZE_CAP: u64 = 100 * 1024 * 1024;

/// Magic bytes at the end of a v2 shard identifying the extended footer.
/// Readers that don't see this magic treat the shard as v1.
const SHARD_V2_MAGIC: &[u8; 4] = b"SH02";

/// Size of the v2 trailer: `bloom_offset` (8 bytes) + magic (4 bytes).
const SHARD_V2_TRAILER_SIZE: usize = 12;

/// Thin wrapper around xet-core's `MDBInMemoryShard` for building shards.
pub struct ShardWriter {
    inner: MDBInMemoryShard,
    size_cap: u64,
}

impl ShardWriter {
    /// Create a new empty shard writer with the default 100 MiB soft cap.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: MDBInMemoryShard::default(),
            size_cap: SHARD_SIZE_CAP,
        }
    }

    /// Add xorb CAS info to the shard.
    ///
    /// # Errors
    /// Returns `XetError::Internal` if the underlying shard rejects the entry.
    pub fn add_xorb(&mut self, xorb_info: Arc<MDBXorbInfo>) -> Result<()> {
        self.inner
            .add_xorb_block(xorb_info)
            .map_err(|e| XetError::Internal(format!("shard add_xorb: {e}")))?;
        Ok(())
    }

    /// Add file reconstruction info.
    ///
    /// # Errors
    /// Returns `XetError::Internal` if the underlying shard rejects the entry.
    pub fn add_file(&mut self, file_info: MDBFileInfo) -> Result<()> {
        self.inner
            .add_file_reconstruction_info(file_info)
            .map_err(|e| XetError::Internal(format!("shard add_file: {e}")))?;
        Ok(())
    }

    /// Whether the shard has exceeded the soft size cap and should be split.
    #[must_use]
    pub fn should_split(&self) -> bool {
        self.inner.shard_file_size() > self.size_cap
    }

    /// Current estimated size in bytes.
    #[must_use]
    pub fn size(&self) -> u64 {
        self.inner.shard_file_size()
    }

    /// Serialize the shard to bytes and return `(bytes, shard_hash)`.
    ///
    /// The hash is computed with xet-core's keyed data hash convention.
    ///
    /// # Errors
    /// Returns `XetError::Internal` on serialization failure.
    pub fn finalize(self) -> Result<(Vec<u8>, MerkleHash)> {
        let mut buf = Vec::new();
        let mut hashed = HashedWrite::new(&mut buf);
        MDBShardInfo::serialize_from(&mut hashed, &self.inner, None)
            .map_err(|e| XetError::Internal(format!("shard finalize: {e}")))?;
        let hash = hashed.hash();
        Ok((buf, hash))
    }

    /// Serialize the shard with an appended bloom filter (footer version 2).
    ///
    /// Layout: `[shard v1 data][bloom encoded][bloom_offset: u64 LE][magic: "SH02"]`
    ///
    /// The `bloom_offset` points to the start of the bloom section relative
    /// to the beginning of the buffer. The hash covers the entire buffer
    /// including the bloom and trailer.
    ///
    /// # Errors
    /// Returns `XetError::Internal` on serialization failure.
    pub fn finalize_with_bloom(
        self,
        file_hashes: &[MerkleHash],
        chunk_hashes: &[MerkleHash],
    ) -> Result<(Vec<u8>, MerkleHash)> {
        // Serialize the v1 shard data first.
        let mut buf = Vec::new();
        MDBShardInfo::serialize_from(&mut buf, &self.inner, None)
            .map_err(|e| XetError::Internal(format!("shard finalize: {e}")))?;

        // Record where the bloom section starts.
        let bloom_offset = buf.len() as u64;

        // Build and append bloom + v2 trailer.
        let bloom = ShardBloom::build(file_hashes, chunk_hashes);
        buf.extend_from_slice(&bloom.encode());
        buf.extend_from_slice(&bloom_offset.to_le_bytes());
        buf.extend_from_slice(SHARD_V2_MAGIC);

        // Hash the entire buffer.
        let hash = {
            use std::io::Write;
            let mut hashed = HashedWrite::new(std::io::sink());
            hashed
                .write_all(&buf)
                .map_err(|e| XetError::Internal(format!("shard hash: {e}")))?;
            hashed.hash()
        };

        Ok((buf, hash))
    }

    /// Whether the shard is empty (no xorb or file entries).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

impl Default for ShardWriter {
    fn default() -> Self {
        Self::new()
    }
}

/// Lazy-loading shard reader backed by raw bytes.
///
/// The shard is parsed on first query via `OnceLock`, avoiding upfront
/// deserialization cost when the shard is never actually queried.
pub struct ShardReader {
    data: Bytes,
    /// Lazily populated on first access. `Err` string cached on parse failure
    /// so we don't retry a broken shard repeatedly.
    parsed: std::sync::OnceLock<std::result::Result<MDBShardInfo, String>>,
    hash: MerkleHash,
}

impl ShardReader {
    /// Create from raw shard bytes and a known content hash.
    #[must_use]
    pub fn from_bytes(data: Bytes, hash: MerkleHash) -> Self {
        Self {
            data,
            parsed: std::sync::OnceLock::new(),
            hash,
        }
    }

    /// Query chunk dedup info for a sequence of chunk hashes.
    ///
    /// Returns `(matched_count, FileDataSequenceEntry)` on hit.
    ///
    /// # Errors
    /// Returns `XetError::Internal` on parse or query failure.
    pub fn chunk_dedup_query(
        &self,
        hashes: &[MerkleHash],
    ) -> Result<Option<(usize, FileDataSequenceEntry)>> {
        let shard = self.shard_info()?;
        let mut cursor = Cursor::new(self.data.as_ref());
        shard
            .chunk_hash_dedup_query(&mut cursor, hashes)
            .map_err(|e| XetError::Internal(format!("shard chunk_dedup_query: {e}")))
    }

    /// Get file reconstruction info by file hash.
    ///
    /// # Errors
    /// Returns `XetError::Internal` on parse or query failure.
    pub fn get_file_info(&self, file_hash: &MerkleHash) -> Result<Option<MDBFileInfo>> {
        let shard = self.shard_info()?;
        let mut cursor = Cursor::new(self.data.as_ref());
        shard
            .get_file_reconstruction_info(&mut cursor, file_hash)
            .map_err(|e| XetError::Internal(format!("shard get_file_info: {e}")))
    }

    /// Get xorb chunk metadata by xorb hash.
    ///
    /// # Errors
    /// Returns `XetError::Internal` on parse or query failure, and
    /// `XetError::CorruptObject` if the lookup points outside the xorb
    /// metadata section.
    pub fn get_xorb_info(&self, xorb_hash: &MerkleHash) -> Result<Option<MDBXorbInfo>> {
        let shard = self.shard_info()?;
        let mut cursor = Cursor::new(self.data.as_ref());
        let mut dest_indices = [0u32; 8];
        let num_indices = shard
            .get_xorb_info_index_by_hash(&mut cursor, xorb_hash, &mut dest_indices)
            .map_err(|e| XetError::Internal(format!("shard get_xorb_info index: {e}")))?;

        for &xorb_entry_index in dest_indices.iter().take(num_indices) {
            cursor
                .seek(SeekFrom::Start(
                    shard.metadata.xorb_info_offset
                        + (size_of::<XorbChunkSequenceHeader>() as u64)
                            * u64::from(xorb_entry_index),
                ))
                .map_err(|e| XetError::Internal(format!("shard get_xorb_info seek: {e}")))?;

            let xorb_info = MDBXorbInfo::deserialize(&mut cursor)
                .map_err(|e| XetError::Internal(format!("shard get_xorb_info deserialize: {e}")))?
                .ok_or_else(|| XetError::CorruptObject {
                    path: format!("shard:{}", self.hash.hex()),
                    reason: "xorb lookup pointed at bookend".to_owned(),
                })?;
            if xorb_info.metadata.xorb_hash == *xorb_hash {
                return Ok(Some(xorb_info));
            }
        }

        Ok(None)
    }

    /// Return `true` when the shard's file-info section has an entry for
    /// `file_hash`, without surfacing the reconstruction terms to the caller.
    ///
    /// Intended for the shard-hint fast path: after fetching a shard by its
    /// advisory hint, the caller verifies the hint actually covers the file
    /// before running reconstruction. A stale or corrupt hint returns `false`
    /// so the caller can fall back to the file-index lookup — the hint is
    /// advisory, parse failures must not propagate as hydrate errors.
    #[must_use]
    pub fn has_file(&self, file_hash: &MerkleHash) -> bool {
        match self.get_file_info(file_hash) {
            Ok(Some(_)) => true,
            Ok(None) => false,
            Err(err) => {
                debug!(
                    shard_hash = %self.hash.hex(),
                    file_hash = %file_hash.hex(),
                    error = %err,
                    "shard has_file lookup failed, treating as absent",
                );
                false
            }
        }
    }

    /// Shard content hash.
    #[must_use]
    pub fn hash(&self) -> MerkleHash {
        self.hash
    }

    /// Try to read the bloom filter from a v2 shard.
    ///
    /// Returns `None` for v1 shards (no bloom). Returns an error only if the
    /// v2 trailer is present but the bloom data is corrupt.
    ///
    /// # Errors
    /// Returns `XetError::CorruptObject` if the bloom section is malformed.
    pub fn bloom(&self) -> Result<Option<ShardBloom>> {
        let data = self.data.as_ref();
        if data.len() < SHARD_V2_TRAILER_SIZE {
            return Ok(None);
        }
        let tail = &data[data.len() - 4..];
        if tail != SHARD_V2_MAGIC {
            return Ok(None);
        }
        let offset_start = data.len() - SHARD_V2_TRAILER_SIZE;
        let bloom_offset_u64 = u64::from_le_bytes(
            data[offset_start..offset_start + 8]
                .try_into()
                .map_err(|_| XetError::CorruptObject {
                    path: "shard".to_owned(),
                    reason: "bad bloom_offset bytes".to_owned(),
                })?,
        );
        let bloom_offset =
            usize::try_from(bloom_offset_u64).map_err(|_| XetError::CorruptObject {
                path: "shard".to_owned(),
                reason: "bloom_offset exceeds addressable range".to_owned(),
            })?;

        if bloom_offset >= offset_start {
            return Err(XetError::CorruptObject {
                path: "shard".to_owned(),
                reason: "bloom_offset past bloom data".to_owned(),
            });
        }

        let bloom_data = &data[bloom_offset..offset_start];
        ShardBloom::decode(bloom_data).map(Some)
    }

    /// Lazily parse the shard header + footer on first access.
    ///
    /// Handles both v1 and v2 shard formats. V2 shards have a bloom
    /// section + trailer appended after the v1 data. The `bloom_offset`
    /// in the v2 trailer tells us where the v1 data ends.
    fn shard_info(&self) -> Result<&MDBShardInfo> {
        let result = self.parsed.get_or_init(|| {
            let data = self.data.as_ref();

            // Check for v2 trailer and extract the v1 portion.
            let v1_data = if data.len() >= SHARD_V2_TRAILER_SIZE
                && &data[data.len() - 4..] == SHARD_V2_MAGIC
            {
                let offset_start = data.len() - SHARD_V2_TRAILER_SIZE;
                let bloom_offset = u64::from_le_bytes(
                    data[offset_start..offset_start + 8]
                        .try_into()
                        .map_err(|_| "bad bloom_offset bytes".to_string())?,
                ) as usize;
                if bloom_offset <= data.len() {
                    &data[..bloom_offset]
                } else {
                    data
                }
            } else {
                data
            };

            let mut cursor = Cursor::new(v1_data);
            MDBShardInfo::load_from_reader(&mut cursor).map_err(|e| format!("{e}"))
        });
        match result {
            Ok(info) => Ok(info),
            Err(msg) => Err(XetError::Internal(format!("shard parse: {msg}"))),
        }
    }

    /// Public accessor for the parsed shard info.
    ///
    /// Used by shard readers that need access to the full shard structure.
    pub fn shard_info_public(&self) -> Result<&MDBShardInfo> {
        self.shard_info()
    }

    /// Raw shard bytes.
    #[must_use]
    pub fn data(&self) -> &Bytes {
        &self.data
    }

    /// Returns the v1 portion of the shard data (without the v2 bloom trailer).
    ///
    /// For v1 shards, returns the full data. For v2 shards, strips the
    /// bloom section and trailer using the `bloom_offset`.
    #[must_use]
    pub fn v1_data(&self) -> &[u8] {
        let data = self.data.as_ref();
        if data.len() >= SHARD_V2_TRAILER_SIZE && &data[data.len() - 4..] == SHARD_V2_MAGIC {
            let offset_start = data.len() - SHARD_V2_TRAILER_SIZE;
            if let Ok(bytes) = data[offset_start..offset_start + 8].try_into() {
                let bloom_offset = u64::from_le_bytes(bytes) as usize;
                if bloom_offset <= data.len() {
                    return &data[..bloom_offset];
                }
            }
        }
        data
    }
}

/// One-shard-per-push session that splits at file boundaries when the
/// current shard exceeds the 100 MiB soft cap.
///
/// After adding an entry, if the current shard exceeds the cap, a new
/// shard is started for subsequent entries. The entry that caused the
/// overflow stays in the current shard (soft cap, not hard limit).
///
/// When `bloom_enabled` is true (the default), a bloom filter is built
/// from the accumulated file and chunk hashes and appended to each shard
/// at finalize time.
pub struct PushShardSession {
    writers: Vec<(ShardWriter, ShardHashes)>,
    current: ShardWriter,
    current_hashes: ShardHashes,
    bloom_enabled: bool,
}

/// Accumulated hashes for a single shard, used to build the bloom filter.
struct ShardHashes {
    file_hashes: Vec<MerkleHash>,
    chunk_hashes: Vec<MerkleHash>,
}

impl ShardHashes {
    fn new() -> Self {
        Self {
            file_hashes: Vec::new(),
            chunk_hashes: Vec::new(),
        }
    }
}

impl PushShardSession {
    /// Create a new push session with an empty initial shard.
    /// Bloom filters are enabled by default.
    #[must_use]
    pub fn new() -> Self {
        Self {
            writers: Vec::new(),
            current: ShardWriter::new(),
            current_hashes: ShardHashes::new(),
            bloom_enabled: true,
        }
    }

    /// Create a new push session with explicit bloom control.
    #[must_use]
    pub fn with_bloom(bloom_enabled: bool) -> Self {
        Self {
            bloom_enabled,
            ..Self::new()
        }
    }

    /// Add xorb info, rotating to a new shard if the current one is full.
    ///
    /// Chunk hashes from the xorb are collected for bloom construction.
    ///
    /// # Errors
    /// Returns `XetError::Internal` if the underlying shard rejects the entry.
    pub fn add_xorb(&mut self, xorb_info: Arc<MDBXorbInfo>) -> Result<()> {
        if self.bloom_enabled {
            for chunk in &xorb_info.chunks {
                self.current_hashes.chunk_hashes.push(chunk.chunk_hash);
            }
        }
        self.current.add_xorb(xorb_info)?;
        self.maybe_rotate();
        Ok(())
    }

    /// Add file info, rotating to a new shard if the current one is full.
    ///
    /// The file hash is collected for bloom construction.
    ///
    /// Returns the 0-based index of the shard this file was added to.
    /// The index corresponds to the position in the `finalize()` result
    /// vector, so callers can build a precise file→shard mapping even
    /// for multi-shard pushes. See finding CR1-F12.
    ///
    /// # Errors
    /// Returns `XetError::Internal` if the underlying shard rejects the entry.
    pub fn add_file(&mut self, file_info: MDBFileInfo) -> Result<usize> {
        if self.bloom_enabled {
            self.current_hashes
                .file_hashes
                .push(file_info.metadata.file_hash);
        }
        // Capture the shard index BEFORE maybe_rotate — the file is
        // being added to the current (not-yet-rotated) shard, which
        // becomes index `writers.len()` when it's pushed.
        let shard_idx = self.writers.len();
        self.current.add_file(file_info)?;
        self.maybe_rotate();
        Ok(shard_idx)
    }

    /// Finalize all shards, returning `(bytes, hash)` pairs.
    ///
    /// When bloom is enabled, each shard is finalized with an appended bloom
    /// filter (footer version 2). Otherwise, plain v1 finalization is used.
    ///
    /// Empty shards are skipped.
    ///
    /// # Errors
    /// Returns `XetError::Internal` on serialization failure.
    pub fn finalize(self) -> Result<Vec<(Vec<u8>, MerkleHash)>> {
        let mut results = Vec::with_capacity(self.writers.len() + 1);

        for (w, hashes) in self.writers {
            if !w.is_empty() {
                results.push(Self::finalize_one(w, &hashes, self.bloom_enabled)?);
            }
        }
        if !self.current.is_empty() {
            results.push(Self::finalize_one(
                self.current,
                &self.current_hashes,
                self.bloom_enabled,
            )?);
        }
        Ok(results)
    }

    /// Finalize a single shard writer, optionally with bloom.
    fn finalize_one(
        writer: ShardWriter,
        hashes: &ShardHashes,
        bloom_enabled: bool,
    ) -> Result<(Vec<u8>, MerkleHash)> {
        if bloom_enabled {
            writer.finalize_with_bloom(&hashes.file_hashes, &hashes.chunk_hashes)
        } else {
            writer.finalize()
        }
    }

    /// If the current shard exceeds the soft cap, archive it and start fresh.
    fn maybe_rotate(&mut self) {
        if self.current.should_split() {
            let full_writer = std::mem::take(&mut self.current);
            let full_hashes = std::mem::replace(&mut self.current_hashes, ShardHashes::new());
            self.writers.push((full_writer, full_hashes));
        }
    }
}

impl Default for PushShardSession {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xet_core_structures::metadata_shard::file_structs::{
        FileDataSequenceEntry, FileDataSequenceHeader,
    };
    use xet_core_structures::metadata_shard::xorb_structs::{
        XorbChunkSequenceEntry, XorbChunkSequenceHeader,
    };

    fn make_xorb(hash_seed: u64, num_chunks: usize) -> Arc<MDBXorbInfo> {
        let xorb_hash = MerkleHash::from([hash_seed, hash_seed, hash_seed, hash_seed]);
        let chunks: Vec<XorbChunkSequenceEntry> = (0..num_chunks)
            .map(|i| {
                let h = hash_seed.wrapping_add(i as u64 + 1);
                XorbChunkSequenceEntry::new(
                    MerkleHash::from([h, h, h, h]),
                    1024u32,
                    (i as u32) * 1024,
                )
            })
            .collect();
        Arc::new(MDBXorbInfo {
            metadata: XorbChunkSequenceHeader::new(xorb_hash, num_chunks, num_chunks * 1024),
            chunks,
        })
    }

    fn make_file(hash_seed: u64, xorb_hash_seed: u64) -> MDBFileInfo {
        let file_hash = MerkleHash::from([hash_seed, hash_seed, hash_seed, hash_seed]);
        MDBFileInfo {
            metadata: FileDataSequenceHeader::new(file_hash, 1u32, false, false),
            segments: vec![FileDataSequenceEntry::new(
                MerkleHash::from([
                    xorb_hash_seed,
                    xorb_hash_seed,
                    xorb_hash_seed,
                    xorb_hash_seed,
                ]),
                4096u32,
                0u32,
                1u32,
            )],
            verification: vec![],
            metadata_ext: None,
        }
    }

    #[test]
    fn writer_empty_by_default() {
        let w = ShardWriter::new();
        assert!(w.is_empty());
        assert!(!w.should_split());
    }

    #[test]
    fn writer_add_xorb_and_file() {
        let mut w = ShardWriter::new();
        w.add_xorb(make_xorb(1, 4)).unwrap();
        w.add_file(make_file(10, 1)).unwrap();
        assert!(!w.is_empty());
        assert!(w.size() > 0);
    }

    #[test]
    fn writer_finalize_produces_valid_shard() {
        let mut w = ShardWriter::new();
        w.add_xorb(make_xorb(1, 2)).unwrap();
        w.add_file(make_file(10, 1)).unwrap();

        let (bytes, hash) = w.finalize().unwrap();
        assert!(!bytes.is_empty());
        assert_ne!(hash, MerkleHash::default());

        // Verify the bytes can be loaded back as a shard.
        let reader = ShardReader::from_bytes(Bytes::from(bytes), hash);
        assert_eq!(reader.hash(), hash);
    }

    #[test]
    fn reader_lazy_parse_and_file_lookup() {
        let file_hash = MerkleHash::from([10u64, 10, 10, 10]);
        let xorb = make_xorb(1, 2);

        let mut w = ShardWriter::new();
        w.add_xorb(xorb).unwrap();
        w.add_file(make_file(10, 1)).unwrap();

        let (bytes, hash) = w.finalize().unwrap();
        let reader = ShardReader::from_bytes(Bytes::from(bytes), hash);

        let info = reader.get_file_info(&file_hash).unwrap();
        assert!(info.is_some());
        let info = info.unwrap();
        assert_eq!(info.metadata.file_hash, file_hash);

        // Non-existent file returns None.
        let missing = MerkleHash::from([999u64, 999, 999, 999]);
        assert!(reader.get_file_info(&missing).unwrap().is_none());
    }

    #[test]
    fn has_file_true_for_present_and_false_for_absent() {
        let file_hash = MerkleHash::from([10u64, 10, 10, 10]);
        let mut w = ShardWriter::new();
        w.add_xorb(make_xorb(1, 2)).unwrap();
        w.add_file(make_file(10, 1)).unwrap();

        let (bytes, hash) = w.finalize().unwrap();
        let reader = ShardReader::from_bytes(Bytes::from(bytes), hash);

        assert!(reader.has_file(&file_hash));

        let missing = MerkleHash::from([999u64, 999, 999, 999]);
        assert!(!reader.has_file(&missing));
    }

    #[test]
    fn has_file_returns_false_on_corrupt_shard() {
        // A byte blob that cannot be parsed as a shard must be treated as
        // "file not present" so a stale shard-hint gracefully falls back to
        // the file-index lookup path.
        let garbage = Bytes::from_static(b"not a shard");
        let reader = ShardReader::from_bytes(garbage, MerkleHash::default());
        let any_hash = MerkleHash::from([1u64, 2, 3, 4]);
        assert!(!reader.has_file(&any_hash));
    }

    #[test]
    fn reader_chunk_dedup_query() {
        let xorb = make_xorb(1, 3);
        let chunk_hashes: Vec<MerkleHash> = xorb.chunks.iter().map(|c| c.chunk_hash).collect();

        let mut w = ShardWriter::new();
        w.add_xorb(xorb).unwrap();
        let (bytes, hash) = w.finalize().unwrap();

        let reader = ShardReader::from_bytes(Bytes::from(bytes), hash);
        let result = reader.chunk_dedup_query(&chunk_hashes).unwrap();
        assert!(result.is_some());
        let (matched, _entry) = result.unwrap();
        assert_eq!(matched, 3);
    }

    #[test]
    fn push_session_single_shard_when_small() {
        let mut session = PushShardSession::new();
        session.add_xorb(make_xorb(1, 2)).unwrap();
        session.add_file(make_file(10, 1)).unwrap();

        let shards = session.finalize().unwrap();
        assert_eq!(shards.len(), 1);
        assert!(!shards[0].0.is_empty());
    }

    #[test]
    fn push_session_empty_produces_no_shards() {
        let session = PushShardSession::new();
        let shards = session.finalize().unwrap();
        assert!(shards.is_empty());
    }

    #[test]
    fn push_session_splits_at_size_cap() {
        let mut session = PushShardSession::with_bloom(false);
        // Override the cap on the current writer to force rotation.
        session.current.size_cap = 1; // 1 byte cap — any entry triggers split.

        session.add_xorb(make_xorb(1, 2)).unwrap();
        // After adding, should_split is true → rotated.
        session.add_xorb(make_xorb(2, 2)).unwrap();

        let shards = session.finalize().unwrap();
        // At least 2 shards since each add triggers rotation.
        assert!(shards.len() >= 2);
    }

    #[test]
    fn v1_shard_has_no_bloom() {
        let mut w = ShardWriter::new();
        w.add_xorb(make_xorb(1, 2)).unwrap();
        let (bytes, hash) = w.finalize().unwrap();

        let reader = ShardReader::from_bytes(Bytes::from(bytes), hash);
        assert!(reader.bloom().unwrap().is_none());
    }

    #[test]
    fn v2_shard_has_bloom_with_no_false_negatives() {
        let xorb = make_xorb(1, 3);
        let chunk_hashes: Vec<MerkleHash> = xorb.chunks.iter().map(|c| c.chunk_hash).collect();
        let file_hash = MerkleHash::from([10u64, 10, 10, 10]);

        let mut w = ShardWriter::new();
        w.add_xorb(xorb).unwrap();
        w.add_file(make_file(10, 1)).unwrap();

        let (bytes, hash) = w.finalize_with_bloom(&[file_hash], &chunk_hashes).unwrap();

        let reader = ShardReader::from_bytes(Bytes::from(bytes), hash);
        let bloom = reader.bloom().unwrap().expect("v2 shard should have bloom");

        assert!(bloom.maybe_contains_file(&file_hash));
        for ch in &chunk_hashes {
            assert!(bloom.maybe_contains_chunk(ch));
        }
    }

    #[test]
    fn push_session_with_bloom_produces_v2_shards() {
        let xorb = make_xorb(1, 3);
        let chunk_hashes: Vec<MerkleHash> = xorb.chunks.iter().map(|c| c.chunk_hash).collect();

        let mut session = PushShardSession::new(); // bloom enabled by default
        session.add_xorb(xorb).unwrap();
        session.add_file(make_file(10, 1)).unwrap();

        let shards = session.finalize().unwrap();
        assert_eq!(shards.len(), 1);

        let reader = ShardReader::from_bytes(Bytes::from(shards[0].0.clone()), shards[0].1);
        let bloom = reader.bloom().unwrap().expect("should be v2 with bloom");

        // File hash should be present.
        let file_hash = MerkleHash::from([10u64, 10, 10, 10]);
        assert!(bloom.maybe_contains_file(&file_hash));

        // Chunk hashes should be present.
        for ch in &chunk_hashes {
            assert!(bloom.maybe_contains_chunk(ch));
        }
    }

    #[test]
    fn push_session_without_bloom_produces_v1_shards() {
        let mut session = PushShardSession::with_bloom(false);
        session.add_xorb(make_xorb(1, 2)).unwrap();
        session.add_file(make_file(10, 1)).unwrap();

        let shards = session.finalize().unwrap();
        assert_eq!(shards.len(), 1);

        let reader = ShardReader::from_bytes(Bytes::from(shards[0].0.clone()), shards[0].1);
        assert!(reader.bloom().unwrap().is_none());
    }
}
