use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Instant,
};

use bytes::Bytes;
use http::{Method, StatusCode};
use http_body::{Body as _, Frame, SizeHint};
use metrics::{Counter, Gauge, Histogram, Key, KeyName, Label, Level, Metadata, Recorder, Unit};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

use crate::{
    admission::{Admission, AdmissionOutcome, RequestClass},
    multipart::SweepStats,
};

mod backend;
mod cache;
mod filesystem;
mod scratch;

pub(crate) use filesystem::{ScratchCapacityError, ScratchReservation};
pub(crate) use scratch::{ScratchFailure, ScratchPurpose, ScratchUsage};

const METHOD_COUNT: usize = 6;
const OUTCOME_COUNT: usize = 8;
const DURATION_BUCKETS_SECONDS: [f64; 16] = [
    0.005, 0.010, 0.025, 0.050, 0.100, 0.250, 0.500, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0,
    600.0,
];
const METHOD_LABELS: [&str; METHOD_COUNT] = ["get", "head", "put", "post", "delete", "other"];
const OUTCOME_LABELS: [&str; OUTCOME_COUNT] = [
    "1xx",
    "2xx",
    "3xx",
    "4xx",
    "5xx",
    "other",
    "transport_error",
    "cancelled",
];
const MAINTENANCE_CYCLE_OUTCOMES: [&str; 3] = ["success", "degraded", "clock_error"];
const MAINTENANCE_ACTIONS: [&str; 4] = [
    "expired_upload",
    "terminal_cleanup",
    "missing_session_cleanup",
    "published_recovery",
];
const MAINTENANCE_FAILURES: [&str; 7] = [
    "timeout",
    "sweep_error",
    "repository_missing",
    "reconciliation_error",
    "publication_recovery_error",
    "publication_unresolved",
    "clock_error",
];
const METADATA: Metadata<'static> = Metadata::new(
    "crab_s3_gateway",
    Level::INFO,
    Some("crab_s3_gateway::metrics"),
);

#[derive(Clone)]
pub(crate) struct Metrics {
    inner: Arc<MetricsInner>,
}

struct MetricsInner {
    handle: PrometheusHandle,
    methods: [MethodMetrics; METHOD_COUNT],
    admission: [AdmissionMetrics; RequestClass::ALL.len()],
    multipart_maintenance: MultipartMaintenanceMetrics,
    backend: backend::BackendMetrics,
    cache: cache::CacheMetrics,
    filesystem: filesystem::FilesystemMetrics,
    scratch: scratch::ScratchMetrics,
}

struct MethodMetrics {
    requests: [Counter; OUTCOME_COUNT],
    in_flight: Gauge,
    duration: Histogram,
    body_errors: Counter,
    body_aborts: Counter,
}

struct AdmissionMetrics {
    in_flight: Gauge,
    capacity: Gauge,
    queued: Gauge,
    queue_capacity: Gauge,
    events: [Counter; AdmissionOutcome::COUNT],
}

struct MultipartMaintenanceMetrics {
    cycles: [Counter; MAINTENANCE_CYCLE_OUTCOMES.len()],
    actions: [Counter; MAINTENANCE_ACTIONS.len()],
    failures: [Counter; MAINTENANCE_FAILURES.len()],
    duration: Histogram,
    last_success: Gauge,
}

#[derive(Clone, Copy)]
pub(crate) enum MaintenanceFailure {
    Timeout,
    Sweep,
    RepositoryMissing,
    Reconciliation,
    PublicationRecovery,
    PublicationUnresolved,
    Clock,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum MaintenanceCycleOutcome {
    Success,
    Degraded,
    ClockError,
}

impl MaintenanceCycleOutcome {
    const fn index(self) -> usize {
        self as usize
    }
}

impl MaintenanceFailure {
    const fn index(self) -> usize {
        self as usize
    }
}

impl Metrics {
    pub(crate) fn new() -> Result<Self, metrics_exporter_prometheus::BuildError> {
        Self::new_with_scratch_path(std::env::temp_dir())
    }

