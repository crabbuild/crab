use std::time::{SystemTime, UNIX_EPOCH};

mod address;
pub(crate) use address::Address;
pub(crate) mod application;
mod client;
pub(crate) mod directory;
mod initializer;
pub(crate) mod organization;
mod records;
pub(crate) mod runtime;
pub(crate) use initializer::initialize_repository;
pub(crate) mod repository;

pub(crate) use application::MutationIdentity;
pub(crate) use client::{CellClient, Committed, Observed, RepositoryCell};

/// Cell execution and transport failures with original error sources.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid Cell request: {0}")]
    Invalid(&'static str),
    #[error("Cell operation failed")]
    Runtime {
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("Cell JSON encoding failed")]
    Json(#[from] serde_json::Error),
    #[error("Cell request ID was reused with another payload")]
    RequestConflict,
    #[error("Cell request has expired")]
    Expired,
    #[error("Cell is unavailable")]
    Unavailable,
    #[error("Cell requires explicit initialization")]
    NotInitialized,
    #[error("Cell schema requires explicit migration")]
    SchemaUnavailable,
    #[error("discussion index requires explicit preparation")]
    IndexUnavailable,
    #[error("Cell command may have committed; retry with the same submission ID")]
    Pending {
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("Current Cell membership does not permit this operation")]
    AccessDenied,
    #[error("Cell has not reached the requested revision")]
    StaleRead,
    #[error("discussion cursor no longer matches the index snapshot or query")]
    StaleSearch,
}

impl Error {
    pub(crate) fn sdk(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> Self {
        Self::Runtime {
            source: error.into(),
        }
    }
}

pub(crate) type Result<T> = std::result::Result<T, Error>;

pub(crate) fn unix_now_ms() -> Result<i64> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(Error::sdk)?;
    i64::try_from(now.as_millis())
        .map_err(|_| Error::Invalid("system clock exceeds timestamp range"))
}

#[cfg(test)]
mod native_tests;
