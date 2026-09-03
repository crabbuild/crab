//! Canonical bounded scheduling for LFS object transfers.
//!
//! Batch porcelain, publication, and custom-agent adapters use this module
//! for admission and operation-level policy. The coordinator never retains a
//! task for a request that has not acquired both object and byte capacity.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::core::error::{CrabError, Result, check_cancelled};
use crate::lfs::config::LfsConfig;

/// Default logical in-flight transfer-byte budget.
pub const DEFAULT_IN_FLIGHT_BYTES: u64 = 128 * 1024 * 1024;
const BYTE_PERMIT_UNIT: u64 = 1024 * 1024;

/// Direction used when applying transfer error policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransferDirection {
    Upload,
    Download,
}

/// One bounded coordinator request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TransferRequest {
    pub oid: [u8; 32],
    pub size: u64,
}

/// Result category returned by an adapter after it completes one request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransferOutcome {
    Transferred,
    AlreadyValid,
    Skipped,
}

/// Immutable policy shared by every LFS transfer surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TransferPolicy {
    pub max_concurrency: usize,
    pub max_retries: u32,
    pub max_retry_delay: u32,
    pub skip_download_errors: bool,
    pub max_bandwidth: u64,
    pub in_flight_bytes: u64,
}

impl From<&LfsConfig> for TransferPolicy {
    fn from(config: &LfsConfig) -> Self {
        Self {
            max_concurrency: config.concurrent_transfers.max(1) as usize,
            max_retries: config.transfer_max_retries,
            max_retry_delay: config.transfer_max_retry_delay,
            skip_download_errors: config.skip_download_errors,
            max_bandwidth: config.transfer_max_bandwidth,
            in_flight_bytes: DEFAULT_IN_FLIGHT_BYTES,
        }
    }
}

/// Aggregate operation counters. Counts never contain paths, OIDs, or URLs.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TransferSummary {
    pub requested: u64,
    pub transferred: u64,
    pub already_valid: u64,
    pub skipped: u64,
    pub failed: u64,
    pub logical_bytes: u64,
    pub peak_active_objects: u64,
    pub peak_active_bytes: u64,
}

struct CoordinatorMetrics {
    active_objects: AtomicU64,
    active_bytes: AtomicU64,
    peak_active_objects: AtomicU64,
    peak_active_bytes: AtomicU64,
}

impl CoordinatorMetrics {
    fn admitted(&self, bytes: u64) {
        let active_objects = self.active_objects.fetch_add(1, Ordering::AcqRel) + 1;
        let active_bytes = self.active_bytes.fetch_add(bytes, Ordering::AcqRel) + bytes;
        update_peak(&self.peak_active_objects, active_objects);
        update_peak(&self.peak_active_bytes, active_bytes);
    }

    fn released(&self, bytes: u64) {
        self.active_objects.fetch_sub(1, Ordering::AcqRel);
        self.active_bytes.fetch_sub(bytes, Ordering::AcqRel);
    }
}

fn update_peak(target: &AtomicU64, value: u64) {
    let mut current = target.load(Ordering::Acquire);
    while value > current {
        match target.compare_exchange_weak(current, value, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => break,
            Err(observed) => current = observed,
        }
    }
}

struct RateLimiter {
    bytes_per_second: u64,
    next_available: Mutex<Instant>,
}

impl RateLimiter {
    fn new(bytes_per_second: u64) -> Self {
        Self {
            bytes_per_second,
            next_available: Mutex::new(Instant::now()),
        }
    }

    async fn acquire(&self, bytes: u64, cancellation: &CancellationToken) -> Result<()> {
        if self.bytes_per_second == 0 || bytes == 0 {
            return Ok(());
        }
        let duration = Duration::from_secs_f64(bytes as f64 / self.bytes_per_second as f64);
        let now = Instant::now();
        let mut next = self.next_available.lock().await;
        let start = (*next).max(now);
        *next = start + duration;
        drop(next);
        if start > now {
            let mut sleep = Box::pin(tokio::time::sleep_until(start.into()));
            tokio::select! {
                _ = cancellation.cancelled() => return Err(CrabError::Cancelled),
                _ = &mut sleep => {}
            }
        }
        Ok(())
    }
}

