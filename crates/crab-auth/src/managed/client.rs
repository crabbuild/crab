use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, ETAG, HeaderName, IF_MATCH, RETRY_AFTER};
use reqwest::{Method, StatusCode, Url};
use serde::de::DeserializeOwned;
use time::OffsetDateTime;
use uuid::Uuid;

use super::{
    AddOrganizationMemberRequest, ApiError, ApiErrorEnvelope, CreateOrganizationRequest,
    CreateRepositoryRequest, CreateServiceAccountRequest, DiscoveryDocument, EntityTag,
    GrantValidationContext, IdempotencyKey, IssuedServiceToken, LogicalRepository, Organization,
    OrganizationMemberPage, OrganizationMembership, OrganizationPage, OrganizationRole, PageCursor,
    PushAbortResponse, PushFinalizeRequest, PushPrepareRequest, PushPrepareResponse,
    RepositoryPage, RepositoryRequestedState, RotateServiceTokenRequest, ServiceAccount,
    ServiceAccountKind, ServiceAccountList, ServiceCapabilities, ServiceProfile, TransferGrant,
    TransferGrantRequest, TransferMode, TransferOperation, TransferPermission, TransferTransport,
    UpdateOrganizationMemberRequest, UpdateOrganizationRequest, UpdateRepositoryRequest,
    managed_http_client,
};
use crate::error::AuthError;
use crate::{PushFinalizeResponse, validate_push_finalize_response};

const MAX_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_ATTEMPTS: u8 = 3;
const MAX_RETRY_DELAY: Duration = Duration::from_secs(2);
const IDEMPOTENCY_KEY: HeaderName = HeaderName::from_static("idempotency-key");

pub type ManagedApiResult<T> = std::result::Result<T, ManagedApiError>;

/// Bearer credential retained only in memory and redacted from diagnostics.
#[derive(Clone)]
pub struct BearerToken(String);

impl BearerToken {
    pub fn new(value: impl Into<String>) -> ManagedApiResult<Self> {
        let value = value.into();
        if value.is_empty() || value.len() > 16_384 || value.chars().any(char::is_control) {
            return Err(ManagedApiError::InvalidRequest {
                reason: "managed bearer token is empty, oversized, or contains controls".to_owned(),
            });
        }
        Ok(Self(value))
    }

    fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for BearerToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

/// Stable managed API failures with the service envelope preserved.
#[derive(Debug, thiserror::Error)]
pub enum ManagedApiError {
    #[error("managed API request failed at {endpoint}: {source}")]
    Transport {
        endpoint: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("managed API returned HTTP {status}: {error}")]
    Service {
        status: u16,
        retry_after: Option<Duration>,
        error: Box<ApiError>,
    },
    #[error("invalid managed API request: {reason}")]
    InvalidRequest { reason: String },
    #[error("invalid managed API response: {reason}")]
    InvalidResponse { reason: String },
    #[error(transparent)]
    Contract(#[from] AuthError),
    #[error("failed to encode managed API request: {source}")]
    Encode {
        #[source]
        source: serde_json::Error,
    },
    #[error("failed to decode managed API response: {source}")]
    Decode {
        #[source]
        source: serde_json::Error,
    },
    #[error("managed API returned an expired transfer grant")]
    ExpiredGrant,
}

impl ManagedApiError {
    #[must_use]
    pub fn service_error(&self) -> Option<&ApiError> {
        match self {
            Self::Service { error, .. } => Some(error),
            _ => None,
        }
    }

    fn can_retry(&self) -> bool {
        match self {
            Self::Transport { source, .. } => source.is_connect() || source.is_timeout(),
            Self::Service { status, error, .. } => {
                error.retryable
                    && matches!(
                        StatusCode::from_u16(*status),
                        Ok(StatusCode::TOO_MANY_REQUESTS
                            | StatusCode::BAD_GATEWAY
                            | StatusCode::SERVICE_UNAVAILABLE
                            | StatusCode::GATEWAY_TIMEOUT)
                    )
            }
            _ => false,
        }
    }

    fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Service { retry_after, .. } => *retry_after,
            _ => None,
        }
    }
}

/// A response value paired with its required strong entity tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedEntity<T> {
    pub value: T,
    pub etag: EntityTag,
}

/// Shared managed control-plane client used by CLI, SDK, and desktop adapters.
pub struct ManagedApiClient {
    authority: String,
    api_base: Url,
    bearer: BearerToken,
    transport: Arc<dyn ApiTransport>,
}

impl ManagedApiClient {
    /// Builds an API client from already validated, authority-bound discovery.
    pub fn new(
        profile: &ServiceProfile,
        discovery: &DiscoveryDocument,
        bearer: BearerToken,
    ) -> ManagedApiResult<Self> {
        profile.validate()?;
        discovery.validate(&profile.authority, &[1])?;
        let api_base = Url::parse(&format!("{}/", discovery.api_base.trim_end_matches('/')))
            .map_err(|_| ManagedApiError::InvalidResponse {
                reason: "managed API base is invalid".to_owned(),
            })?;
        let transport = Arc::new(ReqwestApiTransport {
            client: managed_http_client(profile)?,
        });
        Ok(Self {
            authority: profile.authority.clone(),
            api_base,
            bearer,
            transport,
        })
    }

    /// Fetches and validates the authenticated service capability contract.
    pub async fn capabilities(&self) -> ManagedApiResult<ServiceCapabilities> {
        let capabilities = self
            .execute::<ServiceCapabilities>(
                ApiRequest::get(self.endpoint("capabilities")?, self.bearer.clone()),
                &[StatusCode::OK],
                RetrySafety::Safe,
            )
            .await?
            .value;
        capabilities.validate()?;
        Ok(capabilities)
    }

