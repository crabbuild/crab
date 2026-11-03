use std::process::Stdio;

use crab_auth::{PageCursor, ServiceProfile, ServiceProfileStore, service_profile_directory};
use crab_auth_store::{ManagedRepositoryError, ManagedRepositoryResolver};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::{AgentError, Result, SharedStdout, shellout};

const DEFAULT_PAGE_LIMIT: u16 = 50;
const MAX_CLI_JSON_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Deserialize)]
pub struct SelectProfileParams {
    authority: String,
}

#[derive(Debug, Deserialize)]
pub struct LoginParams {
    job_id: String,
    service_origin: String,
}

#[derive(Debug, Deserialize)]
pub struct OrganizationPageParams {
    #[serde(default)]
    service: Option<String>,
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default = "default_page_limit")]
    limit: u16,
}

#[derive(Debug, Deserialize)]
pub struct RepositoryPageParams {
    #[serde(default)]
    service: Option<String>,
    organization: String,
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default = "default_page_limit")]
    limit: u16,
}

#[derive(Debug, Deserialize)]
pub struct CreateRepositoryParams {
    service: String,
    repository: String,
}

fn profile_store() -> Result<ServiceProfileStore> {
    Ok(ServiceProfileStore::new(service_profile_directory(
        &crate::repository::token_cache_directory()?,
    )))
}

pub fn profiles() -> Result<Value> {
    profiles_for(&profile_store()?)
}

pub fn select_profile(params: SelectProfileParams) -> Result<Value> {
    let store = profile_store()?;
    store.set_active(&params.authority).map_err(profile_error)?;
    profiles_for(&store)
}

pub async fn login(params: LoginParams, out: SharedStdout) -> Result<Value> {
    shellout::run_streaming(
        "managed-login",
        &params.job_id,
        None,
        &["login", &params.service_origin],
        out,
    )
    .await
    .map_err(|_| managed_error("login_failed", "managed service sign-in did not complete"))?;
    profiles()
}

pub async fn organizations(params: OrganizationPageParams) -> Result<Value> {
    let cursor = page_cursor(params.cursor)?;
    let cancel = CancellationToken::new();
    let service = ManagedRepositoryResolver::new(crate::repository::token_cache_directory()?)
        .connect(params.service.as_deref(), &cancel)
        .await
        .map_err(managed_repository_error)?;
    let page = service
        .client
        .list_organizations(cursor.as_ref(), params.limit)
        .await
        .map_err(|source| {
            managed_repository_error(ManagedRepositoryError::Api {
                canonical_url: service.authority.clone(),
                source,
            })
        })?;
    Ok(
        json!({ "authority": service.authority, "organizations": page.organizations, "next_cursor": page.next_cursor }),
    )
}

pub async fn repositories(params: RepositoryPageParams) -> Result<Value> {
    let cursor = page_cursor(params.cursor)?;
    let cancel = CancellationToken::new();
    let service = ManagedRepositoryResolver::new(crate::repository::token_cache_directory()?)
        .connect(params.service.as_deref(), &cancel)
        .await
        .map_err(managed_repository_error)?;
    let page = service
        .client
        .list_repositories(&params.organization, cursor.as_ref(), params.limit)
        .await
        .map_err(|source| {
            managed_repository_error(ManagedRepositoryError::Api {
                canonical_url: service.authority.clone(),
                source,
            })
        })?;
    Ok(
        json!({ "authority": service.authority, "repositories": page.repositories, "next_cursor": page.next_cursor }),
    )
}

pub async fn create_repository(params: CreateRepositoryParams) -> Result<Value> {
    let response = run_cli_json(&[
        "repo",
        "--service",
        &params.service,
        "--json",
        "create",
        &params.repository,
    ])
    .await?;
    if response.get("schema").and_then(Value::as_str) != Some("managed.repository") {
        return Err(managed_error(
            "invalid_service_response",
            "Crab returned an unexpected repository-create response",
        ));
    }
    response.get("data").cloned().ok_or_else(|| {
        managed_error(
            "invalid_service_response",
            "Crab returned no repository after creation",
        )
    })
}

fn profiles_for(store: &ServiceProfileStore) -> Result<Value> {
    let profiles = store.list().map_err(profile_error)?;
    let active_authority = store
        .active()
        .map_err(profile_error)?
        .map(|profile| profile.authority);
    let values = profiles
        .iter()
        .map(|profile| profile_value(profile, active_authority.as_deref()))
        .collect::<Vec<_>>();
    Ok(json!({ "profiles": values, "active_authority": active_authority }))
}

