use crate::{Digest, Error, NodeId, Result, SessionId};

pub(crate) const RECOVERY_CLAIM_LIFETIME_MS: i64 = 30_000;
pub(crate) const MAX_NODE_LOG_MEMBERS: usize = 2;

/// Authoritative lifecycle state for one node-session durability log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeLogPhase {
    Open,
    Recovering,
    Sealed,
    Retired,
}

/// Bounded lease held by the live node session recovering a failed owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NodeRecoveryClaim {
    claimant: SessionId,
    generation: u64,
    expires_at_ms: i64,
}

impl NodeRecoveryClaim {
    #[must_use]
    pub const fn claimant(&self) -> SessionId {
        self.claimant
    }

    #[must_use]
    pub const fn generation(&self) -> u64 {
        self.generation
    }

    #[must_use]
    pub const fn expires_at_ms(&self) -> i64 {
        self.expires_at_ms
    }

    pub(crate) fn new(claimant: SessionId, generation: u64, now_ms: i64) -> Result<Self> {
        let expires_at_ms = now_ms
            .checked_add(RECOVERY_CLAIM_LIFETIME_MS)
            .ok_or(Error::Node("node recovery claim time overflow"))?;
        let claim = Self {
            claimant,
            generation,
            expires_at_ms,
        };
        claim.validate()?;
        Ok(claim)
    }

    pub(crate) fn from_parts(
        claimant: SessionId,
        generation: u64,
        expires_at_ms: i64,
    ) -> Result<Self> {
        let claim = Self {
            claimant,
            generation,
            expires_at_ms,
        };
        claim.validate()?;
        Ok(claim)
    }

    pub(crate) fn renew(self, claimant: SessionId, now_ms: i64) -> Result<Self> {
        if self.claimant != claimant || now_ms >= self.expires_at_ms {
            return Err(Error::Fenced);
        }
        let renewed = Self::new(claimant, self.generation, now_ms)?;
        if renewed.expires_at_ms <= self.expires_at_ms {
            return Err(Error::Node("node recovery claim expiry did not advance"));
        }
        Ok(renewed)
    }

    pub(crate) fn take_over(self, claimant: SessionId, now_ms: i64) -> Result<Self> {
        if now_ms < self.expires_at_ms {
            return Err(Error::Node("node recovery is already claimed"));
        }
        Self::new(
            claimant,
            self.generation
                .checked_add(1)
                .ok_or(Error::Node("node recovery claim generation overflow"))?,
            now_ms,
        )
    }

    pub(crate) fn validate(&self) -> Result<()> {
        if self.claimant.as_bytes().iter().all(|byte| *byte == 0)
            || self.generation == 0
            || self.expires_at_ms < 0
        {
            return Err(Error::Node("node recovery claim is invalid"));
        }
        Ok(())
    }
}

/// CAS-protected follower ensemble and object-coverage watermark.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeLogStatus {
    phase: NodeLogPhase,
    epoch: u64,
    members: Vec<NodeId>,
    active: bool,
    tiered_through: u64,
    recovery: Option<NodeRecoveryClaim>,
    recovery_manifest: Option<Digest>,
}

impl NodeLogStatus {
    pub(crate) fn open(leader: NodeId, epoch: u64, members: Vec<NodeId>) -> Result<Self> {
        let status = Self {
            phase: NodeLogPhase::Open,
            epoch,
            members,
            active: false,
            tiered_through: 0,
            recovery: None,
            recovery_manifest: None,
        };
        status.validate(leader)?;
        Ok(status)
    }

    pub(crate) fn from_parts(
        leader: NodeId,
        phase: NodeLogPhase,
        epoch: u64,
        members: Vec<NodeId>,
        active: bool,
        tiered_through: u64,
        recovery: Option<NodeRecoveryClaim>,
        recovery_manifest: Option<Digest>,
    ) -> Result<Self> {
        let status = Self {
            phase,
            epoch,
            members,
            active,
            tiered_through,
            recovery,
            recovery_manifest,
        };
        status.validate(leader)?;
        Ok(status)
    }

    #[must_use]
    pub const fn phase(&self) -> NodeLogPhase {
        self.phase
    }

    #[must_use]
    pub const fn epoch(&self) -> u64 {
        self.epoch
    }

    #[must_use]
    pub fn members(&self) -> &[NodeId] {
        &self.members
    }

    #[must_use]
    pub const fn active(&self) -> bool {
        self.active
    }

    #[must_use]
    pub const fn tiered_through(&self) -> u64 {
        self.tiered_through
    }

    #[must_use]
    pub const fn recovery(&self) -> Option<NodeRecoveryClaim> {
        self.recovery
    }

    #[must_use]
    pub const fn recovery_manifest(&self) -> Option<Digest> {
        self.recovery_manifest
    }

    pub(crate) fn activate(&self, leader: NodeId) -> Result<Self> {
        if self.phase != NodeLogPhase::Open || self.active {
            return Err(Error::Node("node log cannot be activated"));
        }
        Self::from_parts(
            leader,
            self.phase,
            self.epoch,
            self.members.clone(),
            true,
            self.tiered_through,
            None,
            None,
        )
    }

