use std::path::{Path, PathBuf};
#[cfg(feature = "remote")]
use std::sync::Arc;
#[cfg(feature = "local")]
use std::{ffi::OsString, fmt::Write as _};

use crate::{Error, ErrorKind, Result};
#[cfg(feature = "remote")]
use crab_storage::Store;

mod azure;
pub use azure::AzureOptions;

/// Explicit provider selection; building resolves only the selected credential policy.
#[derive(Clone)]
pub struct DirectStoreOptions {
    #[cfg_attr(
        not(feature = "remote"),
        expect(
            dead_code,
            reason = "Default-feature configuration is retained for the remote provider constructor"
        )
    )]
    target: DirectTarget,
}

#[derive(Clone)]
#[cfg_attr(
    not(feature = "remote"),
    expect(
        dead_code,
        reason = "Default-feature configuration retains targets without constructing providers"
    )
)]
enum DirectTarget {
    Filesystem(PathBuf),
    Cloud(CloudTarget),
    S3(S3Options),
    Gcs(GcsOptions),
    Azure(AzureOptions),
}

/// Explicit GCS bucket and static bearer token; no default credential chain is used.
#[derive(Clone)]
#[cfg_attr(
    not(feature = "remote"),
    expect(dead_code, reason = "Configuration is consumed by remote builds")
)]
pub struct GcsOptions {
    bucket: String,
    access_token: String,
}

impl std::fmt::Debug for GcsOptions {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("GcsOptions").finish_non_exhaustive()
    }
}

impl GcsOptions {
    /// Validate a bare bucket and nonempty bearer token without resolving credentials.
    pub fn new(bucket: &str, access_token: &str) -> Result<Self> {
        if !valid_cloud_name(bucket) || access_token.is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "GCS requires a bare bucket and nonempty access token",
            ));
        }
        Ok(Self {
            bucket: bucket.to_owned(),
            access_token: access_token.to_owned(),
        })
    }
}

/// Explicit S3 credentials and placement; construction does not read environment state.
#[derive(Clone)]
#[cfg_attr(
    not(feature = "remote"),
    expect(dead_code, reason = "Configuration is consumed by remote builds")
)]
pub struct S3Options {
    bucket: String,
    region: String,
    access_key_id: String,
    secret_access_key: String,
    session_token: Option<String>,
    endpoint: Option<String>,
}

impl std::fmt::Debug for S3Options {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("S3Options").finish_non_exhaustive()
    }
}

impl S3Options {
    /// Validate a bare bucket and nonempty region/access/secret values.
    pub fn new(
        bucket: &str,
        region: &str,
        access_key_id: &str,
        secret_access_key: &str,
    ) -> Result<Self> {
        if !valid_cloud_name(bucket) || [region, access_key_id, secret_access_key].contains(&"") {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "S3 requires a bare bucket and nonempty region and credentials",
            ));
        }
        Ok(Self {
            bucket: bucket.to_owned(),
            region: region.to_owned(),
            access_key_id: access_key_id.to_owned(),
            secret_access_key: secret_access_key.to_owned(),
            session_token: None,
            endpoint: None,
        })
    }

    /// Add a nonempty session token for temporary credentials.
    pub fn with_session_token(mut self, token: &str) -> Result<Self> {
        if token.is_empty() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "S3 session token must be nonempty",
            ));
        }
        self.session_token = Some(token.to_owned());
        Ok(self)
    }

    /// Select an HTTP(S) endpoint, validated by the provider when building.
    ///
    /// Selecting HTTP explicitly permits plaintext transport for this store.
    /// Credentials, query strings and fragments in the endpoint are rejected.
    #[must_use]
    pub fn with_endpoint(mut self, endpoint: &str) -> Self {
        self.endpoint = Some(endpoint.trim().to_owned());
        self
    }
}

fn valid_cloud_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
}

#[derive(Clone)]
enum CloudTarget {
    S3(String),
    Gcs(String),
    Azure { account: String, container: String },
}

