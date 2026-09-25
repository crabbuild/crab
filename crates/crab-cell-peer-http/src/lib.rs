//! Owner-resolving HTTP transport for authenticated Cell peer requests.

mod tls;

pub use tls::{LoadedPeerTls, PeerTlsClient, PeerTlsIdentity, PeerTlsListener, TlsError};

use std::{
    collections::VecDeque,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use crab_cell_runtime::Error as CellError;
use crab_cell_runtime::cell::application::ApplicationIdentity;
use crab_cell_runtime::control::authority::CellAuthority;
use crab_cell_runtime::identity::{CellTarget, Digest, SessionId};
use crab_cell_runtime::node::{NodeAdvertisement, NodeDirectory};
use crab_cell_runtime::peer::{PeerRoundTrip, wire as peer_wire};
use futures_util::StreamExt;
use http::{StatusCode, header};

const PEER_FORWARD_PATH: &str = "internal/cells/v1/forward";
const MAX_PEER_CLIENTS: usize = 1_024;
/// Content type accepted by the private peer forwarding endpoint.
pub const PROTOBUF_MEDIA_TYPE: &str = "application/x-protobuf";

/// Builds an mTLS HTTP client pinned to one enrolled peer certificate and key.
pub trait PeerHttpClientFactory: Send + Sync + 'static {
    /// Builds a client that rejects any peer other than the pinned identity.
    fn client(
        &self,
        certificate: Digest,
        public_key: [u8; 32],
    ) -> crab_cell_runtime::Result<reqwest::Client>;
}

/// Restricts which Cell targets this node may forward through a peer.
pub trait PeerTargetScope: Send + Sync + 'static {
    /// Rejects a target outside the product's routing scope.
    fn check_target(&self, target: &CellTarget) -> crab_cell_runtime::Result<()>;
}

impl PeerTargetScope for ApplicationIdentity {
    fn check_target(&self, target: &CellTarget) -> crab_cell_runtime::Result<()> {
        if target.tenant() != self.tenant() || target.application() != self.application() {
            return Err(CellError::PeerAuthorization(
                "Cell target is outside the routed application",
            ));
        }
        Ok(())
    }
}

/// Sends signed Cell requests to the current enrolled owner over HTTP.
pub struct PeerHttpRoundTrip {
    scope: Arc<dyn PeerTargetScope>,
    authority: CellAuthority,
    directory: NodeDirectory,
    tls: Arc<dyn PeerHttpClientFactory>,
    session: SessionId,
    clients: Arc<Mutex<VecDeque<CachedPeerClient>>>,
}

