//! Provider-specific object-store construction.

use std::sync::Arc;

use object_store::aws::{
    AmazonS3, AmazonS3Builder, AmazonS3ConfigKey, AwsCredentialProvider, S3CopyIfNotExists,
};
use object_store::azure::MicrosoftAzureBuilder;
use object_store::gcp::{GcpCredential, GoogleCloudStorageBuilder, GoogleConfigKey};
use object_store::path::Path;
use object_store::{ObjectStore, StaticCredentialProvider};

use crate::error::{Result, StorageError};
use crate::identity::{BucketIdentity, StorageProviderKind};
use crate::provider_options::{
    default_client_options, parse_sas_query_pairs, s3_endpoint_from_env,
    s3_virtual_hosted_style_from_env,
};
use crate::store::Store;

pub const STATIC_ENV_PROVIDER_ENV: &str = "CRAB_STORAGE_PROVIDER";

/// Provider credentials consumed by object-store builders.
#[derive(Debug, Clone)]
pub enum ObjectStoreCredentials {
    /// Use the provider SDK's default environment chain.
    StaticEnv { provider: StorageProviderKind },
    /// S3-compatible credentials.
    Aws {
        access_key_id: String,
        secret_access_key: String,
        session_token: Option<String>,
        region: String,
    },
    /// GCP OAuth2 access token.
    Gcp { access_token: String },
    /// Azure bearer or SAS token.
    Azure {
        account: String,
        token: AzureAuthorization,
    },
}

impl ObjectStoreCredentials {
    /// Returns the physical storage provider used by these credentials.
    #[must_use]
    pub fn provider_kind(&self) -> StorageProviderKind {
        match self {
            Self::StaticEnv { provider } => *provider,
            Self::Aws { .. } => StorageProviderKind::S3,
            Self::Gcp { .. } => StorageProviderKind::Gcs,
            Self::Azure { .. } => StorageProviderKind::Azure,
        }
    }
}

/// Azure authorization accepted by the object-store builder.
#[derive(Debug, Clone)]
pub enum AzureAuthorization {
    /// OAuth2 bearer token.
    Bearer(String),
    /// SAS query string.
    Sas(String),
}

/// Built provider object store plus optional signing adapter.
pub struct BuiltObjectStore {
    pub inner: Arc<dyn ObjectStore>,
    pub provider: StorageProviderKind,
    /// Exact destination used to bind resumable provider sessions.
    ///
    /// Custom endpoint and credential-derived endpoint material is stored as
    /// a non-secret fingerprint, never as credentials or raw endpoint text.
    pub multipart_identity: Option<BucketIdentity>,
    pub signer: Option<Arc<dyn object_store::signer::Signer>>,
    pub multipart: Option<Arc<dyn object_store::multipart::MultipartStore>>,
}

/// Object-store handle parsed from a URL plus the path prefix embedded in that URL.
#[derive(Clone)]
pub struct UrlObjectStore {
    store: Arc<dyn ObjectStore>,
    prefix: Path,
}

impl UrlObjectStore {
    /// Creates a URL-backed object-store wrapper around an existing store and prefix.
    #[must_use]
    pub fn new(store: Arc<dyn ObjectStore>, prefix: Path) -> Self {
        Self { store, prefix }
    }

    /// Returns the object-store backend.
    #[must_use]
    pub fn store(&self) -> &dyn ObjectStore {
        self.store.as_ref()
    }

    /// Clones the shared object-store backend for a higher-level composition
    /// boundary that needs to build a domain-specific store handle.
    #[must_use]
    pub fn store_arc(&self) -> Arc<dyn ObjectStore> {
        Arc::clone(&self.store)
    }

    /// Returns the URL path prefix applied to store operations.
    #[must_use]
    pub fn prefix(&self) -> &Path {
        &self.prefix
    }

    /// Applies the URL prefix to a caller-supplied object path.
    #[must_use]
    pub fn path(&self, path: &Path) -> Path {
        match (self.prefix.as_ref(), path.as_ref()) {
            ("", _) => path.clone(),
            (_, "") => self.prefix.clone(),
            (prefix, path) => Path::from(format!("{prefix}/{path}")),
        }
    }
}

/// Normalized target for provider SDK default environment-chain construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StaticEnvStoreTarget {
    /// Bucket/container target where the provider uses the same name for host
    /// and storage container identity.
    Bucket {
        provider: StorageProviderKind,
        bucket: String,
    },
    /// Raw Azure target where the URL host is the storage account and the first
    /// path segment is the container.
    AzureAccountContainer { account: String, container: String },
}

