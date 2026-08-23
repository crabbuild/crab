//! Coordination contracts for Crab active-active writes.

pub mod active_active;
pub mod active_active_runtime;
pub mod cosmosdb_coordinator;
pub mod dynamodb_coordinator;
pub mod error;
#[cfg(feature = "object-store-lock")]
pub mod gc_fence;
#[cfg(feature = "object-store-lock")]
pub mod push_admission;
#[cfg(feature = "object-store-lock")]
pub mod push_lock;
pub mod spanner_coordinator;
pub mod write_coordinator;

#[cfg(test)]
mod active_active_tests;

pub use active_active::*;
pub use active_active_runtime::*;
pub use error::{CoordinationError, Result};
#[cfg(feature = "object-store-lock")]
pub use gc_fence::*;
#[cfg(feature = "object-store-lock")]
pub use push_admission::*;
#[cfg(feature = "object-store-lock")]
pub use push_lock::*;
pub use write_coordinator::*;
