//! Verification for retained cache-service evidence bundles.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

mod summary;

pub use summary::{
    EvidenceArtifactSummary, EvidenceCacheSummary, EvidenceDedupSummary, EvidenceEnterpriseSummary,
    EvidenceHydrateSummary, EvidenceRouteContractSummary, EvidenceSummary,
    summarize_evidence_report,
};

const EVIDENCE_MANIFEST_SCHEMA: &str = "crab-cache-service.evidence-manifest.v1";
const DEFAULT_PSK: &str = "cache-smoke-psk";
const DEFAULT_PSK_BLAKE3: &str = "4fb898757c4c93662343bbbb25419f8c4f9c979352d40ff896578cabf620cf6e";
const EXPECTED_ROUTE_SCHEMA: &str = "crab-cache-service.routes.v3";
const EXPECTED_IMMUTABLE_ROUTE_PATTERNS: &[&str] = &[
    ".crab/xorbs/{first-two-hex}/{hash}",
    ".crab/shards/{first-two-hex}/{hash}",
    "{repo}/packs/pack-{id}.pack",
    "{repo}/packs/pack-{id}.idx",
    "{repo}/file_index_db/compacted/*.sst",
    "{repo}/file_index_db/manifest/*.manifest",
    "{repo}/file_index_db/wal/*.sst",
    "{repo}/file_index_db/compactions/*.compactions",
    ".crab/chunk_index_db/compacted/*.sst",
    ".crab/chunk_index_db/manifest/*.manifest",
    ".crab/chunk_index_db/wal/*.sst",
    ".crab/chunk_index_db/compactions/*.compactions",
];
const EXPECTED_MUTABLE_ROUTE_PATTERNS: &[&str] = &[
    "{repo}/refs/heads/*",
    "{repo}/HEAD",
    "{repo}/locks/*",
    "{repo}/packs/pack-{id}.meta",
    "{repo}/manifests/*",
    "{repo}/pack-list",
    "{repo}/shard-list",
    ".crab/ref-registry/*",
    "{repo}/file_index_db/manifest/current",
    ".crab/chunk_index_db/manifest/current",
];

#[derive(Debug, Serialize)]
pub struct EvidenceVerificationReport {
    pub status: EvidenceVerificationStatus,
    pub report: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub verified_checks: usize,
    pub checks: Vec<EvidenceVerificationCheck>,
}

#[derive(Debug, Serialize)]
pub struct EvidenceReleaseVerification {
    pub status: EvidenceVerificationStatus,
    pub report: String,
    pub expected_run_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub verified_checks: usize,
    pub checks: Vec<EvidenceVerificationCheck>,
    pub verification: EvidenceVerificationReport,
}

#[derive(Debug, Serialize)]
pub struct EvidenceDoctorReport {
    pub status: EvidenceVerificationStatus,
    pub verification: String,
    pub categories: Vec<EvidenceDoctorCategory>,
}

