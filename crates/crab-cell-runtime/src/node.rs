use bytes::Bytes;
use crab_storage::{CellStorageLayout, ETag, StorageError, map_object_store_error};
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};

use crate::{Digest, Error, NodeLogPhase, NodeLogStatus, NodeRecoveryClaim, Result, SessionId};

const MAX_NODE_BYTES: u64 = 64 * 1024;
const MAX_ENDPOINT_BYTES: usize = 512;
const MAX_MODULES: usize = 128;
const MAX_PEER_VERSIONS: usize = 16;
const MAX_ADVERTISEMENT_LIFETIME_MS: i64 = 15_000;
const MAX_CLOCK_SKEW_MS: i64 = 5 * 60_000;
const STALE_ADVERTISEMENT_RETENTION_MS: i64 = MAX_CLOCK_SKEW_MS + MAX_ADVERTISEMENT_LIFETIME_MS;
const MAX_STALE_COLLECTION_ITEMS: usize = 1_024;
const SIGNING_DOMAIN: &[u8] = b"crab.node.v1\0";
const NODE_LOG_SELECTION_DOMAIN: &[u8] = b"crab.node-log.member.v1\0";

/// Current private follower-log wire and persistence protocol.
pub const NODE_LOG_PROTOCOL_VERSION: u32 = 1;

/// Capacity hints published by one node boot session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NodeCapacity {
    pub free_memory_bytes: u64,
    pub free_disk_bytes: u64,
    pub follower_free_bytes: u64,
    pub job_credits: u32,
    pub log_protocol: u32,
}

/// Signed, short-lived identity and capacity advertisement for one node session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeAdvertisement {
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
    capacity: NodeCapacity,
    log: Option<NodeLogStatus>,
    signature: [u8; 64],
}

impl NodeAdvertisement {
    /// Builds and signs a canonical advertisement with the boot-session key.
    #[expect(
        clippy::too_many_arguments,
        reason = "all signed identity fields stay explicit"
    )]
    pub fn sign(
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
        capacity: NodeCapacity,
    ) -> Result<Self> {
        let mut advertisement = Self {
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
            capacity,
            log: None,
            signature: [0; 64],
        };
        advertisement.validate_shape()?;
        advertisement.signature = signing_key.sign(&advertisement.signing_bytes()?).to_bytes();
        Ok(advertisement)
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
    pub fn module_digests(&self) -> &[Digest] {
        &self.module_digests
    }

    #[must_use]
    pub fn peer_versions(&self) -> &[u32] {
        &self.peer_versions
    }

    #[must_use]
    pub const fn capacity(&self) -> NodeCapacity {
        self.capacity
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
        if self.session.as_bytes().iter().all(|byte| *byte == 0)
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
            log.validate(self.session)?;
            if log.phase() != NodeLogPhase::Open || log.recovery().is_some() {
                return Err(Error::Node("live node advertisement has a terminal log"));
            }
        }
        if self.capacity.follower_free_bytes > self.capacity.free_disk_bytes
            || (self.capacity.log_protocol == 0 && self.capacity.follower_free_bytes != 0)
        {
            return Err(Error::Node("advertisement follower capacity is invalid"));
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
            .map_err(Error::PeerSignature)
    }

    fn signing_bytes(&self) -> Result<Vec<u8>> {
        let unsigned = serde_json::to_vec(&RawUnsignedIdentity::from(self))?;
        let mut bytes = Vec::with_capacity(SIGNING_DOMAIN.len() + unsigned.len());
        bytes.extend_from_slice(SIGNING_DOMAIN);
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
    session: SessionId,
    claimant: SessionId,
    claim_generation: u64,
    claim_expires_at_ms: i64,
    log: Option<NodeLogStatus>,
}

impl FencedNodeSession {
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
                tombstone.claim(claimant, now_ms)?
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
        let renewed = current.renew(fenced, now_ms)?;
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
        let sealed = current.seal(fenced, recovery_manifest, now_ms)?;
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

    /// Verifies that this exact follower belongs to a live leader log epoch.
    pub async fn authorize_log_append(
        &self,
        leader: SessionId,
        member: SessionId,
        log_epoch: u64,
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
        log.permits_append(leader, member, log_epoch)?;
        Ok(log.clone())
    }

    /// Verifies a live claimant may seal or read this follower's failed-owner lane.
    pub async fn authorize_log_recovery(
        &self,
        leader: SessionId,
        claimant: SessionId,
        member: SessionId,
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
        log.permits_recovery_read(leader, claimant, member, log_epoch, now_ms)?;
        Ok(log.clone())
    }

    /// Streams and verifies every currently live boot-session advertisement.
    ///
    /// Expired records do not count against `limit`; malformed, misplaced, or
    /// foreign live records fail closed so maintenance cannot mistake an active
    /// incompatible fleet for an offline deployment.
    pub async fn live(&self, now_ms: i64, limit: usize) -> Result<Vec<NodeAdvertisement>> {
        self.scan_advertisements(now_ms, limit, AdvertisementScan::LiveRelease)
            .await
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
    ) -> Result<Vec<SessionId>> {
        if required_follower_bytes == 0 {
            return Err(Error::Node("node-log follower byte requirement is zero"));
        }
        let live = self.live(now_ms, limit).await?;
        if !live.iter().any(|candidate| candidate.session == leader) {
            return Err(Error::Node("node-log leader is not live"));
        }
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
                candidate.session != leader
                    && candidate.capacity.log_protocol == NODE_LOG_PROTOCOL_VERSION
                    && candidate.capacity.follower_free_bytes >= required_follower_bytes
                    && candidate.capacity.free_memory_bytes != 0
                    && candidate.capacity.free_disk_bytes != 0
                    && candidate.capacity.job_credits != 0
            })
            .map(|candidate| {
                let mut hasher = blake3::Hasher::new();
                hasher.update(NODE_LOG_SELECTION_DOMAIN);
                hasher.update(leader.as_bytes());
                hasher.update(candidate.session.as_bytes());
                (candidate.session, *hasher.finalize().as_bytes())
            })
            .collect::<Vec<_>>();
        if eligible.len() < desired {
            return Ok(Vec::new());
        }
        eligible.sort_unstable_by(|(left_session, left_score), (right_session, right_score)| {
            right_score
                .cmp(left_score)
                .then_with(|| left_session.as_bytes().cmp(right_session.as_bytes()))
        });
        let mut selected = eligible
            .into_iter()
            .take(desired)
            .map(|(session, _)| session)
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
            return Err(Error::Node("node-log follower ensemble is unavailable"));
        }
        let mut next = observed.advertisement.clone();
        next.generation = next
            .generation
            .checked_add(1)
            .ok_or(Error::Node("node session generation overflow"))?;
        next.log = Some(NodeLogStatus::open(next.session, log_epoch, members)?);
        self.update_advertisement(observed, next, now_ms).await
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
            .activate(observed.advertisement.session)?;
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
            .advance_tiered(observed.advertisement.session, tiered_through)?;
        let mut next = observed.advertisement.clone();
        next.generation = next
            .generation
            .checked_add(1)
            .ok_or(Error::Node("node session generation overflow"))?;
        next.log = Some(log);
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
    Tombstone(NodeTombstone),
}

