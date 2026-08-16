use std::time::Duration;

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

/// Metrics sink used when callers do not install instrumentation.
#[derive(Debug, Default)]
pub struct NoopMetrics;

impl RemoteGitMetrics for NoopMetrics {
    fn record(&self, _observation: MetricObservation) {}
}
