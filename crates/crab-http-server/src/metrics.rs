use std::{
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
    time::Instant,
};

use axum::{
    body::Body,
    http::{Method, StatusCode},
};
use bytes::Bytes;
use http_body::{Body as _, Frame, SizeHint};
use metrics::{Counter, Gauge, Histogram, Key, KeyName, Label, Level, Metadata, Recorder, Unit};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

const METHOD_COUNT: usize = 6;
const OUTCOME_COUNT: usize = 7;
const ADMISSION_COUNT: usize = 4;
const TRANSFER_REJECTION_COUNT: usize = 2;
const DURATION_BUCKETS_SECONDS: [f64; 16] = [
    0.005, 0.010, 0.025, 0.050, 0.100, 0.250, 0.500, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0,
    600.0,
];
const METHOD_LABELS: [&str; METHOD_COUNT] = ["get", "head", "put", "post", "delete", "other"];
const OUTCOME_LABELS: [&str; OUTCOME_COUNT] =
    ["1xx", "2xx", "3xx", "4xx", "5xx", "other", "cancelled"];
pub(crate) const ADMISSION_LABELS: [&str; ADMISSION_COUNT] =
    ["read", "git_transfer", "application", "maintenance"];
const TRANSFER_REJECTION_LABELS: [&str; TRANSFER_REJECTION_COUNT] = ["capacity", "coordination"];
const METADATA: Metadata<'static> = Metadata::new(
    "crab_http_server",
    Level::INFO,
    Some("crab_http_server::metrics"),
);

#[derive(Clone)]
pub(crate) struct Metrics {
    inner: Arc<MetricsInner>,
}

struct MetricsInner {
    handle: PrometheusHandle,
    methods: [MethodMetrics; METHOD_COUNT],
    admission: [AdmissionMetrics; ADMISSION_COUNT],
    repositories: Gauge,
    catalog_healthy: Gauge,
    draining: Gauge,
    receive_workers: Gauge,
    catalog_refresh_failures: Counter,
    transfer_admission_rejections: [Counter; TRANSFER_REJECTION_COUNT],
}

struct MethodMetrics {
    requests: [Counter; OUTCOME_COUNT],
    in_flight: Gauge,
    duration: Histogram,
    body_errors: Counter,
    body_aborts: Counter,
}

struct AdmissionMetrics {
    available: Gauge,
    capacity: Gauge,
}

pub(crate) struct RuntimeSnapshot {
    pub(crate) repositories: usize,
    pub(crate) catalog_healthy: bool,
    pub(crate) draining: bool,
    pub(crate) receive_workers: usize,
    pub(crate) admission_available: [usize; ADMISSION_COUNT],
    pub(crate) admission_capacity: [usize; ADMISSION_COUNT],
}

