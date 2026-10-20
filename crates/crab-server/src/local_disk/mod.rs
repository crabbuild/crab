use std::{
    io,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use tokio_util::sync::CancellationToken;

#[cfg(test)]
const MIB: u64 = 1024 * 1024;
pub(crate) const RESERVATION_GROWTH: u64 = 256 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub(crate) enum Error {
    #[error("local staging request exceeds the node disk budget")]
    TooLarge,
    #[error("local staging disk is busy")]
    Busy(#[source] Option<Box<dyn std::error::Error + Send + Sync>>),
    #[error("local staging was cancelled")]
    Cancelled,
    #[error("local staging I/O failed")]
    Io(#[from] std::io::Error),
    #[error("local staging worker failed")]
    Worker(#[from] tokio::task::JoinError),
}

#[derive(Clone, Debug)]
pub(crate) struct DiskBudget(Arc<DiskBudgetState>);

#[derive(Debug)]
struct DiskBudgetState {
    capacity: u64,
    used: AtomicU64,
}

#[derive(Debug)]
pub(crate) struct DiskReservation {
    budget: DiskBudget,
    bytes: AtomicU64,
}

impl DiskBudget {
    pub(crate) fn new(capacity: u64) -> Self {
        Self(Arc::new(DiskBudgetState {
            capacity,
            used: AtomicU64::new(0),
        }))
    }
    pub(crate) fn capacity(&self) -> u64 {
        self.0.capacity
    }
    pub(crate) fn used(&self) -> u64 {
        self.0.used.load(Ordering::Acquire)
    }
    pub(crate) fn available(&self) -> u64 {
        self.capacity().saturating_sub(self.used())
    }
    fn acquire(&self, bytes: u64) -> Result<(), Error> {
        self.0
            .used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes)
                    .filter(|used| *used <= self.capacity())
            })
            .map(|_| ())
            .map_err(|_| Error::Busy(None))
    }
    pub(crate) fn try_reserve(&self, bytes: u64) -> Result<DiskReservation, Error> {
        self.acquire(bytes)?;
        Ok(DiskReservation {
            budget: self.clone(),
            bytes: AtomicU64::new(bytes),
        })
    }
}

impl DiskReservation {
    pub(crate) fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Acquire)
    }
    pub(crate) fn resize(&self, desired: u64) -> Result<(), Error> {
        loop {
            let previous = self.bytes();
            let growth = desired.saturating_sub(previous);
            self.budget.acquire(growth)?;
            match self.bytes.compare_exchange(
                previous,
                desired,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => {
                    self.budget
                        .0
                        .used
                        .fetch_sub(previous.saturating_sub(desired), Ordering::AcqRel);
                    return Ok(());
                }
                Err(_) => {
                    self.budget.0.used.fetch_sub(growth, Ordering::AcqRel);
                }
            }
        }
    }
}

impl Drop for DiskReservation {
    fn drop(&mut self) {
        self.budget.0.used.fetch_sub(self.bytes(), Ordering::AcqRel);
    }
}

#[derive(Clone)]
pub(crate) struct LocalStaging {
    root: Arc<PathBuf>,
    budget: DiskBudget,
    disk_reserve_bytes: u64,
    restart_inventory: Option<Arc<RestartDiskInventory>>,
    #[cfg(test)]
    _root_owner: Option<Arc<tempfile::TempDir>>,
}

#[derive(Debug)]
struct RestartDiskInventory {
    _reservation: DiskReservation,
    bytes: u64,
    sessions: usize,
}

impl RestartDiskInventory {
    #[cfg(test)]
    fn bytes(&self) -> u64 {
        self.bytes
    }

    #[cfg(test)]
    fn sessions(&self) -> usize {
        self.sessions
    }
}

