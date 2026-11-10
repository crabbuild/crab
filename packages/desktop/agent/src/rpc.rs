//! JSON-RPC 2.0 framing helpers.
//!
//! We do not pull in a full rpc crate because the wire contract is
//! tiny and the sidecar only speaks to one peer. The types here match
//! the [JSON-RPC 2.0 spec](https://www.jsonrpc.org/specification) —
//! `jsonrpc: "2.0"`, optional string/number id, and either a `result`
//! or `error` field on responses.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Request id — strings and numbers are both allowed by the spec;
/// notifications omit the field entirely. We model the whole space
/// with an opaque `Value` so passthrough works without a custom
/// deserializer.
pub type RequestId = Value;

#[derive(Debug, Clone, Deserialize)]
pub struct Request {
    #[allow(dead_code)]
    pub jsonrpc: Option<String>,
    pub id: Option<RequestId>,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

#[derive(Debug, Serialize)]
pub struct Response {
    pub jsonrpc: &'static str,
    pub id: Option<RequestId>,
    #[serde(flatten)]
    pub body: ResponseBody,
}

/// Flattened into the outer `Response` so the wire shape is exactly
/// `{ "jsonrpc": "2.0", "id": ..., "result": ... }` or
/// `{ "jsonrpc": "2.0", "id": ..., "error": { ... } }`.
#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum ResponseBody {
    Ok { result: Value },
    Err { error: RpcError },
}

impl ResponseBody {
    pub fn ok(value: Value) -> Self {
        Self::Ok { result: value }
    }
    pub fn err(err: RpcError) -> Self {
        Self::Err { error: err }
    }
}

impl Response {
    pub fn new(id: Option<RequestId>, body: ResponseBody) -> Self {
        Self {
            jsonrpc: "2.0",
            id,
            body,
        }
    }

    pub fn error(id: Option<RequestId>, err: RpcError) -> Self {
        Self::new(id, ResponseBody::err(err))
    }
}

#[derive(Debug, Serialize)]
pub struct Notification<T: Serialize> {
    pub jsonrpc: &'static str,
    pub method: String,
    pub params: T,
}

impl<T: Serialize> Notification<T> {
    pub fn new(method: &str, params: T) -> Self {
        Self {
            jsonrpc: "2.0",
            method: method.to_owned(),
            params,
        }
    }
}

#[derive(Debug, Serialize, Clone)]
pub struct RpcError {
    pub code: i32,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    pub fn parse_error(e: &serde_json::Error) -> Self {
        Self {
            code: -32700,
            message: format!("Parse error: {e}"),
            data: None,
        }
    }

    #[allow(
        dead_code,
        reason = "kept for completeness of the JSON-RPC error surface"
    )]
    pub fn method_not_found(method: &str) -> Self {
        Self {
            code: -32601,
            message: format!("Method not found: {method}"),
            data: None,
        }
    }

    #[allow(
        dead_code,
        reason = "kept for completeness of the JSON-RPC error surface"
    )]
    pub fn invalid_params(method: &str, e: &serde_json::Error) -> Self {
        Self {
            code: -32602,
            message: format!("Invalid params for {method}: {e}"),
            data: None,
        }
    }
}

impl From<crate::AgentError> for RpcError {
    fn from(err: crate::AgentError) -> Self {
        use crate::AgentError as E;
        let mut detail = &err;
        while let E::Shared(source) = detail {
            detail = source;
        }
        let (code, data) = match detail {
            E::UnknownMethod(_) => (-32601, None),
            E::InvalidParams { .. } => (-32602, None),
            E::Sdk(_) => (1001, Some(serde_json::json!({ "kind": "sdk" }))),
            E::Cli { status, stderr } => (
                1002,
                Some(serde_json::json!({
                    "kind": "cli",
                    "status": status,
                    "stderr": stderr,
                })),
            ),
            E::Io(_) => (1003, None),
            E::Command {
                status, details, ..
            } => (
                1002,
                Some(serde_json::json!({ "kind": "cli", "status": status, "error": details })),
            ),
            E::NativeRead { code, .. } => (
                1012,
                Some(serde_json::json!({ "kind": "content_read", "code": code })),
            ),
            E::Integrity {
                algorithm,
                expected,
                actual,
            } => (
                1012,
                Some(
                    serde_json::json!({ "kind": "content_read", "code": "integrity", "algorithm": algorithm, "expected": expected, "actual": actual }),
                ),
            ),
            E::Decode(_) => (-32700, None),
            E::RepoNotOpen(_) => (1004, None),
            E::StaleHunk {
                path,
                expected_old_start,
                expected_old_lines,
            } => (
                1005,
                Some(serde_json::json!({
                    "kind": "stale_hunk",
                    "path": path,
                    "expected_old_start": expected_old_start,
                    "expected_old_lines": expected_old_lines,
                })),
            ),
            E::PatchApply { stderr } => (
                1006,
                Some(serde_json::json!({
                    "kind": "patch_apply",
                    "stderr": stderr,
                })),
            ),
            E::OpCancelled { op_id } => (
                1010,
                Some(serde_json::json!({
                    "kind": "op_cancelled",
                    "op_id": op_id,
                })),
            ),
            E::Managed {
                kind, remediation, ..
            } => (
                1011,
                Some(serde_json::json!({
                    "kind": kind,
                    "remediation": remediation,
                })),
            ),
            E::PathNotFound { .. } => (1004, None),
            E::Storage(_)
            | E::Cache(_)
            | E::Utf8(_)
            | E::ParsedUrl(_)
            | E::Url(_)
            | E::Auth(_)
            | E::Config(_)
            | E::Workflow(_)
            | E::Yaml(_)
            | E::Shared(_)
            | E::Cleanup { .. }
            | E::Preview { .. }
            | E::Other(_) => (1000, None),
        };
        Self {
            code,
            message: err.to_string(),
            data,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentError;

    #[test]
    fn cancelled_setup_with_failed_cleanup_remains_a_terminal_failure() {
        let error = AgentError::Cleanup {
            operation: Box::new(AgentError::OpCancelled {
                op_id: "opening".into(),
            }),
            cleanup: Box::new(AgentError::Io(std::io::Error::other("close failed"))),
        };
        let cancelled = error.is_cancelled();
        let response = RpcError::from(error);
        assert!(!cancelled && response.code == 1000 && response.message.contains("close failed"));
    }
}