    /// Lists one cursor page of organizations visible to the current principal.
    pub async fn list_organizations(
        &self,
        cursor: Option<&PageCursor>,
        limit: u16,
    ) -> ManagedApiResult<OrganizationPage> {
        let endpoint = self.collection_endpoint("organizations", cursor, limit)?;
        let page = self
            .execute::<OrganizationPage>(
                ApiRequest::get(endpoint, self.bearer.clone()),
                &[StatusCode::OK],
                RetrySafety::Safe,
            )
            .await?
            .value;
        validate_organization_page(&page)?;
        Ok(page)
    }

    /// Fetches an organization and its strong revision tag.
    pub async fn organization(
        &self,
        organization: &str,
    ) -> ManagedApiResult<ManagedEntity<Organization>> {
        validate_slug(organization, "organization")?;
        let response = self
            .execute::<Organization>(
                ApiRequest::get(
                    self.endpoint(&format!("organizations/{organization}"))?,
                    self.bearer.clone(),
                ),
                &[StatusCode::OK],
                RetrySafety::Safe,
            )
            .await?;
        validate_organization(&response.value)?;
        response.require_etag()
    }

    /// Creates an organization with an idempotency key safe for retry.
    pub async fn create_organization(
        &self,
        organization: &str,
        idempotency_key: &IdempotencyKey,
    ) -> ManagedApiResult<ManagedEntity<Organization>> {
        validate_slug(organization, "organization")?;
        let request = CreateOrganizationRequest {
            schema_version: 1,
            slug: organization.to_owned(),
        };
        let response = self
            .execute::<Organization>(
                ApiRequest::post(
                    self.endpoint("organizations")?,
                    self.bearer.clone(),
                    encode(&request)?,
                )
                .with_idempotency(idempotency_key),
                &[StatusCode::CREATED],
                RetrySafety::Idempotent,
            )
            .await?;
        validate_organization(&response.value)?;
        response.require_etag()
    }

    /// Renames an organization using optimistic concurrency and idempotency.
    pub async fn update_organization(
        &self,
        organization: &str,
        slug: &str,
        etag: &EntityTag,
        idempotency_key: &IdempotencyKey,
    ) -> ManagedApiResult<ManagedEntity<Organization>> {
        validate_slug(organization, "organization")?;
        validate_slug(slug, "organization")?;
        let request = UpdateOrganizationRequest {
            schema_version: 1,
            slug: slug.to_owned(),
        };
        let response = self
            .execute::<Organization>(
                ApiRequest::patch(
                    self.endpoint(&format!("organizations/{organization}"))?,
                    self.bearer.clone(),
                    encode(&request)?,
                )
                .with_if_match(etag)
                .with_idempotency(idempotency_key),
                &[StatusCode::OK],
                RetrySafety::Idempotent,
            )
            .await?;
        validate_organization(&response.value)?;
        response.require_etag()
    }

    /// Soft-deletes an organization using optimistic concurrency and idempotency.
    pub async fn delete_organization(
        &self,
        organization: &str,
        etag: &EntityTag,
        idempotency_key: &IdempotencyKey,
    ) -> ManagedApiResult<()> {
        validate_slug(organization, "organization")?;
        self.execute_empty(
            ApiRequest::delete(
                self.endpoint(&format!("organizations/{organization}"))?,
                self.bearer.clone(),
            )
            .with_if_match(etag)
            .with_idempotency(idempotency_key),
            &[StatusCode::NO_CONTENT],
            RetrySafety::Idempotent,
        )
        .await
    }

    /// Lists one cursor page of organization memberships.
    pub async fn list_organization_members(
        &self,
        organization: &str,
        cursor: Option<&PageCursor>,
        limit: u16,
    ) -> ManagedApiResult<OrganizationMemberPage> {
        validate_slug(organization, "organization")?;
        let endpoint = self.collection_endpoint(
            &format!("organizations/{organization}/members"),
            cursor,
            limit,
        )?;
        let page = self
            .execute::<OrganizationMemberPage>(
                ApiRequest::get(endpoint, self.bearer.clone()),
                &[StatusCode::OK],
                RetrySafety::Safe,
            )
            .await?
            .value;
        validate_member_page(&page)?;
        Ok(page)
    }

    /// Adds an organization member with an idempotency key safe for retry.
    pub async fn add_organization_member(
        &self,
        organization: &str,
        principal_id: Uuid,
        role: OrganizationRole,
        idempotency_key: &IdempotencyKey,
    ) -> ManagedApiResult<ManagedEntity<OrganizationMembership>> {
        validate_slug(organization, "organization")?;
        if principal_id.is_nil() {
            return invalid_request("principal ID must not be nil");
        }
        let request = AddOrganizationMemberRequest {
            schema_version: 1,
            principal_id,
            role,
        };
        let response = self
            .execute::<OrganizationMembership>(
                ApiRequest::post(
                    self.endpoint(&format!("organizations/{organization}/members"))?,
                    self.bearer.clone(),
                    encode(&request)?,
                )
                .with_idempotency(idempotency_key),
                &[StatusCode::CREATED],
                RetrySafety::Idempotent,
            )
            .await?;
        validate_membership(&response.value)?;
        response.require_etag()
    }

    /// Changes an organization member role using optimistic concurrency.
    pub async fn update_organization_member(
        &self,
        organization: &str,
        principal_id: Uuid,
        role: OrganizationRole,
        etag: &EntityTag,
        idempotency_key: &IdempotencyKey,
    ) -> ManagedApiResult<ManagedEntity<OrganizationMembership>> {
        validate_slug(organization, "organization")?;
        if principal_id.is_nil() {
            return invalid_request("principal ID must not be nil");
        }
        let request = UpdateOrganizationMemberRequest {
            schema_version: 1,
            role,
        };
        let response = self
            .execute::<OrganizationMembership>(
                ApiRequest::patch(
                    self.endpoint(&format!(
                        "organizations/{organization}/members/{principal_id}"
                    ))?,
                    self.bearer.clone(),
                    encode(&request)?,
                )
                .with_if_match(etag)
                .with_idempotency(idempotency_key),
                &[StatusCode::OK],
                RetrySafety::Idempotent,
            )
            .await?;
        validate_membership(&response.value)?;
        response.require_etag()
    }