impl Metrics {
    pub(crate) fn new() -> Result<Self, metrics_exporter_prometheus::BuildError> {
        let recorder = PrometheusBuilder::new()
            .set_buckets(&DURATION_BUCKETS_SECONDS)?
            .build_recorder();
        describe_metrics(&recorder);
        let methods = METHOD_LABELS.map(|method| MethodMetrics::new(&recorder, method));
        let admission = ADMISSION_LABELS.map(|class| AdmissionMetrics::new(&recorder, class));
        Ok(Self {
            inner: Arc::new(MetricsInner {
                handle: recorder.handle(),
                methods,
                admission,
                repositories: recorder.register_gauge(
                    &Key::from_static_name("crab_http_server_repositories"),
                    &METADATA,
                ),
                catalog_healthy: recorder.register_gauge(
                    &Key::from_static_name("crab_http_server_catalog_healthy"),
                    &METADATA,
                ),
                draining: recorder.register_gauge(
                    &Key::from_static_name("crab_http_server_draining"),
                    &METADATA,
                ),
                receive_workers: recorder.register_gauge(
                    &Key::from_static_name("crab_http_server_receive_workers"),
                    &METADATA,
                ),
                catalog_refresh_failures: recorder.register_counter(
                    &Key::from_static_name("crab_http_server_catalog_refresh_failures_total"),
                    &METADATA,
                ),
                transfer_admission_rejections: TRANSFER_REJECTION_LABELS.map(|reason| {
                    recorder.register_counter(
                        &key(
                            "crab_http_server_transfer_admission_rejections_total",
                            &[("reason", reason)],
                        ),
                        &METADATA,
                    )
                }),
            }),
        })
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

    pub(crate) fn record_catalog_refresh_failure(&self) {
        self.inner.catalog_refresh_failures.increment(1);
    }

    pub(crate) fn record_transfer_admission_rejection(&self, coordination: bool) {
        self.inner.transfer_admission_rejections[usize::from(coordination)].increment(1);
    }

    pub(crate) fn render(&self, snapshot: RuntimeSnapshot) -> String {
        self.inner.repositories.set(snapshot.repositories as f64);
        self.inner
            .catalog_healthy
            .set(f64::from(snapshot.catalog_healthy));
        self.inner.draining.set(f64::from(snapshot.draining));
        self.inner
            .receive_workers
            .set(snapshot.receive_workers as f64);
        for (index, admission) in self.inner.admission.iter().enumerate() {
            admission
                .available
                .set(snapshot.admission_available[index] as f64);
            admission
                .capacity
                .set(snapshot.admission_capacity[index] as f64);
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
                        "crab_http_server_requests_total",
                        &[("method", method), ("outcome", outcome)],
                    ),
                    &METADATA,
                )
            }),
            in_flight: recorder.register_gauge(
                &key("crab_http_server_in_flight_requests", &[("method", method)]),
                &METADATA,
            ),
            duration: recorder.register_histogram(
                &key(
                    "crab_http_server_request_duration_seconds",
                    &[("method", method)],
                ),
                &METADATA,
            ),
            body_errors: recorder.register_counter(
                &key(
                    "crab_http_server_response_body_errors_total",
                    &[("method", method)],
                ),
                &METADATA,
            ),
            body_aborts: recorder.register_counter(
                &key(
                    "crab_http_server_response_body_aborts_total",
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
            available: recorder.register_gauge(
                &key(
                    "crab_http_server_admission_available_permits",
                    &[("class", class)],
                ),
                &METADATA,
            ),
            capacity: recorder.register_gauge(
                &key("crab_http_server_admission_capacity", &[("class", class)]),
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
            self.metrics.record_request(self.method, 6);
            self.outcome_recorded = true;
        }
        self.finish();
    }
}

pub(crate) struct ObservedBody {
    inner: Body,
    observation: Option<RequestObservation>,
}

