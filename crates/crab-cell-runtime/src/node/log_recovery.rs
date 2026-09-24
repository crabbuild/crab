//! Node-log recovery: inventory, witnesses, and owner-loss coordination.
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures_util::{StreamExt, future::join_all, stream};

use crate::cell::catalog::{CatalogProof, CellCatalog};
use crate::control::Transition;
use crate::control::authority::{CellAuthority, VersionedControl};
use crate::follower::FollowerReceipt;
use crate::identity::NodeId;
use crate::identity::{ApplicationId, CellId, Digest, SessionId};
use crate::node::log::{
    RecoveryBase, build_recovery_overlays_file_backed, build_recovery_overlays_file_backed_stream,
};
use crate::node::log_state::NodeLogPhase;
use crate::node::log_transport::{NodeLogTransport, SealRequest, TailRequest};
use crate::node::{FencedNodeSession, NodeDirectory, NodeTakeoverProof, SealedNodeLog};
use crate::recovery::manifest::RecoveryManifestStore;
use crate::{Error, Result};

const MAX_RECOVERY_CATALOG_HEAD_READS: usize = 32;

const MAX_RECOVERY_PAGE_BYTES: u64 = 1 << 20;
const MAX_RECOVERY_PAGE_FRAMES: usize = 4_096;

mod witness;

use witness::*;

/// Bounded work counters emitted for one node-log recovery attempt.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RecoveryWorkSummary {
    pub candidate_count: u64,
    pub affected_cells: u64,
    pub catalog_shards: u64,
    pub catalog_pages: u64,
    pub control_reads: u64,
    pub follower_pages: u64,
    pub follower_frames: u64,
    pub follower_bytes: u64,
    pub peer_requests: u64,
    pub bundle_bytes: u64,
    pub object_reads: u64,
    pub object_writes: u64,
}

impl RecoveryWorkSummary {
    pub fn merge(&mut self, other: Self) -> Result<()> {
        self.candidate_count = self
            .candidate_count
            .checked_add(other.candidate_count)
            .ok_or(Error::Capacity("recovery candidate count"))?;
        self.affected_cells = self
            .affected_cells
            .checked_add(other.affected_cells)
            .ok_or(Error::Capacity("recovery affected Cell count"))?;
        self.catalog_shards = self
            .catalog_shards
            .checked_add(other.catalog_shards)
            .ok_or(Error::Capacity("recovery catalog shard count"))?;
        self.catalog_pages = self
            .catalog_pages
            .checked_add(other.catalog_pages)
            .ok_or(Error::Capacity("recovery catalog page count"))?;
        self.control_reads = self
            .control_reads
            .checked_add(other.control_reads)
            .ok_or(Error::Capacity("recovery control read count"))?;
        self.follower_pages = self
            .follower_pages
            .checked_add(other.follower_pages)
            .ok_or(Error::Capacity("recovery follower page count"))?;
        self.follower_frames = self
            .follower_frames
            .checked_add(other.follower_frames)
            .ok_or(Error::Capacity("recovery follower frame count"))?;
        self.follower_bytes = self
            .follower_bytes
            .checked_add(other.follower_bytes)
            .ok_or(Error::Capacity("recovery follower byte count"))?;
        self.peer_requests = self
            .peer_requests
            .checked_add(other.peer_requests)
            .ok_or(Error::Capacity("recovery peer request count"))?;
        self.bundle_bytes = self
            .bundle_bytes
            .checked_add(other.bundle_bytes)
            .ok_or(Error::Capacity("recovery bundle byte count"))?;
        self.object_reads = self
            .object_reads
            .checked_add(other.object_reads)
            .ok_or(Error::Capacity("recovery object read count"))?;
        self.object_writes = self
            .object_writes
            .checked_add(other.object_writes)
            .ok_or(Error::Capacity("recovery object write count"))?;
        Ok(())
    }
}

/// Catalog work counters collected while validating recovered frame scopes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RecoveryInventorySummary {
    pub affected_cells: u64,
    pub catalog_shards: u64,
    pub catalog_pages: u64,
    pub control_reads: u64,
}

/// Recovery Cells together with the bounded catalog work needed to discover them.
pub struct RecoverableCellInventory {
    pub cells: Vec<RecoveryCell>,
    pub summary: RecoveryInventorySummary,
}

