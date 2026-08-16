//! Rate-limited JSONL event writer for `--jsonl` streaming output.
//!
//! `JsonlStream<W>` is generic over `W: Write` so that normal CLI
//! commands can write to `Stdout` while the remote helper (where git
//! owns stdout) writes to `Stderr`.

use std::io::{BufWriter, Write};
use std::time::{Duration, Instant};

use crab_types::time::now_rfc3339_millis;
use serde::Serialize;

use super::EventEnvelope;
use super::envelope::ErrorEventEnvelope;
use super::error_info::ErrorInfo;
use super::event_payloads::{
    WORKFLOW_STAGE_CACHE_CHECKED_SCHEMA, WORKFLOW_STAGE_COMMITTED_SCHEMA,
    WORKFLOW_STAGE_FAILED_SCHEMA, WORKFLOW_STAGE_HASHED_SCHEMA, WORKFLOW_STAGE_PRODUCED_SCHEMA,
    WORKFLOW_STAGE_RUNNING_SCHEMA, WORKFLOW_STAGE_STARTED_SCHEMA, WorkflowStageCacheChecked,
    WorkflowStageCommitted, WorkflowStageFailed, WorkflowStageHashed, WorkflowStageProduced,
    WorkflowStageRunning, WorkflowStageStarted,
};

/// Minimum interval between consecutive `progress` event emissions.
const PROGRESS_RATE_LIMIT: Duration = Duration::from_millis(250);

/// Rate-limited JSONL event writer.
///
/// Each `emit_*` method serializes an [`EventEnvelope`] as a single
/// JSON line terminated by `\n`. `BufWriter` ensures each line is
/// written atomically under typical pipe buffer sizes (4 KiB+).
///
/// `emit_progress` is rate-limited to one emission per 250 ms
/// wall-clock. All other event types are emitted unconditionally.
///
/// `emit_result` and `emit_error` flush the writer. The `Drop` impl
/// also flushes to avoid losing the final line in buffered output.
pub struct JsonlStream<W: Write> {
    schema: &'static str,
    version: &'static str,
    last_progress_emit: Option<Instant>,
    writer: BufWriter<W>,
}

impl<W: Write> std::fmt::Debug for JsonlStream<W> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JsonlStream")
            .field("schema", &self.schema)
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}

impl<W: Write> JsonlStream<W> {
    /// Create a new JSONL stream writing to `writer`.
    ///
    /// `schema` is the event schema name (e.g. `"hydrate.event"`).
    /// `version` is the schema version (e.g. `"1.0"`).
    pub fn new(schema: &'static str, version: &'static str, writer: W) -> Self {
        Self {
            schema,
            version,
            last_progress_emit: None,
            writer: BufWriter::new(writer),
        }
    }

    /// Emit a `"progress"` event, rate-limited to at most once per 250 ms.
    ///
    /// Returns `true` if the event was actually written, `false` if it
    /// was suppressed by the rate limiter.
    pub fn emit_progress<T: Serialize>(&mut self, data: T) -> bool {
        let now = Instant::now();
        if self
            .last_progress_emit
            .is_some_and(|last| now.duration_since(last) < PROGRESS_RATE_LIMIT)
        {
            return false;
        }
        self.last_progress_emit = Some(now);
        self.write_event("progress", data);
        true
    }

    /// Emit a `"file_done"` event. Never rate-limited.
    pub fn emit_file_done<T: Serialize>(&mut self, data: T) {
        self.write_event("file_done", data);
    }

    /// Emit a `"xorb_done"` event. Never rate-limited.
    pub fn emit_xorb_done<T: Serialize>(&mut self, data: T) {
        self.write_event("xorb_done", data);
    }

    /// Emit a `"warning"` event. Never rate-limited.
    pub fn emit_warning<T: Serialize>(&mut self, data: T) {
        self.write_event("warning", data);
    }

