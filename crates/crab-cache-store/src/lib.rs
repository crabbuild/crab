//! Client-side store wrapper that routes immutable reads through a local
//! disk cache and an optional feature-gated remote cache service, falling back
//! to the origin store on any cache error.
//!
//! The local cache (`LocalCache`) is always active - it provides
//! hash-verified, LRU-evicted caching of shards, xorbs, and manifests on
//! disk at `~/.cache/crab/`. The remote cache
//! service is compiled only with the `remote-client` feature and used when
//! `config.cache.service_url` is set and the service is healthy.
//!
//! All code paths (smudge, hydrate, FUSE, fetch, push) benefit from
//! caching without per-callsite changes because `CachingStore` mirrors
//! the [`Store`] interface.

#[cfg(feature = "remote-client")]
use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use async_trait::async_trait;
use bytes::Bytes;
use object_store::path::Path;
use object_store::{
    Attributes, CopyOptions, GetOptions, GetRange, GetResult, GetResultPayload, ListResult,
    MultipartUpload, ObjectMeta, ObjectStore, ObjectStoreExt, PutMultipartOptions, PutOptions,
    PutPayload, PutResult,
};

#[cfg(feature = "remote-client")]
use crab_cache::path_class::cache_route_contract_matches_current;
use crab_cache::path_class::{PathClass, classify_path};
#[cfg(feature = "remote-client")]
use crab_cache::{CacheClient, CacheServiceCapabilities};
use crab_cache::{
    CacheError, CacheKey, CacheObjectHead, CacheObjectRange, CacheServiceAuth, CacheServiceMode,
    DedupQueryResult, LocalCache, MAX_CACHE_CHUNK_BYTES, MAX_CACHE_SHARD_BYTES, cache_key_for_path,
    default_cache_root,
};
use crab_storage::{ETag, StorageError, Store};
use crab_xet::hash::MerkleHash;
use crab_xet::xorb::format::MAX_XORB_SIZE;

mod xorb_read;
use xorb_read::XorbReadState;

/// Result alias for cache/storage adapter operations.
pub type Result<T> = std::result::Result<T, CacheStoreError>;

#[cfg(feature = "remote-client")]
const DEDUP_QUERY_MAX_UNIQUE_HASHES: usize = 50_000;
const DEFAULT_LOCAL_CACHE_MAX_BYTES: u64 = 10 * 1024 * 1024 * 1024;

/// Errors raised by the read-through cache/storage adapter.
#[derive(Debug, thiserror::Error)]
pub enum CacheStoreError {
    /// Local or remote cache behavior failed.
    // Keep the domain error in the chain: transparent would skip its type,
    // losing terminal auth/integrity classification at reconstruction callers.
    #[error("{0}")]
    Cache(#[from] CacheError),
    /// Origin object-store behavior failed.
    #[error("{0}")]
    Storage(#[from] StorageError),
    /// Origin bytes were readable but failed data-plane integrity checks.
    #[error("origin object at {path} failed integrity verification: {source}")]
    OriginIntegrity {
        path: String,
        #[source]
        source: CacheError,
    },
}

/// Cache-service options needed by [`CachingStore`].
#[derive(Debug, Clone)]
pub struct CacheConfig {
    /// Product-wide byte budget for disposable local cache state.
    pub max_bytes: u64,
    /// Cache service URL.
    pub service_url: Option<String>,
    /// Service mode: cache, dedup, or both.
    pub service_mode: CacheServiceMode,
    /// Warm cache on push.
    pub push_warming: bool,
    /// Authentication mode for the cache service.
    pub service_auth: CacheServiceAuth,
    /// Path to a PEM CA bundle for connecting to cache services using private CAs.
    pub service_ca_cert: Option<PathBuf>,
    /// Path to the PEM client certificate chain for native mTLS.
    pub service_client_cert: Option<PathBuf>,
    /// Path to the PEM private key for native mTLS.
    pub service_client_key: Option<PathBuf>,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            max_bytes: DEFAULT_LOCAL_CACHE_MAX_BYTES,
            service_url: None,
            service_mode: CacheServiceMode::CacheAndDedup,
            push_warming: true,
            service_auth: CacheServiceAuth::None,
            service_ca_cert: None,
            service_client_cert: None,
            service_client_key: None,
        }
    }
}

impl From<&CacheConfig> for CacheConfig {
    fn from(config: &CacheConfig) -> Self {
        config.clone()
    }
}

/// Store wrapper that routes immutable reads through a local disk cache
/// and an optional feature-gated remote cache service.
///
/// The local cache is always active. The remote cache client is only
/// used when the `remote-client` feature is enabled, configured, and healthy.
/// On any cache error the wrapper falls back to the origin transparently.
///
/// Cheap to clone: `Store` is `Arc`-backed, `LocalCache` is `Arc`-wrapped,
/// and the feature-gated `CacheClient` is clone-cheap as well.
#[derive(Clone)]
pub struct CachingStore {
    origin: Store,
    /// Local disk cache - always present, always active.
    local_cache: Arc<LocalCache>,
    /// Optional remote cache service client.
    #[cfg(feature = "remote-client")]
    cache_client: Option<CacheClient>,
    #[cfg(feature = "remote-client")]
    mode: CacheServiceMode,
    push_warming: bool,
    xorb_reads: Arc<XorbReadState>,
    #[cfg(feature = "remote-client")]
    max_push_warming_object_bytes: Option<u64>,
}

impl CachingStore {
    /// Build a `CachingStore` from an origin store and cache config.
    ///
    /// The local disk cache is always constructed. When `service_url`
    /// is `None` the remote cache client is omitted - reads still
    /// benefit from the local cache.
    pub fn new<S, C>(origin: S, cache_config: C) -> Result<Self>
    where
        S: Into<Store>,
        C: Into<CacheConfig>,
    {
        let cache_config = cache_config.into();
        Self::new_with_local_cache(
            origin,
            cache_config.clone(),
            Arc::new(LocalCache::with_limits(
                default_cache_root(),
                cache_config.max_bytes,
                Some(cache_config.max_bytes),
            )),
        )
    }

    /// Build a `CachingStore` with an explicit local cache instance.
    ///
    /// This keeps callers that already own cache placement from relying on
    /// process-wide cache-root environment state.
    pub fn new_with_local_cache<S, C>(
        origin: S,
        cache_config: C,
        local_cache: Arc<LocalCache>,
    ) -> Result<Self>
    where
        S: Into<Store>,
        C: Into<CacheConfig>,
    {
        let origin = origin.into();
        let cache_config = cache_config.into();
        tracing::debug!(
            service_url = ?cache_config.service_url,
            mode = ?cache_config.service_mode,
            push_warming = cache_config.push_warming,
            "building CachingStore",
        );

        #[cfg(feature = "remote-client")]
        let cache_client = match &cache_config.service_url {
            Some(url) => Some(CacheClient::new(
                url,
                &cache_config.service_auth,
                cache_config.service_ca_cert.as_deref(),
                cache_config.service_client_cert.as_deref(),
                cache_config.service_client_key.as_deref(),
            )?),
            None => None,
        };

        #[cfg(not(feature = "remote-client"))]
        if cache_config.service_url.is_some() {
            return Err(CacheError::Service {
                reason: "cache service URL configured but crab-cache-store remote-client feature is disabled"
                    .to_string(),
            }
            .into());
        }

        Ok(Self {
            origin,
            local_cache,
            #[cfg(feature = "remote-client")]
            cache_client,
            #[cfg(feature = "remote-client")]
            mode: cache_config.service_mode,
            push_warming: cache_config.push_warming,
            xorb_reads: Arc::new(XorbReadState::new()),
            #[cfg(feature = "remote-client")]
            max_push_warming_object_bytes: None,
        })
    }

