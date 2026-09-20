use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::io;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock as SyncRwLock};
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use axum::{
    Extension, Json, Router,
    extract::{Request, State},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use bytes::Bytes;
use crab_cell_host::{CellNode, CellNodeBuilder};
use crab_cell_runtime::{
    ACTIVE_CELL_FILE_DESCRIPTORS, ACTIVE_CELL_NATIVE_BYTES, ACTIVE_CELL_PAGE_CACHE_BYTES,
    ApplicationIdentityStore, CellRuntime, Digest, NodeDirectory, Owner, PeerRoundTrip, PeerSigner,
    ReleaseState, ReleaseStore, ReplicaHost, ScratchMonitor, SessionId, SqlWorkerPool,
};
use crab_metadata::manifest_store::read_manifest;
use crab_remote_git::{
    OperationLimits, RemoteGitRepository, RemoteGitRuntime, RepositoryIdentity, RepositoryOptions,
};
use crab_storage::{StorageError, Store, StoreLayout};
use object_store::path::Path as ObjectPath;
use serde::Serialize;
use serde_json::json;
use tokio::sync::{Mutex, Semaphore};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;
use uuid::Uuid;

use crate::catalog::CatalogStore;
use crate::{
    Config, RepositoryConfig, Result, api, app, archive, assets, assignees,
    auth::{self, Authentication, Principal},
    branches, checks, contents, git, git_import, issues, labels, lfs, maintenance, pulls, receive,
    releases,
    repository_settings::{self, BranchProtections, RepositoryLifecycle},
    statuses,
    transfer_admission::TransferAdmission,
};

pub(crate) const MAX_DEPENDENCY_FILE_BYTES: u64 = 512 * 1024 * 1024;
const READ_ADMISSION_CAPACITY: usize = 16;
const GIT_ADMISSION_CAPACITY: usize = 4;
const APP_ADMISSION_CAPACITY: usize = 8;
const MAINTENANCE_ADMISSION_CAPACITY: usize = 2;
const MAX_ACTIVE_CELLS: usize = 10_000;
const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;
const MIN_CELL_MEMORY_BYTES: u64 = 2 * GIB;
const MIN_USABLE_CELL_DISK_BYTES: u64 = 20 * GIB;
const FILE_DESCRIPTOR_RESERVE_MINIMUM: usize = 128;
const DIRTY_JOB_MEMORY_BYTES: u64 = 64 * MIB;
const MAX_BLOCKING_JOBS: usize = 16;
const MAX_RECOVERY_JOBS: usize = 2;
const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(110);
const RELEASE_POLL_INTERVAL: Duration = Duration::from_secs(1);
const NODE_LOG_RECRUIT_INTERVAL: Duration = Duration::from_secs(3);
const NODE_LOG_LIVE_NODE_LIMIT: usize = 1_024;
const NODE_LOG_ROTATION_INTERVAL: Duration = Duration::from_secs(5);
const NODE_LOG_ROTATION_FRAMES: u64 = 1_000_000;
const RETIRED_FOLLOWER_COLLECTION_INTERVAL: Duration = Duration::from_secs(60);
const RETIRED_FOLLOWER_GRACE_MS: i64 = 10 * 60 * 1_000;
const RETIRED_FOLLOWER_BATCH: usize = 64;
const PROJECTION_SWEEP_INTERVAL: Duration = Duration::from_secs(5);
const PROJECTION_SWEEP_BATCH: usize = 16;
const PROJECTION_RETRY_BASE: Duration = Duration::from_secs(10);
const PROJECTION_RETRY_MAX: Duration = Duration::from_secs(5 * 60);
const CELL_COMPONENT_REPOSITORY_ROUTER: &str = "repository-cell-router";
const CELL_COMPONENT_PEER_RECEIVER: &str = "peer-receiver";
const CELL_COMPONENT_FOLLOWER_STORE: &str = "follower-store";
const CELL_COMPONENT_NODE_LOG_TRANSPORT: &str = "node-log-transport";
const CELL_COMPONENT_NODE_PUBLISHER: &str = "node-publisher";
const CELL_COMPONENT_CATALOG: &str = "repository-catalog";
const CELL_COMPONENT_SCHEDULER_STATUS: &str = "scheduler-status";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CellRuntimeBudget {
    node_retained_bytes: usize,
    max_active_cells: usize,
    blocking_jobs: usize,
    dirty_jobs: usize,
    recovery_jobs: usize,
    scratch_mebibytes: usize,
    local_disk_mebibytes: usize,
    disk_reserve_bytes: u64,
}

#[derive(Clone, Serialize)]
pub(crate) struct CellCapacityReport {
    version: u32,
    resources: CellCapacityResources,
    admission: CellCapacityAdmission,
    reservations: CellCapacityReservations,
}

#[derive(Clone, Serialize)]
struct CellCapacityResources {
    memory_bytes: u64,
    disk_limit_bytes: u64,
    disk_capacity_bytes: u64,
    free_disk_bytes: u64,
    available_file_descriptors: usize,
    job_credits: usize,
}

#[derive(Clone, Serialize)]
struct CellCapacityAdmission {
    active_cells: usize,
    retained_bytes: usize,
    blocking_jobs: usize,
    dirty_jobs: usize,
    recovery_jobs: usize,
    scratch_bytes: u64,
    local_disk_bytes: u64,
    disk_reserve_bytes: u64,
}

#[derive(Clone, Serialize)]
struct CellCapacityReservations {
    active_cell_page_cache_bytes: u64,
    active_cell_native_bytes: u64,
    active_cell_file_descriptors: usize,
    dirty_job_memory_bytes: u64,
    maximum_recovery_jobs: usize,
}

impl CellRuntimeBudget {
    pub(crate) fn from_resources(resources: crate::peer::LocalResources) -> Result<Self> {
        if resources.memory_bytes < MIN_CELL_MEMORY_BYTES {
            return Err(crate::Error::Config(
                "Cell runtime requires at least 2 GiB effective memory",
            ));
        }
        let disk_reserve = (resources.free_disk_bytes / 5).max(10 * GIB);
        let usable_disk = resources.free_disk_bytes.saturating_sub(disk_reserve);
        if usable_disk < MIN_USABLE_CELL_DISK_BYTES {
            return Err(crate::Error::Config(
                "Cell runtime requires at least 20 GiB usable local disk",
            ));
        }
        let process_reserve = (resources.memory_bytes / 4).max(512 * MIB);
        let cell_memory = resources.memory_bytes - process_reserve;
        let page_cache_memory = cell_memory.saturating_mul(35) / 100;
        let memory_cells = page_cache_memory / ACTIVE_CELL_PAGE_CACHE_BYTES;
        let native_memory = cell_memory.saturating_mul(10) / 100;
        let native_cells = native_memory / ACTIVE_CELL_NATIVE_BYTES;
        let descriptor_reserve =
            (resources.available_file_descriptors / 10).max(FILE_DESCRIPTOR_RESERVE_MINIMUM);
        let descriptor_cells = resources
            .available_file_descriptors
            .saturating_sub(descriptor_reserve)
            / ACTIVE_CELL_FILE_DESCRIPTORS;
        let max_active_cells = usize::try_from(memory_cells)
            .unwrap_or(usize::MAX)
            .min(usize::try_from(native_cells).unwrap_or(usize::MAX))
            .min(descriptor_cells)
            .min(MAX_ACTIVE_CELLS);
        if max_active_cells == 0 {
            return Err(crate::Error::Config(
                "Cell runtime has no memory and file-descriptor capacity",
            ));
        }
        let mailbox = usize::try_from(cell_memory / 20)
            .unwrap_or(usize::MAX)
            .min(tokio::sync::Semaphore::MAX_PERMITS);
        let blocking_jobs = resources.job_credits.min(MAX_BLOCKING_JOBS);
        if blocking_jobs == 0 {
            return Err(crate::Error::Config(
                "Cell runtime has no blocking job capacity",
            ));
        }
        let dirty_memory = cell_memory.saturating_mul(25) / 100;
        let dirty_jobs = usize::try_from(dirty_memory / DIRTY_JOB_MEMORY_BYTES)
            .unwrap_or(usize::MAX)
            .min(blocking_jobs);
        if dirty_jobs == 0 {
            return Err(crate::Error::Config(
                "Cell runtime has no capture and recovery job capacity",
            ));
        }
        let recovery_jobs = dirty_jobs.min(MAX_RECOVERY_JOBS);
        let scratch_mebibytes = usize::try_from(usable_disk / 3 / MIB)
            .unwrap_or(usize::MAX)
            .min(u32::MAX as usize)
            .min(tokio::sync::Semaphore::MAX_PERMITS);
        if scratch_mebibytes == 0 {
            return Err(crate::Error::Config(
                "Cell runtime has no temporary scratch-disk capacity",
            ));
        }
        let local_disk_mebibytes = usize::try_from(usable_disk.saturating_mul(2) / 3 / MIB)
            .unwrap_or(usize::MAX)
            .min(u32::MAX as usize)
            .min(tokio::sync::Semaphore::MAX_PERMITS);
        if local_disk_mebibytes == 0 {
            return Err(crate::Error::Config(
                "Cell runtime has no local working-disk capacity",
            ));
        }
        Ok(Self {
            node_retained_bytes: mailbox,
            max_active_cells,
            blocking_jobs,
            dirty_jobs,
            recovery_jobs,
            scratch_mebibytes,
            local_disk_mebibytes,
            disk_reserve_bytes: disk_reserve,
        })
    }

    pub(crate) fn local_disk(self) -> crab_cell_runtime::DiskBudget {
        crab_cell_runtime::DiskBudget::new(self.local_disk_mebibytes as u64 * MIB)
    }

    pub(crate) fn replica_host(
        self,
        local_disk: crab_cell_runtime::DiskBudget,
        scratch_root: PathBuf,
    ) -> ReplicaHost {
        let scratch_monitor = Arc::new(ActualScratchMonitor {
            root: scratch_root,
            local_disk: local_disk.clone(),
            reserve_bytes: self.disk_reserve_bytes,
        });
        ReplicaHost::default()
            .with_job_slots(Arc::new(Semaphore::new(self.blocking_jobs)))
            .with_recovery_slots(Arc::new(Semaphore::new(self.recovery_jobs)))
            .with_dirty_slots(Arc::new(Semaphore::new(self.dirty_jobs)))
            .with_scratch_slots(Arc::new(Semaphore::new(self.scratch_mebibytes)))
            .with_scratch_monitor(scratch_monitor)
            .with_local_disk_budget(local_disk)
    }
}

impl CellCapacityReport {
    fn new(resources: crate::peer::LocalResources, budget: CellRuntimeBudget) -> Self {
        Self {
            version: 1,
            resources: CellCapacityResources {
                memory_bytes: resources.memory_bytes,
                disk_limit_bytes: resources.disk_limit_bytes,
                disk_capacity_bytes: resources.disk_capacity_bytes,
                free_disk_bytes: resources.free_disk_bytes,
                available_file_descriptors: resources.available_file_descriptors,
                job_credits: resources.job_credits,
            },
            admission: CellCapacityAdmission {
                active_cells: budget.max_active_cells,
                retained_bytes: budget.node_retained_bytes,
                blocking_jobs: budget.blocking_jobs,
                dirty_jobs: budget.dirty_jobs,
                recovery_jobs: budget.recovery_jobs,
                scratch_bytes: budget.scratch_mebibytes as u64 * MIB,
                local_disk_bytes: budget.local_disk_mebibytes as u64 * MIB,
                disk_reserve_bytes: budget.disk_reserve_bytes,
            },
            reservations: CellCapacityReservations {
                active_cell_page_cache_bytes: ACTIVE_CELL_PAGE_CACHE_BYTES,
                active_cell_native_bytes: ACTIVE_CELL_NATIVE_BYTES,
                active_cell_file_descriptors: ACTIVE_CELL_FILE_DESCRIPTORS,
                dirty_job_memory_bytes: DIRTY_JOB_MEMORY_BYTES,
                maximum_recovery_jobs: MAX_RECOVERY_JOBS,
            },
        }
    }
}

pub(crate) fn cell_capacity_report(
    data_dir: &std::path::Path,
    local_disk_limit_bytes: u64,
) -> Result<Vec<u8>> {
    encode_cell_capacity_report(crate::peer::local_resources(
        data_dir,
        local_disk_limit_bytes,
    )?)
}

fn encode_cell_capacity_report(resources: crate::peer::LocalResources) -> Result<Vec<u8>> {
    let budget = CellRuntimeBudget::from_resources(resources)?;
    Ok(serde_json::to_vec_pretty(&CellCapacityReport::new(
        resources, budget,
    ))?)
}

#[cfg(test)]
pub(crate) fn test_cell_capacity_report() -> CellCapacityReport {
    let resources = crate::peer::LocalResources {
        memory_bytes: 2 * GIB,
        disk_limit_bytes: 30 * GIB,
        disk_capacity_bytes: 30 * GIB,
        free_disk_bytes: 30 * GIB,
        available_file_descriptors: 10_000,
        job_credits: 2,
    };
    let budget = CellRuntimeBudget::from_resources(resources).unwrap();
    CellCapacityReport::new(resources, budget)
}

struct ActualScratchMonitor {
    root: PathBuf,
    local_disk: crab_cell_runtime::DiskBudget,
    reserve_bytes: u64,
}

impl ScratchMonitor for ActualScratchMonitor {
    fn ensure_available(&self, scratch_bytes: u64) -> io::Result<()> {
        let required = self
            .reserve_bytes
            .checked_add(self.local_disk.used())
            .and_then(|bytes| bytes.checked_add(scratch_bytes))
            .ok_or_else(|| io::Error::from(io::ErrorKind::StorageFull))?;
        if fs4::available_space(&self.root)? < required {
            return Err(io::Error::from(io::ErrorKind::StorageFull));
        }
        Ok(())
    }
}

fn transfer_admission(catalog: &CatalogStore) -> TransferAdmission {
    TransferAdmission::new(
        catalog.root().store.clone(),
        catalog
            .root()
            .path(".crab/http-server/v1/admission")
            .to_string(),
        GIT_ADMISSION_CAPACITY,
    )
}

async fn before_shutdown_deadline<T>(
    deadline: Instant,
    future: impl Future<Output = T>,
) -> Result<T> {
    tokio::time::timeout_at(deadline.into(), future)
        .await
        .map_err(|_| crate::Error::ShutdownTimeout)
}

fn release_admits_process(
    state: ReleaseState,
    current: Option<Digest>,
    desired: Option<Digest>,
    compiled: Digest,
) -> bool {
    match state {
        ReleaseState::Ready => current == Some(compiled) && desired == Some(compiled),
        ReleaseState::Prepared | ReleaseState::Activating => {
            current == Some(compiled) || desired == Some(compiled)
        }
        ReleaseState::Maintenance | ReleaseState::Failed => false,
    }
}

async fn watch_release(
    releases: ReleaseStore,
    compiled: Digest,
    cancellation: CancellationToken,
) -> Result<()> {
    loop {
        tokio::select! {
            () = cancellation.cancelled() => return Ok(()),
            () = tokio::time::sleep(RELEASE_POLL_INTERVAL) => {}
        }
        let observed = match releases.load().await {
            Ok(Some(observed)) => observed,
            Ok(None) => {
                cancellation.cancel();
                return Err(crate::Error::Config("Cell application release disappeared"));
            }
            Err(error) => {
                cancellation.cancel();
                return Err(error.into());
            }
        };
        let record = observed.record();
        if !release_admits_process(record.state(), record.current(), record.desired(), compiled) {
            cancellation.cancel();
            return Ok(());
        }
    }
}

#[cfg(test)]
fn start_test_cell_runtime() -> CellRuntime {
    crate::cells::compiled_registry().unwrap();
    CellRuntime::new(
        SqlWorkerPool::new(1, 16).unwrap(),
        2 * 1024 * 1024,
        SessionId::from_bytes(Uuid::now_v7().into_bytes()),
    )
    .unwrap()
}

async fn probe_storage_contract(
    catalog: &CatalogStore,
    transfer_admission: &TransferAdmission,
) -> Result<()> {
    let root = catalog.root();
    root.store
        .list_prefix_bounded(&root.path(".crab/http-server/v1"), 1)
        .await?;
    transfer_admission.probe().await?;

    // A unique object avoids cross-pod interference. Provider lifecycle rules
    // bound residue if a pod dies between creation and deletion.
    let path = root.path(&format!(
        ".crab/http-server/v1/auth/preflight/{}",
        Uuid::now_v7()
    ));
    root.store
        .put_overwrite(&path, Bytes::from_static(b"crab-storage-probe-v1"))
        .await?;
    match root.store.delete(&path).await {
        Ok(()) | Err(StorageError::NotFound { .. }) => {}
        Err(error) => return Err(error.into()),
    }
    match root.store.head(&path).await {
        Err(StorageError::NotFound { .. }) => Ok(()),
        Ok(_) => Err(crate::Error::StorageProbe(
            "an object remained visible after a successful delete",
        )),
        Err(error) => Err(error.into()),
    }
}

pub(crate) struct Repository {
    pub id: Uuid,
    pub config: RepositoryConfig,
    pub store: Store,
    pub layout: StoreLayout<Store>,
    pub identity: RepositoryIdentity,
    pinned: Mutex<Option<(Instant, RemoteGitRepository)>>,
    maintenance: Mutex<Option<tokio::task::JoinHandle<crab_write::Result<()>>>>,
}

pub(crate) struct RepositorySet {
    current: SyncRwLock<RepositoryIndex>,
}

struct RepositoryIndex {
    by_name: BTreeMap<(String, String), Arc<Repository>>,
    by_id: HashMap<Uuid, (String, String)>,
}

impl RepositorySet {
    pub(crate) fn get(&self, key: &(String, String)) -> Option<Arc<Repository>> {
        self.current
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .by_name
            .get(key)
            .cloned()
    }

    pub(crate) fn by_id(&self, id: Uuid) -> Option<Arc<Repository>> {
        let current = self
            .current
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        current
            .by_id
            .get(&id)
            .and_then(|key| current.by_name.get(key))
            .cloned()
    }

    pub(crate) fn values(&self) -> Vec<Arc<Repository>> {
        self.current
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .by_name
            .values()
            .cloned()
            .collect()
    }

    pub(crate) fn len(&self) -> usize {
        self.current
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .by_name
            .len()
    }

    pub(crate) fn replace(&self, next: BTreeMap<(String, String), Arc<Repository>>) {
        *self
            .current
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = RepositoryIndex::new(next);
    }

    #[cfg(test)]
    pub(crate) fn get_mut(&mut self, key: &(String, String)) -> Option<&mut Repository> {
        self.current
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .by_name
            .get_mut(key)
            .and_then(Arc::get_mut)
    }
}

impl From<BTreeMap<(String, String), Repository>> for RepositorySet {
    fn from(repositories: BTreeMap<(String, String), Repository>) -> Self {
        Self {
            current: SyncRwLock::new(RepositoryIndex::new(
                repositories
                    .into_iter()
                    .map(|(key, repository)| (key, Arc::new(repository)))
                    .collect(),
            )),
        }
    }
}

impl From<BTreeMap<(String, String), Arc<Repository>>> for RepositorySet {
    fn from(repositories: BTreeMap<(String, String), Arc<Repository>>) -> Self {
        Self {
            current: SyncRwLock::new(RepositoryIndex::new(repositories)),
        }
    }
}

impl RepositoryIndex {
    fn new(by_name: BTreeMap<(String, String), Arc<Repository>>) -> Self {
        let by_id = by_name
            .iter()
            .map(|(key, repository)| (repository.id, key.clone()))
            .collect();
        Self { by_name, by_id }
    }
}

impl Repository {
    pub(crate) async fn branch_protections(
        &self,
        server: &Server,
        actor: &auth::Identity,
    ) -> app::Result<BranchProtections> {
        repository_settings::load(server, self, actor).await
    }

    pub(crate) async fn lifecycle(
        &self,
        server: &Server,
        actor: &auth::Identity,
    ) -> app::Result<RepositoryLifecycle> {
        repository_settings::load_lifecycle(server, self, actor).await
    }

    pub(crate) async fn invalidate(&self) {
        *self.pinned.lock().await = None;
    }

    pub(crate) async fn schedule_maintenance(
        &self,
        server: &Server,
    ) -> Result<MaintenanceSchedule> {
        if server
            .git_import
            .as_ref()
            .is_some_and(|context| context.is_repository_importing(self.id))
        {
            return Ok(MaintenanceSchedule::Deferred);
        }
        let completed = {
            let mut worker = self.maintenance.lock().await;
            worker
                .as_ref()
                .is_some_and(tokio::task::JoinHandle::is_finished)
                .then(|| worker.take())
                .flatten()
        };
        if let Some(completed) = completed {
            completed.await??;
            *self.pinned.lock().await = None;
            // The next retry must reopen the just-published generation. Starting
            // another worker here would duplicate the same immutable index build.
            return Ok(MaintenanceSchedule::Completed);
        }
        let mut worker = self.maintenance.lock().await;
        if worker.is_some() {
            return Ok(MaintenanceSchedule::Running);
        }
        let permit = match server.maintenance_admission.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => return Ok(MaintenanceSchedule::Deferred),
        };
        *worker = Some(tokio::spawn(maintenance::run_with_projection(
            self.store.clone(),
            self.layout.clone(),
            self.identity.clone(),
            Arc::clone(&server.runtime),
            server.options,
            Arc::clone(&server.maintenance_admission),
            Some(permit),
            server.cancellation.clone(),
            server
                .repository_cells()
                .map(|router| maintenance::ProjectionContext {
                    repository_id: self.id,
                    router: (*router).clone(),
                    metrics: server.metrics.clone(),
                }),
        )));
        Ok(MaintenanceSchedule::Started)
    }

    pub async fn open(
        &self,
        server: &Server,
        cancellation: &CancellationToken,
    ) -> Result<RemoteGitRepository> {
        let mut pinned = tokio::select! {
            _ = cancellation.cancelled() => return Err(crab_remote_git::Error::Cancelled.into()),
            pinned = self.pinned.lock() => pinned,
        };
        if let Some((checked, repository)) = pinned.as_ref()
            && checked.elapsed() < Duration::from_secs(2)
        {
            return Ok(repository.clone());
        }
        // Journal commits can change refs without changing the manifest ETag.
        // Reopen after the cache window so pending publication becomes visible.
        let repository = self
            .open_current(server, server.options, cancellation)
            .await?;
        *pinned = Some((Instant::now(), repository.clone()));
        Ok(repository)
    }

    pub(crate) async fn open_current(
        &self,
        server: &Server,
        options: RepositoryOptions,
        cancellation: &CancellationToken,
    ) -> Result<RemoteGitRepository> {
        let open = || {
            RemoteGitRepository::open(
                self.store.clone(),
                self.layout.clone(),
                self.identity.clone(),
                Arc::clone(&server.runtime),
                options,
                cancellation,
            )
        };
        match open().await {
            Ok(repository)
                if repository.refs().is_empty() || repository.commit_graph_available() =>
            {
                return Ok(repository);
            }
            Ok(_) => {}
            Err(crab_remote_git::Error::RepositoryIndexing { .. }) => {}
            Err(error) => return Err(error.into()),
        }
        let mut worker = tokio::select! {
            () = cancellation.cancelled() => return Err(crab_remote_git::Error::Cancelled.into()),
            worker = self.maintenance.lock() => worker,
        };
        if worker.is_none() {
            // A preceding request may have finished maintenance while this one waited.
            match open().await {
                Ok(repository)
                    if repository.refs().is_empty() || repository.commit_graph_available() =>
                {
                    return Ok(repository);
                }
                Ok(_) => {}
                Err(crab_remote_git::Error::RepositoryIndexing { .. }) => {}
                Err(error) => return Err(error.into()),
            }
            *worker = Some(tokio::spawn(maintenance::run_with_projection(
                self.store.clone(),
                self.layout.clone(),
                self.identity.clone(),
                Arc::clone(&server.runtime),
                options,
                Arc::clone(&server.maintenance_admission),
                None,
                server.cancellation.clone(),
                server
                    .repository_cells()
                    .map(|router| maintenance::ProjectionContext {
                        repository_id: self.id,
                        router: (*router).clone(),
                        metrics: server.metrics.clone(),
                    }),
            )));
        }
        if let Some(task) = worker.as_mut() {
            // A cancelled reader leaves the handle in this slot. A later reader
            // or server shutdown must drain publication and its lease cleanup.
            let result = tokio::select! {
                () = cancellation.cancelled() => return Err(crab_remote_git::Error::Cancelled.into()),
                result = task => result,
            };
            *worker = None;
            result??;
        }
        open().await.map_err(Into::into)
    }
}

