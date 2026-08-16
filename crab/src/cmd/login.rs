//! `crab login` — authenticate against a corporate identity provider.

use std::io::IsTerminal;
use std::path::PathBuf;
use tracing::{debug, warn};

use crate::audit::{AuditEvent, AuditOutcome, NewAuditEvent, append_event, default_log_path};
use crate::auth::oidc;
use crate::core::config::{AuthProvider, Config};
use crate::core::error::{CrabError, Result};
use crab_auth::managed::{
    EnterpriseCaReference, ManagedDiscoveryClient, ServiceProfile, ServiceProfileStore,
    ServiceTrust, managed_http_client, managed_token_cache_key, service_profile_directory,
};
use crab_auth::token_cache::{TokenCache, expand_token_cache_path};

pub struct LoginArgs {
    pub service: Option<String>,
    pub headless: bool,
    pub provider: Option<String>,
    pub enterprise_ca: Option<PathBuf>,
    pub private_ca_only: bool,
}

/// Run the `crab login` command.
///
/// Managed login discovers and activates an exact service profile. An explicit
/// `--provider` retains direct cloud-provider OIDC login.
pub async fn run_login(args: LoginArgs, config: &Config) -> Result<()> {
    if let Some(provider) = args.provider.clone() {
        return run_provider_login(provider, args.headless, config).await;
    }
    run_managed_login(args, config).await
}

async fn run_managed_login(args: LoginArgs, config: &Config) -> Result<()> {
    let origin = args.service.as_deref().unwrap_or("https://crab.build");
    let trust = ServiceTrust {
        system_roots: !args.private_ca_only,
        enterprise_ca: args
            .enterprise_ca
            .map(|pem_file| EnterpriseCaReference { pem_file }),
    };
    let profile = ServiceProfile::from_origin(origin, trust)?;
    let authority = profile.authority.clone();
    let cache_dir = expand_token_cache_path(&config.auth.token_cache_path);
    let profile_store = ServiceProfileStore::new(service_profile_directory(&cache_dir));
    let discovery = ManagedDiscoveryClient::new()
        .bootstrap(&profile_store, profile.clone())
        .await?;
    let required_flow = if args.headless || !std::io::stderr().is_terminal() {
        "device_authorization"
    } else {
        "authorization_code_pkce"
    };
    if !discovery
        .document
        .auth_flows
        .iter()
        .any(|flow| flow.as_str() == required_flow)
    {
        return Err(CrabError::AuthFailed {
            path: format!("managed service does not advertise {required_flow} login"),
        });
    }

    let client = managed_http_client(&profile)?;
    let oidc_discovery =
        crab_auth::oidc::discover_with_client(&discovery.document.oidc.issuer, &client).await?;
    oidc_discovery.validate_for_issuer(&discovery.document.oidc.issuer)?;
    let scopes = discovery.document.oidc.scopes.join(" ");
    let use_device_code = required_flow == "device_authorization";
    let tokens = if use_device_code {
        oidc::device_code_flow_with_client(
            &oidc_discovery,
            &discovery.document.oidc.client_id,
            &scopes,
            &client,
        )
        .await?
    } else {
        oidc::authorization_code_flow_with_client(
            &oidc_discovery,
            &discovery.document.oidc.client_id,
            &scopes,
            &client,
        )
        .await?
    };

    let cache = TokenCache::new(cache_dir)?;
    let token_key = managed_token_cache_key(&authority)?;
    cache.store_oidc_tokens(
        &token_key,
        &tokens.id_token,
        &tokens.access_token,
        tokens.refresh_token.as_deref(),
        tokens.expires_in,
    )?;
    if let Err(error) = profile_store.set_active(&authority) {
        let _ = cache.delete(&token_key);
        return Err(error.into());
    }

    let identity = TokenCache::parse_identity(&tokens.id_token)?;
    let display_name = identity.email.as_deref().unwrap_or(&identity.subject);
    eprintln!("Authenticated as {display_name} ({authority})");
    record_managed_login_audits(
        &authority,
        display_name,
        &discovery.document.oidc.issuer,
        required_flow,
        &scopes,
    );
    Ok(())
}

