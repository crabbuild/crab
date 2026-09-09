use crate::{Error, ErrorKind};

pub(crate) fn remote_error(source: crab_remote_git::Error) -> Error {
    match source {
        crab_remote_git::Error::Consumer { source } => match source.downcast::<Error>() {
            Ok(error) => *error,
            Err(source) => Error::with_source(
                ErrorKind::Io,
                "remote read consumer failed",
                crab_remote_git::Error::Consumer { source },
            ),
        },
        crab_remote_git::Error::CloseAfterFailure { operation, close } => remote_error(*operation)
            .with_cleanup(remote_error(crab_remote_git::Error::Metadata(close))),
        source => Error::with_source(kind(&source), "remote repository operation failed", source),
    }
}

pub(crate) fn consumer_error(source: Error) -> crab_remote_git::Error {
    crab_remote_git::Error::Consumer {
        source: Box::new(source),
    }
}

pub(crate) fn kind(error: &crab_remote_git::Error) -> ErrorKind {
    use crab_remote_git::{Error as E, RevisionError as R};
    match error {
        E::InvalidRepositoryIdentity { .. }
        | E::InvalidPath { .. }
        | E::InvalidCursor { .. }
        | E::InvalidLimit { .. }
        | E::Revision {
            reason:
                R::InvalidReference
                | R::AbbreviatedObjectId
                | R::MalformedObjectId
                | R::AmbiguousReference,
        } => ErrorKind::InvalidInput,
        E::PathNotFound
        | E::ObjectNotFound { .. }
        | E::EmptyRepository
        | E::SnapshotUnavailable
        | E::Revision {
            reason: R::NotFound | R::NotReachable,
        } => ErrorKind::NotFound,
        E::PathComponentNotTree { .. }
        | E::EntryNotBlob { .. }
        | E::EntryNotSymlink { .. }
        | E::EntryNotSubmodule { .. }
        | E::BlameUnsupported { .. }
        | E::UnsupportedObjectFormat
        | E::Revision {
            reason: R::NotCommit,
        } => ErrorKind::UnsupportedCapability,
        E::RepositoryIndexing { .. } => ErrorKind::Indexing,
        E::LimitExceeded { .. }
        | E::Allocation { .. }
        | E::Revision {
            reason: R::TagDepth,
        } => ErrorKind::LimitExceeded,
        E::Cancelled => ErrorKind::Cancelled,
        E::AuthorizationDenied => ErrorKind::Authorization,
        E::Timeout { .. } => ErrorKind::Timeout,
        E::SharedRead { source } => kind(source),
        E::CloseAfterFailure { operation, .. } => kind(operation),
        E::Storage(source) => storage_kind(source),
        E::Metadata(source) | E::Manifest { source } | E::Inventory { source } => {
            metadata_kind(source)
        }
        E::DecodeTask { .. } | E::InternalInvariant { .. } => ErrorKind::Io,
        E::GeneratedPackLease { .. } => ErrorKind::Transport,
        E::InvalidTreeMode { .. }
        | E::RepositoryState { .. }
        | E::ObjectKind { .. }
        | E::CommitParse { .. }
        | E::TreeParse { .. }
        | E::TagParse { .. }
        | E::Corrupt { .. }
        | E::PackedEntryCrcMismatch { .. }
        | E::ObjectIdMismatch { .. }
        | E::CacheCorrupt { .. }
        | E::PackEntry { .. }
        | E::Inflate { .. }
        | E::InvalidInflatedEntry { .. }
        | E::InvalidDelta { .. }
        | E::DeltaBaseNotFound { .. }
        | E::PackIndex { .. }
        | E::ResponsePackConsolidation { .. }
        | E::Revision {
            reason: R::TagCycle,
        } => ErrorKind::Corruption,
        _ => ErrorKind::Io,
    }
}

