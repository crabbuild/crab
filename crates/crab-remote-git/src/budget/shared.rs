use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::sync::{Mutex, watch};
use tokio_util::sync::CancellationToken;

use super::{BudgetDimension, BudgetUsage, OperationBudget};
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
            .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
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
    maximum_participants: usize,
}

impl SharedBudget {
    pub(crate) fn waiters(&self) -> usize {
        let count = self.waiters.load(Ordering::Acquire);
        if count == usize::MAX { 0 } else { count }
    }

    pub(crate) fn new(cancellation: CancellationToken, maximum_participants: usize) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(State::default()),
            waiters: AtomicUsize::new(0),
            cancellation: cancellation.child_token(),
            maximum_participants,
        })
    }

    pub(crate) fn cancellation(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    pub(crate) async fn register(
        self: &Arc<Self>,
        budget: &OperationBudget,
        cancellation: &CancellationToken,
    ) -> Result<Option<(Rejection, SharedLease)>> {
        match self
            .waiters
            .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < self.maximum_participants).then(|| count + 1)
            }) {
            Ok(_) => {}
            Err(usize::MAX) => return Ok(None),
            Err(count) => {
                return Err(Error::LimitExceeded {
                    limit: "shared read participants",
                    actual: count.saturating_add(1) as u64,
                    maximum: self.maximum_participants as u64,
                });
            }
        }
        let lease = SharedLease {
            budget: Some(self.clone()),
        };
        let mut state = self.state.lock().await;
        if state.retired {
            return Ok(None);
        }
        // Keep departed ledgers until the flight ends: rejoining the same
        // operation must not replay already charged work. Bound tombstones too.
        if !state.participants.contains_key(&budget.id())
            && state.participants.len() >= self.maximum_participants
        {
            return Err(Error::LimitExceeded {
                limit: "shared read participants",
                actual: state.participants.len().saturating_add(1) as u64,
                maximum: self.maximum_participants as u64,
            });
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
        for dimension in BudgetDimension::ALL {
            if participant.rejection.borrow().is_some() {
                break;
            }
            let total = charged[dimension as usize];
            let prior = participant.charged[dimension as usize];
            if total == prior {
                continue;
            }
            let amount = if dimension == BudgetDimension::Depth {
                total
            } else {
                total - prior
            };
            if let Err(error) = participant.budget.charge(dimension, amount).await {
                participant.rejection.send_replace(Some(Arc::new(error)));
                break;
            }
            participant.charged[dimension as usize] = charged[dimension as usize];
        }
        Ok(Some((receiver, lease)))
    }

    pub(crate) async fn charge(&self, dimension: BudgetDimension, amount: u64) -> Result<()> {
        let mut state = self.state.lock().await;
        let index = dimension as usize;
        let total = if dimension == BudgetDimension::Depth {
            Some(state.charged[index].max(amount))
        } else {
            state.charged[index].checked_add(amount)
        }
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
                let amount = if dimension == BudgetDimension::Depth {
                    total
                } else {
                    total - participant.charged[index]
                };
                participant.budget.charge(dimension, amount).await
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

    pub(crate) async fn usage(&self) -> BudgetUsage {
        BudgetUsage {
            used: self.state.lock().await.charged,
        }
    }

    pub(crate) async fn remaining(&self, dimension: BudgetDimension) -> u64 {
        let state = self.state.lock().await;
        let mut remaining = 0;
        for participant in state.participants.values() {
            if participant.cancellation.is_cancelled()
                || participant.rejection.is_closed()
                || participant.rejection.borrow().is_some()
            {
                continue;
            }
            remaining = remaining.max(participant.budget.remaining(dimension).await);
        }
        remaining
    }
}

#[async_trait::async_trait]
impl crab_storage::ReadAdmission for SharedBudget {
    fn cancellation(&self) -> &CancellationToken {
        &self.cancellation
    }

