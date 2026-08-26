use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crab_metadata::git_object_locator::GitPackInventoryEntry;
use crab_metadata::manifests::Manifest;
use crab_xet::hash::MerkleHash;
use gix_hash::ObjectId;
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore, watch};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tokio_util::task::task_tracker::TaskTrackerToken;

use crate::budget::BudgetUsage;
use crate::cache::BoundedLru;
use crate::objects::RawTreeEntry;
use crate::reader::{GitObject, PackIndex, PackedEntry};
use crate::{
    AnnotatedTag, Blame, CacheOutcome, Commit, Error, GeneratedPack, GeneratedPackCacheKey,
    GitPath, MetricKind, MetricObservation, NoopMetrics, RemoteGitMetrics, RepositoryIdentity,
    RepositoryOptions, Result,
};

/// Process-wide bounds for remote Git caches and execution admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeOptions {
    /// Maximum concurrent object-store operations.
    pub max_origin_concurrency: usize,
    /// Maximum concurrent blocking decode operations.
    pub max_decode_concurrency: usize,
    /// Maximum distinct immutable object reads executing concurrently.
    pub max_object_flights: usize,
    /// Maximum distinct pack-index loads executing concurrently.
    pub max_pack_index_flights: usize,
    /// Maximum verified Git objects retained in memory.
    pub max_object_cache_entries: usize,
    /// Maximum verified Git-object payload bytes retained in memory.
    pub max_object_cache_bytes: usize,
    /// Maximum verified pack indexes retained in memory.
    pub max_pack_index_cache_entries: usize,
    /// Maximum parsed pack-index bytes retained in memory.
    pub max_pack_index_cache_bytes: usize,
    /// Maximum parsed commits, tags, and trees retained in memory.
    pub max_parsed_cache_entries: usize,
    /// Maximum estimated parsed-object bytes retained in memory.
    pub max_parsed_cache_bytes: usize,
    /// Maximum immutable blame results retained in memory.
    pub max_blame_cache_entries: usize,
    /// Maximum estimated immutable blame-result bytes retained in memory.
    pub max_blame_cache_bytes: usize,
    /// Maximum short-lived manifest entries retained in memory.
    pub max_manifest_cache_entries: usize,
    /// Maximum estimated manifest bytes retained in memory.
    pub max_manifest_cache_bytes: usize,
    /// Lifetime before a manifest body must be fetched again.
    pub manifest_cache_ttl: Duration,
    /// Maximum content-addressed inventories retained in memory.
    pub max_inventory_cache_entries: usize,
    /// Maximum estimated inventory bytes retained in memory.
    pub max_inventory_cache_bytes: usize,
    /// Maximum exact misses retained across generations.
    pub max_negative_cache_entries: usize,
    /// Maximum identity-key bytes retained for exact misses.
    pub max_negative_cache_bytes: usize,
    /// Lifetime of one generation-scoped exact miss.
    pub negative_cache_ttl: Duration,
}

/// Low-cardinality occupancy snapshot for readiness and capacity metrics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeSnapshot {
    /// Retained verified Git objects.
    pub object_entries: usize,
    /// Estimated resident verified-object bytes.
    pub object_bytes: usize,
    /// Retained parsed pack indexes.
    pub pack_index_entries: usize,
    /// Estimated resident parsed pack-index bytes.
    pub pack_index_bytes: usize,
    /// Retained parsed commits, trees, and annotated tags.
    pub parsed_entries: usize,
    /// Estimated resident parsed-object bytes.
    pub parsed_bytes: usize,
    /// Retained immutable blame results.
    pub blame_entries: usize,
    /// Estimated resident immutable blame-result bytes.
    pub blame_bytes: usize,
    /// Retained manifest pointers.
    pub manifest_entries: usize,
    /// Estimated resident manifest bytes.
    pub manifest_bytes: usize,
    /// Retained immutable pack inventories.
    pub inventory_entries: usize,
    /// Estimated resident inventory bytes.
    pub inventory_bytes: usize,
    /// Retained exact negative object results.
    pub negative_entries: usize,
    /// Estimated resident negative-cache bytes.
    pub negative_bytes: usize,
    /// Distinct immutable object reads currently executing.
    pub active_object_flights: usize,
    /// Distinct immutable pack-index loads currently executing.
    pub active_pack_index_flights: usize,
    /// Distinct generated response packs currently executing.
    pub active_generated_pack_flights: usize,
}

impl Default for RuntimeOptions {
    fn default() -> Self {
        Self {
            max_origin_concurrency: 64,
            max_decode_concurrency: 16,
            max_object_flights: 256,
            max_pack_index_flights: 32,
            max_object_cache_entries: 16_384,
            max_object_cache_bytes: 128 * 1024 * 1024,
            max_pack_index_cache_entries: 256,
            max_pack_index_cache_bytes: 64 * 1024 * 1024,
            max_parsed_cache_entries: 16_384,
            max_parsed_cache_bytes: 64 * 1024 * 1024,
            max_blame_cache_entries: 256,
            max_blame_cache_bytes: 32 * 1024 * 1024,
            max_manifest_cache_entries: 1_024,
            max_manifest_cache_bytes: 16 * 1024 * 1024,
            manifest_cache_ttl: Duration::from_secs(2),
            max_inventory_cache_entries: 1_024,
            max_inventory_cache_bytes: 64 * 1024 * 1024,
            max_negative_cache_entries: 4_096,
            max_negative_cache_bytes: 1024 * 1024,
            negative_cache_ttl: Duration::from_secs(5),
        }
    }
}

