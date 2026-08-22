//! Per-command schema validation: constructs a minimal valid instance of
//! each payload type, serializes it to JSON, and validates the result
//! against the committed JSON schema in `crab/schemas/`.
//!
//! This ensures that the Rust types actually produce JSON conforming to
//! the committed schemas — catching serialization mismatches that the
//! drift test alone cannot detect.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use rust_decimal::Decimal;
use serde::Serialize;

// -- Envelope and core output types --
use crab::core::output::event_payloads::{RestoreCompletePayload, RestoreSubmitPayload};
use crab::core::output::{
    ErrorCategory, ErrorInfo, ErrorSource, FileDonePayload, ProgressPayload, WarningPayload,
    XorbDonePayload,
};

// -- Command payload types (public fields) --
use crab::cmd::add::AddSummary;
use crab::cmd::clone::CloneSummary;
use crab::cmd::config::ConfigGetPayload;
use crab::cmd::dehydrate::DehydrateSummaryPayload;
use crab::cmd::env::EnvPayload;
use crab::cmd::errors::{ErrorCatalogPayload, ErrorDocEntry, ErrorDocPayload};
use crab::cmd::export::{ExportFileResult, ExportFileStatus, ExportPlanEvent, ExportSummary};
use crab::cmd::fetch::FetchSummary;
use crab::cmd::fsck::FsckSummary;
use crab::cmd::gc::GcSummary;
use crab::cmd::hydrate::HydrateSummaryPayload;
use crab::cmd::optimize::{
    OptimizePayload, OptimizeStep, OptimizeStepKind, OptimizeStepStatus, OptimizeSummary,
    OptimizeWorkflowMode,
};
use crab::cmd::prune::PruneSummary;
use crab::cmd::push::{PushRefOutcome, PushSummaryPayload};
use crab::cmd::repack::RepackSummary;
use crab::cmd::restripe::{
    OptimizeXorbsControlEvent, OptimizeXorbsControlEventKind, OptimizeXorbsEventPayload,
    RestripeCounts, RestripeSummary,
};
use crab::cmd::stat::{ClassEntry, StatClassesPayload};
use crab::cmd::tier::{TierEventPayload, TierPlanPayload, TierRulePayload, TierTransitionPayload};
use crab::cmd::track::{TrackPattern, TrackPayload};
use crab::cmd::version::VersionPayload;
use crab::cost::recommendations::{Recommendation, RiskLevel};
use crab::cost::report::{ClassCost, ColdObjectSummary, CostReport, InventorySummary};
use crab::restripe::planner::RestripeEstimate;
use crab::tier::classes::StorageClass;

fn schemas_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("schemas")
}

/// Load a committed JSON schema from disk and compile it.
fn load_schema(name: &str) -> serde_json::Value {
    let path = schemas_dir().join(format!("{name}.json"));
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read schema {}: {e}", path.display()));
    serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("invalid JSON in schema {}: {e}", path.display()))
}

/// Validate a serialized JSON value against a committed schema.
fn assert_valid(name: &str, instance: &serde_json::Value) {
    let schema = load_schema(name);
    let compiled = jsonschema::JSONSchema::compile(&schema)
        .unwrap_or_else(|e| panic!("failed to compile schema `{name}`: {e}"));
    let result = compiled.validate(instance);
    if let Err(errors) = result {
        let msgs: Vec<String> = errors.map(|e| format!("  - {e}")).collect();
        panic!(
            "schema validation failed for `{name}`:\n{}\n  instance: {}",
            msgs.join("\n"),
            serde_json::to_string_pretty(instance).unwrap_or_default(),
        );
    }
}

/// Assert a serialized JSON value is rejected by a committed schema.
fn assert_invalid(name: &str, instance: &serde_json::Value) {
    let schema = load_schema(name);
    let compiled = jsonschema::JSONSchema::compile(&schema)
        .unwrap_or_else(|e| panic!("failed to compile schema `{name}`: {e}"));
    if compiled.is_valid(instance) {
        panic!(
            "schema validation unexpectedly passed for `{name}`:\n  instance: {}",
            serde_json::to_string_pretty(instance).unwrap_or_default(),
        );
    }
}

/// Serialize a value and validate it against the named schema.
fn validate<T: Serialize>(name: &str, value: &T) {
    let json =
        serde_json::to_value(value).unwrap_or_else(|e| panic!("failed to serialize `{name}`: {e}"));
    assert_valid(name, &json);
}

// ---------------------------------------------------------------------------
// Event payload types
// ---------------------------------------------------------------------------

#[test]
fn validate_progress_event() {
    validate(
        "progress.event",
        &ProgressPayload {
            operation: "uploading".into(),
            current: 5,
            total: 10,
            bytes: 1024,
            total_bytes: 2048,
            rate_bytes_per_sec: 512.0,
            xorbs_produced: None,
        },
    );
}

#[test]
fn validate_file_done_event() {
    validate(
        "file_done.event",
        &FileDonePayload {
            path: "data/file.bin".into(),
            bytes: 4096,
            duration_ms: 120,
            status: "ok".into(),
        },
    );
}

#[test]
fn validate_xorb_done_event() {
    validate(
        "xorb_done.event",
        &XorbDonePayload {
            hash: "abc123".into(),
            bytes: 8192,
            compressed_bytes: 4096,
            status: "ok".into(),
        },
    );
}

#[test]
fn validate_warning_event() {
    validate(
        "warning.event",
        &WarningPayload {
            code: "CRAB-E0001".into(),
            message: "transient network error".into(),
            path: Some("data/file.bin".into()),
        },
    );
}

// ---------------------------------------------------------------------------
// Error types
// ---------------------------------------------------------------------------

#[test]
fn validate_error_info() {
    validate(
        "error",
        &ErrorInfo {
            code: "CRAB-E0001".into(),
            category: ErrorCategory::Transient,
            message: "network transient error".into(),
            retryable: true,
            details: serde_json::json!({"endpoint": "s3://bucket"}),
            source_chain: vec![ErrorSource {
                message: "connection reset".into(),
            }],
        },
    );
}

#[test]
fn validate_error_category() {
    validate("error_category", &ErrorCategory::Transient);
}

#[test]
fn validate_error_source() {
    validate(
        "error_source",
        &ErrorSource {
            message: "underlying cause".into(),
        },
    );
}

