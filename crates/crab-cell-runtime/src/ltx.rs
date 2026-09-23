//! LTX surface the runtime exposes to embedders.
//!
//! `crab-cell-host` and product servers may not depend on `crab-ltx` directly,
//! so the runtime re-exports the exact LTX types its own API uses.

pub use crab_ltx::{
    CaptureTiming, CellObjectKind, CellReplica, CellStorageLayout, DiskBudget, DiskReservation,
    Host, Limits, LtxPhase, LtxReadOrigin, LtxRequestOutcome, ScratchMonitor,
};
