use bytes::Bytes;
use crab_storage::{CellStorageLayout, StorageError};
use serde::{Deserialize, Serialize};

use crate::{
    ApplicationId, CellId, Digest, Error, IncarnationId, RecoveredCellTail, RecoveryOverlayRef,
    Result, RootRef, SessionId,
};

const MAX_MANIFEST_BYTES: u64 = 2 << 20;

/// One control-ready pointer returned after bundle and manifest publication.
pub struct PinnedRecoveryCell {
    pub application: ApplicationId,
    pub cell: CellId,
    pub incarnation: IncarnationId,
    pub cell_epoch: u64,
    pub recovery: RecoveryOverlayRef,
}

/// Immutable object-store owner for recovered follower bundles and manifests.
#[derive(Clone)]
pub struct RecoveryManifestStore {
    layout: CellStorageLayout,
    limits: crab_ltx::Limits,
}

impl RecoveryManifestStore {
    #[must_use]
    pub fn new(layout: CellStorageLayout, limits: crab_ltx::Limits) -> Self {
        Self { layout, limits }
    }

    /// Publishes every verified bundle before one content-addressed manifest.
    pub async fn pin(
        &self,
        leader_session: SessionId,
        log_epoch: u64,
        tails: Vec<RecoveredCellTail>,
    ) -> Result<Vec<PinnedRecoveryCell>> {
        if leader_session.as_bytes().iter().all(|byte| *byte == 0)
            || log_epoch == 0
            || tails.is_empty()
        {
            return Err(Error::Node("invalid recovery manifest scope"));
        }
        let mut rows = Vec::with_capacity(tails.len());
        for tail in &tails {
            let bundle = tail.overlay.bundle().bytes();
            let bundle_digest = *blake3::hash(bundle).as_bytes();
            let path = self.layout.node_log_bundle_path(
                leader_session.as_bytes(),
                log_epoch,
                &bundle_digest,
            );
            publish_immutable(&self.layout, &path, bundle, self.limits.max_plan_bytes).await?;
            let predecessor = tail.overlay.predecessor();
            rows.push(ManifestCell {
                application: tail.application,
                cell: predecessor.cell,
                incarnation: predecessor.incarnation,
                cell_epoch: tail.cell_epoch,
                first_node_sequence: tail.first_node_sequence,
                last_node_sequence: tail.last_node_sequence,
                predecessor,
                final_position: tail.overlay.final_position(),
                final_commit_sequence: tail.overlay.final_commit_sequence(),
                bundle_digest,
            });
        }
        rows.sort_unstable_by(|left, right| {
            (
                left.application,
                left.cell,
                left.incarnation,
                left.cell_epoch,
            )
                .cmp(&(
                    right.application,
                    right.cell,
                    right.incarnation,
                    right.cell_epoch,
                ))
        });
        if rows.windows(2).any(|pair| {
            pair[0].application == pair[1].application
                && pair[0].cell == pair[1].cell
                && pair[0].incarnation == pair[1].incarnation
                && pair[0].cell_epoch == pair[1].cell_epoch
        }) {
            return Err(Error::Node(
                "recovery manifest contains duplicate Cell scope",
            ));
        }
        let manifest = RecoveryManifest {
            leader_session,
            log_epoch,
            cells: rows,
        };
        let body = manifest.encode()?;
        let manifest_digest = *blake3::hash(&body).as_bytes();
        let path = self.layout.node_log_recovery_path(
            leader_session.as_bytes(),
            log_epoch,
            &manifest_digest,
        );
        publish_immutable(&self.layout, &path, &body, MAX_MANIFEST_BYTES).await?;
        Ok(manifest
            .cells
            .into_iter()
            .map(|cell| PinnedRecoveryCell {
                application: ApplicationId::from_bytes(cell.application),
                cell: CellId::from_bytes(cell.cell),
                incarnation: IncarnationId::from_bytes(cell.incarnation),
                cell_epoch: cell.cell_epoch,
                recovery: RecoveryOverlayRef {
                    leader_session,
                    log_epoch,
                    manifest_digest: Digest::from_bytes(manifest_digest),
                    first_node_sequence: cell.first_node_sequence,
                    last_node_sequence: cell.last_node_sequence,
                    predecessor: runtime_root(cell.predecessor),
                    final_txid: cell.final_position.txid,
                    final_checksum: cell.final_position.checksum,
                    final_commit_sequence: cell.final_commit_sequence,
                },
            })
            .collect())
    }

