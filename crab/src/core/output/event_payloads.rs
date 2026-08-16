//! Standard event payload types for `--jsonl` streaming commands.
//!
//! These structs are the `data` field inside [`super::EventEnvelope`]
//! for the four non-terminal event types (`progress`, `file_done`,
//! `xorb_done`, `warning`). The terminal `result` event carries a
//! command-specific payload defined per command.
//!
//! Workflow-layer schemas live here too: the single-envelope
//! [`WorkflowStageResult`] (schema `workflow.stage_result`), the
//! seven per-state event payloads under `workflow.stage.*`, plus
//! the DAG-run [`WorkflowRunSummary`] (schema `workflow.run`) and
//! [`WorkflowStageNotStarted`] event payload. Schema names and the
//! version string (`"1.0"`) are defined alongside the payload
//! structs so downstream consumers have a single source of truth.

use std::path::PathBuf;

use schemars::JsonSchema;
use serde::Serialize;

/// Schema name for operation phase timing events.
pub const PERF_PHASE_SCHEMA: &str = "perf.phase";

/// Schema version for [`PerfPhasePayload`].
pub const PERF_PHASE_SCHEMA_VERSION: &str = "1.0";

/// Stable phase timing payload for clone/fetch/pull/push performance analysis.
#[derive(Debug, Clone, Serialize, JsonSchema, PartialEq, Eq)]
pub struct PerfPhasePayload {
    /// High-level operation name, e.g. `"clone"`, `"fetch"`, `"push"`.
    pub operation: String,
    /// Phase name within the operation, e.g. `"pack_fetch"`.
    pub phase: String,
    /// Wall-clock duration of the phase in milliseconds.
    pub elapsed_ms: u64,
    /// Bytes consumed by the phase, when known.
    pub bytes_in: u64,
    /// Bytes produced or transferred by the phase, when known.
    pub bytes_out: u64,
    /// Number of files, packs, xorbs, shards, or refs handled by the phase.
    pub item_count: u64,
    /// Best-effort resident set size sample in bytes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub peak_rss_bytes: Option<u64>,
}

/// Periodic throughput / progress update.
///
/// Emitted at most once per 250 ms by [`super::JsonlStream::emit_progress`].
#[derive(Debug, Serialize, JsonSchema)]
pub struct ProgressPayload {
    /// Human-readable operation label, e.g. `"uploading"`, `"hydrating"`.
    pub operation: String,
    /// Items completed so far.
    pub current: u64,
    /// Total items expected (0 when unknown).
    pub total: u64,
    /// Bytes transferred so far.
    pub bytes: u64,
    /// Total bytes expected (0 when unknown).
    pub total_bytes: u64,
    /// Current transfer rate in bytes per second.
    pub rate_bytes_per_sec: f64,
    /// Number of xorbs newly produced during the current push pipeline.
    ///
    /// Only meaningful for push `packing` events; omitted from
    /// other operations to keep the JSONL line small. Consumers
    /// summarising "xorbs created" should prefer this over `current`
    /// (which is a file count during packing) and over the
    /// `uploading` phase's `current` (xorbs *uploaded*, which can
    /// be lower when uploads dedupe against the remote store).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub xorbs_produced: Option<u64>,
}

/// Completion event for a single file.
#[derive(Debug, Serialize, JsonSchema)]
pub struct FileDonePayload {
    /// Relative path of the file.
    pub path: String,
    /// Size of the file in bytes.
    pub bytes: u64,
    /// Wall-clock duration of the operation in milliseconds.
    pub duration_ms: u64,
    /// Outcome: `"ok"`, `"failed"`, or `"skipped"`.
    pub status: String,
}

/// Completion event for a single xorb upload or download.
#[derive(Debug, Serialize, JsonSchema)]
pub struct XorbDonePayload {
    /// Hex-encoded content hash of the xorb.
    pub hash: String,
    /// Uncompressed size in bytes.
    pub bytes: u64,
    /// Compressed (on-wire) size in bytes.
    pub compressed_bytes: u64,
    /// Outcome: `"ok"`, `"failed"`, or `"skipped"`.
    pub status: String,
}

