//! Rule-conflict detection and `--merge` logic for lifecycle rules.
//!
//! [`detect_conflicts`] compares an existing lifecycle configuration
//! against a proposed new one and returns any conflicts. V1 operates
//! at the rule-ID level: two rules conflict when they share the same
//! ID but have different bodies.
//!
//! [`merge`] replaces Crab-managed rules while preserving every
//! user-managed rule. It never silently drops a rule it cannot classify.

use crate::core::error::{CrabError, Result};

use super::provider::{Format, RenderedLifecycle};

/// A conflict between an existing lifecycle rule and a proposed new one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    /// ID of the existing rule that conflicts.
    pub existing_id: String,
    /// ID of the new rule that conflicts.
    pub new_id: String,
    /// Human-readable description of why the rules conflict.
    pub reason: String,
}

/// Detect conflicts between an existing lifecycle configuration and a
/// proposed new one.
///
/// Returns an empty `Vec` when there is no existing configuration or
/// when the existing and new configurations are compatible.
///
/// V1 conflict detection operates at the rule-ID level:
/// - Same ID with different body → conflict.
/// - Disjoint IDs → no conflict.
///
/// This function is pure — no network calls.
pub fn detect_conflicts(
    existing: Option<&RenderedLifecycle>,
    new: &RenderedLifecycle,
) -> Vec<Conflict> {
    let Some(existing) = existing else {
        return Vec::new();
    };

    let mut conflicts = Vec::new();

    for new_id in &new.rule_ids {
        if existing.rule_ids.contains(new_id) {
            // Same ID exists in both — check whether the bodies differ.
            // In V1 we compare the full serialised bodies. If the
            // bodies are byte-identical the rules are the same
            // (idempotent re-apply), so no conflict.
            if existing.body != new.body {
                conflicts.push(Conflict {
                    existing_id: new_id.clone(),
                    new_id: new_id.clone(),
                    reason: format!("rule '{new_id}' exists with a different body"),
                });
            }
        }
    }

    conflicts
}

/// Merge a new lifecycle configuration into an existing one.
///
/// Replaces Crab-managed rules with the rules from `new` and preserves every
/// user-managed rule. S3 uses the XML rule ID namespace, Azure uses the rule
/// name namespace, and GCS uses the exact `.crab/xorbs/` prefix because its
/// wire format has no rule IDs. Never silently drops an unclassified rule.
///
/// The merged result uses the same format as `new`.
pub fn merge(existing: &RenderedLifecycle, new: &RenderedLifecycle) -> Result<RenderedLifecycle> {
    if existing.format != new.format {
        return Err(CrabError::Internal(format!(
            "cannot merge lifecycle documents with different formats: existing={:?}, new={:?}",
            existing.format, new.format
        )));
    }

    match new.format {
        Format::Xml => merge_s3_xml(existing, new),
        Format::Json => merge_json(existing, new),
    }
}

