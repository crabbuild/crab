//! Immutable, placement-independent recipes for staged file versions.

use crab_diff::chunk_sequence::{ChunkSequence, ChunkSpan};
use crab_diff::types::ChunkSequenceSourceKind;
use crab_xet::hash::MerkleHash;

use crate::{Result, StagingError};

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

/// Incrementally records the exact ordered CDC output for one file read.
///
/// The recorder owns no payload bytes or remote-placement facts. It is the
/// common recipe boundary for streaming add, filter clean, and recovery.
#[derive(Debug, Clone)]
pub struct RecipeRecorder {
    policy: ChunkingPolicyId,
    chunks: Vec<(MerkleHash, u64)>,
    recorded_bytes: u64,
}

impl RecipeRecorder {
    #[must_use]
    pub const fn new(policy: ChunkingPolicyId) -> Self {
        Self {
            policy,
            chunks: Vec::new(),
            recorded_bytes: 0,
        }
    }

    /// Record one occurrence, preserving repeats and checking byte overflow.
    pub fn record(&mut self, chunk_hash: MerkleHash, size: u64) -> Result<()> {
        self.recorded_bytes = self.recorded_bytes.checked_add(size).ok_or_else(|| {
            StagingError::StagingCorrupt("recipe byte length overflow".to_owned())
        })?;
        self.chunks.push((chunk_hash, size));
        Ok(())
    }

    #[must_use]
    pub fn chunks(&self) -> &[(MerkleHash, u64)] {
        &self.chunks
    }

    /// Seal the immutable recipe after the whole-file hash is known.
    pub fn seal(self, file_hash: MerkleHash, file_size: u64) -> Result<FileRecipe> {
        if self.recorded_bytes != file_size {
            return Err(StagingError::StagingCorrupt(format!(
                "recipe recorder covered {} bytes, expected {file_size}",
                self.recorded_bytes
            )));
        }
        FileRecipe::from_staged_chunks(self.policy, file_hash, file_size, &self.chunks)
    }
}

/// Immutable recipe for reconstructing one content-addressed file version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRecipe {
    policy: ChunkingPolicyId,
    sequence: ChunkSequence,
    recipe_hash: [u8; 32],
}

impl FileRecipe {
    /// Build and validate a recipe from the shared ordered chunk model.
    pub fn new(policy: ChunkingPolicyId, sequence: ChunkSequence) -> Result<Self> {
        if sequence.source != ChunkSequenceSourceKind::Staged {
            return Err(StagingError::StagingCorrupt(
                "new file recipes must originate from staged bytes".to_owned(),
            ));
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
        if expected_offset != sequence.file_size {
            return Err(StagingError::StagingCorrupt(format!(
                "recipe chunks cover {expected_offset} bytes, expected {}",
                sequence.file_size
            )));
        }
        let recipe_hash = recipe_hash(policy, &sequence);
        Ok(Self {
            policy,
            sequence,
            recipe_hash,
        })
    }

    /// Build a checked recipe from staging's ordered hash/size pairs.
    pub fn from_staged_chunks(
        policy: ChunkingPolicyId,
        file_hash: MerkleHash,
        file_size: u64,
        chunks: &[(MerkleHash, u64)],
    ) -> Result<Self> {
        Self::new(
            policy,
            ChunkSequence::from_staged(file_hash, file_size, chunks),
        )
    }

    #[must_use]
    pub const fn policy(&self) -> ChunkingPolicyId {
        self.policy
    }

    #[must_use]
    pub fn sequence(&self) -> &ChunkSequence {
        &self.sequence
    }

    #[must_use]
    pub const fn hash(&self) -> [u8; 32] {
        self.recipe_hash
    }
}

fn recipe_hash(policy: ChunkingPolicyId, sequence: &ChunkSequence) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab file recipe v1\0");
    hasher.update(policy.as_str().as_bytes());
    hasher.update(&<[u8; 32]>::from(sequence.file_hash));
    hasher.update(&sequence.file_size.to_le_bytes());
    hasher.update(&(sequence.spans.len() as u64).to_le_bytes());
    for chunk in &sequence.spans {
        hasher.update(&<[u8; 32]>::from(chunk.chunk_hash));
        hasher.update(&chunk.len.to_le_bytes());
    }
    *hasher.finalize().as_bytes()
}

#[cfg(test)]
mod tests {
    use crab_diff::chunk_sequence::ChunkOrigin;

    use super::*;

    fn hash(byte: u8) -> MerkleHash {
        MerkleHash::from([byte; 32])
    }

    #[test]
    fn repeated_chunks_remain_ordered_occurrences() {
        let chunks = [(hash(1), 3), (hash(2), 5), (hash(1), 3)];
        let recipe =
            FileRecipe::from_staged_chunks(ChunkingPolicyId::XetGearV1_64KiB, hash(9), 11, &chunks)
                .unwrap();

        assert_eq!(recipe.sequence().spans.len(), 3);
        assert_eq!(recipe.sequence().spans[0].chunk_hash, hash(1));
        assert_eq!(recipe.sequence().spans[2].chunk_hash, hash(1));
        assert_eq!(recipe.sequence().spans[2].offset, 8);
    }

    #[test]
    fn recipe_hash_ignores_remote_placement() {
        let chunks = [(hash(1), 3), (hash(2), 5)];
        let local =
            FileRecipe::from_staged_chunks(ChunkingPolicyId::XetGearV1_64KiB, hash(9), 8, &chunks)
                .unwrap();
        let mut remote_sequence = local.sequence().clone();
        for (index, span) in remote_sequence.spans.iter_mut().enumerate() {
            span.origin = ChunkOrigin {
                xorb_hash: Some(hash(7)),
                xorb_chunk_index: Some(index as u32),
            };
        }
        let remote = FileRecipe::new(ChunkingPolicyId::XetGearV1_64KiB, remote_sequence).unwrap();

        assert_eq!(local.hash(), remote.hash());
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
    fn recorder_preserves_repeats_and_seals_exact_size() {
        let mut recorder = RecipeRecorder::new(ChunkingPolicyId::XetGearV1_64KiB);
        recorder.record(hash(1), 3).unwrap();
        recorder.record(hash(2), 5).unwrap();
        recorder.record(hash(1), 3).unwrap();

        let recipe = recorder.seal(hash(9), 11).unwrap();

        assert_eq!(recipe.sequence().spans.len(), 3);
        assert_eq!(recipe.sequence().spans[2].chunk_hash, hash(1));
        assert_eq!(recipe.sequence().spans[2].offset, 8);
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
