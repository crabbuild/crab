//! AWS credential provider using OIDC to STS AssumeRoleWithWebIdentity.

use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use async_trait::async_trait;
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;
use tracing::{debug, warn};

use crate::client_config::AwsOidcConfig;
use crate::credential_provider::CredentialProvider;
use crate::credentials::{CloudCredentials, CredentialResolution};
use crate::error::{AuthError, Result};
use crate::token_cache::TokenCache;

/// Refresh window: credentials within 5 minutes of expiry trigger a refresh.
const REFRESH_WINDOW: Duration = Duration::from_secs(300);

/// Canonical token-cache key for AWS OIDC.
const AWS_OIDC_CACHE_KEY: &str = "aws-oidc";

/// AWS credential provider using OIDC to STS AssumeRoleWithWebIdentity.
pub struct AwsOidcProvider {
    role_arn: String,
    region: String,
    session_duration_secs: u64,
    issuer_url: String,
    client_id: String,
    token_cache: Arc<TokenCache>,
    cached: RwLock<Option<CachedAwsCreds>>,
}

struct CachedAwsCreds {
    creds: CloudCredentials,
    expires_at: SystemTime,
}

impl AwsOidcProvider {
    /// Creates an AWS OIDC provider from validated auth-domain config.
    pub fn new(config: AwsOidcConfig) -> Result<Self> {
        let cache_path = shellexpand_tilde(&config.token_cache_path);
        let token_cache = Arc::new(TokenCache::new(PathBuf::from(cache_path))?);

        Ok(Self {
            role_arn: config.role_arn,
            region: config.region,
            session_duration_secs: config.session_duration_secs,
            issuer_url: config.issuer_url,
            client_id: config.client_id,
            token_cache,
            cached: RwLock::new(None),
        })
    }

    fn is_cached_valid(cached: Option<&CachedAwsCreds>) -> bool {
        match cached {
            Some(c) => {
                let now = SystemTime::now();
                match c.expires_at.duration_since(now) {
                    Ok(remaining) => remaining > REFRESH_WINDOW,
                    Err(_) => false,
                }
            }
            None => false,
        }
    }

    fn session_name(email: Option<&str>) -> String {
        let input = email.unwrap_or("unknown");
        let mut hasher = Sha256::new();
        hasher.update(input.as_bytes());
        let hash = hasher.finalize();
        let hex = hash.iter().fold(String::new(), |mut output, byte| {
            let _ = write!(output, "{byte:02x}");
            output
        });
        format!("crab-{}", &hex[..12])
    }

    async fn assume_role_with_web_identity(
        &self,
        id_token: &str,
        session_name: &str,
    ) -> Result<CloudCredentials> {
        let endpoint = format!("https://sts.{}.amazonaws.com/", self.region);

        let client = reqwest::Client::new();
        let resp = client
            .post(&endpoint)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .body(format!(
                "Action=AssumeRoleWithWebIdentity\
                 &Version=2011-06-15\
                 &RoleArn={}\
                 &RoleSessionName={}\
                 &WebIdentityToken={}\
                 &DurationSeconds={}",
                urlencoded(&self.role_arn),
                urlencoded(session_name),
                urlencoded(id_token),
                self.session_duration_secs,
            ))
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|source| AuthError::AwsStsRequest {
                endpoint: endpoint.clone(),
                source,
            })?;

        let status = resp.status();
        let body = resp
            .text()
            .await
            .map_err(|source| AuthError::AwsStsRequest {
                endpoint: endpoint.clone(),
                source,
            })?;

        if !status.is_success() {
            return Err(classify_sts_error(&body));
        }

        parse_sts_response(&body, &self.region)
    }

    async fn refresh_id_token(&self) -> Result<String> {
        let cached_tokens = self
            .token_cache
            .load(AWS_OIDC_CACHE_KEY)?
            .ok_or(AuthError::NoCredentials)?;

        let refresh_token = cached_tokens.refresh_token.as_deref().ok_or_else(|| {
            AuthError::CredentialsExpired("no refresh token available; run `crab login`".into())
        })?;

        let discovery = crate::oidc::discover(&self.issuer_url).await?;
        let new_tokens =
            crate::oidc::refresh_tokens(&discovery.token_endpoint, &self.client_id, refresh_token)
                .await?;

        self.token_cache.store(
            AWS_OIDC_CACHE_KEY,
            &new_tokens.id_token,
            new_tokens.refresh_token.as_deref(),
        )?;

        Ok(new_tokens.id_token)
    }
}

