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

use std::collections::HashSet;
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
    /// Low-level provider handle with stable explicit upload IDs.
    ///
    /// Present for S3 and GCS, including refreshing wrappers. Azure and
    /// generic `ObjectStore` implementations stay on whole-upload retry.
    multipart: Option<Arc<dyn object_store::multipart::MultipartStore>>,
    /// Exact physical destination identity for explicit multipart sessions.
    ///
    /// This is deliberately separate from `identity`: bucket identity drives
    /// cross-scheme equality and provider SDK selection, while resumable
    /// sessions must also distinguish custom endpoints.
    multipart_identity: Option<BucketIdentity>,
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
            multipart_identity: None,
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
            multipart_identity: None,
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

    /// Attaches the provider's explicit multipart API.
    ///
    /// The handle must address the same physical store as `inner`; otherwise
    /// journal identity and provider sessions would diverge.
    #[must_use]
    pub fn with_multipart(
        mut self,
        multipart: Arc<dyn object_store::multipart::MultipartStore>,
        identity: BucketIdentity,
    ) -> Self {
        self.multipart = Some(multipart);
        self.multipart_identity = Some(identity);
        self
    }

    #[must_use]
    pub fn has_resumable_multipart(&self) -> bool {
        self.multipart.is_some()
            && self.multipart_identity.is_some()
            && self.staging_writes.is_none()
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
    /// content already occupies `path`. Providers that cannot honor
    /// create-only writes fail closed; Crab never retries them as an
    /// unconditional overwrite.
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
        retry(&self.retry, || async {
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
        })
        .await
    }

    /// Streams the physical destination selected for a write and verifies it.
    ///
    /// Protected pushes write beneath an unpublished staging prefix, so a
    /// canonical read cannot prove those bytes before publication.
    pub async fn verify_written_size_and_hash(
        &self,
        path: &Path,
        expected_size: u64,
        expected_hash: &[u8; 32],
    ) -> Result<()> {
        let (write_path, write_inner, _) = self.exact_write_target(path);
        retry(&self.retry, || async {
            let meta = write_inner
                .head(&write_path)
                .await
                .map_err(|error| map_object_store_error(error, write_path.as_ref()))?;
            if meta.size != expected_size {
                return Err(StorageError::CorruptObject {
                    path: write_path.to_string(),
                    reason: format!("expected {expected_size} bytes, found {}", meta.size),
                });
            }
            if Self::matches_in_streaming(&write_inner, &write_path, expected_hash).await? {
                Ok(())
            } else {
                Err(StorageError::CorruptObject {
                    path: write_path.to_string(),
                    reason: format!(
                        "Blake3 content hash did not match {}",
                        hex_lower(expected_hash)
                    ),
                })
            }
        })
        .await
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
        retry(&self.retry, || async {
            match self.head(canonical).await {
                Ok(meta) if meta.size == expected_size => {
                    Self::matches_in_streaming(
                        &self.read_inner_for(canonical),
                        canonical,
                        expected_hash,
                    )
                    .await
                }
                Ok(_) => Ok(false),
                Err(StorageError::NotFound { .. }) => Ok(false),
                Err(e) => Err(e),
            }
        })
        .await
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
            let item = item.map_err(|error| map_object_store_error(error, prefix.as_ref()))?;
            if objects.len() == limit {
                return Ok(None);
            }
            objects.push(item);
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

    /// Uploads a content-addressed file with exclusive, durable part resume.
    ///
    /// The source is fully size/hash-verified before a provider session is
    /// claimed. An active owner is waited out rather than bypassed, every part
    /// mutation renews and checks the lease, and the completed remote body is
    /// streamed back through Blake3 verification before the journal row is
    /// removed.
    #[allow(clippy::too_many_arguments)]
    pub async fn put_multipart_file_resumable(
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
    ) -> Result<crate::multipart::ResumableUploadOutcome> {
        if part_size == 0 {
            return Err(StorageError::Internal(
                "multipart part size must be greater than zero".to_owned(),
            ));
        }
        Self::verify_local_file(file_path, size, &expected_hash, cancel).await?;

        let (Some(multipart), Some(journal), Some(target)) = (
            self.multipart.as_ref(),
            journal,
            self.multipart_target(path),
        ) else {
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
            self.verify_written_size_and_hash(path, size, &expected_hash)
                .await?;
            return Ok(crate::multipart::ResumableUploadOutcome::Uploaded);
        };
        if self.canonical_matches(path, &expected_hash, size).await? {
            return Ok(crate::multipart::ResumableUploadOutcome::AlreadyPresent);
        }

        let owner_token = random_owner_token();
        let mut repair_attempt = 0_u8;
        let mut credited_parts = HashSet::new();
        loop {
            if cancel.is_cancelled() {
                return Err(StorageError::Cancelled);
            }
            let claim = loop {
                match journal_call(
                    "claim",
                    journal
                        .claim(
                            &target,
                            payload_hash,
                            &expected_hash,
                            size,
                            part_size,
                            &owner_token,
                            unix_now(),
                            MULTIPART_LEASE_DURATION,
                        )
                        .await,
                )? {
                    crate::multipart::JournalClaimOutcome::Acquired(claim) => break claim,
                    crate::multipart::JournalClaimOutcome::Busy => {
                        tokio::select! {
                            () = tokio::time::sleep(MULTIPART_CLAIM_POLL) => {}
                            () = cancel.cancelled() => return Err(StorageError::Cancelled),
                        }
                    }
                }
            };

            let canonical_matches = await_with_heartbeat(
                journal,
                &claim.lease,
                cancel,
                self.canonical_matches(path, &expected_hash, size),
            )
            .await?;
            let canonical_matches = match canonical_matches {
                Ok(matches) => matches,
                Err(error) => {
                    release_journal(journal, &claim.lease).await;
                    return Err(error);
                }
            };
            if canonical_matches {
                let provider_state_released = match claim.upload_id.as_deref() {
                    Some(upload_id) => match await_with_heartbeat(
                        journal,
                        &claim.lease,
                        cancel,
                        self.abort_explicit_multipart(path, upload_id),
                    )
                    .await?
                    {
                        Ok(()) => true,
                        Err(error) => {
                            tracing::warn!(
                                path = %path,
                                error = %error,
                                "canonical object is complete but stale multipart cleanup failed"
                            );
                            false
                        }
                    },
                    None => true,
                };
                if provider_state_released {
                    require_journal_owner(
                        path,
                        "complete already-present upload",
                        journal.complete_owned(&claim.lease, unix_now()).await,
                    )?;
                } else {
                    release_journal(journal, &claim.lease).await;
                }
                return Ok(crate::multipart::ResumableUploadOutcome::AlreadyPresent);
            }

            let plan_matches = claim.payload_hash == payload_hash
                && claim.expected_hash == expected_hash
                && crate::multipart::compatible_parts(&claim, size, part_size).is_some()
                && (claim.upload_id.is_some() || claim.parts.is_empty());
            let mut claim = claim;
            if !plan_matches {
                if let Some(upload_id) = claim.upload_id.as_deref()
                    && let Err(error) = await_with_heartbeat(
                        journal,
                        &claim.lease,
                        cancel,
                        self.abort_explicit_multipart(path, upload_id),
                    )
                    .await?
                {
                    release_journal(journal, &claim.lease).await;
                    return Err(error);
                }
                require_journal_owner(
                    path,
                    "reset",
                    journal
                        .reset_owned(
                            &claim.lease,
                            payload_hash,
                            &expected_hash,
                            size,
                            part_size,
                            unix_now(),
                            MULTIPART_LEASE_DURATION,
                        )
                        .await,
                )?;
                claim.upload_id = None;
                claim.payload_hash = payload_hash.to_vec();
                claim.expected_hash = expected_hash;
                claim.size = size;
                claim.part_size = part_size;
                claim.parts.clear();
            }

            let resumed = claim.upload_id.is_some();
            let upload_id = match claim.upload_id.clone() {
                Some(upload_id) => upload_id,
                None => {
                    let created = await_with_heartbeat(
                        journal,
                        &claim.lease,
                        cancel,
                        multipart.create_multipart(path),
                    )
                    .await?;
                    let upload_id = match created {
                        Ok(upload_id) => upload_id,
                        Err(error) => {
                            release_journal(journal, &claim.lease).await;
                            return Err(map_object_store_error(error, path.as_ref()));
                        }
                    };
                    if let Err(error) = require_journal_owner(
                        path,
                        "bind upload",
                        journal
                            .bind_upload(
                                &claim.lease,
                                upload_id.as_ref(),
                                unix_now(),
                                MULTIPART_LEASE_DURATION,
                            )
                            .await,
                    ) {
                        if let Err(abort_error) = self
                            .abort_explicit_multipart(path, upload_id.as_ref())
                            .await
                        {
                            tracing::warn!(
                                path = %path,
                                error = %abort_error,
                                "failed to abort multipart session after journal bind failure"
                            );
                        }
                        release_journal(journal, &claim.lease).await;
                        return Err(error);
                    }
                    upload_id.to_string()
                }
            };

            let slots =
                crate::multipart::compatible_parts(&claim, size, part_size).ok_or_else(|| {
                    StorageError::Internal("multipart plan changed after reset".into())
                })?;
            let uploaded = await_with_heartbeat(
                journal,
                &claim.lease,
                cancel,
                self.upload_missing_parts(
                    multipart,
                    journal,
                    &claim.lease,
                    path,
                    file_path,
                    size,
                    expected_hash,
                    part_size,
                    &upload_id,
                    slots,
                    &mut credited_parts,
                    cancel,
                    on_part_done,
                ),
            )
            .await?;
            match uploaded {
                Ok(parts) => {
                    let provider_parts = parts
                        .iter()
                        .map(|part| object_store::multipart::PartId {
                            content_id: part.content_id.clone(),
                        })
                        .collect();
                    let completed = await_with_heartbeat(
                        journal,
                        &claim.lease,
                        cancel,
                        multipart.complete_multipart(
                            path,
                            &object_store::MultipartId::from(upload_id.as_str()),
                            provider_parts,
                        ),
                    )
                    .await?;
                    if let Err(provider_error) = completed {
                        if await_with_heartbeat(
                            journal,
                            &claim.lease,
                            cancel,
                            self.verify_size_and_hash(path, size, &expected_hash),
                        )
                        .await?
                        .is_ok()
                        {
                            require_journal_owner(
                                path,
                                "complete after uncertain provider response",
                                journal.complete_owned(&claim.lease, unix_now()).await,
                            )?;
                            return Ok(if resumed {
                                crate::multipart::ResumableUploadOutcome::Resumed
                            } else {
                                crate::multipart::ResumableUploadOutcome::Uploaded
                            });
                        }
                        if matches!(provider_error, object_store::Error::NotFound { .. }) {
                            require_journal_owner(
                                path,
                                "drop missing provider session",
                                journal.abandon_owned(&claim.lease, unix_now()).await,
                            )?;
                            if repair_attempt == 0 {
                                repair_attempt += 1;
                                continue;
                            }
                        } else {
                            release_journal(journal, &claim.lease).await;
                        }
                        return Err(map_object_store_error(provider_error, path.as_ref()));
                    }

                    match await_with_heartbeat(
                        journal,
                        &claim.lease,
                        cancel,
                        self.verify_size_and_hash(path, size, &expected_hash),
                    )
                    .await?
                    {
                        Ok(()) => {
                            require_journal_owner(
                                path,
                                "complete",
                                journal.complete_owned(&claim.lease, unix_now()).await,
                            )?;
                            return Ok(if resumed {
                                crate::multipart::ResumableUploadOutcome::Resumed
                            } else {
                                crate::multipart::ResumableUploadOutcome::Uploaded
                            });
                        }
                        Err(error) => {
                            require_journal_owner(
                                path,
                                "drop corrupt completed session",
                                journal.abandon_owned(&claim.lease, unix_now()).await,
                            )?;
                            if repair_attempt == 0 {
                                repair_attempt += 1;
                                continue;
                            }
                            return Err(error);
                        }
                    }
                }
                Err(error) => {
                    if matches!(error, StorageError::NotFound { .. }) {
                        // Providers return NotFound when a recorded upload ID
                        // was expired or externally aborted. Drop that exact
                        // owned row so it cannot trap every future retry.
                        require_journal_owner(
                            path,
                            "drop missing provider session",
                            journal.abandon_owned(&claim.lease, unix_now()).await,
                        )?;
                        if repair_attempt == 0 {
                            repair_attempt += 1;
                            continue;
                        }
                        return Err(error);
                    }
                    if matches!(
                        error,
                        StorageError::CorruptObject { .. } | StorageError::Io { .. }
                    ) {
                        match await_with_heartbeat(
                            journal,
                            &claim.lease,
                            cancel,
                            self.abort_explicit_multipart(path, &upload_id),
                        )
                        .await?
                        {
                            Ok(()) => require_journal_owner(
                                path,
                                "abandon changed local source",
                                journal.abandon_owned(&claim.lease, unix_now()).await,
                            )?,
                            Err(abort_error) => {
                                release_journal(journal, &claim.lease).await;
                                tracing::warn!(
                                    path = %path,
                                    error = %abort_error,
                                    "failed to abort multipart session after local source changed"
                                );
                            }
                        }
                        return Err(error);
                    }
                    release_journal(journal, &claim.lease).await;
                    return Err(error);
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn upload_missing_parts(
        &self,
        multipart: &Arc<dyn object_store::multipart::MultipartStore>,
        journal: &dyn crate::multipart::MultipartJournal,
        lease: &crate::multipart::JournalLease,
        path: &Path,
        file_path: &std::path::Path,
        size: u64,
        expected_hash: [u8; 32],
        part_size: usize,
        upload_id: &str,
        mut slots: Vec<Option<crate::multipart::JournalPart>>,
        credited_parts: &mut HashSet<usize>,
        cancel: &tokio_util::sync::CancellationToken,
        on_part_done: Option<&(dyn Fn(u64) + Send + Sync)>,
    ) -> Result<Vec<crate::multipart::JournalPart>> {
        use futures_util::stream::{FuturesUnordered, StreamExt};

        const IN_FLIGHT_PARTS: usize = 4;

        let mut file = tokio::fs::File::open(file_path).await?;
        let mut hasher = blake3::Hasher::new();
        let mut remaining = size;
        let mut part_idx = 0_usize;
        let mut pending = FuturesUnordered::new();
        while remaining > 0 {
            if cancel.is_cancelled() {
                return Err(StorageError::Cancelled);
            }
            while pending.len() >= IN_FLIGHT_PARTS {
                let completed = pending.next().await;
                let (idx, bytes, result) = completed.ok_or_else(|| {
                    StorageError::Internal("multipart part queue ended while non-empty".into())
                })?;
                let provider_part: object_store::multipart::PartId = result?;
                let part = crate::multipart::JournalPart {
                    part_idx: idx,
                    content_id: provider_part.content_id,
                    size: bytes,
                };
                require_journal_owner(
                    path,
                    "record part",
                    journal
                        .record_part(lease, &part, unix_now(), MULTIPART_LEASE_DURATION)
                        .await,
                )?;
                if let Some(callback) = on_part_done
                    && credited_parts.insert(idx)
                {
                    callback(bytes);
                }
                slots[idx] = Some(part);
            }

            let want = remaining.min(part_size as u64) as usize;
            let mut buffer = vec![0_u8; want];
            file.read_exact(&mut buffer).await?;
            hasher.update(&buffer);
            if let Some(part) = &slots[part_idx] {
                if let Some(callback) = on_part_done
                    && credited_parts.insert(part_idx)
                {
                    callback(part.size);
                }
            } else {
                let multipart = Arc::clone(multipart);
                let retry_policy = self.retry;
                let path = path.clone();
                let upload_id = object_store::MultipartId::from(upload_id);
                let payload: object_store::PutPayload = bytes::Bytes::from(buffer).into();
                let bytes = want as u64;
                pending.push(async move {
                    let result = retry(&retry_policy, || {
                        let multipart = Arc::clone(&multipart);
                        let path = path.clone();
                        let upload_id = upload_id.clone();
                        let payload = payload.clone();
                        async move {
                            multipart
                                .put_part(&path, &upload_id, part_idx, payload)
                                .await
                                .map_err(|error| map_object_store_error(error, path.as_ref()))
                        }
                    })
                    .await;
                    (part_idx, bytes, result)
                });
            }
            remaining -= want as u64;
            part_idx += 1;
        }

        while !pending.is_empty() {
            let completed = pending.next().await;
            let (idx, bytes, result) = completed.ok_or_else(|| {
                StorageError::Internal("multipart part queue ended while non-empty".into())
            })?;
            let provider_part: object_store::multipart::PartId = result?;
            let part = crate::multipart::JournalPart {
                part_idx: idx,
                content_id: provider_part.content_id,
                size: bytes,
            };
            require_journal_owner(
                path,
                "record part",
                journal
                    .record_part(lease, &part, unix_now(), MULTIPART_LEASE_DURATION)
                    .await,
            )?;
            if let Some(callback) = on_part_done
                && credited_parts.insert(idx)
            {
                callback(bytes);
            }
            slots[idx] = Some(part);
        }

        let actual_hash = *hasher.finalize().as_bytes();
        if actual_hash != expected_hash {
            return Err(StorageError::CorruptObject {
                path: file_path.display().to_string(),
                reason: format!(
                    "local blake3 hash {} does not match expected {}",
                    hex_lower(&actual_hash),
                    hex_lower(&expected_hash)
                ),
            });
        }
        if file.metadata().await?.len() != size {
            return Err(StorageError::CorruptObject {
                path: file_path.display().to_string(),
                reason: "local file size changed during multipart upload".to_owned(),
            });
        }
        slots
            .into_iter()
            .enumerate()
            .map(|(idx, part)| {
                part.ok_or_else(|| {
                    StorageError::Internal(format!(
                        "multipart plan is missing completed part {idx}"
                    ))
                })
            })
            .collect()
    }

    async fn verify_local_file(
        file_path: &std::path::Path,
        size: u64,
        expected_hash: &[u8; 32],
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<()> {
        let mut file = tokio::fs::File::open(file_path).await?;
        let actual_size = file.metadata().await?.len();
        if actual_size != size {
            return Err(StorageError::CorruptObject {
                path: file_path.display().to_string(),
                reason: format!("local file has {actual_size} bytes; upload expects {size}"),
            });
        }
        let mut remaining = size;
        let mut buffer = vec![0_u8; 1024 * 1024];
        let mut hasher = blake3::Hasher::new();
        while remaining > 0 {
            if cancel.is_cancelled() {
                return Err(StorageError::Cancelled);
            }
            let want = remaining.min(buffer.len() as u64) as usize;
            file.read_exact(&mut buffer[..want]).await?;
            hasher.update(&buffer[..want]);
            remaining -= want as u64;
        }
        let actual_hash = *hasher.finalize().as_bytes();
        if actual_hash != *expected_hash {
            return Err(StorageError::CorruptObject {
                path: file_path.display().to_string(),
                reason: format!(
                    "local blake3 hash {} does not match expected {}",
                    hex_lower(&actual_hash),
                    hex_lower(expected_hash)
                ),
            });
        }
        Ok(())
    }

    #[must_use]
    pub fn multipart_target(&self, path: &Path) -> Option<crate::multipart::MultipartTarget> {
        if self.multipart.is_none() || self.staging_writes.is_some() {
            return None;
        }
        let identity = self.multipart_identity.as_ref()?;
        let provider = match identity.cloud {
            crate::identity::StorageProviderKind::S3 => "s3",
            crate::identity::StorageProviderKind::Gcs => "gcs",
            crate::identity::StorageProviderKind::Azure => "azure",
            crate::identity::StorageProviderKind::Local => "local",
        };
        Some(crate::multipart::MultipartTarget {
            provider: provider.to_owned(),
            host: identity.host.clone(),
            container: identity.container.clone(),
            key: path.to_string(),
        })
    }

    pub async fn abort_explicit_multipart(&self, path: &Path, upload_id: &str) -> Result<()> {
        let multipart = self
            .multipart
            .as_ref()
            .ok_or_else(|| StorageError::NotSupported {
                source: object_store::Error::NotSupported {
                    source: "store has no stable multipart upload-id API".into(),
                },
            })?;
        retry(&self.retry, || async {
            match multipart
                .abort_multipart(path, &object_store::MultipartId::from(upload_id))
                .await
            {
                Ok(()) | Err(object_store::Error::NotFound { .. }) => Ok(()),
                Err(error) => Err(map_object_store_error(error, path.as_ref())),
            }
        })
        .await
    }

    /// One full multipart-upload attempt: create, write parts with bounded
    /// in-flight concurrency (progress-aware) or via `WriteMultipart`
    /// (sequential), then complete. Any error aborts the upload so S3 does
    /// not retain orphaned parts.
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

const MULTIPART_LEASE_DURATION: std::time::Duration = std::time::Duration::from_secs(60);
const MULTIPART_HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);
const MULTIPART_CLAIM_POLL: std::time::Duration = std::time::Duration::from_millis(250);

async fn await_with_heartbeat<F, T>(
    journal: &dyn crate::multipart::MultipartJournal,
    lease: &crate::multipart::JournalLease,
    cancel: &tokio_util::sync::CancellationToken,
    future: F,
) -> Result<T>
where
    F: std::future::Future<Output = T>,
{
    if cancel.is_cancelled() {
        release_journal(journal, lease).await;
        return Err(StorageError::Cancelled);
    }
    // A disk read or a paused process can outlive its lease between phases.
    // Revalidate before polling a provider future, not only on a later tick.
    if let Err(error) = require_journal_owner(
        &Path::from(lease.entry_id.as_str()),
        "renew lease",
        journal
            .renew(lease, unix_now(), MULTIPART_LEASE_DURATION)
            .await,
    ) {
        release_journal(journal, lease).await;
        return Err(error);
    }
    let result = {
        tokio::pin!(future);
        let start = tokio::time::Instant::now() + MULTIPART_HEARTBEAT_INTERVAL;
        let mut heartbeat = tokio::time::interval_at(start, MULTIPART_HEARTBEAT_INTERVAL);
        loop {
            tokio::select! {
                output = &mut future => break Ok(output),
                () = cancel.cancelled() => break Err(StorageError::Cancelled),
                _ = heartbeat.tick() => {
                    if let Err(error) = require_journal_owner(
                        &Path::from(lease.entry_id.as_str()),
                        "renew lease",
                        journal.renew(
                            lease,
                            unix_now(),
                            MULTIPART_LEASE_DURATION,
                        )
                        .await,
                    ) {
                        break Err(error);
                    }
                }
            }
        }
    };
    if result.is_err() {
        // Drop the transport future before another process can acquire the
        // released lease and start using the recorded provider upload ID.
        release_journal(journal, lease).await;
    }
    result
}

fn journal_call<T>(
    operation: &'static str,
    result: crate::multipart::JournalResult<T>,
) -> Result<T> {
    result.map_err(|source| StorageError::MultipartJournal { operation, source })
}

fn require_journal_owner(
    path: &Path,
    operation: &'static str,
    result: crate::multipart::JournalResult<bool>,
) -> Result<()> {
    if journal_call(operation, result)? {
        Ok(())
    } else {
        Err(StorageError::StateConflict {
            path: path.to_string(),
        })
    }
}

async fn release_journal(
    journal: &dyn crate::multipart::MultipartJournal,
    lease: &crate::multipart::JournalLease,
) {
    match journal.release_owned(lease, unix_now()).await {
        Ok(true) => {}
        Ok(false) => tracing::debug!(
            entry_id = %lease.entry_id,
            "multipart lease already changed while releasing"
        ),
        Err(error) => tracing::warn!(
            entry_id = %lease.entry_id,
            error = %error,
            "failed to release multipart lease"
        ),
    }
}

fn random_owner_token() -> String {
    use rand::Rng as _;
    format!("{:032x}", rand::rng().random::<u128>())
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_secs()).ok())
        .unwrap_or(0)
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
    use futures_util::{TryStreamExt as _, stream::BoxStream};
    use object_store::memory::InMemory;
    use object_store::multipart::{MultipartStore, PartId};
    use object_store::{
        CopyOptions, GetOptions, GetResult, ListResult, MultipartId, PutMultipartOptions,
        PutOptions, PutPayload, PutResult,
    };
    use std::fmt;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    #[derive(Default)]
    struct TestJournal {
        row: std::sync::Mutex<Option<TestJournalRow>>,
        claims: AtomicU64,
        renewals: AtomicU64,
    }

    struct TestJournalRow {
        target: crate::multipart::MultipartTarget,
        claim: crate::multipart::JournalClaim,
        expires_at: i64,
    }

    impl TestJournal {
        fn owned_mut<'a>(
            row: &'a mut Option<TestJournalRow>,
            lease: &crate::multipart::JournalLease,
            now: i64,
        ) -> Option<&'a mut TestJournalRow> {
            row.as_mut()
                .filter(|row| row.claim.lease == *lease && row.expires_at > now)
        }

        fn deadline(now: i64, duration: Duration) -> i64 {
            now.saturating_add(i64::try_from(duration.as_secs()).unwrap_or(i64::MAX))
        }
    }

    #[async_trait::async_trait]
    impl crate::multipart::MultipartJournal for TestJournal {
        async fn claim(
            &self,
            target: &crate::multipart::MultipartTarget,
            payload_hash: &[u8],
            expected_hash: &[u8; 32],
            size: u64,
            part_size: usize,
            owner_token: &str,
            now: i64,
            lease_duration: Duration,
        ) -> crate::multipart::JournalResult<crate::multipart::JournalClaimOutcome> {
            self.claims.fetch_add(1, Ordering::Relaxed);
            let mut row = self.row.lock().unwrap();
            if let Some(existing) = row.as_mut() {
                assert_eq!(&existing.target, target);
                if existing.expires_at > now && existing.claim.lease.owner_token != owner_token {
                    return Ok(crate::multipart::JournalClaimOutcome::Busy);
                }
                existing.claim.lease.owner_token = owner_token.to_owned();
                existing.expires_at = Self::deadline(now, lease_duration);
                return Ok(crate::multipart::JournalClaimOutcome::Acquired(
                    existing.claim.clone(),
                ));
            }
            let claim = crate::multipart::JournalClaim {
                lease: crate::multipart::JournalLease {
                    entry_id: "entry".to_owned(),
                    owner_token: owner_token.to_owned(),
                },
                upload_id: None,
                payload_hash: payload_hash.to_vec(),
                expected_hash: *expected_hash,
                size,
                part_size,
                parts: Vec::new(),
            };
            *row = Some(TestJournalRow {
                target: target.clone(),
                claim: claim.clone(),
                expires_at: Self::deadline(now, lease_duration),
            });
            Ok(crate::multipart::JournalClaimOutcome::Acquired(claim))
        }

        async fn bind_upload(
            &self,
            lease: &crate::multipart::JournalLease,
            upload_id: &str,
            now: i64,
            lease_duration: Duration,
        ) -> crate::multipart::JournalResult<bool> {
            let mut row = self.row.lock().unwrap();
            let Some(row) = Self::owned_mut(&mut row, lease, now) else {
                return Ok(false);
            };
            row.claim.upload_id = Some(upload_id.to_owned());
            row.expires_at = Self::deadline(now, lease_duration);
            Ok(true)
        }

        async fn renew(
            &self,
            lease: &crate::multipart::JournalLease,
            now: i64,
            lease_duration: Duration,
        ) -> crate::multipart::JournalResult<bool> {
            self.renewals.fetch_add(1, Ordering::Relaxed);
            let mut row = self.row.lock().unwrap();
            let Some(row) = Self::owned_mut(&mut row, lease, now) else {
                return Ok(false);
            };
            row.expires_at = Self::deadline(now, lease_duration);
            Ok(true)
        }

        async fn record_part(
            &self,
            lease: &crate::multipart::JournalLease,
            part: &crate::multipart::JournalPart,
            now: i64,
            lease_duration: Duration,
        ) -> crate::multipart::JournalResult<bool> {
            let mut row = self.row.lock().unwrap();
            let Some(row) = Self::owned_mut(&mut row, lease, now) else {
                return Ok(false);
            };
            row.claim
                .parts
                .retain(|saved| saved.part_idx != part.part_idx);
            row.claim.parts.push(part.clone());
            row.expires_at = Self::deadline(now, lease_duration);
            Ok(true)
        }

        async fn reset_owned(
            &self,
            lease: &crate::multipart::JournalLease,
            payload_hash: &[u8],
            expected_hash: &[u8; 32],
            size: u64,
            part_size: usize,
            now: i64,
            lease_duration: Duration,
        ) -> crate::multipart::JournalResult<bool> {
            let mut row = self.row.lock().unwrap();
            let Some(row) = Self::owned_mut(&mut row, lease, now) else {
                return Ok(false);
            };
            row.claim.upload_id = None;
            row.claim.payload_hash = payload_hash.to_vec();
            row.claim.expected_hash = *expected_hash;
            row.claim.size = size;
            row.claim.part_size = part_size;
            row.claim.parts.clear();
            row.expires_at = Self::deadline(now, lease_duration);
            Ok(true)
        }

        async fn complete_owned(
            &self,
            lease: &crate::multipart::JournalLease,
            now: i64,
        ) -> crate::multipart::JournalResult<bool> {
            let mut row = self.row.lock().unwrap();
            if Self::owned_mut(&mut row, lease, now).is_none() {
                return Ok(false);
            }
            *row = None;
            Ok(true)
        }

        async fn abandon_owned(
            &self,
            lease: &crate::multipart::JournalLease,
            now: i64,
        ) -> crate::multipart::JournalResult<bool> {
            let mut row = self.row.lock().unwrap();
            if Self::owned_mut(&mut row, lease, now).is_none() {
                return Ok(false);
            }
            *row = None;
            Ok(true)
        }

        async fn release_owned(
            &self,
            lease: &crate::multipart::JournalLease,
            now: i64,
        ) -> crate::multipart::JournalResult<bool> {
            let mut row = self.row.lock().unwrap();
            let Some(row) = row.as_mut().filter(|row| row.claim.lease == *lease) else {
                return Ok(false);
            };
            row.expires_at = now;
            Ok(true)
        }
    }

    #[derive(Clone, Copy)]
    enum CompleteBehavior {
        Normal,
        ErrorAfterCommit,
        CorruptAfterCommit,
    }

    struct TestMultipartStore {
        inner: Arc<InMemory>,
        behavior: CompleteBehavior,
        gate_first_part: bool,
        part_started: Arc<tokio::sync::Semaphore>,
        part_release: Arc<tokio::sync::Semaphore>,
        creates: AtomicU64,
        parts: AtomicU64,
        completes: AtomicU64,
        aborts: AtomicU64,
        abort_failures: AtomicU64,
        missing_upload_ids: std::sync::Mutex<HashSet<String>>,
    }

    #[derive(Debug)]
    struct FailFirstBodyGetStore {
        inner: Arc<InMemory>,
        remaining_failures: AtomicU64,
    }

    impl fmt::Display for FailFirstBodyGetStore {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("fail-first-body-get")
        }
    }

    #[async_trait::async_trait]
    impl ObjectStore for FailFirstBodyGetStore {
        async fn put_opts(
            &self,
            location: &Path,
            payload: PutPayload,
            options: PutOptions,
        ) -> object_store::Result<PutResult> {
            self.inner.put_opts(location, payload, options).await
        }

        async fn put_multipart_opts(
            &self,
            location: &Path,
            options: PutMultipartOptions,
        ) -> object_store::Result<Box<dyn MultipartUpload>> {
            self.inner.put_multipart_opts(location, options).await
        }

        async fn get_opts(
            &self,
            location: &Path,
            options: GetOptions,
        ) -> object_store::Result<GetResult> {
            if !options.head
                && self
                    .remaining_failures
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                        remaining.checked_sub(1)
                    })
                    .is_ok()
            {
                return Err(object_store::Error::Generic {
                    store: "test",
                    source: "transient body read".into(),
                });
            }
            self.inner.get_opts(location, options).await
        }

        fn delete_stream(
            &self,
            locations: BoxStream<'static, object_store::Result<Path>>,
        ) -> BoxStream<'static, object_store::Result<Path>> {
            self.inner.delete_stream(locations)
        }

        fn list(
            &self,
            prefix: Option<&Path>,
        ) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
            self.inner.list(prefix)
        }

        async fn list_with_delimiter(
            &self,
            prefix: Option<&Path>,
        ) -> object_store::Result<ListResult> {
            self.inner.list_with_delimiter(prefix).await
        }

        async fn copy_opts(
            &self,
            from: &Path,
            to: &Path,
            options: CopyOptions,
        ) -> object_store::Result<()> {
            self.inner.copy_opts(from, to, options).await
        }
    }

    impl TestMultipartStore {
        fn new(inner: Arc<InMemory>, behavior: CompleteBehavior, gate_first_part: bool) -> Self {
            Self {
                inner,
                behavior,
                gate_first_part,
                part_started: Arc::new(tokio::sync::Semaphore::new(0)),
                part_release: Arc::new(tokio::sync::Semaphore::new(0)),
                creates: AtomicU64::new(0),
                parts: AtomicU64::new(0),
                completes: AtomicU64::new(0),
                aborts: AtomicU64::new(0),
                abort_failures: AtomicU64::new(0),
                missing_upload_ids: std::sync::Mutex::new(HashSet::new()),
            }
        }
    }

    #[async_trait::async_trait]
    impl MultipartStore for TestMultipartStore {
        async fn create_multipart(&self, path: &Path) -> object_store::Result<MultipartId> {
            self.creates.fetch_add(1, Ordering::Relaxed);
            self.inner.create_multipart(path).await
        }

        async fn put_part(
            &self,
            path: &Path,
            id: &MultipartId,
            part_idx: usize,
            data: PutPayload,
        ) -> object_store::Result<PartId> {
            if self
                .missing_upload_ids
                .lock()
                .unwrap()
                .contains(id.as_str())
            {
                return Err(object_store::Error::NotFound {
                    path: path.to_string(),
                    source: "provider multipart session is absent".into(),
                });
            }
            let call = self.parts.fetch_add(1, Ordering::Relaxed);
            if self.gate_first_part && call == 0 {
                self.part_started.add_permits(1);
                let permit = self.part_release.acquire().await.map_err(|_| {
                    object_store::Error::Generic {
                        store: "test",
                        source: "multipart test gate closed".into(),
                    }
                })?;
                permit.forget();
            }
            self.inner.put_part(path, id, part_idx, data).await
        }

        async fn complete_multipart(
            &self,
            path: &Path,
            id: &MultipartId,
            parts: Vec<PartId>,
        ) -> object_store::Result<PutResult> {
            self.completes.fetch_add(1, Ordering::Relaxed);
            let result = self.inner.complete_multipart(path, id, parts).await?;
            match self.behavior {
                CompleteBehavior::Normal => Ok(result),
                CompleteBehavior::ErrorAfterCommit => Err(object_store::Error::Generic {
                    store: "test",
                    source: "response lost after commit".into(),
                }),
                CompleteBehavior::CorruptAfterCommit => {
                    self.inner
                        .put(path, Bytes::from_static(b"corrupt").into())
                        .await?;
                    Ok(result)
                }
            }
        }

        async fn abort_multipart(&self, path: &Path, id: &MultipartId) -> object_store::Result<()> {
            self.aborts.fetch_add(1, Ordering::Relaxed);
            if self
                .abort_failures
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |remaining| {
                    remaining.checked_sub(1)
                })
                .is_ok()
            {
                return Err(object_store::Error::Generic {
                    store: "test",
                    source: "transient abort failure".into(),
                });
            }
            self.inner.abort_multipart(path, id).await
        }
    }

    fn resumable_store(multipart: Arc<TestMultipartStore>) -> Store {
        let inner: Arc<dyn ObjectStore> = multipart.inner.clone();
        let multipart_handle: Arc<dyn MultipartStore> = multipart;
        let identity = BucketIdentity::new(StorageProviderKind::S3, "s3.example.test", "bucket");
        memory_store_with_inner(inner)
            .with_bucket_identity(identity.clone())
            .with_multipart(multipart_handle, identity)
    }

    fn memory_store_with_inner(inner: Arc<dyn ObjectStore>) -> Store {
        Store::with_retry(
            inner,
            RetryPolicy {
                max_attempts: 2,
                base: Duration::from_millis(1),
                cap: Duration::from_millis(5),
            },
        )
    }

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
    async fn heartbeat_refuses_to_poll_provider_after_ownership_is_lost() {
        let journal = TestJournal::default();
        let lease = crate::multipart::JournalLease {
            entry_id: "retired-entry".into(),
            owner_token: "former-owner".into(),
        };
        let provider_calls = AtomicU64::new(0);
        let result = await_with_heartbeat(
            &journal,
            &lease,
            &tokio_util::sync::CancellationToken::new(),
            async { provider_calls.fetch_add(1, Ordering::Relaxed) },
        )
        .await;

        assert!(matches!(result, Err(StorageError::StateConflict { .. })));
        assert_eq!(provider_calls.load(Ordering::Relaxed), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn heartbeat_renews_ownership_through_a_slow_phase() {
        use crate::multipart::MultipartJournal as _;

        let journal = TestJournal::default();
        let target = crate::multipart::MultipartTarget {
            provider: "s3".into(),
            host: "endpoint".into(),
            container: "bucket".into(),
            key: "key".into(),
        };
        let crate::multipart::JournalClaimOutcome::Acquired(claim) = journal
            .claim(
                &target,
                &[0; 32],
                &[0; 32],
                0,
                8,
                "owner",
                unix_now(),
                MULTIPART_LEASE_DURATION,
            )
            .await
            .unwrap()
        else {
            panic!("fresh journal must be acquirable");
        };
        await_with_heartbeat(
            &journal,
            &claim.lease,
            &tokio_util::sync::CancellationToken::new(),
            tokio::time::sleep(MULTIPART_HEARTBEAT_INTERVAL * 2 + Duration::from_secs(1)),
        )
        .await
        .unwrap();

        assert_eq!(journal.renewals.load(Ordering::Relaxed), 3);
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
    async fn resumable_upload_rejects_hash_before_claim_or_provider_creation() {
        let inner = Arc::new(InMemory::new());
        let multipart = Arc::new(TestMultipartStore::new(
            inner,
            CompleteBehavior::Normal,
            false,
        ));
        let store = resumable_store(Arc::clone(&multipart));
        let journal = TestJournal::default();
        let body = b"verified before provider state";
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("payload");
        tokio::fs::write(&source, body).await.unwrap();

        let error = store
            .put_multipart_file_resumable(
                &Path::from("blobs/preverified"),
                &source,
                body.len() as u64,
                [9; 32],
                b"payload",
                5,
                &tokio_util::sync::CancellationToken::new(),
                None,
                Some(&journal),
            )
            .await
            .expect_err("wrong expected hash must fail before provider state exists");

        assert!(matches!(error, StorageError::CorruptObject { .. }));
        assert_eq!(journal.claims.load(Ordering::Relaxed), 0);
        assert_eq!(multipart.creates.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn resumable_existing_object_retries_transient_body_read() {
        let inner = Arc::new(InMemory::new());
        let body = Bytes::from_static(b"already durable");
        let path = Path::from("blobs/existing-after-transient-read");
        inner.put(&path, body.clone().into()).await.unwrap();
        let read_store: Arc<dyn ObjectStore> = Arc::new(FailFirstBodyGetStore {
            inner: Arc::clone(&inner),
            remaining_failures: AtomicU64::new(1),
        });
        let multipart: Arc<dyn MultipartStore> = inner;
        let identity = BucketIdentity::new(StorageProviderKind::S3, "s3.example.test", "bucket");
        let store = memory_store_with_inner(read_store)
            .with_bucket_identity(identity.clone())
            .with_multipart(multipart, identity);
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("payload");
        tokio::fs::write(&source, &body).await.unwrap();
        let journal = TestJournal::default();
        let hash = *blake3::hash(&body).as_bytes();

        let outcome = store
            .put_multipart_file_resumable(
                &path,
                &source,
                body.len() as u64,
                hash,
                &hash,
                8,
                &tokio_util::sync::CancellationToken::new(),
                None,
                Some(&journal),
            )
            .await
            .unwrap();

        assert_eq!(
            outcome,
            crate::multipart::ResumableUploadOutcome::AlreadyPresent
        );
        assert_eq!(journal.claims.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn explicit_multipart_abort_retries_transient_failure() {
        let inner = Arc::new(InMemory::new());
        let multipart = Arc::new(TestMultipartStore::new(
            inner,
            CompleteBehavior::Normal,
            false,
        ));
        let store = resumable_store(Arc::clone(&multipart));
        let path = Path::from("blobs/abort-retry");
        let upload_id = multipart.create_multipart(&path).await.unwrap();
        multipart.abort_failures.store(1, Ordering::Relaxed);

        store
            .abort_explicit_multipart(&path, upload_id.as_ref())
            .await
            .unwrap();

        assert_eq!(multipart.aborts.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn resumable_upload_reuses_recorded_provider_parts() {
        use crate::multipart::MultipartJournal as _;

        let inner = Arc::new(InMemory::new());
        let multipart = Arc::new(TestMultipartStore::new(
            inner,
            CompleteBehavior::Normal,
            false,
        ));
        let store = resumable_store(Arc::clone(&multipart));
        let journal = TestJournal::default();
        let path = Path::from("blobs/resume");
        let body = b"resume exact provider parts";
        let hash = *blake3::hash(body).as_bytes();
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("payload");
        tokio::fs::write(&source, body).await.unwrap();
        let target = store.multipart_target(&path).unwrap();
        let now = unix_now();
        let claim = match journal
            .claim(
                &target,
                &hash,
                &hash,
                body.len() as u64,
                8,
                "first-owner",
                now,
                MULTIPART_LEASE_DURATION,
            )
            .await
            .unwrap()
        {
            crate::multipart::JournalClaimOutcome::Acquired(claim) => claim,
            crate::multipart::JournalClaimOutcome::Busy => panic!("new journal must be acquirable"),
        };
        let upload_id = multipart.create_multipart(&path).await.unwrap();
        assert!(
            journal
                .bind_upload(
                    &claim.lease,
                    upload_id.as_ref(),
                    now,
                    MULTIPART_LEASE_DURATION,
                )
                .await
                .unwrap()
        );
        let provider_part = multipart
            .put_part(
                &path,
                &upload_id,
                0,
                Bytes::copy_from_slice(&body[..8]).into(),
            )
            .await
            .unwrap();
        assert!(
            journal
                .record_part(
                    &claim.lease,
                    &crate::multipart::JournalPart {
                        part_idx: 0,
                        content_id: provider_part.content_id,
                        size: 8,
                    },
                    now,
                    MULTIPART_LEASE_DURATION,
                )
                .await
                .unwrap()
        );
        assert!(journal.release_owned(&claim.lease, now).await.unwrap());

        let outcome = store
            .put_multipart_file_resumable(
                &path,
                &source,
                body.len() as u64,
                hash,
                &hash,
                8,
                &tokio_util::sync::CancellationToken::new(),
                None,
                Some(&journal),
            )
            .await
            .unwrap();

        assert_eq!(outcome, crate::multipart::ResumableUploadOutcome::Resumed);
        assert_eq!(multipart.creates.load(Ordering::Relaxed), 1);
        assert_eq!(multipart.parts.load(Ordering::Relaxed), 4);
        assert_eq!(
            store.get_with_etag(&path).await.unwrap().0,
            Bytes::copy_from_slice(body)
        );
    }

    #[tokio::test]
    async fn resumable_upload_replaces_missing_provider_session() {
        use crate::multipart::MultipartJournal as _;

        let inner = Arc::new(InMemory::new());
        let multipart = Arc::new(TestMultipartStore::new(
            inner,
            CompleteBehavior::Normal,
            false,
        ));
        let store = resumable_store(Arc::clone(&multipart));
        let journal = TestJournal::default();
        let path = Path::from("blobs/provider-session-lost");
        let body = b"provider session vanishes after one durable part";
        let hash = *blake3::hash(body).as_bytes();
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("payload");
        tokio::fs::write(&source, body).await.unwrap();
        let target = store.multipart_target(&path).unwrap();
        let now = unix_now();
        let claim = match journal
            .claim(
                &target,
                &hash,
                &hash,
                body.len() as u64,
                8,
                "first-owner",
                now,
                MULTIPART_LEASE_DURATION,
            )
            .await
            .unwrap()
        {
            crate::multipart::JournalClaimOutcome::Acquired(claim) => claim,
            crate::multipart::JournalClaimOutcome::Busy => panic!("new journal must be acquirable"),
        };
        let lost_upload_id = multipart.create_multipart(&path).await.unwrap();
        assert!(
            journal
                .bind_upload(
                    &claim.lease,
                    lost_upload_id.as_ref(),
                    now,
                    MULTIPART_LEASE_DURATION,
                )
                .await
                .unwrap()
        );
        let provider_part = multipart
            .put_part(
                &path,
                &lost_upload_id,
                0,
                Bytes::copy_from_slice(&body[..8]).into(),
            )
            .await
            .unwrap();
        assert!(
            journal
                .record_part(
                    &claim.lease,
                    &crate::multipart::JournalPart {
                        part_idx: 0,
                        content_id: provider_part.content_id,
                        size: 8,
                    },
                    now,
                    MULTIPART_LEASE_DURATION,
                )
                .await
                .unwrap()
        );
        assert!(journal.release_owned(&claim.lease, now).await.unwrap());
        multipart
            .abort_multipart(&path, &lost_upload_id)
            .await
            .unwrap();
        multipart
            .missing_upload_ids
            .lock()
            .unwrap()
            .insert(lost_upload_id.to_string());
        let credited = AtomicU64::new(0);
        let on_part = |bytes| {
            credited.fetch_add(bytes, Ordering::Relaxed);
        };

        let outcome = store
            .put_multipart_file_resumable(
                &path,
                &source,
                body.len() as u64,
                hash,
                &hash,
                8,
                &tokio_util::sync::CancellationToken::new(),
                Some(&on_part),
                Some(&journal),
            )
            .await
            .unwrap();

        assert_eq!(outcome, crate::multipart::ResumableUploadOutcome::Uploaded);
        assert_eq!(multipart.creates.load(Ordering::Relaxed), 2);
        assert_eq!(credited.load(Ordering::Relaxed), body.len() as u64);
        assert_eq!(
            store.get_with_etag(&path).await.unwrap().0,
            Bytes::copy_from_slice(body)
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_resumable_uploads_have_one_provider_owner() {
        use crate::multipart::MultipartJournal as _;

        for resume_existing in [false, true] {
            let inner = Arc::new(InMemory::new());
            let multipart = Arc::new(TestMultipartStore::new(
                inner,
                CompleteBehavior::Normal,
                true,
            ));
            let store = resumable_store(Arc::clone(&multipart));
            let journal = Arc::new(TestJournal::default());
            let body = b"one owner completes this upload";
            let hash = *blake3::hash(body).as_bytes();
            let dir = tempfile::tempdir().unwrap();
            let source = dir.path().join("payload");
            tokio::fs::write(&source, body).await.unwrap();
            let path = Path::from("blobs/concurrent");

            if resume_existing {
                let target = store.multipart_target(&path).unwrap();
                let now = unix_now();
                let crate::multipart::JournalClaimOutcome::Acquired(claim) = journal
                    .claim(
                        &target,
                        &hash,
                        &hash,
                        body.len() as u64,
                        64,
                        "original",
                        now,
                        MULTIPART_LEASE_DURATION,
                    )
                    .await
                    .unwrap()
                else {
                    panic!("new journal must be acquirable")
                };
                let upload_id = multipart.create_multipart(&path).await.unwrap();
                assert!(
                    journal
                        .bind_upload(&claim.lease, &upload_id, now, MULTIPART_LEASE_DURATION)
                        .await
                        .unwrap()
                );
                assert!(journal.release_owned(&claim.lease, now).await.unwrap());
            }

            let first = {
                let store = store.clone();
                let journal = Arc::clone(&journal);
                let source = source.clone();
                let path = path.clone();
                tokio::spawn(async move {
                    store
                        .put_multipart_file_resumable(
                            &path,
                            &source,
                            body.len() as u64,
                            hash,
                            &hash,
                            64,
                            &tokio_util::sync::CancellationToken::new(),
                            None,
                            Some(journal.as_ref()),
                        )
                        .await
                })
            };
            let started = multipart.part_started.acquire().await.unwrap();
            started.forget();
            let claims_before_second = journal.claims.load(Ordering::Relaxed);
            let second = {
                let store = store.clone();
                let journal = Arc::clone(&journal);
                let source = source.clone();
                let path = path.clone();
                tokio::spawn(async move {
                    store
                        .put_multipart_file_resumable(
                            &path,
                            &source,
                            body.len() as u64,
                            hash,
                            &hash,
                            64,
                            &tokio_util::sync::CancellationToken::new(),
                            None,
                            Some(journal.as_ref()),
                        )
                        .await
                })
            };
            tokio::time::timeout(Duration::from_secs(5), async {
                while journal.claims.load(Ordering::Relaxed) == claims_before_second {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            multipart.part_release.add_permits(1);
            let first = first.await.unwrap().unwrap();
            let second = second.await.unwrap().unwrap();

            let expected = if resume_existing {
                crate::multipart::ResumableUploadOutcome::Resumed
            } else {
                crate::multipart::ResumableUploadOutcome::Uploaded
            };
            assert_eq!(first, expected);
            assert_eq!(
                second,
                crate::multipart::ResumableUploadOutcome::AlreadyPresent
            );
            assert_eq!(multipart.creates.load(Ordering::Relaxed), 1);
            assert_eq!(multipart.completes.load(Ordering::Relaxed), 1);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancelled_upload_releases_lease_and_resumes_same_provider_session() {
        let inner = Arc::new(InMemory::new());
        let multipart = Arc::new(TestMultipartStore::new(
            inner,
            CompleteBehavior::Normal,
            true,
        ));
        let store = resumable_store(Arc::clone(&multipart));
        let journal = Arc::new(TestJournal::default());
        let body = b"interrupted upload resumes its provider session";
        let hash = *blake3::hash(body).as_bytes();
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("payload");
        tokio::fs::write(&source, body).await.unwrap();
        let path = Path::from("blobs/interrupted");
        let cancel = tokio_util::sync::CancellationToken::new();

        let interrupted = {
            let store = store.clone();
            let journal = Arc::clone(&journal);
            let source = source.clone();
            let path = path.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move {
                store
                    .put_multipart_file_resumable(
                        &path,
                        &source,
                        body.len() as u64,
                        hash,
                        &hash,
                        64,
                        &cancel,
                        None,
                        Some(journal.as_ref()),
                    )
                    .await
            })
        };
        let started = multipart.part_started.acquire().await.unwrap();
        started.forget();
        cancel.cancel();
        assert!(matches!(
            interrupted.await.unwrap(),
            Err(StorageError::Cancelled)
        ));
        assert!(journal.row.lock().unwrap().is_some());

        let outcome = store
            .put_multipart_file_resumable(
                &path,
                &source,
                body.len() as u64,
                hash,
                &hash,
                64,
                &tokio_util::sync::CancellationToken::new(),
                None,
                Some(journal.as_ref()),
            )
            .await
            .unwrap();

        assert_eq!(outcome, crate::multipart::ResumableUploadOutcome::Resumed);
        assert_eq!(multipart.creates.load(Ordering::Relaxed), 1);
        assert_eq!(multipart.completes.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn uncertain_completion_is_accepted_only_after_canonical_readback() {
        let inner = Arc::new(InMemory::new());
        let multipart = Arc::new(TestMultipartStore::new(
            inner,
            CompleteBehavior::ErrorAfterCommit,
            false,
        ));
        let store = resumable_store(multipart);
        let journal = TestJournal::default();
        let body = b"provider committed before response was lost";
        let hash = *blake3::hash(body).as_bytes();
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("payload");
        tokio::fs::write(&source, body).await.unwrap();

        let outcome = store
            .put_multipart_file_resumable(
                &Path::from("blobs/uncertain"),
                &source,
                body.len() as u64,
                hash,
                &hash,
                9,
                &tokio_util::sync::CancellationToken::new(),
                None,
                Some(&journal),
            )
            .await
            .unwrap();
        assert_eq!(outcome, crate::multipart::ResumableUploadOutcome::Uploaded);
        assert!(journal.row.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn corrupt_provider_completion_is_repaired_once_then_rejected() {
        let inner = Arc::new(InMemory::new());
        let multipart = Arc::new(TestMultipartStore::new(
            inner,
            CompleteBehavior::CorruptAfterCommit,
            false,
        ));
        let store = resumable_store(Arc::clone(&multipart));
        let journal = TestJournal::default();
        let body = b"provider returns the wrong canonical bytes";
        let hash = *blake3::hash(body).as_bytes();
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("payload");
        tokio::fs::write(&source, body).await.unwrap();

        let error = store
            .put_multipart_file_resumable(
                &Path::from("blobs/corrupt-complete"),
                &source,
                body.len() as u64,
                hash,
                &hash,
                11,
                &tokio_util::sync::CancellationToken::new(),
                None,
                Some(&journal),
            )
            .await
            .expect_err("two corrupt completions must not be accepted");

        assert!(matches!(error, StorageError::CorruptObject { .. }));
        assert_eq!(multipart.creates.load(Ordering::Relaxed), 2);
        assert_eq!(multipart.completes.load(Ordering::Relaxed), 2);
        assert!(journal.row.lock().unwrap().is_none());
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
