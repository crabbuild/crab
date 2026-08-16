//! `crab logout` — clear cached credentials and optionally revoke tokens.

use tracing::{debug, warn};

use crate::audit::{AuditEvent, AuditOutcome, NewAuditEvent, append_event, default_log_path};
use crate::auth::oidc;
use crate::core::config::Config;
use crate::core::error::{CrabError, Result};
use crab_auth::managed::{
    ServiceProfile, ServiceProfileStore, ServiceTrust, managed_http_client,
    managed_token_cache_key, service_profile_directory,
};
use crab_auth::token_cache::{TokenCache, expand_token_cache_path};

/// Run the `crab logout` command.
///
/// Loads cached tokens, attempts best-effort revocation at the IdP (if
/// a revocation endpoint exists), then deletes only the selected profile's
/// tokens. When `all` is true, every managed and provider token is deleted.
///
/// Revocation failure never prevents local deletion — the user's machine
/// is always cleaned up regardless of IdP reachability.
pub async fn run_logout(service: Option<String>, all: bool, config: &Config) -> Result<()> {
    let cache_dir = expand_token_cache_path(&config.auth.token_cache_path);
    let cache = TokenCache::new(cache_dir.clone())?;

    if all {
        debug!("logout --all: deleting tokens for all providers and managed profiles");
        cache.delete_all()?;
        eprintln!("Logged out from all providers and managed services");
        if let Err(err) = record_logout_audit("all", true, false) {
            warn!(%err, "failed to append auth logout audit event");
        }
        return Ok(());
    }

    let profiles = ServiceProfileStore::new(service_profile_directory(&cache_dir));
    let managed_profile = match service {
        Some(selector) => Some(load_selected_profile(&profiles, &selector)?),
        None => profiles.active()?,
    };
    if let Some(profile) = managed_profile {
        return logout_managed(&cache, profile).await;
    }

    run_provider_logout(&cache, config).await
}

async fn run_provider_logout(cache: &TokenCache, config: &Config) -> Result<()> {
    if !config.auth.provider.uses_token_cache() {
        let provider_name = config.auth.provider.as_str();
        debug!(provider = %provider_name, "logout is a no-op for non-token auth provider");
        eprintln!("Logged out ({provider_name})");
        if let Err(err) = record_logout_audit(provider_name, false, true) {
            warn!(%err, "failed to append auth logout audit event");
        }
        return Ok(());
    }

    let provider = config.auth.provider;
    let provider_name = provider.as_str();
    debug!(provider = %provider_name, "logging out");

    attempt_revocation(cache, provider.token_cache_keys(), config).await;
    cache.delete_all_names(provider.token_cache_keys())?;
    eprintln!("Logged out ({provider_name})");
    if let Err(err) = record_logout_audit(provider_name, false, false) {
        warn!(%err, "failed to append auth logout audit event");
    }
    Ok(())
}

fn load_selected_profile(profiles: &ServiceProfileStore, selector: &str) -> Result<ServiceProfile> {
    let authority = if selector.starts_with("https://") {
        ServiceProfile::from_origin(selector, ServiceTrust::default())?.authority
    } else {
        ServiceProfile::new(selector, ServiceTrust::default())?.authority
    };
    profiles
        .load(&authority)?
        .ok_or_else(|| CrabError::Configuration {
            key: format!("no managed service profile is installed for {authority}"),
            origin: "managed service profiles".to_owned(),
        })
}

async fn logout_managed(cache: &TokenCache, profile: ServiceProfile) -> Result<()> {
    let authority = profile.authority.clone();
    let token_key = managed_token_cache_key(&authority)?;
    attempt_managed_revocation(cache, &token_key, &profile).await;
    cache.delete(&token_key)?;
    eprintln!("Logged out ({authority})");
    if let Err(error) = record_logout_audit(&authority, false, false) {
        warn!(%error, "failed to append managed logout audit event");
    }
    Ok(())
}