impl LocalStaging {
    pub(crate) fn new(
        root: PathBuf,
        budget: DiskBudget,
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
        budget: DiskBudget,
        disk_reserve_bytes: u64,
        data_dir: &Path,
        current_session: &Path,
    ) -> Result<Self, Error> {
        // Staging owns its session directory now that Cellar has a separate
        // cache root. Inventory must see the new session before checking it.
        let mut staging = Self::new(root, budget.clone(), disk_reserve_bytes)?;
        let inventory = Arc::new(reserve_restart_inventory(
            data_dir,
            current_session,
            budget,
        )?);
        staging.restart_inventory = Some(inventory);
        Ok(staging)
    }

    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        let owner = Arc::new(tempfile::TempDir::new().unwrap());
        let root = owner.path().join("staging");
        let mut staging = Self::new(root, DiskBudget::new(8 * 1024 * MIB), 0).unwrap();
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
        let reservation = self
            .budget
            .try_reserve(bytes)
            .map_err(|source| Error::Busy(Some(Box::new(source))))?;
        let required = self
            .disk_reserve_bytes
            .checked_add(self.budget.used())
            .ok_or(Error::TooLarge)?;
        if fs4::available_space(self.root.as_path())? < required {
            return Err(Error::Busy(None));
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
        if bytes > self.budget.capacity() {
            return Err(Error::TooLarge);
        }
        let previous = directory._reservation.bytes();
        directory
            ._reservation
            .resize(bytes.max(1))
            .map_err(|source| Error::Busy(Some(Box::new(source))))?;
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
            return Err(Error::Busy(None));
        }
        Ok(())
    }

    pub(crate) fn scratch(
        &self,
        directory: Arc<StagingDirectory>,
    ) -> Result<crab_git::incoming_pack::ScratchSpace, Error> {
        let body_bytes = inventory_bytes(directory.path())?;
        self.resize(&directory, body_bytes)?;
        let path = directory.path().to_owned();
        Ok(crab_git::incoming_pack::ScratchSpace::new(
            &path,
            StagingAdmission {
                staging: self.clone(),
                directory,
                body_bytes,
                last_bytes: AtomicU64::new(0),
            },
        ))
    }

    pub(crate) fn available_bytes(&self) -> u64 {
        self.budget.available()
    }

    pub(crate) fn restart_inventory_usage(&self) -> Option<(u64, usize)> {
        self.restart_inventory
            .as_ref()
            .map(|inventory| (inventory.bytes, inventory.sessions))
    }

    #[cfg(test)]
    pub(crate) fn available_mebibytes(&self) -> u64 {
        self.available_bytes() / MIB
    }
}

struct StagingAdmission {
    staging: LocalStaging,
    directory: Arc<StagingDirectory>,
    body_bytes: u64,
    last_bytes: AtomicU64,
}

impl crab_git::incoming_pack::ScratchAdmission for StagingAdmission {
    fn resize(&self, bytes: u64) -> Result<(), crab_git::incoming_pack::ScratchError> {
        use crab_git::incoming_pack::ScratchError;
        let required = self
            .body_bytes
            .checked_add(bytes)
            .ok_or(ScratchError::Overflow)?;
        let previous = self.directory._reservation.bytes();
        let shrinking = bytes < self.last_bytes.load(Ordering::Relaxed);
        if shrinking || required > previous {
            let desired = if shrinking {
                required
            } else {
                required
                    .saturating_add(RESERVATION_GROWTH)
                    .min(self.staging.budget.capacity())
                    .min(previous.saturating_add(self.staging.budget.available()))
                    .max(required)
            };
            self.staging
                .resize(&self.directory, desired)
                .map_err(|source| ScratchError::Admission(Box::new(source)))?;
        }
        // ScratchSpace serializes this policy. Retain admitted headroom between
        // bounded writes; release it when a private directory actually disappears.
        self.last_bytes.store(bytes, Ordering::Relaxed);
        Ok(())
    }
}

fn reserve_restart_inventory(
    data_dir: &Path,
    current_session: &Path,
    budget: DiskBudget,
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
                _reservation: budget
                    .try_reserve(0)
                    .map_err(|source| Error::Busy(Some(Box::new(source))))?,
                bytes: 0,
                sessions: 0,
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

    let reservation = budget
        .try_reserve(bytes)
        .map_err(|source| Error::Busy(Some(Box::new(source))))?;
    Ok(RestartDiskInventory {
        _reservation: reservation,
        bytes,
        sessions: session_count,
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
    _reservation: DiskReservation,
}

impl StagingDirectory {
    pub(crate) fn path(&self) -> &Path {
        self.directory.path()
    }
}

#[cfg(test)]
mod tests;