    /// Build a `CachingStore` with local disk cache always active and
    /// the remote cache service enabled only when configured and healthy.
    ///
    /// Always returns `Some` because the local cache is unconditional.
    /// When the remote service is down or not configured, the returned
    /// `CachingStore` still provides local disk caching for immutable
    /// objects (shards and xorbs).
    pub async fn try_build_healthy<S, C>(origin: S, cache_config: C) -> Option<Self>
    where
        S: Into<Store>,
        C: Into<CacheConfig>,
    {
        let cache_config = cache_config.into();
        let cs = match Self::new(origin, &cache_config) {
            Ok(cs) => cs,
            Err(e) => {
                tracing::debug!(error = %e, "failed to build CachingStore");
                return None;
            }
        };

        #[cfg(feature = "remote-client")]
        {
            let mut cs = cs;
            // Health-check the remote cache service. If unhealthy, disable
            // the remote client but keep the local cache active.
            if let Some(client) = &cs.cache_client {
                if client.is_healthy().await {
                    match client.capabilities().await {
                        Ok(capabilities) => {
                            if !cache_service_capabilities_route_contract_current(&capabilities) {
                                tracing::warn!(
                                    "cache service route contract missing or mismatched, using local cache only"
                                );
                                cs.cache_client = None;
                            } else {
                                let max_object_bytes = capabilities.limits.max_object_bytes;
                                cs.max_push_warming_object_bytes = Some(max_object_bytes);
                                tracing::info!(
                                    url = %cache_config.service_url.as_deref().unwrap_or(""),
                                    max_object_bytes,
                                    "cache service healthy, enabling cache-accelerated push"
                                );
                            }
                        }
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                "cache service capabilities unavailable, using local cache only"
                            );
                            cs.cache_client = None;
                        }
                    }
                } else {
                    tracing::info!("cache service not healthy, using local cache only");
                    cs.cache_client = None;
                }
            }
            Some(cs)
        }

        #[cfg(not(feature = "remote-client"))]
        {
            Some(cs)
        }
    }

    /// Borrow the underlying origin store.
    pub fn origin(&self) -> &Store {
        &self.origin
    }

    /// Borrow the local disk cache.
    pub fn local_cache(&self) -> &Arc<LocalCache> {
        &self.local_cache
    }

    /// Whether a cache service is configured (regardless of mode).
    pub fn has_cache_service(&self) -> bool {
        #[cfg(feature = "remote-client")]
        {
            self.cache_client.is_some()
        }
        #[cfg(not(feature = "remote-client"))]
        {
            false
        }
    }

    /// Expose this cache-aware store as an [`ObjectStore`] for read-only
    /// dependencies such as SlateDB.
    pub fn object_store(&self) -> Arc<dyn ObjectStore> {
        Arc::new(CacheAwareObjectStore {
            store: self.clone(),
        })
    }

    /// Build a cache-aware storage facade for read-only metadata consumers.
    ///
    /// The returned facade preserves the origin store's bucket identity and
    /// storage scope so scoped readers keep their no-checkpoint-write mode
    /// while immutable metadata reads still use the cache service.
    pub fn cache_aware_storage(&self) -> Store {
        let mut storage =
            Store::new(self.object_store()).with_bucket_identity(self.origin.bucket_identity());
        if let Some(scope) = self.origin.storage_scope().cloned() {
            storage = storage.with_storage_scope(scope);
        }
        storage
    }

    /// Whether the cache leg is active for reads.
    fn cache_reads_enabled(&self) -> bool {
        #[cfg(feature = "remote-client")]
        {
            self.cache_client.is_some() && self.mode.cache_reads_enabled()
        }
        #[cfg(not(feature = "remote-client"))]
        {
            false
        }
    }

    #[cfg(feature = "remote-client")]
    fn dedup_enabled(&self) -> bool {
        self.cache_client.is_some() && self.mode.dedup_enabled()
    }

    /// Read an object, returning its body and CAS token.
    ///
    /// For immutable paths the lookup order is:
    /// 1. Local disk cache (hash-verified)
    /// 2. Remote cache service (when configured)
    /// 3. Origin S3
    ///
    /// On a cache miss the fetched data is written back to the local
    /// cache for future reads. Mutable paths always go direct to origin.
    ///
    /// # ETag semantics for cache hits
    ///
    /// When the response is served from a cache (local disk or remote
    /// service), the returned `ETag` has `e_tag: None` and
    /// `version: None`. This synthetic ETag must **not** be used for
    /// subsequent CAS updates - CAS on a cached immutable path would
    /// reject any update with a meaningless pre-condition. In practice,
    /// cached paths (shards and xorbs) are content-addressed and
    /// never updated, so no caller uses the returned ETag for CAS.
    /// Mutable paths (refs, manifests) skip the cache entirely and
    /// always get a real ETag from origin. See finding CR11-F3.
    pub async fn get_with_etag(&self, path: &Path) -> Result<(Bytes, ETag)> {
        if let Some(max_bytes) = immutable_read_limit(path) {
            return self.get_with_etag_bounded(path, max_bytes).await;
        }
        let is_immutable = classify_path(path.as_ref()) == PathClass::Immutable;

        if is_immutable {
            // Try local disk cache first.
            if let Some(key) = cache_key_for_path(path.as_ref())
                && let Ok(data) = self
                    .local_cache
                    .get_or_fetch_with(&key, || async {
                        Err(CacheStoreError::Storage(StorageError::NotFound {
                            path: path.as_ref().to_string(),
                        }))
                    })
                    .await
            {
                let etag = ETag {
                    e_tag: None,
                    version: None,
                };
                return Ok((data, etag));
            }

            // Try remote cache service.
            match self.get_cache_service_object(path).await {
                Ok(Some(data)) => {
                    let etag = ETag {
                        e_tag: None,
                        version: None,
                    };
                    return Ok((data, etag));
                }
                Ok(None) => {}
                Err(e) => {
                    tracing::warn!(
                        path = %path,
                        error = %e,
                        "cache service unavailable, falling back to origin",
                    );
                }
            }
        }

        // Origin fetch.
        let result = self.origin.get_with_etag(path).await?;

        // Write back immutable objects to local cache.
        if is_immutable
            && let Some(key) = cache_key_for_path(path.as_ref())
            && let Err(e) = self.local_cache.put_bytes(&key, result.0.clone()).await
        {
            if cache_integrity_error(&e) {
                return Err(e.into());
            }
            tracing::warn!(
                path = %path,
                error = %e,
                "failed to write origin response to local cache",
            );
        }

        Ok(result)
    }

    /// Read an object while enforcing a maximum body size before consumption.
    pub async fn get_with_etag_bounded(
        &self,
        path: &Path,
        max_bytes: u64,
    ) -> Result<(Bytes, ETag)> {
        let max_bytes = immutable_read_limit(path).map_or(max_bytes, |limit| max_bytes.min(limit));
        let is_immutable = classify_path(path.as_ref()) == PathClass::Immutable;

        if is_immutable && let Some(key) = cache_key_for_path(path.as_ref()) {
            if let Ok(data) = self
                .local_cache
                .get_or_fetch_bounded_with(&key, max_bytes, || async {
                    Err::<Bytes, CacheStoreError>(CacheStoreError::Storage(
                        StorageError::NotFound {
                            path: path.as_ref().to_owned(),
                        },
                    ))
                })
                .await
            {
                return Ok((
                    data,
                    ETag {
                        e_tag: None,
                        version: None,
                    },
                ));
            }

            // The HTTP client bounds both advertised and streamed body bytes.
            // Size rejection is a cache failure, so it must reach origin fallback.
            match self.get_cache_service_object_bounded(path, max_bytes).await {
                Ok(Some(data)) => {
                    return Ok((
                        data,
                        ETag {
                            e_tag: None,
                            version: None,
                        },
                    ));
                }
                Ok(None) => {}
                Err(error) => tracing::warn!(
                    family = "remote-object",
                    operation = "bounded-read",
                    path = %path,
                    recovery = "use-bounded-origin",
                    error = %error,
                    "cache service bounded read failed"
                ),
            }
        }

        let result = self.origin.get_with_etag_bounded(path, max_bytes).await?;
        if is_immutable
            && let Some(key) = cache_key_for_path(path.as_ref())
            && let Err(error) = self.local_cache.put_bytes(&key, result.0.clone()).await
        {
            if cache_integrity_error(&error) {
                return Err(error.into());
            }
            tracing::warn!(
                path = %path,
                error = %error,
                "failed to write bounded origin response to local cache"
            );
        }
        Ok(result)
    }

    /// Read an immutable object from the cache service without origin fallback.
    ///
    /// Returns `Ok(None)` when cache-service reads are not enabled. Any cache
    /// service or integrity failure is returned to the caller so proof paths can
    /// treat the object as unverified instead of silently querying origin.
    pub async fn get_cache_service_object(&self, path: &Path) -> Result<Option<Bytes>> {
        let Some(data) = self.get_cache_service_object_without_install(path).await? else {
            return Ok(None);
        };
        #[cfg(feature = "remote-client")]
        {
            self.store_cache_service_response(path, &data).await?;
            Ok(Some(data))
        }
        #[cfg(not(feature = "remote-client"))]
        {
            let _ = data;
            Ok(None)
        }
    }

    async fn get_cache_service_object_bounded(
        &self,
        path: &Path,
        max_bytes: u64,
    ) -> Result<Option<Bytes>> {
        let Some(data) = self
            .get_cache_service_object_without_install_limit(path, Some(max_bytes))
            .await?
        else {
            return Ok(None);
        };
        #[cfg(feature = "remote-client")]
        {
            self.store_cache_service_response(path, &data).await?;
            Ok(Some(data))
        }
        #[cfg(not(feature = "remote-client"))]
        {
            let _ = data;
            Ok(None)
        }
    }

    /// Read an immutable object from the cache service without updating the
    /// local cache. Used by full-object hydrate reads that intentionally avoid
    /// installing a second local copy before reconstruction consumes the body.
    async fn get_cache_service_object_without_install(&self, path: &Path) -> Result<Option<Bytes>> {
        self.get_cache_service_object_without_install_limit(path, immutable_read_limit(path))
            .await
    }

    async fn get_cache_service_object_without_install_limit(
        &self,
        path: &Path,
        max_bytes: Option<u64>,
    ) -> Result<Option<Bytes>> {
        if classify_path(path.as_ref()) == PathClass::Mutable {
            return Ok(None);
        }
        if !self.cache_reads_enabled() {
            return Ok(None);
        }

        #[cfg(feature = "remote-client")]
        {
            let Some(client) = &self.cache_client else {
                return Ok(None);
            };
            let data = match max_bytes {
                Some(max_bytes) => client.get_bounded(path.as_ref(), max_bytes).await?,
                None => client.get(path.as_ref()).await?,
            };
            Ok(Some(data))
        }
        #[cfg(not(feature = "remote-client"))]
        {
            let _ = max_bytes;
            Ok(None)
        }
    }

    #[cfg(feature = "remote-client")]
    async fn get_cache_service_stream(&self, path: &Path) -> Result<Option<GetResult>> {
        if classify_path(path.as_ref()) == PathClass::Mutable || !self.cache_reads_enabled() {
            return Ok(None);
        }

        let Some(client) = &self.cache_client else {
            return Ok(None);
        };
        let Some(response) = client.get_stream(path.as_ref()).await? else {
            return Ok(None);
        };
        let Some(size) = response.content_length() else {
            return Err(CacheError::Service {
                reason: format!("cache service omitted Content-Length for {path}"),
            }
            .into());
        };

        use futures_util::StreamExt as _;

        let stream = response
            .into_stream()
            .map(|result| {
                result.map_err(|error| object_store::Error::Generic {
                    store: "crab-cache",
                    source: Box::new(error),
                })
            })
            .boxed();
        Ok(Some(GetResult {
            payload: GetResultPayload::Stream(stream),
            meta: ObjectMeta {
                location: path.clone(),
                last_modified: SystemTime::now().into(),
                size,
                e_tag: None,
                version: None,
            },
            range: 0..size,
            attributes: Attributes::default(),
            extensions: Default::default(),
        }))
    }

    /// Read cache-service object metadata without origin fallback.
    ///
    /// Returns `Ok(None)` when cache-service reads are not enabled.
    pub async fn head_cache_service_object(&self, path: &Path) -> Result<Option<CacheObjectHead>> {
        if classify_path(path.as_ref()) == PathClass::Mutable {
            return Ok(None);
        }
        if !self.cache_reads_enabled() {
            return Ok(None);
        }

        #[cfg(feature = "remote-client")]
        {
            let Some(client) = &self.cache_client else {
                return Ok(None);
            };

            Ok(client.head(path.as_ref()).await.map(Some)?)
        }
        #[cfg(not(feature = "remote-client"))]
        {
            Ok(None)
        }
    }

    /// Read a cache-service object range without origin fallback.
    ///
    /// Returns `Ok(None)` when cache-service reads are not enabled.
    pub async fn range_get_cache_service_object(
        &self,
        path: &Path,
        range: Range<u64>,
    ) -> Result<Option<CacheObjectRange>> {
        #[cfg(not(feature = "remote-client"))]
        let _ = range;

        if classify_path(path.as_ref()) == PathClass::Mutable {
            return Ok(None);
        }
        if !self.cache_reads_enabled() {
            return Ok(None);
        }
        if let Some(max_bytes) = immutable_read_limit(path) {
            let requested_bytes =
                range
                    .end
                    .checked_sub(range.start)
                    .ok_or_else(|| CacheError::Service {
                        reason: format!("invalid cache range {}..{}", range.start, range.end),
                    })?;
            if requested_bytes > max_bytes {
                return Err(CacheError::CorruptObject {
                    path: path.to_string(),
                    reason: format!(
                        "cache range is {requested_bytes} bytes; bounded read supports at most {max_bytes} bytes"
                    ),
                }
                .into());
            }
        }

        #[cfg(feature = "remote-client")]
        {
            let Some(client) = &self.cache_client else {
                return Ok(None);
            };

            client
                .get_range_with_status(path.as_ref(), range)
                .await
                .map(Some)
                .map_err(Into::into)
        }
        #[cfg(not(feature = "remote-client"))]
        {
            Ok(None)
        }
    }

    #[cfg(feature = "remote-client")]
    async fn store_cache_service_response(&self, path: &Path, data: &Bytes) -> Result<()> {
        let Some(key) = cache_key_for_path(path.as_ref()) else {
            return Ok(());
        };
        match self.local_cache.put_bytes(&key, data.clone()).await {
            Ok(()) => Ok(()),
            Err(e) if cache_integrity_error(&e) => Err(e.into()),
            Err(e) => {
                tracing::warn!(
                    path = %path,
                    error = %e,
                    "failed to write cache service response to local cache",
                );
                Ok(())
            }
        }
    }

    /// Read a byte range from an object.
    ///
    /// Immutable paths are routed through the cache when available.
    pub async fn range_get(&self, path: &Path, range: Range<u64>) -> Result<Bytes> {
        let is_immutable = classify_path(path.as_ref()) == PathClass::Immutable;

        if is_immutable
            && let Some((data, _)) = self.local_cached_range_with_size(path, &range).await
        {
            return Ok(data);
        }

        if is_immutable {
            #[cfg(feature = "remote-client")]
            if self.cache_reads_enabled()
                && let Some(client) = &self.cache_client
            {
                match client.get_range(path.as_ref(), range.clone()).await {
                    Ok(data) => return Ok(data),
                    Err(e) => {
                        tracing::warn!(
                            path = %path,
                            error = %e,
                            "cache service range read failed, falling back to origin",
                        );
                    }
                }
            }
        }

        Ok(self.origin.range_get(path, range).await?)
    }

    async fn local_cached_range_with_size(
        &self,
        path: &Path,
        range: &Range<u64>,
    ) -> Option<(Bytes, u64)> {
        let key = cache_key_for_path(path.as_ref())?;
        if let CacheKey::Xorb(hash) = &key {
            return self
                .local_cache
                .get_xorb_range_with_size_if_present(hash, range.clone())
                .await;
        }

        let data = self
            .local_cache
            .get_or_fetch_bounded_with(
                &key,
                immutable_read_limit(path).unwrap_or(u64::MAX),
                || async {
                    Err(CacheStoreError::Storage(StorageError::NotFound {
                        path: path.as_ref().to_string(),
                    }))
                },
            )
            .await
            .ok()?;
        let total_size = data.len() as u64;
        let Some(slice) = slice_cached_range(&data, range) else {
            tracing::warn!(
                path = %path,
                start = range.start,
                end = range.end,
                cached_len = data.len(),
                "cached immutable object does not cover requested range, falling back to origin",
            );
            return None;
        };
        Some((slice, total_size))
    }

    /// Write an object to the origin store.
    ///
    /// Immutable objects are also written to the local disk cache.
    /// When `push_warming` is enabled the object is additionally PUT
    /// to the remote cache service. Warming failures are non-fatal.
    pub async fn put(&self, path: &Path, bytes: Bytes) -> Result<()> {
        let is_immutable = classify_path(path.as_ref()) == PathClass::Immutable;
        let cache_key = if is_immutable {
            cache_key_for_path(path.as_ref())
        } else {
            None
        };

        if let Some(key) = &cache_key {
            LocalCache::validate_bytes(key, &bytes)?;
        }

        self.origin.put(path, bytes.clone()).await?;

        // Write to local cache for immutable objects.
        if let Some(key) = &cache_key {
            match self.local_cache.put_bytes(key, bytes.clone()).await {
                Ok(()) => {}
                Err(e) if cache_integrity_error(&e) => return Err(e.into()),
                Err(e) => {
                    tracing::warn!(
                        path = %path,
                        error = %e,
                        "failed to write uploaded object to local cache",
                    );
                }
            }
        }

        // Remote push warming.
        if self.push_warming
            && is_immutable
            && let Err(e) = self
                .warm_remote_object(path, bytes, "cache push warming")
                .await
        {
            tracing::warn!(
                path = %path,
                error = %e,
                "cache push warming failed, continuing",
            );
        }

        Ok(())
    }

    /// Stream an object to a local file, using the remote cache service for
    /// immutable paths when it is enabled and falling back to origin.
    ///
    /// The byte bound protects fetch callers from a cache response that is
    /// larger than the manifest commitment. Cache failures are advisory: the
    /// authoritative origin remains the correctness path.
    pub async fn download_to_path_bounded(
        &self,
        path: &Path,
        dest: &std::path::Path,
        max_bytes: u64,
    ) -> Result<u64> {
        if classify_path(path.as_ref()) == PathClass::Immutable {
            #[cfg(feature = "remote-client")]
            if self.cache_reads_enabled()
                && let Some(client) = &self.cache_client
            {
                match client
                    .download_to_path_bounded(path.as_ref(), dest, max_bytes)
                    .await
                {
                    Ok(Some(bytes)) => return Ok(bytes),
                    Ok(None) => {
                        tracing::debug!(path = %path, "cache service stream miss, using origin")
                    }
                    Err(error) => {
                        tracing::warn!(
                            path = %path,
                            error = %error,
                            "cache service stream failed, falling back to origin",
                        );
                    }
                }
            }
        }

        self.origin
            .download_to_path_bounded(path, dest, max_bytes)
            .await
            .map_err(Into::into)
    }

    /// Stream an object to a local file without buffering the body.
    pub async fn download_to_path(&self, path: &Path, dest: &std::path::Path) -> Result<u64> {
        self.download_to_path_bounded(path, dest, u64::MAX).await
    }

    /// Batch dedup query against the cache service's chunk index.
    ///
    /// Requests contain at most 50,000 unique hashes. A failed request marks
    /// only its batch unknown; successful batches and duplicate input
    /// positions retain their results.
    pub async fn dedup_query(
        &self,
        repo_path: &str,
        hashes: &[[u8; 32]],
    ) -> Result<DedupQueryResult> {
        #[cfg(not(feature = "remote-client"))]
        let _ = repo_path;

        #[cfg(feature = "remote-client")]
        if self.dedup_enabled()
            && let Some(client) = &self.cache_client
        {
            let mut unique_indexes: HashMap<[u8; 32], usize> = HashMap::with_capacity(hashes.len());
            let mut unique_hashes = Vec::with_capacity(hashes.len());
            let mut input_indexes: Vec<Vec<usize>> = Vec::with_capacity(hashes.len());
            for (input_index, hash) in hashes.iter().copied().enumerate() {
                if let Some(&unique_index) = unique_indexes.get(&hash) {
                    input_indexes[unique_index].push(input_index);
                    continue;
                }
                let unique_index = unique_hashes.len();
                unique_indexes.insert(hash, unique_index);
                unique_hashes.push(hash);
                input_indexes.push(vec![input_index]);
            }

            let mut known_unique: Vec<Option<crab_cache::KnownChunk>> =
                (0..unique_hashes.len()).map(|_| None).collect();
            for (batch_index, batch) in unique_hashes
                .chunks(DEDUP_QUERY_MAX_UNIQUE_HASHES)
                .enumerate()
            {
                let batch_start = batch_index * DEDUP_QUERY_MAX_UNIQUE_HASHES;
                match client.dedup_query(repo_path, batch).await {
                    Ok(result) => {
                        for known in result.known {
                            let Some(slot) = known_unique.get_mut(batch_start + known.index) else {
                                tracing::warn!(
                                    repo_path = %repo_path,
                                    batch = batch_index,
                                    index = known.index,
                                    count = batch.len(),
                                    "cache service dedup response referenced an out-of-range batch index"
                                );
                                continue;
                            };
                            *slot = Some(known);
                        }
                    }
                    Err(e) => {
                        // Dedup is advisory. Preserve successes from other
                        // batches and classify only this batch as unknown.
                        tracing::warn!(
                            repo_path = %repo_path,
                            batch = batch_index,
                            count = batch.len(),
                            error = %e,
                            "dedup query batch failed, treating this batch as unknown",
                        );
                    }
                }
            }

            let mut known = Vec::new();
            let mut unknown = Vec::new();
            for (unique_index, occurrences) in input_indexes.into_iter().enumerate() {
                match known_unique[unique_index].as_ref() {
                    Some(hit) => {
                        known.extend(occurrences.into_iter().map(|index| crab_cache::KnownChunk {
                            index,
                            xorb_hash: hit.xorb_hash.clone(),
                            chunk_index: hit.chunk_index,
                            length: hit.length,
                            cache_verified: hit.cache_verified,
                        }));
                    }
                    None => unknown.extend(occurrences),
                }
            }
            known.sort_unstable_by_key(|hit| hit.index);
            unknown.sort_unstable();
            return Ok(DedupQueryResult { known, unknown });
        }

        // All unknown - indices 0..N.
        Ok(DedupQueryResult {
            known: Vec::new(),
            unknown: (0..hashes.len()).collect(),
        })
    }

    /// Query the cache service for cache-local dedup candidates.
    ///
    /// Converts between `MerkleHash` and `[u8; 32]` so callers in the
    /// push pipeline don't need to know about the wire format. Results
    /// are candidates only; publication must independently prove the
    /// canonical origin object is durable.
    ///
    /// On any error, returns an empty set so the caller proceeds
    /// normally - dedup is an optimisation, not a correctness gate.
    pub async fn query_known_chunks(
        &self,
        repo_path: &str,
        chunk_hashes: &[MerkleHash],
    ) -> HashSet<MerkleHash> {
        let raw: Vec<[u8; 32]> = chunk_hashes.iter().map(|h| (*h).into()).collect();

        match self.dedup_query(repo_path, &raw).await {
            Ok(result) => result
                .known
                .iter()
                .filter(|k| k.cache_verified)
                .filter_map(|k| {
                    if let Some(hash) = raw.get(k.index) {
                        Some(MerkleHash::from(*hash))
                    } else {
                        tracing::warn!(
                            index = k.index,
                            candidates = raw.len(),
                            "cache service dedup response referenced an out-of-range chunk index"
                        );
                        None
                    }
                })
                .collect(),
            Err(e) => {
                tracing::warn!(
                    repo_path = %repo_path,
                    error = %e,
                    "query_known_chunks failed, treating all as unknown",
                );
                HashSet::new()
            }
        }
    }

    /// Fetch object metadata. Always goes direct to origin because
    /// metadata (size, last-modified) is mutable.
    pub async fn head(&self, path: &Path) -> Result<ObjectMeta> {
        Ok(self.origin.head(path).await?)
    }

    /// Delete an object. Always goes direct to origin.
    pub async fn delete(&self, path: &Path) -> Result<()> {
        Ok(self.origin.delete(path).await?)
    }

    /// PUT to the remote cache service only (no origin upload).
    ///
    /// Used for post-push warming where origin already has the data.
    /// Failures are non-fatal - a warning is logged and `Ok(())` is
    /// returned regardless.
    pub async fn warm_remote_only(&self, path: &Path, bytes: Bytes) -> Result<()> {
        if let Err(e) = self
            .warm_remote_object(path, bytes, "remote cache warming")
            .await
        {
            tracing::warn!(path = %path, error = %e, "remote cache warming failed");
        }
        Ok(())
    }

    /// Whether remote cache warming would attempt to send this object.
    #[must_use]
    pub fn should_warm_remote_object(&self, path: &Path, len: u64) -> bool {
        #[cfg(not(feature = "remote-client"))]
        let _ = len;

        if classify_path(path.as_ref()) == PathClass::Mutable {
            return false;
        }
        if !self.push_warming {
            return false;
        }

        #[cfg(not(feature = "remote-client"))]
        {
            false
        }
        #[cfg(feature = "remote-client")]
        {
            if self.cache_client.is_none() {
                return false;
            }
            self.max_push_warming_object_bytes
                .is_none_or(|max_bytes| len <= max_bytes)
        }
    }

    async fn warm_remote_object(
        &self,
        path: &Path,
        bytes: Bytes,
        operation: &'static str,
    ) -> Result<()> {
        #[cfg(not(feature = "remote-client"))]
        let _ = (&bytes, operation);

        if classify_path(path.as_ref()) == PathClass::Mutable {
            return Ok(());
        }
        if !self.push_warming {
            return Ok(());
        }

        #[cfg(feature = "remote-client")]
        {
            let Some(client) = &self.cache_client else {
                return Ok(());
            };
            if let Some(max_bytes) = self.max_push_warming_object_bytes
                && bytes.len() as u64 > max_bytes
            {
                tracing::debug!(
                    path = %path,
                    bytes = bytes.len(),
                    max_bytes,
                    operation,
                    "skipping oversized cache service push warming body"
                );
                return Ok(());
            }
            return client.put(path.as_ref(), bytes).await.map_err(Into::into);
        }

        #[cfg(not(feature = "remote-client"))]
        return Ok(());
    }
}

