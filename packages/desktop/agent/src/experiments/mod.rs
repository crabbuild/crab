//! Experiment-tracking bridge.
//!
//! Provides a provider-agnostic interface to ML experiment trackers
//! (MLflow, W&B). Each provider implements [`ExperimentProvider`] and
//! is selected per-workspace via stored credentials. The four verbs
//! exposed to the renderer are:
//!
//! - `experiments_runs_for_commit` — runs tagged with a given commit
//! - `experiments_runs_recent` — paginated recent runs
//! - `experiments_run_detail` — full metrics + hyperparams for one run
//! - `experiments_configure` — test + persist provider credentials

pub mod mlflow;
pub mod provider;
pub mod wandb;

pub use provider::{ExperimentProvider, ProviderConfig, Run, RunDetail, RunStatus};