/// URL form supplied to static-env target normalization.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StaticEnvStoreUrlForm {
    /// Raw provider URL such as `s3://bucket/repo` or `az://account/container/repo`.
    Raw,
    /// Crab URL such as `crab://bucket/repo`, where provider selection is external.
    Crab,
}

/// Primitive URL fields needed to normalize a static-env object-store target.
#[derive(Clone, Copy)]
pub struct StaticEnvStoreUrlParts<'a> {
    pub provider: StorageProviderKind,
    pub form: StaticEnvStoreUrlForm,
    pub bucket: &'a str,
    pub prefix: &'a str,
}

/// Static-env store target plus the effective repository prefix for routing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaticEnvStoreTargetSelection {
    pub target: StaticEnvStoreTarget,
    pub repo_prefix: String,
}

impl StaticEnvStoreTarget {
    /// Build a cloud bucket/container target.
    #[must_use]
    pub fn bucket(provider: StorageProviderKind, bucket: impl Into<String>) -> Self {
        Self::Bucket {
            provider,
            bucket: bucket.into(),
        }
    }

    /// Build a raw Azure account/container target.
    #[must_use]
    pub fn azure_account_container(
        account: impl Into<String>,
        container: impl Into<String>,
    ) -> Self {
        Self::AzureAccountContainer {
            account: account.into(),
            container: container.into(),
        }
    }
}

/// Normalizes a parsed static-env URL into a provider target and repo prefix.
pub fn static_env_target_selection(
    parts: StaticEnvStoreUrlParts<'_>,
    crab_provider: Option<StorageProviderKind>,
    default_repo_prefix: &str,
) -> Result<StaticEnvStoreTargetSelection> {
    if parts.provider == StorageProviderKind::Azure && parts.form == StaticEnvStoreUrlForm::Raw {
        let (container, object_prefix) =
            split_azure_account_container_prefix(parts.bucket, parts.prefix)?;
        return Ok(StaticEnvStoreTargetSelection {
            target: StaticEnvStoreTarget::azure_account_container(
                parts.bucket,
                container.to_ascii_lowercase(),
            ),
            repo_prefix: effective_repo_prefix(object_prefix, default_repo_prefix),
        });
    }

    let provider = match parts.form {
        StaticEnvStoreUrlForm::Raw => parts.provider,
        StaticEnvStoreUrlForm::Crab => match crab_provider {
            Some(provider) => provider,
            None => resolve_static_env_provider()?,
        },
    };
    Ok(StaticEnvStoreTargetSelection {
        target: StaticEnvStoreTarget::bucket(provider, parts.bucket),
        repo_prefix: effective_repo_prefix(parts.prefix, default_repo_prefix),
    })
}

/// Validates that a parsed static-env URL is compatible with the expected provider.
pub fn validate_static_env_url_provider(
    parts: StaticEnvStoreUrlParts<'_>,
    expected_provider: StorageProviderKind,
) -> Result<()> {
    if parts.form == StaticEnvStoreUrlForm::Raw && parts.provider != expected_provider {
        return Err(StorageError::StaticEnvProviderMismatch {
            expected: expected_provider,
            actual: parts.provider,
            bucket: parts.bucket.to_owned(),
        });
    }
    Ok(())
}

/// Normalizes a static-env URL whose provider has already been selected by config.
pub fn static_env_target_selection_for_provider(
    parts: StaticEnvStoreUrlParts<'_>,
    expected_provider: StorageProviderKind,
    default_repo_prefix: &str,
) -> Result<StaticEnvStoreTargetSelection> {
    validate_static_env_url_provider(parts, expected_provider)?;
    static_env_target_selection(parts, Some(expected_provider), default_repo_prefix)
}

fn split_azure_account_container_prefix<'a>(
    account: &str,
    prefix: &'a str,
) -> Result<(&'a str, &'a str)> {
    let (container, object_prefix) = prefix
        .split_once('/')
        .map_or((prefix, ""), |(container, object_prefix)| {
            (container, object_prefix)
        });
    if container.is_empty() {
        return Err(StorageError::InvalidStaticEnvTarget {
            target: format!("az://{account}"),
            reason: "raw Azure URLs must include a container path segment".into(),
        });
    }
    Ok((container, object_prefix.trim_matches('/')))
}

fn effective_repo_prefix(prefix: &str, default_repo_prefix: &str) -> String {
    let prefix = prefix.trim_matches('/');
    if prefix.is_empty() {
        default_repo_prefix.to_owned()
    } else {
        prefix.to_owned()
    }
}

/// Resolves the static-env provider from `CRAB_STORAGE_PROVIDER`.
///
/// An unset or empty value selects S3. An explicit value must name a cloud provider.
pub fn resolve_static_env_provider() -> Result<StorageProviderKind> {
    let value = std::env::var(STATIC_ENV_PROVIDER_ENV).ok();
    resolve_static_env_provider_value(value.as_deref())
}

