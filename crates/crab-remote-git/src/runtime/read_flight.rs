use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::sync::Arc;

use tokio::sync::{Mutex, Semaphore, watch};
use tokio_util::sync::CancellationToken;

use super::RemoteGitRuntime;
use crate::budget::{OperationBudget, SharedBudget};
use crate::{Error, Result};

#[derive(Clone)]
struct Flight<T: Clone> {
    receiver: watch::Receiver<Option<std::result::Result<T, Arc<Error>>>>,
    budget: Arc<SharedBudget>,
}

pub(super) struct ReadFlights<K, T: Clone> {
    flights: Mutex<HashMap<K, Flight<T>>>,
}

impl<K, T> ReadFlights<K, T>
where
    K: Clone + Eq + Hash + Send + Sync + 'static,
    T: Clone + Send + Sync + 'static,
{
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            flights: Mutex::new(HashMap::new()),
        })
    }

    pub(super) async fn len(&self) -> usize {
        self.flights.lock().await.len()
    }

    pub(super) async fn run<F, Fut>(
        self: &Arc<Self>,
        runtime: &Arc<RemoteGitRuntime>,
        key: K,
        cancellation: &CancellationToken,
        budget: &OperationBudget,
        admission: &Arc<Semaphore>,
        work: F,
    ) -> Result<T>
    where
        F: FnOnce(CancellationToken, Arc<SharedBudget>) -> Fut + Send + 'static,
        Fut: Future<Output = Result<T>> + Send + 'static,
    {
        let mut work = Some(work);
        let (flight, mut rejection, mut lease) = loop {
            let existing = self.flights.lock().await.get(&key).cloned();
            let (flight, registered) = if let Some(flight) = existing {
                (flight, None)
            } else {
                let permit = tokio::select! {
                    biased;
                    () = cancellation.cancelled() => return Err(Error::Cancelled),
                    permit = Arc::clone(admission).acquire_owned() => permit.map_err(|_| Error::Cancelled)?,
                };
                let mut flights = self.flights.lock().await;
                if let Some(flight) = flights.get(&key) {
                    drop(permit);
                    (flight.clone(), None)
                } else {
                    let (sender, receiver) = watch::channel(None);
                    let shared_budget = SharedBudget::new(runtime.background_cancellation());
                    let registered = shared_budget.register(budget, cancellation).await;
                    let flight = Flight {
                        receiver,
                        budget: shared_budget.clone(),
                    };
                    flights.insert(key.clone(), flight.clone());
                    let registry = self.clone();
                    let task_key = key.clone();
                    let cancellation = shared_budget.cancellation();
                    let task_work = work.take().ok_or(Error::InternalInvariant {
                        invariant: "new immutable read flight has no work",
                    })?;
                    runtime.tasks.spawn(async move {
                        let result = task_work(cancellation, shared_budget.clone())
                            .await
                            .map_err(Arc::new);
                        // A rejected producer can be replaced before it exits;
                        // retiring it must not remove that replacement flight.
                        registry.retire(&task_key, &shared_budget).await;
                        drop(permit);
                        let _ = sender.send(Some(result));
                    });
                    (flight, registered)
                }
            };
            let registered = match registered {
                Some(registered) => Some(registered),
                None => flight.budget.register(budget, cancellation).await,
            };
            if let Some((rejection, lease)) = registered {
                break (flight, rejection, lease);
            }
            // All previous participants rejected further work. Fresh callers
            // start a producer instead of inheriting another budget's failure.
            self.retire(&key, &flight.budget).await;
        };
        let mut receiver = flight.receiver;
        let mut result = loop {
            if let Some(source) = rejection.borrow().clone() {
                break Err(Error::SharedRead { source });
            }
            let completed = receiver.borrow().clone();
            if let Some(result) = completed {
                break Ok(result);
            }
            tokio::select! {
                biased;
                () = cancellation.cancelled() => break Err(Error::Cancelled),
                changed = rejection.changed() => {
                    if changed.is_err() { break Err(Error::Cancelled); }
                }
                changed = receiver.changed() => {
                    if changed.is_err() {
                        break Err(Error::InternalInvariant {
                            invariant: "immutable read flight ended without a result",
                        });
                    }
                }
            }
        };
        // The last live caller owns joining cleanup. Dropped futures cancel via
        // the lease guard; runtime shutdown still tracks and joins that work.
        if lease.release() {
            while receiver.borrow().is_none() {
                if receiver.changed().await.is_err() {
                    break;
                }
            }
            // Draining a cancelled producer must not hide a semantic failure
            // that completed concurrently with the cancellation request.
            if matches!(result, Err(Error::Cancelled))
                && let Some(Err(source)) = receiver.borrow().clone()
            {
                result = Ok(Err(source));
            }
        }
        drop(receiver);
        result?.map_err(|source| match Arc::try_unwrap(source) {
            Ok(error) => error,
            Err(source) => Error::SharedRead { source },
        })
    }

    async fn retire(&self, key: &K, budget: &Arc<SharedBudget>) {
        let mut flights = self.flights.lock().await;
        if flights
            .get(key)
            .is_some_and(|flight| Arc::ptr_eq(&flight.budget, budget))
        {
            flights.remove(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::sync::Notify;

    #[tokio::test]
    async fn last_waiter_retains_failure_reported_during_cancellation() {
        let runtime = Arc::new(RemoteGitRuntime::default());
        let flights = ReadFlights::<u8, ()>::new();
        let cancellation = CancellationToken::new();
        let budget = OperationBudget::new(crate::OperationLimits::default(), runtime.clone());
        let admission = Arc::new(Semaphore::new(1));
        let started = Arc::new(Notify::new());
        let producer_started = started.clone();
        let mut read = Box::pin(flights.run(
            &runtime,
            0,
            &cancellation,
            &budget,
            &admission,
            move |cancellation, _| async move {
                producer_started.notify_one();
                cancellation.cancelled().await;
                Err(Error::Corrupt {
                    stage: crate::CorruptionStage::PackEntry,
                })
            },
        ));
        tokio::select! {
            () = started.notified() => {},
            result = &mut read => panic!("producer ended before cancellation: {result:?}"),
        }
        cancellation.cancel();
        let result = tokio::time::timeout(Duration::from_secs(1), read).await;
        runtime.shutdown().await;
        assert!(matches!(
            result,
            Ok(Err(Error::Corrupt {
                stage: crate::CorruptionStage::PackEntry
            }))
        ));
    }

    #[tokio::test]
    async fn last_waiter_stops_pending_producer() {
        for cancel in [false, true] {
            let runtime = Arc::new(RemoteGitRuntime::default());
            let flights = ReadFlights::<u8, ()>::new();
            let cancellation = CancellationToken::new();
            let budget = OperationBudget::new(crate::OperationLimits::default(), runtime.clone());
            let admission = Arc::new(Semaphore::new(1));
            let started = Arc::new(Notify::new());
            let stopped = Arc::new(Notify::new());
            {
                let started = started.clone();
                let stopped = stopped.clone();
                let producer_started = started.clone();
                let mut read = Box::pin(flights.run(
                    &runtime,
                    0,
                    &cancellation,
                    &budget,
                    &admission,
                    move |cancellation, _| async move {
                        producer_started.notify_one();
                        cancellation.cancelled().await;
                        stopped.notify_one();
                        Err(Error::Cancelled)
                    },
                ));
                tokio::select! {
                    () = started.notified() => {},
                    result = &mut read => panic!("producer ended before cancellation: {result:?}"),
                }
                if cancel {
                    cancellation.cancel();
                    assert!(matches!(
                        tokio::time::timeout(Duration::from_secs(1), read).await,
                        Ok(Err(Error::Cancelled))
                    ));
                }
            }
            let stopped_before_shutdown =
                tokio::time::timeout(Duration::from_secs(1), stopped.notified()).await;
            let fresh_cancellation = CancellationToken::new();
            let fresh = tokio::time::timeout(
                Duration::from_secs(1),
                flights.run(
                    &runtime,
                    0,
                    &fresh_cancellation,
                    &budget,
                    &admission,
                    |cancellation, _| async move {
                        if cancellation.is_cancelled() {
                            Err(Error::Cancelled)
                        } else {
                            Ok(())
                        }
                    },
                ),
            )
            .await;
            runtime.shutdown().await;
            assert!(stopped_before_shutdown.is_ok(), "cancel={cancel}");
            assert!(
                matches!(fresh, Ok(Ok(()))),
                "fresh reader after cancel={cancel}"
            );
        }
    }
}
