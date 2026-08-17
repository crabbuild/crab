//! Runtime cost-report pipeline.
//!
//! This module owns the read-only path from a configured Crab store to a
//! priced report. CLI modules only resolve the remote and render the result.

use std::collections::BTreeMap;
use std::sync::Arc;

use rust_decimal::Decimal;
use tokio_util::sync::CancellationToken;

use crate::core::config::Config;
use crate::core::error::{CrabError, Result};
use crate::storage::Store;
use crate::tier::provider::Provider;
use crate::tier::runtime;

use super::inventory::live::{self, LiveWalkConfig};
use super::inventory::{self, InventorySource};
use super::pricing::embedded::PRICE_TABLE_VERSION;
use super::pricing::override_file::{self, ResolvedTable};
use super::recommendations::{RuleContext, RuleSet};
use super::report::{self, ClassCost, CostReport};

const BYTES_PER_GIB: u64 = 1_073_741_824;
const DEFAULT_TOP_K_COLD: usize = 100;
const MAX_LIST_CONCURRENCY: u32 = 128;

/// CLI overrides for a cost report.
#[derive(Debug, Clone, Default)]
pub struct ReportOptions {
    /// Optional pricing override path.
    pub pricing_file: Option<String>,
    /// Optional inventory source selector.
    pub inventory_source: Option<String>,
    /// Optional deterministic live-walk sample ratio.
    pub sample_ratio: Option<f64>,
    /// Optional number of cold objects to retain in the report.
    pub top_k: Option<usize>,
}

/// Collect and price a live inventory for the configured Crab store.
pub async fn build_report(
    config: &Config,
    store: &Store,
    options: &ReportOptions,
    cancel: &CancellationToken,
) -> Result<CostReport> {
    check_cancelled(cancel)?;

    let requested_source = options
        .inventory_source
        .as_deref()
        .unwrap_or(&config.cost.inventory_source)
        .parse::<InventorySource>()
        .map_err(|error| CrabError::Configuration {
            key: "cost.inventory_source".to_string(),
            origin: error.to_string(),
        })?;
    let source = inventory::resolve_source(
        &requested_source,
        false,
        None,
        config.cost.report_max_staleness_hours,
    );
    if source == InventorySource::Report {
        return Err(CrabError::Configuration {
            key: "cost.inventory_source=report".to_string(),
            origin:
                "provider inventory report discovery is not configured; use --inventory-source live"
                    .to_string(),
        });
    }

    let list_concurrency = config.cost.list_concurrency;
    if !(1..=MAX_LIST_CONCURRENCY).contains(&list_concurrency) {
        return Err(CrabError::Configuration {
            key: format!("cost.list_concurrency={list_concurrency}"),
            origin: format!("expected a value from 1 through {MAX_LIST_CONCURRENCY}"),
        });
    }

    let sample_ratio = options.sample_ratio.unwrap_or(config.cost.sample_ratio);
    validate_sample_ratio(sample_ratio)?;
    if config.cost.apply_free_tier {
        return Err(CrabError::Configuration {
            key: "cost.apply_free_tier".to_string(),
            origin: "free-tier modeling has no account-age or billing-account contract; unset it for a list-price report".to_string(),
        });
    }

    let pricing_path = options
        .pricing_file
        .as_deref()
        .filter(|path| !path.trim().is_empty())
        .or_else(|| {
            (!config.cost.pricing_file.trim().is_empty())
                .then_some(config.cost.pricing_file.as_str())
        })
        .map(crab_auth::token_cache::expand_token_cache_path);
    let price_table = override_file::load_resolved_table(pricing_path.as_deref())?;
    validate_price_table_version(config, &price_table)?;

    let provider = runtime::resolve_provider(config);
    let inventory = live::walk_live(
        Arc::clone(store.inner()),
        LiveWalkConfig {
            list_concurrency,
            sample_ratio: (sample_ratio < 1.0).then_some(sample_ratio),
            top_k_cold: options.top_k.unwrap_or(DEFAULT_TOP_K_COLD),
            provider,
        },
        cancel,
    )
    .await?;
    check_cancelled(cancel)?;

    let region = pricing_region(config, provider);
    let provider_name = provider_name(provider);
    let (per_class_costs, current_monthly_usd) =
        price_inventory(&inventory, &price_table, provider_name, &region)?;

    let applied_rules = Vec::new();
    let recommendations = RuleSet::builtin().run(&RuleContext {
        inventory: &inventory,
        price_table: &price_table,
        provider: provider_name.to_string(),
        region,
        applied_rules: &applied_rules,
    });

    let mut report = report::build_report(
        &inventory,
        &price_table.embedded_version,
        price_table.override_version.as_deref(),
        per_class_costs,
        current_monthly_usd,
        current_monthly_usd,
        recommendations,
    );
    report.assumptions.extend([
        "Live inventory infers each provider's standard storage class because object_store list metadata does not expose storage class consistently; retrieval charges are omitted without access telemetry.".to_string(),
        "Projected cost equals current cost until age/access data is available to model tier transitions.".to_string(),
    ]);
    if sample_ratio < 1.0 {
        report.assumptions.push(format!(
            "Inventory is a deterministic {sample_ratio:.4} sample; object and byte totals are scaled estimates."
        ));
    }
    Ok(report)
}