// ---------------------------------------------------------------------------
// Command result payloads — types with public fields
// ---------------------------------------------------------------------------

#[test]
fn validate_add() {
    validate(
        "add",
        &AddSummary {
            files_staged: 3,
            files_skipped: 1,
            files_failed: 0,
            chunks_staged: 12,
            bytes_processed: 65536,
            staging_duration_ms: 100,
            planning_duration_ms: 40,
            flushing_duration_ms: 10,
            indexing_duration_ms: 50,
            duration_ms: 200,
        },
    );
}

#[test]
fn validate_clone() {
    validate(
        "clone",
        &CloneSummary {
            url: "crab://bucket/repo".into(),
            directory: "/tmp/repo".into(),
            branch: None,
            lazy: true,
            duration_ms: 5000,
        },
    );
}

#[test]
fn validate_config_get() {
    validate(
        "config.get",
        &ConfigGetPayload {
            key: "remote.url".into(),
            value: "crab://bucket/repo".into(),
            source: "local".into(),
        },
    );
}

#[test]
fn validate_cost() {
    validate(
        "cost",
        &CostReport {
            price_table_version: "2026-03-01".into(),
            override_version: None,
            generated_at: "2026-04-24T18:32:17Z".into(),
            inventory: InventorySummary {
                source: "live".into(),
                scanned_at: "2026-04-24T18:30:00Z".into(),
                total_objects: 2,
                total_bytes: 2048,
                total_bytes_human: "2.0 KiB".into(),
            },
            current_monthly_usd: Decimal::new(250, 2),
            projected_monthly_usd: Decimal::new(125, 2),
            projected_savings_usd: Decimal::new(125, 2),
            per_class_costs: BTreeMap::from([(
                "STANDARD".into(),
                ClassCost {
                    objects: 2,
                    bytes: 2048,
                    bytes_human: "2.0 KiB".into(),
                    share_pct: Decimal::new(1000, 1),
                    monthly_storage_usd: Decimal::new(200, 2),
                    monthly_retrieval_usd: Decimal::new(50, 2),
                    monthly_total_usd: Decimal::new(250, 2),
                },
            )]),
            recommendations: vec![Recommendation {
                title: "Enable Standard-IA tiering".into(),
                rationale: "cold data can move to IA".into(),
                action_cmd: "crab tier plan --apply".into(),
                delta_usd_month: Decimal::new(125, 2),
                risk_level: RiskLevel::Low,
                dependencies: Vec::new(),
                enabled: true,
            }],
            heaviest_cold: vec![ColdObjectSummary {
                key: ".crab/xorbs/abc".into(),
                size: 2048,
                size_human: "2.0 KiB".into(),
                storage_class: "STANDARD".into(),
                last_modified: "2026-04-24T18:00:00Z".into(),
            }],
            assumptions: vec!["sample data".into()],
        },
    );
}

#[test]
fn cost_schema_rejects_missing_required_fields() {
    assert_invalid(
        "cost",
        &serde_json::json!({"price_table_version": "2026-03-01"}),
    );
}

#[test]
fn validate_dehydrate() {
    validate(
        "dehydrate",
        &DehydrateSummaryPayload {
            dehydrated: 5,
            bytes_freed: 10240,
            skipped: 1,
            failed: 0,
            dirty_skipped: 0,
            duration_ms: 300,
            profile_protected: 0,
        },
    );
}

#[test]
fn validate_env() {
    validate(
        "env",
        &EnvPayload {
            crab_version: "0.1.0".into(),
            git_sha: "abc1234".into(),
            build_timestamp: "2026-01-01T00:00:00Z".into(),
            git_version: Some("git version 2.45.0".into()),
            remote_url: None,
            platform: "aarch64-unix-macos".into(),
        },
    );
}

#[test]
fn validate_errors_catalog() {
    validate(
        "errors",
        &ErrorCatalogPayload {
            codes: vec![ErrorDocEntry {
                code: "CRAB-E0001",
                name: "NetworkTransient",
                category: "transient",
                retryable: true,
                message_template: "network transient error: {0}",
                remediation: "Retry the operation.",
            }],
        },
    );
}

#[test]
fn validate_errors_entry() {
    validate(
        "errors.entry",
        &ErrorDocEntry {
            code: "CRAB-E0001",
            name: "NetworkTransient",
            category: "transient",
            retryable: true,
            message_template: "network transient error: {0}",
            remediation: "Retry the operation.",
        },
    );
}

#[test]
fn validate_errors_lookup() {
    validate(
        "errors.lookup",
        &ErrorDocPayload {
            code: "CRAB-E0001",
            name: "NetworkTransient",
            category: "transient",
            retryable: true,
            message_template: "network transient error: {0}",
            remediation: "Retry the operation.",
        },
    );
}

#[test]
fn validate_export_file() {
    validate(
        "export.file",
        &ExportFileResult {
            repo_path: "crab/large-files/model.bin".into(),
            object_path: "exports/crab/large-files/model.bin".into(),
            bytes: 1024,
            status: ExportFileStatus::Exported,
        },
    );
}

#[test]
fn validate_export_plan() {
    validate(
        "export.plan",
        &ExportPlanEvent {
            repo: "crab://crab/import-demo".into(),
            target_url: "s3://crab/export-demo".into(),
            requested_revision: "HEAD".into(),
            resolved_revision: "abc123".into(),
            files: 1,
            bytes: 1024,
            dry_run: false,
            force: false,
        },
    );
}

#[test]
fn validate_export_summary() {
    validate(
        "export.summary",
        &ExportSummary {
            repo: "crab://crab/import-demo".into(),
            target_url: "s3://crab/export-demo".into(),
            requested_revision: "HEAD".into(),
            resolved_revision: "abc123".into(),
            files_planned: 1,
            files_exported: 1,
            files_conflicted: 0,
            bytes_planned: 1024,
            bytes_exported: 1024,
            dry_run: false,
            files: vec![ExportFileResult {
                repo_path: "crab/large-files/model.bin".into(),
                object_path: "exports/crab/large-files/model.bin".into(),
                bytes: 1024,
                status: ExportFileStatus::Exported,
            }],
            duration_ms: 50,
        },
    );
}

