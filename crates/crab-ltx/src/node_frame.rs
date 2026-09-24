//! Verified binary envelopes for multiplexed follower replication.

use bytes::Bytes;

use crate::{CrabError, Limits, Result, SegmentInfo};

const MAGIC: &[u8; 4] = b"CNL1";
const VERSION: u16 = 1;
const HEADER_BYTES: usize = 240;

/// Immutable routing and ordering fields authenticated by a node-log frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NodeFrameScope {
    /// Session of the leader that published the frame.
    pub leader_session: [u8; 16],
    /// Node-log epoch the frame was captured under.
    pub log_epoch: u64,
    /// Position of the frame in the node log.
    pub node_sequence: u64,
    /// Application the frame belongs to.
    pub application: [u8; 16],
    /// Cell the frame belongs to.
    pub cell: [u8; 32],
    /// Incarnation that captured the frame.
    pub incarnation: [u8; 16],
    /// Cell epoch the segment was captured under.
    pub cell_epoch: u64,
    /// Root commit sequence the segment publishes.
    pub commit_sequence: u64,
}

/// One canonical node-log frame whose complete LTX body has been verified.
///
/// Construction is restricted to [`encode_node_frame`] and
/// [`inspect_node_frame`], so callers cannot attach trusted metadata to an
/// unchecked body.
#[derive(Clone, Debug)]
pub struct VerifiedNodeFrame {
    scope: NodeFrameScope,
    segment: SegmentInfo,
    body: Bytes,
    encoded: Bytes,
}

impl VerifiedNodeFrame {
    /// Returns the routing fields the frame was authenticated with.
    #[must_use]
    pub const fn scope(&self) -> NodeFrameScope {
        self.scope
    }

    /// Returns the manifest expectations of the embedded segment.
    #[must_use]
    pub const fn segment(&self) -> &SegmentInfo {
        &self.segment
    }

    /// Returns the verified LTX body.
    #[must_use]
    pub fn body(&self) -> &Bytes {
        &self.body
    }

    /// Returns the encoded frame bytes as received.
    #[must_use]
    pub fn encoded(&self) -> &Bytes {
        &self.encoded
    }

    /// Returns the digest followers use to distinguish an exact retry from a
    /// conflicting duplicate sequence.
    #[must_use]
    pub fn digest(&self) -> [u8; 32] {
        *blake3::hash(&self.encoded).as_bytes()
    }
}

/// Verifies an LTX body and encodes its canonical node-log frame.
pub fn encode_node_frame(
    scope: NodeFrameScope,
    segment: SegmentInfo,
    body: Bytes,
    limits: Limits,
) -> Result<VerifiedNodeFrame> {
    validate_scope(scope)?;
    validate_body(&body, &segment, limits)?;

    let capacity = HEADER_BYTES
        .checked_add(body.len())
        .ok_or(CrabError::Limit(crate::LimitKind::NodeFrameBytes))?;
    let mut encoded = Vec::with_capacity(capacity);
    encoded.extend_from_slice(MAGIC);
    encoded.extend_from_slice(&VERSION.to_le_bytes());
    encoded.extend_from_slice(&(HEADER_BYTES as u16).to_le_bytes());
    encoded.extend_from_slice(&scope.leader_session);
    push_u64(&mut encoded, scope.log_epoch);
    push_u64(&mut encoded, scope.node_sequence);
    encoded.extend_from_slice(&scope.application);
    encoded.extend_from_slice(&scope.cell);
    encoded.extend_from_slice(&scope.incarnation);
    push_u64(&mut encoded, scope.cell_epoch);
    push_u64(&mut encoded, scope.commit_sequence);
    push_segment(&mut encoded, &segment);
    push_u64(&mut encoded, body.len() as u64);
    encoded.extend_from_slice(blake3::hash(&body).as_bytes());
    debug_assert_eq!(encoded.len(), HEADER_BYTES);
    encoded.extend_from_slice(&body);
    inspect_node_frame(Bytes::from(encoded), limits)
}

