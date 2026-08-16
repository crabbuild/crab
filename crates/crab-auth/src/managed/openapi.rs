use utoipa::openapi::security::{HttpAuthScheme, HttpBuilder, SecurityScheme};
use utoipa::{Modify, OpenApi};

use std::collections::{BTreeMap, BTreeSet};

use super::{
    ApiErrorEnvelope, AuditDecision, AuditEvent, AuditEventPage, AuditExportFormat,
    AuditExportOperation, AuditExportRequest, AuthFlow, CapabilityName, DirectObjectCredentials,
    DiscoveryCache, DiscoveryDocument, EntityTag, GatewayAccess, IdempotencyKey, JobActionRequest,
    JobPage, JobState, JobSummary, LogicalRepository, OidcClientDiscovery, PageCursor,
    PromoteReplicaRequest, PromoteReplicaResponse, ProviderCredentials, PushAbortResponse,
    PushAdmissionPlan, PushFinalizeRequest, PushPrepareRequest, PushPrepareResponse,
    PushReplicationRequest, RepositoryState, SecretString, ServiceCapabilities, StagingScope,
    TransferGrant, TransferGrantRequest, TransferMode, TransferOperation, TransferPermission,
    TransferScope, TransferTransport, UsageCategory, UsageReport, UsageTotal,
};
use crate::PushFinalizeResponse;

#[utoipa::path(
    get,
    path = "/.well-known/crab",
    tag = "managed",
    responses(
        (status = 200, description = "Managed service discovery", body = DiscoveryDocument),
        (status = 503, description = "Discovery is unavailable", body = ApiErrorEnvelope)
    )
)]
#[expect(
    dead_code,
    reason = "OpenAPI-only operation registered by ManagedApiDoc"
)]
fn discover_service() {}

#[utoipa::path(
    get,
    path = "/v1/capabilities",
    tag = "managed",
    responses(
        (status = 200, description = "Negotiated service capabilities", body = ServiceCapabilities),
        (status = 401, description = "Authentication required", body = ApiErrorEnvelope)
    ),
    security(("bearer_auth" = []))
)]
#[expect(
    dead_code,
    reason = "OpenAPI-only operation registered by ManagedApiDoc"
)]
fn service_capabilities() {}

#[utoipa::path(
    get,
    path = "/v1/repositories/{organization}/{repository}",
    tag = "managed",
    params(
        ("organization" = String, Path, description = "Organization slug"),
        ("repository" = String, Path, description = "Repository slug")
    ),
    responses(
        (status = 200, description = "Logical repository", body = LogicalRepository),
        (status = 401, description = "Authentication required", body = ApiErrorEnvelope),
        (status = 404, description = "Repository absent or inaccessible", body = ApiErrorEnvelope)
    ),
    security(("bearer_auth" = []))
)]
#[expect(
    dead_code,
    reason = "OpenAPI-only operation registered by ManagedApiDoc"
)]
fn resolve_repository() {}

#[utoipa::path(
    post,
    path = "/v1/repositories/{organization}/{repository}/transfer-grants",
    tag = "managed",
    params(
        ("organization" = String, Path, description = "Organization slug"),
        ("repository" = String, Path, description = "Repository slug")
    ),
    request_body = TransferGrantRequest,
    responses(
        (status = 201, description = "Short-lived transfer grant", body = TransferGrant),
        (status = 400, description = "Invalid grant request", body = ApiErrorEnvelope),
        (status = 401, description = "Authentication required", body = ApiErrorEnvelope),
        (status = 403, description = "Transfer is not authorized", body = ApiErrorEnvelope),
        (status = 404, description = "Repository absent or inaccessible", body = ApiErrorEnvelope),
        (status = 429, description = "Transfer grant rate limited", body = ApiErrorEnvelope),
        (status = 503, description = "Transfer provider unavailable", body = ApiErrorEnvelope)
    ),
    security(("bearer_auth" = []))
)]
#[expect(
    dead_code,
    reason = "OpenAPI-only operation registered by ManagedApiDoc"
)]
fn issue_transfer_grant() {}

