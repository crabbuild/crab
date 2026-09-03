use crab_storage::StorageError;

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
pub struct ReconstructionError(pub(crate) xet_data::file_reconstruction::FileReconstructionError);

impl ReconstructionError {
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
        std::fmt::Display::fmt(&self.0, formatter)
    }
}

impl std::error::Error for ReconstructionError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        use xet_data::file_reconstruction::FileReconstructionError;

        // Xet 1.6 retains these errors but omits their source annotations.
        // Borrow through its Arc: cloning ClientError would stringify the cause.
        match &self.0 {
            FileReconstructionError::ClientError(error) => match error.as_ref() {
                xet_client::ClientError::InternalError(source) => Some(source.as_ref()),
                other => Some(other),
            },
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

    #[test]
    fn reconstruction_recognizes_source_free_storage_cancellation() {
        use std::error::Error;

        let storage = crab_cache_store::CacheStoreError::from(StorageError::Cancelled);
        let client = xet_client::ClientError::internal(ReadError::from(storage));
        let reconstruction = ReconstructionError(client.into());

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