    /// Reopens the exact bundle named by a control-pinned recovery reference.
    pub async fn load_overlay(
        &self,
        cell: CellId,
        incarnation: IncarnationId,
        recovery: &RecoveryOverlayRef,
    ) -> Result<crab_ltx::RecoveryOverlay> {
        let path = self.layout.node_log_recovery_path(
            recovery.leader_session.as_bytes(),
            recovery.log_epoch,
            recovery.manifest_digest.as_bytes(),
        );
        let (body, _) = self
            .layout
            .store()
            .get_with_etag_bounded(&path, MAX_MANIFEST_BYTES)
            .await?;
        if *blake3::hash(&body).as_bytes() != *recovery.manifest_digest.as_bytes() {
            return Err(Error::Node("recovery manifest digest differs"));
        }
        let manifest = RecoveryManifest::decode(&body)?;
        if manifest.leader_session != recovery.leader_session
            || manifest.log_epoch != recovery.log_epoch
        {
            return Err(Error::Node("recovery manifest path scope differs"));
        }
        let row = manifest
            .cells
            .into_iter()
            .find(|row| {
                row.application == *self.layout.application_id()
                    && row.cell == *cell.as_bytes()
                    && row.incarnation == *incarnation.as_bytes()
            })
            .ok_or(Error::Node("recovery manifest does not contain Cell"))?;
        let expected = RecoveryOverlayRef {
            leader_session: recovery.leader_session,
            log_epoch: recovery.log_epoch,
            manifest_digest: recovery.manifest_digest,
            first_node_sequence: row.first_node_sequence,
            last_node_sequence: row.last_node_sequence,
            predecessor: runtime_root(row.predecessor),
            final_txid: row.final_position.txid,
            final_checksum: row.final_position.checksum,
            final_commit_sequence: row.final_commit_sequence,
        };
        if &expected != recovery {
            return Err(Error::Node(
                "recovery control pointer differs from manifest",
            ));
        }
        let bundle_path = self.layout.node_log_bundle_path(
            recovery.leader_session.as_bytes(),
            recovery.log_epoch,
            &row.bundle_digest,
        );
        let (bundle_bytes, _) = self
            .layout
            .store()
            .get_with_etag_bounded(&bundle_path, self.limits.max_plan_bytes)
            .await?;
        if *blake3::hash(&bundle_bytes).as_bytes() != row.bundle_digest {
            return Err(Error::Node("recovery bundle digest differs"));
        }
        let bundle = crab_ltx::bundle::Bundle::decode(bundle_bytes.to_vec(), self.limits)?;
        Ok(crab_ltx::RecoveryOverlay::new(
            row.predecessor,
            bundle,
            row.final_position,
            row.final_commit_sequence,
        ))
    }
}

struct RecoveryManifest {
    leader_session: SessionId,
    log_epoch: u64,
    cells: Vec<ManifestCell>,
}

struct ManifestCell {
    application: [u8; 16],
    cell: [u8; 32],
    incarnation: [u8; 16],
    cell_epoch: u64,
    first_node_sequence: u64,
    last_node_sequence: u64,
    predecessor: crab_ltx::RootRef,
    final_position: crab_ltx::Position,
    final_commit_sequence: u64,
    bundle_digest: [u8; 32],
}

impl RecoveryManifest {
    fn encode(&self) -> Result<Vec<u8>> {
        let body = serde_json::to_vec(&RawManifest::from(self))?;
        if body.len() as u64 > MAX_MANIFEST_BYTES {
            return Err(Error::Node("recovery manifest exceeds limit"));
        }
        Ok(body)
    }

