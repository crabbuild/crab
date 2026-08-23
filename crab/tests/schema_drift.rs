//! CI drift test: asserts that committed JSON schemas in `crab/schemas/`
//! match the schemas generated from Rust types via `schemars::schema_for!()`.
//!
//! This test runs in normal `cargo test` (NOT `#[ignore]`). If it fails,
//! regenerate schemas with:
//!
//! ```sh
//! cargo test -p crab --test generate_schemas -- --ignored
//! ```

use std::fs;
use std::path::PathBuf;

use schemars::schema_for;

use crab::audit::{
    AuditEvent, AuditExportPayload, AuditIssue, AuditLogPayload, AuditOutcome,
    AuditRemotePublishPayload, AuditVerifyPayload,
};

// -- Envelope and core output types --
use crab::core::output::{
    Envelope, ErrorCategory, ErrorEventEnvelope, ErrorInfo, ErrorSource, EventEnvelope,
    FileDonePayload, ProgressPayload, WarningPayload, WorkflowRunSummary, WorkflowStageFailed,
    WorkflowStageResult, XorbDonePayload,
};

// -- Command payload types --
use crab::cmd::add::AddSummary;
use crab::cmd::clone::CloneSummary;
use crab::cmd::config::ConfigGetPayload;
use crab::cmd::dehydrate::DehydrateSummaryPayload;
use crab::cmd::doctor::{CheckResult, CheckStatus, DoctorPayload, DoctorSummary};
use crab::cmd::du::DuPayload;
use crab::cmd::env::EnvPayload;
use crab::cmd::errors::{ErrorCatalogPayload, ErrorDocEntry, ErrorDocPayload};
use crab::cmd::export::{ExportFileResult, ExportPlanEvent, ExportSummary};
use crab::cmd::fetch::FetchSummary;
use crab::cmd::fsck::FsckSummary;
use crab::cmd::gc::GcSummary;
use crab::cmd::history_recovery::{
    HistoryListPayload, HistoryPrunePayload, HistoryRestorePayload, HistoryVerificationPayload,
};
use crab::cmd::hydrate::HydrateSummaryPayload;
use crab::cmd::ls_files::{LsFileEntry, LsFilesPayload};
use crab::cmd::optimize::OptimizePayload;
use crab::cmd::optimize::xorbs::OptimizeXorbsEventPayload;
use crab::cmd::prune::PruneSummary;
use crab::cmd::push::{PushRefOutcome, PushSummaryPayload};
use crab::cmd::recover::{RecoverApplyPayload, RecoverPlanPayload, RecoverStatusPayload};
use crab::cmd::release::{
    ReleaseCreatePayload, ReleaseExportPayload, ReleaseListPayload, ReleaseVerifyPayload,
};
use crab::cmd::repack::RepackSummary;
use crab::cmd::staging::StagingStatsPayload;
use crab::cmd::stat::{StatClassesPayload, StatPayload, StatPushPlanPayload};
use crab::cmd::status::{StatusEntry, StatusPayload};
use crab::cmd::tier::{TierEventPayload, TierPlanPayload};
use crab::cmd::track::{TrackPattern, TrackPayload};
use crab::cmd::version::VersionPayload;
use crab::cost::report::CostReport;
use crab::import::{ExtensionBucket, ImportPlanSummary, ImportSummary};
use crab::optimize::xorbs::planner::OptimizeXorbsEstimate;
use crab::release::ReleaseManifest;

fn schemas_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("schemas")
}

/// Generate a schema and return it as a `serde_json::Value` for structural comparison.
fn schema_value<T: schemars::JsonSchema>() -> serde_json::Value {
    let root = schema_for!(T);
    serde_json::to_value(&root).expect("schema serialization should not fail")
}