async fn attempt_managed_revocation(cache: &TokenCache, token_key: &str, profile: &ServiceProfile) {
    let tokens = match cache.load(token_key) {
        Ok(Some(tokens)) => tokens,
        Ok(None) => return,
        Err(error) => {
            warn!(%error, "failed to load managed tokens for revocation, continuing with deletion");
            return;
        }
    };
    let Some(cached_discovery) = profile.discovery.as_ref() else {
        return;
    };
    let client = match managed_http_client(profile) {
        Ok(client) => client,
        Err(error) => {
            warn!(%error, "failed to build managed TLS client for revocation, continuing with deletion");
            return;
        }
    };
    let discovery = match crab_auth::oidc::discover_with_client(
        &cached_discovery.document.oidc.issuer,
        &client,
    )
    .await
    {
        Ok(discovery) => discovery,
        Err(error) => {
            warn!(%error, "managed OIDC discovery failed during logout, continuing with deletion");
            return;
        }
    };
    if let Err(error) = discovery.validate_for_issuer(&cached_discovery.document.oidc.issuer) {
        warn!(%error, "managed OIDC discovery validation failed during logout, continuing with deletion");
        return;
    }
    let Some(revocation_endpoint) = discovery.revocation_endpoint.as_deref() else {
        return;
    };
    let token = tokens.refresh_token.as_deref().unwrap_or(&tokens.id_token);
    if let Err(error) = crab_auth::oidc::revoke_token_with_client(
        revocation_endpoint,
        &cached_discovery.document.oidc.client_id,
        token,
        &client,
    )
    .await
    {
        warn!(%error, "managed token revocation failed, continuing with deletion");
    }
}

fn record_logout_audit(provider: &str, all: bool, no_op: bool) -> Result<()> {
    let event = AuditEvent::new(NewAuditEvent {
        operation: "auth.logout".to_owned(),
        outcome: AuditOutcome::Success,
        actor: None,
        repository: None,
        details: serde_json::json!({
            "provider": provider,
            "all": all,
            "no_op": no_op,
        }),
    });
    append_event(&default_log_path(), &event)
}

/// Attempt to revoke the refresh token at the IdP. Errors are logged
/// but never propagated — revocation is strictly best-effort.
async fn attempt_revocation(cache: &TokenCache, provider_names: &[&str], config: &Config) {
    let provider_name = provider_names.first().copied().unwrap_or("unknown");
    let tokens = match cache.load_any(provider_names) {
        Ok(Some(t)) => t,
        Ok(None) => {
            debug!(provider = %provider_name, "no cached tokens to revoke");
            return;
        }
        Err(e) => {
            warn!(error = %e, "failed to load tokens for revocation, continuing with deletion");
            return;
        }
    };

    // We need the issuer URL and client ID to discover the revocation endpoint.
    let Some(issuer_url) = config.auth.issuer_url.as_deref() else {
        debug!("no issuer_url configured, skipping revocation");
        return;
    };

    let Some(client_id) = config.auth.client_id.as_deref() else {
        debug!("no client_id configured, skipping revocation");
        return;
    };

    // Discover the revocation endpoint.
    let discovery = match oidc::discover(issuer_url).await {
        Ok(d) => d,
        Err(e) => {
            warn!(error = %e, "OIDC discovery failed during logout, skipping revocation");
            return;
        }
    };

    let Some(revocation_endpoint) = discovery.revocation_endpoint.as_deref() else {
        debug!("IdP has no revocation endpoint, skipping revocation");
        return;
    };

    // Prefer revoking the refresh token; fall back to the ID token.
    let token_to_revoke = tokens.refresh_token.as_deref().unwrap_or(&tokens.id_token);

    if let Err(e) = oidc::revoke_token(revocation_endpoint, client_id, token_to_revoke).await {
        warn!(error = %e, "token revocation failed (best-effort, continuing)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn expand_tilde_with_home() {
        let result = expand_token_cache_path("~/.config/crab/tokens/");
        assert!(!result.as_os_str().is_empty());
    }

    #[test]
    fn expand_tilde_without_tilde() {
        let result = expand_token_cache_path("/absolute/path");
        assert_eq!(result, PathBuf::from("/absolute/path"));
    }
}
