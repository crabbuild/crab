use super::{
    AuthError, Authentication, FLOW_LIFETIME, Flow, Identity, Principal, SESSION_LIFETIME, Session,
    cookie_value, key, now_epoch,
    transport::{bounded_http, discover},
};
use crate::{AuthProvider, config::validate_identity_url, server::Server};
use axum::{
    Extension, Json,
    extract::{Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Redirect, Response},
};
use openidconnect::{
    AccessTokenHash, AuthorizationCode, CsrfToken, Nonce, OAuth2TokenResponse, PkceCodeChallenge,
    Scope, TokenResponse, core::CoreAuthenticationFlow,
};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::json;
use std::{
    sync::{Arc, atomic::AtomicBool},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use url::Url;

#[derive(Deserialize)]
pub(crate) struct LoginQuery {
    return_to: Option<String>,
}

pub(crate) async fn login(
    State(server): State<Arc<Server>>,
    Query(query): Query<LoginQuery>,
) -> Result<Response, AuthError> {
    let auth = server.auth.as_ref().ok_or(AuthError::Invalid)?;
    let return_to = query.return_to.unwrap_or_else(|| "/".into());
    validate_return_to(auth, &return_to)?;
    match auth.config.provider {
        AuthProvider::Oidc => login_oidc(auth, return_to).await,
        AuthProvider::GitHub => login_github(auth, return_to).await,
    }
}

fn validate_return_to(auth: &Authentication, return_to: &str) -> Result<(), AuthError> {
    if !return_to.starts_with('/')
        || return_to.starts_with("//")
        || return_to.contains('\\')
        || return_to.chars().any(char::is_control)
    {
        return Err(AuthError::Invalid);
    }
    let destination = auth
        .config
        .public_url
        .join(return_to)
        .map_err(AuthError::provider)?;
    if destination.origin() != auth.config.public_url.origin()
        || destination.path().starts_with("/auth/")
    {
        return Err(AuthError::Invalid);
    }
    Ok(())
}

async fn login_oidc(auth: &Authentication, return_to: String) -> Result<Response, AuthError> {
    let client = auth.client.as_ref().ok_or(AuthError::Invalid)?;
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let (url, state, nonce) = client
        .authorize_url(
            CoreAuthenticationFlow::AuthorizationCode,
            CsrfToken::new_random,
            Nonce::new_random,
        )
        .add_scope(Scope::new("profile".into()))
        .set_pkce_challenge(challenge)
        .url();
    auth.store_flow(
        key(state.secret()),
        Flow {
            provider: AuthProvider::Oidc,
            nonce,
            verifier,
            return_to,
            expires_at: now_epoch()? + FLOW_LIFETIME.as_secs(),
        },
    )
    .await?;
    Ok((
        [(
            header::SET_COOKIE,
            auth.cookie(true, state.secret(), FLOW_LIFETIME)?,
        )],
        Redirect::to(url.as_str()),
    )
        .into_response())
}

async fn login_github(auth: &Authentication, return_to: String) -> Result<Response, AuthError> {
    let github = auth.config.github.clone().unwrap_or_default();
    let authorize_url = Url::parse(&github.authorize_url).map_err(AuthError::provider)?;
    validate_identity_url(&authorize_url, auth.config.public_url.scheme() == "http")
        .map_err(AuthError::provider)?;
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let state = CsrfToken::new_random();
    let nonce = Nonce::new_random();
    let redirect = auth
        .config
        .public_url
        .join("auth/callback")
        .map_err(AuthError::provider)?;
    let mut url = authorize_url;
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("client_id", &auth.config.client_id)
            .append_pair("redirect_uri", redirect.as_str())
            .append_pair("response_type", "code")
            .append_pair("scope", "read:user")
            .append_pair("state", state.secret())
            .append_pair("code_challenge", challenge.as_str())
            .append_pair("code_challenge_method", "S256");
    }
    auth.store_flow(
        key(state.secret()),
        Flow {
            provider: AuthProvider::GitHub,
            nonce,
            verifier,
            return_to,
            expires_at: now_epoch()? + FLOW_LIFETIME.as_secs(),
        },
    )
    .await?;
    Ok((
        [(
            header::SET_COOKIE,
            auth.cookie(true, state.secret(), FLOW_LIFETIME)?,
        )],
        Redirect::to(url.as_str()),
    )
        .into_response())
}

#[derive(Deserialize)]
pub(crate) struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
}

pub(crate) async fn callback(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    Query(query): Query<CallbackQuery>,
) -> Response {
    let result = match server.auth.as_ref().map(|auth| auth.config.provider) {
        Some(AuthProvider::Oidc) => finish_login(&server, headers, query).await,
        Some(AuthProvider::GitHub) => finish_github_login(&server, headers, query).await,
        None => Err(AuthError::Invalid),
    };
    match result {
        Ok(response) => response,
        Err(_) => Redirect::to("/?auth_error=sign_in_failed").into_response(),
    }
}