#[utoipa::path(
    post,
    path = "/v1/repositories/{organization}/{repository}/pushes",
    tag = "managed",
    params(
        ("organization" = String, Path, description = "Organization slug"),
        ("repository" = String, Path, description = "Repository slug"),
        ("Idempotency-Key" = String, Header, description = "Unique push preparation key")
    ),
    request_body = PushPrepareRequest,
    responses(
        (status = 201, description = "Durable push session and staging grant", body = PushPrepareResponse),
        (status = 400, description = "Invalid push request", body = ApiErrorEnvelope),
        (status = 401, description = "Authentication required", body = ApiErrorEnvelope),
        (status = 404, description = "Repository absent or inaccessible", body = ApiErrorEnvelope),
        (status = 409, description = "Ref, quota, concurrency, or idempotency conflict", body = ApiErrorEnvelope),
        (status = 429, description = "Push admission limit reached", body = ApiErrorEnvelope),
        (status = 503, description = "Catalog, storage, or transfer provider unavailable", body = ApiErrorEnvelope)
    ),
    security(("bearer_auth" = []))
)]
#[expect(
    dead_code,
    reason = "OpenAPI-only operation registered by ManagedApiDoc"
)]
fn prepare_push() {}

#[utoipa::path(
    post,
    path = "/v1/repositories/{organization}/{repository}/pushes/{push_id}:finalize",
    tag = "managed",
    params(
        ("organization" = String, Path, description = "Organization slug"),
        ("repository" = String, Path, description = "Repository slug"),
        ("push_id" = uuid::Uuid, Path, description = "Prepared push session ID")
    ),
    request_body = PushFinalizeRequest,
    responses(
        (status = 200, description = "Protected push committed", body = PushFinalizeResponse),
        (status = 400, description = "Invalid finalize request", body = ApiErrorEnvelope),
        (status = 401, description = "Authentication required", body = ApiErrorEnvelope),
        (status = 404, description = "Repository or push absent or inaccessible", body = ApiErrorEnvelope),
        (status = 409, description = "Replay, ref, session, or serialization conflict", body = ApiErrorEnvelope),
        (status = 422, description = "Staged push failed receive verification", body = ApiErrorEnvelope),
        (status = 429, description = "Current push quota rejects finalization", body = ApiErrorEnvelope),
        (status = 503, description = "Catalog, storage, or coordination unavailable", body = ApiErrorEnvelope)
    ),
    security(("bearer_auth" = []))
)]
#[expect(
    dead_code,
    reason = "OpenAPI-only operation registered by ManagedApiDoc"
)]
fn finalize_push() {}

#[utoipa::path(
    post,
    path = "/v1/repositories/{organization}/{repository}/pushes/{push_id}:abort",
    tag = "managed",
    params(
        ("organization" = String, Path, description = "Organization slug"),
        ("repository" = String, Path, description = "Repository slug"),
        ("push_id" = uuid::Uuid, Path, description = "Prepared push session ID")
    ),
    responses(
        (status = 200, description = "Push session aborted or already aborted", body = PushAbortResponse),
        (status = 400, description = "Invalid abort request", body = ApiErrorEnvelope),
        (status = 401, description = "Authentication required", body = ApiErrorEnvelope),
        (status = 403, description = "Only the session owner or a repository administrator may abort", body = ApiErrorEnvelope),
        (status = 404, description = "Repository or push absent or inaccessible", body = ApiErrorEnvelope),
        (status = 409, description = "Committed, terminal, expired, or actively finalizing session", body = ApiErrorEnvelope),
        (status = 503, description = "Catalog unavailable", body = ApiErrorEnvelope)
    ),
    security(("bearer_auth" = []))
)]
#[expect(
    dead_code,
    reason = "OpenAPI-only operation registered by ManagedApiDoc"
)]
fn abort_push() {}

