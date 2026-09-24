//! Canonical JSON codec for the Cell control record.

use serde::{Deserialize, Serialize};

use super::{Control, ControlState, Owner, RecoveryOverlayRef, RootRef};
use crate::identity::{CellId, Digest, IncarnationId, SessionId, decode_hex, encode_hex};
use crate::{Error, Result};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawControl {
    version: u32,
    cell: String,
    incarnation: String,
    epoch: String,
    revision: String,
    progress: String,
    state: ControlState,
    owner: Option<RawOwner>,
    root: Option<RawRoot>,
    recovery: Option<RawRecoveryOverlay>,
    code: String,
    schema: u32,
    next_due_ms: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawOwner {
    session: String,
    endpoint: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawRoot {
    digest: String,
    txid: String,
    checksum: String,
    commit_sequence: String,
}

impl From<&RootRef> for RawRoot {
    fn from(root: &RootRef) -> Self {
        Self {
            digest: encode_hex(root.digest.as_bytes()),
            txid: root.txid.to_string(),
            checksum: encode_hex(&root.checksum.to_be_bytes()),
            commit_sequence: root.commit_sequence.to_string(),
        }
    }
}

impl TryFrom<RawRoot> for RootRef {
    type Error = Error;

    fn try_from(root: RawRoot) -> Result<Self> {
        Ok(Self {
            digest: Digest::from_bytes(decode_hex(&root.digest)?),
            txid: decimal_u64(&root.txid)?,
            checksum: u64::from_be_bytes(decode_hex(&root.checksum)?),
            commit_sequence: decimal_u64(&root.commit_sequence)?,
        })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawRecoveryOverlay {
    leader_session: String,
    log_epoch: String,
    manifest_digest: String,
    first_node_sequence: String,
    last_node_sequence: String,
    predecessor: RawRoot,
    final_txid: String,
    final_checksum: String,
    final_commit_sequence: String,
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
            recovery: control
                .recovery
                .as_ref()
                .map(|recovery| RawRecoveryOverlay {
                    leader_session: encode_hex(recovery.leader_session.as_bytes()),
                    log_epoch: recovery.log_epoch.to_string(),
                    manifest_digest: encode_hex(recovery.manifest_digest.as_bytes()),
                    first_node_sequence: recovery.first_node_sequence.to_string(),
                    last_node_sequence: recovery.last_node_sequence.to_string(),
                    predecessor: RawRoot::from(&recovery.predecessor),
                    final_txid: recovery.final_txid.to_string(),
                    final_checksum: encode_hex(&recovery.final_checksum.to_be_bytes()),
                    final_commit_sequence: recovery.final_commit_sequence.to_string(),
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
            recovery: raw
                .recovery
                .map(|recovery| {
                    Ok::<RecoveryOverlayRef, Error>(RecoveryOverlayRef {
                        leader_session: SessionId::from_bytes(decode_hex(
                            &recovery.leader_session,
                        )?),
                        log_epoch: decimal_u64(&recovery.log_epoch)?,
                        manifest_digest: Digest::from_bytes(decode_hex(&recovery.manifest_digest)?),
                        first_node_sequence: decimal_u64(&recovery.first_node_sequence)?,
                        last_node_sequence: decimal_u64(&recovery.last_node_sequence)?,
                        predecessor: RootRef::try_from(recovery.predecessor)?,
                        final_txid: decimal_u64(&recovery.final_txid)?,
                        final_checksum: u64::from_be_bytes(decode_hex(&recovery.final_checksum)?),
                        final_commit_sequence: decimal_u64(&recovery.final_commit_sequence)?,
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