async fn finish_login(
    server: &Server,
    headers: HeaderMap,
    query: CallbackQuery,
) -> Result<Response, AuthError> {
    let auth = server.auth.as_ref().ok_or(AuthError::Invalid)?;
    let _permit = auth.admission.try_acquire().map_err(|_| AuthError::Busy)?;
    let state = query.state.ok_or(AuthError::Invalid)?;
    let cookie = cookie_value(&headers, auth.cookie_name(true)).ok_or(AuthError::Invalid)?;
    if blake3::hash(state.as_bytes()) != blake3::hash(cookie.as_bytes()) {
        return Err(AuthError::Invalid);
    }
    // Consume only after binding the state to this browser. Replays cannot exchange a code.
    let flow = auth.take_flow(key(&state)).await?;
    if flow.provider != AuthProvider::Oidc {
        return Err(AuthError::Invalid);
    }
    let code = query.code.ok_or(AuthError::Invalid)?;
    // Discover fresh signing keys at every callback so provider rotation does not require a restart.
    let client = discover(&auth.config, auth.secret.clone(), &auth.http).await?;
    let transport = |request| {
        bounded_http(
            auth.http.clone(),
            request,
            auth.config.public_url.scheme() == "http",
        )
    };
    let tokens = client
        .exchange_code(AuthorizationCode::new(code))
        .map_err(AuthError::provider)?
        .set_pkce_verifier(flow.verifier)
        .request_async(&transport)
        .await
        .map_err(AuthError::provider)?;
    let id_token = tokens.id_token().ok_or(AuthError::Invalid)?;
    let verifier = client.id_token_verifier();
    let claims = id_token
        .claims(&verifier, &flow.nonce)
        .map_err(AuthError::verification)?;
    // The library deliberately leaves azp policy to callers. Bind any authorized party
    // to this relying party so a token issued to another client cannot start a session.
    if claims
        .authorized_party()
        .is_some_and(|party| party.as_str() != auth.config.client_id)
        || (claims.audiences().len() > 1 && claims.authorized_party().is_none())
    {
        return Err(AuthError::Invalid);
    }
    if let Some(expected) = claims.access_token_hash() {
        let actual = AccessTokenHash::from_token(
            tokens.access_token(),
            id_token.signing_alg().map_err(AuthError::verification)?,
            id_token
                .signing_key(&verifier)
                .map_err(AuthError::verification)?,
        )
        .map_err(AuthError::verification)?;
        if actual != *expected {
            return Err(AuthError::Invalid);
        }
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(AuthError::provider)?
        .as_secs();
    if claims.issue_time().timestamp() > now as i64 + 60 {
        return Err(AuthError::Invalid);
    }
    let expiration =
        u64::try_from(claims.expiration().timestamp()).map_err(AuthError::verification)?;
    let seconds = expiration
        .checked_sub(now)
        .filter(|seconds| *seconds > 0)
        .ok_or(AuthError::Invalid)?;
    let lifetime = Duration::from_secs(seconds).min(SESSION_LIFETIME);
    let subject = claims.subject().as_str().to_owned();
    if subject.is_empty() {
        return Err(AuthError::Invalid);
    }
    let name = claims
        .preferred_username()
        .map(|name| name.as_str())
        .unwrap_or(&subject)
        .to_owned();
    establish_session(
        auth,
        &headers,
        flow.return_to,
        Identity {
            issuer: auth.config.issuer.as_str().to_owned(),
            subject,
            name,
        },
        lifetime,
    )
    .await
}

async fn establish_session(
    auth: &Authentication,
    headers: &HeaderMap,
    return_to: String,
    identity: Identity,
    lifetime: Duration,
) -> Result<Response, AuthError> {
    let now = now_epoch()?;
    let token = CsrfToken::new_random_len(32);
    let session_key = key(token.secret());
    let session = Arc::new(Session {
        identity,
        csrf: CsrfToken::new_random_len(32).secret().clone(),
        expires_at: now
            .checked_add(lifetime.as_secs())
            .ok_or(AuthError::Invalid)?,
        session_key,
        revoked: AtomicBool::new(false),
    });
    if let Some(old) = cookie_value(headers, auth.cookie_name(false)) {
        auth.remove_session(key(old)).await?;
    }
    auth.store_session(session).await?;
    let mut response = Redirect::to(&return_to).into_response();
    // Axum's tuple header arrays replace duplicate names; both cookies must reach the browser.
    response.headers_mut().append(
        header::SET_COOKIE,
        auth.cookie(false, token.secret(), lifetime)?,
    );
    response
        .headers_mut()
        .append(header::SET_COOKIE, auth.cookie(true, "", Duration::ZERO)?);
    Ok(response)
}

#[derive(Deserialize)]
struct GitHubTokenResponse {
    access_token: String,
}

#[derive(Deserialize)]
struct GitHubUserResponse {
    id: u64,
    login: String,
    name: Option<String>,
}

fn github_endpoints(auth: &Authentication) -> Result<(Url, Url, Url), AuthError> {
    let github = auth.config.github.clone().unwrap_or_default();
    let authorize = Url::parse(&github.authorize_url).map_err(AuthError::provider)?;
    let token = Url::parse(&github.token_url).map_err(AuthError::provider)?;
    let api = Url::parse(&github.api_url).map_err(AuthError::provider)?;
    let allow_http = auth.config.public_url.scheme() == "http";
    for endpoint in [&authorize, &token, &api] {
        validate_identity_url(endpoint, allow_http).map_err(AuthError::provider)?;
    }
    if !api.path().ends_with('/') {
        return Err(AuthError::Invalid);
    }
    Ok((authorize, token, api))
}

async fn bounded_json<T: DeserializeOwned>(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<T, AuthError> {
    if !response.status().is_success() {
        return Err(AuthError::provider(std::io::Error::other(
            "GitHub endpoint returned an unsuccessful response",
        )));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(AuthError::provider)? {
        if body.len() + chunk.len() > limit {
            return Err(AuthError::Invalid);
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).map_err(AuthError::provider)
}

async fn finish_github_login(
    server: &Server,
    headers: HeaderMap,
    query: CallbackQuery,
) -> Result<Response, AuthError> {
    let auth = server.auth.as_ref().ok_or(AuthError::Invalid)?;
    let _permit = auth.admission.try_acquire().map_err(|_| AuthError::Busy)?;
    let state = query.state.ok_or(AuthError::Invalid)?;
    let cookie = cookie_value(&headers, auth.cookie_name(true)).ok_or(AuthError::Invalid)?;
    if blake3::hash(state.as_bytes()) != blake3::hash(cookie.as_bytes()) {
        return Err(AuthError::Invalid);
    }
    let flow = auth.take_flow(key(&state)).await?;
    if flow.provider != AuthProvider::GitHub {
        return Err(AuthError::Invalid);
    }
    let code = query.code.ok_or(AuthError::Invalid)?;
    let secret = auth.secret.as_ref().ok_or(AuthError::Invalid)?;
    let (_, token_url, api_url) = github_endpoints(auth)?;
    let redirect = auth
        .config
        .public_url
        .join("auth/callback")
        .map_err(AuthError::provider)?;
    let client_secret = secret.secret().as_str();
    let form = [
        ("client_id", auth.config.client_id.as_str()),
        ("client_secret", client_secret),
        ("code", code.as_str()),
        ("redirect_uri", redirect.as_str()),
        ("code_verifier", flow.verifier.secret().as_str()),
    ];
    let token_response = auth
        .http
        .post(token_url)
        .header(header::ACCEPT, "application/json")
        .header(header::USER_AGENT, "crab-server")
        .form(&form)
        .send()
        .await
        .map_err(AuthError::provider)?;
    let token: GitHubTokenResponse = bounded_json(token_response, 64 * 1024).await?;
    if token.access_token.is_empty() || token.access_token.len() > 16 * 1024 {
        return Err(AuthError::Invalid);
    }
    let user_url = api_url.join("user").map_err(AuthError::provider)?;
    let user_response = auth
        .http
        .get(user_url)
        .header(header::ACCEPT, "application/vnd.github+json")
        .header(header::USER_AGENT, "crab-server")
        .header("X-GitHub-Api-Version", "2022-11-28")
        .bearer_auth(&token.access_token)
        .send()
        .await
        .map_err(AuthError::provider)?;
    let user: GitHubUserResponse = bounded_json(user_response, 64 * 1024).await?;
    drop(token);
    if user.id == 0
        || user.login.is_empty()
        || user.login.chars().any(char::is_control)
        || user.login.chars().count() > 160
    {
        return Err(AuthError::Invalid);
    }
    let name = user
        .name
        .filter(|name| !name.trim().is_empty())
        .unwrap_or(user.login);
    if name.chars().any(char::is_control) || name.chars().count() > 160 {
        return Err(AuthError::Invalid);
    }
    establish_session(
        auth,
        &headers,
        flow.return_to,
        Identity {
            issuer: auth.config.issuer.as_str().to_owned(),
            subject: user.id.to_string(),
            name,
        },
        SESSION_LIFETIME,
    )
    .await
}

pub(crate) async fn session(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
) -> Json<serde_json::Value> {
    let mode = server
        .auth
        .as_ref()
        .map(|auth| match auth.config.provider {
            AuthProvider::Oidc => "oidc",
            AuthProvider::GitHub => "github",
        })
        .unwrap_or("local");
    Json(match principal {
        Principal::Local => json!({"authenticated":true,"mode":"local","user":null,"csrf":null}),
        Principal::Anonymous | Principal::Git(_) => {
            json!({"authenticated":false,"mode":mode,"user":null,"csrf":null})
        }
        Principal::User(session) => {
            json!({"authenticated":true,"mode":mode,"user":session.identity,"csrf":session.csrf})
        }
    })
}

pub(crate) async fn logout(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
) -> Result<Response, AuthError> {
    let auth = server.auth.as_ref().ok_or(AuthError::Invalid)?;
    // The transport boundary has already required both session CSRF and the canonical Origin.
    let token = cookie_value(&headers, auth.cookie_name(false)).ok_or(AuthError::Invalid)?;
    auth.remove_session(key(token)).await?;
    Ok((
        [(header::SET_COOKIE, auth.cookie(false, "", Duration::ZERO)?)],
        StatusCode::NO_CONTENT,
    )
        .into_response())
}
