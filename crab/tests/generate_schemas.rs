//! Schema generator for structured CLI output types.
//!
//! Generates JSON Schema (draft-07) files in `crab/schemas/` from Rust
//! types via `schemars::schema_for!()`. Run explicitly with:
//!
//! ```sh
//! cargo test -p crab --test generate_schemas -- --ignored
//! ```
//!
//! Marked `#[ignore]` so it doesn't run in normal `cargo test` suites.
//! The generated files are committed to the repo; CI drift tests compare
//! them against freshly generated output.

mod schema_custom;

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
use crab::cmd::replica::{
    CertificationPayload, EvidenceVerifyPayload, LiveControlPlaneEvidencePayload,
    LiveSmokeEvidencePayload,
};
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

/// Directory where generated schemas are written, relative to the crate root.
fn schemas_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("schemas")
}

/// Write a JSON Schema to `crab/schemas/<name>.json`, pretty-printed.
fn write_schema(name: &str, schema: &schemars::schema::RootSchema) {
    let dir = schemas_dir();
    fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("failed to create {}: {e}", dir.display()));
    let path = dir.join(format!("{name}.json"));
    let generated = serde_json::to_value(schema)
        .unwrap_or_else(|e| panic!("failed to serialize schema {name}: {e}"));
    if let Ok(existing_text) = fs::read_to_string(&path) {
        if let Ok(existing) = serde_json::from_str::<serde_json::Value>(&existing_text) {
            if existing == generated {
                return;
            }
        }
    }

    let json = serde_json::to_string_pretty(&generated)
        .unwrap_or_else(|e| panic!("failed to serialize schema {name}: {e}"));
    fs::write(&path, format!("{json}\n"))
        .unwrap_or_else(|e| panic!("failed to write {}: {e}", path.display()));
}