#[derive(Clone)]
struct CacheAwareObjectStore {
    store: CachingStore,
}

impl fmt::Debug for CacheAwareObjectStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CacheAwareObjectStore")
            .finish_non_exhaustive()
    }
}

impl fmt::Display for CacheAwareObjectStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("CacheAwareObjectStore")
    }
}

#[async_trait]
impl ObjectStore for CacheAwareObjectStore {
    async fn put_opts(
        &self,
        location: &Path,
        payload: PutPayload,
        opts: PutOptions,
    ) -> object_store::Result<PutResult> {
        let warm_body = (self.store.push_warming
            && classify_path(location.as_ref()) == PathClass::Immutable)
            .then(|| Bytes::from(payload.clone()));
        let result = self
            .store
            .origin()
            .inner()
            .put_opts(location, payload, opts)
            .await?;

        if let Some(body) = warm_body
            && let Err(e) = self.store.warm_remote_only(location, body).await
        {
            tracing::warn!(
                path = %location,
                error = %e,
                "object-store cache push warming failed, continuing",
            );
        }

        Ok(result)
    }

    async fn put_multipart_opts(
        &self,
        location: &Path,
        opts: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        self.store
            .origin()
            .inner()
            .put_multipart_opts(location, opts)
            .await
    }

    async fn get_opts(
        &self,
        location: &Path,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        if !cacheable_get_options(&options)
            || classify_path(location.as_ref()) == PathClass::Mutable
        {
            return self
                .store
                .origin()
                .inner()
                .get_opts(location, options)
                .await;
        }

        let (body, returned_range, object_size) = if options.head {
            let meta = head_immutable_object(&self.store, location).await?;
            (Bytes::new(), 0..0, meta.size)
        } else if let Some(range) = options.range {
            match range {
                GetRange::Bounded(requested) => {
                    if let Some((body, range, object_size)) =
                        bounded_cache_range(&self.store, location, requested.clone()).await?
                    {
                        (body, range, object_size)
                    } else {
                        let (range, object_size) = resolve_cache_range(
                            &self.store,
                            location,
                            GetRange::Bounded(requested),
                        )
                        .await?;
                        let body = self
                            .store
                            .range_get(location, range.clone())
                            .await
                            .map_err(|e| cache_error(location, e))?;
                        (body, range, object_size)
                    }
                }
                range => {
                    let (range, object_size) =
                        resolve_cache_range(&self.store, location, range).await?;
                    let body = self
                        .store
                        .range_get(location, range.clone())
                        .await
                        .map_err(|e| cache_error(location, e))?;
                    (body, range, object_size)
                }
            }
        } else {
            #[cfg(feature = "remote-client")]
            if cache_key_for_path(location.as_ref()).is_none() {
                match self.store.get_cache_service_stream(location).await {
                    Ok(Some(result)) => return Ok(result),
                    Ok(None) => {}
                    Err(error) => {
                        tracing::warn!(
                            path = %location,
                            error = %error,
                            "cache service stream failed, falling back to origin",
                        );
                    }
                }

                return self
                    .store
                    .origin()
                    .inner()
                    .get_opts(location, options)
                    .await;
            }

            let (body, _) = self
                .store
                .get_with_etag(location)
                .await
                .map_err(|e| cache_error(location, e))?;
            let len = body.len() as u64;
            (body, 0..len, len)
        };

        Ok(bytes_get_result(
            location.clone(),
            body,
            returned_range,
            object_size,
        ))
    }

    fn delete_stream(
        &self,
        locations: futures_util::stream::BoxStream<'static, object_store::Result<Path>>,
    ) -> futures_util::stream::BoxStream<'static, object_store::Result<Path>> {
        self.store.origin().inner().delete_stream(locations)
    }

    fn list(
        &self,
        prefix: Option<&Path>,
    ) -> futures_util::stream::BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.store.origin().inner().list(prefix)
    }

    async fn list_with_delimiter(&self, prefix: Option<&Path>) -> object_store::Result<ListResult> {
        self.store
            .origin()
            .inner()
            .list_with_delimiter(prefix)
            .await
    }

    async fn copy_opts(
        &self,
        from: &Path,
        to: &Path,
        options: CopyOptions,
    ) -> object_store::Result<()> {
        self.store
            .origin()
            .inner()
            .copy_opts(from, to, options)
            .await
    }
}

fn cacheable_get_options(options: &GetOptions) -> bool {
    options.if_match.is_none()
        && options.if_none_match.is_none()
        && options.if_modified_since.is_none()
        && options.if_unmodified_since.is_none()
        && options.version.is_none()
}

async fn bounded_cache_range(
    store: &CachingStore,
    location: &Path,
    requested: Range<u64>,
) -> object_store::Result<Option<(Bytes, Range<u64>, u64)>> {
    GetRange::Bounded(requested.clone())
        .is_valid()
        .map_err(|e| object_store::Error::Generic {
            store: "crab-cache",
            source: Box::new(e),
        })?;

    if let Some((body, object_size)) = store
        .local_cached_range_with_size(location, &requested)
        .await
    {
        return Ok(Some((body, requested, object_size)));
    }

    match store
        .range_get_cache_service_object(location, requested.clone())
        .await
    {
        Ok(Some(range)) => Ok(Some((range.data, range.range, range.total_size))),
        Ok(None) => Ok(None),
        Err(e) => {
            tracing::warn!(
                path = %location,
                error = %e,
                "cache service bounded range read failed, falling back to resolved range",
            );
            Ok(None)
        }
    }
}

async fn resolve_cache_range(
    store: &CachingStore,
    location: &Path,
    range: GetRange,
) -> object_store::Result<(Range<u64>, u64)> {
    let len = head_immutable_object(store, location).await?.size;
    let range = range
        .as_range(len)
        .map_err(|e| object_store::Error::Generic {
            store: "crab-cache",
            source: Box::new(e),
        })?;
    Ok((range, len))
}

async fn head_immutable_object(
    store: &CachingStore,
    location: &Path,
) -> object_store::Result<ObjectMeta> {
    match store.head_cache_service_object(location).await {
        Ok(Some(head)) => {
            return Ok(ObjectMeta {
                location: location.clone(),
                last_modified: SystemTime::now().into(),
                size: head.size,
                e_tag: None,
                version: None,
            });
        }
        Ok(None) => {}
        Err(e) => {
            tracing::warn!(
                path = %location,
                error = %e,
                "cache service HEAD failed, falling back to origin",
            );
        }
    }

    store.origin().inner().head(location).await
}

#[cfg(feature = "remote-client")]
fn cache_service_capabilities_route_contract_current(
    capabilities: &CacheServiceCapabilities,
) -> bool {
    capabilities
        .routes
        .as_ref()
        .is_some_and(cache_route_contract_matches_current)
}

fn bytes_get_result(location: Path, body: Bytes, range: Range<u64>, object_size: u64) -> GetResult {
    let payload =
        GetResultPayload::Stream(Box::pin(futures_util::stream::once(async { Ok(body) })));
    GetResult {
        payload,
        meta: ObjectMeta {
            location,
            last_modified: SystemTime::now().into(),
            size: object_size,
            e_tag: None,
            version: None,
        },
        range,
        attributes: Attributes::default(),
        extensions: Default::default(),
    }
}

fn cache_error(path: &Path, error: CacheStoreError) -> object_store::Error {
    if matches!(
        error,
        CacheStoreError::Storage(StorageError::NotFound { .. })
    ) {
        object_store::Error::NotFound {
            path: path.to_string(),
            source: Box::new(error),
        }
    } else {
        object_store::Error::Generic {
            store: "crab-cache",
            source: Box::new(error),
        }
    }
}

fn cache_integrity_error(error: &CacheError) -> bool {
    matches!(
        error,
        CacheError::HashMismatch { .. } | CacheError::CorruptObject { .. }
    )
}

fn slice_cached_range(data: &Bytes, range: &Range<u64>) -> Option<Bytes> {
    let start = usize::try_from(range.start).ok()?;
    let end = usize::try_from(range.end).ok()?;
    if start > end || end > data.len() {
        return None;
    }
    Some(data.slice(start..end))
}

fn immutable_read_limit(path: &Path) -> Option<u64> {
    match cache_key_for_path(path.as_ref())? {
        CacheKey::Chunk(_) => Some(MAX_CACHE_CHUNK_BYTES),
        CacheKey::Shard(_) => Some(MAX_CACHE_SHARD_BYTES),
        CacheKey::Xorb(_) => Some(MAX_XORB_SIZE as u64),
        CacheKey::Stage(_) | CacheKey::Manifest { .. } => None,
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, clippy::expect_used, reason = "test assertions")]
mod tests {
    use super::*;

    #[test]
    fn domain_wrappers_preserve_typed_source_and_display() {
        use std::error::Error as _;

        let storage = CacheStoreError::from(StorageError::Forbidden {
            path: "repo/xorbs/private".into(),
        });
        let cache = CacheStoreError::from(CacheError::Cancelled);
        for wrapped in [storage, cache] {
            let source = wrapped.source().unwrap();
            assert_eq!(wrapped.to_string(), source.to_string());
            match &wrapped {
                CacheStoreError::Storage(inner) => {
                    assert!(std::ptr::eq(
                        source.downcast_ref::<StorageError>().unwrap(),
                        inner
                    ));
                }
                CacheStoreError::Cache(inner) => {
                    assert!(std::ptr::eq(
                        source.downcast_ref::<CacheError>().unwrap(),
                        inner
                    ));
                }
                CacheStoreError::OriginIntegrity { .. } => unreachable!(),
            }
        }
    }

    #[cfg(feature = "remote-client")]
    use async_trait::async_trait;
    #[cfg(feature = "remote-client")]
    use crab_cache_server::cache_store::CacheStore;
    #[cfg(feature = "remote-client")]
    use crab_cache_server::chunk_index::ChunkIndex;
    #[cfg(feature = "remote-client")]
    use crab_cache_server::config::{AuthConfig, CacheServerConfig, DedupScope, MutablePathMode};
    #[cfg(feature = "remote-client")]
    use crab_cache_server::db::{CACHE_DB_FILE, CacheDb};
    #[cfg(feature = "remote-client")]
    use crab_cache_server::evictor::start_evictor_task;
    #[cfg(feature = "remote-client")]
    use crab_cache_server::metrics::CacheMetrics;
    #[cfg(feature = "remote-client")]
    use crab_cache_server::origin_client::OriginClient;
    #[cfg(feature = "remote-client")]
    use crab_cache_server::state::{AppState, DedupIndexRebuildStats, build_router};
    use crab_storage::Store;
    use crab_storage::test_support::{CountingObjectStore, ObjectReadCounts, ObjectReadKind};
    use crab_xet::xorb::builder::{CompressionPolicy, FixedCompression, RunId, XorbBuilder};
    use crab_xet::xorb::format::{Chunk, CompressionScheme};
    use crab_xet::xorb::parser::XorbParser;
    use futures_util::stream::BoxStream;
    #[cfg(feature = "remote-client")]
    use object_store::GetRange;
    use object_store::memory::InMemory;
    use object_store::path::Path as ObjectPath;
    use object_store::{
        GetOptions, GetResult, ListResult, MultipartUpload, ObjectMeta, ObjectStore,
        ObjectStoreExt, PutMultipartOptions, PutOptions, PutPayload, PutResult,
    };
    #[cfg(feature = "remote-client")]
    use std::net::SocketAddr;
    use std::sync::Arc;
    #[cfg(feature = "remote-client")]
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::{AtomicUsize, Ordering};
    #[cfg(feature = "remote-client")]
    use std::time::Duration;
    #[cfg(feature = "remote-client")]
    use tempfile::TempDir;
    #[cfg(feature = "remote-client")]
    use tokio::net::TcpListener;
    #[cfg(feature = "remote-client")]
    use tokio::time::Instant;

    #[cfg(feature = "remote-client")]
    const TEST_PSK: &str = "test-psk-key";
    #[cfg(feature = "remote-client")]
    const TEST_MAX_CACHE_BYTES: u64 = 1_048_576;

    #[derive(Debug)]
    struct CountingStore {
        inner: Arc<InMemory>,
        get_count: Arc<AtomicUsize>,
    }

