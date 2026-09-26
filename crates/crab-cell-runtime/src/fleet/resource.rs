//! Node resource ledger: memory, disk, and job admission.
use std::sync::{Arc, Mutex, Weak};

use crate::{Error, Result};

pub(crate) const ACTIVE_CELL_NATIVE_BYTES: usize = 64 * 1024;
/// Persistent database, WAL, SHM and capture descriptors reserved per active Cell.
pub const ACTIVE_CELL_FILE_DESCRIPTORS: usize = 8;
// Conservatively cover the entire 8 MiB shared page cache plus 4 MiB for
// SQLite, fetch/decode buffers and view metadata. Do not assume another view
// or writer pays for the cache; measured sharing may reduce this charge later.
pub(crate) const READ_REPLICA_NATIVE_BYTES: usize = 12 * 1024 * 1024;
pub(crate) const READ_REPLICA_FILE_DESCRIPTORS: usize = 4;
pub(crate) const HYDRATION_JOB_CAPACITY: usize = 2;

/// Bounded resources owned by one runtime admission token.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResourceCost {
    active_cells: usize,
    resident_bytes: usize,
    file_descriptors: usize,
    retained_bytes: usize,
    disk_bytes: u64,
    worker_jobs: usize,
    primitive_jobs: usize,
    hydration_jobs: usize,
    io_slots: usize,
    blocking_jobs: usize,
    recovery_jobs: usize,
    dirty_jobs: usize,
    scratch_units: usize,
}

impl ResourceCost {
    /// Cost of one active Cell with no work in flight.
    #[must_use]
    pub const fn active_cell() -> Self {
        Self {
            active_cells: 1,
            resident_bytes: ACTIVE_CELL_NATIVE_BYTES,
            file_descriptors: ACTIVE_CELL_FILE_DESCRIPTORS,
            ..Self::zero()
        }
    }

    /// Empty cost, used as the base for [`ResourceCost::active_cell`].
    #[must_use]
    pub const fn zero() -> Self {
        Self {
            active_cells: 0,
            resident_bytes: 0,
            file_descriptors: 0,
            retained_bytes: 0,
            disk_bytes: 0,
            worker_jobs: 0,
            primitive_jobs: 0,
            hydration_jobs: 0,
            io_slots: 0,
            blocking_jobs: 0,
            recovery_jobs: 0,
            dirty_jobs: 0,
            scratch_units: 0,
        }
    }

    /// Returns the Cells this cost covers.
    #[must_use]
    pub const fn active_cells(self) -> usize {
        self.active_cells
    }

    /// Returns the resident memory in bytes.
    #[must_use]
    pub const fn resident_bytes(self) -> usize {
        self.resident_bytes
    }

    /// Returns the open file descriptors.
    #[must_use]
    pub const fn file_descriptors(self) -> usize {
        self.file_descriptors
    }

    /// Returns the bytes retained beyond the active set.
    #[must_use]
    pub const fn retained_bytes(self) -> usize {
        self.retained_bytes
    }

    /// Returns the scratch disk in bytes.
    #[must_use]
    pub const fn disk_bytes(self) -> u64 {
        self.disk_bytes
    }

    /// Returns the SQL worker jobs.
    #[must_use]
    pub const fn worker_jobs(self) -> usize {
        self.worker_jobs
    }

    /// Returns the primitive maintenance jobs.
    #[must_use]
    pub const fn primitive_jobs(self) -> usize {
        self.primitive_jobs
    }

    /// Returns the hydration jobs.
    #[must_use]
    pub const fn hydration_jobs(self) -> usize {
        self.hydration_jobs
    }

    /// Returns the object-store I/O slots.
    #[must_use]
    pub const fn io_slots(self) -> usize {
        self.io_slots
    }

    /// Returns the blocking activity jobs.
    #[must_use]
    pub const fn blocking_jobs(self) -> usize {
        self.blocking_jobs
    }

    /// Returns the recovery jobs.
    #[must_use]
    pub const fn recovery_jobs(self) -> usize {
        self.recovery_jobs
    }

