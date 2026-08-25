//! HTTP client for communicating with the crab cache service.
//!
//! Wraps `reqwest::Client` and provides typed methods for the cache
//! service's REST API.

use std::ops::Range;
use std::path::Path;
use std::time::Duration;

use bytes::Bytes;
use reqwest::Client;
use reqwest::header::{AUTHORIZATION, HeaderName};

use crate::error::{CacheError, Result};
pub use crate::service::{
    CacheObjectHead, CacheObjectRange, CacheServiceAuth, CacheServiceCapabilities,
    CacheServiceLimits, CacheServiceMode, DedupQueryResult, KnownChunk,
};

/// Request body for the dedup query endpoint.
#[derive(serde::Serialize)]
struct DedupQueryRequest {
    repo_path: String,
    chunk_hashes: Vec<String>,
}

/// HTTP client for the crab cache service.
#[derive(Clone, Debug)]
pub struct CacheClient {
    client: Client,
    base_url: String,
    auth_header: Option<(HeaderName, String)>,
}

impl CacheClient {
    /// Create a new cache client targeting the given service URL.
    ///
    /// Configures a 30-second timeout and rustls TLS backend. When `ca_cert`
    /// is provided, the PEM file is added as a trusted root; when
    /// `client_cert` and `client_key` are provided, they are sent as the
    /// native mTLS client identity.
    pub fn new(
        base_url: &str,
        auth: &CacheServiceAuth,
        ca_cert: Option<&Path>,
        client_cert: Option<&Path>,
        client_key: Option<&Path>,
    ) -> Result<Self> {
        let client = build_cache_service_http_client(
            Duration::from_secs(30),
            ca_cert,
            client_cert,
            client_key,
        )?;

        let auth_header = match auth {
            CacheServiceAuth::None | CacheServiceAuth::Mtls => None,
            CacheServiceAuth::Psk(key) => {
                Some((HeaderName::from_static("x-cache-psk"), key.clone()))
            }
            CacheServiceAuth::Bearer(token) => Some((AUTHORIZATION, format!("Bearer {token}"))),
        };

        Ok(Self {
            client,
            base_url: base_url.trim_end_matches('/').to_owned(),
            auth_header,
        })
    }

    /// Attach the stored auth header (if any) to a request builder.
    fn apply_auth(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.auth_header {
            Some((name, value)) => req.header(name.clone(), value.as_str()),
            None => req,
        }
    }

    /// Quick health check: GET `{base_url}/v1/health` with a short timeout.
    ///
    /// Returns `true` if the service responds with 2xx within 2 seconds.
    pub async fn is_healthy(&self) -> bool {
        let url = format!("{}/v1/health", self.base_url);
        let req = self.client.get(&url).timeout(Duration::from_secs(2));
        let result = self.apply_auth(req).send().await;
        matches!(result, Ok(resp) if resp.status().is_success())
    }

    /// Fetch service capabilities after authentication succeeds.
    pub async fn capabilities(&self) -> Result<CacheServiceCapabilities> {
        let url = format!("{}/v1/capabilities", self.base_url);
        let req = self.client.get(&url);
        let resp = self
            .apply_auth(req)
            .send()
            .await
            .map_err(|e| map_reqwest_error(&url, e))?;

        check_status(&url, &resp)?;
        resp.json::<CacheServiceCapabilities>()
            .await
            .map_err(|e| map_reqwest_error(&url, e))
    }

    /// GET an immutable object from the cache service.
    ///
    /// Fetches `{base_url}/v1/{path}` and returns the response body.
    pub async fn get(&self, path: &str) -> Result<Bytes> {
        let url = format!("{}/v1/{}", self.base_url, path);
        let req = self.client.get(&url);
        let resp = self
            .apply_auth(req)
            .send()
            .await
            .map_err(|e| map_reqwest_error(&url, e))?;

        check_get_status(&url, &resp)?;
        resp.bytes().await.map_err(|e| map_reqwest_error(&url, e))
    }