#[test]
fn validate_fetch() {
    validate(
        "fetch",
        &FetchSummary {
            objects_fetched: 10,
            bytes_downloaded: 102400,
            objects_skipped: 2,
            duration_ms: 1500,
        },
    );
}

#[test]
fn validate_fsck() {
    validate(
        "fsck",
        &FsckSummary {
            errors: 0,
            info_count: 1,
            repaired: 0,
            repair_failures: 0,
            passed: true,
        },
    );
}

#[test]
fn validate_gc() {
    validate(
        "gc",
        &GcSummary {
            packs_deleted: 2,
            xorbs_deleted: 5,
            shards_deleted: 1,
            file_index_entries_deleted: 3,
            bytes_reclaimed: 50000,
            dry_run: false,
            cancelled: false,
            partial_enumeration: false,
            delete_failures: 0,
            reconciliation_failed: false,
        },
    );
}

#[test]
fn validate_hydrate() {
    validate(
        "hydrate",
        &HydrateSummaryPayload {
            hydrated: 8,
            bytes_written: 81920,
            skipped: 0,
            bytes_skipped: 0,
            failed: 0,
            duration_ms: 400,
            recovered: 0,
            bytes_recovered: 0,
            cow_cloned: 0,
            bytes_cow_cloned: 0,
        },
    );
}

#[test]
fn validate_optimize_xorbs_plan() {
    validate(
        "optimize.xorbs.plan",
        &RestripeEstimate {
            profile: "ml".into(),
            source_count: 4,
            source_bytes: 1024,
            estimated_dest_count: 2,
            estimated_dest_bytes: 900,
            estimated_wall_secs: 30,
            estimated_cost_usd: "0.000123".into(),
            archive_source_count: 1,
            archive_source_bytes: 256,
        },
    );
}

#[test]
fn validate_optimize_xorbs_event_variants() {
    let cases = [
        serde_json::to_value(OptimizeXorbsEventPayload::Summary(RestripeSummary {
            run_id: "01976b86-12b7-7000-8000-000000000001".into(),
            profile: "ml".into(),
            counts: RestripeCounts {
                done: 2,
                corrupt: 0,
                skipped: 1,
                pending: 0,
            },
            bytes_read: 1024,
            bytes_written: 900,
            elapsed_ms: 1234,
            corrupt_list: Vec::new(),
        }))
        .expect("summary serializes"),
        serde_json::to_value(OptimizeXorbsEventPayload::Control(
            OptimizeXorbsControlEvent {
                event: OptimizeXorbsControlEventKind::Aborted,
                run_id: Some("01976b86-12b7-7000-8000-000000000001".into()),
            },
        ))
        .expect("control event serializes"),
    ];

    for case in cases {
        assert_valid("optimize.xorbs.event", &case);
    }
}

#[test]
fn validate_optimize_plan_and_apply() {
    let payload = OptimizePayload {
        mode: OptimizeWorkflowMode::Plan,
        generated_at: "2026-04-24T18:32:17Z".into(),
        steps: vec![OptimizeStep {
            id: "cost-report".into(),
            kind: OptimizeStepKind::CostReport,
            title: "Cost report".into(),
            command: "crab doctor --cost".into(),
            mutates: false,
            status: OptimizeStepStatus::Planned,
            detail: "collect bucket inventory".into(),
        }],
        summary: OptimizeSummary {
            planned: 1,
            skipped: 0,
            running: 0,
            succeeded: 0,
            failed: 0,
            mutating_steps: 0,
        },
        assumptions: vec!["cost analysis uses doctor inputs".into()],
    };

    validate("optimize.plan", &payload);
    validate("optimize.apply", &payload);
}

#[test]
fn validate_prune() {
    validate(
        "prune",
        &PruneSummary {
            objects_pruned: 3,
            chunks_pruned: 1,
            shards_pruned: 1,
            xorbs_pruned: 1,
            bytes_freed: 30000,
            dry_run: false,
        },
    );
}

#[test]
fn validate_push() {
    validate(
        "push",
        &PushSummaryPayload {
            refs_pushed: 1,
            refs: vec![PushRefOutcome {
                src: "refs/heads/main".into(),
                dst: "refs/heads/main".into(),
                status: "ok".into(),
                error: None,
                retryable: None,
                retry_after_secs: None,
            }],
            duration_ms: 2000,
            remote_url: "crab://bucket/repo".into(),
            integration_retries: Some(3),
            integration_retry_limit: Some(256),
            integration_retry_stages: Some(std::collections::BTreeMap::from([
                ("lock".to_owned(), 2),
                ("ref-commit".to_owned(), 1),
            ])),
            operation_id: None,
            coordinator_epoch: None,
            writer_region: None,
            commit_state: None,
        },
    );
}

#[test]
fn validate_push_ref_outcome() {
    validate(
        "push.ref_outcome",
        &PushRefOutcome {
            src: "refs/heads/main".into(),
            dst: "refs/heads/main".into(),
            status: "ok".into(),
            error: None,
            retryable: None,
            retry_after_secs: None,
        },
    );
}

#[test]
fn validate_repack() {
    validate(
        "repack",
        &RepackSummary {
            packs_before: 5,
            packs_after: 2,
            bytes_before: 100000,
            bytes_after: 80000,
            elapsed_ms: 600,
        },
    );
}

#[test]
fn validate_stat_classes() {
    validate(
        "stat.classes",
        &StatClassesPayload {
            classes: vec![ClassEntry {
                class: "STANDARD".into(),
                bytes: 2048,
                objects: 2,
                share: 1.0,
            }],
            total_bytes: 2048,
            total_objects: 2,
        },
    );
}

#[test]
fn validate_tier_plan() {
    validate(
        "tier.plan",
        &TierPlanPayload {
            provider: "s3".into(),
            rules: vec![TierRulePayload {
                id: "crab-xorbs-to-ia".into(),
                prefix: ".crab/xorbs/".into(),
                transitions: vec![TierTransitionPayload {
                    days: 30,
                    to_class: StorageClass::S3StandardIa,
                }],
                noncurrent_expiration_days: Some(30),
                min_object_size_bytes: Some(128_000),
            }],
            versioning_enabled: true,
            object_lock_enabled: false,
        },
    );
}

