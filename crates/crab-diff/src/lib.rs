//! Chunk-diff reports and pure comparison algorithms.

pub mod chunk_comparator;
pub mod chunk_sequence;
pub mod pointer_pairs;
pub mod types;

pub use chunk_comparator::compare_terms;
pub use chunk_sequence::{ChunkOrigin, ChunkSequence, ChunkSpan, compare_sequences};
pub use pointer_pairs::pair_files;
pub use types::{
    ChunkDiffMetrics, ChunkDiffReport, ChunkSequenceSourceKind, DiffSummary, FileDiffEntry,
    FileStatus, OutputMode, SegmentDiff, SegmentStatus,
};
