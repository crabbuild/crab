//! Single-process HTTP composition for object-storage-backed Crab repositories.
mod api;
mod assets;
mod config;
mod server;

pub use config::{Config, RepositoryConfig};
pub use server::serve;

/// Startup and server lifecycle errors with their original sources retained.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid server configuration: {0}")]
    Config(&'static str),
    #[error("configuration or listener I/O failed")]
    Io(#[from] std::io::Error),
    #[error("invalid TOML configuration")]
    Toml(#[from] toml::de::Error),
    #[error("object storage configuration failed")]
    Storage(#[from] crab_storage::StorageError),
    #[error("repository initialization failed")]
    Remote(#[from] crab_remote_git::Error),
}

/// Server startup or lifecycle result.
pub type Result<T> = std::result::Result<T, Error>;
