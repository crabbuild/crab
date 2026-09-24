//! Node directory records, tombstones, and recovery-candidate windows.
use crate::node::advertisement::RawNodeTombstoneEnvelope;
use crate::node::advertisement::canonical_i64;
use crate::node::advertisement::canonical_u64;
use crate::node::advertisement::decode_hex;
use crate::node::advertisement::decode_log;
use crate::node::advertisement::same_boot_identity;
use crate::node::advertisement::validate_successor;

use super::*;

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

#[derive(Clone, Copy)]
pub(super) enum AdvertisementScan {
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
            recovery_scan: Arc::new(RwLock::new(None)),
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

    /// Chooses a destination using only authenticated, measured placement
    /// blocks advertised by the current live fleet.
    ///
    /// Nodes that have not rolled out the placement block are omitted. They
    /// remain usable for ordinary authority routing but cannot become an
    /// advisory destination through this method. This path chooses a new
    /// owner when no live owner can be preferred, so no candidate receives
    /// the owner stickiness bonus merely for handling the request.
    pub async fn choose_advertised_placement(
        &self,
        planner: &PlacementPlanner,
        cell: crate::CellId,
        now_ms: i64,
        limit: usize,
    ) -> Result<Option<PlacementScore>> {
        let live = self.live(now_ms, limit).await?;
        let observations = live
            .iter()
            .filter_map(|advertisement| {
                PlacementObservation::from_signed_advertisement(advertisement, now_ms, false).ok()
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
            .min(crate::node::log_state::MAX_NODE_LOG_MEMBERS);
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

    pub(super) async fn scan_advertisements(
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
        let stream = self.layout.store().inner().list(Some(&prefix));
        let mut records = stream
            .map(|item| {
                let prefix = prefix.clone();
                async move {
                    let meta =
                        item.map_err(|error| map_object_store_error(error, prefix.as_ref()))?;
                    let (body, _) = match self
                        .layout
                        .store()
                        .get_with_etag_bounded(&meta.location, MAX_NODE_BYTES)
                        .await
                    {
                        Ok(value) => value,
                        Err(StorageError::NotFound { .. }) => return Ok(None),
                        Err(error) => return Err(error.into()),
                    };
                    let NodeRecord::Advertisement(advertisement) =
                        NodeRecord::decode_canonical(&body)?
                    else {
                        return Ok(None);
                    };
                    validate_record_path(&self.layout, advertisement.session, &meta.location)?;
                    match scan {
                        AdvertisementScan::LiveRelease => {
                            if advertisement.expires_at_ms <= now_ms {
                                return Ok(None);
                            }
                            self.validate(&advertisement, now_ms)?;
                        }
                        AdvertisementScan::AdvertisedFleet => {
                            advertisement.validate_shape()?;
                            advertisement.verify_signature()?;
                            if advertisement.fleet != self.fleet
                                || advertisement.issued_at_ms
                                    > now_ms.saturating_add(MAX_CLOCK_SKEW_MS)
                            {
                                return Err(Error::Node(
                                    "advertised node fleet or issue time differs",
                                ));
                            }
                        }
                    }
                    Ok(Some(*advertisement))
                }
            })
            .buffer_unordered(NODE_DIRECTORY_READ_CONCURRENCY);
        let mut advertisements = Vec::new();
        while let Some(advertisement) = records.next().await {
            let Some(advertisement) = advertisement? else {
                continue;
            };
            if advertisements.len() == limit {
                return Err(Error::Node(match scan {
                    AdvertisementScan::LiveRelease => "live node directory exceeds its limit",
                    AdvertisementScan::AdvertisedFleet => {
                        "advertised node session directory exceeds its limit"
                    }
                }));
            }
            advertisements.push(advertisement);
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
    ) -> Result<crate::peer::VerifiedPeerRequest> {
        let session = crate::peer::claimed_peer_session(input)?;
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
        crate::peer::PeerVerifier::new(
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