/// Resolves a static-env provider from an optional config/env value.
///
/// This parser accepts only cloud aliases. Missing values select the canonical
/// S3 default; invalid explicit values fail before an object-store client is built.
pub fn resolve_static_env_provider_value(value: Option<&str>) -> Result<StorageProviderKind> {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return Ok(StorageProviderKind::S3);
    };
    StorageProviderKind::parse_cloud_alias(value).ok_or_else(|| {
        StorageError::InvalidStaticEnvTarget {
            target: STATIC_ENV_PROVIDER_ENV.to_owned(),
            reason: format!("unsupported provider {value:?}; expected s3, gcs, or azure"),
        }
    })
}

/// Builds an object-store backend from provider credentials.
///
/// The returned signer is present only for providers whose concrete
/// `object_store` adapter also implements URL signing.
pub fn build_object_store(
    bucket: &str,
    credentials: ObjectStoreCredentials,
) -> Result<BuiltObjectStore> {
    build_object_store_inner(bucket, credentials, None, true)
}

/// Builds an S3 object store with a caller-supplied refreshable credential provider.
pub fn build_s3_object_store_with_provider(
    bucket: &str,
    region: &str,
    credentials: AwsCredentialProvider,
) -> Result<BuiltObjectStore> {
    let endpoint = s3_endpoint_from_env();
    let builder = AmazonS3Builder::from_env()
        .with_bucket_name(bucket)
        .with_region(region)
        .with_credentials(credentials)
        .with_client_options(default_client_options());
    build_s3_object_store(bucket, builder, None, endpoint.as_deref(), true)
}

/// Builds an object-store backend with an optional grant-pinned endpoint.
pub fn build_object_store_with_endpoint(
    bucket: &str,
    credentials: ObjectStoreCredentials,
    endpoint: Option<&str>,
) -> Result<BuiltObjectStore> {
    build_object_store_inner(bucket, credentials, endpoint, false)
}

fn build_object_store_inner(
    bucket: &str,
    credentials: ObjectStoreCredentials,
    endpoint: Option<&str>,
    allow_environment_overrides: bool,
) -> Result<BuiltObjectStore> {
    let provider = credentials.provider_kind();
    match credentials {
        ObjectStoreCredentials::StaticEnv { provider } => {
            build_static_env_object_store(bucket, provider)
        }
        ObjectStoreCredentials::Aws {
            access_key_id,
            secret_access_key,
            session_token,
            region,
        } => {
            let endpoint = endpoint.map(str::to_owned).or_else(|| {
                allow_environment_overrides
                    .then(s3_endpoint_from_env)
                    .flatten()
            });
            let builder = AmazonS3Builder::new()
                .with_bucket_name(bucket)
                .with_access_key_id(&access_key_id)
                .with_secret_access_key(&secret_access_key)
                .with_region(&region)
                .with_client_options(default_client_options());
            build_s3_object_store(
                bucket,
                builder,
                session_token.as_deref(),
                endpoint.as_deref(),
                allow_environment_overrides,
            )
        }
        ObjectStoreCredentials::Gcp { access_token } => {
            if endpoint.is_some() {
                return Err(StorageError::InvalidStaticEnvTarget {
                    target: bucket.to_owned(),
                    reason: "custom GCS endpoints are not supported for managed grants".to_owned(),
                });
            }
            let credential_provider = Arc::new(StaticCredentialProvider::new(GcpCredential {
                bearer: access_token,
            }));
            let builder = GoogleCloudStorageBuilder::new()
                .with_bucket_name(bucket)
                .with_credentials(credential_provider)
                .with_client_options(default_client_options());
            let (builder, multipart_identity) = gcs_multipart_builder(builder, bucket)?;
            let gcs = builder
                .build()
                .map_err(|source| provider_config_error(provider, bucket, source))?;
            let gcs = Arc::new(gcs);
            Ok(BuiltObjectStore {
                inner: gcs.clone() as Arc<dyn ObjectStore>,
                provider,
                multipart_identity: Some(multipart_identity),
                signer: None,
                multipart: Some(gcs),
            })
        }
        ObjectStoreCredentials::Azure { account, token } => {
            let builder = MicrosoftAzureBuilder::new()
                .with_account(account)
                .with_container_name(bucket)
                .with_client_options(default_client_options());
            let builder = match endpoint {
                Some(value) => builder.with_endpoint(value.to_owned()),
                None => builder,
            };
            let builder = match token {
                AzureAuthorization::Bearer(token) => builder.with_bearer_token_authorization(token),
                AzureAuthorization::Sas(sas) => {
                    builder.with_sas_authorization(parse_sas_query_pairs(&sas))
                }
            };
            let azure = builder
                .build()
                .map_err(|source| provider_config_error(provider, bucket, source))?;
            Ok(BuiltObjectStore {
                inner: Arc::new(azure),
                provider,
                multipart_identity: None,
                signer: None,
                multipart: None,
            })
        }
    }
}