/// Capacity token held for the full lifetime of one transfer.
pub(crate) struct TransferPermit {
    _concurrency: OwnedSemaphorePermit,
    _bytes: OwnedSemaphorePermit,
    metrics: Arc<CoordinatorMetrics>,
    accounted_bytes: u64,
}

impl Drop for TransferPermit {
    fn drop(&mut self) {
        self.metrics.released(self.accounted_bytes);
    }
}

/// One bounded transfer scheduler.
pub(crate) struct TransferCoordinator {
    policy: TransferPolicy,
    concurrency: Arc<Semaphore>,
    bytes: Arc<Semaphore>,
    byte_permits: u32,
    rate_limiter: RateLimiter,
    cancellation: CancellationToken,
    metrics: Arc<CoordinatorMetrics>,
}

impl TransferCoordinator {
    pub(crate) fn new(policy: TransferPolicy) -> Self {
        let byte_permits = byte_permits_for_budget(policy.in_flight_bytes);
        Self {
            concurrency: Arc::new(Semaphore::new(policy.max_concurrency.max(1))),
            bytes: Arc::new(Semaphore::new(byte_permits as usize)),
            byte_permits,
            rate_limiter: RateLimiter::new(policy.max_bandwidth),
            cancellation: CancellationToken::new(),
            metrics: Arc::new(CoordinatorMetrics {
                active_objects: AtomicU64::new(0),
                active_bytes: AtomicU64::new(0),
                peak_active_objects: AtomicU64::new(0),
                peak_active_bytes: AtomicU64::new(0),
            }),
            policy,
        }
    }

