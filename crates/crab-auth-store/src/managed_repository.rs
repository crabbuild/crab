use std::path::PathBuf;
use std::sync::Arc;

use crab_auth::cdn::CdnProvider;
use crab_auth::managed::{
    BearerToken, IdempotencyKey, ManagedApiClient, ManagedApiError, ManagedDiscoveryClient,
    PushAdmissionPlan, PushFinalizeRequest, PushPrepareRequest, PushPrepareResponse,
    PushReplicationRequest, RepositoryState, ServiceCapabilities, ServiceProfile,
    ServiceProfileLocatorError, ServiceProfileStore, TransferOperation, managed_http_client,
    service_profile_directory,
};
use crab_git::{ManagedRepository, RepositoryLocator};
use crab_storage::{
    StorageProviderKind,
    cdn::{CdnConsumer, CdnPolicy, CdnProfile, CdnVisibility},
};
use futures_util::FutureExt;
use tokio_util::sync::CancellationToken;

mod login;

use self::login::ManagedLogin;
use crate::{
    AuthStoreError, ManagedStore, build_push_store_from_transfer_grant,
    build_store_from_transfer_grant, managed_read::build_managed_read_store,
    refreshing_store::to_object_store_error,
};

/// A durable managed push session and its canonical-read/staging-write store.
pub struct ManagedPush {
    pub store: ManagedStore,
    pub prepared: PushPrepareResponse,
    pub request: PushPrepareRequest,
}

/// Authenticated managed control-plane connection and its canonical authority.
pub struct ManagedControlPlane {
    pub authority: String,
    pub client: ManagedApiClient,
}

/// Errors raised while resolving a logical managed repository to a store.
#[derive(Debug, thiserror::Error)]
pub enum ManagedRepositoryError {
    /// Locator classification or installed-profile lookup failed.
    #[error(transparent)]
    Locator(#[from] ServiceProfileLocatorError),
    /// Discovery, token-cache, or OIDC processing failed.
    #[error("managed authentication failed for {authority}: {source}")]
    Auth {
        authority: String,
        #[source]
        source: crab_auth::error::AuthError,
    },
    /// The managed control-plane request failed.
    #[error("managed API request failed for {canonical_url}: {source}")]
    Api {
        canonical_url: String,
        #[source]
        source: ManagedApiError,
    },
    /// The validated grant could not be composed into a storage handle.
    #[error("managed store construction failed for {canonical_url}: {source}")]
    Store {
        canonical_url: String,
        #[source]
        source: Box<AuthStoreError>,
    },
    /// An explicit in-memory bearer failed validation before a repository was known.
    #[error("invalid explicit managed API bearer: {source}")]
    Bearer {
        #[source]
        source: ManagedApiError,
    },
    /// No managed service profile is selected for control-plane commands.
    #[error("no active managed service profile; run `crab login https://crab.build`")]
    ActiveProfileMissing,
    /// The logical repository exists but cannot currently serve data.
    #[error("managed repository {canonical_url} is not active ({state:?})")]
    Inactive {
        canonical_url: String,
        state: RepositoryState,
    },
    /// Resolution was cancelled before a usable grant was returned.
    #[error("managed repository resolution cancelled")]
    Cancelled,
}

/// Redacted, actionable failure classes shared by all managed clients.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ManagedRepositoryDiagnostic {
    #[error(
        "malformed managed repository locator; expected crab://authority/organization/repository"
    )]
    MalformedLocator,
    #[error(
        "managed service profile for {authority} is missing; run `crab login https://{authority}`"
    )]
    MissingProfile { authority: String },
    #[error(
        "managed service profile for {authority} is invalid; run `crab logout {authority}` and log in again"
    )]
    InvalidProfile { authority: String },
    #[error(
        "managed service discovery failed for {authority}; verify https://{authority}/.well-known/crab and the profile trust settings"
    )]
    DiscoveryFailed { authority: String },
    #[error(
        "managed service API for {authority} is incompatible with this Crab client; upgrade Crab or the service"
    )]
    IncompatibleApi { authority: String },
    #[error("managed login is required for {authority}; run `crab login https://{authority}`")]
    LoginRequired { authority: String },
    #[error("explicit managed API bearer is invalid; provide a valid bearer or use `crab login`")]
    InvalidBearer,
    #[error("no active managed service profile; run `crab login https://crab.build`")]
    ActiveProfileMissing,
    #[error("managed repository was not found: {canonical_url}")]
    NotFound { canonical_url: String },
    #[error("managed repository access is forbidden: {canonical_url}")]
    Forbidden { canonical_url: String },
    #[error("managed transfer grant expired for {canonical_url}; retry the operation")]
    ExpiredGrant { canonical_url: String },
    #[error(
        "managed service {authority} is unavailable after retries; retry later or contact the service operator"
    )]
    ServiceUnavailable { authority: String },
    #[error(
        "managed service returned an invalid response for {canonical_url}; upgrade Crab or contact the service operator"
    )]
    InvalidServiceResponse { canonical_url: String },
    #[error("managed repository is not active ({state:?}): {canonical_url}")]
    Inactive {
        canonical_url: String,
        state: RepositoryState,
    },
    #[error("managed repository resolution was cancelled")]
    Cancelled,
}