pub(crate) struct Server {
    pub repositories: RepositorySet,
    pub runtime: Arc<RemoteGitRuntime>,
    pub(crate) cell_node: Option<Arc<CellNode>>,
    #[cfg(test)]
    pub(crate) cell_runtime: CellRuntime,
    #[cfg(test)]
    pub(crate) repository_cells: Option<crate::cells::RepositoryCellRouter>,
    #[cfg(test)]
    pub(crate) peer_receiver: Option<crate::peer::PeerReceiver>,
    #[cfg(test)]
    pub(crate) follower_store: Option<crab_cell_runtime::FollowerStore>,
    #[cfg(test)]
    pub(crate) node_log_transport: Option<Arc<dyn crab_cell_runtime::NodeLogTransport>>,
    pub options: RepositoryOptions,
    pub cursor_key: [u8; 32],
    pub admission: Semaphore,
    pub transfer_admission: TransferAdmission,
    pub(crate) local_staging: crate::local_disk::LocalStaging,
    pub app_admission: Semaphore,
    maintenance_admission: Arc<Semaphore>,
    pub cancellation: CancellationToken,
    pub receives: tokio_util::task::TaskTracker,
    pub auth: Option<Authentication>,
    pub(crate) git_import: Option<git_import::ImportContext>,
    #[cfg(test)]
    pub(crate) catalog: Option<CatalogStore>,
    catalog_healthy: AtomicBool,
    pub(crate) node_healthy: AtomicBool,
    #[cfg(test)]
    pub(crate) scheduler_status: crate::cells::SchedulerStatus,
    cell_capacity: CellCapacityReport,
    pub(crate) metrics: crate::metrics::Metrics,
}