    pub(crate) fn advance_tiered(&self, leader: NodeId, through: u64) -> Result<Self> {
        if self.phase != NodeLogPhase::Open || through < self.tiered_through {
            return Err(Error::Node("node log object coverage regressed"));
        }
        Self::from_parts(
            leader,
            self.phase,
            self.epoch,
            self.members.clone(),
            self.active,
            through,
            None,
            None,
        )
    }

    pub(crate) fn begin_recovery(
        &self,
        leader: NodeId,
        claimant: SessionId,
        now_ms: i64,
    ) -> Result<Self> {
        match (self.phase, self.recovery) {
            (NodeLogPhase::Open, None) => Self::from_parts(
                leader,
                NodeLogPhase::Recovering,
                self.epoch,
                self.members.clone(),
                self.active,
                self.tiered_through,
                Some(NodeRecoveryClaim::new(claimant, 1, now_ms)?),
                None,
            ),
            (NodeLogPhase::Recovering, Some(current)) if current.claimant == claimant => {
                if now_ms < current.expires_at_ms {
                    return Ok(self.clone());
                }
                Self::from_parts(
                    leader,
                    NodeLogPhase::Recovering,
                    self.epoch,
                    self.members.clone(),
                    self.active,
                    self.tiered_through,
                    Some(current.take_over(claimant, now_ms)?),
                    None,
                )
            }
            (NodeLogPhase::Recovering, Some(current)) => Self::from_parts(
                leader,
                NodeLogPhase::Recovering,
                self.epoch,
                self.members.clone(),
                self.active,
                self.tiered_through,
                Some(current.take_over(claimant, now_ms)?),
                None,
            ),
            _ => Err(Error::Node("node log cannot enter recovery")),
        }
    }

    pub(crate) fn renew_recovery(
        &self,
        leader: NodeId,
        claimant: SessionId,
        generation: u64,
        now_ms: i64,
    ) -> Result<Self> {
        let current = self
            .recovery
            .filter(|claim| claim.generation == generation)
            .ok_or(Error::Fenced)?;
        Self::from_parts(
            leader,
            NodeLogPhase::Recovering,
            self.epoch,
            self.members.clone(),
            self.active,
            self.tiered_through,
            Some(current.renew(claimant, now_ms)?),
            None,
        )
    }

    pub(crate) fn seal_recovery(
        &self,
        leader: NodeId,
        claimant: SessionId,
        generation: u64,
        manifest: Option<Digest>,
    ) -> Result<Self> {
        let current = self.recovery.ok_or(Error::Fenced)?;
        if self.phase != NodeLogPhase::Recovering
            || current.claimant != claimant
            || current.generation != generation
        {
            return Err(Error::Fenced);
        }
        Self::from_parts(
            leader,
            NodeLogPhase::Sealed,
            self.epoch,
            self.members.clone(),
            self.active,
            self.tiered_through,
            None,
            manifest,
        )
    }

    pub(crate) fn permits_append(&self, leader: NodeId, member: NodeId, epoch: u64) -> Result<()> {
        self.validate(leader)?;
        if self.phase != NodeLogPhase::Open
            || self.epoch != epoch
            || !self.members.contains(&member)
        {
            return Err(Error::PeerAuthorization(
                "node-log append is outside the enrolled ensemble",
            ));
        }
        Ok(())
    }

    pub(crate) fn permits_recovery_read(
        &self,
        leader: NodeId,
        claimant: SessionId,
        member: NodeId,
        epoch: u64,
        now_ms: i64,
    ) -> Result<()> {
        self.validate(leader)?;
        let claim = self
            .recovery
            .ok_or(Error::PeerAuthorization("node-log recovery is not claimed"))?;
        if self.phase != NodeLogPhase::Recovering
            || self.epoch != epoch
            || claim.claimant != claimant
            || claim.expires_at_ms <= now_ms
            || !self.members.contains(&member)
        {
            return Err(Error::PeerAuthorization(
                "node-log recovery claim or member differs",
            ));
        }
        Ok(())
    }

    pub(crate) fn validate(&self, leader: NodeId) -> Result<()> {
        if self.epoch == 0
            || self.members.is_empty()
            || self.members.len() > MAX_NODE_LOG_MEMBERS
            || self.members.contains(&leader)
            || self
                .members
                .iter()
                .any(|member| member.as_bytes().iter().all(|byte| *byte == 0))
            || !self
                .members
                .windows(2)
                .all(|pair| pair[0].as_bytes() < pair[1].as_bytes())
        {
            return Err(Error::Node("node log ensemble is invalid"));
        }
        match (self.phase, self.recovery, self.recovery_manifest) {
            (NodeLogPhase::Open, None, None) => {}
            (NodeLogPhase::Recovering, Some(claim), None) => claim.validate()?,
            (NodeLogPhase::Sealed, None, manifest) | (NodeLogPhase::Retired, None, manifest) => {
                if manifest.is_some_and(|digest| digest.as_bytes().iter().all(|byte| *byte == 0)) {
                    return Err(Error::Node("node recovery manifest digest is zero"));
                }
            }
            _ => return Err(Error::Node("node log state is invalid")),
        }
        Ok(())
    }
}