/// Verified uncovered suffix gathered after every reachable follower is sealed.
pub struct SealedSession {
    pub leader_session: SessionId,
    pub log_epoch: u64,
    pub tiered_through: u64,
    pub durable_through: u64,
    pub frames: Vec<crab_ltx::VerifiedNodeFrame>,
    witness: Option<SealedWitness>,
    work: RecoveryWorkSummary,
    // Keep admission until the caller has pinned or discarded the recovered
    // bytes, not merely until their last network page arrives.
    _reservation: Option<crab_ltx::DiskReservation>,
}

impl SealedSession {
    #[must_use]
    pub fn frame_count(&self) -> u64 {
        self.witness
            .as_ref()
            .map_or(self.frames.len() as u64, |witness| witness.frame_count)
    }

    #[must_use]
    pub const fn work(&self) -> RecoveryWorkSummary {
        self.work
    }

    /// Returns authenticated frame scopes without materializing frame bodies.
    pub fn scopes(&self, limits: crab_ltx::Limits) -> Result<Vec<crab_ltx::NodeFrameScope>> {
        if let Some(witness) = &self.witness {
            return unique_scopes(
                witness
                    .reader(limits)?
                    .map(|frame| frame.map(|frame| frame.scope())),
            );
        }
        unique_scopes(self.frames.iter().map(|frame| Ok(frame.scope())))
    }
}

/// Retains one authenticated generation per Cell while validating every frame
/// in the witness. Recovery only needs the affected Cell set; retaining every
/// frame scope would make catalog discovery grow with the uncovered tail.
fn unique_scopes<I>(scopes: I) -> Result<Vec<crab_ltx::NodeFrameScope>>
where
    I: IntoIterator<Item = Result<crab_ltx::NodeFrameScope>>,
{
    let mut unique = BTreeMap::<[u8; 32], crab_ltx::NodeFrameScope>::new();
    for scope in scopes {
        let scope = scope?;
        if let Some(existing) = unique.get(&scope.cell) {
            if existing.leader_session != scope.leader_session
                || existing.log_epoch != scope.log_epoch
                || existing.application != scope.application
                || existing.incarnation != scope.incarnation
                || existing.cell_epoch != scope.cell_epoch
            {
                return Err(Error::Control(
                    "recovery Cell scope has multiple generations",
                ));
            }
            continue;
        }
        unique.insert(scope.cell, scope);
    }
    Ok(unique.into_values().collect())
}

/// Mechanical seal-and-gather coordinator for one already claimed dead session.
///
/// Session expiry and recovery-claim CAS remain authority concerns. This type
/// never decides that a leader is dead; it only gathers the exact lane named by
/// its constructor.
pub struct NodeLogRecovery {
    transport: Arc<dyn NodeLogTransport>,
    leader_session: SessionId,
    log_epoch: u64,
    members: Vec<NodeId>,
    tiered_through: u64,
    active: bool,
    limits: crab_ltx::Limits,
    recovery_disk: crab_ltx::DiskBudget,
    recovery_scratch: Option<PathBuf>,
}

/// One dead-session Cell control that may need a recovered tail attached.
pub struct RecoveryCell {
    pub application: ApplicationId,
    pub authority: CellAuthority,
    pub observed: VersionedControl,
}

/// Seals one claimed node log and pins every recovered Cell tail before return.
pub struct RecoveryCoordinator {
    recovery: NodeLogRecovery,
    manifests: RecoveryManifestStore,
}

/// Completed dead-session recovery with every overlay pinned before log seal.
pub struct CompletedNodeRecovery {
    pub sealed: SealedNodeLog,
    pub controls: Vec<VersionedControl>,
    pub takeover: NodeTakeoverProof,
}

/// Recovery controls and publication work produced by one sealed recovery.
pub struct RecoveryCoordinatorResult {
    pub controls: Vec<VersionedControl>,
    pub publication: crate::recovery::manifest::RecoveryPublicationSummary,
}

/// Scans one application catalog for published Cells owned by a dead session.
pub async fn recoverable_cells(
    catalog: &CellCatalog,
    authority: &CellAuthority,
    owner: SessionId,
    limit: usize,
) -> Result<Vec<RecoveryCell>> {
    if owner.as_bytes().iter().all(|byte| *byte == 0) || limit == 0 {
        return Err(Error::Node("node recovery inventory bound is invalid"));
    }
    let mut cells = Vec::new();
    let mut scans = stream::iter(0_u16..=u8::MAX.into())
        .map(|shard| catalog.scan_shard(shard as u8))
        .buffer_unordered(MAX_RECOVERY_CATALOG_HEAD_READS);
    while let Some(scan) = scans.next().await {
        let mut scan = scan?;
        while let Some(page) = scan.next_page().await? {
            for proof in page.entries() {
                let Some(observed) = authority.load(proof.entry().cell()).await? else {
                    continue;
                };
                if observed.value().owner.as_ref().map(|owner| owner.session) != Some(owner)
                    || observed.value().ltx_root().is_none()
                {
                    continue;
                }
                if cells.len() == limit {
                    return Err(Error::Node(
                        "node recovery Cell inventory exceeds its limit",
                    ));
                }
                cells.push(RecoveryCell {
                    application: catalog.application(),
                    authority: authority.clone(),
                    observed,
                });
            }
        }
    }
    Ok(cells)
}