#[test]
fn validate_tier_event_variants() {
    let cases = [
        serde_json::to_value(TierEventPayload::Plan(TierPlanPayload {
            provider: "s3".into(),
            rules: Vec::new(),
            versioning_enabled: false,
            object_lock_enabled: false,
        }))
        .expect("tier plan event serializes"),
        serde_json::to_value(TierEventPayload::RestoreSubmit(RestoreSubmitPayload {
            xorb_hash: "abc123".into(),
            class: "GLACIER".into(),
            restore_tier: "standard".into(),
            requested_at: "2026-04-24T18:32:17Z".into(),
            expected_ready_at: Some("2026-04-24T21:32:17Z".into()),
            poll_interval_ms: 30_000,
        }))
        .expect("restore submit event serializes"),
        serde_json::to_value(TierEventPayload::RestoreComplete(RestoreCompletePayload {
            xorb_hash: "abc123".into(),
            class: "GLACIER".into(),
            state: "ready".into(),
            completed_at: "2026-04-24T21:32:17Z".into(),
            wait_ms: 10_800_000,
        }))
        .expect("restore complete event serializes"),
    ];

    for case in cases {
        assert_valid("tier.event", &case);
    }
}

#[test]
fn validate_track() {
    validate(
        "track",
        &TrackPayload {
            patterns: vec![TrackPattern {
                glob: "*.bin".into(),
                source: ".gitattributes".into(),
            }],
        },
    );
}

#[test]
fn validate_track_pattern() {
    validate(
        "track.pattern",
        &TrackPattern {
            glob: "*.bin".into(),
            source: ".gitattributes".into(),
        },
    );
}

#[test]
fn validate_version() {
    validate(
        "version",
        &VersionPayload {
            crab_version: "0.1.0".into(),
            git_sha: "abc1234".into(),
            build_timestamp: "2026-01-01T00:00:00Z".into(),
            schemas: BTreeMap::from([("status".to_owned(), "1.0".to_owned())]),
        },
    );
}

#[test]
fn validate_replica_certification() {
    let instance = serde_json::json!({
        "collected_at_ms": 1,
        "profile": "enterprise",
        "certified": false,
        "deep": true,
        "redacted": true,
        "status": {
            "primary": "crab://bucket/repo",
            "replicas": [],
            "health": [],
            "backfill": [],
            "control_plane": []
        },
        "active_active": {
            "mode": "read-replica",
            "coordinator_configured": false,
            "coordinator_ready": false,
            "writes_enabled": false,
            "enabled_writers": 0,
            "reason": null
        },
        "evidence": {
            "directory": "<redacted>",
            "verified": true,
            "require_redacted": true,
            "profile": "enterprise",
            "summary": {
                "files_seen": 64,
                "files_verified": 64,
                "files_failed": 0,
                "control_plane_evidence": 30,
                "smoke_evidence": 34,
                "redacted": 64,
                "unredacted": 0
            },
            "gates": [{
                "code": "enterprise-storage-provider-matrix",
                "state": "passed",
                "message": "provider matrix verified",
                "labels": []
            }]
        },
        "gates": [{
            "code": "certification.replica-inventory",
            "state": "failed",
            "message": "no read replicas are configured",
            "remediation": "add and verify at least one regional read replica"
        }],
        "findings": [{
            "code": "replica.none_configured",
            "severity": "warning",
            "message": "no replicas are configured",
            "replica": null,
            "remediation": "run crab replica add"
        }],
        "fix_plan": [{
            "code": "replica.none_configured",
            "severity": "warning",
            "replica": null,
            "description": "configure a read replica",
            "command": "crab replica add west --dry-run --json",
            "cost_hints": [],
            "risk_hints": [],
            "destructive": false
        }]
    });
    assert_valid("replica.certification", &instance);
}

#[test]
fn validate_replica_live_control_plane_evidence() {
    let instance = serde_json::json!({
        "schema": "replica.live-control-plane.evidence",
        "version": "1.0",
        "collected_at_ms": 1,
        "harness": "replica-live-control-plane",
        "run_id": "schema-test-control-plane",
        "sequence": 1,
        "label": "coordinator-status",
        "provider": "dynamodb",
        "redacted": true,
        "args": [
            "replica",
            "coordinator",
            "status",
            "--json"
        ],
        "result": {
            "schema": "replica.coordinator.status",
            "data": {
                "status": {
                    "backend_available": true,
                    "checked_drift": true
                }
            }
        }
    });
    assert_valid("replica.live-control-plane.evidence", &instance);
}

#[test]
fn validate_replica_evidence_verify() {
    let instance = serde_json::json!({
        "directory": "replica-live-evidence",
        "verified": true,
        "require_redacted": true,
        "profile": "active-active-smoke",
        "summary": {
            "files_seen": 2,
            "files_verified": 2,
            "files_failed": 0,
            "control_plane_evidence": 1,
            "smoke_evidence": 1,
            "redacted": 2,
            "unredacted": 0
        },
        "gates": [{
            "code": "active-active-certification",
            "state": "passed",
            "message": "active-active certification evidence was recorded",
            "labels": ["active-active-certification"]
        }],
        "files": [{
            "path": "001-coordinator-status.json",
            "state": "verified",
            "kind": "live-control-plane",
            "harness": "replica-live-control-plane",
            "run_id": "schema-test-control-plane",
            "sequence": 1,
            "schema": "replica.live-control-plane.evidence",
            "version": "1.0",
            "label": "coordinator-status",
            "provider": "dynamodb",
            "collected_at_ms": 1,
            "redacted": true
        }]
    });
    assert_valid("replica.evidence.verify", &instance);
}

#[test]
fn validate_replica_live_smoke_evidence_result() {
    let instance = serde_json::json!({
        "schema": "replica.live-smoke.evidence",
        "version": "1.0",
        "collected_at_ms": 1,
        "harness": "replica-live-cross-region",
        "run_id": "schema-test-smoke",
        "sequence": 1,
        "label": "active-active-certification",
        "coordinator_provider": "dynamodb",
        "redacted": true,
        "cwd": "writer-a",
        "args": [
            "replica",
            "certify",
            "--profile",
            "active-active",
            "--json"
        ],
        "result": {
            "schema": "replica.certification",
            "data": {
                "certified": true,
                "deep": true,
                "profile": "active-active",
                "coordinator": {
                    "provider": "dynamodb"
                },
                "gates": [{
                    "code": "certification.active-active",
                    "state": "passed"
                }]
            }
        }
    });
    assert_valid("replica.live-smoke.evidence", &instance);
}

