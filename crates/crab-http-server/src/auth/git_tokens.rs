use base64::{Engine as _, engine::general_purpose::STANDARD};

use super::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TokenRequest {
    owner: String,
    repository: String,
    access: GitTokenAccess,
}

#[derive(Clone, Copy, Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
enum GitTokenAccess {
    Read,
    Write,
}

impl Authentication {
    pub(crate) async fn git_principal(&self, headers: &HeaderMap) -> Principal {
        let Some(header) = headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .filter(|value| value.len() <= 1024)
        else {
            return Principal::Anonymous;
        };
        let Some((scheme, encoded)) = header.split_once(' ') else {
            return Principal::Anonymous;
        };
        if !scheme.eq_ignore_ascii_case("Basic") {
            return Principal::Anonymous;
        }
        let Ok(decoded) = STANDARD.decode(encoded) else {
            return Principal::Anonymous;
        };
        let Some(token) = std::str::from_utf8(&decoded)
            .ok()
            .and_then(|value| value.strip_prefix("crab:"))
        else {
            return Principal::Anonymous;
        };
        self.load_git_token(key(token))
            .await
            .map(Principal::Git)
            .unwrap_or(Principal::Anonymous)
    }

    pub(super) async fn load_git_token(&self, token_key: Key) -> Option<Arc<GitToken>> {
        let Some(state) = &self.state else {
            let mut tokens = self.git_tokens.lock().await;
            tokens.retain(|_, token| token.active());
            return tokens.get(&token_key).cloned();
        };
        let body = state
            .store
            .get_with_etag_bounded(&state.path("git-tokens", &token_key), 64 * 1024)
            .await
            .ok()?
            .0;
        let record: StoredGitToken = serde_json::from_slice(&body).ok()?;
        if record.expires_at <= now_epoch().ok()? {
            let _ = state
                .store
                .delete(&state.path("git-tokens", &token_key))
                .await;
            return None;
        }
        let Some(session) = self.load_session(record.session_key).await else {
            let _ = state
                .store
                .delete(&state.path("git-tokens", &token_key))
                .await;
            return None;
        };
        let token = Arc::new(GitToken {
            session,
            owner: record.owner,
            repository: record.repository,
            access: record.access,
            revoked: AtomicBool::new(false),
        });
        token.active().then_some(token)
    }

    pub(super) async fn store_git_token(
        &self,
        token_key: Key,
        token: Arc<GitToken>,
    ) -> Result<(), AuthError> {
        let Some(state) = &self.state else {
            let mut tokens = self.git_tokens.lock().await;
            tokens.retain(|_, token| token.active());
            if tokens.len() >= 4096
                || tokens
                    .values()
                    .filter(|candidate| Arc::ptr_eq(&candidate.session, &token.session))
                    .count()
                    >= 10
            {
                return Err(AuthError::Busy);
            }
            tokens.insert(token_key, token);
            return Ok(());
        };
        let record = StoredGitToken {
            session_key: token.session.session_key,
            owner: token.owner.clone(),
            repository: token.repository.clone(),
            access: token.access,
            expires_at: token.session.expires_at,
        };
        state
            .store
            .create_strict(
                &state.path("git-tokens", &token_key),
                Bytes::from(serde_json::to_vec(&record)?),
            )
            .await?;
        if let Err(error) = self
            .attach_git_token(state, token.session.session_key, token_key)
            .await
        {
            let _ = state
                .store
                .delete(&state.path("git-tokens", &token_key))
                .await;
            return Err(error);
        }
        Ok(())
    }

