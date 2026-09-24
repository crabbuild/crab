//! Signed node advertisements, leases, proofs, and their wire payloads.
use crate::node::directory::NodeTombstone;

use super::*;

mod codec;

pub(super) use codec::*;

/// Signed, short-lived identity and capacity advertisement for one node session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeAdvertisement {
    pub(super) node: NodeId,
    pub(super) session: SessionId,
    pub(super) endpoint: String,
    pub(super) fleet: Digest,
    pub(super) certificate: Digest,
    pub(super) image: Digest,
    pub(super) release: Digest,
    pub(super) public_key: [u8; 32],
    pub(super) generation: u64,
    pub(super) progress: u64,
    pub(super) issued_at_ms: i64,
    pub(super) expires_at_ms: i64,
    pub(super) module_digests: Vec<Digest>,
    pub(super) peer_versions: Vec<u32>,
    pub(super) failure_domain: NodeFailureDomain,
    pub(super) capacity: NodeCapacity,
    pub(super) log: Option<NodeLogStatus>,
    pub(super) signature: [u8; 64],
    pub(super) placement_version: u32,
    pub(super) placement_signature: [u8; 64],
    pub(super) placement: Option<NodePlacementCapacity>,
}

impl NodeAdvertisement {
    /// Builds and signs a canonical advertisement with the boot-session key.
    #[expect(
        clippy::too_many_arguments,
        reason = "all signed identity fields stay explicit"
    )]
    pub fn sign(
        node: NodeId,
        session: SessionId,
        endpoint: String,
        fleet: Digest,
        certificate: Digest,
        image: Digest,
        release: Digest,
        signing_key: &SigningKey,
        progress: u64,
        issued_at_ms: i64,
        expires_at_ms: i64,
        module_digests: Vec<Digest>,
        peer_versions: Vec<u32>,
        failure_domain: NodeFailureDomain,
        capacity: NodeCapacity,
    ) -> Result<Self> {
        let mut advertisement = Self {
            node,
            session,
            endpoint,
            fleet,
            certificate,
            image,
            release,
            public_key: signing_key.verifying_key().to_bytes(),
            generation: 1,
            progress,
            issued_at_ms,
            expires_at_ms,
            module_digests,
            peer_versions,
            failure_domain,
            capacity,
            log: None,
            signature: [0; 64],
            placement_version: 0,
            placement_signature: [0; 64],
            placement: None,
        };
        advertisement.validate_shape()?;
        advertisement.signature = signing_key.sign(&advertisement.signing_bytes()?).to_bytes();
        Ok(advertisement)
    }

    /// Returns the node identity this advertisement describes.
    #[must_use]
    pub const fn node(&self) -> NodeId {
        self.node
    }

    /// Returns the boot session that signed the advertisement.
    #[must_use]
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the peer endpoint the node advertises.
    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    /// Returns the fleet the node belongs to.
    #[must_use]
    pub const fn fleet(&self) -> Digest {
        self.fleet
    }

    /// Returns the certificate the node authenticates with.
    #[must_use]
    pub const fn certificate(&self) -> Digest {
        self.certificate
    }

    /// Returns the compiled image the node runs.
    #[must_use]
    pub const fn image(&self) -> Digest {
        self.image
    }

    /// Returns the release the node runs.
    #[must_use]
    pub const fn release(&self) -> Digest {
        self.release
    }

    /// Returns the key that verifies this advertisement's signatures.
    pub fn verifying_key(&self) -> Result<VerifyingKey> {
        VerifyingKey::from_bytes(&self.public_key).map_err(Error::PeerSignature)
    }

    /// Returns the recovery progress watermark the node reports.
    #[must_use]
    pub const fn progress(&self) -> u64 {
        self.progress
    }

    /// Returns the generation this advertisement replaced.
    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    /// Returns the logical time the advertisement stops being valid.
    #[must_use]
    pub const fn expires_at_ms(&self) -> i64 {
        self.expires_at_ms
    }

    /// Returns the logical time the advertisement was signed.
    #[must_use]
    pub const fn issued_at_ms(&self) -> i64 {
        self.issued_at_ms
    }

    /// Returns the module digests the running image declares.
    #[must_use]
    pub fn module_digests(&self) -> &[Digest] {
        &self.module_digests
    }

    /// Returns the peer protocol versions the node accepts.
    #[must_use]
    pub fn peer_versions(&self) -> &[u32] {
        &self.peer_versions
    }

    /// Returns the failure domain the node reports for placement.
    #[must_use]
    pub const fn failure_domain(&self) -> &NodeFailureDomain {
        &self.failure_domain
    }

    /// Returns the signed capacity block, when the node publishes one.
    #[must_use]
    pub const fn capacity(&self) -> NodeCapacity {
        self.capacity
    }

    /// Returns the optional signed runtime snapshot used for placement.
    #[must_use]
    pub const fn placement_capacity(&self) -> Option<NodePlacementCapacity> {
        self.placement
    }

    /// Replaces the signed runtime snapshot after the caller has measured the
    /// node-wide ledger and local disk envelope.
    pub fn with_placement_capacity(
        mut self,
        placement: NodePlacementCapacity,
        signing_key: &SigningKey,
    ) -> Result<Self> {
        placement.validated()?;
        self.placement = Some(placement);
        self.placement_version = PLACEMENT_SCHEMA_VERSION;
        self.placement_signature = signing_key
            .sign(&self.placement_signing_bytes()?)
            .to_bytes();
        Ok(self)
    }

    /// Reports whether this advertisement carries an authenticated placement
    /// snapshot. Legacy identity-only records remain readable but are never
    /// eligible for weighted ownership placement.
    #[must_use]
    pub fn has_signed_placement(&self) -> bool {
        self.placement_version == PLACEMENT_SCHEMA_VERSION
            && self.placement.is_some()
            && self.placement_signature.iter().any(|byte| *byte != 0)
    }

    /// Returns the node-log status, when the node has an enrolled log.
    #[must_use]
    pub const fn log(&self) -> Option<&NodeLogStatus> {
        self.log.as_ref()
    }

    pub(super) fn encode(&self) -> Result<Vec<u8>> {
        self.validate_shape()?;
        self.verify_signature()?;
        let encoded = serde_json::to_vec(&RawAdvertisement::from(self))?;
        if encoded.len() as u64 > MAX_NODE_BYTES {
            return Err(Error::Node("advertisement exceeds 64 KiB"));
        }
        Ok(encoded)
    }

    pub(super) fn decode_canonical(bytes: &[u8]) -> Result<Self> {
        if bytes.len() as u64 > MAX_NODE_BYTES {
            return Err(Error::Node("advertisement exceeds 64 KiB"));
        }
        let raw: RawAdvertisement = serde_json::from_slice(bytes)?;
        let advertisement = Self::try_from(raw)?;
        advertisement.validate_shape()?;
        advertisement.verify_signature()?;
        if advertisement.encode()?.as_slice() != bytes {
            return Err(Error::Node("advertisement JSON is not canonical"));
        }
        Ok(advertisement)
    }

    pub(super) fn validate_at(&self, now_ms: i64) -> Result<()> {
        self.validate_shape()?;
        if self.issued_at_ms > now_ms.saturating_add(MAX_CLOCK_SKEW_MS)
            || self.expires_at_ms <= now_ms
        {
            return Err(Error::Node("advertisement is not currently valid"));
        }
        Ok(())
    }

    pub(super) fn validate_shape(&self) -> Result<()> {
        if self.node.as_bytes().iter().all(|byte| *byte == 0)
            || self.session.as_bytes().iter().all(|byte| *byte == 0)
            || self.fleet.as_bytes().iter().all(|byte| *byte == 0)
            || self.certificate.as_bytes().iter().all(|byte| *byte == 0)
            || self.image.as_bytes().iter().all(|byte| *byte == 0)
            || self.release.as_bytes().iter().all(|byte| *byte == 0)
            || self.public_key.iter().all(|byte| *byte == 0)
        {
            return Err(Error::Node("advertisement identity is zero"));
        }
        if !valid_endpoint(&self.endpoint) {
            return Err(Error::Node("advertisement endpoint is invalid"));
        }
        if self.generation == 0
            || self.progress == 0
            || self.issued_at_ms < 0
            || self.expires_at_ms <= self.issued_at_ms
            || self.expires_at_ms.saturating_sub(self.issued_at_ms) > MAX_ADVERTISEMENT_LIFETIME_MS
        {
            return Err(Error::Node("advertisement progress or time is invalid"));
        }
        if let Some(log) = &self.log {
            log.validate(self.node)?;
            if log.phase() != NodeLogPhase::Open || log.recovery().is_some() {
                return Err(Error::Node("live node advertisement has a terminal log"));
            }
        }
        self.failure_domain.validate()?;
        if self.capacity.follower_free_bytes > self.capacity.free_disk_bytes
            || (self.capacity.log_protocol == 0 && self.capacity.follower_free_bytes != 0)
        {
            return Err(Error::Node("advertisement follower capacity is invalid"));
        }
        if let Some(placement) = self.placement {
            placement.validated()?;
        }
        if self.module_digests.is_empty()
            || self.module_digests.len() > MAX_MODULES
            || !self
                .module_digests
                .windows(2)
                .all(|pair| pair[0].as_bytes() < pair[1].as_bytes())
            || self.peer_versions.is_empty()
            || self.peer_versions.len() > MAX_PEER_VERSIONS
            || !strictly_sorted(&self.peer_versions)
            || self.peer_versions.iter().any(|version| *version != 1)
        {
            return Err(Error::Node("advertisement inventory is invalid"));
        }
        Ok(())
    }

    pub(super) fn verify_signature(&self) -> Result<()> {
        self.verifying_key()?
            .verify(
                &self.signing_bytes()?,
                &Signature::from_bytes(&self.signature),
            )
            .map_err(Error::PeerSignature)?;
        match self.placement_version {
            0 => {
                if self.placement_signature.iter().any(|byte| *byte != 0) {
                    return Err(Error::Node("legacy placement record carries a signature"));
                }
            }
            PLACEMENT_SCHEMA_VERSION => {
                if !self.has_signed_placement() {
                    return Err(Error::Node("placement signature is missing"));
                }
                self.verifying_key()?
                    .verify(
                        &self.placement_signing_bytes()?,
                        &Signature::from_bytes(&self.placement_signature),
                    )
                    .map_err(Error::PeerSignature)?;
            }
            _ => {
                // Unknown placement schemas remain readable for identity and
                // liveness. They are never eligible for placement until this
                // binary understands and verifies their signed fields.
            }
        }
        Ok(())
    }

    pub(super) fn signing_bytes(&self) -> Result<Vec<u8>> {
        let unsigned = serde_json::to_vec(&RawNodeSigningPayload {
            identity: RawUnsignedIdentity::from(self),
            capacity: RawCapacity::from(self.capacity),
        })?;
        let mut bytes = Vec::with_capacity(SIGNING_DOMAIN.len() + unsigned.len());
        bytes.extend_from_slice(SIGNING_DOMAIN);
        bytes.extend_from_slice(&unsigned);
        Ok(bytes)
    }

    pub(super) fn placement_signing_bytes(&self) -> Result<Vec<u8>> {
        let mut unsigned = RawAdvertisement::from(self);
        unsigned.placement_signature = None;
        unsigned.log = None;
        // The directory assigns the monotonic heartbeat generation during a
        // CAS refresh; the signed lease timestamps/progress remain immutable
        // evidence while this server-owned counter is intentionally excluded.
        unsigned.lease.generation.clear();
        let unsigned = serde_json::to_vec(&unsigned)?;
        let mut bytes = Vec::with_capacity(PLACEMENT_SIGNING_DOMAIN.len() + unsigned.len());
        bytes.extend_from_slice(PLACEMENT_SIGNING_DOMAIN);
        bytes.extend_from_slice(&unsigned);
        Ok(bytes)
    }
}

