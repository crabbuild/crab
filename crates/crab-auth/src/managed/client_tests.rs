use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;

use serde::Serialize;

use super::*;
use crate::PushRefUpdate;
use crate::managed::{
    CapabilityName, GatewayAccess, OrganizationState, PushAdmissionPlan, SecretString,
    TransferScope,
};

struct MockTransport {
    responses: Mutex<VecDeque<ManagedApiResult<ApiHttpResponse>>>,
    requests: Mutex<Vec<ApiRequest>>,
}

#[async_trait]
impl ApiTransport for MockTransport {
    async fn send(&self, request: ApiRequest) -> ManagedApiResult<ApiHttpResponse> {
        self.requests.lock().unwrap().push(request);
        self.responses.lock().unwrap().pop_front().unwrap()
    }
}

fn client(responses: Vec<ApiHttpResponse>) -> (ManagedApiClient, Arc<MockTransport>) {
    let transport = Arc::new(MockTransport {
        responses: Mutex::new(responses.into_iter().map(Ok).collect()),
        requests: Mutex::new(Vec::new()),
    });
    let client = ManagedApiClient::with_transport(
        "crab.build",
        BearerToken::new("secret-token").unwrap(),
        transport.clone(),
    );
    (client, transport)
}

fn repository() -> LogicalRepository {
    LogicalRepository {
        schema_version: 1,
        repository_id: Uuid::now_v7(),
        organization_id: Uuid::now_v7(),
        canonical_url: "crab://crab.build/acme/models".to_owned(),
        state: super::super::RepositoryState::Active,
        revision: 1,
        transfer_modes: vec![TransferMode::Gateway],
        protected_push: true,
    }
}

fn response<T: Serialize>(status: StatusCode, value: &T) -> ApiHttpResponse {
    ApiHttpResponse {
        status,
        etag: None,
        retry_after: None,
        body: serde_json::to_vec(value).unwrap(),
    }
}

fn service_error(status: StatusCode, retryable: bool) -> ApiHttpResponse {
    response(
        status,
        &ApiErrorEnvelope {
            error: ApiError {
                code: "service_unavailable".to_owned(),
                message: "service is temporarily unavailable".to_owned(),
                request_id: "req_test".to_owned(),
                retryable,
                details: BTreeMap::new(),
            },
        },
    )
}

#[tokio::test]
async fn repository_resolution_retries_stable_error_and_requires_etag() {
    let mut success = response(StatusCode::OK, &repository());
    success.etag = Some("\"revision-1\"".to_owned());
    let (client, transport) = client(vec![
        service_error(StatusCode::SERVICE_UNAVAILABLE, true),
        success,
    ]);

    let resolved = client.resolve_repository("acme", "models").await.unwrap();

    assert_eq!(resolved.etag.as_str(), "\"revision-1\"");
    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0].endpoint.as_str(),
        "https://api.crab.build/v1/repositories/acme/models"
    );
    assert_eq!(format!("{:?}", requests[0].bearer), "<redacted>");
}

#[tokio::test]
async fn create_repository_retries_only_with_idempotency_key() {
    let mut success = response(StatusCode::CREATED, &repository());
    success.etag = Some("\"revision-1\"".to_owned());
    let (client, transport) = client(vec![
        service_error(StatusCode::TOO_MANY_REQUESTS, true),
        success,
    ]);
    let key = IdempotencyKey::new("create-models-1").unwrap();

    client
        .create_repository("acme", "models", &key)
        .await
        .unwrap();

    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(
        requests
            .iter()
            .all(|request| request.idempotency_key.as_deref() == Some("create-models-1"))
    );
    let body: CreateRepositoryRequest =
        serde_json::from_slice(requests[0].body.as_deref().unwrap()).unwrap();
    assert_eq!(body.slug, "models");
}

#[tokio::test]
async fn repository_page_sends_and_retains_opaque_cursor() {
    let next = PageCursor::new("next.cursor_1").unwrap();
    let page = RepositoryPage {
        schema_version: 1,
        repositories: vec![repository()],
        next_cursor: Some(next.clone()),
    };
    let (client, transport) = client(vec![response(StatusCode::OK, &page)]);

    let listed = client
        .list_repositories("acme", Some(&PageCursor::new("current_1").unwrap()), 25)
        .await
        .unwrap();

    assert_eq!(listed.next_cursor, Some(next));
    let requests = transport.requests.lock().unwrap();
    assert_eq!(
        requests[0].endpoint.query(),
        Some("limit=25&cursor=current_1")
    );
}

