//! Cell control record, transitions, and authority CAS ownership.

pub mod authority;

mod codec;

#[cfg(test)]
mod tests;

use serde::{Deserialize, Serialize};

use crate::identity::IncarnationId;
use crate::identity::{CellId, Digest, SessionId};
use crate::{Error, Result};

const MAX_CONTROL_BYTES: usize = 8 * 1024;
const CHECKSUM_FLAG: u64 = 1 << 63;

/// Exact immutable recovery root published by the current Cell control record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RootRef {
    pub digest: Digest,
    pub txid: u64,
    pub checksum: u64,
    pub commit_sequence: u64,
}

impl RootRef {
    /// Narrows a Cell-scoped LTX reference to the fields persisted in control JSON.
    pub fn from_ltx(
        cell: CellId,
        incarnation: IncarnationId,
        root: crab_ltx::RootRef,
    ) -> Result<Self> {
        if root.cell != *cell.as_bytes() || root.incarnation != *incarnation.as_bytes() {
            return Err(Error::Control("prepared root changed Cell scope"));
        }
        Ok(Self {
            digest: Digest::from_bytes(root.digest),
            txid: root.position.txid,
            checksum: root.position.checksum,
            commit_sequence: root.commit_sequence,
        })
    }

    /// Restores the typed Cell/incarnation scope inherited from its control record.
    #[must_use]
    pub fn to_ltx(&self, cell: CellId, incarnation: IncarnationId) -> crab_ltx::RootRef {
        crab_ltx::RootRef {
            cell: *cell.as_bytes(),
            incarnation: *incarnation.as_bytes(),
            digest: *self.digest.as_bytes(),
            position: crab_ltx::Position {
                txid: self.txid,
                checksum: self.checksum,
            },
            commit_sequence: self.commit_sequence,
        }
    }
}

/// Enrolled process currently responsible for one Cell.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Owner {
    pub session: SessionId,
    pub endpoint: String,
}

/// Exact recovered follower tail pinned before a dead owner's Cell can move.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecoveryOverlayRef {
    pub leader_session: SessionId,
    pub log_epoch: u64,
    pub manifest_digest: Digest,
    pub first_node_sequence: u64,
    pub last_node_sequence: u64,
    pub predecessor: RootRef,
    pub final_txid: u64,
    pub final_checksum: u64,
    pub final_commit_sequence: u64,
}

impl RecoveryOverlayRef {
    fn validate(&self) -> Result<()> {
        if self.leader_session.as_bytes().iter().all(|byte| *byte == 0)
            || self
                .manifest_digest
                .as_bytes()
                .iter()
                .all(|byte| *byte == 0)
            || self.log_epoch == 0
            || self.first_node_sequence == 0
            || self.first_node_sequence > self.last_node_sequence
            || self.final_txid <= self.predecessor.txid
            || self.final_checksum & CHECKSUM_FLAG == 0
            || self.final_commit_sequence <= self.predecessor.commit_sequence
            || self.final_commit_sequence > i64::MAX as u64
        {
            return Err(Error::Control("invalid recovery overlay"));
        }
        Ok(())
    }
}

/// Durable Cell lifecycle state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ControlState {
    Recovering,
    Serving,
    Idle,
    Tombstoned,
}

/// Strict, versioned owner/root authority stored with an object-store ETag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Control {
    pub cell: CellId,
    pub incarnation: IncarnationId,
    pub epoch: u64,
    pub revision: u64,
    pub progress: u64,
    pub state: ControlState,
    pub owner: Option<Owner>,
    pub root: Option<RootRef>,
    pub recovery: Option<RecoveryOverlayRef>,
    pub code: Digest,
    pub schema: u32,
    pub next_due_ms: Option<i64>,
}

/// Named transition whose complete predicate must pass before an ETag update.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transition {
    Renew,
    Activate,
    Publish,
    Migrate,
    Release,
    AttachRecovery,
    PublishRecovery,
    Takeover,
    Tombstone,
}

