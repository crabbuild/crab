//! Exact HTTP reads from an operator-selected CDN.

use std::{
    ops::Range,
    sync::Arc,
    time::{Duration, SystemTime},
};

use bytes::Bytes;
use object_store::path::Path;
use reqwest::{
    Client, Response, StatusCode,
    header::{self, HeaderMap, HeaderValue},
};
use tokio_util::sync::CancellationToken;
use url::Url;

mod placement;
mod policy;
mod publication;
mod telemetry;

pub use crab_types::storage::CdnPublicationFile;
pub(crate) use placement::NativeAuthority;
pub use placement::{AzureManagementAuthorization, NativeCdnOrigin};
pub use policy::{
    CdnConsumer, CdnPolicy, CdnProfile, CdnReadFamily, CdnVisibility, release_policy_sha256,
};
#[cfg(any(test, feature = "test-support"))]
pub use publication::write_publication_fixture;
pub(crate) use telemetry::PublicTransitionObservation;
pub use telemetry::{PayloadObservation, is_read_evidence_target, observe_service_build};

use crate::{
    Result, StorageError, StorageProviderKind, Store,
    layout::StoreLayout,
    map_object_store_error,
    transport_read_admission::{self, TransportReadReceipt},
};

/// Whether this build can exercise unqualified provider endpoints.
///
/// Shipped builds leave this off. Release-qualified routing is a separate
/// policy decision from the presence of an operator binding.
#[must_use]
pub const fn qualification_enabled() -> bool {
    cfg!(feature = "provider-cdn-qualification")
}

/// A CDN route for one selected storage target and physical repository view.
pub struct CdnRoute {
    pub(crate) target: [u8; 32],
    pub(crate) provider: StorageProviderKind,
    pub(crate) layout: StoreLayout<()>,
    pub(crate) allow_global: bool,
    pub(crate) access: CdnAccess,
    pub(crate) transport: CdnTransport,
    pub(crate) timeout: Duration,
    pub(crate) context: Option<(CdnConsumer, CdnProfile)>,
    pub(crate) families: Vec<CdnReadFamily>,
    pub(crate) publication: Option<publication::Publication>,
    pub(crate) publication_origin: Option<Arc<NativeCdnOrigin>>,
}

impl CdnRoute {
    /// Construct from an independently authorized origin and deployment binding.
    pub fn new(
        target: [u8; 32],
        repo_prefix: String,
        global_prefix: Option<String>,
        access: CdnAccess,
        transport: CdnTransport,
        timeout: Duration,
    ) -> Self {
        Self {
            target,
            provider: StorageProviderKind::Local,
            allow_global: global_prefix.is_some(),
            layout: StoreLayout::with_global_prefix(
                (),
                repo_prefix,
                global_prefix.unwrap_or_else(|| crate::GLOBAL_PREFIX.to_owned()),
            ),
            access,
            transport,
            timeout,
            context: None,
            families: Vec::new(),
            publication: None,
            publication_origin: None,
        }
    }

    /// Pin the immutable inventory that the operator approved for this public export.
    pub async fn with_publication(
        mut self,
        origin: &Store,
        file: &CdnPublicationFile,
        export_origin: &str,
        azure_authority: Option<&dyn AzureManagementAuthorization>,
    ) -> Result<Self> {
        if !matches!(self.access, CdnAccess::Public) {
            return Err(publication::invalid(
                "private routes cannot use a public export receipt",
            ));
        }
        let file = file.clone();
        let layout = self.layout.clone();
        let base = self
            .transport
            .base
            .as_str()
            .trim_end_matches('/')
            .to_owned();
        let export = export_origin.to_owned();
        let publication = tokio::task::spawn_blocking(move || {
            publication::Publication::load(&file, &layout, &base, &export)
        })
        .await
        .map_err(std::io::Error::other)??;
        let authority = origin
            .native_cdn_origin
            .as_ref()
            .filter(|native| native.address == publication.source_origin)
            .filter(|_| origin.read_prefix_target_identity(&Path::ROOT) == Some(&self.target))
            .filter(|_| origin.storage_scope().is_none())
            .ok_or_else(|| StorageError::Forbidden {
                path: "CDN publication receipt does not match the selected native origin".into(),
            })?;
        self.provider = origin.bucket_identity().cloud;
        self.publication_origin = None;
        if !self.transport.is_loopback()
            && self
                .policy()
                .is_some_and(|policy| !policy.families().is_empty())
        {
            authority
                .verify(
                    &publication.source_placement,
                    self.edge_timeout(),
                    azure_authority,
                )
                .await?;
            self.publication_origin = Some(Arc::clone(authority));
        }
        self.publication = Some(publication);
        Ok(self)
    }