    /// Removes an organization member using optimistic concurrency.
    pub async fn remove_organization_member(
        &self,
        organization: &str,
        principal_id: Uuid,
        etag: &EntityTag,
        idempotency_key: &IdempotencyKey,
    ) -> ManagedApiResult<()> {
        validate_slug(organization, "organization")?;
        if principal_id.is_nil() {
            return invalid_request("principal ID must not be nil");
        }
        self.execute_empty(
            ApiRequest::delete(
                self.endpoint(&format!(
                    "organizations/{organization}/members/{principal_id}"
                ))?,
                self.bearer.clone(),
            )
            .with_if_match(etag)
            .with_idempotency(idempotency_key),
            &[StatusCode::NO_CONTENT],
            RetrySafety::Idempotent,
        )
        .await
    }

    /// Resolves one logical repository without exposing its physical placement.
    pub async fn resolve_repository(
        &self,
        organization: &str,
        repository: &str,
    ) -> ManagedApiResult<ManagedEntity<LogicalRepository>> {
        validate_slug(organization, "organization")?;
        validate_slug(repository, "repository")?;
        let response = self
            .execute::<LogicalRepository>(
                ApiRequest::get(
                    self.endpoint(&format!("repositories/{organization}/{repository}"))?,
                    self.bearer.clone(),
                ),
                &[StatusCode::OK],
                RetrySafety::Safe,
            )
            .await?;
        response.value.validate()?;
        let expected = format!("crab://{}/{organization}/{repository}", self.authority);
        if response.value.canonical_url != expected {
            return Err(ManagedApiError::InvalidResponse {
                reason: "resolved repository canonical URL does not match the request".to_owned(),
            });
        }
        response.require_etag()
    }

    /// Lists one cursor page of repositories visible in an organization.
    pub async fn list_repositories(
        &self,
        organization: &str,
        cursor: Option<&PageCursor>,
        limit: u16,
    ) -> ManagedApiResult<RepositoryPage> {
        validate_slug(organization, "organization")?;
        if !(1..=100).contains(&limit) {
            return Err(ManagedApiError::InvalidRequest {
                reason: "repository page limit must be between 1 and 100".to_owned(),
            });
        }
        let mut endpoint = self.endpoint(&format!("organizations/{organization}/repositories"))?;
        {
            let mut query = endpoint.query_pairs_mut();
            query.append_pair("limit", &limit.to_string());
            if let Some(cursor) = cursor {
                query.append_pair("cursor", cursor.as_str());
            }
        }
        let page = self
            .execute::<RepositoryPage>(
                ApiRequest::get(endpoint, self.bearer.clone()),
                &[StatusCode::OK],
                RetrySafety::Safe,
            )
            .await?
            .value;
        page.validate()?;
        let prefix = format!("crab://{}/{organization}/", self.authority);
        if page
            .repositories
            .iter()
            .any(|repository| !repository.canonical_url.starts_with(&prefix))
        {
            return Err(ManagedApiError::InvalidResponse {
                reason: "repository page contains an entry from another authority or organization"
                    .to_owned(),
            });
        }
        Ok(page)
    }

    /// Creates a repository with an idempotency key safe for automatic retry.
    pub async fn create_repository(
        &self,
        organization: &str,
        repository: &str,
        idempotency_key: &IdempotencyKey,
    ) -> ManagedApiResult<ManagedEntity<LogicalRepository>> {
        validate_slug(organization, "organization")?;
        validate_slug(repository, "repository")?;
        let body = serde_json::to_vec(&CreateRepositoryRequest {
            schema_version: 1,
            slug: repository.to_owned(),
        })
        .map_err(|source| ManagedApiError::Encode { source })?;
        let response = self
            .execute::<LogicalRepository>(
                ApiRequest::post(
                    self.endpoint(&format!("organizations/{organization}/repositories"))?,
                    self.bearer.clone(),
                    body,
                )
                .with_idempotency(idempotency_key),
                &[StatusCode::CREATED],
                RetrySafety::Idempotent,
            )
            .await?;
        response.value.validate()?;
        let expected = format!("crab://{}/{organization}/{repository}", self.authority);
        if response.value.canonical_url != expected {
            return Err(ManagedApiError::InvalidResponse {
                reason: "created repository canonical URL does not match the request".to_owned(),
            });
        }
        response.require_etag()
    }

    /// Renames a repository using optimistic concurrency and idempotency.
    pub async fn rename_repository(
        &self,
        organization: &str,
        repository: &str,
        slug: &str,
        etag: &EntityTag,
        idempotency_key: &IdempotencyKey,
    ) -> ManagedApiResult<ManagedEntity<LogicalRepository>> {
        validate_slug(slug, "repository")?;
        self.update_repository(
            organization,
            repository,
            UpdateRepositoryRequest {
                schema_version: 1,
                slug: Some(slug.to_owned()),
                state: None,
            },
            etag,
            idempotency_key,
        )
        .await
    }

    /// Archives a repository using optimistic concurrency and idempotency.
    pub async fn archive_repository(
        &self,
        organization: &str,
        repository: &str,
        etag: &EntityTag,
        idempotency_key: &IdempotencyKey,
    ) -> ManagedApiResult<ManagedEntity<LogicalRepository>> {
        self.update_repository(
            organization,
            repository,
            UpdateRepositoryRequest {
                schema_version: 1,
                slug: None,
                state: Some(RepositoryRequestedState::Archived),
            },
            etag,
            idempotency_key,
        )
        .await
    }

    /// Soft-deletes a repository using optimistic concurrency and idempotency.
    pub async fn delete_repository(
        &self,
        organization: &str,
        repository: &str,
        etag: &EntityTag,
        idempotency_key: &IdempotencyKey,
    ) -> ManagedApiResult<ManagedEntity<LogicalRepository>> {
        self.repository_action(
            Method::DELETE,
            organization,
            repository,
            "",
            etag,
            idempotency_key,
        )
        .await
    }

