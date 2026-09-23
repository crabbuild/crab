//! Replica-only telemetry and scratch-monitor contracts.

use std::{io, time::Duration};

/// Finite replica phases exposed to an embedding runtime.
#[cfg(feature = "replica")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LtxPhase {
    Capture,
    Preparation,
    SchemaCheck,
    WalExistence,
    PositionResolution,
    WalRead,
    PageCollection,
    Verification,
    Encode,
    LocalWrite,
    Fsync,
    ParentSync,
    Checkpoint,
    RootOpen,
    Directory,
    FrameFetch,
    RestoreWrite,
    Compaction,
}

/// Finite origin-read classes exposed to an embedding runtime.
#[cfg(feature = "replica")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LtxReadOrigin {
    Cold,
    Sparse,
    Hydrating,
    Resident,
}

/// Finite provider-attempt outcomes exposed to an embedding runtime.
#[cfg(feature = "replica")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LtxRequestOutcome {
    Succeeded,
    Failed,
}

/// Non-blocking, bounded-cardinality observations emitted by replica work.
#[cfg(feature = "replica")]
pub trait LtxTelemetry: Send + Sync {
    /// Records one completed phase and whether it succeeded.
    fn phase(&self, _phase: LtxPhase, _elapsed: Duration, _succeeded: bool) {}

    /// Records one logical read issued by the runtime or a paged database.
    fn logical_read(&self, _origin: LtxReadOrigin) {}

    /// Records one provider attempt and bytes returned before its outcome.
    fn origin_request(&self, _origin: LtxReadOrigin, _outcome: LtxRequestOutcome, _bytes: u64) {}

    /// Records one complete capture attempt, including failed attempts.
    fn capture(&self, _timing: &crate::CaptureTiming, _succeeded: bool) {}
}

/// Rechecks host disk pressure after full-job scratch admission.
///
/// `reserved_bytes` is the process-wide scratch reservation, including the
/// current job. An embedding service can combine it with other local-disk
/// reservations and an operator reserve before allowing remote downloads.
#[cfg(feature = "replica")]
pub trait ScratchMonitor: Send + Sync {
    fn ensure_available(&self, reserved_bytes: u64) -> io::Result<()>;
}

#[cfg(feature = "replica")]
pub(crate) struct UnlimitedScratch;

#[cfg(feature = "replica")]
impl ScratchMonitor for UnlimitedScratch {
    fn ensure_available(&self, _: u64) -> io::Result<()> {
        Ok(())
    }
}
