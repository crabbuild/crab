//! Bounded ordered file-chunk metadata shared by read clients.

use serde::{Deserialize, Serialize};

/// A bounded metadata page; repeated hashes remain separate occurrences.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChunkPage {
    pub chunk_count: u64,
    pub xorb_count: Option<u64>,
    pub chunks: Vec<Chunk>,
    pub next: Option<u64>,
}

/// One ordered occurrence; placement is absent for local recipe inspection.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Chunk {
    /// Canonical Xet hexadecimal chunk identity.
    pub hash: String,
    pub offset: u64,
    pub length: u64,
    pub xorb_hash: Option<String>,
    pub xorb_chunk_index: Option<u32>,
}