    /// Restores a soft-deleted repository using optimistic concurrency and idempotency.
    pub async fn restore_repository(
        &self,
        organization: &str,
        repository: &str,
        etag: &EntityTag,
        idempotency_key: &IdempotencyKey,
    ) -> ManagedApiResult<ManagedEntity<LogicalRepository>> {
        self.repository_action(
            Method::POST,
            organization,
            repository,
            ":restore",
            etag,
            idempotency_key,
        )
        .await
    }

    /// Lists service accounts in an organization.
    pub async fn list_service_accounts(
        &self,
        organization: &str,
    ) -> ManagedApiResult<ServiceAccountList> {
        validate_slug(organization, "organization")?;
        let list = self
            .execute::<ServiceAccountList>(
                ApiRequest::get(
                    self.endpoint(&format!("organizations/{organization}/service-accounts"))?,
                    self.bearer.clone(),
                ),
                &[StatusCode::OK],
                RetrySafety::Safe,
            )
            .await?
            .value;
        validate_service_account_list(&list)?;
        Ok(list)
    }

    /// Creates an OIDC workload service account.
    pub async fn create_workload_service_account(
        &self,
        organization: &str,
        name: &str,
        role: &str,
        issuer: &str,
        subject: &str,
    ) -> ManagedApiResult<ManagedEntity<ServiceAccount>> {
        let request = CreateServiceAccountRequest {
            kind: ServiceAccountKind::OidcWorkload,
            name: name.to_owned(),
            role: role.to_owned(),
            issuer: Some(issuer.to_owned()),
            subject: Some(subject.to_owned()),
            expires_in_seconds: None,
        };
        let response = self.create_service_account(organization, &request).await?;
        validate_service_account(&response.value)?;
        response.require_etag()
    }

    /// Creates an opaque service account and returns its one-time credential.
    pub async fn create_opaque_service_account(
        &self,
        organization: &str,
        name: &str,
        role: &str,
        expires_in_seconds: u64,
    ) -> ManagedApiResult<ManagedEntity<IssuedServiceToken>> {
        let request = CreateServiceAccountRequest {
            kind: ServiceAccountKind::OpaqueToken,
            name: name.to_owned(),
            role: role.to_owned(),
            issuer: None,
            subject: None,
            expires_in_seconds: Some(expires_in_seconds),
        };
        let response = self.create_service_account(organization, &request).await?;
        validate_issued_token(&response.value)?;
        response.require_etag()
    }

    /// Rotates an opaque service-account credential and returns the one-time secret.
    pub async fn rotate_service_account_token(
        &self,
        organization: &str,
        account_id: Uuid,
        expires_in_seconds: u64,
        overlap_seconds: u64,
        etag: &EntityTag,
    ) -> ManagedApiResult<ManagedEntity<IssuedServiceToken>> {
        validate_slug(organization, "organization")?;
        if account_id.is_nil() {
            return invalid_request("service account ID must not be nil");
        }
        let request = RotateServiceTokenRequest {
            expires_in_seconds,
            overlap_seconds,
        };
        let response = self
            .execute::<IssuedServiceToken>(
                ApiRequest::post(
                    self.endpoint(&format!(
                        "organizations/{organization}/service-accounts/{account_id}/credentials:rotate"
                    ))?,
                    self.bearer.clone(),
                    encode(&request)?,
                )
                .with_if_match(etag),
                &[StatusCode::CREATED],
                RetrySafety::Never,
            )
            .await?;
        validate_issued_token(&response.value)?;
        response.require_etag()
    }

    /// Revokes a service account using optimistic concurrency.
    pub async fn revoke_service_account(
        &self,
        organization: &str,
        account_id: Uuid,
        etag: &EntityTag,
    ) -> ManagedApiResult<()> {
        validate_slug(organization, "organization")?;
        if account_id.is_nil() {
            return invalid_request("service account ID must not be nil");
        }
        self.execute_empty(
            ApiRequest::delete(
                self.endpoint(&format!(
                    "organizations/{organization}/service-accounts/{account_id}"
                ))?,
                self.bearer.clone(),
            )
            .with_if_match(etag),
            &[StatusCode::NO_CONTENT],
            RetrySafety::Never,
        )
        .await
    }

    /// Issues a new short-lived read grant. Grant POSTs are never auto-retried.
    pub async fn issue_transfer_grant(
        &self,
        organization: &str,
        repository: &str,
        repository_id: Uuid,
        operation: TransferOperation,
        capabilities: &ServiceCapabilities,
    ) -> ManagedApiResult<TransferGrant> {
        validate_slug(organization, "organization")?;
        validate_slug(repository, "repository")?;
        capabilities.validate()?;
        if !matches!(
            operation,
            TransferOperation::Clone | TransferOperation::Fetch | TransferOperation::Hydrate
        ) {
            return Err(ManagedApiError::InvalidRequest {
                reason: "read grant client cannot request a push operation".to_owned(),
            });
        }
        let body = serde_json::to_vec(&TransferGrantRequest {
            schema_version: 1,
            repository_id,
            operation,
            push_id: None,
        })
        .map_err(|source| ManagedApiError::Encode { source })?;
        let grant = self
            .execute::<TransferGrant>(
                ApiRequest::post(
                    self.endpoint(&format!(
                        "repositories/{organization}/{repository}/transfer-grants"
                    ))?,
                    self.bearer.clone(),
                    body,
                ),
                &[StatusCode::CREATED],
                RetrySafety::Never,
            )
            .await?
            .value;
        validate_grant(&grant, repository_id, operation, capabilities)?;
        Ok(grant)
    }

    /// Refreshes a read grant by issuing a new independently validated grant.
    pub async fn refresh_transfer_grant(
        &self,
        organization: &str,
        repository: &str,
        repository_id: Uuid,
        operation: TransferOperation,
        capabilities: &ServiceCapabilities,
    ) -> ManagedApiResult<TransferGrant> {
        self.issue_transfer_grant(
            organization,
            repository,
            repository_id,
            operation,
            capabilities,
        )
        .await
    }

