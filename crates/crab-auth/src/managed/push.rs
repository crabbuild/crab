use serde::{Deserialize, Serialize};
use serde_json::Value;
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;

use super::TransferGrant;
use crate::PushRefUpdate;

/// Client estimates used for push quota admission.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PushAdmissionPlan {
    pub estimated_bytes: u64,
    pub estimated_objects: u64,
}

/// Optional replication identity bound to a managed push session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum PushReplicationRequest {
    ActiveActive {
        writer: String,
        configuration: Value,
    },
}

/// Versioned request for a durable managed push session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PushPrepareRequest {
    pub schema_version: u16,
    pub repository_id: Uuid,
    pub ref_updates: Vec<PushRefUpdate>,
    pub plan: PushAdmissionPlan,
    pub client_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replication: Option<PushReplicationRequest>,
}

/// Durable session identity and exact staging authorization for one push.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PushPrepareResponse {
    pub schema_version: u16,
    pub push_id: Uuid,
    pub repository_id: Uuid,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub expires_at: OffsetDateTime,
    pub base_manifest_generation: u64,
    pub base_manifest_etag: String,
    pub staging_grant: TransferGrant,
}

/// Versioned request to finalize one durable managed push session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PushFinalizeRequest {
    pub schema_version: u16,
    pub repository_id: Uuid,
    pub ref_updates: Vec<PushRefUpdate>,
    pub plan: PushAdmissionPlan,
    pub client_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replication: Option<PushReplicationRequest>,
}

/// Durable response after a managed push session is aborted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PushAbortResponse {
    pub schema_version: u16,
    pub push_id: Uuid,
    pub state: String,
}
