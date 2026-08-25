//! Thin wrapper over [`object_store::ObjectStore`] with CAS-oriented primitives.
//!
//! The wrapper exists so every call site in the crate funnels through one
//! place for:
//!
//! - Error mapping — raw `object_store::Error` values are translated via
//!   [`map_object_store_error`] into richer `StorageError` variants before
//!   reaching the caller, so the retry layer can branch on the classified
//!   variant without parsing error strings.
//! - Retry — each primitive wraps its inner call in [`retry`] using the
//!   policy configured on the `Store`; transient network failures are
//!   absorbed and conflicts get the state-dependent attempt budget.
//! - CAS semantics — `put` uses `PutMode::Create` with content-addressed
//!   idempotency, `create_strict` uses raw `PutMode::Create` for mutable
//!   coordination pointers, and `update` uses `PutMode::Update(etag)`.
//!   Callers never hand-build `PutOptions`.

use std::ops::Range;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;

use bytes::Bytes;
use futures_util::Stream;
use futures_util::StreamExt as _;
use object_store::path::Path;
use object_store::{
    GetOptions, GetRange, MultipartUpload, ObjectMeta, ObjectStore, ObjectStoreExt, PutMode,
    PutOptions,
};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tracing::debug;

use crab_types::storage::StorageScope;

use crate::error::{Result, StorageError};
use crate::error_map::map_object_store_error;
use crate::identity::BucketIdentity;
use crate::retry::{RetryPolicy, retry};

/// Opaque CAS token used by [`Store::update`] and returned by
/// reads/writes so callers can chain compare-and-swap flows.
///
/// Some backends populate `e_tag`, some `version`, and a few both; keep
/// the pair together because `PutMode::Update` consumes both.
pub type ETag = object_store::UpdateVersion;

/// Bounded-memory byte stream returned by object reads.
pub type StorageByteStream = Pin<Box<dyn Stream<Item = Result<Bytes>> + Send + 'static>>;

/// CAS-aware facade over an `object_store::ObjectStore`.
///
/// Cheap to clone: the inner store is held behind `Arc`, the retry
/// policy is `Copy`.
///
#[derive(Clone)]
pub struct Store {
    inner: Arc<dyn ObjectStore>,
    retry: RetryPolicy,
    /// Stable bucket identity used for cross-scheme equality (same-
    /// bucket detection, safety rails). Defaults to
    /// [`BucketIdentity::local_unset`] for wrappers built without an
    /// explicit identity — typically tests and the in-memory store.
    identity: BucketIdentity,
    /// Optional parallel handle to the same underlying store viewed
    /// as a [`object_store::signer::Signer`]. Populated by storage
    /// provider builders for S3 backends (the only backend that
    /// implements `Signer` in `object_store` 0.14); `None` for GCS,
    /// Azure, and the in-memory / test stores. Kept separate from
    /// `inner` because `ObjectStore` does not expose `as_any`, so we
    /// cannot downcast after the fact.
    signer: Option<Arc<dyn object_store::signer::Signer>>,
    /// Optional parallel handle to the same underlying store viewed as
    /// a [`object_store::multipart::MultipartStore`], enabling explicit
    /// upload-id / part-index control for resumable multipart uploads.
    /// Populated by provider builders with stable upload IDs (S3 and GCS)
    /// and explicitly by tests; kept separate from `inner` for the same
    /// downcast limitation as `signer`.
    multipart: Option<Arc<dyn object_store::multipart::MultipartStore>>,
    storage_scope: Option<StorageScope>,
    read_routes: Option<Arc<Vec<ReadRoute>>>,
    staging_writes: Option<Arc<StagingWriteState>>,
    read_byte_observer: Option<Arc<dyn Fn(u64) + Send + Sync>>,
    read_request_observer: Option<Arc<dyn Fn(StorageReadKind) + Send + Sync>>,
}

/// Provider-neutral kind of canonical storage read attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StorageReadKind {
    /// Complete immutable or mutable object body.
    Get,
    /// Exact provider object version.
    GetVersion,
    /// Streaming body, optionally ranged.
    Stream,
    /// Metadata-only request.
    Head,
    /// Bounded byte-range request.
    Range,
}

#[derive(Clone)]
struct ReadRoute {
    prefix: String,
    inner: Arc<dyn ObjectStore>,
}

#[derive(Default)]
struct StagingWriteState {
    prefix: String,
    write_inner: Option<Arc<dyn ObjectStore>>,
    writes: Mutex<Vec<StagedWrite>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StagedWrite {
    pub canonical_key: String,
    pub staged_key: String,
    pub blake3: String,
    pub size: u64,
}

impl Store {
    /// Wraps `inner` with the default retry policy.
    #[must_use]
    pub fn new(inner: Arc<dyn ObjectStore>) -> Self {
        Self {
            inner,
            retry: RetryPolicy::DEFAULT,
            identity: BucketIdentity::local_unset(),
            signer: None,
            multipart: None,
            storage_scope: None,
            read_routes: None,
            staging_writes: None,
            read_byte_observer: None,
            read_request_observer: None,
        }
    }

    /// Wraps `inner` with a custom retry policy.
    ///
    /// Callers that need distinct policies per call site (e.g., tighter
    /// budgets for ref CAS than for blob uploads) construct a separate
    /// `Store` per policy.
    #[must_use]
    pub fn with_retry(inner: Arc<dyn ObjectStore>, retry: RetryPolicy) -> Self {
        Self {
            inner,
            retry,
            identity: BucketIdentity::local_unset(),
            signer: None,
            multipart: None,
            storage_scope: None,
            read_routes: None,
            staging_writes: None,
            read_byte_observer: None,
            read_request_observer: None,
        }
    }

    /// Attaches a [`BucketIdentity`] so [`Store::bucket_identity`]
    /// returns a meaningful value for cross-scheme comparisons.
    ///
    /// Production provider builders set this per cloud. Tests that
    /// don't care leave the default [`BucketIdentity::local_unset`].
    #[must_use]
    pub fn with_bucket_identity(mut self, identity: BucketIdentity) -> Self {
        self.identity = identity;
        self
    }

    /// Attaches a URL-signer handle so [`Self::signed_url`] can
    /// produce presigned URLs.
    ///
    /// The passed `signer` must be the same underlying instance as
    /// `inner` so the credentials that sign match the credentials that
    /// would normally access the bucket. Other backends leave this unset
    /// and `signed_url` returns a descriptive "unsupported" error.
    #[must_use]
    pub fn with_signer(mut self, signer: Arc<dyn object_store::signer::Signer>) -> Self {
        self.signer = Some(signer);
        self
    }

    /// Attaches a low-level multipart handle so
    /// [`Self::put_multipart_file_resumable_retry`] can drive explicit
    /// upload ids and part indexes.
    ///
    /// The passed handle must be the same underlying instance as
    /// `inner`. Provider builders populate this for S3 and GCS; other
    /// backends leave it unset and resumable
    /// uploads fall back to the whole-retry path.
    #[must_use]
    pub fn with_multipart(
        mut self,
        multipart: Arc<dyn object_store::multipart::MultipartStore>,
    ) -> Self {
        self.multipart = Some(multipart);
        self
    }

    /// Low-level multipart handle for the same underlying store, if the
    /// provider supports explicit upload-id control.
    #[must_use]
    pub fn multipart(&self) -> Option<Arc<dyn object_store::multipart::MultipartStore>> {
        self.multipart.clone()
    }

    /// Abort a provider multipart session when explicit upload IDs are available.
    ///
    /// Returns `false` without side effects when the provider cannot satisfy the
    /// explicit multipart contract. A successful provider abort returns `true`.
    pub async fn abort_multipart(&self, path: &Path, upload_id: &str) -> Result<bool> {
        let Some(multipart) = self.multipart() else {
            return Ok(false);
        };
        let result = retry(&self.retry, || {
            let multipart = multipart.clone();
            let path = path.clone();
            let upload_id = object_store::MultipartId::from(upload_id);
            async move {
                multipart
                    .abort_multipart(&path, &upload_id)
                    .await
                    .map_err(|error| map_object_store_error(error, path.as_ref()))
            }
        })
        .await;
        match result {
            Ok(()) | Err(StorageError::NotFound { .. }) => Ok(true),
            Err(error) => Err(error),
        }
    }

    #[must_use]
    pub fn with_storage_scope(mut self, scope: StorageScope) -> Self {
        self.storage_scope = Some(scope);
        self
    }

    #[must_use]
    pub fn storage_scope(&self) -> Option<&StorageScope> {
        self.storage_scope.as_ref()
    }

    /// Routes reads for specific object prefixes through scoped stores.
    ///
    /// Used by protected push credentials when one cloud credential cannot
    /// safely cover every canonical read prefix. The longest prefix wins so
    /// narrow object scopes cannot be shadowed by broader directory scopes.
    #[must_use]
    pub fn with_read_routes(mut self, routes: Vec<(String, Arc<dyn ObjectStore>)>) -> Self {
        let mut routes = routes
            .into_iter()
            .map(|(prefix, inner)| ReadRoute {
                prefix: prefix.trim().trim_matches('/').to_owned(),
                inner,
            })
            .filter(|route| !route.prefix.is_empty())
            .collect::<Vec<_>>();
        routes.sort_by_key(|route| std::cmp::Reverse(route.prefix.len()));
        if !routes.is_empty() {
            self.read_routes = Some(Arc::new(routes));
        }
        self
    }

    #[must_use]
    pub fn with_read_byte_observer(mut self, observer: Arc<dyn Fn(u64) + Send + Sync>) -> Self {
        self.read_byte_observer = Some(observer);
        self
    }

    /// Observe each provider read attempt using a bounded operation kind.
    ///
    /// The callback runs once inside the existing retry loop immediately
    /// before provider I/O. It receives no path, endpoint, or credentials.
    #[must_use]
    pub fn with_read_request_observer(
        mut self,
        observer: Arc<dyn Fn(StorageReadKind) + Send + Sync>,
    ) -> Self {
        self.read_request_observer = Some(observer);
        self
    }

    #[must_use]
    pub fn with_staging_writes(mut self, upload_prefix: String) -> Self {
        let prefix = upload_prefix.trim().trim_matches('/').to_owned();
        self.staging_writes = Some(Arc::new(StagingWriteState {
            prefix,
            write_inner: None,
            writes: Mutex::new(Vec::new()),
        }));
        self
    }

    #[must_use]
    pub fn with_staging_write_store(
        mut self,
        upload_prefix: String,
        write_inner: Arc<dyn ObjectStore>,
    ) -> Self {
        let prefix = upload_prefix.trim().trim_matches('/').to_owned();
        self.staging_writes = Some(Arc::new(StagingWriteState {
            prefix,
            write_inner: Some(write_inner),
            writes: Mutex::new(Vec::new()),
        }));
        self
    }

    #[must_use]
    pub fn staging_write_prefix(&self) -> Option<&str> {
        self.staging_writes
            .as_ref()
            .map(|state| state.prefix.as_str())
    }

    #[must_use]
    pub fn staged_writes(&self) -> Vec<StagedWrite> {
        let Some(state) = &self.staging_writes else {
            return Vec::new();
        };
        match state.writes.lock() {
            Ok(writes) => writes.clone(),
            Err(_) => Vec::new(),
        }
    }

    /// Waits for and confirms every recorded staging write is remotely complete.
    ///
    /// This is the publication barrier for protected pushes: no finalizer
    /// may run until all immutable uploads have completed and their durable
    /// object sizes match the locally recorded writes. Content verification
    /// remains the protected receive service's publication responsibility.
    pub async fn flush_staged_writes(&self, max_concurrency: usize) -> Result<Vec<StagedWrite>> {
        use futures_util::stream;

        let writes = self.staged_writes();
        let concurrency = max_concurrency.max(1);
        let write_inner = self.write_inner();
        stream::iter(writes.iter().cloned())
            .map(|write| {
                let inner = Arc::clone(&write_inner);
                async move {
                    let path = Path::from(write.staged_key.clone());
                    let meta = inner
                        .head(&path)
                        .await
                        .map_err(|error| map_object_store_error(error, path.as_ref()))?;
                    if meta.size != write.size {
                        return Err(StorageError::CorruptObject {
                            path: write.staged_key,
                            reason: format!(
                                "staged object size mismatch: expected {}, found {}",
                                write.size, meta.size
                            ),
                        });
                    }
                    Ok(())
                }
            })
            .buffer_unordered(concurrency)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .collect::<Result<Vec<_>>>()?;
        Ok(writes)
    }

    /// Waits for and confirms one exact staging control object is complete.
    pub async fn flush_staging_object(&self, path: &Path, expected_size: u64) -> Result<()> {
        let inner = self.write_inner();
        let meta = inner
            .head(path)
            .await
            .map_err(|error| map_object_store_error(error, path.as_ref()))?;
        if meta.size != expected_size {
            return Err(StorageError::CorruptObject {
                path: path.to_string(),
                reason: format!(
                    "staging control object size mismatch: expected {expected_size}, found {}",
                    meta.size
                ),
            });
        }
        Ok(())
    }

    /// Returns the store's bucket identity.
    ///
    /// Cheap: clones two short `String`s. Used by same-bucket detection
    /// in the import pipeline and by safety rails that need to compare
    /// two stores without parsing URLs.
    #[must_use]
    pub fn bucket_identity(&self) -> BucketIdentity {
        self.identity.clone()
    }

    /// Borrow the inner object store for operations this wrapper does
    /// not yet cover (e.g., listing, multipart uploads).
    #[must_use]
    pub fn inner(&self) -> &Arc<dyn ObjectStore> {
        &self.inner
    }

    /// Writes `bytes` at `path` iff nothing exists there yet.
    ///
    /// Idempotent on conflict: if the backend reports `CasConflict` but
    /// the object already stored at `path` hashes to the same content we
    /// were about to write, the call succeeds. This matters because the
    /// retry layer may invoke the closure twice — the first attempt's
    /// write could have succeeded server-side before the client saw the
    /// response, and the second attempt then observes `AlreadyExists`
    /// even though the intended state is already on disk.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::StateConflict`] if an object with different
    /// content already occupies `path`; transient errors are surfaced
    /// after the retry budget is exhausted.
    pub async fn put(&self, path: &Path, bytes: Bytes) -> Result<()> {
        let expected_hash = *blake3::hash(&bytes).as_bytes();

        retry(&self.retry, || {
            let path = path.clone();
            let bytes = bytes.clone();
            async move { self.put_once(&path, bytes, &expected_hash).await }
        })
        .await
    }