impl RuntimeOptions {
    fn validate(self) -> Result<Self> {
        for (name, value) in [
            ("max_origin_concurrency", self.max_origin_concurrency),
            ("max_decode_concurrency", self.max_decode_concurrency),
            ("max_object_flights", self.max_object_flights),
            ("max_pack_index_flights", self.max_pack_index_flights),
        ] {
            if value == 0 {
                return Err(Error::InvalidLimit { name });
            }
        }
        if self.max_negative_cache_entries > 0
            && self.max_negative_cache_bytes > 0
            && self.negative_cache_ttl.is_zero()
        {
            return Err(Error::InvalidLimit {
                name: "negative_cache_ttl",
            });
        }
        if self.max_manifest_cache_entries > 0
            && self.max_manifest_cache_bytes > 0
            && self.manifest_cache_ttl.is_zero()
        {
            return Err(Error::InvalidLimit {
                name: "manifest_cache_ttl",
            });
        }
        Ok(self)
    }
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct ObjectCacheKey {
    identity: RepositoryIdentity,
    generation: u64,
    oid: ObjectId,
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct PackIndexCacheKey {
    identity: RepositoryIdentity,
    pack_id: MerkleHash,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct InventoryCacheKey {
    identity: RepositoryIdentity,
    pack_index_hash: MerkleHash,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct BlameCacheKey {
    identity: RepositoryIdentity,
    generation: u64,
    commit: ObjectId,
    path: GitPath,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct PackedFlightKey {
    object: ObjectCacheKey,
    max_inflated_bytes: u64,
    max_object_bytes: u64,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct PackIndexFlightKey {
    index: PackIndexCacheKey,
    max_source_bytes: u64,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct GeneratedPackFlightKey {
    request: GeneratedPackCacheKey,
    options: RepositoryOptions,
}

#[derive(Clone)]
struct CachedManifest {
    manifest: Manifest,
    etag: String,
    expires: Instant,
}

impl PackIndexCacheKey {
    pub(crate) fn new(identity: &RepositoryIdentity, pack_id: MerkleHash) -> Self {
        Self {
            identity: identity.clone(),
            pack_id,
        }
    }
}

impl ObjectCacheKey {
    pub(crate) fn new(identity: &RepositoryIdentity, generation: u64, oid: ObjectId) -> Self {
        Self {
            identity: identity.clone(),
            generation,
            oid,
        }
    }
}

type SharedPackedResult = std::result::Result<Arc<PackedEntry>, Arc<Error>>;
type PackedFlight = watch::Receiver<Option<SharedPackedResult>>;
type SharedPackIndexResult = std::result::Result<Arc<PackIndex>, Arc<Error>>;
type PackIndexFlight = watch::Receiver<Option<SharedPackIndexResult>>;
type SharedGeneratedPackResult = std::result::Result<Arc<GeneratedPack>, Arc<Error>>;
type GeneratedPackFlight = watch::Receiver<Option<SharedGeneratedPackResult>>;

#[derive(Clone)]
enum ParsedObject {
    Commit {
        value: Arc<Commit>,
        source_bytes: u64,
    },
    Tag {
        value: Arc<AnnotatedTag>,
        source_bytes: u64,
    },
    Tree {
        value: Arc<Vec<RawTreeEntry>>,
        source_bytes: u64,
    },
}

#[derive(Clone)]
pub(crate) struct CachedBlame {
    pub(crate) value: Arc<Blame>,
    pub(crate) usage: BudgetUsage,
}

/// Shared bounded runtime state for filesystem-free repository readers.
///
/// A service constructs one runtime and reuses it across repository handles.
/// Every cache is a disposable accelerator; immutable repository metadata and
/// verified object bytes remain the only correctness authority.
pub struct RemoteGitRuntime {
    options: RuntimeOptions,
    metrics: Arc<dyn RemoteGitMetrics>,
    origin: Arc<Semaphore>,
    decode: Arc<Semaphore>,
    object_flight_admission: Arc<Semaphore>,
    pack_index_flight_admission: Arc<Semaphore>,
    generated_pack_flight_admission: Arc<Semaphore>,
    object_cache: Mutex<BoundedLru<ObjectCacheKey, Arc<GitObject>>>,
    pack_index_cache: Mutex<BoundedLru<PackIndexCacheKey, Arc<PackIndex>>>,
    parsed_cache: Mutex<BoundedLru<ObjectCacheKey, ParsedObject>>,
    blame_cache: Mutex<BoundedLru<BlameCacheKey, CachedBlame>>,
    manifest_cache: Mutex<BoundedLru<RepositoryIdentity, Arc<CachedManifest>>>,
    inventory_cache:
        Mutex<BoundedLru<InventoryCacheKey, Arc<HashMap<MerkleHash, GitPackInventoryEntry>>>>,
    packed_flights: Mutex<HashMap<PackedFlightKey, PackedFlight>>,
    pack_index_flights: Mutex<HashMap<PackIndexFlightKey, PackIndexFlight>>,
    generated_pack_flights: Mutex<HashMap<GeneratedPackFlightKey, GeneratedPackFlight>>,
    negative_cache: Mutex<BoundedLru<ObjectCacheKey, Instant>>,
    tasks: TaskTracker,
    shutdown: CancellationToken,
}

impl RemoteGitRuntime {
    /// Construct a validated process runtime and metrics sink.
    ///
    /// Zero object-cache bounds disable that cache. Admission bounds must be
    /// non-zero, and an enabled negative cache requires a non-zero lifetime.
    pub fn new(options: RuntimeOptions, metrics: Arc<dyn RemoteGitMetrics>) -> Result<Self> {
        Ok(Self::from_validated(options.validate()?, metrics))
    }

    fn from_validated(options: RuntimeOptions, metrics: Arc<dyn RemoteGitMetrics>) -> Self {
        Self {
            options,
            metrics,
            origin: Arc::new(Semaphore::new(options.max_origin_concurrency)),
            decode: Arc::new(Semaphore::new(options.max_decode_concurrency)),
            object_flight_admission: Arc::new(Semaphore::new(options.max_object_flights)),
            pack_index_flight_admission: Arc::new(Semaphore::new(options.max_pack_index_flights)),
            generated_pack_flight_admission: Arc::new(Semaphore::new(
                options.max_decode_concurrency,
            )),
            object_cache: Mutex::new(BoundedLru::new(
                options.max_object_cache_entries,
                options.max_object_cache_bytes,
            )),
            pack_index_cache: Mutex::new(BoundedLru::new(
                options.max_pack_index_cache_entries,
                options.max_pack_index_cache_bytes,
            )),
            parsed_cache: Mutex::new(BoundedLru::new(
                options.max_parsed_cache_entries,
                options.max_parsed_cache_bytes,
            )),
            blame_cache: Mutex::new(BoundedLru::new(
                options.max_blame_cache_entries,
                options.max_blame_cache_bytes,
            )),
            manifest_cache: Mutex::new(BoundedLru::new(
                options.max_manifest_cache_entries,
                options.max_manifest_cache_bytes,
            )),
            inventory_cache: Mutex::new(BoundedLru::new(
                options.max_inventory_cache_entries,
                options.max_inventory_cache_bytes,
            )),
            packed_flights: Mutex::new(HashMap::new()),
            pack_index_flights: Mutex::new(HashMap::new()),
            generated_pack_flights: Mutex::new(HashMap::new()),
            negative_cache: Mutex::new(BoundedLru::new(
                options.max_negative_cache_entries,
                options.max_negative_cache_bytes,
            )),
            tasks: TaskTracker::new(),
            shutdown: CancellationToken::new(),
        }
    }

    /// Return the validated runtime configuration.
    #[must_use]
    pub const fn options(&self) -> RuntimeOptions {
        self.options
    }

    /// Snapshot bounded cache occupancy and active immutable work.
    pub async fn snapshot(&self) -> RuntimeSnapshot {
        let object = self.object_cache.lock().await;
        let (object_entries, object_bytes) = (object.len(), object.resident_bytes());
        drop(object);
        let pack_index = self.pack_index_cache.lock().await;
        let (pack_index_entries, pack_index_bytes) =
            (pack_index.len(), pack_index.resident_bytes());
        drop(pack_index);
        let parsed = self.parsed_cache.lock().await;
        let (parsed_entries, parsed_bytes) = (parsed.len(), parsed.resident_bytes());
        drop(parsed);
        let blame = self.blame_cache.lock().await;
        let (blame_entries, blame_bytes) = (blame.len(), blame.resident_bytes());
        drop(blame);
        let manifest = self.manifest_cache.lock().await;
        let (manifest_entries, manifest_bytes) = (manifest.len(), manifest.resident_bytes());
        drop(manifest);
        let inventory = self.inventory_cache.lock().await;
        let (inventory_entries, inventory_bytes) = (inventory.len(), inventory.resident_bytes());
        drop(inventory);
        let negative = self.negative_cache.lock().await;
        let (negative_entries, negative_bytes) = (negative.len(), negative.resident_bytes());
        drop(negative);
        RuntimeSnapshot {
            object_entries,
            object_bytes,
            pack_index_entries,
            pack_index_bytes,
            parsed_entries,
            parsed_bytes,
            blame_entries,
            blame_bytes,
            manifest_entries,
            manifest_bytes,
            inventory_entries,
            inventory_bytes,
            negative_entries,
            negative_bytes,
            active_object_flights: self.packed_flights.lock().await.len(),
            active_pack_index_flights: self.pack_index_flights.lock().await.len(),
            active_generated_pack_flights: self.generated_pack_flights.lock().await.len(),
        }
    }

    /// Cancel and join every runtime-owned single-flight task.
    pub async fn shutdown(&self) {
        self.shutdown.cancel();
        self.tasks.close();
        self.tasks.wait().await;
    }

    pub(crate) fn metrics(&self) -> &dyn RemoteGitMetrics {
        self.metrics.as_ref()
    }

    pub(crate) fn background_cancellation(&self) -> CancellationToken {
        self.shutdown.clone()
    }

    pub(crate) fn operation_token(&self) -> TaskTrackerToken {
        self.tasks.token()
    }

    pub(crate) fn track_cleanup<F>(&self, cleanup: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.tasks.spawn(cleanup);
    }

    pub(crate) fn spawn_blocking<F, T>(&self, task: F) -> tokio::task::JoinHandle<T>
    where
        F: FnOnce() -> T + Send + 'static,
        T: Send + 'static,
    {
        self.tasks.spawn_blocking(task)
    }

    pub(crate) async fn origin_permit(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<OwnedSemaphorePermit> {
        tokio::select! {
            biased;
            () = cancellation.cancelled() => Err(Error::Cancelled),
            permit = Arc::clone(&self.origin).acquire_owned() => permit.map_err(|_| Error::Cancelled),
        }
    }

    pub(crate) async fn decode_permit(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<OwnedSemaphorePermit> {
        tokio::select! {
            biased;
            () = cancellation.cancelled() => Err(Error::Cancelled),
            permit = Arc::clone(&self.decode).acquire_owned() => permit.map_err(|_| Error::Cancelled),
        }
    }

    pub(crate) async fn cached_object(&self, key: &ObjectCacheKey) -> Option<Arc<GitObject>> {
        let value = self.object_cache.lock().await.get(key);
        let cache_event = if value.is_some() { "hit" } else { "miss" };
        tracing::debug!(
            cache_kind = "object",
            cache_event,
            "remote Git cache lookup"
        );
        self.metrics.record(MetricObservation {
            kind: MetricKind::Cache,
            value: u64::from(value.is_some()),
            duration: None,
            outcome: None,
            cache: Some(if value.is_some() {
                CacheOutcome::Hit
            } else {
                CacheOutcome::Miss
            }),
        });
        value
    }

    pub(crate) async fn remove_object(&self, key: &ObjectCacheKey) {
        self.object_cache.lock().await.remove(key);
    }

    pub(crate) async fn exact_miss_is_cached(&self, key: &ObjectCacheKey) -> bool {
        let now = Instant::now();
        let mut cache = self.negative_cache.lock().await;
        match cache.get(key) {
            Some(expires) if expires > now => true,
            Some(_) => {
                cache.remove(key);
                false
            }
            None => false,
        }
    }

    pub(crate) async fn cached_manifest(
        &self,
        identity: &RepositoryIdentity,
    ) -> Option<(Manifest, String)> {
        let cached = self.manifest_cache.lock().await.get(identity)?;
        (cached.expires > Instant::now()).then(|| (cached.manifest.clone(), cached.etag.clone()))
    }

    pub(crate) async fn insert_manifest(
        &self,
        identity: RepositoryIdentity,
        manifest: Manifest,
        etag: String,
    ) {
        let bytes = manifest_bytes(&manifest).saturating_add(etag.len());
        let cached = Arc::new(CachedManifest {
            manifest,
            etag,
            expires: Instant::now() + self.options.manifest_cache_ttl,
        });
        self.manifest_cache
            .lock()
            .await
            .insert(identity, cached, bytes);
    }

    pub(crate) async fn cached_inventory(
        &self,
        identity: &RepositoryIdentity,
        pack_index_hash: MerkleHash,
    ) -> Option<Arc<HashMap<MerkleHash, GitPackInventoryEntry>>> {
        self.inventory_cache.lock().await.get(&InventoryCacheKey {
            identity: identity.clone(),
            pack_index_hash,
        })
    }

    pub(crate) async fn insert_inventory(
        &self,
        identity: RepositoryIdentity,
        pack_index_hash: MerkleHash,
        inventory: Arc<HashMap<MerkleHash, GitPackInventoryEntry>>,
    ) {
        let bytes = std::mem::size_of::<HashMap<MerkleHash, GitPackInventoryEntry>>()
            .saturating_add(
                inventory.capacity().saturating_mul(
                    std::mem::size_of::<MerkleHash>()
                        .saturating_add(std::mem::size_of::<GitPackInventoryEntry>())
                        .saturating_add(1),
                ),
            );
        self.inventory_cache.lock().await.insert(
            InventoryCacheKey {
                identity,
                pack_index_hash,
            },
            inventory,
            bytes,
        );
    }

    pub(crate) async fn cached_commit(
        &self,
        key: &ObjectCacheKey,
        max_source_bytes: u64,
    ) -> Option<Arc<Commit>> {
        match self.parsed_cache.lock().await.get(key) {
            Some(ParsedObject::Commit {
                value,
                source_bytes,
            }) if source_bytes <= max_source_bytes => Some(value),
            _ => None,
        }
    }

    pub(crate) async fn cached_blame(
        &self,
        identity: &RepositoryIdentity,
        generation: u64,
        commit: ObjectId,
        path: &GitPath,
    ) -> Option<CachedBlame> {
        let key = BlameCacheKey {
            identity: identity.clone(),
            generation,
            commit,
            path: path.clone(),
        };
        let value = self.blame_cache.lock().await.get(&key);
        self.metrics.record(MetricObservation {
            kind: MetricKind::Cache,
            value: u64::from(value.is_some()),
            duration: None,
            outcome: None,
            cache: Some(if value.is_some() {
                CacheOutcome::Hit
            } else {
                CacheOutcome::Miss
            }),
        });
        value
    }

    pub(crate) async fn insert_blame(
        &self,
        identity: RepositoryIdentity,
        generation: u64,
        commit: ObjectId,
        path: GitPath,
        value: Arc<Blame>,
        usage: BudgetUsage,
    ) {
        let bytes = blame_bytes(&value);
        let evicted = self.blame_cache.lock().await.insert(
            BlameCacheKey {
                identity,
                generation,
                commit,
                path,
            },
            CachedBlame { value, usage },
            bytes,
        );
        if evicted > 0 {
            self.metrics.record(MetricObservation {
                kind: MetricKind::Cache,
                value: evicted as u64,
                duration: None,
                outcome: None,
                cache: Some(CacheOutcome::Eviction),
            });
        }
    }

    pub(crate) async fn insert_commit(
        &self,
        key: ObjectCacheKey,
        commit: Arc<Commit>,
        source_bytes: u64,
    ) {
        let bytes = commit_bytes(&commit);
        self.parsed_cache.lock().await.insert(
            key,
            ParsedObject::Commit {
                value: commit,
                source_bytes,
            },
            bytes,
        );
    }

    pub(crate) async fn cached_tag(
        &self,
        key: &ObjectCacheKey,
        max_source_bytes: u64,
    ) -> Option<Arc<AnnotatedTag>> {
        match self.parsed_cache.lock().await.get(key) {
            Some(ParsedObject::Tag {
                value,
                source_bytes,
            }) if source_bytes <= max_source_bytes => Some(value),
            _ => None,
        }
    }

    pub(crate) async fn insert_tag(
        &self,
        key: ObjectCacheKey,
        tag: Arc<AnnotatedTag>,
        source_bytes: u64,
    ) {
        let bytes = tag_bytes(&tag);
        self.parsed_cache.lock().await.insert(
            key,
            ParsedObject::Tag {
                value: tag,
                source_bytes,
            },
            bytes,
        );
    }

    pub(crate) async fn cached_tree(
        &self,
        key: &ObjectCacheKey,
        max_source_bytes: u64,
    ) -> Option<Arc<Vec<RawTreeEntry>>> {
        match self.parsed_cache.lock().await.get(key) {
            Some(ParsedObject::Tree {
                value,
                source_bytes,
            }) if source_bytes <= max_source_bytes => Some(value),
            _ => None,
        }
    }

    pub(crate) async fn insert_tree(
        &self,
        key: ObjectCacheKey,
        tree: Arc<Vec<RawTreeEntry>>,
        source_bytes: u64,
    ) {
        let bytes = tree_bytes(&tree);
        self.parsed_cache.lock().await.insert(
            key,
            ParsedObject::Tree {
                value: tree,
                source_bytes,
            },
            bytes,
        );
    }

    pub(crate) async fn cached_pack_index(
        &self,
        key: &PackIndexCacheKey,
        max_source_bytes: u64,
    ) -> Option<Arc<PackIndex>> {
        let value = self
            .pack_index_cache
            .lock()
            .await
            .get(key)
            .filter(|index| index.source_bytes <= max_source_bytes);
        let cache_event = if value.is_some() { "hit" } else { "miss" };
        tracing::debug!(
            cache_kind = "pack_index",
            cache_event,
            "remote Git cache lookup"
        );
        value
    }

    pub(crate) async fn insert_pack_index(&self, key: PackIndexCacheKey, index: Arc<PackIndex>) {
        let bytes = index.resident_bytes();
        let evicted = self.pack_index_cache.lock().await.insert(key, index, bytes);
        if evicted > 0 {
            self.metrics.record(MetricObservation {
                kind: MetricKind::Cache,
                value: evicted as u64,
                duration: None,
                outcome: None,
                cache: Some(CacheOutcome::Eviction),
            });
        }
    }

    pub(crate) async fn load_pack_index_singleflight<F, Fut>(
        self: &Arc<Self>,
        key: PackIndexCacheKey,
        max_source_bytes: u64,
        cancellation: &CancellationToken,
        work: F,
    ) -> Result<Arc<PackIndex>>
    where
        F: FnOnce(CancellationToken) -> Fut + Send + 'static,
        Fut: Future<Output = Result<PackIndex>> + Send + 'static,
    {
        let flight_key = PackIndexFlightKey {
            index: key.clone(),
            max_source_bytes,
        };
        let mut work = Some(work);
        let mut receiver = if let Some(receiver) = self
            .pack_index_flights
            .lock()
            .await
            .get(&flight_key)
            .cloned()
        {
            receiver
        } else {
            let admission = tokio::select! {
                biased;
                () = cancellation.cancelled() => return Err(Error::Cancelled),
                permit = Arc::clone(&self.pack_index_flight_admission).acquire_owned() => {
                    permit.map_err(|_| Error::Cancelled)?
                }
            };
            let mut flights = self.pack_index_flights.lock().await;
            if let Some(receiver) = flights.get(&flight_key) {
                drop(admission);
                receiver.clone()
            } else {
                let (sender, receiver) = watch::channel(None);
                flights.insert(flight_key.clone(), receiver.clone());
                let runtime = Arc::clone(self);
                let task_key = flight_key;
                let cache_key = key;
                let task_work = work.take().ok_or(Error::InternalInvariant {
                    invariant: "new pack-index flight has no work",
                })?;
                self.tasks.spawn(async move {
                    let _admission = admission;
                    let result = task_work(runtime.background_cancellation())
                        .await
                        .map(Arc::new)
                        .map_err(Arc::new);
                    if let Ok(index) = &result {
                        runtime
                            .insert_pack_index(cache_key, Arc::clone(index))
                            .await;
                    }
                    runtime.pack_index_flights.lock().await.remove(&task_key);
                    let _ = sender.send(Some(result));
                });
                receiver
            }
        };

        loop {
            let completed = receiver.borrow().clone();
            if let Some(result) = completed {
                drop(receiver);
                return match result {
                    Ok(index) => Ok(index),
                    Err(source) => match Arc::try_unwrap(source) {
                        Ok(error) => Err(error),
                        Err(source) => Err(Error::SharedRead { source }),
                    },
                };
            }
            tokio::select! {
                biased;
                () = cancellation.cancelled() => return Err(Error::Cancelled),
                changed = receiver.changed() => {
                    if changed.is_err() {
                        return Err(Error::InternalInvariant {
                            invariant: "pack-index flight ended without a result",
                        });
                    }
                }
            }
        }
    }

    pub(crate) async fn generate_pack_singleflight<F, Fut>(
        self: &Arc<Self>,
        key: GeneratedPackCacheKey,
        options: RepositoryOptions,
        cancellation: &CancellationToken,
        work: F,
    ) -> Result<Arc<GeneratedPack>>
    where
        F: FnOnce(CancellationToken) -> Fut + Send + 'static,
        Fut: Future<Output = Result<GeneratedPack>> + Send + 'static,
    {
        let flight_key = GeneratedPackFlightKey {
            request: key,
            options,
        };
        let mut work = Some(work);
        let mut coalesced = false;
        let mut receiver = if let Some(receiver) = self
            .generated_pack_flights
            .lock()
            .await
            .get(&flight_key)
            .cloned()
        {
            coalesced = true;
            receiver
        } else {
            let admission = tokio::select! {
                biased;
                () = cancellation.cancelled() => return Err(Error::Cancelled),
                permit = Arc::clone(&self.generated_pack_flight_admission).acquire_owned() => {
                    permit.map_err(|_| Error::Cancelled)?
                }
            };
            let mut flights = self.generated_pack_flights.lock().await;
            if let Some(receiver) = flights.get(&flight_key) {
                coalesced = true;
                drop(admission);
                receiver.clone()
            } else {
                let (sender, receiver) = watch::channel(None);
                flights.insert(flight_key.clone(), receiver.clone());
                let runtime = Arc::clone(self);
                let task_key = flight_key;
                let task_work = work.take().ok_or(Error::InternalInvariant {
                    invariant: "new generated-pack flight has no work",
                })?;
                self.tasks.spawn(async move {
                    let _admission = admission;
                    let result = task_work(runtime.background_cancellation())
                        .await
                        .map(Arc::new)
                        .map_err(Arc::new);
                    runtime
                        .generated_pack_flights
                        .lock()
                        .await
                        .remove(&task_key);
                    let _ = sender.send(Some(result));
                });
                receiver
            }
        };
        if coalesced {
            self.metrics.record(MetricObservation {
                kind: MetricKind::Cache,
                value: 1,
                duration: None,
                outcome: None,
                cache: Some(CacheOutcome::Coalesced),
            });
        }

        loop {
            let completed = receiver.borrow().clone();
            if let Some(result) = completed {
                drop(receiver);
                return match result {
                    Ok(pack) => Ok(pack),
                    Err(source) => match Arc::try_unwrap(source) {
                        Ok(error) => Err(error),
                        Err(source) => Err(Error::SharedRead { source }),
                    },
                };
            }
            tokio::select! {
                biased;
                () = cancellation.cancelled() => return Err(Error::Cancelled),
                changed = receiver.changed() => {
                    if changed.is_err() {
                        return Err(Error::InternalInvariant {
                            invariant: "generated-pack flight ended without a result",
                        });
                    }
                }
            }
        }
    }

    pub(crate) async fn read_packed_singleflight<F, Fut>(
        self: &Arc<Self>,
        key: ObjectCacheKey,
        max_inflated_bytes: u64,
        max_object_bytes: u64,
        cancellation: &CancellationToken,
        work: F,
    ) -> Result<Arc<PackedEntry>>
    where
        F: FnOnce(CancellationToken) -> Fut + Send + 'static,
        Fut: Future<Output = Result<PackedEntry>> + Send + 'static,
    {
        let flight_key = PackedFlightKey {
            object: key,
            max_inflated_bytes,
            max_object_bytes,
        };
        let mut work = Some(work);
        let mut receiver =
            if let Some(receiver) = self.packed_flights.lock().await.get(&flight_key).cloned() {
                receiver
            } else {
                let admission = tokio::select! {
                    biased;
                    () = cancellation.cancelled() => return Err(Error::Cancelled),
                    permit = Arc::clone(&self.object_flight_admission).acquire_owned() => {
                        permit.map_err(|_| Error::Cancelled)?
                    }
                };
                let mut flights = self.packed_flights.lock().await;
                if let Some(receiver) = flights.get(&flight_key) {
                    drop(admission);
                    receiver.clone()
                } else {
                    let (sender, receiver) = watch::channel(None);
                    flights.insert(flight_key.clone(), receiver.clone());
                    let runtime = Arc::clone(self);
                    let task_key = flight_key;
                    let task_work = work.take().ok_or(Error::InternalInvariant {
                        invariant: "new packed-entry flight has no work",
                    })?;
                    self.tasks.spawn(async move {
                        let _admission = admission;
                        let result = task_work(runtime.background_cancellation())
                            .await
                            .map(Arc::new)
                            .map_err(Arc::new);
                        runtime.packed_flights.lock().await.remove(&task_key);
                        let _ = sender.send(Some(result));
                    });
                    receiver
                }
            };

        loop {
            let completed = receiver.borrow().clone();
            if let Some(result) = completed {
                drop(receiver);
                return match result {
                    Ok(packed) => Ok(packed),
                    Err(source) => match Arc::try_unwrap(source) {
                        Ok(error) => Err(error),
                        Err(source) => Err(Error::SharedRead { source }),
                    },
                };
            }
            tokio::select! {
                biased;
                () = cancellation.cancelled() => return Err(Error::Cancelled),
                changed = receiver.changed() => {
                    if changed.is_err() {
                        return Err(Error::InternalInvariant {
                            invariant: "packed-entry flight ended without a result",
                        });
                    }
                }
            }
        }
    }

    pub(crate) async fn insert_object(&self, key: ObjectCacheKey, object: Arc<GitObject>) {
        let bytes = object
            .data
            .len()
            .saturating_add(std::mem::size_of::<GitObject>());
        let evicted = self.object_cache.lock().await.insert(key, object, bytes);
        if evicted > 0 {
            self.metrics.record(MetricObservation {
                kind: MetricKind::Cache,
                value: evicted as u64,
                duration: None,
                outcome: None,
                cache: Some(CacheOutcome::Eviction),
            });
        }
    }

    pub(crate) async fn insert_exact_miss(&self, key: ObjectCacheKey) {
        let bytes =
            std::mem::size_of::<ObjectCacheKey>().saturating_add(std::mem::size_of::<Instant>());
        self.negative_cache.lock().await.insert(
            key,
            Instant::now() + self.options.negative_cache_ttl,
            bytes,
        );
    }
}

fn commit_bytes(commit: &Commit) -> usize {
    std::mem::size_of::<Commit>()
        .saturating_add(
            commit
                .parents
                .capacity()
                .saturating_mul(std::mem::size_of::<ObjectId>()),
        )
        .saturating_add(commit.author.name.len())
        .saturating_add(commit.author.email.len())
        .saturating_add(commit.committer.name.len())
        .saturating_add(commit.committer.email.len())
        .saturating_add(commit.encoding.as_ref().map_or(0, bytes::Bytes::len))
        .saturating_add(commit.message.len())
        .saturating_add(
            commit
                .signature_headers
                .iter()
                .map(|header| {
                    std::mem::size_of_val(header)
                        .saturating_add(header.name.len())
                        .saturating_add(header.value.len())
                })
                .sum::<usize>(),
        )
}

fn blame_bytes(blame: &Blame) -> usize {
    std::mem::size_of::<Blame>()
        .saturating_add(blame.path.as_bytes().len())
        .saturating_add(
            blame
                .ranges
                .capacity()
                .saturating_mul(std::mem::size_of::<crate::BlameRange>()),
        )
        .saturating_add(
            blame
                .ranges
                .iter()
                .map(|range| {
                    range
                        .source_path
                        .as_bytes()
                        .len()
                        .saturating_add(commit_bytes(&range.commit))
                })
                .sum::<usize>(),
        )
}

fn manifest_bytes(manifest: &Manifest) -> usize {
    let optional = |value: &Option<String>| value.as_ref().map_or(0, String::len);
    std::mem::size_of::<Manifest>()
        .saturating_add(manifest.created_at.len())
        .saturating_add(optional(&manifest.pusher))
        .saturating_add(manifest.session_id.len())
        .saturating_add(manifest.head.len())
        .saturating_add(manifest.shard_index_hash.len())
        .saturating_add(manifest.pack_index_hash.len())
        .saturating_add(manifest.git_validation_digest.len())
        .saturating_add(optional(&manifest.commit_graph_hash))
        .saturating_add(optional(&manifest.ref_registry_hash))
        .saturating_add(
            manifest
                .refs
                .iter()
                .chain(&manifest.peeled_refs)
                .map(|(name, oid)| {
                    std::mem::size_of::<(String, String)>()
                        .saturating_add(name.len())
                        .saturating_add(oid.len())
                })
                .sum::<usize>(),
        )
}

fn tag_bytes(tag: &AnnotatedTag) -> usize {
    std::mem::size_of::<AnnotatedTag>()
        .saturating_add(tag.name.len())
        .saturating_add(tag.message.len())
        .saturating_add(tag.signature.as_ref().map_or(0, bytes::Bytes::len))
        .saturating_add(tag.tagger.as_ref().map_or(0, |tagger| {
            tagger.name.len().saturating_add(tagger.email.len())
        }))
}

fn tree_bytes(tree: &[RawTreeEntry]) -> usize {
    std::mem::size_of::<Vec<RawTreeEntry>>()
        .saturating_add(
            tree.len()
                .saturating_mul(std::mem::size_of::<RawTreeEntry>()),
        )
        .saturating_add(tree.iter().map(|entry| entry.name.len()).sum::<usize>())
}

impl Default for RemoteGitRuntime {
    fn default() -> Self {
        Self::from_validated(RuntimeOptions::default(), Arc::new(NoopMetrics))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use bytes::Bytes;
    use gix_pack::data::entry::Header;

    use super::*;

    #[test]
    fn runtime_rejects_zero_admission_and_zero_enabled_negative_ttl() {
        for options in [
            RuntimeOptions {
                max_origin_concurrency: 0,
                ..RuntimeOptions::default()
            },
            RuntimeOptions {
                max_decode_concurrency: 0,
                ..RuntimeOptions::default()
            },
            RuntimeOptions {
                negative_cache_ttl: Duration::ZERO,
                ..RuntimeOptions::default()
            },
            RuntimeOptions {
                manifest_cache_ttl: Duration::ZERO,
                ..RuntimeOptions::default()
            },
        ] {
            assert!(RemoteGitRuntime::new(options, Arc::new(NoopMetrics)).is_err());
        }
    }

    #[test]
    fn zero_cache_bounds_are_a_supported_disabled_mode() {
        let options = RuntimeOptions {
            max_object_cache_entries: 0,
            max_object_cache_bytes: 0,
            max_negative_cache_entries: 0,
            max_negative_cache_bytes: 0,
            negative_cache_ttl: Duration::ZERO,
            ..RuntimeOptions::default()
        };
        assert!(RemoteGitRuntime::new(options, Arc::new(NoopMetrics)).is_ok());
    }

    #[tokio::test]
    async fn object_cache_isolates_provider_repository_and_placement_generation() {
        let runtime = RemoteGitRuntime::default();
        let oid = ObjectId::empty_blob(gix_hash::Kind::Sha1);
        let object = Arc::new(GitObject {
            oid,
            kind: gix_object::Kind::Blob,
            data: Bytes::new(),
        });
        let source =
            RepositoryIdentity::new("provider-a", "repository-a", 1).expect("source identity");
        let other_provider =
            RepositoryIdentity::new("provider-b", "repository-a", 1).expect("provider identity");
        let other_repository =
            RepositoryIdentity::new("provider-a", "repository-b", 1).expect("repository identity");
        let other_placement =
            RepositoryIdentity::new("provider-a", "repository-a", 2).expect("placement identity");
        runtime
            .insert_object(ObjectCacheKey::new(&source, 7, oid), object)
            .await;
        assert!(
            runtime
                .cached_object(&ObjectCacheKey::new(&source, 7, oid))
                .await
                .is_some()
        );
        for identity in [other_provider, other_repository, other_placement] {
            assert!(
                runtime
                    .cached_object(&ObjectCacheKey::new(&identity, 7, oid))
                    .await
                    .is_none()
            );
        }
        assert!(
            runtime
                .cached_object(&ObjectCacheKey::new(&source, 8, oid))
                .await
                .is_none()
        );
    }

    #[tokio::test]
    async fn disabled_object_cache_never_retains_verified_bytes() {
        let options = RuntimeOptions {
            max_object_cache_entries: 0,
            max_object_cache_bytes: 0,
            ..RuntimeOptions::default()
        };
        let runtime = RemoteGitRuntime::new(options, Arc::new(NoopMetrics)).expect("runtime");
        let identity = RepositoryIdentity::new("provider", "repository", 1).expect("identity");
        let oid = ObjectId::empty_blob(gix_hash::Kind::Sha1);
        let key = ObjectCacheKey::new(&identity, 1, oid);
        runtime
            .insert_object(
                key.clone(),
                Arc::new(GitObject {
                    oid,
                    kind: gix_object::Kind::Blob,
                    data: Bytes::new(),
                }),
            )
            .await;
        assert!(runtime.cached_object(&key).await.is_none());
    }

    #[tokio::test]
    async fn parsed_cache_does_not_bypass_a_stricter_object_limit() {
        let runtime = RemoteGitRuntime::default();
        let identity = RepositoryIdentity::new("provider", "repository", 1).expect("identity");
        let oid = ObjectId::empty_tree(gix_hash::Kind::Sha1);
        let key = ObjectCacheKey::new(&identity, 1, oid);
        runtime
            .insert_tree(key.clone(), Arc::new(Vec::new()), 10)
            .await;

        assert!(runtime.cached_tree(&key, 9).await.is_none());
        assert!(runtime.cached_tree(&key, 10).await.is_some());
    }

    #[tokio::test]
    async fn inventory_cache_is_content_hash_and_repository_scoped() {
        let runtime = RemoteGitRuntime::default();
        let identity = RepositoryIdentity::new("provider", "repository", 1).expect("identity");
        let other = RepositoryIdentity::new("provider", "other", 1).expect("identity");
        let hash = crab_xet::hash::compute_data_hash(b"inventory-one");
        let other_hash = crab_xet::hash::compute_data_hash(b"inventory-two");
        runtime
            .insert_inventory(identity.clone(), hash, Arc::new(HashMap::new()))
            .await;

        assert!(runtime.cached_inventory(&identity, hash).await.is_some());
        assert!(
            runtime
                .cached_inventory(&identity, other_hash)
                .await
                .is_none()
        );
        assert!(runtime.cached_inventory(&other, hash).await.is_none());
    }

    #[tokio::test]
    async fn pack_index_cache_retains_multiple_packs_and_evicts_by_lru_order() {
        let options = RuntimeOptions {
            max_pack_index_cache_entries: 2,
            max_pack_index_cache_bytes: 4_096,
            ..RuntimeOptions::default()
        };
        let runtime = RemoteGitRuntime::new(options, Arc::new(NoopMetrics)).expect("runtime");
        let identity = RepositoryIdentity::new("provider", "repository", 1).expect("identity");
        let pack_ids = [
            crab_xet::hash::compute_data_hash(b"pack-one"),
            crab_xet::hash::compute_data_hash(b"pack-two"),
            crab_xet::hash::compute_data_hash(b"pack-three"),
        ];
        let keys = pack_ids.map(|pack_id| PackIndexCacheKey::new(&identity, pack_id));
        for key in keys.iter().take(2) {
            runtime
                .insert_pack_index(
                    key.clone(),
                    Arc::new(PackIndex {
                        object_ids: Vec::new(),
                        pack_offsets: Vec::new(),
                        crc32: Vec::new(),
                        offset_order: Vec::new(),
                        pack_data_end: 0,
                        pack_checksum: [0; 20],
                        source_bytes: 32,
                    }),
                )
                .await;
        }
        assert!(runtime.cached_pack_index(&keys[0], 32).await.is_some());
        assert!(runtime.cached_pack_index(&keys[1], 32).await.is_some());
        runtime
            .insert_pack_index(
                keys[2].clone(),
                Arc::new(PackIndex {
                    object_ids: Vec::new(),
                    pack_offsets: Vec::new(),
                    crc32: Vec::new(),
                    offset_order: Vec::new(),
                    pack_data_end: 0,
                    pack_checksum: [0; 20],
                    source_bytes: 32,
                }),
            )
            .await;

        assert!(runtime.cached_pack_index(&keys[0], 32).await.is_none());
        assert!(runtime.cached_pack_index(&keys[1], 32).await.is_some());
        assert!(runtime.cached_pack_index(&keys[2], 32).await.is_some());
    }

    #[tokio::test]
    async fn negative_cache_is_generation_scoped_and_expires() {
        let options = RuntimeOptions {
            negative_cache_ttl: Duration::from_millis(1),
            ..RuntimeOptions::default()
        };
        let runtime = RemoteGitRuntime::new(options, Arc::new(NoopMetrics)).expect("runtime");
        let identity = RepositoryIdentity::new("provider", "repository", 1).expect("identity");
        let oid = ObjectId::empty_blob(gix_hash::Kind::Sha1);
        let first = ObjectCacheKey::new(&identity, 1, oid);
        let next = ObjectCacheKey::new(&identity, 2, oid);
        runtime.insert_exact_miss(first.clone()).await;

        assert!(runtime.exact_miss_is_cached(&first).await);
        assert!(!runtime.exact_miss_is_cached(&next).await);
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert!(!runtime.exact_miss_is_cached(&first).await);
    }

    #[tokio::test]
    async fn packed_singleflight_separates_incompatible_decode_limits() {
        let runtime = Arc::new(RemoteGitRuntime::default());
        let identity = RepositoryIdentity::new("provider", "repository", 1).expect("identity");
        let key = ObjectCacheKey::new(&identity, 1, ObjectId::empty_blob(gix_hash::Kind::Sha1));
        let starts = Arc::new(AtomicUsize::new(0));
        let first_starts = Arc::clone(&starts);
        let first_cancellation = CancellationToken::new();
        let first = runtime.read_packed_singleflight(
            key.clone(),
            1,
            1,
            &first_cancellation,
            move |_| async move {
                first_starts.fetch_add(1, Ordering::SeqCst);
                tokio::task::yield_now().await;
                Ok(PackedEntry {
                    header: Header::Blob,
                    inflated: Bytes::new(),
                    charged_budget: None,
                })
            },
        );
        let second_starts = Arc::clone(&starts);
        let second_cancellation = CancellationToken::new();
        let second = runtime.read_packed_singleflight(
            key,
            2,
            2,
            &second_cancellation,
            move |_| async move {
                second_starts.fetch_add(1, Ordering::SeqCst);
                tokio::task::yield_now().await;
                Ok(PackedEntry {
                    header: Header::Blob,
                    inflated: Bytes::new(),
                    charged_budget: None,
                })
            },
        );

        let (first, second) = tokio::join!(first, second);
        assert!(first.is_ok());
        assert!(second.is_ok());
        assert_eq!(starts.load(Ordering::SeqCst), 2);
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn distinct_object_flights_wait_at_the_configured_admission_bound() {
        let options = RuntimeOptions {
            max_object_flights: 1,
            ..RuntimeOptions::default()
        };
        let runtime = Arc::new(
            RemoteGitRuntime::new(options, Arc::new(NoopMetrics)).expect("bounded runtime"),
        );
        let identity = RepositoryIdentity::new("provider", "repository", 1).expect("identity");
        let first_key =
            ObjectCacheKey::new(&identity, 1, ObjectId::empty_blob(gix_hash::Kind::Sha1));
        let second_key =
            ObjectCacheKey::new(&identity, 1, ObjectId::empty_tree(gix_hash::Kind::Sha1));
        let started = Arc::new(AtomicUsize::new(0));
        let gate = Arc::new(Semaphore::new(0));
        let first_runtime = Arc::clone(&runtime);
        let first_started = Arc::clone(&started);
        let first_gate = Arc::clone(&gate);
        let first = tokio::spawn(async move {
            first_runtime
                .read_packed_singleflight(
                    first_key,
                    1,
                    1,
                    &CancellationToken::new(),
                    move |_| async move {
                        first_started.fetch_add(1, Ordering::SeqCst);
                        let permit = first_gate.acquire().await.expect("gate open");
                        permit.forget();
                        Ok(PackedEntry {
                            header: Header::Blob,
                            inflated: Bytes::new(),
                            charged_budget: None,
                        })
                    },
                )
                .await
        });
        while started.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        let second_runtime = Arc::clone(&runtime);
        let second_started = Arc::clone(&started);
        let second = tokio::spawn(async move {
            second_runtime
                .read_packed_singleflight(
                    second_key,
                    1,
                    1,
                    &CancellationToken::new(),
                    move |_| async move {
                        second_started.fetch_add(1, Ordering::SeqCst);
                        Ok(PackedEntry {
                            header: Header::Tree,
                            inflated: Bytes::new(),
                            charged_budget: None,
                        })
                    },
                )
                .await
        });
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert_eq!(started.load(Ordering::SeqCst), 1);
        gate.add_permits(1);
        assert!(first.await.expect("first joins").is_ok());
        assert!(second.await.expect("second joins").is_ok());
        assert_eq!(started.load(Ordering::SeqCst), 2);
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_cancels_and_joins_tracked_blocking_work() {
        let runtime = RemoteGitRuntime::default();
        let cancellation = runtime.background_cancellation();
        let finished = Arc::new(AtomicBool::new(false));
        let worker_finished = Arc::clone(&finished);
        let (entered_sender, entered_receiver) = tokio::sync::oneshot::channel();
        let worker = runtime.spawn_blocking(move || {
            let _ = entered_sender.send(());
            while !cancellation.is_cancelled() {
                std::thread::yield_now();
            }
            worker_finished.store(true, Ordering::SeqCst);
        });
        entered_receiver.await.expect("worker starts");

        tokio::time::timeout(Duration::from_secs(1), runtime.shutdown())
            .await
            .expect("shutdown joins blocking work");
        assert!(finished.load(Ordering::SeqCst));
        worker.await.expect("blocking worker joins without panic");
    }
}
