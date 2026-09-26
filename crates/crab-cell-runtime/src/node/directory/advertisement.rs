//! Advertisement scans, liveness, placement inputs, and refreshes.
//!
//! Every entry point here reads or writes the signed advertisement records a
//! node publishes, so they share the scan bounds and fail-closed rules the
//! maintenance loop relies on.

use super::*;

impl NodeDirectory {
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
        let request = crate::peer::UnverifiedPeerRequest::decode(input)?;
        self.peer_verifier(
            request.session(),
            certificate,
            certificate_public_key,
            now_ms,
        )
        .await?
        .verify(request, now_ms)
    }

    /// Loads a live enrollment and binds its verifier to the mTLS identity.
    ///
    /// The returned verifier rechecks enrollment expiry when verification runs,
    /// allowing callers to release CPU admission during this provider read.
    pub async fn peer_verifier(
        &self,
        session: SessionId,
        certificate: Digest,
        certificate_public_key: [u8; 32],
        now_ms: i64,
    ) -> Result<EnrolledPeerVerifier> {
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
        let verifier = crate::peer::PeerVerifier::new(
            session,
            self.release,
            enrolled.advertisement.verifying_key()?,
        );
        Ok(EnrolledPeerVerifier {
            advertisement: enrolled.advertisement,
            verifier,
        })
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
}