/// Exact node advertisement plus the token required for conditional refresh.
#[derive(Clone)]
pub struct VersionedNodeAdvertisement {
    pub(super) advertisement: NodeAdvertisement,
    pub(super) token: ETag,
}

/// Proof that the exact predecessor session was atomically fenced after expiry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FencedNodeSession {
    pub(super) node: NodeId,
    pub(super) session: SessionId,
    pub(super) claimant: SessionId,
    pub(super) claim_generation: u64,
    pub(super) claim_expires_at_ms: i64,
    pub(super) log: Option<NodeLogStatus>,
}

impl FencedNodeSession {
    /// Returns the node whose session was fenced.
    #[must_use]
    pub const fn node(&self) -> NodeId {
        self.node
    }

    /// Returns the predecessor session that was fenced.
    #[must_use]
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the session that fenced it.
    #[must_use]
    pub const fn claimant(&self) -> SessionId {
        self.claimant
    }

    /// Returns the claim generation the fence was issued under.
    #[must_use]
    pub const fn claim_generation(&self) -> u64 {
        self.claim_generation
    }

    /// Returns the logical time the claim expires.
    #[must_use]
    pub const fn claim_expires_at_ms(&self) -> i64 {
        self.claim_expires_at_ms
    }

    /// Returns the node-log status observed while fencing.
    #[must_use]
    pub const fn log(&self) -> Option<&NodeLogStatus> {
        self.log.as_ref()
    }

