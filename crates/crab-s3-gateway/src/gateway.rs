use std::{
    collections::BTreeMap,
    pin::Pin,
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use base64::Engine as _;
use bytes::Bytes;
use crab_cache::LocalCache;
use crab_cache_store::{CacheConfig as StoreCacheConfig, CacheObserver, CachingStore};
use crab_git::pointer_detect::PointerKind;
use crab_remote_git::{
    ContentClassification, EntryKind, OperationKind, RemoteGitRuntime, RepositoryIdentity,
    RepositoryOptions,
};
use crab_storage::{StorageProviderKind, Store, StoreLayout, build_static_env_store};
use futures_util::StreamExt as _;
use s3s::{S3, S3Request, S3Response, S3Result, dto::*, s3_error};
use tokio_util::sync::CancellationToken;

use crate::{
    Config, RepositoryAccess, RepositoryConfig,
    admission::{Admission, RequestClass, RequestPermit},
    auth::GatewayAuth,
    metrics::{MaintenanceCycleOutcome, MaintenanceFailure, Metrics},
    mutation, namespace,
};

pub(crate) struct Repository {
    pub(crate) config: RepositoryConfig,
    pub(crate) store: Store,
    pub(crate) layout: StoreLayout<Store>,
    pub(crate) identity: RepositoryIdentity,
    pub(crate) read_views: crate::repository::ReadViewCache,
    pub(crate) maintenance: Arc<crate::repository::WriteMaintenance>,
    hydrator: crab_read::ShardHydrator,
    lfs: crab_lfs::LfsObjectStore,
}

impl Repository {
    #[cfg(test)]
    pub(crate) fn new_with_cache(
        config: RepositoryConfig,
        store: Store,
        local_cache: Arc<LocalCache>,
    ) -> crate::Result<Self> {
        let cache_config = StoreCacheConfig {
            max_bytes: local_cache.max_bytes(),
            ..StoreCacheConfig::default()
        };
        let caching = CachingStore::new_with_local_cache(store.clone(), cache_config, local_cache)?;
        Self::new_with_caching_store(config, store, caching)
    }

    pub(crate) fn new_with_cache_observer(
        config: RepositoryConfig,
        store: Store,
        local_cache: Arc<LocalCache>,
        observer: Arc<dyn CacheObserver>,
    ) -> crate::Result<Self> {
        let cache_config = StoreCacheConfig {
            max_bytes: local_cache.max_bytes(),
            ..StoreCacheConfig::default()
        };
        let caching = CachingStore::new_with_local_cache(store.clone(), cache_config, local_cache)?
            .with_cache_observer(observer);
        Self::new_with_caching_store(config, store, caching)
    }

    fn new_with_caching_store(
        config: RepositoryConfig,
        store: Store,
        caching: CachingStore,
    ) -> crate::Result<Self> {
        let layout = StoreLayout::new(store.clone(), config.prefix.clone());
        let read_layout = crab_read::ReadStoreLayout::with_global_prefix(
            store.clone(),
            layout.repo_prefix().to_owned(),
            layout.global_prefix().to_owned(),
        );
        Ok(Self {
            hydrator: crab_read::ReadRuntimeBuilder::new(caching, read_layout, 16).build()?,
            lfs: crab_lfs::LfsObjectStore::new(store.clone(), layout.repo_prefix()),
            identity: RepositoryIdentity::new(
                format!("{}:{}", provider_name(config.provider), config.bucket),
                config.prefix.clone(),
                1,
            )?,
            read_views: crate::repository::ReadViewCache::new(),
            maintenance: Arc::new(crate::repository::WriteMaintenance::new()),
            config,
            store,
            layout,
        })
    }

    #[cfg(test)]
    pub(crate) fn new(config: RepositoryConfig, store: Store) -> crate::Result<Self> {
        let cache = Arc::new(LocalCache::with_limits(
            crab_cache::default_cache_root(),
            128 * 1024 * 1024,
            Some(128 * 1024 * 1024),
        ));
        Self::new_with_cache(config, store, cache)
    }

    fn access(&self, principal: &str) -> Option<RepositoryAccess> {
        self.config
            .members
            .iter()
            .find(|member| member.principal == principal)
            .map(|member| member.access)
    }
}

#[derive(Clone)]
pub(crate) struct Gateway {
    repositories: Arc<BTreeMap<String, Repository>>,
    runtime: Arc<RemoteGitRuntime>,
    options: RepositoryOptions,
    mutations: Arc<mutation::Coordinator>,
    auth: GatewayAuth,
    region: Arc<str>,
    admission: Admission,
    metrics: Metrics,
    local_cache: Arc<LocalCache>,
    cancellation: CancellationToken,
}

struct ReadObject {
    content: ReadContent,
    size: u64,
    etag: String,
    modified: Timestamp,
    attributes: Option<crate::attributes::ObjectAttributes>,
}

struct ReadObjectMetadata {
    blob_oid: gix_hash::ObjectId,
    size: u64,
    etag: String,
    modified: Timestamp,
    attributes: Option<crate::attributes::ObjectAttributes>,
}

struct ReadSelection {
    range: std::ops::Range<u64>,
    content_range: Option<String>,
    parts_count: Option<i32>,
    checksums: Option<crate::attributes::Checksums>,
}

#[derive(Clone)]
enum ReadContent {
    Ordinary(Bytes),
    CrabPointer(Bytes),
    LfsPointer(crab_git::LfsPointer),
}

type ContentStream =
    Pin<Box<dyn futures_util::Stream<Item = Result<Bytes, s3s::StdError>> + Send + 'static>>;

fn hold_permit(stream: ContentStream, permit: RequestPermit) -> ContentStream {
    Box::pin(futures_util::stream::unfold(
        (stream, permit),
        |(mut stream, permit)| async move {
            stream.next().await.map(|result| (result, (stream, permit)))
        },
    ))
}

fn verified_crab_stream(
    stream: crab_read::ReconstructionStream,
    expected_size: u64,
    expected_hash: [u8; 32],
) -> crab_read::ReconstructionStream {
    use futures_util::StreamExt as _;

    let expected_hash_hex = blake3::Hash::from_bytes(expected_hash).to_hex().to_string();
    Box::pin(futures_util::stream::try_unfold(
        (stream, blake3::Hasher::new(), 0_u64, None::<Bytes>, false),
        move |(mut stream, mut hasher, mut received, mut pending, finished)| {
            let expected_hash_hex = expected_hash_hex.clone();
            async move {
                if finished {
                    return Ok(None);
                }
                loop {
                    match stream.next().await {
                        Some(Ok(chunk)) if chunk.is_empty() => {}
                        Some(Ok(chunk)) => {
                            received = received
                                .checked_add(chunk.len() as u64)
                                .filter(|size| *size <= expected_size)
                                .ok_or_else(|| crab_read::ReadError::CorruptObject {
                                    path: expected_hash_hex.clone(),
                                    reason: format!(
                                        "reconstruction exceeded declared size {expected_size}"
                                    ),
                                })?;
                            hasher.update(&chunk);
                            if let Some(previous) = pending.replace(chunk) {
                                return Ok(Some((
                                    previous,
                                    (stream, hasher, received, pending, false),
                                )));
                            }
                        }
                        Some(Err(error)) => return Err(error),
                        None => {
                            if received != expected_size {
                                return Err(crab_read::ReadError::CorruptObject {
                                    path: expected_hash_hex.clone(),
                                    reason: format!(
                                        "reconstruction size mismatch: expected {expected_size}, got {received}"
                                    ),
                                });
                            }
                            let actual = hasher.clone().finalize();
                            if actual.as_bytes() != &expected_hash {
                                return Err(crab_read::ReadError::HashMismatch {
                                    requested: expected_hash_hex,
                                    actual: actual.to_hex().to_string(),
                                });
                            }
                            if let Some(last) = pending.take() {
                                return Ok(Some((last, (stream, hasher, received, pending, true))));
                            }
                            return Ok(None);
                        }
                    }
                }
            }
        },
    ))
}

impl ReadContent {
    async fn stream(
        self,
        repository: &Repository,
        range: std::ops::Range<u64>,
        metrics: &Metrics,
        direct_range: bool,
    ) -> S3Result<ContentStream> {
        use futures_util::{StreamExt as _, TryStreamExt as _};
        use tokio::io::AsyncReadExt as _;

        match self {
            Self::Ordinary(bytes) => {
                let start = usize::try_from(range.start).map_err(|_| s3_error!(InvalidRange))?;
                let end = usize::try_from(range.end).map_err(|_| s3_error!(InvalidRange))?;
                Ok(Box::pin(futures_util::stream::once(async move {
                    Ok(bytes.slice(start..end))
                })))
            }
            Self::LfsPointer(pointer) => {
                let (_, actual, stream) = repository
                    .lfs
                    .get_stream(&pointer.oid, pointer.size, Some(range.clone()))
                    .await
                    .map_err(|error| gateway_error(error.into()))?;
                if actual != range {
                    return Err(s3_error!(InvalidObjectState));
                }
                Ok(Box::pin(
                    stream.map_err(|error| Box::new(error) as s3s::StdError),
                ))
            }
            Self::CrabPointer(pointer_bytes) => {
                let PointerKind::Crab(pointer) = crab_git::classify(&pointer_bytes) else {
                    return Err(s3_error!(InvalidObjectState));
                };
                let full = range.start == 0 && range.end == pointer.size;
                if direct_range || full {
                    let stream = repository
                        .hydrator
                        .reconstruct_range_stream(&pointer, range)
                        .map_err(|error| gateway_error(error.into()))?;
                    let stream = (if full {
                        verified_crab_stream(stream, pointer.size, pointer.file_hash)
                    } else {
                        stream
                    })
                    .map_err(|error| Box::new(error) as s3s::StdError);
                    return Ok(Box::pin(stream));
                }
                let size = range.end - range.start;
                let capacity = metrics
                    .reserve_scratch(size)
                    .map_err(scratch_capacity_error)?;
                let mut scratch =
                    metrics.start_scratch(crate::metrics::ScratchPurpose::XetReconstruction);
                scratch.reserve(size);
                let directory = tempfile::tempdir().map_err(|error| {
                    scratch.record_failure(crate::metrics::ScratchFailure::Create);
                    gateway_error(error.into())
                })?;
                let path = directory.path().join("content");
                let reconstruction = if range.start == 0 && range.end == pointer.size {
                    repository
                        .hydrator
                        .reconstruct_to_path(&pointer, &path)
                        .await
                } else {
                    repository
                        .hydrator
                        .reconstruct_range_to_path(&pointer, range.start, range.end, &path)
                        .await
                };
                if let Err(error) = reconstruction {
                    if error.is_destination_io() {
                        scratch.record_failure(crate::metrics::ScratchFailure::Write);
                    }
                    return Err(gateway_error(error.into()));
                }
                // The completed file is now reflected by statvfs, so future
                // reservations no longer need its pre-write capacity claim.
                drop(capacity);
                scratch.record_written(size);
                let file = tokio::fs::File::open(path).await.map_err(|error| {
                    scratch.record_failure(crate::metrics::ScratchFailure::Read);
                    gateway_error(error.into())
                })?;
                let reader = tokio_util::io::ReaderStream::new(file.take(size));
                // The response stream owns the directory and accounting lease so a
                // disconnect removes the reconstructed bytes and releases the gauges.
                let stream = futures_util::stream::try_unfold(
                    (reader, directory, scratch),
                    |(mut reader, directory, scratch)| async move {
                        match reader.next().await {
                            Some(Ok(bytes)) => Ok(Some((bytes, (reader, directory, scratch)))),
                            Some(Err(error)) => {
                                scratch.record_failure(crate::metrics::ScratchFailure::Read);
                                Err(error)
                            }
                            None => Ok(None),
                        }
                    },
                )
                .map_err(|error| Box::new(error) as s3s::StdError);
                Ok(Box::pin(stream))
            }
        }
    }

    async fn spool(
        self,
        repository: &Repository,
        range: std::ops::Range<u64>,
        max_bytes: u64,
        metrics: &Metrics,
    ) -> S3Result<crate::content::Spool> {
        use futures_util::StreamExt as _;

        let expected = range.end.saturating_sub(range.start);
        let mut stream = self.stream(repository, range, metrics, false).await?;
        let mut writer = crate::content::SpoolWriter::new(metrics, Some(expected))
            .await
            .map_err(content_error)?;
        while let Some(chunk) = stream.next().await {
            writer
                .write(
                    &chunk.map_err(|error| {
                        tracing::warn!(%error, "S3 source object stream failed");
                        s3_error!(InternalError)
                    })?,
                    max_bytes,
                )
                .await
                .map_err(content_error)?;
        }
        writer.finish().await.map_err(content_error)
    }

    fn projected_etag(&self) -> S3Result<String> {
        match self {
            Self::Ordinary(bytes) => Ok(md5_hex(bytes)),
            Self::CrabPointer(pointer_bytes) => {
                let PointerKind::Crab(pointer) = crab_git::classify(pointer_bytes) else {
                    return Err(s3_error!(InvalidObjectState));
                };
                Ok(blake3::Hash::from_bytes(pointer.file_hash)
                    .to_hex()
                    .to_string())
            }
            Self::LfsPointer(pointer) => Ok(pointer
                .oid
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect()),
        }
    }
}

impl Gateway {
    pub(crate) fn new(config: Config, cancellation: CancellationToken) -> crate::Result<Self> {
        let auth = GatewayAuth::load(&config)?;
        let region = Arc::from(config.region.clone());
        let metrics = Metrics::new()?;
        let local_cache = Arc::new(LocalCache::with_limits(
            config.cache.directory.clone(),
            config.cache.max_bytes,
            Some(config.cache.max_bytes),
        ));
        local_cache.prepare()?;
        metrics.set_cache_limit(config.cache.max_bytes);
        let mut stores: BTreeMap<String, Store> = BTreeMap::new();
        let mut repositories = BTreeMap::new();
        for entry in config.repositories {
            let store_key = format!("{}:{}", provider_name(entry.provider), entry.bucket);
            let store = match stores.get(&store_key) {
                Some(store) => store.clone(),
                None => {
                    let store =
                        build_store(&entry)?.with_storage_observer(metrics.storage_observer());
                    stores.insert(store_key, store.clone());
                    store
                }
            };
            let repository = Repository::new_with_cache_observer(
                entry.clone(),
                store,
                Arc::clone(&local_cache),
                metrics.cache_observer(),
            )?;
            repositories.insert(entry.name.clone(), repository);
        }
        let runtime = Arc::new(RemoteGitRuntime::default());
        let options = RepositoryOptions::default();
        let admission = Admission::new(
            config.max_in_flight_requests,
            cancellation.clone(),
            metrics.clone(),
        );
        Ok(Self {
            repositories: Arc::new(repositories),
            mutations: Arc::new(mutation::Coordinator::new(
                Arc::clone(&runtime),
                options,
                metrics.clone(),
            )),
            runtime,
            options,
            auth,
            region,
            admission,
            metrics,
            local_cache,
            cancellation,
        })
    }

    pub(crate) async fn initialize_repositories(&self) -> crate::Result<()> {
        for repository in self.repositories.values() {
            let head = format!("refs/heads/{}", repository.config.default_branch);
            crab_write::initialize::initialize_repository(
                &repository.store,
                &repository.layout,
                &head,
            )
            .await?;
        }
        Ok(())
    }

    pub(crate) async fn ready(&self) -> crate::Result<()> {
        let cancellation = self.cancellation.child_token();
        let _cancel_on_drop = cancellation.clone().drop_guard();
        for repository in self.repositories.values() {
            repository
                .read_views
                .current(
                    repository,
                    Arc::clone(&self.runtime),
                    self.options,
                    &cancellation,
                )
                .await?;
        }
        Ok(())
    }

    pub(crate) fn start_multipart_maintenance(&self) -> tokio::task::JoinHandle<()> {
        let gateway = self.clone();
        let repositories = Arc::clone(&self.repositories);
        let cancellation = self.cancellation.clone();
        tokio::spawn(async move {
            loop {
                let started = Instant::now();
                let now = match SystemTime::now().duration_since(UNIX_EPOCH) {
                    Ok(duration) => duration.as_secs(),
                    Err(error) => {
                        tracing::warn!(%error, "multipart maintenance clock unavailable");
                        0
                    }
                };
                if now != 0 {
                    let names = repositories.keys().cloned().collect::<Vec<_>>();
                    let maintenance = futures_util::stream::iter(names)
                        .map(|name| {
                            let gateway = gateway.clone();
                            let repositories = Arc::clone(&repositories);
                            async move {
                                let result = if let Some(repository) = repositories.get(&name) {
                                    // Every transition is restart-idempotent; a timed-out
                                    // pass retains its slot/state for the next sweep.
                                    Some(
                                        tokio::time::timeout(
                                            Duration::from_secs(30),
                                            gateway.maintain_multipart_repository(repository, now),
                                        )
                                        .await,
                                    )
                                } else {
                                    None
                                };
                                (name, result)
                            }
                        })
                        .buffer_unordered(4)
                        .collect::<Vec<_>>();
                    let results = tokio::select! {
                        () = cancellation.cancelled() => break,
                        results = maintenance => results,
                    };
                    let mut healthy = true;
                    for (repository, result) in results {
                        match result {
                            None => {
                                healthy = false;
                                gateway.metrics.record_maintenance_failure(
                                    MaintenanceFailure::RepositoryMissing,
                                    1,
                                );
                                tracing::warn!(%repository, "multipart repository disappeared");
                            }
                            Some(Err(_)) => {
                                healthy = false;
                                gateway
                                    .metrics
                                    .record_maintenance_failure(MaintenanceFailure::Timeout, 1);
                                tracing::warn!(%repository, "multipart maintenance timed out");
                            }
                            Some(Ok(Ok(stats))) => {
                                healthy &= gateway.metrics.record_maintenance_result(&stats);
                                if stats.expired != 0
                                    || stats.terminal_cleanups != 0
                                    || stats.missing_cleanups != 0
                                    || stats.published_recoveries != 0
                                {
                                    tracing::info!(
                                        %repository,
                                        expired = stats.expired,
                                        terminal_cleanups = stats.terminal_cleanups,
                                        missing_cleanups = stats.missing_cleanups,
                                        published_recoveries = stats.published_recoveries,
                                        "multipart maintenance completed"
                                    );
                                }
                            }
                            Some(Ok(Err(error))) => {
                                healthy = false;
                                gateway
                                    .metrics
                                    .record_maintenance_failure(MaintenanceFailure::Sweep, 1);
                                tracing::warn!(%repository, %error, "multipart maintenance failed");
                            }
                        }
                    }
                    gateway.metrics.record_maintenance_cycle(
                        if healthy {
                            MaintenanceCycleOutcome::Success
                        } else {
                            MaintenanceCycleOutcome::Degraded
                        },
                        started,
                        now,
                    );
                } else {
                    gateway
                        .metrics
                        .record_maintenance_failure(MaintenanceFailure::Clock, 1);
                    gateway.metrics.record_maintenance_cycle(
                        MaintenanceCycleOutcome::ClockError,
                        started,
                        now,
                    );
                }
                tokio::select! {
                    () = cancellation.cancelled() => break,
                    () = tokio::time::sleep(Duration::from_secs(60)) => {}
                }
            }
        })
    }

    async fn maintain_multipart_repository(
        &self,
        repository: &Repository,
        now: u64,
    ) -> crate::multipart::Result<crate::multipart::SweepStats> {
        let mut stats = crate::multipart::sweep(repository, now).await?;
        let recoveries = futures_util::stream::iter(stats.take_completing())
            .map(|loaded| async move {
                let upload_id = loaded.session.id.clone();
                let result = self.recover_published_multipart(repository, loaded).await;
                (upload_id, result)
            })
            .buffer_unordered(4)
            .collect::<Vec<_>>()
            .await;
        for (upload_id, result) in recoveries {
            match result {
                Ok(true) => stats.published_recoveries += 1,
                Ok(false) => {
                    stats.unresolved_completions = stats.unresolved_completions.saturating_add(1);
                }
                Err(error) => {
                    stats.publication_recovery_failures =
                        stats.publication_recovery_failures.saturating_add(1);
                    tracing::warn!(%upload_id, %error, "multipart publication recovery failed");
                }
            }
        }
        Ok(stats)
    }

    async fn recover_published_multipart(
        &self,
        repository: &Repository,
        loaded: crate::multipart::Loaded,
    ) -> S3Result<bool> {
        let plan_id = crate::multipart::publication_plan_id(&loaded.session.id);
        if crab_metadata::plan_receipt::resolve_plan_receipt(
            &repository.store,
            &repository.layout,
            &plan_id,
        )
        .await
        .map_err(|error| gateway_error(error.into()))?
        .is_some()
            && let Some((etag, checksums)) =
                crate::multipart::planned_completion(&loaded.session).map_err(multipart_error)?
        {
            crate::multipart::complete(repository, loaded, etag, checksums)
                .await
                .map_err(multipart_error)?;
            return Ok(true);
        }
        let Some(attributes) = self
            .current_object_attributes(repository, &loaded.session.key)
            .await?
        else {
            return Ok(false);
        };
        let Some((etag, checksums)) = completion_receipt(&loaded.session, &attributes)? else {
            return Ok(false);
        };
        crate::multipart::complete(repository, loaded, etag, checksums)
            .await
            .map_err(multipart_error)?;
        Ok(true)
    }

    pub(crate) fn auth(&self) -> GatewayAuth {
        self.auth.clone()
    }

    pub(crate) fn metrics(&self) -> Metrics {
        self.metrics.clone()
    }

    pub(crate) async fn render_metrics(&self) -> String {
        self.metrics.refresh_cache(&self.local_cache).await;
        self.metrics.render(&self.admission)
    }

    pub(crate) async fn shutdown(&self) {
        self.cancellation.cancel();
        self.runtime.shutdown().await;
    }

    async fn admit(&self, class: RequestClass) -> S3Result<RequestPermit> {
        self.admission.acquire(class).await.map_err(admission_error)
    }

    fn principal<'a, T>(&'a self, req: &S3Request<T>) -> S3Result<&'a str> {
        if req
            .region
            .as_ref()
            .is_some_and(|region| region.as_str() != self.region.as_ref())
        {
            return Err(s3_error!(
                AuthorizationHeaderMalformed,
                "The authorization region does not match this endpoint"
            ));
        }
        if req
            .service
            .as_deref()
            .is_some_and(|service| service != "s3")
        {
            return Err(s3_error!(SignatureDoesNotMatch));
        }
        let access_key = req
            .credentials
            .as_ref()
            .map(|credentials| credentials.access_key.as_str())
            .ok_or_else(|| s3_error!(AccessDenied, "Signature is required"))?;
        self.auth
            .principal(access_key)
            .ok_or_else(|| s3_error!(AccessDenied))
    }

    fn repository<'a, T>(
        &'a self,
        req: &S3Request<T>,
        bucket: &str,
        required: RepositoryAccess,
    ) -> S3Result<&'a Repository> {
        let principal = self.principal(req)?;
        let repository = self
            .repositories
            .get(bucket)
            .ok_or_else(|| s3_error!(NoSuchBucket))?;
        if repository
            .access(principal)
            .is_some_and(|access| access >= required)
        {
            Ok(repository)
        } else {
            Err(s3_error!(AccessDenied))
        }
    }

    async fn open(&self, repository: &Repository) -> S3Result<Arc<crate::repository::ReadView>> {
        repository
            .read_views
            .current(
                repository,
                Arc::clone(&self.runtime),
                self.options,
                &self.cancellation,
            )
            .await
            .map_err(gateway_error)
    }

    async fn read_object(&self, repository: &Repository, key: &str) -> S3Result<ReadObject> {
        let address = namespace::object_address(key).map_err(namespace_error)?;
        let repo = self.open(repository).await?;
        let operation = repo
            .remote()
            .operation(OperationKind::Repository, &self.cancellation)
            .await
            .map_err(remote_error)?;
        let result = async {
            let snapshot = repo
                .snapshot(&address.reference, &operation)
                .await
                .map_err(gateway_error)?;
            let commit = snapshot.commit(&operation).await.map_err(remote_error)?;
            let blob = snapshot
                .read_blob(&address.path, &operation)
                .await
                .map_err(remote_error)?;
            if blob.metadata.kind != EntryKind::Blob {
                return Err(s3_error!(InvalidObjectState));
            }
            let manifest = repo
                .attributes(repository, snapshot.commit_oid())
                .await
                .map_err(gateway_error)?;
            let path = std::str::from_utf8(address.path.as_bytes())
                .map_err(|_| s3_error!(InvalidObjectState))?;
            let attributes = manifest.object(path, blob.metadata.oid).cloned();
            let modified_seconds = attributes
                .as_ref()
                .and_then(|value| i64::try_from(value.modified_seconds).ok())
                .unwrap_or(commit.committer.seconds);
            let modified = timestamp(modified_seconds)?;
            let (content, size) = classify_blob(blob)?;
            let etag = match attributes.as_ref() {
                Some(value) => value.etag.clone(),
                None => content.projected_etag()?,
            };
            Ok(ReadObject {
                content,
                size,
                etag,
                modified,
                attributes,
            })
        }
        .await;
        finish(operation, result).await
    }

    async fn read_object_metadata(
        &self,
        repository: &Repository,
        key: &str,
    ) -> S3Result<ReadObjectMetadata> {
        let address = namespace::object_address(key).map_err(namespace_error)?;
        let view = self.open(repository).await?;
        let operation = view
            .remote()
            .operation(OperationKind::Repository, &self.cancellation)
            .await
            .map_err(remote_error)?;
        let result = async {
            let snapshot = view
                .snapshot(&address.reference, &operation)
                .await
                .map_err(gateway_error)?;
            let entry = snapshot
                .entry(&address.path, &operation)
                .await
                .map_err(remote_error)?
                .ok_or_else(|| s3_error!(NoSuchKey))?;
            if entry.kind != EntryKind::Blob {
                return Err(object_entry_error(entry.kind));
            }
            let path = std::str::from_utf8(address.path.as_bytes())
                .map_err(|_| s3_error!(InvalidObjectState))?;
            let attributes = view
                .object_attributes(repository, snapshot.commit_oid(), path, entry.oid)
                .await
                .map_err(gateway_error)?;
            if let Some(attributes) = attributes {
                return Ok(ReadObjectMetadata {
                    blob_oid: entry.oid,
                    size: attributes.size,
                    etag: attributes.etag.clone(),
                    modified: timestamp(
                        i64::try_from(attributes.modified_seconds)
                            .map_err(|_| s3_error!(InternalError))?,
                    )?,
                    attributes: Some(attributes),
                });
            }
            let commit = snapshot.commit(&operation).await.map_err(remote_error)?;
            let blob = snapshot
                .read_blob(&address.path, &operation)
                .await
                .map_err(remote_error)?;
            let blob_oid = blob.metadata.oid;
            let (content, size) = classify_blob(blob)?;
            let etag = content.projected_etag()?;
            Ok(ReadObjectMetadata {
                blob_oid,
                size,
                etag,
                modified: timestamp(commit.committer.seconds)?,
                attributes: None,
            })
        }
        .await;
        finish(operation, result).await
    }

    async fn current_object_attributes(
        &self,
        repository: &Repository,
        key: &str,
    ) -> S3Result<Option<crate::attributes::ObjectAttributes>> {
        let address = namespace::object_address(key).map_err(namespace_error)?;
        let view = self.open(repository).await?;
        let operation = view
            .remote()
            .operation(OperationKind::Repository, &self.cancellation)
            .await
            .map_err(remote_error)?;
        let result = async {
            let snapshot = view
                .snapshot(&address.reference, &operation)
                .await
                .map_err(gateway_error)?;
            let Some(entry) = snapshot
                .entry(&address.path, &operation)
                .await
                .map_err(remote_error)?
            else {
                return Ok(None);
            };
            if entry.kind != EntryKind::Blob {
                return Ok(None);
            }
            let path = std::str::from_utf8(address.path.as_bytes())
                .map_err(|_| s3_error!(InvalidObjectState))?;
            view.object_attributes(repository, snapshot.commit_oid(), path, entry.oid)
                .await
                .map_err(gateway_error)
        }
        .await;
        finish(operation, result).await
    }

    fn writable_address<T>(
        &self,
        req: &S3Request<T>,
        bucket: &str,
        key: &str,
    ) -> S3Result<(&Repository, namespace::ObjectAddress, String)> {
        let address = namespace::object_address(key).map_err(namespace_error)?;
        self.writable_object_address(req, bucket, address)
    }

    fn writable_object_address<'a, T>(
        &'a self,
        req: &S3Request<T>,
        bucket: &str,
        address: namespace::ObjectAddress,
    ) -> S3Result<(&'a Repository, namespace::ObjectAddress, String)> {
        let principal = self.principal(req)?.to_owned();
        let repository = self.repository(req, bucket, RepositoryAccess::Write)?;
        let branch = address
            .branch
            .as_deref()
            .and_then(|name| name.strip_prefix("refs/heads/"))
            .ok_or_else(|| s3_error!(MethodNotAllowed, "Writes require a branch key"))?;
        if repository
            .config
            .protected_branches
            .iter()
            .any(|protected| protected == branch)
        {
            return Err(s3_error!(
                AccessDenied,
                "The destination branch is protected"
            ));
        }
        Ok((repository, address, principal))
    }

    async fn put_condition(
        &self,
        repository: &Repository,
        key: &str,
        input: &PutObjectInput,
    ) -> S3Result<mutation::PutCondition> {
        self.put_condition_values(
            repository,
            key,
            input.if_match.clone(),
            input.if_none_match.clone(),
        )
        .await
    }

    async fn put_condition_values(
        &self,
        repository: &Repository,
        key: &str,
        if_match: Option<ETagCondition>,
        if_none_match: Option<ETagCondition>,
    ) -> S3Result<mutation::PutCondition> {
        if if_match.is_some() && if_none_match.is_some() {
            return Err(s3_error!(InvalidRequest, "Conflicting write preconditions"));
        }
        if let Some(condition) = if_match {
            let object = match self.read_object_metadata(repository, key).await {
                Ok(object) => object,
                Err(error) if error.code().as_str() == "NoSuchKey" => {
                    return Err(s3_error!(PreconditionFailed));
                }
                Err(error) => return Err(error),
            };
            let actual = ETag::Strong(object.etag);
            let matches = match condition {
                ETagCondition::Any => true,
                ETagCondition::ETag(expected) => actual.strong_cmp(&expected),
            };
            if !matches {
                return Err(s3_error!(PreconditionFailed));
            }
            return Ok(mutation::PutCondition::IfMatch(object.blob_oid));
        }
        match if_none_match {
            None => Ok(mutation::PutCondition::None),
            Some(ETagCondition::Any) => Ok(mutation::PutCondition::IfNoneMatchAny),
            Some(ETagCondition::ETag(_)) => Err(s3_error!(InvalidRequest)),
        }
    }

    async fn delete_condition(
        &self,
        repository: &Repository,
        key: &str,
        if_match: Option<&ETagCondition>,
    ) -> S3Result<mutation::DeleteCondition> {
        let Some(if_match) = if_match else {
            return Ok(mutation::DeleteCondition::None);
        };
        match if_match {
            ETagCondition::Any => Ok(mutation::DeleteCondition::IfMatchAny),
            ETagCondition::ETag(ETag::Weak(_)) => Err(s3_error!(InvalidRequest)),
            ETagCondition::ETag(ETag::Strong(expected)) => {
                let object = match self.read_object_metadata(repository, key).await {
                    Ok(object) => object,
                    Err(error) if error.code().as_str() == "NoSuchKey" => {
                        return Err(s3_error!(PreconditionFailed));
                    }
                    Err(error) => return Err(error),
                };
                if object.etag != *expected {
                    return Err(s3_error!(PreconditionFailed));
                }
                Ok(mutation::DeleteCondition::IfMatch {
                    object: object.blob_oid,
                    etag: expected.clone(),
                    attributes_present: object.attributes.is_some(),
                })
            }
        }
    }
}

