//! Structured summary + streaming-event payload types for
//! `crab import`.
//!
//! Three roles live here:
//!
//! 1. [`ImportSummary`] — the terminal success envelope for
//!    `--json` output and the source of truth the coordinator
//!    hands back to the CLI.
//! 2. [`ImportPlanSummary`] — dry-run analysis returned by the
//!    planner when no mutations are allowed. Reuses the same
//!    `files_imported` / `bytes_source` vocabulary as
//!    [`ImportSummary`] but adds pre-ingest fields the real
//!    summary doesn't have (extension histogram, LFS-pointer
//!    count, prefix collision warnings).
//! 3. Streaming JSONL event payloads that the pipeline sinks
//!    emit through [`crate::core::output::JsonlStream`] — one
//!    per pipeline stage (`enumerate.event`, `stage.event`,
//!    `assemble.event`, `publish.event`).
//!
//! The split keeps the coordinator lean — it owns pipeline
//! wiring, and this module owns the wire format.

use std::collections::BTreeMap;
use std::io::Stdout;
use std::sync::Mutex;

use schemars::JsonSchema;
use serde::Serialize;

use crate::core::output::JsonlStream;
use crate::import::assemble::{AssembleEvent, AssembleProgressSink};
use crate::import::enumerate::{EnumerateEvent, ProgressSink};
use crate::import::ingest::{IngestProgressSink, StageEvent};

/// Versioning mode reported by the detect stage, exposed on
/// [`ImportSummary`] and in the [`ImportPlanSummary`].
///
/// Kept serialisable so operators can filter summaries by mode
/// without parsing the source URL back apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema, Default)]
#[serde(rename_all = "snake_case")]
pub enum SummaryVersioning {
    /// Non-versioned bucket; exactly one commit reflects the
    /// current state.
    #[default]
    Flat,
    /// Versioned bucket; history landed as N time-windowed
    /// commits.
    Versioned,
    /// `--at <timestamp>` single-snapshot import.
    SingleSnapshot,
}

/// Optional `--since` / `--until` range surfaced in the summary
/// when at least one bound is set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct HistoryRange {
    /// RFC3339 lower bound. Empty string when unset.
    pub since: String,
    /// RFC3339 upper bound. Empty string when unset.
    pub until: String,
}

/// Rolled-up counters and identifiers from a single
/// `crab import` run.
///
/// This is the terminal payload of `--json` and the final
/// `import.summary` event under `--jsonl`. Every field is
/// derivable from the per-stage stats the coordinator folds
/// together; the ordering here mirrors requirement I10.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct ImportSummary {
    pub source_url: String,
    pub target_url: String,
    pub versioning: SummaryVersioning,
    pub files_imported: u64,
    pub versions_imported: u64,
    pub commits_created: u64,
    pub files_skipped: u64,
    pub files_failed: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub lfs_resolved: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub lfs_skipped: u64,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub lfs_failed: u64,
    pub bytes_source: u64,
    pub bytes_staged: u64,
    pub bytes_uploaded: u64,
    pub same_bucket: bool,
    pub duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_commit_oid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_commit_oid: Option<String>,
    pub branch: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub history_range: Option<HistoryRange>,
    /// `true` when the run was `--dry-run` — no mutations
    /// happened. A dry-run summary still carries
    /// `files_imported` / `bytes_source` (from the plan) but
    /// zeros out `bytes_staged` / `bytes_uploaded` /
    /// `commits_created` because those require real work.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dry_run: bool,
    /// Manifest preview populated only in dry-run mode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub plan: Option<ImportPlanSummary>,
}

impl Default for ImportSummary {
    fn default() -> Self {
        Self {
            source_url: String::new(),
            target_url: String::new(),
            versioning: SummaryVersioning::Flat,
            files_imported: 0,
            versions_imported: 0,
            commits_created: 0,
            files_skipped: 0,
            files_failed: 0,
            lfs_resolved: 0,
            lfs_skipped: 0,
            lfs_failed: 0,
            bytes_source: 0,
            bytes_staged: 0,
            bytes_uploaded: 0,
            same_bucket: false,
            duration_ms: 0,
            first_commit_oid: None,
            head_commit_oid: None,
            branch: String::new(),
            history_range: None,
            dry_run: false,
            plan: None,
        }
    }
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

/// One bucket in the extension histogram reported by a dry-run.
///
/// `count` and `total_bytes` are across every matching entry
/// before deduplication — i.e. summed source-object sizes, not
/// post-CDC staging bytes. That's the honest "how much is in
/// this bucket?" answer for a dry-run; real byte accounting
/// lands after ingest runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct ExtensionBucket {
    /// Lowercase file extension (no leading dot). `""` for
    /// entries without an extension.
    pub extension: String,
    /// Number of matching entries across all windows.
    pub count: u64,
    /// Summed source-object bytes.
    pub total_bytes: u64,
}

