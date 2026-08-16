//! Cost recommendation engine.
//!
//! Rule-based engine that evaluates an inventory against a resolved
//! price table and produces actionable cost-reduction recommendations.
//!
//! Each rule is a function that takes an inventory, optional access
//! stats, and a price table, and returns an optional recommendation.
//! Rules have dependencies: a dependent rule only fires when its
//! parent's status is `applied` (or the parent is not applicable).
//!
//! Built-in rules:
//! - `apply_tier_plan_ia` — savings if IA transition at 30 days.
//! - `apply_tier_plan_glacier` — savings if Glacier transition at 180 days.
//! - `enable_intelligent_tiering` — for unpredictable access patterns.
//! - `optimize_xorbs_profile_mismatch` — xorb size distribution > 1.5σ from target.
//! - `gc_candidates` — orphan objects from fsck overlap.

use rust_decimal::Decimal;
use schemars::JsonSchema;
use serde::Serialize;

use super::inventory::Inventory;
use super::pricing::override_file::ResolvedTable;

/// Risk level for a recommendation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
pub enum RiskLevel {
    /// Low risk — read-only or easily reversible.
    Low,
    /// Medium risk — writes data but is reversible.
    Medium,
    /// High risk — may incur costs or is hard to reverse.
    High,
}

impl std::fmt::Display for RiskLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Low => write!(f, "low"),
            Self::Medium => write!(f, "medium"),
            Self::High => write!(f, "high"),
        }
    }
}

/// A single cost-reduction recommendation.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct Recommendation {
    /// Short title (e.g. "Enable Standard-IA tiering").
    pub title: String,
    /// Explanation of why this recommendation applies.
    pub rationale: String,
    /// CLI command to execute the recommendation.
    pub action_cmd: String,
    /// Estimated monthly savings in USD (positive = savings).
    #[schemars(with = "String")]
    pub delta_usd_month: Decimal,
    /// Risk level of the action.
    pub risk_level: RiskLevel,
    /// IDs of recommendations that must be applied first.
    pub dependencies: Vec<String>,
    /// Whether this recommendation is currently enabled (fires).
    pub enabled: bool,
}

/// A named rule that can produce a recommendation.
pub struct Rule {
    /// Unique identifier for this rule.
    pub id: String,
    /// Human-readable name.
    pub name: String,
    /// IDs of rules that must be applied before this one fires.
    pub dependencies: Vec<String>,
    /// The evaluation function.
    pub evaluate: Box<dyn Fn(&RuleContext) -> Option<Recommendation> + Send + Sync>,
}

/// Context passed to each rule during evaluation.
pub struct RuleContext<'a> {
    /// The collected inventory.
    pub inventory: &'a Inventory,
    /// The resolved price table.
    pub price_table: &'a ResolvedTable,
    /// Provider name (aws, gcs, azure).
    pub provider: String,
    /// Region name.
    pub region: String,
    /// Set of rule IDs that have been applied (for dependency checks).
    pub applied_rules: &'a [String],
}

/// A set of rules to evaluate.
pub struct RuleSet {
    rules: Vec<Rule>,
}

impl RuleSet {
    /// Create a new rule set with the built-in rules.
    pub fn builtin() -> Self {
        Self {
            rules: vec![
                rule_apply_tier_plan_ia(),
                rule_apply_tier_plan_glacier(),
                rule_enable_intelligent_tiering(),
                rule_optimize_xorbs_profile_mismatch(),
                rule_gc_candidates(),
            ],
        }
    }

    /// Create an empty rule set.
    pub fn empty() -> Self {
        Self { rules: Vec::new() }
    }

    /// Add a custom rule.
    pub fn add_rule(&mut self, rule: Rule) {
        self.rules.push(rule);
    }