#[test]
#[ignore]
fn regenerate_schemas() {
    // -- Envelope types (generic instantiated with serde_json::Value) --
    write_schema("envelope", &schema_for!(Envelope<serde_json::Value>));
    write_schema(
        "event_envelope",
        &schema_for!(EventEnvelope<serde_json::Value>),
    );
    write_schema("error_event_envelope", &schema_for!(ErrorEventEnvelope));

    // -- Error types --
    write_schema("error", &schema_for!(ErrorInfo));
    write_schema("error_category", &schema_for!(ErrorCategory));
    write_schema("error_source", &schema_for!(ErrorSource));

    // -- Event payload types (JSONL streaming) --
    write_schema("progress.event", &schema_for!(ProgressPayload));
    write_schema("file_done.event", &schema_for!(FileDonePayload));
    write_schema("xorb_done.event", &schema_for!(XorbDonePayload));
    write_schema("warning.event", &schema_for!(WarningPayload));
    write_schema("workflow.stage_result", &schema_for!(WorkflowStageResult));
    write_schema("workflow.stage.failed", &schema_for!(WorkflowStageFailed));
    write_schema("workflow.run", &schema_for!(WorkflowRunSummary));

    // -- Command result payloads --
    write_schema("add", &schema_for!(AddSummary));
    write_schema("audit.event", &schema_for!(AuditEvent));
    write_schema("audit.export", &schema_for!(AuditExportPayload));
    write_schema("audit.issue", &schema_for!(AuditIssue));
    write_schema("audit.log", &schema_for!(AuditLogPayload));
    write_schema("audit.outcome", &schema_for!(AuditOutcome));
    write_schema(
        "audit.remote_publish",
        &schema_for!(AuditRemotePublishPayload),
    );
    write_schema("audit.verify", &schema_for!(AuditVerifyPayload));
    write_schema("clone", &schema_for!(CloneSummary));
    write_schema("config.get", &schema_for!(ConfigGetPayload));
    write_schema("cost", &schema_for!(CostReport));
    write_schema("dehydrate", &schema_for!(DehydrateSummaryPayload));
    write_schema("doctor", &schema_for!(DoctorPayload));
    write_schema("doctor.check_result", &schema_for!(CheckResult));
    write_schema("doctor.check_status", &schema_for!(CheckStatus));
    write_schema("doctor.summary", &schema_for!(DoctorSummary));
    write_schema("du", &schema_for!(DuPayload));
    write_schema("env", &schema_for!(EnvPayload));
    write_schema("errors", &schema_for!(ErrorCatalogPayload));
    write_schema("errors.entry", &schema_for!(ErrorDocEntry));
    write_schema("errors.lookup", &schema_for!(ErrorDocPayload));
    write_schema("export.file", &schema_for!(ExportFileResult));
    write_schema("export.plan", &schema_for!(ExportPlanEvent));
    write_schema("export.summary", &schema_for!(ExportSummary));
    write_schema("fetch", &schema_for!(FetchSummary));
    write_schema("fsck", &schema_for!(FsckSummary));
    write_schema("gc", &schema_for!(GcSummary));
    write_schema("hydrate", &schema_for!(HydrateSummaryPayload));
    write_schema("import.plan", &schema_for!(ImportPlanSummary));
    write_schema("import.summary", &schema_for!(ImportSummary));
    write_schema("import.extension_bucket", &schema_for!(ExtensionBucket));
    write_schema("ls-files", &schema_for!(LsFilesPayload));
    write_schema("ls-files.entry", &schema_for!(LsFileEntry));
    write_schema("optimize.apply", &schema_for!(OptimizePayload));
    write_schema("optimize.plan", &schema_for!(OptimizePayload));
    write_schema(
        "optimize.xorbs.event",
        &schema_for!(OptimizeXorbsEventPayload),
    );
    write_schema("optimize.xorbs.plan", &schema_for!(OptimizeXorbsEstimate));
    write_schema("prune", &schema_for!(PruneSummary));
    write_schema("push", &schema_for!(PushSummaryPayload));
    write_schema("push.ref_outcome", &schema_for!(PushRefOutcome));
    write_schema("repack", &schema_for!(RepackSummary));
    write_schema("release.create", &schema_for!(ReleaseCreatePayload));
    write_schema("release.export", &schema_for!(ReleaseExportPayload));
    write_schema("release.list", &schema_for!(ReleaseListPayload));
    write_schema("release.manifest", &schema_for!(ReleaseManifest));
    write_schema("release.verify", &schema_for!(ReleaseVerifyPayload));
    write_schema("recover.apply", &schema_for!(RecoverApplyPayload));
    write_schema("recover.history.list", &schema_for!(HistoryListPayload));
    write_schema("recover.history.prune", &schema_for!(HistoryPrunePayload));
    write_schema(
        "recover.history.restore",
        &schema_for!(HistoryRestorePayload),
    );
    write_schema(
        "recover.history.verify",
        &schema_for!(HistoryVerificationPayload),
    );
    write_schema("recover.plan", &schema_for!(RecoverPlanPayload));
    write_schema("recover.status", &schema_for!(RecoverStatusPayload));
    write_schema(
        "replica.evidence.verify",
        &schema_for!(EvidenceVerifyPayload),
    );
    write_schema("replica.certification", &schema_for!(CertificationPayload));
    write_schema(
        "replica.live-control-plane.evidence",
        &schema_for!(LiveControlPlaneEvidencePayload),
    );
    let live_smoke_schema =
        schema_custom::add_live_smoke_evidence_constraints(schema_for!(LiveSmokeEvidencePayload));
    write_schema("replica.live-smoke.evidence", &live_smoke_schema);
    write_schema("staging.stats", &schema_for!(StagingStatsPayload));
    write_schema("stat", &schema_for!(StatPayload));
    write_schema("stat.classes", &schema_for!(StatClassesPayload));
    write_schema("stat.push-plan", &schema_for!(StatPushPlanPayload));
    write_schema("status", &schema_for!(StatusPayload));
    write_schema("status.entry", &schema_for!(StatusEntry));
    write_schema("tier.event", &schema_for!(TierEventPayload));
    write_schema("tier.plan", &schema_for!(TierPlanPayload));
    write_schema("track", &schema_for!(TrackPayload));
    write_schema("track.pattern", &schema_for!(TrackPattern));
    write_schema("version", &schema_for!(VersionPayload));
}