    /// Assign the product boundary before installing a provider route.
    #[must_use]
    pub fn for_consumer(mut self, consumer: CdnConsumer, profile: CdnProfile) -> Self {
        self.context = Some((consumer, profile));
        self
    }

    pub(crate) fn policy(&self) -> Option<CdnPolicy> {
        let (consumer, profile) = self.context?;
        Some(CdnPolicy {
            provider: self.provider,
            visibility: match self.access {
                CdnAccess::Public => CdnVisibility::Public,
                CdnAccess::Private(_) => CdnVisibility::Private,
            },
            consumer,
            profile,
        })
    }

    pub(crate) fn edge_timeout(&self) -> Duration {
        // Reserve time for one origin attempt or one credential retry.
        self.timeout / 3
    }
}

/// Viewer authority independently bound to the selected physical export.
pub enum CdnAccess {
    /// Operator-approved public export.
    Public,
    /// An issuer backing an authenticated export.
    Private(Arc<dyn CdnCredentials>),
}

/// Resolve a viewer capability for one physical key.
#[allow(
    clippy::double_must_use,
    reason = "async_trait marks boxed futures must-use"
)]
#[async_trait::async_trait]
pub trait CdnCredentials: Send + Sync {
    /// Return a current credential or refresh a rejected generation.
    /// Return None for a key outside the authorized CDN prefixes.
    async fn credential(&self, key: &Path, rejected: Option<u64>) -> Result<Option<CdnCredential>>;
}

/// A sensitive viewer capability and its process-local generation.
pub struct CdnCredential {
    generation: u64,
    value: String,
    bearer: bool,
    pub(crate) expires_at: Option<SystemTime>,
}

impl CdnCredential {
    #[must_use]
    pub fn cookie(generation: u64, value: String) -> Self {
        Self {
            generation,
            value,
            bearer: false,
            expires_at: None,
        }
    }

    #[must_use]
    pub fn bearer(generation: u64, value: String) -> Self {
        Self {
            generation,
            value,
            bearer: true,
            expires_at: None,
        }
    }

    /// Attach the issuer's expiry for credential diagnostics.
    #[must_use]
    pub fn with_expiration(mut self, expires_at: SystemTime) -> Self {
        self.expires_at = Some(expires_at);
        self
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn header(&self) -> CdnViewer<'_> {
        if self.bearer {
            CdnViewer::Bearer(&self.value)
        } else {
            CdnViewer::Cookie(&self.value)
        }
    }
}

/// An HTTPS client for immutable object bytes with no origin credential access.
#[derive(Debug, Clone)]
pub struct CdnTransport {
    base: Url,
    client: Client,
}

#[derive(Clone)]
pub(crate) struct CdnGet<'a> {
    pub(crate) path: &'a Path,
    pub(crate) size: u64,
    pub(crate) range: Option<Range<u64>>,
    pub(crate) context: Option<(CdnPolicy, CdnReadFamily)>,
}

impl<'a> CdnGet<'a> {
    pub(crate) fn diagnostic(path: &'a Path, size: u64, range: Option<Range<u64>>) -> Self {
        Self {
            path,
            size,
            range,
            context: None,
        }
    }
}

