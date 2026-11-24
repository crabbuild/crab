//! Repository reads, pinned browse snapshots, and maintenance ownership.

pub(crate) mod code_index;
pub(crate) mod commit_closures;
pub(crate) mod maintenance;
pub(crate) mod projection;
pub(crate) mod settings;

use crate::{
    RepositoryConfig, Result, cells::repository::projection::SourceIdentity, server::Server,
};
use crab_remote_git::{RemoteGitRepository, RepositoryIdentity, RepositoryOptions};
use crab_storage::{Store, StoreLayout};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

pub(crate) mod access;
pub(crate) mod catalog;
pub(crate) const BROWSE_PROBE_INTERVAL: Duration = Duration::from_secs(2);

pub(crate) struct Repository {
    pub(crate) id: Uuid,
    pub(crate) organization: Option<Uuid>,
    pub(crate) ownership_epoch: u32,
    pub(crate) forked_from: Option<Uuid>,
    pub(crate) topics: Vec<String>,
    pub(crate) config: RepositoryConfig,
    pub(crate) store: Store,
    pub(crate) layout: StoreLayout<Store>,
    edge_reads: Option<EdgeReads>,
    pub(crate) identity: RepositoryIdentity,
    pinned: Mutex<Option<PinnedRepository>>,
    pub(crate) projection_source: Mutex<Option<crate::repository::projection::CachedSource>>,
    maintenance: Mutex<MaintenanceState>,
    pub(crate) integrity: crate::integrity::Status,
}

