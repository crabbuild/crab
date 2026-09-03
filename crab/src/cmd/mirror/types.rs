//! Structured mirror integrity, plan, and apply contracts.

use std::collections::BTreeMap;
use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
/// Aggregate relationship between the collaboration source and Crab refs.
pub enum MirrorDriftState {
    Equal,
    SourceAhead,
    CrabAhead,
    Diverged,
    Unverifiable,
}

impl fmt::Display for MirrorDriftState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Equal => "equal",
            Self::SourceAhead => "source_ahead",
            Self::CrabAhead => "crab_ahead",
            Self::Diverged => "diverged",
            Self::Unverifiable => "unverifiable",
        })
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
/// Relationship between one source ref and its Crab counterpart.
pub enum MirrorRefState {
    Equal,
    SourceAhead,
    CrabAhead,
    Diverged,
    Unverifiable,
}

impl fmt::Display for MirrorRefState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Equal => "equal",
            Self::SourceAhead => "source_ahead",
            Self::CrabAhead => "crab_ahead",
            Self::Diverged => "diverged",
            Self::Unverifiable => "unverifiable",
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
/// Snapshot-bound comparison result for one Git ref.
pub struct MirrorRefStatus {
    /// Full Git ref name.
    pub name: String,
    /// Source object ID, when the ref exists in the source.
    pub source_oid: Option<String>,
    /// Crab object ID, when the ref exists in Crab.
    pub crab_oid: Option<String>,
    /// Classified relationship between the two values.
    pub state: MirrorRefState,
    /// Failure detail when ancestry could not be proven.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
/// Availability state for source-reachable Crab pointer data.
pub enum MirrorPointerState {
    Verified,
    Missing,
    Corrupt,
    Unverifiable,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
/// One incomplete or unverifiable Crab pointer dependency.
pub struct MirrorPointerIssue {
    /// Pointer file hash, or empty when discovery itself failed.
    pub file_hash: String,
    /// Size declared by the source pointer.
    pub expected_size: u64,
    /// Pointer dependency state.
    pub state: MirrorPointerState,
    /// Concrete integrity failure.
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
/// Aggregate proof for all source-reachable Crab pointer blobs.
pub struct MirrorPointerStatus {
    /// Unique pointers discovered from source refs.
    pub discovered: u64,
    /// Pointers whose recipe and immutable data were fully verified.
    pub verified: u64,
    /// Identity of the exact verified recipes; absent when coverage is incomplete.
    pub recipe_digest: Option<String>,
    /// Aggregate pointer proof state.
    pub state: MirrorPointerState,
    /// Per-pointer or discovery failures.
    pub issues: Vec<MirrorPointerIssue>,
}

impl MirrorPointerStatus {
    pub(super) fn unverifiable(detail: impl Into<String>) -> Self {
        Self {
            discovered: 0,
            verified: 0,
            recipe_digest: None,
            state: MirrorPointerState::Unverifiable,
            issues: vec![MirrorPointerIssue {
                file_hash: String::new(),
                expected_size: 0,
                state: MirrorPointerState::Unverifiable,
                detail: detail.into(),
            }],
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
/// Local mirror pre-push hook state.
pub enum MirrorHookState {
    Installed,
    Missing,
    NotApplicable,
    Unverifiable,
}

impl fmt::Display for MirrorHookState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Installed => "installed",
            Self::Missing => "missing",
            Self::NotApplicable => "not_applicable",
            Self::Unverifiable => "unverifiable",
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
/// Result of inspecting the source working tree's pre-push hook.
pub struct MirrorHookStatus {
    /// Classified hook state.
    pub state: MirrorHookState,
    /// Resolved hook path when applicable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Explanation for non-installed or non-applicable states.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
/// Read-only mirror integrity result used by text and structured output.
pub struct MirrorCheckSummary {
    /// Resolved collaboration source.
    pub source: String,
    /// Crab destination URL.
    pub destination: String,
    /// Local bare cache used to pin and inspect source objects.
    pub cache_dir: String,
    /// Aggregate ref relationship.
    pub state: MirrorDriftState,
    /// Per-ref snapshot and relationship.
    pub refs: Vec<MirrorRefStatus>,
    /// Resolved storage target and scope, independent of mutable metadata.
    pub destination_identity: Option<String>,
    /// Complete Crab metadata snapshot bound to its resolved storage namespace.
    pub destination_snapshot: Option<String>,
    /// Source-reachable pointer data proof.
    pub pointers: MirrorPointerStatus,
    /// Local pre-push guard state.
    pub hook: MirrorHookStatus,
    /// Whether the result satisfies the required CI policy.
    pub ci_passed: bool,
    /// Operator-readable reasons the result is not clean.
    pub issues: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
/// Mutation permitted by a reconciliation plan.
pub enum MirrorPlanActionKind {
    UpdateCrabRef,
    DeleteCrabRef,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
/// One exact source-to-Crab ref mutation bound to both snapshots.
pub struct MirrorPlanAction {
    /// Mutation kind.
    pub kind: MirrorPlanActionKind,
    /// Full destination ref name.
    pub ref_name: String,
    /// Expected source object ID before apply.
    pub expected_source_oid: Option<String>,
    /// Expected Crab object ID before apply.
    pub expected_crab_oid: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
/// Immutable, content-identified mirror reconciliation contract.
pub struct MirrorReconciliationPlan {
    /// Plan format version.
    pub format_version: u32,
    /// Blake3 identity of the canonical plan body.
    pub plan_id: String,
    /// Resolved collaboration source.
    pub source: String,
    /// Crab destination URL.
    pub destination: String,
    /// Exact source ref snapshot.
    pub source_refs: BTreeMap<String, String>,
    /// Exact Crab ref snapshot.
    pub crab_refs: BTreeMap<String, String>,
    /// Resolved storage target and scope, required even for converged replay.
    pub destination_identity: Option<String>,
    /// Exact namespace-bound Crab metadata identity, absent only in blocked plans.
    pub destination_snapshot: Option<String>,
    /// Exact verified recipe identity, absent only in blocked plans.
    pub recipe_digest: Option<String>,
    /// Number of source-reachable pointers proved before planning.
    pub pointer_count: u64,
    /// Whether the plan explicitly permits destination-only ref deletion.
    pub allow_delete_refs: bool,
    /// Whether safety checks prevent apply.
    pub blocked: bool,
    /// Reasons the plan cannot be applied.
    pub blockers: Vec<String>,
    /// Ordered, source-to-Crab mutations.
    pub actions: Vec<MirrorPlanAction>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
/// Terminal result of applying a reconciliation plan.
pub struct MirrorApplySummary {
    /// Applied plan identity.
    pub plan_id: String,
    /// Resolved collaboration source.
    pub source: String,
    /// Crab destination URL.
    pub destination: String,
    /// Number of actions encoded in the plan.
    pub actions_planned: u64,
    /// Number of actions submitted by this invocation.
    pub actions_applied: u64,
    /// Whether the exact plan outcome was already present.
    pub already_applied: bool,
    /// Verified destination relationship after apply.
    pub final_state: MirrorDriftState,
}

/// Typed terminal outcome of mirror integrity or reconciliation execution.
pub enum MirrorCommandOutcome {
    Check(Box<MirrorCheckSummary>),
    Apply(MirrorApplySummary),
}

impl MirrorCommandOutcome {
    /// Return the structured output schema name for this outcome.
    pub fn schema_name(&self) -> &'static str {
        match self {
            Self::Check(_) => "mirror.check",
            Self::Apply(_) => "mirror.apply",
        }
    }

    /// Emit one versioned JSON envelope.
    pub fn emit_json(&self) {
        match self {
            Self::Check(summary) => {
                crate::core::output::emit_json(self.schema_name(), "1.0", summary);
            }
            Self::Apply(summary) => {
                crate::core::output::emit_json(self.schema_name(), "1.0", summary);
            }
        }
    }

    /// Emit one terminal JSONL result event.
    pub fn emit_jsonl(&self) {
        match self {
            Self::Check(summary) => {
                let mut stream = crate::core::output::JsonlStream::new(
                    "mirror.check.event",
                    "1.0",
                    std::io::stdout(),
                );
                stream.emit_result(summary);
            }
            Self::Apply(summary) => {
                let mut stream = crate::core::output::JsonlStream::new(
                    "mirror.apply.event",
                    "1.0",
                    std::io::stdout(),
                );
                stream.emit_result(summary);
            }
        }
    }

    /// Return whether this outcome satisfies the requested CI policy.
    pub fn ci_passed(&self) -> bool {
        match self {
            Self::Check(summary) => summary.ci_passed,
            Self::Apply(summary) => summary.final_state == MirrorDriftState::Equal,
        }
    }
}