    /// Prepares or resumes one durable protected-push session.
    ///
    /// Reusing `idempotency_key` with the identical request returns the same
    /// push ID and may return fresh credentials for that session.
    pub async fn prepare_push(
        &self,
        organization: &str,
        repository: &str,
        request: &PushPrepareRequest,
        idempotency_key: &IdempotencyKey,
        capabilities: &ServiceCapabilities,
    ) -> ManagedApiResult<PushPrepareResponse> {
        validate_slug(organization, "organization")?;
        validate_slug(repository, "repository")?;
        validate_push_prepare_request(request)?;
        capabilities.validate()?;
        let body =
            serde_json::to_vec(request).map_err(|source| ManagedApiError::Encode { source })?;
        let prepared = self
            .execute::<PushPrepareResponse>(
                ApiRequest::post(
                    self.endpoint(&format!("repositories/{organization}/{repository}/pushes"))?,
                    self.bearer.clone(),
                    body,
                )
                .with_idempotency(idempotency_key),
                &[StatusCode::CREATED],
                RetrySafety::Idempotent,
            )
            .await?
            .value;
        validate_push_prepare_response(&prepared, request.repository_id, capabilities)?;
        Ok(prepared)
    }

    /// Finalizes one durable protected-push session.
    pub async fn finalize_push(
        &self,
        organization: &str,
        repository: &str,
        push_id: Uuid,
        request: &PushFinalizeRequest,
        capabilities: &ServiceCapabilities,
    ) -> ManagedApiResult<PushFinalizeResponse> {
        validate_slug(organization, "organization")?;
        validate_slug(repository, "repository")?;
        if push_id.is_nil() {
            return Err(ManagedApiError::InvalidRequest {
                reason: "managed push ID must be non-nil".to_owned(),
            });
        }
        validate_push_finalize_request(request)?;
        capabilities.validate()?;
        let body =
            serde_json::to_vec(request).map_err(|source| ManagedApiError::Encode { source })?;
        let response = self
            .execute::<PushFinalizeResponse>(
                ApiRequest::post(
                    self.endpoint(&format!(
                        "repositories/{organization}/{repository}/pushes/{push_id}:finalize"
                    ))?,
                    self.bearer.clone(),
                    body,
                ),
                &[StatusCode::OK],
                RetrySafety::Idempotent,
            )
            .await?
            .value;
        validate_push_finalize_response(&response).map_err(ManagedApiError::Contract)?;
        Ok(response)
    }

    /// Aborts one owned managed push session or an administratively controlled session.
    pub async fn abort_push(
        &self,
        organization: &str,
        repository: &str,
        push_id: Uuid,
        capabilities: &ServiceCapabilities,
    ) -> ManagedApiResult<PushAbortResponse> {
        validate_slug(organization, "organization")?;
        validate_slug(repository, "repository")?;
        if push_id.is_nil() {
            return Err(ManagedApiError::InvalidRequest {
                reason: "managed push ID must be non-nil".to_owned(),
            });
        }
        capabilities.validate()?;
        let response = self
            .execute::<PushAbortResponse>(
                ApiRequest::post_empty(
                    self.endpoint(&format!(
                        "repositories/{organization}/{repository}/pushes/{push_id}:abort"
                    ))?,
                    self.bearer.clone(),
                ),
                &[StatusCode::OK],
                RetrySafety::Idempotent,
            )
            .await?
            .value;
        if response.schema_version != 1
            || response.push_id != push_id
            || response.state != "aborted"
        {
            return Err(ManagedApiError::InvalidResponse {
                reason: "managed abort response does not match the requested push session"
                    .to_owned(),
            });
        }
        Ok(response)
    }

    async fn update_repository(
        &self,
        organization: &str,
        repository: &str,
        request: UpdateRepositoryRequest,
        etag: &EntityTag,
        idempotency_key: &IdempotencyKey,
    ) -> ManagedApiResult<ManagedEntity<LogicalRepository>> {
        validate_slug(organization, "organization")?;
        validate_slug(repository, "repository")?;
        let response = self
            .execute::<LogicalRepository>(
                ApiRequest::patch(
                    self.endpoint(&format!("repositories/{organization}/{repository}"))?,
                    self.bearer.clone(),
                    encode(&request)?,
                )
                .with_if_match(etag)
                .with_idempotency(idempotency_key),
                &[StatusCode::OK],
                RetrySafety::Idempotent,
            )
            .await?;
        response.value.validate()?;
        response.require_etag()
    }

    async fn repository_action(
        &self,
        method: Method,
        organization: &str,
        repository: &str,
        suffix: &str,
        etag: &EntityTag,
        idempotency_key: &IdempotencyKey,
    ) -> ManagedApiResult<ManagedEntity<LogicalRepository>> {
        validate_slug(organization, "organization")?;
        validate_slug(repository, "repository")?;
        let endpoint =
            self.endpoint(&format!("repositories/{organization}/{repository}{suffix}"))?;
        let request = if method == Method::DELETE {
            ApiRequest::delete(endpoint, self.bearer.clone())
        } else {
            ApiRequest::post_empty(endpoint, self.bearer.clone())
        };
        let response = self
            .execute::<LogicalRepository>(
                request
                    .with_if_match(etag)
                    .with_idempotency(idempotency_key),
                &[StatusCode::OK],
                RetrySafety::Idempotent,
            )
            .await?;
        response.value.validate()?;
        response.require_etag()
    }

    async fn create_service_account<T: DeserializeOwned>(
        &self,
        organization: &str,
        request: &CreateServiceAccountRequest,
    ) -> ManagedApiResult<ApiValue<T>> {
        validate_slug(organization, "organization")?;
        if request.name.is_empty() || request.role.is_empty() {
            return invalid_request("service account name and role must not be empty");
        }
        self.execute::<T>(
            ApiRequest::post(
                self.endpoint(&format!("organizations/{organization}/service-accounts"))?,
                self.bearer.clone(),
                encode(request)?,
            ),
            &[StatusCode::CREATED],
            RetrySafety::Never,
        )
        .await
    }

