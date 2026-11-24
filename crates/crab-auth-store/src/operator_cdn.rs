//! Operator-owned CDN bindings for services with independent origin credentials.

use std::{sync::Arc, time::Duration};

use crab_auth::{
    CrabAuthClientConfig, CredentialProvider, cdn_checkout::validate_issuer,
    crab_auth_client::CdnViewerDeployment,
};
use crab_storage::{
    StorageError, Store,
    cdn::{
        CdnAccess, CdnConsumer, CdnPolicy, CdnProfile, CdnPublicationFile, CdnRoute, CdnTransport,
        CdnVisibility,
    },
};
use serde::{Deserialize, Serialize};

use crate::{AuthStoreError, cdn::build_direct_cdn_store};

/// Operator-owned viewer deployment for one repository's physical placement.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorCdn {
    origin: String,
    export_origin: Option<String>,
    publication: Option<CdnPublicationFile>,
    approval_sha256: Option<String>,
    base_url: String,
    #[serde(default)]
    access: Access,
    issuer: Option<Issuer>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Access {
    #[default]
    Private,
    Public,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Issuer {
    endpoint: String,
    oidc_issuer: String,
    client_id: String,
    token_cache_directory: String,
}

impl Issuer {
    fn config(&self, client_version: &str) -> CrabAuthClientConfig {
        CrabAuthClientConfig {
            endpoint: self.endpoint.clone(),
            issuer_url: self.oidc_issuer.clone(),
            client_id: self.client_id.clone(),
            token_cache_path: self.token_cache_directory.clone(),
            client_version: client_version.to_owned(),
        }
    }
}

/// Rejection while validating or authorizing an operator CDN binding.
#[derive(Debug, thiserror::Error)]
pub enum OperatorCdnError {
    #[error("invalid repository CDN binding: {0}")]
    Binding(&'static str),
    #[error("repository CDN issuer rejected the read")]
    Auth(#[from] crab_auth::error::AuthError),
    #[error("repository CDN credential construction failed")]
    Credentials(#[from] AuthStoreError),
    #[error("repository CDN read transport setup failed")]
    Storage(#[from] StorageError),
}

impl OperatorCdn {
    fn deployment(&self, repository_prefix: &str) -> Result<CdnViewerDeployment, OperatorCdnError> {
        let deployment = CdnViewerDeployment::new(&self.origin, repository_prefix, &self.base_url)?
            .with_approval(self.approval_sha256.as_deref())?;
        match (self.access, self.export_origin.as_deref()) {
            (Access::Public, Some(export)) if self.approval_sha256.is_none() => {
                deployment.validate_public_export(export)?;
            }
            (Access::Private, None) if self.publication.is_none() => {}
            _ => {
                return Err(OperatorCdnError::Binding(
                    "public CDN requires export_origin",
                ));
            }
        }
        Ok(deployment)
    }

    /// Validate operator settings and return the exact viewer repository path.
    pub fn validate(&self, repository_prefix: &str) -> Result<String, OperatorCdnError> {
        let deployment = self.deployment(repository_prefix)?;
        match (self.access, &self.issuer) {
            (Access::Private, Some(issuer)) => validate_issuer(&issuer.config(""))?,
            (Access::Public, None) => {}
            _ => {
                return Err(OperatorCdnError::Binding(
                    "private access requires an issuer and public access cannot use one",
                ));
            }
        }
        Ok(format!(
            "{}/{}",
            deployment.base_url, deployment.source_repo
        ))
    }

    /// Attach a viewer route to an independently authorized, full repository store.
    ///
    /// The configured issuer cannot change the physical target or select a
    /// filtered view. The service still owns its native origin credentials.
    pub async fn read_store(
        &self,
        origin: &Store,
        repository_prefix: &str,
        client_version: &str,
        timeout: Duration,
        consumer: CdnConsumer,
    ) -> Result<Store, OperatorCdnError> {
        let deployment = self.deployment(repository_prefix)?;
        let target = origin
            .target_identity()
            .copied()
            .ok_or(OperatorCdnError::Binding(
                "repository CDN requires a resolved native storage target",
            ))?;
        if origin.native_cdn_origin() != Some(deployment.origin.as_str())
            || origin.storage_scope().is_some()
        {
            return Err(OperatorCdnError::Binding(
                "repository CDN does not match the service's full native repository",
            ));
        }
        match (self.access, &self.issuer) {
            (Access::Public, None) => {
                let Some(publication) = &self.publication else {
                    return Ok(origin.clone().without_cdn_route());
                };
                let export = self
                    .export_origin
                    .as_deref()
                    .ok_or(OperatorCdnError::Binding(
                        "public CDN requires export_origin",
                    ))?;
                Ok(origin.clone().with_cdn_route(
                    CdnRoute::new(
                        target,
                        repository_prefix.to_owned(),
                        None,
                        CdnAccess::Public,
                        CdnTransport::new(&deployment.base_url)?,
                        timeout,
                    )
                    .for_consumer(consumer, CdnProfile::Direct)
                    .with_publication(
                        origin,
                        publication,
                        export,
                        Some(&crate::azure_cdn::AzureCdnAuthority),
                    )
                    .await?,
                )?)
            }
            (Access::Private, Some(issuer)) => {
                let config = issuer.config(client_version);
                validate_issuer(&config)?;
                if deployment.approval_sha256().is_none()
                    || !(CdnPolicy {
                        provider: origin.bucket_identity().cloud,
                        consumer,
                        profile: CdnProfile::Direct,
                        visibility: CdnVisibility::Private,
                    })
                    .enabled()
                {
                    return Ok(origin.clone().without_cdn_route());
                }
                let bucket = deployment.bucket.clone();
                let provider =
                    Arc::new(crab_auth::create_crab_auth_provider(config)?.with_cdn(deployment)?);
                let resolution = provider
                    .resolve(&bucket, repository_prefix, "fetch")
                    .await?;
                if resolution.storage_scope.is_some() {
                    return Err(OperatorCdnError::Binding(
                        "repository CDN issuer selected a filtered view",
                    ));
                }
                let authorized = build_direct_cdn_store(
                    provider,
                    bucket,
                    repository_prefix.to_owned(),
                    "fetch".into(),
                    resolution,
                    timeout,
                    consumer,
                )?;
                if authorized.target_identity() != Some(&target)
                    || authorized.native_cdn_origin() != origin.native_cdn_origin()
                {
                    return Err(OperatorCdnError::Binding(
                        "repository CDN issuer selected a different native target",
                    ));
                }
                Ok(origin.clone().inherit_cdn_route(&authorized))
            }
            _ => Err(OperatorCdnError::Binding(
                "private access requires an issuer and public access cannot use one",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use object_store::{memory::InMemory, path::Path};

    #[tokio::test]
    async fn service_cdn_matches_the_resolved_native_origin_and_repository() {
        use crab_storage::{BucketIdentity, StorageProviderKind};
        for (provider, origin, export) in [
            (
                StorageProviderKind::S3,
                "s3://team-models",
                "s3://approved-export",
            ),
            (
                StorageProviderKind::Gcs,
                "gs://team-models",
                "gs://approved-export",
            ),
            (
                StorageProviderKind::Azure,
                "https://teamaccount.blob.core.windows.net/team-models",
                "https://exportaccount.blob.core.windows.net/team-models",
            ),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let hash = "a".repeat(64);
            let relative = format!("v2/history/aa/{hash}");
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
            let store = Store::new(Arc::new(InMemory::new()))
                .with_target_identity([4; 32])
                .with_bucket_identity(BucketIdentity::new(provider, "team-models", "team-models"))
                .with_native_cdn_origin(Some(origin.into()));
            let mut settings = OperatorCdn {
                origin: origin.into(),
                export_origin: Some(export.into()),
                publication: Some(publication),
                approval_sha256: None,
                base_url: "https://downloads.example.com/".into(),
                access: Access::Public,
                issuer: None,
            };
            assert_eq!(
                settings.validate("org/repo").unwrap(),
                "https://downloads.example.com/org/repo"
            );
            let reader = settings
                .read_store(
                    &store,
                    "org/repo",
                    "test",
                    Duration::from_secs(30),
                    CdnConsumer::Server,
                )
                .await
                .unwrap();
            let key = |prefix| Path::from(format!("{prefix}/{relative}"));
            assert_eq!(
                reader.cdn_attempt(&key("org/repo")).is_some(),
                crab_storage::cdn::qualification_enabled()
            );
            assert!(reader.cdn_attempt(&key("org/repository")).is_none());
            assert!(store.cdn_attempt(&key("org/repo")).is_none());

            settings.origin = match origin {
                "s3://team-models" => "gs://team-models",
                "gs://team-models" => "s3://team-models",
                _ => "https://otheraccount.blob.core.windows.net/team-models",
            }
            .into();
            if provider != StorageProviderKind::Azure {
                settings.export_origin =
                    Some(settings.origin.replace("team-models", "approved-export"));
            }
            assert!(matches!(
                settings
                    .read_store(
                        &store,
                        "org/repo",
                        "test",
                        Duration::from_secs(30),
                        CdnConsumer::Server
                    )
                    .await,
                Err(OperatorCdnError::Binding(_)),
            ));
        }
    }
}
