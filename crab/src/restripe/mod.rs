//! Xorb-level restripe subsystem — rewrites xorbs to a target size and
//! grouping profile for cost and performance optimization.
//!
//! This is **not** `cmd::repack` (git-pack consolidation). `restripe`
//! operates on content-addressed xorbs, not git packs. The existing
//! `crab repack` command and its `RepackConfig` are untouched.
//!
//! # Submodules
//!
//! - [`profile`] — `Profile` type + built-in profiles (`ml`, `dataset`,
//!   `code`) + validation.
//! - [`inference`] — auto-select profile from `RepoStats`.
//! - [`planner`] — dry-run estimator (destination count, bytes,
//!   wall-clock, API cost).
//! - [`executor`] — streaming source-xorb → dest-xorb pipeline (stub).
//! - [`journal`] — WAL-mode SQLite journal for crash-safe resume.
//! - [`reconcile`] — online reconciliation with concurrent pushes (stub).

pub mod executor;
pub mod inference;
pub mod journal;
pub mod planner;
pub mod profile;
pub mod reconcile;

/// Create an OTLP span for a restripe operation when the `otlp`
/// feature is enabled. Returns a span guard that should be held for
/// the duration of the operation.
///
/// When `otlp` is not enabled this is a no-op that the compiler
/// eliminates entirely.
#[cfg(feature = "otlp")]
pub fn restripe_span(profile: &str, dry_run: bool) -> tracing::span::EnteredSpan {
    tracing::info_span!(
        "restripe",
        command = "restripe",
        profile = %profile,
        dry_run = %dry_run,
    )
    .entered()
}