    fn collection_endpoint(
        &self,
        route: &str,
        cursor: Option<&PageCursor>,
        limit: u16,
    ) -> ManagedApiResult<Url> {
        if !(1..=100).contains(&limit) {
            return invalid_request("page limit must be between 1 and 100");
        }
        let mut endpoint = self.endpoint(route)?;
        let mut query = endpoint.query_pairs_mut();
        query.append_pair("limit", &limit.to_string());
        if let Some(cursor) = cursor {
            query.append_pair("cursor", cursor.as_str());
        }
        drop(query);
        Ok(endpoint)
    }

    fn endpoint(&self, relative: &str) -> ManagedApiResult<Url> {
        self.api_base
            .join(relative)
            .map_err(|_| ManagedApiError::InvalidResponse {
                reason: "managed API route is invalid".to_owned(),
            })
    }

    async fn execute<T: DeserializeOwned>(
        &self,
        request: ApiRequest,
        expected: &[StatusCode],
        safety: RetrySafety,
    ) -> ManagedApiResult<ApiValue<T>> {
        let mut attempt = 0u8;
        loop {
            attempt = attempt.saturating_add(1);
            let result = self
                .transport
                .send(request.clone())
                .await
                .and_then(|response| decode_response(response, expected));
            match result {
                Ok(value) => return Ok(value),
                Err(error) if safety.can_retry() && attempt < MAX_ATTEMPTS && error.can_retry() => {
                    let backoff = Duration::from_millis(100 * (1u64 << (attempt - 1)));
                    tokio::time::sleep(error.retry_after().unwrap_or(backoff).min(MAX_RETRY_DELAY))
                        .await;
                }
                Err(error) => return Err(error),
            }
        }
    }

    async fn execute_empty(
        &self,
        request: ApiRequest,
        expected: &[StatusCode],
        safety: RetrySafety,
    ) -> ManagedApiResult<()> {
        let mut attempt = 0u8;
        loop {
            attempt = attempt.saturating_add(1);
            let result = self
                .transport
                .send(request.clone())
                .await
                .and_then(|response| decode_empty_response(response, expected));
            match result {
                Ok(()) => return Ok(()),
                Err(error) if safety.can_retry() && attempt < MAX_ATTEMPTS && error.can_retry() => {
                    let backoff = Duration::from_millis(100 * (1u64 << (attempt - 1)));
                    tokio::time::sleep(error.retry_after().unwrap_or(backoff).min(MAX_RETRY_DELAY))
                        .await;
                }
                Err(error) => return Err(error),
            }
        }
    }

    #[cfg(test)]
    fn with_transport(
        authority: &str,
        bearer: BearerToken,
        transport: Arc<dyn ApiTransport>,
    ) -> Self {
        Self {
            authority: authority.to_owned(),
            api_base: Url::parse(&format!("https://api.{authority}/v1/")).unwrap(),
            bearer,
            transport,
        }
    }
}

fn validate_grant(
    grant: &TransferGrant,
    repository_id: Uuid,
    operation: TransferOperation,
    capabilities: &ServiceCapabilities,
) -> ManagedApiResult<()> {
    let mode = transfer_mode(grant);
    if !capabilities.transfer_modes.contains(&mode) {
        return Err(ManagedApiError::InvalidResponse {
            reason: "transfer grant mode was not advertised by capabilities".to_owned(),
        });
    }
    let now = OffsetDateTime::now_utc();
    if grant.expires_at <= now {
        return Err(ManagedApiError::ExpiredGrant);
    }
    let not_after =
        now + time::Duration::seconds(i64::from(capabilities.max_grant_lifetime_seconds));
    let allowed = [
        TransferPermission::ReadObject,
        TransferPermission::ReadMetadata,
        TransferPermission::ListPrefix,
    ];
    grant.validate(GrantValidationContext {
        repository_id,
        operation,
        repository_prefix: &grant.storage_scope.repository_prefix,
        push_id: None,
        allowed_permissions: &allowed,
        now,
        not_after,
    })?;
    Ok(())
}

fn validate_push_prepare_request(request: &PushPrepareRequest) -> ManagedApiResult<()> {
    if request.schema_version != 1
        || request.repository_id.is_nil()
        || request.ref_updates.is_empty()
        || request.plan.estimated_bytes == 0
        || request.plan.estimated_objects == 0
        || request.client_version.is_empty()
        || request.client_version.len() > 128
        || request.client_version.chars().any(char::is_control)
    {
        return Err(ManagedApiError::InvalidRequest {
            reason:
                "managed push request has invalid version, identity, plan, refs, or client version"
                    .to_owned(),
        });
    }
    Ok(())
}

fn validate_push_finalize_request(request: &PushFinalizeRequest) -> ManagedApiResult<()> {
    if request.schema_version != 1
        || request.repository_id.is_nil()
        || request.ref_updates.is_empty()
        || request.plan.estimated_bytes == 0
        || request.plan.estimated_objects == 0
        || request.client_version.is_empty()
        || request.client_version.len() > 128
        || request.client_version.chars().any(char::is_control)
    {
        return Err(ManagedApiError::InvalidRequest {
            reason:
                "managed finalize request has invalid version, identity, plan, refs, or client version"
                    .to_owned(),
        });
    }
    Ok(())
}