/// Loads only catalog entries whose authenticated node-frame scopes are present
/// in a sealed witness. Each affected catalog shard is scanned once, then the
/// exact current Cell control is revalidated before recovery may proceed.
pub async fn recoverable_cells_from_frames(
    catalog: &CellCatalog,
    authority: &CellAuthority,
    owner: SessionId,
    frames: &[crab_ltx::VerifiedNodeFrame],
    limit: usize,
) -> Result<Vec<RecoveryCell>> {
    let scopes = unique_scopes(frames.iter().map(|frame| Ok(frame.scope())))?;
    recoverable_cells_from_scopes(catalog, authority, owner, &scopes, limit).await
}

/// Loads only catalog entries named by authenticated witness scopes.
pub async fn recoverable_cells_from_scopes(
    catalog: &CellCatalog,
    authority: &CellAuthority,
    owner: SessionId,
    frame_scopes: &[crab_ltx::NodeFrameScope],
    limit: usize,
) -> Result<Vec<RecoveryCell>> {
    Ok(
        recoverable_cells_from_scopes_with_summary(catalog, authority, owner, frame_scopes, limit)
            .await?
            .cells,
    )
}

pub async fn recoverable_cells_from_scopes_with_summary(
    catalog: &CellCatalog,
    authority: &CellAuthority,
    owner: SessionId,
    frame_scopes: &[crab_ltx::NodeFrameScope],
    limit: usize,
) -> Result<RecoverableCellInventory> {
    if owner.as_bytes().iter().all(|byte| *byte == 0) || limit == 0 {
        return Err(Error::Node("node recovery inventory bound is invalid"));
    }
    type Scope = ([u8; 16], u64);
    let mut scopes = BTreeMap::<[u8; 32], Scope>::new();
    let application = *catalog.application().as_bytes();
    for scope in frame_scopes {
        if scope.leader_session != *owner.as_bytes() || scope.application != application {
            return Err(Error::Node("recovery frame application or owner differs"));
        }
        let key = (scope.incarnation, scope.cell_epoch);
        if let Some(existing) = scopes.get(&scope.cell) {
            if *existing != key {
                return Err(Error::Control(
                    "recovery Cell scope has multiple generations",
                ));
            }
        } else {
            if scopes.len() == limit {
                return Err(Error::Node(
                    "node recovery Cell inventory exceeds its limit",
                ));
            }
            scopes.insert(scope.cell, key);
        }
    }
    if scopes.is_empty() {
        return Ok(RecoverableCellInventory {
            cells: Vec::new(),
            summary: RecoveryInventorySummary::default(),
        });
    }

    let mut needed = BTreeMap::<u8, Vec<[u8; 32]>>::new();
    for cell in scopes.keys() {
        needed.entry(cell[0]).or_default().push(*cell);
    }
    let affected_cells =
        u64::try_from(scopes.len()).map_err(|_| Error::Capacity("recovery affected Cell count"))?;
    let catalog_shards =
        u64::try_from(needed.len()).map_err(|_| Error::Capacity("recovery catalog shard count"))?;
    let mut entries = BTreeMap::<[u8; 32], CatalogProof>::new();
    let mut catalog_pages = 0_u64;
    for (shard, cells) in needed {
        let mut scan = catalog.scan_shard(shard).await?;
        while let Some(page) = scan.next_page().await? {
            catalog_pages = catalog_pages
                .checked_add(1)
                .ok_or(Error::Capacity("recovery catalog page count"))?;
            for proof in page.entries() {
                if cells.binary_search(proof.entry().cell().as_bytes()).is_ok() {
                    entries.insert(*proof.entry().cell().as_bytes(), proof.clone());
                }
            }
        }
    }

    let mut recovered = Vec::with_capacity(scopes.len());
    for (cell_bytes, (incarnation, cell_epoch)) in scopes {
        let cell = CellId::from_bytes(cell_bytes);
        let proof = entries
            .remove(&cell_bytes)
            .ok_or(Error::Catalog("recovery frame Cell is not cataloged"))?;
        if proof.entry().cell() != cell {
            return Err(Error::Catalog("recovery catalog proof scope differs"));
        }
        let observed = authority
            .load(cell)
            .await?
            .ok_or(Error::Control("recovery Cell control is missing"))?;
        let control = observed.value();
        if control.owner.as_ref().map(|current| current.session) != Some(owner)
            || control.incarnation.as_bytes() != &incarnation
            || control.epoch != cell_epoch
            || control.ltx_root().is_none()
        {
            return Err(Error::Control("recovery Cell control scope differs"));
        }
        recovered.push(RecoveryCell {
            application: catalog.application(),
            authority: authority.clone(),
            observed,
        });
    }
    Ok(RecoverableCellInventory {
        summary: RecoveryInventorySummary {
            affected_cells,
            catalog_shards,
            catalog_pages,
            control_reads: u64::try_from(recovered.len())
                .map_err(|_| Error::Capacity("recovery control read count"))?,
        },
        cells: recovered,
    })
}

