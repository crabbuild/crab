//! Account-bound personal access tokens for GitHub-compatible clients.
//!
//! A token acts as its account through the GitHub API and HTTPS Git, never
//! through the browser API. It carries no OIDC group evidence, so only
//! durable individual, organization, and team grants apply. Issuance pins the
//! account revision: suspension or a provisioning change revokes the token for
//! good, exactly like the browser sessions issued before that change.

use std::collections::BTreeSet;
use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{DefaultBodyLimit, Path, State},
    http::StatusCode,
    routing::{delete, get},
};
use bytes::Bytes;
use crab_storage::StorageError;
use futures_util::StreamExt;
use object_store::path::Path as ObjectPath;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{AuthError, AuthState, Authentication, Identity, Principal, key, now_epoch};
use crate::{RepositoryAccess, server::Server};

const PREFIX: &str = "crab_pat_";
const RECORD_FORMAT: u8 = 1;
const MAX_TOKENS: usize = 50;
const MAX_NAME_CHARS: usize = 100;
const MAX_LIFETIME_DAYS: u64 = 366;
const DAY: u64 = 24 * 60 * 60;
const ORPHAN_GRACE_SECONDS: u64 = 10 * 60;

/// A GitHub classic OAuth scope that Crab enforces or explicitly accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Deserialize, Serialize)]
pub(crate) enum Scope {
    /// Repository access bounded by the account's own current role. Without
    /// it a token reads public repositories only.
    #[serde(rename = "repo")]
    Repo,
    /// Organization and team membership reads.
    #[serde(rename = "read:org")]
    ReadOrg,
    /// Repository deletion, which also requires an administrator role.
    #[serde(rename = "delete_repo")]
    DeleteRepo,
    /// Accepted for GitHub CLI's default request. Crab has no workflow files
    /// behind this scope and grants nothing for it.
    #[serde(rename = "workflow")]
    Workflow,
    /// Accepted for GitHub CLI's default request. Crab has no gists.
    #[serde(rename = "gist")]
    Gist,
}

impl Scope {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Repo => "repo",
            Self::ReadOrg => "read:org",
            Self::DeleteRepo => "delete_repo",
            Self::Workflow => "workflow",
            Self::Gist => "gist",
        }
    }

    /// Parses a requested scope. Write and admin organization scopes imply
    /// the read scope, which is the only organization capability exposed.
    pub(crate) fn parse(value: &str) -> Option<Self> {
        match value {
            "repo" => Some(Self::Repo),
            "read:org" | "write:org" | "admin:org" => Some(Self::ReadOrg),
            "delete_repo" => Some(Self::DeleteRepo),
            "workflow" => Some(Self::Workflow),
            "gist" => Some(Self::Gist),
            _ => None,
        }
    }
}

/// A verified token bound to one account and request.
pub(crate) struct PersonalToken {
    identity: Identity,
    scopes: BTreeSet<Scope>,
    expires_at: u64,
}

impl PersonalToken {
    pub(super) fn active(&self) -> bool {
        now_epoch().is_ok_and(|now| now < self.expires_at)
    }

    pub(super) fn identity(&self) -> &Identity {
        &self.identity
    }

    pub(crate) fn has(&self, scope: Scope) -> bool {
        self.scopes.contains(&scope)
    }

