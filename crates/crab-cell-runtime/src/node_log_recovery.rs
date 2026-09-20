use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures_util::{StreamExt, future::join_all, stream};

use crate::{
    ApplicationId, CatalogProof, CellAuthority, CellCatalog, CellId, Digest, Error,
    FencedNodeSession, NodeDirectory, NodeId, NodeLogPhase, NodeLogTransport, NodeTakeoverProof,
    RecoveryBase, RecoveryManifestStore, Result, SealRequest, SealedNodeLog, SessionId,
    TailRequest, Transition, VersionedControl, build_recovery_overlays_file_backed,
    build_recovery_overlays_file_backed_stream,
};

const MAX_RECOVERY_CATALOG_HEAD_READS: usize = 32;

const MAX_RECOVERY_PAGE_BYTES: u64 = 1 << 20;
const MAX_RECOVERY_PAGE_FRAMES: usize = 4_096;
const WITNESS_RECORD_HEADER_BYTES: usize = 8 + 32;

struct WitnessWriter {
    path: tempfile::TempPath,
    file: File,
    first_sequence: Option<u64>,
    last_sequence: Option<u64>,
    frame_count: u64,
}

impl WitnessWriter {
    fn new(directory: &Path) -> Result<Self> {
        let temporary = tempfile::Builder::new()
            .prefix(".crab-witness-")
            .tempfile_in(directory)?;
        let path = temporary.into_temp_path();
        let file = OpenOptions::new().append(true).open(&path)?;
        Ok(Self {
            path,
            file,
            first_sequence: None,
            last_sequence: None,
            frame_count: 0,
        })
    }

    fn push(&mut self, frame: &crab_ltx::VerifiedNodeFrame) -> Result<()> {
        let encoded = frame.encoded();
        let length = u64::try_from(encoded.len())
            .map_err(|_| Error::Node("recovery witness frame length overflows"))?;
        self.file.write_all(&length.to_le_bytes())?;
        self.file.write_all(&frame.digest())?;
        self.file.write_all(encoded)?;
        self.first_sequence
            .get_or_insert(frame.scope().node_sequence);
        self.last_sequence = Some(frame.scope().node_sequence);
        self.frame_count = self
            .frame_count
            .checked_add(1)
            .ok_or(Error::Node("recovery witness frame count overflows"))?;
        Ok(())
    }

    fn matches_range(&self, first: u64, last: u64) -> bool {
        self.first_sequence == Some(first)
            && self.last_sequence == Some(last)
            && self.frame_count == last.saturating_sub(first).saturating_add(1)
    }

    fn finish(self) -> Result<SealedWitness> {
        self.file.sync_all()?;
        if self.first_sequence.is_none() || self.last_sequence.is_none() {
            return Err(Error::Node("recovery witness is empty"));
        }
        Ok(SealedWitness {
            path: self.path,
            frame_count: self.frame_count,
        })
    }
}

struct SealedWitness {
    path: tempfile::TempPath,
    frame_count: u64,
}

impl SealedWitness {
    fn reader(&self, limits: crab_ltx::Limits) -> Result<WitnessReader> {
        Ok(WitnessReader {
            file: File::open(&self.path)?,
            limits,
            remaining: self.frame_count,
        })
    }
}

struct WitnessReader {
    file: File,
    limits: crab_ltx::Limits,
    remaining: u64,
}

impl Iterator for WitnessReader {
    type Item = Result<crab_ltx::VerifiedNodeFrame>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        let mut header = [0_u8; WITNESS_RECORD_HEADER_BYTES];
        if let Err(error) = self.file.read_exact(&mut header) {
            return Some(Err(error.into()));
        }
        let length = u64::from_le_bytes(header[..8].try_into().ok()?);
        let max_encoded = self.limits.max_capture_bytes.saturating_add(240);
        if length > max_encoded || length > usize::MAX as u64 {
            return Some(Err(Error::Node("recovery witness frame exceeds limit")));
        }
        let mut encoded = vec![0_u8; length as usize];
        if let Err(error) = self.file.read_exact(&mut encoded) {
            return Some(Err(error.into()));
        }
        if *blake3::hash(&encoded).as_bytes() != header[8..] {
            return Some(Err(Error::Node("recovery witness frame digest differs")));
        }
        self.remaining = self.remaining.saturating_sub(1);
        Some(crab_ltx::inspect_node_frame(encoded.into(), self.limits).map_err(Into::into))
    }
}

