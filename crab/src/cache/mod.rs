//! Local disk cache with hash-verified reads and LRU eviction.
//!
//! Layout per repo:
//! ```text
//! ~/.cache/crab/
//! ├── chunks/{h[:2]}/{h}
//! ├── shards/{h[:2]}/{h}
//! ├── xorbs/{h[:2]}/{h}
//! ├── xorb-index/index.db
//! ├── hints/shard-hints.sqlite
//! ├── manifests/{name}.json
//! ├── manifests/{name}.etag
//! └── stages/{h[:2]}/{h}
//! ```
//!
//! All crab commands share the same cache root so that shards
//! downloaded during push are immediately available for hydrate, diff,
//! and FUSE — and vice versa.

use std::path::{Path, PathBuf};

use crab_storage::BucketIdentity;
use crab_types::storage::StorageProviderKind;

pub mod chunks {
    pub use crab_vfs::chunk_cache::*;
}
pub(crate) mod add_remote_candidates;
pub(crate) mod add_validation;
pub mod hydrated_pointer;
pub mod shard_hints {
    pub use crab_cache::shard_hints::*;
}
pub mod xet_chunk_cache;

pub use crab_cache::ShardHintCache;
pub use crab_cache::{
    CacheKey, CacheStats, LocalCache, PruneObjectKind, PruneOptions, PruneStats, PrunedCacheObject,
    VerifyReport, default_cache_root,
};
pub use crab_vfs::ChunkCache;
pub use hydrated_pointer::{HydratedEntry, HydratedPointerCache};
pub use xet_chunk_cache::{
    XetChunkCacheHandle, XetChunkCachePruneStats, XetChunkCacheStats, XetChunkCacheVerifyStats,
    prune_xet_chunk_cache, prune_xet_chunk_cache_with_cancel, verify_xet_chunk_cache,
    verify_xet_chunk_cache_with_cancel, xet_chunk_cache_from_config, xet_chunk_cache_stats,
    xet_chunk_cache_stats_with_cancel,
};

/// Return the bucket-global persistent chunk-index cache path.
pub(crate) fn chunk_index_cache_path(cache_root: &Path, identity: &BucketIdentity) -> PathBuf {
    let provider = match identity.cloud {
        StorageProviderKind::S3 => b"s3".as_slice(),
        StorageProviderKind::Gcs => b"gcs".as_slice(),
        StorageProviderKind::Azure => b"azure".as_slice(),
        StorageProviderKind::Local => b"local".as_slice(),
    };
    let mut hasher = blake3::Hasher::new();
    hasher.update(provider);
    hasher.update(b"\0");
    hasher.update(identity.host.as_bytes());
    hasher.update(b"\0");
    hasher.update(identity.container.as_bytes());
    let hex = hasher.finalize().to_hex();
    cache_root
        .join("buckets")
        .join(&hex[..16])
        .join("chunk-index.sqlite")
}

/// Return the scoped persistent add-time remote-candidate cache path.
pub(crate) fn add_remote_candidate_cache_path(
    cache_root: &Path,
    identity: &BucketIdentity,
    global_prefix: &str,
) -> PathBuf {
    let chunk_index_path = chunk_index_cache_path(cache_root, identity);
    let Some(parent) = chunk_index_path.parent() else {
        return cache_root.join("remote-candidates.sqlite");
    };
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab-add-remote-candidates-v1\0");
    hasher.update(global_prefix.as_bytes());
    let hex = hasher.finalize().to_hex();
    parent.join(format!("remote-candidates-{}.sqlite", &hex[..16]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_index_cache_is_shared_within_bucket_and_isolated_across_buckets() {
        let root = Path::new("cache");
        let first = BucketIdentity::new(StorageProviderKind::S3, "bucket-a", "bucket-a");
        let same = BucketIdentity::new(StorageProviderKind::S3, "BUCKET-A/", "bucket-a");
        let other = BucketIdentity::new(StorageProviderKind::S3, "bucket-b", "bucket-b");

        assert_eq!(
            chunk_index_cache_path(root, &first),
            chunk_index_cache_path(root, &same)
        );
        assert_ne!(
            chunk_index_cache_path(root, &first),
            chunk_index_cache_path(root, &other)
        );
        assert!(chunk_index_cache_path(root, &first).ends_with("chunk-index.sqlite"));
    }

    #[test]
    fn add_remote_candidate_cache_isolated_by_global_prefix() {
        let root = Path::new("cache");
        let bucket = BucketIdentity::new(StorageProviderKind::S3, "bucket-a", "bucket-a");
        let first = add_remote_candidate_cache_path(root, &bucket, "repo-a/.crab");
        let same = add_remote_candidate_cache_path(root, &bucket, "repo-a/.crab");
        let other = add_remote_candidate_cache_path(root, &bucket, "repo-b/.crab");
        assert_eq!(first, same);
        assert_ne!(first, other);
        assert_eq!(
            first.extension().and_then(|extension| extension.to_str()),
            Some("sqlite")
        );
    }
}
