use std::{collections::BTreeMap, io::Read as _, path::Path, sync::Arc};

use http::{HeaderMap, Uri, header};
use s3s::{
    S3Result,
    access::{S3Access, S3AccessContext},
    auth::{S3Auth, SecretKey},
};

use crate::{Config, CredentialConfig, Error, Result};

#[derive(Clone)]
pub(crate) struct GatewayAuth {
    keys: Arc<BTreeMap<String, Credential>>,
}

struct Credential {
    secret: SecretKey,
    session: Option<SessionCredential>,
    principal: String,
}

struct SessionCredential {
    token: SecretKey,
    expires_at: time::OffsetDateTime,
}

const MAX_SESSION_TOKEN_BYTES: usize = 64 * 1024;

impl GatewayAuth {
    pub(crate) fn load(config: &Config) -> Result<Self> {
        let keys = config
            .credentials
            .iter()
            .map(load_credential)
            .collect::<Result<BTreeMap<_, _>>>()?;
        Ok(Self {
            keys: Arc::new(keys),
        })
    }

    pub(crate) fn principal(&self, access_key: &str) -> Option<&str> {
        self.keys
            .get(access_key)
            .map(|credential| credential.principal.as_str())
    }
}

fn load_credential(config: &CredentialConfig) -> Result<(String, Credential)> {
    let secret = read_credential_file(
        &config.secret_key_file,
        16,
        256,
        "credential secret files must contain one 16-256 character value",
    )?;
    let session = match (config.session_token_file.as_deref(), config.expires_at) {
        (Some(path), Some(expires_at)) => {
            let token = read_credential_file(
                path,
                16,
                MAX_SESSION_TOKEN_BYTES,
                "credential session token files must contain one 16-65536 character value",
            )?;
            Some(SessionCredential {
                token: SecretKey::from(token),
                expires_at,
            })
        }
        (None, None) => None,
        _ => {
            return Err(Error::Config(
                "credential session_token_file and expires_at must be configured together",
            ));
        }
    };
    Ok((
        config.access_key.clone(),
        Credential {
            secret: SecretKey::from(secret),
            session,
            principal: config.principal.clone(),
        },
    ))
}

fn read_credential_file(
    path: &Path,
    min: usize,
    max: usize,
    message: &'static str,
) -> Result<String> {
    let file = std::fs::File::open(path)?;
    let limit = u64::try_from(max)
        .map_err(|_| Error::Config(message))?
        .saturating_add(1);
    let mut value = String::new();
    file.take(limit).read_to_string(&mut value)?;
    let value = value.trim_end_matches(['\r', '\n']);
    if !(min..=max).contains(&value.len()) || value.chars().any(char::is_whitespace) {
        return Err(Error::Config(message));
    }
    Ok(value.to_owned())
}

fn request_session_token(headers: &HeaderMap, uri: &Uri) -> S3Result<Option<(String, bool)>> {
    let mut header_values = headers.get_all("x-amz-security-token").iter();
    let header_token = header_values
        .next()
        .map(|value| {
            let value = value.to_str().map_err(|_| s3s::s3_error!(InvalidToken))?;
            if value.len() > MAX_SESSION_TOKEN_BYTES {
                return Err(s3s::s3_error!(InvalidToken));
            }
            Ok(value.to_owned())
        })
        .transpose()?;
    if header_values.next().is_some() {
        return Err(s3s::s3_error!(InvalidToken));
    }

    let mut query_token = None;
    for (name, value) in uri
        .query()
        .into_iter()
        .flat_map(|query| url::form_urlencoded::parse(query.as_bytes()))
    {
        if name != "X-Amz-Security-Token" {
            continue;
        }
        if value.len() > MAX_SESSION_TOKEN_BYTES || query_token.is_some() {
            return Err(s3s::s3_error!(InvalidToken));
        }
        query_token = Some(value.into_owned());
    }
    if header_token.is_some() && query_token.is_some() {
        return Err(s3s::s3_error!(InvalidToken));
    }
    Ok(header_token
        .map(|token| (token, true))
        .or_else(|| query_token.map(|token| (token, false))))
}

fn sigv4_binds_session_token(headers: &HeaderMap, uri: &Uri, token_in_header: bool) -> bool {
    if let Some(authorization) = headers.get(header::AUTHORIZATION) {
        let Ok(authorization) = authorization.to_str() else {
            return false;
        };
        if !authorization.starts_with("AWS4-HMAC-SHA256 ") {
            return false;
        }
        if !token_in_header {
            return true;
        }
        return authorization
            .split(',')
            .map(str::trim)
            .find_map(|field| field.strip_prefix("SignedHeaders="))
            .is_some_and(|headers| {
                headers
                    .split(';')
                    .any(|name| name == "x-amz-security-token")
            });
    }

    let mut signature_present = false;
    let mut signed_headers_seen = false;
    let mut token_signed = false;
    for (name, value) in url::form_urlencoded::parse(uri.query().unwrap_or_default().as_bytes()) {
        if name == "X-Amz-Signature" {
            signature_present = true;
        } else if name == "X-Amz-SignedHeaders" && !signed_headers_seen {
            signed_headers_seen = true;
            token_signed = value.split(';').any(|name| name == "x-amz-security-token");
        }
    }
    if !signature_present {
        return false;
    }
    !token_in_header || token_signed
}

fn constant_time_eq(actual: &str, expected: &str) -> bool {
    if actual.len() != expected.len() {
        return false;
    }
    actual
        .bytes()
        .zip(expected.bytes())
        .fold(0_u8, |difference, (left, right)| {
            difference | (left ^ right)
        })
        == 0
}

