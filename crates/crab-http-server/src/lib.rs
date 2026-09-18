//! Single-process HTTP composition for object-storage-backed Crab repositories.
mod api;
mod app;
mod archive;
mod assets;
mod assignees;
mod auth;
mod branches;
pub mod catalog;
mod cells;
mod checks;
mod config;
mod contents;
mod git;
mod git_objects;
mod issues;
mod labels;
mod lfs;
mod local_disk;
mod maintenance;
mod metrics;
mod peer;
mod peer_tls;
mod pulls;
mod receive;
mod releases;
mod repository_settings;
mod server;
mod state_stream;
mod statuses;
mod storage_root;
mod transfer_admission;

pub use config::{
    BranchProtection, CellsConfig, Config, OidcConfig, RepositoryAccess, RepositoryConfig,
    RepositoryMember, StorageConfig,
};
pub use server::{probe_storage, serve};
pub use state_stream::state_observing_body;

/// Returns the resource-derived Cell admission envelope for this process.
pub fn cell_capacity(config: &Config) -> Result<Vec<u8>> {
    config.validate()?;
    server::cell_capacity_report(&config.cells.data_dir, config.cells.local_disk_limit_bytes)
}

/// Returns the canonical release descriptor compiled into this server binary.
pub fn cell_release_descriptor() -> Result<Vec<u8>> {
    Ok(cells::compiled_registry()?.release_bytes().to_vec())
}

/// Uploads the compiled descriptor and conditionally prepares it for rollout.
pub async fn prepare_cell_release(
    config: &Config,
    expected_revision: u64,
    image: &str,
) -> Result<Vec<u8>> {
    config.validate()?;
    cells::prepare_release(config, expected_revision, image).await
}

/// Initializes an empty application or admits this binary's selected release.
pub async fn bootstrap_cell_release(config: &Config, image: &str) -> Result<Vec<u8>> {
    config.validate()?;
    cells::bootstrap_release(config, image).await
}

/// Returns the canonical release selection stored for this application.
pub async fn cell_release_status(config: &Config) -> Result<Vec<u8>> {
    config.validate()?;
    cells::release_status(config).await
}

/// Returns a bounded page of Cells still pending or failed for the selected release.
pub async fn cell_release_migrations(
    config: &Config,
    after: Option<&str>,
    limit: usize,
) -> Result<Vec<u8>> {
    config.validate()?;
    cells::release_migrations(config, after, limit).await
}

/// Activates the prepared release after the requested live-node quorum is eligible.
pub async fn activate_cell_release(
    config: &Config,
    expected_revision: u64,
    minimum_eligible_nodes: usize,
) -> Result<Vec<u8>> {
    config.validate()?;
    cells::activate_release(config, expected_revision, minimum_eligible_nodes).await
}

/// Enters offline maintenance, optionally collects old immutable objects, and activates.
pub async fn enter_cell_maintenance(
    config: &Config,
    expected_revision: u64,
    retention_grace: Option<std::time::Duration>,
    retention_max_deletes: Option<u64>,
) -> Result<Vec<u8>> {
    config.validate()?;
    let retention = match (retention_grace, retention_max_deletes) {
        (Some(grace), max_deletes) => {
            let grace_ms = u64::try_from(grace.as_millis())
                .map_err(|_| Error::Config("Cell retention grace is too large"))?;
            let max_deletes = max_deletes.unwrap_or(10_000);
            crab_cell_runtime::GarbageCollectionPolicy::new(0, grace_ms, max_deletes)?;
            Some(cells::RetentionRequest { grace, max_deletes })
        }
        (None, None) => None,
        (None, Some(_)) => {
            return Err(Error::Config(
                "Cell retention deletion limit requires a retention grace",
            ));
        }
    };
    cells::enter_maintenance(config, expected_revision, retention).await
}

/// Initializes the empty application Cell for one newly cataloged repository.
pub async fn initialize_repository_cell(config: &Config, repository: uuid::Uuid) -> Result<()> {
    config.validate()?;
    cells::initialize_repository(config, repository).await
}

/// Returns the durable control state for one cataloged repository Cell.
pub async fn repository_cell_status(config: &Config, owner: &str, name: &str) -> Result<Vec<u8>> {
    config.validate()?;
    cells::repository_status(config, owner, name).await
}

/// Reports whether one exact node boot session has a currently valid advertisement.
pub async fn cell_node_status(config: &Config, session: &str) -> Result<Vec<u8>> {
    config.validate()?;
    cells::node_status(config, session).await
}

/// Creates or reopens one immutable application backup pin and verifies it.
pub async fn create_cell_backup(config: &Config, pin: &str) -> Result<Vec<u8>> {
    config.validate()?;
    cells::create_backup(config, pin).await
}

/// Reopens one immutable application backup pin and verifies every dependency.
pub async fn verify_cell_backup(config: &Config, pin: &str) -> Result<Vec<u8>> {
    config.validate()?;
    cells::verify_backup(config, pin).await
}

/// Restores one verified pin into a separate prefix in the configured bucket.
pub async fn restore_cell_backup(
    config: &Config,
    pin: &str,
    destination_prefix: &str,
) -> Result<Vec<u8>> {
    config.validate()?;
    cells::restore_backup(config, pin, destination_prefix).await
}

/// Startup and server lifecycle errors with their original sources retained.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid server configuration: {0}")]
    Config(&'static str),
    #[error("identity initialization failed")]
    Identity {
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("configuration or listener I/O failed")]
    Io(#[from] std::io::Error),
    #[error("invalid TOML configuration")]
    Toml(#[from] toml::de::Error),
    #[error("JSON encoding failed")]
    Json(#[from] serde_json::Error),
    #[error("object storage configuration failed")]
    Storage(#[from] crab_storage::StorageError),
    #[error("object storage coordination failed")]
    Coordination(#[from] crab_coordination::CoordinationError),
    #[error("embedded Cell runtime initialization failed")]
    Cell(#[from] crab_cell_runtime::Error),
    #[error("private Cell TLS setup failed: {context}")]
    PeerTls {
        context: &'static str,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("peer address discovery failed: {context}")]
    PeerDiscovery {
        context: &'static str,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("object storage preflight failed: {0}")]
    StorageProbe(&'static str),
    #[error("local staging setup failed")]
    LocalStaging {
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("repository initialization failed")]
    Remote(#[from] crab_remote_git::Error),
    #[error("repository maintenance failed")]
    Maintenance(#[from] crab_write::WriteError),
    #[error("repository catalog operation failed")]
    Catalog(#[from] catalog::CatalogError),
    #[error("server metrics setup failed")]
    Metrics(#[from] metrics_exporter_prometheus::BuildError),
    #[error("repository maintenance task failed")]
    Worker(#[from] tokio::task::JoinError),
    #[error("server shutdown exceeded its 110-second deadline")]
    ShutdownTimeout,
    #[error("repository settings could not be loaded")]
    Settings {
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("server logging initialization failed")]
    Logging {
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("server readiness check failed")]
    Healthcheck {
        #[source]
        source: reqwest::Error,
    },
}

/// Server startup or lifecycle result.
pub type Result<T> = std::result::Result<T, Error>;