    /// Returns the dirty, not-yet-published jobs.
    #[must_use]
    pub const fn dirty_jobs(self) -> usize {
        self.dirty_jobs
    }

    /// Returns the scratch units.
    #[must_use]
    pub const fn scratch_units(self) -> usize {
        self.scratch_units
    }

    /// Sets the retained bytes.
    #[must_use]
    pub const fn with_retained_bytes(mut self, bytes: usize) -> Self {
        self.retained_bytes = bytes;
        self
    }

    /// Sets the resident bytes.
    #[must_use]
    pub const fn with_resident_bytes(mut self, bytes: usize) -> Self {
        self.resident_bytes = bytes;
        self
    }

    /// Sets the open file descriptors.
    #[must_use]
    pub const fn with_file_descriptors(mut self, descriptors: usize) -> Self {
        self.file_descriptors = descriptors;
        self
    }

    /// Sets the Cell count.
    #[must_use]
    pub const fn with_active_cells(mut self, cells: usize) -> Self {
        self.active_cells = cells;
        self
    }

    /// Sets the scratch disk in bytes.
    #[must_use]
    pub const fn with_disk_bytes(mut self, bytes: u64) -> Self {
        self.disk_bytes = bytes;
        self
    }

    /// Sets the SQL worker jobs.
    #[must_use]
    pub const fn with_worker_jobs(mut self, jobs: usize) -> Self {
        self.worker_jobs = jobs;
        self
    }

    /// Sets the primitive maintenance jobs.
    #[must_use]
    pub const fn with_primitive_jobs(mut self, jobs: usize) -> Self {
        self.primitive_jobs = jobs;
        self
    }

    /// Sets the hydration jobs.
    #[must_use]
    pub const fn with_hydration_jobs(mut self, jobs: usize) -> Self {
        self.hydration_jobs = jobs;
        self
    }

    /// Sets the object-store I/O slots.
    #[must_use]
    pub const fn with_io_slots(mut self, slots: usize) -> Self {
        self.io_slots = slots;
        self
    }

    /// Sets the blocking activity jobs.
    #[must_use]
    pub const fn with_blocking_jobs(mut self, jobs: usize) -> Self {
        self.blocking_jobs = jobs;
        self
    }

    /// Sets the recovery jobs.
    #[must_use]
    pub const fn with_recovery_jobs(mut self, jobs: usize) -> Self {
        self.recovery_jobs = jobs;
        self
    }

    /// Sets the dirty, not-yet-published jobs.
    #[must_use]
    pub const fn with_dirty_jobs(mut self, jobs: usize) -> Self {
        self.dirty_jobs = jobs;
        self
    }

    /// Sets the scratch units.
    #[must_use]
    pub const fn with_scratch_units(mut self, units: usize) -> Self {
        self.scratch_units = units;
        self
    }

    fn checked_add(self, other: Self) -> Option<Self> {
        Some(Self {
            active_cells: self.active_cells.checked_add(other.active_cells)?,
            resident_bytes: self.resident_bytes.checked_add(other.resident_bytes)?,
            file_descriptors: self.file_descriptors.checked_add(other.file_descriptors)?,
            retained_bytes: self.retained_bytes.checked_add(other.retained_bytes)?,
            disk_bytes: self.disk_bytes.checked_add(other.disk_bytes)?,
            worker_jobs: self.worker_jobs.checked_add(other.worker_jobs)?,
            primitive_jobs: self.primitive_jobs.checked_add(other.primitive_jobs)?,
            hydration_jobs: self.hydration_jobs.checked_add(other.hydration_jobs)?,
            io_slots: self.io_slots.checked_add(other.io_slots)?,
            blocking_jobs: self.blocking_jobs.checked_add(other.blocking_jobs)?,
            recovery_jobs: self.recovery_jobs.checked_add(other.recovery_jobs)?,
            dirty_jobs: self.dirty_jobs.checked_add(other.dirty_jobs)?,
            scratch_units: self.scratch_units.checked_add(other.scratch_units)?,
        })
    }

