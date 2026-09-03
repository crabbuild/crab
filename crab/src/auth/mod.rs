//! Authentication and credential resolution for cloud object stores.
//!
//! This module adapts Crab's CLI auth config into credential providers and
//! composes resolved credentials with storage construction.

#[cfg(feature = "gix-credentials")]
pub mod gix_credentials_adapter;
mod managed;
pub mod oidc;

use std::sync::Arc;

use async_trait::async_trait;
use object_store::ObjectStore;
#[cfg(feature = "tier-s3")]
use object_store::aws::AwsCredential;
use tokio_util::sync::CancellationToken;

use crate::core::config::{AuthConfig, AuthProvider, Config};
use crate::core::error::{CrabError, Result};
use crate::storage::store::{BucketIdentity, Store};

pub use ::crab_auth::{
    AwsOidcConfig, AzureEntraConfig, AzureReadScope, AzureToken, CloudCredentials,
    CrabAuthClientConfig, CredentialProvider, CredentialProviderConfig, CredentialResolution,
    GcpWorkloadIdentityConfig, StaticAuthConfig,
};
use crab_auth_store::{
    AuthStoreError, ManagedRepositoryError, RefreshingObjectStore, RefreshingStoreParts,
};
use crab_storage::BuiltObjectStore;
#[cfg(feature = "tier-s3")]
use crab_types::storage::StorageProviderKind;
pub use crab_types::storage::StorageScope;
pub use managed::{RepositoryStore, build_repository_store};

#[cfg(feature = "tier-s3")]
#[derive(Debug)]
struct AwsSdkCredentialProvider {
    inner: aws_credential_types::provider::SharedCredentialsProvider,
}

#[cfg(feature = "tier-s3")]
#[async_trait]
impl object_store::CredentialProvider for AwsSdkCredentialProvider {
    type Credential = AwsCredential;

    async fn get_credential(&self) -> object_store::Result<Arc<Self::Credential>> {
        use aws_credential_types::provider::ProvideCredentials as _;

        let credentials = self.inner.provide_credentials().await.map_err(|source| {
            object_store::Error::Generic {
                store: "S3",
                source: Box::new(source),
            }
        })?;
        Ok(Arc::new(AwsCredential {
            key_id: credentials.access_key_id().to_owned(),
            secret_key: credentials.secret_access_key().to_owned(),
            token: credentials.session_token().map(str::to_owned),
        }))
    }
}

/// Create the appropriate [`CredentialProvider`] from config.
///
/// This is a dispatch function that reads `config.auth.provider` and
/// constructs the matching provider implementation. The CLI still owns
/// full `AuthConfig` parsing; extracted providers receive auth-domain DTOs.
pub fn create_provider(config: &Config) -> Result<Box<dyn CredentialProvider<Error = CrabError>>> {
    let provider =
        ::crab_auth::create_credential_provider(credential_provider_config(&config.auth)?)?;
    Ok(crab_error_provider(provider))
}

struct CrabErrorProvider<P> {
    inner: P,
}

// Extracted auth providers return auth-domain errors. Keep CLI error taxonomy
// at this dispatch seam so `crab-auth` does not learn about `CrabError`.
fn crab_error_provider(
    inner: Box<dyn CredentialProvider<Error = ::crab_auth::error::AuthError>>,
) -> Box<dyn CredentialProvider<Error = CrabError>> {
    Box::new(CrabErrorProvider { inner })
}

#[async_trait]
impl CredentialProvider
    for CrabErrorProvider<Box<dyn CredentialProvider<Error = ::crab_auth::error::AuthError>>>
{
    type Error = CrabError;

    async fn resolve(
        &self,
        bucket: &str,
        prefix: &str,
        operation: &str,
    ) -> Result<CredentialResolution> {
        self.inner
            .resolve(bucket, prefix, operation)
            .await
            .map_err(CrabError::from)
    }

    fn needs_refresh(&self) -> bool {
        self.inner.needs_refresh()
    }

    async fn refresh(&self) -> Result<CredentialResolution> {
        self.inner.refresh().await.map_err(CrabError::from)
    }

    async fn refresh_for(
        &self,
        bucket: &str,
        prefix: &str,
        operation: &str,
    ) -> Result<CredentialResolution> {
        self.inner
            .refresh_for(bucket, prefix, operation)
            .await
            .map_err(CrabError::from)
    }

    fn identity(&self) -> Option<&str> {
        self.inner.identity()
    }
}

