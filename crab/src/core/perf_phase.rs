//! Lightweight operation phase timing.
//!
//! Commands use this helper to produce the stable `perf.phase` payload
//! without each pipeline carrying its own stopwatch and RSS sampling code.

use std::io::{Stderr, Stdout};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::core::output::{
    JsonlStream, PERF_PHASE_SCHEMA, PERF_PHASE_SCHEMA_VERSION, PerfPhasePayload,
};

/// Stopwatch for a single operation phase.
#[derive(Debug)]
pub struct PhaseTimer {
    operation: &'static str,
    phase: &'static str,
    started: Instant,
    rss_start: Option<u64>,
}

/// Destination for optional JSONL phase events.
#[derive(Clone)]
pub enum PerfPhaseSink {
    Stdout(Arc<Mutex<JsonlStream<Stdout>>>),
    Stderr(Arc<Mutex<JsonlStream<Stderr>>>),
}

impl std::fmt::Debug for PerfPhaseSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stdout(_) => f.write_str("PerfPhaseSink::Stdout"),
            Self::Stderr(_) => f.write_str("PerfPhaseSink::Stderr"),
        }
    }
}

impl PerfPhaseSink {
    /// Emit a phase payload to the configured JSONL stream.
    pub fn emit(&self, payload: PerfPhasePayload) {
        match self {
            Self::Stdout(stream) => {
                if let Ok(mut s) = stream.lock() {
                    s.emit_schema_event(PERF_PHASE_SCHEMA, "event", payload);
                }
            }
            Self::Stderr(stream) => {
                if let Ok(mut s) = stream.lock() {
                    s.emit_schema_event(PERF_PHASE_SCHEMA, "event", payload);
                }
            }
        }
    }
}

impl PhaseTimer {
    /// Start timing a named phase.
    #[must_use]
    pub fn start(operation: &'static str, phase: &'static str) -> Self {
        Self {
            operation,
            phase,
            started: Instant::now(),
            rss_start: current_rss_bytes(),
        }
    }

    /// Finish timing and build a payload.
    #[must_use]
    pub fn finish(self, bytes_in: u64, bytes_out: u64, item_count: u64) -> PerfPhasePayload {
        let payload = payload_from_elapsed(
            self.operation,
            self.phase,
            self.started.elapsed(),
            bytes_in,
            bytes_out,
            item_count,
            self.rss_start,
        );
        trace_phase(&payload);
        payload
    }
}

/// Build a phase payload from an externally measured duration.
#[must_use]
pub fn payload_from_elapsed(
    operation: impl Into<String>,
    phase: impl Into<String>,
    elapsed: Duration,
    bytes_in: u64,
    bytes_out: u64,
    item_count: u64,
    rss_start: Option<u64>,
) -> PerfPhasePayload {
    let rss_end = current_rss_bytes();
    let peak_rss_bytes = match (rss_start, rss_end) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (None, Some(b)) => Some(b),
        (Some(a), None) => Some(a),
        (None, None) => None,
    };

    PerfPhasePayload {
        operation: operation.into(),
        phase: phase.into(),
        elapsed_ms: elapsed.as_millis() as u64,
        bytes_in,
        bytes_out,
        item_count,
        peak_rss_bytes,
    }
}

/// Best-effort resident memory sample.
#[must_use]
pub fn current_rss_bytes() -> Option<u64> {
    memory_stats::memory_stats().map(|m| m.physical_mem as u64)
}

fn trace_phase(payload: &PerfPhasePayload) {
    tracing::info!(
        target: PERF_PHASE_SCHEMA,
        schema = PERF_PHASE_SCHEMA,
        schema_version = PERF_PHASE_SCHEMA_VERSION,
        operation = %payload.operation,
        phase = %payload.phase,
        elapsed_ms = payload.elapsed_ms,
        bytes_in = payload.bytes_in,
        bytes_out = payload.bytes_out,
        item_count = payload.item_count,
        peak_rss_bytes = payload.peak_rss_bytes,
        "perf.phase"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_timer_payload_contains_counts() {
        let payload = PhaseTimer::start("push", "xorb_pack").finish(1, 2, 3);
        assert_eq!(payload.operation, "push");
        assert_eq!(payload.phase, "xorb_pack");
        assert_eq!(payload.bytes_in, 1);
        assert_eq!(payload.bytes_out, 2);
        assert_eq!(payload.item_count, 3);
    }
}
