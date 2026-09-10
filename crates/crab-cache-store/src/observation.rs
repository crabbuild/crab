//! Bounded cache-routing observations for service metrics.

/// Cache layer consulted by one read attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum CacheSource {
    /// Process-local decoded xorb result cache.
    Memory,
    /// Verified local filesystem cache.
    Local,
    /// Optional remote cache service.
    Service,
}

impl CacheSource {
    /// Every cache layer in stable index order.
    pub const ALL: [Self; 3] = [Self::Memory, Self::Local, Self::Service];

    /// Returns the stable array index for this layer.
    #[must_use]
    pub const fn index(self) -> usize {
        self as usize
    }

    /// Returns the bounded metric label for this layer.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Memory => "memory",
            Self::Local => "local",
            Self::Service => "service",
        }
    }
}

/// Terminal result of one cache-layer read attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum CacheReadOutcome {
    /// The layer returned verified bytes.
    Hit,
    /// The layer had no usable entry.
    Miss,
    /// The layer failed and the caller used the next source when permitted.
    Failure,
}

impl CacheReadOutcome {
    /// Every read outcome in stable index order.
    pub const ALL: [Self; 3] = [Self::Hit, Self::Miss, Self::Failure];

    /// Returns the stable array index for this outcome.
    #[must_use]
    pub const fn index(self) -> usize {
        self as usize
    }

    /// Returns the bounded metric label for this outcome.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Hit => "hit",
            Self::Miss => "miss",
            Self::Failure => "failure",
        }
    }
}

/// Completed cache read without object identity or storage placement.
#[derive(Clone, Copy, Debug)]
pub struct CacheReadObservation {
    /// Cache layer that was consulted.
    pub source: CacheSource,
    /// Terminal result of that attempt.
    pub outcome: CacheReadOutcome,
    /// Verified bytes returned by a hit.
    pub bytes: u64,
}

/// Receives bounded cache-routing events.
pub trait CacheObserver: Send + Sync {
    /// Records one completed cache-layer read attempt.
    fn read(&self, observation: CacheReadObservation);

    /// Records a local-cache persistence failure after verified bytes remain usable.
    fn local_write_failure(&self);
}

#[derive(Debug)]
pub(crate) struct NoopCacheObserver;

impl CacheObserver for NoopCacheObserver {
    fn read(&self, _observation: CacheReadObservation) {}

    fn local_write_failure(&self) {}
}
