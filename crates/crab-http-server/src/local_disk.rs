use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use tokio_util::sync::CancellationToken;

#[cfg(test)]
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
    budget: crab_cell_runtime::DiskBudget,
    disk_reserve_bytes: u64,
    #[cfg(test)]
    _root_owner: Option<Arc<tempfile::TempDir>>,
}

impl LocalStaging {
    pub(crate) fn new(
        root: PathBuf,
        budget: crab_cell_runtime::DiskBudget,
        disk_reserve_bytes: u64,
    ) -> Result<Self, Error> {
        if budget.capacity() == 0 {
            return Err(Error::TooLarge);
        }
        std::fs::create_dir_all(&root)?;
        Ok(Self {
            root: Arc::new(root),
            budget,
            disk_reserve_bytes,
            #[cfg(test)]
            _root_owner: None,
        })
    }

    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        let owner = Arc::new(tempfile::TempDir::new().unwrap());
        let root = owner.path().join("staging");
        let mut staging =
            Self::new(root, crab_cell_runtime::DiskBudget::new(8 * 1024 * MIB), 0).unwrap();
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
        let bytes = bytes.max(1);
        if bytes > self.budget.capacity() {
            return Err(Error::TooLarge);
        }
        let reservation = self.budget.try_reserve(bytes).map_err(|_| Error::Busy)?;
        let required = self
            .disk_reserve_bytes
            .checked_add(self.budget.used())
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
            _reservation: reservation,
        })
    }

    #[cfg(test)]
    pub(crate) fn available_bytes(&self) -> u64 {
        self.budget.available()
    }

    #[cfg(test)]
    pub(crate) fn available_mebibytes(&self) -> u64 {
        self.available_bytes() / MIB
    }
}

pub(crate) struct StagingDirectory {
    directory: tempfile::TempDir,
    _reservation: crab_cell_runtime::DiskReservation,
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
        let staging = LocalStaging::new(
            owner.path().join("staging"),
            crab_cell_runtime::DiskBudget::new(2 * MIB),
            0,
        )
        .unwrap();
        let cancellation = CancellationToken::new();
        let first = staging.create(2 * MIB, &cancellation).await.unwrap();
        assert!(first.path().starts_with(owner.path()));
        assert_eq!(staging.available_bytes(), 0);
        assert!(matches!(
            staging.create(1, &cancellation).await,
            Err(Error::Busy)
        ));
        drop(first);
        assert_eq!(staging.available_bytes(), 2 * MIB);
        drop(staging.create(1, &cancellation).await.unwrap());
        assert_eq!(staging.available_bytes(), 2 * MIB);
    }

    #[tokio::test]
    async fn staging_shares_capacity_with_cell_disk_reservations() {
        let owner = tempfile::TempDir::new().unwrap();
        let budget = crab_cell_runtime::DiskBudget::new(2 * MIB);
        let staging = LocalStaging::new(owner.path().join("staging"), budget.clone(), 0).unwrap();
        let cell = budget.try_reserve(MIB + 1).unwrap();

        assert!(matches!(
            staging.create(MIB, &CancellationToken::new()).await,
            Err(Error::Busy)
        ));
        drop(cell);
        assert!(staging.create(MIB, &CancellationToken::new()).await.is_ok());
    }

    #[tokio::test]
    async fn oversized_and_cancelled_requests_never_create_a_directory() {
        let owner = tempfile::TempDir::new().unwrap();
        let root = owner.path().join("staging");
        let staging =
            LocalStaging::new(root.clone(), crab_cell_runtime::DiskBudget::new(MIB), 0).unwrap();
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
        let reserve = u64::MAX - MIB;
        let staging = LocalStaging::new(
            root.clone(),
            crab_cell_runtime::DiskBudget::new(MIB),
            reserve,
        )
        .unwrap();
        assert!(matches!(
            staging.create(1, &CancellationToken::new()).await,
            Err(Error::Busy)
        ));
        assert_eq!(staging.available_bytes(), MIB);
        assert_eq!(std::fs::read_dir(root).unwrap().count(), 0);
    }
}
