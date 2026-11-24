use std::{
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use crab_auth::{
    DiscoveryDocument,
    managed::{
        BearerToken, ManagedApiClient, ServiceProfile, managed_http_client, managed_token_cache_key,
    },
    token_cache::{CachedTokens, TokenCache},
};

use super::ManagedRepositoryError;

pub(super) struct ManagedLogin {
    profile: ServiceProfile,
    discovery: DiscoveryDocument,
    cache: TokenCache,
    subject: String,
}

impl ManagedLogin {
    pub(super) async fn load(
        cache_path: PathBuf,
        profile: &ServiceProfile,
        discovery: &DiscoveryDocument,
    ) -> Result<(Self, BearerToken), ManagedRepositoryError> {
        let auth_error = |source| ManagedRepositoryError::Auth {
            authority: profile.authority.clone(),
            source,
        };
        let cache = TokenCache::new(cache_path).map_err(auth_error)?;
        let (bearer, subject) = managed_access_token(profile, discovery, &cache, None, false)
            .await
            .map_err(auth_error)?;
        Ok((
            Self {
                profile: profile.clone(),
                discovery: discovery.clone(),
                cache,
                subject,
            },
            bearer,
        ))
    }

    pub(super) async fn connect(
        &self,
        force_refresh: bool,
    ) -> Result<ManagedApiClient, ManagedRepositoryError> {
        let (bearer, _) = managed_access_token(
            &self.profile,
            &self.discovery,
            &self.cache,
            Some(&self.subject),
            force_refresh,
        )
        .await
        .map_err(|source| ManagedRepositoryError::Auth {
            authority: self.profile.authority.clone(),
            source,
        })?;
        ManagedApiClient::new(&self.profile, &self.discovery, bearer).map_err(|source| {
            ManagedRepositoryError::Api {
                canonical_url: format!("crab://{}", self.profile.authority),
                source,
            }
        })
    }
}

async fn managed_access_token(
    profile: &ServiceProfile,
    discovery: &DiscoveryDocument,
    cache: &TokenCache,
    expected_subject: Option<&str>,
    force_refresh: bool,
) -> Result<(BearerToken, String), crab_auth::error::AuthError> {
    let cache_key = managed_token_cache_key(&profile.authority)?;
    let cached = cache.load(&cache_key)?.ok_or_else(|| {
        crab_auth::error::AuthError::CredentialsExpired(format!(
            "no managed login is available for {}; run `crab login https://{}`",
            profile.authority, profile.authority
        ))
    })?;
    let subject = TokenCache::oidc_identity(&cached.id_token, &discovery.oidc.issuer)?.subject;
    if expected_subject.is_some_and(|expected| expected != subject) {
        return Err(crab_auth::error::AuthError::CredentialsExpired(
            "cached managed login subject changed; resolve a new operation".into(),
        ));
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    if !force_refresh
        && cached
            .expires_at
            .is_some_and(|expires_at| expires_at > now.saturating_add(60))
        && let Some(access_token) = cached.access_token
    {
        let bearer = BearerToken::new(access_token).map_err(|_| {
            crab_auth::error::AuthError::InvalidManagedContract(
                "cached access token is invalid for managed API use".to_owned(),
            )
        })?;
        return Ok((bearer, subject));
    }
    let bearer =
        refresh_managed_access_token(profile, discovery, cache, &cache_key, cached).await?;
    Ok((bearer, subject))
}

async fn refresh_managed_access_token(
    profile: &ServiceProfile,
    discovery: &DiscoveryDocument,
    cache: &TokenCache,
    cache_key: &str,
    cached: CachedTokens,
) -> Result<BearerToken, crab_auth::error::AuthError> {
    let refresh_token = cached.refresh_token.as_deref().ok_or_else(|| {
        crab_auth::error::AuthError::CredentialsExpired(format!(
            "managed login for {} cannot be refreshed; run `crab login https://{}`",
            profile.authority, profile.authority
        ))
    })?;
    let http = managed_http_client(profile)?;
    let oidc = crab_auth::discover_with_client(&discovery.oidc.issuer, &http).await?;
    oidc.validate_for_issuer(&discovery.oidc.issuer)?;
    let tokens = crab_auth::refresh_tokens_with_client(
        &oidc.token_endpoint,
        &discovery.oidc.client_id,
        refresh_token,
        &http,
    )
    .await?;
    let bearer = BearerToken::new(tokens.access_token.as_str()).map_err(|_| {
        crab_auth::error::AuthError::InvalidManagedContract(
            "OIDC access token is invalid for managed API use".to_owned(),
        )
    })?;
    cache.store_refreshed_oidc_tokens(cache_key, &discovery.oidc.issuer, &cached, &tokens)?;
    Ok(bearer)
}