    fn checked_sub(self, other: Self) -> Option<Self> {
        Some(Self {
            active_cells: self.active_cells.checked_sub(other.active_cells)?,
            resident_bytes: self.resident_bytes.checked_sub(other.resident_bytes)?,
            file_descriptors: self.file_descriptors.checked_sub(other.file_descriptors)?,
            retained_bytes: self.retained_bytes.checked_sub(other.retained_bytes)?,
            disk_bytes: self.disk_bytes.checked_sub(other.disk_bytes)?,
            worker_jobs: self.worker_jobs.checked_sub(other.worker_jobs)?,
            primitive_jobs: self.primitive_jobs.checked_sub(other.primitive_jobs)?,
            hydration_jobs: self.hydration_jobs.checked_sub(other.hydration_jobs)?,
            io_slots: self.io_slots.checked_sub(other.io_slots)?,
            blocking_jobs: self.blocking_jobs.checked_sub(other.blocking_jobs)?,
            recovery_jobs: self.recovery_jobs.checked_sub(other.recovery_jobs)?,
            dirty_jobs: self.dirty_jobs.checked_sub(other.dirty_jobs)?,
            scratch_units: self.scratch_units.checked_sub(other.scratch_units)?,
        })
    }

    fn fits_within(self, limit: Self) -> bool {
        self.active_cells <= limit.active_cells
            && self.resident_bytes <= limit.resident_bytes
            && self.file_descriptors <= limit.file_descriptors
            && self.retained_bytes <= limit.retained_bytes
            && self.disk_bytes <= limit.disk_bytes
            && self.worker_jobs <= limit.worker_jobs
            && self.primitive_jobs <= limit.primitive_jobs
            && self.hydration_jobs <= limit.hydration_jobs
            && self.io_slots <= limit.io_slots
            && self.blocking_jobs <= limit.blocking_jobs
            && self.recovery_jobs <= limit.recovery_jobs
            && self.dirty_jobs <= limit.dirty_jobs
            && self.scratch_units <= limit.scratch_units
    }
}

/// Point-in-time usage and limits for one resource ledger.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResourceSnapshot {
    /// Cost currently reserved.
    pub used: ResourceCost,
    /// Cost the ledger admits.
    pub limit: ResourceCost,
}

#[derive(Clone)]
pub(crate) struct ResourceLedger {
    // Reservations are short synchronous critical sections; no ledger lock is
    // held while a worker, filesystem, or provider operation runs.
    state: Arc<LedgerState>,
}

pub(crate) struct LedgerState {
    snapshot: Mutex<ResourceSnapshot>,
    released: tokio::sync::Notify,
}

impl ResourceLedger {
    pub(crate) fn new(limit: ResourceCost) -> Self {
        Self {
            state: Arc::new(LedgerState {
                snapshot: Mutex::new(ResourceSnapshot {
                    used: ResourceCost::zero(),
                    limit,
                }),
                released: tokio::sync::Notify::new(),
            }),
        }
    }

    pub(crate) fn weak(&self) -> Weak<LedgerState> {
        Arc::downgrade(&self.state)
    }

    pub(crate) fn try_reserve(&self, cost: ResourceCost) -> Result<ResourceReservation> {
        let mut state = self
            .state
            .snapshot
            .lock()
            .map_err(|_| Error::Control("resource ledger lock poisoned"))?;
        let next = state
            .used
            .checked_add(cost)
            .ok_or(Error::Capacity("resource ledger arithmetic"))?;
        if !next.fits_within(state.limit) {
            return Err(Error::Capacity("resource ledger"));
        }
        state.used = next;
        Ok(ResourceReservation {
            ledger: self.clone(),
            cost,
        })
    }

