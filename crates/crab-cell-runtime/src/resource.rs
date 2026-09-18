use std::sync::{Arc, Mutex};

use crate::{Error, Result};

pub(crate) const ACTIVE_CELL_NATIVE_BYTES: usize = 64 * 1024;
pub(crate) const HYDRATION_JOB_CAPACITY: usize = 2;

/// Bounded resources owned by one runtime admission token.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ResourceCost {
    active_cells: usize,
    resident_bytes: usize,
    retained_bytes: usize,
    disk_bytes: u64,
    worker_jobs: usize,
    primitive_jobs: usize,
    hydration_jobs: usize,
}

impl ResourceCost {
    #[must_use]
    pub const fn active_cell() -> Self {
        Self {
            active_cells: 1,
            resident_bytes: ACTIVE_CELL_NATIVE_BYTES,
            ..Self::zero()
        }
    }

    #[must_use]
    pub const fn zero() -> Self {
        Self {
            active_cells: 0,
            resident_bytes: 0,
            retained_bytes: 0,
            disk_bytes: 0,
            worker_jobs: 0,
            primitive_jobs: 0,
            hydration_jobs: 0,
        }
    }

    #[must_use]
    pub const fn active_cells(self) -> usize {
        self.active_cells
    }

    #[must_use]
    pub const fn resident_bytes(self) -> usize {
        self.resident_bytes
    }

    #[must_use]
    pub const fn retained_bytes(self) -> usize {
        self.retained_bytes
    }

    #[must_use]
    pub const fn disk_bytes(self) -> u64 {
        self.disk_bytes
    }

    #[must_use]
    pub const fn worker_jobs(self) -> usize {
        self.worker_jobs
    }

    #[must_use]
    pub const fn primitive_jobs(self) -> usize {
        self.primitive_jobs
    }

    #[must_use]
    pub const fn hydration_jobs(self) -> usize {
        self.hydration_jobs
    }

    #[must_use]
    pub const fn with_retained_bytes(mut self, bytes: usize) -> Self {
        self.retained_bytes = bytes;
        self
    }

    #[must_use]
    pub const fn with_resident_bytes(mut self, bytes: usize) -> Self {
        self.resident_bytes = bytes;
        self
    }

    #[must_use]
    pub const fn with_active_cells(mut self, cells: usize) -> Self {
        self.active_cells = cells;
        self
    }

    #[must_use]
    pub const fn with_disk_bytes(mut self, bytes: u64) -> Self {
        self.disk_bytes = bytes;
        self
    }

    #[must_use]
    pub const fn with_worker_jobs(mut self, jobs: usize) -> Self {
        self.worker_jobs = jobs;
        self
    }

    #[must_use]
    pub const fn with_primitive_jobs(mut self, jobs: usize) -> Self {
        self.primitive_jobs = jobs;
        self
    }

    #[must_use]
    pub const fn with_hydration_jobs(mut self, jobs: usize) -> Self {
        self.hydration_jobs = jobs;
        self
    }

    fn checked_add(self, other: Self) -> Option<Self> {
        Some(Self {
            active_cells: self.active_cells.checked_add(other.active_cells)?,
            resident_bytes: self.resident_bytes.checked_add(other.resident_bytes)?,
            retained_bytes: self.retained_bytes.checked_add(other.retained_bytes)?,
            disk_bytes: self.disk_bytes.checked_add(other.disk_bytes)?,
            worker_jobs: self.worker_jobs.checked_add(other.worker_jobs)?,
            primitive_jobs: self.primitive_jobs.checked_add(other.primitive_jobs)?,
            hydration_jobs: self.hydration_jobs.checked_add(other.hydration_jobs)?,
        })
    }

    fn checked_sub(self, other: Self) -> Option<Self> {
        Some(Self {
            active_cells: self.active_cells.checked_sub(other.active_cells)?,
            resident_bytes: self.resident_bytes.checked_sub(other.resident_bytes)?,
            retained_bytes: self.retained_bytes.checked_sub(other.retained_bytes)?,
            disk_bytes: self.disk_bytes.checked_sub(other.disk_bytes)?,
            worker_jobs: self.worker_jobs.checked_sub(other.worker_jobs)?,
            primitive_jobs: self.primitive_jobs.checked_sub(other.primitive_jobs)?,
            hydration_jobs: self.hydration_jobs.checked_sub(other.hydration_jobs)?,
        })
    }

    fn fits_within(self, limit: Self) -> bool {
        self.active_cells <= limit.active_cells
            && self.resident_bytes <= limit.resident_bytes
            && self.retained_bytes <= limit.retained_bytes
            && self.disk_bytes <= limit.disk_bytes
            && self.worker_jobs <= limit.worker_jobs
            && self.primitive_jobs <= limit.primitive_jobs
            && self.hydration_jobs <= limit.hydration_jobs
    }
}

/// Point-in-time usage and limits for one resource ledger.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResourceSnapshot {
    pub used: ResourceCost,
    pub limit: ResourceCost,
}

#[derive(Clone)]
pub(crate) struct ResourceLedger {
    // Reservations are short synchronous critical sections; no ledger lock is
    // held while a worker, filesystem, or provider operation runs.
    state: Arc<Mutex<ResourceSnapshot>>,
}

impl ResourceLedger {
    pub(crate) fn new(limit: ResourceCost) -> Self {
        Self {
            state: Arc::new(Mutex::new(ResourceSnapshot {
                used: ResourceCost::zero(),
                limit,
            })),
        }
    }

    pub(crate) fn try_reserve(&self, cost: ResourceCost) -> Result<ResourceReservation> {
        let mut state = self
            .state
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

    pub(crate) fn snapshot(&self) -> Result<ResourceSnapshot> {
        self.state
            .lock()
            .map(|state| *state)
            .map_err(|_| Error::Control("resource ledger lock poisoned"))
    }

    pub(crate) fn set_retained_limit(&self, bytes: usize) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| Error::Control("resource ledger lock poisoned"))?;
        if state.used.retained_bytes() > bytes {
            return Err(Error::Capacity("resource ledger retained bytes"));
        }
        state.limit = state.limit.with_retained_bytes(bytes);
        Ok(())
    }

    fn release(&self, cost: ResourceCost) {
        if let Ok(mut state) = self.state.lock()
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn concurrent_reservations_release_to_the_same_baseline() {
        let ledger = ResourceLedger::new(ResourceCost::active_cell().with_active_cells(4));
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(16));
        std::thread::scope(|scope| {
            for _ in 0..16 {
                let ledger = ledger.clone();
                let barrier = std::sync::Arc::clone(&barrier);
                scope.spawn(move || {
                    let reservation = ledger.try_reserve(ResourceCost::active_cell()).ok();
                    barrier.wait();
                    drop(reservation);
                });
            }
        });
        assert_eq!(ledger.snapshot().unwrap().used, ResourceCost::zero());
    }
}
