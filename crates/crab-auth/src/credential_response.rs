//! Credential extraction from Crab Auth endpoint responses.

use std::time::SystemTime;

use crab_types::storage::StorageScope;
use serde::Deserialize;

use crate::credentials::{AzureReadScope, AzureToken, CloudCredentials};
use crate::error::{AuthError, Result};

/// Parsed and locally validated `/v1/credentials` response from Crab Auth.
#[derive(Debug, Clone)]
pub struct CrabAuthCredentialResponse {
    /// Cloud provider selected by the auth server.
    pub provider: String,
    /// Provider-specific credential JSON.
    pub credentials: serde_json::Value,
    /// Expiration timestamp in the server wire format.
    pub expires_at: String,
    /// Permission labels returned by the auth server.
    pub permissions: Vec<String>,
    /// Optional path-scoped view storage route.
    pub storage_scope: Option<StorageScope>,
}

impl CrabAuthCredentialResponse {
    /// Constructs cloud credentials with a caller-parsed expiration time.
    pub fn cloud_credentials(&self, expires_at: SystemTime) -> Result<CloudCredentials> {
        credentials_from_response(&self.provider, &self.credentials, expires_at)
    }
}

#[derive(Debug, Deserialize)]
struct RawCredentialResponse {
    provider: String,
    credentials: serde_json::Value,
    expires_at: String,
    permissions: Vec<String>,
    storage_scope: Option<RawStorageScope>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStorageScope {
    repo_prefix: String,
    global_prefix: String,
    source_repo: String,
    scope_hash: String,
}

/// Parses and validates a Crab Auth `/v1/credentials` response.
pub fn parse_credential_response(body: &str) -> Result<CrabAuthCredentialResponse> {
    let response: RawCredentialResponse = serde_json::from_str(body)
        .map_err(|source| AuthError::ParseCredentialResponse { source })?;
    let storage_scope = response
        .storage_scope
        .map(validate_storage_scope)
        .transpose()?;

    Ok(CrabAuthCredentialResponse {
        provider: response.provider,
        credentials: response.credentials,
        expires_at: response.expires_at,
        permissions: response.permissions,
        storage_scope,
    })
}

/// Constructs cloud credentials from a Crab Auth provider response.
pub fn credentials_from_response(
    provider: &str,
    credentials: &serde_json::Value,
    expires_at: SystemTime,
) -> Result<CloudCredentials> {
    match provider {
        "aws" => aws_credentials(credentials, expires_at, true),
        "s3" => aws_credentials(credentials, expires_at, false),
        "gcp" => gcp_credentials(credentials, expires_at),
        "azure" => azure_credentials(credentials, expires_at),
        other => invalid(format!(
            "unsupported provider in crab-auth response: {other}"
        )),
    }
}

fn aws_credentials(
    creds: &serde_json::Value,
    expires_at: SystemTime,
    require_session_token: bool,
) -> Result<CloudCredentials> {
    let access_key_id = string_field(creds, "access_key_id")?;
    let secret_access_key = string_field(creds, "secret_access_key")?;
    let session_token = optional_string_field(creds, "session_token");
    if require_session_token && session_token.is_none() {
        return invalid("crab-auth response missing credentials.session_token");
    }

    let region = optional_string_field(creds, "region").unwrap_or_else(|| "us-east-1".to_owned());

    Ok(CloudCredentials::Aws {
        access_key_id,
        secret_access_key,
        session_token,
        expires_at,
        region,
    })
}

fn gcp_credentials(creds: &serde_json::Value, expires_at: SystemTime) -> Result<CloudCredentials> {
    let access_token = string_field(creds, "access_token")?;

    Ok(CloudCredentials::Gcp {
        access_token,
        expires_at,
    })
}

fn azure_credentials(
    creds: &serde_json::Value,
    expires_at: SystemTime,
) -> Result<CloudCredentials> {
    if let (Some(write_sas), Some(write_prefix)) = (
        optional_string_field(creds, "write_sas_token"),
        optional_string_field(creds, "write_prefix"),
    ) {
        let account = string_field(creds, "storage_account")?;
        let read_sas_tokens = creds.get("read_sas_tokens").and_then(|v| v.as_array());
        if matches!(read_sas_tokens, Some(tokens) if tokens.is_empty()) {
            return invalid(
                "crab-auth response Azure protected credentials contain empty read scopes",
            );
        }

        let mut read_scopes = Vec::with_capacity(read_sas_tokens.map_or(0, Vec::len));
        for value in read_sas_tokens.into_iter().flatten() {
            let prefix = string_field(value, "prefix")?;
            let sas_token = string_field(value, "sas_token")?;
            read_scopes.push(AzureReadScope {
                prefix,
                token: AzureToken::Sas(sas_token),
            });
        }

        return Ok(CloudCredentials::AzureScoped {
            account,
            read_scopes,
            write_token: AzureToken::Sas(write_sas),
            write_prefix,
            expires_at,
        });
    }

    if let Some(sas) = optional_string_field(creds, "sas_token") {
        let account = string_field(creds, "storage_account")?;
        return Ok(CloudCredentials::Azure {
            account,
            token: AzureToken::Sas(sas),
            expires_at,
        });
    }

    if let Some(bearer) = optional_string_field(creds, "bearer_token") {
        let account = string_field(creds, "storage_account")?;
        return Ok(CloudCredentials::Azure {
            account,
            token: AzureToken::Bearer(bearer),
            expires_at,
        });
    }

    invalid("crab-auth response Azure credentials contain neither sas_token nor bearer_token")
}

fn string_field(creds: &serde_json::Value, field: &'static str) -> Result<String> {
    optional_string_field(creds, field).ok_or_else(|| {
        AuthError::InvalidCredentialResponse(format!(
            "crab-auth response missing credentials.{field}"
        ))
    })
}

fn optional_string_field(creds: &serde_json::Value, field: &'static str) -> Option<String> {
    creds
        .get(field)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn invalid<T>(message: impl Into<String>) -> Result<T> {
    Err(AuthError::InvalidCredentialResponse(message.into()))
}

fn validate_storage_scope(scope: RawStorageScope) -> Result<StorageScope> {
    validate_scope_component(&scope.repo_prefix, "storage_scope.repo_prefix")?;
    validate_scope_component(&scope.global_prefix, "storage_scope.global_prefix")?;
    validate_scope_component(&scope.source_repo, "storage_scope.source_repo")?;
    if scope.scope_hash.len() != 64 || !scope.scope_hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return invalid("crab-auth response returned invalid storage_scope.scope_hash");
    }
    let expected_global = format!("{}/.crab", scope.repo_prefix.trim_matches('/'));
    if scope.global_prefix.trim_matches('/') != expected_global {
        return invalid("crab-auth response storage_scope.global_prefix did not match repo_prefix");
    }
    Ok(StorageScope {
        repo_prefix: scope.repo_prefix.trim_matches('/').to_owned(),
        global_prefix: scope.global_prefix.trim_matches('/').to_owned(),
        source_repo: scope.source_repo.trim_matches('/').to_owned(),
        scope_hash: scope.scope_hash.to_ascii_lowercase(),
    })
}

fn validate_scope_component(value: &str, field: &str) -> Result<()> {
    let trimmed = value.trim_matches('/');
    if trimmed.is_empty()
        || trimmed != value
        || value.trim() != value
        || value.contains("//")
        || value.chars().any(char::is_control)
        || trimmed
            .split('/')
            .any(|segment| matches!(segment, "" | "." | ".."))
    {
        return invalid(format!("crab-auth response returned invalid {field}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use super::*;

    #[test]
    fn parse_credential_response_accepts_aws() {
        let json = r#"{
            "provider": "aws",
            "credentials": {
                "access_key_id": "ASIAXXXXXXXXXXX",
                "secret_access_key": "wJalrXUtnFEMI/K7MDENG",
                "session_token": "FwoGZXIvYXdzEBY",
                "region": "us-west-2"
            },
            "expires_at": "2026-04-24T18:00:00Z",
            "permissions": ["read", "write"]
        }"#;

        let response = parse_credential_response(json).unwrap();

        assert_eq!(response.provider, "aws");
        assert_eq!(response.expires_at, "2026-04-24T18:00:00Z");
        assert_eq!(response.permissions, vec!["read", "write"]);
        assert!(response.storage_scope.is_none());
    }

    #[test]
    fn parse_credential_response_rejects_malformed_json() {
        let err = parse_credential_response("not json").unwrap_err();

        assert!(matches!(err, AuthError::ParseCredentialResponse { .. }));
    }

    #[test]
    fn parse_credential_response_accepts_storage_scope() {
        let scope_hash = "c".repeat(64);
        let json = format!(
            r#"{{
                "provider": "gcp",
                "credentials": {{
                    "access_token": "ya29.test"
                }},
                "expires_at": "2026-04-24T18:00:00Z",
                "permissions": ["read"],
                "storage_scope": {{
                    "repo_prefix": "repo/acl-views/v1/{scope_hash}/7-deadbeef",
                    "global_prefix": "repo/acl-views/v1/{scope_hash}/7-deadbeef/.crab",
                    "source_repo": "repo",
                    "scope_hash": "{}"
                }}
            }}"#,
            scope_hash.to_ascii_uppercase()
        );

        let response = parse_credential_response(&json).unwrap();
        let scope = response.storage_scope.unwrap();

        assert_eq!(
            scope.repo_prefix,
            format!("repo/acl-views/v1/{}/7-deadbeef", "c".repeat(64))
        );
        assert_eq!(
            scope.global_prefix,
            format!("repo/acl-views/v1/{}/7-deadbeef/.crab", "c".repeat(64))
        );
        assert_eq!(scope.source_repo, "repo");
        assert_eq!(scope.scope_hash, "c".repeat(64));
    }

    #[test]
    fn parse_credential_response_rejects_storage_scope_outside_view() {
        let json = format!(
            r#"{{
                "provider": "gcp",
                "credentials": {{
                    "access_token": "ya29.test"
                }},
                "expires_at": "2026-04-24T18:00:00Z",
                "permissions": ["read"],
                "storage_scope": {{
                    "repo_prefix": "repo/acl-views/v1/view/7-deadbeef",
                    "global_prefix": ".crab",
                    "source_repo": "repo",
                    "scope_hash": "{}"
                }}
            }}"#,
            "d".repeat(64)
        );