impl RecoveryCoordinator {
    #[must_use]
    pub const fn new(recovery: NodeLogRecovery, manifests: RecoveryManifestStore) -> Self {
        Self {
            recovery,
            manifests,
        }
    }

    /// Attaches immutable overlays to the exact dead-owner controls.
    ///
    /// Successful earlier attachments remain valid if a later Cell conflicts;
    /// a retry must reload every control and rebuild against its exact root.
    pub async fn recover(
        &self,
        fenced: FencedNodeSession,
        cells: Vec<RecoveryCell>,
    ) -> Result<Vec<VersionedControl>> {
        self.recovery.validate_fence(&fenced)?;
        let sealed = self.recovery.ensure_sealed().await?;
        self.recover_sealed(fenced, cells, sealed).await
    }

    /// Attaches overlays from a witness that was already sealed by the caller.
    /// Keeping the witness explicit lets orchestration derive affected Cells
    /// from authenticated frame scopes before any catalog scan.
    pub async fn recover_sealed(
        &self,
        fenced: FencedNodeSession,
        cells: Vec<RecoveryCell>,
        sealed: SealedSession,
    ) -> Result<Vec<VersionedControl>> {
        Ok(self
            .recover_sealed_with_summary(fenced, cells, sealed)
            .await?
            .controls)
    }

    pub async fn recover_sealed_with_summary(
        &self,
        fenced: FencedNodeSession,
        cells: Vec<RecoveryCell>,
        sealed: SealedSession,
    ) -> Result<RecoveryCoordinatorResult> {
        self.recovery.validate_fence(&fenced)?;
        let mut bases = Vec::with_capacity(cells.len());
        for cell in &cells {
            let control = cell.observed.value();
            let root = control
                .ltx_root()
                .ok_or(Error::Control("recovery Cell has no published root"))?;
            if control.owner.as_ref().map(|owner| owner.session) != Some(fenced.session())
                || *cell.application.as_bytes() == [0; 16]
                || control.recovery.as_ref().is_some_and(|recovery| {
                    recovery.leader_session != fenced.session()
                        || recovery.log_epoch != self.recovery.log_epoch
                })
            {
                return Err(Error::Control("recovery Cell scope differs"));
            }
            bases.push(RecoveryBase {
                application: *cell.application.as_bytes(),
                cell_epoch: control.epoch,
                root,
            });
        }

        if sealed.frame_count() == 0 {
            return Ok(RecoveryCoordinatorResult {
                controls: Vec::new(),
                publication: crate::recovery::manifest::RecoveryPublicationSummary::default(),
            });
        }
        let scratch = self.manifests.recovery_scratch_directory();
        let tails = if let Some(witness) = &sealed.witness {
            let reader = witness.reader(self.recovery.limits)?;
            build_recovery_overlays_file_backed_stream(
                reader,
                &bases,
                self.recovery.limits,
                &scratch,
            )?
        } else {
            build_recovery_overlays_file_backed(
                sealed.frames,
                &bases,
                self.recovery.limits,
                &scratch,
            )?
        };
        if tails.is_empty() {
            return Ok(RecoveryCoordinatorResult {
                controls: Vec::new(),
                publication: crate::recovery::manifest::RecoveryPublicationSummary::default(),
            });
        }
        let pinned = self
            .manifests
            .pin_with_summary(sealed.leader_session, sealed.log_epoch, tails)
            .await?;
        if pinned.cells.len() > cells.len() {
            return Err(Error::Node("recovery manifest exceeds Cell inventory"));
        }

        let mut attached = Vec::with_capacity(pinned.cells.len());
        for pin in pinned.cells {
            let cell = cells
                .iter()
                .find(|candidate| {
                    candidate.application == pin.application
                        && candidate.observed.value().cell == pin.cell
                        && candidate.observed.value().incarnation == pin.incarnation
                        && candidate.observed.value().epoch == pin.cell_epoch
                })
                .ok_or(Error::Node("recovered Cell is absent from inventory"))?;
            if let Some(current) = cell.observed.value().recovery.as_ref() {
                if current == &pin.recovery {
                    attached.push(cell.observed.clone());
                    continue;
                }
                return Err(Error::Control(
                    "different recovery overlay is already pinned",
                ));
            }
            let successor = cell.observed.value().attach_recovery(pin.recovery)?;
            let versioned = match cell
                .authority
                .transition(
                    &cell.observed,
                    successor.clone(),
                    Transition::AttachRecovery,
                )
                .await
            {
                Ok(versioned) => versioned,
                Err(error) => {
                    let current = cell
                        .authority
                        .load(cell.observed.value().cell)
                        .await?
                        .ok_or(Error::Fenced)?;
                    if current.value() == &successor {
                        current
                    } else {
                        return Err(error);
                    }
                }
            };
            attached.push(versioned);
        }
        Ok(RecoveryCoordinatorResult {
            controls: attached,
            publication: pinned.summary,
        })
    }

