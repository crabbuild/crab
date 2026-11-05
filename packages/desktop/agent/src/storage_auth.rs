use std::future::Future;
use std::time::{Duration, Instant};

use crab_sdk::storage::{AzureOptions, DirectStoreOptions, GcsOptions, S3Options};
use futures_util::TryStreamExt;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{AgentError, Result};

#[derive(Clone, Deserialize)]
pub struct Connection {
    bucket: String,
    #[serde(default)]
    region: String,
    #[serde(default)]
    endpoint: String,
    credentials: Credentials,
}

#[derive(Clone, Deserialize)]
#[serde(tag = "provider", rename_all = "lowercase")]
enum Credentials {
    S3 {
        access_key_id: String,
        secret_access_key: String,
        session_token: Option<String>,
    },
    Gcs {
        service_account_key: String,
    },
    Azure {
        account: String,
        access_key: Option<String>,
        sas_token: Option<String>,
    },
}

tokio::task_local! { static CONNECTION: Option<Connection>; }

pub async fn scope<F: Future>(connection: Option<Connection>, future: F) -> F::Output {
    CONNECTION.scope(connection, future).await
}

pub fn current() -> Option<Connection> {
    CONNECTION.try_with(Clone::clone).ok().flatten()
}

pub fn configure_command(command: &mut tokio::process::Command) -> Result<()> {
    if let Some(connection) = current() {
        connection
            .options()?
            .configure_command(command.as_std_mut())?;
    }
    Ok(())
}