    pub(crate) async fn reserve(&self, cost: ResourceCost) -> Result<ResourceReservation> {
        if !cost.fits_within(self.snapshot()?.limit) {
            return Err(Error::Capacity("resource ledger"));
        }
        loop {
            // Register before checking capacity so a release between the
            // failed attempt and await cannot leave the waiter asleep.
            let released = self.state.released.notified();
            match self.try_reserve(cost) {
                Ok(reservation) => return Ok(reservation),
                Err(Error::Capacity(_)) => released.await,
                Err(error) => return Err(error),
            }
        }
    }

    pub(crate) fn snapshot(&self) -> Result<ResourceSnapshot> {
        self.state
            .snapshot
            .lock()
            .map(|state| *state)
            .map_err(|_| Error::Control("resource ledger lock poisoned"))
    }

    pub(crate) fn set_retained_limit(&self, bytes: usize) -> Result<()> {
        let mut state = self
            .state
            .snapshot
            .lock()
            .map_err(|_| Error::Control("resource ledger lock poisoned"))?;
        if state.used.retained_bytes() > bytes {
            return Err(Error::Capacity("resource ledger retained bytes"));
        }
        state.limit = state.limit.with_retained_bytes(bytes);
        Ok(())
    }

    pub(crate) fn set_disk_limit(&self, bytes: u64) -> Result<()> {
        let mut state = self
            .state
            .snapshot
            .lock()
            .map_err(|_| Error::Control("resource ledger lock poisoned"))?;
        if state.used.disk_bytes() > bytes {
            return Err(Error::Capacity("resource ledger disk bytes"));
        }
        state.limit = state.limit.with_disk_bytes(bytes);
        Ok(())
    }

    pub(crate) fn set_host_limits(
        &self,
        io_slots: usize,
        blocking_jobs: usize,
        recovery_jobs: usize,
        dirty_jobs: usize,
        scratch_units: usize,
    ) -> Result<()> {
        let mut state = self
            .state
            .snapshot
            .lock()
            .map_err(|_| Error::Control("resource ledger lock poisoned"))?;
        let limit = state
            .limit
            .with_io_slots(io_slots)
            .with_blocking_jobs(blocking_jobs)
            .with_recovery_jobs(recovery_jobs)
            .with_dirty_jobs(dirty_jobs)
            .with_scratch_units(scratch_units);
        if !state.used.fits_within(limit) {
            return Err(Error::Capacity("resource ledger host limits"));
        }
        state.limit = limit;
        Ok(())
    }

    pub(crate) fn reconcile_disk(&self, bytes: u64) -> Result<()> {
        let mut state = self
            .state
            .snapshot
            .lock()
            .map_err(|_| Error::Control("resource ledger lock poisoned"))?;
        let used = state.used.with_disk_bytes(bytes);
        if !used.fits_within(state.limit) {
            return Err(Error::Capacity("resource ledger"));
        }
        state.used = used;
        Ok(())
    }

    fn release(&self, cost: ResourceCost) {
        if let Ok(mut state) = self.state.snapshot.lock()
            && let Some(used) = state.used.checked_sub(cost)
        {
            state.used = used;
        }
    }
}

/// RAII reservation that returns its exact cost on every exit path.
#[must_use = "dropping the reservation releases its capacity"]
pub(crate) struct ResourceReservation {
    ledger: ResourceLedger,
    cost: ResourceCost,
}

impl Drop for ResourceReservation {
    fn drop(&mut self) {
        self.ledger.release(self.cost);
        self.ledger.state.released.notify_waiters();
    }
}

/// Reports one host that outlived the runtime which owned its ledger.
///
/// The LTX layer consults these admissions on every disk and slot operation, so
/// a stale host would otherwise log per call; one line names the runtime
/// session that must be matched against the deployment's shut-down sessions.
fn warn_dropped_ledger(
    session: crate::identity::SessionId,
    warned: &std::sync::atomic::AtomicBool,
    operation: &'static str,
) {
    if !warned.swap(true, std::sync::atomic::Ordering::Relaxed) {
        tracing::warn!(
            session = %crate::identity::encode_hex(session.as_bytes()),
            operation,
            "runtime ledger closed; a host outlived the Cell runtime that owned it"
        );
    }
}

