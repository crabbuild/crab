use std::collections::HashSet;

use bytes::Bytes;
use crab_ltx::CellStorageLayout;
use crab_storage::{ETag, StorageError, map_object_store_error};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};

use crate::placement::{
    PlacementObservation, PlacementPlanner, PlacementRuntimeSnapshot, PlacementScore,
};
use crate::{
    Digest, Error, NodeId, NodeLogPhase, NodeLogRotationBarrier, NodeLogStatus, NodeRecoveryClaim,
    Result, SessionId,
};

const MAX_NODE_BYTES: u64 = 64 * 1024;
const MAX_ENDPOINT_BYTES: usize = 512;
const MAX_FAILURE_DOMAIN_BYTES: usize = 253;
const MAX_MODULES: usize = 128;
const MAX_PEER_VERSIONS: usize = 16;
const MAX_ADVERTISEMENT_LIFETIME_MS: i64 = 15_000;
const MAX_CLOCK_SKEW_MS: i64 = 5 * 60_000;
const STALE_ADVERTISEMENT_RETENTION_MS: i64 = MAX_CLOCK_SKEW_MS + MAX_ADVERTISEMENT_LIFETIME_MS;
const MAX_STALE_COLLECTION_ITEMS: usize = 1_024;
const MAX_LIVE_NODE_RECORDS: usize = 10_000;
const SIGNING_DOMAIN: &[u8] = b"crab.node.v1\0";
const PLACEMENT_SIGNING_DOMAIN: &[u8] = b"crab.node-placement.v1\0";
const NODE_LOG_SELECTION_DOMAIN: &[u8] = b"crab.node-log.member.v1\0";
const PLACEMENT_SCHEMA_VERSION: u32 = 1;

/// Current private follower-log wire and persistence protocol.
pub const NODE_LOG_PROTOCOL_VERSION: u32 = 1;

/// Capacity hints published by one node boot session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NodeCapacity {
    pub free_memory_bytes: u64,
    pub free_disk_bytes: u64,
    pub follower_free_bytes: u64,
    pub follower_retained_bytes: u64,
    pub job_credits: u32,
    pub log_protocol: u32,
}

/// Signed runtime capacity measurements used by the placement planner.
///
/// The ordinary capacity hints remain intentionally small and compatible with
/// older node records. This optional block carries the totals and live counts
/// required to compare a node's usable headroom without guessing from host
/// totals on the receiving side.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NodePlacementCapacity {
    pub memory_capacity_bytes: u64,
    pub disk_capacity_bytes: u64,
    pub active_cells: u32,
    pub max_active_cells: u32,
    pub running_jobs: u32,
    pub job_capacity: u32,
}

impl NodePlacementCapacity {
    /// Creates a bounded placement snapshot from measured node totals.
    pub const fn new(
        memory_capacity_bytes: u64,
        disk_capacity_bytes: u64,
        active_cells: u32,
        max_active_cells: u32,
        running_jobs: u32,
        job_capacity: u32,
    ) -> Result<Self> {
        if memory_capacity_bytes == 0
            || disk_capacity_bytes == 0
            || max_active_cells == 0
            || active_cells > max_active_cells
            || job_capacity == 0
            || running_jobs > job_capacity
        {
            return Err(Error::Node("placement capacity is invalid"));
        }
        Ok(Self {
            memory_capacity_bytes,
            disk_capacity_bytes,
            active_cells,
            max_active_cells,
            running_jobs,
            job_capacity,
        })
    }

    fn validate(self) -> Result<()> {
        Self::new(
            self.memory_capacity_bytes,
            self.disk_capacity_bytes,
            self.active_cells,
            self.max_active_cells,
            self.running_jobs,
            self.job_capacity,
        )
        .map(|_| ())
    }

    fn from_capacity(capacity: NodeCapacity) -> Option<Self> {
        Self::new(
            capacity.free_memory_bytes,
            capacity.free_disk_bytes,
            0,
            1,
            0,
            capacity.job_credits,
        )
        .ok()
    }
}

/// Stable topology labels used only to prefer independent follower nodes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NodeFailureDomain {
    zone: Option<String>,
    host: Option<String>,
}

impl NodeFailureDomain {
    /// Validates optional zone and host labels advertised for one boot identity.
    pub fn new(zone: Option<String>, host: Option<String>) -> Result<Self> {
        let domain = Self { zone, host };
        domain.validate()?;
        Ok(domain)
    }

    #[must_use]
    pub fn zone(&self) -> Option<&str> {
        self.zone.as_deref()
    }

    #[must_use]
    pub fn host(&self) -> Option<&str> {
        self.host.as_deref()
    }

    fn validate(&self) -> Result<()> {
        if [self.zone.as_deref(), self.host.as_deref()]
            .into_iter()
            .flatten()
            .any(|value| {
                value.is_empty()
                    || value.len() > MAX_FAILURE_DOMAIN_BYTES
                    || !value.is_ascii()
                    || value.bytes().any(|byte| !byte.is_ascii_graphic())
            })
        {
            return Err(Error::Node("node failure domain is invalid"));
        }
        Ok(())
    }
}

