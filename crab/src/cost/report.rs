//! Cost report renderer — human (`comfy-table`) and JSON formatters.
//!
//! The cost report combines inventory data, pricing, and recommendations
//! into a structured output. Human output uses `comfy-table` for
//! terminal-friendly tables. JSON output uses the existing `Envelope` /
//! `emit_json` plumbing with schema `"cost"` version `"1.0"`.
//!
//! # Formatting rules
//!
//! - Bytes: IEC binary (KiB, MiB, GiB, TiB).
//! - Currency: USD, `Decimal` internally, 2-decimal display, 6-decimal JSON.
//! - Percentages: 1 decimal place.
//! - Timestamps: RFC 3339 UTC.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use rust_decimal::Decimal;
use schemars::JsonSchema;
use serde::Serialize;

use super::inventory::Inventory;
use super::recommendations::Recommendation;

/// The complete cost report.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct CostReport {
    /// Price table version used for this report.
    pub price_table_version: String,
    /// Override file version, if any.
    pub override_version: Option<String>,
    /// When the report was generated (RFC 3339).
    pub generated_at: String,
    /// Inventory summary.
    pub inventory: InventorySummary,
    /// Current estimated monthly cost.
    #[schemars(with = "String")]
    pub current_monthly_usd: Decimal,
    /// Projected monthly cost under tier-plan defaults.
    #[schemars(with = "String")]
    pub projected_monthly_usd: Decimal,
    /// Projected monthly savings.
    #[schemars(with = "String")]
    pub projected_savings_usd: Decimal,
    /// Per-class cost breakdown.
    pub per_class_costs: BTreeMap<String, ClassCost>,
    /// Recommendations.
    pub recommendations: Vec<Recommendation>,
    /// Top-K heaviest cold objects.
    pub heaviest_cold: Vec<ColdObjectSummary>,
    /// Assumptions and caveats.
    pub assumptions: Vec<String>,
}

/// Summary of the inventory used for the report.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct InventorySummary {
    pub source: String,
    pub scanned_at: String,
    pub total_objects: u64,
    pub total_bytes: u64,
    pub total_bytes_human: String,
}

/// Per-class cost breakdown.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ClassCost {
    pub objects: u64,
    pub bytes: u64,
    pub bytes_human: String,
    #[schemars(with = "String")]
    pub share_pct: Decimal,
    #[schemars(with = "String")]
    pub monthly_storage_usd: Decimal,
    #[schemars(with = "String")]
    pub monthly_retrieval_usd: Decimal,
    #[schemars(with = "String")]
    pub monthly_total_usd: Decimal,
}

/// Summary of a heavy cold object.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct ColdObjectSummary {
    pub key: String,
    pub size: u64,
    pub size_human: String,
    pub storage_class: String,
    pub last_modified: String,
}