pub(crate) struct LedgerDiskAdmission {
    state: Weak<LedgerState>,
    session: crate::identity::SessionId,
    warned: std::sync::atomic::AtomicBool,
}

impl LedgerDiskAdmission {
    /// Binds one disk admission to the ledger of the runtime that installs it.
    pub(crate) fn new(session: crate::identity::SessionId, ledger: &ResourceLedger) -> Self {
        Self {
            state: ledger.weak(),
            session,
            warned: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

impl crab_ltx::DiskBudgetAdmission for LedgerDiskAdmission {
    fn reconcile(&self, bytes: u64) -> crab_ltx::Result<()> {
        let Some(state) = self.state.upgrade() else {
            warn_dropped_ledger(self.session, &self.warned, "disk reconcile");
            return Err(crab_ltx::CrabError::InvalidState("runtime ledger closed"));
        };
        ResourceLedger { state }
            .reconcile_disk(bytes)
            .map_err(|error| crab_ltx::CrabError::Other(Box::new(error)))?;
        Ok(())
    }

    fn is_live(&self) -> bool {
        self.state.strong_count() != 0
    }
}

pub(crate) struct LedgerHostResourceAdmission {
    state: Weak<LedgerState>,
    session: crate::identity::SessionId,
    warned: std::sync::atomic::AtomicBool,
}

impl LedgerHostResourceAdmission {
    /// Binds one host admission to the ledger of the runtime that installs it.
    pub(crate) fn new(session: crate::identity::SessionId, ledger: &ResourceLedger) -> Self {
        Self {
            state: ledger.weak(),
            session,
            warned: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

struct LedgerHostResourcePermit {
    _reservation: ResourceReservation,
}

impl crab_ltx::HostResourcePermit for LedgerHostResourcePermit {}

impl crab_ltx::HostResourceAdmission for LedgerHostResourceAdmission {
    fn reserve(
        &self,
        kind: crab_ltx::HostResourceKind,
        units: u32,
    ) -> crab_ltx::Result<Box<dyn crab_ltx::HostResourcePermit>> {
        let Some(state) = self.state.upgrade() else {
            warn_dropped_ledger(self.session, &self.warned, "host resource reserve");
            return Err(crab_ltx::CrabError::InvalidState("runtime ledger closed"));
        };
        let units = usize::try_from(units)
            .map_err(|_| crab_ltx::CrabError::Limit(crab_ltx::LimitKind::HostResourceUnits))?;
        let cost = match kind {
            crab_ltx::HostResourceKind::Io => ResourceCost::zero().with_io_slots(units),
            crab_ltx::HostResourceKind::BlockingJob => {
                ResourceCost::zero().with_blocking_jobs(units)
            }
            crab_ltx::HostResourceKind::Recovery => ResourceCost::zero().with_recovery_jobs(units),
            crab_ltx::HostResourceKind::Dirty => ResourceCost::zero().with_dirty_jobs(units),
            crab_ltx::HostResourceKind::Scratch => ResourceCost::zero().with_scratch_units(units),
        };
        let reservation = ResourceLedger { state }
            .try_reserve(cost)
            .map_err(|error| crab_ltx::CrabError::Other(Box::new(error)))?;
        Ok(Box::new(LedgerHostResourcePermit {
            _reservation: reservation,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crab_ltx::HostResourceAdmission;

    #[tokio::test]
    async fn waiting_reservation_reuses_released_capacity_without_overcommit() {
        let cost = ResourceCost::zero().with_primitive_jobs(1);
        let ledger = ResourceLedger::new(cost);
        let held = ledger.try_reserve(cost).unwrap();
        let pending = ledger.reserve(cost);
        tokio::pin!(pending);
        assert!(futures_util::poll!(&mut pending).is_pending());
        assert_eq!(ledger.snapshot().unwrap().used, cost);
        drop(held);
        let acquired = tokio::time::timeout(std::time::Duration::from_secs(1), pending)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(ledger.snapshot().unwrap().used, cost);
        drop(acquired);
        assert_eq!(ledger.snapshot().unwrap().used, ResourceCost::zero());
    }

    #[tokio::test]
    async fn cancelled_reservation_does_not_retain_capacity() {
        let cost = ResourceCost::zero().with_primitive_jobs(1);
        let ledger = ResourceLedger::new(cost);
        let held = ledger.try_reserve(cost).unwrap();
        {
            let pending = ledger.reserve(cost);
            tokio::pin!(pending);
            assert!(futures_util::poll!(&mut pending).is_pending());
        }
        drop(held);
        let acquired = ledger.try_reserve(cost).unwrap();
        assert_eq!(ledger.snapshot().unwrap().used, cost);
        drop(acquired);
    }

    #[test]
    fn reservations_are_bounded_and_return_to_baseline() {
        let ledger = ResourceLedger::new(
            ResourceCost::active_cell()
                .with_resident_bytes(ACTIVE_CELL_NATIVE_BYTES)
                .with_retained_bytes(64)
                .with_worker_jobs(1)
                .with_hydration_jobs(1),
        );
        let reservation = ledger
            .try_reserve(ResourceCost::active_cell().with_retained_bytes(32))
            .unwrap();
        assert!(ledger.try_reserve(ResourceCost::active_cell()).is_err());
        assert_eq!(ledger.snapshot().unwrap().used.active_cells(), 1);
        let job = ledger
            .try_reserve(ResourceCost::zero().with_worker_jobs(1))
            .unwrap();
        assert!(
            ledger
                .try_reserve(ResourceCost::zero().with_worker_jobs(1))
                .is_err()
        );
        let hydration = ledger
            .try_reserve(ResourceCost::zero().with_hydration_jobs(1))
            .unwrap();
        assert!(
            ledger
                .try_reserve(ResourceCost::zero().with_hydration_jobs(1))
                .is_err()
        );
        drop(hydration);
        drop(job);
        drop(reservation);
        assert_eq!(ledger.snapshot().unwrap().used, ResourceCost::zero());
    }

    #[test]
    fn checked_costs_reject_underflow_and_overflow() {
        assert!(
            ResourceCost::active_cell()
                .checked_sub(ResourceCost::active_cell().with_worker_jobs(1))
                .is_none()
        );
        assert!(
            ResourceCost {
                active_cells: usize::MAX,
                ..ResourceCost::zero()
            }
            .checked_add(ResourceCost::active_cell())
            .is_none()
        );
    }

    #[test]
    fn active_cell_cost_tracks_file_descriptors() {
        let cost = ResourceCost::active_cell();
        assert_eq!(cost.file_descriptors(), ACTIVE_CELL_FILE_DESCRIPTORS);
        assert_eq!(cost.with_file_descriptors(0).file_descriptors(), 0);
    }

    #[test]
    fn concurrent_reservations_release_to_the_same_baseline() {
        let ledger = ResourceLedger::new(
            ResourceCost::active_cell()
                .with_active_cells(4)
                .with_resident_bytes(4 * ACTIVE_CELL_NATIVE_BYTES)
                .with_file_descriptors(4 * ACTIVE_CELL_FILE_DESCRIPTORS),
        );
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(16));
        let successful = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        std::thread::scope(|scope| {
            for _ in 0..16 {
                let ledger = ledger.clone();
                let barrier = std::sync::Arc::clone(&barrier);
                let successful = std::sync::Arc::clone(&successful);
                scope.spawn(move || {
                    let reservation = ledger.try_reserve(ResourceCost::active_cell()).ok();
                    if reservation.is_some() {
                        successful.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    barrier.wait();
                    drop(reservation);
                });
            }
        });
        assert_eq!(successful.load(std::sync::atomic::Ordering::Relaxed), 4);
        assert_eq!(ledger.snapshot().unwrap().used, ResourceCost::zero());
    }

    #[test]
    fn ltx_disk_reservations_share_the_runtime_ledger() {
        let ledger = ResourceLedger::new(ResourceCost::zero().with_disk_bytes(10));
        let budget = crab_ltx::DiskBudget::new(20);
        let existing = budget.try_reserve(1).unwrap();
        budget
            .install_admission(Arc::new(LedgerDiskAdmission::new(
                crate::identity::SessionId::from_bytes([7; 16]),
                &ledger,
            )))
            .unwrap();
        assert_eq!(ledger.snapshot().unwrap().used.disk_bytes(), 1);
        drop(existing);
        assert_eq!(ledger.snapshot().unwrap().used.disk_bytes(), 0);

        let reservation = budget.try_reserve(4).unwrap();
        assert_eq!(budget.used(), 4);
        assert_eq!(ledger.snapshot().unwrap().used.disk_bytes(), 4);
        reservation.resize(7).unwrap();
        assert_eq!(ledger.snapshot().unwrap().used.disk_bytes(), 7);
        assert!(budget.try_reserve(4).is_err());
        assert_eq!(budget.used(), 7);
        assert_eq!(ledger.snapshot().unwrap().used.disk_bytes(), 7);
        assert!(reservation.try_grow(4).is_err());
        assert_eq!(reservation.bytes(), 7);
        assert!(reservation.resize(11).is_err());
        assert_eq!(reservation.bytes(), 7);
        reservation.resize(2).unwrap();
        assert_eq!(ledger.snapshot().unwrap().used.disk_bytes(), 2);
        drop(reservation);
        assert_eq!(budget.used(), 0);
        assert_eq!(ledger.snapshot().unwrap().used.disk_bytes(), 0);
    }

    #[test]
    fn ltx_host_admissions_share_one_runtime_ledger() {
        let ledger = ResourceLedger::new(
            ResourceCost::zero()
                .with_io_slots(1)
                .with_blocking_jobs(1)
                .with_recovery_jobs(1)
                .with_dirty_jobs(1)
                .with_scratch_units(2),
        );
        let admission = LedgerHostResourceAdmission::new(
            crate::identity::SessionId::from_bytes([8; 16]),
            &ledger,
        );
        let io = admission
            .reserve(crab_ltx::HostResourceKind::Io, 1)
            .unwrap();
        assert!(
            admission
                .reserve(crab_ltx::HostResourceKind::Io, 1)
                .is_err()
        );
        let scratch = admission
            .reserve(crab_ltx::HostResourceKind::Scratch, 2)
            .unwrap();
        assert!(
            admission
                .reserve(crab_ltx::HostResourceKind::Scratch, 1)
                .is_err()
        );
        assert_eq!(ledger.snapshot().unwrap().used.io_slots(), 1);
        assert_eq!(ledger.snapshot().unwrap().used.scratch_units(), 2);
        drop(scratch);
        drop(io);
        assert_eq!(ledger.snapshot().unwrap().used, ResourceCost::zero());
    }

    #[test]
    fn admissions_fail_closed_once_their_runtime_ledger_is_gone() {
        let session = crate::identity::SessionId::from_bytes([9; 16]);
        let ledger = ResourceLedger::new(ResourceCost::zero().with_disk_bytes(10).with_io_slots(1));
        let disk = LedgerDiskAdmission::new(session, &ledger);
        let host = LedgerHostResourceAdmission::new(session, &ledger);
        drop(ledger);

        let closed = match crab_ltx::DiskBudgetAdmission::reconcile(&disk, 1) {
            Ok(()) => panic!("a closed ledger reconciled disk bytes"),
            Err(error) => error,
        };
        assert!(
            closed.to_string().contains("runtime ledger closed"),
            "{closed}"
        );
        let closed = match host.reserve(crab_ltx::HostResourceKind::Io, 1) {
            Ok(_) => panic!("a closed ledger admitted host slots"),
            Err(error) => error,
        };
        assert!(
            closed.to_string().contains("runtime ledger closed"),
            "{closed}"
        );
    }
}