    /// Emit a `"restore_submit"` event for tier restore tracking.
    /// Never rate-limited. Uses the stream's schema (expected to be
    /// `"tier.event"` v `"1.0"` for hydrate restore streams).
    pub fn emit_restore_submit<T: Serialize>(&mut self, data: T) {
        self.write_event("restore_submit", data);
    }

    /// Emit a `"restore_complete"` event for tier restore tracking.
    /// Never rate-limited.
    pub fn emit_restore_complete<T: Serialize>(&mut self, data: T) {
        self.write_event("restore_complete", data);
    }

    /// Emit a terminal `"result"` event and flush the writer.
    pub fn emit_result<T: Serialize>(&mut self, data: T) {
        self.write_event("result", data);
        let _ = self.writer.flush();
    }

    /// Emit a `"snapshot"` event and flush the writer.
    pub fn emit_snapshot<T: Serialize>(&mut self, data: T) {
        self.write_event("snapshot", data);
        let _ = self.writer.flush();
    }

    /// Emit a terminal `"result"` event with an error payload and flush.
    ///
    /// Used when a streaming command fails — the final event carries
    /// the structured error instead of a success payload.
    pub fn emit_error<T: Serialize>(&mut self, data: T) {
        self.write_event("result", data);
        let _ = self.writer.flush();
    }

    /// Emit a terminal `"result"` event with a structured [`ErrorInfo`]
    /// in the `"error"` field (not `"data"`) and flush.
    ///
    /// Per SO4.8, a fatal error in JSONL mode is emitted as a terminal
    /// `result` event with `"error"` populated and no `"data"` field.
    pub fn emit_error_info(&mut self, error: ErrorInfo) {
        let envelope = ErrorEventEnvelope {
            schema: self.schema,
            version: self.version,
            timestamp: now_rfc3339_millis(),
            event_type: "result".to_owned(),
            error,
        };
        let _ = serde_json::to_writer(&mut self.writer, &envelope);
        let _ = self.writer.write_all(b"\n");
        let _ = self.writer.flush();
    }

    /// Serialize an [`EventEnvelope`] as a single JSON line.
    ///
    /// Write errors are silently ignored — these helpers are called
    /// during normal operation and there is nothing useful to do if
    /// the output pipe is broken.
    fn write_event<T: Serialize>(&mut self, event_type: &str, data: T) {
        let envelope = EventEnvelope {
            schema: self.schema,
            version: self.version,
            timestamp: now_rfc3339_millis(),
            event_type: event_type.to_owned(),
            data,
        };
        let _ = serde_json::to_writer(&mut self.writer, &envelope);
        let _ = self.writer.write_all(b"\n");
    }

    /// Emit a workflow-layer `workflow.stage.*` event.
    ///
    /// Each variant carries its own schema name (e.g.
    /// `workflow.stage.running`), unlike the default event writer
    /// which reuses the stream's umbrella schema. The envelope's
    /// `type` field is always `"event"` — the `schema` identifies
    /// the specific stage transition. Never rate-limited.
    pub fn emit_workflow_stage_event(&mut self, event: &WorkflowStageEvent<'_>) {
        let schema = event.schema();
        match event {
            WorkflowStageEvent::Started(p) => self.write_schema_event(schema, "event", p),
            WorkflowStageEvent::CacheChecked(p) => self.write_schema_event(schema, "event", p),
            WorkflowStageEvent::Running(p) => self.write_schema_event(schema, "event", p),
            WorkflowStageEvent::Produced(p) => self.write_schema_event(schema, "event", p),
            WorkflowStageEvent::Hashed(p) => self.write_schema_event(schema, "event", p),
            WorkflowStageEvent::Committed(p) => self.write_schema_event(schema, "event", p),
            WorkflowStageEvent::Failed(p) => self.write_schema_event(schema, "event", p),
        }
    }

