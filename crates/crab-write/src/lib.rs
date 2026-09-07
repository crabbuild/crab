//! Shared publication mechanics; authentication and product policy stay with callers.
pub mod catalog;
pub mod generation;
pub mod initialize;
pub mod journal;
mod namespace;
pub use namespace::with_ref_namespace;

/// Failure while preparing or publishing canonical Git metadata.
#[derive(Debug, thiserror::Error)]
pub enum WriteError {
    /// The clock cannot produce a valid persisted timestamp.
    #[error("invalid timestamp: {0}")]
    Timestamp(#[from] crab_types::time::TimestampError),
    #[error(transparent)]
    Namespace(#[from] crab_git::refname::RefNamespaceError),
    #[error("generation {generation} has no verified Git visibility proof")]
    VisibilityUnavailable { generation: u64 },
    #[error("ref {ref_name} no longer matches its expected old value at {path}")]
    RefChanged { ref_name: String, path: String },
    #[error("publication coordination failed")]
    Coordination(#[from] crab_coordination::CoordinationError),
    #[error("publication storage operation failed")]
    Storage(#[from] crab_storage::StorageError),
    #[error("publication metadata operation failed")]
    Metadata(#[from] crab_metadata::error::MetadataError),
    #[error("remote Git publication read failed")]
    RemoteGit(#[from] crab_remote_git::Error),
    #[error("Git pack evidence is invalid")]
    Git(#[from] crab_git::pack::PackError),
    #[error("publication file I/O failed")]
    Io(#[from] std::io::Error),
    #[error("publication worker failed")]
    Worker(#[from] tokio::task::JoinError),
    #[error("invalid manifest {field} hash")]
    ManifestHash {
        field: &'static str,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("invalid pack identity")]
    PackIdentity {
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("corrupt object at {path}: {reason}")]
    CorruptObject { path: String, reason: String },
    #[error("{0}")]
    Internal(String),
    #[error("publication cancelled")]
    Cancelled,
}

/// Shared publication result preserving dependency errors.
pub type Result<T> = std::result::Result<T, WriteError>;

// Callers await cleanup before choosing an outcome. A cleanup failure surfaces
// only when the operation succeeded; otherwise retain and return its primary error.
// Namespace publication separately preserves known commits despite lease errors.
pub(crate) fn finish_after_cleanup<T, C, E>(
    operation: Result<T>,
    cleanup: std::result::Result<C, E>,
    message: &'static str,
) -> Result<T>
where
    E: std::fmt::Display + Into<WriteError>,
{
    match (operation, cleanup) {
        (Ok(value), Ok(_)) => Ok(value),
        (Err(error), Ok(_)) => Err(error),
        (Ok(_), Err(error)) => Err(error.into()),
        (Err(error), Err(cleanup_error)) => {
            tracing::warn!(error = %cleanup_error, "{message}");
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_failure_retains_the_primary_io_cause() {
        for (operation, expected) in [
            (Ok(()), "cleanup"),
            (
                Err(WriteError::Io(std::io::Error::other("operation"))),
                "operation",
            ),
        ] {
            let cleanup: std::io::Result<()> = Err(std::io::Error::other("cleanup"));
            let error = finish_after_cleanup(operation, cleanup, "cleanup also failed")
                .expect_err("operation or cleanup failed");
            let WriteError::Io(source) = error else {
                panic!("original I/O cause must survive");
            };
            assert_eq!(source.to_string(), expected);
        }
    }
}
