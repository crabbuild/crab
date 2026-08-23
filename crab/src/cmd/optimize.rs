//! Operator workflow for repository cost optimization.

pub mod xorbs;

use std::process::Stdio;

use clap::Parser;
use schemars::JsonSchema;
use serde::Serialize;
use tokio::io::AsyncRead;

use crate::core::config::Config;
use crate::core::error::{CrabError, Result};
use crate::core::output::{OutputMode, emit_json};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

pub const OPTIMIZE_PLAN_SCHEMA: &str = "optimize.plan";
pub const OPTIMIZE_APPLY_SCHEMA: &str = "optimize.apply";
pub const OPTIMIZE_SCHEMA_VERSION: &str = "1.0";

/// Arguments for `crab optimize plan`.
#[derive(Debug, Clone, Parser)]
pub struct OptimizePlanArgs {
    /// Structured JSON output.
    #[arg(long)]
    pub json: bool,
    /// Path to a pricing override YAML file for the cost phase.
    #[arg(long, value_name = "PATH")]
    pub pricing_file: Option<String>,
    /// Inventory source for the cost phase: auto, live, or report.
    #[arg(long, value_name = "SOURCE")]
    pub inventory_source: Option<String>,
    /// Sample ratio for live inventory (0.0-1.0).
    #[arg(long, value_name = "RATIO")]
    pub sample: Option<f64>,
    /// Number of heaviest cold objects to report.
    #[arg(long, value_name = "K")]
    pub top_k: Option<usize>,
    /// Xorb rewrite profile to include when --include-xorbs is set.
    #[arg(long)]
    pub profile: Option<String>,
    /// Include the xorb rewrite step in the apply plan.
    #[arg(long)]
    pub include_xorbs: bool,
    /// Omit lifecycle tiering from the workflow.
    #[arg(long)]
    pub skip_tiers: bool,
    /// Omit local cache pruning from the workflow.
    #[arg(long)]
    pub skip_cache: bool,
    /// Omit replica policy checks from the workflow.
    #[arg(long)]
    pub skip_replicas: bool,
}

/// Arguments for `crab optimize apply`.
#[derive(Debug, Clone, Parser)]
pub struct OptimizeApplyArgs {
    /// Structured JSON output.
    #[arg(long)]
    pub json: bool,
    /// Path to a pricing override YAML file for the cost phase.
    #[arg(long, value_name = "PATH")]
    pub pricing_file: Option<String>,
    /// Inventory source for the cost phase: auto, live, or report.
    #[arg(long, value_name = "SOURCE")]
    pub inventory_source: Option<String>,
    /// Sample ratio for live inventory (0.0-1.0).
    #[arg(long, value_name = "RATIO")]
    pub sample: Option<f64>,
    /// Number of heaviest cold objects to report.
    #[arg(long, value_name = "K")]
    pub top_k: Option<usize>,
    /// Xorb rewrite profile to execute when --include-xorbs is set.
    #[arg(long)]
    pub profile: Option<String>,
    /// Run the xorb rewrite step. Omitted by default because it can be long-running.
    #[arg(long)]
    pub include_xorbs: bool,
    /// Omit lifecycle tiering from the workflow.
    #[arg(long)]
    pub skip_tiers: bool,
    /// Omit local cache pruning from the workflow.
    #[arg(long)]
    pub skip_cache: bool,
    /// Omit replica policy checks from the workflow.
    #[arg(long)]
    pub skip_replicas: bool,
}

impl From<&OptimizeApplyArgs> for OptimizePlanArgs {
    fn from(args: &OptimizeApplyArgs) -> Self {
        Self {
            json: args.json,
            pricing_file: args.pricing_file.clone(),
            inventory_source: args.inventory_source.clone(),
            sample: args.sample,
            top_k: args.top_k,
            profile: args.profile.clone(),
            include_xorbs: args.include_xorbs,
            skip_tiers: args.skip_tiers,
            skip_cache: args.skip_cache,
            skip_replicas: args.skip_replicas,
        }
    }
}

/// Workflow mode represented by an optimizer payload.
#[derive(Debug, Clone, Copy, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OptimizeWorkflowMode {
    Plan,
    Apply,
}

/// Step kind in the combined optimizer workflow.
#[derive(Debug, Clone, Copy, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OptimizeStepKind {
    CostReport,
    SafetyChecks,
    LifecycleTiering,
    OptimizeXorbs,
    CachePrune,
    ReplicaPolicy,
}

/// Step execution or planning status.
#[derive(Debug, Clone, Copy, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OptimizeStepStatus {
    Planned,
    Skipped,
    Running,
    Succeeded,
    Failed,
}

