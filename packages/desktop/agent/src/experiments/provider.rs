//! Experiment provider trait and shared types.
//!
//! Concrete implementations (MLflow, W&B) live in sibling modules and
//! are wired in by tasks 20.2 / 20.3. Until then, the handler layer
//! returns a "not configured" error when no provider is active.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::Result;

/// Status of an experiment run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Completed,
    Failed,
    Killed,
    Unknown,
}

/// Summary of a single experiment run. Returned in list endpoints.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub id: String,
    pub name: String,
    pub commit_sha: Option<String>,
    /// Summary-level metrics (e.g. final loss, accuracy).
    pub metrics: HashMap<String, f64>,
    pub status: RunStatus,
    /// ISO-8601 timestamp string. Using `String` keeps chrono optional
    /// at this layer — providers parse into their native type and
    /// serialize back.
    pub start_time: String,
    pub url: String,
}

/// Extended detail for a single run, including hyperparameters and
/// artifact paths.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunDetail {
    #[serde(flatten)]
    pub run: Run,
    /// Hyperparameter key-value pairs logged with the run.
    pub hyperparams: HashMap<String, String>,
    /// Artifact URIs associated with the run.
    pub artifacts: Vec<String>,
    /// Wall-clock duration in seconds, if available.
    pub duration_secs: Option<f64>,
}

/// Credentials needed to connect to an experiment provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderConfig {
    pub provider_id: String,
    pub base_url: String,
    pub token: String,
}

/// Trait implemented by each experiment-tracking backend.
///
/// Providers are stateless — credentials are passed at construction
/// time and stored in the OS keychain by the `experiments_configure`
/// handler.
#[async_trait::async_trait]
pub trait ExperimentProvider: Send + Sync {
    /// Short identifier for this provider (e.g. `"mlflow"`, `"wandb"`).
    fn id(&self) -> &'static str;

    /// List runs tagged with the given commit SHA.
    async fn runs_for_commit(&self, sha: &str) -> Result<Vec<Run>>;

    /// List the most recent runs, up to `limit`.
    async fn runs_recent(&self, limit: usize) -> Result<Vec<Run>>;

    /// Fetch full detail (metrics, hyperparams, artifacts) for one run.
    async fn run_detail(&self, run_id: &str) -> Result<RunDetail>;
}
