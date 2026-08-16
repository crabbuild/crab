//! Envelope types for structured JSON / JSONL output.
//!
//! `Envelope<T>` wraps single-result `--json` responses.
//! `EventEnvelope<T>` wraps individual `--jsonl` streaming events.

use schemars::JsonSchema;
use serde::Serialize;

use super::error_info::ErrorInfo;
use crab_types::time::now_rfc3339_millis;

/// Outer wrapper for every `--json` response.
///
/// Exactly one of `data` or `error` is present. The `skip_serializing_if`
/// attributes keep the absent field out of the JSON entirely.
#[derive(Debug, Serialize, JsonSchema)]
#[schemars(bound = "T: JsonSchema")]
pub struct Envelope<T: Serialize> {
    /// Canonical command name, e.g. `"hydrate"`, `"daemon.list"`.
    pub schema: &'static str,
    /// Semver-ish version of this schema, e.g. `"1.0"`.
    pub version: &'static str,
    /// RFC 3339 UTC timestamp with millisecond precision.
    pub timestamp: String,
    /// Command-specific payload on success.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<T>,
    /// Structured error on failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorInfo>,
}

impl<T: Serialize> Envelope<T> {
    /// Build a success envelope with the current timestamp.
    pub fn ok(schema: &'static str, version: &'static str, data: T) -> Self {
        Self {
            schema,
            version,
            timestamp: now_rfc3339_millis(),
            data: Some(data),
            error: None,
        }
    }
}

impl Envelope<serde_json::Value> {
    /// Build an error envelope with the current timestamp.
    pub fn err(schema: &'static str, version: &'static str, error: ErrorInfo) -> Self {
        Self {
            schema,
            version,
            timestamp: now_rfc3339_millis(),
            data: None,
            error: Some(error),
        }
    }
}

/// Outer wrapper for each line in a `--jsonl` stream.
#[derive(Debug, Serialize, JsonSchema)]
#[schemars(bound = "T: JsonSchema")]
pub struct EventEnvelope<T: Serialize> {
    /// Canonical event schema name, e.g. `"hydrate.event"`.
    pub schema: &'static str,
    /// Schema version.
    pub version: &'static str,
    /// RFC 3339 UTC timestamp with millisecond precision.
    pub timestamp: String,
    /// Event discriminator: `"progress"`, `"file_done"`, `"xorb_done"`,
    /// `"warning"`, or `"result"`.
    #[serde(rename = "type")]
    pub event_type: String,
    /// Event-specific payload.
    pub data: T,
}

/// Terminal `"result"` event carrying a structured error instead of a
/// success payload. Used when a streaming command fails (e.g. cancelled).
#[derive(Debug, Serialize, JsonSchema)]
pub struct ErrorEventEnvelope {
    pub schema: &'static str,
    pub version: &'static str,
    pub timestamp: String,
    #[serde(rename = "type")]
    pub event_type: String,
    pub error: ErrorInfo,
}
