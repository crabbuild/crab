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
        Self {
            min_txid: file.header.min_txid.0,
            max_txid: file.header.max_txid.0,
            page_size: file.header.page_size,
            database_pages: file.header.commit,
            pre_checksum: file.header.pre_apply_checksum,
            post_checksum: file.trailer.post_apply_checksum,
            size_bytes: bytes.len() as u64,
            blake3: *blake3::hash(bytes).as_bytes(),
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
