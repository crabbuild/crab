use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError};
use tokio_util::sync::CancellationToken;

const MIB: u64 = 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("local staging request exceeds the node disk budget")]
    TooLarge,
    #[error("local staging disk is busy")]
    Busy,
    #[error("local staging was cancelled")]
    Cancelled,
    #[error("local staging I/O failed")]
    Io(#[from] std::io::Error),
    #[error("local staging worker failed")]
    Worker(#[from] tokio::task::JoinError),
}

#[derive(Clone)]
pub(crate) struct LocalStaging {
    root: Arc<PathBuf>,
    slots: Arc<Semaphore>,
    capacity: u32,
    disk_reserve_bytes: u64,
    #[cfg(test)]
    _root_owner: Option<Arc<tempfile::TempDir>>,
}

impl LocalStaging {
    pub(crate) fn new(
        root: PathBuf,
        capacity_mebibytes: usize,
        disk_reserve_bytes: u64,
    ) -> Result<Self, Error> {
        let capacity = u32::try_from(capacity_mebibytes).map_err(|_| Error::TooLarge)?;
        if capacity == 0 || capacity_mebibytes > Semaphore::MAX_PERMITS {
            return Err(Error::TooLarge);
        }
        std::fs::create_dir_all(&root)?;
        Ok(Self {
            root: Arc::new(root),
            slots: Arc::new(Semaphore::new(capacity_mebibytes)),
            capacity,
            disk_reserve_bytes,
            #[cfg(test)]
            _root_owner: None,
        })
    }

    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        let owner = Arc::new(tempfile::TempDir::new().unwrap());
        let root = owner.path().join("staging");
        let mut staging = Self::new(root, 8 * 1024, 0).unwrap();
        staging._root_owner = Some(owner);
        staging
    }

    pub(crate) async fn create(
        &self,
        bytes: u64,
        cancellation: &CancellationToken,
    ) -> Result<StagingDirectory, Error> {
        if cancellation.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let units = bytes.max(1).checked_add(MIB - 1).ok_or(Error::TooLarge)? / MIB;
        let units = u32::try_from(units).map_err(|_| Error::TooLarge)?;
        if units > self.capacity {
            return Err(Error::TooLarge);
        }
        let permit =
            self.slots
                .clone()
                .try_acquire_many_owned(units)
                .map_err(|error| match error {
                    TryAcquireError::NoPermits | TryAcquireError::Closed => Error::Busy,
                })?;
        let reserved = u64::from(self.capacity - self.available_units())
            .checked_mul(MIB)
            .ok_or(Error::TooLarge)?;
        let required = self
            .disk_reserve_bytes
            .checked_add(reserved)
            .ok_or(Error::TooLarge)?;
        if fs4::available_space(self.root.as_path())? < required {
            return Err(Error::Busy);
        }
        let root = Arc::clone(&self.root);
        let directory = tokio::task::spawn_blocking(move || {
            tempfile::Builder::new()
                .prefix("transfer-")
                .tempdir_in(root.as_path())
        })
        .await??;
        if cancellation.is_cancelled() {
            return Err(Error::Cancelled);
        }
        Ok(StagingDirectory {
            directory,
            _permit: permit,
        })
    }

    fn available_units(&self) -> u32 {
        u32::try_from(self.slots.available_permits()).unwrap_or(u32::MAX)
    }

    #[cfg(test)]
    pub(crate) fn available_mebibytes(&self) -> u32 {
        self.available_units()
    }
}

pub(crate) struct StagingDirectory {
    directory: tempfile::TempDir,
    _permit: OwnedSemaphorePermit,
}

impl StagingDirectory {
    pub(crate) fn path(&self) -> &Path {
        self.directory.path()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn capacity_precedes_creation_and_releases_with_the_directory() {
        let owner = tempfile::TempDir::new().unwrap();
        let staging = LocalStaging::new(owner.path().join("staging"), 2, 0).unwrap();
        let cancellation = CancellationToken::new();
        let first = staging.create(2 * MIB, &cancellation).await.unwrap();
        assert!(first.path().starts_with(owner.path()));
        assert_eq!(staging.available_mebibytes(), 0);
        assert!(matches!(
            staging.create(1, &cancellation).await,
            Err(Error::Busy)
        ));
        drop(first);
        assert_eq!(staging.available_mebibytes(), 2);
        drop(staging.create(1, &cancellation).await.unwrap());
        assert_eq!(staging.available_mebibytes(), 2);
    }

    #[tokio::test]
    async fn oversized_and_cancelled_requests_never_create_a_directory() {
        let owner = tempfile::TempDir::new().unwrap();
        let root = owner.path().join("staging");
        let staging = LocalStaging::new(root.clone(), 1, 0).unwrap();
        assert!(matches!(
            staging.create(2 * MIB, &CancellationToken::new()).await,
            Err(Error::TooLarge)
        ));
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        assert!(matches!(
            staging.create(1, &cancellation).await,
            Err(Error::Cancelled)
        ));
        assert_eq!(std::fs::read_dir(root).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn actual_free_space_preserves_the_configured_node_reserve() {
        let owner = tempfile::TempDir::new().unwrap();
        let root = owner.path().join("staging");
        std::fs::create_dir_all(&root).unwrap();
        let free = fs4::available_space(&root).unwrap();
        let staging = LocalStaging::new(root.clone(), 1, free).unwrap();
        assert!(matches!(
            staging.create(1, &CancellationToken::new()).await,
            Err(Error::Busy)
        ));
        assert_eq!(staging.available_mebibytes(), 1);
        assert_eq!(std::fs::read_dir(root).unwrap().count(), 0);
    }
}