impl Connection {
    pub fn provider(&self) -> &'static str {
        match &self.credentials {
            Credentials::S3 { .. } => "s3",
            Credentials::Gcs { .. } => "gcs",
            Credentials::Azure { .. } => "azure",
        }
    }

    pub fn options(&self) -> Result<DirectStoreOptions> {
        let options = match &self.credentials {
            Credentials::S3 {
                access_key_id,
                secret_access_key,
                session_token,
            } => {
                let mut options =
                    S3Options::new(&self.bucket, &self.region, access_key_id, secret_access_key)?;
                if let Some(token) = session_token {
                    options = options.with_session_token(token)?;
                }
                if !self.endpoint.is_empty() {
                    options = options.with_endpoint(&self.endpoint);
                }
                DirectStoreOptions::s3(options)
            }
            Credentials::Gcs {
                service_account_key,
            } => DirectStoreOptions::gcs(GcsOptions::service_account(
                &self.bucket,
                service_account_key,
            )?),
            Credentials::Azure {
                account,
                access_key,
                sas_token,
            } => {
                let mut options = match (access_key, sas_token) {
                    (Some(key), None) => AzureOptions::access_key(account, &self.bucket, key)?,
                    (None, Some(token)) => AzureOptions::sas(account, &self.bucket, token)?,
                    _ => {
                        return Err(AgentError::Other(
                            "Azure requires an account key or SAS token".into(),
                        ));
                    }
                };
                if !self.endpoint.is_empty() {
                    options = options.with_endpoint(&self.endpoint);
                }
                DirectStoreOptions::azure(options)
            }
        };
        Ok(options)
    }

    pub fn repository_prefix(&self, url: &str) -> Result<String> {
        let uri = url::Url::parse(url)?;
        if !uri.username().is_empty()
            || uri.password().is_some()
            || uri.query().is_some()
            || uri.fragment().is_some()
        {
            return Err(AgentError::Other(
                "Repository URLs must not contain credentials or query strings".into(),
            ));
        }
        let matches_provider = match &self.credentials {
            Credentials::S3 { .. } => matches!(uri.scheme(), "crab" | "s3"),
            Credentials::Gcs { .. } => matches!(uri.scheme(), "crab" | "gs" | "gcs"),
            Credentials::Azure { .. } => matches!(uri.scheme(), "crab" | "az" | "azure"),
        };
        if !matches_provider {
            return Err(AgentError::Other(
                "The repository URL uses a different storage provider".into(),
            ));
        }
        let parsed = crab_git::RepositoryUrl::parse(url)?;
        let prefix = match &self.credentials {
            Credentials::Azure { account, .. } => {
                if uri.scheme() == "crab" {
                    if parsed.bucket != self.bucket {
                        return Err(AgentError::Other(
                            "The repository container does not match the selected connection"
                                .into(),
                        ));
                    }
                    parsed.repo_prefix
                } else {
                    let (container, prefix) =
                        parsed.repo_prefix.split_once('/').ok_or_else(|| {
                            AgentError::Other(
                                "Azure repositories use az://account/container/repository".into(),
                            )
                        })?;
                    if &parsed.bucket != account || container != self.bucket {
                        return Err(AgentError::Other("The repository account or container does not match the selected connection".into()));
                    }
                    prefix.to_owned()
                }
            }
            Credentials::S3 { .. } | Credentials::Gcs { .. } => {
                if parsed.bucket != self.bucket {
                    return Err(AgentError::Other(
                        "The repository's bucket does not match the selected connection".into(),
                    ));
                }
                parsed.repo_prefix
            }
        };
        Ok(prefix)
    }

    pub fn cli_url(&self, url: &str) -> Result<String> {
        let prefix = self.repository_prefix(url)?;
        Ok(format!("crab://{}/{}", self.bucket, prefix))
    }

    async fn probe(&self) -> Result<()> {
        use crab_storage::{AzureAuthorization, ObjectStoreCredentials};
        let credentials = match &self.credentials {
            Credentials::S3 {
                access_key_id,
                secret_access_key,
                session_token,
            } => ObjectStoreCredentials::Aws {
                access_key_id: access_key_id.clone(),
                secret_access_key: secret_access_key.clone(),
                session_token: session_token.clone(),
                region: self.region.clone(),
            },
            Credentials::Gcs {
                service_account_key,
            } => ObjectStoreCredentials::GcpServiceAccount {
                service_account_key: service_account_key.clone(),
            },
            Credentials::Azure {
                account,
                access_key,
                sas_token,
            } => {
                let token = match (access_key, sas_token) {
                    (Some(key), None) => AzureAuthorization::AccessKey(key.clone()),
                    (None, Some(token)) => AzureAuthorization::Sas(token.clone()),
                    _ => {
                        return Err(AgentError::Other(
                            "Choose one Azure authentication method".into(),
                        ));
                    }
                };
                ObjectStoreCredentials::Azure {
                    account: account.clone(),
                    token,
                }
            }
        };
        let endpoint = (!self.endpoint.is_empty()).then_some(self.endpoint.as_str());
        let store = crab_storage::build_explicit_store(
            &self.bucket,
            credentials,
            endpoint,
            self.endpoint.starts_with("http://"),
        )
        .map_err(|_| AgentError::Other("Invalid provider configuration".into()))?;
        let mut objects = store.list_stream(&object_store::path::Path::from(""));
        objects.try_next().await.map_err(|_| {
            AgentError::Other(
                "Cannot list this bucket. Check credentials, endpoint, and read/list permissions."
                    .into(),
            )
        })?;
        Ok(())
    }
}

pub async fn test_connection() -> Result<Value> {
    let connection =
        current().ok_or_else(|| AgentError::Other("Choose a storage connection".into()))?;
    let start = Instant::now();
    let outcome = tokio::time::timeout(Duration::from_secs(30), connection.probe()).await;
    let error = match outcome {
        Ok(Ok(())) => None,
        // Provider errors can contain signed URLs. Return a credential-free
        // diagnostic rather than forwarding the underlying request text.
        Ok(Err(error)) => Some(error.to_string()),
        Err(_) => Some("Connection timed out. Check the endpoint and network.".to_owned()),
    };
    Ok(
        json!({ "ok": error.is_none(), "latency_ms": start.elapsed().as_millis() as u64, "error": error }),
    )
}