fn build_store(entry: &RepositoryConfig) -> crate::Result<Store> {
    Ok(build_static_env_store(&entry.bucket, entry.provider)?)
}

fn classify_blob(blob: crab_remote_git::Blob) -> S3Result<(ReadContent, u64)> {
    let physical_size = u64::try_from(blob.bytes.len()).map_err(|_| s3_error!(EntityTooLarge))?;
    let logical_size = blob.metadata.logical_size.unwrap_or(physical_size);
    if logical_size > crate::content::MAX_MULTIPART_OBJECT_BYTES {
        return Err(s3_error!(EntityTooLarge));
    }
    let content = match blob.metadata.classification {
        ContentClassification::OrdinaryGit => ReadContent::Ordinary(blob.bytes),
        ContentClassification::CrabPointer => ReadContent::CrabPointer(blob.bytes),
        ContentClassification::LfsPointer => {
            let PointerKind::Lfs(pointer) = crab_git::classify(&blob.bytes) else {
                return Err(s3_error!(InvalidObjectState));
            };
            if pointer.size != logical_size {
                return Err(s3_error!(InvalidObjectState));
            }
            ReadContent::LfsPointer(pointer)
        }
    };
    Ok((content, logical_size))
}

fn provider_name(provider: StorageProviderKind) -> &'static str {
    match provider {
        StorageProviderKind::S3 => "s3",
        StorageProviderKind::Gcs => "gcs",
        StorageProviderKind::Azure => "azure",
        StorageProviderKind::Local => "local",
    }
}

struct MutationContent {
    bytes: Bytes,
    track_lfs: bool,
}

enum MultipartAssemblyWriter {
    Inline(Box<crate::content::SpoolWriter>),
    Large(Box<crate::content::Digester>),
}

enum MultipartAssembly {
    Inline(crate::content::Spool),
    Large {
        size: u64,
        digests: crate::content::Digests,
    },
}

impl MultipartAssemblyWriter {
    async fn new(size: u64, metrics: &Metrics) -> Result<Self, crate::content::Error> {
        if size <= crate::content::INLINE_GIT_BLOB_BYTES {
            Ok(Self::Inline(Box::new(
                crate::content::SpoolWriter::new(metrics, Some(size)).await?,
            )))
        } else {
            Ok(Self::Large(Box::new(crate::content::Digester::new())))
        }
    }

    async fn write(&mut self, bytes: &[u8], max_bytes: u64) -> Result<(), crate::content::Error> {
        match self {
            Self::Inline(writer) => writer.write(bytes, max_bytes).await,
            Self::Large(digester) => digester.write(bytes, max_bytes),
        }
    }

    async fn finish(self) -> Result<MultipartAssembly, crate::content::Error> {
        match self {
            Self::Inline(writer) => writer.finish().await.map(MultipartAssembly::Inline),
            Self::Large(digester) => {
                let (size, digests) = digester.finish()?;
                Ok(MultipartAssembly::Large { size, digests })
            }
        }
    }
}

impl MultipartAssembly {
    fn size(&self) -> u64 {
        match self {
            Self::Inline(spool) => spool.size,
            Self::Large { size, .. } => *size,
        }
    }

    fn digests(&self) -> &crate::content::Digests {
        match self {
            Self::Inline(spool) => &spool.digests,
            Self::Large { digests, .. } => digests,
        }
    }
}

fn lfs_pointer_content(oid: [u8; 32], size: u64) -> MutationContent {
    MutationContent {
        bytes: Bytes::from(
            crab_git::LfsPointer {
                oid,
                size,
                extensions: Vec::new(),
            }
            .serialize(),
        ),
        track_lfs: true,
    }
}

async fn mutation_bytes(
    repository: &Repository,
    spool: &crate::content::Spool,
) -> S3Result<MutationContent> {
    mutation_bytes_with_inline_limit(repository, spool, crate::content::INLINE_GIT_BLOB_BYTES).await
}

async fn mutation_bytes_with_inline_limit(
    repository: &Repository,
    spool: &crate::content::Spool,
    inline_limit: u64,
) -> S3Result<MutationContent> {
    if spool.size <= inline_limit {
        return Ok(MutationContent {
            bytes: spool.bytes().await.map_err(content_error)?,
            track_lfs: false,
        });
    }
    // The LFS object is content-addressed and uploaded before its pointer commit.
    // Crab's GC grace period protects this brief publication window and cleans an
    // orphan if the later ref mutation fails.
    repository
        .lfs
        .put_stream_with_size(&spool.digests.sha256, Some(spool.size), spool.path())
        .await
        .map_err(|error| gateway_error(error.into()))?;
    Ok(lfs_pointer_content(spool.digests.sha256, spool.size))
}

async fn mutation_multipart_bytes(
    repository: &Repository,
    parts: &[crate::multipart::Part],
    assembly: MultipartAssembly,
) -> S3Result<MutationContent> {
    let (size, digests) = match assembly {
        MultipartAssembly::Inline(spool) => return mutation_bytes(repository, &spool).await,
        MultipartAssembly::Large { size, digests } => (size, digests),
    };
    use futures_util::TryStreamExt as _;

    // Completion has already verified every durable part and the aggregate
    // digest. Replay those parts into LFS so large objects never need an
    // assembled local spool; LFS rechecks both size and SHA-256 before publish.
    let source =
        crate::multipart::parts_stream(repository, parts).map_err(|error| crab_lfs::LfsError::Io {
            source: std::io::Error::other(error),
        });
    repository
        .lfs
        .put_byte_stream_with_size(&digests.sha256, size, source)
        .await
        .map_err(|error| gateway_error(error.into()))?;
    Ok(lfs_pointer_content(digests.sha256, size))
}

#[async_trait::async_trait]
impl S3 for Gateway {
    async fn list_buckets(
        &self,
        req: S3Request<ListBucketsInput>,
    ) -> S3Result<S3Response<ListBucketsOutput>> {
        let _permit = self.admit(RequestClass::Control).await?;
        if req
            .input
            .bucket_region
            .as_deref()
            .is_some_and(|region| region != self.region.as_ref())
        {
            return Err(s3_error!(InvalidRequest));
        }
        let principal = self.principal(&req)?;
        let mut buckets = self
            .repositories
            .values()
            .filter(|repository| repository.access(principal).is_some())
            .filter(|repository| {
                req.input
                    .prefix
                    .as_deref()
                    .is_none_or(|prefix| repository.config.name.starts_with(prefix))
            })
            .filter(|repository| {
                req.input
                    .continuation_token
                    .as_deref()
                    .is_none_or(|token| repository.config.name.as_str() > token)
            })
            .map(|repository| Bucket {
                name: Some(repository.config.name.clone()),
                ..Default::default()
            })
            .collect::<Vec<_>>();
        let limit = match req.input.max_buckets {
            Some(value) if (1..=10_000).contains(&value) => {
                usize::try_from(value).map_err(|_| s3_error!(InvalidArgument))?
            }
            Some(_) => return Err(s3_error!(InvalidArgument)),
            None => usize::MAX,
        };
        let truncated = buckets.len() > limit;
        buckets.truncate(limit);
        let continuation_token = truncated
            .then(|| buckets.last().and_then(|bucket| bucket.name.clone()))
            .flatten();
        Ok(S3Response::new(ListBucketsOutput {
            buckets: Some(buckets),
            continuation_token,
            prefix: req.input.prefix,
            ..Default::default()
        }))
    }

    async fn head_bucket(
        &self,
        req: S3Request<HeadBucketInput>,
    ) -> S3Result<S3Response<HeadBucketOutput>> {
        let _permit = self.admit(RequestClass::Control).await?;
        if req.input.expected_bucket_owner.is_some() {
            return Err(s3_error!(NotImplemented));
        }
        self.repository(&req, &req.input.bucket, RepositoryAccess::Read)?;
        let mut response = S3Response::new(HeadBucketOutput::default());
        response.headers.insert(
            "x-amz-bucket-region",
            http::HeaderValue::from_str(&self.region).map_err(|_| s3_error!(InternalError))?,
        );
        Ok(response)
    }

    async fn get_bucket_location(
        &self,
        req: S3Request<GetBucketLocationInput>,
    ) -> S3Result<S3Response<GetBucketLocationOutput>> {
        let _permit = self.admit(RequestClass::Control).await?;
        if req.input.expected_bucket_owner.is_some() {
            return Err(s3_error!(NotImplemented));
        }
        self.repository(&req, &req.input.bucket, RepositoryAccess::Read)?;
        Ok(S3Response::new(GetBucketLocationOutput {
            location_constraint: (self.region.as_ref() != "us-east-1")
                .then(|| BucketLocationConstraint::from(self.region.to_string())),
        }))
    }

    async fn get_bucket_versioning(
        &self,
        req: S3Request<GetBucketVersioningInput>,
    ) -> S3Result<S3Response<GetBucketVersioningOutput>> {
        let _permit = self.admit(RequestClass::Control).await?;
        if req.input.expected_bucket_owner.is_some() {
            return Err(s3_error!(NotImplemented));
        }
        self.repository(&req, &req.input.bucket, RepositoryAccess::Read)?;
        // An empty response is S3's representation for a bucket that has never
        // enabled versioning. Crab refs remain a separate namespace contract.
        Ok(S3Response::new(GetBucketVersioningOutput::default()))
    }

    async fn get_object(
        &self,
        req: S3Request<GetObjectInput>,
    ) -> S3Result<S3Response<GetObjectOutput>> {
        let permit = self.admit(RequestClass::Read).await?;
        reject_get_extensions(&req.input)?;
        let repository = self.repository(&req, &req.input.bucket, RepositoryAccess::Read)?;
        let object = self.read_object(repository, &req.input.key).await?;
        let tag_count = tag_count(object.attributes.as_ref())?;
        evaluate_conditions(
            req.input.if_match.as_ref(),
            req.input.if_none_match.as_ref(),
            req.input.if_modified_since.as_ref(),
            req.input.if_unmodified_since.as_ref(),
            &object.etag,
            &object.modified,
        )?;
        let selection = read_selection(
            object.size,
            object.attributes.as_ref(),
            req.input.range.as_ref(),
            req.input.part_number,
        )?;
        let response_checksums = response_checksums(
            req.input.checksum_mode.as_ref(),
            selection.checksums.as_ref(),
        )?;
        let direct_range = selection.content_range.is_some();
        let range = selection.range;
        let content_length =
            i64::try_from(range.end - range.start).map_err(|_| s3_error!(InternalError))?;
        let body = hold_permit(
            object
                .content
                .stream(repository, range, &self.metrics, direct_range)
                .await?,
            permit,
        );
        let body =
            http_body_util::StreamBody::new(body.map(|result| result.map(http_body::Frame::data)));
        let output = GetObjectOutput {
            accept_ranges: Some("bytes".to_owned()),
            body: Some(StreamingBlob::from(s3s::Body::http_body_unsync(body))),
            content_length: Some(content_length),
            content_range: selection.content_range,
            content_type: req
                .input
                .response_content_type
                .or_else(|| {
                    object
                        .attributes
                        .as_ref()
                        .and_then(|value| value.content_type.clone())
                })
                .or_else(|| Some("application/octet-stream".to_owned())),
            cache_control: req.input.response_cache_control.or_else(|| {
                object
                    .attributes
                    .as_ref()
                    .and_then(|value| value.cache_control.clone())
            }),
            content_disposition: req.input.response_content_disposition.or_else(|| {
                object
                    .attributes
                    .as_ref()
                    .and_then(|value| value.content_disposition.clone())
            }),
            content_encoding: req.input.response_content_encoding.or_else(|| {
                object
                    .attributes
                    .as_ref()
                    .and_then(|value| value.content_encoding.clone())
            }),
            content_language: req.input.response_content_language.or_else(|| {
                object
                    .attributes
                    .as_ref()
                    .and_then(|value| value.content_language.clone())
            }),
            expires: req.input.response_expires.or_else(|| {
                object
                    .attributes
                    .as_ref()
                    .and_then(|value| value.expires.clone())
            }),
            e_tag: Some(ETag::Strong(object.etag)),
            checksum_crc32: response_checksums.crc32,
            checksum_crc32c: response_checksums.crc32c,
            checksum_crc64nvme: response_checksums.crc64nvme,
            checksum_sha1: response_checksums.sha1,
            checksum_sha256: response_checksums.sha256,
            checksum_type: response_checksums.checksum_type.map(ChecksumType::from),
            last_modified: Some(object.modified),
            parts_count: selection.parts_count,
            tag_count,
            metadata: object
                .attributes
                .map(|value| value.metadata.into_iter().collect()),
            ..Default::default()
        };
        Ok(S3Response::new(output))
    }

    async fn head_object(
        &self,
        req: S3Request<HeadObjectInput>,
    ) -> S3Result<S3Response<HeadObjectOutput>> {
        let _permit = self.admit(RequestClass::Read).await?;
        reject_head_extensions(&req.input)?;
        let repository = self.repository(&req, &req.input.bucket, RepositoryAccess::Read)?;
        let object = self
            .read_object_metadata(repository, &req.input.key)
            .await?;
        let tag_count = tag_count(object.attributes.as_ref())?;
        evaluate_conditions(
            req.input.if_match.as_ref(),
            req.input.if_none_match.as_ref(),
            req.input.if_modified_since.as_ref(),
            req.input.if_unmodified_since.as_ref(),
            &object.etag,
            &object.modified,
        )?;
        let selection = read_selection(
            object.size,
            object.attributes.as_ref(),
            req.input.range.as_ref(),
            req.input.part_number,
        )?;
        let response_checksums = response_checksums(
            req.input.checksum_mode.as_ref(),
            selection.checksums.as_ref(),
        )?;
        let content_length = selection.range.end - selection.range.start;
        let mut response = S3Response::new(HeadObjectOutput {
            accept_ranges: Some("bytes".to_owned()),
            content_length: Some(
                i64::try_from(content_length).map_err(|_| s3_error!(InternalError))?,
            ),
            content_range: selection.content_range,
            cache_control: response_override(
                req.input.response_cache_control,
                object
                    .attributes
                    .as_ref()
                    .and_then(|value| value.cache_control.as_ref()),
            ),
            content_disposition: response_override(
                req.input.response_content_disposition,
                object
                    .attributes
                    .as_ref()
                    .and_then(|value| value.content_disposition.as_ref()),
            ),
            content_encoding: response_override(
                req.input.response_content_encoding,
                object
                    .attributes
                    .as_ref()
                    .and_then(|value| value.content_encoding.as_ref()),
            ),
            content_language: response_override(
                req.input.response_content_language,
                object
                    .attributes
                    .as_ref()
                    .and_then(|value| value.content_language.as_ref()),
            ),
            content_type: response_override(
                req.input.response_content_type,
                object
                    .attributes
                    .as_ref()
                    .and_then(|value| value.content_type.as_ref()),
            )
            .or_else(|| Some("application/octet-stream".to_owned())),
            e_tag: Some(ETag::Strong(object.etag)),
            checksum_crc32: response_checksums.crc32,
            checksum_crc32c: response_checksums.crc32c,
            checksum_crc64nvme: response_checksums.crc64nvme,
            checksum_sha1: response_checksums.sha1,
            checksum_sha256: response_checksums.sha256,
            checksum_type: response_checksums.checksum_type.map(ChecksumType::from),
            expires: response_override(
                req.input.response_expires,
                object
                    .attributes
                    .as_ref()
                    .and_then(|value| value.expires.as_ref()),
            ),
            last_modified: Some(object.modified),
            parts_count: selection.parts_count,
            metadata: object
                .attributes
                .map(|value| value.metadata.into_iter().collect()),
            ..Default::default()
        });
        if let Some(tag_count) = tag_count {
            response.headers.insert(
                "x-amz-tagging-count",
                http::HeaderValue::from_str(&tag_count.to_string())
                    .map_err(|_| s3_error!(InternalError))?,
            );
        }
        Ok(response)
    }

