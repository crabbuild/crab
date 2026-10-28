use crate::{Error, ErrorKind};

#[derive(Debug, thiserror::Error)]
pub(crate) enum Failure {
    #[error("SDK publication failed")]
    Sdk(#[from] Error),
    #[error("publication admission failed")]
    Publication(#[from] crab_remote::publication::Error),
    #[error("publication metadata failed")]
    Metadata(#[from] crab_metadata::error::MetadataError),
    #[error("publication commit failed")]
    Write(#[from] crab_write::WriteError),
    #[error("publication preparation failed")]
    Prepare(#[from] crab_remote::prepare::Error),
    #[error("publication scratch I/O failed")]
    Io(#[from] std::io::Error),
    #[error("publication worker failed")]
    Worker(#[from] tokio::task::JoinError),
}

impl Failure {
    pub(super) fn rejected(&self) -> bool {
        matches!(
            self,
            Self::Prepare(crab_remote::prepare::Error::Request(_))
                | Self::Write(
                    crab_write::WriteError::RefChanged { .. }
                        | crab_write::WriteError::Namespace(_)
                        | crab_write::WriteError::CapsuleRootChanged { .. }
                        | crab_write::WriteError::CapsuleRefEpochChanged { .. }
                )
                | Self::Prepare(crab_remote::prepare::Error::Write(
                    crab_write::WriteError::RefChanged { .. }
                        | crab_write::WriteError::Namespace(_)
                        | crab_write::WriteError::CapsuleRootChanged { .. }
                        | crab_write::WriteError::CapsuleRefEpochChanged { .. }
                ))
        )
    }
}

impl From<Failure> for Error {
    fn from(source: Failure) -> Self {
        let source = match source {
            Failure::Sdk(error) => return error,
            Failure::Prepare(crab_remote::prepare::Error::CapsuleRead(error)) => {
                return crate::remote_error::read_error(error);
            }
            Failure::Prepare(crab_remote::prepare::Error::Remote(error)) => {
                return crate::remote_error::remote_error(error);
            }
            Failure::Prepare(crab_remote::prepare::Error::Close { operation, close }) => {
                return Self::from(Failure::Prepare(*operation))
                    .with_cleanup(crate::remote_error::remote_error(close));
            }
            source => source,
        };
        let kind = match &source {
            Failure::Metadata(source) => crate::remote_error::metadata_kind(source),
            Failure::Publication(crab_remote::publication::Error::Cancelled) => {
                ErrorKind::Cancelled
            }
            Failure::Publication(crab_remote::publication::Error::Coordination(source)) => {
                coordination_kind(source)
            }
            Failure::Write(source)
            | Failure::Prepare(crab_remote::prepare::Error::Write(source)) => write_kind(source),
            Failure::Prepare(crab_remote::prepare::Error::Cancelled) => ErrorKind::Cancelled,
            Failure::Prepare(crab_remote::prepare::Error::Metadata(source)) => {
                crate::remote_error::metadata_kind(source)
            }
            Failure::Prepare(crab_remote::prepare::Error::Storage(source)) => {
                crate::remote_error::storage_kind(source)
            }
            Failure::Prepare(crab_remote::prepare::Error::Graph(
                crab_git::receive_plan::ReceivePlanError::Stale { .. }
                | crab_git::receive_plan::ReceivePlanError::NonFastForward { .. }
                | crab_git::receive_plan::ReceivePlanError::Namespace(_),
            )) => ErrorKind::Conflict,
            Failure::Prepare(crab_remote::prepare::Error::Graph(
                crab_git::receive_plan::ReceivePlanError::Cancelled,
            )) => ErrorKind::Cancelled,
            Failure::Prepare(crab_remote::prepare::Error::Graph(
                crab_git::receive_plan::ReceivePlanError::Limit(_),
            )) => ErrorKind::LimitExceeded,
            Failure::Prepare(crab_remote::prepare::Error::Graph(
                crab_git::receive_plan::ReceivePlanError::Source { source, .. },
            )) => source
                .downcast_ref::<crab_remote_git::Error>()
                .map_or(ErrorKind::Io, crate::remote_error::kind),
            Failure::Prepare(crab_remote::prepare::Error::Graph(
                crab_git::receive_plan::ReceivePlanError::Missing { .. },
            )) => ErrorKind::NotFound,
            Failure::Prepare(crab_remote::prepare::Error::Graph(
                crab_git::receive_plan::ReceivePlanError::Parse { .. }
                | crab_git::receive_plan::ReceivePlanError::Invalid { .. }
                | crab_git::receive_plan::ReceivePlanError::Path { .. }
                | crab_git::receive_plan::ReceivePlanError::TagName { .. },
            )) => ErrorKind::Corruption,
            Failure::Prepare(crab_remote::prepare::Error::Graph(_)) => ErrorKind::InvalidInput,
            Failure::Prepare(crab_remote::prepare::Error::Dependency(source)) => {
                dependency_kind(source)
            }
            Failure::Prepare(crab_remote::prepare::Error::Request(_)) => ErrorKind::Conflict,
            Failure::Prepare(
                crab_remote::prepare::Error::Content(_)
                | crab_remote::prepare::Error::ContentFormat(_),
            ) => ErrorKind::Corruption,
            _ => ErrorKind::Io,
        };
        Self::with_source(kind, "remote mutation failed", source)
    }
}

fn write_kind(source: &crab_write::WriteError) -> ErrorKind {
    use crab_write::WriteError as W;
    match source {
        W::CorruptObject { .. } => ErrorKind::Corruption,
        W::Cancelled => ErrorKind::Cancelled,
        W::RefChanged { .. }
        | W::Namespace(_)
        | W::CapsuleRootChanged { .. }
        | W::CapsuleRefEpochChanged { .. } => ErrorKind::Conflict,
        W::Metadata(source) => crate::remote_error::metadata_kind(source),
        W::Storage(source) => crate::remote_error::storage_kind(source),
        W::VisibilityUnavailable { .. } => ErrorKind::Indexing,
        W::Coordination(source) => coordination_kind(source),
        W::RemoteGit(source) => crate::remote_error::kind(source),
        _ => ErrorKind::Io,
    }
}

fn dependency_kind(source: &crab_read::dependency_proof::DependencyProofError) -> ErrorKind {
    use crab_read::{
        dependency_proof::DependencyProofError as D, pointer_proof::PointerProofError as P,
    };
    match source {
        D::Cancelled => ErrorKind::Cancelled,
        D::Deadline => ErrorKind::Timeout,
        D::Limit(_) => ErrorKind::LimitExceeded,
        D::Invalid { .. } => ErrorKind::InvalidInput,
        D::Metadata(source) => crate::remote_error::metadata_kind(source),
        D::Lfs { source, .. } => crate::remote_error::lfs_kind(source),
        D::Crab { source, .. } => match source {
            P::Cancelled => ErrorKind::Cancelled,
            P::Deadline => ErrorKind::Timeout,
            P::Limit(_) => ErrorKind::LimitExceeded,
            P::Integrity(_) | P::Xet(_) => ErrorKind::Corruption,
            P::Storage(source) => crate::remote_error::storage_kind(source),
            P::Admission(_) | P::Worker(_) => ErrorKind::Io,
        },
    }
}

fn coordination_kind(source: &crab_coordination::CoordinationError) -> ErrorKind {
    use crab_coordination::CoordinationError as C;
    match source {
        C::ObjectStore { source, .. } => crate::remote_error::object_store_kind(source),
        C::PushLockHeld { .. }
        | C::GcFenceHeld { .. }
        | C::CasConflict { .. }
        | C::NonFastForward { .. } => ErrorKind::Conflict,
        C::RetryDeadline { .. } => ErrorKind::Timeout,
        C::NotFound { .. } => ErrorKind::NotFound,
        C::Configuration { .. } => ErrorKind::InvalidInput,
        C::MalformedPushLock { .. }
        | C::GcFenceMalformed { .. }
        | C::ReadRetentionMalformed { .. } => ErrorKind::Corruption,
        C::GcFenceLost { .. } | C::ReadRetentionLost { .. } => ErrorKind::Cancelled,
        C::ReadRetentionFailed(source) => coordination_kind(source),
        C::Serialize { .. } => ErrorKind::Io,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crab_coordination::CoordinationError as C;
    use std::sync::Arc;

    #[test]
    fn retention_failures_keep_their_category_across_mutation_boundaries() {
        let boundaries: [fn(C) -> Failure; 3] = [
            |source| Failure::Publication(source.into()),
            |source| Failure::Write(source.into()),
            |source| Failure::Prepare(crab_remote::prepare::Error::Write(source.into())),
        ];
        for boundary in boundaries {
            let cases = [
                (
                    C::ReadRetentionLost {
                        path: "claim".into(),
                    },
                    ErrorKind::Cancelled,
                ),
                (
                    C::ReadRetentionMalformed {
                        path: "claim".into(),
                        reason: "invalid claim".into(),
                    },
                    ErrorKind::Corruption,
                ),
                (
                    C::ObjectStore {
                        path: "claim".into(),
                        source: object_store::Error::PermissionDenied {
                            path: "claim".into(),
                            source: Box::new(std::io::Error::from(
                                std::io::ErrorKind::PermissionDenied,
                            )),
                        },
                    },
                    ErrorKind::Authorization,
                ),
                (
                    C::RetryDeadline {
                        path: "claim".into(),
                        source: Box::new(C::ReadRetentionLost {
                            path: "claim".into(),
                        }),
                    },
                    ErrorKind::Timeout,
                ),
            ];
            for (source, expected) in cases {
                assert_eq!(
                    Error::from(boundary(C::ReadRetentionFailed(Arc::new(source)))).kind(),
                    expected
                );
            }
            assert_eq!(
                Error::from(boundary(C::ReadRetentionLost {
                    path: "claim".into()
                }))
                .kind(),
                ErrorKind::Cancelled
            );
            assert_eq!(
                Error::from(boundary(C::ReadRetentionMalformed {
                    path: "claim".into(),
                    reason: "invalid claim".into()
                }))
                .kind(),
                ErrorKind::Corruption
            );
        }
    }

    #[test]
    fn retention_failure_preserves_the_shared_diagnostic_source() {
        let original = Arc::new(C::ReadRetentionLost {
            path: "claim".into(),
        });
        let error = Error::from(Failure::Write(
            C::ReadRetentionFailed(Arc::clone(&original)).into(),
        ));
        let mut current = std::error::Error::source(&error);
        while let Some(source) = current {
            if let Some(C::ReadRetentionFailed(shared)) = source.downcast_ref::<C>() {
                assert!(Arc::ptr_eq(shared, &original));
                return;
            }
            current = source.source();
        }
        panic!("retention source missing from SDK error chain");
    }
}
