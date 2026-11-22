use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crab_auth::{
    CrabAuthClientConfig, CredentialProvider, CredentialResolution, cdn_checkout::validate_issuer,
    crab_auth_client::CdnViewerDeployment,
};
use crab_storage::{
    Store,
    cdn::{
        CdnAccess, CdnConsumer, CdnPolicy, CdnProfile, CdnPublicationFile, CdnRoute, CdnTransport,
        CdnVisibility,
    },
};
use tokio_util::sync::CancellationToken;

use crate::{Error, ErrorKind, Result, client::ResolvedRepository};

const READ_OPERATION: &str = "fetch";

/// Trusted issuer used to authorize a private direct repository read.
#[derive(Debug, Clone)]
pub struct CdnIssuerOptions(CrabAuthClientConfig);

impl CdnIssuerOptions {
    /// Select an HTTPS Crab Auth endpoint and an existing local OIDC login.
    pub fn new(
        endpoint: &str,
        oidc_issuer: &str,
        oidc_client_id: &str,
        token_cache_directory: impl AsRef<Path>,
    ) -> Result<Self> {
        let path = token_cache_directory
            .as_ref()
            .to_str()
            .ok_or_else(|| invalid("CDN issuer token-cache directory must be Unicode"))?;
        let config = CrabAuthClientConfig {
            endpoint: endpoint.to_owned(),
            issuer_url: oidc_issuer.to_owned(),
            client_id: oidc_client_id.to_owned(),
            token_cache_path: path.to_owned(),
            client_version: env!("CARGO_PKG_VERSION").to_owned(),
        };
        validate_issuer(&config).map_err(|source| {
            Error::with_source(ErrorKind::InvalidInput, "invalid CDN issuer", source)
        })?;
        Ok(Self(config))
    }
}

/// Opt-in CDN binding for one direct native origin and source repository.
#[derive(Debug, Clone)]
pub struct DirectCdnOptions {
    deployment: CdnViewerDeployment,
    access: Access,
    publication: Option<CdnPublicationFile>,
}

#[derive(Debug, Clone)]
enum Access {
    Public(String),
    Private(CdnIssuerOptions),
}

impl DirectCdnOptions {
    /// Select an isolated public export. Omit the approval to use native reads.
    pub fn public_export(
        origin: &str,
        repository_prefix: &str,
        export_origin: &str,
        base_url: &str,
        publication: Option<CdnPublicationFile>,
    ) -> Result<Self> {
        let mut options = Self::new(
            origin,
            repository_prefix,
            base_url,
            Access::Public(export_origin.to_owned()),
        )?;
        options.publication = publication;
        Ok(options)
    }

    /// Select a private export. Omit approval to use the issuer's native authorization.
    pub fn private_export(
        origin: &str,
        repository_prefix: &str,
        base_url: &str,
        issuer: CdnIssuerOptions,
        approval_sha256: Option<&str>,
    ) -> Result<Self> {
        let mut options = Self::new(origin, repository_prefix, base_url, Access::Private(issuer))?;
        options.deployment =
            options
                .deployment
                .with_approval(approval_sha256)
                .map_err(|source| {
                    Error::with_source(
                        ErrorKind::InvalidInput,
                        "invalid private CDN approval",
                        source,
                    )
                })?;
        Ok(options)
    }

    fn new(origin: &str, repository_prefix: &str, base_url: &str, access: Access) -> Result<Self> {
        let deployment =
            CdnViewerDeployment::new(origin, repository_prefix, base_url).map_err(|source| {
                Error::with_source(
                    ErrorKind::InvalidInput,
                    "invalid direct CDN binding",
                    source,
                )
            })?;
        if let Access::Public(export) = &access {
            deployment
                .validate_public_export(export)
                .map_err(|source| {
                    Error::with_source(ErrorKind::InvalidInput, "invalid public CDN export", source)
                })?;
        }
        Ok(Self {
            deployment,
            access,
            publication: None,
        })
    }

