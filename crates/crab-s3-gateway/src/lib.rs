//! S3 protocol composition for logical Crab repositories.

mod attributes;
mod auth;
mod config;
mod content;
mod gateway;
mod multipart;
mod mutation;
mod namespace;
mod repository;
mod server;

pub use config::{Config, CredentialConfig, RepositoryAccess, RepositoryConfig, RepositoryMember};
pub use server::{initialize, serve};

/// Gateway startup, configuration, and runtime failures.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid gateway configuration: {0}")]
    Config(&'static str),
    #[error("gateway configuration I/O failed")]
    Io(#[from] std::io::Error),
    #[error("invalid gateway TOML configuration")]
    Toml(#[from] toml::de::Error),
    #[error("invalid S3 endpoint domain")]
    Host(#[from] s3s::host::DomainError),
    #[error("object storage configuration failed")]
    Storage(#[from] crab_storage::StorageError),
    #[error("repository reader configuration failed")]
    Remote(#[from] crab_remote_git::Error),
    #[error("repository read publication failed")]
    Write(#[from] crab_write::WriteError),
    #[error("repository metadata failed")]
    Metadata(#[from] crab_metadata::error::MetadataError),
    #[error("repository cache setup failed")]
    Cache(#[from] crab_cache_store::CacheStoreError),
    #[error("repository hydration failed")]
    Read(#[from] crab_read::ReadError),
    #[error("Git LFS hydration failed")]
    Lfs(#[from] crab_lfs::LfsError),
    #[error("S3 object attributes are corrupt")]
    Attributes {
        #[source]
        source: serde_json::Error,
    },
    #[error("gateway worker failed")]
    Worker(#[from] tokio::task::JoinError),
}

/// Gateway result retaining original sources.
pub type Result<T> = std::result::Result<T, Error>;
