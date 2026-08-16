//! Structured output helpers for `--json` and `--jsonl` modes.
//!
//! Entry points:
//! - [`OutputMode`] — resolved once per command from CLI flags.
//! - [`emit_json`] — writes a single success `Envelope` to stdout.
//! - [`emit_error_json`] — writes an error `Envelope` to stdout.
//! - [`JsonlStream`] — rate-limited JSONL event writer.

mod envelope;
pub mod error_info;
pub mod event_payloads;
mod jsonl;

pub use crab_types::error::ErrorCategory;
pub use envelope::{Envelope, ErrorEventEnvelope, EventEnvelope};
pub use error_info::{ErrorInfo, ErrorSource};
pub use event_payloads::{
    FileDonePayload, PERF_PHASE_SCHEMA, PERF_PHASE_SCHEMA_VERSION, PerfPhasePayload,
    ProgressPayload, WORKFLOW_RUN_SCHEMA, WORKFLOW_SCHEMA_VERSION,
    WORKFLOW_STAGE_CACHE_CHECKED_SCHEMA, WORKFLOW_STAGE_COMMITTED_SCHEMA,
    WORKFLOW_STAGE_FAILED_SCHEMA, WORKFLOW_STAGE_HASHED_SCHEMA, WORKFLOW_STAGE_NOT_STARTED_SCHEMA,
    WORKFLOW_STAGE_PRODUCED_SCHEMA, WORKFLOW_STAGE_RESULT_SCHEMA, WORKFLOW_STAGE_RETRY_SCHEMA,
    WORKFLOW_STAGE_RUNNING_SCHEMA, WORKFLOW_STAGE_STARTED_SCHEMA, WORKFLOW_WATCH_TRIGGERED_SCHEMA,
    WarningPayload, WorkflowRunSummary, WorkflowStageCacheChecked, WorkflowStageCommitted,
    WorkflowStageFailed, WorkflowStageHashed, WorkflowStageNotStarted, WorkflowStageOut,
    WorkflowStageProduced, WorkflowStageResult, WorkflowStageRetry, WorkflowStageRunning,
    WorkflowStageStarted, WorkflowWatchTriggered, XorbDonePayload,
};
pub use jsonl::{JsonlStream, WorkflowStageEvent};

use std::io::Write;

use serde::Serialize;

use crate::core::error::CrabError;

// ---------------------------------------------------------------------------
// OutputMode
// ---------------------------------------------------------------------------

/// How the current command should render its output.
///
/// Resolved once at the top of each `run_*` function via
/// [`OutputMode::from_flags`]. Downstream code branches on the enum
/// instead of carrying raw booleans.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputMode {
    /// Human-readable text (default).
    #[default]
    Text,
    /// Single JSON envelope on stdout.
    Json,
    /// Newline-delimited JSON stream on stdout.
    Jsonl,
}

impl OutputMode {
    /// Derive the mode from the two CLI flags.
    ///
    /// Clap `conflicts_with` ensures `(true, true)` is unreachable.
    pub fn from_flags(json: bool, jsonl: bool) -> Self {
        match (json, jsonl) {
            (false, false) => Self::Text,
            (true, false) => Self::Json,
            (false, true) => Self::Jsonl,
            (true, true) => unreachable!("clap conflict guard prevents --json + --jsonl"),
        }
    }

    /// Returns `true` when the output is machine-readable (Json or Jsonl).
    pub fn is_machine(self) -> bool {
        matches!(self, Self::Json | Self::Jsonl)
    }
}

// ---------------------------------------------------------------------------
// emit_json / emit_error_json
// ---------------------------------------------------------------------------

/// Write a success envelope to stdout and flush.
///
/// Locks stdout for the duration of the write so no interleaved
/// `println!` can corrupt the JSON. Errors writing to stdout are
/// silently ignored — these helpers are called right before process
/// exit, and there is nothing useful to do if stdout is broken.
pub fn emit_json<T: Serialize>(schema: &'static str, version: &'static str, data: T) {
    let envelope = Envelope::ok(schema, version, data);
    let stdout = std::io::stdout();
    let lock = stdout.lock();
    let mut writer = std::io::BufWriter::new(lock);
    let _ = serde_json::to_writer(&mut writer, &envelope);
    let _ = writer.write_all(b"\n");
    let _ = writer.flush();
}

/// Write an error envelope to stdout and flush.
///
/// Same stdout-locking and error-swallowing semantics as [`emit_json`].
pub fn emit_error_json(schema: &'static str, version: &'static str, err: &CrabError) {
    let error_info = ErrorInfo::from(err);
    let envelope = Envelope::err(schema, version, error_info);
    let stdout = std::io::stdout();
    let lock = stdout.lock();
    let mut writer = std::io::BufWriter::new(lock);
    let _ = serde_json::to_writer(&mut writer, &envelope);
    let _ = writer.write_all(b"\n");
    let _ = writer.flush();
}
