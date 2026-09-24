//! Disk budget admission tests.

use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use crab_ltx::{CrabError, DiskBudget, DiskBudgetAdmission};

/// A hook whose install-time reconcile succeeds and whose later reconciles fail.
///
/// `dies_on_failure` models the owner dropping between the budget's liveness
/// prune and the reconcile call itself; without it the owner stays live and its
/// rejection must keep binding the budget.
struct ScriptedAdmission {
    reconciles: AtomicUsize,
    dies_on_failure: bool,
    live: AtomicBool,
}

impl ScriptedAdmission {
    fn new(dies_on_failure: bool) -> Self {
        Self {
            reconciles: AtomicUsize::new(0),
            dies_on_failure,
            live: AtomicBool::new(true),
        }
    }
}

impl DiskBudgetAdmission for ScriptedAdmission {
    fn reconcile(&self, _bytes: u64) -> crab_ltx::Result<()> {
        if self.reconciles.fetch_add(1, Ordering::AcqRel) == 0 {
            return Ok(());
        }
        if self.dies_on_failure {
            self.live.store(false, Ordering::Release);
            return Err(CrabError::InvalidState("runtime ledger closed"));
        }
        Err(CrabError::InvalidState("live ledger rejects bytes"))
    }

    fn is_live(&self) -> bool {
        self.live.load(Ordering::Acquire)
    }
}

#[test]
fn hook_that_dies_during_reconcile_does_not_fail_a_live_reservation() {
    let budget = DiskBudget::new(1 << 20);
    budget
        .install_admission(Arc::new(ScriptedAdmission::new(true)))
        .expect("a live hook installs");

    let reservation = budget
        .try_reserve(64)
        .expect("a hook whose owner is gone stops constraining the budget");
    assert_eq!(budget.used(), 64);
    drop(reservation);
    assert_eq!(budget.used(), 0);
    assert!(budget.try_reserve(64).is_ok());
}

#[test]
fn live_hook_rejection_still_fails_the_reservation() {
    let budget = DiskBudget::new(1 << 20);
    budget
        .install_admission(Arc::new(ScriptedAdmission::new(false)))
        .expect("a live hook installs");

    assert!(budget.try_reserve(64).is_err());
    assert_eq!(budget.used(), 0);
}
