//! Test-only helpers shared across the crate's unit tests.
//!
//! Gated behind `#[cfg(test)]` at the `lib.rs` level so none of this
//! ships in release builds. Kept under `src/test/` (rather than as a
//! single top-level file) so individual helpers can grow into their
//! own modules without churning the module tree.

pub mod git_repo;
