use std::{
    io,
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
    restart_inventory: Option<Arc<RestartDiskInventory>>,
    #[cfg(test)]
    _root_owner: Option<Arc<tempfile::TempDir>>,
}

#[derive(Debug)]
struct RestartDiskInventory {
    _reservation: crab_cell_runtime::DiskReservation,
    _bytes: u64,
    _sessions: usize,
}

impl RestartDiskInventory {
    #[cfg(test)]
    fn bytes(&self) -> u64 {
        self._bytes
    }

    #[cfg(test)]
    fn sessions(&self) -> usize {
        self._sessions
    }
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
            restart_inventory: None,
            #[cfg(test)]
            _root_owner: None,
        })
    }

    pub(crate) fn new_with_restart_inventory(
        root: PathBuf,
        budget: crab_cell_runtime::DiskBudget,
        disk_reserve_bytes: u64,
        data_dir: &Path,
        current_session: &Path,
    ) -> Result<Self, Error> {
        let inventory = Arc::new(reserve_restart_inventory(
            data_dir,
            current_session,
            budget.clone(),
        )?);
        let mut staging = Self::new(root, budget, disk_reserve_bytes)?;
        staging.restart_inventory = Some(inventory);
        Ok(staging)
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

    pub(crate) fn resize(&self, directory: &StagingDirectory, bytes: u64) -> Result<(), Error> {
        let previous = directory._reservation.bytes();
        directory
            ._reservation
            .resize(bytes.max(1))
            .map_err(|_| Error::Busy)?;
        let required = match self.disk_reserve_bytes.checked_add(self.budget.used()) {
            Some(required) => required,
            None => {
                let _ = directory._reservation.resize(previous);
                return Err(Error::TooLarge);
            }
        };
        let available = match fs4::available_space(self.root.as_path()) {
            Ok(available) => available,
            Err(error) => {
                let _ = directory._reservation.resize(previous);
                return Err(Error::Io(error));
            }
        };
        if available < required {
            let _ = directory._reservation.resize(previous);
            return Err(Error::Busy);
        }
        Ok(())
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

fn reserve_restart_inventory(
    data_dir: &Path,
    current_session: &Path,
    budget: crab_cell_runtime::DiskBudget,
) -> Result<RestartDiskInventory, Error> {
    let sessions = data_dir.join("sessions");
    let current_name = current_session
        .file_name()
        .ok_or_else(|| invalid_inventory("current session has no directory name"))?;
    if current_session.parent() != Some(sessions.as_path()) {
        return Err(invalid_inventory("current session is outside the sessions root").into());
    }
    match std::fs::symlink_metadata(&sessions) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            return Err(invalid_inventory("sessions root is not a directory").into());
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(RestartDiskInventory {
                _reservation: budget.try_reserve(0).map_err(|_| Error::Busy)?,
                _bytes: 0,
                _sessions: 0,
            });
        }
        Err(error) => return Err(error.into()),
    }
    let current_metadata = std::fs::symlink_metadata(current_session)?;
    if !current_metadata.is_dir() || current_metadata.file_type().is_symlink() {
        return Err(invalid_inventory("current session is not a regular directory").into());
    }

    let mut bytes = 0_u64;
    let mut session_count = 0_usize;
    for entry in std::fs::read_dir(&sessions)? {
        let entry = entry?;
        if entry.file_name() == current_name {
            continue;
        }
        let metadata = std::fs::symlink_metadata(entry.path())?;
        if !metadata.is_dir() {
            return Err(invalid_inventory("sessions root contains a non-directory entry").into());
        }
        bytes = bytes
            .checked_add(inventory_bytes(&entry.path())?)
            .ok_or(Error::TooLarge)?;
        session_count = session_count.checked_add(1).ok_or(Error::TooLarge)?;
    }

    let reservation = budget.try_reserve(bytes).map_err(|_| Error::Busy)?;
    Ok(RestartDiskInventory {
        _reservation: reservation,
        _bytes: bytes,
        _sessions: session_count,
    })
}

fn inventory_bytes(path: &Path) -> Result<u64, Error> {
    let metadata = std::fs::symlink_metadata(path)?;
    let file_type = metadata.file_type();
    if file_type.is_symlink() {
        return Err(invalid_inventory("restart inventory contains a symlink").into());
    }
    if file_type.is_file() {
        return Ok(metadata.len());
    }
    if !file_type.is_dir() {
        return Err(invalid_inventory("restart inventory contains a special file").into());
    }
    let mut bytes = 0_u64;
    for entry in std::fs::read_dir(path)? {
        bytes = bytes
            .checked_add(inventory_bytes(&entry?.path())?)
            .ok_or(Error::TooLarge)?;
    }
    Ok(bytes)
}

