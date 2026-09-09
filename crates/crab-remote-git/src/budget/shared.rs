use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::sync::{Mutex, watch};
use tokio_util::sync::CancellationToken;

use super::{BudgetDimension, OperationBudget};
use crate::{Error, Result};

type Rejection = watch::Receiver<Option<Arc<Error>>>;

struct Participant {
    charged: [u64; 14],
    budget: OperationBudget,
    cancellation: CancellationToken,
    rejection: watch::Sender<Option<Arc<Error>>>,
}

#[derive(Default)]
struct State {
    charged: [u64; 14],
    retired: bool,
    participants: HashMap<u64, Participant>,
}

pub(crate) struct SharedLease {
    budget: Option<Arc<SharedBudget>>,
}

impl SharedLease {
    pub(crate) fn release(&mut self) -> bool {
        let Some(budget) = self.budget.take() else {
            return false;
        };
        // Closing admission and removing the last waiter must be atomic: a new
        // caller must never join a producer whose cancellation is inevitable.
        let previous = budget
            .waiters
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                Some(if count == 1 { usize::MAX } else { count - 1 })
            });
        if previous == Ok(1) {
            budget.cancellation.cancel();
            return true;
        }
        false
    }
}

impl Drop for SharedLease {
    fn drop(&mut self) {
        self.release();
    }
}

pub(crate) struct SharedBudget {
    waiters: AtomicUsize,
    state: Mutex<State>,
    cancellation: CancellationToken,
}