/// One step in the combined optimizer workflow.
#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct OptimizeStep {
    pub id: String,
    pub kind: OptimizeStepKind,
    pub title: String,
    pub command: String,
    pub mutates: bool,
    pub status: OptimizeStepStatus,
    pub detail: String,
}

/// Counts for a plan or apply payload.
#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct OptimizeSummary {
    pub planned: u32,
    pub skipped: u32,
    pub running: u32,
    pub succeeded: u32,
    pub failed: u32,
    pub mutating_steps: u32,
}

/// Payload emitted by `crab optimize plan` and `crab optimize apply --json`.
#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct OptimizePayload {
    pub mode: OptimizeWorkflowMode,
    pub generated_at: String,
    pub steps: Vec<OptimizeStep>,
    pub summary: OptimizeSummary,
    pub assumptions: Vec<String>,
}

/// Build and render `crab optimize plan`.
pub fn run_plan(args: &OptimizePlanArgs, config: &Config, mode: OutputMode) -> OptimizePayload {
    let payload = build_payload(args, config, OptimizeWorkflowMode::Plan);
    render_payload(&payload, mode, OPTIMIZE_PLAN_SCHEMA);
    payload
}

/// Build an apply payload with planned steps.
#[must_use]
pub fn build_apply_payload(args: &OptimizeApplyArgs, config: &Config) -> OptimizePayload {
    build_payload(
        &OptimizePlanArgs::from(args),
        config,
        OptimizeWorkflowMode::Apply,
    )
}

/// Render the final apply payload.
pub fn render_apply(payload: &OptimizePayload, mode: OutputMode) {
    render_payload(payload, mode, OPTIMIZE_APPLY_SCHEMA);
}

/// Execute a child `crab` command as one optimizer step.
pub async fn run_child_step(
    step: &mut OptimizeStep,
    mode: OutputMode,
    args: &[String],
    cancel: &CancellationToken,
) -> Result<()> {
    if step.status == OptimizeStepStatus::Skipped {
        return Ok(());
    }

    step.status = OptimizeStepStatus::Running;
    let bin = crate::cmd::init::crab_binary_path();
    let mut command = Command::new(&bin);
    command.args(args);
    if mode.is_machine() {
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(CrabError::Io)?;
        let stdout_task = tokio::spawn(read_child_pipe(child.stdout.take()));
        let stderr_task = tokio::spawn(read_child_pipe(child.stderr.take()));
        let status = tokio::select! {
            result = child.wait() => result.map_err(CrabError::Io)?,
            () = cancel.cancelled() => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                stdout_task.abort();
                stderr_task.abort();
                step.status = OptimizeStepStatus::Failed;
                "cancelled".clone_into(&mut step.detail);
                return Err(CrabError::Cancelled);
            }
        };
        let stdout = stdout_task.await.map_err(|error| {
            CrabError::Internal(format!("stdout capture task failed: {error}"))
        })??;
        let stderr = stderr_task.await.map_err(|error| {
            CrabError::Internal(format!("stderr capture task failed: {error}"))
        })??;
        let diagnostic = if status.success() {
            None
        } else {
            bounded_child_output(&stderr).or_else(|| bounded_child_output(&stdout))
        };
        return finish_child_step(step, status, diagnostic.as_deref());
    }

    let mut child = command
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(CrabError::Io)?;
    let status = tokio::select! {
        result = child.wait() => result.map_err(CrabError::Io)?,
        () = cancel.cancelled() => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            step.status = OptimizeStepStatus::Failed;
            "cancelled".clone_into(&mut step.detail);
            return Err(CrabError::Cancelled);
        }
    };
    finish_child_step(step, status, None)
}

async fn read_child_pipe<R>(reader: Option<R>) -> std::io::Result<Vec<u8>>
where
    R: AsyncRead + Unpin,
{
    let Some(mut reader) = reader else {
        return Ok(Vec::new());
    };
    let mut output = Vec::new();
    tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut output).await?;
    Ok(output)
}

fn finish_child_step(
    step: &mut OptimizeStep,
    status: std::process::ExitStatus,
    diagnostic: Option<&str>,
) -> Result<()> {
    if status.success() {
        step.status = OptimizeStepStatus::Succeeded;
        "completed".clone_into(&mut step.detail);
        return Ok(());
    }

    step.status = OptimizeStepStatus::Failed;
    let code = status
        .code()
        .map_or_else(|| "signal".to_owned(), |code| code.to_string());
    step.detail = match diagnostic {
        Some(diagnostic) => format!("failed with exit status {code}: {diagnostic}"),
        None => format!("failed with exit status {code}"),
    };
    Err(CrabError::Configuration {
        key: format!("{} failed", step.id),
        origin: step.detail.clone(),
    })
}