    /// GET an immutable object while bounding response-body consumption.
    pub async fn get_bounded(&self, path: &str, max_bytes: u64) -> Result<Bytes> {
        let url = format!("{}/v1/{}", self.base_url, path);
        let req = self.client.get(&url);
        let mut resp = self
            .apply_auth(req)
            .send()
            .await
            .map_err(|e| map_reqwest_error(&url, e))?;

        check_get_status(&url, &resp)?;
        if let Some(size) = resp.content_length()
            && size > max_bytes
        {
            return Err(CacheError::CorruptObject {
                path: path.to_owned(),
                reason: format!(
                    "cache service object is {size} bytes; bounded read supports at most {max_bytes} bytes"
                ),
            });
        }

        let mut body = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(|e| map_reqwest_error(&url, e))? {
            let next_len =
                body.len()
                    .checked_add(chunk.len())
                    .ok_or_else(|| CacheError::CorruptObject {
                        path: path.to_owned(),
                        reason: "cache service response length overflow".to_owned(),
                    })?;
            if next_len as u64 > max_bytes {
                return Err(CacheError::CorruptObject {
                    path: path.to_owned(),
                    reason: format!(
                        "cache service response exceeded the bounded read limit of {max_bytes} bytes"
                    ),
                });
            }
            body.extend_from_slice(&chunk);
        }
        Ok(Bytes::from(body))
    }

    /// HEAD an immutable object through the cache service.
    pub async fn head(&self, path: &str) -> Result<CacheObjectHead> {
        let url = format!("{}/v1/{}", self.base_url, path);
        let req = self.client.head(&url);
        let resp = self
            .apply_auth(req)
            .send()
            .await
            .map_err(|e| map_reqwest_error(&url, e))?;

        check_head_status(&url, &resp)?;
        let size = response_content_length(&url, &resp)?;
        let cache_status = response_cache_status(&resp);
        Ok(CacheObjectHead { size, cache_status })
    }

    /// GET a byte range from a cached object.
    ///
    /// Sends a `Range: bytes=start-(end-1)` header. The range is
    /// half-open `[start, end)` to match Rust conventions.
    pub async fn get_range(&self, path: &str, range: Range<u64>) -> Result<Bytes> {
        self.get_range_with_status(path, range)
            .await
            .map(|range| range.data)
    }

    /// GET a byte range and retain the cache-status response header.
    pub async fn get_range_with_status(
        &self,
        path: &str,
        range: Range<u64>,
    ) -> Result<CacheObjectRange> {
        if range.start > range.end {
            return Err(CacheError::Service {
                reason: format!("invalid cache range {}..{}", range.start, range.end),
            });
        }
        if range.start == range.end {
            let total_size = range.end;
            return Ok(CacheObjectRange {
                data: Bytes::new(),
                range,
                total_size,
                cache_status: None,
            });
        }

        let url = format!("{}/v1/{}", self.base_url, path);
        let last_byte = range.end - 1;
        let range_value = format!("bytes={}-{}", range.start, last_byte);
        let req = self
            .client
            .get(&url)
            .header(reqwest::header::RANGE, &range_value);
        let mut resp = self
            .apply_auth(req)
            .send()
            .await
            .map_err(|e| map_reqwest_error(&url, e))?;

        let returned = check_range_status(&url, &resp, &range_value, range.start, last_byte)?;
        let cache_status = response_cache_status(&resp);
        let expected_len = returned.range.end - returned.range.start;
        if resp
            .content_length()
            .is_some_and(|length| length > expected_len)
        {
            return Err(CacheError::Service {
                reason: format!(
                    "range response body exceeds {expected_len} bytes for {range_value}: {url}"
                ),
            });
        }

        let mut body = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(|e| map_reqwest_error(&url, e))? {
            let next_len =
                body.len()
                    .checked_add(chunk.len())
                    .ok_or_else(|| CacheError::Service {
                        reason: format!(
                            "range response body length overflow for {range_value}: {url}"
                        ),
                    })?;
            if u64::try_from(next_len).unwrap_or(u64::MAX) > expected_len {
                return Err(CacheError::Service {
                    reason: format!(
                        "range response body exceeds {expected_len} bytes for {range_value}: {url}"
                    ),
                });
            }
            body.extend_from_slice(&chunk);
        }
        check_range_body_len(&url, &range_value, expected_len, body.len())?;
        Ok(CacheObjectRange {
            data: Bytes::from(body),
            range: returned.range,
            total_size: returned.total_size,
            cache_status,
        })
    }