impl PeerHttpRoundTrip {
    /// Binds one application, authority, fleet directory, and local session.
    #[must_use]
    pub fn new(
        scope: Arc<dyn PeerTargetScope>,
        authority: CellAuthority,
        directory: NodeDirectory,
        tls: Arc<dyn PeerHttpClientFactory>,
        session: SessionId,
    ) -> Self {
        Self {
            scope,
            authority,
            directory,
            tls,
            session,
            clients: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    async fn send_inner(
        &self,
        target: CellTarget,
        request: Vec<u8>,
        timeout_ms: u32,
    ) -> crab_cell_runtime::Result<Vec<u8>> {
        if request.len() > crab_cell_runtime::peer::MAX_PEER_REQUEST_BYTES {
            return Err(CellError::Peer("request exceeds peer byte limit"));
        }
        let started = Instant::now();
        let mut last_retry = None;
        for _ in 0..2 {
            let remaining_ms = remaining_timeout(started, timeout_ms)?;
            let owner = tokio::time::timeout(
                Duration::from_millis(u64::from(remaining_ms)),
                self.owner(&target),
            )
            .await
            .map_err(|_| CellError::Deadline)??;
            let remaining_ms = remaining_timeout(started, timeout_ms)?;
            match self.send_once(&owner, request.clone(), remaining_ms).await {
                Ok(PeerHttpAttempt::Reply(reply)) => return Ok(reply),
                Ok(PeerHttpAttempt::Retry(error)) => last_retry = Some(error),
                Ok(PeerHttpAttempt::Unknown(error)) => {
                    return Err(CellError::PeerTransportUnknown {
                        context: "peer HTTP response was lost or invalid",
                        source: Box::new(error),
                    });
                }
                Err(error) => return Err(error),
            }
        }
        Err(last_retry.unwrap_or(CellError::CellNotActive))
    }

    async fn send_to_node_inner(
        &self,
        target: CellTarget,
        node: NodeAdvertisement,
        request: Vec<u8>,
        timeout_ms: u32,
    ) -> crab_cell_runtime::Result<Vec<u8>> {
        self.scope.check_target(&target)?;
        if node.session() == self.session {
            return Err(CellError::CellNotActive);
        }
        let now_ms = now_ms()?;
        if node.expires_at_ms() <= now_ms || node.endpoint().is_empty() {
            return Err(CellError::CellNotActive);
        }
        let owner = RemotePeer {
            session: node.session(),
            endpoint: url::Url::parse(node.endpoint()).map_err(peer_transport)?,
            certificate: node.certificate(),
            public_key: node.verifying_key()?.to_bytes(),
        };
        match self.send_once(&owner, request, timeout_ms).await? {
            PeerHttpAttempt::Reply(reply) => Ok(reply),
            PeerHttpAttempt::Retry(error) => Err(error),
            PeerHttpAttempt::Unknown(error) => Err(CellError::PeerTransportUnknown {
                context: "peer HTTP activation response was lost or invalid",
                source: Box::new(error),
            }),
        }
    }

    async fn owner(&self, target: &CellTarget) -> crab_cell_runtime::Result<RemotePeer> {
        self.scope.check_target(target)?;
        let control = self
            .authority
            .load(target.cell_id())
            .await?
            .ok_or(CellError::CellNotActive)?;
        let owner = control
            .value()
            .owner
            .as_ref()
            .ok_or(CellError::CellNotActive)?;
        if owner.session == self.session {
            return Err(CellError::CellNotActive);
        }
        let now_ms = now_ms()?;
        let enrolled = self
            .directory
            .load(owner.session, now_ms)
            .await?
            .ok_or(CellError::CellNotActive)?;
        let advertisement = enrolled.advertisement();
        if advertisement.endpoint() != owner.endpoint {
            return Err(CellError::PeerAuthorization(
                "Cell owner endpoint is not enrolled",
            ));
        }
        Ok(RemotePeer {
            session: owner.session,
            endpoint: url::Url::parse(advertisement.endpoint()).map_err(peer_transport)?,
            certificate: advertisement.certificate(),
            public_key: advertisement.verifying_key()?.to_bytes(),
        })
    }

    fn client(&self, owner: &RemotePeer) -> crab_cell_runtime::Result<reqwest::Client> {
        {
            let clients = self
                .clients
                .lock()
                .map_err(|_| CellError::Peer("peer HTTP client cache is poisoned"))?;
            if let Some(cached) = clients.iter().find(|cached| cached.matches(owner)) {
                return Ok(cached.client.clone());
            }
        }

        let client = self.tls.client(owner.certificate, owner.public_key)?;
        let mut clients = self
            .clients
            .lock()
            .map_err(|_| CellError::Peer("peer HTTP client cache is poisoned"))?;
        if let Some(cached) = clients.iter().find(|cached| cached.matches(owner)) {
            return Ok(cached.client.clone());
        }
        if clients.len() == MAX_PEER_CLIENTS {
            clients.pop_front();
        }
        clients.push_back(CachedPeerClient {
            session: owner.session,
            certificate: owner.certificate,
            public_key: owner.public_key,
            client: client.clone(),
        });
        Ok(client)
    }

    async fn send_once(
        &self,
        owner: &RemotePeer,
        request: Vec<u8>,
        remaining_ms: u32,
    ) -> crab_cell_runtime::Result<PeerHttpAttempt> {
        let client = self.client(owner)?;
        let url = owner
            .endpoint
            .join(PEER_FORWARD_PATH)
            .map_err(peer_transport)?;
        let response = match client
            .post(url)
            .header(header::CONTENT_TYPE, PROTOBUF_MEDIA_TYPE)
            .header(header::CACHE_CONTROL, "no-store")
            .timeout(Duration::from_millis(u64::from(remaining_ms)))
            .body(request)
            .send()
            .await
        {
            Ok(response) => response,
            Err(error) if error.is_connect() => {
                return Ok(PeerHttpAttempt::Retry(peer_transport(error)));
            }
            Err(error) => return Ok(PeerHttpAttempt::Unknown(peer_transport(error))),
        };
        match response.status() {
            StatusCode::OK => {}
            StatusCode::TOO_MANY_REQUESTS | StatusCode::SERVICE_UNAVAILABLE => {
                return Ok(PeerHttpAttempt::Retry(CellError::CellNotActive));
            }
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                return Err(CellError::PeerAuthorization(
                    "remote node rejected the enrolled peer",
                ));
            }
            status if status.is_server_error() => {
                return Ok(PeerHttpAttempt::Unknown(CellError::Peer(
                    "remote peer returned a server error",
                )));
            }
            _ => return Err(CellError::Peer("remote peer rejected the HTTP request")),
        }
        if response
            .headers()
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            != Some(PROTOBUF_MEDIA_TYPE)
            || response
                .headers()
                .get(header::CACHE_CONTROL)
                .and_then(|value| value.to_str().ok())
                != Some("no-store")
            || response.content_length().is_some_and(|length| {
                length > crab_cell_runtime::peer::MAX_PEER_REQUEST_BYTES as u64
            })
        {
            return Ok(PeerHttpAttempt::Unknown(CellError::Peer(
                "remote peer response metadata is invalid",
            )));
        }
        let mut body = Vec::new();
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(error) => return Ok(PeerHttpAttempt::Unknown(peer_transport(error))),
            };
            if body.len().saturating_add(chunk.len())
                > crab_cell_runtime::peer::MAX_PEER_REQUEST_BYTES
            {
                return Ok(PeerHttpAttempt::Unknown(CellError::Peer(
                    "remote peer response exceeds the byte limit",
                )));
            }
            body.extend_from_slice(&chunk);
        }
        let decoded = match crab_cell_runtime::peer::decode_peer_reply(&body) {
            Ok(decoded) => decoded,
            Err(error) => return Ok(PeerHttpAttempt::Unknown(error)),
        };
        if matches!(
            decoded.outcome,
            Some(peer_wire::peer_reply::Outcome::Error(ref error))
                if error.code == peer_wire::error::Code::Unavailable as i32
                    && error.outcome == peer_wire::error::Outcome::NotStarted as i32
        ) {
            return Ok(PeerHttpAttempt::Retry(CellError::CellNotActive));
        }
        Ok(PeerHttpAttempt::Reply(body))
    }
}

