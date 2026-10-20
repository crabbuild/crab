use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
mod backchannel_logout;
mod git_tokens;
mod http;
mod state;
mod transport;
pub(crate) use backchannel_logout::backchannel_logout;
pub(crate) use git_tokens::{issue_git_token, revoke_git_tokens};
pub(crate) use http::{callback, login, logout, session};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::{AuthProvider, OidcConfig, RepositoryAccess, RepositoryConfig};
use axum::{
    Json,
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use crab_storage::{StorageError, Store};
use openidconnect::{
    ClientSecret, EndpointMaybeSet, EndpointNotSet, EndpointSet, Nonce, PkceCodeVerifier,
    core::CoreClient,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::{Mutex, Semaphore};
use transport::discover;

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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
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
    provider: AuthProvider,
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
    provider: AuthProvider,
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
    git_tokens: Vec<Key>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct IdentitySessionMarker {
    schema_version: u32,
    issuer: String,
    subject: String,
    session_key: Key,
    expires_at: u64,
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
    client: Option<Client>,
    sessions: Mutex<HashMap<Key, Arc<Session>>>,
    git_tokens: Mutex<HashMap<Key, Arc<GitToken>>>,
    state: AuthState,
    cursor_key: [u8; 32],
    admission: Semaphore,
    logout_admission: Semaphore,
}

impl Authentication {
    pub(crate) async fn new(
        config: OidcConfig,
        root: &crate::storage_root::StorageRoot,
    ) -> Result<Self, AuthError> {
        let state = AuthState {
            store: root.store.clone(),
            prefix: root.path(".crab/http-server/v1/auth").to_string(),
        };
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
        let client = match config.provider {
            AuthProvider::Oidc => Some(discover(&config, secret.clone(), &http).await?),
            AuthProvider::GitHub => None,
        };
        Ok(Self {
            config,
            secret,
            http,
            client,
            sessions: Mutex::new(HashMap::new()),
            git_tokens: Mutex::new(HashMap::new()),
            state,
            cursor_key,
            admission: Semaphore::new(8),
            logout_admission: Semaphore::new(4),
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
mod tests;
