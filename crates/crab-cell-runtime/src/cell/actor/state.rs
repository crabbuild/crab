//! Actor state: queues, admissions, activations, and task results.

use super::*;

pub(super) struct RuntimeInner {
    pub(super) sender: mpsc::Sender<Message>,
    pub(super) resources: ResourceLedger,
    pub(super) shutting_down: AtomicBool,
    pub(super) accepting_cells: AtomicBool,
    pub(super) session: SessionId,
    pub(super) pool: SqlWorkerPool,
    pub(super) replica_host: crab_ltx::Host,
    pub(super) application_limits: OnceLock<HashMap<crate::identity::NamespaceId, (u64, u64)>>,
    pub(super) node_lease: Arc<RuntimeNodeLease>,
    pub(super) node_durability: NodeDurabilitySlot,
    pub(super) telemetry: crate::fleet::telemetry::CellTelemetryHandle,
    pub(super) unpublished_node_log_bytes: Arc<AtomicU64>,
}

pub(super) enum RuntimeNodeLease {
    ObjectOnly,
    Required(OnceLock<NodeLeaseGuard>),
}

impl RuntimeNodeLease {
    pub(super) fn check(&self) -> crate::Result<()> {
        match self {
            Self::ObjectOnly => Ok(()),
            Self::Required(guard) => guard.get().ok_or(Error::Fenced)?.check(),
        }
    }

    pub(super) fn guard(&self) -> crate::Result<Option<NodeLeaseGuard>> {
        match self {
            Self::ObjectOnly => Ok(None),
            Self::Required(guard) => guard.get().cloned().map(Some).ok_or(Error::Fenced),
        }
    }

    pub(super) fn install(&self, guard: NodeLeaseGuard) -> crate::Result<()> {
        match self {
            Self::ObjectOnly => Err(Error::Control(
                "object-only Cell runtime does not accept a node lease",
            )),
            Self::Required(slot) => slot
                .set(guard)
                .map_err(|_| Error::Control("Cell runtime node lease was initialized twice")),
        }
    }
}

pub(super) enum Activation {
    Restored(Box<RestoredActivation>),
    Bootstrap(Box<BootstrapActivation>),
}

pub(super) struct RestoredActivation {
    pub(super) database: crate::cell::worker::RestoredDatabase,
    pub(super) destination: PathBuf,
    pub(super) incarnation: crate::identity::IncarnationId,
    pub(super) schema: u32,
    pub(super) root: crab_ltx::RootRef,
    pub(super) reservation: CellReservation,
}

pub(super) struct BootstrapActivation {
    pub(super) replica: crab_ltx::CellReplica,
    pub(super) destination: PathBuf,
    pub(super) incarnation: crate::identity::IncarnationId,
    pub(super) schema: u32,
    pub(super) initialize: Initializer,
    pub(super) reservation: CellReservation,
}

pub(super) type IdleTransferCandidates = Vec<(CellId, u64, i64, CatalogRole)>;

pub(super) enum Message {
    Activate {
        cell: CellId,
        role: CatalogRole,
        catalog: CatalogProof,
        activation: Activation,
        publisher: Box<CellPublisher>,
        reply: oneshot::Sender<crate::Result<Arc<CellAdmission>>>,
    },
    Execute(Box<QueuedCommand>),
    Query(Box<QueuedQuery>),
    Resolve(Box<QueuedResolve>),
    Migrate(Box<QueuedMigration>),
    Lookup {
        cell: CellId,
        require_resident: bool,
        reply: oneshot::Sender<Option<LocalCell>>,
    },
    /// Lists resident Cells whose published due time has passed.
    ///
    /// The scheduler uses this to tick a Cell it already owns without reading
    /// its catalog entry or control record first.
    DueResident {
        now_ms: i64,
        limit: usize,
        reply: oneshot::Sender<Vec<DueResidentCell>>,
    },
    Drain {
        cell: CellId,
        admission: Arc<CellAdmission>,
        reply: oneshot::Sender<crate::Result<()>>,
    },
    EvictIdle {
        limit: usize,
        reply: oneshot::Sender<crate::Result<usize>>,
    },
    IdleTransferCandidates {
        reply: oneshot::Sender<crate::Result<IdleTransferCandidates>>,
    },
    UnreleasedCellCount {
        reply: oneshot::Sender<crate::Result<usize>>,
    },
    ReleaseIdleCell {
        cell: CellId,
        generation: u64,
        reply: oneshot::Sender<crate::Result<()>>,
    },
    ObservePressure {
        sample: PressureSample,
        reply: oneshot::Sender<crate::Result<PressureState>>,
    },
    Shutdown {
        reply: oneshot::Sender<crate::Result<()>>,
    },
}

pub(super) struct QueuedCommand {
    pub(super) telemetry: crate::fleet::telemetry::CellTelemetryHandle,
    pub(super) queued_at: std::time::Instant,
    pub(super) response_proof: Option<(crate::node::log::DurabilitySource, std::time::Duration)>,
    pub(super) cell: CellId,
    pub(super) admission: Arc<CellAdmission>,
    pub(super) operation: QueuedOperation,
    pub(super) now_ms: i64,
    pub(super) max_result_bytes: usize,
    pub(super) handler: Option<Handler>,
    pub(super) reply: Option<oneshot::Sender<crate::Result<StoredOutcome>>>,
    pub(super) _work: WorkAdmission,
}