#[async_trait]
impl CredentialProvider for AwsOidcProvider {
    type Error = AuthError;

    async fn resolve(
        &self,
        _bucket: &str,
        _prefix: &str,
        _operation: &str,
    ) -> Result<CredentialResolution> {
        {
            let guard = self.cached.read().await;
            if let Some(c) = guard.as_ref()
                && Self::is_cached_valid(Some(c))
            {
                return Ok(CredentialResolution::new(c.creds.clone()));
            }
        }

        let cached_tokens = self
            .token_cache
            .load(AWS_OIDC_CACHE_KEY)?
            .ok_or(AuthError::NoCredentials)?;

        let email = cached_tokens.identity.email.as_deref();
        let session_name = Self::session_name(email);

        let mut id_token = cached_tokens.id_token.clone();
        match self
            .assume_role_with_web_identity(&id_token, &session_name)
            .await
        {
            Ok(creds) => {
                let expires_at = extract_expires_at(&creds);
                let mut guard = self.cached.write().await;
                *guard = Some(CachedAwsCreds {
                    creds: creds.clone(),
                    expires_at,
                });
                return Ok(CredentialResolution::new(creds));
            }
            Err(AuthError::CredentialsExpired(_)) => {
                debug!("STS returned ExpiredTokenException, attempting ID token refresh");
                id_token = self.refresh_id_token().await?;
            }
            Err(error) => return Err(error),
        }

        let creds = self
            .assume_role_with_web_identity(&id_token, &session_name)
            .await?;

        let expires_at = extract_expires_at(&creds);
        let mut guard = self.cached.write().await;
        *guard = Some(CachedAwsCreds {
            creds: creds.clone(),
            expires_at,
        });
        Ok(CredentialResolution::new(creds))
    }

    fn needs_refresh(&self) -> bool {
        match self.cached.try_read() {
            Ok(guard) => !Self::is_cached_valid(guard.as_ref()),
            Err(_) => true,
        }
    }

    async fn refresh(&self) -> Result<CredentialResolution> {
        self.refresh_for("", "", "").await
    }

    async fn refresh_for(
        &self,
        bucket: &str,
        prefix: &str,
        operation: &str,
    ) -> Result<CredentialResolution> {
        let mut guard = self.cached.write().await;
        *guard = None;
        drop(guard);
        self.resolve(bucket, prefix, operation).await
    }

    fn identity(&self) -> Option<&str> {
        None
    }
}

fn extract_xml_tag(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(xml[start..end].to_owned())
}

fn parse_sts_response(xml: &str, region: &str) -> Result<CloudCredentials> {
    let access_key_id = required_sts_field(xml, "AccessKeyId")?;
    let secret_access_key = required_sts_field(xml, "SecretAccessKey")?;
    let session_token = required_sts_field(xml, "SessionToken")?;
    let expiration_str = required_sts_field(xml, "Expiration")?;

    let expires_at = parse_iso8601(&expiration_str).unwrap_or_else(|| {
        warn!(
            expiration = %expiration_str,
            "failed to parse STS Expiration, defaulting to 1 hour from now"
        );
        SystemTime::now() + Duration::from_secs(3600)
    });

    Ok(CloudCredentials::Aws {
        access_key_id,
        secret_access_key,
        session_token: Some(session_token),
        expires_at,
        region: region.to_owned(),
    })
}

fn required_sts_field(xml: &str, tag: &str) -> Result<String> {
    extract_xml_tag(xml, tag)
        .ok_or_else(|| AuthError::InvalidCredentialResponse(format!("STS response missing {tag}")))
}

