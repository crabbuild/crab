//! Compact current Git object locations for object-store range reads.

mod format;
mod reader;
mod writer;

pub use reader::{GitObjectLocatorSession, GitObjectLookup};
pub use writer::{GitObjectLocatorWriter, LocatorSweepStats, LocatorWriteStats};

use crab_xet::hash::MerkleHash;

/// Exact location of one Git object inside an immutable pack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GitObjectLocation {
    /// Offset of the complete packed entry header.
    pub pack_offset: u64,
    /// Complete packed entry length.
    pub entry_len: u64,
    /// CRC32 over the complete packed entry.
    pub crc32: u32,
}

/// Exact location joined to its immutable pack identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GitObjectLocator {
    /// Crab content identity of the pack object.
    pub pack_id: MerkleHash,
    /// Location within the pack object.
    pub location: GitObjectLocation,
}

/// Git object ID paired with its pack-local location.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GitObjectLocatorEntry {
    /// SHA-1 Git object ID.
    pub oid: [u8; 20],
    /// Location within the pack object.
    pub location: GitObjectLocation,
}

/// Committed metadata for one immutable Git pack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GitPackLocatorRecord {
    /// Crab content identity of the pack object.
    pub pack_id: MerkleHash,
    /// Manifest generation that committed the pack.
    pub committed_generation: u64,
    /// Pack inventory hash that committed the pack.
    pub pack_index_hash: MerkleHash,
    /// Number of objects reported by the verified index.
    pub object_count: u64,
    /// Pack body size including header and trailer.
    pub pack_size: u64,
}

/// Durable numeric slot permanently bound to one immutable pack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GitPackLocatorBinding {
    /// Non-zero monotonically allocated pack slot.
    pub pack_slot: u64,
    /// Pack identity and manifest evidence bound to the slot.
    pub record: GitPackLocatorRecord,
}

/// Canonical pack facts pinned by one manifest inventory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GitPackInventoryEntry {
    /// Crab content identity of the pack object.
    pub pack_id: MerkleHash,
    /// Number of objects reported by the canonical pack index.
    pub object_count: u64,
    /// Pack body size including header and trailer.
    pub pack_size: u64,
}

/// Latest immutable pack inventory fully covered by the locator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GitLocatorCoverage {
    /// Covered manifest generation.
    pub generation: u64,
    /// Covered manifest pack-inventory hash.
    pub pack_index_hash: MerkleHash,
}

/// Object-store prefix of the sole Git locator database.
#[must_use]
pub fn git_object_locator_path(repo_prefix: &str) -> String {
    format!("{}/git_locator_db/", repo_prefix.trim_end_matches('/'))
}
