use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
mod git_tokens;
pub(crate) use git_tokens::{issue_git_token, revoke_git_tokens};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::{
    Extension, Json,
    extract::{Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Redirect, Response},
};
use bytes::Bytes;
use crab_storage::{StorageError, Store};
use object_store::path::Path as ObjectPath;
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
const STATE_CAS_ATTEMPTS: usize = 8;

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
    #[error("shared identity state storage failed")]
    Storage(#[from] StorageError),
    #[error("shared identity state encoding failed")]
    Json(#[from] serde_json::Error),
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
            Self::Provider(_) | Self::Storage(_) | Self::Json(_) => (
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
    expires_at: u64,
    session_key: Key,
    revoked: AtomicBool,
}

impl Session {
    fn active(&self) -> bool {
        self.expires_at > now_epoch().unwrap_or(u64::MAX) && !self.revoked.load(Ordering::Acquire)
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
    expires_at: u64,
}

#[derive(Clone)]
struct AuthState {
    store: Store,
    prefix: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredFlow {
    nonce: String,
    verifier: String,
    return_to: String,
    expires_at: u64,
    consumed: bool,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredSession {
    identity: Identity,
    csrf: String,
    expires_at: u64,
    #[serde(default)]
    git_tokens: Vec<Key>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredGitToken {
    session_key: Key,
    owner: String,
    repository: String,
    access: RepositoryAccess,
    expires_at: u64,
}

pub(crate) struct Authentication {
    config: OidcConfig,
    secret: Option<ClientSecret>,
    http: reqwest::Client,
    client: Client,
    flows: Mutex<HashMap<Key, Flow>>,
    sessions: Mutex<HashMap<Key, Arc<Session>>>,
    git_tokens: Mutex<HashMap<Key, Arc<GitToken>>>,
    state: Option<AuthState>,
    cursor_key: [u8; 32],
    admission: Semaphore,
}

impl Authentication {
    #[cfg(test)]
    pub async fn new(config: OidcConfig) -> Result<Self, AuthError> {
        Self::build(config, None).await
    }

    pub(crate) async fn new_durable(
        config: OidcConfig,
        root: &crate::storage_root::StorageRoot,
    ) -> Result<Self, AuthError> {
        Self::build(
            config,
            Some(AuthState {
                store: root.store.clone(),
                prefix: root.path(".crab/http-server/v1/auth").to_string(),
            }),
        )
        .await
    }

    async fn build(config: OidcConfig, state: Option<AuthState>) -> Result<Self, AuthError> {
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
        let cursor_key = match &config.state_key_file {
            Some(path) => {
                let secret = std::fs::read(path).map_err(AuthError::provider)?;
                if secret.len() < 32 {
                    return Err(AuthError::Invalid);
                }
                blake3::derive_key("crab http server state key v1", &secret)
            }
            None => rand::random(),
        };
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
            state,
            cursor_key,
            admission: Semaphore::new(8),
        })
    }

    pub(crate) fn cursor_key(&self) -> [u8; 32] {
        self.cursor_key
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
        self.load_session(key(token))
            .await
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

    async fn store_flow(&self, state_key: Key, flow: Flow) -> Result<(), AuthError> {
        let Some(state) = &self.state else {
            let mut flows = self.flows.lock().await;
            flows.retain(|_, flow| flow.expires_at > now_epoch().unwrap_or(u64::MAX));
            if flows.len() >= 512 {
                return Err(AuthError::Busy);
            }
            flows.insert(state_key, flow);
            return Ok(());
        };
        let record = StoredFlow {
            nonce: flow.nonce.secret().clone(),
            verifier: flow.verifier.secret().clone(),
            return_to: flow.return_to,
            expires_at: flow.expires_at,
            consumed: false,
        };
        state
            .store
            .create_strict(
                &state.path("flows", &state_key),
                Bytes::from(serde_json::to_vec(&record)?),
            )
            .await?;
        Ok(())
    }

    async fn take_flow(&self, state_key: Key) -> Result<Flow, AuthError> {
        let Some(state) = &self.state else {
            return self
                .flows
                .lock()
                .await
                .remove(&state_key)
                .filter(|flow| flow.expires_at > now_epoch().unwrap_or(u64::MAX))
                .ok_or(AuthError::Invalid);
        };
        let path = state.path("flows", &state_key);
        let (body, etag) = state
            .store
            .get_with_etag_bounded(&path, 64 * 1024)
            .await
            .map_err(|error| match error {
                StorageError::NotFound { .. } => AuthError::Invalid,
                other => AuthError::Storage(other),
            })?;
        let mut record: StoredFlow = serde_json::from_slice(&body)?;
        if record.consumed || record.expires_at <= now_epoch()? {
            let _ = state.store.delete(&path).await;
            return Err(AuthError::Invalid);
        }
        record.consumed = true;
        state
            .store
            .update(&path, Bytes::from(serde_json::to_vec(&record)?), etag)
            .await
            .map_err(|error| match error {
                StorageError::StateConflict { .. } => AuthError::Invalid,
                other => AuthError::Storage(other),
            })?;
        Ok(Flow {
            nonce: Nonce::new(record.nonce),
            verifier: PkceCodeVerifier::new(record.verifier),
            return_to: record.return_to,
            expires_at: record.expires_at,
        })
    }

    async fn store_session(&self, session: Arc<Session>) -> Result<(), AuthError> {
        let Some(state) = &self.state else {
            let mut sessions = self.sessions.lock().await;
            sessions.retain(|_, session| session.active());
            if sessions.len() >= 4096 {
                return Err(AuthError::Busy);
            }
            sessions.insert(session.session_key, session);
            return Ok(());
        };
        let record = StoredSession {
            identity: session.identity.clone(),
            csrf: session.csrf.clone(),
            expires_at: session.expires_at,
            git_tokens: Vec::new(),
        };
        state
            .store
            .create_strict(
                &state.path("sessions", &session.session_key),
                Bytes::from(serde_json::to_vec(&record)?),
            )
            .await?;
        Ok(())
    }

    async fn load_session(&self, session_key: Key) -> Option<Arc<Session>> {
        let Some(state) = &self.state else {
            let mut sessions = self.sessions.lock().await;
            sessions.retain(|_, session| session.active());
            return sessions.get(&session_key).cloned();
        };
        let body = state
            .store
            .get_with_etag_bounded(&state.path("sessions", &session_key), 64 * 1024)
            .await
            .ok()?
            .0;
        let record: StoredSession = serde_json::from_slice(&body).ok()?;
        let session = Arc::new(Session {
            identity: record.identity,
            csrf: record.csrf,
            expires_at: record.expires_at,
            session_key,
            revoked: AtomicBool::new(false),
        });
        if session.active() {
            Some(session)
        } else {
            let _ = state
                .store
                .delete(&state.path("sessions", &session_key))
                .await;
            None
        }
    }

    async fn remove_session(&self, session_key: Key) -> Result<(), AuthError> {
        if let Some(state) = &self.state {
            match state
                .store
                .delete(&state.path("sessions", &session_key))
                .await
            {
                Ok(()) | Err(StorageError::NotFound { .. }) => {}
                Err(error) => return Err(error.into()),
            }
        }
        if let Some(session) = self.sessions.lock().await.remove(&session_key) {
            session.revoked.store(true, Ordering::Release);
        }
        Ok(())
    }
}

impl AuthState {
    fn path(&self, kind: &str, key: &Key) -> ObjectPath {
        ObjectPath::from(format!("{}/{kind}/{}", self.prefix, hex_key(key)))
    }
}

fn hex_key(key: &Key) -> String {
    key.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn now_epoch() -> Result<u64, AuthError> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(AuthError::provider)?
        .as_secs())
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
    auth.store_flow(
        key(state.secret()),
        Flow {
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
    let flow = auth.take_flow(key(&state)).await?;
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
    let session_key = key(token.secret());
    let session = Arc::new(Session {
        identity: Identity {
            issuer: auth.config.issuer.as_str().to_owned(),
            subject,
            name,
        },
        csrf: CsrfToken::new_random_len(32).secret().clone(),
        expires_at: now
            .checked_add(lifetime.as_secs())
            .ok_or(AuthError::Invalid)?,
        session_key,
        revoked: AtomicBool::new(false),
    });
    if let Some(old) = cookie_value(&headers, auth.cookie_name(false)) {
        auth.remove_session(key(old)).await?;
    }
    auth.store_session(session).await?;
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
    auth.remove_session(key(token)).await?;
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
            expires_at: now_epoch().unwrap() + 60,
            session_key: [0; 32],
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
            state_key_file: None,
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
            state: None,
            cursor_key: [0; 32],
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
            expires_at: now_epoch().unwrap() - 1,
            session_key: key("test-token"),
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

    fn shared_auth(store: Store) -> Authentication {
        let config = OidcConfig {
            issuer: openidconnect::IssuerUrl::new("https://id.example".into()).unwrap(),
            public_url: Url::parse("https://git.example").unwrap(),
            client_id: "crab".into(),
            client_secret_file: None,
            state_key_file: None,
        };
        let metadata: CoreProviderMetadata = serde_json::from_value(json!({
            "issuer":"https://id.example",
            "authorization_endpoint":"https://id.example/auth",
            "jwks_uri":"https://id.example/keys",
            "response_types_supported":["code"],
            "subject_types_supported":["public"],
            "id_token_signing_alg_values_supported":["RS256"]
        }))
        .unwrap();
        Authentication {
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
            state: Some(AuthState {
                store,
                prefix: "root/.crab/http-server/v1/auth".into(),
            }),
            cursor_key: [7; 32],
            admission: Semaphore::new(1),
        }
    }

    #[tokio::test]
    async fn shared_state_crosses_replica_boundaries_and_consumes_flows_once() {
        let store = Store::new(Arc::new(object_store::memory::InMemory::new()));
        let first = shared_auth(store.clone());
        let second = shared_auth(store);
        let session_key = key("browser-token");
        let session = Arc::new(Session {
            identity: Identity {
                issuer: "https://id.example".into(),
                subject: "alice".into(),
                name: "Alice".into(),
            },
            csrf: "csrf".into(),
            expires_at: now_epoch().unwrap() + 600,
            session_key,
            revoked: AtomicBool::new(false),
        });
        first.store_session(Arc::clone(&session)).await.unwrap();
        let second_session = second.load_session(session_key).await.unwrap();
        assert!(second_session.active());

        let git_key = key("git-token");
        first
            .store_git_token(
                git_key,
                Arc::new(GitToken {
                    session,
                    owner: "team".into(),
                    repository: "private".into(),
                    access: RepositoryAccess::Write,
                    revoked: AtomicBool::new(false),
                }),
            )
            .await
            .unwrap();
        assert!(second.load_git_token(git_key).await.unwrap().active());
        second.remove_git_tokens(&second_session).await.unwrap();
        assert!(first.load_git_token(git_key).await.is_none());

        let flow_key = key("login-state");
        first
            .store_flow(
                flow_key,
                Flow {
                    nonce: Nonce::new("nonce".into()),
                    verifier: PkceCodeVerifier::new("v".repeat(43)),
                    return_to: "/team/private".into(),
                    expires_at: now_epoch().unwrap() + 60,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            second.take_flow(flow_key).await.unwrap().return_to,
            "/team/private"
        );
        assert!(matches!(
            first.take_flow(flow_key).await,
            Err(AuthError::Invalid)
        ));
    }
}
