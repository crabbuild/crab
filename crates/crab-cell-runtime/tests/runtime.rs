//! Cell lifecycle, durability, and publication integration tests.
//!
//! The suite is one test binary. Its modules live in `tests/runtime/`; shared
//! fixtures stay in `tests/support/` and are reached through the crate root, so
//! no target needs a `#[path]` attribute.

mod support;

mod runtime {
    pub mod backup;
    pub mod catalog;
    pub mod lifecycle;
    pub mod migration;
    pub mod publication;
    pub mod release_progress;
    pub mod scheduler;
    pub mod scheduler_properties;
    pub mod workers;
}
