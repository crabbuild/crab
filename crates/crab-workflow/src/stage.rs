//! Workflow stage contract.

use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub use crate::{
    Cmd, Dep, EnvSpec, Out, OutKind, ParamRef, PlotConfig, Resources, RetryPolicy, StageCondition,
    StageName, is_external_url_out, is_external_url_out_path, is_url_dep, validate_wdir,
};

#[cfg(any(test, feature = "testing"))]
#[doc(hidden)]
pub use crate::stage_runtime::test_support;
pub use crate::stage_runtime::{DepUrlHashExt, expand_external_url_out_alias};

/// A single workflow stage: command, deps, outs, env, and execution policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Stage {
    pub name: StageName,
    pub cmd: Cmd,
    pub deps: Vec<Dep>,
    pub outs: Vec<Out>,
    pub env: EnvSpec,
    pub retry: Option<RetryPolicy>,
    /// Stage-level timeout. When `Some`, runtime escalation is SIGTERM, then
    /// the configured graceful-shutdown timeout, then SIGKILL.
    #[serde(default)]
    pub timeout: Option<Duration>,
    /// Working directory for the stage command, relative to repo root.
    ///
    /// When set, runtime adapters resolve `deps` and `outs` relative to this
    /// directory. The value participates in the stage hash.
    #[serde(default)]
    pub wdir: Option<PathBuf>,
    /// Skip pre-exec deletion of declared out paths.
    #[serde(default)]
    pub persist: bool,
    /// Whether the stage is non-deterministic.
    #[serde(default)]
    pub nondeterministic: bool,
    /// Opt into the hermetic sandbox.
    #[serde(default)]
    pub hermetic: bool,
    /// Dotted-key references into declared params files.
    #[serde(default)]
    pub params: Vec<ParamRef>,
    /// Metric output paths.
    #[serde(default)]
    pub metrics: Vec<PathBuf>,
    /// Plot source paths surfaced to the metrics diff renderer.
    #[serde(default)]
    pub plots: Vec<PathBuf>,
    /// Stage has observable side effects outside declared outs.
    #[serde(default)]
    pub side_effects: bool,
    /// Optional hook invoked on cache hit for stages with side effects.
    #[serde(default)]
    pub on_cache_hit: Option<Cmd>,
    /// Resource requirements for the parallel scheduler.
    #[serde(default)]
    pub resources: Resources,
    /// When `true`, the stage is skipped entirely during runtime execution.
    #[serde(default)]
    pub frozen: bool,
    /// Human-readable description shown in workflow reports.
    #[serde(default)]
    pub desc: Option<String>,
    /// Arbitrary YAML metadata preserved for external tooling.
    #[serde(default)]
    pub meta: Option<serde_yaml::Value>,
    /// Condition that gates execution.
    #[serde(default)]
    pub condition: Option<StageCondition>,
}

impl Stage {
    /// Build a minimal deterministic stage with inherited env and default resources.
    #[must_use]
    pub fn new(name: StageName, cmd: Cmd) -> Self {
        Self {
            name,
            cmd,
            deps: Vec::new(),
            outs: Vec::new(),
            env: EnvSpec::Inherit,
            retry: None,
            timeout: None,
            wdir: None,
            persist: false,
            nondeterministic: false,
            hermetic: false,
            params: Vec::new(),
            metrics: Vec::new(),
            plots: Vec::new(),
            side_effects: false,
            on_cache_hit: None,
            resources: Resources::default(),
            frozen: false,
            desc: None,
            meta: None,
            condition: None,
        }
    }

    /// Return whether this stage may read or write the run cache.
    #[must_use]
    pub fn run_cache_enabled(&self) -> bool {
        self.outs.iter().all(|out| out.cache)
    }

    /// Return whether this stage must execute on every run.
    #[must_use]
    pub fn always_changed(&self) -> bool {
        self.nondeterministic || (self.deps.is_empty() && self.outs.is_empty())
    }

    /// Return whether this stage may read from the run cache.
    #[must_use]
    pub fn run_cache_lookup_enabled(&self) -> bool {
        self.run_cache_enabled() && !self.always_changed()
    }

    /// Return whether this stage may publish a remote cache entry.
    #[must_use]
    pub fn remote_cache_push_enabled(&self) -> bool {
        self.run_cache_lookup_enabled() && self.outs.iter().all(|out| out.push)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{OutKind, WorkflowError};

    #[test]
    fn no_dep_no_out_stage_is_always_changed() {
        let stage = Stage::new(StageName::parse("poll").unwrap(), Cmd::Shell("true".into()));
        assert!(stage.always_changed());
        assert!(!stage.run_cache_lookup_enabled());
        assert!(!stage.remote_cache_push_enabled());
    }

    #[test]
    fn non_cached_out_disables_run_cache() {
        let mut stage = Stage::new(
            StageName::parse("train").unwrap(),
            Cmd::Shell("true".into()),
        );
        let mut out = Out::new(PathBuf::from("model.bin"), OutKind::File);
        out.cache = false;
        stage.outs.push(out);
        assert!(!stage.run_cache_enabled());
        assert!(!stage.run_cache_lookup_enabled());
        assert!(!stage.remote_cache_push_enabled());
    }

    #[test]
    fn stage_validates_out_contract_through_owned_type() {
        let mut stage = Stage::new(
            StageName::parse("train").unwrap(),
            Cmd::Shell("true".into()),
        );
        stage
            .outs
            .push(Out::new(PathBuf::from("../escape"), OutKind::File));
        let err = stage.outs[0].validate(&stage.name).unwrap_err();
        assert!(matches!(err, WorkflowError::StageOutMalformed { .. }));
    }
}