/// Builds a Crab store from the provider SDK's default environment chain.
pub fn build_static_env_store(bucket: &str, provider: StorageProviderKind) -> Result<Store> {
    let built = build_object_store(bucket, ObjectStoreCredentials::StaticEnv { provider })?;
    let identity = BucketIdentity::new(built.provider, bucket, bucket);
    let mut store = Store::new(built.inner).with_bucket_identity(identity);
    if let Some(signer) = built.signer {
        store = store.with_signer(signer);
    }
    if let (Some(multipart), Some(multipart_identity)) = (built.multipart, built.multipart_identity)
    {
        store = store.with_multipart(multipart, multipart_identity);
    }
    Ok(store)
}

/// Builds an Azure store for raw account/container URLs from the default environment chain.
pub fn build_static_env_azure_account_container_store(
    account: &str,
    container: &str,
) -> Result<Store> {
    let provider = StorageProviderKind::Azure;
    let azure = MicrosoftAzureBuilder::from_env()
        .with_account(account)
        .with_container_name(container)
        .with_client_options(default_client_options())
        .build()
        .map_err(|source| {
            provider_config_error(provider, &format!("{account}/{container}"), source)
        })?;
    Ok(Store::new(Arc::new(azure))
        .with_bucket_identity(BucketIdentity::new(provider, account, container)))
}

/// Builds a Crab store for a normalized static-env target.
pub fn build_static_env_target_store(target: StaticEnvStoreTarget) -> Result<Store> {
    match target {
        StaticEnvStoreTarget::Bucket { provider, bucket } => {
            build_static_env_store(&bucket, provider)
        }
        StaticEnvStoreTarget::AzureAccountContainer { account, container } => {
            build_static_env_azure_account_container_store(&account, &container)
        }
    }
}

/// Builds an object-store backend from a URL and process environment options.
pub fn build_url_object_store(url: &str) -> Result<UrlObjectStore> {
    let parsed_url =
        url::Url::parse(url).map_err(|source| StorageError::InvalidObjectStoreUrl {
            url: url.to_owned(),
            source,
        })?;

    let options = object_store_options_from_env();
    let (store, prefix) = object_store::parse_url_opts(&parsed_url, options).map_err(|source| {
        StorageError::UrlStoreConfig {
            url: url.to_owned(),
            source,
        }
    })?;

    Ok(UrlObjectStore::new(Arc::from(store), prefix))
}

fn build_static_env_object_store(
    bucket: &str,
    provider: StorageProviderKind,
) -> Result<BuiltObjectStore> {
    match provider {
        StorageProviderKind::S3 => {
            let endpoint = s3_endpoint_from_env();
            build_s3_object_store(
                bucket,
                AmazonS3Builder::from_env()
                    .with_bucket_name(bucket)
                    .with_client_options(default_client_options()),
                None,
                endpoint.as_deref(),
                true,
            )
        }
        StorageProviderKind::Gcs => {
            let builder = GoogleCloudStorageBuilder::from_env()
                .with_bucket_name(bucket)
                .with_client_options(default_client_options());
            let (builder, multipart_identity) = gcs_multipart_builder(builder, bucket)?;
            let gcs = builder
                .build()
                .map_err(|source| provider_config_error(provider, bucket, source))?;
            let gcs = Arc::new(gcs);
            Ok(BuiltObjectStore {
                inner: gcs.clone() as Arc<dyn ObjectStore>,
                provider,
                multipart_identity: Some(multipart_identity),
                signer: None,
                multipart: Some(gcs),
            })
        }
        StorageProviderKind::Azure => {
            let azure = MicrosoftAzureBuilder::from_env()
                .with_container_name(bucket)
                .with_client_options(default_client_options())
                .build()
                .map_err(|source| provider_config_error(provider, bucket, source))?;
            Ok(BuiltObjectStore {
                inner: Arc::new(azure),
                provider,
                multipart_identity: None,
                signer: None,
                multipart: None,
            })
        }
        StorageProviderKind::Local => Err(StorageError::UnsupportedProvider { provider }),
    }
}

