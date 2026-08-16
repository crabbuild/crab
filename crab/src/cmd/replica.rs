//! `crab replica` — configure and inspect read replicas for Crab remotes.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use clap::{Parser, Subcommand, ValueEnum};
use object_store::path::Path as ObjectPath;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use tokio_util::sync::CancellationToken;
use tracing::warn;

use crate::audit::{AuditEvent, AuditOutcome, NewAuditEvent, append_event, default_log_path};
use crate::core::config::Config;
use crate::core::error::{CrabError, Result};
use crate::core::output::{JsonlStream, OutputMode, emit_json};
use crate::core::project_config::{ProjectConfig, RemoteConfig};
use crate::git::url::CrabUrl;
use crate::replication::{
    ActiveActiveFailoverOperation, ActiveActiveFailoverOutcome, ActiveActiveRepairPlan,
    ActiveActiveResumeProof, ActiveActiveStatus, ControlPlaneApplyStatus, ControlPlaneCheck,
    ControlPlaneCheckState, ControlPlaneExportFormat, ControlPlaneStatus, ReadinessCheckOptions,
    ReplicaConfig, ReplicaFallbackClass, ReplicaStatus, ReplicationConfig,
    ReplicationControlPlanePlan, ReplicationCoordinatorConfig, ReplicationCoordinatorConsistency,
    ReplicationCoordinatorKind, ReplicationMode, ReplicationProviderKind, ReplicationRpo,
    StoreResolver, WriterConfig, active_active_coordinator_health,
    active_active_repair_plan_from_coordinator, active_active_status,
    active_active_status_with_coordinator_status, apply_active_active_repair_from_coordinator,
    apply_control_plane_plan, control_plane_plan, control_plane_remove_plan,
    export_control_plane_plan, fence_active_active_writes, inspect_control_plane_plan_default,
    project_config_path, readiness_check_options_from_env, remove_control_plane_plan,
    replica_statuses_with_options, resume_active_active_writes, sync_readiness_cache_control_plane,
    validate_active_active_config, validate_replica_url_provider,
};
#[cfg(feature = "coordinator-cosmosdb")]
use crab_coordination::cosmosdb_coordinator::AzureCosmosDbCoordinatorBackend;
#[cfg(feature = "coordinator-dynamodb")]
use crab_coordination::dynamodb_coordinator::AwsDynamoDbCoordinatorBackend;
#[cfg(feature = "coordinator-spanner")]
use crab_coordination::spanner_coordinator::GoogleSpannerCoordinatorBackend;
use crab_coordination::write_coordinator::{
    CoordinatorApplyStatus, CoordinatorCheckState, CoordinatorControlPlaneBackend,
    CoordinatorControlPlaneCheck, CoordinatorControlPlanePlan, CoordinatorControlPlaneStatus,
    CoordinatorHealth, ManagedCoordinatorProvider, apply_coordinator_control_plane_plan,
    apply_coordinator_control_plane_plan_with_backend, coordinator_control_plane_remove_plan,
    cosmosdb_coordinator_plan, dynamodb_coordinator_plan, inspect_coordinator_control_plane_plan,
    inspect_coordinator_control_plane_plan_with_backend, remove_coordinator_control_plane_plan,
    remove_coordinator_control_plane_plan_with_backend, spanner_coordinator_plan,
};

const SCHEMA: &str = "replica";
const SCHEMA_VERSION: &str = "1.0";
const DEFAULT_STATUS_WATCH_INTERVAL_SECS: u64 = 5;
const DEFAULT_REPAIR_WATCH_INTERVAL_SECS: u64 = 30;
const MIN_REPAIR_WATCH_LEASE_TTL_SECS: u64 = 300;
const MAX_REPAIR_WATCH_BACKOFF_SECS: u64 = 300;
const REPAIR_WATCH_LEASE_SCHEMA_VERSION: u32 = 1;
const COORDINATOR_STATE_WARNING_PERCENT: u64 = 80;
const COORDINATOR_STATE_CRITICAL_PERCENT: u64 = 95;
#[cfg(test)]
const ACTIVE_ACTIVE_SMOKE_SEQUENCE: &[&str] = &[
    "mode-active-active",
    "initial-failover-status",
    "repair-service-template",
    "repair-worker-deployment",
    "push-origin-main",
    "repair-snapshot",
    "clone-main",
    "hydrate-main",
    "failover-fence",
    "writes-fenced",
    "push-rejected-fenced-west-main",
    "failover-resume",
    "writes-enabled",
    "push-west-feature",
    "repair-snapshot",
    "clone-feature",
    "hydrate-feature",
    "push-rejected-west-main",
    "active-active-certification",
];
const ACTIVE_ACTIVE_SMOKE_SEMANTIC_SEQUENCE: &[&str] = &[
    "mode-active-active",
    "initial-failover-status",
    "repair-service-template",
    "repair-worker-deployment",
    "push-success",
    "repair-snapshot",
    "clone",
    "hydrate",
    "failover-fence",
    "writes-fenced",
    "push-rejected-fenced",
    "failover-resume",
    "writes-enabled",
    "push-success",
    "repair-snapshot",
    "clone",
    "hydrate",
    "push-rejected-stale",
    "active-active-certification",
];

/// Subcommands for `crab replica`.
#[derive(Debug, Clone, Subcommand)]
pub enum ReplicaCommand {
    /// Add or update a read replica.
    Add(AddArgs),
    /// Export provider control-plane operations for audit or IaC review.
    Export(ExportArgs),
    /// Estimate billable replication usage quantities.
    Cost(CostArgs),
    /// Show incident recovery steps for enterprise replication.
    Runbook(RunbookArgs),
    /// Wait for a replica to become read-ready.
    Wait(WaitArgs),
    /// Verify replica manifest-referenced objects with live checks.
    Verify(VerifyArgs),
    /// Inspect historical object backfill before enabling replica reads.
    #[command(subcommand)]
    Backfill(BackfillCommand),
    /// Enable a verified read replica.
    Enable(EnableArgs),
    /// Disable a read replica.
    Disable(DisableArgs),
    /// Configure the replication write mode.
    Mode(ModeArgs),
    /// Manage active-active writer regions.
    #[command(subcommand)]
    Writers(WritersCommand),
    /// Manage active-active write coordinators.
    #[command(subcommand)]
    Coordinator(CoordinatorCommand),
    /// Show active-active failover status.
    #[command(subcommand)]
    Failover(FailoverCommand),
    /// Repair regional state from coordinator truth.
    Repair(RepairArgs),
    /// Promote a read replica to the primary remote.
    Promote(PromoteArgs),
    /// Set the primary remote as a guarded disaster-recovery operation.
    SetPrimary(SetPrimaryArgs),
    /// Collect a portable replica diagnostics bundle.
    Diagnostics(DiagnosticsArgs),
    /// Run strict enterprise production-readiness certification gates.
    Certify(CertifyArgs),
    /// Verify retained enterprise replication evidence artifacts.
    #[command(subcommand)]
    Evidence(EvidenceCommand),
    /// Show configured replica health and lag.
    Status(StatusArgs),
    /// Diagnose replica configuration and readiness.
    Doctor(DoctorArgs),
    /// Remove a configured read replica.
    Remove(RemoveArgs),
}

#[derive(Debug, Clone, Parser)]
pub struct AddArgs {
    /// Replica name.
    pub name: String,
    /// Cloud provider backing the replica.
    #[arg(long)]
    pub provider: ProviderArg,
    /// Primary Crab remote URL.
    #[arg(long)]
    pub primary: String,
    /// Replica cloud URL. If no repo prefix is present, the primary repo path is reused.
    #[arg(long)]
    pub replica: String,
    /// Replica region.
    #[arg(long)]
    pub region: String,
    /// Backfill existing objects with the provider-native batch mechanism.
    #[arg(long)]
    pub backfill: bool,
    /// Recovery profile.
    #[arg(long, default_value = "standard")]
    pub rpo: RpoArg,
    /// Print the provider setup plan without changing Crab config.
    #[arg(long, conflicts_with = "apply")]
    pub dry_run: bool,
    /// Apply provider replication resources through Crab's cloud control plane.
    #[arg(long)]
    pub apply: bool,
    /// Structured JSON output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Parser)]
pub struct ExportArgs {
    /// Replica name. Required when multiple replicas are configured.
    #[arg(long)]
    pub name: Option<String>,
    /// Export format.
    #[arg(long)]
    pub format: ExportFormatArg,
    /// Structured JSON output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Parser)]
pub struct CostArgs {
    /// Replica name. Estimates every configured replica when omitted.
    #[arg(long)]
    pub name: Option<String>,
    /// Expected new immutable object data replicated each month.
    #[arg(long, default_value_t = 0.0, value_name = "GB")]
    pub monthly_write_gb: f64,
    /// Expected replica read egress each month.
    #[arg(long, default_value_t = 0.0, value_name = "GB")]
    pub monthly_read_gb: f64,
    /// One-time historical data backfill to model.
    #[arg(long, default_value_t = 0.0, value_name = "GB")]
    pub backfill_gb: f64,
    /// Expected provider API request volume each month.
    #[arg(long, default_value_t = 0.0, value_name = "MILLIONS")]
    pub monthly_requests_million: f64,
    /// Structured JSON output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Parser)]
pub struct RunbookArgs {
    /// Incident scenario to recover.
    pub scenario: RunbookScenarioArg,
    /// Replica name for replica-specific scenarios.
    #[arg(long)]
    pub name: Option<String>,
    /// Structured JSON output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Parser)]
pub struct WaitArgs {
    /// Replica name.
    pub name: String,
    /// Enable reads after readiness passes.
    #[arg(long)]
    pub enable_read: bool,
    /// Structured JSON output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Parser)]
pub struct VerifyArgs {
    /// Replica name. Verifies every configured replica when omitted.
    #[arg(long)]
    pub name: Option<String>,
    /// Force live manifest/object checks instead of using the readiness cache.
    #[arg(long)]
    pub deep: bool,
    /// Explicitly verify every referenced object. This is the default for --deep.
    #[arg(long, conflicts_with = "sample_size")]
    pub exhaustive: bool,
    /// Verify at most this many referenced pack/shard/xorb objects per replica.
    #[arg(long, value_name = "OBJECTS", conflicts_with = "exhaustive")]
    pub sample_size: Option<u64>,
    /// Structured JSON output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Subcommand)]
pub enum BackfillCommand {
    /// Show backfill verification state.
    Status(BackfillStatusArgs),
}

#[derive(Debug, Clone, Parser)]
pub struct BackfillStatusArgs {
    /// Replica name. Shows every configured replica when omitted.
    #[arg(long)]
    pub name: Option<String>,
    /// Structured JSON output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Parser)]
pub struct EnableArgs {
    /// Replica name.
    pub name: String,
    /// Structured JSON output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Parser)]
pub struct DisableArgs {
    /// Replica name.
    pub name: String,
    /// Structured JSON output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Parser)]
pub struct ModeArgs {
    /// Replication mode.
    pub mode: ModeArg,
    /// Managed coordinator URL for active-active mode.
    #[arg(long)]
    pub coordinator: Option<String>,
    /// Coordinator region. Defaults to global when omitted.
    #[arg(long, default_value = "global")]
    pub coordinator_region: String,
    /// Managed coordinator failover region.
    #[arg(long = "failover-region")]
    pub failover_regions: Vec<String>,
    /// Active-active writer spec: name=url,region=region[,enabled=true|false].
    #[arg(long = "writer")]
    pub writers: Vec<String>,
    /// Structured JSON output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Subcommand)]
pub enum WritersCommand {
    /// Show active-active writer regions.
    Status(WritersStatusArgs),
    /// Enable a writer region.
    Enable(WriterToggleArgs),
    /// Disable a writer region.
    Disable(WriterToggleArgs),
}

#[derive(Debug, Clone, Subcommand)]
pub enum CoordinatorCommand {
    /// Add a managed active-active coordinator.
    Add(CoordinatorAddArgs),
    /// Show managed coordinator control-plane status.
    Status(CoordinatorStatusArgs),
    /// Remove a managed coordinator.
    Remove(CoordinatorRemoveArgs),
}

#[derive(Debug, Clone, Parser)]
pub struct CoordinatorAddArgs {
    /// Coordinator backend provider.
    #[arg(long)]
    pub provider: CoordinatorProviderArg,
    /// Coordinator resource name.
    #[arg(long)]
    pub name: String,
    /// Primary coordinator region.
    #[arg(long)]
    pub region: String,
    /// Managed coordinator failover region.
    #[arg(long = "failover-region")]
    pub failover_regions: Vec<String>,
    /// Print the coordinator setup plan without changing cloud resources.
    #[arg(long, conflicts_with = "apply")]
    pub dry_run: bool,
    /// Apply coordinator resources through Crab's cloud control plane.
    #[arg(long)]
    pub apply: bool,
    /// Structured JSON output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Parser)]
pub struct CoordinatorStatusArgs {
    /// Coordinator backend provider. Uses configured coordinator when omitted.
    #[arg(long)]
    pub provider: Option<CoordinatorProviderArg>,
    /// Coordinator resource name. Uses configured coordinator when omitted.
    #[arg(long)]
    pub name: Option<String>,
    /// Primary coordinator region. Uses configured coordinator when omitted.
    #[arg(long)]
    pub region: Option<String>,
    /// Managed coordinator failover region.
    #[arg(long = "failover-region")]
    pub failover_regions: Vec<String>,
    /// Structured JSON output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Parser)]
pub struct CoordinatorRemoveArgs {
    /// Coordinator backend provider. Uses configured coordinator when omitted.
    #[arg(long)]
    pub provider: Option<CoordinatorProviderArg>,
    /// Coordinator resource name. Uses configured coordinator when omitted.
    #[arg(long)]
    pub name: Option<String>,
    /// Primary coordinator region. Uses configured coordinator when omitted.
    #[arg(long)]
    pub region: Option<String>,
    /// Managed coordinator failover region.
    #[arg(long = "failover-region")]
    pub failover_regions: Vec<String>,
    /// Remove coordinator resources through Crab's cloud control plane.
    #[arg(long)]
    pub apply: bool,
    /// Structured JSON output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Parser)]
pub struct WritersStatusArgs {
    /// Structured JSON output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Parser)]
pub struct WriterToggleArgs {
    /// Writer name.
    pub name: String,
    /// Structured JSON output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Subcommand)]
pub enum FailoverCommand {
    /// Show active-active failover status.
    Status(FailoverStatusArgs),
    /// Plan the next safe failover automation action.
    Plan(FailoverPlanArgs),
    /// Apply one safe failover automation action.
    Run(FailoverRunArgs),
    /// Fence active-active writes by incrementing the coordinator epoch.
    Fence(FailoverFenceArgs),
    /// Resume active-active writes after an epoch has been fenced.
    Resume(FailoverResumeArgs),
}

#[derive(Debug, Clone, Parser)]
pub struct FailoverStatusArgs {
    /// Structured JSON output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Parser)]
pub struct FailoverPlanArgs {
    /// Writer reported unhealthy by an external regional health check.
    #[arg(long = "writer-unhealthy")]
    pub unhealthy_writers: Vec<String>,
    /// Confirm coordinator-backed repair and external failover checks completed.
    #[arg(long)]
    pub repair_verified: bool,
    /// Structured JSON output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Parser)]
pub struct FailoverRunArgs {
    /// Mutate coordinator state or repair manifests when the plan is actionable.
    #[arg(long)]
    pub apply: bool,
    /// Writer reported unhealthy by an external regional health check.
    #[arg(long = "writer-unhealthy")]
    pub unhealthy_writers: Vec<String>,
    /// Confirm coordinator-backed repair and external failover checks completed.
    #[arg(long)]
    pub repair_verified: bool,
    /// Structured JSON output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Parser)]
pub struct FailoverFenceArgs {
    /// Mutate the managed coordinator state.
    #[arg(long)]
    pub apply: bool,
    /// Operator-visible reason recorded with the fenced coordinator.
    #[arg(long)]
    pub reason: Option<String>,
    /// Structured JSON output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Parser)]
pub struct FailoverResumeArgs {
    /// Mutate the managed coordinator state.
    #[arg(long)]
    pub apply: bool,
    /// Confirm coordinator-backed repair and external failover checks completed.
    #[arg(long)]
    pub repair_verified: bool,
    /// Structured JSON output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Parser)]
pub struct RepairArgs {
    /// Reconcile from coordinator truth.
    #[arg(long)]
    pub from_coordinator: bool,
    /// Print planned repair actions without applying them.
    #[arg(long)]
    pub dry_run: bool,
    /// Print a supervisor template for the long-running repair worker.
    #[arg(long = "service-template", value_enum, conflicts_with_all = ["watch", "json", "jsonl"])]
    pub service_template: Option<RepairServiceTemplateArg>,
    /// Service/deployment name used by --service-template.
    #[arg(
        long = "service-name",
        default_value = "crab-replica-repair",
        requires = "service_template"
    )]
    pub service_name: String,
    /// Working directory for the supervised repair worker.
    #[arg(
        long = "working-directory",
        value_name = "PATH",
        requires = "service_template"
    )]
    pub working_directory: Option<PathBuf>,
    /// Container image for Kubernetes repair-worker templates.
    #[arg(
        long = "container-image",
        default_value = "ghcr.io/crab-build/crab:latest",
        requires = "service_template"
    )]
    pub container_image: String,
    /// Write the rendered service template atomically instead of printing it.
    #[arg(long, value_name = "PATH", requires = "service_template")]
    pub output: Option<PathBuf>,
    /// Continuously repair from coordinator truth until interrupted.
    #[arg(long)]
    pub watch: bool,
    /// Refresh interval in seconds for --watch.
    #[arg(long, default_value_t = DEFAULT_REPAIR_WATCH_INTERVAL_SECS, value_name = "SECONDS")]
    pub interval: u64,
    /// Stop watch mode after this many samples. Default is unlimited.
    #[arg(long, value_name = "COUNT", requires = "watch")]
    pub samples: Option<u64>,
    /// Structured JSON output.
    #[arg(long, conflicts_with = "jsonl")]
    pub json: bool,
    /// Newline-delimited JSON repair events.
    #[arg(long, conflicts_with = "json")]
    pub jsonl: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum, PartialEq, Eq)]
pub enum RepairServiceTemplateArg {
    Systemd,
    Launchd,
    Kubernetes,
}

#[derive(Debug, Clone, Parser)]
pub struct PromoteArgs {
    /// Replica name.
    pub name: String,
    /// Print planned promotion without changing config.
    #[arg(long, alias = "plan")]
    pub dry_run: bool,
    /// Promote even if the replica has not been read-enabled.
    #[arg(long)]
    pub force: bool,
    /// Structured JSON output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Parser)]
pub struct SetPrimaryArgs {
    /// New primary Crab remote URL.
    pub url: String,
    /// Apply the primary change. Without this flag, only a DR plan is printed.
    #[arg(long, conflicts_with = "dry_run")]
    pub apply: bool,
    /// Print the guarded DR plan without changing config.
    #[arg(long, alias = "plan")]
    pub dry_run: bool,
    /// Allow setting a configured read-disabled replica after external verification.
    #[arg(long)]
    pub force: bool,
    /// Structured JSON output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Parser)]
pub struct DiagnosticsArgs {
    /// Bypass the local readiness cache and verify every referenced object.
    #[arg(long, alias = "no-cache")]
    pub deep: bool,
    /// Include ordered, non-mutating runbook actions for current findings.
    #[arg(long)]
    pub fix_plan: bool,
    /// Write the diagnostics bundle as pretty JSON.
    #[arg(long, alias = "out", value_name = "PATH")]
    pub output: Option<PathBuf>,
    /// Redact cloud bucket, account, repo, and managed resource identifiers.
    #[arg(long)]
    pub redact: bool,
    /// Publish the redacted diagnostics bundle to the primary remote.
    #[arg(long, requires = "redact")]
    pub publish: bool,
    /// Structured JSON output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Parser)]
pub struct CertifyArgs {
    /// Certification profile.
    #[arg(long, default_value = "enterprise")]
    pub profile: CertificationProfileArg,
    /// Retained live evidence directory to verify before enterprise certification.
    #[arg(long = "evidence-dir", value_name = "PATH")]
    pub evidence_dir: Option<PathBuf>,
    /// Bind enterprise retained evidence to one live workflow run attempt.
    #[arg(long, value_name = "RUN_ID", requires = "evidence_dir")]
    pub expected_run_id: Option<String>,
    /// Write the certification evidence bundle as pretty JSON.
    #[arg(long, alias = "out", value_name = "PATH")]
    pub output: Option<PathBuf>,
    /// Redact cloud bucket, account, repo, and managed resource identifiers.
    #[arg(long)]
    pub redact: bool,
    /// Structured JSON output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Subcommand)]
pub enum EvidenceCommand {
    /// Verify retained live control-plane and cross-region smoke evidence.
    Verify(EvidenceVerifyArgs),
}

#[derive(Debug, Clone, Parser)]
pub struct EvidenceVerifyArgs {
    /// Directory containing retained evidence JSON artifacts.
    pub dir: PathBuf,
    /// Fail unless every evidence artifact is redacted.
    #[arg(long)]
    pub require_redacted: bool,
    /// Require a complete milestone set for a retained evidence profile.
    #[arg(long, value_enum, default_value = "artifacts")]
    pub profile: EvidenceVerifyProfile,
    /// Require known live evidence artifacts to match a specific run_id.
    /// Enterprise evidence requires replica-live-<run-id>-<attempt>.
    #[arg(long, value_name = "RUN_ID")]
    pub expected_run_id: Option<String>,
    /// Structured JSON output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum CertificationProfileArg {
    Enterprise,
    ReadReplica,
    ActiveActive,
}

impl CertificationProfileArg {
    fn as_str(self) -> &'static str {
        match self {
            Self::Enterprise => "enterprise",
            Self::ReadReplica => "read-replica",
            Self::ActiveActive => "active-active",
        }
    }
}

#[derive(Debug, Clone, Parser)]
pub struct StatusArgs {
    /// Bypass the local readiness cache and verify every referenced object.
    #[arg(long, alias = "no-cache")]
    pub deep: bool,
    /// Continuously refresh replica status until interrupted.
    #[arg(long, conflicts_with_all = ["json", "prometheus"])]
    pub watch: bool,
    /// Refresh interval in seconds for --watch.
    #[arg(long, default_value_t = DEFAULT_STATUS_WATCH_INTERVAL_SECS, value_name = "SECONDS")]
    pub interval: u64,
    /// Structured JSON output.
    #[arg(long, conflicts_with_all = ["jsonl", "prometheus"])]
    pub json: bool,
    /// Newline-delimited JSON result event.
    #[arg(long, conflicts_with_all = ["json", "prometheus"])]
    pub jsonl: bool,
    /// Prometheus text exposition for replica health metrics.
    #[arg(long, conflicts_with_all = ["json", "jsonl"])]
    pub prometheus: bool,
}

#[derive(Debug, Clone, Parser)]
pub struct DoctorArgs {
    /// Bypass the local readiness cache and verify every referenced object.
    #[arg(long, alias = "no-cache")]
    pub deep: bool,
    /// Include ordered, non-mutating runbook actions for current findings.
    #[arg(long)]
    pub fix_plan: bool,
    /// Structured JSON output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Parser)]
pub struct RemoveArgs {
    /// Replica name.
    pub name: String,
    /// Remove Crab-managed provider resources before removing local config.
    #[arg(long)]
    pub apply: bool,
    /// Structured JSON output.
    #[arg(long)]
    pub json: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ProviderArg {
    S3,
    Gcs,
    Azure,
}

impl From<ProviderArg> for ReplicationProviderKind {
    fn from(value: ProviderArg) -> Self {
        match value {
            ProviderArg::S3 => Self::S3,
            ProviderArg::Gcs => Self::Gcs,
            ProviderArg::Azure => Self::Azure,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum RpoArg {
    Standard,
    Fast,
}

#[derive(Debug, Clone, Copy, ValueEnum, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum RunbookScenarioArg {
    PrimaryOutage,
    ReplicaStale,
    FailedBackfill,
    PolicyDrift,
    DestinationWrites,
}

impl RunbookScenarioArg {
    fn as_str(self) -> &'static str {
        match self {
            Self::PrimaryOutage => "primary-outage",
            Self::ReplicaStale => "replica-stale",
            Self::FailedBackfill => "failed-backfill",
            Self::PolicyDrift => "policy-drift",
            Self::DestinationWrites => "destination-writes",
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ExportFormatArg {
    Terraform,
    Cloudformation,
    Bicep,
}

impl From<ExportFormatArg> for ControlPlaneExportFormat {
    fn from(value: ExportFormatArg) -> Self {
        match value {
            ExportFormatArg::Terraform => Self::Terraform,
            ExportFormatArg::Cloudformation => Self::CloudFormation,
            ExportFormatArg::Bicep => Self::Bicep,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum CoordinatorProviderArg {
    Dynamodb,
    Spanner,
    Cosmosdb,
}

impl From<CoordinatorProviderArg> for ManagedCoordinatorProvider {
    fn from(value: CoordinatorProviderArg) -> Self {
        match value {
            CoordinatorProviderArg::Dynamodb => Self::DynamoDb,
            CoordinatorProviderArg::Spanner => Self::Spanner,
            CoordinatorProviderArg::Cosmosdb => Self::CosmosDb,
        }
    }
}

impl From<RpoArg> for ReplicationRpo {
    fn from(value: RpoArg) -> Self {
        match value {
            RpoArg::Standard => Self::Standard,
            RpoArg::Fast => Self::Fast,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ModeArg {
    ReadReplica,
    ActiveActive,
}

impl From<ModeArg> for ReplicationMode {
    fn from(value: ModeArg) -> Self {
        match value {
            ModeArg::ReadReplica => Self::ReadReplica,
            ModeArg::ActiveActive => Self::ActiveActive,
        }
    }
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct AddPayload {
    pub configured: bool,
    pub applied: bool,
    pub plan: ReplicationControlPlanePlan,
    pub apply_status: Option<ControlPlaneApplyStatus>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ExportPayload {
    pub format: ControlPlaneExportFormat,
    pub plan: ReplicationControlPlanePlan,
    pub body: String,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CostPayload {
    pub primary: Option<String>,
    pub assumptions: CostAssumptions,
    pub estimates: Vec<ReplicaCostEstimate>,
    pub totals: CostTotals,
    pub pricing_notice: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, JsonSchema)]
pub struct CostAssumptions {
    pub monthly_write_gb: f64,
    pub monthly_read_gb: f64,
    pub backfill_gb: f64,
    pub monthly_requests_million: f64,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ReplicaCostEstimate {
    pub name: String,
    pub provider: ReplicationProviderKind,
    pub region: String,
    pub rpo: ReplicationRpo,
    pub read_enabled: bool,
    pub backfill_configured: bool,
    pub meters: Vec<CostMeter>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CostMeter {
    pub code: String,
    pub description: String,
    pub quantity: f64,
    pub unit: String,
    pub pricing_input: String,
}

#[derive(Debug, Clone, Copy, Default, Serialize, JsonSchema)]
pub struct CostTotals {
    pub replicas: u64,
    pub monthly_replicated_write_gb: f64,
    pub monthly_replica_read_gb: f64,
    pub one_time_backfill_gb: f64,
    pub monthly_request_millions: f64,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RunbookPayload {
    pub scenario: RunbookScenarioArg,
    pub primary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<ReplicationMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replica: Option<RunbookReplicaContext>,
    pub warnings: Vec<String>,
    pub steps: Vec<RunbookStep>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct RunbookReplicaContext {
    pub name: String,
    pub provider: ReplicationProviderKind,
    pub url: String,
    pub region: String,
    pub read_enabled: bool,
    pub backfill_configured: bool,
    pub rpo: ReplicationRpo,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct RunbookStep {
    pub order: u64,
    pub title: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    pub rationale: String,
    pub requires_external_verification: bool,
    pub destructive: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct CoordinatorPayload {
    pub configured: bool,
    pub applied: bool,
    pub plan: CoordinatorControlPlanePlan,
    pub apply_status: Option<CoordinatorApplyStatus>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct CoordinatorStatusPayload {
    pub configured: bool,
    pub status: CoordinatorControlPlaneStatus,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct CoordinatorRemovePayload {
    pub configured: bool,
    pub removed_config: bool,
    pub applied: bool,
    pub plan: CoordinatorControlPlanePlan,
    pub apply_status: Option<CoordinatorApplyStatus>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct WaitPayload {
    pub name: String,
    pub ready: bool,
    pub read_enabled: bool,
    pub reason: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct VerifyPayload {
    pub primary: String,
    pub deep: bool,
    pub proof_mode: VerifyProofMode,
    pub exhaustive: bool,
    pub sample_size: Option<u64>,
    pub verified: bool,
    pub summary: VerifySummary,
    pub replicas: Vec<ReplicaStatus>,
}

#[derive(Debug, Clone, Copy, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum VerifyProofMode {
    Exhaustive,
    Sampled,
}

impl VerifyProofMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Exhaustive => "exhaustive",
            Self::Sampled => "sampled",
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct VerifySummary {
    pub proof_mode: VerifyProofMode,
    pub exhaustive: bool,
    pub sample_size: Option<u64>,
    pub replica_count: u64,
    pub ready_count: u64,
    pub not_ready_count: u64,
    pub read_enabled_count: u64,
    pub max_lag_generations: Option<u64>,
    pub readiness_object_probe_count: u64,
    pub readiness_object_read_count: u64,
    pub primary_fallback_bytes: u64,
    pub provider_inventory: Vec<VerifyProviderSummary>,
    pub cutover_ready: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub cutover_blockers: Vec<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct VerifyProviderSummary {
    pub provider: ReplicationProviderKind,
    pub replica_count: u64,
    pub ready_count: u64,
    pub not_ready_count: u64,
    pub regions: Vec<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct BackfillPayload {
    pub primary: String,
    pub replicas: Vec<BackfillReplicaStatus>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct BackfillReplicaStatus {
    pub name: String,
    pub provider: ReplicationProviderKind,
    pub url: String,
    pub region: String,
    pub required: bool,
    pub read_enabled: bool,
    pub state: BackfillState,
    pub blocks_read_enable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress_percent: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub check_code: Option<String>,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remediation: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum BackfillState {
    NotRequired,
    Verified,
    Missing,
    Drifted,
    Unknown,
    Unsupported,
    Untracked,
    Unavailable,
}

impl BackfillState {
    const ALL: [Self; 8] = [
        Self::NotRequired,
        Self::Verified,
        Self::Missing,
        Self::Drifted,
        Self::Unknown,
        Self::Unsupported,
        Self::Untracked,
        Self::Unavailable,
    ];

    fn as_str(self) -> &'static str {
        match self {
            Self::NotRequired => "not-required",
            Self::Verified => "verified",
            Self::Missing => "missing",
            Self::Drifted => "drifted",
            Self::Unknown => "unknown",
            Self::Unsupported => "unsupported",
            Self::Untracked => "untracked",
            Self::Unavailable => "unavailable",
        }
    }
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct TogglePayload {
    pub name: String,
    pub enabled: bool,
    pub changed: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct ModePayload {
    pub mode: ReplicationMode,
    pub active_active: ActiveActiveStatus,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct WritersPayload {
    pub writers: Vec<WriterConfig>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct FailoverPayload {
    pub active_active: ActiveActiveStatus,
    pub automation_policy: FailoverAutomationPolicy,
    pub automation_plan: FailoverAutomationDecision,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coordinator: Option<CoordinatorControlPlaneStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coordinator_health: Option<CoordinatorHealth>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct FailoverAutomationPolicy {
    pub automatic_write_failover_supported: bool,
    pub orchestration: String,
    pub split_brain_policy: String,
    pub required_operator_steps: Vec<String>,
    pub adr: String,
}

#[derive(Debug, Clone, Copy, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum FailoverAutomationAction {
    Hold,
    Monitor,
    Fence,
    Repair,
    Resume,
}

impl FailoverAutomationAction {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hold => "hold",
            Self::Monitor => "monitor",
            Self::Fence => "fence",
            Self::Repair => "repair",
            Self::Resume => "resume",
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct FailoverAutomationDecision {
    pub action: FailoverAutomationAction,
    pub automatic_apply_supported: bool,
    pub reason: String,
    pub unhealthy_writers: Vec<String>,
    pub repair_verified: bool,
    pub commands: Vec<String>,
    pub required_evidence: Vec<String>,
}

const FAILOVER_ORCHESTRATION: &str = "manual-fence-repair-resume";
const FAILOVER_SPLIT_BRAIN_POLICY: &str = "fail-closed";
const FAILOVER_ADR: &str = "crab/docs/design/replica-active-active-failover.md";

#[derive(Debug, Serialize, JsonSchema)]
pub struct FailoverOperationPayload {
    pub operation: ActiveActiveFailoverOperation,
    pub applied: bool,
    pub repair_verified: bool,
    pub coordinator_url: String,
    pub repo_prefix: String,
    pub automation_policy: FailoverAutomationPolicy,
    pub planned_actions: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<ActiveActiveFailoverOutcome>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct FailoverRunPayload {
    pub apply_requested: bool,
    pub applied: bool,
    pub active_active: ActiveActiveStatus,
    pub automation_policy: FailoverAutomationPolicy,
    pub automation_plan: FailoverAutomationDecision,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked_reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation: Option<FailoverOperationPayload>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repair: Option<RepairPayload>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coordinator: Option<CoordinatorControlPlaneStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coordinator_health: Option<CoordinatorHealth>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct RepairPayload {
    pub from_coordinator: bool,
    pub dry_run: bool,
    pub planned_actions: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coordinator_plan: Option<ActiveActiveRepairPlan>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct RepairWatchWorkerState {
    schema_version: u32,
    worker_id: String,
    pid: u32,
    lease_path: String,
    acquired_at_ms: u64,
    heartbeat_at_ms: u64,
    expires_at_ms: u64,
    base_interval_seconds: u64,
    next_interval_seconds: u64,
    consecutive_errors: u64,
    dry_run: bool,
}

#[derive(Debug)]
struct RepairWatchLeaseGuard {
    path: PathBuf,
    state: RepairWatchWorkerState,
    ttl_seconds: u64,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct PromotePayload {
    pub name: String,
    pub old_primary: String,
    pub new_primary: String,
    pub dry_run: bool,
    pub forced: bool,
    pub read_enabled: bool,
    pub new_primary_is_crab_url: bool,
    pub provider: ReplicationProviderKind,
    pub region: String,
    pub plan_ready: bool,
    pub plan_checks: Vec<PromotePlanCheck>,
    pub planned_actions: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub control_plane: Option<ControlPlaneStatus>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SetPrimaryPayload {
    pub old_primary: String,
    pub new_primary: String,
    pub applied: bool,
    pub forced: bool,
    pub new_primary_is_crab_url: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_replica: Option<String>,
    pub plan_ready: bool,
    pub plan_checks: Vec<PromotePlanCheck>,
    pub planned_actions: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub control_plane: Option<ControlPlaneStatus>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct PromotePlanCheck {
    pub code: String,
    pub state: PromotePlanCheckState,
    pub message: String,
    pub remediation: String,
    pub blocking: bool,
}

#[derive(Debug, Clone, Copy, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum PromotePlanCheckState {
    Passed,
    Warning,
    Blocked,
}

impl PromotePlanCheckState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Warning => "warning",
            Self::Blocked => "blocked",
        }
    }
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct StatusPayload {
    pub primary: Option<String>,
    pub replicas: Vec<ReplicaStatus>,
    pub health: Vec<ReplicaHealth>,
    pub backfill: Vec<BackfillReplicaStatus>,
    pub control_plane: Vec<ControlPlaneStatus>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct ReplicaHealth {
    pub name: String,
    pub provider: ReplicationProviderKind,
    pub region: String,
    pub state: ReplicaHealthState,
    pub reason: String,
}

#[derive(Debug, Clone, Copy, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ReplicaHealthState {
    Ready,
    Lagging,
    Partial,
    AuthFailed,
    PolicyDrift,
    BackfillRunning,
    Disabled,
}

impl ReplicaHealthState {
    const ALL: [Self; 7] = [
        Self::Ready,
        Self::Lagging,
        Self::Partial,
        Self::AuthFailed,
        Self::PolicyDrift,
        Self::BackfillRunning,
        Self::Disabled,
    ];

    fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Lagging => "lagging",
            Self::Partial => "partial",
            Self::AuthFailed => "auth-failed",
            Self::PolicyDrift => "policy-drift",
            Self::BackfillRunning => "backfill-running",
            Self::Disabled => "disabled",
        }
    }
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct DoctorPayload {
    pub primary: Option<String>,
    pub deep: bool,
    pub replicas: Vec<ReplicaStatus>,
    pub health: Vec<ReplicaHealth>,
    pub backfill: Vec<BackfillReplicaStatus>,
    pub control_plane: Vec<ControlPlaneStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coordinator: Option<CoordinatorControlPlaneStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coordinator_health: Option<CoordinatorHealth>,
    pub active_active: ActiveActiveStatus,
    pub findings: Vec<DoctorFinding>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fix_plan: Vec<DoctorFixAction>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct DiagnosticsPayload {
    pub collected_at_ms: u64,
    pub deep: bool,
    pub fix_plan_included: bool,
    pub redacted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub published: Option<DiagnosticsPublicationPayload>,
    pub status: StatusPayload,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coordinator: Option<CoordinatorControlPlaneStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coordinator_health: Option<CoordinatorHealth>,
    pub active_active: ActiveActiveStatus,
    pub findings: Vec<DoctorFinding>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fix_plan: Vec<DoctorFixAction>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct DiagnosticsPublicationPayload {
    pub primary: String,
    pub object_key: String,
    pub redacted: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct CertificationPayload {
    pub collected_at_ms: u64,
    pub profile: CertificationProfileArg,
    pub certified: bool,
    pub deep: bool,
    pub redacted: bool,
    pub status: StatusPayload,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coordinator: Option<CoordinatorControlPlaneStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coordinator_health: Option<CoordinatorHealth>,
    pub active_active: ActiveActiveStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<CertificationEvidencePayload>,
    pub gates: Vec<CertificationGate>,
    pub findings: Vec<DoctorFinding>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub fix_plan: Vec<DoctorFixAction>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct CertificationEvidencePayload {
    pub directory: String,
    pub verified: bool,
    pub require_redacted: bool,
    pub profile: EvidenceVerifyProfile,
    pub summary: EvidenceVerifySummary,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gates: Vec<EvidenceVerifyGate>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LiveControlPlaneEvidencePayload {
    pub schema: LiveControlPlaneEvidenceSchema,
    pub version: LiveEvidenceSchemaVersion,
    pub collected_at_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sequence: Option<u64>,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    pub redacted: bool,
    pub args: Vec<String>,
    pub result: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LiveSmokeEvidencePayload {
    pub schema: LiveSmokeEvidenceSchema,
    pub version: LiveEvidenceSchemaVersion,
    pub collected_at_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sequence: Option<u64>,
    pub label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coordinator_provider: Option<String>,
    pub redacted: bool,
    pub cwd: String,
    pub args: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdout_json: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stdout: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub enum LiveControlPlaneEvidenceSchema {
    #[serde(rename = "replica.live-control-plane.evidence")]
    ReplicaLiveControlPlaneEvidence,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub enum LiveSmokeEvidenceSchema {
    #[serde(rename = "replica.live-smoke.evidence")]
    ReplicaLiveSmokeEvidence,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
pub enum LiveEvidenceSchemaVersion {
    #[serde(rename = "1.0")]
    V1,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct EvidenceVerifyPayload {
    pub directory: String,
    pub verified: bool,
    pub require_redacted: bool,
    pub profile: EvidenceVerifyProfile,
    pub summary: EvidenceVerifySummary,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gates: Vec<EvidenceVerifyGate>,
    pub files: Vec<EvidenceFileStatus>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct EvidenceVerifySummary {
    pub files_seen: u64,
    pub files_verified: u64,
    pub files_failed: u64,
    pub control_plane_evidence: u64,
    pub smoke_evidence: u64,
    pub redacted: u64,
    pub unredacted: u64,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct EvidenceVerifyGate {
    pub code: String,
    pub state: CertificationGateState,
    pub message: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remediation: Option<String>,
}

#[derive(Debug, Clone, Copy, ValueEnum, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum EvidenceVerifyProfile {
    Artifacts,
    ControlPlaneStatus,
    ControlPlaneMutate,
    ProviderHydrate,
    ActiveActiveSmoke,
    Enterprise,
}

impl EvidenceVerifyProfile {
    fn as_str(self) -> &'static str {
        match self {
            Self::Artifacts => "artifacts",
            Self::ControlPlaneStatus => "control-plane-status",
            Self::ControlPlaneMutate => "control-plane-mutate",
            Self::ProviderHydrate => "provider-hydrate",
            Self::ActiveActiveSmoke => "active-active-smoke",
            Self::Enterprise => "enterprise",
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct EvidenceFileStatus {
    pub path: String,
    pub state: EvidenceFileState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<EvidenceKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub harness: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sequence: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coordinator_provider: Option<String>,
    #[serde(skip)]
    #[schemars(skip)]
    writer_region: Option<String>,
    #[serde(skip)]
    #[schemars(skip)]
    reader_region: Option<String>,
    #[serde(skip)]
    #[schemars(skip)]
    push_operation_id: Option<String>,
    #[serde(skip)]
    #[schemars(skip)]
    repair_service_template: Option<String>,
    #[serde(skip)]
    #[schemars(skip)]
    repair_template_blake3: Option<String>,
    #[serde(skip)]
    #[schemars(skip)]
    repair_command_blake3: Option<String>,
    #[serde(skip)]
    #[schemars(skip)]
    provider_log_artifact_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redacted: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collected_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum EvidenceFileState {
    Verified,
    Failed,
}

impl EvidenceFileState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::Failed => "failed",
        }
    }
}

const STORAGE_EVIDENCE_PROVIDERS: [&str; 3] = ["s3", "gcs", "azure"];
const COORDINATOR_EVIDENCE_PROVIDERS: [&str; 3] = ["dynamodb", "spanner", "cosmosdb"];
const STORAGE_PROVIDER_LOG_LABEL: &str = "storage-provider-log";
const COORDINATOR_PROVIDER_LOG_LABEL: &str = "coordinator-provider-log";
const PRODUCTION_LOAD_LABEL: &str = "production-load";
const PROVIDER_HYDRATE_SEQUENCE: &[&str] = &[
    "provider-hydrate-init",
    "provider-hydrate-push",
    "provider-hydrate-copy",
    "provider-hydrate-read-enabled",
    "provider-hydrate-primary-xorbs-deleted",
    "provider-hydrate-selected-replica",
];
const CONTROL_PLANE_EVIDENCE_HARNESS: &str = "replica-live-control-plane";
const PROVIDER_HYDRATE_EVIDENCE_HARNESS: &str = "replica-binary-hydrate-live";
const ACTIVE_ACTIVE_EVIDENCE_HARNESS: &str = "replica-live-cross-region";
const PRODUCTION_LOAD_EVIDENCE_HARNESS: &str = "replica-live-load";

#[derive(Debug, Clone, Copy, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum EvidenceKind {
    LiveControlPlane,
    LiveSmoke,
}

impl EvidenceKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::LiveControlPlane => "live-control-plane",
            Self::LiveSmoke => "live-smoke",
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct CertificationGate {
    pub code: String,
    pub state: CertificationGateState,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remediation: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum CertificationGateState {
    Passed,
    Failed,
}

impl CertificationGateState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
        }
    }
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct DoctorFinding {
    pub code: String,
    pub severity: DoctorSeverity,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replica: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remediation: Option<String>,
}

#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct DoctorFixAction {
    pub code: String,
    pub severity: DoctorSeverity,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replica: Option<String>,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub cost_hints: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub risk_hints: Vec<String>,
    pub destructive: bool,
}

#[derive(Debug, Clone, Copy, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum DoctorSeverity {
    Info,
    Warning,
    Error,
}

impl DoctorSeverity {
    fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct RemovePayload {
    pub removed: bool,
    pub name: String,
    pub applied: bool,
    pub apply_status: Option<ControlPlaneApplyStatus>,
}

pub async fn exec(command: ReplicaCommand, cancel: &CancellationToken) -> Result<()> {
    match command {
        ReplicaCommand::Add(args) => run_add(&args).await,
        ReplicaCommand::Export(args) => run_export(&args),
        ReplicaCommand::Cost(args) => run_cost(&args),
        ReplicaCommand::Runbook(args) => run_runbook(&args),
        ReplicaCommand::Wait(args) => run_wait(&args, cancel).await,
        ReplicaCommand::Verify(args) => run_verify(&args, cancel).await,
        ReplicaCommand::Backfill(BackfillCommand::Status(args)) => run_backfill_status(&args).await,
        ReplicaCommand::Enable(args) => run_replica_enable(&args, cancel).await,
        ReplicaCommand::Disable(args) => run_replica_toggle(&args.name, false, args.json),
        ReplicaCommand::Mode(args) => run_mode(&args),
        ReplicaCommand::Writers(command) => run_writers(command),
        ReplicaCommand::Coordinator(command) => run_coordinator(command).await,
        ReplicaCommand::Failover(command) => run_failover(command).await,
        ReplicaCommand::Repair(args) => run_repair(&args, cancel).await,
        ReplicaCommand::Promote(args) => run_promote(&args).await,
        ReplicaCommand::SetPrimary(args) => run_set_primary(&args).await,
        ReplicaCommand::Diagnostics(args) => run_diagnostics(&args, cancel).await,
        ReplicaCommand::Certify(args) => run_certify(&args, cancel).await,
        ReplicaCommand::Evidence(command) => run_evidence(&command),
        ReplicaCommand::Status(args) => run_status(&args, cancel).await,
        ReplicaCommand::Doctor(args) => run_doctor(&args, cancel).await,
        ReplicaCommand::Remove(args) => run_remove(&args).await,
    }
}

pub async fn run_add(args: &AddArgs) -> Result<()> {
    let provider: ReplicationProviderKind = args.provider.into();
    let rpo: ReplicationRpo = args.rpo.into();
    validate_replica_url_provider(provider, &args.replica)?;
    let plan = control_plane_plan(
        &args.name,
        provider,
        &args.primary,
        &args.replica,
        &args.region,
        rpo,
        args.backfill,
    );
    let mode = OutputMode::from_flags(args.json, false);
    let apply_status = if args.apply {
        Some(apply_control_plane_plan(&plan).await?)
    } else {
        None
    };

    if !args.dry_run {
        let cwd = std::env::current_dir()?;
        let path = project_config_path(&cwd);
        add_replica_to_project_config(&path, args, provider, rpo)?;
    }

    let payload = AddPayload {
        configured: !args.dry_run,
        applied: args.apply,
        plan,
        apply_status,
    };
    render_add(&payload, mode);
    Ok(())
}

pub fn run_export(args: &ExportArgs) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let (primary, config) = resolved_replication_context(&cwd)?;
    let primary = primary.ok_or_else(|| CrabError::Configuration {
        key: "remote.url".into(),
        origin: "replica export requires a configured primary remote".into(),
    })?;
    let replica = select_configured_replica(&config, args.name.as_deref())?;
    let format: ControlPlaneExportFormat = args.format.into();
    let plan = control_plane_plan(
        &replica.name,
        replica.provider,
        &primary,
        &replica.url,
        &replica.region,
        replica.rpo,
        replica.backfill,
    );
    let body = export_control_plane_plan(&plan, format)?;
    let payload = ExportPayload { format, plan, body };
    render_export(&payload, OutputMode::from_flags(args.json, false));
    Ok(())
}

pub fn run_cost(args: &CostArgs) -> Result<()> {
    let assumptions = cost_assumptions_from_args(args)?;
    let cwd = std::env::current_dir()?;
    let (primary, config) = resolved_replication_context(&cwd)?;
    let replication = config
        .replication
        .as_ref()
        .ok_or_else(|| CrabError::Configuration {
            key: "replication".into(),
            origin: "replica cost requires configured replication".into(),
        })?;
    let payload = cost_payload(primary, replication, args.name.as_deref(), assumptions)?;
    render_cost(&payload, OutputMode::from_flags(args.json, false));
    Ok(())
}

pub fn run_runbook(args: &RunbookArgs) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let (primary, config) = resolved_replication_context(&cwd)?;
    let payload = runbook_payload(
        args.scenario,
        primary,
        config.replication.as_ref(),
        args.name.as_deref(),
    )?;
    render_runbook(&payload, OutputMode::from_flags(args.json, false));
    Ok(())
}

pub async fn run_wait(args: &WaitArgs, cancel: &CancellationToken) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let (primary, config) = resolved_replication_context(&cwd)?;
    let Some(primary) = primary else {
        return Err(CrabError::Configuration {
            key: "remote.url".into(),
            origin: "replica wait requires a configured primary remote".into(),
        });
    };
    let parsed = CrabUrl::parse(&primary)?;
    let statuses = replica_statuses_with_options(
        &config,
        &parsed,
        "replica-wait",
        cancel,
        ReadinessCheckOptions::deep(),
    )
    .await?;
    let status = statuses
        .into_iter()
        .find(|status| status.name == args.name)
        .ok_or_else(|| CrabError::Configuration {
            key: "replication.replicas".into(),
            origin: format!("replica {} is not configured", args.name),
        })?;
    let configured_replica = select_configured_replica(&config, Some(&args.name))?;
    let control_plane = if args.enable_read {
        control_plane_statuses(Some(&primary), config.replication.as_ref()).await
    } else {
        Vec::new()
    };
    let cutover_blocker = if args.enable_read {
        replica_read_cutover_blocker(&status, configured_replica, &control_plane)
    } else {
        None
    };
    let ready = if args.enable_read {
        cutover_blocker.is_none()
    } else {
        status.ready
    };

    if ready && args.enable_read {
        set_replica_read_enabled(&project_config_path(&cwd), &args.name, true)?;
    }

    let payload = WaitPayload {
        name: args.name.clone(),
        ready,
        read_enabled: if ready && args.enable_read {
            true
        } else {
            status.read_enabled
        },
        reason: cutover_blocker.or(status.last_fallback_reason),
    };
    render_wait(&payload, OutputMode::from_flags(args.json, false));
    if payload.ready {
        Ok(())
    } else {
        Err(CrabError::Configuration {
            key: format!("replication.replicas.{}", args.name),
            origin: payload
                .reason
                .clone()
                .unwrap_or_else(|| "replica is not ready".to_owned()),
        })
    }
}

pub async fn run_verify(args: &VerifyArgs, cancel: &CancellationToken) -> Result<()> {
    if !args.deep {
        return Err(CrabError::Configuration {
            key: "replica.verify.deep".into(),
            origin: "replica verify requires --deep so cached readiness is never mistaken for production proof".into(),
        });
    }
    let sample_size = verify_sample_size(args)?;
    let proof_mode = if sample_size.is_some() {
        VerifyProofMode::Sampled
    } else {
        VerifyProofMode::Exhaustive
    };
    let readiness_options = match sample_size {
        Some(sample_size) => ReadinessCheckOptions::sampled(sample_size),
        None => ReadinessCheckOptions::deep(),
    };

    let cwd = std::env::current_dir()?;
    let (primary, config) = resolved_replication_context(&cwd)?;
    let primary = primary.ok_or_else(|| CrabError::Configuration {
        key: "remote.url".into(),
        origin: "replica verify requires a configured primary remote".into(),
    })?;
    let parsed = CrabUrl::parse(&primary)?;
    let mut replicas = replica_statuses_with_options(
        &config,
        &parsed,
        "replica-verify",
        cancel,
        readiness_options,
    )
    .await?;
    if let Some(name) = args.name.as_deref() {
        replicas.retain(|status| status.name == name);
        if replicas.is_empty() {
            return Err(CrabError::Configuration {
                key: "replication.replicas".into(),
                origin: format!("replica {name} is not configured"),
            });
        }
    }
    if replicas.is_empty() {
        return Err(CrabError::Configuration {
            key: "replication.replicas".into(),
            origin: "no replicas are configured".into(),
        });
    }

    let verified = replicas.iter().all(|status| status.ready);
    let failure = verify_failure_reason(&replicas);
    let summary = verify_summary(&replicas, proof_mode, sample_size);
    let payload = VerifyPayload {
        primary,
        deep: true,
        proof_mode,
        exhaustive: proof_mode == VerifyProofMode::Exhaustive,
        sample_size,
        verified,
        summary,
        replicas,
    };
    render_verify(&payload, OutputMode::from_flags(args.json, false));
    if payload.verified {
        return Ok(());
    }
    Err(CrabError::Configuration {
        key: "replication.replicas".into(),
        origin: failure.unwrap_or_else(|| "one or more replicas failed verification".to_owned()),
    })
}

pub fn run_replica_toggle(name: &str, enabled: bool, json: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let changed = set_replica_read_enabled(&project_config_path(&cwd), name, enabled)?;
    let payload = TogglePayload {
        name: name.to_owned(),
        enabled,
        changed,
    };
    render_toggle(&payload, OutputMode::from_flags(json, false), "replica");
    Ok(())
}

pub async fn run_replica_enable(args: &EnableArgs, cancel: &CancellationToken) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let changed = enable_replica_reads_after_cutover_check(&cwd, &args.name, cancel).await?;
    let payload = TogglePayload {
        name: args.name.clone(),
        enabled: true,
        changed,
    };
    render_toggle(
        &payload,
        OutputMode::from_flags(args.json, false),
        "replica",
    );
    Ok(())
}

async fn enable_replica_reads_after_cutover_check(
    root: &Path,
    name: &str,
    cancel: &CancellationToken,
) -> Result<bool> {
    let (primary, config) = resolved_replication_context(root)?;
    let primary = primary.ok_or_else(|| CrabError::Configuration {
        key: "remote.url".into(),
        origin: "replica enable requires a configured primary remote".into(),
    })?;
    let parsed = CrabUrl::parse(&primary)?;
    let status = replica_statuses_with_options(
        &config,
        &parsed,
        "replica-enable",
        cancel,
        ReadinessCheckOptions::deep(),
    )
    .await?
    .into_iter()
    .find(|status| status.name == name)
    .ok_or_else(|| CrabError::Configuration {
        key: "replication.replicas".into(),
        origin: format!("replica {name} is not configured"),
    })?;
    let configured_replica = select_configured_replica(&config, Some(name))?;
    let control_plane = control_plane_statuses(Some(&primary), config.replication.as_ref()).await;
    if let Some(reason) = replica_read_cutover_blocker(&status, configured_replica, &control_plane)
    {
        return Err(CrabError::Configuration {
            key: format!("replication.replicas.{name}.read"),
            origin: format!("replica {name} cannot be enabled for reads: {reason}"),
        });
    }
    set_replica_read_enabled(&project_config_path(root), name, true)
}

pub async fn run_backfill_status(args: &BackfillStatusArgs) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let (primary, config) = resolved_replication_context(&cwd)?;
    let payload = backfill_payload(primary.as_deref(), &config, args.name.as_deref()).await?;
    render_backfill_status(&payload, OutputMode::from_flags(args.json, false));
    Ok(())
}

pub fn run_mode(args: &ModeArgs) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let path = project_config_path(&cwd);
    let mut config = ProjectConfig::load(&path)?;
    let mut replication = config.replication.take().unwrap_or_default();
    replication.primary = replication
        .primary
        .or_else(|| Some(config.remote.url.clone()));
    replication.mode = args.mode.into();

    if replication.mode == ReplicationMode::ActiveActive {
        let coordinator_url = args
            .coordinator
            .clone()
            .ok_or_else(|| CrabError::Configuration {
                key: "replication.coordinator.url".into(),
                origin: "active-active mode requires --coordinator".into(),
            })?;
        replication.coordinator = Some(ReplicationCoordinatorConfig {
            kind: ReplicationCoordinatorKind::Managed,
            url: coordinator_url,
            region: args.coordinator_region.clone(),
            failover_regions: args.failover_regions.clone(),
            consistency: ReplicationCoordinatorConsistency::Linearizable,
        });
        replication.writers = args
            .writers
            .iter()
            .map(|raw| parse_writer_spec(raw))
            .collect::<Result<Vec<_>>>()?;
        validate_active_active_config(&replication)?;
    }

    config.replication = Some(replication.clone());
    ProjectConfig::write(&path, &config)?;

    let payload = ModePayload {
        mode: replication.mode,
        active_active: active_active_status(Some(&replication)),
    };
    render_mode(&payload, OutputMode::from_flags(args.json, false));
    Ok(())
}

pub fn run_writers(command: WritersCommand) -> Result<()> {
    match command {
        WritersCommand::Status(args) => {
            let cwd = std::env::current_dir()?;
            let config = ProjectConfig::load(&project_config_path(&cwd))?;
            let writers = config.replication.map_or_else(Vec::new, |r| r.writers);
            render_writers(
                &WritersPayload { writers },
                OutputMode::from_flags(args.json, false),
            );
            Ok(())
        }
        WritersCommand::Enable(args) => run_writer_toggle(&args.name, true, args.json),
        WritersCommand::Disable(args) => run_writer_toggle(&args.name, false, args.json),
    }
}

pub async fn run_coordinator(command: CoordinatorCommand) -> Result<()> {
    match command {
        CoordinatorCommand::Add(args) => run_coordinator_add(&args).await,
        CoordinatorCommand::Status(args) => run_coordinator_status(&args).await,
        CoordinatorCommand::Remove(args) => run_coordinator_remove(&args).await,
    }
}

trait CoordinatorBackendResolver {
    fn backend_for(
        &self,
        provider: ManagedCoordinatorProvider,
    ) -> Option<&dyn CoordinatorControlPlaneBackend>;
}

#[cfg(test)]
struct NoCoordinatorBackends;

#[cfg(test)]
impl CoordinatorBackendResolver for NoCoordinatorBackends {
    fn backend_for(
        &self,
        _provider: ManagedCoordinatorProvider,
    ) -> Option<&dyn CoordinatorControlPlaneBackend> {
        None
    }
}

struct DefaultCoordinatorBackends {
    #[cfg(feature = "coordinator-dynamodb")]
    dynamodb: AwsDynamoDbCoordinatorBackend,
    #[cfg(feature = "coordinator-spanner")]
    spanner: GoogleSpannerCoordinatorBackend,
    #[cfg(feature = "coordinator-cosmosdb")]
    cosmosdb: AzureCosmosDbCoordinatorBackend,
}

impl DefaultCoordinatorBackends {
    fn new() -> Self {
        Self {
            #[cfg(feature = "coordinator-dynamodb")]
            dynamodb: AwsDynamoDbCoordinatorBackend,
            #[cfg(feature = "coordinator-spanner")]
            spanner: GoogleSpannerCoordinatorBackend,
            #[cfg(feature = "coordinator-cosmosdb")]
            cosmosdb: AzureCosmosDbCoordinatorBackend,
        }
    }
}

impl CoordinatorBackendResolver for DefaultCoordinatorBackends {
    fn backend_for(
        &self,
        provider: ManagedCoordinatorProvider,
    ) -> Option<&dyn CoordinatorControlPlaneBackend> {
        #[cfg(feature = "coordinator-dynamodb")]
        if provider == ManagedCoordinatorProvider::DynamoDb {
            return Some(&self.dynamodb);
        }
        #[cfg(feature = "coordinator-spanner")]
        if provider == ManagedCoordinatorProvider::Spanner {
            return Some(&self.spanner);
        }
        #[cfg(feature = "coordinator-cosmosdb")]
        if provider == ManagedCoordinatorProvider::CosmosDb {
            return Some(&self.cosmosdb);
        }
        let _ = provider;
        None
    }
}

pub async fn run_coordinator_add(args: &CoordinatorAddArgs) -> Result<()> {
    let backends = DefaultCoordinatorBackends::new();
    run_coordinator_add_with_backends(args, &backends).await
}

async fn run_coordinator_add_with_backends(
    args: &CoordinatorAddArgs,
    backends: &dyn CoordinatorBackendResolver,
) -> Result<()> {
    let provider: ManagedCoordinatorProvider = args.provider.into();
    let plan = managed_coordinator_plan(provider, &args.name, &args.region, &args.failover_regions);
    let mode = OutputMode::from_flags(args.json, false);
    let apply_status = if args.apply {
        Some(apply_coordinator_plan_with_backends(&plan, backends).await?)
    } else {
        None
    };
    let payload = CoordinatorPayload {
        configured: false,
        applied: args.apply,
        plan,
        apply_status,
    };
    render_coordinator(&payload, mode);
    Ok(())
}

pub async fn run_coordinator_status(args: &CoordinatorStatusArgs) -> Result<()> {
    let backends = DefaultCoordinatorBackends::new();
    run_coordinator_status_with_backends(args, &backends).await
}

async fn run_coordinator_status_with_backends(
    args: &CoordinatorStatusArgs,
    backends: &dyn CoordinatorBackendResolver,
) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let path = project_config_path(&cwd);
    let config = ProjectConfig::load(&path).ok();
    let (plan, configured) = coordinator_plan_from_args_or_config(
        args.provider,
        args.name.as_deref(),
        args.region.as_deref(),
        &args.failover_regions,
        config.as_ref(),
    )?;
    let payload = CoordinatorStatusPayload {
        configured,
        status: inspect_coordinator_plan_with_backends(&plan, backends).await?,
    };
    render_coordinator_status_payload(&payload, OutputMode::from_flags(args.json, false));
    Ok(())
}

pub async fn run_coordinator_remove(args: &CoordinatorRemoveArgs) -> Result<()> {
    let backends = DefaultCoordinatorBackends::new();
    run_coordinator_remove_with_backends(args, &backends).await
}

async fn run_coordinator_remove_with_backends(
    args: &CoordinatorRemoveArgs,
    backends: &dyn CoordinatorBackendResolver,
) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let path = project_config_path(&cwd);
    let config = ProjectConfig::load(&path).ok();
    let (plan, configured) = coordinator_plan_from_args_or_config(
        args.provider,
        args.name.as_deref(),
        args.region.as_deref(),
        &args.failover_regions,
        config.as_ref(),
    )?;
    let remove_plan = coordinator_control_plane_remove_plan(&plan);
    let apply_status = if args.apply {
        Some(remove_coordinator_plan_with_backends(&remove_plan, backends).await?)
    } else {
        None
    };
    let removed_config = if args.apply && configured {
        remove_coordinator_from_project_config(&path)?
    } else {
        false
    };
    let payload = CoordinatorRemovePayload {
        configured,
        removed_config,
        applied: args.apply,
        plan: remove_plan,
        apply_status,
    };
    render_coordinator_remove(&payload, OutputMode::from_flags(args.json, false));
    Ok(())
}

async fn apply_coordinator_plan_with_backends(
    plan: &CoordinatorControlPlanePlan,
    backends: &dyn CoordinatorBackendResolver,
) -> Result<CoordinatorApplyStatus> {
    if let Some(backend) = backends.backend_for(plan.provider) {
        return Ok(apply_coordinator_control_plane_plan_with_backend(plan, backend).await?);
    }
    Ok(apply_coordinator_control_plane_plan(plan)?)
}

async fn inspect_coordinator_plan_with_backends(
    plan: &CoordinatorControlPlanePlan,
    backends: &dyn CoordinatorBackendResolver,
) -> Result<CoordinatorControlPlaneStatus> {
    if let Some(backend) = backends.backend_for(plan.provider) {
        return Ok(inspect_coordinator_control_plane_plan_with_backend(plan, backend).await?);
    }
    Ok(inspect_coordinator_control_plane_plan(plan))
}

async fn remove_coordinator_plan_with_backends(
    plan: &CoordinatorControlPlanePlan,
    backends: &dyn CoordinatorBackendResolver,
) -> Result<CoordinatorApplyStatus> {
    if let Some(backend) = backends.backend_for(plan.provider) {
        return Ok(remove_coordinator_control_plane_plan_with_backend(plan, backend).await?);
    }
    Ok(remove_coordinator_control_plane_plan(plan)?)
}

pub fn run_writer_toggle(name: &str, enabled: bool, json: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let path = project_config_path(&cwd);
    let mut config = ProjectConfig::load(&path)?;
    let replication = config
        .replication
        .as_mut()
        .ok_or_else(|| CrabError::Configuration {
            key: "replication".into(),
            origin: "replication is not configured".into(),
        })?;
    let writer = replication
        .writers
        .iter_mut()
        .find(|writer| writer.name == name)
        .ok_or_else(|| CrabError::Configuration {
            key: "replication.writers".into(),
            origin: format!("writer {name} is not configured"),
        })?;
    let changed = writer.enabled != enabled;
    writer.enabled = enabled;
    validate_active_active_config(replication)?;
    ProjectConfig::write(&path, &config)?;
    let payload = TogglePayload {
        name: name.to_owned(),
        enabled,
        changed,
    };
    render_toggle(&payload, OutputMode::from_flags(json, false), "writer");
    Ok(())
}

pub async fn run_failover(command: FailoverCommand) -> Result<()> {
    match command {
        FailoverCommand::Status(args) => run_failover_status(&args).await,
        FailoverCommand::Plan(args) => run_failover_plan(&args).await,
        FailoverCommand::Run(args) => run_failover_run(&args).await,
        FailoverCommand::Fence(args) => run_failover_fence(&args).await,
        FailoverCommand::Resume(args) => run_failover_resume(&args).await,
    }
}

pub async fn run_failover_status(args: &FailoverStatusArgs) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let snapshot = failover_status_snapshot(&cwd).await;
    render_failover(
        &FailoverPayload {
            automation_plan: failover_automation_decision(
                &snapshot,
                snapshot.replication.as_ref(),
                &[],
                false,
            ),
            active_active: snapshot.active_active,
            automation_policy: failover_automation_policy(),
            coordinator: snapshot.coordinator,
            coordinator_health: snapshot.coordinator_health,
        },
        OutputMode::from_flags(args.json, false),
    );
    Ok(())
}

async fn run_failover_plan(args: &FailoverPlanArgs) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let snapshot = failover_status_snapshot(&cwd).await;
    render_failover_plan(
        &FailoverPayload {
            automation_plan: failover_automation_decision(
                &snapshot,
                snapshot.replication.as_ref(),
                &args.unhealthy_writers,
                args.repair_verified,
            ),
            active_active: snapshot.active_active,
            automation_policy: failover_automation_policy(),
            coordinator: snapshot.coordinator,
            coordinator_health: snapshot.coordinator_health,
        },
        OutputMode::from_flags(args.json, false),
    );
    Ok(())
}

async fn run_failover_run(args: &FailoverRunArgs) -> Result<()> {
    let payload = failover_run_payload(args).await?;
    render_failover_run(&payload, OutputMode::from_flags(args.json, false));
    Ok(())
}

async fn failover_run_payload(args: &FailoverRunArgs) -> Result<FailoverRunPayload> {
    let cwd = std::env::current_dir()?;
    let snapshot = failover_status_snapshot(&cwd).await;
    let decision = failover_automation_decision(
        &snapshot,
        snapshot.replication.as_ref(),
        &args.unhealthy_writers,
        args.repair_verified,
    );
    let mut payload = FailoverRunPayload {
        apply_requested: args.apply,
        applied: false,
        active_active: snapshot.active_active,
        automation_policy: failover_automation_policy(),
        automation_plan: decision,
        blocked_reason: None,
        operation: None,
        repair: None,
        coordinator: snapshot.coordinator,
        coordinator_health: snapshot.coordinator_health,
    };

    if let Some(blocked_reason) =
        failover_automation_apply_blocker(&payload.automation_plan, args.apply)
    {
        payload.blocked_reason = Some(blocked_reason);
        return Ok(payload);
    }
    if !args.apply {
        return Ok(payload);
    }

    match payload.automation_plan.action {
        FailoverAutomationAction::Fence => {
            let reason = format!(
                "writer-unhealthy:{}",
                payload.automation_plan.unhealthy_writers.join(",")
            );
            let operation = failover_operation_payload(
                ActiveActiveFailoverOperation::Fence,
                true,
                Some(&reason),
                false,
            )
            .await?;
            payload.applied = operation.applied;
            payload.operation = Some(operation);
        }
        FailoverAutomationAction::Repair => {
            let repair = repair_payload(&repair_from_coordinator_apply_args()).await?;
            payload.applied = repair
                .coordinator_plan
                .as_ref()
                .is_some_and(|plan| !plan.actions.is_empty());
            payload.repair = Some(repair);
        }
        FailoverAutomationAction::Resume => {
            let operation = failover_operation_payload(
                ActiveActiveFailoverOperation::Resume,
                true,
                None,
                args.repair_verified,
            )
            .await?;
            payload.applied = operation.applied;
            payload.operation = Some(operation);
        }
        FailoverAutomationAction::Hold | FailoverAutomationAction::Monitor => {}
    }

    Ok(payload)
}

fn repair_from_coordinator_apply_args() -> RepairArgs {
    RepairArgs {
        from_coordinator: true,
        dry_run: false,
        service_template: None,
        service_name: "crab-replica-repair".to_owned(),
        working_directory: None,
        container_image: "ghcr.io/crab-build/crab:latest".to_owned(),
        output: None,
        watch: false,
        interval: DEFAULT_REPAIR_WATCH_INTERVAL_SECS,
        samples: None,
        json: false,
        jsonl: false,
    }
}

#[derive(Debug)]
struct FailoverStatusSnapshot {
    active_active: ActiveActiveStatus,
    replication: Option<ReplicationConfig>,
    coordinator: Option<CoordinatorControlPlaneStatus>,
    coordinator_health: Option<CoordinatorHealth>,
}

async fn failover_status_snapshot(root: &Path) -> FailoverStatusSnapshot {
    let config = ProjectConfig::load(&project_config_path(root)).ok();
    let replication = config.as_ref().and_then(|c| c.replication.as_ref());
    let coordinator_probe = coordinator_control_plane_probe(replication).await;
    let coordinator = coordinator_probe.status;
    let mut status =
        active_active_status_with_coordinator_status(replication, coordinator.as_ref());
    apply_coordinator_probe_error_to_active_active_status(
        &mut status,
        coordinator_probe.error.as_deref(),
    );
    let coordinator_health = if status.writes_enabled {
        apply_data_plane_health_to_failover_status(root, &mut status).await
    } else {
        None
    };

    FailoverStatusSnapshot {
        active_active: status,
        replication: replication.cloned(),
        coordinator,
        coordinator_health,
    }
}

async fn apply_data_plane_health_to_failover_status(
    root: &Path,
    status: &mut ActiveActiveStatus,
) -> Option<CoordinatorHealth> {
    let result = async {
        let (primary, config) = resolved_replication_context(root)?;
        let primary = primary.ok_or_else(|| CrabError::Configuration {
            key: "remote.url".into(),
            origin: "active-active failover status requires a configured primary remote".into(),
        })?;
        let repo_prefix = CrabUrl::parse(&primary)?.repo_path;
        active_active_coordinator_health(&config, &repo_prefix).await
    }
    .await;

    match result {
        Ok(health) if health.healthy && health.linearizable => Some(health),
        Ok(health) => {
            status.coordinator_ready = false;
            status.writes_enabled = false;
            status.reason = Some(health.reason.clone().unwrap_or_else(|| {
                "coordinator data-plane health does not admit active-active writes".to_owned()
            }));
            Some(health)
        }
        Err(err) => {
            status.coordinator_ready = false;
            status.writes_enabled = false;
            status.reason = Some(err.to_string());
            None
        }
    }
}

async fn run_failover_fence(args: &FailoverFenceArgs) -> Result<()> {
    let payload = failover_operation_payload(
        ActiveActiveFailoverOperation::Fence,
        args.apply,
        args.reason.as_deref(),
        false,
    )
    .await?;
    render_failover_operation(&payload, OutputMode::from_flags(args.json, false));
    Ok(())
}

async fn run_failover_resume(args: &FailoverResumeArgs) -> Result<()> {
    let payload = failover_operation_payload(
        ActiveActiveFailoverOperation::Resume,
        args.apply,
        None,
        args.repair_verified,
    )
    .await?;
    render_failover_operation(&payload, OutputMode::from_flags(args.json, false));
    Ok(())
}

pub async fn run_repair(args: &RepairArgs, cancel: &CancellationToken) -> Result<()> {
    validate_repair_args(args)?;
    if args.service_template.is_some() {
        let template = repair_service_template(args)?;
        if let Some(output) = args.output.as_deref() {
            write_text_bundle(output, &template)?;
        } else {
            print!("{template}");
        }
        return Ok(());
    }
    if args.watch {
        return run_repair_watch(args, cancel).await;
    }

    let payload = repair_payload(args).await?;
    render_repair(&payload, OutputMode::from_flags(args.json, args.jsonl));
    Ok(())
}

async fn run_repair_watch(args: &RepairArgs, cancel: &CancellationToken) -> Result<()> {
    let mode = OutputMode::from_flags(args.json, args.jsonl);
    let cwd = std::env::current_dir()?;
    let mut lease = acquire_repair_watch_lease(&cwd, args)?;
    let mut sample = 1_u64;
    let mut consecutive_errors = 0_u64;

    loop {
        if cancel.is_cancelled() {
            return Ok(());
        }
        lease.heartbeat(consecutive_errors, args.interval)?;
        let payload = match repair_payload(args).await {
            Ok(payload) => {
                consecutive_errors = 0;
                payload
            }
            Err(err) => {
                consecutive_errors = consecutive_errors.saturating_add(1);
                repair_error_payload(args.dry_run, err)
            }
        };
        let next_interval_seconds =
            repair_watch_next_interval_seconds(args.interval, consecutive_errors);
        let worker = lease.heartbeat(consecutive_errors, next_interval_seconds)?;
        render_repair_watch_snapshot(&payload, mode, sample, next_interval_seconds, &worker);
        if args.samples.is_some_and(|max| sample >= max) {
            return Ok(());
        }
        sample = sample.saturating_add(1);

        tokio::select! {
            () = cancel.cancelled() => return Ok(()),
            () = tokio::time::sleep(Duration::from_secs(next_interval_seconds)) => {}
        }
    }
}

fn acquire_repair_watch_lease(root: &Path, args: &RepairArgs) -> Result<RepairWatchLeaseGuard> {
    let path = repair_watch_lease_path(root);
    let worker_id = format!("repair-watch-{}-{}", std::process::id(), now_unix_ms());
    acquire_repair_watch_lease_at(&path, args, &worker_id, now_unix_ms())
}

fn acquire_repair_watch_lease_at(
    path: &Path,
    args: &RepairArgs,
    worker_id: &str,
    now_ms: u64,
) -> Result<RepairWatchLeaseGuard> {
    if let Some(existing) = read_repair_watch_lease(path)?
        && existing.expires_at_ms > now_ms
    {
        return Err(CrabError::Configuration {
            key: "replica.repair.watch".into(),
            origin: format!(
                "repair worker lease is held by {} (pid {}) until {} ms; stop that worker or wait for the lease to expire",
                existing.worker_id, existing.pid, existing.expires_at_ms
            ),
        });
    }

    let ttl_seconds = repair_watch_lease_ttl_seconds(args.interval);
    let state = RepairWatchWorkerState {
        schema_version: REPAIR_WATCH_LEASE_SCHEMA_VERSION,
        worker_id: worker_id.to_owned(),
        pid: std::process::id(),
        lease_path: path.display().to_string(),
        acquired_at_ms: now_ms,
        heartbeat_at_ms: now_ms,
        expires_at_ms: lease_expires_at_ms(now_ms, ttl_seconds),
        base_interval_seconds: args.interval,
        next_interval_seconds: args.interval,
        consecutive_errors: 0,
        dry_run: args.dry_run,
    };
    write_repair_watch_lease(path, &state)?;

    Ok(RepairWatchLeaseGuard {
        path: path.to_owned(),
        state,
        ttl_seconds,
    })
}

impl RepairWatchLeaseGuard {
    fn heartbeat(
        &mut self,
        consecutive_errors: u64,
        next_interval_seconds: u64,
    ) -> Result<RepairWatchWorkerState> {
        self.heartbeat_at(now_unix_ms(), consecutive_errors, next_interval_seconds)
    }

    fn heartbeat_at(
        &mut self,
        now_ms: u64,
        consecutive_errors: u64,
        next_interval_seconds: u64,
    ) -> Result<RepairWatchWorkerState> {
        if let Some(existing) = read_repair_watch_lease(&self.path)? {
            if existing.worker_id != self.state.worker_id {
                return Err(CrabError::Configuration {
                    key: "replica.repair.watch".into(),
                    origin: format!(
                        "repair worker lease was taken over by {} (pid {}); stopping this worker",
                        existing.worker_id, existing.pid
                    ),
                });
            }
        } else {
            return Err(CrabError::Configuration {
                key: "replica.repair.watch".into(),
                origin: "repair worker lease disappeared; stopping this worker".into(),
            });
        }

        self.state.heartbeat_at_ms = now_ms;
        self.state.expires_at_ms = lease_expires_at_ms(now_ms, self.ttl_seconds);
        self.state.next_interval_seconds = next_interval_seconds;
        self.state.consecutive_errors = consecutive_errors;
        write_repair_watch_lease(&self.path, &self.state)?;
        Ok(self.state.clone())
    }

    fn release(&self) -> Result<()> {
        let Some(existing) = read_repair_watch_lease(&self.path)? else {
            return Ok(());
        };
        if existing.worker_id == self.state.worker_id {
            std::fs::remove_file(&self.path)?;
        }
        Ok(())
    }
}

impl Drop for RepairWatchLeaseGuard {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

fn repair_watch_lease_path(root: &Path) -> PathBuf {
    let config_path = project_config_path(root);
    let base = config_path
        .parent()
        .map_or_else(|| root.to_path_buf(), Path::to_path_buf);
    base.join(".crab")
        .join("replication")
        .join("repair-watch-lease.json")
}

fn repair_watch_lease_ttl_seconds(interval_seconds: u64) -> u64 {
    interval_seconds
        .saturating_mul(3)
        .max(MIN_REPAIR_WATCH_LEASE_TTL_SECS)
}

fn repair_watch_next_interval_seconds(base_interval_seconds: u64, consecutive_errors: u64) -> u64 {
    let exponent = consecutive_errors.min(4) as u32;
    let multiplier = 1_u64 << exponent;
    base_interval_seconds
        .saturating_mul(multiplier)
        .clamp(1, MAX_REPAIR_WATCH_BACKOFF_SECS)
}

fn lease_expires_at_ms(now_ms: u64, ttl_seconds: u64) -> u64 {
    now_ms.saturating_add(ttl_seconds.saturating_mul(1_000))
}

fn read_repair_watch_lease(path: &Path) -> Result<Option<RepairWatchWorkerState>> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let state: RepairWatchWorkerState =
                serde_json::from_slice(&bytes).map_err(|err| CrabError::Configuration {
                    key: "replica.repair.watch".into(),
                    origin: format!(
                        "repair worker lease {} is not valid JSON: {err}",
                        path.display()
                    ),
                })?;
            if state.schema_version != REPAIR_WATCH_LEASE_SCHEMA_VERSION {
                return Err(CrabError::Configuration {
                    key: "replica.repair.watch".into(),
                    origin: format!(
                        "repair worker lease {} has unsupported schema version {}",
                        path.display(),
                        state.schema_version
                    ),
                });
            }
            Ok(Some(state))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(CrabError::Io(err)),
    }
}

fn write_repair_watch_lease(path: &Path, state: &RepairWatchWorkerState) -> Result<()> {
    let parent = path.parent().ok_or_else(|| CrabError::Configuration {
        key: "replica.repair.watch".into(),
        origin: format!("repair worker lease path {} has no parent", path.display()),
    })?;
    std::fs::create_dir_all(parent)?;
    let temp = NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(temp.as_file(), state)
        .map_err(|err| CrabError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, err)))?;
    temp.persist(path).map_err(|err| CrabError::Io(err.error))?;
    Ok(())
}

fn write_diagnostics_bundle(path: &Path, payload: &DiagnosticsPayload) -> Result<()> {
    write_json_bundle(path, payload)
}

fn write_certification_bundle(path: &Path, payload: &CertificationPayload) -> Result<()> {
    write_json_bundle(path, payload)
}

fn write_text_bundle(path: &Path, body: &str) -> Result<()> {
    use std::io::Write as _;

    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let mut temp = NamedTempFile::new_in(parent)?;
    temp.write_all(body.as_bytes())?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|err| CrabError::Io(err.error))?;
    Ok(())
}

fn write_json_bundle<T: Serialize>(path: &Path, payload: &T) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let temp = NamedTempFile::new_in(parent)?;
    serde_json::to_writer_pretty(temp.as_file(), payload)
        .map_err(|err| CrabError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, err)))?;
    temp.as_file().sync_all()?;
    temp.persist(path).map_err(|err| CrabError::Io(err.error))?;
    Ok(())
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

async fn repair_payload(args: &RepairArgs) -> Result<RepairPayload> {
    let payload = if args.from_coordinator {
        let cwd = std::env::current_dir()?;
        let (primary, config) = resolved_replication_context(&cwd)?;
        let replication = config
            .replication
            .as_ref()
            .ok_or_else(|| CrabError::Configuration {
                key: "replication".into(),
                origin: "coordinator-backed repair requires active-active replication config"
                    .into(),
            })?;
        if !replication.is_active_active() {
            return Err(CrabError::Configuration {
                key: "replication.mode".into(),
                origin: "coordinator-backed repair is only valid in active-active mode".into(),
            });
        }
        validate_active_active_config(replication)?;
        let primary = primary.ok_or_else(|| CrabError::Configuration {
            key: "remote.url".into(),
            origin: "coordinator-backed repair requires a configured primary remote".into(),
        })?;
        let repo_prefix = CrabUrl::parse(&primary)?.repo_path;
        if args.dry_run {
            match active_active_repair_plan_from_coordinator(&config, &repo_prefix).await {
                Ok(plan) => repair_payload_from_coordinator_plan(true, Some(plan), None),
                Err(err) => repair_payload_from_coordinator_plan(true, None, Some(err.to_string())),
            }
        } else {
            let plan = apply_active_active_repair_from_coordinator(&config, &repo_prefix).await?;
            repair_payload_from_coordinator_plan(false, Some(plan), None)
        }
    } else {
        RepairPayload {
            from_coordinator: false,
            dry_run: args.dry_run,
            planned_actions: vec!["run replica doctor --deep before repair".to_owned()],
            coordinator_plan: None,
            blocked_reason: None,
        }
    };
    Ok(payload)
}

async fn failover_operation_payload(
    operation: ActiveActiveFailoverOperation,
    apply: bool,
    reason: Option<&str>,
    repair_verified: bool,
) -> Result<FailoverOperationPayload> {
    validate_failover_apply_confirmation(operation, apply, repair_verified)?;
    let cwd = std::env::current_dir()?;
    let (primary, config) = resolved_replication_context(&cwd)?;
    let replication = config
        .replication
        .as_ref()
        .ok_or_else(|| CrabError::Configuration {
            key: "replication".into(),
            origin: "active-active failover requires active-active replication config".into(),
        })?;
    if !replication.is_active_active() {
        return Err(CrabError::Configuration {
            key: "replication.mode".into(),
            origin: "active-active failover is only valid in active-active mode".into(),
        });
    }
    validate_active_active_config(replication)?;
    let coordinator = replication
        .coordinator
        .as_ref()
        .ok_or_else(|| CrabError::Configuration {
            key: "replication.coordinator".into(),
            origin: "active-active failover requires a managed coordinator".into(),
        })?;
    let primary = primary.ok_or_else(|| CrabError::Configuration {
        key: "remote.url".into(),
        origin: "active-active failover requires a configured primary remote".into(),
    })?;
    let repo_prefix = CrabUrl::parse(&primary)?.repo_path;
    let planned_actions = failover_planned_actions(operation, &coordinator.url, &repo_prefix);
    let outcome = if apply {
        Some(match operation {
            ActiveActiveFailoverOperation::Fence => {
                fence_active_active_writes(&config, &repo_prefix, reason).await?
            }
            ActiveActiveFailoverOperation::Resume => {
                resume_active_active_writes(
                    &config,
                    &repo_prefix,
                    ActiveActiveResumeProof::verified_after_repair(),
                )
                .await?
            }
        })
    } else {
        None
    };

    Ok(FailoverOperationPayload {
        operation,
        applied: apply,
        repair_verified,
        coordinator_url: coordinator.url.clone(),
        repo_prefix,
        automation_policy: failover_automation_policy(),
        planned_actions,
        outcome,
    })
}

fn validate_failover_apply_confirmation(
    operation: ActiveActiveFailoverOperation,
    apply: bool,
    repair_verified: bool,
) -> Result<()> {
    if operation == ActiveActiveFailoverOperation::Resume && apply && !repair_verified {
        return Err(CrabError::Configuration {
            key: "replica.failover.resume.repair_verified".into(),
            origin: "failover resume --apply requires --repair-verified after coordinator-backed repair and external provider failover checks complete".into(),
        });
    }
    Ok(())
}

fn failover_automation_policy() -> FailoverAutomationPolicy {
    FailoverAutomationPolicy {
        automatic_write_failover_supported: false,
        orchestration: FAILOVER_ORCHESTRATION.to_owned(),
        split_brain_policy: FAILOVER_SPLIT_BRAIN_POLICY.to_owned(),
        required_operator_steps: vec![
            "inspect failover status and coordinator health".to_owned(),
            "fence writes before ambiguous provider or coordinator failover".to_owned(),
            "repair regional manifests from coordinator truth".to_owned(),
            "resume writes only after external provider failover and repair proof".to_owned(),
        ],
        adr: FAILOVER_ADR.to_owned(),
    }
}

fn failover_automation_decision(
    snapshot: &FailoverStatusSnapshot,
    replication: Option<&ReplicationConfig>,
    unhealthy_writers: &[String],
    repair_verified: bool,
) -> FailoverAutomationDecision {
    let unhealthy_writers = normalized_unhealthy_writers(unhealthy_writers);
    let mut required_evidence = vec![
        "coordinator control-plane status with verified drift checks".to_owned(),
        "coordinator data-plane health from the configured linearizable backend".to_owned(),
    ];
    if !unhealthy_writers.is_empty() {
        required_evidence.push("external writer-region health signal".to_owned());
    }
    if repair_verified {
        required_evidence.push("coordinator-backed repair and provider failover proof".to_owned());
    }

    if let Some(reason) = unhealthy_writer_config_error(replication, &unhealthy_writers) {
        return failover_decision(
            FailoverAutomationAction::Hold,
            reason,
            unhealthy_writers,
            repair_verified,
            Vec::new(),
            required_evidence,
        );
    }

    let Some(health) = snapshot.coordinator_health.as_ref() else {
        if !snapshot.active_active.coordinator_ready {
            let reason = snapshot.active_active.reason.clone().unwrap_or_else(|| {
                "coordinator is not ready for active-active failover".to_owned()
            });
            return failover_decision(
                FailoverAutomationAction::Hold,
                reason,
                unhealthy_writers,
                repair_verified,
                vec!["crab replica failover status --json".to_owned()],
                required_evidence,
            );
        }
        return failover_decision(
            FailoverAutomationAction::Hold,
            "coordinator data-plane health is unavailable; automation must fail closed".to_owned(),
            unhealthy_writers,
            repair_verified,
            vec!["crab replica failover status --json".to_owned()],
            required_evidence,
        );
    };

    if !health.linearizable {
        return failover_decision(
            FailoverAutomationAction::Hold,
            "coordinator data-plane cannot prove linearizable writes; automation must fail closed"
                .to_owned(),
            unhealthy_writers,
            repair_verified,
            vec!["crab replica coordinator status --json".to_owned()],
            required_evidence,
        );
    }

    if !health.healthy {
        if repair_verified {
            return failover_decision(
                FailoverAutomationAction::Resume,
                "coordinator is fenced and repair proof was provided".to_owned(),
                unhealthy_writers,
                repair_verified,
                vec!["crab replica failover resume --repair-verified --apply".to_owned()],
                required_evidence,
            );
        }
        return failover_decision(
            FailoverAutomationAction::Repair,
            "coordinator is fenced; repair regional manifests before resuming writes".to_owned(),
            unhealthy_writers,
            repair_verified,
            vec![
                "crab replica repair --from-coordinator --dry-run --json".to_owned(),
                "crab replica repair --from-coordinator --watch --jsonl".to_owned(),
                "crab replica failover resume --repair-verified --apply".to_owned(),
            ],
            required_evidence,
        );
    }

    if !unhealthy_writers.is_empty() {
        let unhealthy_writer_list = unhealthy_writers.join(",");
        return failover_decision(
            FailoverAutomationAction::Fence,
            format!(
                "external health reported unhealthy writer region(s): {}",
                unhealthy_writers.join(", ")
            ),
            unhealthy_writers,
            repair_verified,
            vec![format!(
                "crab replica failover fence --apply --reason writer-unhealthy:{}",
                unhealthy_writer_list
            )],
            required_evidence,
        );
    }

    failover_decision(
        FailoverAutomationAction::Monitor,
        "coordinator is healthy and no unhealthy writer signal was provided".to_owned(),
        unhealthy_writers,
        repair_verified,
        vec!["crab replica failover status --json".to_owned()],
        required_evidence,
    )
}

fn failover_decision(
    action: FailoverAutomationAction,
    reason: String,
    unhealthy_writers: Vec<String>,
    repair_verified: bool,
    commands: Vec<String>,
    required_evidence: Vec<String>,
) -> FailoverAutomationDecision {
    FailoverAutomationDecision {
        action,
        automatic_apply_supported: matches!(
            action,
            FailoverAutomationAction::Fence
                | FailoverAutomationAction::Repair
                | FailoverAutomationAction::Resume
        ),
        reason,
        unhealthy_writers,
        repair_verified,
        commands,
        required_evidence,
    }
}

fn failover_automation_apply_blocker(
    decision: &FailoverAutomationDecision,
    apply: bool,
) -> Option<String> {
    if !apply || decision.automatic_apply_supported {
        return None;
    }
    Some(format!(
        "failover run cannot apply {}: {}",
        decision.action.as_str(),
        decision.reason
    ))
}

fn normalized_unhealthy_writers(unhealthy_writers: &[String]) -> Vec<String> {
    let mut writers = unhealthy_writers
        .iter()
        .map(|writer| writer.trim())
        .filter(|writer| !writer.is_empty())
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    writers.sort();
    writers.dedup();
    writers
}

fn unhealthy_writer_config_error(
    replication: Option<&ReplicationConfig>,
    unhealthy_writers: &[String],
) -> Option<String> {
    if unhealthy_writers.is_empty() {
        return None;
    }
    let Some(replication) = replication else {
        return Some(
            "unhealthy writer signal cannot be trusted without replication config".to_owned(),
        );
    };
    if !replication.is_active_active() {
        return Some("unhealthy writer signal is only valid in active-active mode".to_owned());
    }
    let enabled = replication
        .writers
        .iter()
        .filter(|writer| writer.enabled)
        .map(|writer| writer.name.as_str())
        .collect::<BTreeSet<_>>();
    if enabled.is_empty() {
        return Some(
            "unhealthy writer signal cannot be trusted without enabled writers".to_owned(),
        );
    }
    let unknown = unhealthy_writers
        .iter()
        .filter(|writer| !enabled.contains(writer.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    if unknown.is_empty() {
        return None;
    }
    Some(format!(
        "unhealthy writer signal references unknown or disabled writer(s): {}",
        unknown.join(", ")
    ))
}

fn failover_planned_actions(
    operation: ActiveActiveFailoverOperation,
    coordinator_url: &str,
    repo_prefix: &str,
) -> Vec<String> {
    match operation {
        ActiveActiveFailoverOperation::Fence => vec![
            format!("verify configured coordinator {coordinator_url} control-plane drift"),
            format!("increment coordinator epoch for repo {repo_prefix} and mark writes unhealthy"),
            "new active-active writes fail closed until resume is applied".to_owned(),
        ],
        ActiveActiveFailoverOperation::Resume => vec![
            format!("verify configured coordinator {coordinator_url} control-plane drift"),
            format!("keep fenced coordinator epoch for repo {repo_prefix} and mark writes healthy"),
            "confirm coordinator-backed repair and external provider failover checks with --repair-verified".to_owned(),
            "new active-active writes can resume after failover status is green".to_owned(),
        ],
    }
}

fn repair_error_payload(dry_run: bool, err: CrabError) -> RepairPayload {
    repair_payload_from_coordinator_plan(dry_run, None, Some(err.to_string()))
}

fn repair_service_template(args: &RepairArgs) -> Result<String> {
    let format = args
        .service_template
        .ok_or_else(|| CrabError::Configuration {
            key: "replica.repair.service_template".into(),
            origin: "replica repair service template requires --service-template".into(),
        })?;
    let working_directory = match args.working_directory.as_ref() {
        Some(path) => path.clone(),
        None => std::env::current_dir()?,
    };
    let command = repair_service_command_args(args);
    let rendered = match format {
        RepairServiceTemplateArg::Systemd => {
            render_repair_systemd_template(args, &working_directory, &command)
        }
        RepairServiceTemplateArg::Launchd => {
            render_repair_launchd_template(args, &working_directory, &command)
        }
        RepairServiceTemplateArg::Kubernetes => {
            render_repair_kubernetes_template(args, &working_directory, &command)
        }
    };
    Ok(rendered)
}

fn repair_service_command_args(args: &RepairArgs) -> Vec<String> {
    let mut command = vec![
        "crab".to_owned(),
        "replica".to_owned(),
        "repair".to_owned(),
        "--from-coordinator".to_owned(),
        "--watch".to_owned(),
        "--jsonl".to_owned(),
        "--interval".to_owned(),
        args.interval.to_string(),
    ];
    if args.dry_run {
        command.push("--dry-run".to_owned());
    }
    command
}

fn render_repair_systemd_template(
    args: &RepairArgs,
    working_directory: &Path,
    command: &[String],
) -> String {
    let restart_seconds = args.interval.clamp(1, 60);
    format!(
        "[Unit]\n\
         Description=Crab active-active replica repair worker ({service})\n\
         Wants=network-online.target\n\
         After=network-online.target\n\n\
         [Service]\n\
         Type=simple\n\
         WorkingDirectory={working_directory}\n\
         ExecStart=/usr/bin/env {command}\n\
         Restart=always\n\
         RestartSec={restart_seconds}\n\
         KillSignal=SIGINT\n\
         Environment=RUST_LOG=info\n\n\
         [Install]\n\
         WantedBy=multi-user.target\n",
        service = args.service_name,
        working_directory = systemd_escape_value(working_directory.display().to_string().as_str()),
        command = command
            .iter()
            .map(|arg| shell_quote(arg))
            .collect::<Vec<_>>()
            .join(" "),
    )
}

fn render_repair_launchd_template(
    args: &RepairArgs,
    working_directory: &Path,
    command: &[String],
) -> String {
    let program_arguments = command
        .iter()
        .map(|arg| format!("    <string>{}</string>", xml_escape(arg)))
        .collect::<Vec<_>>()
        .join("\n");
    let log_prefix = working_directory
        .join(".crab")
        .join("replication")
        .join("repair-watch");
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n\
         <dict>\n\
         <key>Label</key><string>{service}</string>\n\
         <key>WorkingDirectory</key><string>{working_directory}</string>\n\
         <key>ProgramArguments</key>\n\
         <array>\n{program_arguments}\n\
         </array>\n\
         <key>RunAtLoad</key><true/>\n\
         <key>KeepAlive</key><true/>\n\
         <key>StandardOutPath</key><string>{stdout_path}.out.log</string>\n\
         <key>StandardErrorPath</key><string>{stdout_path}.err.log</string>\n\
         </dict>\n\
         </plist>\n",
        service = xml_escape(&args.service_name),
        working_directory = xml_escape(working_directory.display().to_string().as_str()),
        stdout_path = xml_escape(log_prefix.display().to_string().as_str()),
    )
}

fn render_repair_kubernetes_template(
    args: &RepairArgs,
    working_directory: &Path,
    command: &[String],
) -> String {
    let args_yaml = command
        .iter()
        .skip(1)
        .map(|arg| format!("        - {}", yaml_quote(arg)))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "apiVersion: apps/v1\n\
         kind: Deployment\n\
         metadata:\n\
           name: {service}\n\
           labels:\n\
             app.kubernetes.io/name: {service}\n\
             app.kubernetes.io/component: crab-replica-repair\n\
         spec:\n\
           replicas: 1\n\
           selector:\n\
             matchLabels:\n\
               app.kubernetes.io/name: {service}\n\
           template:\n\
             metadata:\n\
               labels:\n\
                 app.kubernetes.io/name: {service}\n\
                 app.kubernetes.io/component: crab-replica-repair\n\
             spec:\n\
               restartPolicy: Always\n\
               containers:\n\
                 - name: repair-worker\n\
                   image: {image}\n\
                   imagePullPolicy: IfNotPresent\n\
                   workingDir: {working_directory}\n\
                   command:\n\
                     - \"crab\"\n\
                   args:\n{args_yaml}\n",
        service = kubernetes_name(&args.service_name),
        image = yaml_quote(&args.container_image),
        working_directory = yaml_quote(working_directory.display().to_string().as_str()),
    )
}

fn systemd_escape_value(value: &str) -> String {
    value.replace('%', "%%")
}

fn shell_quote(value: &str) -> String {
    if value
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-' | b':' | b'='))
    {
        return value.to_owned();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn yaml_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

fn kubernetes_name(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut last_was_dash = false;
    for c in value.chars() {
        let normalized = if c.is_ascii_alphanumeric() {
            c.to_ascii_lowercase()
        } else {
            '-'
        };
        if normalized == '-' {
            if !last_was_dash && !out.is_empty() {
                out.push('-');
            }
            last_was_dash = true;
        } else {
            out.push(normalized);
            last_was_dash = false;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "crab-replica-repair".to_owned()
    } else {
        out
    }
}

fn validate_repair_args(args: &RepairArgs) -> Result<()> {
    if args.interval == 0 {
        return Err(CrabError::Configuration {
            key: "replica.repair.interval".into(),
            origin: "replica repair --interval must be at least 1 second".into(),
        });
    }
    if args.samples == Some(0) {
        return Err(CrabError::Configuration {
            key: "replica.repair.samples".into(),
            origin: "replica repair --samples must be at least 1".into(),
        });
    }
    if args.samples.is_some() && !args.watch {
        return Err(CrabError::Configuration {
            key: "replica.repair.samples".into(),
            origin: "replica repair --samples is only valid with --watch".into(),
        });
    }
    if args.watch && args.json {
        return Err(CrabError::Configuration {
            key: "replica.repair.watch".into(),
            origin: "replica repair --watch cannot be combined with --json; use --jsonl".into(),
        });
    }
    if args.watch && !args.from_coordinator {
        return Err(CrabError::Configuration {
            key: "replica.repair.watch".into(),
            origin: "replica repair --watch requires --from-coordinator".into(),
        });
    }
    if args.service_template.is_some() && !args.from_coordinator {
        return Err(CrabError::Configuration {
            key: "replica.repair.service_template".into(),
            origin: "replica repair --service-template requires --from-coordinator".into(),
        });
    }
    if args.service_template.is_some() && (args.watch || args.json || args.jsonl) {
        return Err(CrabError::Configuration {
            key: "replica.repair.service_template".into(),
            origin: "replica repair --service-template renders a supervisor manifest; omit --watch/--json/--jsonl".into(),
        });
    }
    if args.service_template.is_none()
        && (args.output.is_some()
            || args.working_directory.is_some()
            || args.service_name != "crab-replica-repair"
            || args.container_image != "ghcr.io/crab-build/crab:latest")
    {
        return Err(CrabError::Configuration {
            key: "replica.repair.service_template".into(),
            origin: "repair worker deployment options require --service-template".into(),
        });
    }
    Ok(())
}

pub async fn run_promote(args: &PromoteArgs) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let path = project_config_path(&cwd);
    let mut config = ProjectConfig::load(&path)?;
    let replication = config
        .replication
        .as_ref()
        .ok_or_else(|| CrabError::Configuration {
            key: "replication".into(),
            origin: "replication is not configured".into(),
        })?;
    if replication.is_active_active() {
        return Err(CrabError::Configuration {
            key: "replication.mode".into(),
            origin: "manual promote is only valid in read-replica mode".into(),
        });
    }
    let replica = replication
        .replicas
        .iter()
        .find(|replica| replica.name == args.name)
        .cloned()
        .ok_or_else(|| CrabError::Configuration {
            key: "replication.replicas".into(),
            origin: format!("replica {} is not configured", args.name),
        })?;
    let old_primary = replication
        .primary
        .clone()
        .unwrap_or_else(|| config.remote.url.clone());
    let new_primary = replica.url.clone();
    let read_enabled = replica.read;
    let control_plane = control_plane_statuses(Some(&old_primary), config.replication.as_ref())
        .await
        .into_iter()
        .find(|status| status.replica_name == args.name);
    let new_primary_is_crab_url = new_primary.starts_with("crab://");
    let plan_checks = promote_plan_checks(
        &replica,
        &old_primary,
        new_primary_is_crab_url,
        args.force,
        control_plane.as_ref(),
    );
    let planned_actions = promote_planned_actions(
        &replica.name,
        new_primary_is_crab_url,
        read_enabled,
        args.force,
    );
    let plan_ready = !plan_checks.iter().any(|check| check.blocking);
    let payload = PromotePayload {
        name: args.name.clone(),
        old_primary,
        new_primary: new_primary.clone(),
        dry_run: args.dry_run,
        forced: args.force,
        read_enabled,
        new_primary_is_crab_url,
        provider: replica.provider,
        region: replica.region.clone(),
        plan_ready,
        plan_checks,
        planned_actions,
        control_plane,
    };
    if !args.dry_run {
        if !new_primary.starts_with("crab://") {
            return Err(CrabError::Configuration {
                key: "replication.replicas.url".into(),
                origin: "replica promotion requires a crab:// replica URL so writes remain valid"
                    .into(),
            });
        }
        ensure_replica_promotable(&replica, args.force)?;
        config.remote.url = new_primary.clone();
        if let Some(replication) = config.replication.as_mut() {
            replication.primary = Some(new_primary);
        }
        ProjectConfig::write(&path, &config)?;
        let audit_root = path.parent().unwrap_or(cwd.as_path());
        if let Err(err) = record_replica_promote_audit(audit_root, &payload) {
            warn!(%err, "failed to append replica promotion audit event");
        }
    }
    render_promote(&payload, OutputMode::from_flags(args.json, false));
    Ok(())
}

fn record_replica_promote_audit(repo_root: &Path, payload: &PromotePayload) -> Result<()> {
    let event = AuditEvent::new(NewAuditEvent {
        operation: "replica.promote".to_owned(),
        outcome: AuditOutcome::Success,
        actor: None,
        repository: Some(payload.new_primary.clone()),
        details: serde_json::json!({
            "name": payload.name,
            "old_primary": payload.old_primary,
            "new_primary": payload.new_primary,
            "forced": payload.forced,
            "read_enabled": payload.read_enabled,
            "provider": payload.provider,
            "region": payload.region,
            "plan_ready": payload.plan_ready,
            "plan_checks": payload.plan_checks,
        }),
    });
    append_event(&repo_root.join(default_log_path()), &event)
}

pub async fn run_set_primary(args: &SetPrimaryArgs) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let path = project_config_path(&cwd);
    let mut config = ProjectConfig::load(&path)?;
    let replication = config
        .replication
        .as_ref()
        .ok_or_else(|| CrabError::Configuration {
            key: "replication".into(),
            origin: "replication is not configured".into(),
        })?;
    if replication.is_active_active() {
        return Err(CrabError::Configuration {
            key: "replication.mode".into(),
            origin: "set-primary is only valid in read-replica mode".into(),
        });
    }

    let old_primary = replication
        .primary
        .clone()
        .unwrap_or_else(|| config.remote.url.clone());
    let new_primary = args.url.clone();
    let new_primary_is_crab_url = new_primary.starts_with("crab://");
    let target_replica = replication
        .replicas
        .iter()
        .find(|replica| replica.url == new_primary)
        .cloned();
    let control_plane = match target_replica.as_ref() {
        Some(replica) => control_plane_statuses(Some(&old_primary), config.replication.as_ref())
            .await
            .into_iter()
            .find(|status| status.replica_name == replica.name),
        None => None,
    };
    let plan_checks = set_primary_plan_checks(
        target_replica.as_ref(),
        &old_primary,
        new_primary_is_crab_url,
        args.force,
        control_plane.as_ref(),
    );
    let plan_ready = !plan_checks.iter().any(|check| check.blocking);
    let planned_actions = set_primary_planned_actions(
        &new_primary,
        target_replica.as_ref(),
        new_primary_is_crab_url,
        args.force,
    );
    let apply = args.apply && !args.dry_run;
    let mut payload = SetPrimaryPayload {
        old_primary,
        new_primary: new_primary.clone(),
        applied: false,
        forced: args.force,
        new_primary_is_crab_url,
        target_replica: target_replica.as_ref().map(|replica| replica.name.clone()),
        plan_ready,
        plan_checks,
        planned_actions,
        control_plane,
    };

    if apply {
        ensure_set_primary_applicable(
            target_replica.as_ref(),
            new_primary_is_crab_url,
            args.force,
            plan_ready,
        )?;
        write_primary_to_project_config(&path, &mut config, &new_primary)?;
        payload.applied = true;
    }

    render_set_primary(&payload, OutputMode::from_flags(args.json, false));
    Ok(())
}

pub async fn run_status(args: &StatusArgs, cancel: &CancellationToken) -> Result<()> {
    validate_status_args(args)?;
    if args.watch {
        return run_status_watch(args, cancel).await;
    }

    let mode = OutputMode::from_flags(args.json, args.jsonl);
    let payload = status_payload(args.deep, cancel).await?;
    if args.prometheus {
        print!("{}", prometheus_status(&payload));
    } else {
        render_status(&payload, mode);
    }
    Ok(())
}

async fn run_status_watch(args: &StatusArgs, cancel: &CancellationToken) -> Result<()> {
    let mode = OutputMode::from_flags(args.json, args.jsonl);
    let interval = Duration::from_secs(args.interval);
    let mut sample = 1_u64;
    let mut previous_health: Option<Vec<ReplicaHealth>> = None;

    loop {
        if cancel.is_cancelled() {
            return Ok(());
        }
        let payload = status_payload(args.deep, cancel).await?;
        render_status_watch_snapshot(
            &payload,
            mode,
            sample,
            args.interval,
            previous_health.as_deref(),
        );
        previous_health = Some(payload.health.clone());
        sample = sample.saturating_add(1);

        tokio::select! {
            () = cancel.cancelled() => return Ok(()),
            () = tokio::time::sleep(interval) => {}
        }
    }
}

fn validate_status_args(args: &StatusArgs) -> Result<()> {
    if args.interval == 0 {
        return Err(CrabError::Configuration {
            key: "replica.status.interval".into(),
            origin: "replica status --interval must be at least 1 second".into(),
        });
    }
    if args.watch && args.json {
        return Err(CrabError::Configuration {
            key: "replica.status.watch".into(),
            origin: "replica status --watch cannot be combined with --json; use --jsonl".into(),
        });
    }
    if args.watch && args.prometheus {
        return Err(CrabError::Configuration {
            key: "replica.status.watch".into(),
            origin: "replica status --watch cannot be combined with --prometheus".into(),
        });
    }
    Ok(())
}

async fn status_payload(deep: bool, cancel: &CancellationToken) -> Result<StatusPayload> {
    let cwd = std::env::current_dir()?;
    let (primary, config) = resolved_replication_context(&cwd)?;
    let statuses = if let Some(primary) = primary.as_ref() {
        let parsed = CrabUrl::parse(primary)?;
        let options = if deep {
            ReadinessCheckOptions::deep()
        } else {
            readiness_check_options_from_env()?
        };
        replica_statuses_with_options(&config, &parsed, "replica-status", cancel, options).await?
    } else {
        Vec::new()
    };
    let control_plane =
        control_plane_statuses(primary.as_deref(), config.replication.as_ref()).await;
    sync_readiness_cache_from_control_plane(
        primary.as_deref(),
        config.replication.as_ref(),
        &control_plane,
    )?;
    let health = replica_health(&statuses, &control_plane);
    let backfill = backfill_statuses(config.replication.as_ref(), &control_plane);
    let payload = StatusPayload {
        primary,
        replicas: statuses,
        health,
        backfill,
        control_plane,
    };
    Ok(payload)
}

pub async fn run_doctor(args: &DoctorArgs, cancel: &CancellationToken) -> Result<()> {
    let payload = doctor_payload(args.deep, args.fix_plan, cancel).await?;
    render_doctor(&payload, OutputMode::from_flags(args.json, false));
    Ok(())
}

async fn doctor_payload(
    deep: bool,
    include_fix_plan: bool,
    cancel: &CancellationToken,
) -> Result<DoctorPayload> {
    let cwd = std::env::current_dir()?;
    let (primary, config) = resolved_replication_context(&cwd)?;
    let options = if deep {
        ReadinessCheckOptions::deep()
    } else {
        readiness_check_options_from_env()?
    };
    let replicas = if let Some(primary) = primary.as_ref() {
        let parsed = CrabUrl::parse(primary)?;
        replica_statuses_with_options(&config, &parsed, "replica-doctor", cancel, options).await?
    } else {
        Vec::new()
    };
    let control_plane =
        control_plane_statuses(primary.as_deref(), config.replication.as_ref()).await;
    sync_readiness_cache_from_control_plane(
        primary.as_deref(),
        config.replication.as_ref(),
        &control_plane,
    )?;
    let health = replica_health(&replicas, &control_plane);
    let backfill = backfill_statuses(config.replication.as_ref(), &control_plane);
    let coordinator_probe = coordinator_control_plane_probe(config.replication.as_ref()).await;
    let coordinator = coordinator_probe.status;
    let mut active_active = active_active_status_with_coordinator_status(
        config.replication.as_ref(),
        coordinator.as_ref(),
    );
    apply_coordinator_probe_error_to_active_active_status(
        &mut active_active,
        coordinator_probe.error.as_deref(),
    );
    let coordinator_health = if active_active.writes_enabled {
        apply_data_plane_health_to_failover_status(&cwd, &mut active_active).await
    } else {
        None
    };
    let findings = doctor_findings(
        primary.as_deref(),
        config.replication.as_ref(),
        &replicas,
        &control_plane,
        coordinator.as_ref(),
        coordinator_probe.error.as_deref(),
        coordinator_health.as_ref(),
        &active_active,
        deep,
    );
    let fix_plan = if include_fix_plan {
        doctor_fix_plan(
            primary.as_deref(),
            config.replication.as_ref(),
            &control_plane,
            coordinator.as_ref(),
            &findings,
        )
    } else {
        Vec::new()
    };
    Ok(DoctorPayload {
        primary,
        deep,
        replicas,
        health,
        backfill,
        control_plane,
        coordinator,
        coordinator_health,
        active_active,
        findings,
        fix_plan,
    })
}

pub async fn run_diagnostics(args: &DiagnosticsArgs, cancel: &CancellationToken) -> Result<()> {
    let doctor = doctor_payload(args.deep, args.fix_plan, cancel).await?;
    let primary_for_publish = doctor.primary.clone();
    let mut payload = diagnostics_payload_from_doctor(doctor, args.fix_plan);
    if args.redact {
        redact_diagnostics_payload(&mut payload);
    }
    if args.publish {
        let publication =
            publish_diagnostics_bundle(&payload, primary_for_publish.as_deref(), cancel).await?;
        payload.published = Some(publication.redacted_payload());
    }
    if let Some(path) = args.output.as_ref() {
        write_diagnostics_bundle(path, &payload)?;
    }
    render_diagnostics(
        &payload,
        args.output.as_deref(),
        OutputMode::from_flags(args.json, false),
    );
    Ok(())
}

#[derive(Debug)]
struct DiagnosticsPublication {
    primary: String,
    object_key: String,
    redacted: bool,
}

impl DiagnosticsPublication {
    fn redacted_payload(&self) -> DiagnosticsPublicationPayload {
        DiagnosticsPublicationPayload {
            primary: if self.redacted {
                "<redacted>".to_owned()
            } else {
                self.primary.clone()
            },
            object_key: if self.redacted {
                "<redacted>".to_owned()
            } else {
                self.object_key.clone()
            },
            redacted: self.redacted,
        }
    }
}

async fn publish_diagnostics_bundle(
    payload: &DiagnosticsPayload,
    primary: Option<&str>,
    cancel: &CancellationToken,
) -> Result<DiagnosticsPublication> {
    if !payload.redacted {
        return Err(CrabError::Configuration {
            key: "replica.diagnostics.publish".into(),
            origin: "diagnostics publication requires --redact before writing operator evidence to the primary remote".into(),
        });
    }
    let Some(primary) = primary.filter(|primary| !primary.is_empty()) else {
        return Err(CrabError::Configuration {
            key: "remote.url".into(),
            origin: "diagnostics publication requires a configured primary remote".into(),
        });
    };

    let cwd = std::env::current_dir()?;
    let (_, config) = resolved_replication_context(&cwd)?;
    let parsed = CrabUrl::parse(primary)?;
    let selection = StoreResolver::new(&config, &parsed, cancel)
        .write_store("replica-diagnostics-publish")
        .await?;
    let object_key = diagnostics_publication_key(
        selection.router.repo_prefix(),
        payload.collected_at_ms,
        std::process::id(),
    );
    let body = serde_json::to_vec_pretty(payload)
        .map_err(|err| CrabError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, err)))?;
    selection
        .store
        .put(&ObjectPath::from(object_key.as_str()), Bytes::from(body))
        .await?;

    Ok(DiagnosticsPublication {
        primary: primary.to_owned(),
        object_key,
        redacted: true,
    })
}

fn diagnostics_publication_key(repo_prefix: &str, collected_at_ms: u64, process_id: u32) -> String {
    let repo_prefix = repo_prefix.trim_matches('/');
    let relative = format!("diagnostics/replica/{collected_at_ms}-{process_id}.json");
    if repo_prefix.is_empty() {
        relative
    } else {
        format!("{repo_prefix}/{relative}")
    }
}

pub async fn run_certify(args: &CertifyArgs, cancel: &CancellationToken) -> Result<()> {
    let doctor = doctor_payload(true, true, cancel).await?;
    let evidence = certification_evidence_payload(args)?;
    let mut payload = certification_payload_from_doctor(doctor, args.profile, evidence);
    if args.redact {
        redact_certification_payload(&mut payload);
    }
    if let Some(path) = args.output.as_ref() {
        write_certification_bundle(path, &payload)?;
    }
    render_certification(
        &payload,
        args.output.as_deref(),
        OutputMode::from_flags(args.json, false),
    );
    if payload.certified {
        return Ok(());
    }
    Err(CrabError::Configuration {
        key: "replica.certification".into(),
        origin: "enterprise certification failed; inspect certification gates and fix plan".into(),
    })
}

fn certification_evidence_payload(
    args: &CertifyArgs,
) -> Result<Option<CertificationEvidencePayload>> {
    let Some(dir) = args.evidence_dir.as_ref() else {
        return Ok(None);
    };
    let expected_run_id = args
        .expected_run_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if args.profile == CertificationProfileArg::Enterprise && expected_run_id.is_none() {
        return Err(CrabError::Configuration {
            key: "replica.certification.expected_run_id".into(),
            origin: "enterprise certification requires --expected-run-id so retained live evidence is bound to one workflow attempt".into(),
        });
    }
    let verified = evidence_verify_payload_with_expected_run_id(
        dir,
        true,
        evidence_profile_for_certification(args.profile),
        expected_run_id,
    )?;
    Ok(Some(CertificationEvidencePayload::from(verified)))
}

pub fn run_evidence(command: &EvidenceCommand) -> Result<()> {
    match command {
        EvidenceCommand::Verify(args) => run_evidence_verify(args),
    }
}

fn run_evidence_verify(args: &EvidenceVerifyArgs) -> Result<()> {
    let payload = evidence_verify_payload_with_expected_run_id(
        &args.dir,
        args.require_redacted,
        args.profile,
        args.expected_run_id.as_deref(),
    )?;
    render_evidence_verify(&payload, OutputMode::from_flags(args.json, false));
    if payload.verified {
        return Ok(());
    }
    Err(CrabError::Configuration {
        key: "replica.evidence".into(),
        origin: "replica evidence verification failed; inspect failed artifact details".into(),
    })
}

#[cfg(test)]
fn evidence_verify_payload(
    dir: &Path,
    require_redacted: bool,
    profile: EvidenceVerifyProfile,
) -> Result<EvidenceVerifyPayload> {
    evidence_verify_payload_with_expected_run_id(dir, require_redacted, profile, None)
}

fn evidence_verify_payload_with_expected_run_id(
    dir: &Path,
    require_redacted: bool,
    profile: EvidenceVerifyProfile,
    expected_run_id: Option<&str>,
) -> Result<EvidenceVerifyPayload> {
    if !dir.is_dir() {
        return Err(CrabError::Configuration {
            key: "replica.evidence.dir".into(),
            origin: format!("{} is not an evidence directory", dir.display()),
        });
    }

    let mut paths = Vec::new();
    collect_evidence_json_files(dir, &mut paths)?;
    paths.sort();

    let mut files = paths
        .iter()
        .map(|path| verify_evidence_file(dir, path, require_redacted))
        .collect::<Vec<_>>();
    files.sort_by(|left, right| {
        left.collected_at_ms
            .cmp(&right.collected_at_ms)
            .then_with(|| left.path.cmp(&right.path))
    });
    if files.is_empty() {
        files.push(EvidenceFileStatus {
            path: ".".to_owned(),
            state: EvidenceFileState::Failed,
            kind: None,
            harness: None,
            run_id: None,
            sequence: None,
            schema: None,
            version: None,
            label: None,
            provider: None,
            coordinator_provider: None,
            writer_region: None,
            reader_region: None,
            push_operation_id: None,
            repair_service_template: None,
            repair_template_blake3: None,
            repair_command_blake3: None,
            provider_log_artifact_ref: None,
            redacted: None,
            collected_at_ms: None,
            errors: vec!["no evidence JSON artifacts found".to_owned()],
        });
    }

    let summary = evidence_verify_summary(&files);
    let gates = evidence_verify_gates(&files, require_redacted, profile, expected_run_id);
    let verified = summary.files_seen > 0
        && summary.files_failed == 0
        && gates
            .iter()
            .all(|gate| gate.state == CertificationGateState::Passed);
    Ok(EvidenceVerifyPayload {
        directory: dir.display().to_string(),
        verified,
        require_redacted,
        profile,
        summary,
        gates,
        files,
    })
}

fn collect_evidence_json_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            collect_evidence_json_files(&path, out)?;
            continue;
        }
        if file_type.is_file() && path.extension().and_then(|ext| ext.to_str()) == Some("json") {
            out.push(path);
        }
    }
    Ok(())
}

fn verify_evidence_file(root: &Path, path: &Path, require_redacted: bool) -> EvidenceFileStatus {
    let mut status = EvidenceFileStatus {
        path: evidence_relative_path(root, path),
        state: EvidenceFileState::Failed,
        kind: None,
        harness: None,
        run_id: None,
        sequence: None,
        schema: None,
        version: None,
        label: None,
        provider: None,
        coordinator_provider: None,
        writer_region: None,
        reader_region: None,
        push_operation_id: None,
        repair_service_template: None,
        repair_template_blake3: None,
        repair_command_blake3: None,
        provider_log_artifact_ref: None,
        redacted: None,
        collected_at_ms: None,
        errors: Vec::new(),
    };

    let value = match std::fs::read_to_string(path)
        .map_err(|err| format!("failed to read evidence file: {err}"))
        .and_then(|text| {
            serde_json::from_str::<serde_json::Value>(&text)
                .map_err(|err| format!("invalid evidence JSON: {err}"))
        }) {
        Ok(value) => value,
        Err(err) => {
            status.errors.push(err);
            return status;
        }
    };

    status.schema = value
        .get("schema")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    status.harness = value
        .get("harness")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    status.run_id = value
        .get("run_id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    status.sequence = value.get("sequence").and_then(serde_json::Value::as_u64);
    status.version = value
        .get("version")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    status.label = value
        .get("label")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    status.provider = value
        .get("provider")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    status.coordinator_provider = value
        .get("coordinator_provider")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    status.redacted = value.get("redacted").and_then(serde_json::Value::as_bool);
    status.collected_at_ms = value
        .get("collected_at_ms")
        .and_then(serde_json::Value::as_u64);
    if status.redacted == Some(true) {
        collect_redacted_evidence_secret_findings(&value, "$", &mut status.errors);
    }

    match status.schema.as_deref() {
        Some("replica.live-control-plane.evidence") => {
            status.kind = Some(EvidenceKind::LiveControlPlane);
            match serde_json::from_value::<LiveControlPlaneEvidencePayload>(value) {
                Ok(payload) => {
                    status.provider_log_artifact_ref =
                        live_control_plane_provider_log_artifact_ref(&payload);
                    status
                        .errors
                        .extend(validate_live_control_plane_evidence(root, &payload));
                    if payload.redacted
                        && let Some(reference) = status.provider_log_artifact_ref.as_deref()
                    {
                        collect_redacted_local_artifact_secret_findings(
                            root,
                            reference,
                            &payload.label,
                            &mut status.errors,
                        );
                    }
                }
                Err(err) => status
                    .errors
                    .push(format!("invalid live control-plane evidence shape: {err}")),
            }
        }
        Some("replica.live-smoke.evidence") => {
            status.kind = Some(EvidenceKind::LiveSmoke);
            match serde_json::from_value::<LiveSmokeEvidencePayload>(value) {
                Ok(payload) => {
                    status.writer_region = live_smoke_writer_region(&payload);
                    status.reader_region = live_smoke_reader_region(&payload);
                    status.push_operation_id = live_smoke_push_operation_id(&payload);
                    status.repair_service_template = live_smoke_repair_service_template(&payload);
                    status.repair_template_blake3 = live_smoke_repair_template_blake3(&payload);
                    status.repair_command_blake3 = live_smoke_repair_command_blake3(&payload);
                    status
                        .errors
                        .extend(validate_live_smoke_evidence(root, &payload));
                    if payload.redacted
                        && let Some(reference) = live_smoke_artifact_ref(&payload)
                    {
                        collect_redacted_local_artifact_secret_findings(
                            root,
                            &reference,
                            &payload.label,
                            &mut status.errors,
                        );
                    }
                }
                Err(err) => status
                    .errors
                    .push(format!("invalid live smoke evidence shape: {err}")),
            }
        }
        Some(schema) => status
            .errors
            .push(format!("unsupported evidence schema {schema}")),
        None => status.errors.push("missing evidence schema".to_owned()),
    }

    if require_redacted && status.redacted != Some(true) {
        status
            .errors
            .push("evidence artifact is not redacted".to_owned());
    }

    if status.errors.is_empty() {
        status.state = EvidenceFileState::Verified;
    }
    status
}

fn live_control_plane_provider_log_artifact_ref(
    payload: &LiveControlPlaneEvidencePayload,
) -> Option<String> {
    if !matches!(
        payload.label.as_str(),
        STORAGE_PROVIDER_LOG_LABEL | COORDINATOR_PROVIDER_LOG_LABEL
    ) {
        return None;
    }
    value_at(&payload.result, &["data", "artifact_ref"])
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn live_smoke_writer_region(payload: &LiveSmokeEvidencePayload) -> Option<String> {
    if !payload.label.starts_with("push-") || payload.label.starts_with("push-rejected-") {
        return None;
    }
    payload
        .result
        .as_ref()
        .and_then(|result| value_at(result, &["data", "writer_region"]))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|region| !region.is_empty())
        .map(str::to_owned)
}

fn live_smoke_reader_region(payload: &LiveSmokeEvidencePayload) -> Option<String> {
    if !payload.label.starts_with("clone-") && !payload.label.starts_with("hydrate-") {
        return None;
    }
    payload
        .result
        .as_ref()
        .and_then(|result| value_at(result, &["data", "reader_region"]))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|region| !region.is_empty())
        .map(str::to_owned)
}

fn live_smoke_push_operation_id(payload: &LiveSmokeEvidencePayload) -> Option<String> {
    if !payload.label.starts_with("push-") || payload.label.starts_with("push-rejected-") {
        return None;
    }
    payload
        .result
        .as_ref()
        .and_then(|result| value_at(result, &["data", "operation_id"]))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|operation_id| !operation_id.is_empty())
        .map(str::to_owned)
}

fn live_smoke_repair_service_template(payload: &LiveSmokeEvidencePayload) -> Option<String> {
    if !matches!(
        payload.label.as_str(),
        "repair-service-template" | "repair-worker-deployment"
    ) {
        return None;
    }
    payload
        .result
        .as_ref()
        .and_then(|result| value_at(result, &["data", "service_template"]))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn live_smoke_repair_template_blake3(payload: &LiveSmokeEvidencePayload) -> Option<String> {
    if !matches!(
        payload.label.as_str(),
        "repair-service-template" | "repair-worker-deployment"
    ) {
        return None;
    }
    payload
        .result
        .as_ref()
        .and_then(|result| value_at(result, &["data", "template_blake3"]))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn live_smoke_repair_command_blake3(payload: &LiveSmokeEvidencePayload) -> Option<String> {
    if !matches!(
        payload.label.as_str(),
        "repair-service-template" | "repair-worker-deployment"
    ) {
        return None;
    }
    payload
        .result
        .as_ref()
        .and_then(|result| value_at(result, &["data", "command_blake3"]))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn live_smoke_artifact_ref(payload: &LiveSmokeEvidencePayload) -> Option<String> {
    if payload.label != "repair-worker-deployment" {
        return None;
    }
    payload
        .result
        .as_ref()
        .and_then(|result| value_at(result, &["data", "artifact_ref"]))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn validate_live_control_plane_evidence(
    root: &Path,
    payload: &LiveControlPlaneEvidencePayload,
) -> Vec<String> {
    let mut errors = evidence_common_errors(&payload.label, &payload.args);
    validate_collected_at_ms(&mut errors, payload.collected_at_ms, &payload.label);
    validate_optional_provenance(
        &mut errors,
        payload.harness.as_deref(),
        payload.run_id.as_deref(),
        payload.sequence,
        &payload.label,
    );
    if is_storage_control_plane_label(&payload.label) {
        validate_evidence_provider(
            &mut errors,
            payload.provider.as_deref(),
            &STORAGE_EVIDENCE_PROVIDERS,
            &payload.label,
            "storage provider",
        );
        validate_storage_provider_consistency(&mut errors, payload);
        if payload.label == STORAGE_PROVIDER_LOG_LABEL {
            validate_provider_log_evidence(
                &mut errors,
                &payload.result,
                payload.provider.as_deref(),
                "storage-control-plane",
                root,
                &payload.label,
            );
        }
    } else if is_coordinator_control_plane_label(&payload.label) {
        validate_evidence_provider(
            &mut errors,
            payload.provider.as_deref(),
            &COORDINATOR_EVIDENCE_PROVIDERS,
            &payload.label,
            "coordinator provider",
        );
        validate_coordinator_provider_consistency(&mut errors, payload);
        if payload.label == COORDINATOR_PROVIDER_LOG_LABEL {
            validate_provider_log_evidence(
                &mut errors,
                &payload.result,
                payload.provider.as_deref(),
                "coordinator-control-plane",
                root,
                &payload.label,
            );
        }
    }
    match payload.label.as_str() {
        "storage-status" | "storage-status-after-apply" => {
            validate_storage_control_plane_status(&mut errors, &payload.result, &payload.label);
        }
        "storage-apply" | "storage-remove" => {
            validate_storage_control_plane_mutation(&mut errors, &payload.result, &payload.label);
        }
        "storage-plan" | "storage-remove-plan" => {
            validate_non_empty_object(&mut errors, &payload.result, &payload.label);
        }
        "coordinator-plan" => {
            require_schema(
                &mut errors,
                &payload.result,
                "replica.coordinator",
                &payload.label,
            );
            require_string_path(
                &mut errors,
                &payload.result,
                &["data", "plan", "provider"],
                &payload.label,
            );
        }
        "coordinator-status" | "coordinator-status-after-apply" => {
            require_schema(
                &mut errors,
                &payload.result,
                "replica.coordinator.status",
                &payload.label,
            );
            validate_coordinator_status(&mut errors, &payload.result, &payload.label);
        }
        "coordinator-apply" => {
            validate_coordinator_mutation(
                &mut errors,
                &payload.result,
                "replica.coordinator",
                &payload.label,
            );
        }
        "coordinator-remove" => {
            validate_coordinator_mutation(
                &mut errors,
                &payload.result,
                "replica.coordinator.remove",
                &payload.label,
            );
        }
        _ => {}
    }
    errors
}

fn validate_provider_log_evidence(
    errors: &mut Vec<String>,
    result: &serde_json::Value,
    provider: Option<&str>,
    scope: &str,
    root: &Path,
    label: &str,
) {
    require_schema(errors, result, "replica.live-provider-log", label);
    require_artifact_ref_path(errors, result, &["data", "artifact_ref"], root, label);
    require_string_value(errors, result, &["data", "scope"], scope, label);
    if let Some(provider) = provider {
        require_string_value(errors, result, &["data", "provider"], provider, label);
    }
}

fn validate_live_smoke_evidence(root: &Path, payload: &LiveSmokeEvidencePayload) -> Vec<String> {
    let mut errors = evidence_common_errors(&payload.label, &payload.args);
    validate_collected_at_ms(&mut errors, payload.collected_at_ms, &payload.label);
    validate_optional_provenance(
        &mut errors,
        payload.harness.as_deref(),
        payload.run_id.as_deref(),
        payload.sequence,
        &payload.label,
    );
    if payload.cwd.trim().is_empty() {
        errors.push("evidence cwd is empty".to_owned());
    }
    if is_provider_hydrate_label(&payload.label) {
        validate_evidence_provider(
            &mut errors,
            payload.provider.as_deref(),
            &STORAGE_EVIDENCE_PROVIDERS,
            &payload.label,
            "hydrate provider",
        );
        validate_provider_hydrate_consistency(&mut errors, payload);
    }
    if is_active_active_smoke_label(&payload.label) {
        validate_evidence_provider(
            &mut errors,
            payload.coordinator_provider.as_deref(),
            &COORDINATOR_EVIDENCE_PROVIDERS,
            &payload.label,
            "coordinator provider",
        );
    }
    if is_production_load_label(&payload.label) {
        validate_evidence_provider(
            &mut errors,
            payload.coordinator_provider.as_deref(),
            &COORDINATOR_EVIDENCE_PROVIDERS,
            &payload.label,
            "coordinator provider",
        );
    }

    if payload.label.starts_with("push-rejected-") {
        validate_rejection_evidence(&mut errors, payload);
        return errors;
    }

    let Some(result) = payload.result.as_ref() else {
        if known_smoke_success_label(&payload.label)
            || is_production_load_label(&payload.label)
            || payload.label.starts_with("push-")
        {
            errors.push(format!(
                "{} evidence must include a successful command result",
                payload.label
            ));
        }
        return errors;
    };

    if payload.exit_code.is_some() {
        errors.push(format!(
            "{} success evidence must not include an exit code",
            payload.label
        ));
    }
    validate_active_active_coordinator_consistency(&mut errors, payload, result);

    match payload.label.as_str() {
        "mode-active-active" => validate_mode_active_active(&mut errors, result, &payload.label),
        "initial-failover-status" | "writes-enabled" => {
            validate_failover_status(&mut errors, result, true, &payload.label);
        }
        "writes-fenced" => validate_failover_status(&mut errors, result, false, &payload.label),
        "failover-fence" => {
            validate_failover_operation_or_run(
                &mut errors,
                &payload.args,
                result,
                "fence",
                false,
                &payload.label,
            );
        }
        "failover-resume" => {
            validate_failover_operation_or_run(
                &mut errors,
                &payload.args,
                result,
                "resume",
                true,
                &payload.label,
            );
        }
        "repair-service-template" => {
            validate_repair_service_template(&mut errors, result, &payload.label);
        }
        "repair-worker-deployment" => {
            validate_repair_worker_deployment(&mut errors, result, &payload.label, root);
        }
        "repair-snapshot" => validate_repair_snapshot_evidence(&mut errors, payload, result),
        "active-active-certification" => {
            validate_active_active_certification(&mut errors, result, &payload.label);
        }
        PRODUCTION_LOAD_LABEL => {
            validate_production_load_evidence(&mut errors, result, &payload.label);
        }
        "provider-hydrate-init" => require_schema(&mut errors, result, "init", &payload.label),
        "provider-hydrate-push" => {
            validate_provider_hydrate_push(&mut errors, result, &payload.label);
        }
        "provider-hydrate-copy" => {
            validate_provider_hydrate_count(&mut errors, result, "copied_objects", &payload.label);
        }
        "provider-hydrate-read-enabled" => {
            validate_provider_hydrate_read_enabled(&mut errors, result, &payload.label);
        }
        "provider-hydrate-primary-xorbs-deleted" => {
            validate_provider_hydrate_count(&mut errors, result, "deleted_xorbs", &payload.label);
        }
        "provider-hydrate-selected-replica" => {
            validate_provider_hydrate_selected_replica(&mut errors, result, &payload.label);
        }
        label if label.starts_with("push-") => {
            validate_push_success(&mut errors, payload, result, label);
        }
        label if label.starts_with("clone-") => {
            validate_clone_success(&mut errors, payload, result, label);
        }
        label if label.starts_with("hydrate-") => {
            validate_hydrate_success(&mut errors, payload, result, label);
        }
        _ => {}
    }

    errors
}

fn validate_optional_provenance(
    errors: &mut Vec<String>,
    harness: Option<&str>,
    run_id: Option<&str>,
    sequence: Option<u64>,
    label: &str,
) {
    if harness.is_some_and(|value| value.trim().is_empty()) {
        errors.push(format!("{label} evidence harness is empty"));
    }
    if run_id.is_some_and(|value| value.trim().is_empty()) {
        errors.push(format!("{label} evidence run_id is empty"));
    }
    if sequence.is_some_and(|value| value == 0) {
        errors.push(format!(
            "{label} evidence sequence must be greater than zero"
        ));
    }
}

fn evidence_common_errors(label: &str, args: &[String]) -> Vec<String> {
    let mut errors = Vec::new();
    if label.trim().is_empty() {
        errors.push("evidence label is empty".to_owned());
    }
    if args.is_empty() {
        errors.push("evidence args are empty".to_owned());
    }
    errors
}

fn validate_collected_at_ms(errors: &mut Vec<String>, collected_at_ms: u64, label: &str) {
    if collected_at_ms == 0 {
        errors.push(format!(
            "{label} evidence collected_at_ms must be greater than zero"
        ));
    }
}

fn validate_evidence_provider(
    errors: &mut Vec<String>,
    provider: Option<&str>,
    allowed: &[&str],
    label: &str,
    field_name: &str,
) {
    match provider.map(str::trim).filter(|value| !value.is_empty()) {
        Some(provider) if allowed.contains(&provider) => {}
        Some(provider) => errors.push(format!("{label} has unsupported {field_name} {provider}")),
        None => errors.push(format!("{label} is missing {field_name}")),
    }
}

fn validate_storage_provider_consistency(
    errors: &mut Vec<String>,
    payload: &LiveControlPlaneEvidencePayload,
) {
    let Some(provider) = payload.provider.as_deref() else {
        return;
    };
    let result_paths: &[&[&str]] = match payload.label.as_str() {
        "storage-status" | "storage-status-after-apply" | "storage-apply" | "storage-remove" => {
            &[&["provider"]]
        }
        STORAGE_PROVIDER_LOG_LABEL => &[&["data", "provider"]],
        "storage-plan" | "storage-remove-plan" => &[&["setup", "provider"]],
        _ => &[],
    };
    require_matching_provider_paths(
        errors,
        &payload.result,
        result_paths,
        provider,
        &payload.label,
        "storage provider",
    );
}

fn validate_coordinator_provider_consistency(
    errors: &mut Vec<String>,
    payload: &LiveControlPlaneEvidencePayload,
) {
    let Some(provider) = payload.provider.as_deref() else {
        return;
    };
    let result_paths: &[&[&str]] = match payload.label.as_str() {
        "coordinator-plan" => &[&["data", "plan", "provider"]],
        "coordinator-status" | "coordinator-status-after-apply" => {
            &[&["data", "status", "provider"]]
        }
        COORDINATOR_PROVIDER_LOG_LABEL => &[&["data", "provider"]],
        "coordinator-apply" | "coordinator-remove" => &[
            &["data", "plan", "provider"],
            &["data", "apply_status", "provider"],
        ],
        _ => &[],
    };
    require_matching_provider_paths(
        errors,
        &payload.result,
        result_paths,
        provider,
        &payload.label,
        "coordinator provider",
    );
}

fn validate_provider_hydrate_consistency(
    errors: &mut Vec<String>,
    payload: &LiveSmokeEvidencePayload,
) {
    let (Some(provider), Some(result)) = (payload.provider.as_deref(), payload.result.as_ref())
    else {
        return;
    };
    let result_paths: &[&[&str]] = match payload.label.as_str() {
        "provider-hydrate-copy" | "provider-hydrate-primary-xorbs-deleted" => {
            &[&["data", "provider"]]
        }
        _ => &[],
    };
    require_matching_provider_paths(
        errors,
        result,
        result_paths,
        provider,
        &payload.label,
        "hydrate provider",
    );
}

fn validate_active_active_coordinator_consistency(
    errors: &mut Vec<String>,
    payload: &LiveSmokeEvidencePayload,
    result: &serde_json::Value,
) {
    let Some(provider) = payload.coordinator_provider.as_deref() else {
        return;
    };
    let result_paths: &[&[&str]] = match payload.label.as_str() {
        "initial-failover-status" | "writes-enabled" | "writes-fenced" => {
            &[&["data", "coordinator", "provider"]]
        }
        "failover-fence" | "failover-resume"
            if value_at(result, &["schema"]).and_then(serde_json::Value::as_str)
                == Some("replica.failover.run") =>
        {
            &[&["data", "operation", "outcome", "provider"]]
        }
        "failover-fence" | "failover-resume" => &[&["data", "outcome", "provider"]],
        "active-active-certification" => &[&["data", "coordinator", "provider"]],
        PRODUCTION_LOAD_LABEL => &[&["data", "coordinator_provider"]],
        _ => &[],
    };
    require_matching_provider_paths(
        errors,
        result,
        result_paths,
        provider,
        &payload.label,
        "coordinator provider",
    );
}

fn validate_production_load_evidence(
    errors: &mut Vec<String>,
    result: &serde_json::Value,
    label: &str,
) {
    require_schema(errors, result, "replica.production-load", label);
    require_string_value(errors, result, &["data", "profile"], "production", label);
    require_string_value(
        errors,
        result,
        &["data", "xorb_count_source"],
        "writer-store-delta",
        label,
    );
    for path in [
        &["data", "repository_bytes"][..],
        &["data", "file_count"],
        &["data", "xorb_count"],
        &["data", "xorb_count_after"],
        &["data", "refs_pushed"],
        &["data", "writer_regions"],
        &["data", "reader_regions"],
        &["data", "clone_count"],
        &["data", "hydrate_count"],
        &["data", "push_latency_ms"],
        &["data", "push_latency_budget_ms"],
        &["data", "read_latency_ms"],
        &["data", "read_latency_budget_ms"],
    ] {
        require_min_u64_path(errors, result, path, 1, label);
    }
    require_min_u64_path(errors, result, &["data", "xorb_count_before"], 0, label);
    require_min_u64_path(errors, result, &["data", "refs_pushed"], 2, label);
    require_min_u64_path(errors, result, &["data", "writer_regions"], 2, label);
    require_min_u64_path(errors, result, &["data", "reader_regions"], 2, label);
    require_min_u64_path(errors, result, &["data", "clone_count"], 2, label);
    require_min_u64_path(errors, result, &["data", "hydrate_count"], 2, label);
    validate_xorb_count_delta(errors, result, label);
    require_u64_lte_path_pair(
        errors,
        result,
        &["data", "push_latency_ms"],
        &["data", "push_latency_budget_ms"],
        label,
    );
    require_u64_lte_path_pair(
        errors,
        result,
        &["data", "read_latency_ms"],
        &["data", "read_latency_budget_ms"],
        label,
    );
}

fn validate_xorb_count_delta(errors: &mut Vec<String>, result: &serde_json::Value, label: &str) {
    let before =
        value_at(result, &["data", "xorb_count_before"]).and_then(serde_json::Value::as_u64);
    let after = value_at(result, &["data", "xorb_count_after"]).and_then(serde_json::Value::as_u64);
    let count = value_at(result, &["data", "xorb_count"]).and_then(serde_json::Value::as_u64);
    let (Some(before), Some(after), Some(count)) = (before, after, count) else {
        return;
    };
    if after <= before {
        errors.push(format!(
            "{label} expected data.xorb_count_after ({after}) to be greater than data.xorb_count_before ({before})"
        ));
        return;
    }
    let delta = after - before;
    if delta != count {
        errors.push(format!(
            "{label} expected data.xorb_count ({count}) to equal data.xorb_count_after - data.xorb_count_before ({delta})"
        ));
    }
}

fn require_matching_provider_paths(
    errors: &mut Vec<String>,
    result: &serde_json::Value,
    paths: &[&[&str]],
    expected: &str,
    label: &str,
    field_name: &str,
) {
    for path in paths {
        match value_at(result, path).and_then(serde_json::Value::as_str) {
            Some(actual) if actual == expected => {}
            Some(actual) => errors.push(format!(
                "{label} {field_name} {expected} does not match {}={actual}",
                path.join(".")
            )),
            None => errors.push(format!(
                "{label} is missing {} to confirm {field_name}",
                path.join(".")
            )),
        }
    }
}

fn is_storage_control_plane_label(label: &str) -> bool {
    matches!(
        label,
        "storage-plan"
            | "storage-status"
            | "storage-apply"
            | "storage-status-after-apply"
            | "storage-remove-plan"
            | "storage-remove"
            | STORAGE_PROVIDER_LOG_LABEL
    )
}

fn is_coordinator_control_plane_label(label: &str) -> bool {
    matches!(
        label,
        "coordinator-plan"
            | "coordinator-status"
            | "coordinator-apply"
            | "coordinator-status-after-apply"
            | "coordinator-remove"
            | COORDINATOR_PROVIDER_LOG_LABEL
    )
}

fn is_provider_hydrate_label(label: &str) -> bool {
    label.starts_with("provider-hydrate-")
}

fn is_provider_hydrate_milestone_label(label: &str) -> bool {
    PROVIDER_HYDRATE_SEQUENCE.contains(&label)
}

fn is_production_load_label(label: &str) -> bool {
    label == PRODUCTION_LOAD_LABEL
}

fn is_active_active_smoke_label(label: &str) -> bool {
    (known_smoke_success_label(label) && !is_provider_hydrate_label(label))
        || label.starts_with("push-rejected-")
}

fn known_smoke_success_label(label: &str) -> bool {
    matches!(
        label,
        "mode-active-active"
            | "initial-failover-status"
            | "writes-enabled"
            | "writes-fenced"
            | "failover-fence"
            | "failover-resume"
            | "repair-service-template"
            | "repair-worker-deployment"
            | "repair-snapshot"
            | "active-active-certification"
            | "provider-hydrate-init"
            | "provider-hydrate-push"
            | "provider-hydrate-copy"
            | "provider-hydrate-read-enabled"
            | "provider-hydrate-primary-xorbs-deleted"
            | "provider-hydrate-selected-replica"
    ) || label.starts_with("push-")
        || label.starts_with("clone-")
        || label.starts_with("hydrate-")
}

fn validate_storage_control_plane_status(
    errors: &mut Vec<String>,
    result: &serde_json::Value,
    label: &str,
) {
    require_bool_path(errors, result, &["backend_available"], true, label);
    require_bool_path(errors, result, &["checked_drift"], true, label);
    require_verified_checks(errors, value_at(result, &["checks"]), label);
}

fn validate_storage_control_plane_mutation(
    errors: &mut Vec<String>,
    result: &serde_json::Value,
    label: &str,
) {
    require_bool_path(errors, result, &["applied"], true, label);
    require_bool_path(errors, result, &["checked_drift"], true, label);
    require_non_empty_string_array_path(errors, result, &["actions"], label);
}

fn validate_coordinator_status(errors: &mut Vec<String>, result: &serde_json::Value, label: &str) {
    require_bool_path(
        errors,
        result,
        &["data", "status", "backend_available"],
        true,
        label,
    );
    require_bool_path(
        errors,
        result,
        &["data", "status", "checked_drift"],
        true,
        label,
    );
    require_verified_checks(
        errors,
        value_at(result, &["data", "status", "checks"]),
        label,
    );
}

fn validate_coordinator_mutation(
    errors: &mut Vec<String>,
    result: &serde_json::Value,
    schema: &str,
    label: &str,
) {
    require_schema(errors, result, schema, label);
    require_bool_path(errors, result, &["data", "applied"], true, label);
    require_bool_path(
        errors,
        result,
        &["data", "apply_status", "applied"],
        true,
        label,
    );
    require_bool_path(
        errors,
        result,
        &["data", "apply_status", "checked_drift"],
        true,
        label,
    );
    require_non_empty_string_array_path(
        errors,
        result,
        &["data", "apply_status", "actions"],
        label,
    );
}

fn validate_mode_active_active(errors: &mut Vec<String>, result: &serde_json::Value, label: &str) {
    require_schema(errors, result, "replica.mode", label);
    require_string_value(errors, result, &["data", "mode"], "active-active", label);
    require_bool_path(
        errors,
        result,
        &["data", "active_active", "coordinator_configured"],
        true,
        label,
    );
    require_min_u64_path(
        errors,
        result,
        &["data", "active_active", "enabled_writers"],
        2,
        label,
    );
}

fn validate_failover_status(
    errors: &mut Vec<String>,
    result: &serde_json::Value,
    writes_enabled: bool,
    label: &str,
) {
    require_schema(errors, result, "replica.failover", label);
    require_bool_path(
        errors,
        result,
        &["data", "active_active", "writes_enabled"],
        writes_enabled,
        label,
    );
    validate_failover_automation_policy(errors, result, label);
    validate_failover_automation_plan(errors, result, label);
}

fn validate_failover_operation(
    errors: &mut Vec<String>,
    args: &[String],
    result: &serde_json::Value,
    operation: &str,
    healthy: bool,
    label: &str,
) {
    require_schema(errors, result, "replica.failover.operation", label);
    require_string_value(errors, result, &["data", "operation"], operation, label);
    require_bool_path(errors, result, &["data", "applied"], true, label);
    require_bool_path(
        errors,
        result,
        &["data", "outcome", "healthy"],
        healthy,
        label,
    );
    if operation == "resume" {
        require_args_contain(errors, args, &["--repair-verified"], label);
        require_bool_path(errors, result, &["data", "repair_verified"], true, label);
    }
    validate_failover_automation_policy(errors, result, label);
}

fn validate_failover_operation_or_run(
    errors: &mut Vec<String>,
    args: &[String],
    result: &serde_json::Value,
    operation: &str,
    healthy: bool,
    label: &str,
) {
    match value_at(result, &["schema"]).and_then(serde_json::Value::as_str) {
        Some("replica.failover.operation") => {
            validate_failover_operation(errors, args, result, operation, healthy, label);
        }
        Some("replica.failover.run") => {
            validate_failover_run(errors, args, result, operation, healthy, label);
        }
        Some(schema) => errors.push(format!(
            "{label} expected schema replica.failover.operation or replica.failover.run, got {schema}"
        )),
        None => errors.push(format!("{label} is missing schema")),
    }
}

fn validate_failover_run(
    errors: &mut Vec<String>,
    args: &[String],
    result: &serde_json::Value,
    operation: &str,
    healthy: bool,
    label: &str,
) {
    require_args_contain(errors, args, &["failover", "run", "--apply"], label);
    require_bool_path(errors, result, &["data", "apply_requested"], true, label);
    require_bool_path(errors, result, &["data", "applied"], true, label);
    require_string_value(
        errors,
        result,
        &["data", "automation_plan", "action"],
        operation,
        label,
    );
    require_bool_path(
        errors,
        result,
        &["data", "automation_plan", "automatic_apply_supported"],
        true,
        label,
    );
    require_string_value(
        errors,
        result,
        &["data", "operation", "operation"],
        operation,
        label,
    );
    require_bool_path(
        errors,
        result,
        &["data", "operation", "applied"],
        true,
        label,
    );
    require_bool_path(
        errors,
        result,
        &["data", "operation", "outcome", "healthy"],
        healthy,
        label,
    );
    if operation == "resume" {
        require_args_contain(errors, args, &["--repair-verified"], label);
        require_bool_path(
            errors,
            result,
            &["data", "operation", "repair_verified"],
            true,
            label,
        );
    }
    validate_failover_automation_policy(errors, result, label);
}

fn validate_failover_automation_policy(
    errors: &mut Vec<String>,
    result: &serde_json::Value,
    label: &str,
) {
    require_bool_path(
        errors,
        result,
        &[
            "data",
            "automation_policy",
            "automatic_write_failover_supported",
        ],
        false,
        label,
    );
    require_string_value(
        errors,
        result,
        &["data", "automation_policy", "orchestration"],
        FAILOVER_ORCHESTRATION,
        label,
    );
    require_string_value(
        errors,
        result,
        &["data", "automation_policy", "split_brain_policy"],
        FAILOVER_SPLIT_BRAIN_POLICY,
        label,
    );
    require_string_value(
        errors,
        result,
        &["data", "automation_policy", "adr"],
        FAILOVER_ADR,
        label,
    );
}

fn validate_failover_automation_plan(
    errors: &mut Vec<String>,
    result: &serde_json::Value,
    label: &str,
) {
    require_string_path(
        errors,
        result,
        &["data", "automation_plan", "action"],
        label,
    );
    require_bool_any_path(
        errors,
        result,
        &["data", "automation_plan", "automatic_apply_supported"],
        label,
    );
    require_string_path(
        errors,
        result,
        &["data", "automation_plan", "reason"],
        label,
    );
    require_non_empty_string_array_path(
        errors,
        result,
        &["data", "automation_plan", "required_evidence"],
        label,
    );
}

fn validate_push_success(
    errors: &mut Vec<String>,
    payload: &LiveSmokeEvidencePayload,
    result: &serde_json::Value,
    label: &str,
) {
    require_args_contain(errors, &payload.args, &["push", "--json"], label);
    require_schema(errors, result, "push", label);
    require_min_u64_path(errors, result, &["data", "refs_pushed"], 1, label);
    require_string_path(errors, result, &["data", "operation_id"], label);
    require_min_u64_path(errors, result, &["data", "coordinator_epoch"], 1, label);
    require_string_path(errors, result, &["data", "writer_region"], label);
    require_string_value(
        errors,
        result,
        &["data", "commit_state"],
        "materialized",
        label,
    );
}

fn validate_provider_hydrate_push(
    errors: &mut Vec<String>,
    result: &serde_json::Value,
    label: &str,
) {
    require_schema(errors, result, "push", label);
    require_min_u64_path(errors, result, &["data", "refs_pushed"], 1, label);
}

fn validate_rejection_evidence(errors: &mut Vec<String>, payload: &LiveSmokeEvidencePayload) {
    require_args_contain(errors, &payload.args, &["push", "--json"], &payload.label);
    match payload.exit_code {
        Some(code) if code != 0 => {}
        Some(_) => errors.push(format!(
            "{} rejection evidence must have a non-zero exit code",
            payload.label
        )),
        None => errors.push(format!(
            "{} rejection evidence must include an exit code",
            payload.label
        )),
    }
    if payload.result.is_some() {
        errors.push(format!(
            "{} rejection evidence must not include a success result",
            payload.label
        ));
    }
    let text = rejection_evidence_text(payload);
    if payload.label.starts_with("push-rejected-fenced-") {
        require_rejection_fragment(errors, &payload.label, &text, "coordinator");
        if !text.contains("fail closed") && !text.contains("fenc") && !text.contains("unhealthy") {
            errors.push(format!(
                "{} rejection evidence must identify coordinator fencing or fail-closed write admission",
                payload.label
            ));
        }
    } else {
        require_rejection_fragment(errors, &payload.label, &text, "non-fast-forward");
    }
}

fn rejection_evidence_text(payload: &LiveSmokeEvidencePayload) -> String {
    format!(
        "{}\n{}",
        payload.stdout.as_deref().unwrap_or_default(),
        payload.stderr.as_deref().unwrap_or_default()
    )
    .to_ascii_lowercase()
}

fn require_rejection_fragment(errors: &mut Vec<String>, label: &str, text: &str, fragment: &str) {
    if !text.contains(fragment) {
        errors.push(format!(
            "{label} rejection evidence must contain {fragment}"
        ));
    }
}

fn require_args_contain(errors: &mut Vec<String>, args: &[String], expected: &[&str], label: &str) {
    let observed = args.iter().map(String::as_str).collect::<BTreeSet<_>>();
    for expected in expected {
        if !observed.contains(*expected) {
            errors.push(format!("{label} expected args to contain {expected}"));
        }
    }
}

fn validate_repair_service_template(
    errors: &mut Vec<String>,
    result: &serde_json::Value,
    label: &str,
) {
    require_schema(errors, result, "replica.repair.service-template", label);
    require_supported_repair_service_template(errors, result, label);
    require_bool_path(errors, result, &["data", "from_coordinator"], true, label);
    require_bool_path(errors, result, &["data", "watch"], true, label);
    require_bool_path(errors, result, &["data", "jsonl"], true, label);
    require_bool_path(errors, result, &["data", "rendered"], true, label);
    require_bool_path(errors, result, &["data", "non_mutating"], true, label);
    require_min_u64_path(errors, result, &["data", "interval_seconds"], 1, label);
    require_blake3_hex_path(errors, result, &["data", "template_blake3"], label);
    require_command_blake3_path(
        errors,
        result,
        &["data", "command"],
        &["data", "command_blake3"],
        label,
    );
    require_string_array_contains(
        errors,
        result,
        &["data", "command"],
        &[
            "crab",
            "replica",
            "repair",
            "--from-coordinator",
            "--watch",
            "--jsonl",
        ],
        label,
    );
}

fn validate_repair_worker_deployment(
    errors: &mut Vec<String>,
    result: &serde_json::Value,
    label: &str,
    evidence_root: &Path,
) {
    require_schema(errors, result, "replica.repair.worker-deployment", label);
    require_artifact_ref_path(
        errors,
        result,
        &["data", "artifact_ref"],
        evidence_root,
        label,
    );
    require_bool_path(
        errors,
        result,
        &["data", "deployment_verified"],
        true,
        label,
    );
    require_supported_repair_service_template(errors, result, label);
    require_blake3_hex_path(errors, result, &["data", "template_blake3"], label);
    require_command_blake3_path(
        errors,
        result,
        &["data", "command"],
        &["data", "command_blake3"],
        label,
    );
    require_string_array_contains(
        errors,
        result,
        &["data", "command"],
        &[
            "crab",
            "replica",
            "repair",
            "--from-coordinator",
            "--watch",
            "--jsonl",
        ],
        label,
    );
}

fn require_artifact_ref_path(
    errors: &mut Vec<String>,
    value: &serde_json::Value,
    path: &[&str],
    evidence_root: &Path,
    label: &str,
) {
    let Some(reference) = value_at(value, path)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|reference| !reference.is_empty())
    else {
        errors.push(format!("{label} is missing {}", path.join(".")));
        return;
    };

    if is_artifact_uri(reference) || artifact_ref_exists(evidence_root, reference) {
        return;
    }

    errors.push(format!(
        "{label} {} must be an artifact URI or existing relative artifact inside the evidence directory; use a secure artifact URI when the artifact is not local",
        path.join(".")
    ));
}

fn is_artifact_uri(reference: &str) -> bool {
    let Some((scheme, rest)) = reference.split_once("://") else {
        return false;
    };

    if !matches!(scheme, "https" | "s3" | "gs" | "az" | "azure") {
        return false;
    }

    is_complete_artifact_uri_rest(rest)
}

fn is_complete_artifact_uri_rest(rest: &str) -> bool {
    if rest.is_empty() || rest.chars().any(char::is_whitespace) {
        return false;
    }

    let Some((authority, path)) = rest.split_once('/') else {
        return false;
    };

    !authority.is_empty()
        && !authority.contains('@')
        && !path.is_empty()
        && !path.ends_with('/')
        && path
            .split('/')
            .all(|segment| !segment.is_empty() && segment != "." && segment != "..")
        && !path.contains('?')
        && !path.contains('#')
}

fn artifact_ref_exists(evidence_root: &Path, reference: &str) -> bool {
    artifact_ref_local_path(evidence_root, reference).is_some()
}

fn artifact_ref_local_path(evidence_root: &Path, reference: &str) -> Option<PathBuf> {
    let path = Path::new(reference);
    if !path.is_relative()
        || path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return None;
    }
    let root = evidence_root.canonicalize().ok()?;
    let candidate = evidence_root.join(path).canonicalize().ok()?;
    if candidate.is_file() && candidate.starts_with(root) {
        Some(candidate)
    } else {
        None
    }
}

fn collect_redacted_local_artifact_secret_findings(
    evidence_root: &Path,
    reference: &str,
    label: &str,
    errors: &mut Vec<String>,
) {
    let Some(path) = artifact_ref_local_path(evidence_root, reference) else {
        return;
    };
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(err) => {
            errors.push(format!(
                "{label} artifact_ref {reference} could not be scanned for secrets: {err}"
            ));
            return;
        }
    };
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) {
        collect_redacted_evidence_secret_findings(&value, &format!("artifact:{reference}"), errors);
        return;
    }
    for (index, line) in String::from_utf8_lossy(&bytes).lines().enumerate() {
        if errors.len() >= 16 {
            return;
        }
        if let Some(kind) = redacted_evidence_secret_string_kind(line) {
            errors.push(format!(
                "redacted evidence referenced local artifact {reference} contains possible secret at line {} ({kind})",
                index + 1
            ));
        }
    }
}

fn collect_redacted_evidence_secret_findings(
    value: &serde_json::Value,
    path: &str,
    errors: &mut Vec<String>,
) {
    if errors.len() >= 16 {
        return;
    }
    match value {
        serde_json::Value::Object(object) => {
            for (key, child) in object {
                let child_path = if path == "$" {
                    format!("$.{key}")
                } else {
                    format!("{path}.{key}")
                };
                if let Some(kind) = redacted_evidence_sensitive_key_kind(key)
                    && !redacted_evidence_value_is_safe(child)
                {
                    errors.push(format!(
                        "redacted evidence contains possible secret at {child_path} ({kind})"
                    ));
                }
                collect_redacted_evidence_secret_findings(child, &child_path, errors);
                if errors.len() >= 16 {
                    return;
                }
            }
        }
        serde_json::Value::Array(values) => {
            for (index, child) in values.iter().enumerate() {
                collect_redacted_evidence_secret_findings(
                    child,
                    &format!("{path}[{index}]"),
                    errors,
                );
                if errors.len() >= 16 {
                    return;
                }
            }
        }
        serde_json::Value::String(text) => {
            if let Some(kind) = redacted_evidence_secret_string_kind(text) {
                errors.push(format!(
                    "redacted evidence contains possible secret at {path} ({kind})"
                ));
            }
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {}
    }
}

fn redacted_evidence_sensitive_key_kind(key: &str) -> Option<&'static str> {
    let normalized = key
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>();
    let sensitive = [
        ("aws_access_key_id", "aws-access-key-id"),
        ("aws_secret_access_key", "aws-secret-access-key"),
        ("aws_session_token", "aws-session-token"),
        ("secret_access_key", "aws-secret-access-key"),
        ("x_amz_security_token", "aws-session-token"),
        (
            "google_application_credentials",
            "google-application-credentials",
        ),
        ("google_oauth_access_token", "google-oauth-token"),
        ("azure_client_secret", "azure-client-secret"),
        ("client_secret", "client-secret"),
        ("private_key", "private-key"),
        ("access_token", "access-token"),
        ("refresh_token", "refresh-token"),
        ("authorization", "authorization-header"),
    ];
    sensitive
        .iter()
        .find_map(|(needle, kind)| normalized.contains(needle).then_some(*kind))
}

fn redacted_evidence_value_is_safe(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Null => true,
        serde_json::Value::String(value) => {
            let value = value.trim();
            value.is_empty()
                || matches!(
                    value,
                    "<redacted>" | "redacted" | "***" | "REDACTED" | "<REDACTED>"
                )
        }
        serde_json::Value::Array(values) => values.iter().all(redacted_evidence_value_is_safe),
        serde_json::Value::Object(object) => object.values().all(redacted_evidence_value_is_safe),
        serde_json::Value::Bool(_) | serde_json::Value::Number(_) => false,
    }
}

fn redacted_evidence_secret_string_kind(value: &str) -> Option<&'static str> {
    let trimmed = value.trim();
    if trimmed.is_empty() || redacted_evidence_value_is_safe(&serde_json::json!(trimmed)) {
        return None;
    }
    if contains_aws_access_key_id(trimmed) {
        return Some("aws-access-key-id");
    }
    if trimmed.contains("-----BEGIN ") && trimmed.contains("PRIVATE KEY-----") {
        return Some("private-key");
    }
    let lower = trimmed.to_ascii_lowercase();
    if lower.contains("bearer ") {
        return Some("bearer-token");
    }
    if lower.contains("x-amz-signature=") || lower.contains("x-amz-credential=") {
        return Some("aws-signed-url");
    }
    if lower.contains("?sig=") || lower.contains("&sig=") {
        return Some("azure-sas-token");
    }
    if lower.contains("aws_secret_access_key=")
        || lower.contains("client_secret=")
        || lower.contains("access_token=")
        || lower.contains("refresh_token=")
    {
        return Some("credential-query-parameter");
    }
    if trimmed
        .split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_' && ch != '-')
        .any(|token| token.starts_with("AIza") && token.len() >= 20)
    {
        return Some("google-api-key");
    }
    None
}

fn contains_aws_access_key_id(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() < 20 {
        return false;
    }
    bytes.windows(20).any(|window| {
        matches!(&window[..4], b"AKIA" | b"ASIA")
            && window[4..]
                .iter()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
    })
}

fn require_supported_repair_service_template(
    errors: &mut Vec<String>,
    result: &serde_json::Value,
    label: &str,
) {
    match value_at(result, &["data", "service_template"]).and_then(serde_json::Value::as_str) {
        Some("systemd" | "launchd" | "kubernetes") => {}
        Some(value) => errors.push(format!(
            "{label} has unsupported data.service_template {value}"
        )),
        None => errors.push(format!("{label} is missing data.service_template")),
    }
}

fn require_blake3_hex_path(
    errors: &mut Vec<String>,
    result: &serde_json::Value,
    path: &[&str],
    label: &str,
) -> Option<String> {
    match value_at(result, path)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
    {
        Some(value) if is_blake3_hex(value) => Some(value.to_owned()),
        Some(value) => {
            errors.push(format!(
                "{label} expected {} to be a 64-character lowercase Blake3 hex digest, got {value}",
                path.join(".")
            ));
            None
        }
        None => {
            errors.push(format!("{label} is missing {}", path.join(".")));
            None
        }
    }
}

fn require_command_blake3_path(
    errors: &mut Vec<String>,
    result: &serde_json::Value,
    command_path: &[&str],
    digest_path: &[&str],
    label: &str,
) -> Option<String> {
    let Some(expected) = require_blake3_hex_path(errors, result, digest_path, label) else {
        return None;
    };
    let Some(command) = string_array_at(result, command_path) else {
        errors.push(format!("{label} is missing {}", command_path.join(".")));
        return Some(expected);
    };
    let actual = command_blake3(&command);
    if expected != actual {
        errors.push(format!(
            "{label} expected {} to equal Blake3 digest of {}, got {expected} but computed {actual}",
            digest_path.join("."),
            command_path.join(".")
        ));
    }
    Some(expected)
}

fn is_blake3_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

fn string_array_at(result: &serde_json::Value, path: &[&str]) -> Option<Vec<String>> {
    value_at(result, path)
        .and_then(serde_json::Value::as_array)
        .and_then(|values| {
            values
                .iter()
                .map(|value| value.as_str().map(str::to_owned))
                .collect::<Option<Vec<_>>>()
        })
        .filter(|values| !values.is_empty())
}

fn command_blake3(command: &[String]) -> String {
    let mut hasher = blake3::Hasher::new();
    for part in command {
        hasher.update(part.as_bytes());
        hasher.update(&[0]);
    }
    hasher.finalize().to_hex().to_string()
}

fn validate_repair_snapshot_evidence(
    errors: &mut Vec<String>,
    payload: &LiveSmokeEvidencePayload,
    result: &serde_json::Value,
) {
    require_args_contain(
        errors,
        &payload.args,
        &[
            "replica",
            "repair",
            "--from-coordinator",
            "--watch",
            "--samples",
            "--jsonl",
        ],
        &payload.label,
    );
    validate_repair_snapshot(errors, result, &payload.label);
}

fn validate_repair_snapshot(errors: &mut Vec<String>, result: &serde_json::Value, label: &str) {
    require_schema(errors, result, "replica.repair.event", label);
    require_string_value(errors, result, &["type"], "snapshot", label);
    require_min_u64_path(errors, result, &["data", "sample"], 1, label);
    require_min_u64_path(errors, result, &["data", "interval_seconds"], 1, label);
    require_u64_value(
        errors,
        result,
        &["data", "worker", "schema_version"],
        u64::from(REPAIR_WATCH_LEASE_SCHEMA_VERSION),
        label,
    );
    require_string_path(errors, result, &["data", "worker", "worker_id"], label);
    require_min_u64_path(errors, result, &["data", "worker", "pid"], 1, label);
    require_string_path(errors, result, &["data", "worker", "lease_path"], label);
    require_min_u64_path(
        errors,
        result,
        &["data", "worker", "heartbeat_at_ms"],
        1,
        label,
    );
    require_min_u64_path(
        errors,
        result,
        &["data", "worker", "expires_at_ms"],
        1,
        label,
    );
    require_min_u64_path(
        errors,
        result,
        &["data", "worker", "base_interval_seconds"],
        1,
        label,
    );
    require_min_u64_path(
        errors,
        result,
        &["data", "worker", "next_interval_seconds"],
        1,
        label,
    );
    require_min_u64_path(
        errors,
        result,
        &["data", "worker", "consecutive_errors"],
        0,
        label,
    );
    require_bool_any_path(errors, result, &["data", "worker", "dry_run"], label);
    validate_repair_worker_lease_window(errors, result, label);
    require_bool_path(
        errors,
        result,
        &["data", "repair", "from_coordinator"],
        true,
        label,
    );
    match value_at(result, &["data", "repair", "blocked_reason"]) {
        Some(value) if value.is_null() => {}
        Some(_) => errors.push(format!("{label} repair snapshot is blocked")),
        None => errors.push(format!("{label} is missing data.repair.blocked_reason")),
    }
}

fn validate_repair_worker_lease_window(
    errors: &mut Vec<String>,
    result: &serde_json::Value,
    label: &str,
) {
    let heartbeat_at_ms = value_at(result, &["data", "worker", "heartbeat_at_ms"])
        .and_then(serde_json::Value::as_u64);
    let expires_at_ms =
        value_at(result, &["data", "worker", "expires_at_ms"]).and_then(serde_json::Value::as_u64);
    if let (Some(heartbeat_at_ms), Some(expires_at_ms)) = (heartbeat_at_ms, expires_at_ms)
        && expires_at_ms <= heartbeat_at_ms
    {
        errors.push(format!(
            "{label} expected data.worker.expires_at_ms to be after data.worker.heartbeat_at_ms"
        ));
    }
}

fn validate_active_active_certification(
    errors: &mut Vec<String>,
    result: &serde_json::Value,
    label: &str,
) {
    require_schema(errors, result, "replica.certification", label);
    require_bool_path(errors, result, &["data", "certified"], true, label);
    require_bool_path(errors, result, &["data", "deep"], true, label);
    require_string_value(errors, result, &["data", "profile"], "active-active", label);
    require_active_active_certification_gates(errors, result, label);
}

fn require_active_active_certification_gates(
    errors: &mut Vec<String>,
    result: &serde_json::Value,
    label: &str,
) {
    let Some(gates) = value_at(result, &["data", "gates"]).and_then(serde_json::Value::as_array)
    else {
        errors.push(format!("{label} is missing data.gates"));
        return;
    };
    if gates.is_empty() {
        errors.push(format!("{label} data.gates is empty"));
        return;
    }

    let mut active_active_gate_seen = false;
    for gate in gates {
        let code = value_at(gate, &["code"]).and_then(serde_json::Value::as_str);
        match code {
            Some("certification.active-active") => active_active_gate_seen = true,
            Some(value) if !value.trim().is_empty() => {}
            _ => errors.push(format!(
                "{label} contains a certification gate without code"
            )),
        }
        match value_at(gate, &["state"]).and_then(serde_json::Value::as_str) {
            Some("passed") => {}
            Some(state) => errors.push(format!(
                "{label} expected certification gate {} to be passed, got {state}",
                code.unwrap_or("<unknown>")
            )),
            None => errors.push(format!(
                "{label} certification gate {} is missing state",
                code.unwrap_or("<unknown>")
            )),
        }
    }
    if !active_active_gate_seen {
        errors.push(format!(
            "{label} data.gates is missing certification.active-active"
        ));
    }
}

fn validate_provider_hydrate_count(
    errors: &mut Vec<String>,
    result: &serde_json::Value,
    field: &str,
    label: &str,
) {
    require_schema(errors, result, "replica.live-hydrate", label);
    require_min_u64_path(errors, result, &["data", field], 1, label);
}

fn validate_provider_hydrate_read_enabled(
    errors: &mut Vec<String>,
    result: &serde_json::Value,
    label: &str,
) {
    require_schema(errors, result, "replica.wait", label);
    require_bool_path(errors, result, &["data", "read_enabled"], true, label);
}

fn validate_provider_hydrate_selected_replica(
    errors: &mut Vec<String>,
    result: &serde_json::Value,
    label: &str,
) {
    validate_hydrate_result(errors, result, label);
}

fn validate_clone_success(
    errors: &mut Vec<String>,
    payload: &LiveSmokeEvidencePayload,
    result: &serde_json::Value,
    label: &str,
) {
    require_args_contain(errors, &payload.args, &["clone", "--json"], label);
    require_schema(errors, result, "clone", label);
    require_string_path(errors, result, &["data", "url"], label);
    require_string_path(errors, result, &["data", "directory"], label);
    require_bool_path(errors, result, &["data", "lazy"], false, label);
    require_string_path(errors, result, &["data", "reader_region"], label);
}

fn validate_hydrate_success(
    errors: &mut Vec<String>,
    payload: &LiveSmokeEvidencePayload,
    result: &serde_json::Value,
    label: &str,
) {
    validate_hydrate_result(errors, result, label);
    require_args_contain(errors, &payload.args, &["hydrate", "--json"], label);
    require_string_path(errors, result, &["data", "reader_region"], label);
}

fn validate_hydrate_result(errors: &mut Vec<String>, result: &serde_json::Value, label: &str) {
    require_schema(errors, result, "hydrate", label);
    require_min_u64_path(errors, result, &["data", "hydrated"], 1, label);
    require_u64_value(errors, result, &["data", "failed"], 0, label);
}

fn validate_non_empty_object(errors: &mut Vec<String>, result: &serde_json::Value, label: &str) {
    match result.as_object() {
        Some(object) if !object.is_empty() => {}
        _ => errors.push(format!("{label} result must be a non-empty object")),
    }
}

fn require_schema(
    errors: &mut Vec<String>,
    result: &serde_json::Value,
    expected: &str,
    label: &str,
) {
    require_string_value(errors, result, &["schema"], expected, label);
}

fn require_string_path(
    errors: &mut Vec<String>,
    result: &serde_json::Value,
    path: &[&str],
    label: &str,
) {
    match value_at(result, path).and_then(serde_json::Value::as_str) {
        Some(value) if !value.trim().is_empty() => {}
        _ => errors.push(format!("{label} is missing {}", path.join("."))),
    }
}

fn require_string_value(
    errors: &mut Vec<String>,
    result: &serde_json::Value,
    path: &[&str],
    expected: &str,
    label: &str,
) {
    match value_at(result, path).and_then(serde_json::Value::as_str) {
        Some(actual) if actual == expected => {}
        Some(actual) => errors.push(format!(
            "{label} expected {} to be {expected}, got {actual}",
            path.join(".")
        )),
        None => errors.push(format!("{label} is missing {}", path.join("."))),
    }
}

fn require_string_array_contains(
    errors: &mut Vec<String>,
    result: &serde_json::Value,
    path: &[&str],
    expected: &[&str],
    label: &str,
) {
    let Some(values) = value_at(result, path).and_then(serde_json::Value::as_array) else {
        errors.push(format!("{label} is missing {}", path.join(".")));
        return;
    };
    let observed = values
        .iter()
        .filter_map(serde_json::Value::as_str)
        .collect::<BTreeSet<_>>();
    for expected in expected {
        if !observed.contains(*expected) {
            errors.push(format!(
                "{label} expected {} to contain {expected}",
                path.join(".")
            ));
        }
    }
}

fn require_non_empty_string_array_path(
    errors: &mut Vec<String>,
    result: &serde_json::Value,
    path: &[&str],
    label: &str,
) {
    let Some(values) = value_at(result, path).and_then(serde_json::Value::as_array) else {
        errors.push(format!("{label} is missing {}", path.join(".")));
        return;
    };
    if values.is_empty() {
        errors.push(format!("{label} {} must not be empty", path.join(".")));
        return;
    }
    if !values
        .iter()
        .all(|value| value.as_str().is_some_and(|value| !value.trim().is_empty()))
    {
        errors.push(format!(
            "{label} {} must contain only non-empty strings",
            path.join(".")
        ));
    }
}

fn require_bool_path(
    errors: &mut Vec<String>,
    result: &serde_json::Value,
    path: &[&str],
    expected: bool,
    label: &str,
) {
    match value_at(result, path).and_then(serde_json::Value::as_bool) {
        Some(actual) if actual == expected => {}
        Some(actual) => errors.push(format!(
            "{label} expected {} to be {expected}, got {actual}",
            path.join(".")
        )),
        None => errors.push(format!("{label} is missing {}", path.join("."))),
    }
}

fn require_bool_any_path(
    errors: &mut Vec<String>,
    result: &serde_json::Value,
    path: &[&str],
    label: &str,
) {
    if value_at(result, path)
        .and_then(serde_json::Value::as_bool)
        .is_none()
    {
        errors.push(format!("{label} is missing {}", path.join(".")));
    }
}

fn require_min_u64_path(
    errors: &mut Vec<String>,
    result: &serde_json::Value,
    path: &[&str],
    minimum: u64,
    label: &str,
) {
    match value_at(result, path).and_then(serde_json::Value::as_u64) {
        Some(actual) if actual >= minimum => {}
        Some(actual) => errors.push(format!(
            "{label} expected {} to be at least {minimum}, got {actual}",
            path.join(".")
        )),
        None => errors.push(format!("{label} is missing {}", path.join("."))),
    }
}

fn require_u64_value(
    errors: &mut Vec<String>,
    result: &serde_json::Value,
    path: &[&str],
    expected: u64,
    label: &str,
) {
    match value_at(result, path).and_then(serde_json::Value::as_u64) {
        Some(actual) if actual == expected => {}
        Some(actual) => errors.push(format!(
            "{label} expected {} to be {expected}, got {actual}",
            path.join(".")
        )),
        None => errors.push(format!("{label} is missing {}", path.join("."))),
    }
}

fn require_u64_lte_path_pair(
    errors: &mut Vec<String>,
    result: &serde_json::Value,
    actual_path: &[&str],
    budget_path: &[&str],
    label: &str,
) {
    let actual = value_at(result, actual_path).and_then(serde_json::Value::as_u64);
    let budget = value_at(result, budget_path).and_then(serde_json::Value::as_u64);
    if let (Some(actual), Some(budget)) = (actual, budget)
        && actual > budget
    {
        errors.push(format!(
            "{label} expected {} ({actual}) to be <= {} ({budget})",
            actual_path.join("."),
            budget_path.join(".")
        ));
    }
}

fn require_verified_checks(
    errors: &mut Vec<String>,
    checks: Option<&serde_json::Value>,
    label: &str,
) {
    let Some(checks) = checks.and_then(serde_json::Value::as_array) else {
        errors.push(format!("{label} is missing checks"));
        return;
    };
    if checks.is_empty() {
        errors.push(format!("{label} checks are empty"));
        return;
    }
    for (index, check) in checks.iter().enumerate() {
        match value_at(check, &["state"]).and_then(serde_json::Value::as_str) {
            Some("verified") => {}
            Some(state) => errors.push(format!(
                "{label} contains non-verified control-plane check state {state}"
            )),
            None => {
                errors.push(format!(
                    "{label} contains a control-plane check without state"
                ));
            }
        }
        require_check_identity(errors, check, "code", label, index);
        require_check_identity(errors, check, "target", label, index);
        require_check_identity(errors, check, "managed_resource_id", label, index);
    }
}

fn require_check_identity(
    errors: &mut Vec<String>,
    check: &serde_json::Value,
    field: &str,
    label: &str,
    index: usize,
) {
    match value_at(check, &[field])
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
    {
        Some(value) if !value.is_empty() => {}
        _ => errors.push(format!(
            "{label} control-plane check {index} is missing {field}"
        )),
    }
}

fn value_at<'a>(value: &'a serde_json::Value, path: &[&str]) -> Option<&'a serde_json::Value> {
    let mut current = value;
    for segment in path {
        current = current.get(*segment)?;
    }
    Some(current)
}

fn evidence_relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .display()
        .to_string()
}

fn evidence_verify_summary(files: &[EvidenceFileStatus]) -> EvidenceVerifySummary {
    let mut summary = EvidenceVerifySummary {
        files_seen: files
            .iter()
            .filter(|file| file.path != "." || file.schema.is_some())
            .count() as u64,
        files_verified: 0,
        files_failed: 0,
        control_plane_evidence: 0,
        smoke_evidence: 0,
        redacted: 0,
        unredacted: 0,
    };

    for file in files {
        match file.state {
            EvidenceFileState::Verified => summary.files_verified += 1,
            EvidenceFileState::Failed => summary.files_failed += 1,
        }
        match file.kind {
            Some(EvidenceKind::LiveControlPlane) => summary.control_plane_evidence += 1,
            Some(EvidenceKind::LiveSmoke) => summary.smoke_evidence += 1,
            None => {}
        }
        match file.redacted {
            Some(true) => summary.redacted += 1,
            Some(false) => summary.unredacted += 1,
            None => {}
        }
    }
    summary
}

fn evidence_verify_gates(
    files: &[EvidenceFileStatus],
    require_redacted: bool,
    profile: EvidenceVerifyProfile,
    expected_run_id: Option<&str>,
) -> Vec<EvidenceVerifyGate> {
    let labels = evidence_verified_labels(files);
    let mut gates = Vec::new();
    let expected_run_id = expected_run_id
        .map(str::trim)
        .filter(|value| !value.is_empty());
    if profile == EvidenceVerifyProfile::Enterprise {
        match expected_run_id {
            Some(expected_run_id) if is_live_evidence_run_attempt_id(expected_run_id) => {
                gates.push(expected_evidence_run_id_gate(files, expected_run_id));
            }
            Some(expected_run_id) => {
                gates.push(malformed_expected_evidence_run_id_gate(expected_run_id))
            }
            None => gates.push(missing_expected_evidence_run_id_gate()),
        }
    } else if let Some(expected_run_id) = expected_run_id {
        gates.push(expected_evidence_run_id_gate(files, expected_run_id));
    }
    match profile {
        EvidenceVerifyProfile::Artifacts => {}
        EvidenceVerifyProfile::ControlPlaneStatus => {
            gates.push(profile_known_evidence_labels_gate(files, profile));
            gates.push(control_plane_status_gate(&labels));
        }
        EvidenceVerifyProfile::ControlPlaneMutate => {
            gates.push(profile_known_evidence_labels_gate(files, profile));
            gates.push(control_plane_status_gate(&labels));
            gates.push(control_plane_mutate_gate(&labels));
        }
        EvidenceVerifyProfile::ProviderHydrate => {
            gates.push(profile_known_evidence_labels_gate(files, profile));
            gates.extend(provider_hydrate_gates(&labels));
        }
        EvidenceVerifyProfile::ActiveActiveSmoke => {
            gates.push(profile_known_evidence_labels_gate(files, profile));
            gates.extend(active_active_smoke_gates(&labels));
        }
        EvidenceVerifyProfile::Enterprise => {
            gates.push(enterprise_evidence_redaction_gate(files, require_redacted));
            gates.push(enterprise_evidence_provenance_gate(files));
            gates.push(enterprise_evidence_sequence_gate(files));
            gates.push(enterprise_known_evidence_labels_gate(files));
            gates.push(control_plane_provider_sequence_gate(
                "enterprise-storage-provider-matrix",
                &labels,
                &STORAGE_EVIDENCE_PROVIDERS,
                &[
                    "storage-status",
                    "storage-apply",
                    "storage-status-after-apply",
                    "storage-remove-plan",
                    "storage-remove",
                ],
                "live storage replication apply/status/remove evidence covers S3, GCS, and Azure",
                "rerun the live storage control-plane harness with --storage-provider all and CRAB_REPLICA_LIVE_MUTATE=1 against disposable buckets",
            ));
            gates.push(control_plane_provider_sequence_gate(
                "enterprise-storage-provider-logs",
                &labels,
                &STORAGE_EVIDENCE_PROVIDERS,
                &[STORAGE_PROVIDER_LOG_LABEL],
                "provider-side storage replication logs are retained for S3, GCS, and Azure",
                "attach CRAB_REPLICA_LIVE_<PROVIDER>_PROVIDER_LOG_EVIDENCE for every storage provider and rerun the live control-plane harness",
            ));
            gates.push(control_plane_provider_log_artifact_gate(
                "enterprise-storage-provider-log-artifacts",
                &labels,
                STORAGE_PROVIDER_LOG_LABEL,
                &STORAGE_EVIDENCE_PROVIDERS,
                "storage provider logs point at distinct retained artifacts for S3, GCS, and Azure",
                "attach a distinct CRAB_REPLICA_LIVE_<PROVIDER>_PROVIDER_LOG_EVIDENCE artifact for every storage provider",
            ));
            gates.push(control_plane_provider_sequence_gate(
                "enterprise-coordinator-provider-matrix",
                &labels,
                &COORDINATOR_EVIDENCE_PROVIDERS,
                &[
                    "coordinator-plan",
                    "coordinator-status",
                    "coordinator-apply",
                    "coordinator-status-after-apply",
                    "coordinator-remove",
                ],
                "live coordinator apply/status/remove evidence covers DynamoDB, Spanner, and Cosmos DB",
                "rerun the live coordinator control-plane harness with --coordinator all and CRAB_REPLICA_LIVE_MUTATE=1 against disposable coordinators",
            ));
            gates.push(control_plane_provider_sequence_gate(
                "enterprise-coordinator-provider-logs",
                &labels,
                &COORDINATOR_EVIDENCE_PROVIDERS,
                &[COORDINATOR_PROVIDER_LOG_LABEL],
                "provider-side coordinator logs are retained for DynamoDB, Spanner, and Cosmos DB",
                "attach CRAB_REPLICA_LIVE_<PROVIDER>_PROVIDER_LOG_EVIDENCE for every coordinator provider and rerun the live control-plane harness",
            ));
            gates.push(control_plane_provider_log_artifact_gate(
                "enterprise-coordinator-provider-log-artifacts",
                &labels,
                COORDINATOR_PROVIDER_LOG_LABEL,
                &COORDINATOR_EVIDENCE_PROVIDERS,
                "coordinator provider logs point at distinct retained artifacts for DynamoDB, Spanner, and Cosmos DB",
                "attach a distinct CRAB_REPLICA_LIVE_<PROVIDER>_PROVIDER_LOG_EVIDENCE artifact for every coordinator provider",
            ));
            gates.push(enterprise_provider_log_artifact_scope_gate(&labels));
            gates.push(smoke_provider_sequence_gate(
                "enterprise-hydrate-provider-matrix",
                &labels,
                &STORAGE_EVIDENCE_PROVIDERS,
                PROVIDER_HYDRATE_SEQUENCE,
                "provider-backed hydrate evidence covers S3, GCS, and Azure",
                "rerun the provider-backed hydrate harness with --hydrate-provider all and retained redacted evidence enabled",
            ));
            gates.push(smoke_coordinator_provider_active_active_gate(
                "enterprise-active-active-coordinator-matrix",
                &labels,
                &COORDINATOR_EVIDENCE_PROVIDERS,
                "active-active smoke evidence covers DynamoDB, Spanner, and Cosmos DB coordinators",
                "rerun the active-active smoke harness with --coordinator all and retained redacted evidence enabled",
            ));
            gates.push(smoke_coordinator_writer_region_gate(
                "enterprise-active-active-writer-region-matrix",
                &labels,
                &COORDINATOR_EVIDENCE_PROVIDERS,
                2,
                "active-active smoke evidence records two writer regions for every coordinator provider",
                "rerun the active-active smoke harness with provider-specific writer A and writer B regions for every selected coordinator",
            ));
            gates.push(smoke_coordinator_provider_label_gate(
                "enterprise-production-load-matrix",
                &labels,
                &COORDINATOR_EVIDENCE_PROVIDERS,
                PRODUCTION_LOAD_LABEL,
                "production load evidence covers DynamoDB, Spanner, and Cosmos DB coordinators",
                "rerun the production load harness with retained redacted evidence enabled for every coordinator provider",
            ));
            gates.extend(active_active_smoke_gates(&labels));
        }
    }
    gates
}

fn expected_evidence_run_id_gate(
    files: &[EvidenceFileStatus],
    expected_run_id: &str,
) -> EvidenceVerifyGate {
    let mut observed = Vec::new();
    let mut failed = expected_run_id.is_empty();
    for file in files {
        if file.state != EvidenceFileState::Verified {
            continue;
        }
        let Some(label) = file.label.as_deref() else {
            continue;
        };
        if expected_evidence_harness(file.kind, label).is_none() {
            continue;
        }
        let Some(run_id) = file
            .run_id
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            failed = true;
            observed.push(format!("missing:{label}:{}", file.path));
            continue;
        };
        observed.push(format!("{run_id}:{label}:{}", file.path));
        if run_id != expected_run_id {
            failed = true;
        }
    }
    evidence_gate(
        "expected-live-evidence-run-id",
        !failed && !observed.is_empty(),
        "live evidence artifacts match the expected retained evidence run ID",
        "rerun the live evidence workflow for this release and verify the artifact from the same run attempt",
        observed,
    )
}

fn missing_expected_evidence_run_id_gate() -> EvidenceVerifyGate {
    evidence_gate(
        "expected-live-evidence-run-id",
        false,
        "enterprise evidence verification requires an expected retained evidence run ID",
        "rerun crab replica evidence verify --profile enterprise with --expected-run-id replica-live-<run-id>-<attempt>",
        Vec::new(),
    )
}

fn malformed_expected_evidence_run_id_gate(expected_run_id: &str) -> EvidenceVerifyGate {
    evidence_gate(
        "expected-live-evidence-run-id",
        false,
        "enterprise evidence verification requires a retained evidence run-attempt ID",
        "rerun crab replica evidence verify --profile enterprise with --expected-run-id replica-live-<run-id>-<attempt>",
        vec![format!("malformed:{expected_run_id}")],
    )
}

fn is_live_evidence_run_attempt_id(value: &str) -> bool {
    let Some(rest) = value.strip_prefix("replica-live-") else {
        return false;
    };
    let Some((run_id, attempt)) = rest.split_once('-') else {
        return false;
    };
    !run_id.is_empty()
        && !attempt.is_empty()
        && run_id.bytes().all(|byte| byte.is_ascii_digit())
        && attempt.bytes().all(|byte| byte.is_ascii_digit())
}

fn enterprise_evidence_redaction_gate(
    files: &[EvidenceFileStatus],
    require_redacted: bool,
) -> EvidenceVerifyGate {
    let mut labels = Vec::new();
    let mut failed = !require_redacted;
    if !require_redacted {
        labels.push("verification did not require redaction".to_owned());
    }
    for file in files {
        let Some(label) = file.label.as_deref() else {
            continue;
        };
        if expected_evidence_harness(file.kind, label).is_none() {
            continue;
        }
        let label_ref = format!("{label}:{}", file.path);
        if file.redacted == Some(true) {
            labels.push(label_ref);
        } else {
            failed = true;
            labels.push(label_ref);
        }
    }
    evidence_gate(
        "enterprise-redacted-evidence",
        !failed && !labels.is_empty(),
        "enterprise evidence artifacts are redacted and verification required redaction",
        "rerun crab replica evidence verify --profile enterprise with --require-redacted against a redacted retained evidence bundle",
        labels,
    )
}

fn enterprise_evidence_provenance_gate(files: &[EvidenceFileStatus]) -> EvidenceVerifyGate {
    let mut observed = Vec::new();
    let mut missing = Vec::new();
    let mut run_ids = BTreeSet::new();
    for file in files {
        if file.state != EvidenceFileState::Verified {
            continue;
        }
        let Some(label) = file.label.as_deref() else {
            continue;
        };
        let Some(expected_harness) = expected_evidence_harness(file.kind, label) else {
            continue;
        };
        let label_ref = format!("{label}:{}", file.path);
        let has_expected_harness = file.harness.as_deref() == Some(expected_harness);
        let has_run_id = file
            .run_id
            .as_deref()
            .is_some_and(|run_id| !run_id.trim().is_empty());
        let has_sequence = file.sequence.is_some_and(|sequence| sequence > 0);
        if has_expected_harness && has_run_id && has_sequence {
            if let Some(run_id) = file.run_id.as_deref() {
                run_ids.insert(run_id.to_owned());
            }
            observed.push(label_ref);
        } else {
            missing.push(label_ref);
        }
    }
    let passed = !observed.is_empty() && missing.is_empty() && run_ids.len() == 1;
    evidence_gate(
        "enterprise-live-evidence-provenance",
        passed,
        "enterprise evidence includes live harness, one shared run ID, and sequence provenance",
        "rerun the live evidence workflow with CRAB_REPLICA_LIVE_RUN_ID set so every artifact includes harness, the same run_id, and sequence",
        observed,
    )
}

fn enterprise_evidence_sequence_gate(files: &[EvidenceFileStatus]) -> EvidenceVerifyGate {
    let mut streams: BTreeMap<String, Vec<(u64, String)>> = BTreeMap::new();
    for file in files {
        if file.state != EvidenceFileState::Verified {
            continue;
        }
        let Some(label) = file.label.as_deref() else {
            continue;
        };
        if expected_evidence_harness(file.kind, label).is_none() {
            continue;
        }
        let (Some(sequence), Some(key)) = (file.sequence, evidence_sequence_stream_key(file))
        else {
            continue;
        };
        streams
            .entry(key)
            .or_default()
            .push((sequence, format!("{label}:{}", file.path)));
    }

    let mut observed = Vec::new();
    let mut failed = false;
    for (stream, entries) in streams {
        let mut seen = BTreeSet::new();
        let mut previous = None;
        let mut expected = 1;
        for (sequence, label_ref) in entries {
            let entry = format!("{stream}:{sequence}:{label_ref}");
            if sequence != expected
                || !seen.insert(sequence)
                || previous.is_some_and(|value| sequence <= value)
            {
                failed = true;
            }
            previous = Some(sequence);
            expected += 1;
            observed.push(entry);
        }
    }

    evidence_gate(
        "enterprise-live-evidence-sequences",
        !observed.is_empty() && !failed,
        "enterprise evidence sequences are contiguous per live harness/provider stream",
        "rerun the live evidence workflow into a fresh evidence directory so each run/harness/provider stream has contiguous sequence numbers starting at 1",
        observed,
    )
}

fn profile_known_evidence_labels_gate(
    files: &[EvidenceFileStatus],
    profile: EvidenceVerifyProfile,
) -> EvidenceVerifyGate {
    let mut observed = Vec::new();
    let mut unknown = Vec::new();
    for file in files {
        if file.state != EvidenceFileState::Verified {
            continue;
        }
        let Some(label) = file.label.as_deref() else {
            continue;
        };
        match file.kind {
            Some(EvidenceKind::LiveControlPlane | EvidenceKind::LiveSmoke) => {
                let label_ref = format!("{label}:{}", file.path);
                if evidence_label_allowed_for_profile(profile, file.kind, label) {
                    observed.push(label_ref);
                } else {
                    unknown.push(label_ref);
                }
            }
            None => {}
        }
    }
    observed.extend(unknown.iter().map(|label| format!("unexpected:{label}")));
    evidence_gate(
        &format!("{}-known-evidence-labels", profile.as_str()),
        !observed.is_empty() && unknown.is_empty(),
        "retained evidence contains only milestones supported by the selected profile",
        "remove unsupported artifacts from the retained bundle or rerun evidence verification with the matching profile",
        observed,
    )
}

fn enterprise_known_evidence_labels_gate(files: &[EvidenceFileStatus]) -> EvidenceVerifyGate {
    let mut observed = Vec::new();
    let mut unknown = Vec::new();
    for file in files {
        if file.state != EvidenceFileState::Verified {
            continue;
        }
        let Some(label) = file.label.as_deref() else {
            continue;
        };
        match file.kind {
            Some(EvidenceKind::LiveControlPlane | EvidenceKind::LiveSmoke) => {
                let label_ref = format!("{label}:{}", file.path);
                if expected_evidence_harness(file.kind, label).is_some() {
                    observed.push(label_ref);
                } else {
                    unknown.push(label_ref);
                }
            }
            None => {}
        }
    }
    observed.extend(unknown.iter().map(|label| format!("unknown:{label}")));
    evidence_gate(
        "enterprise-known-evidence-labels",
        !observed.is_empty() && unknown.is_empty(),
        "enterprise evidence contains only supported live evidence milestones",
        "remove unsupported artifacts from the retained bundle or rerun the live evidence workflow with a supported evidence profile",
        observed,
    )
}

fn evidence_label_allowed_for_profile(
    profile: EvidenceVerifyProfile,
    kind: Option<EvidenceKind>,
    label: &str,
) -> bool {
    match profile {
        EvidenceVerifyProfile::Artifacts => true,
        EvidenceVerifyProfile::ControlPlaneStatus => {
            kind == Some(EvidenceKind::LiveControlPlane)
                && matches!(
                    label,
                    "storage-status"
                        | "coordinator-status"
                        | STORAGE_PROVIDER_LOG_LABEL
                        | COORDINATOR_PROVIDER_LOG_LABEL
                )
        }
        EvidenceVerifyProfile::ControlPlaneMutate => {
            kind == Some(EvidenceKind::LiveControlPlane)
                && (is_storage_control_plane_label(label)
                    || is_coordinator_control_plane_label(label))
        }
        EvidenceVerifyProfile::ProviderHydrate => {
            kind == Some(EvidenceKind::LiveSmoke) && is_provider_hydrate_milestone_label(label)
        }
        EvidenceVerifyProfile::ActiveActiveSmoke => {
            kind == Some(EvidenceKind::LiveSmoke) && is_active_active_smoke_label(label)
        }
        EvidenceVerifyProfile::Enterprise => expected_evidence_harness(kind, label).is_some(),
    }
}

fn evidence_sequence_stream_key(file: &EvidenceFileStatus) -> Option<String> {
    let run_id = file.run_id.as_deref()?.trim();
    let harness = file.harness.as_deref()?.trim();
    if run_id.is_empty() || harness.is_empty() {
        return None;
    }
    let discriminator = match file.kind {
        Some(EvidenceKind::LiveControlPlane) => file.provider.as_deref(),
        Some(EvidenceKind::LiveSmoke) if file.provider.is_some() => file.provider.as_deref(),
        Some(EvidenceKind::LiveSmoke) => file.coordinator_provider.as_deref(),
        None => None,
    }?
    .trim();
    if discriminator.is_empty() {
        return None;
    }
    Some(format!("{run_id}:{harness}:{discriminator}"))
}

fn expected_evidence_harness(kind: Option<EvidenceKind>, label: &str) -> Option<&'static str> {
    match kind {
        Some(EvidenceKind::LiveControlPlane)
            if is_storage_control_plane_label(label)
                || is_coordinator_control_plane_label(label) =>
        {
            Some(CONTROL_PLANE_EVIDENCE_HARNESS)
        }
        Some(EvidenceKind::LiveSmoke) if is_provider_hydrate_milestone_label(label) => {
            Some(PROVIDER_HYDRATE_EVIDENCE_HARNESS)
        }
        Some(EvidenceKind::LiveSmoke) if is_active_active_smoke_label(label) => {
            Some(ACTIVE_ACTIVE_EVIDENCE_HARNESS)
        }
        Some(EvidenceKind::LiveSmoke) if is_production_load_label(label) => {
            Some(PRODUCTION_LOAD_EVIDENCE_HARNESS)
        }
        _ => None,
    }
}

#[derive(Debug, Default)]
struct EvidenceVerifiedLabels {
    control_plane: Vec<String>,
    control_plane_by_provider: Vec<ProviderEvidenceLabel>,
    control_plane_provider_log_refs: Vec<ProviderLogArtifactEvidenceLabel>,
    smoke: Vec<String>,
    smoke_by_provider: Vec<ProviderEvidenceLabel>,
    smoke_by_coordinator: Vec<ProviderEvidenceLabel>,
    smoke_push_writer_regions: Vec<WriterRegionEvidenceLabel>,
    smoke_read_regions: Vec<ReaderRegionEvidenceLabel>,
    smoke_push_operation_ids: Vec<PushOperationEvidenceLabel>,
    smoke_repair_service_templates: Vec<RepairServiceTemplateEvidenceLabel>,
}

#[derive(Debug, Clone)]
struct ProviderEvidenceLabel {
    provider: String,
    label: String,
}

#[derive(Debug, Clone)]
struct ProviderLogArtifactEvidenceLabel {
    provider: String,
    label: String,
    artifact_ref: String,
}

#[derive(Debug, Clone)]
struct RepairServiceTemplateEvidenceLabel {
    coordinator_provider: Option<String>,
    label: String,
    service_template: String,
    template_blake3: Option<String>,
    command_blake3: Option<String>,
}

#[derive(Debug, Clone)]
struct WriterRegionEvidenceLabel {
    coordinator_provider: Option<String>,
    region: String,
    label: String,
}

#[derive(Debug, Clone)]
struct ReaderRegionEvidenceLabel {
    coordinator_provider: Option<String>,
    region: String,
    label: String,
}

#[derive(Debug, Clone)]
struct PushOperationEvidenceLabel {
    coordinator_provider: Option<String>,
    operation_id: String,
    label: String,
}

fn evidence_verified_labels(files: &[EvidenceFileStatus]) -> EvidenceVerifiedLabels {
    let mut labels = EvidenceVerifiedLabels::default();
    for file in files {
        if file.state != EvidenceFileState::Verified {
            continue;
        }
        let Some(label) = file.label.as_ref() else {
            continue;
        };
        match file.kind {
            Some(EvidenceKind::LiveControlPlane) => {
                labels.control_plane.push(label.clone());
                if let Some(provider) = file.provider.as_ref() {
                    labels
                        .control_plane_by_provider
                        .push(ProviderEvidenceLabel {
                            provider: provider.clone(),
                            label: label.clone(),
                        });
                    if let Some(artifact_ref) = file.provider_log_artifact_ref.as_ref() {
                        labels.control_plane_provider_log_refs.push(
                            ProviderLogArtifactEvidenceLabel {
                                provider: provider.clone(),
                                label: label.clone(),
                                artifact_ref: artifact_ref.clone(),
                            },
                        );
                    }
                }
            }
            Some(EvidenceKind::LiveSmoke) => {
                labels.smoke.push(label.clone());
                if let Some(provider) = file.provider.as_ref() {
                    labels.smoke_by_provider.push(ProviderEvidenceLabel {
                        provider: provider.clone(),
                        label: label.clone(),
                    });
                }
                if let Some(provider) = file.coordinator_provider.as_ref() {
                    labels.smoke_by_coordinator.push(ProviderEvidenceLabel {
                        provider: provider.clone(),
                        label: label.clone(),
                    });
                }
                if let Some(region) = file.writer_region.as_ref() {
                    labels
                        .smoke_push_writer_regions
                        .push(WriterRegionEvidenceLabel {
                            coordinator_provider: file.coordinator_provider.clone(),
                            region: region.clone(),
                            label: label.clone(),
                        });
                }
                if let Some(region) = file.reader_region.as_ref() {
                    labels.smoke_read_regions.push(ReaderRegionEvidenceLabel {
                        coordinator_provider: file.coordinator_provider.clone(),
                        region: region.clone(),
                        label: label.clone(),
                    });
                }
                if let Some(operation_id) = file.push_operation_id.as_ref() {
                    labels
                        .smoke_push_operation_ids
                        .push(PushOperationEvidenceLabel {
                            coordinator_provider: file.coordinator_provider.clone(),
                            operation_id: operation_id.clone(),
                            label: label.clone(),
                        });
                }
                if let Some(service_template) = file.repair_service_template.as_ref() {
                    labels.smoke_repair_service_templates.push(
                        RepairServiceTemplateEvidenceLabel {
                            coordinator_provider: file.coordinator_provider.clone(),
                            label: label.clone(),
                            service_template: service_template.clone(),
                            template_blake3: file.repair_template_blake3.clone(),
                            command_blake3: file.repair_command_blake3.clone(),
                        },
                    );
                }
            }
            None => {}
        }
    }
    labels
}

fn control_plane_status_gate(labels: &EvidenceVerifiedLabels) -> EvidenceVerifyGate {
    let accepted = ["storage-status", "coordinator-status"];
    let observed = matching_exact_labels(&labels.control_plane, &accepted);
    evidence_gate(
        "control-plane-status",
        !observed.is_empty(),
        "live control-plane status evidence is present",
        "run the live control-plane harness without CRAB_REPLICA_LIVE_MUTATE first",
        observed,
    )
}

fn control_plane_mutate_gate(labels: &EvidenceVerifiedLabels) -> EvidenceVerifyGate {
    let storage = [
        "storage-status",
        "storage-apply",
        "storage-status-after-apply",
        "storage-remove-plan",
        "storage-remove",
    ];
    let coordinator = [
        "coordinator-plan",
        "coordinator-status",
        "coordinator-apply",
        "coordinator-status-after-apply",
        "coordinator-remove",
    ];
    let storage_labels = matching_exact_labels(&labels.control_plane, &storage);
    let coordinator_labels = matching_exact_labels(&labels.control_plane, &coordinator);
    let passed =
        storage_labels.len() == storage.len() || coordinator_labels.len() == coordinator.len();
    let mut observed = storage_labels;
    observed.extend(coordinator_labels);
    observed.sort();
    observed.dedup();
    evidence_gate(
        "control-plane-apply-remove",
        passed,
        "live control-plane apply/status/remove evidence is complete",
        "rerun the live control-plane harness with CRAB_REPLICA_LIVE_MUTATE=1 against disposable resources",
        observed,
    )
}

fn control_plane_provider_sequence_gate(
    code: &str,
    labels: &EvidenceVerifiedLabels,
    providers: &[&str],
    required: &[&str],
    message: &str,
    remediation: &str,
) -> EvidenceVerifyGate {
    provider_sequence_gate(
        code,
        &labels.control_plane_by_provider,
        providers,
        required,
        message,
        remediation,
    )
}

fn control_plane_provider_log_artifact_gate(
    code: &str,
    labels: &EvidenceVerifiedLabels,
    required_label: &str,
    providers: &[&str],
    message: &str,
    remediation: &str,
) -> EvidenceVerifyGate {
    let mut observed = Vec::new();
    let mut provider_refs = BTreeMap::<String, BTreeSet<String>>::new();
    let mut artifact_providers = BTreeMap::<String, BTreeSet<String>>::new();
    for entry in labels
        .control_plane_provider_log_refs
        .iter()
        .filter(|entry| entry.label == required_label)
    {
        provider_refs
            .entry(entry.provider.clone())
            .or_default()
            .insert(entry.artifact_ref.clone());
        artifact_providers
            .entry(entry.artifact_ref.clone())
            .or_default()
            .insert(entry.provider.clone());
        observed.push(format!(
            "{}:{}:{}",
            entry.provider, entry.label, entry.artifact_ref
        ));
    }

    let mut passed = !providers.is_empty();
    for provider in providers {
        if !provider_refs.contains_key(*provider) {
            passed = false;
            observed.push(format!("{provider}:missing-{required_label}-artifact"));
        }
    }
    for (artifact_ref, providers) in artifact_providers {
        if providers.len() > 1 {
            passed = false;
            observed.push(format!(
                "duplicate-artifact:{}:{}",
                providers.into_iter().collect::<Vec<_>>().join(","),
                artifact_ref
            ));
        }
    }

    observed.sort();
    observed.dedup();
    evidence_gate(
        code,
        passed && !observed.is_empty(),
        message,
        remediation,
        observed,
    )
}

fn enterprise_provider_log_artifact_scope_gate(
    labels: &EvidenceVerifiedLabels,
) -> EvidenceVerifyGate {
    let mut observed = Vec::new();
    let mut artifact_scopes = BTreeMap::<String, BTreeSet<String>>::new();
    for entry in labels
        .control_plane_provider_log_refs
        .iter()
        .filter(|entry| {
            matches!(
                entry.label.as_str(),
                STORAGE_PROVIDER_LOG_LABEL | COORDINATOR_PROVIDER_LOG_LABEL
            )
        })
    {
        let scope = format!("{}:{}", entry.provider, entry.label);
        artifact_scopes
            .entry(entry.artifact_ref.clone())
            .or_default()
            .insert(scope.clone());
        observed.push(format!("{scope}:{}", entry.artifact_ref));
    }

    let mut passed = !observed.is_empty();
    for (artifact_ref, scopes) in artifact_scopes {
        if scopes.len() > 1 {
            passed = false;
            observed.push(format!(
                "duplicate-artifact-scope:{}:{}",
                scopes.into_iter().collect::<Vec<_>>().join(","),
                artifact_ref
            ));
        }
    }
    observed.sort();
    observed.dedup();
    evidence_gate(
        "enterprise-provider-log-artifact-scopes",
        passed,
        "provider-side logs use distinct retained artifacts across storage and coordinator scopes",
        "attach a distinct provider-log artifact for every storage and coordinator provider before retaining enterprise evidence",
        observed,
    )
}

fn provider_hydrate_gates(labels: &EvidenceVerifiedLabels) -> Vec<EvidenceVerifyGate> {
    vec![smoke_sequence_gate(
        "provider-hydrate",
        labels,
        PROVIDER_HYDRATE_SEQUENCE,
        "provider-backed hydrate proof recorded replica-selected reconstruction after primary data loss",
        "rerun the provider-backed hydrate harness with CRAB_REPLICA_LIVE_EVIDENCE_DIR and CRAB_REPLICA_LIVE_EVIDENCE_REDACT=1",
    )]
}

fn smoke_provider_sequence_gate(
    code: &str,
    labels: &EvidenceVerifiedLabels,
    providers: &[&str],
    required: &[&str],
    message: &str,
    remediation: &str,
) -> EvidenceVerifyGate {
    provider_sequence_gate(
        code,
        &labels.smoke_by_provider,
        providers,
        required,
        message,
        remediation,
    )
}

fn smoke_coordinator_provider_active_active_gate(
    code: &str,
    labels: &EvidenceVerifiedLabels,
    providers: &[&str],
    message: &str,
    remediation: &str,
) -> EvidenceVerifyGate {
    let mut observed = Vec::new();
    let mut passed = !providers.is_empty();
    for provider in providers {
        let provider_labels = labels
            .smoke_by_coordinator
            .iter()
            .filter(|label| label.provider == *provider)
            .map(|label| label.label.as_str())
            .collect::<Vec<_>>();
        let sequence = matching_active_active_semantic_sequence(&provider_labels);
        observed.extend(sequence.iter().map(|label| format!("{provider}:{label}")));
        if sequence.len() != ACTIVE_ACTIVE_SMOKE_SEMANTIC_SEQUENCE.len() {
            passed = false;
            observed.push(format!("{provider}:incomplete-active-active-sequence"));
        }
    }
    evidence_gate(code, passed, message, remediation, observed)
}

fn smoke_coordinator_provider_label_gate(
    code: &str,
    labels: &EvidenceVerifiedLabels,
    providers: &[&str],
    required_label: &str,
    message: &str,
    remediation: &str,
) -> EvidenceVerifyGate {
    let mut observed = Vec::new();
    let mut passed = !providers.is_empty();
    for provider in providers {
        let has_label = labels
            .smoke_by_coordinator
            .iter()
            .any(|label| label.provider == *provider && label.label == required_label);
        if has_label {
            observed.push(format!("{provider}:{required_label}"));
        } else {
            passed = false;
            observed.push(format!("{provider}:missing-{required_label}"));
        }
    }
    evidence_gate(code, passed, message, remediation, observed)
}

fn smoke_coordinator_writer_region_gate(
    code: &str,
    labels: &EvidenceVerifiedLabels,
    providers: &[&str],
    required_regions: usize,
    message: &str,
    remediation: &str,
) -> EvidenceVerifyGate {
    let mut observed = Vec::new();
    let mut passed = true;
    for provider in providers {
        let mut regions = BTreeSet::new();
        for entry in labels
            .smoke_push_writer_regions
            .iter()
            .filter(|entry| entry.coordinator_provider.as_deref() == Some(*provider))
        {
            regions.insert(entry.region.clone());
            observed.push(format!("{provider}:{}:{}", entry.region, entry.label));
        }
        if regions.len() < required_regions {
            passed = false;
        }
    }
    observed.sort();
    observed.dedup();
    evidence_gate(
        code,
        passed && !observed.is_empty(),
        message,
        remediation,
        observed,
    )
}

fn smoke_coordinator_reader_region_gate(
    code: &str,
    labels: &EvidenceVerifiedLabels,
    providers: &[&str],
    required_regions: usize,
    message: &str,
    remediation: &str,
) -> EvidenceVerifyGate {
    let mut observed = Vec::new();
    let mut passed = true;
    for provider in providers {
        let mut regions = BTreeSet::new();
        for entry in labels
            .smoke_read_regions
            .iter()
            .filter(|entry| entry.coordinator_provider.as_deref() == Some(*provider))
        {
            regions.insert(entry.region.clone());
            observed.push(format!("{provider}:{}:{}", entry.region, entry.label));
        }
        if regions.len() < required_regions {
            passed = false;
        }
    }
    observed.sort();
    observed.dedup();
    evidence_gate(
        code,
        passed && !observed.is_empty(),
        message,
        remediation,
        observed,
    )
}

fn provider_sequence_gate(
    code: &str,
    labels: &[ProviderEvidenceLabel],
    providers: &[&str],
    required: &[&str],
    message: &str,
    remediation: &str,
) -> EvidenceVerifyGate {
    let observed = matching_provider_sequences(labels, providers, required);
    evidence_gate(
        code,
        observed.len() == providers.len() * required.len(),
        message,
        remediation,
        observed,
    )
}

fn observed_smoke_coordinator_providers(labels: &EvidenceVerifiedLabels) -> Vec<&str> {
    let mut providers = labels
        .smoke_by_coordinator
        .iter()
        .map(|label| label.provider.as_str())
        .collect::<Vec<_>>();
    providers.sort();
    providers.dedup();
    providers
}

fn active_active_smoke_gates(labels: &EvidenceVerifiedLabels) -> Vec<EvidenceVerifyGate> {
    let coordinator_providers = observed_smoke_coordinator_providers(labels);
    vec![
        smoke_coordinator_provider_gate(labels),
        smoke_active_active_semantic_sequence_gate(labels),
        smoke_coordinator_provider_active_active_gate(
            "active-active-coordinator-provider-sequence",
            labels,
            &coordinator_providers,
            "active-active smoke evidence records a complete story for each observed coordinator provider",
            "rerun the active-active smoke from a fresh evidence directory for one coordinator provider at a time, or use the enterprise matrix harness",
        ),
        smoke_sequence_gate(
            "active-active-configured",
            labels,
            &["mode-active-active", "initial-failover-status"],
            "active-active mode and initial write admission were recorded",
            "rerun the active-active smoke until mode setup and failover status are recorded",
        ),
        smoke_exact_gate(
            "active-active-repair-service-template",
            labels,
            &["repair-service-template"],
            "repair-worker supervisor template evidence was recorded",
            "rerun the active-active smoke until crab replica repair --service-template evidence is recorded",
        ),
        smoke_exact_gate(
            "active-active-repair-worker-deployment",
            labels,
            &["repair-worker-deployment"],
            "repair-worker supervisor deployment evidence was recorded",
            "rerun the active-active smoke with CRAB_REPLICA_LIVE_REPAIR_WORKER_DEPLOYMENT_EVIDENCE set",
        ),
        smoke_repair_service_template_consistency_gate(labels),
        smoke_push_writer_region_gate(
            "active-active-writer-pushes",
            labels,
            2,
            "pushes through both writer regions were recorded",
            "rerun the active-active smoke until both writer-region pushes succeed",
        ),
        smoke_push_distinct_operation_ids_gate(labels),
        smoke_coordinator_writer_region_gate(
            "active-active-writer-regions-by-coordinator",
            labels,
            &coordinator_providers,
            2,
            "active-active smoke evidence records two writer regions for each observed coordinator provider",
            "rerun the active-active smoke with writer A and writer B regions for the same coordinator provider",
        ),
        smoke_coordinator_reader_region_gate(
            "active-active-reader-regions-by-coordinator",
            labels,
            &coordinator_providers,
            2,
            "active-active smoke evidence records two reader regions for each observed coordinator provider",
            "rerun the active-active smoke so each coordinator provider clones and hydrates from both writer-region URLs",
        ),
        smoke_sequence_gate(
            "active-active-fencing",
            labels,
            &[
                "failover-fence",
                "writes-fenced",
                "failover-resume",
                "writes-enabled",
            ],
            "coordinator fence and resume milestones were recorded",
            "rerun the active-active smoke until fence, blocked status, resume, and healthy status are recorded",
        ),
        smoke_count_gate(
            "active-active-fenced-rejection",
            labels,
            |label| label.starts_with("push-rejected-fenced-"),
            1,
            "a fenced write rejection was recorded",
            "rerun the active-active smoke until a push is rejected while writes are fenced",
        ),
        smoke_count_gate(
            "active-active-stale-ref-rejection",
            labels,
            |label| {
                label.starts_with("push-rejected-") && !label.starts_with("push-rejected-fenced-")
            },
            1,
            "a same-ref stale writer rejection was recorded",
            "rerun the active-active smoke until a divergent same-ref push is rejected",
        ),
        smoke_count_gate(
            "active-active-repair",
            labels,
            |label| label == "repair-snapshot",
            2,
            "coordinator-backed repair snapshots were recorded for both directions",
            "rerun the active-active smoke until repair snapshots are recorded after both writer pushes",
        ),
        smoke_count_gate(
            "active-active-clone-hydrate",
            labels,
            |label| label.starts_with("clone-"),
            2,
            "cross-region clone evidence was recorded for both directions",
            "rerun the active-active smoke until both opposite-region clones are recorded",
        ),
        smoke_count_gate(
            "active-active-hydrate",
            labels,
            |label| label.starts_with("hydrate-"),
            2,
            "cross-region hydrate evidence was recorded for both directions",
            "rerun the active-active smoke until both opposite-region hydrates are recorded",
        ),
        smoke_exact_gate(
            "active-active-certification",
            labels,
            &["active-active-certification"],
            "active-active certification evidence was recorded",
            "rerun the active-active smoke until crab replica certify --profile active-active passes",
        ),
    ]
}

fn smoke_active_active_semantic_sequence_gate(
    labels: &EvidenceVerifiedLabels,
) -> EvidenceVerifyGate {
    let label_refs = labels.smoke.iter().map(String::as_str).collect::<Vec<_>>();
    let observed = matching_active_active_semantic_sequence(&label_refs);
    evidence_gate(
        "active-active-smoke-sequence",
        observed.len() == ACTIVE_ACTIVE_SMOKE_SEMANTIC_SEQUENCE.len(),
        "active-active smoke recorded the full ordered write, failover, repair, clone, hydrate, and conflict story",
        "rerun the active-active smoke from a fresh evidence directory so all milestones are recorded in order",
        observed,
    )
}

fn smoke_repair_service_template_consistency_gate(
    labels: &EvidenceVerifiedLabels,
) -> EvidenceVerifyGate {
    let mut grouped =
        BTreeMap::<Option<String>, BTreeMap<String, RepairServiceTemplateEvidenceLabel>>::new();
    for entry in &labels.smoke_repair_service_templates {
        grouped
            .entry(entry.coordinator_provider.clone())
            .or_default()
            .insert(entry.label.clone(), entry.clone());
    }

    let mut observed = Vec::new();
    let mut passed = !grouped.is_empty();
    for (provider, entries) in grouped {
        let provider = provider.unwrap_or_else(|| "unknown".to_owned());
        let template = entries.get("repair-service-template");
        let deployment = entries.get("repair-worker-deployment");
        match (template, deployment) {
            (Some(template), Some(deployment))
                if repair_template_proof_matches(template, deployment) =>
            {
                observed.push(repair_template_observed_label(&provider, template));
                observed.push(repair_template_observed_label(&provider, deployment));
            }
            (Some(template), Some(deployment)) => {
                passed = false;
                observed.push(repair_template_observed_label(&provider, template));
                observed.push(repair_template_observed_label(&provider, deployment));
            }
            (Some(template), None) => {
                passed = false;
                observed.push(repair_template_observed_label(&provider, template));
            }
            (None, Some(deployment)) => {
                passed = false;
                observed.push(repair_template_observed_label(&provider, deployment));
            }
            (None, None) => {
                passed = false;
            }
        }
    }
    observed.sort();
    observed.dedup();
    evidence_gate(
        "active-active-repair-worker-template-match",
        passed,
        "repair-worker template and deployment evidence use the same supervisor target, generated template digest, and worker command digest",
        "rerun the active-active smoke after deploying the generated repair-worker supervisor template and recording matching deployment proof",
        observed,
    )
}

fn repair_template_proof_matches(
    template: &RepairServiceTemplateEvidenceLabel,
    deployment: &RepairServiceTemplateEvidenceLabel,
) -> bool {
    template.service_template == deployment.service_template
        && template.template_blake3.is_some()
        && template.template_blake3 == deployment.template_blake3
        && template.command_blake3.is_some()
        && template.command_blake3 == deployment.command_blake3
}

fn repair_template_observed_label(
    provider: &str,
    entry: &RepairServiceTemplateEvidenceLabel,
) -> String {
    format!(
        "{provider}:{}={}:template_blake3={}:command_blake3={}",
        entry.label,
        entry.service_template,
        entry.template_blake3.as_deref().unwrap_or("missing"),
        entry.command_blake3.as_deref().unwrap_or("missing")
    )
}

fn smoke_coordinator_provider_gate(labels: &EvidenceVerifiedLabels) -> EvidenceVerifyGate {
    let mut observed = labels
        .smoke_by_coordinator
        .iter()
        .map(|label| label.provider.clone())
        .collect::<Vec<_>>();
    observed.sort();
    observed.dedup();
    evidence_gate(
        "active-active-coordinator-provider",
        !observed.is_empty(),
        "active-active smoke identified the managed coordinator provider",
        "rerun the active-active smoke with a supported coordinator URL and retained evidence enabled",
        observed,
    )
}

fn smoke_exact_gate(
    code: &str,
    labels: &EvidenceVerifiedLabels,
    required: &[&str],
    message: &str,
    remediation: &str,
) -> EvidenceVerifyGate {
    let observed = matching_exact_smoke_labels(&labels.smoke, required);
    evidence_gate(
        code,
        observed.len() == required.len(),
        message,
        remediation,
        observed,
    )
}

fn smoke_sequence_gate(
    code: &str,
    labels: &EvidenceVerifiedLabels,
    required: &[&str],
    message: &str,
    remediation: &str,
) -> EvidenceVerifyGate {
    let observed = matching_ordered_labels(&labels.smoke, required);
    evidence_gate(
        code,
        observed.len() == required.len(),
        message,
        remediation,
        observed,
    )
}

fn smoke_count_gate(
    code: &str,
    labels: &EvidenceVerifiedLabels,
    predicate: impl Fn(&str) -> bool,
    required_count: usize,
    message: &str,
    remediation: &str,
) -> EvidenceVerifyGate {
    let observed = labels
        .smoke
        .iter()
        .filter(|label| predicate(label))
        .cloned()
        .collect::<Vec<_>>();
    evidence_gate(
        code,
        observed.len() >= required_count,
        message,
        remediation,
        observed,
    )
}

fn smoke_push_writer_region_gate(
    code: &str,
    labels: &EvidenceVerifiedLabels,
    required_regions: usize,
    message: &str,
    remediation: &str,
) -> EvidenceVerifyGate {
    let mut observed = labels
        .smoke_push_writer_regions
        .iter()
        .map(|entry| format!("{}:{}", entry.region, entry.label))
        .collect::<Vec<_>>();
    observed.sort();
    observed.dedup();
    let mut regions = labels
        .smoke_push_writer_regions
        .iter()
        .map(|entry| entry.region.clone())
        .collect::<Vec<_>>();
    regions.sort();
    regions.dedup();
    evidence_gate(
        code,
        regions.len() >= required_regions,
        message,
        remediation,
        observed,
    )
}

fn smoke_push_distinct_operation_ids_gate(labels: &EvidenceVerifiedLabels) -> EvidenceVerifyGate {
    let mut observed = Vec::new();
    let mut by_operation = BTreeMap::<(String, String), BTreeSet<String>>::new();
    for entry in &labels.smoke_push_operation_ids {
        let provider = entry
            .coordinator_provider
            .as_deref()
            .unwrap_or("unknown")
            .to_owned();
        observed.push(format!(
            "{}:{}:{}",
            provider, entry.operation_id, entry.label
        ));
        by_operation
            .entry((provider, entry.operation_id.clone()))
            .or_default()
            .insert(entry.label.clone());
    }
    let duplicates = by_operation
        .values()
        .filter(|labels| labels.len() > 1)
        .count();
    observed.sort();
    observed.dedup();
    evidence_gate(
        "active-active-distinct-push-operations",
        !observed.is_empty() && duplicates == 0,
        "successful active-active push milestones used distinct coordinator operation IDs",
        "rerun the active-active smoke from a clean worktree so each distinct push uses its own operation ID while retries reuse only their original push ID",
        observed,
    )
}

fn matching_exact_labels(labels: &[String], required: &[&str]) -> Vec<String> {
    required
        .iter()
        .filter(|required| labels.iter().any(|label| label == **required))
        .map(|label| (*label).to_owned())
        .collect()
}

fn matching_exact_smoke_labels(labels: &[String], required: &[&str]) -> Vec<String> {
    required
        .iter()
        .filter(|required| labels.iter().any(|label| label == **required))
        .map(|label| (*label).to_owned())
        .collect()
}

fn matching_provider_sequences(
    labels: &[ProviderEvidenceLabel],
    providers: &[&str],
    required: &[&str],
) -> Vec<String> {
    let mut observed = Vec::new();
    for provider in providers {
        let provider_labels = labels
            .iter()
            .filter(|label| label.provider == *provider)
            .map(|label| label.label.clone())
            .collect::<Vec<_>>();
        observed.extend(
            matching_ordered_labels(&provider_labels, required)
                .into_iter()
                .map(|label| format!("{provider}:{label}")),
        );
    }
    observed
}

fn matching_active_active_semantic_sequence(labels: &[&str]) -> Vec<String> {
    let classes = labels
        .iter()
        .filter_map(|label| active_active_label_class(label).map(|class| (class, *label)))
        .collect::<Vec<_>>();
    let mut observed = Vec::new();
    let mut required = ACTIVE_ACTIVE_SMOKE_SEMANTIC_SEQUENCE.iter();
    let Some(mut next) = required.next() else {
        return observed;
    };
    for (class, label) in classes {
        if class == *next {
            observed.push(format!("{class}:{label}"));
            let Some(candidate) = required.next() else {
                break;
            };
            next = candidate;
        }
    }
    observed
}

fn active_active_label_class(label: &str) -> Option<&'static str> {
    match label {
        "mode-active-active" => Some("mode-active-active"),
        "initial-failover-status" => Some("initial-failover-status"),
        "repair-service-template" => Some("repair-service-template"),
        "repair-worker-deployment" => Some("repair-worker-deployment"),
        "repair-snapshot" => Some("repair-snapshot"),
        "failover-fence" => Some("failover-fence"),
        "writes-fenced" => Some("writes-fenced"),
        "failover-resume" => Some("failover-resume"),
        "writes-enabled" => Some("writes-enabled"),
        "active-active-certification" => Some("active-active-certification"),
        label if label.starts_with("push-rejected-fenced-") => Some("push-rejected-fenced"),
        label if label.starts_with("push-rejected-") => Some("push-rejected-stale"),
        label if label.starts_with("push-") => Some("push-success"),
        label if label.starts_with("clone-") => Some("clone"),
        label if label.starts_with("hydrate-") => Some("hydrate"),
        _ => None,
    }
}

fn matching_ordered_labels(labels: &[String], required: &[&str]) -> Vec<String> {
    let mut observed = Vec::new();
    let mut required = required.iter();
    let Some(mut next) = required.next() else {
        return observed;
    };
    for label in labels {
        if label == next {
            observed.push((*next).to_owned());
            let Some(candidate) = required.next() else {
                break;
            };
            next = candidate;
        }
    }
    observed
}

fn evidence_gate(
    code: &str,
    passed: bool,
    message: &str,
    remediation: &str,
    labels: Vec<String>,
) -> EvidenceVerifyGate {
    EvidenceVerifyGate {
        code: code.to_owned(),
        state: if passed {
            CertificationGateState::Passed
        } else {
            CertificationGateState::Failed
        },
        message: message.to_owned(),
        labels,
        remediation: (!passed).then(|| remediation.to_owned()),
    }
}

fn diagnostics_payload_from_doctor(
    doctor: DoctorPayload,
    fix_plan_included: bool,
) -> DiagnosticsPayload {
    DiagnosticsPayload {
        collected_at_ms: now_unix_ms(),
        deep: doctor.deep,
        fix_plan_included,
        redacted: false,
        published: None,
        status: StatusPayload {
            primary: doctor.primary,
            replicas: doctor.replicas,
            health: doctor.health,
            backfill: doctor.backfill,
            control_plane: doctor.control_plane,
        },
        coordinator: doctor.coordinator,
        coordinator_health: doctor.coordinator_health,
        active_active: doctor.active_active,
        findings: doctor.findings,
        fix_plan: doctor.fix_plan,
    }
}

impl From<EvidenceVerifyPayload> for CertificationEvidencePayload {
    fn from(payload: EvidenceVerifyPayload) -> Self {
        Self {
            directory: payload.directory,
            verified: payload.verified,
            require_redacted: payload.require_redacted,
            profile: payload.profile,
            summary: payload.summary,
            gates: payload.gates,
        }
    }
}

fn certification_payload_from_doctor(
    doctor: DoctorPayload,
    profile: CertificationProfileArg,
    evidence: Option<CertificationEvidencePayload>,
) -> CertificationPayload {
    let gates = certification_gates(&doctor, profile, evidence.as_ref());
    let certified = gates
        .iter()
        .all(|gate| gate.state == CertificationGateState::Passed);
    CertificationPayload {
        collected_at_ms: now_unix_ms(),
        profile,
        certified,
        deep: doctor.deep,
        redacted: false,
        status: StatusPayload {
            primary: doctor.primary,
            replicas: doctor.replicas,
            health: doctor.health,
            backfill: doctor.backfill,
            control_plane: doctor.control_plane,
        },
        coordinator: doctor.coordinator,
        coordinator_health: doctor.coordinator_health,
        active_active: doctor.active_active,
        evidence,
        gates,
        findings: doctor.findings,
        fix_plan: doctor.fix_plan,
    }
}

fn certification_gates(
    doctor: &DoctorPayload,
    profile: CertificationProfileArg,
    evidence: Option<&CertificationEvidencePayload>,
) -> Vec<CertificationGate> {
    let mut gates = vec![
        certification_primary_gate(doctor),
        certification_deep_gate(doctor),
    ];
    if profile != CertificationProfileArg::ActiveActive {
        gates.extend([
            certification_replica_inventory_gate(doctor),
            certification_replica_readiness_gate(doctor),
            certification_read_enable_gate(doctor),
            certification_provider_gate(doctor),
            certification_backfill_gate(doctor),
        ]);
    }
    gates.push(certification_active_active_gate(doctor, profile));
    gates.push(certification_coordinator_state_gate(doctor, profile));
    if let Some(gate) = certification_evidence_gate(profile, evidence) {
        gates.push(gate);
    }
    gates.push(certification_findings_gate(doctor, profile));
    gates
}

fn evidence_profile_for_certification(profile: CertificationProfileArg) -> EvidenceVerifyProfile {
    match profile {
        CertificationProfileArg::Enterprise => EvidenceVerifyProfile::Enterprise,
        CertificationProfileArg::ReadReplica => EvidenceVerifyProfile::ProviderHydrate,
        CertificationProfileArg::ActiveActive => EvidenceVerifyProfile::ActiveActiveSmoke,
    }
}

fn certification_primary_gate(doctor: &DoctorPayload) -> CertificationGate {
    certification_gate(
        "certification.primary",
        doctor.primary.is_some(),
        if doctor.primary.is_some() {
            "primary remote is configured".to_owned()
        } else {
            "primary remote is not configured".to_owned()
        },
        Some("configure [remote].url before certifying enterprise replication".to_owned()),
    )
}

fn certification_deep_gate(doctor: &DoctorPayload) -> CertificationGate {
    let cached = doctor
        .replicas
        .iter()
        .filter(|replica| replica.readiness_cache_hit)
        .map(|replica| replica.name.clone())
        .collect::<Vec<_>>();
    let passed = doctor.deep && cached.is_empty();
    let message = if !doctor.deep {
        "certification did not use deep readiness checks".to_owned()
    } else if cached.is_empty() {
        "certification used live readiness checks without cached replica proof".to_owned()
    } else {
        format!(
            "deep readiness still used cached proof for replica(s): {}",
            cached.join(", ")
        )
    };
    certification_gate(
        "certification.deep-proof",
        passed,
        message,
        Some(
            "rerun crab replica certify after forcing live readiness checks to refresh".to_owned(),
        ),
    )
}

fn certification_replica_inventory_gate(doctor: &DoctorPayload) -> CertificationGate {
    certification_gate(
        "certification.replica-inventory",
        !doctor.replicas.is_empty(),
        if doctor.replicas.is_empty() {
            "no read replicas are configured".to_owned()
        } else {
            format!("{} read replica(s) are configured", doctor.replicas.len())
        },
        Some("add and verify at least one regional read replica".to_owned()),
    )
}

fn certification_replica_readiness_gate(doctor: &DoctorPayload) -> CertificationGate {
    let blockers = doctor
        .replicas
        .iter()
        .filter_map(|replica| {
            if !replica.ready {
                return Some(format!(
                    "{} not ready ({})",
                    replica.name,
                    replica
                        .last_fallback_reason
                        .as_deref()
                        .unwrap_or("manifest or referenced objects are missing")
                ));
            }
            if replica.lag_generations.is_some_and(|lag| lag > 0) {
                return Some(format!(
                    "{} lagging by {} generation(s)",
                    replica.name,
                    replica.lag_generations.unwrap_or_default()
                ));
            }
            None
        })
        .collect::<Vec<_>>();
    certification_gate(
        "certification.replica-readiness",
        !doctor.replicas.is_empty() && blockers.is_empty(),
        if doctor.replicas.is_empty() {
            "no replicas are configured for readiness certification".to_owned()
        } else if blockers.is_empty() {
            "all configured replicas are at the primary generation and object-ready".to_owned()
        } else {
            format!("replica readiness blockers: {}", blockers.join("; "))
        },
        Some("run crab replica verify --deep for each blocked replica".to_owned()),
    )
}

fn certification_read_enable_gate(doctor: &DoctorPayload) -> CertificationGate {
    let disabled = doctor
        .replicas
        .iter()
        .filter(|replica| !replica.read_enabled)
        .map(|replica| replica.name.clone())
        .collect::<Vec<_>>();
    certification_gate(
        "certification.read-routing",
        !doctor.replicas.is_empty() && disabled.is_empty(),
        if doctor.replicas.is_empty() {
            "no replicas are configured for read-routing certification".to_owned()
        } else if disabled.is_empty() {
            "all configured replicas are enabled for read routing".to_owned()
        } else {
            format!(
                "read routing is disabled for replica(s): {}",
                disabled.join(", ")
            )
        },
        Some(
            "run crab replica wait <name> --enable-read after readiness and backfill pass"
                .to_owned(),
        ),
    )
}

fn certification_provider_gate(doctor: &DoctorPayload) -> CertificationGate {
    let mut blockers = Vec::new();
    for replica in &doctor.replicas {
        if !doctor
            .control_plane
            .iter()
            .any(|status| status.replica_name == replica.name)
        {
            blockers.push(format!("{} provider status missing", replica.name));
        }
    }
    for status in &doctor.control_plane {
        if !status.backend_available {
            blockers.push(format!(
                "{} {} backend unavailable",
                status.replica_name, status.provider
            ));
        }
        if !status.checked_drift {
            blockers.push(format!("{} drift not checked", status.replica_name));
        }
        if status.checks.is_empty() {
            blockers.push(format!(
                "{} provider status reported no drift checks",
                status.replica_name
            ));
        }
        for check in &status.checks {
            if check.state != ControlPlaneCheckState::Verified {
                blockers.push(format!(
                    "{} {} is {}",
                    status.replica_name,
                    check.code,
                    check.state.as_str()
                ));
            }
        }
    }
    certification_gate(
        "certification.provider-control-plane",
        !doctor.replicas.is_empty() && blockers.is_empty(),
        if doctor.replicas.is_empty() {
            "no replicas are configured for provider control-plane certification".to_owned()
        } else if blockers.is_empty() {
            "provider control-plane drift checks are verified for every replica".to_owned()
        } else {
            format!("provider control-plane blockers: {}", blockers.join("; "))
        },
        Some("run crab replica doctor --deep --fix-plan and apply only Crab-managed provider resources".to_owned()),
    )
}

fn certification_backfill_gate(doctor: &DoctorPayload) -> CertificationGate {
    let mut blockers = Vec::new();
    if !doctor.replicas.is_empty() && doctor.backfill.len() < doctor.replicas.len() {
        blockers.push("backfill status is missing for one or more replicas".to_owned());
    }
    for backfill in &doctor.backfill {
        if backfill.blocks_read_enable
            || !matches!(
                backfill.state,
                BackfillState::NotRequired | BackfillState::Verified
            )
        {
            blockers.push(format!(
                "{} backfill is {}",
                backfill.name,
                backfill.state.as_str()
            ));
        }
    }
    certification_gate(
        "certification.backfill",
        !doctor.replicas.is_empty() && blockers.is_empty(),
        if doctor.replicas.is_empty() {
            "no replicas are configured for backfill certification".to_owned()
        } else if blockers.is_empty() {
            "historical-object backfill is verified or not required".to_owned()
        } else {
            format!("backfill blockers: {}", blockers.join("; "))
        },
        Some(
            "run crab replica backfill status --name <name> --json before enabling reads"
                .to_owned(),
        ),
    )
}

fn certification_active_active_gate(
    doctor: &DoctorPayload,
    profile: CertificationProfileArg,
) -> CertificationGate {
    if doctor.active_active.mode != ReplicationMode::ActiveActive {
        return certification_gate(
            "certification.active-active",
            profile != CertificationProfileArg::ActiveActive,
            if profile == CertificationProfileArg::ActiveActive {
                "replication is not configured for active-active writes".to_owned()
            } else {
                "replication is in read-replica mode; active-active write admission is not required"
                    .to_owned()
            },
            Some(
                "run crab replica mode active-active with a managed coordinator and enabled writers"
                    .to_owned(),
            ),
        );
    }

    if profile == CertificationProfileArg::ReadReplica {
        return certification_gate(
            "certification.active-active",
            true,
            "read-replica certification does not require active-active write admission".to_owned(),
            None,
        );
    }

    let mut blockers = Vec::new();
    if !doctor.active_active.writes_enabled {
        blockers.push(
            doctor
                .active_active
                .reason
                .clone()
                .unwrap_or_else(|| "active-active writes are not currently admitted".to_owned()),
        );
    }
    match doctor.coordinator.as_ref() {
        Some(coordinator) => {
            if !coordinator.backend_available {
                blockers.push("coordinator status backend is unavailable".to_owned());
            }
            if !coordinator.checked_drift {
                blockers.push("coordinator drift was not checked".to_owned());
            }
            if coordinator.checks.is_empty() {
                blockers.push("coordinator status reported no drift checks".to_owned());
            }
            for check in &coordinator.checks {
                if check.state != CoordinatorCheckState::Verified {
                    blockers.push(format!("{} is {}", check.code, check.state.as_str()));
                }
            }
        }
        None => blockers.push("coordinator status is unavailable".to_owned()),
    }

    certification_gate(
        "certification.active-active",
        blockers.is_empty(),
        if blockers.is_empty() {
            "active-active writer admission and coordinator drift checks are verified".to_owned()
        } else {
            format!("active-active blockers: {}", blockers.join("; "))
        },
        Some("run crab replica failover status --json and resolve coordinator blockers".to_owned()),
    )
}

fn certification_coordinator_state_gate(
    doctor: &DoctorPayload,
    profile: CertificationProfileArg,
) -> CertificationGate {
    if profile == CertificationProfileArg::ReadReplica
        || doctor.active_active.mode != ReplicationMode::ActiveActive
    {
        return certification_gate(
            "certification.coordinator-state",
            true,
            "coordinator state pressure is not required for read-replica certification".to_owned(),
            None,
        );
    }

    let Some(health) = doctor.coordinator_health.as_ref() else {
        return certification_gate(
            "certification.coordinator-state",
            false,
            "coordinator data-plane health is unavailable".to_owned(),
            Some(
                "run crab replica failover status --json and resolve coordinator health blockers"
                    .to_owned(),
            ),
        );
    };
    let Some(summary) = health.state_summary.as_ref() else {
        return certification_gate(
            "certification.coordinator-state",
            false,
            "coordinator data-plane health did not report state pressure".to_owned(),
            Some("upgrade or enable the managed coordinator data-plane health adapter".to_owned()),
        );
    };
    if let Some(max_state_bytes) = summary.max_state_bytes
        && usage_at_or_above(
            summary.state_bytes,
            max_state_bytes,
            COORDINATOR_STATE_CRITICAL_PERCENT,
        )
    {
        return certification_gate(
            "certification.coordinator-state",
            false,
            format!(
                "coordinator state is at critical pressure: {} of {} bytes ({}%)",
                summary.state_bytes,
                max_state_bytes,
                usage_percent(summary.state_bytes, max_state_bytes)
            ),
            Some(
                "clear pending coordinator transactions and rerun coordinator-backed repair before certification"
                    .to_owned(),
            ),
        );
    }

    certification_gate(
        "certification.coordinator-state",
        true,
        "coordinator state pressure is below the critical certification threshold".to_owned(),
        None,
    )
}

fn certification_evidence_gate(
    profile: CertificationProfileArg,
    evidence: Option<&CertificationEvidencePayload>,
) -> Option<CertificationGate> {
    if profile != CertificationProfileArg::Enterprise && evidence.is_none() {
        return None;
    }

    let required_profile = evidence_profile_for_certification(profile);
    let Some(evidence) = evidence else {
        return Some(certification_gate(
            "certification.retained-evidence",
            false,
            "retained redacted enterprise live evidence was not provided".to_owned(),
            Some(
                "run crab replica evidence verify <dir> --profile enterprise --require-redacted, then rerun crab replica certify --profile enterprise --evidence-dir <dir>"
                    .to_owned(),
            ),
        ));
    };

    let passed =
        evidence.verified && evidence.require_redacted && evidence.profile == required_profile;
    let message = if evidence.profile != required_profile {
        format!(
            "retained evidence profile {} does not match required certification profile {}",
            evidence.profile.as_str(),
            required_profile.as_str()
        )
    } else if !evidence.require_redacted {
        "retained evidence was not verified with redaction required".to_owned()
    } else if evidence.verified {
        format!(
            "retained redacted {} live evidence verified",
            required_profile.as_str()
        )
    } else {
        format!(
            "retained {} live evidence failed verification",
            required_profile.as_str()
        )
    };

    Some(certification_gate(
        "certification.retained-evidence",
        passed,
        message,
        Some(format!(
            "run crab replica evidence verify <dir> --profile {} --require-redacted and pass the verified directory with --evidence-dir <dir>",
            required_profile.as_str()
        )),
    ))
}

fn certification_findings_gate(
    doctor: &DoctorPayload,
    profile: CertificationProfileArg,
) -> CertificationGate {
    let blockers = doctor
        .findings
        .iter()
        .filter(|finding| !certification_ignores_finding(profile, finding))
        .filter(|finding| finding.severity != DoctorSeverity::Info)
        .map(|finding| finding.code.clone())
        .collect::<Vec<_>>();
    certification_gate(
        "certification.doctor-findings",
        blockers.is_empty(),
        if blockers.is_empty() {
            "doctor reported no warning or error findings".to_owned()
        } else {
            format!(
                "doctor warning/error findings remain: {}",
                blockers.join(", ")
            )
        },
        Some(
            "resolve every warning and error from crab replica doctor --deep --fix-plan".to_owned(),
        ),
    )
}

fn certification_ignores_finding(
    profile: CertificationProfileArg,
    finding: &DoctorFinding,
) -> bool {
    if matches!(profile, CertificationProfileArg::ActiveActive)
        && matches!(finding.code.as_str(), "replica.none_configured")
    {
        return true;
    }
    matches!(
        finding.code.as_str(),
        "coordinator.state_size_high" | "coordinator.completed_operations_high"
    )
}

fn certification_gate(
    code: &str,
    passed: bool,
    message: String,
    remediation: Option<String>,
) -> CertificationGate {
    CertificationGate {
        code: code.to_owned(),
        state: if passed {
            CertificationGateState::Passed
        } else {
            CertificationGateState::Failed
        },
        message,
        remediation: if passed { None } else { remediation },
    }
}

fn redact_diagnostics_payload(payload: &mut DiagnosticsPayload) {
    let redactor = DiagnosticsRedactor::from_diagnostics_payload(payload);
    payload.redacted = true;

    redact_status_payload(&redactor, &mut payload.status);
    redact_diagnostics_publication(&redactor, payload.published.as_mut());
    redact_coordinator_payload(&redactor, payload.coordinator.as_mut());
    redact_coordinator_health_payload(&redactor, payload.coordinator_health.as_mut());
    redact_active_active_payload(&redactor, &mut payload.active_active);
    redact_doctor_findings(&redactor, &mut payload.findings);
    redact_doctor_fix_plan(&redactor, &mut payload.fix_plan);
}

fn redact_certification_payload(payload: &mut CertificationPayload) {
    let redactor = DiagnosticsRedactor::from_certification_payload(payload);
    payload.redacted = true;

    redact_status_payload(&redactor, &mut payload.status);
    redact_coordinator_payload(&redactor, payload.coordinator.as_mut());
    redact_coordinator_health_payload(&redactor, payload.coordinator_health.as_mut());
    redact_active_active_payload(&redactor, &mut payload.active_active);
    redact_certification_evidence(&redactor, payload.evidence.as_mut());
    redact_doctor_findings(&redactor, &mut payload.findings);
    redact_doctor_fix_plan(&redactor, &mut payload.fix_plan);
}

fn redact_status_payload(redactor: &DiagnosticsRedactor, status: &mut StatusPayload) {
    redactor.redact_optional(&mut status.primary);
    for replica in &mut status.replicas {
        redactor.redact(&mut replica.url);
        redactor.redact_optional(&mut replica.last_fallback_reason);
    }
    for health in &mut status.health {
        redactor.redact(&mut health.reason);
    }
    for backfill in &mut status.backfill {
        redactor.redact(&mut backfill.url);
        redactor.redact(&mut backfill.message);
        redactor.redact_optional(&mut backfill.remediation);
    }
    for control_plane in &mut status.control_plane {
        redactor.redact(&mut control_plane.primary);
        redactor.redact(&mut control_plane.replica);
        for check in &mut control_plane.checks {
            redactor.redact(&mut check.target);
            redactor.redact(&mut check.managed_resource_id);
            redactor.redact(&mut check.message);
            redactor.redact(&mut check.remediation);
        }
    }
}

fn redact_diagnostics_publication(
    redactor: &DiagnosticsRedactor,
    publication: Option<&mut DiagnosticsPublicationPayload>,
) {
    if let Some(publication) = publication {
        redactor.redact(&mut publication.primary);
        redactor.redact(&mut publication.object_key);
        publication.redacted = true;
    }
}

fn redact_coordinator_payload(
    redactor: &DiagnosticsRedactor,
    coordinator: Option<&mut CoordinatorControlPlaneStatus>,
) {
    if let Some(coordinator) = coordinator {
        redactor.redact(&mut coordinator.name);
        redactor.redact(&mut coordinator.url);
        for region in &mut coordinator.failover_regions {
            redactor.redact(region);
        }
        for check in &mut coordinator.checks {
            redactor.redact(&mut check.target);
            redactor.redact(&mut check.managed_resource_id);
            redactor.redact(&mut check.message);
            redactor.redact(&mut check.remediation);
        }
    }
}

fn redact_coordinator_health_payload(
    redactor: &DiagnosticsRedactor,
    health: Option<&mut CoordinatorHealth>,
) {
    if let Some(health) = health {
        redactor.redact_optional(&mut health.reason);
    }
}

fn redact_active_active_payload(redactor: &DiagnosticsRedactor, status: &mut ActiveActiveStatus) {
    redactor.redact_optional(&mut status.reason);
}

fn redact_certification_evidence(
    redactor: &DiagnosticsRedactor,
    evidence: Option<&mut CertificationEvidencePayload>,
) {
    if let Some(evidence) = evidence {
        redactor.redact(&mut evidence.directory);
    }
}

fn redact_doctor_findings(redactor: &DiagnosticsRedactor, findings: &mut [DoctorFinding]) {
    for finding in findings {
        redactor.redact(&mut finding.message);
        redactor.redact_optional(&mut finding.remediation);
    }
}

fn redact_doctor_fix_plan(redactor: &DiagnosticsRedactor, fix_plan: &mut [DoctorFixAction]) {
    for action in fix_plan {
        redactor.redact(&mut action.description);
        redactor.redact_optional(&mut action.command);
    }
}

struct DiagnosticsRedactor {
    sensitive_values: Vec<String>,
}

impl DiagnosticsRedactor {
    fn from_diagnostics_payload(payload: &DiagnosticsPayload) -> Self {
        let mut values = Vec::new();
        collect_status_sensitive(&mut values, &payload.status);
        collect_diagnostics_publication_sensitive(&mut values, payload.published.as_ref());
        collect_coordinator_sensitive(&mut values, payload.coordinator.as_ref());
        Self::from_sensitive_values(values)
    }

    fn from_certification_payload(payload: &CertificationPayload) -> Self {
        let mut values = Vec::new();
        collect_status_sensitive(&mut values, &payload.status);
        collect_coordinator_sensitive(&mut values, payload.coordinator.as_ref());
        collect_certification_evidence_sensitive(&mut values, payload.evidence.as_ref());
        Self::from_sensitive_values(values)
    }

    fn from_sensitive_values(mut values: Vec<String>) -> Self {
        values.sort_by_key(|value| std::cmp::Reverse(value.len()));
        values.dedup();
        Self {
            sensitive_values: values,
        }
    }

    fn redact_optional(&self, value: &mut Option<String>) {
        if let Some(value) = value.as_mut() {
            self.redact(value);
        }
    }

    fn redact(&self, value: &mut String) {
        for sensitive in &self.sensitive_values {
            if value == sensitive {
                value.clear();
                value.push_str("<redacted>");
                continue;
            }
            *value = value.replace(sensitive, "<redacted>");
        }
    }
}

fn collect_status_sensitive(values: &mut Vec<String>, status: &StatusPayload) {
    collect_optional_sensitive(values, status.primary.as_deref());
    for replica in &status.replicas {
        collect_sensitive(values, &replica.url);
    }
    for backfill in &status.backfill {
        collect_sensitive(values, &backfill.url);
    }
    for control_plane in &status.control_plane {
        collect_sensitive(values, &control_plane.primary);
        collect_sensitive(values, &control_plane.replica);
        for check in &control_plane.checks {
            collect_sensitive(values, &check.target);
            collect_sensitive(values, &check.managed_resource_id);
        }
    }
}

fn collect_diagnostics_publication_sensitive(
    values: &mut Vec<String>,
    publication: Option<&DiagnosticsPublicationPayload>,
) {
    if let Some(publication) = publication {
        collect_sensitive(values, &publication.primary);
        collect_sensitive(values, &publication.object_key);
    }
}

fn collect_coordinator_sensitive(
    values: &mut Vec<String>,
    coordinator: Option<&CoordinatorControlPlaneStatus>,
) {
    if let Some(coordinator) = coordinator {
        collect_sensitive(values, &coordinator.name);
        collect_sensitive(values, &coordinator.url);
        for check in &coordinator.checks {
            collect_sensitive(values, &check.target);
            collect_sensitive(values, &check.managed_resource_id);
        }
    }
}

fn collect_certification_evidence_sensitive(
    values: &mut Vec<String>,
    evidence: Option<&CertificationEvidencePayload>,
) {
    if let Some(evidence) = evidence {
        collect_sensitive(values, &evidence.directory);
    }
}

fn collect_optional_sensitive(values: &mut Vec<String>, value: Option<&str>) {
    if let Some(value) = value {
        collect_sensitive(values, value);
    }
}

fn collect_sensitive(values: &mut Vec<String>, value: &str) {
    let trimmed = value.trim();
    if trimmed.len() < 4 {
        return;
    }
    values.push(trimmed.to_owned());
}

pub async fn run_remove(args: &RemoveArgs) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let path = project_config_path(&cwd);
    let apply_status = if args.apply {
        let config = ProjectConfig::load(&path)?;
        let replication = config
            .replication
            .as_ref()
            .ok_or_else(|| CrabError::Configuration {
                key: "replication".into(),
                origin: "replication is not configured".into(),
            })?;
        let primary = replication
            .primary
            .as_deref()
            .unwrap_or(config.remote.url.as_str());
        let replica = replication
            .replicas
            .iter()
            .find(|replica| replica.name == args.name)
            .ok_or_else(|| CrabError::Configuration {
                key: "replication.replicas".into(),
                origin: format!("replica {} is not configured", args.name),
            })?;
        let plan = control_plane_remove_plan(replica, primary);
        Some(remove_control_plane_plan(&plan).await?)
    } else {
        None
    };
    let removed = remove_replica_from_project_config(&path, &args.name)?;
    let payload = RemovePayload {
        removed,
        name: args.name.clone(),
        applied: args.apply,
        apply_status,
    };
    if args.json {
        emit_json(SCHEMA, SCHEMA_VERSION, payload);
    } else if removed {
        println!("Removed replica '{}'", args.name);
    } else {
        println!("Replica '{}' was not configured", args.name);
    }
    Ok(())
}

async fn control_plane_statuses(
    primary: Option<&str>,
    replication: Option<&crate::replication::ReplicationConfig>,
) -> Vec<ControlPlaneStatus> {
    let (Some(primary), Some(replication)) = (primary, replication) else {
        return Vec::new();
    };

    let mut statuses = Vec::new();
    for replica in &replication.replicas {
        let plan = control_plane_plan(
            &replica.name,
            replica.provider,
            primary,
            &replica.url,
            &replica.region,
            replica.rpo,
            replica.backfill,
        );
        statuses.push(inspect_control_plane_plan_default(&plan).await);
    }
    statuses
}

fn sync_readiness_cache_from_control_plane(
    primary: Option<&str>,
    replication: Option<&crate::replication::ReplicationConfig>,
    control_plane: &[ControlPlaneStatus],
) -> Result<()> {
    let (Some(primary), Some(replication)) = (primary, replication) else {
        return Ok(());
    };
    let parsed = CrabUrl::parse(primary)?;
    sync_readiness_cache_control_plane(&parsed.repo_path, replication, control_plane);
    Ok(())
}

fn backfill_cutover_blocker(
    replica: &ReplicaConfig,
    status: Option<&ControlPlaneStatus>,
) -> Option<String> {
    if !replica.backfill {
        return None;
    }
    let Some(status) = status else {
        return Some("backfill status is unavailable".to_owned());
    };
    let Some(check) = status
        .checks
        .iter()
        .find(|check| is_backfill_control_plane_check(check))
    else {
        return Some("backfill status is not tracked for this provider".to_owned());
    };
    if check.state == ControlPlaneCheckState::Verified {
        return None;
    }
    Some(format!(
        "backfill is {}; rerun after provider backfill status is verified",
        check.state.as_str()
    ))
}

fn replica_read_cutover_blocker(
    status: &ReplicaStatus,
    replica: &ReplicaConfig,
    control_plane: &[ControlPlaneStatus],
) -> Option<String> {
    if !status.ready {
        return Some(
            status
                .last_fallback_reason
                .clone()
                .unwrap_or_else(|| "replica is not ready".to_owned()),
        );
    }
    backfill_cutover_blocker(
        replica,
        control_plane
            .iter()
            .find(|status| status.replica_name == replica.name),
    )
}

async fn backfill_payload(
    primary: Option<&str>,
    config: &Config,
    name: Option<&str>,
) -> Result<BackfillPayload> {
    let primary = primary.ok_or_else(|| CrabError::Configuration {
        key: "remote.url".into(),
        origin: "replica backfill status requires a configured primary remote".into(),
    })?;
    let replication = config
        .replication
        .as_ref()
        .ok_or_else(|| CrabError::Configuration {
            key: "replication".into(),
            origin: "replication is not configured".into(),
        })?;
    let control_plane = control_plane_statuses(Some(primary), Some(replication)).await;
    let mut replicas = Vec::new();
    for replica in &replication.replicas {
        if name.is_some_and(|requested| requested != replica.name) {
            continue;
        }
        replicas.push(backfill_replica_status(
            replica,
            control_plane
                .iter()
                .find(|status| status.replica_name == replica.name),
        ));
    }
    if let Some(name) = name
        && replicas.is_empty()
    {
        return Err(CrabError::Configuration {
            key: "replication.replicas".into(),
            origin: format!("replica {name} is not configured"),
        });
    }
    Ok(BackfillPayload {
        primary: primary.to_owned(),
        replicas,
    })
}

fn backfill_replica_status(
    replica: &ReplicaConfig,
    status: Option<&ControlPlaneStatus>,
) -> BackfillReplicaStatus {
    if !replica.backfill {
        return BackfillReplicaStatus {
            name: replica.name.clone(),
            provider: replica.provider,
            url: replica.url.clone(),
            region: replica.region.clone(),
            required: false,
            read_enabled: replica.read,
            state: BackfillState::NotRequired,
            blocks_read_enable: false,
            progress_percent: None,
            check_code: None,
            message: "replica was not added with --backfill".to_owned(),
            remediation: None,
        };
    }

    let Some(status) = status else {
        return BackfillReplicaStatus {
            name: replica.name.clone(),
            provider: replica.provider,
            url: replica.url.clone(),
            region: replica.region.clone(),
            required: true,
            read_enabled: replica.read,
            state: BackfillState::Unavailable,
            blocks_read_enable: true,
            progress_percent: None,
            check_code: None,
            message: "backfill status is unavailable".to_owned(),
            remediation: Some("configure the primary remote and rerun backfill status".to_owned()),
        };
    };

    let Some(check) = status
        .checks
        .iter()
        .find(|check| is_backfill_control_plane_check(check))
    else {
        return BackfillReplicaStatus {
            name: replica.name.clone(),
            provider: replica.provider,
            url: replica.url.clone(),
            region: replica.region.clone(),
            required: true,
            read_enabled: replica.read,
            state: BackfillState::Untracked,
            blocks_read_enable: true,
            progress_percent: None,
            check_code: None,
            message: "backfill status is not tracked for this provider".to_owned(),
            remediation: Some(
                "use a provider-supported historical copy path before enabling reads".to_owned(),
            ),
        };
    };

    let state = match check.state {
        ControlPlaneCheckState::Verified => BackfillState::Verified,
        ControlPlaneCheckState::Missing => BackfillState::Missing,
        ControlPlaneCheckState::Drifted => BackfillState::Drifted,
        ControlPlaneCheckState::Unknown => BackfillState::Unknown,
        ControlPlaneCheckState::Unsupported => BackfillState::Unsupported,
    };

    BackfillReplicaStatus {
        name: replica.name.clone(),
        provider: replica.provider,
        url: replica.url.clone(),
        region: replica.region.clone(),
        required: true,
        read_enabled: replica.read,
        state,
        blocks_read_enable: state != BackfillState::Verified,
        progress_percent: check.progress_percent,
        check_code: Some(check.code.clone()),
        message: check.message.clone(),
        remediation: Some(check.remediation.clone()),
    }
}

fn backfill_statuses(
    replication: Option<&crate::replication::ReplicationConfig>,
    control_plane: &[ControlPlaneStatus],
) -> Vec<BackfillReplicaStatus> {
    let Some(replication) = replication else {
        return Vec::new();
    };
    replication
        .replicas
        .iter()
        .map(|replica| {
            backfill_replica_status(
                replica,
                control_plane
                    .iter()
                    .find(|status| status.replica_name == replica.name),
            )
        })
        .collect()
}

fn is_backfill_control_plane_check(check: &crate::replication::ControlPlaneCheck) -> bool {
    check.code.contains("backfill") || check.code.contains("batch-replication")
}

fn replica_health(
    replicas: &[ReplicaStatus],
    control_plane: &[ControlPlaneStatus],
) -> Vec<ReplicaHealth> {
    replicas
        .iter()
        .map(|replica| {
            let status = control_plane
                .iter()
                .find(|status| status.replica_name == replica.name);
            classify_replica_health(replica, status)
        })
        .collect()
}

fn classify_replica_health(
    replica: &ReplicaStatus,
    status: Option<&ControlPlaneStatus>,
) -> ReplicaHealth {
    let (state, reason) = if !replica.read_enabled {
        (
            ReplicaHealthState::Disabled,
            "replica reads are disabled in local config".to_owned(),
        )
    } else if let Some(reason) = auth_failure_reason(replica) {
        (ReplicaHealthState::AuthFailed, reason)
    } else if let Some(reason) = policy_drift_reason(status) {
        (ReplicaHealthState::PolicyDrift, reason)
    } else if let Some(reason) = backfill_health_reason(replica, status) {
        (ReplicaHealthState::BackfillRunning, reason)
    } else if replica.lag_generations.is_some_and(|lag| lag > 0) {
        (
            ReplicaHealthState::Lagging,
            format!(
                "replica is {} generation(s) behind the primary",
                replica.lag_generations.unwrap_or_default()
            ),
        )
    } else if !replica.ready {
        (
            ReplicaHealthState::Partial,
            replica.last_fallback_reason.clone().unwrap_or_else(|| {
                "replica manifest or referenced objects are not ready".to_owned()
            }),
        )
    } else if let Some(reason) = provider_status_gap_reason(status) {
        (ReplicaHealthState::Partial, reason)
    } else {
        (
            ReplicaHealthState::Ready,
            "replica is read-enabled and manifest-referenced objects are ready".to_owned(),
        )
    };

    ReplicaHealth {
        name: replica.name.clone(),
        provider: replica.provider,
        region: replica.region.clone(),
        state,
        reason,
    }
}

fn auth_failure_reason(replica: &ReplicaStatus) -> Option<String> {
    let reason = replica.last_fallback_reason.as_ref()?;
    let lower = reason.to_ascii_lowercase();
    let auth_like = [
        "auth",
        "unauthorized",
        "forbidden",
        "permission",
        "credential",
        "access denied",
    ];
    if auth_like.iter().any(|needle| lower.contains(needle)) {
        Some(reason.clone())
    } else {
        None
    }
}

fn policy_drift_reason(status: Option<&ControlPlaneStatus>) -> Option<String> {
    let check = status?.checks.iter().find(|check| {
        !is_backfill_control_plane_check(check)
            && matches!(
                check.state,
                ControlPlaneCheckState::Missing | ControlPlaneCheckState::Drifted
            )
    })?;
    Some(format!(
        "provider check {} is {}: {}",
        check.code,
        check.state.as_str(),
        check.message
    ))
}

fn backfill_health_reason(
    replica: &ReplicaStatus,
    status: Option<&ControlPlaneStatus>,
) -> Option<String> {
    if !replica.backfill_required {
        return None;
    }
    let Some(status) = status else {
        return Some("backfill is required but provider status is unavailable".to_owned());
    };
    let Some(check) = status
        .checks
        .iter()
        .find(|check| is_backfill_control_plane_check(check))
    else {
        return Some("backfill is required but no provider backfill check was reported".to_owned());
    };
    if check.state == ControlPlaneCheckState::Verified {
        None
    } else {
        Some(format!(
            "backfill check {} is {}: {}",
            check.code,
            check.state.as_str(),
            check.message
        ))
    }
}

fn provider_status_gap_reason(status: Option<&ControlPlaneStatus>) -> Option<String> {
    let Some(status) = status else {
        return Some("provider control-plane status is unavailable".to_owned());
    };
    if !status.backend_available {
        return Some("provider control-plane status backend is unavailable".to_owned());
    }
    if !status.checked_drift {
        return Some("provider control-plane drift was not checked".to_owned());
    }
    let check = status.checks.iter().find(|check| {
        !is_backfill_control_plane_check(check)
            && matches!(
                check.state,
                ControlPlaneCheckState::Unknown | ControlPlaneCheckState::Unsupported
            )
    })?;
    Some(format!(
        "provider check {} is {}: {}",
        check.code,
        check.state.as_str(),
        check.message
    ))
}

fn coordinator_plan_from_args_or_config(
    provider: Option<CoordinatorProviderArg>,
    name: Option<&str>,
    region: Option<&str>,
    failover_regions: &[String],
    config: Option<&ProjectConfig>,
) -> Result<(CoordinatorControlPlanePlan, bool)> {
    if provider.is_some() || name.is_some() || region.is_some() || !failover_regions.is_empty() {
        let provider = provider.ok_or_else(|| CrabError::Configuration {
            key: "replication.coordinator.provider".into(),
            origin: "explicit coordinator target requires --provider".into(),
        })?;
        let name = name.ok_or_else(|| CrabError::Configuration {
            key: "replication.coordinator.name".into(),
            origin: "explicit coordinator target requires --name".into(),
        })?;
        let region = region.ok_or_else(|| CrabError::Configuration {
            key: "replication.coordinator.region".into(),
            origin: "explicit coordinator target requires --region".into(),
        })?;
        return Ok((
            managed_coordinator_plan(provider.into(), name, region, failover_regions),
            false,
        ));
    }

    let replication = config.and_then(|config| config.replication.as_ref());
    configured_coordinator_plan(replication).map(|plan| (plan, true))
}

fn configured_coordinator_plan(
    replication: Option<&crate::replication::ReplicationConfig>,
) -> Result<CoordinatorControlPlanePlan> {
    let replication = replication.ok_or_else(|| CrabError::Configuration {
        key: "replication".into(),
        origin: "replication is not configured; pass --provider, --name, and --region".into(),
    })?;
    let coordinator = replication
        .coordinator
        .as_ref()
        .ok_or_else(|| CrabError::Configuration {
            key: "replication.coordinator".into(),
            origin: "active-active coordinator is not configured".into(),
        })?;
    let (provider, name) =
        parse_coordinator_url(&coordinator.url).ok_or_else(|| CrabError::Configuration {
            key: "replication.coordinator.url".into(),
            origin: format!("unsupported coordinator URL {}", coordinator.url),
        })?;
    Ok(managed_coordinator_plan(
        provider,
        &name,
        &coordinator.region,
        &coordinator.failover_regions,
    ))
}

fn managed_coordinator_plan(
    provider: ManagedCoordinatorProvider,
    name: &str,
    region: &str,
    failover_regions: &[String],
) -> CoordinatorControlPlanePlan {
    match provider {
        ManagedCoordinatorProvider::DynamoDb => {
            dynamodb_coordinator_plan(name, region, failover_regions)
        }
        ManagedCoordinatorProvider::Spanner => {
            spanner_coordinator_plan(name, region, failover_regions)
        }
        ManagedCoordinatorProvider::CosmosDb => {
            cosmosdb_coordinator_plan(name, region, failover_regions)
        }
    }
}

fn remove_coordinator_from_project_config(path: &Path) -> Result<bool> {
    let mut config = ProjectConfig::load(path)?;
    let Some(replication) = config.replication.as_mut() else {
        return Ok(false);
    };
    let changed = replication.coordinator.is_some()
        || !replication.writers.is_empty()
        || replication.mode != ReplicationMode::ReadReplica;
    if !changed {
        return Ok(false);
    }
    replication.coordinator = None;
    replication.writers.clear();
    replication.mode = ReplicationMode::ReadReplica;
    ProjectConfig::write(path, &config)?;
    Ok(true)
}

struct CoordinatorControlPlaneProbe {
    status: Option<CoordinatorControlPlaneStatus>,
    error: Option<String>,
}

impl CoordinatorControlPlaneProbe {
    fn unavailable() -> Self {
        Self {
            status: None,
            error: None,
        }
    }
}

async fn coordinator_control_plane_probe(
    replication: Option<&crate::replication::ReplicationConfig>,
) -> CoordinatorControlPlaneProbe {
    let backends = DefaultCoordinatorBackends::new();
    coordinator_control_plane_probe_with_backends(replication, &backends).await
}

async fn coordinator_control_plane_probe_with_backends(
    replication: Option<&crate::replication::ReplicationConfig>,
    backends: &dyn CoordinatorBackendResolver,
) -> CoordinatorControlPlaneProbe {
    let Some(replication) = replication.filter(|replication| replication.is_active_active()) else {
        return CoordinatorControlPlaneProbe::unavailable();
    };
    let plan = match configured_coordinator_plan(Some(replication)) {
        Ok(plan) => plan,
        Err(err) => {
            return CoordinatorControlPlaneProbe {
                status: None,
                error: Some(err.to_string()),
            };
        }
    };
    match inspect_coordinator_plan_with_backends(&plan, backends).await {
        Ok(status) => CoordinatorControlPlaneProbe {
            status: Some(status),
            error: None,
        },
        Err(err) => CoordinatorControlPlaneProbe {
            status: None,
            error: Some(err.to_string()),
        },
    }
}

#[cfg(test)]
async fn coordinator_control_plane_status_with_backends(
    replication: Option<&crate::replication::ReplicationConfig>,
    backends: &dyn CoordinatorBackendResolver,
) -> Option<CoordinatorControlPlaneStatus> {
    coordinator_control_plane_probe_with_backends(replication, backends)
        .await
        .status
}

fn apply_coordinator_probe_error_to_active_active_status(
    status: &mut ActiveActiveStatus,
    error: Option<&str>,
) {
    if let Some(error) = error
        && status.coordinator_configured
    {
        status.coordinator_ready = false;
        status.writes_enabled = false;
        status.reason = Some(format!("managed coordinator status probe failed: {error}"));
    }
}

fn parse_coordinator_url(url: &str) -> Option<(ManagedCoordinatorProvider, String)> {
    let (scheme, rest) = url.split_once("://")?;
    let provider = match scheme {
        "dynamodb" => ManagedCoordinatorProvider::DynamoDb,
        "spanner" => ManagedCoordinatorProvider::Spanner,
        "cosmosdb" => ManagedCoordinatorProvider::CosmosDb,
        _ => return None,
    };
    let name = rest.split('/').next().unwrap_or(rest).trim();
    if name.is_empty() {
        return None;
    }
    Some((provider, name.to_owned()))
}

fn doctor_findings(
    primary: Option<&str>,
    replication: Option<&crate::replication::ReplicationConfig>,
    replicas: &[ReplicaStatus],
    control_plane: &[ControlPlaneStatus],
    coordinator: Option<&CoordinatorControlPlaneStatus>,
    coordinator_error: Option<&str>,
    coordinator_health: Option<&CoordinatorHealth>,
    active_active: &ActiveActiveStatus,
    deep: bool,
) -> Vec<DoctorFinding> {
    let mut findings = Vec::new();
    if primary.is_none() {
        findings.push(finding(
            "replication.primary_missing",
            DoctorSeverity::Error,
            "no primary remote is configured",
            None,
            Some("run crab init <url> or configure [remote].url".to_owned()),
        ));
    }

    let Some(replication) = replication else {
        findings.push(finding(
            "replication.not_configured",
            DoctorSeverity::Error,
            "replication is not configured",
            None,
            Some("run crab replica add before enabling replica reads".to_owned()),
        ));
        return findings;
    };

    if replication.replicas.is_empty() {
        findings.push(finding(
            "replica.none_configured",
            DoctorSeverity::Warning,
            "no read replicas are configured",
            None,
            Some("run crab replica add <name> --provider <provider>".to_owned()),
        ));
    }

    for replica in &replication.replicas {
        if !replica.read {
            findings.push(finding(
                "replica.read_disabled",
                DoctorSeverity::Warning,
                format!("replica {} is configured but read-disabled", replica.name),
                Some(replica.name.clone()),
                Some(
                    "run crab replica wait <name> --enable-read after readiness passes".to_owned(),
                ),
            ));
        }
        if let Some(reason) = backfill_cutover_blocker(
            replica,
            control_plane
                .iter()
                .find(|status| status.replica_name == replica.name),
        ) {
            findings.push(finding(
                "replica.backfill_unverified",
                if replica.read {
                    DoctorSeverity::Error
                } else {
                    DoctorSeverity::Warning
                },
                format!("replica {} backfill is not verified", replica.name),
                Some(replica.name.clone()),
                Some(reason),
            ));
        }
    }

    for status in control_plane {
        if !status.backend_available {
            findings.push(finding(
                "provider.control_plane_unavailable",
                DoctorSeverity::Warning,
                provider_control_plane_unavailable_message(status),
                Some(status.replica_name.clone()),
                Some(
                    "fix provider credentials, permissions, topology, or backend feature support, then rerun crab replica doctor --deep"
                        .to_owned(),
                ),
            ));
        }
        for check in &status.checks {
            if matches!(
                check.state,
                ControlPlaneCheckState::Missing | ControlPlaneCheckState::Drifted
            ) {
                findings.push(finding(
                    &check.code,
                    DoctorSeverity::Error,
                    check.message.clone(),
                    Some(status.replica_name.clone()),
                    Some(check.remediation.clone()),
                ));
            }
        }
    }

    if let Some(status) = coordinator {
        if !status.backend_available {
            findings.push(finding(
                "coordinator.control_plane_unavailable",
                DoctorSeverity::Warning,
                format!(
                    "{} coordinator status backend is unavailable for {}",
                    status.provider.as_str(),
                    status.name
                ),
                None,
                Some(
                    "rerun crab replica failover status after the managed coordinator status backend is wired"
                        .to_owned(),
                ),
            ));
        }
        for check in &status.checks {
            if matches!(
                check.state,
                CoordinatorCheckState::Missing | CoordinatorCheckState::Drifted
            ) {
                findings.push(finding(
                    &check.code,
                    DoctorSeverity::Error,
                    check.message.clone(),
                    None,
                    Some(check.remediation.clone()),
                ));
            }
        }
    }

    if let Some(error) = coordinator_error {
        findings.push(finding(
            "coordinator.status_probe_failed",
            DoctorSeverity::Error,
            format!("managed coordinator status probe failed: {error}"),
            None,
            Some(
                "fix coordinator credentials, permissions, or provider topology, then rerun crab replica failover status"
                    .to_owned(),
            ),
        ));
    }

    findings.extend(coordinator_state_pressure_findings(coordinator_health));

    for status in replicas {
        if !status.ready {
            findings.push(finding(
                "replica.not_ready",
                DoctorSeverity::Error,
                format!("replica {} is not ready for reads", status.name),
                Some(status.name.clone()),
                status.last_fallback_reason.clone(),
            ));
        }
        if status.lag_generations.is_some_and(|lag| lag > 0) {
            findings.push(finding(
                "replica.lagging",
                DoctorSeverity::Warning,
                format!("replica {} is behind the primary", status.name),
                Some(status.name.clone()),
                Some("wait for provider replication or run crab replica status --deep".to_owned()),
            ));
        }
        if status.fallback_count > 0 {
            findings.push(finding(
                "replica.fallback_observed",
                DoctorSeverity::Warning,
                format!(
                    "replica {} has {} recorded primary fallback(s)",
                    status.name, status.fallback_count
                ),
                Some(status.name.clone()),
                status.last_fallback_reason.clone(),
            ));
        }
        if status.readiness_cache_hit && !deep {
            let control_status = control_plane
                .iter()
                .find(|control_status| control_status.replica_name == status.name);
            if let Some(reason) = readiness_cache_risk_reason(control_status) {
                findings.push(finding(
                    "replica.cache_hit_unverified",
                    DoctorSeverity::Warning,
                    format!(
                        "replica {} readiness came from local cache while provider status is not verified",
                        status.name
                    ),
                    Some(status.name.clone()),
                    Some(format!(
                        "{reason}; rerun crab replica doctor --deep after provider status is verified"
                    )),
                ));
            } else {
                findings.push(finding(
                    "replica.cache_hit",
                    DoctorSeverity::Info,
                    format!("replica {} readiness came from local cache", status.name),
                    Some(status.name.clone()),
                    Some(
                        "rerun crab replica doctor --deep for live manifest/object checks"
                            .to_owned(),
                    ),
                ));
            }
        }
    }

    if active_active.mode == ReplicationMode::ActiveActive && !active_active.writes_enabled {
        findings.push(finding(
            "active_active.writes_blocked",
            DoctorSeverity::Error,
            "active-active writes are blocked",
            None,
            active_active.reason.clone(),
        ));
    }
    findings
}

fn coordinator_state_pressure_findings(
    coordinator_health: Option<&CoordinatorHealth>,
) -> Vec<DoctorFinding> {
    let Some(health) = coordinator_health else {
        return Vec::new();
    };
    let Some(summary) = health.state_summary.as_ref() else {
        return vec![finding(
            "coordinator.state_summary_missing",
            DoctorSeverity::Warning,
            "coordinator health did not include state pressure counters",
            None,
            Some("upgrade or enable the managed coordinator data-plane health adapter".to_owned()),
        )];
    };

    let mut findings = Vec::new();
    if let Some(max_state_bytes) = summary.max_state_bytes {
        if usage_at_or_above(
            summary.state_bytes,
            max_state_bytes,
            COORDINATOR_STATE_CRITICAL_PERCENT,
        ) {
            findings.push(finding(
                "coordinator.state_size_critical",
                DoctorSeverity::Error,
                format!(
                    "coordinator repo authority state uses {} of {} bytes ({}%)",
                    summary.state_bytes,
                    max_state_bytes,
                    usage_percent(summary.state_bytes, max_state_bytes)
                ),
                None,
                Some(
                    "run crab replica repair --from-coordinator, ensure terminal operation compaction is active, and reduce pending coordinator transactions before admitting more writes"
                        .to_owned(),
                ),
            ));
        } else if usage_at_or_above(
            summary.state_bytes,
            max_state_bytes,
            COORDINATOR_STATE_WARNING_PERCENT,
        ) {
            findings.push(finding(
                "coordinator.state_size_high",
                DoctorSeverity::Warning,
                format!(
                    "coordinator repo authority state uses {} of {} bytes ({}%)",
                    summary.state_bytes,
                    max_state_bytes,
                    usage_percent(summary.state_bytes, max_state_bytes)
                ),
                None,
                Some(
                    "watch coordinator state pressure and verify repair/materialization workers are clearing committed operations"
                        .to_owned(),
                ),
            ));
        }
    }

    if usage_at_or_above(
        summary.completed_operation_count,
        summary.max_completed_operations,
        COORDINATOR_STATE_WARNING_PERCENT,
    ) {
        findings.push(finding(
            "coordinator.completed_operations_high",
            DoctorSeverity::Warning,
            format!(
                "coordinator completed-operation replay cache uses {} of {} records ({}%)",
                summary.completed_operation_count,
                summary.max_completed_operations,
                usage_percent(
                    summary.completed_operation_count,
                    summary.max_completed_operations
                )
            ),
            None,
            Some(
                "shorten client retry windows or raise the managed coordinator replay-cache limit for this topology"
                    .to_owned(),
            ),
        ));
    }

    findings
}

fn usage_at_or_above(used: usize, limit: usize, threshold_percent: u64) -> bool {
    limit > 0 && (used as u128) * 100 >= (limit as u128) * (threshold_percent as u128)
}

fn usage_percent(used: usize, limit: usize) -> u64 {
    if limit == 0 {
        return 0;
    }
    (((used as u128) * 100) / (limit as u128)).min(u64::MAX as u128) as u64
}

fn provider_control_plane_unavailable_message(status: &ControlPlaneStatus) -> String {
    let prefix = format!(
        "{} control-plane status backend is unavailable for replica {}",
        status.provider, status.replica_name
    );
    status
        .checks
        .iter()
        .find(|check| check.state != ControlPlaneCheckState::Verified)
        .map(|check| format!("{prefix}: {}", check.message))
        .unwrap_or(prefix)
}

fn readiness_cache_risk_reason(status: Option<&ControlPlaneStatus>) -> Option<String> {
    policy_drift_reason(status).or_else(|| provider_status_gap_reason(status))
}

fn doctor_fix_plan(
    primary: Option<&str>,
    replication: Option<&crate::replication::ReplicationConfig>,
    control_plane: &[ControlPlaneStatus],
    coordinator: Option<&CoordinatorControlPlaneStatus>,
    findings: &[DoctorFinding],
) -> Vec<DoctorFixAction> {
    let mut plan = Vec::new();
    for finding in findings {
        match finding.code.as_str() {
            "replication.primary_missing" => push_fix_once(
                &mut plan,
                fix_action(
                    finding,
                    "Configure the primary Crab remote before adding or enabling replicas.",
                    Some("crab init <crab-url>".to_owned()),
                    false,
                ),
            ),
            "replication.not_configured" | "replica.none_configured" => push_fix_once(
                &mut plan,
                fix_action(
                    finding,
                    "Add at least one provider-backed replica, then keep it read-disabled until verification passes.",
                    None,
                    false,
                ),
            ),
            "replica.read_disabled" => {
                if let Some(name) = finding.replica.as_deref() {
                    push_fix_once(
                        &mut plan,
                        fix_action(
                            finding,
                            "Enable replica reads after live readiness and backfill checks pass.",
                            Some(replica_wait_command(name)),
                            false,
                        ),
                    );
                }
            }
            "replica.backfill_unverified" => {
                if let Some(name) = finding.replica.as_deref() {
                    let mut action = fix_action(
                        finding,
                        "Inspect provider-native historical-object backfill before read cutover.",
                        Some(replica_backfill_status_command(name)),
                        false,
                    );
                    if let Some(replica) = replica_config_for_finding(replication, finding) {
                        action.cost_hints = provider_backfill_cost_hints(replica);
                        action.risk_hints = provider_backfill_risk_hints(replica);
                    }
                    push_fix_once(&mut plan, action);
                }
            }
            "replica.not_ready" | "replica.fallback_observed" => {
                if let Some(name) = finding.replica.as_deref() {
                    push_fix_once(
                        &mut plan,
                        fix_action(
                            finding,
                            "Run live manifest and referenced-object checks against the replica.",
                            Some(replica_verify_command(name)),
                            false,
                        ),
                    );
                }
            }
            "replica.lagging" => push_fix_once(
                &mut plan,
                fix_action(
                    finding,
                    "Bypass the readiness cache and re-measure primary-to-replica generation lag.",
                    Some("crab replica status --deep".to_owned()),
                    false,
                ),
            ),
            "replica.cache_hit" | "replica.cache_hit_unverified" => push_fix_once(
                &mut plan,
                fix_action(
                    finding,
                    "Refresh doctor findings with live replica manifest and object checks.",
                    Some("crab replica doctor --deep --fix-plan".to_owned()),
                    false,
                ),
            ),
            "provider.control_plane_unavailable" => {
                if let Some(replica) = replica_config_for_finding(replication, finding) {
                    push_fix_once(
                        &mut plan,
                        fix_action(
                            finding,
                            "Export the desired provider resources for audit until the live provider backend is available.",
                            Some(replica_export_command(&replica.name)),
                            false,
                        ),
                    );
                }
            }
            "coordinator.control_plane_unavailable" => push_fix_once(
                &mut plan,
                fix_action(
                    finding,
                    "Inspect failover and coordinator health; writes stay blocked until Crab can verify the coordinator.",
                    Some("crab replica failover status --json".to_owned()),
                    false,
                ),
            ),
            "coordinator.state_summary_missing" => push_fix_once(
                &mut plan,
                fix_action(
                    finding,
                    "Refresh coordinator data-plane health with a backend that reports state pressure.",
                    Some("crab replica failover status --json".to_owned()),
                    false,
                ),
            ),
            "coordinator.state_size_high" | "coordinator.completed_operations_high" => {
                push_fix_once(
                    &mut plan,
                    fix_action(
                        finding,
                        "Inspect coordinator pressure and verify repair/materialization workers are draining completed operations.",
                        Some("crab replica repair --from-coordinator --dry-run --json".to_owned()),
                        false,
                    ),
                );
            }
            "coordinator.state_size_critical" => push_fix_once(
                &mut plan,
                fix_action(
                    finding,
                    "Reduce coordinator repo-authority state before admitting more active-active writes.",
                    Some("crab replica repair --from-coordinator --dry-run --json".to_owned()),
                    false,
                ),
            ),
            "active_active.writes_blocked" => push_fix_once(
                &mut plan,
                fix_action(
                    finding,
                    "Check coordinator health, fencing readiness, and enabled writer regions before retrying writes.",
                    Some("crab replica failover status --json".to_owned()),
                    false,
                ),
            ),
            _ => {
                if let Some((status, check)) =
                    control_plane_check_for_finding(control_plane, finding)
                    && let Some(replica) = replica_config_for_finding(replication, finding)
                {
                    let (description, command) =
                        provider_check_fix(primary, status, check, replica);
                    let mut action = fix_action(finding, description, command, false);
                    action.cost_hints = provider_control_plane_cost_hints(replica, check);
                    action.risk_hints = provider_control_plane_risk_hints(replica, check);
                    push_fix_once(&mut plan, action);
                    continue;
                }
                if let Some((status, check)) = coordinator_check_for_finding(coordinator, finding) {
                    let (description, command) = coordinator_check_fix(status, check);
                    let mut action = fix_action(finding, description, command, false);
                    action.cost_hints = coordinator_cost_hints(status);
                    action.risk_hints = coordinator_risk_hints(status, check);
                    push_fix_once(&mut plan, action);
                }
            }
        }
    }
    plan
}

fn provider_check_fix(
    primary: Option<&str>,
    status: &ControlPlaneStatus,
    check: &ControlPlaneCheck,
    replica: &ReplicaConfig,
) -> (&'static str, Option<String>) {
    match check.state {
        ControlPlaneCheckState::Missing => (
            "Create missing Crab-managed provider resources through the provider control plane.",
            Some(replica_apply_command(
                primary.unwrap_or(status.primary.as_str()),
                replica,
            )),
        ),
        ControlPlaneCheckState::Drifted => (
            "Review drift against the Crab-owned provider plan before changing cloud resources.",
            Some(replica_export_command(&replica.name)),
        ),
        ControlPlaneCheckState::Unsupported | ControlPlaneCheckState::Unknown => (
            "Resolve the provider-specific unsupported or unknown state before enabling reads.",
            None,
        ),
        ControlPlaneCheckState::Verified => {
            ("No provider action is required for this check.", None)
        }
    }
}

fn coordinator_check_fix(
    status: &CoordinatorControlPlaneStatus,
    check: &CoordinatorControlPlaneCheck,
) -> (&'static str, Option<String>) {
    match check.state {
        CoordinatorCheckState::Missing => (
            "Create missing Crab-managed coordinator resources through the coordinator control plane.",
            Some(coordinator_add_command(status, true)),
        ),
        CoordinatorCheckState::Drifted => (
            "Review coordinator drift against the Crab-owned plan before changing cloud resources.",
            Some(coordinator_add_command(status, false)),
        ),
        CoordinatorCheckState::Unsupported | CoordinatorCheckState::Unknown => (
            "Resolve the coordinator-specific unsupported or unknown state before enabling active-active writes.",
            Some("crab replica failover status --json".to_owned()),
        ),
        CoordinatorCheckState::Verified => {
            ("No coordinator action is required for this check.", None)
        }
    }
}

fn provider_control_plane_cost_hints(
    replica: &ReplicaConfig,
    check: &ControlPlaneCheck,
) -> Vec<String> {
    let mut hints = provider_common_cost_hints(replica);
    match replica.provider {
        ReplicationProviderKind::S3 => {
            if replica.rpo == ReplicationRpo::Fast {
                hints.push(
                    "S3 Replication Time Control and replication metrics can add per-object and per-GB charges."
                        .to_owned(),
                );
            }
            if replica.backfill || check.action.contains("batch") {
                hints.push(
                    "S3 Batch Replication can add batch job, request, and replicated storage charges."
                        .to_owned(),
                );
            }
        }
        ReplicationProviderKind::Gcs => {
            if replica.rpo == ReplicationRpo::Fast {
                hints.push(
                    "GCS Turbo Replication uses ASYNC_TURBO RPO and can increase dual-region storage costs."
                        .to_owned(),
                );
            }
            if replica.backfill || check.action.contains("transfer") {
                hints.push(
                    "GCS Storage Transfer backfill can add transfer service, operation, and request costs."
                        .to_owned(),
                );
            }
        }
        ReplicationProviderKind::Azure => {
            hints.push(
                "Azure change feed, blob versioning, object replication, and verification reads can increase storage and transaction costs."
                    .to_owned(),
            );
        }
    }
    hints
}

fn provider_backfill_cost_hints(replica: &ReplicaConfig) -> Vec<String> {
    let mut hints = provider_common_cost_hints(replica);
    match replica.provider {
        ReplicationProviderKind::S3 => hints.push(
            "Historical backfill through S3 Batch Replication can add batch job, request, and replicated-byte charges."
                .to_owned(),
        ),
        ReplicationProviderKind::Gcs => hints.push(
            "Historical backfill through Storage Transfer can add transfer, operation, and inter-region network charges."
                .to_owned(),
        ),
        ReplicationProviderKind::Azure => hints.push(
            "Existing-blob verification and object replication can add list/head transaction and replicated storage charges."
                .to_owned(),
        ),
    }
    hints
}

fn provider_common_cost_hints(replica: &ReplicaConfig) -> Vec<String> {
    vec![format!(
        "{} replication may add replicated storage, inter-region transfer, request, monitoring, and backfill charges for replica {} in {}.",
        replica.provider, replica.name, replica.region
    )]
}

fn provider_control_plane_risk_hints(
    replica: &ReplicaConfig,
    check: &ControlPlaneCheck,
) -> Vec<String> {
    let mut hints = provider_common_risk_hints(replica);
    if check.state == ControlPlaneCheckState::Drifted {
        hints.push(
            "Drifted provider resources should be reviewed with `crab replica export` before applying changes."
                .to_owned(),
        );
    }
    match replica.provider {
        ReplicationProviderKind::S3 => hints.push(
            "Cross-account buckets, KMS keys, Object Lock, lifecycle rules, and bucket policies can block S3 replication."
                .to_owned(),
        ),
        ReplicationProviderKind::Gcs => hints.push(
            "Unsupported bucket topology, CMEK permissions, Storage Transfer service accounts, and retention policies can block GCS replication."
                .to_owned(),
        ),
        ReplicationProviderKind::Azure => hints.push(
            "Cross-tenant RBAC, customer-managed keys, immutable storage policies, lifecycle rules, and private endpoints can block Azure object replication."
                .to_owned(),
        ),
    }
    hints
}

fn provider_backfill_risk_hints(replica: &ReplicaConfig) -> Vec<String> {
    let mut hints = provider_common_risk_hints(replica);
    match replica.provider {
        ReplicationProviderKind::S3 => hints.push(
            "S3 backfill must not enable reads until Batch Replication status and referenced-object readiness are verified."
                .to_owned(),
        ),
        ReplicationProviderKind::Gcs => hints.push(
            "GCS backfill depends on Storage Transfer permissions and can lag object replication even after the replica manifest appears."
                .to_owned(),
        ),
        ReplicationProviderKind::Azure => hints.push(
            "Azure existing-blob replication progress can be hard to infer; keep reads disabled until object-set verification passes."
                .to_owned(),
        ),
    }
    hints
}

fn provider_common_risk_hints(replica: &ReplicaConfig) -> Vec<String> {
    vec![format!(
        "Apply only Crab-managed resources for replica {}; do not mutate unrelated bucket/account policies from this plan.",
        replica.name
    )]
}

fn coordinator_cost_hints(status: &CoordinatorControlPlaneStatus) -> Vec<String> {
    match status.provider {
        ManagedCoordinatorProvider::DynamoDb => vec![format!(
            "DynamoDB Global Table coordinator {} can add replicated write, storage, stream, and multi-region table costs across {} and failover regions.",
            status.name, status.region
        )],
        ManagedCoordinatorProvider::Spanner => vec![format!(
            "Cloud Spanner coordinator {} can add Enterprise Plus instance/node, storage, backup, and inter-region replication costs.",
            status.name
        )],
        ManagedCoordinatorProvider::CosmosDb => vec![format!(
            "Azure Cosmos DB coordinator {} can add RU/s, storage, backup, and replicated-region costs.",
            status.name
        )],
    }
}

fn coordinator_risk_hints(
    status: &CoordinatorControlPlaneStatus,
    check: &CoordinatorControlPlaneCheck,
) -> Vec<String> {
    let mut hints = Vec::new();
    if check.state == CoordinatorCheckState::Drifted {
        hints.push(
            "Coordinator drift can block active-active writes; review the dry-run plan before applying changes."
                .to_owned(),
        );
    }
    match status.provider {
        ManagedCoordinatorProvider::DynamoDb => hints.push(
            "DynamoDB coordinator safety depends on MRSC global tables, same-account replicas, and conditional state-record CAS."
                .to_owned(),
        ),
        ManagedCoordinatorProvider::Spanner => hints.push(
            "Spanner coordinator safety depends on strong reads, read-write transactions, and the expected RepoState schema."
                .to_owned(),
        ),
        ManagedCoordinatorProvider::CosmosDb => hints.push(
            "Cosmos DB coordinator safety depends on Strong consistency, disabled multi-region writes, and ETag CAS on the repo_state document."
                .to_owned(),
        ),
    }
    hints
}

fn control_plane_check_for_finding<'a>(
    statuses: &'a [ControlPlaneStatus],
    finding: &DoctorFinding,
) -> Option<(&'a ControlPlaneStatus, &'a ControlPlaneCheck)> {
    let replica = finding.replica.as_deref()?;
    statuses
        .iter()
        .find(|status| status.replica_name == replica)
        .and_then(|status| {
            status
                .checks
                .iter()
                .find(|check| check.code == finding.code)
                .map(|check| (status, check))
        })
}

fn coordinator_check_for_finding<'a>(
    status: Option<&'a CoordinatorControlPlaneStatus>,
    finding: &DoctorFinding,
) -> Option<(
    &'a CoordinatorControlPlaneStatus,
    &'a CoordinatorControlPlaneCheck,
)> {
    let status = status?;
    status
        .checks
        .iter()
        .find(|check| check.code == finding.code)
        .map(|check| (status, check))
}

fn replica_config_for_finding<'a>(
    replication: Option<&'a crate::replication::ReplicationConfig>,
    finding: &DoctorFinding,
) -> Option<&'a ReplicaConfig> {
    let name = finding.replica.as_deref()?;
    replication?
        .replicas
        .iter()
        .find(|replica| replica.name == name)
}

fn fix_action(
    finding: &DoctorFinding,
    description: impl Into<String>,
    command: Option<String>,
    destructive: bool,
) -> DoctorFixAction {
    DoctorFixAction {
        code: finding.code.clone(),
        severity: finding.severity,
        replica: finding.replica.clone(),
        description: description.into(),
        command,
        cost_hints: Vec::new(),
        risk_hints: Vec::new(),
        destructive,
    }
}

fn push_fix_once(plan: &mut Vec<DoctorFixAction>, action: DoctorFixAction) {
    if plan.iter().any(|existing| {
        existing.code == action.code
            && existing.replica == action.replica
            && existing.command == action.command
    }) {
        return;
    }
    plan.push(action);
}

fn replica_apply_command(primary: &str, replica: &ReplicaConfig) -> String {
    let mut parts = vec![
        "crab".to_owned(),
        "replica".to_owned(),
        "add".to_owned(),
        shell_arg(&replica.name),
        "--provider".to_owned(),
        shell_arg(replica.provider.as_str()),
        "--primary".to_owned(),
        shell_arg(primary),
        "--replica".to_owned(),
        shell_arg(&replica.url),
        "--region".to_owned(),
        shell_arg(&replica.region),
        "--rpo".to_owned(),
        shell_arg(replica.rpo.as_str()),
    ];
    if replica.backfill {
        parts.push("--backfill".to_owned());
    }
    parts.push("--apply".to_owned());
    parts.join(" ")
}

fn replica_export_command(name: &str) -> String {
    format!(
        "crab replica export --name {} --format terraform",
        shell_arg(name)
    )
}

fn replica_verify_command(name: &str) -> String {
    format!("crab replica verify --deep --name {}", shell_arg(name))
}

fn replica_wait_command(name: &str) -> String {
    format!("crab replica wait {} --enable-read", shell_arg(name))
}

fn replica_backfill_status_command(name: &str) -> String {
    format!(
        "crab replica backfill status --name {} --json",
        shell_arg(name)
    )
}

fn coordinator_add_command(status: &CoordinatorControlPlaneStatus, apply: bool) -> String {
    let mut parts = vec![
        "crab".to_owned(),
        "replica".to_owned(),
        "coordinator".to_owned(),
        "add".to_owned(),
        "--provider".to_owned(),
        shell_arg(status.provider.as_str()),
        "--name".to_owned(),
        shell_arg(&status.name),
        "--region".to_owned(),
        shell_arg(&status.region),
    ];
    for region in &status.failover_regions {
        parts.push("--failover-region".to_owned());
        parts.push(shell_arg(region));
    }
    if apply {
        parts.push("--apply".to_owned());
    } else {
        parts.push("--dry-run".to_owned());
        parts.push("--json".to_owned());
    }
    parts.join(" ")
}

fn shell_arg(value: &str) -> String {
    if value.chars().all(|ch| {
        ch.is_ascii_alphanumeric()
            || matches!(ch, '-' | '_' | '.' | '/' | ':' | '=' | '@' | ',' | '+')
    }) {
        return value.to_owned();
    }
    let escaped = value.replace('\'', "'\\''");
    format!("'{escaped}'")
}

fn cost_assumptions_from_args(args: &CostArgs) -> Result<CostAssumptions> {
    Ok(CostAssumptions {
        monthly_write_gb: validate_cost_quantity(
            "replica.cost.monthly-write-gb",
            args.monthly_write_gb,
        )?,
        monthly_read_gb: validate_cost_quantity(
            "replica.cost.monthly-read-gb",
            args.monthly_read_gb,
        )?,
        backfill_gb: validate_cost_quantity("replica.cost.backfill-gb", args.backfill_gb)?,
        monthly_requests_million: validate_cost_quantity(
            "replica.cost.monthly-requests-million",
            args.monthly_requests_million,
        )?,
    })
}

fn validate_cost_quantity(key: &str, value: f64) -> Result<f64> {
    if value.is_finite() && value >= 0.0 {
        return Ok(value);
    }
    Err(CrabError::Configuration {
        key: key.to_owned(),
        origin: "cost estimate quantities must be finite non-negative numbers".into(),
    })
}

fn cost_payload(
    primary: Option<String>,
    replication: &crate::replication::ReplicationConfig,
    name: Option<&str>,
    assumptions: CostAssumptions,
) -> Result<CostPayload> {
    let replicas = cost_selected_replicas(replication, name)?;
    let estimates = replicas
        .iter()
        .map(|replica| replica_cost_estimate(replica, assumptions))
        .collect::<Vec<_>>();
    let replica_count = estimates.len() as f64;
    Ok(CostPayload {
        primary,
        assumptions,
        totals: CostTotals {
            replicas: estimates.len() as u64,
            monthly_replicated_write_gb: assumptions.monthly_write_gb * replica_count,
            monthly_replica_read_gb: assumptions.monthly_read_gb * replica_count,
            one_time_backfill_gb: assumptions.backfill_gb * replica_count,
            monthly_request_millions: assumptions.monthly_requests_million * replica_count,
        },
        estimates,
        pricing_notice: vec![
            "Crab estimates billable quantities, not currency; multiply these meters by provider regional prices and account-specific discounts.".to_owned(),
            "Cloud bills can also include destination storage class, KMS/CMEK, inventory, monitoring, logs, taxes, and support charges outside Crab's object replication path.".to_owned(),
        ],
    })
}

fn cost_selected_replicas<'a>(
    replication: &'a crate::replication::ReplicationConfig,
    name: Option<&str>,
) -> Result<Vec<&'a ReplicaConfig>> {
    if let Some(name) = name {
        let replica = replication
            .replicas
            .iter()
            .find(|replica| replica.name == name)
            .ok_or_else(|| CrabError::Configuration {
                key: "replication.replicas".into(),
                origin: format!("replica {name} is not configured"),
            })?;
        return Ok(vec![replica]);
    }
    if replication.replicas.is_empty() {
        return Err(CrabError::Configuration {
            key: "replication.replicas".into(),
            origin: "no replicas are configured".into(),
        });
    }
    Ok(replication.replicas.iter().collect())
}

fn replica_cost_estimate(
    replica: &ReplicaConfig,
    assumptions: CostAssumptions,
) -> ReplicaCostEstimate {
    let mut meters = common_cost_meters(assumptions);
    meters.extend(provider_cost_meters(replica, assumptions));
    ReplicaCostEstimate {
        name: replica.name.clone(),
        provider: replica.provider,
        region: replica.region.clone(),
        rpo: replica.rpo,
        read_enabled: replica.read,
        backfill_configured: replica.backfill,
        meters,
        warnings: cost_warnings(replica, assumptions),
    }
}

fn common_cost_meters(assumptions: CostAssumptions) -> Vec<CostMeter> {
    vec![
        cost_meter(
            "replication.write-data",
            "New immutable Crab objects replicated to this regional replica.",
            assumptions.monthly_write_gb,
            "GB/month",
            "provider replicated-data, destination write, and destination storage rates",
        ),
        cost_meter(
            "replication.inter-region-transfer",
            "Cross-region data transfer from the primary or writer region to this replica.",
            assumptions.monthly_write_gb + assumptions.backfill_gb,
            "GB",
            "provider inter-region transfer and replication data-transfer rates",
        ),
        cost_meter(
            "replica.read-egress",
            "Replica read traffic for clone, fetch, hydrate, mount, and SDK reads.",
            assumptions.monthly_read_gb,
            "GB/month",
            "provider egress rate for the client location and storage region",
        ),
        cost_meter(
            "replication.requests",
            "Object-store requests generated by replication, readiness probes, backfill, and reads.",
            assumptions.monthly_requests_million,
            "million requests/month",
            "provider GET/HEAD/LIST/PUT/COPY and replication request rates",
        ),
    ]
}

fn provider_cost_meters(replica: &ReplicaConfig, assumptions: CostAssumptions) -> Vec<CostMeter> {
    let mut meters = Vec::new();
    if replica.backfill || assumptions.backfill_gb > 0.0 {
        meters.push(provider_backfill_cost_meter(
            replica,
            assumptions.backfill_gb,
        ));
    }
    if replica.rpo == ReplicationRpo::Fast {
        meters.push(provider_fast_rpo_cost_meter(
            replica,
            assumptions.monthly_write_gb,
        ));
    }
    meters
}

fn provider_backfill_cost_meter(replica: &ReplicaConfig, backfill_gb: f64) -> CostMeter {
    match replica.provider {
        ReplicationProviderKind::S3 => cost_meter(
            "s3.batch-replication",
            "S3 Batch Replication for historical Crab objects.",
            backfill_gb,
            "GB one-time",
            "S3 Batch Operations, replication requests, replicated bytes, and manifest inventory rates",
        ),
        ReplicationProviderKind::Gcs => cost_meter(
            "gcs.storage-transfer",
            "GCS Storage Transfer backfill for historical Crab objects.",
            backfill_gb,
            "GB one-time",
            "Storage Transfer Service operation, transfer, and destination write rates",
        ),
        ReplicationProviderKind::Azure => cost_meter(
            "azure.existing-blob-replication",
            "Azure existing-blob object replication and verification for historical Crab objects.",
            backfill_gb,
            "GB one-time",
            "Azure blob write, read/list verification, replication, and transaction rates",
        ),
    }
}

fn provider_fast_rpo_cost_meter(replica: &ReplicaConfig, monthly_write_gb: f64) -> CostMeter {
    match replica.provider {
        ReplicationProviderKind::S3 => cost_meter(
            "s3.replication-time-control",
            "S3 Replication Time Control and replication metrics for fast RPO.",
            monthly_write_gb,
            "GB/month",
            "S3 RTC replicated bytes, metrics, events, and replication request rates",
        ),
        ReplicationProviderKind::Gcs => cost_meter(
            "gcs.turbo-replication",
            "GCS Turbo Replication RPO for dual-region buckets.",
            monthly_write_gb,
            "GB/month",
            "GCS Turbo Replication premium replicated-byte and operation rates",
        ),
        ReplicationProviderKind::Azure => cost_meter(
            "azure.priority-replication-review",
            "Fast RPO was requested; verify the Azure account's supported priority replication or premium monitoring meter.",
            monthly_write_gb,
            "GB/month",
            "Azure account-specific replication, transaction, monitoring, and SLA premium rates",
        ),
    }
}

fn cost_warnings(replica: &ReplicaConfig, assumptions: CostAssumptions) -> Vec<String> {
    let mut warnings = Vec::new();
    if assumptions.monthly_write_gb == 0.0
        && assumptions.monthly_read_gb == 0.0
        && assumptions.backfill_gb == 0.0
        && assumptions.monthly_requests_million == 0.0
    {
        warnings.push(
            "all usage assumptions are zero; pass --monthly-write-gb, --monthly-read-gb, --backfill-gb, or --monthly-requests-million for a rollout estimate"
                .to_owned(),
        );
    }
    if !replica.read && assumptions.monthly_read_gb > 0.0 {
        warnings.push(
            "replica reads are disabled in config; read-egress quantities apply only after enabling reads"
                .to_owned(),
        );
    }
    if replica.backfill && assumptions.backfill_gb == 0.0 {
        warnings.push(
            "replica was configured with backfill but --backfill-gb is zero; historical copy cost is not modeled"
                .to_owned(),
        );
    }
    if !replica.backfill && assumptions.backfill_gb > 0.0 {
        warnings.push(
            "backfill data was supplied for a replica not configured with --backfill; treat it as an external historical-copy estimate"
                .to_owned(),
        );
    }
    if replica.provider == ReplicationProviderKind::Azure && replica.rpo == ReplicationRpo::Fast {
        warnings.push(
            "Azure object replication does not expose the same native RTC/Turbo contract as S3/GCS; confirm the selected account SKU and SLA before using this fast-RPO estimate"
                .to_owned(),
        );
    }
    warnings
}

fn cost_meter(
    code: &str,
    description: &str,
    quantity: f64,
    unit: &str,
    pricing_input: &str,
) -> CostMeter {
    CostMeter {
        code: code.to_owned(),
        description: description.to_owned(),
        quantity,
        unit: unit.to_owned(),
        pricing_input: pricing_input.to_owned(),
    }
}

fn runbook_payload(
    scenario: RunbookScenarioArg,
    primary: Option<String>,
    replication: Option<&crate::replication::ReplicationConfig>,
    name: Option<&str>,
) -> Result<RunbookPayload> {
    let selected_replica = runbook_selected_replica(replication, name)?;
    let replica = selected_replica.map(runbook_replica_context);
    let mode = replication.map(|replication| replication.mode);
    let warnings = runbook_warnings(scenario, replication, replica.as_ref(), name);
    let steps = match scenario {
        RunbookScenarioArg::PrimaryOutage => {
            primary_outage_runbook(replication, replica.as_ref(), primary.as_deref())
        }
        RunbookScenarioArg::ReplicaStale => replica_stale_runbook(replica.as_ref()),
        RunbookScenarioArg::FailedBackfill => {
            failed_backfill_runbook(replica.as_ref(), primary.as_deref())
        }
        RunbookScenarioArg::PolicyDrift => {
            policy_drift_runbook(replica.as_ref(), primary.as_deref())
        }
        RunbookScenarioArg::DestinationWrites => {
            destination_writes_runbook(replication, replica.as_ref())
        }
    };
    Ok(RunbookPayload {
        scenario,
        primary,
        mode,
        replica,
        warnings,
        steps,
    })
}

fn runbook_selected_replica<'a>(
    replication: Option<&'a crate::replication::ReplicationConfig>,
    name: Option<&str>,
) -> Result<Option<&'a ReplicaConfig>> {
    let Some(replication) = replication else {
        return Ok(None);
    };
    if let Some(name) = name {
        return replication
            .replicas
            .iter()
            .find(|replica| replica.name == name)
            .map(Some)
            .ok_or_else(|| CrabError::Configuration {
                key: "replication.replicas".into(),
                origin: format!("replica {name} is not configured"),
            });
    }
    match replication.replicas.as_slice() {
        [replica] => Ok(Some(replica)),
        _ => Ok(None),
    }
}

fn runbook_replica_context(replica: &ReplicaConfig) -> RunbookReplicaContext {
    RunbookReplicaContext {
        name: replica.name.clone(),
        provider: replica.provider,
        url: replica.url.clone(),
        region: replica.region.clone(),
        read_enabled: replica.read,
        backfill_configured: replica.backfill,
        rpo: replica.rpo,
    }
}

fn runbook_warnings(
    scenario: RunbookScenarioArg,
    replication: Option<&crate::replication::ReplicationConfig>,
    replica: Option<&RunbookReplicaContext>,
    requested_name: Option<&str>,
) -> Vec<String> {
    let mut warnings = Vec::new();
    let Some(replication) = replication else {
        warnings.push(
            "replication is not configured; commands with <name> need a configured replica first"
                .to_owned(),
        );
        return warnings;
    };
    if requested_name.is_none() && replication.replicas.len() > 1 && replica.is_none() {
        warnings.push("multiple replicas are configured; rerun with --name <replica> for concrete replica commands".to_owned());
    }
    if requested_name.is_none() && replication.replicas.is_empty() {
        warnings.push("no replicas are configured; add a replica before using replica-specific recovery steps".to_owned());
    }
    if let Some(replica) = replica {
        if !replica.read_enabled
            && matches!(
                scenario,
                RunbookScenarioArg::PrimaryOutage | RunbookScenarioArg::ReplicaStale
            )
        {
            warnings.push(format!(
                "replica {} is read-disabled; verify readiness and backfill before relying on it",
                replica.name
            ));
        }
        if replica.backfill_configured
            && matches!(
                scenario,
                RunbookScenarioArg::PrimaryOutage
                    | RunbookScenarioArg::FailedBackfill
                    | RunbookScenarioArg::PolicyDrift
            )
        {
            warnings.push(format!(
                "replica {} was configured with backfill; read cutover must wait for provider backfill verification",
                replica.name
            ));
        }
    }
    warnings
}

fn primary_outage_runbook(
    replication: Option<&crate::replication::ReplicationConfig>,
    replica: Option<&RunbookReplicaContext>,
    primary: Option<&str>,
) -> Vec<RunbookStep> {
    let mut steps = Vec::new();
    if replication.is_some_and(crate::replication::ReplicationConfig::is_active_active) {
        push_runbook_step(
            &mut steps,
            "Check write admission",
            Some("crab replica failover status --json".to_owned()),
            "Confirm coordinator health, writer-region state, and whether writes are already fenced.",
            false,
            false,
        );
        push_runbook_step(
            &mut steps,
            "Fence ambiguous writes",
            Some("crab replica failover fence --apply --reason primary-outage".to_owned()),
            "Block new active-active writes before changing provider traffic or recovering in-flight transactions.",
            false,
            false,
        );
        push_runbook_step(
            &mut steps,
            "Repair regional manifests",
            Some("crab replica repair --from-coordinator --dry-run --json".to_owned()),
            "Plan manifest repair from coordinator truth after referenced objects are present.",
            false,
            false,
        );
        push_runbook_step(
            &mut steps,
            "Apply coordinator repair",
            Some("crab replica repair --from-coordinator".to_owned()),
            "Materialize missing regional manifests from committed coordinator records.",
            false,
            false,
        );
        push_runbook_step(
            &mut steps,
            "Resume writes after external failover",
            Some("crab replica failover resume --repair-verified --apply".to_owned()),
            "Re-admit writes only after provider traffic, object replication, and repair checks are complete.",
            false,
            false,
        );
        return steps;
    }

    let name = runbook_replica_name(replica);
    push_runbook_step(
        &mut steps,
        "Measure replica readiness",
        Some("crab replica status --deep --json".to_owned()),
        "Use live manifest/object checks before considering a read-replica promotion.",
        false,
        false,
    );
    push_runbook_step(
        &mut steps,
        "Verify target replica",
        Some(format!("crab replica verify --deep --name {name}")),
        "Prove the target has the publication manifest and every referenced immutable object.",
        false,
        false,
    );
    push_runbook_step(
        &mut steps,
        "Review promotion plan",
        Some(format!("crab replica promote {name} --plan --json")),
        "Check URL write-safety, read-enable proof, provider drift, and ordered promotion blockers.",
        false,
        false,
    );
    if let (Some(primary), Some(replica)) = (primary, replica) {
        push_runbook_step(
            &mut steps,
            "Plan guarded primary rewrite",
            Some(format!(
                "crab replica set-primary {} --plan --json",
                shell_arg(&replica.url)
            )),
            &format!(
                "Compare the configured primary {primary} with the verified target before applying disaster recovery."
            ),
            false,
            false,
        );
    }
    push_runbook_step(
        &mut steps,
        "Promote after external DR verification",
        Some(format!("crab replica promote {name}")),
        "Change Crab's primary only after object-store, provider traffic, credentials, and application routing checks are complete.",
        true,
        false,
    );
    steps
}

fn replica_stale_runbook(replica: Option<&RunbookReplicaContext>) -> Vec<RunbookStep> {
    let mut steps = Vec::new();
    let name = runbook_replica_name(replica);
    push_runbook_step(
        &mut steps,
        "Force primary reads for the process",
        Some(
            "CRAB_REPLICA_READ_POLICY=prefer-primary crab replica status --deep --json".to_owned(),
        ),
        "Keep this incident command on the primary while measuring replica lag with live checks.",
        false,
        false,
    );
    push_runbook_step(
        &mut steps,
        "Disable replica reads",
        Some(format!("crab replica disable {name}")),
        "Stop future local read routing to the stale replica until readiness is proven again.",
        false,
        false,
    );
    push_runbook_step(
        &mut steps,
        "Verify replica object completeness",
        Some(format!("crab replica verify --deep --name {name}")),
        "Check whether lag is only manifest generation delay or missing referenced objects.",
        false,
        false,
    );
    push_runbook_step(
        &mut steps,
        "Re-enable reads after proof",
        Some(format!("crab replica wait {name} --enable-read")),
        "Only restore read routing after live readiness and backfill gates pass.",
        false,
        false,
    );
    steps
}

fn failed_backfill_runbook(
    replica: Option<&RunbookReplicaContext>,
    primary: Option<&str>,
) -> Vec<RunbookStep> {
    let mut steps = Vec::new();
    let name = runbook_replica_name(replica);
    push_runbook_step(
        &mut steps,
        "Inspect provider backfill state",
        Some(format!("crab replica backfill status --name {name} --json")),
        "Identify the provider-native backfill check, progress, and remediation before read cutover.",
        false,
        false,
    );
    push_runbook_step(
        &mut steps,
        "Export provider plan for drift review",
        Some(format!(
            "crab replica export --name {name} --format terraform"
        )),
        "Compare Crab-owned provider resources before mutating cloud control-plane state.",
        false,
        false,
    );
    if let (Some(primary), Some(replica)) = (primary, replica) {
        push_runbook_step(
            &mut steps,
            "Re-apply Crab-managed provider resources",
            Some(runbook_replica_apply_command(primary, replica)),
            "Repair missing Crab-managed provider resources after reviewing drift and provider permissions.",
            false,
            false,
        );
    }
    push_runbook_step(
        &mut steps,
        "Verify object readiness",
        Some(format!("crab replica verify --deep --name {name}")),
        "Backfill completion is not enough; Crab must also prove manifest-referenced objects are present.",
        false,
        false,
    );
    push_runbook_step(
        &mut steps,
        "Enable reads only after backfill proof",
        Some(format!("crab replica wait {name} --enable-read")),
        "The wait gate refuses read enablement until provider backfill and object readiness are verified.",
        false,
        false,
    );
    steps
}

fn policy_drift_runbook(
    replica: Option<&RunbookReplicaContext>,
    primary: Option<&str>,
) -> Vec<RunbookStep> {
    let mut steps = Vec::new();
    let name = runbook_replica_name(replica);
    push_runbook_step(
        &mut steps,
        "Collect drift findings",
        Some("crab replica doctor --deep --fix-plan --json".to_owned()),
        "Use live readiness and provider control-plane checks to identify blocking drift.",
        false,
        false,
    );
    push_runbook_step(
        &mut steps,
        "Export intended provider state",
        Some(format!(
            "crab replica export --name {name} --format terraform"
        )),
        "Review Crab-managed resources and shared bucket/account state before applying changes.",
        false,
        false,
    );
    if let (Some(primary), Some(replica)) = (primary, replica) {
        push_runbook_step(
            &mut steps,
            "Apply Crab-managed drift repair",
            Some(runbook_replica_apply_command(primary, replica)),
            "Create missing Crab-managed resources only after drift review; Crab refuses uninspected or wrong-replica mutations.",
            false,
            false,
        );
    }
    push_runbook_step(
        &mut steps,
        "Re-run doctor after repair",
        Some("crab replica doctor --deep --fix-plan".to_owned()),
        "Confirm provider drift is resolved before enabling or promoting replica reads.",
        false,
        false,
    );
    steps
}

fn destination_writes_runbook(
    replication: Option<&crate::replication::ReplicationConfig>,
    replica: Option<&RunbookReplicaContext>,
) -> Vec<RunbookStep> {
    let mut steps = Vec::new();
    let name = runbook_replica_name(replica);
    push_runbook_step(
        &mut steps,
        "Stop using the destination for reads",
        Some(format!("crab replica disable {name}")),
        "Prevent clients from reading a destination that may contain unauthorized or conflicting writes.",
        false,
        false,
    );
    push_runbook_step(
        &mut steps,
        "Force primary reads while investigating",
        Some(
            "CRAB_REPLICA_READ_POLICY=prefer-primary crab replica doctor --deep --fix-plan"
                .to_owned(),
        ),
        "Keep incident diagnostics on primary-authoritative state while collecting drift and readiness findings.",
        false,
        false,
    );
    if replication.is_some_and(crate::replication::ReplicationConfig::is_active_active) {
        push_runbook_step(
            &mut steps,
            "Compare regional manifests with coordinator truth",
            Some("crab replica repair --from-coordinator --dry-run --json".to_owned()),
            "Active-active recovery must reconcile regional manifests from linearizable coordinator records.",
            false,
            false,
        );
    }
    push_runbook_step(
        &mut steps,
        "Identify non-Crab destination objects externally",
        None,
        "Use provider inventory/audit logs to separate unauthorized destination writes from Crab-owned immutable objects before cleanup.",
        true,
        false,
    );
    push_runbook_step(
        &mut steps,
        "Avoid bucket-wide Crab GC during the incident",
        None,
        "Bucket-wide cleanup can delete shared .crab/ objects for other repositories; use scoped provider cleanup after inventory review.",
        true,
        true,
    );
    push_runbook_step(
        &mut steps,
        "Verify replica after cleanup",
        Some(format!("crab replica verify --deep --name {name}")),
        "Restore confidence by proving the replica generation and referenced objects match Crab's publication boundary.",
        false,
        false,
    );
    steps
}

fn runbook_replica_name(replica: Option<&RunbookReplicaContext>) -> String {
    replica.map_or_else(|| "<name>".to_owned(), |replica| shell_arg(&replica.name))
}

fn runbook_replica_apply_command(primary: &str, replica: &RunbookReplicaContext) -> String {
    let replica_config = ReplicaConfig {
        name: replica.name.clone(),
        provider: replica.provider,
        url: replica.url.clone(),
        region: replica.region.clone(),
        backfill: replica.backfill_configured,
        read: replica.read_enabled,
        rpo: replica.rpo,
    };
    replica_apply_command(primary, &replica_config)
}

fn push_runbook_step(
    steps: &mut Vec<RunbookStep>,
    title: &str,
    command: Option<String>,
    rationale: &str,
    requires_external_verification: bool,
    destructive: bool,
) {
    steps.push(RunbookStep {
        order: steps.len() as u64 + 1,
        title: title.to_owned(),
        command,
        rationale: rationale.to_owned(),
        requires_external_verification,
        destructive,
    });
}

fn repair_payload_from_coordinator_plan(
    dry_run: bool,
    coordinator_plan: Option<ActiveActiveRepairPlan>,
    blocked_reason: Option<String>,
) -> RepairPayload {
    let planned_actions = match coordinator_plan.as_ref() {
        Some(plan) if plan.actions.is_empty() => {
            vec!["coordinator snapshot has no regional materialization gaps".to_owned()]
        }
        Some(plan) => plan
            .actions
            .iter()
            .map(|action| {
                format!(
                    "materialize manifest generation {} for region {} via writer {} from {} (operation {})",
                    action.manifest_generation,
                    action.region,
                    action.writer.name,
                    action.source_region,
                    action.operation_id
                )
            })
            .collect(),
        None => vec![
            "read committed refs and manifest generation from the coordinator".to_owned(),
            "compare each writer region's manifest with coordinator truth".to_owned(),
            "build a per-region repair plan from configured active-active writers".to_owned(),
            "materialize missing regional manifests after referenced objects are present".to_owned(),
        ],
    };
    RepairPayload {
        from_coordinator: true,
        dry_run,
        planned_actions,
        coordinator_plan,
        blocked_reason,
    }
}

fn verify_failure_reason(replicas: &[ReplicaStatus]) -> Option<String> {
    let failures = replicas
        .iter()
        .filter(|replica| !replica.ready)
        .map(|replica| {
            format!(
                "{}: {}",
                replica.name,
                replica
                    .last_fallback_reason
                    .as_deref()
                    .unwrap_or("replica is not ready")
            )
        })
        .collect::<Vec<_>>();
    if failures.is_empty() {
        None
    } else {
        Some(format!(
            "replica verification failed ({})",
            failures.join("; ")
        ))
    }
}

fn verify_sample_size(args: &VerifyArgs) -> Result<Option<u64>> {
    let Some(sample_size) = args.sample_size else {
        return Ok(None);
    };
    if sample_size == 0 {
        return Err(CrabError::Configuration {
            key: "replica.verify.sample-size".into(),
            origin: "sample size must be greater than zero".into(),
        });
    }
    Ok(Some(sample_size))
}

fn verify_summary(
    replicas: &[ReplicaStatus],
    proof_mode: VerifyProofMode,
    sample_size: Option<u64>,
) -> VerifySummary {
    let replica_count = replicas.len() as u64;
    let ready_count = replicas.iter().filter(|replica| replica.ready).count() as u64;
    let read_enabled_count = replicas
        .iter()
        .filter(|replica| replica.read_enabled)
        .count() as u64;
    let mut cutover_blockers = replicas
        .iter()
        .filter(|replica| !replica.ready)
        .map(|replica| {
            let reason = replica
                .last_fallback_reason
                .as_deref()
                .unwrap_or("replica is not ready");
            format!("{}: {reason}", replica.name)
        })
        .collect::<Vec<_>>();
    if proof_mode == VerifyProofMode::Sampled {
        cutover_blockers.push(
            "verification used a bounded object sample; rerun with --exhaustive for cutover proof"
                .to_owned(),
        );
    }

    VerifySummary {
        proof_mode,
        exhaustive: proof_mode == VerifyProofMode::Exhaustive,
        sample_size,
        replica_count,
        ready_count,
        not_ready_count: replica_count.saturating_sub(ready_count),
        read_enabled_count,
        max_lag_generations: replicas
            .iter()
            .filter_map(|replica| replica.lag_generations)
            .max(),
        readiness_object_probe_count: replicas
            .iter()
            .map(|replica| replica.readiness_object_probe_count)
            .sum(),
        readiness_object_read_count: replicas
            .iter()
            .map(|replica| replica.readiness_object_read_count)
            .sum(),
        primary_fallback_bytes: replicas
            .iter()
            .map(|replica| replica.primary_fallback_bytes)
            .sum(),
        provider_inventory: verify_provider_inventory(replicas),
        cutover_ready: proof_mode == VerifyProofMode::Exhaustive
            && !replicas.is_empty()
            && cutover_blockers.is_empty(),
        cutover_blockers,
    }
}

fn verify_provider_inventory(replicas: &[ReplicaStatus]) -> Vec<VerifyProviderSummary> {
    let mut providers = Vec::<VerifyProviderSummary>::new();
    for replica in replicas {
        let Some(summary) = providers
            .iter_mut()
            .find(|summary| summary.provider == replica.provider)
        else {
            providers.push(VerifyProviderSummary {
                provider: replica.provider,
                replica_count: 1,
                ready_count: u64::from(replica.ready),
                not_ready_count: u64::from(!replica.ready),
                regions: vec![replica.region.clone()],
            });
            continue;
        };
        summary.replica_count = summary.replica_count.saturating_add(1);
        if replica.ready {
            summary.ready_count = summary.ready_count.saturating_add(1);
        } else {
            summary.not_ready_count = summary.not_ready_count.saturating_add(1);
        }
        if !summary.regions.contains(&replica.region) {
            summary.regions.push(replica.region.clone());
        }
    }

    for summary in &mut providers {
        let regions = summary.regions.drain(..).collect::<BTreeSet<_>>();
        summary.regions = regions.into_iter().collect();
    }
    providers.sort_by_key(|summary| summary.provider.to_string());
    providers
}

fn prometheus_status(payload: &StatusPayload) -> String {
    let mut out = String::new();
    out.push_str(
        "# HELP crab_replica_ready Replica manifest and referenced objects are ready for reads.\n",
    );
    out.push_str("# TYPE crab_replica_ready gauge\n");
    out.push_str(
        "# HELP crab_replica_read_enabled Replica is enabled for read routing in local config.\n",
    );
    out.push_str("# TYPE crab_replica_read_enabled gauge\n");
    out.push_str(
        "# HELP crab_replica_generation_lag Primary generation minus replica generation.\n",
    );
    out.push_str("# TYPE crab_replica_generation_lag gauge\n");
    out.push_str("# HELP crab_replica_primary_generation Primary manifest generation observed during status.\n");
    out.push_str("# TYPE crab_replica_primary_generation gauge\n");
    out.push_str(
        "# HELP crab_replica_generation Replica manifest generation observed during status.\n",
    );
    out.push_str("# TYPE crab_replica_generation gauge\n");
    out.push_str(
        "# HELP crab_replica_selected_total Locally recorded read selections for this replica.\n",
    );
    out.push_str("# TYPE crab_replica_selected_total counter\n");
    out.push_str(
        "# HELP crab_replica_fallback_total Locally recorded primary fallbacks for this replica.\n",
    );
    out.push_str("# TYPE crab_replica_fallback_total counter\n");
    out.push_str(
        "# HELP crab_replica_primary_fallback_bytes_total Bytes read from primary after this replica fell back.\n",
    );
    out.push_str("# TYPE crab_replica_primary_fallback_bytes_total counter\n");
    out.push_str(
        "# HELP crab_replica_readiness_cache_hit Replica readiness result came from local cache.\n",
    );
    out.push_str("# TYPE crab_replica_readiness_cache_hit gauge\n");
    out.push_str("# HELP crab_replica_readiness_latency_ms Replica readiness check wall-clock latency in milliseconds.\n");
    out.push_str("# TYPE crab_replica_readiness_latency_ms gauge\n");
    out.push_str("# HELP crab_replica_readiness_object_probe_total Replica readiness object HEAD probes performed during status.\n");
    out.push_str("# TYPE crab_replica_readiness_object_probe_total gauge\n");
    out.push_str("# HELP crab_replica_readiness_object_read_total Replica readiness object reads performed during status.\n");
    out.push_str("# TYPE crab_replica_readiness_object_read_total gauge\n");
    out.push_str("# HELP crab_replica_last_fallback_timestamp_ms Last locally recorded fallback timestamp in Unix milliseconds.\n");
    out.push_str("# TYPE crab_replica_last_fallback_timestamp_ms gauge\n");
    out.push_str("# HELP crab_replica_last_selected_timestamp_ms Last locally recorded replica selection timestamp in Unix milliseconds.\n");
    out.push_str("# TYPE crab_replica_last_selected_timestamp_ms gauge\n");
    out.push_str(
        "# HELP crab_replica_last_fallback_class Last locally recorded fallback reason class.\n",
    );
    out.push_str("# TYPE crab_replica_last_fallback_class gauge\n");
    out.push_str("# HELP crab_replica_health_state Alert-friendly derived replica health state.\n");
    out.push_str("# TYPE crab_replica_health_state gauge\n");
    out.push_str(
        "# HELP crab_replica_backfill_state Provider backfill state for historical objects.\n",
    );
    out.push_str("# TYPE crab_replica_backfill_state gauge\n");
    out.push_str(
        "# HELP crab_replica_backfill_blocks_read_enable Backfill state blocks read enablement.\n",
    );
    out.push_str("# TYPE crab_replica_backfill_blocks_read_enable gauge\n");
    out.push_str(
        "# HELP crab_replica_backfill_progress_percent Provider-reported backfill progress percentage.\n",
    );
    out.push_str("# TYPE crab_replica_backfill_progress_percent gauge\n");
    out.push_str("# HELP crab_replica_control_plane_backend_available Provider control-plane status backend is available.\n");
    out.push_str("# TYPE crab_replica_control_plane_backend_available gauge\n");
    out.push_str("# HELP crab_replica_control_plane_checked_drift Provider control-plane status inspected drift.\n");
    out.push_str("# TYPE crab_replica_control_plane_checked_drift gauge\n");
    out.push_str(
        "# HELP crab_replica_control_plane_check_ok Provider control-plane check is verified.\n",
    );
    out.push_str("# TYPE crab_replica_control_plane_check_ok gauge\n");

    for replica in &payload.replicas {
        let labels = replica_metric_labels(replica);
        push_prometheus_metric(
            &mut out,
            "crab_replica_ready",
            &labels,
            bool_metric(replica.ready),
        );
        push_prometheus_metric(
            &mut out,
            "crab_replica_read_enabled",
            &labels,
            bool_metric(replica.read_enabled),
        );
        push_optional_prometheus_metric(
            &mut out,
            "crab_replica_generation_lag",
            &labels,
            replica.lag_generations,
        );
        push_optional_prometheus_metric(
            &mut out,
            "crab_replica_primary_generation",
            &labels,
            replica.primary_generation,
        );
        push_optional_prometheus_metric(
            &mut out,
            "crab_replica_generation",
            &labels,
            replica.replica_generation,
        );
        push_prometheus_metric(
            &mut out,
            "crab_replica_selected_total",
            &labels,
            replica.selected_count,
        );
        push_prometheus_metric(
            &mut out,
            "crab_replica_fallback_total",
            &labels,
            replica.fallback_count,
        );
        push_prometheus_metric(
            &mut out,
            "crab_replica_primary_fallback_bytes_total",
            &labels,
            replica.primary_fallback_bytes,
        );
        push_prometheus_metric(
            &mut out,
            "crab_replica_readiness_cache_hit",
            &labels,
            bool_metric(replica.readiness_cache_hit),
        );
        push_optional_prometheus_metric(
            &mut out,
            "crab_replica_readiness_latency_ms",
            &labels,
            replica.readiness_check_latency_ms,
        );
        push_prometheus_metric(
            &mut out,
            "crab_replica_readiness_object_probe_total",
            &labels,
            replica.readiness_object_probe_count,
        );
        push_prometheus_metric(
            &mut out,
            "crab_replica_readiness_object_read_total",
            &labels,
            replica.readiness_object_read_count,
        );
        push_optional_prometheus_metric(
            &mut out,
            "crab_replica_last_fallback_timestamp_ms",
            &labels,
            replica.last_fallback_at_ms,
        );
        push_optional_prometheus_metric(
            &mut out,
            "crab_replica_last_selected_timestamp_ms",
            &labels,
            replica.last_selected_at_ms,
        );
        for class in ReplicaFallbackClass::ALL {
            let labels = [
                ("replica", replica.name.as_str()),
                ("provider", replica.provider.as_str()),
                ("region", replica.region.as_str()),
                ("class", class.as_str()),
            ];
            push_prometheus_metric(
                &mut out,
                "crab_replica_last_fallback_class",
                &labels,
                bool_metric(replica.last_fallback_class == Some(class)),
            );
        }
    }

    for health in &payload.health {
        for state in ReplicaHealthState::ALL {
            let labels = [
                ("replica", health.name.as_str()),
                ("provider", health.provider.as_str()),
                ("region", health.region.as_str()),
                ("state", state.as_str()),
            ];
            push_prometheus_metric(
                &mut out,
                "crab_replica_health_state",
                &labels,
                bool_metric(health.state == state),
            );
        }
    }

    for backfill in &payload.backfill {
        let labels = [
            ("replica", backfill.name.as_str()),
            ("provider", backfill.provider.as_str()),
            ("region", backfill.region.as_str()),
        ];
        push_prometheus_metric(
            &mut out,
            "crab_replica_backfill_blocks_read_enable",
            &labels,
            bool_metric(backfill.blocks_read_enable),
        );
        push_optional_prometheus_metric(
            &mut out,
            "crab_replica_backfill_progress_percent",
            &labels,
            backfill.progress_percent.map(u64::from),
        );
        for state in BackfillState::ALL {
            let labels = [
                ("replica", backfill.name.as_str()),
                ("provider", backfill.provider.as_str()),
                ("region", backfill.region.as_str()),
                ("state", state.as_str()),
            ];
            push_prometheus_metric(
                &mut out,
                "crab_replica_backfill_state",
                &labels,
                bool_metric(backfill.state == state),
            );
        }
    }

    for status in &payload.control_plane {
        let labels = [
            ("replica", status.replica_name.as_str()),
            ("provider", status.provider.as_str()),
        ];
        push_prometheus_metric(
            &mut out,
            "crab_replica_control_plane_backend_available",
            &labels,
            bool_metric(status.backend_available),
        );
        push_prometheus_metric(
            &mut out,
            "crab_replica_control_plane_checked_drift",
            &labels,
            bool_metric(status.checked_drift),
        );
        for check in &status.checks {
            let labels = [
                ("replica", status.replica_name.as_str()),
                ("provider", status.provider.as_str()),
                ("code", check.code.as_str()),
                ("action", check.action.as_str()),
                ("state", check.state.as_str()),
            ];
            push_prometheus_metric(
                &mut out,
                "crab_replica_control_plane_check_ok",
                &labels,
                bool_metric(check.state == ControlPlaneCheckState::Verified),
            );
        }
    }

    out
}

fn replica_metric_labels(replica: &ReplicaStatus) -> [(&'static str, &str); 3] {
    [
        ("replica", replica.name.as_str()),
        ("provider", replica.provider.as_str()),
        ("region", replica.region.as_str()),
    ]
}

fn push_optional_prometheus_metric(
    out: &mut String,
    name: &str,
    labels: &[(&str, &str)],
    value: Option<u64>,
) {
    if let Some(value) = value {
        push_prometheus_metric(out, name, labels, value);
    }
}

fn push_prometheus_metric(out: &mut String, name: &str, labels: &[(&str, &str)], value: u64) {
    out.push_str(name);
    out.push('{');
    for (index, (key, value)) in labels.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push_str(key);
        out.push_str("=\"");
        out.push_str(&prometheus_label_value(value));
        out.push('"');
    }
    out.push_str("} ");
    out.push_str(&value.to_string());
    out.push('\n');
}

fn prometheus_label_value(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

fn bool_metric(value: bool) -> u64 {
    u64::from(value)
}

fn ensure_replica_promotable(replica: &ReplicaConfig, force: bool) -> Result<()> {
    if replica.read || force {
        return Ok(());
    }
    Err(CrabError::Configuration {
        key: format!("replication.replicas.{}.read", replica.name),
        origin: format!(
            "replica {} is not read-enabled; run crab replica verify --deep --name {} and crab replica wait {} --enable-read before promotion, or rerun with --force for emergency DR after external verification",
            replica.name, replica.name, replica.name
        ),
    })
}

fn promote_plan_checks(
    replica: &ReplicaConfig,
    old_primary: &str,
    new_primary_is_crab_url: bool,
    force: bool,
    control_plane: Option<&ControlPlaneStatus>,
) -> Vec<PromotePlanCheck> {
    let mut checks = Vec::new();
    checks.push(if new_primary_is_crab_url {
        promote_plan_check(
            "promote.url.write-safe",
            PromotePlanCheckState::Passed,
            "replica URL is a crab:// endpoint and can become the write primary",
            "no action required",
        )
    } else {
        promote_plan_check(
            "promote.url.write-safe",
            PromotePlanCheckState::Blocked,
            "replica URL is not a crab:// endpoint, so future pushes would not use Crab's write path",
            "configure a crab:// endpoint for the replica before non-plan promotion",
        )
    });

    checks.push(if replica.read {
        promote_plan_check(
            "promote.read-enabled",
            PromotePlanCheckState::Passed,
            "replica has local read-enabled proof",
            format!(
                "rerun {} for fresh object proof before promotion",
                replica_verify_command(&replica.name)
            ),
        )
    } else if force {
        promote_plan_check(
            "promote.read-enabled",
            PromotePlanCheckState::Warning,
            "promotion is forced without local read-enabled proof",
            format!(
                "prefer {} and crab replica wait {} --enable-read before promotion",
                replica_verify_command(&replica.name),
                shell_arg(&replica.name)
            ),
        )
    } else {
        promote_plan_check(
            "promote.read-enabled",
            PromotePlanCheckState::Blocked,
            "replica is not read-enabled",
            format!(
                "run {} and crab replica wait {} --enable-read before promotion",
                replica_verify_command(&replica.name),
                shell_arg(&replica.name)
            ),
        )
    });

    checks.push(promote_control_plane_check(
        replica,
        old_primary,
        control_plane,
    ));
    checks
}

fn promote_control_plane_check(
    replica: &ReplicaConfig,
    old_primary: &str,
    control_plane: Option<&ControlPlaneStatus>,
) -> PromotePlanCheck {
    let Some(status) = control_plane else {
        return promote_plan_check(
            "promote.provider-control-plane",
            PromotePlanCheckState::Warning,
            "provider control-plane status is unavailable for this replica",
            format!(
                "run crab replica status --deep and crab replica doctor --deep --fix-plan before promoting {}",
                shell_arg(&replica.name)
            ),
        );
    };
    if !status.backend_available || !status.checked_drift {
        return promote_plan_check(
            "promote.provider-control-plane",
            PromotePlanCheckState::Warning,
            "provider control-plane drift was not fully checked",
            format!(
                "run crab replica add {} --provider {} --primary {} --replica {} --region {} --apply or export an audit plan",
                shell_arg(&replica.name),
                replica.provider,
                shell_arg(old_primary),
                shell_arg(&replica.url),
                shell_arg(&replica.region)
            ),
        );
    }

    let blocking = status
        .checks
        .iter()
        .filter(|check| {
            matches!(
                check.state,
                ControlPlaneCheckState::Missing | ControlPlaneCheckState::Drifted
            )
        })
        .map(|check| check.code.as_str())
        .collect::<Vec<_>>();
    if !blocking.is_empty() {
        return promote_plan_check(
            "promote.provider-control-plane",
            PromotePlanCheckState::Blocked,
            format!(
                "provider control-plane has blocking drift: {}",
                blocking.join(", ")
            ),
            "repair provider replication state before promotion",
        );
    }

    let uncertain = status
        .checks
        .iter()
        .filter(|check| {
            matches!(
                check.state,
                ControlPlaneCheckState::Unknown | ControlPlaneCheckState::Unsupported
            )
        })
        .map(|check| check.code.as_str())
        .collect::<Vec<_>>();
    if !uncertain.is_empty() {
        return promote_plan_check(
            "promote.provider-control-plane",
            PromotePlanCheckState::Warning,
            format!(
                "provider control-plane has unverified checks: {}",
                uncertain.join(", ")
            ),
            "complete external provider verification before promotion",
        );
    }

    promote_plan_check(
        "promote.provider-control-plane",
        PromotePlanCheckState::Passed,
        "provider control-plane checks are verified",
        "no action required",
    )
}

fn promote_plan_check(
    code: &str,
    state: PromotePlanCheckState,
    message: impl Into<String>,
    remediation: impl Into<String>,
) -> PromotePlanCheck {
    PromotePlanCheck {
        code: code.to_owned(),
        state,
        message: message.into(),
        remediation: remediation.into(),
        blocking: state == PromotePlanCheckState::Blocked,
    }
}

fn promote_planned_actions(
    name: &str,
    new_primary_is_crab_url: bool,
    read_enabled: bool,
    force: bool,
) -> Vec<String> {
    let mut actions = vec![
        replica_verify_command(name),
        "crab replica doctor --deep --fix-plan".to_owned(),
    ];
    if !read_enabled {
        actions.push(format!(
            "crab replica wait {} --enable-read",
            shell_arg(name)
        ));
    }
    if new_primary_is_crab_url {
        let mut command = format!("crab replica promote {}", shell_arg(name));
        if force && !read_enabled {
            command.push_str(" --force");
        }
        actions.push(command);
    } else {
        actions.push("configure a crab:// replica endpoint before non-plan promotion".to_owned());
    }
    actions
}

fn set_primary_plan_checks(
    target_replica: Option<&ReplicaConfig>,
    old_primary: &str,
    new_primary_is_crab_url: bool,
    force: bool,
    control_plane: Option<&ControlPlaneStatus>,
) -> Vec<PromotePlanCheck> {
    let mut checks = Vec::new();
    checks.push(if new_primary_is_crab_url {
        promote_plan_check(
            "set-primary.url.write-safe",
            PromotePlanCheckState::Passed,
            "new primary URL is a crab:// endpoint and can receive future pushes",
            "no action required",
        )
    } else {
        promote_plan_check(
            "set-primary.url.write-safe",
            PromotePlanCheckState::Blocked,
            "new primary URL is not a crab:// endpoint, so future pushes would not use Crab's write path",
            "choose a crab:// primary endpoint before applying the DR change",
        )
    });

    match target_replica {
        Some(replica) => {
            checks.push(if replica.read {
                promote_plan_check(
                    "set-primary.read-enabled",
                    PromotePlanCheckState::Passed,
                    "configured target replica has local read-enabled proof",
                    format!(
                        "rerun {} for fresh object proof before changing the primary",
                        replica_verify_command(&replica.name)
                    ),
                )
            } else if force {
                promote_plan_check(
                    "set-primary.read-enabled",
                    PromotePlanCheckState::Warning,
                    "primary change is forced without local read-enabled proof",
                    format!(
                        "prefer {} and crab replica wait {} --enable-read before applying",
                        replica_verify_command(&replica.name),
                        shell_arg(&replica.name)
                    ),
                )
            } else {
                promote_plan_check(
                    "set-primary.read-enabled",
                    PromotePlanCheckState::Blocked,
                    "configured target replica is not read-enabled",
                    format!(
                        "run {} and crab replica wait {} --enable-read before applying",
                        replica_verify_command(&replica.name),
                        shell_arg(&replica.name)
                    ),
                )
            });
            let mut provider_check =
                promote_control_plane_check(replica, old_primary, control_plane);
            provider_check.code.clear();
            provider_check.code.push_str("set-primary.provider-control-plane");
            checks.push(provider_check);
        }
        None if force => checks.push(promote_plan_check(
            "set-primary.configured-target",
            PromotePlanCheckState::Warning,
            "new primary is not one of the configured replicas",
            "complete external provider, object-presence, DNS, and traffic verification before applying",
        )),
        None => checks.push(promote_plan_check(
            "set-primary.configured-target",
            PromotePlanCheckState::Blocked,
            "new primary is not one of the configured replicas, so Crab cannot prove readiness",
            "add and verify the target as a replica first, or rerun with --force after external DR verification",
        )),
    }
    checks
}

fn set_primary_planned_actions(
    new_primary: &str,
    target_replica: Option<&ReplicaConfig>,
    new_primary_is_crab_url: bool,
    force: bool,
) -> Vec<String> {
    let mut actions = Vec::new();
    if let Some(replica) = target_replica {
        actions.push(replica_verify_command(&replica.name));
        actions.push("crab replica doctor --deep --fix-plan".to_owned());
        if !replica.read {
            actions.push(replica_wait_command(&replica.name));
        }
    } else {
        actions.push(
            "externally verify target object store, provider replication state, DNS, and application traffic routing"
                .to_owned(),
        );
        actions.push("crab replica doctor --deep --fix-plan".to_owned());
    }

    if new_primary_is_crab_url {
        let mut command = format!("crab replica set-primary {}", shell_arg(new_primary));
        if force {
            command.push_str(" --force");
        }
        command.push_str(" --apply");
        actions.push(command);
    } else {
        actions.push("choose a crab:// endpoint before applying set-primary".to_owned());
    }
    actions
}

fn ensure_set_primary_applicable(
    target_replica: Option<&ReplicaConfig>,
    new_primary_is_crab_url: bool,
    force: bool,
    plan_ready: bool,
) -> Result<()> {
    if !new_primary_is_crab_url {
        return Err(CrabError::Configuration {
            key: "replica.set-primary.url".into(),
            origin: "set-primary requires a crab:// URL so writes remain on Crab's write path"
                .into(),
        });
    }
    match target_replica {
        Some(replica) => ensure_replica_promotable(replica, force)?,
        None if !force => {
            return Err(CrabError::Configuration {
                key: "replica.set-primary.target".into(),
                origin: "new primary is not a configured replica; add and verify it first, or rerun with --force after external DR verification".into(),
            });
        }
        None => {}
    }
    if !plan_ready {
        return Err(CrabError::Configuration {
            key: "replica.set-primary.plan".into(),
            origin: "set-primary has blocking plan checks; rerun without --apply for details"
                .into(),
        });
    }
    Ok(())
}

fn write_primary_to_project_config(
    path: &Path,
    config: &mut ProjectConfig,
    new_primary: &str,
) -> Result<()> {
    new_primary.clone_into(&mut config.remote.url);
    if let Some(replication) = config.replication.as_mut() {
        if let Some(primary) = replication.primary.as_mut() {
            new_primary.clone_into(primary);
        } else {
            replication.primary = Some(new_primary.to_owned());
        }
    }
    ProjectConfig::write(path, config)
}

fn finding(
    code: &str,
    severity: DoctorSeverity,
    message: impl Into<String>,
    replica: Option<String>,
    remediation: Option<String>,
) -> DoctorFinding {
    DoctorFinding {
        code: code.to_owned(),
        severity,
        message: message.into(),
        replica,
        remediation,
    }
}

fn add_replica_to_project_config(
    path: &Path,
    args: &AddArgs,
    provider: ReplicationProviderKind,
    rpo: ReplicationRpo,
) -> Result<()> {
    let mut config = load_project_config_or_default(path, &args.primary)?;
    if config.remote.url != args.primary {
        return Err(CrabError::Configuration {
            key: "replication.primary".into(),
            origin: format!(
                "primary {} does not match existing remote {}",
                args.primary, config.remote.url
            ),
        });
    }

    let replica = ReplicaConfig {
        name: args.name.clone(),
        provider,
        url: args.replica.clone(),
        region: args.region.clone(),
        backfill: args.backfill,
        read: false,
        rpo,
    };

    let mut replication = config.replication.take().unwrap_or_default();
    replication.primary = Some(args.primary.clone());
    match replication
        .replicas
        .iter_mut()
        .find(|existing| existing.name == args.name)
    {
        Some(existing) => *existing = replica,
        None => replication.replicas.push(replica),
    }
    config.replication = Some(replication);
    ProjectConfig::write(path, &config)
}

fn set_replica_read_enabled(path: &Path, name: &str, enabled: bool) -> Result<bool> {
    let mut config = ProjectConfig::load(path)?;
    let replication = config
        .replication
        .as_mut()
        .ok_or_else(|| CrabError::Configuration {
            key: "replication".into(),
            origin: "replication is not configured".into(),
        })?;
    let replica = replication
        .replicas
        .iter_mut()
        .find(|replica| replica.name == name)
        .ok_or_else(|| CrabError::Configuration {
            key: "replication.replicas".into(),
            origin: format!("replica {name} is not configured"),
        })?;
    let changed = replica.read != enabled;
    replica.read = enabled;
    if changed {
        ProjectConfig::write(path, &config)?;
    }
    Ok(changed)
}

fn remove_replica_from_project_config(path: &Path, name: &str) -> Result<bool> {
    let mut config = ProjectConfig::load(path)?;
    let Some(replication) = config.replication.as_mut() else {
        return Ok(false);
    };
    let before = replication.replicas.len();
    replication.replicas.retain(|replica| replica.name != name);
    let removed = replication.replicas.len() != before;
    if removed {
        ProjectConfig::write(path, &config)?;
    }
    Ok(removed)
}

fn load_project_config_or_default(path: &Path, primary: &str) -> Result<ProjectConfig> {
    if path.is_file() {
        return ProjectConfig::load(path);
    }
    Ok(ProjectConfig {
        remote: RemoteConfig {
            url: primary.to_owned(),
        },
        track: None,
        hydrate: None,
        mirror: None,
        replication: None,
        auth: None,
    })
}

fn resolved_replication_context(root: &Path) -> Result<(Option<String>, Config)> {
    let mut config = Config::resolve_local().unwrap_or_default();
    let project_path = project_config_path(root);
    if project_path.is_file() {
        let project = ProjectConfig::load(&project_path)?;
        config.remote_url = Some(project.remote.url.clone());
        if let Some(replication) = project.replication {
            config.replication = Some(replication);
        }
    }
    let primary = config
        .replication
        .as_ref()
        .and_then(|replication| replication.primary.clone())
        .or_else(|| config.remote_url.clone());
    Ok((primary, config))
}

fn select_configured_replica<'a>(
    config: &'a Config,
    name: Option<&str>,
) -> Result<&'a ReplicaConfig> {
    let replication = config
        .replication
        .as_ref()
        .ok_or_else(|| CrabError::Configuration {
            key: "replication".into(),
            origin: "replication is not configured".into(),
        })?;
    if let Some(name) = name {
        return replication
            .replicas
            .iter()
            .find(|replica| replica.name == name)
            .ok_or_else(|| CrabError::Configuration {
                key: "replication.replicas".into(),
                origin: format!("replica {name} is not configured"),
            });
    }
    match replication.replicas.as_slice() {
        [replica] => Ok(replica),
        [] => Err(CrabError::Configuration {
            key: "replication.replicas".into(),
            origin: "no replicas are configured".into(),
        }),
        _ => Err(CrabError::Configuration {
            key: "replica export --name".into(),
            origin: "multiple replicas are configured; choose one with --name".into(),
        }),
    }
}

fn parse_writer_spec(raw: &str) -> Result<WriterConfig> {
    let mut parts = raw.split(',');
    let first = parts.next().ok_or_else(|| CrabError::Configuration {
        key: "replication.writers".into(),
        origin: "writer spec must not be empty".into(),
    })?;
    let (name, url) = first
        .split_once('=')
        .ok_or_else(|| CrabError::Configuration {
            key: "replication.writers".into(),
            origin: format!("writer spec {raw:?} must start with name=url"),
        })?;

    let mut region = None;
    let mut enabled = true;
    for part in parts {
        let (key, value) = part
            .split_once('=')
            .ok_or_else(|| CrabError::Configuration {
                key: "replication.writers".into(),
                origin: format!("writer spec segment {part:?} must be key=value"),
            })?;
        match key {
            "region" => region = Some(value.to_owned()),
            "enabled" => {
                enabled = match value {
                    "true" => true,
                    "false" => false,
                    other => {
                        return Err(CrabError::Configuration {
                            key: "replication.writers.enabled".into(),
                            origin: format!("unsupported writer enabled value {other:?}"),
                        });
                    }
                };
            }
            other => {
                return Err(CrabError::Configuration {
                    key: "replication.writers".into(),
                    origin: format!("unsupported writer spec key {other:?}"),
                });
            }
        }
    }

    Ok(WriterConfig {
        name: name.to_owned(),
        url: url.to_owned(),
        region: region.ok_or_else(|| CrabError::Configuration {
            key: "replication.writers.region".into(),
            origin: format!("writer {name} requires region=..."),
        })?,
        enabled,
    })
}

fn render_add(payload: &AddPayload, mode: OutputMode) {
    if mode == OutputMode::Json {
        emit_json(SCHEMA, SCHEMA_VERSION, payload);
        return;
    }

    if payload.configured {
        println!(
            "Configured replica metadata for {}",
            payload.plan.setup.replica
        );
    } else {
        println!("Replica setup plan for {}", payload.plan.setup.replica);
    }
    println!("Provider: {}", payload.plan.setup.provider);
    println!("RPO: {}", payload.plan.setup.rpo);
    for action in &payload.plan.setup.actions {
        let mode = if action.automated { "auto" } else { "manual" };
        let required = if action.required {
            "required"
        } else {
            "optional"
        };
        println!("  - [{required}/{mode}] {}", action.description);
    }
    if !payload.plan.requests.is_empty() {
        println!("Cloud control-plane operations:");
        for request in &payload.plan.requests {
            println!("  - {} {}", request.action, request.target);
        }
    }
}

fn render_export(payload: &ExportPayload, mode: OutputMode) {
    if mode == OutputMode::Json {
        emit_json("replica.export", SCHEMA_VERSION, payload);
        return;
    }
    print!("{}", payload.body);
}

fn render_cost(payload: &CostPayload, mode: OutputMode) {
    if mode == OutputMode::Json {
        emit_json("replica.cost", SCHEMA_VERSION, payload);
        return;
    }
    println!("Replication cost estimate:");
    if let Some(primary) = payload.primary.as_ref() {
        println!("  primary: {primary}");
    }
    println!(
        "  assumptions: writes={} GB/month reads={} GB/month backfill={} GB requests={}M/month",
        cost_quantity(payload.assumptions.monthly_write_gb),
        cost_quantity(payload.assumptions.monthly_read_gb),
        cost_quantity(payload.assumptions.backfill_gb),
        cost_quantity(payload.assumptions.monthly_requests_million)
    );
    for notice in &payload.pricing_notice {
        println!("  pricing: {notice}");
    }
    for estimate in &payload.estimates {
        println!(
            "Replica '{}' provider={} region={} rpo={} read-enabled={} backfill={}",
            estimate.name,
            estimate.provider,
            estimate.region,
            estimate.rpo,
            estimate.read_enabled,
            estimate.backfill_configured
        );
        for meter in &estimate.meters {
            println!(
                "  - {}: {} {}",
                meter.code,
                cost_quantity(meter.quantity),
                meter.unit
            );
            println!("    price input: {}", meter.pricing_input);
        }
        for warning in &estimate.warnings {
            println!("    warning: {warning}");
        }
    }
    println!(
        "Totals: replicas={} replicated-writes={} GB/month replica-reads={} GB/month backfill={} GB requests={}M/month",
        payload.totals.replicas,
        cost_quantity(payload.totals.monthly_replicated_write_gb),
        cost_quantity(payload.totals.monthly_replica_read_gb),
        cost_quantity(payload.totals.one_time_backfill_gb),
        cost_quantity(payload.totals.monthly_request_millions)
    );
}

fn render_runbook(payload: &RunbookPayload, mode: OutputMode) {
    if mode == OutputMode::Json {
        emit_json("replica.runbook", SCHEMA_VERSION, payload);
        return;
    }
    println!("Replica runbook: {}", payload.scenario.as_str());
    if let Some(primary) = payload.primary.as_ref() {
        println!("  primary: {primary}");
    }
    if let Some(mode) = payload.mode {
        println!("  mode: {mode}");
    }
    if let Some(replica) = payload.replica.as_ref() {
        println!(
            "  replica: {} provider={} region={} read-enabled={} backfill={}",
            replica.name,
            replica.provider,
            replica.region,
            replica.read_enabled,
            replica.backfill_configured
        );
    }
    for warning in &payload.warnings {
        println!("  warning: {warning}");
    }
    for step in &payload.steps {
        println!("{}. {}", step.order, step.title);
        if let Some(command) = step.command.as_ref() {
            println!("   command: {command}");
        }
        println!("   why: {}", step.rationale);
        if step.requires_external_verification {
            println!("   gate: external verification required");
        }
        if step.destructive {
            println!("   risk: destructive if run against the wrong scope");
        }
    }
}

fn cost_quantity(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        format!("{value:.2}")
    }
}

fn render_coordinator(payload: &CoordinatorPayload, mode: OutputMode) {
    if mode == OutputMode::Json {
        emit_json("replica.coordinator", SCHEMA_VERSION, payload);
        return;
    }
    println!("Coordinator setup plan for {}", payload.plan.url);
    println!("Provider: {}", payload.plan.provider.as_str());
    println!("Region: {}", payload.plan.region);
    if !payload.plan.failover_regions.is_empty() {
        println!(
            "Failover regions: {}",
            payload.plan.failover_regions.join(", ")
        );
    }
    for request in &payload.plan.requests {
        println!("  - {} {}", request.action, request.target);
    }
}

fn render_coordinator_status_payload(payload: &CoordinatorStatusPayload, mode: OutputMode) {
    if mode == OutputMode::Json {
        emit_json("replica.coordinator.status", SCHEMA_VERSION, payload);
        return;
    }
    if payload.configured {
        println!("Configured coordinator:");
    } else {
        println!("Coordinator target:");
    }
    render_coordinator_status(&payload.status);
}

fn render_coordinator_remove(payload: &CoordinatorRemovePayload, mode: OutputMode) {
    if mode == OutputMode::Json {
        emit_json("replica.coordinator.remove", SCHEMA_VERSION, payload);
        return;
    }
    if payload.applied {
        println!("Coordinator remove applied for {}", payload.plan.url);
    } else {
        println!("Coordinator remove plan for {}", payload.plan.url);
        println!("No cloud resources or local config were changed; rerun with --apply to remove.");
    }
    if payload.removed_config {
        println!("Removed active-active coordinator config and disabled writer regions");
    }
    for request in &payload.plan.requests {
        println!("  - {} {}", request.action, request.target);
    }
}

fn render_wait(payload: &WaitPayload, mode: OutputMode) {
    if mode == OutputMode::Json {
        emit_json("replica.wait", SCHEMA_VERSION, payload);
        return;
    }
    let state = if payload.ready { "ready" } else { "not-ready" };
    println!(
        "Replica {} is {} (read={})",
        payload.name, state, payload.read_enabled
    );
    if let Some(reason) = payload.reason.as_ref() {
        println!("  reason: {reason}");
    }
}

fn render_verify(payload: &VerifyPayload, mode: OutputMode) {
    if mode == OutputMode::Json {
        emit_json("replica.verify", SCHEMA_VERSION, payload);
        return;
    }
    let state = if payload.verified {
        "verified"
    } else {
        "failed"
    };
    println!("Replica verification {state}");
    println!("Primary: {}", payload.primary);
    match payload.sample_size {
        Some(sample_size) => println!(
            "Proof: {} sample-size={} cutover-ready={}",
            payload.proof_mode.as_str(),
            sample_size,
            payload.summary.cutover_ready
        ),
        None => println!(
            "Proof: {} cutover-ready={}",
            payload.proof_mode.as_str(),
            payload.summary.cutover_ready
        ),
    }
    println!(
        "Summary: ready={}/{} not-ready={} read-enabled={} max-lag={:?}",
        payload.summary.ready_count,
        payload.summary.replica_count,
        payload.summary.not_ready_count,
        payload.summary.read_enabled_count,
        payload.summary.max_lag_generations
    );
    println!(
        "Readiness objects: probes={} reads={} primary-fallback-bytes={}",
        payload.summary.readiness_object_probe_count,
        payload.summary.readiness_object_read_count,
        payload.summary.primary_fallback_bytes
    );
    if !payload.summary.provider_inventory.is_empty() {
        println!("Provider inventory:");
        for provider in &payload.summary.provider_inventory {
            println!(
                "  {} replicas={} ready={} not-ready={} regions={}",
                provider.provider,
                provider.replica_count,
                provider.ready_count,
                provider.not_ready_count,
                provider.regions.join(",")
            );
        }
    }
    if !payload.summary.cutover_blockers.is_empty() {
        println!("Cutover blockers:");
        for blocker in &payload.summary.cutover_blockers {
            println!("  - {blocker}");
        }
    }
    for replica in &payload.replicas {
        let state = if replica.ready { "ready" } else { "not-ready" };
        println!(
            "  {} [{}] {} gen={:?} lag={:?}",
            replica.name,
            replica.provider,
            state,
            replica.replica_generation,
            replica.lag_generations
        );
        if let Some(reason) = replica.last_fallback_reason.as_ref() {
            println!("    reason: {reason}");
        }
    }
}

fn render_backfill_status(payload: &BackfillPayload, mode: OutputMode) {
    if mode == OutputMode::Json {
        emit_json("replica.backfill", SCHEMA_VERSION, payload);
        return;
    }
    println!("Primary: {}", payload.primary);
    if payload.replicas.is_empty() {
        println!("Backfill: none");
        return;
    }
    for replica in &payload.replicas {
        println!(
            "{} [{}] backfill={} read={} blocks-read-enable={}",
            replica.name,
            replica.provider,
            replica.state.as_str(),
            replica.read_enabled,
            replica.blocks_read_enable
        );
        if let Some(code) = replica.check_code.as_ref() {
            println!("  check: {code}");
        }
        if let Some(progress) = replica.progress_percent {
            println!("  progress: {progress}%");
        }
        println!("  status: {}", replica.message);
        if let Some(remediation) = replica.remediation.as_ref() {
            println!("  fix: {remediation}");
        }
    }
}

fn render_toggle(payload: &TogglePayload, mode: OutputMode, label: &str) {
    if mode == OutputMode::Json {
        emit_json("replica.toggle", SCHEMA_VERSION, payload);
        return;
    }
    let state = if payload.enabled {
        "enabled"
    } else {
        "disabled"
    };
    let changed = if payload.changed {
        "updated"
    } else {
        "unchanged"
    };
    println!("{label} '{}' {state} ({changed})", payload.name);
}

fn render_mode(payload: &ModePayload, mode: OutputMode) {
    if mode == OutputMode::Json {
        emit_json("replica.mode", SCHEMA_VERSION, payload);
        return;
    }
    println!("Replication mode: {}", payload.mode);
    if let Some(reason) = payload.active_active.reason.as_ref() {
        println!("Active-active writes: blocked ({reason})");
    } else {
        println!("Active-active writes: ready");
    }
}

fn render_writers(payload: &WritersPayload, mode: OutputMode) {
    if mode == OutputMode::Json {
        emit_json("replica.writers", SCHEMA_VERSION, payload);
        return;
    }
    if payload.writers.is_empty() {
        println!("Writers: none");
        return;
    }
    for writer in &payload.writers {
        let state = if writer.enabled {
            "enabled"
        } else {
            "disabled"
        };
        println!(
            "{} [{}] {} {}",
            writer.name, writer.region, state, writer.url
        );
    }
}

fn render_failover(payload: &FailoverPayload, mode: OutputMode) {
    if mode == OutputMode::Json {
        emit_json("replica.failover", SCHEMA_VERSION, payload);
        return;
    }
    println!("Mode: {}", payload.active_active.mode);
    println!(
        "Coordinator configured: {}",
        payload.active_active.coordinator_configured
    );
    println!(
        "Coordinator ready: {}",
        payload.active_active.coordinator_ready
    );
    println!("Writes enabled: {}", payload.active_active.writes_enabled);
    println!("Enabled writers: {}", payload.active_active.enabled_writers);
    if let Some(reason) = payload.active_active.reason.as_ref() {
        println!("Reason: {reason}");
    }
    render_failover_automation_policy(&payload.automation_policy);
    render_failover_automation_decision(&payload.automation_plan);
    if let Some(health) = payload.coordinator_health.as_ref() {
        println!("Coordinator epoch: {}", health.epoch);
        if let Some(summary) = health.state_summary.as_ref() {
            let max_state_bytes = summary
                .max_state_bytes
                .map_or_else(|| "unbounded".to_owned(), |bytes| bytes.to_string());
            println!(
                "Coordinator state: transactions={} completed_operations={}/{} state_bytes={}/{}",
                summary.transaction_count,
                summary.completed_operation_count,
                summary.max_completed_operations,
                summary.state_bytes,
                max_state_bytes
            );
        }
    }
    if let Some(coordinator) = payload.coordinator.as_ref() {
        render_coordinator_status(coordinator);
    }
}

fn render_failover_plan(payload: &FailoverPayload, mode: OutputMode) {
    if mode == OutputMode::Json {
        emit_json("replica.failover.plan", SCHEMA_VERSION, payload);
        return;
    }
    render_failover(payload, mode);
}

fn render_failover_run(payload: &FailoverRunPayload, mode: OutputMode) {
    if mode == OutputMode::Json {
        emit_json("replica.failover.run", SCHEMA_VERSION, payload);
        return;
    }
    println!("Apply requested: {}", payload.apply_requested);
    println!("Applied: {}", payload.applied);
    println!("Mode: {}", payload.active_active.mode);
    println!("Writes enabled: {}", payload.active_active.writes_enabled);
    render_failover_automation_policy(&payload.automation_policy);
    render_failover_automation_decision(&payload.automation_plan);
    if let Some(reason) = payload.blocked_reason.as_ref() {
        println!("Blocked: {reason}");
    }
    if let Some(operation) = payload.operation.as_ref() {
        println!("Applied operation: {}", operation.operation.as_str());
        if let Some(outcome) = operation.outcome.as_ref() {
            println!(
                "Epoch: {} -> {}",
                outcome.previous_epoch, outcome.coordinator_epoch
            );
        }
    }
    if let Some(repair) = payload.repair.as_ref()
        && let Some(plan) = repair.coordinator_plan.as_ref()
    {
        println!("Repair actions: {}", plan.actions.len());
    }
}

fn render_failover_automation_policy(policy: &FailoverAutomationPolicy) {
    println!(
        "Automatic write failover: {}",
        policy.automatic_write_failover_supported
    );
    println!("Failover orchestration: {}", policy.orchestration);
    println!("Split-brain policy: {}", policy.split_brain_policy);
    println!("Failover ADR: {}", policy.adr);
}

fn render_failover_automation_decision(decision: &FailoverAutomationDecision) {
    println!("Failover plan action: {}", decision.action.as_str());
    println!(
        "Automatic apply supported: {}",
        decision.automatic_apply_supported
    );
    println!("Failover plan reason: {}", decision.reason);
    if !decision.unhealthy_writers.is_empty() {
        println!(
            "Unhealthy writers: {}",
            decision.unhealthy_writers.join(", ")
        );
    }
    println!("Repair verified: {}", decision.repair_verified);
    for command in &decision.commands {
        println!("Next command: {command}");
    }
}

fn render_failover_operation(payload: &FailoverOperationPayload, mode: OutputMode) {
    if mode == OutputMode::Json {
        emit_json("replica.failover.operation", SCHEMA_VERSION, payload);
        return;
    }
    println!("Failover operation: {}", payload.operation.as_str());
    println!("Applied: {}", payload.applied);
    println!("Repair verified: {}", payload.repair_verified);
    println!("Coordinator: {}", payload.coordinator_url);
    println!("Repo: {}", payload.repo_prefix);
    render_failover_automation_policy(&payload.automation_policy);
    if let Some(outcome) = payload.outcome.as_ref() {
        println!(
            "Epoch: {} -> {}",
            outcome.previous_epoch, outcome.coordinator_epoch
        );
        println!(
            "Healthy: {} -> {}",
            outcome.previous_healthy, outcome.healthy
        );
        println!("Changed: {}", outcome.changed);
        if let Some(reason) = outcome.reason.as_ref() {
            println!("Reason: {reason}");
        }
    } else {
        println!("Planned actions:");
        for action in &payload.planned_actions {
            println!("  - {action}");
        }
        println!("Rerun with --apply to mutate coordinator state.");
    }
}

fn render_repair(payload: &RepairPayload, mode: OutputMode) {
    if mode == OutputMode::Json {
        emit_json("replica.repair", SCHEMA_VERSION, payload);
        return;
    }
    if mode == OutputMode::Jsonl {
        let stdout = std::io::stdout();
        let mut stream = JsonlStream::new("replica.repair.event", SCHEMA_VERSION, stdout.lock());
        stream.emit_result(payload);
        return;
    }
    render_repair_text(payload);
}

#[derive(Debug, Serialize)]
struct RepairWatchPayload<'a> {
    sample: u64,
    interval_seconds: u64,
    worker: &'a RepairWatchWorkerState,
    repair: &'a RepairPayload,
}

fn render_repair_watch_snapshot(
    payload: &RepairPayload,
    mode: OutputMode,
    sample: u64,
    interval_seconds: u64,
    worker: &RepairWatchWorkerState,
) {
    if mode == OutputMode::Jsonl {
        let stdout = std::io::stdout();
        let mut stream = JsonlStream::new("replica.repair.event", SCHEMA_VERSION, stdout.lock());
        stream.emit_snapshot(RepairWatchPayload {
            sample,
            interval_seconds,
            worker,
            repair: payload,
        });
        return;
    }

    if sample > 1 {
        println!();
    }
    println!("Repair sample: {sample}");
    println!(
        "Worker: {} pid={} errors={} lease_expires_at_ms={}",
        worker.worker_id, worker.pid, worker.consecutive_errors, worker.expires_at_ms
    );
    render_repair_text(payload);
}

fn render_repair_text(payload: &RepairPayload) {
    let label = if payload.dry_run {
        "Repair plan"
    } else {
        "Repair"
    };
    println!("{label}:");
    if let Some(reason) = payload.blocked_reason.as_ref() {
        println!("  blocked: {reason}");
    }
    if let Some(plan) = payload.coordinator_plan.as_ref() {
        println!("  coordinator epoch: {}", plan.coordinator_epoch);
        println!("  materialization actions: {}", plan.actions.len());
    }
    for action in &payload.planned_actions {
        println!("  - {action}");
    }
}

fn render_promote(payload: &PromotePayload, mode: OutputMode) {
    if mode == OutputMode::Json {
        emit_json("replica.promote", SCHEMA_VERSION, payload);
        return;
    }
    if payload.dry_run {
        println!(
            "Would promote replica '{}' from {} to {}",
            payload.name, payload.old_primary, payload.new_primary
        );
    } else {
        println!(
            "Promoted replica '{}' from {} to {}",
            payload.name, payload.old_primary, payload.new_primary
        );
        if payload.forced {
            println!("Promotion was forced without local read-enabled proof");
        }
    }
    println!(
        "Plan ready: {} provider={} region={} read-enabled={} crab-url={}",
        payload.plan_ready,
        payload.provider,
        payload.region,
        payload.read_enabled,
        payload.new_primary_is_crab_url
    );
    if !payload.plan_checks.is_empty() {
        println!("Plan checks:");
        for check in &payload.plan_checks {
            println!(
                "  - [{}] {}: {}",
                check.state.as_str(),
                check.code,
                check.message
            );
            if check.blocking {
                println!("    blocks promotion");
            }
            println!("    fix: {}", check.remediation);
        }
    }
    if let Some(status) = payload.control_plane.as_ref() {
        println!(
            "Control plane: backend={} drift_checked={} checks={}",
            status.backend_available,
            status.checked_drift,
            status.checks.len()
        );
        for check in &status.checks {
            if check.state != ControlPlaneCheckState::Verified {
                println!(
                    "  - {} {}: {}",
                    check.code,
                    check.state.as_str(),
                    check.message
                );
            }
        }
    }
    if !payload.planned_actions.is_empty() {
        println!("Planned actions:");
        for action in &payload.planned_actions {
            println!("  - {action}");
        }
    }
}

fn render_diagnostics(payload: &DiagnosticsPayload, written_to: Option<&Path>, mode: OutputMode) {
    if mode == OutputMode::Json {
        emit_json("replica.diagnostics", SCHEMA_VERSION, payload);
        return;
    }

    match written_to {
        Some(path) => println!("Diagnostics written: {}", path.display()),
        None => println!("Diagnostics collected"),
    }
    println!("Collected at: {}ms", payload.collected_at_ms);
    println!("Deep checks: {}", payload.deep);
    if let Some(publication) = payload.published.as_ref() {
        println!("Published to primary: {}", publication.primary);
        println!("Published object: {}", publication.object_key);
    }
    render_status(&payload.status, OutputMode::Text);
    println!(
        "Active-active writes: {}",
        payload.active_active.writes_enabled
    );
    if let Some(reason) = payload.active_active.reason.as_ref() {
        println!("Active-active reason: {reason}");
    }
    if let Some(coordinator) = payload.coordinator.as_ref() {
        render_coordinator_status(coordinator);
    }
    if payload.findings.is_empty() {
        println!("Findings: none");
    } else {
        println!("Findings:");
        for finding in &payload.findings {
            render_doctor_finding(finding);
        }
    }
    if payload.fix_plan.is_empty() {
        return;
    }
    println!("Fix plan:");
    for action in &payload.fix_plan {
        render_doctor_fix_action(action);
    }
}

fn render_certification(
    payload: &CertificationPayload,
    written_to: Option<&Path>,
    mode: OutputMode,
) {
    if mode == OutputMode::Json {
        emit_json("replica.certification", SCHEMA_VERSION, payload);
        return;
    }

    let state = if payload.certified {
        "passed"
    } else {
        "failed"
    };
    println!("Enterprise certification: {state}");
    println!("Profile: {}", payload.profile.as_str());
    if let Some(path) = written_to {
        println!("Evidence written: {}", path.display());
    }
    println!("Redacted: {}", payload.redacted);
    println!("Collected at: {}ms", payload.collected_at_ms);
    println!("Deep checks: {}", payload.deep);
    if let Some(evidence) = payload.evidence.as_ref() {
        println!("Evidence directory: {}", evidence.directory);
        println!("Evidence profile: {}", evidence.profile.as_str());
        println!("Evidence verified: {}", evidence.verified);
        println!(
            "Evidence files: seen={} verified={} failed={}",
            evidence.summary.files_seen,
            evidence.summary.files_verified,
            evidence.summary.files_failed
        );
    }
    println!("Gates:");
    for gate in &payload.gates {
        println!(
            "  - [{}] {}: {}",
            gate.state.as_str(),
            gate.code,
            gate.message
        );
        if let Some(remediation) = gate.remediation.as_ref() {
            println!("    fix: {remediation}");
        }
    }
    if payload.findings.is_empty() {
        println!("Findings: none");
    } else {
        println!("Findings:");
        for finding in &payload.findings {
            render_doctor_finding(finding);
        }
    }
    if payload.fix_plan.is_empty() {
        return;
    }
    println!("Fix plan:");
    for action in &payload.fix_plan {
        render_doctor_fix_action(action);
    }
}

fn render_evidence_verify(payload: &EvidenceVerifyPayload, mode: OutputMode) {
    if mode == OutputMode::Json {
        emit_json("replica.evidence.verify", SCHEMA_VERSION, payload);
        return;
    }

    let state = if payload.verified {
        "verified"
    } else {
        "failed"
    };
    println!("Replica evidence: {state}");
    println!("Directory: {}", payload.directory);
    println!("Require redacted: {}", payload.require_redacted);
    println!("Profile: {}", payload.profile.as_str());
    println!(
        "Files: seen={} verified={} failed={} control-plane={} smoke={} redacted={} unredacted={}",
        payload.summary.files_seen,
        payload.summary.files_verified,
        payload.summary.files_failed,
        payload.summary.control_plane_evidence,
        payload.summary.smoke_evidence,
        payload.summary.redacted,
        payload.summary.unredacted
    );
    for gate in &payload.gates {
        println!("  gate [{}] {}", gate.state.as_str(), gate.code);
        println!("    {}", gate.message);
        if !gate.labels.is_empty() {
            println!("    labels: {}", gate.labels.join(", "));
        }
        if let Some(remediation) = gate.remediation.as_ref() {
            println!("    remediation: {remediation}");
        }
    }
    for file in &payload.files {
        println!("  - [{}] {}", file.state.as_str(), file.path);
        if let Some(kind) = file.kind {
            println!("    kind: {}", kind.as_str());
        }
        if let Some(label) = file.label.as_ref() {
            println!("    label: {label}");
        }
        if let Some(redacted) = file.redacted {
            println!("    redacted: {redacted}");
        }
        for error in &file.errors {
            println!("    error: {error}");
        }
    }
}

fn render_doctor(payload: &DoctorPayload, mode: OutputMode) {
    if mode == OutputMode::Json {
        emit_json("replica.doctor", SCHEMA_VERSION, payload);
        return;
    }

    render_status(
        &StatusPayload {
            primary: payload.primary.clone(),
            replicas: payload.replicas.clone(),
            health: payload.health.clone(),
            backfill: payload.backfill.clone(),
            control_plane: payload.control_plane.clone(),
        },
        mode,
    );
    println!(
        "Active-active writes: {}",
        payload.active_active.writes_enabled
    );
    if let Some(reason) = payload.active_active.reason.as_ref() {
        println!("Active-active reason: {reason}");
    }
    if let Some(coordinator) = payload.coordinator.as_ref() {
        render_coordinator_status(coordinator);
    }
    if payload.findings.is_empty() {
        println!("Findings: none");
    } else {
        println!("Findings:");
        for finding in &payload.findings {
            render_doctor_finding(finding);
        }
    }
    if payload.fix_plan.is_empty() {
        return;
    }
    println!("Fix plan:");
    for action in &payload.fix_plan {
        render_doctor_fix_action(action);
    }
}

fn render_doctor_finding(finding: &DoctorFinding) {
    if let Some(replica) = finding.replica.as_ref() {
        println!(
            "  - [{}] {} ({replica}): {}",
            finding.severity.as_str(),
            finding.code,
            finding.message
        );
    } else {
        println!(
            "  - [{}] {}: {}",
            finding.severity.as_str(),
            finding.code,
            finding.message
        );
    }
    if let Some(remediation) = finding.remediation.as_ref() {
        println!("    fix: {remediation}");
    }
}

fn render_doctor_fix_action(action: &DoctorFixAction) {
    if let Some(replica) = action.replica.as_ref() {
        println!(
            "  - [{}] {} ({replica}): {}",
            action.severity.as_str(),
            action.code,
            action.description
        );
    } else {
        println!(
            "  - [{}] {}: {}",
            action.severity.as_str(),
            action.code,
            action.description
        );
    }
    if let Some(command) = action.command.as_ref() {
        println!("    command: {command}");
    }
    for hint in &action.cost_hints {
        println!("    cost: {hint}");
    }
    for hint in &action.risk_hints {
        println!("    risk: {hint}");
    }
}

fn render_coordinator_status(status: &CoordinatorControlPlaneStatus) {
    let backend = if status.backend_available {
        "available"
    } else {
        "unavailable"
    };
    println!(
        "Coordinator: {} [{}] backend={} drift_checked={} checks={}",
        status.name,
        status.provider.as_str(),
        backend,
        status.checked_drift,
        status.checks.len()
    );
    for check in &status.checks {
        if check.state != CoordinatorCheckState::Verified {
            println!(
                "  - {} {}: {}",
                check.code,
                check.state.as_str(),
                check.message
            );
        }
    }
}

fn render_set_primary(payload: &SetPrimaryPayload, mode: OutputMode) {
    if mode == OutputMode::Json {
        emit_json("replica.set_primary", SCHEMA_VERSION, payload);
        return;
    }
    if payload.applied {
        println!(
            "Set primary from {} to {}",
            payload.old_primary, payload.new_primary
        );
        if payload.forced {
            println!("Primary change was forced after external DR verification");
        }
    } else {
        println!(
            "Would set primary from {} to {}",
            payload.old_primary, payload.new_primary
        );
        println!("No config was changed; rerun with --apply after completing the DR runbook");
    }
    println!(
        "Plan ready: {} configured-replica={} crab-url={}",
        payload.plan_ready,
        payload.target_replica.as_deref().unwrap_or("none"),
        payload.new_primary_is_crab_url
    );
    println!(
        "Warning: set-primary only changes Crab's primary remote; it does not perform provider DNS, bucket, database, or application traffic failover"
    );
    if !payload.plan_checks.is_empty() {
        println!("Plan checks:");
        for check in &payload.plan_checks {
            println!(
                "  - [{}] {}: {}",
                check.state.as_str(),
                check.code,
                check.message
            );
            if check.blocking {
                println!("    blocks primary change");
            }
            println!("    fix: {}", check.remediation);
        }
    }
    if let Some(status) = payload.control_plane.as_ref() {
        println!(
            "Control plane: backend={} drift_checked={} checks={}",
            status.backend_available,
            status.checked_drift,
            status.checks.len()
        );
        for check in &status.checks {
            if check.state != ControlPlaneCheckState::Verified {
                println!(
                    "  - {} {}: {}",
                    check.code,
                    check.state.as_str(),
                    check.message
                );
            }
        }
    }
    if !payload.planned_actions.is_empty() {
        println!("Planned actions:");
        for action in &payload.planned_actions {
            println!("  - {action}");
        }
    }
}

fn render_status(payload: &StatusPayload, mode: OutputMode) {
    if mode == OutputMode::Json {
        emit_json("replica.status", SCHEMA_VERSION, payload);
        return;
    }
    if mode == OutputMode::Jsonl {
        let stdout = std::io::stdout();
        let mut stream = JsonlStream::new("replica.status.event", SCHEMA_VERSION, stdout.lock());
        stream.emit_result(payload);
        return;
    }

    match payload.primary.as_deref() {
        Some(primary) => println!("Primary: {primary}"),
        None => println!("Primary: <not configured>"),
    }

    if payload.replicas.is_empty() {
        println!("Replicas: none");
        return;
    }

    for replica in &payload.replicas {
        let state = if replica.ready { "ready" } else { "not-ready" };
        let health = payload
            .health
            .iter()
            .find(|health| health.name == replica.name);
        let health_state = health.map_or("partial", |health| health.state.as_str());
        println!(
            "{} [{}] {} health={} gen={:?} lag={:?}",
            replica.name,
            replica.provider,
            state,
            health_state,
            replica.replica_generation,
            replica.lag_generations
        );
        if let Some(health) = health
            && health.state != ReplicaHealthState::Ready
        {
            println!("  health: {}", health.reason);
        }
        if let Some(reason) = replica.last_fallback_reason.as_ref() {
            if let Some(class) = replica.last_fallback_class {
                println!("  fallback: {} ({reason})", class.as_str());
            } else {
                println!("  fallback: {reason}");
            }
        }
        if let Some(backfill) = payload
            .backfill
            .iter()
            .find(|backfill| backfill.name == replica.name)
            && (backfill.required || backfill.state != BackfillState::NotRequired)
        {
            println!(
                "  backfill: {} blocks-read-enable={}",
                backfill.state.as_str(),
                backfill.blocks_read_enable
            );
            if let Some(code) = backfill.check_code.as_ref() {
                println!("  backfill check: {code}");
            }
            if let Some(progress) = backfill.progress_percent {
                println!("  backfill progress: {progress}%");
            }
        }
        if let Some(timestamp) = replica.last_fallback_at_ms {
            let operation = replica
                .last_fallback_operation
                .as_deref()
                .unwrap_or("unknown");
            println!(
                "  last fallback: operation={operation} at={timestamp}ms count={}",
                replica.fallback_count
            );
        } else if replica.fallback_count > 0 {
            println!("  fallback count: {}", replica.fallback_count);
        }
        if replica.primary_fallback_bytes > 0 {
            println!(
                "  primary fallback bytes: {}",
                replica.primary_fallback_bytes
            );
        }
        if let Some(timestamp) = replica.last_selected_at_ms {
            let operation = replica
                .last_selected_operation
                .as_deref()
                .unwrap_or("unknown");
            println!(
                "  last selected: operation={operation} at={timestamp}ms count={}",
                replica.selected_count
            );
        } else if replica.selected_count > 0 {
            println!("  selected count: {}", replica.selected_count);
        }
        if replica.readiness_cache_hit {
            if let Some(age_ms) = replica.readiness_cache_age_ms {
                println!("  readiness cache: hit age={age_ms}ms");
            } else {
                println!("  readiness cache: hit");
            }
        }
        if replica.readiness_check_latency_ms.is_some()
            || replica.readiness_object_probe_count > 0
            || replica.readiness_object_read_count > 0
        {
            let latency = replica
                .readiness_check_latency_ms
                .map_or_else(|| "unknown".to_owned(), |ms| format!("{ms}ms"));
            println!(
                "  readiness check: latency={} probes={} reads={}",
                latency, replica.readiness_object_probe_count, replica.readiness_object_read_count
            );
        }
    }
    if !payload.control_plane.is_empty() {
        println!("Control plane:");
        for status in &payload.control_plane {
            let backend = if status.backend_available {
                "available"
            } else {
                "unavailable"
            };
            println!(
                "  {} [{}] backend={} drift_checked={} checks={}",
                status.replica_name,
                status.provider,
                backend,
                status.checked_drift,
                status.checks.len()
            );
            for check in &status.checks {
                if check.state != ControlPlaneCheckState::Verified {
                    println!(
                        "    - {} {}: {}",
                        check.code,
                        check.state.as_str(),
                        check.message
                    );
                }
            }
        }
    }
}

#[derive(Debug, Serialize)]
struct StatusWatchPayload<'a> {
    sample: u64,
    interval_seconds: u64,
    status: &'a StatusPayload,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
struct ReplicaHealthTransition {
    sample: u64,
    name: String,
    provider: ReplicationProviderKind,
    region: String,
    previous_state: ReplicaHealthState,
    state: ReplicaHealthState,
    previous_reason: String,
    reason: String,
}

fn render_status_watch_snapshot(
    payload: &StatusPayload,
    mode: OutputMode,
    sample: u64,
    interval_seconds: u64,
    previous_health: Option<&[ReplicaHealth]>,
) {
    if mode == OutputMode::Jsonl {
        let stdout = std::io::stdout();
        let mut stream = JsonlStream::new("replica.status.event", SCHEMA_VERSION, stdout.lock());
        if let Some(previous) = previous_health {
            for transition in replica_health_transitions(sample, previous, &payload.health) {
                stream.emit_schema_event("replica.health.transition", "event", transition);
            }
        }
        stream.emit_snapshot(StatusWatchPayload {
            sample,
            interval_seconds,
            status: payload,
        });
        return;
    }

    if sample > 1 {
        println!();
    }
    if let Some(previous) = previous_health {
        for transition in replica_health_transitions(sample, previous, &payload.health) {
            println!(
                "Health transition: {} {} -> {} ({})",
                transition.name,
                transition.previous_state.as_str(),
                transition.state.as_str(),
                transition.reason
            );
        }
    }
    println!("Sample: {sample}");
    render_status(payload, OutputMode::Text);
}

fn replica_health_transitions(
    sample: u64,
    previous: &[ReplicaHealth],
    current: &[ReplicaHealth],
) -> Vec<ReplicaHealthTransition> {
    current
        .iter()
        .filter_map(|health| {
            let prior = previous.iter().find(|prior| prior.name == health.name)?;
            if prior.state == health.state {
                return None;
            }
            Some(ReplicaHealthTransition {
                sample,
                name: health.name.clone(),
                provider: health.provider,
                region: health.region.clone(),
                previous_state: prior.state,
                state: health.state,
                previous_reason: prior.reason.clone(),
                reason: health.reason.clone(),
            })
        })
        .collect()
}

#[allow(dead_code)]
fn _project_config_path_for_tests(root: &Path) -> PathBuf {
    project_config_path(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crab_coordination::error::CoordinationError;

    const ENTERPRISE_EVIDENCE_RUN_ID: &str = "replica-live-123456789-1";

    struct VerifiedCoordinatorBackend {
        provider: ManagedCoordinatorProvider,
    }

    impl VerifiedCoordinatorBackend {
        fn new(provider: ManagedCoordinatorProvider) -> Self {
            Self { provider }
        }
    }

    #[async_trait::async_trait]
    impl CoordinatorControlPlaneBackend for VerifiedCoordinatorBackend {
        fn provider(&self) -> ManagedCoordinatorProvider {
            self.provider
        }

        async fn apply(
            &self,
            plan: &CoordinatorControlPlanePlan,
        ) -> crab_coordination::Result<CoordinatorApplyStatus> {
            Ok(CoordinatorApplyStatus {
                provider: plan.provider,
                applied: true,
                checked_drift: true,
                actions: plan
                    .requests
                    .iter()
                    .map(|request| request.action.clone())
                    .collect(),
                message: format!("{} coordinator resources applied", plan.provider.as_str()),
            })
        }

        async fn status(
            &self,
            plan: &CoordinatorControlPlanePlan,
        ) -> crab_coordination::Result<CoordinatorControlPlaneStatus> {
            Ok(CoordinatorControlPlaneStatus {
                provider: plan.provider,
                name: plan.name.clone(),
                url: plan.url.clone(),
                region: plan.region.clone(),
                failover_regions: plan.failover_regions.clone(),
                backend_available: true,
                checked_drift: true,
                checks: plan
                    .requests
                    .iter()
                    .map(|request| {
                        crab_coordination::write_coordinator::CoordinatorControlPlaneCheck {
                            provider: request.provider,
                            code: format!(
                                "coordinator.{}.{}.verified",
                                request.provider.as_str(),
                                request.action.replace(':', "-")
                            ),
                            state: CoordinatorCheckState::Verified,
                            action: request.action.clone(),
                            target: request.target.clone(),
                            managed_resource_id: request.managed_resource_id.clone(),
                            message: "verified by test backend".to_owned(),
                            remediation: "no action required".to_owned(),
                        }
                    })
                    .collect(),
            })
        }

        async fn remove(
            &self,
            plan: &CoordinatorControlPlanePlan,
        ) -> crab_coordination::Result<CoordinatorApplyStatus> {
            Ok(CoordinatorApplyStatus {
                provider: plan.provider,
                applied: true,
                checked_drift: true,
                actions: plan
                    .requests
                    .iter()
                    .map(|request| format!("remove:{}", request.action))
                    .collect(),
                message: format!("{} coordinator resources removed", plan.provider.as_str()),
            })
        }
    }

    struct FailingCoordinatorBackend {
        provider: ManagedCoordinatorProvider,
        message: String,
    }

    impl FailingCoordinatorBackend {
        fn new(provider: ManagedCoordinatorProvider, message: &str) -> Self {
            Self {
                provider,
                message: message.to_owned(),
            }
        }

        fn error(&self, action: &str) -> CoordinationError {
            CoordinationError::Configuration {
                key: format!("replication.coordinator.{action}"),
                origin: self.message.clone(),
            }
        }
    }

    #[async_trait::async_trait]
    impl CoordinatorControlPlaneBackend for FailingCoordinatorBackend {
        fn provider(&self) -> ManagedCoordinatorProvider {
            self.provider
        }

        async fn apply(
            &self,
            _plan: &CoordinatorControlPlanePlan,
        ) -> crab_coordination::Result<CoordinatorApplyStatus> {
            Err(self.error("apply"))
        }

        async fn status(
            &self,
            _plan: &CoordinatorControlPlanePlan,
        ) -> crab_coordination::Result<CoordinatorControlPlaneStatus> {
            Err(self.error("status"))
        }

        async fn remove(
            &self,
            _plan: &CoordinatorControlPlanePlan,
        ) -> crab_coordination::Result<CoordinatorApplyStatus> {
            Err(self.error("remove"))
        }
    }

    struct SingleCoordinatorBackend<'a> {
        backend: &'a dyn CoordinatorControlPlaneBackend,
    }

    impl CoordinatorBackendResolver for SingleCoordinatorBackend<'_> {
        fn backend_for(
            &self,
            provider: ManagedCoordinatorProvider,
        ) -> Option<&dyn CoordinatorControlPlaneBackend> {
            if self.backend.provider() == provider {
                Some(self.backend)
            } else {
                None
            }
        }
    }

    fn configured_replica(read: bool) -> ReplicaConfig {
        ReplicaConfig {
            name: "west".into(),
            provider: ReplicationProviderKind::S3,
            url: "s3://replica/org/repo".into(),
            region: "us-west-2".into(),
            backfill: false,
            read,
            rpo: ReplicationRpo::Standard,
        }
    }

    fn configured_write_replica(read: bool) -> ReplicaConfig {
        ReplicaConfig {
            url: "crab://replica/org/repo".into(),
            ..configured_replica(read)
        }
    }

    fn replica_status(ready: bool) -> ReplicaStatus {
        ReplicaStatus {
            name: "west".into(),
            provider: ReplicationProviderKind::S3,
            url: "s3://replica/org/repo".into(),
            region: "us-west-2".into(),
            backfill_required: false,
            read_enabled: true,
            primary_generation: Some(7),
            replica_generation: Some(6),
            ready,
            lag_generations: Some(1),
            last_fallback_reason: Some("replica manifest is stale".into()),
            last_fallback_class: Some(ReplicaFallbackClass::StaleManifest),
            last_fallback_at_ms: Some(123),
            last_fallback_operation: Some("clone".into()),
            fallback_count: 2,
            primary_fallback_bytes: 1024,
            last_selected_at_ms: Some(111),
            last_selected_operation: Some("fetch".into()),
            selected_count: 3,
            readiness_cache_hit: true,
            readiness_cache_age_ms: Some(42),
            readiness_check_latency_ms: Some(17),
            readiness_object_probe_count: 5,
            readiness_object_read_count: 2,
        }
    }

    async fn control_plane_status() -> ControlPlaneStatus {
        control_plane_statuses(
            Some("crab://primary/org/repo"),
            Some(&crate::replication::ReplicationConfig {
                primary: Some("crab://primary/org/repo".into()),
                replicas: vec![configured_replica(true)],
                ..Default::default()
            }),
        )
        .await
        .into_iter()
        .next()
        .unwrap()
    }

    fn doctor_payload_for_test() -> DoctorPayload {
        let replicas = vec![replica_status(false)];
        let health = vec![replica_health_for(
            "west",
            ReplicaHealthState::Lagging,
            "replica is behind the primary",
        )];
        DoctorPayload {
            primary: Some("crab://primary/org/repo".into()),
            deep: true,
            replicas,
            health,
            backfill: Vec::new(),
            control_plane: Vec::new(),
            coordinator: None,
            coordinator_health: None,
            active_active: active_active_status(None),
            findings: vec![DoctorFinding {
                code: "replica.unready".into(),
                severity: DoctorSeverity::Error,
                message: "replica west is not ready".into(),
                replica: Some("west".into()),
                remediation: Some("run crab replica verify --deep --name west".into()),
            }],
            fix_plan: vec![DoctorFixAction {
                code: "replica.unready".into(),
                severity: DoctorSeverity::Error,
                replica: Some("west".into()),
                description: "verify every referenced object before read cutover".into(),
                command: Some("crab replica verify --deep --name west".into()),
                cost_hints: Vec::new(),
                risk_hints: Vec::new(),
                destructive: false,
            }],
        }
    }

    fn verified_control_plane_status() -> ControlPlaneStatus {
        ControlPlaneStatus {
            provider: ReplicationProviderKind::S3,
            replica_name: "west".into(),
            primary: "crab://primary/org/repo".into(),
            replica: "s3://replica/org/repo".into(),
            backend_available: true,
            checked_drift: true,
            checks: vec![provider_check(
                "provider.s3.replication-rule",
                ControlPlaneCheckState::Verified,
            )],
        }
    }

    fn provider_check(code: &str, state: ControlPlaneCheckState) -> ControlPlaneCheck {
        ControlPlaneCheck {
            provider: ReplicationProviderKind::S3,
            code: code.into(),
            state,
            action: "check".into(),
            target: "s3://replica/org/repo".into(),
            managed_resource_id: format!("crab:replica:west:{code}"),
            message: format!("{code} is {}", state.as_str()),
            remediation: "repair provider resource".into(),
            progress_percent: None,
        }
    }

    fn backfill_replica() -> ReplicaConfig {
        ReplicaConfig {
            backfill: true,
            ..configured_replica(false)
        }
    }

    fn active_active_replication() -> crate::replication::ReplicationConfig {
        crate::replication::ReplicationConfig {
            primary: Some("crab://primary/org/repo".into()),
            mode: ReplicationMode::ActiveActive,
            coordinator: Some(ReplicationCoordinatorConfig {
                kind: ReplicationCoordinatorKind::Managed,
                url: "dynamodb://crab-coordinator".into(),
                region: "us-east-1".into(),
                failover_regions: vec!["us-west-2".into()],
                consistency: ReplicationCoordinatorConsistency::Linearizable,
            }),
            writers: vec![WriterConfig {
                name: "east".into(),
                url: "crab://primary/org/repo".into(),
                region: "us-east-1".into(),
                enabled: true,
            }],
            replicas: Vec::new(),
        }
    }

    fn certified_replica_status() -> ReplicaStatus {
        ReplicaStatus {
            primary_generation: Some(7),
            replica_generation: Some(7),
            lag_generations: Some(0),
            last_fallback_reason: None,
            last_fallback_class: None,
            last_fallback_at_ms: None,
            last_fallback_operation: None,
            fallback_count: 0,
            primary_fallback_bytes: 0,
            readiness_cache_hit: false,
            readiness_cache_age_ms: None,
            ..replica_status(true)
        }
    }

    fn certified_backfill_status() -> BackfillReplicaStatus {
        BackfillReplicaStatus {
            name: "west".into(),
            provider: ReplicationProviderKind::S3,
            url: "s3://replica/org/repo".into(),
            region: "us-west-2".into(),
            required: false,
            read_enabled: true,
            state: BackfillState::NotRequired,
            blocks_read_enable: false,
            progress_percent: None,
            check_code: None,
            message: "replica was not added with --backfill".to_owned(),
            remediation: None,
        }
    }

    fn coordinator_health_with_summary(
        state_bytes: usize,
        max_state_bytes: Option<usize>,
        completed_operation_count: usize,
        max_completed_operations: usize,
    ) -> CoordinatorHealth {
        CoordinatorHealth {
            healthy: true,
            epoch: 7,
            linearizable: true,
            reason: None,
            state_summary: Some(
                crab_coordination::write_coordinator::CoordinatorStateSummary {
                    transaction_count: 0,
                    completed_operation_count,
                    max_completed_operations,
                    state_bytes,
                    max_state_bytes,
                },
            ),
        }
    }

    fn certified_doctor_payload() -> DoctorPayload {
        let replication = crate::replication::ReplicationConfig {
            primary: Some("crab://primary/org/repo".into()),
            replicas: vec![configured_replica(true)],
            ..Default::default()
        };
        DoctorPayload {
            primary: Some("crab://primary/org/repo".into()),
            deep: true,
            replicas: vec![certified_replica_status()],
            health: vec![replica_health_for(
                "west",
                ReplicaHealthState::Ready,
                "replica is read-enabled and manifest-referenced objects are ready",
            )],
            backfill: vec![certified_backfill_status()],
            control_plane: vec![verified_control_plane_status()],
            coordinator: None,
            coordinator_health: None,
            active_active: active_active_status(Some(&replication)),
            findings: Vec::new(),
            fix_plan: Vec::new(),
        }
    }

    fn certified_active_active_doctor_payload() -> DoctorPayload {
        DoctorPayload {
            primary: Some("crab://primary/org/repo".into()),
            deep: true,
            replicas: Vec::new(),
            health: Vec::new(),
            backfill: Vec::new(),
            control_plane: Vec::new(),
            coordinator: Some(CoordinatorControlPlaneStatus {
                provider: ManagedCoordinatorProvider::DynamoDb,
                name: "crab-coordinator".into(),
                url: "dynamodb://crab-coordinator".into(),
                region: "us-east-1".into(),
                failover_regions: vec!["us-west-2".into()],
                backend_available: true,
                checked_drift: true,
                checks: vec![CoordinatorControlPlaneCheck {
                    provider: ManagedCoordinatorProvider::DynamoDb,
                    code: "coordinator.dynamodb.global-table".into(),
                    state: CoordinatorCheckState::Verified,
                    action: "create-global-table".into(),
                    target: "dynamodb://crab-coordinator".into(),
                    managed_resource_id: "crab-coordinator-global-table".into(),
                    message: "DynamoDB coordinator is verified".into(),
                    remediation: "no action required".into(),
                }],
            }),
            coordinator_health: Some(coordinator_health_with_summary(1_024, Some(16_384), 4, 512)),
            active_active: ActiveActiveStatus {
                mode: ReplicationMode::ActiveActive,
                coordinator_configured: true,
                coordinator_ready: true,
                writes_enabled: true,
                enabled_writers: 2,
                reason: None,
            },
            findings: vec![DoctorFinding {
                code: "replica.none_configured".into(),
                severity: DoctorSeverity::Warning,
                message: "no read replicas are configured".into(),
                replica: None,
                remediation: Some("add a read replica when certifying read routing".into()),
            }],
            fix_plan: Vec::new(),
        }
    }

    fn verified_enterprise_evidence_payload() -> CertificationEvidencePayload {
        CertificationEvidencePayload {
            directory: "replica-live-evidence/customer-a".into(),
            verified: true,
            require_redacted: true,
            profile: EvidenceVerifyProfile::Enterprise,
            summary: EvidenceVerifySummary {
                files_seen: 64,
                files_verified: 64,
                files_failed: 0,
                control_plane_evidence: 30,
                smoke_evidence: 34,
                redacted: 64,
                unredacted: 0,
            },
            gates: vec![EvidenceVerifyGate {
                code: "enterprise-retained-proof".into(),
                state: CertificationGateState::Passed,
                message: "retained enterprise evidence verified".into(),
                labels: Vec::new(),
                remediation: None,
            }],
        }
    }

    fn certify_args(evidence_dir: &Path, expected_run_id: Option<&str>) -> CertifyArgs {
        CertifyArgs {
            profile: CertificationProfileArg::Enterprise,
            evidence_dir: Some(evidence_dir.to_path_buf()),
            expected_run_id: expected_run_id.map(str::to_owned),
            output: None,
            redact: false,
            json: false,
        }
    }

    fn finding_with_code<'a>(findings: &'a [DoctorFinding], code: &str) -> &'a DoctorFinding {
        findings
            .iter()
            .find(|finding| finding.code == code)
            .unwrap_or_else(|| panic!("missing finding code {code} in {findings:?}"))
    }

    fn add_args(path: &str) -> AddArgs {
        AddArgs {
            name: "west".into(),
            provider: ProviderArg::S3,
            primary: "crab://primary/org/repo".into(),
            replica: path.into(),
            region: "us-west-2".into(),
            backfill: false,
            rpo: RpoArg::Standard,
            dry_run: false,
            apply: false,
            json: false,
        }
    }

    fn status_args() -> StatusArgs {
        StatusArgs {
            deep: false,
            watch: false,
            interval: DEFAULT_STATUS_WATCH_INTERVAL_SECS,
            json: false,
            jsonl: false,
            prometheus: false,
        }
    }

    fn repair_args() -> RepairArgs {
        RepairArgs {
            from_coordinator: true,
            dry_run: false,
            service_template: None,
            service_name: "crab-replica-repair".into(),
            working_directory: None,
            container_image: "ghcr.io/crab-build/crab:latest".into(),
            output: None,
            watch: false,
            interval: DEFAULT_REPAIR_WATCH_INTERVAL_SECS,
            samples: None,
            json: false,
            jsonl: false,
        }
    }

    fn replica_health_for(name: &str, state: ReplicaHealthState, reason: &str) -> ReplicaHealth {
        ReplicaHealth {
            name: name.into(),
            provider: ReplicationProviderKind::S3,
            region: "us-west-2".into(),
            state,
            reason: reason.into(),
        }
    }

    #[test]
    fn status_watch_rejects_invalid_output_modes_and_interval() {
        let mut args = status_args();
        args.watch = true;
        args.json = true;
        assert!(validate_status_args(&args).is_err());

        let mut args = status_args();
        args.watch = true;
        args.prometheus = true;
        assert!(validate_status_args(&args).is_err());

        let mut args = status_args();
        args.interval = 0;
        assert!(validate_status_args(&args).is_err());
    }

    #[test]
    fn diagnostics_payload_preserves_doctor_evidence() {
        let payload = diagnostics_payload_from_doctor(doctor_payload_for_test(), true);

        assert_eq!(
            payload.status.primary.as_deref(),
            Some("crab://primary/org/repo")
        );
        assert!(payload.deep);
        assert!(payload.fix_plan_included);
        assert_eq!(payload.status.replicas.len(), 1);
        assert_eq!(payload.status.health[0].state, ReplicaHealthState::Lagging);
        assert_eq!(payload.findings[0].code, "replica.unready");
        assert_eq!(
            payload.fix_plan[0].command.as_deref(),
            Some("crab replica verify --deep --name west")
        );
    }

    #[test]
    fn certification_payload_passes_with_verified_live_evidence() {
        let payload = certification_payload_from_doctor(
            certified_doctor_payload(),
            CertificationProfileArg::Enterprise,
            Some(verified_enterprise_evidence_payload()),
        );

        assert!(payload.certified);
        assert_eq!(payload.profile, CertificationProfileArg::Enterprise);
        assert!(
            payload
                .evidence
                .as_ref()
                .is_some_and(|evidence| evidence.verified)
        );
        assert!(!payload.redacted);
        assert!(
            payload
                .gates
                .iter()
                .all(|gate| gate.state == CertificationGateState::Passed)
        );
    }

    #[test]
    fn enterprise_certification_requires_retained_live_evidence() {
        let payload = certification_payload_from_doctor(
            certified_doctor_payload(),
            CertificationProfileArg::Enterprise,
            None,
        );

        assert!(!payload.certified);
        assert_eq!(
            certification_gate_state(&payload, "certification.retained-evidence"),
            CertificationGateState::Failed
        );
    }

    #[test]
    fn certification_evidence_requires_expected_live_run_id() {
        let tmp = tempfile::tempdir().unwrap();
        write_complete_active_active_smoke_matrix(tmp.path());
        write_complete_provider_hydrate_matrix(tmp.path());
        write_complete_control_plane_matrix(tmp.path());
        set_all_evidence_redacted(tmp.path());

        let err = certification_evidence_payload(&certify_args(tmp.path(), None)).unwrap_err();

        assert!(
            err.to_string()
                .contains("enterprise certification requires --expected-run-id")
        );
    }

    #[test]
    fn certification_evidence_accepts_expected_live_run_id() {
        let tmp = tempfile::tempdir().unwrap();
        write_complete_active_active_smoke_matrix(tmp.path());
        write_complete_provider_hydrate_matrix(tmp.path());
        write_complete_control_plane_matrix(tmp.path());
        set_all_evidence_redacted(tmp.path());

        let payload = certification_evidence_payload(&certify_args(
            tmp.path(),
            Some(ENTERPRISE_EVIDENCE_RUN_ID),
        ))
        .unwrap()
        .unwrap();

        assert!(payload.verified);
        assert!(payload.gates.iter().any(|gate| {
            gate.code == "expected-live-evidence-run-id"
                && gate.state == CertificationGateState::Passed
        }));
    }

    #[test]
    fn certification_evidence_rejects_unexpected_live_run_id() {
        let tmp = tempfile::tempdir().unwrap();
        write_complete_active_active_smoke_matrix(tmp.path());
        write_complete_provider_hydrate_matrix(tmp.path());
        write_complete_control_plane_matrix(tmp.path());
        set_all_evidence_redacted(tmp.path());

        let payload =
            certification_evidence_payload(&certify_args(tmp.path(), Some("replica-live-12345-2")))
                .unwrap()
                .unwrap();

        assert!(!payload.verified);
        assert!(payload.gates.iter().any(|gate| {
            gate.code == "expected-live-evidence-run-id"
                && gate.state == CertificationGateState::Failed
        }));
    }

    #[test]
    fn certification_evidence_rejects_malformed_expected_live_run_id() {
        let tmp = tempfile::tempdir().unwrap();
        write_complete_active_active_smoke_matrix(tmp.path());
        write_complete_provider_hydrate_matrix(tmp.path());
        write_complete_control_plane_matrix(tmp.path());
        set_all_evidence_redacted(tmp.path());

        let payload =
            certification_evidence_payload(&certify_args(tmp.path(), Some("test-live-run")))
                .unwrap()
                .unwrap();

        assert!(!payload.verified);
        assert!(payload.gates.iter().any(|gate| {
            gate.code == "expected-live-evidence-run-id"
                && gate.state == CertificationGateState::Failed
                && gate.labels.contains(&"malformed:test-live-run".to_owned())
        }));
    }

    #[test]
    fn certification_payload_fails_on_cached_unverified_provider_evidence() {
        let mut doctor = certified_doctor_payload();
        doctor.replicas[0].readiness_cache_hit = true;
        doctor.control_plane[0].checks[0].state = ControlPlaneCheckState::Unknown;
        doctor.findings.push(DoctorFinding {
            code: "replica.cache_hit_unverified".into(),
            severity: DoctorSeverity::Warning,
            message: "replica readiness came from cache".into(),
            replica: Some("west".into()),
            remediation: Some("rerun deep doctor".into()),
        });

        let payload = certification_payload_from_doctor(
            doctor,
            CertificationProfileArg::Enterprise,
            Some(verified_enterprise_evidence_payload()),
        );

        assert!(!payload.certified);
        assert_eq!(
            certification_gate_state(&payload, "certification.deep-proof"),
            CertificationGateState::Failed
        );
        assert_eq!(
            certification_gate_state(&payload, "certification.provider-control-plane"),
            CertificationGateState::Failed
        );
        assert_eq!(
            certification_gate_state(&payload, "certification.doctor-findings"),
            CertificationGateState::Failed
        );
    }

    #[test]
    fn active_active_certification_profile_allows_writer_only_smoke() {
        let payload = certification_payload_from_doctor(
            certified_active_active_doctor_payload(),
            CertificationProfileArg::ActiveActive,
            None,
        );

        assert!(payload.certified);
        assert!(
            payload
                .gates
                .iter()
                .all(|gate| gate.code != "certification.replica-inventory")
        );
        assert_eq!(
            certification_gate_state(&payload, "certification.active-active"),
            CertificationGateState::Passed
        );
        assert_eq!(
            certification_gate_state(&payload, "certification.doctor-findings"),
            CertificationGateState::Passed
        );
    }

    #[test]
    fn active_active_certification_blocks_critical_coordinator_state_pressure() {
        let mut doctor = certified_active_active_doctor_payload();
        doctor.coordinator_health = Some(coordinator_health_with_summary(950, Some(1_000), 1, 512));

        let payload =
            certification_payload_from_doctor(doctor, CertificationProfileArg::ActiveActive, None);

        assert!(!payload.certified);
        assert_eq!(
            certification_gate_state(&payload, "certification.coordinator-state"),
            CertificationGateState::Failed
        );
    }

    #[test]
    fn active_active_certification_allows_noncritical_coordinator_pressure_warning() {
        let mut doctor = certified_active_active_doctor_payload();
        doctor.coordinator_health =
            Some(coordinator_health_with_summary(800, Some(1_000), 80, 100));
        doctor.findings.extend([
            DoctorFinding {
                code: "coordinator.state_size_high".into(),
                severity: DoctorSeverity::Warning,
                message: "coordinator state is high".into(),
                replica: None,
                remediation: None,
            },
            DoctorFinding {
                code: "coordinator.completed_operations_high".into(),
                severity: DoctorSeverity::Warning,
                message: "coordinator replay cache is high".into(),
                replica: None,
                remediation: None,
            },
        ]);

        let payload =
            certification_payload_from_doctor(doctor, CertificationProfileArg::ActiveActive, None);

        assert!(payload.certified);
        assert_eq!(
            certification_gate_state(&payload, "certification.coordinator-state"),
            CertificationGateState::Passed
        );
        assert_eq!(
            certification_gate_state(&payload, "certification.doctor-findings"),
            CertificationGateState::Passed
        );
    }

    #[test]
    fn evidence_verify_accepts_redacted_control_plane_and_smoke_artifacts() {
        let tmp = tempfile::tempdir().unwrap();
        write_json(
            &tmp.path().join("001-control-plane.json"),
            serde_json::json!({
                "schema": "replica.live-control-plane.evidence",
                "version": "1.0",
                "collected_at_ms": 1,
                "label": "coordinator-status",
                "provider": "dynamodb",
                "redacted": true,
                "args": ["replica", "coordinator", "status", "--json"],
                "result": coordinator_status_result()
            }),
        );
        std::fs::create_dir_all(tmp.path().join("smoke")).unwrap();
        write_json(
            &tmp.path().join("smoke").join("002-smoke.json"),
            serde_json::json!({
                "schema": "replica.live-smoke.evidence",
                "version": "1.0",
                "collected_at_ms": 2,
                "label": "active-active-certification",
                "coordinator_provider": "dynamodb",
                "redacted": true,
                "cwd": "writer-a",
                "args": ["replica", "certify", "--json"],
                "result": {
                    "schema": "replica.certification",
                    "data": {
                        "certified": true,
                        "deep": true,
                        "profile": "active-active",
                        "gates": [
                            {
                                "code": "certification.active-active",
                                "state": "passed",
                                "message": "active-active write admission is healthy"
                            },
                            {
                                "code": "certification.doctor-findings",
                                "state": "passed",
                                "message": "doctor has no blocking findings"
                            }
                        ],
                        "coordinator": {
                            "provider": "dynamodb"
                        }
                    }
                }
            }),
        );

        let payload =
            evidence_verify_payload(tmp.path(), true, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(payload.verified);
        assert_eq!(payload.profile, EvidenceVerifyProfile::Artifacts);
        assert_eq!(payload.summary.files_seen, 2);
        assert_eq!(payload.summary.files_verified, 2);
        assert_eq!(payload.summary.control_plane_evidence, 1);
        assert_eq!(payload.summary.smoke_evidence, 1);
        assert_eq!(payload.summary.redacted, 2);
        assert_eq!(payload.files[0].state, EvidenceFileState::Verified);
        assert_eq!(payload.files[1].path, "smoke/002-smoke.json");
    }

    #[test]
    fn evidence_verify_rejects_unredacted_artifacts_when_required() {
        let tmp = tempfile::tempdir().unwrap();
        write_json(
            &tmp.path().join("001-control-plane.json"),
            serde_json::json!({
                "schema": "replica.live-control-plane.evidence",
                "version": "1.0",
                "collected_at_ms": 1,
                "label": "coordinator-status",
                "provider": "dynamodb",
                "redacted": false,
                "args": ["replica", "coordinator", "status", "--json"],
                "result": coordinator_status_result()
            }),
        );

        let payload =
            evidence_verify_payload(tmp.path(), true, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert_eq!(payload.summary.files_seen, 1);
        assert_eq!(payload.summary.files_failed, 1);
        assert_eq!(payload.summary.unredacted, 1);
        assert!(
            payload.files[0]
                .errors
                .iter()
                .any(|error| error.contains("not redacted"))
        );
    }

    #[test]
    fn evidence_verify_rejects_redacted_artifact_with_aws_access_key_id() {
        let tmp = tempfile::tempdir().unwrap();
        write_control_plane_evidence(tmp.path(), "storage-status");
        let entry = evidence_path_for_label(tmp.path(), "storage-status");
        let mut value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&entry).unwrap()).unwrap();
        value["redacted"] = serde_json::json!(true);
        value["result"]["audit_note"] =
            serde_json::json!("captured request id AKIAABCDEFGHIJKLMNOP before upload");
        write_json(&entry, value);

        let payload =
            evidence_verify_payload(tmp.path(), true, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert_eq!(payload.summary.files_failed, 1);
        assert!(payload.files[0].errors.iter().any(|error| {
            error.contains("$.result.audit_note") && error.contains("aws-access-key-id")
        }));
    }

    #[test]
    fn evidence_verify_rejects_redacted_artifact_with_sensitive_key_value() {
        let tmp = tempfile::tempdir().unwrap();
        write_control_plane_evidence(tmp.path(), "storage-status");
        let entry = evidence_path_for_label(tmp.path(), "storage-status");
        let mut value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&entry).unwrap()).unwrap();
        value["redacted"] = serde_json::json!(true);
        value["result"]["AWS_SECRET_ACCESS_KEY"] = serde_json::json!("not-redacted");
        write_json(&entry, value);

        let payload =
            evidence_verify_payload(tmp.path(), true, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert_eq!(payload.summary.files_failed, 1);
        assert!(payload.files[0].errors.iter().any(|error| {
            error.contains("$.result.AWS_SECRET_ACCESS_KEY")
                && error.contains("aws-secret-access-key")
        }));
    }

    #[test]
    fn evidence_verify_rejects_redacted_provider_log_with_signed_url() {
        let tmp = tempfile::tempdir().unwrap();
        write_control_plane_evidence_with_provider(tmp.path(), STORAGE_PROVIDER_LOG_LABEL, "s3");
        set_provider_log_artifact_ref_for_label_and_provider(
            tmp.path(),
            STORAGE_PROVIDER_LOG_LABEL,
            "s3",
            "https://evidence.example/s3-provider-log.json?X-Amz-Signature=abc123",
        );
        set_all_evidence_redacted(tmp.path());

        let payload =
            evidence_verify_payload(tmp.path(), true, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert_eq!(payload.summary.files_failed, 1);
        assert!(payload.files[0].errors.iter().any(|error| {
            error.contains("$.result.data.artifact_ref") && error.contains("aws-signed-url")
        }));
    }

    #[test]
    fn evidence_verify_rejects_provider_log_with_bucket_only_artifact_uri() {
        let tmp = tempfile::tempdir().unwrap();
        write_control_plane_evidence_with_provider(tmp.path(), STORAGE_PROVIDER_LOG_LABEL, "s3");
        set_provider_log_artifact_ref_for_label_and_provider(
            tmp.path(),
            STORAGE_PROVIDER_LOG_LABEL,
            "s3",
            "s3://provider-log-bucket",
        );

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert_eq!(payload.summary.files_failed, 1);
        assert!(payload.files[0].errors.iter().any(|error| {
            error.contains("data.artifact_ref") && error.contains("secure artifact URI")
        }));
    }

    #[test]
    fn evidence_verify_rejects_provider_log_with_query_artifact_uri() {
        let tmp = tempfile::tempdir().unwrap();
        write_control_plane_evidence_with_provider(tmp.path(), STORAGE_PROVIDER_LOG_LABEL, "s3");
        set_provider_log_artifact_ref_for_label_and_provider(
            tmp.path(),
            STORAGE_PROVIDER_LOG_LABEL,
            "s3",
            "https://evidence.example/s3-provider-log.json?download=1",
        );

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert_eq!(payload.summary.files_failed, 1);
        assert!(payload.files[0].errors.iter().any(|error| {
            error.contains("data.artifact_ref") && error.contains("secure artifact URI")
        }));
    }

    #[test]
    fn evidence_verify_rejects_provider_log_with_prefix_artifact_uri() {
        let tmp = tempfile::tempdir().unwrap();
        write_control_plane_evidence_with_provider(tmp.path(), STORAGE_PROVIDER_LOG_LABEL, "s3");
        set_provider_log_artifact_ref_for_label_and_provider(
            tmp.path(),
            STORAGE_PROVIDER_LOG_LABEL,
            "s3",
            "s3://provider-log-bucket/logs/",
        );

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert_eq!(payload.summary.files_failed, 1);
        assert!(payload.files[0].errors.iter().any(|error| {
            error.contains("data.artifact_ref") && error.contains("secure artifact URI")
        }));
    }

    #[test]
    fn evidence_verify_rejects_provider_log_with_empty_segment_artifact_uri() {
        let tmp = tempfile::tempdir().unwrap();
        write_control_plane_evidence_with_provider(tmp.path(), STORAGE_PROVIDER_LOG_LABEL, "s3");
        set_provider_log_artifact_ref_for_label_and_provider(
            tmp.path(),
            STORAGE_PROVIDER_LOG_LABEL,
            "s3",
            "https://evidence.example//s3-provider-log.json",
        );

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert_eq!(payload.summary.files_failed, 1);
        assert!(payload.files[0].errors.iter().any(|error| {
            error.contains("data.artifact_ref") && error.contains("secure artifact URI")
        }));
    }

    #[test]
    fn evidence_verify_rejects_provider_log_with_parent_segment_artifact_uri() {
        let tmp = tempfile::tempdir().unwrap();
        write_control_plane_evidence_with_provider(tmp.path(), STORAGE_PROVIDER_LOG_LABEL, "s3");
        set_provider_log_artifact_ref_for_label_and_provider(
            tmp.path(),
            STORAGE_PROVIDER_LOG_LABEL,
            "s3",
            "https://evidence.example/logs/../s3-provider-log.json",
        );

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert_eq!(payload.summary.files_failed, 1);
        assert!(payload.files[0].errors.iter().any(|error| {
            error.contains("data.artifact_ref") && error.contains("secure artifact URI")
        }));
    }

    #[test]
    fn evidence_verify_rejects_redacted_provider_log_local_artifact_with_secret() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("s3-provider-log.txt"),
            "request captured AKIAABCDEFGHIJKLMNOP before upload\n",
        )
        .unwrap();
        write_control_plane_evidence_with_provider(tmp.path(), STORAGE_PROVIDER_LOG_LABEL, "s3");
        set_provider_log_artifact_ref_for_label_and_provider(
            tmp.path(),
            STORAGE_PROVIDER_LOG_LABEL,
            "s3",
            "s3-provider-log.txt",
        );
        set_evidence_redacted_for_label(tmp.path(), STORAGE_PROVIDER_LOG_LABEL);

        let payload =
            evidence_verify_payload(tmp.path(), true, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert_eq!(payload.summary.files_failed, 1);
        assert!(payload.files[0].errors.iter().any(|error| {
            error.contains("referenced local artifact s3-provider-log.txt")
                && error.contains("aws-access-key-id")
        }));
    }

    #[test]
    fn evidence_verify_rejects_provider_log_with_dot_segment_local_artifact_ref() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("s3-provider-log.txt"),
            "{\"provider\":\"s3\"}",
        )
        .unwrap();
        write_control_plane_evidence_with_provider(tmp.path(), STORAGE_PROVIDER_LOG_LABEL, "s3");
        set_provider_log_artifact_ref_for_label_and_provider(
            tmp.path(),
            STORAGE_PROVIDER_LOG_LABEL,
            "s3",
            "./s3-provider-log.txt",
        );

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert_eq!(payload.summary.files_failed, 1);
        assert!(payload.files[0].errors.iter().any(|error| {
            error.contains("data.artifact_ref")
                && error.contains("relative artifact inside the evidence directory")
        }));
    }

    #[test]
    fn evidence_verify_rejects_redacted_repair_worker_local_artifact_with_secret() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("deployment-proof.txt"),
            "Authorization: Bearer not-redacted\n",
        )
        .unwrap();
        write_repair_worker_deployment_evidence_with_ref(
            &tmp.path().join("001-repair-worker-deployment.json"),
            "systemd",
            "deployment-proof.txt",
        );
        set_evidence_redacted_for_label(tmp.path(), "repair-worker-deployment");

        let payload =
            evidence_verify_payload(tmp.path(), true, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert_eq!(payload.summary.files_failed, 1);
        assert!(payload.files[0].errors.iter().any(|error| {
            error.contains("referenced local artifact deployment-proof.txt")
                && error.contains("bearer-token")
        }));
    }

    #[test]
    fn evidence_verify_rejects_empty_directory() {
        let tmp = tempfile::tempdir().unwrap();

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert_eq!(payload.summary.files_seen, 0);
        assert_eq!(payload.summary.files_failed, 1);
        assert_eq!(payload.files[0].path, ".");
    }

    #[test]
    fn evidence_verify_rejects_control_plane_evidence_without_provider_identity() {
        let tmp = tempfile::tempdir().unwrap();
        write_json(
            &tmp.path().join("001-coordinator-status.json"),
            serde_json::json!({
                "schema": "replica.live-control-plane.evidence",
                "version": "1.0",
                "collected_at_ms": 1,
                "label": "coordinator-status",
                "redacted": false,
                "args": ["replica", "coordinator", "status", "--json"],
                "result": coordinator_status_result()
            }),
        );

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert!(
            payload.files[0]
                .errors
                .iter()
                .any(|error| error.contains("missing coordinator provider"))
        );
    }

    #[test]
    fn evidence_verify_rejects_storage_evidence_with_mismatched_provider() {
        let tmp = tempfile::tempdir().unwrap();
        write_json(
            &tmp.path().join("001-storage-status.json"),
            serde_json::json!({
                "schema": "replica.live-control-plane.evidence",
                "version": "1.0",
                "collected_at_ms": 1,
                "label": "storage-status",
                "provider": "s3",
                "redacted": false,
                "args": ["replica", "status", "--json"],
                "result": {
                    "provider": "gcs",
                    "backend_available": true,
                    "checked_drift": true,
                    "checks": [control_plane_check_result("gcs", "storage.replication")]
                }
            }),
        );

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert!(
            payload.files[0]
                .errors
                .iter()
                .any(|error| error.contains("storage provider s3 does not match provider=gcs"))
        );
    }

    #[test]
    fn evidence_verify_rejects_control_plane_status_with_drifted_check() {
        let tmp = tempfile::tempdir().unwrap();
        write_json(
            &tmp.path().join("001-storage-status.json"),
            serde_json::json!({
                "schema": "replica.live-control-plane.evidence",
                "version": "1.0",
                "collected_at_ms": 1,
                "label": "storage-status",
                "provider": "s3",
                "redacted": false,
                "args": ["replica", "status", "--json"],
                "result": {
                    "provider": "s3",
                    "backend_available": true,
                    "checked_drift": true,
                    "checks": [{
                        "code": "storage.replication",
                        "state": "drifted",
                        "target": "s3://replica/org/repo",
                        "managed_resource_id": "crab:s3:storage.replication"
                    }]
                }
            }),
        );

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert!(
            payload.files[0]
                .errors
                .iter()
                .any(|error| error.contains("non-verified control-plane check state drifted"))
        );
    }

    #[test]
    fn evidence_verify_rejects_control_plane_status_without_check_identity() {
        let tmp = tempfile::tempdir().unwrap();
        write_json(
            &tmp.path().join("001-coordinator-status.json"),
            serde_json::json!({
                "schema": "replica.live-control-plane.evidence",
                "version": "1.0",
                "collected_at_ms": 1,
                "label": "coordinator-status",
                "provider": "dynamodb",
                "redacted": false,
                "args": ["replica", "coordinator", "status", "--json"],
                "result": {
                    "schema": "replica.coordinator.status",
                    "data": {
                        "status": {
                            "provider": "dynamodb",
                            "backend_available": true,
                            "checked_drift": true,
                            "checks": [{"state": "verified"}]
                        }
                    }
                }
            }),
        );

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert!(
            payload.files[0]
                .errors
                .iter()
                .any(|error| error.contains("control-plane check 0 is missing code"))
        );
    }

    #[test]
    fn evidence_verify_rejects_coordinator_evidence_with_mismatched_provider() {
        let tmp = tempfile::tempdir().unwrap();
        write_json(
            &tmp.path().join("001-coordinator-plan.json"),
            serde_json::json!({
                "schema": "replica.live-control-plane.evidence",
                "version": "1.0",
                "collected_at_ms": 1,
                "label": "coordinator-plan",
                "provider": "spanner",
                "redacted": false,
                "args": ["replica", "coordinator", "add", "--json"],
                "result": {
                    "schema": "replica.coordinator",
                    "data": {
                        "plan": {
                            "provider": "dynamodb"
                        }
                    }
                }
            }),
        );

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert!(payload.files[0].errors.iter().any(|error| {
            error
                .contains("coordinator provider spanner does not match data.plan.provider=dynamodb")
        }));
    }

    #[test]
    fn evidence_verify_rejects_provider_hydrate_evidence_with_mismatched_provider() {
        let tmp = tempfile::tempdir().unwrap();
        write_json(
            &tmp.path().join("001-provider-hydrate-copy.json"),
            serde_json::json!({
                "schema": "replica.live-smoke.evidence",
                "version": "1.0",
                "collected_at_ms": 1,
                "label": "provider-hydrate-copy",
                "provider": "azure",
                "redacted": false,
                "cwd": "repo",
                "args": ["copy-primary-to-replica"],
                "result": {
                    "schema": "replica.live-hydrate",
                    "data": {
                        "provider": "s3",
                        "copied_objects": 1
                    }
                }
            }),
        );

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert!(payload.files[0].errors.iter().any(|error| {
            error.contains("hydrate provider azure does not match data.provider=s3")
        }));
    }

    #[test]
    fn evidence_verify_rejects_failover_status_with_mismatched_coordinator_provider() {
        let tmp = tempfile::tempdir().unwrap();
        write_json(
            &tmp.path().join("001-initial-failover-status.json"),
            serde_json::json!({
                "schema": "replica.live-smoke.evidence",
                "version": "1.0",
                "collected_at_ms": 1,
                "label": "initial-failover-status",
                "coordinator_provider": "dynamodb",
                "redacted": false,
                "cwd": "repo",
                "args": ["replica", "failover", "status", "--json"],
                "result": {
                    "schema": "replica.failover",
                    "data": {
                        "active_active": {
                            "writes_enabled": true
                        },
                        "coordinator": {
                            "provider": "spanner"
                        }
                    }
                }
            }),
        );

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert!(payload.files[0].errors.iter().any(|error| {
            error.contains(
                "coordinator provider dynamodb does not match data.coordinator.provider=spanner",
            )
        }));
    }

    #[test]
    fn evidence_verify_rejects_failover_operation_with_mismatched_coordinator_provider() {
        let tmp = tempfile::tempdir().unwrap();
        write_json(
            &tmp.path().join("001-failover-fence.json"),
            serde_json::json!({
                "schema": "replica.live-smoke.evidence",
                "version": "1.0",
                "collected_at_ms": 1,
                "label": "failover-fence",
                "coordinator_provider": "spanner",
                "redacted": false,
                "cwd": "repo",
                "args": ["replica", "failover", "fence", "--apply", "--json"],
                "result": {
                    "schema": "replica.failover.operation",
                    "data": {
                        "operation": "fence",
                        "applied": true,
                        "outcome": {
                            "provider": "cosmosdb",
                            "healthy": false
                        }
                    }
                }
            }),
        );

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert!(payload.files[0].errors.iter().any(|error| {
            error.contains(
                "coordinator provider spanner does not match data.outcome.provider=cosmosdb",
            )
        }));
    }

    #[test]
    fn evidence_verify_accepts_failover_run_operation() {
        let tmp = tempfile::tempdir().unwrap();
        let mut result = failover_run_result("fence", false, false);
        result["data"]["operation"]["outcome"]["provider"] = serde_json::json!("dynamodb");
        write_json(
            &tmp.path().join("001-failover-fence.json"),
            serde_json::json!({
                "schema": "replica.live-smoke.evidence",
                "version": "1.0",
                "collected_at_ms": 1,
                "label": "failover-fence",
                "coordinator_provider": "dynamodb",
                "redacted": false,
                "cwd": "repo",
                "args": [
                    "replica",
                    "failover",
                    "run",
                    "--writer-unhealthy",
                    "west",
                    "--apply",
                    "--json"
                ],
                "result": result
            }),
        );

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(payload.verified);
    }

    #[test]
    fn evidence_verify_rejects_failover_status_without_automation_policy() {
        let tmp = tempfile::tempdir().unwrap();
        write_json(
            &tmp.path().join("001-initial-failover-status.json"),
            serde_json::json!({
                "schema": "replica.live-smoke.evidence",
                "version": "1.0",
                "collected_at_ms": 1,
                "label": "initial-failover-status",
                "redacted": false,
                "cwd": "repo",
                "args": ["replica", "failover", "status", "--json"],
                "result": {
                    "schema": "replica.failover",
                    "data": {
                        "active_active": {
                            "writes_enabled": true
                        }
                    }
                }
            }),
        );

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert!(payload.files[0].errors.iter().any(|error| {
            error.contains("initial-failover-status is missing data.automation_policy")
        }));
    }

    #[test]
    fn evidence_verify_rejects_failover_status_without_automation_plan() {
        let tmp = tempfile::tempdir().unwrap();
        write_json(
            &tmp.path().join("001-initial-failover-status.json"),
            serde_json::json!({
                "schema": "replica.live-smoke.evidence",
                "version": "1.0",
                "collected_at_ms": 1,
                "label": "initial-failover-status",
                "redacted": false,
                "cwd": "repo",
                "args": ["replica", "failover", "status", "--json"],
                "result": {
                    "schema": "replica.failover",
                    "data": {
                        "active_active": {
                            "writes_enabled": true
                        },
                        "automation_policy": failover_automation_policy_result()
                    }
                }
            }),
        );

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert!(payload.files[0].errors.iter().any(|error| {
            error.contains("initial-failover-status is missing data.automation_plan.action")
        }));
    }

    #[test]
    fn evidence_verify_rejects_failover_operation_with_automatic_write_failover() {
        let tmp = tempfile::tempdir().unwrap();
        write_json(
            &tmp.path().join("001-failover-resume.json"),
            serde_json::json!({
                "schema": "replica.live-smoke.evidence",
                "version": "1.0",
                "collected_at_ms": 1,
                "label": "failover-resume",
                "redacted": false,
                "cwd": "repo",
                "args": ["replica", "failover", "resume", "--apply", "--json"],
                "result": {
                    "schema": "replica.failover.operation",
                    "data": {
                        "operation": "resume",
                        "applied": true,
                        "outcome": {
                            "healthy": true
                        },
                        "automation_policy": {
                            "automatic_write_failover_supported": true,
                            "orchestration": "automatic",
                            "split_brain_policy": "last-writer-wins",
                            "adr": "operator-local-note"
                        }
                    }
                }
            }),
        );

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert!(payload.files[0].errors.iter().any(|error| {
            error.contains(
                "failover-resume expected data.automation_policy.automatic_write_failover_supported to be false, got true",
            )
        }));
    }

    #[test]
    fn evidence_verify_rejects_failover_resume_without_repair_verified() {
        let tmp = tempfile::tempdir().unwrap();
        write_json(
            &tmp.path().join("001-failover-resume.json"),
            serde_json::json!({
                "schema": "replica.live-smoke.evidence",
                "version": "1.0",
                "collected_at_ms": 1,
                "label": "failover-resume",
                "redacted": false,
                "cwd": "repo",
                "args": ["replica", "failover", "resume", "--apply", "--json"],
                "result": {
                    "schema": "replica.failover.operation",
                    "data": {
                        "operation": "resume",
                        "applied": true,
                        "repair_verified": false,
                        "outcome": {
                            "healthy": true
                        },
                        "automation_policy": failover_automation_policy_result()
                    }
                }
            }),
        );

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert!(payload.files[0].errors.iter().any(|error| {
            error.contains("failover-resume expected args to contain --repair-verified")
        }));
        assert!(payload.files[0].errors.iter().any(|error| {
            error.contains("failover-resume expected data.repair_verified to be true, got false")
        }));
    }

    #[test]
    fn evidence_verify_rejects_certification_with_mismatched_coordinator_provider() {
        let tmp = tempfile::tempdir().unwrap();
        write_json(
            &tmp.path().join("001-active-active-certification.json"),
            serde_json::json!({
                "schema": "replica.live-smoke.evidence",
                "version": "1.0",
                "collected_at_ms": 1,
                "label": "active-active-certification",
                "coordinator_provider": "cosmosdb",
                "redacted": false,
                "cwd": "repo",
                "args": ["replica", "certify", "--profile", "active-active", "--json"],
                "result": {
                    "schema": "replica.certification",
                    "data": {
                        "certified": true,
                        "deep": true,
                        "profile": "active-active",
                        "gates": [
                            {
                                "code": "certification.active-active",
                                "state": "passed",
                                "message": "active-active write admission is healthy"
                            }
                        ],
                        "coordinator": {
                            "provider": "dynamodb"
                        }
                    }
                }
            }),
        );

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert!(payload.files[0].errors.iter().any(|error| {
            error.contains(
                "coordinator provider cosmosdb does not match data.coordinator.provider=dynamodb",
            )
        }));
    }

    #[test]
    fn evidence_verify_profile_rejects_incomplete_active_active_smoke() {
        let tmp = tempfile::tempdir().unwrap();
        write_smoke_evidence(tmp.path(), "active-active-certification");

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::ActiveActiveSmoke)
                .unwrap();

        assert!(!payload.verified);
        assert!(
            payload
                .gates
                .iter()
                .any(|gate| gate.code == "active-active-writer-pushes"
                    && gate.state == CertificationGateState::Failed)
        );
    }

    #[test]
    fn evidence_verify_control_plane_status_rejects_mutation_milestones() {
        let tmp = tempfile::tempdir().unwrap();
        write_control_plane_evidence(tmp.path(), "storage-status");
        write_control_plane_evidence(tmp.path(), "storage-apply");

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::ControlPlaneStatus)
                .unwrap();

        assert!(!payload.verified);
        assert!(payload.gates.iter().any(|gate| {
            gate.code == "control-plane-status-known-evidence-labels"
                && gate.state == CertificationGateState::Failed
                && gate
                    .labels
                    .iter()
                    .any(|label| label.contains("unexpected:storage-apply"))
        }));
    }

    #[test]
    fn evidence_verify_rejects_storage_mutation_without_actions() {
        let tmp = tempfile::tempdir().unwrap();
        write_control_plane_evidence(tmp.path(), "storage-apply");
        let entry = evidence_path_for_label(tmp.path(), "storage-apply");
        let mut value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&entry).unwrap()).unwrap();
        value["result"].as_object_mut().unwrap().remove("actions");
        write_json(&entry, value);

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert_eq!(payload.summary.files_failed, 1);
        assert!(
            payload.files[0]
                .errors
                .iter()
                .any(|error| error.contains("storage-apply is missing actions"))
        );
    }

    #[test]
    fn evidence_verify_rejects_coordinator_mutation_without_actions() {
        let tmp = tempfile::tempdir().unwrap();
        write_control_plane_evidence(tmp.path(), "coordinator-apply");
        let entry = evidence_path_for_label(tmp.path(), "coordinator-apply");
        let mut value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&entry).unwrap()).unwrap();
        value["result"]["data"]["apply_status"]
            .as_object_mut()
            .unwrap()
            .remove("actions");
        write_json(&entry, value);

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert_eq!(payload.summary.files_failed, 1);
        assert!(payload.files[0].errors.iter().any(|error| {
            error.contains("coordinator-apply is missing data.apply_status.actions")
        }));
    }

    #[test]
    fn evidence_verify_profile_accepts_complete_active_active_smoke() {
        let tmp = tempfile::tempdir().unwrap();
        write_complete_active_active_smoke(tmp.path());

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::ActiveActiveSmoke)
                .unwrap();

        assert!(payload.verified);
        assert!(
            payload
                .gates
                .iter()
                .all(|gate| gate.state == CertificationGateState::Passed)
        );
    }

    #[test]
    fn evidence_verify_profile_accepts_complete_provider_hydrate() {
        let tmp = tempfile::tempdir().unwrap();
        write_complete_provider_hydrate(tmp.path());

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::ProviderHydrate)
                .unwrap();

        assert!(payload.verified);
        assert!(
            payload
                .gates
                .iter()
                .all(|gate| gate.state == CertificationGateState::Passed)
        );
    }

    #[test]
    fn evidence_verify_provider_hydrate_rejects_unknown_prefix_milestone() {
        let tmp = tempfile::tempdir().unwrap();
        write_complete_provider_hydrate(tmp.path());
        write_smoke_evidence_with_provider(
            tmp.path(),
            "provider-hydrate-note",
            Some("s3"),
            Some("dynamodb"),
        );

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::ProviderHydrate)
                .unwrap();

        assert!(!payload.verified);
        assert!(payload.gates.iter().any(|gate| {
            gate.code == "provider-hydrate-known-evidence-labels"
                && gate.state == CertificationGateState::Failed
                && gate
                    .labels
                    .iter()
                    .any(|label| label.contains("unexpected:provider-hydrate-note"))
        }));
    }

    #[test]
    fn evidence_verify_profile_rejects_incomplete_provider_hydrate() {
        let tmp = tempfile::tempdir().unwrap();
        write_smoke_evidence(tmp.path(), "provider-hydrate-selected-replica");

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::ProviderHydrate)
                .unwrap();

        assert!(!payload.verified);
        assert!(
            payload
                .gates
                .iter()
                .any(|gate| gate.code == "provider-hydrate"
                    && gate.state == CertificationGateState::Failed)
        );
    }

    #[test]
    fn evidence_verify_enterprise_requires_control_plane_and_smoke_profiles() {
        let tmp = tempfile::tempdir().unwrap();
        write_complete_active_active_smoke_matrix(tmp.path());

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Enterprise).unwrap();

        assert!(!payload.verified);
        assert!(
            payload
                .gates
                .iter()
                .any(|gate| gate.code == "enterprise-storage-provider-matrix"
                    && gate.state == CertificationGateState::Failed)
        );
        assert!(
            payload
                .gates
                .iter()
                .any(|gate| gate.code == "enterprise-coordinator-provider-matrix"
                    && gate.state == CertificationGateState::Failed)
        );
        assert!(
            payload
                .gates
                .iter()
                .any(|gate| gate.code == "enterprise-hydrate-provider-matrix"
                    && gate.state == CertificationGateState::Failed)
        );
    }

    #[test]
    fn evidence_verify_enterprise_requires_provider_hydrate_proof() {
        let tmp = tempfile::tempdir().unwrap();
        write_complete_active_active_smoke_matrix(tmp.path());
        write_complete_control_plane_mutation(tmp.path());

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Enterprise).unwrap();

        assert!(!payload.verified);
        assert!(
            payload
                .gates
                .iter()
                .any(|gate| gate.code == "enterprise-hydrate-provider-matrix"
                    && gate.state == CertificationGateState::Failed)
        );
    }

    #[test]
    fn evidence_verify_enterprise_requires_storage_and_coordinator_mutation_proof() {
        let tmp = tempfile::tempdir().unwrap();
        write_complete_active_active_smoke_matrix(tmp.path());
        write_complete_provider_hydrate(tmp.path());

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Enterprise).unwrap();

        assert!(!payload.verified);
        assert!(
            payload
                .gates
                .iter()
                .any(|gate| gate.code == "enterprise-storage-provider-matrix"
                    && gate.state == CertificationGateState::Failed)
        );
        assert!(
            payload
                .gates
                .iter()
                .any(|gate| gate.code == "enterprise-coordinator-provider-matrix"
                    && gate.state == CertificationGateState::Failed)
        );
    }

    #[test]
    fn evidence_verify_enterprise_requires_provider_log_proof() {
        let tmp = tempfile::tempdir().unwrap();
        write_complete_active_active_smoke_matrix(tmp.path());
        write_complete_provider_hydrate_matrix(tmp.path());
        for provider in STORAGE_EVIDENCE_PROVIDERS {
            for label in [
                "storage-status",
                "storage-apply",
                "storage-status-after-apply",
                "storage-remove-plan",
                "storage-remove",
            ] {
                write_control_plane_evidence_with_provider(tmp.path(), label, provider);
            }
        }
        for provider in COORDINATOR_EVIDENCE_PROVIDERS {
            for label in [
                "coordinator-plan",
                "coordinator-status",
                "coordinator-apply",
                "coordinator-status-after-apply",
                "coordinator-remove",
            ] {
                write_control_plane_evidence_with_provider(tmp.path(), label, provider);
            }
        }
        set_all_evidence_redacted(tmp.path());

        let payload = evidence_verify_payload_with_expected_run_id(
            tmp.path(),
            true,
            EvidenceVerifyProfile::Enterprise,
            Some(ENTERPRISE_EVIDENCE_RUN_ID),
        )
        .unwrap();

        assert!(!payload.verified);
        assert!(payload.gates.iter().any(|gate| {
            gate.code == "enterprise-storage-provider-logs"
                && gate.state == CertificationGateState::Failed
        }));
        assert!(payload.gates.iter().any(|gate| {
            gate.code == "enterprise-coordinator-provider-logs"
                && gate.state == CertificationGateState::Failed
        }));
    }

    #[test]
    fn evidence_verify_enterprise_rejects_reused_storage_provider_log_artifact() {
        let tmp = tempfile::tempdir().unwrap();
        write_complete_active_active_smoke_matrix(tmp.path());
        write_complete_provider_hydrate_matrix(tmp.path());
        write_complete_control_plane_matrix(tmp.path());
        set_provider_log_artifact_ref_for_label_and_provider(
            tmp.path(),
            STORAGE_PROVIDER_LOG_LABEL,
            "s3",
            "https://evidence.example/shared/storage-provider-log.json",
        );
        set_provider_log_artifact_ref_for_label_and_provider(
            tmp.path(),
            STORAGE_PROVIDER_LOG_LABEL,
            "gcs",
            "https://evidence.example/shared/storage-provider-log.json",
        );
        set_all_evidence_redacted(tmp.path());

        let payload = evidence_verify_payload_with_expected_run_id(
            tmp.path(),
            true,
            EvidenceVerifyProfile::Enterprise,
            Some(ENTERPRISE_EVIDENCE_RUN_ID),
        )
        .unwrap();

        assert!(!payload.verified);
        assert!(payload.gates.iter().any(|gate| {
            gate.code == "enterprise-storage-provider-log-artifacts"
                && gate.state == CertificationGateState::Failed
                && gate.labels.iter().any(|label| {
                    label.contains("duplicate-artifact:gcs,s3")
                        && label.contains("shared/storage-provider-log.json")
                })
        }));
    }

    #[test]
    fn evidence_verify_enterprise_rejects_reused_coordinator_provider_log_artifact() {
        let tmp = tempfile::tempdir().unwrap();
        write_complete_active_active_smoke_matrix(tmp.path());
        write_complete_provider_hydrate_matrix(tmp.path());
        write_complete_control_plane_matrix(tmp.path());
        set_provider_log_artifact_ref_for_label_and_provider(
            tmp.path(),
            COORDINATOR_PROVIDER_LOG_LABEL,
            "dynamodb",
            "https://evidence.example/shared/coordinator-provider-log.json",
        );
        set_provider_log_artifact_ref_for_label_and_provider(
            tmp.path(),
            COORDINATOR_PROVIDER_LOG_LABEL,
            "spanner",
            "https://evidence.example/shared/coordinator-provider-log.json",
        );
        set_all_evidence_redacted(tmp.path());

        let payload = evidence_verify_payload_with_expected_run_id(
            tmp.path(),
            true,
            EvidenceVerifyProfile::Enterprise,
            Some(ENTERPRISE_EVIDENCE_RUN_ID),
        )
        .unwrap();

        assert!(!payload.verified);
        assert!(payload.gates.iter().any(|gate| {
            gate.code == "enterprise-coordinator-provider-log-artifacts"
                && gate.state == CertificationGateState::Failed
                && gate.labels.iter().any(|label| {
                    label.contains("duplicate-artifact:dynamodb,spanner")
                        && label.contains("shared/coordinator-provider-log.json")
                })
        }));
    }

    #[test]
    fn evidence_verify_enterprise_rejects_reused_provider_log_artifact_across_scopes() {
        let tmp = tempfile::tempdir().unwrap();
        write_complete_active_active_smoke_matrix(tmp.path());
        write_complete_provider_hydrate_matrix(tmp.path());
        write_complete_control_plane_matrix(tmp.path());
        set_provider_log_artifact_ref_for_label_and_provider(
            tmp.path(),
            STORAGE_PROVIDER_LOG_LABEL,
            "s3",
            "https://evidence.example/shared/provider-log.json",
        );
        set_provider_log_artifact_ref_for_label_and_provider(
            tmp.path(),
            COORDINATOR_PROVIDER_LOG_LABEL,
            "dynamodb",
            "https://evidence.example/shared/provider-log.json",
        );
        set_all_evidence_redacted(tmp.path());

        let payload = evidence_verify_payload_with_expected_run_id(
            tmp.path(),
            true,
            EvidenceVerifyProfile::Enterprise,
            Some(ENTERPRISE_EVIDENCE_RUN_ID),
        )
        .unwrap();

        assert!(!payload.verified);
        assert!(payload.gates.iter().any(|gate| {
            gate.code == "enterprise-provider-log-artifact-scopes"
                && gate.state == CertificationGateState::Failed
                && gate.labels.iter().any(|label| {
                    label.contains("duplicate-artifact-scope:dynamodb:coordinator-provider-log,s3:storage-provider-log")
                        && label.contains("shared/provider-log.json")
                })
        }));
    }

    #[test]
    fn evidence_verify_enterprise_rejects_single_provider_topology_evidence() {
        let tmp = tempfile::tempdir().unwrap();
        write_complete_active_active_smoke(tmp.path());
        write_complete_provider_hydrate(tmp.path());
        write_complete_control_plane_mutation(tmp.path());

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Enterprise).unwrap();

        assert!(!payload.verified);
        assert!(payload.gates.iter().any(|gate| {
            gate.code == "enterprise-storage-provider-matrix"
                && gate.state == CertificationGateState::Failed
                && gate.labels.contains(&"s3:storage-remove".to_owned())
        }));
        assert!(
            payload
                .gates
                .iter()
                .any(|gate| gate.code == "enterprise-hydrate-provider-matrix"
                    && gate.state == CertificationGateState::Failed)
        );
        assert!(payload.gates.iter().any(|gate| gate.code
            == "enterprise-active-active-coordinator-matrix"
            && gate.state == CertificationGateState::Failed));
    }

    #[test]
    fn evidence_verify_enterprise_rejects_single_region_pushes_for_one_coordinator() {
        let tmp = tempfile::tempdir().unwrap();
        write_complete_active_active_smoke_for(tmp.path(), "dynamodb", "us-west-2", "us-east-1");
        write_complete_active_active_smoke_for(tmp.path(), "spanner", "us-west2", "us-west2");
        write_complete_active_active_smoke_for(tmp.path(), "cosmosdb", "westus2", "eastus");
        write_complete_provider_hydrate_matrix(tmp.path());
        write_complete_control_plane_matrix(tmp.path());

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Enterprise).unwrap();

        assert!(!payload.verified);
        assert!(payload.gates.iter().any(|gate| {
            gate.code == "enterprise-active-active-writer-region-matrix"
                && gate.state == CertificationGateState::Failed
                && gate
                    .labels
                    .contains(&"spanner:us-west2:push-origin-main".to_owned())
                && gate
                    .labels
                    .contains(&"spanner:us-west2:push-west-feature".to_owned())
        }));
    }

    #[test]
    fn evidence_verify_enterprise_requires_production_load_proof() {
        let tmp = tempfile::tempdir().unwrap();
        for provider in COORDINATOR_EVIDENCE_PROVIDERS {
            write_complete_active_active_smoke_for(tmp.path(), provider, "us-west-2", "us-east-1");
        }
        write_complete_provider_hydrate_matrix(tmp.path());
        write_complete_control_plane_matrix(tmp.path());
        set_all_evidence_redacted(tmp.path());

        let payload = evidence_verify_payload_with_expected_run_id(
            tmp.path(),
            true,
            EvidenceVerifyProfile::Enterprise,
            Some(ENTERPRISE_EVIDENCE_RUN_ID),
        )
        .unwrap();

        assert!(!payload.verified);
        assert!(payload.gates.iter().any(|gate| {
            gate.code == "enterprise-production-load-matrix"
                && gate.state == CertificationGateState::Failed
                && gate
                    .labels
                    .contains(&"dynamodb:missing-production-load".to_owned())
                && gate
                    .labels
                    .contains(&"spanner:missing-production-load".to_owned())
                && gate
                    .labels
                    .contains(&"cosmosdb:missing-production-load".to_owned())
        }));
    }

    #[test]
    fn evidence_verify_enterprise_accepts_complete_storage_hydrate_and_active_active_proof() {
        let tmp = tempfile::tempdir().unwrap();
        write_complete_active_active_smoke_matrix(tmp.path());
        write_complete_provider_hydrate_matrix(tmp.path());
        write_complete_control_plane_matrix(tmp.path());
        set_all_evidence_redacted(tmp.path());

        let payload = evidence_verify_payload_with_expected_run_id(
            tmp.path(),
            true,
            EvidenceVerifyProfile::Enterprise,
            Some(ENTERPRISE_EVIDENCE_RUN_ID),
        )
        .unwrap();

        assert!(payload.verified);
        assert!(
            payload
                .gates
                .iter()
                .all(|gate| gate.state == CertificationGateState::Passed)
        );
    }

    #[test]
    fn evidence_verify_enterprise_accepts_dynamic_active_active_smoke_labels() {
        let tmp = tempfile::tempdir().unwrap();
        write_dynamic_active_active_smoke_matrix(tmp.path());
        write_complete_provider_hydrate_matrix(tmp.path());
        write_complete_control_plane_matrix(tmp.path());
        set_all_evidence_redacted(tmp.path());

        let payload = evidence_verify_payload_with_expected_run_id(
            tmp.path(),
            true,
            EvidenceVerifyProfile::Enterprise,
            Some(ENTERPRISE_EVIDENCE_RUN_ID),
        )
        .unwrap();

        assert!(payload.verified);
        assert!(payload.gates.iter().any(|gate| {
            gate.code == "enterprise-active-active-coordinator-matrix"
                && gate.state == CertificationGateState::Passed
                && gate
                    .labels
                    .iter()
                    .any(|label| label.contains("push-success:push-origin-crab-live-a-"))
        }));
    }

    #[test]
    fn evidence_verify_enterprise_requires_expected_live_run_id() {
        let tmp = tempfile::tempdir().unwrap();
        write_complete_active_active_smoke_matrix(tmp.path());
        write_complete_provider_hydrate_matrix(tmp.path());
        write_complete_control_plane_matrix(tmp.path());
        set_all_evidence_redacted(tmp.path());

        let payload = evidence_verify_payload_with_expected_run_id(
            tmp.path(),
            true,
            EvidenceVerifyProfile::Enterprise,
            None,
        )
        .unwrap();

        assert!(!payload.verified);
        assert!(payload.gates.iter().any(|gate| {
            gate.code == "expected-live-evidence-run-id"
                && gate.state == CertificationGateState::Failed
        }));
    }

    #[test]
    fn evidence_verify_enterprise_rejects_unredacted_complete_proof() {
        let tmp = tempfile::tempdir().unwrap();
        write_complete_active_active_smoke_matrix(tmp.path());
        write_complete_provider_hydrate_matrix(tmp.path());
        write_complete_control_plane_matrix(tmp.path());

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Enterprise).unwrap();

        assert!(!payload.verified);
        assert!(payload.gates.iter().any(|gate| {
            gate.code == "enterprise-redacted-evidence"
                && gate.state == CertificationGateState::Failed
                && gate
                    .labels
                    .contains(&"verification did not require redaction".to_owned())
        }));
    }

    #[test]
    fn evidence_verify_enterprise_accepts_expected_live_run_id() {
        let tmp = tempfile::tempdir().unwrap();
        write_complete_active_active_smoke_matrix(tmp.path());
        write_complete_provider_hydrate_matrix(tmp.path());
        write_complete_control_plane_matrix(tmp.path());
        set_all_evidence_redacted(tmp.path());

        let payload = evidence_verify_payload_with_expected_run_id(
            tmp.path(),
            true,
            EvidenceVerifyProfile::Enterprise,
            Some(ENTERPRISE_EVIDENCE_RUN_ID),
        )
        .unwrap();

        assert!(payload.verified);
        assert!(payload.gates.iter().any(|gate| {
            gate.code == "expected-live-evidence-run-id"
                && gate.state == CertificationGateState::Passed
        }));
    }

    #[test]
    fn evidence_verify_enterprise_rejects_unexpected_live_run_id() {
        let tmp = tempfile::tempdir().unwrap();
        write_complete_active_active_smoke_matrix(tmp.path());
        write_complete_provider_hydrate_matrix(tmp.path());
        write_complete_control_plane_matrix(tmp.path());

        let payload = evidence_verify_payload_with_expected_run_id(
            tmp.path(),
            false,
            EvidenceVerifyProfile::Enterprise,
            Some("replica-live-12345-2"),
        )
        .unwrap();

        assert!(!payload.verified);
        assert!(payload.gates.iter().any(|gate| {
            gate.code == "expected-live-evidence-run-id"
                && gate.state == CertificationGateState::Failed
        }));
    }

    #[test]
    fn evidence_verify_enterprise_rejects_malformed_expected_live_run_id() {
        let tmp = tempfile::tempdir().unwrap();
        write_complete_active_active_smoke_matrix(tmp.path());
        write_complete_provider_hydrate_matrix(tmp.path());
        write_complete_control_plane_matrix(tmp.path());
        set_all_evidence_redacted(tmp.path());

        let payload = evidence_verify_payload_with_expected_run_id(
            tmp.path(),
            true,
            EvidenceVerifyProfile::Enterprise,
            Some("test-live-run"),
        )
        .unwrap();

        assert!(!payload.verified);
        assert!(payload.gates.iter().any(|gate| {
            gate.code == "expected-live-evidence-run-id"
                && gate.state == CertificationGateState::Failed
                && gate.labels.contains(&"malformed:test-live-run".to_owned())
        }));
    }

    #[test]
    fn evidence_verify_enterprise_requires_live_harness_provenance() {
        let tmp = tempfile::tempdir().unwrap();
        write_complete_active_active_smoke_matrix(tmp.path());
        write_complete_provider_hydrate_matrix(tmp.path());
        write_complete_control_plane_matrix(tmp.path());
        remove_evidence_provenance_for_label(tmp.path(), "storage-status");

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Enterprise).unwrap();

        assert!(!payload.verified);
        assert!(payload.gates.iter().any(|gate| {
            gate.code == "enterprise-live-evidence-provenance"
                && gate.state == CertificationGateState::Failed
        }));
    }

    #[test]
    fn evidence_verify_enterprise_rejects_mixed_live_run_ids() {
        let tmp = tempfile::tempdir().unwrap();
        write_complete_active_active_smoke_matrix(tmp.path());
        write_complete_provider_hydrate_matrix(tmp.path());
        write_complete_control_plane_matrix(tmp.path());
        set_evidence_run_id_for_label(tmp.path(), "storage-status", "different-live-run");

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Enterprise).unwrap();

        assert!(!payload.verified);
        assert!(payload.gates.iter().any(|gate| {
            gate.code == "enterprise-live-evidence-provenance"
                && gate.state == CertificationGateState::Failed
        }));
    }

    #[test]
    fn evidence_verify_enterprise_rejects_unknown_live_evidence_label() {
        let tmp = tempfile::tempdir().unwrap();
        write_complete_active_active_smoke_matrix(tmp.path());
        write_complete_provider_hydrate_matrix(tmp.path());
        write_complete_control_plane_matrix(tmp.path());
        set_all_evidence_redacted(tmp.path());
        write_unknown_live_smoke_evidence(tmp.path());

        let payload = evidence_verify_payload_with_expected_run_id(
            tmp.path(),
            true,
            EvidenceVerifyProfile::Enterprise,
            Some(ENTERPRISE_EVIDENCE_RUN_ID),
        )
        .unwrap();

        assert!(!payload.verified);
        assert!(payload.gates.iter().any(|gate| {
            gate.code == "enterprise-known-evidence-labels"
                && gate.state == CertificationGateState::Failed
                && gate
                    .labels
                    .iter()
                    .any(|label| label.contains("unknown:operator-note"))
        }));
    }

    #[test]
    fn evidence_verify_enterprise_rejects_replayed_live_sequence() {
        let tmp = tempfile::tempdir().unwrap();
        write_complete_active_active_smoke_matrix(tmp.path());
        write_complete_provider_hydrate_matrix(tmp.path());
        write_complete_control_plane_matrix(tmp.path());
        let replayed = evidence_sequence_for_label(tmp.path(), "storage-status");
        set_evidence_sequence_for_label(tmp.path(), "storage-apply", replayed);

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Enterprise).unwrap();

        assert!(!payload.verified);
        assert!(payload.gates.iter().any(|gate| {
            gate.code == "enterprise-live-evidence-sequences"
                && gate.state == CertificationGateState::Failed
        }));
    }

    #[test]
    fn evidence_verify_enterprise_rejects_gapped_live_sequence() {
        let tmp = tempfile::tempdir().unwrap();
        write_complete_active_active_smoke_matrix(tmp.path());
        write_complete_provider_hydrate_matrix(tmp.path());
        write_complete_control_plane_matrix(tmp.path());
        set_evidence_sequence_for_label(tmp.path(), "storage-apply", 99);

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Enterprise).unwrap();

        assert!(!payload.verified);
        assert!(payload.gates.iter().any(|gate| {
            gate.code == "enterprise-live-evidence-sequences"
                && gate.state == CertificationGateState::Failed
        }));
    }

    #[test]
    fn evidence_verify_rejects_control_plane_zero_collected_at_ms() {
        let tmp = tempfile::tempdir().unwrap();
        write_control_plane_evidence(tmp.path(), "storage-status");
        set_evidence_collected_at_for_label(tmp.path(), "storage-status", 0);

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::ControlPlaneStatus)
                .unwrap();

        assert!(!payload.verified);
        assert!(payload.files.iter().any(|file| {
            file.label.as_deref() == Some("storage-status")
                && file.state == EvidenceFileState::Failed
                && file
                    .errors
                    .iter()
                    .any(|error| error.contains("collected_at_ms must be greater than zero"))
        }));
    }

    #[test]
    fn evidence_verify_rejects_smoke_zero_collected_at_ms() {
        let tmp = tempfile::tempdir().unwrap();
        write_smoke_evidence(tmp.path(), "initial-failover-status");
        set_evidence_collected_at_for_label(tmp.path(), "initial-failover-status", 0);

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert!(payload.files.iter().any(|file| {
            file.label.as_deref() == Some("initial-failover-status")
                && file.state == EvidenceFileState::Failed
                && file
                    .errors
                    .iter()
                    .any(|error| error.contains("collected_at_ms must be greater than zero"))
        }));
    }

    #[test]
    fn evidence_verify_enterprise_rejects_out_of_order_storage_control_plane() {
        let tmp = tempfile::tempdir().unwrap();
        write_complete_active_active_smoke_matrix(tmp.path());
        write_complete_provider_hydrate_matrix(tmp.path());
        write_complete_coordinator_control_plane_for(tmp.path(), "dynamodb");
        write_complete_coordinator_control_plane_for(tmp.path(), "spanner");
        write_complete_coordinator_control_plane_for(tmp.path(), "cosmosdb");
        write_complete_storage_control_plane_for(tmp.path(), "gcs");
        write_complete_storage_control_plane_for(tmp.path(), "azure");
        for label in [
            "storage-status",
            "storage-remove",
            "storage-apply",
            "storage-status-after-apply",
            "storage-remove-plan",
            "coordinator-plan",
            "coordinator-status",
            "coordinator-apply",
            "coordinator-status-after-apply",
            "coordinator-remove",
        ] {
            write_control_plane_evidence(tmp.path(), label);
        }

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Enterprise).unwrap();

        assert!(!payload.verified);
        assert!(
            payload
                .gates
                .iter()
                .any(|gate| gate.code == "enterprise-storage-provider-matrix"
                    && gate.state == CertificationGateState::Failed)
        );
    }

    #[test]
    fn evidence_verify_orders_control_plane_by_collected_time_not_filename() {
        let tmp = tempfile::tempdir().unwrap();
        write_complete_active_active_smoke_matrix(tmp.path());
        write_complete_provider_hydrate_matrix(tmp.path());
        write_complete_storage_control_plane_for(tmp.path(), "gcs");
        write_complete_storage_control_plane_for(tmp.path(), "azure");
        write_complete_coordinator_control_plane_for(tmp.path(), "dynamodb");
        write_complete_coordinator_control_plane_for(tmp.path(), "spanner");
        write_complete_coordinator_control_plane_for(tmp.path(), "cosmosdb");
        write_control_plane_evidence_at(
            tmp.path(),
            "900-storage-status.json",
            "storage-status",
            100,
        );
        write_control_plane_evidence_at(tmp.path(), "800-storage-apply.json", "storage-apply", 101);
        write_control_plane_evidence_at(
            tmp.path(),
            "700-storage-status-after-apply.json",
            "storage-status-after-apply",
            102,
        );
        write_control_plane_evidence_at(
            tmp.path(),
            "600-storage-remove-plan.json",
            "storage-remove-plan",
            103,
        );
        write_control_plane_evidence_at(
            tmp.path(),
            "500-storage-remove.json",
            "storage-remove",
            104,
        );
        write_control_plane_evidence_at(
            tmp.path(),
            "400-storage-provider-log.json",
            STORAGE_PROVIDER_LOG_LABEL,
            105,
        );
        for label in [
            "coordinator-plan",
            "coordinator-status",
            "coordinator-apply",
            "coordinator-status-after-apply",
            "coordinator-remove",
        ] {
            write_control_plane_evidence(tmp.path(), label);
        }
        set_all_evidence_redacted(tmp.path());

        let payload = evidence_verify_payload_with_expected_run_id(
            tmp.path(),
            true,
            EvidenceVerifyProfile::Enterprise,
            Some(ENTERPRISE_EVIDENCE_RUN_ID),
        )
        .unwrap();

        assert!(payload.verified);
        assert!(
            payload
                .files
                .iter()
                .any(|file| file.label.as_deref() == Some("storage-status")
                    && file.collected_at_ms == Some(100))
        );
    }

    #[test]
    fn evidence_verify_provider_hydrate_rejects_out_of_order_milestones() {
        let tmp = tempfile::tempdir().unwrap();
        for label in [
            "provider-hydrate-selected-replica",
            "provider-hydrate-init",
            "provider-hydrate-push",
            "provider-hydrate-copy",
            "provider-hydrate-read-enabled",
            "provider-hydrate-primary-xorbs-deleted",
        ] {
            write_smoke_evidence(tmp.path(), label);
        }

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::ProviderHydrate)
                .unwrap();

        assert!(!payload.verified);
        assert!(
            payload
                .gates
                .iter()
                .any(|gate| gate.code == "provider-hydrate"
                    && gate.state == CertificationGateState::Failed)
        );
    }

    #[test]
    fn evidence_verify_active_active_rejects_out_of_order_fencing() {
        let tmp = tempfile::tempdir().unwrap();
        for label in [
            "mode-active-active",
            "initial-failover-status",
            "push-origin-main",
            "repair-snapshot",
            "clone-main",
            "hydrate-main",
            "failover-resume",
            "writes-enabled",
            "failover-fence",
            "writes-fenced",
            "push-rejected-fenced-west-main",
            "push-west-feature",
            "repair-snapshot",
            "clone-feature",
            "hydrate-feature",
            "push-rejected-west-main",
            "active-active-certification",
        ] {
            write_smoke_evidence(tmp.path(), label);
        }

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::ActiveActiveSmoke)
                .unwrap();

        assert!(!payload.verified);
        assert!(
            payload
                .gates
                .iter()
                .any(|gate| gate.code == "active-active-fencing"
                    && gate.state == CertificationGateState::Failed)
        );
    }

    #[test]
    fn evidence_verify_active_active_rejects_out_of_order_story() {
        let tmp = tempfile::tempdir().unwrap();
        for label in [
            "mode-active-active",
            "initial-failover-status",
            "repair-service-template",
            "repair-worker-deployment",
            "push-origin-main",
            "failover-fence",
            "writes-fenced",
            "push-rejected-fenced-west-feature",
            "failover-resume",
            "writes-enabled",
            "repair-snapshot",
            "clone-main",
            "hydrate-main",
            "push-west-feature",
            "repair-snapshot",
            "clone-feature",
            "hydrate-feature",
            "push-rejected-west-main",
            "active-active-certification",
        ] {
            write_smoke_evidence(tmp.path(), label);
        }

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::ActiveActiveSmoke)
                .unwrap();

        assert!(!payload.verified);
        assert!(
            payload
                .gates
                .iter()
                .any(|gate| gate.code == "active-active-smoke-sequence"
                    && gate.state == CertificationGateState::Failed)
        );
    }

    #[test]
    fn evidence_verify_active_active_rejects_single_writer_region_pushes() {
        let tmp = tempfile::tempdir().unwrap();
        write_complete_active_active_smoke_with_writer_regions(
            tmp.path(),
            "us-west-2",
            "us-west-2",
        );

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::ActiveActiveSmoke)
                .unwrap();

        assert!(!payload.verified);
        assert!(
            payload
                .gates
                .iter()
                .any(|gate| gate.code == "active-active-writer-pushes"
                    && gate.state == CertificationGateState::Failed
                    && gate.labels
                        == vec![
                            "us-west-2:push-origin-main".to_owned(),
                            "us-west-2:push-west-feature".to_owned()
                        ])
        );
    }

    #[test]
    fn evidence_verify_active_active_rejects_single_reader_region() {
        let tmp = tempfile::tempdir().unwrap();
        write_complete_active_active_smoke(tmp.path());
        set_reader_region_for_label(tmp.path(), "clone-feature", "us-east-1");
        set_reader_region_for_label(tmp.path(), "hydrate-feature", "us-east-1");

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::ActiveActiveSmoke)
                .unwrap();

        assert!(!payload.verified);
        assert!(payload.gates.iter().any(|gate| {
            gate.code == "active-active-reader-regions-by-coordinator"
                && gate.state == CertificationGateState::Failed
                && gate
                    .labels
                    .contains(&"dynamodb:us-east-1:clone-main".to_owned())
                && gate
                    .labels
                    .contains(&"dynamodb:us-east-1:clone-feature".to_owned())
        }));
    }

    #[test]
    fn evidence_verify_active_active_rejects_writer_regions_split_across_coordinators() {
        let tmp = tempfile::tempdir().unwrap();
        for label in ACTIVE_ACTIVE_SMOKE_SEQUENCE {
            let coordinator_provider = if *label == "push-west-feature" {
                "spanner"
            } else {
                "dynamodb"
            };
            let writer_region = match *label {
                "push-origin-main" => Some("us-west-2"),
                "push-west-feature" => Some("us-east-1"),
                _ => None,
            };
            write_smoke_evidence_with_provider_and_writer_region(
                tmp.path(),
                label,
                None,
                Some(coordinator_provider),
                writer_region,
            );
        }

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::ActiveActiveSmoke)
                .unwrap();

        assert!(!payload.verified);
        assert!(payload.gates.iter().any(|gate| {
            gate.code == "active-active-writer-pushes"
                && gate.state == CertificationGateState::Passed
        }));
        assert!(payload.gates.iter().any(|gate| {
            gate.code == "active-active-coordinator-provider-sequence"
                && gate.state == CertificationGateState::Failed
                && gate
                    .labels
                    .contains(&"spanner:incomplete-active-active-sequence".to_owned())
        }));
        assert!(payload.gates.iter().any(|gate| {
            gate.code == "active-active-writer-regions-by-coordinator"
                && gate.state == CertificationGateState::Failed
                && gate
                    .labels
                    .contains(&"dynamodb:us-west-2:push-origin-main".to_owned())
                && gate
                    .labels
                    .contains(&"spanner:us-east-1:push-west-feature".to_owned())
        }));
    }

    #[test]
    fn evidence_verify_active_active_rejects_duplicate_push_operation_ids() {
        let tmp = tempfile::tempdir().unwrap();
        write_complete_active_active_smoke(tmp.path());
        set_push_operation_id_for_label(tmp.path(), "push-west-feature", "op-push-origin-main");

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::ActiveActiveSmoke)
                .unwrap();

        assert!(!payload.verified);
        assert!(payload.gates.iter().any(|gate| {
            gate.code == "active-active-distinct-push-operations"
                && gate.state == CertificationGateState::Failed
                && gate
                    .labels
                    .contains(&"dynamodb:op-push-origin-main:push-origin-main".to_owned())
                && gate
                    .labels
                    .contains(&"dynamodb:op-push-origin-main:push-west-feature".to_owned())
        }));
    }

    #[test]
    fn evidence_verify_active_active_requires_repair_service_template() {
        let tmp = tempfile::tempdir().unwrap();
        write_complete_active_active_smoke(tmp.path());
        std::fs::remove_file(evidence_path_for_label(
            tmp.path(),
            "repair-service-template",
        ))
        .unwrap();

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::ActiveActiveSmoke)
                .unwrap();

        assert!(!payload.verified);
        assert!(payload.gates.iter().any(|gate| {
            gate.code == "active-active-repair-service-template"
                && gate.state == CertificationGateState::Failed
        }));
    }

    #[test]
    fn evidence_verify_active_active_requires_repair_worker_deployment() {
        let tmp = tempfile::tempdir().unwrap();
        write_complete_active_active_smoke(tmp.path());
        std::fs::remove_file(evidence_path_for_label(
            tmp.path(),
            "repair-worker-deployment",
        ))
        .unwrap();

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::ActiveActiveSmoke)
                .unwrap();

        assert!(!payload.verified);
        assert!(payload.gates.iter().any(|gate| {
            gate.code == "active-active-repair-worker-deployment"
                && gate.state == CertificationGateState::Failed
        }));
    }

    #[test]
    fn evidence_verify_active_active_requires_matching_repair_worker_template() {
        let tmp = tempfile::tempdir().unwrap();
        write_complete_active_active_smoke(tmp.path());
        set_repair_service_template_for_label(tmp.path(), "repair-worker-deployment", "kubernetes");

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::ActiveActiveSmoke)
                .unwrap();

        assert!(!payload.verified);
        assert!(payload.gates.iter().any(|gate| {
            gate.code == "active-active-repair-worker-template-match"
                && gate.state == CertificationGateState::Failed
                && gate
                    .labels
                    .iter()
                    .any(|label| label.contains("repair-service-template=systemd"))
                && gate
                    .labels
                    .iter()
                    .any(|label| label.contains("repair-worker-deployment=kubernetes"))
        }));
    }

    #[test]
    fn evidence_verify_active_active_requires_matching_repair_worker_template_digest() {
        let tmp = tempfile::tempdir().unwrap();
        write_complete_active_active_smoke(tmp.path());
        set_repair_template_blake3_for_label(
            tmp.path(),
            "repair-worker-deployment",
            alternate_repair_worker_template_blake3_fixture(),
        );

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::ActiveActiveSmoke)
                .unwrap();

        assert!(!payload.verified);
        assert!(payload.gates.iter().any(|gate| {
            gate.code == "active-active-repair-worker-template-match"
                && gate.state == CertificationGateState::Failed
                && gate.labels.iter().any(|label| {
                    label.contains("repair-service-template=systemd")
                        && label.contains(repair_worker_template_blake3_fixture())
                })
                && gate.labels.iter().any(|label| {
                    label.contains("repair-worker-deployment=systemd")
                        && label.contains(alternate_repair_worker_template_blake3_fixture())
                })
        }));
    }

    #[test]
    fn evidence_verify_active_active_requires_matching_repair_worker_command_digest() {
        let tmp = tempfile::tempdir().unwrap();
        write_complete_active_active_smoke(tmp.path());
        set_repair_command_for_label(
            tmp.path(),
            "repair-worker-deployment",
            alternate_repair_worker_command_fixture(),
        );

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::ActiveActiveSmoke)
                .unwrap();

        assert!(!payload.verified);
        assert!(payload.gates.iter().any(|gate| {
            gate.code == "active-active-repair-worker-template-match"
                && gate.state == CertificationGateState::Failed
                && gate.labels.iter().any(|label| {
                    label.contains("repair-service-template=systemd")
                        && label.contains(&repair_worker_command_blake3_fixture())
                })
                && gate.labels.iter().any(|label| {
                    label.contains("repair-worker-deployment=systemd")
                        && label
                            .contains(&command_blake3(&alternate_repair_worker_command_fixture()))
                })
        }));
    }

    #[test]
    fn evidence_verify_rejects_semantically_invalid_required_smoke_artifact() {
        let tmp = tempfile::tempdir().unwrap();
        write_json(
            &tmp.path().join("001-push-origin-main.json"),
            serde_json::json!({
                "schema": "replica.live-smoke.evidence",
                "version": "1.0",
                "collected_at_ms": 1,
                "label": "push-origin-main",
                "coordinator_provider": "dynamodb",
                "redacted": false,
                "cwd": "repo",
                "args": ["crab", "push", "--json"],
                "result": {"schema": "replica.test"}
            }),
        );

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert_eq!(payload.summary.files_failed, 1);
        assert!(
            payload.files[0]
                .errors
                .iter()
                .any(|error| error.contains("expected schema to be push"))
        );
    }

    #[test]
    fn evidence_verify_rejects_push_evidence_without_push_command_args() {
        let tmp = tempfile::tempdir().unwrap();
        write_smoke_evidence(tmp.path(), "push-origin-main");
        set_smoke_args_for_label(
            tmp.path(),
            "push-origin-main",
            &["crab", "status", "--json"],
        );

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert!(
            payload.files[0]
                .errors
                .iter()
                .any(|error| error.contains("expected args to contain push"))
        );
    }

    #[test]
    fn evidence_verify_rejects_production_load_above_latency_budget() {
        let tmp = tempfile::tempdir().unwrap();
        write_production_load_evidence_for(tmp.path(), "dynamodb");
        let path = evidence_path_for_label(tmp.path(), PRODUCTION_LOAD_LABEL);
        let mut value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        value["result"]["data"]["push_latency_ms"] = serde_json::json!(120_000_u64);
        value["result"]["data"]["push_latency_budget_ms"] = serde_json::json!(60_000_u64);
        write_json(&path, value);

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert!(payload.files[0].errors.iter().any(|error| {
            error.contains("production-load expected data.push_latency_ms (120000)")
        }));
    }

    #[test]
    fn evidence_verify_rejects_production_load_without_xorb_count_source() {
        let tmp = tempfile::tempdir().unwrap();
        write_production_load_evidence_for(tmp.path(), "dynamodb");
        let path = evidence_path_for_label(tmp.path(), PRODUCTION_LOAD_LABEL);
        let mut value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        value["result"]["data"]
            .as_object_mut()
            .unwrap()
            .remove("xorb_count_source");
        write_json(&path, value);

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert!(
            payload.files[0]
                .errors
                .iter()
                .any(|error| error.contains("production-load is missing data.xorb_count_source"))
        );
    }

    #[test]
    fn evidence_verify_rejects_production_load_with_mismatched_xorb_count_delta() {
        let tmp = tempfile::tempdir().unwrap();
        write_production_load_evidence_for(tmp.path(), "dynamodb");
        let path = evidence_path_for_label(tmp.path(), PRODUCTION_LOAD_LABEL);
        let mut value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        value["result"]["data"]["xorb_count_after"] = serde_json::json!(17_u64);
        write_json(&path, value);

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert!(payload.files[0].errors.iter().any(|error| {
            error.contains("to equal data.xorb_count_after - data.xorb_count_before")
        }));
    }

    #[test]
    fn evidence_verify_rejects_repair_service_template_without_watch_command() {
        let tmp = tempfile::tempdir().unwrap();
        write_json(
            &tmp.path().join("001-repair-service-template.json"),
            serde_json::json!({
                "schema": "replica.live-smoke.evidence",
                "version": "1.0",
                "collected_at_ms": 1,
                "label": "repair-service-template",
                "coordinator_provider": "dynamodb",
                "redacted": false,
                "cwd": "repo",
                "args": ["replica", "repair", "--from-coordinator", "--service-template", "systemd"],
                "result": {
                    "schema": "replica.repair.service-template",
                    "data": {
                        "service_template": "systemd",
                        "from_coordinator": true,
                        "watch": false,
                        "jsonl": true,
                        "rendered": true,
                        "non_mutating": true,
                        "interval_seconds": 30,
                        "command": ["crab", "replica", "repair", "--from-coordinator"]
                    }
                }
            }),
        );

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert!(payload.files[0].errors.iter().any(|error| {
            error.contains("expected data.watch to be true")
                || error.contains("expected data.command to contain --watch")
        }));
    }

    #[test]
    fn evidence_verify_rejects_repair_service_template_without_template_digest() {
        let tmp = tempfile::tempdir().unwrap();
        write_smoke_evidence(tmp.path(), "repair-service-template");
        let path = evidence_path_for_label(tmp.path(), "repair-service-template");
        let mut value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        value["result"]["data"]
            .as_object_mut()
            .unwrap()
            .remove("template_blake3");
        write_json(&path, value);

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert!(
            payload.files[0]
                .errors
                .iter()
                .any(|error| error.contains("missing data.template_blake3"))
        );
    }

    #[test]
    fn evidence_verify_rejects_repair_service_template_with_wrong_command_digest() {
        let tmp = tempfile::tempdir().unwrap();
        write_smoke_evidence(tmp.path(), "repair-service-template");
        let path = evidence_path_for_label(tmp.path(), "repair-service-template");
        let mut value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        value["result"]["data"]["command_blake3"] =
            serde_json::json!(alternate_repair_worker_template_blake3_fixture());
        write_json(&path, value);

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert!(payload.files[0].errors.iter().any(|error| {
            error.contains("expected data.command_blake3 to equal Blake3 digest of data.command")
        }));
    }

    #[test]
    fn evidence_verify_rejects_repair_worker_deployment_without_artifact_ref() {
        let tmp = tempfile::tempdir().unwrap();
        write_json(
            &tmp.path().join("001-repair-worker-deployment.json"),
            serde_json::json!({
                "schema": "replica.live-smoke.evidence",
                "version": "1.0",
                "collected_at_ms": 1,
                "label": "repair-worker-deployment",
                "coordinator_provider": "dynamodb",
                "redacted": false,
                "cwd": "repo",
                "args": ["crab", "replica", "repair", "--service-template", "systemd"],
                "result": {
                    "schema": "replica.repair.worker-deployment",
                    "data": {
                        "deployment_verified": false,
                        "service_template": "systemd",
                        "command": ["crab", "replica", "repair", "--from-coordinator"]
                    }
                }
            }),
        );

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert!(payload.files[0].errors.iter().any(|error| {
            error.contains("data.artifact_ref")
                || error.contains("expected data.deployment_verified to be true")
        }));
    }

    #[test]
    fn evidence_verify_accepts_supported_repair_worker_deployment_templates() {
        let tmp = tempfile::tempdir().unwrap();
        for (index, service_template) in ["launchd", "kubernetes"].into_iter().enumerate() {
            write_repair_worker_deployment_evidence(
                &tmp.path()
                    .join(format!("{index:03}-repair-worker-deployment.json")),
                service_template,
            );
        }

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(payload.verified);
        assert_eq!(payload.summary.files_failed, 0);
    }

    #[test]
    fn evidence_verify_rejects_unsupported_repair_worker_deployment_template() {
        let tmp = tempfile::tempdir().unwrap();
        write_repair_worker_deployment_evidence(
            &tmp.path().join("001-repair-worker-deployment.json"),
            "cron",
        );

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert!(
            payload.files[0]
                .errors
                .iter()
                .any(|error| error.contains("unsupported data.service_template cron"))
        );
    }

    #[test]
    fn evidence_verify_rejects_repair_worker_deployment_with_missing_artifact_ref() {
        let tmp = tempfile::tempdir().unwrap();
        write_repair_worker_deployment_evidence_with_ref(
            &tmp.path().join("001-repair-worker-deployment.json"),
            "systemd",
            "missing-deployment-proof.txt",
        );

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert!(
            payload.files[0]
                .errors
                .iter()
                .any(|error| error.contains("data.artifact_ref must be an artifact URI"))
        );
    }

    #[test]
    fn evidence_verify_accepts_repair_worker_deployment_with_local_artifact_ref() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("deployment-proof.txt"), "deployment ok").unwrap();
        write_repair_worker_deployment_evidence_with_ref(
            &tmp.path().join("001-repair-worker-deployment.json"),
            "systemd",
            "deployment-proof.txt",
        );

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(payload.verified);
        assert_eq!(payload.summary.files_failed, 0);
    }

    #[test]
    fn evidence_verify_rejects_repair_worker_deployment_with_http_artifact_ref() {
        let tmp = tempfile::tempdir().unwrap();
        write_repair_worker_deployment_evidence_with_ref(
            &tmp.path().join("001-repair-worker-deployment.json"),
            "systemd",
            "http://evidence.example/deployment-proof.txt",
        );

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert!(payload.files[0].errors.iter().any(|error| {
            error.contains("data.artifact_ref") && error.contains("secure artifact URI")
        }));
    }

    #[test]
    fn evidence_verify_rejects_repair_worker_deployment_with_host_only_artifact_uri() {
        let tmp = tempfile::tempdir().unwrap();
        write_repair_worker_deployment_evidence_with_ref(
            &tmp.path().join("001-repair-worker-deployment.json"),
            "systemd",
            "https://evidence.example",
        );

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert!(payload.files[0].errors.iter().any(|error| {
            error.contains("data.artifact_ref") && error.contains("secure artifact URI")
        }));
    }

    #[test]
    fn evidence_verify_rejects_repair_worker_deployment_with_absolute_local_artifact_ref() {
        let root = tempfile::tempdir().unwrap();
        let evidence_dir = root.path().join("evidence");
        std::fs::create_dir(&evidence_dir).unwrap();
        let outside = root.path().join("deployment-proof.txt");
        std::fs::write(&outside, "deployment ok").unwrap();
        write_repair_worker_deployment_evidence_with_ref(
            &evidence_dir.join("001-repair-worker-deployment.json"),
            "systemd",
            &outside.display().to_string(),
        );

        let payload =
            evidence_verify_payload(&evidence_dir, false, EvidenceVerifyProfile::Artifacts)
                .unwrap();

        assert!(!payload.verified);
        assert!(payload.files[0].errors.iter().any(|error| {
            error.contains("data.artifact_ref")
                && error.contains("relative artifact inside the evidence directory")
        }));
    }

    #[test]
    fn evidence_verify_rejects_repair_worker_deployment_with_parent_dir_artifact_ref() {
        let root = tempfile::tempdir().unwrap();
        let evidence_dir = root.path().join("evidence");
        std::fs::create_dir(&evidence_dir).unwrap();
        std::fs::write(root.path().join("deployment-proof.txt"), "deployment ok").unwrap();
        write_repair_worker_deployment_evidence_with_ref(
            &evidence_dir.join("001-repair-worker-deployment.json"),
            "systemd",
            "../deployment-proof.txt",
        );

        let payload =
            evidence_verify_payload(&evidence_dir, false, EvidenceVerifyProfile::Artifacts)
                .unwrap();

        assert!(!payload.verified);
        assert!(payload.files[0].errors.iter().any(|error| {
            error.contains("data.artifact_ref")
                && error.contains("relative artifact inside the evidence directory")
        }));
    }

    #[test]
    fn evidence_verify_rejects_clone_evidence_without_checkout_details() {
        let tmp = tempfile::tempdir().unwrap();
        write_json(
            &tmp.path().join("001-clone-main.json"),
            serde_json::json!({
                "schema": "replica.live-smoke.evidence",
                "version": "1.0",
                "collected_at_ms": 1,
                "label": "clone-main",
                "coordinator_provider": "dynamodb",
                "redacted": false,
                "cwd": "repo",
                "args": ["crab", "clone", "--json"],
                "result": {"schema": "clone"}
            }),
        );

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert_eq!(payload.summary.files_failed, 1);
        assert!(
            payload.files[0]
                .errors
                .iter()
                .any(|error| error.contains("missing data.directory"))
        );
    }

    #[test]
    fn evidence_verify_rejects_clone_evidence_without_clone_command_args() {
        let tmp = tempfile::tempdir().unwrap();
        write_smoke_evidence(tmp.path(), "clone-main");
        set_smoke_args_for_label(tmp.path(), "clone-main", &["crab", "status", "--json"]);

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert!(
            payload.files[0]
                .errors
                .iter()
                .any(|error| error.contains("expected args to contain clone"))
        );
    }

    #[test]
    fn evidence_verify_rejects_hydrate_evidence_without_hydrated_file() {
        let tmp = tempfile::tempdir().unwrap();
        write_json(
            &tmp.path().join("001-hydrate-main.json"),
            serde_json::json!({
                "schema": "replica.live-smoke.evidence",
                "version": "1.0",
                "collected_at_ms": 1,
                "label": "hydrate-main",
                "coordinator_provider": "dynamodb",
                "redacted": false,
                "cwd": "repo",
                "args": ["crab", "hydrate", "--json"],
                "result": {
                    "schema": "hydrate",
                    "data": {
                        "hydrated": 0,
                        "failed": 0
                    }
                }
            }),
        );

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert_eq!(payload.summary.files_failed, 1);
        assert!(
            payload.files[0]
                .errors
                .iter()
                .any(|error| error.contains("expected data.hydrated to be at least 1"))
        );
    }

    #[test]
    fn evidence_verify_rejects_hydrate_evidence_without_hydrate_command_args() {
        let tmp = tempfile::tempdir().unwrap();
        write_smoke_evidence(tmp.path(), "hydrate-main");
        set_smoke_args_for_label(tmp.path(), "hydrate-main", &["crab", "status", "--json"]);

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert!(
            payload.files[0]
                .errors
                .iter()
                .any(|error| error.contains("expected args to contain hydrate"))
        );
    }

    #[test]
    fn evidence_verify_rejects_repair_snapshot_not_from_coordinator() {
        let tmp = tempfile::tempdir().unwrap();
        write_json(
            &tmp.path().join("001-repair-snapshot.json"),
            serde_json::json!({
                "schema": "replica.live-smoke.evidence",
                "version": "1.0",
                "collected_at_ms": 1,
                "label": "repair-snapshot",
                "coordinator_provider": "dynamodb",
                "redacted": false,
                "cwd": "repo",
                "args": ["replica", "repair", "--jsonl"],
                "result": {
                    "schema": "replica.repair.event",
                    "type": "snapshot",
                    "data": {
                        "repair": {
                            "from_coordinator": false,
                            "blocked_reason": null
                        }
                    }
                }
            }),
        );

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert!(
            payload.files[0].errors.iter().any(|error| {
                error.contains("expected data.repair.from_coordinator to be true")
            })
        );
    }

    #[test]
    fn evidence_verify_rejects_repair_snapshot_without_worker_lease_state() {
        let tmp = tempfile::tempdir().unwrap();
        write_json(
            &tmp.path().join("001-repair-snapshot.json"),
            serde_json::json!({
                "schema": "replica.live-smoke.evidence",
                "version": "1.0",
                "collected_at_ms": 1,
                "label": "repair-snapshot",
                "coordinator_provider": "dynamodb",
                "redacted": false,
                "cwd": "repo",
                "args": ["replica", "repair", "--from-coordinator", "--watch", "--jsonl"],
                "result": {
                    "schema": "replica.repair.event",
                    "type": "snapshot",
                    "data": {
                        "repair": {
                            "from_coordinator": true,
                            "blocked_reason": null
                        }
                    }
                }
            }),
        );

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert!(
            payload.files[0]
                .errors
                .iter()
                .any(|error| error.contains("data.worker.worker_id"))
        );
    }

    #[test]
    fn evidence_verify_rejects_repair_snapshot_without_bounded_worker_args() {
        let tmp = tempfile::tempdir().unwrap();
        write_json(
            &tmp.path().join("001-repair-snapshot.json"),
            serde_json::json!({
                "schema": "replica.live-smoke.evidence",
                "version": "1.0",
                "collected_at_ms": 1,
                "label": "repair-snapshot",
                "coordinator_provider": "dynamodb",
                "redacted": false,
                "cwd": "repo",
                "args": ["replica", "repair", "--from-coordinator", "--jsonl"],
                "result": smoke_result_for_label("repair-snapshot")
            }),
        );

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert!(payload.files[0].errors.iter().any(|error| {
            error.contains("repair-snapshot expected args to contain --watch")
                || error.contains("repair-snapshot expected args to contain --samples")
        }));
    }

    #[test]
    fn evidence_verify_rejects_certification_without_gate_proof() {
        let tmp = tempfile::tempdir().unwrap();
        write_json(
            &tmp.path().join("001-active-active-certification.json"),
            serde_json::json!({
                "schema": "replica.live-smoke.evidence",
                "version": "1.0",
                "collected_at_ms": 1,
                "label": "active-active-certification",
                "coordinator_provider": "dynamodb",
                "redacted": false,
                "cwd": "repo",
                "args": ["replica", "certify", "--profile", "active-active", "--json"],
                "result": {
                    "schema": "replica.certification",
                    "data": {
                        "certified": true,
                        "deep": true,
                        "profile": "active-active",
                        "coordinator": {
                            "provider": "dynamodb"
                        }
                    }
                }
            }),
        );

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert!(
            payload.files[0]
                .errors
                .iter()
                .any(|error| error.contains("missing data.gates"))
        );
    }

    #[test]
    fn evidence_verify_rejects_certification_with_failed_gate() {
        let tmp = tempfile::tempdir().unwrap();
        write_json(
            &tmp.path().join("001-active-active-certification.json"),
            serde_json::json!({
                "schema": "replica.live-smoke.evidence",
                "version": "1.0",
                "collected_at_ms": 1,
                "label": "active-active-certification",
                "coordinator_provider": "dynamodb",
                "redacted": false,
                "cwd": "repo",
                "args": ["replica", "certify", "--profile", "active-active", "--json"],
                "result": {
                    "schema": "replica.certification",
                    "data": {
                        "certified": true,
                        "deep": true,
                        "profile": "active-active",
                        "coordinator": {
                            "provider": "dynamodb"
                        },
                        "gates": [
                            {
                                "code": "certification.active-active",
                                "state": "failed",
                                "message": "write admission is blocked"
                            }
                        ]
                    }
                }
            }),
        );

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert!(payload.files[0].errors.iter().any(|error| {
            error.contains("expected certification gate certification.active-active to be passed")
        }));
    }

    #[test]
    fn evidence_verify_rejects_push_evidence_without_coordinator_epoch() {
        let tmp = tempfile::tempdir().unwrap();
        write_json(
            &tmp.path().join("001-push-origin-main.json"),
            serde_json::json!({
                "schema": "replica.live-smoke.evidence",
                "version": "1.0",
                "collected_at_ms": 1,
                "label": "push-origin-main",
                "coordinator_provider": "dynamodb",
                "redacted": false,
                "cwd": "repo",
                "args": ["crab", "push", "--json"],
                "result": {
                    "schema": "push",
                    "data": {
                        "refs_pushed": 1,
                        "operation_id": "op-1",
                        "writer_region": "us-west-2",
                        "commit_state": "materialized"
                    }
                }
            }),
        );

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert_eq!(payload.summary.files_failed, 1);
        assert!(
            payload.files[0]
                .errors
                .iter()
                .any(|error| error.contains("missing data.coordinator_epoch"))
        );
    }

    #[test]
    fn evidence_verify_rejects_push_evidence_with_zero_coordinator_epoch() {
        let tmp = tempfile::tempdir().unwrap();
        write_json(
            &tmp.path().join("001-push-origin-main.json"),
            serde_json::json!({
                "schema": "replica.live-smoke.evidence",
                "version": "1.0",
                "collected_at_ms": 1,
                "label": "push-origin-main",
                "coordinator_provider": "dynamodb",
                "redacted": false,
                "cwd": "repo",
                "args": ["crab", "push", "--json"],
                "result": {
                    "schema": "push",
                    "data": {
                        "refs_pushed": 1,
                        "operation_id": "op-1",
                        "coordinator_epoch": 0,
                        "writer_region": "us-west-2",
                        "commit_state": "materialized"
                    }
                }
            }),
        );

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert_eq!(payload.summary.files_failed, 1);
        assert!(
            payload.files[0]
                .errors
                .iter()
                .any(|error| error.contains("expected data.coordinator_epoch to be at least 1"))
        );
    }

    #[test]
    fn evidence_verify_rejects_clone_evidence_without_reader_region() {
        let tmp = tempfile::tempdir().unwrap();
        write_json(
            &tmp.path().join("001-clone-main.json"),
            serde_json::json!({
                "schema": "replica.live-smoke.evidence",
                "version": "1.0",
                "collected_at_ms": 1,
                "label": "clone-main",
                "coordinator_provider": "dynamodb",
                "redacted": false,
                "cwd": "repo",
                "args": ["crab", "clone", "--json"],
                "result": {
                    "schema": "clone",
                    "data": {
                        "url": "crab://bucket/repo",
                        "directory": "repo",
                        "lazy": false
                    }
                }
            }),
        );

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert!(
            payload.files[0]
                .errors
                .iter()
                .any(|error| error.contains("missing data.reader_region"))
        );
    }

    #[test]
    fn evidence_verify_rejects_push_evidence_before_manifest_materialization() {
        let tmp = tempfile::tempdir().unwrap();
        write_json(
            &tmp.path().join("001-push-origin-main.json"),
            serde_json::json!({
                "schema": "replica.live-smoke.evidence",
                "version": "1.0",
                "collected_at_ms": 1,
                "label": "push-origin-main",
                "coordinator_provider": "dynamodb",
                "redacted": false,
                "cwd": "repo",
                "args": ["crab", "push", "--json"],
                "result": {
                    "schema": "push",
                    "data": {
                        "refs_pushed": 1,
                        "operation_id": "op-1",
                        "coordinator_epoch": 7,
                        "writer_region": "us-west-2",
                        "commit_state": "committed"
                    }
                }
            }),
        );

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert_eq!(payload.summary.files_failed, 1);
        assert!(
            payload.files[0]
                .errors
                .iter()
                .any(|error| error.contains("expected data.commit_state to be materialized"))
        );
    }

    #[test]
    fn evidence_verify_rejects_fenced_write_without_fencing_reason() {
        let tmp = tempfile::tempdir().unwrap();
        write_json(
            &tmp.path().join("001-push-rejected-fenced-west-main.json"),
            serde_json::json!({
                "schema": "replica.live-smoke.evidence",
                "version": "1.0",
                "collected_at_ms": 1,
                "label": "push-rejected-fenced-west-main",
                "coordinator_provider": "dynamodb",
                "redacted": false,
                "cwd": "repo",
                "args": ["crab", "push", "--json"],
                "exit_code": 1,
                "stdout": "",
                "stderr": "rejected"
            }),
        );

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert_eq!(payload.summary.files_failed, 1);
        assert!(
            payload.files[0]
                .errors
                .iter()
                .any(|error| error.contains("must contain coordinator"))
        );
    }

    #[test]
    fn evidence_verify_rejects_push_rejection_without_push_command_args() {
        let tmp = tempfile::tempdir().unwrap();
        write_smoke_evidence(tmp.path(), "push-rejected-fenced-west-main");
        set_smoke_args_for_label(
            tmp.path(),
            "push-rejected-fenced-west-main",
            &["crab", "status", "--json"],
        );

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert!(
            payload.files[0]
                .errors
                .iter()
                .any(|error| error.contains("expected args to contain push"))
        );
    }

    #[test]
    fn evidence_verify_rejects_stale_ref_without_non_fast_forward_reason() {
        let tmp = tempfile::tempdir().unwrap();
        write_json(
            &tmp.path().join("001-push-rejected-west-main.json"),
            serde_json::json!({
                "schema": "replica.live-smoke.evidence",
                "version": "1.0",
                "collected_at_ms": 1,
                "label": "push-rejected-west-main",
                "coordinator_provider": "dynamodb",
                "redacted": false,
                "cwd": "repo",
                "args": ["crab", "push", "--json"],
                "exit_code": 1,
                "stdout": "",
                "stderr": "rejected"
            }),
        );

        let payload =
            evidence_verify_payload(tmp.path(), false, EvidenceVerifyProfile::Artifacts).unwrap();

        assert!(!payload.verified);
        assert_eq!(payload.summary.files_failed, 1);
        assert!(
            payload.files[0]
                .errors
                .iter()
                .any(|error| error.contains("must contain non-fast-forward"))
        );
    }

    fn write_json(path: &Path, value: serde_json::Value) {
        let body = serde_json::to_string_pretty(&value).unwrap();
        std::fs::write(path, body).unwrap();
    }

    fn write_complete_active_active_smoke(dir: &Path) {
        write_complete_active_active_smoke_for(dir, "dynamodb", "us-west-2", "us-east-1");
    }

    fn write_complete_active_active_smoke_matrix(dir: &Path) {
        for provider in COORDINATOR_EVIDENCE_PROVIDERS {
            write_complete_active_active_smoke_for(dir, provider, "us-west-2", "us-east-1");
            write_production_load_evidence_for(dir, provider);
        }
    }

    fn write_dynamic_active_active_smoke_matrix(dir: &Path) {
        for provider in COORDINATOR_EVIDENCE_PROVIDERS {
            write_dynamic_active_active_smoke_for(dir, provider);
        }
    }

    fn write_dynamic_active_active_smoke_for(dir: &Path, coordinator_provider: &str) {
        let branch_a = format!("crab-live-a-{coordinator_provider}-123");
        let branch_b = format!("crab-live-b-{coordinator_provider}-123");
        let conflict = format!("crab-live-conflict-{coordinator_provider}-123");
        for label in [
            "mode-active-active".to_owned(),
            "initial-failover-status".to_owned(),
            "repair-service-template".to_owned(),
            "repair-worker-deployment".to_owned(),
            format!("push-origin-{branch_a}"),
            "repair-snapshot".to_owned(),
            format!("clone-{branch_a}"),
            format!("hydrate-{branch_a}"),
            "failover-fence".to_owned(),
            "writes-fenced".to_owned(),
            format!("push-rejected-fenced-west-{branch_b}"),
            "failover-resume".to_owned(),
            "writes-enabled".to_owned(),
            format!("push-west-{branch_b}"),
            "repair-snapshot".to_owned(),
            format!("clone-{branch_b}"),
            format!("hydrate-{branch_b}"),
            format!("push-origin-{conflict}"),
            format!("push-origin-{conflict}"),
            format!("push-rejected-west-{conflict}"),
            "active-active-certification".to_owned(),
        ] {
            let writer_region = if label.starts_with("push-origin-") {
                Some("us-west-2")
            } else if label.starts_with("push-west-") {
                Some("us-east-1")
            } else {
                None
            };
            write_smoke_evidence_with_provider_and_writer_region(
                dir,
                &label,
                None,
                Some(coordinator_provider),
                writer_region,
            );
        }
        write_production_load_evidence_for(dir, coordinator_provider);
    }

    fn write_production_load_evidence_for(dir: &Path, coordinator_provider: &str) {
        write_smoke_evidence_with_provider_and_writer_region(
            dir,
            PRODUCTION_LOAD_LABEL,
            None,
            Some(coordinator_provider),
            None,
        );
    }

    fn write_complete_active_active_smoke_with_writer_regions(
        dir: &Path,
        origin_region: &str,
        west_region: &str,
    ) {
        write_complete_active_active_smoke_for(dir, "dynamodb", origin_region, west_region);
    }

    fn write_complete_active_active_smoke_for(
        dir: &Path,
        coordinator_provider: &str,
        origin_region: &str,
        west_region: &str,
    ) {
        for &label in ACTIVE_ACTIVE_SMOKE_SEQUENCE {
            let writer_region = match label {
                "push-origin-main" => Some(origin_region),
                "push-west-feature" => Some(west_region),
                _ => None,
            };
            write_smoke_evidence_with_provider_and_writer_region(
                dir,
                label,
                None,
                Some(coordinator_provider),
                writer_region,
            );
        }
    }

    fn write_complete_provider_hydrate(dir: &Path) {
        write_complete_provider_hydrate_for(dir, "s3");
    }

    fn write_complete_provider_hydrate_matrix(dir: &Path) {
        for provider in STORAGE_EVIDENCE_PROVIDERS {
            write_complete_provider_hydrate_for(dir, provider);
        }
    }

    fn write_complete_provider_hydrate_for(dir: &Path, provider: &str) {
        for label in [
            "provider-hydrate-init",
            "provider-hydrate-push",
            "provider-hydrate-copy",
            "provider-hydrate-read-enabled",
            "provider-hydrate-primary-xorbs-deleted",
            "provider-hydrate-selected-replica",
        ] {
            write_smoke_evidence_with_provider(dir, label, Some(provider), Some("dynamodb"));
        }
    }

    fn write_complete_control_plane_mutation(dir: &Path) {
        write_complete_storage_control_plane_for(dir, "s3");
        write_complete_coordinator_control_plane_for(dir, "dynamodb");
    }

    fn write_complete_control_plane_matrix(dir: &Path) {
        for provider in STORAGE_EVIDENCE_PROVIDERS {
            write_complete_storage_control_plane_for(dir, provider);
        }
        for provider in COORDINATOR_EVIDENCE_PROVIDERS {
            write_complete_coordinator_control_plane_for(dir, provider);
        }
    }

    fn write_complete_storage_control_plane_for(dir: &Path, provider: &str) {
        for label in [
            "storage-status",
            "storage-apply",
            "storage-status-after-apply",
            "storage-remove-plan",
            "storage-remove",
            STORAGE_PROVIDER_LOG_LABEL,
        ] {
            write_control_plane_evidence_with_provider(dir, label, provider);
        }
    }

    fn write_complete_coordinator_control_plane_for(dir: &Path, provider: &str) {
        for label in [
            "coordinator-plan",
            "coordinator-status",
            "coordinator-apply",
            "coordinator-status-after-apply",
            "coordinator-remove",
            COORDINATOR_PROVIDER_LOG_LABEL,
        ] {
            write_control_plane_evidence_with_provider(dir, label, provider);
        }
    }

    fn remove_evidence_provenance_for_label(dir: &Path, label: &str) {
        let entry = evidence_path_for_label(dir, label);
        let mut value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&entry).unwrap()).unwrap();
        let object = value.as_object_mut().unwrap();
        object.remove("harness");
        object.remove("run_id");
        object.remove("sequence");
        write_json(&entry, value);
    }

    fn set_evidence_run_id_for_label(dir: &Path, label: &str, run_id: &str) {
        let entry = evidence_path_for_label(dir, label);
        let mut value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&entry).unwrap()).unwrap();
        value["run_id"] = serde_json::json!(run_id);
        write_json(&entry, value);
    }

    fn set_provider_log_artifact_ref_for_label_and_provider(
        dir: &Path,
        label: &str,
        provider: &str,
        artifact_ref: &str,
    ) {
        let entry = evidence_path_for_label_and_provider(dir, label, provider);
        let mut value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&entry).unwrap()).unwrap();
        value["result"]["data"]["artifact_ref"] = serde_json::json!(artifact_ref);
        write_json(&entry, value);
    }

    fn set_repair_service_template_for_label(dir: &Path, label: &str, service_template: &str) {
        let entry = evidence_path_for_label(dir, label);
        let mut value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&entry).unwrap()).unwrap();
        value["result"]["data"]["service_template"] = serde_json::json!(service_template);
        write_json(&entry, value);
    }

    fn set_repair_template_blake3_for_label(dir: &Path, label: &str, template_blake3: &str) {
        let entry = evidence_path_for_label(dir, label);
        let mut value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&entry).unwrap()).unwrap();
        value["result"]["data"]["template_blake3"] = serde_json::json!(template_blake3);
        write_json(&entry, value);
    }

    fn set_repair_command_for_label(dir: &Path, label: &str, command: Vec<String>) {
        let entry = evidence_path_for_label(dir, label);
        let command_blake3 = command_blake3(&command);
        let mut value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&entry).unwrap()).unwrap();
        value["result"]["data"]["command"] = serde_json::json!(command);
        value["result"]["data"]["command_blake3"] = serde_json::json!(command_blake3);
        write_json(&entry, value);
    }

    fn set_push_operation_id_for_label(dir: &Path, label: &str, operation_id: &str) {
        let entry = evidence_path_for_label(dir, label);
        let mut value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&entry).unwrap()).unwrap();
        value["result"]["data"]["operation_id"] = serde_json::json!(operation_id);
        write_json(&entry, value);
    }

    fn set_reader_region_for_label(dir: &Path, label: &str, reader_region: &str) {
        let entry = evidence_path_for_label(dir, label);
        let mut value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&entry).unwrap()).unwrap();
        value["result"]["data"]["reader_region"] = serde_json::json!(reader_region);
        write_json(&entry, value);
    }

    fn set_smoke_args_for_label(dir: &Path, label: &str, args: &[&str]) {
        let entry = evidence_path_for_label(dir, label);
        let mut value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&entry).unwrap()).unwrap();
        value["args"] = serde_json::json!(args);
        write_json(&entry, value);
    }

    fn set_all_evidence_redacted(dir: &Path) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if !path.is_file() {
                continue;
            }
            let mut value: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            value["redacted"] = serde_json::json!(true);
            write_json(&path, value);
        }
    }

    fn set_evidence_redacted_for_label(dir: &Path, label: &str) {
        let entry = evidence_path_for_label(dir, label);
        let mut value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&entry).unwrap()).unwrap();
        value["redacted"] = serde_json::json!(true);
        write_json(&entry, value);
    }

    fn evidence_sequence_for_label(dir: &Path, label: &str) -> u64 {
        let entry = evidence_path_for_label(dir, label);
        let value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(entry).unwrap()).unwrap();
        value["sequence"].as_u64().unwrap()
    }

    fn set_evidence_sequence_for_label(dir: &Path, label: &str, sequence: u64) {
        let entry = evidence_path_for_label(dir, label);
        let mut value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&entry).unwrap()).unwrap();
        value["sequence"] = serde_json::json!(sequence);
        write_json(&entry, value);
    }

    fn set_evidence_collected_at_for_label(dir: &Path, label: &str, collected_at_ms: u64) {
        let entry = evidence_path_for_label(dir, label);
        let mut value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&entry).unwrap()).unwrap();
        value["collected_at_ms"] = serde_json::json!(collected_at_ms);
        write_json(&entry, value);
    }

    fn evidence_path_for_label(dir: &Path, label: &str) -> PathBuf {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
            .find(|path| {
                let value: serde_json::Value =
                    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
                value["label"].as_str() == Some(label)
            })
            .unwrap()
    }

    fn evidence_path_for_label_and_provider(dir: &Path, label: &str, provider: &str) -> PathBuf {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.extension().and_then(|ext| ext.to_str()) == Some("json"))
            .find(|path| {
                let value: serde_json::Value =
                    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
                value["label"].as_str() == Some(label)
                    && value["provider"].as_str() == Some(provider)
            })
            .unwrap()
    }

    fn next_evidence_sequence(
        dir: &Path,
        harness: &str,
        provider: Option<&str>,
        coordinator_provider: Option<&str>,
    ) -> u64 {
        let discriminator = provider.or(coordinator_provider).unwrap_or_default();
        std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                let Ok(value) = serde_json::from_str::<serde_json::Value>(
                    &std::fs::read_to_string(path).unwrap(),
                ) else {
                    return false;
                };
                value["run_id"].as_str() == Some(ENTERPRISE_EVIDENCE_RUN_ID)
                    && value["harness"].as_str() == Some(harness)
                    && value["provider"]
                        .as_str()
                        .or_else(|| value["coordinator_provider"].as_str())
                        == Some(discriminator)
            })
            .count() as u64
            + 1
    }

    fn write_control_plane_evidence(dir: &Path, label: &str) {
        let provider = provider_for_control_plane_label(label);
        write_control_plane_evidence_with_provider(dir, label, provider);
    }

    fn write_control_plane_evidence_with_provider(dir: &Path, label: &str, provider: &str) {
        let index = std::fs::read_dir(dir).unwrap().count() + 1;
        let sequence =
            next_evidence_sequence(dir, CONTROL_PLANE_EVIDENCE_HARNESS, Some(provider), None);
        write_control_plane_evidence_with_time(
            &dir.join(format!("{index:03}-{label}.json")),
            label,
            provider,
            index as u64,
            sequence,
        );
    }

    fn write_control_plane_evidence_at(
        dir: &Path,
        file_name: &str,
        label: &str,
        collected_at: u64,
    ) {
        let provider = provider_for_control_plane_label(label);
        let sequence =
            next_evidence_sequence(dir, CONTROL_PLANE_EVIDENCE_HARNESS, Some(provider), None);
        write_control_plane_evidence_with_time(
            &dir.join(file_name),
            label,
            provider,
            collected_at,
            sequence,
        );
    }

    fn write_control_plane_evidence_with_time(
        path: &Path,
        label: &str,
        provider: &str,
        collected_at: u64,
        sequence: u64,
    ) {
        write_json(
            path,
            serde_json::json!({
                "schema": "replica.live-control-plane.evidence",
                "version": "1.0",
                "collected_at_ms": collected_at,
                "harness": CONTROL_PLANE_EVIDENCE_HARNESS,
                "run_id": ENTERPRISE_EVIDENCE_RUN_ID,
                "sequence": sequence,
                "label": label,
                "provider": provider,
                "redacted": false,
                "args": ["crab"],
                "result": control_plane_result_for_label(label, provider)
            }),
        );
    }

    fn provider_for_control_plane_label(label: &str) -> &'static str {
        if is_coordinator_control_plane_label(label) {
            "dynamodb"
        } else {
            "s3"
        }
    }

    fn write_smoke_evidence(dir: &Path, label: &str) {
        write_smoke_evidence_with_writer_region(dir, label, None);
    }

    fn write_unknown_live_smoke_evidence(dir: &Path) {
        let index = std::fs::read_dir(dir).unwrap().count() + 1;
        write_json(
            &dir.join(format!("{index:03}-operator-note.json")),
            serde_json::json!({
                "schema": "replica.live-smoke.evidence",
                "version": "1.0",
                "collected_at_ms": index as u64,
                "harness": ACTIVE_ACTIVE_EVIDENCE_HARNESS,
                "run_id": ENTERPRISE_EVIDENCE_RUN_ID,
                "sequence": 1,
                "label": "operator-note",
                "redacted": true,
                "cwd": "repo",
                "args": ["operator", "note"],
                "result": {
                    "schema": "replica.operator-note",
                    "data": {
                        "message": "not a supported release milestone"
                    }
                }
            }),
        );
    }

    fn write_repair_worker_deployment_evidence(path: &Path, service_template: &str) {
        write_repair_worker_deployment_evidence_with_ref(
            path,
            service_template,
            "https://evidence.example/repair-worker-deployment.json",
        );
    }

    fn write_repair_worker_deployment_evidence_with_ref(
        path: &Path,
        service_template: &str,
        artifact_ref: &str,
    ) {
        write_json(
            path,
            serde_json::json!({
                "schema": "replica.live-smoke.evidence",
                "version": "1.0",
                "collected_at_ms": 1,
                "label": "repair-worker-deployment",
                "coordinator_provider": "dynamodb",
                "redacted": false,
                "cwd": "repo",
                "args": [
                    "external",
                    "repair-worker-deployment-evidence",
                    artifact_ref
                ],
                "result": {
                    "schema": "replica.repair.worker-deployment",
                    "data": {
                        "artifact_ref": artifact_ref,
                        "deployment_verified": true,
                        "service_template": service_template,
                        "template_blake3": repair_worker_template_blake3_fixture(),
                        "command_blake3": repair_worker_command_blake3_fixture(),
                        "command": repair_worker_command_fixture()
                    }
                }
            }),
        );
    }

    fn repair_worker_template_blake3_fixture() -> &'static str {
        "1111111111111111111111111111111111111111111111111111111111111111"
    }

    fn alternate_repair_worker_template_blake3_fixture() -> &'static str {
        "2222222222222222222222222222222222222222222222222222222222222222"
    }

    fn repair_worker_command_fixture() -> Vec<String> {
        [
            "crab",
            "replica",
            "repair",
            "--from-coordinator",
            "--watch",
            "--jsonl",
            "--interval",
            "30",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    }

    fn alternate_repair_worker_command_fixture() -> Vec<String> {
        [
            "crab",
            "replica",
            "repair",
            "--from-coordinator",
            "--watch",
            "--jsonl",
            "--interval",
            "60",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    }

    fn repair_worker_command_blake3_fixture() -> String {
        command_blake3(&repair_worker_command_fixture())
    }

    fn write_smoke_evidence_with_writer_region(
        dir: &Path,
        label: &str,
        writer_region: Option<&str>,
    ) {
        write_smoke_evidence_with_provider_and_writer_region(
            dir,
            label,
            None,
            Some("dynamodb"),
            writer_region,
        );
    }

    fn write_smoke_evidence_with_provider(
        dir: &Path,
        label: &str,
        provider: Option<&str>,
        coordinator_provider: Option<&str>,
    ) {
        write_smoke_evidence_with_provider_and_writer_region(
            dir,
            label,
            provider,
            coordinator_provider,
            None,
        );
    }

    fn write_smoke_evidence_with_provider_and_writer_region(
        dir: &Path,
        label: &str,
        provider: Option<&str>,
        coordinator_provider: Option<&str>,
        writer_region: Option<&str>,
    ) {
        let index = std::fs::read_dir(dir).unwrap().count() + 1;
        let sequence = next_evidence_sequence(
            dir,
            smoke_harness_for_label(label),
            provider,
            coordinator_provider,
        );
        let mut value = serde_json::json!({
            "schema": "replica.live-smoke.evidence",
            "version": "1.0",
            "collected_at_ms": index,
            "harness": smoke_harness_for_label(label),
            "run_id": ENTERPRISE_EVIDENCE_RUN_ID,
            "sequence": sequence,
            "label": label,
            "redacted": false,
            "cwd": "repo",
            "args": smoke_args_for_label(label),
        });
        if label == "repair-snapshot" {
            value["args"] = serde_json::json!([
                "replica",
                "repair",
                "--from-coordinator",
                "--watch",
                "--samples",
                "1",
                "--jsonl"
            ]);
        }
        if let Some(provider) = provider {
            value["provider"] = serde_json::json!(provider);
        }
        if let Some(coordinator_provider) = coordinator_provider {
            value["coordinator_provider"] = serde_json::json!(coordinator_provider);
        }
        if label.starts_with("push-rejected-") {
            value["exit_code"] = serde_json::json!(1);
            value["stdout"] = serde_json::json!("");
            value["stderr"] = serde_json::json!(rejection_stderr_for_label(label));
        } else {
            let mut result = smoke_result_for_label(label);
            if matches!(
                label,
                "provider-hydrate-copy" | "provider-hydrate-primary-xorbs-deleted"
            ) && let Some(provider) = provider
            {
                result["data"]["provider"] = serde_json::json!(provider);
            }
            if let Some(coordinator_provider) = coordinator_provider {
                match label {
                    "initial-failover-status" | "writes-enabled" | "writes-fenced" => {
                        result["data"]["coordinator"]["provider"] =
                            serde_json::json!(coordinator_provider);
                    }
                    "failover-fence" | "failover-resume" => {
                        result["data"]["operation"]["outcome"]["provider"] =
                            serde_json::json!(coordinator_provider);
                    }
                    "active-active-certification" => {
                        result["data"]["coordinator"]["provider"] =
                            serde_json::json!(coordinator_provider);
                    }
                    PRODUCTION_LOAD_LABEL => {
                        result["data"]["coordinator_provider"] =
                            serde_json::json!(coordinator_provider);
                    }
                    _ => {}
                }
            }
            if let Some(writer_region) = writer_region
                && label.starts_with("push-")
                && !label.starts_with("push-rejected-")
            {
                result["data"]["writer_region"] = serde_json::json!(writer_region);
            }
            value["result"] = result;
        }
        write_json(&dir.join(format!("{index:03}-{label}.json")), value);
    }

    fn smoke_args_for_label(label: &str) -> Vec<&'static str> {
        if label.starts_with("push-") {
            vec!["push", "--json"]
        } else if label.starts_with("clone-") {
            vec!["clone", "--json"]
        } else if label.starts_with("hydrate-") {
            vec!["hydrate", "--all", "--json"]
        } else if label == "failover-fence" {
            vec![
                "replica",
                "failover",
                "run",
                "--writer-unhealthy",
                "west",
                "--apply",
                "--json",
            ]
        } else if label == "failover-resume" {
            vec![
                "replica",
                "failover",
                "run",
                "--repair-verified",
                "--apply",
                "--json",
            ]
        } else if label == PRODUCTION_LOAD_LABEL {
            vec!["external", "production-load", "--json"]
        } else {
            vec!["crab"]
        }
    }

    fn smoke_harness_for_label(label: &str) -> &'static str {
        if is_provider_hydrate_label(label) {
            PROVIDER_HYDRATE_EVIDENCE_HARNESS
        } else if is_production_load_label(label) {
            PRODUCTION_LOAD_EVIDENCE_HARNESS
        } else {
            ACTIVE_ACTIVE_EVIDENCE_HARNESS
        }
    }

    fn rejection_stderr_for_label(label: &str) -> &'static str {
        if label.starts_with("push-rejected-fenced-") {
            "coordinator write admission is fenced; fail closed"
        } else {
            "non-fast-forward"
        }
    }

    fn control_plane_result_for_label(label: &str, provider: &str) -> serde_json::Value {
        match label {
            "storage-status" | "storage-status-after-apply" => serde_json::json!({
                "provider": provider,
                "backend_available": true,
                "checked_drift": true,
                "checks": [control_plane_check_result(provider, "storage.replication")]
            }),
            "storage-apply" => serde_json::json!({
                "provider": provider,
                "applied": true,
                "checked_drift": true,
                "actions": ["apply"]
            }),
            "storage-remove" => serde_json::json!({
                "provider": provider,
                "applied": true,
                "checked_drift": true,
                "actions": ["remove"]
            }),
            "storage-plan" | "storage-remove-plan" => serde_json::json!({
                "setup": {
                    "provider": provider
                },
                "actions": ["check", "apply"]
            }),
            STORAGE_PROVIDER_LOG_LABEL => serde_json::json!({
                "schema": "replica.live-provider-log",
                "data": {
                    "provider": provider,
                    "scope": "storage-control-plane",
                    "artifact_ref": format!("https://evidence.example/{provider}/storage-provider-log.json")
                }
            }),
            "coordinator-plan" => serde_json::json!({
                "schema": "replica.coordinator",
                "data": {"plan": {"provider": provider}}
            }),
            "coordinator-status" | "coordinator-status-after-apply" => {
                coordinator_status_result_for(provider)
            }
            "coordinator-apply" => serde_json::json!({
                "schema": "replica.coordinator",
                "data": {
                    "plan": {
                        "provider": provider
                    },
                    "provider": provider,
                    "applied": true,
                    "apply_status": {
                        "provider": provider,
                        "applied": true,
                        "checked_drift": true,
                        "actions": ["apply"]
                    }
                }
            }),
            "coordinator-remove" => serde_json::json!({
                "schema": "replica.coordinator.remove",
                "data": {
                    "plan": {
                        "provider": provider
                    },
                    "provider": provider,
                    "applied": true,
                    "apply_status": {
                        "provider": provider,
                        "applied": true,
                        "checked_drift": true,
                        "actions": ["remove"]
                    }
                }
            }),
            COORDINATOR_PROVIDER_LOG_LABEL => serde_json::json!({
                "schema": "replica.live-provider-log",
                "data": {
                    "provider": provider,
                    "scope": "coordinator-control-plane",
                    "artifact_ref": format!("https://evidence.example/{provider}/coordinator-provider-log.json")
                }
            }),
            _ => serde_json::json!({"schema": "replica.test"}),
        }
    }

    fn coordinator_status_result() -> serde_json::Value {
        coordinator_status_result_for("dynamodb")
    }

    fn coordinator_status_result_for(provider: &str) -> serde_json::Value {
        serde_json::json!({
            "schema": "replica.coordinator.status",
            "data": {
                "status": {
                    "provider": provider,
                    "backend_available": true,
                    "checked_drift": true,
                    "checks": [control_plane_check_result(provider, "coordinator.global-state")]
                }
            }
        })
    }

    fn control_plane_check_result(provider: &str, code: &str) -> serde_json::Value {
        serde_json::json!({
            "code": code,
            "state": "verified",
            "target": format!("{provider}:target"),
            "managed_resource_id": format!("crab:{provider}:{code}")
        })
    }

    fn smoke_result_for_label(label: &str) -> serde_json::Value {
        match label {
            "mode-active-active" => serde_json::json!({
                "schema": "replica.mode",
                "data": {
                    "mode": "active-active",
                    "active_active": {
                        "coordinator_configured": true,
                        "enabled_writers": 2
                    }
                }
            }),
            "initial-failover-status" | "writes-enabled" => failover_status_result(true),
            "writes-fenced" => failover_status_result(false),
            "failover-fence" => failover_run_result("fence", false, false),
            "failover-resume" => failover_run_result("resume", true, true),
            "repair-service-template" => serde_json::json!({
                "schema": "replica.repair.service-template",
                "data": {
                    "service_template": "systemd",
                    "from_coordinator": true,
                    "watch": true,
                    "jsonl": true,
                    "rendered": true,
                    "non_mutating": true,
                    "interval_seconds": 30,
                    "template_blake3": repair_worker_template_blake3_fixture(),
                    "command_blake3": repair_worker_command_blake3_fixture(),
                    "command": repair_worker_command_fixture()
                }
            }),
            "repair-worker-deployment" => serde_json::json!({
                "schema": "replica.repair.worker-deployment",
                "data": {
                    "artifact_ref": "https://evidence.example/repair-worker-deployment.json",
                    "deployment_verified": true,
                    "service_template": "systemd",
                    "template_blake3": repair_worker_template_blake3_fixture(),
                    "command_blake3": repair_worker_command_blake3_fixture(),
                    "command": repair_worker_command_fixture()
                }
            }),
            "repair-snapshot" => serde_json::json!({
                "schema": "replica.repair.event",
                "type": "snapshot",
                "data": {
                    "sample": 1,
                    "interval_seconds": 30,
                    "worker": {
                        "schema_version": REPAIR_WATCH_LEASE_SCHEMA_VERSION,
                        "worker_id": "repair-watch-test",
                        "pid": 42,
                        "lease_path": ".crab/replication/repair-watch-lease.json",
                        "acquired_at_ms": 1,
                        "heartbeat_at_ms": 2,
                        "expires_at_ms": 302000,
                        "base_interval_seconds": 30,
                        "next_interval_seconds": 30,
                        "consecutive_errors": 0,
                        "dry_run": false
                    },
                    "repair": {
                        "from_coordinator": true,
                        "blocked_reason": null
                    }
                }
            }),
            "active-active-certification" => serde_json::json!({
                "schema": "replica.certification",
                "data": {
                    "certified": true,
                    "deep": true,
                    "profile": "active-active",
                    "gates": [
                        {
                            "code": "certification.active-active",
                            "state": "passed",
                            "message": "active-active write admission is healthy"
                        },
                        {
                            "code": "certification.doctor-findings",
                            "state": "passed",
                            "message": "doctor has no blocking findings"
                        }
                    ]
                }
            }),
            PRODUCTION_LOAD_LABEL => serde_json::json!({
                "schema": "replica.production-load",
                "data": {
                    "profile": "production",
                    "coordinator_provider": "dynamodb",
                    "repository_bytes": 1_048_576_u64,
                    "file_count": 128_u64,
                    "xorb_count_source": "writer-store-delta",
                    "xorb_count_before": 10_u64,
                    "xorb_count_after": 18_u64,
                    "xorb_count": 8_u64,
                    "refs_pushed": 4_u64,
                    "writer_regions": 2_u64,
                    "reader_regions": 2_u64,
                    "clone_count": 2_u64,
                    "hydrate_count": 2_u64,
                    "push_latency_ms": 9_000_u64,
                    "push_latency_budget_ms": 60_000_u64,
                    "read_latency_ms": 7_500_u64,
                    "read_latency_budget_ms": 60_000_u64
                }
            }),
            "provider-hydrate-init" => serde_json::json!({"schema": "init"}),
            "provider-hydrate-push" => serde_json::json!({
                "schema": "push",
                "data": {
                    "refs_pushed": 1
                }
            }),
            "provider-hydrate-copy" => serde_json::json!({
                "schema": "replica.live-hydrate",
                "data": {
                    "copied_objects": 3
                }
            }),
            "provider-hydrate-read-enabled" => serde_json::json!({
                "schema": "replica.wait",
                "data": {
                    "read_enabled": true
                }
            }),
            "provider-hydrate-primary-xorbs-deleted" => serde_json::json!({
                "schema": "replica.live-hydrate",
                "data": {
                    "deleted_xorbs": 1
                }
            }),
            "provider-hydrate-selected-replica" => serde_json::json!({
                "schema": "hydrate",
                "data": {
                    "hydrated": 1,
                    "failed": 0
                }
            }),
            label if label.starts_with("push-") => {
                let operation_id = format!("op-{label}");
                serde_json::json!({
                    "schema": "push",
                    "data": {
                        "refs_pushed": 1,
                        "operation_id": operation_id,
                        "coordinator_epoch": 7,
                        "writer_region": "us-west-2",
                        "commit_state": "materialized"
                    }
                })
            }
            label if label.starts_with("clone-") => {
                let reader_region = reader_region_for_smoke_label(label);
                serde_json::json!({
                    "schema": "clone",
                    "data": {
                        "url": "crab://bucket/repo",
                        "directory": "repo",
                        "branch": label.strip_prefix("clone-").unwrap_or("main"),
                        "lazy": false,
                        "duration_ms": 1,
                        "reader_region": reader_region
                    }
                })
            }
            label if label.starts_with("hydrate-") => {
                let reader_region = reader_region_for_smoke_label(label);
                serde_json::json!({
                    "schema": "hydrate",
                    "data": {
                        "hydrated": 1,
                        "bytes_written": 12,
                        "skipped": 0,
                        "bytes_skipped": 0,
                        "failed": 0,
                        "duration_ms": 1,
                        "reader_region": reader_region
                    }
                })
            }
            _ => serde_json::json!({"schema": "replica.test"}),
        }
    }

    fn reader_region_for_smoke_label(label: &str) -> &'static str {
        if label.contains("feature") || label.contains("crab-live-b") {
            "us-west-2"
        } else {
            "us-east-1"
        }
    }

    fn failover_automation_policy_result() -> serde_json::Value {
        serde_json::json!({
            "automatic_write_failover_supported": false,
            "orchestration": FAILOVER_ORCHESTRATION,
            "split_brain_policy": FAILOVER_SPLIT_BRAIN_POLICY,
            "adr": FAILOVER_ADR
        })
    }

    fn failover_automation_plan_result(action: &str) -> serde_json::Value {
        serde_json::json!({
            "action": action,
            "automatic_apply_supported": matches!(action, "fence" | "repair" | "resume"),
            "reason": "test failover plan",
            "unhealthy_writers": [],
            "repair_verified": false,
            "commands": ["crab replica failover status --json"],
            "required_evidence": [
                "coordinator control-plane status with verified drift checks",
                "coordinator data-plane health from the configured linearizable backend"
            ]
        })
    }

    fn failover_status_result(writes_enabled: bool) -> serde_json::Value {
        serde_json::json!({
            "schema": "replica.failover",
            "data": {
                "active_active": {
                    "writes_enabled": writes_enabled
                },
                "automation_policy": failover_automation_policy_result(),
                "automation_plan": failover_automation_plan_result(if writes_enabled {
                    "monitor"
                } else {
                    "repair"
                })
            }
        })
    }

    fn failover_operation_result(
        operation: &str,
        healthy: bool,
        repair_verified: bool,
    ) -> serde_json::Value {
        serde_json::json!({
            "schema": "replica.failover.operation",
            "data": {
                "operation": operation,
                "applied": true,
                "repair_verified": repair_verified,
                "outcome": {
                    "healthy": healthy
                },
                "automation_policy": failover_automation_policy_result()
            }
        })
    }

    fn failover_run_result(
        operation: &str,
        healthy: bool,
        repair_verified: bool,
    ) -> serde_json::Value {
        serde_json::json!({
            "schema": "replica.failover.run",
            "data": {
                "apply_requested": true,
                "applied": true,
                "active_active": {
                    "writes_enabled": !healthy
                },
                "automation_policy": failover_automation_policy_result(),
                "automation_plan": failover_automation_plan_result(operation),
                "operation": failover_operation_result(operation, healthy, repair_verified)["data"]
                    .clone()
            }
        })
    }

    #[test]
    fn certification_bundle_writer_creates_redacted_json_artifact() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("replication").join("certification.json");
        let mut payload = certification_payload_from_doctor(
            certified_doctor_payload(),
            CertificationProfileArg::Enterprise,
            Some(verified_enterprise_evidence_payload()),
        );

        redact_certification_payload(&mut payload);
        write_certification_bundle(&path, &payload).unwrap();

        let body = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["profile"], "enterprise");
        assert_eq!(parsed["certified"], true);
        assert_eq!(parsed["redacted"], true);
        assert_eq!(parsed["evidence"]["directory"], "<redacted>");
        assert!(body.contains("<redacted>"));
        for secret in [
            "crab://primary/org/repo",
            "s3://replica/org/repo",
            "replica-live-evidence/customer-a",
        ] {
            assert!(!body.contains(secret), "{secret} leaked into certification");
        }
    }

    fn certification_gate_state(
        payload: &CertificationPayload,
        code: &str,
    ) -> CertificationGateState {
        payload
            .gates
            .iter()
            .find(|gate| gate.code == code)
            .map(|gate| gate.state)
            .unwrap_or_else(|| panic!("missing certification gate {code}"))
    }

    #[test]
    fn diagnostics_bundle_writer_creates_nested_json_artifact() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("replication").join("diagnostics.json");
        let payload = diagnostics_payload_from_doctor(doctor_payload_for_test(), true);

        write_diagnostics_bundle(&path, &payload).unwrap();

        let body = std::fs::read_to_string(&path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(parsed["status"]["primary"], "crab://primary/org/repo");
        assert_eq!(parsed["findings"][0]["code"], "replica.unready");
        assert_eq!(
            parsed["fix_plan"][0]["command"],
            "crab replica verify --deep --name west"
        );
    }

    #[test]
    fn diagnostics_publication_key_stays_repo_scoped() {
        assert_eq!(
            diagnostics_publication_key("/org/repo/", 1234, 42),
            "org/repo/diagnostics/replica/1234-42.json"
        );
        assert_eq!(
            diagnostics_publication_key("", 1234, 42),
            "diagnostics/replica/1234-42.json"
        );
    }

    #[tokio::test]
    async fn diagnostics_publish_rejects_unredacted_payload() {
        let payload = diagnostics_payload_from_doctor(doctor_payload_for_test(), true);
        let cancel = CancellationToken::new();

        let err = publish_diagnostics_bundle(&payload, Some("crab://primary/org/repo"), &cancel)
            .await
            .unwrap_err();

        assert!(err.to_string().contains("requires --redact"));
    }

    #[tokio::test]
    async fn diagnostics_publish_rejects_missing_primary() {
        let mut payload = diagnostics_payload_from_doctor(doctor_payload_for_test(), true);
        redact_diagnostics_payload(&mut payload);
        let cancel = CancellationToken::new();

        let err = publish_diagnostics_bundle(&payload, None, &cancel)
            .await
            .unwrap_err();

        assert!(
            err.to_string()
                .contains("requires a configured primary remote")
        );
    }

    #[test]
    fn diagnostics_redaction_removes_publication_metadata() {
        let mut payload = diagnostics_payload_from_doctor(doctor_payload_for_test(), true);
        payload.published = Some(DiagnosticsPublicationPayload {
            primary: "crab://primary/org/repo".into(),
            object_key: "org/repo/diagnostics/replica/1234-42.json".into(),
            redacted: false,
        });

        redact_diagnostics_payload(&mut payload);

        let published = payload.published.as_ref().unwrap();
        assert_eq!(published.primary, "<redacted>");
        assert_eq!(published.object_key, "<redacted>");
        assert!(published.redacted);
    }

    #[test]
    fn diagnostics_redaction_removes_cloud_and_repo_identifiers() {
        let primary = "crab://prod-primary-bucket/acme/secret-repo";
        let replica = "s3://prod-replica-bucket/acme/secret-repo";
        let coordinator_name = "prod-crab-coordinator";
        let coordinator_url = "dynamodb://prod-crab-coordinator";
        let coordinator_target =
            "arn:aws:dynamodb:us-east-1:123456789012:table/prod-crab-coordinator";

        let mut doctor = doctor_payload_for_test();
        doctor.primary = Some(primary.into());
        doctor.replicas[0].url = replica.into();
        doctor.control_plane = vec![ControlPlaneStatus {
            provider: ReplicationProviderKind::S3,
            replica_name: "west".into(),
            primary: primary.into(),
            replica: replica.into(),
            backend_available: true,
            checked_drift: true,
            checks: vec![ControlPlaneCheck {
                provider: ReplicationProviderKind::S3,
                code: "provider.s3.replication-rule".into(),
                state: ControlPlaneCheckState::Verified,
                action: "put-replication-rule".into(),
                target: "arn:aws:s3:::prod-primary-bucket".into(),
                managed_resource_id: "crab:replica:west:prod-primary-bucket".into(),
                message: format!("replication rule for {primary} is verified"),
                remediation: format!(
                    "rerun crab replica add --primary {primary} --replica {replica} --apply"
                ),
                progress_percent: None,
            }],
        }];
        doctor.coordinator = Some(CoordinatorControlPlaneStatus {
            provider: ManagedCoordinatorProvider::DynamoDb,
            name: coordinator_name.into(),
            url: coordinator_url.into(),
            region: "us-east-1".into(),
            failover_regions: vec!["us-west-2".into()],
            backend_available: true,
            checked_drift: true,
            checks: vec![CoordinatorControlPlaneCheck {
                provider: ManagedCoordinatorProvider::DynamoDb,
                code: "coordinator.dynamodb.global-table".into(),
                state: CoordinatorCheckState::Verified,
                action: "create-global-table".into(),
                target: coordinator_target.into(),
                managed_resource_id: "prod-crab-coordinator-global-table".into(),
                message: format!("{coordinator_target} is healthy"),
                remediation: format!(
                    "run crab replica coordinator add --name {coordinator_name} --apply"
                ),
            }],
        });
        doctor.findings[0].message = format!("replica {replica} is lagging behind {primary}");
        doctor.fix_plan[0].command = Some(format!(
            "crab replica add west --primary {primary} --replica {replica} --apply"
        ));
        let mut payload = diagnostics_payload_from_doctor(doctor, true);

        redact_diagnostics_payload(&mut payload);

        let body = serde_json::to_string(&payload).unwrap();
        assert!(payload.redacted);
        assert!(body.contains("<redacted>"));
        assert_eq!(payload.status.replicas[0].name, "west");
        assert_eq!(payload.status.replicas[0].region, "us-west-2");
        for secret in [
            "prod-primary-bucket",
            "prod-replica-bucket",
            "acme/secret-repo",
            "123456789012",
            coordinator_name,
        ] {
            assert!(!body.contains(secret), "{secret} leaked into diagnostics");
        }
    }

    #[test]
    fn repair_watch_rejects_invalid_output_modes_and_interval() {
        let mut args = repair_args();
        args.watch = true;
        args.json = true;
        assert!(validate_repair_args(&args).is_err());

        let mut args = repair_args();
        args.watch = true;
        args.from_coordinator = false;
        assert!(validate_repair_args(&args).is_err());

        let mut args = repair_args();
        args.interval = 0;
        assert!(validate_repair_args(&args).is_err());

        let mut args = repair_args();
        args.watch = true;
        args.samples = Some(0);
        assert!(validate_repair_args(&args).is_err());

        let mut args = repair_args();
        args.samples = Some(1);
        assert!(validate_repair_args(&args).is_err());

        let mut args = repair_args();
        args.watch = true;
        args.samples = Some(1);
        assert!(validate_repair_args(&args).is_ok());
    }

    #[test]
    fn repair_watch_backoff_scales_with_consecutive_errors() {
        assert_eq!(repair_watch_next_interval_seconds(10, 0), 10);
        assert_eq!(repair_watch_next_interval_seconds(10, 1), 20);
        assert_eq!(repair_watch_next_interval_seconds(10, 2), 40);
        assert_eq!(repair_watch_next_interval_seconds(30, 8), 300);
    }

    #[test]
    fn repair_watch_lease_blocks_unexpired_worker_and_reclaims_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let lease_path = tmp.path().join("repair-watch-lease.json");
        let mut args = repair_args();
        args.watch = true;
        args.interval = 10;

        let first = acquire_repair_watch_lease_at(&lease_path, &args, "worker-a", 1_000).unwrap();
        let err = acquire_repair_watch_lease_at(&lease_path, &args, "worker-b", 2_000)
            .unwrap_err()
            .to_string();
        assert!(err.contains("lease is held by worker-a"));
        drop(first);

        let stale = RepairWatchWorkerState {
            schema_version: REPAIR_WATCH_LEASE_SCHEMA_VERSION,
            worker_id: "stale-worker".into(),
            pid: 123,
            lease_path: lease_path.display().to_string(),
            acquired_at_ms: 1_000,
            heartbeat_at_ms: 1_000,
            expires_at_ms: 2_000,
            base_interval_seconds: 10,
            next_interval_seconds: 10,
            consecutive_errors: 0,
            dry_run: false,
        };
        write_repair_watch_lease(&lease_path, &stale).unwrap();
        let reclaimed =
            acquire_repair_watch_lease_at(&lease_path, &args, "worker-b", 3_000).unwrap();

        assert_eq!(reclaimed.state.worker_id, "worker-b");
    }

    #[test]
    fn repair_watch_heartbeat_stops_after_lease_takeover() {
        let tmp = tempfile::tempdir().unwrap();
        let lease_path = tmp.path().join("repair-watch-lease.json");
        let mut args = repair_args();
        args.watch = true;

        let mut guard =
            acquire_repair_watch_lease_at(&lease_path, &args, "worker-a", 1_000).unwrap();
        let takeover = RepairWatchWorkerState {
            schema_version: REPAIR_WATCH_LEASE_SCHEMA_VERSION,
            worker_id: "worker-b".into(),
            pid: 456,
            lease_path: lease_path.display().to_string(),
            acquired_at_ms: 2_000,
            heartbeat_at_ms: 2_000,
            expires_at_ms: 3_000,
            base_interval_seconds: args.interval,
            next_interval_seconds: args.interval,
            consecutive_errors: 0,
            dry_run: false,
        };
        write_repair_watch_lease(&lease_path, &takeover).unwrap();

        let err = guard
            .heartbeat_at(2_100, 0, args.interval)
            .unwrap_err()
            .to_string();
        assert!(err.contains("lease was taken over by worker-b"));
    }

    #[test]
    fn repair_service_template_systemd_runs_jsonl_watch_worker() {
        let mut args = repair_args();
        args.service_template = Some(RepairServiceTemplateArg::Systemd);
        args.service_name = "crab-repair-prod".into();
        args.working_directory = Some(PathBuf::from("/srv/crab repo"));
        args.interval = 45;

        let template = repair_service_template(&args).unwrap();

        assert!(template.contains("Description=Crab active-active replica repair worker"));
        assert!(template.contains("WorkingDirectory=/srv/crab repo"));
        assert!(template.contains(
            "ExecStart=/usr/bin/env crab replica repair --from-coordinator --watch --jsonl --interval 45"
        ));
        assert!(template.contains("Restart=always"));
    }

    #[test]
    fn repair_service_template_launchd_escapes_service_fields() {
        let mut args = repair_args();
        args.service_template = Some(RepairServiceTemplateArg::Launchd);
        args.service_name = "com.example.crab&repair".into();
        args.working_directory = Some(PathBuf::from("/Users/example/Crab & Repo"));

        let template = repair_service_template(&args).unwrap();

        assert!(template.contains("<key>KeepAlive</key><true/>"));
        assert!(template.contains("<string>com.example.crab&amp;repair</string>"));
        assert!(template.contains("<string>/Users/example/Crab &amp; Repo</string>"));
        assert!(template.contains("<string>--jsonl</string>"));
    }

    #[test]
    fn repair_service_template_kubernetes_normalizes_name_and_image() {
        let mut args = repair_args();
        args.service_template = Some(RepairServiceTemplateArg::Kubernetes);
        args.service_name = "Crab Repair Prod!".into();
        args.container_image = "registry.example.com/crab:prod".into();
        args.working_directory = Some(PathBuf::from("/workspace/repo"));
        args.dry_run = true;

        let template = repair_service_template(&args).unwrap();

        assert!(template.contains("name: crab-repair-prod"));
        assert!(template.contains("image: \"registry.example.com/crab:prod\""));
        assert!(template.contains("workingDir: \"/workspace/repo\""));
        assert!(template.contains("        - \"--dry-run\""));
    }

    #[test]
    fn repair_service_template_validation_requires_from_coordinator() {
        let mut args = repair_args();
        args.service_template = Some(RepairServiceTemplateArg::Systemd);
        args.from_coordinator = false;

        let err = validate_repair_args(&args).unwrap_err().to_string();

        assert!(err.contains("--service-template requires --from-coordinator"));
    }

    #[test]
    fn replica_health_transitions_report_state_changes_only() {
        let previous = vec![
            replica_health_for("west", ReplicaHealthState::Ready, "ready"),
            replica_health_for("east", ReplicaHealthState::Lagging, "behind"),
        ];
        let current = vec![
            replica_health_for("west", ReplicaHealthState::PolicyDrift, "rule drifted"),
            replica_health_for("east", ReplicaHealthState::Lagging, "still behind"),
            replica_health_for("central", ReplicaHealthState::Ready, "new replica"),
        ];

        let transitions = replica_health_transitions(2, &previous, &current);

        assert_eq!(transitions.len(), 1);
        assert_eq!(transitions[0].sample, 2);
        assert_eq!(transitions[0].name, "west");
        assert_eq!(transitions[0].previous_state, ReplicaHealthState::Ready);
        assert_eq!(transitions[0].state, ReplicaHealthState::PolicyDrift);
        assert_eq!(transitions[0].reason, "rule drifted");
    }

    #[test]
    fn replica_health_classifies_alert_friendly_states() {
        let mut disabled = replica_status(true);
        disabled.read_enabled = false;
        assert_eq!(
            classify_replica_health(&disabled, None).state,
            ReplicaHealthState::Disabled
        );

        let mut auth_failed = replica_status(true);
        auth_failed.last_fallback_reason = Some("permission denied by replica".into());
        auth_failed.last_fallback_class = Some(ReplicaFallbackClass::Auth);
        assert_eq!(
            classify_replica_health(&auth_failed, Some(&verified_control_plane_status())).state,
            ReplicaHealthState::AuthFailed
        );

        let mut drifted = verified_control_plane_status();
        drifted.checks = vec![provider_check(
            "provider.s3.replication-rule",
            ControlPlaneCheckState::Drifted,
        )];
        assert_eq!(
            classify_replica_health(&replica_status(true), Some(&drifted)).state,
            ReplicaHealthState::PolicyDrift
        );

        let mut backfill = replica_status(true);
        backfill.backfill_required = true;
        let mut backfill_status = verified_control_plane_status();
        backfill_status.checks = vec![provider_check(
            "provider.s3.batch-replication",
            ControlPlaneCheckState::Unknown,
        )];
        assert_eq!(
            classify_replica_health(&backfill, Some(&backfill_status)).state,
            ReplicaHealthState::BackfillRunning
        );

        let mut lagging = replica_status(false);
        lagging.lag_generations = Some(3);
        assert_eq!(
            classify_replica_health(&lagging, Some(&verified_control_plane_status())).state,
            ReplicaHealthState::Lagging
        );

        let mut partial = replica_status(false);
        partial.lag_generations = Some(0);
        partial.last_fallback_reason = Some("missing referenced xorb".into());
        partial.last_fallback_class = Some(ReplicaFallbackClass::MissingObject);
        assert_eq!(
            classify_replica_health(&partial, Some(&verified_control_plane_status())).state,
            ReplicaHealthState::Partial
        );

        let mut ready = replica_status(true);
        ready.lag_generations = Some(0);
        ready.last_fallback_reason = None;
        ready.last_fallback_class = None;
        assert_eq!(
            classify_replica_health(&ready, Some(&verified_control_plane_status())).state,
            ReplicaHealthState::Ready
        );
    }

    #[test]
    fn doctor_findings_report_missing_primary_and_replication_config() {
        let active_active = active_active_status(None);

        let findings = doctor_findings(
            None,
            None,
            &[],
            &[],
            None,
            None,
            None,
            &active_active,
            false,
        );

        assert_eq!(
            finding_with_code(&findings, "replication.primary_missing").severity,
            DoctorSeverity::Error
        );
        assert_eq!(
            finding_with_code(&findings, "replication.not_configured").severity,
            DoctorSeverity::Error
        );
    }

    #[tokio::test]
    async fn doctor_findings_report_unready_replica_and_fallback_history() {
        let replication = crate::replication::ReplicationConfig {
            primary: Some("crab://primary/org/repo".into()),
            replicas: vec![configured_replica(false)],
            ..Default::default()
        };
        let active_active = active_active_status(Some(&replication));

        let findings = doctor_findings(
            replication.primary.as_deref(),
            Some(&replication),
            &[replica_status(false)],
            &[control_plane_status().await],
            None,
            None,
            None,
            &active_active,
            false,
        );

        assert_eq!(
            finding_with_code(&findings, "replica.read_disabled").severity,
            DoctorSeverity::Warning
        );
        assert_eq!(
            finding_with_code(&findings, "replica.not_ready").severity,
            DoctorSeverity::Error
        );
        assert_eq!(
            finding_with_code(&findings, "replica.fallback_observed").severity,
            DoctorSeverity::Warning
        );
        assert_eq!(
            finding_with_code(&findings, "provider.control_plane_unavailable").severity,
            DoctorSeverity::Warning
        );
        assert_eq!(
            finding_with_code(&findings, "replica.cache_hit_unverified").severity,
            DoctorSeverity::Warning
        );
    }

    #[test]
    fn doctor_findings_preserve_provider_status_probe_failure() {
        let replication = crate::replication::ReplicationConfig {
            primary: Some("crab://primary/org/repo".into()),
            replicas: vec![configured_replica(true)],
            ..Default::default()
        };
        let active_active = active_active_status(Some(&replication));
        let mut failed_check = provider_check(
            "provider.s3.replication-rule",
            ControlPlaneCheckState::Unknown,
        );
        failed_check.message =
            "S3 control-plane status failed: AccessDenied missing s3:GetReplicationConfiguration"
                .into();
        let failed_status = ControlPlaneStatus {
            provider: ReplicationProviderKind::S3,
            replica_name: "west".into(),
            primary: "crab://primary/org/repo".into(),
            replica: "s3://replica/org/repo".into(),
            backend_available: false,
            checked_drift: false,
            checks: vec![failed_check],
        };

        let findings = doctor_findings(
            replication.primary.as_deref(),
            Some(&replication),
            &[replica_status(true)],
            &[failed_status],
            None,
            None,
            None,
            &active_active,
            false,
        );

        let finding = finding_with_code(&findings, "provider.control_plane_unavailable");
        assert_eq!(finding.severity, DoctorSeverity::Warning);
        assert!(
            finding
                .message
                .contains("missing s3:GetReplicationConfiguration")
        );
        assert!(
            finding
                .remediation
                .as_deref()
                .is_some_and(|remediation| remediation.contains("credentials"))
        );
    }

    #[test]
    fn doctor_findings_report_benign_cache_hit_when_provider_is_verified() {
        let replication = crate::replication::ReplicationConfig {
            primary: Some("crab://primary/org/repo".into()),
            replicas: vec![configured_replica(true)],
            ..Default::default()
        };
        let active_active = active_active_status(Some(&replication));

        let findings = doctor_findings(
            replication.primary.as_deref(),
            Some(&replication),
            &[replica_status(true)],
            &[verified_control_plane_status()],
            None,
            None,
            None,
            &active_active,
            false,
        );

        assert_eq!(
            finding_with_code(&findings, "replica.cache_hit").severity,
            DoctorSeverity::Info
        );
        assert!(
            findings
                .iter()
                .all(|finding| finding.code != "replica.cache_hit_unverified")
        );
    }

    #[test]
    fn doctor_findings_warn_when_cached_readiness_has_provider_drift() {
        let replication = crate::replication::ReplicationConfig {
            primary: Some("crab://primary/org/repo".into()),
            replicas: vec![configured_replica(true)],
            ..Default::default()
        };
        let active_active = active_active_status(Some(&replication));
        let mut drifted = verified_control_plane_status();
        drifted.checks = vec![provider_check(
            "provider.s3.replication-rule",
            ControlPlaneCheckState::Drifted,
        )];

        let findings = doctor_findings(
            replication.primary.as_deref(),
            Some(&replication),
            &[replica_status(true)],
            &[drifted],
            None,
            None,
            None,
            &active_active,
            false,
        );

        let cache_finding = finding_with_code(&findings, "replica.cache_hit_unverified");
        assert_eq!(cache_finding.severity, DoctorSeverity::Warning);
        assert!(
            cache_finding
                .remediation
                .as_deref()
                .is_some_and(|remediation| remediation.contains("provider check"))
        );
    }

    #[test]
    fn doctor_findings_report_coordinator_state_pressure() {
        let replication = active_active_replication();
        let active_active = ActiveActiveStatus {
            mode: ReplicationMode::ActiveActive,
            coordinator_configured: true,
            coordinator_ready: true,
            writes_enabled: true,
            enabled_writers: 1,
            reason: None,
        };
        let coordinator_health = coordinator_health_with_summary(950, Some(1_000), 80, 100);

        let findings = doctor_findings(
            replication.primary.as_deref(),
            Some(&replication),
            &[],
            &[],
            None,
            None,
            Some(&coordinator_health),
            &active_active,
            true,
        );

        assert_eq!(
            finding_with_code(&findings, "coordinator.state_size_critical").severity,
            DoctorSeverity::Error
        );
        assert_eq!(
            finding_with_code(&findings, "coordinator.completed_operations_high").severity,
            DoctorSeverity::Warning
        );
    }

    #[test]
    fn doctor_findings_warn_before_coordinator_state_is_critical() {
        let replication = active_active_replication();
        let active_active = ActiveActiveStatus {
            mode: ReplicationMode::ActiveActive,
            coordinator_configured: true,
            coordinator_ready: true,
            writes_enabled: true,
            enabled_writers: 1,
            reason: None,
        };
        let coordinator_health = coordinator_health_with_summary(800, Some(1_000), 1, 100);

        let findings = doctor_findings(
            replication.primary.as_deref(),
            Some(&replication),
            &[],
            &[],
            None,
            None,
            Some(&coordinator_health),
            &active_active,
            true,
        );

        assert_eq!(
            finding_with_code(&findings, "coordinator.state_size_high").severity,
            DoctorSeverity::Warning
        );
        assert!(
            findings
                .iter()
                .all(|finding| finding.code != "coordinator.state_size_critical")
        );
    }

    #[test]
    fn doctor_fix_plan_recommends_verify_and_wait_for_disabled_unready_replica() {
        let replication = crate::replication::ReplicationConfig {
            primary: Some("crab://primary/org/repo".into()),
            replicas: vec![configured_replica(false)],
            ..Default::default()
        };
        let active_active = active_active_status(Some(&replication));
        let findings = doctor_findings(
            replication.primary.as_deref(),
            Some(&replication),
            &[replica_status(false)],
            &[],
            None,
            None,
            None,
            &active_active,
            false,
        );

        let plan = doctor_fix_plan(
            replication.primary.as_deref(),
            Some(&replication),
            &[],
            None,
            &findings,
        );

        assert!(plan.iter().any(|action| {
            action.code == "replica.read_disabled"
                && action.command.as_deref() == Some("crab replica wait west --enable-read")
        }));
        assert!(plan.iter().any(|action| {
            action.code == "replica.not_ready"
                && action.command.as_deref() == Some("crab replica verify --deep --name west")
        }));
    }

    #[test]
    fn doctor_fix_plan_recommends_deep_doctor_for_unverified_cache_hit() {
        let replication = crate::replication::ReplicationConfig {
            primary: Some("crab://primary/org/repo".into()),
            replicas: vec![configured_replica(true)],
            ..Default::default()
        };
        let findings = vec![finding(
            "replica.cache_hit_unverified",
            DoctorSeverity::Warning,
            "replica west readiness came from local cache while provider status is not verified",
            Some("west".into()),
            Some("provider check drifted".into()),
        )];

        let plan = doctor_fix_plan(
            replication.primary.as_deref(),
            Some(&replication),
            &[],
            None,
            &findings,
        );

        assert_eq!(
            plan[0].command.as_deref(),
            Some("crab replica doctor --deep --fix-plan")
        );
    }

    #[test]
    fn doctor_fix_plan_recommends_provider_apply_for_missing_control_plane_check() {
        let replication = crate::replication::ReplicationConfig {
            primary: Some("crab://primary/org/repo".into()),
            replicas: vec![configured_replica(false)],
            ..Default::default()
        };
        let check = ControlPlaneCheck {
            provider: ReplicationProviderKind::S3,
            code: "provider.s3.replication-rule".into(),
            state: ControlPlaneCheckState::Missing,
            action: "put-replication".into(),
            target: "s3://replica/org/repo".into(),
            managed_resource_id: "crab:replica:west:replication-rule".into(),
            message: "replication rule is missing".into(),
            remediation: "rerun crab replica add --apply".into(),
            progress_percent: None,
        };
        let status = ControlPlaneStatus {
            provider: ReplicationProviderKind::S3,
            replica_name: "west".into(),
            primary: "crab://primary/org/repo".into(),
            replica: "s3://replica/org/repo".into(),
            backend_available: true,
            checked_drift: true,
            checks: vec![check.clone()],
        };
        let findings = vec![finding(
            &check.code,
            DoctorSeverity::Error,
            check.message.clone(),
            Some("west".into()),
            Some(check.remediation.clone()),
        )];

        let plan = doctor_fix_plan(
            replication.primary.as_deref(),
            Some(&replication),
            &[status],
            None,
            &findings,
        );

        assert_eq!(
            plan[0].command.as_deref(),
            Some(
                "crab replica add west --provider s3 --primary crab://primary/org/repo --replica s3://replica/org/repo --region us-west-2 --rpo standard --apply"
            )
        );
        assert!(
            plan[0]
                .cost_hints
                .iter()
                .any(|hint| hint.contains("replicated storage"))
        );
        assert!(
            plan[0]
                .risk_hints
                .iter()
                .any(|hint| hint.contains("KMS keys"))
        );
    }

    #[test]
    fn doctor_fix_plan_recommends_coordinator_dry_run_for_drift() {
        let check = CoordinatorControlPlaneCheck {
            provider: ManagedCoordinatorProvider::DynamoDb,
            code: "coordinator.dynamodb.global-table".into(),
            state: CoordinatorCheckState::Drifted,
            action: "update-table".into(),
            target: "dynamodb://crab-coordinator".into(),
            managed_resource_id: "crab:coordinator:crab-coordinator".into(),
            message: "coordinator table drifted".into(),
            remediation: "review coordinator drift".into(),
        };
        let status = CoordinatorControlPlaneStatus {
            provider: ManagedCoordinatorProvider::DynamoDb,
            name: "crab-coordinator".into(),
            url: "dynamodb://crab-coordinator".into(),
            region: "us-east-1".into(),
            failover_regions: vec!["us-west-2".into()],
            backend_available: true,
            checked_drift: true,
            checks: vec![check.clone()],
        };
        let findings = vec![finding(
            &check.code,
            DoctorSeverity::Error,
            check.message.clone(),
            None,
            Some(check.remediation.clone()),
        )];

        let plan = doctor_fix_plan(None, None, &[], Some(&status), &findings);

        assert_eq!(
            plan[0].command.as_deref(),
            Some(
                "crab replica coordinator add --provider dynamodb --name crab-coordinator --region us-east-1 --failover-region us-west-2 --dry-run --json"
            )
        );
        assert!(
            plan[0]
                .cost_hints
                .iter()
                .any(|hint| hint.contains("DynamoDB Global Table"))
        );
        assert!(
            plan[0]
                .risk_hints
                .iter()
                .any(|hint| hint.contains("MRSC global tables"))
        );
    }

    #[test]
    fn shell_arg_quotes_spaces_and_single_quotes() {
        assert_eq!(shell_arg("west region's"), "'west region'\\''s'");
        assert_eq!(
            shell_arg("crab://primary/org/repo"),
            "crab://primary/org/repo"
        );
    }

    #[test]
    fn cost_estimate_includes_fast_rpo_and_backfill_meters() {
        let replica = ReplicaConfig {
            provider: ReplicationProviderKind::Gcs,
            rpo: ReplicationRpo::Fast,
            backfill: true,
            ..configured_replica(false)
        };

        let estimate = replica_cost_estimate(
            &replica,
            CostAssumptions {
                monthly_write_gb: 120.0,
                monthly_read_gb: 30.0,
                backfill_gb: 500.0,
                monthly_requests_million: 2.5,
            },
        );

        assert!(
            estimate
                .meters
                .iter()
                .any(|meter| meter.code == "gcs.turbo-replication")
        );
        assert!(
            estimate
                .meters
                .iter()
                .any(|meter| meter.code == "gcs.storage-transfer")
        );
        assert!(
            estimate
                .warnings
                .iter()
                .any(|warning| warning.contains("read-egress"))
        );
    }

    #[test]
    fn cost_payload_totals_scale_across_selected_replicas() {
        let east = ReplicaConfig {
            name: "east".into(),
            region: "us-east-1".into(),
            ..configured_replica(true)
        };
        let replication = crate::replication::ReplicationConfig {
            primary: Some("crab://primary/org/repo".into()),
            replicas: vec![configured_replica(true), east],
            ..Default::default()
        };

        let payload = cost_payload(
            Some("crab://primary/org/repo".into()),
            &replication,
            None,
            CostAssumptions {
                monthly_write_gb: 10.0,
                monthly_read_gb: 20.0,
                backfill_gb: 30.0,
                monthly_requests_million: 4.0,
            },
        )
        .unwrap();

        assert_eq!(payload.totals.replicas, 2);
        assert_eq!(payload.totals.monthly_replicated_write_gb, 20.0);
        assert_eq!(payload.totals.monthly_replica_read_gb, 40.0);
        assert_eq!(payload.totals.one_time_backfill_gb, 60.0);
        assert_eq!(payload.totals.monthly_request_millions, 8.0);
    }

    #[test]
    fn cost_assumptions_reject_negative_and_nan_quantities() {
        let negative = CostArgs {
            name: None,
            monthly_write_gb: -1.0,
            monthly_read_gb: 0.0,
            backfill_gb: 0.0,
            monthly_requests_million: 0.0,
            json: false,
        };
        let nan = CostArgs {
            name: None,
            monthly_write_gb: f64::NAN,
            monthly_read_gb: 0.0,
            backfill_gb: 0.0,
            monthly_requests_million: 0.0,
            json: false,
        };

        assert!(cost_assumptions_from_args(&negative).is_err());
        assert!(cost_assumptions_from_args(&nan).is_err());
    }

    #[test]
    fn runbook_primary_outage_for_read_replica_promotes_after_verification() {
        let replication = crate::replication::ReplicationConfig {
            primary: Some("crab://primary/org/repo".into()),
            replicas: vec![configured_write_replica(true)],
            ..Default::default()
        };

        let payload = runbook_payload(
            RunbookScenarioArg::PrimaryOutage,
            Some("crab://primary/org/repo".into()),
            Some(&replication),
            Some("west"),
        )
        .unwrap();

        assert_eq!(payload.scenario, RunbookScenarioArg::PrimaryOutage);
        assert!(payload.steps.iter().any(|step| {
            step.command.as_deref() == Some("crab replica promote west")
                && step.requires_external_verification
        }));
        assert!(payload.steps.iter().any(|step| {
            step.command
                .as_deref()
                .is_some_and(|command| command.contains("set-primary crab://replica/org/repo"))
        }));
    }

    #[test]
    fn runbook_primary_outage_for_active_active_fences_before_repair() {
        let replication = active_active_replication();

        let payload = runbook_payload(
            RunbookScenarioArg::PrimaryOutage,
            Some("crab://primary/org/repo".into()),
            Some(&replication),
            None,
        )
        .unwrap();

        assert_eq!(payload.mode, Some(ReplicationMode::ActiveActive));
        assert_eq!(
            payload.steps[1].command.as_deref(),
            Some("crab replica failover fence --apply --reason primary-outage")
        );
        assert!(payload.steps.iter().any(|step| step.command.as_deref()
            == Some("crab replica repair --from-coordinator --dry-run --json")));
    }

    #[test]
    fn runbook_warns_when_multiple_replicas_need_name() {
        let east = ReplicaConfig {
            name: "east".into(),
            region: "us-east-1".into(),
            ..configured_replica(true)
        };
        let replication = crate::replication::ReplicationConfig {
            primary: Some("crab://primary/org/repo".into()),
            replicas: vec![configured_replica(true), east],
            ..Default::default()
        };

        let payload = runbook_payload(
            RunbookScenarioArg::ReplicaStale,
            Some("crab://primary/org/repo".into()),
            Some(&replication),
            None,
        )
        .unwrap();

        assert!(
            payload
                .warnings
                .iter()
                .any(|warning| warning.contains("--name <replica>"))
        );
        assert!(
            payload
                .steps
                .iter()
                .any(|step| step.command.as_deref() == Some("crab replica disable <name>"))
        );
    }

    #[test]
    fn runbook_destination_writes_marks_bucket_cleanup_as_destructive() {
        let replication = crate::replication::ReplicationConfig {
            primary: Some("crab://primary/org/repo".into()),
            replicas: vec![configured_replica(true)],
            ..Default::default()
        };

        let payload = runbook_payload(
            RunbookScenarioArg::DestinationWrites,
            Some("crab://primary/org/repo".into()),
            Some(&replication),
            Some("west"),
        )
        .unwrap();

        assert!(payload.steps.iter().any(|step| {
            step.title == "Avoid bucket-wide Crab GC during the incident" && step.destructive
        }));
    }

    #[test]
    fn doctor_findings_suppress_cache_hit_when_deep_checks_run() {
        let replication = crate::replication::ReplicationConfig {
            primary: Some("crab://primary/org/repo".into()),
            replicas: vec![configured_replica(true)],
            ..Default::default()
        };
        let active_active = active_active_status(Some(&replication));

        let findings = doctor_findings(
            replication.primary.as_deref(),
            Some(&replication),
            &[replica_status(true)],
            &[],
            None,
            None,
            None,
            &active_active,
            true,
        );

        assert!(
            findings
                .iter()
                .all(|finding| finding.code != "replica.cache_hit")
        );
    }

    #[test]
    fn verify_failure_reason_names_unready_replicas() {
        let reason = verify_failure_reason(&[replica_status(false)]).unwrap();

        assert!(reason.contains("west"));
        assert!(reason.contains("replica manifest is stale"));
    }

    #[test]
    fn verify_failure_reason_is_none_when_all_replicas_ready() {
        let reason = verify_failure_reason(&[replica_status(true)]);

        assert!(reason.is_none());
    }

    #[test]
    fn verify_summary_reports_cutover_inventory_and_blockers() {
        let mut ready = replica_status(true);
        ready.name = "east".into();
        ready.provider = ReplicationProviderKind::Gcs;
        ready.region = "us-east1".into();
        ready.read_enabled = false;
        ready.lag_generations = Some(0);
        ready.readiness_object_probe_count = 3;
        ready.readiness_object_read_count = 1;
        ready.primary_fallback_bytes = 0;

        let mut blocked = replica_status(false);
        blocked.name = "west".into();
        blocked.provider = ReplicationProviderKind::S3;
        blocked.region = "us-west-2".into();
        blocked.readiness_object_probe_count = 5;
        blocked.readiness_object_read_count = 2;
        blocked.primary_fallback_bytes = 1024;

        let summary = verify_summary(&[ready, blocked], VerifyProofMode::Exhaustive, None);

        assert_eq!(summary.proof_mode, VerifyProofMode::Exhaustive);
        assert!(summary.exhaustive);
        assert_eq!(summary.sample_size, None);
        assert_eq!(summary.replica_count, 2);
        assert_eq!(summary.ready_count, 1);
        assert_eq!(summary.not_ready_count, 1);
        assert_eq!(summary.read_enabled_count, 1);
        assert_eq!(summary.max_lag_generations, Some(1));
        assert_eq!(summary.readiness_object_probe_count, 8);
        assert_eq!(summary.readiness_object_read_count, 3);
        assert_eq!(summary.primary_fallback_bytes, 1024);
        assert!(!summary.cutover_ready);
        assert_eq!(
            summary.cutover_blockers,
            vec!["west: replica manifest is stale"]
        );
        assert_eq!(summary.provider_inventory.len(), 2);
        assert_eq!(
            summary.provider_inventory[0].provider,
            ReplicationProviderKind::Gcs
        );
        assert_eq!(summary.provider_inventory[0].regions, vec!["us-east1"]);
        assert_eq!(
            summary.provider_inventory[1].provider,
            ReplicationProviderKind::S3
        );
        assert_eq!(summary.provider_inventory[1].not_ready_count, 1);
    }

    #[test]
    fn verify_summary_marks_all_ready_replicas_cutover_ready() {
        let mut ready = replica_status(true);
        ready.lag_generations = Some(0);
        ready.last_fallback_reason = None;
        ready.primary_fallback_bytes = 0;

        let summary = verify_summary(&[ready], VerifyProofMode::Exhaustive, None);

        assert!(summary.cutover_ready);
        assert!(summary.cutover_blockers.is_empty());
        assert_eq!(summary.not_ready_count, 0);
    }

    #[test]
    fn verify_summary_marks_sampled_runs_not_cutover_ready() {
        let mut ready = replica_status(true);
        ready.lag_generations = Some(0);
        ready.last_fallback_reason = None;

        let summary = verify_summary(&[ready], VerifyProofMode::Sampled, Some(25));

        assert_eq!(summary.proof_mode, VerifyProofMode::Sampled);
        assert!(!summary.exhaustive);
        assert_eq!(summary.sample_size, Some(25));
        assert!(!summary.cutover_ready);
        assert_eq!(
            summary.cutover_blockers,
            vec![
                "verification used a bounded object sample; rerun with --exhaustive for cutover proof"
            ]
        );
    }

    #[test]
    fn verify_sample_size_rejects_zero() {
        let args = VerifyArgs {
            name: None,
            deep: true,
            exhaustive: false,
            sample_size: Some(0),
            json: false,
        };

        let error = verify_sample_size(&args).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("sample size must be greater than zero")
        );
    }

    #[tokio::test]
    async fn prometheus_status_exports_replica_and_control_plane_metrics() {
        let replicas = vec![replica_status(true)];
        let control_plane = vec![control_plane_status().await];
        let health = replica_health(&replicas, &control_plane);
        let mut backfill_status =
            backfill_replica_status(&configured_replica(true), control_plane.first());
        backfill_status.progress_percent = Some(90);
        let backfill = vec![backfill_status];
        let payload = StatusPayload {
            primary: Some("crab://primary/org/repo".into()),
            replicas,
            health,
            backfill,
            control_plane,
        };

        let output = prometheus_status(&payload);

        assert!(output.contains(
            "crab_replica_ready{replica=\"west\",provider=\"s3\",region=\"us-west-2\"} 1"
        ));
        assert!(output.contains(
            "crab_replica_generation_lag{replica=\"west\",provider=\"s3\",region=\"us-west-2\"} 1"
        ));
        assert!(output.contains(
            "crab_replica_selected_total{replica=\"west\",provider=\"s3\",region=\"us-west-2\"} 3"
        ));
        assert!(output.contains(
            "crab_replica_last_selected_timestamp_ms{replica=\"west\",provider=\"s3\",region=\"us-west-2\"} 111"
        ));
        assert!(output.contains(
            "crab_replica_readiness_latency_ms{replica=\"west\",provider=\"s3\",region=\"us-west-2\"} 17"
        ));
        assert!(output.contains(
            "crab_replica_readiness_object_probe_total{replica=\"west\",provider=\"s3\",region=\"us-west-2\"} 5"
        ));
        assert!(output.contains(
            "crab_replica_readiness_object_read_total{replica=\"west\",provider=\"s3\",region=\"us-west-2\"} 2"
        ));
        assert!(output.contains(
            "crab_replica_last_fallback_class{replica=\"west\",provider=\"s3\",region=\"us-west-2\",class=\"stale-manifest\"} 1"
        ));
        assert!(output.contains(
            "crab_replica_primary_fallback_bytes_total{replica=\"west\",provider=\"s3\",region=\"us-west-2\"} 1024"
        ));
        assert!(output.contains(
            "crab_replica_control_plane_backend_available{replica=\"west\",provider=\"s3\"} 0"
        ));
        assert!(output.contains(
            "crab_replica_health_state{replica=\"west\",provider=\"s3\",region=\"us-west-2\",state=\"lagging\"} 1"
        ));
        assert!(output.contains(
            "crab_replica_backfill_state{replica=\"west\",provider=\"s3\",region=\"us-west-2\",state=\"not-required\"} 1"
        ));
        assert!(output.contains(
            "crab_replica_backfill_progress_percent{replica=\"west\",provider=\"s3\",region=\"us-west-2\"} 90"
        ));
        assert!(output.contains("crab_replica_control_plane_check_ok{"));
    }

    #[test]
    fn prometheus_label_value_escapes_special_characters() {
        let escaped = prometheus_label_value("west\"region\\line\nnext");

        assert_eq!(escaped, "west\\\"region\\\\line\\nnext");
    }

    #[test]
    fn promotion_requires_read_enabled_replica_unless_forced() {
        let replica = configured_replica(false);

        let err = ensure_replica_promotable(&replica, false).unwrap_err();

        assert!(err.to_string().contains("not read-enabled"));
        ensure_replica_promotable(&replica, true).unwrap();
    }

    #[test]
    fn promotion_allows_read_enabled_replica() {
        let replica = configured_replica(true);

        ensure_replica_promotable(&replica, false).unwrap();
    }

    #[test]
    fn promotion_plan_blocks_non_crab_url_and_missing_read_proof() {
        let replica = configured_replica(false);

        let checks = promote_plan_checks(
            &replica,
            "crab://primary/org/repo",
            false,
            false,
            Some(&verified_control_plane_status()),
        );

        assert!(
            checks
                .iter()
                .any(|check| check.code == "promote.url.write-safe" && check.blocking)
        );
        assert!(
            checks
                .iter()
                .any(|check| check.code == "promote.read-enabled" && check.blocking)
        );
    }

    #[test]
    fn promotion_plan_warns_for_force_and_blocks_provider_drift() {
        let replica = configured_replica(false);
        let mut drifted = verified_control_plane_status();
        drifted.checks = vec![provider_check(
            "provider.s3.replication-rule",
            ControlPlaneCheckState::Drifted,
        )];

        let checks = promote_plan_checks(
            &replica,
            "crab://primary/org/repo",
            true,
            true,
            Some(&drifted),
        );

        assert!(
            checks
                .iter()
                .any(|check| check.code == "promote.read-enabled"
                    && check.state == PromotePlanCheckState::Warning
                    && !check.blocking)
        );
        assert!(
            checks
                .iter()
                .any(|check| check.code == "promote.provider-control-plane" && check.blocking)
        );
    }

    #[test]
    fn promotion_planned_actions_include_verify_wait_and_force_command() {
        let actions = promote_planned_actions("west", true, false, true);

        assert_eq!(actions[0], "crab replica verify --deep --name west");
        assert!(
            actions
                .iter()
                .any(|action| action == "crab replica wait west --enable-read")
        );
        assert!(
            actions
                .iter()
                .any(|action| action == "crab replica promote west --force")
        );
    }

    #[test]
    fn replica_promote_audit_appends_local_event() {
        let dir = tempfile::tempdir().unwrap();
        let payload = PromotePayload {
            name: "west".to_owned(),
            old_primary: "crab://primary/org/repo".to_owned(),
            new_primary: "crab://replica/org/repo".to_owned(),
            dry_run: false,
            forced: true,
            read_enabled: false,
            new_primary_is_crab_url: true,
            provider: ReplicationProviderKind::S3,
            region: "us-west-2".to_owned(),
            plan_ready: true,
            plan_checks: vec![promote_plan_check(
                "promote.read-enabled",
                PromotePlanCheckState::Warning,
                "forced promotion without local read proof",
                "verify externally",
            )],
            planned_actions: vec!["crab replica promote west --force".to_owned()],
            control_plane: None,
        };

        record_replica_promote_audit(dir.path(), &payload).unwrap();

        let events = crate::audit::read_events(&dir.path().join(default_log_path())).unwrap();
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.operation, "replica.promote");
        assert_eq!(event.outcome, AuditOutcome::Success);
        assert_eq!(event.repository.as_deref(), Some("crab://replica/org/repo"));
        assert_eq!(event.details["name"], "west");
        assert_eq!(event.details["forced"], true);
        assert_eq!(
            event.details["plan_checks"][0]["code"],
            "promote.read-enabled"
        );
        assert!(event.digest_valid());
    }

    #[test]
    fn set_primary_plan_blocks_non_crab_url_and_unconfigured_target() {
        let checks = set_primary_plan_checks(None, "crab://primary/org/repo", false, false, None);

        assert!(
            checks
                .iter()
                .any(|check| check.code == "set-primary.url.write-safe" && check.blocking)
        );
        assert!(
            checks
                .iter()
                .any(|check| check.code == "set-primary.configured-target" && check.blocking)
        );
    }

    #[test]
    fn set_primary_plan_allows_forced_unconfigured_crab_url_with_warning() {
        let checks = set_primary_plan_checks(None, "crab://primary/org/repo", true, true, None);

        assert!(!checks.iter().any(|check| check.blocking));
        assert!(checks.iter().any(|check| {
            check.code == "set-primary.configured-target"
                && check.state == PromotePlanCheckState::Warning
        }));
    }

    #[test]
    fn set_primary_plan_blocks_provider_drift_for_configured_replica() {
        let replica = configured_write_replica(true);
        let mut drifted = verified_control_plane_status();
        drifted.checks = vec![provider_check(
            "provider.s3.replication-rule",
            ControlPlaneCheckState::Drifted,
        )];

        let checks = set_primary_plan_checks(
            Some(&replica),
            "crab://primary/org/repo",
            true,
            false,
            Some(&drifted),
        );

        assert!(
            checks.iter().any(|check| {
                check.code == "set-primary.provider-control-plane" && check.blocking
            })
        );
    }

    #[test]
    fn set_primary_planned_actions_include_force_apply_command() {
        let actions = set_primary_planned_actions(
            "crab://replica/org/repo",
            Some(&configured_write_replica(false)),
            true,
            true,
        );

        assert_eq!(actions[0], "crab replica verify --deep --name west");
        assert!(
            actions
                .iter()
                .any(|action| action == "crab replica wait west --enable-read")
        );
        assert!(actions.iter().any(|action| {
            action == "crab replica set-primary crab://replica/org/repo --force --apply"
        }));
    }

    #[test]
    fn set_primary_apply_updates_remote_and_replication_primary() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".crab.toml");
        let mut project = ProjectConfig {
            remote: RemoteConfig {
                url: "crab://primary/org/repo".into(),
            },
            track: None,
            hydrate: None,
            mirror: None,
            replication: Some(crate::replication::ReplicationConfig {
                primary: Some("crab://primary/org/repo".into()),
                replicas: vec![configured_write_replica(true)],
                ..Default::default()
            }),
            auth: None,
        };

        write_primary_to_project_config(&path, &mut project, "crab://replica/org/repo").unwrap();
        let loaded = ProjectConfig::load(&path).unwrap();

        assert_eq!(loaded.remote.url, "crab://replica/org/repo");
        assert_eq!(
            loaded.replication.unwrap().primary.as_deref(),
            Some("crab://replica/org/repo")
        );
    }

    #[tokio::test]
    async fn doctor_findings_report_unverified_backfill() {
        let replica = backfill_replica();
        let replication = crate::replication::ReplicationConfig {
            primary: Some("crab://primary/org/repo".into()),
            replicas: vec![replica],
            ..Default::default()
        };
        let control_plane =
            control_plane_statuses(replication.primary.as_deref(), Some(&replication)).await;
        let active_active = active_active_status(Some(&replication));

        let findings = doctor_findings(
            replication.primary.as_deref(),
            Some(&replication),
            &[],
            &control_plane,
            None,
            None,
            None,
            &active_active,
            true,
        );

        assert_eq!(
            finding_with_code(&findings, "replica.backfill_unverified").severity,
            DoctorSeverity::Warning
        );
    }

    #[tokio::test]
    async fn backfill_status_reports_unknown_required_backfill() {
        let replication = crate::replication::ReplicationConfig {
            primary: Some("crab://primary/org/repo".into()),
            replicas: vec![backfill_replica()],
            ..Default::default()
        };
        let mut config = Config::default();
        config.replication = Some(replication);

        let payload = backfill_payload(Some("crab://primary/org/repo"), &config, Some("west"))
            .await
            .unwrap();
        let status = &payload.replicas[0];

        assert_eq!(status.state, BackfillState::Unknown);
        assert!(status.blocks_read_enable);
        assert_eq!(
            status.check_code.as_deref(),
            Some("provider.s3.batch-replication.unverified")
        );
        assert_eq!(status.progress_percent, None);
    }

    #[test]
    fn backfill_status_preserves_provider_progress_percent() {
        let mut check = provider_check(
            "provider.azure.backfill.unverified",
            ControlPlaneCheckState::Missing,
        );
        check.action = "track-existing-blob-backfill".into();
        check.progress_percent = Some(90);
        let control_plane = ControlPlaneStatus {
            provider: ReplicationProviderKind::Azure,
            replica_name: "west".into(),
            primary: "azure://primary/org/repo".into(),
            replica: "azure://replica/org/repo".into(),
            backend_available: true,
            checked_drift: true,
            checks: vec![check],
        };
        let mut replica = backfill_replica();
        replica.provider = ReplicationProviderKind::Azure;
        replica.url = "azure://replica/org/repo".into();

        let status = backfill_replica_status(&replica, Some(&control_plane));

        assert_eq!(status.state, BackfillState::Missing);
        assert_eq!(status.progress_percent, Some(90));
    }

    #[tokio::test]
    async fn backfill_status_reports_not_required_for_incremental_replica() {
        let replication = crate::replication::ReplicationConfig {
            primary: Some("crab://primary/org/repo".into()),
            replicas: vec![configured_replica(false)],
            ..Default::default()
        };
        let mut config = Config::default();
        config.replication = Some(replication);

        let payload = backfill_payload(Some("crab://primary/org/repo"), &config, None)
            .await
            .unwrap();
        let status = &payload.replicas[0];

        assert_eq!(status.state, BackfillState::NotRequired);
        assert!(!status.blocks_read_enable);
    }

    #[tokio::test]
    async fn backfill_status_rejects_unknown_replica() {
        let replication = crate::replication::ReplicationConfig {
            primary: Some("crab://primary/org/repo".into()),
            replicas: vec![configured_replica(false)],
            ..Default::default()
        };
        let mut config = Config::default();
        config.replication = Some(replication);

        let err = backfill_payload(Some("crab://primary/org/repo"), &config, Some("east"))
            .await
            .unwrap_err();

        assert!(matches!(err, CrabError::Configuration { .. }));
    }

    #[tokio::test]
    async fn doctor_findings_report_blocked_active_active_writes() {
        let replication = active_active_replication();
        let active_active = active_active_status(Some(&replication));
        let coordinator = coordinator_control_plane_status_with_backends(
            Some(&replication),
            &NoCoordinatorBackends,
        )
        .await;

        let findings = doctor_findings(
            replication.primary.as_deref(),
            Some(&replication),
            &[],
            &[],
            coordinator.as_ref(),
            None,
            None,
            &active_active,
            false,
        );

        assert_eq!(
            finding_with_code(&findings, "active_active.writes_blocked").severity,
            DoctorSeverity::Error
        );
        assert_eq!(
            finding_with_code(&findings, "coordinator.control_plane_unavailable").severity,
            DoctorSeverity::Warning
        );
    }

    #[test]
    fn doctor_findings_do_not_report_crab_auth_active_active_blocker() {
        let replication = active_active_replication();
        let active_active = ActiveActiveStatus {
            mode: ReplicationMode::ActiveActive,
            coordinator_configured: true,
            coordinator_ready: true,
            writes_enabled: true,
            enabled_writers: 1,
            reason: None,
        };

        let findings = doctor_findings(
            replication.primary.as_deref(),
            Some(&replication),
            &[],
            &[],
            None,
            None,
            None,
            &active_active,
            false,
        );
        assert!(
            findings
                .iter()
                .all(|finding| finding.code != "active_active.crab_auth_unsupported")
        );
        assert!(
            findings
                .iter()
                .all(|finding| finding.code != "active_active.writes_blocked")
        );
    }

    #[tokio::test]
    async fn doctor_findings_preserve_coordinator_status_probe_failure() {
        let replication = active_active_replication();
        let backend = FailingCoordinatorBackend::new(
            ManagedCoordinatorProvider::DynamoDb,
            "AccessDenied: missing dynamodb:DescribeTable",
        );
        let backends = SingleCoordinatorBackend { backend: &backend };

        let probe =
            coordinator_control_plane_probe_with_backends(Some(&replication), &backends).await;
        let mut active_active =
            active_active_status_with_coordinator_status(Some(&replication), probe.status.as_ref());
        apply_coordinator_probe_error_to_active_active_status(
            &mut active_active,
            probe.error.as_deref(),
        );
        let findings = doctor_findings(
            replication.primary.as_deref(),
            Some(&replication),
            &[],
            &[],
            probe.status.as_ref(),
            probe.error.as_deref(),
            None,
            &active_active,
            false,
        );

        assert!(!active_active.writes_enabled);
        assert!(
            active_active
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("missing dynamodb:DescribeTable"))
        );
        let finding = finding_with_code(&findings, "coordinator.status_probe_failed");
        assert_eq!(finding.severity, DoctorSeverity::Error);
        assert!(finding.message.contains("missing dynamodb:DescribeTable"));
    }

    #[tokio::test]
    async fn coordinator_control_plane_status_infers_dynamodb_backend() {
        let replication = active_active_replication();

        let status = coordinator_control_plane_status_with_backends(
            Some(&replication),
            &NoCoordinatorBackends,
        )
        .await
        .unwrap();

        assert_eq!(status.provider, ManagedCoordinatorProvider::DynamoDb);
        assert_eq!(status.name, "crab-coordinator");
        assert!(!status.backend_available);
        assert!(
            status
                .checks
                .iter()
                .any(|check| check.code == "coordinator.dynamodb.global-table.unverified")
        );
    }

    #[cfg(feature = "coordinator-cosmosdb")]
    #[test]
    fn default_coordinator_backends_register_cosmosdb() {
        let backends = DefaultCoordinatorBackends::new();

        assert!(
            backends
                .backend_for(ManagedCoordinatorProvider::CosmosDb)
                .is_some()
        );
    }

    #[cfg(feature = "coordinator-spanner")]
    #[test]
    fn default_coordinator_backends_register_spanner() {
        let backends = DefaultCoordinatorBackends::new();

        assert!(
            backends
                .backend_for(ManagedCoordinatorProvider::Spanner)
                .is_some()
        );
    }

    #[test]
    fn coordinator_plan_from_explicit_target_requires_complete_args() {
        let err = coordinator_plan_from_args_or_config(
            Some(CoordinatorProviderArg::Dynamodb),
            None,
            Some("us-east-1"),
            &[],
            None,
        )
        .unwrap_err();

        assert!(matches!(err, CrabError::Configuration { .. }));
    }

    #[test]
    fn coordinator_plan_from_config_reports_configured_dynamodb() {
        let project = ProjectConfig {
            remote: RemoteConfig {
                url: "crab://primary/org/repo".into(),
            },
            track: None,
            hydrate: None,
            mirror: None,
            replication: Some(active_active_replication()),
            auth: None,
        };

        let (plan, configured) =
            coordinator_plan_from_args_or_config(None, None, None, &[], Some(&project)).unwrap();

        assert!(configured);
        assert_eq!(plan.provider, ManagedCoordinatorProvider::DynamoDb);
        assert_eq!(plan.url, "dynamodb://crab-coordinator");
    }

    #[test]
    fn remove_coordinator_from_project_config_disables_active_active() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".crab.toml");
        let project = ProjectConfig {
            remote: RemoteConfig {
                url: "crab://primary/org/repo".into(),
            },
            track: None,
            hydrate: None,
            mirror: None,
            replication: Some(active_active_replication()),
            auth: None,
        };
        ProjectConfig::write(&path, &project).unwrap();

        assert!(remove_coordinator_from_project_config(&path).unwrap());
        let loaded = ProjectConfig::load(&path).unwrap();
        let replication = loaded.replication.unwrap();
        assert_eq!(replication.mode, ReplicationMode::ReadReplica);
        assert!(replication.coordinator.is_none());
        assert!(replication.writers.is_empty());
    }

    #[test]
    fn add_replica_writes_project_config() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".crab.toml");
        add_replica_to_project_config(
            &path,
            &add_args("s3://replica/org/repo"),
            ReplicationProviderKind::S3,
            ReplicationRpo::Standard,
        )
        .unwrap();

        let loaded = ProjectConfig::load(&path).unwrap();
        let replication = loaded.replication.unwrap();
        assert_eq!(
            replication.primary.as_deref(),
            Some("crab://primary/org/repo")
        );
        assert_eq!(replication.replicas.len(), 1);
        assert_eq!(replication.replicas[0].name, "west");
        assert!(!replication.replicas[0].backfill);
        assert!(!replication.replicas[0].read);
    }

    #[test]
    fn add_replica_records_backfill_requirement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".crab.toml");
        let mut args = add_args("s3://replica/org/repo");
        args.backfill = true;

        add_replica_to_project_config(
            &path,
            &args,
            ReplicationProviderKind::S3,
            ReplicationRpo::Standard,
        )
        .unwrap();

        let loaded = ProjectConfig::load(&path).unwrap();
        let replication = loaded.replication.unwrap();
        assert!(replication.replicas[0].backfill);
    }

    #[tokio::test]
    async fn backfill_cutover_blocks_unknown_batch_status() {
        let replica = backfill_replica();
        let status = control_plane_statuses(
            Some("crab://primary/org/repo"),
            Some(&crate::replication::ReplicationConfig {
                primary: Some("crab://primary/org/repo".into()),
                replicas: vec![replica.clone()],
                ..Default::default()
            }),
        )
        .await
        .into_iter()
        .next();

        let blocker = backfill_cutover_blocker(&replica, status.as_ref());

        assert!(
            blocker
                .as_deref()
                .is_some_and(|reason| reason.contains("backfill is unknown"))
        );
    }

    #[test]
    fn backfill_cutover_allows_replicas_without_backfill() {
        let replica = configured_replica(false);

        let blocker = backfill_cutover_blocker(&replica, None);

        assert!(blocker.is_none());
    }

    #[test]
    fn read_cutover_blocks_unready_replica() {
        let replica = configured_replica(false);
        let status = replica_status(false);

        let blocker = replica_read_cutover_blocker(&status, &replica, &[]);

        assert!(
            blocker
                .as_deref()
                .is_some_and(|reason| reason.contains("replica manifest is stale"))
        );
    }

    #[tokio::test]
    async fn read_cutover_blocks_unverified_backfill() {
        let replica = backfill_replica();
        let status = replica_status(true);
        let control_plane = control_plane_statuses(
            Some("crab://primary/org/repo"),
            Some(&crate::replication::ReplicationConfig {
                primary: Some("crab://primary/org/repo".into()),
                replicas: vec![replica.clone()],
                ..Default::default()
            }),
        )
        .await;

        let blocker = replica_read_cutover_blocker(&status, &replica, &control_plane);

        assert!(
            blocker
                .as_deref()
                .is_some_and(|reason| reason.contains("backfill is unknown"))
        );
    }

    #[test]
    fn read_cutover_allows_ready_replica_without_backfill() {
        let replica = configured_replica(false);
        let status = replica_status(true);

        let blocker = replica_read_cutover_blocker(&status, &replica, &[]);

        assert!(blocker.is_none());
    }

    #[test]
    fn remove_replica_deletes_matching_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(".crab.toml");
        add_replica_to_project_config(
            &path,
            &add_args("s3://replica/org/repo"),
            ReplicationProviderKind::S3,
            ReplicationRpo::Standard,
        )
        .unwrap();

        assert!(remove_replica_from_project_config(&path, "west").unwrap());
        let loaded = ProjectConfig::load(&path).unwrap();
        assert!(loaded.replication.unwrap().replicas.is_empty());
    }

    #[tokio::test]
    async fn add_replica_rejects_provider_url_mismatch() {
        let mut args = add_args("s3://replica/org/repo");
        args.provider = ProviderArg::Azure;
        args.dry_run = true;

        let err = run_add(&args).await.unwrap_err();
        assert!(
            err.to_string()
                .contains("provider azure does not match replica URL scheme")
        );
    }

    #[tokio::test]
    async fn add_apply_fails_closed_before_config_write() {
        let mut args = add_args("az://replica/org/repo");
        args.provider = ProviderArg::Azure;
        args.apply = true;

        let err = run_add(&args).await.unwrap_err();

        assert!(matches!(err, CrabError::Configuration { .. }));
        assert!(
            err.to_string()
                .contains("backend is not wired or configured")
        );
        assert!(err.to_string().contains("no cloud resources were changed"));
    }

    #[tokio::test]
    async fn coordinator_add_apply_fails_closed_without_backend() {
        let args = CoordinatorAddArgs {
            provider: CoordinatorProviderArg::Dynamodb,
            name: "crab-coordinator".into(),
            region: "us-east-1".into(),
            failover_regions: vec!["us-west-2".into()],
            dry_run: false,
            apply: true,
            json: false,
        };

        let err = run_coordinator_add_with_backends(&args, &NoCoordinatorBackends)
            .await
            .unwrap_err();

        assert!(err.to_string().contains("coordinator apply backend"));
    }

    #[tokio::test]
    async fn coordinator_add_apply_uses_verified_backend() {
        let args = CoordinatorAddArgs {
            provider: CoordinatorProviderArg::Dynamodb,
            name: "crab-coordinator".into(),
            region: "us-east-1".into(),
            failover_regions: vec!["us-west-2".into()],
            dry_run: false,
            apply: true,
            json: false,
        };
        let backend = VerifiedCoordinatorBackend::new(ManagedCoordinatorProvider::DynamoDb);
        let backends = SingleCoordinatorBackend { backend: &backend };

        run_coordinator_add_with_backends(&args, &backends)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn coordinator_status_uses_verified_backend_for_active_active() {
        let replication = active_active_replication();
        let backend = VerifiedCoordinatorBackend::new(ManagedCoordinatorProvider::DynamoDb);
        let backends = SingleCoordinatorBackend { backend: &backend };

        let status = coordinator_control_plane_status_with_backends(Some(&replication), &backends)
            .await
            .unwrap();

        assert!(status.backend_available);
        assert!(status.checked_drift);
        assert!(
            status
                .checks
                .iter()
                .all(|check| check.state == CoordinatorCheckState::Verified)
        );
    }

    #[tokio::test]
    async fn coordinator_remove_apply_uses_verified_backend() {
        let plan =
            dynamodb_coordinator_plan("crab-coordinator", "us-east-1", &["us-west-2".to_owned()]);
        let remove_plan = coordinator_control_plane_remove_plan(&plan);
        let backend = VerifiedCoordinatorBackend::new(ManagedCoordinatorProvider::DynamoDb);
        let backends = SingleCoordinatorBackend { backend: &backend };

        let status = remove_coordinator_plan_with_backends(&remove_plan, &backends)
            .await
            .unwrap();

        assert!(status.applied);
        assert!(
            status
                .actions
                .iter()
                .all(|action| action.starts_with("remove:"))
        );
    }

    #[test]
    fn parse_writer_spec_accepts_name_url_region() {
        let writer =
            parse_writer_spec("west=crab://bucket/repo,region=us-west-2,enabled=false").unwrap();

        assert_eq!(writer.name, "west");
        assert_eq!(writer.url, "crab://bucket/repo");
        assert_eq!(writer.region, "us-west-2");
        assert!(!writer.enabled);
    }

    #[test]
    fn parse_writer_spec_requires_region() {
        let err = parse_writer_spec("west=crab://bucket/repo").unwrap_err();

        assert!(err.to_string().contains("requires region"));
    }

    #[test]
    fn coordinator_repair_payload_preserves_materialization_actions() {
        let writer = active_active_replication().writers[0].clone();
        let plan = ActiveActiveRepairPlan {
            coordinator_epoch: 9,
            actions: vec![crate::replication::ActiveActiveRepairAction {
                operation_id: "op-1".into(),
                manifest_generation: 12,
                region: "us-east-1".into(),
                writer,
                source_region: "us-west-2".into(),
                refs: Vec::new(),
                uploaded_objects: vec!["xorbs/aa/object".into()],
            }],
        };

        let payload = repair_payload_from_coordinator_plan(true, Some(plan.clone()), None);

        assert_eq!(payload.coordinator_plan.as_ref(), Some(&plan));
        assert_eq!(payload.planned_actions.len(), 1);
        assert!(payload.planned_actions[0].contains("generation 12"));
        assert!(payload.blocked_reason.is_none());
    }

    #[test]
    fn coordinator_repair_payload_reports_missing_snapshot_adapter() {
        let payload = repair_payload_from_coordinator_plan(
            true,
            None,
            Some("managed coordinator repair snapshot adapter is not wired".into()),
        );

        assert!(payload.coordinator_plan.is_none());
        assert!(
            payload
                .blocked_reason
                .as_deref()
                .is_some_and(|reason| reason.contains("not wired"))
        );
        assert!(
            payload
                .planned_actions
                .iter()
                .any(|action| action.contains("per-region repair plan"))
        );
    }

    #[test]
    fn failover_planned_actions_describe_fence_and_resume() {
        let fence = failover_planned_actions(
            ActiveActiveFailoverOperation::Fence,
            "dynamodb://crab-coordinator",
            "org/repo",
        );
        let resume = failover_planned_actions(
            ActiveActiveFailoverOperation::Resume,
            "dynamodb://crab-coordinator",
            "org/repo",
        );

        assert!(
            fence
                .iter()
                .any(|action| action.contains("increment coordinator epoch"))
        );
        assert!(
            resume
                .iter()
                .any(|action| action.contains("mark writes healthy"))
        );
        assert!(
            resume
                .iter()
                .any(|action| action.contains("--repair-verified"))
        );
    }

    #[test]
    fn failover_resume_apply_requires_repair_verified_confirmation() {
        let err = validate_failover_apply_confirmation(
            ActiveActiveFailoverOperation::Resume,
            true,
            false,
        )
        .unwrap_err();

        assert!(err.to_string().contains("--repair-verified"));
        validate_failover_apply_confirmation(ActiveActiveFailoverOperation::Resume, true, true)
            .unwrap();
        validate_failover_apply_confirmation(ActiveActiveFailoverOperation::Resume, false, false)
            .unwrap();
        validate_failover_apply_confirmation(ActiveActiveFailoverOperation::Fence, true, false)
            .unwrap();
    }

    #[test]
    fn failover_plan_monitors_healthy_coordinator_without_external_signal() {
        let replication = active_active_replication_for_failover_plan();
        let snapshot = failover_snapshot(true, true, true);

        let decision = failover_automation_decision(&snapshot, Some(&replication), &[], false);

        assert_eq!(decision.action, FailoverAutomationAction::Monitor);
        assert!(!decision.automatic_apply_supported);
        assert!(decision.reason.contains("healthy"));
    }

    #[test]
    fn failover_plan_recommends_fence_for_verified_unhealthy_writer_signal() {
        let replication = active_active_replication_for_failover_plan();
        let snapshot = failover_snapshot(true, true, true);

        let decision =
            failover_automation_decision(&snapshot, Some(&replication), &["west".into()], false);

        assert_eq!(decision.action, FailoverAutomationAction::Fence);
        assert!(decision.automatic_apply_supported);
        assert!(decision.commands.iter().any(|command| {
            command.contains("failover fence") && command.contains("writer-unhealthy:west")
        }));
    }

    #[test]
    fn failover_plan_holds_for_unknown_unhealthy_writer_signal() {
        let replication = active_active_replication_for_failover_plan();
        let snapshot = failover_snapshot(true, true, true);

        let decision = failover_automation_decision(
            &snapshot,
            Some(&replication),
            &["disabled".into(), "missing".into()],
            false,
        );

        assert_eq!(decision.action, FailoverAutomationAction::Hold);
        assert!(!decision.automatic_apply_supported);
        assert!(decision.reason.contains("unknown or disabled"));
    }

    #[test]
    fn failover_plan_recommends_repair_when_coordinator_is_fenced() {
        let replication = active_active_replication_for_failover_plan();
        let snapshot = failover_snapshot(false, true, false);

        let decision = failover_automation_decision(&snapshot, Some(&replication), &[], false);

        assert_eq!(decision.action, FailoverAutomationAction::Repair);
        assert!(decision.automatic_apply_supported);
        assert!(decision.commands.iter().any(|command| {
            command.contains("repair --from-coordinator") && command.contains("--dry-run")
        }));
    }

    #[test]
    fn failover_plan_recommends_resume_after_repair_proof() {
        let replication = active_active_replication_for_failover_plan();
        let snapshot = failover_snapshot(false, true, false);

        let decision = failover_automation_decision(&snapshot, Some(&replication), &[], true);

        assert_eq!(decision.action, FailoverAutomationAction::Resume);
        assert!(decision.automatic_apply_supported);
        assert!(decision.repair_verified);
        assert!(decision.commands.iter().any(|command| {
            command.contains("failover resume") && command.contains("--repair-verified")
        }));
    }

    #[test]
    fn failover_plan_holds_without_linearizable_health() {
        let replication = active_active_replication_for_failover_plan();
        let snapshot = failover_snapshot(true, false, true);

        let decision =
            failover_automation_decision(&snapshot, Some(&replication), &["west".into()], false);

        assert_eq!(decision.action, FailoverAutomationAction::Hold);
        assert!(!decision.automatic_apply_supported);
        assert!(decision.reason.contains("linearizable"));
    }

    #[test]
    fn failover_run_apply_blocks_hold_and_monitor_decisions() {
        let replication = active_active_replication_for_failover_plan();
        let monitor_snapshot = failover_snapshot(true, true, true);
        let monitor =
            failover_automation_decision(&monitor_snapshot, Some(&replication), &[], false);
        let hold_snapshot = failover_snapshot(true, false, true);
        let hold = failover_automation_decision(
            &hold_snapshot,
            Some(&replication),
            &["west".into()],
            false,
        );

        assert!(failover_automation_apply_blocker(&monitor, true).is_some());
        assert!(failover_automation_apply_blocker(&hold, true).is_some());
        assert!(failover_automation_apply_blocker(&monitor, false).is_none());
    }

    #[test]
    fn failover_run_apply_allows_actionable_decisions() {
        let replication = active_active_replication_for_failover_plan();
        let healthy = failover_snapshot(true, true, true);
        let fenced = failover_snapshot(false, true, false);
        let fence =
            failover_automation_decision(&healthy, Some(&replication), &["west".into()], false);
        let repair = failover_automation_decision(&fenced, Some(&replication), &[], false);
        let resume = failover_automation_decision(&fenced, Some(&replication), &[], true);

        assert!(failover_automation_apply_blocker(&fence, true).is_none());
        assert!(failover_automation_apply_blocker(&repair, true).is_none());
        assert!(failover_automation_apply_blocker(&resume, true).is_none());
    }

    #[test]
    fn failover_policy_disables_automatic_write_failover() {
        let policy = failover_automation_policy();

        assert!(!policy.automatic_write_failover_supported);
        assert_eq!(policy.orchestration, "manual-fence-repair-resume");
        assert_eq!(policy.split_brain_policy, "fail-closed");
        assert!(
            policy
                .required_operator_steps
                .iter()
                .any(|step| step.contains("fence writes"))
        );
        assert_eq!(
            policy.adr,
            "crab/docs/design/replica-active-active-failover.md"
        );
    }

    fn active_active_replication_for_failover_plan() -> ReplicationConfig {
        let mut replication = active_active_replication();
        replication.writers.push(WriterConfig {
            name: "west".into(),
            url: "s3://replica/org/repo".into(),
            region: "us-west-2".into(),
            enabled: true,
        });
        replication.writers.push(WriterConfig {
            name: "disabled".into(),
            url: "s3://disabled/org/repo".into(),
            region: "us-west-1".into(),
            enabled: false,
        });
        replication
    }

    fn failover_snapshot(
        healthy: bool,
        linearizable: bool,
        writes_enabled: bool,
    ) -> FailoverStatusSnapshot {
        FailoverStatusSnapshot {
            active_active: ActiveActiveStatus {
                mode: ReplicationMode::ActiveActive,
                coordinator_configured: true,
                coordinator_ready: writes_enabled,
                writes_enabled,
                enabled_writers: 2,
                reason: if writes_enabled {
                    None
                } else {
                    Some("coordinator data-plane health does not admit active-active writes".into())
                },
            },
            replication: Some(active_active_replication_for_failover_plan()),
            coordinator: None,
            coordinator_health: Some(CoordinatorHealth {
                healthy,
                epoch: 7,
                linearizable,
                reason: None,
                state_summary: None,
            }),
        }
    }
}
