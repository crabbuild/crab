//! Process composition and lifecycle for the repository application.

use crate::{
    Config, Result, auth::Authentication, catalog::CatalogStore, git_import,
    repository::catalog::RepositorySet, transfer_admission::TransferAdmission,
};
use capacity::CellCapacityReport;
use crab_remote_git::{OperationLimits, RemoteGitRuntime, RepositoryOptions};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

mod background;
pub(crate) mod capacity;
pub(crate) mod http;
pub(crate) mod management;
pub(crate) mod storage;

pub(crate) const MAX_DEPENDENCY_FILE_BYTES: u64 = 512 * 1024 * 1024;
const READ_ADMISSION_CAPACITY: usize = 16;
const GIT_ADMISSION_CAPACITY: usize = 4;
const APP_ADMISSION_CAPACITY: usize = 8;
const MAINTENANCE_ADMISSION_CAPACITY: usize = 2;
const SHUTDOWN_DEADLINE: Duration = Duration::from_secs(110);

pub(crate) struct Server {
    pub(crate) repositories: RepositorySet,
    pub(crate) runtime: Arc<RemoteGitRuntime>,
    pub(crate) repository_cells: Option<Arc<crate::cells::RepositoryClient>>,
    pub(crate) cell_services: Option<Arc<cellar::sdk::HostServices>>,
    #[cfg(test)]
    pub(crate) cell_test_runtime: Option<Arc<crate::cells::runtime::TestRuntime>>,
    pub(crate) shutdown_mode: tokio::sync::watch::Sender<cellar::sdk::ShutdownMode>,
    pub(crate) options: RepositoryOptions,
    pub(crate) cursor_key: [u8; 32],
    pub(crate) admission: Semaphore,
    pub(crate) transfer_admission: TransferAdmission,
    pub(crate) local_staging: crate::local_disk::LocalStaging,
    pub(crate) app_admission: Semaphore,
    pub(crate) maintenance_admission: Arc<Semaphore>,
    pub(crate) cancellation: CancellationToken,
    pub(crate) receives: tokio_util::task::TaskTracker,
    pub(crate) auth: Option<Authentication>,
    pub(crate) git_import: Option<git_import::ImportContext>,
    pub(crate) catalog: Option<CatalogStore>,
    pub(crate) catalog_healthy: AtomicBool,
    pub(crate) node_healthy: AtomicBool,
    pub(crate) cell_capacity: CellCapacityReport,
    pub(crate) metrics: crate::metrics::Metrics,
}

impl Server {
    #[cfg(test)]
    pub(crate) fn for_test(
        repositories: RepositorySet,
        transfer_admission: TransferAdmission,
    ) -> Self {
        Self {
            repositories,
            runtime: Arc::new(RemoteGitRuntime::default()),
            repository_cells: None,
            cell_services: None,
            cell_test_runtime: None,
            shutdown_mode: tokio::sync::watch::channel(cellar::sdk::ShutdownMode::Preserve).0,
            options: RepositoryOptions::default(),
            cursor_key: [0; 32],
            admission: Semaphore::new(READ_ADMISSION_CAPACITY),
            transfer_admission,
            local_staging: crate::local_disk::LocalStaging::for_test(),
            app_admission: Semaphore::new(APP_ADMISSION_CAPACITY),
            maintenance_admission: Arc::new(Semaphore::new(MAINTENANCE_ADMISSION_CAPACITY)),
            cancellation: CancellationToken::new(),
            receives: tokio_util::task::TaskTracker::new(),
            auth: None,
            git_import: None,
            catalog: None,
            catalog_healthy: AtomicBool::new(false),
            node_healthy: AtomicBool::new(false),
            cell_capacity: capacity::for_test(),
            metrics: crate::metrics::Metrics::new().unwrap(),
        }
    }

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
        let next = crate::repository::catalog::materialize(&catalog, document).await?;
        self.repositories.replace(next);
        Ok(())
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

    pub(crate) async fn finish_maintenance(&self) -> Result<()> {
        let mut result = Ok(());
        for repository in self.repositories.values() {
            result = result.and(repository.finish_maintenance().await);
        }
        result
    }

    #[cfg(test)]
    pub(crate) async fn shutdown_runtimes(&self) -> Result<()> {
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
    let repositories = crate::repository::catalog::materialize(&catalog, document).await?;
    let auth = match config.auth.clone() {
        Some(config) => Some(Authentication::new(config, catalog.root()).await.map_err(
            |source| crate::Error::Identity {
                source: Box::new(source),
            },
        )?),
        None => None,
    };
    std::fs::create_dir_all(&config.cells.data_dir)?;
    let capacity =
        CellCapacityReport::measure(&config.cells.data_dir, config.cells.local_disk_limit_bytes)?;
    let budget = capacity.admission;
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
        transfer_admission: storage::transfer_admission(&catalog),
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
    let management = management::router(server.clone(), incoming);
    let stopped = management_shutdown.clone();
    let mut management_task = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async move {
        axum::serve(tls.listener(management_listener), management)
            .with_graceful_shutdown(stopped.cancelled_owned())
            .await
    }));
    let mut background = tokio::task::JoinSet::new();
    let refresh = server.clone();
    background.spawn(async move {
        background::refresh_catalog(refresh).await;
        Ok(())
    });
    let projection = server.clone();
    background.spawn(async move { background::sweep_projections(projection).await });
    let integrity = server.clone();
    background.spawn(async move {
        crate::integrity::run(integrity).await;
        Ok(())
    });
    let stop = cancellation.clone();
    let public_app = http::router(server.clone());
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

#[cfg(test)]
#[path = "server_peer_e2e_tests.rs"]
mod peer_e2e_tests;

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
