// Contains adapted Celld lib.rs types at the revision in UPSTREAM.md.
// Apache-2.0; modified by Crab contributors. See LICENSE.

use std::path::{Path, PathBuf};

use crate::{CrabError, Result};

/// An LTX position in a checksum-linked lineage, not a Git revision or owner epoch.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(feature = "replica", derive(serde::Serialize, serde::Deserialize))]
pub struct Position {
    pub txid: u64,
    pub checksum: u64,
}

/// Admission bounds for local capture and recovery, not an RSS quota.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_database_bytes: u64,
    pub max_capture_bytes: u64,
    pub max_file_bytes: u64,
    pub max_plan_bytes: u64,
    pub max_segments: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_database_bytes: 512 << 20,
            max_capture_bytes: 64 << 20,
            max_file_bytes: 512 << 20,
            max_plan_bytes: 1 << 30,
            max_segments: 1024,
        }
    }
}

impl Limits {
    pub(crate) fn validate(self) -> Result<Self> {
        if self.max_database_bytes < 512
            || self.max_capture_bytes < 128
            || self.max_file_bytes < 128
            || self.max_segments == 0
            || self.max_capture_bytes > self.max_file_bytes
            || self.max_file_bytes > self.max_plan_bytes
            || self.max_plan_bytes > (usize::MAX / 8) as u64
            || self.max_database_bytes > (usize::MAX / 8) as u64
        {
            return Err(CrabError::InvalidState("invalid resource limits"));
        }
        Ok(self)
    }
}

/// Immutable-file expectations to record in the server's authoritative manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "replica", derive(serde::Serialize, serde::Deserialize))]
pub struct SegmentInfo {
    pub min_txid: u64,
    pub max_txid: u64,
    pub page_size: u32,
    pub database_pages: u32,
    pub pre_checksum: u64,
    pub post_checksum: u64,
    pub size_bytes: u64,
    pub blake3: [u8; 32],
}

impl SegmentInfo {
    #[must_use]
    pub fn position(&self) -> Position {
        Position {
            txid: self.max_txid,
            checksum: self.post_checksum,
        }
    }

    #[cfg_attr(not(feature = "replica"), expect(dead_code))]
    pub(crate) fn from_decoded(bytes: &[u8], file: &crate::ltx::DecodedFile) -> Self {
        Self::from_inspected(file, bytes.len() as u64, *blake3::hash(bytes).as_bytes())
    }

    pub(crate) fn from_inspected(
        file: &crate::ltx::DecodedFile,
        size_bytes: u64,
        blake3: [u8; 32],
    ) -> Self {
        Self {
            min_txid: file.header.min_txid.0,
            max_txid: file.header.max_txid.0,
            page_size: file.header.page_size,
            database_pages: file.header.commit,
            pre_checksum: file.header.pre_apply_checksum,
            post_checksum: file.trailer.post_apply_checksum,
            size_bytes,
            blake3,
        }
    }
}

/// A caller-selected local file plus manifest expectations; not yet verified.
#[derive(Clone)]
pub struct LocalSegment {
    path: PathBuf,
    info: SegmentInfo,
    #[cfg(feature = "replica")]
    captured_index: Option<std::sync::Arc<[u8]>>,
}

impl std::fmt::Debug for LocalSegment {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LocalSegment")
            .field("path", &self.path)
            .field("info", &self.info)
            .finish()
    }
}

impl LocalSegment {
    #[must_use]
    pub fn new(path: PathBuf, info: SegmentInfo) -> Self {
        Self {
            path,
            info,
            #[cfg(feature = "replica")]
            captured_index: None,
        }
    }
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
    #[must_use]
    pub fn info(&self) -> &SegmentInfo {
        &self.info
    }

    #[cfg(feature = "replica")]
    pub(crate) fn with_captured_index(mut self, index: Vec<u8>) -> Self {
        self.captured_index = Some(index.into());
        self
    }