fn credential_provider_config(auth: &AuthConfig) -> Result<CredentialProviderConfig> {
    match auth.provider {
        AuthProvider::AwsOidc => Ok(CredentialProviderConfig::AwsOidc(aws_oidc_config(auth)?)),
        AuthProvider::GcpWorkloadIdentity => Ok(CredentialProviderConfig::GcpWorkloadIdentity(
            gcp_workload_identity_config(auth)?,
        )),
        AuthProvider::AzureEntra => Ok(CredentialProviderConfig::AzureEntra(azure_entra_config(
            auth,
        )?)),
        AuthProvider::CrabAuth => Ok(CredentialProviderConfig::CrabAuth(crab_auth_client_config(
            auth,
        )?)),
        AuthProvider::Static => Ok(CredentialProviderConfig::Static(static_auth_config(auth)?)),
        AuthProvider::None => Ok(CredentialProviderConfig::None(static_auth_config(auth)?)),
    }
}

fn static_auth_config(auth: &AuthConfig) -> Result<StaticAuthConfig> {
    let storage_provider = auth
        .storage_provider
        .storage_provider_kind()
        .map_or_else(crab_storage::resolve_static_env_provider, Ok)?;
    Ok(StaticAuthConfig { storage_provider })
}

fn aws_oidc_config(auth: &AuthConfig) -> Result<AwsOidcConfig> {
    Ok(AwsOidcConfig {
        role_arn: required(
            auth.aws.role_arn.as_deref(),
            "auth.aws.role_arn",
            "aws-oidc provider requires a role_arn",
        )?,
        region: auth
            .aws
            .region
            .as_deref()
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .or_else(|| std::env::var("AWS_REGION").ok().filter(|s| !s.is_empty()))
            .unwrap_or_else(|| "us-east-1".into()),
        session_duration_secs: auth.aws.session_duration_secs,
        issuer_url: required(
            auth.issuer_url.as_deref(),
            "auth.issuer_url",
            "aws-oidc provider requires an issuer_url",
        )?,
        client_id: required(
            auth.client_id.as_deref(),
            "auth.client_id",
            "aws-oidc provider requires a client_id",
        )?,
        token_cache_path: auth.token_cache_path.clone(),
    })
}

fn gcp_workload_identity_config(auth: &AuthConfig) -> Result<GcpWorkloadIdentityConfig> {
    Ok(GcpWorkloadIdentityConfig {
        workload_identity_pool: required(
            auth.gcp.workload_identity_pool.as_deref(),
            "auth.gcp.workload_identity_pool",
            "gcp-workload-identity provider requires a workload_identity_pool",
        )?,
        service_account: required(
            auth.gcp.service_account.as_deref(),
            "auth.gcp.service_account",
            "gcp-workload-identity provider requires a service_account",
        )?,
        issuer_url: required(
            auth.issuer_url.as_deref(),
            "auth.issuer_url",
            "gcp-workload-identity provider requires an issuer_url",
        )?,
        client_id: required(
            auth.client_id.as_deref(),
            "auth.client_id",
            "gcp-workload-identity provider requires a client_id",
        )?,
        token_cache_path: auth.token_cache_path.clone(),
    })
}

fn azure_entra_config(auth: &AuthConfig) -> Result<AzureEntraConfig> {
    Ok(AzureEntraConfig {
        tenant_id: required(
            auth.azure.tenant_id.as_deref(),
            "auth.azure.tenant_id",
            "azure-entra provider requires a tenant_id",
        )?,
        auth_endpoint: optional(auth.auth_endpoint.as_deref()),
        storage_account: optional(auth.azure.storage_account.as_deref()),
        issuer_url: required(
            auth.issuer_url.as_deref(),
            "auth.issuer_url",
            "azure-entra provider requires an issuer_url",
        )?,
        client_id: required(
            auth.client_id.as_deref(),
            "auth.client_id",
            "azure-entra provider requires a client_id",
        )?,
        token_cache_path: auth.token_cache_path.clone(),
    })
}