fn check_cancelled(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        Err(CrabError::Cancelled)
    } else {
        Ok(())
    }
}

fn validate_sample_ratio(ratio: f64) -> Result<()> {
    if !(ratio.is_finite() && 0.0 < ratio && ratio <= 1.0) {
        return Err(CrabError::Configuration {
            key: format!("cost.sample_ratio={ratio}"),
            origin: "expected a finite value greater than 0 and no greater than 1".to_string(),
        });
    }
    Ok(())
}

fn validate_price_table_version(config: &Config, table: &ResolvedTable) -> Result<()> {
    if !config.cost.price_table_version.is_empty()
        && config.cost.price_table_version != PRICE_TABLE_VERSION
    {
        return Err(CrabError::Configuration {
            key: format!(
                "cost.price_table_version={}",
                config.cost.price_table_version
            ),
            origin: format!(
                "this binary embeds price table {PRICE_TABLE_VERSION}; rebuild Crab to select another table"
            ),
        });
    }
    if table.embedded_version != PRICE_TABLE_VERSION {
        return Err(CrabError::Internal(format!(
            "resolved price table version {} does not match embedded version {PRICE_TABLE_VERSION}",
            table.embedded_version
        )));
    }
    Ok(())
}

fn price_inventory(
    inventory: &super::inventory::Inventory,
    table: &ResolvedTable,
    provider: &str,
    region: &str,
) -> Result<(BTreeMap<String, ClassCost>, Decimal)> {
    let mut per_class = BTreeMap::new();
    let mut current_monthly_usd = Decimal::ZERO;

    for (display_class, stats) in &inventory.per_class {
        let pricing_class =
            pricing_class(provider, display_class).ok_or_else(|| CrabError::Configuration {
                key: format!("storage class {display_class}"),
                origin: format!("no {provider} price exists for this live inventory class"),
            })?;
        let schedule = table
            .lookup(provider, region, pricing_class)
            .ok_or_else(|| CrabError::Configuration {
                key: format!("pricing {provider}/{region}/{pricing_class}"),
                origin: "price table has no matching storage class".to_string(),
            })?;

        let storage_gib = Decimal::from(stats.bytes) / Decimal::from(BYTES_PER_GIB);
        let monthly_storage_usd = storage_gib * schedule.gb_month_usd;
        let monthly_retrieval_usd = Decimal::ZERO;
        let monthly_total_usd = monthly_storage_usd + monthly_retrieval_usd;
        current_monthly_usd += monthly_total_usd;

        let share_pct = if inventory.total_bytes == 0 {
            Decimal::ZERO
        } else {
            Decimal::from(stats.bytes) * Decimal::from(100u64)
                / Decimal::from(inventory.total_bytes)
        };
        per_class.insert(
            display_class.clone(),
            ClassCost {
                objects: stats.objects,
                bytes: stats.bytes,
                bytes_human: report::format_bytes_iec(stats.bytes),
                share_pct,
                monthly_storage_usd,
                monthly_retrieval_usd,
                monthly_total_usd,
            },
        );
    }

    Ok((per_class, current_monthly_usd))
}

