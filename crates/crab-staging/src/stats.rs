//! Staging statistics and metrics wiring.

use serde::Serialize;

/// Point-in-time snapshot of staging area statistics.
#[derive(Debug, Clone, Default, PartialEq, Serialize, schemars::JsonSchema)]
pub struct StagingStats {
    /// Number of sealed segments.
    pub segments_sealed: u64,
    /// Bytes written to the current (unsealed) segment.
    pub current_segment_bytes: u64,
    /// Total bytes across all segments (sealed + current).
    pub total_staged_bytes: u64,
    /// Bytes occupied by live (referenced) chunks.
    pub live_bytes: u64,
    /// Bytes occupied by dead (unreferenced) chunks.
    pub dead_bytes: u64,
    /// Ratio of dead bytes to total staged bytes.
    pub dead_ratio: f64,
    /// Total number of staged chunks.
    pub chunk_count: u64,
    /// Total number of registered files.
    pub file_count: u64,
}

/// Recipe/lease migration and push-snapshot health for diagnostics.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StagingLifecycleHealth {
    pub layout_version: String,
    pub quarantined_entries: u64,
    pub open_push_snapshots: u64,
    pub committed_push_snapshots: u64,
    pub recipes: u64,
    pub path_leases: u64,
    pub payloads: u64,
}

/// Statistics returned by [`super::StagingArea::compact`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CompactionStats {
    /// Number of segments rewritten.
    pub segments_compacted: u64,
    /// Total bytes freed from disk.
    pub bytes_reclaimed: u64,
    /// Total chunks moved to new segments.
    pub chunks_moved: u64,
}

/// Statistics returned by [`super::StagingArea::clean`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StagingCleanStats {
    /// Number of whole segment files removed (zero live chunks).
    pub segments_removed: u64,
    /// Number of segments rewritten by compaction.
    pub segments_compacted: u64,
    /// Total bytes freed from disk.
    pub bytes_reclaimed: u64,
    /// Total dead chunks removed.
    pub chunks_reclaimed: u64,
    /// Number of stale `push-{uuid}.inflight` markers removed.
    pub stale_markers_removed: u64,
}

/// Statistics returned by [`super::StagingAreaReadOnly::verify`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct StagingVerifyStats {
    /// Number of registered files checked.
    pub files_checked: u64,
    /// Number of file chunk references checked.
    pub chunk_refs_checked: u64,
    /// Number of unique chunk payloads read and verified.
    pub unique_chunks_checked: u64,
    /// Number of unique staged payload bytes verified.
    pub bytes_checked: u64,
}

/// Statistics returned by [`super::StagingAreaReadOnly::retire_file`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RetireStats {
    /// Number of `chunks` rows deleted from the staging index.
    pub rows_deleted: u64,
    /// Segments whose `live_chunk_count` was decremented by this call.
    ///
    /// A segment whose count reaches zero and is sealed becomes a
    /// sweep candidate on the next `sweep_orphans` pass. Reclamation
    /// of the segment file itself is deferred to that sweep.
    pub segments_touched: Vec<u64>,
}