    async fn put_object(
        &self,
        req: S3Request<PutObjectInput>,
    ) -> S3Result<S3Response<PutObjectOutput>> {
        let _permit = self.admit(RequestClass::Transfer).await?;
        reject_put_extensions(&req.input)?;
        let marker_address =
            namespace::directory_marker_address(&req.input.key).map_err(namespace_error)?;
        let is_directory_marker = marker_address.is_some();
        let (repository, address, principal) = match marker_address {
            Some(address) => self.writable_object_address(&req, &req.input.bucket, address)?,
            None => self.writable_address(&req, &req.input.bucket, &req.input.key)?,
        };
        let condition = if is_directory_marker {
            virtual_marker_put_condition(&req.input)?
        } else {
            self.put_condition(repository, &req.input.key, &req.input)
                .await?
        };
        let content_length = req.input.content_length;
        let content_md5 = req.input.content_md5.clone();
        let mut checksums = RequestChecksums::from(&req.input);
        let trailing_headers = req.trailing_headers.clone();
        let content_encoding = stored_content_encoding(
            req.input.content_encoding.as_deref(),
            trailing_headers.is_some(),
        );
        let spool = crate::content::spool_body(
            req.input.body,
            content_length,
            if is_directory_marker {
                0
            } else {
                crate::content::MAX_PUT_OBJECT_BYTES
            },
            &self.metrics,
        )
        .await
        .map_err(content_error)?;
        checksums.merge_trailers(trailing_headers.as_ref())?;
        verify_content_md5(&spool.digests.md5, content_md5.as_deref())?;
        checksums.verify(&spool.digests)?;
        let stored_checksums = checksums.stored(&spool.digests);
        let etag = crate::content::md5_hex(&spool.digests.md5);
        if is_directory_marker {
            // Filesystem clients use empty trailing-slash PUTs as directory hints.
            // Git trees already represent non-empty directories, so no blob is published.
            return Ok(S3Response::new(PutObjectOutput {
                e_tag: Some(ETag::Strong(etag)),
                checksum_crc32: stored_checksums.crc32,
                checksum_crc32c: stored_checksums.crc32c,
                checksum_crc64nvme: stored_checksums.crc64nvme,
                checksum_sha1: stored_checksums.sha1,
                checksum_sha256: stored_checksums.sha256,
                checksum_type: stored_checksums.checksum_type.map(ChecksumType::from),
                ..Default::default()
            }));
        }
        let content = mutation_bytes(repository, &spool).await?;
        let outcome = self
            .mutations
            .apply(
                repository,
                address
                    .branch
                    .as_deref()
                    .ok_or_else(|| s3_error!(MethodNotAllowed))?,
                &address.path,
                mutation::Change::Put {
                    bytes: content.bytes,
                    track_lfs: content.track_lfs,
                    attributes: Box::new(crate::attributes::PutAttributes {
                        etag_override: Some(etag),
                        completion_upload_id: None,
                        logical_size: Some(spool.size),
                        checksums: stored_checksums.clone(),
                        tags: parse_tagging_header(req.input.tagging.as_deref())?,
                        parts: Vec::new(),
                        cache_control: req.input.cache_control,
                        content_disposition: req.input.content_disposition,
                        content_encoding,
                        content_language: req.input.content_language,
                        content_type: req.input.content_type,
                        expires: req.input.expires,
                        metadata: req.input.metadata.unwrap_or_default().into_iter().collect(),
                    }),
                    condition,
                },
                &principal,
                &self.cancellation,
            )
            .await
            .map_err(mutation_error)?;
        Ok(S3Response::new(PutObjectOutput {
            e_tag: outcome.etag.map(ETag::Strong),
            checksum_crc32: stored_checksums.crc32,
            checksum_crc32c: stored_checksums.crc32c,
            checksum_crc64nvme: stored_checksums.crc64nvme,
            checksum_sha1: stored_checksums.sha1,
            checksum_sha256: stored_checksums.sha256,
            checksum_type: stored_checksums.checksum_type.map(ChecksumType::from),
            ..Default::default()
        }))
    }

    async fn get_object_attributes(
        &self,
        req: S3Request<GetObjectAttributesInput>,
    ) -> S3Result<S3Response<GetObjectAttributesOutput>> {
        let _permit = self.admit(RequestClass::Read).await?;
        if req.input.expected_bucket_owner.is_some()
            || req.input.request_payer.is_some()
            || req.input.sse_customer_algorithm.is_some()
            || req.input.sse_customer_key.is_some()
            || req.input.sse_customer_key_md5.is_some()
            || req.input.version_id.is_some()
        {
            return Err(s3_error!(NotImplemented));
        }
        multipart_page_bounds(req.input.part_number_marker, req.input.max_parts)?;
        let repository = self.repository(&req, &req.input.bucket, RepositoryAccess::Read)?;
        let object = self
            .read_object_metadata(repository, &req.input.key)
            .await?;
        let requested = object_attribute_names(&req.input.object_attributes);
        if requested.iter().any(|value| {
            !matches!(
                *value,
                ObjectAttributes::CHECKSUM
                    | ObjectAttributes::ETAG
                    | ObjectAttributes::OBJECT_PARTS
                    | ObjectAttributes::OBJECT_SIZE
                    | ObjectAttributes::STORAGE_CLASS
            )
        }) {
            return Err(s3_error!(InvalidArgument));
        }
        let checksum = requested
            .contains(ObjectAttributes::CHECKSUM)
            .then(|| {
                object
                    .attributes
                    .as_ref()
                    .map(|value| checksum_dto(&value.checksums))
            })
            .flatten();
        let object_parts = requested
            .contains(ObjectAttributes::OBJECT_PARTS)
            .then(|| object.attributes.as_ref().map(|value| &value.parts))
            .flatten()
            .filter(|parts| !parts.is_empty())
            .map(|parts| object_parts(parts, req.input.part_number_marker, req.input.max_parts))
            .transpose()?;
        Ok(S3Response::new(GetObjectAttributesOutput {
            checksum,
            e_tag: requested
                .contains(ObjectAttributes::ETAG)
                .then_some(ETag::Strong(object.etag)),
            last_modified: Some(object.modified),
            object_parts,
            object_size: requested
                .contains(ObjectAttributes::OBJECT_SIZE)
                .then(|| i64::try_from(object.size).map_err(|_| s3_error!(InternalError)))
                .transpose()?,
            ..Default::default()
        }))
    }

    async fn get_object_tagging(
        &self,
        req: S3Request<GetObjectTaggingInput>,
    ) -> S3Result<S3Response<GetObjectTaggingOutput>> {
        let _permit = self.admit(RequestClass::Read).await?;
        if req.input.expected_bucket_owner.is_some()
            || req.input.request_payer.is_some()
            || req.input.version_id.is_some()
        {
            return Err(s3_error!(NotImplemented));
        }
        let repository = self.repository(&req, &req.input.bucket, RepositoryAccess::Read)?;
        let object = self
            .read_object_metadata(repository, &req.input.key)
            .await?;
        let tags = object
            .attributes
            .map(|value| value.tags)
            .unwrap_or_default();
        Ok(S3Response::new(GetObjectTaggingOutput {
            tag_set: tags
                .into_iter()
                .map(|(key, value)| Tag {
                    key: Some(key),
                    value: Some(value),
                })
                .collect(),
            ..Default::default()
        }))
    }

    async fn put_object_tagging(
        &self,
        req: S3Request<PutObjectTaggingInput>,
    ) -> S3Result<S3Response<PutObjectTaggingOutput>> {
        let _permit = self.admit(RequestClass::Transfer).await?;
        if req.input.expected_bucket_owner.is_some()
            || req.input.request_payer.is_some()
            || req.input.version_id.is_some()
        {
            return Err(s3_error!(NotImplemented));
        }
        verify_xml_request_integrity(
            &req,
            req.input.content_md5.as_deref(),
            req.input.checksum_algorithm.as_ref(),
            true,
        )?;
        let (repository, address, principal) =
            self.writable_address(&req, &req.input.bucket, &req.input.key)?;
        let tags = validate_tags(
            req.input
                .tagging
                .tag_set
                .into_iter()
                .map(|tag| {
                    Ok((
                        tag.key.ok_or_else(|| s3_error!(InvalidTag))?,
                        tag.value.ok_or_else(|| s3_error!(InvalidTag))?,
                    ))
                })
                .collect::<S3Result<Vec<_>>>()?,
        )?;
        let object = self
            .read_object_metadata(repository, &req.input.key)
            .await?;
        let mut attributes = object
            .attributes
            .as_ref()
            .map(stored_to_pending)
            .unwrap_or_default();
        attributes.etag_override = Some(object.etag);
        attributes.logical_size = Some(object.size);
        attributes.tags = tags;
        self.mutations
            .apply(
                repository,
                address
                    .branch
                    .as_deref()
                    .ok_or_else(|| s3_error!(MethodNotAllowed))?,
                &address.path,
                mutation::Change::Attributes {
                    expected: object.blob_oid,
                    attributes: Box::new(attributes),
                },
                &principal,
                &self.cancellation,
            )
            .await
            .map_err(mutation_error)?;
        Ok(S3Response::new(PutObjectTaggingOutput::default()))
    }

    async fn delete_object_tagging(
        &self,
        req: S3Request<DeleteObjectTaggingInput>,
    ) -> S3Result<S3Response<DeleteObjectTaggingOutput>> {
        let _permit = self.admit(RequestClass::Transfer).await?;
        if req.input.expected_bucket_owner.is_some() || req.input.version_id.is_some() {
            return Err(s3_error!(NotImplemented));
        }
        let (repository, address, principal) =
            self.writable_address(&req, &req.input.bucket, &req.input.key)?;
        let object = self
            .read_object_metadata(repository, &req.input.key)
            .await?;
        let mut attributes = object
            .attributes
            .as_ref()
            .map(stored_to_pending)
            .unwrap_or_default();
        attributes.etag_override = Some(object.etag);
        attributes.logical_size = Some(object.size);
        attributes.tags.clear();
        self.mutations
            .apply(
                repository,
                address
                    .branch
                    .as_deref()
                    .ok_or_else(|| s3_error!(MethodNotAllowed))?,
                &address.path,
                mutation::Change::Attributes {
                    expected: object.blob_oid,
                    attributes: Box::new(attributes),
                },
                &principal,
                &self.cancellation,
            )
            .await
            .map_err(mutation_error)?;
        Ok(S3Response::new(DeleteObjectTaggingOutput::default()))
    }

    async fn delete_object(
        &self,
        req: S3Request<DeleteObjectInput>,
    ) -> S3Result<S3Response<DeleteObjectOutput>> {
        let _permit = self.admit(RequestClass::Transfer).await?;
        reject_delete_extensions(&req.input)?;
        let marker_address =
            namespace::directory_marker_address(&req.input.key).map_err(namespace_error)?;
        let is_directory_marker = marker_address.is_some();
        let (repository, address, principal) = match marker_address {
            Some(address) => self.writable_object_address(&req, &req.input.bucket, address)?,
            None => self.writable_address(&req, &req.input.bucket, &req.input.key)?,
        };
        if is_directory_marker {
            if req.input.if_match.is_some() {
                return Err(s3_error!(PreconditionFailed));
            }
            return Ok(S3Response::new(DeleteObjectOutput::default()));
        }
        let condition = self
            .delete_condition(repository, &req.input.key, req.input.if_match.as_ref())
            .await?;
        self.mutations
            .apply(
                repository,
                address
                    .branch
                    .as_deref()
                    .ok_or_else(|| s3_error!(MethodNotAllowed))?,
                &address.path,
                mutation::Change::Delete { condition },
                &principal,
                &self.cancellation,
            )
            .await
            .map_err(mutation_error)?;
        Ok(S3Response::new(DeleteObjectOutput::default()))
    }

    async fn delete_objects(
        &self,
        req: S3Request<DeleteObjectsInput>,
    ) -> S3Result<S3Response<DeleteObjectsOutput>> {
        let _permit = self.admit(RequestClass::Transfer).await?;
        if req.input.bypass_governance_retention.is_some()
            || req.input.expected_bucket_owner.is_some()
            || req.input.mfa.is_some()
            || req.input.request_payer.is_some()
        {
            return Err(s3_error!(NotImplemented));
        }
        verify_xml_request_integrity(&req, None, req.input.checksum_algorithm.as_ref(), true)?;
        if req.input.delete.objects.len() > 1000 {
            return Err(s3_error!(
                MalformedXML,
                "DeleteObjects accepts at most 1000 keys"
            ));
        }
        let repository = self.repository(&req, &req.input.bucket, RepositoryAccess::Write)?;
        let principal = self.principal(&req)?.to_owned();
        let quiet = req.input.delete.quiet.unwrap_or(false);
        let mut deleted = Vec::new();
        let mut errors = Vec::new();
        for object in req.input.delete.objects {
            if object.version_id.is_some()
                || object.e_tag.is_some()
                || object.last_modified_time.is_some()
                || object.size.is_some()
            {
                errors.push(s3s::dto::Error {
                    key: Some(object.key),
                    code: Some("NotImplemented".to_owned()),
                    message: Some(
                        "Per-object version and condition fields are unsupported".to_owned(),
                    ),
                    ..Default::default()
                });
                continue;
            }
            let key = object.key;
            let result: S3Result<()> = async {
                let marker_address =
                    namespace::directory_marker_address(&key).map_err(namespace_error)?;
                let is_directory_marker = marker_address.is_some();
                let address = match marker_address {
                    Some(address) => address,
                    None => namespace::object_address(&key).map_err(namespace_error)?,
                };
                let branch = writable_branch(repository, &address)?;
                if is_directory_marker {
                    return Ok(());
                }
                self.mutations
                    .apply(
                        repository,
                        branch,
                        &address.path,
                        mutation::Change::Delete {
                            condition: mutation::DeleteCondition::default(),
                        },
                        &principal,
                        &self.cancellation,
                    )
                    .await
                    .map_err(mutation_error)?;
                Ok(())
            }
            .await;
            match result {
                Ok(_) if !quiet => deleted.push(DeletedObject {
                    key: Some(key),
                    ..Default::default()
                }),
                Ok(_) => {}
                Err(error) => errors.push(s3s::dto::Error {
                    key: Some(key),
                    code: Some(error.code().as_str().to_owned()),
                    message: Some(
                        error
                            .message()
                            .unwrap_or("Object deletion failed")
                            .to_owned(),
                    ),
                    ..Default::default()
                }),
            }
        }
        Ok(S3Response::new(DeleteObjectsOutput {
            deleted: (!deleted.is_empty()).then_some(deleted),
            errors: (!errors.is_empty()).then_some(errors),
            ..Default::default()
        }))
    }

