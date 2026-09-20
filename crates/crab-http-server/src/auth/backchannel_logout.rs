use std::collections::BTreeMap;
use std::sync::Arc;

use axum::{
    body::Bytes,
    extract::{State, rejection::BytesRejection},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use jsonwebtoken::{
    Algorithm, DecodingKey, Validation, decode, decode_header,
    jwk::{JwkSet, KeyAlgorithm, KeyOperations, PublicKeyUse},
};
use openidconnect::core::CoreProviderMetadata;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{AuthError, Authentication, Server, StorageError, bounded_http, now_epoch};

const BACKCHANNEL_EVENT: &str = "http://schemas.openid.net/event/backchannel-logout";

#[derive(Deserialize)]
struct Claims {
    iss: String,
    sub: String,
    exp: u64,
    iat: u64,
    jti: String,
    events: BTreeMap<String, Value>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Replay {
    token_digest: String,
    expires_at: u64,
    completed: bool,
}

fn invalid() -> Response {
    (
        StatusCode::BAD_REQUEST,
        axum::Json(json!({"error":"invalid_request"})),
    )
        .into_response()
}

pub(crate) async fn backchannel_logout(
    State(server): State<Arc<Server>>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Response {
    let Ok(body) = body else {
        return invalid();
    };
    if !headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value.split(';').next().is_some_and(|mime| {
                mime.trim()
                    .eq_ignore_ascii_case("application/x-www-form-urlencoded")
            })
        })
    {
        return invalid();
    }
    let mut values = url::form_urlencoded::parse(&body).filter(|(name, _)| name == "logout_token");
    let Some((_, token)) = values.next() else {
        return invalid();
    };
    if values.next().is_some() || token.is_empty() || token.len() > 48 * 1024 {
        return invalid();
    }
    let Some(auth) = server.auth.as_ref() else {
        return invalid();
    };
    let Ok(_permit) = auth.logout_admission.try_acquire() else {
        return invalid();
    };
    match process(auth, &token).await {
        Ok(()) => StatusCode::OK.into_response(),
        Err(error) => {
            let category = match error {
                AuthError::Storage(_) | AuthError::Json(_) => "state",
                AuthError::Provider(_) => "provider",
                _ => "validation",
            };
            tracing::warn!(category, "back-channel logout rejected");
            invalid()
        }
    }
}

async fn process(auth: &Authentication, token: &str) -> Result<(), AuthError> {
    let claims = verify(auth, token).await?;
    let state = auth.state.as_ref().ok_or(AuthError::Invalid)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab logout replay v1\0");
    hasher.update(claims.iss.as_bytes());
    hasher.update(b"\0");
    hasher.update(claims.jti.as_bytes());
    // Authentication's 24-hour lifecycle must not delete replay protection for
    // a provider token whose signed expiry is later than that lifecycle.
    let root = state
        .prefix
        .strip_suffix("/auth")
        .ok_or(AuthError::Invalid)?;
    let path = object_store::path::Path::from(format!(
        "{root}/logout-replays/{}",
        hasher.finalize().to_hex()
    ));
    let mut replay = Replay {
        token_digest: blake3::hash(token.as_bytes()).to_hex().to_string(),
        expires_at: claims.exp,
        completed: false,
    };
    let etag = match state
        .store
        .create_strict_with_etag(&path, Bytes::from(serde_json::to_vec(&replay)?))
        .await
    {
        Ok(etag) => etag,
        Err(StorageError::StateConflict { .. }) => {
            let (body, etag) = state.store.get_with_etag_bounded(&path, 1024).await?;
            let previous: Replay = serde_json::from_slice(&body)?;
            if previous.token_digest != replay.token_digest
                || previous.expires_at != replay.expires_at
            {
                return Err(AuthError::Invalid);
            }
            if previous.completed {
                return Ok(());
            }
            etag
        }
        Err(error) => return Err(error.into()),
    };
    // Pending records allow delivery retries to finish interrupted revocation;
    // completed records cannot revoke a new session on a duplicate delivery.
    let revoked = auth.revoke_identity(&claims.iss, &claims.sub).await?;
    replay.completed = true;
    match state
        .store
        .update(&path, Bytes::from(serde_json::to_vec(&replay)?), etag)
        .await
    {
        Ok(_) | Err(StorageError::StateConflict { .. }) => {
            tracing::info!(replay_key = %hasher.finalize().to_hex(), issuer = %claims.iss, subject = %claims.sub, revoked, "back-channel logout completed");
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

async fn verify(auth: &Authentication, token: &str) -> Result<Claims, AuthError> {
    let header = decode_header(token).map_err(AuthError::verification)?;
    let kid = header
        .kid
        .as_deref()
        .filter(|value| !value.is_empty())
        .ok_or(AuthError::Invalid)?;
    if !matches!(
        header.alg,
        Algorithm::RS256
            | Algorithm::RS384
            | Algorithm::RS512
            | Algorithm::PS256
            | Algorithm::PS384
            | Algorithm::PS512
            | Algorithm::ES256
            | Algorithm::ES384
            | Algorithm::EdDSA
    ) || header.jwk.is_some()
        || header.crit.as_ref().is_some_and(|items| !items.is_empty())
        || header
            .typ
            .as_deref()
            .is_some_and(|value| value != "logout+jwt" && value != "application/logout+jwt")
    {
        return Err(AuthError::Invalid);
    }
    let discovery_url = auth
        .config
        .issuer
        .join(".well-known/openid-configuration")
        .map_err(AuthError::provider)?;
    let metadata: CoreProviderMetadata = provider_json(auth, discovery_url.as_str()).await?;
    if metadata.issuer() != &auth.config.issuer {
        return Err(AuthError::Invalid);
    }
    let algorithm = serde_json::to_value(header.alg)?;
    if !metadata
        .id_token_signing_alg_values_supported()
        .iter()
        .any(|allowed| serde_json::to_value(allowed).ok().as_ref() == Some(&algorithm))
    {
        return Err(AuthError::Invalid);
    }
    // Preserve JWK key_ops and unsupported entries: the login library's typed
    // JWK round-trip drops those fields before we can enforce key selection.
    let keys: JwkSet = provider_json(auth, metadata.jwks_uri().as_str()).await?;
    let mut matching = keys
        .keys
        .iter()
        .filter(|key| key.common.key_id.as_deref() == Some(kid));
    let key = matching.next().ok_or(AuthError::Invalid)?;
    if matching.next().is_some()
        || key
            .common
            .key_algorithm
            .is_some_and(|algorithm| algorithm != KeyAlgorithm::from(header.alg))
        || key
            .common
            .public_key_use
            .as_ref()
            .is_some_and(|usage| *usage != PublicKeyUse::Signature)
        || key
            .common
            .key_operations
            .as_ref()
            .is_some_and(|ops| !ops.contains(&KeyOperations::Verify))
    {
        return Err(AuthError::Invalid);
    }
    let decoding = DecodingKey::try_from(key).map_err(AuthError::verification)?;
    let mut validation = Validation::new(header.alg);
    validation.set_issuer(&[auth.config.issuer.as_str()]);
    validation.set_audience(&[auth.config.client_id.as_str()]);
    validation.set_required_spec_claims(&["exp", "iss", "aud", "sub"]);
    validation.leeway = 0;
    let claims = decode::<Claims>(token, &decoding, &validation)
        .map_err(AuthError::verification)?
        .claims;
    if [&claims.sub, &claims.jti].iter().any(|value| {
        value.is_empty() || value.chars().count() > 512 || value.chars().any(char::is_control)
    }) || claims.iat > now_epoch()? + 60
        || claims.iat >= claims.exp
        || claims.extra.contains_key("nonce")
        || !matches!(claims.events.get(BACKCHANNEL_EVENT), Some(Value::Object(_)))
    {
        return Err(AuthError::Invalid);
    }
    Ok(claims)
}

async fn provider_json<T: serde::de::DeserializeOwned>(
    auth: &Authentication,
    url: &str,
) -> Result<T, AuthError> {
    let request = axum::http::Request::builder()
        .uri(url)
        .header(header::ACCEPT, "application/json")
        .body(Vec::new())
        .map_err(AuthError::provider)?;
    let response = bounded_http(
        auth.http.clone(),
        request,
        auth.config.public_url.scheme() == "http",
    )
    .await?;
    if response.status() != StatusCode::OK {
        return Err(AuthError::Invalid);
    }
    serde_json::from_slice(response.body()).map_err(AuthError::provider)
}
