use std::collections::HashSet;
use std::path::PathBuf;

use crab_auth::{AuthProviderKind, crab_auth_client::CdnViewerDeployment};
use crab_auth_store::ManagedRepositoryResolver;
use crab_sdk::storage::{
    CdnIssuerOptions, CdnPublicationFile, DirectCdnOptions, DirectStoreOptions,
};
use crab_sdk::{ClientBuilder, RepositoryLocator};
use crab_storage::StorageProviderKind;
use serde::Deserialize;

use crate::storage_auth::Connection;
use crate::{AgentError, Result};

pub(super) struct UserConfiguration(toml::Table);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Binding {
    origin: String,
    export_origin: Option<String>,
    publication: Option<CdnPublicationFile>,
    approval_sha256: Option<String>,
    repo_prefix: String,
    base_url: String,
    #[serde(default)]
    access: Access,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Access {
    Public,
    #[default]
    Private,
}

impl UserConfiguration {
    pub(super) fn load() -> Result<Self> {
        let home = dirs::home_dir()
            .ok_or_else(|| AgentError::Other("cannot determine home directory".into()))?;
        match std::fs::read_to_string(home.join(".config/crab/config.toml")) {
            Ok(text) => Ok(Self(toml::from_str(&text)?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(Self(toml::Table::new()))
            }
            Err(error) => Err(error.into()),
        }
    }

    pub(super) fn token_cache_directory(&self) -> PathBuf {
        let path = self
            .auth_value("token_cache_path")
            .unwrap_or("~/.config/crab/tokens/");
        crab_auth::token_cache::expand_token_cache_path(path)
    }

    pub(super) fn select(
        &self,
        url: &str,
        cache: PathBuf,
        connection: Option<&Connection>,
        builder: ClientBuilder,
    ) -> Result<(ClientBuilder, RepositoryLocator)> {
        let builder = builder.for_desktop_browsing();
        let mut crab_remote = None;
        if url.starts_with("crab://") {
            let resolver = ManagedRepositoryResolver::new(cache);
            match resolver
                .classify(url)
                .map_err(crate::managed_service::managed_repository_error)?
            {
                crab_git::RepositoryLocator::Managed(repo) => {
                    if connection.is_some() {
                        return Err(AgentError::Other("Managed repositories require their managed login. Select Use default credentials.".into()));
                    }
                    return Ok((
                        builder,
                        RepositoryLocator::managed(
                            &repo.authority,
                            &repo.organization,
                            &repo.repository,
                        )?,
                    ));
                }
                crab_git::RepositoryLocator::Direct(repo) => crab_remote = Some(repo),
            }
        }
        let (store, origin, prefix) = if let Some(connection) = connection {
            let prefix = connection.repository_prefix(url)?;
            (connection.options()?, connection.cdn_origin(), prefix)
        } else if let Some(repo) = crab_remote {
            let (store, origin) = match self.direct_provider()? {
                StorageProviderKind::S3 => env_s3(&repo.bucket)?,
                StorageProviderKind::Gcs => env_gcs(&repo.bucket)?,
                StorageProviderKind::Azure => {
                    let account = self
                        .0
                        .get("auth")
                        .and_then(|auth| auth.get("azure"))
                        .and_then(|azure| azure.get("storage_account"))
                        .and_then(toml::Value::as_str)
                        .map(str::to_owned)
                        .or_else(|| std::env::var("AZURE_STORAGE_ACCOUNT_NAME").ok())
                        .ok_or_else(|| {
                            AgentError::Other(
                                "A storage account is required for default Azure credentials"
                                    .into(),
                            )
                        })?;
                    env_azure(&account, &repo.bucket)?
                }
                StorageProviderKind::Local => {
                    return Err(AgentError::Other(
                        "Local storage is not a cloud repository provider".into(),
                    ));
                }
            };
            (store, origin, repo.repo_prefix)
        } else {
            let parsed = crab_git::RepositoryUrl::parse(url)?;
            let (store, origin, prefix) = match url.split_once("://").map(|(scheme, _)| scheme) {
                Some("s3") => {
                    let (store, origin) = env_s3(&parsed.bucket)?;
                    (store, origin, parsed.repo_prefix)
                }
                Some("gs" | "gcs") => {
                    let (store, origin) = env_gcs(&parsed.bucket)?;
                    (store, origin, parsed.repo_prefix)
                }
                Some("az" | "azure") => {
                    let (container, prefix) =
                        parsed.repo_prefix.split_once('/').ok_or_else(|| {
                            AgentError::Other(
                                "Azure URLs require an account, container, and repository".into(),
                            )
                        })?;
                    let (store, origin) = env_azure(&parsed.bucket, container)?;
                    (store, origin, prefix.to_owned())
                }
                _ => {
                    return Err(AgentError::Other(
                        "unsupported remote repository scheme".into(),
                    ));
                }
            };
            (store, origin, prefix)
        };
        let mut builder = builder.direct_store(store);
        if let Some(cdn) = self.cdn_options(&origin, &prefix, connection.is_some())? {
            builder = builder.direct_cdn(cdn);
        }
        Ok((builder, RepositoryLocator::new(&prefix)?))
    }

    fn cdn_options(
        &self,
        origin: &str,
        repo_prefix: &str,
        saved_connection: bool,
    ) -> Result<Option<DirectCdnOptions>> {
        let Some(entries) = self.0.get("cdn") else {
            return Ok(None);
        };
        let entries: Vec<Binding> = entries.clone().try_into().map_err(|_| invalid_binding())?;
        let mut seen = HashSet::new();
        let mut viewer_routes = HashSet::new();
        let mut selected = None;
        for entry in entries {
            let deployment =
                CdnViewerDeployment::new(&entry.origin, &entry.repo_prefix, &entry.base_url)
                    .and_then(|deployment| {
                        deployment.with_approval(entry.approval_sha256.as_deref())
                    })
                    .map_err(|_| invalid_binding())?;
            match (&entry.access, entry.export_origin.as_deref()) {
                (Access::Public, Some(export)) if deployment.approval_sha256().is_none() => {
                    deployment
                        .validate_public_export(export)
                        .map_err(|_| invalid_binding())?;
                }
                (Access::Private, None) if entry.publication.is_none() => {}
                _ => return Err(invalid_binding()),
            }
            if !seen.insert((entry.origin, entry.repo_prefix)) {
                return Err(AgentError::Other(
                    "Duplicate CDN binding for one origin and repository in user configuration"
                        .into(),
                ));
            }
            if !viewer_routes.insert(format!(
                "{}/{}",
                deployment.base_url, deployment.source_repo
            )) {
                return Err(AgentError::Other(
                    "A CDN viewer path is bound more than once in user configuration".into(),
                ));
            }
            if deployment.origin == origin && deployment.source_repo == repo_prefix {
                selected = Some((
                    deployment,
                    entry.access,
                    entry.export_origin,
                    entry.publication,
                ));
            }
        }
        let Some((deployment, access, export_origin, publication)) = selected else {
            return Ok(None);
        };
        let (origin, prefix, base) = (
            &deployment.origin,
            &deployment.source_repo,
            &deployment.base_url,
        );
        let cdn = match (access, export_origin) {
            (Access::Public, Some(export)) => {
                DirectCdnOptions::public_export(origin, prefix, &export, base, publication)?
            }
            (Access::Private, None) => {
                // A saved connection explicitly selects a different credential
                // source. Only default-mode reads may adopt the issuer principal.
                if saved_connection {
                    return Err(AgentError::Other(
                        "Private CDN reads use Crab Auth. Select Use default credentials.".into(),
                    ));
                }
                let issuer = self.private_issuer()?;
                DirectCdnOptions::private_export(
                    origin,
                    prefix,
                    base,
                    issuer,
                    deployment.approval_sha256(),
                )?
            }
            _ => return Err(invalid_binding()),
        };
        Ok(Some(cdn))
    }

    fn private_issuer(&self) -> Result<CdnIssuerOptions> {
        let missing = || {
            AgentError::Other(
                "Private CDN reads require a Crab Auth issuer in user configuration".into(),
            )
        };
        if self
            .auth_value("provider")
            .and_then(AuthProviderKind::parse)
            != Some(AuthProviderKind::CrabAuth)
        {
            return Err(missing());
        }
        Ok(CdnIssuerOptions::new(
            self.auth_value("auth_endpoint").ok_or_else(missing)?,
            self.auth_value("issuer_url").ok_or_else(missing)?,
            self.auth_value("client_id").ok_or_else(missing)?,
            self.token_cache_directory(),
        )?)
    }

    fn direct_provider(&self) -> Result<StorageProviderKind> {
        match self.auth_value("storage_provider") {
            Some(value) if value != "auto" => StorageProviderKind::parse_cloud_alias(value)
                .ok_or_else(|| {
                    AgentError::Other("Invalid storage provider in user configuration".into())
                }),
            _ => Ok(crab_storage::resolve_static_env_provider()?),
        }
    }

    fn auth_value(&self, key: &str) -> Option<&str> {
        self.0.get("auth")?.get(key)?.as_str()
    }
}

fn env_s3(bucket: &str) -> Result<(DirectStoreOptions, String)> {
    Ok((
        DirectStoreOptions::s3_from_env(bucket)?,
        format!("s3://{bucket}"),
    ))
}

fn env_gcs(bucket: &str) -> Result<(DirectStoreOptions, String)> {
    Ok((
        DirectStoreOptions::gcs_from_env(bucket)?,
        format!("gs://{bucket}"),
    ))
}

fn env_azure(account: &str, container: &str) -> Result<(DirectStoreOptions, String)> {
    Ok((
        DirectStoreOptions::azure_from_env(account, container)?,
        format!("https://{account}.blob.core.windows.net/{container}"),
    ))
}

fn invalid_binding() -> AgentError {
    AgentError::Other("Invalid CDN binding in user configuration".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crab_sdk::Client;
    use serde_json::json;

    fn configuration(text: &str) -> UserConfiguration {
        UserConfiguration(toml::from_str(text).unwrap())
    }

    fn saved_s3(endpoint: &str) -> Connection {
        serde_json::from_value(json!({
            "bucket": "desktop-models",
            "region": "us-east-1",
            "endpoint": endpoint,
            "credentials": {
                "provider": "s3",
                "access_key_id": "fixture",
                "secret_access_key": "fixture",
            }
        }))
        .unwrap()
    }

    #[test]
    fn cdn_binding_matches_provider_account_and_exact_prefix() {
        let config = configuration(
            r#"
            [[cdn]]
            origin = "s3://desktop-models"
            export_origin = "s3://approved-export"
            repo_prefix = "org/repo"
            base_url = "https://aws.example.com"
            access = "public"

            [[cdn]]
            origin = "gs://desktop-models"
            export_origin = "gs://approved-export"
            repo_prefix = "org/repo"
            base_url = "https://google.example.com"
            access = "public"

            [[cdn]]
            origin = "https://first.blob.core.windows.net/desktop-models"
            export_origin = "https://firstexport.blob.core.windows.net/approved-export"
            repo_prefix = "org/repo"
            base_url = "https://gateway.example.com/crab"
            access = "public"
        "#,
        );
        for origin in [
            "s3://desktop-models",
            "gs://desktop-models",
            "https://first.blob.core.windows.net/desktop-models",
        ] {
            assert!(
                config
                    .cdn_options(origin, "org/repo", true)
                    .unwrap()
                    .is_some()
            );
            assert!(
                config
                    .cdn_options(origin, "org/repo2", true)
                    .unwrap()
                    .is_none()
            );
        }
        assert!(
            config
                .cdn_options(
                    "https://second.blob.core.windows.net/desktop-models",
                    "org/repo",
                    true,
                )
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn cdn_options_reject_aliases_of_the_same_viewer_path() {
        let config = configuration(
            r#"
            [[cdn]]
            origin = "s3://desktop-models"
            export_origin = "s3://approved-export"
            repo_prefix = "org/repo"
            base_url = "https://edge.example.com"
            access = "public"

            [[cdn]]
            origin = "gs://desktop-models"
            export_origin = "gs://approved-export"
            repo_prefix = "repo"
            base_url = "https://edge.example.com/org"
            access = "public"
        "#,
        );
        assert!(
            config
                .cdn_options("s3://desktop-models", "org/repo", false)
                .unwrap_err()
                .to_string()
                .contains("viewer path is bound more than once")
        );
    }

    #[test]
    fn private_cdn_requires_default_mode_and_a_trusted_user_issuer() {
        let binding = r#"
            [[cdn]]
            origin = "gs://desktop-models"
            repo_prefix = "org/repo"
            base_url = "https://google.example.com"
        "#;
        let config = configuration(binding);
        assert!(
            config
                .cdn_options("gs://desktop-models", "org/repo", false)
                .is_err()
        );

        let fixture = tempfile::tempdir().unwrap();
        let cache = fixture.path().join("tokens");
        let config = configuration(&format!(
            r#"
            [auth]
            provider = "crab-auth"
            storage_provider = "gcs"
            auth_endpoint = "https://auth.example.com/v1/credentials"
            issuer_url = "https://oidc.example.com"
            client_id = "desktop-fixture"
            token_cache_path = {:?}
            {binding}
        "#,
            cache.to_str().unwrap(),
        ));
        assert_eq!(config.direct_provider().unwrap(), StorageProviderKind::Gcs);
        assert!(
            config
                .cdn_options("gs://desktop-models", "org/repo", false)
                .unwrap()
                .is_some()
        );
        assert!(
            config
                .cdn_options("gs://desktop-models", "org/repo", true)
                .is_err()
        );
    }

    #[tokio::test]
    async fn browse_builder_rejects_a_matching_binding_on_a_custom_endpoint() {
        let config = configuration(
            r#"
            [[cdn]]
            origin = "s3://desktop-models"
            export_origin = "s3://approved-export"
            repo_prefix = "org/repo"
            base_url = "https://edge.example.com"
            access = "public"
        "#,
        );
        let connection = saved_s3("https://custom.example.com");
        let fixture = tempfile::tempdir().unwrap();
        let target = |url| {
            config.select(
                url,
                fixture.path().join("tokens"),
                Some(&connection),
                Client::builder(),
            )
        };
        let (builder, _) = target("s3://desktop-models/org/repo").unwrap();
        let error = builder.build().err().unwrap();
        assert!(
            error
                .to_string()
                .contains("does not match the selected native origin")
        );
        let (builder, _) = target("s3://desktop-models/org/other").unwrap();
        builder.build().unwrap().close().await.unwrap();
    }

    #[test]
    fn browse_builder_keeps_managed_authority_out_of_direct_bindings() {
        use crab_auth::managed::{
            ServiceProfile, ServiceProfileStore, ServiceTrust, service_profile_directory,
        };

        let fixture = tempfile::tempdir().unwrap();
        let cache = fixture.path().join("tokens");
        let profiles = ServiceProfileStore::new(service_profile_directory(&cache));
        profiles
            .store(&ServiceProfile::new("desktop-models", ServiceTrust::default()).unwrap())
            .unwrap();
        // Direct CDN settings are irrelevant once the managed authority is selected.
        let config = configuration(r#"cdn = "invalid direct config""#);
        let url = "crab://desktop-models/org/repo";
        assert!(
            config
                .select(url, cache.clone(), None, Client::builder())
                .is_ok()
        );
        assert!(
            config
                .select(url, cache, Some(&saved_s3("")), Client::builder())
                .is_err()
        );
    }
}