impl ManagedRepositoryDiagnostic {
    /// Stable machine-readable class without any secret or placement data.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::MalformedLocator => "malformed_locator",
            Self::MissingProfile { .. } => "missing_profile",
            Self::InvalidProfile { .. } => "invalid_profile",
            Self::DiscoveryFailed { .. } => "discovery_failed",
            Self::IncompatibleApi { .. } => "incompatible_api",
            Self::LoginRequired { .. } => "login_required",
            Self::InvalidBearer => "invalid_bearer",
            Self::ActiveProfileMissing => "active_profile_missing",
            Self::NotFound { .. } => "not_found",
            Self::Forbidden { .. } => "forbidden",
            Self::ExpiredGrant { .. } => "expired_grant",
            Self::ServiceUnavailable { .. } => "service_unavailable",
            Self::InvalidServiceResponse { .. } => "invalid_service_response",
            Self::Inactive { .. } => "inactive",
            Self::Cancelled => "cancelled",
        }
    }
}

impl ManagedRepositoryError {
    /// Converts internal failures into a placement-free client diagnostic.
    #[must_use]
    pub fn diagnostic(&self) -> ManagedRepositoryDiagnostic {
        match self {
            Self::Locator(ServiceProfileLocatorError::Url(_)) => {
                ManagedRepositoryDiagnostic::MalformedLocator
            }
            Self::Locator(ServiceProfileLocatorError::Profile { authority, .. }) => {
                ManagedRepositoryDiagnostic::InvalidProfile {
                    authority: authority.clone(),
                }
            }
            Self::Bearer { .. } => ManagedRepositoryDiagnostic::InvalidBearer,
            Self::ActiveProfileMissing => ManagedRepositoryDiagnostic::ActiveProfileMissing,
            Self::Auth { authority, source } => diagnostic_for_auth(authority, source),
            Self::Api {
                canonical_url,
                source,
            } => diagnostic_for_api(canonical_url, source),
            Self::Store { canonical_url, .. } => {
                ManagedRepositoryDiagnostic::InvalidServiceResponse {
                    canonical_url: canonical_url.clone(),
                }
            }
            Self::Inactive {
                canonical_url,
                state,
            } => ManagedRepositoryDiagnostic::Inactive {
                canonical_url: canonical_url.clone(),
                state: *state,
            },
            Self::Cancelled => ManagedRepositoryDiagnostic::Cancelled,
        }
    }
}

/// Shared profile, discovery, token, API, and grant resolver for managed clients.
pub struct ManagedRepositoryResolver {
    token_cache_directory: PathBuf,
    bearer: Option<BearerToken>,
    cdn_consumer: Option<CdnConsumer>,
}

impl ManagedRepositoryResolver {
    /// Creates a resolver backed by the configured encrypted token-cache directory.
    #[must_use]
    pub fn new(token_cache_directory: PathBuf) -> Self {
        Self {
            token_cache_directory,
            bearer: None,
            cdn_consumer: None,
        }
    }