    /// PUT an object to the cache service for push warming.
    ///
    /// Sends the body to `{base_url}/v1/{path}`.
    pub async fn put(&self, path: &str, data: Bytes) -> Result<()> {
        let url = format!("{}/v1/{}", self.base_url, path);
        let req = self.client.put(&url).body(data);
        let resp = self
            .apply_auth(req)
            .send()
            .await
            .map_err(|e| map_reqwest_error(&url, e))?;

        check_status(&url, &resp)?;
        Ok(())
    }

    /// Batch dedup query against the cache service's chunk index.
    ///
    /// Sends hex-encoded chunk hashes and returns which are known/unknown.
    pub async fn dedup_query(
        &self,
        repo_path: &str,
        chunk_hashes: &[[u8; 32]],
    ) -> Result<DedupQueryResult> {
        let url = format!("{}/v1/dedup/query", self.base_url);
        let hex_hashes: Vec<String> = chunk_hashes.iter().map(hex::encode).collect();
        let body = DedupQueryRequest {
            repo_path: repo_path.to_string(),
            chunk_hashes: hex_hashes,
        };

        let req = self.client.post(&url).json(&body);
        let resp = self
            .apply_auth(req)
            .send()
            .await
            .map_err(|e| map_reqwest_error(&url, e))?;

        check_status(&url, &resp)?;
        resp.json::<DedupQueryResult>()
            .await
            .map_err(|e| map_reqwest_error(&url, e))
    }
}

/// Builds a reqwest client with the cache-service TLS contract.
pub fn build_cache_service_http_client(
    timeout: Duration,
    ca_cert: Option<&Path>,
    client_cert: Option<&Path>,
    client_key: Option<&Path>,
) -> Result<Client> {
    let mut builder = Client::builder().timeout(timeout).use_rustls_tls();

    if let Some(ca_path) = ca_cert {
        let pem = std::fs::read(ca_path).map_err(|source| CacheError::ReadCaCert {
            path: ca_path.display().to_string(),
            source,
        })?;
        let certs = reqwest::Certificate::from_pem_bundle(&pem).map_err(|source| {
            CacheError::InvalidCaCert {
                path: ca_path.display().to_string(),
                source,
            }
        })?;
        if certs.is_empty() {
            return Err(CacheError::Service {
                reason: format!(
                    "invalid PEM CA cert {}: no certificates found",
                    ca_path.display()
                ),
            });
        }
        for cert in certs {
            builder = builder.add_root_certificate(cert);
        }
    }

    if let Some(identity) = load_client_identity(client_cert, client_key)? {
        builder = builder.identity(identity);
    }

    builder
        .build()
        .map_err(|source| CacheError::HttpClientBuild { source })
}

fn load_client_identity(
    client_cert: Option<&Path>,
    client_key: Option<&Path>,
) -> Result<Option<reqwest::Identity>> {
    let Some(cert_path) = client_cert else {
        return Ok(None);
    };
    let Some(key_path) = client_key else {
        return Err(CacheError::MissingClientKey);
    };

    let cert = std::fs::read(cert_path).map_err(|source| CacheError::ReadClientCert {
        path: cert_path.display().to_string(),
        source,
    })?;
    let key = std::fs::read(key_path).map_err(|source| CacheError::ReadClientKey {
        path: key_path.display().to_string(),
        source,
    })?;
    let mut identity = Vec::with_capacity(cert.len() + key.len() + 1);
    identity.extend_from_slice(&cert);
    identity.push(b'\n');
    identity.extend_from_slice(&key);
    reqwest::Identity::from_pem(&identity)
        .map(Some)
        .map_err(|source| CacheError::InvalidClientIdentity {
            cert_path: cert_path.display().to_string(),
            key_path: key_path.display().to_string(),
            source,
        })
}