    /// Write a line with an explicit schema override.
    ///
    /// The default `write_event` uses the stream's construction-time
    /// schema; workflow stage events need a per-event schema name
    /// (e.g. `workflow.stage.cache_checked`) so each line identifies
    /// its shape without forcing consumers to key on `type` +
    /// umbrella-schema.
    fn write_schema_event<T: Serialize>(
        &mut self,
        schema: &'static str,
        event_type: &str,
        data: T,
    ) {
        let envelope = EventEnvelope {
            schema,
            version: self.version,
            timestamp: now_rfc3339_millis(),
            event_type: event_type.to_owned(),
            data,
        };
        let _ = serde_json::to_writer(&mut self.writer, &envelope);
        let _ = self.writer.write_all(b"\n");
    }

    /// Emit an event whose `schema` field overrides the stream's
    /// construction-time umbrella schema.
    ///
    /// Used by workflow-layer callers to emit per-line schema names
    /// that don't fit the `WorkflowStageEvent` enum — for instance
    /// `workflow.stage.not_started` (no stage_hash) and the
    /// terminal `workflow.run` summary that rides the same stream.
    /// Consumers dispatch on `schema` per line rather than on the
    /// stream's umbrella name. Never rate-limited. When
    /// `event_type = "result"` the writer is flushed so a terminal
    /// summary on a dropped stream doesn't get swallowed.
    pub fn emit_schema_event<T: Serialize>(
        &mut self,
        schema: &'static str,
        event_type: &str,
        data: T,
    ) {
        self.write_schema_event(schema, event_type, data);
        if event_type == "result" {
            let _ = self.writer.flush();
        }
    }
}