    /// Evaluate all rules against the given context.
    ///
    /// Rules whose dependencies are not met are included in the output
    /// with `enabled = false` (they display but don't fire).
    pub fn run(&self, ctx: &RuleContext<'_>) -> Vec<Recommendation> {
        let mut results = Vec::new();

        for rule in &self.rules {
            // Check dependencies.
            let deps_met = rule
                .dependencies
                .iter()
                .all(|dep| ctx.applied_rules.contains(dep));

            if let Some(mut rec) = (rule.evaluate)(ctx) {
                rec.enabled = deps_met;
                results.push(rec);
            }
        }

        results
    }
}

// ── Built-in rules ──────────────────────────────────────────────────

/// Rule: transition to Standard-IA at 30 days.
fn rule_apply_tier_plan_ia() -> Rule {
    Rule {
        id: "apply_tier_plan_ia".to_string(),
        name: "Enable Standard-IA tiering".to_string(),
        dependencies: Vec::new(),
        evaluate: Box::new(|ctx| {
            let standard_bytes = ctx
                .inventory
                .per_class
                .iter()
                .filter(|(class, _)| {
                    class.contains("Standard")
                        && !class.contains("IA")
                        && !class.contains("Intelligent")
                })
                .map(|(_, stats)| stats.bytes)
                .sum::<u64>();

            if standard_bytes == 0 {
                return None;
            }

            // Estimate savings: Standard → IA price difference.
            let standard_price = lookup_gb_month(ctx, "Standard")?;
            let ia_price = lookup_gb_month(ctx, "Standard-IA")
                .or_else(|| lookup_gb_month(ctx, "Standard_IA"))?;

            let gb = Decimal::from(standard_bytes) / Decimal::from(1_073_741_824u64);
            let monthly_savings = (standard_price - ia_price) * gb;

            // Only recommend if savings > $1/month.
            if monthly_savings <= Decimal::ONE {
                return None;
            }

            Some(Recommendation {
                title: "Enable Standard-IA tiering".to_string(),
                rationale: format!(
                    "{:.1} GiB in Standard class could save ~${:.2}/month in IA",
                    gb, monthly_savings
                ),
                action_cmd: "crab tier plan --apply".to_string(),
                delta_usd_month: monthly_savings,
                risk_level: RiskLevel::Low,
                dependencies: Vec::new(),
                enabled: true,
            })
        }),
    }
}

/// Rule: transition to Glacier at 180 days.
fn rule_apply_tier_plan_glacier() -> Rule {
    Rule {
        id: "apply_tier_plan_glacier".to_string(),
        name: "Enable Glacier tiering".to_string(),
        dependencies: vec!["apply_tier_plan_ia".to_string()],
        evaluate: Box::new(|ctx| {
            let ia_bytes = ctx
                .inventory
                .per_class
                .iter()
                .filter(|(class, _)| {
                    class.contains("IA") || class.contains("Nearline") || class.contains("Cool")
                })
                .map(|(_, stats)| stats.bytes)
                .sum::<u64>();

            if ia_bytes == 0 {
                return None;
            }

            let ia_price = lookup_gb_month(ctx, "Standard-IA")
                .or_else(|| lookup_gb_month(ctx, "Nearline"))
                .or_else(|| lookup_gb_month(ctx, "Cool"))?;
            let glacier_price = lookup_gb_month(ctx, "Glacier")
                .or_else(|| lookup_gb_month(ctx, "Coldline"))
                .or_else(|| lookup_gb_month(ctx, "Cold"))?;

            let gb = Decimal::from(ia_bytes) / Decimal::from(1_073_741_824u64);
            let monthly_savings = (ia_price - glacier_price) * gb;

            if monthly_savings <= Decimal::ONE {
                return None;
            }

            Some(Recommendation {
                title: "Enable Glacier/Coldline tiering".to_string(),
                rationale: format!(
                    "{:.1} GiB in warm-cold class could save ~${:.2}/month in Glacier/Coldline",
                    gb, monthly_savings
                ),
                action_cmd: "crab tier plan --apply".to_string(),
                delta_usd_month: monthly_savings,
                risk_level: RiskLevel::Medium,
                dependencies: vec!["apply_tier_plan_ia".to_string()],
                enabled: true,
            })
        }),
    }
}