    fn new_with_scratch_path(
        scratch_path: std::path::PathBuf,
    ) -> Result<Self, metrics_exporter_prometheus::BuildError> {
        let recorder = PrometheusBuilder::new()
            .set_buckets(&DURATION_BUCKETS_SECONDS)?
            .build_recorder();
        describe_metrics(&recorder);
        let methods = METHOD_LABELS.map(|method| MethodMetrics::new(&recorder, method));
        let admission =
            RequestClass::ALL.map(|class| AdmissionMetrics::new(&recorder, class.label()));
        let multipart_maintenance = MultipartMaintenanceMetrics::new(&recorder);
        let backend = backend::BackendMetrics::new(&recorder);
        let cache = cache::CacheMetrics::new(&recorder);
        let filesystem = filesystem::FilesystemMetrics::new(&recorder, scratch_path);
        let scratch = scratch::ScratchMetrics::new(&recorder);
        Ok(Self {
            inner: Arc::new(MetricsInner {
                handle: recorder.handle(),
                methods,
                admission,
                multipart_maintenance,
                backend,
                cache,
                filesystem,
                scratch,
            }),
        })
    }

    pub(crate) fn set_cache_limit(&self, bytes: u64) {
        self.inner.cache.set_limit(bytes);
    }

    pub(crate) fn cache_observer(&self) -> Arc<dyn crab_cache_store::CacheObserver> {
        self.inner.cache.observer()
    }

    pub(crate) async fn refresh_cache(&self, cache: &crab_cache::LocalCache) {
        self.inner.cache.refresh(cache.root()).await;
    }

    pub(crate) fn start_request(&self, method: &Method) -> RequestObservation {
        let method = method_index(method);
        self.inner.methods[method].in_flight.increment(1);
        RequestObservation {
            metrics: self.clone(),
            method,
            started: Instant::now(),
            outcome_recorded: false,
            finished: false,
        }
    }

    pub(crate) fn record_admission(&self, class: RequestClass, outcome: AdmissionOutcome) {
        self.inner.admission[class.index()].events[outcome.index()].increment(1);
    }

    pub(crate) fn record_maintenance_failure(&self, failure: MaintenanceFailure, count: usize) {
        self.inner.multipart_maintenance.failures[failure.index()].increment(saturating_u64(count));
    }

    pub(crate) fn record_maintenance_result(&self, stats: &SweepStats) -> bool {
        self.record_maintenance_actions(
            stats.expired,
            stats.terminal_cleanups,
            stats.missing_cleanups,
            stats.published_recoveries,
        );
        for (failure, count) in [
            (
                MaintenanceFailure::Reconciliation,
                stats.reconciliation_failures,
            ),
            (
                MaintenanceFailure::PublicationRecovery,
                stats.publication_recovery_failures,
            ),
            (
                MaintenanceFailure::PublicationUnresolved,
                stats.unresolved_completions,
            ),
        ] {
            self.record_maintenance_failure(failure, count);
        }
        stats.reconciliation_failures == 0
            && stats.publication_recovery_failures == 0
            && stats.unresolved_completions == 0
    }

    fn record_maintenance_actions(
        &self,
        expired: usize,
        terminal_cleanups: usize,
        missing_cleanups: usize,
        published_recoveries: usize,
    ) {
        for (counter, count) in self.inner.multipart_maintenance.actions.iter().zip([
            expired,
            terminal_cleanups,
            missing_cleanups,
            published_recoveries,
        ]) {
            counter.increment(saturating_u64(count));
        }
    }