#[derive(Default)]
struct MaintenanceState {
    worker: Option<
        tokio::task::JoinHandle<crate::repository::maintenance::Result<Option<SourceIdentity>>>,
    >,
    completed: Option<(SourceIdentity, Instant)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MaintenanceSchedule {
    Started,
    Running,
    Completed,
    Deferred,
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

struct EdgeReads {
    layout: StoreLayout<Store>,
    identity: RepositoryIdentity,
    pinned: Mutex<Option<PinnedRepository>>,
}

#[derive(Clone)]
pub(crate) struct BrowseRepository {
    pub(crate) git: RemoteGitRepository,
    pub(crate) source: crate::cells::repository::projection::SourceIdentity,
}

impl Repository {
    pub(crate) fn fork_source(&self, server: &Server) -> Option<Arc<Self>> {
        let organization = self.organization?;
        server
            .repositories
            .by_id(self.forked_from?)
            .filter(|source| {
                source.organization == Some(organization) && source.forked_from.is_none()
            })
    }

    pub(crate) fn new(
        id: Uuid,
        config: RepositoryConfig,
        layout: StoreLayout<Store>,
        identity: RepositoryIdentity,
    ) -> Self {
        Self {
            id,
            organization: None,
            ownership_epoch: 0,
            forked_from: None,
            topics: Vec::new(),
            config,
            store: layout.store().clone(),
            edge_reads: None,
            layout,
            identity,
            pinned: Mutex::new(None),
            projection_source: Mutex::new(None),
            maintenance: Mutex::new(MaintenanceState::default()),
            integrity: crate::integrity::Status::default(),
        }
    }

    pub(crate) fn with_read_layout(
        mut self,
        read_layout: Option<(StoreLayout<Store>, RepositoryIdentity)>,
    ) -> Self {
        self.edge_reads = read_layout.map(|(layout, identity)| EdgeReads {
            layout,
            identity,
            pinned: Mutex::new(None),
        });
        self
    }

    pub(crate) async fn finish_maintenance(&self) -> Result<()> {
        // Keep scheduling serialized until the worker joins, so shutdown
        // cannot release ownership while the previous job is still active.
        let mut state = self.maintenance.lock().await;
        let Some(task) = state.worker.take() else {
            return Ok(());
        };
        match task.await {
            Ok(Ok(_)) | Ok(Err(maintenance::Error::Cancelled)) => Ok(()),
            Ok(Err(error)) => Err(error.into()),
            Err(error) => Err(error.into()),
        }
    }

    #[cfg(test)]
    pub(crate) async fn expire_browse_probe(&self) {
        self.invalidate_browse_probes().await;
    }

    pub(crate) async fn invalidate(&self) {
        // Maintenance can finish without changing Git state. Keep verified
        // data while forcing the next read to check ref and index authority.
        self.invalidate_browse_probes().await;
        *self.projection_source.lock().await = None;
        self.maintenance.lock().await.completed = None;
    }

    async fn invalidate_browse_probes(&self) {
        for pinned in
            std::iter::once(&self.pinned).chain(self.edge_reads.iter().map(|reader| &reader.pinned))
        {
            if let Some(cached) = pinned.lock().await.as_mut() {
                cached.checked = None;
            }
        }
    }

    pub(crate) async fn schedule_maintenance(
        &self,
        server: &Server,
        trigger: MaintenanceTrigger<'_>,
    ) -> Result<MaintenanceSchedule> {
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
            self.invalidate_browse_probes().await;
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
                    client: server.cells().map(|router| (*router).clone()),
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
        crate::repository::maintenance::run(
            self.layout.clone(),
            Arc::clone(&server.maintenance_admission),
            None,
            cancellation.clone(),
            crate::repository::maintenance::Pass::ForegroundCheckpoint,
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

    pub(crate) async fn ref_oid(&self, name: &str) -> Result<Option<String>> {
        let names = std::collections::BTreeSet::from([name.to_owned()]);
        Ok(self.ref_oids(&names).await?.remove(name))
    }

    pub(crate) async fn ref_oids(
        &self,
        names: &std::collections::BTreeSet<String>,
    ) -> Result<std::collections::BTreeMap<String, String>> {
        Ok(self.ref_view_for_refs(names).await?.refs().clone())
    }

    pub(crate) async fn ref_view_for_refs(
        &self,
        names: &std::collections::BTreeSet<String>,
    ) -> Result<crab_read::capsule_protocol::CapsuleRefView> {
        let root = crab_metadata::capsule_protocol::load_root(&self.layout)
            .await
            .map_err(|source| crate::Error::Settings {
                source: Box::new(source),
            })?;
        // Capture selected heads from one authoritative view. List and review
        // freshness need no packs, browse indexes, or unrelated branches.
        Ok(
            crab_read::capsule_protocol::open_ref_view_from_root_for_refs(
                &self.layout,
                root,
                names,
            )
            .await?,
        )
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

    pub(crate) async fn default_ref(&self) -> Result<String> {
        let root = crab_metadata::capsule_protocol::load_root(&self.layout)
            .await
            .map_err(|source| crate::Error::Settings {
                source: Box::new(source),
            })?;
        Ok(root.record().root().head().to_owned())
    }

    pub(crate) async fn open_git_view(
        &self,
    ) -> Result<crab_read::capsule_protocol::CapsuleRepositoryView> {
        self.open_git_view_in(&self.layout).await
    }

    async fn open_git_view_in(
        &self,
        layout: &StoreLayout<Store>,
    ) -> Result<crab_read::capsule_protocol::CapsuleRepositoryView> {
        // Ref authority comes from origin even when immutable reads use CDN.
        let root = crab_metadata::capsule_protocol::load_root(&self.layout)
            .await
            .map_err(|source| crate::Error::Settings {
                source: Box::new(source),
            })?;
        crab_read::capsule_protocol::open_git_view_from_root(
            layout,
            root,
            crab_read::capsule_protocol::CapsuleReadLimits {
                max_capsule_bytes: 2 * 1024 * 1024 * 1024,
                max_frontier_bytes: 2 * 1024 * 1024 * 1024,
            },
        )
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
        self.open_capsule(&self.layout, &self.identity, server, options, cancellation)
            .await
    }

    pub(crate) async fn open_capsule_read_repository(
        &self,
        server: &Server,
        options: RepositoryOptions,
        cancellation: &CancellationToken,
    ) -> Result<(
        crab_read::capsule_protocol::CapsuleRepositoryView,
        RemoteGitRepository,
    )> {
        let (layout, identity) = self
            .edge_reads
            .as_ref()
            .map_or((&self.layout, &self.identity), |reader| {
                (&reader.layout, &reader.identity)
            });
        self.open_capsule(layout, identity, server, options, cancellation)
            .await
    }

    async fn open_capsule(
        &self,
        layout: &StoreLayout<Store>,
        identity: &RepositoryIdentity,
        server: &Server,
        options: RepositoryOptions,
        cancellation: &CancellationToken,
    ) -> Result<(
        crab_read::capsule_protocol::CapsuleRepositoryView,
        RemoteGitRepository,
    )> {
        let view = self.open_git_view_in(layout).await?;
        let repository = view
            .git_repository_from_store(
                layout.clone(),
                identity.clone(),
                Arc::clone(&server.runtime),
                options,
                2 * 1024 * 1024 * 1024,
                cancellation,
                None,
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
                let indexes = crate::repository::projection::load_browse_indexes(&self.layout).await?
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
        self.open_pinned(
            &self.layout,
            &self.identity,
            &self.pinned,
            server,
            cancellation,
            true,
        )
        .await
    }

    pub(crate) async fn revalidate(
        &self,
        server: &Server,
        cancellation: &CancellationToken,
    ) -> Result<BrowseRepository> {
        // A 304 reuses browser-held bytes as a current response. The browse
        // freshness window cannot authorize that decision after a ref/epoch
        // change; unchanged captures still reuse their verified reader.
        self.open_pinned(
            &self.layout,
            &self.identity,
            &self.pinned,
            server,
            cancellation,
            false,
        )
        .await
    }

    pub(crate) async fn open_read(
        &self,
        server: &Server,
        cancellation: &CancellationToken,
    ) -> Result<BrowseRepository> {
        use crab_storage::cdn::CdnConsumer;

        let Some(reader) = &self.edge_reads else {
            self.layout.observe_cdn_read_target(CdnConsumer::Server);
            return self.open(server, cancellation).await;
        };
        reader.layout.observe_cdn_read_target(CdnConsumer::Server);
        self.open_pinned(
            &reader.layout,
            &reader.identity,
            &reader.pinned,
            server,
            cancellation,
            true,
        )
        .await
    }

    pub(crate) fn read_layout(&self) -> &StoreLayout<Store> {
        self.edge_reads
            .as_ref()
            .map_or(&self.layout, |reader| &reader.layout)
    }

    async fn open_pinned(
        &self,
        layout: &StoreLayout<Store>,
        identity: &RepositoryIdentity,
        pinned: &Mutex<Option<PinnedRepository>>,
        server: &Server,
        cancellation: &CancellationToken,
        reuse_recent: bool,
    ) -> Result<BrowseRepository> {
        let mut pinned = tokio::select! {
            _ = cancellation.cancelled() => return Err(crab_remote_git::Error::Cancelled.into()),
            pinned = pinned.lock() => pinned,
        };
        if let Some(cached) = pinned.as_mut() {
            if reuse_recent
                && cached
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
            cached = self.open_browse(layout, identity, server, server.options, cancellation) => cached?,
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
        layout: &StoreLayout<Store>,
        identity: &RepositoryIdentity,
        server: &Server,
        options: RepositoryOptions,
        cancellation: &CancellationToken,
    ) -> Result<PinnedRepository> {
        let view = crate::repository::projection::open_view(layout).await?;
        let state_digest = view.state_digest();
        let browse_indexes = view.browse_indexes().cloned();
        let source = crate::repository::projection::source_identity(&view.git_snapshot()?)?;
        let git = view
            .git_repository_from_store(
                layout.clone(),
                identity.clone(),
                Arc::clone(&server.runtime),
                options,
                2 * 1024 * 1024 * 1024,
                cancellation,
                None,
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

#[cfg(test)]
mod tests;
