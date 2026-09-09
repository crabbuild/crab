use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crab_auth::managed::{IdempotencyKey, LogicalRepository, PageCursor, RepositoryState};
use crab_auth_store::{ManagedRepositoryError, ManagedRepositoryResolver};

use crate::{Client, Error, ErrorKind, OperationOptions, Result};

/// Managed profile and credential-cache selection.
#[derive(Clone)]
pub struct ManagedOptions {
    token_cache_directory: PathBuf,
    authority: Option<String>,
    bearer_token: Option<String>,
}

impl fmt::Debug for ManagedOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ManagedOptions")
            .field("token_cache_directory", &self.token_cache_directory)
            .field("authority", &self.authority)
            .field(
                "bearer_token",
                &self.bearer_token.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

impl ManagedOptions {
    /// Use the existing managed profile and encrypted token cache at this directory.
    pub fn new(token_cache_directory: impl AsRef<Path>) -> Result<Self> {
        let token_cache_directory = token_cache_directory.as_ref().to_owned();
        if !token_cache_directory.is_absolute() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "managed token-cache directory must be absolute",
            ));
        }
        Ok(Self {
            token_cache_directory,
            authority: None,
            bearer_token: None,
        })
    }

    /// Select an installed authority instead of the active profile.
    pub fn with_authority(mut self, authority: &str) -> Result<Self> {
        if authority.is_empty() || authority.contains(['/', '\\', '?', '#']) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "managed authority must be a bare host authority",
            ));
        }
        self.authority = Some(authority.to_owned());
        Ok(self)
    }

    /// Use one in-memory API bearer; it is redacted and never persisted.
    pub fn with_bearer_token(mut self, token: impl Into<String>) -> Result<Self> {
        let token = token.into();
        crab_auth::managed::BearerToken::new(token.clone()).map_err(managed_api_error)?;
        self.bearer_token = Some(token);
        Ok(self)
    }
}

#[derive(Clone)]
pub(crate) struct ManagedState {
    pub(crate) resolver: Arc<ManagedRepositoryResolver>,
    pub(crate) authority: Option<String>,
    pub(crate) cache_scope: String,
    #[cfg(feature = "local")]
    pub(crate) token_cache_directory: PathBuf,
    #[cfg(feature = "local")]
    pub(crate) local_compatible: bool,
}

impl ManagedState {
    pub(crate) fn new(options: ManagedOptions) -> Result<Self> {
        #[cfg(feature = "local")]
        let local_compatible = options.bearer_token.is_none();
        let token_cache_directory = options.token_cache_directory;
        let resolver = ManagedRepositoryResolver::new(token_cache_directory.clone())
            .with_bearer_token(options.bearer_token)
            .map_err(managed_repository_error)?;
        Ok(Self {
            resolver: Arc::new(resolver),
            authority: options.authority,
            cache_scope: uuid::Uuid::now_v7().simple().to_string(),
            #[cfg(feature = "local")]
            token_cache_directory,
            #[cfg(feature = "local")]
            local_compatible,
        })
    }
}

/// Managed repository lifecycle state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum ManagedRepositoryState {
    Provisioning,
    Active,
    Archived,
    SoftDeleted,
    Failed,
}

/// SDK-owned logical managed repository value without physical placement or credentials.
#[derive(Clone, Debug)]
pub struct ManagedRepository {
    id: String,
    organization_id: String,
    canonical_url: String,
    state: ManagedRepositoryState,
    revision: u64,
    protected_push: bool,
}

impl ManagedRepository {
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }
    #[must_use]
    pub fn organization_id(&self) -> &str {
        &self.organization_id
    }
    #[must_use]
    pub fn canonical_url(&self) -> &str {
        &self.canonical_url
    }
    #[must_use]
    pub const fn state(&self) -> ManagedRepositoryState {
        self.state
    }
    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.revision
    }
    #[must_use]
    pub const fn protected_push(&self) -> bool {
        self.protected_push
    }
}

/// One bounded managed repository page.
#[derive(Clone, Debug)]
pub struct ManagedPage {
    repositories: Vec<ManagedRepository>,
    next_cursor: Option<String>,
}

impl ManagedPage {
    #[must_use]
    pub fn repositories(&self) -> &[ManagedRepository] {
        &self.repositories
    }
    #[must_use]
    pub fn next_cursor(&self) -> Option<&str> {
        self.next_cursor.as_deref()
    }
}

/// Authenticated managed lifecycle API bound to one canonical authority.
#[derive(Clone)]
pub struct ManagedRepositories {
    client: Client,
    owner: Arc<crab_auth::managed::ManagedApiClient>,
}