pub(crate) fn metadata_kind(error: &crab_metadata::error::MetadataError) -> ErrorKind {
    use crab_metadata::error::MetadataError as E;
    if let Some(kind) = wrapped_storage_kind(error) {
        return kind;
    }
    match error {
        #[cfg(any(feature = "content", feature = "write"))]
        E::FileLookupLimit { .. } => ErrorKind::LimitExceeded,
        E::Storage { source } => storage_kind(source),
        E::CorruptObject { .. } => ErrorKind::Corruption,
        E::Io { source } if source.kind() == std::io::ErrorKind::InvalidData => {
            ErrorKind::Corruption
        }
        E::RefJournalCancelled => ErrorKind::Cancelled,
        E::ManifestCasConflict { .. } => ErrorKind::Conflict,
        E::ObjectStore { source } => object_store_kind(source),
        _ => ErrorKind::Io,
    }
}

#[cfg(any(feature = "content", feature = "write"))]
pub(crate) fn lfs_kind(source: &crab_lfs::LfsError) -> ErrorKind {
    use crab_lfs::LfsError as E;
    match source {
        E::ObjectCorrupt { .. } => ErrorKind::Corruption,
        E::ObjectMissing { .. } => ErrorKind::NotFound,
        E::Storage { source } => storage_kind(source),
        E::Io { source } => match source.kind() {
            std::io::ErrorKind::Unsupported => ErrorKind::UnsupportedCapability,
            std::io::ErrorKind::InvalidInput => ErrorKind::InvalidInput,
            _ => ErrorKind::Io,
        },
    }
}