fn provider_name(provider: Provider) -> &'static str {
    match provider {
        Provider::S3 => "aws",
        Provider::Gcs => "gcs",
        Provider::Azure => "azure",
    }
}

fn pricing_region(config: &Config, provider: Provider) -> String {
    match provider {
        Provider::S3 => config
            .remote_region
            .clone()
            .or_else(|| config.auth.aws.region.clone())
            .or_else(|| std::env::var("AWS_REGION").ok())
            .or_else(|| std::env::var("AWS_DEFAULT_REGION").ok())
            .unwrap_or_else(|| "us-east-1".to_string()),
        Provider::Gcs => std::env::var("GOOGLE_CLOUD_REGION")
            .ok()
            .filter(|region| !region.is_empty())
            .unwrap_or_else(|| "us-central1".to_string()),
        Provider::Azure => std::env::var("AZURE_REGION")
            .ok()
            .filter(|region| !region.is_empty())
            .unwrap_or_else(|| "eastus".to_string()),
    }
}

fn pricing_class(provider: &str, display_class: &str) -> Option<&'static str> {
    match provider {
        "aws" => match display_class {
            "S3 Standard" => Some("Standard"),
            "S3 Standard-IA" => Some("Standard-IA"),
            "S3 One Zone-IA" => Some("One-Zone-IA"),
            "S3 Glacier Instant Retrieval" => Some("Glacier-Instant-Retrieval"),
            "S3 Glacier Flexible Retrieval" => Some("Glacier-Flexible-Retrieval"),
            "S3 Glacier Deep Archive" => Some("Glacier-Deep-Archive"),
            _ => None,
        },
        "gcs" => match display_class {
            "GCS Standard" => Some("Standard"),
            "GCS Nearline" => Some("Nearline"),
            "GCS Coldline" => Some("Coldline"),
            "GCS Archive" => Some("Archive"),
            _ => None,
        },
        "azure" => match display_class {
            "Azure Hot" => Some("Hot"),
            "Azure Cool" => Some("Cool"),
            "Azure Cold" => Some("Cold"),
            "Azure Archive" => Some("Archive"),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::str::FromStr;

    use super::*;
    use crate::cost::inventory::{ClassStats, InventorySourceInfo};

    #[test]
    fn pricing_classes_match_embedded_names() {
        assert_eq!(pricing_class("aws", "S3 Standard"), Some("Standard"));
        assert_eq!(
            pricing_class("aws", "S3 Glacier Deep Archive"),
            Some("Glacier-Deep-Archive")
        );
        assert_eq!(pricing_class("gcs", "GCS Nearline"), Some("Nearline"));
        assert_eq!(pricing_class("azure", "Azure Hot"), Some("Hot"));
    }

    #[test]
    fn sample_ratio_must_be_positive_and_bounded() {
        assert!(validate_sample_ratio(0.0).is_err());
        assert!(validate_sample_ratio(f64::NAN).is_err());
        assert!(validate_sample_ratio(1.0).is_ok());
        assert!(validate_sample_ratio(0.25).is_ok());
        assert!(validate_sample_ratio(1.01).is_err());
    }

    #[test]
    fn prices_live_standard_inventory_from_embedded_table() {
        let inventory = super::super::inventory::Inventory {
            source: InventorySourceInfo::Live {
                list_concurrency: 1,
                sample_ratio: None,
            },
            scanned_at: "2026-01-01T00:00:00Z".to_owned(),
            total_objects: 1,
            total_bytes: BYTES_PER_GIB,
            per_class: BTreeMap::from([(
                "S3 Standard".to_owned(),
                ClassStats {
                    objects: 1,
                    bytes: BYTES_PER_GIB,
                },
            )]),
            per_prefix: BTreeMap::new(),
            heaviest_cold: Vec::new(),
        };
        let table = override_file::load_resolved_table(None).unwrap();

        let (costs, total) = price_inventory(&inventory, &table, "aws", "us-east-1").unwrap();

        assert_eq!(total, Decimal::from_str("0.023").unwrap());
        assert_eq!(costs["S3 Standard"].monthly_total_usd, total);
    }
}
