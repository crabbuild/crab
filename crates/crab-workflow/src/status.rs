//! Workflow status computation - classifies each stage as
//! `up-to-date`, `outdated`, `not-run`, or `frozen` by comparing
//! current dep/param hashes against the lockfile.
//!
//! This module is the domain logic layer. It does not perform I/O
//! itself (no filesystem reads, no YAML parsing): callers supply
//! the resolved stage map, [`Lockfile`], and a dep-hash resolver
//! callback. Runtime adapters wire these together with real filesystem
//! access.
//!
//! Designed to support both human-readable text output and structured
//! JSON (via serde on the result types).

use std::collections::BTreeMap;
use std::fmt::Write as _;

use serde::Serialize;

use crate::{Dep, ExplainMissDiff, Lockfile, Stage, StageName};

/// All stages are up-to-date.
pub const EXIT_UP_TO_DATE: i32 = 0;

/// At least one stage is outdated or not-run.
pub const EXIT_OUTDATED: i32 = 1;

/// Parse or configuration error prevented status computation.
pub const EXIT_CONFIG_ERROR: i32 = 2;

/// The computed status of a single stage in the workflow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum StageStatus {
    /// Current dep/param hashes match the lockfile entry exactly.
    UpToDate,
    /// At least one dep or param differs from the lockfile.
    Outdated {
        /// What changed: deps, params, env, or cmd.
        changes: Vec<StatusChange>,
        /// If the stage is outdated because an upstream stage is
        /// outdated, name the upstream stage here.
        #[serde(skip_serializing_if = "Option::is_none")]
        upstream: Option<String>,
    },
    /// No lockfile entry exists for this stage (never been run).
    NotRun,
    /// Stage is marked `frozen: true`; skipped regardless of input
    /// state.
    Frozen,
}

impl StageStatus {
    /// Short human-readable label for display.
    pub fn label(&self) -> &'static str {
        match self {
            Self::UpToDate => "up-to-date",
            Self::Outdated { .. } => "outdated",
            Self::NotRun => "not-run",
            Self::Frozen => "frozen",
        }
    }

    /// Whether this status counts as "needs work" for exit-code
    /// purposes.
    pub fn is_outdated(&self) -> bool {
        matches!(self, Self::Outdated { .. } | Self::NotRun)
    }
}

/// A single change that makes a stage outdated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct StatusChange {
    /// Category: `dep`, `param`, `env`, or `cmd`.
    pub category: String,
    /// The specific key that changed (e.g. file path for deps,
    /// dotted param name for params).
    pub key: String,
    /// Human-readable description of the change.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// The full status report for a workflow pipeline.
#[derive(Debug, Clone, Serialize)]
pub struct PipelineStatus {
    /// Per-stage status, ordered by stage name.
    pub stages: Vec<StageStatusEntry>,
    /// Overall pipeline state summary.
    pub summary: PipelineSummary,
}

/// One stage's status entry in the pipeline report.
#[derive(Debug, Clone, Serialize)]
pub struct StageStatusEntry {
    /// Stage name.
    pub name: String,
    /// Computed status.
    pub status: StageStatus,
}

/// Aggregate summary of the pipeline status.
#[derive(Debug, Clone, Serialize)]
pub struct PipelineSummary {
    pub total: usize,
    pub up_to_date: usize,
    pub outdated: usize,
    pub not_run: usize,
    pub frozen: usize,
}

impl PipelineSummary {
    /// The exit code this pipeline status should produce.
    pub fn exit_code(&self) -> i32 {
        if self.outdated > 0 || self.not_run > 0 {
            EXIT_OUTDATED
        } else {
            EXIT_UP_TO_DATE
        }
    }
}

/// Input required to compute status for a single stage.
///
/// Callers build this by resolving deps against the filesystem and
/// loading params from the params files. This decouples the status
/// logic from filesystem I/O.
pub struct StageInputs {
    /// Current Blake3 hashes of all path deps, keyed by repo-relative
    /// path string.
    pub dep_hashes: BTreeMap<String, [u8; 32]>,
    /// Current resolved param values, keyed by dotted param name.
    pub params: BTreeMap<String, String>,
    /// Current resolved env values (for allowlist stages).
    pub env: BTreeMap<String, String>,
    /// Whether the stage is frozen.
    pub frozen: bool,
    /// Whether the stage is always considered changed.
    pub always_changed: bool,
}

