//! Single-process HTTP composition for object-storage-backed Crab repositories.
#![recursion_limit = "256"]
mod api;
mod app;
mod assets;
mod auth;
pub mod catalog;
mod cells;
mod config;
mod git;
mod git_import;
mod integrity;
mod local_disk;
mod metrics;
mod peer;
mod repository;
mod resources;
mod server;
mod storage_root;
#[cfg(test)]
mod test_support;
mod transfer_admission;

pub use cells::directory::DirectoryState;
pub use cells::organization::teams::OrganizationTeamState;
pub use cells::repository::collaboration::lifecycle::LifecycleStatus;
pub use cells::repository::search::index::{
    RebuildSearchInput, SearchIndexPhase, SearchIndexProgress, SearchIndexStatus,
};
pub use config::{
    AuthProvider, BranchProtection, CellDurabilityMode, CellsConfig, Config, GitHubConfig,
    ImportConfig, OidcConfig, RepositoryAccess, RepositoryConfig, RepositoryGroup,
    RepositoryMember, StorageConfig,
};
pub use server::serve;
pub use server::storage::probe_storage;

/// Returns the resource-derived Cell admission envelope for this process.
pub fn cell_capacity(config: &Config) -> Result<Vec<u8>> {
    config.validate()?;
    server::capacity::report(&config.cells.data_dir, config.cells.local_disk_limit_bytes)
}

/// Initializes the empty application Cell for one newly cataloged repository.
pub async fn initialize_repository_cell(config: &Config, repository: uuid::Uuid) -> Result<()> {
    config.validate()?;
    cells::initialize_repository(config, repository).await
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
    Cell(#[from] cells::Error),
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
    #[error("capsule repository read failed")]
    Read(#[source] Box<crab_read::ReadError>),
    #[error("repository browse index record is corrupt")]
    BrowseIndexes(#[source] Box<crab_metadata::error::MetadataError>),
    #[error("repository maintenance failed")]
    Maintenance(#[source] Box<dyn std::error::Error + Send + Sync>),
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

impl From<crab_read::ReadError> for Error {
    fn from(source: crab_read::ReadError) -> Self {
        Self::Read(Box::new(source))
    }
}

impl From<repository::maintenance::Error> for Error {
    fn from(source: repository::maintenance::Error) -> Self {
        Self::Maintenance(Box::new(source))
    }
}