/// Non-fatal warning emitted during a streaming operation.
#[derive(Debug, Serialize, JsonSchema)]
pub struct WarningPayload {
    /// Warning code, e.g. a `CRAB-E####` code or a command-specific tag.
    pub code: String,
    /// Human-readable warning message.
    pub message: String,
    /// Optional file path associated with the warning.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

// ---------------------------------------------------------------------------
// Workflow layer schemas
// ---------------------------------------------------------------------------

/// Shared schema version for every workflow-layer output envelope.
/// Bumped in lockstep with `StageCacheEntry.schema_version`.
pub const WORKFLOW_SCHEMA_VERSION: &str = "1.0";

/// Schema name for the single-envelope `workflow.stage_result`
/// terminal payload emitted by `crab run` (single-stage mode).
pub const WORKFLOW_STAGE_RESULT_SCHEMA: &str = "workflow.stage_result";

/// Schema name for the terminal `workflow.run` summary emitted at
/// the end of a multi-stage DAG run. One envelope per run,
/// enumerating succeeded / failed / not-started stages, per-stage
/// `WorkflowStageResult` records, and wall-clock duration.
pub const WORKFLOW_RUN_SCHEMA: &str = "workflow.run";

/// Schema name for the `workflow.stage.not_started` event: emitted
/// when a stage is skipped during a DAG walk because a prior stage
/// failed (`reason = "prior_stage_failed"`) or because one of its
/// upstream producers failed (`reason = "upstream_failed"`).
pub const WORKFLOW_STAGE_NOT_STARTED_SCHEMA: &str = "workflow.stage.not_started";

/// Schema name for the `workflow.stage.started` event: emitted once
/// per stage attempt immediately after journal state `Resolved`.
pub const WORKFLOW_STAGE_STARTED_SCHEMA: &str = "workflow.stage.started";

/// Schema name for the `workflow.stage.cache_checked` event: emitted
/// after the local/remote cache probe completes.
pub const WORKFLOW_STAGE_CACHE_CHECKED_SCHEMA: &str = "workflow.stage.cache_checked";

/// Schema name for the `workflow.stage.running` event: emitted right
/// after the user command is spawned on the miss path.
pub const WORKFLOW_STAGE_RUNNING_SCHEMA: &str = "workflow.stage.running";

/// Schema name for the `workflow.stage.produced` event: emitted after
/// the user command exits successfully.
pub const WORKFLOW_STAGE_PRODUCED_SCHEMA: &str = "workflow.stage.produced";

/// Schema name for the `workflow.stage.hashed` event: emitted after
/// declared outs are hashed.
pub const WORKFLOW_STAGE_HASHED_SCHEMA: &str = "workflow.stage.hashed";

/// Schema name for the `workflow.stage.committed` event: emitted when
/// a stage attempt reaches the terminal `Committed` state.
pub const WORKFLOW_STAGE_COMMITTED_SCHEMA: &str = "workflow.stage.committed";

/// Schema name for the `workflow.stage.failed` event: emitted when a
/// stage attempt reaches the terminal `Failed` state.
pub const WORKFLOW_STAGE_FAILED_SCHEMA: &str = "workflow.stage.failed";

/// Schema name for the `workflow.stage.retry` event: emitted when a
/// failed stage attempt qualifies for retry per the stage's retry
/// policy. Carries the attempt number, failure reason, and computed
/// backoff duration.
pub const WORKFLOW_STAGE_RETRY_SCHEMA: &str = "workflow.stage.retry";

/// Schema name for the `workflow.watch.triggered` event: emitted when
/// watch mode detects dep file changes and begins a re-execution cycle.
pub const WORKFLOW_WATCH_TRIGGERED_SCHEMA: &str = "workflow.watch.triggered";

/// One output recorded in a [`WorkflowStageResult`] or
/// [`WorkflowStageHashed`] payload.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct WorkflowStageOut {
    /// Repo-relative path of the output.
    pub path: PathBuf,
    /// `"b3:" + lowercase_hex(32 bytes)` content hash.
    pub file_hash: String,
    /// Size of the output in bytes.
    pub size: u64,
}