pub(crate) fn crab_auth_client_config(auth: &AuthConfig) -> Result<CrabAuthClientConfig> {
    Ok(CrabAuthClientConfig {
        endpoint: required(
            auth.auth_endpoint.as_deref(),
            "auth.auth_endpoint",
            "crab-auth provider requires an auth_endpoint",
        )?,
        issuer_url: required(
            auth.issuer_url.as_deref(),
            "auth.issuer_url",
            "crab-auth provider requires an issuer_url",
        )?,
        client_id: required(
            auth.client_id.as_deref(),
            "auth.client_id",
            "crab-auth provider requires a client_id",
        )?,
        token_cache_path: auth.token_cache_path.clone(),
        client_version: env!("CARGO_PKG_VERSION").to_owned(),
    })
}

fn required(value: Option<&str>, key: &str, origin: &str) -> Result<String> {
    value
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| CrabError::Configuration {
            key: key.into(),
            origin: origin.into(),
        })
}

fn optional(value: Option<&str>) -> Option<String> {
    value.filter(|s| !s.is_empty()).map(str::to_owned)
}

/// Build an `ObjectStore`-backed [`Store`] from config and URL.
///
/// Single entry point for all store construction. Resolves credentials
/// via the configured auth provider, constructs the cloud-specific
/// `ObjectStore` implementation, and wraps it in a `Store`.
pub async fn build_store(
    config: &Config,
    url: impl Into<crab_git::url::CrabUrl>,
    operation: &str,
    cancel: &CancellationToken,
) -> Result<Store> {
    if cancel.is_cancelled() {
        return Err(CrabError::Cancelled);
    }

    let url = url.into();
    if url.bucket.eq_ignore_ascii_case("crab.build") {
        let (organization, repository) = url
            .repo_path
            .split_once('/')
            .unwrap_or((url.repo_path.as_str(), ""));
        return Err(CrabError::from(
            crab_git::UrlError::ManagedServiceNotEnabled {
                authority: "crab.build".to_owned(),
                organization: organization.to_owned(),
                repository: repository.to_owned(),
            },
        ));
    }
    let aws_sdk_store = build_aws_sdk_store(config, &url.bucket).await?;
    let (storage_scope, credential_provider, mut built) = if let Some(built) = aws_sdk_store {
        (None, None, built)
    } else {
        let provider = create_provider(config)?;
        let resolution = provider
            .resolve(&url.bucket, &url.repo_path, operation)
            .await?;
        let storage_scope = resolution.storage_scope.clone();
        let credential_provider: Option<Arc<dyn CredentialProvider<Error = CrabError>>> =
            if !config.auth.provider.uses_token_cache() || storage_scope.is_some() {
                // View credentials and StoreLayout prefixes are a single effective scope.
                // Refreshing one without rebuilding the other can only fail closed.
                None
            } else {
                Some(Arc::from(provider))
            };
        let built = build_object_store(&url.bucket, resolution.credentials)?;
        (storage_scope, credential_provider, built)
    };

    // Multipart recovery has a stricter endpoint-bound identity; retain the
    // existing bucket identity used for provider selection and safety rails.
    let identity = BucketIdentity::new(built.provider, url.bucket.as_str(), url.bucket.as_str());
    let multipart_identity = built.multipart_identity.clone();

    if let Some(cp) = credential_provider.as_ref() {
        let bucket = url.bucket.clone();
        let builder_bucket = bucket.clone();
        let builder = Arc::new(move |resolution: CredentialResolution| {
            crab_auth_store::build_object_store_from_credentials(
                &builder_bucket,
                resolution.credentials,
            )
            .map(|built| RefreshingStoreParts {
                inner: built.inner,
                signer: built.signer,
                multipart: built.multipart,
                multipart_identity: built.multipart_identity,
                target_identity: built.target_identity,
            })
        });
        let refreshing = Arc::new(RefreshingObjectStore::new(
            Arc::clone(cp),
            bucket,
            url.repo_path.clone(),
            operation.to_owned(),
            RefreshingStoreParts {
                inner: built.inner,
                signer: built.signer,
                multipart: built.multipart,
                multipart_identity: built.multipart_identity,
                target_identity: built.target_identity,
            },
            builder,
        ));
        built.inner = refreshing.clone() as Arc<dyn ObjectStore>;
        built.signer = if refreshing.has_signer().await {
            Some(refreshing.clone() as Arc<dyn object_store::signer::Signer>)
        } else {
            None
        };
        built.multipart = if refreshing.has_multipart().await {
            Some(refreshing as Arc<dyn object_store::multipart::MultipartStore>)
        } else {
            None
        };
    }

    let mut store = Store::new(built.inner)
        .with_bucket_identity(identity)
        .with_target_identity(built.target_identity);
    if let Some(s) = built.signer {
        store = store.with_signer(s);
    }
    if let (Some(multipart), Some(identity)) = (built.multipart, multipart_identity) {
        store = store.with_multipart(multipart, identity);
    }
    if let Some(scope) = storage_scope {
        store = store.with_storage_scope(scope);
    }
    Ok(store)
}