#[test]
fn validate_replica_live_smoke_active_active_failover_and_repair_evidence() {
    let cases = vec![
        (
            2,
            "initial-failover-status",
            vec!["replica", "failover", "status", "--json"],
            serde_json::json!({
                "schema": "replica.failover",
                "data": {
                    "active_active": {
                        "writes_enabled": true
                    },
                    "coordinator": {
                        "provider": "dynamodb"
                    },
                    "automation_policy": {
                        "automatic_write_failover_supported": false,
                        "orchestration": "manual",
                        "split_brain_policy": "fail-closed",
                        "adr": "crab/docs/design/replica-active-active-failover.md"
                    },
                    "automation_plan": {
                        "action": "monitor",
                        "automatic_apply_supported": false,
                        "reason": "coordinator is healthy",
                        "required_evidence": [
                            "coordinator data-plane health"
                        ]
                    }
                }
            }),
        ),
        (
            3,
            "writes-fenced",
            vec!["replica", "failover", "status", "--json"],
            serde_json::json!({
                "schema": "replica.failover",
                "data": {
                    "active_active": {
                        "writes_enabled": false
                    },
                    "coordinator": {
                        "provider": "dynamodb"
                    },
                    "automation_policy": {
                        "automatic_write_failover_supported": false,
                        "orchestration": "manual",
                        "split_brain_policy": "fail-closed",
                        "adr": "crab/docs/design/replica-active-active-failover.md"
                    },
                    "automation_plan": {
                        "action": "repair",
                        "automatic_apply_supported": true,
                        "reason": "writes are fenced",
                        "required_evidence": [
                            "coordinator repair proof"
                        ]
                    }
                }
            }),
        ),
        (
            4,
            "failover-fence",
            vec![
                "replica",
                "failover",
                "run",
                "--writer-unhealthy",
                "west",
                "--apply",
                "--json",
            ],
            serde_json::json!({
                "schema": "replica.failover.run",
                "data": {
                    "apply_requested": true,
                    "applied": true,
                    "active_active": {
                        "writes_enabled": true
                    },
                    "automation_policy": {
                        "automatic_write_failover_supported": false,
                        "orchestration": "manual",
                        "split_brain_policy": "fail-closed",
                        "adr": "crab/docs/design/replica-active-active-failover.md"
                    },
                    "automation_plan": {
                        "action": "fence",
                        "automatic_apply_supported": true,
                        "reason": "writer is unhealthy",
                        "required_evidence": [
                            "coordinator fence proof"
                        ]
                    },
                    "operation": {
                        "operation": "fence",
                        "applied": true,
                        "automation_policy": {
                            "automatic_write_failover_supported": false,
                            "orchestration": "manual",
                            "split_brain_policy": "fail-closed",
                            "adr": "crab/docs/design/replica-active-active-failover.md"
                        },
                        "outcome": {
                            "healthy": false,
                            "provider": "dynamodb"
                        }
                    }
                }
            }),
        ),
        (
            5,
            "failover-resume",
            vec![
                "replica",
                "failover",
                "run",
                "--repair-verified",
                "--apply",
                "--json",
            ],
            serde_json::json!({
                "schema": "replica.failover.run",
                "data": {
                    "apply_requested": true,
                    "applied": true,
                    "active_active": {
                        "writes_enabled": false
                    },
                    "automation_policy": {
                        "automatic_write_failover_supported": false,
                        "orchestration": "manual",
                        "split_brain_policy": "fail-closed",
                        "adr": "crab/docs/design/replica-active-active-failover.md"
                    },
                    "automation_plan": {
                        "action": "resume",
                        "automatic_apply_supported": true,
                        "reason": "repair is verified",
                        "required_evidence": [
                            "coordinator repair proof"
                        ]
                    },
                    "operation": {
                        "operation": "resume",
                        "applied": true,
                        "repair_verified": true,
                        "automation_policy": {
                            "automatic_write_failover_supported": false,
                            "orchestration": "manual",
                            "split_brain_policy": "fail-closed",
                            "adr": "crab/docs/design/replica-active-active-failover.md"
                        },
                        "outcome": {
                            "healthy": true,
                            "provider": "dynamodb"
                        }
                    }
                }
            }),
        ),
        (
            6,
            "repair-snapshot",
            vec![
                "replica",
                "repair",
                "--from-coordinator",
                "--watch",
                "--samples",
                "1",
                "--jsonl",
            ],
            serde_json::json!({
                "schema": "replica.repair.event",
                "type": "snapshot",
                "data": {
                    "sample": 1,
                    "interval_seconds": 30,
                    "worker": {
                        "schema_version": 1,
                        "worker_id": "worker-a",
                        "pid": 42,
                        "lease_path": ".crab/replication/repair-watch-lease.json",
                        "heartbeat_at_ms": 2,
                        "expires_at_ms": 302000,
                        "base_interval_seconds": 30,
                        "next_interval_seconds": 30,
                        "consecutive_errors": 0,
                        "dry_run": false
                    },
                    "repair": {
                        "from_coordinator": true,
                        "blocked_reason": null
                    }
                }
            }),
        ),
        (
            7,
            "clone-main",
            vec!["clone", "--json"],
            serde_json::json!({
                "schema": "clone",
                "data": {
                    "url": "crab://bucket/repo",
                    "directory": "repo",
                    "lazy": false,
                    "reader_region": "us-west-2"
                }
            }),
        ),
        (
            8,
            "hydrate-main",
            vec!["hydrate", "--all", "--json"],
            serde_json::json!({
                "schema": "hydrate",
                "data": {
                    "hydrated": 1,
                    "failed": 0,
                    "reader_region": "us-west-2"
                }
            }),
        ),
    ];

    for (sequence, label, args, result) in cases {
        let instance = serde_json::json!({
            "schema": "replica.live-smoke.evidence",
            "version": "1.0",
            "collected_at_ms": 1,
            "harness": "replica-live-cross-region",
            "run_id": "schema-test-smoke",
            "sequence": sequence,
            "label": label,
            "coordinator_provider": "dynamodb",
            "redacted": true,
            "cwd": "writer-a",
            "args": args,
            "result": result
        });
        assert_valid("replica.live-smoke.evidence", &instance);
    }
}