/// Single-envelope terminal payload for `crab run` (single-stage
/// mode). Emitted under `--json` as `Envelope<WorkflowStageResult>`
/// and under `--jsonl` as the final `result` event.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct WorkflowStageResult {
    /// Stage name as declared on the CLI.
    pub stage_name: String,
    /// Lowercase hex `stage_hash` (64 chars).
    pub stage_hash: String,
    /// Whether the result came from the cache.
    pub cache_hit: bool,
    /// Wall-clock duration of the attempt in milliseconds.
    pub duration_ms: u64,
    /// Declared outputs with their content hashes and sizes.
    pub outs: Vec<WorkflowStageOut>,
    /// Number of attempts it took to reach success (always `>= 1`).
    pub attempts: u32,
    /// Set to `true` when a stage has `side_effects: true` but no
    /// `on_cache_hit` hook — the side effects were skipped on this
    /// cache hit.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub side_effects_skipped: bool,
    /// Cache hit source: `"Local"`, `"Remote"`, or `"Execution"`.
    /// Present only when the stage was served from cache.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// Payload for `workflow.stage.started`.
#[derive(Debug, Serialize, JsonSchema)]
pub struct WorkflowStageStarted {
    pub stage: String,
    pub stage_hash: String,
    pub attempt: u32,
    /// Milliseconds elapsed since the DAG run started.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
}

/// Payload for `workflow.stage.cache_checked`.
#[derive(Debug, Serialize, JsonSchema)]
pub struct WorkflowStageCacheChecked {
    pub stage: String,
    pub stage_hash: String,
    pub hit: bool,
    /// `"local"`, `"remote"`, or `"none"`.
    pub hit_source: String,
    /// Milliseconds elapsed since the DAG run started.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
}

/// Payload for `workflow.stage.running`.
#[derive(Debug, Serialize, JsonSchema)]
pub struct WorkflowStageRunning {
    pub stage: String,
    pub stage_hash: String,
    pub pid: u32,
    pub attempt: u32,
    /// RFC3339 UTC timestamp with millisecond precision.
    pub started_at: String,
}

/// Payload for `workflow.stage.produced`.
#[derive(Debug, Serialize, JsonSchema)]
pub struct WorkflowStageProduced {
    pub stage: String,
    pub stage_hash: String,
    /// Always `0` — `produced` only fires on a clean exit.
    pub exit_code: i32,
    /// Milliseconds elapsed since the DAG run started.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
}

/// Payload for `workflow.stage.hashed`.
#[derive(Debug, Serialize, JsonSchema)]
pub struct WorkflowStageHashed {
    pub stage: String,
    pub stage_hash: String,
    pub outs: Vec<WorkflowStageOut>,
    /// Milliseconds elapsed since the DAG run started.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
}

/// Payload for `workflow.stage.committed`.
#[derive(Debug, Serialize, JsonSchema)]
pub struct WorkflowStageCommitted {
    pub stage: String,
    pub stage_hash: String,
    pub duration_ms: u64,
    pub attempts: u32,
    pub cache_hit: bool,
    /// Milliseconds elapsed since the DAG run started.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
}

/// Payload for `workflow.stage.failed`.
#[derive(Debug, Serialize, JsonSchema)]
pub struct WorkflowStageFailed {
    pub stage: String,
    pub stage_hash: String,
    /// Short reason tag, e.g. `"exit_nonzero"`, `"signal"`,
    /// `"timeout"`, `"disk_full"`, `"out_malformed"`, `"other"`.
    pub reason: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
    pub timed_out: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stderr_tail: Option<String>,
    /// Milliseconds elapsed since the DAG run started.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
}

/// Payload for `workflow.stage.not_started`.
///
/// Emitted in DAG mode when the scheduler refuses to start a stage
/// because a prior stage failed and `--keep-going` was not set
/// (`reason = "prior_stage_failed"`) or because one of the stage's
/// upstream producers failed and `--ignore-errors` was not set
/// (`reason = "upstream_failed"`).
#[derive(Debug, Serialize, JsonSchema)]
pub struct WorkflowStageNotStarted {
    pub stage: String,
    /// Short reason tag; stable vocabulary for downstream consumers.
    pub reason: String,
}

/// Payload for `workflow.stage.retry`.
///
/// Emitted when a failed stage attempt qualifies for retry per the
/// stage's declared retry policy. The event fires after the journal
/// records the failed attempt and before the backoff sleep begins.
#[derive(Debug, Serialize, JsonSchema)]
pub struct WorkflowStageRetry {
    pub stage: String,
    pub stage_hash: String,
    /// The attempt that just failed (1-indexed).
    pub attempt: u32,
    /// Short reason tag for the failure that triggered the retry.
    pub reason: String,
    /// Backoff duration in milliseconds before the next attempt.
    pub backoff_ms: u64,
    /// Whether this is the last retry before exhaustion.
    pub exhausted: bool,
    /// Milliseconds elapsed since the DAG run started.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
}

