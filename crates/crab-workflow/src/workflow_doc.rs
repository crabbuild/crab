//! Parsed workflow document contract.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::{EnvSpec, PlotConfig, RetryPolicy, Stage, StageName};

/// Top-level `crab.yaml` document parsed into validated, owned stages.
///
/// Stages are stored in a [`BTreeMap`] so iteration order stays stable for
/// deterministic graph planning and command output.
#[derive(Debug, Clone, PartialEq)]
pub struct Workflow {
    /// Declared params files at the workflow level.
    pub params: Vec<PathBuf>,
    /// Declared metrics files at the workflow level.
    pub metrics: Vec<PathBuf>,
    /// Declared plot source paths at the workflow level.
    pub plots: Vec<PathBuf>,
    /// Structured plot configurations for desktop and metrics rendering.
    pub plot_configs: Vec<PlotConfig>,
    /// Stage-level defaults applied when a stage does not override them.
    pub defaults: Defaults,
    /// Stages keyed by validated stage name.
    pub stages: BTreeMap<StageName, Stage>,
    /// Maps stage names to their named workflow group.
    pub workflow_membership: BTreeMap<StageName, String>,
}

/// Defaults applied to every stage unless overridden.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Defaults {
    /// Default environment policy.
    pub env: Option<EnvSpec>,
    /// Default retry policy.
    pub retry: Option<RetryPolicy>,
}