// Metadata and cache wrappers preserve typed storage sources, including
// terminal admission/framing failures carried by a NotSupported envelope.
fn wrapped_storage_kind(error: &(dyn std::error::Error + 'static)) -> Option<ErrorKind> {
    let mut current = Some(error);
    while let Some(error) = current {
        if let Some(error) = error.downcast_ref::<crab_storage::StorageError>() {
            return Some(storage_kind(error));
        }
        current = error
            .downcast_ref::<std::io::Error>()
            .and_then(|error| {
                error
                    .get_ref()
                    .map(|source| source as &(dyn std::error::Error + 'static))
            })
            .or_else(|| error.source());
    }
    None
}

pub(crate) fn object_store_kind(error: &object_store::Error) -> ErrorKind {
    if let Some(kind) = wrapped_storage_kind(error) {
        return kind;
    }
    match error {
        object_store::Error::NotFound { .. } => ErrorKind::NotFound,
        object_store::Error::PermissionDenied { .. } => ErrorKind::Authorization,
        object_store::Error::Unauthenticated { .. } => ErrorKind::Authentication,
        object_store::Error::NotSupported { .. } | object_store::Error::NotImplemented { .. } => {
            ErrorKind::UnsupportedCapability
        }
        object_store::Error::Precondition { .. } | object_store::Error::AlreadyExists { .. } => {
            ErrorKind::Conflict
        }
        _ => ErrorKind::Transport,
    }
}

pub(crate) fn storage_kind(error: &crab_storage::StorageError) -> ErrorKind {
    use crab_storage::StorageError as E;
    match error {
        E::AuthFailed { .. } | E::AuthExpired { .. } | E::NoCredentials => {
            ErrorKind::Authentication
        }
        E::Forbidden { .. } => ErrorKind::Authorization,
        E::StateConflict { .. } => ErrorKind::Conflict,
        E::NotFound { .. } => ErrorKind::NotFound,
        E::CorruptObject { .. } => ErrorKind::Corruption,
        E::Cancelled => ErrorKind::Cancelled,
        E::NotSupported { .. } | E::UnsupportedProvider { .. } => ErrorKind::UnsupportedCapability,
        E::InvalidHash { .. }
        | E::InvalidStaticEnvTarget { .. }
        | E::StaticEnvProviderMismatch { .. }
        | E::ProviderConfig { .. }
        | E::InvalidObjectStoreUrl { .. }
        | E::UrlStoreConfig { .. } => ErrorKind::InvalidInput,
        E::Io { .. } | E::Internal(_) | E::MultipartJournal { .. } => ErrorKind::Io,
        E::ObjectStore { source } | E::NetworkTransient { source } => object_store_kind(source),
        E::Throttled { .. } => ErrorKind::Transport,
        E::ReadRejected { source } => {
            let mut current: Option<&(dyn std::error::Error + 'static)> = Some(source.as_ref());
            while let Some(error) = current {
                if let Some(error) = error.downcast_ref::<crab_remote_git::Error>() {
                    return kind(error);
                }
                if let Some(error) = error.downcast_ref::<crab_storage::StorageError>() {
                    return storage_kind(error);
                }
                current = error
                    .downcast_ref::<std::io::Error>()
                    .and_then(|error| {
                        error
                            .get_ref()
                            .map(|source| source as &(dyn std::error::Error + 'static))
                    })
                    .or_else(|| error.source());
            }
            ErrorKind::Io
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn admission_failure_keeps_its_kind_through_cache_and_io_wrappers() {
        let sources: [(Box<dyn std::error::Error + Send + Sync>, ErrorKind); 3] = [
            (
                Box::new(crab_remote_git::Error::Cancelled),
                ErrorKind::Cancelled,
            ),
            (
                Box::new(crab_remote_git::Error::LimitExceeded {
                    limit: "fetched bytes",
                    actual: 2,
                    maximum: 1,
                }),
                ErrorKind::LimitExceeded,
            ),
            (
                Box::new(crab_storage::StorageError::Cancelled),
                ErrorKind::Cancelled,
            ),
        ];
        for (source, expected) in sources {
            let source = crab_storage::StorageError::ReadRejected { source };
            let wrapped = object_store::Error::Generic {
                store: "cache",
                source: Box::new(std::io::Error::other(source)),
            };
            assert_eq!(object_store_kind(&wrapped), expected);
            let mapped = crab_storage::map_object_store_error(wrapped, "object");
            assert_eq!(storage_kind(&mapped), expected);
        }
    }

    #[test]
    fn framing_corruption_keeps_its_kind_through_metadata_and_io_wrappers() {
        let terminal = object_store::Error::NotSupported {
            source: Box::new(crab_storage::StorageError::CorruptObject {
                path: "object".into(),
                reason: "invalid response length".into(),
            }),
        };
        assert_eq!(object_store_kind(&terminal), ErrorKind::Corruption);
        let metadata = crab_metadata::error::MetadataError::Io {
            source: std::io::Error::other(terminal),
        };
        assert_eq!(metadata_kind(&metadata), ErrorKind::Corruption);
    }

    #[test]
    fn operation_and_close_failures_remain_separately_observable() {
        let error = remote_error(crab_remote_git::Error::CloseAfterFailure {
            operation: Box::new(crab_remote_git::Error::Cancelled),
            close: crab_metadata::error::MetadataError::Io {
                source: std::io::Error::other("injected close failure"),
            },
        });
        assert_eq!(error.kind(), ErrorKind::Cancelled);
        let cleanup = error.cleanup_error().unwrap();
        assert_eq!(cleanup.kind(), ErrorKind::Io);
        assert!(std::error::Error::source(cleanup).is_some());
        assert!(std::error::Error::source(&error).is_some());
        assert!(!format!("{error:?}").contains("injected"));
    }
    #[test]
    fn consumer_failure_keeps_its_category_and_secondary_close_source() {
        let primary = Error::with_source(
            ErrorKind::InvalidInput,
            "invalid range",
            std::io::Error::other("caller diagnostic"),
        );
        let error = remote_error(crab_remote_git::Error::CloseAfterFailure {
            operation: Box::new(consumer_error(primary)),
            close: crab_metadata::error::MetadataError::Io {
                source: std::io::Error::other("close diagnostic"),
            },
        });
        assert_eq!(error.kind(), ErrorKind::InvalidInput);
        assert!(
            std::error::Error::source(&error)
                .unwrap()
                .downcast_ref::<std::io::Error>()
                .is_some()
        );
        assert_eq!(error.cleanup_error().unwrap().kind(), ErrorKind::Io);
    }
}
