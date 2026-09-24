//! Shared integration-test harness.
//!
//! Suites declare `mod support;` and use `crate::support::<helper>`. Only
//! genuinely shared fixtures live here; a fixture used by a single suite stays
//! in that suite's module.

pub mod fencing;
pub mod fixtures;