    pub(crate) fn validate_origin(&self, store: &Store) -> Result<()> {
        if store.native_cdn_origin() != Some(self.deployment.origin.as_str()) {
            return Err(invalid(
                "CDN binding does not match the selected native origin",
            ));
        }
        Ok(())
    }

    #[cfg(feature = "local")]
    pub(crate) fn matches_checkout(
        &self,
        locator: &crate::RepositoryLocator,
        consumer: CdnConsumer,
    ) -> bool {
        (self.edge_enabled(consumer) || self.uses_checkout_issuer())
            && locator.direct_prefix().ok() == Some(self.deployment.source_repo.as_str())
    }

    #[cfg(feature = "local")]
    pub(crate) fn uses_checkout_issuer(&self) -> bool {
        matches!(&self.access, Access::Private(_))
    }

    fn edge_enabled(&self, consumer: CdnConsumer) -> bool {
        if match self.access {
            Access::Public(_) => self.publication.is_none(),
            Access::Private(_) => self.deployment.approval_sha256().is_none(),
        } {
            return false;
        }
        self.deployment.storage_provider().is_some_and(|provider| {
            CdnPolicy {
                provider,
                consumer,
                profile: CdnProfile::Direct,
                visibility: match self.access {
                    Access::Public(_) => CdnVisibility::Public,
                    Access::Private(_) => CdnVisibility::Private,
                },
            }
            .enabled()
        })
    }

    #[cfg(feature = "local")]
    pub(crate) fn checkout_binding(
        &self,
        selected: &ResolvedRepository,
        worktree: &Path,
        consumer: CdnConsumer,
    ) -> Result<crab_auth::cdn_checkout::CdnCheckoutBinding> {
        self.validate_origin(&selected.store)?;
        let target_identity = selected
            .store
            .target_identity()
            .copied()
            .ok_or_else(|| invalid("CDN checkout requires a resolved physical target"))?;
        Ok(crab_auth::cdn_checkout::CdnCheckoutBinding {
            worktree: worktree.to_path_buf(),
            origin: self.deployment.origin.clone(),
            export_origin: match &self.access {
                Access::Public(export) => Some(export.clone()),
                Access::Private(_) => None,
            },
            publication: self.publication.clone(),
            approval_sha256: self.deployment.approval_sha256().map(str::to_owned),
            source_repo: self.deployment.source_repo.clone(),
            base_url: self.deployment.base_url.clone(),
            target_identity,
            consumer: match consumer {
                CdnConsumer::Sdk => crab_auth::cdn_checkout::CdnCheckoutConsumer::Sdk,
                CdnConsumer::Desktop => crab_auth::cdn_checkout::CdnCheckoutConsumer::Desktop,
                _ => return Err(invalid("CDN checkout requires an SDK or desktop read")),
            },
            scope: selected.store.storage_scope().cloned(),
            issuer: match &self.access {
                Access::Public(_) => None,
                Access::Private(issuer) => Some(crab_auth::cdn_checkout::CdnCheckoutIssuer {
                    config: issuer.0.clone(),
                    principal: selected.checkout_principal.ok_or_else(|| {
                        invalid("private CDN checkout requires a captured principal")
                    })?,
                }),
            },
        })
    }