    #[cfg(feature = "replica")]
    pub(crate) fn captured_index(&self) -> Option<&[u8]> {
        self.captured_index.as_deref()
    }
}

/// All cuts produced by one capture, including checkpoint-boundary cuts.
#[derive(Debug, Clone)]
pub struct CaptureBatch {
    pub segments: Vec<LocalSegment>,
    pub position: Position,
    pub timing: CaptureTiming,
}

/// Bounded, in-memory observations for one capture operation.
///
/// The ledger is not persisted and never participates in capture, checkpoint,
/// or fencing decisions. Durations are nanoseconds from the host's monotonic
/// clock; byte fields distinguish logical work, physical reads, and allocation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CaptureTiming {
    /// Total elapsed time for the capture phase represented by this batch.
    pub total_nanos: u64,
    /// Time spent preparing the managed capture before WAL synchronization.
    pub preparation_nanos: u64,
    /// Time spent validating the managed control-table schema.
    pub schema_check_nanos: u64,
    /// Time spent confirming that the WAL contains a readable frame.
    pub wal_existence_nanos: u64,
    /// Time spent resolving the current checksum-linked WAL position.
    pub position_resolution_nanos: u64,
    /// Time spent reading and parsing the WAL image.
    pub wal_read_nanos: u64,
    /// Time spent collecting the committed page map from the WAL image.
    pub page_collection_nanos: u64,
    /// Time spent validating WAL or produced LTX data.
    pub verification_nanos: u64,
    /// Time spent encoding LTX bytes and page records.
    pub encode_nanos: u64,
    /// Time spent writing LTX and temporary index bytes locally.
    pub local_write_nanos: u64,
    /// Time spent syncing completed LTX file contents.
    pub fsync_nanos: u64,
    /// Time spent publishing the LTX name and, for immediate captures, syncing
    /// its parent directory.
    pub parent_sync_nanos: u64,
    /// Time spent in checkpoint maintenance associated with the capture.
    pub checkpoint_nanos: u64,
    /// Logical WAL bytes consumed by the capture.
    pub wal_bytes: u64,
    /// Largest physical WAL file length observed by the capture.
    pub wal_file_bytes: u64,
    /// Physical WAL bytes transferred into capture memory.
    pub wal_read_bytes: u64,
    /// Database bytes represented by the captured commit.
    pub database_bytes: u64,
    /// LTX bytes inspected for the returned segments.
    pub ltx_bytes: u64,
    /// Number of segments inspected for the returned batch.
    pub segment_count: u32,
    /// Number of sparse WAL image reads selected.
    pub wal_sparse_reads: u32,
    /// Number of complete WAL image reads selected.
    pub wal_full_reads: u32,
    /// Number of complete images selected before incremental WAL parsing.
    pub wal_snapshot_reads: u32,
    /// Number of sparse WAL reads that required a complete-image retry.
    pub wal_fallback_reads: u32,
    /// Peak allocated bytes in one WAL image used by this capture.
    pub wal_image_bytes: u64,
    /// Number of SQLite checkpoint pragmas executed.
    pub checkpoint_runs: u32,
    /// Number of checkpoint pragmas that reported a busy reader or writer.
    pub checkpoint_busy: u32,
    /// Number of checkpoint pragmas that failed with SQLITE_BUSY or SQLITE_LOCKED.
    pub checkpoint_busy_errors: u32,
    /// WAL frames reported by completed checkpoint pragmas.
    pub checkpoint_frames: u64,
    /// WAL frames backfilled by completed checkpoint pragmas.
    pub checkpoint_backfilled: u64,
    /// Number of checkpoints that restarted the WAL lineage.
    pub checkpoint_restarts: u32,
}

