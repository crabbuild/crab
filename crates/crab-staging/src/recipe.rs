//! Immutable, placement-independent recipes for staged file versions.

use crab_diff::chunk_sequence::{ChunkSequence, ChunkSpan};
use crab_diff::types::ChunkSequenceSourceKind;
use crab_xet::hash::MerkleHash;

use crate::{Result, StagingError};

/// Maximum number of ordered terms returned by one indexed recipe read.
pub const RECIPE_PAGE_ENTRIES: usize = 512;

const SEQUENCE_DOMAIN: &[u8] = b"crab recipe sequence v1\0";
const PAGE_DOMAIN: &[u8] = b"crab recipe page v1\0";
const PAGE_ROOT_DOMAIN: &[u8] = b"crab recipe page root v1\0";
const RECIPE_DOMAIN: &[u8] = b"crab file recipe v1\0";

/// Versioned identity of the content-defined chunking contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkingPolicyId {
    /// xet-data gearhash defaults pinned by Crab's conformance fixtures.
    XetGearV1_64KiB,
}

impl ChunkingPolicyId {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::XetGearV1_64KiB => "xet-gear-v1-64k",
        }
    }

    /// Parse a durable policy identifier from staging metadata.
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "xet-gear-v1-64k" => Ok(Self::XetGearV1_64KiB),
            other => Err(StagingError::StagingCorrupt(format!(
                "unsupported chunking policy {other}"
            ))),
        }
    }
}

/// One ordered recipe occurrence. Repeated hashes remain separate spans.
pub type RecipeChunk = ChunkSpan;

/// One bounded, contiguous page from the canonical indexed recipe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecipePage {
    pub start_occurrence: u64,
    pub start_offset: u64,
    pub chunks: Vec<RecipeChunk>,
}

impl RecipePage {
    #[must_use]
    pub fn next_occurrence(&self) -> u64 {
        self.start_occurrence
            .saturating_add(self.chunks.len() as u64)
    }

    #[must_use]
    pub fn next_offset(&self) -> u64 {
        self.chunks.last().map_or(self.start_offset, |chunk| {
            chunk.offset.saturating_add(chunk.len)
        })
    }
}

/// Incrementally records the exact ordered CDC output for one file read.
///
/// Ordered terms are durably spilled by the streaming caller. This recorder
/// retains only fixed-size hashing and count state, independent of file size.
#[derive(Debug, Clone)]
pub struct RecipeRecorder {
    policy: ChunkingPolicyId,
    sequence_hasher: blake3::Hasher,
    page_root_hasher: blake3::Hasher,
    page_buffer: Vec<(MerkleHash, u64)>,
    page_count: u64,
    page_start_offset: u64,
    recorded_chunks: u64,
    recorded_bytes: u64,
}

impl RecipeRecorder {
    #[must_use]
    pub fn new(policy: ChunkingPolicyId) -> Self {
        let mut sequence_hasher = blake3::Hasher::new();
        sequence_hasher.update(SEQUENCE_DOMAIN);
        let mut page_root_hasher = blake3::Hasher::new();
        page_root_hasher.update(PAGE_ROOT_DOMAIN);
        Self {
            policy,
            sequence_hasher,
            page_root_hasher,
            page_buffer: Vec::with_capacity(RECIPE_PAGE_ENTRIES),
            page_count: 0,
            page_start_offset: 0,
            recorded_chunks: 0,
            recorded_bytes: 0,
        }
    }

    /// Record one occurrence, preserving repeats and checking count/byte overflow.
    pub fn record(&mut self, chunk_hash: MerkleHash, size: u64) -> Result<()> {
        self.recorded_chunks = self.recorded_chunks.checked_add(1).ok_or_else(|| {
            StagingError::StagingCorrupt("recipe occurrence count overflow".to_owned())
        })?;
        self.recorded_bytes = self.recorded_bytes.checked_add(size).ok_or_else(|| {
            StagingError::StagingCorrupt("recipe byte length overflow".to_owned())
        })?;
        update_sequence_hasher(&mut self.sequence_hasher, chunk_hash, size);
        self.page_buffer.push((chunk_hash, size));
        if self.page_buffer.len() == RECIPE_PAGE_ENTRIES {
            self.flush_page()?;
        }
        Ok(())
    }

    #[must_use]
    pub const fn chunk_count(&self) -> u64 {
        self.recorded_chunks
    }

    #[must_use]
    pub const fn recorded_bytes(&self) -> u64 {
        self.recorded_bytes
    }