/// Event emitted by `--watch` mode when dep file changes are detected
/// and a re-execution cycle begins.
#[derive(Debug, Serialize, JsonSchema)]
pub struct WorkflowWatchTriggered {
    /// Paths that changed since the last execution cycle.
    pub changed_paths: Vec<String>,
    /// Number of changes coalesced in this debounce window.
    pub coalesced_events: usize,
}

// ---------------------------------------------------------------------------
// Tier restore event schemas (schema: "tier.event", version: "1.0")
// ---------------------------------------------------------------------------

/// Schema name for tier restore events emitted during hydrate.
pub const TIER_EVENT_SCHEMA: &str = "tier.event";

/// Schema version for tier restore events.
pub const TIER_EVENT_VERSION: &str = "1.0";

/// JSONL event emitted when a restore request is submitted for an
/// archived xorb during `crab hydrate`.
///
/// Schema: `"tier.event"` v `"1.0"`, event type: `"restore_submit"`.
#[derive(Debug, Serialize, JsonSchema)]
pub struct RestoreSubmitPayload {
    /// Hex-encoded content hash of the xorb being restored.
    pub xorb_hash: String,
    /// Provider-native storage class of the archived object.
    pub class: String,
    /// Restore tier requested (e.g. `"standard"`, `"expedited"`).
    pub restore_tier: String,
    /// RFC 3339 UTC timestamp when the restore was requested.
    pub requested_at: String,
    /// Estimated RFC 3339 UTC timestamp when the object will be ready.
    /// `None` when the provider does not supply an estimate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_ready_at: Option<String>,
    /// Initial poll interval in milliseconds.
    pub poll_interval_ms: u64,
}

/// JSONL event emitted when a restore completes (object is warm) during
/// `crab hydrate`.
///
/// Schema: `"tier.event"` v `"1.0"`, event type: `"restore_complete"`.
#[derive(Debug, Serialize, JsonSchema)]
pub struct RestoreCompletePayload {
    /// Hex-encoded content hash of the restored xorb.
    pub xorb_hash: String,
    /// Provider-native storage class of the object.
    pub class: String,
    /// Final restore state: `"ready"`, `"failed"`, `"timeout"`.
    pub state: String,
    /// RFC 3339 UTC timestamp when the restore completed.
    pub completed_at: String,
    /// Wall-clock milliseconds spent waiting for the restore.
    pub wait_ms: u64,
}

/// Terminal summary payload for a DAG run.
///
/// Emitted once per `crab run` invocation in multi-stage mode under
/// `--json` (as an `Envelope<WorkflowRunSummary>`) or as the final
/// `result` event on the `--jsonl` stream. Enumerates every stage the
/// scheduler touched, split into `succeeded`, `failed`, and
/// `not_started` bins, plus the per-stage cache/duration records that
/// consumers need to reconstruct the run without replaying the
/// event stream.
#[derive(Debug, Serialize, JsonSchema)]
pub struct WorkflowRunSummary {
    /// Stages that reached `Committed`. Names are ordered for stable
    /// JSON output; the executor builds them from a `BTreeSet`.
    pub succeeded: Vec<String>,
    /// Stages that terminated in `Failed`.
    pub failed: Vec<String>,
    /// Stages that the scheduler skipped — blocked by a prior
    /// failure or an upstream producer failure.
    pub not_started: Vec<String>,
    /// Per-stage success records (cache hit / duration / outs).
    /// Empty when no stage succeeded. Preserves the scheduler's
    /// topological execution order.
    pub stages: Vec<WorkflowStageResult>,
    /// Wall-clock duration of the full DAG run in milliseconds.
    pub duration_ms: u64,
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]
mod tests {
    //! Round-trip every workflow payload schema through `serde_json`
    //! and assert the field shapes the CLI and downstream consumers
    //! depend on. Paths are serialized as plain strings, hashes are
    //! lowercase 64-char hex (with an optional `b3:` prefix that the
    //! payload treats as opaque), and every schema constant has its
    //! matching stable name.

    use super::*;

