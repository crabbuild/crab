use schemars::JsonSchema;
use serde::Serialize;

/// Broad classification bucket for Crab errors.
///
/// Serializes to lowercase (`"transient"`, `"conflict"`, and so on) for
/// structured machine-readable error envelopes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ErrorCategory {
    Transient,
    Conflict,
    Integrity,
    Permanent,
    Config,
    Transport,
    Staging,
    Lfs,
    Internal,
    Cancelled,
}
