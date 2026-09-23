//! Object-store-only host resource admission contracts.

/// Resource class charged by an embedding runtime for replica-host work.
#[cfg(feature = "replica")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostResourceKind {
    /// One bounded object-store or immutable-file I/O operation.
    Io,
    /// One blocking host job dispatched to the replica executor.
    BlockingJob,
    /// One full recovery or restore cohort.
    Recovery,
    /// One capture/compaction dirty-memory cohort.
    Dirty,
    /// One MiB of temporary scratch admission.
    Scratch,
}

/// Admission hook used by an embedding runtime to charge host work to its
/// node-wide resource ledger. The returned permit owns the charge until drop.
#[cfg(feature = "replica")]
pub trait HostResourceAdmission: Send + Sync {
    /// Reserves `units` of one host resource without waiting.
    fn reserve(
        &self,
        kind: HostResourceKind,
        units: u32,
    ) -> crate::Result<Box<dyn HostResourcePermit>>;
}

/// Opaque lifetime token returned by [`HostResourceAdmission::reserve`].
#[cfg(feature = "replica")]
pub trait HostResourcePermit: Send + Sync {}