        let err = parse_credential_response(&json).unwrap_err();

        assert!(matches!(err, AuthError::InvalidCredentialResponse(_)));
        assert!(err.to_string().contains("global_prefix"));
    }

    #[test]
    fn parse_credential_response_rejects_unknown_storage_scope_fields() {
        let json = format!(
            r#"{{
                "provider": "gcp",
                "credentials": {{
                    "access_token": "ya29.test"
                }},
                "expires_at": "2026-04-24T18:00:00Z",
                "permissions": ["read"],
                "storage_scope": {{
                    "repo_prefix": "repo/acl-views/v1/{scope_hash}/7-deadbeef",
                    "global_prefix": "repo/acl-views/v1/{scope_hash}/7-deadbeef/.crab",
                    "source_repo": "repo",
                    "scope_hash": "{scope_hash}",
                    "extra": true
                }}
            }}"#,
            scope_hash = "e".repeat(64)
        );

        let err = parse_credential_response(&json).unwrap_err();

        assert!(matches!(err, AuthError::ParseCredentialResponse { .. }));
    }

    #[test]
    fn parse_credential_response_rejects_invalid_scope_component() {
        let json = format!(
            r#"{{
                "provider": "gcp",
                "credentials": {{
                    "access_token": "ya29.test"
                }},
                "expires_at": "2026-04-24T18:00:00Z",
                "permissions": ["read"],
                "storage_scope": {{
                    "repo_prefix": "repo//view",
                    "global_prefix": "repo//view/.crab",
                    "source_repo": "repo",
                    "scope_hash": "{}"
                }}
            }}"#,
            "f".repeat(64)
        );

        let err = parse_credential_response(&json).unwrap_err();

        assert!(matches!(err, AuthError::InvalidCredentialResponse(_)));
        assert!(err.to_string().contains("repo_prefix"));
    }

    #[test]
    fn credential_response_constructs_cloud_credentials() {
        let json = r#"{
            "provider": "azure",
            "credentials": {
                "storage_account": "acct",
                "sas_token": "sv=2024-11-04&ss=b&srt=sco&sp=rl"
            },
            "expires_at": "2026-04-24T18:00:00Z",
            "permissions": ["read"]
        }"#;
        let response = parse_credential_response(json).unwrap();

        let creds = response.cloud_credentials(SystemTime::now()).unwrap();

        assert!(matches!(
            creds,
            CloudCredentials::Azure {
                token: AzureToken::Sas(_),
                ..
            }
        ));
    }

    #[test]
    fn aws_credentials_include_session_token() {
        let creds_json = serde_json::json!({
            "access_key_id": "ASIAXXXXXXXXXXX",
            "secret_access_key": "wJalrXUtnFEMI/K7MDENG",
            "session_token": "FwoGZXIvYXdzEBY",
            "region": "us-west-2"
        });
        let creds = credentials_from_response(
            "aws",
            &creds_json,
            SystemTime::now() + Duration::from_secs(3600),
        )
        .unwrap();

        match creds {
            CloudCredentials::Aws {
                access_key_id,
                secret_access_key,
                session_token,
                region,
                ..
            } => {
                assert_eq!(access_key_id, "ASIAXXXXXXXXXXX");
                assert_eq!(secret_access_key, "wJalrXUtnFEMI/K7MDENG");
                assert_eq!(session_token.as_deref(), Some("FwoGZXIvYXdzEBY"));
                assert_eq!(region, "us-west-2");
            }
            _ => panic!("expected Aws variant"),
        }
    }

    #[test]
    fn s3_credentials_allow_static_key_without_session_token() {
        let creds_json = serde_json::json!({
            "access_key_id": "crab",
            "secret_access_key": "crab"
        });
        let creds = credentials_from_response("s3", &creds_json, SystemTime::now()).unwrap();

        assert!(matches!(
            creds,
            CloudCredentials::Aws {
                session_token: None,
                region,
                ..
            } if region == "us-east-1"
        ));
    }

    #[test]
    fn aws_credentials_require_session_token() {
        let creds_json = serde_json::json!({
            "access_key_id": "AKIA",
            "secret_access_key": "secret"
        });
        let err = credentials_from_response("aws", &creds_json, SystemTime::now()).unwrap_err();

        assert!(matches!(err, AuthError::InvalidCredentialResponse(_)));
    }

    #[test]
    fn gcp_credentials_require_access_token() {
        let err = credentials_from_response("gcp", &serde_json::json!({}), SystemTime::now())
            .unwrap_err();

        assert!(matches!(err, AuthError::InvalidCredentialResponse(_)));
    }

    #[test]
    fn azure_sas_credentials_include_storage_account() {
        let creds_json = serde_json::json!({
            "storage_account": "acct",
            "sas_token": "sv=2024-11-04&ss=b&srt=sco&sp=rl"
        });
        let creds = credentials_from_response("azure", &creds_json, SystemTime::now()).unwrap();

        match creds {
            CloudCredentials::Azure { account, token, .. } => {
                assert_eq!(account, "acct");
                assert!(matches!(token, AzureToken::Sas(ref s) if s.starts_with("sv=")));
            }
            _ => panic!("expected Azure variant"),
        }
    }

    #[test]
    fn azure_bearer_credentials_include_storage_account() {
        let creds_json = serde_json::json!({
            "storage_account": "acct",
            "bearer_token": "eyJhbGciOiJSUzI1NiIs.test.token"
        });
        let creds = credentials_from_response("azure", &creds_json, SystemTime::now()).unwrap();

        match creds {
            CloudCredentials::Azure { account, token, .. } => {
                assert_eq!(account, "acct");
                assert!(matches!(token, AzureToken::Bearer(ref s) if s.contains("test.token")));
            }
            _ => panic!("expected Azure variant"),
        }
    }

    #[test]
    fn azure_scoped_credentials_parse_read_scopes() {
        let creds_json = serde_json::json!({
            "storage_account": "acct",
            "read_sas_tokens": [
                {
                    "prefix": "team/repo/manifest",
                    "sas_token": "sv=2024&sp=r"
                },
                {
                    "prefix": "team/repo/packs",
                    "sas_token": "sv=2024&sp=rl"
                }
            ],
            "write_sas_token": "sv=2024&sp=acw",
            "write_prefix": "team/repo/staging/0123456789abcdef0123456789abcdef"
        });
        let creds = credentials_from_response("azure", &creds_json, SystemTime::now()).unwrap();

        match creds {
            CloudCredentials::AzureScoped {
                account,
                read_scopes,
                write_token,
                write_prefix,
                ..
            } => {
                assert_eq!(account, "acct");
                assert_eq!(read_scopes.len(), 2);
                assert_eq!(read_scopes[0].prefix, "team/repo/manifest");
                assert!(
                    matches!(read_scopes[0].token, AzureToken::Sas(ref s) if s.contains("sp=r"))
                );
                assert_eq!(read_scopes[1].prefix, "team/repo/packs");
                assert!(
                    matches!(read_scopes[1].token, AzureToken::Sas(ref s) if s.contains("sp=rl"))
                );
                assert!(matches!(write_token, AzureToken::Sas(ref s) if s.contains("sp=acw")));
                assert_eq!(
                    write_prefix,
                    "team/repo/staging/0123456789abcdef0123456789abcdef"
                );
            }
            _ => panic!("expected AzureScoped variant"),
        }
    }

    #[test]
    fn azure_scoped_credentials_allow_staging_only_token() {
        let creds_json = serde_json::json!({
            "storage_account": "acct",
            "write_sas_token": "sv=2024&sp=acw",
            "write_prefix": "team/repo/staging/0123456789abcdef0123456789abcdef"
        });
        let creds = credentials_from_response("azure", &creds_json, SystemTime::now()).unwrap();

        match creds {
            CloudCredentials::AzureScoped {
                account,
                read_scopes,
                write_token,
                write_prefix,
                ..
            } => {
                assert_eq!(account, "acct");
                assert!(read_scopes.is_empty());
                assert!(matches!(write_token, AzureToken::Sas(ref s) if s.contains("sp=acw")));
                assert_eq!(
                    write_prefix,
                    "team/repo/staging/0123456789abcdef0123456789abcdef"
                );
            }
            _ => panic!("expected AzureScoped variant"),
        }
    }

    #[test]
    fn azure_sas_takes_precedence() {
        let creds_json = serde_json::json!({
            "storage_account": "acct",
            "sas_token": "sv=2024&ss=b",
            "bearer_token": "eyJ.test"
        });
        let creds = credentials_from_response("azure", &creds_json, SystemTime::now()).unwrap();

        assert!(matches!(
            creds,
            CloudCredentials::Azure {
                token: AzureToken::Sas(_),
                ..
            }
        ));
    }

    #[test]
    fn azure_credentials_require_storage_account() {
        let creds_json = serde_json::json!({
            "sas_token": "sv=2024&ss=b"
        });
        let err = credentials_from_response("azure", &creds_json, SystemTime::now()).unwrap_err();

        assert!(matches!(err, AuthError::InvalidCredentialResponse(_)));
    }

    #[test]
    fn azure_credentials_require_token() {
        let creds_json = serde_json::json!({
            "storage_account": "acct",
            "sas_token": "",
            "bearer_token": ""
        });
        let err = credentials_from_response("azure", &creds_json, SystemTime::now()).unwrap_err();

        assert!(matches!(err, AuthError::InvalidCredentialResponse(_)));
    }

    #[test]
    fn unsupported_provider_is_rejected() {
        let err = credentials_from_response("oracle", &serde_json::json!({}), SystemTime::now())
            .unwrap_err();

        assert!(matches!(err, AuthError::InvalidCredentialResponse(_)));
    }
}