fn build_s3_object_store(
    bucket: &str,
    builder: AmazonS3Builder,
    session_token: Option<&str>,
    endpoint: Option<&str>,
    allow_environment_overrides: bool,
) -> Result<BuiltObjectStore> {
    let mut builder = match endpoint {
        Some(endpoint) => builder.with_config(AmazonS3ConfigKey::S3Endpoint, endpoint),
        None => builder,
    };
    if allow_environment_overrides && let Some(virtual_hosted) = s3_virtual_hosted_style_from_env()
    {
        builder = builder.with_virtual_hosted_style_request(virtual_hosted);
    }
    if let Some(session_token) = session_token {
        builder = builder.with_token(session_token);
    }
    let multipart_identity = s3_multipart_identity(&builder, bucket);
    let s3 = builder
        .with_copy_if_not_exists(S3CopyIfNotExists::Multipart)
        .build()
        .map_err(|source| provider_config_error(StorageProviderKind::S3, bucket, source))?;
    let s3: Arc<AmazonS3> = Arc::new(s3);
    Ok(BuiltObjectStore {
        inner: s3.clone() as Arc<dyn ObjectStore>,
        provider: StorageProviderKind::S3,
        multipart_identity: Some(multipart_identity),
        signer: Some(s3.clone() as Arc<dyn object_store::signer::Signer>),
        multipart: Some(s3),
    })
}

fn s3_multipart_identity(builder: &AmazonS3Builder, bucket: &str) -> BucketIdentity {
    // object_store 0.14 derives the bucket URL from all four routing fields;
    // custom endpoints with different addressing modes are different targets.
    let endpoint = builder
        .get_config_value(&AmazonS3ConfigKey::S3Endpoint)
        .or_else(|| builder.get_config_value(&AmazonS3ConfigKey::Endpoint));
    let region = builder.get_config_value(&AmazonS3ConfigKey::Region);
    let virtual_hosted = builder.get_config_value(&AmazonS3ConfigKey::VirtualHostedStyleRequest);
    let express = builder.get_config_value(&AmazonS3ConfigKey::S3Express);
    let routing = format!("{endpoint:?}\0{region:?}\0{virtual_hosted:?}\0{express:?}");
    multipart_identity(
        StorageProviderKind::S3,
        bucket,
        "s3-routing",
        routing.as_bytes(),
    )
}

fn gcs_multipart_builder(
    builder: GoogleCloudStorageBuilder,
    bucket: &str,
) -> Result<(GoogleCloudStorageBuilder, BucketIdentity)> {
    let endpoint = if let Some(base_url) = builder.get_config_value(&GoogleConfigKey::BaseUrl) {
        Some(base_url)
    } else if let Some(service_account) =
        builder.get_config_value(&GoogleConfigKey::ServiceAccountKey)
    {
        gcs_service_account_base_url(service_account.as_bytes())
    } else if let Some(path) = builder.get_config_value(&GoogleConfigKey::ServiceAccount) {
        let service_account = std::fs::read(path)?;
        gcs_service_account_base_url(&service_account)
    } else {
        None
    };
    // Pin object_store 0.14's resolved default/custom URL before build rereads
    // a service-account file, so a concurrent file change cannot retarget it.
    let endpoint = endpoint.unwrap_or_else(|| "https://storage.googleapis.com".to_owned());
    let identity = multipart_identity(
        StorageProviderKind::Gcs,
        bucket,
        "gcs-base-url",
        endpoint.as_bytes(),
    );
    Ok((builder.with_base_url(&endpoint), identity))
}

fn gcs_service_account_base_url(service_account: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(service_account).ok()?;
    value.get("gcs_base_url")?.as_str().map(str::to_owned)
}

fn multipart_identity(
    provider: StorageProviderKind,
    bucket: &str,
    kind: &str,
    material: &[u8],
) -> BucketIdentity {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab multipart destination v1\0");
    hasher.update(kind.as_bytes());
    hasher.update(b"\0");
    hasher.update(material);
    let host = format!("endpoint:{}", hasher.finalize().to_hex());
    BucketIdentity::new(provider, host, bucket)
}

fn provider_config_error(
    provider: StorageProviderKind,
    bucket: &str,
    source: object_store::Error,
) -> StorageError {
    StorageError::ProviderConfig {
        provider,
        bucket: bucket.to_owned(),
        source,
    }
}

fn object_store_options_from_env() -> Vec<(String, String)> {
    let mut options = Vec::new();

    for (key, value) in std::env::vars() {
        options.push((key.clone(), value.clone()));
        if let Some(normalized) = normalize_env_option_key(&key) {
            options.push((normalized, value));
        }
    }

    options
}

fn normalize_env_option_key(key: &str) -> Option<String> {
    let normalized = key.to_ascii_lowercase();
    if normalized == key {
        None
    } else {
        Some(normalized)
    }
}