    /// Pins every recovered Cell and then atomically seals the claimed node log.
    pub async fn recover_and_seal(
        &self,
        directory: &NodeDirectory,
        fenced: FencedNodeSession,
        cells: Vec<RecoveryCell>,
        now_ms: i64,
    ) -> Result<CompletedNodeRecovery> {
        let controls = self.recover(fenced.clone(), cells).await?;
        self.finish(directory, fenced, controls, now_ms).await
    }

    /// Seals one still-current claim after all recovered controls were pinned.
    pub async fn finish(
        &self,
        directory: &NodeDirectory,
        fenced: FencedNodeSession,
        controls: Vec<VersionedControl>,
        now_ms: i64,
    ) -> Result<CompletedNodeRecovery> {
        self.recovery.validate_fence(&fenced)?;
        let mut manifest = None::<Digest>;
        for control in &controls {
            let recovery = control
                .value()
                .recovery
                .as_ref()
                .ok_or(Error::Control("recovered Cell has no pinned overlay"))?;
            if recovery.leader_session != fenced.session()
                || recovery.log_epoch != self.recovery.log_epoch
            {
                return Err(Error::Control("recovered Cell overlay scope differs"));
            }
            match manifest {
                None => manifest = Some(recovery.manifest_digest),
                Some(current) if current == recovery.manifest_digest => {}
                Some(_) => {
                    return Err(Error::Control(
                        "recovered session produced multiple manifests",
                    ));
                }
            }
        }
        let sealed = directory.seal_recovery(&fenced, manifest, now_ms).await?;
        let takeover = NodeTakeoverProof::after_recovery(&fenced, &sealed)?;
        Ok(CompletedNodeRecovery {
            sealed,
            controls,
            takeover,
        })
    }
}

impl NodeLogRecovery {
    pub(crate) fn new(
        transport: Arc<dyn NodeLogTransport>,
        leader_node: NodeId,
        leader_session: SessionId,
        log_epoch: u64,
        members: Vec<NodeId>,
        tiered_through: u64,
        active: bool,
        limits: crab_ltx::Limits,
    ) -> Result<Self> {
        if leader_node.as_bytes().iter().all(|byte| *byte == 0)
            || leader_session.as_bytes().iter().all(|byte| *byte == 0)
            || log_epoch == 0
            || members.is_empty()
            || members.len() > 2
            || members.contains(&leader_node)
            || members
                .iter()
                .any(|member| member.as_bytes().iter().all(|byte| *byte == 0))
            || !members
                .windows(2)
                .all(|pair| pair[0].as_bytes() < pair[1].as_bytes())
        {
            return Err(Error::Node("invalid node-log recovery ensemble"));
        }
        Ok(Self {
            transport,
            leader_session,
            log_epoch,
            members,
            tiered_through,
            active,
            limits,
            recovery_disk: default_recovery_disk(limits),
            recovery_scratch: None,
        })
    }

    /// Builds recovery only from the exact CAS-protected failed-session log.
    pub fn from_fenced(
        transport: Arc<dyn NodeLogTransport>,
        fenced: &FencedNodeSession,
        limits: crab_ltx::Limits,
    ) -> Result<Self> {
        Self::from_fenced_with_disk(transport, fenced, limits, default_recovery_disk(limits))
    }