impl Server {
    pub(crate) fn cell_runtime(&self) -> Result<CellRuntime> {
        self.cell_node
            .as_ref()
            .map(|node| node.runtime())
            .or_else(|| {
                #[cfg(test)]
                {
                    Some(self.cell_runtime.clone())
                }
                #[cfg(not(test))]
                {
                    None
                }
            })
            .ok_or(crate::Error::Config("Cell runtime host is unavailable"))
    }

    fn node_component<T>(&self, name: &str) -> Option<Arc<T>>
    where
        T: Send + Sync + 'static,
    {
        self.cell_node
            .as_ref()
            .and_then(|node| node.owned_component(name))
    }

    pub(crate) fn repository_cells(&self) -> Option<Arc<crate::cells::RepositoryCellRouter>> {
        self.node_component(CELL_COMPONENT_REPOSITORY_ROUTER)
            .or_else(|| {
                #[cfg(test)]
                {
                    self.repository_cells.clone().map(Arc::new)
                }
                #[cfg(not(test))]
                {
                    None
                }
            })
    }

    pub(crate) fn peer_receiver(&self) -> Option<Arc<crate::peer::PeerReceiver>> {
        self.node_component(CELL_COMPONENT_PEER_RECEIVER)
            .or_else(|| {
                #[cfg(test)]
                {
                    self.peer_receiver.clone().map(Arc::new)
                }
                #[cfg(not(test))]
                {
                    None
                }
            })
    }

    pub(crate) fn follower_store(&self) -> Option<Arc<crab_cell_runtime::FollowerStore>> {
        self.node_component(CELL_COMPONENT_FOLLOWER_STORE)
            .or_else(|| {
                #[cfg(test)]
                {
                    self.follower_store.clone().map(Arc::new)
                }
                #[cfg(not(test))]
                {
                    None
                }
            })
    }

    pub(crate) fn node_log_transport(
        &self,
    ) -> Option<Arc<dyn crab_cell_runtime::NodeLogTransport>> {
        self.node_component::<Arc<dyn crab_cell_runtime::NodeLogTransport>>(
            CELL_COMPONENT_NODE_LOG_TRANSPORT,
        )
        .map(|transport| transport.as_ref().clone())
        .or_else(|| {
            #[cfg(test)]
            {
                self.node_log_transport.clone()
            }
            #[cfg(not(test))]
            {
                None
            }
        })
    }

    fn catalog(&self) -> Option<CatalogStore> {
        self.node_component::<CatalogStore>(CELL_COMPONENT_CATALOG)
            .map(|catalog| catalog.as_ref().clone())
            .or_else(|| {
                #[cfg(test)]
                {
                    self.catalog.clone()
                }
                #[cfg(not(test))]
                {
                    None
                }
            })
    }

    fn scheduler_status(&self) -> Option<crate::cells::SchedulerStatus> {
        self.node_component::<crate::cells::SchedulerStatus>(CELL_COMPONENT_SCHEDULER_STATUS)
            .map(|status| status.as_ref().clone())
            .or({
                #[cfg(test)]
                {
                    Some(self.scheduler_status.clone())
                }
                #[cfg(not(test))]
                {
                    None
                }
            })
    }

    pub(crate) fn accepts_application_peers(&self) -> bool {
        self.node_healthy.load(Ordering::Acquire) && !self.cancellation.is_cancelled()
    }

    pub(crate) async fn acquire_transfer(
        &self,
        cancellation: &CancellationToken,
    ) -> std::result::Result<
        crate::transfer_admission::TransferPermit,
        crate::transfer_admission::Error,
    > {
        let result = self.transfer_admission.try_acquire(cancellation).await;
        match &result {
            Err(crate::transfer_admission::Error::Busy) => {
                self.metrics.record_transfer_admission_rejection(false);
            }
            Err(crate::transfer_admission::Error::Coordination(_)) => {
                self.metrics.record_transfer_admission_rejection(true);
            }
            Ok(_) | Err(crate::transfer_admission::Error::Cancelled) => {}
        }
        result
    }

    async fn finish_maintenance(&self) -> Result<()> {
        let mut result = Ok(());
        for repository in self.repositories.values() {
            if let Some(task) = repository.maintenance.lock().await.take() {
                let completed = match task.await {
                    Ok(Ok(())) | Ok(Err(crab_write::WriteError::Cancelled)) => Ok(()),
                    Ok(Err(error)) => Err(crate::Error::from(error)),
                    Err(error) => Err(crate::Error::from(error)),
                };
                result = result.and(completed);
            }
        }
        result
    }

    async fn shutdown_runtimes(&self) -> Result<()> {
        self.shutdown_runtimes_until(None).await
    }

    async fn shutdown_runtimes_until(&self, deadline: Option<Instant>) -> Result<()> {
        let cells = match self.cell_node.as_ref() {
            Some(node) => match deadline {
                Some(deadline) => node.shutdown_until(deadline).await,
                None => node.shutdown().await,
            },
            None => self.cell_runtime()?.shutdown().await,
        };
        self.runtime.shutdown().await;
        cells.map_err(Into::into)
    }
}