    /// Converts a fence into takeover authority when no fleet proof needs recovery.
    pub fn direct_takeover(&self) -> Result<NodeTakeoverProof> {
        if self.log.as_ref().is_some_and(NodeLogStatus::active) {
            return Err(Error::PendingPublication);
        }
        Ok(NodeTakeoverProof {
            session: self.session,
            claimant: self.claimant,
        })
    }
}

/// Proof that predecessor node-log recovery cannot add newer durable Cell state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NodeTakeoverProof {
    pub(super) session: SessionId,
    pub(super) claimant: SessionId,
}

impl NodeTakeoverProof {
    /// Returns the predecessor session this proof covers.
    #[must_use]
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the session permitted to take over.
    #[must_use]
    pub const fn claimant(&self) -> SessionId {
        self.claimant
    }

    pub(crate) fn after_recovery(
        fenced: &FencedNodeSession,
        sealed: &SealedNodeLog,
    ) -> Result<Self> {
        let recovering = fenced.log.as_ref().ok_or(Error::Fenced)?;
        let sealed_log = sealed.log();
        if sealed.session() != fenced.session
            || sealed_log.phase() != NodeLogPhase::Sealed
            || sealed_log.epoch() != recovering.epoch()
            || sealed_log.members() != recovering.members()
            || sealed_log.active() != recovering.active()
            || sealed_log.tiered_through() != recovering.tiered_through()
        {
            return Err(Error::Fenced);
        }
        Ok(Self {
            session: fenced.session,
            claimant: fenced.claimant,
        })
    }
}