    /// Builds recovery with a shared node-local disk budget.
    pub fn from_fenced_with_disk(
        transport: Arc<dyn NodeLogTransport>,
        fenced: &FencedNodeSession,
        limits: crab_ltx::Limits,
        recovery_disk: crab_ltx::DiskBudget,
    ) -> Result<Self> {
        let log = fenced
            .log()
            .ok_or(Error::Node("fenced session has no enrolled node log"))?;
        let claim = log
            .recovery()
            .ok_or(Error::Node("fenced node log has no recovery claim"))?;
        if log.phase() != NodeLogPhase::Recovering
            || claim.claimant() != fenced.claimant()
            || claim.generation() != fenced.claim_generation()
            || claim.expires_at_ms() != fenced.claim_expires_at_ms()
        {
            return Err(Error::Fenced);
        }
        Self::new(
            transport,
            fenced.node(),
            fenced.session(),
            log.epoch(),
            log.members().to_vec(),
            log.tiered_through(),
            log.active(),
            limits,
        )
        .map(|mut recovery| {
            recovery.recovery_disk = recovery_disk;
            recovery
        })
    }

    /// Uses a shared budget for temporary follower-tail materialization.
    pub fn with_recovery_disk(mut self, recovery_disk: crab_ltx::DiskBudget) -> Self {
        self.recovery_disk = recovery_disk;
        self
    }

    /// Uses the runtime-owned session volume for the bounded witness file.
    pub fn with_recovery_scratch(mut self, directory: PathBuf) -> Self {
        self.recovery_scratch = Some(directory);
        self
    }

    fn validate_fence(&self, fenced: &FencedNodeSession) -> Result<()> {
        let log = fenced.log().ok_or(Error::Fenced)?;
        let claim = log.recovery().ok_or(Error::Fenced)?;
        if fenced.session() != self.leader_session
            || log.phase() != NodeLogPhase::Recovering
            || log.epoch() != self.log_epoch
            || log.members() != self.members
            || log.tiered_through() != self.tiered_through
            || log.active() != self.active
            || claim.claimant() != fenced.claimant()
            || claim.generation() != fenced.claim_generation()
            || claim.expires_at_ms() != fenced.claim_expires_at_ms()
        {
            return Err(Error::Fenced);
        }
        Ok(())
    }

    /// Seals all reachable members, rejects conflicts, and returns a complete witness.
    pub async fn ensure_sealed(&self) -> Result<SealedSession> {
        self.ensure_sealed_with_mode(false).await
    }

    /// Seals all reachable members while retaining the selected witness on the
    /// runtime scratch volume instead of the heap.
    pub async fn ensure_sealed_bounded(&self) -> Result<SealedSession> {
        self.ensure_sealed_with_mode(true).await
    }