/// Serve configured repositories and compiled React assets until shutdown.
pub async fn serve(config: Config) -> Result<()> {
    config.validate()?;
    let catalog = CatalogStore::from_config(&config)?;
    catalog.flush_membership_audit().await?;
    let (document, _) = catalog.load().await?;
    let auth = match config.auth.clone() {
        Some(config) => Some(
            Authentication::new_durable(config, catalog.root())
                .await
                .map_err(|source| crate::Error::Identity {
                    source: Box::new(source),
                })?,
        ),
        None => None,
    };
    let catalog_version = document.version;
    let repository_cells = document
        .repositories
        .iter()
        .filter(|record| {
            record.application == crate::catalog::RepositoryApplicationState::CellReady
        })
        .map(|record| (record.id, record.application))
        .collect::<Vec<_>>();
    let repositories = materialize_catalog(&catalog, document).await?;
    let runtime = Arc::new(RemoteGitRuntime::default());
    let cancellation = CancellationToken::new();
    let options = RepositoryOptions::new(
        Default::default(),
        OperationLimits {
            // Graph-backed batches keep deep blame bounded while covering
            // first-parent histories of Kubernetes-scale repositories.
            max_duration: Duration::from_secs(5 * 60),
            max_logical_objects: 5_000_000,
            max_storage_requests: 6_000_000,
            max_fetched_bytes: 4 * 1024 * 1024 * 1024,
            max_inflated_bytes: 4 * 1024 * 1024 * 1024,
            max_entries: 2_000_000,
            max_history_commits: 75_000,
            max_blame_comparison_cells: 64_000_000,
            max_response_bytes: 8 * 1024 * 1024,
            ..Default::default()
        },
    )?;
    let transfer_admission = transfer_admission(&catalog);
    let startup = crate::cells::verify_startup_release(&config).await?;
    crate::cells::verify_repository_cells(&startup.layout, startup.identity, repository_cells)
        .await?;
    let peer_tls = crate::peer_tls::LoadedPeerTls::load(&config.cells)?;
    let session = SessionId::from_bytes(Uuid::now_v7().into_bytes());
    let registry = Arc::new(startup.registry);
    // Keep node-directory CAS traffic on a separate HTTP client from bulk Git
    // object writes. A large import must not starve the lease heartbeat until
    // the ten-second advertisement expires and fences the whole process.
    let control_root = crate::storage_root::StorageRoot::build(&config.storage)?;
    let control_layout = ApplicationIdentityStore::new(
        control_root.store.clone(),
        ObjectPath::from(control_root.prefix.clone()),
    )
    .layout(startup.identity)
    .await?;
    let directory = NodeDirectory::new(
        control_layout,
        peer_tls.fleet(),
        startup.image,
        registry.release_digest(),
    );
    let scheduler_status = crate::cells::SchedulerStatus::new(crate::cells::unix_now_ms()?)?;
    let node_publisher = crate::peer::NodePublisher::new(
        directory.clone(),
        peer_tls.signing_key().clone(),
        session,
        config.cells.peer_advertise.to_string(),
        crab_cell_runtime::NodeFailureDomain::new(
            config.cells.failure_zone.clone(),
            config.cells.failure_host.clone(),
        )?,
        peer_tls.fleet(),
        peer_tls.certificate(),
        startup.image,
        registry.release_digest(),
        registry.module_digests(),
        config.cells.data_dir.clone(),
        config.cells.local_disk_limit_bytes,
        scheduler_status.clone(),
    )?;
    let node = node_publisher.node();
    let session_dir = node_publisher.session_dir();
    let local_resources = node_publisher.local_resources()?;
    let cell_budget = CellRuntimeBudget::from_resources(local_resources)?;
    let cell_capacity = CellCapacityReport::new(local_resources, cell_budget);
    let local_disk = cell_budget.local_disk();
    let local_staging = crate::local_disk::LocalStaging::new_with_restart_inventory(
        session_dir.join("transfers"),
        local_disk.clone(),
        cell_budget.disk_reserve_bytes,
        &config.cells.data_dir,
        &session_dir,
    )
    .map_err(|source| crate::Error::LocalStaging {
        source: Box::new(source),
    })?;
    let listener = tokio::net::TcpListener::bind(config.listen).await?;
    let public_address = listener.local_addr()?;
    let management_listener = tokio::net::TcpListener::bind(config.management_listen).await?;
    let metrics = crate::metrics::Metrics::new()?;
    let node_shutdown = CancellationToken::new();
    let cell_node = Arc::new(
        CellNodeBuilder::new(crate::cells::compiled_application()?)
            .with_runtime(
                SqlWorkerPool::for_system(cell_budget.max_active_cells)?,
                cell_budget.node_retained_bytes,
            )
            .with_replica_host(cell_budget.replica_host(local_disk.clone(), session_dir.clone()))
            .with_session(session)
            .build()?,
    );
    let cell_tasks = cell_node.install_task_group(cancellation.clone(), node_shutdown.clone())?;
    cell_node.install_telemetry(Arc::new(metrics.clone()))?;
    let cell_runtime = cell_node.runtime();
    let follower_store = crab_cell_runtime::FollowerStore::open(
        config.cells.data_dir.clone(),
        crate::cells::repository_replica_limits(),
        local_disk.clone(),
    )?;
    if follower_store.quarantined_entries() != 0 {
        tracing::warn!(
            entries = follower_store.quarantined_entries(),
            "corrupt follower storage remains quarantined"
        );
    }
    let node_publisher = Arc::new(
        node_publisher
            .with_follower_store(follower_store.clone())
            .with_runtime(cell_runtime.clone())
            .with_telemetry(cell_runtime.telemetry_handle())
            .with_metrics(metrics.clone()),
    );
    let cell_resolver = crate::peer::LocalCellResolver::new(
        startup.layout.clone(),
        startup.identity,
        cell_runtime.clone(),
    );
    let peer_round_trip: Arc<dyn PeerRoundTrip> = Arc::new(crate::peer::PeerHttpRoundTrip::new(
        startup.identity,
        crab_cell_runtime::CellAuthority::new(startup.layout.clone()),
        directory.clone(),
        peer_tls.client_identity(),
        session,
    ));
    let node_log_transport: Arc<dyn crab_cell_runtime::NodeLogTransport> = Arc::new(
        crate::peer::NodeLogHttpTransport::new(
            directory.clone(),
            peer_tls.client_identity(),
            session,
        )
        .with_local_follower(node, follower_store.clone()),
    );
    let recovery_artifacts = Arc::new(crate::cells::RecoveryArtifactRegistry::new(
        session_dir.join("recovery-artifacts"),
        crate::cells::repository_replica_limits(),
        local_disk.clone(),
    )?);
    let release_store = ReleaseStore::new(startup.layout.clone(), startup.identity)?;
    let peer_receiver = crate::peer::PeerReceiver::new(
        node,
        session,
        directory.clone(),
        Arc::clone(&registry),
        release_store.clone(),
        cell_resolver,
        Arc::clone(&peer_round_trip),
    );
    let repository_cells = crate::cells::RepositoryCellRouter::new(
        startup.identity,
        startup.layout.clone(),
        Arc::clone(&registry),
        cell_runtime.clone(),
        crate::cells::RepositoryCellPeer::new(
            directory.clone(),
            Arc::new(PeerSigner::new(
                session,
                registry.release_digest(),
                peer_tls.signing_key().clone(),
            )),
            peer_round_trip,
            Owner {
                session,
                endpoint: config.cells.peer_advertise.to_string(),
            },
        ),
        session_dir,
    )?
    .with_recovery_artifacts(Arc::clone(&recovery_artifacts));
    let cell_scheduler = crate::cells::RepositoryCellScheduler::new(
        startup.identity,
        startup.layout,
        directory.clone(),
        repository_cells.clone(),
        session,
        scheduler_status.clone(),
    )?
    .with_node_recovery(Arc::clone(&node_log_transport))
    .with_node(node)
    .with_node_recovery_disk(local_disk.clone())
    .with_metrics(metrics.clone());
    cell_node.install_owned_component(
        CELL_COMPONENT_REPOSITORY_ROUTER,
        Arc::new(repository_cells.clone()),
    )?;
    cell_node.install_owned_component(
        CELL_COMPONENT_PEER_RECEIVER,
        Arc::new(peer_receiver.clone()),
    )?;
    cell_node.install_owned_component(
        CELL_COMPONENT_FOLLOWER_STORE,
        Arc::new(follower_store.clone()),
    )?;
    cell_node.install_owned_component(
        CELL_COMPONENT_NODE_LOG_TRANSPORT,
        Arc::new(node_log_transport.clone()),
    )?;
    cell_node
        .install_owned_component(CELL_COMPONENT_NODE_PUBLISHER, Arc::clone(&node_publisher))?;
    cell_node.install_owned_component(CELL_COMPONENT_CATALOG, Arc::new(catalog.clone()))?;
    cell_node.install_owned_component(
        CELL_COMPONENT_SCHEDULER_STATUS,
        Arc::new(scheduler_status.clone()),
    )?;
    let durability_application = startup.identity.application();
    let server = Arc::new(Server {
        repositories: repositories.into(),
        runtime: Arc::clone(&runtime),
        #[cfg(test)]
        cell_runtime: cell_runtime.clone(),
        cell_node: Some(Arc::clone(&cell_node)),
        #[cfg(test)]
        repository_cells: None,
        #[cfg(test)]
        peer_receiver: None,
        #[cfg(test)]
        follower_store: None,
        #[cfg(test)]
        node_log_transport: None,
        cancellation: cancellation.clone(),
        receives: tokio_util::task::TaskTracker::new(),
        options,
        cursor_key: auth
            .as_ref()
            .map(Authentication::cursor_key)
            .unwrap_or_else(rand::random),
        admission: Semaphore::new(READ_ADMISSION_CAPACITY),
        transfer_admission,
        local_staging,
        app_admission: Semaphore::new(APP_ADMISSION_CAPACITY),
        maintenance_admission: Arc::new(Semaphore::new(MAINTENANCE_ADMISSION_CAPACITY)),
        auth,
        git_import: Some(git_import::ImportContext::new(
            config.clone(),
            public_address,
        )),
        #[cfg(test)]
        catalog: None,
        catalog_healthy: AtomicBool::new(true),
        node_healthy: AtomicBool::new(false),
        #[cfg(test)]
        scheduler_status,
        cell_capacity,
        metrics,
    });
    let management = management_router(Arc::clone(&server));
    tracing::info!(address = %management_listener.local_addr()?, "management listener started");
    let recovery_shutdown = CancellationToken::new();
    let management_shutdown = recovery_shutdown.clone();
    let mut management_listener = tokio::spawn(async move {
        axum::serve(
            peer_tls.listener(management_listener),
            management.into_make_service_with_connect_info::<crate::peer_tls::PeerTlsIdentity>(),
        )
        .with_graceful_shutdown(management_shutdown.cancelled_owned())
        .await
    });
    let startup_result = {
        let startup = async {
            // Recovery is reachable before the comprehensive object-store probe.
            // Application peer routes remain gated until all startup tasks exist.
            probe_storage_contract(&catalog, &server.transfer_admission).await?;
            node_publisher.publish_initial().await?;
            let node_lease = node_publisher.lease_guard()?;
            cell_node.install_node_lease_for_startup(node_lease.clone())?;
            Ok::<_, crate::Error>(node_lease)
        };
        tokio::pin!(startup);
        tokio::select! {
            result = &mut startup => result,
            result = &mut management_listener => {
                cancellation.cancel();
                let listener_error = match listener_task_result(result) {
                    Ok(()) => crate::Error::Config("management listener stopped during startup"),
                    Err(error) => error,
                };
                if let Err(shutdown_error) = server.shutdown_runtimes().await {
                    tracing::warn!(error = %shutdown_error, "runtime startup cleanup failed");
                }
                return Err(listener_error);
            }
        }
    };
    let node_lease = match startup_result {
        Ok(node_lease) => node_lease,
        Err(error) => {
            cancellation.cancel();
            recovery_shutdown.cancel();
            if tokio::time::timeout(Duration::from_secs(5), &mut management_listener)
                .await
                .is_err()
            {
                management_listener.abort();
                let _ = management_listener.await;
            }
            if let Err(shutdown_error) = server.shutdown_runtimes().await {
                tracing::warn!(error = %shutdown_error, "runtime startup cleanup failed");
            }
            return Err(error);
        }
    };
    let signal_cancellation = cancellation.clone();
    cell_tasks.spawn(async move {
        tokio::select! {
            () = shutdown_signal() => signal_cancellation.cancel(),
            () = signal_cancellation.cancelled() => {}
        }
        Ok::<(), crate::Error>(())
    })?;
    let refresh_server = Arc::clone(&server);
    cell_tasks.spawn(async move {
        refresh_catalog(refresh_server, catalog_version).await;
        Ok::<(), crate::Error>(())
    })?;
    let projection_sweep_server = Arc::clone(&server);
    cell_tasks.spawn(async move { sweep_projections(projection_sweep_server).await })?;
    let durability_publisher = Arc::clone(&node_publisher);
    let durability_runtime = server.cell_runtime()?;
    let durability_transport = server
        .node_log_transport()
        .ok_or(crate::Error::Config("node-log transport is unavailable"))?;
    let durability_cancellation = cancellation.clone();
    cell_tasks.spawn(async move {
        recruit_node_durability(
            durability_publisher,
            durability_runtime,
            durability_application,
            durability_transport,
            durability_cancellation,
        )
        .await
    })?;
    let rotation_publisher = Arc::clone(&node_publisher);
    let rotation_runtime = server.cell_runtime()?;
    let rotation_transport = server
        .node_log_transport()
        .ok_or(crate::Error::Config("node-log transport is unavailable"))?;
    let rotation_cancellation = cancellation.clone();
    let rotation_metrics = server.metrics.clone();
    cell_tasks.spawn(async move {
        rotate_node_durability(
            rotation_publisher,
            rotation_runtime,
            durability_application,
            rotation_transport,
            rotation_cancellation,
            rotation_metrics,
        )
        .await
    })?;
    let follower_collection_store = follower_store;
    let follower_collection_directory = directory.clone();
    let follower_collection_cancellation = cancellation.clone();
    cell_tasks.spawn(async move {
        collect_retired_follower_lanes(
            follower_collection_store,
            follower_collection_directory,
            follower_collection_cancellation,
        )
        .await
    })?;
    let node_server = Arc::clone(&server);
    let heartbeat_shutdown = node_shutdown.clone();
    let heartbeat_publisher = Arc::clone(&node_publisher);
    cell_tasks.spawn(async move {
        heartbeat_publisher
            .run_shared(node_server, heartbeat_shutdown)
            .await
    })?;
    let lease_server = Arc::clone(&server);
    let lease_cancellation = cancellation.clone();
    let lease_watch = async move {
        tokio::select! {
            () = node_lease.wait_fenced() => {
                if lease_server.node_healthy.load(Ordering::Acquire) {
                    lease_server
                        .metrics
                        .record_self_fence(crate::metrics::SelfFenceReason::Expiry);
                } else if lease_cancellation.is_cancelled() {
                    lease_server
                        .metrics
                        .record_self_fence(crate::metrics::SelfFenceReason::Shutdown);
                }
                lease_server.node_healthy.store(false, Ordering::Release);
                lease_cancellation.cancel();
            }
            () = lease_cancellation.cancelled() => {
                lease_server
                    .metrics
                    .record_self_fence(crate::metrics::SelfFenceReason::Shutdown);
                lease_server.node_healthy.store(false, Ordering::Release);
            }
        }
        Ok::<(), crate::Error>(())
    };
    cell_tasks.spawn(lease_watch)?;
    let release_cancellation = cancellation.clone();
    let compiled_release = registry.release_digest();
    let release_watch =
        async move { watch_release(release_store, compiled_release, release_cancellation).await };
    cell_tasks.spawn(release_watch)?;
    let scheduler_cancellation = cancellation.clone();
    cell_tasks.spawn(async move { cell_scheduler.run(scheduler_cancellation).await })?;
    cell_node.start()?;
    server.node_healthy.store(true, Ordering::Release);
    let app = router(Arc::clone(&server));
    tracing::info!(address = %listener.local_addr()?, "public listener started");
    let public_shutdown = cancellation.clone();
    let public = async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(public_shutdown.cancelled_owned())
            .await
    };
    tokio::pin!(public);
    let mut management_result = None;
    let (public_result, shutdown_deadline) = tokio::select! {
        result = &mut public => {
            server.node_healthy.store(false, Ordering::Release);
            cancellation.cancel();
            (Ok(result), Instant::now() + SHUTDOWN_DEADLINE)
        }
        result = &mut management_listener => {
            server.node_healthy.store(false, Ordering::Release);
            cancellation.cancel();
            management_result = Some(match listener_task_result(result) {
                Ok(()) => Err(crate::Error::Config("management listener stopped unexpectedly")),
                Err(error) => Err(error),
            });
            let deadline = Instant::now() + SHUTDOWN_DEADLINE;
            (before_shutdown_deadline(deadline, &mut public).await, deadline)
        }
        () = cancellation.cancelled() => {
            server.node_healthy.store(false, Ordering::Release);
            let deadline = Instant::now() + SHUTDOWN_DEADLINE;
            (before_shutdown_deadline(deadline, &mut public).await, deadline)
        }
    };
    let result = public_result?;
    before_shutdown_deadline(shutdown_deadline, async move {
        // Axum has drained its connections, so no handler can register a new
        // receive after the tracker becomes empty. Close readers only after that drain.
        server.cancellation.cancel();
        server.receives.close();
        server.receives.wait().await;
        server.transfer_admission.close();
        server.transfer_admission.wait().await;
        let maintenance = server.finish_maintenance().await;
        let runtimes = server
            .shutdown_runtimes_until(Some(shutdown_deadline))
            .await;
        recovery_shutdown.cancel();
        let management = match management_result {
            Some(result) => result,
            None => listener_task_result(management_listener.await),
        };
        result
            .map(|_| ())
            .map_err(crate::Error::from)
            .and(management)
            .and(maintenance)
            .and(runtimes)
    })
    .await?
}

