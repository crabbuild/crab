use std::io;

pub type Result<T> = std::result::Result<T, AuthServerError>;

#[derive(thiserror::Error, Debug)]
pub enum AuthServerError {
    #[error("{0}")]
    Read(#[source] Box<crab_read::ReadError>),

    #[error("authentication failed for {path}")]
    AuthFailed { path: String },

    #[error("forbidden: {path}")]
    Forbidden { path: String },

    #[error("not found: {path}")]
    NotFound { path: String },

    #[error("configuration error in {origin}: {key}")]
    Configuration { key: String, origin: String },

    #[error("CAS conflict at {path}")]
    CasConflict {
        path: String,
        expected_etag: Option<String>,
    },

    #[error("non-fast-forward update for {ref_name}: have {have}, want {want}")]
    NonFastForward {
        ref_name: String,
        have: String,
        want: String,
    },

    #[error("corrupt object {path}: {reason}")]
    CorruptObject { path: String, reason: String },

    #[error("origin object at {path} failed integrity verification: {source}")]
    OriginIntegrity {
        path: String,
        #[source]
        source: crab_cache::CacheError,
    },

    #[error("hash mismatch: requested {requested}, actual {actual}")]
    HashMismatch { requested: String, actual: String },

    #[error("Git visibility traversal failed")]
    GitVisibilityWalk {
        #[source]
        source: crab_git::walk::WalkError,
    },

    #[error("Git visibility traversal task failed")]
    GitVisibilityJoin {
        #[source]
        source: tokio::task::JoinError,
    },

    #[error("view content repacking task failed")]
    ViewRepackJoin {
        #[source]
        source: tokio::task::JoinError,
    },

    #[error("incomplete shard reconstruction for {file_hash}")]
    IncompleteShardReconstruction {
        file_hash: String,
        path: Option<String>,
        uncovered_chunks: u64,
        example_chunk_hash: String,
        example_chunk_index: u32,
    },

    #[error("I/O error: {0}")]
    Io(#[source] io::Error),

    #[error("{0}")]
    Internal(String),
}

impl AuthServerError {
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::Configuration {
            key: "auth-server".to_owned(),
            origin: message.into(),
        }
    }
}

impl From<crab_auth::error::AuthError> for AuthServerError {
    fn from(error: crab_auth::error::AuthError) -> Self {
        Self::Internal(error.to_string())
    }
}

impl From<crab_coordination::CoordinationError> for AuthServerError {
    fn from(error: crab_coordination::CoordinationError) -> Self {
        match error {
            crab_coordination::CoordinationError::CasConflict {
                path,
                expected_etag,
            } => Self::CasConflict {
                path,
                expected_etag,
            },
            crab_coordination::CoordinationError::NonFastForward {
                ref_name,
                have,
                want,
            } => Self::NonFastForward {
                ref_name,
                have,
                want,
            },
            crab_coordination::CoordinationError::NotFound { path } => Self::NotFound { path },
            crab_coordination::CoordinationError::Configuration { key, origin } => {
                Self::Configuration { key, origin }
            }
            other => Self::Internal(other.to_string()),
        }
    }
}

impl From<crab_git::UrlError> for AuthServerError {
    fn from(error: crab_git::UrlError) -> Self {
        Self::Configuration {
            key: "repo_url".to_owned(),
            origin: error.to_string(),
        }
    }
}

impl From<crab_git::pack::PackError> for AuthServerError {
    fn from(error: crab_git::pack::PackError) -> Self {
        Self::CorruptObject {
            path: "pack".to_owned(),
            reason: error.to_string(),
        }
    }
}

impl From<crab_metadata::error::MetadataError> for AuthServerError {
    fn from(error: crab_metadata::error::MetadataError) -> Self {
        match error {
            crab_metadata::error::MetadataError::Io { source } => Self::Io(source),
            crab_metadata::error::MetadataError::CorruptObject { path, reason } => {
                Self::CorruptObject { path, reason }
            }
            crab_metadata::error::MetadataError::Storage { source } => Self::from(source),
            crab_metadata::error::MetadataError::ManifestCasConflict {
                path,
                expected_etag,
            } => Self::CasConflict {
                path,
                expected_etag,
            },
            other => Self::Internal(other.to_string()),
        }
    }
}

impl From<crab_cache_store::CacheStoreError> for AuthServerError {
    fn from(error: crab_cache_store::CacheStoreError) -> Self {
        match error {
            crab_cache_store::CacheStoreError::Storage(source) => Self::from(source),
            crab_cache_store::CacheStoreError::OriginIntegrity { path, source } => {
                Self::OriginIntegrity { path, source }
            }
            crab_cache_store::CacheStoreError::Cache(source) => Self::Internal(source.to_string()),
        }
    }
}

impl From<crab_staging::StagingError> for AuthServerError {
    fn from(error: crab_staging::StagingError) -> Self {
        match error {
            crab_staging::StagingError::Io(source) => Self::Io(source),
            crab_staging::StagingError::ShardReplayCorrupt { reason } => Self::CorruptObject {
                path: "Xet shard replay".to_owned(),
                reason,
            },
            other => Self::Internal(other.to_string()),
        }
    }
}

impl From<crab_lfs::LfsError> for AuthServerError {
    fn from(error: crab_lfs::LfsError) -> Self {
        match error {
            crab_lfs::LfsError::ObjectMissing { oid } => Self::NotFound {
                path: format!("lfs:{oid}"),
            },
            crab_lfs::LfsError::ObjectCorrupt { oid } => Self::CorruptObject {
                path: format!("lfs:{oid}"),
                reason: "sha256 mismatch".to_owned(),
            },
            crab_lfs::LfsError::Io { source } => Self::Io(source),
            crab_lfs::LfsError::Storage { source } => Self::from(source),
        }
    }
}

impl From<crab_read::ReadError> for AuthServerError {
    fn from(error: crab_read::ReadError) -> Self {
        match error {
            crab_read::ReadError::CacheStore(source) => Self::from(source),
            crab_read::ReadError::Io(source) => Self::Io(source),
            crab_read::ReadError::Storage(source) => Self::from(source),
            crab_read::ReadError::NotFound { path } => Self::NotFound { path },
            crab_read::ReadError::CorruptObject { path, reason } => {
                Self::CorruptObject { path, reason }
            }
            crab_read::ReadError::HashMismatch { requested, actual } => {
                Self::HashMismatch { requested, actual }
            }
            crab_read::ReadError::IncompleteShardReconstruction {
                file_hash,
                uncovered_chunks,
                example_chunk_hash,
                example_chunk_index,
                ..
            } => Self::IncompleteShardReconstruction {
                file_hash,
                path: None,
                uncovered_chunks,
                example_chunk_hash,
                example_chunk_index,
            },
            other => Self::Read(Box::new(other)),
        }
    }
}

impl From<crab_storage::StorageError> for AuthServerError {
    fn from(error: crab_storage::StorageError) -> Self {
        match error {
            crab_storage::StorageError::NotFound { path } => Self::NotFound { path },
            crab_storage::StorageError::AuthFailed { path }
            | crab_storage::StorageError::AuthExpired { path } => Self::AuthFailed { path },
            crab_storage::StorageError::NoCredentials => Self::AuthFailed {
                path: "storage credentials".to_owned(),
            },
            crab_storage::StorageError::Forbidden { path } => Self::Forbidden { path },
            crab_storage::StorageError::StateConflict { path } => Self::CasConflict {
                path,
                expected_etag: None,
            },
            crab_storage::StorageError::CorruptObject { path, reason } => {
                Self::CorruptObject { path, reason }
            }
            crab_storage::StorageError::Io { source } => Self::Io(source),
            other => Self::Internal(other.to_string()),
        }
    }
}

impl From<crab_xet::error::XetError> for AuthServerError {
    fn from(error: crab_xet::error::XetError) -> Self {
        Self::Internal(error.to_string())
    }
}

impl From<crab_types::pointer::PointerParseError> for AuthServerError {
    fn from(error: crab_types::pointer::PointerParseError) -> Self {
        Self::CorruptObject {
            path: "pointer".to_owned(),
            reason: error.to_string(),
        }
    }
}

impl From<io::Error> for AuthServerError {
    fn from(source: io::Error) -> Self {
        Self::Io(source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_availability_failure_retains_nested_source() {
        use std::error::Error;

        let source = io::Error::new(io::ErrorKind::PermissionDenied, "restore denied");
        let error = AuthServerError::from(crab_read::ReadError::availability(source));
        let nested = std::iter::successors(error.source(), |source| (*source).source())
            .find_map(|source| source.downcast_ref::<io::Error>())
            .unwrap();
        assert_eq!(nested.kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn shared_origin_integrity_retains_the_source_error() {
        use std::error::Error as _;

        let error = AuthServerError::from(crab_read::ReadError::from(
            crab_cache_store::CacheStoreError::OriginIntegrity {
                path: "xorbs/bad".into(),
                source: crab_cache::CacheError::CorruptObject {
                    path: "xorb".into(),
                    reason: "invalid footer".into(),
                },
            },
        ));
        assert!(
            matches!(&error, AuthServerError::OriginIntegrity { path, .. } if path == "xorbs/bad")
        );
        assert!(error.source().unwrap().is::<crab_cache::CacheError>());
    }
}