#[utoipa::path(
    post,
    path = "/v1/repositories/{organization}/{repository}/replicas/{replica}/promotion",
    tag = "managed",
    params(
        ("organization" = String, Path, description = "Organization slug"),
        ("repository" = String, Path, description = "Repository slug"),
        ("replica" = String, Path, description = "Configured replica name"),
        ("Idempotency-Key" = String, Header, description = "Unique promotion request key")
    ),
    request_body = PromoteReplicaRequest,
    responses(
        (status = 202, description = "Fenced promotion job accepted", body = PromoteReplicaResponse),
        (status = 400, description = "Invalid promotion request", body = ApiErrorEnvelope),
        (status = 401, description = "Authentication required", body = ApiErrorEnvelope),
        (status = 404, description = "Repository or replica absent or inaccessible", body = ApiErrorEnvelope),
        (status = 409, description = "Replica, placement, or idempotency conflict", body = ApiErrorEnvelope),
        (status = 503, description = "Catalog unavailable", body = ApiErrorEnvelope)
    ),
    security(("bearer_auth" = []))
)]
#[expect(
    dead_code,
    reason = "OpenAPI-only operation registered by ManagedApiDoc"
)]
fn promote_replica() {}

#[utoipa::path(
    get,
    path = "/v1/organizations/{organization}/audit-events",
    tag = "managed",
    params(
        ("organization" = String, Path, description = "Organization slug"),
        ("from" = Option<String>, Query, description = "Inclusive RFC 3339 lower bound"),
        ("to" = Option<String>, Query, description = "Exclusive RFC 3339 upper bound"),
        ("repository_id" = Option<uuid::Uuid>, Query, description = "Repository filter"),
        ("action" = Option<String>, Query, description = "Exact audit action filter"),
        ("decision" = Option<AuditDecision>, Query, description = "Authorization decision filter"),
        ("cursor" = Option<PageCursor>, Query, description = "Opaque next-page cursor"),
        ("limit" = Option<u16>, Query, description = "Page size, at most 100")
    ),
    responses(
        (status = 200, description = "Tenant audit events", body = AuditEventPage),
        (status = 400, description = "Invalid filter or cursor", body = ApiErrorEnvelope),
        (status = 401, description = "Authentication required", body = ApiErrorEnvelope),
        (status = 403, description = "Administrative role required", body = ApiErrorEnvelope)
    ),
    security(("bearer_auth" = []))
)]
#[expect(
    dead_code,
    reason = "OpenAPI-only operation registered by ManagedApiDoc"
)]
fn list_audit_events() {}

#[utoipa::path(
    post,
    path = "/v1/organizations/{organization}/audit-exports",
    tag = "managed",
    params(
        ("organization" = String, Path, description = "Organization slug"),
        ("Idempotency-Key" = String, Header, description = "Unique mutation key")
    ),
    request_body = AuditExportRequest,
    responses(
        (status = 202, description = "Audit export accepted", body = AuditExportOperation),
        (status = 400, description = "Invalid export request", body = ApiErrorEnvelope),
        (status = 401, description = "Authentication required", body = ApiErrorEnvelope),
        (status = 403, description = "Administrative role required", body = ApiErrorEnvelope),
        (status = 409, description = "Idempotency conflict", body = ApiErrorEnvelope),
        (status = 503, description = "Catalog unavailable", body = ApiErrorEnvelope)
    ),
    security(("bearer_auth" = []))
)]
#[expect(
    dead_code,
    reason = "OpenAPI-only operation registered by ManagedApiDoc"
)]
fn request_audit_export() {}

#[utoipa::path(
    get,
    path = "/v1/organizations/{organization}/audit-exports/{job}",
    tag = "managed",
    params(
        ("organization" = String, Path, description = "Organization slug"),
        ("job" = uuid::Uuid, Path, description = "Completed audit-export job")
    ),
    responses(
        (status = 200, description = "Hash-chained JSON Lines audit export", body = String, content_type = "application/x-ndjson"),
        (status = 401, description = "Authentication required", body = ApiErrorEnvelope),
        (status = 403, description = "Administrative role required", body = ApiErrorEnvelope),
        (status = 404, description = "Export not found in this tenant", body = ApiErrorEnvelope),
        (status = 409, description = "Export is not complete", body = ApiErrorEnvelope),
        (status = 503, description = "Catalog or archive unavailable", body = ApiErrorEnvelope)
    ),
    security(("bearer_auth" = []))
)]
#[expect(
    dead_code,
    reason = "OpenAPI-only operation registered by ManagedApiDoc"
)]
fn download_audit_export() {}