/// Compute the status of a single stage given its current inputs and
/// the lockfile.
///
/// Returns the classification plus any change details for outdated
/// stages.
pub fn classify_stage(
    stage_name: &StageName,
    inputs: &StageInputs,
    lockfile: &Lockfile,
) -> StageStatus {
    // Frozen stages are always reported as frozen regardless of
    // lockfile state.
    if inputs.frozen {
        return StageStatus::Frozen;
    }

    if inputs.always_changed {
        return always_changed_status(stage_name);
    }

    // No lockfile entry means the stage has never run.
    let Some(locked) = lockfile.get(stage_name) else {
        return StageStatus::NotRun;
    };

    // Use the lockfile's diff_against_resolved to get field-level
    // differences.
    let cached_cmd = &locked.cmd;
    let diffs = lockfile.diff_against_resolved(
        stage_name,
        &inputs.dep_hashes,
        &inputs.params,
        &inputs.env,
        cached_cmd,
    );

    match diffs {
        // diff_against_resolved returns None when no lockfile entry
        // exists. This should not happen since we checked above, but
        // handle gracefully.
        None => StageStatus::NotRun,
        Some(diffs) if diffs.is_empty() => StageStatus::UpToDate,
        Some(diffs) => {
            let changes = diffs.into_iter().map(diff_to_change).collect();
            StageStatus::Outdated {
                changes,
                upstream: None,
            }
        }
    }
}

/// Compute the full pipeline status.
///
/// `resolve_inputs` is called for each non-frozen stage to obtain its
/// current dep hashes and params. If resolution fails (e.g. a dep
/// file is missing), the stage is classified as outdated with a dep
/// change noting the missing file.
pub fn compute_pipeline_status<F>(
    stages: &BTreeMap<StageName, Stage>,
    lockfile: &Lockfile,
    mut resolve_inputs: F,
) -> PipelineStatus
where
    F: FnMut(&StageName) -> Result<StageInputs, StageInputError>,
{
    let mut entries = Vec::with_capacity(stages.len());
    let mut outdated_stages: Vec<String> = Vec::new();

    for (name, stage) in stages {
        let status = match resolve_inputs(name) {
            Ok(inputs) => {
                let mut s = classify_stage(name, &inputs, lockfile);
                // Check if this stage depends on an outdated upstream.
                if matches!(s, StageStatus::UpToDate)
                    && let Some(upstream) = find_outdated_upstream(name, &outdated_stages, stages)
                {
                    s = StageStatus::Outdated {
                        changes: Vec::new(),
                        upstream: Some(upstream),
                    };
                }
                s
            }
            Err(err) => {
                if stage.always_changed() {
                    always_changed_status(name)
                } else {
                    StageStatus::Outdated {
                        changes: vec![StatusChange {
                            category: "dep".to_owned(),
                            key: err.path.unwrap_or_else(|| "<unknown>".to_owned()),
                            detail: Some(err.reason),
                        }],
                        upstream: None,
                    }
                }
            }
        };

        if status.is_outdated() {
            outdated_stages.push(name.as_str().to_owned());
        }

        entries.push(StageStatusEntry {
            name: name.as_str().to_owned(),
            status,
        });
    }

    let summary = compute_summary(&entries);
    PipelineStatus {
        stages: entries,
        summary,
    }
}

/// Error returned when stage input resolution fails.
pub struct StageInputError {
    /// The dep path that couldn't be resolved (if applicable).
    pub path: Option<String>,
    /// Human-readable reason for the failure.
    pub reason: String,
}

