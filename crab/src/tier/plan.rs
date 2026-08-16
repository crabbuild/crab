//! Plan builder for lifecycle tiering rules.
//!
//! [`build`] is a **pure** function — it takes a [`TierConfig`] and a
//! [`BucketProbe`] (pre-fetched by the caller) and returns a
//! [`TierPlan`] without making any network calls. The caller is
//! responsible for probing the bucket and passing the results in.
//!
//! The only tier-eligible prefix in V1 is `.crab/xorbs/`. Rules are
//! deterministic: given the same config and probe, the output is
//! byte-identical.

use crate::core::config::TierConfig;
use crate::core::error::Result;

use super::classes::StorageClass;
use super::provider::{Provider, TierPlan, TierRule, Transition};

/// The only tier-eligible prefix in V1.
const XORBS_PREFIX: &str = ".crab/xorbs/";

/// Results of probing a bucket before plan generation.
///
/// The caller fetches this information via provider APIs and passes it
/// into [`build`]. Keeping the probe separate from the plan builder
/// means the builder is pure and trivially testable.
#[derive(Debug, Clone)]
pub struct BucketProbe {
    /// Cloud provider hosting the bucket.
    pub provider: Provider,
    /// Whether bucket versioning is enabled.
    pub versioning_enabled: bool,
    /// Whether object-lock is enabled on the bucket.
    pub object_lock_enabled: bool,
    /// IDs of existing lifecycle rules already on the bucket.
    pub existing_rule_ids: Vec<String>,
}

/// Build a [`TierPlan`] from configuration and a pre-fetched bucket probe.
///
/// This function is pure — no network calls. It:
///
/// 1. Uses the probe to determine provider, versioning, and object-lock state.
/// 2. Targets the `.crab/xorbs/` prefix (the only tier-eligible prefix in V1).
/// 3. Computes transition rules from `cfg.to_ia_days` and `cfg.to_deep_days`.
/// 4. Applies per-class `min_object_size_bytes` clamps via [`StorageClass::min_object_size_bytes`].
/// 5. Sets `noncurrent_expiration_days` when versioning is enabled.
/// 6. Returns a [`TierPlan`] with deterministic `crab-`-prefixed rule IDs.
pub fn build(cfg: &TierConfig, probe: &BucketProbe) -> Result<TierPlan> {
    let mut rules = Vec::new();

    // Determine the provider-appropriate storage classes for IA and deep-cold.
    let ia_class = default_ia_class(probe.provider);
    let deep_class = default_deep_class(probe.provider);

    // IA transition rule.
    if cfg.to_ia_days > 0 {
        let min_size = ia_class.min_object_size_bytes();
        let noncurrent_expiration_days = if probe.versioning_enabled {
            Some(cfg.noncurrent_days)
        } else {
            None
        };

        rules.push(TierRule {
            id: format!("crab-xorbs-to-{}", class_slug(ia_class)),
            prefix: XORBS_PREFIX.to_string(),
            transitions: vec![Transition {
                days: cfg.to_ia_days,
                to_class: ia_class,
            }],
            noncurrent_expiration_days,
            min_object_size_bytes: if min_size > 0 { Some(min_size) } else { None },
        });
    }

    // Deep-cold transition rule.
    if cfg.to_deep_days > 0 {
        let min_size = deep_class.min_object_size_bytes();
        let noncurrent_expiration_days = if probe.versioning_enabled {
            Some(cfg.noncurrent_days)
        } else {
            None
        };

        rules.push(TierRule {
            id: format!("crab-xorbs-to-{}", class_slug(deep_class)),
            prefix: XORBS_PREFIX.to_string(),
            transitions: vec![Transition {
                days: cfg.to_deep_days,
                to_class: deep_class,
            }],
            noncurrent_expiration_days,
            min_object_size_bytes: if min_size > 0 { Some(min_size) } else { None },
        });
    }

    Ok(TierPlan {
        provider: probe.provider,
        rules,
        versioning_enabled: probe.versioning_enabled,
        object_lock_enabled: probe.object_lock_enabled,
    })
}

/// Map a provider to its default warm-cold (IA-equivalent) storage class.
fn default_ia_class(provider: Provider) -> StorageClass {
    match provider {
        Provider::S3 => StorageClass::S3StandardIa,
        Provider::Gcs => StorageClass::GcsNearline,
        Provider::Azure => StorageClass::AzureCool,
    }
}

/// Map a provider to its default deep-cold storage class.
fn default_deep_class(provider: Provider) -> StorageClass {
    match provider {
        Provider::S3 => StorageClass::S3GlacierFlexibleRetrieval,
        Provider::Gcs => StorageClass::GcsArchive,
        Provider::Azure => StorageClass::AzureArchive,
    }
}