#[derive(Clone, Copy)]
pub(super) enum QueuedOperation {
    Mutation {
        identity: MutationIdentity,
        operation_digest: Digest,
    },
    Effect {
        delivery: InboxDelivery,
    },
}

impl QueuedOperation {
    pub(super) fn unknown(self, source: Error) -> Error {
        match self {
            Self::Mutation {
                identity,
                operation_digest,
            } => Error::OutcomeUnknown {
                request_id: identity.request_id,
                operation_digest,
                source: Box::new(source),
            },
            Self::Effect { delivery } => Error::EffectOutcomeUnknown {
                effect_id: delivery.effect_id,
                operation_digest: delivery.operation_digest,
                source: Box::new(source),
            },
        }
    }
}

pub(super) struct QueuedQuery {
    pub(super) cell: CellId,
    pub(super) admission: Arc<CellAdmission>,
    pub(super) max_result_bytes: usize,
    pub(super) handler: Option<QueryHandler>,
    pub(super) reply: Option<oneshot::Sender<crate::Result<Vec<u8>>>>,
    pub(super) _work: WorkAdmission,
}

pub(super) struct QueuedResolve {
    pub(super) cell: CellId,
    pub(super) admission: Arc<CellAdmission>,
    pub(super) operation: ResolveOperation,
    pub(super) now_ms: i64,
    pub(super) max_result_bytes: usize,
    pub(super) reply: Option<oneshot::Sender<crate::Result<Resolution>>>,
    pub(super) _work: WorkAdmission,
}

pub(super) struct QueuedMigration {
    pub(super) cell: CellId,
    pub(super) admission: Arc<CellAdmission>,
    pub(super) successor_admission: Arc<CellAdmission>,
    pub(super) plan: MigrationPlan,
    pub(super) now_ms: i64,
    pub(super) reply: Option<oneshot::Sender<crate::Result<MigratedAdmission>>>,
    pub(super) _work: WorkAdmission,
}

pub(super) struct MigratedAdmission {
    pub(super) admission: Arc<CellAdmission>,
    pub(super) outcome: MigrationOutcome,
}

#[derive(Clone, Copy)]
pub(super) enum ResolveOperation {
    Mutation {
        identity: MutationIdentity,
        operation_digest: Digest,
    },
    Effect {
        delivery: InboxDelivery,
    },
}

pub(super) enum QueuedWork {
    Command(Box<QueuedCommand>),
    Query(Box<QueuedQuery>),
    Resolve(Box<QueuedResolve>),
    Migration(Box<QueuedMigration>),
}

pub(super) struct ActiveCell {
    pub(super) generation: u64,
    pub(super) admission: Arc<CellAdmission>,
    pub(super) incarnation: crate::identity::IncarnationId,
    pub(super) code: Digest,
    pub(super) schema: u32,
    pub(super) role: CatalogRole,
    pub(super) catalog: CatalogProof,
    pub(super) interrupt: Arc<crab_ltx::rusqlite::InterruptHandle>,
    pub(super) publisher: Option<CellPublisher>,
    pub(super) durability_submitter: CellDurabilitySubmitter,
    pub(super) publications: VecDeque<QueuedPublication>,
    pub(super) publication_bytes: u64,
    pub(super) unpublished_node_logs: usize,
    pub(super) queue: VecDeque<QueuedWork>,
    pub(super) coordination: CoordinationState,
    pub(super) persisted_work: crate::primitives::maintenance::PersistedWorkInventory,
    pub(super) inventory_refreshing: bool,
    pub(super) drain: Option<oneshot::Sender<crate::Result<()>>>,
    // Transfer closes the old capability and installs a fresh one; failed fresh
    // inventory must leave the current owner serving through that capability.
    pub(super) transfer: Option<TransferPreflight>,
    pub(super) last_used_ms: i64,
    pub(super) last_work_at: std::time::Instant,
    pub(super) compaction_retry_at: std::time::Instant,
    // The published head's due time and commit sequence, mirrored from the
    // authoritative control so a resident Cell can be ticked without a
    // metadata read. Both advance through the same publication that writes
    // control, so a stale reader only produces a `Stale` Tick.
    pub(super) next_due_ms: Option<i64>,
    pub(super) published_sequence: u64,
}

pub(super) struct TransferPreflight {
    pub(super) reply: oneshot::Sender<crate::Result<()>>,
}

pub(super) struct QueuedPublication {
    pub(super) pending: PendingCommit,
    pub(super) durability: Option<PendingDurability>,
    pub(super) retained_reservation: ResourceReservation,
    pub(super) submitted_at: std::time::Instant,
    pub(super) proof: oneshot::Sender<crate::Result<()>>,
}