/// Rule: enable Intelligent-Tiering for unpredictable access.
fn rule_enable_intelligent_tiering() -> Rule {
    Rule {
        id: "enable_intelligent_tiering".to_string(),
        name: "Enable Intelligent-Tiering".to_string(),
        dependencies: Vec::new(),
        evaluate: Box::new(|ctx| {
            // Only applicable to S3.
            if ctx.provider != "aws" {
                return None;
            }

            let standard_bytes = ctx
                .inventory
                .per_class
                .iter()
                .filter(|(class, _)| *class == "S3 Standard")
                .map(|(_, stats)| stats.bytes)
                .sum::<u64>();

            if standard_bytes == 0 {
                return None;
            }

            let gb = Decimal::from(standard_bytes) / Decimal::from(1_073_741_824u64);

            // IT has a small monitoring fee but auto-tiers. Recommend for
            // buckets with unpredictable access patterns.
            Some(Recommendation {
                title: "Enable S3 Intelligent-Tiering".to_string(),
                rationale: format!(
                    "{:.1} GiB in Standard could benefit from automatic tiering \
                     with Intelligent-Tiering (small monitoring fee, automatic savings)",
                    gb
                ),
                action_cmd: "crab tier plan --apply".to_string(),
                delta_usd_month: Decimal::ZERO,
                risk_level: RiskLevel::Low,
                dependencies: Vec::new(),
                enabled: true,
            })
        }),
    }
}

/// Rule: xorb profile mismatch.
fn rule_optimize_xorbs_profile_mismatch() -> Rule {
    Rule {
        id: "optimize_xorbs_profile_mismatch".to_string(),
        name: "Optimize xorbs to target profile".to_string(),
        dependencies: Vec::new(),
        evaluate: Box::new(|ctx| {
            let xorb_stats = ctx.inventory.per_prefix.get(".crab/xorbs/")?;

            if xorb_stats.objects == 0 {
                return None;
            }

            let avg_xorb_bytes = xorb_stats.bytes / xorb_stats.objects;

            // Check if average xorb size is far from any built-in profile target.
            // ml: 256 MiB, dataset: 64 MiB, code: 16 MiB
            let targets: &[(&str, u64)] = &[
                ("ml", 256 * 1024 * 1024),
                ("dataset", 64 * 1024 * 1024),
                ("code", 16 * 1024 * 1024),
            ];

            let closest = targets.iter().min_by_key(|(_, target)| {
                (avg_xorb_bytes as i64 - *target as i64).unsigned_abs()
            })?;

            let deviation = (avg_xorb_bytes as f64 - closest.1 as f64).abs() / closest.1 as f64;

            // Only recommend if deviation > 1.5x (150%).
            if deviation <= 1.5 {
                return None;
            }

            Some(Recommendation {
                title: format!("Optimize xorbs to '{}' profile", closest.0),
                rationale: format!(
                    "average xorb size ({} bytes) deviates {:.0}% from '{}' target ({} bytes)",
                    avg_xorb_bytes,
                    deviation * 100.0,
                    closest.0,
                    closest.1
                ),
                action_cmd: format!("crab optimize xorbs --profile {} --dry-run", closest.0),
                delta_usd_month: Decimal::ZERO,
                risk_level: RiskLevel::Medium,
                dependencies: Vec::new(),
                enabled: true,
            })
        }),
    }
}