    fn decode(body: &[u8]) -> Result<Self> {
        if body.len() as u64 > MAX_MANIFEST_BYTES {
            return Err(Error::Node("recovery manifest exceeds limit"));
        }
        let raw: RawManifest = serde_json::from_slice(body)?;
        let manifest = Self::try_from(raw)?;
        if manifest.encode()? != body {
            return Err(Error::Node("recovery manifest is not canonical"));
        }
        Ok(manifest)
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    version: u32,
    leader_session: String,
    log_epoch: String,
    cells: Vec<RawManifestCell>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifestCell {
    application: String,
    cell: String,
    incarnation: String,
    cell_epoch: String,
    first_node_sequence: String,
    last_node_sequence: String,
    predecessor_digest: String,
    predecessor_txid: String,
    predecessor_checksum: String,
    predecessor_commit_sequence: String,
    final_txid: String,
    final_checksum: String,
    final_commit_sequence: String,
    bundle_digest: String,
}

impl From<&RecoveryManifest> for RawManifest {
    fn from(manifest: &RecoveryManifest) -> Self {
        Self {
            version: 1,
            leader_session: hex(manifest.leader_session.as_bytes()),
            log_epoch: manifest.log_epoch.to_string(),
            cells: manifest
                .cells
                .iter()
                .map(|cell| RawManifestCell {
                    application: hex(&cell.application),
                    cell: hex(&cell.cell),
                    incarnation: hex(&cell.incarnation),
                    cell_epoch: cell.cell_epoch.to_string(),
                    first_node_sequence: cell.first_node_sequence.to_string(),
                    last_node_sequence: cell.last_node_sequence.to_string(),
                    predecessor_digest: hex(&cell.predecessor.digest),
                    predecessor_txid: cell.predecessor.position.txid.to_string(),
                    predecessor_checksum: hex(&cell.predecessor.position.checksum.to_be_bytes()),
                    predecessor_commit_sequence: cell.predecessor.commit_sequence.to_string(),
                    final_txid: cell.final_position.txid.to_string(),
                    final_checksum: hex(&cell.final_position.checksum.to_be_bytes()),
                    final_commit_sequence: cell.final_commit_sequence.to_string(),
                    bundle_digest: hex(&cell.bundle_digest),
                })
                .collect(),
        }
    }
}

impl TryFrom<RawManifest> for RecoveryManifest {
    type Error = Error;

    fn try_from(raw: RawManifest) -> Result<Self> {
        if raw.version != 1 || raw.cells.is_empty() || raw.cells.len() > 1_024 {
            return Err(Error::Node("invalid recovery manifest shape"));
        }
        let leader_session = SessionId::from_bytes(unhex(&raw.leader_session)?);
        let log_epoch = decimal(&raw.log_epoch)?;
        let mut cells = Vec::with_capacity(raw.cells.len());
        for cell in raw.cells {
            let cell_id = unhex(&cell.cell)?;
            let incarnation = unhex(&cell.incarnation)?;
            cells.push(ManifestCell {
                application: unhex(&cell.application)?,
                cell: cell_id,
                incarnation,
                cell_epoch: decimal(&cell.cell_epoch)?,
                first_node_sequence: decimal(&cell.first_node_sequence)?,
                last_node_sequence: decimal(&cell.last_node_sequence)?,
                predecessor: crab_ltx::RootRef {
                    cell: cell_id,
                    incarnation,
                    digest: unhex(&cell.predecessor_digest)?,
                    position: crab_ltx::Position {
                        txid: decimal(&cell.predecessor_txid)?,
                        checksum: u64::from_be_bytes(unhex(&cell.predecessor_checksum)?),
                    },
                    commit_sequence: decimal(&cell.predecessor_commit_sequence)?,
                },
                final_position: crab_ltx::Position {
                    txid: decimal(&cell.final_txid)?,
                    checksum: u64::from_be_bytes(unhex(&cell.final_checksum)?),
                },
                final_commit_sequence: decimal(&cell.final_commit_sequence)?,
                bundle_digest: unhex(&cell.bundle_digest)?,
            });
        }
        let manifest = Self {
            leader_session,
            log_epoch,
            cells,
        };
        if leader_session.as_bytes().iter().all(|byte| *byte == 0)
            || log_epoch == 0
            || manifest.cells.iter().any(|cell| {
                cell.cell_epoch == 0
                    || cell.first_node_sequence == 0
                    || cell.first_node_sequence > cell.last_node_sequence
                    || cell.final_position.txid <= cell.predecessor.position.txid
                    || cell.final_commit_sequence <= cell.predecessor.commit_sequence
            })
        {
            return Err(Error::Node("invalid recovery manifest values"));
        }
        Ok(manifest)
    }
}

async fn publish_immutable(
    layout: &CellStorageLayout,
    path: &object_store::path::Path,
    body: &[u8],
    limit: u64,
) -> Result<()> {
    if body.len() as u64 > limit {
        return Err(Error::Node("recovery object exceeds limit"));
    }
    match layout
        .store()
        .create_strict(path, Bytes::copy_from_slice(body))
        .await
    {
        Ok(()) => Ok(()),
        Err(create_error) => match layout.store().get_with_etag_bounded(path, limit).await {
            Ok((existing, _)) if existing.as_ref() == body => Ok(()),
            Ok(_) => Err(Error::Node("recovery digest path contains different bytes")),
            Err(StorageError::NotFound { .. }) => Err(create_error.into()),
            Err(error) => Err(error.into()),
        },
    }
}

fn runtime_root(root: crab_ltx::RootRef) -> RootRef {
    RootRef {
        digest: Digest::from_bytes(root.digest),
        txid: root.position.txid,
        checksum: root.position.checksum,
        commit_sequence: root.commit_sequence,
    }
}

fn decimal(value: &str) -> Result<u64> {
    let parsed = value
        .parse::<u64>()
        .map_err(|_| Error::Node("invalid recovery manifest decimal"))?;
    if parsed.to_string() != value {
        return Err(Error::Node("noncanonical recovery manifest decimal"));
    }
    Ok(parsed)
}

fn hex(bytes: &[u8]) -> String {
    const TABLE: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(TABLE[(byte >> 4) as usize] as char);
        encoded.push(TABLE[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn unhex<const N: usize>(value: &str) -> Result<[u8; N]> {
    if value.len() != N * 2 {
        return Err(Error::Node("invalid recovery manifest hex length"));
    }
    let mut decoded = [0_u8; N];
    for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
        decoded[index] = (nibble(pair[0])? << 4) | nibble(pair[1])?;
    }
    Ok(decoded)
}

fn nibble(value: u8) -> Result<u8> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        _ => Err(Error::Node("invalid recovery manifest hex")),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use object_store::{memory::InMemory, path::Path};

    use super::*;
    use crate::{RecoveryBase, build_recovery_overlays};

    #[tokio::test]
    async fn pinned_manifest_reopens_exact_overlay_and_prepares_successor() {
        let limits = crab_ltx::Limits::default();
        let directory = tempfile::TempDir::new().unwrap();
        let mut database =
            crab_ltx::ManagedDb::open(&directory.path().join("cell.sqlite"), limits).unwrap();
        database
            .transaction(|transaction| transaction.execute_batch("CREATE TABLE values_(v)"))
            .unwrap();
        let first = database.capture().unwrap();
        let store = crab_storage::Store::new(Arc::new(InMemory::new()));
        let application = [3; 16];
        let cell = [4; 32];
        let incarnation = [5; 16];
        let layout = CellStorageLayout::new(store, Path::from("root"), application);
        let replica =
            crab_ltx::CellReplica::new(layout.clone(), cell, incarnation, limits).unwrap();
        let base = replica.prepare(None, &first, 1, 1).await.unwrap().root();
        database
            .transaction(|transaction| {
                transaction.execute("INSERT INTO values_ VALUES (1)", [])?;
                Ok(())
            })
            .unwrap();
        let tail = database.capture().unwrap();
        let segment = tail.segments.first().unwrap();
        let frame = crab_ltx::encode_node_frame(
            crab_ltx::NodeFrameScope {
                leader_session: [1; 16],
                log_epoch: 2,
                node_sequence: 1,
                application,
                cell,
                incarnation,
                cell_epoch: 6,
                commit_sequence: 2,
            },
            segment.info().clone(),
            Bytes::from(std::fs::read(segment.path()).unwrap()),
            limits,
        )
        .unwrap();
        let recovered = build_recovery_overlays(
            vec![frame],
            &[RecoveryBase {
                application,
                cell_epoch: 6,
                root: base,
            }],
            limits,
        )
        .unwrap();
        let manifests = RecoveryManifestStore::new(layout, limits);
        let pinned = manifests
            .pin(SessionId::from_bytes([1; 16]), 2, recovered)
            .await
            .unwrap();
        assert_eq!(pinned.len(), 1);
        let overlay = manifests
            .load_overlay(
                CellId::from_bytes(cell),
                IncarnationId::from_bytes(incarnation),
                &pinned[0].recovery,
            )
            .await
            .unwrap();
        let prepared = replica
            .prepare_recovered_overlay(&overlay, 1)
            .await
            .unwrap();
        assert_eq!(prepared.predecessor(), Some(base));
        assert_eq!(prepared.root().position, tail.position);
        let mut control = crate::Control::initial(
            CellId::from_bytes(cell),
            IncarnationId::from_bytes(incarnation),
            crate::Owner {
                session: SessionId::from_bytes([1; 16]),
                endpoint: "https://dead.internal:8081".into(),
            },
            Digest::from_bytes([12; 32]),
            1,
        )
        .unwrap();
        control.state = crate::ControlState::Serving;
        control.root = Some(runtime_root(base));
        let attached = control.attach_recovery(pinned[0].recovery.clone()).unwrap();
        let takeover = attached
            .takeover(crate::Owner {
                session: SessionId::from_bytes([13; 16]),
                endpoint: "https://successor.internal:8081".into(),
            })
            .unwrap();
        let published = takeover.publish_recovery(&prepared, None).unwrap();
        assert_eq!(published.state, crate::ControlState::Recovering);
        assert!(published.recovery.is_none());
        assert_eq!(published.root.unwrap().txid, tail.position.txid);
        database.close().unwrap();
    }
}