impl PeerRoundTrip for PeerHttpRoundTrip {
    fn send(
        &self,
        target: CellTarget,
        request: Vec<u8>,
        remaining_ms: u32,
    ) -> Pin<Box<dyn Future<Output = crab_cell_runtime::Result<Vec<u8>>> + Send + 'static>> {
        let round_trip = self.clone();
        Box::pin(async move { round_trip.send_inner(target, request, remaining_ms).await })
    }

    fn send_to_node(
        &self,
        target: CellTarget,
        node: NodeAdvertisement,
        request: Vec<u8>,
        remaining_ms: u32,
    ) -> Pin<Box<dyn Future<Output = crab_cell_runtime::Result<Vec<u8>>> + Send + 'static>> {
        let round_trip = self.clone();
        Box::pin(async move {
            round_trip
                .send_to_node_inner(target, node, request, remaining_ms)
                .await
        })
    }
}

impl Clone for PeerHttpRoundTrip {
    fn clone(&self) -> Self {
        Self {
            scope: Arc::clone(&self.scope),
            authority: self.authority.clone(),
            directory: self.directory.clone(),
            tls: Arc::clone(&self.tls),
            session: self.session,
            clients: Arc::clone(&self.clients),
        }
    }
}

struct RemotePeer {
    session: SessionId,
    endpoint: url::Url,
    certificate: Digest,
    public_key: [u8; 32],
}

struct CachedPeerClient {
    session: SessionId,
    certificate: Digest,
    public_key: [u8; 32],
    client: reqwest::Client,
}

impl CachedPeerClient {
    fn matches(&self, owner: &RemotePeer) -> bool {
        self.session == owner.session
            && self.certificate == owner.certificate
            && self.public_key == owner.public_key
    }
}

enum PeerHttpAttempt {
    Reply(Vec<u8>),
    Retry(CellError),
    Unknown(CellError),
}

fn peer_transport(
    source: impl std::error::Error + Send + Sync + 'static,
) -> crab_cell_runtime::Error {
    CellError::PeerTransport {
        context: "peer HTTP transport failed",
        source: Box::new(source),
    }
}

fn now_ms() -> crab_cell_runtime::Result<i64> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| CellError::Peer("system clock precedes Unix epoch"))?;
    i64::try_from(elapsed.as_millis())
        .map_err(|_| CellError::Peer("system clock exceeds peer time range"))
}

fn remaining_timeout(started: Instant, original_ms: u32) -> crab_cell_runtime::Result<u32> {
    let elapsed_ms = u32::try_from(started.elapsed().as_millis()).unwrap_or(u32::MAX);
    original_ms
        .checked_sub(elapsed_ms)
        .filter(|remaining| *remaining > 0)
        .ok_or(CellError::Deadline)
}
