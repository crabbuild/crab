//! Pricing subsystem — versioned price tables with optional user overrides.
//!
//! The embedded price table is generated at build time from
//! `pricing/data/<version>.yaml` by `build.rs`. At runtime, users can
//! layer overrides via `cost.pricing_file` in the config.
//!
//! # Submodules
//!
//! - `embedded` — build-time generated price table (`include!`-ed from
//!   `$OUT_DIR/pricing_embedded.rs`).
//! - `override_file` — YAML parse + validation for user override files,
//!   deep-merge onto embedded.

pub mod embedded;
pub mod override_file;