    /// The `X-OAuth-Scopes` value GitHub clients use to check capabilities.
    pub(crate) fn scope_header(&self) -> String {
        self.scopes
            .iter()
            .map(|scope| scope.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// The highest repository role this token may exercise, or `None` when
    /// it may only read public repositories.
    pub(super) fn ceiling(&self) -> Option<RepositoryAccess> {
        self.has(Scope::Repo).then_some(RepositoryAccess::Admin)
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct StoredToken {
    format: u8,
    id: Uuid,
    digest: String,
    issuer: String,
    subject: String,
    // The issuing session's display name, recorded as attribution fallback
    // like other credentials; Directory profiles still supply public names.
    account_name: String,
    name: String,
    scopes: BTreeSet<Scope>,
    created_at: u64,
    expires_at: u64,
    account_revision: u64,
}

/// What an account sees about one of its tokens; the secret is never stored.
#[derive(Serialize)]
pub(crate) struct TokenSummary {
    id: Uuid,
    name: String,
    scopes: Vec<&'static str>,
    created_at: u64,
    expires_at: u64,
    expired: bool,
}

impl From<&StoredToken> for TokenSummary {
    fn from(token: &StoredToken) -> Self {
        Self {
            id: token.id,
            name: token.name.clone(),
            scopes: token.scopes.iter().map(|scope| scope.as_str()).collect(),
            created_at: token.created_at,
            expires_at: token.expires_at,
            expired: now_epoch().is_ok_and(|now| now >= token.expires_at),
        }
    }
}

pub(super) fn recognizes(secret: &str) -> bool {
    secret.starts_with(PREFIX)
}

fn parse(token: &str) -> Option<Uuid> {
    let (id, secret) = token.strip_prefix(PREFIX)?.split_once('.')?;
    blake3::Hash::from_hex(secret).ok()?;
    Uuid::parse_str(id).ok().filter(|id| !id.is_nil())
}

impl AuthState {
    fn token_path(&self, id: Uuid) -> ObjectPath {
        ObjectPath::from(format!(
            "{}/personal-tokens/{}.json",
            self.prefix,
            id.simple()
        ))
    }

    fn identity_tokens_prefix(&self, issuer: &str, subject: &str) -> ObjectPath {
        let mut digest = blake3::Hasher::new_derive_key("crab identity personal tokens v1");
        digest.update(&(issuer.len() as u64).to_le_bytes());
        digest.update(issuer.as_bytes());
        digest.update(subject.as_bytes());
        ObjectPath::from(format!(
            "{}/identity-personal-tokens/{}/",
            self.prefix,
            digest.finalize().to_hex()
        ))
    }

    fn identity_token_path(&self, issuer: &str, subject: &str, id: Uuid) -> ObjectPath {
        ObjectPath::from(format!(
            "{}/{}",
            self.identity_tokens_prefix(issuer, subject),
            id.simple()
        ))
    }

    async fn read_token(&self, id: Uuid) -> Result<Option<StoredToken>, AuthError> {
        match self
            .store
            .get_with_etag_bounded(&self.token_path(id), 8 * 1024)
            .await
        {
            Ok((body, _)) => {
                let record: StoredToken = serde_json::from_slice(&body)?;
                if record.format != RECORD_FORMAT || record.id != id {
                    return Err(AuthError::Invalid);
                }
                Ok(Some(record))
            }
            Err(StorageError::NotFound { .. }) => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    async fn identity_token_ids(
        &self,
        issuer: &str,
        subject: &str,
    ) -> Result<Vec<(Uuid, u64)>, AuthError> {
        let mut entries = self
            .store
            .list_stream(&self.identity_tokens_prefix(issuer, subject));
        let mut ids = Vec::new();
        while let Some(entry) = entries.next().await {
            let entry = entry?;
            // Bound the listing even if the index was written past the cap.
            if ids.len() > MAX_TOKENS {
                break;
            }
            if let Some(id) = entry
                .location
                .filename()
                .and_then(|name| Uuid::try_parse(name).ok())
            {
                let written = u64::try_from(entry.last_modified.timestamp()).unwrap_or(0);
                ids.push((id, written));
            }
        }
        Ok(ids)
    }
}

impl Authentication {
    /// Verifies a presented token against its record and the account's
    /// current lifecycle revision.
    pub(super) async fn verify_personal_token(&self, token: &str) -> Option<Principal> {
        let id = parse(token)?;
        let state = &self.state;
        let record = state.read_token(id).await.ok()??;
        // `blake3::Hash` equality is constant time.
        if blake3::Hash::from_hex(&record.digest).ok()? != blake3::Hash::from_bytes(key(token))
            || record.issuer != self.issuer()
            || record.expires_at <= now_epoch().ok()?
        {
            return None;
        }
        let account = state
            .account_state(&record.issuer, &record.subject)
            .await
            .ok()?;
        if account.suspended || account.revision != record.account_revision {
            return None;
        }
        let token = PersonalToken {
            identity: Identity {
                issuer: record.issuer,
                subject: record.subject,
                name: record.account_name,
            },
            scopes: record.scopes,
            expires_at: record.expires_at,
        };
        Some(Principal::token(Arc::new(token)))
    }

    async fn create_personal_token(
        &self,
        identity: &Identity,
        request: CreateRequest,
    ) -> Result<Created, AuthError> {
        let state = &self.state;
        let account = state
            .account_state(&identity.issuer, &identity.subject)
            .await?;
        if account.suspended {
            return Err(AuthError::Suspended);
        }
        let name = request.name.trim().to_owned();
        let lifetime = request.expires_in_days.unwrap_or(30);
        let scopes: BTreeSet<Scope> = request
            .scopes
            .iter()
            .map(|scope| Scope::parse(scope).ok_or(AuthError::Invalid))
            .collect::<Result<_, _>>()?;
        if name.is_empty()
            || name.chars().count() > MAX_NAME_CHARS
            || name.chars().any(char::is_control)
            || !(1..=MAX_LIFETIME_DAYS).contains(&lifetime)
        {
            return Err(AuthError::Invalid);
        }
        if state
            .identity_token_ids(&identity.issuer, &identity.subject)
            .await?
            .len()
            >= MAX_TOKENS
        {
            return Err(AuthError::Busy);
        }
        let id = Uuid::now_v7();
        let secret = blake3::Hash::from_bytes(rand::random());
        let token = format!("{PREFIX}{}.{secret}", id.simple());
        let created_at = now_epoch()?;
        let record = StoredToken {
            format: RECORD_FORMAT,
            id,
            digest: blake3::Hash::from_bytes(key(&token)).to_hex().to_string(),
            issuer: identity.issuer.clone(),
            subject: identity.subject.clone(),
            account_name: identity.name.chars().take(160).collect(),
            name,
            scopes,
            created_at,
            expires_at: created_at + lifetime * DAY,
            account_revision: account.revision,
        };
        let index = state.identity_token_path(&identity.issuer, &identity.subject, id);
        // Publish the index entry first so a crash never leaves a usable token
        // that its account cannot list or revoke.
        state.store.create_strict(&index, Bytes::new()).await?;
        if let Err(error) = state
            .store
            .create_strict(
                &state.token_path(id),
                Bytes::from(serde_json::to_vec(&record)?),
            )
            .await
        {
            let _ = state.delete_if_present(&index).await;
            return Err(error.into());
        }
        Ok(Created {
            token,
            summary: TokenSummary::from(&record),
        })
    }

    async fn list_personal_tokens(
        &self,
        identity: &Identity,
    ) -> Result<Vec<TokenSummary>, AuthError> {
        let state = &self.state;
        let mut summaries = Vec::new();
        let now = now_epoch()?;
        for (id, written) in state
            .identity_token_ids(&identity.issuer, &identity.subject)
            .await?
        {
            match state.read_token(id).await? {
                Some(record)
                    if record.issuer == identity.issuer && record.subject == identity.subject =>
                {
                    summaries.push(TokenSummary::from(&record));
                }
                // A creation that never published its record names no token.
                // Collect it only after any in-flight creation must be done.
                None if written + ORPHAN_GRACE_SECONDS < now => {
                    state
                        .delete_if_present(&state.identity_token_path(
                            &identity.issuer,
                            &identity.subject,
                            id,
                        ))
                        .await?;
                }
                _ => {}
            }
        }
        summaries.sort_by_key(|summary| std::cmp::Reverse(summary.created_at));
        Ok(summaries)
    }

    async fn revoke_personal_token(&self, identity: &Identity, id: Uuid) -> Result<(), AuthError> {
        let state = &self.state;
        let index = state.identity_token_path(&identity.issuer, &identity.subject, id);
        // Another account's token ID is indistinguishable from an absent one.
        if let Some(record) = state.read_token(id).await?
            && record.issuer == identity.issuer
            && record.subject == identity.subject
        {
            // Deleting the record revokes the token; the index follows.
            state.delete_if_present(&state.token_path(id)).await?;
        }
        state.delete_if_present(&index).await
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateRequest {
    name: String,
    scopes: Vec<String>,
    expires_in_days: Option<u64>,
}

#[derive(Serialize)]
struct Created {
    token: String,
    #[serde(flatten)]
    summary: TokenSummary,
}

pub(super) fn routes() -> Router<Arc<Server>> {
    Router::new()
        .route(
            "/api/personal-access-tokens",
            get(list).post(create).layer(DefaultBodyLimit::max(2048)),
        )
        .route("/api/personal-access-tokens/{id}", delete(revoke))
}

/// Only an interactive browser session can manage tokens. A token cannot
/// mint, list, or revoke credentials, even for its own account.
fn manager(server: &Server, principal: &Principal) -> Result<Identity, AuthError> {
    if server.auth.is_none() || !principal.has_browser_session() {
        return Err(AuthError::Invalid);
    }
    principal.identity().ok_or(AuthError::Invalid)
}

async fn list(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
) -> Result<Json<Vec<TokenSummary>>, AuthError> {
    let identity = manager(&server, &principal)?;
    let auth = server.auth.as_ref().ok_or(AuthError::Invalid)?;
    Ok(Json(auth.list_personal_tokens(&identity).await?))
}

async fn create(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Json(request): Json<CreateRequest>,
) -> Result<(StatusCode, Json<Created>), AuthError> {
    let identity = manager(&server, &principal)?;
    let auth = server.auth.as_ref().ok_or(AuthError::Invalid)?;
    let created = auth.create_personal_token(&identity, request).await?;
    Ok((StatusCode::CREATED, Json(created)))
}

async fn revoke(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, AuthError> {
    let identity = manager(&server, &principal)?;
    let auth = server.auth.as_ref().ok_or(AuthError::Invalid)?;
    auth.revoke_personal_token(&identity, id).await?;
    Ok(StatusCode::NO_CONTENT)
}