/// Merge the JSON lifecycle shapes used by GCS and Azure.
///
/// GCS has no wire-level rule ID, so its exact `.crab/xorbs/` prefix is the
/// managed namespace. Azure carries a rule name and reserves the `crab-`
/// prefix. Any malformed or ambiguous document fails closed before a write;
/// silently treating an unknown rule as managed could delete user policy.
fn merge_json(existing: &RenderedLifecycle, new: &RenderedLifecycle) -> Result<RenderedLifecycle> {
    let existing_value: serde_json::Value =
        serde_json::from_slice(&existing.body).map_err(|e| CrabError::CorruptObject {
            path: "tier/lifecycle/existing".to_owned(),
            reason: format!("existing lifecycle is not valid JSON: {e}"),
        })?;
    let new_value: serde_json::Value =
        serde_json::from_slice(&new.body).map_err(|e| CrabError::Configuration {
            key: "tier.lifecycle".to_owned(),
            origin: format!("intended lifecycle is not valid JSON: {e}"),
        })?;

    if let (Some(existing_lifecycle), Some(new_lifecycle)) =
        (existing_value.get("lifecycle"), new_value.get("lifecycle"))
    {
        let existing_rules = json_rule_array(existing_lifecycle, "lifecycle.rule", true)?;
        let new_rules = json_rule_array(new_lifecycle, "lifecycle.rule", false)?;
        let mut merged_rules = Vec::with_capacity(existing_rules.len() + new_rules.len());
        let mut user_ids = Vec::new();
        for (index, rule) in existing_rules.iter().enumerate() {
            if !gcs_rule_is_managed(rule)? {
                merged_rules.push(rule.clone());
                let id = existing
                    .rule_ids
                    .get(index)
                    .ok_or_else(|| CrabError::CorruptObject {
                        path: "tier/lifecycle/existing".to_owned(),
                        reason: "GCS lifecycle rule IDs do not match rule count".to_owned(),
                    })?;
                user_ids.push(id.clone());
            }
        }
        merged_rules.extend(new_rules.iter().cloned());

        let body = serde_json::to_vec_pretty(&serde_json::json!({
            "lifecycle": { "rule": merged_rules },
        }))
        .map_err(|e| {
            CrabError::Internal(format!("GCS lifecycle merge serialization failed: {e}"))
        })?;
        let mut rule_ids = user_ids;
        rule_ids.extend(new.rule_ids.iter().cloned());
        return Ok(RenderedLifecycle {
            format: Format::Json,
            body,
            rule_ids,
        });
    }

    if let (Some(existing_rules), Some(new_rules)) =
        (existing_value.get("rules"), new_value.get("rules"))
    {
        let existing_rules = existing_rules
            .as_array()
            .ok_or_else(|| CrabError::CorruptObject {
                path: "tier/lifecycle/existing".to_owned(),
                reason: "Azure lifecycle rules is not an array".to_owned(),
            })?;
        let new_rules = new_rules
            .as_array()
            .ok_or_else(|| CrabError::Configuration {
                key: "tier.lifecycle".to_owned(),
                origin: "Azure lifecycle rules is not an array".to_owned(),
            })?;
        let mut merged_rules = Vec::with_capacity(existing_rules.len() + new_rules.len());
        let mut user_ids = Vec::new();
        for rule in existing_rules {
            let name = rule
                .get("name")
                .and_then(serde_json::Value::as_str)
                .filter(|name| !name.is_empty())
                .ok_or_else(|| CrabError::CorruptObject {
                    path: "tier/lifecycle/existing".to_owned(),
                    reason: "Azure lifecycle rule has no non-empty name".to_owned(),
                })?;
            if !is_crab_managed(name) {
                merged_rules.push(rule.clone());
                user_ids.push(name.to_owned());
            }
        }
        for rule in new_rules {
            let name = rule
                .get("name")
                .and_then(serde_json::Value::as_str)
                .filter(|name| !name.is_empty())
                .ok_or_else(|| CrabError::Configuration {
                    key: "tier.lifecycle".to_owned(),
                    origin: "Azure lifecycle rule has no non-empty name".to_owned(),
                })?;
            if !is_crab_managed(name) {
                return Err(CrabError::Configuration {
                    key: "tier.lifecycle".to_owned(),
                    origin: format!(
                        "intended Azure lifecycle rule {name:?} is outside Crab's namespace"
                    ),
                });
            }
            merged_rules.push(rule.clone());
        }

        let body = serde_json::to_vec_pretty(&serde_json::json!({ "rules": merged_rules }))
            .map_err(|e| {
                CrabError::Internal(format!("Azure lifecycle merge serialization failed: {e}"))
            })?;
        let mut rule_ids = user_ids;
        rule_ids.extend(new.rule_ids.iter().cloned());
        return Ok(RenderedLifecycle {
            format: Format::Json,
            body,
            rule_ids,
        });
    }

    Err(CrabError::CorruptObject {
        path: "tier/lifecycle/existing".to_owned(),
        reason: "JSON lifecycle documents use neither GCS lifecycle.rule nor Azure rules"
            .to_owned(),
    })
}

