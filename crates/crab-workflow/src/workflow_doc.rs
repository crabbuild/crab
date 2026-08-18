//! Parsed workflow document contract.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::{EnvSpec, PlotConfig, RetryPolicy, Stage, StageName};

/// Versioned, non-executable metadata carried by a workflow's top-level
/// `artifacts` declaration.
#[derive(Debug, Clone, PartialEq)]
pub struct ArtifactMetadata {
    /// Schema version for the preserved declaration shape.
    pub schema_version: u32,
    /// Declarations keyed by their source artifact name.
    pub declarations: BTreeMap<String, serde_yaml::Value>,
}

impl ArtifactMetadata {
    /// Current schema for preserved artifact declarations.
    pub const SCHEMA_VERSION: u32 = 1;

    /// Preserve raw declarations without making them executable workflow state.
    #[must_use]
    pub fn from_declarations(declarations: BTreeMap<String, serde_yaml::Value>) -> Self {
        Self {
            schema_version: Self::SCHEMA_VERSION,
            declarations,
        }
    }

    /// Return whether this value contains any preserved declarations.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.declarations.is_empty()
    }
}

impl Default for ArtifactMetadata {
    fn default() -> Self {
        Self::from_declarations(BTreeMap::new())
    }
}

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
    /// Preserved top-level artifact declarations; not executable until the
    /// artifact lifecycle contract validates them.
    pub artifacts: ArtifactMetadata,
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
