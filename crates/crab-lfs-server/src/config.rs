//! Configuration for the standalone Git LFS HTTP gateway.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;

use crate::auth::AuthConfig;
use crate::error::LfsServerError;

/// Secret material used to sign short-lived Batch action URLs.
#[derive(Clone)]
pub struct ActionSecret([u8; 32]);

impl std::fmt::Debug for ActionSecret {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ActionSecret")
            .finish_non_exhaustive()
    }
}

impl ActionSecret {
    pub(crate) fn from_value(value: &str) -> Result<Self, LfsServerError> {
        if value.trim().is_empty() {
            return Err(LfsServerError::Config(
                "server.action_secret must not be empty".to_owned(),
            ));
        }
        Ok(Self(*blake3::hash(value.as_bytes()).as_bytes()))
    }

    pub(crate) fn key(&self) -> &[u8; 32] {
        &self.0
    }
}

/// Gateway configuration loaded from TOML.
#[derive(Debug, Clone)]
pub struct LfsServerConfig {
    /// Address on which the gateway listens.
    pub listen_addr: SocketAddr,
    /// Optional externally visible base URL used in Batch action links.
    pub public_url: Option<String>,
    /// Directory used to spool streamed uploads before verification.
    pub spool_dir: PathBuf,
    /// TLS certificate and key, or `None` for a reverse-proxy deployment.
    pub tls: Option<TlsConfig>,
    /// Request authentication mechanism.
    pub auth: AuthConfig,
    /// Whether a trusted TLS-terminating proxy may provide `x-client-cn`.
    pub trust_proxy_mtls: bool,
    /// Optional repository authorization policy.
    pub policy_path: Option<PathBuf>,
    /// Maximum number of objects accepted in one Batch request.
    pub max_batch_objects: usize,
    /// Maximum size of one uploaded LFS object.
    pub max_object_bytes: u64,
    /// Maximum concurrent upload bodies being spooled or committed.
    pub max_uploads: usize,
    /// Maximum duration of one HTTP request.
    pub request_timeout: Duration,
    /// Optional key for signing short-lived Batch action URLs.
    pub action_secret: Option<ActionSecret>,
    /// Lifetime of signed Batch action URLs.
    pub action_ttl: Duration,
    /// Object-store origin URL, including an optional common prefix.
    pub origin_url: String,
}

/// TLS certificate and private-key paths.
#[derive(Debug, Clone)]
pub struct TlsConfig {
    /// PEM-encoded server certificate chain.
    pub cert_path: PathBuf,
    /// PEM-encoded private key.
    pub key_path: PathBuf,
    /// Optional client CA bundle for native mTLS client authentication.
    pub client_ca_path: Option<PathBuf>,
}