#[utoipa::path(
    get,
    path = "/v1/organizations/{organization}/usage",
    tag = "managed",
    params(
        ("organization" = String, Path, description = "Organization slug"),
        ("from" = Option<String>, Query, description = "Inclusive RFC 3339 lower bound"),
        ("to" = Option<String>, Query, description = "Exclusive RFC 3339 upper bound"),
        ("repository_id" = Option<uuid::Uuid>, Query, description = "Repository filter"),
        ("category" = Option<UsageCategory>, Query, description = "Usage category filter")
    ),
    responses(
        (status = 200, description = "Tenant usage window", body = UsageReport),
        (status = 400, description = "Invalid usage filter", body = ApiErrorEnvelope),
        (status = 401, description = "Authentication required", body = ApiErrorEnvelope),
        (status = 403, description = "Billing or administrative role required", body = ApiErrorEnvelope)
    ),
    security(("bearer_auth" = []))
)]
#[expect(
    dead_code,
    reason = "OpenAPI-only operation registered by ManagedApiDoc"
)]
fn query_usage() {}

#[utoipa::path(
    get,
    path = "/v1/organizations/{organization}/jobs",
    tag = "managed",
    params(
        ("organization" = String, Path, description = "Organization slug"),
        ("state" = Option<JobState>, Query, description = "Job state filter"),
        ("kind" = Option<String>, Query, description = "Job kind filter"),
        ("cursor" = Option<PageCursor>, Query, description = "Opaque next-page cursor"),
        ("limit" = Option<u16>, Query, description = "Page size, at most 100")
    ),
    responses(
        (status = 200, description = "Sanitized durable jobs", body = JobPage),
        (status = 400, description = "Invalid filter or cursor", body = ApiErrorEnvelope),
        (status = 401, description = "Authentication required", body = ApiErrorEnvelope),
        (status = 403, description = "Administrative role required", body = ApiErrorEnvelope)
    ),
    security(("bearer_auth" = []))
)]
#[expect(
    dead_code,
    reason = "OpenAPI-only operation registered by ManagedApiDoc"
)]
fn list_jobs() {}

#[utoipa::path(
    get,
    path = "/v1/organizations/{organization}/jobs/{job}",
    tag = "managed",
    params(
        ("organization" = String, Path, description = "Organization slug"),
        ("job" = uuid::Uuid, Path, description = "Job ID")
    ),
    responses(
        (status = 200, description = "Sanitized durable job", body = JobSummary),
        (status = 401, description = "Authentication required", body = ApiErrorEnvelope),
        (status = 403, description = "Administrative role required", body = ApiErrorEnvelope),
        (status = 404, description = "Job absent or inaccessible", body = ApiErrorEnvelope)
    ),
    security(("bearer_auth" = []))
)]
#[expect(
    dead_code,
    reason = "OpenAPI-only operation registered by ManagedApiDoc"
)]
fn inspect_job() {}

#[utoipa::path(
    post,
    path = "/v1/organizations/{organization}/jobs/{job}:retry",
    tag = "managed",
    params(
        ("organization" = String, Path, description = "Organization slug"),
        ("job" = uuid::Uuid, Path, description = "Job ID"),
        ("If-Match" = String, Header, description = "Current job revision entity tag"),
        ("Idempotency-Key" = String, Header, description = "Unique mutation key")
    ),
    request_body = JobActionRequest,
    responses(
        (status = 200, description = "Job scheduled for retry", body = JobSummary),
        (status = 400, description = "Invalid retry request", body = ApiErrorEnvelope),
        (status = 401, description = "Authentication required", body = ApiErrorEnvelope),
        (status = 403, description = "Administrative role required", body = ApiErrorEnvelope),
        (status = 404, description = "Job absent or inaccessible", body = ApiErrorEnvelope),
        (status = 409, description = "Unsafe job state or idempotency conflict", body = ApiErrorEnvelope),
        (status = 412, description = "Job revision changed", body = ApiErrorEnvelope),
        (status = 428, description = "Revision precondition required", body = ApiErrorEnvelope),
        (status = 503, description = "Catalog unavailable", body = ApiErrorEnvelope)
    ),
    security(("bearer_auth" = []))
)]
#[expect(
    dead_code,
    reason = "OpenAPI-only operation registered by ManagedApiDoc"
)]
fn retry_job() {}