/// Dry-run manifest preview: what the real run would have done
/// if the user hadn't passed `--dry-run`.
///
/// Kept separate from the main [`ImportSummary`] body so the
/// dry-run-only fields can grow without disturbing the terminal
/// schema for real imports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema, Default)]
pub struct ImportPlanSummary {
    /// Extension histogram over the enumerated entries.
    pub extension_histogram: Vec<ExtensionBucket>,
    /// Total files the real run would have attempted to ingest.
    pub files_total: u64,
    /// Total source-object bytes across those files.
    pub bytes_total: u64,
    /// Number of entries the ingest guard would have flagged as
    /// LFS pointer blobs.
    pub lfs_pointer_count: u64,
    /// `true` when `--from` and `--to` resolve to the same
    /// physical bucket. Informational only; affects nothing
    /// about the plan itself.
    pub same_bucket: bool,
    /// Collision / safety warnings gathered during preflight.
    /// Empty when the plan is clean. Rendered verbatim by the
    /// text mode.
    pub collision_warnings: Vec<String>,
    /// Versioning mode the detect stage reported.
    pub versioning: SummaryVersioning,
    /// Number of commits the window planner produced. For
    /// `Flat` / `SingleSnapshot` always `1`.
    pub planned_commit_count: u64,
}

// ── Histogram builder ───────────────────────────────────────────

/// Build the extension histogram from a stream of
/// `(relative_path, size)` tuples. Kept separate from
/// `ImportPlanSummary` so tests can drive it without plumbing
/// a whole `Vec<ImportEntry>` through.
#[must_use]
pub fn build_extension_histogram<'a, I>(entries: I) -> Vec<ExtensionBucket>
where
    I: IntoIterator<Item = (&'a str, u64)>,
{
    let mut buckets: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    for (path, size) in entries {
        let ext = extension_of(path).unwrap_or_default();
        let slot = buckets.entry(ext).or_insert((0, 0));
        slot.0 = slot.0.saturating_add(1);
        slot.1 = slot.1.saturating_add(size);
    }
    buckets
        .into_iter()
        .map(|(extension, (count, total_bytes))| ExtensionBucket {
            extension,
            count,
            total_bytes,
        })
        .collect()
}

fn extension_of(relative_path: &str) -> Option<String> {
    let name = relative_path.rsplit('/').next().unwrap_or(relative_path);
    let (_, ext) = name.rsplit_once('.')?;
    if ext.is_empty() {
        return None;
    }
    Some(ext.to_ascii_lowercase())
}

// ── JSONL event payloads ────────────────────────────────────────

/// Payload for an `enumerate.event` in JSONL mode. Derived from
/// the enumerate stage's internal event; this is the stable wire
/// format.
#[derive(Debug, Serialize, JsonSchema)]
pub struct EnumerateEventPayload {
    pub done: u64,
    pub kept: u64,
    pub versioning: bool,
    pub terminal: bool,
}

/// Payload for a `stage.event` in JSONL mode.
#[derive(Debug, Serialize, JsonSchema)]
pub struct StageEventPayload {
    pub relative_path: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub version_id: String,
    pub size: u64,
    pub duration_ms: u64,
}

/// Payload for an `assemble.event` in JSONL mode.
#[derive(Debug, Serialize, JsonSchema)]
pub struct AssembleEventPayload {
    pub window_start: i64,
    pub window_end: i64,
    pub commit_oid: String,
    pub files_added: u64,
    pub files_modified: u64,
    pub files_deleted: u64,
}

/// Payload for a `publish.event` in JSONL mode. The publish
/// stage emits a single event today (`phase = "start"` | `"done"`);
/// richer per-xorb events land in a follow-up.
#[derive(Debug, Serialize, JsonSchema)]
pub struct PublishEventPayload {
    pub phase: String,
    pub branch: String,
    pub head_commit_oid: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes_uploaded: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refs_pushed: Option<u64>,
}

// ── JSONL progress sinks ────────────────────────────────────────

/// JSONL-streaming [`ProgressSink`] for enumerate events.
///
/// The underlying stream is shared (and rate-limited internally)
/// so this sink can be constructed once per stage and dropped at
/// the stage boundary.
pub struct JsonlEnumerateSink<'a> {
    stream: &'a Mutex<JsonlStream<Stdout>>,
}

impl<'a> JsonlEnumerateSink<'a> {
    #[must_use]
    pub fn new(stream: &'a Mutex<JsonlStream<Stdout>>) -> Self {
        Self { stream }
    }
}

impl ProgressSink for JsonlEnumerateSink<'_> {
    fn enumerate_event(&mut self, event: EnumerateEvent) {
        let payload = EnumerateEventPayload {
            done: event.done,
            kept: event.kept,
            versioning: event.versioning,
            terminal: event.terminal,
        };
        if let Ok(mut guard) = self.stream.lock() {
            // Use `emit_file_done` semantics: never rate-limited,
            // tagged as a non-progress event. The schema field
            // carries `enumerate.event` via the stream header,
            // and the envelope `type` is set by the writer.
            guard.emit_file_done(payload);
        }
    }
}

/// JSONL-streaming [`IngestProgressSink`] for `stage.event`
/// events.
pub struct JsonlStageSink<'a> {
    stream: &'a Mutex<JsonlStream<Stdout>>,
}

