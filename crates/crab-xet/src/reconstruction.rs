//! File reconstruction terms over xorb chunk placements.

use std::collections::{HashMap, HashSet};
use std::fmt;

use xet_core_structures::merklehash::MerkleHash;

use crate::error::{Result, XetError};
use crate::xorb::format::ChunkPlacement;

/// Map from chunk hash to where that chunk was packed.
pub type ChunkPlacementMap = HashMap<MerkleHash, ChunkPlacement>;

/// A contiguous xorb-local chunk range used to reconstruct one file.
#[derive(Debug, Clone, PartialEq)]
pub struct FileTerm {
    /// Content hash of the xorb containing these chunks.
    pub xorb_hash: MerkleHash,
    /// First xorb-local chunk index, inclusive.
    pub chunk_start: u32,
    /// One past the last xorb-local chunk index.
    pub chunk_end: u32,
    /// Total uncompressed bytes across the term.
    pub unpacked_bytes: u32,
}

fn shard_format_overflow(field: &str, value: impl fmt::Display) -> XetError {
    XetError::ShardFormat {
        field: field.to_owned(),
        value: value.to_string(),
    }
}

fn checked_shard_add(field: &str, lhs: u32, rhs: u32) -> Result<u32> {
    lhs.checked_add(rhs)
        .ok_or_else(|| shard_format_overflow(field, u64::from(lhs).saturating_add(u64::from(rhs))))
}

fn checked_shard_len(field: &str, start: u32, end: u32) -> Result<u32> {
    end.checked_sub(start)
        .ok_or_else(|| XetError::Internal(format!("shard term {field} has end before start")))
}

/// Incremental file-term builder whose memory is independent of recipe length.
pub struct FileTermBuilder {
    terms: Vec<FileTerm>,
    current: Option<FileTerm>,
    emitted_starts: HashSet<(MerkleHash, u32)>,
    uncovered: usize,
    first_miss: Option<(u32, MerkleHash)>,
    chunk_count: u64,
}

impl FileTermBuilder {
    #[must_use]
    pub fn new() -> Self {
        Self {
            terms: Vec::new(),
            current: None,
            emitted_starts: HashSet::new(),
            uncovered: 0,
            first_miss: None,
            chunk_count: 0,
        }
    }

    /// Consume one ordered recipe occurrence.
    pub fn push(&mut self, chunk_hash: MerkleHash, placement: &ChunkPlacementMap) -> Result<()> {
        let file_index = u32::try_from(self.chunk_count)
            .map_err(|_| shard_format_overflow("file chunk index", self.chunk_count))?;
        self.chunk_count = self
            .chunk_count
            .checked_add(1)
            .ok_or_else(|| shard_format_overflow("file chunk count", u64::MAX))?;
        let Some(p) = placement.get(&chunk_hash) else {
            self.uncovered = self.uncovered.saturating_add(1);
            self.first_miss.get_or_insert((file_index, chunk_hash));
            return Ok(());
        };
        match &mut self.current {
            Some(term)
                if term.xorb_hash == p.xorb_hash
                    && term.chunk_end == p.chunk_index
                    && !self
                        .emitted_starts
                        .contains(&(term.xorb_hash, term.chunk_start)) =>
            {
                term.chunk_end = checked_shard_add("xorb chunk range end", term.chunk_end, 1)?;
                term.unpacked_bytes = checked_shard_add(
                    "file term uncompressed bytes",
                    term.unpacked_bytes,
                    p.uncompressed_size,
                )?;
            }
            _ => {
                if let Some(term) = self.current.take() {
                    self.emitted_starts
                        .insert((term.xorb_hash, term.chunk_start));
                    self.terms.push(term);
                }
                self.current = Some(FileTerm {
                    xorb_hash: p.xorb_hash,
                    chunk_start: p.chunk_index,
                    chunk_end: checked_shard_add("xorb chunk range end", p.chunk_index, 1)?,
                    unpacked_bytes: p.uncompressed_size,
                });
            }
        }
        Ok(())
    }