#[cfg(feature = "tier-s3")]
async fn build_aws_sdk_store(config: &Config, bucket: &str) -> Result<Option<BuiltObjectStore>> {
    if !should_build_aws_sdk_store(config)? {
        return Ok(None);
    }

    let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest());
    if let Some(profile) = config.auth.aws.profile.as_deref() {
        loader = loader.profile_name(profile);
    }
    let sdk_config = loader.load().await;
    let credentials = sdk_config
        .credentials_provider()
        .ok_or(CrabError::NoCredentials)?;
    let region = config
        .auth
        .aws
        .region
        .clone()
        .or_else(|| sdk_config.region().map(|region| region.as_ref().to_owned()))
        .unwrap_or_else(|| "us-east-1".to_owned());
    let provider = Arc::new(AwsSdkCredentialProvider { inner: credentials });
    crab_storage::build_s3_object_store_with_provider(bucket, &region, provider)
        .map(Some)
        .map_err(CrabError::from)
}

#[cfg(feature = "tier-s3")]
fn should_build_aws_sdk_store(config: &Config) -> Result<bool> {
    if config.auth.provider != AuthProvider::Static {
        return Ok(false);
    }
    Ok(static_auth_config(&config.auth)?.storage_provider == StorageProviderKind::S3)
}

#[cfg(not(feature = "tier-s3"))]
async fn build_aws_sdk_store(config: &Config, _bucket: &str) -> Result<Option<BuiltObjectStore>> {
    if config.auth.aws.profile.is_some() {
        return Err(CrabError::Configuration {
            key: "auth.aws_profile".to_owned(),
            origin: "this Crab build does not include AWS shared-profile support".to_owned(),
        });
    }
    Ok(None)
}

/// Build and validate the store for one direct canonical-v1 repository URL.
///
/// Bucket-level and arbitrary object-source callers must continue to use
/// [`build_store`]; this boundary rejects missing or non-v1 repository state
/// before a repository command can read or mutate metadata.
pub async fn build_repository_url_store(
    config: &Config,
    url: impl Into<crab_git::url::CrabUrl>,
    operation: &str,
    cancel: &CancellationToken,
) -> Result<Store> {
    let url = url.into();
    let repository_prefix = url.repo_path.clone();
    let store = build_store(config, url, operation, cancel).await?;
    validate_repository_store(&store, &repository_prefix).await?;
    Ok(store)
}

pub(crate) async fn validate_repository_store(
    store: &Store,
    repository_prefix: &str,
) -> Result<()> {
    let router = crate::storage::StoreLayout::new(store.clone(), repository_prefix.to_owned());
    crate::core::remote_layout::open(store, &router).await?;
    Ok(())
}

pub fn build_store_from_credentials(bucket: &str, creds: CloudCredentials) -> Result<Store> {
    crab_auth_store::build_store_from_credentials(bucket, creds)
        .map(Store::from_storage)
        .map_err(CrabError::from)
}

pub fn build_protected_push_store(
    bucket: &str,
    creds: CloudCredentials,
    upload_prefix: &str,
) -> Result<Store> {
    crab_auth_store::build_protected_push_store(bucket, creds, upload_prefix)
        .map(Store::from_storage)
        .map_err(CrabError::from)
}

fn build_object_store(bucket: &str, creds: CloudCredentials) -> Result<BuiltObjectStore> {
    crab_auth_store::build_object_store_from_credentials(bucket, creds).map_err(CrabError::from)
}