/// Typed envelope for the seven canonical `workflow.stage.*` events.
///
/// Each variant maps 1:1 to a schema constant in
/// [`super::event_payloads`]. Callers build the payload, pass it to
/// [`JsonlStream::emit_workflow_stage_event`], and the stream takes
/// care of serialization, timestamping, and line framing.
#[derive(Debug)]
pub enum WorkflowStageEvent<'a> {
    Started(&'a WorkflowStageStarted),
    CacheChecked(&'a WorkflowStageCacheChecked),
    Running(&'a WorkflowStageRunning),
    Produced(&'a WorkflowStageProduced),
    Hashed(&'a WorkflowStageHashed),
    Committed(&'a WorkflowStageCommitted),
    Failed(&'a WorkflowStageFailed),
}

impl WorkflowStageEvent<'_> {
    /// The canonical schema name for this event variant.
    pub fn schema(&self) -> &'static str {
        match self {
            Self::Started(_) => WORKFLOW_STAGE_STARTED_SCHEMA,
            Self::CacheChecked(_) => WORKFLOW_STAGE_CACHE_CHECKED_SCHEMA,
            Self::Running(_) => WORKFLOW_STAGE_RUNNING_SCHEMA,
            Self::Produced(_) => WORKFLOW_STAGE_PRODUCED_SCHEMA,
            Self::Hashed(_) => WORKFLOW_STAGE_HASHED_SCHEMA,
            Self::Committed(_) => WORKFLOW_STAGE_COMMITTED_SCHEMA,
            Self::Failed(_) => WORKFLOW_STAGE_FAILED_SCHEMA,
        }
    }
}

impl<W: Write> Drop for JsonlStream<W> {
    fn drop(&mut self) {
        let _ = self.writer.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde::Deserialize;

    /// Minimal payload for testing.
    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    struct TestPayload {
        value: u64,
    }

    /// Parsed event envelope for assertions.
    #[derive(Deserialize, Debug)]
    struct ParsedEvent {
        schema: String,
        version: String,
        timestamp: String,
        #[serde(rename = "type")]
        event_type: String,
        data: TestPayload,
    }

    use std::cell::RefCell;
    use std::rc::Rc;

    /// Shared buffer that implements `Write`, allowing us to inspect
    /// the output after the `JsonlStream` is dropped.
    #[derive(Clone)]
    struct SharedBuf(Rc<RefCell<Vec<u8>>>);

    impl SharedBuf {
        fn new() -> Self {
            Self(Rc::new(RefCell::new(Vec::new())))
        }

        fn to_string(&self) -> String {
            String::from_utf8(self.0.borrow().clone()).expect("invalid utf-8")
        }
    }

    impl Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.borrow_mut().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn emit_result_writes_valid_jsonl_line() {
        let buf = SharedBuf::new();
        let mut stream = JsonlStream::new("test.event", "1.0", buf.clone());
        stream.emit_result(TestPayload { value: 42 });
        drop(stream);

        let output = buf.to_string();
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines.len(), 1);

        let parsed: ParsedEvent = serde_json::from_str(lines[0]).expect("invalid JSON");
        assert_eq!(parsed.schema, "test.event");
        assert_eq!(parsed.version, "1.0");
        assert_eq!(parsed.event_type, "result");
        assert_eq!(parsed.data, TestPayload { value: 42 });
        assert!(parsed.timestamp.ends_with('Z'));
    }

    #[test]
    fn emit_snapshot_writes_snapshot_event() {
        let buf = SharedBuf::new();
        let mut stream = JsonlStream::new("test.event", "1.0", buf.clone());
        stream.emit_snapshot(TestPayload { value: 42 });
        drop(stream);

        let output = buf.to_string();
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines.len(), 1);

        let parsed: ParsedEvent = serde_json::from_str(lines[0]).expect("invalid JSON");
        assert_eq!(parsed.schema, "test.event");
        assert_eq!(parsed.version, "1.0");
        assert_eq!(parsed.event_type, "snapshot");
        assert_eq!(parsed.data, TestPayload { value: 42 });
    }

    #[test]
    fn each_line_ends_with_newline() {
        let buf = SharedBuf::new();
        let mut stream = JsonlStream::new("t", "1.0", buf.clone());
        stream.emit_file_done(TestPayload { value: 1 });
        stream.emit_xorb_done(TestPayload { value: 2 });
        stream.emit_warning(TestPayload { value: 3 });
        stream.emit_result(TestPayload { value: 4 });
        drop(stream);

        let output = buf.to_string();
        assert!(output.ends_with('\n'));
        for line in output.lines() {
            let _: serde_json::Value = serde_json::from_str(line).expect("invalid JSON line");
        }
        assert_eq!(output.lines().count(), 4);
    }

    #[test]
    fn emit_progress_rate_limits() {
        let buf = SharedBuf::new();
        let mut stream = JsonlStream::new("t", "1.0", buf.clone());

        // First progress should always emit.
        assert!(stream.emit_progress(TestPayload { value: 1 }));

        // Immediate second call should be suppressed.
        assert!(!stream.emit_progress(TestPayload { value: 2 }));
        drop(stream);

        let output = buf.to_string();
        assert_eq!(output.lines().count(), 1);
    }

    #[test]
    fn completion_events_never_rate_limited() {
        let buf = SharedBuf::new();
        let mut stream = JsonlStream::new("t", "1.0", buf.clone());

        for i in 0..10 {
            stream.emit_file_done(TestPayload { value: i });
        }
        drop(stream);

        let output = buf.to_string();
        assert_eq!(output.lines().count(), 10);
    }

    #[test]
    fn progress_timer_resets_only_on_progress() {
        let buf = SharedBuf::new();
        let mut stream = JsonlStream::new("t", "1.0", buf.clone());

        // Emit first progress.
        assert!(stream.emit_progress(TestPayload { value: 1 }));

        // file_done events should NOT reset the progress timer.
        for i in 0..5 {
            stream.emit_file_done(TestPayload { value: i });
        }

        // Immediate second progress should still be suppressed.
        assert!(!stream.emit_progress(TestPayload { value: 2 }));
        drop(stream);

        let output = buf.to_string();
        // 1 progress + 5 file_done = 6 lines
        assert_eq!(output.lines().count(), 6);
    }

    #[test]
    fn event_types_are_correct() {
        let buf = SharedBuf::new();
        let mut stream = JsonlStream::new("t", "1.0", buf.clone());
        stream.emit_progress(TestPayload { value: 0 });
        stream.emit_file_done(TestPayload { value: 0 });
        stream.emit_xorb_done(TestPayload { value: 0 });
        stream.emit_warning(TestPayload { value: 0 });
        stream.emit_result(TestPayload { value: 0 });
        drop(stream);

        let output = buf.to_string();
        let types: Vec<String> = output
            .lines()
            .map(|line| {
                let v: serde_json::Value = serde_json::from_str(line).unwrap();
                v["type"].as_str().unwrap().to_owned()
            })
            .collect();

        assert_eq!(
            types,
            vec!["progress", "file_done", "xorb_done", "warning", "result"]
        );
    }

    #[test]
    fn emit_error_writes_result_type() {
        let buf = SharedBuf::new();
        let mut stream = JsonlStream::new("t", "1.0", buf.clone());
        stream.emit_error(TestPayload { value: 99 });
        drop(stream);

        let output = buf.to_string();
        let parsed: ParsedEvent = serde_json::from_str(output.trim()).expect("invalid JSON");
        // emit_error writes a "result" type event (terminal event with error payload).
        assert_eq!(parsed.event_type, "result");
        assert_eq!(parsed.data, TestPayload { value: 99 });
    }

    #[test]
    fn emit_error_info_writes_error_field() {
        let buf = SharedBuf::new();
        let mut stream = JsonlStream::new("add.event", "1.0", buf.clone());

        let error_info = ErrorInfo {
            code: "CRAB-E0090".to_owned(),
            category: crate::core::output::ErrorCategory::Cancelled,
            message: "operation cancelled".to_owned(),
            retryable: false,
            details: serde_json::Value::Null,
            source_chain: vec![],
        };
        stream.emit_error_info(error_info);
        drop(stream);

        let output = buf.to_string();
        let v: serde_json::Value = serde_json::from_str(output.trim()).expect("invalid JSON");

        assert_eq!(v["schema"], "add.event");
        assert_eq!(v["version"], "1.0");
        assert_eq!(v["type"], "result");
        assert_eq!(v["error"]["code"], "CRAB-E0090");
        assert_eq!(v["error"]["category"], "cancelled");
        assert_eq!(v["error"]["message"], "operation cancelled");
        // No "data" field should be present.
        assert!(
            v.get("data").is_none(),
            "error event should not have a data field"
        );
    }

    // --- Workflow stage event tests --------------------------------

    /// The seven `workflow.stage.*` events stamp the envelope's
    /// `schema` field with their canonical schema name (not the
    /// stream's umbrella schema). This is what lets jsonl consumers
    /// dispatch on `schema` without inspecting the payload shape.
    #[test]
    fn workflow_stage_events_carry_per_event_schema() {
        use crate::core::output::event_payloads::{
            WorkflowStageCacheChecked, WorkflowStageCommitted, WorkflowStageFailed,
            WorkflowStageHashed, WorkflowStageOut, WorkflowStageProduced, WorkflowStageRunning,
            WorkflowStageStarted,
        };

        let buf = SharedBuf::new();
        let mut stream = JsonlStream::new("workflow.stage.event", "1.0", buf.clone());

        let started = WorkflowStageStarted {
            stage: "s".to_owned(),
            stage_hash: "h".to_owned(),
            attempt: 1,
            elapsed_ms: None,
        };
        let cache = WorkflowStageCacheChecked {
            stage: "s".to_owned(),
            stage_hash: "h".to_owned(),
            hit: false,
            hit_source: "none".to_owned(),
            elapsed_ms: None,
        };
        let running = WorkflowStageRunning {
            stage: "s".to_owned(),
            stage_hash: "h".to_owned(),
            pid: 1,
            attempt: 1,
            started_at: "t".to_owned(),
        };
        let produced = WorkflowStageProduced {
            stage: "s".to_owned(),
            stage_hash: "h".to_owned(),
            exit_code: 0,
            elapsed_ms: None,
        };
        let hashed = WorkflowStageHashed {
            stage: "s".to_owned(),
            stage_hash: "h".to_owned(),
            outs: vec![WorkflowStageOut {
                path: "a".into(),
                file_hash: "b3:aa".to_owned(),
                size: 1,
            }],
            elapsed_ms: None,
        };
        let committed = WorkflowStageCommitted {
            stage: "s".to_owned(),
            stage_hash: "h".to_owned(),
            duration_ms: 10,
            attempts: 1,
            cache_hit: false,
            elapsed_ms: None,
        };
        let failed = WorkflowStageFailed {
            stage: "s".to_owned(),
            stage_hash: "h".to_owned(),
            reason: "exit_nonzero".to_owned(),
            exit_code: Some(1),
            signal: None,
            timed_out: false,
            stderr_tail: None,
            elapsed_ms: None,
        };

        stream.emit_workflow_stage_event(&WorkflowStageEvent::Started(&started));
        stream.emit_workflow_stage_event(&WorkflowStageEvent::CacheChecked(&cache));
        stream.emit_workflow_stage_event(&WorkflowStageEvent::Running(&running));
        stream.emit_workflow_stage_event(&WorkflowStageEvent::Produced(&produced));
        stream.emit_workflow_stage_event(&WorkflowStageEvent::Hashed(&hashed));
        stream.emit_workflow_stage_event(&WorkflowStageEvent::Committed(&committed));
        stream.emit_workflow_stage_event(&WorkflowStageEvent::Failed(&failed));
        drop(stream);

        let output = buf.to_string();
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(lines.len(), 7);

        let schemas: Vec<String> = lines
            .iter()
            .map(|line| {
                let v: serde_json::Value = serde_json::from_str(line).unwrap();
                v["schema"].as_str().unwrap().to_owned()
            })
            .collect();

        assert_eq!(
            schemas,
            vec![
                "workflow.stage.started",
                "workflow.stage.cache_checked",
                "workflow.stage.running",
                "workflow.stage.produced",
                "workflow.stage.hashed",
                "workflow.stage.committed",
                "workflow.stage.failed",
            ]
        );

        // Every line also carries event_type = "event" and the
        // matching payload fields.
        for line in &lines {
            let v: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(v["type"], "event");
            assert_eq!(v["version"], "1.0");
            assert!(v["timestamp"].as_str().unwrap().ends_with('Z'));
            assert_eq!(v["data"]["stage"], "s");
            assert_eq!(v["data"]["stage_hash"], "h");
        }
    }

    /// Payload-specific fields reach the consumer intact. Guards
    /// against refactors that accidentally re-shape the envelope.
    #[test]
    fn workflow_stage_failed_surfaces_reason_and_exit_code() {
        use crate::core::output::event_payloads::WorkflowStageFailed;

        let buf = SharedBuf::new();
        let mut stream = JsonlStream::new("workflow.stage.event", "1.0", buf.clone());
        let payload = WorkflowStageFailed {
            stage: "train".to_owned(),
            stage_hash: "abc".to_owned(),
            reason: "timeout".to_owned(),
            exit_code: None,
            signal: None,
            timed_out: true,
            stderr_tail: None,
            elapsed_ms: None,
        };
        stream.emit_workflow_stage_event(&WorkflowStageEvent::Failed(&payload));
        drop(stream);

        let output = buf.to_string();
        let v: serde_json::Value = serde_json::from_str(output.trim()).unwrap();
        assert_eq!(v["schema"], "workflow.stage.failed");
        assert_eq!(v["data"]["reason"], "timeout");
        assert_eq!(v["data"]["timed_out"], true);
        assert!(v["data"].get("exit_code").is_none());
    }
}
