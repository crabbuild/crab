use std::time::Duration;

use axum::http::{StatusCode, header};
use bytes::Bytes;
use crab_cell_runtime::Error as CellError;
use crab_cell_runtime::follower::FollowerReceipt;
use crab_cell_runtime::follower::FollowerStore;
use crab_cell_runtime::identity::NodeId;
use crab_cell_runtime::identity::SessionId;
use crab_cell_runtime::node::NodeDirectory;
use crab_cell_runtime::node::log_transport::{
    AppendRequest, NodeLogTransport, RetireRequest, SealRequest, TailRequest,
};
use futures_util::{StreamExt, future::BoxFuture};
use serde::Deserialize;

use super::{NODE_LOG_MEDIA_TYPE, now_ms};
use crate::peer_tls::PeerTlsClient;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_HTTP_BODY_BYTES: usize = (65 * 1024 * 1024) + (4096 * 8) + 12;
const MAX_TAIL_PAGE_FRAMES: usize = 4096;
const MAX_RECOVERY_TAIL_BYTES: usize = 512 * 1024 * 1024;

/// Private mTLS transport for follower shipping and claimed recovery.
#[derive(Clone)]
pub(crate) struct NodeLogHttpTransport {
    directory: NodeDirectory,
    tls: PeerTlsClient,
    session: SessionId,
    local_member: Option<NodeId>,
    local_store: Option<FollowerStore>,
}

impl NodeLogHttpTransport {
    pub(crate) const fn new(
        directory: NodeDirectory,
        tls: PeerTlsClient,
        session: SessionId,
    ) -> Self {
        Self {
            directory,
            tls,
            session,
            local_member: None,
            local_store: None,
        }
    }

    pub(crate) fn with_local_follower(mut self, member: NodeId, store: FollowerStore) -> Self {
        self.local_member = Some(member);
        self.local_store = Some(store);
        self
    }

    fn local_store(&self, member: NodeId) -> Option<FollowerStore> {
        (self.local_member == Some(member))
            .then(|| self.local_store.clone())
            .flatten()
    }

    async fn remote(&self, member: NodeId) -> crab_cell_runtime::Result<RemoteFollower> {
        let advertisement = self
            .directory
            .resolve_node(member, now_ms().map_err(transport_error)?)
            .await?
            .ok_or(CellError::CellNotActive)?;
        Ok(RemoteFollower {
            endpoint: url::Url::parse(advertisement.endpoint()).map_err(transport_error)?,
            client: self
                .tls
                .client(
                    advertisement.certificate(),
                    advertisement.verifying_key()?.to_bytes(),
                )
                .map_err(transport_error)?,
        })
    }

    async fn append_inner(
        &self,
        member: NodeId,
        request: AppendRequest,
    ) -> crab_cell_runtime::Result<FollowerReceipt> {
        if let Some(store) = self.local_store(member) {
            let now_ms = now_ms().map_err(transport_error)?;
            self.directory
                .authorize_log_append(
                    request.leader_session,
                    member,
                    request.log_epoch,
                    request.covered_through,
                    now_ms,
                )
                .await?;
            return store
                .append(
                    request.leader_session,
                    request.log_epoch,
                    request.frames,
                    request.covered_through,
                )
                .await;
        }
        let remote = self.remote(member).await?;
        let path = format!(
            "internal/cells/v1/node-log/{}/{}/append",
            encode_session(request.leader_session),
            request.log_epoch
        );
        let body = encode_append(request)?;
        let response = remote
            .client
            .post(remote.endpoint.join(&path).map_err(transport_error)?)
            .header(header::CONTENT_TYPE, NODE_LOG_MEDIA_TYPE)
            .header(header::CACHE_CONTROL, "no-store")
            .timeout(REQUEST_TIMEOUT)
            .body(body)
            .send()
            .await
            .map_err(transport_unknown)?;
        decode_receipt(response).await
    }

    async fn seal_inner(
        &self,
        member: NodeId,
        request: SealRequest,
    ) -> crab_cell_runtime::Result<FollowerReceipt> {
        if let Some(store) = self.local_store(member) {
            let now_ms = now_ms().map_err(transport_error)?;
            self.directory
                .authorize_log_recovery(
                    request.leader_session,
                    self.session,
                    member,
                    request.log_epoch,
                    now_ms,
                )
                .await?;
            return store.seal(request.leader_session, request.log_epoch).await;
        }
        let remote = self.remote(member).await?;
        let path = format!(
            "internal/cells/v1/node-log/{}/{}/recovery/{}/seal",
            encode_session(request.leader_session),
            request.log_epoch,
            encode_session(self.session)
        );
        let response = remote
            .client
            .post(remote.endpoint.join(&path).map_err(transport_error)?)
            .header(header::CACHE_CONTROL, "no-store")
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(transport_unknown)?;
        decode_receipt(response).await
    }

