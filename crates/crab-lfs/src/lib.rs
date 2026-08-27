//! Git LFS object storage contracts for Crab.
//!
//! This crate owns the object-store layout and integrity checks for Git LFS
//! object bytes. Git LFS pointer parsing stays in `crab-git`; CLI command
//! output, transfer-agent protocol handling, and lifecycle commands stay in
//! higher crates.

pub mod lock;
pub mod object_store;

pub use lock::{LfsLockError, LfsLockManager, LockRecord, LockResult};
pub use object_store::{LfsByteStream, LfsError, LfsObjectStore, Result};