#[cfg(test)]
#[expect(clippy::panic, clippy::expect_used, reason = "test assertions")]
mod tests {
    use super::*;

    #[test]
    fn credential_provider_kind_matches_variant() {
        assert_eq!(
            ObjectStoreCredentials::StaticEnv {
                provider: StorageProviderKind::Gcs,
            }
            .provider_kind(),
            StorageProviderKind::Gcs
        );
        assert_eq!(
            ObjectStoreCredentials::Aws {
                access_key_id: "access".into(),
                secret_access_key: "secret".into(),
                session_token: None,
                region: "us-east-1".into(),
            }
            .provider_kind(),
            StorageProviderKind::S3
        );
        assert_eq!(
            ObjectStoreCredentials::Gcp {
                access_token: "token".into(),
            }
            .provider_kind(),
            StorageProviderKind::Gcs
        );
        assert_eq!(
            ObjectStoreCredentials::Azure {
                account: "account".into(),
                token: AzureAuthorization::Bearer("token".into()),
            }
            .provider_kind(),
            StorageProviderKind::Azure
        );
    }

    #[test]
    fn explicit_s3_credentials_build_signing_store() {
        let built = build_object_store(
            "crab-test-bucket",
            ObjectStoreCredentials::Aws {
                access_key_id: "access".into(),
                secret_access_key: "secret".into(),
                session_token: Some("session".into()),
                region: "us-east-1".into(),
            },
        )
        .expect("static S3 builder does not perform network I/O");

        assert_eq!(built.provider, StorageProviderKind::S3);
        assert!(built.signer.is_some());
    }

    #[test]
    fn explicit_s3_endpoint_is_part_of_destination_identity() {
        let first = build_object_store_with_endpoint(
            "shared-bucket",
            ObjectStoreCredentials::Aws {
                access_key_id: "access".into(),
                secret_access_key: "secret".into(),
                session_token: None,
                region: "us-east-1".into(),
            },
            Some("https://objects.example.test"),
        )
        .expect("S3 builder construction does not perform network I/O");
        let second = build_object_store_with_endpoint(
            "shared-bucket",
            ObjectStoreCredentials::Aws {
                access_key_id: "access".into(),
                secret_access_key: "secret".into(),
                session_token: None,
                region: "us-east-1".into(),
            },
            Some("https://other.example.test"),
        )
        .expect("S3 builder construction does not perform network I/O");

        assert_ne!(first.multipart_identity, second.multipart_identity);
        let identity = first.multipart_identity.expect("S3 supports multipart");
        assert_eq!(identity.container, "shared-bucket");
        assert!(identity.host.starts_with("endpoint:"));
        assert!(!identity.host.contains("objects.example.test"));
    }

    #[test]
    fn gcs_base_url_and_service_account_config_bind_multipart_identity() {
        let explicit = GoogleCloudStorageBuilder::new()
            .with_config(GoogleConfigKey::BaseUrl, "https://gcs-a.example.test");
        let other_explicit = GoogleCloudStorageBuilder::new()
            .with_config(GoogleConfigKey::BaseUrl, "https://gcs-b.example.test");
        let service_account = GoogleCloudStorageBuilder::new().with_config(
            GoogleConfigKey::ServiceAccountKey,
            r#"{"gcs_base_url":"https://gcs-a.example.test"}"#,
        );

        let (_, explicit) = gcs_multipart_builder(explicit, "bucket").expect("identity");
        let (_, other_explicit) =
            gcs_multipart_builder(other_explicit, "bucket").expect("identity");
        let (_, service_account) =
            gcs_multipart_builder(service_account, "bucket").expect("identity");

        assert_ne!(explicit, other_explicit);
        assert_eq!(explicit, service_account);
        assert_eq!(explicit.container, "bucket");
        assert!(explicit.host.starts_with("endpoint:"));
    }

    #[test]
    fn s3_multipart_identity_distinguishes_addressing_mode_and_region() {
        let path_style = AmazonS3Builder::new()
            .with_endpoint("https://objects.example.test")
            .with_region("us-east-1");
        let virtual_hosted = path_style.clone().with_virtual_hosted_style_request(true);
        let other_region = path_style.clone().with_region("us-west-2");
        let identity = s3_multipart_identity(&path_style, "bucket");

        assert_ne!(identity, s3_multipart_identity(&virtual_hosted, "bucket"));
        assert_ne!(identity, s3_multipart_identity(&other_region, "bucket"));
    }