    async fn ensure_sealed_with_mode(&self, bounded: bool) -> Result<SealedSession> {
        let receipts = join_all(self.members.iter().map(|member| {
            let transport = Arc::clone(&self.transport);
            let member = *member;
            async move {
                (
                    member,
                    transport
                        .seal(
                            member,
                            SealRequest {
                                leader_session: self.leader_session,
                                log_epoch: self.log_epoch,
                            },
                        )
                        .await,
                )
            }
        }))
        .await;
        if receipts
            .iter()
            .filter_map(|(_, receipt)| receipt.as_ref().ok())
            .any(|receipt| {
                (receipt.base_sequence == 0 && receipt.durable_through != 0)
                    || receipt.base_sequence > receipt.durable_through.saturating_add(1)
            })
        {
            return Err(Error::Node("follower seal receipt is invalid"));
        }
        let successful_seals = receipts
            .iter()
            .filter(|(_, receipt)| receipt.is_ok())
            .count();
        let durable_through = receipts
            .iter()
            .filter_map(|(_, receipt)| receipt.as_ref().ok())
            .map(|receipt| receipt.durable_through)
            .max()
            .unwrap_or(self.tiered_through);
        if durable_through <= self.tiered_through {
            if self.active && successful_seals == 0 {
                return Err(Error::Node(
                    "active node log has no complete follower witness",
                ));
            }
            return Ok(SealedSession {
                leader_session: self.leader_session,
                log_epoch: self.log_epoch,
                tiered_through: self.tiered_through,
                durable_through: self.tiered_through,
                frames: Vec::new(),
                witness: None,
                work: RecoveryWorkSummary {
                    peer_requests: u64::try_from(self.members.len())
                        .map_err(|_| Error::Capacity("recovery peer request count"))?,
                    ..RecoveryWorkSummary::default()
                },
                _reservation: None,
            });
        }

        let required_first = self
            .tiered_through
            .checked_add(1)
            .ok_or(Error::Node("node-log recovery sequence overflow"))?;
        let scratch = self
            .recovery_scratch
            .clone()
            .unwrap_or_else(std::env::temp_dir);
        let expected_frames = durable_through
            .checked_sub(required_first)
            .and_then(|count| count.checked_add(1))
            .ok_or(Error::Node("node-log recovery frame range overflows"))?;
        let digest_bytes = expected_frames
            .checked_mul(WITNESS_DIGEST_RECORD_BYTES)
            .ok_or(Error::Capacity("recovery witness digest table"))?;
        let digest_reservation = self.recovery_disk.try_reserve(digest_bytes)?;
        let mut candidates = receipts
            .iter()
            .filter_map(|(member, receipt)| {
                let receipt = receipt.as_ref().ok()?;
                (receipt.durable_through == durable_through
                    && receipt.base_sequence <= required_first)
                    .then_some((*member, *receipt))
            })
            .collect::<Vec<_>>();
        candidates.sort_unstable_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));

        let mut work = RecoveryWorkSummary {
            peer_requests: u64::try_from(self.members.len())
                .map_err(|_| Error::Capacity("recovery peer request count"))?,
            ..RecoveryWorkSummary::default()
        };
        for (candidate_member, candidate_receipt) in candidates {
            let candidate_reservation = self.recovery_disk.try_reserve(0)?;
            let mut collector = if bounded {
                WitnessCollector::file(&scratch)?
            } else {
                WitnessCollector::Memory(Vec::new())
            };
            let mut digest_writer =
                WitnessDigestWriter::new(&scratch, required_first, expected_frames)?;
            if !collect_member_tail(
                candidate_member,
                candidate_receipt,
                TailReadContext {
                    transport: self.transport.as_ref(),
                    leader_session: self.leader_session,
                    log_epoch: self.log_epoch,
                    required_first,
                    limits: self.limits,
                    reservation: Some(&candidate_reservation),
                    work: Some(&mut work),
                },
                TailSinks {
                    collector: Some(&mut collector),
                    digests: Some(&mut digest_writer),
                    compare: None,
                },
            )
            .await?
            {
                continue;
            }
            if !collector.matches_range(required_first, durable_through) {
                continue;
            }
            let mut digests = digest_writer.finish()?;
            for (member, receipt) in &receipts {
                if *member == candidate_member {
                    continue;
                }
                let Ok(receipt) = receipt else {
                    continue;
                };
                if !collect_member_tail(
                    *member,
                    *receipt,
                    TailReadContext {
                        transport: self.transport.as_ref(),
                        leader_session: self.leader_session,
                        log_epoch: self.log_epoch,
                        required_first,
                        limits: self.limits,
                        reservation: None,
                        work: Some(&mut work),
                    },
                    TailSinks {
                        collector: None,
                        digests: None,
                        compare: Some(&mut digests),
                    },
                )
                .await?
                {
                    continue;
                }
            }
            drop(digest_reservation);
            let selected = collector.finish()?;
            let (frames, witness) = match selected {
                WitnessMaterial::Memory(frames) => (frames, None),
                WitnessMaterial::File(witness) => (Vec::new(), Some(witness)),
            };
            return Ok(SealedSession {
                leader_session: self.leader_session,
                log_epoch: self.log_epoch,
                tiered_through: self.tiered_through,
                durable_through,
                frames,
                witness,
                work,
                _reservation: Some(candidate_reservation),
            });
        }
        drop(digest_reservation);
        Err(Error::Node(
            "active node log has no complete follower witness",
        ))
    }
}

/// Streams one follower tail through the common page/framing checks. A
/// transport or malformed-page failure returns `false` so another complete
/// witness may be tried; digest disagreement remains a hard recovery error.
struct TailReadContext<'a> {
    transport: &'a dyn NodeLogTransport,
    leader_session: SessionId,
    log_epoch: u64,
    required_first: u64,
    limits: crab_ltx::Limits,
    reservation: Option<&'a crab_ltx::DiskReservation>,
    work: Option<&'a mut RecoveryWorkSummary>,
}

struct TailSinks<'a> {
    collector: Option<&'a mut WitnessCollector>,
    digests: Option<&'a mut WitnessDigestWriter>,
    compare: Option<&'a mut SealedWitnessDigests>,
}