    /// Writes `bytes` at `path` iff nothing exists there yet.
    ///
    /// Unlike [`Self::put`], this method does not treat same-content
    /// conflicts as success and does not fall back to unconditional PUT when
    /// a backend mishandles conditional create. Use it for mutable
    /// coordination objects whose bytes carry writer identity, such as lock
    /// records and manifest pointers.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::StateConflict`] if any object already occupies
    /// `path`; transient errors are surfaced after the retry budget is
    /// exhausted.
    pub async fn create_strict(&self, path: &Path, bytes: Bytes) -> Result<()> {
        self.create_strict_with_etag(path, bytes).await.map(|_| ())
    }

    /// Writes `bytes` at `path` iff nothing exists there yet, returning
    /// the backend CAS token for callers that will immediately update
    /// the object they just created.
    ///
    /// Use this for mutable coordination objects when the next operation
    /// needs a holder-checked CAS update and an extra read would only
    /// recover the token the backend already returned from create.
    pub async fn create_strict_with_etag(&self, path: &Path, bytes: Bytes) -> Result<ETag> {
        let expected_hash = *blake3::hash(&bytes).as_bytes();
        let size = bytes.len() as u64;

        retry(&self.retry, || {
            let path = path.clone();
            let bytes = bytes.clone();
            async move {
                let write_path = self.write_path(&path);
                let write_inner = self.write_inner();
                let opts = PutOptions::from(PutMode::Create);
                write_inner
                    .put_opts(&write_path, bytes.into(), opts)
                    .await
                    .map(|result| {
                        self.record_staged_write(&path, &write_path, &expected_hash, size);
                        ETag::from(result)
                    })
                    .map_err(|e| map_object_store_error(e, write_path.as_ref()))
            }
        })
        .await
    }

    /// Writes `bytes` at `path`, replacing any existing object.
    ///
    /// This is intentionally separate from [`Self::put`], whose
    /// create-if-absent contract protects Crab metadata and content
    /// objects. Export uses overwrite only when the user passes
    /// `--force`.
    pub async fn put_overwrite(&self, path: &Path, bytes: Bytes) -> Result<()> {
        retry(&self.retry, || {
            let path = path.clone();
            let bytes = bytes.clone();
            async move {
                let (write_path, write_inner, _record_staged_write) =
                    self.exact_write_target(&path);
                write_inner
                    .put_opts(&write_path, bytes.into(), PutOptions::default())
                    .await
                    .map(|_| ())
                    .map_err(|e| map_object_store_error(e, write_path.as_ref()))
            }
        })
        .await
    }

    async fn put_once(&self, path: &Path, bytes: Bytes, expected_hash: &[u8; 32]) -> Result<()> {
        let write_path = self.write_path(path);
        let write_inner = self.write_inner();
        let size = bytes.len() as u64;
        let opts = PutOptions::from(PutMode::Create);
        match write_inner
            .put_opts(&write_path, bytes.clone().into(), opts)
            .await
        {
            Ok(_) => {
                self.record_staged_write(path, &write_path, expected_hash, size);
                Ok(())
            }
            Err(err) => {
                let mapped = map_object_store_error(err, write_path.as_ref());
                if matches!(mapped, StorageError::StateConflict { .. })
                    && self
                        .matches_write_target(&write_inner, &write_path, expected_hash)
                        .await?
                {
                    // Someone (possibly us on a previous retry) wrote the
                    // same content; treat as success.
                    self.record_staged_write(path, &write_path, expected_hash, size);
                    return Ok(());
                }
                // MinIO returns `NoSuchKey` (mapped to `NotFound`) when a
                // conditional PUT (`If-None-Match: *`) targets a key that
                // does not exist. Real S3 would succeed. Fall back to an
                // unconditional PUT since the key genuinely doesn't exist.
                if matches!(mapped, StorageError::NotFound { .. }) {
                    debug!(
                        path = %write_path,
                        "PutMode::Create returned NotFound; \
                         falling back to unconditional PUT \
                         (S3-compatible backend may not support If-None-Match)"
                    );
                    write_inner
                        .put_opts(&write_path, bytes.into(), PutOptions::default())
                        .await
                        .map(|_| ())
                        .map_err(|e| map_object_store_error(e, write_path.as_ref()))?;
                    self.record_staged_write(path, &write_path, expected_hash, size);
                    return Ok(());
                }
                Err(mapped)
            }
        }
    }

    async fn matches_in(
        inner: &Arc<dyn ObjectStore>,
        path: &Path,
        expected: &[u8; 32],
    ) -> Result<bool> {
        let get_result = inner
            .get(path)
            .await
            .map_err(|e| map_object_store_error(e, path.as_ref()))?;
        let body = get_result
            .bytes()
            .await
            .map_err(|e| map_object_store_error(e, path.as_ref()))?;
        Ok(blake3::hash(&body).as_bytes() == expected)
    }

    async fn matches_in_streaming(
        inner: &Arc<dyn ObjectStore>,
        path: &Path,
        expected: &[u8; 32],
    ) -> Result<bool> {
        use futures_util::StreamExt;

        let get_result = inner
            .get(path)
            .await
            .map_err(|e| map_object_store_error(e, path.as_ref()))?;
        let mut stream = get_result.into_stream();
        let mut hasher = blake3::Hasher::new();
        while let Some(chunk) = stream.next().await {
            let bytes = chunk.map_err(|e| map_object_store_error(e, path.as_ref()))?;
            hasher.update(&bytes);
        }
        Ok(hasher.finalize().as_bytes() == expected)
    }

    async fn matches_write_target(
        &self,
        write_inner: &Arc<dyn ObjectStore>,
        path: &Path,
        expected: &[u8; 32],
    ) -> Result<bool> {
        match Self::matches_in(write_inner, path, expected).await {
            Ok(matches) => Ok(matches),
            Err(first_err) if self.read_routes.is_some() => {
                match Self::matches_in(&self.read_inner_for(path), path, expected).await {
                    Ok(matches) => Ok(matches),
                    Err(_) => Err(first_err),
                }
            }
            Err(e) => Err(e),
        }
    }

    /// Writes `bytes` at `path` iff the current version matches `etag`.
    ///
    /// Returns the new CAS token so callers can chain subsequent
    /// compare-and-swap updates without a fresh read.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::StateConflict`] if the stored version no
    /// longer matches `etag`. Unlike other storage primitives, `update`
    /// does **not** retry — a stale etag cannot be rescued by retrying
    /// with the same etag, and transient network errors can also leave
    /// the remote in an ambiguous state (the write may have succeeded
    /// before the client saw an error). Callers that need
    /// read-modify-write semantics should use
    /// the caller's higher-level CAS loop, which should re-read the current
    /// state on each iteration with a fresh etag.
    pub async fn update(&self, path: &Path, bytes: Bytes, etag: ETag) -> Result<ETag> {
        if self.staging_writes.is_some() {
            return Err(StorageError::Internal(
                "staging write store cannot update canonical objects".to_owned(),
            ));
        }
        let opts = PutOptions::from(PutMode::Update(etag));
        match self.inner.put_opts(path, bytes.into(), opts).await {
            Ok(result) => Ok(ETag::from(result)),
            Err(err) => Err(map_object_store_error(err, path.as_ref())),
        }
    }

    pub async fn put_exact(&self, path: &Path, bytes: Bytes) -> Result<()> {
        let expected_hash = *blake3::hash(&bytes).as_bytes();
        retry(&self.retry, || {
            let path = path.clone();
            let bytes = bytes.clone();
            async move {
                let (write_path, write_inner, record_staged_write) = self.exact_write_target(&path);
                let opts = PutOptions::from(PutMode::Create);
                match write_inner
                    .put_opts(&write_path, bytes.clone().into(), opts)
                    .await
                {
                    Ok(_) => {
                        if record_staged_write {
                            self.record_staged_write(
                                &path,
                                &write_path,
                                &expected_hash,
                                bytes.len() as u64,
                            );
                        }
                        Ok(())
                    }
                    Err(err) => {
                        let mapped = map_object_store_error(err, write_path.as_ref());
                        if matches!(mapped, StorageError::StateConflict { .. })
                            && self
                                .matches_write_target(&write_inner, &write_path, &expected_hash)
                                .await?
                        {
                            if record_staged_write {
                                self.record_staged_write(
                                    &path,
                                    &write_path,
                                    &expected_hash,
                                    bytes.len() as u64,
                                );
                            }
                            return Ok(());
                        }
                        Err(mapped)
                    }
                }
            }
        })
        .await
    }

    fn exact_write_target(&self, path: &Path) -> (Path, Arc<dyn ObjectStore>, bool) {
        let Some(state) = &self.staging_writes else {
            return (path.clone(), self.inner.clone(), false);
        };
        if path_is_inside_prefix(path.as_ref(), &state.prefix) {
            return (path.clone(), self.write_inner(), false);
        }
        (self.write_path(path), self.write_inner(), true)
    }

    fn write_path(&self, canonical: &Path) -> Path {
        let Some(state) = &self.staging_writes else {
            return canonical.clone();
        };
        Path::from(format!(
            "{}/objects/{}",
            state.prefix,
            canonical.as_ref().trim_start_matches('/')
        ))
    }

    fn write_inner(&self) -> Arc<dyn ObjectStore> {
        self.staging_writes
            .as_ref()
            .and_then(|state| state.write_inner.clone())
            .unwrap_or_else(|| self.inner.clone())
    }

    fn read_inner_for(&self, path: &Path) -> Arc<dyn ObjectStore> {
        let path = path.as_ref();
        if self
            .staging_writes
            .as_ref()
            .is_some_and(|state| path_is_inside_prefix(path, &state.prefix))
        {
            return self.write_inner();
        }
        if let Some(routes) = &self.read_routes {
            for route in routes.iter() {
                if path_is_inside_prefix(path, &route.prefix) {
                    return route.inner.clone();
                }
            }
        }
        self.inner.clone()
    }

    fn record_read_bytes(&self, bytes: u64) {
        if bytes == 0 {
            return;
        }
        if let Some(observer) = &self.read_byte_observer {
            observer(bytes);
        }
    }

    fn record_read_request(&self, kind: StorageReadKind) {
        if let Some(observer) = &self.read_request_observer {
            observer(kind);
        }
    }

    fn record_staged_write(&self, canonical: &Path, staged: &Path, hash: &[u8; 32], size: u64) {
        let Some(state) = &self.staging_writes else {
            return;
        };
        let entry = StagedWrite {
            canonical_key: canonical.as_ref().to_owned(),
            staged_key: staged.as_ref().to_owned(),
            blake3: hex_lower(hash),
            size,
        };
        if let Ok(mut writes) = state.writes.lock()
            && !writes.iter().any(|existing| existing == &entry)
        {
            writes.push(entry);
        }
    }

    /// Reads `path` and returns the body alongside its CAS token.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::NotFound`] if `path` does not exist.
    pub async fn get_with_etag(&self, path: &Path) -> Result<(Bytes, ETag)> {
        retry(&self.retry, || {
            let path = path.clone();
            async move {
                let read_inner = self.read_inner_for(&path);
                self.record_read_request(StorageReadKind::Get);
                let got = read_inner
                    .get(&path)
                    .await
                    .map_err(|e| map_object_store_error(e, path.as_ref()))?;
                let expected_size = got.meta.size;
                let etag = ETag {
                    e_tag: got.meta.e_tag.clone(),
                    version: got.meta.version.clone(),
                };
                let body = got
                    .bytes()
                    .await
                    .map_err(|e| map_object_store_error(e, path.as_ref()))?;
                ensure_complete_body(&path, expected_size, body.len() as u64)?;
                self.record_read_bytes(body.len() as u64);
                Ok((body, etag))
            }
        })
        .await
    }

    /// Reads `path` into memory only when the provider advertises and
    /// delivers at most `max_bytes`.
    ///
    /// The metadata check happens before the response stream is consumed, so
    /// malformed or unexpectedly large immutable objects fail closed without
    /// allocating their full body. The streamed body is checked again because
    /// provider metadata and response bytes must agree.
    pub async fn get_with_etag_bounded(
        &self,
        path: &Path,
        max_bytes: u64,
    ) -> Result<(Bytes, ETag)> {
        retry(&self.retry, || {
            let path = path.clone();
            async move {
                let read_inner = self.read_inner_for(&path);
                self.record_read_request(StorageReadKind::Get);
                let got = read_inner
                    .get(&path)
                    .await
                    .map_err(|e| map_object_store_error(e, path.as_ref()))?;
                let expected_size = got.meta.size;
                if expected_size > max_bytes {
                    return Err(StorageError::CorruptObject {
                        path: path.to_string(),
                        reason: format!(
                            "object is {expected_size} bytes; bounded read supports at most {max_bytes} bytes"
                        ),
                    });
                }
                let etag = ETag {
                    e_tag: got.meta.e_tag.clone(),
                    version: got.meta.version.clone(),
                };
                let capacity = usize::try_from(expected_size).map_err(|_| {
                    StorageError::Internal(format!(
                        "object size {expected_size} cannot be represented on this platform"
                    ))
                })?;
                let mut body = Vec::new();
                body.try_reserve_exact(capacity).map_err(|error| {
                    StorageError::Internal(format!(
                        "failed to reserve {expected_size} bytes for bounded object read: {error}"
                    ))
                })?;
                let mut stream = got.into_stream();
                while let Some(chunk) = stream.next().await {
                    let chunk = chunk.map_err(|e| map_object_store_error(e, path.as_ref()))?;
                    let next_len = body.len().checked_add(chunk.len()).ok_or_else(|| {
                        StorageError::CorruptObject {
                            path: path.to_string(),
                            reason: "object body length overflowed while reading".to_owned(),
                        }
                    })?;
                    let next_size = u64::try_from(next_len).unwrap_or(u64::MAX);
                    if next_size > max_bytes {
                        return Err(StorageError::CorruptObject {
                            path: path.to_string(),
                            reason: format!(
                                "streamed body exceeds bounded read limit of {max_bytes} bytes"
                            ),
                        });
                    }
                    body.extend_from_slice(&chunk);
                }
                ensure_complete_body(&path, expected_size, body.len() as u64)?;
                self.record_read_bytes(body.len() as u64);
                Ok((Bytes::from(body), etag))
            }
        })
        .await
    }

