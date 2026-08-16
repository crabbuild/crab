//! Shared building blocks: config, metrics, progress, error types.

pub mod config;
pub mod context;
pub(crate) mod cow_clone;
pub mod credential_discovery;
pub mod error;
pub mod error_catalog;
pub mod first_run;
pub mod fuse_prereq {
    pub use crab_vfs::fuse_prereq::*;
}
pub mod metrics;
pub mod output;
pub mod pattern;
pub mod perf_phase;
pub mod project_config;
pub mod style;
pub mod tracing;
pub mod tracing_init;

// Consolidated attributes + pathspec readers that replace the four
// independent hand-rolled glob engines scattered across `cmd/`,
// `git/`, and `lfs/`. Gated behind `gix-pathmatch` while the legacy
// engines stay reachable during rollout.
#[cfg(feature = "gix-pathmatch")]
pub mod attrs;
#[cfg(feature = "gix-pathmatch")]
pub mod pathmatch;

// Layered git-config reader (system → global → local → worktree →
// env → CLI). Shells out to `git config --get` collapse onto this
// resolver under the `gix-config` flag; the legacy shellouts stay
// reachable on the default build.
#[cfg(feature = "gix-config")]
pub mod config_resolver;

pub use config::{CompressionConfig, Config, ConfigOverlay, EngineConfig, StagingConfig};
pub use context::AppContext;
pub use error::{CrabError, Result, check_cancelled};
pub use metrics::Metrics;
pub use project_config::ProjectConfig;