fn bounded_child_output(bytes: &[u8]) -> Option<String> {
    const MAX_DIAGNOSTIC_BYTES: usize = 2048;
    let output = String::from_utf8_lossy(bytes);
    let output = output.trim();
    if output.is_empty() {
        return None;
    }
    Some(output.chars().take(MAX_DIAGNOSTIC_BYTES).collect())
}

/// Mark a planned in-process safety step as succeeded.
pub fn mark_succeeded(step: &mut OptimizeStep, detail: impl Into<String>) {
    if step.status == OptimizeStepStatus::Skipped {
        return;
    }
    step.status = OptimizeStepStatus::Succeeded;
    step.detail = detail.into();
}

/// Recompute the summary after apply changes step statuses.
pub fn refresh_summary(payload: &mut OptimizePayload) {
    payload.summary = summarize(&payload.steps);
}

/// Arguments for the cost-report child command.
#[must_use]
pub fn cost_command_args(args: &OptimizeApplyArgs) -> Vec<String> {
    let mut cmd = vec!["doctor".to_owned(), "--cost".to_owned()];
    append_cost_flags(
        &mut cmd,
        args.pricing_file.as_deref(),
        args.inventory_source.as_deref(),
        args.sample,
        args.top_k,
    );
    cmd
}

/// Arguments for the lifecycle-tiering child command.
#[must_use]
pub fn tier_apply_command_args() -> Vec<String> {
    vec![
        "tier".to_owned(),
        "plan".to_owned(),
        "--apply".to_owned(),
        "--merge".to_owned(),
    ]
}

/// Arguments for the xorb optimization child command.
#[must_use]
pub fn xorb_apply_command_args(args: &OptimizeApplyArgs) -> Vec<String> {
    let mut cmd = vec![
        "optimize".to_owned(),
        "xorbs".to_owned(),
        "--apply".to_owned(),
    ];
    if let Some(profile) = args.profile.as_deref() {
        cmd.push("--profile".to_owned());
        cmd.push(profile.to_owned());
    }
    cmd
}

/// Arguments for the local-cache-prune child command.
#[must_use]
pub fn cache_prune_command_args() -> Vec<String> {
    vec!["prune".to_owned()]
}

/// Arguments for the replica-policy child command.
#[must_use]
pub fn replica_policy_command_args() -> Vec<String> {
    vec![
        "replica".to_owned(),
        "doctor".to_owned(),
        "--fix-plan".to_owned(),
    ]
}

fn build_payload(
    args: &OptimizePlanArgs,
    config: &Config,
    mode: OptimizeWorkflowMode,
) -> OptimizePayload {
    let mut steps = vec![
        cost_step(args),
        safety_step(config),
        tier_step(args, config),
        xorb_step(args),
        cache_step(args),
        replica_step(args, config),
    ];
    if mode == OptimizeWorkflowMode::Apply {
        for step in &mut steps {
            if step.status == OptimizeStepStatus::Planned {
                step.detail = format!("ready to apply: {}", step.detail);
            }
        }
    }
    let generated_at = crate::cost::inventory::live::chrono_now_rfc3339();
    let summary = summarize(&steps);
    OptimizePayload {
        mode,
        generated_at,
        steps,
        summary,
        assumptions: vec![
            "Cost analysis uses the same inputs as `crab doctor --cost`.".to_owned(),
            "Lifecycle, xorb rewrite, and replica commands retain their own provider safety checks."
                .to_owned(),
            "Xorb rewrite is opt-in with --include-xorbs because it can restore archived data and rewrite remote xorbs."
                .to_owned(),
        ],
    }
}

fn cost_step(args: &OptimizePlanArgs) -> OptimizeStep {
    let mut cmd = vec!["crab".to_owned(), "doctor".to_owned(), "--cost".to_owned()];
    append_cost_flags(
        &mut cmd,
        args.pricing_file.as_deref(),
        args.inventory_source.as_deref(),
        args.sample,
        args.top_k,
    );
    OptimizeStep {
        id: "cost-report".to_owned(),
        kind: OptimizeStepKind::CostReport,
        title: "Cost report".to_owned(),
        command: shell_command(&cmd),
        mutates: false,
        status: OptimizeStepStatus::Planned,
        detail: "collect bucket inventory, pricing inputs, and cost recommendations".to_owned(),
    }
}