/// Viewer authorization for the approved CDN authority. Values are opaque.
pub enum CdnViewer<'a> {
    /// Public export with no viewer credential.
    Public,
    /// CloudFront or Cloud CDN signed cookie.
    Cookie(&'a str),
    /// Azure private delivery gateway token.
    Bearer(&'a str),
}

impl CdnViewer<'_> {
    fn attach(self, request: reqwest::RequestBuilder) -> Result<reqwest::RequestBuilder> {
        let (name, value) = match self {
            Self::Public => return Ok(request),
            Self::Cookie(value) => (header::COOKIE, HeaderValue::from_str(value)),
            Self::Bearer(value) => (
                header::AUTHORIZATION,
                HeaderValue::from_str(&format!("Bearer {value}")),
            ),
        };
        let mut value = value.map_err(|source| StorageError::ObjectStore {
            source: object_store::Error::Generic {
                store: "CDN viewer credentials",
                source: Box::new(source),
            },
        })?;
        value.set_sensitive(true);
        Ok(request.header(name, value))
    }
}

impl CdnTransport {
    pub(crate) fn is_loopback(&self) -> bool {
        // The only constructors permitting HTTP are explicit loopback fixtures.
        self.base.scheme() == "http"
    }

    pub(crate) fn authority(&self) -> &str {
        &self.base[url::Position::BeforeHost..url::Position::AfterPort]
    }

    pub(crate) fn binding_sha256(&self, private: bool) -> String {
        use sha2::{Digest, Sha256};

        let access = if private { "private" } else { "public" };
        let base = self.base.as_str().trim_end_matches('/');
        format!("{:x}", Sha256::digest(format!("{access}\n{base}")))
    }

    /// Construct an HTTP transport for a loopback fixture.
    #[cfg(feature = "test-support")]
    pub fn loopback_fixture(addr: std::net::SocketAddr) -> Result<Self> {
        if !addr.ip().is_loopback() {
            return Err(StorageError::Forbidden {
                path: "CDN fixture requires a loopback address".into(),
            });
        }
        let base = Url::parse(&format!("http://{addr}")).map_err(|source| {
            StorageError::InvalidObjectStoreUrl {
                url: "CDN fixture".to_owned(),
                source,
            }
        })?;
        let client = client_builder()
            .no_proxy()
            .build()
            .map_err(|source| transport_error(source, false))?;
        Ok(Self { base, client })
    }

    /// Bind to a trusted HTTPS base that maps object keys to the selected origin.
    pub fn new(base: &str) -> Result<Self> {
        let invalid = || StorageError::InvalidStaticEnvTarget {
            target: "CDN base URL".into(),
            reason: "expected a canonical HTTPS base URL".into(),
        };
        // Validate before Url normalizes path separators and dot segments.
        if base.trim() != base
            || base.chars().any(char::is_control)
            || base.contains(['%', '\\'])
            || base.split('/').any(|segment| matches!(segment, "." | ".."))
        {
            return Err(invalid());
        }
        let base = Url::parse(base).map_err(|_| invalid())?;
        if base.scheme() != "https"
            || base.host().is_none()
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
            || base.path().contains('%')
        {
            return Err(invalid());
        }
        let client = client_builder()
            .https_only(true)
            .build()
            .map_err(|source| transport_error(source, false))?;
        Ok(Self { base, client })
    }