/// Map a reqwest error to a cache-service error with context.
fn map_reqwest_error(url: &str, err: reqwest::Error) -> CacheError {
    if err.is_timeout() {
        tracing::warn!(url, error = %err, "cache service request timed out");
        return CacheError::ServiceRequestTimeout {
            url: url.to_string(),
            source: err,
        };
    }
    if err.is_connect() {
        tracing::warn!(url, error = %err, "cache service connection failed");
        return CacheError::ServiceConnection {
            url: url.to_string(),
            source: err,
        };
    }
    tracing::warn!(url, error = %err, "cache service request failed");
    CacheError::ServiceRequest {
        url: url.to_string(),
        source: err,
    }
}

/// Check the HTTP status code and return an error for 4xx/5xx responses.
fn check_status(url: &str, resp: &reqwest::Response) -> Result<()> {
    let status = resp.status();
    if status.is_success() {
        return Ok(());
    }
    tracing::warn!(
        url,
        status = status.as_u16(),
        "cache service returned error"
    );
    Err(CacheError::Service {
        reason: format!("HTTP {status}: {url}"),
    })
}

/// Check that a full-object GET response satisfies the cache service contract.
fn check_get_status(url: &str, resp: &reqwest::Response) -> Result<()> {
    let status = resp.status();
    if status == reqwest::StatusCode::OK {
        return Ok(());
    }
    tracing::warn!(
        url,
        status = status.as_u16(),
        "cache service returned invalid full-object status"
    );
    Err(CacheError::Service {
        reason: format!("expected HTTP 200 for full-object GET, got HTTP {status}: {url}"),
    })
}

/// Check that a HEAD response satisfies the cache service contract.
fn check_head_status(url: &str, resp: &reqwest::Response) -> Result<()> {
    let status = resp.status();
    if status == reqwest::StatusCode::OK {
        return Ok(());
    }
    tracing::warn!(
        url,
        status = status.as_u16(),
        "cache service returned invalid HEAD status"
    );
    Err(CacheError::Service {
        reason: format!("expected HTTP 200 for HEAD, got HTTP {status}: {url}"),
    })
}

/// Check that a range response satisfies the cache service contract.
fn check_range_status(
    url: &str,
    resp: &reqwest::Response,
    request_range: &str,
    first_byte: u64,
    last_byte: u64,
) -> Result<ValidatedContentRange> {
    let status = resp.status();
    if status != reqwest::StatusCode::PARTIAL_CONTENT {
        tracing::warn!(
            url,
            status = status.as_u16(),
            request_range,
            "cache service returned invalid range status"
        );
        return Err(CacheError::Service {
            reason: format!("expected HTTP 206 for {request_range}, got HTTP {status}: {url}"),
        });
    }

    let content_range = resp
        .headers()
        .get(reqwest::header::CONTENT_RANGE)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| CacheError::Service {
            reason: format!("missing Content-Range for {request_range}: {url}"),
        })?;

    let returned = parse_content_range(url, request_range, content_range, first_byte, last_byte)?;
    if returned.range.start == first_byte
        && returned.range.end <= last_byte + 1
        && (returned.range.end == last_byte + 1 || returned.range.end == returned.total_size)
    {
        return Ok(returned);
    }

    tracing::warn!(
        url,
        request_range,
        content_range,
        "cache service returned mismatched Content-Range"
    );
    Err(CacheError::Service {
        reason: format!("mismatched Content-Range {content_range:?} for {request_range}: {url}"),
    })
}

struct ValidatedContentRange {
    range: Range<u64>,
    total_size: u64,
}