fn listener_task_result(
    result: std::result::Result<std::io::Result<()>, tokio::task::JoinError>,
) -> Result<()> {
    result.map_err(crate::Error::from)?.map_err(Into::into)
}

async fn collect_retired_follower_lanes(
    store: crab_cell_runtime::FollowerStore,
    directory: NodeDirectory,
    cancellation: CancellationToken,
) -> Result<()> {
    loop {
        tokio::select! {
            () = cancellation.cancelled() => return Ok(()),
            () = tokio::time::sleep(RETIRED_FOLLOWER_COLLECTION_INTERVAL) => {}
        }
        let now_ms = crate::cells::unix_now_ms()?;
        let cutoff_ms = now_ms.saturating_sub(RETIRED_FOLLOWER_GRACE_MS);
        let candidates = match store.retired_lanes(cutoff_ms, RETIRED_FOLLOWER_BATCH).await {
            Ok(candidates) => candidates,
            Err(error) => {
                tracing::warn!(error = %error, "retired follower scan failed");
                continue;
            }
        };
        for candidate in candidates {
            match directory
                .log_epoch_referenced(candidate.leader(), candidate.epoch())
                .await
            {
                Ok(true) => {}
                Ok(false) => {
                    if let Err(error) = store.remove_retired(candidate, cutoff_ms).await {
                        tracing::warn!(error = %error, "retired follower collection failed");
                    }
                }
                Err(error) => {
                    tracing::warn!(error = %error, "retired follower authority check failed");
                }
            }
        }
    }
}

async fn recruit_node_durability(
    publisher: Arc<crate::peer::NodePublisher>,
    runtime: CellRuntime,
    application: crab_cell_runtime::ApplicationId,
    transport: Arc<dyn crab_cell_runtime::NodeLogTransport>,
    cancellation: CancellationToken,
) -> Result<()> {
    let limits = crate::cells::repository_replica_limits();
    loop {
        let recruited = publisher
            .recruit_node_durability(
                Arc::clone(&transport),
                limits,
                limits.max_capture_bytes,
                NODE_LOG_LIVE_NODE_LIMIT,
            )
            .await;
        match recruited {
            Ok(Some(durability)) => {
                runtime.install_node_durability(application, durability)?;
                tracing::info!("node-log follower durability recruited");
                return Ok(());
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(error = %error, "node-log follower recruitment failed");
            }
        }
        tokio::select! {
            () = cancellation.cancelled() => return Ok(()),
            () = tokio::time::sleep(NODE_LOG_RECRUIT_INTERVAL) => {}
        }
    }
}

async fn rotate_node_durability(
    publisher: Arc<crate::peer::NodePublisher>,
    runtime: CellRuntime,
    application: crab_cell_runtime::ApplicationId,
    transport: Arc<dyn crab_cell_runtime::NodeLogTransport>,
    cancellation: CancellationToken,
    metrics: crate::metrics::Metrics,
) -> Result<()> {
    let limits = crate::cells::repository_replica_limits();
    loop {
        tokio::select! {
            () = cancellation.cancelled() => return Ok(()),
            () = tokio::time::sleep(NODE_LOG_ROTATION_INTERVAL) => {}
        }
        let Some((installed_application, durability)) = runtime.node_durability() else {
            continue;
        };
        if installed_application != application {
            return Err(crate::Error::Config(
                "node-log durability application changed during rotation",
            ));
        }
        if !durability.needs_rotation(NODE_LOG_ROTATION_FRAMES) {
            continue;
        }
        metrics.record_node_log_rotation(crate::metrics::NodeLogRotationResult::Started);
        loop {
            match durability.shutdown().await {
                Ok(()) => break,
                Err(crab_cell_runtime::Error::PendingPublication) => {
                    metrics
                        .record_node_log_rotation(crate::metrics::NodeLogRotationResult::Pending);
                    tokio::select! {
                        () = cancellation.cancelled() => return Ok(()),
                        () = tokio::time::sleep(NODE_LOG_RECRUIT_INTERVAL) => {}
                    }
                }
                Err(error) => {
                    metrics.record_node_log_rotation(crate::metrics::NodeLogRotationResult::Failed);
                    return Err(crate::Error::Cell(error));
                }
            }
        }
        if cancellation.is_cancelled() {
            return Ok(());
        }
        let replacement = loop {
            match publisher
                .recruit_node_durability(
                    Arc::clone(&transport),
                    limits,
                    limits.max_capture_bytes,
                    NODE_LOG_LIVE_NODE_LIMIT,
                )
                .await
            {
                Ok(Some(durability)) => break durability,
                Ok(None) => {}
                Err(error) => {
                    metrics.record_node_log_rotation(crate::metrics::NodeLogRotationResult::Failed);
                    tracing::warn!(error = %error, "node-log epoch rotation recruitment failed");
                }
            }
            tokio::select! {
                () = cancellation.cancelled() => return Ok(()),
                () = tokio::time::sleep(NODE_LOG_RECRUIT_INTERVAL) => {}
            }
        };
        if cancellation.is_cancelled() {
            replacement.shutdown().await?;
            return Ok(());
        }
        if let Err(error) = runtime.replace_node_durability(application, Arc::clone(&replacement)) {
            metrics.record_node_log_rotation(crate::metrics::NodeLogRotationResult::Failed);
            replacement.shutdown().await?;
            return Err(crate::Error::Cell(error));
        }
        metrics.record_node_log_rotation(crate::metrics::NodeLogRotationResult::Completed);
        tracing::info!("node-log epoch rotated after object coverage");
    }
}

/// Validate the durable catalog and the storage coordination write path.
///
/// # Errors
///
/// Returns the original configuration, storage, catalog, or coordination
/// error when the configured workload cannot satisfy the server contract.
pub async fn probe_storage(config: &Config) -> Result<()> {
    let catalog = CatalogStore::from_config(config)?;
    catalog.load().await?;
    probe_storage_contract(&catalog, &transfer_admission(&catalog)).await
}

pub(crate) async fn materialize_catalog(
    catalog: &CatalogStore,
    document: crate::catalog::CatalogDocument,
) -> Result<BTreeMap<(String, String), Arc<Repository>>> {
    let mut repositories = BTreeMap::new();
    for record in document.repositories.into_iter().filter(|record| {
        record.application == crate::catalog::RepositoryApplicationState::CellReady
    }) {
        let store = catalog.root().store.clone();
        let prefix = catalog.root().repository_prefix(&record.prefix)?;
        let layout = StoreLayout::new(store.clone(), prefix.clone());
        let (manifest, _) =
            read_manifest(&store, &layout)
                .await
                .map_err(|source| crate::Error::Settings {
                    source: Box::new(source),
                })?;
        let default_branch =
            manifest
                .head
                .strip_prefix("refs/heads/")
                .ok_or(crate::Error::Config(
                    "catalog repository HEAD must name a branch",
                ))?;
        let entry = record.runtime_config(catalog.root(), default_branch)?;
        let repository = Repository {
            id: record.id,
            layout,
            identity: RepositoryIdentity::new(
                catalog.root().provider_namespace.clone(),
                prefix,
                record.placement_generation,
            )?,
            config: entry.clone(),
            store,
            pinned: Mutex::new(None),
            maintenance: Mutex::new(None),
        };
        repositories.insert(
            (entry.owner.clone(), entry.name.clone()),
            Arc::new(repository),
        );
    }
    Ok(repositories)
}

async fn refresh_catalog(server: Arc<Server>, mut version: u64) {
    let Some(catalog) = server.catalog() else {
        return;
    };
    loop {
        tokio::select! {
            () = server.cancellation.cancelled() => return,
            () = tokio::time::sleep(Duration::from_secs(5)) => {}
        }
        let (document, _) = match catalog.load().await {
            Ok(value) => value,
            Err(error) => {
                server.catalog_healthy.store(false, Ordering::Release);
                server.metrics.record_catalog_refresh_failure();
                tracing::warn!(error = ?error, "repository catalog refresh failed");
                continue;
            }
        };
        if let Err(error) = catalog.flush_membership_audit().await {
            tracing::warn!(error = ?error, "membership audit flush failed");
        }
        if document.version < version {
            server.catalog_healthy.store(false, Ordering::Release);
            server.metrics.record_catalog_refresh_failure();
            tracing::warn!(
                catalog_version = document.version,
                active_version = version,
                "repository catalog version moved backwards"
            );
            continue;
        }
        if document.version == version {
            server.catalog_healthy.store(true, Ordering::Release);
            continue;
        }
        let repository_cells = document
            .repositories
            .iter()
            .filter(|record| {
                record.application == crate::catalog::RepositoryApplicationState::CellReady
            })
            .map(|record| (record.id, record.application))
            .collect::<Vec<_>>();
        let verified = match server.repository_cells() {
            Some(router) => router.verify_repositories(repository_cells).await,
            None => Err(crate::Error::Config(
                "repository catalog refresh requires Cell routing",
            )),
        };
        if let Err(error) = verified {
            server.catalog_healthy.store(false, Ordering::Release);
            server.metrics.record_catalog_refresh_failure();
            tracing::warn!(error = ?error, "repository catalog Cell readiness failed");
            continue;
        }
        let next_version = document.version;
        match materialize_catalog(&catalog, document).await {
            Ok(repositories) => {
                server.repositories.replace(repositories);
                version = next_version;
                server.catalog_healthy.store(true, Ordering::Release);
                tracing::info!(catalog_version = version, "repository catalog refreshed");
            }
            Err(error) => {
                server.catalog_healthy.store(false, Ordering::Release);
                server.metrics.record_catalog_refresh_failure();
                tracing::warn!(error = ?error, "repository catalog materialization failed");
            }
        }
    }
}