enum WitnessCollector {
    Memory(Vec<crab_ltx::VerifiedNodeFrame>),
    File(WitnessWriter),
}

enum WitnessMaterial {
    Memory(Vec<crab_ltx::VerifiedNodeFrame>),
    File(SealedWitness),
}

impl WitnessCollector {
    fn file(directory: &Path) -> Result<Self> {
        Ok(Self::File(WitnessWriter::new(directory)?))
    }

    fn push(&mut self, frames: Vec<crab_ltx::VerifiedNodeFrame>) -> Result<()> {
        match self {
            Self::Memory(existing) => existing.extend(frames),
            Self::File(writer) => {
                for frame in &frames {
                    writer.push(frame)?;
                }
            }
        }
        Ok(())
    }

    fn matches_range(&self, first: u64, last: u64) -> bool {
        match self {
            Self::Memory(frames) => {
                frames.first().map(|frame| frame.scope().node_sequence) == Some(first)
                    && frames.last().map(|frame| frame.scope().node_sequence) == Some(last)
                    && frames.windows(2).all(|pair| {
                        pair[0].scope().node_sequence.checked_add(1)
                            == Some(pair[1].scope().node_sequence)
                    })
            }
            Self::File(writer) => writer.matches_range(first, last),
        }
    }

    fn finish(self) -> Result<WitnessMaterial> {
        match self {
            Self::Memory(frames) => Ok(WitnessMaterial::Memory(frames)),
            Self::File(writer) => Ok(WitnessMaterial::File(writer.finish()?)),
        }
    }
}

/// Verified uncovered suffix gathered after every reachable follower is sealed.
pub struct SealedSession {
    pub leader_session: SessionId,
    pub log_epoch: u64,
    pub tiered_through: u64,
    pub durable_through: u64,
    pub frames: Vec<crab_ltx::VerifiedNodeFrame>,
    witness: Option<SealedWitness>,
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