#[test]
fn schemas_up_to_date() {
    let schemas: Vec<(&str, serde_json::Value)> = vec![
        // Envelope types
        ("envelope", schema_value::<Envelope<serde_json::Value>>()),
        (
            "event_envelope",
            schema_value::<EventEnvelope<serde_json::Value>>(),
        ),
        ("error_event_envelope", schema_value::<ErrorEventEnvelope>()),
        // Error types
        ("error", schema_value::<ErrorInfo>()),
        ("error_category", schema_value::<ErrorCategory>()),
        ("error_source", schema_value::<ErrorSource>()),
        // Event payload types
        ("progress.event", schema_value::<ProgressPayload>()),
        ("file_done.event", schema_value::<FileDonePayload>()),
        ("xorb_done.event", schema_value::<XorbDonePayload>()),
        ("warning.event", schema_value::<WarningPayload>()),
        (
            "workflow.stage_result",
            schema_value::<WorkflowStageResult>(),
        ),
        (
            "workflow.stage.failed",
            schema_value::<WorkflowStageFailed>(),
        ),
        ("workflow.run", schema_value::<WorkflowRunSummary>()),
        // Command result payloads
        ("add", schema_value::<AddSummary>()),
        ("audit.event", schema_value::<AuditEvent>()),
        ("audit.export", schema_value::<AuditExportPayload>()),
        ("audit.issue", schema_value::<AuditIssue>()),
        ("audit.log", schema_value::<AuditLogPayload>()),
        ("audit.outcome", schema_value::<AuditOutcome>()),
        (
            "audit.remote_publish",
            schema_value::<AuditRemotePublishPayload>(),
        ),
        ("audit.verify", schema_value::<AuditVerifyPayload>()),
        ("clone", schema_value::<CloneSummary>()),
        ("config.get", schema_value::<ConfigGetPayload>()),
        ("cost", schema_value::<CostReport>()),
        ("dehydrate", schema_value::<DehydrateSummaryPayload>()),
        ("doctor", schema_value::<DoctorPayload>()),
        ("doctor.check_result", schema_value::<CheckResult>()),
        ("doctor.check_status", schema_value::<CheckStatus>()),
        ("doctor.summary", schema_value::<DoctorSummary>()),
        ("du", schema_value::<DuPayload>()),
        ("env", schema_value::<EnvPayload>()),
        ("errors", schema_value::<ErrorCatalogPayload>()),
        ("errors.entry", schema_value::<ErrorDocEntry>()),
        ("errors.lookup", schema_value::<ErrorDocPayload>()),
        ("export.file", schema_value::<ExportFileResult>()),
        ("export.plan", schema_value::<ExportPlanEvent>()),
        ("export.summary", schema_value::<ExportSummary>()),
        ("fetch", schema_value::<FetchSummary>()),
        ("fsck", schema_value::<FsckSummary>()),
        ("gc", schema_value::<GcSummary>()),
        ("hydrate", schema_value::<HydrateSummaryPayload>()),
        ("import.plan", schema_value::<ImportPlanSummary>()),
        ("import.summary", schema_value::<ImportSummary>()),
        ("import.extension_bucket", schema_value::<ExtensionBucket>()),
        ("ls-files", schema_value::<LsFilesPayload>()),
        ("ls-files.entry", schema_value::<LsFileEntry>()),
        ("optimize.apply", schema_value::<OptimizePayload>()),
        ("optimize.plan", schema_value::<OptimizePayload>()),
        (
            "optimize.xorbs.event",
            schema_value::<OptimizeXorbsEventPayload>(),
        ),
        (
            "optimize.xorbs.plan",
            schema_value::<OptimizeXorbsEstimate>(),
        ),
        ("prune", schema_value::<PruneSummary>()),
        ("push", schema_value::<PushSummaryPayload>()),
        ("push.ref_outcome", schema_value::<PushRefOutcome>()),
        ("repack", schema_value::<RepackSummary>()),
        ("release.create", schema_value::<ReleaseCreatePayload>()),
        ("release.export", schema_value::<ReleaseExportPayload>()),
        ("release.list", schema_value::<ReleaseListPayload>()),
        ("release.manifest", schema_value::<ReleaseManifest>()),
        ("release.verify", schema_value::<ReleaseVerifyPayload>()),
        ("recover.apply", schema_value::<RecoverApplyPayload>()),
        ("recover.history.list", schema_value::<HistoryListPayload>()),
        (
            "recover.history.prune",
            schema_value::<HistoryPrunePayload>(),
        ),
        (
            "recover.history.restore",
            schema_value::<HistoryRestorePayload>(),
        ),
        (
            "recover.history.verify",
            schema_value::<HistoryVerificationPayload>(),
        ),
        ("recover.plan", schema_value::<RecoverPlanPayload>()),
        ("recover.status", schema_value::<RecoverStatusPayload>()),
        ("staging.stats", schema_value::<StagingStatsPayload>()),
        ("stat", schema_value::<StatPayload>()),
        ("stat.classes", schema_value::<StatClassesPayload>()),
        ("stat.push-plan", schema_value::<StatPushPlanPayload>()),
        ("status", schema_value::<StatusPayload>()),
        ("status.entry", schema_value::<StatusEntry>()),
        ("tier.event", schema_value::<TierEventPayload>()),
        ("tier.plan", schema_value::<TierPlanPayload>()),
        ("track", schema_value::<TrackPayload>()),
        ("track.pattern", schema_value::<TrackPattern>()),
        ("version", schema_value::<VersionPayload>()),
    ];

    let dir = schemas_dir();
    let mut failures: Vec<String> = Vec::new();

    for (name, generated) in &schemas {
        let path = dir.join(format!("{name}.json"));

        let committed_text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) => {
                failures.push(format!(
                    "{name}: missing committed schema at {}: {e}",
                    path.display()
                ));
                continue;
            }
        };

        let committed: serde_json::Value = match serde_json::from_str(&committed_text) {
            Ok(v) => v,
            Err(e) => {
                failures.push(format!("{name}: committed schema is invalid JSON: {e}"));
                continue;
            }
        };

        if *generated != committed {
            failures.push(format!(
                "{name}: generated schema differs from committed {}",
                path.display()
            ));
        }
    }

    if !failures.is_empty() {
        let list = failures.join("\n  - ");
        panic!(
            "Schema drift detected! The following schemas are out of date:\n  \
             - {list}\n\n\
             Run `cargo test -p crab --test generate_schemas -- --ignored` to regenerate schemas."
        );
    }
}