fn safety_step(config: &Config) -> OptimizeStep {
    let detail = if config.remote_url.is_some() {
        "remote configuration present; active-active maintenance admission will run before mutations"
    } else {
        "no remote URL configured; remote mutations may be skipped or fail in child commands"
    };
    OptimizeStep {
        id: "safety-checks".to_owned(),
        kind: OptimizeStepKind::SafetyChecks,
        title: "Safety checks".to_owned(),
        command: "crab doctor".to_owned(),
        mutates: false,
        status: OptimizeStepStatus::Planned,
        detail: detail.to_owned(),
    }
}

fn tier_step(args: &OptimizePlanArgs, config: &Config) -> OptimizeStep {
    if args.skip_tiers {
        return skipped_step(
            "lifecycle-tiering",
            OptimizeStepKind::LifecycleTiering,
            "Lifecycle tiering",
            "crab tier plan --apply --merge",
            true,
            "disabled by --skip-tiers",
        );
    }
    if !config.tier.enabled {
        return skipped_step(
            "lifecycle-tiering",
            OptimizeStepKind::LifecycleTiering,
            "Lifecycle tiering",
            "crab tier plan --apply --merge",
            true,
            "tier.enabled is false; set [tier] enabled = true before applying lifecycle rules",
        );
    }
    OptimizeStep {
        id: "lifecycle-tiering".to_owned(),
        kind: OptimizeStepKind::LifecycleTiering,
        title: "Lifecycle tiering".to_owned(),
        command: "crab tier plan --apply --merge".to_owned(),
        mutates: true,
        status: OptimizeStepStatus::Planned,
        detail: format!(
            "apply Crab-managed lifecycle rules: IA after {}d, deep after {}d",
            config.tier.to_ia_days, config.tier.to_deep_days
        ),
    }
}

fn xorb_step(args: &OptimizePlanArgs) -> OptimizeStep {
    let mut command = "crab optimize xorbs --apply".to_owned();
    if let Some(profile) = args.profile.as_deref() {
        command.push_str(" --profile ");
        command.push_str(profile);
    }
    if !args.include_xorbs {
        return skipped_step(
            "optimize-xorbs",
            OptimizeStepKind::OptimizeXorbs,
            "Xorb rewrite",
            &command,
            true,
            "disabled by default; pass --include-xorbs to include the remote xorb rewrite",
        );
    }
    OptimizeStep {
        id: "optimize-xorbs".to_owned(),
        kind: OptimizeStepKind::OptimizeXorbs,
        title: "Xorb rewrite".to_owned(),
        command,
        mutates: true,
        status: OptimizeStepStatus::Planned,
        detail: "rewrite source xorbs with the selected profile, then reconcile file indexes and shard manifests".to_owned(),
    }
}

fn cache_step(args: &OptimizePlanArgs) -> OptimizeStep {
    if args.skip_cache {
        return skipped_step(
            "cache-prune",
            OptimizeStepKind::CachePrune,
            "Local cache prune",
            "crab prune",
            true,
            "disabled by --skip-cache",
        );
    }
    OptimizeStep {
        id: "cache-prune".to_owned(),
        kind: OptimizeStepKind::CachePrune,
        title: "Local cache prune".to_owned(),
        command: "crab prune".to_owned(),
        mutates: true,
        status: OptimizeStepStatus::Planned,
        detail: "evict local cache objects until configured budgets are satisfied".to_owned(),
    }
}

fn replica_step(args: &OptimizePlanArgs, config: &Config) -> OptimizeStep {
    if args.skip_replicas {
        return skipped_step(
            "replica-policy",
            OptimizeStepKind::ReplicaPolicy,
            "Replica policy",
            "crab replica doctor --fix-plan",
            false,
            "disabled by --skip-replicas",
        );
    }
    let Some(replication) = config.replication.as_ref() else {
        return skipped_step(
            "replica-policy",
            OptimizeStepKind::ReplicaPolicy,
            "Replica policy",
            "crab replica doctor --fix-plan",
            false,
            "no replication config is present",
        );
    };
    OptimizeStep {
        id: "replica-policy".to_owned(),
        kind: OptimizeStepKind::ReplicaPolicy,
        title: "Replica policy".to_owned(),
        command: "crab replica doctor --fix-plan".to_owned(),
        mutates: false,
        status: OptimizeStepStatus::Planned,
        detail: format!(
            "check {} configured replica(s) in {} mode",
            replication.replicas.len(),
            replication.mode
        ),
    }
}

fn skipped_step(
    id: &str,
    kind: OptimizeStepKind,
    title: &str,
    command: &str,
    mutates: bool,
    detail: &str,
) -> OptimizeStep {
    OptimizeStep {
        id: id.to_owned(),
        kind,
        title: title.to_owned(),
        command: command.to_owned(),
        mutates,
        status: OptimizeStepStatus::Skipped,
        detail: detail.to_owned(),
    }
}

