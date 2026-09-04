use crab_storage::StorageError;
use std::sync::{Arc, OnceLock};
use xet_client::ClientError;
use xet_data::file_reconstruction::FileReconstructionError;

pub type Result<T> = std::result::Result<T, ReadError>;

/// Errors from read-side reconstruction and hydration.
#[derive(thiserror::Error, Debug)]
pub enum ReadError {
    #[error("pointer parse failed")]
    Pointer(#[from] crab_types::pointer::PointerParseError),

    #[error("cache error")]
    Cache(#[from] crab_cache::CacheError),

    #[error("cache store error")]
    CacheStore(#[from] crab_cache_store::CacheStoreError),

    #[error("storage error")]
    Storage(#[from] StorageError),

    #[error("metadata error")]
    Metadata(#[from] crab_metadata::error::MetadataError),

    #[error("remote Git error: {0}")]
    RemoteGit(#[from] crab_remote_git::Error),

    #[error("xet data-plane error")]
    Xet(#[from] crab_xet::error::XetError),

    #[error("xet runtime error: {0}")]
    Runtime(#[from] xet_runtime::RuntimeError),

    #[error("file reconstruction failed for {file_hash}: {source}")]
    Reconstruction {
        file_hash: String,
        #[source]
        source: ReconstructionError,
    },

    #[error("I/O error")]
    Io(#[from] std::io::Error),

    #[error("configuration error in {origin}: {key}")]
    Configuration { key: String, origin: String },

    #[error("not found: {path}")]
    NotFound { path: String },

    #[error("corrupt object at {path}: {reason}")]
    CorruptObject { path: String, reason: String },

    #[error("hash mismatch: requested {requested}, actual {actual}")]
    HashMismatch { requested: String, actual: String },

    #[error("incomplete shard reconstruction for {file_hash}")]
    IncompleteShardReconstruction {
        file_hash: String,
        uncovered_chunks: u64,
        example_chunk_hash: String,
        example_chunk_index: u32,
    },

    #[error("read operation cancelled")]
    Cancelled,

    #[error("xorb availability preparation failed")]
    Availability {
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },

    #[error("requested object is outside the visible generation")]
    UnauthorizedObject,

    #[error("{0}")]
    Internal(String),
}

/// Reconstruction failure with the pinned Xet dependency's nested sources exposed.
#[derive(Debug)]
pub struct ReconstructionError {
    upstream: FileReconstructionError,
    read: Option<Arc<ClientError>>,
    writer: Option<Arc<std::io::Error>>,
}

impl From<FileReconstructionError> for ReconstructionError {
    fn from(upstream: FileReconstructionError) -> Self {
        Self {
            upstream,
            read: None,
            writer: None,
        }
    }
}

// Only terminal adapter failures enter this operation-local state. Advisory
// hint/cache failures that recover must never become the operation's cause.
#[derive(Default)]
pub(crate) struct OperationFailures {
    read: OnceLock<Arc<ClientError>>,
    writer: OnceLock<Arc<std::io::Error>>,
}

impl OperationFailures {
    pub(crate) fn read_error(&self, error: ClientError) -> ClientError {
        let error = Arc::new(error);
        let _ = self.read.set(Arc::clone(&error));
        ClientError::internal(SharedClientFailure(error))
    }

    pub(crate) fn writer_error(&self, error: std::io::Error) -> std::io::Error {
        // Interrupted is a retry request in Write, not a terminal failure.
        if error.kind() == std::io::ErrorKind::Interrupted {
            return error;
        }
        let error = Arc::new(error);
        let _ = self.writer.set(Arc::clone(&error));
        std::io::Error::new(error.kind(), error)
    }

    pub(crate) fn finish(&self, upstream: FileReconstructionError) -> ReconstructionError {
        ReconstructionError {
            upstream,
            read: self.read.get().cloned(),
            writer: self.writer.get().cloned(),
        }
    }
}

#[derive(Debug)]
struct SharedClientFailure(Arc<ClientError>);

impl std::fmt::Display for SharedClientFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, formatter)
    }
}

impl std::error::Error for SharedClientFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(client_source(&self.0))
    }
}

fn client_source(error: &ClientError) -> &(dyn std::error::Error + 'static) {
    match error {
        ClientError::InternalError(source) => source.as_ref(),
        other => other,
    }
}

impl ReconstructionError {
    pub(crate) fn has_writer_error(&self) -> bool {
        self.writer.is_some() || matches!(self.upstream, FileReconstructionError::IoError(_))
    }
    pub(crate) fn is_cancelled(&self) -> bool {
        use std::error::Error;

        let mut current = self.source();
        while let Some(error) = current {
            if matches!(
                error.downcast_ref::<ReadError>(),
                Some(ReadError::Cancelled)
            ) || matches!(
                error.downcast_ref::<StorageError>(),
                Some(StorageError::Cancelled)
            ) {
                return true;
            }
            current = error.source();
        }
        false
    }
}

impl std::fmt::Display for ReconstructionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(writer) = &self.writer {
            write!(formatter, "{writer}; Xet: {}", self.upstream)
        } else if matches!(self.upstream, FileReconstructionError::IoError(_)) {
            std::fmt::Display::fmt(&self.upstream, formatter)
        } else if let Some(read) = &self.read {
            write!(formatter, "{read}; Xet: {}", self.upstream)
        } else {
            std::fmt::Display::fmt(&self.upstream, formatter)
        }
    }
}

impl std::error::Error for ReconstructionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        // A completed destination failure is never reclassified as a retryable
        // read error. Retain the upstream and read failures for diagnostics too.
        if let Some(writer) = &self.writer {
            return Some(writer.as_ref());
        }
        if let FileReconstructionError::IoError(source) = &self.upstream {
            return Some(source.as_ref());
        }
        if let Some(read) = &self.read {
            return Some(client_source(read));
        }
        // Xet 1.6 retains these errors but omits their source annotations.
        // Borrow through its Arc: cloning ClientError would stringify the cause.
        match &self.upstream {
            FileReconstructionError::ClientError(error) => Some(client_source(error)),
            FileReconstructionError::IoError(source) => Some(source.as_ref()),
            FileReconstructionError::TaskRuntimeError(source) => Some(source.as_ref()),
            FileReconstructionError::TaskJoinError(source) => Some(source.as_ref()),
            FileReconstructionError::RuntimeError(source) => Some(source.as_ref()),
            other => Some(other),
        }
    }
}