    pub(crate) async fn resolve(
        &self,
        mut selected: ResolvedRepository,
        consumer: CdnConsumer,
        cancel: &CancellationToken,
    ) -> Result<ResolvedRepository> {
        if selected.managed || selected.prefix != self.deployment.source_repo {
            return Ok(selected);
        }
        if cancel.is_cancelled() {
            return Err(Error::new(
                ErrorKind::Cancelled,
                "CDN authorization cancelled",
            ));
        }
        self.validate_origin(&selected.store)?;
        let timeout = Duration::from_secs(300);
        match &self.access {
            Access::Public(export) => {
                let Some(publication) = &self.publication else {
                    return Ok(selected);
                };
                let target =
                    selected.store.target_identity().copied().ok_or_else(|| {
                        invalid("CDN binding requires a resolved physical target")
                    })?;
                let transport = CdnTransport::new(&self.deployment.base_url).map_err(|source| {
                    Error::with_source(ErrorKind::InvalidInput, "invalid CDN transport", source)
                })?;
                let route = CdnRoute::new(
                    target,
                    selected.prefix.clone(),
                    None,
                    CdnAccess::Public,
                    transport,
                    timeout,
                )
                .for_consumer(consumer, CdnProfile::Direct)
                .with_publication(&selected.store, publication, export)
                .await
                .map_err(|source| {
                    Error::with_source(
                        ErrorKind::InvalidInput,
                        "invalid public CDN publication approval",
                        source,
                    )
                })?;
                selected.store = selected.store.with_cdn_route(route).map_err(|source| {
                    Error::with_source(
                        ErrorKind::InvalidInput,
                        "cannot attach public CDN route",
                        source,
                    )
                })?;
            }
            Access::Private(issuer) => {
                let provider = Arc::new(
                    crab_auth::create_crab_auth_provider(issuer.0.clone())
                        .and_then(|provider| provider.with_cdn(self.deployment.clone()))
                        .map_err(|source| {
                            Error::with_source(
                                ErrorKind::Authentication,
                                "cannot open CDN issuer",
                                source,
                            )
                        })?
                        .with_viewer_grants(self.edge_enabled(consumer)),
                );
                let resolution = tokio::select! {
                    biased;
                    () = cancel.cancelled() => return Err(Error::new(ErrorKind::Cancelled, "CDN authorization cancelled")),
                    result = provider.resolve(&self.deployment.bucket, &selected.prefix, READ_OPERATION) => result
                        .map_err(|source| Error::with_source(ErrorKind::Authentication, "CDN issuer rejected repository read", source))?,
                };
                #[cfg(feature = "local")]
                {
                    selected.checkout_principal = provider.cdn_principal();
                }
                return self.bind_issuer_store(selected, provider, resolution, timeout, consumer);
            }
        }
        Ok(selected)
    }

    fn bind_issuer_store<P: CredentialProvider + 'static>(
        &self,
        mut selected: ResolvedRepository,
        provider: Arc<P>,
        resolution: CredentialResolution,
        timeout: Duration,
        consumer: CdnConsumer,
    ) -> Result<ResolvedRepository> {
        let store = crab_auth_store::cdn::build_direct_cdn_store(
            provider,
            self.deployment.bucket.clone(),
            self.deployment.source_repo.clone(),
            READ_OPERATION.to_owned(),
            resolution,
            timeout,
            consumer,
        )
        .map_err(|source| {
            Error::with_source(
                ErrorKind::Authentication,
                "cannot open the issuer's direct origin",
                source,
            )
        })?;
        self.validate_origin(&store)?;
        if store.target_identity() != selected.store.target_identity() {
            return Err(invalid("CDN issuer selected a different physical target"));
        }
        if let Some(scope) = store.storage_scope() {
            selected.prefix.clone_from(&scope.repo_prefix);
        }
        selected.store = store;
        // The issuer can select a narrower principal than the direct options.
        // Isolate its cache until there is a stable issuer-bound principal key.
        selected.namespace = format!("direct-issuer:{}", uuid::Uuid::now_v7().simple());
        Ok(selected)
    }
}