impl From<AuthStoreError> for CrabError {
    fn from(error: AuthStoreError) -> Self {
        match error {
            AuthStoreError::Storage(source) => Self::from(source),
            AuthStoreError::InvalidCredentials { reason } => Self::Configuration {
                key: reason,
                origin: "storage credentials".into(),
            },
            AuthStoreError::AuthFailed { path } => Self::AuthFailed { path },
        }
    }
}

impl From<ManagedRepositoryError> for CrabError {
    fn from(error: ManagedRepositoryError) -> Self {
        let diagnostic = error.diagnostic();
        if diagnostic == crab_auth_store::ManagedRepositoryDiagnostic::Cancelled {
            return Self::Cancelled;
        }
        Self::ManagedRepository { diagnostic }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]
mod tests {
    use super::*;
    use crate::core::config::{AuthConfig, AuthProvider, Config, StorageProvider};
    use crate::storage::StoreLayout;
    use crab_types::storage::StorageProviderKind;
    use object_store::memory::InMemory;
    use std::sync::{LazyLock, Mutex, MutexGuard};

    static ENV_MUTEX: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

    #[tokio::test]
    async fn repository_validation_requires_descriptor_without_creating_state() {
        let store = Store::new(Arc::new(InMemory::new()));
        let router = StoreLayout::new(store.clone(), "org/repo".to_owned());

        let error = validate_repository_store(&store, "org/repo")
            .await
            .expect_err("descriptor-less repository must fail closed");

        assert!(error.to_string().contains("layout descriptor is missing"));
        assert!(store.head(&router.manifest_path()).await.is_err());
        assert!(store.head(&router.layout_descriptor_path()).await.is_err());
    }

    #[tokio::test]
    async fn repository_validation_accepts_only_initialized_canonical_v1() {
        let store = Store::new(Arc::new(InMemory::new()));
        let router = StoreLayout::new(store.clone(), "org/repo".to_owned());
        crate::core::remote_layout::initialize(&store, &router)
            .await
            .expect("initialize canonical descriptor");

        validate_repository_store(&store, "org/repo")
            .await
            .expect("canonical descriptor should open");
    }

