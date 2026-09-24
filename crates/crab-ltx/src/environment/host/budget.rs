//! Byte-precise local disk admission for active database work.
//!
//! A budget owns the capacity an embedding runtime granted it, charges the
//! exact bytes each reservation holds, and reconciles every change with the
//! registered admissions so a node ledger never drifts from the files.

use super::*;

type DiskAdmissions = Vec<Arc<dyn DiskBudgetAdmission>>;

/// Shared byte-precise admission for local files owned by active database work.
#[derive(Clone)]
pub struct DiskBudget {
    inner: Arc<DiskBudgetInner>,
}

pub(crate) struct DiskBudgetInner {
    capacity: u64,
    used: AtomicU64,
    has_admissions: AtomicBool,
    admissions: Mutex<DiskAdmissions>,
}

impl fmt::Debug for DiskBudget {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DiskBudget")
            .field("capacity", &self.capacity())
            .field("used", &self.used())
            .finish_non_exhaustive()
    }
}

impl DiskBudget {
    /// Creates a budget. A zero capacity rejects every non-empty reservation.
    #[must_use]
    pub fn new(capacity: u64) -> Self {
        Self {
            inner: Arc::new(DiskBudgetInner {
                capacity,
                used: AtomicU64::new(0),
                has_admissions: AtomicBool::new(false),
                admissions: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Installs one embedding ledger and immediately reconciles existing bytes.
    ///
    /// All clones of this budget observe the same hook. Multiple live runtimes
    /// may observe the same process-wide budget; dead hooks are removed before
    /// the new hook is registered.
    pub fn install_admission(&self, admission: Arc<dyn DiskBudgetAdmission>) -> crate::Result<()> {
        let mut current = self
            .inner
            .admissions
            .lock()
            .map_err(|_| crate::CrabError::InvalidState("disk admission lock poisoned"))?;
        current.retain(|admission| admission.is_live());
        let had_admissions = !current.is_empty();
        self.inner.has_admissions.store(true, Ordering::Release);
        current.push(admission);
        let result = current.last().map_or_else(
            || {
                Err(crate::CrabError::InvalidState(
                    "disk admission was not installed",
                ))
            },
            |admission| admission.reconcile(self.used()),
        );
        if let Err(error) = result {
            current.pop();
            self.inner
                .has_admissions
                .store(had_admissions, Ordering::Release);
            return Err(error);
        }
        Ok(())
    }

    /// Reserves bytes without waiting or overcommitting the configured capacity.
    pub fn try_reserve(&self, bytes: u64) -> crate::Result<DiskReservation> {
        self.add(bytes)?;
        if let Err(error) = self.reconcile_admissions(self.used()) {
            let _ = self.remove(bytes);
            let _ = self.reconcile_admissions(self.used());
            return Err(error);
        }
        Ok(DiskReservation {
            budget: self.clone(),
            bytes: Mutex::new(bytes),
        })
    }

    #[must_use]
    pub fn capacity(&self) -> u64 {
        self.inner.capacity
    }

    #[must_use]
    pub fn used(&self) -> u64 {
        self.inner.used.load(Ordering::Acquire)
    }

    #[must_use]
    pub fn available(&self) -> u64 {
        self.capacity().saturating_sub(self.used())
    }

    fn add(&self, bytes: u64) -> crate::Result<()> {
        self.inner
            .used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(bytes)
                    .filter(|next| *next <= self.inner.capacity)
            })
            .map(|_| ())
            .map_err(|_| crate::CrabError::Limit(crate::LimitKind::LocalDiskBytes))
    }

    fn reconcile_admissions(&self, bytes: u64) -> crate::Result<()> {
        let Some(mut admissions) = self.live_admissions()? else {
            return Ok(());
        };
        Self::reconcile_admissions_locked(&mut admissions, bytes)
    }

    fn live_admissions(&self) -> crate::Result<Option<MutexGuard<'_, DiskAdmissions>>> {
        if !self.inner.has_admissions.load(Ordering::Acquire) {
            return Ok(None);
        }
        let mut admissions = self
            .inner
            .admissions
            .lock()
            .map_err(|_| crate::CrabError::InvalidState("disk admission lock poisoned"))?;
        admissions.retain(|admission| admission.is_live());
        if admissions.is_empty() {
            self.inner.has_admissions.store(false, Ordering::Release);
            return Ok(None);
        }
        Ok(Some(admissions))
    }

    fn reconcile_admissions_locked(
        admissions: &mut DiskAdmissions,
        bytes: u64,
    ) -> crate::Result<()> {
        let mut index = 0;
        while index < admissions.len() {
            match admissions[index].reconcile(bytes) {
                Ok(()) => index += 1,
                // The owner can drop between the liveness prune and this call.
                // Its reservations died with it, and a closed hook must never
                // fail a reservation that the live runtimes still own.
                Err(_) if !admissions[index].is_live() => {
                    admissions.remove(index);
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    fn remove(&self, bytes: u64) -> crate::Result<()> {
        self.inner
            .used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_sub(bytes)
            })
            .map(|_| ())
            .map_err(|_| crate::CrabError::InvalidState("local disk reservation underflow"))
    }
}

/// Owned local-disk admission released when its owner drops it.
pub struct DiskReservation {
    budget: DiskBudget,
    bytes: Mutex<u64>,
}

impl fmt::Debug for DiskReservation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DiskReservation")
            .field("bytes", &self.bytes())
            .finish_non_exhaustive()
    }
}

impl DiskReservation {
    /// Adds bytes to this reservation without exceeding the shared budget.
    pub fn try_grow(&self, bytes: u64) -> crate::Result<()> {
        let mut held = match self.bytes.lock() {
            Ok(held) => held,
            Err(poisoned) => poisoned.into_inner(),
        };
        let next = held
            .checked_add(bytes)
            .ok_or(crate::CrabError::Limit(crate::LimitKind::LocalDiskBytes))?;
        self.budget.add(bytes)?;
        if let Err(error) = self.budget.reconcile_admissions(self.budget.used()) {
            let _ = self.budget.remove(bytes);
            let _ = self.budget.reconcile_admissions(self.budget.used());
            return Err(error);
        }
        *held = next;
        Ok(())
    }