    /// Seal the immutable root after the whole-file hash is known.
    pub fn seal(mut self, file_hash: MerkleHash, file_size: u64) -> Result<FileRecipe> {
        if self.recorded_bytes != file_size {
            return Err(StagingError::StagingCorrupt(format!(
                "recipe recorder covered {} bytes, expected {file_size}",
                self.recorded_bytes
            )));
        }
        self.flush_page()?;
        let sequence_hash = *self.sequence_hasher.finalize().as_bytes();
        let page_root_hash = *self.page_root_hasher.finalize().as_bytes();
        Ok(FileRecipe::from_root(
            self.policy,
            file_hash,
            file_size,
            self.recorded_chunks,
            sequence_hash,
            self.page_count,
            page_root_hash,
        ))
    }

    fn flush_page(&mut self) -> Result<()> {
        if self.page_buffer.is_empty() {
            return Ok(());
        }
        let start_occurrence = self
            .page_count
            .checked_mul(RECIPE_PAGE_ENTRIES as u64)
            .ok_or_else(|| {
                StagingError::StagingCorrupt("recipe page occurrence overflow".to_owned())
            })?;
        let digest = page_hash(start_occurrence, self.page_start_offset, &self.page_buffer)?;
        self.page_root_hasher.update(&digest);
        self.page_count = self
            .page_count
            .checked_add(1)
            .ok_or_else(|| StagingError::StagingCorrupt("recipe page count overflow".to_owned()))?;
        self.page_start_offset =
            self.page_buffer
                .iter()
                .try_fold(self.page_start_offset, |offset, (_, size)| {
                    offset.checked_add(*size).ok_or_else(|| {
                        StagingError::StagingCorrupt("recipe page byte overflow".to_owned())
                    })
                })?;
        self.page_buffer.clear();
        Ok(())
    }
}

/// Immutable root for a recipe whose ordered terms live in the staging index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRecipe {
    policy: ChunkingPolicyId,
    file_hash: MerkleHash,
    file_size: u64,
    chunk_count: u64,
    sequence_hash: [u8; 32],
    page_count: u64,
    page_root_hash: [u8; 32],
    recipe_hash: [u8; 32],
}

impl FileRecipe {
    /// Build and validate a recipe root from the shared ordered chunk model.
    pub fn new(policy: ChunkingPolicyId, sequence: ChunkSequence) -> Result<Self> {
        if sequence.source != ChunkSequenceSourceKind::Staged {
            return Err(StagingError::StagingCorrupt(
                "new file recipes must originate from staged bytes".to_owned(),
            ));
        }
        let chunks = sequence
            .spans
            .iter()
            .map(|chunk| (chunk.chunk_hash, chunk.len));
        let identity = sequence_identity(chunks)?;
        let (chunk_count, covered_bytes, sequence_hash) =
            (identity.chunk_count, identity.bytes, identity.sequence_hash);
        if covered_bytes != sequence.file_size {
            return Err(StagingError::StagingCorrupt(format!(
                "recipe chunks cover {covered_bytes} bytes, expected {}",
                sequence.file_size
            )));
        }
        let mut expected_offset = 0u64;
        for chunk in &sequence.spans {
            if chunk.offset != expected_offset {
                return Err(StagingError::StagingCorrupt(format!(
                    "recipe occurrence offset {} does not match expected {expected_offset}",
                    chunk.offset
                )));
            }
            expected_offset = expected_offset.checked_add(chunk.len).ok_or_else(|| {
                StagingError::StagingCorrupt("recipe byte length overflow".to_owned())
            })?;
        }
        Ok(Self::from_root(
            policy,
            sequence.file_hash,
            sequence.file_size,
            chunk_count,
            sequence_hash,
            identity.page_count,
            identity.page_root_hash,
        ))
    }

    /// Build a checked root from an already bounded caller-owned chunk slice.
    pub fn from_staged_chunks(
        policy: ChunkingPolicyId,
        file_hash: MerkleHash,
        file_size: u64,
        chunks: &[(MerkleHash, u64)],
    ) -> Result<Self> {
        let identity = sequence_identity(chunks.iter().copied())?;
        let (chunk_count, covered_bytes, sequence_hash) =
            (identity.chunk_count, identity.bytes, identity.sequence_hash);
        if covered_bytes != file_size {
            return Err(StagingError::StagingCorrupt(format!(
                "recipe chunks cover {covered_bytes} bytes, expected {file_size}"
            )));
        }
        Ok(Self::from_root(
            policy,
            file_hash,
            file_size,
            chunk_count,
            sequence_hash,
            identity.page_count,
            identity.page_root_hash,
        ))
    }

    pub(crate) fn from_stored_root(
        policy: ChunkingPolicyId,
        file_hash: MerkleHash,
        file_size: u64,
        chunk_count: u64,
        sequence_hash: [u8; 32],
        page_count: u64,
        page_root_hash: [u8; 32],
        stored_recipe_hash: [u8; 32],
    ) -> Result<Self> {
        let recipe = Self::from_root(
            policy,
            file_hash,
            file_size,
            chunk_count,
            sequence_hash,
            page_count,
            page_root_hash,
        );
        if recipe.recipe_hash != stored_recipe_hash {
            return Err(StagingError::StagingCorrupt(
                "stored recipe root digest does not match its metadata".to_owned(),
            ));
        }
        Ok(recipe)
    }

