use std::time::{Duration, Instant};

/// Bounded metric category emitted by remote repository operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MetricKind {
    /// Complete semantic repository operation.
    Operation,
    /// Manifest or inventory metadata access.
    Metadata,
    /// Object-store request or transferred bytes.
    Storage,
    /// Cache lookup, insertion, or eviction.
    Cache,
    /// Exact object-locator lookup.
    Locator,
    /// Pack inflation or Git parsing.
    Decode,
    /// Full checksum verification, with examined bytes and elapsed duration.
    CacheVerification,
    /// Admitted blocking-job lifetime: no duration starts, a duration finishes.
    CpuJob,
    /// Reserved buffer/result bytes: no duration reserves, a duration releases.
    ResidentBytes,
    /// Uncached attribution lifetime: no duration starts, a duration finishes.
    BlameProducer,
    /// Tree or commit traversal.
    Traversal,
    /// Resource-budget rejection.
    Budget,
    /// Gap between manifest and locator publication.
    PublicationLag,
    /// Explicit or disconnect-driven cancellation.
    Cancellation,
}

/// Bounded semantic outcome for a completed observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MetricOutcome {
    /// Operation completed successfully.
    Success,
    /// Operation failed without caller cancellation.
    Error,
    /// Caller cancellation stopped the operation.
    Cancelled,
}

/// Bounded cache event without key or repository content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CacheOutcome {
    /// Requested immutable value was retained.
    Hit,
    /// Requested immutable value was absent.
    Miss,
    /// A caller joined production already owned by another caller.
    Coalesced,
    /// One or more values were evicted to preserve configured bounds.
    Eviction,
}

/// One low-cardinality runtime observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetricObservation {
    /// Stable bounded metric category.
    pub kind: MetricKind,
    /// Count or byte value associated with the observation.
    pub value: u64,
    /// Optional elapsed duration for timed work.
    pub duration: Option<Duration>,
    /// Semantic completion outcome, when applicable.
    pub outcome: Option<MetricOutcome>,
    /// Cache behavior, when applicable.
    pub cache: Option<CacheOutcome>,
}

/// Sink for runtime observations without repository content in labels.
pub trait RemoteGitMetrics: Send + Sync + 'static {
    /// Record one bounded observation.
    fn record(&self, observation: MetricObservation);
}

pub(crate) struct WorkObservation {
    metrics: std::sync::Arc<dyn RemoteGitMetrics>,
    kind: MetricKind,
    value: u64,
    started: Instant,
}

impl WorkObservation {
    pub(crate) fn new(
        metrics: std::sync::Arc<dyn RemoteGitMetrics>,
        kind: MetricKind,
        value: u64,
    ) -> Self {
        metrics.record(MetricObservation {
            kind,
            value,
            duration: None,
            outcome: None,
            cache: None,
        });
        Self {
            metrics,
            kind,
            value,
            started: Instant::now(),
        }
    }
}

impl Drop for WorkObservation {
    fn drop(&mut self) {
        self.metrics.record(MetricObservation {
            kind: self.kind,
            value: self.value,
            duration: Some(self.started.elapsed()),
            outcome: None,
            cache: None,
        });
    }
}

/// Metrics sink used when callers do not install instrumentation.
#[derive(Debug, Default)]
pub struct NoopMetrics;

impl RemoteGitMetrics for NoopMetrics {
    fn record(&self, _observation: MetricObservation) {}
}

pub(crate) struct BlameProducerObservation<'a> {
    metrics: &'a dyn RemoteGitMetrics,
    started: Instant,
}

impl<'a> BlameProducerObservation<'a> {
    pub(crate) fn new(metrics: &'a dyn RemoteGitMetrics) -> Self {
        metrics.record(MetricObservation {
            kind: MetricKind::BlameProducer,
            value: 1,
            duration: None,
            outcome: None,
            cache: None,
        });
        Self {
            metrics,
            started: Instant::now(),
        }
    }
}

impl Drop for BlameProducerObservation<'_> {
    fn drop(&mut self) {
        // A dropped attribution future still ends its measured lifetime; this
        // does not claim its backend/session cleanup has already drained.
        self.metrics.record(MetricObservation {
            kind: MetricKind::BlameProducer,
            value: 1,
            duration: Some(self.started.elapsed()),
            outcome: None,
            cache: None,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct Observations(Mutex<Vec<MetricObservation>>);

    impl RemoteGitMetrics for Observations {
        fn record(&self, observation: MetricObservation) {
            self.0.lock().unwrap().push(observation);
        }
    }

    #[test]
    fn abandoned_blame_future_ends_its_observed_lifetime() {
        let observations = Observations::default();
        drop(BlameProducerObservation::new(&observations));
        let events = observations.0.lock().unwrap();
        assert_eq!(events.len(), 2);
        assert!(events[0].duration.is_none() && events[1].duration.is_some());
    }
}