    /// 64-char lowercase hex fixture representing a blake3 stage hash.
    const HASH_HEX: &str = "0000000000000000000000000000000000000000000000000000000000000000";

    #[test]
    fn schema_constants_are_stable() {
        // Stable schema names — consumers key on these strings.
        assert_eq!(WORKFLOW_SCHEMA_VERSION, "1.0");
        assert_eq!(WORKFLOW_STAGE_RESULT_SCHEMA, "workflow.stage_result");
        assert_eq!(WORKFLOW_RUN_SCHEMA, "workflow.run");
        assert_eq!(WORKFLOW_STAGE_STARTED_SCHEMA, "workflow.stage.started");
        assert_eq!(
            WORKFLOW_STAGE_CACHE_CHECKED_SCHEMA,
            "workflow.stage.cache_checked"
        );
        assert_eq!(WORKFLOW_STAGE_RUNNING_SCHEMA, "workflow.stage.running");
        assert_eq!(WORKFLOW_STAGE_PRODUCED_SCHEMA, "workflow.stage.produced");
        assert_eq!(WORKFLOW_STAGE_HASHED_SCHEMA, "workflow.stage.hashed");
        assert_eq!(WORKFLOW_STAGE_COMMITTED_SCHEMA, "workflow.stage.committed");
        assert_eq!(WORKFLOW_STAGE_FAILED_SCHEMA, "workflow.stage.failed");
        assert_eq!(
            WORKFLOW_STAGE_NOT_STARTED_SCHEMA,
            "workflow.stage.not_started"
        );
    }

    #[test]
    fn stage_result_serializes_expected_fields() {
        let payload = WorkflowStageResult {
            stage_name: "train".to_owned(),
            stage_hash: HASH_HEX.to_owned(),
            cache_hit: true,
            duration_ms: 1234,
            outs: vec![WorkflowStageOut {
                path: PathBuf::from("out/model.pkl"),
                file_hash: "b3:aa".to_owned(),
                size: 42,
            }],
            attempts: 2,
            side_effects_skipped: false,
            source: Some("Local".to_owned()),
        };
        let v = serde_json::to_value(&payload).unwrap();
        assert_eq!(v["stage_name"], "train");
        assert_eq!(v["stage_hash"], HASH_HEX);
        assert_eq!(v["cache_hit"], true);
        assert_eq!(v["duration_ms"], 1234);
        assert_eq!(v["attempts"], 2);
        assert_eq!(v["outs"][0]["path"], "out/model.pkl");
        assert_eq!(v["outs"][0]["file_hash"], "b3:aa");
        assert_eq!(v["outs"][0]["size"], 42);
        // side_effects_skipped is false, so it should be omitted
        assert!(v.get("side_effects_skipped").is_none());
    }

    #[test]
    fn stage_started_round_trips() {
        let payload = WorkflowStageStarted {
            stage: "train".to_owned(),
            stage_hash: HASH_HEX.to_owned(),
            attempt: 1,
            elapsed_ms: None,
        };
        let v = serde_json::to_value(&payload).unwrap();
        assert_eq!(v["stage"], "train");
        assert_eq!(v["stage_hash"], HASH_HEX);
        assert_eq!(v["attempt"], 1);
    }

    #[test]
    fn stage_cache_checked_encodes_hit_source() {
        let hit = WorkflowStageCacheChecked {
            stage: "train".to_owned(),
            stage_hash: HASH_HEX.to_owned(),
            hit: true,
            hit_source: "local".to_owned(),
            elapsed_ms: None,
        };
        let v = serde_json::to_value(&hit).unwrap();
        assert_eq!(v["hit"], true);
        assert_eq!(v["hit_source"], "local");

        // Miss-path shape: `hit: false`, `hit_source: "none"`.
        let miss = WorkflowStageCacheChecked {
            stage: "train".to_owned(),
            stage_hash: HASH_HEX.to_owned(),
            hit: false,
            hit_source: "none".to_owned(),
            elapsed_ms: None,
        };
        let v = serde_json::to_value(&miss).unwrap();
        assert_eq!(v["hit"], false);
        assert_eq!(v["hit_source"], "none");
    }

