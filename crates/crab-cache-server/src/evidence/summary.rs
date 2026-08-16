use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;

use super::{
    EXPECTED_IMMUTABLE_ROUTE_PATTERNS, EXPECTED_MUTABLE_ROUTE_PATTERNS, EvidenceVerificationStatus,
    find_named_record, float_field, int_field, normalize_path, read_json_file, string_array_field,
    verify_evidence_report,
};

#[derive(Debug, Serialize)]
pub struct EvidenceSummary {
    pub status: EvidenceVerificationStatus,
    pub report: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub verified_checks: usize,
    pub failed_checks: Vec<String>,
    pub cache: EvidenceCacheSummary,
    pub hydrates: Vec<EvidenceHydrateSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dedup: Option<EvidenceDedupSummary>,
    pub enterprise: EvidenceEnterpriseSummary,
    pub routes: EvidenceRouteContractSummary,
    pub artifacts: Vec<EvidenceArtifactSummary>,
}

#[derive(Debug, Default, Serialize)]
pub struct EvidenceCacheSummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_hit_rate: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_hit_total: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin_avoided_reads_total: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin_fetch_total: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin_fallback_rate: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct EvidenceHydrateSummary {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin_gets_delta: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin_fetches_delta: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_hits_delta: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_misses_delta: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin_avoided_reads_delta: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct EvidenceDedupSummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dedup_queries_delta: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dedup_known_chunks_delta: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cacheable_origin_gets_delta: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mutable_origin_gets_delta: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub xorb_puts_delta: Option<i64>,
}

#[derive(Debug, Default, Serialize)]
pub struct EvidenceEnterpriseSummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preflight_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mutable_path_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_object_bytes: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub policy_rule_count: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authz_read: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authz_write: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authz_dedup: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authz_admin: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct EvidenceRouteContractSummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capabilities_status: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route_schema: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub route_transport_prefix: Option<String>,
    pub expected_immutable_route_count: usize,
    pub expected_mutable_route_count: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub immutable_route_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mutable_route_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retired_route_count: Option<usize>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub retired_routes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mutable_read_probe_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mutable_read_probe_unique_patterns: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mutable_write_probe_count: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mutable_write_probe_unique_patterns: Option<usize>,
}

#[derive(Debug, Serialize)]
pub struct EvidenceArtifactSummary {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
}

#[must_use]
pub fn summarize_evidence_report(report_path: &Path) -> EvidenceSummary {
    let verification = verify_evidence_report(report_path);
    let failed_checks = verification
        .failed_check_names()
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let report_path = normalize_path(report_path);
    let report = read_json_file(&report_path).ok();
    let manifest = report.as_ref().and_then(|report| {
        read_report_artifact(&report_path, report, "cache_service_evidence_manifest")
    });
    let preflight = report.as_ref().and_then(|report| {
        read_report_artifact(&report_path, report, "cache_server_preflight_json")
    });
    let support = report
        .as_ref()
        .and_then(|report| find_named_record(report, "support_bundles", "post-traffic"));

    EvidenceSummary {
        status: verification.status,
        report: verification.report,
        run_id: verification.run_id,
        verified_checks: verification.verified_checks,
        failed_checks,
        cache: cache_summary(support),
        hydrates: hydrate_summaries(report.as_ref()),
        dedup: dedup_summary(report.as_ref()),
        enterprise: enterprise_summary(preflight.as_ref(), support),
        routes: route_contract_summary(report.as_ref()),
        artifacts: artifact_summaries(manifest.as_ref()),
    }
}

fn read_report_artifact(report_path: &Path, report: &Value, key: &str) -> Option<Value> {
    artifact_path_from_report(report_path, report, key).and_then(|path| read_json_file(&path).ok())
}

fn artifact_path_from_report(report_path: &Path, report: &Value, key: &str) -> Option<PathBuf> {
    let path = PathBuf::from(
        report
            .get("artifacts")
            .and_then(Value::as_object)?
            .get(key)?
            .as_str()?,
    );
    if path.is_absolute() {
        return None;
    }
    let path = report_path
        .parent()
        .map_or(path.clone(), |parent| parent.join(path));
    Some(normalize_path(&path))
}

fn cache_summary(support: Option<&Value>) -> EvidenceCacheSummary {
    let Some(support) = support else {
        return EvidenceCacheSummary::default();
    };
    EvidenceCacheSummary {
        cache_hit_rate: float_field(support, "cache_hit_rate"),
        cache_hit_total: float_field(support, "cache_hit_total"),
        origin_avoided_reads_total: float_field(support, "origin_avoided_reads_total"),
        origin_fetch_total: float_field(support, "origin_fetch_total"),
        origin_fallback_rate: float_field(support, "origin_fallback_rate"),
    }
}

fn hydrate_summaries(report: Option<&Value>) -> Vec<EvidenceHydrateSummary> {
    report
        .and_then(|report| report.get("cli_hydrates"))
        .and_then(Value::as_array)
        .map_or_else(Vec::new, |records| {
            records
                .iter()
                .filter_map(|record| {
                    let name = record.get("name").and_then(Value::as_str)?;
                    Some(EvidenceHydrateSummary {
                        name: name.to_string(),
                        origin_gets_delta: match (
                            int_field(record, "origin_gets_after"),
                            int_field(record, "origin_gets_before"),
                        ) {
                            (Some(after), Some(before)) => Some(after - before),
                            _ => None,
                        },
                        origin_fetches_delta: int_field(record, "origin_fetches_delta"),
                        cache_hits_delta: int_field(record, "cache_hits_delta"),
                        cache_misses_delta: int_field(record, "cache_misses_delta"),
                        origin_avoided_reads_delta: int_field(record, "origin_avoided_reads_delta"),
                    })
                })
                .collect()
        })
}