impl Control {
    /// Creates and validates the only legal first control record.
    pub fn initial(
        cell: CellId,
        incarnation: IncarnationId,
        owner: Owner,
        code: Digest,
        schema: u32,
    ) -> Result<Self> {
        let control = Self {
            cell,
            incarnation,
            epoch: 1,
            revision: 1,
            progress: 1,
            state: ControlState::Recovering,
            owner: Some(owner),
            root: None,
            recovery: None,
            code,
            schema,
            next_due_ms: None,
        };
        control.validate()?;
        Ok(control)
    }

    /// Encodes canonical compact JSON after validating the complete record.
    pub fn encode(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let bytes = serde_json::to_vec(&codec::RawControl::from(self))?;
        if bytes.len() > MAX_CONTROL_BYTES {
            return Err(Error::Control("body exceeds 8 KiB"));
        }
        Ok(bytes)
    }

    /// Decodes strict JSON, rejecting duplicate/unknown fields and noncanonical numbers.
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_CONTROL_BYTES {
            return Err(Error::Control("body exceeds 8 KiB"));
        }
        let raw: codec::RawControl = serde_json::from_slice(bytes)?;
        let control = Self::try_from(raw)?;
        control.validate()?;
        if control.encode()? != bytes {
            return Err(Error::Control("JSON is not canonical"));
        }
        Ok(control)
    }

    /// Reattaches this control record's Cell scope to its immutable LTX root.
    #[must_use]
    pub fn ltx_root(&self) -> Option<crab_ltx::RootRef> {
        self.root
            .as_ref()
            .map(|root| root.to_ltx(self.cell, self.incarnation))
    }

    pub(crate) fn is_same_or_pure_renewal_of(&self, previous: &Self) -> bool {
        let revisions = self.revision.checked_sub(previous.revision);
        let progress = self.progress.checked_sub(previous.progress);
        revisions.is_some()
            && revisions == progress
            && previous.cell == self.cell
            && previous.incarnation == self.incarnation
            && previous.epoch == self.epoch
            && previous.state == self.state
            && previous.owner == self.owner
            && previous.root == self.root
            && previous.recovery == self.recovery
            && previous.code == self.code
            && previous.schema == self.schema
            && previous.next_due_ms == self.next_due_ms
    }

    /// Builds the sole valid successor that proves this owner is still live.
    pub(crate) fn renew(&self) -> Result<Self> {
        let mut next = self.clone();
        next.revision = next
            .revision
            .checked_add(1)
            .ok_or(Error::Control("revision overflow"))?;
        next.progress = next
            .progress
            .checked_add(1)
            .ok_or(Error::Control("progress overflow"))?;
        self.validate_transition(&next, Transition::Renew)?;
        Ok(next)
    }

    // Recovery becomes externally serving only after the restored root is verified.
    pub(crate) fn activate(&self) -> Result<Self> {
        let mut next = self.clone();
        next.revision = next
            .revision
            .checked_add(1)
            .ok_or(Error::Control("revision overflow"))?;
        next.progress = next
            .progress
            .checked_add(1)
            .ok_or(Error::Control("progress overflow"))?;
        next.state = ControlState::Serving;
        self.validate_transition(&next, Transition::Activate)?;
        Ok(next)
    }

    /// Builds a recovering successor owned by a different enrolled session.
    pub fn takeover(&self, owner: Owner) -> Result<Self> {
        let mut next = self.clone();
        next.epoch = next
            .epoch
            .checked_add(1)
            .ok_or(Error::Control("epoch overflow"))?;
        next.revision = next
            .revision
            .checked_add(1)
            .ok_or(Error::Control("revision overflow"))?;
        next.progress = next
            .progress
            .checked_add(1)
            .ok_or(Error::Control("progress overflow"))?;
        next.state = ControlState::Recovering;
        next.owner = Some(owner);
        self.validate_transition(&next, Transition::Takeover)?;
        Ok(next)
    }

    /// Pins an exact follower-recovered tail while retaining the dead owner.
    pub fn attach_recovery(&self, recovery: RecoveryOverlayRef) -> Result<Self> {
        let mut next = self.clone();
        next.revision = next
            .revision
            .checked_add(1)
            .ok_or(Error::Control("revision overflow"))?;
        next.progress = next
            .progress
            .checked_add(1)
            .ok_or(Error::Control("progress overflow"))?;
        next.recovery = Some(recovery);
        self.validate_transition(&next, Transition::AttachRecovery)?;
        Ok(next)
    }

    /// Publishes the root materialized from the currently pinned recovery tail.
    pub fn publish_recovery(
        &self,
        prepared: &crab_ltx::PreparedRoot,
        next_due_ms: Option<i64>,
    ) -> Result<Self> {
        let recovery = self
            .recovery
            .as_ref()
            .ok_or(Error::Control("recovery overlay is not pinned"))?;
        let expected = recovery.predecessor.to_ltx(self.cell, self.incarnation);
        let root = prepared.root();
        if prepared.predecessor() != Some(expected)
            || prepared.verified().schema() != self.schema
            || root.position.txid != recovery.final_txid
            || root.position.checksum != recovery.final_checksum
            || root.commit_sequence != recovery.final_commit_sequence
        {
            return Err(Error::Control(
                "prepared recovery does not match pinned overlay",
            ));
        }
        let mut next = self.clone();
        next.revision = next
            .revision
            .checked_add(1)
            .ok_or(Error::Control("revision overflow"))?;
        next.progress = next
            .progress
            .checked_add(1)
            .ok_or(Error::Control("progress overflow"))?;
        next.root = Some(RootRef::from_ltx(self.cell, self.incarnation, root)?);
        next.recovery = None;
        next.next_due_ms = next_due_ms;
        self.validate_transition(&next, Transition::PublishRecovery)?;
        Ok(next)
    }

    /// Builds the sole valid publication successor for an uploaded root proposal.
    pub fn publish_prepared(
        &self,
        prepared: &crab_ltx::PreparedRoot,
        next_due_ms: Option<i64>,
    ) -> Result<Self> {
        if prepared.predecessor() != self.ltx_root() || prepared.verified().schema() != self.schema
        {
            return Err(Error::Control("prepared root does not continue control"));
        }
        let mut next = self.clone();
        next.revision = next
            .revision
            .checked_add(1)
            .ok_or(Error::Control("revision overflow"))?;
        next.progress = next
            .progress
            .checked_add(1)
            .ok_or(Error::Control("progress overflow"))?;
        next.state = ControlState::Serving;
        next.root = Some(RootRef::from_ltx(
            self.cell,
            self.incarnation,
            prepared.root(),
        )?);
        next.next_due_ms = next_due_ms;
        self.validate_transition(&next, Transition::Publish)?;
        Ok(next)
    }

    /// Builds the sole valid successor for one published schema migration.
    pub fn migrate_prepared(
        &self,
        prepared: &crab_ltx::PreparedRoot,
        next_due_ms: Option<i64>,
        code: Digest,
        schema: u32,
    ) -> Result<Self> {
        if prepared.predecessor() != self.ltx_root()
            || prepared.verified().schema() != schema
            || code.as_bytes().iter().all(|byte| *byte == 0)
            || !valid_migration_version(self.code, self.schema, code, schema)
        {
            return Err(Error::Control(
                "prepared migration does not continue control",
            ));
        }
        let mut next = self.clone();
        next.revision = next
            .revision
            .checked_add(1)
            .ok_or(Error::Control("revision overflow"))?;
        next.progress = next
            .progress
            .checked_add(1)
            .ok_or(Error::Control("progress overflow"))?;
        next.state = ControlState::Serving;
        next.root = Some(RootRef::from_ltx(
            self.cell,
            self.incarnation,
            prepared.root(),
        )?);
        next.code = code;
        next.schema = schema;
        next.next_due_ms = next_due_ms;
        self.validate_transition(&next, Transition::Migrate)?;
        Ok(next)
    }

    /// Builds the sole valid successor that releases a drained Cell owner.
    pub(crate) fn release(&self) -> Result<Self> {
        let mut next = self.clone();
        next.revision = next
            .revision
            .checked_add(1)
            .ok_or(Error::Control("revision overflow"))?;
        next.progress = next
            .progress
            .checked_add(1)
            .ok_or(Error::Control("progress overflow"))?;
        next.state = ControlState::Idle;
        next.owner = None;
        self.validate_transition(&next, Transition::Release)?;
        Ok(next)
    }

    /// Validates a named successor before its conditional object-store update.
    pub fn validate_transition(&self, next: &Self, transition: Transition) -> Result<()> {
        self.validate()?;
        next.validate()?;
        if self.cell != next.cell || self.incarnation != next.incarnation {
            return Err(Error::Control("successor changed Cell scope"));
        }
        if self.revision.checked_add(1) != Some(next.revision)
            || self.progress.checked_add(1) != Some(next.progress)
        {
            return Err(Error::Control("successor revision or progress"));
        }
        match transition {
            Transition::Renew => {
                if self.epoch != next.epoch
                    || self.state != next.state
                    || self.owner != next.owner
                    || self.root != next.root
                    || self.recovery != next.recovery
                    || self.code != next.code
                    || self.schema != next.schema
                    || self.next_due_ms != next.next_due_ms
                {
                    return Err(Error::Control("renew changed protected fields"));
                }
            }
            Transition::Activate => {
                if self.state != ControlState::Recovering
                    || next.state != ControlState::Serving
                    || self.epoch != next.epoch
                    || self.owner != next.owner
                    || self.root.is_none()
                    || self.root != next.root
                    || self.recovery.is_some()
                    || next.recovery.is_some()
                    || self.code != next.code
                    || self.schema != next.schema
                    || self.next_due_ms != next.next_due_ms
                {
                    return Err(Error::Control("invalid activation transition"));
                }
            }
            Transition::Publish => {
                if !matches!(self.state, ControlState::Recovering | ControlState::Serving)
                    || next.state != ControlState::Serving
                    || self.epoch != next.epoch
                    || self.owner != next.owner
                    || next.root.is_none()
                    || self.recovery.is_some()
                    || next.recovery.is_some()
                    || self.code != next.code
                    || self.schema != next.schema
                    || !valid_root_successor(self.root.as_ref(), next.root.as_ref())
                {
                    return Err(Error::Control("invalid publish transition"));
                }
            }
            Transition::Migrate => {
                if !matches!(self.state, ControlState::Recovering | ControlState::Serving)
                    || next.state != ControlState::Serving
                    || self.epoch != next.epoch
                    || self.owner != next.owner
                    || next.root.is_none()
                    || self.recovery.is_some()
                    || next.recovery.is_some()
                    || next.code.as_bytes().iter().all(|byte| *byte == 0)
                    || !valid_migration_version(self.code, self.schema, next.code, next.schema)
                    || !valid_root_successor(self.root.as_ref(), next.root.as_ref())
                {
                    return Err(Error::Control("invalid migration transition"));
                }
            }
            Transition::Release => {
                if !matches!(self.state, ControlState::Recovering | ControlState::Serving)
                    || next.state != ControlState::Idle
                    || next.owner.is_some()
                    || next.root.is_none()
                    || self.recovery.is_some()
                    || next.recovery.is_some()
                    || self.epoch != next.epoch
                    || self.root != next.root
                    || self.recovery != next.recovery
                    || self.code != next.code
                    || self.schema != next.schema
                    || self.next_due_ms != next.next_due_ms
                {
                    return Err(Error::Control("invalid release transition"));
                }
            }
            Transition::AttachRecovery => {
                let Some(recovery) = next.recovery.as_ref() else {
                    return Err(Error::Control("invalid recovery attachment"));
                };
                if self.state == ControlState::Tombstoned
                    || self.recovery.is_some()
                    || self.epoch != next.epoch
                    || self.state != next.state
                    || self.owner.as_ref().map(|owner| owner.session)
                        != Some(recovery.leader_session)
                    || self.owner != next.owner
                    || self.root.as_ref() != Some(&recovery.predecessor)
                    || self.root != next.root
                    || self.code != next.code
                    || self.schema != next.schema
                    || self.next_due_ms != next.next_due_ms
                {
                    return Err(Error::Control("invalid recovery attachment"));
                }
            }
            Transition::PublishRecovery => {
                let Some(recovery) = self.recovery.as_ref() else {
                    return Err(Error::Control("invalid recovery publication"));
                };
                if self.state != ControlState::Recovering
                    || next.state != ControlState::Recovering
                    || next.recovery.is_some()
                    || self.epoch != next.epoch
                    || self.owner != next.owner
                    || self.root.as_ref() != Some(&recovery.predecessor)
                    || next.root.as_ref().is_none_or(|root| {
                        root.txid != recovery.final_txid
                            || root.checksum != recovery.final_checksum
                            || root.commit_sequence != recovery.final_commit_sequence
                    })
                    || self.code != next.code
                    || self.schema != next.schema
                {
                    return Err(Error::Control("invalid recovery publication"));
                }
            }
            Transition::Takeover => {
                if self.state == ControlState::Tombstoned
                    || next.state != ControlState::Recovering
                    || next.owner.is_none()
                    || self.owner == next.owner
                    || self.epoch.checked_add(1) != Some(next.epoch)
                    || self.root != next.root
                    || self.recovery != next.recovery
                    || self.code != next.code
                    || self.schema != next.schema
                    || self.next_due_ms != next.next_due_ms
                {
                    return Err(Error::Control("invalid takeover transition"));
                }
            }
            Transition::Tombstone => {
                if self.state == ControlState::Tombstoned
                    || next.state != ControlState::Tombstoned
                    || next.owner.is_some()
                    || self.epoch.checked_add(1) != Some(next.epoch)
                    || self.root != next.root
                    || self.recovery.is_some()
                    || next.recovery.is_some()
                    || self.code != next.code
                    || self.schema != next.schema
                    || self.next_due_ms != next.next_due_ms
                {
                    return Err(Error::Control("invalid tombstone transition"));
                }
            }
        }
        Ok(())
    }

    fn validate(&self) -> Result<()> {
        if self.epoch == 0 || self.revision == 0 || self.schema == 0 {
            return Err(Error::Control("zero epoch, revision, or schema"));
        }
        if self.next_due_ms.is_some_and(|value| value < 0) {
            return Err(Error::Control("negative next due time"));
        }
        if let Some(owner) = &self.owner
            && (owner.endpoint.is_empty()
                || owner.endpoint.len() > 512
                || owner.endpoint.chars().any(char::is_control))
        {
            return Err(Error::Control("invalid owner endpoint"));
        }
        if let Some(root) = &self.root
            && (root.txid == 0
                || root.checksum & CHECKSUM_FLAG == 0
                || root.commit_sequence > i64::MAX as u64)
        {
            return Err(Error::Control("invalid recovery root"));
        }
        if let Some(recovery) = &self.recovery {
            recovery.validate()?;
            if self.root.as_ref() != Some(&recovery.predecessor) {
                return Err(Error::Control("recovery overlay does not match control"));
            }
        }
        match self.state {
            ControlState::Recovering if self.owner.is_some() => {}
            ControlState::Serving if self.owner.is_some() && self.root.is_some() => {}
            ControlState::Idle | ControlState::Tombstoned if self.owner.is_none() => {}
            _ => return Err(Error::Control("owner/root do not match state")),
        }
        Ok(())
    }
}

fn valid_root_successor(previous: Option<&RootRef>, next: Option<&RootRef>) -> bool {
    let Some(next) = next else {
        return false;
    };
    let Some(previous) = previous else {
        return true;
    };
    if next.txid < previous.txid || next.commit_sequence < previous.commit_sequence {
        return false;
    }
    next.txid != previous.txid
        || (next.checksum == previous.checksum && next.commit_sequence == previous.commit_sequence)
}

fn valid_migration_version(
    current_code: Digest,
    current_schema: u32,
    next_code: Digest,
    next_schema: u32,
) -> bool {
    current_schema.checked_add(1) == Some(next_schema)
        || (current_schema == next_schema && current_code != next_code)
}
