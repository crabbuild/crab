use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
mod git_tokens;
pub(crate) use git_tokens::{issue_git_token, revoke_git_tokens};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::{
    Extension, Json,
    extract::{Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Redirect, Response},
};
use openidconnect::{
    AccessTokenHash, AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointMaybeSet,
    EndpointNotSet, EndpointSet, Nonce, OAuth2TokenResponse, PkceCodeChallenge, PkceCodeVerifier,
    RedirectUrl, Scope, TokenResponse,
    core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::{Mutex, Semaphore};
use url::Url;

use crate::{
    OidcConfig, RepositoryAccess, RepositoryConfig, config::validate_identity_url, server::Server,
};

type Client = CoreClient<
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointMaybeSet,
    EndpointMaybeSet,
>;
type Key = [u8; 32];
const FLOW_LIFETIME: Duration = Duration::from_secs(600);
const SESSION_LIFETIME: Duration = Duration::from_secs(8 * 60 * 60);

#[derive(Debug, thiserror::Error)]
pub(crate) enum AuthError {
    #[error("invalid or expired sign-in")]
    Invalid,
    #[error("repository access denied")]
    Forbidden,
    #[error("identity claims failed verification")]
    Verification(#[source] Box<dyn std::error::Error + Send + Sync>),
    #[error("sign-in capacity exceeded")]
    Busy,
    #[error("identity provider request failed")]
    Provider(#[source] Box<dyn std::error::Error + Send + Sync>),
}

impl AuthError {
    fn verification(error: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Verification(Box::new(error))
    }

    fn provider(error: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Provider(Box::new(error))
    }
}

impl IntoResponse for AuthError {
    fn into_response(self) -> Response {
        let (status, code, message) = match self {
            Self::Forbidden => (
                StatusCode::FORBIDDEN,
                "repository_access_denied",
                "You do not have the requested repository access.",
            ),
            Self::Invalid | Self::Verification(_) => (
                StatusCode::BAD_REQUEST,
                "invalid_sign_in",
                "Sign-in expired or could not be verified. Start again from the sign-in page.",
            ),
            Self::Busy => (
                StatusCode::TOO_MANY_REQUESTS,
                "sign_in_busy",
                "Sign-in is busy. Try again shortly.",
            ),
            Self::Provider(_) => (
                StatusCode::BAD_GATEWAY,
                "identity_unavailable",
                "The identity provider is unavailable. Try signing in again.",
            ),
        };
        // Provider errors can contain token endpoint bodies. Only fixed messages cross HTTP.
        (
            status,
            Json(json!({"error":{"code":code,"message":message}})),
        )
            .into_response()
    }
}

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub(crate) struct Identity {
    pub issuer: String,
    pub subject: String,
    pub name: String,
}

pub(crate) struct Session {
    identity: Identity,
    csrf: String,
    expires: Instant,
    revoked: AtomicBool,
}

impl Session {
    fn active(&self) -> bool {
        self.expires > Instant::now() && !self.revoked.load(Ordering::Acquire)
    }
}

pub(crate) struct GitToken {
    session: Arc<Session>,
    owner: String,
    repository: String,
    access: RepositoryAccess,
    revoked: AtomicBool,
}

impl GitToken {
    fn active(&self) -> bool {
        self.session.active() && !self.revoked.load(Ordering::Acquire)
    }
}

#[derive(Clone)]
pub(crate) enum Principal {
    Anonymous,
    Local,
    User(Arc<Session>),
    Git(Arc<GitToken>),
}

impl Principal {
    pub(crate) fn identity(&self) -> Option<Identity> {
        match self {
            Self::User(session) if session.active() => Some(session.identity.clone()),
            Self::Git(token) if token.active() => Some(token.session.identity.clone()),
            Self::Local => Some(Identity {
                issuer: "urn:crab:local".into(),
                subject: "operator".into(),
                name: "Local operator".into(),
            }),
            _ => None,
        }
    }
    pub fn can_read(&self, repository: &RepositoryConfig) -> bool {
        self.access(repository).is_some()
    }

    pub fn can_write(&self, repository: &RepositoryConfig) -> bool {
        matches!(
            self.access(repository),
            Some(RepositoryAccess::Write | RepositoryAccess::Admin)
        )
    }

    pub fn can_admin(&self, repository: &RepositoryConfig) -> bool {
        self.access(repository) == Some(RepositoryAccess::Admin)
    }

    fn access(&self, repository: &RepositoryConfig) -> Option<RepositoryAccess> {
        let (session, ceiling) = match self {
            Self::Local => return Some(RepositoryAccess::Admin),
            Self::User(session) => (session, RepositoryAccess::Admin),
            Self::Git(token)
                if token.active()
                    && token.owner == repository.owner
                    && token.repository == repository.name =>
            {
                (&token.session, token.access)
            }
            _ => return None,
        };
        if !session.active() {
            return None;
        }
        repository
            .members
            .iter()
            .find(|member| member.subject == session.identity.subject)
            .map(|member| std::cmp::min(member.access, ceiling))
    }

    pub fn authenticated(&self) -> bool {
        match self {
            Self::Anonymous => false,
            Self::Local => true,
            Self::User(session) => session.active(),
            Self::Git(token) => token.active(),
        }
    }
}

struct Flow {
    nonce: Nonce,
    verifier: PkceCodeVerifier,
    return_to: String,
    expires: Instant,
}

pub(crate) struct Authentication {
    config: OidcConfig,
    secret: Option<ClientSecret>,
    http: reqwest::Client,
    client: Client,
    flows: Mutex<HashMap<Key, Flow>>,
    sessions: Mutex<HashMap<Key, Arc<Session>>>,
    git_tokens: Mutex<HashMap<Key, Arc<GitToken>>>,
    admission: Semaphore,
}

impl Authentication {
    pub async fn new(config: OidcConfig) -> Result<Self, AuthError> {
        let secret = config
            .client_secret_file
            .as_ref()
            .map(|path| {
                let text = std::fs::read_to_string(path).map_err(AuthError::provider)?;
                let text = text
                    .strip_suffix("\r\n")
                    .or_else(|| text.strip_suffix('\n'))
                    .unwrap_or(&text);
                if text.is_empty() {
                    return Err(AuthError::Invalid);
                }
                Ok(ClientSecret::new(text.to_owned()))
            })
            .transpose()?;
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(AuthError::provider)?;
        let client = discover(&config, secret.clone(), &http).await?;
        Ok(Self {
            config,
            secret,
            http,
            client,
            flows: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            git_tokens: Mutex::new(HashMap::new()),
            admission: Semaphore::new(8),
        })
    }

    pub fn origin(&self) -> String {
        self.config.public_url.origin().ascii_serialization()
    }

    pub fn allows_host(&self, host: Option<&str>) -> bool {
        let origin = self.origin();
        origin
            .split_once("://")
            .map(|(_, authority)| Some(authority) == host)
            .unwrap_or(false)
    }

    fn cookie_name(&self, login: bool) -> &'static str {
        match (self.config.public_url.scheme() == "https", login) {
            (true, true) => "__Host-crab_login",
            (true, false) => "__Host-crab_session",
            (false, true) => "crab_login",
            (false, false) => "crab_session",
        }
    }

    fn cookie(
        &self,
        login: bool,
        value: &str,
        lifetime: Duration,
    ) -> Result<HeaderValue, AuthError> {
        let secure = if self.config.public_url.scheme() == "https" {
            "; Secure"
        } else {
            ""
        };
        HeaderValue::from_str(&format!(
            "{}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}{secure}",
            self.cookie_name(login),
            lifetime.as_secs()
        ))
        .map_err(AuthError::provider)
    }

    pub async fn principal(&self, headers: &HeaderMap) -> Principal {
        let Some(token) = cookie_value(headers, self.cookie_name(false)) else {
            return Principal::Anonymous;
        };
        let mut sessions = self.sessions.lock().await;
        sessions.retain(|_, session| session.active());
        sessions
            .get(&key(token))
            .cloned()
            .map(Principal::User)
            .unwrap_or(Principal::Anonymous)
    }

    pub fn accepts_mutation(&self, principal: &Principal, headers: &HeaderMap) -> bool {
        let Principal::User(session) = principal else {
            return false;
        };
        let origin = headers
            .get(header::ORIGIN)
            .and_then(|value| value.to_str().ok());
        let csrf = headers
            .get("x-csrf-token")
            .and_then(|value| value.to_str().ok());
        origin == Some(self.origin().as_str())
            && csrf.is_some_and(|csrf| {
                blake3::hash(csrf.as_bytes()) == blake3::hash(session.csrf.as_bytes())
            })
    }
}

async fn discover(
    config: &OidcConfig,
    secret: Option<ClientSecret>,
    http: &reqwest::Client,
) -> Result<Client, AuthError> {
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
    Ok(CoreClient::from_provider_metadata(
        metadata,
        ClientId::new(config.client_id.clone()),
        secret,
    )
    .set_redirect_uri(RedirectUrl::new(redirect.to_string()).map_err(AuthError::provider)?))
}

async fn bounded_http(
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
    let mut builder = axum::http::Response::builder()
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
        .join(&return_to)
        .map_err(AuthError::provider)?;
    if destination.origin() != auth.config.public_url.origin()
        || destination.path().starts_with("/auth/")
    {
        return Err(AuthError::Invalid);
    }
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let (url, state, nonce) = auth
        .client
        .authorize_url(
            CoreAuthenticationFlow::AuthorizationCode,
            CsrfToken::new_random,
            Nonce::new_random,
        )
        .add_scope(Scope::new("profile".into()))
        .set_pkce_challenge(challenge)
        .url();
    let mut flows = auth.flows.lock().await;
    flows.retain(|_, flow| flow.expires > Instant::now());
    if flows.len() >= 512 {
        return Err(AuthError::Busy);
    }
    flows.insert(
        key(state.secret()),
        Flow {
            nonce,
            verifier,
            return_to,
            expires: Instant::now() + FLOW_LIFETIME,
        },
    );
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
    match finish_login(&server, headers, query).await {
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
    let flow = auth
        .flows
        .lock()
        .await
        .remove(&key(&state))
        .filter(|flow| flow.expires > Instant::now())
        .ok_or(AuthError::Invalid)?;
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
    let token = CsrfToken::new_random_len(32);
    let session = Arc::new(Session {
        identity: Identity {
            issuer: auth.config.issuer.as_str().to_owned(),
            subject,
            name,
        },
        csrf: CsrfToken::new_random_len(32).secret().clone(),
        expires: Instant::now() + lifetime,
        revoked: AtomicBool::new(false),
    });
    let mut sessions = auth.sessions.lock().await;
    sessions.retain(|_, session| session.active());
    if sessions.len() >= 4096 {
        return Err(AuthError::Busy);
    }
    if let Some(old) = cookie_value(&headers, auth.cookie_name(false))
        && let Some(session) = sessions.remove(&key(old))
    {
        session.revoked.store(true, Ordering::Release);
    }
    sessions.insert(key(token.secret()), session);
    let mut response = Redirect::to(&flow.return_to).into_response();
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

pub(crate) async fn session(Extension(principal): Extension<Principal>) -> Json<serde_json::Value> {
    Json(match principal {
        Principal::Local => json!({"authenticated":true,"mode":"local","user":null,"csrf":null}),
        Principal::Anonymous | Principal::Git(_) => {
            json!({"authenticated":false,"mode":"oidc","user":null,"csrf":null})
        }
        Principal::User(session) => {
            json!({"authenticated":true,"mode":"oidc","user":session.identity,"csrf":session.csrf})
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
    if let Some(session) = auth.sessions.lock().await.remove(&key(token)) {
        session.revoked.store(true, Ordering::Release);
    }
    Ok((
        [(header::SET_COOKIE, auth.cookie(false, "", Duration::ZERO)?)],
        StatusCode::NO_CONTENT,
    )
        .into_response())
}

fn key(value: &str) -> Key {
    *blake3::hash(value.as_bytes()).as_bytes()
}

fn cookie_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|header| header.to_str().ok())
        .flat_map(|header| header.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .filter(|(key, _)| *key == name)
        .map(|(_, value)| value);
    let value = values.next()?;
    if values.next().is_some() || value.len() > 128 {
        return None;
    }
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member_repository(access: RepositoryAccess) -> RepositoryConfig {
        RepositoryConfig {
            owner: "team".into(),
            name: "private".into(),
            bucket: "bucket".into(),
            prefix: "private".into(),
            default_branch: "main".into(),
            description: String::new(),
            members: vec![crate::RepositoryMember {
                subject: "alice".into(),
                name: "Alice".into(),
                access,
            }],
            protected_branches: vec![],
        }
    }

    fn active_session() -> Arc<Session> {
        Arc::new(Session {
            identity: Identity {
                issuer: "https://identity.example".into(),
                subject: "alice".into(),
                name: "Alice".into(),
            },
            csrf: "csrf".into(),
            expires: Instant::now() + Duration::from_secs(60),
            revoked: AtomicBool::new(false),
        })
    }

    #[test]
    fn repository_administration_stays_out_of_write_scoped_git_tokens() {
        let repository = member_repository(RepositoryAccess::Admin);
        let session = active_session();
        let browser = Principal::User(Arc::clone(&session));
        assert!(browser.can_read(&repository));
        assert!(browser.can_write(&repository));
        assert!(browser.can_admin(&repository));

        let git = Principal::Git(Arc::new(GitToken {
            session,
            owner: repository.owner.clone(),
            repository: repository.name.clone(),
            access: RepositoryAccess::Write,
            revoked: AtomicBool::new(false),
        }));
        assert!(git.can_write(&repository));
        assert!(!git.can_admin(&repository));
        assert!(
            !Principal::User(active_session())
                .can_admin(&member_repository(RepositoryAccess::Write))
        );
    }

    #[tokio::test]
    async fn expired_sessions_are_rejected_and_https_cookies_cannot_be_shadowed() {
        let config = OidcConfig {
            issuer: openidconnect::IssuerUrl::new("https://id.example".into()).unwrap(),
            public_url: Url::parse("https://git.example").unwrap(),
            client_id: "crab".into(),
            client_secret_file: None,
        };
        let metadata: CoreProviderMetadata = serde_json::from_value(json!({"issuer":"https://id.example","authorization_endpoint":"https://id.example/auth","jwks_uri":"https://id.example/keys","response_types_supported":["code"],"subject_types_supported":["public"],"id_token_signing_alg_values_supported":["RS256"]})).unwrap();
        let auth = Authentication {
            config,
            secret: None,
            http: reqwest::Client::new(),
            client: CoreClient::from_provider_metadata(
                metadata,
                ClientId::new("crab".into()),
                None,
            ),
            flows: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
            git_tokens: Mutex::new(HashMap::new()),
            admission: Semaphore::new(1),
        };
        let cookie = auth.cookie(false, "test-token", SESSION_LIFETIME).unwrap();
        assert_eq!(
            cookie,
            "__Host-crab_session=test-token; Path=/; HttpOnly; SameSite=Lax; Max-Age=28800; Secure"
        );
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("__Host-crab_session=test-token"),
        );
        let session = Arc::new(Session {
            identity: Identity {
                issuer: "https://identity.example".into(),
                subject: "alice".into(),
                name: "Alice".into(),
            },
            csrf: "test-csrf".into(),
            expires: Instant::now() - Duration::from_secs(1),
            revoked: AtomicBool::new(false),
        });
        auth.sessions
            .lock()
            .await
            .insert(key("test-token"), Arc::clone(&session));
        let principal = Principal::Git(Arc::new(GitToken {
            session,
            owner: "team".into(),
            repository: "private".into(),
            access: RepositoryAccess::Write,
            revoked: AtomicBool::new(false),
        }));
        assert!(!principal.authenticated());
        assert!(!auth.principal(&headers).await.authenticated());
        assert!(auth.sessions.lock().await.is_empty());
        headers.append(
            header::COOKIE,
            HeaderValue::from_static("__Host-crab_session=shadow"),
        );
        assert!(cookie_value(&headers, "__Host-crab_session").is_none());
    }
}
