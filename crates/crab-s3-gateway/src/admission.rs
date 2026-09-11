use std::{sync::Arc, time::Duration};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::metrics::Metrics;

const QUEUE_DEPTH_MULTIPLIER: usize = 4;
const QUEUE_WAIT_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug)]
pub(crate) enum RequestClass {
    Control,
    Read,
    Transfer,
}

impl RequestClass {
    pub(crate) const ALL: [Self; 3] = [Self::Control, Self::Read, Self::Transfer];

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Control => "control",
            Self::Read => "read",
            Self::Transfer => "transfer",
        }
    }

    pub(crate) const fn index(self) -> usize {
        self as usize
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct PoolSnapshot {
    pub(crate) class: RequestClass,
    pub(crate) active: usize,
    pub(crate) active_capacity: usize,
    pub(crate) queued: usize,
    pub(crate) queue_capacity: usize,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum AdmissionOutcome {
    AdmittedImmediate,
    AdmittedQueued,
    Overloaded,
    Timeout,
    Cancelled,
    StateError,
}

impl AdmissionOutcome {
    pub(crate) const COUNT: usize = 6;
    pub(crate) const ALL: [Self; Self::COUNT] = [
        Self::AdmittedImmediate,
        Self::AdmittedQueued,
        Self::Overloaded,
        Self::Timeout,
        Self::Cancelled,
        Self::StateError,
    ];

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::AdmittedImmediate => "admitted_immediate",
            Self::AdmittedQueued => "admitted_queued",
            Self::Overloaded => "overloaded",
            Self::Timeout => "timeout",
            Self::Cancelled => "cancelled",
            Self::StateError => "state_error",
        }
    }

    pub(crate) const fn index(self) -> usize {
        self as usize
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub(crate) enum Error {
    #[error("request admission queue is full")]
    Overloaded,
    #[error("request admission wait timed out")]
    AdmissionTimeout,
    #[error("request admission was cancelled")]
    Cancelled,
    #[error("request admission state is unavailable")]
    AdmissionState,
}

#[derive(Clone)]
pub(crate) struct Admission {
    control: Pool,
    read: Pool,
    transfer: Pool,
    metrics: Metrics,
    cancellation: CancellationToken,
}

#[derive(Clone)]
struct Pool {
    active: Arc<Semaphore>,
    queued: Arc<Semaphore>,
    active_capacity: usize,
    queue_capacity: usize,
    wait_timeout: Duration,
}

#[derive(Debug)]
pub(crate) struct RequestPermit {
    _active: OwnedSemaphorePermit,
}

impl Admission {
    pub(crate) fn new(
        max_in_flight: usize,
        cancellation: CancellationToken,
        metrics: Metrics,
    ) -> Self {
        Self::with_timeout(max_in_flight, cancellation, QUEUE_WAIT_TIMEOUT, metrics)
    }

    fn with_timeout(
        max_in_flight: usize,
        cancellation: CancellationToken,
        wait_timeout: Duration,
        metrics: Metrics,
    ) -> Self {
        let control = (max_in_flight / 8).max(2);
        let read = (max_in_flight / 4).max(4);
        let transfer = max_in_flight - control - read;
        Self {
            control: Pool::new(control, wait_timeout),
            read: Pool::new(read, wait_timeout),
            transfer: Pool::new(transfer, wait_timeout),
            metrics,
            cancellation,
        }
    }

    pub(crate) async fn acquire(&self, class: RequestClass) -> Result<RequestPermit, Error> {
        let result = self.pool(class).acquire(&self.cancellation).await;
        let outcome = match &result {
            Ok((_, false)) => AdmissionOutcome::AdmittedImmediate,
            Ok((_, true)) => AdmissionOutcome::AdmittedQueued,
            Err(Error::Overloaded) => AdmissionOutcome::Overloaded,
            Err(Error::AdmissionTimeout) => AdmissionOutcome::Timeout,
            Err(Error::Cancelled) => AdmissionOutcome::Cancelled,
            Err(Error::AdmissionState) => AdmissionOutcome::StateError,
        };
        self.metrics.record_admission(class, outcome);
        result.map(|(permit, _)| permit)
    }

    pub(crate) fn snapshot(&self) -> [PoolSnapshot; 3] {
        RequestClass::ALL.map(|class| self.pool(class).snapshot(class))
    }

    fn pool(&self, class: RequestClass) -> &Pool {
        match class {
            RequestClass::Control => &self.control,
            RequestClass::Read => &self.read,
            RequestClass::Transfer => &self.transfer,
        }
    }
}

impl Pool {
    fn new(max_active: usize, wait_timeout: Duration) -> Self {
        let queue_capacity = max_active.saturating_mul(QUEUE_DEPTH_MULTIPLIER);
        Self {
            active: Arc::new(Semaphore::new(max_active)),
            queued: Arc::new(Semaphore::new(queue_capacity)),
            active_capacity: max_active,
            queue_capacity,
            wait_timeout,
        }
    }

    async fn acquire(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<(RequestPermit, bool), Error> {
        if cancellation.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if let Ok(active) = Arc::clone(&self.active).try_acquire_owned() {
            return Ok((RequestPermit { _active: active }, false));
        }
        let queued = Arc::clone(&self.queued)
            .try_acquire_owned()
            .map_err(|_| Error::Overloaded)?;
        let acquired = tokio::select! {
            () = cancellation.cancelled() => Err(Error::Cancelled),
            result = tokio::time::timeout(
                self.wait_timeout,
                Arc::clone(&self.active).acquire_owned(),
            ) => {
                match result {
                    Ok(Ok(permit)) => Ok(permit),
                    Ok(Err(_)) => Err(Error::AdmissionState),
                    Err(_) => Err(Error::AdmissionTimeout),
                }
            }
        };
        drop(queued);
        acquired.map(|active| (RequestPermit { _active: active }, true))
    }

    fn snapshot(&self, class: RequestClass) -> PoolSnapshot {
        PoolSnapshot {
            class,
            active: self
                .active_capacity
                .saturating_sub(self.active.available_permits()),
            active_capacity: self.active_capacity,
            queued: self
                .queue_capacity
                .saturating_sub(self.queued.available_permits()),
            queue_capacity: self.queue_capacity,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn admission(wait_timeout: Duration) -> Admission {
        Admission::with_timeout(
            8,
            CancellationToken::new(),
            wait_timeout,
            Metrics::new().unwrap(),
        )
    }

    #[test]
    fn request_budget_is_partitioned_without_expansion() {
        let admission = admission(Duration::from_secs(1));

        assert_eq!(admission.control.active.available_permits(), 2);
        assert_eq!(admission.read.active.available_permits(), 4);
        assert_eq!(admission.transfer.active.available_permits(), 2);
    }

    #[tokio::test]
    async fn saturated_transfers_do_not_consume_control_or_read_capacity() {
        let admission = admission(Duration::from_secs(1));
        let _first = admission.acquire(RequestClass::Transfer).await.unwrap();
        let _second = admission.acquire(RequestClass::Transfer).await.unwrap();

        let _control = admission.acquire(RequestClass::Control).await.unwrap();
        let _read = admission.acquire(RequestClass::Read).await.unwrap();
    }

    #[tokio::test]
    async fn queued_request_acquires_released_capacity() {
        let admission = admission(Duration::from_secs(1));
        let first = admission.acquire(RequestClass::Transfer).await.unwrap();
        let _second = admission.acquire(RequestClass::Transfer).await.unwrap();
        let queued_admission = admission.clone();
        let queued =
            tokio::spawn(async move { queued_admission.acquire(RequestClass::Transfer).await });
        tokio::task::yield_now().await;

        drop(first);

        queued.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn queue_bound_rejects_excess_work() {
        let admission = admission(Duration::from_secs(1));
        let _first = admission.acquire(RequestClass::Transfer).await.unwrap();
        let _second = admission.acquire(RequestClass::Transfer).await.unwrap();
        let mut queued = Vec::new();
        for _ in 0..8 {
            let queued_admission = admission.clone();
            queued.push(tokio::spawn(async move {
                queued_admission.acquire(RequestClass::Transfer).await
            }));
        }
        while admission.transfer.queued.available_permits() != 0 {
            tokio::task::yield_now().await;
        }

        assert_eq!(
            admission.acquire(RequestClass::Transfer).await.unwrap_err(),
            Error::Overloaded
        );
        admission.cancellation.cancel();
        for task in queued {
            assert_eq!(task.await.unwrap().unwrap_err(), Error::Cancelled);
        }
    }

    #[tokio::test]
    async fn wait_timeout_releases_queue_capacity() {
        let admission = admission(Duration::from_millis(1));
        let _first = admission.acquire(RequestClass::Transfer).await.unwrap();
        let _second = admission.acquire(RequestClass::Transfer).await.unwrap();

        assert_eq!(
            admission.acquire(RequestClass::Transfer).await.unwrap_err(),
            Error::AdmissionTimeout
        );
        assert_eq!(admission.transfer.queued.available_permits(), 8);
    }

    #[tokio::test]
    async fn cancellation_releases_queue_capacity() {
        let admission = admission(Duration::from_secs(1));
        let _first = admission.acquire(RequestClass::Transfer).await.unwrap();
        let _second = admission.acquire(RequestClass::Transfer).await.unwrap();
        let queued_admission = admission.clone();
        let queued =
            tokio::spawn(async move { queued_admission.acquire(RequestClass::Transfer).await });
        while admission.transfer.queued.available_permits() == 8 {
            tokio::task::yield_now().await;
        }

        admission.cancellation.cancel();

        assert_eq!(queued.await.unwrap().unwrap_err(), Error::Cancelled);
        assert_eq!(admission.transfer.queued.available_permits(), 8);
    }

    #[tokio::test]
    async fn snapshot_reports_capacity_pressure_and_bounded_outcomes() {
        let admission = admission(Duration::from_secs(1));
        let _first = admission.acquire(RequestClass::Transfer).await.unwrap();
        let _second = admission.acquire(RequestClass::Transfer).await.unwrap();
        let queued_admission = admission.clone();
        let queued =
            tokio::spawn(async move { queued_admission.acquire(RequestClass::Transfer).await });
        while admission.transfer.queued.available_permits() == 8 {
            tokio::task::yield_now().await;
        }

        let transfer = admission.snapshot()[2];
        assert_eq!(
            (
                transfer.class.label(),
                transfer.active,
                transfer.active_capacity,
                transfer.queued,
                transfer.queue_capacity,
            ),
            ("transfer", 2, 2, 1, 8)
        );

        admission.cancellation.cancel();
        assert_eq!(queued.await.unwrap().unwrap_err(), Error::Cancelled);
        assert!(admission.metrics.render(&admission).contains(
            "crab_s3_gateway_admission_events_total{class=\"transfer\",outcome=\"cancelled\"} 1"
        ));
    }
}
