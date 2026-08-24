//! Generation-bound Git object catalog for object-store range reads.

mod format;
mod reader;
mod writer;

pub use reader::{GitObjectLocatorSession, GitObjectLookup};
pub use writer::{GitObjectLocatorWriter, LocatorSweepStats, LocatorWriteStats};

const READER_CHECKPOINT_PREFIX: &str = "crab-git-catalog-";
const UNPUBLISHED_CHECKPOINT_NAME: &str = "crab-git-unpublished";

use crab_xet::hash::MerkleHash;

fn catalog_checkpoint_name(digest: MerkleHash) -> String {
    format!("{READER_CHECKPOINT_PREFIX}{}", digest.hex())
}

/// Dense, stable position assigned to one Git object in the catalog.
pub type GitObjectOrdinal = u32;

/// Canonical Git object kind when publication has already proven it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitObjectKind {
    /// Commit object.
    Commit,
    /// Tree object.
    Tree,
    /// Blob object.
    Blob,
    /// Annotated tag object.
    Tag,
}

/// Optional logical facts known independently of the packed-entry location.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GitObjectMetadata {
    /// Fully resolved object kind, when proven.
    pub kind: Option<GitObjectKind>,
    /// Fully resolved uncompressed object bytes, when proven.
    pub logical_size: Option<u64>,
    /// Proven delta base object ID for a delta entry, when known.
    pub delta_base_oid: Option<[u8; 20]>,
}

/// Generation identity of one immutable published catalog checkpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GitObjectCatalogIdentity {
    /// Manifest generation whose pack inventory is covered.
    pub generation: u64,
    /// Covered manifest pack-inventory hash.
    pub pack_index_hash: MerkleHash,
    /// Number of allocated object ordinals.
    pub object_count: u64,
    /// Digest naming the exact immutable catalog checkpoint.
    pub catalog_digest: MerkleHash,
}

/// Read-only size and layer facts for catalog maintenance planning.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GitObjectCatalogStats {
    /// Number of objects in the latest published catalog.
    pub object_count: u64,
    /// Active read layers: level-zero tables plus compacted sorted runs.
    pub active_layers: u64,
    /// Physical SSTs referenced by the active manifest.
    pub active_ssts: u64,
    /// Estimated bytes referenced by the active manifest.
    pub active_bytes: u64,
    /// Immutable catalog checkpoints retained by SlateDB.
    pub checkpoints: u64,
}

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
    /// Stable catalog position for this object.
    pub ordinal: GitObjectOrdinal,
    /// Crab content identity of the pack object.
    pub pack_id: MerkleHash,
    /// Location within the pack object.
    pub location: GitObjectLocation,
    /// Optional proven logical and delta-base facts.
    pub metadata: GitObjectMetadata,
}

/// Git object ID paired with its pack-local location.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GitObjectLocatorEntry {
    /// SHA-1 Git object ID.
    pub oid: [u8; 20],
    /// Location within the pack object.
    pub location: GitObjectLocation,
    /// Optional proven logical and delta-base facts.
    pub metadata: GitObjectMetadata,
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
    format!(
        "{}/git_object_catalog_db/",
        repo_prefix.trim_end_matches('/')
    )
}