impl SharedBudget {
    pub(crate) fn new(cancellation: CancellationToken) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State::default()),
            waiters: AtomicUsize::new(0),
            cancellation: cancellation.child_token(),
        })
    }

    pub(crate) fn cancellation(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    pub(crate) async fn register(
        self: &Arc<Self>,
        budget: &OperationBudget,
        cancellation: &CancellationToken,
    ) -> Option<(Rejection, SharedLease)> {
        self.waiters
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < usize::MAX - 1).then(|| count + 1)
            })
            .ok()?;
        let lease = SharedLease {
            budget: Some(self.clone()),
        };
        let mut state = self.state.lock().await;
        if state.retired {
            return None;
        }
        let charged = state.charged;
        let participant = state.participants.entry(budget.id()).or_insert_with(|| {
            let (rejection, _) = watch::channel(None);
            Participant {
                charged: [0; 14],
                budget: budget.clone(),
                cancellation: cancellation.clone(),
                rejection,
            }
        });
        let receiver = participant.rejection.subscribe();
        // A late waiter must reserve prior work before receiving the shared
        // result; joining after I/O started cannot bypass its own limits.
        if cancellation.is_cancelled() {
            participant
                .rejection
                .send_replace(Some(Arc::new(Error::Cancelled)));
        }
        for dimension in [
            BudgetDimension::StorageRequests,
            BudgetDimension::FetchedBytes,
            BudgetDimension::InflatedBytes,
        ] {
            if participant.rejection.borrow().is_some() {
                break;
            }
            let amount = charged[dimension as usize] - participant.charged[dimension as usize];
            if amount == 0 {
                continue;
            }
            if let Err(error) = participant.budget.charge(dimension, amount).await {
                participant.rejection.send_replace(Some(Arc::new(error)));
                break;
            }
            participant.charged[dimension as usize] = charged[dimension as usize];
        }
        Some((receiver, lease))
    }

    pub(crate) async fn charge(&self, dimension: BudgetDimension, amount: u64) -> Result<()> {
        let mut state = self.state.lock().await;
        let index = dimension as usize;
        let total = state.charged[index]
            .checked_add(amount)
            .ok_or(Error::LimitExceeded {
                limit: dimension.label(),
                actual: u64::MAX,
                maximum: u64::MAX,
            })?;
        // Reserve before awaiting participant locks so cancellation cannot leave
        // a participant's ledger ahead of the replay total for later joiners.
        state.charged[index] = total;
        let mut admitted = false;
        for participant in state.participants.values_mut() {
            if participant.rejection.is_closed() || participant.rejection.borrow().is_some() {
                continue;
            }
            let result = if participant.cancellation.is_cancelled() {
                Err(Error::Cancelled)
            } else {
                participant
                    .budget
                    .charge(dimension, total - participant.charged[index])
                    .await
            };
            match result {
                Ok(()) => {
                    participant.charged[index] = total;
                    admitted = true;
                }
                Err(error) => {
                    participant.rejection.send_replace(Some(Arc::new(error)));
                }
            }
        }
        if !admitted {
            state.retired = true;
            drop(state);
            // The producer has no remaining participant that can accept this
            // I/O, so provider retries must observe terminal cancellation.
            self.cancellation.cancel();
            return Err(Error::Cancelled);
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl crab_storage::ReadAdmission for SharedBudget {
    fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    async fn request(&self) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.charge(BudgetDimension::StorageRequests, 1).await?;
        Ok(())
    }

    async fn bytes(
        &self,
        bytes: u64,
    ) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
        self.charge(BudgetDimension::FetchedBytes, bytes).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{OperationLimits, RemoteGitRuntime};

    fn budget(maximum: u64) -> OperationBudget {
        OperationBudget::new(
            OperationLimits {
                max_inflated_bytes: maximum,
                ..OperationLimits::default()
            },
            Arc::new(RemoteGitRuntime::default()),
        )
    }

    #[tokio::test]
    async fn rejecting_one_participant_does_not_reject_other_budgets() {
        let shared = SharedBudget::new(CancellationToken::new());
        let cancellation = CancellationToken::new();
        let tight = budget(1);
        let generous = budget(10);
        let (tight_result, _tight_lease) = shared.register(&tight, &cancellation).await.unwrap();
        let (generous_result, _generous_lease) =
            shared.register(&generous, &cancellation).await.unwrap();
        shared
            .charge(BudgetDimension::InflatedBytes, 2)
            .await
            .unwrap();
        assert!(matches!(
            tight_result.borrow().as_deref(),
            Some(Error::LimitExceeded { maximum: 1, .. })
        ));
        assert!(generous_result.borrow().is_none());
        assert_eq!(
            generous
                .usage()
                .await
                .amount(BudgetDimension::InflatedBytes),
            2
        );
    }

    #[tokio::test]
    async fn late_participants_reserve_prior_work_once_per_operation() {
        let shared = SharedBudget::new(CancellationToken::new());
        let cancellation = CancellationToken::new();
        let first = budget(10);
        let _first = shared.register(&first, &cancellation).await.unwrap();
        shared
            .charge(BudgetDimension::InflatedBytes, 2)
            .await
            .unwrap();
        let late = budget(10);
        let _late = shared.register(&late, &cancellation).await.unwrap();
        let _same_operation = shared.register(&late, &cancellation).await.unwrap();
        shared
            .charge(BudgetDimension::InflatedBytes, 1)
            .await
            .unwrap();
        assert_eq!(late.usage().await.amount(BudgetDimension::InflatedBytes), 3);
        let tight = budget(1);
        let (rejected, _rejected_lease) = shared.register(&tight, &cancellation).await.unwrap();
        assert!(matches!(
            rejected.borrow().as_deref(),
            Some(Error::LimitExceeded { maximum: 1, .. })
        ));
    }

    #[tokio::test]
    async fn released_participants_stop_charging_without_cancelling_survivors() {
        let parent = CancellationToken::new();
        let shared = SharedBudget::new(parent.clone());
        let cancellation = CancellationToken::new();
        let first = budget(10);
        let survivor = budget(10);
        let first_registration = shared.register(&first, &cancellation).await.unwrap();
        let survivor_registration = shared.register(&survivor, &cancellation).await.unwrap();
        drop(first_registration);
        shared
            .charge(BudgetDimension::InflatedBytes, 2)
            .await
            .unwrap();
        assert_eq!(
            first.usage().await.amount(BudgetDimension::InflatedBytes),
            0
        );
        assert_eq!(
            survivor
                .usage()
                .await
                .amount(BudgetDimension::InflatedBytes),
            2
        );
        assert!(!shared.cancellation.is_cancelled());
        let rejoined = shared.register(&first, &cancellation).await.unwrap();
        assert_eq!(
            first.usage().await.amount(BudgetDimension::InflatedBytes),
            2
        );
        drop(rejoined);
        drop(survivor_registration);
        assert!(shared.cancellation.is_cancelled());
        assert!(!parent.is_cancelled());
        assert!(shared.register(&budget(10), &cancellation).await.is_none());
    }

    #[tokio::test]
    async fn interrupted_admission_preserves_rejoin_accounting() {
        let shared = SharedBudget::new(CancellationToken::new());
        let cancellation = CancellationToken::new();
        let first = budget(10);
        let second = budget(10);
        let _first = shared.register(&first, &cancellation).await.unwrap();
        let _second = shared.register(&second, &cancellation).await.unwrap();
        let (charged, blocked) = {
            let state = shared.state.lock().await;
            let mut participants = state.participants.values();
            (
                participants.next().unwrap().budget.clone(),
                participants.next().unwrap().budget.clone(),
            )
        };
        let held = blocked.work.lock().await;
        let mut admission = Box::pin(shared.charge(BudgetDimension::InflatedBytes, 2));
        tokio::select! {
            result = &mut admission => panic!("admission passed blocked participant: {result:?}"),
            () = async {
                while charged.usage().await.amount(BudgetDimension::InflatedBytes) != 2 {
                    tokio::task::yield_now().await;
                }
            } => {},
        }
        drop(admission);
        drop(held);
        let _rejoined = shared.register(&charged, &cancellation).await.unwrap();
        shared
            .charge(BudgetDimension::InflatedBytes, 1)
            .await
            .unwrap();
        assert_eq!(
            (
                charged.usage().await.amount(BudgetDimension::InflatedBytes),
                blocked.usage().await.amount(BudgetDimension::InflatedBytes)
            ),
            (3, 3)
        );
    }

    #[tokio::test]
    async fn exhausted_producer_cannot_accept_a_fresh_participant() {
        let shared = SharedBudget::new(CancellationToken::new());
        let cancellation = CancellationToken::new();
        let tight = budget(1);
        let _tight = shared.register(&tight, &cancellation).await.unwrap();
        assert!(
            shared
                .charge(BudgetDimension::InflatedBytes, 2)
                .await
                .is_err()
        );
        assert!(shared.cancellation.is_cancelled());
        assert!(shared.register(&budget(10), &cancellation).await.is_none());
    }
}
