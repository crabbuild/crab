//! Node directory records, tombstones, and recovery-candidate windows.
use crate::node::advertisement::RawNodeTombstoneEnvelope;
use crate::node::advertisement::canonical_i64;
use crate::node::advertisement::canonical_u64;
use crate::node::advertisement::decode_hex;
use crate::node::advertisement::decode_log;
use crate::node::advertisement::same_boot_identity;
use crate::node::advertisement::validate_successor;

use super::*;

mod advertisement;
mod log;
mod recovery;

/// Object-store directory for one fleet and compiled release.
#[derive(Clone)]
pub struct NodeDirectory {
    pub(super) layout: CellStorageLayout,
    pub(super) fleet: Digest,
    pub(super) image: Digest,
    pub(super) release: Digest,
    // Candidate discovery is advisory; claims always reload the authoritative
    // record. Sharing this short-lived snapshot keeps cloned schedulers from
    // multiplying a full directory scan without changing failover authority.
    pub(super) recovery_scan: Arc<RwLock<Option<Arc<RecoveryScanSnapshot>>>>,
}

/// Request verifier bound to one mTLS-authenticated enrollment observation.
pub struct EnrolledPeerVerifier {
    advertisement: NodeAdvertisement,
    verifier: crate::peer::PeerVerifier,
}

impl EnrolledPeerVerifier {
    /// Verifies the signed request while rechecking the enrollment's lifetime.
    pub fn verify(
        &self,
        request: crate::peer::UnverifiedPeerRequest,
        now_ms: i64,
    ) -> Result<crate::peer::VerifiedPeerRequest> {
        self.advertisement.validate_at(now_ms)?;
        self.verifier.verify_decoded(request, now_ms)
    }
}

#[derive(Clone, Copy)]
pub(super) enum AdvertisementScan {
    LiveRelease,
    AdvertisedFleet,
}

impl NodeDirectory {
    /// Binds the directory to one fleet, image, and release scope.
    #[must_use]
    pub fn new(layout: CellStorageLayout, fleet: Digest, image: Digest, release: Digest) -> Self {
        Self {
            layout,
            fleet,
            image,
            release,
            recovery_scan: Arc::new(RwLock::new(None)),
        }
    }

    /// Returns the fleet this directory is scoped to.
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

    pub(super) fn validate(&self, advertisement: &NodeAdvertisement, now_ms: i64) -> Result<()> {
        advertisement.validate_at(now_ms)?;
        advertisement.verify_signature()?;
        self.validate_scope(advertisement)
    }

