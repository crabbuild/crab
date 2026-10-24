use super::{AuthError, Client};
use crate::{OidcConfig, config::validate_identity_url};
use axum::http;
use openidconnect::{ClientId, ClientSecret, RedirectUrl, core::CoreProviderMetadata};
use url::Url;

pub(super) async fn discover(
    config: &OidcConfig,
    secret: Option<ClientSecret>,
    http: &reqwest::Client,
) -> Result<(Client, bool), AuthError> {
    let transport =
        |request| bounded_http(http.clone(), request, config.public_url.scheme() == "http");
    let issuer = config.issuer.clone();
    let metadata = CoreProviderMetadata::discover_async(issuer, &transport)
        .await
        .map_err(AuthError::provider)?;
    validate_identity_url(
        metadata.authorization_endpoint().url(),
        config.public_url.scheme() == "http",
    )
    .map_err(AuthError::provider)?;
    let token = metadata.token_endpoint().ok_or(AuthError::Invalid)?;
    validate_identity_url(token.url(), config.public_url.scheme() == "http")
        .map_err(AuthError::provider)?;
    let redirect = config
        .public_url
        .join("auth/callback")
        .map_err(AuthError::provider)?;
    let groups_scope = metadata
        .scopes_supported()
        .is_some_and(|scopes| scopes.iter().any(|scope| scope.as_str() == "groups"));
    let client =
        Client::from_provider_metadata(metadata, ClientId::new(config.client_id.clone()), secret)
            .set_redirect_uri(RedirectUrl::new(redirect.to_string()).map_err(AuthError::provider)?);
    Ok((client, groups_scope))
}

pub(super) async fn bounded_http(
    http: reqwest::Client,
    request: openidconnect::HttpRequest,
    allow_http: bool,
) -> Result<openidconnect::HttpResponse, AuthError> {
    let url = Url::parse(&request.uri().to_string()).map_err(AuthError::provider)?;
    validate_identity_url(&url, allow_http).map_err(AuthError::provider)?;
    let mut response = http
        .execute(request.try_into().map_err(AuthError::provider)?)
        .await
        .map_err(AuthError::provider)?;
    let mut builder = http::Response::builder()
        .status(response.status())
        .version(response.version());
    for (name, value) in response.headers() {
        builder = builder.header(name, value);
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(AuthError::provider)? {
        if body.len() + chunk.len() > 1024 * 1024 {
            return Err(AuthError::Invalid);
        }
        body.extend_from_slice(&chunk);
    }
    builder.body(body).map_err(AuthError::provider)
}