    #[test]
    fn stage_running_carries_pid_and_started_at() {
        let payload = WorkflowStageRunning {
            stage: "train".to_owned(),
            stage_hash: HASH_HEX.to_owned(),
            pid: 42,
            attempt: 3,
            started_at: "2026-01-01T00:00:00.000Z".to_owned(),
        };
        let v = serde_json::to_value(&payload).unwrap();
        assert_eq!(v["pid"], 42);
        assert_eq!(v["attempt"], 3);
        assert_eq!(v["started_at"], "2026-01-01T00:00:00.000Z");
    }

    #[test]
    fn stage_produced_exit_code_is_always_zero() {
        // The schema models `Produced` as clean-exit only — the
        // `exit_code` field documents that invariant.
        let payload = WorkflowStageProduced {
            stage: "train".to_owned(),
            stage_hash: HASH_HEX.to_owned(),
            exit_code: 0,
            elapsed_ms: None,
        };
        let v = serde_json::to_value(&payload).unwrap();
        assert_eq!(v["exit_code"], 0);
    }

    #[test]
    fn stage_hashed_enumerates_outs() {
        let payload = WorkflowStageHashed {
            stage: "train".to_owned(),
            stage_hash: HASH_HEX.to_owned(),
            outs: vec![
                WorkflowStageOut {
                    path: PathBuf::from("a.txt"),
                    file_hash: "b3:aa".to_owned(),
                    size: 1,
                },
                WorkflowStageOut {
                    path: PathBuf::from("b.txt"),
                    file_hash: "b3:bb".to_owned(),
                    size: 2,
                },
            ],
            elapsed_ms: None,
        };
        let v = serde_json::to_value(&payload).unwrap();
        assert_eq!(v["outs"].as_array().unwrap().len(), 2);
        assert_eq!(v["outs"][0]["path"], "a.txt");
        assert_eq!(v["outs"][1]["file_hash"], "b3:bb");
    }

    #[test]
    fn stage_committed_echoes_duration_attempts_cache_hit() {
        let payload = WorkflowStageCommitted {
            stage: "train".to_owned(),
            stage_hash: HASH_HEX.to_owned(),
            duration_ms: 999,
            attempts: 2,
            cache_hit: false,
            elapsed_ms: None,
        };
        let v = serde_json::to_value(&payload).unwrap();
        assert_eq!(v["duration_ms"], 999);
        assert_eq!(v["attempts"], 2);
        assert_eq!(v["cache_hit"], false);
    }

    #[test]
    fn stage_failed_skips_none_optional_fields() {
        // `None` fields drop out of the JSON so consumers can
        // distinguish "not applicable" from "zero".
        let timed_out = WorkflowStageFailed {
            stage: "train".to_owned(),
            stage_hash: HASH_HEX.to_owned(),
            reason: "timeout".to_owned(),
            exit_code: None,
            signal: None,
            timed_out: true,
            stderr_tail: None,
            elapsed_ms: None,
        };
        let v = serde_json::to_value(&timed_out).unwrap();
        assert_eq!(v["reason"], "timeout");
        assert_eq!(v["timed_out"], true);
        assert!(v.get("exit_code").is_none());
        assert!(v.get("signal").is_none());
        assert!(v.get("stderr_tail").is_none());

        // Populated-exit-code case: exit_nonzero carries the real
        // code while signal/timed_out stay at their defaults.
        let bad_exit = WorkflowStageFailed {
            stage: "train".to_owned(),
            stage_hash: HASH_HEX.to_owned(),
            reason: "exit_nonzero".to_owned(),
            exit_code: Some(77),
            signal: None,
            timed_out: false,
            stderr_tail: Some("last 2KB of stderr".to_owned()),
            elapsed_ms: None,
        };
        let v = serde_json::to_value(&bad_exit).unwrap();
        assert_eq!(v["exit_code"], 77);
        assert_eq!(v["timed_out"], false);
        assert_eq!(v["stderr_tail"], "last 2KB of stderr");
    }

    #[test]
    fn stage_not_started_encodes_reason() {
        // The `reason` vocabulary is stable — `"prior_stage_failed"`
        // and `"upstream_failed"` are the two values the scheduler
        // emits today. New reasons must be added here before the
        // scheduler can use them.
        let payload = WorkflowStageNotStarted {
            stage: "report".to_owned(),
            reason: "upstream_failed".to_owned(),
        };
        let v = serde_json::to_value(&payload).unwrap();
        assert_eq!(v["stage"], "report");
        assert_eq!(v["reason"], "upstream_failed");
    }