pub fn cloud_url(url: &str) -> bool {
    url.split_once("://")
        .is_some_and(|(scheme, _)| matches!(scheme, "crab" | "s3" | "gs" | "gcs" | "az" | "azure"))
}

pub fn command_url(url: &str) -> Result<String> {
    if !cloud_url(url) {
        return Ok(url.to_owned());
    }
    let parsed = url::Url::parse(url)?;
    if !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(AgentError::Other(
            "Put credentials in the connection form, not in repository URLs".into(),
        ));
    }
    if let Some(connection) = current() {
        return connection.cli_url(url);
    }
    if parsed.scheme() != "crab" {
        return Err(AgentError::Other("Select a saved connection for provider URLs, or use a configured crab:// remote with default credentials".into()));
    }
    Ok(url.to_owned())
}

pub async fn validate_target(
    state: &crate::handlers::State,
    method: &str,
    params: &Value,
) -> Result<()> {
    let Some(connection) = current() else {
        return Ok(());
    };
    let git = matches!(
        method,
        "push"
            | "git_fetch"
            | "git_fetch_branch"
            | "git_fetch_all"
            | "git_pull"
            | "git_pull_branch"
            | "git_pull_mode"
            | "git_push_branch"
            | "git_push_remote"
    );
    if !git
        && !matches!(
            method,
            "crab_add"
                | "crab_add_structured"
                | "crab_push"
                | "crab_push_structured"
                | "hydrate"
                | "dehydrate"
                | "crab_add_auto"
                | "crab_reset_files"
                | "storage_summary"
        )
    {
        return Ok(());
    }
    let work_dir: std::path::PathBuf = match params.get("repo_id").and_then(Value::as_str) {
        Some(id) => state.get_work_dir(id).await?,
        None => match params.get("cwd").and_then(Value::as_str) {
            Some(path) => path.into(),
            None => return Ok(()),
        },
    };
    if git {
        // Git may choose a branch upstream or a push URL independently of
        // origin. One selected identity must match every cloud transport.
        let output = crate::shellout::git_command()
            .args(["config", "--get-regexp", "^remote\\..*\\.(url|pushurl)$"])
            .current_dir(&work_dir)
            .kill_on_drop(true)
            .output()
            .await?;
        for row in String::from_utf8(output.stdout)?.lines() {
            if let Some((_, url)) = row.split_once(' ') {
                if cloud_url(url) {
                    connection.repository_prefix(url)?;
                }
            }
        }
        return Ok(());
    }
    let remote = params.get("remote").and_then(Value::as_str);
    let url = remote_url(Some(&work_dir), "", remote).await?;
    connection.repository_prefix(&url)?;
    Ok(())
}