fn validate_session_credential(
    credential: &Credential,
    headers: &HeaderMap,
    uri: &Uri,
    now: time::OffsetDateTime,
) -> S3Result<()> {
    let actual = request_session_token(headers, uri)?;
    let Some(expected) = &credential.session else {
        return actual.map_or(Ok(()), |_| Err(s3s::s3_error!(InvalidToken)));
    };
    let (actual, token_in_header) = actual.ok_or_else(|| s3s::s3_error!(InvalidToken))?;
    if actual.len() > MAX_SESSION_TOKEN_BYTES || !constant_time_eq(&actual, expected.token.expose())
    {
        return Err(s3s::s3_error!(InvalidToken));
    }
    if !sigv4_binds_session_token(headers, uri, token_in_header) {
        return Err(s3s::s3_error!(SignatureDoesNotMatch));
    }
    if now >= expected.expires_at {
        return Err(s3s::s3_error!(ExpiredToken));
    }
    Ok(())
}

#[async_trait::async_trait]
impl S3Auth for GatewayAuth {
    async fn get_secret_key(&self, access_key: &str) -> S3Result<SecretKey> {
        self.keys
            .get(access_key)
            .map(|credential| credential.secret.clone())
            .ok_or_else(|| s3s::s3_error!(InvalidAccessKeyId))
    }
}

#[async_trait::async_trait]
impl S3Access for GatewayAuth {
    async fn check(&self, context: &mut S3AccessContext<'_>) -> S3Result<()> {
        let credentials = context
            .credentials()
            .ok_or_else(|| s3s::s3_error!(AccessDenied, "Signature is required"))?;
        let credential = self
            .keys
            .get(&credentials.access_key)
            .ok_or_else(|| s3s::s3_error!(InvalidAccessKeyId))?;
        validate_session_credential(
            credential,
            context.headers(),
            context.uri(),
            time::OffsetDateTime::now_utc(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "session-token-qualification-value";

    fn credential(expires_at: Option<time::OffsetDateTime>) -> Credential {
        Credential {
            secret: SecretKey::from("qualification-secret"),
            session: expires_at.map(|expires_at| SessionCredential {
                token: SecretKey::from(TOKEN),
                expires_at,
            }),
            principal: "principal".to_owned(),
        }
    }

    fn signed_headers(token: Option<&str>, signs_token: bool) -> HeaderMap {
        let mut headers = HeaderMap::new();
        let signed_headers = if signs_token {
            "host;x-amz-date;x-amz-security-token"
        } else {
            "host;x-amz-date"
        };
        headers.insert(
            header::AUTHORIZATION,
            format!(
                "AWS4-HMAC-SHA256 Credential=key/scope, SignedHeaders={signed_headers}, Signature=value"
            )
            .parse()
            .unwrap(),
        );
        if let Some(token) = token {
            headers.insert("x-amz-security-token", token.parse().unwrap());
        }
        headers
    }

    #[test]
    fn static_credentials_reject_session_tokens() {
        let error = validate_session_credential(
            &credential(None),
            &signed_headers(Some(TOKEN), true),
            &"/bucket/key".parse().unwrap(),
            time::OffsetDateTime::now_utc(),
        )
        .unwrap_err();
        assert_eq!(error.code().as_str(), "InvalidToken");
    }

    #[test]
    fn temporary_credentials_require_exact_signed_header_token() {
        let now = time::OffsetDateTime::now_utc();
        let credential = credential(Some(now + time::Duration::hours(1)));
        validate_session_credential(
            &credential,
            &signed_headers(Some(TOKEN), true),
            &"/bucket/key".parse().unwrap(),
            now,
        )
        .unwrap();
        for (token, signs_token, code) in [
            (None, true, "InvalidToken"),
            (Some("wrong-session-token"), true, "InvalidToken"),
            (Some(TOKEN), false, "SignatureDoesNotMatch"),
        ] {
            let error = validate_session_credential(
                &credential,
                &signed_headers(token, signs_token),
                &"/bucket/key".parse().unwrap(),
                now,
            )
            .unwrap_err();
            assert_eq!(error.code().as_str(), code);
        }
    }

    #[test]
    fn temporary_credentials_accept_signed_query_token_and_expire() {
        let now = time::OffsetDateTime::now_utc();
        let active = credential(Some(now + time::Duration::hours(1)));
        let uri: Uri = format!("/bucket/key?X-Amz-Signature=value&X-Amz-Security-Token={TOKEN}")
            .parse()
            .unwrap();
        validate_session_credential(&active, &HeaderMap::new(), &uri, now).unwrap();

        let expired = credential(Some(now));
        let error =
            validate_session_credential(&expired, &HeaderMap::new(), &uri, now).unwrap_err();
        assert_eq!(error.code().as_str(), "ExpiredToken");
    }

    #[test]
    fn temporary_credentials_reject_ambiguous_token_sources() {
        let now = time::OffsetDateTime::now_utc();
        let credential = credential(Some(now + time::Duration::hours(1)));
        for (headers, uri) in [
            (
                signed_headers(Some(TOKEN), true),
                format!("/bucket/key?X-Amz-Signature=value&X-Amz-Security-Token={TOKEN}"),
            ),
            (
                HeaderMap::new(),
                format!(
                    "/bucket/key?X-Amz-Signature=value&X-Amz-Security-Token={TOKEN}&X-Amz-Security-Token={TOKEN}"
                ),
            ),
        ] {
            let error =
                validate_session_credential(&credential, &headers, &uri.parse().unwrap(), now)
                    .unwrap_err();
            assert_eq!(error.code().as_str(), "InvalidToken");
        }
    }
}