fn parse_content_range(
    url: &str,
    request_range: &str,
    content_range: &str,
    first_byte: u64,
    last_byte: u64,
) -> Result<ValidatedContentRange> {
    let Some(value) = content_range.strip_prefix("bytes ") else {
        return invalid_content_range(url, request_range, content_range);
    };
    let Some((byte_range, total_size)) = value.split_once('/') else {
        return invalid_content_range(url, request_range, content_range);
    };
    let Some((start, end)) = byte_range.split_once('-') else {
        return invalid_content_range(url, request_range, content_range);
    };

    let start = start
        .parse::<u64>()
        .map_err(|_| invalid_content_range_error(url, request_range, content_range))?;
    let end_inclusive = end
        .parse::<u64>()
        .map_err(|_| invalid_content_range_error(url, request_range, content_range))?;
    let total_size = total_size
        .parse::<u64>()
        .map_err(|_| invalid_content_range_error(url, request_range, content_range))?;
    let end_exclusive = end_inclusive
        .checked_add(1)
        .ok_or_else(|| invalid_content_range_error(url, request_range, content_range))?;

    if start != first_byte
        || start > end_inclusive
        || end_exclusive > last_byte + 1
        || end_exclusive > total_size
    {
        return Err(CacheError::Service {
            reason: format!(
                "mismatched Content-Range {content_range:?} for {request_range}: {url}"
            ),
        });
    }

    Ok(ValidatedContentRange {
        range: start..end_exclusive,
        total_size,
    })
}

fn invalid_content_range<T>(url: &str, request_range: &str, content_range: &str) -> Result<T> {
    Err(invalid_content_range_error(
        url,
        request_range,
        content_range,
    ))
}

fn invalid_content_range_error(url: &str, request_range: &str, content_range: &str) -> CacheError {
    CacheError::Service {
        reason: format!("invalid Content-Range {content_range:?} for {request_range}: {url}"),
    }
}

fn response_content_length(url: &str, resp: &reqwest::Response) -> Result<u64> {
    let raw = resp
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| CacheError::Service {
            reason: format!("missing Content-Length for HEAD: {url}"),
        })?;
    raw.parse::<u64>().map_err(|e| CacheError::Service {
        reason: format!("invalid Content-Length {raw:?} for HEAD {url}: {e}"),
    })
}

