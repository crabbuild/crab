//! In-process test doubles for storage primitives.
//!
//! Only compiled when the `testing` feature is enabled so the release
//! binary doesn't pull in any of this.

pub mod mock_store;

pub use mock_store::{FailSpec, MockStore};
