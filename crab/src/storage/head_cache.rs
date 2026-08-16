//! Per-command HEAD cache for storage-class metadata.
//!
//! [`HeadCache`] deduplicates HEAD calls within a single command
//! invocation. When `crab hydrate` touches the same xorb via
//! multiple file pointers, the first reference pays for the HEAD
//! round-trip and subsequent references read from the cache.
//!
//! The cache is scoped to a single command run — it is not persisted
//! across invocations. Thread-safe via [`DashMap`].

use dashmap::DashMap;
use object_store::path::Path;

use crate::core::error::Result;
use crate::storage::Store;
use crate::storage::head_class::head_with_class;
use crate::tier::provider::HeadMeta;

/// In-memory cache of [`HeadMeta`] keyed by object path.
///
/// Designed for per-command lifetime: create one at the start of a
/// `hydrate` run, share it across concurrent tasks, and drop it when
/// the command completes.
pub struct HeadCache {
    cache: DashMap<String, HeadMeta>,
}

impl HeadCache {
    /// Create an empty cache.
    pub fn new() -> Self {
        Self {
            cache: DashMap::new(),
        }
    }

    /// Return cached [`HeadMeta`] for `path`, or fetch it via
    /// [`head_with_class`] and cache the result.
    ///
    /// Concurrent callers requesting the same path may both issue a
    /// HEAD — the second write is a harmless overwrite with an
    /// identical value. This avoids holding a lock across the async
    /// fetch boundary.
    pub async fn get_or_fetch(&self, store: &Store, path: &Path) -> Result<HeadMeta> {
        let key = path.to_string();

        if let Some(entry) = self.cache.get(&key) {
            return Ok(entry.clone());
        }

        let meta = head_with_class(store, path).await?;
        self.cache.insert(key, meta.clone());
        Ok(meta)
    }

    /// Number of cached entries. Useful for diagnostics.
    pub fn len(&self) -> usize {
        self.cache.len()
    }

    /// Whether the cache is empty.
    pub fn is_empty(&self) -> bool {
        self.cache.is_empty()
    }
}

impl Default for HeadCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]
mod tests {
    use super::*;
    use crate::tier::StorageClass;
    use object_store::ObjectStoreExt;

    #[tokio::test]
    async fn caches_head_result() {
        let mem = std::sync::Arc::new(object_store::memory::InMemory::new());
        let store = Store::new(mem.clone());
        let path = Path::from("test/cached-xorb");
        let data = bytes::Bytes::from_static(b"xorb-data");
        mem.put(&path, data.into()).await.unwrap();

        let cache = HeadCache::new();
        assert!(cache.is_empty());

        // First call fetches.
        let meta1 = cache.get_or_fetch(&store, &path).await.unwrap();
        assert_eq!(meta1.class, StorageClass::Unknown);
        assert_eq!(cache.len(), 1);

        // Second call returns cached value.
        let meta2 = cache.get_or_fetch(&store, &path).await.unwrap();
        assert_eq!(meta2.class, StorageClass::Unknown);
        assert_eq!(cache.len(), 1);
    }

    #[tokio::test]
    async fn different_paths_cached_separately() {
        let mem = std::sync::Arc::new(object_store::memory::InMemory::new());
        let store = Store::new(mem.clone());

        let path_a = Path::from("test/xorb-a");
        let path_b = Path::from("test/xorb-b");
        mem.put(&path_a, bytes::Bytes::from_static(b"a").into())
            .await
            .unwrap();
        mem.put(&path_b, bytes::Bytes::from_static(b"b").into())
            .await
            .unwrap();

        let cache = HeadCache::new();
        cache.get_or_fetch(&store, &path_a).await.unwrap();
        cache.get_or_fetch(&store, &path_b).await.unwrap();
        assert_eq!(cache.len(), 2);
    }

    #[tokio::test]
    async fn missing_object_not_cached() {
        let mem = std::sync::Arc::new(object_store::memory::InMemory::new());
        let store = Store::new(mem);
        let path = Path::from("does/not/exist");

        let cache = HeadCache::new();
        let result = cache.get_or_fetch(&store, &path).await;
        assert!(result.is_err());
        assert!(cache.is_empty());
    }
}
