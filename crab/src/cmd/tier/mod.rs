//! CLI entry point for `crab tier` — lifecycle rule generation,
//! application, and rollback.
//!
//! Subcommands:
//! - `plan` — generate lifecycle rules for the bucket.
//! - `rollback <path>` — restore a prior lifecycle configuration from
//!   a backup file.

use serde::Serialize;
use tracing::{debug, info};

use crate::core::context::AppContext;
use crate::core::error::{CrabError, Result};
use crate::core::output::event_payloads::{RestoreCompletePayload, RestoreSubmitPayload};
use crate::core::output::{JsonlStream, OutputMode, emit_json};
use crate::tier::apply::{self, ApplyOpts};
use crate::tier::classes::StorageClass;
use crate::tier::plan;

/// Subcommands for `crab tier`.
#[derive(Debug, clap::Subcommand)]
pub enum TierCommand {
    /// Generate lifecycle rules for the bucket.
    Plan {
        /// Apply the generated rules to the bucket.
        #[arg(long)]
        apply: bool,
        /// Merge with existing non-crab rules instead of failing on conflict.
        #[arg(long)]
        merge: bool,
        /// Show what would be done without making changes.
        #[arg(long)]
        dry_run: bool,
        /// Output format: xml, json, or yaml.
        #[arg(long, default_value = "xml")]
        output: String,
        /// Structured JSON output (single envelope with terminal result).
        #[arg(long, conflicts_with = "jsonl")]
        json: bool,
        /// Streaming JSONL output (one event per line).
        #[arg(long, conflicts_with = "json")]
        jsonl: bool,
    },
    /// Rollback to a prior lifecycle configuration from a backup file.
    Rollback {
        /// Path to the backup file.
        path: String,
    },
}

// ── Structured output payloads ──────────────────────────────────────

/// Structured payload for `crab tier plan --json`.
///
/// Wraps the plan output with provider, rules, and versioning status
/// for the `"tier.plan"` schema envelope.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct TierPlanPayload {
    /// Cloud provider hosting the bucket.
    pub provider: String,
    /// Lifecycle rules in the plan.
    pub rules: Vec<TierRulePayload>,
    /// Whether bucket versioning is enabled.
    pub versioning_enabled: bool,
    /// Whether object-lock is enabled on the bucket.
    pub object_lock_enabled: bool,
}

/// One lifecycle rule within the structured plan output.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct TierRulePayload {
    /// Rule identifier (always prefixed `crab-`).
    pub id: String,
    /// Object key prefix this rule applies to.
    pub prefix: String,
    /// Transitions in this rule.
    pub transitions: Vec<TierTransitionPayload>,
    /// Days after which noncurrent versions expire (versioned buckets).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub noncurrent_expiration_days: Option<u32>,
    /// Minimum object size in bytes for the filter.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_object_size_bytes: Option<u64>,
}

/// A storage-class transition within a lifecycle rule.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct TierTransitionPayload {
    /// Days after object creation before the transition fires.
    pub days: u32,
    /// Target storage class.
    pub to_class: StorageClass,
}

impl TierPlanPayload {
    /// Build a payload from a `TierPlan`.
    fn from_plan(plan: &crate::tier::provider::TierPlan) -> Self {
        Self {
            provider: format!("{:?}", plan.provider),
            rules: plan
                .rules
                .iter()
                .map(|r| TierRulePayload {
                    id: r.id.clone(),
                    prefix: r.prefix.clone(),
                    transitions: r
                        .transitions
                        .iter()
                        .map(|t| TierTransitionPayload {
                            days: t.days,
                            to_class: t.to_class,
                        })
                        .collect(),
                    noncurrent_expiration_days: r.noncurrent_expiration_days,
                    min_object_size_bytes: r.min_object_size_bytes,
                })
                .collect(),
            versioning_enabled: plan.versioning_enabled,
            object_lock_enabled: plan.object_lock_enabled,
        }
    }
}

/// Payload variants emitted under the `tier.event` schema.
#[derive(Debug, Serialize, schemars::JsonSchema)]
#[serde(untagged)]
pub enum TierEventPayload {
    Plan(TierPlanPayload),
    RestoreSubmit(RestoreSubmitPayload),
    RestoreComplete(RestoreCompletePayload),
}