#[derive(Debug, Serialize)]
pub struct EvidenceDoctorCategory {
    pub category: String,
    pub title: String,
    pub checks: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<String>,
    pub remediation: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceVerificationStatus {
    Passed,
    Failed,
}

#[derive(Debug, Serialize)]
pub struct EvidenceVerificationCheck {
    pub name: String,
    pub ok: bool,
    pub detail: Value,
}

impl EvidenceVerificationReport {
    #[must_use]
    pub fn failed_check_names(&self) -> Vec<&str> {
        self.checks
            .iter()
            .filter(|check| !check.ok)
            .map(|check| check.name.as_str())
            .collect()
    }
}

impl EvidenceReleaseVerification {
    #[must_use]
    pub fn failed_check_names(&self) -> Vec<&str> {
        self.checks
            .iter()
            .filter(|check| !check.ok)
            .map(|check| check.name.as_str())
            .chain(self.verification.failed_check_names())
            .collect()
    }
}

struct EvidenceVerifier {
    report_path: PathBuf,
    report: Value,
    checks: Vec<EvidenceVerificationCheck>,
}

pub fn verify_evidence_report(report_path: &Path) -> EvidenceVerificationReport {
    let report_path = normalize_path(report_path);
    match read_json_file(&report_path) {
        Ok(report) => {
            let mut verifier = EvidenceVerifier {
                report_path,
                report,
                checks: Vec::new(),
            };
            verifier.verify();
            verifier.finish()
        }
        Err(error) => EvidenceVerificationReport {
            status: EvidenceVerificationStatus::Failed,
            report: report_path.display().to_string(),
            run_id: None,
            verified_checks: 0,
            checks: vec![EvidenceVerificationCheck {
                name: "report-json-readable".to_string(),
                ok: false,
                detail: json!({ "error": error }),
            }],
        },
    }
}

#[must_use]
pub fn verify_release_evidence_report(
    report_path: &Path,
    expected_run_id: &str,
) -> EvidenceReleaseVerification {
    let verification = verify_evidence_report(report_path);
    let mut checks = Vec::new();
    let expected_run_id = expected_run_id.to_owned();
    checks.push(EvidenceVerificationCheck {
        name: "release-expected-run-id-present".to_string(),
        ok: !expected_run_id.trim().is_empty(),
        detail: json!({ "expected_run_id": &expected_run_id }),
    });
    checks.push(EvidenceVerificationCheck {
        name: "release-run-id-matches".to_string(),
        ok: verification.run_id.as_deref() == Some(expected_run_id.as_str()),
        detail: json!({
            "expected": &expected_run_id,
            "actual": &verification.run_id,
        }),
    });

    let ok = verification.status == EvidenceVerificationStatus::Passed
        && checks.iter().all(|check| check.ok);
    EvidenceReleaseVerification {
        status: if ok {
            EvidenceVerificationStatus::Passed
        } else {
            EvidenceVerificationStatus::Failed
        },
        report: verification.report.clone(),
        expected_run_id,
        run_id: verification.run_id.clone(),
        verified_checks: verification.verified_checks
            + checks.iter().filter(|check| check.ok).count(),
        checks,
        verification,
    }
}

pub fn doctor_evidence_verification(verification_path: &Path) -> EvidenceDoctorReport {
    let verification_path = normalize_path(verification_path);
    match read_json_file(&verification_path) {
        Ok(verification) => doctor_verification_json(&verification_path, &verification),
        Err(error) => EvidenceDoctorReport {
            status: EvidenceVerificationStatus::Failed,
            verification: verification_path.display().to_string(),
            categories: vec![doctor_category(
                "verification_json",
                vec!["verification-json-readable".to_string()],
                Vec::new(),
                Some(error),
            )],
        },
    }
}

pub fn find_release_evidence_report(evidence_dir: &Path) -> Result<PathBuf, String> {
    let evidence_dir = normalize_path(evidence_dir);
    if !evidence_dir.is_dir() {
        return Err(format!(
            "cache-service evidence directory does not exist: {}",
            evidence_dir.display()
        ));
    }

    let mut reports = Vec::new();
    collect_report_paths(&evidence_dir, &mut reports)?;
    reports.sort();
    match reports.as_slice() {
        [report] => Ok(report.clone()),
        [] => Err(format!(
            "cache-service evidence directory contains no report.json: {}",
            evidence_dir.display()
        )),
        _ => Err(format!(
            "cache-service evidence directory contains multiple report.json files: {}",
            reports
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

fn collect_report_paths(dir: &Path, reports: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries =
        fs::read_dir(dir).map_err(|error| format!("failed to read {}: {error}", dir.display()))?;
    for entry in entries {
        let entry =
            entry.map_err(|error| format!("failed to read entry in {}: {error}", dir.display()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|error| format!("failed to stat {}: {error}", path.display()))?;
        if file_type.is_dir() {
            collect_report_paths(&path, reports)?;
        } else if file_type.is_file()
            && path.file_name().and_then(|name| name.to_str()) == Some("report.json")
        {
            reports.push(normalize_path(&path));
        }
    }
    Ok(())
}

fn doctor_verification_json(path: &Path, verification: &Value) -> EvidenceDoctorReport {
    let mut failed_checks = failed_evidence_checks(verification);
    if verification.get("status").and_then(Value::as_str) != Some("passed")
        && failed_checks.is_empty()
    {
        failed_checks.push(FailedEvidenceCheck {
            name: "verification-status-failed".to_string(),
            detail: Value::Null,
        });
    }

    EvidenceDoctorReport {
        status: if failed_checks.is_empty() {
            EvidenceVerificationStatus::Passed
        } else {
            EvidenceVerificationStatus::Failed
        },
        verification: path.display().to_string(),
        categories: categorize_failed_checks(failed_checks),
    }
}

#[derive(Debug)]
struct FailedEvidenceCheck {
    name: String,
    detail: Value,
}

fn failed_evidence_checks(value: &Value) -> Vec<FailedEvidenceCheck> {
    let mut checks = Vec::new();
    collect_failed_checks(value, &mut checks);
    checks
}

fn collect_failed_checks(value: &Value, checks: &mut Vec<FailedEvidenceCheck>) {
    if let Some(records) = value.get("checks").and_then(Value::as_array) {
        for check in records {
            if check.get("ok").and_then(Value::as_bool) == Some(true) {
                continue;
            }
            if let Some(name) = check.get("name").and_then(Value::as_str) {
                checks.push(FailedEvidenceCheck {
                    name: name.to_string(),
                    detail: check.get("detail").cloned().unwrap_or(Value::Null),
                });
            }
        }
    }

    if let Some(verification) = value.get("verification") {
        collect_failed_checks(verification, checks);
    }
}

fn categorize_failed_checks(
    failed_checks: Vec<FailedEvidenceCheck>,
) -> Vec<EvidenceDoctorCategory> {
    let mut groups: Vec<(&'static str, Vec<FailedEvidenceCheck>)> = Vec::new();
    for check in failed_checks {
        let category = doctor_category_name(&check.name);
        if let Some((_, checks)) = groups
            .iter_mut()
            .find(|(existing, _)| *existing == category)
        {
            checks.push(check);
        } else {
            groups.push((category, vec![check]));
        }
    }
    groups
        .into_iter()
        .map(|(category, checks)| {
            let details = doctor_category_details(category, &checks);
            let check_names = checks
                .into_iter()
                .map(|check| check.name)
                .collect::<Vec<_>>();
            doctor_category(category, check_names, details, None)
        })
        .collect()
}

fn doctor_category_name(check: &str) -> &'static str {
    if check.starts_with("release-") {
        "release_run_binding"
    } else if check.contains("secret-free") {
        "secret_redaction"
    } else if check.starts_with("artifact-")
        || check.starts_with("evidence-manifest-")
        || check.starts_with("retained-")
        || matches!(
            check,
            "report-json-readable" | "report-status-passed" | "report-run-id-present"
        )
    {
        "evidence_artifact_integrity"
    } else if check.starts_with("cache-server-preflight-") {
        "enterprise_preflight"
    } else if check.starts_with("cli-cold-hydrate-") || check.starts_with("cli-warm-hydrate-") {
        "cache_hydrate_traffic"
    } else if check.starts_with("cli-dedup-") {
        "cache_dedup_traffic"
    } else if check.starts_with("route-contract-") {
        "route_contract"
    } else if check.starts_with("support-bundle-") {
        "support_bundle"
    } else if check.starts_with("origin-outage-") {
        "origin_outage_cache_resilience"
    } else {
        "unknown_evidence_failure"
    }
}

fn doctor_category(
    category: &'static str,
    checks: Vec<String>,
    details: Vec<String>,
    detail: Option<String>,
) -> EvidenceDoctorCategory {
    let (title, remediation) = doctor_category_copy(category);
    let remediation = match detail {
        Some(detail) => format!("{remediation} Detail: {detail}"),
        None => remediation.to_string(),
    };
    EvidenceDoctorCategory {
        category: category.to_string(),
        title: title.to_string(),
        checks,
        details,
        remediation,
    }
}

fn doctor_category_details(category: &str, checks: &[FailedEvidenceCheck]) -> Vec<String> {
    match category {
        "route_contract" => route_contract_doctor_details(checks),
        _ => Vec::new(),
    }
}

fn route_contract_doctor_details(checks: &[FailedEvidenceCheck]) -> Vec<String> {
    let mut details = Vec::new();
    for check in checks {
        match check.name.as_str() {
            "route-contract-immutable-patterns" => push_route_pattern_details(
                &mut details,
                "immutable routes",
                "immutable",
                &check.detail,
            ),
            "route-contract-mutable-patterns" => {
                push_route_pattern_details(&mut details, "mutable routes", "mutable", &check.detail)
            }
            "route-contract-mutable-behavior-count" => {
                push_route_probe_count_detail(&mut details, "mutable read probes", &check.detail);
            }
            "route-contract-mutable-write-behavior-count" => {
                push_route_probe_count_detail(&mut details, "mutable write probes", &check.detail);
            }
            "route-contract-mutable-behavior-patterns" => push_route_pattern_details(
                &mut details,
                "mutable read probe patterns",
                "mutable read probe",
                &check.detail,
            ),
            "route-contract-mutable-write-behavior-patterns" => push_route_pattern_details(
                &mut details,
                "mutable write probe patterns",
                "mutable write probe",
                &check.detail,
            ),
            "route-contract-no-retired-routes" => {
                if let Some(retired) = string_array_value(check.detail.get("retired")) {
                    push_unique_detail(
                        &mut details,
                        format!("retired routes: {}", joined_or_none(&retired)),
                    );
                }
            }
            "route-contract-capabilities-status" => push_unique_detail(
                &mut details,
                format!(
                    "capabilities status: expected 200, actual {}",
                    fmt_json_field(check.detail.get("status"))
                ),
            ),
            "route-contract-capabilities-schema" => push_unique_detail(
                &mut details,
                format!(
                    "capabilities schema: expected crab-cache-service.capabilities.v1, actual {}",
                    fmt_json_field(check.detail.get("schema"))
                ),
            ),
            "route-contract-route-schema" => push_unique_detail(
                &mut details,
                format!(
                    "route schema: expected {EXPECTED_ROUTE_SCHEMA}, actual {}",
                    fmt_json_field(check.detail.get("route_schema"))
                ),
            ),
            "route-contract-transport-prefix" => push_unique_detail(
                &mut details,
                format!(
                    "route transport prefix: expected /v1/, actual {}",
                    fmt_json_field(check.detail.get("route_transport_prefix"))
                ),
            ),
            _ => push_mutable_route_probe_detail(&mut details, check),
        }
    }
    details
}

fn push_route_pattern_details(
    details: &mut Vec<String>,
    count_label: &str,
    diff_label: &str,
    detail: &Value,
) {
    let expected = string_array_value(detail.get("expected"));
    let actual = string_array_value(detail.get("actual"));
    if let (Some(expected), Some(actual)) = (expected.as_ref(), actual.as_ref()) {
        push_unique_detail(
            details,
            format!(
                "{count_label}: expected {}, actual {}",
                expected.len(),
                actual.len()
            ),
        );
        let missing = missing_values(expected, actual);
        if !missing.is_empty() {
            push_unique_detail(
                details,
                format!("missing {diff_label} routes: {}", missing.join(", ")),
            );
        }
        let unexpected = missing_values(actual, expected);
        if !unexpected.is_empty() {
            push_unique_detail(
                details,
                format!("unexpected {diff_label} routes: {}", unexpected.join(", ")),
            );
        }
    }
}

fn push_route_probe_count_detail(details: &mut Vec<String>, label: &str, detail: &Value) {
    let expected = EXPECTED_MUTABLE_ROUTE_PATTERNS.len();
    push_unique_detail(
        details,
        format!(
            "{label}: records {}/{expected}, unique_patterns {}/{expected}",
            fmt_i64_detail(int_field(detail, "record_count")),
            fmt_i64_detail(int_field(detail, "unique_patterns")),
        ),
    );
}

fn push_mutable_route_probe_detail(details: &mut Vec<String>, check: &FailedEvidenceCheck) {
    if !(check.name.starts_with("route-contract-mutable-read-")
        || check.name.starts_with("route-contract-mutable-write-"))
    {
        return;
    }
    let route_id = check
        .detail
        .get("route_id")
        .and_then(Value::as_str)
        .unwrap_or("unknown-route");
    let pattern = check
        .detail
        .get("pattern")
        .and_then(Value::as_str)
        .unwrap_or("unknown-pattern");
    push_unique_detail(
        details,
        format!("failing mutable route probe: {route_id} ({pattern})"),
    );
}

fn missing_values(left: &[String], right: &[String]) -> Vec<String> {
    let right = right.iter().map(String::as_str).collect::<BTreeSet<_>>();
    left.iter()
        .filter(|value| !right.contains(value.as_str()))
        .cloned()
        .collect()
}

fn string_array_value(value: Option<&Value>) -> Option<Vec<String>> {
    value?
        .as_array()?
        .iter()
        .map(|value| value.as_str().map(str::to_owned))
        .collect()
}

fn joined_or_none(values: &[String]) -> String {
    if values.is_empty() {
        "none".to_string()
    } else {
        values.join(", ")
    }
}

fn fmt_json_field(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(value)) => value.clone(),
        Some(Value::Null) | None => "missing".to_string(),
        Some(value) => value.to_string(),
    }
}

fn fmt_i64_detail(value: Option<i64>) -> String {
    value.map_or_else(|| "missing".to_string(), |value| value.to_string())
}

fn push_unique_detail(details: &mut Vec<String>, detail: String) {
    if !details.iter().any(|existing| existing == &detail) {
        details.push(detail);
    }
}

fn doctor_category_copy(category: &str) -> (&'static str, &'static str) {
    match category {
        "verification_json" => (
            "Verification JSON is unreadable",
            "Regenerate the release evidence verification JSON with `crab-cache-server evidence release-verify --output`.",
        ),
        "release_run_binding" => (
            "Release evidence belongs to a different run",
            "Pass the cache-service workflow run id and attempt that produced this artifact, or rerun the cache-service smoke workflow and release gate together.",
        ),
        "evidence_artifact_integrity" => (
            "Retained evidence artifact is incomplete or changed",
            "Redownload the cache-service smoke artifact for one workflow attempt and rerun release verification before trusting the evidence.",
        ),
        "secret_redaction" => (
            "Retained evidence leaked non-redacted sensitive material",
            "Regenerate the smoke evidence with redacted configs and policies, then delete the unsafe artifact.",
        ),
        "enterprise_preflight" => (
            "Enterprise cache-service posture check failed",
            "Fix the cache-server preflight failures, especially policy, mutable-path mode, auth, and trusted proxy posture, then rerun the smoke workflow.",
        ),
        "cache_hydrate_traffic" => (
            "Hydrate path reached origin storage through the cache server",
            "Inspect the CLI cache URL wiring and cache-server object cache metrics; warm hydrate should be served from cache without S3 object reads.",
        ),
        "cache_dedup_traffic" => (
            "Push dedup path reached origin storage for immutable data",
            "Inspect known-chunk lookup and immutable object caching; only the mutable manifest CAS read should reach object storage.",
        ),
        "route_contract" => (
            "Cache-service route contract evidence is stale or incomplete",
            "Rerun the RustFS smoke with a current crab-cache-server; capabilities.routes and mutable route rejection probes must match the local Crab route taxonomy.",
        ),
        "support_bundle" => (
            "Support bundle proof is missing cache effectiveness metrics",
            "Collect a fresh post-traffic support bundle after the smoke workflow and keep it with the release evidence artifact.",
        ),
        "origin_outage_cache_resilience" => (
            "Cache service did not prove cached reads survive origin outage",
            "Rerun the RustFS smoke and inspect cache-service hit/miss counters; warm immutable full and range reads must remain HIT while readiness reports the origin outage.",
        ),
        _ => (
            "Unclassified cache-service evidence failure",
            "Inspect the failed check details in the verification JSON and add a cache-service doctor category if this is a recurring operator action.",
        ),
    }
}

impl EvidenceVerifier {
    fn verify(&mut self) {
        self.verify_report_status();
        self.verify_evidence_manifest();
        self.verify_embedded_checks();
        self.verify_cache_server_preflight();
        self.verify_route_contract();
        self.verify_cli_hydrate_traffic();
        self.verify_cli_dedup_traffic();
        self.verify_support_bundle_summary();
        self.verify_origin_outage();
        self.verify_origin_outage_support_bundle();
    }

    fn finish(self) -> EvidenceVerificationReport {
        let ok = self.checks.iter().all(|check| check.ok);
        EvidenceVerificationReport {
            status: if ok {
                EvidenceVerificationStatus::Passed
            } else {
                EvidenceVerificationStatus::Failed
            },
            report: self.report_path.display().to_string(),
            run_id: self
                .report
                .get("run_id")
                .and_then(Value::as_str)
                .map(str::to_owned),
            verified_checks: self.checks.iter().filter(|check| check.ok).count(),
            checks: self.checks,
        }
    }

    fn check(&mut self, name: impl Into<String>, ok: bool, detail: Value) {
        self.checks.push(EvidenceVerificationCheck {
            name: name.into(),
            ok,
            detail,
        });
    }

    fn artifact_path(&mut self, key: &str) -> Option<PathBuf> {
        let artifacts = match self.report.get("artifacts").and_then(Value::as_object) {
            Some(artifacts) => artifacts,
            None => {
                self.check(
                    "artifacts-is-object",
                    false,
                    json!({ "type": value_type(self.report.get("artifacts")) }),
                );
                return None;
            }
        };
        let Some(value) = artifacts.get(key).and_then(Value::as_str) else {
            self.check(
                format!("artifact-{key}-present"),
                false,
                json!({ key: null }),
            );
            return None;
        };
        let path = PathBuf::from(value);
        self.check(
            format!("artifact-{key}-relative"),
            !path.is_absolute(),
            json!({ key: value }),
        );
        if path.is_absolute() {
            return Some(normalize_path(&path));
        }
        let path = self
            .report_path
            .parent()
            .map_or(path.clone(), |parent| parent.join(path));
        Some(normalize_path(&path))
    }

    fn verify_report_status(&mut self) {
        self.check(
            "report-status-passed",
            self.report.get("status").and_then(Value::as_str) == Some("passed"),
            json!({ "status": self.report.get("status") }),
        );
        self.check(
            "report-run-id-present",
            self.report.get("run_id").and_then(Value::as_str).is_some(),
            json!({ "run_id": self.report.get("run_id") }),
        );
    }

    fn verify_evidence_manifest(&mut self) {
        let Some(manifest_path) = self.artifact_path("cache_service_evidence_manifest") else {
            return;
        };
        self.check(
            "evidence-manifest-artifact-exists",
            manifest_path.is_file(),
            json!({ "path": manifest_path }),
        );
        if !manifest_path.is_file() {
            return;
        }
        let manifest = match read_json_file(&manifest_path) {
            Ok(manifest) => manifest,
            Err(error) => {
                self.check(
                    "evidence-manifest-json-readable",
                    false,
                    json!({ "error": error }),
                );
                return;
            }
        };
        self.check(
            "evidence-manifest-schema",
            manifest.get("schema").and_then(Value::as_str) == Some(EVIDENCE_MANIFEST_SCHEMA),
            json!({ "schema": manifest.get("schema") }),
        );
        self.check(
            "evidence-manifest-run-id",
            manifest.get("run_id").and_then(Value::as_str)
                == self.report.get("run_id").and_then(Value::as_str),
            json!({
                "manifest": manifest.get("run_id"),
                "report": self.report.get("run_id"),
            }),
        );

        let Some(artifacts) = manifest
            .get("artifacts")
            .and_then(Value::as_object)
            .cloned()
        else {
            self.check(
                "evidence-manifest-artifacts-object",
                false,
                json!({ "type": value_type(manifest.get("artifacts")) }),
            );
            return;
        };
        self.check("evidence-manifest-artifacts-object", true, json!({}));

        let mut artifact_paths = vec![
            ("report", self.report_path.clone()),
            (
                "cache_server_preflight_json",
                match self.artifact_path("cache_server_preflight_json") {
                    Some(path) => path,
                    None => return,
                },
            ),
            (
                "rustfs_smoke_script",
                match self.artifact_path("rustfs_smoke_script") {
                    Some(path) => path,
                    None => return,
                },
            ),
            (
                "smoke_report_verifier",
                match self.artifact_path("smoke_report_verifier") {
                    Some(path) => path,
                    None => return,
                },
            ),
        ];
        for key in [
            "cache_server_config",
            "transparent_cache_server_config",
            "cache_server_policy",
        ] {
            if report_artifact_exists(&self.report, key)
                && let Some(path) = self.artifact_path(key)
            {
                artifact_paths.push((key, path));
            }
        }
        for (key, path) in artifact_paths {
            self.verify_evidence_file_record(&artifacts, key, &path);
        }

        self.verify_manifest_runtime(&manifest);
        self.verify_manifest_parameters(&manifest);
        self.verify_retained_config_artifacts();
    }

    fn verify_manifest_runtime(&mut self, manifest: &Value) {
        let Some(runtime) = manifest.get("runtime").and_then(Value::as_object) else {
            self.check(
                "evidence-manifest-runtime-object",
                false,
                json!({ "type": value_type(manifest.get("runtime")) }),
            );
            return;
        };
        self.check("evidence-manifest-runtime-object", true, json!({}));
        self.check(
            "evidence-manifest-crab-version-recorded",
            runtime
                .get("crab_version")
                .and_then(Value::as_str)
                .is_some_and(|version| version.starts_with("crab ")),
            json!({ "runtime": runtime }),
        );
        self.check(
            "evidence-manifest-cache-server-version-recorded",
            runtime
                .get("cache_server_version")
                .and_then(Value::as_str)
                .is_some_and(|version| version.starts_with("crab-cache-server ")),
            json!({ "runtime": runtime }),
        );
        self.check(
            "evidence-manifest-bucket-matches-report",
            runtime.get("rustfs_bucket").and_then(Value::as_str)
                == self.report.get("bucket").and_then(Value::as_str),
            json!({
                "runtime_bucket": runtime.get("rustfs_bucket"),
                "report_bucket": self.report.get("bucket"),
            }),
        );
    }

    fn verify_manifest_parameters(&mut self, manifest: &Value) {
        let Some(parameters) = manifest.get("parameters").and_then(Value::as_object) else {
            self.check(
                "evidence-manifest-parameters-object",
                false,
                json!({ "type": value_type(manifest.get("parameters")) }),
            );
            return;
        };
        self.check("evidence-manifest-parameters-object", true, json!({}));
        self.check(
            "evidence-manifest-dedup-scope",
            parameters.get("dedup_scope").and_then(Value::as_str) == Some("all"),
            json!({ "parameters": parameters }),
        );
        self.check(
            "evidence-manifest-strict-mutable-path-mode",
            parameters.get("mutable_path_mode").and_then(Value::as_str) == Some("strict"),
            json!({ "parameters": parameters }),
        );
    }

    fn verify_evidence_file_record(
        &mut self,
        artifacts: &Map<String, Value>,
        key: &str,
        path: &Path,
    ) {
        let Some(record) = artifacts.get(key).and_then(Value::as_object) else {
            self.check(
                format!("evidence-manifest-{key}-record"),
                false,
                json!({ "record": artifacts.get(key) }),
            );
            return;
        };
        self.check(format!("evidence-manifest-{key}-record"), true, json!({}));
        self.check(
            format!("evidence-manifest-{key}-file-exists"),
            path.is_file(),
            json!({ "path": path }),
        );
        if !path.is_file() {
            return;
        }
        let expected_path = self.artifact_reference(path);
        self.check(
            format!("evidence-manifest-{key}-path"),
            record.get("path").and_then(Value::as_str) == expected_path.as_deref(),
            json!({
                "expected": expected_path,
                "actual": record.get("path"),
            }),
        );
        let expected_sha = match sha256_file(path) {
            Ok(sha) => sha,
            Err(error) => {
                self.check(
                    format!("evidence-manifest-{key}-sha256"),
                    false,
                    json!({ "path": path, "error": error }),
                );
                return;
            }
        };
        self.check(
            format!("evidence-manifest-{key}-sha256"),
            record.get("sha256").and_then(Value::as_str) == Some(expected_sha.as_str()),
            json!({
                "path": path,
                "expected": expected_sha,
                "actual": record.get("sha256"),
            }),
        );
        let expected_bytes = path
            .metadata()
            .map_or(None, |metadata| Some(metadata.len()));
        self.check(
            format!("evidence-manifest-{key}-bytes"),
            record.get("bytes").and_then(Value::as_u64) == expected_bytes,
            json!({
                "path": path,
                "expected": expected_bytes,
                "actual": record.get("bytes"),
            }),
        );
    }

    fn artifact_reference(&self, path: &Path) -> Option<String> {
        let parent = self.report_path.parent()?;
        path.strip_prefix(parent).ok().map(path_to_slash)
    }

    fn verify_retained_config_artifacts(&mut self) {
        for key in [
            "cache_server_config",
            "transparent_cache_server_config",
            "cache_server_policy",
        ] {
            let Some(path) = self.artifact_path(key) else {
                continue;
            };
            self.check(
                format!("retained-{key}-artifact-exists"),
                path.is_file(),
                json!({ "path": path }),
            );
            if !path.is_file() {
                continue;
            }
            let text = match fs::read_to_string(&path) {
                Ok(text) => text,
                Err(error) => {
                    self.check(
                        format!("retained-{key}-readable"),
                        false,
                        json!({ "path": path, "error": error.to_string() }),
                    );
                    continue;
                }
            };
            let leaked = forbidden_literals_in(&text);
            self.check(
                format!("retained-{key}-secret-free"),
                leaked.is_empty(),
                json!({ "path": path, "leaked": leaked }),
            );
        }
    }

    fn verify_embedded_checks(&mut self) {
        let Some(checks) = self.report.get("checks").and_then(Value::as_array).cloned() else {
            self.check(
                "embedded-checks-present",
                false,
                json!({ "type": value_type(self.report.get("checks")) }),
            );
            return;
        };
        self.check(
            "embedded-checks-present",
            !checks.is_empty(),
            json!({ "count": checks.len() }),
        );
        let failed = checks
            .iter()
            .filter_map(|check| {
                if check.get("ok").and_then(Value::as_bool) == Some(true) {
                    None
                } else {
                    Some(
                        check
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or("<unnamed>")
                            .to_string(),
                    )
                }
            })
            .collect::<Vec<_>>();
        self.check(
            "embedded-checks-all-passed",
            failed.is_empty(),
            json!({ "failed": failed }),
        );
    }

    fn verify_cache_server_preflight(&mut self) {
        let Some(path) = self.artifact_path("cache_server_preflight_json") else {
            return;
        };
        self.check(
            "cache-server-preflight-artifact-exists",
            path.is_file(),
            json!({ "path": path }),
        );
        if !path.is_file() {
            return;
        }
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) => {
                self.check(
                    "cache-server-preflight-readable",
                    false,
                    json!({ "path": path, "error": error.to_string() }),
                );
                return;
            }
        };
        let payload = match serde_json::from_str::<Value>(&text) {
            Ok(payload) => payload,
            Err(error) => {
                self.check(
                    "cache-server-preflight-json-object",
                    false,
                    json!({ "error": error.to_string() }),
                );
                return;
            }
        };
        self.check(
            "cache-server-preflight-json-object",
            payload.is_object(),
            json!({ "type": type_name(&payload) }),
        );
        let Some(summary) = payload.get("summary").and_then(Value::as_object) else {
            self.check(
                "cache-server-preflight-summary-object",
                false,
                json!({ "type": value_type(payload.get("summary")) }),
            );
            return;
        };
        let Some(checks) = payload.get("checks").and_then(Value::as_array) else {
            self.check(
                "cache-server-preflight-checks-list",
                false,
                json!({ "type": value_type(payload.get("checks")) }),
            );
            return;
        };
        self.check("cache-server-preflight-summary-object", true, json!({}));
        self.check("cache-server-preflight-checks-list", true, json!({}));
        let issue_codes = checks
            .iter()
            .filter_map(|check| check.get("code").and_then(Value::as_str).map(str::to_owned))
            .collect::<Vec<_>>();
        let enterprise = checks
            .iter()
            .find(|check| check.get("name").and_then(Value::as_str) == Some("enterprise profile"));
        self.check(
            "cache-server-preflight-status",
            matches!(
                payload.get("status").and_then(Value::as_str),
                Some("ok" | "warn")
            ),
            json!({ "status": payload.get("status"), "codes": issue_codes }),
        );
        self.check(
            "cache-server-preflight-policy-configured",
            summary.get("policy").and_then(Value::as_str) == Some("configured"),
            json!({ "summary": summary }),
        );
        self.check(
            "cache-server-preflight-strict-mutable-paths",
            summary.get("mutable_path_mode").and_then(Value::as_str) == Some("strict"),
            json!({ "summary": summary }),
        );
        self.check(
            "cache-server-preflight-max-object-bytes",
            summary
                .get("max_object_bytes")
                .and_then(Value::as_i64)
                .is_some_and(|value| value > 0),
            json!({ "max_object_bytes": summary.get("max_object_bytes") }),
        );
        self.check(
            "cache-server-preflight-enterprise-ok",
            enterprise
                .and_then(|check| check.get("status"))
                .and_then(Value::as_str)
                == Some("ok"),
            json!({ "enterprise": enterprise }),
        );
        self.check(
            "cache-server-preflight-no-enterprise-codes",
            !checks.iter().any(|check| {
                check
                    .get("code")
                    .and_then(Value::as_str)
                    .is_some_and(|code| code.starts_with("enterprise_"))
            }),
            json!({ "codes": issue_codes }),
        );
        self.check(
            "cache-server-preflight-policy-diagnostics",
            summary.get("policy_diagnostics")
                == Some(&json!({
                    "rule_count": 1,
                    "repo_pattern_count": 2,
                    "actions": ["read", "write", "dedup", "admin"],
                })),
            json!({ "policy_diagnostics": summary.get("policy_diagnostics") }),
        );
        let leaked = forbidden_literals_in(&text);
        self.check(
            "cache-server-preflight-secret-free",
            leaked.is_empty(),
            json!({ "leaked": leaked }),
        );
    }

    fn verify_route_contract(&mut self) {
        let Some(record) =
            find_named_record(&self.report, "capabilities", "cache-service-capabilities").cloned()
        else {
            self.check(
                "route-contract-capabilities-record-present",
                false,
                json!({ "field": "capabilities" }),
            );
            return;
        };
        self.check(
            "route-contract-capabilities-status",
            int_field(&record, "status") == Some(200),
            json!({ "status": record.get("status") }),
        );
        self.check(
            "route-contract-capabilities-schema",
            record.get("schema").and_then(Value::as_str)
                == Some("crab-cache-service.capabilities.v1"),
            json!({ "schema": record.get("schema") }),
        );
        self.check(
            "route-contract-route-schema",
            record.get("route_schema").and_then(Value::as_str) == Some(EXPECTED_ROUTE_SCHEMA),
            json!({ "route_schema": record.get("route_schema") }),
        );
        self.check(
            "route-contract-transport-prefix",
            record.get("route_transport_prefix").and_then(Value::as_str) == Some("/v1/"),
            json!({ "route_transport_prefix": record.get("route_transport_prefix") }),
        );

        let expected_immutable = expected_patterns(EXPECTED_IMMUTABLE_ROUTE_PATTERNS);
        let expected_mutable = expected_patterns(EXPECTED_MUTABLE_ROUTE_PATTERNS);
        let immutable = string_array_field(&record, "immutable_route_patterns");
        let mutable = string_array_field(&record, "mutable_route_patterns");
        self.check(
            "route-contract-immutable-patterns",
            immutable.as_ref() == Some(&expected_immutable),
            json!({
                "expected": expected_immutable,
                "actual": immutable,
            }),
        );
        self.check(
            "route-contract-mutable-patterns",
            mutable.as_ref() == Some(&expected_mutable),
            json!({
                "expected": expected_mutable,
                "actual": mutable,
            }),
        );
        let advertised = string_array_field(&record, "immutable_route_patterns")
            .into_iter()
            .flatten()
            .chain(
                string_array_field(&record, "mutable_route_patterns")
                    .into_iter()
                    .flatten(),
            )
            .collect::<Vec<_>>();
        let retired = advertised
            .iter()
            .filter(|pattern| pattern.contains("xet/") || pattern.as_str() == ".crab/file-index/*")
            .cloned()
            .collect::<Vec<_>>();
        self.check(
            "route-contract-no-retired-routes",
            retired.is_empty(),
            json!({ "retired": retired }),
        );

        self.verify_mutable_route_behavior(
            "mutable_route_behaviors",
            "route-contract-mutable-behavior",
            false,
        );
        self.verify_mutable_route_behavior(
            "mutable_route_write_behaviors",
            "route-contract-mutable-write-behavior",
            true,
        );
    }

    fn verify_mutable_route_behavior(&mut self, field: &str, check_prefix: &str, writes: bool) {
        let Some((record_count, by_pattern)) = pattern_records(&self.report, field) else {
            self.check(
                format!("{check_prefix}-count"),
                false,
                json!({ "field": field, "type": value_type(self.report.get(field)) }),
            );
            return;
        };
        let expected = sorted_patterns(EXPECTED_MUTABLE_ROUTE_PATTERNS);
        let actual = sorted_owned(by_pattern.keys().cloned().collect());
        self.check(
            format!("{check_prefix}-count"),
            record_count == EXPECTED_MUTABLE_ROUTE_PATTERNS.len()
                && by_pattern.len() == EXPECTED_MUTABLE_ROUTE_PATTERNS.len(),
            json!({
                "record_count": record_count,
                "unique_patterns": by_pattern.len(),
            }),
        );
        self.check(
            format!("{check_prefix}-patterns"),
            actual == expected,
            json!({
                "expected": expected,
                "actual": actual,
            }),
        );

        for pattern in EXPECTED_MUTABLE_ROUTE_PATTERNS {
            let route_id = mutable_route_pattern_id(pattern);
            let Some(record) = by_pattern.get(*pattern) else {
                self.check(
                    format!("{check_prefix}-{route_id}-present"),
                    false,
                    json!({ "pattern": pattern }),
                );
                continue;
            };
            let name = if writes {
                format!("route-contract-mutable-write-{route_id}")
            } else {
                format!("route-contract-mutable-read-{route_id}")
            };
            self.check(
                format!("{name}-status"),
                int_field(record, "status") == Some(400),
                json!({ "route_id": route_id, "pattern": pattern, "record": record }),
            );
            self.check(
                format!("{name}-cache-status-empty"),
                record.get("cache_status").and_then(Value::as_str) == Some(""),
                json!({ "route_id": route_id, "pattern": pattern, "record": record }),
            );
            self.check(
                format!("{name}-origin-gets-flat"),
                int_fields_equal(record, "origin_gets_after", "origin_gets_before"),
                json!({ "route_id": route_id, "pattern": pattern, "record": record }),
            );
            if writes {
                self.check(
                    format!("{name}-origin-puts-flat"),
                    int_fields_equal(record, "origin_puts_after", "origin_puts_before"),
                    json!({ "route_id": route_id, "pattern": pattern, "record": record }),
                );
                self.check(
                    format!("{name}-total-origin-traffic-flat"),
                    int_fields_equal(
                        record,
                        "total_origin_gets_after",
                        "total_origin_gets_before",
                    ) && int_fields_equal(
                        record,
                        "total_origin_puts_after",
                        "total_origin_puts_before",
                    ),
                    json!({ "route_id": route_id, "pattern": pattern, "record": record }),
                );
                self.check(
                    format!("{name}-cache-bytes-flat"),
                    int_fields_equal(record, "total_bytes_after", "total_bytes_before"),
                    json!({ "route_id": route_id, "pattern": pattern, "record": record }),
                );
                self.check(
                    format!("{name}-push-warming-flat"),
                    int_fields_equal(
                        record,
                        "push_warming_writes_after",
                        "push_warming_writes_before",
                    ) && int_fields_equal(
                        record,
                        "push_warming_bytes_after",
                        "push_warming_bytes_before",
                    ),
                    json!({ "route_id": route_id, "pattern": pattern, "record": record }),
                );
                self.check(
                    format!("{name}-request-body-present"),
                    int_field(record, "request_body_len").is_some_and(|value| value > 0),
                    json!({ "route_id": route_id, "pattern": pattern, "record": record }),
                );
            }
        }
    }

    fn verify_cli_hydrate_traffic(&mut self) {
        for name in ["cli-cold-hydrate", "cli-warm-hydrate"] {
            let Some(record) = find_named_record(&self.report, "cli_hydrates", name).cloned()
            else {
                self.check(
                    format!("{name}-record-present"),
                    false,
                    json!({ "field": "cli_hydrates" }),
                );
                continue;
            };
            let origin_delta = match (
                int_field(&record, "origin_gets_after"),
                int_field(&record, "origin_gets_before"),
            ) {
                (Some(after), Some(before)) => Some(after - before),
                _ => None,
            };
            let immutable_key_delta = record
                .get("origin_get_key_delta")
                .and_then(Value::as_object)
                .map(|keys| {
                    keys.iter()
                        .filter(|(key, _)| {
                            key.starts_with(".crab/xorbs/") || key.starts_with(".crab/shards/")
                        })
                        .map(|(key, value)| (key.clone(), value.clone()))
                        .collect::<Map<_, _>>()
                })
                .unwrap_or_default();
            self.check(
                format!("{name}-immutable-origin-get-delta-zero"),
                immutable_key_delta.is_empty(),
                json!({
                    "origin_delta": origin_delta,
                    "immutable_key_delta": immutable_key_delta,
                }),
            );
            self.check(
                format!("{name}-cache-hits-observed"),
                int_field(&record, "cache_hits_delta").is_some_and(|value| value > 0),
                json!({ "cache_hits_delta": record.get("cache_hits_delta") }),
            );
            self.check(
                format!("{name}-origin-fetches-flat"),
                int_field(&record, "origin_fetches_delta") == Some(0),
                json!({ "origin_fetches_delta": record.get("origin_fetches_delta") }),
            );
            self.check(
                format!("{name}-cache-service-mutable-read-rejections-flat"),
                int_field(&record, "mutable_read_rejections_delta") == Some(0),
                json!({ "mutable_read_rejections_delta": record.get("mutable_read_rejections_delta") }),
            );
            self.check(
                format!("{name}-cache-service-mutable-write-rejections-flat"),
                int_field(&record, "mutable_write_rejections_delta") == Some(0),
                json!({ "mutable_write_rejections_delta": record.get("mutable_write_rejections_delta") }),
            );
            self.check(
                format!("{name}-origin-avoidance-observed"),
                int_field(&record, "origin_avoided_reads_delta").is_some_and(|value| value > 0),
                json!({ "origin_avoided_reads_delta": record.get("origin_avoided_reads_delta") }),
            );
        }
    }

    fn verify_cli_dedup_traffic(&mut self) {
        let Some(record) =
            find_named_record(&self.report, "cli_push_dedup", "cli-dedup-push").cloned()
        else {
            self.check(
                "cli-dedup-record-present",
                false,
                json!({ "field": "cli_push_dedup" }),
            );
            return;
        };
        self.check(
            "cli-dedup-queries-observed",
            int_field(&record, "dedup_queries_delta").is_some_and(|value| value > 0),
            json!({ "dedup_queries_delta": record.get("dedup_queries_delta") }),
        );
        self.check(
            "cli-dedup-known-chunks-observed",
            int_field(&record, "dedup_known_chunks_delta").is_some_and(|value| value > 0),
            json!({ "dedup_known_chunks_delta": record.get("dedup_known_chunks_delta") }),
        );
        self.check(
            "cli-dedup-no-unknown-chunks",
            int_field(&record, "dedup_unknown_chunks_delta") == Some(0),
            json!({ "dedup_unknown_chunks_delta": record.get("dedup_unknown_chunks_delta") }),
        );
        self.check(
            "cli-dedup-skipped-xorb-put",
            int_field(&record, "xorb_puts_delta") == Some(0),
            json!({ "xorb_puts_delta": record.get("xorb_puts_delta") }),
        );
        self.check(
            "cli-dedup-canonical-xorb-proof",
            int_field(&record, "xorb_gets_delta").is_some_and(|value| value > 0),
            json!({ "xorb_gets_delta": record.get("xorb_gets_delta") }),
        );
        self.check(
            "cli-dedup-canonical-shard-proof",
            int_field(&record, "shard_gets_delta").is_some_and(|value| value > 0),
            json!({ "shard_gets_delta": record.get("shard_gets_delta") }),
        );
        self.check(
            "cli-dedup-metadata-read",
            int_field(&record, "metadata_gets_delta").is_some_and(|value| value > 0),
            json!({ "metadata_gets_delta": record.get("metadata_gets_delta") }),
        );
        let cacheable_keys = record
            .get("cacheable_origin_get_key_delta")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        self.check(
            "cli-dedup-cacheable-origin-proof",
            int_field(&record, "cacheable_origin_gets_delta").is_some_and(|value| value > 0)
                && cacheable_keys
                    .keys()
                    .any(|key| key.starts_with(".crab/xorbs/"))
                && cacheable_keys
                    .keys()
                    .any(|key| key.starts_with(".crab/shards/")),
            json!({
                "cacheable_origin_gets_delta": record.get("cacheable_origin_gets_delta"),
                "cacheable_origin_get_key_delta": record.get("cacheable_origin_get_key_delta"),
                "origin_get_key_delta": record.get("origin_get_key_delta"),
            }),
        );
        let expected_manifest = self
            .report
            .get("run_id")
            .and_then(Value::as_str)
            .map(|run_id| format!("e2e-cache-service/{run_id}/cli-dedup/manifest"));
        let mutable_keys = record
            .get("mutable_origin_get_key_delta")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let origin_keys = record
            .get("origin_get_key_delta")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let immutable_origin_keys_are_cacheable = origin_keys
            .iter()
            .filter(|(key, _)| key.starts_with(".crab/xorbs/") || key.starts_with(".crab/shards/"))
            .all(|(key, value)| cacheable_keys.get(key) == Some(value));
        let manifest_cas = expected_manifest.as_ref().is_some_and(|manifest_key| {
            int_field(&record, "mutable_origin_gets_delta").is_some_and(|value| value > 0)
                && mutable_keys
                    .get(manifest_key)
                    .and_then(Value::as_i64)
                    .is_some_and(|value| value > 0)
                && immutable_origin_keys_are_cacheable
        });
        self.check(
            "cli-dedup-manifest-cas-origin-read",
            manifest_cas,
            json!({
                "expected_key": expected_manifest,
                "actual": record.get("origin_get_key_delta"),
                "mutable_actual": mutable_keys,
                "origin_gets_delta": record.get("origin_gets_delta"),
                "mutable_origin_gets_delta": record.get("mutable_origin_gets_delta"),
            }),
        );
        self.check(
            "cli-dedup-mutable-read-rejections-flat",
            int_field(&record, "mutable_read_rejections_delta") == Some(0),
            json!({ "mutable_read_rejections_delta": record.get("mutable_read_rejections_delta") }),
        );
        self.check(
            "cli-dedup-mutable-write-rejections-flat",
            int_field(&record, "mutable_write_rejections_delta") == Some(0),
            json!({ "mutable_write_rejections_delta": record.get("mutable_write_rejections_delta") }),
        );
    }

    fn verify_support_bundle_summary(&mut self) {
        let Some(record) =
            find_named_record(&self.report, "support_bundles", "post-traffic").cloned()
        else {
            self.check(
                "support-bundle-record-present",
                false,
                json!({ "field": "support_bundles" }),
            );
            return;
        };
        self.check(
            "support-bundle-schema",
            record.get("schema").and_then(Value::as_str) == Some("cache-service.support-bundle"),
            json!({ "schema": record.get("schema") }),
        );
        self.check(
            "support-bundle-cache-hit-rate-positive",
            float_field(&record, "cache_hit_rate").is_some_and(|value| value > 0.0),
            json!({ "cache_hit_rate": record.get("cache_hit_rate") }),
        );
        self.check(
            "support-bundle-origin-avoidance-metric-present",
            float_field(&record, "origin_avoided_reads_total").is_some_and(|value| value > 0.0),
            json!({ "origin_avoided_reads_total": record.get("origin_avoided_reads_total") }),
        );
        self.check(
            "support-bundle-origin-fetches-observed",
            float_field(&record, "origin_fetch_total").is_some_and(|value| value > 0.0),
            json!({ "origin_fetch_total": record.get("origin_fetch_total") }),
        );
    }

    fn verify_origin_outage(&mut self) {
        let Some(record) = find_named_record(
            &self.report,
            "origin_outages",
            "origin-outage-cached-read-through",
        )
        .cloned() else {
            self.check(
                "origin-outage-record-present",
                false,
                json!({ "field": "origin_outages" }),
            );
            return;
        };
        self.check(
            "origin-outage-readiness-degraded",
            int_field(&record, "health_status") == Some(503),
            json!({ "health_status": record.get("health_status") }),
        );
        self.check(
            "origin-outage-liveness-ok",
            int_field(&record, "live_status") == Some(200),
            json!({ "live_status": record.get("live_status") }),
        );
        self.check(
            "origin-outage-warm-miss-recorded",
            int_field(&record, "warm_status") == Some(200)
                && record.get("warm_cache_status").and_then(Value::as_str) == Some("MISS"),
            json!({
                "warm_status": record.get("warm_status"),
                "warm_cache_status": record.get("warm_cache_status"),
            }),
        );
        self.check(
            "origin-outage-hot-full-hit",
            int_field(&record, "hot_status") == Some(200)
                && record.get("hot_cache_status").and_then(Value::as_str) == Some("HIT"),
            json!({
                "hot_status": record.get("hot_status"),
                "hot_cache_status": record.get("hot_cache_status"),
            }),
        );
        self.check(
            "origin-outage-hot-range-hit",
            int_field(&record, "range_status") == Some(206)
                && record.get("range_cache_status").and_then(Value::as_str) == Some("HIT"),
            json!({
                "range_status": record.get("range_status"),
                "range_cache_status": record.get("range_cache_status"),
            }),
        );
        self.check(
            "origin-outage-cold-miss-fails-closed",
            int_field(&record, "cold_status") == Some(504)
                && record.get("cold_cache_status").and_then(Value::as_str) == Some(""),
            json!({
                "cold_status": record.get("cold_status"),
                "cold_cache_status": record.get("cold_cache_status"),
            }),
        );
        self.check(
            "origin-outage-hot-origin-counters-flat",
            int_field(&record, "hot_origin_gets_after_hot")
                == int_field(&record, "hot_origin_gets_before_outage")
                && int_field(&record, "hot_origin_gets_after_range")
                    == int_field(&record, "hot_origin_gets_before_outage"),
            json!({
                "hot_origin_gets_before_outage": record.get("hot_origin_gets_before_outage"),
                "hot_origin_gets_after_hot": record.get("hot_origin_gets_after_hot"),
                "hot_origin_gets_after_range": record.get("hot_origin_gets_after_range"),
            }),
        );
        self.check(
            "origin-outage-cold-origin-counter-flat",
            int_field(&record, "cold_origin_gets_after_cold")
                == int_field(&record, "cold_origin_gets_before_outage"),
            json!({
                "cold_origin_gets_before_outage": record.get("cold_origin_gets_before_outage"),
                "cold_origin_gets_after_cold": record.get("cold_origin_gets_after_cold"),
            }),
        );
        self.check(
            "origin-outage-total-origin-counters-flat",
            int_field(&record, "total_origin_gets_after_hot")
                == int_field(&record, "total_origin_gets_before_outage")
                && int_field(&record, "total_origin_gets_after_range")
                    == int_field(&record, "total_origin_gets_before_outage")
                && int_field(&record, "total_origin_gets_after_cold")
                    == int_field(&record, "total_origin_gets_before_outage"),
            json!({
                "total_origin_gets_before_outage": record.get("total_origin_gets_before_outage"),
                "total_origin_gets_after_hot": record.get("total_origin_gets_after_hot"),
                "total_origin_gets_after_range": record.get("total_origin_gets_after_range"),
                "total_origin_gets_after_cold": record.get("total_origin_gets_after_cold"),
            }),
        );
        self.check(
            "origin-outage-cache-hit-counters-increase",
            match (
                int_field(&record, "cache_hits_before_outage"),
                int_field(&record, "cache_hits_after_outage"),
            ) {
                (Some(before), Some(after)) => after >= before + 2,
                _ => false,
            },
            json!({
                "cache_hits_before_outage": record.get("cache_hits_before_outage"),
                "cache_hits_after_outage": record.get("cache_hits_after_outage"),
            }),
        );
        self.check(
            "origin-outage-origin-fetch-counters-flat",
            int_field(&record, "origin_fetches_after_outage")
                == int_field(&record, "origin_fetches_before_outage"),
            json!({
                "origin_fetches_before_outage": record.get("origin_fetches_before_outage"),
                "origin_fetches_after_outage": record.get("origin_fetches_after_outage"),
            }),
        );
        self.check(
            "origin-outage-body-lengths-present",
            int_field(&record, "hot_body_len").is_some_and(|value| value > 0)
                && int_field(&record, "range_body_len").is_some_and(|value| value > 0)
                && int_field(&record, "cold_body_len").is_some_and(|value| value > 0),
            json!({
                "hot_body_len": record.get("hot_body_len"),
                "range_body_len": record.get("range_body_len"),
                "cold_body_len": record.get("cold_body_len"),
            }),
        );
    }

    fn verify_origin_outage_support_bundle(&mut self) {
        let Some(record) =
            find_named_record(&self.report, "support_bundles", "origin-outage").cloned()
        else {
            self.check(
                "origin-outage-support-bundle-record-present",
                false,
                json!({ "field": "support_bundles" }),
            );
            return;
        };
        self.check(
            "origin-outage-support-bundle-schema",
            record.get("schema").and_then(Value::as_str) == Some("cache-service.support-bundle"),
            json!({ "schema": record.get("schema") }),
        );
        self.check(
            "origin-outage-support-bundle-health-degraded",
            record.get("health_ok").and_then(Value::as_bool) == Some(false)
                && int_field(&record, "health_status") == Some(503),
            json!({
                "health_ok": record.get("health_ok"),
                "health_status": record.get("health_status"),
            }),
        );
        self.check(
            "origin-outage-support-bundle-auth-probe-control-plane",
            record.get("auth_endpoint").and_then(Value::as_str) == Some("/v1/capabilities"),
            json!({ "auth_endpoint": record.get("auth_endpoint") }),
        );
        for probe_name in ["auth", "capabilities", "authz", "admin_stats", "metrics"] {
            let ok_field = format!("{probe_name}_ok");
            let status_field = format!("{probe_name}_status");
            self.check(
                format!("origin-outage-support-bundle-{probe_name}-probe-ok"),
                record.get(&ok_field).and_then(Value::as_bool) == Some(true)
                    && int_field(&record, &status_field) == Some(200),
                json!({
                    "ok": record.get(&ok_field),
                    "status": record.get(&status_field),
                }),
            );
        }
        self.check(
            "origin-outage-support-bundle-cache-hit-rate-positive",
            float_field(&record, "cache_hit_rate").is_some_and(|value| value > 0.0),
            json!({ "cache_hit_rate": record.get("cache_hit_rate") }),
        );
        self.check(
            "origin-outage-support-bundle-origin-avoidance-metric-present",
            float_field(&record, "origin_avoided_reads_total").is_some_and(|value| value > 0.0),
            json!({ "origin_avoided_reads_total": record.get("origin_avoided_reads_total") }),
        );
    }
}

fn read_json_file(path: &Path) -> Result<Value, String> {
    let text = fs::read_to_string(path)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    serde_json::from_str::<Value>(&text)
        .map_err(|error| format!("failed to parse {} as JSON: {error}", path.display()))
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = fs::File::open(path)
        .map_err(|error| format!("failed to open {}: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 1024 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn normalize_path(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn value_type(value: Option<&Value>) -> &'static str {
    value.map_or("missing", type_name)
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn path_to_slash(path: &Path) -> String {
    path.iter()
        .map(|component| component.to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

fn report_artifact_exists(report: &Value, key: &str) -> bool {
    report
        .get("artifacts")
        .and_then(Value::as_object)
        .is_some_and(|artifacts| artifacts.contains_key(key))
}

fn find_named_record<'a>(report: &'a Value, field: &str, name: &str) -> Option<&'a Value> {
    report
        .get(field)
        .and_then(Value::as_array)?
        .iter()
        .find(|record| record.get("name").and_then(Value::as_str) == Some(name))
}

fn pattern_records(report: &Value, field: &str) -> Option<(usize, BTreeMap<String, Value>)> {
    let records = report.get(field)?.as_array()?;
    let mut by_pattern = BTreeMap::new();
    for record in records {
        let Some(pattern) = record.get("pattern").and_then(Value::as_str) else {
            continue;
        };
        by_pattern.insert(pattern.to_owned(), record.clone());
    }
    Some((records.len(), by_pattern))
}

fn int_field(record: &Value, key: &str) -> Option<i64> {
    record.get(key).and_then(Value::as_i64)
}

fn int_fields_equal(record: &Value, left: &str, right: &str) -> bool {
    matches!(
        (int_field(record, left), int_field(record, right)),
        (Some(left), Some(right)) if left == right
    )
}

fn string_array_field(record: &Value, key: &str) -> Option<Vec<String>> {
    record
        .get(key)?
        .as_array()?
        .iter()
        .map(|value| value.as_str().map(str::to_owned))
        .collect()
}

fn expected_patterns(patterns: &[&str]) -> Vec<String> {
    patterns
        .iter()
        .map(|pattern| (*pattern).to_owned())
        .collect()
}

fn sorted_patterns(patterns: &[&str]) -> Vec<String> {
    sorted_owned(expected_patterns(patterns))
}

fn sorted_owned(mut values: Vec<String>) -> Vec<String> {
    values.sort();
    values
}

fn mutable_route_pattern_id(pattern: &str) -> &'static str {
    match pattern {
        "{repo}/refs/heads/*" => "repo-refs-heads",
        "{repo}/HEAD" => "repo-head",
        "{repo}/locks/*" => "repo-locks",
        "{repo}/packs/pack-{id}.meta" => "repo-pack-meta",
        "{repo}/manifests/*" => "repo-manifests",
        "{repo}/pack-list" => "repo-pack-list",
        "{repo}/shard-list" => "repo-shard-list",
        ".crab/ref-registry/*" => "global-ref-registry",
        "{repo}/file_index_db/manifest/current" => "repo-file-index-current",
        ".crab/chunk_index_db/manifest/current" => "global-chunk-index-current",
        _ => "unknown-route",
    }
}

fn float_field(record: &Value, key: &str) -> Option<f64> {
    record.get(key).and_then(Value::as_f64)
}

fn forbidden_literals_in(text: &str) -> Vec<&'static str> {
    [
        ("default-psk", DEFAULT_PSK),
        ("default-psk-hash", DEFAULT_PSK_BLAKE3),
        ("policy-principal", "psk-client"),
    ]
    .into_iter()
    .filter_map(|(label, literal)| text.contains(literal).then_some(label))
    .collect()
}