    /// Fetch one bounded full body or exact range from an independently authorized key.
    ///
    /// Pass the authenticated total length. The caller owns content verification
    /// and must not publish these bytes until it verifies them. Callers of this
    /// byte-only API cannot use its result as origin metadata or a write token.
    pub(crate) async fn get(
        &self,
        get: CdnGet<'_>,
        viewer: CdnViewer<'_>,
        timeout: Duration,
        cancel: &CancellationToken,
    ) -> Result<Bytes> {
        let CdnGet {
            path,
            size,
            range,
            context,
        } = get;
        if let Some(range) = &range {
            if range.start > range.end || range.end > size {
                return Err(corrupt(
                    path,
                    "range is outside the authenticated object size",
                ));
            }
            if range.is_empty() {
                return Ok(Bytes::new());
            }
        }
        let expected = range.as_ref().map_or(size, |range| range.end - range.start);
        let url = self.url_for(path)?;
        let mut request = self.client.get(url).header(
            header::ACCEPT_ENCODING,
            HeaderValue::from_static("identity"),
        );
        if let Some(range) = &range {
            request = request.header(
                header::RANGE,
                format!("bytes={}-{}", range.start, range.end - 1),
            );
        }
        let request = viewer.attach(request)?;
        let attempt = async {
            let receipt = transport_read_admission::request()
                .await
                .map_err(|source| map_object_store_error(source, path.as_ref()))?;
            let cancelled = async {
                if let Some(receipt) = &receipt {
                    receipt.cancelled().await;
                } else {
                    std::future::pending::<()>().await;
                }
            };
            let transfer = async {
                let mut observation = context
                    .and_then(|context| telemetry::Transfer::start(context, size, range.is_some()));
                let request = match &mut observation {
                    Some(observation) => observation.prepare_request(request),
                    None => request,
                };
                let response = request.send().await.map_err(|source| {
                    // reqwest exposes both DNS and certificate failures as
                    // connect errors. Keep unclassified connect failures terminal.
                    let transient = source.is_timeout();
                    transport_error(source, transient)
                })?;
                if let Some(observation) = &mut observation {
                    observation.response(&response);
                }
                validate_response(&response, path, size, range.as_ref(), expected)?;
                let bytes = collect_body(
                    response,
                    path,
                    expected,
                    receipt.as_ref(),
                    observation.as_mut(),
                )
                .await?;
                if let Some(observation) = &mut observation {
                    observation.complete = true;
                }
                Ok(bytes)
            };
            tokio::select! {
                biased;
                () = cancelled => Err(StorageError::Cancelled),
                result = transfer => result,
            }
        };
        tokio::select! {
            biased;
            () = cancel.cancelled() => Err(StorageError::Cancelled),
            result = tokio::time::timeout(timeout, attempt) => {
                result.map_err(unavailable)?
            }
        }
    }

    pub(crate) async fn probe_mutable_status(
        &self,
        path: &Path,
        viewer: CdnViewer<'_>,
        timeout: Duration,
    ) -> Result<u16> {
        // This opt-in check inspects status only. GET exercises the same
        // viewer path policy as normal reads, with a one-byte requested range.
        let request = viewer.attach(
            self.client
                .get(self.url_for(path)?)
                .header(header::RANGE, "bytes=0-0")
                .header(header::CACHE_CONTROL, "no-store")
                .timeout(timeout),
        )?;
        let probe = async {
            let receipt = transport_read_admission::request()
                .await
                .map_err(|source| map_object_store_error(source, path.as_ref()))?;
            let cancelled = async {
                match &receipt {
                    Some(receipt) => receipt.cancelled().await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                biased;
                () = cancelled => Err(StorageError::Cancelled),
                response = request.send() => response
                    .map(|response| response.status().as_u16())
                    .map_err(|source| transport_error(source, false)),
            }
        };
        tokio::time::timeout(timeout, probe)
            .await
            .map_err(unavailable)?
    }

    fn url_for(&self, path: &Path) -> Result<Url> {
        if path.as_ref().is_empty() || path.as_ref().contains('\\') {
            return Err(corrupt(path, "invalid CDN object key"));
        }
        let mut url = self.base.clone();
        {
            let mut segments = url
                .path_segments_mut()
                .map_err(|_| corrupt(path, "invalid CDN base path"))?;
            // ObjectStore treats Path::as_ref() as the storage key. Encode each
            // raw segment for HTTP so a literal percent remains part of that key.
            segments.pop_if_empty().extend(path.as_ref().split('/'));
        }
        Ok(url)
    }
}

fn client_builder() -> reqwest::ClientBuilder {
    Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .no_zstd()
}

fn validate_response(
    response: &Response,
    path: &Path,
    size: u64,
    range: Option<&Range<u64>>,
    expected: u64,
) -> Result<()> {
    let status = response.status();
    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            return Err(StorageError::Forbidden {
                path: path.to_string(),
            });
        }
        StatusCode::TOO_MANY_REQUESTS
        | StatusCode::INTERNAL_SERVER_ERROR
        | StatusCode::BAD_GATEWAY
        | StatusCode::SERVICE_UNAVAILABLE
        | StatusCode::GATEWAY_TIMEOUT => {
            return Err(unavailable(std::io::Error::other(format!(
                "CDN HTTP {status}"
            ))));
        }
        StatusCode::OK | StatusCode::PARTIAL_CONTENT => {}
        // A CDN 404 is not authoritative and can mask viewer denial.
        _ => {
            return Err(StorageError::ObjectStore {
                source: object_store::Error::Generic {
                    store: "CDN",
                    source: Box::new(std::io::Error::other(format!("unexpected HTTP {status}"))),
                },
            });
        }
    }

    let headers = response.headers();
    if let Some(range) = range {
        if status != StatusCode::PARTIAL_CONTENT
            || !content_range(headers)
                .is_some_and(|(actual, total)| actual == *range && total == size)
        {
            return Err(corrupt(
                path,
                "CDN Content-Range does not match the request",
            ));
        }
    } else if status != StatusCode::OK || headers.contains_key(header::CONTENT_RANGE) {
        return Err(corrupt(path, "CDN did not return a complete body"));
    }
    let mut encodings = headers.get_all(header::CONTENT_ENCODING).iter();
    if encodings.next().is_some_and(|value| value != "identity") || encodings.next().is_some() {
        return Err(corrupt(path, "CDN transformed the object representation"));
    }
    if response.content_length().is_some_and(|len| len != expected) {
        return Err(corrupt(
            path,
            "CDN response length does not match the request",
        ));
    }
    Ok(())
}