    /// Helper: build a `Config` with the given auth provider and storage provider.
    fn config_with(provider: AuthProvider, storage: StorageProvider) -> Config {
        Config {
            auth: AuthConfig {
                provider,
                storage_provider: storage,
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// Helper: build a `Config` with full OIDC fields populated so that
    /// cloud-specific providers can construct without missing-field errors.
    fn oidc_config(provider: AuthProvider) -> Config {
        let mut cfg = Config::default();
        cfg.auth.provider = provider;
        cfg.auth.issuer_url = Some("https://idp.example.com".into());
        cfg.auth.client_id = Some("crab-cli".into());
        cfg.auth.auth_endpoint = Some("https://vend.example.com/v1/creds".into());
        cfg.auth.aws.role_arn = Some("arn:aws:iam::123456789012:role/test".into());
        cfg.auth.gcp.workload_identity_pool =
            Some("projects/123/locations/global/workloadIdentityPools/pool/providers/idp".into());
        cfg.auth.gcp.service_account = Some("sa@proj.iam.gserviceaccount.com".into());
        cfg.auth.azure.tenant_id = Some("00000000-0000-0000-0000-000000000000".into());
        cfg
    }

    // --- create_provider dispatch tests ---

    #[test]
    fn create_provider_static_succeeds() {
        let cfg = config_with(AuthProvider::Static, StorageProvider::S3);
        let provider = create_provider(&cfg).unwrap();
        assert!(provider.identity().is_none());
        assert!(!provider.needs_refresh());
    }

    #[test]
    fn create_provider_none_succeeds() {
        let cfg = config_with(AuthProvider::None, StorageProvider::Auto);
        let provider = create_provider(&cfg).unwrap();
        assert!(provider.identity().is_none());
    }

    #[test]
    fn create_provider_aws_oidc_missing_role_arn_fails() {
        let mut cfg = Config::default();
        cfg.auth.provider = AuthProvider::AwsOidc;
        cfg.auth.issuer_url = Some("https://idp.example.com".into());
        cfg.auth.client_id = Some("cli".into());
        // role_arn is None — should fail
        let result = create_provider(&cfg);
        assert!(result.is_err(), "missing role_arn should fail");
        let err = result.err().unwrap();
        assert!(
            matches!(err, CrabError::Configuration { ref key, .. } if key.contains("role_arn")),
            "expected Configuration error about role_arn, got {err:?}"
        );
    }

    #[test]
    fn create_provider_aws_oidc_with_all_fields_succeeds() {
        let cfg = oidc_config(AuthProvider::AwsOidc);
        let provider = create_provider(&cfg).unwrap();
        // AWS OIDC provider has no identity until tokens are resolved.
        assert!(provider.identity().is_none());
    }

    #[test]
    fn aws_oidc_config_prefers_configured_region_over_env() {
        let _guard = EnvGuard::set("AWS_REGION", Some("eu-west-1"));
        let mut cfg = oidc_config(AuthProvider::AwsOidc);
        cfg.auth.aws.region = Some("ap-southeast-1".into());

        let auth_config = aws_oidc_config(&cfg.auth).unwrap();

        assert_eq!(auth_config.region, "ap-southeast-1");
    }

    #[test]
    fn aws_oidc_config_uses_env_region_then_default() {
        let _guard = EnvGuard::set("AWS_REGION", Some("eu-central-1"));
        let cfg = oidc_config(AuthProvider::AwsOidc);

        let auth_config = aws_oidc_config(&cfg.auth).unwrap();

        assert_eq!(auth_config.region, "eu-central-1");

        _guard.update(None);

        let auth_config = aws_oidc_config(&cfg.auth).unwrap();

        assert_eq!(auth_config.region, "us-east-1");
    }

    #[test]
    fn create_provider_gcp_missing_pool_fails() {
        let mut cfg = Config::default();
        cfg.auth.provider = AuthProvider::GcpWorkloadIdentity;
        cfg.auth.issuer_url = Some("https://idp.example.com".into());
        cfg.auth.client_id = Some("cli".into());
        cfg.auth.gcp.service_account = Some("sa@proj.iam.gserviceaccount.com".into());
        // workload_identity_pool is None — should fail
        let result = create_provider(&cfg);
        assert!(result.is_err(), "missing pool should fail");
        let err = result.err().unwrap();
        assert!(
            matches!(err, CrabError::Configuration { ref key, .. } if key.contains("workload_identity_pool")),
            "expected Configuration error about workload_identity_pool, got {err:?}"
        );
    }

    #[test]
    fn create_provider_gcp_with_all_fields_succeeds() {
        let cfg = oidc_config(AuthProvider::GcpWorkloadIdentity);
        let provider = create_provider(&cfg).unwrap();
        assert!(provider.identity().is_none());
    }

    #[test]
    fn gcp_workload_identity_config_requires_service_account() {
        let mut cfg = oidc_config(AuthProvider::GcpWorkloadIdentity);
        cfg.auth.gcp.service_account = None;

        let err = gcp_workload_identity_config(&cfg.auth).unwrap_err();

        assert!(
            matches!(err, CrabError::Configuration { ref key, .. } if key.contains("service_account")),
            "expected Configuration error about service_account, got {err:?}"
        );
    }

    #[test]
    fn gcp_workload_identity_config_requires_issuer_url() {
        let mut cfg = oidc_config(AuthProvider::GcpWorkloadIdentity);
        cfg.auth.issuer_url = None;

        let err = gcp_workload_identity_config(&cfg.auth).unwrap_err();

        assert!(
            matches!(err, CrabError::Configuration { ref key, .. } if key.contains("issuer_url")),
            "expected Configuration error about issuer_url, got {err:?}"
        );
    }

    #[test]
    fn gcp_workload_identity_config_requires_client_id() {
        let mut cfg = oidc_config(AuthProvider::GcpWorkloadIdentity);
        cfg.auth.client_id = None;

        let err = gcp_workload_identity_config(&cfg.auth).unwrap_err();

        assert!(
            matches!(err, CrabError::Configuration { ref key, .. } if key.contains("client_id")),
            "expected Configuration error about client_id, got {err:?}"
        );
    }

    #[test]
    fn create_provider_azure_missing_tenant_fails() {
        let mut cfg = Config::default();
        cfg.auth.provider = AuthProvider::AzureEntra;
        cfg.auth.issuer_url = Some("https://idp.example.com".into());
        cfg.auth.client_id = Some("cli".into());
        // tenant_id is None — should fail
        let result = create_provider(&cfg);
        assert!(result.is_err(), "missing tenant_id should fail");
        let err = result.err().unwrap();
        assert!(
            matches!(err, CrabError::Configuration { ref key, .. } if key.contains("tenant_id")),
            "expected Configuration error about tenant_id, got {err:?}"
        );
    }

    #[test]
    fn create_provider_azure_with_all_fields_succeeds() {
        let cfg = oidc_config(AuthProvider::AzureEntra);
        let provider = create_provider(&cfg).unwrap();
        assert!(provider.identity().is_none());
    }

    #[test]
    fn azure_entra_config_requires_issuer_url() {
        let mut cfg = oidc_config(AuthProvider::AzureEntra);
        cfg.auth.issuer_url = None;

        let err = azure_entra_config(&cfg.auth).unwrap_err();

        assert!(
            matches!(err, CrabError::Configuration { ref key, .. } if key.contains("issuer_url")),
            "expected Configuration error about issuer_url, got {err:?}"
        );
    }

    #[test]
    fn azure_entra_config_requires_client_id() {
        let mut cfg = oidc_config(AuthProvider::AzureEntra);
        cfg.auth.client_id = None;

        let err = azure_entra_config(&cfg.auth).unwrap_err();

        assert!(
            matches!(err, CrabError::Configuration { ref key, .. } if key.contains("client_id")),
            "expected Configuration error about client_id, got {err:?}"
        );
    }

    #[test]
    fn azure_entra_config_preserves_optional_endpoint_and_account() {
        let mut cfg = oidc_config(AuthProvider::AzureEntra);
        cfg.auth.auth_endpoint = Some("https://crab-auth.example.com/v1/azure".into());
        cfg.auth.azure.storage_account = Some("acct".into());

        let auth_config = azure_entra_config(&cfg.auth).unwrap();

        assert_eq!(
            auth_config.auth_endpoint.as_deref(),
            Some("https://crab-auth.example.com/v1/azure")
        );
        assert_eq!(auth_config.storage_account.as_deref(), Some("acct"));
    }

    #[test]
    fn create_provider_crab_auth_missing_endpoint_fails() {
        let mut cfg = Config::default();
        cfg.auth.provider = AuthProvider::CrabAuth;
        cfg.auth.issuer_url = Some("https://idp.example.com".into());
        cfg.auth.client_id = Some("cli".into());
        // auth_endpoint is None — should fail
        let result = create_provider(&cfg);
        assert!(result.is_err(), "missing auth_endpoint should fail");
        let err = result.err().unwrap();
        assert!(
            matches!(err, CrabError::Configuration { ref key, .. } if key.contains("auth_endpoint")),
            "expected Configuration error about auth_endpoint, got {err:?}"
        );
    }

    #[test]
    fn create_provider_crab_auth_with_all_fields_succeeds() {
        let cfg = oidc_config(AuthProvider::CrabAuth);
        let provider = create_provider(&cfg).unwrap();
        assert!(provider.identity().is_none());
    }

    // --- StaticProvider resolved storage provider tests ---

    #[tokio::test]
    async fn static_provider_resolves_s3() {
        let cfg = config_with(AuthProvider::Static, StorageProvider::S3);
        let provider = create_provider(&cfg).unwrap();
        let creds = provider.resolve("bucket", "prefix", "fetch").await.unwrap();
        match creds.credentials {
            CloudCredentials::StaticEnv { provider } => {
                assert_eq!(provider, StorageProviderKind::S3);
            }
            _ => panic!("expected StaticEnv, got {creds:?}"),
        }
    }

    #[tokio::test]
    async fn static_provider_resolves_gcs() {
        let cfg = config_with(AuthProvider::Static, StorageProvider::Gcs);
        let provider = create_provider(&cfg).unwrap();
        let creds = provider.resolve("bucket", "prefix", "push").await.unwrap();
        match creds.credentials {
            CloudCredentials::StaticEnv { provider } => {
                assert_eq!(provider, StorageProviderKind::Gcs);
            }
            _ => panic!("expected StaticEnv, got {creds:?}"),
        }
    }

    #[tokio::test]
    async fn static_provider_resolves_azure() {
        let cfg = config_with(AuthProvider::Static, StorageProvider::Azure);
        let provider = create_provider(&cfg).unwrap();
        let creds = provider.resolve("bucket", "prefix", "clone").await.unwrap();
        match creds.credentials {
            CloudCredentials::StaticEnv { provider } => {
                assert_eq!(provider, StorageProviderKind::Azure);
            }
            _ => panic!("expected StaticEnv, got {creds:?}"),
        }
    }

    // Env-var-dependent Auto tests are combined into a single serial
    // test to avoid races with other threads mutating the same var.
    #[tokio::test]
    async fn static_provider_auto_env_detection() {
        // Sub-test 1: no env var → defaults to S3.
        let _guard = EnvGuard::set("CRAB_STORAGE_PROVIDER", None);
        let cfg = config_with(AuthProvider::Static, StorageProvider::Auto);
        let provider = create_provider(&cfg).unwrap();
        let creds = provider.resolve("bucket", "prefix", "fetch").await.unwrap();
        match creds.credentials {
            CloudCredentials::StaticEnv { provider } => {
                assert_eq!(
                    provider,
                    StorageProviderKind::S3,
                    "Auto with no env should default to S3"
                );
            }
            _ => panic!("expected StaticEnv S3, got {creds:?}"),
        }

        // Sub-test 2: env var set to "gcs" → resolves to Gcs.
        _guard.update(Some("gcs"));
        let cfg = config_with(AuthProvider::Static, StorageProvider::Auto);
        let provider = create_provider(&cfg).unwrap();
        let creds = provider.resolve("bucket", "prefix", "fetch").await.unwrap();
        match creds.credentials {
            CloudCredentials::StaticEnv { provider } => {
                assert_eq!(
                    provider,
                    StorageProviderKind::Gcs,
                    "Auto with CRAB_STORAGE_PROVIDER=gcs should resolve to Gcs"
                );
            }
            _ => panic!("expected StaticEnv Gcs, got {creds:?}"),
        }

        // Sub-test 3: explicit invalid env values fail closed.
        _guard.update(Some("auto"));
        let cfg = config_with(AuthProvider::Static, StorageProvider::Auto);
        assert!(matches!(
            create_provider(&cfg),
            Err(CrabError::Configuration { .. })
        ));
    }

    #[cfg(feature = "tier-s3")]
    #[test]
    fn aws_sdk_store_selection_uses_resolved_static_provider() {
        let _guard = EnvGuard::set("CRAB_STORAGE_PROVIDER", None);
        let auto = config_with(AuthProvider::Static, StorageProvider::Auto);
        assert!(should_build_aws_sdk_store(&auto).unwrap());

        _guard.update(Some("gcs"));
        assert!(!should_build_aws_sdk_store(&auto).unwrap());

        let s3 = config_with(AuthProvider::Static, StorageProvider::S3);
        assert!(should_build_aws_sdk_store(&s3).unwrap());

        let no_auth = config_with(AuthProvider::None, StorageProvider::S3);
        assert!(!should_build_aws_sdk_store(&no_auth).unwrap());
    }

    // --- Env var guard for test isolation ---

    /// RAII guard that sets/unsets an env var and restores the original value.
    struct EnvGuard {
        _lock: MutexGuard<'static, ()>,
        key: &'static str,
        original: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: Option<&str>) -> Self {
            let lock = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
            let original = std::env::var(key).ok();
            // SAFETY: process-wide env mutation is serialized by ENV_MUTEX.
            unsafe {
                match value {
                    Some(v) => std::env::set_var(key, v),
                    None => std::env::remove_var(key),
                }
            }
            Self {
                _lock: lock,
                key,
                original,
            }
        }

        fn update(&self, value: Option<&str>) {
            // SAFETY: process-wide env mutation is serialized by ENV_MUTEX.
            unsafe {
                match value {
                    Some(v) => std::env::set_var(self.key, v),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            // SAFETY: see EnvGuard::set.
            unsafe {
                match &self.original {
                    Some(v) => std::env::set_var(self.key, v),
                    None => std::env::remove_var(self.key),
                }
            }
        }
    }
}