impl<'a> JsonlStageSink<'a> {
    #[must_use]
    pub fn new(stream: &'a Mutex<JsonlStream<Stdout>>) -> Self {
        Self { stream }
    }
}

impl IngestProgressSink for JsonlStageSink<'_> {
    fn stage_event(&mut self, event: &StageEvent<'_>) {
        let payload = StageEventPayload {
            relative_path: event.relative_path.to_owned(),
            version_id: event.version_id.to_owned(),
            size: event.size,
            duration_ms: event.duration_ms,
        };
        if let Ok(mut guard) = self.stream.lock() {
            guard.emit_file_done(payload);
        }
    }
}

/// JSONL-streaming [`AssembleProgressSink`] for `assemble.event`
/// events.
pub struct JsonlAssembleSink<'a> {
    stream: &'a Mutex<JsonlStream<Stdout>>,
}

impl<'a> JsonlAssembleSink<'a> {
    #[must_use]
    pub fn new(stream: &'a Mutex<JsonlStream<Stdout>>) -> Self {
        Self { stream }
    }
}

impl AssembleProgressSink for JsonlAssembleSink<'_> {
    fn assemble_event(&mut self, event: &AssembleEvent) {
        let payload = AssembleEventPayload {
            window_start: event.window_start,
            window_end: event.window_end,
            commit_oid: event.commit_oid.clone(),
            files_added: event.files_added,
            files_modified: event.files_modified,
            files_deleted: event.files_deleted,
        };
        if let Ok(mut guard) = self.stream.lock() {
            guard.emit_file_done(payload);
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]
mod tests {
    use super::*;

    #[test]
    fn extension_of_lowercases_and_tolerates_missing() {
        assert_eq!(extension_of("a/b/c.BIN").as_deref(), Some("bin"));
        assert_eq!(extension_of("noext").as_deref(), None);
        assert_eq!(extension_of("dot.").as_deref(), None);
        assert_eq!(extension_of(".hidden").as_deref(), Some("hidden"));
    }

    #[test]
    fn extension_histogram_groups_by_extension_and_sums_bytes() {
        let entries = vec![
            ("a.bin", 100u64),
            ("b.bin", 200),
            ("c.txt", 50),
            ("no-ext", 25),
            ("d.Bin", 75),
        ];
        let hist = build_extension_histogram(entries.iter().map(|(p, s)| (*p, *s)));

        // BTreeMap is ordered: "" < "bin" < "txt".
        assert_eq!(hist.len(), 3);
        assert_eq!(hist[0].extension, "");
        assert_eq!(hist[0].count, 1);
        assert_eq!(hist[0].total_bytes, 25);
        assert_eq!(hist[1].extension, "bin");
        assert_eq!(hist[1].count, 3);
        assert_eq!(hist[1].total_bytes, 375);
        assert_eq!(hist[2].extension, "txt");
        assert_eq!(hist[2].count, 1);
        assert_eq!(hist[2].total_bytes, 50);
    }

    #[test]
    fn import_summary_serializes_dry_run_with_plan() {
        let summary = ImportSummary {
            source_url: "s3://src/".into(),
            target_url: "s3://dst/".into(),
            versioning: SummaryVersioning::Flat,
            dry_run: true,
            plan: Some(ImportPlanSummary {
                files_total: 3,
                bytes_total: 1024,
                same_bucket: false,
                planned_commit_count: 1,
                versioning: SummaryVersioning::Flat,
                ..Default::default()
            }),
            branch: "main".into(),
            ..Default::default()
        };
        let json = serde_json::to_value(&summary).unwrap();
        assert_eq!(json["dry_run"], true);
        assert_eq!(json["plan"]["files_total"], 3);
        assert_eq!(json["plan"]["planned_commit_count"], 1);
        assert_eq!(json["versioning"], "flat");
    }

    #[test]
    fn import_summary_omits_dry_run_when_false() {
        let summary = ImportSummary {
            source_url: "s3://src/".into(),
            target_url: "s3://dst/".into(),
            versioning: SummaryVersioning::Versioned,
            branch: "main".into(),
            ..Default::default()
        };
        let json = serde_json::to_value(&summary).unwrap();
        // skip_serializing_if = Not::not keeps the field out of
        // the JSON when false.
        assert!(json.get("dry_run").is_none());
        assert!(json.get("plan").is_none());
        assert!(json.get("lfs_resolved").is_none());
        assert!(json.get("lfs_skipped").is_none());
        assert!(json.get("lfs_failed").is_none());
    }

    #[test]
    fn import_summary_serializes_nonzero_lfs_counts() {
        let summary = ImportSummary {
            source_url: "s3://src/".into(),
            target_url: "s3://dst/".into(),
            versioning: SummaryVersioning::Flat,
            branch: "main".into(),
            lfs_resolved: 2,
            lfs_skipped: 1,
            lfs_failed: 1,
            ..Default::default()
        };
        let json = serde_json::to_value(&summary).unwrap();
        assert_eq!(json["lfs_resolved"], 2);
        assert_eq!(json["lfs_skipped"], 1);
        assert_eq!(json["lfs_failed"], 1);
    }
}