pub(crate) fn content_range(headers: &HeaderMap) -> Option<(Range<u64>, u64)> {
    let mut values = headers.get_all(header::CONTENT_RANGE).iter();
    let value = values
        .next()?
        .to_str()
        .ok()?
        .trim()
        .strip_prefix("bytes ")?;
    if values.next().is_some() {
        return None;
    }
    let (offsets, total) = value.split_once('/')?;
    let (start, end) = offsets.split_once('-')?;
    let start = start.parse::<u64>().ok()?;
    let end = end.parse::<u64>().ok()?.checked_add(1)?;
    let total = total.parse::<u64>().ok()?;
    (start < end && end <= total).then_some((start..end, total))
}

async fn collect_body(
    mut response: Response,
    path: &Path,
    expected: u64,
    receipt: Option<&TransportReadReceipt>,
    mut observation: Option<&mut telemetry::Transfer>,
) -> Result<Bytes> {
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|source| transport_error(source, true))?
    {
        if let Some(observation) = observation.as_deref_mut() {
            observation.received(chunk.len() as u64);
        }
        if let Some(receipt) = receipt {
            receipt
                .bytes(chunk.len() as u64)
                .await
                .map_err(|source| map_object_store_error(source, path.as_ref()))?;
        }
        if (body.len() as u64).saturating_add(chunk.len() as u64) > expected {
            return Err(corrupt(path, "CDN body exceeded the authenticated length"));
        }
        body.try_reserve(chunk.len())
            .map_err(|source| StorageError::Io {
                source: std::io::Error::other(source),
            })?;
        body.extend_from_slice(&chunk);
    }
    if body.len() as u64 != expected {
        return Err(corrupt(
            path,
            "CDN body ended before the authenticated length",
        ));
    }
    Ok(Bytes::from(body))
}

fn corrupt(path: &Path, reason: &str) -> StorageError {
    StorageError::CorruptObject {
        path: path.to_string(),
        reason: reason.into(),
    }
}

fn unavailable(source: impl std::error::Error + Send + Sync + 'static) -> StorageError {
    StorageError::NetworkTransient {
        source: object_store::Error::Generic {
            store: "CDN",
            source: Box::new(source),
        },
    }
}

fn transport_error(source: reqwest::Error, transient: bool) -> StorageError {
    let source = source.without_url();
    if transient {
        unavailable(source)
    } else {
        StorageError::ObjectStore {
            source: object_store::Error::Generic {
                store: "CDN",
                source: Box::new(source),
            },
        }
    }
}

#[cfg(test)]
mod tests;