    pub(crate) fn record_maintenance_cycle(
        &self,
        outcome: MaintenanceCycleOutcome,
        started: Instant,
        now_seconds: u64,
    ) {
        self.inner.multipart_maintenance.cycles[outcome.index()].increment(1);
        self.inner
            .multipart_maintenance
            .duration
            .record(started.elapsed().as_secs_f64());
        if outcome == MaintenanceCycleOutcome::Success {
            self.inner
                .multipart_maintenance
                .last_success
                .set(now_seconds as f64);
        }
    }

    pub(crate) fn start_scratch(&self, purpose: ScratchPurpose) -> ScratchUsage {
        self.inner.scratch.start(purpose)
    }

    pub(crate) fn reserve_scratch(
        &self,
        bytes: u64,
    ) -> Result<ScratchReservation, ScratchCapacityError> {
        self.inner.filesystem.reserve(bytes)
    }

    pub(crate) fn storage_observer(&self) -> Arc<dyn crab_storage::StorageObserver> {
        self.inner.backend.observer()
    }

    pub(crate) fn render(&self, admission: &Admission) -> String {
        self.inner.filesystem.refresh();
        for pool in admission.snapshot() {
            let metrics = &self.inner.admission[pool.class.index()];
            metrics.in_flight.set(pool.active as f64);
            metrics.capacity.set(pool.active_capacity as f64);
            metrics.queued.set(pool.queued as f64);
            metrics.queue_capacity.set(pool.queue_capacity as f64);
        }
        self.inner.handle.run_upkeep();
        self.inner.handle.render()
    }

    fn record_request(&self, method: usize, outcome: usize) {
        self.inner.methods[method].requests[outcome].increment(1);
    }

    fn record_duration(&self, method: usize, started: Instant) {
        self.inner.methods[method]
            .duration
            .record(started.elapsed().as_secs_f64());
    }
}

impl MethodMetrics {
    fn new(recorder: &impl Recorder, method: &'static str) -> Self {
        Self {
            requests: OUTCOME_LABELS.map(|outcome| {
                recorder.register_counter(
                    &key(
                        "crab_s3_gateway_http_requests_total",
                        &[("method", method), ("outcome", outcome)],
                    ),
                    &METADATA,
                )
            }),
            in_flight: recorder.register_gauge(
                &key(
                    "crab_s3_gateway_http_in_flight_requests",
                    &[("method", method)],
                ),
                &METADATA,
            ),
            duration: recorder.register_histogram(
                &key(
                    "crab_s3_gateway_http_request_duration_seconds",
                    &[("method", method)],
                ),
                &METADATA,
            ),
            body_errors: recorder.register_counter(
                &key(
                    "crab_s3_gateway_http_response_body_errors_total",
                    &[("method", method)],
                ),
                &METADATA,
            ),
            body_aborts: recorder.register_counter(
                &key(
                    "crab_s3_gateway_http_response_body_aborts_total",
                    &[("method", method)],
                ),
                &METADATA,
            ),
        }
    }
}

impl AdmissionMetrics {
    fn new(recorder: &impl Recorder, class: &'static str) -> Self {
        Self {
            in_flight: recorder.register_gauge(
                &key(
                    "crab_s3_gateway_admission_in_flight_requests",
                    &[("class", class)],
                ),
                &METADATA,
            ),
            capacity: recorder.register_gauge(
                &key("crab_s3_gateway_admission_capacity", &[("class", class)]),
                &METADATA,
            ),
            queued: recorder.register_gauge(
                &key(
                    "crab_s3_gateway_admission_queued_requests",
                    &[("class", class)],
                ),
                &METADATA,
            ),
            queue_capacity: recorder.register_gauge(
                &key(
                    "crab_s3_gateway_admission_queue_capacity",
                    &[("class", class)],
                ),
                &METADATA,
            ),
            events: AdmissionOutcome::ALL.map(|outcome| {
                recorder.register_counter(
                    &key(
                        "crab_s3_gateway_admission_events_total",
                        &[("class", class), ("outcome", outcome.label())],
                    ),
                    &METADATA,
                )
            }),
        }
    }
}

impl MultipartMaintenanceMetrics {
    fn new(recorder: &impl Recorder) -> Self {
        Self {
            cycles: labeled_counters(
                recorder,
                "crab_s3_gateway_multipart_maintenance_cycles_total",
                "outcome",
                MAINTENANCE_CYCLE_OUTCOMES,
            ),
            actions: labeled_counters(
                recorder,
                "crab_s3_gateway_multipart_maintenance_actions_total",
                "action",
                MAINTENANCE_ACTIONS,
            ),
            failures: labeled_counters(
                recorder,
                "crab_s3_gateway_multipart_maintenance_failures_total",
                "reason",
                MAINTENANCE_FAILURES,
            ),
            duration: recorder.register_histogram(
                &Key::from_static_name(
                    "crab_s3_gateway_multipart_maintenance_cycle_duration_seconds",
                ),
                &METADATA,
            ),
            last_success: recorder.register_gauge(
                &Key::from_static_name(
                    "crab_s3_gateway_multipart_maintenance_last_success_timestamp_seconds",
                ),
                &METADATA,
            ),
        }
    }
}

pub(crate) struct RequestObservation {
    metrics: Metrics,
    method: usize,
    started: Instant,
    outcome_recorded: bool,
    finished: bool,
}

impl RequestObservation {
    pub(crate) fn response(mut self, status: StatusCode) -> Self {
        self.metrics
            .record_request(self.method, status_outcome(status));
        self.outcome_recorded = true;
        self
    }