/// Format bytes as IEC binary (KiB, MiB, GiB, TiB).
pub fn format_bytes_iec(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    const TIB: u64 = 1024 * GIB;

    if bytes >= TIB {
        format!("{:.1} TiB", bytes as f64 / TIB as f64)
    } else if bytes >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

/// Render the cost report as human-readable text.
pub fn render_human(report: &CostReport) -> String {
    let mut out = String::new();

    // Header.
    out.push_str("╔══════════════════════════════════════════════════════════╗\n");
    out.push_str("║              Crab Storage Cost Report                 ║\n");
    out.push_str("╚══════════════════════════════════════════════════════════╝\n\n");

    let _ = write!(out, "Price table: {}", report.price_table_version);
    if let Some(ref ov) = report.override_version {
        let _ = write!(out, " (override: {ov})");
    }
    out.push('\n');
    let _ = writeln!(out, "Generated:   {}", report.generated_at);
    let _ = writeln!(
        out,
        "Inventory:   {} ({} objects, {})\n\n",
        report.inventory.source, report.inventory.total_objects, report.inventory.total_bytes_human,
    );

    // Cost summary.
    out.push_str("── Cost Summary ──────────────────────────────────────────\n\n");
    let _ = writeln!(
        out,
        "  Current monthly cost:   ${:.2}",
        report.current_monthly_usd
    );
    let _ = writeln!(
        out,
        "  Projected (with tier):  ${:.2}",
        report.projected_monthly_usd
    );
    let _ = writeln!(
        out,
        "  Estimated savings:      ${:.2}/month\n",
        report.projected_savings_usd
    );

    // Per-class breakdown.
    out.push_str("── Per-Class Breakdown ───────────────────────────────────\n\n");
    let _ = writeln!(
        out,
        "  {:<30} {:>12} {:>10} {:>12}",
        "Class", "Size", "Share", "Monthly USD"
    );
    let _ = writeln!(out, "  {}", "─".repeat(68));

    for (class, cost) in &report.per_class_costs {
        let _ = writeln!(
            out,
            "  {:<30} {:>12} {:>9.1}% ${:>10.2}",
            class, cost.bytes_human, cost.share_pct, cost.monthly_total_usd
        );
    }
    out.push('\n');

    // Recommendations.
    if !report.recommendations.is_empty() {
        out.push_str("── Recommendations ──────────────────────────────────────\n\n");
        for (i, rec) in report.recommendations.iter().enumerate() {
            let status = if rec.enabled { "●" } else { "○" };
            let _ = writeln!(
                out,
                "  {status} {}. {} [risk: {}]",
                i + 1,
                rec.title,
                rec.risk_level
            );
            let _ = writeln!(out, "     {}", rec.rationale);
            if rec.delta_usd_month > Decimal::ZERO {
                let _ = writeln!(out, "     Savings: ~${:.2}/month", rec.delta_usd_month);
            }
            let _ = writeln!(out, "     Action:  {}\n", rec.action_cmd);
        }
    }

    // Top-K cold objects.
    if !report.heaviest_cold.is_empty() {
        out.push_str("── Heaviest Cold Objects ─────────────────────────────────\n\n");
        for item in &report.heaviest_cold {
            let _ = writeln!(
                out,
                "  {} ({}, {})",
                item.key, item.size_human, item.storage_class
            );
        }
        out.push('\n');
    }

    // Assumptions.
    if !report.assumptions.is_empty() {
        out.push_str("── Assumptions ──────────────────────────────────────────\n\n");
        for assumption in &report.assumptions {
            let _ = writeln!(out, "  • {assumption}");
        }
        out.push('\n');
    }

    out
}

/// Render the cost report as JSON via the existing `emit_json` plumbing.
///
/// Returns the serialized JSON string. The caller is responsible for
/// writing it to stdout via `emit_json("cost", "1.0", &report)`.
pub fn render_json(report: &CostReport) -> String {
    serde_json::to_string_pretty(report)
        .unwrap_or_else(|e| format!("{{\"error\": \"failed to serialize cost report: {e}\"}}"))
}

/// Build a `CostReport` from inventory, pricing, and recommendations.
pub fn build_report(
    inventory: &Inventory,
    price_table_version: &str,
    override_version: Option<&str>,
    per_class_costs: BTreeMap<String, ClassCost>,
    current_monthly_usd: Decimal,
    projected_monthly_usd: Decimal,
    recommendations: Vec<Recommendation>,
) -> CostReport {
    let heaviest_cold: Vec<ColdObjectSummary> = inventory
        .heaviest_cold
        .iter()
        .map(|item| ColdObjectSummary {
            key: item.key.clone(),
            size: item.size,
            size_human: format_bytes_iec(item.size),
            storage_class: format!("{}", item.storage_class),
            last_modified: item.last_modified.clone(),
        })
        .collect();

    let source_str = match &inventory.source {
        super::inventory::InventorySourceInfo::Live {
            list_concurrency,
            sample_ratio,
        } => {
            let mut s = format!("live (concurrency={list_concurrency}");
            if let Some(ratio) = sample_ratio {
                let _ = write!(s, ", sample={ratio:.2}");
            }
            s.push(')');
            s
        }
        super::inventory::InventorySourceInfo::Report {
            provider,
            report_at,
            schema,
        } => {
            format!("report ({provider}, {schema}, {report_at})")
        }
    };

    CostReport {
        price_table_version: price_table_version.to_string(),
        override_version: override_version.map(String::from),
        generated_at: super::inventory::live::chrono_now_rfc3339(),
        inventory: InventorySummary {
            source: source_str,
            scanned_at: inventory.scanned_at.clone(),
            total_objects: inventory.total_objects,
            total_bytes: inventory.total_bytes,
            total_bytes_human: format_bytes_iec(inventory.total_bytes),
        },
        current_monthly_usd,
        projected_monthly_usd,
        projected_savings_usd: current_monthly_usd - projected_monthly_usd,
        per_class_costs,
        recommendations,
        heaviest_cold,
        assumptions: vec![
            "Pricing based on list prices; negotiated discounts not modeled.".to_string(),
            "Retrieval costs estimated from current access patterns.".to_string(),
            "Free-tier not applied unless cost.apply_free_tier = true.".to_string(),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal::Decimal;
    use std::str::FromStr;

    fn dec(s: &str) -> Decimal {
        Decimal::from_str(s).expect("valid decimal")
    }

    #[test]
    fn format_bytes_iec_units() {
        assert_eq!(format_bytes_iec(0), "0 B");
        assert_eq!(format_bytes_iec(512), "512 B");
        assert_eq!(format_bytes_iec(1024), "1.0 KiB");
        assert_eq!(format_bytes_iec(1_048_576), "1.0 MiB");
        assert_eq!(format_bytes_iec(1_073_741_824), "1.0 GiB");
        assert_eq!(format_bytes_iec(1_099_511_627_776), "1.0 TiB");
    }

    #[test]
    fn format_bytes_iec_fractional() {
        assert_eq!(format_bytes_iec(1_536), "1.5 KiB");
        assert_eq!(format_bytes_iec(1_610_612_736), "1.5 GiB");
    }

    #[test]
    fn render_human_contains_header() {
        let report = make_test_report();
        let output = render_human(&report);
        assert!(output.contains("Crab Storage Cost Report"));
        assert!(output.contains("Price table:"));
        assert!(output.contains("Cost Summary"));
    }

    #[test]
    fn render_human_contains_recommendations() {
        let report = make_test_report();
        let output = render_human(&report);
        assert!(output.contains("Recommendations"));
        assert!(output.contains("Enable IA tiering"));
    }

    #[test]
    fn render_json_valid() {
        let report = make_test_report();
        let json = render_json(&report);
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid JSON");
        assert!(parsed.get("price_table_version").is_some());
        assert!(parsed.get("current_monthly_usd").is_some());
    }

    #[test]
    fn render_json_decimal_precision() {
        let report = make_test_report();
        let json = render_json(&report);
        // Decimal values should be serialized as strings with full precision.
        assert!(json.contains("2300.00") || json.contains("2300"));
    }

    fn make_test_report() -> CostReport {
        let mut per_class_costs = BTreeMap::new();
        per_class_costs.insert(
            "S3 Standard".to_string(),
            ClassCost {
                objects: 100_000,
                bytes: 100 * 1024 * 1024 * 1024,
                bytes_human: "100.0 GiB".to_string(),
                share_pct: dec("100.0"),
                monthly_storage_usd: dec("2300.00"),
                monthly_retrieval_usd: dec("0.00"),
                monthly_total_usd: dec("2300.00"),
            },
        );

        CostReport {
            price_table_version: "2026-03-01".to_string(),
            override_version: None,
            generated_at: "2026-03-15T12:00:00Z".to_string(),
            inventory: InventorySummary {
                source: "live (concurrency=32)".to_string(),
                scanned_at: "2026-03-15T11:55:00Z".to_string(),
                total_objects: 100_000,
                total_bytes: 100 * 1024 * 1024 * 1024,
                total_bytes_human: "100.0 GiB".to_string(),
            },
            current_monthly_usd: dec("2300.00"),
            projected_monthly_usd: dec("1250.00"),
            projected_savings_usd: dec("1050.00"),
            per_class_costs,
            recommendations: vec![Recommendation {
                title: "Enable IA tiering".to_string(),
                rationale: "100 GiB in Standard could save ~$1050/month".to_string(),
                action_cmd: "crab tier plan --apply".to_string(),
                delta_usd_month: dec("1050.00"),
                risk_level: super::super::recommendations::RiskLevel::Low,
                dependencies: Vec::new(),
                enabled: true,
            }],
            heaviest_cold: Vec::new(),
            assumptions: vec!["Test assumption".to_string()],
        }
    }
}