async fn run_provider_login(
    provider_override: String,
    headless: bool,
    config: &Config,
) -> Result<()> {
    // 1. Resolve provider.
    let provider = resolve_provider(Some(provider_override), config.auth.provider)?;

    // 2. Validate that login makes sense for this provider.
    validate_login_provider(provider)?;

    // 3. Validate required OIDC config.
    let issuer_url = config
        .auth
        .issuer_url
        .as_deref()
        .ok_or_else(|| CrabError::Configuration {
            key: "auth.issuer_url".into(),
            origin: "issuer_url is required for OIDC login".into(),
        })?;
    let client_id = config
        .auth
        .client_id
        .as_deref()
        .ok_or_else(|| CrabError::Configuration {
            key: "auth.client_id".into(),
            origin: "client_id is required for OIDC login".into(),
        })?;

    debug!(provider = %provider, issuer_url = %issuer_url, "starting login flow");

    // 4. Fetch OIDC discovery document.
    let discovery = oidc::discover(issuer_url).await?;

    // 5. Run the appropriate auth flow.
    let use_device_code = headless || !std::io::stderr().is_terminal();
    let tokens = if use_device_code {
        debug!("using device code flow (headless or non-TTY)");
        oidc::device_code_flow(&discovery, client_id, &config.auth.scopes).await?
    } else {
        debug!("using authorization code flow (desktop)");
        oidc::authorization_code_flow(&discovery, client_id, &config.auth.scopes).await?
    };

    // 6. Store tokens in the cache.
    let cache_dir = expand_token_cache_path(&config.auth.token_cache_path);
    let cache = TokenCache::new(cache_dir)?;
    let provider_name = provider.as_str();
    cache.store(
        provider_name,
        &tokens.id_token,
        tokens.refresh_token.as_deref(),
    )?;

    // 7. Display identity on stderr.
    let identity = TokenCache::parse_identity(&tokens.id_token)?;
    let display_name = identity.email.as_deref().unwrap_or(&identity.subject);
    eprintln!("Authenticated as {display_name} ({provider_name})");
    if let Err(err) = record_login_audit(
        provider_name,
        display_name,
        issuer_url,
        if use_device_code {
            "device_code"
        } else {
            "authorization_code"
        },
        &config.auth.scopes,
    ) {
        warn!(%err, "failed to append auth login audit event");
    }
    if let Err(err) = record_auth_grant_audit(
        provider_name,
        display_name,
        issuer_url,
        if use_device_code {
            "device_code"
        } else {
            "authorization_code"
        },
        &config.auth.scopes,
    ) {
        warn!(%err, "failed to append auth grant audit event");
    }

    Ok(())
}

fn record_managed_login_audits(
    authority: &str,
    actor: &str,
    issuer_url: &str,
    flow: &str,
    scopes: &str,
) {
    if let Err(error) = record_login_audit(authority, actor, issuer_url, flow, scopes) {
        warn!(%error, "failed to append managed login audit event");
    }
    if let Err(error) = record_auth_grant_audit(authority, actor, issuer_url, flow, scopes) {
        warn!(%error, "failed to append managed auth grant audit event");
    }
}

fn record_login_audit(
    provider: &str,
    actor: &str,
    issuer_url: &str,
    flow: &str,
    scopes: &str,
) -> Result<()> {
    let event = AuditEvent::new(NewAuditEvent {
        operation: "auth.login".to_owned(),
        outcome: AuditOutcome::Success,
        actor: Some(actor.to_owned()),
        repository: None,
        details: serde_json::json!({
            "provider": provider,
            "issuer_url": issuer_url,
            "flow": flow,
            "scopes": scopes,
        }),
    });
    append_event(&default_log_path(), &event)
}

fn record_auth_grant_audit(
    provider: &str,
    actor: &str,
    issuer_url: &str,
    grant_type: &str,
    scopes: &str,
) -> Result<()> {
    let event = AuditEvent::new(NewAuditEvent {
        operation: "auth.grant".to_owned(),
        outcome: AuditOutcome::Success,
        actor: Some(actor.to_owned()),
        repository: None,
        details: serde_json::json!({
            "provider": provider,
            "issuer_url": issuer_url,
            "grant_type": grant_type,
            "scopes": scopes,
        }),
    });
    append_event(&default_log_path(), &event)
}