/// Format the pipeline status as a human-readable string matching the
/// status output format.
pub fn format_text(status: &PipelineStatus) -> String {
    let mut out = String::new();

    // Header line.
    let header = if status.summary.exit_code() == EXIT_UP_TO_DATE {
        "Pipeline is up-to-date."
    } else {
        "Pipeline is outdated."
    };
    out.push_str(header);
    out.push('\n');
    out.push('\n');

    // Per-stage lines.
    for entry in &status.stages {
        out.push_str("  ");
        let _ = write!(
            out,
            "{:<16} {}",
            format!("{}:", entry.name),
            entry.status.label()
        );

        // For outdated stages with an upstream reason, show it.
        if let StageStatus::Outdated {
            upstream: Some(up),
            changes,
        } = &entry.status
            && changes.is_empty()
        {
            let _ = write!(out, " (upstream: {up})");
        }

        // For frozen stages, add the skip note.
        if matches!(entry.status, StageStatus::Frozen) {
            out.push_str(" (skipped)");
        }

        out.push('\n');

        // For outdated stages with changes, list them.
        if let StageStatus::Outdated { changes, .. } = &entry.status {
            let dep_changes: Vec<&StatusChange> =
                changes.iter().filter(|c| c.category == "dep").collect();
            let param_changes: Vec<&StatusChange> =
                changes.iter().filter(|c| c.category == "param").collect();

            if !dep_changes.is_empty() {
                out.push_str("    changed deps:\n");
                for change in dep_changes {
                    out.push_str("      - ");
                    out.push_str(&change.key);
                    if let Some(ref detail) = change.detail {
                        let _ = write!(out, " ({detail})");
                    }
                    out.push('\n');
                }
            }

            if !param_changes.is_empty() {
                out.push_str("    changed params:\n");
                for change in param_changes {
                    out.push_str("      - ");
                    out.push_str(&change.key);
                    if let Some(ref detail) = change.detail {
                        let _ = write!(out, ": {detail}");
                    }
                    out.push('\n');
                }
            }
        }
    }

    out
}

/// Format the pipeline status as pretty-printed JSON.
///
/// Uses `serde_json::to_string_pretty` for human-readable structured
/// output when `--json` is passed to `crab status`.
pub fn format_json(status: &PipelineStatus) -> std::result::Result<String, serde_json::Error> {
    serde_json::to_string_pretty(status)
}

/// Convert an [`ExplainMissDiff`] into a [`StatusChange`].
fn diff_to_change(diff: ExplainMissDiff) -> StatusChange {
    let detail = match (diff.old.as_deref(), diff.new.as_deref()) {
        (Some(old), Some(new)) => {
            if diff.category == "param" || diff.category == "env" {
                Some(format!("{old} → {new}"))
            } else {
                Some("content modified".to_owned())
            }
        }
        (None, Some(_)) => Some("added".to_owned()),
        (Some(_), None) => Some("removed".to_owned()),
        (None, None) => None,
    };

    StatusChange {
        category: diff.category,
        key: diff.key,
        detail,
    }
}

fn always_changed_status(stage_name: &StageName) -> StageStatus {
    StageStatus::Outdated {
        changes: vec![StatusChange {
            category: "always_changed".to_owned(),
            key: stage_name.as_str().to_owned(),
            detail: Some("stage is configured to run every time".to_owned()),
        }],
        upstream: None,
    }
}

/// Check whether any of the stage's upstream dependencies are in the
/// outdated set. Returns the first outdated upstream stage name.
fn find_outdated_upstream(
    stage_name: &StageName,
    outdated_stages: &[String],
    stages: &BTreeMap<StageName, Stage>,
) -> Option<String> {
    let stage = stages.get(stage_name)?;
    for dep in &stage.deps {
        if let Dep::StageOut {
            stage: dep_stage, ..
        } = dep
        {
            let dep_name = dep_stage.as_str();
            if outdated_stages.contains(&dep_name.to_owned()) {
                return Some(dep_name.to_owned());
            }
        }
    }
    None
}

