use serde::{Deserialize, Serialize};

use crate::identity::{decode_hex, encode_hex};
use crate::{CellId, Digest, Error, IncarnationId, Result, SessionId};

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
    pub code: Digest,
    pub schema: u32,
    pub next_due_ms: Option<i64>,
}

/// Named transition whose complete predicate must pass before an ETag update.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transition {
    Renew,
    Publish,
    Release,
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
        let bytes = serde_json::to_vec(&RawControl::from(self))?;
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
        let raw: RawControl = serde_json::from_slice(bytes)?;
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
                    || self.code != next.code
                    || self.schema != next.schema
                    || self.next_due_ms != next.next_due_ms
                {
                    return Err(Error::Control("renew changed protected fields"));
                }
            }
            Transition::Publish => {
                if !matches!(self.state, ControlState::Recovering | ControlState::Serving)
                    || next.state != ControlState::Serving
                    || self.epoch != next.epoch
                    || self.owner != next.owner
                    || next.root.is_none()
                    || !valid_root_successor(self.root.as_ref(), next.root.as_ref())
                {
                    return Err(Error::Control("invalid publish transition"));
                }
            }
            Transition::Release => {
                if !matches!(self.state, ControlState::Recovering | ControlState::Serving)
                    || next.state != ControlState::Idle
                    || next.owner.is_some()
                    || next.root.is_none()
                    || self.epoch != next.epoch
                    || self.root != next.root
                    || self.code != next.code
                    || self.schema != next.schema
                    || self.next_due_ms != next.next_due_ms
                {
                    return Err(Error::Control("invalid release transition"));
                }
            }
            Transition::Takeover => {
                if self.state == ControlState::Tombstoned
                    || next.state != ControlState::Recovering
                    || next.owner.is_none()
                    || self.owner == next.owner
                    || self.epoch.checked_add(1) != Some(next.epoch)
                    || self.root != next.root
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

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawControl {
    version: u32,
    cell: String,
    incarnation: String,
    epoch: String,
    revision: String,
    progress: String,
    state: ControlState,
    owner: Option<RawOwner>,
    root: Option<RawRoot>,
    code: String,
    schema: u32,
    next_due_ms: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOwner {
    session: String,
    endpoint: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRoot {
    digest: String,
    txid: String,
    checksum: String,
    commit_sequence: String,
}

impl From<&Control> for RawControl {
    fn from(control: &Control) -> Self {
        Self {
            version: 1,
            cell: encode_hex(control.cell.as_bytes()),
            incarnation: encode_hex(control.incarnation.as_bytes()),
            epoch: control.epoch.to_string(),
            revision: control.revision.to_string(),
            progress: control.progress.to_string(),
            state: control.state,
            owner: control.owner.as_ref().map(|owner| RawOwner {
                session: encode_hex(owner.session.as_bytes()),
                endpoint: owner.endpoint.clone(),
            }),
            root: control.root.as_ref().map(|root| RawRoot {
                digest: encode_hex(root.digest.as_bytes()),
                txid: root.txid.to_string(),
                checksum: encode_hex(&root.checksum.to_be_bytes()),
                commit_sequence: root.commit_sequence.to_string(),
            }),
            code: encode_hex(control.code.as_bytes()),
            schema: control.schema,
            next_due_ms: control.next_due_ms.map(|value| value.to_string()),
        }
    }
}

impl TryFrom<RawControl> for Control {
    type Error = Error;

    fn try_from(raw: RawControl) -> Result<Self> {
        if raw.version != 1 {
            return Err(Error::Control("unsupported version"));
        }
        Ok(Self {
            cell: CellId::from_bytes(decode_hex(&raw.cell)?),
            incarnation: IncarnationId::from_bytes(decode_hex(&raw.incarnation)?),
            epoch: decimal_u64(&raw.epoch)?,
            revision: decimal_u64(&raw.revision)?,
            progress: decimal_u64(&raw.progress)?,
            state: raw.state,
            owner: raw
                .owner
                .map(|owner| {
                    Ok::<Owner, Error>(Owner {
                        session: SessionId::from_bytes(decode_hex(&owner.session)?),
                        endpoint: owner.endpoint,
                    })
                })
                .transpose()?,
            root: raw
                .root
                .map(|root| {
                    Ok::<RootRef, Error>(RootRef {
                        digest: Digest::from_bytes(decode_hex(&root.digest)?),
                        txid: decimal_u64(&root.txid)?,
                        checksum: u64::from_be_bytes(decode_hex(&root.checksum)?),
                        commit_sequence: decimal_u64(&root.commit_sequence)?,
                    })
                })
                .transpose()?,
            code: Digest::from_bytes(decode_hex(&raw.code)?),
            schema: raw.schema,
            next_due_ms: raw
                .next_due_ms
                .map(|value| decimal_i64(&value))
                .transpose()?,
        })
    }
}

fn decimal_u64(value: &str) -> Result<u64> {
    let number = value
        .parse::<u64>()
        .map_err(|_| Error::Control("invalid decimal u64"))?;
    if number.to_string() != value {
        return Err(Error::Control("noncanonical decimal u64"));
    }
    Ok(number)
}

fn decimal_i64(value: &str) -> Result<i64> {
    let number = value
        .parse::<i64>()
        .map_err(|_| Error::Control("invalid decimal i64"))?;
    if number.to_string() != value {
        return Err(Error::Control("noncanonical decimal i64"));
    }
    Ok(number)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner(byte: u8) -> Owner {
        Owner {
            session: SessionId::from_bytes([byte; 16]),
            endpoint: format!("https://node-{byte}.internal:8081"),
        }
    }

    fn root(sequence: u64) -> RootRef {
        RootRef {
            digest: Digest::from_bytes([9; 32]),
            txid: sequence,
            checksum: CHECKSUM_FLAG | sequence,
            commit_sequence: sequence,
        }
    }

    fn initial() -> Control {
        Control::initial(
            CellId::from_bytes([1; 32]),
            IncarnationId::from_bytes([2; 16]),
            owner(3),
            Digest::from_bytes([4; 32]),
            1,
        )
        .unwrap()
    }

    #[test]
    fn canonical_control_roundtrips_and_rejects_alternate_encodings() {
        let mut control = initial();
        control.root = Some(root(7));
        control.state = ControlState::Serving;
        let bytes = control.encode().unwrap();
        assert_eq!(Control::decode(&bytes).unwrap(), control);

        let text = String::from_utf8(bytes).unwrap();
        assert!(
            Control::decode(
                text.replace("\"epoch\":\"1\"", "\"epoch\":\"01\"")
                    .as_bytes()
            )
            .is_err()
        );
        assert!(
            Control::decode(
                text.replace("\"schema\":1", "\"schema\":1,\"schema\":1")
                    .as_bytes()
            )
            .is_err()
        );
        assert!(Control::decode(format!("{{\"unknown\":1,{}}}", &text[1..]).as_bytes()).is_err());
    }

    #[test]
    fn transitions_protect_scope_owner_and_published_root() {
        let recovering = initial();
        let mut serving = recovering.clone();
        serving.state = ControlState::Serving;
        serving.root = Some(root(1));
        serving.revision += 1;
        serving.progress += 1;
        recovering
            .validate_transition(&serving, Transition::Publish)
            .unwrap();

        let mut renewed = serving.clone();
        renewed.revision += 1;
        renewed.progress += 1;
        serving
            .validate_transition(&renewed, Transition::Renew)
            .unwrap();

        let mut takeover = renewed.clone();
        takeover.state = ControlState::Recovering;
        takeover.owner = Some(owner(5));
        takeover.epoch += 1;
        takeover.revision += 1;
        takeover.progress += 1;
        renewed
            .validate_transition(&takeover, Transition::Takeover)
            .unwrap();

        let mut corrupted_compaction = takeover.clone();
        corrupted_compaction.state = ControlState::Serving;
        corrupted_compaction.root.as_mut().unwrap().checksum ^= 1;
        corrupted_compaction.revision += 1;
        corrupted_compaction.progress += 1;
        assert!(
            takeover
                .validate_transition(&corrupted_compaction, Transition::Publish)
                .is_err()
        );
    }
}