fn profile_value(profile: &ServiceProfile, active_authority: Option<&str>) -> Value {
    let discovery = profile.discovery.as_ref();
    json!({
        "authority": profile.authority,
        "discovery_origin": profile.discovery_origin,
        "active": active_authority == Some(profile.authority.as_str()),
        "discovered": discovery.is_some(),
        "service_version": discovery.map(|cached| cached.document.service_version.as_str()),
        "minimum_cli_version": discovery.map(|cached| cached.document.minimum_cli_version.as_str()),
    })
}

fn page_cursor(cursor: Option<String>) -> Result<Option<PageCursor>> {
    cursor
        .map(PageCursor::new)
        .transpose()
        .map_err(|_| managed_error("invalid_cursor", "managed catalog cursor is invalid"))
}

async fn run_cli_json(args: &[&str]) -> Result<Value> {
    let output = Command::new(shellout::resolve_crab_bin())
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await?;
    if output.stdout.len() > MAX_CLI_JSON_BYTES {
        return Err(managed_error(
            "invalid_service_response",
            "Crab structured output exceeded the desktop response limit",
        ));
    }
    let response: Value = serde_json::from_slice(&output.stdout).map_err(|_| {
        managed_error(
            "invalid_service_response",
            "Crab returned invalid structured output",
        )
    })?;
    if output.status.success() {
        return Ok(response);
    }
    Err(cli_managed_error(&response))
}

fn cli_managed_error(response: &Value) -> AgentError {
    let error = response.get("error").unwrap_or(&Value::Null);
    let details = error.get("details").unwrap_or(&Value::Null);
    let kind = details
        .get("kind")
        .and_then(Value::as_str)
        .or_else(|| error.get("code").and_then(Value::as_str))
        .unwrap_or("managed_request_failed");
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("managed repository request failed");
    managed_error(kind, message)
}

pub(crate) fn managed_repository_error(error: ManagedRepositoryError) -> AgentError {
    let diagnostic = error.diagnostic();
    let mut result = managed_error(diagnostic.kind(), diagnostic.to_string());
    if let AgentError::Managed { source, .. } = &mut result {
        *source = Some(Box::new(error));
    }
    result
}

fn profile_error(error: crab_auth::error::AuthError) -> AgentError {
    AgentError::Managed {
        kind: "invalid_profile".into(),
        message: "managed service profiles could not be read safely".into(),
        remediation: remediation("invalid_profile").into(),
        source: Some(Box::new(error)),
    }
}

fn managed_error(kind: impl Into<String>, message: impl Into<String>) -> AgentError {
    let kind = kind.into();
    AgentError::Managed {
        remediation: remediation(&kind).to_owned(),
        kind,
        message: message.into(),
        source: None,
    }
}

fn remediation(kind: &str) -> &'static str {
    match kind {
        "active_profile_missing" | "missing_profile" => {
            "Sign in to Crab Cloud or select an installed service profile."
        }
        "invalid_profile" | "discovery_failed" => {
            "Verify the service origin and trust settings, then sign in again."
        }
        "incompatible_api" | "invalid_service_response" => {
            "Upgrade Crab Desktop and the Crab CLI, or contact the service operator."
        }
        "login_required" | "login_failed" => "Sign in to the selected managed service and retry.",
        "forbidden" => "Ask an organization owner or administrator for repository access.",
        "not_found" => "Verify the repository name and your access with an organization owner.",
        "inactive" | "repository_provisioning" => {
            "Wait for the repository to become active, then refresh the catalog."
        }
        "repository_archived" => {
            "Ask an organization administrator to restore the repository before cloning."
        }
        "service_unavailable" => "Check the service status and network connection, then retry.",
        "invalid_cursor" => "Refresh the repository catalog from the first page.",
        _ => "Retry the operation; if it still fails, contact the service operator.",
    }
}

const fn default_page_limit() -> u16 {
    DEFAULT_PAGE_LIMIT
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_error_uses_safe_kind_and_actionable_remediation() {
        let error = cli_managed_error(&json!({
            "error": {
                "code": "CRAB-E0140",
                "message": "managed repository access is forbidden",
                "details": { "kind": "forbidden", "secret": "must-not-surface" }
            }
        }));

        let AgentError::Managed {
            kind,
            message,
            remediation,
            ..
        } = error
        else {
            panic!("expected managed error");
        };
        assert_eq!(kind, "forbidden");
        assert_eq!(message, "managed repository access is forbidden");
        assert!(remediation.contains("owner or administrator"));
        assert!(!remediation.contains("must-not-surface"));
    }

    #[test]
    fn invalid_catalog_cursor_has_refresh_remediation() {
        let error = page_cursor(Some("contains spaces".to_owned())).expect_err("invalid cursor");

        let AgentError::Managed {
            kind, remediation, ..
        } = error
        else {
            panic!("expected managed error");
        };
        assert_eq!(kind, "invalid_cursor");
        assert!(remediation.contains("Refresh"));
    }
}
