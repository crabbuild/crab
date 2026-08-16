//! Structured local audit event log.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use object_store::path::Path as ObjectPath;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::core::error::{CrabError, Result};
use crate::storage::Store;

pub const AUDIT_EVENT_SCHEMA: &str = "audit.event";
pub const AUDIT_LOG_SCHEMA: &str = "audit.log";
pub const AUDIT_VERIFY_SCHEMA: &str = "audit.verify";
pub const AUDIT_EXPORT_SCHEMA: &str = "audit.export";
pub const AUDIT_REMOTE_PUBLISH_SCHEMA: &str = "audit.remote_publish";
pub const AUDIT_SCHEMA_VERSION: &str = "1.0";

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum AuditOutcome {
    Success,
    Failure,
}

impl std::fmt::Display for AuditOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Success => f.write_str("success"),
            Self::Failure => f.write_str("failure"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AuditEvent {
    pub schema_version: String,
    pub event_id: String,
    pub timestamp_unix: u64,
    pub operation: String,
    pub outcome: AuditOutcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    #[serde(default)]
    pub details: Value,
    pub digest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct NewAuditEvent {
    pub operation: String,
    pub outcome: AuditOutcome,
    pub actor: Option<String>,
    pub repository: Option<String>,
    pub details: Value,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct AuditLogPayload {
    pub path: String,
    pub events: Vec<AuditEvent>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct AuditVerifyPayload {
    pub path: String,
    pub checked: usize,
    pub valid: usize,
    pub invalid: usize,
    pub issues: Vec<AuditIssue>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct AuditIssue {
    pub line: usize,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct AuditExportPayload {
    pub source_path: String,
    pub output_path: String,
    pub exported: usize,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct AuditRemotePublishPayload {
    pub path: String,
    pub event_id: String,
    pub digest: String,
}

#[derive(Serialize)]
struct AuditEventDigestInput<'a> {
    schema_version: &'a str,
    event_id: &'a str,
    timestamp_unix: u64,
    operation: &'a str,
    outcome: &'a AuditOutcome,
    actor: &'a Option<String>,
    repository: &'a Option<String>,
    details: &'a Value,
}

impl AuditEvent {
    pub fn new(input: NewAuditEvent) -> Self {
        let mut event = Self {
            schema_version: AUDIT_SCHEMA_VERSION.to_owned(),
            event_id: uuid::Uuid::now_v7().to_string(),
            timestamp_unix: unix_now(),
            operation: input.operation,
            outcome: input.outcome,
            actor: input.actor,
            repository: input.repository,
            details: redact_details(input.details),
            digest: String::new(),
        };
        event.digest = event.compute_digest();
        event
    }

    #[must_use]
    pub fn compute_digest(&self) -> String {
        let input = AuditEventDigestInput {
            schema_version: &self.schema_version,
            event_id: &self.event_id,
            timestamp_unix: self.timestamp_unix,
            operation: &self.operation,
            outcome: &self.outcome,
            actor: &self.actor,
            repository: &self.repository,
            details: &self.details,
        };
        let bytes = serde_json::to_vec(&input).unwrap_or_default();
        blake3::hash(&bytes).to_hex().to_string()
    }

    #[must_use]
    pub fn digest_valid(&self) -> bool {
        self.digest == self.compute_digest()
    }
}

#[must_use]
pub fn redact_details(mut value: Value) -> Value {
    redact_value(&mut value);
    value
}

fn redact_value(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                if is_sensitive_key(key) {
                    *child = Value::String("[REDACTED]".to_owned());
                } else {
                    redact_value(child);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                redact_value(item);
            }
        }
        _ => {}
    }
}

fn is_sensitive_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    [
        "secret",
        "token",
        "password",
        "credential",
        "access_key",
        "private_key",
        "session_key",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

#[must_use]
pub fn default_log_path() -> PathBuf {
    PathBuf::from(".crab/audit/events.jsonl")
}

pub fn append_event(path: &Path, event: &AuditEvent) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    let mut bytes = serde_json::to_vec(event)
        .map_err(|err| CrabError::Internal(format!("serialize audit event: {err}")))?;
    bytes.push(b'\n');
    file.write_all(&bytes)?;
    Ok(())
}

pub async fn publish_remote_event(
    store: &Store,
    repo_prefix: &str,
    event: &AuditEvent,
) -> Result<AuditRemotePublishPayload> {
    let path = remote_event_object_path(repo_prefix, event);
    let mut bytes = serde_json::to_vec(event)
        .map_err(|err| CrabError::Internal(format!("serialize remote audit event: {err}")))?;
    bytes.push(b'\n');
    store.create_strict(&path, Bytes::from(bytes)).await?;
    Ok(AuditRemotePublishPayload {
        path: path.to_string(),
        event_id: event.event_id.clone(),
        digest: event.digest.clone(),
    })
}

#[must_use]
pub fn remote_event_object_path(repo_prefix: &str, event: &AuditEvent) -> ObjectPath {
    let prefix = repo_prefix.trim_matches('/');
    let leaf = format!(
        "{}-{}.json",
        event.timestamp_unix,
        audit_path_component(&event.event_id)
    );
    if prefix.is_empty() {
        ObjectPath::from(format!(".crab/audit/events/{leaf}"))
    } else {
        ObjectPath::from(format!("{prefix}/.crab/audit/events/{leaf}"))
    }
}

fn audit_path_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

pub fn read_events(path: &Path) -> Result<Vec<AuditEvent>> {
    if !path.exists() {
        return Ok(Vec::new());
    }

    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut events = Vec::new();
    for (idx, line) in reader.lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let event =
            serde_json::from_str::<AuditEvent>(&line).map_err(|err| CrabError::CorruptObject {
                path: path.display().to_string(),
                reason: format!("invalid audit JSON at line {}: {err}", idx + 1),
            })?;
        events.push(event);
    }
    Ok(events)
}

pub fn verify_log(path: &Path) -> Result<AuditVerifyPayload> {
    if !path.exists() {
        return Ok(AuditVerifyPayload {
            path: path.display().to_string(),
            checked: 0,
            valid: 0,
            invalid: 0,
            issues: Vec::new(),
        });
    }

    let file = File::open(path)?;
    let reader = BufReader::new(file);
    let mut checked = 0usize;
    let mut valid = 0usize;
    let mut issues = Vec::new();
    let mut seen_event_ids = HashSet::new();
    let mut latest_timestamp = None;

    for (idx, line) in reader.lines().enumerate() {
        let line_no = idx + 1;
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        checked += 1;
        match serde_json::from_str::<AuditEvent>(&line) {
            Ok(event) if event.schema_version != AUDIT_SCHEMA_VERSION => {
                issues.push(AuditIssue {
                    line: line_no,
                    reason: format!("unsupported schema version {}", event.schema_version),
                });
            }
            Ok(event) if !event.digest_valid() => {
                issues.push(AuditIssue {
                    line: line_no,
                    reason: "digest mismatch".to_owned(),
                });
            }
            Ok(event) => {
                let mut sequence_issues = Vec::new();
                if !seen_event_ids.insert(event.event_id.clone()) {
                    sequence_issues.push(format!("duplicate event id {}", event.event_id));
                }
                if let Some(previous) = latest_timestamp
                    && event.timestamp_unix < previous
                {
                    sequence_issues.push(format!(
                        "timestamp regressed from {previous} to {}",
                        event.timestamp_unix
                    ));
                }
                latest_timestamp = Some(match latest_timestamp {
                    Some(previous) => previous.max(event.timestamp_unix),
                    None => event.timestamp_unix,
                });

                if sequence_issues.is_empty() {
                    valid += 1;
                } else {
                    issues.push(AuditIssue {
                        line: line_no,
                        reason: sequence_issues.join("; "),
                    });
                }
            }
            Err(err) => issues.push(AuditIssue {
                line: line_no,
                reason: format!("invalid JSON: {err}"),
            }),
        }
    }

    Ok(AuditVerifyPayload {
        path: path.display().to_string(),
        checked,
        valid,
        invalid: issues.len(),
        issues,
    })
}

pub fn export_events(source: &Path, output: &Path, operation: Option<&str>) -> Result<usize> {
    let events = filter_events(read_events(source)?, operation);
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = File::create(output)?;
    serde_json::to_writer_pretty(&mut file, &events)
        .map_err(|err| CrabError::Internal(format!("serialize audit export: {err}")))?;
    file.write_all(b"\n")?;
    Ok(events.len())
}

#[must_use]
pub fn filter_events(events: Vec<AuditEvent>, operation: Option<&str>) -> Vec<AuditEvent> {
    match operation {
        Some(op) => events
            .into_iter()
            .filter(|event| event.operation == op)
            .collect(),
        None => events,
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs())
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
    use std::sync::Arc;

    use object_store::memory::InMemory;
    use object_store::{ObjectStore, ObjectStoreExt};

    #[test]
    fn event_digest_detects_tampering() {
        let mut event = AuditEvent::new(NewAuditEvent {
            operation: "release.publish".to_owned(),
            outcome: AuditOutcome::Success,
            actor: Some("alice".to_owned()),
            repository: Some("crab://bucket/repo".to_owned()),
            details: serde_json::json!({"release": "v1"}),
        });

        assert!(event.digest_valid());
        event.operation = "gc.delete".to_owned();
        assert!(!event.digest_valid());
    }

    #[test]
    fn append_read_verify_and_filter_events() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("audit/events.jsonl");
        let first = AuditEvent::new(NewAuditEvent {
            operation: "release.publish".to_owned(),
            outcome: AuditOutcome::Success,
            actor: None,
            repository: None,
            details: serde_json::json!({"release": "v1"}),
        });
        let second = AuditEvent::new(NewAuditEvent {
            operation: "recover.apply".to_owned(),
            outcome: AuditOutcome::Failure,
            actor: None,
            repository: None,
            details: serde_json::json!({"items": 1}),
        });

        append_event(&path, &first).expect("append first");
        append_event(&path, &second).expect("append second");

        let events = read_events(&path).expect("read events");
        assert_eq!(events.len(), 2);

        let verify = verify_log(&path).expect("verify");
        assert_eq!(verify.checked, 2);
        assert_eq!(verify.invalid, 0);

        let filtered = filter_events(events, Some("recover.apply"));
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].operation, "recover.apply");
    }

    #[test]
    fn verify_reports_invalid_digest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.jsonl");
        let mut event = AuditEvent::new(NewAuditEvent {
            operation: "release.publish".to_owned(),
            outcome: AuditOutcome::Success,
            actor: None,
            repository: None,
            details: serde_json::json!({}),
        });
        event.digest = "bad".to_owned();
        append_event(&path, &event).expect("append");

        let verify = verify_log(&path).expect("verify");
        assert_eq!(verify.checked, 1);
        assert_eq!(verify.valid, 0);
        assert_eq!(verify.invalid, 1);
        assert_eq!(verify.issues[0].reason, "digest mismatch");
    }

    #[test]
    fn verify_reports_duplicate_event_ids() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.jsonl");
        let event = AuditEvent::new(NewAuditEvent {
            operation: "release.publish".to_owned(),
            outcome: AuditOutcome::Success,
            actor: None,
            repository: None,
            details: serde_json::json!({}),
        });

        append_event(&path, &event).expect("append first");
        append_event(&path, &event).expect("append duplicate");

        let verify = verify_log(&path).expect("verify");
        assert_eq!(verify.checked, 2);
        assert_eq!(verify.valid, 1);
        assert_eq!(verify.invalid, 1);
        assert!(verify.issues[0].reason.contains("duplicate event id"));
    }

    #[test]
    fn verify_reports_timestamp_regression() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("events.jsonl");
        let mut first = AuditEvent::new(NewAuditEvent {
            operation: "release.publish".to_owned(),
            outcome: AuditOutcome::Success,
            actor: None,
            repository: None,
            details: serde_json::json!({}),
        });
        first.timestamp_unix = 10;
        first.digest = first.compute_digest();
        let mut second = AuditEvent::new(NewAuditEvent {
            operation: "recover.apply".to_owned(),
            outcome: AuditOutcome::Success,
            actor: None,
            repository: None,
            details: serde_json::json!({}),
        });
        second.timestamp_unix = 5;
        second.digest = second.compute_digest();

        append_event(&path, &first).expect("append first");
        append_event(&path, &second).expect("append second");

        let verify = verify_log(&path).expect("verify");
        assert_eq!(verify.checked, 2);
        assert_eq!(verify.valid, 1);
        assert_eq!(verify.invalid, 1);
        assert_eq!(verify.issues[0].reason, "timestamp regressed from 10 to 5");
    }

    #[test]
    fn new_event_redacts_sensitive_detail_keys_before_digest() {
        let event = AuditEvent::new(NewAuditEvent {
            operation: "auth.grant".to_owned(),
            outcome: AuditOutcome::Success,
            actor: None,
            repository: None,
            details: serde_json::json!({
                "provider": "aws-oidc",
                "grant_type": "refresh_token",
                "access_token": "live-token",
                "nested": {
                    "password": "secret",
                    "safe": "visible"
                }
            }),
        });

        assert_eq!(event.operation, "auth.grant");
        assert_eq!(event.details["provider"], "aws-oidc");
        assert_eq!(event.details["grant_type"], "refresh_token");
        assert_eq!(event.details["access_token"], "[REDACTED]");
        assert_eq!(event.details["nested"]["password"], "[REDACTED]");
        assert_eq!(event.details["nested"]["safe"], "visible");
        assert!(event.digest_valid());
    }

    #[test]
    fn remote_event_object_path_uses_repo_audit_namespace() {
        let mut event = AuditEvent::new(NewAuditEvent {
            operation: "release.publish".to_owned(),
            outcome: AuditOutcome::Success,
            actor: None,
            repository: None,
            details: serde_json::json!({}),
        });
        event.timestamp_unix = 42;
        event.event_id = "event/with space".to_owned();

        let path = remote_event_object_path("/repos/demo/", &event);
        assert_eq!(
            path.as_ref(),
            "repos/demo/.crab/audit/events/42-event_with_space.json"
        );
    }

    #[tokio::test]
    async fn publish_remote_event_uses_create_if_absent() {
        let inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        let store = Store::new(Arc::clone(&inner));
        let event = AuditEvent::new(NewAuditEvent {
            operation: "release.publish".to_owned(),
            outcome: AuditOutcome::Success,
            actor: Some("alice".to_owned()),
            repository: Some("crab://bucket/repo".to_owned()),
            details: serde_json::json!({"release": "v1"}),
        });

        let payload = publish_remote_event(&store, "repo", &event)
            .await
            .expect("remote audit publish");
        assert_eq!(payload.event_id, event.event_id);
        assert_eq!(payload.digest, event.digest);

        let path = ObjectPath::from(payload.path.clone());
        let bytes = inner
            .get(&path)
            .await
            .expect("remote audit object")
            .bytes()
            .await
            .expect("remote audit bytes");
        let stored: AuditEvent = serde_json::from_slice(bytes.as_ref()).expect("audit JSON");
        assert_eq!(stored.event_id, event.event_id);
        assert!(stored.digest_valid());

        let err = publish_remote_event(&store, "repo", &event)
            .await
            .expect_err("second publish must conflict");
        assert!(
            matches!(err, CrabError::CasConflict { .. }),
            "expected CasConflict, got {err:?}"
        );
    }
}
