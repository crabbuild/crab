//! Injectable local I/O, clocks and jobs adapted from Celld host.rs.
//!
//! Object-store-only resources, telemetry, directory cache, and executors are
//! gated at the module boundary instead of per item.

#[cfg(feature = "replica")]
pub mod directory_cache;
#[cfg(feature = "replica")]
pub mod executor;
pub mod host;
#[cfg(feature = "replica")]
pub mod resources;
#[cfg(feature = "replica")]
pub mod telemetry;

#[cfg(feature = "replica")]
pub use directory_cache::DirectoryCacheStats;
#[cfg(feature = "replica")]
pub use executor::{Executor, Worker};
pub use host::{
    Clock, DirectFileSystem, DiskBudget, DiskBudgetAdmission, DiskReservation, FileIo, FileSystem,
    Host, SystemClock,
};
#[cfg(feature = "replica")]
pub use resources::{HostResourceAdmission, HostResourceKind, HostResourcePermit};
#[cfg(feature = "replica")]
pub use telemetry::{LtxPhase, LtxReadOrigin, LtxRequestOutcome, LtxTelemetry, ScratchMonitor};

#[cfg(test)]
mod tests;