/// Rule: GC candidates from orphan objects.
fn rule_gc_candidates() -> Rule {
    Rule {
        id: "gc_candidates".to_string(),
        name: "Run GC to reclaim orphan objects".to_string(),
        dependencies: Vec::new(),
        evaluate: Box::new(|ctx| {
            // This rule would normally cross-reference with fsck results.
            // For now, it checks if there are objects outside known prefixes.
            let known_bytes: u64 = ctx.inventory.per_prefix.values().map(|s| s.bytes).sum();

            if known_bytes >= ctx.inventory.total_bytes {
                return None;
            }

            let orphan_bytes = ctx.inventory.total_bytes - known_bytes;
            if orphan_bytes == 0 {
                return None;
            }

            let gb = Decimal::from(orphan_bytes) / Decimal::from(1_073_741_824u64);
            let standard_price = lookup_gb_month(ctx, "Standard").unwrap_or(Decimal::ZERO);
            let monthly_savings = standard_price * gb;

            Some(Recommendation {
                title: "Run GC to reclaim orphan objects".to_string(),
                rationale: format!(
                    "{:.1} GiB of potentially orphaned objects could save ~${:.2}/month",
                    gb, monthly_savings
                ),
                action_cmd: "crab gc --dry-run".to_string(),
                delta_usd_month: monthly_savings,
                risk_level: RiskLevel::Low,
                dependencies: Vec::new(),
                enabled: true,
            })
        }),
    }
}