fn classify_sts_error(xml: &str) -> AuthError {
    let error_code = extract_xml_tag(xml, "Code").unwrap_or_default();
    let message = extract_xml_tag(xml, "Message").unwrap_or_else(|| "unknown STS error".into());

    match error_code.as_str() {
        "ExpiredTokenException" => AuthError::CredentialsExpired(format!("STS: {message}")),
        "AccessDeniedException" | "AccessDenied" => {
            AuthError::AwsStsRejected(format!("STS AccessDenied: {message}"))
        }
        "InvalidIdentityToken" => {
            AuthError::AwsStsRejected(format!("STS InvalidIdentityToken: {message}"))
        }
        _ => AuthError::AwsStsRejected(format!("STS error ({error_code}): {message}")),
    }
}

fn parse_iso8601(s: &str) -> Option<SystemTime> {
    let s = s.trim();
    let s = s.strip_suffix('Z')?;
    let (date, time) = s.split_once('T')?;
    let date_parts: Vec<&str> = date.split('-').collect();
    let time_parts: Vec<&str> = time.split(':').collect();

    if date_parts.len() != 3 || time_parts.len() != 3 {
        return None;
    }

    let year: i64 = date_parts[0].parse().ok()?;
    let month: i64 = date_parts[1].parse().ok()?;
    let day: i64 = date_parts[2].parse().ok()?;
    let hour: i64 = time_parts[0].parse().ok()?;
    let min: i64 = time_parts[1].parse().ok()?;
    let sec: i64 = time_parts[2].parse().ok()?;

    let days = days_from_civil(year, month, day);
    let unix_secs = days * 86400 + hour * 3600 + min * 60 + sec;

    if unix_secs < 0 {
        return None;
    }

    Some(std::time::UNIX_EPOCH + Duration::from_secs(unix_secs as u64))
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * m + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn extract_expires_at(creds: &CloudCredentials) -> SystemTime {
    match creds {
        CloudCredentials::Aws { expires_at, .. } => *expires_at,
        _ => SystemTime::now() + Duration::from_secs(3600),
    }
}

fn urlencoded(s: &str) -> String {
    url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
}

fn shellexpand_tilde(path: &str) -> String {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
        if let Ok(home) = std::env::var("USERPROFILE") {
            return format!("{home}/{rest}");
        }
    }
    path.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(region: &str) -> AwsOidcConfig {
        AwsOidcConfig {
            role_arn: "arn:aws:iam::123456789012:role/test".into(),
            region: region.into(),
            session_duration_secs: 3600,
            issuer_url: "https://idp.example.com".into(),
            client_id: "test-client".into(),
            token_cache_path: tempfile::NamedTempFile::new()
                .unwrap()
                .path()
                .to_string_lossy()
                .into_owned(),
        }
    }

    #[test]
    fn session_name_hashes_email() {
        let name = AwsOidcProvider::session_name(Some("alice@example.com"));

        assert!(name.starts_with("crab-"));
        assert_eq!(name.len(), 5 + 12);
        assert_eq!(
            name,
            AwsOidcProvider::session_name(Some("alice@example.com"))
        );
    }

    #[test]
    fn session_name_different_emails_differ() {
        let a = AwsOidcProvider::session_name(Some("alice@example.com"));
        let b = AwsOidcProvider::session_name(Some("bob@example.com"));

        assert_ne!(a, b);
    }

    #[test]
    fn session_name_none_uses_unknown() {
        let name = AwsOidcProvider::session_name(None);

        assert_eq!(name, AwsOidcProvider::session_name(Some("unknown")));
    }

    #[test]
    fn extract_xml_tag_basic() {
        let xml = "<Root><AccessKeyId>AKIA123</AccessKeyId></Root>";

        assert_eq!(extract_xml_tag(xml, "AccessKeyId"), Some("AKIA123".into()));
    }

    #[test]
    fn parse_sts_response_success() {
        let xml = r#"
<AssumeRoleWithWebIdentityResponse>
  <AssumeRoleWithWebIdentityResult>
    <Credentials>
      <AccessKeyId>ASIAXXXXXXXXXXX</AccessKeyId>
      <SecretAccessKey>wJalrXUtnFEMI/K7MDENG</SecretAccessKey>
      <SessionToken>FwoGZXIvYXdzEBY</SessionToken>
      <Expiration>2026-04-24T18:00:00Z</Expiration>
    </Credentials>
  </AssumeRoleWithWebIdentityResult>
</AssumeRoleWithWebIdentityResponse>"#;

        let creds = parse_sts_response(xml, "us-west-2").unwrap();

        match creds {
            CloudCredentials::Aws {
                access_key_id,
                secret_access_key,
                session_token,
                region,
                ..
            } => {
                assert_eq!(access_key_id, "ASIAXXXXXXXXXXX");
                assert_eq!(secret_access_key, "wJalrXUtnFEMI/K7MDENG");
                assert_eq!(session_token.as_deref(), Some("FwoGZXIvYXdzEBY"));
                assert_eq!(region, "us-west-2");
            }
            _ => panic!("expected Aws variant"),
        }
    }

    #[test]
    fn parse_sts_response_missing_field() {
        let xml = "<Root><AccessKeyId>AKIA</AccessKeyId></Root>";

        assert!(matches!(
            parse_sts_response(xml, "us-east-1"),
            Err(AuthError::InvalidCredentialResponse(_))
        ));
    }

    #[test]
    fn classify_sts_error_expired_token() {
        let xml =
            "<Error><Code>ExpiredTokenException</Code><Message>Token expired</Message></Error>";

        assert!(matches!(
            classify_sts_error(xml),
            AuthError::CredentialsExpired(_)
        ));
    }

    #[test]
    fn classify_sts_error_access_denied() {
        let xml = "<Error><Code>AccessDenied</Code><Message>Not authorized</Message></Error>";

        assert!(matches!(
            classify_sts_error(xml),
            AuthError::AwsStsRejected(_)
        ));
    }

    #[test]
    fn parse_iso8601_valid() {
        let ts = parse_iso8601("2026-04-24T18:00:00Z").unwrap();
        let epoch_secs = ts.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();

        assert_eq!(epoch_secs, 1777053600);
    }

    #[test]
    fn parse_iso8601_invalid() {
        assert!(parse_iso8601("not-a-date").is_none());
        assert!(parse_iso8601("2026-04-24").is_none());
        assert!(parse_iso8601("").is_none());
    }

    #[test]
    fn shellexpand_tilde_no_tilde() {
        assert_eq!(shellexpand_tilde("/absolute/path"), "/absolute/path");
    }

    #[test]
    fn urlencoded_special_chars() {
        let encoded = urlencoded("arn:aws:iam::123:role/test");

        assert!(encoded.contains("%3A"));
    }

    #[test]
    fn new_preserves_config_region() {
        let provider = AwsOidcProvider::new(config("ap-southeast-1")).unwrap();

        assert_eq!(provider.region, "ap-southeast-1");
    }

    #[test]
    fn is_cached_valid_returns_false_for_none() {
        assert!(!AwsOidcProvider::is_cached_valid(None));
    }

    #[test]
    fn is_cached_valid_returns_false_within_refresh_window() {
        let cached = Some(CachedAwsCreds {
            creds: CloudCredentials::Aws {
                access_key_id: "AK".into(),
                secret_access_key: "SK".into(),
                session_token: Some("ST".into()),
                expires_at: SystemTime::now() + Duration::from_secs(240),
                region: "us-east-1".into(),
            },
            expires_at: SystemTime::now() + Duration::from_secs(240),
        });

        assert!(!AwsOidcProvider::is_cached_valid(cached.as_ref()));
    }

    #[test]
    fn is_cached_valid_returns_true_outside_refresh_window() {
        let cached = Some(CachedAwsCreds {
            creds: CloudCredentials::Aws {
                access_key_id: "AK".into(),
                secret_access_key: "SK".into(),
                session_token: Some("ST".into()),
                expires_at: SystemTime::now() + Duration::from_secs(1800),
                region: "us-east-1".into(),
            },
            expires_at: SystemTime::now() + Duration::from_secs(1800),
        });

        assert!(AwsOidcProvider::is_cached_valid(cached.as_ref()));
    }
}