    /// Returns authenticated frame scopes without materializing frame bodies.
    pub fn scopes(&self, limits: crab_ltx::Limits) -> Result<Vec<crab_ltx::NodeFrameScope>> {
        if let Some(witness) = &self.witness {
            return witness
                .reader(limits)?
                .map(|frame| frame.map(|frame| frame.scope()))
                .collect();
        }
        Ok(self.frames.iter().map(|frame| frame.scope()).collect())
    }
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
    let scopes = frames.iter().map(|frame| frame.scope()).collect::<Vec<_>>();
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
        return Ok(Vec::new());
    }

    let mut needed = BTreeMap::<u8, Vec<[u8; 32]>>::new();
    for cell in scopes.keys() {
        needed.entry(cell[0]).or_default().push(*cell);
    }
    let mut entries = BTreeMap::<[u8; 32], CatalogProof>::new();
    for (shard, cells) in needed {
        let mut scan = catalog.scan_shard(shard).await?;
        while let Some(page) = scan.next_page().await? {
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
    Ok(recovered)
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
            return Ok(Vec::new());
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
            return Ok(Vec::new());
        }
        let pinned = self
            .manifests
            .pin(sealed.leader_session, sealed.log_epoch, tails)
            .await?;
        if pinned.len() > cells.len() {
            return Err(Error::Node("recovery manifest exceeds Cell inventory"));
        }

        let mut attached = Vec::with_capacity(pinned.len());
        for pin in pinned {
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
        Ok(attached)
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
                _reservation: None,
            });
        }

        let required_first = self
            .tiered_through
            .checked_add(1)
            .ok_or(Error::Node("node-log recovery sequence overflow"))?;
        let reservation = self
            .recovery_disk
            .try_reserve(recovery_tail_reservation_bytes(self.limits))?;
        let mut observed = BTreeMap::new();
        let mut selected = None::<WitnessMaterial>;
        let scratch = self
            .recovery_scratch
            .clone()
            .unwrap_or_else(std::env::temp_dir);
        for (member, receipt) in receipts {
            let Ok(receipt) = receipt else {
                continue;
            };
            if receipt.durable_through < required_first {
                continue;
            }
            let first = required_first.max(receipt.base_sequence);
            if first > receipt.durable_through {
                continue;
            }
            let retain = selected.is_none()
                && first == required_first
                && receipt.durable_through == durable_through;
            let mut first_sequence = first;
            let mut candidate = if retain {
                Some(if bounded {
                    WitnessCollector::file(&scratch)?
                } else {
                    WitnessCollector::Memory(Vec::new())
                })
            } else {
                None
            };
            let mut tail_bytes = 0_u64;
            let complete;
            loop {
                let Ok(page) = self
                    .transport
                    .tail_page(
                        member,
                        TailRequest {
                            leader_session: self.leader_session,
                            log_epoch: self.log_epoch,
                            first_sequence,
                        },
                    )
                    .await
                else {
                    complete = false;
                    break;
                };
                if page.frames.is_empty() {
                    complete = false;
                    break;
                }
                let page_count = page.frames.len();
                if page_count > MAX_RECOVERY_PAGE_FRAMES {
                    complete = false;
                    break;
                }
                let Ok(verified) = page
                    .frames
                    .into_iter()
                    .map(|bytes| crab_ltx::inspect_node_frame(bytes, self.limits))
                    .collect::<crab_ltx::Result<Vec<_>>>()
                else {
                    complete = false;
                    break;
                };
                // A valid frame digest proves its bytes, not that it belongs to
                // the claimed failed session. Bind every page to that lane and
                // the sealed range before retaining it as recovery evidence.
                if verified.iter().enumerate().any(|(offset, frame)| {
                    let scope = frame.scope();
                    scope.leader_session != *self.leader_session.as_bytes()
                        || scope.log_epoch != self.log_epoch
                        || first_sequence.checked_add(offset as u64) != Some(scope.node_sequence)
                        || scope.node_sequence > receipt.durable_through
                }) {
                    complete = false;
                    break;
                }
                let page_bytes = verified.iter().try_fold(0_u64, |bytes, frame| {
                    bytes.checked_add(frame.encoded().len() as u64)
                });
                let Some(page_bytes) = page_bytes else {
                    complete = false;
                    break;
                };
                let page_limit = if page_count == 1 {
                    // A single LTX frame may exceed the network page target;
                    // keep the same bounded-one-frame exception as FollowerStore.
                    MAX_RECOVERY_PAGE_BYTES.saturating_add(self.limits.max_capture_bytes)
                } else {
                    MAX_RECOVERY_PAGE_BYTES
                };
                if page_bytes > page_limit {
                    complete = false;
                    break;
                }
                tail_bytes = match tail_bytes.checked_add(page_bytes) {
                    Some(bytes) if bytes <= recovery_tail_reservation_bytes(self.limits) => bytes,
                    _ => {
                        complete = false;
                        break;
                    }
                };
                // A shorter or partially readable follower may still expose
                // a conflicting valid frame. Never choose a witness by order
                // while silently ignoring that evidence from another member.
                for frame in &verified {
                    let digest = frame.digest();
                    if observed
                        .insert(frame.scope().node_sequence, digest)
                        .is_some_and(|previous| previous != digest)
                    {
                        return Err(Error::Node("follower witnesses disagree"));
                    }
                }
                let last_sequence = verified.last().map(|frame| frame.scope().node_sequence);
                if let Some(collector) = candidate.as_mut()
                    && collector.push(verified).is_err()
                {
                    complete = false;
                    break;
                }
                let Some(next_sequence) = page.next_sequence else {
                    complete = last_sequence == Some(receipt.durable_through);
                    break;
                };
                let Ok(page_count) = u64::try_from(page_count) else {
                    complete = false;
                    break;
                };
                let Some(expected_next) = first_sequence.checked_add(page_count) else {
                    complete = false;
                    break;
                };
                if next_sequence != expected_next || next_sequence > receipt.durable_through {
                    complete = false;
                    break;
                }
                first_sequence = next_sequence;
            }
            if !complete || !retain {
                continue;
            }
            let Some(candidate) = candidate else {
                continue;
            };
            if !candidate.matches_range(required_first, durable_through) {
                continue;
            }
            selected = Some(candidate.finish()?);
        }
        if let Some(selected) = selected {
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
                _reservation: Some(reservation),
            });
        }
        Err(Error::Node(
            "active node log has no complete follower witness",
        ))
    }
}