/// Compute the aggregate summary from the entries.
fn compute_summary(entries: &[StageStatusEntry]) -> PipelineSummary {
    let mut summary = PipelineSummary {
        total: entries.len(),
        up_to_date: 0,
        outdated: 0,
        not_run: 0,
        frozen: 0,
    };
    for entry in entries {
        match &entry.status {
            StageStatus::UpToDate => summary.up_to_date += 1,
            StageStatus::Outdated { .. } => summary.outdated += 1,
            StageStatus::NotRun => summary.not_run += 1,
            StageStatus::Frozen => summary.frozen += 1,
        }
    }
    summary
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::{CachedCmd, Cmd, EnvSpec, LockedDep, StageCacheEntry};
    use crab_types::workflow::StageHash;
    use std::path::PathBuf;

    fn make_stage(name: &str) -> Stage {
        let mut s = Stage::new(
            StageName::parse(name).unwrap(),
            Cmd::Shell("echo hi".into()),
        );
        s.env = EnvSpec::Empty;
        s
    }

    fn hash_bytes(data: &[u8]) -> [u8; 32] {
        let mut hash = [0u8; 32];
        for (i, byte) in data.iter().take(31).enumerate() {
            hash[i] = *byte;
        }
        hash[31] = data.len() as u8;
        hash
    }

    fn seed_lockfile_entry(
        lockfile: &mut Lockfile,
        stage: &Stage,
        dep_hashes: &BTreeMap<String, [u8; 32]>,
        params: &BTreeMap<String, String>,
    ) {
        let entry = StageCacheEntry {
            schema_version: 1,
            stage_hash: StageHash([0xab; 32]),
            stage_name: stage.name.as_str().to_owned(),
            cmd: CachedCmd::Shell {
                shell: "echo hi".into(),
            },
            outs: Vec::new(),
            metrics: Vec::new(),
            plots: Vec::new(),
            executed_at: "2024-01-01T00:00:00Z".into(),
            duration_ms: 100,
            exec_id: None,
            attempts: 1,
            host_fingerprint: "test".into(),
        };

        let locked_deps: Vec<LockedDep> = dep_hashes
            .iter()
            .map(|(path, hash)| LockedDep {
                path: PathBuf::from(path),
                hash: *hash,
                size: 0,
            })
            .collect();

        lockfile
            .upsert(&entry, locked_deps, params.clone(), BTreeMap::new())
            .unwrap();
    }

    #[test]
    fn classify_up_to_date_when_hashes_match() {
        let stage = make_stage("build");
        let dep_hash = hash_bytes(b"hello");
        let mut deps = BTreeMap::new();
        deps.insert("a.txt".to_owned(), dep_hash);

        let mut lockfile = Lockfile::new();
        seed_lockfile_entry(&mut lockfile, &stage, &deps, &BTreeMap::new());

        let inputs = StageInputs {
            dep_hashes: deps,
            params: BTreeMap::new(),
            env: BTreeMap::new(),
            frozen: false,
            always_changed: false,
        };

        let status = classify_stage(&stage.name, &inputs, &lockfile);
        assert_eq!(status, StageStatus::UpToDate);
    }

    #[test]
    fn classify_not_run_when_no_lockfile_entry() {
        let stage = make_stage("build");
        let lockfile = Lockfile::new();

        let inputs = StageInputs {
            dep_hashes: BTreeMap::new(),
            params: BTreeMap::new(),
            env: BTreeMap::new(),
            frozen: false,
            always_changed: false,
        };

        let status = classify_stage(&stage.name, &inputs, &lockfile);
        assert_eq!(status, StageStatus::NotRun);
    }

    #[test]
    fn classify_frozen_regardless_of_lockfile() {
        let stage = make_stage("build");
        let lockfile = Lockfile::new();

        let inputs = StageInputs {
            dep_hashes: BTreeMap::new(),
            params: BTreeMap::new(),
            env: BTreeMap::new(),
            frozen: true,
            always_changed: true,
        };

        let status = classify_stage(&stage.name, &inputs, &lockfile);
        assert_eq!(status, StageStatus::Frozen);
    }

    #[test]
    fn classify_always_changed_as_outdated() {
        let stage = make_stage("poll");
        let mut lockfile = Lockfile::new();
        seed_lockfile_entry(&mut lockfile, &stage, &BTreeMap::new(), &BTreeMap::new());

        let inputs = StageInputs {
            dep_hashes: BTreeMap::new(),
            params: BTreeMap::new(),
            env: BTreeMap::new(),
            frozen: false,
            always_changed: true,
        };

        let status = classify_stage(&stage.name, &inputs, &lockfile);
        match status {
            StageStatus::Outdated { changes, upstream } => {
                assert!(upstream.is_none());
                assert_eq!(changes[0].category, "always_changed");
                assert_eq!(changes[0].key, "poll");
            }
            other => panic!("expected Outdated, got {other:?}"),
        }
    }

    #[test]
    fn classify_outdated_when_dep_changed() {
        let stage = make_stage("build");
        let original_hash = hash_bytes(b"original");
        let mut deps = BTreeMap::new();
        deps.insert("data.csv".to_owned(), original_hash);

        let mut lockfile = Lockfile::new();
        seed_lockfile_entry(&mut lockfile, &stage, &deps, &BTreeMap::new());

        // Now the dep has changed.
        let new_hash = hash_bytes(b"modified");
        let mut new_deps = BTreeMap::new();
        new_deps.insert("data.csv".to_owned(), new_hash);

        let inputs = StageInputs {
            dep_hashes: new_deps,
            params: BTreeMap::new(),
            env: BTreeMap::new(),
            frozen: false,
            always_changed: false,
        };

        let status = classify_stage(&stage.name, &inputs, &lockfile);
        match status {
            StageStatus::Outdated { changes, upstream } => {
                assert!(upstream.is_none());
                assert!(!changes.is_empty());
                assert_eq!(changes[0].category, "dep");
                assert_eq!(changes[0].key, "data.csv");
            }
            other => panic!("expected Outdated, got {other:?}"),
        }
    }

    #[test]
    fn classify_outdated_when_param_changed() {
        let stage = make_stage("train");
        let mut params = BTreeMap::new();
        params.insert("model.lr".to_owned(), "0.001".to_owned());

        let mut lockfile = Lockfile::new();
        seed_lockfile_entry(&mut lockfile, &stage, &BTreeMap::new(), &params);

        // Param value changed.
        let mut new_params = BTreeMap::new();
        new_params.insert("model.lr".to_owned(), "0.01".to_owned());

        let inputs = StageInputs {
            dep_hashes: BTreeMap::new(),
            params: new_params,
            env: BTreeMap::new(),
            frozen: false,
            always_changed: false,
        };

        let status = classify_stage(&stage.name, &inputs, &lockfile);
        match status {
            StageStatus::Outdated { changes, .. } => {
                assert!(
                    changes
                        .iter()
                        .any(|c| c.category == "param" && c.key == "model.lr")
                );
            }
            other => panic!("expected Outdated, got {other:?}"),
        }
    }

    #[test]
    fn summary_exit_code_zero_when_all_up_to_date() {
        let entries = vec![
            StageStatusEntry {
                name: "a".to_owned(),
                status: StageStatus::UpToDate,
            },
            StageStatusEntry {
                name: "b".to_owned(),
                status: StageStatus::Frozen,
            },
        ];
        let summary = compute_summary(&entries);
        assert_eq!(summary.exit_code(), EXIT_UP_TO_DATE);
    }

    #[test]
    fn summary_exit_code_one_when_outdated() {
        let entries = vec![
            StageStatusEntry {
                name: "a".to_owned(),
                status: StageStatus::UpToDate,
            },
            StageStatusEntry {
                name: "b".to_owned(),
                status: StageStatus::Outdated {
                    changes: vec![],
                    upstream: None,
                },
            },
        ];
        let summary = compute_summary(&entries);
        assert_eq!(summary.exit_code(), EXIT_OUTDATED);
    }

    #[test]
    fn summary_exit_code_one_when_not_run() {
        let entries = vec![StageStatusEntry {
            name: "a".to_owned(),
            status: StageStatus::NotRun,
        }];
        let summary = compute_summary(&entries);
        assert_eq!(summary.exit_code(), EXIT_OUTDATED);
    }

    #[test]
    fn format_text_shows_outdated_pipeline() {
        let status = PipelineStatus {
            stages: vec![
                StageStatusEntry {
                    name: "clean".to_owned(),
                    status: StageStatus::UpToDate,
                },
                StageStatusEntry {
                    name: "train".to_owned(),
                    status: StageStatus::Outdated {
                        changes: vec![
                            StatusChange {
                                category: "dep".to_owned(),
                                key: "data/features.csv".to_owned(),
                                detail: Some("content modified".to_owned()),
                            },
                            StatusChange {
                                category: "param".to_owned(),
                                key: "model.lr".to_owned(),
                                detail: Some("0.001 → 0.01".to_owned()),
                            },
                        ],
                        upstream: None,
                    },
                },
                StageStatusEntry {
                    name: "evaluate".to_owned(),
                    status: StageStatus::Outdated {
                        changes: vec![],
                        upstream: Some("train".to_owned()),
                    },
                },
                StageStatusEntry {
                    name: "deploy".to_owned(),
                    status: StageStatus::Frozen,
                },
            ],
            summary: PipelineSummary {
                total: 4,
                up_to_date: 1,
                outdated: 2,
                not_run: 0,
                frozen: 1,
            },
        };

        let text = format_text(&status);
        assert!(text.contains("Pipeline is outdated."));
        assert!(text.contains("clean:"));
        assert!(text.contains("up-to-date"));
        assert!(text.contains("train:"));
        assert!(text.contains("outdated"));
        assert!(text.contains("data/features.csv"));
        assert!(text.contains("content modified"));
        assert!(text.contains("model.lr"));
        assert!(text.contains("0.001 → 0.01"));
        assert!(text.contains("evaluate:"));
        assert!(text.contains("upstream: train"));
        assert!(text.contains("deploy:"));
        assert!(text.contains("frozen"));
        assert!(text.contains("skipped"));
    }

    #[test]
    fn format_text_shows_up_to_date_pipeline() {
        let status = PipelineStatus {
            stages: vec![StageStatusEntry {
                name: "build".to_owned(),
                status: StageStatus::UpToDate,
            }],
            summary: PipelineSummary {
                total: 1,
                up_to_date: 1,
                outdated: 0,
                not_run: 0,
                frozen: 0,
            },
        };

        let text = format_text(&status);
        assert!(text.contains("Pipeline is up-to-date."));
    }

    #[test]
    fn format_json_produces_valid_structured_output() {
        let status = PipelineStatus {
            stages: vec![
                StageStatusEntry {
                    name: "clean".to_owned(),
                    status: StageStatus::UpToDate,
                },
                StageStatusEntry {
                    name: "train".to_owned(),
                    status: StageStatus::Outdated {
                        changes: vec![StatusChange {
                            category: "dep".to_owned(),
                            key: "data.csv".to_owned(),
                            detail: Some("content modified".to_owned()),
                        }],
                        upstream: None,
                    },
                },
                StageStatusEntry {
                    name: "deploy".to_owned(),
                    status: StageStatus::Frozen,
                },
            ],
            summary: PipelineSummary {
                total: 3,
                up_to_date: 1,
                outdated: 1,
                not_run: 0,
                frozen: 1,
            },
        };

        let json_str = format_json(&status).unwrap();

        // Verify it's valid JSON by round-tripping through serde_json.
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();

        // Check top-level structure.
        assert!(parsed.get("stages").unwrap().is_array());
        assert!(parsed.get("summary").unwrap().is_object());

        // Check stage entries.
        let stages = parsed["stages"].as_array().unwrap();
        assert_eq!(stages.len(), 3);
        assert_eq!(stages[0]["name"], "clean");
        assert_eq!(stages[0]["status"]["status"], "up_to_date");
        assert_eq!(stages[1]["name"], "train");
        assert_eq!(stages[1]["status"]["status"], "outdated");
        assert_eq!(stages[1]["status"]["changes"][0]["category"], "dep");
        assert_eq!(stages[1]["status"]["changes"][0]["key"], "data.csv");
        assert_eq!(stages[2]["name"], "deploy");
        assert_eq!(stages[2]["status"]["status"], "frozen");

        // Check summary.
        let summary = &parsed["summary"];
        assert_eq!(summary["total"], 3);
        assert_eq!(summary["up_to_date"], 1);
        assert_eq!(summary["outdated"], 1);
        assert_eq!(summary["frozen"], 1);
    }
}
