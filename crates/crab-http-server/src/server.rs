use std::collections::{BTreeMap, HashMap};
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock as SyncRwLock};
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use axum::{
    Extension, Json, Router,
    extract::{Query, Request, State},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use bytes::Bytes;
use crab_remote_git::{
    OperationLimits, RemoteGitRepository, RemoteGitRuntime, RepositoryIdentity, RepositoryOptions,
};
use crab_storage::{StorageError, Store, StoreLayout};
#[cfg(test)]
use object_store::path::Path as ObjectPath;
use serde_json::json;
use tokio::sync::{Mutex, Semaphore};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;
use uuid::Uuid;

use crate::catalog::CatalogStore;
use crate::cells::repository::projection::SourceIdentity;
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
const FILE_DESCRIPTOR_RESERVE_MINIMUM: usize = 128;
const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(110);
pub(crate) const BROWSE_PROBE_INTERVAL: Duration = Duration::from_secs(2);
const PROJECTION_SWEEP_INTERVAL: Duration = Duration::from_secs(5);
const PROJECTION_SWEEP_BATCH: usize = 16;
const PROJECTION_RETRY_BASE: Duration = Duration::from_secs(10);
const PROJECTION_RETRY_MAX: Duration = Duration::from_secs(5 * 60);
#[derive(Clone, Copy, Debug, serde::Serialize)]
pub(crate) struct CellRuntimeBudget {
    max_active_cells: usize,
    local_disk_bytes: u64,
    disk_reserve_bytes: u64,
}

#[derive(Clone, serde::Serialize)]
pub(crate) struct CellCapacityReport {
    version: u32,
    resources: crate::resources::LocalResources,
    admission: CellRuntimeBudget,
}

impl CellRuntimeBudget {
    pub(crate) fn from_resources(resources: crate::resources::LocalResources) -> Result<Self> {
        if resources.memory_bytes < GIB {
            return Err(crate::Error::Config(
                "Cellar requires at least 1 GiB effective memory",
            ));
        }
        let disk_reserve_bytes = (resources.free_disk_bytes / 5).max(10 * GIB);
        let local_disk_bytes = resources.free_disk_bytes.saturating_sub(disk_reserve_bytes);
        if local_disk_bytes < 20 * GIB {
            return Err(crate::Error::Config(
                "Cellar requires at least 20 GiB usable local disk",
            ));
        }
        let memory = resources
            .memory_bytes
            .saturating_sub((resources.memory_bytes / 4).max(512 * MIB));
        let descriptors = resources
            .available_file_descriptors
            .saturating_sub(FILE_DESCRIPTOR_RESERVE_MINIMUM)
            / 8;
        let max_active_cells = usize::try_from(memory / (64 * MIB))
            .unwrap_or(usize::MAX)
            .min(descriptors)
            .min(MAX_ACTIVE_CELLS);
        if max_active_cells == 0 {
            return Err(crate::Error::Config(
                "Cellar has no memory or file-descriptor capacity",
            ));
        }
        Ok(Self {
            max_active_cells,
            local_disk_bytes,
            disk_reserve_bytes,
        })
    }
    pub(crate) fn local_disk(self) -> crate::local_disk::DiskBudget {
        crate::local_disk::DiskBudget::new(self.local_disk_bytes)
    }
}

pub(crate) fn cell_capacity_report(directory: &std::path::Path, limit: u64) -> Result<Vec<u8>> {
    let resources = crate::resources::local_resources(directory, limit)?;
    let admission = CellRuntimeBudget::from_resources(resources)?;
    Ok(serde_json::to_vec(&CellCapacityReport {
        version: 2,
        resources,
        admission,
    })?)
}

#[cfg(test)]
fn test_cell_capacity_report() -> CellCapacityReport {
    let resources = crate::resources::LocalResources {
        memory_bytes: 2 * GIB,
        disk_limit_bytes: 32 * GIB,
        disk_capacity_bytes: 32 * GIB,
        free_disk_bytes: 32 * GIB,
        available_file_descriptors: 10_000,
        job_credits: 4,
    };
    CellCapacityReport {
        version: 2,
        admission: CellRuntimeBudget::from_resources(resources).unwrap(),
        resources,
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
    pinned: Mutex<Option<PinnedRepository>>,
    pub(crate) projection_source: Mutex<Option<crate::projection::CachedSource>>,
    maintenance: Mutex<MaintenanceState>,
    pub(crate) integrity: crate::integrity::Status,
}

#[derive(Default)]
struct MaintenanceState {
    worker: Option<tokio::task::JoinHandle<crate::maintenance::Result<Option<SourceIdentity>>>>,
    completed: Option<(SourceIdentity, Instant)>,
}

pub(crate) enum MaintenanceTrigger<'a> {
    Poll(&'a SourceIdentity),
    Repair,
}

struct PinnedRepository {
    checked: Option<Instant>,
    state_digest: String,
    browse_indexes: Option<crab_metadata::capsule_protocol::BrowseIndexes>,
    opened: BrowseRepository,
}

#[derive(Clone)]
pub(crate) struct BrowseRepository {
    pub git: RemoteGitRepository,
    pub source: crate::cells::repository::projection::SourceIdentity,
}

pub(crate) struct RepositorySet {
    current: SyncRwLock<RepositoryIndex>,
}

struct RepositoryIndex {
    version: u64,
    by_name: BTreeMap<(String, String), Arc<Repository>>,
    by_id: HashMap<Uuid, (String, String)>,
}

impl RepositorySet {
    fn version(&self) -> u64 {
        self.current
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .version
    }

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

    fn replace(&self, next: RepositoryIndex) {
        let mut current = self
            .current
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Import and refresh await metadata independently. Publish revision and
        // both indexes together so a late snapshot cannot restore revoked access.
        if next.version <= current.version {
            return;
        }
        let version = next.version;
        *current = next;
        drop(current);
        tracing::info!(catalog_version = version, "repository catalog refreshed");
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

#[cfg(test)]
impl From<BTreeMap<(String, String), Repository>> for RepositorySet {
    fn from(repositories: BTreeMap<(String, String), Repository>) -> Self {
        RepositoryIndex::new(
            0,
            repositories
                .into_iter()
                .map(|(key, repository)| (key, Arc::new(repository)))
                .collect(),
        )
        .into()
    }
}

#[cfg(test)]
impl From<BTreeMap<(String, String), Arc<Repository>>> for RepositorySet {
    fn from(repositories: BTreeMap<(String, String), Arc<Repository>>) -> Self {
        RepositoryIndex::new(0, repositories).into()
    }
}

impl From<RepositoryIndex> for RepositorySet {
    fn from(repositories: RepositoryIndex) -> Self {
        Self {
            current: SyncRwLock::new(repositories),
        }
    }
}

impl RepositoryIndex {
    fn new(version: u64, by_name: BTreeMap<(String, String), Arc<Repository>>) -> Self {
        let by_id = by_name
            .iter()
            .map(|(key, repository)| (repository.id, key.clone()))
            .collect();
        Self {
            version,
            by_name,
            by_id,
        }
    }
}

impl Repository {
    pub(crate) async fn branch_protections(
        &self,
        server: &Server,
    ) -> app::Result<BranchProtections> {
        repository_settings::load(server, self).await
    }

    pub(crate) async fn lifecycle(&self, server: &Server) -> app::Result<RepositoryLifecycle> {
        repository_settings::load_lifecycle(server, self).await
    }

    #[cfg(test)]
    pub(crate) async fn expire_browse_probe(&self) {
        if let Some(cached) = self.pinned.lock().await.as_mut() {
            cached.checked = None;
        }
    }

    pub(crate) async fn invalidate(&self) {
        // Maintenance can finish without changing Git state. Keep verified
        // data while forcing the next read to check ref and index authority.
        if let Some(cached) = self.pinned.lock().await.as_mut() {
            cached.checked = None;
        }
        *self.projection_source.lock().await = None;
        self.maintenance.lock().await.completed = None;
    }

    pub(crate) async fn schedule_maintenance(
        &self,
        server: &Server,
        trigger: MaintenanceTrigger<'_>,
    ) -> Result<MaintenanceSchedule> {
        if server
            .git_import
            .as_ref()
            .is_some_and(|context| context.is_repository_importing(self.id))
        {
            return Ok(MaintenanceSchedule::Deferred);
        }
        let mut state = self.maintenance.lock().await;
        if matches!(trigger, MaintenanceTrigger::Repair) {
            state.completed = None;
        }
        if state
            .worker
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished)
        {
            if let Some(worker) = state.worker.take() {
                let completed = worker.await??;
                if matches!(trigger, MaintenanceTrigger::Poll(_)) {
                    state.completed = completed.map(|source| (source, Instant::now()));
                }
            }
            if let Some(cached) = self.pinned.lock().await.as_mut() {
                cached.checked = None;
            }
            *self.projection_source.lock().await = None;
            // Retain the completed source while forcing a control/index probe;
            // invalidate would erase completion and reschedule the same epoch.
            return Ok(MaintenanceSchedule::Completed);
        }
        if state.worker.is_some() {
            return Ok(MaintenanceSchedule::Running);
        }
        if let MaintenanceTrigger::Poll(source) = trigger
            && state.completed.as_ref().is_some_and(|(complete, checked)| {
                complete.source_token == source.source_token
                    && checked.elapsed() < crate::integrity::SCRUB_INTERVAL
            })
        {
            return Ok(MaintenanceSchedule::Completed);
        }
        let permit = match server.maintenance_admission.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => return Ok(MaintenanceSchedule::Deferred),
        };
        *state = MaintenanceState {
            completed: None,
            worker: Some(tokio::spawn(maintenance::run(
                self.layout.clone(),
                Arc::clone(&server.maintenance_admission),
                Some(permit),
                server.cancellation.clone(),
                maintenance::Pass::BackgroundMaintenance,
                Some(maintenance::ProjectionContext {
                    repository_id: self.id,
                    client: server.repository_cells().map(|router| (*router).clone()),
                    metrics: server.metrics.clone(),
                    identity: self.identity.clone(),
                    runtime: Arc::clone(&server.runtime),
                    options: server.options,
                }),
            ))),
        };
        Ok(MaintenanceSchedule::Started)
    }

    pub(crate) async fn checkpoint_now(
        &self,
        server: &Server,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        crate::maintenance::run(
            self.layout.clone(),
            Arc::clone(&server.maintenance_admission),
            None,
            cancellation.clone(),
            crate::maintenance::Pass::ForegroundCheckpoint,
            None,
        )
        .await
        .map(drop)
        .map_err(Into::into)
    }

    #[cfg(test)]
    pub(crate) async fn open_view(
        &self,
    ) -> Result<crab_read::capsule_protocol::CapsuleRepositoryView> {
        crab_read::capsule_protocol::open_view(
            &self.layout,
            crab_read::capsule_protocol::CapsuleReadLimits {
                max_capsule_bytes: 2 * 1024 * 1024 * 1024,
                max_frontier_bytes: 2 * 1024 * 1024 * 1024,
            },
        )
        .await
        .map_err(Into::into)
    }

    pub(crate) async fn open_ref_view(
        &self,
    ) -> Result<crab_read::capsule_protocol::CapsuleRefView> {
        let root = crab_metadata::capsule_protocol::load_root(&self.layout)
            .await
            .map_err(|source| crate::Error::Settings {
                source: Box::new(source),
            })?;
        crab_read::capsule_protocol::open_ref_view_from_root(&self.layout, root)
            .await
            .map_err(Into::into)
    }

    pub(crate) async fn open_capsule_repository(
        &self,
        server: &Server,
        options: RepositoryOptions,
        cancellation: &CancellationToken,
    ) -> Result<(
        crab_read::capsule_protocol::CapsuleRepositoryView,
        RemoteGitRepository,
    )> {
        let root = crab_metadata::capsule_protocol::load_root(&self.layout)
            .await
            .map_err(|source| crate::Error::Settings {
                source: Box::new(source),
            })?;
        let view = crab_read::capsule_protocol::open_git_view_from_root(
            &self.layout,
            root,
            crab_read::capsule_protocol::CapsuleReadLimits {
                max_capsule_bytes: 2 * 1024 * 1024 * 1024,
                max_frontier_bytes: 2 * 1024 * 1024 * 1024,
            },
        )
        .await?;
        let repository = view
            .git_repository_from_store(
                self.layout.clone(),
                self.identity.clone(),
                Arc::clone(&server.runtime),
                options,
                2 * 1024 * 1024 * 1024,
                cancellation,
            )
            .await?;
        Ok((view, repository))
    }

    pub(crate) async fn browse_state(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<(
        crab_read::capsule_protocol::CapsuleRepositoryActivity,
        Option<crab_metadata::capsule_protocol::BrowseIndexes>,
    )> {
        tokio::select! {
            biased;
            () = cancellation.cancelled() => Err(crab_remote_git::Error::Cancelled.into()),
            result = async {
                let root = crab_metadata::capsule_protocol::load_root(&self.layout)
                    .await
                    .map_err(|source| crate::Error::Settings { source: Box::new(source) })?;
                let activity = crab_read::capsule_protocol::read_activity_from_root(&self.layout, &root).await?;
                let indexes = crate::projection::load_browse_indexes(&self.layout).await?
                    .filter(|value| value.state_digest() == activity.state_digest());
                Ok((activity, indexes))
            } => result,
        }
    }

    pub(crate) async fn open(
        &self,
        server: &Server,
        cancellation: &CancellationToken,
    ) -> Result<BrowseRepository> {
        let mut pinned = tokio::select! {
            _ = cancellation.cancelled() => return Err(crab_remote_git::Error::Cancelled.into()),
            pinned = self.pinned.lock() => pinned,
        };
        if let Some(cached) = pinned.as_mut() {
            if cached
                .checked
                .is_some_and(|checked| checked.elapsed() < BROWSE_PROBE_INTERVAL)
            {
                return Ok(cached.opened.clone());
            }
            // Ref CAS can advance without a root ETag change; derived indexes
            // can arrive without a ref change. Check both before reusing the
            // immutable reader instead of rebuilding it on every cache expiry.
            let (activity, indexes) = self.browse_state(cancellation).await?;
            if activity.state_digest() == cached.state_digest && indexes == cached.browse_indexes {
                cached.checked = Some(Instant::now());
                return Ok(cached.opened.clone());
            }
        }
        let cached = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Err(crab_remote_git::Error::Cancelled.into()),
            cached = self.open_browse(server, server.options, cancellation) => cached?,
        };
        let opened = cached.opened.clone();
        *pinned = Some(cached);
        Ok(opened)
    }

    pub(crate) async fn open_current(
        &self,
        server: &Server,
        options: RepositoryOptions,
        cancellation: &CancellationToken,
    ) -> Result<RemoteGitRepository> {
        // Native Git and mutation validation must not depend on optional browse
        // indexes or pay their origin requests on the foreground push path.
        self.open_capsule_repository(server, options, cancellation)
            .await
            .map(|(_, repository)| repository)
    }

    async fn open_browse(
        &self,
        server: &Server,
        options: RepositoryOptions,
        cancellation: &CancellationToken,
    ) -> Result<PinnedRepository> {
        let view = crate::projection::open_view(&self.layout).await?;
        let state_digest = view.state_digest();
        let browse_indexes = view.browse_indexes().cloned();
        let source = crate::projection::source_identity(&view.git_snapshot()?)?;
        let git = view
            .git_repository_from_store(
                self.layout.clone(),
                self.identity.clone(),
                Arc::clone(&server.runtime),
                options,
                2 * 1024 * 1024 * 1024,
                cancellation,
            )
            .await?;
        Ok(PinnedRepository {
            checked: Some(Instant::now()),
            state_digest,
            browse_indexes,
            opened: BrowseRepository { git, source },
        })
    }
}

pub(crate) struct Server {
    pub repositories: RepositorySet,
    pub runtime: Arc<RemoteGitRuntime>,
    pub(crate) repository_cells: Option<Arc<crate::cells::RepositoryClient>>,
    pub(crate) cell_services: Option<Arc<cellar::sdk::HostServices>>,
    #[cfg(test)]
    pub(crate) cell_test_runtime: Option<Arc<crate::cells::runtime::TestRuntime>>,
    pub(crate) shutdown_mode: tokio::sync::watch::Sender<cellar::sdk::ShutdownMode>,
    pub options: RepositoryOptions,
    pub cursor_key: [u8; 32],
    pub admission: Semaphore,
    pub transfer_admission: TransferAdmission,
    pub(crate) local_staging: crate::local_disk::LocalStaging,
    pub app_admission: Semaphore,
    pub(crate) maintenance_admission: Arc<Semaphore>,
    pub cancellation: CancellationToken,
    pub receives: tokio_util::task::TaskTracker,
    pub auth: Option<Authentication>,
    pub(crate) git_import: Option<git_import::ImportContext>,
    pub(crate) catalog: Option<CatalogStore>,
    catalog_healthy: AtomicBool,
    pub(crate) node_healthy: AtomicBool,
    pub(crate) cell_capacity: CellCapacityReport,
    pub(crate) metrics: crate::metrics::Metrics,
}

impl Server {
    pub(crate) fn repository_cells(&self) -> Option<Arc<crate::cells::RepositoryClient>> {
        self.repository_cells.clone()
    }
    pub(crate) fn catalog(&self) -> Option<CatalogStore> {
        self.catalog.clone()
    }

    pub(crate) async fn install_catalog(
        &self,
        document: crate::catalog::CatalogDocument,
    ) -> Result<()> {
        if document.version <= self.repositories.version() {
            return Ok(());
        }
        let catalog = self.catalog().ok_or(crate::Error::Config(
            "repository catalog installation requires a catalog",
        ))?;
        let router = self.repository_cells().ok_or(crate::Error::Config(
            "repository catalog installation requires Cell routing",
        ))?;
        let repositories = document
            .repositories
            .iter()
            .filter(|record| {
                record.application == crate::catalog::RepositoryApplicationState::CellReady
            })
            .map(|record| record.id)
            .collect::<Vec<_>>();
        // Every publisher validates Cell readiness before advancing the shared
        // revision; otherwise import could make refresh skip unverified Cells.
        router.verify_repositories(repositories).await?;
        let next = materialize_catalog(&catalog, document).await?;
        self.repositories.replace(next);
        Ok(())
    }

    fn cell_capacity(&self) -> Option<CellCapacityReport> {
        Some(self.cell_capacity.clone())
    }

    pub(crate) fn accepts_new_cells(&self) -> bool {
        self.node_healthy.load(Ordering::Acquire)
            && !self.cancellation.is_cancelled()
            && self
                .repository_cells
                .as_ref()
                .is_some_and(|router| !router.is_closed())
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
            if let Some(task) = repository.maintenance.lock().await.worker.take() {
                let completed = match task.await {
                    Ok(Ok(_)) | Ok(Err(crate::maintenance::Error::Cancelled)) => Ok(()),
                    Ok(Err(error)) => Err(crate::Error::from(error)),
                    Err(error) => Err(crate::Error::from(error)),
                };
                result = result.and(completed);
            }
        }
        result
    }

    #[cfg(test)]
    async fn shutdown_runtimes(&self) -> Result<()> {
        #[cfg(test)]
        if let Some(cells) = &self.cell_test_runtime {
            cells.shutdown().await?;
        }
        self.runtime.shutdown().await;
        Ok(())
    }
}

/// Serve configured repositories and compiled React assets until shutdown.
pub async fn serve(config: Config) -> Result<()> {
    config.validate()?;
    let catalog = CatalogStore::from_config(&config)?;
    catalog.flush_membership_audit().await?;
    let (document, _) = catalog.load().await?;
    let repositories = materialize_catalog(&catalog, document).await?;
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
    std::fs::create_dir_all(&config.cells.data_dir)?;
    let resources = crate::resources::local_resources(
        &config.cells.data_dir,
        config.cells.local_disk_limit_bytes,
    )?;
    let budget = CellRuntimeBudget::from_resources(resources)?;
    let capacity = CellCapacityReport {
        version: 2,
        resources,
        admission: budget,
    };
    let session_dir = config
        .cells
        .data_dir
        .join("sessions")
        .join(Uuid::now_v7().to_string());
    let local_staging = crate::local_disk::LocalStaging::new_with_restart_inventory(
        session_dir.join("transfers"),
        budget.local_disk(),
        budget.disk_reserve_bytes,
        &config.cells.data_dir,
        &session_dir,
    )
    .map_err(|source| crate::Error::LocalStaging {
        source: Box::new(source),
    })?;
    if let Some((bytes, sessions)) = local_staging.restart_inventory_usage() {
        tracing::info!(
            retained_bytes = bytes,
            retained_sessions = sessions,
            "accounted for retained local staging"
        );
    }
    let options = RepositoryOptions::new(
        Default::default(),
        OperationLimits {
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
    let metrics = crate::metrics::Metrics::new()?;
    let tls = crate::peer_tls::LoadedPeerTls::load(&config.cells)?;
    // Cellar Preserve identifies a logical node by its stable peer address.
    // A fleet may share a TLS certificate; that certificate cannot name a node.
    let node_name = format!(
        "crab-{}",
        blake3::hash(config.cells.peer_advertise.as_str().as_bytes()).to_hex()
    );
    let listener = tokio::net::TcpListener::bind(config.listen).await?;
    let public_address = listener.local_addr()?;
    let management_listener = tokio::net::TcpListener::bind(config.management_listen).await?;
    let internal_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let internal_address = internal_listener.local_addr()?;
    let peer_work = tokio_util::task::TaskTracker::new();
    let management_shutdown = CancellationToken::new();
    let incoming = crate::peer::incoming(
        internal_address,
        peer_work.clone(),
        management_shutdown.clone(),
    )?;
    let transport = crate::peer::PeerTransport::start(&tls).await?;
    let started = crate::cells::runtime::start(
        &config,
        public_address,
        internal_listener,
        node_name,
        transport.http.clone(),
        transport.stream_http.clone(),
        budget.max_active_cells,
    )
    .await;
    let started = match started {
        Ok(started) => started,
        Err(error) => {
            let _ = transport.shutdown().await;
            return Err(error.into());
        }
    };
    let mut node = started.node;
    let runtime = Arc::new(RemoteGitRuntime::default());
    let cancellation = CancellationToken::new();
    let server = Arc::new(Server {
        repositories: repositories.into(),
        runtime: runtime.clone(),
        repository_cells: Some(started.client.clone()),
        cell_services: Some(node.services()),
        shutdown_mode: tokio::sync::watch::channel(cellar::sdk::ShutdownMode::Preserve).0,
        options,
        #[cfg(test)]
        cell_test_runtime: None,
        cursor_key: auth
            .as_ref()
            .map(Authentication::cursor_key)
            .unwrap_or_else(rand::random),
        admission: Semaphore::new(READ_ADMISSION_CAPACITY),
        transfer_admission: transfer_admission(&catalog),
        local_staging,
        app_admission: Semaphore::new(APP_ADMISSION_CAPACITY),
        maintenance_admission: Arc::new(Semaphore::new(MAINTENANCE_ADMISSION_CAPACITY)),
        cancellation: cancellation.clone(),
        receives: tokio_util::task::TaskTracker::new(),
        auth,
        git_import: Some(git_import::ImportContext::new(
            config.clone(),
            public_address,
        )),
        catalog: Some(catalog),
        catalog_healthy: AtomicBool::new(true),
        node_healthy: AtomicBool::new(true),
        cell_capacity: capacity,
        metrics,
    });
    let management = management_router(server.clone(), incoming);
    let stopped = management_shutdown.clone();
    let mut management_task = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async move {
        axum::serve(tls.listener(management_listener), management)
            .with_graceful_shutdown(stopped.cancelled_owned())
            .await
    }));
    let mut background = tokio::task::JoinSet::new();
    let refresh = server.clone();
    background.spawn(async move {
        refresh_catalog(refresh).await;
        Ok(())
    });
    let projection = server.clone();
    background.spawn(async move { sweep_projections(projection).await });
    let integrity = server.clone();
    background.spawn(async move {
        crate::integrity::run(integrity).await;
        Ok(())
    });
    let stop = cancellation.clone();
    let public_app = router(server.clone());
    let public = async move {
        axum::serve(listener, public_app)
            .with_graceful_shutdown(stop.cancelled_owned())
            .await
    };
    tokio::pin!(public);
    let mut management_ended = false;
    let mut node_ended = false;
    let mut public_ended = false;
    let ready = started
        .client
        .verify_repositories(
            server
                .repositories
                .values()
                .iter()
                .map(|repository| repository.id)
                .collect(),
        )
        .await;
    let primary: Result<()> = match ready {
        Err(error) => Err(error),
        Ok(()) => tokio::select! {
        result = &mut public => { public_ended = true; result.map_err(Into::into) },
        result = &mut management_task => {
            management_ended = true;
            result.map_err(crate::Error::from).and_then(|result| result.map_err(Into::into))
                .and(Err(crate::Error::Config("management listener stopped unexpectedly")))
        },
        result = node.finished() => {
            node_ended = true;
            result.map_err(crate::cells::Error::sdk).map_err(crate::Error::Cell)
                .and(Err(crate::Error::Config("Cellar stopped unexpectedly")))
        },
        result = background.join_next() => {
            match result {
                Some(Ok(Err(error))) => Err(error),
                Some(Err(error)) => Err(error.into()),
                _ => Err(crate::Error::Config("server background task stopped unexpectedly")),
            }
        },
        () = shutdown_signal() => Ok(()),
        () = cancellation.cancelled() => Ok(()),
        },
    };
    server.node_healthy.store(false, Ordering::Release);
    cancellation.cancel();
    let deadline = tokio::time::Instant::now() + SHUTDOWN_DEADLINE;
    let cleanup = async {
        // HTTP and Git work must finish before Cellar closes its event domain.
        let public_result = if public_ended {
            Ok(())
        } else {
            public.await.map_err(crate::Error::from)
        };
        server.receives.close();
        server.receives.wait().await;
        server.transfer_admission.close();
        server.transfer_admission.wait().await;
        let maintenance = server.finish_maintenance().await;
        let mut background_result = Ok(());
        while let Some(result) = background.join_next().await {
            let result = result.map_err(crate::Error::from).and_then(|result| result);
            background_result = background_result.and(result);
        }
        started.client.close_admission();
        // Restarts preserve this logical node. Explicit scale-down releases
        // ownership and waits for a successor before the transport closes.
        let shutdown_mode = *server.shutdown_mode.borrow();
        let cells = if node_ended {
            Ok(())
        } else {
            node.shutdown(shutdown_mode)
                .await
                .map_err(crate::cells::Error::sdk)
                .map_err(crate::Error::Cell)
                .and_then(|outcome| match outcome {
                    cellar::sdk::NodeOutcome::Stopped(report)
                        if report.ingress_drained
                            && report.application_work_drained
                            && report.cells_drained
                            && !report.drain_timed_out =>
                    {
                        Ok(())
                    }
                    _ => Err(crate::Error::Config(
                        "Cellar did not complete a graceful shutdown",
                    )),
                })
        };
        runtime.shutdown().await;
        management_shutdown.cancel();
        let management = if management_ended {
            Ok(())
        } else {
            management_task
                .await
                .map_err(crate::Error::from)
                .and_then(|result| result.map_err(crate::Error::from))
        };
        peer_work.close();
        peer_work.wait().await;
        let peers = transport.shutdown().await;
        public_result
            .and(maintenance)
            .and(background_result)
            .and(cells)
            .and(management)
            .and(peers)
    };
    let cleaned = tokio::time::timeout_at(deadline, cleanup)
        .await
        .map_err(|_| crate::Error::ShutdownTimeout)?;
    primary.and(cleaned)
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

async fn materialize_catalog(
    catalog: &CatalogStore,
    document: crate::catalog::CatalogDocument,
) -> Result<RepositoryIndex> {
    let version = document.version;
    let mut repositories = BTreeMap::new();
    for record in document.repositories.into_iter().filter(|record| {
        record.application == crate::catalog::RepositoryApplicationState::CellReady
    }) {
        let store = catalog.root().store.clone();
        let prefix = catalog.root().repository_prefix(&record.prefix)?;
        let layout = catalog.root().repository_layout(prefix.clone());
        let root = crab_metadata::capsule_protocol::load_root(&layout)
            .await
            .map_err(|source| crate::Error::Settings {
                source: Box::new(source),
            })?;
        let default_branch = root
            .record()
            .root()
            .head()
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
            projection_source: Mutex::new(None),
            maintenance: Mutex::new(MaintenanceState::default()),
            integrity: crate::integrity::Status::default(),
        };
        repositories.insert(
            (entry.owner.clone(), entry.name.clone()),
            Arc::new(repository),
        );
    }
    Ok(RepositoryIndex::new(version, repositories))
}

async fn refresh_catalog(server: Arc<Server>) {
    let Some(catalog) = server.catalog() else {
        return;
    };
    loop {
        tokio::select! {
            () = server.cancellation.cancelled() => return,
            () = tokio::time::sleep(Duration::from_secs(5)) => {}
        }
        let active_version = server.repositories.version();
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
        if document.version < active_version {
            server.catalog_healthy.store(false, Ordering::Release);
            server.metrics.record_catalog_refresh_failure();
            tracing::warn!(
                catalog_version = document.version,
                active_version,
                "repository catalog version moved backwards"
            );
            continue;
        }
        match server.install_catalog(document).await {
            Ok(()) => {
                server.catalog_healthy.store(true, Ordering::Release);
            }
            Err(error) => {
                server.catalog_healthy.store(false, Ordering::Release);
                server.metrics.record_catalog_refresh_failure();
                tracing::warn!(error = ?error, "repository catalog installation failed");
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
            let source =
                match crate::projection::current_source(&repository, &server, &server.cancellation)
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
                        if let Err(schedule_error) = repository
                            .schedule_maintenance(&server, MaintenanceTrigger::Repair)
                            .await
                        {
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
            match repository
                .schedule_maintenance(&server, MaintenanceTrigger::Poll(&source))
                .await
            {
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

fn management_router(server: Arc<Server>, peers: Router) -> Router {
    Router::new()
        .route("/healthz", get(|| async { Json(json!({"status": "ok"})) }))
        .route("/readyz", get(readiness))
        .route("/capacity", get(render_capacity))
        .route("/integrityz", get(integrity_status))
        .route("/metrics", get(render_metrics))
        .route(
            "/internal/repositories/{owner}/{name}/cell",
            get(cell_status),
        )
        .route("/internal/cells/v1/scale-down", post(perform_scale_down))
        .with_state(server)
        .fallback_service(peers)
}

async fn perform_scale_down(State(server): State<Arc<Server>>) -> Response {
    server
        .shutdown_mode
        .send_replace(cellar::sdk::ShutdownMode::Handoff);
    server.cancellation.cancel();
    (StatusCode::ACCEPTED, Json(json!({"status": "draining"}))).into_response()
}

async fn cell_status(
    State(server): State<Arc<Server>>,
    axum::extract::Path(key): axum::extract::Path<(String, String)>,
) -> Response {
    let Some(repository) = server.repositories.get(&key) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(router) = server.repository_cells() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let result: Result<_> = async {
        let cell = router.repository(repository.id)?;
        let state = cell.client.query::<crate::cells::repository::GetRepositoryLifecycle>(&repository.id, None, ()).await?;
        Ok(json!({"runtime": "cellar", "version": "0.1.0", "repository": repository.id, "revision": state.receipt.revision, "lifecycle": state.output}))
    }.await;
    match result {
        Ok(value) => Json(value).into_response(),
        Err(error) => {
            tracing::warn!(error = %error, "repository Cell status failed");
            StatusCode::SERVICE_UNAVAILABLE.into_response()
        }
    }
}

async fn render_capacity(State(server): State<Arc<Server>>) -> Response {
    let Some(capacity) = server.cell_capacity() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    (
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        Json(capacity),
    )
        .into_response()
}

async fn integrity_status(State(server): State<Arc<Server>>) -> Response {
    let mut failed = false;
    let mut complete = true;
    let repositories = server
        .repositories
        .values()
        .into_iter()
        .map(|repository| {
            let snapshot = repository.integrity.snapshot();
            failed |= snapshot.state == crate::integrity::State::Failed;
            complete &= snapshot.state == crate::integrity::State::Complete;
            json!({
                "owner": repository.config.owner,
                "name": repository.config.name,
                "proof": snapshot,
            })
        })
        .collect::<Vec<_>>();
    let (status, state) = if failed {
        (StatusCode::SERVICE_UNAVAILABLE, "failed")
    } else if complete {
        (StatusCode::OK, "complete")
    } else {
        (StatusCode::ACCEPTED, "pending")
    };
    (
        status,
        Json(json!({"status": state, "repositories": repositories})),
    )
        .into_response()
}

async fn render_metrics(State(server): State<Arc<Server>>) -> Response {
    let mut body = server.metrics.render(crate::metrics::RuntimeSnapshot {
        repositories: server.repositories.len(),
        catalog_healthy: server.catalog_healthy.load(Ordering::Acquire),
        draining: server.cancellation.is_cancelled(),
        receive_workers: server.receives.len(),
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
    });
    if let Some(services) = &server.cell_services {
        let work = services.application_work();
        let resources = services.sample_metrics();
        body.push_str(&services.event_metrics().prometheus());
        body.push_str(&format!("# TYPE crab_cellar_pending_events gauge\ncrab_cellar_pending_events {}\n# TYPE crab_cellar_retired_events_total counter\ncrab_cellar_retired_events_total {}\n# TYPE crab_cellar_rss_bytes gauge\ncrab_cellar_rss_bytes {}\n# TYPE crab_cellar_open_file_descriptors gauge\ncrab_cellar_open_file_descriptors {}\n", work.pending_events, work.retired_events, resources.rss_bytes, resources.open_fds));
    }
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
    if !server.accepts_new_cells() {
        return Err(crate::Error::Config("Cellar is not accepting requests"));
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

#[derive(Default, serde::Deserialize)]
struct RepositorySelection {
    owner: Option<String>,
    name: Option<String>,
}

async fn catalog(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Query(selection): Query<RepositorySelection>,
) -> app::Result<Json<serde_json::Value>> {
    if selection.owner.is_some() != selection.name.is_some() {
        return Err(app::Error::Invalid(
            "owner and name must be provided together",
        ));
    }
    app::actor(&principal)?;
    let mut repositories = Vec::new();
    for repository in server
        .repositories
        .values()
        .into_iter()
        .filter(|repository| {
            principal.can_read(&repository.config)
                && selection.owner.as_ref().is_none_or(|owner| {
                    owner == &repository.config.owner
                        && selection.name.as_ref() == Some(&repository.config.name)
                })
        })
    {
        let protections = repository.branch_protections(&server).await?;
        let lifecycle = repository.lifecycle(&server).await?;
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
            event = "http_response_ready",
            status = response.status().as_u16(),
            elapsed_us = started.elapsed().as_micros(),
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
    let authentication_started = Instant::now();
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
    tracing::debug!(
        target: "crab_http_server::action",
        event = "http_authentication_completed",
        elapsed_us = authentication_started.elapsed().as_micros(),
    );
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
    let check_archive = !denied && !rejected_mutation && unsafe_method && !git_request;
    let deferred_archive = (check_archive && command_checks_archive(request.uri().path()))
        .then(|| request.uri().clone());
    let archive_check_started = Instant::now();
    let archived_response = if check_archive && deferred_archive.is_none() {
        archived_mutation_response(&server, &principal, request.uri().path()).await
    } else {
        None
    };
    tracing::debug!(
        target: "crab_http_server::action",
        event = "http_archive_check_completed",
        command_checks_archive = deferred_archive.is_some(),
        elapsed_us = archive_check_started.elapsed().as_micros(),
    );
    request.extensions_mut().insert(principal.clone());
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
    // The shipped archive error takes precedence over invalid mutation input.
    // Guarded commands need the HTTP lookup only on client errors; never replace
    // a server error that may represent an ambiguous committed mutation.
    if let Some(uri) = deferred_archive
        && response.status().is_client_error()
        && let Some(archived) = archived_mutation_response(&server, &principal, uri.path()).await
    {
        response = archived;
    }
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

fn command_checks_archive(path: &str) -> bool {
    // All issue/comment/label mutations check lifecycle inside their command.
    // Other mutation families retain their existing preflight policy.
    path.strip_prefix("/api/repos/")
        .is_some_and(|path| matches!(path.split('/').nth(2), Some("issues" | "labels")))
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
    app::actor(principal).ok()?;
    match repository.lifecycle(server).await {
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
pub(crate) mod maintenance_tests;

#[cfg(test)]
#[path = "server_peer_e2e_tests.rs"]
mod peer_e2e_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    #[tokio::test]
    async fn scoped_catalog_skips_unrelated_repository_cells() {
        let server = super::maintenance_tests::fixture().await;
        let existing = server.repositories.values().pop().unwrap();
        let mut config = existing.config.clone();
        config.owner = "other".into();
        config.name = "unavailable".into();
        let unavailable = Arc::new(Repository {
            id: Uuid::from_bytes([2; 16]),
            config,
            identity: existing.identity.clone(),
            store: existing.store.clone(),
            layout: existing.layout.clone(),
            pinned: Mutex::new(None),
            projection_source: Mutex::new(None),
            maintenance: Mutex::new(MaintenanceState::default()),
            integrity: crate::integrity::Status::default(),
        });
        server.repositories.replace(RepositoryIndex::new(
            1,
            BTreeMap::from([
                (("team".into(), "repo".into()), existing),
                (("other".into(), "unavailable".into()), unavailable),
            ]),
        ));
        let response = router(Arc::clone(&server))
            .oneshot(
                Request::builder()
                    .uri("/api/repos?owner=team&name=repo")
                    .header("host", "localhost:8788")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        super::maintenance_tests::close(&server).await;
        assert_eq!(
            (status, value["repositories"].as_array().map(Vec::len)),
            (StatusCode::OK, Some(1))
        );
    }

    #[tokio::test]
    async fn delayed_catalog_materialization_preserves_newer_membership() {
        let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
        let catalog = CatalogStore::new(crate::storage_root::StorageRoot::memory(
            store,
            "catalog-ordering",
        ));
        let member = crate::RepositoryMember {
            subject: "revoked-member".into(),
            name: "Member".into(),
            access: crate::RepositoryAccess::Write,
        };
        let record = catalog
            .create_repository(
                "team".into(),
                "project".into(),
                "team/project".into(),
                "main".into(),
                String::new(),
                vec![member],
            )
            .await
            .unwrap();
        catalog.mark_cell_ready(record.id).await.unwrap();
        // Import and periodic refresh both load a document before awaiting Git
        // metadata. An older import may finish after refresh installs revocation.
        let (delayed, _) = catalog.load().await.unwrap();
        catalog
            .set_members("team", "project", Vec::new(), false)
            .await
            .unwrap();
        let (current, _) = catalog.load().await.unwrap();
        let repositories =
            RepositorySet::from(materialize_catalog(&catalog, current).await.unwrap());
        repositories.replace(materialize_catalog(&catalog, delayed).await.unwrap());
        assert!(
            repositories
                .by_id(record.id)
                .unwrap()
                .config
                .members
                .is_empty()
        );
    }

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

    fn local_resources(
        memory_bytes: u64,
        free_disk_bytes: u64,
        available_file_descriptors: usize,
    ) -> crate::resources::LocalResources {
        crate::resources::LocalResources {
            memory_bytes,
            disk_limit_bytes: free_disk_bytes,
            disk_capacity_bytes: free_disk_bytes,
            free_disk_bytes,
            available_file_descriptors,
            job_credits: 16,
        }
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
            CellRuntimeBudget::from_resources(local_resources(GIB - 1, 30 * GIB, 10_000)).is_err()
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
            CellRuntimeBudget::from_resources(crate::resources::LocalResources {
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
            repository_cells: None,
            cell_services: None,
            shutdown_mode: tokio::sync::watch::channel(cellar::sdk::ShutdownMode::Preserve).0,
            cell_test_runtime: None,
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
        for path in ["/healthz", "/readyz", "/integrityz"] {
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
        let management = management_router(Arc::clone(&server), Router::new());
        for (path, expected) in [
            ("/healthz", StatusCode::OK),
            ("/readyz", StatusCode::SERVICE_UNAVAILABLE),
            ("/integrityz", StatusCode::OK),
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
            "/internal/cells/v1/scale-down",
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