    /// Select the product's managed CDN qualification scope for read grants.
    #[must_use]
    pub fn with_cdn_consumer(mut self, consumer: CdnConsumer) -> Self {
        self.cdn_consumer = Some(consumer);
        self
    }

    /// Uses an explicit in-memory API bearer instead of the encrypted token cache.
    ///
    /// The bearer is never persisted and cannot be refreshed after rejection.
    pub fn with_bearer_token(
        mut self,
        token: Option<String>,
    ) -> Result<Self, ManagedRepositoryError> {
        self.bearer = token
            .map(BearerToken::new)
            .transpose()
            .map_err(|source| ManagedRepositoryError::Bearer { source })?;
        Ok(self)
    }

    /// Classifies a Crab URL using the same exact installed-profile set as resolution.
    pub fn classify(&self, url: &str) -> Result<RepositoryLocator, ManagedRepositoryError> {
        Ok(self.profile_store().classify_repository_url(url)?)
    }

    /// Connects to the selected managed control plane with refreshable authentication.
    pub async fn connect(
        &self,
        authority: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<ManagedControlPlane, ManagedRepositoryError> {
        cancelled(cancel)?;
        let profiles = self.profile_store();
        let profile = match authority {
            Some(authority) => profiles
                .load(authority)
                .map_err(|source| ManagedRepositoryError::Auth {
                    authority: authority.to_owned(),
                    source,
                })?
                .ok_or_else(|| ManagedRepositoryError::Auth {
                    authority: authority.to_owned(),
                    source: crab_auth::error::AuthError::ManagedProfileNotFound {
                        authority: authority.to_owned(),
                    },
                })?,
            None => profiles
                .active()
                .map_err(|source| ManagedRepositoryError::Auth {
                    authority: "active profile".to_owned(),
                    source,
                })?
                .ok_or(ManagedRepositoryError::ActiveProfileMissing)?,
        };
        self.connect_api(&profiles, &profile, cancel)
            .await
            .map(|(client, _, _)| ManagedControlPlane {
                authority: profile.authority,
                client,
            })
    }

    /// Resolves an active logical repository and returns its validated data store.
    pub async fn resolve(
        &self,
        repository: &ManagedRepository,
        operation: TransferOperation,
        cancel: &CancellationToken,
    ) -> Result<ManagedStore, ManagedRepositoryError> {
        let mut access = self.resolve_access(repository, cancel).await?;
        // A gateway-only repository keeps its v1 read even when the service
        // offers v2 direct transfers to other repositories.
        access
            .capabilities
            .transfer_modes
            .retain(|mode| access.logical.transfer_modes.contains(mode));
        // Capture the independently resolved placement for this operation and
        // for every renewal. A v2 response cannot select a different mode.
        access.logical.cdn_read_binding =
            access.logical.cdn_read_binding.take().filter(|binding| {
                self.cdn_consumer.is_some_and(|consumer| {
                    let provider = match binding.provider {
                        CdnProvider::Cloudfront => StorageProviderKind::S3,
                        CdnProvider::CloudCdn => StorageProviderKind::Gcs,
                        CdnProvider::FrontDoor => StorageProviderKind::Azure,
                    };
                    CdnPolicy {
                        edge: provider.into(),
                        consumer,
                        visibility: CdnVisibility::Private,
                        profile: CdnProfile::Managed,
                    }
                    .enabled()
                })
            });
        let grant = access
            .client
            .issue_transfer_grant(
                &repository.organization,
                &repository.repository,
                access.logical.repository_id,
                operation,
                &access.capabilities,
                access.logical.cdn_read_binding.as_ref(),
            )
            .await
            .map_err(|source| managed_api_error(repository, source))?;
        cancelled(cancel)?;
        let store_error = |source| ManagedRepositoryError::Store {
            canonical_url: repository.canonical_url(),
            source: Box::new(source),
        };
        if grant.schema_version != 2
            || !matches!(grant.transport, crab_auth::TransferTransport::Direct { .. })
        {
            return build_store_from_transfer_grant(&grant, access.gateway_client)
                .map_err(store_error);
        }
        let source = repository.clone();
        let refresh = move |access: Arc<ManagedAccess>| {
            let source = source.clone();
            async move {
                let renewed_client = match &access.login {
                    Some(login) => Some(login.connect(false).await.map_err(to_object_store_error)?),
                    None => None,
                };
                renewed_client
                    .as_ref()
                    .unwrap_or(&access.client)
                    .refresh_transfer_grant(
                        &source.organization,
                        &source.repository,
                        access.logical.repository_id,
                        operation,
                        &access.capabilities,
                        access.logical.cdn_read_binding.as_ref(),
                    )
                    .await
                    .map_err(to_object_store_error)
            }
            .boxed()
        };
        build_managed_read_store(Arc::new(access), grant, self.cdn_consumer, refresh)
            .map_err(store_error)
    }

    /// Prepares or resumes a durable push and composes its exact staging store.
    pub async fn prepare_push(
        &self,
        repository: &ManagedRepository,
        request: &PushPrepareRequest,
        idempotency_key: &IdempotencyKey,
        cancel: &CancellationToken,
    ) -> Result<ManagedPush, ManagedRepositoryError> {
        let access = self.resolve_access(repository, cancel).await?;
        if request.repository_id != access.logical.repository_id {
            return Err(managed_api_error(
                repository,
                ManagedApiError::InvalidRequest {
                    reason: "managed push repository ID does not match resolution".to_owned(),
                },
            ));
        }
        self.prepare_push_with_access(repository, access, request.clone(), idempotency_key, cancel)
            .await
    }

    /// Resolves the logical repository ID and prepares one protected push request.
    pub async fn prepare_push_for_updates(
        &self,
        repository: &ManagedRepository,
        ref_updates: Vec<crab_auth::PushRefUpdate>,
        plan: PushAdmissionPlan,
        client_version: String,
        replication: Option<PushReplicationRequest>,
        idempotency_key: &IdempotencyKey,
        cancel: &CancellationToken,
    ) -> Result<ManagedPush, ManagedRepositoryError> {
        let access = self.resolve_access(repository, cancel).await?;
        let request = PushPrepareRequest {
            schema_version: 1,
            repository_id: access.logical.repository_id,
            ref_updates,
            plan,
            client_version,
            replication,
        };
        self.prepare_push_with_access(repository, access, request, idempotency_key, cancel)
            .await
    }

    async fn prepare_push_with_access(
        &self,
        repository: &ManagedRepository,
        access: ManagedAccess,
        request: PushPrepareRequest,
        idempotency_key: &IdempotencyKey,
        cancel: &CancellationToken,
    ) -> Result<ManagedPush, ManagedRepositoryError> {
        let read_grant = access
            .client
            .issue_transfer_grant(
                &repository.organization,
                &repository.repository,
                access.logical.repository_id,
                TransferOperation::Fetch,
                &access.capabilities,
                None,
            )
            .await
            .map_err(|source| managed_api_error(repository, source))?;
        let prepared = access
            .client
            .prepare_push(
                &repository.organization,
                &repository.repository,
                &request,
                idempotency_key,
                &access.capabilities,
            )
            .await
            .map_err(|source| managed_api_error(repository, source))?;
        cancelled(cancel)?;
        let read = build_store_from_transfer_grant(&read_grant, access.gateway_client.clone())
            .map_err(|source| ManagedRepositoryError::Store {
                canonical_url: repository.canonical_url(),
                source: Box::new(source),
            })?;
        let store = build_push_store_from_transfer_grant(
            read,
            &prepared.staging_grant,
            access.gateway_client,
        )
        .map_err(|source| ManagedRepositoryError::Store {
            canonical_url: repository.canonical_url(),
            source: Box::new(source),
        })?;
        Ok(ManagedPush {
            store,
            prepared,
            request,
        })
    }

    /// Finalizes a durable push through its logical managed repository.
    pub async fn finalize_push(
        &self,
        repository: &ManagedRepository,
        push_id: uuid::Uuid,
        request: &PushFinalizeRequest,
        cancel: &CancellationToken,
    ) -> Result<crab_auth::PushFinalizeResponse, ManagedRepositoryError> {
        let access = self.resolve_access(repository, cancel).await?;
        if request.repository_id != access.logical.repository_id {
            return Err(managed_api_error(
                repository,
                ManagedApiError::InvalidRequest {
                    reason: "managed finalize repository ID does not match resolution".to_owned(),
                },
            ));
        }
        let response = access
            .client
            .finalize_push(
                &repository.organization,
                &repository.repository,
                push_id,
                request,
                &access.capabilities,
            )
            .await
            .map_err(|source| managed_api_error(repository, source))?;
        cancelled(cancel)?;
        Ok(response)
    }

    async fn resolve_access(
        &self,
        repository: &ManagedRepository,
        cancel: &CancellationToken,
    ) -> Result<ManagedAccess, ManagedRepositoryError> {
        let profile_store = self.profile_store();
        let profile = profile_store
            .load(&repository.authority)
            .map_err(|source| managed_auth_error(repository, source))?
            .ok_or_else(|| {
                managed_auth_error(
                    repository,
                    crab_auth::error::AuthError::ManagedProfileNotFound {
                        authority: repository.authority.clone(),
                    },
                )
            })?;
        let (client, capabilities, login) =
            self.connect_api(&profile_store, &profile, cancel).await?;
        cancelled(cancel)?;

        let logical = client
            .resolve_repository(&repository.organization, &repository.repository)
            .await
            .map_err(|source| managed_api_error(repository, source))?
            .value;
        if logical.state != RepositoryState::Active {
            return Err(ManagedRepositoryError::Inactive {
                canonical_url: repository.canonical_url(),
                state: logical.state,
            });
        }
        let gateway_client = managed_http_client(&profile)
            .map_err(|source| managed_auth_error(repository, source))?;
        Ok(ManagedAccess {
            client,
            capabilities,
            logical,
            gateway_client,
            login,
        })
    }

    async fn connect_api(
        &self,
        profiles: &ServiceProfileStore,
        profile: &ServiceProfile,
        cancel: &CancellationToken,
    ) -> Result<(ManagedApiClient, ServiceCapabilities, Option<ManagedLogin>), ManagedRepositoryError>
    {
        let authority = &profile.authority;
        let resource = format!("crab://{authority}");
        let resolved = ManagedDiscoveryClient::new()
            .resolve(profiles, authority)
            .await
            .map_err(|source| ManagedRepositoryError::Auth {
                authority: authority.clone(),
                source,
            })?;
        cancelled(cancel)?;
        let (bearer, login) = match self.bearer.as_ref() {
            Some(bearer) => (bearer.clone(), None),
            None => {
                let (login, bearer) = ManagedLogin::load(
                    self.token_cache_directory.clone(),
                    profile,
                    &resolved.document,
                )
                .await?;
                (bearer, Some(login))
            }
        };
        let mut client =
            ManagedApiClient::new(profile, &resolved.document, bearer).map_err(|source| {
                ManagedRepositoryError::Api {
                    canonical_url: resource.clone(),
                    source,
                }
            })?;
        let capabilities = match client.capabilities().await {
            Ok(capabilities) => capabilities,
            Err(error) if managed_unauthorized(&error) => {
                let Some(login) = login.as_ref() else {
                    return Err(ManagedRepositoryError::Api {
                        canonical_url: resource,
                        source: error,
                    });
                };
                client = login.connect(true).await?;
                client
                    .capabilities()
                    .await
                    .map_err(|source| ManagedRepositoryError::Api {
                        canonical_url: resource,
                        source,
                    })?
            }
            Err(error) => {
                return Err(ManagedRepositoryError::Api {
                    canonical_url: resource,
                    source: error,
                });
            }
        };
        cancelled(cancel)?;
        Ok((client, capabilities, login))
    }

    fn profile_store(&self) -> ServiceProfileStore {
        ServiceProfileStore::new(service_profile_directory(&self.token_cache_directory))
    }
}

struct ManagedAccess {
    client: ManagedApiClient,
    capabilities: ServiceCapabilities,
    logical: crab_auth::managed::LogicalRepository,
    gateway_client: reqwest::Client,
    login: Option<ManagedLogin>,
}

fn managed_unauthorized(error: &ManagedApiError) -> bool {
    matches!(error, ManagedApiError::Service { status: 401, .. })
}

fn cancelled(cancel: &CancellationToken) -> Result<(), ManagedRepositoryError> {
    if cancel.is_cancelled() {
        return Err(ManagedRepositoryError::Cancelled);
    }
    Ok(())
}

fn managed_auth_error(
    repository: &ManagedRepository,
    source: crab_auth::error::AuthError,
) -> ManagedRepositoryError {
    ManagedRepositoryError::Auth {
        authority: repository.authority.clone(),
        source,
    }
}

fn managed_api_error(
    repository: &ManagedRepository,
    source: ManagedApiError,
) -> ManagedRepositoryError {
    ManagedRepositoryError::Api {
        canonical_url: repository.canonical_url(),
        source,
    }
}

fn diagnostic_for_auth(
    authority: &str,
    source: &crab_auth::error::AuthError,
) -> ManagedRepositoryDiagnostic {
    use crab_auth::error::AuthError;

    match source {
        AuthError::ManagedProfileNotFound { .. } => ManagedRepositoryDiagnostic::MissingProfile {
            authority: authority.to_owned(),
        },
        AuthError::UnsupportedManagedApiVersion { .. } => {
            ManagedRepositoryDiagnostic::IncompatibleApi {
                authority: authority.to_owned(),
            }
        }
        AuthError::CredentialsExpired(_) | AuthError::OidcRefreshExpired { .. } => {
            ManagedRepositoryDiagnostic::LoginRequired {
                authority: authority.to_owned(),
            }
        }
        AuthError::ManagedDiscoveryRequest { .. }
        | AuthError::ManagedDiscoveryUnavailable { .. }
        | AuthError::OidcRequest { .. } => ManagedRepositoryDiagnostic::ServiceUnavailable {
            authority: authority.to_owned(),
        },
        AuthError::ManagedDiscoveryRejected { .. }
        | AuthError::ParseManagedDiscovery { .. }
        | AuthError::InvalidManagedContract(_) => ManagedRepositoryDiagnostic::DiscoveryFailed {
            authority: authority.to_owned(),
        },
        _ => ManagedRepositoryDiagnostic::LoginRequired {
            authority: authority.to_owned(),
        },
    }
}

fn diagnostic_for_api(
    canonical_url: &str,
    source: &ManagedApiError,
) -> ManagedRepositoryDiagnostic {
    let authority = canonical_url
        .strip_prefix("crab://")
        .and_then(|value| value.split('/').next())
        .unwrap_or("managed service")
        .to_owned();
    match source {
        ManagedApiError::Service { status: 401, .. } => {
            ManagedRepositoryDiagnostic::LoginRequired { authority }
        }
        ManagedApiError::Service { status: 403, .. } => ManagedRepositoryDiagnostic::Forbidden {
            canonical_url: canonical_url.to_owned(),
        },
        ManagedApiError::Service { status: 404, .. } => ManagedRepositoryDiagnostic::NotFound {
            canonical_url: canonical_url.to_owned(),
        },
        ManagedApiError::Transport { .. }
        | ManagedApiError::Service {
            status: 408 | 425 | 429 | 500..=599,
            ..
        } => ManagedRepositoryDiagnostic::ServiceUnavailable { authority },
        ManagedApiError::ExpiredGrant => ManagedRepositoryDiagnostic::ExpiredGrant {
            canonical_url: canonical_url.to_owned(),
        },
        ManagedApiError::Contract(crab_auth::error::AuthError::UnsupportedManagedApiVersion {
            ..
        }) => ManagedRepositoryDiagnostic::IncompatibleApi { authority },
        _ => ManagedRepositoryDiagnostic::InvalidServiceResponse {
            canonical_url: canonical_url.to_owned(),
        },
    }
}

#[cfg(test)]
#[expect(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use std::collections::BTreeMap;