#[utoipa::path(
    post,
    path = "/v1/organizations/{organization}/jobs/{job}:cancel",
    tag = "managed",
    params(
        ("organization" = String, Path, description = "Organization slug"),
        ("job" = uuid::Uuid, Path, description = "Job ID"),
        ("If-Match" = String, Header, description = "Current job revision entity tag"),
        ("Idempotency-Key" = String, Header, description = "Unique mutation key")
    ),
    request_body = JobActionRequest,
    responses(
        (status = 200, description = "Job safely cancelled", body = JobSummary),
        (status = 400, description = "Invalid cancellation request", body = ApiErrorEnvelope),
        (status = 401, description = "Authentication required", body = ApiErrorEnvelope),
        (status = 403, description = "Administrative role required", body = ApiErrorEnvelope),
        (status = 404, description = "Job absent or inaccessible", body = ApiErrorEnvelope),
        (status = 409, description = "Unsafe job state or idempotency conflict", body = ApiErrorEnvelope),
        (status = 412, description = "Job revision changed", body = ApiErrorEnvelope),
        (status = 428, description = "Revision precondition required", body = ApiErrorEnvelope),
        (status = 503, description = "Catalog unavailable", body = ApiErrorEnvelope)
    ),
    security(("bearer_auth" = []))
)]
#[expect(
    dead_code,
    reason = "OpenAPI-only operation registered by ManagedApiDoc"
)]
fn cancel_job() {}

#[utoipa::path(
    post,
    path = "/v1/organizations/{organization}/jobs/{job}:quarantine",
    tag = "managed",
    params(
        ("organization" = String, Path, description = "Organization slug"),
        ("job" = uuid::Uuid, Path, description = "Job ID"),
        ("If-Match" = String, Header, description = "Current job revision entity tag"),
        ("Idempotency-Key" = String, Header, description = "Unique mutation key")
    ),
    request_body = JobActionRequest,
    responses(
        (status = 200, description = "Job quarantined", body = JobSummary),
        (status = 400, description = "Invalid quarantine request", body = ApiErrorEnvelope),
        (status = 401, description = "Authentication required", body = ApiErrorEnvelope),
        (status = 403, description = "Administrative role required", body = ApiErrorEnvelope),
        (status = 404, description = "Job absent or inaccessible", body = ApiErrorEnvelope),
        (status = 409, description = "Unsafe job state or idempotency conflict", body = ApiErrorEnvelope),
        (status = 412, description = "Job revision changed", body = ApiErrorEnvelope),
        (status = 428, description = "Revision precondition required", body = ApiErrorEnvelope),
        (status = 503, description = "Catalog unavailable", body = ApiErrorEnvelope)
    ),
    security(("bearer_auth" = []))
)]
#[expect(
    dead_code,
    reason = "OpenAPI-only operation registered by ManagedApiDoc"
)]
fn quarantine_job() {}

struct SecurityAddon;
impl Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let scheme = SecurityScheme::Http(
            HttpBuilder::new()
                .scheme(HttpAuthScheme::Bearer)
                .bearer_format("JWT or Crab service token")
                .build(),
        );
        if let Some(components) = &mut openapi.components {
            components.add_security_scheme("bearer_auth", scheme);
            return;
        }

        let mut components = utoipa::openapi::Components::new();
        components.add_security_scheme("bearer_auth", scheme);
        openapi.components = Some(components);
    }
}

