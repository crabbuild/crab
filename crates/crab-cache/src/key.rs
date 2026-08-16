//! Cache key contracts shared by local cache users and cache-aware readers.

use crab_types::workflow::StageHash;
use crab_xet::xorb::format::MerkleHash;

/// Cache key identifying what kind of object is cached.
#[derive(Debug, Clone)]
pub enum CacheKey {
    /// A content-addressed chunk.
    Chunk(MerkleHash),
    /// A content-addressed shard.
    Shard(MerkleHash),
    /// A content-addressed xorb.
    Xorb(MerkleHash),
    /// A named manifest with an optional freshness token.
    Manifest { name: String, etag: Option<String> },
    /// A workflow stage cache entry.
    Stage(StageHash),
}