    pub(crate) fn transport_error(mut self) {
        self.metrics.record_request(self.method, 6);
        self.outcome_recorded = true;
        self.finish();
    }

    fn body_error(&mut self) {
        self.metrics.inner.methods[self.method]
            .body_errors
            .increment(1);
        self.finish();
    }

    fn body_abort(&mut self) {
        self.metrics.inner.methods[self.method]
            .body_aborts
            .increment(1);
        self.finish();
    }

    fn finish(&mut self) {
        if self.finished {
            return;
        }
        self.metrics.record_duration(self.method, self.started);
        self.metrics.inner.methods[self.method]
            .in_flight
            .decrement(1);
        self.finished = true;
    }
}

impl Drop for RequestObservation {
    fn drop(&mut self) {
        if !self.outcome_recorded {
            self.metrics.record_request(self.method, 7);
            self.outcome_recorded = true;
        }
        self.finish();
    }
}

pub(crate) struct ObservedBody {
    inner: s3s::Body,
    observation: Option<RequestObservation>,
}

impl ObservedBody {
    pub(crate) fn new(inner: s3s::Body, mut observation: RequestObservation) -> Self {
        let observation = if inner.is_end_stream() {
            observation.finish();
            None
        } else {
            Some(observation)
        };
        Self { inner, observation }
    }

    fn finish(&mut self) {
        if let Some(mut observation) = self.observation.take() {
            observation.finish();
        }
    }
}

impl http_body::Body for ObservedBody {
    type Data = Bytes;
    type Error = s3s::StdError;