/// Resolve the output mode from the `Plan` subcommand flags.
pub fn plan_output_mode(json: bool, jsonl: bool) -> OutputMode {
    OutputMode::from_flags(json, jsonl)
}

/// Execute a tier subcommand.
///
/// For `Plan`: builds a provider-backed `BucketProbe`, calls
/// `plan::build()`, renders via the provider, and optionally applies.
///
/// For `Rollback`: calls `apply::rollback()` to restore a prior
/// lifecycle configuration from a backup file.
pub async fn run_tier(cmd: TierCommand, ctx: &AppContext, mode: OutputMode) -> Result<()> {
    #[cfg(feature = "otlp")]
    let _span = tracing::info_span!(
        "tier.plan",
        command = "tier",
        bucket_url = tracing::field::Empty,
        dry_run = tracing::field::Empty,
    )
    .entered();

    match cmd {
        TierCommand::Plan {
            apply: should_apply,
            merge,
            dry_run,
            output: _output,
            ..
        } => {
            let tier_cfg = &ctx.config().tier;
            debug!(apply = should_apply, merge, dry_run, "generating tier plan");

            if should_apply && !dry_run {
                crate::replication::ensure_active_active_maintenance_admitted(
                    ctx.config(),
                    "lifecycle tier apply",
                )?;
            }

            if should_apply && !tier_cfg.enabled {
                return Err(CrabError::Configuration {
                    key: "tier.enabled is false; refusing to apply lifecycle rules".into(),
                    origin: "tier".into(),
                });
            }

            let crab_url = crate::tier::runtime::current_crab_url()?;
            let lifecycle_provider =
                crate::tier::runtime::build_lifecycle_provider(ctx.config(), &crab_url).await?;
            let probe = crate::tier::runtime::probe_bucket(
                ctx.config(),
                &crab_url,
                lifecycle_provider.as_ref(),
            )
            .await?;

            let tier_plan = plan::build(tier_cfg, &probe)?;
            info!(
                rules = tier_plan.rules.len(),
                provider = ?tier_plan.provider,
                "tier plan generated"
            );

            match mode {
                OutputMode::Json => {
                    let payload = TierPlanPayload::from_plan(&tier_plan);
                    emit_json("tier.plan", "1.0", &payload);
                }
                OutputMode::Jsonl => {
                    let mut stream = JsonlStream::new("tier.event", "1.0", std::io::stdout());
                    let payload = TierPlanPayload::from_plan(&tier_plan);
                    stream.emit_result(&payload);
                }
                OutputMode::Text => {
                    println!("Tier plan for {:?}:", tier_plan.provider);
                    for rule in &tier_plan.rules {
                        println!(
                            "  Rule {}: prefix={}, transitions={}",
                            rule.id,
                            rule.prefix,
                            rule.transitions.len()
                        );
                        for t in &rule.transitions {
                            println!("    → {:?} after {} days", t.to_class, t.days);
                        }
                        if let Some(min) = rule.min_object_size_bytes {
                            println!("    min object size: {min} bytes");
                        }
                        if let Some(nc) = rule.noncurrent_expiration_days {
                            println!("    noncurrent expiration: {nc} days");
                        }
                    }
                }
            }

            if should_apply {
                let outcome = apply::apply(
                    lifecycle_provider.as_ref(),
                    &tier_plan,
                    ApplyOpts { merge, dry_run },
                )
                .await?;

                if matches!(mode, OutputMode::Text) {
                    if dry_run {
                        println!("\n(dry-run: no lifecycle changes applied)");
                    } else {
                        println!(
                            "\nApplied lifecycle rules at {} (backup: {})",
                            outcome.applied_at,
                            outcome.backup_path.as_deref().unwrap_or("none")
                        );
                    }
                }
            }

            Ok(())
        }
        TierCommand::Rollback { path } => {
            info!(backup = %path, "rolling back lifecycle configuration");
            crate::replication::ensure_active_active_maintenance_admitted(
                ctx.config(),
                "lifecycle tier rollback",
            )?;

            let crab_url = crate::tier::runtime::current_crab_url()?;
            let lifecycle_provider =
                crate::tier::runtime::build_lifecycle_provider(ctx.config(), &crab_url).await?;
            apply::rollback(lifecycle_provider.as_ref(), &path).await?;
            if matches!(mode, OutputMode::Text) {
                println!("Rolled back lifecycle configuration from backup: {path}");
            }

            Ok(())
        }
    }
}