struct ProjectionSweepState {
    source_token: String,
    next_attempt: Instant,
    retry_delay: Duration,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MaintenanceSchedule {
    Started,
    Running,
    Completed,
    Deferred,
}

impl ProjectionSweepState {
    fn record_schedule(&mut self, now: Instant, schedule: MaintenanceSchedule) {
        if schedule == MaintenanceSchedule::Deferred {
            self.next_attempt = now + PROJECTION_SWEEP_INTERVAL;
            return;
        }
        self.next_attempt = now + self.retry_delay;
        self.retry_delay = (self.retry_delay * 2).min(PROJECTION_RETRY_MAX);
    }
}

fn next_projection_sweep_cursor(start: usize, count: usize, repository_count: usize) -> usize {
    let advance = if count == repository_count { 1 } else { count };
    start.saturating_add(advance) % repository_count
}

async fn sweep_projections(server: Arc<Server>) -> Result<()> {
    let mut cursor = 0_usize;
    let mut states = HashMap::<Uuid, ProjectionSweepState>::new();
    loop {
        tokio::select! {
            () = server.cancellation.cancelled() => return Ok(()),
            () = tokio::time::sleep(PROJECTION_SWEEP_INTERVAL) => {}
        }
        let repositories = server.repositories.values();
        if repositories.is_empty() {
            continue;
        }
        let start = cursor % repositories.len();
        let count = repositories.len().min(PROJECTION_SWEEP_BATCH);
        for offset in 0..count {
            let repository = Arc::clone(&repositories[(start + offset) % repositories.len()]);
            let now = Instant::now();
            let probe_started = now;
            let source = match crate::projection::current_source(
                &repository.store,
                &repository.layout,
            )
            .await
            {
                Ok(source) => source,
                Err(error) => {
                    server.metrics.record_projection_probe(
                        crate::metrics::ProjectionProbeResult::Error,
                        probe_started.elapsed(),
                    );
                    tracing::warn!(
                        repository_id = %repository.id,
                        error = %error,
                        "Git projection source probe failed"
                    );
                    if let Err(schedule_error) = repository.schedule_maintenance(&server).await {
                        tracing::warn!(
                            repository_id = %repository.id,
                            error = %schedule_error,
                            "Git projection repair could not be scheduled"
                        );
                    }
                    continue;
                }
            };
            let state = states
                .entry(repository.id)
                .or_insert_with(|| ProjectionSweepState {
                    source_token: String::new(),
                    next_attempt: now,
                    retry_delay: PROJECTION_RETRY_BASE,
                });
            if state.source_token != source.source_token {
                server.metrics.record_projection_probe(
                    crate::metrics::ProjectionProbeResult::Changed,
                    probe_started.elapsed(),
                );
                server.metrics.update_projection_state(false, 0, 0.0, 0);
                state.source_token = source.source_token.clone();
                state.next_attempt = now;
                state.retry_delay = PROJECTION_RETRY_BASE;
            } else {
                server.metrics.record_projection_probe(
                    crate::metrics::ProjectionProbeResult::Ready,
                    probe_started.elapsed(),
                );
            }
            if now < state.next_attempt {
                continue;
            }
            match repository.schedule_maintenance(&server).await {
                Ok(schedule) => state.record_schedule(now, schedule),
                Err(error) => {
                    tracing::warn!(
                        repository_id = %repository.id,
                        error = %error,
                        "Git projection maintenance scheduling failed"
                    );
                    state.next_attempt = now + state.retry_delay;
                    state.retry_delay = (state.retry_delay * 2).min(PROJECTION_RETRY_MAX);
                }
            }
        }
        cursor = next_projection_sweep_cursor(start, count, repositories.len());
        states.retain(|repository_id, _| server.repositories.by_id(*repository_id).is_some());
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(signal) => signal,
                Err(error) => {
                    eprintln!("Unable to listen for SIGTERM: {error}");
                    if let Err(error) = tokio::signal::ctrl_c().await {
                        eprintln!("Unable to listen for Ctrl-C: {error}");
                    }
                    return;
                }
            };
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                if let Err(error) = result {
                    eprintln!("Unable to listen for Ctrl-C: {error}");
                }
            }
            _ = terminate.recv() => {}
        }
    }
    #[cfg(not(unix))]
    if let Err(error) = tokio::signal::ctrl_c().await {
        eprintln!("Unable to listen for Ctrl-C: {error}");
    }
}

pub(crate) fn router(server: Arc<Server>) -> Router {
    Router::new()
        .merge(crate::members::routes())
        .merge(assignees::routes(Arc::clone(&server)))
        .merge(branches::routes())
        .merge(checks::routes(Arc::clone(&server)))
        .merge(contents::routes())
        .merge(issues::routes(Arc::clone(&server)))
        .merge(labels::routes(Arc::clone(&server)))
        .merge(releases::routes(Arc::clone(&server)))
        .merge(pulls::routes(Arc::clone(&server)))
        .merge(statuses::routes(Arc::clone(&server)))
        .merge(git_import::routes(Arc::clone(&server)))
        .route(
            "/git/{owner}/{name}/info/lfs/objects/batch",
            post(lfs::batch).layer(axum::extract::DefaultBodyLimit::max(64 * 1024)),
        )
        .route(
            "/git/{owner}/{name}/info/lfs/objects/{oid}",
            get(lfs::download).put(lfs::upload),
        )
        .route(
            "/git/{owner}/{name}/info/lfs/locks",
            get(lfs::list_locks)
                .post(lfs::create_lock)
                .layer(axum::extract::DefaultBodyLimit::max(16 * 1024)),
        )
        .route(
            "/git/{owner}/{name}/info/lfs/locks/verify",
            post(lfs::verify_locks).layer(axum::extract::DefaultBodyLimit::max(16 * 1024)),
        )
        .route(
            "/git/{owner}/{name}/info/lfs/locks/{id}/unlock",
            post(lfs::unlock_lock).layer(axum::extract::DefaultBodyLimit::max(16 * 1024)),
        )
        .route("/git/{owner}/{name}/info/refs", get(git::advertise))
        .route(
            "/git/{owner}/{name}/git-receive-pack",
            post(receive::receive),
        )
        .route(
            "/git/{owner}/{name}/git-upload-pack",
            post(git::upload_pack).layer(axum::extract::DefaultBodyLimit::max(git::MAX_BODY_BYTES)),
        )
        .route(
            "/api/git-token",
            post(auth::issue_git_token)
                .delete(auth::revoke_git_tokens)
                .layer(axum::extract::DefaultBodyLimit::max(2048)),
        )
        .route("/api/session", get(auth::session))
        .route("/auth/login", get(auth::login))
        .route("/auth/callback", get(auth::callback))
        .route("/auth/logout", post(auth::logout))
        .route(
            "/auth/backchannel-logout",
            post(auth::backchannel_logout).layer(axum::extract::DefaultBodyLimit::max(64 * 1024)),
        )
        .route("/livez", get(|| async { Json(json!({"status": "ok"})) }))
        .route("/api/repos", get(catalog))
        .route("/api/repos/{owner}/{name}/archive", get(archive::download))
        .route("/api/repos/{owner}/{name}/{action}", get(api::read))
        .fallback(assets::serve)
        .layer(middleware::from_fn_with_state(
            Arc::clone(&server),
            boundary,
        ))
        .with_state(server)
}

fn management_router(server: Arc<Server>) -> Router {
    let application = Router::new()
        .route(
            "/internal/cells/v1/forward",
            post(crate::peer::forward).layer(axum::extract::DefaultBodyLimit::max(
                crab_cell_runtime::MAX_PEER_REQUEST_BYTES,
            )),
        )
        .route(
            "/internal/cells/v1/node-log/{leader}/{epoch}/append",
            post(crate::peer::append_node_log).layer(axum::extract::DefaultBodyLimit::max(
                (65 * 1024 * 1024) + (64 * 8) + 8,
            )),
        )
        .route(
            "/internal/cells/v1/node-log/{leader}/{epoch}/retire/{covered_through}",
            post(crate::peer::retire_node_log).layer(axum::extract::DefaultBodyLimit::max(0)),
        )
        .route_layer(middleware::from_fn_with_state(
            Arc::clone(&server),
            application_peer_admission,
        ));
    let recovery = Router::new()
        .route(
            "/internal/cells/v1/node-log/{leader}/{epoch}/recovery/{claimant}/seal",
            post(crate::peer::seal_node_log).layer(axum::extract::DefaultBodyLimit::max(0)),
        )
        .route(
            "/internal/cells/v1/node-log/{leader}/{epoch}/recovery/{claimant}/tail/{first}",
            post(crate::peer::tail_node_log).layer(axum::extract::DefaultBodyLimit::max(0)),
        );
    Router::new()
        .route("/healthz", get(|| async { Json(json!({"status": "ok"})) }))
        .route("/readyz", get(readiness))
        .route("/capacity", get(render_capacity))
        .route("/metrics", get(render_metrics))
        .merge(application)
        .merge(recovery)
        .with_state(server)
}

