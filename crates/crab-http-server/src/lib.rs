//! Single-process HTTP composition for object-storage-backed Crab repositories.
mod api;
mod app;
mod app_storage;
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
mod maintenance;
mod metrics;
mod peer;
mod pulls;
mod receive;
mod releases;
mod repository_settings;
mod server;
mod statuses;
mod storage_root;
mod transfer_admission;

pub use config::{
    BranchProtection, Config, OidcConfig, RepositoryAccess, RepositoryConfig, RepositoryMember,
    StorageConfig,
};
pub use server::{probe_storage, serve};

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

/// Activates the prepared compiled release after exact Cell compatibility checks.
pub async fn activate_cell_release(config: &Config, expected_revision: u64) -> Result<Vec<u8>> {
    config.validate()?;
    cells::activate_release(config, expected_revision).await
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
    #[error("object storage preflight failed: {0}")]
    StorageProbe(&'static str),
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