fn dedup_summary(report: Option<&Value>) -> Option<EvidenceDedupSummary> {
    let record =
        report.and_then(|report| find_named_record(report, "cli_push_dedup", "cli-dedup-push"))?;
    Some(EvidenceDedupSummary {
        dedup_queries_delta: int_field(record, "dedup_queries_delta"),
        dedup_known_chunks_delta: int_field(record, "dedup_known_chunks_delta"),
        cacheable_origin_gets_delta: int_field(record, "cacheable_origin_gets_delta"),
        mutable_origin_gets_delta: int_field(record, "mutable_origin_gets_delta"),
        xorb_puts_delta: int_field(record, "xorb_puts_delta"),
    })
}

fn enterprise_summary(
    preflight: Option<&Value>,
    support: Option<&Value>,
) -> EvidenceEnterpriseSummary {
    let summary = preflight
        .and_then(|preflight| preflight.get("summary"))
        .and_then(Value::as_object);
    let policy_diagnostics = summary.and_then(|summary| summary.get("policy_diagnostics"));

    EvidenceEnterpriseSummary {
        preflight_status: preflight
            .and_then(|preflight| preflight.get("status"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        policy: summary
            .and_then(|summary| summary.get("policy"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        mutable_path_mode: summary
            .and_then(|summary| summary.get("mutable_path_mode"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        max_object_bytes: summary
            .and_then(|summary| summary.get("max_object_bytes"))
            .and_then(Value::as_i64),
        policy_rule_count: policy_diagnostics
            .and_then(|diagnostics| diagnostics.get("rule_count"))
            .and_then(Value::as_i64),
        authz_read: bool_field(support, "authz_read"),
        authz_write: bool_field(support, "authz_write"),
        authz_dedup: bool_field(support, "authz_dedup"),
        authz_admin: bool_field(support, "authz_admin"),
    }
}

fn route_contract_summary(report: Option<&Value>) -> EvidenceRouteContractSummary {
    let capabilities = report
        .and_then(|report| find_named_record(report, "capabilities", "cache-service-capabilities"));
    let immutable_routes = capabilities
        .and_then(|capabilities| string_array_field(capabilities, "immutable_route_patterns"));
    let mutable_routes = capabilities
        .and_then(|capabilities| string_array_field(capabilities, "mutable_route_patterns"));
    let has_route_lists = immutable_routes.is_some() && mutable_routes.is_some();
    let retired_routes = immutable_routes
        .iter()
        .flatten()
        .chain(mutable_routes.iter().flatten())
        .filter(|pattern| is_retired_route_pattern(pattern.as_str()))
        .cloned()
        .collect::<Vec<_>>();

    EvidenceRouteContractSummary {
        capabilities_status: capabilities
            .and_then(|capabilities| int_field(capabilities, "status")),
        route_schema: capabilities
            .and_then(|capabilities| capabilities.get("route_schema"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        route_transport_prefix: capabilities
            .and_then(|capabilities| capabilities.get("route_transport_prefix"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        expected_immutable_route_count: EXPECTED_IMMUTABLE_ROUTE_PATTERNS.len(),
        expected_mutable_route_count: EXPECTED_MUTABLE_ROUTE_PATTERNS.len(),
        immutable_route_count: immutable_routes.as_ref().map(Vec::len),
        mutable_route_count: mutable_routes.as_ref().map(Vec::len),
        retired_route_count: has_route_lists.then_some(retired_routes.len()),
        retired_routes,
        mutable_read_probe_count: route_probe_count(report, "mutable_route_behaviors"),
        mutable_read_probe_unique_patterns: route_probe_unique_pattern_count(
            report,
            "mutable_route_behaviors",
        ),
        mutable_write_probe_count: route_probe_count(report, "mutable_route_write_behaviors"),
        mutable_write_probe_unique_patterns: route_probe_unique_pattern_count(
            report,
            "mutable_route_write_behaviors",
        ),
    }
}

fn route_probe_count(report: Option<&Value>, field: &str) -> Option<usize> {
    report
        .and_then(|report| report.get(field))
        .and_then(Value::as_array)
        .map(Vec::len)
}

fn route_probe_unique_pattern_count(report: Option<&Value>, field: &str) -> Option<usize> {
    let records = report
        .and_then(|report| report.get(field))
        .and_then(Value::as_array)?;
    Some(
        records
            .iter()
            .filter_map(|record| record.get("pattern").and_then(Value::as_str))
            .collect::<BTreeSet<_>>()
            .len(),
    )
}

fn is_retired_route_pattern(pattern: &str) -> bool {
    pattern.contains("xet/") || pattern == ".crab/file-index/*"
}

fn artifact_summaries(manifest: Option<&Value>) -> Vec<EvidenceArtifactSummary> {
    let mut artifacts = manifest
        .and_then(|manifest| manifest.get("artifacts"))
        .and_then(Value::as_object)
        .map_or_else(Vec::new, |records| {
            records
                .iter()
                .filter_map(|(name, record)| {
                    let record = record.as_object()?;
                    Some(EvidenceArtifactSummary {
                        name: name.to_string(),
                        sha256: record
                            .get("sha256")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        bytes: record.get("bytes").and_then(Value::as_u64),
                    })
                })
                .collect::<Vec<_>>()
        });
    artifacts.sort_by(|left, right| left.name.cmp(&right.name));
    artifacts
}

fn bool_field(record: Option<&Value>, key: &str) -> Option<bool> {
    record
        .and_then(|record| record.get(key))
        .and_then(Value::as_bool)
}