/// Proof that one failed node log has finished pinning every recovered tail.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SealedNodeLog {
    pub(super) session: SessionId,
    pub(super) log: NodeLogStatus,
}

impl SealedNodeLog {
    /// Returns the session whose log was sealed.
    #[must_use]
    pub const fn session(&self) -> SessionId {
        self.session
    }

    /// Returns the status of the sealed log.
    #[must_use]
    pub const fn log(&self) -> &NodeLogStatus {
        &self.log
    }
}

impl VersionedNodeAdvertisement {
    /// Returns the decoded advertisement.
    #[must_use]
    pub const fn advertisement(&self) -> &NodeAdvertisement {
        &self.advertisement
    }
}

pub(super) fn validate_successor(
    current: &NodeAdvertisement,
    next: &NodeAdvertisement,
) -> Result<()> {
    if !same_boot_identity(current, next)
        || current.log != next.log
        || current.generation.checked_add(1) != Some(next.generation)
        || next.progress < current.progress
        || next.issued_at_ms <= current.issued_at_ms
        || next.expires_at_ms <= current.expires_at_ms
    {
        return Err(Error::Node(
            "advertisement refresh changed boot identity or regressed",
        ));
    }
    Ok(())
}

pub(super) fn same_boot_identity(current: &NodeAdvertisement, next: &NodeAdvertisement) -> bool {
    // Capacity is a fresh signed heartbeat measurement, so its signature may
    // change without allowing the boot identity or signing key to change.
    current.node == next.node
        && current.session == next.session
        && current.endpoint == next.endpoint
        && current.fleet == next.fleet
        && current.certificate == next.certificate
        && current.image == next.image
        && current.release == next.release
        && current.public_key == next.public_key
        && current.module_digests == next.module_digests
        && current.peer_versions == next.peer_versions
        && current.failure_domain == next.failure_domain
}

pub(super) fn valid_endpoint(endpoint: &str) -> bool {
    if endpoint.len() > MAX_ENDPOINT_BYTES
        || !endpoint.is_ascii()
        || endpoint
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte == b' ')
        || endpoint.contains(['@', '?', '#'])
    {
        return false;
    }
    let Some(authority) = endpoint.strip_prefix("https://") else {
        return false;
    };
    let authority = authority.strip_suffix('/').unwrap_or(authority);
    if authority.is_empty() || authority.contains('/') {
        return false;
    }
    let (host, port) = match authority.strip_prefix('[') {
        Some(bracketed) => {
            let Some((host, suffix)) = bracketed.split_once(']') else {
                return false;
            };
            if host.parse::<std::net::Ipv6Addr>().is_err() {
                return false;
            }
            let port = if suffix.is_empty() {
                None
            } else {
                let Some(port) = suffix.strip_prefix(':') else {
                    return false;
                };
                Some(port)
            };
            (None, port)
        }
        None => match authority.rsplit_once(':') {
            Some((host, port)) => (Some(host), Some(port)),
            None => (Some(authority), None),
        },
    };
    if host.is_some_and(|host| !valid_host(host)) {
        return false;
    }
    port.is_none_or(|port| {
        port.parse::<u16>()
            .is_ok_and(|parsed| parsed != 0 && parsed.to_string() == port)
    })
}