    impl std::fmt::Display for CountingStore {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "CountingStore")
        }
    }

    #[async_trait]
    impl ObjectStore for CountingStore {
        async fn put_opts(
            &self,
            location: &ObjectPath,
            payload: PutPayload,
            opts: PutOptions,
        ) -> object_store::Result<PutResult> {
            self.inner.put_opts(location, payload, opts).await
        }

        async fn put_multipart_opts(
            &self,
            location: &ObjectPath,
            opts: PutMultipartOptions,
        ) -> object_store::Result<Box<dyn MultipartUpload>> {
            self.inner.put_multipart_opts(location, opts).await
        }

        async fn get_opts(
            &self,
            location: &ObjectPath,
            options: GetOptions,
        ) -> object_store::Result<GetResult> {
            if !options.head {
                self.get_count.fetch_add(1, Ordering::Relaxed);
            }
            self.inner.get_opts(location, options).await
        }

        fn delete_stream(
            &self,
            locations: BoxStream<'static, object_store::Result<ObjectPath>>,
        ) -> BoxStream<'static, object_store::Result<ObjectPath>> {
            self.inner.delete_stream(locations)
        }

        fn list(
            &self,
            prefix: Option<&ObjectPath>,
        ) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
            self.inner.list(prefix)
        }

        async fn list_with_delimiter(
            &self,
            prefix: Option<&ObjectPath>,
        ) -> object_store::Result<ListResult> {
            self.inner.list_with_delimiter(prefix).await
        }

        async fn copy_opts(
            &self,
            from: &ObjectPath,
            to: &ObjectPath,
            options: object_store::CopyOptions,
        ) -> object_store::Result<()> {
            self.inner.copy_opts(from, to, options).await
        }
    }

    #[cfg(feature = "remote-client")]
    struct TestCacheServer {
        addr: SocketAddr,
        origin: Arc<InMemory>,
        origin_get_count: Arc<AtomicUsize>,
        shutdown: Option<tokio::sync::oneshot::Sender<()>>,
        _tempdir: TempDir,
    }

    #[cfg(feature = "remote-client")]
    impl Drop for TestCacheServer {
        fn drop(&mut self) {
            if let Some(shutdown) = self.shutdown.take() {
                let _ = shutdown.send(());
            }
        }
    }

    #[cfg(feature = "remote-client")]
    struct MalformedDedupServer {
        addr: SocketAddr,
        shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    }

    #[cfg(feature = "remote-client")]
    struct BatchedDedupServer {
        addr: SocketAddr,
        request_sizes: Arc<std::sync::Mutex<Vec<usize>>>,
        shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    }

    #[cfg(feature = "remote-client")]
    impl Drop for MalformedDedupServer {
        fn drop(&mut self) {
            if let Some(shutdown) = self.shutdown.take() {
                let _ = shutdown.send(());
            }
        }
    }

    #[cfg(feature = "remote-client")]
    struct MalformedObjectServer {
        addr: SocketAddr,
        shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    }

    #[cfg(feature = "remote-client")]
    impl Drop for MalformedObjectServer {
        fn drop(&mut self) {
            if let Some(shutdown) = self.shutdown.take() {
                let _ = shutdown.send(());
            }
        }
    }

    fn origin_store() -> Store {
        Store::new(Arc::new(InMemory::new()))
    }

    fn test_xorb(payload: &[u8]) -> (Bytes, String) {
        let chunk = Chunk::new(Bytes::copy_from_slice(payload));
        let mut builder = XorbBuilder::new();
        builder.push(&chunk, RunId(0)).unwrap();
        let mut xorbs = builder.finalize().unwrap();
        let xorb = xorbs.pop().unwrap();
        (xorb.bytes, xorb.hash.hex())
    }

    fn content_path(kind: &str, hash: &str) -> Path {
        Path::from(format!(".crab/{kind}/{}/{hash}", &hash[..2]))
    }

    fn test_raw_xorb(payloads: &[Bytes]) -> (Bytes, MerkleHash) {
        let policy: Arc<dyn CompressionPolicy> =
            Arc::new(FixedCompression::new(CompressionScheme::None));
        let mut builder = XorbBuilder::with_policy(policy);
        for payload in payloads {
            builder
                .push(&Chunk::new(payload.clone()), RunId(0))
                .unwrap();
        }
        let mut xorbs = builder.finalize().unwrap();
        let xorb = xorbs.pop().unwrap();
        assert!(xorbs.is_empty());
        (xorb.bytes, xorb.hash)
    }

    fn no_cache_config() -> CacheConfig {
        CacheConfig {
            max_bytes: DEFAULT_LOCAL_CACHE_MAX_BYTES,
            service_url: None,
            service_mode: CacheServiceMode::CacheAndDedup,
            push_warming: true,
            service_auth: CacheServiceAuth::None,
            service_ca_cert: None,
            service_client_cert: None,
            service_client_key: None,
        }
    }

    #[tokio::test]
    async fn bounded_read_rejects_oversized_origin_before_caching() {
        let hash = "a".repeat(64);
        let path = content_path("shards", &hash);
        let origin = origin_store();
        origin.put(&path, Bytes::from(vec![0u8; 32])).await.unwrap();
        let tempdir = tempfile::tempdir().unwrap();
        let cache = Arc::new(LocalCache::new(tempdir.path().join("cache")));
        let store =
            CachingStore::new_with_local_cache(origin, no_cache_config(), Arc::clone(&cache))
                .unwrap();

        let error = store.get_with_etag_bounded(&path, 8).await.unwrap_err();

        assert!(matches!(
            error,
            CacheStoreError::Storage(StorageError::CorruptObject { .. })
        ));
        assert_eq!(
            cache
                .cached_size(&CacheKey::Shard(MerkleHash::from_hex(&hash).unwrap()))
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn bounded_read_repairs_oversized_local_entry_from_origin() {
        let good_body = Bytes::from_static(b"verified shard body");
        let hash = crab_xet::hash::compute_data_hash(&good_body);
        let path = content_path("shards", &hash.hex());
        let inner = Arc::new(InMemory::new());
        inner
            .put(&path, PutPayload::from_bytes(good_body.clone()))
            .await
            .unwrap();
        let counting_origin = Arc::new(CountingObjectStore::new(inner));
        let origin = Store::new(Arc::clone(&counting_origin) as Arc<dyn ObjectStore>);
        let tempdir = tempfile::tempdir().unwrap();
        let cache = Arc::new(LocalCache::new(tempdir.path().join("cache")));
        let key = CacheKey::Shard(hash);
        cache
            .put_unchecked_for_test(&key, &[0x55; 256])
            .await
            .unwrap();
        let store =
            CachingStore::new_with_local_cache(origin, no_cache_config(), Arc::clone(&cache))
                .unwrap();

        let (got, _) = store
            .get_with_etag_bounded(&path, good_body.len() as u64)
            .await
            .unwrap();

        assert_eq!(got, good_body);
        assert_eq!(counting_origin.counts().body_requests(), 1);
        assert_eq!(
            cache.cached_size(&key).await.unwrap(),
            Some(good_body.len() as u64)
        );
    }

    #[tokio::test]
    async fn bounded_read_repairs_wrong_hash_local_entry_from_origin() {
        let good_body = Bytes::from_static(b"verified shard body");
        let hash = crab_xet::hash::compute_data_hash(&good_body);
        let path = content_path("shards", &hash.hex());
        let inner = Arc::new(InMemory::new());
        inner
            .put(&path, PutPayload::from_bytes(good_body.clone()))
            .await
            .unwrap();
        let counting_origin = Arc::new(CountingObjectStore::new(inner));
        let origin = Store::new(Arc::clone(&counting_origin) as Arc<dyn ObjectStore>);
        let tempdir = tempfile::tempdir().unwrap();
        let cache = Arc::new(LocalCache::new(tempdir.path().join("cache")));
        let key = CacheKey::Shard(hash);
        cache
            .put_unchecked_for_test(&key, b"wrong shard body")
            .await
            .unwrap();
        let store =
            CachingStore::new_with_local_cache(origin, no_cache_config(), Arc::clone(&cache))
                .unwrap();

        let (got, _) = store
            .get_with_etag_bounded(&path, good_body.len() as u64)
            .await
            .unwrap();

        assert_eq!(got, good_body);
        assert_eq!(counting_origin.counts().body_requests(), 1);
        assert!(cache.contains_verified(&key).await);
    }

    #[tokio::test]
    async fn leased_cache_working_set_does_not_block_valid_origin() {
        const MIB: usize = 1024 * 1024;
        let good_body = Bytes::from(vec![2; 3 * MIB]);
        let hash = crab_xet::hash::compute_data_hash(&good_body);
        let path = content_path("shards", &hash.hex());
        let inner = Arc::new(InMemory::new());
        inner
            .put(&path, PutPayload::from_bytes(good_body.clone()))
            .await
            .unwrap();
        let counting_origin = Arc::new(CountingObjectStore::new(inner));
        let origin = Store::new(Arc::clone(&counting_origin) as Arc<dyn ObjectStore>);
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("cache");
        let cache = Arc::new(LocalCache::with_limits(root.clone(), 10 * MIB as u64, None));
        let old_body = Bytes::from(vec![1; 8 * MIB]);
        let old_hash = crab_xet::hash::compute_data_hash(&old_body);
        let old_key = CacheKey::Shard(old_hash);
        cache.put(&old_key, &old_body).await.unwrap();
        let old_hex = old_hash.hex();
        let old_path = root.join("shards").join(&old_hex[..2]).join(old_hex);
        let catalog = crab_cache::CacheCatalog::new(root, cache.max_bytes());
        let lease = catalog.lease(&old_path).await.unwrap();
        let store =
            CachingStore::new_with_local_cache(origin, no_cache_config(), Arc::clone(&cache))
                .unwrap();

        let (got, _) = store
            .get_with_etag_bounded(&path, good_body.len() as u64)
            .await
            .unwrap();

        assert_eq!(got, good_body);
        assert_eq!(counting_origin.counts().body_requests(), 1);
        assert!(!cache.contains(&CacheKey::Shard(hash)).await);
        assert!(cache.contains_verified(&old_key).await);
        drop(lease);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn replaced_catalog_bypasses_cache_until_old_generation_is_released() {
        let good_body = Bytes::from_static(b"origin survives replaced catalog");
        let hash = crab_xet::hash::compute_data_hash(&good_body);
        let path = content_path("shards", &hash.hex());
        let inner = Arc::new(InMemory::new());
        inner
            .put(&path, PutPayload::from_bytes(good_body.clone()))
            .await
            .unwrap();
        let counting_origin = Arc::new(CountingObjectStore::new(inner));
        let origin = Store::new(Arc::clone(&counting_origin) as Arc<dyn ObjectStore>);
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("cache");
        let cache = Arc::new(LocalCache::new(root.clone()));
        let catalog = crab_cache::CacheCatalog::new(root.clone(), cache.max_bytes());
        let reservation = catalog
            .reserve(&root.join("pending"), 7)
            .await
            .unwrap()
            .unwrap();
        let main = root.join(".catalog.sqlite");
        let retired = tmp.path().join("retired.sqlite");
        std::fs::rename(&main, &retired).unwrap();
        std::fs::copy(retired, &main).unwrap();
        let before = std::fs::read(&main).unwrap();
        let key = CacheKey::Shard(hash);
        let store =
            CachingStore::new_with_local_cache(origin, no_cache_config(), Arc::clone(&cache))
                .unwrap();

        let (got, _) = store
            .get_with_etag_bounded(&path, good_body.len() as u64)
            .await
            .unwrap();

        assert_eq!(got, good_body);
        assert_eq!(counting_origin.counts().body_requests(), 1);
        assert!(!cache.contains(&key).await);
        assert!(
            std::fs::read(&main).unwrap() == before,
            "replacement catalog changed"
        );
        drop(reservation);
        let (got, _) = store
            .get_with_etag_bounded(&path, good_body.len() as u64)
            .await
            .unwrap();
        assert_eq!(got, good_body);
        assert!(cache.contains_verified(&key).await);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unwritable_cache_root_does_not_block_valid_origin() {
        use std::os::unix::fs::PermissionsExt as _;

        let good_body = Bytes::from_static(b"origin survives local write failure");
        let hash = crab_xet::hash::compute_data_hash(&good_body);
        let path = content_path("shards", &hash.hex());
        let inner = Arc::new(InMemory::new());
        inner
            .put(&path, PutPayload::from_bytes(good_body.clone()))
            .await
            .unwrap();
        let counting_origin = Arc::new(CountingObjectStore::new(inner));
        let origin = Store::new(Arc::clone(&counting_origin) as Arc<dyn ObjectStore>);
        let tempdir = tempfile::tempdir().unwrap();
        let root = tempdir.path().join("cache");
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o500)).unwrap();
        let cache = Arc::new(LocalCache::new(root));
        let key = CacheKey::Shard(hash);
        let store =
            CachingStore::new_with_local_cache(origin, no_cache_config(), Arc::clone(&cache))
                .unwrap();

        let (got, _) = store
            .get_with_etag_bounded(&path, good_body.len() as u64)
            .await
            .unwrap();

        assert_eq!(got, good_body);
        assert_eq!(counting_origin.counts().body_requests(), 1);
        assert!(!cache.contains(&key).await);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unsafe_root_after_construction_does_not_block_valid_origin() {
        use std::os::unix::fs::PermissionsExt as _;

        let good_body = Bytes::from_static(b"origin survives unsafe root");
        let hash = crab_xet::hash::compute_data_hash(&good_body);
        let path = content_path("shards", &hash.hex());
        let inner = Arc::new(InMemory::new());
        inner
            .put(&path, PutPayload::from_bytes(good_body.clone()))
            .await
            .unwrap();
        let origin = Store::new(inner as Arc<dyn ObjectStore>);
        let tempdir = tempfile::tempdir().unwrap();
        let root = tempdir.path().join("cache");
        let cache = Arc::new(LocalCache::new(root.clone()));
        let key = CacheKey::Shard(hash);
        cache
            .put_unchecked_for_test(&key, b"stale local body")
            .await
            .unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        let store =
            CachingStore::new_with_local_cache(origin, no_cache_config(), Arc::clone(&cache))
                .unwrap();

        let (got, _) = store
            .get_with_etag_bounded(&path, good_body.len() as u64)
            .await
            .unwrap();

        assert_eq!(got, good_body);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlinked_cache_entry_is_not_consumed() {
        use std::os::unix::fs::symlink;

        let good_body = Bytes::from_static(b"verified origin shard");
        let hash = crab_xet::hash::compute_data_hash(&good_body);
        let path = content_path("shards", &hash.hex());
        let inner = Arc::new(InMemory::new());
        inner
            .put(&path, PutPayload::from_bytes(good_body.clone()))
            .await
            .unwrap();
        let origin = Store::new(inner as Arc<dyn ObjectStore>);
        let tempdir = tempfile::tempdir().unwrap();
        let root = tempdir.path().join("cache");
        let cache = Arc::new(LocalCache::new(root.clone()));
        let key = CacheKey::Shard(hash);
        cache
            .put_unchecked_for_test(&key, b"temporary fixture")
            .await
            .unwrap();
        let entry = root.join("shards").join(&hash.hex()[..2]).join(hash.hex());
        tokio::fs::remove_file(&entry).await.unwrap();
        let outside = tempdir.path().join("outside");
        tokio::fs::write(&outside, b"attacker-controlled bytes")
            .await
            .unwrap();
        symlink(&outside, &entry).unwrap();
        let store = CachingStore::new_with_local_cache(origin, no_cache_config(), cache).unwrap();

        let (got, _) = store
            .get_with_etag_bounded(&path, good_body.len() as u64)
            .await
            .unwrap();

        assert_eq!(got, good_body);
        assert!(!entry.is_symlink());
    }

    #[cfg(feature = "remote-client")]
    fn cache_service_config(addr: SocketAddr) -> CacheConfig {
        CacheConfig {
            max_bytes: DEFAULT_LOCAL_CACHE_MAX_BYTES,
            service_url: Some(format!("http://{addr}")),
            service_mode: CacheServiceMode::CacheAndDedup,
            push_warming: false,
            service_auth: CacheServiceAuth::Psk(TEST_PSK.to_string()),
            service_ca_cert: None,
            service_client_cert: None,
            service_client_key: None,
        }
    }

    #[cfg(feature = "remote-client")]
    fn cache_service_push_warming_config(addr: SocketAddr) -> CacheConfig {
        CacheConfig {
            push_warming: true,
            ..cache_service_config(addr)
        }
    }

    #[cfg(feature = "remote-client")]
    async fn cache_server_admin_stats(server: &TestCacheServer) -> serde_json::Value {
        reqwest::Client::new()
            .get(format!("http://{}/v1/admin/stats", server.addr))
            .header("x-cache-psk", TEST_PSK)
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap()
    }

    #[cfg(feature = "remote-client")]
    fn traffic_value(stats: &serde_json::Value, name: &str) -> u64 {
        stats
            .get("traffic")
            .and_then(|traffic| traffic.get(name))
            .and_then(serde_json::Value::as_u64)
            .unwrap()
    }

    #[cfg(feature = "remote-client")]
    async fn start_test_cache_server() -> TestCacheServer {
        let tempdir = tempfile::tempdir().unwrap();
        let cache_root = tempdir.path().join("cache");
        std::fs::create_dir_all(&cache_root).unwrap();

        let cache_db = CacheDb::open_or_create(&cache_root.join(CACHE_DB_FILE)).unwrap();
        let cache_store = Arc::new(
            CacheStore::open(
                cache_root.clone(),
                TEST_MAX_CACHE_BYTES,
                cache_db.connect().unwrap(),
            )
            .unwrap(),
        );
        let chunk_index = ChunkIndex::open(cache_db.connect().unwrap()).unwrap();

        let origin_store = Arc::new(InMemory::new());
        let origin_get_count = Arc::new(AtomicUsize::new(0));
        let counting_store: Arc<dyn ObjectStore> = Arc::new(CountingStore {
            inner: Arc::clone(&origin_store),
            get_count: Arc::clone(&origin_get_count),
        });
        let origin = OriginClient::from_store(counting_store);

        let psk_hash = blake3::hash(TEST_PSK.as_bytes());
        let config = CacheServerConfig {
            listen_addr: "127.0.0.1:0".parse().unwrap(),
            tls: None,
            auth: AuthConfig::Psk {
                key_hash: *psk_hash.as_bytes(),
            },
            origin_url: "memory://".to_string(),
            cache_root,
            max_cache_bytes: TEST_MAX_CACHE_BYTES,
            dedup_scope: DedupScope::All,
            drain_timeout: Duration::from_secs(1),
            mutable_path_mode: MutablePathMode::Strict,
            high_water_ratio: 0.95,
            low_water_ratio: 0.90,
            policy_path: None,
        };

        let evictor_handle = start_evictor_task(
            Arc::clone(&cache_store),
            config.high_water_ratio,
            config.low_water_ratio,
            Duration::from_secs(60),
        );
        let evictor_notify = evictor_handle.notify_handle();
        let state = Arc::new(AppState {
            cache_store,
            chunk_index,
            origin,
            config,
            metrics: CacheMetrics::stub(),
            policy: None,
            evictor_notify,
            origin_healthy: AtomicBool::new(true),
            origin_health_checked_at: tokio::sync::Mutex::new(Instant::now()),
            cache_miss_locks: std::sync::Mutex::new(std::collections::HashMap::new()),
            push_warming_body_permits: tokio::sync::Semaphore::new(8),
            dedup_index_rebuild: DedupIndexRebuildStats {
                status: "not_run".to_string(),
                entries: 0,
                error: None,
            },
            dedup_last_ingestion_error: tokio::sync::RwLock::new(None),
        });

        let router = build_router(state);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .unwrap();
            evictor_handle.shutdown().await;
        });

        tokio::time::sleep(Duration::from_millis(20)).await;

        TestCacheServer {
            addr,
            origin: origin_store,
            origin_get_count,
            shutdown: Some(shutdown_tx),
            _tempdir: tempdir,
        }
    }

    #[cfg(feature = "remote-client")]
    async fn start_malformed_dedup_server() -> MalformedDedupServer {
        let router = axum::Router::new().route(
            "/v1/dedup/query",
            axum::routing::post(|| async {
                axum::Json(serde_json::json!({
                    "known": [{
                        "index": 99,
                        "xorb_hash": MerkleHash::from([7u8; 32]).hex(),
                        "chunk_index": 0,
                        "length": 1,
                        "cache_verified": true,
                    }],
                    "unknown": []
                }))
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .unwrap();
        });

        MalformedDedupServer {
            addr,
            shutdown: Some(shutdown_tx),
        }
    }

    #[cfg(feature = "remote-client")]
    async fn start_batched_dedup_server(fail_request: Option<usize>) -> BatchedDedupServer {
        use axum::response::IntoResponse;

        let request_sizes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sizes = Arc::clone(&request_sizes);
        let request_count = Arc::new(AtomicUsize::new(0));
        let router = axum::Router::new()
            .route(
                "/v1/dedup/query",
                axum::routing::post(move |axum::Json(body): axum::Json<serde_json::Value>| {
                let sizes = Arc::clone(&sizes);
                let request_count = Arc::clone(&request_count);
                async move {
                    let request_index = request_count.fetch_add(1, Ordering::SeqCst);
                    let count = body["chunk_hashes"].as_array().map_or(0, Vec::len);
                    sizes
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .push(count);
                    if fail_request == Some(request_index) {
                        return axum::http::StatusCode::INTERNAL_SERVER_ERROR.into_response();
                    }
                    let unknown = (1..count).collect::<Vec<_>>();
                    axum::Json(serde_json::json!({
                        "known": if count == 0 { Vec::<serde_json::Value>::new() } else {
                            vec![serde_json::json!({
                                "index": 0,
                                "xorb_hash": MerkleHash::from([(request_index + 1) as u8; 32]).hex(),
                                "chunk_index": request_index,
                                "length": 4096,
                                "cache_verified": true,
                            })]
                        },
                        "unknown": unknown,
                    }))
                    .into_response()
                }
                }),
            )
            .layer(axum::extract::DefaultBodyLimit::max(8 * 1024 * 1024));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .unwrap();
        });
        BatchedDedupServer {
            addr,
            request_sizes,
            shutdown: Some(shutdown_tx),
        }
    }

    #[cfg(feature = "remote-client")]
    fn unique_hash(index: usize) -> [u8; 32] {
        let mut hash = [0u8; 32];
        hash[..8].copy_from_slice(&(index as u64).to_le_bytes());
        hash
    }

    #[cfg(feature = "remote-client")]
    async fn start_malformed_object_server(
        body: &'static [u8],
        stream_body: bool,
    ) -> MalformedObjectServer {
        let router = axum::Router::new().route(
            "/v1/{*path}",
            axum::routing::get(move || async move {
                let response_body = if stream_body {
                    axum::body::Body::from_stream(futures_util::stream::iter([Ok::<
                        _,
                        std::convert::Infallible,
                    >(
                        Bytes::from_static(body),
                    )]))
                } else {
                    axum::body::Body::from(body)
                };
                axum::http::Response::new(response_body)
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

        tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .unwrap();
        });

        MalformedObjectServer {
            addr,
            shutdown: Some(shutdown_tx),
        }
    }

    #[tokio::test]
    async fn get_with_etag_delegates_to_origin_when_no_cache() {
        let origin = origin_store();
        let path = Path::from(".crab/xorbs/abc123");
        let body = Bytes::from_static(b"xorb data");
        origin.put(&path, body.clone()).await.unwrap();

        let cs = CachingStore::new(origin, no_cache_config()).unwrap();
        let (got, _etag) = cs.get_with_etag(&path).await.unwrap();
        assert_eq!(got, body);
    }

    #[cfg(not(feature = "remote-client"))]
    #[tokio::test]
    async fn configured_cache_service_requires_remote_client_feature() {
        let mut config = no_cache_config();
        config.service_url = Some("http://127.0.0.1:1".to_string());

        let err = match CachingStore::new(origin_store(), &config) {
            Ok(_) => panic!("cache service URL must require the remote-client feature"),
            Err(err) => err,
        };

        assert!(matches!(
            err,
            CacheStoreError::Cache(CacheError::Service { reason })
                if reason.contains("remote-client")
        ));
    }

    #[cfg(feature = "remote-client")]
    #[tokio::test]
    async fn get_with_etag_uses_cache_service_and_reuses_server_cache() {
        let server = start_test_cache_server().await;
        let (body, hash_hex) = test_xorb(b"crab-side cache service read path");
        let path = Path::from(format!(".crab/xorbs/{}/{hash_hex}", &hash_hex[..2]));
        server
            .origin
            .put(&path, PutPayload::from_bytes(body.clone()))
            .await
            .unwrap();

        let config = cache_service_config(server.addr);
        let direct_origin = Store::new(Arc::new(InMemory::new()));
        let client_cache_a = Arc::new(LocalCache::new(server._tempdir.path().join("client-a")));
        let client_cache_b = Arc::new(LocalCache::new(server._tempdir.path().join("client-b")));

        let first = CachingStore::new_with_local_cache(
            direct_origin.clone(),
            &config,
            Arc::clone(&client_cache_a),
        )
        .unwrap();
        let (got, _etag) = first.get_with_etag(&path).await.unwrap();
        assert_eq!(got, body);
        assert_eq!(server.origin_get_count.load(Ordering::Relaxed), 1);

        let second =
            CachingStore::new_with_local_cache(direct_origin, &config, client_cache_b).unwrap();
        let (got, _etag) = second.get_with_etag(&path).await.unwrap();
        assert_eq!(got, body);
        assert_eq!(
            server.origin_get_count.load(Ordering::Relaxed),
            1,
            "fresh Crab-side client should hit cache server without origin GET"
        );
    }

    #[cfg(feature = "remote-client")]
    #[tokio::test]
    async fn download_to_path_uses_cache_service_for_pack_bodies() {
        let server = start_test_cache_server().await;
        let body = Bytes::from_static(b"pack body served from cache");
        let pack_id = blake3::hash(&body).to_hex().to_string();
        let path = Path::from(format!("org/repo/packs/pack-{pack_id}.pack"));
        server
            .origin
            .put(&path, PutPayload::from_bytes(body.clone()))
            .await
            .unwrap();

        let config = cache_service_config(server.addr);
        let direct_origin = Store::new(Arc::new(InMemory::new()));
        let first = CachingStore::new_with_local_cache(
            direct_origin.clone(),
            &config,
            Arc::new(LocalCache::new(
                server._tempdir.path().join("pack-client-a"),
            )),
        )
        .unwrap();
        let first_dest = server._tempdir.path().join("first.pack");
        let first_bytes = first.download_to_path(&path, &first_dest).await.unwrap();

        assert_eq!(first_bytes, body.len() as u64);
        assert_eq!(tokio::fs::read(&first_dest).await.unwrap(), body);
        assert_eq!(server.origin_get_count.load(Ordering::Relaxed), 1);

        let second = CachingStore::new_with_local_cache(
            direct_origin,
            &config,
            Arc::new(LocalCache::new(
                server._tempdir.path().join("pack-client-b"),
            )),
        )
        .unwrap();
        let second_dest = server._tempdir.path().join("second.pack");
        let second_bytes = second.download_to_path(&path, &second_dest).await.unwrap();

        assert_eq!(second_bytes, body.len() as u64);
        assert_eq!(tokio::fs::read(&second_dest).await.unwrap(), body);
        assert_eq!(
            server.origin_get_count.load(Ordering::Relaxed),
            1,
            "a fresh Crab client should stream the warm pack from the cache service"
        );
    }

    #[cfg(feature = "remote-client")]
    #[tokio::test]
    async fn cache_aware_object_store_streams_pack_bodies() {
        let server = start_test_cache_server().await;
        let body = Bytes::from_static(b"pack body streamed through object store");
        let pack_id = blake3::hash(&body).to_hex().to_string();
        let path = Path::from(format!("org/repo/packs/pack-{pack_id}.pack"));
        server
            .origin
            .put(&path, PutPayload::from_bytes(body.clone()))
            .await
            .unwrap();

        let config = cache_service_config(server.addr);
        let first = CachingStore::new_with_local_cache(
            Store::new(Arc::new(InMemory::new())),
            &config,
            Arc::new(LocalCache::new(
                server._tempdir.path().join("object-store-client-a"),
            )),
        )
        .unwrap();
        let first_result = first.object_store().get(&path).await.unwrap();
        assert_eq!(first_result.meta.size, body.len() as u64);
        assert_eq!(first_result.bytes().await.unwrap(), body);
        assert_eq!(server.origin_get_count.load(Ordering::Relaxed), 1);

        let second = CachingStore::new_with_local_cache(
            Store::new(Arc::new(InMemory::new())),
            &config,
            Arc::new(LocalCache::new(
                server._tempdir.path().join("object-store-client-b"),
            )),
        )
        .unwrap();
        let second_result = second.object_store().get(&path).await.unwrap();
        assert_eq!(second_result.meta.size, body.len() as u64);
        assert_eq!(second_result.bytes().await.unwrap(), body);
        assert_eq!(
            server.origin_get_count.load(Ordering::Relaxed),
            1,
            "object-store consumers should receive warm pack streams without origin reads"
        );
    }

    #[cfg(feature = "remote-client")]
    #[tokio::test]
    async fn head_cache_service_object_reports_warm_hit_without_origin_get() {
        let server = start_test_cache_server().await;
        let (body, hash_hex) = test_xorb(b"crab-side cache service head path");
        let path = content_path("xorbs", &hash_hex);
        server
            .origin
            .put(&path, PutPayload::from_bytes(body.clone()))
            .await
            .unwrap();

        let config = cache_service_config(server.addr);
        let direct_origin = Store::new(Arc::new(InMemory::new()));
        let warmer = CachingStore::new_with_local_cache(
            direct_origin.clone(),
            &config,
            Arc::new(LocalCache::new(server._tempdir.path().join("head-warmer"))),
        )
        .unwrap();
        let (got, _etag) = warmer.get_with_etag(&path).await.unwrap();
        assert_eq!(got, body);
        assert_eq!(server.origin_get_count.load(Ordering::Relaxed), 1);

        let reader = CachingStore::new_with_local_cache(
            direct_origin,
            &config,
            Arc::new(LocalCache::new(server._tempdir.path().join("head-reader"))),
        )
        .unwrap();
        let head = reader
            .head_cache_service_object(&path)
            .await
            .unwrap()
            .expect("cache service head enabled");

        assert_eq!(head.size, body.len() as u64);
        assert_eq!(head.cache_status.as_deref(), Some("HIT"));
        assert_eq!(server.origin_get_count.load(Ordering::Relaxed), 1);
    }

    #[cfg(feature = "remote-client")]
    #[tokio::test]
    async fn object_store_head_uses_cache_service_head_without_origin_get() {
        let server = start_test_cache_server().await;
        let (body, hash_hex) = test_xorb(b"object-store head should not fetch body");
        let path = content_path("xorbs", &hash_hex);
        server
            .origin
            .put(&path, PutPayload::from_bytes(body.clone()))
            .await
            .unwrap();

        let config = cache_service_config(server.addr);
        let client_origin = Store::new(Arc::new(InMemory::new()));
        let cache_store = CachingStore::new_with_local_cache(
            client_origin,
            &config,
            Arc::new(LocalCache::new(
                server._tempdir.path().join("object-store-head-client"),
            )),
        )
        .unwrap();
        let object_store = cache_store.object_store();

        let meta = object_store.head(&path).await.unwrap();
        assert_eq!(meta.size, body.len() as u64);
        assert_eq!(
            server.origin_get_count.load(Ordering::Relaxed),
            0,
            "cold object-store HEAD should use cache-service HEAD/origin HEAD, not GET"
        );

        let head_result = object_store
            .get_opts(
                &path,
                GetOptions {
                    head: true,
                    ..GetOptions::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(head_result.meta.size, body.len() as u64);
        assert_eq!(head_result.range, 0..0);
        assert_eq!(
            server.origin_get_count.load(Ordering::Relaxed),
            0,
            "GetOptions::head must not fetch immutable object bytes"
        );
    }

    #[cfg(feature = "remote-client")]
    #[test]
    fn cache_service_capabilities_require_current_route_contract() {
        let valid = CacheServiceCapabilities {
            limits: crab_cache::CacheServiceLimits {
                max_cache_bytes: 4096,
                max_object_bytes: 8192,
            },
            routes: Some(crab_cache::path_class::cache_route_contract()),
        };
        assert!(cache_service_capabilities_route_contract_current(&valid));

        let mut missing = valid.clone();
        missing.routes = None;
        assert!(!cache_service_capabilities_route_contract_current(&missing));

        let mut drifted = valid.clone();
        drifted.routes.as_mut().unwrap().immutable.pop();
        assert!(!cache_service_capabilities_route_contract_current(&drifted));
    }

    #[cfg(feature = "remote-client")]
    #[tokio::test]
    async fn cache_service_route_contract_separates_mutable_immutable_and_dedup() {
        let server = start_test_cache_server().await;
        let config = cache_service_config(server.addr);
        let origin = origin_store();
        let mutable_path = Path::from("org/repo/refs/heads/main");
        let mutable_body = Bytes::from_static(b"ref: refs/heads/main\n");
        origin
            .put(&mutable_path, mutable_body.clone())
            .await
            .unwrap();
        let store = CachingStore::new_with_local_cache(
            origin,
            &config,
            Arc::new(LocalCache::new(
                server._tempdir.path().join("route-contract-client"),
            )),
        )
        .unwrap();
        let before = cache_server_admin_stats(&server).await;
        let read_rejections_before = traffic_value(&before, "mutable_read_rejections");
        let dedup_queries_before = traffic_value(&before, "dedup_queries");

        let (got, _etag) = store.get_with_etag(&mutable_path).await.unwrap();
        assert_eq!(got, mutable_body);
        let got = store.range_get(&mutable_path, 0..4).await.unwrap();
        assert_eq!(got.as_ref(), b"ref:");
        let object_store = store.object_store();
        let got = object_store
            .get_opts(&mutable_path, GetOptions::default())
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert_eq!(got, mutable_body);
        let meta = object_store.head(&mutable_path).await.unwrap();
        assert_eq!(meta.size, mutable_body.len() as u64);

        let after_mutable = cache_server_admin_stats(&server).await;
        assert_eq!(
            traffic_value(&after_mutable, "mutable_read_rejections"),
            read_rejections_before,
            "mutable repository reads must not hit the strict cache-service route"
        );
        assert_eq!(server.origin_get_count.load(Ordering::Relaxed), 0);

        let (immutable_body, hash_hex) = test_xorb(b"immutable route contract body");
        let immutable_path = content_path("xorbs", &hash_hex);
        server
            .origin
            .put(
                &immutable_path,
                PutPayload::from_bytes(immutable_body.clone()),
            )
            .await
            .unwrap();
        let (got, _etag) = store.get_with_etag(&immutable_path).await.unwrap();
        assert_eq!(got, immutable_body);
        assert_eq!(
            server.origin_get_count.load(Ordering::Relaxed),
            1,
            "immutable read should use the cache service and let it own the origin fill"
        );

        let dedup = store.dedup_query("org/repo", &[[7u8; 32]]).await.unwrap();
        assert!(dedup.known.is_empty());
        assert_eq!(dedup.unknown, vec![0]);
        let after_dedup = cache_server_admin_stats(&server).await;
        assert_eq!(
            traffic_value(&after_dedup, "dedup_queries"),
            dedup_queries_before + 1,
            "dedup remains routed to the cache service"
        );
    }

    #[cfg(feature = "remote-client")]
    #[tokio::test]
    async fn cache_service_object_helpers_ignore_mutable_paths() {
        let server = start_test_cache_server().await;
        let config = cache_service_push_warming_config(server.addr);
        let store = CachingStore::new_with_local_cache(
            origin_store(),
            &config,
            Arc::new(LocalCache::new(
                server._tempdir.path().join("helper-route-contract-client"),
            )),
        )
        .unwrap();
        let path = Path::from("org/repo/manifest");
        let before = cache_server_admin_stats(&server).await;
        let read_rejections_before = traffic_value(&before, "mutable_read_rejections");
        let write_rejections_before = traffic_value(&before, "mutable_write_rejections");

        assert!(
            store
                .get_cache_service_object(&path)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .head_cache_service_object(&path)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .range_get_cache_service_object(&path, 0..1)
                .await
                .unwrap()
                .is_none()
        );
        store
            .warm_remote_only(&path, Bytes::from_static(b"manifest"))
            .await
            .unwrap();

        let after = cache_server_admin_stats(&server).await;
        assert_eq!(
            traffic_value(&after, "mutable_read_rejections"),
            read_rejections_before
        );
        assert_eq!(
            traffic_value(&after, "mutable_write_rejections"),
            write_rejections_before
        );
        assert_eq!(server.origin_get_count.load(Ordering::Relaxed), 0);
    }

    #[cfg(feature = "remote-client")]
    #[tokio::test]
    async fn object_store_suffix_range_resolves_length_without_full_get() {
        use axum::{
            Router,
            body::Body,
            extract::Path as AxumPath,
            http::{HeaderMap, HeaderValue, StatusCode, header},
            response::Response,
            routing::get,
        };

        let (body, hash_hex) = test_xorb(b"object-store suffix range should use head plus range");
        let path = content_path("xorbs", &hash_hex).to_string();
        let object_path = Path::from(path.clone());
        let body = Arc::new(body);
        let head_count = Arc::new(AtomicUsize::new(0));
        let range_get_count = Arc::new(AtomicUsize::new(0));
        let full_get_count = Arc::new(AtomicUsize::new(0));

        let app = {
            let get_body = Arc::clone(&body);
            let get_path = path.clone();
            let range_get_count = Arc::clone(&range_get_count);
            let full_get_count = Arc::clone(&full_get_count);
            let head_body = Arc::clone(&body);
            let head_path = path.clone();
            let head_count = Arc::clone(&head_count);
            Router::new().route(
                "/v1/{*path}",
                get(
                    move |AxumPath(request_path): AxumPath<String>, headers: HeaderMap| {
                        let body = Arc::clone(&get_body);
                        let path = get_path.clone();
                        let range_get_count = Arc::clone(&range_get_count);
                        let full_get_count = Arc::clone(&full_get_count);
                        async move {
                            if request_path != path {
                                let mut response = Response::new(Body::empty());
                                *response.status_mut() = StatusCode::NOT_FOUND;
                                return response;
                            }

                            let Some(range_header) = headers.get(header::RANGE) else {
                                full_get_count.fetch_add(1, Ordering::Relaxed);
                                let mut response = Response::new(Body::from(body.as_ref().clone()));
                                response.headers_mut().insert(
                                    header::CONTENT_LENGTH,
                                    HeaderValue::from_str(&body.len().to_string()).unwrap(),
                                );
                                response
                                    .headers_mut()
                                    .insert("x-cache", HeaderValue::from_static("HIT"));
                                return response;
                            };

                            range_get_count.fetch_add(1, Ordering::Relaxed);
                            let range = range_header.to_str().unwrap();
                            let range = range.strip_prefix("bytes=").unwrap();
                            let (start, end) = range.split_once('-').unwrap();
                            let start = start.parse::<usize>().unwrap();
                            let end_inclusive = end.parse::<usize>().unwrap();
                            if start >= body.len() {
                                let mut response = Response::new(Body::empty());
                                *response.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
                                response.headers_mut().insert(
                                    header::CONTENT_RANGE,
                                    HeaderValue::from_str(&format!("bytes */{}", body.len()))
                                        .unwrap(),
                                );
                                return response;
                            }
                            let end = (end_inclusive + 1).min(body.len());
                            let slice = body.slice(start..end);
                            let mut response = Response::new(Body::from(slice.clone()));
                            *response.status_mut() = StatusCode::PARTIAL_CONTENT;
                            response.headers_mut().insert(
                                header::CONTENT_LENGTH,
                                HeaderValue::from_str(&slice.len().to_string()).unwrap(),
                            );
                            response.headers_mut().insert(
                                header::CONTENT_RANGE,
                                HeaderValue::from_str(&format!(
                                    "bytes {}-{}/{}",
                                    start,
                                    end - 1,
                                    body.len()
                                ))
                                .unwrap(),
                            );
                            response
                                .headers_mut()
                                .insert("x-cache", HeaderValue::from_static("HIT"));
                            response
                        }
                    },
                )
                .head(move |AxumPath(request_path): AxumPath<String>| {
                    let body = Arc::clone(&head_body);
                    let path = head_path.clone();
                    let head_count = Arc::clone(&head_count);
                    async move {
                        if request_path != path {
                            let mut response = Response::new(Body::empty());
                            *response.status_mut() = StatusCode::NOT_FOUND;
                            return response;
                        }

                        head_count.fetch_add(1, Ordering::Relaxed);
                        let mut response = Response::new(Body::empty());
                        response.headers_mut().insert(
                            header::CONTENT_LENGTH,
                            HeaderValue::from_str(&body.len().to_string()).unwrap(),
                        );
                        response
                            .headers_mut()
                            .insert("x-cache", HeaderValue::from_static("HIT"));
                        response
                    }
                }),
            )
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .unwrap();
        });

        let config = CacheConfig {
            max_bytes: DEFAULT_LOCAL_CACHE_MAX_BYTES,
            service_url: Some(format!("http://{addr}")),
            service_mode: CacheServiceMode::CacheAndDedup,
            push_warming: false,
            service_auth: CacheServiceAuth::None,
            service_ca_cert: None,
            service_client_cert: None,
            service_client_key: None,
        };
        let cache_store = CachingStore::new(Store::new(Arc::new(InMemory::new())), &config)
            .expect("caching store");
        let object_store = cache_store.object_store();
        let suffix_len = 9u64;

        let result = object_store
            .get_opts(
                &object_path,
                GetOptions {
                    range: Some(GetRange::Suffix(suffix_len)),
                    ..GetOptions::default()
                },
            )
            .await
            .unwrap();
        let returned_range = result.range.clone();
        let got = result.bytes().await.unwrap();
        let start = body.len() - suffix_len as usize;

        assert_eq!(got, body.slice(start..body.len()));
        assert_eq!(returned_range, start as u64..body.len() as u64);
        assert_eq!(head_count.load(Ordering::Relaxed), 1);
        assert_eq!(range_get_count.load(Ordering::Relaxed), 1);
        assert_eq!(
            full_get_count.load(Ordering::Relaxed),
            0,
            "suffix range length resolution must not full-GET the object"
        );

        let _ = shutdown_tx.send(());
        server.await.unwrap();
    }

    #[cfg(feature = "remote-client")]
    #[tokio::test]
    async fn object_store_bounded_range_clamps_to_object_size() {
        use axum::{
            Router,
            body::Body,
            extract::Path as AxumPath,
            http::{HeaderMap, HeaderValue, StatusCode, header},
            response::Response,
            routing::get,
        };

        let (body, hash_hex) = test_xorb(b"object-store bounded range clamps past eof");
        let path = content_path("xorbs", &hash_hex).to_string();
        let object_path = Path::from(path.clone());
        let body = Arc::new(body);
        let head_count = Arc::new(AtomicUsize::new(0));
        let range_get_count = Arc::new(AtomicUsize::new(0));
        let full_get_count = Arc::new(AtomicUsize::new(0));
        let observed_ranges = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));

        let app = {
            let get_body = Arc::clone(&body);
            let get_path = path.clone();
            let range_get_count = Arc::clone(&range_get_count);
            let full_get_count = Arc::clone(&full_get_count);
            let observed_ranges = Arc::clone(&observed_ranges);
            let head_body = Arc::clone(&body);
            let head_path = path.clone();
            let head_count = Arc::clone(&head_count);
            Router::new().route(
                "/v1/{*path}",
                get(
                    move |AxumPath(request_path): AxumPath<String>, headers: HeaderMap| {
                        let body = Arc::clone(&get_body);
                        let path = get_path.clone();
                        let range_get_count = Arc::clone(&range_get_count);
                        let full_get_count = Arc::clone(&full_get_count);
                        let observed_ranges = Arc::clone(&observed_ranges);
                        async move {
                            if request_path != path {
                                let mut response = Response::new(Body::empty());
                                *response.status_mut() = StatusCode::NOT_FOUND;
                                return response;
                            }

                            let Some(range_header) = headers.get(header::RANGE) else {
                                full_get_count.fetch_add(1, Ordering::Relaxed);
                                let mut response = Response::new(Body::from(body.as_ref().clone()));
                                response.headers_mut().insert(
                                    header::CONTENT_LENGTH,
                                    HeaderValue::from_str(&body.len().to_string()).unwrap(),
                                );
                                response
                                    .headers_mut()
                                    .insert("x-cache", HeaderValue::from_static("HIT"));
                                return response;
                            };

                            range_get_count.fetch_add(1, Ordering::Relaxed);
                            let range = range_header.to_str().unwrap().to_owned();
                            observed_ranges.lock().unwrap().push(range.clone());
                            let range = range.strip_prefix("bytes=").unwrap();
                            let (start, end) = range.split_once('-').unwrap();
                            let start = start.parse::<usize>().unwrap();
                            let end_inclusive = end.parse::<usize>().unwrap();
                            if start >= body.len() {
                                let mut response = Response::new(Body::empty());
                                *response.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
                                response.headers_mut().insert(
                                    header::CONTENT_RANGE,
                                    HeaderValue::from_str(&format!("bytes */{}", body.len()))
                                        .unwrap(),
                                );
                                return response;
                            }
                            let end = (end_inclusive + 1).min(body.len());
                            let slice = body.slice(start..end);
                            let mut response = Response::new(Body::from(slice.clone()));
                            *response.status_mut() = StatusCode::PARTIAL_CONTENT;
                            response.headers_mut().insert(
                                header::CONTENT_LENGTH,
                                HeaderValue::from_str(&slice.len().to_string()).unwrap(),
                            );
                            response.headers_mut().insert(
                                header::CONTENT_RANGE,
                                HeaderValue::from_str(&format!(
                                    "bytes {}-{}/{}",
                                    start,
                                    end - 1,
                                    body.len()
                                ))
                                .unwrap(),
                            );
                            response
                                .headers_mut()
                                .insert("x-cache", HeaderValue::from_static("HIT"));
                            response
                        }
                    },
                )
                .head(move |AxumPath(request_path): AxumPath<String>| {
                    let body = Arc::clone(&head_body);
                    let path = head_path.clone();
                    let head_count = Arc::clone(&head_count);
                    async move {
                        if request_path != path {
                            let mut response = Response::new(Body::empty());
                            *response.status_mut() = StatusCode::NOT_FOUND;
                            return response;
                        }

                        head_count.fetch_add(1, Ordering::Relaxed);
                        let mut response = Response::new(Body::empty());
                        response.headers_mut().insert(
                            header::CONTENT_LENGTH,
                            HeaderValue::from_str(&body.len().to_string()).unwrap(),
                        );
                        response
                            .headers_mut()
                            .insert("x-cache", HeaderValue::from_static("HIT"));
                        response
                    }
                }),
            )
        };
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .unwrap();
        });

        let config = CacheConfig {
            max_bytes: DEFAULT_LOCAL_CACHE_MAX_BYTES,
            service_url: Some(format!("http://{addr}")),
            service_mode: CacheServiceMode::CacheAndDedup,
            push_warming: false,
            service_auth: CacheServiceAuth::None,
            service_ca_cert: None,
            service_client_cert: None,
            service_client_key: None,
        };
        let cache_store = CachingStore::new(Store::new(Arc::new(InMemory::new())), &config)
            .expect("caching store");
        let object_store = cache_store.object_store();
        let start = 7u64;

        let result = object_store
            .get_opts(
                &object_path,
                GetOptions {
                    range: Some(GetRange::Bounded(start..body.len() as u64 + 1024)),
                    ..GetOptions::default()
                },
            )
            .await
            .unwrap();
        let returned_range = result.range.clone();
        let got = result.bytes().await.unwrap();

        assert_eq!(got, body.slice(start as usize..body.len()));
        assert_eq!(returned_range, start..body.len() as u64);
        assert_eq!(head_count.load(Ordering::Relaxed), 0);
        assert_eq!(range_get_count.load(Ordering::Relaxed), 1);
        assert_eq!(full_get_count.load(Ordering::Relaxed), 0);
        assert_eq!(
            observed_ranges.lock().unwrap().as_slice(),
            &[format!("bytes={}-{}", start, body.len() + 1023)]
        );

        let _ = shutdown_tx.send(());
        server.await.unwrap();
    }

    #[cfg(feature = "remote-client")]
    #[tokio::test]
    async fn bounded_read_bypasses_oversized_cache_service_and_still_verifies_origin() {
        let good = Bytes::from_static(b"valid");
        let hash = crab_xet::hash::compute_data_hash(&good);
        let key = CacheKey::Shard(hash);
        let path = content_path("shards", &hash.hex());

        for stream_body in [false, true] {
            let server =
                start_malformed_object_server(b"oversized cache service body", stream_body).await;
            // Storage retries its own bounded-size corruption once; body-hash
            // validation runs above storage and must not add an origin retry.
            for (name, origin_body, origin_gets) in [
                ("healthy", good.clone(), 1),
                ("wrong-hash", Bytes::from_static(b"wrong"), 1),
                ("oversized", Bytes::from_static(b"oversized origin body"), 2),
            ] {
                let inner = Arc::new(InMemory::new());
                inner
                    .put(&path, PutPayload::from_bytes(origin_body.clone()))
                    .await
                    .unwrap();
                let counting = Arc::new(CountingObjectStore::new(inner));
                let origin = Store::new(Arc::clone(&counting) as Arc<dyn ObjectStore>);
                let tempdir = tempfile::tempdir().unwrap();
                let cache = Arc::new(LocalCache::new(tempdir.path().join("cache")));
                let store = CachingStore::new_with_local_cache(
                    origin,
                    cache_service_config(server.addr),
                    Arc::clone(&cache),
                )
                .unwrap();

                let result = store.get_with_etag_bounded(&path, good.len() as u64).await;

                assert_eq!(
                    counting.counts().full,
                    origin_gets,
                    "{name}: origin requests"
                );
                if origin_body == good {
                    assert_eq!(result.unwrap().0, good);
                    assert!(cache.contains_verified(&key).await);
                } else {
                    assert!(result.is_err(), "{name}: invalid origin must fail");
                    assert!(!cache.contains(&key).await, "{name}: no invalid cache fill");
                }
            }
        }
    }

    #[cfg(feature = "remote-client")]
    #[tokio::test]
    async fn get_with_etag_rejects_bad_hash_verified_cache_service_body() {
        let server = start_malformed_object_server(b"bad shard body", false).await;
        let good_body = Bytes::from_static(b"correct shard body");
        let hash = crab_xet::hash::compute_data_hash(&good_body);
        let path = content_path("shards", &hash.hex());
        let origin = origin_store();
        origin.put(&path, good_body.clone()).await.unwrap();
        let tempdir = tempfile::tempdir().unwrap();
        let cache = Arc::new(LocalCache::new(tempdir.path().join("client-cache")));
        let store = CachingStore::new_with_local_cache(
            origin,
            cache_service_config(server.addr),
            Arc::clone(&cache),
        )
        .unwrap();

        let (got, _) = store.get_with_etag(&path).await.unwrap();

        assert_eq!(got, good_body);
        let cached = cache
            .get_or_fetch(&CacheKey::Shard(hash), || async {
                panic!("verified origin fallback should populate local cache")
            })
            .await
            .unwrap();
        assert_eq!(cached, good_body);
    }

    #[cfg(feature = "remote-client")]
    #[tokio::test]
    async fn get_with_etag_rejects_bad_xorb_cache_service_body() {
        let server = start_malformed_object_server(b"bad xorb body", false).await;
        let (good_body, hash_hex) = test_xorb(b"correct xorb body");
        let hash = MerkleHash::from_hex(&hash_hex).unwrap();
        let path = content_path("xorbs", &hash_hex);
        let origin = origin_store();
        origin.put(&path, good_body.clone()).await.unwrap();
        let tempdir = tempfile::tempdir().unwrap();
        let cache = Arc::new(LocalCache::new(tempdir.path().join("client-cache")));
        let store = CachingStore::new_with_local_cache(
            origin,
            cache_service_config(server.addr),
            Arc::clone(&cache),
        )
        .unwrap();

        let (got, _) = store.get_with_etag(&path).await.unwrap();

        assert_eq!(got, good_body);
        let cached = cache
            .get_or_fetch(&CacheKey::Xorb(hash), || async {
                panic!("verified origin fallback should populate local cache")
            })
            .await
            .unwrap();
        assert_eq!(cached, good_body);
    }

    #[cfg(feature = "remote-client")]
    #[tokio::test]
    async fn range_get_uses_cache_service_when_server_cache_is_warm() {
        let server = start_test_cache_server().await;
        let (body, hash_hex) = test_xorb(b"0123456789abcdefghijklmnopqrstuvwxyz");
        let path = content_path("xorbs", &hash_hex);
        server
            .origin
            .put(&path, PutPayload::from_bytes(body.clone()))
            .await
            .unwrap();

        let config = cache_service_config(server.addr);
        let direct_origin = Store::new(Arc::new(InMemory::new()));
        let warm_cache = Arc::new(LocalCache::new(server._tempdir.path().join("warm-client")));
        let range_cache = Arc::new(LocalCache::new(server._tempdir.path().join("range-client")));

        let warmer = CachingStore::new_with_local_cache(
            direct_origin.clone(),
            &config,
            Arc::clone(&warm_cache),
        )
        .unwrap();
        let (got, _etag) = warmer.get_with_etag(&path).await.unwrap();
        assert_eq!(got, body);
        assert_eq!(server.origin_get_count.load(Ordering::Relaxed), 1);

        let range_reader =
            CachingStore::new_with_local_cache(direct_origin, &config, range_cache).unwrap();
        let slice = range_reader.range_get(&path, 10..20).await.unwrap();
        assert_eq!(slice, body.slice(10..20));
        assert_eq!(
            server.origin_get_count.load(Ordering::Relaxed),
            1,
            "fresh Crab-side range reader should hit cache server without origin GET"
        );
    }

    #[cfg(feature = "remote-client")]
    #[tokio::test]
    async fn cold_range_get_uses_cache_service_instead_of_client_origin() {
        let server = start_test_cache_server().await;
        let (body, hash_hex) = test_xorb(b"cache server owns cold range origin fetches");
        let path = content_path("xorbs", &hash_hex);
        server
            .origin
            .put(&path, PutPayload::from_bytes(body.clone()))
            .await
            .unwrap();

        let config = cache_service_config(server.addr);
        let empty_client_origin = Store::new(Arc::new(InMemory::new()));
        let client_cache = Arc::new(LocalCache::new(
            server._tempdir.path().join("cold-range-client"),
        ));
        let cache_store =
            CachingStore::new_with_local_cache(empty_client_origin, &config, client_cache).unwrap();

        let slice = cache_store.range_get(&path, 7..19).await.unwrap();
        assert_eq!(slice, body.slice(7..19));
        assert_eq!(
            server.origin_get_count.load(Ordering::Relaxed),
            1,
            "cache server should fetch origin once on cold range miss"
        );
    }

    #[cfg(feature = "remote-client")]
    #[tokio::test]
    async fn object_store_put_warms_versioned_metadb_metadata() {
        let server = start_test_cache_server().await;
        let config = cache_service_push_warming_config(server.addr);
        let writer_origin: Arc<dyn ObjectStore> = server.origin.clone();
        let writer = CachingStore::new_with_local_cache(
            Store::new(writer_origin),
            &config,
            Arc::new(LocalCache::new(
                server._tempdir.path().join("metadata-writer"),
            )),
        )
        .unwrap();

        let cases = [
            (
                "file-index-manifest",
                "org/repo/file_index_db/manifest/00000000000000000009.manifest",
            ),
            (
                "file-index-wal",
                "org/repo/file_index_db/wal/00000000000000000003.sst",
            ),
            (
                "chunk-index-compacted",
                ".crab/chunk_index_db/compacted/01KVK1S7EDDF30QW005R4TY0R4.sst",
            ),
            (
                "chunk-index-compactions",
                ".crab/chunk_index_db/compactions/00000000000000000004.compactions",
            ),
        ];
        for (name, path) in cases {
            let path = Path::from(path);
            let body = Bytes::from(format!("slatedb metadata bytes: {name}"));

            writer
                .object_store()
                .put_opts(
                    &path,
                    PutPayload::from_bytes(body.clone()),
                    PutOptions::default(),
                )
                .await
                .unwrap();

            let reader = CachingStore::new_with_local_cache(
                Store::new(Arc::new(InMemory::new())),
                &config,
                Arc::new(LocalCache::new(
                    server
                        ._tempdir
                        .path()
                        .join(format!("metadata-reader-{name}")),
                )),
            )
            .unwrap();
            let got = reader
                .object_store()
                .get(&path)
                .await
                .unwrap()
                .bytes()
                .await
                .unwrap();

            assert_eq!(got, body);
        }
        assert_eq!(
            server.origin_get_count.load(Ordering::Relaxed),
            0,
            "fresh metadata reader should hit cache server without origin GET"
        );
    }

    #[tokio::test]
    async fn range_get_delegates_to_origin_when_no_cache() {
        let origin = origin_store();
        let path = Path::from(".crab/xorbs/abc123");
        let body = Bytes::from_static(b"0123456789");
        origin.put(&path, body).await.unwrap();

        let cs = CachingStore::new(origin, no_cache_config()).unwrap();
        let slice = cs.range_get(&path, 2..7).await.unwrap();
        assert_eq!(slice.as_ref(), b"23456");
    }

    #[tokio::test]
    async fn selective_xorb_read_avoids_full_cache_install() {
        let payloads = [
            Bytes::from(vec![0x11; 32 * 1024]),
            Bytes::from(vec![0x22; 32 * 1024]),
            Bytes::from(vec![0x33; 32 * 1024]),
            Bytes::from(vec![0x44; 32 * 1024]),
        ];
        let (xorb, hash) = test_raw_xorb(&payloads);
        let path = content_path("xorbs", &hash.hex());
        let origin = origin_store();
        origin.put(&path, xorb).await.unwrap();
        let tempdir = tempfile::tempdir().unwrap();
        let cache = Arc::new(LocalCache::new(tempdir.path().join("cache")));
        let store =
            CachingStore::new_with_local_cache(origin, no_cache_config(), Arc::clone(&cache))
                .unwrap();

        let (data, offsets) = store
            .get_xorb_chunks(&path, &hash, &[(1, 2)])
            .await
            .unwrap();

        assert_eq!(data, payloads[1]);
        assert_eq!(offsets, vec![0, payloads[1].len() as u32]);
        assert!(!cache.contains(&CacheKey::Xorb(hash)).await);
    }

    #[tokio::test]
    async fn high_coverage_xorb_read_installs_complete_verified_xorb() {
        let payloads = [
            Bytes::from(vec![0x11; 32 * 1024]),
            Bytes::from(vec![0x22; 32 * 1024]),
            Bytes::from(vec![0x33; 32 * 1024]),
            Bytes::from(vec![0x44; 32 * 1024]),
        ];
        let expected = payloads
            .iter()
            .flat_map(|payload| payload.iter().copied())
            .collect::<Vec<_>>();
        let (xorb, hash) = test_raw_xorb(&payloads);
        let chunk_hash = XorbParser::parse(xorb.clone())
            .unwrap()
            .chunk_meta(0)
            .unwrap()
            .hash;
        let path = content_path("xorbs", &hash.hex());
        let origin = origin_store();
        origin.put(&path, xorb).await.unwrap();
        let tempdir = tempfile::tempdir().unwrap();
        let cache = Arc::new(LocalCache::new(tempdir.path().join("cache")));
        let store =
            CachingStore::new_with_local_cache(origin, no_cache_config(), Arc::clone(&cache))
                .unwrap();

        let (data, offsets) = store
            .get_xorb_chunks(&path, &hash, &[(0, 4)])
            .await
            .unwrap();

        assert_eq!(data, expected);
        assert_eq!(
            offsets,
            vec![0, 32 * 1024, 64 * 1024, 96 * 1024, 128 * 1024]
        );
        assert!(cache.contains_verified(&CacheKey::Xorb(hash)).await);
        assert!(
            cache
                .cached_xorb_candidates_for_chunks(&[chunk_hash])
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn noninstalling_full_coverage_uses_one_origin_get() {
        let payloads = [
            Bytes::from(vec![0x11; 32 * 1024]),
            Bytes::from(vec![0x22; 32 * 1024]),
            Bytes::from(vec![0x33; 32 * 1024]),
            Bytes::from(vec![0x44; 32 * 1024]),
        ];
        let expected = payloads
            .iter()
            .flat_map(|payload| payload.iter().copied())
            .collect::<Vec<_>>();
        let (xorb, hash) = test_raw_xorb(&payloads);
        let path = content_path("xorbs", &hash.hex());
        let inner = Arc::new(InMemory::new());
        inner
            .put(&path, PutPayload::from_bytes(xorb))
            .await
            .unwrap();
        let counting_origin = Arc::new(CountingObjectStore::new(inner));
        let origin = Store::new(Arc::clone(&counting_origin) as Arc<dyn ObjectStore>);
        let tempdir = tempfile::tempdir().unwrap();
        let cache = Arc::new(LocalCache::new(tempdir.path().join("cache")));
        let store =
            CachingStore::new_with_local_cache(origin, no_cache_config(), Arc::clone(&cache))
                .unwrap();

        let (data, offsets) = store
            .get_xorb_chunks_without_install(&path, &hash, &[(0, 4)])
            .await
            .unwrap();

        assert_eq!(data, expected);
        assert_eq!(offsets.last().copied(), Some(128 * 1024));
        assert!(
            !cache.contains(&CacheKey::Xorb(hash)).await,
            "hydrate reads must not write a second full copy before output"
        );
        assert_eq!(
            counting_origin.counts(),
            ObjectReadCounts {
                heads: 0,
                ranges: 0,
                full: 1,
            }
        );
        assert_eq!(
            counting_origin.requests(),
            vec![crab_storage::test_support::ObjectReadRequest {
                location: path.to_string(),
                kind: ObjectReadKind::Full,
            }]
        );
    }

    #[cfg(feature = "remote-client")]
    #[tokio::test]
    async fn noninstalling_full_coverage_uses_warmed_cache_service() {
        let payloads = [
            Bytes::from(vec![0x11; 32 * 1024]),
            Bytes::from(vec![0x22; 32 * 1024]),
        ];
        let expected = payloads
            .iter()
            .flat_map(|payload| payload.iter().copied())
            .collect::<Vec<_>>();
        let (xorb, hash) = test_raw_xorb(&payloads);
        let path = content_path("xorbs", &hash.hex());
        let server = start_test_cache_server().await;
        server
            .origin
            .put(&path, PutPayload::from_bytes(xorb.clone()))
            .await
            .unwrap();

        let warmer = CachingStore::new_with_local_cache(
            Store::new(Arc::clone(&server.origin) as Arc<dyn ObjectStore>),
            cache_service_push_warming_config(server.addr),
            Arc::new(LocalCache::new(
                server._tempdir.path().join("noninstalling-warmer"),
            )),
        )
        .unwrap();
        warmer.warm_remote_only(&path, xorb).await.unwrap();

        let reader_cache = Arc::new(LocalCache::new(
            server._tempdir.path().join("noninstalling-reader"),
        ));
        let reader = CachingStore::new_with_local_cache(
            Store::new(Arc::clone(&server.origin) as Arc<dyn ObjectStore>),
            cache_service_config(server.addr),
            Arc::clone(&reader_cache),
        )
        .unwrap();
        let (data, offsets) = reader
            .get_xorb_chunks_without_install(&path, &hash, &[(0, 2)])
            .await
            .unwrap();

        assert_eq!(data, expected);
        assert_eq!(offsets.last().copied(), Some(expected.len() as u32));
        assert!(!reader_cache.contains(&CacheKey::Xorb(hash)).await);
        assert_eq!(server.origin_get_count.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn corrupt_origin_xorb_fails_once_with_origin_provenance() {
        use crab_xet::xorb::format::FOOTER_SIZE;
        use std::error::Error as _;

        let payloads = [
            Bytes::from(vec![0x11; 32 * 1024]),
            Bytes::from(vec![0x22; 32 * 1024]),
            Bytes::from(vec![0x33; 32 * 1024]),
            Bytes::from(vec![0x44; 32 * 1024]),
        ];
        let (xorb, hash) = test_raw_xorb(&payloads);
        let path = content_path("xorbs", &hash.hex());
        let footer_start = xorb.len() - FOOTER_SIZE;
        let metadata_start = u64::from_le_bytes(
            xorb[footer_start + 4..footer_start + 12]
                .try_into()
                .unwrap(),
        ) as usize;

        for corruption in ["footer", "metadata", "payload", "truncated"] {
            for mode in ["noninstalling", "full", "selective", "metadata"] {
                if mode == "metadata" && corruption == "payload" {
                    continue; // Metadata reads deliberately do not fetch payload.
                }
                let mut corrupt = xorb.to_vec();
                match corruption {
                    "footer" => *corrupt.last_mut().unwrap() ^= 0xff,
                    "metadata" => corrupt[metadata_start] ^= 0xff,
                    "payload" => corrupt[32 * 1024] ^= 0xff,
                    "truncated" => {
                        corrupt.pop();
                    }
                    _ => unreachable!(),
                }
                let inner = Arc::new(InMemory::new());
                inner
                    .put(&path, PutPayload::from_bytes(Bytes::from(corrupt)))
                    .await
                    .unwrap();
                let counting = Arc::new(CountingObjectStore::new(inner));
                let origin = Store::new(Arc::clone(&counting) as Arc<dyn ObjectStore>);
                let tempdir = tempfile::tempdir().unwrap();
                let cache = Arc::new(LocalCache::new(tempdir.path().join("cache")));
                let store = CachingStore::new_with_local_cache(
                    origin,
                    no_cache_config(),
                    Arc::clone(&cache),
                )
                .unwrap();
                let error = match mode {
                    "noninstalling" => store
                        .get_xorb_chunks_without_install(&path, &hash, &[(0, 4)])
                        .await
                        .unwrap_err(),
                    "full" => store
                        .get_xorb_chunks(&path, &hash, &[(0, 4)])
                        .await
                        .unwrap_err(),
                    "selective" => store
                        .get_xorb_chunks(&path, &hash, &[(1, 2)])
                        .await
                        .unwrap_err(),
                    "metadata" => store.xorb_chunk_metadata(&path, &hash).await.unwrap_err(),
                    _ => unreachable!(),
                };
                assert!(
                    matches!(&error, CacheStoreError::OriginIntegrity { path: failed_path, .. } if failed_path == path.as_ref()),
                    "{mode}/{corruption}: {error:?}"
                );
                assert!(error.source().unwrap().is::<CacheError>());
                assert!(!cache.contains(&CacheKey::Xorb(hash)).await);
                let expected = if mode == "noninstalling" {
                    ObjectReadCounts {
                        heads: 0,
                        ranges: 0,
                        full: 1,
                    }
                } else {
                    ObjectReadCounts {
                        heads: 1,
                        ranges: if matches!(corruption, "footer" | "truncated") {
                            1
                        } else if mode == "selective" && corruption == "payload" {
                            3
                        } else {
                            2
                        },
                        full: usize::from(mode == "full" && corruption == "payload"),
                    }
                };
                assert_eq!(
                    counting.counts(),
                    expected,
                    "{mode}/{corruption}: no cache-repair retry of origin"
                );
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn repair_preserves_read_policy_when_cache_eviction_fails() {
        use std::os::unix::fs::PermissionsExt as _;

        let payloads = [
            Bytes::from(vec![0x11; 32 * 1024]),
            Bytes::from(vec![0x22; 32 * 1024]),
            Bytes::from(vec![0x33; 32 * 1024]),
            Bytes::from(vec![0x44; 32 * 1024]),
        ];
        let expected = payloads
            .iter()
            .flat_map(|data| data.iter().copied())
            .collect::<Vec<_>>();
        let (xorb, hash) = test_raw_xorb(&payloads);
        let path = content_path("xorbs", &hash.hex());
        for removal_allowed in [true, false] {
            for mode in ["noninstalling", "full", "selective", "metadata"] {
                let inner = Arc::new(InMemory::new());
                inner
                    .put(&path, PutPayload::from_bytes(xorb.clone()))
                    .await
                    .unwrap();
                let counting = Arc::new(CountingObjectStore::new(inner));
                let origin = Store::new(Arc::clone(&counting) as Arc<dyn ObjectStore>);
                let tempdir = tempfile::tempdir().unwrap();
                let root = tempdir.path().join("cache");
                let cache = Arc::new(LocalCache::new(root.clone()));
                let mut corrupt = xorb.to_vec();
                if mode == "metadata" {
                    *corrupt.last_mut().unwrap() ^= 0xff;
                } else {
                    corrupt[32 * 1024] ^= 0xff;
                }
                cache
                    .put_unchecked_for_test(&CacheKey::Xorb(hash), &corrupt)
                    .await
                    .unwrap();
                let entry = root.join("xorbs").join(&hash.hex()[..2]).join(hash.hex());
                let parent = entry.parent().unwrap();
                if !removal_allowed {
                    std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o500))
                        .unwrap();
                }
                let store = CachingStore::new_with_local_cache(
                    origin,
                    no_cache_config(),
                    Arc::clone(&cache),
                )
                .unwrap();
                let result = match mode {
                    "noninstalling" => store
                        .get_xorb_chunks_without_install(&path, &hash, &[(0, 4)])
                        .await
                        .map(|(data, _)| data == expected),
                    "full" => store
                        .get_xorb_chunks(&path, &hash, &[(0, 4)])
                        .await
                        .map(|(data, _)| data == expected),
                    "selective" => store
                        .get_xorb_chunks(&path, &hash, &[(1, 2)])
                        .await
                        .map(|(data, _)| data == payloads[1]),
                    "metadata" => store
                        .xorb_chunk_metadata(&path, &hash)
                        .await
                        .map(|chunks| chunks.len() == 4 && chunks[1].uncompressed_len == 32 * 1024),
                    _ => unreachable!(),
                };
                std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).unwrap();
                assert!(result.unwrap(), "{mode}: repaired bytes/metadata");
                let expected_reads = match mode {
                    "noninstalling" => ObjectReadCounts {
                        heads: 0,
                        ranges: 0,
                        full: 1,
                    },
                    "full" => ObjectReadCounts {
                        heads: 1,
                        ranges: 2,
                        full: 1,
                    },
                    "selective" => ObjectReadCounts {
                        heads: 1,
                        ranges: 3,
                        full: 0,
                    },
                    "metadata" => ObjectReadCounts {
                        heads: 1,
                        ranges: 2,
                        full: 0,
                    },
                    _ => unreachable!(),
                };
                assert_eq!(
                    counting.counts(),
                    expected_reads,
                    "{mode}: origin request shape"
                );
                if !removal_allowed {
                    assert_eq!(
                        std::fs::read(entry).unwrap(),
                        corrupt,
                        "{mode}: bypass unwritable cache"
                    );
                } else if mode == "full" {
                    assert!(cache.contains_verified(&CacheKey::Xorb(hash)).await);
                } else {
                    assert!(!entry.exists(), "{mode}: do not install a full xorb");
                }
            }
        }
    }

    #[tokio::test]
    async fn invalid_chunk_ranges_leave_verified_local_xorb_intact() {
        let payload = Bytes::from(vec![0x71; 32 * 1024]);
        let (xorb, hash) = test_raw_xorb(std::slice::from_ref(&payload));
        let path = content_path("xorbs", &hash.hex());
        let tempdir = tempfile::tempdir().unwrap();
        let cache = Arc::new(LocalCache::new(tempdir.path().join("cache")));
        cache.put_read_xorb(&hash, xorb).await.unwrap();
        let counting = Arc::new(CountingObjectStore::new(Arc::new(InMemory::new())));
        let store = CachingStore::new_with_local_cache(
            Store::new(Arc::clone(&counting) as Arc<dyn ObjectStore>),
            no_cache_config(),
            Arc::clone(&cache),
        )
        .unwrap();

        for range in [(0, 2), (1, 0)] {
            for install in [false, true] {
                let result = if install {
                    store.get_xorb_chunks(&path, &hash, &[range]).await
                } else {
                    store
                        .get_xorb_chunks_without_install(&path, &hash, &[range])
                        .await
                };
                assert!(matches!(
                    result,
                    Err(CacheStoreError::Cache(CacheError::ChunkNotFound { .. }))
                ));
                assert!(cache.contains_verified(&CacheKey::Xorb(hash)).await);
            }
        }
        assert_eq!(counting.counts(), ObjectReadCounts::default());
    }

    #[cfg(feature = "remote-client")]
    #[tokio::test]
    async fn corrupt_cache_service_xorb_does_not_poison_origin_or_install_policy() {
        let server = start_malformed_object_server(b"bad xorb body", false).await;
        let payload = Bytes::from(vec![0x71; 32 * 1024]);
        let (xorb, hash) = test_raw_xorb(std::slice::from_ref(&payload));
        let path = content_path("xorbs", &hash.hex());
        for corrupt_origin in [false, true] {
            let origin_body = if corrupt_origin {
                Bytes::from_static(b"bad origin")
            } else {
                xorb.clone()
            };
            let inner = Arc::new(InMemory::new());
            inner
                .put(&path, PutPayload::from_bytes(origin_body))
                .await
                .unwrap();
            let counting = Arc::new(CountingObjectStore::new(inner));
            let tempdir = tempfile::tempdir().unwrap();
            let cache = Arc::new(LocalCache::new(tempdir.path().join("cache")));
            let store = CachingStore::new_with_local_cache(
                Store::new(Arc::clone(&counting) as Arc<dyn ObjectStore>),
                cache_service_config(server.addr),
                Arc::clone(&cache),
            )
            .unwrap();
            let result = store
                .get_xorb_chunks_without_install(&path, &hash, &[(0, 1)])
                .await;
            if corrupt_origin {
                assert!(matches!(
                    result,
                    Err(CacheStoreError::OriginIntegrity { .. })
                ));
            } else {
                assert_eq!(result.unwrap().0, payload);
            }
            assert_eq!(
                counting.counts(),
                ObjectReadCounts {
                    heads: 0,
                    ranges: 0,
                    full: 1
                }
            );
            assert!(!cache.contains(&CacheKey::Xorb(hash)).await);
        }
    }

    #[tokio::test]
    async fn warm_sparse_result_owns_only_the_requested_bytes() {
        let (xorb, hash) = test_raw_xorb(&[
            Bytes::from(vec![1; 4 * 1024 * 1024]),
            Bytes::from_static(b"xyz"),
        ]);
        let path = content_path("xorbs", &hash.hex());
        let inner = Arc::new(InMemory::new());
        inner
            .put(&path, PutPayload::from_bytes(xorb))
            .await
            .unwrap();
        let counting = Arc::new(CountingObjectStore::new(inner));
        let origin = Store::new(Arc::clone(&counting) as Arc<dyn ObjectStore>);
        let tmp = tempfile::tempdir().unwrap();
        let local = Arc::new(LocalCache::new(tmp.path().join("cache")));
        let store =
            CachingStore::new_with_local_cache(origin, no_cache_config(), local.clone()).unwrap();

        let cold = store
            .get_xorb_chunks_without_install(&path, &hash, &[(1, 2)])
            .await
            .unwrap();
        let before = counting.counts();
        let warm = store
            .get_xorb_chunks_without_install(&path, &hash, &[(1, 2)])
            .await
            .unwrap();

        assert_eq!(warm, (Bytes::from_static(b"xyz"), vec![0, 3]));
        assert_eq!(cold, warm);
        assert_ne!(cold.0.as_ptr(), warm.0.as_ptr());
        assert_eq!(counting.counts(), before);
        assert!(!local.contains(&CacheKey::Xorb(hash)).await);
    }

    #[tokio::test]
    async fn retained_results_preserve_range_order_and_multiplicity() {
        let (xorb, hash) = test_raw_xorb(&[Bytes::from_static(b"ab"), Bytes::from_static(b"cde")]);
        let path = content_path("xorbs", &hash.hex());
        let origin = origin_store();
        origin.put(&path, xorb).await.unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let local = Arc::new(LocalCache::new(tmp.path().join("cache")));
        let store = CachingStore::new_with_local_cache(origin, no_cache_config(), local).unwrap();
        let ranges = [(1, 2), (0, 2), (1, 2)];

        for _ in 0..2 {
            let result = store
                .get_xorb_chunks_without_install(&path, &hash, &ranges)
                .await
                .unwrap();
            assert_eq!(
                result,
                (Bytes::from_static(b"cdeabcdecde"), vec![0, 3, 5, 8, 11])
            );
        }
    }

    #[tokio::test]
    async fn concurrent_selective_xorb_reads_share_one_origin_fetch() {
        let payloads = [
            Bytes::from(vec![0x11; 32 * 1024]),
            Bytes::from(vec![0x22; 32 * 1024]),
            Bytes::from(vec![0x33; 32 * 1024]),
            Bytes::from(vec![0x44; 32 * 1024]),
        ];
        let (xorb, hash) = test_raw_xorb(&payloads);
        let path = content_path("xorbs", &hash.hex());
        let inner = Arc::new(InMemory::new());
        inner
            .put(&path, PutPayload::from_bytes(xorb))
            .await
            .unwrap();
        let get_count = Arc::new(AtomicUsize::new(0));
        let origin = Store::new(Arc::new(CountingStore {
            inner,
            get_count: Arc::clone(&get_count),
        }));
        let tempdir = tempfile::tempdir().unwrap();
        let cache = Arc::new(LocalCache::new(tempdir.path().join("cache")));
        let store =
            CachingStore::new_with_local_cache(origin, no_cache_config(), Arc::clone(&cache))
                .unwrap();

        let (left, right) = tokio::join!(
            store.get_xorb_chunks(&path, &hash, &[(1, 2)]),
            store.get_xorb_chunks(&path, &hash, &[(1, 2)]),
        );

        assert_eq!(left.unwrap().0, payloads[1]);
        assert_eq!(right.unwrap().0, payloads[1]);
        assert_eq!(
            get_count.load(Ordering::Relaxed),
            3,
            "one footer, metadata, and payload range fetch should serve both readers"
        );
    }

    #[tokio::test]
    async fn warm_local_xorb_read_does_not_require_origin_metadata() {
        let payloads = [
            Bytes::from(vec![0x11; 32 * 1024]),
            Bytes::from(vec![0x22; 32 * 1024]),
            Bytes::from(vec![0x33; 32 * 1024]),
            Bytes::from(vec![0x44; 32 * 1024]),
        ];
        let expected = payloads
            .iter()
            .flat_map(|payload| payload.iter().copied())
            .collect::<Vec<_>>();
        let (xorb, hash) = test_raw_xorb(&payloads);
        let path = content_path("xorbs", &hash.hex());
        let origin = origin_store();
        origin.put(&path, xorb).await.unwrap();
        let tempdir = tempfile::tempdir().unwrap();
        let cache = Arc::new(LocalCache::new(tempdir.path().join("cache")));
        let warmer =
            CachingStore::new_with_local_cache(origin, no_cache_config(), Arc::clone(&cache))
                .unwrap();
        warmer
            .get_xorb_chunks(&path, &hash, &[(0, 4)])
            .await
            .unwrap();
        let warm_reader = CachingStore::new_with_local_cache(
            origin_store(),
            no_cache_config(),
            Arc::clone(&cache),
        )
        .unwrap();

        let (data, offsets) = warm_reader
            .get_xorb_chunks(&path, &hash, &[(0, 4)])
            .await
            .unwrap();

        assert_eq!(data, expected);
        assert_eq!(offsets.last().copied(), Some(128 * 1024));
    }

    #[tokio::test]
    async fn put_writes_to_origin_when_no_cache() {
        let origin = origin_store();
        let cs = CachingStore::new(origin, no_cache_config()).unwrap();

        let path = Path::from(".crab/xorbs/def456");
        let body = Bytes::from_static(b"new xorb");
        cs.put(&path, body.clone()).await.unwrap();

        let (got, _) = cs.get_with_etag(&path).await.unwrap();
        assert_eq!(got, body);
    }

    #[tokio::test]
    async fn put_rejects_bad_content_addressed_xorb_before_origin_write() {
        let origin = origin_store();
        let origin_probe = origin.clone();
        let cs = CachingStore::new(origin, no_cache_config()).unwrap();
        let (_good_body, hash_hex) = test_xorb(b"expected xorb body");
        let path = Path::from(format!(".crab/xorbs/{}/{hash_hex}", &hash_hex[..2]));

        let err = cs
            .put(&path, Bytes::from_static(b"not a serialized xorb"))
            .await
            .unwrap_err();

        assert!(matches!(
            err,
            CacheStoreError::Cache(
                CacheError::CorruptObject { .. } | CacheError::HashMismatch { .. }
            )
        ));
        assert!(matches!(
            origin_probe.head(&path).await,
            Err(StorageError::NotFound { .. })
        ));
    }

    #[tokio::test]
    async fn dedup_query_returns_all_unknown_when_no_cache() {
        let origin = origin_store();
        let cs = CachingStore::new(origin, no_cache_config()).unwrap();

        let hashes = vec![[0u8; 32], [1u8; 32]];
        let result = cs.dedup_query("org/repo", &hashes).await.unwrap();
        assert!(result.known.is_empty());
        assert_eq!(result.unknown, vec![0, 1]);
    }

    #[cfg(feature = "remote-client")]
    #[tokio::test]
    async fn dedup_query_caps_unique_hashes_per_request() {
        let mut server = start_batched_dedup_server(None).await;
        let config = CacheConfig {
            max_bytes: DEFAULT_LOCAL_CACHE_MAX_BYTES,
            service_url: Some(format!("http://{}", server.addr)),
            service_mode: CacheServiceMode::Dedup,
            push_warming: false,
            service_auth: CacheServiceAuth::None,
            service_ca_cert: None,
            service_client_cert: None,
            service_client_key: None,
        };
        let store = CachingStore::new(origin_store(), &config).unwrap();
        let hashes = (0..150_001).map(unique_hash).collect::<Vec<_>>();

        store.dedup_query("org/repo", &hashes).await.unwrap();

        let sizes = server
            .request_sizes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        assert_eq!(sizes, vec![50_000, 50_000, 50_000, 1]);
        if let Some(shutdown) = server.shutdown.take() {
            let _ = shutdown.send(());
        }
    }

    #[cfg(feature = "remote-client")]
    #[tokio::test]
    async fn dedup_query_preserves_successful_batches_and_duplicate_order() {
        let mut server = start_batched_dedup_server(Some(1)).await;
        let config = CacheConfig {
            max_bytes: DEFAULT_LOCAL_CACHE_MAX_BYTES,
            service_url: Some(format!("http://{}", server.addr)),
            service_mode: CacheServiceMode::Dedup,
            push_warming: false,
            service_auth: CacheServiceAuth::None,
            service_ca_cert: None,
            service_client_cert: None,
            service_client_key: None,
        };
        let store = CachingStore::new(origin_store(), &config).unwrap();
        let mut hashes = (0..120_001).map(unique_hash).collect::<Vec<_>>();
        hashes.push(hashes[0]);
        hashes.push(hashes[50_000]);

        let result = store.dedup_query("org/repo", &hashes).await.unwrap();

        assert_eq!(
            result
                .known
                .iter()
                .map(|known| known.index)
                .collect::<Vec<_>>(),
            vec![0, 100_000, 120_001]
        );
        assert!(result.unknown.contains(&50_000));
        assert!(result.unknown.contains(&120_002));
        if let Some(shutdown) = server.shutdown.take() {
            let _ = shutdown.send(());
        }
    }

    #[tokio::test]
    async fn head_always_goes_to_origin() {
        let origin = origin_store();
        let path = Path::from("repo/refs/heads/main");
        let body = Bytes::from_static(b"ref data");
        origin.put(&path, body.clone()).await.unwrap();

        let cs = CachingStore::new(origin, no_cache_config()).unwrap();
        let meta = cs.head(&path).await.unwrap();
        assert_eq!(meta.size, body.len() as u64);
    }

    #[tokio::test]
    async fn delete_always_goes_to_origin() {
        let origin = origin_store();
        let path = Path::from(".crab/xorbs/todelete");
        origin.put(&path, Bytes::from_static(b"bye")).await.unwrap();

        let cs = CachingStore::new(origin, no_cache_config()).unwrap();
        cs.delete(&path).await.unwrap();

        let err = cs.head(&path).await;
        assert!(err.is_err());
    }

    #[tokio::test]
    async fn mutable_path_bypasses_cache_even_when_configured() {
        // With no real cache service, mutable paths should still work
        // because they always go direct to origin.
        let origin = origin_store();
        let path = Path::from("repo/refs/heads/main");
        let body = Bytes::from_static(b"ref content");
        origin.put(&path, body.clone()).await.unwrap();

        let cs = CachingStore::new(origin, no_cache_config()).unwrap();
        let (got, _) = cs.get_with_etag(&path).await.unwrap();
        assert_eq!(got, body);
    }

    #[tokio::test]
    async fn query_known_chunks_returns_empty_when_no_cache() {
        let origin = origin_store();
        let cs = CachingStore::new(origin, no_cache_config()).unwrap();

        let hashes = vec![MerkleHash::from([0u8; 32]), MerkleHash::from([1u8; 32])];
        let known = cs.query_known_chunks("org/repo", &hashes).await;
        assert!(known.is_empty());
    }

    #[cfg(feature = "remote-client")]
    #[tokio::test]
    async fn query_known_chunks_ignores_out_of_range_service_indexes() {
        let server = start_malformed_dedup_server().await;
        let config = CacheConfig {
            max_bytes: DEFAULT_LOCAL_CACHE_MAX_BYTES,
            service_url: Some(format!("http://{}", server.addr)),
            service_mode: CacheServiceMode::CacheAndDedup,
            push_warming: false,
            service_auth: CacheServiceAuth::None,
            service_ca_cert: None,
            service_client_cert: None,
            service_client_key: None,
        };
        let cs = CachingStore::new(origin_store(), &config).unwrap();

        let hashes = vec![MerkleHash::from([1u8; 32])];
        let known = cs.query_known_chunks("org/repo", &hashes).await;

        assert!(known.is_empty());
    }

    #[tokio::test]
    async fn warm_remote_only_is_noop_when_no_cache_client() {
        let origin = origin_store();
        let cs = CachingStore::new(origin, no_cache_config()).unwrap();

        let path = Path::from(".crab/xorbs/abc123");
        let body = Bytes::from_static(b"xorb data");
        // Should succeed silently - no client configured.
        cs.warm_remote_only(&path, body).await.unwrap();
    }

    #[cfg(feature = "remote-client")]
    #[tokio::test]
    async fn warm_remote_only_skips_bodies_above_capability_limit() {
        let put_count = Arc::new(AtomicUsize::new(0));
        let put_count_for_route = Arc::clone(&put_count);
        let router = axum::Router::new().route(
            "/v1/{*path}",
            axum::routing::put(move || {
                let put_count = Arc::clone(&put_count_for_route);
                async move {
                    put_count.fetch_add(1, Ordering::Relaxed);
                    axum::http::StatusCode::CREATED
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .unwrap();
        });

        let config = CacheConfig {
            max_bytes: DEFAULT_LOCAL_CACHE_MAX_BYTES,
            service_url: Some(format!("http://{addr}")),
            service_mode: CacheServiceMode::CacheAndDedup,
            push_warming: true,
            service_auth: CacheServiceAuth::None,
            service_ca_cert: None,
            service_client_cert: None,
            service_client_key: None,
        };
        let mut cs = CachingStore::new(origin_store(), &config).unwrap();
        cs.max_push_warming_object_bytes = Some(4);
        let path = content_path("xorbs", &MerkleHash::from([1u8; 32]).hex());

        cs.warm_remote_only(&path, Bytes::from_static(b"12345"))
            .await
            .unwrap();
        assert_eq!(put_count.load(Ordering::Relaxed), 0);

        cs.warm_remote_only(&path, Bytes::from_static(b"1234"))
            .await
            .unwrap();
        assert_eq!(put_count.load(Ordering::Relaxed), 1);

        let _ = shutdown_tx.send(());
    }

    #[test]
    fn slice_cached_range_returns_requested_bytes() {
        let data = Bytes::from_static(b"0123456789");
        let slice = slice_cached_range(&data, &(2..7)).unwrap();
        assert_eq!(slice.as_ref(), b"23456");
    }

    #[test]
    fn slice_cached_range_rejects_out_of_bounds_range() {
        let data = Bytes::from_static(b"0123456789");
        assert!(slice_cached_range(&data, &(8..12)).is_none());
    }

    #[test]
    fn slice_cached_range_rejects_inverted_range() {
        let data = Bytes::from_static(b"0123456789");
        assert!(slice_cached_range(&data, &Range { start: 7, end: 2 }).is_none());
    }
}