#[test]
fn validate_replica_live_smoke_rejects_enabled_failover_status_when_fenced() {
    let instance = serde_json::json!({
        "schema": "replica.live-smoke.evidence",
        "version": "1.0",
        "collected_at_ms": 1,
        "harness": "replica-live-cross-region",
        "run_id": "schema-test-smoke",
        "sequence": 9,
        "label": "writes-fenced",
        "coordinator_provider": "dynamodb",
        "redacted": true,
        "cwd": "writer-a",
        "args": [
            "replica",
            "failover",
            "status",
            "--json"
        ],
        "result": {
            "schema": "replica.failover",
            "data": {
                "active_active": {
                    "writes_enabled": true
                },
                "coordinator": {
                    "provider": "dynamodb"
                },
                "automation_policy": {
                    "automatic_write_failover_supported": false,
                    "orchestration": "manual",
                    "split_brain_policy": "fail-closed",
                    "adr": "crab/docs/design/replica-active-active-failover.md"
                },
                "automation_plan": {
                    "action": "repair",
                    "automatic_apply_supported": true,
                    "reason": "writes are fenced",
                    "required_evidence": [
                        "coordinator repair proof"
                    ]
                }
            }
        }
    });
    assert_invalid("replica.live-smoke.evidence", &instance);
}

#[test]
fn validate_replica_live_smoke_rejects_active_active_certification_without_gate() {
    let instance = serde_json::json!({
        "schema": "replica.live-smoke.evidence",
        "version": "1.0",
        "collected_at_ms": 1,
        "harness": "replica-live-cross-region",
        "run_id": "schema-test-smoke",
        "sequence": 10,
        "label": "active-active-certification",
        "coordinator_provider": "dynamodb",
        "redacted": true,
        "cwd": "writer-a",
        "args": [
            "replica",
            "certify",
            "--profile",
            "active-active",
            "--json"
        ],
        "result": {
            "schema": "replica.certification",
            "data": {
                "certified": true,
                "deep": true,
                "profile": "active-active",
                "coordinator": {
                    "provider": "dynamodb"
                },
                "gates": []
            }
        }
    });
    assert_invalid("replica.live-smoke.evidence", &instance);
}

#[test]
fn validate_replica_live_smoke_evidence_rejection() {
    let instance = serde_json::json!({
        "schema": "replica.live-smoke.evidence",
        "version": "1.0",
        "collected_at_ms": 1,
        "harness": "replica-live-cross-region",
        "run_id": "schema-test-smoke",
        "sequence": 2,
        "label": "fenced-push-rejection",
        "coordinator_provider": "dynamodb",
        "redacted": true,
        "cwd": "writer-b",
        "args": [
            "push",
            "--json"
        ],
        "exit_code": 1,
        "stdout_json": null,
        "stdout": "",
        "stderr": "coordinator health check failed; active-active writes fail closed"
    });
    assert_valid("replica.live-smoke.evidence", &instance);
}

#[test]
fn validate_replica_live_smoke_provider_hydrate_evidence() {
    let cases = vec![
        (
            3,
            "provider-hydrate-init",
            vec!["init", "crab://source/repo", "--json"],
            serde_json::json!({"schema": "init"}),
        ),
        (
            4,
            "provider-hydrate-push",
            vec![
                "push",
                "origin",
                "refs/heads/main:refs/heads/main",
                "--json",
            ],
            serde_json::json!({
                "schema": "push",
                "data": {
                    "refs_pushed": 1
                }
            }),
        ),
        (
            5,
            "provider-hydrate-copy",
            vec!["copy-primary-to-replica"],
            serde_json::json!({
                "schema": "replica.live-hydrate",
                "data": {
                    "provider": "s3",
                    "copied_objects": 3
                }
            }),
        ),
        (
            6,
            "provider-hydrate-read-enabled",
            vec!["replica", "wait", "west", "--enable-read", "--json"],
            serde_json::json!({
                "schema": "replica.wait",
                "data": {
                    "read_enabled": true
                }
            }),
        ),
        (
            7,
            "provider-hydrate-primary-xorbs-deleted",
            vec!["delete-primary-xorbs"],
            serde_json::json!({
                "schema": "replica.live-hydrate",
                "data": {
                    "provider": "s3",
                    "deleted_xorbs": 1
                }
            }),
        ),
        (
            8,
            "provider-hydrate-selected-replica",
            vec!["hydrate", "--all", "--json"],
            serde_json::json!({
                "schema": "hydrate",
                "data": {
                    "hydrated": 1,
                    "failed": 0
                }
            }),
        ),
    ];

    for (sequence, label, args, result) in cases {
        let instance = serde_json::json!({
            "schema": "replica.live-smoke.evidence",
            "version": "1.0",
            "collected_at_ms": 1,
            "harness": "replica-binary-hydrate-live",
            "run_id": "schema-test-hydrate",
            "sequence": sequence,
            "label": label,
            "provider": "s3",
            "redacted": true,
            "cwd": "source",
            "args": args,
            "result": result
        });
        assert_valid("replica.live-smoke.evidence", &instance);
    }
}

#[test]
fn validate_replica_live_smoke_rejects_provider_hydrate_copy_without_provider() {
    let instance = serde_json::json!({
        "schema": "replica.live-smoke.evidence",
        "version": "1.0",
        "collected_at_ms": 1,
        "harness": "replica-binary-hydrate-live",
        "run_id": "schema-test-hydrate",
        "sequence": 9,
        "label": "provider-hydrate-copy",
        "provider": "s3",
        "redacted": true,
        "cwd": "source",
        "args": [
            "copy-primary-to-replica"
        ],
        "result": {
            "schema": "replica.live-hydrate",
            "data": {
                "copied_objects": 3
            }
        }
    });
    assert_invalid("replica.live-smoke.evidence", &instance);
}

#[test]
fn validate_replica_live_smoke_rejects_failed_provider_hydrate() {
    let instance = serde_json::json!({
        "schema": "replica.live-smoke.evidence",
        "version": "1.0",
        "collected_at_ms": 1,
        "harness": "replica-binary-hydrate-live",
        "run_id": "schema-test-hydrate",
        "sequence": 10,
        "label": "provider-hydrate-selected-replica",
        "provider": "s3",
        "redacted": true,
        "cwd": "source",
        "args": [
            "hydrate",
            "--all",
            "--json"
        ],
        "result": {
            "schema": "hydrate",
            "data": {
                "hydrated": 1,
                "failed": 1
            }
        }
    });
    assert_invalid("replica.live-smoke.evidence", &instance);
}