/// Generated OpenAPI 3.1 contract for the initial managed API surface.
#[derive(OpenApi)]
#[openapi(
    paths(
        discover_service,
        service_capabilities,
        resolve_repository,
        issue_transfer_grant,
        prepare_push,
        finalize_push,
        abort_push,
        promote_replica,
        list_audit_events,
        request_audit_export,
        download_audit_export,
        query_usage,
        list_jobs,
        inspect_job,
        retry_job,
        cancel_job,
        quarantine_job
    ),
    modifiers(&SecurityAddon),
    components(
        schemas(
            ApiErrorEnvelope,
            AuditDecision,
            AuditEvent,
            AuditEventPage,
            AuditExportFormat,
            AuditExportOperation,
            AuditExportRequest,
            AuthFlow,
            CapabilityName,
            DirectObjectCredentials,
            DiscoveryCache,
            DiscoveryDocument,
            EntityTag,
            GatewayAccess,
            IdempotencyKey,
            JobActionRequest,
            JobPage,
            JobState,
            JobSummary,
            LogicalRepository,
            OidcClientDiscovery,
            PageCursor,
            ProviderCredentials,
            PromoteReplicaRequest,
            PromoteReplicaResponse,
            PushAbortResponse,
            PushAdmissionPlan,
            PushFinalizeRequest,
            PushFinalizeResponse,
            PushPrepareRequest,
            PushPrepareResponse,
            PushReplicationRequest,
            RepositoryState,
            SecretString,
            ServiceCapabilities,
            StagingScope,
            TransferGrant,
            TransferGrantRequest,
            TransferMode,
            TransferOperation,
            TransferPermission,
            TransferScope,
            TransferTransport,
            UsageCategory,
            UsageReport,
            UsageTotal
        )
    ),
    tags((name = "managed", description = "Crab managed-service API")),
    info(title = "Crab Managed Service API", version = "1.0.0")
)]
pub struct ManagedApiDoc;

/// Generates the canonical pretty-printed managed OpenAPI document.
pub fn managed_openapi_json() -> std::result::Result<String, serde_json::Error> {
    ManagedApiDoc::openapi().to_pretty_json()
}

/// Reports backward-incompatible changes between two managed OpenAPI documents.
#[must_use]
pub fn managed_openapi_breaking_changes(
    baseline: &serde_json::Value,
    candidate: &serde_json::Value,
) -> Vec<String> {
    let mut changes = Vec::new();
    compare_paths(baseline, candidate, &mut changes);

    let request_schemas = request_schema_closure(baseline);
    let baseline_schemas = object_at(baseline, "/components/schemas");
    let candidate_schemas = object_at(candidate, "/components/schemas");
    for (name, baseline_schema) in baseline_schemas {
        let Some(candidate_schema) = candidate_schemas.get(name) else {
            changes.push(format!("removed schema {name}"));
            continue;
        };
        compare_schema(
            baseline_schema,
            candidate_schema,
            &format!("schema {name}"),
            request_schemas.contains(name),
            &mut changes,
        );
    }
    changes
}

fn compare_paths(
    baseline: &serde_json::Value,
    candidate: &serde_json::Value,
    changes: &mut Vec<String>,
) {
    let candidate_paths = object_at(candidate, "/paths");
    for (path, baseline_path) in object_at(baseline, "/paths") {
        let Some(candidate_path) = candidate_paths.get(path) else {
            changes.push(format!("removed path {path}"));
            continue;
        };
        for method in ["get", "put", "post", "delete", "patch", "head", "options"] {
            let Some(baseline_operation) = baseline_path.get(method) else {
                continue;
            };
            let Some(candidate_operation) = candidate_path.get(method) else {
                changes.push(format!("removed operation {method} {path}"));
                continue;
            };
            let operation = format!("{method} {path}");
            compare_operation(baseline_operation, candidate_operation, &operation, changes);
        }
    }
}

