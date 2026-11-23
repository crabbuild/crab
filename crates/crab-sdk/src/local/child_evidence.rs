//! Relay only bounded, redacted CDN diagnostics from the selected Crab child.

use serde_json::{Map, Value};

const TARGET: &str = "crab_sdk::cdn::child";
pub(super) const FILTER: &str = "off,crab::cdn::checkout_target=debug,crab_storage::cdn::transfer=debug,crab_storage::native_read=debug,crab_storage::cdn::gcs_viewer_log=trace";

pub(super) fn enabled() -> bool {
    tracing::enabled!(target: TARGET, tracing::Level::DEBUG)
}

pub(super) fn relay(stderr: &[u8]) {
    for line in stderr
        .split(|byte| *byte == b'\n')
        .filter(|line| line.len() <= 4096)
    {
        if let Some(event) = decode(line) {
            tracing::debug!(target: TARGET, parent: None, observation = %event);
        }
    }
}

fn decode(line: &[u8]) -> Option<Value> {
    let event: Value = serde_json::from_slice(line).ok()?;
    let target = event["target"].as_str()?;
    let fields = event["fields"].as_object()?;
    let value = |name| fields.get(name).and_then(Value::as_str).unwrap_or_default();
    let valid = match (target, value("message")) {
        ("crab::cdn::checkout_target", "CDN child captured read target") => {
            fields.len() == 8
                && fingerprints(
                    fields,
                    &[
                        "origin_sha256",
                        "prefix_sha256",
                        "global_prefix_sha256",
                        "cache_root_sha256",
                    ],
                )
                && optional_fingerprint(fields, "binding_sha256")
                && optional_fingerprint(fields, "approval_sha256")
                && fields
                    .get("local_cache_only")
                    .is_some_and(Value::is_boolean)
        }
        ("crab_storage::native_read", "Native object store read") => {
            fields.len() == 5
                && provider(value("provider"))
                && matches!(value("method"), "get" | "head")
                && http_status(fields)
                && unsigned(fields, "bytes")
        }
        ("crab_storage::cdn::transfer", "CDN immutable GET") => {
            fields.len() == 16
                && route(fields)
                && http_status(fields)
                && unsigned(fields, "bytes")
                && unsigned(fields, "elapsed_ms")
                && unsigned(fields, "object_bytes")
                && unsigned(fields, "requested_bytes")
                && unsigned(fields, "range_start")
                && fields.get("range").is_some_and(Value::is_boolean)
                && fields.get("complete").is_some_and(Value::is_boolean)
                && matches!(value("cache"), "hit" | "miss" | "unknown")
                && optional_fingerprint(fields, "request_sha256")
        }
        ("crab_storage::cdn::transfer", "CDN verified xorb payload") => {
            fields.len() == 8
                && route(fields)
                && value("family") == "xorb"
                && matches!(value("source"), "edge" | "local" | "origin")
                && fingerprints(fields, &["xorb_key_sha256"])
        }
        _ => false,
    };
    valid.then(|| serde_json::json!({"target": target, "fields": fields}))
}

fn provider(name: &str) -> bool {
    matches!(name, "s3" | "gcs" | "azure")
}

fn route(fields: &Map<String, Value>) -> bool {
    let value = |name| fields.get(name).and_then(Value::as_str).unwrap_or_default();
    provider(value("provider"))
        && matches!(value("access"), "public" | "private")
        && matches!(
            value("consumer"),
            "sdk" | "desktop" | "cli" | "mount" | "server" | "s3-gateway"
        )
        && matches!(value("profile"), "direct" | "managed")
        && matches!(
            value("family"),
            "git" | "capsule" | "lfs" | "xorb" | "chunk-page" | "shard"
        )
}

fn http_status(fields: &Map<String, Value>) -> bool {
    matches!(
        fields.get("status").and_then(Value::as_u64),
        Some(0 | 100..=599)
    )
}

fn unsigned(fields: &Map<String, Value>, name: &str) -> bool {
    fields.get(name).and_then(Value::as_u64).is_some()
}

fn fingerprints(fields: &Map<String, Value>, names: &[&str]) -> bool {
    names.iter().all(|name| {
        fields
            .get(*name)
            .and_then(Value::as_str)
            .is_some_and(fingerprint)
    })
}

fn optional_fingerprint(fields: &Map<String, Value>, name: &str) -> bool {
    fields
        .get(name)
        .and_then(Value::as_str)
        .is_some_and(|value| value.is_empty() || fingerprint(value))
}

fn fingerprint(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_evidence_relays_redacted_range_sizes() {
        let mut event = serde_json::json!({
            "target": "crab_storage::cdn::transfer",
            "fields": {
                "message": "CDN immutable GET", "provider": "s3", "access": "private",
                "consumer": "sdk", "profile": "direct", "family": "xorb",
                "status": 206, "cache": "hit", "range": true, "range_start": 8, "object_bytes": 32,
                "requested_bytes": 8, "bytes": 8, "complete": true, "elapsed_ms": 1,
                "request_sha256": "a".repeat(64),
            },
        });
        assert_eq!(
            decode(&serde_json::to_vec(&event).unwrap()),
            Some(event.clone())
        );
        event["fields"]["range_start"] = serde_json::json!("unexpected string");
        assert!(decode(&serde_json::to_vec(&event).unwrap()).is_none());
    }

    #[test]
    fn child_evidence_only_retains_allowlisted_fields() {
        let payload = serde_json::json!({
            "message": "CDN verified xorb payload", "provider": "s3", "access": "private",
            "consumer": "sdk", "profile": "direct", "family": "xorb", "source": "edge",
            "xorb_key_sha256": "a".repeat(64),
        });
        let mut event = serde_json::json!({
            "target": "crab_storage::cdn::transfer", "fields": payload,
            "span": {"cookie": "ignored secret"},
        });
        let redacted = decode(&serde_json::to_vec(&event).unwrap()).unwrap();
        assert_eq!(redacted["fields"], payload);
        assert_eq!(redacted.as_object().unwrap().len(), 2);
        event["fields"]["xorb_key_sha256"] = serde_json::json!("not-a-fingerprint");
        assert!(decode(&serde_json::to_vec(&event).unwrap()).is_none());
        event["fields"] = payload;
        event["fields"]["authorization"] = serde_json::json!("unexpected secret");
        assert!(decode(&serde_json::to_vec(&event).unwrap()).is_none());
    }
}