impl Client {
    /// Connect to the selected managed service and validate discovery/capabilities.
    pub fn managed_repositories(
        &self,
    ) -> crate::Request<'_, ManagedRepositories, OperationOptions> {
        crate::Request::new(move |operation| {
            let client = self.clone();
            Box::pin(async move {
                let managed = client.0.managed.clone().ok_or_else(|| {
                    Error::new(
                        ErrorKind::InvalidInput,
                        "managed service options are required",
                    )
                })?;
                let result_client = client.clone();
                client
                    .0
                    .operations
                    .run(operation, move |cancel| async move {
                        let connection = managed
                            .resolver
                            .connect(managed.authority.as_deref(), &cancel)
                            .await
                            .map_err(managed_repository_error)?;
                        connection
                            .client
                            .capabilities()
                            .await
                            .map_err(managed_api_error)?;
                        Ok(ManagedRepositories {
                            client: result_client,
                            owner: Arc::new(connection.client),
                        })
                    })
                    .await
            })
        })
    }
}

impl ManagedRepositories {
    /// List one validated cursor page for an organization.
    pub async fn list(
        &self,
        organization: &str,
        cursor: Option<&str>,
        limit: u16,
        operation: OperationOptions,
    ) -> Result<ManagedPage> {
        let organization = organization.to_owned();
        let cursor = cursor
            .map(PageCursor::new)
            .transpose()
            .map_err(auth_contract_error)?;
        let owner = self.owner.clone();
        self.client
            .0
            .operations
            .run(operation, move |_| async move {
                let page = owner
                    .list_repositories(&organization, cursor.as_ref(), limit)
                    .await
                    .map_err(managed_api_error)?;
                Ok(ManagedPage {
                    repositories: page.repositories.into_iter().map(map_repository).collect(),
                    next_cursor: page.next_cursor.map(|cursor| cursor.as_str().to_owned()),
                })
            })
            .await
    }

    /// Create a repository using a caller-persisted idempotency key.
    pub async fn create(
        &self,
        organization: &str,
        repository: &str,
        idempotency_key: &str,
        operation: OperationOptions,
    ) -> Result<ManagedRepository> {
        let organization = organization.to_owned();
        let repository = repository.to_owned();
        let key = IdempotencyKey::new(idempotency_key).map_err(auth_contract_error)?;
        let owner = self.owner.clone();
        self.client
            .0
            .operations
            .run_mutation(operation, move |_| async move {
                owner
                    .create_repository(&organization, &repository, &key)
                    .await
                    .map(|entity| map_repository(entity.value))
                    .map_err(managed_api_error)
            })
            .await
    }

    /// Rename a repository using the caller's last observed positive revision.
    pub async fn rename(
        &self,
        organization: &str,
        repository: &str,
        slug: &str,
        revision: u64,
        idempotency_key: &str,
        operation: OperationOptions,
    ) -> Result<ManagedRepository> {
        let slug = slug.to_owned();
        self.update(
            organization,
            repository,
            revision,
            idempotency_key,
            operation,
            move |owner, organization, repository, etag, key| async move {
                owner
                    .rename_repository(&organization, &repository, &slug, &etag, &key)
                    .await
            },
        )
        .await
    }

    /// Archive a repository using the caller's last observed positive revision.
    pub async fn archive(
        &self,
        organization: &str,
        repository: &str,
        revision: u64,
        idempotency_key: &str,
        operation: OperationOptions,
    ) -> Result<ManagedRepository> {
        self.update(
            organization,
            repository,
            revision,
            idempotency_key,
            operation,
            |owner, organization, repository, etag, key| async move {
                owner
                    .archive_repository(&organization, &repository, &etag, &key)
                    .await
            },
        )
        .await
    }

    /// Restore a soft-deleted repository using its deletion revision.
    pub async fn restore(
        &self,
        organization: &str,
        repository: &str,
        revision: u64,
        idempotency_key: &str,
        operation: OperationOptions,
    ) -> Result<ManagedRepository> {
        self.update(
            organization,
            repository,
            revision,
            idempotency_key,
            operation,
            |owner, organization, repository, etag, key| async move {
                owner
                    .restore_repository(&organization, &repository, &etag, &key)
                    .await
            },
        )
        .await
    }

    async fn update<F, Fut>(
        &self,
        organization: &str,
        repository: &str,
        revision: u64,
        idempotency_key: &str,
        operation: OperationOptions,
        action: F,
    ) -> Result<ManagedRepository>
    where
        F: FnOnce(
                Arc<crab_auth::managed::ManagedApiClient>,
                String,
                String,
                crab_auth::managed::EntityTag,
                IdempotencyKey,
            ) -> Fut
            + Send
            + 'static,
        Fut: std::future::Future<
                Output = crab_auth::managed::ManagedApiResult<
                    crab_auth::managed::ManagedEntity<LogicalRepository>,
                >,
            > + Send,
    {
        let organization = organization.to_owned();
        let repository = repository.to_owned();
        let etag = revision_etag(revision)?;
        let key = IdempotencyKey::new(idempotency_key).map_err(auth_contract_error)?;
        let owner = self.owner.clone();
        self.client
            .0
            .operations
            .run_mutation(operation, move |_| async move {
                action(owner, organization, repository, etag, key)
                    .await
                    .map(|entity| map_repository(entity.value))
                    .map_err(managed_api_error)
            })
            .await
    }
}