fn compare_operation(
    baseline: &serde_json::Value,
    candidate: &serde_json::Value,
    operation: &str,
    changes: &mut Vec<String>,
) {
    if baseline.get("security") != candidate.get("security") {
        changes.push(format!("changed security requirements for {operation}"));
    }
    let candidate_responses = object_field(candidate, "responses");
    for status in object_field(baseline, "responses").keys() {
        if !candidate_responses.contains_key(status) {
            changes.push(format!("removed response {status} from {operation}"));
        }
    }

    let candidate_parameters = parameters_by_identity(candidate);
    for (identity, parameter) in parameters_by_identity(baseline) {
        let Some(candidate_parameter) = candidate_parameters.get(&identity) else {
            changes.push(format!(
                "removed {} parameter {} from {operation}",
                identity.0, identity.1
            ));
            continue;
        };
        if parameter.get("required") == Some(&serde_json::Value::Bool(true))
            && candidate_parameter.get("required") != Some(&serde_json::Value::Bool(true))
        {
            changes.push(format!(
                "made required {} parameter {} optional for {operation}",
                identity.0, identity.1
            ));
        }
        if let (Some(baseline_schema), Some(candidate_schema)) =
            (parameter.get("schema"), candidate_parameter.get("schema"))
        {
            compare_schema(
                baseline_schema,
                candidate_schema,
                &format!("{} parameter {} on {operation}", identity.0, identity.1),
                true,
                changes,
            );
        }
    }

    let Some(baseline_body) = baseline.get("requestBody") else {
        return;
    };
    let Some(candidate_body) = candidate.get("requestBody") else {
        changes.push(format!("removed request body from {operation}"));
        return;
    };
    if baseline_body.get("required") == Some(&serde_json::Value::Bool(true))
        && candidate_body.get("required") != Some(&serde_json::Value::Bool(true))
    {
        changes.push(format!("made request body optional for {operation}"));
    }
    let baseline_schema = baseline_body.pointer("/content/application~1json/schema");
    let candidate_schema = candidate_body.pointer("/content/application~1json/schema");
    match (baseline_schema, candidate_schema) {
        (Some(baseline_schema), Some(candidate_schema)) => compare_schema(
            baseline_schema,
            candidate_schema,
            &format!("request body for {operation}"),
            true,
            changes,
        ),
        (Some(_), None) => changes.push(format!("removed JSON request schema from {operation}")),
        _ => {}
    }
}