    async fn copy_object(
        &self,
        req: S3Request<CopyObjectInput>,
    ) -> S3Result<S3Response<CopyObjectOutput>> {
        let _permit = self.admit(RequestClass::Transfer).await?;
        reject_copy_extensions(&req.input)?;
        let (source_bucket, source_key) = match &req.input.copy_source {
            CopySource::Bucket {
                bucket,
                key,
                version_id: None,
            } => (bucket.to_string(), key.to_string()),
            CopySource::Bucket {
                version_id: Some(_),
                ..
            } => {
                return Err(s3_error!(
                    NotImplemented,
                    "Copying an object version is unsupported"
                ));
            }
            CopySource::AccessPoint { .. } | CopySource::Outpost { .. } => {
                return Err(s3_error!(
                    NotImplemented,
                    "Copy access points are unsupported"
                ));
            }
        };
        let source_repository = self.repository(&req, &source_bucket, RepositoryAccess::Read)?;
        let source_object = self.read_object(source_repository, &source_key).await?;
        evaluate_conditions(
            req.input.copy_source_if_match.as_ref(),
            req.input.copy_source_if_none_match.as_ref(),
            req.input.copy_source_if_modified_since.as_ref(),
            req.input.copy_source_if_unmodified_since.as_ref(),
            &source_object.etag,
            &source_object.modified,
        )?;
        let source_size = source_object.size;
        if source_size > crate::content::MAX_PUT_OBJECT_BYTES {
            return Err(s3_error!(EntityTooLarge));
        }
        let spool = source_object
            .content
            .spool(
                source_repository,
                0..source_size,
                crate::content::MAX_PUT_OBJECT_BYTES,
                &self.metrics,
            )
            .await?;
        let (repository, address, principal) =
            self.writable_address(&req, &req.input.bucket, &req.input.key)?;
        let mut attributes = if req
            .input
            .metadata_directive
            .as_ref()
            .is_some_and(|value| value.as_str() == MetadataDirective::REPLACE)
        {
            crate::attributes::PutAttributes {
                etag_override: None,
                completion_upload_id: None,
                logical_size: None,
                checksums: crate::attributes::Checksums::default(),
                tags: BTreeMap::new(),
                parts: Vec::new(),
                cache_control: req.input.cache_control,
                content_disposition: req.input.content_disposition,
                content_encoding: req.input.content_encoding,
                content_language: req.input.content_language,
                content_type: req.input.content_type,
                expires: req.input.expires,
                metadata: req.input.metadata.unwrap_or_default().into_iter().collect(),
            }
        } else {
            source_object
                .attributes
                .as_ref()
                .map(stored_to_pending)
                .unwrap_or_default()
        };
        // A copy creates a new S3 object version even when the source came
        // from multipart upload; completion identity and part layout remain
        // properties of the source object only.
        attributes.completion_upload_id = None;
        attributes.parts.clear();
        let replace_tags = req
            .input
            .tagging_directive
            .as_ref()
            .is_some_and(|value| value.as_str() == TaggingDirective::REPLACE);
        if req.input.tagging.is_some() && !replace_tags {
            return Err(s3_error!(InvalidRequest));
        }
        attributes.tags = if replace_tags {
            parse_tagging_header(req.input.tagging.as_deref())?
        } else {
            source_object
                .attributes
                .as_ref()
                .map(|value| value.tags.clone())
                .unwrap_or_default()
        };
        attributes.etag_override = Some(crate::content::md5_hex(&spool.digests.md5));
        attributes.logical_size = Some(spool.size);
        attributes.checksums = match req.input.checksum_algorithm.as_ref() {
            Some(algorithm) => calculated_checksums(&spool.digests, algorithm.as_str())?,
            None => source_object
                .attributes
                .as_ref()
                .map(|value| value.checksums.clone())
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| default_checksums(&spool.digests)),
        };
        let response_checksums = attributes.checksums.clone();
        let content = mutation_bytes(repository, &spool).await?;
        let outcome = self
            .mutations
            .apply(
                repository,
                address
                    .branch
                    .as_deref()
                    .ok_or_else(|| s3_error!(MethodNotAllowed))?,
                &address.path,
                mutation::Change::Put {
                    bytes: content.bytes,
                    track_lfs: content.track_lfs,
                    attributes: Box::new(attributes),
                    condition: mutation::PutCondition::None,
                },
                &principal,
                &self.cancellation,
            )
            .await
            .map_err(mutation_error)?;
        Ok(S3Response::new(CopyObjectOutput {
            copy_object_result: Some(CopyObjectResult {
                e_tag: outcome.etag.map(ETag::Strong),
                checksum_crc32: response_checksums.crc32,
                checksum_crc32c: response_checksums.crc32c,
                checksum_crc64nvme: response_checksums.crc64nvme,
                checksum_sha1: response_checksums.sha1,
                checksum_sha256: response_checksums.sha256,
                checksum_type: response_checksums.checksum_type.map(ChecksumType::from),
                last_modified: Some(timestamp(
                    i64::try_from(
                        std::time::SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .map_err(|_| s3_error!(InternalError))?
                            .as_secs(),
                    )
                    .map_err(|_| s3_error!(InternalError))?,
                )?),
            }),
            ..Default::default()
        }))
    }

    async fn create_multipart_upload(
        &self,
        req: S3Request<CreateMultipartUploadInput>,
    ) -> S3Result<S3Response<CreateMultipartUploadOutput>> {
        let _permit = self.admit(RequestClass::Control).await?;
        reject_create_multipart_extensions(&req.input)?;
        let (checksum_algorithm, checksum_type) = multipart_checksum_profile(
            req.input.checksum_algorithm.as_ref(),
            req.input.checksum_type.as_ref(),
        )?;
        let (repository, address, principal) =
            self.writable_address(&req, &req.input.bucket, &req.input.key)?;
        let branch = address
            .branch
            .as_deref()
            .ok_or_else(|| s3_error!(MethodNotAllowed))?;
        let path =
            std::str::from_utf8(address.path.as_bytes()).map_err(|_| s3_error!(InvalidArgument))?;
        let session = crate::multipart::create(
            repository,
            crate::multipart::Initiation {
                bucket: &req.input.bucket,
                key: &req.input.key,
                branch,
                path,
                principal: &principal,
                attributes: crate::attributes::PutAttributes {
                    etag_override: None,
                    completion_upload_id: None,
                    logical_size: None,
                    checksums: crate::attributes::Checksums::default(),
                    tags: parse_tagging_header(req.input.tagging.as_deref())?,
                    parts: Vec::new(),
                    cache_control: req.input.cache_control,
                    content_disposition: req.input.content_disposition,
                    content_encoding: req.input.content_encoding,
                    content_language: req.input.content_language,
                    content_type: req.input.content_type,
                    expires: req.input.expires,
                    metadata: req.input.metadata.unwrap_or_default().into_iter().collect(),
                },
                checksum_algorithm,
                checksum_type,
                now: now_seconds()?,
            },
        )
        .await
        .map_err(multipart_error)?;
        Ok(S3Response::new(CreateMultipartUploadOutput {
            bucket: Some(req.input.bucket),
            key: Some(req.input.key),
            checksum_algorithm: session
                .checksum_algorithm
                .clone()
                .map(ChecksumAlgorithm::from),
            checksum_type: session.checksum_type.clone().map(ChecksumType::from),
            upload_id: Some(session.id),
            ..Default::default()
        }))
    }

    async fn upload_part(
        &self,
        req: S3Request<UploadPartInput>,
    ) -> S3Result<S3Response<UploadPartOutput>> {
        let _permit = self.admit(RequestClass::Transfer).await?;
        reject_upload_part_extensions(&req.input)?;
        let repository = self.repository(&req, &req.input.bucket, RepositoryAccess::Write)?;
        let principal = self.principal(&req)?.to_owned();
        let now = now_seconds()?;
        let loaded = crate::multipart::load_open(repository, &req.input.upload_id, now)
            .await
            .map_err(multipart_error)?;
        crate::multipart::authorize(
            &loaded.session,
            &req.input.bucket,
            &req.input.key,
            &principal,
        )
        .map_err(multipart_error)?;
        let content_length = req.input.content_length;
        let content_md5 = req.input.content_md5.clone();
        let mut checksums = RequestChecksums::from_upload_part(&req.input);
        let trailing_headers = req.trailing_headers.clone();
        let spool = crate::content::spool_body(
            req.input.body,
            content_length,
            crate::content::MAX_MULTIPART_PART_BYTES,
            &self.metrics,
        )
        .await
        .map_err(content_error)?;
        checksums.merge_trailers(trailing_headers.as_ref())?;
        checksums.ensure_single_value()?;
        if let Some(algorithm) = loaded.session.checksum_algorithm.as_deref()
            && (checksums
                .algorithm
                .as_ref()
                .is_some_and(|requested| requested.as_str() != algorithm)
                || (!checksums.values_empty() && !checksums.has_algorithm(algorithm))
                || (loaded.session.checksum_type.as_deref() == Some(ChecksumType::COMPOSITE)
                    && checksums.values_empty()))
        {
            return Err(s3_error!(
                InvalidRequest,
                "Multipart checksum algorithm mismatch"
            ));
        }
        verify_content_md5(&spool.digests.md5, content_md5.as_deref())?;
        checksums.verify_values(&spool.digests)?;
        let stored_checksums = checksums
            .stored_for_algorithm(&spool.digests, loaded.session.checksum_algorithm.as_deref())?;
        let etag = crate::content::md5_hex(&spool.digests.md5);
        crate::multipart::register_part(
            repository,
            loaded,
            req.input.part_number,
            &spool,
            etag.clone(),
            stored_checksums.clone(),
            now_seconds()?,
            &self.cancellation,
        )
        .await
        .map_err(multipart_error)?;
        Ok(S3Response::new(UploadPartOutput {
            e_tag: Some(ETag::Strong(etag)),
            checksum_crc32: stored_checksums.crc32,
            checksum_crc32c: stored_checksums.crc32c,
            checksum_crc64nvme: stored_checksums.crc64nvme,
            checksum_sha1: stored_checksums.sha1,
            checksum_sha256: stored_checksums.sha256,
            ..Default::default()
        }))
    }

    async fn upload_part_copy(
        &self,
        req: S3Request<UploadPartCopyInput>,
    ) -> S3Result<S3Response<UploadPartCopyOutput>> {
        let _permit = self.admit(RequestClass::Transfer).await?;
        if req.input.copy_source_sse_customer_algorithm.is_some()
            || req.input.copy_source_sse_customer_key.is_some()
            || req.input.copy_source_sse_customer_key_md5.is_some()
            || req.input.expected_bucket_owner.is_some()
            || req.input.expected_source_bucket_owner.is_some()
            || req.input.request_payer.is_some()
            || req.input.sse_customer_algorithm.is_some()
            || req.input.sse_customer_key.is_some()
            || req.input.sse_customer_key_md5.is_some()
        {
            return Err(s3_error!(NotImplemented));
        }
        let (source_bucket, source_key) = copy_source_bucket_key(&req.input.copy_source)?;
        let source_repository = self.repository(&req, &source_bucket, RepositoryAccess::Read)?;
        let source = self.read_object(source_repository, &source_key).await?;
        evaluate_conditions(
            req.input.copy_source_if_match.as_ref(),
            req.input.copy_source_if_none_match.as_ref(),
            req.input.copy_source_if_modified_since.as_ref(),
            req.input.copy_source_if_unmodified_since.as_ref(),
            &source.etag,
            &source.modified,
        )?;
        let range = match req.input.copy_source_range.as_deref() {
            Some(value) => {
                let range = Range::parse(value).map_err(|_| s3_error!(InvalidArgument))?;
                let range = range.check(source.size)?;
                range.start..range.end
            }
            None => 0..source.size,
        };
        if range.end - range.start > crate::content::MAX_MULTIPART_PART_BYTES {
            return Err(s3_error!(EntityTooLarge));
        }
        let spool = source
            .content
            .spool(
                source_repository,
                range,
                crate::content::MAX_MULTIPART_PART_BYTES,
                &self.metrics,
            )
            .await?;
        let repository = self.repository(&req, &req.input.bucket, RepositoryAccess::Write)?;
        let principal = self.principal(&req)?.to_owned();
        let now = now_seconds()?;
        let loaded = crate::multipart::load_open(repository, &req.input.upload_id, now)
            .await
            .map_err(multipart_error)?;
        crate::multipart::authorize(
            &loaded.session,
            &req.input.bucket,
            &req.input.key,
            &principal,
        )
        .map_err(multipart_error)?;
        let part_checksums = loaded
            .session
            .checksum_algorithm
            .as_deref()
            .map(|algorithm| calculated_checksums(&spool.digests, algorithm))
            .transpose()?
            .unwrap_or_else(|| default_checksums(&spool.digests));
        let etag = crate::content::md5_hex(&spool.digests.md5);
        crate::multipart::register_part(
            repository,
            loaded,
            req.input.part_number,
            &spool,
            etag.clone(),
            part_checksums.clone(),
            now,
            &self.cancellation,
        )
        .await
        .map_err(multipart_error)?;
        Ok(S3Response::new(UploadPartCopyOutput {
            copy_part_result: Some(CopyPartResult {
                e_tag: Some(ETag::Strong(etag)),
                checksum_crc32: part_checksums.crc32,
                checksum_crc32c: part_checksums.crc32c,
                checksum_crc64nvme: part_checksums.crc64nvme,
                checksum_sha1: part_checksums.sha1,
                checksum_sha256: part_checksums.sha256,
                last_modified: Some(timestamp(
                    i64::try_from(now_seconds()?).map_err(|_| s3_error!(InternalError))?,
                )?),
            }),
            ..Default::default()
        }))
    }

    async fn complete_multipart_upload(
        &self,
        req: S3Request<CompleteMultipartUploadInput>,
    ) -> S3Result<S3Response<CompleteMultipartUploadOutput>> {
        let _permit = self.admit(RequestClass::Transfer).await?;
        reject_complete_multipart_extensions(&req.input)?;
        let if_match = req.input.if_match.clone();
        let if_none_match = req.input.if_none_match.clone();
        let completion_checksums = RequestChecksums::from_complete(&req.input);
        completion_checksums.ensure_single_value()?;
        let requested_checksum_type = req.input.checksum_type.clone();
        let expected_size = req.input.mpu_object_size;
        let repository = self.repository(&req, &req.input.bucket, RepositoryAccess::Write)?;
        let principal = self.principal(&req)?.to_owned();
        let loaded =
            crate::multipart::load_completion(repository, &req.input.upload_id, now_seconds()?)
                .await
                .map_err(multipart_error)?;
        crate::multipart::authorize(
            &loaded.session,
            &req.input.bucket,
            &req.input.key,
            &principal,
        )
        .map_err(multipart_error)?;
        if requested_checksum_type.as_ref().map(ChecksumType::as_str)
            != loaded.session.checksum_type.as_deref()
            && requested_checksum_type.is_some()
        {
            return Err(s3_error!(BadDigest));
        }
        if let Some(algorithm) = loaded.session.checksum_algorithm.as_deref()
            && !completion_checksums.values_empty()
            && !completion_checksums.has_algorithm(algorithm)
        {
            return Err(s3_error!(BadDigest));
        }
        let composite_algorithm = (loaded.session.checksum_type.as_deref()
            == Some(ChecksumType::COMPOSITE))
        .then(|| loaded.session.checksum_algorithm.clone())
        .flatten();
        let selected = req
            .input
            .multipart_upload
            .and_then(|upload| upload.parts)
            .ok_or_else(|| s3_error!(InvalidPart))?
            .into_iter()
            .map(|part| {
                let number = part.part_number.ok_or_else(|| s3_error!(InvalidPart))?;
                let etag = match part.e_tag.as_ref().ok_or_else(|| s3_error!(InvalidPart))? {
                    ETag::Strong(value) => value.clone(),
                    ETag::Weak(_) => return Err(s3_error!(InvalidPart)),
                };
                let stored = loaded
                    .session
                    .parts
                    .get(&number)
                    .ok_or_else(|| s3_error!(InvalidPart))?;
                completed_part_checksums_match(&part, &stored.checksums)?;
                if let Some(algorithm) = composite_algorithm.as_deref()
                    && completed_part_checksum_value(&part, algorithm)
                        != checksum_value(&stored.checksums, algorithm)
                {
                    return Err(s3_error!(InvalidPart));
                }
                Ok((number, etag))
            })
            .collect::<S3Result<Vec<_>>>()?;
        if composite_algorithm.is_some()
            && selected
                .iter()
                .zip(1_i32..)
                .any(|((number, _), expected)| *number != expected)
        {
            return Err(s3_error!(InvalidPartOrder));
        }
        if let Some(response) = completed_multipart_response(
            &loaded.session,
            &selected,
            &req.input.bucket,
            &req.input.key,
        )? {
            return Ok(response);
        }
        let loaded = if crate::multipart::is_completing(&loaded.session) {
            let recovered = self.recover_published_multipart(repository, loaded).await?;
            let loaded =
                crate::multipart::load_completion(repository, &req.input.upload_id, now_seconds()?)
                    .await
                    .map_err(multipart_error)?;
            if recovered {
                return completed_multipart_response(
                    &loaded.session,
                    &selected,
                    &req.input.bucket,
                    &req.input.key,
                )?
                .ok_or_else(|| s3_error!(InternalError));
            }
            loaded
        } else {
            loaded
        };
        let condition = self
            .put_condition_values(repository, &req.input.key, if_match, if_none_match)
            .await?;
        let max_object_bytes =
            crate::content::max_multipart_object_bytes(repository.config.provider);
        let (session, parts) =
            crate::multipart::freeze(repository, loaded, &selected, max_object_bytes)
                .await
                .map_err(multipart_error)?;
        let selected_size = parts.iter().try_fold(0_u64, |total, part| {
            total
                .checked_add(part.size)
                .ok_or_else(|| s3_error!(EntityTooLarge))
        })?;
        let mut writer = MultipartAssemblyWriter::new(selected_size, &self.metrics)
            .await
            .map_err(content_error)?;
        for part in &parts {
            use md5::Digest as _;

            let mut stream = crate::multipart::part_stream(repository, part)
                .await
                .map_err(multipart_error)?;
            let mut digest = md5::Md5::new();
            let mut size = 0_u64;
            while let Some(chunk) = stream.next().await {
                let chunk = chunk
                    .map_err(|error| multipart_error(crate::multipart::Error::Storage(error)))?;
                size = size
                    .checked_add(chunk.len() as u64)
                    .ok_or_else(|| s3_error!(EntityTooLarge))?;
                digest.update(&chunk);
                writer
                    .write(&chunk, selected_size)
                    .await
                    .map_err(content_error)?;
            }
            let actual = digest
                .finalize()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            if size != part.size || actual != part.etag {
                return Err(s3_error!(InvalidPart));
            }
        }
        let assembly = writer.finish().await.map_err(content_error)?;
        let actual_size = i64::try_from(assembly.size()).map_err(|_| s3_error!(EntityTooLarge))?;
        if expected_size.is_some_and(|expected| expected != actual_size) {
            return Err(s3_error!(InvalidRequest, "Multipart object size mismatch"));
        }
        let stored_checksums = if let Some(algorithm) = composite_algorithm.as_deref() {
            let calculated =
                composite_checksums(parts.iter().map(|part| &part.checksums), algorithm)?;
            if !request_checksums_match(&completion_checksums, &calculated) {
                return Err(s3_error!(BadDigest));
            }
            calculated
        } else {
            completion_checksums.verify_values(assembly.digests())?;
            completion_checksums
                .stored_for_algorithm(assembly.digests(), session.checksum_algorithm.as_deref())?
        };
        let etag = multipart_etag(&parts)?;
        let session = crate::multipart::record_completion_outcome(
            repository,
            &session.id,
            &etag,
            &stored_checksums,
        )
        .await
        .map_err(multipart_error)?;
        let mut attributes = session.attributes.clone();
        attributes.etag_override = Some(etag.clone());
        attributes.completion_upload_id = Some(session.id.clone());
        attributes.logical_size = Some(assembly.size());
        attributes.checksums = stored_checksums.clone();
        attributes.parts = parts
            .iter()
            .map(|part| crate::attributes::PartAttributes {
                number: part.number,
                size: part.size,
                checksums: part.checksums.clone(),
            })
            .collect();
        let address = namespace::object_address(&session.key).map_err(namespace_error)?;
        let content = mutation_multipart_bytes(repository, &parts, assembly).await?;
        self.mutations
            .apply(
                repository,
                &session.branch,
                &address.path,
                mutation::Change::Put {
                    bytes: content.bytes,
                    track_lfs: content.track_lfs,
                    attributes: Box::new(attributes),
                    condition,
                },
                &principal,
                &self.cancellation,
            )
            .await
            .map_err(mutation_error)?;
        let loaded = crate::multipart::load(repository, &session.id)
            .await
            .map_err(multipart_error)?;
        crate::multipart::complete(repository, loaded, etag.clone(), stored_checksums.clone())
            .await
            .map_err(multipart_error)?;
        Ok(S3Response::new(CompleteMultipartUploadOutput {
            bucket: Some(req.input.bucket),
            key: Some(req.input.key),
            e_tag: Some(ETag::Strong(etag)),
            checksum_crc32: stored_checksums.crc32,
            checksum_crc32c: stored_checksums.crc32c,
            checksum_crc64nvme: stored_checksums.crc64nvme,
            checksum_sha1: stored_checksums.sha1,
            checksum_sha256: stored_checksums.sha256,
            checksum_type: stored_checksums.checksum_type.map(ChecksumType::from),
            ..Default::default()
        }))
    }

    async fn abort_multipart_upload(
        &self,
        req: S3Request<AbortMultipartUploadInput>,
    ) -> S3Result<S3Response<AbortMultipartUploadOutput>> {
        let _permit = self.admit(RequestClass::Control).await?;
        if req.input.expected_bucket_owner.is_some()
            || req.input.if_match_initiated_time.is_some()
            || req.input.request_payer.is_some()
        {
            return Err(s3_error!(NotImplemented));
        }
        let repository = self.repository(&req, &req.input.bucket, RepositoryAccess::Write)?;
        let principal = self.principal(&req)?.to_owned();
        let loaded = crate::multipart::load_open(repository, &req.input.upload_id, now_seconds()?)
            .await
            .map_err(multipart_error)?;
        crate::multipart::authorize(
            &loaded.session,
            &req.input.bucket,
            &req.input.key,
            &principal,
        )
        .map_err(multipart_error)?;
        crate::multipart::abort(repository, loaded)
            .await
            .map_err(multipart_error)?;
        Ok(S3Response::new(AbortMultipartUploadOutput::default()))
    }

    async fn list_parts(
        &self,
        req: S3Request<ListPartsInput>,
    ) -> S3Result<S3Response<ListPartsOutput>> {
        let _permit = self.admit(RequestClass::Control).await?;
        if req.input.expected_bucket_owner.is_some()
            || req.input.request_payer.is_some()
            || req.input.sse_customer_algorithm.is_some()
            || req.input.sse_customer_key.is_some()
            || req.input.sse_customer_key_md5.is_some()
        {
            return Err(s3_error!(NotImplemented));
        }
        let repository = self.repository(&req, &req.input.bucket, RepositoryAccess::Write)?;
        let principal = self.principal(&req)?;
        let loaded = crate::multipart::load_open(repository, &req.input.upload_id, now_seconds()?)
            .await
            .map_err(multipart_error)?;
        crate::multipart::authorize(
            &loaded.session,
            &req.input.bucket,
            &req.input.key,
            principal,
        )
        .map_err(multipart_error)?;
        let (marker, max) =
            multipart_page_bounds(req.input.part_number_marker, req.input.max_parts)?;
        let mut parts = loaded
            .session
            .parts
            .values()
            .filter(|part| part.number > marker)
            .cloned()
            .collect::<Vec<_>>();
        let truncated = parts.len() > max as usize;
        parts.truncate(max as usize);
        let next = truncated
            .then(|| parts.last().map(|part| part.number))
            .flatten();
        let checksum_algorithm = loaded
            .session
            .checksum_algorithm
            .clone()
            .map(ChecksumAlgorithm::from);
        let checksum_type = loaded.session.checksum_type.clone().map(ChecksumType::from);
        Ok(S3Response::new(ListPartsOutput {
            bucket: Some(req.input.bucket),
            key: Some(req.input.key),
            upload_id: Some(req.input.upload_id),
            max_parts: Some(max),
            part_number_marker: Some(marker),
            next_part_number_marker: next,
            is_truncated: Some(truncated),
            checksum_algorithm,
            checksum_type,
            parts: Some(
                parts
                    .into_iter()
                    .map(|part| {
                        Ok(Part {
                            e_tag: Some(ETag::Strong(part.etag)),
                            checksum_crc32: part.checksums.crc32,
                            checksum_crc32c: part.checksums.crc32c,
                            checksum_crc64nvme: part.checksums.crc64nvme,
                            checksum_sha1: part.checksums.sha1,
                            checksum_sha256: part.checksums.sha256,
                            last_modified: Some(timestamp(
                                i64::try_from(part.modified_seconds)
                                    .map_err(|_| s3_error!(InternalError))?,
                            )?),
                            part_number: Some(part.number),
                            size: Some(
                                i64::try_from(part.size).map_err(|_| s3_error!(InternalError))?,
                            ),
                        })
                    })
                    .collect::<S3Result<Vec<_>>>()?,
            ),
            ..Default::default()
        }))
    }

    async fn list_multipart_uploads(
        &self,
        req: S3Request<ListMultipartUploadsInput>,
    ) -> S3Result<S3Response<ListMultipartUploadsOutput>> {
        let _permit = self.admit(RequestClass::Control).await?;
        if req
            .input
            .delimiter
            .as_deref()
            .is_some_and(|value| value != "/")
            || req.input.expected_bucket_owner.is_some()
            || req.input.request_payer.is_some()
        {
            return Err(s3_error!(NotImplemented));
        }
        let url_encode = list_url_encoding(req.input.encoding_type.as_ref())?;
        let repository = self.repository(&req, &req.input.bucket, RepositoryAccess::Write)?;
        let principal = self.principal(&req)?;
        let prefix = req.input.prefix.as_deref().unwrap_or("");
        let sessions = crate::multipart::list(repository, now_seconds()?)
            .await
            .map_err(multipart_error)?
            .into_iter()
            .filter(|session| session.principal == principal && session.key.starts_with(prefix))
            .collect::<Vec<_>>();
        let mut groups = std::collections::BTreeSet::new();
        let mut projected = Vec::new();
        for session in sessions {
            if req.input.delimiter.as_deref() == Some("/")
                && let Some(relative) = session.key.strip_prefix(prefix)
                && let Some(position) = relative.find('/')
            {
                groups.insert(format!("{}{}", prefix, &relative[..=position]));
                continue;
            }
            projected.push((session.key.clone(), session.id.clone(), Some(session), None));
        }
        projected.extend(groups.into_iter().map(|prefix| {
            (
                prefix.clone(),
                String::new(),
                None,
                Some(CommonPrefix {
                    prefix: Some(prefix),
                }),
            )
        }));
        projected.sort_by(|left, right| (&left.0, &left.1).cmp(&(&right.0, &right.1)));
        if let Some(key_marker) = req.input.key_marker.as_deref() {
            projected.retain(|item| match req.input.upload_id_marker.as_deref() {
                Some(upload_id_marker) if item.0 == key_marker => {
                    item.1.as_str() > upload_id_marker
                }
                _ => item.0.as_str() > key_marker,
            });
        }
        let max = req.input.max_uploads.unwrap_or(1000);
        if !(1..=1000).contains(&max) {
            return Err(s3_error!(InvalidArgument));
        }
        let truncated = projected.len() > max as usize;
        projected.truncate(max as usize);
        let (next_key, next_id) = if truncated {
            projected
                .last()
                .map(|item| {
                    (
                        Some(item.0.clone()),
                        (!item.1.is_empty()).then(|| item.1.clone()),
                    )
                })
                .unwrap_or_default()
        } else {
            (None, None)
        };
        let mut sessions = projected
            .iter_mut()
            .filter_map(|item| item.2.take())
            .collect::<Vec<_>>();
        let mut common_prefixes = projected
            .iter_mut()
            .filter_map(|item| item.3.take())
            .collect::<Vec<_>>();
        if url_encode {
            for session in &mut sessions {
                session.key = encode_list_value(&session.key, true);
            }
            for group in &mut common_prefixes {
                group.prefix = encode_list_option(group.prefix.take(), true);
            }
        }
        Ok(S3Response::new(ListMultipartUploadsOutput {
            bucket: Some(req.input.bucket),
            prefix: encode_list_option(req.input.prefix, url_encode),
            delimiter: encode_list_option(req.input.delimiter, url_encode),
            encoding_type: req.input.encoding_type,
            key_marker: encode_list_option(req.input.key_marker, url_encode),
            upload_id_marker: req.input.upload_id_marker,
            max_uploads: Some(max),
            is_truncated: Some(truncated),
            next_key_marker: encode_list_option(next_key, url_encode),
            next_upload_id_marker: next_id,
            common_prefixes: (!common_prefixes.is_empty()).then_some(common_prefixes),
            uploads: Some(
                sessions
                    .into_iter()
                    .map(|session| {
                        Ok(MultipartUpload {
                            key: Some(session.key),
                            upload_id: Some(session.id),
                            checksum_algorithm: session
                                .checksum_algorithm
                                .map(ChecksumAlgorithm::from),
                            checksum_type: session.checksum_type.map(ChecksumType::from),
                            initiated: Some(timestamp(
                                i64::try_from(session.created_seconds)
                                    .map_err(|_| s3_error!(InternalError))?,
                            )?),
                            storage_class: Some(StorageClass::from_static("STANDARD")),
                            ..Default::default()
                        })
                    })
                    .collect::<S3Result<Vec<_>>>()?,
            ),
            ..Default::default()
        }))
    }

    async fn list_objects(
        &self,
        req: S3Request<ListObjectsInput>,
    ) -> S3Result<S3Response<ListObjectsOutput>> {
        let marker = req.input.marker.clone();
        let response = self.list_objects_v2(req.map_input(Into::into)).await?;
        Ok(response.map_output(|output| {
            let url_encode = output.encoding_type.is_some();
            ListObjectsOutput {
                name: output.name,
                prefix: output.prefix,
                marker: encode_list_option(marker, url_encode),
                max_keys: output.max_keys,
                is_truncated: output.is_truncated,
                contents: output.contents,
                common_prefixes: output.common_prefixes,
                delimiter: output.delimiter,
                next_marker: encode_list_option(output.next_continuation_token, url_encode),
                encoding_type: output.encoding_type,
                ..Default::default()
            }
        }))
    }

    async fn list_objects_v2(
        &self,
        req: S3Request<ListObjectsV2Input>,
    ) -> S3Result<S3Response<ListObjectsV2Output>> {
        let _permit = self.admit(RequestClass::Read).await?;
        let repository = self.repository(&req, &req.input.bucket, RepositoryAccess::Read)?;
        let url_encode = list_url_encoding(req.input.encoding_type.as_ref())?;
        reject_list_extensions(&req.input)?;
        let prefix = req.input.prefix.as_deref().unwrap_or("");
        let Some((reference, path_prefix)) =
            namespace::listing_reference(prefix).map_err(namespace_error)?
        else {
            return self.list_refs(&req, repository).await;
        };
        let max_keys = req.input.max_keys.unwrap_or(1000);
        if max_keys < 0 {
            return Err(s3_error!(InvalidArgument));
        }
        let max_keys = max_keys.min(1000);
        let limit = usize::try_from(max_keys).map_err(|_| s3_error!(InvalidArgument))?;
        let repo = self.open(repository).await?;
        if repo.remote().refs().find(&reference).is_none() {
            return empty_list_objects_response(&req, url_encode);
        }
        if limit == 0 {
            return empty_list_objects_response(&req, url_encode);
        }
        let operation = repo
            .remote()
            .operation(OperationKind::Repository, &self.cancellation)
            .await
            .map_err(remote_error)?;
        let result = async {
            let snapshot = repo
                .snapshot(&reference, &operation)
                .await
                .map_err(gateway_error)?;
            let commit = snapshot.commit(&operation).await.map_err(remote_error)?;
            let commit_modified = timestamp(commit.committer.seconds)?;
            let encoded_ref = prefix
                .split_once('/')
                .map(|(value, _)| value)
                .unwrap_or_default();
            let ref_prefix = format!("{encoded_ref}/");
            let after = req
                .input
                .continuation_token
                .as_deref()
                .or(req.input.start_after.as_deref());
            let after = match after {
                Some(after) => match after.strip_prefix(&ref_prefix) {
                    Some(path) => Some(Bytes::copy_from_slice(path.as_bytes())),
                    None if after < ref_prefix.as_str() => None,
                    None => return empty_list_objects_response(&req, url_encode),
                },
                None => None,
            };
            let listing = snapshot
                .list_tree_blobs(
                    &crab_remote_git::TreeListingRequest::new(
                        Bytes::copy_from_slice(path_prefix.as_bytes()),
                        after,
                        req.input.delimiter.as_deref().map(|_| b'/'),
                        limit,
                    )
                    .map_err(remote_error)?,
                    &operation,
                )
                .await
                .map_err(remote_error)?;
            let paths = listing
                .items
                .iter()
                .map(|item| {
                    std::str::from_utf8(item.path())
                        .map(str::to_owned)
                        .map_err(|_| s3_error!(InvalidObjectState))
                })
                .collect::<S3Result<Vec<_>>>()?;
            let page_objects = listing
                .items
                .iter()
                .zip(&paths)
                .filter_map(|(item, path)| match item {
                    crab_remote_git::TreeListingItem::Blob(entry) => {
                        Some((path.clone(), entry.oid))
                    }
                    crab_remote_git::TreeListingItem::CommonPrefix(_) => None,
                })
                .collect::<Vec<_>>();
            let page_attributes =
                crate::attributes::load_objects(repository, snapshot.commit_oid(), &page_objects)
                    .await
                    .map_err(gateway_error)?;
            let mut contents = Vec::new();
            let mut common_prefixes = Vec::new();
            let mut next = None;
            for (item, path) in listing.items.into_iter().zip(paths) {
                let key = format!("{encoded_ref}/{path}");
                next = Some(key.clone());
                match item {
                    crab_remote_git::TreeListingItem::CommonPrefix(_) => {
                        common_prefixes.push(CommonPrefix { prefix: Some(key) });
                    }
                    crab_remote_git::TreeListingItem::Blob(entry) => {
                        let attributes = page_attributes.get(&path);
                        let modified = match attributes {
                            Some(attributes) => timestamp(
                                i64::try_from(attributes.modified_seconds)
                                    .map_err(|_| s3_error!(InternalError))?,
                            )?,
                            None => commit_modified.clone(),
                        };
                        let (etag, logical_size) = match attributes {
                            Some(attributes) => (attributes.etag.clone(), attributes.size),
                            None => {
                                let blob = snapshot
                                    .read_blob(&entry.path, &operation)
                                    .await
                                    .map_err(remote_error)?;
                                let (content, logical_size) = classify_blob(blob)?;
                                (content.projected_etag()?, logical_size)
                            }
                        };
                        contents.push(Object {
                            key: Some(key),
                            e_tag: Some(ETag::Strong(etag)),
                            last_modified: Some(modified),
                            size: i64::try_from(logical_size).ok(),
                            storage_class: Some(ObjectStorageClass::from_static("STANDARD")),
                            ..Default::default()
                        });
                    }
                }
            }
            let truncated = listing.has_more;
            let next = truncated.then_some(next).flatten();
            let key_count = contents.len().saturating_add(common_prefixes.len());
            if url_encode {
                for object in &mut contents {
                    object.key = encode_list_option(object.key.take(), true);
                }
                for group in &mut common_prefixes {
                    group.prefix = encode_list_option(group.prefix.take(), true);
                }
            }
            Ok(S3Response::new(ListObjectsV2Output {
                name: Some(req.input.bucket.clone()),
                prefix: encode_list_option(req.input.prefix.clone(), url_encode),
                max_keys: Some(max_keys),
                key_count: Some(i32::try_from(key_count).map_err(|_| s3_error!(InternalError))?),
                continuation_token: req.input.continuation_token.clone(),
                next_continuation_token: next,
                is_truncated: Some(truncated),
                contents: (!contents.is_empty()).then_some(contents),
                common_prefixes: (!common_prefixes.is_empty()).then_some(common_prefixes),
                delimiter: encode_list_option(req.input.delimiter.clone(), url_encode),
                encoding_type: req.input.encoding_type.clone(),
                start_after: encode_list_option(req.input.start_after.clone(), url_encode),
                ..Default::default()
            }))
        }
        .await;
        finish(operation, result).await
    }
}

