use base64::{Engine as _, engine::general_purpose::STANDARD};

use super::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TokenRequest {
    owner: String,
    repository: String,
    access: RepositoryAccess,
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
        let mut tokens = self.git_tokens.lock().await;
        tokens.retain(|_, token| token.active());
        tokens
            .get(&key(token))
            .cloned()
            .map(Principal::Git)
            .unwrap_or(Principal::Anonymous)
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
            RepositoryAccess::Read => principal.can_read(&repo.config),
            RepositoryAccess::Write => principal.can_write(&repo.config),
        })
        .ok_or(AuthError::Forbidden)?;
    let Principal::User(session) = principal else {
        return Err(AuthError::Invalid);
    };
    if !session.active() {
        return Err(AuthError::Invalid);
    }
    let token = format!("crab_git_{}", CsrfToken::new_random_len(32).secret());
    let expires_in = session
        .expires
        .saturating_duration_since(Instant::now())
        .as_secs();
    let mut tokens = auth.git_tokens.lock().await;
    tokens.retain(|_, token| token.active());
    if tokens.len() >= 4096
        || tokens
            .values()
            .filter(|token| Arc::ptr_eq(&token.session, &session))
            .count()
            >= 10
    {
        return Err(AuthError::Busy);
    }
    tokens.insert(
        key(&token),
        Arc::new(GitToken {
            session,
            owner: repository.config.owner.clone(),
            repository: repository.config.name.clone(),
            access: request.access,
            revoked: AtomicBool::new(false),
        }),
    );
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
    auth.git_tokens.lock().await.retain(|_, token| {
        if Arc::ptr_eq(&token.session, &session) {
            // In-flight operations may retain a principal after its map entry is removed.
            // Revoke the shared record so their next authorization check also fails.
            token.revoked.store(true, Ordering::Release);
            false
        } else {
            true
        }
    });
    Ok(StatusCode::NO_CONTENT)
}
