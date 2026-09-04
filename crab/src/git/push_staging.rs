//! Staging availability for native push admission.

use std::path::PathBuf;
use std::sync::Arc;

use crab_staging::{StagingAreaReadOnly, StagingError};

use crate::core::error::Result;

/// A push reader or the precise reason no reader is available.
#[derive(Clone)]
pub enum PushStaging {
    /// This checkout has no staging directory.
    Missing,
    /// A shared reader owns the staging lock for this push.
    Ready(Arc<StagingAreaReadOnly>),
    /// An exclusive holder outlasted the staging lock acquisition budget.
    Locked { holder_pid: Option<u32> },
}

impl PushStaging {
    pub(crate) async fn open(root: PathBuf) -> Result<Self> {
        if !root.try_exists()? {
            return Ok(Self::Missing);
        }
        Self::from_open_result(StagingAreaReadOnly::open_blocking_default(root).await)
    }

    fn from_open_result(result: crab_staging::Result<StagingAreaReadOnly>) -> Result<Self> {
        // Only contention can be ignored by a pointer-free push. A damaged
        // index or I/O failure must retain its source, not masquerade as a lock.
        match result {
            Ok(reader) => Ok(Self::Ready(Arc::new(reader))),
            Err(StagingError::StagingLocked { holder_pid }) => Ok(Self::Locked { holder_pid }),
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) fn reader(&self) -> Option<&Arc<StagingAreaReadOnly>> {
        match self {
            Self::Ready(reader) => Some(reader),
            Self::Missing | Self::Locked { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::error::CrabError;

    #[tokio::test]
    async fn missing_directory_is_not_created() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let root = temporary.path().join("staging");
        let opened = PushStaging::open(root.clone())
            .await
            .expect("missing staging");
        assert!(matches!(opened, PushStaging::Missing) && !root.exists());
    }

    #[tokio::test]
    async fn missing_index_is_an_error_not_a_lock() {
        let temporary = tempfile::tempdir().expect("tempdir");
        let error = PushStaging::open(temporary.path().to_path_buf())
            .await
            .err()
            .expect("missing index must fail");
        assert!(matches!(error, CrabError::NotFound { path } if path.ends_with("index.db")));
    }

    #[test]
    fn contention_preserves_the_observed_holder() {
        for holder_pid in [None, Some(1234)] {
            let opened =
                PushStaging::from_open_result(Err(StagingError::StagingLocked { holder_pid }))
                    .expect("deferred contention");
            assert!(
                matches!(opened, PushStaging::Locked { holder_pid: actual } if actual == holder_pid)
            );
        }
    }

    #[test]
    fn io_failure_retains_its_source() {
        let error = PushStaging::from_open_result(Err(StagingError::Io(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "staging read denied",
        ))))
        .err()
        .expect("I/O failure must not be deferred");
        assert!(
            matches!(error, CrabError::Io(source) if source.kind() == std::io::ErrorKind::PermissionDenied)
        );
    }
}