impl ReadError {
    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self::Internal(message.into())
    }

    pub fn availability(error: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Availability {
            source: Box::new(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    #[test]
    fn observed_read_retains_typed_cause_when_xet_reports_a_secondary_failure() {
        let failures = OperationFailures::default();
        let handed_to_xet = failures.read_error(ClientError::internal(ReadError::availability(
            StorageError::AuthExpired {
                path: "object".into(),
            },
        )));
        let returned = failures.finish(FileReconstructionError::InternalWriterError(
            "channel closed".into(),
        ));
        assert!(
            std::iter::successors(returned.source(), |error| (*error).source()).any(
                |error| matches!(
                    error.downcast_ref::<StorageError>(),
                    Some(StorageError::AuthExpired { .. })
                )
            )
        );
        assert!(matches!(
            returned.upstream,
            FileReconstructionError::InternalWriterError(_)
        ));
        assert!(matches!(handed_to_xet, ClientError::InternalError(_)));
    }

    #[test]
    fn writer_failure_precedes_read_cancellation_in_either_observation_order() {
        for writer_first in [false, true] {
            let failures = OperationFailures::default();
            let record_writer = || {
                let _ = failures
                    .writer_error(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
            };
            if writer_first {
                record_writer();
            }
            let _ = failures.read_error(ClientError::internal(ReadError::Cancelled));
            if !writer_first {
                record_writer();
            }
            let returned = failures.finish(FileReconstructionError::InternalWriterError(
                "channel closed".into(),
            ));
            assert_eq!(
                returned
                    .source()
                    .unwrap()
                    .downcast_ref::<std::io::Error>()
                    .unwrap()
                    .kind(),
                std::io::ErrorKind::PermissionDenied
            );
            assert!(!returned.is_cancelled());
            assert!(returned.read.is_some());
        }
    }

    #[test]
    fn interrupted_write_is_not_a_terminal_failure() {
        let failures = OperationFailures::default();
        let _ = failures.writer_error(std::io::Error::from(std::io::ErrorKind::Interrupted));
        let _ = failures.read_error(ClientError::internal(ReadError::Cancelled));
        let returned = failures.finish(FileReconstructionError::InternalWriterError(
            "channel closed".into(),
        ));
        assert!(returned.is_cancelled());
        assert!(!returned.has_writer_error());
    }

    #[test]
    fn failure_snapshots_do_not_change_after_late_callbacks() {
        let failures = OperationFailures::default();
        let returned = failures.finish(FileReconstructionError::ConfigurationError(
            "invalid configuration".into(),
        ));
        let _ = failures.read_error(ClientError::internal(ReadError::Cancelled));
        assert!(!returned.is_cancelled());
        assert!(returned.read.is_none());
    }

    #[test]
    fn reconstruction_recognizes_source_free_storage_cancellation() {
        use std::error::Error;

        let storage = crab_cache_store::CacheStoreError::from(StorageError::Cancelled);
        let client = xet_client::ClientError::internal(ReadError::from(storage));
        let reconstruction = ReconstructionError::from(FileReconstructionError::from(client));

        assert!(reconstruction.is_cancelled());
        assert!(
            std::iter::successors(reconstruction.source(), |source| (*source).source()).any(
                |source| matches!(
                    source.downcast_ref::<StorageError>(),
                    Some(StorageError::Cancelled)
                )
            )
        );
    }

    #[test]
    fn remote_git_error_retains_its_safe_diagnostic() {
        let error = ReadError::from(crab_remote_git::Error::LimitExceeded {
            limit: "decoded object bytes",
            actual: 65,
            maximum: 64,
        });

        assert_eq!(
            error.to_string(),
            "remote Git error: remote Git read exceeded decoded object bytes: requested 65, maximum 64"
        );
    }
}
