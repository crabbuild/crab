use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use utoipa::ToSchema;
use uuid::Uuid;

use super::PageCursor;

/// Audit decision recorded by the managed service.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AuditDecision {
    Allowed,
    Denied,
}

/// Tenant-visible immutable audit event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct AuditEvent {
    pub id: Uuid,
    pub repository_id: Option<Uuid>,
    pub actor_key: String,
    pub action: String,
    pub decision: AuditDecision,
    pub request_id: String,
    pub resource_version: Option<i64>,
    pub source_category: String,
    pub reason_code: Option<String>,
    pub details: serde_json::Value,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub occurred_at: OffsetDateTime,
}

/// Cursor-paginated tenant audit events.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct AuditEventPage {
    pub schema_version: u16,
    pub events: Vec<AuditEvent>,
    pub next_cursor: Option<PageCursor>,
}

/// Supported audit-export encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum AuditExportFormat {
    JsonLines,
}

/// Request for an integrity-protected tenant audit export.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct AuditExportRequest {
    pub schema_version: u16,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub from: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub to: OffsetDateTime,
    pub format: AuditExportFormat,
}

/// Durable operation returned after an audit export is accepted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct AuditExportOperation {
    pub schema_version: u16,
    pub job_id: Uuid,
    pub state: JobState,
}

/// Usage dimension supported by the managed service.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum UsageCategory {
    StoredBytes,
    UploadedBytes,
    DownloadedBytes,
    CacheEgressBytes,
    Requests,
    JobCompute,
}

/// One aggregated usage total within a requested window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct UsageTotal {
    pub repository_id: Option<Uuid>,
    pub category: UsageCategory,
    pub quantity: u64,
}

/// Tenant-scoped usage report derived from append-only observations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct UsageReport {
    pub schema_version: u16,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub from: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub to: OffsetDateTime,
    pub totals: Vec<UsageTotal>,
}

/// Durable job state visible through administrative APIs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Pending,
    Running,
    RetryWait,
    Completed,
    Cancelled,
    DeadLetter,
    Quarantined,
}

/// Sanitized durable-job representation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct JobSummary {
    pub schema_version: u16,
    pub id: Uuid,
    pub repository_id: Option<Uuid>,
    pub kind: String,
    pub state: JobState,
    pub attempts: u32,
    pub max_attempts: u32,
    pub payload_version: u16,
    pub last_error_code: Option<String>,
    pub revision: u64,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub available_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339::option")]
    #[schema(value_type = Option<String>, format = DateTime)]
    pub lease_expires_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub created_at: OffsetDateTime,
    #[serde(with = "time::serde::rfc3339")]
    #[schema(value_type = String, format = DateTime)]
    pub updated_at: OffsetDateTime,
}

/// Cursor-paginated job summaries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct JobPage {
    pub schema_version: u16,
    pub jobs: Vec<JobSummary>,
    pub next_cursor: Option<PageCursor>,
}

/// Operator reason supplied for a job state transition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct JobActionRequest {
    pub schema_version: u16,
    pub reason: String,
}

/// Operator request to promote one verified read replica to primary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PromoteReplicaRequest {
    pub schema_version: u16,
    pub placement_generation: i64,
    pub replica_revision: i64,
    pub reason: String,
}

/// Durable promotion job accepted by the managed service.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct PromoteReplicaResponse {
    pub schema_version: u16,
    pub job_id: Uuid,
    pub state: JobState,
}
