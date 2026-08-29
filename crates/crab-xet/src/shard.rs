//! Thin wrappers around xet-core's `MDBInMemoryShard` for building and
//! reading shards, plus a one-shard-per-push session with 100 MiB splitting.
//!
//! Crab's canonical v1 shard may append a bloom section after xet-core's shard
//! body. An 8-byte `bloom_offset` and the `SH01` magic terminate the object.
//! A body without the optional bloom remains a v1 shard.

use std::collections::{HashMap, HashSet};
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
pub use xet_core_structures::metadata_shard::shard_file_handle::{
    MDBShardFile, ShardFileCache, new_shard_file_cache,
};
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

/// Magic bytes identifying the canonical v1 bloom trailer.
const SHARD_V1_MAGIC: &[u8; 4] = b"SH01";

/// Size of the v1 bloom trailer: `bloom_offset` (8 bytes) + magic (4 bytes).
const SHARD_V1_TRAILER_SIZE: usize = 12;

/// Thin wrapper around xet-core's `MDBInMemoryShard` for building shards.
pub struct ShardWriter {
    inner: MDBInMemoryShard,
    size_cap: u64,
    xorb_info: HashMap<MerkleHash, Arc<MDBXorbInfo>>,
}

impl ShardWriter {
    /// Create a new empty shard writer with the default 100 MiB soft cap.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: MDBInMemoryShard::default(),
            size_cap: SHARD_SIZE_CAP,
            xorb_info: HashMap::new(),
        }
    }

    /// Add xorb CAS info to the shard.
    ///
    /// # Errors
    /// Returns `XetError::Internal` if the underlying shard rejects the entry.
    pub fn add_xorb(&mut self, xorb_info: Arc<MDBXorbInfo>) -> Result<()> {
        let xorb_hash = xorb_info.metadata.xorb_hash;
        if let Some(existing) = self.xorb_info.get(&xorb_hash) {
            if existing.as_ref() == xorb_info.as_ref() {
                return Ok(());
            }
            return Err(XetError::Internal(format!(
                "shard supplies conflicting metadata for xorb {}",
                xorb_hash.hex()
            )));
        }
        self.inner
            .add_xorb_block(Arc::clone(&xorb_info))
            .map_err(|e| XetError::Internal(format!("shard add_xorb: {e}")))?;
        self.xorb_info.insert(xorb_hash, xorb_info);
        Ok(())
    }

    /// Add file reconstruction info.
    ///
    /// # Errors
    /// Returns `XetError::Internal` if any reconstruction dependency is
    /// absent or invalid, or if the underlying shard rejects the entry.
    pub fn add_file(&mut self, file_info: MDBFileInfo) -> Result<()> {
        validate_file_terms(&file_info, |xorb_hash| {
            self.xorb_info.get(xorb_hash).map(Arc::as_ref)
        })?;
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

    /// Serialize the canonical v1 shard with an appended bloom filter.
    ///
    /// Layout: `[xet shard body][bloom][bloom_offset: u64 LE][magic: "SH01"]`
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
        // Serialize the xet shard body first.
        let mut buf = Vec::new();
        MDBShardInfo::serialize_from(&mut buf, &self.inner, None)
            .map_err(|e| XetError::Internal(format!("shard finalize: {e}")))?;

        // Record where the bloom section starts.
        let bloom_offset = buf.len() as u64;

        // Build and append the canonical v1 bloom trailer.
        let bloom = ShardBloom::build(file_hashes, chunk_hashes);
        buf.extend_from_slice(&bloom.encode());
        buf.extend_from_slice(&bloom_offset.to_le_bytes());
        buf.extend_from_slice(SHARD_V1_MAGIC);

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

fn validate_file_terms<'a>(
    file_info: &MDBFileInfo,
    dependency_for: impl Fn(&MerkleHash) -> Option<&'a MDBXorbInfo>,
) -> Result<()> {
    for segment in &file_info.segments {
        let dependency = dependency_for(&segment.xorb_hash).ok_or_else(|| {
            XetError::Internal(format!(
                "shard file {} lacks xorb {}",
                file_info.metadata.file_hash.hex(),
                segment.xorb_hash.hex()
            ))
        })?;
        let start = usize::try_from(segment.chunk_index_start).map_err(|_| {
            XetError::Internal(format!(
                "shard file {} chunk start cannot be represented",
                file_info.metadata.file_hash.hex()
            ))
        })?;
        let end = usize::try_from(segment.chunk_index_end).map_err(|_| {
            XetError::Internal(format!(
                "shard file {} chunk end cannot be represented",
                file_info.metadata.file_hash.hex()
            ))
        })?;
        let selected = dependency.chunks.get(start..end).ok_or_else(|| {
            XetError::Internal(format!(
                "shard file {} range {start}..{end} exceeds xorb {} bounds",
                file_info.metadata.file_hash.hex(),
                segment.xorb_hash.hex()
            ))
        })?;
        let selected_bytes = selected.iter().try_fold(0u64, |total, chunk| {
            total
                .checked_add(u64::from(chunk.unpacked_segment_bytes))
                .ok_or_else(|| {
                    XetError::Internal(format!(
                        "shard file {} byte count overflow",
                        file_info.metadata.file_hash.hex()
                    ))
                })
        })?;
        if selected_bytes != u64::from(segment.unpacked_segment_bytes) {
            return Err(XetError::Internal(format!(
                "shard file {} range {start}..{end} covers {selected_bytes} bytes, expected {}",
                file_info.metadata.file_hash.hex(),
                segment.unpacked_segment_bytes
            )));
        }
    }
    Ok(())
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

    /// Try to read the optional bloom filter from a canonical v1 shard.
    ///
    /// Returns `None` when the v1 shard has no bloom. Returns an error when a
    /// present trailer is corrupt.
    ///
    /// # Errors
    /// Returns `XetError::CorruptObject` if the bloom section is malformed.
    pub fn bloom(&self) -> Result<Option<ShardBloom>> {
        let data = self.data.as_ref();
        if data.len() < SHARD_V1_TRAILER_SIZE {
            return Ok(None);
        }
        let tail = &data[data.len() - 4..];
        if tail != SHARD_V1_MAGIC {
            return Ok(None);
        }
        let offset_start = data.len() - SHARD_V1_TRAILER_SIZE;
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
    /// The optional canonical v1 bloom trailer identifies where the xet shard
    /// body ends.
    fn shard_info(&self) -> Result<&MDBShardInfo> {
        let result = self.parsed.get_or_init(|| {
            let data = self.data.as_ref();

            let shard_body = if data.len() >= SHARD_V1_TRAILER_SIZE
                && &data[data.len() - 4..] == SHARD_V1_MAGIC
            {
                let offset_start = data.len() - SHARD_V1_TRAILER_SIZE;
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

            let mut cursor = Cursor::new(shard_body);
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

    /// Returns the xet shard body without Crab's optional v1 bloom trailer.
    ///
    /// Shards without a bloom return the full data.
    #[must_use]
    pub fn v1_data(&self) -> &[u8] {
        let data = self.data.as_ref();
        if data.len() >= SHARD_V1_TRAILER_SIZE && &data[data.len() - 4..] == SHARD_V1_MAGIC {
            let offset_start = data.len() - SHARD_V1_TRAILER_SIZE;
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
    xorb_info: HashMap<MerkleHash, Arc<MDBXorbInfo>>,
}

impl ShardHashes {
    fn new() -> Self {
        Self {
            file_hashes: Vec::new(),
            chunk_hashes: Vec::new(),
            xorb_info: HashMap::new(),
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

    /// Create a session with a qualification-only soft cap override.
    ///
    /// This is not a product configuration surface; sibling producer tests
    /// use it to force real multi-shard boundaries with small fixtures.
    #[doc(hidden)]
    #[must_use]
    pub fn with_qualification_size_cap(size_cap: u64) -> Self {
        let mut session = Self::new();
        session.current.size_cap = size_cap;
        session
    }

    /// Add one file together with every xorb-info block it references.
    ///
    /// Returns the 0-based index of the shard this file was added to.
    /// The index corresponds to the position in the `finalize()` result
    /// vector. Rotation happens only after the complete bundle is present, so
    /// a file never lands in a shard that lacks one of its xorb dependencies.
    /// Xorb metadata shared by files in one shard is serialized once.
    ///
    /// # Errors
    /// Returns `XetError::Internal` when dependencies are missing or the
    /// underlying shard rejects an entry.
    pub fn add_file_bundle(
        &mut self,
        file_info: MDBFileInfo,
        dependencies: &[Arc<MDBXorbInfo>],
    ) -> Result<usize> {
        let required = file_info
            .segments
            .iter()
            .map(|segment| segment.xorb_hash)
            .collect::<HashSet<_>>();
        let supplied = dependencies
            .iter()
            .map(|dependency| dependency.metadata.xorb_hash)
            .collect::<HashSet<_>>();
        if required != supplied || supplied.len() != dependencies.len() {
            let missing = required.difference(&supplied).next();
            let extra = supplied.difference(&required).next();
            return Err(XetError::Internal(format!(
                "shard file bundle dependency mismatch for {}: missing={}, extra={}, duplicate={}",
                file_info.metadata.file_hash.hex(),
                missing.map_or_else(|| "none".to_owned(), MerkleHash::hex),
                extra.map_or_else(|| "none".to_owned(), MerkleHash::hex),
                supplied.len() != dependencies.len(),
            )));
        }

        let dependencies_by_hash = dependencies
            .iter()
            .map(|dependency| (dependency.metadata.xorb_hash, dependency.as_ref()))
            .collect::<HashMap<_, _>>();
        validate_file_terms(&file_info, |xorb_hash| {
            dependencies_by_hash.get(xorb_hash).copied()
        })?;
        for dependency in dependencies {
            let xorb_hash = dependency.metadata.xorb_hash;
            if self
                .current_hashes
                .xorb_info
                .get(&xorb_hash)
                .is_some_and(|existing| existing.as_ref() != dependency.as_ref())
            {
                return Err(XetError::Internal(format!(
                    "shard file bundle supplies conflicting metadata for xorb {}",
                    xorb_hash.hex()
                )));
            }
        }

        let mut ordered_dependencies = dependencies.iter().collect::<Vec<_>>();
        ordered_dependencies.sort_by_key(|dependency| dependency.metadata.xorb_hash);
        for dependency in ordered_dependencies {
            let xorb_hash = dependency.metadata.xorb_hash;
            if self.current_hashes.xorb_info.contains_key(&xorb_hash) {
                continue;
            }
            self.current_hashes
                .xorb_info
                .insert(xorb_hash, Arc::clone(dependency));
            if self.bloom_enabled {
                self.current_hashes
                    .chunk_hashes
                    .extend(dependency.chunks.iter().map(|chunk| chunk.chunk_hash));
            }
            self.current.add_xorb(Arc::clone(dependency))?;
        }

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
    /// When bloom is enabled, each v1 shard carries the canonical bloom
    /// trailer. Otherwise, the plain xet shard body is used.
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
                1024u32,
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
    fn writer_rejects_file_without_dependency_before_mutation() {
        let mut writer = ShardWriter::new();
        let error = writer
            .add_file(make_file(10, 1))
            .expect_err("missing dependency must fail");

        assert!(error.to_string().contains("lacks xorb"));
        assert!(writer.is_empty());
    }

    #[test]
    fn writer_rejects_conflicting_xorb_metadata() {
        let mut writer = ShardWriter::new();
        writer.add_xorb(make_xorb(1, 2)).unwrap();

        let error = writer
            .add_xorb(make_xorb(1, 3))
            .expect_err("same xorb hash with different metadata must fail");

        assert!(error.to_string().contains("conflicting metadata"));
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
        let xorb = make_xorb(1, 2);
        session.add_file_bundle(make_file(10, 1), &[xorb]).unwrap();

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

        session
            .add_file_bundle(make_file(10, 1), &[make_xorb(1, 2)])
            .unwrap();
        // After adding the complete first bundle, should_split is true → rotated.
        session
            .add_file_bundle(make_file(20, 2), &[make_xorb(2, 2)])
            .unwrap();

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
    fn canonical_v1_shard_has_bloom_with_no_false_negatives() {
        let xorb = make_xorb(1, 3);
        let chunk_hashes: Vec<MerkleHash> = xorb.chunks.iter().map(|c| c.chunk_hash).collect();
        let file_hash = MerkleHash::from([10u64, 10, 10, 10]);

        let mut w = ShardWriter::new();
        w.add_xorb(xorb).unwrap();
        w.add_file(make_file(10, 1)).unwrap();

        let (bytes, hash) = w.finalize_with_bloom(&[file_hash], &chunk_hashes).unwrap();

        let reader = ShardReader::from_bytes(Bytes::from(bytes), hash);
        let bloom = reader.bloom().unwrap().expect("v1 shard should have bloom");

        assert!(bloom.maybe_contains_file(&file_hash));
        for ch in &chunk_hashes {
            assert!(bloom.maybe_contains_chunk(ch));
        }
    }

    #[test]
    fn push_session_with_bloom_produces_canonical_v1_shards() {
        let xorb = make_xorb(1, 3);
        let chunk_hashes: Vec<MerkleHash> = xorb.chunks.iter().map(|c| c.chunk_hash).collect();

        let mut session = PushShardSession::new(); // bloom enabled by default
        session.add_file_bundle(make_file(10, 1), &[xorb]).unwrap();

        let shards = session.finalize().unwrap();
        assert_eq!(shards.len(), 1);

        let reader = ShardReader::from_bytes(Bytes::from(shards[0].0.clone()), shards[0].1);
        let bloom = reader
            .bloom()
            .unwrap()
            .expect("canonical v1 shard should have bloom");

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
        session
            .add_file_bundle(make_file(10, 1), &[make_xorb(1, 2)])
            .unwrap();

        let shards = session.finalize().unwrap();
        assert_eq!(shards.len(), 1);

        let reader = ShardReader::from_bytes(Bytes::from(shards[0].0.clone()), shards[0].1);
        assert!(reader.bloom().unwrap().is_none());
    }

    #[test]
    fn push_session_rejects_file_without_complete_dependency_set() {
        let mut session = PushShardSession::new();
        let error = session
            .add_file_bundle(make_file(10, 1), &[])
            .expect_err("missing dependency must fail");
        assert!(error.to_string().contains("dependency mismatch"));
    }

    #[test]
    fn push_session_rejects_invalid_bundle_before_mutation() {
        let mut session = PushShardSession::new();
        let dependency = make_xorb(1, 2);
        let mut file = make_file(10, 1);
        file.segments[0] = FileDataSequenceEntry::new(dependency.metadata.xorb_hash, 1024, 1, 3);

        let error = session
            .add_file_bundle(file, &[dependency])
            .expect_err("out-of-bounds dependency range must fail");
        assert!(error.to_string().contains("exceeds xorb"));
        assert!(session.current.is_empty());
        assert!(session.writers.is_empty());
    }

    #[test]
    fn push_session_rotation_keeps_each_file_with_its_dependencies() {
        let mut session = PushShardSession::with_bloom(false);
        session.current.size_cap = 1;
        session
            .add_file_bundle(make_file(10, 1), &[make_xorb(1, 2)])
            .unwrap();
        session
            .add_file_bundle(make_file(20, 2), &[make_xorb(2, 2)])
            .unwrap();

        let shards = session.finalize().unwrap();
        assert_eq!(shards.len(), 2);
        for (bytes, hash) in shards {
            let reader = ShardReader::from_bytes(Bytes::from(bytes), hash);
            let file_hashes = [
                MerkleHash::from([10u64, 10, 10, 10]),
                MerkleHash::from([20u64, 20, 20, 20]),
            ];
            let file = file_hashes
                .iter()
                .find_map(|file_hash| reader.get_file_info(file_hash).unwrap());
            let file = file.expect("one file per forced shard");
            for segment in file.segments {
                assert!(reader.get_xorb_info(&segment.xorb_hash).unwrap().is_some());
            }
        }
    }

    #[test]
    fn push_session_repeats_shared_dependency_across_partitions() {
        let mut session = PushShardSession::with_bloom(false);
        session.current.size_cap = 1;
        let shared = make_xorb(1, 2);
        session
            .add_file_bundle(make_file(10, 1), &[Arc::clone(&shared)])
            .unwrap();
        session
            .add_file_bundle(make_file(20, 1), &[shared])
            .unwrap();

        let shards = session.finalize().unwrap();
        assert_eq!(shards.len(), 2);
        for (bytes, _) in shards {
            let recipes = crate::shard_parse::extract_file_recipes(&Bytes::from(bytes))
                .expect("each partition must be dependency closed");
            assert_eq!(recipes.len(), 1);
            assert_eq!(recipes[0].chunks.len(), 1);
        }
    }

    #[test]
    fn push_session_accepts_zero_byte_file_without_dependencies() {
        let file_hash = MerkleHash::from([30u64, 30, 30, 30]);
        let file = MDBFileInfo {
            metadata: FileDataSequenceHeader::new(file_hash, 0, false, false),
            segments: Vec::new(),
            verification: Vec::new(),
            metadata_ext: None,
        };
        let mut session = PushShardSession::with_bloom(false);
        session.add_file_bundle(file, &[]).unwrap();

        let shards = session.finalize().unwrap();
        let recipes = crate::shard_parse::extract_file_recipes(&Bytes::from(shards[0].0.clone()))
            .expect("zero-byte recipe");
        assert_eq!(recipes[0].file_hash, file_hash);
        assert!(recipes[0].chunks.is_empty());
    }
}