fn validate_push_prepare_response(
    prepared: &PushPrepareResponse,
    repository_id: Uuid,
    capabilities: &ServiceCapabilities,
) -> ManagedApiResult<()> {
    if prepared.schema_version != 1
        || prepared.push_id.is_nil()
        || prepared.repository_id != repository_id
        || prepared.base_manifest_etag.is_empty()
        || prepared.base_manifest_etag.len() > 4_096
        || prepared.base_manifest_etag.chars().any(char::is_control)
    {
        return Err(ManagedApiError::InvalidResponse {
            reason: "managed push response has invalid session or base-manifest identity"
                .to_owned(),
        });
    }
    let mode = transfer_mode(&prepared.staging_grant);
    if !capabilities.transfer_modes.contains(&mode) {
        return Err(ManagedApiError::InvalidResponse {
            reason: "push staging grant mode was not advertised by capabilities".to_owned(),
        });
    }
    let now = OffsetDateTime::now_utc();
    let maximum_grant_expiry =
        now + time::Duration::seconds(i64::from(capabilities.max_grant_lifetime_seconds));
    if prepared.expires_at <= now {
        return Err(ManagedApiError::InvalidResponse {
            reason: "managed push session is already expired".to_owned(),
        });
    }
    let allowed = [
        TransferPermission::ReadObject,
        TransferPermission::ReadMetadata,
        TransferPermission::ListPrefix,
        TransferPermission::CreateImmutableObject,
    ];
    prepared.staging_grant.validate(GrantValidationContext {
        repository_id,
        operation: TransferOperation::PushUpload,
        repository_prefix: &prepared.staging_grant.storage_scope.repository_prefix,
        push_id: Some(prepared.push_id),
        allowed_permissions: &allowed,
        now,
        not_after: prepared.expires_at.min(maximum_grant_expiry),
    })?;
    Ok(())
}

fn transfer_mode(grant: &TransferGrant) -> TransferMode {
    match &grant.transport {
        TransferTransport::Direct { direct } => match &direct.credentials {
            super::ProviderCredentials::Aws { .. } => TransferMode::DirectS3,
            super::ProviderCredentials::Gcp { .. } => TransferMode::DirectGcs,
            super::ProviderCredentials::Azure { .. } => TransferMode::DirectAzure,
        },
        TransferTransport::Gateway { .. } => TransferMode::Gateway,
    }
}

fn validate_slug(value: &str, field: &str) -> ManagedApiResult<()> {
    if value.is_empty()
        || value.len() > 100
        || value.starts_with('-')
        || value.ends_with('-')
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(ManagedApiError::InvalidRequest {
            reason: format!("{field} slug is invalid"),
        });
    }
    Ok(())
}

fn encode(value: &impl serde::Serialize) -> ManagedApiResult<Vec<u8>> {
    serde_json::to_vec(value).map_err(|source| ManagedApiError::Encode { source })
}

fn invalid_request<T>(reason: impl Into<String>) -> ManagedApiResult<T> {
    Err(ManagedApiError::InvalidRequest {
        reason: reason.into(),
    })
}

fn invalid_response<T>(reason: impl Into<String>) -> ManagedApiResult<T> {
    Err(ManagedApiError::InvalidResponse {
        reason: reason.into(),
    })
}

fn validate_organization(organization: &Organization) -> ManagedApiResult<()> {
    if organization.schema_version != 1 || organization.id.is_nil() || organization.revision == 0 {
        return invalid_response(
            "organization response has invalid version, identity, or revision",
        );
    }
    validate_slug(&organization.slug, "organization").map_err(|_| {
        ManagedApiError::InvalidResponse {
            reason: "organization response has an invalid slug".to_owned(),
        }
    })
}

fn validate_organization_page(page: &OrganizationPage) -> ManagedApiResult<()> {
    if page.schema_version != 1 {
        return invalid_response("organization page has an unsupported schema version");
    }
    for organization in &page.organizations {
        validate_organization(organization)?;
    }
    Ok(())
}

fn validate_membership(membership: &OrganizationMembership) -> ManagedApiResult<()> {
    if membership.schema_version != 1
        || membership.organization_id.is_nil()
        || membership.principal_id.is_nil()
        || membership.revision == 0
    {
        return invalid_response("membership response has invalid version, identity, or revision");
    }
    Ok(())
}

fn validate_member_page(page: &OrganizationMemberPage) -> ManagedApiResult<()> {
    if page.schema_version != 1 {
        return invalid_response("membership page has an unsupported schema version");
    }
    for membership in &page.members {
        validate_membership(membership)?;
    }
    Ok(())
}

fn validate_service_account(account: &ServiceAccount) -> ManagedApiResult<()> {
    if account.id.is_nil()
        || account.organization_id.is_nil()
        || account.name.is_empty()
        || account.kind.is_empty()
        || account.role.is_empty()
        || account.revision == 0
    {
        return invalid_response(
            "service account response has invalid identity, fields, or revision",
        );
    }
    Ok(())
}

fn validate_service_account_list(list: &ServiceAccountList) -> ManagedApiResult<()> {
    if list.schema_version != 1 {
        return invalid_response("service account list has an unsupported schema version");
    }
    for account in &list.accounts {
        validate_service_account(account)?;
    }
    Ok(())
}

fn validate_issued_token(issued: &IssuedServiceToken) -> ManagedApiResult<()> {
    if issued.schema_version != 1 || issued.credential_id.is_nil() {
        return invalid_response("issued service token has invalid version or credential identity");
    }
    validate_service_account(&issued.account)
}

#[derive(Clone, Copy)]
enum RetrySafety {
    Safe,
    Idempotent,
    Never,
}

impl RetrySafety {
    const fn can_retry(self) -> bool {
        !matches!(self, Self::Never)
    }
}

#[derive(Clone)]
struct ApiRequest {
    method: Method,
    endpoint: Url,
    bearer: BearerToken,
    body: Option<Vec<u8>>,
    idempotency_key: Option<String>,
    if_match: Option<String>,
}

impl ApiRequest {
    fn get(endpoint: Url, bearer: BearerToken) -> Self {
        Self {
            method: Method::GET,
            endpoint,
            bearer,
            body: None,
            idempotency_key: None,
            if_match: None,
        }
    }

    fn post(endpoint: Url, bearer: BearerToken, body: Vec<u8>) -> Self {
        Self {
            method: Method::POST,
            endpoint,
            bearer,
            body: Some(body),
            idempotency_key: None,
            if_match: None,
        }
    }