fn json_rule_array<'a>(
    lifecycle: &'a serde_json::Value,
    field: &str,
    existing: bool,
) -> Result<&'a [serde_json::Value]> {
    lifecycle
        .get("rule")
        .and_then(serde_json::Value::as_array)
        .map(Vec::as_slice)
        .ok_or_else(|| {
            let error = format!("GCS lifecycle {field} is not an array");
            if existing {
                CrabError::CorruptObject {
                    path: "tier/lifecycle/existing".to_owned(),
                    reason: error,
                }
            } else {
                CrabError::Configuration {
                    key: "tier.lifecycle".to_owned(),
                    origin: error,
                }
            }
        })
}

fn gcs_rule_is_managed(rule: &serde_json::Value) -> Result<bool> {
    let Some(prefixes) = rule
        .get("condition")
        .and_then(|condition| condition.get("matchesPrefix"))
        .and_then(serde_json::Value::as_array)
    else {
        return Ok(false);
    };
    if prefixes.is_empty() {
        return Ok(false);
    }
    if prefixes
        .iter()
        .all(|prefix| prefix.as_str() == Some(".crab/xorbs/"))
    {
        return Ok(true);
    }
    if prefixes.iter().any(|prefix| prefix.as_str().is_none()) {
        return Err(CrabError::CorruptObject {
            path: "tier/lifecycle/existing".to_owned(),
            reason: "GCS lifecycle matchesPrefix contains a non-string value".to_owned(),
        });
    }
    Ok(false)
}

/// Check whether a rule ID is managed by crab.
pub fn is_crab_managed(id: &str) -> bool {
    id.starts_with("crab-")
}

#[cfg(feature = "tier-s3")]
fn merge_s3_xml(
    existing: &RenderedLifecycle,
    new: &RenderedLifecycle,
) -> Result<RenderedLifecycle> {
    let existing_rules = crate::tier::provider::s3::parse_xml_to_sdk_rules(&existing.body)?;
    let mut merged_rules = Vec::new();
    let mut merged_ids = Vec::new();

    for rule in existing_rules {
        let id = rule.id().unwrap_or_default().to_owned();
        if !is_crab_managed(&id) {
            merged_ids.push(id);
            merged_rules.push(rule);
        }
    }

    let new_rules = crate::tier::provider::s3::parse_xml_to_sdk_rules(&new.body)?;
    for rule in new_rules {
        if let Some(id) = rule.id() {
            merged_ids.push(id.to_owned());
        }
        merged_rules.push(rule);
    }

    let body = crate::tier::provider::s3::serialize_lifecycle_rules_to_xml(&merged_rules)?;

    Ok(RenderedLifecycle {
        format: Format::Xml,
        body,
        rule_ids: merged_ids,
    })
}

