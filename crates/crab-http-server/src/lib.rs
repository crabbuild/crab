//! Single-process HTTP composition for object-storage-backed Crab repositories.
mod api;
mod app;
mod app_storage;
mod assets;
mod assignees;
mod auth;
mod checks;
mod config;
mod contents;
mod git;
mod issues;
mod labels;
mod lfs;
mod maintenance;
mod pulls;
mod receive;
mod server;
mod statuses;

pub use config::{
    BranchProtection, Config, OidcConfig, RepositoryAccess, RepositoryConfig, RepositoryMember,
};
pub use server::serve;

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
    #[error("object storage configuration failed")]
    Storage(#[from] crab_storage::StorageError),
    #[error("repository initialization failed")]
    Remote(#[from] crab_remote_git::Error),
    #[error("repository maintenance failed")]
    Maintenance(#[from] crab_write::WriteError),
    #[error("repository maintenance task failed")]
    Worker(#[from] tokio::task::JoinError),
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