fn empty_list_objects_response(
    req: &S3Request<ListObjectsV2Input>,
    url_encode: bool,
) -> S3Result<S3Response<ListObjectsV2Output>> {
    let max_keys = req.input.max_keys.unwrap_or(1000);
    if max_keys < 0 {
        return Err(s3_error!(InvalidArgument));
    }
    Ok(S3Response::new(ListObjectsV2Output {
        name: Some(req.input.bucket.clone()),
        prefix: encode_list_option(req.input.prefix.clone(), url_encode),
        max_keys: Some(max_keys.min(1000)),
        key_count: Some(0),
        continuation_token: req.input.continuation_token.clone(),
        is_truncated: Some(false),
        delimiter: encode_list_option(req.input.delimiter.clone(), url_encode),
        encoding_type: req.input.encoding_type.clone(),
        start_after: encode_list_option(req.input.start_after.clone(), url_encode),
        ..Default::default()
    }))
}

fn response_override<T: Clone>(requested: Option<T>, stored: Option<&T>) -> Option<T> {
    requested.or_else(|| stored.cloned())
}

fn verify_content_md5(actual: &[u8; 16], expected: Option<&str>) -> S3Result<()> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let expected = base64::engine::general_purpose::STANDARD
        .decode(expected)
        .map_err(|_| s3_error!(InvalidDigest))?;
    if expected.as_slice() != actual {
        return Err(s3_error!(BadDigest));
    }
    Ok(())
}

fn verify_xml_request_integrity<T>(
    req: &S3Request<T>,
    content_md5: Option<&str>,
    checksum_algorithm: Option<&ChecksumAlgorithm>,
    require_integrity: bool,
) -> S3Result<()> {
    let header_md5 = req
        .headers
        .get(http::HeaderName::from_static("content-md5"))
        .map(|value| value.to_str().map_err(|_| s3_error!(InvalidDigest)))
        .transpose()?;
    let expected_md5 = content_md5.or(header_md5);
    let has_checksum_header = [
        "x-amz-checksum-crc32",
        "x-amz-checksum-crc32c",
        "x-amz-checksum-crc64nvme",
        "x-amz-checksum-sha1",
        "x-amz-checksum-sha256",
    ]
    .iter()
    .any(|name| req.headers.contains_key(*name));
    if require_integrity
        && expected_md5.is_none()
        && checksum_algorithm.is_none()
        && !has_checksum_header
    {
        return Err(s3_error!(InvalidDigest));
    }
    if expected_md5.is_none() && checksum_algorithm.is_none() && !has_checksum_header {
        return Ok(());
    }
    let digests = req
        .extensions
        .get::<crate::server::RequestBodyDigest>()
        .and_then(crate::server::RequestBodyDigest::finish)
        .ok_or_else(|| s3_error!(InvalidRequest, "request body checksum unavailable"))?;
    verify_content_md5(&digests.md5, expected_md5)?;
    let mut checksums = RequestChecksums {
        algorithm: checksum_algorithm.cloned(),
        ..Default::default()
    };
    checksums.merge_headers(&req.headers)?;
    checksums.verify(&digests)
}

fn reject_put_extensions(input: &PutObjectInput) -> S3Result<()> {
    if input.acl.is_some()
        || input.bucket_key_enabled.is_some()
        || input.expected_bucket_owner.is_some()
        || input.grant_full_control.is_some()
        || input.grant_read.is_some()
        || input.grant_read_acp.is_some()
        || input.grant_write_acp.is_some()
        || input.object_lock_legal_hold_status.is_some()
        || input.object_lock_mode.is_some()
        || input.object_lock_retain_until_date.is_some()
        || input.request_payer.is_some()
        || input.server_side_encryption.is_some()
        || input.sse_customer_algorithm.is_some()
        || input.sse_customer_key.is_some()
        || input.sse_customer_key_md5.is_some()
        || input.ssekms_encryption_context.is_some()
        || input.ssekms_key_id.is_some()
        || !standard_storage_class(input.storage_class.as_ref())
        || input.website_redirect_location.is_some()
        || input.write_offset_bytes.is_some()
    {
        return Err(s3_error!(NotImplemented));
    }
    Ok(())
}

fn virtual_marker_put_condition(input: &PutObjectInput) -> S3Result<mutation::PutCondition> {
    if input.if_match.is_some() {
        return Err(s3_error!(PreconditionFailed));
    }
    match input.if_none_match {
        None | Some(ETagCondition::Any) => Ok(mutation::PutCondition::None),
        Some(ETagCondition::ETag(_)) => Err(s3_error!(InvalidRequest)),
    }
}

fn stored_content_encoding(value: Option<&str>, streaming: bool) -> Option<String> {
    if !streaming {
        return value.map(str::to_owned);
    }

    let encodings = value?
        .split(',')
        .map(str::trim)
        .filter(|encoding| !encoding.eq_ignore_ascii_case("aws-chunked"))
        .collect::<Vec<_>>();
    (!encodings.is_empty()).then(|| encodings.join(","))
}

#[derive(Clone, Default)]
struct RequestChecksums {
    algorithm: Option<ChecksumAlgorithm>,
    crc32: Option<String>,
    crc32c: Option<String>,
    crc64nvme: Option<String>,
    sha1: Option<String>,
    sha256: Option<String>,
}

impl From<&PutObjectInput> for RequestChecksums {
    fn from(input: &PutObjectInput) -> Self {
        Self {
            algorithm: input.checksum_algorithm.clone(),
            crc32: input.checksum_crc32.clone(),
            crc32c: input.checksum_crc32c.clone(),
            crc64nvme: input.checksum_crc64nvme.clone(),
            sha1: input.checksum_sha1.clone(),
            sha256: input.checksum_sha256.clone(),
        }
    }
}

impl RequestChecksums {
    fn from_upload_part(input: &UploadPartInput) -> Self {
        Self {
            algorithm: input.checksum_algorithm.clone(),
            crc32: input.checksum_crc32.clone(),
            crc32c: input.checksum_crc32c.clone(),
            crc64nvme: input.checksum_crc64nvme.clone(),
            sha1: input.checksum_sha1.clone(),
            sha256: input.checksum_sha256.clone(),
        }
    }

    fn from_complete(input: &CompleteMultipartUploadInput) -> Self {
        Self {
            algorithm: None,
            crc32: input.checksum_crc32.clone(),
            crc32c: input.checksum_crc32c.clone(),
            crc64nvme: input.checksum_crc64nvme.clone(),
            sha1: input.checksum_sha1.clone(),
            sha256: input.checksum_sha256.clone(),
        }
    }

    fn merge_trailers(&mut self, trailers: Option<&s3s::TrailingHeaders>) -> S3Result<()> {
        let Some(headers) = trailers.and_then(s3s::TrailingHeaders::take) else {
            return Ok(());
        };
        merge_checksum_header(&mut self.crc32, &headers, "x-amz-checksum-crc32")?;
        merge_checksum_header(&mut self.crc32c, &headers, "x-amz-checksum-crc32c")?;
        merge_checksum_header(&mut self.crc64nvme, &headers, "x-amz-checksum-crc64nvme")?;
        merge_checksum_header(&mut self.sha1, &headers, "x-amz-checksum-sha1")?;
        merge_checksum_header(&mut self.sha256, &headers, "x-amz-checksum-sha256")?;
        Ok(())
    }

    fn merge_headers(&mut self, headers: &http::HeaderMap) -> S3Result<()> {
        merge_checksum_header(&mut self.crc32, headers, "x-amz-checksum-crc32")?;
        merge_checksum_header(&mut self.crc32c, headers, "x-amz-checksum-crc32c")?;
        merge_checksum_header(&mut self.crc64nvme, headers, "x-amz-checksum-crc64nvme")?;
        merge_checksum_header(&mut self.sha1, headers, "x-amz-checksum-sha1")?;
        merge_checksum_header(&mut self.sha256, headers, "x-amz-checksum-sha256")?;
        Ok(())
    }

    fn verify(&self, digests: &crate::content::Digests) -> S3Result<()> {
        self.ensure_single_value()?;
        self.verify_values(digests)?;
        if let Some(algorithm) = &self.algorithm {
            let supplied = match algorithm.as_str() {
                ChecksumAlgorithm::CRC32 => self.crc32.is_some(),
                ChecksumAlgorithm::CRC32C => self.crc32c.is_some(),
                ChecksumAlgorithm::CRC64NVME => self.crc64nvme.is_some(),
                ChecksumAlgorithm::SHA1 => self.sha1.is_some(),
                ChecksumAlgorithm::SHA256 => self.sha256.is_some(),
                _ => return Err(s3_error!(InvalidRequest, "Unsupported checksum algorithm")),
            };
            if !supplied {
                return Err(s3_error!(InvalidRequest, "Checksum value is missing"));
            }
        }
        Ok(())
    }

    fn verify_values(&self, digests: &crate::content::Digests) -> S3Result<()> {
        verify_base64_checksum(self.crc32.as_deref(), &digests.crc32.to_be_bytes())?;
        verify_base64_checksum(self.crc32c.as_deref(), &digests.crc32c.to_be_bytes())?;
        verify_base64_checksum(self.crc64nvme.as_deref(), &digests.crc64nvme.to_be_bytes())?;
        verify_base64_checksum(self.sha1.as_deref(), &digests.sha1)?;
        verify_base64_checksum(self.sha256.as_deref(), &digests.sha256)?;
        Ok(())
    }

    fn stored(&self, digests: &crate::content::Digests) -> crate::attributes::Checksums {
        let mut stored = crate::attributes::Checksums {
            crc32: self.crc32.clone(),
            crc32c: self.crc32c.clone(),
            crc64nvme: self.crc64nvme.clone(),
            sha1: self.sha1.clone(),
            sha256: self.sha256.clone(),
            checksum_type: Some(ChecksumType::FULL_OBJECT.to_owned()),
        };
        if stored.is_empty() {
            stored = default_checksums(digests);
        }
        stored
    }

    fn stored_for_algorithm(
        &self,
        digests: &crate::content::Digests,
        algorithm: Option<&str>,
    ) -> S3Result<crate::attributes::Checksums> {
        let stored = self.stored(digests);
        if !self.values_empty() {
            return Ok(stored);
        }
        algorithm
            .map(|algorithm| calculated_checksums(digests, algorithm))
            .transpose()
            .map(|value| value.unwrap_or(stored))
    }

    fn values_empty(&self) -> bool {
        self.crc32.is_none()
            && self.crc32c.is_none()
            && self.crc64nvme.is_none()
            && self.sha1.is_none()
            && self.sha256.is_none()
    }

    fn ensure_single_value(&self) -> S3Result<()> {
        let count = [
            self.crc32.is_some(),
            self.crc32c.is_some(),
            self.crc64nvme.is_some(),
            self.sha1.is_some(),
            self.sha256.is_some(),
        ]
        .into_iter()
        .filter(|present| *present)
        .count();
        if count > 1 {
            return Err(s3_error!(
                InvalidRequest,
                "Only one object checksum algorithm can be supplied"
            ));
        }
        Ok(())
    }

    fn has_algorithm(&self, algorithm: &str) -> bool {
        match algorithm {
            ChecksumAlgorithm::CRC32 => self.crc32.is_some(),
            ChecksumAlgorithm::CRC32C => self.crc32c.is_some(),
            ChecksumAlgorithm::CRC64NVME => self.crc64nvme.is_some(),
            ChecksumAlgorithm::SHA1 => self.sha1.is_some(),
            ChecksumAlgorithm::SHA256 => self.sha256.is_some(),
            _ => false,
        }
    }
}

fn multipart_checksum_profile(
    algorithm: Option<&ChecksumAlgorithm>,
    checksum_type: Option<&ChecksumType>,
) -> S3Result<(Option<String>, Option<String>)> {
    let Some(algorithm) = algorithm else {
        if checksum_type.is_some() {
            return Err(s3_error!(InvalidRequest));
        }
        return Ok((None, None));
    };
    let checksum_type = checksum_type.map(ChecksumType::as_str).unwrap_or_else(|| {
        if algorithm.as_str() == ChecksumAlgorithm::CRC64NVME {
            ChecksumType::FULL_OBJECT
        } else {
            ChecksumType::COMPOSITE
        }
    });
    let supported = matches!(
        (algorithm.as_str(), checksum_type),
        (
            ChecksumAlgorithm::CRC32 | ChecksumAlgorithm::CRC32C | ChecksumAlgorithm::CRC64NVME,
            ChecksumType::FULL_OBJECT
        ) | (
            ChecksumAlgorithm::CRC32
                | ChecksumAlgorithm::CRC32C
                | ChecksumAlgorithm::SHA1
                | ChecksumAlgorithm::SHA256,
            ChecksumType::COMPOSITE
        )
    );
    if !supported {
        return Err(s3_error!(
            InvalidRequest,
            "Invalid multipart checksum profile"
        ));
    }
    Ok((
        Some(algorithm.as_str().to_owned()),
        Some(checksum_type.to_owned()),
    ))
}

fn default_checksums(digests: &crate::content::Digests) -> crate::attributes::Checksums {
    crate::attributes::Checksums {
        crc64nvme: Some(
            base64::engine::general_purpose::STANDARD.encode(digests.crc64nvme.to_be_bytes()),
        ),
        checksum_type: Some(ChecksumType::FULL_OBJECT.to_owned()),
        ..Default::default()
    }
}

fn calculated_checksums(
    digests: &crate::content::Digests,
    algorithm: &str,
) -> S3Result<crate::attributes::Checksums> {
    let encode = |value: &[u8]| base64::engine::general_purpose::STANDARD.encode(value);
    let mut checksums = crate::attributes::Checksums {
        checksum_type: Some(ChecksumType::FULL_OBJECT.to_owned()),
        ..Default::default()
    };
    match algorithm {
        ChecksumAlgorithm::CRC32 => checksums.crc32 = Some(encode(&digests.crc32.to_be_bytes())),
        ChecksumAlgorithm::CRC32C => {
            checksums.crc32c = Some(encode(&digests.crc32c.to_be_bytes()));
        }
        ChecksumAlgorithm::CRC64NVME => {
            checksums.crc64nvme = Some(encode(&digests.crc64nvme.to_be_bytes()));
        }
        ChecksumAlgorithm::SHA1 => checksums.sha1 = Some(encode(&digests.sha1)),
        ChecksumAlgorithm::SHA256 => checksums.sha256 = Some(encode(&digests.sha256)),
        _ => return Err(s3_error!(InvalidRequest, "Unsupported checksum algorithm")),
    }
    Ok(checksums)
}

fn composite_checksums<'a>(
    parts: impl IntoIterator<Item = &'a crate::attributes::Checksums>,
    algorithm: &str,
) -> S3Result<crate::attributes::Checksums> {
    let mut concatenated = Vec::new();
    let mut part_count = 0_usize;
    for checksums in parts {
        let encoded = checksum_value(checksums, algorithm).ok_or_else(|| s3_error!(InvalidPart))?;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| s3_error!(InvalidPart))?;
        concatenated.extend_from_slice(&decoded);
        part_count = part_count
            .checked_add(1)
            .ok_or_else(|| s3_error!(InternalError))?;
    }
    let encode = |value: &[u8]| {
        format!(
            "{}-{part_count}",
            base64::engine::general_purpose::STANDARD.encode(value)
        )
    };
    let mut checksums = crate::attributes::Checksums {
        checksum_type: Some(ChecksumType::COMPOSITE.to_owned()),
        ..Default::default()
    };
    match algorithm {
        ChecksumAlgorithm::CRC32 => {
            let value = u32::try_from(crc_fast::checksum(
                crc_fast::CrcAlgorithm::Crc32IsoHdlc,
                &concatenated,
            ))
            .map_err(|_| s3_error!(InternalError))?;
            checksums.crc32 = Some(encode(&value.to_be_bytes()));
        }
        ChecksumAlgorithm::CRC32C => {
            let value = u32::try_from(crc_fast::checksum(
                crc_fast::CrcAlgorithm::Crc32Iscsi,
                &concatenated,
            ))
            .map_err(|_| s3_error!(InternalError))?;
            checksums.crc32c = Some(encode(&value.to_be_bytes()));
        }
        ChecksumAlgorithm::SHA1 => {
            use sha1::Digest as _;
            checksums.sha1 = Some(encode(&sha1::Sha1::digest(&concatenated)));
        }
        ChecksumAlgorithm::SHA256 => {
            use sha2::Digest as _;
            checksums.sha256 = Some(encode(&sha2::Sha256::digest(&concatenated)));
        }
        _ => {
            return Err(s3_error!(
                InvalidRequest,
                "Invalid composite checksum algorithm"
            ));
        }
    }
    Ok(checksums)
}

fn checksum_value<'a>(
    checksums: &'a crate::attributes::Checksums,
    algorithm: &str,
) -> Option<&'a str> {
    match algorithm {
        ChecksumAlgorithm::CRC32 => checksums.crc32.as_deref(),
        ChecksumAlgorithm::CRC32C => checksums.crc32c.as_deref(),
        ChecksumAlgorithm::CRC64NVME => checksums.crc64nvme.as_deref(),
        ChecksumAlgorithm::SHA1 => checksums.sha1.as_deref(),
        ChecksumAlgorithm::SHA256 => checksums.sha256.as_deref(),
        _ => None,
    }
}

fn request_checksums_match(
    requested: &RequestChecksums,
    calculated: &crate::attributes::Checksums,
) -> bool {
    [
        (requested.crc32.as_ref(), calculated.crc32.as_ref()),
        (requested.crc32c.as_ref(), calculated.crc32c.as_ref()),
        (requested.crc64nvme.as_ref(), calculated.crc64nvme.as_ref()),
        (requested.sha1.as_ref(), calculated.sha1.as_ref()),
        (requested.sha256.as_ref(), calculated.sha256.as_ref()),
    ]
    .into_iter()
    .all(|(requested, actual)| {
        requested.is_none_or(|requested| {
            actual.is_some_and(|actual| {
                actual == requested
                    || actual.rsplit_once('-').is_some_and(|(digest, count)| {
                        digest == requested && count.parse::<u32>().is_ok()
                    })
            })
        })
    })
}

