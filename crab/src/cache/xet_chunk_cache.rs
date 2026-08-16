//! CLI Adapter for the shared xet-core xorb-range chunk cache.

use crate::core::config::Config;
use crate::core::error::Result;

pub use crab_cache::{XetChunkCacheHandle, XetChunkCacheStats};

/// Opens the shared xet-core chunk cache from CLI configuration.
///
/// The reusable cache Module only needs a directory and byte budget. Resolving
/// those from Crab config belongs in the CLI crate.
pub fn xet_chunk_cache_from_config(config: &Config) -> Result<XetChunkCacheHandle> {
    Ok(XetChunkCacheHandle::open(
        config.effective_chunk_cache_dir(),
        config.chunk_cache_bytes,
    )?)
}