async fn remote_url(
    work_dir: Option<&std::path::Path>,
    opened_url: &str,
    remote: Option<&str>,
) -> Result<String> {
    let Some(work_dir) = work_dir else {
        return Ok(opened_url.to_owned());
    };
    // Project configuration names Crab's data backend even when origin is a
    // forge. Explicit Git remote checks must inspect that named remote instead.
    if remote.is_none() {
        match tokio::fs::read_to_string(work_dir.join("crab.toml")).await {
            Ok(text) => {
                let project: toml::Value = toml::from_str(&text)?;
                if let Some(url) = project
                    .get("remote")
                    .and_then(|remote| remote.get("url"))
                    .and_then(toml::Value::as_str)
                {
                    return Ok(url.to_owned());
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    let output = crate::shellout::git_command()
        .args(["remote", "get-url", remote.unwrap_or("origin")])
        .current_dir(work_dir)
        .kill_on_drop(true)
        .output()
        .await?;
    if output.status.success() {
        return Ok(String::from_utf8(output.stdout)?.trim().to_owned());
    }
    Ok(String::new())
}

pub async fn repository_config(
    work_dir: Option<&std::path::Path>,
    opened_url: &str,
) -> Result<Value> {
    let url = remote_url(work_dir, opened_url, None).await?;
    let parsed = crab_git::RepositoryUrl::parse(&url).ok();
    let initialized = match work_dir {
        Some(directory) => tokio::fs::try_exists(directory.join("crab.toml")).await?,
        None => true,
    };
    let connection = current();
    let provider = match connection.as_ref() {
        Some(connection) => connection.provider(),
        None => match default_target(&url) {
            Ok(selection) => match selection.target {
                crab_storage::StaticEnvStoreTarget::Bucket { provider, .. } => match provider {
                    crab_storage::StorageProviderKind::S3 => "s3",
                    crab_storage::StorageProviderKind::Gcs => "gcs",
                    crab_storage::StorageProviderKind::Azure => "azure",
                    crab_storage::StorageProviderKind::Local => "local",
                },
                crab_storage::StaticEnvStoreTarget::AzureAccountContainer { .. } => "azure",
            },
            Err(_) => "unknown",
        },
    };
    Ok(json!({
        "configured": initialized && parsed.is_some(),
        "provider": parsed.as_ref().map(|_| provider),
        "bucket": connection.as_ref().map(|value| value.bucket.clone()).or_else(|| parsed.as_ref().map(|value| value.bucket.clone())),
        "prefix": parsed.as_ref().map(|value| value.repo_prefix.clone()),
        "region": connection.as_ref().map(|value| value.region.clone()),
        "remote_url": parsed.as_ref().map(|_| url),
        "has_credentials": connection.is_some(),
    }))
}

fn default_target(url: &str) -> Result<crab_storage::StaticEnvStoreTargetSelection> {
    use crab_storage::{StaticEnvStoreUrlForm, StaticEnvStoreUrlParts, StorageProviderKind};
    let parsed = crab_git::RepositoryUrl::parse(url)?;
    let scheme = url
        .split_once("://")
        .map(|value| value.0)
        .unwrap_or_default();
    let provider = match scheme {
        "crab" | "s3" => StorageProviderKind::S3,
        "gs" | "gcs" => StorageProviderKind::Gcs,
        "az" | "azure" => StorageProviderKind::Azure,
        _ => {
            return Err(AgentError::Other(
                "Choose an object-store repository URL".into(),
            ));
        }
    };
    Ok(crab_storage::static_env_target_selection(
        StaticEnvStoreUrlParts {
            provider,
            form: if scheme == "crab" {
                StaticEnvStoreUrlForm::Crab
            } else {
                StaticEnvStoreUrlForm::Raw
            },
            bucket: &parsed.bucket,
            prefix: &parsed.repo_prefix,
        },
        None,
        "",
    )?)
}

pub async fn test_repository(
    work_dir: Option<&std::path::Path>,
    opened_url: &str,
    remote: Option<&str>,
) -> Result<Value> {
    let url = remote_url(work_dir, opened_url, remote).await?;
    if let Some(connection) = current() {
        if let Err(error) = connection.repository_prefix(&url) {
            return Ok(json!({ "ok": false, "latency_ms": 0, "error": error.to_string() }));
        }
        return test_connection().await;
    }
    let start = Instant::now();
    let outcome = tokio::time::timeout(Duration::from_secs(30), async {
        let target = default_target(&url).map_err(|_| ())?;
        let store = crab_storage::build_static_env_target_store(target.target).map_err(|_| ())?;
        store
            .list_stream(&object_store::path::Path::from(target.repo_prefix))
            .try_next()
            .await
            .map_err(|_| ())?;
        Ok::<_, ()>(())
    })
    .await;
    let ok = matches!(outcome, Ok(Ok(())));
    Ok(
        json!({ "ok": ok, "latency_ms": start.elapsed().as_millis() as u64,
        "error": (!ok).then_some("Could not read the configured storage. Select a saved connection or check default credentials and read/list permissions.") }),
    )
}

#[cfg(test)]
mod tests;