fn revision_etag(revision: u64) -> Result<crab_auth::managed::EntityTag> {
    if revision == 0 {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "managed repository revision must be positive",
        ));
    }
    crab_auth::managed::EntityTag::new(format!("\"revision-{revision}\""))
        .map_err(auth_contract_error)
}

fn map_repository(repository: LogicalRepository) -> ManagedRepository {
    ManagedRepository {
        id: repository.repository_id.to_string(),
        organization_id: repository.organization_id.to_string(),
        canonical_url: repository.canonical_url,
        state: match repository.state {
            RepositoryState::Provisioning => ManagedRepositoryState::Provisioning,
            RepositoryState::Active => ManagedRepositoryState::Active,
            RepositoryState::Archived => ManagedRepositoryState::Archived,
            RepositoryState::SoftDeleted => ManagedRepositoryState::SoftDeleted,
            RepositoryState::Failed => ManagedRepositoryState::Failed,
        },
        revision: repository.revision,
        protected_push: repository.protected_push,
    }
}

fn auth_contract_error(source: crab_auth::error::AuthError) -> Error {
    Error::with_source(ErrorKind::InvalidInput, "invalid managed request", source)
}

fn managed_api_error(source: crab_auth::managed::ManagedApiError) -> Error {
    let kind = match &source {
        crab_auth::managed::ManagedApiError::Service { status: 401, .. } => {
            ErrorKind::Authentication
        }
        crab_auth::managed::ManagedApiError::Service { status: 403, .. } => {
            ErrorKind::Authorization
        }
        crab_auth::managed::ManagedApiError::Service { status: 404, .. } => ErrorKind::NotFound,
        crab_auth::managed::ManagedApiError::Service {
            status: 409 | 412, ..
        } => ErrorKind::Conflict,
        crab_auth::managed::ManagedApiError::InvalidRequest { .. } => ErrorKind::InvalidInput,
        crab_auth::managed::ManagedApiError::Transport { .. } => ErrorKind::Transport,
        _ => ErrorKind::Corruption,
    };
    Error::with_source(kind, "managed service request failed", source)
}

pub(crate) fn managed_repository_error(source: ManagedRepositoryError) -> Error {
    use crab_auth_store::ManagedRepositoryDiagnostic as Diagnostic;
    let kind = match source.diagnostic() {
        Diagnostic::MalformedLocator | Diagnostic::InvalidBearer => ErrorKind::InvalidInput,
        Diagnostic::LoginRequired { .. } | Diagnostic::MissingProfile { .. } => {
            ErrorKind::Authentication
        }
        Diagnostic::Forbidden { .. } => ErrorKind::Authorization,
        Diagnostic::NotFound { .. } => ErrorKind::NotFound,
        Diagnostic::ExpiredGrant { .. } => ErrorKind::Authentication,
        Diagnostic::Inactive { .. } => ErrorKind::Conflict,
        Diagnostic::Cancelled => ErrorKind::Cancelled,
        Diagnostic::ServiceUnavailable { .. } => ErrorKind::Transport,
        _ => ErrorKind::Corruption,
    };
    Error::with_source(kind, "managed repository resolution failed", source)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn options_reject_relative_cache_and_malformed_authority() {
        assert_eq!(
            ManagedOptions::new("relative").unwrap_err().kind(),
            ErrorKind::InvalidInput
        );
        let options = ManagedOptions::new(std::env::temp_dir()).unwrap();
        assert_eq!(
            options
                .with_authority("example.com/path")
                .unwrap_err()
                .kind(),
            ErrorKind::InvalidInput
        );
    }

    #[test]
    fn options_debug_redacts_bearer() {
        let options = ManagedOptions::new(std::env::temp_dir())
            .unwrap()
            .with_bearer_token("managed-secret")
            .unwrap();
        let debug = format!("{options:?}");
        assert!(debug.contains("[REDACTED]"));
        assert!(!debug.contains("managed-secret"));
    }

    #[test]
    fn repository_revisions_are_positive_concurrency_tokens() {
        assert_eq!(
            revision_etag(0).unwrap_err().kind(),
            ErrorKind::InvalidInput
        );
        assert!(revision_etag(7).is_ok());
    }

    #[test]
    fn expired_grants_are_retryable_authentication_failures() {
        let source = ManagedRepositoryError::Api {
            canonical_url: "crab://example.invalid/acme/repository".to_owned(),
            source: crab_auth::managed::ManagedApiError::ExpiredGrant,
        };
        assert_eq!(
            managed_repository_error(source).kind(),
            ErrorKind::Authentication
        );
    }
}
