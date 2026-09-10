use std::{
    collections::HashMap,
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
use gix_hash::ObjectId;
use tokio::sync::{Mutex, OnceCell, RwLock};
use tokio_util::sync::CancellationToken;

use crate::gateway::Repository;

const MAINTENANCE_TTL: Duration = Duration::from_secs(60);

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
    snapshots: Mutex<HashMap<String, Arc<OnceCell<RemoteGitSnapshot>>>>,
    manifests: Mutex<HashMap<ObjectId, Arc<OnceCell<Arc<crate::attributes::Manifest>>>>>,
    objects: Mutex<HashMap<ObjectAttributeKey, ObjectAttributeCell>>,
}

impl ReadView {
    pub(crate) fn remote(&self) -> &RemoteGitRepository {
        &self.remote
    }

    pub(crate) async fn snapshot(
        &self,
        revision: &str,
        operation: &OperationContext,
    ) -> crate::Result<RemoteGitSnapshot> {
        let cell = {
            let mut snapshots = self.snapshots.lock().await;
            Arc::clone(
                snapshots
                    .entry(revision.to_owned())
                    .or_insert_with(|| Arc::new(OnceCell::new())),
            )
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
            Arc::clone(
                manifests
                    .entry(commit)
                    .or_insert_with(|| Arc::new(OnceCell::new())),
            )
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
            Arc::clone(
                objects
                    .entry(key)
                    .or_insert_with(|| Arc::new(OnceCell::new())),
            )
        };
        cell.get_or_try_init(|| crate::attributes::load_object(repository, commit, path, oid))
            .await
            .cloned()
    }
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
        let snapshot = crab_metadata::manifest_store::read_repository_snapshot(
            &repository.store,
            &repository.layout,
        )
        .await?;
        let observed_at = tokio::time::Instant::now();
        let key = ReadViewKey {
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
            None => Arc::new(ReadView {
                key,
                remote: RemoteGitRepository::from_snapshot(
                    repository.layout.clone(),
                    &snapshot,
                    repository.identity.clone(),
                    runtime,
                    options,
                    cancel,
                )
                .await?,
                snapshots: Mutex::new(HashMap::new()),
                manifests: Mutex::new(HashMap::new()),
                objects: Mutex::new(HashMap::new()),
            }),
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
            let result = crab_write::generation::ensure_readable(
                &store,
                &layout,
                &identity,
                Arc::clone(&runtime),
                options,
                MAINTENANCE_TTL,
                &cancel,
            )
            .await;
            if let Err(error) = result {
                maintenance.stop();
                match error {
                    crab_write::WriteError::VisibilityUnavailable { generation } => {
                        tracing::warn!(
                            generation,
                            recovery = "run `crab fsck --repair`, then `crab metadb owner --once`, against this repository",
                            "S3 repository requires verified Git visibility repair"
                        );
                    }
                    crab_write::WriteError::Cancelled if cancel.is_cancelled() => {}
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
        () = tokio::time::sleep(Duration::from_millis(100)) => maintenance.is_idle_at(epoch),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RepositoryAccess, RepositoryConfig};

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

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_refreshes_share_one_generation_view() {
        let store = crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new()));
        let layout = crab_storage::StoreLayout::new(store.clone(), "read-view-test".to_owned());
        crab_write::initialize::initialize_repository(&store, &layout, "refs/heads/main")
            .await
            .unwrap();
        let repository = Arc::new(
            Repository::new(
                RepositoryConfig {
                    name: "repo".to_owned(),
                    provider: crab_storage::StorageProviderKind::Local,
                    bucket: "memory".to_owned(),
                    prefix: "read-view-test".to_owned(),
                    default_branch: "main".to_owned(),
                    members: vec![crate::RepositoryMember {
                        principal: "user".to_owned(),
                        access: RepositoryAccess::Read,
                    }],
                    protected_branches: Vec::new(),
                    max_active_multipart_uploads: 16,
                    multipart_staging_bytes_per_upload: 50_000_000_000_000,
                    multipart_upload_ttl_seconds: 604_800,
                },
                store,
            )
            .unwrap(),
        );
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
}
