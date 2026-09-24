//! Host hook, executor, and worker admission tests.

#![cfg(feature = "replica")]

mod host {
    pub mod admissions;
    pub mod hooks;
}
