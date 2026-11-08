use std::sync::Arc;

use slatedb::db_cache::{
    CacheLoader, CachedEntry, CachedKey, DEFAULT_BLOCK_CACHE_CAPACITY, DEFAULT_META_CACHE_CAPACITY,
    DbCache, SplitCache,
};

/// Create a byte-bounded SlateDB memory cache that coalesces concurrent loads.
#[must_use]
pub fn slatedb_cache(capacity: u64) -> Arc<dyn DbCache> {
    Arc::new(MemoryCache(
        moka::future::Cache::builder()
            .weigher(|_, entry: &CachedEntry| u32::try_from(entry.size()).unwrap_or(u32::MAX))
            .max_capacity(capacity)
            .build(),
    ))
}

/// Create coalescing caches with SlateDB's separate block and metadata capacities.
#[must_use]
pub fn default_slatedb_cache() -> Arc<dyn DbCache> {
    Arc::new(
        SplitCache::new()
            .with_block_cache(Some(slatedb_cache(DEFAULT_BLOCK_CACHE_CAPACITY)))
            .with_meta_cache(Some(slatedb_cache(DEFAULT_META_CACHE_CAPACITY)))
            .build(),
    )
}

struct MemoryCache(moka::future::Cache<CachedKey, CachedEntry>);

impl MemoryCache {
    async fn fetch(
        &self,
        key: CachedKey,
        loader: CacheLoader,
    ) -> Result<CachedEntry, slatedb::Error> {
        // SlateDB's built-in Moka adapter does not merge concurrent misses.
        // Keep the previous cache's one-origin-read contract and preserve the
        // source chain when a shared fill fails for multiple waiters.
        self.0
            .try_get_with(key, async move { loader().await })
            .await
            .map_err(|source| {
                slatedb::Error::unavailable("metadata cache fill failed".to_owned())
                    .with_source(Box::new(source))
            })
    }
}

#[async_trait::async_trait]
impl DbCache for MemoryCache {
    async fn get_block(&self, key: &CachedKey) -> Result<Option<CachedEntry>, slatedb::Error> {
        Ok(self.0.get(key).await)
    }

    async fn get_index(&self, key: &CachedKey) -> Result<Option<CachedEntry>, slatedb::Error> {
        Ok(self.0.get(key).await)
    }

    async fn get_filter(&self, key: &CachedKey) -> Result<Option<CachedEntry>, slatedb::Error> {
        Ok(self.0.get(key).await)
    }

    async fn get_stats(&self, key: &CachedKey) -> Result<Option<CachedEntry>, slatedb::Error> {
        Ok(self.0.get(key).await)
    }

    async fn insert(&self, key: CachedKey, value: CachedEntry) {
        self.0.insert(key, value).await;
    }

    async fn remove(&self, key: &CachedKey) {
        self.0.invalidate(key).await;
    }

    fn entry_count(&self) -> u64 {
        self.0.entry_count()
    }

    async fn fetch_block(
        &self,
        key: CachedKey,
        loader: CacheLoader,
    ) -> Result<CachedEntry, slatedb::Error> {
        self.fetch(key, loader).await
    }

    async fn fetch_index(
        &self,
        key: CachedKey,
        loader: CacheLoader,
    ) -> Result<CachedEntry, slatedb::Error> {
        self.fetch(key, loader).await
    }

    async fn fetch_filter(
        &self,
        key: CachedKey,
        loader: CacheLoader,
    ) -> Result<CachedEntry, slatedb::Error> {
        self.fetch(key, loader).await
    }

    async fn fetch_stats(
        &self,
        key: CachedKey,
        loader: CacheLoader,
    ) -> Result<CachedEntry, slatedb::Error> {
        self.fetch(key, loader).await
    }
}