fn response_cache_status(resp: &reqwest::Response) -> Option<String> {
    resp.headers()
        .get("x-cache")
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

fn check_range_body_len(
    url: &str,
    request_range: &str,
    expected_len: u64,
    actual_len: usize,
) -> Result<()> {
    if actual_len as u64 == expected_len {
        return Ok(());
    }

    tracing::warn!(
        url,
        request_range,
        expected_len,
        actual_len,
        "cache service returned invalid range body length"
    );
    Err(CacheError::Service {
        reason: format!(
            "range body length {actual_len} did not match {expected_len} for {request_range}: {url}"
        ),
    })
}

// Hex encoding helper — avoids pulling in the `hex` crate for a single use.
mod hex {
    use std::fmt::Write as _;

    pub fn encode(bytes: &[u8; 32]) -> String {
        let mut s = String::with_capacity(64);
        for b in bytes {
            let _ = write!(s, "{b:02x}");
        }
        s
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    use axum::Router;
    use axum::body::Body;
    use axum::http::{HeaderValue, StatusCode, header};
    use axum::response::Response;
    use axum::routing::get;
    use tokio::net::TcpListener;

    /// Helper: build a CacheClient with the given auth mode (no CA cert).
    fn client_with_auth(auth: &CacheServiceAuth) -> CacheClient {
        CacheClient::new("http://localhost:9999", auth, None, None, None).unwrap()
    }

    async fn start_range_server(
        status: StatusCode,
        content_range: Option<&'static str>,
        body: &'static [u8],
    ) -> (SocketAddr, tokio::sync::oneshot::Sender<()>) {
        let router = Router::new().route(
            "/v1/{*path}",
            get(move || async move {
                let mut response = Response::new(Body::from(body));
                *response.status_mut() = status;
                if let Some(value) = content_range {
                    response
                        .headers_mut()
                        .insert(header::CONTENT_RANGE, HeaderValue::from_static(value));
                }
                response
            }),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .unwrap();
        });

        (addr, shutdown_tx)
    }

    async fn start_head_server(
        status: StatusCode,
        content_length: Option<&'static str>,
        cache_status: Option<&'static str>,
    ) -> (SocketAddr, tokio::sync::oneshot::Sender<()>) {
        let router = Router::new().route(
            "/v1/{*path}",
            get(|| async { StatusCode::METHOD_NOT_ALLOWED }).head(move || async move {
                let mut response = Response::new(Body::empty());
                *response.status_mut() = status;
                if let Some(value) = content_length {
                    response
                        .headers_mut()
                        .insert(header::CONTENT_LENGTH, HeaderValue::from_static(value));
                }
                if let Some(value) = cache_status {
                    response
                        .headers_mut()
                        .insert("x-cache", HeaderValue::from_static(value));
                }
                response
            }),
        );

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .unwrap();
        });

        (addr, shutdown_tx)
    }

    #[test]
    fn apply_auth_none_adds_no_headers() {
        let client = client_with_auth(&CacheServiceAuth::None);
        assert!(client.auth_header.is_none());

        // Verify apply_auth doesn't panic and returns the builder unchanged.
        let req = client.client.get("http://localhost:9999/v1/test");
        let req = client.apply_auth(req);
        let built = req.build().unwrap();
        // No auth-related headers should be present.
        assert!(built.headers().get("x-cache-psk").is_none());
        assert!(built.headers().get("authorization").is_none());
    }

    #[test]
    fn apply_auth_psk_attaches_x_cache_psk_header() {
        let client = client_with_auth(&CacheServiceAuth::Psk("my-secret-key".to_string()));
        assert_eq!(
            client.auth_header,
            Some((
                HeaderName::from_static("x-cache-psk"),
                "my-secret-key".to_string()
            ))
        );

        let req = client.client.get("http://localhost:9999/v1/test");
        let req = client.apply_auth(req);
        let built = req.build().unwrap();
        assert_eq!(
            built
                .headers()
                .get("x-cache-psk")
                .unwrap()
                .to_str()
                .unwrap(),
            "my-secret-key"
        );
    }

    #[test]
    fn apply_auth_bearer_attaches_authorization_header() {
        let client = client_with_auth(&CacheServiceAuth::Bearer("tok-abc123".to_string()));
        assert_eq!(
            client.auth_header,
            Some((AUTHORIZATION, "Bearer tok-abc123".to_string()))
        );

        let req = client.client.get("http://localhost:9999/v1/test");
        let req = client.apply_auth(req);
        let built = req.build().unwrap();
        assert_eq!(
            built
                .headers()
                .get("authorization")
                .unwrap()
                .to_str()
                .unwrap(),
            "Bearer tok-abc123"
        );
    }

    #[test]
    fn apply_auth_mtls_adds_no_header() {
        let client = client_with_auth(&CacheServiceAuth::Mtls);
        assert!(client.auth_header.is_none());

        let req = client.client.get("http://localhost:9999/v1/test");
        let built = client.apply_auth(req).build().unwrap();

        assert!(built.headers().get("x-cache-psk").is_none());
        assert!(built.headers().get("authorization").is_none());
    }

    #[test]
    fn new_with_invalid_ca_cert_path_returns_error() {
        let result = CacheClient::new(
            "http://localhost:9999",
            &CacheServiceAuth::None,
            Some(Path::new("/nonexistent/ca.pem")),
            None,
            None,
        );
        assert!(result.is_err());
        let err = result.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("failed to read CA cert"));
    }

    #[test]
    fn new_with_invalid_pem_content_returns_error() {
        // Write a temp file with invalid PEM content.
        let dir = tempfile::tempdir().unwrap();
        let cert_path = dir.path().join("bad.pem");
        std::fs::write(&cert_path, b"not a valid PEM").unwrap();

        let result = CacheClient::new(
            "http://localhost:9999",
            &CacheServiceAuth::None,
            Some(&cert_path),
            None,
            None,
        );
        assert!(result.is_err());
        let err = result.unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("invalid PEM CA cert"));
    }

    #[test]
    fn new_with_client_cert_without_key_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let cert_path = dir.path().join("client.pem");
        std::fs::write(&cert_path, b"not a valid client cert").unwrap();

        let result = CacheClient::new(
            "http://localhost:9999",
            &CacheServiceAuth::Mtls,
            None,
            Some(&cert_path),
            None,
        );

        assert!(result.unwrap_err().to_string().contains("client key"));
    }

    #[test]
    fn new_with_invalid_client_identity_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let cert_path = dir.path().join("client.pem");
        let key_path = dir.path().join("client-key.pem");
        std::fs::write(&cert_path, b"not a valid client cert").unwrap();
        std::fs::write(&key_path, b"not a valid client key").unwrap();

        let result = CacheClient::new(
            "http://localhost:9999",
            &CacheServiceAuth::Mtls,
            None,
            Some(&cert_path),
            Some(&key_path),
        );

        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("invalid cache service client cert/key")
        );
    }

    #[test]
    fn base_url_trailing_slash_is_stripped() {
        let client = client_with_auth(&CacheServiceAuth::None);
        assert_eq!(client.base_url, "http://localhost:9999");

        let client2 = CacheClient::new(
            "http://localhost:9999/",
            &CacheServiceAuth::None,
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(client2.base_url, "http://localhost:9999");
    }

    #[tokio::test]
    async fn get_accepts_ok_full_object_response() {
        let (addr, shutdown) = start_range_server(StatusCode::OK, None, b"full object").await;
        let client = CacheClient::new(
            &format!("http://{addr}"),
            &CacheServiceAuth::None,
            None,
            None,
            None,
        )
        .unwrap();

        let data = client.get("xorbs/test").await.unwrap();

        assert_eq!(data, Bytes::from_static(b"full object"));
        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn head_parses_content_length_and_cache_status() {
        let (addr, shutdown) = start_head_server(StatusCode::OK, Some("123"), Some("HIT")).await;
        let client = CacheClient::new(
            &format!("http://{addr}"),
            &CacheServiceAuth::None,
            None,
            None,
            None,
        )
        .unwrap();

        let head = client.head("xorbs/test").await.unwrap();

        assert_eq!(head.size, 123);
        assert_eq!(head.cache_status.as_deref(), Some("HIT"));
        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn head_rejects_non_ok_status() {
        let (addr, shutdown) =
            start_head_server(StatusCode::NOT_FOUND, Some("0"), Some("MISS")).await;
        let client = CacheClient::new(
            &format!("http://{addr}"),
            &CacheServiceAuth::None,
            None,
            None,
            None,
        )
        .unwrap();

        let err = client.head("xorbs/test").await.unwrap_err();

        assert!(err.to_string().contains("expected HTTP 200"));
        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn get_rejects_partial_content_for_full_object_response() {
        let (addr, shutdown) =
            start_range_server(StatusCode::PARTIAL_CONTENT, Some("bytes 0-3/10"), b"part").await;
        let client = CacheClient::new(
            &format!("http://{addr}"),
            &CacheServiceAuth::None,
            None,
            None,
            None,
        )
        .unwrap();

        let err = client.get("xorbs/test").await.unwrap_err();

        assert!(err.to_string().contains("expected HTTP 200"));
        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn get_range_accepts_matching_partial_content() {
        let (addr, shutdown) =
            start_range_server(StatusCode::PARTIAL_CONTENT, Some("bytes 2-4/10"), b"234").await;
        let client = CacheClient::new(
            &format!("http://{addr}"),
            &CacheServiceAuth::None,
            None,
            None,
            None,
        )
        .unwrap();

        let data = client.get_range("xorbs/test", 2..5).await.unwrap();

        assert_eq!(data, Bytes::from_static(b"234"));
        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn get_range_with_status_reports_content_range_metadata() {
        let (addr, shutdown) =
            start_range_server(StatusCode::PARTIAL_CONTENT, Some("bytes 2-4/10"), b"234").await;
        let client = CacheClient::new(
            &format!("http://{addr}"),
            &CacheServiceAuth::None,
            None,
            None,
            None,
        )
        .unwrap();

        let range = client
            .get_range_with_status("xorbs/test", 2..5)
            .await
            .unwrap();

        assert_eq!(range.data, Bytes::from_static(b"234"));
        assert_eq!(range.range, 2..5);
        assert_eq!(range.total_size, 10);
        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn get_range_accepts_content_range_clipped_at_eof() {
        let (addr, shutdown) = start_range_server(
            StatusCode::PARTIAL_CONTENT,
            Some("bytes 2-9/10"),
            b"23456789",
        )
        .await;
        let client = CacheClient::new(
            &format!("http://{addr}"),
            &CacheServiceAuth::None,
            None,
            None,
            None,
        )
        .unwrap();

        let range = client
            .get_range_with_status("xorbs/test", 2..100)
            .await
            .unwrap();

        assert_eq!(range.data, Bytes::from_static(b"23456789"));
        assert_eq!(range.range, 2..10);
        assert_eq!(range.total_size, 10);
        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn get_range_rejects_success_status_without_partial_content() {
        let (addr, shutdown) =
            start_range_server(StatusCode::OK, Some("bytes 2-4/10"), b"0123456789").await;
        let client = CacheClient::new(
            &format!("http://{addr}"),
            &CacheServiceAuth::None,
            None,
            None,
            None,
        )
        .unwrap();

        let err = client.get_range("xorbs/test", 2..5).await.unwrap_err();

        assert!(err.to_string().contains("expected HTTP 206"));
        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn get_range_rejects_missing_content_range() {
        let (addr, shutdown) = start_range_server(StatusCode::PARTIAL_CONTENT, None, b"234").await;
        let client = CacheClient::new(
            &format!("http://{addr}"),
            &CacheServiceAuth::None,
            None,
            None,
            None,
        )
        .unwrap();

        let err = client.get_range("xorbs/test", 2..5).await.unwrap_err();

        assert!(err.to_string().contains("missing Content-Range"));
        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn get_range_rejects_mismatched_content_range() {
        let (addr, shutdown) =
            start_range_server(StatusCode::PARTIAL_CONTENT, Some("bytes 0-2/10"), b"012").await;
        let client = CacheClient::new(
            &format!("http://{addr}"),
            &CacheServiceAuth::None,
            None,
            None,
            None,
        )
        .unwrap();

        let err = client.get_range("xorbs/test", 2..5).await.unwrap_err();

        assert!(err.to_string().contains("mismatched Content-Range"));
        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn get_range_rejects_invalid_content_range_total() {
        let (addr, shutdown) = start_range_server(
            StatusCode::PARTIAL_CONTENT,
            Some("bytes 2-4/not-a-size"),
            b"234",
        )
        .await;
        let client = CacheClient::new(
            &format!("http://{addr}"),
            &CacheServiceAuth::None,
            None,
            None,
            None,
        )
        .unwrap();

        let err = client.get_range("xorbs/test", 2..5).await.unwrap_err();

        assert!(err.to_string().contains("invalid Content-Range"));
        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn get_range_rejects_mismatched_body_length() {
        let (addr, shutdown) =
            start_range_server(StatusCode::PARTIAL_CONTENT, Some("bytes 2-4/10"), b"23").await;
        let client = CacheClient::new(
            &format!("http://{addr}"),
            &CacheServiceAuth::None,
            None,
            None,
            None,
        )
        .unwrap();

        let err = client.get_range("xorbs/test", 2..5).await.unwrap_err();

        assert!(err.to_string().contains("range body length"));
        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn get_range_rejects_oversized_body_before_materializing() {
        let (addr, shutdown) = start_range_server(
            StatusCode::PARTIAL_CONTENT,
            Some("bytes 2-4/10"),
            b"23456789",
        )
        .await;
        let client = CacheClient::new(
            &format!("http://{addr}"),
            &CacheServiceAuth::None,
            None,
            None,
            None,
        )
        .unwrap();

        let err = client.get_range("xorbs/test", 2..5).await.unwrap_err();

        assert!(err.to_string().contains("exceeds 3 bytes"));
        let _ = shutdown.send(());
    }

    #[tokio::test]
    async fn get_range_empty_half_open_range_returns_empty_without_http() {
        let client = CacheClient::new(
            "http://127.0.0.1:9",
            &CacheServiceAuth::None,
            None,
            None,
            None,
        )
        .unwrap();

        let data = client.get_range("xorbs/test", 7..7).await.unwrap();

        assert!(data.is_empty());
    }
}