    /// Reads one exact provider object version and returns its metadata.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::NotFound`] when the path or requested version
    /// does not exist. Provider errors remain classified by the storage
    /// boundary and are retried according to this store's policy.
    pub async fn get_version(&self, path: &Path, version: &str) -> Result<(Bytes, ObjectMeta)> {
        retry(&self.retry, || {
            let path = path.clone();
            let version = version.to_owned();
            async move {
                self.record_read_request(StorageReadKind::GetVersion);
                let got = self
                    .read_inner_for(&path)
                    .get_opts(
                        &path,
                        GetOptions {
                            version: Some(version),
                            ..GetOptions::default()
                        },
                    )
                    .await
                    .map_err(|error| map_object_store_error(error, path.as_ref()))?;
                let meta = got.meta.clone();
                let body = got
                    .bytes()
                    .await
                    .map_err(|error| map_object_store_error(error, path.as_ref()))?;
                ensure_complete_body(&path, meta.size, body.len() as u64)?;
                self.record_read_bytes(body.len() as u64);
                Ok((body, meta))
            }
        })
        .await
    }

    /// Opens an object or one bounded range as a backpressured byte stream.
    ///
    /// Provider errors that occur after the response starts remain classified
    /// as [`StorageError`] values in the stream. Dropping the stream cancels the
    /// provider read.
    pub async fn get_stream(
        &self,
        path: &Path,
        range: Option<Range<u64>>,
    ) -> Result<(ObjectMeta, Range<u64>, StorageByteStream)> {
        let got = retry(&self.retry, || {
            let path = path.clone();
            let range = range.clone();
            async move {
                self.record_read_request(StorageReadKind::Stream);
                self.read_inner_for(&path)
                    .get_opts(
                        &path,
                        GetOptions {
                            range: range.map(GetRange::Bounded),
                            ..GetOptions::default()
                        },
                    )
                    .await
                    .map_err(|error| map_object_store_error(error, path.as_ref()))
            }
        })
        .await?;
        let meta = got.meta.clone();
        let result_range = got.range.clone();
        let path = path.to_string();
        let observer = self.read_byte_observer.clone();
        let stream = got
            .into_stream()
            .map(move |result| match result {
                Ok(bytes) => {
                    if let Some(observer) = &observer
                        && !bytes.is_empty()
                    {
                        observer(bytes.len() as u64);
                    }
                    Ok(bytes)
                }
                Err(error) => Err(map_object_store_error(error, &path)),
            })
            .boxed();
        Ok((meta, result_range, stream))
    }

    /// Stream `path` to a local filesystem destination.
    ///
    /// Unlike [`Self::get_with_etag`], this does not collect the full
    /// object body in memory. It is intended for large immutable blobs
    /// such as git packs that are immediately handed to a file-based
    /// verifier/indexer.
    pub async fn download_to_path(&self, path: &Path, dest: &std::path::Path) -> Result<u64> {
        retry(&self.retry, || {
            let path = path.clone();
            let dest = dest.to_owned();
            async move {
                use futures_util::StreamExt;
                use tokio::io::AsyncWriteExt;

                let _ = tokio::fs::remove_file(&dest).await;
                let read_inner = self.read_inner_for(&path);
                let got = read_inner
                    .get(&path)
                    .await
                    .map_err(|e| map_object_store_error(e, path.as_ref()))?;
                let mut stream = got.into_stream();
                let mut file = tokio::fs::File::create(&dest).await?;
                let mut written = 0u64;

                while let Some(chunk) = stream.next().await {
                    let chunk = chunk.map_err(|e| map_object_store_error(e, path.as_ref()))?;
                    file.write_all(&chunk).await?;
                    written = written.saturating_add(chunk.len() as u64);
                }
                file.flush().await?;
                self.record_read_bytes(written);
                Ok(written)
            }
        })
        .await
    }

    /// Stream `path` to a local file while enforcing a maximum body size.
    ///
    /// The provider's advertised size is checked before creating the
    /// destination, and the streamed body is checked again so a changed or
    /// malformed response cannot fill the workspace beyond the caller's
    /// bound. Partial files are removed before the error is returned.
    pub async fn download_to_path_bounded(
        &self,
        path: &Path,
        dest: &std::path::Path,
        max_bytes: u64,
    ) -> Result<u64> {
        retry(&self.retry, || {
            let path = path.clone();
            let dest = dest.to_owned();
            async move {
                use futures_util::StreamExt;
                use tokio::io::AsyncWriteExt;

                let _ = tokio::fs::remove_file(&dest).await;
                let result = async {
                    let read_inner = self.read_inner_for(&path);
                    let got = read_inner
                        .get(&path)
                        .await
                        .map_err(|e| map_object_store_error(e, path.as_ref()))?;
                    if got.meta.size > max_bytes {
                        return Err(StorageError::CorruptObject {
                            path: path.as_ref().to_owned(),
                            reason: format!(
                                "object advertises {} bytes, bounded download allows {max_bytes}",
                                got.meta.size
                            ),
                        });
                    }
                    let expected_size = got.meta.size;
                    let mut stream = got.into_stream();
                    let mut file = tokio::fs::File::create(&dest).await?;
                    let mut written = 0u64;

                    while let Some(chunk) = stream.next().await {
                        let chunk = chunk.map_err(|e| map_object_store_error(e, path.as_ref()))?;
                        let next = written.checked_add(chunk.len() as u64).ok_or_else(|| {
                            StorageError::CorruptObject {
                                path: path.as_ref().to_owned(),
                                reason: "streamed body size overflows u64".to_owned(),
                            }
                        })?;
                        if next > max_bytes {
                            return Err(StorageError::CorruptObject {
                                path: path.as_ref().to_owned(),
                                reason: format!(
                                    "streamed body exceeds bounded download limit of {max_bytes} bytes"
                                ),
                            });
                        }
                        file.write_all(&chunk).await?;
                        written = next;
                    }
                    file.flush().await?;
                    ensure_complete_body(&path, expected_size, written)?;
                    self.record_read_bytes(written);
                    Ok(written)
                }
                .await;
                if result.is_err() {
                    let _ = tokio::fs::remove_file(&dest).await;
                }
                result
            }
        })
        .await
    }