    use crab_auth::{ApiError, ServiceProfileLocatorError};

    use super::*;

    fn service_error(status: u16) -> ManagedApiError {
        ManagedApiError::Service {
            status,
            retry_after: None,
            error: Box::new(ApiError {
                code: "test_error".to_owned(),
                message: "provider secret-bucket/private-prefix token-secret".to_owned(),
                request_id: "request-1".to_owned(),
                retryable: status >= 500,
                details: BTreeMap::new(),
            }),
        }
    }

    fn api_error(source: ManagedApiError) -> ManagedRepositoryError {
        ManagedRepositoryError::Api {
            canonical_url: "crab://crab.build/acme/models".to_owned(),
            source,
        }
    }

    #[test]
    fn diagnostic_classes_cover_managed_failure_contract() {
        let malformed = ManagedRepositoryError::Locator(ServiceProfileLocatorError::Url(
            crab_git::CrabUrl::parse("crab://bad").unwrap_err(),
        ));
        let cases = [
            (malformed.diagnostic(), "malformed_locator"),
            (
                ManagedRepositoryError::Locator(ServiceProfileLocatorError::Profile {
                    authority: "crab.build".to_owned(),
                    source: crab_auth::error::AuthError::InvalidManagedContract(
                        "secret-bucket/private-prefix".to_owned(),
                    ),
                })
                .diagnostic(),
                "invalid_profile",
            ),
            (
                ManagedRepositoryError::Auth {
                    authority: "crab.build".to_owned(),
                    source: crab_auth::error::AuthError::ManagedProfileNotFound {
                        authority: "crab.build".to_owned(),
                    },
                }
                .diagnostic(),
                "missing_profile",
            ),
            (
                ManagedRepositoryError::Auth {
                    authority: "crab.build".to_owned(),
                    source: crab_auth::error::AuthError::ManagedDiscoveryRejected {
                        endpoint: "https://crab.build/.well-known/crab".to_owned(),
                        status: 404,
                    },
                }
                .diagnostic(),
                "discovery_failed",
            ),
            (
                ManagedRepositoryError::Auth {
                    authority: "crab.build".to_owned(),
                    source: crab_auth::error::AuthError::UnsupportedManagedApiVersion {
                        supported: vec![1],
                        advertised: vec![2],
                    },
                }
                .diagnostic(),
                "incompatible_api",
            ),
            (
                ManagedRepositoryError::Auth {
                    authority: "crab.build".to_owned(),
                    source: crab_auth::error::AuthError::CredentialsExpired(
                        "token-secret".to_owned(),
                    ),
                }
                .diagnostic(),
                "login_required",
            ),
            (api_error(service_error(404)).diagnostic(), "not_found"),
            (api_error(service_error(403)).diagnostic(), "forbidden"),
            (
                api_error(ManagedApiError::ExpiredGrant).diagnostic(),
                "expired_grant",
            ),
            (
                api_error(service_error(503)).diagnostic(),
                "service_unavailable",
            ),
        ];

        for (diagnostic, expected) in cases {
            assert_eq!(diagnostic.kind(), expected);
        }
    }

    #[test]
    fn diagnostic_output_excludes_service_and_credential_secrets() {
        let diagnostic = api_error(service_error(403)).diagnostic();
        let rendered = format!("{diagnostic:?} {diagnostic}");

        assert!(rendered.contains("crab://crab.build/acme/models"));
        assert!(!rendered.contains("secret-bucket"));
        assert!(!rendered.contains("private-prefix"));
        assert!(!rendered.contains("token-secret"));
    }

    #[test]
    fn malformed_locator_diagnostic_does_not_echo_userinfo() {
        let source = crab_git::RepositoryLocator::parse(
            "crab://user:token-secret@crab.build/acme/models",
            |_| false,
        )
        .unwrap_err();
        let diagnostic =
            ManagedRepositoryError::Locator(ServiceProfileLocatorError::Url(source)).diagnostic();
        let rendered = format!("{diagnostic:?} {diagnostic}");

        assert_eq!(diagnostic.kind(), "malformed_locator");
        assert!(!rendered.contains("user"));
        assert!(!rendered.contains("token-secret"));
    }
}