impl NodeRecord {
    fn decode_canonical(bytes: &[u8]) -> Result<Self> {
        if let Ok(advertisement) = NodeAdvertisement::decode_canonical(bytes) {
            return Ok(Self::Advertisement(Box::new(advertisement)));
        }
        Ok(Self::Tombstone(NodeTombstone::decode_canonical(bytes)?))
    }

    const fn session(&self) -> SessionId {
        match self {
            Self::Advertisement(advertisement) => advertisement.session,
            Self::Tombstone(tombstone) => tombstone.session,
        }
    }
}

struct NodeTombstone {
    session: SessionId,
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
        expires_at_ms: i64,
        retired_at_ms: i64,
        claimant: Option<SessionId>,
        log: Option<NodeLogStatus>,
    ) -> Result<Self> {
        let tombstone = Self {
            session,
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
            self.log = Some(log.begin_recovery(self.session, claimant, now_ms)?);
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
                self.session,
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
                self.session,
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
        let tombstone = Self {
            session,
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
            log: raw.log.map(|log| decode_log(session, log)).transpose()?,
        };
        tombstone.validate()?;
        if tombstone.encode()?.as_slice() != bytes {
            return Err(Error::Node("node tombstone JSON is not canonical"));
        }
        Ok(tombstone)
    }

    fn validate(&self) -> Result<()> {
        if self.session.as_bytes().iter().all(|byte| *byte == 0)
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
            log.validate(self.session)?;
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
    current.session == next.session
        && current.endpoint == next.endpoint
        && current.fleet == next.fleet
        && current.certificate == next.certificate
        && current.image == next.image
        && current.release == next.release
        && current.public_key == next.public_key
        && current.module_digests == next.module_digests
        && current.peer_versions == next.peer_versions
        && current.signature == next.signature
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
    session: String,
    endpoint: String,
    fleet: String,
    certificate: String,
    image: String,
    release: String,
    public_key: String,
    module_digests: Vec<String>,
    peer_versions: Vec<u32>,
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
                job_credits: value.capacity.job_credits,
                log_protocol: value.capacity.log_protocol,
            },
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
    job_credits: u32,
    log_protocol: u32,
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
        Ok(Self {
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
            capacity: NodeCapacity {
                free_memory_bytes: canonical_u64(&value.capacity.free_memory_bytes)?,
                free_disk_bytes: canonical_u64(&value.capacity.free_disk_bytes)?,
                follower_free_bytes: canonical_u64(&value.capacity.follower_free_bytes)?,
                job_credits: value.capacity.job_credits,
                log_protocol: value.capacity.log_protocol,
            },
            log: value.log.map(|log| decode_log(session, log)).transpose()?,
            signature: decode_hex(&value.identity.signature)?,
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

fn decode_log(leader: SessionId, raw: RawNodeLog) -> Result<NodeLogStatus> {
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
            .map(|member| decode_hex(member).map(SessionId::from_bytes))
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