    pub(crate) fn cancellation(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    /// Admit one request, applying object, byte, cancellation, and rate limits.
    pub(crate) async fn admit(&self, request: TransferRequest) -> Result<TransferPermit> {
        if self.cancellation.is_cancelled() {
            return Err(CrabError::Cancelled);
        }
        let concurrency = tokio::select! {
            biased;
            () = self.cancellation.cancelled() => return Err(CrabError::Cancelled),
            result = self.concurrency.clone().acquire_owned() =>
                result.map_err(|_| CrabError::Cancelled)?,
        };
        let units = byte_permits_for_request(request.size, self.byte_permits);
        let bytes = tokio::select! {
            biased;
            () = self.cancellation.cancelled() => return Err(CrabError::Cancelled),
            result = self.bytes.clone().acquire_many_owned(units) =>
                result.map_err(|_| CrabError::Cancelled)?,
        };
        self.rate_limiter
            .acquire(request.size, &self.cancellation)
            .await?;
        if self.cancellation.is_cancelled() {
            drop(bytes);
            drop(concurrency);
            return Err(CrabError::Cancelled);
        }
        self.metrics.admitted(request.size);
        Ok(TransferPermit {
            _concurrency: concurrency,
            _bytes: bytes,
            metrics: Arc::clone(&self.metrics),
            accounted_bytes: request.size,
        })
    }

    /// Run one admitted request with the canonical retry policy.
    pub(crate) async fn run_admitted<F, Fut>(
        &self,
        _request: TransferRequest,
        permit: TransferPermit,
        operation: F,
    ) -> Result<TransferOutcome>
    where
        F: Fn(CancellationToken) -> Fut + Send + Sync,
        Fut: Future<Output = Result<TransferOutcome>> + Send,
    {
        let _permit = permit;
        run_with_policy(self.policy, self.cancellation(), operation).await
    }

    /// Execute a request iterator with bounded admission and first-error stop.
    pub(crate) async fn execute<I, F, Fut>(
        &self,
        direction: TransferDirection,
        requests: I,
        operation: F,
    ) -> Result<TransferSummary>
    where
        I: IntoIterator<Item = TransferRequest>,
        F: Fn(TransferRequest, CancellationToken) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<TransferOutcome>> + Send,
    {
        check_cancelled(&self.cancellation)?;
        let mut requests = requests.into_iter();
        let mut active = futures_util::stream::FuturesUnordered::new();
        let mut admission = futures_util::stream::FuturesUnordered::new();
        let mut summary = TransferSummary::default();
        let mut first_error = None;
        let mut admitting = true;

        loop {
            if admitting
                && admission.is_empty()
                && active.len() < self.policy.max_concurrency.max(1)
            {
                match requests.next() {
                    Some(request) => {
                        // Keep one admission future alive while polling transfers.
                        // Recreating it loses the semaphore queue position and can
                        // reserve the same rate-limited slot more than once.
                        admission.push(async move { (request, self.admit(request).await) });
                    }
                    None => admitting = false,
                }
            }
            if active.is_empty() && admission.is_empty() {
                break;
            }
            let (request, result) = tokio::select! {
                biased;
                () = self.cancellation.cancelled(), if first_error.is_none() => {
                    first_error = Some(CrabError::Cancelled);
                    admitting = false;
                    admission.clear();
                    continue;
                }
                Some(completed) = active.next(), if !active.is_empty() => completed,
                Some((request, result)) = admission.next(), if !admission.is_empty() => {
                    match result {
                        Ok(permit) => {
                            summary.requested += 1;
                            let operation = &operation;
                            active.push(async move {
                                let result = self.run_admitted(request, permit, |cancel| {
                                    operation(request, cancel)
                                }).await;
                                (request, result)
                            });
                        }
                        Err(error) => {
                            first_error = Some(error);
                            admitting = false;
                            self.cancellation.cancel();
                        }
                    }
                    continue;
                }
            };
            match result {
                Ok(TransferOutcome::Transferred) => {
                    summary.transferred += 1;
                    summary.logical_bytes = summary.logical_bytes.saturating_add(request.size);
                }
                Ok(TransferOutcome::AlreadyValid) => {
                    summary.already_valid += 1;
                    summary.logical_bytes = summary.logical_bytes.saturating_add(request.size);
                }
                Ok(TransferOutcome::Skipped) => summary.skipped += 1,
                Err(error) => {
                    summary.failed += 1;
                    if direction == TransferDirection::Download
                        && self.policy.skip_download_errors
                        && !matches!(error, CrabError::Cancelled)
                    {
                        summary.skipped += 1;
                    } else if first_error.is_none() {
                        first_error = Some(error);
                        admitting = false;
                        self.cancellation.cancel();
                        admission.clear();
                    }
                }
            }
        }

        summary.peak_active_objects = self.metrics.peak_active_objects.load(Ordering::Acquire);
        summary.peak_active_bytes = self.metrics.peak_active_bytes.load(Ordering::Acquire);
        if let Some(error) = first_error {
            return Err(error);
        }
        check_cancelled(&self.cancellation)?;
        Ok(summary)
    }
}

async fn run_with_policy<F, Fut>(
    policy: TransferPolicy,
    cancellation: CancellationToken,
    operation: F,
) -> Result<TransferOutcome>
where
    F: Fn(CancellationToken) -> Fut + Send + Sync,
    Fut: Future<Output = Result<TransferOutcome>> + Send,
{
    let mut retries = 0;
    loop {
        if cancellation.is_cancelled() {
            return Err(CrabError::Cancelled);
        }
        // Adapters may own multipart aborts or temporary-file cleanup across
        // awaits. Drain the attempt rather than dropping it on cancellation,
        // but never report success or begin a retry after cancellation.
        let result = operation(cancellation.clone()).await;
        check_cancelled(&cancellation)?;
        match result {
            Ok(outcome) => return Ok(outcome),
            Err(error) if error.is_retryable() && retries < policy.max_retries => {
                retries += 1;
                let delay = retry_delay(policy.max_retry_delay, retries);
                if !delay.is_zero() {
                    let mut sleep = Box::pin(tokio::time::sleep(delay));
                    tokio::select! {
                        _ = cancellation.cancelled() => return Err(CrabError::Cancelled),
                        _ = &mut sleep => {}
                    }
                }
            }
            Err(error) => return Err(error),
        }
    }
}

fn retry_delay(max_delay_seconds: u32, retry_number: u32) -> Duration {
    let exponent = retry_number.saturating_sub(1).min(30);
    let seconds = 1u64
        .checked_shl(exponent)
        .unwrap_or(u64::MAX)
        .min(max_delay_seconds as u64);
    Duration::from_secs(seconds)
}

fn byte_permits_for_budget(bytes: u64) -> u32 {
    let units = bytes.saturating_add(BYTE_PERMIT_UNIT - 1) / BYTE_PERMIT_UNIT;
    units.clamp(1, u32::MAX as u64) as u32
}

fn byte_permits_for_request(size: u64, budget_units: u32) -> u32 {
    let requested = byte_permits_for_budget(size);
    requested.min(budget_units).max(1)
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    fn request(index: u8, size: u64) -> TransferRequest {
        TransferRequest {
            oid: [index; 32],
            size,
        }
    }

    fn policy() -> TransferPolicy {
        TransferPolicy {
            max_concurrency: 2,
            max_retries: 0,
            max_retry_delay: 0,
            skip_download_errors: false,
            max_bandwidth: 0,
            in_flight_bytes: 2 * BYTE_PERMIT_UNIT,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn byte_backpressure_keeps_polling_admitted_transfers() {
        let coordinator = TransferCoordinator::new(TransferPolicy {
            max_concurrency: 4,
            ..policy()
        });
        let summary = tokio::time::timeout(
            Duration::from_secs(2),
            coordinator.execute(
                TransferDirection::Upload,
                (0..4).map(|index| request(index, 2 * BYTE_PERMIT_UNIT)),
                |_, _| async {
                    tokio::task::yield_now().await;
                    Ok(TransferOutcome::Transferred)
                },
            ),
        )
        .await
        .expect("byte admission must not prevent active transfers from releasing permits")
        .unwrap();
        assert_eq!((summary.transferred, summary.peak_active_objects), (4, 1));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancellation_releases_waiting_object_and_byte_admissions() {
        for max_concurrency in [1, 2] {
            let coordinator = TransferCoordinator::new(TransferPolicy {
                max_concurrency,
                ..policy()
            });
            let permit = coordinator
                .admit(request(0, 2 * BYTE_PERMIT_UNIT))
                .await
                .unwrap();
            let waiting = coordinator.admit(request(1, 2 * BYTE_PERMIT_UNIT));
            tokio::pin!(waiting);
            std::future::poll_fn(|cx| {
                assert!(waiting.as_mut().poll(cx).is_pending());
                std::task::Poll::Ready(())
            })
            .await;
            coordinator.cancellation().cancel();
            let result = tokio::time::timeout(Duration::from_secs(2), waiting)
                .await
                .unwrap();
            assert!(matches!(result, Err(CrabError::Cancelled)));
            drop(permit);
            assert_eq!(
                (
                    coordinator.concurrency.available_permits(),
                    coordinator.bytes.available_permits()
                ),
                (max_concurrency, 2),
            );
            assert_eq!(
                coordinator.metrics.active_objects.load(Ordering::Acquire),
                0
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancellation_drains_active_attempts_and_never_becomes_skipped_success() {
        let coordinator = TransferCoordinator::new(TransferPolicy {
            skip_download_errors: true,
            ..policy()
        });
        let started = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        let completed = Arc::new(AtomicUsize::new(0));
        let execution = coordinator.execute(
            TransferDirection::Download,
            [
                request(0, 2 * BYTE_PERMIT_UNIT),
                request(1, 2 * BYTE_PERMIT_UNIT),
            ],
            {
                let started = Arc::clone(&started);
                let release = Arc::clone(&release);
                let completed = Arc::clone(&completed);
                move |_, _| {
                    let started = Arc::clone(&started);
                    let release = Arc::clone(&release);
                    let completed = Arc::clone(&completed);
                    async move {
                        started.notify_one();
                        release.notified().await;
                        completed.fetch_add(1, Ordering::AcqRel);
                        Ok(TransferOutcome::Transferred)
                    }
                }
            },
        );
        tokio::pin!(execution);
        tokio::select! {
            _ = started.notified() => {}
            result = &mut execution => panic!("operation finished before release: {result:?}"),
            _ = tokio::time::sleep(Duration::from_secs(2)) => panic!("operation did not start"),
        }
        coordinator.cancellation().cancel();
        std::future::poll_fn(|cx| {
            assert!(execution.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        assert_eq!(
            coordinator.metrics.active_objects.load(Ordering::Acquire),
            1
        );
        release.notify_one();
        let result = tokio::time::timeout(Duration::from_secs(2), execution)
            .await
            .unwrap();
        assert!(matches!(result, Err(CrabError::Cancelled)));
        assert_eq!(completed.load(Ordering::Acquire), 1);
        assert_eq!(
            coordinator.metrics.active_objects.load(Ordering::Acquire),
            0
        );
    }

    #[tokio::test]
    async fn cancelled_empty_execution_is_not_success() {
        let coordinator = TransferCoordinator::new(policy());
        coordinator.cancellation().cancel();
        let result = coordinator
            .execute(TransferDirection::Upload, [], |_, _| async {
                panic!("cancelled operation must not start")
            })
            .await;
        assert!(matches!(result, Err(CrabError::Cancelled)));
    }

    #[tokio::test]
    async fn scheduler_bounds_active_objects_and_bytes_for_large_queue() {
        let coordinator = TransferCoordinator::new(TransferPolicy {
            max_concurrency: 4,
            max_retries: 1,
            max_retry_delay: 1,
            skip_download_errors: false,
            max_bandwidth: 0,
            in_flight_bytes: 8 * BYTE_PERMIT_UNIT,
        });
        let requests = (0..100_000).map(|index| request((index % 255) as u8, 2 * BYTE_PERMIT_UNIT));
        let active = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let active_for_operation = Arc::clone(&active);
        let peak_for_operation = Arc::clone(&peak);
        let summary = coordinator
            .execute(TransferDirection::Upload, requests, move |_, _| {
                let active = Arc::clone(&active_for_operation);
                let peak = Arc::clone(&peak_for_operation);
                async move {
                    let now = active.fetch_add(1, Ordering::AcqRel) + 1;
                    peak.fetch_max(now, Ordering::AcqRel);
                    tokio::task::yield_now().await;
                    active.fetch_sub(1, Ordering::AcqRel);
                    Ok(TransferOutcome::Transferred)
                }
            })
            .await
            .unwrap();

        assert_eq!(summary.requested, 100_000);
        assert!(peak.load(Ordering::Acquire) <= 4);
        assert!(summary.peak_active_bytes <= 8 * BYTE_PERMIT_UNIT);
    }

    #[tokio::test]
    async fn first_error_stops_admission_and_cancels_remaining_work() {
        let coordinator = TransferCoordinator::new(TransferPolicy {
            max_concurrency: 2,
            max_retries: 1,
            max_retry_delay: 1,
            skip_download_errors: false,
            max_bandwidth: 0,
            in_flight_bytes: 8 * BYTE_PERMIT_UNIT,
        });
        let admitted = Arc::new(AtomicUsize::new(0));
        let admitted_for_operation = Arc::clone(&admitted);
        let error = coordinator
            .execute(
                TransferDirection::Upload,
                (0..100).map(|index| request(index as u8, 1)),
                move |request, cancellation| {
                    let admitted = Arc::clone(&admitted_for_operation);
                    async move {
                        admitted.fetch_add(1, Ordering::AcqRel);
                        if request.oid[0] == 0 {
                            return Err(CrabError::Internal(
                                "synthetic transfer failure".to_owned(),
                            ));
                        }
                        cancellation.cancelled().await;
                        Err(CrabError::Cancelled)
                    }
                },
            )
            .await
            .unwrap_err();
        assert!(matches!(error, CrabError::Internal(_)));
        assert!(admitted.load(Ordering::Acquire) <= 2);
    }

    #[test]
    fn oversized_request_uses_the_whole_byte_budget() {
        assert_eq!(byte_permits_for_request(64 * BYTE_PERMIT_UNIT, 8), 8);
        assert_eq!(byte_permits_for_request(0, 8), 1);
    }
}
