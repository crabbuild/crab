//! User pricing override file — YAML parse, deep-merge, validation.
//!
//! Users can supply a `cost.pricing_file` that overrides specific
//! `(provider, region, class)` entries in the embedded price table.
//! The override is deep-merged: specified fields replace, missing
//! fields inherit from the embedded table.
//!
//! # Security
//!
//! Override files may contain commercial contract terms. On unix,
//! permissions are checked and must be `0600` (owner read/write only).
//! Looser permissions trigger a warning. On Windows, an informational
//! note is emitted (ACL checks are out of scope).
//!
//! Content is never logged.

use std::collections::BTreeMap;
use std::path::Path;

use rust_decimal::Decimal;
use serde::Deserialize;
use tracing::{info, warn};

use crate::core::error::{CrabError, Result};

/// A user-supplied pricing override file.
#[derive(Debug, Clone, Deserialize)]
pub struct OverrideFile {
    /// Optional version tag for the override.
    #[serde(default)]
    pub version: Option<String>,
    /// Provider overrides keyed by provider name (aws, gcs, azure).
    #[serde(default)]
    pub providers: BTreeMap<String, ProviderOverride>,
}

/// Per-provider override section.
#[derive(Debug, Clone, Deserialize)]
pub struct ProviderOverride {
    /// Region overrides keyed by region name.
    #[serde(default)]
    pub regions: BTreeMap<String, RegionOverride>,
}

/// Per-region override section.
#[derive(Debug, Clone, Deserialize)]
pub struct RegionOverride {
    /// Class overrides keyed by class name.
    #[serde(default)]
    pub classes: BTreeMap<String, ClassOverride>,
}

/// Per-class pricing override. All fields are optional — only specified
/// fields replace the embedded value.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ClassOverride {
    pub gb_month_usd: Option<Decimal>,
    pub put_per_k_ops_usd: Option<Decimal>,
    pub get_per_k_ops_usd: Option<Decimal>,
    pub list_per_k_ops_usd: Option<Decimal>,
    pub head_per_k_ops_usd: Option<Decimal>,
    pub retrieval_per_gb_usd: Option<Decimal>,
    pub min_retention_days: Option<u32>,
    pub min_object_size_bytes: Option<u64>,
    pub egress_per_gb_usd: Option<Decimal>,
}

/// A resolved price schedule after merging embedded + override.
#[derive(Debug, Clone)]
pub struct ResolvedPriceSchedule {
    pub gb_month_usd: Decimal,
    pub put_per_k_ops_usd: Decimal,
    pub get_per_k_ops_usd: Decimal,
    pub list_per_k_ops_usd: Decimal,
    pub head_per_k_ops_usd: Decimal,
    pub retrieval_per_gb_usd: Decimal,
    pub min_retention_days: u32,
    pub min_object_size_bytes: u64,
    pub egress_per_gb_usd: Decimal,
}

/// A fully resolved price table after merging.
#[derive(Debug, Clone)]
pub struct ResolvedTable {
    /// The version of the embedded table.
    pub embedded_version: String,
    /// The version of the override file, if any.
    pub override_version: Option<String>,
    /// Resolved entries keyed by `(provider, region, class)`.
    pub entries: BTreeMap<(String, String, String), ResolvedPriceSchedule>,
}

impl ResolvedTable {
    /// Look up a resolved price schedule.
    pub fn lookup(
        &self,
        provider: &str,
        region: &str,
        class: &str,
    ) -> Option<&ResolvedPriceSchedule> {
        self.entries
            .get(&(provider.to_string(), region.to_string(), class.to_string()))
    }
}

/// Load and validate a pricing override file.
///
/// # Errors
///
/// Returns `CrabError::InvalidConfig` if the file is malformed.
/// Emits warnings for unknown fields (serde ignores them by default
/// with `#[serde(deny_unknown_fields)]` not set).
pub fn load_override(path: &Path) -> Result<OverrideFile> {
    check_permissions(path);

    let content = std::fs::read_to_string(path).map_err(|e| CrabError::Configuration {
        key: format!(
            "failed to read pricing override file {}: {e}",
            path.display()
        ),
        origin: "cost.pricing_file".to_string(),
    })?;

    let override_file: OverrideFile =
        serde_yaml::from_str(&content).map_err(|e| CrabError::Configuration {
            key: format!(
                "failed to parse pricing override file {}: {e}",
                path.display()
            ),
            origin: "cost.pricing_file".to_string(),
        })?;

    // Warn about unknown provider names.
    let known_providers = ["aws", "gcs", "azure"];
    for provider in override_file.providers.keys() {
        if !known_providers.contains(&provider.as_str()) {
            warn!(
                provider = provider.as_str(),
                "unknown provider in pricing override file — will be ignored"
            );
        }
    }

    info!(
        path = %path.display(),
        version = ?override_file.version,
        providers = override_file.providers.len(),
        "loaded pricing override file"
    );

    Ok(override_file)
}