fn invalid_inventory(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
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

    #[test]
    fn restart_inventory_counts_stale_sessions_but_not_the_new_session() {
        let data = tempfile::TempDir::new().unwrap();
        let sessions = data.path().join("sessions");
        let stale = sessions.join("stale-session");
        let current = sessions.join("current-session");
        std::fs::create_dir_all(stale.join("cell/.crab-cell-directory-cache")).unwrap();
        std::fs::create_dir_all(&current).unwrap();
        std::fs::write(stale.join("cell.sqlite"), [0_u8; 17]).unwrap();
        std::fs::write(
            stale.join("cell/.crab-cell-directory-cache/node"),
            [0_u8; 23],
        )
        .unwrap();
        std::fs::write(current.join("new.sqlite"), [0_u8; 101]).unwrap();

        let budget = crab_cell_runtime::DiskBudget::new(40);
        let inventory = reserve_restart_inventory(data.path(), &current, budget.clone()).unwrap();

        assert_eq!(inventory.bytes(), 40);
        assert_eq!(inventory.sessions(), 1);
        assert_eq!(budget.used(), 40);
    }

    #[test]
    fn restart_inventory_accounts_for_cell_recovery_and_compaction_scratch() {
        let data = tempfile::TempDir::new().unwrap();
        let sessions = data.path().join("sessions");
        let stale = sessions.join("stale-session");
        let current = sessions.join("current-session");
        let cell = stale.join("cell");
        std::fs::create_dir_all(&cell).unwrap();
        std::fs::create_dir_all(&current).unwrap();
        std::fs::write(cell.join(".crab-compaction-source-indexes"), [0_u8; 13]).unwrap();
        std::fs::write(cell.join(".crab-recovery-bundle"), [0_u8; 17]).unwrap();

        let budget = crab_cell_runtime::DiskBudget::new(30);
        let inventory = reserve_restart_inventory(data.path(), &current, budget.clone()).unwrap();

        assert_eq!(inventory.bytes(), 30);
        assert_eq!(inventory.sessions(), 1);
        assert_eq!(budget.used(), 30);
    }

    #[test]
    fn restart_inventory_fails_closed_when_stale_bytes_exceed_capacity() {
        let data = tempfile::TempDir::new().unwrap();
        let sessions = data.path().join("sessions");
        let stale = sessions.join("stale-session");
        let current = sessions.join("current-session");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::create_dir_all(&current).unwrap();
        std::fs::write(stale.join("cell.sqlite"), [0_u8; 9]).unwrap();

        let budget = crab_cell_runtime::DiskBudget::new(8);
        assert!(matches!(
            reserve_restart_inventory(data.path(), &current, budget.clone()),
            Err(Error::Busy)
        ));
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn staging_holds_restart_inventory_until_the_owner_drops() {
        let data = tempfile::TempDir::new().unwrap();
        let sessions = data.path().join("sessions");
        let stale = sessions.join("stale-session");
        let current = sessions.join("current-session");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::create_dir_all(&current).unwrap();
        std::fs::write(stale.join("cell.sqlite"), [0_u8; 7]).unwrap();
        let budget = crab_cell_runtime::DiskBudget::new(16);

        let staging = LocalStaging::new_with_restart_inventory(
            current.join("transfers"),
            budget.clone(),
            0,
            data.path(),
            &current,
        )
        .unwrap();
        assert_eq!(budget.used(), 7);
        drop(staging);
        assert_eq!(budget.used(), 0);
    }

    #[test]
    fn restart_inventory_rejects_symlinked_owned_files() {
        let data = tempfile::TempDir::new().unwrap();
        let sessions = data.path().join("sessions");
        let stale = sessions.join("stale-session");
        let current = sessions.join("current-session");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::create_dir_all(&current).unwrap();
        std::fs::write(data.path().join("outside"), [0_u8; 1]).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(data.path().join("outside"), stale.join("link")).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_file(data.path().join("outside"), stale.join("link"))
            .unwrap();

        let error = reserve_restart_inventory(
            data.path(),
            &current,
            crab_cell_runtime::DiskBudget::new(16),
        )
        .unwrap_err();
        assert!(matches!(error, Error::Io(source) if source.kind() == io::ErrorKind::InvalidData));
    }

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
    async fn staging_reservation_grows_without_overcommitting_shared_capacity() {
        let owner = tempfile::TempDir::new().unwrap();
        let staging = LocalStaging::new(
            owner.path().join("staging"),
            crab_cell_runtime::DiskBudget::new(3 * MIB),
            0,
        )
        .unwrap();
        let directory = staging
            .create(MIB, &CancellationToken::new())
            .await
            .unwrap();

        staging.resize(&directory, 3 * MIB).unwrap();
        assert_eq!(staging.available_bytes(), 0);
        assert!(matches!(
            staging.create(1, &CancellationToken::new()).await,
            Err(Error::Busy)
        ));
        drop(directory);
        assert_eq!(staging.available_bytes(), 3 * MIB);
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