fn completed_part_checksums_match(
    part: &CompletedPart,
    stored: &crate::attributes::Checksums,
) -> S3Result<()> {
    let matches = [
        (part.checksum_crc32.as_ref(), stored.crc32.as_ref()),
        (part.checksum_crc32c.as_ref(), stored.crc32c.as_ref()),
        (part.checksum_crc64nvme.as_ref(), stored.crc64nvme.as_ref()),
        (part.checksum_sha1.as_ref(), stored.sha1.as_ref()),
        (part.checksum_sha256.as_ref(), stored.sha256.as_ref()),
    ]
    .into_iter()
    .all(|(requested, actual)| requested.is_none_or(|requested| actual == Some(requested)));
    if matches {
        Ok(())
    } else {
        Err(s3_error!(InvalidPart))
    }
}

fn completed_part_checksum_value<'a>(part: &'a CompletedPart, algorithm: &str) -> Option<&'a str> {
    match algorithm {
        ChecksumAlgorithm::CRC32 => part.checksum_crc32.as_deref(),
        ChecksumAlgorithm::CRC32C => part.checksum_crc32c.as_deref(),
        ChecksumAlgorithm::CRC64NVME => part.checksum_crc64nvme.as_deref(),
        ChecksumAlgorithm::SHA1 => part.checksum_sha1.as_deref(),
        ChecksumAlgorithm::SHA256 => part.checksum_sha256.as_deref(),
        _ => None,
    }
}

fn response_checksums(
    mode: Option<&ChecksumMode>,
    checksums: Option<&crate::attributes::Checksums>,
) -> S3Result<crate::attributes::Checksums> {
    match mode {
        None => Ok(crate::attributes::Checksums::default()),
        // AWS SDKs emit different casing for this modeled header value.
        Some(value) if value.as_str().eq_ignore_ascii_case(ChecksumMode::ENABLED) => {
            Ok(checksums.cloned().unwrap_or_default())
        }
        Some(_) => Err(s3_error!(InvalidArgument)),
    }
}

fn read_selection(
    size: u64,
    attributes: Option<&crate::attributes::ObjectAttributes>,
    requested_range: Option<&Range>,
    part_number: Option<i32>,
) -> S3Result<ReadSelection> {
    if requested_range.is_some() && part_number.is_some() {
        return Err(s3_error!(InvalidRequest));
    }
    let object_checksums = attributes.map(|value| value.checksums.clone());
    let parts = attributes
        .map(|value| value.parts.as_slice())
        .unwrap_or_default();
    if let Some(part_number) = part_number {
        if !(1..=10_000).contains(&part_number) {
            return Err(s3_error!(InvalidArgument));
        }
        if parts.is_empty() {
            if part_number != 1 {
                return Err(s3_error!(InvalidRange));
            }
            return Ok(ReadSelection {
                range: 0..size,
                content_range: None,
                parts_count: None,
                checksums: object_checksums,
            });
        }
        let mut offset = 0_u64;
        let mut selected = None;
        for part in parts {
            let end = offset
                .checked_add(part.size)
                .ok_or_else(|| s3_error!(InternalError))?;
            if part.number == part_number {
                selected = Some((offset..end, part.checksums.clone()));
            }
            offset = end;
        }
        if offset != size {
            return Err(s3_error!(InternalError));
        }
        let (range, checksums) = selected.ok_or_else(|| s3_error!(InvalidRange))?;
        return Ok(ReadSelection {
            content_range: content_range(&range, size),
            range,
            parts_count: Some(i32::try_from(parts.len()).map_err(|_| s3_error!(InternalError))?),
            checksums: Some(checksums),
        });
    }
    let Some(requested_range) = requested_range else {
        return Ok(ReadSelection {
            range: 0..size,
            content_range: None,
            parts_count: None,
            checksums: object_checksums,
        });
    };
    let checked = requested_range.check(size)?;
    let range = checked.start..checked.end;
    let checksums = if range.start == 0 && range.end == size {
        object_checksums
    } else {
        part_checksums_for_range(parts, &range)
    };
    Ok(ReadSelection {
        content_range: content_range(&range, size),
        range,
        parts_count: None,
        checksums,
    })
}

fn part_checksums_for_range(
    parts: &[crate::attributes::PartAttributes],
    requested: &std::ops::Range<u64>,
) -> Option<crate::attributes::Checksums> {
    let mut offset = 0_u64;
    for part in parts {
        let end = offset.checked_add(part.size)?;
        if requested.start == offset && requested.end == end {
            return Some(part.checksums.clone());
        }
        offset = end;
    }
    None
}

fn content_range(range: &std::ops::Range<u64>, size: u64) -> Option<String> {
    (range.start < range.end).then(|| format!("bytes {}-{}/{}", range.start, range.end - 1, size))
}

fn tag_count(attributes: Option<&crate::attributes::ObjectAttributes>) -> S3Result<Option<i32>> {
    attributes
        .filter(|value| !value.tags.is_empty())
        .map(|value| i32::try_from(value.tags.len()).map_err(|_| s3_error!(InternalError)))
        .transpose()
}

fn checksum_dto(checksums: &crate::attributes::Checksums) -> Checksum {
    Checksum {
        checksum_crc32: checksums.crc32.clone(),
        checksum_crc32c: checksums.crc32c.clone(),
        checksum_crc64nvme: checksums.crc64nvme.clone(),
        checksum_sha1: checksums.sha1.clone(),
        checksum_sha256: checksums.sha256.clone(),
        checksum_type: checksums.checksum_type.clone().map(ChecksumType::from),
    }
}

fn object_attribute_names(values: &[ObjectAttributes]) -> std::collections::BTreeSet<&str> {
    // s3s 0.14 yields one DTO item per header line, while AWS SDKs encode this
    // Smithy list as one comma-delimited header value.
    values
        .iter()
        .flat_map(|value| value.as_str().split(','))
        .map(str::trim)
        .collect()
}

fn object_parts(
    parts: &[crate::attributes::PartAttributes],
    marker: Option<i32>,
    max: Option<i32>,
) -> S3Result<GetObjectAttributesParts> {
    let (marker, max) = multipart_page_bounds(marker, max)?;
    let max_usize = usize::try_from(max).map_err(|_| s3_error!(InvalidArgument))?;
    let mut selected = parts
        .iter()
        .filter(|part| part.number > marker)
        .collect::<Vec<_>>();
    let truncated = selected.len() > max_usize;
    selected.truncate(max_usize);
    let next = truncated
        .then(|| selected.last().map(|part| part.number))
        .flatten();
    Ok(GetObjectAttributesParts {
        is_truncated: Some(truncated),
        max_parts: Some(max),
        next_part_number_marker: next,
        part_number_marker: Some(marker),
        parts: Some(
            selected
                .into_iter()
                .map(|part| {
                    Ok(ObjectPart {
                        checksum_crc32: part.checksums.crc32.clone(),
                        checksum_crc32c: part.checksums.crc32c.clone(),
                        checksum_crc64nvme: part.checksums.crc64nvme.clone(),
                        checksum_sha1: part.checksums.sha1.clone(),
                        checksum_sha256: part.checksums.sha256.clone(),
                        part_number: Some(part.number),
                        size: Some(i64::try_from(part.size).map_err(|_| s3_error!(InternalError))?),
                    })
                })
                .collect::<S3Result<Vec<_>>>()?,
        ),
        total_parts_count: Some(i32::try_from(parts.len()).map_err(|_| s3_error!(InternalError))?),
    })
}

fn multipart_page_bounds(marker: Option<i32>, max: Option<i32>) -> S3Result<(i32, i32)> {
    let marker = marker.unwrap_or(0);
    let max = max.unwrap_or(1000);
    if marker < 0 || !(1..=1000).contains(&max) {
        return Err(s3_error!(InvalidArgument));
    }
    Ok((marker, max))
}

fn parse_tagging_header(value: Option<&str>) -> S3Result<BTreeMap<String, String>> {
    let tags = value
        .map(|value| url::form_urlencoded::parse(value.as_bytes()).into_owned())
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    validate_tags(tags)
}

fn validate_tags(
    tags: impl IntoIterator<Item = (String, String)>,
) -> S3Result<BTreeMap<String, String>> {
    let mut validated = BTreeMap::new();
    for (key, value) in tags {
        if key.is_empty()
            || key.chars().count() > 128
            || value.chars().count() > 256
            || key.to_ascii_lowercase().starts_with("aws:")
            || !key.chars().all(valid_tag_character)
            || !value.chars().all(valid_tag_character)
            || validated.insert(key, value).is_some()
            || validated.len() > 10
        {
            return Err(s3_error!(InvalidTag));
        }
    }
    Ok(validated)
}

fn valid_tag_character(value: char) -> bool {
    value.is_alphanumeric()
        || (value.is_whitespace() && !value.is_control())
        || matches!(value, '+' | '-' | '=' | '.' | '_' | ':' | '/' | '@')
}

fn merge_checksum_header(
    current: &mut Option<String>,
    headers: &http::HeaderMap,
    name: &'static str,
) -> S3Result<()> {
    let Some(value) = headers.get(name) else {
        return Ok(());
    };
    let value = value
        .to_str()
        .map_err(|_| s3_error!(InvalidDigest))?
        .to_owned();
    if current.as_ref().is_some_and(|current| current != &value) {
        return Err(s3_error!(BadDigest));
    }
    *current = Some(value);
    Ok(())
}

fn verify_base64_checksum(expected: Option<&str>, actual: &[u8]) -> S3Result<()> {
    let Some(expected) = expected else {
        return Ok(());
    };
    let expected = base64::engine::general_purpose::STANDARD
        .decode(expected)
        .map_err(|_| s3_error!(InvalidDigest))?;
    if expected.as_slice() != actual {
        return Err(s3_error!(BadDigest));
    }
    Ok(())
}

fn reject_delete_extensions(input: &DeleteObjectInput) -> S3Result<()> {
    if input.bypass_governance_retention.is_some()
        || input.expected_bucket_owner.is_some()
        || input.if_match_last_modified_time.is_some()
        || input.if_match_size.is_some()
        || input.mfa.is_some()
        || input.request_payer.is_some()
        || input.version_id.is_some()
    {
        return Err(s3_error!(NotImplemented));
    }
    Ok(())
}

fn reject_copy_extensions(input: &CopyObjectInput) -> S3Result<()> {
    let directive_supported = input.metadata_directive.as_ref().is_none_or(|value| {
        matches!(
            value.as_str(),
            MetadataDirective::COPY | MetadataDirective::REPLACE
        )
    });
    if !directive_supported
        || input.acl.is_some()
        || input.bucket_key_enabled.is_some()
        || input.copy_source_sse_customer_algorithm.is_some()
        || input.copy_source_sse_customer_key.is_some()
        || input.copy_source_sse_customer_key_md5.is_some()
        || input.expected_bucket_owner.is_some()
        || input.expected_source_bucket_owner.is_some()
        || input.grant_full_control.is_some()
        || input.grant_read.is_some()
        || input.grant_read_acp.is_some()
        || input.grant_write_acp.is_some()
        || input.object_lock_legal_hold_status.is_some()
        || input.object_lock_mode.is_some()
        || input.object_lock_retain_until_date.is_some()
        || input.request_payer.is_some()
        || input.server_side_encryption.is_some()
        || input.sse_customer_algorithm.is_some()
        || input.sse_customer_key.is_some()
        || input.sse_customer_key_md5.is_some()
        || input.ssekms_encryption_context.is_some()
        || input.ssekms_key_id.is_some()
        || !standard_storage_class(input.storage_class.as_ref())
        || input.website_redirect_location.is_some()
    {
        return Err(s3_error!(NotImplemented));
    }
    Ok(())
}

fn reject_list_extensions(input: &ListObjectsV2Input) -> S3Result<()> {
    if input.delimiter.as_deref().is_some_and(|value| value != "/")
        || input.expected_bucket_owner.is_some()
        || input.fetch_owner.is_some()
        || !supported_list_attributes(input.optional_object_attributes.as_ref())
        || input.request_payer.is_some()
    {
        return Err(s3_error!(NotImplemented));
    }
    Ok(())
}

fn reject_create_multipart_extensions(input: &CreateMultipartUploadInput) -> S3Result<()> {
    if input.acl.is_some()
        || input.bucket_key_enabled.is_some()
        || input.expected_bucket_owner.is_some()
        || input.grant_full_control.is_some()
        || input.grant_read.is_some()
        || input.grant_read_acp.is_some()
        || input.grant_write_acp.is_some()
        || input.object_lock_legal_hold_status.is_some()
        || input.object_lock_mode.is_some()
        || input.object_lock_retain_until_date.is_some()
        || input.request_payer.is_some()
        || input.server_side_encryption.is_some()
        || input.sse_customer_algorithm.is_some()
        || input.sse_customer_key.is_some()
        || input.sse_customer_key_md5.is_some()
        || input.ssekms_encryption_context.is_some()
        || input.ssekms_key_id.is_some()
        || !standard_storage_class(input.storage_class.as_ref())
        || input.website_redirect_location.is_some()
    {
        return Err(s3_error!(NotImplemented));
    }
    Ok(())
}

fn standard_storage_class(storage_class: Option<&StorageClass>) -> bool {
    storage_class.is_none_or(|value| value.as_str() == StorageClass::STANDARD)
}

fn supported_list_attributes(values: Option<&OptionalObjectAttributesList>) -> bool {
    values.is_none_or(|values| {
        values
            .iter()
            .all(|value| value.as_str() == OptionalObjectAttributes::RESTORE_STATUS)
    })
}

fn reject_upload_part_extensions(input: &UploadPartInput) -> S3Result<()> {
    if input.expected_bucket_owner.is_some()
        || input.request_payer.is_some()
        || input.sse_customer_algorithm.is_some()
        || input.sse_customer_key.is_some()
        || input.sse_customer_key_md5.is_some()
    {
        return Err(s3_error!(NotImplemented));
    }
    Ok(())
}

fn reject_complete_multipart_extensions(input: &CompleteMultipartUploadInput) -> S3Result<()> {
    if input.expected_bucket_owner.is_some()
        || input.request_payer.is_some()
        || input.sse_customer_algorithm.is_some()
        || input.sse_customer_key.is_some()
        || input.sse_customer_key_md5.is_some()
    {
        return Err(s3_error!(NotImplemented));
    }
    Ok(())
}

fn writable_branch<'a>(
    repository: &Repository,
    address: &'a namespace::ObjectAddress,
) -> S3Result<&'a str> {
    let branch = address
        .branch
        .as_deref()
        .ok_or_else(|| s3_error!(MethodNotAllowed, "Writes require a branch key"))?;
    let short = branch.strip_prefix("refs/heads/").unwrap_or(branch);
    if repository
        .config
        .protected_branches
        .iter()
        .any(|protected| protected == short)
    {
        return Err(s3_error!(
            AccessDenied,
            "The destination branch is protected"
        ));
    }
    Ok(branch)
}

fn copy_source_bucket_key(source: &CopySource) -> S3Result<(String, String)> {
    match source {
        CopySource::Bucket {
            bucket,
            key,
            version_id: None,
        } => Ok((bucket.to_string(), key.to_string())),
        CopySource::Bucket {
            version_id: Some(_),
            ..
        } => Err(s3_error!(
            NotImplemented,
            "Copying an object version is unsupported"
        )),
        CopySource::AccessPoint { .. } | CopySource::Outpost { .. } => Err(s3_error!(
            NotImplemented,
            "Copy access points are unsupported"
        )),
    }
}

fn stored_to_pending(
    value: &crate::attributes::ObjectAttributes,
) -> crate::attributes::PutAttributes {
    crate::attributes::PutAttributes {
        etag_override: None,
        completion_upload_id: value.completion_upload_id.clone(),
        logical_size: Some(value.size),
        checksums: value.checksums.clone(),
        tags: value.tags.clone(),
        parts: value.parts.clone(),
        cache_control: value.cache_control.clone(),
        content_disposition: value.content_disposition.clone(),
        content_encoding: value.content_encoding.clone(),
        content_language: value.content_language.clone(),
        content_type: value.content_type.clone(),
        expires: value.expires.clone(),
        metadata: value.metadata.clone(),
    }
}

impl Gateway {
    async fn list_refs(
        &self,
        req: &S3Request<ListObjectsV2Input>,
        repository: &Repository,
    ) -> S3Result<S3Response<ListObjectsV2Output>> {
        let url_encode = list_url_encoding(req.input.encoding_type.as_ref())?;
        let repo = self.open(repository).await?;
        let after = req
            .input
            .continuation_token
            .as_deref()
            .or(req.input.start_after.as_deref());
        let max_keys = req.input.max_keys.unwrap_or(1000);
        if max_keys < 0 {
            return Err(s3_error!(InvalidArgument));
        }
        let max_keys = max_keys.min(1000);
        let limit = usize::try_from(max_keys).map_err(|_| s3_error!(InvalidArgument))?;
        let (keys, truncated) = select_ref_keys(&repo.remote().refs().entries, after, limit);
        let next = truncated.then(|| keys.last().cloned()).flatten();
        let count = i32::try_from(keys.len()).map_err(|_| s3_error!(InternalError))?;
        Ok(S3Response::new(ListObjectsV2Output {
            name: Some(req.input.bucket.clone()),
            prefix: encode_list_option(req.input.prefix.clone(), url_encode),
            max_keys: Some(max_keys),
            key_count: Some(count),
            continuation_token: req.input.continuation_token.clone(),
            next_continuation_token: next,
            is_truncated: Some(truncated),
            common_prefixes: Some(
                keys.into_iter()
                    .map(|prefix| CommonPrefix {
                        prefix: Some(encode_list_value(&prefix, url_encode)),
                    })
                    .collect(),
            ),
            delimiter: encode_list_option(req.input.delimiter.clone(), url_encode),
            encoding_type: req.input.encoding_type.clone(),
            start_after: encode_list_option(req.input.start_after.clone(), url_encode),
            ..Default::default()
        }))
    }
}

fn reject_get_extensions(input: &GetObjectInput) -> S3Result<()> {
    if input.expected_bucket_owner.is_some()
        || input.version_id.is_some()
        || input.request_payer.is_some()
        || input.sse_customer_algorithm.is_some()
        || input.sse_customer_key.is_some()
        || input.sse_customer_key_md5.is_some()
    {
        return Err(s3_error!(NotImplemented));
    }
    Ok(())
}

fn reject_head_extensions(input: &HeadObjectInput) -> S3Result<()> {
    if input.expected_bucket_owner.is_some()
        || input.version_id.is_some()
        || input.request_payer.is_some()
        || input.sse_customer_algorithm.is_some()
        || input.sse_customer_key.is_some()
        || input.sse_customer_key_md5.is_some()
    {
        return Err(s3_error!(NotImplemented));
    }
    Ok(())
}

fn evaluate_conditions(
    if_match: Option<&ETagCondition>,
    if_none_match: Option<&ETagCondition>,
    if_modified_since: Option<&Timestamp>,
    if_unmodified_since: Option<&Timestamp>,
    etag: &str,
    modified: &Timestamp,
) -> S3Result<()> {
    let actual = ETag::Strong(etag.to_owned());
    if let Some(condition) = if_match {
        let matches = match condition {
            ETagCondition::Any => true,
            ETagCondition::ETag(expected) => actual.strong_cmp(expected),
        };
        if !matches {
            return Err(s3_error!(PreconditionFailed));
        }
    } else if if_unmodified_since.is_some_and(|expected| modified > expected) {
        return Err(s3_error!(PreconditionFailed));
    }
    if let Some(condition) = if_none_match {
        let matches = match condition {
            ETagCondition::Any => true,
            ETagCondition::ETag(expected) => actual.weak_cmp(expected),
        };
        if matches {
            return Err(s3_error!(NotModified));
        }
    } else if if_modified_since.is_some_and(|expected| modified <= expected) {
        return Err(s3_error!(NotModified));
    }
    Ok(())
}

async fn finish<T>(
    operation: crab_remote_git::OperationContext,
    result: S3Result<T>,
) -> S3Result<T> {
    match operation.finish(Ok(())).await {
        Ok(()) => result,
        Err(error) => Err(remote_error(error)),
    }
}

fn timestamp(seconds: i64) -> S3Result<Timestamp> {
    let seconds = u64::try_from(seconds).map_err(|_| s3_error!(InternalError))?;
    UNIX_EPOCH
        .checked_add(Duration::from_secs(seconds))
        .map(Timestamp::from)
        .ok_or_else(|| s3_error!(InternalError))
}

fn list_url_encoding(encoding: Option<&EncodingType>) -> S3Result<bool> {
    match encoding {
        None => Ok(false),
        Some(value) if value.as_str() == EncodingType::URL => Ok(true),
        Some(_) => Err(s3_error!(InvalidArgument, "Unsupported list encoding type")),
    }
}

fn encode_list_option(value: Option<String>, enabled: bool) -> Option<String> {
    value.map(|value| encode_list_value(&value, enabled))
}

fn encode_list_value(value: &str, enabled: bool) -> String {
    if enabled {
        percent_encoding::utf8_percent_encode(value, percent_encoding::NON_ALPHANUMERIC).to_string()
    } else {
        value.to_owned()
    }
}

fn namespace_error(error: namespace::NamespaceError) -> s3s::S3Error {
    match error {
        namespace::NamespaceError::MissingObject => s3_error!(NoSuchKey),
        _ => s3_error!(InvalidArgument),
    }
}

fn select_ref_keys(
    entries: &[crab_remote_git::RepositoryRef],
    after: Option<&str>,
    limit: usize,
) -> (Vec<String>, bool) {
    use std::collections::BinaryHeap;

    if limit == 0 {
        return (Vec::new(), false);
    }
    let retain = limit.saturating_add(1);
    let mut selected = BinaryHeap::with_capacity(retain);
    for key in entries
        .iter()
        .filter_map(|reference| reference.name.strip_prefix("refs/heads/"))
        .map(|branch| {
            format!(
                "{}/",
                percent_encoding::utf8_percent_encode(branch, percent_encoding::NON_ALPHANUMERIC)
            )
        })
        .filter(|key| after.is_none_or(|marker| key.as_str() > marker))
    {
        if selected.len() < retain {
            selected.push(key);
        } else if selected.peek().is_some_and(|largest| key < *largest) {
            selected.pop();
            selected.push(key);
        }
    }
    let truncated = selected.len() > limit;
    let mut keys = selected.into_sorted_vec();
    keys.truncate(limit);
    (keys, truncated)
}

fn remote_error(error: crab_remote_git::Error) -> s3s::S3Error {
    match error {
        crab_remote_git::Error::PathNotFound => s3_error!(NoSuchKey),
        crab_remote_git::Error::EntryNotBlob { actual } => object_entry_error(actual),
        crab_remote_git::Error::Revision { .. } => s3_error!(NoSuchKey),
        crab_remote_git::Error::Cancelled => s3_error!(RequestTimeout),
        crab_remote_git::Error::LimitExceeded { .. } => slow_down_error(),
        error => {
            tracing::error!(error = ?error, "S3 repository read failed");
            s3_error!(InternalError)
        }
    }
}