    #[test]
    fn gcs_multipart_pins_endpoint_before_service_account_file_changes() {
        let dir = tempfile::tempdir().expect("temporary directory");
        let path = dir.path().join("service-account.json");
        std::fs::write(&path, r#"{"gcs_base_url":"https://first.example.test"}"#)
            .expect("write original endpoint");
        let builder =
            GoogleCloudStorageBuilder::new().with_service_account_path(path.to_string_lossy());
        let (builder, identity) = gcs_multipart_builder(builder, "bucket").expect("pin endpoint");
        std::fs::write(&path, r#"{"gcs_base_url":"https://second.example.test"}"#)
            .expect("replace credential file");

        assert_eq!(
            builder
                .get_config_value(&GoogleConfigKey::BaseUrl)
                .as_deref(),
            Some("https://first.example.test"),
        );
        assert_eq!(
            gcs_multipart_builder(builder, "bucket")
                .expect("recheck pinned endpoint")
                .1,
            identity,
        );
    }

    #[test]
    fn local_static_env_is_not_cloud_object_store() {
        let result = build_object_store(
            "bucket",
            ObjectStoreCredentials::StaticEnv {
                provider: StorageProviderKind::Local,
            },
        );

        assert!(matches!(
            result,
            Err(StorageError::UnsupportedProvider {
                provider: StorageProviderKind::Local
            })
        ));
    }

    #[test]
    fn static_env_store_is_cloud_only() {
        let result = build_static_env_store("bucket", StorageProviderKind::Local);

        assert!(matches!(
            result,
            Err(StorageError::UnsupportedProvider {
                provider: StorageProviderKind::Local
            })
        ));
    }

    #[test]
    fn static_env_azure_account_container_store_sets_identity() {
        let store = build_static_env_azure_account_container_store("account", "container")
            .expect("Azure builder construction does not perform network I/O");

        assert_eq!(
            store.bucket_identity(),
            BucketIdentity::new(StorageProviderKind::Azure, "account", "container")
        );
    }

    #[test]
    fn static_env_target_azure_account_container_store_sets_identity() {
        let store = build_static_env_target_store(StaticEnvStoreTarget::azure_account_container(
            "account",
            "container",
        ))
        .expect("Azure builder construction does not perform network I/O");

        assert_eq!(
            store.bucket_identity(),
            BucketIdentity::new(StorageProviderKind::Azure, "account", "container")
        );
    }

    #[test]
    fn static_env_target_bucket_rejects_local_provider() {
        let result = build_static_env_target_store(StaticEnvStoreTarget::bucket(
            StorageProviderKind::Local,
            "repo",
        ));

        assert!(matches!(
            result,
            Err(StorageError::UnsupportedProvider {
                provider: StorageProviderKind::Local
            })
        ));
    }

    #[test]
    fn static_env_target_selection_uses_crab_provider_for_crab_url_parts() {
        let selection = static_env_target_selection(
            StaticEnvStoreUrlParts {
                provider: StorageProviderKind::S3,
                form: StaticEnvStoreUrlForm::Crab,
                bucket: "bucket",
                prefix: "org/repo",
            },
            Some(StorageProviderKind::Gcs),
            "fallback/repo",
        )
        .expect("selection");

        assert_eq!(
            selection,
            StaticEnvStoreTargetSelection {
                target: StaticEnvStoreTarget::bucket(StorageProviderKind::Gcs, "bucket"),
                repo_prefix: "org/repo".into(),
            }
        );
    }

    #[test]
    fn static_env_target_selection_for_provider_allows_crab_url_parts() {
        let selection = static_env_target_selection_for_provider(
            StaticEnvStoreUrlParts {
                provider: StorageProviderKind::S3,
                form: StaticEnvStoreUrlForm::Crab,
                bucket: "bucket",
                prefix: "org/repo",
            },
            StorageProviderKind::Azure,
            "fallback/repo",
        )
        .expect("selection");

        assert_eq!(
            selection,
            StaticEnvStoreTargetSelection {
                target: StaticEnvStoreTarget::bucket(StorageProviderKind::Azure, "bucket"),
                repo_prefix: "org/repo".into(),
            }
        );
    }

    #[test]
    fn static_env_target_selection_for_provider_rejects_raw_provider_mismatch() {
        let result = static_env_target_selection_for_provider(
            StaticEnvStoreUrlParts {
                provider: StorageProviderKind::S3,
                form: StaticEnvStoreUrlForm::Raw,
                bucket: "bucket",
                prefix: "org/repo",
            },
            StorageProviderKind::Gcs,
            "fallback/repo",
        );

        assert!(matches!(
            result,
            Err(StorageError::StaticEnvProviderMismatch {
                expected: StorageProviderKind::Gcs,
                actual: StorageProviderKind::S3,
                bucket,
            }) if bucket == "bucket"
        ));
    }

    #[test]
    fn static_env_target_selection_strips_raw_azure_container_from_repo_prefix() {
        let selection = static_env_target_selection(
            StaticEnvStoreUrlParts {
                provider: StorageProviderKind::Azure,
                form: StaticEnvStoreUrlForm::Raw,
                bucket: "account",
                prefix: "Container/org/repo",
            },
            None,
            "primary/repo",
        )
        .expect("selection");

        assert_eq!(
            selection,
            StaticEnvStoreTargetSelection {
                target: StaticEnvStoreTarget::azure_account_container("account", "container"),
                repo_prefix: "org/repo".into(),
            }
        );
    }

    #[test]
    fn static_env_target_selection_uses_default_repo_prefix_for_container_root() {
        let selection = static_env_target_selection(
            StaticEnvStoreUrlParts {
                provider: StorageProviderKind::Azure,
                form: StaticEnvStoreUrlForm::Raw,
                bucket: "account",
                prefix: "container",
            },
            None,
            "primary/repo",
        )
        .expect("selection");

        assert_eq!(selection.repo_prefix, "primary/repo");
        assert_eq!(
            selection.target,
            StaticEnvStoreTarget::azure_account_container("account", "container")
        );
    }

    #[test]
    fn static_env_target_selection_rejects_raw_azure_without_container() {
        let result = static_env_target_selection(
            StaticEnvStoreUrlParts {
                provider: StorageProviderKind::Azure,
                form: StaticEnvStoreUrlForm::Raw,
                bucket: "account",
                prefix: "",
            },
            None,
            "primary/repo",
        );

        assert!(matches!(
            result,
            Err(StorageError::InvalidStaticEnvTarget { .. })
        ));
    }

    #[test]
    fn static_env_provider_defaults_to_s3_for_missing_or_empty() {
        assert_eq!(
            resolve_static_env_provider_value(None).unwrap(),
            StorageProviderKind::S3
        );
        assert_eq!(
            resolve_static_env_provider_value(Some(" ")).unwrap(),
            StorageProviderKind::S3
        );
    }

    #[test]
    fn static_env_provider_rejects_invalid_explicit_values() {
        for value in ["dropbox", "auto", "file"] {
            assert!(matches!(
                resolve_static_env_provider_value(Some(value)),
                Err(StorageError::InvalidStaticEnvTarget { .. })
            ));
        }
    }

    #[test]
    fn static_env_provider_parses_cloud_aliases() {
        for alias in ["aws", "s3"] {
            assert_eq!(
                resolve_static_env_provider_value(Some(alias)).unwrap(),
                StorageProviderKind::S3
            );
        }
        for alias in ["gcp", "gcs", "gs", "google"] {
            assert_eq!(
                resolve_static_env_provider_value(Some(alias)).unwrap(),
                StorageProviderKind::Gcs
            );
        }
        for alias in ["azure", "az", "abs"] {
            assert_eq!(
                resolve_static_env_provider_value(Some(alias)).unwrap(),
                StorageProviderKind::Azure
            );
        }
    }

    #[test]
    fn url_object_store_applies_url_prefix_to_paths() {
        let store = Arc::new(object_store::memory::InMemory::new());
        let prefixed = UrlObjectStore::new(store.clone(), Path::from("base/prefix"));
        let unprefixed = UrlObjectStore::new(store, Path::default());

        assert_eq!(
            prefixed.path(&Path::from("repo/object")).as_ref(),
            "base/prefix/repo/object"
        );
        assert_eq!(prefixed.path(&Path::default()).as_ref(), "base/prefix");
        assert_eq!(
            unprefixed.path(&Path::from("repo/object")).as_ref(),
            "repo/object"
        );
    }

    #[test]
    fn invalid_url_object_store_reports_url_parse_error() {
        let result = build_url_object_store("not a url");

        assert!(matches!(
            result,
            Err(StorageError::InvalidObjectStoreUrl { .. })
        ));
    }

    #[test]
    fn normalize_env_option_key_lowercases_provider_vars_for_parse_url_opts() {
        assert_eq!(
            normalize_env_option_key("AWS_ACCESS_KEY_ID").as_deref(),
            Some("aws_access_key_id")
        );
        assert_eq!(
            normalize_env_option_key("AWS_SECRET_ACCESS_KEY").as_deref(),
            Some("aws_secret_access_key")
        );
        assert_eq!(
            normalize_env_option_key("AWS_ENDPOINT_URL").as_deref(),
            Some("aws_endpoint_url")
        );
        assert_eq!(
            normalize_env_option_key("AWS_VIRTUAL_HOSTED_STYLE_REQUEST").as_deref(),
            Some("aws_virtual_hosted_style_request")
        );
        assert_eq!(normalize_env_option_key("aws_access_key_id"), None);
    }
}