fn compare_schema(
    baseline: &serde_json::Value,
    candidate: &serde_json::Value,
    location: &str,
    request_schema: bool,
    changes: &mut Vec<String>,
) {
    for keyword in ["$ref", "type", "format", "pattern", "minimum", "maximum"] {
        if let Some(value) = baseline.get(keyword)
            && candidate.get(keyword) != Some(value)
        {
            changes.push(format!("changed {keyword} for {location}"));
        }
    }
    if let Some(baseline_enum) = baseline.get("enum").and_then(serde_json::Value::as_array) {
        let candidate_enum = candidate
            .get("enum")
            .and_then(serde_json::Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        for value in baseline_enum {
            if !candidate_enum.contains(value) {
                changes.push(format!("removed enum value {value} from {location}"));
            }
        }
    }

    let baseline_required = string_set(baseline.get("required"));
    let candidate_required = string_set(candidate.get("required"));
    for field in baseline_required.difference(&candidate_required) {
        changes.push(format!("made required field {location}.{field} optional"));
    }
    if request_schema {
        for field in candidate_required.difference(&baseline_required) {
            changes.push(format!("added required request field {location}.{field}"));
        }
    }

    let candidate_properties = object_field(candidate, "properties");
    for (field, baseline_property) in object_field(baseline, "properties") {
        let Some(candidate_property) = candidate_properties.get(field) else {
            changes.push(format!("removed field {location}.{field}"));
            continue;
        };
        compare_schema(
            baseline_property,
            candidate_property,
            &format!("{location}.{field}"),
            request_schema,
            changes,
        );
    }
    if let Some(baseline_items) = baseline.get("items") {
        match candidate.get("items") {
            Some(candidate_items) => compare_schema(
                baseline_items,
                candidate_items,
                &format!("{location} items"),
                request_schema,
                changes,
            ),
            None => changes.push(format!("removed item schema from {location}")),
        }
    }
    if let Some(baseline_variants) = baseline.get("oneOf").and_then(serde_json::Value::as_array) {
        let candidate_variants = candidate
            .get("oneOf")
            .and_then(serde_json::Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        for (index, baseline_variant) in baseline_variants.iter().enumerate() {
            let compatible = candidate_variants.iter().any(|candidate_variant| {
                let mut variant_changes = Vec::new();
                compare_schema(
                    baseline_variant,
                    candidate_variant,
                    location,
                    request_schema,
                    &mut variant_changes,
                );
                variant_changes.is_empty()
            });
            if !compatible {
                changes.push(format!(
                    "removed or changed oneOf variant {index} from {location}"
                ));
            }
        }
    }
    if baseline.get("additionalProperties") == Some(&serde_json::Value::Bool(false))
        && candidate.get("additionalProperties") != Some(&serde_json::Value::Bool(false))
    {
        changes.push(format!("widened additional properties for {location}"));
    }
}

fn request_schema_closure(document: &serde_json::Value) -> BTreeSet<String> {
    let schemas = object_at(document, "/components/schemas");
    let mut found = BTreeSet::new();
    for path in object_at(document, "/paths").values() {
        for method in ["get", "put", "post", "delete", "patch"] {
            if let Some(reference) = path
                .get(method)
                .and_then(|operation| {
                    operation.pointer("/requestBody/content/application~1json/schema/$ref")
                })
                .and_then(serde_json::Value::as_str)
                .and_then(schema_name)
            {
                collect_schema_references(reference, schemas, &mut found);
            }
        }
    }
    found
}

fn collect_schema_references(
    name: &str,
    schemas: &serde_json::Map<String, serde_json::Value>,
    found: &mut BTreeSet<String>,
) {
    if !found.insert(name.to_owned()) {
        return;
    }
    let Some(schema) = schemas.get(name) else {
        return;
    };
    visit_references(schema, &mut |reference| {
        collect_schema_references(reference, schemas, found);
    });
}

fn visit_references(value: &serde_json::Value, visitor: &mut impl FnMut(&str)) {
    match value {
        serde_json::Value::Object(object) => {
            if let Some(name) = object
                .get("$ref")
                .and_then(serde_json::Value::as_str)
                .and_then(schema_name)
            {
                visitor(name);
            }
            for value in object.values() {
                visit_references(value, visitor);
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                visit_references(value, visitor);
            }
        }
        _ => {}
    }
}

fn parameters_by_identity(
    operation: &serde_json::Value,
) -> BTreeMap<(String, String), &serde_json::Value> {
    operation
        .get("parameters")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|parameter| {
            let location = parameter.get("in")?.as_str()?.to_owned();
            let name = parameter.get("name")?.as_str()?.to_owned();
            Some(((location, name), parameter))
        })
        .collect()
}

fn schema_name(reference: &str) -> Option<&str> {
    reference.strip_prefix("#/components/schemas/")
}

fn string_set(value: Option<&serde_json::Value>) -> BTreeSet<String> {
    value
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .map(str::to_owned)
        .collect()
}

fn object_at<'a>(
    value: &'a serde_json::Value,
    pointer: &str,
) -> &'a serde_json::Map<String, serde_json::Value> {
    match value
        .pointer(pointer)
        .and_then(serde_json::Value::as_object)
    {
        Some(object) => object,
        None => empty_object(),
    }
}

fn object_field<'a>(
    value: &'a serde_json::Value,
    field: &str,
) -> &'a serde_json::Map<String, serde_json::Value> {
    match value.get(field).and_then(serde_json::Value::as_object) {
        Some(object) => object,
        None => empty_object(),
    }
}

fn empty_object() -> &'static serde_json::Map<String, serde_json::Value> {
    static EMPTY: std::sync::OnceLock<serde_json::Map<String, serde_json::Value>> =
        std::sync::OnceLock::new();
    EMPTY.get_or_init(serde_json::Map::new)
}