/// Signed, short-lived identity and capacity advertisement for one node session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeAdvertisement {
    node: NodeId,
    session: SessionId,
    endpoint: String,
    fleet: Digest,
    certificate: Digest,
    image: Digest,
    release: Digest,
    public_key: [u8; 32],
    generation: u64,
    progress: u64,
    issued_at_ms: i64,
    expires_at_ms: i64,
    module_digests: Vec<Digest>,
    peer_versions: Vec<u32>,
    failure_domain: NodeFailureDomain,
    capacity: NodeCapacity,
    log: Option<NodeLogStatus>,
    signature: [u8; 64],
    placement_version: u32,
    placement_signature: [u8; 64],
    placement: Option<NodePlacementCapacity>,
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
        let placement = NodePlacementCapacity::from_capacity(capacity);
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
            placement_version: placement.map_or(0, |_| PLACEMENT_SCHEMA_VERSION),
            placement_signature: [0; 64],
            placement,
        };
        advertisement.validate_shape()?;
        advertisement.signature = signing_key.sign(&advertisement.signing_bytes()?).to_bytes();
        if advertisement.placement.is_some() {
            advertisement.placement_signature = signing_key
                .sign(&advertisement.placement_signing_bytes()?)
                .to_bytes();
        }
        Ok(advertisement)
    }

    #[must_use]
    pub const fn node(&self) -> NodeId {
        self.node
    }

    #[must_use]
    pub const fn session(&self) -> SessionId {
        self.session
    }

    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    #[must_use]
    pub const fn fleet(&self) -> Digest {
        self.fleet
    }

    #[must_use]
    pub const fn certificate(&self) -> Digest {
        self.certificate
    }

    #[must_use]
    pub const fn image(&self) -> Digest {
        self.image
    }

    #[must_use]
    pub const fn release(&self) -> Digest {
        self.release
    }

    pub fn verifying_key(&self) -> Result<VerifyingKey> {
        VerifyingKey::from_bytes(&self.public_key).map_err(Error::PeerSignature)
    }

    #[must_use]
    pub const fn progress(&self) -> u64 {
        self.progress
    }

    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    #[must_use]
    pub const fn expires_at_ms(&self) -> i64 {
        self.expires_at_ms
    }

    #[must_use]
    pub const fn issued_at_ms(&self) -> i64 {
        self.issued_at_ms
    }

    #[must_use]
    pub fn module_digests(&self) -> &[Digest] {
        &self.module_digests
    }

    #[must_use]
    pub fn peer_versions(&self) -> &[u32] {
        &self.peer_versions
    }

    #[must_use]
    pub const fn failure_domain(&self) -> &NodeFailureDomain {
        &self.failure_domain
    }

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
        placement.validate()?;
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

    #[must_use]
    pub const fn log(&self) -> Option<&NodeLogStatus> {
        self.log.as_ref()
    }

    fn encode(&self) -> Result<Vec<u8>> {
        self.validate_shape()?;
        self.verify_signature()?;
        let encoded = serde_json::to_vec(&RawAdvertisement::from(self))?;
        if encoded.len() as u64 > MAX_NODE_BYTES {
            return Err(Error::Node("advertisement exceeds 64 KiB"));
        }
        Ok(encoded)
    }

    fn decode_canonical(bytes: &[u8]) -> Result<Self> {
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

    fn validate_at(&self, now_ms: i64) -> Result<()> {
        self.validate_shape()?;
        if self.issued_at_ms > now_ms.saturating_add(MAX_CLOCK_SKEW_MS)
            || self.expires_at_ms <= now_ms
        {
            return Err(Error::Node("advertisement is not currently valid"));
        }
        Ok(())
    }

    fn validate_shape(&self) -> Result<()> {
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
            placement.validate()?;
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

    fn verify_signature(&self) -> Result<()> {
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

    fn signing_bytes(&self) -> Result<Vec<u8>> {
        let unsigned = serde_json::to_vec(&RawUnsignedIdentity::from(self))?;
        let mut bytes = Vec::with_capacity(SIGNING_DOMAIN.len() + unsigned.len());
        bytes.extend_from_slice(SIGNING_DOMAIN);
        bytes.extend_from_slice(&unsigned);
        Ok(bytes)
    }

    fn placement_signing_bytes(&self) -> Result<Vec<u8>> {
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
    advertisement: NodeAdvertisement,
    token: ETag,
}

/// Proof that the exact predecessor session was atomically fenced after expiry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FencedNodeSession {
    node: NodeId,
    session: SessionId,
    claimant: SessionId,
    claim_generation: u64,
    claim_expires_at_ms: i64,
    log: Option<NodeLogStatus>,
}

impl FencedNodeSession {
    #[must_use]
    pub const fn node(&self) -> NodeId {
        self.node
    }

    #[must_use]
    pub const fn session(&self) -> SessionId {
        self.session
    }

    #[must_use]
    pub const fn claimant(&self) -> SessionId {
        self.claimant
    }

    #[must_use]
    pub const fn claim_generation(&self) -> u64 {
        self.claim_generation
    }

    #[must_use]
    pub const fn claim_expires_at_ms(&self) -> i64 {
        self.claim_expires_at_ms
    }

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
    session: SessionId,
    claimant: SessionId,
}

impl NodeTakeoverProof {
    #[must_use]
    pub const fn session(&self) -> SessionId {
        self.session
    }

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
    session: SessionId,
    log: NodeLogStatus,
}

impl SealedNodeLog {
    #[must_use]
    pub const fn session(&self) -> SessionId {
        self.session
    }

    #[must_use]
    pub const fn log(&self) -> &NodeLogStatus {
        &self.log
    }
}

impl VersionedNodeAdvertisement {
    #[must_use]
    pub const fn advertisement(&self) -> &NodeAdvertisement {
        &self.advertisement
    }
}

/// Object-store directory for one fleet and compiled release.
#[derive(Clone)]
pub struct NodeDirectory {
    layout: CellStorageLayout,
    fleet: Digest,
    image: Digest,
    release: Digest,
}

#[derive(Clone, Copy)]
enum AdvertisementScan {
    LiveRelease,
    AdvertisedFleet,
}

impl NodeDirectory {
    #[must_use]
    pub fn new(layout: CellStorageLayout, fleet: Digest, image: Digest, release: Digest) -> Self {
        Self {
            layout,
            fleet,
            image,
            release,
        }
    }

    #[must_use]
    pub const fn fleet(&self) -> Digest {
        self.fleet
    }

    /// Strict-creates one signed boot-session advertisement.
    pub async fn create(
        &self,
        advertisement: NodeAdvertisement,
        now_ms: i64,
    ) -> Result<VersionedNodeAdvertisement> {
        self.validate(&advertisement, now_ms)?;
        let encoded = advertisement.encode()?;
        let path = self.layout.node_path(advertisement.session.as_bytes());
        match self
            .layout
            .store()
            .create_strict_with_etag(&path, Bytes::from(encoded))
            .await
        {
            Ok(token) => Ok(VersionedNodeAdvertisement {
                advertisement,
                token,
            }),
            Err(create_error) => match self.load(advertisement.session, now_ms).await? {
                Some(current) if current.advertisement == advertisement => Ok(current),
                Some(_) | None => Err(create_error.into()),
            },
        }
    }

    /// Loads and verifies one exact, currently valid boot session.
    pub async fn load(
        &self,
        session: SessionId,
        now_ms: i64,
    ) -> Result<Option<VersionedNodeAdvertisement>> {
        let Some((advertisement, token)) = self.load_canonical(session).await? else {
            return Ok(None);
        };
        self.validate(&advertisement, now_ms)?;
        Ok(Some(VersionedNodeAdvertisement {
            advertisement,
            token,
        }))
    }

    /// Inspects one signed advertisement without requiring its lease to remain live.
    ///
    /// This is an operational read only: callers must use [`Self::load`] or
    /// [`Self::is_live`] for admission and takeover decisions.
    pub async fn inspect_advertisement(
        &self,
        session: SessionId,
        now_ms: i64,
    ) -> Result<Option<NodeAdvertisement>> {
        let Some((advertisement, _)) = self.load_canonical(session).await? else {
            return Ok(None);
        };
        advertisement.validate_shape()?;
        advertisement.verify_signature()?;
        self.validate_scope(&advertisement)?;
        if advertisement.issued_at_ms > now_ms.saturating_add(MAX_CLOCK_SKEW_MS) {
            return Err(Error::Node("advertisement issue time is in the future"));
        }
        Ok(Some(advertisement))
    }

    /// Reports whether an exact canonical session is currently live.
    ///
    /// Missing and expired sessions return `false`. Malformed, misplaced, or
    /// foreign records fail closed instead of being treated as takeover evidence.
    pub async fn is_live(&self, session: SessionId, now_ms: i64) -> Result<bool> {
        let Some((advertisement, _)) = self.load_canonical(session).await? else {
            return Ok(false);
        };
        self.validate_scope(&advertisement)?;
        if advertisement.issued_at_ms > now_ms.saturating_add(MAX_CLOCK_SKEW_MS) {
            return Err(Error::Node("advertisement is not currently valid"));
        }
        Ok(advertisement.expires_at_ms > now_ms)
    }

    /// Fences one expired boot session with an ETag CAS before any Cell takeover.
    ///
    /// Missing, live, malformed, or foreign records fail closed. The tombstone
    /// remains durable so a paused owner cannot revive its old advertisement.
    pub async fn claim_expired(
        &self,
        session: SessionId,
        claimant: SessionId,
        now_ms: i64,
    ) -> Result<FencedNodeSession> {
        if now_ms < 0 || claimant.as_bytes().iter().all(|byte| *byte == 0) || claimant == session {
            return Err(Error::Node("node recovery time is invalid"));
        }
        self.load(claimant, now_ms)
            .await?
            .ok_or(Error::Node("node recovery claimant is not live"))?;
        let path = self.layout.node_path(session.as_bytes());
        let Some((record, token)) = self.load_record_at(&path).await? else {
            return Err(Error::Node("expired node session record is missing"));
        };
        let tombstone = match record {
            NodeRecord::Tombstone(tombstone) if tombstone.session == session => {
                if tombstone.claimant == Some(claimant)
                    && tombstone
                        .claim_expires_at_ms
                        .is_some_and(|expires_at_ms| expires_at_ms > now_ms)
                {
                    return tombstone.fenced();
                }
                (*tombstone).claim(claimant, now_ms)?
            }
            NodeRecord::Advertisement(advertisement) => {
                self.validate_scope(&advertisement)?;
                advertisement.validate_shape()?;
                advertisement.verify_signature()?;
                if advertisement.session != session || advertisement.expires_at_ms > now_ms {
                    return Err(Error::Node("node session is not expired"));
                }
                NodeTombstone::new(
                    session,
                    advertisement.node,
                    advertisement.expires_at_ms,
                    now_ms,
                    None,
                    advertisement.log.clone(),
                )?
                .claim(claimant, now_ms)?
            }
            NodeRecord::Tombstone(_) => {
                return Err(Error::Node("node tombstone session differs"));
            }
        };
        let proof = tombstone.fenced()?;
        match self
            .layout
            .store()
            .update(&path, Bytes::from(tombstone.encode()?), token)
            .await
        {
            Ok(_) => Ok(proof),
            Err(update_error) => match self.load_record_at(&path).await? {
                Some((NodeRecord::Tombstone(current), _))
                    if current.session == session
                        && current.claimant == Some(claimant)
                        && current
                            .claim_expires_at_ms
                            .is_some_and(|expires_at_ms| expires_at_ms > now_ms) =>
                {
                    current.fenced()
                }
                Some(_) | None => Err(update_error.into()),
            },
        }
    }

    /// Loads takeover authority already persisted by a completed node recovery.
    pub async fn takeover_proof(
        &self,
        session: SessionId,
        claimant: SessionId,
        now_ms: i64,
    ) -> Result<Option<NodeTakeoverProof>> {
        if now_ms < 0 || claimant == session {
            return Err(Error::Node("node takeover time is invalid"));
        }
        self.load(claimant, now_ms)
            .await?
            .ok_or(Error::Node("node takeover claimant is not live"))?;
        let path = self.layout.node_path(session.as_bytes());
        let Some((record, _)) = self.load_record_at(&path).await? else {
            return Err(Error::Node("node takeover record is missing"));
        };
        let NodeRecord::Tombstone(tombstone) = record else {
            return Ok(None);
        };
        if tombstone.session != session {
            return Err(Error::Node("node tombstone session differs"));
        }
        let claimed_by_caller = tombstone.claimant == Some(claimant)
            && tombstone
                .claim_expires_at_ms
                .is_some_and(|expires_at_ms| expires_at_ms > now_ms);
        let ready = match tombstone.log.as_ref() {
            Some(log) if matches!(log.phase(), NodeLogPhase::Sealed | NodeLogPhase::Retired) => {
                true
            }
            Some(log) => !log.active() && claimed_by_caller,
            None => claimed_by_caller,
        };
        Ok(ready.then_some(NodeTakeoverProof { session, claimant }))
    }

    /// Resolves the deterministic live original-follower successor for a
    /// sealed failed session. The returned advertisement is advisory; the
    /// destination still rechecks takeover proof and Cell control CAS.
    pub async fn preferred_recovery_node(
        &self,
        session: SessionId,
        now_ms: i64,
    ) -> Result<Option<NodeAdvertisement>> {
        if now_ms < 0 {
            return Err(Error::Node("node recovery time is invalid"));
        }
        let path = self.layout.node_path(session.as_bytes());
        let Some((record, _)) = self.load_record_at(&path).await? else {
            return Err(Error::Node("node recovery record is missing"));
        };
        let log = match record {
            NodeRecord::Advertisement(advertisement) => {
                self.validate_scope(&advertisement)?;
                advertisement.validate_shape()?;
                advertisement.verify_signature()?;
                if advertisement.expires_at_ms > now_ms {
                    return Ok(None);
                }
                advertisement.log
            }
            NodeRecord::Tombstone(tombstone) => tombstone.log,
        };
        let Some(log) = log else {
            return Ok(None);
        };
        if !log.active() || !matches!(log.phase(), NodeLogPhase::Sealed | NodeLogPhase::Retired) {
            return Ok(None);
        }
        let live = self.live(now_ms, MAX_LIVE_NODE_RECORDS).await?;
        Ok(log
            .members()
            .iter()
            .filter_map(|member| {
                live.iter().find(|advertisement| {
                    advertisement.node() == *member && recovery_executor_eligible(advertisement)
                })
            })
            .min_by(|left, right| left.node().as_bytes().cmp(right.node().as_bytes()))
            .cloned())
    }

    /// Lists expired active logs whose claim is available to this live session.
    pub async fn recovery_candidates(
        &self,
        claimant: SessionId,
        now_ms: i64,
        limit: usize,
    ) -> Result<Vec<SessionId>> {
        self.recovery_candidates_filtered(claimant, None, false, now_ms, limit)
            .await
    }

    /// Lists expired logs for which this claimant is the deterministic live
    /// original-follower successor. Stable NodeIds choose the winner; the
    /// current boot SessionId remains the claim authority.
    pub async fn recovery_candidates_for_node(
        &self,
        claimant: SessionId,
        claimant_node: NodeId,
        now_ms: i64,
        limit: usize,
    ) -> Result<Vec<SessionId>> {
        self.recovery_candidates_filtered(claimant, Some(claimant_node), false, now_ms, limit)
            .await
    }

    /// Lists expired logs only when no enrolled original follower is live.
    /// This is the bounded any-node fallback after follower-affine attempts.
    pub async fn recovery_candidates_without_live_followers(
        &self,
        claimant: SessionId,
        now_ms: i64,
        limit: usize,
    ) -> Result<Vec<SessionId>> {
        self.recovery_candidates_filtered(claimant, None, true, now_ms, limit)
            .await
    }

    async fn recovery_candidates_filtered(
        &self,
        claimant: SessionId,
        claimant_node: Option<NodeId>,
        require_no_live_followers: bool,
        now_ms: i64,
        limit: usize,
    ) -> Result<Vec<SessionId>> {
        if now_ms < 0 || limit == 0 || limit > MAX_STALE_COLLECTION_ITEMS {
            return Err(Error::Node("node recovery candidate bound is invalid"));
        }
        let claimant_advertisement = self
            .load(claimant, now_ms)
            .await?
            .ok_or(Error::Node("node recovery claimant is not live"))?;
        if !recovery_executor_eligible(claimant_advertisement.advertisement()) {
            return Ok(Vec::new());
        }
        if claimant_node.is_some_and(|node| claimant_advertisement.advertisement.node() != node) {
            return Err(Error::Node("node recovery claimant identity differs"));
        }
        let live_nodes = if claimant_node.is_some() || require_no_live_followers {
            self.live(now_ms, MAX_LIVE_NODE_RECORDS)
                .await?
                .into_iter()
                .filter(recovery_executor_eligible)
                .map(|advertisement| advertisement.node())
                .collect::<HashSet<_>>()
        } else {
            HashSet::new()
        };
        let prefix = self.layout.node_directory_path();
        let mut stream = self.layout.store().inner().list(Some(&prefix));
        let mut candidates = Vec::new();
        while let Some(item) = stream.next().await {
            let meta = item.map_err(|error| map_object_store_error(error, prefix.as_ref()))?;
            let Some((record, _)) = self.load_record_at(&meta.location).await? else {
                continue;
            };
            let session = record.session();
            validate_record_path(&self.layout, session, &meta.location)?;
            let (eligible, members) = match record {
                NodeRecord::Advertisement(advertisement) => {
                    self.validate_scope(&advertisement)?;
                    advertisement.validate_shape()?;
                    advertisement.verify_signature()?;
                    if advertisement.issued_at_ms > now_ms.saturating_add(MAX_CLOCK_SKEW_MS) {
                        return Err(Error::Node("advertised node issue time differs"));
                    }
                    let log = advertisement.log.as_ref();
                    (
                        advertisement.expires_at_ms <= now_ms
                            && log.is_some_and(NodeLogStatus::active)
                            && log.is_some_and(|log| {
                                matches!(log.phase(), NodeLogPhase::Open | NodeLogPhase::Recovering)
                            }),
                        log.map_or_else(Vec::new, |log| log.members().to_vec()),
                    )
                }
                NodeRecord::Tombstone(tombstone) => {
                    let claim_available = tombstone.claimant == Some(claimant)
                        || tombstone
                            .claim_expires_at_ms
                            .is_none_or(|expires_at_ms| expires_at_ms <= now_ms);
                    let log = tombstone.log.as_ref();
                    (
                        claim_available
                            && log.is_some_and(|log| {
                                log.active()
                                    && matches!(
                                        log.phase(),
                                        NodeLogPhase::Open | NodeLogPhase::Recovering
                                    )
                            }),
                        log.map_or_else(Vec::new, |log| log.members().to_vec()),
                    )
                }
            };
            if !eligible || session == claimant {
                continue;
            }
            if let Some(claimant_node) = claimant_node {
                let preferred = members
                    .iter()
                    .filter(|member| live_nodes.contains(member))
                    .min_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
                if preferred != Some(&claimant_node) {
                    continue;
                }
            } else if require_no_live_followers
                && members.iter().any(|member| live_nodes.contains(member))
            {
                continue;
            }
            candidates.push(session);
            if candidates.len() == limit {
                break;
            }
        }
        candidates.sort_unstable_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
        Ok(candidates)
    }

    /// Extends an exact recovery claim while its claimant remains live.
    pub async fn refresh_recovery_claim(
        &self,
        fenced: &FencedNodeSession,
        now_ms: i64,
    ) -> Result<FencedNodeSession> {
        self.load(fenced.claimant, now_ms)
            .await?
            .ok_or(Error::Fenced)?;
        let path = self.layout.node_path(fenced.session.as_bytes());
        let Some((NodeRecord::Tombstone(current), token)) = self.load_record_at(&path).await?
        else {
            return Err(Error::Fenced);
        };
        let renewed = (*current).renew(fenced, now_ms)?;
        let proof = renewed.fenced()?;
        self.layout
            .store()
            .update(&path, Bytes::from(renewed.encode()?), token)
            .await?;
        Ok(proof)
    }

    /// Seals an exact recovery claim after every affected Cell pins its overlay.
    pub(crate) async fn seal_recovery(
        &self,
        fenced: &FencedNodeSession,
        recovery_manifest: Option<Digest>,
        now_ms: i64,
    ) -> Result<SealedNodeLog> {
        let path = self.layout.node_path(fenced.session.as_bytes());
        let Some((NodeRecord::Tombstone(current), token)) = self.load_record_at(&path).await?
        else {
            return Err(Error::Fenced);
        };
        if let Some(log) = &current.log
            && log.phase() == NodeLogPhase::Sealed
            && log.recovery_manifest() == recovery_manifest
        {
            return Ok(SealedNodeLog {
                session: current.session,
                log: log.clone(),
            });
        }
        let sealed = (*current).seal(fenced, recovery_manifest, now_ms)?;
        let log = sealed
            .log
            .clone()
            .ok_or(Error::Node("sealed node session lost its log"))?;
        match self
            .layout
            .store()
            .update(&path, Bytes::from(sealed.encode()?), token)
            .await
        {
            Ok(_) => Ok(SealedNodeLog {
                session: sealed.session,
                log,
            }),
            Err(update_error) => match self.load_record_at(&path).await? {
                Some((NodeRecord::Tombstone(current), _))
                    if current.session == fenced.session
                        && current.log.as_ref().is_some_and(|current| {
                            current.phase() == NodeLogPhase::Sealed
                                && current.recovery_manifest() == recovery_manifest
                        }) =>
                {
                    Ok(SealedNodeLog {
                        session: current.session,
                        log: current
                            .log
                            .ok_or(Error::Node("sealed node session lost its log"))?,
                    })
                }
                Some(_) | None => Err(update_error.into()),
            },
        }
    }

    /// Verifies live enrollment and that requested truncation is object-covered.
    pub async fn authorize_log_append(
        &self,
        leader: SessionId,
        member: NodeId,
        log_epoch: u64,
        covered_through: u64,
        now_ms: i64,
    ) -> Result<NodeLogStatus> {
        let current = self
            .load(leader, now_ms)
            .await?
            .ok_or(Error::PeerAuthorization("node-log leader is not live"))?;
        let log = current
            .advertisement
            .log
            .as_ref()
            .ok_or(Error::PeerAuthorization(
                "node-log leader has no enrolled log",
            ))?;
        log.permits_append(current.advertisement.node, member, log_epoch)?;
        if covered_through > log.tiered_through() {
            return Err(Error::PeerAuthorization(
                "node-log append watermark exceeds authority",
            ));
        }
        Ok(log.clone())
    }

    /// Verifies the leader may retire this member's fully object-covered epoch.
    pub async fn authorize_log_retire(
        &self,
        leader: SessionId,
        member: NodeId,
        log_epoch: u64,
        covered_through: u64,
        now_ms: i64,
    ) -> Result<NodeLogStatus> {
        let log = self
            .authorize_log_append(leader, member, log_epoch, covered_through, now_ms)
            .await?;
        if log.tiered_through() != covered_through {
            return Err(Error::PeerAuthorization(
                "node-log retire watermark differs from authority",
            ));
        }
        Ok(log)
    }

    /// Reports whether the authoritative session record still names one log epoch.
    ///
    /// A missing record is corruption rather than collection authority and fails
    /// closed. Callers may delete an exact grace-aged retired follower lane only
    /// when this returns `false`.
    pub async fn log_epoch_referenced(&self, session: SessionId, epoch: u64) -> Result<bool> {
        if epoch == 0 {
            return Err(Error::Node("node-log epoch is zero"));
        }
        let path = self.layout.node_path(session.as_bytes());
        let Some((record, _)) = self.load_record_at(&path).await? else {
            return Err(Error::Node("node session record is missing"));
        };
        if record.session() != session {
            return Err(Error::Node("node advertisement path and session differ"));
        }
        if let NodeRecord::Advertisement(advertisement) = &record {
            self.validate_scope(advertisement)?;
            advertisement.validate_shape()?;
            advertisement.verify_signature()?;
        }
        Ok(record.log().is_some_and(|log| log.epoch() == epoch))
    }

    /// Verifies a live claimant may seal or read this follower's failed-owner lane.
    pub async fn authorize_log_recovery(
        &self,
        leader: SessionId,
        claimant: SessionId,
        member: NodeId,
        log_epoch: u64,
        now_ms: i64,
    ) -> Result<NodeLogStatus> {
        self.load(claimant, now_ms)
            .await?
            .ok_or(Error::PeerAuthorization("node-log recoverer is not live"))?;
        let path = self.layout.node_path(leader.as_bytes());
        let Some((NodeRecord::Tombstone(tombstone), _)) = self.load_record_at(&path).await? else {
            return Err(Error::PeerAuthorization("node-log leader is not fenced"));
        };
        let log = tombstone.log.as_ref().ok_or(Error::PeerAuthorization(
            "node-log leader has no recovery log",
        ))?;
        log.permits_recovery_read(tombstone.node, claimant, member, log_epoch, now_ms)?;
        Ok(log.clone())
    }

    /// Streams and verifies every currently live boot-session advertisement.
    ///
    /// Expired records do not count against `limit`; malformed, misplaced, or
    /// foreign live records fail closed so maintenance cannot mistake an active
    /// incompatible fleet for an offline deployment.
    pub async fn live(&self, now_ms: i64, limit: usize) -> Result<Vec<NodeAdvertisement>> {
        let advertisements = self
            .scan_advertisements(now_ms, limit, AdvertisementScan::LiveRelease)
            .await?;
        if advertisements.iter().enumerate().any(|(index, left)| {
            advertisements[index + 1..]
                .iter()
                .any(|right| left.node == right.node)
        }) {
            return Err(Error::Node("multiple live sessions advertise one node"));
        }
        Ok(advertisements)
    }

    /// Ranks live, signature-verified nodes using measured runtime snapshots.
    ///
    /// The directory contributes only authenticated advertisement capacity and
    /// lease age. Runtime counters are required explicitly; an absent snapshot
    /// is omitted instead of being interpreted as idle capacity.
    pub async fn choose_placement(
        &self,
        planner: &PlacementPlanner,
        cell: crate::CellId,
        now_ms: i64,
        snapshots: &[PlacementRuntimeSnapshot],
        limit: usize,
    ) -> Result<Option<PlacementScore>> {
        let live = self.live(now_ms, limit).await?;
        let observations = live
            .iter()
            .filter_map(|advertisement| {
                snapshots
                    .iter()
                    .find(|snapshot| snapshot.node == advertisement.node())
                    .and_then(|snapshot| {
                        PlacementObservation::from_advertisement(
                            advertisement,
                            now_ms,
                            snapshot.memory_capacity_bytes,
                            snapshot.disk_capacity_bytes,
                            snapshot.active_cells,
                            snapshot.max_active_cells,
                            snapshot.running_jobs,
                            snapshot.pressure,
                            snapshot.draining,
                            snapshot.locality_bonus,
                            snapshot.current_owner,
                        )
                        .ok()
                    })
            })
            .collect::<Vec<_>>();
        planner.choose(cell, now_ms, &observations)
    }

    /// Chooses a destination using only authenticated, measured placement
    /// blocks advertised by the current live fleet.
    ///
    /// Nodes that have not rolled out the placement block are omitted. They
    /// remain usable for ordinary authority routing but cannot become an
    /// advisory destination through this method.
    pub async fn choose_advertised_placement(
        &self,
        planner: &PlacementPlanner,
        cell: crate::CellId,
        now_ms: i64,
        current_session: SessionId,
        limit: usize,
    ) -> Result<Option<PlacementScore>> {
        let live = self.live(now_ms, limit).await?;
        let observations = live
            .iter()
            .filter_map(|advertisement| {
                PlacementObservation::from_signed_advertisement(
                    advertisement,
                    now_ms,
                    advertisement.session() == current_session,
                )
                .ok()
            })
            .collect::<Vec<_>>();
        planner.choose(cell, now_ms, &observations)
    }

    /// Resolves a stable physical node to its one current live boot session.
    pub async fn resolve_node(
        &self,
        node: NodeId,
        now_ms: i64,
    ) -> Result<Option<NodeAdvertisement>> {
        if node.as_bytes().iter().all(|byte| *byte == 0) {
            return Err(Error::Node("node identity is zero"));
        }
        Ok(self
            .live(now_ms, MAX_STALE_COLLECTION_ITEMS)
            .await?
            .into_iter()
            .find(|advertisement| advertisement.node == node))
    }

    /// Selects the exact deterministic follower ensemble from current live capacity.
    ///
    /// An empty result means this fleet cannot currently satisfy the desired
    /// one-follower/two-follower durability shape and must use object proof.
    pub async fn select_log_members(
        &self,
        leader: SessionId,
        required_follower_bytes: u64,
        now_ms: i64,
        limit: usize,
    ) -> Result<Vec<NodeId>> {
        if required_follower_bytes == 0 {
            return Err(Error::Node("node-log follower byte requirement is zero"));
        }
        let live = self.live(now_ms, limit).await?;
        let leader = live
            .iter()
            .find(|candidate| candidate.session == leader)
            .ok_or(Error::Node("node-log leader is not live"))?;
        let desired = live
            .len()
            .saturating_sub(1)
            .min(crate::node_log_state::MAX_NODE_LOG_MEMBERS);
        if desired == 0 {
            return Ok(Vec::new());
        }
        let mut eligible = live
            .iter()
            .filter(|candidate| {
                candidate.node != leader.node
                    && candidate.capacity.log_protocol == NODE_LOG_PROTOCOL_VERSION
                    && candidate.capacity.follower_free_bytes >= required_follower_bytes
                    && candidate.capacity.free_memory_bytes != 0
                    && candidate.capacity.free_disk_bytes != 0
                    && candidate.capacity.job_credits != 0
            })
            .map(|candidate| {
                let mut hasher = blake3::Hasher::new();
                hasher.update(NODE_LOG_SELECTION_DOMAIN);
                hasher.update(leader.session.as_bytes());
                hasher.update(candidate.node.as_bytes());
                (candidate, *hasher.finalize().as_bytes())
            })
            .collect::<Vec<_>>();
        if eligible.len() < desired {
            return Ok(Vec::new());
        }
        let mut selected_advertisements = Vec::with_capacity(desired);
        while selected_advertisements.len() < desired {
            let context = std::iter::once(leader)
                .chain(selected_advertisements.iter().copied())
                .collect::<Vec<_>>();
            let selected_index = eligible
                .iter()
                .enumerate()
                .max_by(|(_, left), (_, right)| compare_member_candidate(left, right, &context))
                .map(|(index, _)| index)
                .ok_or(Error::Node("node-log follower ensemble is unavailable"))?;
            selected_advertisements.push(eligible.swap_remove(selected_index).0);
        }
        let mut selected = selected_advertisements
            .into_iter()
            .map(|advertisement| advertisement.node)
            .collect::<Vec<_>>();
        selected.sort_unstable_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
        Ok(selected)
    }

    /// Lists every unfenced advertised session in this fleet, including expired records.
    ///
    /// Graceful withdrawal happens only after writers close. Stale collection first fences
    /// the exact record by ETag. Maintenance must not interpret heartbeat expiry alone as drain.
    pub async fn advertised_sessions(&self, now_ms: i64, limit: usize) -> Result<Vec<SessionId>> {
        Ok(self
            .scan_advertisements(now_ms, limit, AdvertisementScan::AdvertisedFleet)
            .await?
            .into_iter()
            .map(|advertisement| advertisement.session)
            .collect())
    }

    async fn scan_advertisements(
        &self,
        now_ms: i64,
        limit: usize,
        scan: AdvertisementScan,
    ) -> Result<Vec<NodeAdvertisement>> {
        if limit == 0 {
            return Err(Error::Node(match scan {
                AdvertisementScan::LiveRelease => "live node limit must be nonzero",
                AdvertisementScan::AdvertisedFleet => {
                    "advertised node session limit must be nonzero"
                }
            }));
        }
        let prefix = self.layout.node_directory_path();
        let mut stream = self.layout.store().inner().list(Some(&prefix));
        let mut advertisements = Vec::new();
        while let Some(item) = stream.next().await {
            let meta = item.map_err(|error| map_object_store_error(error, prefix.as_ref()))?;
            let (body, _) = match self
                .layout
                .store()
                .get_with_etag_bounded(&meta.location, MAX_NODE_BYTES)
                .await
            {
                Ok(value) => value,
                Err(StorageError::NotFound { .. }) => continue,
                Err(error) => return Err(error.into()),
            };
            let NodeRecord::Advertisement(advertisement) = NodeRecord::decode_canonical(&body)?
            else {
                continue;
            };
            validate_record_path(&self.layout, advertisement.session, &meta.location)?;
            match scan {
                AdvertisementScan::LiveRelease => {
                    if advertisement.expires_at_ms <= now_ms {
                        continue;
                    }
                    self.validate(&advertisement, now_ms)?;
                }
                AdvertisementScan::AdvertisedFleet => {
                    advertisement.validate_shape()?;
                    advertisement.verify_signature()?;
                    if advertisement.fleet != self.fleet
                        || advertisement.issued_at_ms > now_ms.saturating_add(MAX_CLOCK_SKEW_MS)
                    {
                        return Err(Error::Node("advertised node fleet or issue time differs"));
                    }
                }
            }
            if advertisements.len() == limit {
                return Err(Error::Node(match scan {
                    AdvertisementScan::LiveRelease => "live node directory exceeds its limit",
                    AdvertisementScan::AdvertisedFleet => {
                        "advertised node session directory exceeds its limit"
                    }
                }));
            }
            advertisements.push(*advertisement);
        }
        advertisements
            .sort_unstable_by(|left, right| left.session.as_bytes().cmp(right.session.as_bytes()));
        Ok(advertisements)
    }

    /// Fences a bounded number of advertisements past the clock-skew horizon.
    ///
    /// Tombstones remain until node-log recovery and every owned Cell complete;
    /// generic stale collection cannot prove that retention condition.
    pub async fn collect_stale(&self, now_ms: i64, limit: usize) -> Result<usize> {
        if now_ms < 0 || !(1..=MAX_STALE_COLLECTION_ITEMS).contains(&limit) {
            return Err(Error::Node(
                "stale node collection limit or time is invalid",
            ));
        }
        let cutoff_ms = now_ms.saturating_sub(STALE_ADVERTISEMENT_RETENTION_MS);
        let prefix = self.layout.node_directory_path();
        let mut stream = self.layout.store().inner().list(Some(&prefix));
        let mut removed = 0;
        while removed < limit
            && let Some(item) = stream.next().await
        {
            let meta = item.map_err(|error| map_object_store_error(error, prefix.as_ref()))?;
            let Some((record, token)) = self.load_record_at(&meta.location).await? else {
                continue;
            };
            let session = record.session();
            validate_record_path(&self.layout, session, &meta.location)?;
            match record {
                NodeRecord::Tombstone(_) => {}
                NodeRecord::Advertisement(advertisement)
                    if advertisement.expires_at_ms <= cutoff_ms =>
                {
                    let tombstone = NodeTombstone::new(
                        advertisement.session,
                        advertisement.node,
                        advertisement.expires_at_ms,
                        now_ms,
                        None,
                        advertisement.log.clone(),
                    )?;
                    let encoded = tombstone.encode()?;
                    match self
                        .layout
                        .store()
                        .update(&meta.location, Bytes::from(encoded), token)
                        .await
                    {
                        Ok(_) => {
                            removed += 1;
                        }
                        Err(update_error) => match self.load_record_at(&meta.location).await? {
                            None => return Err(Error::Node("stale node record disappeared")),
                            Some((NodeRecord::Tombstone(_), _)) => removed += 1,
                            Some((NodeRecord::Advertisement(_), _)) => {
                                if !matches!(update_error, StorageError::StateConflict { .. }) {
                                    return Err(update_error.into());
                                }
                            }
                        },
                    }
                }
                NodeRecord::Advertisement(_) => {}
            }
        }
        Ok(removed)
    }

    /// Conditionally withdraws the exact advertisement owned by a shutting-down node.
    pub async fn withdraw(&self, observed: &VersionedNodeAdvertisement, now_ms: i64) -> Result<()> {
        if now_ms < 0 {
            return Err(Error::Node("node withdrawal time is invalid"));
        }
        self.validate_scope(&observed.advertisement)?;
        if observed.advertisement.log.is_some() {
            return Err(Error::Node(
                "node log must be sealed before session withdrawal",
            ));
        }
        let path = self
            .layout
            .node_path(observed.advertisement.session.as_bytes());
        let tombstone = NodeTombstone::new(
            observed.advertisement.session,
            observed.advertisement.node,
            observed.advertisement.expires_at_ms,
            now_ms,
            None,
            None,
        )?;
        match self
            .layout
            .store()
            .update(
                &path,
                Bytes::from(tombstone.encode()?),
                observed.token.clone(),
            )
            .await
        {
            Ok(_) => Ok(()),
            Err(update_error) => match self.load_record_at(&path).await? {
                None => Ok(()),
                Some((NodeRecord::Tombstone(current), _)) if current.claimant.is_none() => Ok(()),
                Some((NodeRecord::Tombstone(_), _)) => Err(Error::Fenced),
                Some((NodeRecord::Advertisement(current), _))
                    if *current == observed.advertisement =>
                {
                    Err(update_error.into())
                }
                Some((NodeRecord::Advertisement(_), _)) => {
                    Err(Error::Node("advertisement changed during node withdrawal"))
                }
            },
        }
    }

    /// Authenticates one request against its live advertisement and mTLS leaf digest.
    pub async fn verify_peer_request(
        &self,
        input: &[u8],
        certificate: Digest,
        certificate_public_key: [u8; 32],
        now_ms: i64,
    ) -> Result<crate::VerifiedPeerRequest> {
        let session = crate::claimed_peer_session(input)?;
        let enrolled = self
            .load(session, now_ms)
            .await?
            .ok_or(Error::PeerAuthorization("peer session is not enrolled"))?;
        if enrolled.advertisement.certificate != certificate {
            return Err(Error::PeerAuthorization(
                "mTLS certificate does not match peer session",
            ));
        }
        if enrolled.advertisement.public_key != certificate_public_key {
            return Err(Error::PeerAuthorization(
                "mTLS certificate key does not match peer session",
            ));
        }
        crate::PeerVerifier::new(
            session,
            self.release,
            enrolled.advertisement.verifying_key()?,
        )
        .verify(input, now_ms)
    }

    /// Conditionally publishes the next heartbeat for the same boot session.
    pub async fn refresh(
        &self,
        observed: &VersionedNodeAdvertisement,
        next: NodeAdvertisement,
        now_ms: i64,
    ) -> Result<VersionedNodeAdvertisement> {
        let mut base = observed.clone();
        for _ in 0..4 {
            if !same_boot_identity(&base.advertisement, &next) {
                return Err(Error::Node("advertisement refresh changed boot identity"));
            }
            if next.issued_at_ms <= base.advertisement.issued_at_ms {
                if next.progress <= base.advertisement.progress
                    && next.expires_at_ms <= base.advertisement.expires_at_ms
                {
                    return Ok(base);
                }
                return Err(Error::Node("advertisement refresh lease regressed"));
            }
            let mut candidate = next.clone();
            candidate.generation = base
                .advertisement
                .generation
                .checked_add(1)
                .ok_or(Error::Node("node session generation overflow"))?;
            candidate.log.clone_from(&base.advertisement.log);
            self.validate(&candidate, now_ms)?;
            validate_successor(&base.advertisement, &candidate)?;
            let path = self.layout.node_path(candidate.session.as_bytes());
            match self
                .layout
                .store()
                .update(&path, Bytes::from(candidate.encode()?), base.token.clone())
                .await
            {
                Ok(token) => {
                    return Ok(VersionedNodeAdvertisement {
                        advertisement: candidate,
                        token,
                    });
                }
                Err(update_error) => match self.load(candidate.session, now_ms).await? {
                    Some(current) if current.advertisement == candidate => return Ok(current),
                    Some(current)
                        if same_boot_identity(&base.advertisement, &current.advertisement)
                            && current.advertisement.issued_at_ms
                                >= base.advertisement.issued_at_ms
                            && current.advertisement.progress >= base.advertisement.progress =>
                    {
                        base = current;
                    }
                    Some(_) | None => return Err(update_error.into()),
                },
            }
        }
        Err(Error::Node("node session changed during heartbeat refresh"))
    }

    /// Selects and CAS-enrolls the complete follower set before any frame is sent.
    pub async fn recruit_log(
        &self,
        observed: &VersionedNodeAdvertisement,
        log_epoch: u64,
        required_follower_bytes: u64,
        live_node_limit: usize,
        now_ms: i64,
    ) -> Result<VersionedNodeAdvertisement> {
        self.try_recruit_log(
            observed,
            log_epoch,
            required_follower_bytes,
            live_node_limit,
            now_ms,
        )
        .await?
        .ok_or(Error::Node("node-log follower ensemble is unavailable"))
    }

    /// CAS-enrolls followers when a complete ensemble is currently available.
    pub async fn try_recruit_log(
        &self,
        observed: &VersionedNodeAdvertisement,
        log_epoch: u64,
        required_follower_bytes: u64,
        live_node_limit: usize,
        now_ms: i64,
    ) -> Result<Option<VersionedNodeAdvertisement>> {
        self.validate(&observed.advertisement, now_ms)?;
        if observed.advertisement.log.is_some() {
            return Err(Error::Node("node session already has an enrolled log"));
        }
        let members = self
            .select_log_members(
                observed.advertisement.session,
                required_follower_bytes,
                now_ms,
                live_node_limit,
            )
            .await?;
        if members.is_empty() {
            return Ok(None);
        }
        let mut next = observed.advertisement.clone();
        next.generation = next
            .generation
            .checked_add(1)
            .ok_or(Error::Node("node session generation overflow"))?;
        next.log = Some(NodeLogStatus::open(next.node, log_epoch, members)?);
        self.update_advertisement(observed, next, now_ms)
            .await
            .map(Some)
    }

    /// CAS-activates the exact enrolled epoch after every member fsyncs its first batch.
    pub async fn activate_log(
        &self,
        observed: &VersionedNodeAdvertisement,
        now_ms: i64,
    ) -> Result<VersionedNodeAdvertisement> {
        self.validate(&observed.advertisement, now_ms)?;
        let log = observed
            .advertisement
            .log
            .as_ref()
            .ok_or(Error::Node("node session has no enrolled log"))?
            .activate(observed.advertisement.node)?;
        let mut next = observed.advertisement.clone();
        next.generation = next
            .generation
            .checked_add(1)
            .ok_or(Error::Node("node session generation overflow"))?;
        next.log = Some(log);
        self.update_advertisement(observed, next, now_ms).await
    }

    /// CAS-advances the largest contiguous node sequence covered by object roots.
    pub async fn advance_log_coverage(
        &self,
        observed: &VersionedNodeAdvertisement,
        tiered_through: u64,
        now_ms: i64,
    ) -> Result<VersionedNodeAdvertisement> {
        self.validate(&observed.advertisement, now_ms)?;
        let log = observed
            .advertisement
            .log
            .as_ref()
            .ok_or(Error::Node("node session has no enrolled log"))?
            .advance_tiered(observed.advertisement.node, tiered_through)?;
        let mut next = observed.advertisement.clone();
        next.generation = next
            .generation
            .checked_add(1)
            .ok_or(Error::Node("node session generation overflow"))?;
        next.log = Some(log);
        self.update_advertisement(observed, next, now_ms).await
    }

    /// CASes a fully object-covered old epoch to a newly selected inactive epoch.
    pub async fn rotate_log(
        &self,
        observed: &VersionedNodeAdvertisement,
        barrier: &NodeLogRotationBarrier,
        required_follower_bytes: u64,
        live_node_limit: usize,
        now_ms: i64,
    ) -> Result<VersionedNodeAdvertisement> {
        self.validate(&observed.advertisement, now_ms)?;
        let current = observed
            .advertisement
            .log
            .as_ref()
            .ok_or(Error::Node("node session has no enrolled log"))?;
        if barrier.leader_session() != observed.advertisement.session
            || barrier.log_epoch() != current.epoch()
            || barrier.members() != current.members()
            || barrier.covered_through() != current.tiered_through()
        {
            return Err(Error::Node("node-log rotation barrier differs"));
        }
        let members = self
            .select_log_members(
                observed.advertisement.session,
                required_follower_bytes,
                now_ms,
                live_node_limit,
            )
            .await?;
        if members.is_empty() {
            return Err(Error::Node("node-log follower ensemble is unavailable"));
        }
        let next_epoch = current
            .epoch()
            .checked_add(1)
            .ok_or(Error::Node("node-log epoch overflow"))?;
        let mut next = observed.advertisement.clone();
        next.generation = next
            .generation
            .checked_add(1)
            .ok_or(Error::Node("node session generation overflow"))?;
        next.log = Some(NodeLogStatus::open(next.node, next_epoch, members)?);
        self.update_advertisement(observed, next, now_ms).await
    }

    /// CAS-clears one fully object-covered log before clean session withdrawal.
    pub async fn close_log(
        &self,
        observed: &VersionedNodeAdvertisement,
        barrier: &NodeLogRotationBarrier,
        now_ms: i64,
    ) -> Result<VersionedNodeAdvertisement> {
        self.validate(&observed.advertisement, now_ms)?;
        let current = observed
            .advertisement
            .log
            .as_ref()
            .ok_or(Error::Node("node session has no enrolled log"))?;
        if current.phase() != NodeLogPhase::Open
            || barrier.leader_session() != observed.advertisement.session
            || barrier.log_epoch() != current.epoch()
            || barrier.members() != current.members()
            || barrier.covered_through() != current.tiered_through()
        {
            return Err(Error::Node("node-log close barrier differs"));
        }
        let mut next = observed.advertisement.clone();
        next.generation = next
            .generation
            .checked_add(1)
            .ok_or(Error::Node("node session generation overflow"))?;
        next.log = None;
        self.update_advertisement(observed, next, now_ms).await
    }

    fn validate(&self, advertisement: &NodeAdvertisement, now_ms: i64) -> Result<()> {
        advertisement.validate_at(now_ms)?;
        advertisement.verify_signature()?;
        self.validate_scope(advertisement)
    }

    async fn update_advertisement(
        &self,
        observed: &VersionedNodeAdvertisement,
        next: NodeAdvertisement,
        now_ms: i64,
    ) -> Result<VersionedNodeAdvertisement> {
        let path = self.layout.node_path(next.session.as_bytes());
        match self
            .layout
            .store()
            .update(&path, Bytes::from(next.encode()?), observed.token.clone())
            .await
        {
            Ok(token) => Ok(VersionedNodeAdvertisement {
                advertisement: next,
                token,
            }),
            Err(update_error) => match self.load(next.session, now_ms).await? {
                Some(current) if current.advertisement == next => Ok(current),
                Some(_) | None => Err(update_error.into()),
            },
        }
    }

    async fn load_canonical(
        &self,
        session: SessionId,
    ) -> Result<Option<(NodeAdvertisement, ETag)>> {
        let path = self.layout.node_path(session.as_bytes());
        let Some((record, token)) = self.load_record_at(&path).await? else {
            return Ok(None);
        };
        if record.session() != session {
            return Err(Error::Node("advertisement path and session differ"));
        }
        match record {
            NodeRecord::Advertisement(advertisement) => Ok(Some((*advertisement, token))),
            NodeRecord::Tombstone(_) => Ok(None),
        }
    }

    async fn load_record_at(
        &self,
        path: &object_store::path::Path,
    ) -> Result<Option<(NodeRecord, ETag)>> {
        let (body, token) = match self
            .layout
            .store()
            .get_with_etag_bounded(path, MAX_NODE_BYTES)
            .await
        {
            Ok(value) => value,
            Err(StorageError::NotFound { .. }) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        Ok(Some((NodeRecord::decode_canonical(&body)?, token)))
    }

    fn validate_scope(&self, advertisement: &NodeAdvertisement) -> Result<()> {
        if advertisement.fleet != self.fleet
            || advertisement.image != self.image
            || advertisement.release != self.release
        {
            return Err(Error::Node("advertisement fleet, image or release differs"));
        }
        Ok(())
    }
}

fn recovery_executor_eligible(advertisement: &NodeAdvertisement) -> bool {
    let capacity = advertisement.capacity();
    let placement_has_headroom = advertisement.placement_capacity().is_none_or(|placement| {
        placement.active_cells < placement.max_active_cells
            && placement.running_jobs < placement.job_capacity
    });
    capacity.log_protocol == NODE_LOG_PROTOCOL_VERSION
        && capacity.free_memory_bytes != 0
        && capacity.free_disk_bytes != 0
        && capacity.job_credits != 0
        && placement_has_headroom
}

fn validate_record_path(
    layout: &CellStorageLayout,
    session: SessionId,
    path: &object_store::path::Path,
) -> Result<()> {
    if layout.node_path(session.as_bytes()) != *path {
        return Err(Error::Node("advertisement path and session differ"));
    }
    Ok(())
}

enum NodeRecord {
    Advertisement(Box<NodeAdvertisement>),
    Tombstone(Box<NodeTombstone>),
}

impl NodeRecord {
    fn decode_canonical(bytes: &[u8]) -> Result<Self> {
        if let Ok(advertisement) = NodeAdvertisement::decode_canonical(bytes) {
            return Ok(Self::Advertisement(Box::new(advertisement)));
        }
        Ok(Self::Tombstone(Box::new(NodeTombstone::decode_canonical(
            bytes,
        )?)))
    }

    const fn session(&self) -> SessionId {
        match self {
            Self::Advertisement(advertisement) => advertisement.session,
            Self::Tombstone(tombstone) => tombstone.session,
        }
    }

    fn log(&self) -> Option<&NodeLogStatus> {
        match self {
            Self::Advertisement(advertisement) => advertisement.log.as_ref(),
            Self::Tombstone(tombstone) => tombstone.log.as_ref(),
        }
    }
}

struct NodeTombstone {
    session: SessionId,
    node: NodeId,
    expires_at_ms: i64,
    retired_at_ms: i64,
    claimant: Option<SessionId>,
    claim_generation: u64,
    claim_expires_at_ms: Option<i64>,
    log: Option<NodeLogStatus>,
}

impl NodeTombstone {
    fn new(
        session: SessionId,
        node: NodeId,
        expires_at_ms: i64,
        retired_at_ms: i64,
        claimant: Option<SessionId>,
        log: Option<NodeLogStatus>,
    ) -> Result<Self> {
        let tombstone = Self {
            session,
            node,
            expires_at_ms,
            retired_at_ms,
            claimant,
            claim_generation: u64::from(claimant.is_some()),
            claim_expires_at_ms: claimant.map(|_| {
                retired_at_ms.saturating_add(crate::node_log_state::RECOVERY_CLAIM_LIFETIME_MS)
            }),
            log,
        };
        tombstone.validate()?;
        Ok(tombstone)
    }

    fn claim(mut self, claimant: SessionId, now_ms: i64) -> Result<Self> {
        let generation = match (self.claimant, self.claim_expires_at_ms) {
            (Some(current), Some(expires_at_ms))
                if current == claimant && now_ms < expires_at_ms =>
            {
                return Ok(self);
            }
            (Some(_), Some(expires_at_ms)) if now_ms < expires_at_ms => {
                return Err(Error::Node("node recovery is already claimed"));
            }
            (Some(_), Some(_)) => self
                .claim_generation
                .checked_add(1)
                .ok_or(Error::Node("node recovery claim generation overflow"))?,
            (None, None) => 1,
            _ => return Err(Error::Node("node recovery claim is invalid")),
        };
        self.claimant = Some(claimant);
        self.claim_generation = generation;
        self.claim_expires_at_ms = Some(
            now_ms
                .checked_add(crate::node_log_state::RECOVERY_CLAIM_LIFETIME_MS)
                .ok_or(Error::Node("node recovery claim time overflow"))?,
        );
        if let Some(log) = &self.log {
            self.log = Some(log.begin_recovery(self.node, claimant, now_ms)?);
        }
        self.validate()?;
        Ok(self)
    }

    fn renew(mut self, fenced: &FencedNodeSession, now_ms: i64) -> Result<Self> {
        if self.session != fenced.session
            || self.claimant != Some(fenced.claimant)
            || self.claim_generation != fenced.claim_generation
        {
            return Err(Error::Fenced);
        }
        let current_expiry = self
            .claim_expires_at_ms
            .ok_or(Error::Node("node recovery claim expiry is missing"))?;
        if now_ms >= current_expiry {
            return Err(Error::Fenced);
        }
        let next_expiry = now_ms
            .checked_add(crate::node_log_state::RECOVERY_CLAIM_LIFETIME_MS)
            .filter(|expires_at_ms| *expires_at_ms > current_expiry)
            .ok_or(Error::Node("node recovery claim expiry did not advance"))?;
        self.claim_expires_at_ms = Some(next_expiry);
        if let Some(log) = &self.log {
            self.log = Some(log.renew_recovery(
                self.node,
                fenced.claimant,
                fenced.claim_generation,
                now_ms,
            )?);
        }
        self.validate()?;
        Ok(self)
    }

    fn seal(
        mut self,
        fenced: &FencedNodeSession,
        recovery_manifest: Option<Digest>,
        now_ms: i64,
    ) -> Result<Self> {
        if self.session != fenced.session
            || self.claimant != Some(fenced.claimant)
            || self.claim_generation != fenced.claim_generation
            || self
                .claim_expires_at_ms
                .is_none_or(|expires_at_ms| expires_at_ms <= now_ms)
        {
            return Err(Error::Fenced);
        }
        let log = self
            .log
            .as_ref()
            .ok_or(Error::Node("claimed session has no enrolled node log"))?
            .seal_recovery(
                self.node,
                fenced.claimant,
                fenced.claim_generation,
                recovery_manifest,
            )?;
        self.claimant = None;
        self.claim_generation = 0;
        self.claim_expires_at_ms = None;
        self.log = Some(log);
        self.validate()?;
        Ok(self)
    }

    fn fenced(&self) -> Result<FencedNodeSession> {
        let claimant = self
            .claimant
            .ok_or(Error::Node("node session is not claimed"))?;
        Ok(FencedNodeSession {
            node: self.node,
            session: self.session,
            claimant,
            claim_generation: self.claim_generation,
            claim_expires_at_ms: self
                .claim_expires_at_ms
                .ok_or(Error::Node("node recovery claim expiry is missing"))?,
            log: self.log.clone(),
        })
    }

    fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let encoded = serde_json::to_vec(&RawNodeTombstoneEnvelope::from(self))?;
        if encoded.len() as u64 > MAX_NODE_BYTES {
            return Err(Error::Node("node tombstone exceeds 64 KiB"));
        }
        Ok(encoded)
    }

    fn decode_canonical(bytes: &[u8]) -> Result<Self> {
        if bytes.len() as u64 > MAX_NODE_BYTES {
            return Err(Error::Node("node tombstone exceeds 64 KiB"));
        }
        let raw: RawNodeTombstoneEnvelope = serde_json::from_slice(bytes)?;
        if raw.tombstone.version != 1 {
            return Err(Error::Node("unsupported node tombstone version"));
        }
        let raw = raw.tombstone;
        let session = SessionId::from_bytes(decode_hex(&raw.session)?);
        let node = NodeId::from_bytes(decode_hex(&raw.node)?);
        let tombstone = Self {
            session,
            node,
            expires_at_ms: canonical_i64(&raw.expires_at_ms)?,
            retired_at_ms: canonical_i64(&raw.retired_at_ms)?,
            claimant: raw
                .claimant
                .map(|claimant| decode_hex(&claimant).map(SessionId::from_bytes))
                .transpose()?,
            claim_generation: canonical_u64(&raw.claim_generation)?,
            claim_expires_at_ms: raw
                .claim_expires_at_ms
                .as_deref()
                .map(canonical_i64)
                .transpose()?,
            log: raw.log.map(|log| decode_log(node, log)).transpose()?,
        };
        tombstone.validate()?;
        if tombstone.encode()?.as_slice() != bytes {
            return Err(Error::Node("node tombstone JSON is not canonical"));
        }
        Ok(tombstone)
    }

    fn validate(&self) -> Result<()> {
        if self.session.as_bytes().iter().all(|byte| *byte == 0)
            || self.node.as_bytes().iter().all(|byte| *byte == 0)
            || self.expires_at_ms < 0
            || self.retired_at_ms < 0
            || self.claimant.is_some_and(|claimant| {
                claimant == self.session || claimant.as_bytes().iter().all(|byte| *byte == 0)
            })
            || self.claimant.is_some() != self.claim_expires_at_ms.is_some()
            || self.claimant.is_some() != (self.claim_generation != 0)
            || self
                .claim_expires_at_ms
                .is_some_and(|expires_at_ms| expires_at_ms <= self.retired_at_ms)
        {
            return Err(Error::Node("node tombstone is invalid"));
        }
        if let Some(log) = &self.log {
            log.validate(self.node)?;
            let log_claim = log.recovery();
            if self.claimant.is_some() != (log.phase() == NodeLogPhase::Recovering)
                || log_claim.map(|claim| claim.claimant()) != self.claimant
                || log_claim.map(|claim| claim.generation())
                    != (self.claim_generation != 0).then_some(self.claim_generation)
                || log_claim.map(|claim| claim.expires_at_ms()) != self.claim_expires_at_ms
            {
                return Err(Error::Node("node tombstone log claim differs"));
            }
        }
        Ok(())
    }
}

fn validate_successor(current: &NodeAdvertisement, next: &NodeAdvertisement) -> Result<()> {
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

fn same_boot_identity(current: &NodeAdvertisement, next: &NodeAdvertisement) -> bool {
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
        && current.signature == next.signature
}

fn compare_member_candidate(
    left: &(&NodeAdvertisement, [u8; 32]),
    right: &(&NodeAdvertisement, [u8; 32]),
    context: &[&NodeAdvertisement],
) -> std::cmp::Ordering {
    let zone_score = |candidate: &NodeAdvertisement| {
        context
            .iter()
            .filter(|other| {
                known_domain_difference(
                    candidate.failure_domain.zone(),
                    other.failure_domain.zone(),
                )
            })
            .count()
    };
    let host_score = |candidate: &NodeAdvertisement| {
        context
            .iter()
            .filter(|other| {
                known_domain_difference(
                    candidate.failure_domain.host(),
                    other.failure_domain.host(),
                )
            })
            .count()
    };
    zone_score(left.0)
        .cmp(&zone_score(right.0))
        .then_with(|| host_score(left.0).cmp(&host_score(right.0)))
        .then_with(|| left.1.cmp(&right.1))
        .then_with(|| right.0.node.as_bytes().cmp(left.0.node.as_bytes()))
}

fn known_domain_difference(left: Option<&str>, right: Option<&str>) -> bool {
    matches!((left, right), (Some(left), Some(right)) if left != right)
}

fn valid_endpoint(endpoint: &str) -> bool {
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

fn valid_host(host: &str) -> bool {
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

fn strictly_sorted<T: Ord>(values: &[T]) -> bool {
    values.windows(2).all(|pair| pair[0] < pair[1])
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawUnsignedIdentity {
    node: String,
    session: String,
    endpoint: String,
    fleet: String,
    certificate: String,
    image: String,
    release: String,
    public_key: String,
    module_digests: Vec<String>,
    peer_versions: Vec<u32>,
    failure_domain: RawFailureDomain,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawFailureDomain {
    zone: Option<String>,
    host: Option<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawNodeTombstoneEnvelope {
    tombstone: RawNodeTombstone,
}

impl From<&NodeTombstone> for RawNodeTombstoneEnvelope {
    fn from(value: &NodeTombstone) -> Self {
        Self {
            tombstone: RawNodeTombstone {
                version: 1,
                session: encode_hex(value.session.as_bytes()),
                node: encode_hex(value.node.as_bytes()),
                expires_at_ms: value.expires_at_ms.to_string(),
                retired_at_ms: value.retired_at_ms.to_string(),
                claimant: value
                    .claimant
                    .map(|claimant| encode_hex(claimant.as_bytes())),
                claim_generation: value.claim_generation.to_string(),
                claim_expires_at_ms: value.claim_expires_at_ms.map(|value| value.to_string()),
                log: value.log.as_ref().map(encode_log),
            },
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawNodeTombstone {
    version: u8,
    session: String,
    node: String,
    expires_at_ms: String,
    retired_at_ms: String,
    claimant: Option<String>,
    claim_generation: String,
    claim_expires_at_ms: Option<String>,
    log: Option<RawNodeLog>,
}

impl From<&NodeAdvertisement> for RawUnsignedIdentity {
    fn from(value: &NodeAdvertisement) -> Self {
        Self {
            node: encode_hex(value.node.as_bytes()),
            session: encode_hex(value.session.as_bytes()),
            endpoint: value.endpoint.clone(),
            fleet: encode_hex(value.fleet.as_bytes()),
            certificate: encode_hex(value.certificate.as_bytes()),
            image: encode_hex(value.image.as_bytes()),
            release: encode_hex(value.release.as_bytes()),
            public_key: encode_hex(&value.public_key),
            module_digests: value
                .module_digests
                .iter()
                .map(|digest| encode_hex(digest.as_bytes()))
                .collect(),
            peer_versions: value.peer_versions.clone(),
            failure_domain: RawFailureDomain {
                zone: value.failure_domain.zone.clone(),
                host: value.failure_domain.host.clone(),
            },
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawAdvertisement {
    version: u8,
    identity: RawIdentity,
    lease: RawLease,
    log: Option<RawNodeLog>,
    capacity: RawCapacity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    placement: Option<RawPlacementCapacity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    placement_version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    placement_signature: Option<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawIdentity {
    #[serde(flatten)]
    unsigned: RawUnsignedIdentity,
    signature: String,
}

impl From<&NodeAdvertisement> for RawAdvertisement {
    fn from(value: &NodeAdvertisement) -> Self {
        Self {
            version: 1,
            identity: RawIdentity {
                unsigned: RawUnsignedIdentity::from(value),
                signature: encode_hex(&value.signature),
            },
            lease: RawLease {
                generation: value.generation.to_string(),
                progress: value.progress.to_string(),
                issued_at_ms: value.issued_at_ms.to_string(),
                expires_at_ms: value.expires_at_ms.to_string(),
            },
            log: value.log.as_ref().map(encode_log),
            capacity: RawCapacity {
                free_memory_bytes: value.capacity.free_memory_bytes.to_string(),
                free_disk_bytes: value.capacity.free_disk_bytes.to_string(),
                follower_free_bytes: value.capacity.follower_free_bytes.to_string(),
                follower_retained_bytes: value.capacity.follower_retained_bytes.to_string(),
                job_credits: value.capacity.job_credits,
                log_protocol: value.capacity.log_protocol,
            },
            placement: value.placement.map(|placement| RawPlacementCapacity {
                memory_capacity_bytes: placement.memory_capacity_bytes.to_string(),
                disk_capacity_bytes: placement.disk_capacity_bytes.to_string(),
                active_cells: placement.active_cells,
                max_active_cells: placement.max_active_cells,
                running_jobs: placement.running_jobs,
                job_capacity: placement.job_capacity,
            }),
            placement_version: (value.placement_version != 0).then_some(value.placement_version),
            placement_signature: value
                .placement_signature
                .iter()
                .any(|byte| *byte != 0)
                .then(|| encode_hex(&value.placement_signature)),
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawLease {
    generation: String,
    progress: String,
    issued_at_ms: String,
    expires_at_ms: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawCapacity {
    free_memory_bytes: String,
    free_disk_bytes: String,
    follower_free_bytes: String,
    follower_retained_bytes: String,
    job_credits: u32,
    log_protocol: u32,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawPlacementCapacity {
    memory_capacity_bytes: String,
    disk_capacity_bytes: String,
    active_cells: u32,
    max_active_cells: u32,
    running_jobs: u32,
    job_capacity: u32,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawNodeLog {
    state: RawNodeLogPhase,
    epoch: String,
    members: Vec<String>,
    active: bool,
    tiered_through: String,
    recovery: Option<RawNodeRecoveryClaim>,
    recovery_manifest: Option<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum RawNodeLogPhase {
    Open,
    Recovering,
    Sealed,
    Retired,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RawNodeRecoveryClaim {
    claimant: String,
    generation: String,
    expires_at_ms: String,
}

impl TryFrom<RawAdvertisement> for NodeAdvertisement {
    type Error = Error;

    fn try_from(value: RawAdvertisement) -> Result<Self> {
        if value.version != 1 {
            return Err(Error::Node("unsupported advertisement version"));
        }
        let raw = value.identity.unsigned;
        let session = SessionId::from_bytes(decode_hex(&raw.session)?);
        let node = NodeId::from_bytes(decode_hex(&raw.node)?);
        Ok(Self {
            node,
            session,
            endpoint: raw.endpoint,
            fleet: Digest::from_bytes(decode_hex(&raw.fleet)?),
            certificate: Digest::from_bytes(decode_hex(&raw.certificate)?),
            image: Digest::from_bytes(decode_hex(&raw.image)?),
            release: Digest::from_bytes(decode_hex(&raw.release)?),
            public_key: decode_hex(&raw.public_key)?,
            generation: canonical_u64(&value.lease.generation)?,
            progress: canonical_u64(&value.lease.progress)?,
            issued_at_ms: canonical_i64(&value.lease.issued_at_ms)?,
            expires_at_ms: canonical_i64(&value.lease.expires_at_ms)?,
            module_digests: raw
                .module_digests
                .iter()
                .map(|value| decode_hex(value).map(Digest::from_bytes))
                .collect::<Result<Vec<_>>>()?,
            peer_versions: raw.peer_versions,
            failure_domain: NodeFailureDomain::new(
                raw.failure_domain.zone,
                raw.failure_domain.host,
            )?,
            capacity: NodeCapacity {
                free_memory_bytes: canonical_u64(&value.capacity.free_memory_bytes)?,
                free_disk_bytes: canonical_u64(&value.capacity.free_disk_bytes)?,
                follower_free_bytes: canonical_u64(&value.capacity.follower_free_bytes)?,
                follower_retained_bytes: canonical_u64(&value.capacity.follower_retained_bytes)?,
                job_credits: value.capacity.job_credits,
                log_protocol: value.capacity.log_protocol,
            },
            placement: value
                .placement
                .map(|placement| {
                    NodePlacementCapacity::new(
                        canonical_u64(&placement.memory_capacity_bytes)?,
                        canonical_u64(&placement.disk_capacity_bytes)?,
                        placement.active_cells,
                        placement.max_active_cells,
                        placement.running_jobs,
                        placement.job_capacity,
                    )
                })
                .transpose()?,
            log: value.log.map(|log| decode_log(node, log)).transpose()?,
            signature: decode_hex(&value.identity.signature)?,
            placement_version: value.placement_version.unwrap_or(0),
            placement_signature: value
                .placement_signature
                .map(|signature| decode_hex(&signature))
                .transpose()?
                .unwrap_or([0; 64]),
        })
    }
}

fn encode_log(log: &NodeLogStatus) -> RawNodeLog {
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

fn decode_log(leader: NodeId, raw: RawNodeLog) -> Result<NodeLogStatus> {
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

fn canonical_u64(value: &str) -> Result<u64> {
    let parsed = value
        .parse::<u64>()
        .map_err(|_| Error::Node("invalid unsigned decimal"))?;
    if parsed.to_string() != value {
        return Err(Error::Node("unsigned decimal is not canonical"));
    }
    Ok(parsed)
}

fn canonical_i64(value: &str) -> Result<i64> {
    let parsed = value
        .parse::<i64>()
        .map_err(|_| Error::Node("invalid signed decimal"))?;
    if parsed.to_string() != value {
        return Err(Error::Node("signed decimal is not canonical"));
    }
    Ok(parsed)
}

fn encode_hex(bytes: &[u8]) -> String {
    const TABLE: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(TABLE[(byte >> 4) as usize] as char);
        encoded.push(TABLE[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn decode_hex<const N: usize>(value: &str) -> Result<[u8; N]> {
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

fn nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
