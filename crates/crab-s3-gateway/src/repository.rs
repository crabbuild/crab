use std::{
    collections::HashMap,
    hash::Hash,
    sync::{
        Arc, Mutex as StdMutex, MutexGuard,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::Duration,
};

use crab_remote_git::{
    OperationContext, RemoteGitRepository, RemoteGitRuntime, RemoteGitSnapshot, RepositoryOptions,
    Revision,
};
use crab_storage::{Store, StoreLayout};
use gix_hash::ObjectId;
use tokio::sync::{Mutex, OnceCell, RwLock};
use tokio_util::sync::CancellationToken;

use crate::gateway::Repository;

const MAINTENANCE_TTL: Duration = Duration::from_secs(60);
const FULL_MAINTENANCE_IDLE_DELAY: Duration = Duration::from_secs(5);
const CATALOG_MAINTENANCE_EPOCHS: u64 = 64;
const MAX_CACHED_SNAPSHOTS: usize = 64;
const MAX_CACHED_MANIFESTS: usize = 16;
const MAX_CACHED_OBJECT_ATTRIBUTES: usize = 256;
const MAX_CAPSULE_READ_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const CAPSULE_CHECKPOINT_THRESHOLD: u32 = 32;

#[derive(Debug, thiserror::Error)]
enum MaintenanceError {
    #[error("repository protocol detection failed")]
    Metadata(#[from] crab_metadata::error::MetadataError),
    #[error("legacy repository maintenance failed")]
    Legacy(#[from] crab_write::WriteError),
    #[error("capsule repository maintenance failed")]
    Capsule(#[from] crab_remote::checkpoint::CheckpointError),
}

async fn is_capsule_repository(repository: &StoreLayout<Store>) -> Result<bool, MaintenanceError> {
    match crab_metadata::capsule_protocol::load_root(repository).await {
        Ok(_) => Ok(true),
        Err(crab_metadata::error::MetadataError::Storage {
            source: crab_storage::StorageError::NotFound { .. },
        }) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReadViewKey {
    generation: u64,
    snapshot_digest: String,
}

struct CachedReadView {
    observed_at: tokio::time::Instant,
    view: Arc<ReadView>,
}

type ObjectAttributeKey = (ObjectId, String, ObjectId);
type ObjectAttributeCell = Arc<OnceCell<Option<crate::attributes::ObjectAttributes>>>;

/// Generation- and journal-keyed immutable repository state shared by requests.
pub(crate) struct ReadView {
    key: ReadViewKey,
    remote: RemoteGitRepository,
    capsule_ref_counts: Option<HashMap<String, u32>>,
    snapshots: Mutex<HashMap<String, Arc<OnceCell<RemoteGitSnapshot>>>>,
    manifests: Mutex<HashMap<ObjectId, Arc<OnceCell<Arc<crate::attributes::Manifest>>>>>,
    objects: Mutex<HashMap<ObjectAttributeKey, ObjectAttributeCell>>,
}

impl ReadView {
    pub(crate) fn remote(&self) -> &RemoteGitRepository {
        &self.remote
    }

    pub(crate) fn capsule_ref_count(&self, ref_name: &str) -> Option<u32> {
        self.capsule_ref_counts
            .as_ref()
            .map(|counts| counts.get(ref_name).copied().unwrap_or_default())
    }

    pub(crate) async fn snapshot(
        &self,
        revision: &str,
        operation: &OperationContext,
    ) -> crate::Result<RemoteGitSnapshot> {
        let cell = {
            let mut snapshots = self.snapshots.lock().await;
            bounded_cell(&mut snapshots, revision.to_owned(), MAX_CACHED_SNAPSHOTS)
        };
        cell.get_or_try_init(|| async {
            self.remote
                .snapshot(&Revision::parse(revision)?, operation)
                .await
                .map_err(Into::into)
        })
        .await
        .cloned()
    }

    pub(crate) async fn attributes(
        &self,
        repository: &Repository,
        commit: ObjectId,
    ) -> crate::Result<Arc<crate::attributes::Manifest>> {
        let cell = {
            let mut manifests = self.manifests.lock().await;
            bounded_cell(&mut manifests, commit, MAX_CACHED_MANIFESTS)
        };
        cell.get_or_try_init(|| async {
            crate::attributes::load(repository, commit)
                .await
                .map(Arc::new)
        })
        .await
        .cloned()
    }

    pub(crate) async fn object_attributes(
        &self,
        repository: &Repository,
        commit: ObjectId,
        path: &str,
        oid: ObjectId,
    ) -> crate::Result<Option<crate::attributes::ObjectAttributes>> {
        let key = (commit, path.to_owned(), oid);
        let cell = {
            let mut objects = self.objects.lock().await;
            bounded_cell(&mut objects, key, MAX_CACHED_OBJECT_ATTRIBUTES)
        };
        cell.get_or_try_init(|| crate::attributes::load_object(repository, commit, path, oid))
            .await
            .cloned()
    }
}

fn bounded_cell<K, V>(
    cache: &mut HashMap<K, Arc<OnceCell<V>>>,
    key: K,
    capacity: usize,
) -> Arc<OnceCell<V>>
where
    K: Eq + Hash,
{
    if !cache.contains_key(&key) && cache.len() >= capacity {
        cache.clear();
    }
    Arc::clone(
        cache
            .entry(key)
            .or_insert_with(|| Arc::new(OnceCell::new())),
    )
}

/// Singleflight refresh and immutable generation reuse for one repository.
pub(crate) struct ReadViewCache {
    cached: RwLock<Option<CachedReadView>>,
    refresh: Mutex<()>,
}

/// Coalesces repository maintenance until the local write burst is idle.
pub(crate) struct WriteMaintenance {
    epoch: AtomicU64,
    active: AtomicUsize,
    next_catalog_maintenance_epoch: AtomicU64,
    catalog_maintenance_running: std::sync::atomic::AtomicBool,
    state: StdMutex<MaintenanceState>,
}

struct MaintenanceState {
    scheduled: CancellationToken,
    running: bool,
}

impl WriteMaintenance {
    pub(crate) fn new() -> Self {
        Self {
            epoch: AtomicU64::new(0),
            active: AtomicUsize::new(0),
            next_catalog_maintenance_epoch: AtomicU64::new(CATALOG_MAINTENANCE_EPOCHS),
            catalog_maintenance_running: std::sync::atomic::AtomicBool::new(false),
            state: StdMutex::new(MaintenanceState {
                scheduled: CancellationToken::new(),
                running: false,
            }),
        }
    }

    pub(crate) fn begin(&self) {
        self.epoch.fetch_add(1, Ordering::AcqRel);
        self.active.fetch_add(1, Ordering::AcqRel);
        self.state().scheduled.cancel();
    }

    pub(crate) fn finish(&self, parent: &CancellationToken) -> Option<(u64, CancellationToken)> {
        if self.active.fetch_sub(1, Ordering::AcqRel) != 1 {
            return None;
        }
        let epoch = self.epoch.load(Ordering::Acquire);
        if !self.is_idle_at(epoch) {
            return None;
        }
        let token = parent.child_token();
        self.state().scheduled = token.clone();
        Some((epoch, token))
    }

    fn start(&self, epoch: u64) -> bool {
        let mut state = self.state();
        if state.running || state.scheduled.is_cancelled() || !self.is_idle_at(epoch) {
            return false;
        }
        state.running = true;
        state.scheduled = CancellationToken::new();
        true
    }

    fn complete_pass(&self, epoch: u64) -> Option<u64> {
        let mut state = self.state();
        let next = self.epoch.load(Ordering::Acquire);
        if next != epoch && self.is_idle_at(next) {
            state.scheduled.cancel();
            state.scheduled = CancellationToken::new();
            return Some(next);
        }
        state.running = false;
        None
    }

    fn stop(&self) {
        self.state().running = false;
    }

    fn claim_catalog_maintenance(&self, epoch: u64) -> bool {
        if epoch < self.next_catalog_maintenance_epoch.load(Ordering::Acquire)
            || self
                .catalog_maintenance_running
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
        {
            return false;
        }
        self.next_catalog_maintenance_epoch.store(
            epoch.saturating_add(CATALOG_MAINTENANCE_EPOCHS),
            Ordering::Release,
        );
        true
    }

    fn finish_catalog_maintenance(&self) {
        self.catalog_maintenance_running
            .store(false, Ordering::Release);
    }

    pub(crate) fn current_epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(crate) fn catalog_maintenance_is_running(&self) -> bool {
        self.catalog_maintenance_running.load(Ordering::Acquire)
    }

    fn is_idle_at(&self, epoch: u64) -> bool {
        self.active.load(Ordering::Acquire) == 0 && self.epoch.load(Ordering::Acquire) == epoch
    }

    fn state(&self) -> MutexGuard<'_, MaintenanceState> {
        match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

pub(crate) fn schedule_catalog_maintenance(
    repository: &Repository,
    cancel: &CancellationToken,
    epoch: u64,
) {
    if !repository.maintenance.claim_catalog_maintenance(epoch) {
        return;
    }
    let store = repository.store.clone();
    let layout = repository.layout.clone();
    let maintenance = Arc::clone(&repository.maintenance);
    let cancel = cancel.child_token();
    tokio::spawn(async move {
        match is_capsule_repository(&layout).await {
            Ok(true) => {
                maintenance.finish_catalog_maintenance();
                return;
            }
            Ok(false) => {}
            Err(error) => {
                maintenance.finish_catalog_maintenance();
                tracing::warn!(%error, "S3 repository protocol detection failed");
                return;
            }
        }
        let result = crab_write::generation::ensure_catalog_readable(
            &store,
            &layout,
            MAINTENANCE_TTL,
            &cancel,
        )
        .await;
        maintenance.finish_catalog_maintenance();
        match result {
            Ok(_) => {}
            Err(crab_write::WriteError::Cancelled) if cancel.is_cancelled() => {}
            Err(error) => tracing::warn!(%error, "S3 bounded catalog maintenance failed"),
        }
    });
}

impl ReadViewCache {
    pub(crate) fn new() -> Self {
        Self {
            cached: RwLock::new(None),
            refresh: Mutex::new(()),
        }
    }

    pub(crate) async fn current(
        &self,
        repository: &Repository,
        runtime: Arc<RemoteGitRuntime>,
        options: RepositoryOptions,
        cancel: &CancellationToken,
    ) -> crate::Result<Arc<ReadView>> {
        let requested_at = tokio::time::Instant::now();
        let _refresh = self.refresh.lock().await;
        if let Some(view) = self.observed_since(requested_at).await {
            return Ok(view);
        }
        match crab_metadata::capsule_protocol::load_root(&repository.layout).await {
            Ok(root) => {
                let capsule = crab_read::capsule_protocol::open_view_from_root(
                    &repository.layout,
                    root,
                    crab_read::capsule_protocol::CapsuleReadLimits {
                        max_capsule_bytes: MAX_CAPSULE_READ_BYTES,
                        max_frontier_bytes: MAX_CAPSULE_READ_BYTES,
                    },
                )
                .await?;
                let observed_at = tokio::time::Instant::now();
                let key = ReadViewKey {
                    generation: capsule.root().root().generation(),
                    snapshot_digest: capsule.state_digest(),
                };
                let existing = self
                    .cached
                    .read()
                    .await
                    .as_ref()
                    .filter(|cached| cached.view.key == key)
                    .map(|cached| Arc::clone(&cached.view));
                let view = match existing {
                    Some(view) => view,
                    None => {
                        let capsule_ref_counts = capsule
                            .refs()
                            .keys()
                            .map(|name| (name.clone(), capsule.ref_capsule_count(name)))
                            .collect();
                        let remote = capsule
                            .git_repository(
                                repository.identity.clone(),
                                runtime,
                                options,
                                MAX_CAPSULE_READ_BYTES,
                                cancel,
                            )
                            .await?;
                        Arc::new(ReadView {
                            key,
                            remote,
                            capsule_ref_counts: Some(capsule_ref_counts),
                            snapshots: Mutex::new(HashMap::new()),
                            manifests: Mutex::new(HashMap::new()),
                            objects: Mutex::new(HashMap::new()),
                        })
                    }
                };
                *self.cached.write().await = Some(CachedReadView {
                    observed_at,
                    view: Arc::clone(&view),
                });
                return Ok(view);
            }
            Err(crab_metadata::error::MetadataError::Storage {
                source: crab_storage::StorageError::NotFound { .. },
            }) => {}
            Err(error) => return Err(error.into()),
        }
        let mut snapshot = crab_metadata::manifest_store::read_repository_snapshot(
            &repository.store,
            &repository.layout,
        )
        .await?;
        let observed_at = tokio::time::Instant::now();
        let mut key = ReadViewKey {
            generation: snapshot.manifest.generation,
            snapshot_digest: snapshot.digest()?,
        };
        let existing = self
            .cached
            .read()
            .await
            .as_ref()
            .filter(|cached| cached.view.key == key)
            .map(|cached| Arc::clone(&cached.view));
        let view = match existing {
            Some(view) => view,
            None => {
                let remote = if snapshot.journal.transactions.is_empty() {
                    match RemoteGitRepository::open(
                        repository.store.clone(),
                        repository.layout.clone(),
                        repository.identity.clone(),
                        Arc::clone(&runtime),
                        options,
                        cancel,
                    )
                    .await
                    {
                        Ok(remote) => remote,
                        Err(crab_remote_git::Error::RepositoryIndexing { .. }) => {
                            snapshot = crab_metadata::manifest_store::read_repository_snapshot(
                                &repository.store,
                                &repository.layout,
                            )
                            .await?;
                            key = ReadViewKey {
                                generation: snapshot.manifest.generation,
                                snapshot_digest: snapshot.digest()?,
                            };
                            RemoteGitRepository::from_snapshot_with_catalog_tail(
                                repository.layout.clone(),
                                &snapshot,
                                repository.identity.clone(),
                                runtime,
                                options,
                                cancel,
                            )
                            .await?
                        }
                        Err(error) => return Err(error.into()),
                    }
                } else {
                    RemoteGitRepository::from_snapshot_with_catalog_tail(
                        repository.layout.clone(),
                        &snapshot,
                        repository.identity.clone(),
                        runtime,
                        options,
                        cancel,
                    )
                    .await?
                };
                Arc::new(ReadView {
                    key,
                    remote,
                    capsule_ref_counts: None,
                    snapshots: Mutex::new(HashMap::new()),
                    manifests: Mutex::new(HashMap::new()),
                    objects: Mutex::new(HashMap::new()),
                })
            }
        };
        *self.cached.write().await = Some(CachedReadView {
            observed_at,
            view: Arc::clone(&view),
        });
        Ok(view)
    }

    pub(crate) async fn invalidate(&self) {
        *self.cached.write().await = None;
    }

    async fn observed_since(&self, requested_at: tokio::time::Instant) -> Option<Arc<ReadView>> {
        self.cached
            .read()
            .await
            .as_ref()
            .filter(|cached| cached.observed_at >= requested_at)
            .map(|cached| Arc::clone(&cached.view))
    }
}

pub(crate) fn schedule_readability(
    repository: &Repository,
    runtime: Arc<RemoteGitRuntime>,
    options: RepositoryOptions,
    cancel: CancellationToken,
    epoch: u64,
) {
    let store = repository.store.clone();
    let layout = repository.layout.clone();
    let identity = repository.identity.clone();
    let maintenance = Arc::clone(&repository.maintenance);
    tokio::spawn(async move {
        let mut epoch = epoch;
        if !wait_for_idle(&maintenance, &cancel, epoch).await || !maintenance.start(epoch) {
            return;
        }
        loop {
            let result = match is_capsule_repository(&layout).await {
                Ok(true) => crab_remote::checkpoint::publish_capsule_checkpoint(
                    &layout,
                    CAPSULE_CHECKPOINT_THRESHOLD,
                    MAX_CAPSULE_READ_BYTES,
                    &cancel,
                )
                .await
                .map(|_| ())
                .map_err(MaintenanceError::from),
                Ok(false) => crab_write::generation::ensure_readable(
                    &store,
                    &layout,
                    &identity,
                    Arc::clone(&runtime),
                    options,
                    MAINTENANCE_TTL,
                    &cancel,
                )
                .await
                .map(|_| ())
                .map_err(MaintenanceError::from),
                Err(error) => Err(error),
            };
            if let Err(error) = result {
                maintenance.stop();
                match error {
                    MaintenanceError::Legacy(crab_write::WriteError::VisibilityUnavailable {
                        generation,
                    }) => {
                        tracing::warn!(
                            generation,
                            recovery = "run `crab fsck --repair`, then `crab metadb owner --once`, against this repository",
                            "S3 repository requires verified Git visibility repair"
                        );
                    }
                    MaintenanceError::Legacy(crab_write::WriteError::Cancelled)
                    | MaintenanceError::Capsule(
                        crab_remote::checkpoint::CheckpointError::Cancelled,
                    ) if cancel.is_cancelled() => {}
                    error => {
                        tracing::warn!(%error, "S3 repository background read maintenance failed");
                    }
                }
                return;
            }
            let Some(next) = maintenance.complete_pass(epoch) else {
                return;
            };
            epoch = next;
            if !wait_for_idle(&maintenance, &cancel, epoch).await {
                maintenance.stop();
                return;
            }
        }
    });
}

async fn wait_for_idle(
    maintenance: &WriteMaintenance,
    cancel: &CancellationToken,
    epoch: u64,
) -> bool {
    // Let a short write burst accumulate in the journal so one owner can
    // compact it, instead of racing every acknowledgement with maintenance.
    tokio::select! {
        () = cancel.cancelled() => false,
        () = tokio::time::sleep(FULL_MAINTENANCE_IDLE_DELAY) => maintenance.is_idle_at(epoch),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RepositoryAccess, RepositoryConfig};

    fn repository_config(prefix: &str) -> RepositoryConfig {
        RepositoryConfig {
            name: "repo".to_owned(),
            provider: crab_storage::StorageProviderKind::Local,
            bucket: "memory".to_owned(),
            prefix: prefix.to_owned(),
            default_branch: "main".to_owned(),
            members: vec![crate::RepositoryMember {
                principal: "user".to_owned(),
                access: RepositoryAccess::Read,
            }],
            protected_branches: Vec::new(),
            git_blob_max_bytes: 1024 * 1024,
            max_active_multipart_uploads: 16,
            multipart_staging_bytes_per_upload: 50_000_000_000_000,
            multipart_upload_ttl_seconds: 604_800,
        }
    }

    #[test]
    fn foreground_write_cancels_only_maintenance_that_has_not_started() {
        let parent = CancellationToken::new();
        let maintenance = WriteMaintenance::new();
        maintenance.begin();
        let (first_epoch, first) = maintenance
            .finish(&parent)
            .expect("idle repository schedules maintenance");

        maintenance.begin();
        assert!(first.is_cancelled());
        let (second_epoch, second) = maintenance
            .finish(&parent)
            .expect("new idle epoch schedules replacement maintenance");
        assert!(second_epoch > first_epoch && maintenance.start(second_epoch));

        maintenance.begin();
        assert!(!second.is_cancelled());
        let (third_epoch, third) = maintenance
            .finish(&parent)
            .expect("a write during maintenance schedules a follow-up pass");
        assert_eq!(maintenance.complete_pass(second_epoch), Some(third_epoch));
        assert!(third.is_cancelled());
        assert_eq!(maintenance.complete_pass(third_epoch), None);

        parent.cancel();
        assert!(second.is_cancelled());
    }

    #[test]
    fn sustained_writes_claim_one_periodic_catalog_maintenance() {
        let maintenance = WriteMaintenance::new();

        assert!(!maintenance.claim_catalog_maintenance(63));
        assert!(maintenance.claim_catalog_maintenance(65));
        assert!(!maintenance.claim_catalog_maintenance(128));
        maintenance.finish_catalog_maintenance();
        assert!(!maintenance.claim_catalog_maintenance(128));
        assert!(maintenance.claim_catalog_maintenance(129));
    }

    #[test]
    fn read_view_cells_evict_before_inserting_past_capacity() {
        let mut cache: HashMap<String, Arc<OnceCell<()>>> = HashMap::new();
        let first = bounded_cell(&mut cache, "first".to_owned(), 2);
        let _second = bounded_cell(&mut cache, "second".to_owned(), 2);
        assert_eq!(cache.len(), 2);

        let third = bounded_cell(&mut cache, "third".to_owned(), 2);
        assert_eq!(cache.len(), 1);
        assert!(!cache.contains_key("first"));
        assert!(Arc::ptr_eq(
            &third,
            cache.get("third").expect("third cell is cached")
        ));
        assert_eq!(Arc::strong_count(&first), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_refreshes_share_one_generation_view() {
        let store = crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new()));
        let layout = crab_storage::StoreLayout::new(store.clone(), "read-view-test".to_owned());
        crab_write::initialize::initialize_repository(&store, &layout, "refs/heads/main")
            .await
            .unwrap();
        let repository =
            Arc::new(Repository::new(repository_config("read-view-test"), store).unwrap());
        let runtime = Arc::new(RemoteGitRuntime::default());
        let barrier = Arc::new(tokio::sync::Barrier::new(16));
        let mut reads = tokio::task::JoinSet::new();
        for _ in 0..16 {
            let repository = Arc::clone(&repository);
            let runtime = Arc::clone(&runtime);
            let barrier = Arc::clone(&barrier);
            reads.spawn(async move {
                barrier.wait().await;
                repository
                    .read_views
                    .current(
                        &repository,
                        runtime,
                        RepositoryOptions::default(),
                        &CancellationToken::new(),
                    )
                    .await
                    .unwrap()
            });
        }
        let first = reads.join_next().await.unwrap().unwrap();
        while let Some(result) = reads.join_next().await {
            assert!(Arc::ptr_eq(&first, &result.unwrap()));
        }
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn capsule_repository_read_view_does_not_create_v1_metadata() {
        let store = crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new()));
        let layout = crab_storage::StoreLayout::new(store.clone(), "capsule-read-view".to_owned());
        crab_write::capsule_protocol::initialize(&layout, &"a".repeat(64), "refs/heads/main")
            .await
            .unwrap();
        let repository =
            Repository::new(repository_config("capsule-read-view"), store.clone()).unwrap();
        let runtime = Arc::new(RemoteGitRuntime::default());

        let view = repository
            .read_views
            .current(
                &repository,
                Arc::clone(&runtime),
                RepositoryOptions::default(),
                &CancellationToken::new(),
            )
            .await
            .unwrap();

        assert_eq!(view.key.generation, 0);
        assert!(matches!(
            store.head(&layout.manifest_path()).await,
            Err(crab_storage::StorageError::NotFound { .. })
        ));
        runtime.shutdown().await;
    }

    #[tokio::test]
    async fn corrupt_capsule_root_never_falls_back_to_v1() {
        let store = crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new()));
        let layout = crab_storage::StoreLayout::new(store.clone(), "dual-read-view".to_owned());
        crab_write::initialize::initialize_repository(&store, &layout, "refs/heads/main")
            .await
            .unwrap();
        store
            .put_overwrite(
                &layout.capsule_root_path(),
                bytes::Bytes::from_static(b"corrupt v2 authority"),
            )
            .await
            .unwrap();
        let repository = Repository::new(repository_config("dual-read-view"), store).unwrap();
        let runtime = Arc::new(RemoteGitRuntime::default());

        let result = repository
            .read_views
            .current(
                &repository,
                Arc::clone(&runtime),
                RepositoryOptions::default(),
                &CancellationToken::new(),
            )
            .await;
        let Err(error) = result else {
            panic!("corrupt capsule authority unexpectedly opened through v1");
        };

        assert!(matches!(error, crate::Error::Metadata(_)), "{error:?}");
        runtime.shutdown().await;
    }
}