async fn collect_member_tail(
    member: NodeId,
    receipt: FollowerReceipt,
    context: TailReadContext<'_>,
    mut sinks: TailSinks<'_>,
) -> Result<bool> {
    let TailReadContext {
        transport,
        leader_session,
        log_epoch,
        required_first,
        limits,
        reservation,
        mut work,
    } = context;
    if receipt.durable_through < required_first {
        return Ok(false);
    }
    let mut first_sequence = required_first.max(receipt.base_sequence);
    if first_sequence > receipt.durable_through {
        return Ok(false);
    }
    let mut tail_bytes = 0_u64;
    loop {
        if let Some(work) = work.as_deref_mut() {
            work.peer_requests = work
                .peer_requests
                .checked_add(1)
                .ok_or(Error::Capacity("recovery peer request count"))?;
        }
        let page = match transport
            .tail_page(
                member,
                TailRequest {
                    leader_session,
                    log_epoch,
                    first_sequence,
                },
            )
            .await
        {
            Ok(page) => page,
            Err(_) => return Ok(false),
        };
        if page.frames.is_empty() || page.frames.len() > MAX_RECOVERY_PAGE_FRAMES {
            return Ok(false);
        }
        let page_count = page.frames.len();
        if let Some(work) = work.as_deref_mut() {
            work.follower_pages = work
                .follower_pages
                .checked_add(1)
                .ok_or(Error::Capacity("recovery follower page count"))?;
        }
        let verified = match page
            .frames
            .into_iter()
            .map(|bytes| crab_ltx::inspect_node_frame(bytes, limits))
            .collect::<crab_ltx::Result<Vec<_>>>()
        {
            Ok(verified) => verified,
            Err(_) => return Ok(false),
        };
        if verified.iter().enumerate().any(|(offset, frame)| {
            let scope = frame.scope();
            scope.leader_session != *leader_session.as_bytes()
                || scope.log_epoch != log_epoch
                || first_sequence.checked_add(offset as u64) != Some(scope.node_sequence)
                || scope.node_sequence > receipt.durable_through
        }) {
            return Ok(false);
        }
        let Some(page_bytes) = verified.iter().try_fold(0_u64, |bytes, frame| {
            bytes.checked_add(frame.encoded().len() as u64)
        }) else {
            return Ok(false);
        };
        if let Some(work) = work.as_deref_mut() {
            work.follower_frames = work
                .follower_frames
                .checked_add(
                    u64::try_from(verified.len())
                        .map_err(|_| Error::Capacity("recovery follower frame count"))?,
                )
                .ok_or(Error::Capacity("recovery follower frame count"))?;
            work.follower_bytes = work
                .follower_bytes
                .checked_add(page_bytes)
                .ok_or(Error::Capacity("recovery follower byte count"))?;
        }
        let page_limit = if page_count == 1 {
            MAX_RECOVERY_PAGE_BYTES.saturating_add(limits.max_capture_bytes)
        } else {
            MAX_RECOVERY_PAGE_BYTES
        };
        if page_bytes > page_limit {
            return Ok(false);
        }
        tail_bytes = match tail_bytes.checked_add(page_bytes) {
            Some(bytes) if bytes <= recovery_tail_reservation_bytes(limits) => bytes,
            _ => return Ok(false),
        };
        if let Some(reservation) = reservation {
            reservation.try_grow(page_bytes)?;
        }
        for frame in &verified {
            let sequence = frame.scope().node_sequence;
            let digest = frame.digest();
            if let Some(writer) = sinks.digests.as_mut() {
                writer.push(sequence, digest)?;
            }
            if let Some(table) = sinks.compare.as_mut() {
                table.matches(sequence, digest)?;
            }
        }
        let last_sequence = verified.last().map(|frame| frame.scope().node_sequence);
        if let Some(output) = sinks.collector.as_mut() {
            output.push(verified)?;
        }
        let Some(next_sequence) = page.next_sequence else {
            return Ok(last_sequence == Some(receipt.durable_through));
        };
        let page_count = u64::try_from(page_count)
            .map_err(|_| Error::Node("recovery page frame count overflows"))?;
        let expected_next = first_sequence
            .checked_add(page_count)
            .ok_or(Error::Node("recovery page sequence overflows"))?;
        if next_sequence != expected_next || next_sequence > receipt.durable_through {
            return Ok(false);
        }
        first_sequence = next_sequence;
    }
}

fn default_recovery_disk(limits: crab_ltx::Limits) -> crab_ltx::DiskBudget {
    crab_ltx::DiskBudget::new(recovery_tail_reservation_bytes(limits))
}

fn recovery_tail_reservation_bytes(limits: crab_ltx::Limits) -> u64 {
    limits.max_plan_bytes.min(512 << 20)
}

#[cfg(test)]
mod tests;