impl DirectStoreOptions {
    /// Select explicit Azure configuration without provider environment overrides.
    #[must_use]
    pub fn azure(options: AzureOptions) -> Self {
        Self {
            target: DirectTarget::Azure(options),
        }
    }

    /// Select explicit GCS configuration without provider environment overrides.
    #[must_use]
    pub fn gcs(options: GcsOptions) -> Self {
        Self {
            target: DirectTarget::Gcs(options),
        }
    }

    /// Select explicit S3 configuration without provider environment overrides.
    #[must_use]
    pub fn s3(options: S3Options) -> Self {
        Self {
            target: DirectTarget::S3(options),
        }
    }

    /// Select an absolute filesystem root; existence is checked when building.
    pub fn filesystem(root: &Path) -> Result<Self> {
        if !root.is_absolute() {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "store root must be absolute",
            ));
        }
        Ok(Self {
            target: DirectTarget::Filesystem(root.to_owned()),
        })
    }

    /// Select an S3 bucket without resolving environment credentials yet.
    pub fn s3_from_env(bucket: &str) -> Result<Self> {
        Self::cloud(CloudTarget::S3(bucket.to_owned()))
    }

    /// Select a GCS bucket without resolving environment credentials yet.
    pub fn gcs_from_env(bucket: &str) -> Result<Self> {
        Self::cloud(CloudTarget::Gcs(bucket.to_owned()))
    }

    /// Select an Azure account/container without resolving environment credentials yet.
    pub fn azure_from_env(account: &str, container: &str) -> Result<Self> {
        Self::cloud(CloudTarget::Azure {
            account: account.to_owned(),
            container: container.to_owned(),
        })
    }

    fn cloud(target: CloudTarget) -> Result<Self> {
        let valid = match &target {
            CloudTarget::S3(bucket) | CloudTarget::Gcs(bucket) => valid_cloud_name(bucket),
            CloudTarget::Azure { account, container } => {
                valid_cloud_name(account) && valid_cloud_name(container)
            }
        };
        if !valid {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "cloud target must contain bare account, bucket or container names",
            ));
        }
        Ok(Self {
            target: DirectTarget::Cloud(target),
        })
    }

    #[cfg(feature = "remote")]
    pub(crate) fn build(self) -> Result<(Store, String)> {
        match self.target {
            DirectTarget::Azure(options) => options.build(),
            DirectTarget::Gcs(options) => {
                // A token can narrow access within one bucket. Do not reuse a
                // broader token's content cache after selecting a different one.
                let mut namespace =
                    blake3::Hasher::new_derive_key("crab SDK explicit GCS cache v1");
                namespace.update(options.access_token.as_bytes());
                build_explicit(
                    &options.bucket,
                    crab_storage::ObjectStoreCredentials::Gcp {
                        access_token: options.access_token,
                    },
                    None,
                    namespace,
                )
            }
            DirectTarget::S3(options) => {
                // Transport identity intentionally excludes credentials. Keep
                // explicit credential scopes separate in SDK content caches.
                let mut namespace = blake3::Hasher::new_derive_key("crab SDK explicit S3 cache v1");
                for component in [
                    options.access_key_id.as_str(),
                    options.session_token.as_deref().unwrap_or(""),
                ] {
                    namespace.update(&(component.len() as u64).to_be_bytes());
                    namespace.update(component.as_bytes());
                }
                let credentials = crab_storage::ObjectStoreCredentials::Aws {
                    access_key_id: options.access_key_id,
                    secret_access_key: options.secret_access_key,
                    session_token: options.session_token,
                    region: options.region,
                };
                build_explicit(
                    &options.bucket,
                    credentials,
                    options.endpoint.as_deref(),
                    namespace,
                )
            }
            DirectTarget::Filesystem(root) => {
                let root = root.canonicalize().map_err(|source| {
                    Error::with_source(ErrorKind::Io, "cannot resolve store root", source)
                })?;
                let identity = blake3::hash(root.as_os_str().as_encoded_bytes());
                let namespace = identity.to_hex().to_string();
                let store = object_store::local::LocalFileSystem::new_with_prefix(root).map_err(
                    |source| {
                        Error::with_source(ErrorKind::Io, "cannot open filesystem store", source)
                    },
                )?;
                Ok((
                    Store::new(Arc::new(store)).with_target_identity(*identity.as_bytes()),
                    namespace,
                ))
            }
            DirectTarget::Cloud(target) => {
                use crab_storage::{StaticEnvStoreTarget, StorageProviderKind};
                let target = match target {
                    CloudTarget::S3(bucket) => {
                        StaticEnvStoreTarget::bucket(StorageProviderKind::S3, bucket)
                    }
                    CloudTarget::Gcs(bucket) => {
                        StaticEnvStoreTarget::bucket(StorageProviderKind::Gcs, bucket)
                    }
                    CloudTarget::Azure { account, container } => {
                        StaticEnvStoreTarget::azure_account_container(account, container)
                    }
                };
                let store = crab_storage::provider_store::build_static_env_target_store(target)
                    .map_err(|source| {
                        Error::with_source(
                            crate::remote_error::storage_kind(&source),
                            "cannot configure cloud store",
                            source,
                        )
                    })?;
                // The provider owner binds the actual endpoint/account/bucket.
                // A bucket name alone would alias caches across custom endpoints.
                let identity = store.target_identity().ok_or_else(|| {
                    Error::new(ErrorKind::Io, "cloud store has no transport identity")
                })?;
                let namespace = blake3::Hash::from(*identity).to_hex().to_string();
                Ok((store, namespace))
            }
        }
    }

    #[cfg(feature = "local")]
    pub(crate) fn local_transport(
        &self,
        locator: &crate::RepositoryLocator,
    ) -> Result<LocalTransport> {
        let repository_prefix = locator.direct_prefix()?.to_owned();
        let mut environment = Vec::new();
        let (provider, authority, prefix) = match &self.target {
            DirectTarget::S3(options) => {
                environment.extend([
                    (
                        "AWS_ACCESS_KEY_ID".into(),
                        options.access_key_id.clone().into(),
                    ),
                    (
                        "AWS_SECRET_ACCESS_KEY".into(),
                        options.secret_access_key.clone().into(),
                    ),
                    ("AWS_REGION".into(), options.region.clone().into()),
                    ("AWS_DEFAULT_REGION".into(), options.region.clone().into()),
                ]);
                if let Some(token) = &options.session_token {
                    environment.push(("AWS_SESSION_TOKEN".into(), token.clone().into()));
                }
                if let Some(endpoint) = &options.endpoint {
                    environment.push(("AWS_ENDPOINT_URL".into(), endpoint.clone().into()));
                    environment.push(("AWS_ENDPOINT_URL_S3".into(), endpoint.clone().into()));
                    if endpoint.starts_with("http:") {
                        environment.push(("AWS_ALLOW_HTTP".into(), "true".into()));
                    }
                }
                ("s3", options.bucket.as_str(), repository_prefix.clone())
            }
            DirectTarget::Gcs(options) => {
                environment.push((
                    "GOOGLE_BEARER_TOKEN".into(),
                    options.access_token.clone().into(),
                ));
                ("gcs", options.bucket.as_str(), repository_prefix.clone())
            }
            DirectTarget::Azure(options) => {
                return options.local_transport(locator);
            }
            DirectTarget::Cloud(CloudTarget::S3(bucket)) => {
                ("s3", bucket.as_str(), repository_prefix.clone())
            }
            DirectTarget::Cloud(CloudTarget::Gcs(bucket)) => {
                ("gcs", bucket.as_str(), repository_prefix.clone())
            }
            DirectTarget::Cloud(CloudTarget::Azure { account, container }) => {
                let prefix = format!("{container}/{repository_prefix}");
                ("azure", account.as_str(), prefix)
            }
            DirectTarget::Filesystem(_) => {
                return Ok(LocalTransport {
                    remote: format!("crab-sdk:{repository_prefix}"),
                    environment,
                });
            }
        };
        environment.push(("CRAB_STORAGE_PROVIDER".into(), provider.into()));
        let mut remote = String::with_capacity(authority.len() + prefix.len() + 8);
        write!(&mut remote, "crab://{authority}/{prefix}").map_err(|source| {
            Error::with_source(ErrorKind::Io, "cannot construct local remote URL", source)
        })?;
        Ok(LocalTransport {
            remote,
            environment,
        })
    }

    #[cfg(feature = "local")]
    pub(crate) fn local_environment(&self) -> Result<Vec<(OsString, OsString)>> {
        if matches!(self.target, DirectTarget::Filesystem(_)) {
            return Ok(Vec::new());
        }
        let probe = crate::RepositoryLocator::new("sdk-environment-probe")?;
        self.local_transport(&probe)
            .map(|transport| transport.environment)
    }

    #[cfg(feature = "local")]
    pub(crate) fn locator_from_remote(&self, remote: &str) -> Result<crate::RepositoryLocator> {
        if matches!(self.target, DirectTarget::Filesystem(_)) {
            let prefix = remote.strip_prefix("crab-sdk:").ok_or_else(|| {
                Error::new(
                    ErrorKind::UnsupportedCapability,
                    "local repository does not use the SDK direct transport",
                )
            })?;
            return crate::RepositoryLocator::new(prefix);
        }
        let value = remote.strip_prefix("crab://").ok_or_else(|| {
            Error::new(
                ErrorKind::UnsupportedCapability,
                "local push recovery requires a direct Crab remote",
            )
        })?;
        let (authority, path) = value.split_once('/').ok_or_else(|| {
            Error::new(
                ErrorKind::InvalidInput,
                "direct Crab remote has no repository prefix",
            )
        })?;
        let prefix = match &self.target {
            DirectTarget::S3(options) if authority == options.bucket => path,
            DirectTarget::Gcs(options) if authority == options.bucket => path,
            DirectTarget::Azure(options) if authority == options.account => path
                .strip_prefix(&format!("{}/", options.container))
                .ok_or_else(|| {
                    Error::new(
                        ErrorKind::InvalidInput,
                        "Azure remote container does not match",
                    )
                })?,
            DirectTarget::Cloud(CloudTarget::S3(bucket) | CloudTarget::Gcs(bucket))
                if authority == bucket =>
            {
                path
            }
            DirectTarget::Cloud(CloudTarget::Azure { account, container })
                if authority == account =>
            {
                path.strip_prefix(&format!("{container}/")).ok_or_else(|| {
                    Error::new(
                        ErrorKind::InvalidInput,
                        "Azure remote container does not match",
                    )
                })?
            }
            _ => {
                return Err(Error::new(
                    ErrorKind::InvalidInput,
                    "local remote does not match the selected direct store",
                ));
            }
        };
        crate::RepositoryLocator::new(prefix)
    }
}