impl ActiveCell {
    pub(super) fn draining(&self) -> bool {
        self.drain.is_some()
            || self.transfer.is_some()
            || self.coordination.is_draining()
            || self.coordination.is_transfer_preparing()
    }

    pub(super) fn busy(&self) -> bool {
        self.coordination.is_busy()
    }

    pub(super) fn renewing(&self) -> bool {
        self.coordination.is_renewing()
    }

    pub(super) fn begin_task(&mut self, effect: CoordinationEffect) -> u64 {
        self.coordination.begin_effect(effect)
    }

    pub(super) fn finish_task(&mut self, effect_id: u64, effect: CoordinationEffect) -> bool {
        matches!(
            self.coordination
                .step(CoordinationInput::CompleteEffect { effect_id, effect }),
            CoordinationDecision::EffectCompleted
        )
    }
}

pub(super) struct ShutdownState {
    pub(super) reply: oneshot::Sender<crate::Result<()>>,
    pub(super) draining: bool,
    pub(super) error: Option<Error>,
}

pub(super) struct LocalCell {
    pub(super) admission: Arc<CellAdmission>,
    pub(super) incarnation: crate::identity::IncarnationId,
    pub(super) code: Digest,
    pub(super) schema: u32,
}

/// One resident Cell whose published due time has passed.
pub(super) struct DueResidentCell {
    pub(super) cell: CellId,
    pub(super) catalog: CatalogProof,
    pub(super) incarnation: crate::identity::IncarnationId,
    pub(super) code: Digest,
    pub(super) schema: u32,
    pub(super) admission: Arc<CellAdmission>,
    /// Commit sequence the last authoritative publication named.
    pub(super) expected_commit_sequence: u64,
    pub(super) next_due_ms: i64,
}

pub(super) enum TaskResult {
    Activated {
        cell: CellId,
        generation: u64,
        role: CatalogRole,
        catalog: CatalogProof,
        publisher: Box<CellPublisher>,
        admission: Arc<CellAdmission>,
        reply: oneshot::Sender<crate::Result<Arc<CellAdmission>>>,
        result: crate::Result<(
            Arc<crab_ltx::rusqlite::InterruptHandle>,
            Option<crab_ltx::Hydration>,
        )>,
        persisted_work: crate::Result<crate::primitives::maintenance::PersistedWorkInventory>,
    },
    Hydrated {
        cell: CellId,
        generation: u64,
        effect_id: u64,
        result: crate::Result<Option<crab_ltx::Hydration>>,
    },
    InventoryRefreshed {
        cell: CellId,
        generation: u64,
        effect_id: u64,
        result: crate::Result<crate::primitives::maintenance::PersistedWorkInventory>,
    },
    TransferPreflight {
        cell: CellId,
        generation: u64,
        effect_id: u64,
        result: crate::Result<crate::primitives::maintenance::TransferWorkInventory>,
    },
    Executed {
        cell: CellId,
        generation: u64,
        effect_id: u64,
        command: Box<QueuedCommand>,
        result: crate::Result<CommandTaskResult>,
        fenced: bool,
    },
    Proven {
        cell: CellId,
        generation: u64,
        effect_id: u64,
        command: Box<QueuedCommand>,
        result: crate::Result<StoredOutcome>,
        fenced: bool,
    },
    Published {
        cell: CellId,
        generation: u64,
        effect_id: u64,
        publisher: Box<CellPublisher>,
        retained_bytes: u64,
        node_logged: bool,
        next_due_ms: Option<i64>,
        commit_sequence: u64,
        result: crate::Result<()>,
        fenced: bool,
    },
    Compacted {
        cell: CellId,
        generation: u64,
        effect_id: u64,
        publisher: Box<CellPublisher>,
        result: crate::Result<Option<bool>>,
    },
    Queried {
        cell: CellId,
        generation: u64,
        effect_id: u64,
        query: Box<QueuedQuery>,
        result: crate::Result<Vec<u8>>,
        fenced: bool,
    },
    Resolved {
        cell: CellId,
        generation: u64,
        effect_id: u64,
        resolve: Box<QueuedResolve>,
        result: crate::Result<Resolution>,
        fenced: bool,
    },
    Migrated {
        cell: CellId,
        generation: u64,
        effect_id: u64,
        publisher: Box<CellPublisher>,
        migration: Box<QueuedMigration>,
        result: crate::Result<MigrationOutcome>,
        fenced: bool,
        preserve_owner: bool,
        unpublished_bytes: u64,
    },
    Renewed {
        cell: CellId,
        generation: u64,
        effect_id: u64,
        publisher: Box<CellPublisher>,
        result: crate::Result<()>,
    },
    Deactivated {
        cell: CellId,
        generation: u64,
        reply: Option<oneshot::Sender<crate::Result<()>>>,
        shutdown_drain: bool,
        result: crate::Result<()>,
    },
}

pub(super) enum CommandTaskResult {
    Recorded(StoredOutcome),
    Pending {
        pending: Box<PendingCommit>,
        durability: Option<PendingDurability>,
        retained_reservation: ResourceReservation,
    },
}