async fn application_peer_admission(
    State(server): State<Arc<Server>>,
    request: Request,
    next: Next,
) -> Response {
    if !server.accepts_application_peers() {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    next.run(request).await
}

async fn render_capacity(State(server): State<Arc<Server>>) -> Response {
    (
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        Json(server.cell_capacity.clone()),
    )
        .into_response()
}

async fn render_metrics(State(server): State<Arc<Server>>) -> Response {
    let scheduler_now_ms = crate::cells::unix_now_ms().unwrap_or(0);
    let cell_runtime = match server.cell_runtime() {
        Ok(runtime) => runtime.stats(),
        Err(error) => {
            tracing::error!(error = %error, "Cell runtime host is unavailable for metrics");
            return StatusCode::SERVICE_UNAVAILABLE.into_response();
        }
    };
    let body = server.metrics.render(
        crate::metrics::RuntimeSnapshot {
            repositories: server.repositories.len(),
            catalog_healthy: server.catalog_healthy.load(Ordering::Acquire),
            scheduler_healthy: server
                .scheduler_status()
                .is_some_and(|status| status.is_healthy(scheduler_now_ms)),
            scheduler_progress: server
                .scheduler_status()
                .map_or(0, |status| status.progress()),
            scheduler_lag_seconds: server
                .scheduler_status()
                .map_or(0, |status| status.lag_ms(scheduler_now_ms))
                as f64
                / 1_000.0,
            draining: server.cancellation.is_cancelled(),
            receive_workers: server.receives.len(),
            cell_follower_retained_bytes: server
                .follower_store()
                .map_or(0, |store| store.retained_bytes()),
            admission_available: [
                server.admission.available_permits(),
                server.transfer_admission.available_permits(),
                server.app_admission.available_permits(),
                server.maintenance_admission.available_permits(),
            ],
            admission_capacity: [
                READ_ADMISSION_CAPACITY,
                GIT_ADMISSION_CAPACITY,
                APP_ADMISSION_CAPACITY,
                MAINTENANCE_ADMISSION_CAPACITY,
            ],
            ..crate::metrics::RuntimeSnapshot::default()
        }
        .with_cell_runtime(cell_runtime),
    );
    (
        [
            (
                axum::http::header::CONTENT_TYPE,
                "text/plain; version=0.0.4; charset=utf-8",
            ),
            (axum::http::header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response()
}

async fn readiness(State(server): State<Arc<Server>>) -> Response {
    match tokio::time::timeout(Duration::from_secs(10), check_readiness(&server)).await {
        Ok(Ok(())) => Json(json!({"status":"ready"})).into_response(),
        Ok(Err(error)) => {
            tracing::warn!(error = ?error, "repository readiness check failed");
            readiness_unavailable()
        }
        Err(_) => {
            tracing::warn!("repository readiness check timed out");
            readiness_unavailable()
        }
    }
}

async fn check_readiness(server: &Server) -> Result<()> {
    if server.cancellation.is_cancelled() {
        return Err(crate::Error::Config("server is draining"));
    }
    if server.cell_runtime()?.is_shutting_down() {
        return Err(crate::Error::Config("embedded Cell runtime is draining"));
    }
    if server.catalog().is_some() && server.peer_receiver().is_none() {
        return Err(crate::Error::Config("Cell peer receiver is unavailable"));
    }
    if !server.accepts_application_peers() {
        return Err(crate::Error::Config("Cell node advertisement is unhealthy"));
    }
    let scheduler_status = server
        .scheduler_status()
        .ok_or(crate::Error::Config("Cell scheduler status is unavailable"))?;
    if !scheduler_status.is_healthy(crate::cells::unix_now_ms()?) {
        return Err(crate::Error::Config("Cell scheduler is unhealthy"));
    }
    if !server.catalog_healthy.load(Ordering::Acquire) {
        return Err(crate::Error::Config("catalog refresh is unhealthy"));
    }
    let catalog = server
        .catalog()
        .ok_or(crate::Error::Config("catalog is unavailable"))?;
    catalog.load().await?;
    for repository in server.repositories.values() {
        // A pod must not enter endpoint routing while a fresh process would
        // reject Git reads and trigger shared index maintenance on first use.
        repository
            .open_current(server, server.options, &server.cancellation)
            .await?;
    }
    Ok(())
}

fn readiness_unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [("retry-after", "5")],
        Json(json!({"status":"unavailable"})),
    )
        .into_response()
}

async fn catalog(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
) -> app::Result<Json<serde_json::Value>> {
    let actor = app::actor(&principal)?;
    let mut repositories = Vec::new();
    for repository in server
        .repositories
        .values()
        .into_iter()
        .filter(|repository| principal.can_read(&repository.config))
    {
        let protections = repository.branch_protections(&server, &actor).await?;
        let lifecycle = repository.lifecycle(&server, &actor).await?;
        repositories.push(json!({
            "owner": repository.config.owner, "name": repository.config.name,
            "description": repository.config.description,
            "access": if principal.can_write(&repository.config) { "write" } else { "read" },
            "can_admin": principal.can_admin(&repository.config),
            "protection_version": protections.version,
            "protected_branches": protections.rules,
            "archive_version": lifecycle.version,
            "archived": lifecycle.archived,
        }));
    }
    Ok(Json(json!({"repositories":repositories})))
}

async fn boundary(State(server): State<Arc<Server>>, request: Request, next: Next) -> Response {
    let request_id = Uuid::now_v7().to_string();
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let started = Instant::now();
    let observation = server.metrics.start_request(&method);
    let span = tracing::info_span!(
        "http_request",
        request_id = %request_id,
        method = %method,
        path = %path,
    );
    async move {
        let mut response = boundary_request(server, request, next).await;
        if let Ok(value) = axum::http::HeaderValue::from_str(&request_id) {
            response.headers_mut().insert("x-request-id", value);
        }
        tracing::info!(
            status = response.status().as_u16(),
            elapsed_ms = started.elapsed().as_millis(),
            "request completed"
        );
        let status = response.status();
        let (parts, body) = response.into_parts();
        Response::from_parts(
            parts,
            axum::body::Body::new(crate::metrics::ObservedBody::new(
                body,
                observation.response(status),
            )),
        )
    }
    .instrument(span)
    .await
}

async fn boundary_request(server: Arc<Server>, mut request: Request, next: Next) -> Response {
    let host = request
        .headers()
        .get("host")
        .and_then(|value| value.to_str().ok());
    let local_host = is_local_host(host);
    let internal_import = server
        .git_import
        .as_ref()
        .is_some_and(|context| context.authorizes_git(&request));
    let management_probe = matches!(request.uri().path(), "/healthz" | "/readyz");
    let load_balancer_probe = request.uri().path() == "/livez";
    let valid_host = load_balancer_probe
        || (management_probe && local_host)
        || internal_import
        || server
            .auth
            .as_ref()
            .map(|auth| auth.allows_host(host))
            .unwrap_or(local_host);
    if !valid_host {
        return StatusCode::FORBIDDEN.into_response();
    }
    let git_request = request.uri().path().starts_with("/git/");
    let integration_request = integration_api_path(request.uri().path());
    let token_request = integration_request && request.headers().contains_key("authorization");
    let principal = if internal_import {
        Principal::Local
    } else {
        match &server.auth {
            Some(auth) if git_request || token_request => {
                auth.git_principal(request.headers()).await
            }
            Some(auth) => auth.principal(request.headers()).await,
            None => Principal::Local,
        }
    };
    let protected =
        request.uri().path().starts_with("/api/") && request.uri().path() != "/api/session";
    // Membership's handler hides absent and non-admin repositories uniformly.
    // Only anonymous callers bypass generic login/CSRF responses; signed-in
    // mutations retain the normal Origin and session-CSRF requirements.
    let anonymous_membership = matches!(principal, Principal::Anonymous)
        && matches!(
            *request.method(),
            axum::http::Method::GET | axum::http::Method::HEAD | axum::http::Method::PUT
        )
        && request
            .uri()
            .path()
            .strip_prefix("/api/repos/")
            .is_some_and(|path| {
                let mut segments = path.split('/');
                matches!(
                    (segments.next(), segments.next(), segments.next(), segments.next()),
                    (Some(owner), Some(name), Some("members"), None)
                        if !owner.is_empty() && !name.is_empty()
                )
            });
    let denied = (protected || git_request) && !principal.authenticated() && !anonymous_membership;
    let unsafe_method = !matches!(
        *request.method(),
        axum::http::Method::GET | axum::http::Method::HEAD | axum::http::Method::OPTIONS
    );
    let backchannel_logout = request.uri().path() == "/auth/backchannel-logout";
    let rejected_mutation = !backchannel_logout
        && !anonymous_membership
        && !git_request
        && unsafe_method
        && !matches!(principal, Principal::Git(_))
        && server
            .auth
            .as_ref()
            .is_some_and(|auth| !auth.accepts_mutation(&principal, request.headers()));
    let archived_response = if !denied && !rejected_mutation && unsafe_method && !git_request {
        archived_mutation_response(&server, &principal, request.uri().path()).await
    } else {
        None
    };
    request.extensions_mut().insert(principal);
    let mut response = if denied && git_request {
        (
            StatusCode::UNAUTHORIZED,
            [(
                "www-authenticate",
                "Basic realm=\"Crab Git\", charset=\"UTF-8\"",
            )],
            "Use a Git access token from your signed-in Crab account",
        )
            .into_response()
    } else if denied {
        (StatusCode::UNAUTHORIZED, Json(json!({"error":{"code":"sign_in_required","message":"Sign in to access repositories"}}))).into_response()
    } else if rejected_mutation {
        (StatusCode::FORBIDDEN, Json(json!({"error":{"code":"csrf_rejected","message":"Reload the page before trying again"}}))).into_response()
    } else if let Some(response) = archived_response {
        response
    } else {
        next.run(request).await
    };
    response
        .headers_mut()
        .entry("cache-control")
        .or_insert(axum::http::HeaderValue::from_static("no-store"));
    (
        [
            ("x-content-type-options", "nosniff"),
            ("referrer-policy", "same-origin"),
            ("content-security-policy", "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; connect-src 'self' https://extensions.duckdb.org; style-src 'self' 'unsafe-inline'; worker-src 'self' blob:; img-src 'self' data: blob:; media-src 'self' blob:; frame-src 'self' blob:; object-src 'none'; base-uri 'none'; frame-ancestors 'none'"),
        ], response,
    ).into_response()
}

fn is_local_host(host: Option<&str>) -> bool {
    let Some(host) = host else {
        return false;
    };
    let Ok(authority) = host.parse::<axum::http::uri::Authority>() else {
        return false;
    };
    let hostname = authority.host();
    let ip_literal = hostname
        .strip_prefix('[')
        .and_then(|value| value.strip_suffix(']'))
        .unwrap_or(hostname);
    hostname.eq_ignore_ascii_case("localhost")
        || ip_literal
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

async fn archived_mutation_response(
    server: &Server,
    principal: &Principal,
    path: &str,
) -> Option<Response> {
    let mut segments = path.strip_prefix("/api/repos/")?.split('/');
    let owner = segments.next()?;
    let name = segments.next()?;
    let action = segments.next()?;
    // Membership belongs to the catalog, including archived repositories. Its
    // handler must authorize against that revision before any Cell response.
    if action == "members" && segments.next().is_none() {
        return None;
    }
    if owner.is_empty()
        || name.is_empty()
        || path == format!("/api/repos/{owner}/{name}/settings/archive")
    {
        return None;
    }
    let repository = server
        .repositories
        .get(&(owner.to_owned(), name.to_owned()))
        .filter(|repository| principal.can_read(&repository.config))?;
    let actor = app::actor(principal).ok()?;
    match repository.lifecycle(server, &actor).await {
        Ok(lifecycle) if lifecycle.archived => Some(app::Error::Archived.into_response()),
        Ok(_) => None,
        Err(error) => Some(error.into_response()),
    }
}

fn integration_api_path(path: &str) -> bool {
    let mut segments = path.split('/');
    segments.next() == Some("")
        && segments.next() == Some("api")
        && segments.next() == Some("repos")
        && segments.next().is_some_and(|value| !value.is_empty())
        && segments.next().is_some_and(|value| !value.is_empty())
        && match segments.next() {
            Some("check-runs") => segments.next().is_none(),
            Some("statuses") => {
                segments.next().is_some_and(|value| !value.is_empty()) && segments.next().is_none()
            }
            Some("commits") => {
                segments.next().is_some_and(|value| !value.is_empty())
                    && match segments.next() {
                        Some("status") => segments.next().is_none(),
                        Some("check-runs") => segments
                            .next()
                            .is_none_or(|value| !value.is_empty() && segments.next().is_none()),
                        _ => false,
                    }
            }
            _ => false,
        }
}

#[cfg(test)]
#[path = "maintenance_tests.rs"]
mod maintenance_tests;

#[cfg(test)]
#[path = "server_peer_e2e_tests.rs"]
mod peer_e2e_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use crab_cell_host::CellNodeTaskGroup;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    #[test]
    fn projection_sweep_rotates_when_one_batch_covers_every_repository() {
        assert_eq!(next_projection_sweep_cursor(0, 12, 12), 1);
        assert_eq!(next_projection_sweep_cursor(1, 12, 12), 2);
    }

    #[test]
    fn projection_sweep_retries_capacity_deferral_without_growing_backoff() {
        let now = Instant::now();
        let retry_delay = Duration::from_secs(40);
        let mut state = ProjectionSweepState {
            source_token: "source".into(),
            next_attempt: now,
            retry_delay,
        };

        state.record_schedule(now, MaintenanceSchedule::Deferred);

        assert_eq!(state.next_attempt, now + PROJECTION_SWEEP_INTERVAL);
        assert_eq!(state.retry_delay, retry_delay);
    }

    struct DropSignal(Arc<AtomicBool>);

    impl Drop for DropSignal {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }

    fn local_resources(
        memory_bytes: u64,
        free_disk_bytes: u64,
        available_file_descriptors: usize,
    ) -> crate::peer::LocalResources {
        crate::peer::LocalResources {
            memory_bytes,
            disk_limit_bytes: free_disk_bytes,
            disk_capacity_bytes: free_disk_bytes,
            free_disk_bytes,
            available_file_descriptors,
            job_credits: 16,
        }
    }

    #[test]
    fn cell_runtime_budget_derives_mailbox_and_active_cell_capacity() {
        let budget =
            CellRuntimeBudget::from_resources(local_resources(2 * GIB, 30 * GIB, 10_000)).unwrap();
        assert_eq!(
            budget,
            CellRuntimeBudget {
                node_retained_bytes: (3 * GIB / 2 / 20) as usize,
                max_active_cells: 1_125,
                blocking_jobs: 16,
                dirty_jobs: 6,
                recovery_jobs: 2,
                scratch_mebibytes: 6_826,
                local_disk_mebibytes: 13_653,
                disk_reserve_bytes: 10 * GIB,
            }
        );
    }

    #[test]
    fn cell_capacity_report_exposes_measured_inputs_and_distinct_job_limits() {
        let report =
            encode_cell_capacity_report(local_resources(2 * GIB, 30 * GIB, 10_000)).unwrap();
        let report: serde_json::Value = serde_json::from_slice(&report).unwrap();

        assert_eq!(report["version"], 1);
        assert_eq!(report["resources"]["memory_bytes"], 2 * GIB);
        assert_eq!(report["resources"]["disk_limit_bytes"], 30 * GIB);
        assert_eq!(report["resources"]["disk_capacity_bytes"], 30 * GIB);
        assert_eq!(report["resources"]["free_disk_bytes"], 30 * GIB);
        assert_eq!(report["admission"]["active_cells"], 1_125);
        assert_eq!(report["admission"]["blocking_jobs"], 16);
        assert_eq!(report["admission"]["dirty_jobs"], 6);
        assert_eq!(report["admission"]["recovery_jobs"], 2);
        assert_eq!(report["reservations"]["maximum_recovery_jobs"], 2);
    }

    #[test]
    fn cell_scratch_monitor_rechecks_actual_free_space() {
        let directory = tempfile::TempDir::new().unwrap();
        let available = fs4::available_space(directory.path()).unwrap();
        let monitor = ActualScratchMonitor {
            root: directory.path().to_owned(),
            local_disk: crab_cell_runtime::DiskBudget::new(MIB),
            reserve_bytes: available,
        };

        assert!(matches!(
            monitor.ensure_available(1),
            Err(error) if error.kind() == io::ErrorKind::StorageFull
        ));
    }

    #[test]
    fn release_lifecycle_admits_only_the_current_rollout_members() {
        let current = Digest::from_bytes([1; 32]);
        let desired = Digest::from_bytes([2; 32]);

        assert!(release_admits_process(
            ReleaseState::Ready,
            Some(current),
            Some(current),
            current,
        ));
        assert!(release_admits_process(
            ReleaseState::Prepared,
            Some(current),
            Some(desired),
            current,
        ));
        assert!(release_admits_process(
            ReleaseState::Activating,
            Some(current),
            Some(desired),
            desired,
        ));
        assert!(!release_admits_process(
            ReleaseState::Ready,
            Some(desired),
            Some(desired),
            current,
        ));
        for state in [ReleaseState::Maintenance, ReleaseState::Failed] {
            assert!(!release_admits_process(
                state,
                Some(current),
                Some(desired),
                current,
            ));
        }
    }

    #[test]
    fn cell_runtime_budget_memory_bounds_active_cells() {
        let budget =
            CellRuntimeBudget::from_resources(local_resources(2 * GIB, 30 * GIB, 1_000_000))
                .unwrap();
        assert_eq!(budget.max_active_cells, 2_457);
        assert_eq!(budget.blocking_jobs, 16);
        assert_eq!(budget.dirty_jobs, 6);
        assert_eq!(budget.recovery_jobs, 2);
        assert_eq!(budget.scratch_mebibytes, 6_826);
        assert_eq!(budget.local_disk_mebibytes, 13_653);
        assert_eq!(budget.disk_reserve_bytes, 10 * GIB);
    }

    #[test]
    fn cell_runtime_budget_caps_dirty_jobs_by_cpu_credits() {
        let mut resources = local_resources(64 * GIB, 1000 * GIB, 1_000_000);
        resources.job_credits = 2;
        let budget = CellRuntimeBudget::from_resources(resources).unwrap();
        assert_eq!(budget.blocking_jobs, 2);
        assert_eq!(budget.dirty_jobs, 2);
        assert_eq!(budget.recovery_jobs, 2);
    }

    #[test]
    fn cell_runtime_budget_caps_recovery_below_other_replica_jobs() {
        let budget =
            CellRuntimeBudget::from_resources(local_resources(64 * GIB, 1000 * GIB, 1_000_000))
                .unwrap();

        assert_eq!(budget.blocking_jobs, 16);
        assert_eq!(budget.dirty_jobs, 16);
        assert_eq!(budget.recovery_jobs, 2);
    }

    #[test]
    fn cell_runtime_budget_keeps_the_node_safety_ceiling() {
        let budget =
            CellRuntimeBudget::from_resources(local_resources(64 * GIB, 1000 * GIB, 1_000_000))
                .unwrap();
        assert_eq!(budget.max_active_cells, MAX_ACTIVE_CELLS);
    }

    #[test]
    fn cell_runtime_budget_rejects_insufficient_memory() {
        assert!(
            CellRuntimeBudget::from_resources(local_resources(2 * GIB - 1, 30 * GIB, 10_000))
                .is_err()
        );
    }

    #[test]
    fn cell_runtime_budget_rejects_insufficient_disk() {
        assert!(
            CellRuntimeBudget::from_resources(local_resources(2 * GIB, 30 * GIB - 1, 10_000))
                .is_err()
        );
    }

    #[test]
    fn cell_runtime_budget_rejects_insufficient_file_descriptors() {
        assert!(
            CellRuntimeBudget::from_resources(crate::peer::LocalResources {
                memory_bytes: 2 * GIB,
                disk_limit_bytes: 30 * GIB,
                disk_capacity_bytes: 30 * GIB,
                free_disk_bytes: 30 * GIB,
                available_file_descriptors: FILE_DESCRIPTOR_RESERVE_MINIMUM,
                job_credits: 16,
            })
            .is_err()
        );
    }

    #[tokio::test]
    async fn shutdown_deadline_drops_an_unfinished_phase() {
        let dropped = Arc::new(AtomicBool::new(false));
        let signal = DropSignal(dropped.clone());
        let result =
            before_shutdown_deadline(Instant::now() + Duration::from_millis(10), async move {
                let _signal = signal;
                std::future::pending::<()>().await;
            })
            .await;

        assert!(matches!(result, Err(crate::Error::ShutdownTimeout)));
        assert!(dropped.load(Ordering::Acquire));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cell_task_facility_aborts_unfinished_tasks_when_deadline_expires() {
        let facility = CellNodeTaskGroup::new(CancellationToken::new(), CancellationToken::new());
        let dropped = Arc::new(AtomicBool::new(false));
        let signal = DropSignal(Arc::clone(&dropped));
        facility
            .spawn(async move {
                let _signal = signal;
                std::future::pending::<()>().await;
                Ok::<(), crate::Error>(())
            })
            .unwrap();

        let result = tokio::time::timeout(Duration::from_millis(10), facility.drain()).await;
        assert!(result.is_err());
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(dropped.load(Ordering::Acquire));
    }

    #[tokio::test]
    async fn storage_preflight_leaves_no_live_probe_object() {
        let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
        let catalog = CatalogStore::new(crate::storage_root::StorageRoot::memory(
            store.clone(),
            "repositories",
        ));
        let admission = transfer_admission(&catalog);

        probe_storage_contract(&catalog, &admission).await.unwrap();
        probe_storage_contract(&catalog, &admission).await.unwrap();

        let objects = store
            .list_prefix(&catalog.root().path(".crab/http-server/v1/auth/preflight"))
            .await
            .unwrap();
        assert!(objects.is_empty());
    }

    #[test]
    fn repository_tokens_are_only_considered_on_exact_integration_routes() {
        assert!(integration_api_path(
            "/api/repos/team/repo/statuses/0123456789012345678901234567890123456789"
        ));
        assert!(integration_api_path(
            "/api/repos/team/repo/commits/0123456789012345678901234567890123456789/status"
        ));
        assert!(integration_api_path("/api/repos/team/repo/check-runs"));
        assert!(integration_api_path(
            "/api/repos/team/repo/commits/0123456789012345678901234567890123456789/check-runs"
        ));
        assert!(integration_api_path(
            "/api/repos/team/repo/commits/0123456789012345678901234567890123456789/check-runs/1"
        ));
        for path in [
            "/api/repos/team/repo/pulls/1",
            "/api/repos/team/repo/check-runs/1",
            "/api/repos/team/repo/statuses/oid/extra",
            "/api/repos/team/repo/commits/oid/statuses",
            "/api/repos/team/repo/commits/oid/check-runs/1/extra",
            "/api/repos/team/repo/commits/oid/check-runs/",
            "/api/repos//repo/commits/oid/statuses",
            "/api/repos/team/repo/commits//statuses",
        ] {
            assert!(!integration_api_path(path), "{path}");
        }
    }

    #[tokio::test]
    async fn transport_enforces_host_and_preserves_asset_cache_policy() {
        let runtime = Arc::new(RemoteGitRuntime::default());
        let server = Arc::new(Server {
            repositories: RepositorySet::from(BTreeMap::<(String, String), Repository>::new()),
            runtime: Arc::clone(&runtime),
            cell_runtime: start_test_cell_runtime(),
            cell_node: None,
            repository_cells: None,
            peer_receiver: None,
            follower_store: None,
            node_log_transport: None,
            options: RepositoryOptions::default(),
            cursor_key: [0; 32],
            admission: Semaphore::new(1),
            transfer_admission: TransferAdmission::new(
                Store::new(Arc::new(object_store::memory::InMemory::new())),
                "test/.crab/http-server/v1/admission".into(),
                1,
            ),
            local_staging: crate::local_disk::LocalStaging::for_test(),
            app_admission: Semaphore::new(1),
            maintenance_admission: Arc::new(Semaphore::new(1)),
            cancellation: CancellationToken::new(),
            receives: tokio_util::task::TaskTracker::new(),
            auth: None,
            git_import: None,
            catalog: None,
            catalog_healthy: AtomicBool::new(false),
            node_healthy: AtomicBool::new(false),
            scheduler_status: crate::cells::SchedulerStatus::new(
                crate::cells::unix_now_ms().unwrap(),
            )
            .unwrap(),
            cell_capacity: test_cell_capacity_report(),
            metrics: crate::metrics::Metrics::new().unwrap(),
        });
        let app = router(Arc::clone(&server));
        for (path, host, expected, cache) in [
            (
                "/api/repos",
                "untrusted.invalid",
                StatusCode::FORBIDDEN,
                None,
            ),
            (
                "/api/repos",
                "127.0.0.1:18791",
                StatusCode::OK,
                Some("no-store"),
            ),
            (
                "/team/repo.name",
                "localhost:8788",
                StatusCode::OK,
                Some("no-cache"),
            ),
            (
                "/api/repos/team/missing/tree",
                "[::1]:8788",
                StatusCode::NOT_FOUND,
                Some("no-store"),
            ),
            (
                "/api/repos",
                "127.0.0.1.evil.invalid:8788",
                StatusCode::FORBIDDEN,
                None,
            ),
        ] {
            let request = Request::builder()
                .uri(path)
                .header("host", host)
                .body(Body::empty())
                .unwrap();
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), expected, "{path}, {host}");
            let request_id = response
                .headers()
                .get("x-request-id")
                .and_then(|value| value.to_str().ok());
            assert!(
                request_id.is_some_and(|value| Uuid::parse_str(value).is_ok()),
                "{path}, {host}"
            );
            assert_eq!(
                response
                    .headers()
                    .get("cache-control")
                    .and_then(|value| value.to_str().ok()),
                cache
            );
            if expected.is_success() {
                let policy = response
                    .headers()
                    .get("content-security-policy")
                    .and_then(|value| value.to_str().ok())
                    .unwrap();
                assert!(
                    policy.contains("connect-src 'self' https://extensions.duckdb.org"),
                    "{path}, {host}"
                );
            }
            if expected == StatusCode::NOT_FOUND {
                let body = response.into_body().collect().await.unwrap().to_bytes();
                let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(value["error"]["code"], "repository_not_found");
            }
        }
        for path in ["/healthz", "/readyz"] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(path)
                        .header("host", "127.0.0.1:8788")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
        }
        let management = management_router(Arc::clone(&server));
        for (path, expected) in [
            ("/healthz", StatusCode::OK),
            ("/readyz", StatusCode::SERVICE_UNAVAILABLE),
        ] {
            let response = management
                .clone()
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), expected, "{path}");
        }
        for path in [
            "/internal/cells/v1/forward",
            "/internal/cells/v1/node-log/leader/1/append",
            "/internal/cells/v1/node-log/leader/1/retire/0",
        ] {
            let response = management
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(path)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE, "{path}");
        }
        let recovery = management
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/internal/cells/v1/node-log/leader/1/recovery/claimant/seal")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_ne!(recovery.status(), StatusCode::SERVICE_UNAVAILABLE);
        server.node_healthy.store(true, Ordering::Release);
        let admitted = management
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/internal/cells/v1/forward")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_ne!(admitted.status(), StatusCode::SERVICE_UNAVAILABLE);
        let response = management
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/capacity")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("cache-control")
                .and_then(|value| value.to_str().ok()),
            Some("no-store")
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let report: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(report["version"], 1);
        assert_eq!(report["admission"]["active_cells"], 1_125);
        assert_eq!(report["admission"]["recovery_jobs"], 2);
        let response = management
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok()),
            Some("text/plain; version=0.0.4; charset=utf-8")
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let body = std::str::from_utf8(&body).unwrap();
        assert!(body.contains("crab_http_server_catalog_healthy 0"));
        assert!(body.contains("crab_http_server_requests_total{method=\"get\",outcome=\"2xx\"} 2"));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/livez")
                    .header("host", "10.42.3.17:8788")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        server.shutdown_runtimes().await.unwrap();
    }
}

#[cfg(test)]
#[path = "receive_fault_tests.rs"]
mod receive_fault_tests;

#[cfg(test)]
#[path = "receive_tests.rs"]
mod receive_tests;

#[cfg(test)]
#[path = "auth_tests.rs"]
mod auth_tests;

#[cfg(test)]
#[path = "lfs_tests.rs"]
mod lfs_tests;

#[cfg(test)]
#[path = "pulls_tests.rs"]
mod pulls_tests;
