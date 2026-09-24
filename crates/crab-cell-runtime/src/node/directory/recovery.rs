//! Expired-claim fencing, takeover proofs, and recovery candidate windows.
//!
//! Every entry point here runs before a Cell moves: the directory must prove
//! the previous boot session is expired, that a takeover claim is signed by the
//! right node, and that the tail it hands over is object-covered.

use super::*;

impl NodeDirectory {
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
        self.claim_expired_inner(session, claimant, now_ms, false, false)
            .await
    }

    /// Claims an expired session only after the live claimant passes recovery
    /// admission immediately before the fencing CAS.
    ///
    /// Candidate discovery is advisory and may be stale by the time a
    /// scheduler reaches this boundary. The generic [`Self::claim_expired`]
    /// API intentionally remains usable by low-level recovery tests and
    /// callers that provide their own admission policy; production recovery
    /// uses this stricter entry point so a drained claimant cannot acquire a
    /// claim after its signed capacity has become ineligible.
    pub async fn claim_expired_for_recovery(
        &self,
        session: SessionId,
        claimant: SessionId,
        now_ms: i64,
    ) -> Result<FencedNodeSession> {
        self.claim_expired_inner(session, claimant, now_ms, false, true)
            .await
    }

    /// Claims an expired session for request-path takeover only when its node
    /// log is already inactive. An active log returns `PendingPublication`
    /// without writing a claim so the follower recovery scheduler can proceed.
    pub async fn claim_expired_for_takeover(
        &self,
        session: SessionId,
        claimant: SessionId,
        now_ms: i64,
    ) -> Result<NodeTakeoverProof> {
        self.claim_expired_inner(session, claimant, now_ms, true, false)
            .await?
            .direct_takeover()
    }

    pub(super) async fn claim_expired_inner(
        &self,
        session: SessionId,
        claimant: SessionId,
        now_ms: i64,
        reject_active_log: bool,
        require_recovery_eligibility: bool,
    ) -> Result<FencedNodeSession> {
        if now_ms < 0 || claimant.as_bytes().iter().all(|byte| *byte == 0) || claimant == session {
            return Err(Error::Node("node recovery time is invalid"));
        }
        let claimant_advertisement = self
            .load(claimant, now_ms)
            .await?
            .ok_or(Error::Node("node recovery claimant is not live"))?;
        if require_recovery_eligibility
            && !recovery_executor_eligible(claimant_advertisement.advertisement())
        {
            return Err(Error::Capacity("node recovery claimant is not eligible"));
        }
        let path = self.layout.node_path(session.as_bytes());
        let Some((record, token)) = self.load_record_at(&path).await? else {
            return Err(Error::Node("expired node session record is missing"));
        };
        let tombstone = match record {
            NodeRecord::Tombstone(tombstone) => {
                if tombstone.session != session {
                    return Err(Error::Node("node tombstone session differs"));
                }
                if reject_active_log && tombstone.log.as_ref().is_some_and(NodeLogStatus::active) {
                    return Err(Error::PendingPublication);
                }
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
                if reject_active_log
                    && advertisement
                        .log
                        .as_ref()
                        .is_some_and(NodeLogStatus::active)
                {
                    return Err(Error::PendingPublication);
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

    pub(super) async fn recovery_candidates_filtered(
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
        let snapshot = self
            .recovery_scan_snapshot(now_ms, claimant_node.is_some() || require_no_live_followers)
            .await?;
        let live_nodes = &snapshot.live_nodes;
        let mut candidates = RecoveryCandidateWindow::new(now_ms, limit)?;
        for record in &snapshot.records {
            if !record.eligible_for(claimant, now_ms) || record.session == claimant {
                continue;
            }
            if let Some(claimant_node) = claimant_node {
                let preferred = record
                    .members
                    .iter()
                    .filter(|member| live_nodes.contains(member))
                    .min_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
                if preferred != Some(&claimant_node) {
                    continue;
                }
            } else if require_no_live_followers
                && record
                    .members
                    .iter()
                    .any(|member| live_nodes.contains(member))
            {
                continue;
            }
            candidates.push(record.session);
        }
        Ok(candidates.finish())
    }

    pub(in crate::node) async fn recovery_scan_snapshot(
        &self,
        now_ms: i64,
        include_live_nodes: bool,
    ) -> Result<Arc<RecoveryScanSnapshot>> {
        if let Some(snapshot) = self.recovery_scan.read().await.as_ref()
            && now_ms >= snapshot.observed_at_ms
            && now_ms.saturating_sub(snapshot.observed_at_ms) < RECOVERY_SCAN_CACHE_TTL_MS
            && (!include_live_nodes || snapshot.includes_live_nodes)
        {
            return Ok(Arc::clone(snapshot));
        }

        let mut cached = self.recovery_scan.write().await;
        if let Some(snapshot) = cached.as_ref()
            && now_ms >= snapshot.observed_at_ms
            && now_ms.saturating_sub(snapshot.observed_at_ms) < RECOVERY_SCAN_CACHE_TTL_MS
            && (!include_live_nodes || snapshot.includes_live_nodes)
        {
            return Ok(Arc::clone(snapshot));
        }

        let prefix = self.layout.node_directory_path();
        let stream = self.layout.store().inner().list(Some(&prefix));
        let mut live_nodes = HashSet::new();
        let mut live_sessions = HashSet::new();
        let mut live_count = 0_usize;
        let mut records = stream
            .map(|item| {
                let prefix = prefix.clone();
                async move {
                    let meta =
                        item.map_err(|error| map_object_store_error(error, prefix.as_ref()))?;
                    let Some((record, _)) = self.load_record_at(&meta.location).await? else {
                        return Ok(None);
                    };
                    let session = record.session();
                    validate_record_path(&self.layout, session, &meta.location)?;
                    match record {
                        NodeRecord::Advertisement(advertisement) => {
                            self.validate_scope(&advertisement)?;
                            advertisement.validate_shape()?;
                            advertisement.verify_signature()?;
                            if advertisement.issued_at_ms > now_ms.saturating_add(MAX_CLOCK_SKEW_MS)
                            {
                                return Err(Error::Node("advertised node issue time differs"));
                            }
                            let live = if include_live_nodes && advertisement.expires_at_ms > now_ms
                            {
                                self.validate(&advertisement, now_ms)?;
                                Some((
                                    advertisement.node(),
                                    recovery_executor_eligible(&advertisement),
                                ))
                            } else {
                                None
                            };
                            let candidate =
                                advertisement
                                    .log
                                    .as_ref()
                                    .map(|log| RecoveryCandidateRecord {
                                        session,
                                        expires_at_ms: advertisement.expires_at_ms,
                                        claimant: None,
                                        claim_expires_at_ms: None,
                                        active: log.active(),
                                        phase: log.phase(),
                                        members: log.members().to_vec(),
                                    });
                            Ok(Some((candidate, live)))
                        }
                        NodeRecord::Tombstone(tombstone) => {
                            let candidate =
                                tombstone.log.as_ref().map(|log| RecoveryCandidateRecord {
                                    session,
                                    expires_at_ms: tombstone.expires_at_ms,
                                    claimant: tombstone.claimant,
                                    claim_expires_at_ms: tombstone.claim_expires_at_ms,
                                    active: log.active(),
                                    phase: log.phase(),
                                    members: log.members().to_vec(),
                                });
                            Ok(Some((candidate, None)))
                        }
                    }
                }
            })
            .buffer_unordered(NODE_DIRECTORY_READ_CONCURRENCY);
        let mut candidates = Vec::new();
        while let Some(record) = records.next().await {
            if let Some((record, live)) = record? {
                if let Some((node, eligible)) = live {
                    live_count = live_count.saturating_add(1);
                    if live_count > MAX_LIVE_NODE_RECORDS {
                        return Err(Error::Node("live node directory exceeds its limit"));
                    }
                    if !live_sessions.insert(node) {
                        return Err(Error::Node("multiple live sessions advertise one node"));
                    }
                    if eligible {
                        live_nodes.insert(node);
                    }
                }
                let Some(record) = record else {
                    continue;
                };
                if candidates.len() == MAX_LIVE_NODE_RECORDS {
                    return Err(Error::Node("node recovery directory exceeds its limit"));
                }
                candidates.push(record);
            }
        }
        let snapshot = Arc::new(RecoveryScanSnapshot {
            observed_at_ms: now_ms,
            includes_live_nodes: include_live_nodes,
            live_nodes,
            records: candidates,
        });
        *cached = Some(Arc::clone(&snapshot));
        Ok(snapshot)
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
}
