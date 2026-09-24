//! Log authorization and lifecycle transitions for one node advertisement.
//!
//! A log epoch may only accept appends, retirements, or recovery from the
//! session that owns it, and every lifecycle transition is a CAS on the
//! signed advertisement, so these entry points share one validity window.

use super::*;

impl NodeDirectory {
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
}
