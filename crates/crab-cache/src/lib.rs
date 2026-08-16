//! Local and remote cache contracts for Crab.

#[cfg(feature = "active-probe")]
pub mod active_probe;
#[cfg(feature = "remote-client")]
pub mod cache_client;
pub mod error;
pub mod key;
#[cfg(feature = "local-cache")]
pub mod local_cache;
pub mod path_class;
pub mod prefetch_profile;
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
pub use cache_client::{CacheClient, build_cache_service_http_client};
pub use error::{CacheError, Result};
pub use key::CacheKey;
#[cfg(feature = "local-cache")]
pub use local_cache::{
    CacheStats, CachedXorbCandidate, LocalCache, PruneObjectKind, PruneOptions, PruneStats,
    PrunedCacheObject, VerifyReport,
};
pub use path_class::cache_key_for_path;
pub use prefetch_profile::{
    PREFETCH_TOML_FILE, PrefetchConfig, load_prefetch_from_crab_dir, load_prefetch_path,
    parse_prefetch,
};
pub use root::default_cache_root;
pub use service::{
    CacheObjectHead, CacheObjectRange, CacheServiceAuth, CacheServiceCapabilities,
    CacheServiceLimits, CacheServiceMode, DedupQueryResult, KnownChunk,
};
#[cfg(feature = "local-cache")]
pub use shard_hints::{SHARD_HINTS_FILENAME, ShardHintCache, default_path as shard_hints_path};
#[cfg(feature = "xet-chunk-cache")]
pub use xet_chunk_cache::{XetChunkCacheHandle, XetChunkCacheStats};