    async fn retire_inner(
        &self,
        member: NodeId,
        request: RetireRequest,
    ) -> crab_cell_runtime::Result<FollowerReceipt> {
        if let Some(store) = self.local_store(member) {
            let now_ms = now_ms().map_err(transport_error)?;
            self.directory
                .authorize_log_retire(
                    request.leader_session,
                    member,
                    request.log_epoch,
                    request.covered_through,
                    now_ms,
                )
                .await?;
            return store
                .retire(
                    request.leader_session,
                    request.log_epoch,
                    request.covered_through,
                )
                .await;
        }
        let remote = self.remote(member).await?;
        let path = format!(
            "internal/cells/v1/node-log/{}/{}/retire/{}",
            encode_session(request.leader_session),
            request.log_epoch,
            request.covered_through
        );
        let response = remote
            .client
            .post(remote.endpoint.join(&path).map_err(transport_error)?)
            .header(header::CACHE_CONTROL, "no-store")
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(transport_unknown)?;
        decode_receipt(response).await
    }

    async fn tail_inner(
        &self,
        member: NodeId,
        request: TailRequest,
    ) -> crab_cell_runtime::Result<Vec<Bytes>> {
        let mut first = request.first_sequence;
        let mut frames = Vec::new();
        let mut retained_bytes = 0_usize;
        loop {
            let page = self
                .tail_page_inner(
                    member,
                    TailRequest {
                        first_sequence: first,
                        ..request
                    },
                )
                .await?;
            let page_count = page.frames.len();
            if page.frames.is_empty() && page.next_sequence.is_some() {
                return Err(CellError::Peer("follower tail page made no progress"));
            }
            for frame in page.frames {
                retained_bytes = retained_bytes
                    .checked_add(frame.len())
                    .filter(|bytes| *bytes <= MAX_RECOVERY_TAIL_BYTES)
                    .ok_or(CellError::Peer(
                        "follower recovery tail exceeds its byte limit",
                    ))?;
                frames.push(frame);
            }
            let Some(next) = page.next_sequence else {
                return Ok(frames);
            };
            let expected = first
                .checked_add(
                    u64::try_from(page_count)
                        .map_err(|_| CellError::Peer("follower tail frame count overflow"))?,
                )
                .ok_or(CellError::Peer("follower tail sequence overflow"))?;
            if next != expected {
                return Err(CellError::Peer(
                    "follower tail pagination is not contiguous",
                ));
            }
            first = next;
        }
    }

    async fn tail_page_inner(
        &self,
        member: NodeId,
        request: TailRequest,
    ) -> crab_cell_runtime::Result<crab_cell_runtime::follower::FollowerTailPage> {
        if let Some(store) = self.local_store(member) {
            let now_ms = now_ms().map_err(transport_error)?;
            self.directory
                .authorize_log_recovery(
                    request.leader_session,
                    self.session,
                    member,
                    request.log_epoch,
                    now_ms,
                )
                .await?;
            return store
                .read_tail_page(
                    request.leader_session,
                    request.log_epoch,
                    request.first_sequence,
                )
                .await;
        }
        let remote = self.remote(member).await?;
        let path = format!(
            "internal/cells/v1/node-log/{}/{}/recovery/{}/tail/{}",
            encode_session(request.leader_session),
            request.log_epoch,
            encode_session(self.session),
            request.first_sequence
        );
        let response = remote
            .client
            .post(remote.endpoint.join(&path).map_err(transport_error)?)
            .header(header::CACHE_CONTROL, "no-store")
            .timeout(REQUEST_TIMEOUT)
            .send()
            .await
            .map_err(transport_unknown)?;
        let page = decode_tail_page(response).await?;
        Ok(crab_cell_runtime::follower::FollowerTailPage {
            next_sequence: page.next_sequence,
            frames: page.frames,
        })
    }
}

