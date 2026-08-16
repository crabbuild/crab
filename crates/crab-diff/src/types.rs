//! Data models for chunk-level diff reports.

use serde::{Deserialize, Serialize};

/// Classification of a file between two refs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileStatus {
    Modified,
    Added,
    Deleted,
    /// File exists at both refs but is not a crab pointer (git-native).
    GitNative,
}

/// Classification of a single segment in a diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SegmentStatus {
    Unchanged,
    Added,
    Removed,
}

/// Source backing a chunk sequence used by the diff engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChunkSequenceSourceKind {
    Committed,
    Staged,
    Worktree,
}

/// Per-segment diff detail for verbose output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SegmentDiff {
    pub index: u32,
    pub status: SegmentStatus,
    pub old_xorb_hash: Option<String>,
    pub new_xorb_hash: Option<String>,
    pub old_chunk_range: Option<(u32, u32)>,
    pub new_chunk_range: Option<(u32, u32)>,
    pub bytes: u64,
}

/// Diff result for a single file.
///
/// `dedup_ratio` is `f64` which does not implement `Eq`. We derive
/// `PartialEq` normally and implement `Eq` manually — in practice
/// `dedup_ratio` is never NaN (it's computed as a ratio of byte counts).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChunkDiffReport {
    pub path: String,
    pub status: FileStatus,
    pub old_size: u64,
    pub new_size: u64,
    /// Segments unchanged between old and new.
    pub unchanged_segments: u32,
    pub unchanged_bytes: u64,
    /// Segments present in old but not in new.
    pub removed_segments: u32,
    pub removed_bytes: u64,
    /// Segments present in new but not in old.
    pub added_segments: u32,
    pub added_bytes: u64,
    /// Delta: bytes in new that are not in old (`added_bytes`).
    /// This is the transfer cost to go from old → new.
    pub delta_bytes: u64,
    /// Fraction of the larger version that is shared between versions.
    /// `unchanged_bytes / max(old_size, new_size)`. 0.0 for added/deleted.
    pub dedup_ratio: f64,
    /// Changed byte ranges within the file: `(offset, length)` pairs.
    /// Computed from segment positions. Empty when status is Added/Deleted.
    pub changed_byte_ranges: Vec<(u64, u64)>,
    /// Per-segment detail for verbose output. Empty unless requested.
    pub segment_details: Vec<SegmentDiff>,
    /// Format-aware annotations (e.g., tensor names, row groups).
    /// Empty when no format hint is available or parsing fails.
    pub annotations: Vec<String>,
    /// Canonical chunk-level metrics. Present when diff used chunk hashes
    /// instead of reconstruction-term identity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunk_metrics: Option<ChunkDiffMetrics>,
}

impl PartialEq for ChunkDiffReport {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path
            && self.status == other.status
            && self.old_size == other.old_size
            && self.new_size == other.new_size
            && self.unchanged_segments == other.unchanged_segments
            && self.unchanged_bytes == other.unchanged_bytes
            && self.removed_segments == other.removed_segments
            && self.removed_bytes == other.removed_bytes
            && self.added_segments == other.added_segments
            && self.added_bytes == other.added_bytes
            && self.delta_bytes == other.delta_bytes
            && self.dedup_ratio.to_bits() == other.dedup_ratio.to_bits()
            && self.changed_byte_ranges == other.changed_byte_ranges
            && self.segment_details == other.segment_details
            && self.annotations == other.annotations
            && self.chunk_metrics == other.chunk_metrics
    }
}

impl Eq for ChunkDiffReport {}

/// Chunk-level metrics for a file diff.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChunkDiffMetrics {
    pub old_source: ChunkSequenceSourceKind,
    pub new_source: ChunkSequenceSourceKind,
    pub old_chunks: u32,
    pub new_chunks: u32,
    pub unchanged_chunks: u32,
    pub removed_chunks: u32,
    pub added_chunks: u32,
    pub old_bytes: u64,
    pub new_bytes: u64,
    pub unchanged_bytes: u64,
    pub removed_bytes: u64,
    pub added_bytes: u64,
    pub signed_delta_bytes: i64,
    pub reuse_ratio: f64,
    pub changed_byte_ranges_old: Vec<(u64, u64)>,
    pub changed_byte_ranges_new: Vec<(u64, u64)>,
}

/// Aggregate summary across all files in a diff.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffSummary {
    pub files_changed: u32,
    pub total_segments_changed: u32,
    pub total_delta_bytes: u64,
}

/// A single entry in the diff output, covering both crab-tracked
/// and git-native files.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileDiffEntry {
    pub report: ChunkDiffReport,
}

impl PartialEq for FileDiffEntry {
    fn eq(&self, other: &Self) -> bool {
        self.report == other.report
    }
}

impl Eq for FileDiffEntry {}

/// Output rendering mode for the diff formatter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    Human,
    HumanVerbose,
    Json,
    Stat,
    NameOnly,
}
