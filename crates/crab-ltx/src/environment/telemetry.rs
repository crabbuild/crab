//! Object-store-only telemetry and scratch-monitor contracts.

use std::{io, time::Duration};

/// Finite replica phases exposed to an embedding runtime.
#[cfg(feature = "replica")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LtxPhase {
    /// Local WAL capture.
    Capture,
    /// Managed-database preparation before capture.
    Preparation,
    /// Managed control-table schema validation.
    SchemaCheck,
    /// Confirming the WAL holds a readable frame.
    WalExistence,
    /// Resolving the checksum-linked WAL position.
    PositionResolution,
    /// Reading and parsing the WAL image.
    WalRead,
    /// Collecting the committed page map.
    PageCollection,
    /// Validating WAL or produced LTX data.
    Verification,
    /// Encoding LTX bytes and page records.
    Encode,
    /// Writing LTX and index bytes locally.
    LocalWrite,
    /// Syncing completed LTX contents.
    Fsync,
    /// Syncing the published name's parent directory.
    ParentSync,
    /// Checkpoint maintenance for the capture.
    Checkpoint,
    /// Opening an exact root.
    RootOpen,
    /// Reading root directory pages.
    Directory,
    /// Fetching segment frames from the provider.
    FrameFetch,
    /// Writing restored pages locally.
    RestoreWrite,
    /// Compacting segments.
    Compaction,
}

/// Finite origin-read classes exposed to an embedding runtime.
#[cfg(feature = "replica")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LtxReadOrigin {
    /// A read that had to reach the provider.
    Cold,
    /// A read served by the sparse local activation.
    Sparse,
    /// A read that triggered hydration.
    Hydrating,
    /// A read served by a fully resident local database.
    Resident,
}

/// Finite provider-attempt outcomes exposed to an embedding runtime.
#[cfg(feature = "replica")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LtxRequestOutcome {
    /// The provider attempt returned bytes.
    Succeeded,
    /// The provider attempt failed.
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
    /// Rechecks host disk pressure, failing when the reservation cannot be
    /// admitted alongside the process-wide scratch a job needs.
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