    /// Changes the exact held byte count, releasing capacity when it shrinks.
    pub fn resize(&self, bytes: u64) -> crate::Result<()> {
        let mut held = match self.bytes.lock() {
            Ok(held) => held,
            Err(poisoned) => poisoned.into_inner(),
        };
        let current = *held;
        if bytes > current {
            let added = bytes - current;
            self.budget.add(added)?;
            if let Err(error) = self.budget.reconcile_admissions(self.budget.used()) {
                let _ = self.budget.remove(added);
                let _ = self.budget.reconcile_admissions(self.budget.used());
                return Err(error);
            }
            *held = bytes;
            return Ok(());
        }
        let released = current - bytes;
        *held = bytes;
        if let Err(error) = self.budget.remove(released) {
            *held = current;
            return Err(error);
        }
        if let Err(error) = self.budget.reconcile_admissions(self.budget.used()) {
            self.budget.add(released)?;
            *held = current;
            let _ = self.budget.reconcile_admissions(self.budget.used());
            return Err(error);
        }
        Ok(())
    }

    fn release(&self) {
        let mut held = match self.bytes.lock() {
            Ok(held) => held,
            Err(poisoned) => poisoned.into_inner(),
        };
        let released = *held;
        *held = 0;
        let _ = self.budget.remove(released);
        let _ = self.budget.reconcile_admissions(self.budget.used());
    }

    #[must_use]
    pub fn bytes(&self) -> u64 {
        match self.bytes.lock() {
            Ok(held) => *held,
            Err(poisoned) => *poisoned.into_inner(),
        }
    }
}

impl Drop for DiskReservation {
    fn drop(&mut self) {
        self.release();
    }
}