    async fn received(&self, bytes: u64) {
        let owner = {
            let state = self.state.lock().await;
            // Budgets charge every participant, but a physical response is
            // observed once. Keep the earliest live owner; never replay old
            // traffic to late joiners or lose a frame after the last cancel.
            state
                .participants
                .values()
                .min_by_key(|participant| {
                    let retired = participant.cancellation.is_cancelled()
                        || participant.rejection.is_closed()
                        || participant.rejection.borrow().is_some();
                    (retired, participant.budget.id())
                })
                .map(|participant| participant.budget.clone())
        };
        if let Some(owner) = owner {
            owner.received(bytes).await;
        }
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

    #[tokio::test]
    async fn coalesced_origin_receipts_have_one_live_owner_without_replaying_to_late_joiners() {
        let shared = SharedBudget::new(CancellationToken::new(), 256);
        let (first, _) = crab_storage::ReadProgress::channel();
        let (second, _) = crab_storage::ReadProgress::channel();
        let first_budget = budget(100).with_read_progress(first.clone());
        let second_budget = budget(100).with_read_progress(second.clone());
        let first_cancel = CancellationToken::new();
        let second_cancel = CancellationToken::new();
        let first_lease = shared
            .register(&first_budget, &first_cancel)
            .await
            .unwrap()
            .unwrap();
        crab_storage::ReadAdmission::received(shared.as_ref(), 5).await;
        let second_lease = shared
            .register(&second_budget, &second_cancel)
            .await
            .unwrap()
            .unwrap();
        assert_eq!((first.received_bytes(), second.received_bytes()), (5, 0));
        first_cancel.cancel();
        crab_storage::ReadAdmission::received(shared.as_ref(), 7).await;
        assert_eq!((first.received_bytes(), second.received_bytes()), (5, 7));
        drop((first_lease, second_lease));
    }

    fn budget(maximum: u64) -> OperationBudget {
        OperationBudget::new(
            OperationLimits {
                max_inflated_bytes: maximum,
                ..OperationLimits::default()
            },
            Arc::new(RemoteGitRuntime::default()),
        )
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn departed_ledgers_are_bounded_without_double_charging_rejoins() {
        let shared = SharedBudget::new(CancellationToken::new(), 2);
        let cancellation = CancellationToken::new();
        let first = budget(100);
        let first_registration = shared
            .register(&first, &cancellation)
            .await
            .unwrap()
            .unwrap();
        shared
            .charge(BudgetDimension::InflatedBytes, 2)
            .await
            .unwrap();
        let survivor = budget(100);
        let _survivor = shared
            .register(&survivor, &cancellation)
            .await
            .unwrap()
            .unwrap();
        drop(first_registration);
        shared
            .charge(BudgetDimension::InflatedBytes, 3)
            .await
            .unwrap();
        assert!(matches!(
            shared.register(&budget(100), &cancellation).await,
            Err(Error::LimitExceeded {
                limit: "shared read participants",
                maximum: 2,
                ..
            })
        ));
        let _rejoined = shared
            .register(&first, &cancellation)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            first.usage().await.amount(BudgetDimension::InflatedBytes),
            5
        );
        assert!(!shared.cancellation.is_cancelled());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn late_waiters_replay_every_work_dimension_and_depth_is_a_maximum() {
        let shared = SharedBudget::new(CancellationToken::new(), 256);
        let cancellation = CancellationToken::new();
        let first = budget(100);
        let _first = shared
            .register(&first, &cancellation)
            .await
            .unwrap()
            .unwrap();
        for dimension in BudgetDimension::ALL {
            shared.charge(dimension, 4).await.unwrap();
        }
        shared.charge(BudgetDimension::Depth, 2).await.unwrap();
        let late = budget(100);
        let late_registration = shared
            .register(&late, &cancellation)
            .await
            .unwrap()
            .unwrap();
        for dimension in BudgetDimension::ALL {
            assert_eq!(late.usage().await.amount(dimension), 4, "{dimension:?}");
        }
        drop(late_registration);
        shared
            .charge(BudgetDimension::HistoryCommits, 1)
            .await
            .unwrap();
        let _rejoined = shared
            .register(&late, &cancellation)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            late.usage().await.amount(BudgetDimension::HistoryCommits),
            5
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn shared_producer_follows_live_budgets_without_a_default_semantic_ceiling() {
        let shared = SharedBudget::new(CancellationToken::new(), 256);
        let cancellation = CancellationToken::new();
        let runtime = Arc::new(RemoteGitRuntime::default());
        let tight = OperationBudget::new(
            OperationLimits {
                max_history_commits: 1,
                ..Default::default()
            },
            runtime.clone(),
        );
        let (rejected, _tight) = shared
            .register(&tight, &cancellation)
            .await
            .unwrap()
            .unwrap();
        let producer = OperationBudget::shared(shared.clone(), runtime.clone());
        producer
            .charge(BudgetDimension::HistoryCommits, 1)
            .await
            .unwrap();
        let generous = OperationBudget::new(
            OperationLimits {
                max_history_commits: 20_000,
                ..Default::default()
            },
            runtime,
        );
        let _generous = shared
            .register(&generous, &cancellation)
            .await
            .unwrap()
            .unwrap();
        producer
            .charge(BudgetDimension::HistoryCommits, 15_000)
            .await
            .unwrap();
        assert!(matches!(
            rejected.borrow().as_deref(),
            Some(Error::LimitExceeded { maximum: 1, .. })
        ));
        assert_eq!(
            generous
                .usage()
                .await
                .amount(BudgetDimension::HistoryCommits),
            15_001
        );
        assert_eq!(
            producer.remaining(BudgetDimension::HistoryCommits).await,
            4_999
        );
        assert_eq!(
            producer
                .usage()
                .await
                .amount(BudgetDimension::HistoryCommits),
            15_001
        );
    }

    #[tokio::test]
    async fn rejecting_one_participant_does_not_reject_other_budgets() {
        let shared = SharedBudget::new(CancellationToken::new(), 256);
        let cancellation = CancellationToken::new();
        let tight = budget(1);
        let generous = budget(10);
        let (tight_result, _tight_lease) = shared
            .register(&tight, &cancellation)
            .await
            .unwrap()
            .unwrap();
        let (generous_result, _generous_lease) = shared
            .register(&generous, &cancellation)
            .await
            .unwrap()
            .unwrap();
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
        let shared = SharedBudget::new(CancellationToken::new(), 256);
        let cancellation = CancellationToken::new();
        let first = budget(10);
        let _first = shared
            .register(&first, &cancellation)
            .await
            .unwrap()
            .unwrap();
        shared
            .charge(BudgetDimension::InflatedBytes, 2)
            .await
            .unwrap();
        let late = budget(10);
        let _late = shared
            .register(&late, &cancellation)
            .await
            .unwrap()
            .unwrap();
        let _same_operation = shared
            .register(&late, &cancellation)
            .await
            .unwrap()
            .unwrap();
        shared
            .charge(BudgetDimension::InflatedBytes, 1)
            .await
            .unwrap();
        assert_eq!(late.usage().await.amount(BudgetDimension::InflatedBytes), 3);
        let tight = budget(1);
        let (rejected, _rejected_lease) = shared
            .register(&tight, &cancellation)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            rejected.borrow().as_deref(),
            Some(Error::LimitExceeded { maximum: 1, .. })
        ));
    }

    #[tokio::test]
    async fn released_participants_stop_charging_without_cancelling_survivors() {
        let parent = CancellationToken::new();
        let shared = SharedBudget::new(parent.clone(), 256);
        let cancellation = CancellationToken::new();
        let first = budget(10);
        let survivor = budget(10);
        let first_registration = shared
            .register(&first, &cancellation)
            .await
            .unwrap()
            .unwrap();
        let survivor_registration = shared
            .register(&survivor, &cancellation)
            .await
            .unwrap()
            .unwrap();
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
        let rejoined = shared
            .register(&first, &cancellation)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            first.usage().await.amount(BudgetDimension::InflatedBytes),
            2
        );
        drop(rejoined);
        drop(survivor_registration);
        assert!(shared.cancellation.is_cancelled());
        assert!(!parent.is_cancelled());
        assert!(
            shared
                .register(&budget(10), &cancellation)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn interrupted_admission_preserves_rejoin_accounting() {
        let shared = SharedBudget::new(CancellationToken::new(), 256);
        let cancellation = CancellationToken::new();
        let first = budget(10);
        let second = budget(10);
        let _first = shared
            .register(&first, &cancellation)
            .await
            .unwrap()
            .unwrap();
        let _second = shared
            .register(&second, &cancellation)
            .await
            .unwrap()
            .unwrap();
        let (charged, blocked) = {
            let state = shared.state.lock().await;
            let mut participants = state.participants.values();
            (
                participants.next().unwrap().budget.clone(),
                participants.next().unwrap().budget.clone(),
            )
        };
        let super::super::BudgetOwner::Operation(work) = &blocked.owner else {
            panic!("ordinary participant budget");
        };
        let held = work.lock().await;
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
        let _rejoined = shared
            .register(&charged, &cancellation)
            .await
            .unwrap()
            .unwrap();
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
        let shared = SharedBudget::new(CancellationToken::new(), 256);
        let cancellation = CancellationToken::new();
        let tight = budget(1);
        let _tight = shared
            .register(&tight, &cancellation)
            .await
            .unwrap()
            .unwrap();
        assert!(
            shared
                .charge(BudgetDimension::InflatedBytes, 2)
                .await
                .is_err()
        );
        assert!(shared.cancellation.is_cancelled());
        assert!(
            shared
                .register(&budget(10), &cancellation)
                .await
                .unwrap()
                .is_none()
        );
    }
}