fn default_recovery_disk(limits: crab_ltx::Limits) -> crab_ltx::DiskBudget {
    crab_ltx::DiskBudget::new(recovery_tail_reservation_bytes(limits))
}

fn recovery_tail_reservation_bytes(limits: crab_ltx::Limits) -> u64 {
    limits.max_plan_bytes.min(512 << 20)
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use futures_util::future::BoxFuture;

    use super::*;
    use crate::{
        AppendRequest, FollowerStore, LocalFollowerTransport, NodeLogTransport, RetireRequest,
    };

    struct FailingFirstTransport {
        failed: NodeId,
        good: LocalFollowerTransport,
        gap: bool,
        oversized: bool,
        replacement: Option<Bytes>,
    }

    struct FleetTransport {
        stores: Vec<(NodeId, FollowerStore)>,
    }

    impl FleetTransport {
        fn store(&self, member: NodeId) -> Result<&FollowerStore> {
            self.stores
                .iter()
                .find_map(|(candidate, store)| (*candidate == member).then_some(store))
                .ok_or(Error::Node("follower is absent from test fleet"))
        }
    }

    impl NodeLogTransport for FleetTransport {
        fn append<'a>(
            &'a self,
            member: NodeId,
            request: AppendRequest,
        ) -> BoxFuture<'a, Result<crate::FollowerReceipt>> {
            Box::pin(async move {
                self.store(member)?
                    .append(
                        request.leader_session,
                        request.log_epoch,
                        request.frames,
                        request.covered_through,
                    )
                    .await
            })
        }

        fn seal<'a>(
            &'a self,
            member: NodeId,
            request: SealRequest,
        ) -> BoxFuture<'a, Result<crate::FollowerReceipt>> {
            Box::pin(async move {
                self.store(member)?
                    .seal(request.leader_session, request.log_epoch)
                    .await
            })
        }

        fn retire<'a>(
            &'a self,
            member: NodeId,
            request: RetireRequest,
        ) -> BoxFuture<'a, Result<crate::FollowerReceipt>> {
            Box::pin(async move {
                self.store(member)?
                    .retire(
                        request.leader_session,
                        request.log_epoch,
                        request.covered_through,
                    )
                    .await
            })
        }

        fn tail<'a>(
            &'a self,
            member: NodeId,
            request: TailRequest,
        ) -> BoxFuture<'a, Result<Vec<Bytes>>> {
            Box::pin(async move {
                self.store(member)?
                    .read_tail(
                        request.leader_session,
                        request.log_epoch,
                        request.first_sequence,
                    )
                    .await
            })
        }
    }

    impl NodeLogTransport for FailingFirstTransport {
        fn append<'a>(
            &'a self,
            member: NodeId,
            request: AppendRequest,
        ) -> BoxFuture<'a, Result<crate::FollowerReceipt>> {
            self.good.append(member, request)
        }

        fn seal<'a>(
            &'a self,
            member: NodeId,
            request: SealRequest,
        ) -> BoxFuture<'a, Result<crate::FollowerReceipt>> {
            if member == self.failed {
                return Box::pin(async {
                    Ok(crate::FollowerReceipt {
                        base_sequence: 1,
                        durable_through: 1,
                    })
                });
            }
            self.good.seal(member, request)
        }

        fn retire<'a>(
            &'a self,
            member: NodeId,
            request: RetireRequest,
        ) -> BoxFuture<'a, Result<crate::FollowerReceipt>> {
            self.good.retire(member, request)
        }

        fn tail<'a>(
            &'a self,
            member: NodeId,
            request: TailRequest,
        ) -> BoxFuture<'a, Result<Vec<Bytes>>> {
            if member == self.failed {
                return Box::pin(async { Err(Error::Node("injected follower read failure")) });
            }
            self.good.tail(member, request)
        }

        fn tail_page<'a>(
            &'a self,
            member: NodeId,
            request: TailRequest,
        ) -> BoxFuture<'a, Result<crate::FollowerTailPage>> {
            if member == self.failed {
                return Box::pin(async { Err(Error::Node("injected follower read failure")) });
            }
            let good = self.good.clone();
            let gap = self.gap;
            let oversized = self.oversized;
            let replacement = self.replacement.clone();
            Box::pin(async move {
                let mut page = good.tail_page(member, request).await?;
                if let Some(replacement) = replacement {
                    page.frames = vec![replacement];
                }
                if oversized {
                    let first = page
                        .frames
                        .first()
                        .cloned()
                        .ok_or(Error::Node("test page has no frame"))?;
                    page.frames =
                        std::iter::repeat_n(first, MAX_RECOVERY_PAGE_FRAMES + 1).collect();
                    page.next_sequence = Some(
                        request
                            .first_sequence
                            .checked_add(MAX_RECOVERY_PAGE_FRAMES as u64 + 1)
                            .ok_or(Error::Node("test page sequence overflow"))?,
                    );
                }
                if gap {
                    let count = u64::try_from(page.frames.len())
                        .map_err(|_| Error::Node("test page frame count overflow"))?;
                    page.next_sequence = Some(
                        request
                            .first_sequence
                            .checked_add(count)
                            .and_then(|next| next.checked_add(1))
                            .ok_or(Error::Node("test page sequence overflow"))?,
                    );
                }
                Ok(page)
            })
        }
    }

    #[tokio::test]
    async fn active_lane_requires_and_returns_a_complete_follower_tail() {
        let limits = crab_ltx::Limits::default();
        let source = tempfile::TempDir::new().unwrap();
        let mut database = crab_ltx::Db::open(&source.path().join("cell.sqlite"), limits).unwrap();
        database
            .transaction(|transaction| {
                transaction.execute_batch(
                    "CREATE TABLE values_(v); INSERT INTO values_ VALUES(randomblob(2097152))",
                )
            })
            .unwrap();
        let capture = database.capture().unwrap();
        let segment = capture.segments.first().unwrap();
        let leader = SessionId::from_bytes([1; 16]);
        let member = NodeId::from_bytes([2; 16]);
        let frame = crab_ltx::encode_node_frame(
            crab_ltx::NodeFrameScope {
                leader_session: *leader.as_bytes(),
                log_epoch: 3,
                node_sequence: 1,
                application: [4; 16],
                cell: [5; 32],
                incarnation: [6; 16],
                cell_epoch: 7,
                commit_sequence: 1,
            },
            segment.info().clone(),
            Bytes::from(std::fs::read(segment.path()).unwrap()),
            limits,
        )
        .unwrap();
        assert!(frame.encoded().len() > MAX_RECOVERY_PAGE_BYTES as usize);
        let root = tempfile::TempDir::new().unwrap();
        let store = FollowerStore::open(
            root.path().to_owned(),
            limits,
            crab_ltx::DiskBudget::new(1 << 30),
        )
        .unwrap();
        let transport: Arc<dyn NodeLogTransport> =
            Arc::new(LocalFollowerTransport::new(member, store));
        transport
            .append(
                member,
                AppendRequest {
                    leader_session: leader,
                    log_epoch: 3,
                    frames: vec![frame.encoded().clone()],
                    covered_through: 0,
                },
            )
            .await
            .unwrap();
        let rejected = NodeLogRecovery::new(
            Arc::clone(&transport),
            NodeId::from_bytes([1; 16]),
            leader,
            3,
            vec![member],
            0,
            true,
            limits,
        )
        .unwrap()
        .with_recovery_disk(crab_ltx::DiskBudget::new(0));
        assert!(rejected.ensure_sealed().await.is_err());
        let budget = crab_ltx::DiskBudget::new(1 << 30);
        let recovery = NodeLogRecovery::new(
            Arc::clone(&transport),
            NodeId::from_bytes([1; 16]),
            leader,
            3,
            vec![member],
            0,
            true,
            limits,
        )
        .unwrap()
        .with_recovery_disk(budget.clone());
        let sealed = recovery.ensure_sealed().await.unwrap();
        assert_eq!(sealed.durable_through, 1);
        assert_eq!(sealed.frames.len(), 1);
        assert!(budget.used() > 0);
        drop(sealed);
        assert_eq!(budget.used(), 0);
        let scratch = tempfile::TempDir::new().unwrap();
        let bounded = NodeLogRecovery::new(
            Arc::clone(&transport),
            NodeId::from_bytes([1; 16]),
            leader,
            3,
            vec![member],
            0,
            true,
            limits,
        )
        .unwrap()
        .with_recovery_disk(crab_ltx::DiskBudget::new(1 << 30))
        .with_recovery_scratch(scratch.path().to_owned());
        let sealed = bounded.ensure_sealed_bounded().await.unwrap();
        assert!(sealed.frames.is_empty());
        assert_eq!(sealed.frame_count(), 1);
        assert_eq!(sealed.scopes(limits).unwrap().len(), 1);
        drop(sealed);
        assert!(scratch.path().read_dir().unwrap().next().is_none());
        database.close().unwrap();
    }

    #[tokio::test]
    async fn recovery_uses_the_next_complete_witness_after_a_read_failure() {
        let limits = crab_ltx::Limits::default();
        let source = tempfile::TempDir::new().unwrap();
        let mut database = crab_ltx::Db::open(&source.path().join("cell.sqlite"), limits).unwrap();
        database
            .transaction(|transaction| transaction.execute_batch("CREATE TABLE values_(v)"))
            .unwrap();
        let segment = database.capture().unwrap().segments.remove(0);
        let leader = SessionId::from_bytes([1; 16]);
        let failed = NodeId::from_bytes([2; 16]);
        let good = NodeId::from_bytes([3; 16]);
        let frame = crab_ltx::encode_node_frame(
            crab_ltx::NodeFrameScope {
                leader_session: *leader.as_bytes(),
                log_epoch: 3,
                node_sequence: 1,
                application: [4; 16],
                cell: [5; 32],
                incarnation: [6; 16],
                cell_epoch: 7,
                commit_sequence: 1,
            },
            segment.info().clone(),
            Bytes::from(std::fs::read(segment.path()).unwrap()),
            limits,
        )
        .unwrap();
        let root = tempfile::TempDir::new().unwrap();
        let local = LocalFollowerTransport::new(
            good,
            FollowerStore::open(
                root.path().to_owned(),
                limits,
                crab_ltx::DiskBudget::new(1 << 30),
            )
            .unwrap(),
        );
        local
            .append(
                good,
                AppendRequest {
                    leader_session: leader,
                    log_epoch: 3,
                    frames: vec![frame.encoded().clone()],
                    covered_through: 0,
                },
            )
            .await
            .unwrap();
        let transport: Arc<dyn NodeLogTransport> = Arc::new(FailingFirstTransport {
            failed,
            good: local.clone(),
            gap: false,
            oversized: false,
            replacement: None,
        });
        let recovery = NodeLogRecovery::new(
            transport,
            NodeId::from_bytes([1; 16]),
            leader,
            3,
            vec![failed, good],
            0,
            true,
            limits,
        )
        .unwrap();
        assert_eq!(recovery.ensure_sealed().await.unwrap().frames.len(), 1);

        let gapped: Arc<dyn NodeLogTransport> = Arc::new(FailingFirstTransport {
            failed,
            good: local.clone(),
            gap: true,
            oversized: false,
            replacement: None,
        });
        let recovery = NodeLogRecovery::new(
            gapped,
            NodeId::from_bytes([1; 16]),
            leader,
            3,
            vec![good],
            0,
            true,
            limits,
        )
        .unwrap();
        assert!(recovery.ensure_sealed().await.is_err());

        let oversized: Arc<dyn NodeLogTransport> = Arc::new(FailingFirstTransport {
            failed,
            good: local.clone(),
            gap: false,
            oversized: true,
            replacement: None,
        });
        let recovery = NodeLogRecovery::new(
            oversized,
            NodeId::from_bytes([1; 16]),
            leader,
            3,
            vec![good],
            0,
            true,
            limits,
        )
        .unwrap();
        assert!(recovery.ensure_sealed().await.is_err());
        for wrong_epoch in [false, true] {
            let mut scope = frame.scope();
            if wrong_epoch {
                scope.log_epoch += 1;
            } else {
                scope.leader_session = [99; 16];
            }
            let replacement = crab_ltx::encode_node_frame(
                scope,
                frame.segment().clone(),
                frame.body().clone(),
                limits,
            )
            .unwrap();
            let transport = Arc::new(FailingFirstTransport {
                failed,
                good: local.clone(),
                gap: false,
                oversized: false,
                replacement: Some(replacement.encoded().clone()),
            });
            let recovery = NodeLogRecovery::new(
                transport,
                NodeId::from_bytes([1; 16]),
                leader,
                3,
                vec![good],
                0,
                true,
                limits,
            )
            .unwrap();
            assert!(recovery.ensure_sealed().await.is_err());
        }
        database.close().unwrap();
    }

    #[tokio::test]
    async fn recovery_survives_a_simultaneous_follower_fleet_restart() {
        let limits = crab_ltx::Limits::default();
        let source = tempfile::TempDir::new().unwrap();
        let mut database = crab_ltx::Db::open(&source.path().join("cell.sqlite"), limits).unwrap();
        database
            .transaction(|transaction| transaction.execute_batch("CREATE TABLE values_(v)"))
            .unwrap();
        let segment = database.capture().unwrap().segments.remove(0);
        let leader = SessionId::from_bytes([1; 16]);
        let leader_node = NodeId::from_bytes([1; 16]);
        let members = [NodeId::from_bytes([2; 16]), NodeId::from_bytes([3; 16])];
        let frame = crab_ltx::encode_node_frame(
            crab_ltx::NodeFrameScope {
                leader_session: *leader.as_bytes(),
                log_epoch: 3,
                node_sequence: 1,
                application: [4; 16],
                cell: [5; 32],
                incarnation: [6; 16],
                cell_epoch: 7,
                commit_sequence: 1,
            },
            segment.info().clone(),
            Bytes::from(std::fs::read(segment.path()).unwrap()),
            limits,
        )
        .unwrap();
        for conflict in [false, true] {
            let roots = [
                tempfile::TempDir::new().unwrap(),
                tempfile::TempDir::new().unwrap(),
            ];
            for (index, (member, root)) in members.iter().zip(&roots).enumerate() {
                let store = FollowerStore::open(
                    root.path().to_owned(),
                    limits,
                    crab_ltx::DiskBudget::new(1 << 30),
                )
                .unwrap();
                let mut scope = frame.scope();
                if conflict && index == 1 {
                    scope.cell = [99; 32];
                }
                let frame = crab_ltx::encode_node_frame(
                    scope,
                    frame.segment().clone(),
                    frame.body().clone(),
                    limits,
                )
                .unwrap();
                store
                    .append(leader, 3, vec![frame.encoded().clone()], 0)
                    .await
                    .unwrap();
                drop(store);
                assert!(root.path().join("followers").exists(), "{member:?}");
            }

            let transport: Arc<dyn NodeLogTransport> = Arc::new(FleetTransport {
                stores: members
                    .iter()
                    .zip(&roots)
                    .map(|(member, root)| {
                        (
                            *member,
                            FollowerStore::open(
                                root.path().to_owned(),
                                limits,
                                crab_ltx::DiskBudget::new(1 << 30),
                            )
                            .unwrap(),
                        )
                    })
                    .collect(),
            });
            let recovery = NodeLogRecovery::new(
                transport,
                leader_node,
                leader,
                3,
                members.to_vec(),
                0,
                true,
                limits,
            )
            .unwrap();

            let result = recovery.ensure_sealed().await;
            if conflict {
                assert!(matches!(
                    result,
                    Err(Error::Node("follower witnesses disagree"))
                ));
                continue;
            }
            let sealed = result.unwrap();
            assert_eq!(sealed.durable_through, 1);
            assert_eq!(sealed.frames.len(), 1);
            assert_eq!(sealed.frames[0].scope().node_sequence, 1);
        }
        database.close().unwrap();
    }
}