/// Resolve the effective auth provider from an optional CLI override.
fn resolve_provider(
    provider_override: Option<String>,
    configured: AuthProvider,
) -> Result<AuthProvider> {
    match provider_override {
        Some(name) => parse_provider(&name),
        None => Ok(configured),
    }
}

/// Parse a provider name string into an `AuthProvider`.
fn parse_provider(name: &str) -> Result<AuthProvider> {
    crab_auth::AuthProviderKind::parse(name).ok_or_else(|| CrabError::Configuration {
        key: "provider".into(),
        origin: format!("unknown auth provider: {name}"),
    })
}

/// Validate that the provider supports interactive login.
fn validate_login_provider(provider: AuthProvider) -> Result<()> {
    if provider.uses_token_cache() {
        return Ok(());
    }

    let origin = match provider {
        AuthProvider::Static => {
            "login is not needed for the 'static' provider — credentials come from environment variables"
        }
        AuthProvider::None => {
            "login is not needed for the 'none' provider — no authentication is configured"
        }
        _ => unreachable!("token-cache providers returned above"),
    };
    Err(CrabError::Configuration {
        key: "auth.provider".into(),
        origin: origin.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn resolve_provider_uses_override() {
        let configured = AuthProvider::Static;
        let result = resolve_provider(Some("aws-oidc".into()), configured).unwrap();
        assert_eq!(result, AuthProvider::AwsOidc);
    }

    #[test]
    fn resolve_provider_falls_back_to_configured() {
        let configured = AuthProvider::AzureEntra;
        let result = resolve_provider(None, configured).unwrap();
        assert_eq!(result, AuthProvider::AzureEntra);
    }

    #[test]
    fn resolve_provider_rejects_unknown() {
        let configured = AuthProvider::Static;
        let result = resolve_provider(Some("bogus".into()), configured);
        assert!(result.is_err());
    }

    #[test]
    fn parse_provider_all_variants() {
        assert_eq!(parse_provider("aws-oidc").unwrap(), AuthProvider::AwsOidc);
        assert_eq!(
            parse_provider("gcp-workload-identity").unwrap(),
            AuthProvider::GcpWorkloadIdentity
        );
        assert_eq!(
            parse_provider("azure-entra").unwrap(),
            AuthProvider::AzureEntra
        );
        assert_eq!(parse_provider("crab-auth").unwrap(), AuthProvider::CrabAuth);
        assert_eq!(parse_provider("static").unwrap(), AuthProvider::Static);
        assert_eq!(parse_provider("none").unwrap(), AuthProvider::None);
    }

    #[test]
    fn parse_provider_rejects_removed_names() {
        assert!(parse_provider("removed-enterprise-auth").is_err());
    }

    #[test]
    fn validate_login_rejects_static() {
        assert!(validate_login_provider(AuthProvider::Static).is_err());
    }

    #[test]
    fn validate_login_rejects_none() {
        assert!(validate_login_provider(AuthProvider::None).is_err());
    }

    #[test]
    fn validate_login_accepts_oidc_providers() {
        assert!(validate_login_provider(AuthProvider::AwsOidc).is_ok());
        assert!(validate_login_provider(AuthProvider::GcpWorkloadIdentity).is_ok());
        assert!(validate_login_provider(AuthProvider::AzureEntra).is_ok());
        assert!(validate_login_provider(AuthProvider::CrabAuth).is_ok());
    }

    #[test]
    fn expand_tilde_with_home() {
        // Just verify it doesn't panic and returns a PathBuf.
        let result = expand_token_cache_path("~/.config/crab/tokens/");
        assert!(!result.as_os_str().is_empty());
    }

    #[test]
    fn expand_tilde_without_tilde() {
        let result = expand_token_cache_path("/absolute/path");
        assert_eq!(result, PathBuf::from("/absolute/path"));
    }

    #[test]
    fn auth_provider_as_str_round_trips() {
        let variants = [
            (AuthProvider::AwsOidc, "aws-oidc"),
            (AuthProvider::GcpWorkloadIdentity, "gcp-workload-identity"),
            (AuthProvider::AzureEntra, "azure-entra"),
            (AuthProvider::CrabAuth, "crab-auth"),
            (AuthProvider::Static, "static"),
            (AuthProvider::None, "none"),
        ];
        for (variant, expected) in variants {
            assert_eq!(variant.as_str(), expected);
            assert_eq!(variant.to_string(), expected);
        }
    }
}