    /// Seal after every recipe occurrence has been consumed.
    pub fn finish(mut self, file_hash: &MerkleHash, expected_chunks: u64) -> Result<Vec<FileTerm>> {
        if let Some(term) = self.current.take() {
            self.terms.push(term);
        }
        if let Some((example_chunk_index, example_chunk_hash)) = self.first_miss {
            return Err(XetError::IncompleteShardReconstruction {
                file_hash: file_hash.hex(),
                path: None,
                uncovered_chunks: self.uncovered,
                example_chunk_hash: example_chunk_hash.hex(),
                example_chunk_index,
            });
        }
        if self.chunk_count != expected_chunks {
            return Err(XetError::IncompleteShardReconstruction {
                file_hash: file_hash.hex(),
                path: None,
                uncovered_chunks: usize::try_from(expected_chunks.saturating_sub(self.chunk_count))
                    .unwrap_or(usize::MAX),
                example_chunk_hash: String::new(),
                example_chunk_index: u32::try_from(self.chunk_count).unwrap_or(u32::MAX),
            });
        }
        Ok(self.terms)
    }
}

impl Default for FileTermBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Coalesce consecutive chunks in the same xorb into reconstruction terms.
pub fn build_file_terms(
    file_hash: &MerkleHash,
    chunk_hashes: &[MerkleHash],
    placement: &ChunkPlacementMap,
) -> Result<Vec<FileTerm>> {
    let mut builder = FileTermBuilder::new();
    for ch in chunk_hashes {
        builder.push(*ch, placement)?;
    }
    builder.finish(file_hash, chunk_hashes.len() as u64)
}