    async fn attach_git_token(
        &self,
        state: &AuthState,
        session_key: Key,
        token_key: Key,
    ) -> Result<(), AuthError> {
        let path = state.path("sessions", &session_key);
        for _ in 0..STATE_CAS_ATTEMPTS {
            let (body, etag) = state
                .store
                .get_with_etag_bounded(&path, 64 * 1024)
                .await
                .map_err(|error| match error {
                    StorageError::NotFound { .. } => AuthError::Invalid,
                    other => AuthError::Storage(other),
                })?;
            let mut session: StoredSession = serde_json::from_slice(&body)?;
            if session.expires_at <= now_epoch()? {
                return Err(AuthError::Invalid);
            }
            if session.git_tokens.contains(&token_key) {
                return Ok(());
            }
            if session.git_tokens.len() >= 10 {
                return Err(AuthError::Busy);
            }
            session.git_tokens.push(token_key);
            match state
                .store
                .update(&path, Bytes::from(serde_json::to_vec(&session)?), etag)
                .await
            {
                Ok(_) => return Ok(()),
                Err(StorageError::StateConflict { .. }) => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Err(AuthError::Busy)
    }

    pub(super) async fn remove_git_tokens(&self, session: &Arc<Session>) -> Result<(), AuthError> {
        if let Some(state) = &self.state {
            let session_path = state.path("sessions", &session.session_key);
            let mut token_keys = None;
            for _ in 0..STATE_CAS_ATTEMPTS {
                let (body, etag) = state
                    .store
                    .get_with_etag_bounded(&session_path, 64 * 1024)
                    .await
                    .map_err(|error| match error {
                        StorageError::NotFound { .. } => AuthError::Invalid,
                        other => AuthError::Storage(other),
                    })?;
                let mut stored: StoredSession = serde_json::from_slice(&body)?;
                let keys = std::mem::take(&mut stored.git_tokens);
                match state
                    .store
                    .update(
                        &session_path,
                        Bytes::from(serde_json::to_vec(&stored)?),
                        etag,
                    )
                    .await
                {
                    Ok(_) => {
                        token_keys = Some(keys);
                        break;
                    }
                    Err(StorageError::StateConflict { .. }) => continue,
                    Err(error) => return Err(error.into()),
                }
            }
            let token_keys = token_keys.ok_or(AuthError::Busy)?;
            for token_key in token_keys {
                match state
                    .store
                    .delete(&state.path("git-tokens", &token_key))
                    .await
                {
                    Ok(()) | Err(StorageError::NotFound { .. }) => {}
                    Err(error) => return Err(error.into()),
                }
            }
        }
        self.git_tokens.lock().await.retain(|_, token| {
            if Arc::ptr_eq(&token.session, session) {
                token.revoked.store(true, Ordering::Release);
                false
            } else {
                true
            }
        });
        Ok(())
    }
}

pub(crate) async fn issue_git_token(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
    Json(request): Json<TokenRequest>,
) -> Result<Json<serde_json::Value>, AuthError> {
    let auth = server.auth.as_ref().ok_or(AuthError::Invalid)?;
    let repository = server
        .repositories
        .get(&(request.owner.clone(), request.repository.clone()))
        .filter(|repo| match request.access {
            GitTokenAccess::Read => principal.can_read(&repo.config),
            GitTokenAccess::Write => principal.can_write(&repo.config),
        })
        .ok_or(AuthError::Forbidden)?;
    let Principal::User(session) = principal else {
        return Err(AuthError::Invalid);
    };
    if !session.active() {
        return Err(AuthError::Invalid);
    }
    let token = format!("crab_git_{}", CsrfToken::new_random_len(32).secret());
    let expires_in = session.expires_at.saturating_sub(now_epoch()?);
    auth.store_git_token(
        key(&token),
        Arc::new(GitToken {
            session,
            owner: repository.config.owner.clone(),
            repository: repository.config.name.clone(),
            access: match request.access {
                GitTokenAccess::Read => RepositoryAccess::Read,
                GitTokenAccess::Write => RepositoryAccess::Write,
            },
            revoked: AtomicBool::new(false),
        }),
    )
    .await?;
    Ok(Json(
        json!({"username":"crab","token":token,"expires_in":expires_in,
            "owner":request.owner,"repository":request.repository,"access":request.access}),
    ))
}

pub(crate) async fn revoke_git_tokens(
    State(server): State<Arc<Server>>,
    Extension(principal): Extension<Principal>,
) -> Result<StatusCode, AuthError> {
    let auth = server.auth.as_ref().ok_or(AuthError::Invalid)?;
    let Principal::User(session) = principal else {
        return Err(AuthError::Invalid);
    };
    auth.remove_git_tokens(&session).await?;
    Ok(StatusCode::NO_CONTENT)
}