#[test]
fn validate_replica_live_smoke_repair_service_template_evidence() {
    let instance = serde_json::json!({
        "schema": "replica.live-smoke.evidence",
        "version": "1.0",
        "collected_at_ms": 1,
        "harness": "replica-live-cross-region",
        "run_id": "schema-test-smoke",
        "sequence": 3,
        "label": "repair-service-template",
        "coordinator_provider": "dynamodb",
        "redacted": true,
        "cwd": "writer-a",
        "args": [
            "replica",
            "repair",
            "--from-coordinator",
            "--service-template",
            "systemd"
        ],
        "result": {
            "schema": "replica.repair.service-template",
            "data": {
                "service_template": "systemd",
                "from_coordinator": true,
                "watch": true,
                "jsonl": true,
                "rendered": true,
                "non_mutating": true,
                "interval_seconds": 30,
                "template_blake3": "1111111111111111111111111111111111111111111111111111111111111111",
                "command_blake3": "2222222222222222222222222222222222222222222222222222222222222222",
                "command": [
                    "crab",
                    "replica",
                    "repair",
                    "--from-coordinator",
                    "--watch",
                    "--jsonl"
                ]
            }
        }
    });
    assert_valid("replica.live-smoke.evidence", &instance);
}

#[test]
fn validate_replica_live_smoke_repair_worker_deployment_evidence() {
    let instance = serde_json::json!({
        "schema": "replica.live-smoke.evidence",
        "version": "1.0",
        "collected_at_ms": 1,
        "harness": "replica-live-cross-region",
        "run_id": "schema-test-smoke",
        "sequence": 4,
        "label": "repair-worker-deployment",
        "coordinator_provider": "dynamodb",
        "redacted": true,
        "cwd": "writer-a",
        "args": [
            "external",
            "repair-worker-deployment-evidence",
            "s3://evidence/repair-worker-deployment.json"
        ],
        "result": {
            "schema": "replica.repair.worker-deployment",
            "data": {
                "artifact_ref": "s3://evidence/repair-worker-deployment.json",
                "deployment_verified": true,
                "service_template": "systemd",
                "template_blake3": "1111111111111111111111111111111111111111111111111111111111111111",
                "command_blake3": "2222222222222222222222222222222222222222222222222222222222222222",
                "command": [
                    "crab",
                    "replica",
                    "repair",
                    "--from-coordinator",
                    "--watch",
                    "--jsonl"
                ]
            }
        }
    });
    assert_valid("replica.live-smoke.evidence", &instance);
}

#[test]
fn validate_replica_live_smoke_rejects_repair_worker_missing_digest() {
    let instance = serde_json::json!({
        "schema": "replica.live-smoke.evidence",
        "version": "1.0",
        "collected_at_ms": 1,
        "harness": "replica-live-cross-region",
        "run_id": "schema-test-smoke",
        "sequence": 5,
        "label": "repair-worker-deployment",
        "coordinator_provider": "dynamodb",
        "redacted": true,
        "cwd": "writer-a",
        "args": [
            "external",
            "repair-worker-deployment-evidence",
            "s3://evidence/repair-worker-deployment.json"
        ],
        "result": {
            "schema": "replica.repair.worker-deployment",
            "data": {
                "artifact_ref": "s3://evidence/repair-worker-deployment.json",
                "deployment_verified": true,
                "service_template": "systemd",
                "command_blake3": "2222222222222222222222222222222222222222222222222222222222222222",
                "command": [
                    "crab",
                    "replica",
                    "repair"
                ]
            }
        }
    });
    assert_invalid("replica.live-smoke.evidence", &instance);
}

#[test]
fn validate_replica_live_smoke_production_load_evidence() {
    let instance = serde_json::json!({
        "schema": "replica.live-smoke.evidence",
        "version": "1.0",
        "collected_at_ms": 1,
        "harness": "replica-live-load",
        "run_id": "schema-test-smoke",
        "sequence": 6,
        "label": "production-load",
        "coordinator_provider": "dynamodb",
        "redacted": true,
        "cwd": "writer-a",
        "args": [
            "external",
            "production-load",
            "--json"
        ],
        "result": {
            "schema": "replica.production-load",
            "data": {
                "profile": "production",
                "coordinator_provider": "dynamodb",
                "repository_bytes": 1048576,
                "file_count": 32,
                "xorb_count_source": "writer-store-delta",
                "xorb_count_before": 10,
                "xorb_count_after": 18,
                "xorb_count": 8,
                "refs_pushed": 4,
                "writer_regions": 2,
                "reader_regions": 2,
                "clone_count": 2,
                "hydrate_count": 2,
                "push_latency_ms": 9000,
                "push_latency_budget_ms": 60000,
                "read_latency_ms": 7500,
                "read_latency_budget_ms": 60000
            }
        }
    });
    assert_valid("replica.live-smoke.evidence", &instance);
}

#[test]
fn validate_replica_live_smoke_rejects_production_load_without_delta_source() {
    let instance = serde_json::json!({
        "schema": "replica.live-smoke.evidence",
        "version": "1.0",
        "collected_at_ms": 1,
        "harness": "replica-live-load",
        "run_id": "schema-test-smoke",
        "sequence": 7,
        "label": "production-load",
        "coordinator_provider": "dynamodb",
        "redacted": true,
        "cwd": "writer-a",
        "args": [
            "external",
            "production-load",
            "--json"
        ],
        "result": {
            "schema": "replica.production-load",
            "data": {
                "profile": "production",
                "coordinator_provider": "dynamodb",
                "repository_bytes": 1048576,
                "file_count": 32,
                "xorb_count_before": 10,
                "xorb_count_after": 18,
                "xorb_count": 8,
                "refs_pushed": 4,
                "writer_regions": 2,
                "reader_regions": 2,
                "clone_count": 2,
                "hydrate_count": 2,
                "push_latency_ms": 9000,
                "push_latency_budget_ms": 60000,
                "read_latency_ms": 7500,
                "read_latency_budget_ms": 60000
            }
        }
    });
    assert_invalid("replica.live-smoke.evidence", &instance);
}