/// Validate that reconstruction terms cover a file's chunk list completely.
pub fn validate_term_coverage(
    file_hash: &MerkleHash,
    chunk_hashes: &[MerkleHash],
    terms: &[FileTerm],
) -> Result<()> {
    let covered_chunks = terms.iter().try_fold(0u32, |acc, term| {
        let len = checked_shard_len("range", term.chunk_start, term.chunk_end)?;
        checked_shard_add("covered chunk count", acc, len)
    })?;
    if covered_chunks as usize != chunk_hashes.len() {
        let mut covered = vec![false; chunk_hashes.len()];
        let mut pos = 0usize;
        for term in terms {
            let len = checked_shard_len("range", term.chunk_start, term.chunk_end)? as usize;
            for is_covered in covered
                .iter_mut()
                .take((pos + len).min(chunk_hashes.len()))
                .skip(pos)
            {
                *is_covered = true;
            }
            pos += len;
        }
        let (example_chunk_index, example_chunk_hash) = covered
            .iter()
            .position(|is_covered| !is_covered)
            .map(|i| (i as u32, chunk_hashes[i].hex()))
            .unwrap_or((
                0,
                chunk_hashes
                    .first()
                    .map(MerkleHash::hex)
                    .unwrap_or_default(),
            ));
        return Err(XetError::IncompleteShardReconstruction {
            file_hash: file_hash.hex(),
            path: None,
            uncovered_chunks: chunk_hashes.len().saturating_sub(covered_chunks as usize),
            example_chunk_hash,
            example_chunk_index,
        });
    }

    if !terms.is_empty() {
        let last_file_end = terms.iter().try_fold(0u32, |acc, term| {
            let len = checked_shard_len("range", term.chunk_start, term.chunk_end)?;
            checked_shard_add("covered chunk count", acc, len)
        })?;
        if (last_file_end as usize) != chunk_hashes.len() {
            return Err(XetError::IncompleteShardReconstruction {
                file_hash: file_hash.hex(),
                path: None,
                uncovered_chunks: chunk_hashes.len().saturating_sub(last_file_end as usize),
                example_chunk_hash: chunk_hashes
                    .get(last_file_end as usize)
                    .map(MerkleHash::hex)
                    .unwrap_or_default(),
                example_chunk_index: last_file_end,
            });
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_placement(xorb_hash: MerkleHash, chunk_index: u32, size: u32) -> ChunkPlacement {
        ChunkPlacement {
            chunk_hash: MerkleHash::default(),
            xorb_hash,
            chunk_index,
            uncompressed_size: size,
        }
    }

    #[test]
    fn all_chunks_in_same_xorb_produce_single_term() {
        let xorb_hash = MerkleHash::from([1u64, 1, 1, 1]);
        let mut placement = ChunkPlacementMap::new();
        let mut chunk_hashes = Vec::new();

        for idx in 0..10u32 {
            let chunk_hash = MerkleHash::from([100 + idx as u64, 0, 0, 0]);
            placement.insert(chunk_hash, make_placement(xorb_hash, idx, 1024));
            chunk_hashes.push(chunk_hash);
        }

        let terms = build_file_terms(&MerkleHash::default(), &chunk_hashes, &placement).unwrap();
        assert_eq!(terms.len(), 1);
        assert_eq!(terms[0].xorb_hash, xorb_hash);
        assert_eq!(terms[0].chunk_start, 0);
        assert_eq!(terms[0].chunk_end, 10);
        assert_eq!(terms[0].unpacked_bytes, 10 * 1024);
    }

    #[test]
    fn missing_chunk_reports_first_gap_and_total_uncovered_count() {
        let xorb_hash = MerkleHash::from([1u64, 1, 1, 1]);
        let file_hash = MerkleHash::from([9u64, 9, 9, 9]);
        let mut placement = ChunkPlacementMap::new();
        let mut chunk_hashes = Vec::new();

        for idx in 0..10u32 {
            let chunk_hash = MerkleHash::from([100 + idx as u64, 0, 0, 0]);
            chunk_hashes.push(chunk_hash);
            if idx != 5 {
                placement.insert(chunk_hash, make_placement(xorb_hash, idx, 1024));
            }
        }

        let err = build_file_terms(&file_hash, &chunk_hashes, &placement).unwrap_err();
        match err {
            XetError::IncompleteShardReconstruction {
                file_hash: got_file_hash,
                uncovered_chunks,
                example_chunk_hash,
                example_chunk_index,
                ..
            } => {
                assert_eq!(got_file_hash, file_hash.hex());
                assert_eq!(uncovered_chunks, 1);
                assert_eq!(example_chunk_index, 5);
                assert_eq!(example_chunk_hash, chunk_hashes[5].hex());
            }
            other => panic!("expected incomplete reconstruction, got {other:?}"),
        }
    }

    #[test]
    fn duplicate_chunk_hashes_at_multiple_file_positions_are_valid() {
        let xorb_a = MerkleHash::from([1u64, 1, 1, 1]);
        let xorb_b = MerkleHash::from([2u64, 2, 2, 2]);
        let dup_hash = MerkleHash::from([7u64, 7, 7, 7]);
        let mut placement = ChunkPlacementMap::new();
        placement.insert(dup_hash, make_placement(xorb_a, 5, 1024));

        let fillers: Vec<MerkleHash> = (0..3u32)
            .map(|idx| MerkleHash::from([100 + idx as u64, 0, 0, 0]))
            .collect();
        for (idx, hash) in fillers.iter().enumerate() {
            placement.insert(*hash, make_placement(xorb_b, idx as u32, 1024));
        }

        let chunk_hashes = vec![dup_hash, fillers[0], fillers[1], fillers[2], dup_hash];
        let file_hash = MerkleHash::from([9u64, 9, 9, 9]);
        let terms = build_file_terms(&file_hash, &chunk_hashes, &placement).unwrap();

        let covered: u32 = terms
            .iter()
            .map(|term| term.chunk_end - term.chunk_start)
            .sum();
        assert_eq!(covered as usize, chunk_hashes.len());
        validate_term_coverage(&file_hash, &chunk_hashes, &terms).unwrap();
    }
}