fn object_entry_error(kind: EntryKind) -> s3s::S3Error {
    if kind == EntryKind::Tree {
        s3_error!(NoSuchKey)
    } else {
        s3_error!(InvalidObjectState)
    }
}

fn mutation_error(error: mutation::Error) -> s3s::S3Error {
    match error {
        mutation::Error::NotDirectory
        | mutation::Error::IsDirectory
        | mutation::Error::InvalidAttributes => {
            s3_error!(InvalidObjectState)
        }
        mutation::Error::Cancelled
        | mutation::Error::Publication(crab_remote::publication::Error::Cancelled) => {
            s3_error!(RequestTimeout)
        }
        mutation::Error::Overloaded | mutation::Error::AdmissionTimeout => slow_down_error(),
        mutation::Error::Capacity(error) => scratch_capacity_error(error),
        mutation::Error::PreconditionFailed => s3_error!(PreconditionFailed),
        mutation::Error::Write(crab_write::WriteError::RefChanged { .. }) => {
            s3_error!(
                OperationAborted,
                "A conflicting branch write won; retry the request"
            )
        }
        mutation::Error::Coordination(crab_coordination::CoordinationError::PushLockHeld {
            ..
        })
        | mutation::Error::Publication(crab_remote::publication::Error::Coordination(
            crab_coordination::CoordinationError::PushLockHeld { .. },
        ))
        | mutation::Error::Metadata(crab_metadata::error::MetadataError::PlanAlreadyAttempted {
            ..
        }) => s3_error!(
            OperationAborted,
            "The write outcome is being reconciled; retry the request"
        ),
        error => {
            tracing::error!(error = ?error, "S3 repository mutation failed");
            s3_error!(InternalError)
        }
    }
}

fn admission_error(error: crate::admission::Error) -> s3s::S3Error {
    match error {
        crate::admission::Error::Cancelled => s3_error!(RequestTimeout),
        crate::admission::Error::Overloaded | crate::admission::Error::AdmissionTimeout => {
            slow_down_error()
        }
        crate::admission::Error::AdmissionState => {
            tracing::error!(%error, "S3 request admission failed");
            s3_error!(InternalError)
        }
    }
}

fn gateway_error(error: crate::Error) -> s3s::S3Error {
    tracing::error!(error = ?error, "S3 gateway persistence failed");
    s3_error!(InternalError)
}

fn slow_down_error() -> s3s::S3Error {
    let mut error = s3_error!(SlowDown);
    error.set_headers(http::HeaderMap::from_iter([(
        http::header::RETRY_AFTER,
        http::HeaderValue::from_static("1"),
    )]));
    error
}

fn content_error(error: crate::content::Error) -> s3s::S3Error {
    match error {
        crate::content::Error::TooLarge => s3_error!(EntityTooLarge),
        crate::content::Error::Capacity(error) => scratch_capacity_error(error),
        crate::content::Error::Incomplete | crate::content::Error::Body(_) => {
            tracing::warn!(%error, "S3 request body failed");
            s3_error!(IncompleteBody)
        }
        crate::content::Error::Io(_) => {
            tracing::error!(error = ?error, "S3 content spool failed");
            s3_error!(InternalError)
        }
    }
}

fn scratch_capacity_error(error: crate::metrics::ScratchCapacityError) -> s3s::S3Error {
    tracing::warn!(%error, "S3 scratch capacity rejected work");
    slow_down_error()
}

pub(crate) fn md5_hex(bytes: &[u8]) -> String {
    use md5::Digest as _;

    md5::Md5::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn multipart_etag(parts: &[crate::multipart::Part]) -> S3Result<String> {
    use md5::Digest as _;

    let mut binary = Vec::with_capacity(parts.len().saturating_mul(16));
    for part in parts {
        if part.etag.len() != 32 {
            return Err(s3_error!(InvalidPart));
        }
        for index in (0..part.etag.len()).step_by(2) {
            binary.push(
                u8::from_str_radix(&part.etag[index..index + 2], 16)
                    .map_err(|_| s3_error!(InvalidPart))?,
            );
        }
    }
    let digest = md5::Md5::digest(binary);
    let hash: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!("{hash}-{}", parts.len()))
}

fn completed_multipart_response(
    session: &crate::multipart::Session,
    selected: &[(i32, String)],
    bucket: &str,
    key: &str,
) -> S3Result<Option<S3Response<CompleteMultipartUploadOutput>>> {
    let Some(etag) =
        crate::multipart::completed_etag(session, selected).map_err(multipart_error)?
    else {
        return Ok(None);
    };
    let checksums = session.completion_checksums.clone().unwrap_or_default();
    Ok(Some(S3Response::new(CompleteMultipartUploadOutput {
        bucket: Some(bucket.to_owned()),
        key: Some(key.to_owned()),
        e_tag: Some(ETag::Strong(etag.to_owned())),
        checksum_crc32: checksums.crc32,
        checksum_crc32c: checksums.crc32c,
        checksum_crc64nvme: checksums.crc64nvme,
        checksum_sha1: checksums.sha1,
        checksum_sha256: checksums.sha256,
        checksum_type: checksums.checksum_type.map(ChecksumType::from),
        ..Default::default()
    })))
}

fn completion_receipt(
    session: &crate::multipart::Session,
    attributes: &crate::attributes::ObjectAttributes,
) -> S3Result<Option<(String, crate::attributes::Checksums)>> {
    if attributes.completion_upload_id.as_deref() != Some(session.id.as_str()) {
        return Ok(None);
    }
    let parts = crate::multipart::frozen_parts(session)
        .map_err(multipart_error)?
        .ok_or_else(|| s3_error!(InternalError))?;
    let size = parts.iter().try_fold(0_u64, |total, part| {
        total
            .checked_add(part.size)
            .ok_or_else(|| s3_error!(InternalError))
    })?;
    let etag = multipart_etag(&parts)?;
    let mut expected = session.attributes.clone();
    expected.etag_override = Some(etag.clone());
    expected.completion_upload_id = Some(session.id.clone());
    expected.logical_size = Some(size);
    expected.checksums = attributes.checksums.clone();
    expected.parts = parts
        .iter()
        .map(|part| crate::attributes::PartAttributes {
            number: part.number,
            size: part.size,
            checksums: part.checksums.clone(),
        })
        .collect();
    if !attributes.matches_pending(&expected, &etag, size) {
        return Err(s3_error!(InternalError));
    }
    Ok(Some((etag, attributes.checksums.clone())))
}

fn now_seconds() -> S3Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .map_err(|_| s3_error!(InternalError))
}