    fn from_root(
        policy: ChunkingPolicyId,
        file_hash: MerkleHash,
        file_size: u64,
        chunk_count: u64,
        sequence_hash: [u8; 32],
        page_count: u64,
        page_root_hash: [u8; 32],
    ) -> Self {
        let recipe_hash = recipe_hash(
            policy,
            file_hash,
            file_size,
            chunk_count,
            &sequence_hash,
            page_count,
            &page_root_hash,
        );
        Self {
            policy,
            file_hash,
            file_size,
            chunk_count,
            sequence_hash,
            page_count,
            page_root_hash,
            recipe_hash,
        }
    }

    #[must_use]
    pub const fn policy(&self) -> ChunkingPolicyId {
        self.policy
    }

    #[must_use]
    pub const fn file_hash(&self) -> MerkleHash {
        self.file_hash
    }

    #[must_use]
    pub const fn file_size(&self) -> u64 {
        self.file_size
    }

    #[must_use]
    pub const fn chunk_count(&self) -> u64 {
        self.chunk_count
    }

    #[must_use]
    pub const fn sequence_hash(&self) -> [u8; 32] {
        self.sequence_hash
    }

    #[must_use]
    pub const fn page_count(&self) -> u64 {
        self.page_count
    }

    #[must_use]
    pub const fn page_root_hash(&self) -> [u8; 32] {
        self.page_root_hash
    }

    #[must_use]
    pub const fn hash(&self) -> [u8; 32] {
        self.recipe_hash
    }
}

pub(crate) struct RecipeIdentity {
    pub chunk_count: u64,
    pub bytes: u64,
    pub sequence_hash: [u8; 32],
    pub page_count: u64,
    pub page_root_hash: [u8; 32],
}

pub(crate) fn sequence_identity(
    chunks: impl IntoIterator<Item = (MerkleHash, u64)>,
) -> Result<RecipeIdentity> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(SEQUENCE_DOMAIN);
    let mut page_root_hasher = blake3::Hasher::new();
    page_root_hasher.update(PAGE_ROOT_DOMAIN);
    let mut page = Vec::with_capacity(RECIPE_PAGE_ENTRIES);
    let mut page_count = 0u64;
    let mut page_start_offset = 0u64;
    let mut count = 0u64;
    let mut bytes = 0u64;
    for (chunk_hash, size) in chunks {
        count = count.checked_add(1).ok_or_else(|| {
            StagingError::StagingCorrupt("recipe occurrence count overflow".to_owned())
        })?;
        bytes = bytes.checked_add(size).ok_or_else(|| {
            StagingError::StagingCorrupt("recipe byte length overflow".to_owned())
        })?;
        update_sequence_hasher(&mut hasher, chunk_hash, size);
        page.push((chunk_hash, size));
        if page.len() == RECIPE_PAGE_ENTRIES {
            let start = page_count
                .checked_mul(RECIPE_PAGE_ENTRIES as u64)
                .ok_or_else(|| {
                    StagingError::StagingCorrupt("recipe page occurrence overflow".to_owned())
                })?;
            page_root_hasher.update(&page_hash(start, page_start_offset, &page)?);
            page_start_offset = bytes;
            page_count = page_count.checked_add(1).ok_or_else(|| {
                StagingError::StagingCorrupt("recipe page count overflow".to_owned())
            })?;
            page.clear();
        }
    }
    if !page.is_empty() {
        let start = page_count
            .checked_mul(RECIPE_PAGE_ENTRIES as u64)
            .ok_or_else(|| {
                StagingError::StagingCorrupt("recipe page occurrence overflow".to_owned())
            })?;
        page_root_hasher.update(&page_hash(start, page_start_offset, &page)?);
        page_count = page_count
            .checked_add(1)
            .ok_or_else(|| StagingError::StagingCorrupt("recipe page count overflow".to_owned()))?;
    }
    Ok(RecipeIdentity {
        chunk_count: count,
        bytes,
        sequence_hash: *hasher.finalize().as_bytes(),
        page_count,
        page_root_hash: *page_root_hasher.finalize().as_bytes(),
    })
}

pub(crate) fn new_sequence_hasher() -> blake3::Hasher {
    let mut hasher = blake3::Hasher::new();
    hasher.update(SEQUENCE_DOMAIN);
    hasher
}

pub(crate) fn update_sequence_hasher(
    hasher: &mut blake3::Hasher,
    chunk_hash: MerkleHash,
    size: u64,
) {
    hasher.update(&<[u8; 32]>::from(chunk_hash));
    hasher.update(&size.to_le_bytes());
}

