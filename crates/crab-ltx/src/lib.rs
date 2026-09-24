// Contains adapted Celld lib.rs source at the revision in UPSTREAM.md.
// Apache-2.0; modified by Crab contributors. See LICENSE.

//! Local SQLite WAL capture and exact, checksum-verified LTX recovery.
//!
//! Local capture is synchronous; use a dedicated database thread or blocking
//! executor. The optional `replica` feature adds Cell-root publication,
//! authenticated bundles, compaction, and sparse paged SQL. Leases and HTTP
//! policy remain caller-owned.

#![doc = include_str!("../README.md")]

pub mod capture;
#[cfg(feature = "replica")]
mod cell_layout;
mod codec;
mod commit;
pub mod db;
pub mod environment;
pub mod error;
#[cfg(feature = "replica")]
mod hex;
mod host;
#[cfg(feature = "replica")]
pub use cell_layout::{CellObjectKind, CellStorageLayout};
#[cfg(feature = "replica")]
pub use environment::{DirectoryCacheStats, ScratchMonitor};
pub use environment::{DiskBudget, DiskBudgetAdmission, DiskReservation, Host};
#[cfg(feature = "replica")]
pub use environment::{
    HostResourceAdmission, HostResourceKind, HostResourcePermit, LtxPhase, LtxReadOrigin,
    LtxRequestOutcome, LtxTelemetry,
};
mod ltx;
mod lz4_block;
mod pages;
pub mod recovery;
pub mod types;
mod wal;

#[cfg(feature = "replica")]
pub mod bundle;
#[cfg(feature = "replica")]
mod node_frame;
#[cfg(feature = "replica")]
mod paged;
#[cfg(feature = "replica")]
mod paged_io;
#[cfg(feature = "replica")]
mod replica;
#[cfg(feature = "replica")]
pub use paged_io::with_paged_io_deadline;
#[cfg(feature = "replica")]
mod writable_vfs;
#[cfg(feature = "replica")]
pub use node_frame::{NodeFrameScope, VerifiedNodeFrame, encode_node_frame, inspect_node_frame};
#[cfg(feature = "replica")]
pub use replica::{
    CellPagedDatabase, CellReplica, CellWritableDatabase, PreparedRoot, RecoveryOverlay,
    RootObjectRef, RootRef, VerifiedRoot,
};
#[cfg(feature = "replica")]
pub use writable_vfs::Hydration;

#[cfg(all(test, feature = "replica"))]
mod format_tests;

pub use capture::CheckpointMode;
pub use db::{Db, MANAGED_CONNECTION_PAGE_CACHE_BYTES, MANAGED_SQLITE_CONNECTIONS};
pub use error::{CrabError, LimitKind, QueryError, Result, TransactionError};
pub use recovery::{VerifiedPlan, compact_exact, restore_exact};
pub use rusqlite;
pub use types::{CaptureBatch, CaptureTiming, Limits, LocalSegment, Position, SegmentInfo};

use host::{HostFile, LtxHost};
use types::{CHECKSUM_FLAG, Checksum, Pos, Txid};

const META_DIR_SUFFIX: &str = "-crab-ltx";
const CHECKPOINT_MODE_PASSIVE: &str = "PASSIVE";
const CHECKPOINT_MODE_TRUNCATE: &str = "TRUNCATE";
const WAL_HEADER_SIZE: usize = 32;
const WAL_FRAME_HEADER_SIZE: usize = 24;

fn ltx_file_path(root: &str, level: u32, min: Txid, max: Txid) -> String {
    format!("{root}/ltx/{level}/{min}-{max}.ltx")
}

// Derived from Celld's lib.rs at the revision in UPSTREAM.md (Apache-2.0).
// Only validated WAL headers and pages enter this private checksum routine.
fn wal_checksum(big_endian: bool, mut s0: u32, mut s1: u32, bytes: &[u8]) -> (u32, u32) {
    for chunk in bytes.as_chunks::<8>().0 {
        let a = [chunk[0], chunk[1], chunk[2], chunk[3]];
        let b = [chunk[4], chunk[5], chunk[6], chunk[7]];
        let (a, b) = if big_endian {
            (u32::from_be_bytes(a), u32::from_be_bytes(b))
        } else {
            (u32::from_le_bytes(a), u32::from_le_bytes(b))
        };
        s0 = s0.wrapping_add(a).wrapping_add(s1);
        s1 = s1.wrapping_add(b).wrapping_add(s0);
    }
    (s0, s1)
}