impl NodeLogTransport for NodeLogHttpTransport {
    fn append<'a>(
        &'a self,
        member: NodeId,
        request: AppendRequest,
    ) -> BoxFuture<'a, crab_cell_runtime::Result<FollowerReceipt>> {
        Box::pin(self.append_inner(member, request))
    }

    fn seal<'a>(
        &'a self,
        member: NodeId,
        request: SealRequest,
    ) -> BoxFuture<'a, crab_cell_runtime::Result<FollowerReceipt>> {
        Box::pin(self.seal_inner(member, request))
    }

    fn retire<'a>(
        &'a self,
        member: NodeId,
        request: RetireRequest,
    ) -> BoxFuture<'a, crab_cell_runtime::Result<FollowerReceipt>> {
        Box::pin(self.retire_inner(member, request))
    }

    fn tail<'a>(
        &'a self,
        member: NodeId,
        request: TailRequest,
    ) -> BoxFuture<'a, crab_cell_runtime::Result<Vec<Bytes>>> {
        Box::pin(self.tail_inner(member, request))
    }

    fn tail_page<'a>(
        &'a self,
        member: NodeId,
        request: TailRequest,
    ) -> BoxFuture<'a, crab_cell_runtime::Result<crab_cell_runtime::follower::FollowerTailPage>>
    {
        Box::pin(self.tail_page_inner(member, request))
    }
}