/// Check file permissions on unix. Warns if not `0600`.
fn check_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if let Ok(metadata) = std::fs::metadata(path) {
            let mode = metadata.mode() & 0o777;
            if mode != 0o600 {
                warn!(
                    path = %path.display(),
                    mode = format!("{mode:04o}"),
                    "pricing override file has loose permissions (expected 0600)"
                );
            }
        }
    }

    #[cfg(not(unix))]
    {
        info!(
            path = %path.display(),
            "pricing override file permission check skipped on this platform \
             (ACL enforcement is not implemented for Windows)"
        );
    }
}

/// Deep-merge an override onto the embedded price table.
///
/// For each `(provider, region, class)` in the embedded table, if the
/// override specifies a value for a field, it replaces the embedded
/// value. Missing fields inherit from the embedded table.
pub fn merge_tables(
    embedded: &[(String, String, String, ResolvedPriceSchedule)],
    override_file: &OverrideFile,
) -> ResolvedTable {
    let mut entries = BTreeMap::new();

    for (provider, region, class, schedule) in embedded {
        let mut resolved = schedule.clone();

        // Check if override has this (provider, region, class).
        if let Some(prov_override) = override_file.providers.get(provider) {
            if let Some(region_override) = prov_override.regions.get(region) {
                if let Some(class_override) = region_override.classes.get(class) {
                    apply_override(&mut resolved, class_override);
                }
            }
        }

        entries.insert((provider.clone(), region.clone(), class.clone()), resolved);
    }

    ResolvedTable {
        embedded_version: String::new(),
        override_version: override_file.version.clone(),
        entries,
    }
}

/// Apply a class override onto a resolved schedule.
fn apply_override(schedule: &mut ResolvedPriceSchedule, over: &ClassOverride) {
    if let Some(v) = over.gb_month_usd {
        schedule.gb_month_usd = v;
    }
    if let Some(v) = over.put_per_k_ops_usd {
        schedule.put_per_k_ops_usd = v;
    }
    if let Some(v) = over.get_per_k_ops_usd {
        schedule.get_per_k_ops_usd = v;
    }
    if let Some(v) = over.list_per_k_ops_usd {
        schedule.list_per_k_ops_usd = v;
    }
    if let Some(v) = over.head_per_k_ops_usd {
        schedule.head_per_k_ops_usd = v;
    }
    if let Some(v) = over.retrieval_per_gb_usd {
        schedule.retrieval_per_gb_usd = v;
    }
    if let Some(v) = over.min_retention_days {
        schedule.min_retention_days = v;
    }
    if let Some(v) = over.min_object_size_bytes {
        schedule.min_object_size_bytes = v;
    }
    if let Some(v) = over.egress_per_gb_usd {
        schedule.egress_per_gb_usd = v;
    }
}