pub(crate) fn page_hash(
    start_occurrence: u64,
    start_offset: u64,
    chunks: &[(MerkleHash, u64)],
) -> Result<[u8; 32]> {
    if chunks.is_empty() || chunks.len() > RECIPE_PAGE_ENTRIES {
        return Err(StagingError::StagingCorrupt(format!(
            "recipe page has invalid term count {}",
            chunks.len()
        )));
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update(PAGE_DOMAIN);
    hasher.update(&start_occurrence.to_le_bytes());
    hasher.update(&start_offset.to_le_bytes());
    hasher.update(&(chunks.len() as u64).to_le_bytes());
    for (chunk_hash, size) in chunks {
        update_sequence_hasher(&mut hasher, *chunk_hash, *size);
    }
    Ok(*hasher.finalize().as_bytes())
}

pub(crate) fn new_page_root_hasher() -> blake3::Hasher {
    let mut hasher = blake3::Hasher::new();
    hasher.update(PAGE_ROOT_DOMAIN);
    hasher
}

fn recipe_hash(
    policy: ChunkingPolicyId,
    file_hash: MerkleHash,
    file_size: u64,
    chunk_count: u64,
    sequence_hash: &[u8; 32],
    page_count: u64,
    page_root_hash: &[u8; 32],
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(RECIPE_DOMAIN);
    hasher.update(policy.as_str().as_bytes());
    hasher.update(&<[u8; 32]>::from(file_hash));
    hasher.update(&file_size.to_le_bytes());
    hasher.update(&chunk_count.to_le_bytes());
    hasher.update(sequence_hash);
    hasher.update(&page_count.to_le_bytes());
    hasher.update(page_root_hash);
    *hasher.finalize().as_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(byte: u8) -> MerkleHash {
        MerkleHash::from([byte; 32])
    }

    #[test]
    fn repeated_chunks_contribute_distinct_ordered_terms() {
        let chunks = [(hash(1), 3), (hash(2), 5), (hash(1), 3)];
        let recipe =
            FileRecipe::from_staged_chunks(ChunkingPolicyId::XetGearV1_64KiB, hash(9), 11, &chunks)
                .unwrap();
        let reordered = FileRecipe::from_staged_chunks(
            ChunkingPolicyId::XetGearV1_64KiB,
            hash(9),
            11,
            &[(hash(1), 3), (hash(1), 3), (hash(2), 5)],
        )
        .unwrap();

        assert_eq!(recipe.chunk_count(), 3);
        assert_ne!(recipe.sequence_hash(), reordered.sequence_hash());
    }

    #[test]
    fn checked_construction_rejects_size_mismatch() {
        let error = FileRecipe::from_staged_chunks(
            ChunkingPolicyId::XetGearV1_64KiB,
            hash(9),
            9,
            &[(hash(1), 8)],
        )
        .unwrap_err();

        assert!(matches!(error, StagingError::StagingCorrupt(_)));
    }

    #[test]
    fn recorder_is_constant_size_and_matches_reference_identity() {
        let chunks = (0u32..10_000)
            .map(|index| {
                (
                    hash(u8::try_from(index % 3 + 1).unwrap()),
                    u64::from(index % 11 + 1),
                )
            })
            .collect::<Vec<_>>();
        let mut recorder = RecipeRecorder::new(ChunkingPolicyId::XetGearV1_64KiB);
        for (chunk_hash, size) in &chunks {
            recorder.record(*chunk_hash, *size).unwrap();
            assert!(recorder.page_buffer.len() < RECIPE_PAGE_ENTRIES);
        }
        let file_size = chunks.iter().map(|(_, size)| *size).sum();

        let recipe = recorder.seal(hash(9), file_size).unwrap();
        let reference = FileRecipe::from_staged_chunks(
            ChunkingPolicyId::XetGearV1_64KiB,
            hash(9),
            file_size,
            &chunks,
        )
        .unwrap();

        assert_eq!(recipe, reference);
    }

    #[test]
    fn recorder_rejects_byte_count_overflow() {
        let mut recorder = RecipeRecorder::new(ChunkingPolicyId::XetGearV1_64KiB);
        recorder.record(hash(1), u64::MAX).unwrap();

        let error = recorder.record(hash(2), 1).unwrap_err();

        assert!(matches!(error, StagingError::StagingCorrupt(_)));
    }

    #[test]
    fn recorder_rejects_seal_size_mismatch() {
        let mut recorder = RecipeRecorder::new(ChunkingPolicyId::XetGearV1_64KiB);
        recorder.record(hash(1), 8).unwrap();

        let error = recorder.seal(hash(9), 9).unwrap_err();

        assert!(matches!(error, StagingError::StagingCorrupt(_)));
    }
}