    fn post_empty(endpoint: Url, bearer: BearerToken) -> Self {
        Self {
            method: Method::POST,
            endpoint,
            bearer,
            body: None,
            idempotency_key: None,
            if_match: None,
        }
    }

    fn patch(endpoint: Url, bearer: BearerToken, body: Vec<u8>) -> Self {
        Self {
            method: Method::PATCH,
            endpoint,
            bearer,
            body: Some(body),
            idempotency_key: None,
            if_match: None,
        }
    }

    fn delete(endpoint: Url, bearer: BearerToken) -> Self {
        Self {
            method: Method::DELETE,
            endpoint,
            bearer,
            body: None,
            idempotency_key: None,
            if_match: None,
        }
    }

    fn with_idempotency(mut self, key: &IdempotencyKey) -> Self {
        self.idempotency_key = Some(key.as_str().to_owned());
        self
    }

    fn with_if_match(mut self, etag: &EntityTag) -> Self {
        self.if_match = Some(etag.as_str().to_owned());
        self
    }
}

struct ApiHttpResponse {
    status: StatusCode,
    etag: Option<String>,
    retry_after: Option<Duration>,
    body: Vec<u8>,
}

struct ApiValue<T> {
    value: T,
    etag: Option<EntityTag>,
}

impl<T> ApiValue<T> {
    fn require_etag(self) -> ManagedApiResult<ManagedEntity<T>> {
        let etag = self.etag.ok_or_else(|| ManagedApiError::InvalidResponse {
            reason: "managed entity response is missing its ETag".to_owned(),
        })?;
        Ok(ManagedEntity {
            value: self.value,
            etag,
        })
    }
}

#[async_trait]
trait ApiTransport: Send + Sync {
    async fn send(&self, request: ApiRequest) -> ManagedApiResult<ApiHttpResponse>;
}

struct ReqwestApiTransport {
    client: reqwest::Client,
}

#[async_trait]
impl ApiTransport for ReqwestApiTransport {
    async fn send(&self, request: ApiRequest) -> ManagedApiResult<ApiHttpResponse> {
        let mut builder = self
            .client
            .request(request.method, request.endpoint.clone())
            .header(AUTHORIZATION, format!("Bearer {}", request.bearer.expose()));
        if let Some(body) = request.body {
            builder = builder.header(CONTENT_TYPE, "application/json").body(body);
        }
        if let Some(key) = request.idempotency_key {
            builder = builder.header(IDEMPOTENCY_KEY, key);
        }
        if let Some(etag) = request.if_match {
            builder = builder.header(IF_MATCH, etag);
        }
        let mut response = builder
            .send()
            .await
            .map_err(|source| ManagedApiError::Transport {
                endpoint: request.endpoint.to_string(),
                source,
            })?;
        if response.url() != &request.endpoint {
            return Err(ManagedApiError::InvalidResponse {
                reason: "managed API response changed origin or route".to_owned(),
            });
        }
        let status = response.status();
        let etag = response
            .headers()
            .get(ETAG)
            .map(|value| value.to_str().map(str::to_owned))
            .transpose()
            .map_err(|_| ManagedApiError::InvalidResponse {
                reason: "managed API ETag is not valid text".to_owned(),
            })?;
        let retry_after = response
            .headers()
            .get(RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .map(Duration::from_secs);
        let mut body = Vec::new();
        while let Some(chunk) =
            response
                .chunk()
                .await
                .map_err(|source| ManagedApiError::Transport {
                    endpoint: request.endpoint.to_string(),
                    source,
                })?
        {
            if body.len().saturating_add(chunk.len()) > MAX_RESPONSE_BYTES {
                return Err(ManagedApiError::InvalidResponse {
                    reason: "managed API response exceeds the four MiB limit".to_owned(),
                });
            }
            body.extend_from_slice(&chunk);
        }
        Ok(ApiHttpResponse {
            status,
            etag,
            retry_after,
            body,
        })
    }
}

fn decode_response<T: DeserializeOwned>(
    response: ApiHttpResponse,
    expected: &[StatusCode],
) -> ManagedApiResult<ApiValue<T>> {
    if expected.contains(&response.status) {
        let value = serde_json::from_slice(&response.body)
            .map_err(|source| ManagedApiError::Decode { source })?;
        let etag = response.etag.map(EntityTag::new).transpose()?;
        return Ok(ApiValue { value, etag });
    }
    let envelope: ApiErrorEnvelope = serde_json::from_slice(&response.body)
        .map_err(|source| ManagedApiError::Decode { source })?;
    validate_api_error(&envelope.error)?;
    Err(ManagedApiError::Service {
        status: response.status.as_u16(),
        retry_after: response.retry_after,
        error: Box::new(envelope.error),
    })
}

fn decode_empty_response(
    response: ApiHttpResponse,
    expected: &[StatusCode],
) -> ManagedApiResult<()> {
    if expected.contains(&response.status) {
        if response.body.is_empty() {
            return Ok(());
        }
        return invalid_response("managed API no-content response included a body");
    }
    let envelope: ApiErrorEnvelope = serde_json::from_slice(&response.body)
        .map_err(|source| ManagedApiError::Decode { source })?;
    validate_api_error(&envelope.error)?;
    Err(ManagedApiError::Service {
        status: response.status.as_u16(),
        retry_after: response.retry_after,
        error: Box::new(envelope.error),
    })
}

fn validate_api_error(error: &ApiError) -> ManagedApiResult<()> {
    if error.code.is_empty()
        || error.code.len() > 128
        || !error
            .code
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
        || error.message.is_empty()
        || error.message.len() > 4096
        || error.message.chars().any(char::is_control)
        || error.request_id.is_empty()
        || error.request_id.len() > 128
        || error.request_id.chars().any(char::is_control)
    {
        return Err(ManagedApiError::InvalidResponse {
            reason: "managed API error envelope contains invalid bounded fields".to_owned(),
        });
    }
    Ok(())
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;