/// Short slug for a storage class, used in rule IDs.
///
/// Rule IDs follow the pattern `crab-xorbs-to-<slug>`.
fn class_slug(class: StorageClass) -> &'static str {
    match class {
        StorageClass::S3StandardIa => "ia",
        StorageClass::S3OneZoneIa => "onezone-ia",
        StorageClass::S3IntelligentTiering => "intelligent-tiering",
        StorageClass::S3GlacierInstantRetrieval => "glacier-ir",
        StorageClass::S3GlacierFlexibleRetrieval => "glacier",
        StorageClass::S3GlacierDeepArchive => "deep-archive",
        StorageClass::GcsNearline => "nearline",
        StorageClass::GcsColdline => "coldline",
        StorageClass::GcsArchive => "archive",
        StorageClass::AzureCool => "cool",
        StorageClass::AzureCold => "cold",
        StorageClass::AzureArchive => "archive",
        // Standard / hot / unknown classes shouldn't appear as
        // transition targets, but provide a slug defensively.
        StorageClass::S3Standard => "standard",
        StorageClass::GcsStandard => "standard",
        StorageClass::AzureHot => "hot",
        StorageClass::Unknown => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: default S3 probe with versioning off.
    fn s3_probe() -> BucketProbe {
        BucketProbe {
            provider: Provider::S3,
            versioning_enabled: false,
            object_lock_enabled: false,
            existing_rule_ids: Vec::new(),
        }
    }

    /// Helper: default GCS probe.
    fn gcs_probe() -> BucketProbe {
        BucketProbe {
            provider: Provider::Gcs,
            versioning_enabled: false,
            object_lock_enabled: false,
            existing_rule_ids: Vec::new(),
        }
    }

    /// Helper: default Azure probe.
    fn azure_probe() -> BucketProbe {
        BucketProbe {
            provider: Provider::Azure,
            versioning_enabled: false,
            object_lock_enabled: false,
            existing_rule_ids: Vec::new(),
        }
    }

    // ── Default config produces expected rules ──────────────────────

    #[test]
    fn default_config_produces_ia_and_deep_rules() {
        let cfg = TierConfig::default();
        let plan = build(&cfg, &s3_probe()).unwrap();

        assert_eq!(plan.provider, Provider::S3);
        assert_eq!(plan.rules.len(), 2);

        // IA rule at 30 days.
        let ia = &plan.rules[0];
        assert_eq!(ia.id, "crab-xorbs-to-ia");
        assert_eq!(ia.prefix, ".crab/xorbs/");
        assert_eq!(ia.transitions.len(), 1);
        assert_eq!(ia.transitions[0].days, 30);
        assert_eq!(ia.transitions[0].to_class, StorageClass::S3StandardIa);
        assert!(ia.noncurrent_expiration_days.is_none());

        // Deep rule at 180 days.
        let deep = &plan.rules[1];
        assert_eq!(deep.id, "crab-xorbs-to-glacier");
        assert_eq!(deep.prefix, ".crab/xorbs/");
        assert_eq!(deep.transitions.len(), 1);
        assert_eq!(deep.transitions[0].days, 180);
        assert_eq!(
            deep.transitions[0].to_class,
            StorageClass::S3GlacierFlexibleRetrieval
        );
        assert!(deep.noncurrent_expiration_days.is_none());
    }

    // ── Versioning adds noncurrent expiration ───────────────────────

    #[test]
    fn versioning_enabled_adds_noncurrent_expiration() {
        let cfg = TierConfig::default();
        let mut probe = s3_probe();
        probe.versioning_enabled = true;

        let plan = build(&cfg, &probe).unwrap();

        assert!(plan.versioning_enabled);
        for rule in &plan.rules {
            assert_eq!(
                rule.noncurrent_expiration_days,
                Some(cfg.noncurrent_days),
                "rule {} should have noncurrent_expiration_days",
                rule.id
            );
        }
    }

    // ── S3 Glacier transitions get min_object_size_bytes = 40960 ────

    #[test]
    fn s3_glacier_transition_gets_min_object_size() {
        let cfg = TierConfig::default();
        let plan = build(&cfg, &s3_probe()).unwrap();

        let deep = plan
            .rules
            .iter()
            .find(|r| r.id == "crab-xorbs-to-glacier")
            .expect("should have glacier rule");

        assert_eq!(deep.min_object_size_bytes, Some(40_960));
    }

    // ── S3 IA transitions get min_object_size_bytes = 128000 ────────

    #[test]
    fn s3_ia_transition_gets_min_object_size() {
        let cfg = TierConfig::default();
        let plan = build(&cfg, &s3_probe()).unwrap();

        let ia = plan
            .rules
            .iter()
            .find(|r| r.id == "crab-xorbs-to-ia")
            .expect("should have IA rule");

        assert_eq!(ia.min_object_size_bytes, Some(128_000));
    }

    // ── GCS transitions get no min_object_size_bytes ────────────────

    #[test]
    fn gcs_transitions_have_no_min_object_size() {
        let cfg = TierConfig::default();
        let plan = build(&cfg, &gcs_probe()).unwrap();

        for rule in &plan.rules {
            assert_eq!(
                rule.min_object_size_bytes, None,
                "GCS rule {} should have no min_object_size_bytes",
                rule.id
            );
        }
    }

    // ── Azure transitions get no min_object_size_bytes ──────────────

    #[test]
    fn azure_transitions_have_no_min_object_size() {
        let cfg = TierConfig::default();
        let plan = build(&cfg, &azure_probe()).unwrap();

        for rule in &plan.rules {
            assert_eq!(
                rule.min_object_size_bytes, None,
                "Azure rule {} should have no min_object_size_bytes",
                rule.id
            );
        }
    }

    // ── Custom config values are respected ──────────────────────────

    #[test]
    fn custom_config_values_respected() {
        let cfg = TierConfig {
            to_ia_days: 60,
            to_deep_days: 365,
            noncurrent_days: 90,
            ..TierConfig::default()
        };
        let mut probe = s3_probe();
        probe.versioning_enabled = true;

        let plan = build(&cfg, &probe).unwrap();

        assert_eq!(plan.rules.len(), 2);

        let ia = &plan.rules[0];
        assert_eq!(ia.transitions[0].days, 60);
        assert_eq!(ia.noncurrent_expiration_days, Some(90));

        let deep = &plan.rules[1];
        assert_eq!(deep.transitions[0].days, 365);
        assert_eq!(deep.noncurrent_expiration_days, Some(90));
    }

    // ── Zero days disables the transition ───────────────────────────

    #[test]
    fn zero_ia_days_skips_ia_rule() {
        let cfg = TierConfig {
            to_ia_days: 0,
            ..TierConfig::default()
        };
        let plan = build(&cfg, &s3_probe()).unwrap();

        assert_eq!(plan.rules.len(), 1);
        assert!(plan.rules[0].id.contains("glacier"));
    }

    #[test]
    fn zero_deep_days_skips_deep_rule() {
        let cfg = TierConfig {
            to_deep_days: 0,
            ..TierConfig::default()
        };
        let plan = build(&cfg, &s3_probe()).unwrap();

        assert_eq!(plan.rules.len(), 1);
        assert!(plan.rules[0].id.contains("ia"));
    }

    #[test]
    fn both_zero_produces_empty_rules() {
        let cfg = TierConfig {
            to_ia_days: 0,
            to_deep_days: 0,
            ..TierConfig::default()
        };
        let plan = build(&cfg, &s3_probe()).unwrap();

        assert!(plan.rules.is_empty());
    }

    // ── Rule IDs are deterministic and prefixed ─────────────────────

    #[test]
    fn all_rule_ids_prefixed_crab() {
        let cfg = TierConfig::default();
        for probe in [s3_probe(), gcs_probe(), azure_probe()] {
            let plan = build(&cfg, &probe).unwrap();
            for rule in &plan.rules {
                assert!(
                    rule.id.starts_with("crab-"),
                    "rule ID '{}' should start with 'crab-'",
                    rule.id
                );
            }
        }
    }

    // ── GCS default classes ─────────────────────────────────────────

    #[test]
    fn gcs_default_classes() {
        let cfg = TierConfig::default();
        let plan = build(&cfg, &gcs_probe()).unwrap();

        let ia = &plan.rules[0];
        assert_eq!(ia.transitions[0].to_class, StorageClass::GcsNearline);

        let deep = &plan.rules[1];
        assert_eq!(deep.transitions[0].to_class, StorageClass::GcsArchive);
    }

    // ── Azure default classes ───────────────────────────────────────

    #[test]
    fn azure_default_classes() {
        let cfg = TierConfig::default();
        let plan = build(&cfg, &azure_probe()).unwrap();

        let ia = &plan.rules[0];
        assert_eq!(ia.transitions[0].to_class, StorageClass::AzureCool);

        let deep = &plan.rules[1];
        assert_eq!(deep.transitions[0].to_class, StorageClass::AzureArchive);
    }

    // ── Object-lock flag is passed through ──────────────────────────

    #[test]
    fn object_lock_flag_passed_through() {
        let cfg = TierConfig::default();
        let mut probe = s3_probe();
        probe.object_lock_enabled = true;

        let plan = build(&cfg, &probe).unwrap();

        assert!(plan.object_lock_enabled);
    }

    // ── All rules target the xorbs prefix ───────────────────────────

    #[test]
    fn all_rules_target_xorbs_prefix() {
        let cfg = TierConfig::default();
        for probe in [s3_probe(), gcs_probe(), azure_probe()] {
            let plan = build(&cfg, &probe).unwrap();
            for rule in &plan.rules {
                assert_eq!(
                    rule.prefix, ".crab/xorbs/",
                    "rule {} should target .crab/xorbs/",
                    rule.id
                );
            }
        }
    }
}