// ---------------------------------------------------------------------------
// Command result payloads — types with private fields (use serde_json::json!)
// ---------------------------------------------------------------------------

#[test]
fn validate_du() {
    let instance = serde_json::json!({
        "cache_bytes": 1024,
        "cache_path": "/tmp/cache",
        "staging_bytes": 2048,
        "git_dir_bytes": 512,
        "hydrated_bytes": 4096,
        "hydrated_count": 2,
        "pointer_bytes": 256,
        "pointer_count": 3,
        "local_total_bytes": 7936,
        "remote_bytes": null,
        "remote_url": null
    });
    assert_valid("du", &instance);
}

#[test]
fn validate_doctor() {
    let instance = serde_json::json!({
        "checks": [{
            "name": "git-version",
            "status": "ok",
            "detail": "git 2.45.0"
        }],
        "summary": {
            "ok": 1,
            "warn": 0,
            "fail": 0
        }
    });
    assert_valid("doctor", &instance);
}

#[test]
fn validate_doctor_check_result() {
    let instance = serde_json::json!({
        "name": "git-version",
        "status": "ok",
        "detail": "git 2.45.0"
    });
    assert_valid("doctor.check_result", &instance);
}

#[test]
fn validate_doctor_check_status() {
    let instance = serde_json::json!("ok");
    assert_valid("doctor.check_status", &instance);
}

#[test]
fn validate_doctor_summary() {
    let instance = serde_json::json!({
        "ok": 5,
        "warn": 1,
        "fail": 0
    });
    assert_valid("doctor.summary", &instance);
}

#[test]
fn validate_ls_files() {
    let instance = serde_json::json!({
        "files": [{
            "name": "data/file.bin",
            "size": 4096,
            "hydrated": true,
            "oid": null
        }]
    });
    assert_valid("ls-files", &instance);
}

#[test]
fn validate_ls_files_entry() {
    let instance = serde_json::json!({
        "name": "data/file.bin",
        "size": 4096,
        "hydrated": false,
        "oid": "abc123def456"
    });
    assert_valid("ls-files.entry", &instance);
}

#[test]
fn validate_staging_stats() {
    let instance = serde_json::json!({
        "segments_sealed": 3,
        "current_segment_bytes": 1024,
        "total_staged_bytes": 4096,
        "live_bytes": 3072,
        "dead_bytes": 1024,
        "dead_ratio": 0.25,
        "chunk_count": 50,
        "file_count": 10,
        "files": []
    });
    assert_valid("staging.stats", &instance);
}

#[test]
fn validate_stat() {
    let instance = serde_json::json!({
        "segments_sealed": 2,
        "current_segment_bytes": 512,
        "total_staged_bytes": 2048,
        "live_bytes": 1536,
        "dead_bytes": 512,
        "dead_ratio": 0.25,
        "chunk_count": 20,
        "file_count": 5
    });
    assert_valid("stat", &instance);
}

#[test]
fn validate_stat_push_plan() {
    let instance = serde_json::json!({
        "format_version": 4,
        "verified_prepared_xorbs": true,
        "plan_files": 2,
        "invalid_plan_files": 0,
        "planned_file_bytes": 104857600,
        "planned_chunks": 128,
        "existing_chunks": 12,
        "prepared_xorbs": 4,
        "prepared_chunks": 116,
        "prepared_bytes": 94371840,
        "indexed_prepared_xorbs": 4,
        "orphaned_indexed_prepared_xorbs": 0,
        "invalid_indexed_prepared_xorbs": 0,
        "referenced_prepared_xorb_files": 4,
        "referenced_prepared_xorb_file_bytes": 94371840,
        "missing_prepared_xorb_files": 0,
        "mismatched_prepared_xorb_files": 0,
        "stale_prepared_xorb_files": 0,
        "stale_prepared_xorb_file_bytes": 0,
        "verified_prepared_xorb_files": 4,
        "verified_prepared_xorb_file_bytes": 94371840,
        "payload_hash_mismatched_prepared_xorb_files": 0,
        "corrupt_prepared_xorb_files": 0,
        "metadata_mismatched_prepared_xorb_files": 0
    });
    assert_valid("stat.push-plan", &instance);
}

#[test]
fn validate_status() {
    let instance = serde_json::json!({
        "total_tracked": 10,
        "hydrated": 6,
        "pointer": 3,
        "modified": 1,
        "files": [{
            "path": "data/file.bin",
            "state": "hydrated",
            "bytes": 4096
        }]
    });
    assert_valid("status", &instance);
}

#[test]
fn validate_status_entry() {
    let instance = serde_json::json!({
        "path": "data/file.bin",
        "state": "pointer",
        "bytes": 256
    });
    assert_valid("status.entry", &instance);
}

// ---------------------------------------------------------------------------
// Envelope types — validated with inline JSON since they are generic
// ---------------------------------------------------------------------------

#[test]
fn validate_envelope() {
    let instance = serde_json::json!({
        "schema": "status",
        "version": "1.0",
        "timestamp": "2026-01-01T00:00:00.000Z",
        "data": { "total_tracked": 1, "hydrated": 1, "pointer": 0, "modified": 0, "files": [] }
    });
    assert_valid("envelope", &instance);
}

#[test]
fn validate_event_envelope() {
    let instance = serde_json::json!({
        "schema": "add.event",
        "version": "1.0",
        "timestamp": "2026-01-01T00:00:00.000Z",
        "type": "progress",
        "data": {
            "operation": "staging",
            "current": 1,
            "total": 5,
            "bytes": 100,
            "total_bytes": 500,
            "rate_bytes_per_sec": 100.0
        }
    });
    assert_valid("event_envelope", &instance);
}

#[test]
fn validate_error_event_envelope() {
    let instance = serde_json::json!({
        "schema": "add.event",
        "version": "1.0",
        "timestamp": "2026-01-01T00:00:00.000Z",
        "type": "result",
        "error": {
            "code": "CRAB-E0090",
            "category": "cancelled",
            "message": "operation cancelled",
            "retryable": false,
            "details": {},
            "source_chain": []
        }
    });
    assert_valid("error_event_envelope", &instance);
}
