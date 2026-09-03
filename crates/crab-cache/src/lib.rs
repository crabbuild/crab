//! Local and remote cache contracts for Crab.

#[cfg(feature = "active-probe")]
pub mod active_probe;
#[cfg(feature = "remote-client")]
pub mod cache_client;
#[cfg(any(feature = "local-cache", feature = "xet-chunk-cache"))]
pub mod catalog;
#[cfg(any(feature = "local-cache", feature = "xet-chunk-cache"))]
mod clean;
pub mod error;
#[cfg(any(feature = "local-cache", feature = "xet-chunk-cache"))]
pub mod health;
pub mod key;
#[cfg(feature = "local-cache")]
pub mod lifecycle;
#[cfg(feature = "local-cache")]
pub mod local_cache;
pub mod path_class;
#[cfg(any(feature = "local-cache", feature = "xet-chunk-cache"))]
mod private_fs;
pub mod root;
pub mod service;
#[cfg(feature = "local-cache")]
pub mod shard_hints;
#[cfg(feature = "xet-chunk-cache")]
pub mod xet_chunk_cache;

#[cfg(feature = "active-probe")]
pub use active_probe::{
    ActiveProbeAuth, ActiveProbeObject, ActiveProbeOutcome, build_active_probe, run_active_probe,
};
#[cfg(feature = "remote-client")]
pub use cache_client::{CacheClient, CacheObjectStream, build_cache_service_http_client};
#[cfg(any(feature = "local-cache", feature = "xet-chunk-cache"))]
pub use catalog::{CacheCatalog, CacheCatalogStats, CacheMaintenanceStats};
#[cfg(any(feature = "local-cache", feature = "xet-chunk-cache"))]
pub use clean::{CacheCleanReport, clean_cache};
pub use error::{CacheError, Result};
pub use key::CacheKey;
#[cfg(feature = "local-cache")]
pub use local_cache::{
    CacheStats, LocalCache, MAX_CACHE_CHUNK_BYTES, MAX_CACHE_MANIFEST_BYTES, MAX_CACHE_SHARD_BYTES,
    MAX_CACHE_STAGE_BYTES, PruneObjectKind, PruneOptions, PruneStats, PrunedCacheObject,
    VerifyReport,
};
pub use path_class::cache_key_for_path;
pub use root::default_cache_root;
#[cfg(any(feature = "local-cache", feature = "xet-chunk-cache"))]
pub use root::{ensure_private_cache_directory, private_cache_directory_is_safe};
pub use service::{
    CacheObjectHead, CacheObjectRange, CacheServiceAuth, CacheServiceCapabilities,
    CacheServiceLimits, CacheServiceMode, DedupQueryResult, KnownChunk,
};
#[cfg(feature = "local-cache")]
pub use shard_hints::{
    SHARD_HINTS_DATABASE, ShardHintCache, ShardHintScope, database_path as shard_hints_path,
};
#[cfg(feature = "xet-chunk-cache")]
pub use xet_chunk_cache::{
    XetChunkCacheHandle, XetChunkCachePruneStats, XetChunkCacheStats, XetChunkCacheVerifyStats,
    prune_xet_chunk_cache, prune_xet_chunk_cache_with_cancel, verify_xet_chunk_cache,
    verify_xet_chunk_cache_with_cancel, xet_chunk_cache_stats, xet_chunk_cache_stats_with_cancel,
};