/// Decodes a canonical frame and verifies all declared LTX metadata.
pub fn inspect_node_frame(encoded: Bytes, limits: Limits) -> Result<VerifiedNodeFrame> {
    if encoded.len() < HEADER_BYTES || &encoded[..4] != MAGIC {
        return Err(CrabError::LTXCorrupted);
    }
    let mut cursor = 4;
    if take_u16(&encoded, &mut cursor)? != VERSION
        || usize::from(take_u16(&encoded, &mut cursor)?) != HEADER_BYTES
    {
        return Err(CrabError::LTXCorrupted);
    }
    let scope = NodeFrameScope {
        leader_session: take_array(&encoded, &mut cursor)?,
        log_epoch: take_u64(&encoded, &mut cursor)?,
        node_sequence: take_u64(&encoded, &mut cursor)?,
        application: take_array(&encoded, &mut cursor)?,
        cell: take_array(&encoded, &mut cursor)?,
        incarnation: take_array(&encoded, &mut cursor)?,
        cell_epoch: take_u64(&encoded, &mut cursor)?,
        commit_sequence: take_u64(&encoded, &mut cursor)?,
    };
    let segment = take_segment(&encoded, &mut cursor)?;
    let body_len = take_u64(&encoded, &mut cursor)?;
    let body_digest: [u8; 32] = take_array(&encoded, &mut cursor)?;
    if cursor != HEADER_BYTES
        || body_len != segment.size_bytes
        || body_len > limits.max_capture_bytes
        || body_len > usize::MAX as u64
        || encoded.len() != HEADER_BYTES.saturating_add(body_len as usize)
    {
        return Err(CrabError::Limit(crate::LimitKind::NodeFrameBytes));
    }
    validate_scope(scope)?;
    let body = encoded.slice(HEADER_BYTES..);
    if body_digest != segment.blake3 || body_digest != *blake3::hash(&body).as_bytes() {
        return Err(CrabError::ChecksumMismatch);
    }
    validate_body(&body, &segment, limits)?;
    Ok(VerifiedNodeFrame {
        scope,
        segment,
        body,
        encoded,
    })
}

fn validate_scope(scope: NodeFrameScope) -> Result<()> {
    if scope.leader_session.iter().all(|byte| *byte == 0)
        || scope.application.iter().all(|byte| *byte == 0)
        || scope.cell.iter().all(|byte| *byte == 0)
        || scope.incarnation.iter().all(|byte| *byte == 0)
        || scope.log_epoch == 0
        || scope.node_sequence == 0
        || scope.cell_epoch == 0
        || scope.commit_sequence == 0
    {
        return Err(CrabError::InvalidState("invalid node frame scope"));
    }
    Ok(())
}

fn validate_body(body: &[u8], segment: &SegmentInfo, limits: Limits) -> Result<()> {
    if body.len() as u64 > limits.max_capture_bytes {
        return Err(CrabError::Limit(crate::LimitKind::NodeFrameBody));
    }
    crate::recovery::verify_segment(body, segment, limits)
}

fn push_u64(bytes: &mut Vec<u8>, value: u64) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn push_segment(bytes: &mut Vec<u8>, segment: &SegmentInfo) {
    push_u64(bytes, segment.min_txid);
    push_u64(bytes, segment.max_txid);
    bytes.extend_from_slice(&segment.page_size.to_le_bytes());
    bytes.extend_from_slice(&segment.database_pages.to_le_bytes());
    push_u64(bytes, segment.pre_checksum);
    push_u64(bytes, segment.post_checksum);
    push_u64(bytes, segment.size_bytes);
    bytes.extend_from_slice(&segment.blake3);
}

fn take_segment(bytes: &[u8], cursor: &mut usize) -> Result<SegmentInfo> {
    Ok(SegmentInfo {
        min_txid: take_u64(bytes, cursor)?,
        max_txid: take_u64(bytes, cursor)?,
        page_size: take_u32(bytes, cursor)?,
        database_pages: take_u32(bytes, cursor)?,
        pre_checksum: take_u64(bytes, cursor)?,
        post_checksum: take_u64(bytes, cursor)?,
        size_bytes: take_u64(bytes, cursor)?,
        blake3: take_array(bytes, cursor)?,
    })
}

fn take_u16(bytes: &[u8], cursor: &mut usize) -> Result<u16> {
    Ok(u16::from_le_bytes(take_array(bytes, cursor)?))
}

fn take_u32(bytes: &[u8], cursor: &mut usize) -> Result<u32> {
    Ok(u32::from_le_bytes(take_array(bytes, cursor)?))
}

fn take_u64(bytes: &[u8], cursor: &mut usize) -> Result<u64> {
    Ok(u64::from_le_bytes(take_array(bytes, cursor)?))
}

fn take_array<const N: usize>(bytes: &[u8], cursor: &mut usize) -> Result<[u8; N]> {
    let end = cursor.checked_add(N).ok_or(CrabError::LTXCorrupted)?;
    let value = bytes
        .get(*cursor..end)
        .ok_or(CrabError::LTXCorrupted)?
        .try_into()
        .map_err(|_| CrabError::LTXCorrupted)?;
    *cursor = end;
    Ok(value)
}