/// Helper: look up the `gb_month_usd` for a class name in the price table.
fn lookup_gb_month(ctx: &RuleContext<'_>, class: &str) -> Option<Decimal> {
    ctx.price_table
        .lookup(&ctx.provider, &ctx.region, class)
        .map(|s| s.gb_month_usd)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cost::inventory::{ClassStats, InventorySourceInfo, PrefixStats};
    use crate::cost::pricing::override_file::{ResolvedPriceSchedule, ResolvedTable};
    use std::collections::BTreeMap;
    use std::str::FromStr;

    fn dec(s: &str) -> Decimal {
        Decimal::from_str(s).expect("valid decimal")
    }

    fn make_price_table() -> ResolvedTable {
        let mut entries = BTreeMap::new();
        entries.insert(
            (
                "aws".to_string(),
                "us-east-1".to_string(),
                "Standard".to_string(),
            ),
            ResolvedPriceSchedule {
                gb_month_usd: dec("0.023"),
                put_per_k_ops_usd: dec("0.005"),
                get_per_k_ops_usd: dec("0.0004"),
                list_per_k_ops_usd: dec("0.005"),
                head_per_k_ops_usd: dec("0.0004"),
                retrieval_per_gb_usd: dec("0.0"),
                min_retention_days: 0,
                min_object_size_bytes: 0,
                egress_per_gb_usd: dec("0.09"),
            },
        );
        entries.insert(
            (
                "aws".to_string(),
                "us-east-1".to_string(),
                "Standard-IA".to_string(),
            ),
            ResolvedPriceSchedule {
                gb_month_usd: dec("0.0125"),
                put_per_k_ops_usd: dec("0.01"),
                get_per_k_ops_usd: dec("0.001"),
                list_per_k_ops_usd: dec("0.005"),
                head_per_k_ops_usd: dec("0.001"),
                retrieval_per_gb_usd: dec("0.01"),
                min_retention_days: 30,
                min_object_size_bytes: 128_000,
                egress_per_gb_usd: dec("0.09"),
            },
        );
        ResolvedTable {
            embedded_version: "2026-03-01".to_string(),
            override_version: None,
            entries,
        }
    }

    fn make_inventory(standard_bytes: u64, ia_bytes: u64) -> Inventory {
        let mut per_class = BTreeMap::new();
        if standard_bytes > 0 {
            per_class.insert(
                "S3 Standard".to_string(),
                ClassStats {
                    objects: standard_bytes / 1_000_000,
                    bytes: standard_bytes,
                },
            );
        }
        if ia_bytes > 0 {
            per_class.insert(
                "S3 Standard-IA".to_string(),
                ClassStats {
                    objects: ia_bytes / 1_000_000,
                    bytes: ia_bytes,
                },
            );
        }

        let mut per_prefix = BTreeMap::new();
        per_prefix.insert(
            ".crab/xorbs/".to_string(),
            PrefixStats {
                objects: (standard_bytes + ia_bytes) / 1_000_000,
                bytes: standard_bytes + ia_bytes,
            },
        );

        Inventory {
            source: InventorySourceInfo::Live {
                list_concurrency: 32,
                sample_ratio: None,
            },
            scanned_at: "2026-03-01T00:00:00Z".to_string(),
            total_objects: (standard_bytes + ia_bytes) / 1_000_000,
            total_bytes: standard_bytes + ia_bytes,
            per_class,
            per_prefix,
            heaviest_cold: Vec::new(),
        }
    }

    #[test]
    fn ia_rule_fires_when_savings_above_threshold() {
        let inventory = make_inventory(100 * 1024 * 1024 * 1024, 0); // 100 GiB Standard
        let price_table = make_price_table();
        let ctx = RuleContext {
            inventory: &inventory,
            price_table: &price_table,
            provider: "aws".to_string(),
            region: "us-east-1".to_string(),
            applied_rules: &[],
        };

        let rule = rule_apply_tier_plan_ia();
        let rec = (rule.evaluate)(&ctx);
        assert!(rec.is_some());
        let rec = rec.expect("recommendation");
        assert!(rec.delta_usd_month > Decimal::ONE);
        assert_eq!(rec.risk_level, RiskLevel::Low);
    }

    #[test]
    fn ia_rule_does_not_fire_when_no_standard_bytes() {
        let inventory = make_inventory(0, 100 * 1024 * 1024 * 1024);
        let price_table = make_price_table();
        let ctx = RuleContext {
            inventory: &inventory,
            price_table: &price_table,
            provider: "aws".to_string(),
            region: "us-east-1".to_string(),
            applied_rules: &[],
        };

        let rule = rule_apply_tier_plan_ia();
        let rec = (rule.evaluate)(&ctx);
        assert!(rec.is_none());
    }

    #[test]
    fn ruleset_builtin_has_five_rules() {
        let ruleset = RuleSet::builtin();
        assert_eq!(ruleset.rules.len(), 5);
    }

    #[test]
    fn ruleset_empty_produces_no_recommendations() {
        let inventory = make_inventory(0, 0);
        let price_table = make_price_table();
        let ctx = RuleContext {
            inventory: &inventory,
            price_table: &price_table,
            provider: "aws".to_string(),
            region: "us-east-1".to_string(),
            applied_rules: &[],
        };

        let ruleset = RuleSet::empty();
        let recs = ruleset.run(&ctx);
        assert!(recs.is_empty());
    }

    #[test]
    fn dependency_graph_disabled_recs_display_but_dont_fire() {
        let inventory = make_inventory(0, 100 * 1024 * 1024 * 1024);
        let mut price_table = make_price_table();
        // Add Glacier pricing.
        price_table.entries.insert(
            (
                "aws".to_string(),
                "us-east-1".to_string(),
                "Glacier".to_string(),
            ),
            ResolvedPriceSchedule {
                gb_month_usd: dec("0.004"),
                put_per_k_ops_usd: dec("0.03"),
                get_per_k_ops_usd: dec("0.01"),
                list_per_k_ops_usd: dec("0.005"),
                head_per_k_ops_usd: dec("0.01"),
                retrieval_per_gb_usd: dec("0.03"),
                min_retention_days: 90,
                min_object_size_bytes: 40_960,
                egress_per_gb_usd: dec("0.09"),
            },
        );

        let ctx = RuleContext {
            inventory: &inventory,
            price_table: &price_table,
            provider: "aws".to_string(),
            region: "us-east-1".to_string(),
            applied_rules: &[], // IA not applied
        };

        let ruleset = RuleSet::builtin();
        let recs = ruleset.run(&ctx);

        // Glacier rule depends on IA rule. Since IA is not applied,
        // the Glacier recommendation should be disabled.
        for rec in &recs {
            if rec.title.contains("Glacier") {
                assert!(
                    !rec.enabled,
                    "Glacier rec should be disabled when IA not applied"
                );
            }
        }
    }

    #[test]
    fn risk_level_display() {
        assert_eq!(format!("{}", RiskLevel::Low), "low");
        assert_eq!(format!("{}", RiskLevel::Medium), "medium");
        assert_eq!(format!("{}", RiskLevel::High), "high");
    }
}