    #[test]
    fn run_summary_enumerates_bins_and_stage_records() {
        let payload = WorkflowRunSummary {
            succeeded: vec!["clean".to_owned(), "train".to_owned()],
            failed: vec!["report".to_owned()],
            not_started: Vec::new(),
            stages: vec![WorkflowStageResult {
                stage_name: "clean".to_owned(),
                stage_hash: HASH_HEX.to_owned(),
                cache_hit: false,
                duration_ms: 120,
                outs: vec![WorkflowStageOut {
                    path: PathBuf::from("clean.csv"),
                    file_hash: "b3:aa".to_owned(),
                    size: 100,
                }],
                attempts: 1,
                side_effects_skipped: false,
                source: Some("Execution".to_owned()),
            }],
            duration_ms: 999,
        };
        let v = serde_json::to_value(&payload).unwrap();

        // Bins surface as arrays of stage-name strings.
        let succeeded: Vec<&str> = v["succeeded"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_str().unwrap())
            .collect();
        assert_eq!(succeeded, vec!["clean", "train"]);
        let failed: Vec<&str> = v["failed"]
            .as_array()
            .unwrap()
            .iter()
            .map(|x| x.as_str().unwrap())
            .collect();
        assert_eq!(failed, vec!["report"]);
        assert!(v["not_started"].as_array().unwrap().is_empty());

        // Per-stage records keep the `WorkflowStageResult` shape.
        assert_eq!(v["stages"][0]["stage_name"], "clean");
        assert_eq!(v["stages"][0]["cache_hit"], false);
        assert_eq!(v["stages"][0]["outs"][0]["path"], "clean.csv");
        assert_eq!(v["duration_ms"], 999);
    }

    // ── Tier restore event payload tests ────────────────────────────

    #[test]
    fn tier_event_schema_constants_are_stable() {
        assert_eq!(TIER_EVENT_SCHEMA, "tier.event");
        assert_eq!(TIER_EVENT_VERSION, "1.0");
    }

    #[test]
    fn restore_submit_serializes_expected_fields() {
        let payload = RestoreSubmitPayload {
            xorb_hash: "abc123".to_owned(),
            class: "S3 Glacier Flexible Retrieval".to_owned(),
            restore_tier: "standard".to_owned(),
            requested_at: "2026-01-01T00:00:00.000Z".to_owned(),
            expected_ready_at: Some("2026-01-01T06:00:00.000Z".to_owned()),
            poll_interval_ms: 30_000,
        };
        let v = serde_json::to_value(&payload).unwrap();
        assert_eq!(v["xorb_hash"], "abc123");
        assert_eq!(v["class"], "S3 Glacier Flexible Retrieval");
        assert_eq!(v["restore_tier"], "standard");
        assert_eq!(v["requested_at"], "2026-01-01T00:00:00.000Z");
        assert_eq!(v["expected_ready_at"], "2026-01-01T06:00:00.000Z");
        assert_eq!(v["poll_interval_ms"], 30_000);
    }

    #[test]
    fn restore_submit_skips_none_expected_ready_at() {
        let payload = RestoreSubmitPayload {
            xorb_hash: "def456".to_owned(),
            class: "Azure Archive".to_owned(),
            restore_tier: "high".to_owned(),
            requested_at: "2026-01-01T00:00:00.000Z".to_owned(),
            expected_ready_at: None,
            poll_interval_ms: 30_000,
        };
        let v = serde_json::to_value(&payload).unwrap();
        assert!(v.get("expected_ready_at").is_none());
    }

    #[test]
    fn restore_complete_serializes_expected_fields() {
        let payload = RestoreCompletePayload {
            xorb_hash: "abc123".to_owned(),
            class: "S3 Glacier Deep Archive".to_owned(),
            state: "ready".to_owned(),
            completed_at: "2026-01-01T12:00:00.000Z".to_owned(),
            wait_ms: 43_200_000,
        };
        let v = serde_json::to_value(&payload).unwrap();
        assert_eq!(v["xorb_hash"], "abc123");
        assert_eq!(v["class"], "S3 Glacier Deep Archive");
        assert_eq!(v["state"], "ready");
        assert_eq!(v["completed_at"], "2026-01-01T12:00:00.000Z");
        assert_eq!(v["wait_ms"], 43_200_000);
    }
}