pub(super) fn valid_host(host: &str) -> bool {
    if host.parse::<std::net::Ipv4Addr>().is_ok() {
        return true;
    }
    !host.is_empty()
        && host.len() <= 253
        && host.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                && label
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_alphanumeric)
                && label
                    .as_bytes()
                    .last()
                    .is_some_and(u8::is_ascii_alphanumeric)
        })
}

pub(super) fn strictly_sorted<T: Ord>(values: &[T]) -> bool {
    values.windows(2).all(|pair| pair[0] < pair[1])
}

pub(super) fn encode_log(log: &NodeLogStatus) -> RawNodeLog {
    RawNodeLog {
        state: match log.phase() {
            NodeLogPhase::Open => RawNodeLogPhase::Open,
            NodeLogPhase::Recovering => RawNodeLogPhase::Recovering,
            NodeLogPhase::Sealed => RawNodeLogPhase::Sealed,
            NodeLogPhase::Retired => RawNodeLogPhase::Retired,
        },
        epoch: log.epoch().to_string(),
        members: log
            .members()
            .iter()
            .map(|member| encode_hex(member.as_bytes()))
            .collect(),
        active: log.active(),
        tiered_through: log.tiered_through().to_string(),
        recovery: log.recovery().map(|claim| RawNodeRecoveryClaim {
            claimant: encode_hex(claim.claimant().as_bytes()),
            generation: claim.generation().to_string(),
            expires_at_ms: claim.expires_at_ms().to_string(),
        }),
        recovery_manifest: log
            .recovery_manifest()
            .map(|digest| encode_hex(digest.as_bytes())),
    }
}

pub(super) fn decode_log(leader: NodeId, raw: RawNodeLog) -> Result<NodeLogStatus> {
    let recovery = raw
        .recovery
        .map(|claim| {
            NodeRecoveryClaim::from_parts(
                SessionId::from_bytes(decode_hex(&claim.claimant)?),
                canonical_u64(&claim.generation)?,
                canonical_i64(&claim.expires_at_ms)?,
            )
        })
        .transpose()?;
    NodeLogStatus::from_parts(
        leader,
        match raw.state {
            RawNodeLogPhase::Open => NodeLogPhase::Open,
            RawNodeLogPhase::Recovering => NodeLogPhase::Recovering,
            RawNodeLogPhase::Sealed => NodeLogPhase::Sealed,
            RawNodeLogPhase::Retired => NodeLogPhase::Retired,
        },
        canonical_u64(&raw.epoch)?,
        raw.members
            .iter()
            .map(|member| decode_hex(member).map(NodeId::from_bytes))
            .collect::<Result<Vec<_>>>()?,
        raw.active,
        canonical_u64(&raw.tiered_through)?,
        recovery,
        raw.recovery_manifest
            .map(|digest| decode_hex(&digest).map(Digest::from_bytes))
            .transpose()?,
    )
}

pub(super) fn canonical_u64(value: &str) -> Result<u64> {
    let parsed = value
        .parse::<u64>()
        .map_err(|_| Error::Node("invalid unsigned decimal"))?;
    if parsed.to_string() != value {
        return Err(Error::Node("unsigned decimal is not canonical"));
    }
    Ok(parsed)
}

pub(super) fn canonical_i64(value: &str) -> Result<i64> {
    let parsed = value
        .parse::<i64>()
        .map_err(|_| Error::Node("invalid signed decimal"))?;
    if parsed.to_string() != value {
        return Err(Error::Node("signed decimal is not canonical"));
    }
    Ok(parsed)
}

pub(super) fn encode_hex(bytes: &[u8]) -> String {
    pub(super) const TABLE: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(TABLE[(byte >> 4) as usize] as char);
        encoded.push(TABLE[(byte & 0x0f) as usize] as char);
    }
    encoded
}

pub(super) fn decode_hex<const N: usize>(value: &str) -> Result<[u8; N]> {
    if value.len() != N * 2 {
        return Err(Error::Node("hex field length is invalid"));
    }
    let mut decoded = [0; N];
    for (index, pair) in value.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        let high = nibble(pair[0]).ok_or(Error::Node("hex field is not lowercase"))?;
        let low = nibble(pair[1]).ok_or(Error::Node("hex field is not lowercase"))?;
        decoded[index] = (high << 4) | low;
    }
    Ok(decoded)
}

pub(super) fn nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}
