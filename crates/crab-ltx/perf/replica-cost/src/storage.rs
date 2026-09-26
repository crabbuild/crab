use crab_storage::{StorageObservation, StorageObserver, StorageOperation, StorageOutcome};
use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Default)]
struct Counter {
    calls: AtomicU64,
    nanos: AtomicU64,
    bytes_read: AtomicU64,
    bytes_written: AtomicU64,
}

#[derive(Default)]
pub(super) struct StorageCosts([[Counter; StorageOutcome::ALL.len()]; StorageOperation::ALL.len()]);

#[derive(Clone, Debug, Serialize)]
pub(super) struct BackendCost {
    operation: &'static str,
    outcome: &'static str,
    calls: u64,
    elapsed_us: u64,
    bytes_read: u64,
    bytes_written: u64,
}

impl StorageCosts {
    // The runner takes a snapshot only after its serial preparation has ended.
    pub(super) fn take(&self) -> Vec<BackendCost> {
        let mut costs = Vec::new();
        for operation in StorageOperation::ALL {
            for outcome in StorageOutcome::ALL {
                let counter = &self.0[operation.index()][outcome.index()];
                let calls = counter.calls.swap(0, Ordering::Relaxed);
                if calls != 0 {
                    costs.push(BackendCost {
                        operation: operation.label(),
                        outcome: outcome.label(),
                        calls,
                        elapsed_us: counter.nanos.swap(0, Ordering::Relaxed) / 1_000,
                        bytes_read: counter.bytes_read.swap(0, Ordering::Relaxed),
                        bytes_written: counter.bytes_written.swap(0, Ordering::Relaxed),
                    });
                }
            }
        }
        costs
    }
}

impl StorageObserver for StorageCosts {
    fn started(&self, _: StorageOperation) {}

    fn finished(&self, observation: StorageObservation) {
        let counter = &self.0[observation.operation.index()][observation.outcome.index()];
        counter.calls.fetch_add(1, Ordering::Relaxed);
        counter.nanos.fetch_add(
            observation.duration.as_nanos().min(u128::from(u64::MAX)) as u64,
            Ordering::Relaxed,
        );
        counter
            .bytes_read
            .fetch_add(observation.bytes_read, Ordering::Relaxed);
        counter
            .bytes_written
            .fetch_add(observation.bytes_written, Ordering::Relaxed);
    }
}