    fn poll_frame(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        match Pin::new(&mut this.inner).poll_frame(context) {
            Poll::Ready(Some(Ok(frame))) => {
                if this.inner.is_end_stream() {
                    this.finish();
                }
                Poll::Ready(Some(Ok(frame)))
            }
            Poll::Ready(Some(Err(error))) => {
                if let Some(mut observation) = this.observation.take() {
                    observation.body_error();
                }
                Poll::Ready(Some(Err(error)))
            }
            Poll::Ready(None) => {
                this.finish();
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

impl Drop for ObservedBody {
    fn drop(&mut self) {
        if let Some(mut observation) = self.observation.take() {
            observation.body_abort();
        }
    }
}

fn describe_metrics(recorder: &impl Recorder) {
    describe_counter(
        recorder,
        "crab_s3_gateway_http_requests_total",
        "S3 HTTP requests by bounded method and outcome.",
    );
    describe_gauge(
        recorder,
        "crab_s3_gateway_http_in_flight_requests",
        "S3 requests whose request or response body is still active.",
    );
    recorder.describe_histogram(
        KeyName::from_const_str("crab_s3_gateway_http_request_duration_seconds"),
        Some(Unit::Seconds),
        "Full S3 request and response-body lifetime.".into(),
    );
    describe_counter(
        recorder,
        "crab_s3_gateway_http_response_body_errors_total",
        "Response streams that failed after headers were produced.",
    );
    describe_counter(
        recorder,
        "crab_s3_gateway_http_response_body_aborts_total",
        "Response streams dropped before their body completed.",
    );
    describe_gauge(
        recorder,
        "crab_s3_gateway_admission_in_flight_requests",
        "Requests holding an admission permit.",
    );
    describe_gauge(
        recorder,
        "crab_s3_gateway_admission_capacity",
        "Configured per-process admission capacity.",
    );
    describe_gauge(
        recorder,
        "crab_s3_gateway_admission_queued_requests",
        "Requests waiting for an admission permit.",
    );
    describe_gauge(
        recorder,
        "crab_s3_gateway_admission_queue_capacity",
        "Configured per-process admission queue capacity.",
    );
    describe_counter(
        recorder,
        "crab_s3_gateway_admission_events_total",
        "Admission decisions by request class and bounded outcome.",
    );
    describe_counter(
        recorder,
        "crab_s3_gateway_multipart_maintenance_cycles_total",
        "Multipart maintenance cycles by aggregate outcome.",
    );
    describe_counter(
        recorder,
        "crab_s3_gateway_multipart_maintenance_actions_total",
        "Multipart lifecycle actions completed by background maintenance.",
    );
    describe_counter(
        recorder,
        "crab_s3_gateway_multipart_maintenance_failures_total",
        "Multipart maintenance failures by bounded reason.",
    );
    recorder.describe_histogram(
        KeyName::from_const_str("crab_s3_gateway_multipart_maintenance_cycle_duration_seconds"),
        Some(Unit::Seconds),
        "Full multipart maintenance cycle duration.".into(),
    );
    describe_gauge(
        recorder,
        "crab_s3_gateway_multipart_maintenance_last_success_timestamp_seconds",
        "Unix timestamp of the last cycle with no repository failures.",
    );
}

fn describe_counter(recorder: &impl Recorder, name: &'static str, description: &'static str) {
    recorder.describe_counter(KeyName::from_const_str(name), None, description.into());
}

fn describe_gauge(recorder: &impl Recorder, name: &'static str, description: &'static str) {
    recorder.describe_gauge(KeyName::from_const_str(name), None, description.into());
}

fn key(name: &'static str, labels: &[(&'static str, &'static str)]) -> Key {
    Key::from_parts(
        name,
        labels
            .iter()
            .map(|(name, value)| Label::from_static_parts(name, value))
            .collect::<Vec<_>>(),
    )
}

fn labeled_counters<const N: usize>(
    recorder: &impl Recorder,
    metric: &'static str,
    label: &'static str,
    values: [&'static str; N],
) -> [Counter; N] {
    values.map(|value| recorder.register_counter(&key(metric, &[(label, value)]), &METADATA))
}

fn saturating_u64(value: usize) -> u64 {
    u64::try_from(value).unwrap_or(u64::MAX)
}

fn method_index(method: &Method) -> usize {
    match *method {
        Method::GET => 0,
        Method::HEAD => 1,
        Method::PUT => 2,
        Method::POST => 3,
        Method::DELETE => 4,
        _ => 5,
    }
}

fn status_outcome(status: StatusCode) -> usize {
    match status.as_u16() / 100 {
        1 => 0,
        2 => 1,
        3 => 2,
        4 => 3,
        5 => 4,
        _ => 5,
    }
}

#[cfg(test)]
mod tests;