impl CaptureTiming {
    pub(crate) fn merge(&mut self, other: Self) {
        self.total_nanos = self.total_nanos.saturating_add(other.total_nanos);
        self.preparation_nanos = self
            .preparation_nanos
            .saturating_add(other.preparation_nanos);
        self.schema_check_nanos = self
            .schema_check_nanos
            .saturating_add(other.schema_check_nanos);
        self.wal_existence_nanos = self
            .wal_existence_nanos
            .saturating_add(other.wal_existence_nanos);
        self.position_resolution_nanos = self
            .position_resolution_nanos
            .saturating_add(other.position_resolution_nanos);
        self.wal_read_nanos = self.wal_read_nanos.saturating_add(other.wal_read_nanos);
        self.page_collection_nanos = self
            .page_collection_nanos
            .saturating_add(other.page_collection_nanos);
        self.verification_nanos = self
            .verification_nanos
            .saturating_add(other.verification_nanos);
        self.encode_nanos = self.encode_nanos.saturating_add(other.encode_nanos);
        self.local_write_nanos = self
            .local_write_nanos
            .saturating_add(other.local_write_nanos);
        self.fsync_nanos = self.fsync_nanos.saturating_add(other.fsync_nanos);
        self.parent_sync_nanos = self
            .parent_sync_nanos
            .saturating_add(other.parent_sync_nanos);
        self.checkpoint_nanos = self.checkpoint_nanos.saturating_add(other.checkpoint_nanos);
        self.wal_bytes = self.wal_bytes.saturating_add(other.wal_bytes);
        self.wal_file_bytes = self.wal_file_bytes.max(other.wal_file_bytes);
        self.wal_read_bytes = self.wal_read_bytes.saturating_add(other.wal_read_bytes);
        self.database_bytes = self.database_bytes.saturating_add(other.database_bytes);
        self.ltx_bytes = self.ltx_bytes.saturating_add(other.ltx_bytes);
        self.segment_count = self.segment_count.saturating_add(other.segment_count);
        self.wal_sparse_reads = self.wal_sparse_reads.saturating_add(other.wal_sparse_reads);
        self.wal_full_reads = self.wal_full_reads.saturating_add(other.wal_full_reads);
        self.wal_snapshot_reads = self
            .wal_snapshot_reads
            .saturating_add(other.wal_snapshot_reads);
        self.wal_fallback_reads = self
            .wal_fallback_reads
            .saturating_add(other.wal_fallback_reads);
        self.wal_image_bytes = self.wal_image_bytes.max(other.wal_image_bytes);
        self.checkpoint_runs = self.checkpoint_runs.saturating_add(other.checkpoint_runs);
        self.checkpoint_busy = self.checkpoint_busy.saturating_add(other.checkpoint_busy);
        self.checkpoint_busy_errors = self
            .checkpoint_busy_errors
            .saturating_add(other.checkpoint_busy_errors);
        self.checkpoint_frames = self
            .checkpoint_frames
            .saturating_add(other.checkpoint_frames);
        self.checkpoint_backfilled = self
            .checkpoint_backfilled
            .saturating_add(other.checkpoint_backfilled);
        self.checkpoint_restarts = self
            .checkpoint_restarts
            .saturating_add(other.checkpoint_restarts);
    }
}

// Derived from Celld's position types; private to the imported codec/engine.
pub(crate) type Checksum = u64;
pub(crate) const CHECKSUM_FLAG: u64 = 1 << 63;
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub(crate) struct Txid(pub u64);
impl std::fmt::Display for Txid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Pos {
    pub txid: Txid,
    pub post_apply_checksum: u64,
}
impl Pos {
    pub const ZERO: Self = Self {
        txid: Txid(0),
        post_apply_checksum: 0,
    };
    pub fn new(txid: Txid, post_apply_checksum: u64) -> Self {
        Self {
            txid,
            post_apply_checksum,
        }
    }
}
impl From<Pos> for Position {
    fn from(pos: Pos) -> Self {
        Self {
            txid: pos.txid.0,
            checksum: pos.post_apply_checksum,
        }
    }
}