#[cfg(not(feature = "tier-s3"))]
fn merge_s3_xml(
    _existing: &RenderedLifecycle,
    _new: &RenderedLifecycle,
) -> Result<RenderedLifecycle> {
    Err(CrabError::TierProviderUnsupported {
        provider: "s3 lifecycle merge requires the tier-s3 feature".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tier::provider::Format;

    // ── Helpers ─────────────────────────────────────────────────────

    /// Build a `RenderedLifecycle` with the given rule IDs and body.
    fn rendered(rule_ids: &[&str], body: &[u8]) -> RenderedLifecycle {
        RenderedLifecycle {
            format: Format::Json,
            body: body.to_vec(),
            rule_ids: rule_ids.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// Build a `RenderedLifecycle` with rule IDs and a body derived
    /// from the IDs (so different IDs produce different bodies).
    fn rendered_from_ids(rule_ids: &[&str]) -> RenderedLifecycle {
        let body = rule_ids.join(",").into_bytes();
        rendered(rule_ids, &body)
    }

    fn xml_rendered(rule_ids: &[&str]) -> RenderedLifecycle {
        let mut body =
            String::from(r#"<?xml version="1.0" encoding="UTF-8"?><LifecycleConfiguration>"#);
        for id in rule_ids {
            body.push_str("<Rule><ID>");
            body.push_str(id);
            body.push_str("</ID><Status>Enabled</Status></Rule>");
        }
        body.push_str("</LifecycleConfiguration>");
        RenderedLifecycle {
            format: Format::Xml,
            body: body.into_bytes(),
            rule_ids: rule_ids.iter().map(|s| s.to_string()).collect(),
        }
    }

    // ── detect_conflicts: no existing rules → no conflicts ──────────

    #[test]
    fn no_existing_rules_produces_no_conflicts() {
        let new = rendered_from_ids(&["crab-xorbs-to-ia"]);
        let conflicts = detect_conflicts(None, &new);
        assert!(conflicts.is_empty());
    }

    // ── detect_conflicts: same crab rules → no conflicts ──────────

    #[test]
    fn existing_same_crab_rules_no_conflicts() {
        let body = b"identical-body";
        let existing = rendered(&["crab-xorbs-to-ia", "crab-xorbs-to-glacier"], body);
        let new = rendered(&["crab-xorbs-to-ia", "crab-xorbs-to-glacier"], body);

        let conflicts = detect_conflicts(Some(&existing), &new);
        assert!(
            conflicts.is_empty(),
            "idempotent re-apply should produce no conflicts"
        );
    }

    // ── detect_conflicts: different crab rules → conflict ─────────

    #[test]
    fn existing_different_crab_rules_produces_conflict() {
        let existing = rendered(&["crab-xorbs-to-ia"], b"old-body-30-days");
        let new = rendered(&["crab-xorbs-to-ia"], b"new-body-60-days");

        let conflicts = detect_conflicts(Some(&existing), &new);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].existing_id, "crab-xorbs-to-ia");
        assert_eq!(conflicts[0].new_id, "crab-xorbs-to-ia");
        assert!(conflicts[0].reason.contains("different body"));
    }

    // ── detect_conflicts: user rules + new crab rules → no conflict

    #[test]
    fn existing_user_rules_with_new_crab_rules_no_conflict() {
        let existing = rendered(
            &["my-custom-cleanup", "team-archive-policy"],
            b"user-rules-body",
        );
        let new = rendered(
            &["crab-xorbs-to-ia", "crab-xorbs-to-glacier"],
            b"crab-rules-body",
        );

        let conflicts = detect_conflicts(Some(&existing), &new);
        assert!(
            conflicts.is_empty(),
            "disjoint IDs (user vs crab) should not conflict"
        );
    }

    // ── detect_conflicts: multiple overlapping IDs ──────────────────

    #[test]
    fn multiple_overlapping_ids_produce_multiple_conflicts() {
        let existing = rendered(
            &["crab-xorbs-to-ia", "crab-xorbs-to-glacier"],
            b"old-config",
        );
        let new = rendered(
            &["crab-xorbs-to-ia", "crab-xorbs-to-glacier"],
            b"new-config",
        );

        let conflicts = detect_conflicts(Some(&existing), &new);
        assert_eq!(conflicts.len(), 2);

        let ids: Vec<&str> = conflicts.iter().map(|c| c.existing_id.as_str()).collect();
        assert!(ids.contains(&"crab-xorbs-to-ia"));
        assert!(ids.contains(&"crab-xorbs-to-glacier"));
    }

    // ── detect_conflicts: partial overlap ───────────────────────────

    #[test]
    fn partial_overlap_only_conflicts_on_shared_ids() {
        let existing = rendered(&["crab-xorbs-to-ia", "user-cleanup"], b"existing-body");
        let new = rendered(&["crab-xorbs-to-ia", "crab-xorbs-to-glacier"], b"new-body");

        let conflicts = detect_conflicts(Some(&existing), &new);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].existing_id, "crab-xorbs-to-ia");
    }

    // ── detect_conflicts: empty existing rule_ids ───────────────────

    #[test]
    fn empty_existing_rule_ids_no_conflicts() {
        let existing = rendered(&[], b"empty");
        let new = rendered_from_ids(&["crab-xorbs-to-ia"]);

        let conflicts = detect_conflicts(Some(&existing), &new);
        assert!(conflicts.is_empty());
    }

    // ── merge: user rules preserved, crab rules replaced ──────────

    #[test]
    fn merge_preserves_user_rules_replaces_crab() {
        let existing = xml_rendered(&["user-cleanup", "crab-xorbs-to-ia", "team-archive"]);
        let new = xml_rendered(&["crab-xorbs-to-ia", "crab-xorbs-to-glacier"]);

        let merged = merge(&existing, &new).unwrap();

        // User rules are preserved.
        assert!(
            merged.rule_ids.contains(&"user-cleanup".to_string()),
            "user-cleanup should be preserved"
        );
        assert!(
            merged.rule_ids.contains(&"team-archive".to_string()),
            "team-archive should be preserved"
        );

        // New crab rules are present.
        assert!(
            merged.rule_ids.contains(&"crab-xorbs-to-ia".to_string()),
            "crab-xorbs-to-ia should be in merged set"
        );
        assert!(
            merged
                .rule_ids
                .contains(&"crab-xorbs-to-glacier".to_string()),
            "crab-xorbs-to-glacier should be in merged set"
        );

        // Old crab-only rules are NOT separately preserved (they
        // come from the new set).
        let crab_count = merged
            .rule_ids
            .iter()
            .filter(|id| id.starts_with("crab-"))
            .count();
        assert_eq!(crab_count, 2, "should have exactly the 2 new crab rules");
    }

    // ── merge: user rules with overlapping prefix preserved ─────────

    #[test]
    fn merge_user_rules_with_overlapping_prefix_preserved() {
        // User has a rule targeting the same prefix as crab rules.
        // Merge must never drop it.
        let existing = xml_rendered(&["user-xorbs-expire-365d", "crab-xorbs-to-ia"]);
        let new = xml_rendered(&["crab-xorbs-to-ia", "crab-xorbs-to-glacier"]);

        let merged = merge(&existing, &new).unwrap();

        assert!(
            merged
                .rule_ids
                .contains(&"user-xorbs-expire-365d".to_string()),
            "user rule with overlapping prefix must be preserved"
        );
    }

    // ── merge: no existing crab rules ─────────────────────────────

    #[test]
    fn merge_no_existing_crab_rules() {
        let existing = xml_rendered(&["user-cleanup", "team-archive"]);
        let new = xml_rendered(&["crab-xorbs-to-ia"]);

        let merged = merge(&existing, &new).unwrap();

        assert_eq!(merged.rule_ids.len(), 3);
        assert!(merged.rule_ids.contains(&"user-cleanup".to_string()));
        assert!(merged.rule_ids.contains(&"team-archive".to_string()));
        assert!(merged.rule_ids.contains(&"crab-xorbs-to-ia".to_string()));
    }

    // ── merge: all existing rules are crab-managed ────────────────

    #[test]
    fn merge_all_existing_are_crab_managed() {
        let existing = xml_rendered(&["crab-xorbs-to-ia", "crab-xorbs-to-glacier"]);
        let new = xml_rendered(&["crab-xorbs-to-ia"]);

        let merged = merge(&existing, &new).unwrap();

        // Only the new crab rules should remain.
        assert_eq!(merged.rule_ids.len(), 1);
        assert_eq!(merged.rule_ids[0], "crab-xorbs-to-ia");
    }

    // ── merge: empty existing ───────────────────────────────────────

    #[test]
    fn merge_empty_existing() {
        let existing = xml_rendered(&[]);
        let new = xml_rendered(&["crab-xorbs-to-ia"]);

        let merged = merge(&existing, &new).unwrap();

        assert_eq!(merged.rule_ids.len(), 1);
        assert_eq!(merged.rule_ids[0], "crab-xorbs-to-ia");
    }

    #[test]
    fn merge_gcs_json_preserves_user_rules_and_replaces_xorb_rules() {
        let existing_body = serde_json::json!({
            "lifecycle": { "rule": [
                {
                    "action": { "type": "Delete" },
                    "condition": { "age": 7, "matchesPrefix": ["backups/"] }
                },
                {
                    "action": { "type": "SetStorageClass", "storageClass": "NEARLINE" },
                    "condition": { "age": 30, "matchesPrefix": [".crab/xorbs/"] }
                }
            ] }
        });
        let new_body = serde_json::json!({
            "lifecycle": { "rule": [
                {
                    "action": { "type": "SetStorageClass", "storageClass": "ARCHIVE" },
                    "condition": { "age": 365, "matchesPrefix": [".crab/xorbs/"] }
                }
            ] }
        });
        let existing = RenderedLifecycle {
            format: Format::Json,
            body: serde_json::to_vec(&existing_body).unwrap(),
            rule_ids: vec!["gcs-user-rule".into(), "crab-gcs-old".into()],
        };
        let new = RenderedLifecycle {
            format: Format::Json,
            body: serde_json::to_vec(&new_body).unwrap(),
            rule_ids: vec!["crab-xorbs-to-archive".into()],
        };

        let merged = merge(&existing, &new).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&merged.body).unwrap();
        let rules = value["lifecycle"]["rule"].as_array().unwrap();
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0]["condition"]["matchesPrefix"][0], "backups/");
        assert_eq!(rules[1]["action"]["storageClass"], "ARCHIVE");
        assert_eq!(
            merged.rule_ids,
            vec!["gcs-user-rule", "crab-xorbs-to-archive"]
        );
    }

    #[test]
    fn merge_azure_json_preserves_named_user_rules_and_replaces_crab_rules() {
        let existing = rendered(
            &["user-cleanup", "crab-xorbs-to-cool"],
            br#"{"rules":[
                {"enabled":true,"name":"user-cleanup","type":"Lifecycle","definition":{}},
                {"enabled":true,"name":"crab-xorbs-to-cool","type":"Lifecycle","definition":{}}
            ]}"#,
        );
        let new = rendered(
            &["crab-xorbs-to-archive"],
            br#"{"rules":[
                {"enabled":true,"name":"crab-xorbs-to-archive","type":"Lifecycle","definition":{}}
            ]}"#,
        );

        let merged = merge(&existing, &new).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&merged.body).unwrap();
        let rules = value["rules"].as_array().unwrap();
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0]["name"], "user-cleanup");
        assert_eq!(rules[1]["name"], "crab-xorbs-to-archive");
        assert_eq!(
            merged.rule_ids,
            vec!["user-cleanup", "crab-xorbs-to-archive"]
        );
    }

    #[test]
    fn merge_json_rejects_ambiguous_shape() {
        let existing = rendered(&["user"], br#"{"rules":{}}"#);
        let new = rendered(&["crab-rule"], br#"{"rules":[]}"#);
        let error = merge(&existing, &new).unwrap_err();
        assert!(matches!(error, CrabError::CorruptObject { .. }));
    }

    // ── is_crab_managed ───────────────────────────────────────────

    #[test]
    fn crab_prefixed_ids_are_managed() {
        assert!(is_crab_managed("crab-xorbs-to-ia"));
        assert!(is_crab_managed("crab-xorbs-to-glacier"));
        assert!(is_crab_managed("crab-"));
    }

    #[test]
    fn non_crab_ids_are_not_managed() {
        assert!(!is_crab_managed("user-cleanup"));
        assert!(!is_crab_managed("team-archive-policy"));
        assert!(!is_crab_managed("my-crab-rule"));
        assert!(!is_crab_managed(""));
    }
}
