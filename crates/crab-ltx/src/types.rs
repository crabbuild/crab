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
            max_database_bytes: 256 << 20,
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
#[derive(Debug, Clone)]
pub struct LocalSegment {
    path: PathBuf,
    info: SegmentInfo,
}

impl LocalSegment {
    #[must_use]
    pub fn new(path: PathBuf, info: SegmentInfo) -> Self {
        Self { path, info }
    }
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
    #[must_use]
    pub fn info(&self) -> &SegmentInfo {
        &self.info
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
/// clock; byte fields describe the logical work observed by the capture.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CaptureTiming {
    /// Total elapsed time for the capture phase represented by this batch.
    pub total_nanos: u64,
    /// Time spent preparing the managed capture before WAL synchronization.
    pub preparation_nanos: u64,
    /// Time spent reading and parsing the WAL image.
    pub wal_read_nanos: u64,
    /// Time spent validating WAL or produced LTX data.
    pub verification_nanos: u64,
    /// Time spent encoding LTX bytes and page records.
    pub encode_nanos: u64,
    /// Time spent making the LTX file durable and publishing its name.
    pub durable_write_nanos: u64,
    /// Time spent in checkpoint maintenance associated with the capture.
    pub checkpoint_nanos: u64,
    /// Logical WAL bytes consumed by the capture.
    pub wal_bytes: u64,
    /// Database bytes represented by the captured commit.
    pub database_bytes: u64,
    /// LTX bytes inspected for the returned segments.
    pub ltx_bytes: u64,
    /// Number of segments inspected for the returned batch.
    pub segment_count: u32,
}

impl CaptureTiming {
    pub(crate) fn merge(&mut self, other: Self) {
        self.total_nanos = self.total_nanos.saturating_add(other.total_nanos);
        self.preparation_nanos = self
            .preparation_nanos
            .saturating_add(other.preparation_nanos);
        self.wal_read_nanos = self.wal_read_nanos.saturating_add(other.wal_read_nanos);
        self.verification_nanos = self
            .verification_nanos
            .saturating_add(other.verification_nanos);
        self.encode_nanos = self.encode_nanos.saturating_add(other.encode_nanos);
        self.durable_write_nanos = self
            .durable_write_nanos
            .saturating_add(other.durable_write_nanos);
        self.checkpoint_nanos = self.checkpoint_nanos.saturating_add(other.checkpoint_nanos);
        self.wal_bytes = self.wal_bytes.saturating_add(other.wal_bytes);
        self.database_bytes = self.database_bytes.saturating_add(other.database_bytes);
        self.ltx_bytes = self.ltx_bytes.saturating_add(other.ltx_bytes);
        self.segment_count = self.segment_count.saturating_add(other.segment_count);
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