    /// Stream `path` into a blocking writer without collecting the full body.
    pub async fn stream_to_writer<W>(&self, path: &Path, writer: &mut W) -> Result<u64>
    where
        W: std::io::Write + ?Sized,
    {
        use futures_util::StreamExt;

        let read_inner = self.read_inner_for(path);
        let got = read_inner
            .get(path)
            .await
            .map_err(|e| map_object_store_error(e, path.as_ref()))?;
        let mut stream = got.into_stream();
        let mut written = 0u64;

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| map_object_store_error(e, path.as_ref()))?;
            writer.write_all(&chunk)?;
            written = written.saturating_add(chunk.len() as u64);
        }
        writer.flush()?;
        self.record_read_bytes(written);
        Ok(written)
    }

    /// Reads `path` and verifies its Blake3 content hash matches `expected_hash`.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::CorruptObject`] when the computed hash
    /// differs from `expected_hash`. The retry layer treats
    /// `CorruptObject` as `FatalAfterOneRetry` so a single transient
    /// bit-flip on the wire gets one shot at self-healing before the
    /// error is surfaced.
    pub async fn verify(&self, path: &Path, expected_hash: &[u8; 32]) -> Result<Bytes> {
        retry(&self.retry, || {
            let path = path.clone();
            async move {
                let (bytes, _etag) = self.get_with_etag(&path).await?;
                let actual = *blake3::hash(&bytes).as_bytes();
                if actual == *expected_hash {
                    Ok(bytes)
                } else {
                    Err(StorageError::CorruptObject {
                        path: path.to_string(),
                        reason: format!(
                            "expected blake3 {}, got {}",
                            hex_lower(expected_hash),
                            hex_lower(&actual)
                        ),
                    })
                }
            }
        })
        .await
    }

    /// Stream an immutable object and verify both its size and Blake3 identity.
    ///
    /// Unlike [`Self::verify`], this does not retain the body in memory and is
    /// suitable for existing multi-gigabyte packs and xorbs encountered by a
    /// resumable writer.
    pub async fn verify_size_and_hash(
        &self,
        path: &Path,
        expected_size: u64,
        expected_hash: &[u8; 32],
    ) -> Result<()> {
        let meta = self.head(path).await?;
        if meta.size != expected_size {
            return Err(StorageError::CorruptObject {
                path: path.to_string(),
                reason: format!("expected {expected_size} bytes, found {}", meta.size),
            });
        }
        if Self::matches_in_streaming(&self.read_inner_for(path), path, expected_hash).await? {
            Ok(())
        } else {
            Err(StorageError::CorruptObject {
                path: path.to_string(),
                reason: format!(
                    "Blake3 content hash did not match {}",
                    hex_lower(expected_hash)
                ),
            })
        }
    }

    /// Fetches the metadata for `path` without reading its body.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::NotFound`] if `path` does not exist.
    pub async fn head(&self, path: &Path) -> Result<ObjectMeta> {
        retry(&self.retry, || {
            let path = path.clone();
            async move {
                self.record_read_request(StorageReadKind::Head);
                self.read_inner_for(&path)
                    .head(&path)
                    .await
                    .map_err(|e| map_object_store_error(e, path.as_ref()))
            }
        })
        .await
    }

    /// Reads the byte range `[range.start, range.end)` from `path`.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::NotFound`] if `path` does not exist, or
    /// the backend's error if the range is unsatisfiable.
    pub async fn range_get(&self, path: &Path, range: Range<u64>) -> Result<Bytes> {
        retry(&self.retry, || {
            let path = path.clone();
            let range = range.clone();
            async move {
                self.record_read_request(StorageReadKind::Range);
                let bytes = self
                    .read_inner_for(&path)
                    .get_range(&path, range)
                    .await
                    .map_err(|e| map_object_store_error(e, path.as_ref()))?;
                self.record_read_bytes(bytes.len() as u64);
                Ok(bytes)
            }
        })
        .await
    }

    /// Deletes `path`.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError::NotFound`] if `path` did not exist. Most
    /// callers can ignore that variant to keep deletes idempotent.
    pub async fn delete(&self, path: &Path) -> Result<()> {
        retry(&self.retry, || {
            let path = path.clone();
            async move {
                self.inner
                    .delete(&path)
                    .await
                    .map_err(|e| map_object_store_error(e, path.as_ref()))
            }
        })
        .await
    }

    /// Copy `from` to `to`, replacing `to` if it already exists.
    pub async fn copy(&self, from: &Path, to: &Path) -> Result<()> {
        retry(&self.retry, || {
            let from = from.clone();
            let to = to.clone();
            async move {
                self.inner
                    .copy(&from, &to)
                    .await
                    .map_err(|e| map_object_store_error(e, to.as_ref()))
            }
        })
        .await
    }

    /// Copy `from` to `to` only when `to` does not already exist.
    pub async fn copy_if_not_exists(&self, from: &Path, to: &Path) -> Result<()> {
        retry(&self.retry, || {
            let from = from.clone();
            let to = to.clone();
            async move {
                self.inner
                    .copy_if_not_exists(&from, &to)
                    .await
                    .map_err(|e| map_object_store_error(e, to.as_ref()))
            }
        })
        .await
    }

    /// Promotes an unreferenced staged object to a canonical content-addressed key.
    ///
    /// If the canonical key already contains the same content, this succeeds
    /// idempotently. If it contains different content, the repository is
    /// corrupt and the caller must not publish metadata that references it.
    pub async fn promote_staged_content_addressed_object(
        &self,
        staged: &Path,
        canonical: &Path,
        expected_hash: [u8; 32],
        expected_size: u64,
    ) -> Result<bool> {
        if self
            .canonical_matches(canonical, &expected_hash, expected_size)
            .await?
        {
            return Ok(false);
        }

        match self.copy_if_not_exists(staged, canonical).await {
            Ok(()) => {
                if self
                    .canonical_matches(canonical, &expected_hash, expected_size)
                    .await?
                {
                    Ok(true)
                } else {
                    Err(StorageError::CorruptObject {
                        path: canonical.to_string(),
                        reason: "promoted object content does not match expected hash".to_owned(),
                    })
                }
            }
            Err(StorageError::StateConflict { .. }) => {
                if self
                    .canonical_matches(canonical, &expected_hash, expected_size)
                    .await?
                {
                    Ok(false)
                } else {
                    Err(StorageError::CorruptObject {
                        path: canonical.to_string(),
                        reason: "canonical object exists with different content".to_owned(),
                    })
                }
            }
            Err(e) => Err(e),
        }
    }

    async fn canonical_matches(
        &self,
        canonical: &Path,
        expected_hash: &[u8; 32],
        expected_size: u64,
    ) -> Result<bool> {
        match self.head(canonical).await {
            Ok(meta) if meta.size == expected_size => {
                Self::matches_in_streaming(&self.inner, canonical, expected_hash).await
            }
            Ok(meta) => Err(StorageError::CorruptObject {
                path: canonical.to_string(),
                reason: format!(
                    "canonical object size {} does not match expected {}",
                    meta.size, expected_size
                ),
            }),
            Err(StorageError::NotFound { .. }) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Begin a multipart upload for `path`.
    pub async fn create_multipart_upload(&self, path: &Path) -> Result<Box<dyn MultipartUpload>> {
        retry(&self.retry, || {
            let path = path.clone();
            async move {
                let (write_path, write_inner, _record_staged_write) =
                    self.exact_write_target(&path);
                write_inner
                    .put_multipart(&write_path)
                    .await
                    .map_err(|e| map_object_store_error(e, write_path.as_ref()))
            }
        })
        .await
    }

    /// Deletes every object under `prefix` and returns the number removed.
    ///
    /// Missing objects are ignored so concurrent or repeated cleanup stays
    /// idempotent.
    pub async fn delete_prefix(&self, prefix: &Path) -> Result<u64> {
        let mut deleted = 0;
        for meta in self.list_prefix(prefix).await? {
            match self.delete(&meta.location).await {
                Ok(()) => deleted += 1,
                Err(StorageError::NotFound { .. }) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(deleted)
    }

    /// Lists object metadata under `prefix`.
    pub async fn list_prefix(&self, prefix: &Path) -> Result<Vec<ObjectMeta>> {
        use futures_util::StreamExt;

        let mut objects = Vec::new();
        let read_inner = self.read_inner_for(prefix);
        let mut stream = read_inner.list(Some(prefix));
        while let Some(item) = stream.next().await {
            objects.push(item.map_err(|e| map_object_store_error(e, prefix.as_ref()))?);
        }
        Ok(objects)
    }

    /// Lists at most `limit` objects without buffering an unbounded prefix.
    ///
    /// Returns `None` when another object exists beyond the bound.
    pub async fn list_prefix_bounded(
        &self,
        prefix: &Path,
        limit: usize,
    ) -> Result<Option<Vec<ObjectMeta>>> {
        let mut objects = Vec::with_capacity(limit.min(1_024));
        let read_inner = self.read_inner_for(prefix);
        let mut stream = read_inner.list(Some(prefix));
        while let Some(item) = stream.next().await {
            if objects.len() == limit {
                return Ok(None);
            }
            objects.push(item.map_err(|error| map_object_store_error(error, prefix.as_ref()))?);
        }
        Ok(Some(objects))
    }

    /// Upload `data` as a multipart object, retrying the *whole* upload
    /// on a transient part failure.
    ///
    /// Unlike the raw `ObjectStore::put_multipart`, this wraps a complete
    /// create → write-parts → finish attempt in [`retry`]. Each retry is a
    /// fresh multipart upload: the previous attempt is aborted first so no
    /// orphaned parts linger. This is the retry boundary that makes a single
    /// transient connect blip (the `error sending request` failure that
    /// otherwise aborts a push) retry instead of failing the whole ref.
    ///
    /// # Why the whole upload, not per-part
    ///
    /// `object_store`'s high-level [`MultipartUpload::put_part`] assigns
    /// `part_idx` by call order and increments an internal counter; it
    /// offers no way to re-pin an index on retry. Re-calling `put_part`
    /// for a failed part would upload to a *new* index and corrupt the
    /// `complete` sequence. So the safe, idempotent boundary is the entire
    /// upload: re-create and re-write every part from index 0. S3 completes
    /// with overwrite semantics, so a part re-sent at the same index
    /// replaces the stale copy from the aborted attempt.
    ///
    /// `cancel` is checked at part boundaries; cancellation surfaces as
    /// [`StorageError::Cancelled`] and aborts the in-flight attempt first.
    ///
    /// `on_part_done` is invoked with the byte count of each part that
    /// uploads successfully, so callers can drive a byte-granular progress
    /// bar while a single large object is in flight. Pass `None` to use the
    /// simpler sequential `WriteMultipart` path.
    ///
    /// # Errors
    ///
    /// Transient errors (`NetworkTransient`, `Throttled`) retry under the
    /// store's [`RetryPolicy`]; the last error surfaces once the budget is
    /// exhausted. [`StorageError::Cancelled`] and other fatal classes propagate
    /// immediately — the retry layer never re-attempts them.
    pub async fn put_multipart_retry(
        &self,
        path: &Path,
        data: Bytes,
        part_size: usize,
        cancel: &tokio_util::sync::CancellationToken,
        on_part_done: Option<&(dyn Fn(u64) + Send + Sync)>,
    ) -> Result<()> {
        if part_size == 0 {
            return Err(StorageError::Internal(
                "multipart part size must be greater than zero".to_owned(),
            ));
        }
        let expected_hash = *blake3::hash(&data).as_bytes();
        let size = data.len() as u64;
        let (write_path, write_inner, record_staged_write) = self.exact_write_target(path);

        // The closure passed to `retry` is `FnMut` and re-invoked per
        // attempt. We capture clones of the cheap handles (`Path`, `Bytes`
        // refcount bump, `CancellationToken` Arc handle) so each attempt is
        // independent. The `&dyn Fn` callback is shared (read-only), so it
        // is passed by reference into each attempt without `unsafe`.
        let result = retry(&self.retry, || {
            let path = write_path.clone();
            let data = data.clone();
            let cancel = cancel.clone();
            let inner = write_inner.clone();
            async move {
                Self::put_multipart_once(&inner, &path, &data, part_size, &cancel, on_part_done)
                    .await
            }
        })
        .await;
        if result.is_ok() && record_staged_write {
            self.record_staged_write(path, &write_path, &expected_hash, size);
        }
        result
    }

    /// Upload a local file as a multipart object, retrying the whole upload.
    ///
    /// This preserves [`Self::put_multipart_retry`]'s retry and abort
    /// boundary while avoiding a full in-memory `Bytes` body for payloads
    /// that already live on disk.
    pub async fn put_multipart_file_retry(
        &self,
        path: &Path,
        file_path: &std::path::Path,
        size: u64,
        expected_hash: [u8; 32],
        part_size: usize,
        cancel: &tokio_util::sync::CancellationToken,
        on_part_done: Option<&(dyn Fn(u64) + Send + Sync)>,
    ) -> Result<()> {
        if part_size == 0 {
            return Err(StorageError::Internal(
                "multipart part size must be greater than zero".to_owned(),
            ));
        }
        let (write_path, write_inner, record_staged_write) = self.exact_write_target(path);

        let result = retry(&self.retry, || {
            let path = write_path.clone();
            let file_path = file_path.to_owned();
            let cancel = cancel.clone();
            let inner = write_inner.clone();
            async move {
                Self::put_multipart_file_once(
                    &inner,
                    &path,
                    &file_path,
                    size,
                    expected_hash,
                    part_size,
                    &cancel,
                    on_part_done,
                )
                .await
            }
        })
        .await;
        if result.is_ok() && record_staged_write {
            self.record_staged_write(path, &write_path, &expected_hash, size);
        }
        result
    }

    /// Upload a local file as a multipart object with part-level resume.
    ///
    /// Extends [`Self::put_multipart_file_retry`] with a
    /// [`MultipartJournal`]: before uploading, a recorded session for
    /// `payload_hash` is resumed and only its missing parts are sent;
    /// every completed part is journaled so a later invocation can
    /// continue from the last good part instead of restarting from zero.
    ///
    /// Failure policy differs deliberately from the non-resumable path:
    /// cancellation leaves the backend session and its journal row alive
    /// so a later push resumes from the last good part; abandonment is
    /// reclaimed by `fsck` once the row outlives its grace period.
    /// Hard errors clean up immediately (backend abort plus row drop) —
    /// retrying callers start a fresh session rather than inheriting one
    /// of unknown health. Attempts that could not claim journal ownership
    /// ([`JournalLease::StandDown`], e.g. a concurrent uploader holds the
    /// row) abort on every exit path because nothing tracks their
    /// orphaned parts.
    ///
    /// Resume is disabled under staging-write prefixes because those
    /// temporary targets belong to a single protected-push transaction.
    /// Staging callers get the whole-retry path unchanged.
    #[allow(clippy::too_many_arguments)]
    pub async fn put_multipart_file_resumable_retry(
        &self,
        path: &Path,
        file_path: &std::path::Path,
        size: u64,
        expected_hash: [u8; 32],
        payload_hash: &[u8],
        part_size: usize,
        cancel: &tokio_util::sync::CancellationToken,
        on_part_done: Option<&(dyn Fn(u64) + Send + Sync)>,
        journal: Option<&dyn crate::multipart::MultipartJournal>,
    ) -> Result<bool> {
        let (write_path, _, record_staged_write) = self.exact_write_target(path);
        let (Some(multipart), Some(journal)) = (self.multipart(), journal) else {
            self.put_multipart_file_retry(
                path,
                file_path,
                size,
                expected_hash,
                part_size,
                cancel,
                on_part_done,
            )
            .await?;
            return Ok(false);
        };
        if record_staged_write {
            self.put_multipart_file_retry(
                path,
                file_path,
                size,
                expected_hash,
                part_size,
                cancel,
                on_part_done,
            )
            .await?;
            return Ok(false);
        }

        let bucket_label = {
            let identity = self.bucket_identity();
            format!(
                "{:?}/{}/{}",
                identity.cloud, identity.host, identity.container
            )
        };

        // One strike per resume source: a recorded row is probed at most
        // once per call. A stale upload id fails fast here and later
        // attempts start fresh; without this guard a dead id would burn
        // every retry attempt re-probing it. The strike is spent when an
        // attempt starts with probing allowed, so no state crosses the
        // retry boundary.
        let resume_available = std::sync::atomic::AtomicBool::new(true);

        retry(&self.retry, || {
            let allow_resume = resume_available.swap(false, std::sync::atomic::Ordering::Relaxed);
            let path = write_path.clone();
            let file_path = file_path.to_owned();
            let cancel = cancel.clone();
            let multipart = multipart.clone();
            let payload_hash = payload_hash.to_vec();
            let bucket_label = bucket_label.clone();
            async move {
                Self::put_multipart_file_resumable_once(
                    &multipart,
                    journal,
                    &path,
                    &file_path,
                    size,
                    &payload_hash,
                    &bucket_label,
                    part_size,
                    &cancel,
                    on_part_done,
                    allow_resume,
                )
                .await
            }
        })
        .await
    }

    /// One resumable multipart-upload attempt: claim or resume a journal
    /// lease, upload only missing parts with bounded concurrency, then
    /// complete. See [`Self::put_multipart_file_resumable_retry`] for the
    /// failure policy.
    #[allow(clippy::too_many_arguments)]
    async fn put_multipart_file_resumable_once(
        multipart: &Arc<dyn object_store::multipart::MultipartStore>,
        journal: &dyn crate::multipart::MultipartJournal,
        path: &Path,
        file_path: &std::path::Path,
        size: u64,
        payload_hash: &[u8],
        bucket_label: &str,
        part_size: usize,
        cancel: &tokio_util::sync::CancellationToken,
        on_part_done: Option<&(dyn Fn(u64) + Send + Sync)>,
        allow_resume: bool,
    ) -> Result<bool> {
        use futures_util::stream::{FuturesUnordered, StreamExt};

        const IN_FLIGHT_PARTS: usize = 4;

        if part_size == 0 {
            return Err(StorageError::Internal(
                "multipart part_size must be non-zero".to_owned(),
            ));
        }
        let total_parts = size.div_ceil(part_size as u64) as usize;

        let report_part = |bytes: u64| {
            if let Some(cb) = on_part_done {
                cb(bytes);
            }
        };

        // Claim phase: reuse a compatible recorded session when probing
        // is still allowed; discard incompatible rows.
        let mut slots: Vec<Option<crate::multipart::JournalPart>> = vec![None; total_parts];
        let mut lease = crate::multipart::JournalLease::StandDown;
        if allow_resume {
            match journal.resumable(payload_hash, bucket_label, path.as_ref()) {
                Ok(Some(info)) => {
                    match crate::multipart::compatible_parts(&info, total_parts, part_size) {
                        Some(compatible) => {
                            // Surface prior progress immediately so the
                            // caller's bar reflects already-uploaded parts.
                            for slot in compatible.iter().flatten() {
                                report_part(slot.size);
                            }
                            slots = compatible;
                            lease = crate::multipart::JournalLease::Active {
                                upload_id: info.upload_id.clone(),
                            };
                        }
                        None => {
                            // Boundary drift or out-of-range parts make
                            // the recorded id unusable; drop the row and
                            // best-effort abort its backend session so
                            // the provider does not retain stray parts.
                            let stale_id = object_store::MultipartId::from(info.upload_id.as_str());
                            if Self::abort_resumable_session(
                                multipart,
                                path,
                                &stale_id,
                                "incompatible multipart session",
                            )
                            .await
                                && let Err(err) = journal.abort_stale(&info.upload_id)
                            {
                                crate::multipart::warn_journal_error("abort_stale", err);
                            }
                        }
                    }
                }
                Ok(None) => {}
                Err(err) => crate::multipart::warn_journal_error("resumable lookup", err),
            }
        }

        // Fresh sessions must begin before the first part PUT.
        let upload_id = match &lease {
            crate::multipart::JournalLease::Active { upload_id } => upload_id.clone(),
            crate::multipart::JournalLease::StandDown => {
                let id = multipart
                    .create_multipart(path)
                    .await
                    .map_err(|e| map_object_store_error(e, path.as_ref()))?;
                let claimed = journal
                    .begin(payload_hash, bucket_label, path.as_ref(), &id)
                    .unwrap_or_else(|err| {
                        crate::multipart::warn_journal_error("begin", err);
                        false
                    });
                lease = if claimed {
                    crate::multipart::JournalLease::Active {
                        upload_id: id.clone(),
                    }
                } else {
                    // A concurrent uploader owns the row for this payload
                    // hash; proceed unjournaled. Failures below abort the
                    // backend session since nothing tracks our parts.
                    crate::multipart::JournalLease::StandDown
                };
                id
            }
        };
        let multipart_id = object_store::MultipartId::from(upload_id.as_str());

        let resumed = lease.upload_id().is_some() && slots.iter().any(Option::is_some);

        // Upload missing parts with bounded in-flight concurrency. Parts
        // are read at their exact file offsets so resumed sessions skip
        // already-uploaded prefixes without reading them.
        type PartResult = std::result::Result<(usize, String, u64), object_store::Error>;
        let mut pending: futures_util::stream::FuturesUnordered<
            std::pin::Pin<Box<dyn std::future::Future<Output = PartResult> + Send>>,
        > = FuturesUnordered::new();
        let mut next_idx = 0usize;
        let result = loop {
            if cancel.is_cancelled() {
                break Err(StorageError::Cancelled);
            }
            while next_idx < total_parts && slots[next_idx].is_some() {
                next_idx += 1;
            }
            let drained_all = next_idx >= total_parts;
            if drained_all && pending.is_empty() {
                break Ok(());
            }
            // Drain when at capacity or when nothing remains to dispatch;
            // otherwise keep the pipeline full.
            if pending.len() >= IN_FLIGHT_PARTS || drained_all {
                let completed = tokio::select! {
                    () = cancel.cancelled() => break Err(StorageError::Cancelled),
                    completed = pending.next() => completed,
                };
                match completed {
                    Some(Ok((idx, content_id, bytes))) => {
                        if let Some(id) = lease.upload_id()
                            && let Err(err) = journal.record_part(id, idx, &content_id, bytes)
                        {
                            crate::multipart::warn_journal_error("record_part", err);
                        }
                        slots[idx] = Some(crate::multipart::JournalPart {
                            part_idx: idx,
                            content_id,
                            size: bytes,
                        });
                        report_part(bytes);
                    }
                    Some(Err(err)) => {
                        break Err(map_object_store_error(err, path.as_ref()));
                    }
                    None => {
                        break Err(StorageError::Internal(
                            "resumable multipart queue ended while non-empty".to_owned(),
                        ));
                    }
                }
                continue;
            }
            let idx = next_idx;
            next_idx += 1;
            let offset = idx as u64 * part_size as u64;
            let want = std::cmp::min(part_size as u64, size - offset) as usize;
            let mut buf = vec![0u8; want];
            let read = async {
                let mut file = tokio::fs::File::open(file_path).await?;
                use tokio::io::{AsyncReadExt, AsyncSeekExt};
                file.seek(std::io::SeekFrom::Start(offset)).await?;
                file.read_exact(&mut buf).await?;
                Ok::<_, std::io::Error>(())
            };
            if let Err(err) = read.await {
                break Err(StorageError::Io { source: err });
            }
            let fut = multipart.put_part(path, &multipart_id, idx, bytes::Bytes::from(buf).into());
            pending.push(Box::pin(async move {
                let part = fut.await?;
                Ok((idx, part.content_id, want as u64))
            }));
        };

        if result.is_err() {
            let keep_alive =
                matches!(result, Err(StorageError::Cancelled)) && lease.upload_id().is_some();
            if !keep_alive {
                let aborted = Self::abort_resumable_session(
                    multipart,
                    path,
                    &multipart_id,
                    "failed multipart session",
                )
                .await;
                if aborted
                    && let Some(id) = lease.upload_id()
                    && let Err(err) = journal.abort_stale(id)
                {
                    crate::multipart::warn_journal_error("abort_stale", err);
                }
            }
            return result.map(|_| false);
        }

        let parts = slots
            .iter()
            .enumerate()
            .map(|(idx, slot)| {
                let part = slot.as_ref().ok_or_else(|| {
                    StorageError::Internal(format!("multipart part {idx} missing before complete"))
                })?;
                Ok(object_store::multipart::PartId {
                    content_id: part.content_id.clone(),
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let completed = multipart
            .complete_multipart(path, &multipart_id, parts)
            .await;
        if let Err(err) = completed {
            let aborted = Self::abort_resumable_session(
                multipart,
                path,
                &multipart_id,
                "multipart session after completion failure",
            )
            .await;
            if aborted
                && let Some(id) = lease.upload_id()
                && let Err(cleanup) = journal.abort_stale(id)
            {
                crate::multipart::warn_journal_error("abort_stale", cleanup);
            }
            return Err(map_object_store_error(err, path.as_ref()));
        }

        if let Some(id) = lease.upload_id()
            && let Err(err) = journal.complete(id)
        {
            crate::multipart::warn_journal_error("complete", err);
        }
        Ok(resumed)
    }

    /// Abort one explicit multipart session, treating an already-absent
    /// provider session as clean. Other failures keep the journal row so a
    /// later push or fsck run retains enough identity to retry cleanup.
    async fn abort_resumable_session(
        multipart: &Arc<dyn object_store::multipart::MultipartStore>,
        path: &Path,
        upload_id: &object_store::MultipartId,
        reason: &'static str,
    ) -> bool {
        match multipart.abort_multipart(path, upload_id).await {
            Ok(()) | Err(object_store::Error::NotFound { .. }) => true,
            Err(error) => {
                tracing::debug!(
                    path = %path,
                    error = %error,
                    reason,
                    "multipart provider abort failed; preserving recovery row"
                );
                false
            }
        }
    }

    async fn put_multipart_once(
        inner: &Arc<dyn ObjectStore>,
        path: &Path,
        data: &[u8],
        part_size: usize,
        cancel: &tokio_util::sync::CancellationToken,
        on_part_done: Option<&(dyn Fn(u64) + Send + Sync)>,
    ) -> Result<()> {
        use futures_util::stream::{FuturesUnordered, StreamExt};

        const IN_FLIGHT_PARTS: usize = 4;

        // No progress callback? Use the high-level helper that manages part
        // parallelism internally — identical behaviour to the prior fallback
        // path, including cancellation handling.
        let Some(cb) = on_part_done else {
            return Self::put_multipart_writer(inner, path, data, part_size, cancel).await;
        };

        // Progress-aware path: drive MultipartUpload directly so we can
        // report bytes as each part completes. Bounded concurrency keeps
        // the pipeline full without unbounded memory or socket pressure.
        let mut upload = inner
            .put_multipart(path)
            .await
            .map_err(|e| map_object_store_error(e, path.as_ref()))?;

        // On any error path below we abort the upload before returning so
        // partially-uploaded parts don't linger until a lifecycle rule or
        // `fsck --repair` reclaims them.
        let abort_on = |mut upload: Box<dyn object_store::MultipartUpload>| async move {
            if let Err(e) = upload.abort().await {
                tracing::warn!(
                    path = %path,
                    error = %e,
                    "failed to abort multipart upload",
                );
            }
        };

        let mut pending = FuturesUnordered::new();
        for chunk in data.chunks(part_size) {
            if cancel.is_cancelled() {
                abort_on(upload).await;
                return Err(StorageError::Cancelled);
            }
            if pending.len() >= IN_FLIGHT_PARTS {
                match pending.next().await {
                    Some((Ok(()), bytes)) => cb(bytes),
                    Some((Err(e), _)) => {
                        abort_on(upload).await;
                        return Err(map_object_store_error(e, path.as_ref()));
                    }
                    None => {
                        abort_on(upload).await;
                        return Err(StorageError::Internal(
                            "multipart progress queue ended while non-empty".to_owned(),
                        ));
                    }
                }
            }
            let bytes = chunk.len() as u64;
            let fut = upload.put_part(bytes::Bytes::copy_from_slice(chunk).into());
            pending.push(async move { (fut.await, bytes) });
        }

        while let Some((res, bytes)) = pending.next().await {
            match res {
                Ok(()) => cb(bytes),
                Err(e) => {
                    abort_on(upload).await;
                    return Err(map_object_store_error(e, path.as_ref()));
                }
            }
        }

        if cancel.is_cancelled() {
            abort_on(upload).await;
            return Err(StorageError::Cancelled);
        }

        upload
            .complete()
            .await
            .map_err(|e| map_object_store_error(e, path.as_ref()))?;

        Ok(())
    }

    async fn put_multipart_file_once(
        inner: &Arc<dyn ObjectStore>,
        path: &Path,
        file_path: &std::path::Path,
        size: u64,
        expected_hash: [u8; 32],
        part_size: usize,
        cancel: &tokio_util::sync::CancellationToken,
        on_part_done: Option<&(dyn Fn(u64) + Send + Sync)>,
    ) -> Result<()> {
        use futures_util::stream::{FuturesUnordered, StreamExt};

        const IN_FLIGHT_PARTS: usize = 4;

        let mut upload = inner
            .put_multipart(path)
            .await
            .map_err(|e| map_object_store_error(e, path.as_ref()))?;

        let abort_on = |mut upload: Box<dyn object_store::MultipartUpload>| async move {
            if let Err(e) = upload.abort().await {
                tracing::warn!(
                    path = %path,
                    error = %e,
                    "failed to abort multipart file upload",
                );
            }
        };

        let mut file = match tokio::fs::File::open(file_path).await {
            Ok(file) => file,
            Err(error) => {
                abort_on(upload).await;
                return Err(error.into());
            }
        };
        let actual_size = match file.metadata().await {
            Ok(metadata) => metadata.len(),
            Err(error) => {
                abort_on(upload).await;
                return Err(error.into());
            }
        };
        if actual_size != size {
            abort_on(upload).await;
            return Err(StorageError::CorruptObject {
                path: file_path.display().to_string(),
                reason: format!("local file has {actual_size} bytes; upload expects {size}"),
            });
        }
        let mut hasher = blake3::Hasher::new();
        let mut remaining = size;
        let mut pending = FuturesUnordered::new();
        while remaining > 0 {
            if cancel.is_cancelled() {
                abort_on(upload).await;
                return Err(StorageError::Cancelled);
            }
            if pending.len() >= IN_FLIGHT_PARTS {
                match pending.next().await {
                    Some((Ok(()), bytes)) => {
                        if let Some(cb) = on_part_done {
                            cb(bytes);
                        }
                    }
                    Some((Err(e), _)) => {
                        abort_on(upload).await;
                        return Err(map_object_store_error(e, path.as_ref()));
                    }
                    None => {
                        abort_on(upload).await;
                        return Err(StorageError::Internal(
                            "multipart file progress queue ended while non-empty".to_owned(),
                        ));
                    }
                }
            }

            let want = std::cmp::min(part_size as u64, remaining) as usize;
            let mut buf = vec![0u8; want];
            if let Err(error) = file.read_exact(&mut buf).await {
                abort_on(upload).await;
                return Err(error.into());
            }
            hasher.update(&buf);
            remaining -= want as u64;

            let bytes = want as u64;
            let fut = upload.put_part(bytes::Bytes::from(buf).into());
            pending.push(async move { (fut.await, bytes) });
        }

        while let Some((res, bytes)) = pending.next().await {
            match res {
                Ok(()) => {
                    if let Some(cb) = on_part_done {
                        cb(bytes);
                    }
                }
                Err(e) => {
                    abort_on(upload).await;
                    return Err(map_object_store_error(e, path.as_ref()));
                }
            }
        }

        let actual_hash = *hasher.finalize().as_bytes();
        if actual_hash != expected_hash {
            abort_on(upload).await;
            return Err(StorageError::CorruptObject {
                path: file_path.display().to_string(),
                reason: format!(
                    "local blake3 hash {} does not match expected {}",
                    hex_lower(&actual_hash),
                    hex_lower(&expected_hash)
                ),
            });
        }
        let final_size = match file.metadata().await {
            Ok(metadata) => metadata.len(),
            Err(error) => {
                abort_on(upload).await;
                return Err(error.into());
            }
        };
        if final_size != size {
            abort_on(upload).await;
            return Err(StorageError::CorruptObject {
                path: file_path.display().to_string(),
                reason: format!(
                    "local file changed during upload: expected {size} bytes, found {final_size}"
                ),
            });
        }

        if cancel.is_cancelled() {
            abort_on(upload).await;
            return Err(StorageError::Cancelled);
        }

        upload
            .complete()
            .await
            .map_err(|e| map_object_store_error(e, path.as_ref()))?;

        Ok(())
    }

    /// Sequential multipart path using `WriteMultipart` (no per-part
    /// progress callback). Preserves abort-on-cancel semantics so dropped
    /// uploads don't leak parts.
    async fn put_multipart_writer(
        inner: &Arc<dyn ObjectStore>,
        path: &Path,
        data: &[u8],
        part_size: usize,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<()> {
        use object_store::WriteMultipart;

        let upload = inner
            .put_multipart(path)
            .await
            .map_err(|e| map_object_store_error(e, path.as_ref()))?;

        let mut writer = WriteMultipart::new(upload);
        let mut cancelled_during_write = false;
        for chunk in data.chunks(part_size) {
            // Observe cancellation between parts. Racing the write itself
            // via tokio::select! is not practical — WriteMultipart::write
            // is synchronous — but chunk boundaries are frequent enough
            // (8–16 MiB apart) to give timely response.
            if cancel.is_cancelled() {
                cancelled_during_write = true;
                break;
            }
            writer.write(chunk);
        }

        // If cancelled at any point, abort the upload to release any parts
        // already uploaded; without this, `WriteMultipart::drop` would leave
        // orphaned parts consuming storage until a lifecycle rule or
        // `fsck --repair` cleans them up.
        if cancelled_during_write || cancel.is_cancelled() {
            if let Err(e) = writer.abort().await {
                tracing::warn!(
                    path = %path,
                    error = %e,
                    "failed to abort multipart upload after cancellation",
                );
            }
            return Err(StorageError::Cancelled);
        }

        writer
            .finish()
            .await
            .map_err(|e| map_object_store_error(e, path.as_ref()))?;

        Ok(())
    }

    /// Generate a presigned HTTPS URL for a GET of `path`, valid for
    /// `expires_in`.
    ///
    /// Works only for backends whose concrete `ObjectStore` impl also
    /// implements [`object_store::signer::Signer`] — today that's
    /// [`object_store::aws::AmazonS3`]. Other backends surface a
    /// [`StorageError::Internal`] carrying a "presign unsupported for
    /// this backend" marker; callers that present a user-facing
    /// "copy download link" action should pre-check via
    /// [`Self::bucket_identity`] and avoid calling this on non-S3
    /// stores, or catch the error and fall back to a regular
    /// download.
    ///
    /// # Expiry
    ///
    /// `expires_in` is handed to the underlying signer verbatim. S3
    /// caps signed URLs at 7 days; passing a longer `expires_in` on
    /// S3 yields a signer error rather than a silently-clamped URL.
    ///
    /// # Why this lives on `Store`
    ///
    /// The downcast to `AmazonS3` needs concrete-type visibility that
    /// higher-level callers should not have; otherwise they would link
    /// backend-specific signing and transport stacks directly. Keeping
    /// the helper here lets callers request a signed URL without caring
    /// about backend-specific plumbing.
    ///
    /// # Errors
    ///
    /// - [`StorageError::Internal`] when the backing `ObjectStore`
    ///   is not an S3 impl (the `Signer` trait isn't implemented).
    /// - [`StorageError::Internal`] for signer failures (credential
    ///   auth service failure, bogus `expires_in`, transport error while
    ///   computing the signature).
    ///
    /// # Example
    ///
    /// ```ignore
    /// use std::time::Duration;
    /// use object_store::path::Path;
    /// let url = store
    ///     .signed_url(&Path::from("repo/objects/artifact.bin"), Duration::from_secs(3600))
    ///     .await?;
    /// // `url` is a direct HTTPS download link valid for 1 hour.
    /// ```
    pub async fn signed_url(
        &self,
        path: &Path,
        expires_in: std::time::Duration,
    ) -> Result<url::Url> {
        let signer = self.signer.as_ref().ok_or_else(|| {
            StorageError::Internal(
                "presign unsupported for this backend — \
                 only S3-compatible stores can sign URLs today"
                    .to_owned(),
            )
        })?;

        signer
            .signed_url(reqwest::Method::GET, path, expires_in)
            .await
            .map_err(|e| StorageError::Internal(format!("signed_url failed: {e}")))
    }
}

fn ensure_complete_body(path: &Path, expected: u64, actual: u64) -> Result<()> {
    if actual == expected {
        return Ok(());
    }
    Err(StorageError::CorruptObject {
        path: path.as_ref().to_owned(),
        reason: format!("response body has {actual} bytes, object metadata declares {expected}"),
    })
}

fn path_is_inside_prefix(path: &str, prefix: &str) -> bool {
    let path = path.trim_start_matches('/');
    let prefix = prefix.trim_matches('/');
    if prefix.is_empty() {
        return false;
    }
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}

fn hex_lower(bytes: &[u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for b in bytes {
        // Two hex digits per byte; `write!` to a String is infallible.
        use std::fmt::Write as _;
        let _ = write!(out, "{b:02x}");
    }
    out
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
    use crate::identity::StorageProviderKind;
    use futures_util::TryStreamExt as _;
    use object_store::memory::InMemory;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn memory_store() -> Store {
        let inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        // Keep the test retry budget small so failure cases return quickly.
        let policy = RetryPolicy {
            max_attempts: 2,
            base: std::time::Duration::from_millis(1),
            cap: std::time::Duration::from_millis(5),
        };
        Store::with_retry(inner, policy)
    }

    #[tokio::test]
    async fn put_then_get_roundtrips_bytes() {
        let store = memory_store();
        let path = Path::from("blobs/hello");
        let body = Bytes::from_static(b"hello world");

        store.put(&path, body.clone()).await.unwrap();
        let (got, _etag) = store.get_with_etag(&path).await.unwrap();
        assert_eq!(got, body);
    }

    #[tokio::test]
    async fn bounded_get_rejects_objects_before_consuming_oversized_body() {
        let store = memory_store();
        let path = Path::from("blobs/oversized");
        store
            .put(&path, Bytes::from_static(b"0123456789"))
            .await
            .unwrap();

        let error = store
            .get_with_etag_bounded(&path, 9)
            .await
            .expect_err("bounded read must reject an oversized object");
        assert!(matches!(error, StorageError::CorruptObject { .. }));
    }

    #[tokio::test]
    async fn bounded_get_roundtrips_objects_at_the_limit() {
        let store = memory_store();
        let path = Path::from("blobs/bounded");
        let body = Bytes::from_static(b"0123456789");
        store.put(&path, body.clone()).await.unwrap();

        let (got, _etag) = store
            .get_with_etag_bounded(&path, body.len() as u64)
            .await
            .unwrap();
        assert_eq!(got, body);
    }

    #[tokio::test]
    async fn bounded_download_rejects_oversized_objects_before_creating_file() {
        let store = memory_store();
        let path = Path::from("blobs/oversized-download");
        store
            .put(&path, Bytes::from_static(b"0123456789"))
            .await
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("download");

        let error = store
            .download_to_path_bounded(&path, &dest, 9)
            .await
            .expect_err("bounded download must reject an oversized object");

        assert!(matches!(error, StorageError::CorruptObject { .. }));
        assert!(!dest.exists());
    }

    #[tokio::test]
    async fn bounded_download_roundtrips_objects_at_the_limit() {
        let store = memory_store();
        let path = Path::from("blobs/bounded-download");
        let body = Bytes::from_static(b"0123456789");
        store.put(&path, body.clone()).await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("download");

        let size = store
            .download_to_path_bounded(&path, &dest, body.len() as u64)
            .await
            .unwrap();

        assert_eq!(size, body.len() as u64);
        assert_eq!(std::fs::read(dest).unwrap(), body);
    }

    #[tokio::test]
    async fn read_routes_get_and_head_matching_prefix() {
        let default_inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let routed_inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let store = Store::new(default_inner)
            .with_read_routes(vec![("repo/metadata".to_owned(), routed_inner.clone())]);
        let path = Path::from("repo/metadata/pack/indexes/hash.json");
        let body = Bytes::from_static(b"index");

        routed_inner.put(&path, body.clone().into()).await.unwrap();

        let (got, _etag) = store.get_with_etag(&path).await.unwrap();
        let meta = store.head(&path).await.unwrap();
        assert_eq!(got, body);
        assert_eq!(meta.location, path);
    }

    #[tokio::test]
    async fn put_is_idempotent_for_identical_content() {
        let store = memory_store();
        let path = Path::from("blobs/same");
        let body = Bytes::from_static(b"same content");

        store.put(&path, body.clone()).await.unwrap();
        // Second call must succeed: the object is already there with
        // matching content, which is the idempotent-retry case.
        store.put(&path, body).await.unwrap();
    }

    #[tokio::test]
    async fn create_strict_conflicts_even_for_identical_content() {
        let store = memory_store();
        let path = Path::from("locks/refs/heads/main/lock");
        let body = Bytes::from_static(b"same payload");

        store.create_strict(&path, body.clone()).await.unwrap();
        let err = store
            .create_strict(&path, body)
            .await
            .expect_err("strict create must not be idempotent for mutable coordination objects");
        assert!(
            matches!(err, StorageError::StateConflict { .. }),
            "expected CasConflict, got {err:?}"
        );
    }

    #[tokio::test]
    async fn put_conflicts_on_different_content() {
        let store = memory_store();
        let path = Path::from("blobs/diff");

        store
            .put(&path, Bytes::from_static(b"first"))
            .await
            .unwrap();
        let err = store
            .put(&path, Bytes::from_static(b"second"))
            .await
            .expect_err("conflicting content must not overwrite");
        assert!(
            matches!(err, StorageError::StateConflict { .. }),
            "expected CasConflict, got {err:?}"
        );
    }

    #[tokio::test]
    async fn staging_put_records_canonical_mapping_and_only_writes_staged_object() {
        let store = memory_store().with_staging_writes("repo/staging/push-1".to_owned());
        let canonical = Path::from("repo/manifests/candidate.json");
        let staged = Path::from("repo/staging/push-1/objects/repo/manifests/candidate.json");
        let body = Bytes::from_static(b"candidate");
        let hash = *blake3::hash(&body).as_bytes();

        store.put(&canonical, body.clone()).await.unwrap();

        let err = store
            .get_with_etag(&canonical)
            .await
            .expect_err("protected store must not write canonical object");
        assert!(
            matches!(err, StorageError::NotFound { .. }),
            "expected NotFound for canonical object, got {err:?}"
        );
        let (got, _etag) = store.get_with_etag(&staged).await.unwrap();
        assert_eq!(got, body);
        assert_eq!(
            store.staged_writes(),
            vec![StagedWrite {
                canonical_key: canonical.to_string(),
                staged_key: staged.to_string(),
                blake3: hex_lower(&hash),
                size: body.len() as u64,
            }]
        );
    }

    #[tokio::test]
    async fn flush_staged_writes_verifies_every_recorded_remote_object() {
        let inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let store = Store::new(inner.clone()).with_staging_writes("repo/staging/push-1".to_owned());
        let canonical = Path::from("repo/xorbs/xorb-a");
        let staged = Path::from("repo/staging/push-1/objects/repo/xorbs/xorb-a");

        store
            .put(&canonical, Bytes::from_static(b"durable"))
            .await
            .unwrap();
        let flushed = store.flush_staged_writes(4).await.unwrap();
        assert_eq!(flushed, store.staged_writes());

        inner
            .put(&staged, Bytes::from_static(b"short").into())
            .await
            .unwrap();
        let error = store.flush_staged_writes(4).await.unwrap_err();
        assert!(matches!(error, StorageError::CorruptObject { .. }));
    }

    #[tokio::test]
    async fn staging_control_file_reads_use_write_store_without_recording() {
        let read_inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let write_inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let store = Store::new(read_inner.clone())
            .with_staging_write_store("repo/staging/push-1".to_owned(), write_inner.clone());
        let plan = Path::from("repo/staging/push-1/push-plan.json");
        let body = Bytes::from_static(br#"{"schema_version":1}"#);

        store.put_exact(&plan, body.clone()).await.unwrap();

        let err = read_inner
            .get(&plan)
            .await
            .expect_err("canonical read store should not receive staging control writes");
        assert!(
            matches!(err, object_store::Error::NotFound { .. }),
            "expected NotFound from read store, got {err:?}"
        );
        let got = write_inner.get(&plan).await.unwrap().bytes().await.unwrap();
        assert_eq!(got, body);
        let (got_through_staged_route, _) = store.get_with_etag(&plan).await.unwrap();
        assert_eq!(got_through_staged_route, body);
        assert!(store.staged_writes().is_empty());
    }

    #[tokio::test]
    async fn staging_put_exact_canonical_file_records_staged_mapping() {
        let store = memory_store().with_staging_writes("repo/staging/push-1".to_owned());
        let canonical = Path::from("repo/metadata/pack/segments/hash.json");
        let staged =
            Path::from("repo/staging/push-1/objects/repo/metadata/pack/segments/hash.json");
        let body = Bytes::from_static(b"segment");
        let hash = *blake3::hash(&body).as_bytes();

        store.put_exact(&canonical, body.clone()).await.unwrap();

        let err = store
            .get_with_etag(&canonical)
            .await
            .expect_err("put_exact must stage canonical writes in protected mode");
        assert!(
            matches!(err, StorageError::NotFound { .. }),
            "expected NotFound for canonical object, got {err:?}"
        );
        let (got, _etag) = store.get_with_etag(&staged).await.unwrap();
        assert_eq!(got, body);
        assert_eq!(
            store.staged_writes(),
            vec![StagedWrite {
                canonical_key: canonical.to_string(),
                staged_key: staged.to_string(),
                blake3: hex_lower(&hash),
                size: body.len() as u64,
            }]
        );
    }

    #[tokio::test]
    async fn read_routes_use_longest_matching_prefix() {
        let broad: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let narrow: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let fallback: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let path = Path::from("repo/metadata/pack/index.json");
        broad
            .put(&path, Bytes::from_static(b"broad").into())
            .await
            .unwrap();
        narrow
            .put(&path, Bytes::from_static(b"narrow").into())
            .await
            .unwrap();
        let store = Store::new(fallback).with_read_routes(vec![
            ("repo/metadata".to_owned(), broad),
            ("repo/metadata/pack".to_owned(), narrow),
        ]);

        let (got, _etag) = store.get_with_etag(&path).await.unwrap();
        assert_eq!(got.as_ref(), b"narrow");
    }

    #[tokio::test]
    async fn scoped_reads_do_not_grant_canonical_writes() {
        let read_inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let write_inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let canonical = Path::from("repo/packs/pack-1.pack");
        let staged = Path::from("repo/staging/push-1/objects/repo/packs/pack-1.pack");
        read_inner
            .put(&canonical, Bytes::from_static(b"existing").into())
            .await
            .unwrap();
        let store = Store::new(read_inner.clone())
            .with_read_routes(vec![("repo/packs".to_owned(), read_inner.clone())])
            .with_staging_write_store("repo/staging/push-1".to_owned(), write_inner.clone());

        let (got, _etag) = store.get_with_etag(&canonical).await.unwrap();
        assert_eq!(got.as_ref(), b"existing");
        store
            .put(&canonical, Bytes::from_static(b"candidate"))
            .await
            .unwrap();
        let (still_canonical, _etag) = store.get_with_etag(&canonical).await.unwrap();
        assert_eq!(still_canonical.as_ref(), b"existing");
        let staged_body = write_inner
            .get(&staged)
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert_eq!(staged_body.as_ref(), b"candidate");
    }

    #[tokio::test]
    async fn staging_multipart_uses_write_store_and_records_mapping() {
        let read_inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let write_inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let store = Store::new(read_inner.clone())
            .with_staging_write_store("repo/staging/push-1".to_owned(), write_inner.clone());
        let canonical = Path::from("repo/xorbs/abc");
        let staged = Path::from("repo/staging/push-1/objects/repo/xorbs/abc");
        let body = Bytes::from_static(b"large enough for multipart test");
        let hash = *blake3::hash(&body).as_bytes();
        let cancel = tokio_util::sync::CancellationToken::new();

        store
            .put_multipart_retry(&canonical, body.clone(), 5, &cancel, None)
            .await
            .unwrap();

        let err = store
            .get_with_etag(&canonical)
            .await
            .expect_err("protected multipart upload must not write canonical object");
        assert!(
            matches!(err, StorageError::NotFound { .. }),
            "expected NotFound for canonical object, got {err:?}"
        );
        let got = write_inner
            .get(&staged)
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert_eq!(got, body);
        assert_eq!(
            store.staged_writes(),
            vec![StagedWrite {
                canonical_key: canonical.to_string(),
                staged_key: staged.to_string(),
                blake3: hex_lower(&hash),
                size: body.len() as u64,
            }]
        );
    }

    #[tokio::test]
    async fn staging_multipart_file_uses_write_store_and_records_mapping() {
        let read_inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let write_inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let store = Store::new(read_inner.clone())
            .with_staging_write_store("repo/staging/push-1".to_owned(), write_inner.clone());
        let canonical = Path::from("repo/xorbs/file-backed");
        let staged = Path::from("repo/staging/push-1/objects/repo/xorbs/file-backed");
        let body = Bytes::from_static(b"file-backed multipart body");
        let hash = *blake3::hash(&body).as_bytes();
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("payload.xorb");
        tokio::fs::write(&source, &body).await.unwrap();
        let cancel = tokio_util::sync::CancellationToken::new();

        store
            .put_multipart_file_retry(
                &canonical,
                &source,
                body.len() as u64,
                hash,
                5,
                &cancel,
                None,
            )
            .await
            .unwrap();

        let err = store
            .get_with_etag(&canonical)
            .await
            .expect_err("protected multipart upload must not write canonical object");
        assert!(
            matches!(err, StorageError::NotFound { .. }),
            "expected NotFound for canonical object, got {err:?}"
        );
        let got = write_inner
            .get(&staged)
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap();
        assert_eq!(got, body);
        assert_eq!(
            store.staged_writes(),
            vec![StagedWrite {
                canonical_key: canonical.to_string(),
                staged_key: staged.to_string(),
                blake3: hex_lower(&hash),
                size: body.len() as u64,
            }]
        );
    }

    #[tokio::test]
    async fn multipart_file_rejects_hash_mismatch_before_completion() {
        let store = memory_store();
        let path = Path::from("blobs/file-hash-mismatch");
        let body = Bytes::from_static(b"file body");
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("payload");
        tokio::fs::write(&source, &body).await.unwrap();
        let cancel = tokio_util::sync::CancellationToken::new();

        let error = store
            .put_multipart_file_retry(&path, &source, body.len() as u64, [0; 32], 3, &cancel, None)
            .await
            .expect_err("multipart file upload must verify its expected hash");
        assert!(matches!(error, StorageError::CorruptObject { .. }));
        assert!(matches!(
            store.head(&path).await,
            Err(StorageError::NotFound { .. })
        ));
    }

    #[tokio::test]
    async fn multipart_file_rejects_size_mismatch_before_upload() {
        let store = memory_store();
        let path = Path::from("blobs/file-size-mismatch");
        let body = Bytes::from_static(b"file body");
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("payload");
        tokio::fs::write(&source, &body).await.unwrap();
        let cancel = tokio_util::sync::CancellationToken::new();

        let error = store
            .put_multipart_file_retry(
                &path,
                &source,
                body.len() as u64 + 1,
                *blake3::hash(&body).as_bytes(),
                3,
                &cancel,
                None,
            )
            .await
            .expect_err("multipart file upload must verify its expected size");
        assert!(matches!(error, StorageError::CorruptObject { .. }));
        assert!(matches!(
            store.head(&path).await,
            Err(StorageError::NotFound { .. })
        ));
    }

    #[tokio::test]
    async fn multipart_upload_rejects_zero_part_size() {
        let store = memory_store();
        let cancel = tokio_util::sync::CancellationToken::new();
        let error = store
            .put_multipart_retry(
                &Path::from("blobs/zero-part"),
                Bytes::from_static(b"body"),
                0,
                &cancel,
                None,
            )
            .await
            .expect_err("zero-sized multipart parts are invalid");
        assert!(matches!(error, StorageError::Internal(_)));
    }

    #[tokio::test]
    async fn promote_staged_content_addressed_object_copies_missing_canonical() {
        let store = memory_store();
        let staged = Path::from("repo/staging/push-1/packs/tmp.pack");
        let canonical = Path::from("repo/packs/pack-a.pack");
        let body = Bytes::from_static(b"pack body");
        let hash = *blake3::hash(&body).as_bytes();
        store.put(&staged, body.clone()).await.unwrap();

        let promoted = store
            .promote_staged_content_addressed_object(&staged, &canonical, hash, body.len() as u64)
            .await
            .unwrap();

        assert!(promoted);
        let (got, _) = store.get_with_etag(&canonical).await.unwrap();
        assert_eq!(got, body);
    }

    #[tokio::test]
    async fn promote_staged_content_addressed_object_accepts_matching_canonical() {
        let store = memory_store();
        let staged = Path::from("repo/staging/push-1/packs/tmp.pack");
        let canonical = Path::from("repo/packs/pack-a.pack");
        let body = Bytes::from_static(b"pack body");
        let hash = *blake3::hash(&body).as_bytes();
        store.put(&staged, body.clone()).await.unwrap();
        store.put(&canonical, body.clone()).await.unwrap();

        let promoted = store
            .promote_staged_content_addressed_object(&staged, &canonical, hash, body.len() as u64)
            .await
            .unwrap();

        assert!(!promoted);
    }

    #[tokio::test]
    async fn promote_staged_content_addressed_object_rejects_different_canonical() {
        let store = memory_store();
        let staged = Path::from("repo/staging/push-1/packs/tmp.pack");
        let canonical = Path::from("repo/packs/pack-a.pack");
        let body = Bytes::from_static(b"pack body");
        let hash = *blake3::hash(&body).as_bytes();
        store.put(&staged, body).await.unwrap();
        store
            .put(&canonical, Bytes::from_static(b"different"))
            .await
            .unwrap();

        let err = store
            .promote_staged_content_addressed_object(&staged, &canonical, hash, 9)
            .await
            .expect_err("different canonical content must fail");

        assert!(
            matches!(err, StorageError::CorruptObject { .. }),
            "expected corrupt object, got {err:?}"
        );
    }

    #[tokio::test]
    async fn update_with_stale_etag_returns_cas_conflict() {
        let store = memory_store();
        let path = Path::from("refs/heads/main");

        store.put(&path, Bytes::from_static(b"v1")).await.unwrap();
        let (_body, etag) = store.get_with_etag(&path).await.unwrap();

        // Mutate with the fresh etag — succeeds and yields a new etag.
        let new_etag = store
            .update(&path, Bytes::from_static(b"v2"), etag.clone())
            .await
            .unwrap();
        assert_ne!(new_etag, etag);

        // Re-applying the original (now stale) etag must conflict.
        let err = store
            .update(&path, Bytes::from_static(b"v3"), etag)
            .await
            .expect_err("stale etag must not win CAS");
        assert!(
            matches!(err, StorageError::StateConflict { .. }),
            "expected CasConflict, got {err:?}"
        );
    }

    #[tokio::test]
    async fn verify_returns_bytes_when_hash_matches() {
        let store = memory_store();
        let path = Path::from("blobs/verified");
        let body = Bytes::from_static(b"trust but verify");
        let hash = *blake3::hash(&body).as_bytes();

        store.put(&path, body.clone()).await.unwrap();
        let got = store.verify(&path, &hash).await.unwrap();
        assert_eq!(got, body);
    }

    #[tokio::test]
    async fn verify_flags_corruption_when_hash_mismatches() {
        let store = memory_store();
        let path = Path::from("blobs/corrupt");
        let body = Bytes::from_static(b"actual content");
        let wrong_hash = [0u8; 32];

        store.put(&path, body).await.unwrap();
        let err = store
            .verify(&path, &wrong_hash)
            .await
            .expect_err("mismatched hash must surface as corruption");
        assert!(
            matches!(err, StorageError::CorruptObject { .. }),
            "expected CorruptObject, got {err:?}"
        );
    }

    #[tokio::test]
    async fn range_get_returns_requested_slice() {
        let store = memory_store();
        let path = Path::from("blobs/range");
        let body = Bytes::from_static(b"0123456789");

        store.put(&path, body).await.unwrap();
        let slice = store.range_get(&path, 2..7).await.unwrap();
        assert_eq!(slice.as_ref(), b"23456");
    }

    #[tokio::test]
    async fn read_request_observer_reports_bounded_kinds_without_paths() {
        let observed = Arc::new(Mutex::new(Vec::new()));
        let observer = Arc::clone(&observed);
        let store = memory_store().with_read_request_observer(Arc::new(move |kind| {
            observer.lock().expect("observer lock").push(kind);
        }));
        let path = Path::from("blobs/observed");
        store
            .put(&path, Bytes::from_static(b"observed"))
            .await
            .unwrap();

        store.head(&path).await.unwrap();
        store.range_get(&path, 0..4).await.unwrap();
        store.get_with_etag(&path).await.unwrap();

        assert_eq!(
            *observed.lock().expect("observer lock"),
            [
                StorageReadKind::Head,
                StorageReadKind::Range,
                StorageReadKind::Get,
            ]
        );
    }

    #[tokio::test]
    async fn streamed_range_preserves_metadata_and_accounts_consumed_bytes() {
        let bytes_read = Arc::new(AtomicU64::new(0));
        let observer = bytes_read.clone();
        let store = memory_store().with_read_byte_observer(Arc::new(move |bytes| {
            observer.fetch_add(bytes, Ordering::Relaxed);
        }));
        let path = Path::from("blobs/streamed-range");
        store
            .put(&path, Bytes::from_static(b"0123456789"))
            .await
            .unwrap();

        let (meta, range, stream) = store.get_stream(&path, Some(2..7)).await.unwrap();
        let chunks: Vec<Bytes> = stream.try_collect().await.unwrap();
        let body = chunks.into_iter().flatten().collect::<Vec<_>>();

        assert_eq!(meta.size, 10);
        assert_eq!(range, 2..7);
        assert_eq!(body, b"23456");
        assert_eq!(bytes_read.load(Ordering::Relaxed), 5);
    }

    #[tokio::test]
    async fn bounded_listing_stops_before_buffering_an_unbounded_prefix() {
        let store = memory_store();
        let prefix = Path::from("bounded");
        for key in ["bounded/a", "bounded/b"] {
            store
                .put(&Path::from(key), Bytes::from_static(b"x"))
                .await
                .unwrap();
        }

        assert!(
            store
                .list_prefix_bounded(&prefix, 1)
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(
            store
                .list_prefix_bounded(&prefix, 2)
                .await
                .unwrap()
                .map(|objects| objects.len()),
            Some(2)
        );
    }

    #[tokio::test]
    async fn read_byte_observer_counts_successful_body_reads() {
        let bytes_read = Arc::new(AtomicU64::new(0));
        let observer = bytes_read.clone();
        let store = memory_store().with_read_byte_observer(Arc::new(move |bytes| {
            observer.fetch_add(bytes, Ordering::Relaxed);
        }));
        let path = Path::from("blobs/accounted");
        let body = Bytes::from_static(b"0123456789");
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("downloaded");

        store.put(&path, body).await.unwrap();
        let (whole, _etag) = store.get_with_etag(&path).await.unwrap();
        let slice = store.range_get(&path, 2..7).await.unwrap();
        let downloaded = store.download_to_path(&path, &dest).await.unwrap();

        assert_eq!(whole.len() as u64 + slice.len() as u64 + downloaded, 25);
        assert_eq!(bytes_read.load(Ordering::Relaxed), 25);
    }

    #[tokio::test]
    async fn head_returns_metadata_for_existing_object() {
        let store = memory_store();
        let path = Path::from("blobs/head");
        let body = Bytes::from_static(b"meta check");

        store.put(&path, body.clone()).await.unwrap();
        let meta = store.head(&path).await.unwrap();
        assert_eq!(meta.location, path);
        assert_eq!(meta.size, body.len() as u64);
    }

    #[tokio::test]
    async fn delete_removes_object() {
        let store = memory_store();
        let path = Path::from("blobs/bye");

        store.put(&path, Bytes::from_static(b"x")).await.unwrap();
        store.delete(&path).await.unwrap();

        let err = store
            .get_with_etag(&path)
            .await
            .expect_err("deleted object must not be readable");
        assert!(
            matches!(err, StorageError::NotFound { .. }),
            "expected NotFound, got {err:?}"
        );
    }

    // --- BucketIdentity ---

    #[test]
    fn bucket_identity_equal_across_case_and_trailing_slash() {
        // The same bucket written five different ways — host case,
        // container case, trailing slashes — must produce equal
        // identities. This is what makes same-bucket detection work
        // across `s3://My-Bucket` vs `crab://my-bucket/` vs
        // `S3://MY-BUCKET/`.
        let canonical = BucketIdentity::new(StorageProviderKind::S3, "my-bucket", "my-bucket");
        let variants = [
            BucketIdentity::new(StorageProviderKind::S3, "My-Bucket", "my-bucket"),
            BucketIdentity::new(StorageProviderKind::S3, "my-bucket/", "my-bucket"),
            BucketIdentity::new(StorageProviderKind::S3, "my-bucket", "MY-BUCKET/"),
            BucketIdentity::new(StorageProviderKind::S3, "MY-BUCKET/", "My-Bucket/"),
        ];
        for v in &variants {
            assert_eq!(&canonical, v, "expected equality for variant {v:?}");
        }
    }

    #[test]
    fn bucket_identity_differs_across_clouds_for_same_name() {
        // A bucket literally named `foo` exists in every cloud. Same-
        // bucket detection must never cross clouds, so the cloud field
        // has to participate in equality.
        let s3 = BucketIdentity::new(StorageProviderKind::S3, "foo", "foo");
        let gcs = BucketIdentity::new(StorageProviderKind::Gcs, "foo", "foo");
        let azure = BucketIdentity::new(StorageProviderKind::Azure, "foo", "foo");
        let local = BucketIdentity::new(StorageProviderKind::Local, "foo", "foo");

        assert_ne!(s3, gcs);
        assert_ne!(s3, azure);
        assert_ne!(s3, local);
        assert_ne!(gcs, azure);
        assert_ne!(gcs, local);
        assert_ne!(azure, local);
    }

    #[test]
    fn bucket_identity_hash_dedups_in_set() {
        // Hash stability under normalization: a HashSet must collapse
        // case/slash variants into a single entry, and treat different
        // clouds as distinct.
        use std::collections::HashSet;

        let mut set: HashSet<BucketIdentity> = HashSet::new();
        set.insert(BucketIdentity::new(
            StorageProviderKind::S3,
            "my-bucket",
            "my-bucket",
        ));
        set.insert(BucketIdentity::new(
            StorageProviderKind::S3,
            "MY-BUCKET",
            "my-bucket",
        ));
        set.insert(BucketIdentity::new(
            StorageProviderKind::S3,
            "my-bucket/",
            "MY-BUCKET",
        ));
        // Cross-cloud twin must NOT dedup.
        set.insert(BucketIdentity::new(
            StorageProviderKind::Gcs,
            "my-bucket",
            "my-bucket",
        ));

        assert_eq!(set.len(), 2, "expected 2 unique identities, got {set:?}");
    }

    #[test]
    fn store_bucket_identity_defaults_to_local_unset() {
        // Test constructors don't set an identity; callers that care
        // get the sentinel Local identity, not a surprise S3 value.
        let store = memory_store();
        assert_eq!(store.bucket_identity(), BucketIdentity::local_unset());
    }

    #[test]
    fn store_with_bucket_identity_is_preserved() {
        // Builder-set identity survives through the fluent chain so
        // provider construction can attach identity and callers read
        // it back unchanged.
        let identity = BucketIdentity::new(StorageProviderKind::S3, "my-bucket", "my-bucket");
        let store = memory_store().with_bucket_identity(identity.clone());
        assert_eq!(store.bucket_identity(), identity);
    }

    #[tokio::test]
    async fn signed_url_without_signer_returns_unsupported_internal() {
        // Stores built without `with_signer` (every path except S3
        // provider construction) must fail `signed_url` with a
        // descriptive `Internal("presign unsupported...")` so the SDK
        // layer can map it into `Error::UrlUnsupported`. The exact
        // message is not load-bearing but the marker substring is —
        // the SDK matches on it.
        let store = memory_store();
        let path = Path::from("objects/artifact.bin");
        let err = store
            .signed_url(&path, std::time::Duration::from_secs(60))
            .await
            .expect_err("memory store cannot presign — must error");
        match err {
            StorageError::Internal(msg) => assert!(
                msg.contains("presign unsupported"),
                "expected 'presign unsupported' marker, got {msg:?}"
            ),
            other => panic!("expected Internal, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod resumable_multipart_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use crate::multipart::{JournalPart, MultipartJournal, ResumeInfo};
    use object_store::memory::InMemory;
    use object_store::multipart::{MultipartStore, PartId};
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct Row {
        upload_id: String,
        parts: Vec<JournalPart>,
        completed: bool,
    }

    #[derive(Default)]
    struct MemJournal {
        rows: Mutex<HashMap<Vec<u8>, Row>>,
    }

    impl MemJournal {
        fn seed(&self, hash: &[u8], upload_id: &str, parts: Vec<JournalPart>) {
            self.rows.lock().unwrap().insert(
                hash.to_vec(),
                Row {
                    upload_id: upload_id.to_owned(),
                    parts,
                    completed: false,
                },
            );
        }

        fn row(&self, hash: &[u8]) -> Option<(String, Vec<JournalPart>)> {
            self.rows
                .lock()
                .unwrap()
                .get(hash)
                .filter(|row| !row.completed)
                .map(|row| (row.upload_id.clone(), row.parts.clone()))
        }
    }

    impl MultipartJournal for MemJournal {
        fn begin(
            &self,
            payload_hash: &[u8],
            _bucket: &str,
            _key: &str,
            upload_id: &str,
        ) -> crate::multipart::JournalResult<bool> {
            let mut rows = self.rows.lock().unwrap();
            if rows
                .get(payload_hash)
                .is_some_and(|row| !row.completed && row.upload_id != upload_id)
            {
                return Ok(false);
            }
            rows.insert(
                payload_hash.to_vec(),
                Row {
                    upload_id: upload_id.to_owned(),
                    parts: Vec::new(),
                    completed: false,
                },
            );
            Ok(true)
        }

        fn record_part(
            &self,
            upload_id: &str,
            part_idx: usize,
            content_id: &str,
            size: u64,
        ) -> crate::multipart::JournalResult<()> {
            let mut rows = self.rows.lock().unwrap();
            let Some(row) = rows.values_mut().find(|row| row.upload_id == upload_id) else {
                return Ok(());
            };
            row.parts.retain(|part| part.part_idx != part_idx);
            row.parts.push(JournalPart {
                part_idx,
                content_id: content_id.to_owned(),
                size,
            });
            Ok(())
        }

        fn complete(&self, upload_id: &str) -> crate::multipart::JournalResult<()> {
            self.rows
                .lock()
                .unwrap()
                .retain(|_, row| row.upload_id != upload_id);
            Ok(())
        }

        fn abort_stale(&self, upload_id: &str) -> crate::multipart::JournalResult<()> {
            self.rows
                .lock()
                .unwrap()
                .retain(|_, row| row.upload_id != upload_id);
            Ok(())
        }

        fn resumable(
            &self,
            payload_hash: &[u8],
            _bucket: &str,
            _key: &str,
        ) -> crate::multipart::JournalResult<Option<ResumeInfo>> {
            Ok(self
                .row(payload_hash)
                .map(|(upload_id, parts)| ResumeInfo { upload_id, parts }))
        }
    }

    /// Counts `put_part` calls and can fail a specific part index once.
    struct CountingParts {
        inner: Arc<InMemory>,
        put_part_calls: AtomicU64,
        fail_idx_once: AtomicU64,
        create_calls: AtomicU64,
        abort_fails: bool,
    }

    #[async_trait::async_trait]
    impl MultipartStore for CountingParts {
        async fn create_multipart(
            &self,
            path: &Path,
        ) -> object_store::Result<object_store::MultipartId> {
            self.create_calls.fetch_add(1, Ordering::Relaxed);
            self.inner.create_multipart(path).await
        }

        async fn put_part(
            &self,
            path: &Path,
            id: &object_store::MultipartId,
            part_idx: usize,
            data: object_store::PutPayload,
        ) -> object_store::Result<PartId> {
            let armed = self.fail_idx_once.load(Ordering::Relaxed);
            if armed != u64::MAX
                && armed - 1 == part_idx as u64
                && self
                    .fail_idx_once
                    .compare_exchange(armed, u64::MAX, Ordering::Relaxed, Ordering::Relaxed)
                    .is_ok()
            {
                return Err(object_store::Error::Generic {
                    store: "test",
                    source: "injected failure".into(),
                });
            }
            self.put_part_calls.fetch_add(1, Ordering::Relaxed);
            self.inner.put_part(path, id, part_idx, data).await
        }

        async fn complete_multipart(
            &self,
            path: &Path,
            id: &object_store::MultipartId,
            parts: Vec<PartId>,
        ) -> object_store::Result<object_store::PutResult> {
            self.inner.complete_multipart(path, id, parts).await
        }

        async fn abort_multipart(
            &self,
            path: &Path,
            id: &object_store::MultipartId,
        ) -> object_store::Result<()> {
            if self.abort_fails {
                return Err(object_store::Error::Generic {
                    store: "test",
                    source: "injected abort failure".into(),
                });
            }
            self.inner.abort_multipart(path, id).await
        }
    }

    impl CountingParts {
        fn fail_once(&self, idx: usize) {
            self.fail_idx_once.store(idx as u64 + 1, Ordering::Relaxed);
        }
    }

    const PART_SIZE: usize = 16;
    const HASH: [u8; 4] = *b"hash";

    fn temp_file(len: usize, fill: u8) -> (tempfile::NamedTempFile, Vec<u8>) {
        use std::io::Write as _;
        let data = vec![fill; len];
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(&data).unwrap();
        (file, data)
    }

    async fn uploaded_bytes(store: &Store, path: &Path, len: usize) -> Vec<u8> {
        let (_, _, stream) = store.get_stream(path, Some(0..len as u64)).await.unwrap();
        let mut out = Vec::new();
        let mut stream = std::pin::pin!(stream);
        while let Some(chunk) = stream.next().await {
            out.extend_from_slice(&chunk.unwrap());
        }
        out
    }

    #[tokio::test]
    async fn fresh_session_journals_parts_and_completes() {
        let inner = Arc::new(InMemory::new());
        let counting = Arc::new(CountingParts {
            inner: inner.clone(),
            put_part_calls: AtomicU64::new(0),
            fail_idx_once: AtomicU64::new(u64::MAX),
            create_calls: AtomicU64::new(0),
            abort_fails: false,
        });
        let journal = MemJournal::default();
        let store = Store::new(inner.clone() as Arc<dyn ObjectStore>)
            .with_multipart(counting.clone() as Arc<dyn object_store::multipart::MultipartStore>);
        let (file, data) = temp_file(PART_SIZE * 3 - 2, 0xAB);
        let path = Path::from("xet/xorbs/aa/aabb");

        store
            .put_multipart_file_resumable_retry(
                &path,
                file.path(),
                data.len() as u64,
                [7; 32],
                &HASH,
                PART_SIZE,
                &tokio_util::sync::CancellationToken::new(),
                None,
                Some(&journal),
            )
            .await
            .unwrap();

        assert_eq!(counting.create_calls.load(Ordering::Relaxed), 1);
        assert_eq!(
            counting.put_part_calls.load(Ordering::Relaxed),
            3,
            "all three parts uploaded exactly once"
        );
        assert!(journal.row(&HASH).is_none(), "row cleared on completion");
        assert_eq!(uploaded_bytes(&store, &path, data.len()).await, data);
    }

    #[tokio::test]
    async fn recorded_prefix_is_not_reuploaded() {
        let inner = Arc::new(InMemory::new());
        let counting = Arc::new(CountingParts {
            inner: inner.clone(),
            put_part_calls: AtomicU64::new(0),
            fail_idx_once: AtomicU64::new(u64::MAX),
            create_calls: AtomicU64::new(0),
            abort_fails: false,
        });
        let journal = MemJournal::default();
        let store = Store::new(inner.clone() as Arc<dyn ObjectStore>)
            .with_multipart(counting.clone() as Arc<dyn object_store::multipart::MultipartStore>);
        let (file, data) = temp_file(PART_SIZE * 2 + 5, 0x5A);
        let path = Path::from("xet/xorbs/bb/bbcc");

        // Simulate a killed process: session exists server-side with part
        // 0 already stored, journaled under the same payload hash.
        let live_id = counting.create_multipart(&path).await.unwrap();
        let part0 = counting
            .put_part(&path, &live_id, 0, data[..PART_SIZE].to_vec().into())
            .await
            .unwrap();
        let seeded_calls = counting.put_part_calls.load(Ordering::Relaxed);
        let seeded_creates = counting.create_calls.load(Ordering::Relaxed);
        journal.seed(
            &HASH,
            &live_id,
            vec![JournalPart {
                part_idx: 0,
                content_id: part0.content_id,
                size: PART_SIZE as u64,
            }],
        );

        store
            .put_multipart_file_resumable_retry(
                &path,
                file.path(),
                data.len() as u64,
                [7; 32],
                &HASH,
                PART_SIZE,
                &tokio_util::sync::CancellationToken::new(),
                None,
                Some(&journal),
            )
            .await
            .unwrap();

        assert_eq!(
            counting.put_part_calls.load(Ordering::Relaxed) - seeded_calls,
            2,
            "only the two missing suffix parts are uploaded"
        );
        assert_eq!(
            counting.create_calls.load(Ordering::Relaxed) - seeded_creates,
            0,
            "session reused"
        );
        assert!(journal.row(&HASH).is_none());
        assert_eq!(uploaded_bytes(&store, &path, data.len()).await, data);
    }

    #[tokio::test]
    async fn incompatible_recorded_row_is_discarded_then_fresh_upload_succeeds() {
        let inner = Arc::new(InMemory::new());
        let counting = Arc::new(CountingParts {
            inner: inner.clone(),
            put_part_calls: AtomicU64::new(0),
            fail_idx_once: AtomicU64::new(u64::MAX),
            create_calls: AtomicU64::new(0),
            abort_fails: false,
        });
        let journal = MemJournal::default();
        let store = Store::new(inner.clone() as Arc<dyn ObjectStore>)
            .with_multipart(counting.clone() as Arc<dyn object_store::multipart::MultipartStore>);
        let (file, data) = temp_file(PART_SIZE * 2, 0x11);
        let path = Path::from("xet/xorbs/cc/cacc");

        // Boundary drift: non-final part smaller than the current plan.
        journal.seed(
            &HASH,
            "stale-id",
            vec![JournalPart {
                part_idx: 0,
                content_id: "etag".to_owned(),
                size: (PART_SIZE / 2) as u64,
            }],
        );

        store
            .put_multipart_file_resumable_retry(
                &path,
                file.path(),
                data.len() as u64,
                [7; 32],
                &HASH,
                PART_SIZE,
                &tokio_util::sync::CancellationToken::new(),
                None,
                Some(&journal),
            )
            .await
            .unwrap();

        assert_eq!(
            counting.put_part_calls.load(Ordering::Relaxed),
            2,
            "fresh full upload after discarding incompatible row"
        );
        assert_eq!(counting.create_calls.load(Ordering::Relaxed), 1);
        assert_eq!(uploaded_bytes(&store, &path, data.len()).await, data);
    }

    #[tokio::test]
    async fn injected_part_failure_recovers_on_fresh_retry_attempt() {
        let inner = Arc::new(InMemory::new());
        let counting = Arc::new(CountingParts {
            inner: inner.clone(),
            put_part_calls: AtomicU64::new(0),
            fail_idx_once: AtomicU64::new(u64::MAX),
            create_calls: AtomicU64::new(0),
            abort_fails: false,
        });
        let journal = MemJournal::default();
        let store = Store::new(inner.clone() as Arc<dyn ObjectStore>)
            .with_multipart(counting.clone() as Arc<dyn object_store::multipart::MultipartStore>);
        let (file, data) = temp_file(PART_SIZE * 2, 0x77);
        let path = Path::from("xet/xorbs/dd/dadd");
        counting.fail_once(1);

        store
            .put_multipart_file_resumable_retry(
                &path,
                file.path(),
                data.len() as u64,
                [7; 32],
                &HASH,
                PART_SIZE,
                &tokio_util::sync::CancellationToken::new(),
                None,
                Some(&journal),
            )
            .await
            .unwrap();

        assert!(
            counting.create_calls.load(Ordering::Relaxed) >= 2,
            "hard failure is cleaned up before retry opens a new session"
        );
        assert_eq!(uploaded_bytes(&store, &path, data.len()).await, data);
        assert!(
            journal.row(&HASH).is_none(),
            "final attempt completed its row"
        );
    }

    #[tokio::test]
    async fn failed_provider_abort_preserves_recovery_row() {
        let inner = Arc::new(InMemory::new());
        let counting = Arc::new(CountingParts {
            inner: inner.clone(),
            put_part_calls: AtomicU64::new(0),
            fail_idx_once: AtomicU64::new(u64::MAX),
            create_calls: AtomicU64::new(0),
            abort_fails: true,
        });
        counting.fail_once(0);
        let journal = MemJournal::default();
        let store = Store::with_retry(
            inner as Arc<dyn ObjectStore>,
            RetryPolicy {
                max_attempts: 1,
                base: std::time::Duration::ZERO,
                cap: std::time::Duration::ZERO,
            },
        )
        .with_multipart(counting as Arc<dyn object_store::multipart::MultipartStore>);
        let (file, data) = temp_file(PART_SIZE * 2, 0x44);
        let path = Path::from("xet/xorbs/ff/fa11");

        store
            .put_multipart_file_resumable_retry(
                &path,
                file.path(),
                data.len() as u64,
                [7; 32],
                &HASH,
                PART_SIZE,
                &tokio_util::sync::CancellationToken::new(),
                None,
                Some(&journal),
            )
            .await
            .unwrap_err();

        assert!(journal.row(&HASH).is_some());
    }

    #[tokio::test]
    async fn unjournaled_session_aborts_when_concurrent_row_stands_down() {
        let inner = Arc::new(InMemory::new());
        let counting = Arc::new(CountingParts {
            inner: inner.clone(),
            put_part_calls: AtomicU64::new(0),
            fail_idx_once: AtomicU64::new(u64::MAX),
            create_calls: AtomicU64::new(0),
            abort_fails: false,
        });
        let journal = MemJournal::default();
        let store = Store::new(inner.clone() as Arc<dyn ObjectStore>)
            .with_multipart(counting.clone() as Arc<dyn object_store::multipart::MultipartStore>);
        let (file, data) = temp_file(PART_SIZE * 2, 0x33);
        let path = Path::from("xet/xorbs/ee/eadd");

        // Another uploader owns the payload-hash row.
        journal.seed(&HASH, "other-uploader", Vec::new());

        let cancel = tokio_util::sync::CancellationToken::new();
        cancel.cancel();

        let err = store
            .put_multipart_file_resumable_retry(
                &path,
                file.path(),
                data.len() as u64,
                [7; 32],
                &HASH,
                PART_SIZE,
                &cancel,
                None,
                Some(&journal),
            )
            .await
            .unwrap_err();

        assert!(matches!(err, StorageError::Cancelled));
        assert_eq!(
            counting.put_part_calls.load(Ordering::Relaxed),
            0,
            "stand-down session cancelled before dispatching parts"
        );
        // The concurrent row survives untouched.
        assert_eq!(journal.row(&HASH).unwrap().0, "other-uploader");
    }
}
