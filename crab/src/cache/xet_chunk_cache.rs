//! CLI Adapter for the shared xet-core xorb-range chunk cache.

use crate::core::config::Config;
use crate::core::error::Result;

pub use crab_cache::{
    XetChunkCacheHandle, XetChunkCachePruneStats, XetChunkCacheStats, XetChunkCacheVerifyStats,
    prune_xet_chunk_cache, prune_xet_chunk_cache_with_cancel, verify_xet_chunk_cache,
    verify_xet_chunk_cache_with_cancel, xet_chunk_cache_stats, xet_chunk_cache_stats_with_cancel,
};

/// Opens the shared xet-core chunk cache from CLI configuration.
///
/// Resolve tagged placement and caller limits above the shared cache owner.
pub fn xet_chunk_cache_from_config(config: &Config) -> Result<XetChunkCacheHandle> {
    let directory = config.effective_chunk_cache_dir();
    Ok(XetChunkCacheHandle::open(
        directory,
        catalog_from_config(config),
    )?)
}

pub(crate) fn catalog_from_config(config: &Config) -> crab_cache::CacheCatalog {
    let directory = config.effective_chunk_cache_dir();
    let root = directory.parent().unwrap_or(&directory).to_owned();
    crab_cache::CacheCatalog::new(root, config.effective_chunk_cache_max_bytes())
}
