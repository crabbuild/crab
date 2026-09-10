use std::sync::Arc;

use crab_storage::{StorageObservation, StorageObserver, StorageOperation, StorageOutcome};
use metrics::{Counter, Gauge, Histogram, KeyName, Recorder, Unit};

use super::{METADATA, key};

#[derive(Clone)]
pub(super) struct BackendMetrics {
    operations: [OperationMetrics; StorageOperation::ALL.len()],
}

#[derive(Clone)]
struct OperationMetrics {
    requests: [Counter; StorageOutcome::ALL.len()],
    in_flight: Gauge,
    duration: Histogram,
    bytes_read: Counter,
    bytes_written: Counter,
}

impl BackendMetrics {
    pub(super) fn new(recorder: &impl Recorder) -> Self {
        describe(recorder);
        Self {
            operations: StorageOperation::ALL
                .map(|operation| OperationMetrics::new(recorder, operation.label())),
        }
    }

    pub(super) fn observer(&self) -> Arc<dyn StorageObserver> {
        Arc::new(self.clone())
    }
}

impl StorageObserver for BackendMetrics {
    fn started(&self, operation: StorageOperation) {
        self.operations[operation.index()].in_flight.increment(1);
    }

    fn finished(&self, observation: StorageObservation) {
        let operation = &self.operations[observation.operation.index()];
        operation.requests[observation.outcome.index()].increment(1);
        operation
            .duration
            .record(observation.duration.as_secs_f64());
        operation.bytes_read.increment(observation.bytes_read);
        operation.bytes_written.increment(observation.bytes_written);
        operation.in_flight.decrement(1);
    }
}

impl OperationMetrics {
    fn new(recorder: &impl Recorder, operation: &'static str) -> Self {
        Self {
            requests: StorageOutcome::ALL.map(|outcome| {
                recorder.register_counter(
                    &key(
                        "crab_s3_gateway_backend_requests_total",
                        &[("operation", operation), ("outcome", outcome.label())],
                    ),
                    &METADATA,
                )
            }),
            in_flight: recorder.register_gauge(
                &key(
                    "crab_s3_gateway_backend_in_flight_requests",
                    &[("operation", operation)],
                ),
                &METADATA,
            ),
            duration: recorder.register_histogram(
                &key(
                    "crab_s3_gateway_backend_request_duration_seconds",
                    &[("operation", operation)],
                ),
                &METADATA,
            ),
            bytes_read: recorder.register_counter(
                &key(
                    "crab_s3_gateway_backend_bytes_read_total",
                    &[("operation", operation)],
                ),
                &METADATA,
            ),
            bytes_written: recorder.register_counter(
                &key(
                    "crab_s3_gateway_backend_bytes_written_total",
                    &[("operation", operation)],
                ),
                &METADATA,
            ),
        }
    }
}

fn describe(recorder: &impl Recorder) {
    recorder.describe_counter(
        KeyName::from_const_str("crab_s3_gateway_backend_requests_total"),
        None,
        "Logical object-store operations by bounded operation and outcome.".into(),
    );
    recorder.describe_gauge(
        KeyName::from_const_str("crab_s3_gateway_backend_in_flight_requests"),
        None,
        "Logical object-store operations that have not reached a terminal result.".into(),
    );
    recorder.describe_histogram(
        KeyName::from_const_str("crab_s3_gateway_backend_request_duration_seconds"),
        Some(Unit::Seconds),
        "Full logical object-store operation and response-stream lifetime.".into(),
    );
    recorder.describe_counter(
        KeyName::from_const_str("crab_s3_gateway_backend_bytes_read_total"),
        Some(Unit::Bytes),
        "Object body bytes yielded by logical backend reads.".into(),
    );
    recorder.describe_counter(
        KeyName::from_const_str("crab_s3_gateway_backend_bytes_written_total"),
        Some(Unit::Bytes),
        "Payload bytes accepted by successful logical backend writes and multipart parts.".into(),
    );
}