    pub(super) async fn update_advertisement(
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

    pub(super) async fn load_canonical(
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

    pub(super) async fn load_record_at(
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

    pub(super) fn validate_scope(&self, advertisement: &NodeAdvertisement) -> Result<()> {
        if advertisement.fleet != self.fleet
            || advertisement.image != self.image
            || advertisement.release != self.release
        {
            return Err(Error::Node("advertisement fleet, image or release differs"));
        }
        Ok(())
    }
}

pub(super) struct RecoveryScanSnapshot {
    pub(super) observed_at_ms: i64,
    pub(super) includes_live_nodes: bool,
    pub(super) live_nodes: HashSet<NodeId>,
    pub(super) records: Vec<RecoveryCandidateRecord>,
}

pub(super) struct RecoveryCandidateRecord {
    pub(super) session: SessionId,
    pub(super) expires_at_ms: i64,
    pub(super) claimant: Option<SessionId>,
    pub(super) claim_expires_at_ms: Option<i64>,
    pub(super) active: bool,
    pub(super) phase: NodeLogPhase,
    pub(super) members: Vec<NodeId>,
}

impl RecoveryCandidateRecord {
    pub(super) fn eligible_for(&self, claimant: SessionId, now_ms: i64) -> bool {
        self.expires_at_ms <= now_ms
            && self.active
            && matches!(self.phase, NodeLogPhase::Open | NodeLogPhase::Recovering)
            && (self.claimant == Some(claimant)
                || self
                    .claim_expires_at_ms
                    .is_none_or(|expires_at_ms| expires_at_ms <= now_ms))
    }
}

/// Bounded rotating window over the expired sessions discovered in one scan.
///
/// Object-store listings are not a durable work queue. Keeping only the first
/// page lets a permanently failing early session starve every later session,
/// so the window rotates its start key while retaining at most `2 * limit`
/// session IDs.
pub(super) struct RecoveryCandidateWindow {
    pub(super) start: [u8; 16],
    pub(super) limit: usize,
    pub(super) after: BTreeSet<[u8; 16]>,
    pub(super) before: BTreeSet<[u8; 16]>,
}

impl RecoveryCandidateWindow {
    pub(super) fn new(now_ms: i64, limit: usize) -> Result<Self> {
        if now_ms < 0 || limit == 0 {
            return Err(Error::Node("node recovery candidate window is invalid"));
        }
        let bucket = u64::try_from(now_ms / 1_000)
            .map_err(|_| Error::Node("node recovery candidate rotation overflows"))?;
        let mut hasher = blake3::Hasher::new();
        hasher.update(RECOVERY_CANDIDATE_ROTATION_DOMAIN);
        hasher.update(&bucket.to_be_bytes());
        let digest = hasher.finalize();
        let mut start = [0_u8; 16];
        start.copy_from_slice(&digest.as_bytes()[..16]);
        Ok(Self::with_start(start, limit))
    }

    pub(super) fn with_start(start: [u8; 16], limit: usize) -> Self {
        Self {
            start,
            limit,
            after: BTreeSet::new(),
            before: BTreeSet::new(),
        }
    }

    pub(super) fn push(&mut self, session: SessionId) {
        let key = *session.as_bytes();
        let window = if key >= self.start {
            &mut self.after
        } else {
            &mut self.before
        };
        if !window.insert(key) {
            return;
        }
        if window.len() > self.limit {
            let evicted = if key >= self.start {
                window.iter().next_back().copied()
            } else {
                window.iter().next().copied()
            };
            if let Some(evicted) = evicted {
                window.remove(&evicted);
            }
        }
    }

    pub(super) fn finish(self) -> Vec<SessionId> {
        self.after
            .into_iter()
            .chain(self.before.into_iter().rev())
            .take(self.limit)
            .map(SessionId::from_bytes)
            .collect()
    }
}

pub(super) fn recovery_executor_eligible(advertisement: &NodeAdvertisement) -> bool {
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

pub(super) fn validate_record_path(
    layout: &CellStorageLayout,
    session: SessionId,
    path: &object_store::path::Path,
) -> Result<()> {
    if layout.node_path(session.as_bytes()) != *path {
        return Err(Error::Node("advertisement path and session differ"));
    }
    Ok(())
}

pub(super) enum NodeRecord {
    Advertisement(Box<NodeAdvertisement>),
    Tombstone(Box<NodeTombstone>),
}

impl NodeRecord {
    pub(super) fn decode_canonical(bytes: &[u8]) -> Result<Self> {
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

    pub(super) fn log(&self) -> Option<&NodeLogStatus> {
        match self {
            Self::Advertisement(advertisement) => advertisement.log.as_ref(),
            Self::Tombstone(tombstone) => tombstone.log.as_ref(),
        }
    }
}

pub(super) struct NodeTombstone {
    pub(super) session: SessionId,
    pub(super) node: NodeId,
    pub(super) expires_at_ms: i64,
    pub(super) retired_at_ms: i64,
    pub(super) claimant: Option<SessionId>,
    pub(super) claim_generation: u64,
    pub(super) claim_expires_at_ms: Option<i64>,
    pub(super) log: Option<NodeLogStatus>,
}

impl NodeTombstone {
    pub(super) fn new(
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
                retired_at_ms.saturating_add(crate::node::log_state::RECOVERY_CLAIM_LIFETIME_MS)
            }),
            log,
        };
        tombstone.validate()?;
        Ok(tombstone)
    }

    pub(super) fn claim(mut self, claimant: SessionId, now_ms: i64) -> Result<Self> {
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
                .checked_add(crate::node::log_state::RECOVERY_CLAIM_LIFETIME_MS)
                .ok_or(Error::Node("node recovery claim time overflow"))?,
        );
        if let Some(log) = &self.log {
            self.log = Some(log.begin_recovery(self.node, claimant, now_ms)?);
        }
        self.validate()?;
        Ok(self)
    }

    pub(super) fn renew(mut self, fenced: &FencedNodeSession, now_ms: i64) -> Result<Self> {
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
            .checked_add(crate::node::log_state::RECOVERY_CLAIM_LIFETIME_MS)
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

    pub(super) fn seal(
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

    pub(super) fn fenced(&self) -> Result<FencedNodeSession> {
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

    pub(super) fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let encoded = serde_json::to_vec(&RawNodeTombstoneEnvelope::from(self))?;
        if encoded.len() as u64 > MAX_NODE_BYTES {
            return Err(Error::Node("node tombstone exceeds 64 KiB"));
        }
        Ok(encoded)
    }

    pub(super) fn decode_canonical(bytes: &[u8]) -> Result<Self> {
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

    pub(super) fn validate(&self) -> Result<()> {
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

pub(super) fn compare_member_candidate(
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

pub(super) fn known_domain_difference(left: Option<&str>, right: Option<&str>) -> bool {
    matches!((left, right), (Some(left), Some(right)) if left != right)
}