impl ObservedBody {
    pub(crate) fn new(inner: Body, mut observation: RequestObservation) -> Self {
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
    type Error = axum::Error;

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
        "crab_http_server_requests_total",
        "Public HTTP requests by bounded method and response class.",
    );
    describe_gauge(
        recorder,
        "crab_http_server_in_flight_requests",
        "Public requests whose handler or response body is still active.",
    );
    recorder.describe_histogram(
        KeyName::from_const_str("crab_http_server_request_duration_seconds"),
        Some(Unit::Seconds),
        "Full public request and response-body lifetime.".into(),
    );
    describe_counter(
        recorder,
        "crab_http_server_response_body_errors_total",
        "Response streams that failed after headers were produced.",
    );
    describe_counter(
        recorder,
        "crab_http_server_response_body_aborts_total",
        "Response streams dropped before their body completed.",
    );
    describe_gauge(
        recorder,
        "crab_http_server_admission_available_permits",
        "Available fast-path permits in this process; Git transfers also require a deployment-wide storage lease.",
    );
    describe_gauge(
        recorder,
        "crab_http_server_admission_capacity",
        "Configured permits in each process-local admission class.",
    );
    describe_gauge(
        recorder,
        "crab_http_server_repositories",
        "Repositories loaded from the durable catalog.",
    );
    describe_gauge(
        recorder,
        "crab_http_server_catalog_healthy",
        "Whether the latest catalog refresh and storage probe succeeded.",
    );
    describe_gauge(
        recorder,
        "crab_http_server_draining",
        "Whether graceful shutdown has started.",
    );
    describe_gauge(
        recorder,
        "crab_http_server_receive_workers",
        "Receive workers retained for publication or cleanup.",
    );
    describe_counter(
        recorder,
        "crab_http_server_catalog_refresh_failures_total",
        "Catalog refresh attempts that failed or moved backwards.",
    );
    describe_counter(
        recorder,
        "crab_http_server_transfer_admission_rejections_total",
        "Transfers rejected by deployment-wide capacity or coordination failures.",
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
mod tests {
    use super::*;
    use http_body_util::BodyExt as _;

    fn snapshot() -> RuntimeSnapshot {
        RuntimeSnapshot {
            repositories: 2,
            catalog_healthy: true,
            draining: false,
            receive_workers: 1,
            admission_available: [16, 3, 8, 1],
            admission_capacity: [16, 4, 8, 2],
        }
    }

    #[tokio::test]
    async fn completed_request_exports_bounded_full_body_metrics() {
        let metrics = Metrics::new().unwrap();
        metrics.record_transfer_admission_rejection(false);
        metrics.record_transfer_admission_rejection(true);
        let observation = metrics.start_request(&Method::GET).response(StatusCode::OK);
        let body = Body::new(ObservedBody::new(Body::from("response"), observation));
        assert_eq!(
            body.collect().await.unwrap().to_bytes(),
            Bytes::from_static(b"response")
        );

        let rendered = metrics.render(snapshot());
        assert!(
            rendered.contains("crab_http_server_requests_total{method=\"get\",outcome=\"2xx\"} 1")
        );
        assert!(rendered.contains("crab_http_server_in_flight_requests{method=\"get\"} 0"));
        assert!(
            rendered.contains("crab_http_server_request_duration_seconds_count{method=\"get\"} 1")
        );
        assert!(rendered.contains("crab_http_server_catalog_healthy 1"));
        assert!(rendered.contains("crab_http_server_repositories 2"));
        assert!(
            rendered
                .contains("crab_http_server_admission_available_permits{class=\"git_transfer\"} 3")
        );
        assert!(rendered.contains(
            "crab_http_server_transfer_admission_rejections_total{reason=\"capacity\"} 1"
        ));
        assert!(rendered.contains(
            "crab_http_server_transfer_admission_rejections_total{reason=\"coordination\"} 1"
        ));
        assert!(!rendered.contains("repository=\""));
    }

    #[test]
    fn dropped_response_body_records_abort_and_releases_in_flight_gauge() {
        let metrics = Metrics::new().unwrap();
        let observation = metrics.start_request(&Method::GET).response(StatusCode::OK);
        drop(ObservedBody::new(Body::from("response"), observation));

        let rendered = metrics.render(snapshot());
        assert!(rendered.contains("crab_http_server_response_body_aborts_total{method=\"get\"} 1"));
        assert!(rendered.contains("crab_http_server_in_flight_requests{method=\"get\"} 0"));
    }

    #[tokio::test]
    async fn failed_response_body_records_stream_error() {
        let metrics = Metrics::new().unwrap();
        let observation = metrics.start_request(&Method::GET).response(StatusCode::OK);
        let stream =
            futures_util::stream::iter([Err::<Bytes, _>(std::io::Error::other("stream failed"))]);
        let mut body = Body::new(ObservedBody::new(Body::from_stream(stream), observation));

        assert!(body.frame().await.unwrap().is_err());
        let rendered = metrics.render(snapshot());
        assert!(rendered.contains("crab_http_server_response_body_errors_total{method=\"get\"} 1"));
        assert!(rendered.contains("crab_http_server_in_flight_requests{method=\"get\"} 0"));
    }

    #[test]
    fn request_cancelled_before_response_is_counted_and_released() {
        let metrics = Metrics::new().unwrap();
        drop(metrics.start_request(&Method::PUT));

        let rendered = metrics.render(snapshot());
        assert!(
            rendered.contains(
                "crab_http_server_requests_total{method=\"put\",outcome=\"cancelled\"} 1"
            )
        );
        assert!(rendered.contains("crab_http_server_in_flight_requests{method=\"put\"} 0"));
    }
}