struct RemoteFollower {
    endpoint: url::Url,
    client: reqwest::Client,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawReceipt {
    base_sequence: String,
    durable_through: String,
}

struct TailPage {
    next_sequence: Option<u64>,
    frames: Vec<Bytes>,
}

fn encode_append(request: AppendRequest) -> crab_cell_runtime::Result<Vec<u8>> {
    if request.frames.is_empty() || request.frames.len() > 64 {
        return Err(CellError::Peer("node-log append frame count is invalid"));
    }
    let length = request.frames.iter().try_fold(8_usize, |length, frame| {
        length.checked_add(8)?.checked_add(frame.len())
    });
    let mut body = Vec::with_capacity(
        length
            .filter(|length| *length <= MAX_HTTP_BODY_BYTES)
            .ok_or(CellError::Peer("node-log append exceeds its byte limit"))?,
    );
    body.extend_from_slice(&request.covered_through.to_le_bytes());
    for frame in request.frames {
        body.extend_from_slice(&(frame.len() as u64).to_le_bytes());
        body.extend_from_slice(&frame);
    }
    Ok(body)
}

async fn decode_receipt(response: reqwest::Response) -> crab_cell_runtime::Result<FollowerReceipt> {
    validate_status(response.status())?;
    let body = read_bounded(response, 1024).await?;
    let raw: RawReceipt = serde_json::from_slice(&body)
        .map_err(|_| CellError::Peer("follower receipt is invalid"))?;
    Ok(FollowerReceipt {
        base_sequence: decimal(&raw.base_sequence)?,
        durable_through: decimal(&raw.durable_through)?,
    })
}

async fn decode_tail_page(response: reqwest::Response) -> crab_cell_runtime::Result<TailPage> {
    validate_status(response.status())?;
    if response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        != Some(NODE_LOG_MEDIA_TYPE)
    {
        return Err(CellError::Peer("follower tail media type is invalid"));
    }
    let body = read_bounded(response, MAX_HTTP_BODY_BYTES).await?;
    decode_tail_body(&body)
}

fn decode_tail_body(body: &[u8]) -> crab_cell_runtime::Result<TailPage> {
    let next = u64::from_le_bytes(
        body.get(..8)
            .ok_or(CellError::Peer("follower tail header is truncated"))?
            .try_into()
            .map_err(|_| CellError::Peer("follower tail header is invalid"))?,
    );
    let count = u32::from_le_bytes(
        body.get(8..12)
            .ok_or(CellError::Peer("follower tail header is truncated"))?
            .try_into()
            .map_err(|_| CellError::Peer("follower tail header is invalid"))?,
    ) as usize;
    if count > MAX_TAIL_PAGE_FRAMES {
        return Err(CellError::Peer(
            "follower tail frame count exceeds its limit",
        ));
    }
    let mut cursor = 12_usize;
    let mut frames = Vec::with_capacity(count);
    for _ in 0..count {
        let length_end = cursor
            .checked_add(8)
            .ok_or(CellError::Peer("follower tail length overflow"))?;
        let length = u64::from_le_bytes(
            body.get(cursor..length_end)
                .ok_or(CellError::Peer("follower tail length is truncated"))?
                .try_into()
                .map_err(|_| CellError::Peer("follower tail length is invalid"))?,
        );
        if length == 0 {
            return Err(CellError::Peer("follower tail frame is empty"));
        }
        let length = usize::try_from(length)
            .map_err(|_| CellError::Peer("follower tail frame length is invalid"))?;
        let frame_end = length_end
            .checked_add(length)
            .ok_or(CellError::Peer("follower tail frame length overflow"))?;
        frames.push(Bytes::copy_from_slice(
            body.get(length_end..frame_end)
                .ok_or(CellError::Peer("follower tail frame is truncated"))?,
        ));
        cursor = frame_end;
    }
    if cursor != body.len() {
        return Err(CellError::Peer("follower tail has trailing bytes"));
    }
    Ok(TailPage {
        next_sequence: (next != 0).then_some(next),
        frames,
    })
}

async fn read_bounded(
    response: reqwest::Response,
    limit: usize,
) -> crab_cell_runtime::Result<Vec<u8>> {
    if response
        .content_length()
        .is_some_and(|length| length > limit as u64)
    {
        return Err(CellError::Peer("follower response exceeds its byte limit"));
    }
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(transport_unknown)?;
        if body.len().saturating_add(chunk.len()) > limit {
            return Err(CellError::Peer("follower response exceeds its byte limit"));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn validate_status(status: StatusCode) -> crab_cell_runtime::Result<()> {
    match status {
        StatusCode::OK => Ok(()),
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(CellError::PeerAuthorization(
            "follower rejected the enrolled node session",
        )),
        StatusCode::CONFLICT => Err(CellError::Node("follower lane rejected the request")),
        _ => Err(CellError::Peer("follower transport is unavailable")),
    }
}

fn decimal(value: &str) -> crab_cell_runtime::Result<u64> {
    let parsed = value
        .parse::<u64>()
        .map_err(|_| CellError::Peer("follower receipt decimal is invalid"))?;
    if parsed.to_string() != value {
        return Err(CellError::Peer("follower receipt decimal is not canonical"));
    }
    Ok(parsed)
}

fn encode_session(session: SessionId) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(32);
    for byte in session.as_bytes() {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn transport_error(error: impl std::error::Error + Send + Sync + 'static) -> CellError {
    CellError::PeerTransportUnknown {
        context: "node-log HTTP transport failed",
        source: Box::new(error),
    }
}

fn transport_unknown(error: reqwest::Error) -> CellError {
    transport_error(error)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_and_tail_codecs_are_exact_and_bounded() {
        let append = encode_append(AppendRequest {
            leader_session: SessionId::from_bytes([1; 16]),
            log_epoch: 2,
            frames: vec![Bytes::from_static(b"one"), Bytes::from_static(b"two")],
            covered_through: 7,
        })
        .unwrap();
        assert_eq!(u64::from_le_bytes(append[..8].try_into().unwrap()), 7);

        let page = super::super::encode_tail_page(crab_cell_runtime::follower::FollowerTailPage {
            frames: vec![Bytes::from_static(b"one"), Bytes::from_static(b"two")],
            next_sequence: Some(9),
        })
        .unwrap();
        let parsed = decode_tail_body(&page).unwrap();
        assert_eq!(parsed.next_sequence, Some(9));
        assert_eq!(
            parsed.frames,
            [Bytes::from_static(b"one"), Bytes::from_static(b"two")]
        );
    }

    #[test]
    fn tail_body_rejects_untrusted_shapes_before_recovery() {
        let mut too_many = vec![0_u8; 12];
        too_many[8..12].copy_from_slice(&(MAX_TAIL_PAGE_FRAMES as u32 + 1).to_le_bytes());
        assert!(decode_tail_body(&too_many).is_err());

        let mut empty = vec![0_u8; 20];
        empty[8..12].copy_from_slice(&1_u32.to_le_bytes());
        assert!(decode_tail_body(&empty).is_err());

        let mut trailing = vec![0_u8; 13];
        trailing[8..12].copy_from_slice(&0_u32.to_le_bytes());
        assert!(decode_tail_body(&trailing).is_err());

        let mut truncated = vec![0_u8; 20];
        truncated[8..12].copy_from_slice(&1_u32.to_le_bytes());
        truncated[12..20].copy_from_slice(&4_u64.to_le_bytes());
        assert!(decode_tail_body(&truncated).is_err());
    }
}