const DEFAULT_LISTEN_ADDR: &str = "127.0.0.1:8444";
const DEFAULT_MAX_BATCH_OBJECTS: usize = 1_000;
const DEFAULT_MAX_OBJECT_BYTES: u64 = 64 * 1024 * 1024 * 1024;
const DEFAULT_MAX_UPLOADS: usize = 8;
const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 3_600;
const DEFAULT_ACTION_TTL_SECS: u64 = 900;
const MAX_ACTION_TTL_SECS: u64 = 86_400;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    server: Option<RawServer>,
    tls: Option<RawTls>,
    auth: Option<RawAuth>,
    origin: Option<RawOrigin>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawServer {
    listen_addr: Option<String>,
    public_url: Option<String>,
    spool_dir: Option<String>,
    policy_path: Option<String>,
    max_batch_objects: Option<usize>,
    max_object_bytes: Option<u64>,
    max_uploads: Option<usize>,
    request_timeout_secs: Option<u64>,
    action_secret: Option<String>,
    action_ttl_secs: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTls {
    cert_path: Option<String>,
    key_path: Option<String>,
    client_ca_path: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawAuth {
    mechanism: String,
    users: Option<HashMap<String, String>>,
    trust_proxy_mtls: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOrigin {
    url: Option<String>,
}

impl LfsServerConfig {
    /// Reads and validates a TOML configuration file.
    pub fn from_file(path: &std::path::Path) -> Result<Self, LfsServerError> {
        let contents = std::fs::read_to_string(path).map_err(|source| {
            LfsServerError::Config(format!("failed to read {}: {source}", path.display()))
        })?;
        Self::from_toml_str(&contents)
    }

    /// Parses and validates a TOML configuration string.
    pub fn from_toml_str(value: &str) -> Result<Self, LfsServerError> {
        let raw: RawConfig = toml::from_str(value)
            .map_err(|source| LfsServerError::Config(format!("invalid TOML: {source}")))?;
        Self::from_raw(raw)
    }

    fn from_raw(raw: RawConfig) -> Result<Self, LfsServerError> {
        let server = raw.server.unwrap_or(RawServer {
            listen_addr: None,
            public_url: None,
            spool_dir: None,
            policy_path: None,
            max_batch_objects: None,
            max_object_bytes: None,
            max_uploads: None,
            request_timeout_secs: None,
            action_secret: None,
            action_ttl_secs: None,
        });
        let listen_addr = server
            .listen_addr
            .as_deref()
            .unwrap_or(DEFAULT_LISTEN_ADDR)
            .parse()
            .map_err(|source| LfsServerError::Config(format!("invalid listen_addr: {source}")))?;
        let public_url = server
            .public_url
            .map(|value| value.trim_end_matches('/').to_owned());
        if let Some(url) = public_url.as_deref() {
            let parsed = url::Url::parse(url).map_err(|source| {
                LfsServerError::Config(format!("invalid server.public_url: {source}"))
            })?;
            if !matches!(parsed.scheme(), "http" | "https")
                || parsed.host().is_none()
                || parsed.username() != ""
                || parsed.password().is_some()
                || parsed.query().is_some()
                || parsed.fragment().is_some()
            {
                return Err(LfsServerError::Config(
                    "server.public_url must be an absolute http(s) URL without credentials, query, or fragment"
                        .to_owned(),
                ));
            }
        }

        let max_batch_objects = server
            .max_batch_objects
            .unwrap_or(DEFAULT_MAX_BATCH_OBJECTS);
        let max_object_bytes = server.max_object_bytes.unwrap_or(DEFAULT_MAX_OBJECT_BYTES);
        let max_uploads = server.max_uploads.unwrap_or(DEFAULT_MAX_UPLOADS);
        let timeout_secs = server
            .request_timeout_secs
            .unwrap_or(DEFAULT_REQUEST_TIMEOUT_SECS);
        if max_batch_objects == 0 || max_batch_objects > 10_000 {
            return Err(LfsServerError::Config(
                "server.max_batch_objects must be between 1 and 10000".to_owned(),
            ));
        }
        if max_object_bytes == 0 {
            return Err(LfsServerError::Config(
                "server.max_object_bytes must be greater than zero".to_owned(),
            ));
        }
        if max_uploads == 0 {
            return Err(LfsServerError::Config(
                "server.max_uploads must be greater than zero".to_owned(),
            ));
        }
        if timeout_secs == 0 {
            return Err(LfsServerError::Config(
                "server.request_timeout_secs must be greater than zero".to_owned(),
            ));
        }
        let action_ttl_secs = server.action_ttl_secs.unwrap_or(DEFAULT_ACTION_TTL_SECS);
        if action_ttl_secs == 0 || action_ttl_secs > MAX_ACTION_TTL_SECS {
            return Err(LfsServerError::Config(format!(
                "server.action_ttl_secs must be between 1 and {MAX_ACTION_TTL_SECS}"
            )));
        }

        let tls = parse_tls(raw.tls)?;
        let (auth, trust_proxy_mtls) = AuthConfig::from_raw(raw.auth)?;
        if matches!(auth, AuthConfig::Mtls)
            && !trust_proxy_mtls
            && !tls.as_ref().is_some_and(|tls| tls.client_ca_path.is_some())
        {
            return Err(LfsServerError::Config(
                "mTLS authentication requires tls.client_ca_path or auth.trust_proxy_mtls = true"
                    .to_owned(),
            ));
        }
        let action_secret = std::env::var("CRAB_LFS_ACTION_SECRET")
            .ok()
            .or(server.action_secret)
            .map(|value| ActionSecret::from_value(&value))
            .transpose()?;
        if matches!(auth, AuthConfig::Basic { .. } | AuthConfig::Bearer { .. })
            && action_secret.is_none()
        {
            return Err(LfsServerError::Config(
                "basic and bearer authentication require server.action_secret or CRAB_LFS_ACTION_SECRET because Git LFS action requests do not carry repository credentials"
                    .to_owned(),
            ));
        }
        let origin_url = raw
            .origin
            .and_then(|origin| origin.url)
            .ok_or_else(|| LfsServerError::Config("origin.url is required".to_owned()))?;

        Ok(Self {
            listen_addr,
            public_url,
            spool_dir: server
                .spool_dir
                .map(PathBuf::from)
                .unwrap_or_else(|| std::env::temp_dir().join("crab-lfs-server")),
            tls,
            auth,
            trust_proxy_mtls,
            policy_path: server.policy_path.map(PathBuf::from),
            max_batch_objects,
            max_object_bytes,
            max_uploads,
            request_timeout: Duration::from_secs(timeout_secs),
            action_secret,
            action_ttl: Duration::from_secs(action_ttl_secs),
            origin_url,
        })
    }
}

fn parse_tls(raw: Option<RawTls>) -> Result<Option<TlsConfig>, LfsServerError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let cert_path = raw
        .cert_path
        .map(PathBuf::from)
        .ok_or_else(|| LfsServerError::Config("tls.cert_path is required".to_owned()))?;
    let key_path = raw
        .key_path
        .map(PathBuf::from)
        .ok_or_else(|| LfsServerError::Config("tls.key_path is required".to_owned()))?;
    Ok(Some(TlsConfig {
        cert_path,
        key_path,
        client_ca_path: raw.client_ca_path.map(PathBuf::from),
    }))
}

fn parse_hash(value: &str, field: &str) -> Result<[u8; 32], LfsServerError> {
    if value.len() != 64 || !value.is_ascii() {
        return Err(LfsServerError::Config(format!(
            "{field} must contain 64 hexadecimal characters"
        )));
    }
    let bytes = value.as_bytes();
    let mut hash = [0u8; 32];
    for (index, byte) in hash.iter_mut().enumerate() {
        let high = parse_hash_nibble(bytes[index * 2])
            .ok_or_else(|| LfsServerError::Config(format!("{field} is not valid hexadecimal")))?;
        let low = parse_hash_nibble(bytes[index * 2 + 1])
            .ok_or_else(|| LfsServerError::Config(format!("{field} is not valid hexadecimal")))?;
        *byte = (high << 4) | low;
    }
    Ok(hash)
}

fn parse_hash_nibble(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

impl AuthConfig {
    fn from_raw(raw: Option<RawAuth>) -> Result<(Self, bool), LfsServerError> {
        let raw = raw.ok_or_else(|| LfsServerError::Config("[auth] is required".to_owned()))?;
        let trust_proxy_mtls = raw.trust_proxy_mtls.unwrap_or(false);
        if trust_proxy_mtls && raw.mechanism != "mtls" {
            return Err(LfsServerError::Config(
                "auth.trust_proxy_mtls is only valid with auth.mechanism = \"mtls\"".to_owned(),
            ));
        }
        let users = raw.users.unwrap_or_default();
        match raw.mechanism.as_str() {
            "none" => {
                if !users.is_empty() {
                    return Err(LfsServerError::Config(
                        "auth.users is only valid for basic or bearer authentication".to_owned(),
                    ));
                }
                Ok((Self::None, trust_proxy_mtls))
            }
            "basic" => {
                if users.is_empty() {
                    return Err(LfsServerError::Config(
                        "auth.users must contain at least one user for basic authentication"
                            .to_owned(),
                    ));
                }
                users
                    .into_iter()
                    .map(|(principal, hash)| {
                        if principal.is_empty() || principal.contains(':') {
                            return Err(LfsServerError::Config(
                                "basic usernames must be non-empty and must not contain ':'"
                                    .to_owned(),
                            ));
                        }
                        Ok((
                            principal.clone(),
                            parse_hash(&hash, &format!("auth.users.{principal}"))?,
                        ))
                    })
                    .collect::<Result<HashMap<_, _>, _>>()
                    .map(|users| (Self::Basic { users }, trust_proxy_mtls))
            }
            "bearer" => {
                if users.is_empty() {
                    return Err(LfsServerError::Config(
                        "auth.users must contain at least one principal for bearer authentication"
                            .to_owned(),
                    ));
                }
                users
                    .into_iter()
                    .map(|(principal, hash)| {
                        if principal.is_empty() {
                            return Err(LfsServerError::Config(
                                "bearer principals must not be empty".to_owned(),
                            ));
                        }
                        Ok((
                            principal.clone(),
                            parse_hash(&hash, &format!("auth.users.{principal}"))?,
                        ))
                    })
                    .collect::<Result<HashMap<_, _>, _>>()
                    .map(|users| (Self::Bearer { users }, trust_proxy_mtls))
            }
            "mtls" => {
                if !users.is_empty() {
                    return Err(LfsServerError::Config(
                        "auth.users is only valid for basic or bearer authentication".to_owned(),
                    ));
                }
                Ok((Self::Mtls, trust_proxy_mtls))
            }
            other => Err(LfsServerError::Config(format!(
                "unknown auth.mechanism {other:?}; expected none, basic, bearer, or mtls"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";

    #[test]
    fn parses_basic_users_and_defaults() {
        let config = LfsServerConfig::from_toml_str(&format!(
            "[server]\naction_secret = \"test action secret\"\n[auth]\nmechanism = \"basic\"\n[auth.users]\nalice = \"{HASH}\"\n[origin]\nurl = \"memory://\"\n"
        ))
        .unwrap();
        assert_eq!(config.listen_addr, "127.0.0.1:8444".parse().unwrap());
        assert_eq!(config.max_batch_objects, DEFAULT_MAX_BATCH_OBJECTS);
        assert!(matches!(config.auth, AuthConfig::Basic { .. }));
    }

    #[test]
    fn rejects_basic_auth_without_action_secret() {
        let error = LfsServerConfig::from_toml_str(
            "[auth]\nmechanism = \"basic\"\n[auth.users]\nalice = \"000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f\"\n[origin]\nurl = \"memory://\"\n",
        )
        .unwrap_err();
        assert!(error.to_string().contains("require server.action_secret"));
    }

    #[test]
    fn rejects_invalid_limits_and_unknown_fields() {
        let error = LfsServerConfig::from_toml_str(
            "[auth]\nmechanism = \"none\"\n[origin]\nurl = \"memory://\"\n[server]\nmax_uploads = 0\n",
        )
        .unwrap_err();
        assert!(error.to_string().contains("max_uploads"));

        let error = LfsServerConfig::from_toml_str(
            "[auth]\nmechanism = \"none\"\nunknown = true\n[origin]\nurl = \"memory://\"\n",
        )
        .unwrap_err();
        assert!(error.to_string().contains("invalid TOML"));
    }

    #[test]
    fn rejects_unsafe_public_url_and_non_ascii_hash_without_panicking() {
        let error = LfsServerConfig::from_toml_str(
            "[server]\npublic_url = \"https://example.invalid/lfs?token=unexpected\"\n[auth]\nmechanism = \"none\"\n[origin]\nurl = \"memory://\"\n",
        )
        .unwrap_err();
        assert!(error.to_string().contains("without credentials"));

        let error = LfsServerConfig::from_toml_str(
            "[auth]\nmechanism = \"basic\"\n[auth.users]\nalice = \"éééééééééééééééééééééééééééééééé\"\n[origin]\nurl = \"memory://\"\n",
        )
        .unwrap_err();
        assert!(error.to_string().contains("64 hexadecimal characters"));
    }

    #[test]
    fn m_tls_requires_native_client_ca_or_explicit_proxy_trust() {
        let error = LfsServerConfig::from_toml_str(
            "[auth]\nmechanism = \"mtls\"\n[origin]\nurl = \"memory://\"\n",
        )
        .unwrap_err();
        assert!(error.to_string().contains("client_ca_path"));

        let config = LfsServerConfig::from_toml_str(
            "[auth]\nmechanism = \"mtls\"\ntrust_proxy_mtls = true\n[origin]\nurl = \"memory://\"\n",
        )
        .expect("explicit proxy trust should be accepted");
        assert!(config.trust_proxy_mtls);
    }
}