/// Build a `ResolvedTable` from the embedded price table alone (no overrides).
pub fn from_embedded_only(
    entries: &[(String, String, String, ResolvedPriceSchedule)],
    version: &str,
) -> ResolvedTable {
    let mut map = BTreeMap::new();
    for (provider, region, class, schedule) in entries {
        map.insert(
            (provider.clone(), region.clone(), class.clone()),
            schedule.clone(),
        );
    }
    ResolvedTable {
        embedded_version: version.to_string(),
        override_version: None,
        entries: map,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn dec(s: &str) -> Decimal {
        Decimal::from_str(s).expect("valid decimal")
    }

    fn make_schedule() -> ResolvedPriceSchedule {
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
        }
    }

    #[test]
    fn override_replaces_specified_fields() {
        let mut schedule = make_schedule();
        let over = ClassOverride {
            gb_month_usd: Some(dec("0.015")),
            retrieval_per_gb_usd: Some(dec("0.01")),
            ..Default::default()
        };

        apply_override(&mut schedule, &over);

        assert_eq!(schedule.gb_month_usd, dec("0.015"));
        assert_eq!(schedule.retrieval_per_gb_usd, dec("0.01"));
        // Unspecified fields unchanged.
        assert_eq!(schedule.put_per_k_ops_usd, dec("0.005"));
        assert_eq!(schedule.egress_per_gb_usd, dec("0.09"));
    }

    #[test]
    fn override_empty_changes_nothing() {
        let mut schedule = make_schedule();
        let original = schedule.clone();
        let over = ClassOverride::default();

        apply_override(&mut schedule, &over);

        assert_eq!(schedule.gb_month_usd, original.gb_month_usd);
        assert_eq!(schedule.put_per_k_ops_usd, original.put_per_k_ops_usd);
    }

    #[test]
    fn merge_tables_applies_override() {
        let embedded = vec![(
            "aws".to_string(),
            "us-east-1".to_string(),
            "Standard".to_string(),
            make_schedule(),
        )];

        let yaml = r#"
version: "custom-2026"
providers:
  aws:
    regions:
      us-east-1:
        classes:
          Standard:
            gb_month_usd: "0.018"
"#;
        let override_file: OverrideFile = serde_yaml::from_str(yaml).expect("parse");
        let resolved = merge_tables(&embedded, &override_file);

        let entry = resolved
            .lookup("aws", "us-east-1", "Standard")
            .expect("lookup");
        assert_eq!(entry.gb_month_usd, dec("0.018"));
        // Unoverridden field preserved.
        assert_eq!(entry.put_per_k_ops_usd, dec("0.005"));
    }

    #[test]
    fn merge_tables_missing_region_inherits() {
        let embedded = vec![(
            "aws".to_string(),
            "eu-west-1".to_string(),
            "Standard".to_string(),
            make_schedule(),
        )];

        let yaml = r#"
providers:
  aws:
    regions:
      us-east-1:
        classes:
          Standard:
            gb_month_usd: "0.018"
"#;
        let override_file: OverrideFile = serde_yaml::from_str(yaml).expect("parse");
        let resolved = merge_tables(&embedded, &override_file);

        // eu-west-1 not in override, so it inherits embedded values.
        let entry = resolved
            .lookup("aws", "eu-west-1", "Standard")
            .expect("lookup");
        assert_eq!(entry.gb_month_usd, dec("0.023"));
    }

    #[test]
    fn from_embedded_only_preserves_all() {
        let embedded = vec![
            (
                "aws".to_string(),
                "us-east-1".to_string(),
                "Standard".to_string(),
                make_schedule(),
            ),
            (
                "gcs".to_string(),
                "us-central1".to_string(),
                "Standard".to_string(),
                make_schedule(),
            ),
        ];

        let resolved = from_embedded_only(&embedded, "2026-03-01");
        assert_eq!(resolved.embedded_version, "2026-03-01");
        assert_eq!(resolved.entries.len(), 2);
    }

    #[test]
    fn resolved_table_lookup_missing_returns_none() {
        let resolved = from_embedded_only(&[], "2026-03-01");
        assert!(resolved.lookup("aws", "us-east-1", "Standard").is_none());
    }

    #[test]
    fn decimal_math_no_drift() {
        // Verify that Decimal arithmetic doesn't introduce floating-point drift.
        let a = dec("0.023");
        let b = dec("100_000");
        let result = a * b;
        assert_eq!(result, dec("2300.000"));

        // Presentation rounding.
        let display_2 = format!("{:.2}", result);
        assert_eq!(display_2, "2300.00");

        let display_6 = format!("{:.6}", result);
        assert_eq!(display_6, "2300.000000");
    }

    #[test]
    fn version_stamping_in_resolved_table() {
        let embedded = vec![(
            "aws".to_string(),
            "us-east-1".to_string(),
            "Standard".to_string(),
            make_schedule(),
        )];

        let yaml = r#"
version: "contract-2026-Q2"
providers: {}
"#;
        let override_file: OverrideFile = serde_yaml::from_str(yaml).expect("parse");
        let resolved = merge_tables(&embedded, &override_file);

        assert_eq!(
            resolved.override_version,
            Some("contract-2026-Q2".to_string())
        );
    }
}