fn append_cost_flags(
    cmd: &mut Vec<String>,
    pricing_file: Option<&str>,
    inventory_source: Option<&str>,
    sample: Option<f64>,
    top_k: Option<usize>,
) {
    if let Some(path) = pricing_file {
        cmd.push("--pricing-file".to_owned());
        cmd.push(path.to_owned());
    }
    if let Some(source) = inventory_source {
        cmd.push("--inventory-source".to_owned());
        cmd.push(source.to_owned());
    }
    if let Some(sample) = sample {
        cmd.push("--sample".to_owned());
        cmd.push(sample.to_string());
    }
    if let Some(top_k) = top_k {
        cmd.push("--top-k".to_owned());
        cmd.push(top_k.to_string());
    }
}

fn summarize(steps: &[OptimizeStep]) -> OptimizeSummary {
    let mut summary = OptimizeSummary {
        planned: 0,
        skipped: 0,
        running: 0,
        succeeded: 0,
        failed: 0,
        mutating_steps: 0,
    };
    for step in steps {
        if step.mutates && step.status != OptimizeStepStatus::Skipped {
            summary.mutating_steps += 1;
        }
        match step.status {
            OptimizeStepStatus::Planned => summary.planned += 1,
            OptimizeStepStatus::Skipped => summary.skipped += 1,
            OptimizeStepStatus::Running => summary.running += 1,
            OptimizeStepStatus::Succeeded => summary.succeeded += 1,
            OptimizeStepStatus::Failed => summary.failed += 1,
        }
    }
    summary
}

fn render_payload(payload: &OptimizePayload, mode: OutputMode, schema: &'static str) {
    if mode == OutputMode::Json {
        emit_json(schema, OPTIMIZE_SCHEMA_VERSION, payload);
        return;
    }

    let heading = match payload.mode {
        OptimizeWorkflowMode::Plan => "Cost optimization plan",
        OptimizeWorkflowMode::Apply => "Cost optimization apply",
    };
    println!("{heading}");
    println!("Generated: {}", payload.generated_at);
    println!();
    for step in &payload.steps {
        println!(
            "  [{:<9}] {:<18} {}",
            status_label(step.status),
            step.title,
            step.detail
        );
        println!("             command: {}", step.command);
    }
    println!();
    println!(
        "Summary: planned={}, skipped={}, succeeded={}, failed={}, mutating_steps={}",
        payload.summary.planned,
        payload.summary.skipped,
        payload.summary.succeeded,
        payload.summary.failed,
        payload.summary.mutating_steps
    );
}

fn status_label(status: OptimizeStepStatus) -> &'static str {
    match status {
        OptimizeStepStatus::Planned => "planned",
        OptimizeStepStatus::Skipped => "skipped",
        OptimizeStepStatus::Running => "running",
        OptimizeStepStatus::Succeeded => "ok",
        OptimizeStepStatus::Failed => "failed",
    }
}

fn shell_command(parts: &[String]) -> String {
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_skips_remote_mutations_until_enabled_or_requested() {
        let config = Config::default();
        let args = OptimizePlanArgs {
            json: false,
            pricing_file: None,
            inventory_source: None,
            sample: None,
            top_k: None,
            profile: None,
            include_xorbs: false,
            skip_tiers: false,
            skip_cache: false,
            skip_replicas: false,
        };

        let payload = build_payload(&args, &config, OptimizeWorkflowMode::Plan);

        assert_eq!(payload.summary.planned, 3);
        assert_eq!(payload.summary.skipped, 3);
        assert_eq!(payload.summary.mutating_steps, 1);
    }

    #[test]
    fn xorb_step_is_included_only_when_operator_opts_in() {
        let config = Config {
            tier: crate::core::config::TierConfig {
                enabled: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let args = OptimizePlanArgs {
            json: false,
            pricing_file: None,
            inventory_source: None,
            sample: None,
            top_k: None,
            profile: Some("ml".to_owned()),
            include_xorbs: true,
            skip_tiers: false,
            skip_cache: false,
            skip_replicas: true,
        };

        let payload = build_payload(&args, &config, OptimizeWorkflowMode::Apply);
        let xorb = payload
            .steps
            .iter()
            .find(|step| step.id == "optimize-xorbs")
            .expect("xorb step");

        assert_eq!(xorb.status, OptimizeStepStatus::Planned);
        assert_eq!(xorb.command, "crab optimize xorbs --apply --profile ml");
        assert_eq!(payload.summary.mutating_steps, 3);
    }
}