#[tokio::test]
async fn transfer_grant_issue_is_not_retried_and_refreshes_with_new_request() {
    let repository_id = Uuid::now_v7();
    let grant = |grant_id| TransferGrant {
        schema_version: 1,
        grant_id,
        repository_id,
        operation: TransferOperation::Fetch,
        expires_at: OffsetDateTime::now_utc() + time::Duration::minutes(5),
        permissions: vec![TransferPermission::ReadObject],
        storage_scope: TransferScope {
            repository_prefix: format!("env/repositories/{repository_id}"),
            staging: None,
        },
        transport: TransferTransport::Gateway {
            gateway: GatewayAccess {
                service_url: "https://objects.crab.build/v1".to_owned(),
                token: SecretString::new("grant-token").unwrap(),
            },
        },
    };
    let capabilities = ServiceCapabilities {
        schema_version: 1,
        api_versions: vec![1],
        transfer_grant_versions: vec![1],
        transfer_modes: vec![TransferMode::Gateway],
        capabilities: vec![CapabilityName::new("gateway-v1").unwrap()],
        max_page_size: 100,
        max_grant_lifetime_seconds: 900,
    };
    let first = Uuid::now_v7();
    let second = Uuid::now_v7();
    let (client, transport) = client(vec![
        response(StatusCode::CREATED, &grant(first)),
        response(StatusCode::CREATED, &grant(second)),
    ]);

    let issued = client
        .issue_transfer_grant(
            "acme",
            "models",
            repository_id,
            TransferOperation::Fetch,
            &capabilities,
        )
        .await
        .unwrap();
    let refreshed = client
        .refresh_transfer_grant(
            "acme",
            "models",
            repository_id,
            TransferOperation::Fetch,
            &capabilities,
        )
        .await
        .unwrap();

    assert_eq!(issued.grant_id, first);
    assert_eq!(refreshed.grant_id, second);
    assert_eq!(transport.requests.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn transfer_grant_failure_requires_explicit_refresh_attempt() {
    let repository_id = Uuid::now_v7();
    let capabilities = ServiceCapabilities {
        schema_version: 1,
        api_versions: vec![1],
        transfer_grant_versions: vec![1],
        transfer_modes: vec![TransferMode::Gateway],
        capabilities: vec![CapabilityName::new("gateway-v1").unwrap()],
        max_page_size: 100,
        max_grant_lifetime_seconds: 900,
    };
    let (client, transport) = client(vec![service_error(StatusCode::SERVICE_UNAVAILABLE, true)]);

    let error = client
        .issue_transfer_grant(
            "acme",
            "models",
            repository_id,
            TransferOperation::Fetch,
            &capabilities,
        )
        .await
        .unwrap_err();

    assert!(error.service_error().unwrap().retryable);
    assert_eq!(transport.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn push_prepare_retries_same_request_and_resumes_same_session_with_fresh_grant() {
    let repository_id = Uuid::now_v7();
    let push_id = Uuid::now_v7();
    let session_expiry = OffsetDateTime::now_utc() + time::Duration::minutes(10);
    let staging_grant = |grant_id| TransferGrant {
        schema_version: 1,
        grant_id,
        repository_id,
        operation: TransferOperation::PushUpload,
        expires_at: OffsetDateTime::now_utc() + time::Duration::minutes(5),
        permissions: vec![TransferPermission::CreateImmutableObject],
        storage_scope: TransferScope {
            repository_prefix: format!("env/repositories/{repository_id}"),
            staging: Some(super::super::StagingScope {
                push_id,
                prefix: format!(
                    "env/repositories/{repository_id}/staging/{}",
                    push_id.simple()
                ),
            }),
        },
        transport: TransferTransport::Gateway {
            gateway: GatewayAccess {
                service_url: "https://objects.crab.build/v1".to_owned(),
                token: SecretString::new("grant-token").unwrap(),
            },
        },
    };
    let prepared = |grant_id| PushPrepareResponse {
        schema_version: 1,
        push_id,
        repository_id,
        expires_at: session_expiry,
        base_manifest_generation: 7,
        base_manifest_etag: "manifest-etag-7".to_owned(),
        staging_grant: staging_grant(grant_id),
    };
    let capabilities = ServiceCapabilities {
        schema_version: 1,
        api_versions: vec![1],
        transfer_grant_versions: vec![1],
        transfer_modes: vec![TransferMode::Gateway],
        capabilities: vec![CapabilityName::new("gateway-v1").unwrap()],
        max_page_size: 100,
        max_grant_lifetime_seconds: 900,
    };
    let request = PushPrepareRequest {
        schema_version: 1,
        repository_id,
        ref_updates: vec![PushRefUpdate {
            ref_name: "refs/heads/main".to_owned(),
            old_oid: None,
            new_oid: "a".repeat(40),
        }],
        plan: PushAdmissionPlan {
            estimated_bytes: 512,
            estimated_objects: 3,
        },
        client_version: "crab-test".to_owned(),
        replication: None,
    };
    let first_grant_id = Uuid::now_v7();
    let second_grant_id = Uuid::now_v7();
    let (client, transport) = client(vec![
        service_error(StatusCode::SERVICE_UNAVAILABLE, true),
        response(StatusCode::CREATED, &prepared(first_grant_id)),
        response(StatusCode::CREATED, &prepared(second_grant_id)),
    ]);
    let key = IdempotencyKey::new("push-models-1").unwrap();

    let first = client
        .prepare_push("acme", "models", &request, &key, &capabilities)
        .await
        .unwrap();
    let resumed = client
        .prepare_push("acme", "models", &request, &key, &capabilities)
        .await
        .unwrap();

    assert_eq!(first.push_id, resumed.push_id);
    assert_ne!(first.staging_grant.grant_id, resumed.staging_grant.grant_id);
    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests.len(), 3);
    assert!(requests.iter().all(|request| {
        request.endpoint.as_str() == "https://api.crab.build/v1/repositories/acme/models/pushes"
            && request.idempotency_key.as_deref() == Some("push-models-1")
    }));
}

#[tokio::test]
async fn push_finalize_retries_only_the_same_push_identity() {
    let repository_id = Uuid::now_v7();
    let push_id = Uuid::now_v7();
    let request = PushFinalizeRequest {
        schema_version: 1,
        repository_id,
        ref_updates: vec![PushRefUpdate {
            ref_name: "refs/heads/main".to_owned(),
            old_oid: None,
            new_oid: "a".repeat(40),
        }],
        plan: PushAdmissionPlan {
            estimated_bytes: 512,
            estimated_objects: 3,
        },
        client_version: "1.2.3".to_owned(),
        replication: None,
    };
    let response_body = PushFinalizeResponse::updated(request.ref_updates.clone());
    let capabilities = ServiceCapabilities {
        schema_version: 1,
        api_versions: vec![1],
        transfer_grant_versions: vec![1],
        transfer_modes: vec![TransferMode::Gateway],
        capabilities: vec![CapabilityName::new("protected-push-v1").unwrap()],
        max_page_size: 100,
        max_grant_lifetime_seconds: 900,
    };
    let (client, transport) = client(vec![
        service_error(StatusCode::SERVICE_UNAVAILABLE, true),
        response(StatusCode::OK, &response_body),
    ]);

    let finalized = client
        .finalize_push("acme", "models", push_id, &request, &capabilities)
        .await
        .unwrap();

    assert_eq!(finalized.ref_updates, request.ref_updates);
    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests.iter().all(|sent| {
        sent.endpoint.as_str()
            == format!(
                "https://api.crab.build/v1/repositories/acme/models/pushes/{push_id}:finalize"
            )
            && sent.idempotency_key.is_none()
            && sent.body == requests[0].body
    }));
}

#[tokio::test]
async fn push_abort_retries_the_same_empty_request_and_validates_identity() {
    let push_id = Uuid::now_v7();
    let capabilities = ServiceCapabilities {
        schema_version: 1,
        api_versions: vec![1],
        transfer_grant_versions: vec![1],
        transfer_modes: vec![TransferMode::Gateway],
        capabilities: vec![CapabilityName::new("protected-push-v1").unwrap()],
        max_page_size: 100,
        max_grant_lifetime_seconds: 900,
    };
    let aborted = PushAbortResponse {
        schema_version: 1,
        push_id,
        state: "aborted".to_owned(),
    };
    let (client, transport) = client(vec![
        service_error(StatusCode::SERVICE_UNAVAILABLE, true),
        response(StatusCode::OK, &aborted),
    ]);

    let response = client
        .abort_push("acme", "models", push_id, &capabilities)
        .await
        .unwrap();

    assert_eq!(response, aborted);
    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests.iter().all(|sent| {
        sent.endpoint.as_str()
            == format!("https://api.crab.build/v1/repositories/acme/models/pushes/{push_id}:abort")
            && sent.idempotency_key.is_none()
            && sent.body.is_none()
    }));
}

#[tokio::test]
async fn non_retryable_service_error_preserves_stable_envelope() {
    let (client, transport) = client(vec![service_error(StatusCode::FORBIDDEN, false)]);

    let error = client
        .resolve_repository("acme", "models")
        .await
        .unwrap_err();

    assert_eq!(error.service_error().unwrap().code, "service_unavailable");
    assert_eq!(transport.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn organization_mutations_send_concurrency_and_idempotency_headers() {
    let organization = Organization {
        schema_version: 1,
        id: Uuid::now_v7(),
        slug: "acme-ai".to_owned(),
        state: OrganizationState::Active,
        revision: 2,
    };
    let mut updated = response(StatusCode::OK, &organization);
    updated.etag = Some("\"revision-2\"".to_owned());
    let (client, transport) = client(vec![updated]);
    let etag = EntityTag::new("\"revision-1\"").unwrap();
    let key = IdempotencyKey::new("rename-acme-1").unwrap();

    client
        .update_organization("acme", "acme-ai", &etag, &key)
        .await
        .unwrap();

    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests[0].method, Method::PATCH);
    assert_eq!(
        requests[0].endpoint.as_str(),
        "https://api.crab.build/v1/organizations/acme"
    );
    assert_eq!(requests[0].if_match.as_deref(), Some("\"revision-1\""));
    assert_eq!(
        requests[0].idempotency_key.as_deref(),
        Some("rename-acme-1")
    );
}

#[tokio::test]
async fn membership_remove_accepts_only_an_empty_no_content_response() {
    let principal_id = Uuid::now_v7();
    let (client, transport) = client(vec![ApiHttpResponse {
        status: StatusCode::NO_CONTENT,
        etag: None,
        retry_after: None,
        body: Vec::new(),
    }]);

    client
        .remove_organization_member(
            "acme",
            principal_id,
            &EntityTag::new("\"revision-4\"").unwrap(),
            &IdempotencyKey::new("remove-member-1").unwrap(),
        )
        .await
        .unwrap();

    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests[0].method, Method::DELETE);
    assert_eq!(
        requests[0].endpoint.as_str(),
        format!("https://api.crab.build/v1/organizations/acme/members/{principal_id}")
    );
}

#[tokio::test]
async fn repository_restore_uses_canonical_action_route() {
    let mut restored_repository = repository();
    restored_repository.state = super::super::RepositoryState::Provisioning;
    restored_repository.revision = 7;
    let mut restored = response(StatusCode::OK, &restored_repository);
    restored.etag = Some("\"revision-7\"".to_owned());
    let (client, transport) = client(vec![restored]);

    client
        .restore_repository(
            "acme",
            "models",
            &EntityTag::new("\"revision-6\"").unwrap(),
            &IdempotencyKey::new("restore-models-1").unwrap(),
        )
        .await
        .unwrap();

    let requests = transport.requests.lock().unwrap();
    assert_eq!(requests[0].method, Method::POST);
    assert_eq!(
        requests[0].endpoint.as_str(),
        "https://api.crab.build/v1/repositories/acme/models:restore"
    );
}

#[tokio::test]
async fn issued_service_token_is_redacted_in_debug_output() {
    let issued = IssuedServiceToken {
        schema_version: 1,
        account: ServiceAccount {
            id: Uuid::now_v7(),
            organization_id: Uuid::now_v7(),
            name: "ci".to_owned(),
            kind: "opaque_token".to_owned(),
            role: "writer".to_owned(),
            issuer: None,
            subject: None,
            revoked_at: None,
            revision: 1,
        },
        credential_id: Uuid::now_v7(),
        token: SecretString::new("one-time-secret").unwrap(),
        expires_at: OffsetDateTime::now_utc() + time::Duration::hours(1),
    };
    let mut created = response(StatusCode::CREATED, &issued);
    created.etag = Some("\"revision-1\"".to_owned());
    let (client, _) = client(vec![created]);

    let returned = client
        .create_opaque_service_account("acme", "ci", "writer", 3_600)
        .await
        .unwrap();

    let debug = format!("{:?}", returned.value);
    assert!(!debug.contains("one-time-secret"));
    assert!(debug.contains("<redacted>"));
}