#[cfg(feature = "local")]
pub(crate) struct LocalTransport {
    pub(crate) remote: String,
    pub(crate) environment: Vec<(OsString, OsString)>,
}

#[cfg(feature = "remote")]
fn build_explicit(
    bucket: &str,
    credentials: crab_storage::ObjectStoreCredentials,
    endpoint: Option<&str>,
    mut namespace: blake3::Hasher,
) -> Result<(Store, String)> {
    let allow_http = endpoint
        .and_then(|endpoint| endpoint.split_once(':'))
        .is_some_and(|(scheme, _)| scheme.eq_ignore_ascii_case("http"));
    let store = crab_storage::build_explicit_store(bucket, credentials, endpoint, allow_http)
        .map_err(|source| {
            Error::with_source(
                crate::remote_error::storage_kind(&source),
                "cannot configure explicit cloud store",
                source,
            )
        })?;
    let identity = store
        .target_identity()
        .ok_or_else(|| Error::new(ErrorKind::Io, "cloud store has no transport identity"))?;
    namespace.update(identity);
    Ok((store, namespace.finalize().to_hex().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(feature = "remote")]
    #[test]
    fn explicit_gcs_ignores_adc_and_separates_token_cache_scopes() {
        const CHILD: &str = "CRAB_SDK_EXPLICIT_GCS_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let configured = |token| {
                DirectStoreOptions::gcs(GcsOptions::new("bucket", token).unwrap())
                    .build()
                    .unwrap()
            };
            let (first, first_cache) = configured("first-token");
            let (other, other_cache) = configured("other-token");
            assert_eq!(first.target_identity(), other.target_identity());
            assert_ne!(first_cache, other_cache);
            return;
        }
        // Invalid ADC must not break explicit credentials. Isolate environment
        // configuration from other tests running concurrently in this process.
        let home = tempfile::tempdir().unwrap();
        let adc = home.path().join("invalid-adc.json");
        std::fs::write(&adc, b"invalid credential document").unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "store_options::tests::explicit_gcs_ignores_adc_and_separates_token_cache_scopes",
                "--nocapture",
            ])
            .env_clear()
            .env(CHILD, "1")
            .env("HOME", home.path())
            .env("GOOGLE_APPLICATION_CREDENTIALS", adc)
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[cfg(feature = "remote")]
    #[test]
    fn explicit_credentials_separate_caches_without_changing_transport_identity() {
        let configured = |access, token| {
            DirectStoreOptions::s3(
                S3Options::new("bucket", "us-east-1", access, "secret")
                    .unwrap()
                    .with_session_token(token)
                    .unwrap(),
            )
            .build()
            .unwrap()
        };
        let (first, first_cache) = configured("first-access", "first-session");
        for (access, token) in [
            ("other-access", "first-session"),
            ("first-access", "other-session"),
        ] {
            let (other, other_cache) = configured(access, token);
            assert_eq!(first.target_identity(), other.target_identity());
            assert_ne!(first_cache, other_cache);
        }
    }

    #[test]
    fn cloud_targets_reject_paths_and_urls_before_provider_construction() {
        for name in [
            "",
            ".",
            "..",
            "bucket/repo",
            "bucket\\repo",
            "s3://bucket",
            "user@host",
            "bucket?token=value",
            "bad name",
        ] {
            for options in [
                DirectStoreOptions::s3_from_env(name),
                DirectStoreOptions::gcs_from_env(name),
                DirectStoreOptions::azure_from_env(name, "container"),
                DirectStoreOptions::azure_from_env("account", name),
            ] {
                assert_eq!(
                    options.err().unwrap().to_string(),
                    "cloud target must contain bare account, bucket or container names"
                );
            }
        }
    }

    #[cfg(feature = "remote")]
    #[test]
    fn cloud_selection_uses_owner_transport_identity() {
        const CHILD: &str = "CRAB_SDK_CLOUD_SELECTION_CHILD";
        if std::env::var_os(CHILD).is_some() {
            let targets = [
                DirectStoreOptions::s3_from_env("sdk-fixture"),
                DirectStoreOptions::gcs_from_env("sdk-fixture"),
                DirectStoreOptions::azure_from_env("sdkaccount", "sdk-fixture"),
            ];
            let mut identities = std::collections::HashSet::new();
            for options in targets {
                let (store, namespace) = options.unwrap().build().unwrap();
                assert_eq!(
                    namespace,
                    blake3::Hash::from(*store.target_identity().unwrap())
                        .to_hex()
                        .to_string()
                );
                assert!(identities.insert(namespace));
            }
            return;
        }
        // Resolve credentials only in an isolated process, never mutate the
        // environment of concurrently running SDK/native fixture tests.
        let home = tempfile::tempdir().unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "store_options::tests::cloud_selection_uses_owner_transport_identity",
                "--nocapture",
            ])
            .env_clear()
            .env(CHILD, "1")
            .env("HOME", home.path())
            .env("AWS_ACCESS_KEY_ID", "fixture")
            .env("AWS_SECRET_ACCESS_KEY", "fixture")
            .env("AWS_REGION", "us-east-1")
            .env("AZURE_STORAGE_ACCOUNT_KEY", "Zml4dHVyZQ==")
            .status()
            .unwrap();
        assert!(status.success());
    }
}
