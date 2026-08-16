use std::collections::BTreeMap;
use std::fmt::Debug;

use crab_auth::{
    ApiError, ApiErrorEnvelope, AuthFlow, CapabilityName, DirectObjectCredentials, DiscoveryCache,
    DiscoveryDocument, EntityTag, GatewayAccess, GrantValidationContext, IdempotencyKey,
    LogicalRepository, OidcClientDiscovery, PageCursor, ProviderCredentials, RepositoryState,
    SecretString, ServiceCapabilities, StagingScope, TransferGrant, TransferMode,
    TransferOperation, TransferPermission, TransferScope, TransferTransport,
    managed_openapi_breaking_changes, managed_openapi_json,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use time::OffsetDateTime;
use uuid::Uuid;

const REPOSITORY_ID: &str = "01900000-0000-7000-8000-000000000001";
const ORGANIZATION_ID: &str = "01900000-0000-7000-8000-000000000002";
const GRANT_ID: &str = "01900000-0000-7000-8000-000000000003";
const PUSH_ID: &str = "01900000-0000-7000-8000-000000000004";
const REPOSITORY_PREFIX: &str =
    "environments/env-1/repositories/01900000-0000-7000-8000-000000000001";

#[test]
fn managed_v1_fixtures_match_the_strict_wire_contract() {
    assert_fixture(
        include_str!("fixtures/managed-v1/discovery.json"),
        &discovery(),
    );
    assert_fixture(
        include_str!("fixtures/managed-v1/capabilities.json"),
        &capabilities(),
    );
    assert_fixture(
        include_str!("fixtures/managed-v1/logical-repository.json"),
        &logical_repository(),
    );
    assert_fixture(
        include_str!("fixtures/managed-v1/api-error.json"),
        &api_error(),
    );
    assert_fixture(
        include_str!("fixtures/managed-v1/transfer-grant-direct.json"),
        &direct_grant(),
    );
    assert_fixture(
        include_str!("fixtures/managed-v1/transfer-grant-gateway.json"),
        &gateway_grant(),
    );
}

#[test]
fn discovery_validates_https_authority_and_version_overlap() {
    discovery().validate("crab.build", &[1]).unwrap();

    let mut wrong_version = discovery();
    wrong_version.schema_version = 2;
    assert!(wrong_version.validate("crab.build", &[1]).is_err());

    let mut unsafe_endpoint = discovery();
    unsafe_endpoint.api_base = "http://api.crab.build/v1".to_owned();
    assert!(unsafe_endpoint.validate("crab.build", &[1]).is_err());

    let mut wrong_authority = discovery();
    wrong_authority.authority = "other.example.com".to_owned();
    assert!(wrong_authority.validate("crab.build", &[1]).is_err());
}

#[test]
fn discovery_ignores_additive_response_fields_and_retains_unknown_capabilities() {
    let body = include_str!("fixtures/managed-v1/discovery.json")
        .replace("\n}", ",\n  \"future_response_field\": true\n}");
    let parsed: DiscoveryDocument = serde_json::from_str(&body).unwrap();
    let future = CapabilityName::new("future-safe-v1").unwrap();

    assert_eq!(parsed.schema_version, 1);
    assert_eq!(future.as_str(), "future-safe-v1");
}

#[test]
fn direct_grant_validates_all_caller_held_facts() {
    direct_grant().validate(read_context()).unwrap();
}

#[test]
fn gateway_grant_validates_exact_push_scope() {
    gateway_grant().validate(push_context()).unwrap();
}

#[test]
fn grant_rejects_unknown_version_repository_mismatch_and_invalid_expiry() {
    let mut unknown_version = direct_grant();
    unknown_version.schema_version = 2;
    assert!(unknown_version.validate(read_context()).is_err());

    let mut wrong_repository = read_context();
    wrong_repository.repository_id = uuid(ORGANIZATION_ID);
    assert!(direct_grant().validate(wrong_repository).is_err());

    let mut expired = direct_grant();
    expired.expires_at = timestamp(1_893_455_000);
    assert!(expired.validate(read_context()).is_err());

    let mut too_long = direct_grant();
    too_long.expires_at = timestamp(1_893_457_000);
    assert!(too_long.validate(read_context()).is_err());
}

#[test]
fn grant_rejects_widened_permission_and_physical_scope() {
    let mut widened_permission = direct_grant();
    let allowed = [TransferPermission::ReadObject];
    let context = GrantValidationContext {
        allowed_permissions: &allowed,
        ..read_context()
    };
    assert!(widened_permission.validate(context).is_err());

    widened_permission.permissions = vec![TransferPermission::ReadObject];
    widened_permission.storage_scope.repository_prefix =
        "environments/env-1/repositories".to_owned();
    assert!(widened_permission.validate(read_context()).is_err());
}

#[test]
fn grant_rejects_unsafe_gateway_and_direct_endpoints() {
    let mut gateway = gateway_grant();
    let TransferTransport::Gateway { gateway: access } = &mut gateway.transport else {
        panic!("fixture must use gateway transport");
    };
    access.service_url = "http://gateway.crab.build/v1/objects".to_owned();
    assert!(gateway.validate(push_context()).is_err());

    let mut direct = direct_grant();
    let TransferTransport::Direct {
        direct: credentials,
    } = &mut direct.transport
    else {
        panic!("fixture must use direct transport");
    };
    credentials.endpoint = Some("https://user@example.com/storage".to_owned());
    assert!(direct.validate(read_context()).is_err());
}

#[test]
fn transfer_grant_deserialization_rejects_unknown_security_fields_and_variants() {
    let unknown_field = include_str!("fixtures/managed-v1/transfer-grant-gateway.json")
        .replace("\n}", ",\n  \"canonical_write\": true\n}");
    assert!(serde_json::from_str::<TransferGrant>(&unknown_field).is_err());

    let unknown_operation = include_str!("fixtures/managed-v1/transfer-grant-direct.json").replace(
        "\"operation\": \"clone\"",
        "\"operation\": \"administrate\"",
    );
    assert!(serde_json::from_str::<TransferGrant>(&unknown_operation).is_err());
}

#[test]
fn secret_debug_output_is_redacted() {
    let grant = direct_grant();
    let debug = format!("{grant:?}");

    assert!(!debug.contains("fixture-secret-access-key"));
    assert!(!debug.contains("fixture-session-token"));
    assert!(debug.contains("<redacted>"));
}

#[test]
fn logical_repository_and_capabilities_validate_versioned_bounds() {
    logical_repository().validate().unwrap();
    capabilities().validate().unwrap();

    let mut repository = logical_repository();
    repository.schema_version = 9;
    assert!(repository.validate().is_err());

    let mut service = capabilities();
    service.transfer_grant_versions = vec![2];
    assert!(service.validate().is_err());
}

#[test]
fn opaque_header_contracts_reject_unbounded_or_weak_values() {
    assert_eq!(
        PageCursor::new("page.signature").unwrap().as_str(),
        "page.signature"
    );
    assert!(PageCursor::new("page with spaces").is_err());

    assert_eq!(
        EntityTag::new("\"revision-7\"").unwrap().as_str(),
        "\"revision-7\""
    );
    assert!(EntityTag::new("W/\"revision-7\"").is_err());

    assert_eq!(
        IdempotencyKey::new("repo-create-01J0").unwrap().as_str(),
        "repo-create-01J0"
    );
    assert!(IdempotencyKey::new("line\nbreak").is_err());
}

#[test]
fn openapi_compatibility_allows_additive_responses_and_rejects_breakage() {
    let baseline: serde_json::Value =
        serde_json::from_str(include_str!("../openapi/managed-v1.json")).unwrap();
    assert!(managed_openapi_breaking_changes(&baseline, &baseline).is_empty());

    let mut additive = baseline.clone();
    additive["components"]["schemas"]["LogicalRepository"]["properties"]
        .as_object_mut()
        .unwrap()
        .insert(
            "future_field".to_owned(),
            serde_json::json!({ "type": "string" }),
        );
    additive["components"]["schemas"]["LogicalRepository"]["required"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!("future_field"));
    assert!(managed_openapi_breaking_changes(&baseline, &additive).is_empty());

    let mut removed_field = baseline.clone();
    removed_field["components"]["schemas"]["LogicalRepository"]["properties"]
        .as_object_mut()
        .unwrap()
        .remove("canonical_url");
    assert!(
        managed_openapi_breaking_changes(&baseline, &removed_field)
            .iter()
            .any(|change| change.contains("canonical_url"))
    );

    let mut required_request_field = baseline.clone();
    required_request_field["components"]["schemas"]["TransferGrantRequest"]["properties"]
        .as_object_mut()
        .unwrap()
        .insert(
            "new_input".to_owned(),
            serde_json::json!({ "type": "string" }),
        );
    required_request_field["components"]["schemas"]["TransferGrantRequest"]["required"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!("new_input"));
    assert!(
        managed_openapi_breaking_changes(&baseline, &required_request_field)
            .iter()
            .any(|change| change.contains("new_input"))
    );
}

#[test]
fn generated_openapi_matches_the_reviewed_contract() {
    let generated = format!("{}\n", managed_openapi_json().unwrap());

    assert_eq!(
        generated,
        include_str!("../openapi/managed-v1.json"),
        "regenerate and review managed-v1.json when the public DTO contract changes"
    );
}

#[test]
fn managed_openapi_declares_route_outcomes_and_error_envelopes() {
    let document: serde_json::Value =
        serde_json::from_str(&managed_openapi_json().unwrap()).unwrap();
    let routes = [
        ("/.well-known/crab", "get", &[200, 503][..]),
        ("/v1/capabilities", "get", &[200, 401]),
        (
            "/v1/repositories/{organization}/{repository}",
            "get",
            &[200, 401, 404],
        ),
        (
            "/v1/repositories/{organization}/{repository}/transfer-grants",
            "post",
            &[201, 400, 401, 403, 404, 429, 503],
        ),
        (
            "/v1/repositories/{organization}/{repository}/pushes",
            "post",
            &[201, 400, 401, 404, 409, 429, 503],
        ),
        (
            "/v1/repositories/{organization}/{repository}/pushes/{push_id}:finalize",
            "post",
            &[200, 400, 401, 404, 409, 422, 429, 503],
        ),
        (
            "/v1/repositories/{organization}/{repository}/pushes/{push_id}:abort",
            "post",
            &[200, 400, 401, 403, 404, 409, 503],
        ),
        (
            "/v1/organizations/{organization}/audit-events",
            "get",
            &[200, 400, 401, 403],
        ),
        (
            "/v1/organizations/{organization}/audit-exports",
            "post",
            &[202, 400, 401, 403, 409, 503],
        ),
        (
            "/v1/organizations/{organization}/audit-exports/{job}",
            "get",
            &[200, 401, 403, 404, 409, 503],
        ),
        (
            "/v1/organizations/{organization}/usage",
            "get",
            &[200, 400, 401, 403],
        ),
        (
            "/v1/organizations/{organization}/jobs",
            "get",
            &[200, 400, 401, 403],
        ),
        (
            "/v1/organizations/{organization}/jobs/{job}",
            "get",
            &[200, 401, 403, 404],
        ),
    ];
    for (path, method, expected_statuses) in routes {
        assert_route_statuses(&document, path, method, expected_statuses);
    }
    for action in ["retry", "cancel", "quarantine"] {
        assert_route_statuses(
            &document,
            &format!("/v1/organizations/{{organization}}/jobs/{{job}}:{action}"),
            "post",
            &[200, 400, 401, 403, 404, 409, 412, 428, 503],
        );
    }

    for (path, method, _) in routes
        .into_iter()
        .filter(|(path, _, _)| *path != "/.well-known/crab")
    {
        assert_eq!(
            document["paths"][path][method]["security"][0]["bearer_auth"],
            serde_json::json!([]),
            "{method} {path} must require bearer authentication"
        );
    }
    assert!(
        document["paths"]["/v1/organizations/{organization}/audit-events"]["get"]["parameters"]
            .as_array()
            .is_some_and(|parameters| parameters
                .iter()
                .any(|parameter| { parameter["name"] == "cursor" && parameter["in"] == "query" }))
    );
}

fn assert_route_statuses(
    document: &serde_json::Value,
    path: &str,
    method: &str,
    expected_statuses: &[u16],
) {
    let responses = document["paths"][path][method]["responses"]
        .as_object()
        .unwrap_or_else(|| panic!("missing responses for {method} {path}"));
    let actual = responses
        .keys()
        .map(|status| status.parse::<u16>().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        actual, expected_statuses,
        "status contract for {method} {path}"
    );
    for status in expected_statuses
        .iter()
        .copied()
        .filter(|status| *status >= 400)
    {
        assert_eq!(
            responses[&status.to_string()]["content"]["application/json"]["schema"]["$ref"],
            "#/components/schemas/ApiErrorEnvelope",
            "error envelope for {method} {path} status {status}"
        );
    }
}

fn assert_fixture<T>(expected: &str, value: &T)
where
    T: Debug + PartialEq + Serialize + DeserializeOwned,
{
    let encoded = format!("{}\n", serde_json::to_string_pretty(value).unwrap());
    assert_eq!(encoded, expected);
    let decoded: T = serde_json::from_str(expected).unwrap();
    assert_eq!(&decoded, value);
}

fn discovery() -> DiscoveryDocument {
    DiscoveryDocument {
        schema_version: 1,
        authority: "crab.build".to_owned(),
        api_base: "https://api.crab.build/v1".to_owned(),
        api_versions: vec![1],
        oidc: OidcClientDiscovery {
            issuer: "https://identity.crab.build".to_owned(),
            client_id: "crab-cli".to_owned(),
            scopes: vec![
                "openid".to_owned(),
                "profile".to_owned(),
                "email".to_owned(),
                "offline_access".to_owned(),
            ],
        },
        auth_flows: vec![
            AuthFlow::new("authorization_code_pkce").unwrap(),
            AuthFlow::new("device_authorization").unwrap(),
        ],
        service_version: "1.0.0".to_owned(),
        minimum_cli_version: "1.1.0".to_owned(),
        capabilities: vec![
            CapabilityName::new("direct-s3-v1").unwrap(),
            CapabilityName::new("gateway-v1").unwrap(),
            CapabilityName::new("protected-push-v1").unwrap(),
        ],
        cache: DiscoveryCache {
            max_age_seconds: 3600,
        },
    }
}

fn capabilities() -> ServiceCapabilities {
    ServiceCapabilities {
        schema_version: 1,
        api_versions: vec![1],
        transfer_grant_versions: vec![1],
        transfer_modes: vec![TransferMode::DirectS3, TransferMode::Gateway],
        capabilities: discovery().capabilities,
        max_page_size: 100,
        max_grant_lifetime_seconds: 900,
    }
}

fn logical_repository() -> LogicalRepository {
    LogicalRepository {
        schema_version: 1,
        repository_id: uuid(REPOSITORY_ID),
        organization_id: uuid(ORGANIZATION_ID),
        canonical_url: "crab://crab.build/acme/models".to_owned(),
        state: RepositoryState::Active,
        revision: 7,
        transfer_modes: vec![TransferMode::DirectS3],
        protected_push: true,
    }
}

fn api_error() -> ApiErrorEnvelope {
    ApiErrorEnvelope {
        error: ApiError {
            code: "repository_not_found".to_owned(),
            message: "repository was not found or is not accessible".to_owned(),
            request_id: "req_01J00000000000000000000000".to_owned(),
            retryable: false,
            details: BTreeMap::new(),
        },
    }
}

fn direct_grant() -> TransferGrant {
    TransferGrant {
        schema_version: 1,
        grant_id: uuid(GRANT_ID),
        repository_id: uuid(REPOSITORY_ID),
        operation: TransferOperation::Clone,
        expires_at: timestamp(1_893_456_000),
        permissions: vec![
            TransferPermission::ReadObject,
            TransferPermission::ReadMetadata,
            TransferPermission::ListPrefix,
        ],
        storage_scope: TransferScope {
            repository_prefix: REPOSITORY_PREFIX.to_owned(),
            staging: None,
        },
        transport: TransferTransport::Direct {
            direct: DirectObjectCredentials {
                endpoint: Some("https://s3.us-west-2.amazonaws.com".to_owned()),
                region: Some("us-west-2".to_owned()),
                container: "crab-managed-prod".to_owned(),
                object_prefix: REPOSITORY_PREFIX.to_owned(),
                credentials: ProviderCredentials::Aws {
                    access_key_id: secret("fixture-access-key-id"),
                    secret_access_key: secret("fixture-secret-access-key"),
                    session_token: secret("fixture-session-token"),
                },
            },
        },
    }
}

fn gateway_grant() -> TransferGrant {
    let push_id = uuid(PUSH_ID);
    TransferGrant {
        schema_version: 1,
        grant_id: uuid(GRANT_ID),
        repository_id: uuid(REPOSITORY_ID),
        operation: TransferOperation::PushUpload,
        expires_at: timestamp(1_893_456_000),
        permissions: vec![TransferPermission::CreateImmutableObject],
        storage_scope: TransferScope {
            repository_prefix: REPOSITORY_PREFIX.to_owned(),
            staging: Some(StagingScope {
                push_id,
                prefix: format!("{REPOSITORY_PREFIX}/staging/{}", push_id.simple()),
            }),
        },
        transport: TransferTransport::Gateway {
            gateway: GatewayAccess {
                service_url: "https://gateway.crab.build/v1/objects".to_owned(),
                token: secret("fixture-gateway-token"),
            },
        },
    }
}

fn read_context() -> GrantValidationContext<'static> {
    static ALLOWED: [TransferPermission; 3] = [
        TransferPermission::ReadObject,
        TransferPermission::ReadMetadata,
        TransferPermission::ListPrefix,
    ];
    GrantValidationContext {
        repository_id: uuid(REPOSITORY_ID),
        operation: TransferOperation::Clone,
        repository_prefix: REPOSITORY_PREFIX,
        push_id: None,
        allowed_permissions: &ALLOWED,
        now: timestamp(1_893_455_100),
        not_after: timestamp(1_893_456_600),
    }
}

fn push_context() -> GrantValidationContext<'static> {
    static ALLOWED: [TransferPermission; 1] = [TransferPermission::CreateImmutableObject];
    GrantValidationContext {
        repository_id: uuid(REPOSITORY_ID),
        operation: TransferOperation::PushUpload,
        repository_prefix: REPOSITORY_PREFIX,
        push_id: Some(uuid(PUSH_ID)),
        allowed_permissions: &ALLOWED,
        now: timestamp(1_893_455_100),
        not_after: timestamp(1_893_456_600),
    }
}

fn secret(value: &str) -> SecretString {
    SecretString::new(value).unwrap()
}

fn uuid(value: &str) -> Uuid {
    Uuid::parse_str(value).unwrap()
}

fn timestamp(value: i64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(value).unwrap()
}