fn invalid(message: &'static str) -> Error {
    Error::new(ErrorKind::InvalidInput, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Client, RepositoryLocator,
        storage::{AzureOptions, DirectStoreOptions, GcsOptions, S3Options},
    };
    use object_store::path::Path as ObjectPath;

    #[tokio::test]
    async fn native_read_binding_is_exact_for_each_provider_and_origin_only_for_mutations() {
        let targets = [
            (
                DirectStoreOptions::s3(
                    S3Options::new("models", "us-east-1", "test-id", "test-key").unwrap(),
                ),
                "s3://models",
                "s3://approved-export",
            ),
            (
                DirectStoreOptions::gcs(GcsOptions::new("models", "test-token").unwrap()),
                "gs://models",
                "gs://approved-export",
            ),
            (
                DirectStoreOptions::azure(
                    AzureOptions::bearer("sdkfixture", "models", "test-token").unwrap(),
                ),
                "https://sdkfixture.blob.core.windows.net/models",
                "https://sdkexport.blob.core.windows.net/models",
            ),
        ];
        let repo = RepositoryLocator::new("org/repo").unwrap();
        let sibling = RepositoryLocator::new("org/repository").unwrap();
        let cancel = CancellationToken::new();
        let hash = "a".repeat(64);
        let relative = format!("v2/history/aa/{hash}");
        let path = ObjectPath::from(format!("org/repo/{relative}"));
        let sibling_path = ObjectPath::from(format!("org/repository/v2/history/aa/{hash}"));
        let global = ObjectPath::from(format!(".crab/xorbs/aa/{hash}"));
        for (store, origin, export) in targets {
            let directory = tempfile::tempdir().unwrap();
            let publication = crab_storage::cdn::write_publication_fixture(
                directory.path().join("receipt.json"),
                origin,
                export,
                "org/repo",
                "https://downloads.example.com",
                &relative,
                6,
            )
            .unwrap();
            let options = DirectCdnOptions::public_export(
                origin,
                "org/repo",
                export,
                "https://downloads.example.com/",
                Some(publication),
            )
            .unwrap();
            let client = Client::builder()
                .direct_store(store)
                .direct_cdn(options.clone())
                .build()
                .unwrap();
            let mut selected = client.0.resolve_repository(&repo, &cancel).await.unwrap();
            assert_eq!(selected.store.native_cdn_origin(), Some(origin));
            selected.store = selected
                .store
                .with_native_cdn_origin(Some(origin.to_owned()));
            let selected = options
                .resolve(selected, CdnConsumer::Sdk, &cancel)
                .await
                .unwrap();
            assert_eq!(
                selected.store.cdn_attempt(&path).is_some(),
                crab_storage::cdn::qualification_enabled(),
                "{origin}"
            );
            assert!(selected.store.cdn_attempt(&global).is_none(), "{origin}");
            let other = client
                .0
                .resolve_read_repository(&sibling, &cancel)
                .await
                .unwrap();
            assert!(other.store.cdn_attempt(&sibling_path).is_none(), "{origin}");
            let strict_origin = client.0.resolve_repository(&repo, &cancel).await.unwrap();
            assert!(strict_origin.store.cdn_attempt(&path).is_none(), "{origin}");
        }
    }

    #[test]
    fn native_cdn_binding_rejects_custom_endpoints_and_azure_account_mismatch() {
        let options = DirectCdnOptions::public_export(
            "s3://models",
            "org/repo",
            "s3://approved-export",
            "https://downloads.example.com",
            None,
        )
        .unwrap();
        let custom = DirectStoreOptions::s3(
            S3Options::new("models", "us-east-1", "test-id", "test-key")
                .unwrap()
                .with_endpoint("https://storage.example.com"),
        );
        assert!(
            Client::builder()
                .direct_store(custom)
                .direct_cdn(options)
                .build()
                .is_err()
        );

        let options = DirectCdnOptions::public_export(
            "https://other.blob.core.windows.net/models",
            "org/repo",
            "https://sdkexport.blob.core.windows.net/approved-export",
            "https://downloads.example.com",
            None,
        )
        .unwrap();
        let azure = DirectStoreOptions::azure(
            AzureOptions::bearer("sdkfixture", "models", "test-token").unwrap(),
        );
        let Err(error) = Client::builder()
            .direct_store(azure)
            .direct_cdn(options)
            .build()
        else {
            panic!("the SDK accepted a different Azure storage account");
        };
        assert_eq!(
            error.to_string(),
            "CDN binding does not match the selected native origin"
        );
    }

    #[tokio::test]
    async fn cancelled_private_read_leaves_the_issuer_cache_untouched() {
        let directory = tempfile::tempdir().unwrap();
        let token_cache = directory.path().join("tokens");
        let issuer = CdnIssuerOptions::new(
            "https://auth.example.com/v1/credentials",
            "https://identity.example.com",
            "crab-fixture",
            &token_cache,
        )
        .unwrap();
        let options = DirectCdnOptions::private_export(
            "gs://models",
            "org/repo",
            "https://downloads.example.com",
            issuer,
            None,
        )
        .unwrap();
        let client = Client::builder()
            .direct_store(DirectStoreOptions::gcs(
                GcsOptions::new("models", "test-token").unwrap(),
            ))
            .direct_cdn(options)
            .build()
            .unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let repo = RepositoryLocator::new("org/repo").unwrap();
        let result = client.0.resolve_read_repository(&repo, &cancel).await;
        assert!(matches!(result, Err(error) if error.kind() == ErrorKind::Cancelled));
        assert!(!token_cache.exists());
    }

    #[tokio::test]
    async fn private_read_without_viewer_grant_keeps_the_issuer_projection() {
        let directory = tempfile::tempdir().unwrap();
        let issuer = CdnIssuerOptions::new(
            "https://auth.example.com/v1/credentials",
            "https://identity.example.com",
            "crab-fixture",
            directory.path(),
        )
        .unwrap();
        let options = DirectCdnOptions::private_export(
            "gs://models",
            "org/repo",
            "https://downloads.example.com",
            issuer,
            None,
        )
        .unwrap();
        let client = Client::builder()
            .direct_store(DirectStoreOptions::gcs(
                GcsOptions::new("models", "fixture-ambient-token").unwrap(),
            ))
            .build()
            .unwrap();
        let repo = RepositoryLocator::new("org/repo").unwrap();
        let initial = client
            .0
            .resolve_repository(&repo, &CancellationToken::new())
            .await
            .unwrap();
        let initial_namespace = initial.namespace.clone();
        let scope_hash = "d".repeat(64);
        let physical_prefix = format!("org/repo/acl-views/v1/{scope_hash}/1-fixture");
        let scope = crab_storage::StorageScope {
            repo_prefix: physical_prefix.clone(),
            global_prefix: format!("{physical_prefix}/.crab"),
            source_repo: "org/repo".to_owned(),
            scope_hash,
        };
        let provider =
            crab_auth::static_credentials::StaticProvider::new(crab_auth::StaticAuthConfig {
                storage_provider: crab_storage::StorageProviderKind::Gcs,
            });
        let resolution = CredentialResolution {
            credentials: crab_auth::CloudCredentials::Gcp {
                access_token: "fixture-projected-token".into(),
                expires_at: std::time::SystemTime::now() + Duration::from_secs(120),
            },
            storage_scope: Some(scope.clone()),
            cdn: None,
        };
        let selected = options
            .bind_issuer_store(
                initial,
                Arc::new(provider),
                resolution,
                Duration::from_secs(30),
                CdnConsumer::Sdk,
            )
            .unwrap();

        assert_eq!(selected.prefix, physical_prefix);
        assert_eq!(selected.store.storage_scope(), Some(&scope));
        assert_ne!(selected.namespace, initial_namespace);

        #[cfg(feature = "local")]
        {
            let mut selected = selected;
            assert!(
                options
                    .checkout_binding(&selected, directory.path(), CdnConsumer::Sdk)
                    .is_err()
            );
            selected.checkout_principal = Some([9; 32]);
            let binding = options
                .checkout_binding(&selected, directory.path(), CdnConsumer::Desktop)
                .unwrap();
            assert_eq!(binding.scope.as_ref(), Some(&scope));
            assert_eq!(binding.source_repo, "org/repo");
            assert_eq!(binding.issuer.as_ref().unwrap().principal, [9; 32]);
            assert_eq!(
                Some(&binding.target_identity),
                selected.store.target_identity()
            );
            let body = serde_json::to_string(&binding).unwrap();
            assert!(body.contains("\"consumer\":\"desktop\""));
            assert!(!body.contains("fixture-ambient-token"));
            assert!(!body.contains("fixture-projected-token"));
        }
    }
}