fn multipart_error(error: crate::multipart::Error) -> s3s::S3Error {
    match error {
        crate::multipart::Error::NoSuchUpload
        | crate::multipart::Error::NotOpen
        | crate::multipart::Error::Identity => s3_error!(NoSuchUpload),
        crate::multipart::Error::PartNumber | crate::multipart::Error::InvalidPart => {
            s3_error!(InvalidPart)
        }
        crate::multipart::Error::InvalidPartOrder => s3_error!(InvalidPartOrder),
        crate::multipart::Error::EntityTooSmall => s3_error!(EntityTooSmall),
        crate::multipart::Error::EntityTooLarge => s3_error!(EntityTooLarge),
        crate::multipart::Error::Capacity => slow_down_error(),
        crate::multipart::Error::Cancelled => s3_error!(RequestTimeout),
        crate::multipart::Error::Conflict => s3_error!(OperationAborted),
        error => {
            tracing::error!(error = ?error, "S3 multipart state failed");
            s3_error!(InternalError)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list_request(input: ListObjectsV2Input) -> S3Request<ListObjectsV2Input> {
        S3Request {
            input,
            method: http::Method::GET,
            uri: http::Uri::from_static("/repository"),
            headers: http::HeaderMap::new(),
            extensions: http::Extensions::new(),
            credentials: Some(s3s::auth::Credentials {
                access_key: "listing-key".to_owned(),
                secret_key: "listing-secret".into(),
            }),
            region: None,
            service: None,
            trailing_headers: None,
        }
    }

    #[test]
    fn projected_pointer_etags_are_content_bound_without_hydration() {
        let file_hash = "01".repeat(32);
        let crab_pointer = ReadContent::CrabPointer(Bytes::from(format!(
            "version https://crab.build/spec/v1\nfile-hash {file_hash}\nsize 1\n"
        )));
        let lfs_pointer = ReadContent::LfsPointer(crab_git::LfsPointer {
            oid: [2; 32],
            size: 1,
            extensions: Vec::new(),
        });

        assert_eq!(
            (
                ReadContent::Ordinary(Bytes::from_static(b"a"))
                    .projected_etag()
                    .unwrap(),
                crab_pointer.projected_etag().unwrap(),
                lfs_pointer.projected_etag().unwrap(),
            ),
            (
                "0cc175b9c0f1b6a831c399e269772661".to_owned(),
                file_hash,
                "02".repeat(32),
            )
        );
    }

    #[tokio::test]
    async fn verified_crab_stream_withholds_terminal_chunk_until_hash_verifies() {
        use futures_util::StreamExt as _;

        let source: crab_read::ReconstructionStream = Box::pin(futures_util::stream::iter([
            Ok(Bytes::from_static(b"hello")),
            Ok(Bytes::from_static(b" world")),
        ]));
        let mut stream = verified_crab_stream(source, 11, *blake3::hash(b"hello world").as_bytes());

        assert_eq!(
            stream.next().await.unwrap().unwrap(),
            Bytes::from_static(b"hello")
        );
        assert_eq!(
            stream.next().await.unwrap().unwrap(),
            Bytes::from_static(b" world")
        );
        assert!(stream.next().await.is_none());
    }

    #[tokio::test]
    async fn verified_crab_stream_reports_hash_mismatch_before_releasing_last_chunk() {
        use futures_util::StreamExt as _;

        let source: crab_read::ReconstructionStream = Box::pin(futures_util::stream::iter([
            Ok(Bytes::from_static(b"hello")),
            Ok(Bytes::from_static(b" world")),
        ]));
        let mut stream = verified_crab_stream(source, 11, [0; 32]);

        assert_eq!(
            stream.next().await.unwrap().unwrap(),
            Bytes::from_static(b"hello")
        );
        assert!(matches!(
            stream.next().await,
            Some(Err(crab_read::ReadError::HashMismatch { .. }))
        ));
        assert!(stream.next().await.is_none());
    }

    #[test]
    fn unknown_or_unborn_ref_has_an_empty_list_response() {
        let req = list_request(ListObjectsV2Input {
            bucket: "repository".to_owned(),
            prefix: Some("main/".to_owned()),
            delimiter: Some("/".to_owned()),
            max_keys: Some(100),
            ..Default::default()
        });

        let output = empty_list_objects_response(&req, false).unwrap().output;

        assert_eq!(
            (output.key_count, output.is_truncated, output.contents),
            (Some(0), Some(false), None)
        );
    }

    #[test]
    fn empty_list_response_rejects_negative_max_keys() {
        let req = list_request(ListObjectsV2Input {
            bucket: "repository".to_owned(),
            max_keys: Some(-1),
            ..Default::default()
        });

        assert!(empty_list_objects_response(&req, false).is_err());
    }

    #[test]
    fn root_ref_listing_bounds_selection_without_changing_order() {
        let target =
            gix_hash::ObjectId::from_hex(b"1111111111111111111111111111111111111111").unwrap();
        let entries = [
            "refs/heads/z",
            "refs/tags/release",
            "refs/heads/a",
            "refs/heads/m",
        ]
        .into_iter()
        .map(|name| crab_remote_git::RepositoryRef {
            name: name.to_owned(),
            target,
            peeled: None,
        })
        .collect::<Vec<_>>();

        assert_eq!(
            select_ref_keys(&entries, None, 2),
            (vec!["a/".to_owned(), "m/".to_owned()], true)
        );
        assert_eq!(
            select_ref_keys(&entries, Some("m/"), 2),
            (vec!["z/".to_owned()], false)
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn object_listing_pages_merge_blobs_and_prefixes_in_complete_key_order() {
        let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
        let cache = tempfile::tempdir().unwrap();
        let local_cache = Arc::new(LocalCache::with_limits(
            cache.path().join("cache"),
            128 * 1024 * 1024,
            Some(128 * 1024 * 1024),
        ));
        let repository = Repository::new_with_cache(
            RepositoryConfig {
                name: "repo".to_owned(),
                provider: StorageProviderKind::Local,
                bucket: "memory".to_owned(),
                prefix: "listing-page-test".to_owned(),
                default_branch: "main".to_owned(),
                members: vec![crate::RepositoryMember {
                    principal: "user".to_owned(),
                    access: RepositoryAccess::Write,
                }],
                protected_branches: Vec::new(),
                max_active_multipart_uploads: 16,
                multipart_staging_bytes_per_upload: 50_000_000_000_000,
                multipart_upload_ttl_seconds: 604_800,
            },
            store,
            Arc::clone(&local_cache),
        )
        .unwrap();
        crab_write::initialize::initialize_repository(
            &repository.store,
            &repository.layout,
            "refs/heads/main",
        )
        .await
        .unwrap();
        let secret = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(secret.path(), "listing-secret-value").unwrap();
        let auth = GatewayAuth::load(&Config {
            listen: "127.0.0.1:0".parse().unwrap(),
            management_listen: "127.0.0.1:1".parse().unwrap(),
            endpoint_domain: None,
            region: "us-east-1".to_owned(),
            max_in_flight_requests: 8,
            cache: crate::LocalCacheConfig {
                directory: cache.path().join("auth-cache"),
                max_bytes: 128 * 1024 * 1024,
            },
            credentials: vec![crate::CredentialConfig {
                access_key: "listing-key".to_owned(),
                secret_key_file: secret.path().to_owned(),
                session_token_file: None,
                expires_at: None,
                principal: "user".to_owned(),
            }],
            repositories: Vec::new(),
        })
        .unwrap();
        let cancellation = CancellationToken::new();
        let runtime = Arc::new(RemoteGitRuntime::default());
        let options = RepositoryOptions::default();
        let metrics = Metrics::new().unwrap();
        let gateway = Gateway {
            repositories: Arc::new(BTreeMap::from([("repo".to_owned(), repository)])),
            mutations: Arc::new(mutation::Coordinator::new(
                Arc::clone(&runtime),
                options,
                metrics.clone(),
            )),
            runtime,
            options,
            auth,
            region: Arc::from("us-east-1"),
            admission: Admission::new(8, cancellation.clone(), metrics.clone()),
            metrics,
            local_cache,
            cancellation,
        };
        let repository = &gateway.repositories["repo"];
        for path in [
            "prefix/a-1",
            "prefix/a/item",
            "prefix/a0",
            "prefix/b/item",
            "prefix/c",
        ] {
            let attributes = crate::attributes::PutAttributes {
                etag_override: (path == "prefix/a-1").then(|| "listed-etag".to_owned()),
                ..Default::default()
            };
            gateway
                .mutations
                .apply(
                    repository,
                    "refs/heads/main",
                    &crab_remote_git::GitPath::new(path.as_bytes().to_vec()).unwrap(),
                    mutation::Change::Put {
                        bytes: Bytes::from_static(b"value"),
                        track_lfs: false,
                        attributes: Box::new(attributes),
                        condition: mutation::PutCondition::None,
                    },
                    "user",
                    &gateway.cancellation,
                )
                .await
                .unwrap();
        }

        let first = gateway
            .list_objects_v2(list_request(ListObjectsV2Input {
                bucket: "repo".to_owned(),
                prefix: Some("main/prefix/".to_owned()),
                delimiter: Some("/".to_owned()),
                max_keys: Some(2),
                ..Default::default()
            }))
            .await
            .unwrap()
            .output;
        assert_eq!(
            (
                first
                    .contents
                    .as_ref()
                    .and_then(|objects| objects.first())
                    .and_then(|object| object.key.as_deref()),
                first
                    .common_prefixes
                    .as_ref()
                    .and_then(|prefixes| prefixes.first())
                    .and_then(|prefix| prefix.prefix.as_deref()),
                first
                    .contents
                    .as_ref()
                    .and_then(|objects| objects.first())
                    .and_then(|object| object.e_tag.clone()),
                first.key_count,
                first.is_truncated,
            ),
            (
                Some("main/prefix/a-1"),
                Some("main/prefix/a/"),
                Some(ETag::Strong("listed-etag".to_owned())),
                Some(2),
                Some(true),
            )
        );
        let second = gateway
            .list_objects_v2(list_request(ListObjectsV2Input {
                bucket: "repo".to_owned(),
                prefix: Some("main/prefix/".to_owned()),
                delimiter: Some("/".to_owned()),
                continuation_token: first.next_continuation_token,
                max_keys: Some(2),
                ..Default::default()
            }))
            .await
            .unwrap()
            .output;
        assert_eq!(
            (
                second
                    .contents
                    .as_ref()
                    .and_then(|objects| objects.first())
                    .and_then(|object| object.key.as_deref()),
                second
                    .common_prefixes
                    .as_ref()
                    .and_then(|prefixes| prefixes.first())
                    .and_then(|prefix| prefix.prefix.as_deref()),
                second.next_continuation_token.as_deref(),
            ),
            (
                Some("main/prefix/a0"),
                Some("main/prefix/b/"),
                Some("main/prefix/b/"),
            )
        );
        let last = gateway
            .list_objects_v2(list_request(ListObjectsV2Input {
                bucket: "repo".to_owned(),
                prefix: Some("main/prefix/".to_owned()),
                delimiter: Some("/".to_owned()),
                continuation_token: second.next_continuation_token,
                max_keys: Some(2),
                ..Default::default()
            }))
            .await
            .unwrap()
            .output;
        assert_eq!(
            (
                last.contents
                    .and_then(|objects| objects.into_iter().next().and_then(|object| object.key)),
                last.key_count,
                last.is_truncated,
                last.next_continuation_token,
            ),
            (Some("main/prefix/c".to_owned()), Some(1), Some(false), None)
        );
        gateway.shutdown().await;
    }

    #[tokio::test]
    async fn response_stream_holds_read_capacity_until_client_disconnects() {
        let admission = Admission::new(8, CancellationToken::new(), Metrics::new().unwrap());
        let permit = admission.acquire(RequestClass::Read).await.unwrap();
        let _second = admission.acquire(RequestClass::Read).await.unwrap();
        let _third = admission.acquire(RequestClass::Read).await.unwrap();
        let _fourth = admission.acquire(RequestClass::Read).await.unwrap();
        let source: ContentStream = Box::pin(futures_util::stream::iter([Ok(Bytes::from_static(
            b"payload",
        ))]));
        let mut response = hold_permit(source, permit);

        assert_eq!(response.next().await.unwrap().unwrap(), b"payload"[..]);
        assert!(
            tokio::time::timeout(
                Duration::from_millis(10),
                admission.acquire(RequestClass::Read),
            )
            .await
            .is_err()
        );

        drop(response);
        admission.acquire(RequestClass::Read).await.unwrap();
    }

    #[test]
    fn url_encoded_list_values_round_trip_reserved_and_unicode_bytes() {
        let value = "main/special name(1)-雪.txt";
        let encoded = encode_list_value(value, true);
        assert_eq!(
            percent_encoding::percent_decode_str(&encoded)
                .decode_utf8()
                .unwrap(),
            value
        );
        assert!(encoded.contains("%2F"));
        assert!(encoded.contains("%28"));
    }

    #[test]
    fn object_tags_decode_form_encoding_and_reject_duplicate_keys() {
        let tags = parse_tagging_header(Some("project=crab+gateway&path=main%2Ftable")).unwrap();
        assert_eq!(
            tags.get("project").map(String::as_str),
            Some("crab gateway")
        );
        assert_eq!(tags.get("path").map(String::as_str), Some("main/table"));
        assert!(parse_tagging_header(Some("duplicate=one&duplicate=two")).is_err());
        assert!(parse_tagging_header(Some("aws%3Areserved=value")).is_err());
        assert!(parse_tagging_header(Some("invalid=%26")).is_err());
    }

    #[test]
    fn metadata_only_conversion_preserves_multipart_identity() {
        let attributes = crate::attributes::ObjectAttributes {
            blob_oid: "0123456789012345678901234567890123456789".to_owned(),
            etag: "multipart-etag".to_owned(),
            size: 7,
            modified_seconds: 1,
            completion_upload_id: Some("upload-id".to_owned()),
            checksums: crate::attributes::Checksums::default(),
            tags: BTreeMap::new(),
            parts: vec![crate::attributes::PartAttributes {
                number: 1,
                size: 7,
                checksums: crate::attributes::Checksums::default(),
            }],
            cache_control: None,
            content_disposition: None,
            content_encoding: None,
            content_language: None,
            content_type: None,
            expires: None,
            metadata: BTreeMap::new(),
        };

        let pending = stored_to_pending(&attributes);

        assert_eq!(pending.completion_upload_id.as_deref(), Some("upload-id"));
        assert_eq!(pending.parts, attributes.parts);
    }

    #[test]
    fn part_number_and_aligned_range_select_part_checksums() {
        let first = crate::attributes::PartAttributes {
            number: 1,
            size: 5,
            checksums: crate::attributes::Checksums {
                sha256: Some("first".to_owned()),
                ..Default::default()
            },
        };
        let second = crate::attributes::PartAttributes {
            number: 2,
            size: 3,
            checksums: crate::attributes::Checksums {
                sha256: Some("second".to_owned()),
                ..Default::default()
            },
        };
        let attributes = crate::attributes::ObjectAttributes {
            blob_oid: "0123456789012345678901234567890123456789".to_owned(),
            etag: "multipart-etag".to_owned(),
            size: 8,
            modified_seconds: 1,
            completion_upload_id: Some("upload-id".to_owned()),
            checksums: crate::attributes::Checksums::default(),
            tags: BTreeMap::new(),
            parts: vec![first, second],
            cache_control: None,
            content_disposition: None,
            content_encoding: None,
            content_language: None,
            content_type: None,
            expires: None,
            metadata: BTreeMap::new(),
        };

        let selected = read_selection(8, Some(&attributes), None, Some(2)).unwrap();
        let aligned = read_selection(
            8,
            Some(&attributes),
            Some(&Range::parse("bytes=5-7").unwrap()),
            None,
        )
        .unwrap();

        assert_eq!(
            (
                selected.range,
                selected.parts_count,
                selected.checksums.and_then(|value| value.sha256),
                aligned.checksums.and_then(|value| value.sha256),
            ),
            (
                5..8,
                Some(2),
                Some("second".to_owned()),
                Some("second".to_owned())
            )
        );
    }

    #[test]
    fn object_attribute_header_expands_the_aws_comma_delimited_list() {
        let values = vec![ObjectAttributes::from(
            "ETag,Checksum,ObjectSize,ObjectParts".to_owned(),
        )];
        assert_eq!(
            object_attribute_names(&values),
            std::collections::BTreeSet::from([
                ObjectAttributes::CHECKSUM,
                ObjectAttributes::ETAG,
                ObjectAttributes::OBJECT_PARTS,
                ObjectAttributes::OBJECT_SIZE,
            ])
        );
    }

    #[test]
    fn multipart_checksum_profile_accepts_only_validated_full_object_algorithms() {
        let algorithm = ChecksumAlgorithm::from_static(ChecksumAlgorithm::CRC64NVME);
        let full = ChecksumType::from_static(ChecksumType::FULL_OBJECT);
        assert_eq!(
            multipart_checksum_profile(Some(&algorithm), Some(&full)).unwrap(),
            (
                Some(ChecksumAlgorithm::CRC64NVME.to_owned()),
                Some(ChecksumType::FULL_OBJECT.to_owned())
            )
        );
        let composite = ChecksumType::from_static(ChecksumType::COMPOSITE);
        assert!(multipart_checksum_profile(Some(&algorithm), Some(&composite)).is_err());
        let sha256 = ChecksumAlgorithm::from_static(ChecksumAlgorithm::SHA256);
        assert!(multipart_checksum_profile(Some(&sha256), Some(&composite)).is_ok());
    }

    #[test]
    fn composite_sha256_hashes_ordered_binary_part_checksums() {
        use sha2::Digest as _;

        let encode = |value: &[u8]| base64::engine::general_purpose::STANDARD.encode(value);
        let first = sha2::Sha256::digest(b"first");
        let second = sha2::Sha256::digest(b"second");
        let parts = [
            crate::attributes::Checksums {
                sha256: Some(encode(&first)),
                ..Default::default()
            },
            crate::attributes::Checksums {
                sha256: Some(encode(&second)),
                ..Default::default()
            },
        ];
        let expected = sha2::Sha256::digest([first.as_slice(), second.as_slice()].concat());
        let actual = composite_checksums(parts.iter(), ChecksumAlgorithm::SHA256).unwrap();

        assert_eq!(
            actual.sha256.as_deref(),
            Some(format!("{}-2", encode(&expected)).as_str())
        );
        assert_eq!(
            actual.checksum_type.as_deref(),
            Some(ChecksumType::COMPOSITE)
        );
    }

    #[tokio::test]
    async fn content_above_inline_threshold_is_stored_as_streamable_lfs() {
        use futures_util::TryStreamExt as _;

        let store = crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new()));
        let repository = Repository::new(
            RepositoryConfig {
                name: "repo".to_owned(),
                provider: StorageProviderKind::Local,
                bucket: "memory".to_owned(),
                prefix: "large-content-test".to_owned(),
                default_branch: "main".to_owned(),
                members: Vec::new(),
                protected_branches: Vec::new(),
                max_active_multipart_uploads: 16,
                multipart_staging_bytes_per_upload: 50_000_000_000_000,
                multipart_upload_ttl_seconds: 604_800,
            },
            store,
        )
        .unwrap();
        let content = b"content larger than the test inline limit";
        let metrics = Metrics::new().unwrap();
        let mut writer = crate::content::SpoolWriter::new(&metrics, None)
            .await
            .unwrap();
        writer.write(content, u64::MAX).await.unwrap();
        let spool = writer.finish().await.unwrap();

        let mutation = mutation_bytes_with_inline_limit(&repository, &spool, 8)
            .await
            .unwrap();
        assert!(mutation.track_lfs);
        let PointerKind::Lfs(pointer) = crab_git::classify(&mutation.bytes) else {
            panic!("expected an LFS pointer");
        };
        let (_, _, stream) = repository
            .lfs
            .get_stream(&pointer.oid, pointer.size, None)
            .await
            .unwrap();
        let actual = stream
            .try_fold(Vec::new(), |mut bytes, chunk| async move {
                bytes.extend_from_slice(&chunk);
                Ok(bytes)
            })
            .await
            .unwrap();

        assert_eq!(actual, content);
    }

    #[tokio::test]
    async fn large_multipart_assembly_replays_durable_parts_into_lfs() {
        use futures_util::TryStreamExt as _;

        let store = crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new()));
        let repository = Repository::new(
            RepositoryConfig {
                name: "repo".to_owned(),
                provider: StorageProviderKind::Local,
                bucket: "memory".to_owned(),
                prefix: "multipart-replay-test".to_owned(),
                default_branch: "main".to_owned(),
                members: Vec::new(),
                protected_branches: Vec::new(),
                max_active_multipart_uploads: 16,
                multipart_staging_bytes_per_upload: 50_000_000_000_000,
                multipart_upload_ttl_seconds: 604_800,
            },
            store,
        )
        .unwrap();
        let session = crate::multipart::create(
            &repository,
            crate::multipart::Initiation {
                bucket: "repo",
                key: "main/file.bin",
                branch: "refs/heads/main",
                path: "file.bin",
                principal: "user",
                attributes: crate::attributes::PutAttributes::default(),
                checksum_algorithm: None,
                checksum_type: None,
                now: 10,
            },
        )
        .await
        .unwrap();
        let body = Bytes::from_static(b"durable multipart bytes");
        let etag = md5_hex(&body);
        let metrics = Metrics::new().unwrap();
        let mut part_writer = crate::content::SpoolWriter::new(&metrics, None)
            .await
            .unwrap();
        part_writer.write(&body, u64::MAX).await.unwrap();
        let part_spool = part_writer.finish().await.unwrap();
        let loaded = crate::multipart::load(&repository, &session.id)
            .await
            .unwrap();
        crate::multipart::register_part(
            &repository,
            loaded,
            1,
            &part_spool,
            etag.clone(),
            crate::attributes::Checksums::default(),
            11,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        let loaded = crate::multipart::load(&repository, &session.id)
            .await
            .unwrap();
        let (session, parts) = crate::multipart::freeze(
            &repository,
            loaded,
            &[(1, etag)],
            crate::content::MAX_MULTIPART_OBJECT_BYTES,
        )
        .await
        .unwrap();
        let receipt_etag = multipart_etag(&parts).unwrap();
        let receipt_oid =
            gix_hash::ObjectId::from_hex(b"e69de29bb2d1d6434b8b29ae775ad8c2e48c5391").unwrap();
        let receipt_attributes = crate::attributes::ObjectAttributes::new(
            receipt_oid,
            receipt_etag.clone(),
            body.len() as u64,
            12,
            crate::attributes::PutAttributes {
                etag_override: Some(receipt_etag.clone()),
                completion_upload_id: Some(session.id.clone()),
                logical_size: Some(body.len() as u64),
                parts: parts
                    .iter()
                    .map(|part| crate::attributes::PartAttributes {
                        number: part.number,
                        size: part.size,
                        checksums: part.checksums.clone(),
                    })
                    .collect(),
                ..Default::default()
            },
        );
        let receipt = completion_receipt(&session, &receipt_attributes).unwrap();
        assert_eq!(receipt.map(|value| value.0), Some(receipt_etag));
        let mut digester = crate::content::Digester::new();
        digester.write(&body, u64::MAX).unwrap();
        let (size, digests) = digester.finish().unwrap();

        let mutation = mutation_multipart_bytes(
            &repository,
            &parts,
            MultipartAssembly::Large { size, digests },
        )
        .await
        .unwrap();

        let PointerKind::Lfs(pointer) = crab_git::classify(&mutation.bytes) else {
            panic!("expected an LFS pointer");
        };
        let (_, _, stream) = repository
            .lfs
            .get_stream(&pointer.oid, pointer.size, None)
            .await
            .unwrap();
        let actual = stream.try_collect::<Vec<_>>().await.unwrap().concat();
        assert_eq!(actual, body);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn published_frozen_multipart_recovers_without_a_client_retry() {
        let store = crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new()));
        let repository_config = RepositoryConfig {
            name: "repo".to_owned(),
            provider: StorageProviderKind::Local,
            bucket: "memory".to_owned(),
            prefix: "multipart-recovery-test".to_owned(),
            default_branch: "main".to_owned(),
            members: Vec::new(),
            protected_branches: Vec::new(),
            max_active_multipart_uploads: 1,
            multipart_staging_bytes_per_upload: 50_000_000_000_000,
            multipart_upload_ttl_seconds: 604_800,
        };
        let repository = Repository::new(repository_config, store).unwrap();
        crab_write::initialize::initialize_repository(
            &repository.store,
            &repository.layout,
            "refs/heads/main",
        )
        .await
        .unwrap();
        let secret = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(secret.path(), "multipart-recovery-secret").unwrap();
        let auth = GatewayAuth::load(&Config {
            listen: "127.0.0.1:0".parse().unwrap(),
            management_listen: "127.0.0.1:1".parse().unwrap(),
            endpoint_domain: None,
            region: "us-east-1".to_owned(),
            max_in_flight_requests: 8,
            cache: crate::LocalCacheConfig {
                directory: std::env::temp_dir().join("crab-s3-gateway-recovery-cache"),
                max_bytes: 128 * 1024 * 1024,
            },
            credentials: vec![crate::CredentialConfig {
                access_key: "recovery-key".to_owned(),
                secret_key_file: secret.path().to_owned(),
                session_token_file: None,
                expires_at: None,
                principal: "user".to_owned(),
            }],
            repositories: Vec::new(),
        })
        .unwrap();
        let cancellation = CancellationToken::new();
        let runtime = Arc::new(RemoteGitRuntime::default());
        let options = RepositoryOptions::default();
        let metrics = Metrics::new().unwrap();
        let cache_dir = tempfile::tempdir().unwrap();
        let local_cache = Arc::new(LocalCache::with_limits(
            cache_dir.path().join("cache"),
            128 * 1024 * 1024,
            Some(128 * 1024 * 1024),
        ));
        let gateway = Gateway {
            repositories: Arc::new(BTreeMap::from([("repo".to_owned(), repository)])),
            mutations: Arc::new(mutation::Coordinator::new(
                Arc::clone(&runtime),
                options,
                metrics.clone(),
            )),
            runtime,
            options,
            auth,
            region: Arc::from("us-east-1"),
            admission: Admission::new(8, cancellation.clone(), metrics.clone()),
            metrics,
            local_cache,
            cancellation,
        };
        let repository = &gateway.repositories["repo"];
        let session = crate::multipart::create(
            repository,
            crate::multipart::Initiation {
                bucket: "repo",
                key: "main/object.bin",
                branch: "refs/heads/main",
                path: "object.bin",
                principal: "user",
                attributes: crate::attributes::PutAttributes::default(),
                checksum_algorithm: None,
                checksum_type: None,
                now: 10,
            },
        )
        .await
        .unwrap();
        let body = Bytes::from_static(b"published before multipart state completion");
        let part_etag = md5_hex(&body);
        let mut writer = crate::content::SpoolWriter::new(&gateway.metrics, None)
            .await
            .unwrap();
        writer.write(&body, u64::MAX).await.unwrap();
        let spool = writer.finish().await.unwrap();
        crate::multipart::register_part(
            repository,
            crate::multipart::load(repository, &session.id)
                .await
                .unwrap(),
            1,
            &spool,
            part_etag.clone(),
            crate::attributes::Checksums::default(),
            11,
            &gateway.cancellation,
        )
        .await
        .unwrap();
        let (session, parts) = crate::multipart::freeze(
            repository,
            crate::multipart::load(repository, &session.id)
                .await
                .unwrap(),
            &[(1, part_etag.clone())],
            crate::content::MAX_MULTIPART_OBJECT_BYTES,
        )
        .await
        .unwrap();
        let completion_etag = multipart_etag(&parts).unwrap();
        let session = crate::multipart::record_completion_outcome(
            repository,
            &session.id,
            &completion_etag,
            &crate::attributes::Checksums::default(),
        )
        .await
        .unwrap();
        let mut attributes = session.attributes.clone();
        attributes.etag_override = Some(completion_etag.clone());
        attributes.completion_upload_id = Some(session.id.clone());
        attributes.logical_size = Some(body.len() as u64);
        attributes.parts = parts
            .iter()
            .map(|part| crate::attributes::PartAttributes {
                number: part.number,
                size: part.size,
                checksums: part.checksums.clone(),
            })
            .collect();
        gateway
            .mutations
            .apply(
                repository,
                &session.branch,
                &crab_remote_git::GitPath::new(b"object.bin".to_vec()).unwrap(),
                mutation::Change::Put {
                    bytes: body.clone(),
                    track_lfs: false,
                    attributes: Box::new(attributes),
                    condition: mutation::PutCondition::None,
                },
                "user",
                &gateway.cancellation,
            )
            .await
            .unwrap();
        let overwrite = Bytes::from_static(b"newer object version");
        gateway
            .mutations
            .apply(
                repository,
                &session.branch,
                &crab_remote_git::GitPath::new(b"object.bin".to_vec()).unwrap(),
                mutation::Change::Put {
                    bytes: overwrite.clone(),
                    track_lfs: false,
                    attributes: Box::default(),
                    condition: mutation::PutCondition::None,
                },
                "user",
                &gateway.cancellation,
            )
            .await
            .unwrap();

        let recovered = gateway
            .recover_published_multipart(
                repository,
                crate::multipart::load(repository, &session.id)
                    .await
                    .unwrap(),
            )
            .await
            .unwrap();
        let terminal = crate::multipart::load(repository, &session.id)
            .await
            .unwrap();
        let replacement = crate::multipart::create(
            repository,
            crate::multipart::Initiation {
                bucket: "repo",
                key: "main/replacement.bin",
                branch: "refs/heads/main",
                path: "replacement.bin",
                principal: "user",
                attributes: crate::attributes::PutAttributes::default(),
                checksum_algorithm: None,
                checksum_type: None,
                now: 13,
            },
        )
        .await;
        let current = gateway
            .read_object(repository, "main/object.bin")
            .await
            .unwrap();
        let ReadContent::Ordinary(current) = current.content else {
            panic!("expected ordinary overwrite");
        };

        assert!(
            recovered
                && crate::multipart::completed_etag(&terminal.session, &[(1, part_etag)])
                    .is_ok_and(|etag| etag == Some(completion_etag.as_str()))
                && replacement.is_ok()
                && current == overwrite
        );
        gateway.shutdown().await;
    }

    #[tokio::test]
    async fn put_checksums_accept_all_supported_algorithms_and_reject_mismatch() {
        use sha1::Digest as _;

        let body = b"123456789";
        let encode = |bytes: &[u8]| base64::engine::general_purpose::STANDARD.encode(bytes);
        let metrics = Metrics::new().unwrap();
        let mut writer = crate::content::SpoolWriter::new(&metrics, None)
            .await
            .unwrap();
        writer.write(body, u64::MAX).await.unwrap();
        let spool = writer.finish().await.unwrap();
        for algorithm in [
            ChecksumAlgorithm::CRC32,
            ChecksumAlgorithm::CRC32C,
            ChecksumAlgorithm::CRC64NVME,
            ChecksumAlgorithm::SHA1,
            ChecksumAlgorithm::SHA256,
        ] {
            let calculated = calculated_checksums(&spool.digests, algorithm).unwrap();
            RequestChecksums {
                algorithm: Some(ChecksumAlgorithm::from_static(algorithm)),
                crc32: calculated.crc32,
                crc32c: calculated.crc32c,
                crc64nvme: calculated.crc64nvme,
                sha1: calculated.sha1,
                sha256: calculated.sha256,
            }
            .verify(&spool.digests)
            .unwrap();
        }

        let invalid = RequestChecksums {
            algorithm: Some(ChecksumAlgorithm::from_static(ChecksumAlgorithm::SHA256)),
            sha256: Some(encode(&sha2::Sha256::digest(b"different"))),
            ..Default::default()
        };
        let error = invalid.verify(&spool.digests).unwrap_err();
        assert_eq!(error.code().as_str(), "BadDigest");
    }

    #[test]
    fn object_checksums_reject_multiple_algorithms() {
        let checksums = RequestChecksums {
            crc32: Some("AAAAAA==".to_owned()),
            sha256: Some("AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=".to_owned()),
            ..Default::default()
        };

        assert!(checksums.ensure_single_value().is_err());
    }

    #[test]
    fn checksum_mode_accepts_case_insensitive_enabled_value() {
        let mode = ChecksumMode::from_static("enabled");
        let checksums = crate::attributes::Checksums {
            sha256: Some("checksum".to_owned()),
            ..Default::default()
        };

        assert_eq!(
            response_checksums(Some(&mode), Some(&checksums))
                .unwrap()
                .sha256,
            checksums.sha256
        );
    }

    #[test]
    fn copy_accepts_explicit_standard_storage_class() {
        let input = CopyObjectInput::builder()
            .bucket("repo".to_owned())
            .key("main/copy.bin".to_owned())
            .copy_source(CopySource::Bucket {
                bucket: "repo".into(),
                key: "main/source.bin".into(),
                version_id: None,
            })
            .storage_class(Some(StorageClass::from_static(StorageClass::STANDARD)))
            .build()
            .unwrap();

        reject_copy_extensions(&input).unwrap();
    }

    #[test]
    fn head_response_overrides_take_precedence_over_stored_metadata() {
        let stored = "stored-content-type".to_owned();
        assert_eq!(
            response_override(Some("requested-content-type".to_owned()), Some(&stored)),
            Some("requested-content-type".to_owned())
        );
        assert_eq!(
            response_override(None, Some(&stored)),
            Some("stored-content-type".to_owned())
        );
        assert_eq!(response_override::<String>(None, None), None);
    }

    #[test]
    fn modeled_request_extensions_fail_closed() {
        let error = reject_get_extensions(&GetObjectInput {
            expected_bucket_owner: Some("owner".to_owned()),
            ..Default::default()
        })
        .unwrap_err();
        assert_eq!(error.code().as_str(), "NotImplemented");

        let error = reject_head_extensions(&HeadObjectInput {
            expected_bucket_owner: Some("owner".to_owned()),
            ..Default::default()
        })
        .unwrap_err();
        assert_eq!(error.code().as_str(), "NotImplemented");

        let error = reject_put_extensions(&PutObjectInput {
            write_offset_bytes: Some(0),
            ..Default::default()
        })
        .unwrap_err();
        assert_eq!(error.code().as_str(), "NotImplemented");

        let error = reject_list_extensions(&ListObjectsV2Input {
            fetch_owner: Some(true),
            ..Default::default()
        })
        .unwrap_err();
        assert_eq!(error.code().as_str(), "NotImplemented");
    }

    #[test]
    fn delete_if_match_is_supported_but_directory_only_conditions_are_not() {
        reject_delete_extensions(&DeleteObjectInput {
            if_match: Some(ETagCondition::Any),
            ..Default::default()
        })
        .unwrap();
        let error = reject_delete_extensions(&DeleteObjectInput {
            if_match_size: Some(1),
            ..Default::default()
        })
        .unwrap_err();
        assert_eq!(error.code().as_str(), "NotImplemented");
    }

    #[test]
    fn xml_request_integrity_validates_md5_and_checksum_headers() {
        use md5::Digest as _;

        let body = b"tagging body";
        let digest = crate::server::RequestBodyDigest::new();
        digest.update(body);
        let mut req = S3Request {
            input: (),
            method: http::Method::PUT,
            uri: http::Uri::from_static("/repo/main/object?tagging"),
            headers: http::HeaderMap::new(),
            extensions: http::Extensions::new(),
            credentials: None,
            region: None,
            service: None,
            trailing_headers: None,
        };
        req.extensions.insert(digest);
        let md5 = base64::engine::general_purpose::STANDARD.encode(md5::Md5::digest(body));
        verify_xml_request_integrity(&req, Some(&md5), None, true).unwrap();

        let digest = crate::server::RequestBodyDigest::new();
        digest.update(body);
        let mut req = req;
        req.extensions = http::Extensions::new();
        req.extensions.insert(digest);
        let mut checksums = http::HeaderMap::new();
        let mut crc = crc_fast::Digest::new(crc_fast::CrcAlgorithm::Crc32IsoHdlc);
        crc.update(body);
        checksums.insert(
            "x-amz-checksum-crc32",
            base64::engine::general_purpose::STANDARD
                .encode(u32::try_from(crc.finalize()).unwrap().to_be_bytes())
                .parse()
                .unwrap(),
        );
        req.headers = checksums;
        verify_xml_request_integrity(
            &req,
            None,
            Some(&ChecksumAlgorithm::from_static(ChecksumAlgorithm::CRC32)),
            true,
        )
        .unwrap();

        let digest = crate::server::RequestBodyDigest::new();
        digest.update(body);
        req.extensions = http::Extensions::new();
        req.extensions.insert(digest);
        req.headers.clear();
        let error = verify_xml_request_integrity(&req, None, None, true).unwrap_err();
        assert_eq!(error.code().as_str(), "InvalidDigest");

        let digest = crate::server::RequestBodyDigest::new();
        digest.update(body);
        req.extensions = http::Extensions::new();
        req.extensions.insert(digest);
        req.headers.insert(
            "content-md5",
            base64::engine::general_purpose::STANDARD
                .encode(md5::Md5::digest(b"different"))
                .parse()
                .unwrap(),
        );
        let error = verify_xml_request_integrity(&req, None, None, false).unwrap_err();
        assert_eq!(error.code().as_str(), "BadDigest");
    }

    #[test]
    fn multipart_page_bounds_reject_invalid_markers_and_sizes() {
        assert_eq!(
            multipart_page_bounds(Some(-1), None)
                .unwrap_err()
                .code()
                .as_str(),
            "InvalidArgument"
        );
        assert_eq!(
            multipart_page_bounds(None, Some(0))
                .unwrap_err()
                .code()
                .as_str(),
            "InvalidArgument"
        );
        assert_eq!(multipart_page_bounds(Some(7), Some(32)).unwrap(), (7, 32));
    }

    #[test]
    fn git_trees_are_absent_from_the_object_namespace() {
        assert_eq!(
            object_entry_error(EntryKind::Tree).code().as_str(),
            "NoSuchKey"
        );
        assert_eq!(
            object_entry_error(EntryKind::Submodule).code().as_str(),
            "InvalidObjectState"
        );
    }

    #[test]
    fn slow_down_tells_clients_when_to_retry() {
        let error = slow_down_error();

        assert_eq!(
            error
                .headers()
                .and_then(|headers| headers.get(http::header::RETRY_AFTER)),
            Some(&http::HeaderValue::from_static("1"))
        );
    }

    #[test]
    fn scratch_capacity_pressure_is_a_retryable_s3_error() {
        let error = content_error(crate::content::Error::Capacity(
            crate::metrics::ScratchCapacityError::Exhausted,
        ));

        assert_eq!(error.code().as_str(), "SlowDown");
        assert_eq!(
            error
                .headers()
                .and_then(|headers| headers.get(http::header::RETRY_AFTER)),
            Some(&http::HeaderValue::from_static("1"))
        );
    }

    #[test]
    fn virtual_directory_marker_supports_create_if_absent() {
        let input = PutObjectInput::builder()
            .bucket("repo".to_owned())
            .key("main/path/".to_owned())
            .if_none_match(Some(ETagCondition::Any))
            .build()
            .unwrap();

        assert!(virtual_marker_put_condition(&input).is_ok());
    }

    #[test]
    fn streaming_transport_encoding_is_not_stored_as_object_metadata() {
        assert_eq!(stored_content_encoding(Some("aws-chunked"), true), None);
        assert_eq!(
            stored_content_encoding(Some("gzip, aws-chunked"), true).as_deref(),
            Some("gzip")
        );
        assert_eq!(
            stored_content_encoding(Some("AWS-CHUNKED, br"), true).as_deref(),
            Some("br")
        );
    }

    #[test]
    fn ordinary_content_encoding_is_preserved_verbatim() {
        assert_eq!(
            stored_content_encoding(Some("gzip, aws-chunked"), false).as_deref(),
            Some("gzip, aws-chunked")
        );
        assert_eq!(stored_content_encoding(None, true), None);
    }

    #[test]
    fn list_accepts_the_restore_status_optional_attribute() {
        let values = vec![OptionalObjectAttributes::from_static(
            OptionalObjectAttributes::RESTORE_STATUS,
        )];

        assert!(supported_list_attributes(Some(&values)));
    }
}
