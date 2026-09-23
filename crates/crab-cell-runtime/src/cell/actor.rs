use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use tokio::{
    sync::{Semaphore, mpsc, oneshot},
    task::JoinSet,
};

mod handle;

pub use handle::CellHandle;
use handle::{CellAdmission, WorkAdmission};

use crate::cell::executor::{MAX_PENDING_PUBLICATIONS, PENDING_PUBLICATION_HIGH_WATER_BYTES};
use crate::coordination::{
    AdmissionKind, CoordinationDecision, CoordinationEffect, CoordinationInput, CoordinationState,
    RejectReason, Residency,
};
use crate::fleet::eviction::{EvictionObservation, EvictionState, select_victims};
use crate::fleet::pressure::{
    MovementBudget, MovementPermit, PressureClassifier, PressureSample, PressureState,
};
use crate::fleet::resource::{
    ACTIVE_CELL_NATIVE_BYTES as ACTIVE_CELL_NATIVE_BYTES_USIZE, LedgerDiskAdmission,
    LedgerHostResourceAdmission, ResourceCost, ResourceLedger, ResourceReservation,
};
use crate::publication::{CellDurabilitySubmitter, NodeDurabilitySlot, PendingDurability};
use crate::{
    ApplicationId, CatalogEntry, CatalogProof, CatalogRole, CellAuthority, CellId, CellPublisher,
    CellTarget, Digest, Error, InboxDelivery, MigrationOutcome, MigrationPlan, MutationIdentity,
    NodeDurability, NodeLeaseGuard, Owner, PendingCommit, Resolution, SessionId, SqlWorkerPool,
    StoredOutcome, Transition, VersionedControl, WorkerExecution,
    cell::worker::{CellReservation, Handler, Initializer, QueryHandler, WorkerState},
};

const INGRESS_REQUESTS: usize = 1_024;
const CELL_REQUESTS: usize = 64;
// A repository attribution read may reserve a 1 MiB result plus its encoded
// request. Allow a normal burst of concurrent reads; node-wide retained-byte
// admission remains the aggregate safety ceiling.
const CELL_BYTES: usize = 16 * 1024 * 1024;
const RENEWAL_SCAN: std::time::Duration = std::time::Duration::from_millis(100);
const MAX_RENEWALS_IN_FLIGHT: usize = 32;
const SQL_WALL_DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);
const HYDRATION_TICK: std::time::Duration = std::time::Duration::from_millis(100);
const COMPACTION_QUIET: std::time::Duration = std::time::Duration::from_millis(250);
const COMPACTION_RETRY: std::time::Duration = std::time::Duration::from_secs(1);
const HYDRATION_PAGES_PER_STEP: u32 = 64;
const FLEET_PUBLICATION_GRACE: std::time::Duration = std::time::Duration::from_secs(10);

const fn bounded_u32(value: usize) -> u32 {
    if value > u32::MAX as usize {
        u32::MAX
    } else {
        value as u32
    }
}

fn unix_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| i64::try_from(duration.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

/// Conservative per-active-Cell reservation for actor state and native tasks.
///
/// This is admission accounting rather than an RSS guarantee. Embedders must
/// qualify the estimate against their compiled registry and workload.
pub const ACTIVE_CELL_NATIVE_BYTES: u64 = ACTIVE_CELL_NATIVE_BYTES_USIZE as u64;

/// Node-wide dispatcher for bounded per-Cell command mailboxes.
#[derive(Clone)]
pub struct CellRuntime {
    inner: Arc<RuntimeInner>,
}

/// Opaque node-wide byte reservation held until it is dropped.
#[must_use = "dropping the reservation immediately releases its capacity"]
pub struct NodeByteReservation {
    _reservation: ResourceReservation,
}

/// Opaque node-wide worker-job reservation held until a primitive job exits.
#[must_use = "dropping the reservation immediately releases its capacity"]
pub struct NodeJobReservation {
    _reservation: ResourceReservation,
}

/// Point-in-time node admission usage for one embedded Cell runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellRuntimeStats {
    active_cells: usize,
    active_cell_capacity: usize,
    resident_bytes: usize,
    resident_capacity_bytes: usize,
    file_descriptors: usize,
    file_descriptor_capacity: usize,
    retained_bytes: usize,
    retained_capacity_bytes: usize,
    worker_jobs: usize,
    worker_job_capacity: usize,
    primitive_jobs: usize,
    primitive_job_capacity: usize,
    hydration_jobs: usize,
    hydration_job_capacity: usize,
    io_slots: usize,
    io_slot_capacity: usize,
    blocking_jobs: usize,
    blocking_job_capacity: usize,
    recovery_jobs: usize,
    recovery_job_capacity: usize,
    dirty_jobs: usize,
    dirty_job_capacity: usize,
    scratch_units: usize,
    scratch_unit_capacity: usize,
    local_disk_reserved_bytes: u64,
    local_disk_capacity_bytes: u64,
    unpublished_node_log_bytes: u64,
}

impl CellRuntimeStats {
    /// Returns the number of admitted Cell actors.
    #[must_use]
    pub const fn active_cells(self) -> usize {
        self.active_cells
    }

    /// Returns the maximum number of Cell actors admitted by this runtime.
    #[must_use]
    pub const fn active_cell_capacity(self) -> usize {
        self.active_cell_capacity
    }

    /// Returns resident native bytes reserved by active Cells.
    #[must_use]
    pub const fn resident_bytes(self) -> usize {
        self.resident_bytes
    }

    /// Returns the resident native-byte ceiling.
    #[must_use]
    pub const fn resident_capacity_bytes(self) -> usize {
        self.resident_capacity_bytes
    }

    /// Returns file descriptors reserved by active Cells in the shared ledger.
    #[must_use]
    pub const fn file_descriptors(self) -> usize {
        self.file_descriptors
    }

    /// Returns the active-Cell file-descriptor ceiling in the shared ledger.
    #[must_use]
    pub const fn file_descriptor_capacity(self) -> usize {
        self.file_descriptor_capacity
    }

    /// Returns the bytes currently reserved by node-wide native work.
    #[must_use]
    pub const fn retained_bytes(self) -> usize {
        self.retained_bytes
    }

    /// Returns the node-wide native-work byte capacity.
    #[must_use]
    pub const fn retained_capacity_bytes(self) -> usize {
        self.retained_capacity_bytes
    }

    /// Returns SQL jobs currently admitted by the shared ledger.
    #[must_use]
    pub const fn worker_jobs(self) -> usize {
        self.worker_jobs
    }

    /// Returns the SQL-job ceiling in the shared ledger.
    #[must_use]
    pub const fn worker_job_capacity(self) -> usize {
        self.worker_job_capacity
    }

    /// Returns primitive jobs currently admitted by the shared ledger.
    #[must_use]
    pub const fn primitive_jobs(self) -> usize {
        self.primitive_jobs
    }

    /// Returns the primitive-job ceiling in the shared ledger.
    #[must_use]
    pub const fn primitive_job_capacity(self) -> usize {
        self.primitive_job_capacity
    }

    /// Returns background hydration jobs currently admitted by the shared ledger.
    #[must_use]
    pub const fn hydration_jobs(self) -> usize {
        self.hydration_jobs
    }

    /// Returns the background hydration-job ceiling in the shared ledger.
    #[must_use]
    pub const fn hydration_job_capacity(self) -> usize {
        self.hydration_job_capacity
    }

    /// Returns the active-Cell count in the bounded placement wire shape.
    #[must_use]
    pub const fn placement_active_cells(self) -> u32 {
        bounded_u32(self.active_cells)
    }

    /// Returns the active-Cell ceiling in the bounded placement wire shape.
    #[must_use]
    pub const fn placement_active_cell_capacity(self) -> u32 {
        bounded_u32(self.active_cell_capacity)
    }

    /// Returns aggregate admitted worker, primitive, and hydration jobs.
    #[must_use]
    pub const fn placement_running_jobs(self) -> u32 {
        bounded_u32(
            self.worker_jobs
                .saturating_add(self.primitive_jobs)
                .saturating_add(self.hydration_jobs),
        )
    }

    /// Returns aggregate worker, primitive, and hydration job capacity.
    #[must_use]
    pub const fn placement_job_capacity(self) -> u32 {
        bounded_u32(
            self.worker_job_capacity
                .saturating_add(self.primitive_job_capacity)
                .saturating_add(self.hydration_job_capacity),
        )
    }

    /// Returns bounded object-store I/O operations currently admitted.
    #[must_use]
    pub const fn io_slots(self) -> usize {
        self.io_slots
    }

    /// Returns the object-store I/O operation ceiling.
    #[must_use]
    pub const fn io_slot_capacity(self) -> usize {
        self.io_slot_capacity
    }

    /// Returns blocking host jobs currently admitted.
    #[must_use]
    pub const fn blocking_jobs(self) -> usize {
        self.blocking_jobs
    }

    /// Returns the blocking host-job ceiling.
    #[must_use]
    pub const fn blocking_job_capacity(self) -> usize {
        self.blocking_job_capacity
    }

    /// Returns full recovery cohorts currently admitted.
    #[must_use]
    pub const fn recovery_jobs(self) -> usize {
        self.recovery_jobs
    }

    /// Returns the full recovery-cohort ceiling.
    #[must_use]
    pub const fn recovery_job_capacity(self) -> usize {
        self.recovery_job_capacity
    }

    /// Returns dirty-memory cohorts currently admitted.
    #[must_use]
    pub const fn dirty_jobs(self) -> usize {
        self.dirty_jobs
    }

    /// Returns the dirty-memory cohort ceiling.
    #[must_use]
    pub const fn dirty_job_capacity(self) -> usize {
        self.dirty_job_capacity
    }

    /// Returns temporary scratch units currently admitted.
    #[must_use]
    pub const fn scratch_units(self) -> usize {
        self.scratch_units
    }

    /// Returns the temporary scratch-unit ceiling.
    #[must_use]
    pub const fn scratch_unit_capacity(self) -> usize {
        self.scratch_unit_capacity
    }

    /// Returns the bytes currently reserved in the local replica cache.
    #[must_use]
    pub const fn local_disk_reserved_bytes(self) -> u64 {
        self.local_disk_reserved_bytes
    }

    /// Returns the local replica-cache byte capacity.
    #[must_use]
    pub const fn local_disk_capacity_bytes(self) -> u64 {
        self.local_disk_capacity_bytes
    }

    /// Returns owner bytes submitted to node logs but not covered by object roots.
    #[must_use]
    pub const fn unpublished_node_log_bytes(self) -> u64 {
        self.unpublished_node_log_bytes
    }
}

/// New capability and publication receipt returned by one schema migration.
pub struct MigratedCell {
    pub handle: CellHandle,
    pub outcome: MigrationOutcome,
}

pub(super) struct RuntimeInner {
    sender: mpsc::Sender<Message>,
    resources: ResourceLedger,
    shutting_down: AtomicBool,
    accepting_cells: AtomicBool,
    session: SessionId,
    pool: SqlWorkerPool,
    replica_host: crab_ltx::Host,
    node_lease: Arc<RuntimeNodeLease>,
    node_durability: NodeDurabilitySlot,
    telemetry: crate::CellTelemetryHandle,
    unpublished_node_log_bytes: Arc<AtomicU64>,
}

enum RuntimeNodeLease {
    ObjectOnly,
    Required(OnceLock<NodeLeaseGuard>),
}

impl RuntimeNodeLease {
    fn check(&self) -> crate::Result<()> {
        match self {
            Self::ObjectOnly => Ok(()),
            Self::Required(guard) => guard.get().ok_or(Error::Fenced)?.check(),
        }
    }

    fn guard(&self) -> crate::Result<Option<NodeLeaseGuard>> {
        match self {
            Self::ObjectOnly => Ok(None),
            Self::Required(guard) => guard.get().cloned().map(Some).ok_or(Error::Fenced),
        }
    }

    fn install(&self, guard: NodeLeaseGuard) -> crate::Result<()> {
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

impl CellRuntime {
    /// Starts one dispatcher on the current Tokio runtime.
    pub fn new(
        pool: SqlWorkerPool,
        node_retained_bytes: usize,
        session: SessionId,
    ) -> crate::Result<Self> {
        Self::new_with_replica_host(
            pool,
            node_retained_bytes,
            session,
            crab_ltx::Host::default(),
        )
    }

    /// Starts one dispatcher with caller-sized shared replica job admission.
    pub fn new_with_replica_host(
        pool: SqlWorkerPool,
        node_retained_bytes: usize,
        session: SessionId,
        replica_host: crab_ltx::Host,
    ) -> crate::Result<Self> {
        Self::new_inner(
            pool,
            node_retained_bytes,
            session,
            replica_host,
            RuntimeNodeLease::ObjectOnly,
        )
    }

    /// Starts one dispatcher that remains fenced until its node lease is installed.
    pub fn new_with_replica_host_requiring_node_lease(
        pool: SqlWorkerPool,
        node_retained_bytes: usize,
        session: SessionId,
        replica_host: crab_ltx::Host,
    ) -> crate::Result<Self> {
        Self::new_inner(
            pool,
            node_retained_bytes,
            session,
            replica_host,
            RuntimeNodeLease::Required(OnceLock::new()),
        )
    }

    /// Returns the budget shared by this runtime's local replica artifacts.
    ///
    /// Recovery admission must use this same ledger so streamed bundle bytes
    /// cannot bypass WAL, cache, or sparse-page reservations.
    #[must_use]
    pub fn local_disk_budget(&self) -> crab_ltx::DiskBudget {
        self.inner.replica_host.local_disk_budget()
    }

    fn new_inner(
        pool: SqlWorkerPool,
        node_retained_bytes: usize,
        session: SessionId,
        replica_host: crab_ltx::Host,
        node_lease: RuntimeNodeLease,
    ) -> crate::Result<Self> {
        if node_retained_bytes == 0 || node_retained_bytes > Semaphore::MAX_PERMITS {
            return Err(Error::Capacity("node retained bytes"));
        }
        pool.configure_retained_capacity(node_retained_bytes)?;
        let resources = pool.resource_ledger();
        resources.set_disk_limit(replica_host.local_disk_capacity())?;
        resources.set_host_limits(
            replica_host.io_capacity(),
            replica_host.job_capacity(),
            replica_host.recovery_capacity(),
            replica_host.dirty_capacity(),
            replica_host.scratch_capacity() as usize,
        )?;
        let telemetry = crate::CellTelemetryHandle::default();
        let mut replica_host = replica_host.with_ltx_telemetry(Arc::new(telemetry.clone()));
        replica_host.install_resource_admission(Arc::new(LedgerHostResourceAdmission {
            state: resources.weak(),
        }));
        replica_host.install_disk_admission(Arc::new(LedgerDiskAdmission {
            state: resources.weak(),
        }))?;
        let runtime = tokio::runtime::Handle::try_current().map_err(Error::RuntimeStart)?;
        let (sender, receiver) = mpsc::channel(INGRESS_REQUESTS);
        let node_lease = Arc::new(node_lease);
        let unpublished_node_log_bytes = Arc::new(AtomicU64::new(0));
        runtime.spawn(run(
            receiver,
            pool.clone(),
            Arc::clone(&node_lease),
            Arc::clone(&unpublished_node_log_bytes),
        ));
        Ok(Self {
            inner: Arc::new(RuntimeInner {
                sender,
                resources,
                shutting_down: AtomicBool::new(false),
                accepting_cells: AtomicBool::new(true),
                session,
                pool,
                replica_host,
                node_lease,
                node_durability: Arc::new(std::sync::RwLock::new(None)),
                telemetry,
                unpublished_node_log_bytes,
            }),
        })
    }

    /// Installs the process telemetry sink before Cell work begins.
    pub fn install_telemetry(&self, telemetry: Arc<dyn crate::CellTelemetry>) -> crate::Result<()> {
        self.inner.telemetry.install(telemetry)
    }

    /// Returns the shared sink used by node-log components for this runtime.
    #[must_use]
    pub fn telemetry_handle(&self) -> crate::CellTelemetryHandle {
        self.inner.telemetry.clone()
    }

    /// Installs the successfully published process lease before Cell admission opens.
    pub fn install_node_lease(&self, guard: NodeLeaseGuard) -> crate::Result<()> {
        if self.inner.shutting_down.load(Ordering::Acquire) {
            return Err(Error::RuntimeClosed);
        }
        self.inner.node_lease.install(guard)
    }

    /// Installs the one recruited node-log epoch used by newly activated Cells.
    pub fn install_node_durability(
        &self,
        application: ApplicationId,
        durability: Arc<NodeDurability>,
    ) -> crate::Result<()> {
        self.ensure_running()?;
        let mut slot = self
            .inner
            .node_durability
            .write()
            .map_err(|_| Error::Control("Cell runtime node durability lock poisoned"))?;
        if slot.is_some() {
            return Err(Error::Control(
                "Cell runtime node durability was initialized twice",
            ));
        }
        *slot = Some((application, durability));
        Ok(())
    }

    /// Returns the currently installed node-log durability binding.
    #[must_use]
    pub fn node_durability(&self) -> Option<(ApplicationId, Arc<NodeDurability>)> {
        self.inner
            .node_durability
            .read()
            .ok()
            .and_then(|slot| slot.clone())
    }

    /// Replaces the active node-log durability binding after an epoch close.
    pub fn replace_node_durability(
        &self,
        application: ApplicationId,
        durability: Arc<NodeDurability>,
    ) -> crate::Result<Arc<NodeDurability>> {
        self.ensure_running()?;
        let mut slot = self
            .inner
            .node_durability
            .write()
            .map_err(|_| Error::Control("Cell runtime node durability lock poisoned"))?;
        let Some((installed_application, _)) = slot.as_ref() else {
            return Err(Error::Control(
                "Cell runtime node durability is not installed",
            ));
        };
        if *installed_application != application {
            return Err(Error::Control(
                "Cell runtime node durability application changed",
            ));
        }
        let (_, previous) = slot
            .replace((application, durability))
            .ok_or(Error::Control("Cell runtime node durability disappeared"))?;
        Ok(previous)
    }

    /// Stops admission, drains accepted work, closes every Cell, and releases ownership.
    pub async fn shutdown(&self) -> crate::Result<()> {
        if self
            .inner
            .shutting_down
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(Error::RuntimeClosed);
        }
        let (reply, response) = oneshot::channel();
        self.inner
            .sender
            .send(Message::Shutdown { reply })
            .await
            .map_err(|_| Error::RuntimeClosed)?;
        let drain = response.await.map_err(|_| Error::RuntimeClosed)?;
        let workers = self.inner.pool.shutdown().await;
        let durability = match self.node_durability() {
            Some((_, durability)) => durability.shutdown().await,
            None => Ok(()),
        };
        drain.and(workers).and(durability)
    }

    /// Stops new Cell acquisition while existing owners continue serving.
    pub fn stop_acquiring(&self) -> crate::Result<()> {
        self.ensure_running()?;
        self.inner.accepting_cells.store(false, Ordering::Release);
        Ok(())
    }

    /// Reports whether a new owner may be acquired on this node.
    #[must_use]
    pub fn is_acquiring(&self) -> bool {
        !self.inner.shutting_down.load(Ordering::Acquire)
            && self.inner.accepting_cells.load(Ordering::Acquire)
    }

    /// Starts bounded, actor-owned eviction of safe idle Cells.
    ///
    /// The returned count is the number of drains started. Resource
    /// reservations are released only after the worker closes and ownership
    /// release completes; callers must not treat this as immediate capacity.
    pub async fn evict_idle(&self, limit: usize) -> crate::Result<usize> {
        self.ensure_running()?;
        if limit == 0 {
            return Ok(0);
        }
        let (reply, response) = oneshot::channel();
        self.inner
            .sender
            .send(Message::EvictIdle { limit, reply })
            .await
            .map_err(|_| Error::RuntimeClosed)?;
        response.await.map_err(|_| Error::RuntimeClosed)?
    }

    /// Lists currently settled local Cells as advisory transfer candidates.
    /// The exact generation and eligibility are rechecked by `release_idle_cell`.
    pub async fn idle_transfer_candidates(
        &self,
    ) -> crate::Result<Vec<(CellId, u64, i64, CatalogRole)>> {
        self.ensure_running()?;
        let (reply, response) = oneshot::channel();
        self.inner
            .sender
            .send(Message::IdleTransferCandidates { reply })
            .await
            .map_err(|_| Error::RuntimeClosed)?;
        response.await.map_err(|_| Error::RuntimeClosed)?
    }

    /// Counts live and transitioning Cells until their release has completed.
    pub async fn unreleased_cell_count(&self) -> crate::Result<usize> {
        self.ensure_running()?;
        let (reply, response) = oneshot::channel();
        self.inner
            .sender
            .send(Message::UnreleasedCellCount { reply })
            .await
            .map_err(|_| Error::RuntimeClosed)?;
        response.await.map_err(|_| Error::RuntimeClosed)?
    }

    /// Releases one exact local generation only after a fresh settled-work
    /// preflight, actor gate, worker close, and authoritative release complete.
    pub async fn release_idle_cell(
        &self,
        cell: CellId,
        source: SessionId,
        generation: u64,
    ) -> crate::Result<()> {
        self.ensure_running()?;
        if source != self.inner.session || generation == 0 {
            return Err(Error::Fenced);
        }
        let (reply, response) = oneshot::channel();
        self.inner
            .sender
            .send(Message::ReleaseIdleCell {
                cell,
                generation,
                reply,
            })
            .await
            .map_err(|_| Error::RuntimeClosed)?;
        response.await.map_err(|_| Error::RuntimeClosed)?
    }

    /// Feeds one measured node sample into the actor-owned hysteretic pressure
    /// controller. Sustained shedding starts the same bounded idle-eviction
    /// path exposed by [`Self::evict_idle`].
    pub async fn observe_pressure(&self, sample: PressureSample) -> crate::Result<PressureState> {
        self.ensure_running()?;
        let (reply, response) = oneshot::channel();
        self.inner
            .sender
            .send(Message::ObservePressure { sample, reply })
            .await
            .map_err(|_| Error::RuntimeClosed)?;
        response.await.map_err(|_| Error::RuntimeClosed)?
    }

    /// Reports whether node-wide admission has entered its terminal drain.
    #[must_use]
    pub fn is_shutting_down(&self) -> bool {
        self.inner.shutting_down.load(Ordering::Acquire)
    }

    /// Samples node-wide admission usage without waiting for actor work.
    #[must_use]
    pub fn stats(&self) -> CellRuntimeStats {
        let (
            retained,
            retained_capacity,
            resident,
            resident_capacity,
            file_descriptors,
            file_descriptor_capacity,
            worker_jobs,
            worker_job_capacity,
            primitive_jobs,
            primitive_job_capacity,
            hydration_jobs,
            hydration_job_capacity,
            io_slots,
            io_slot_capacity,
            blocking_jobs,
            blocking_job_capacity,
            recovery_jobs,
            recovery_job_capacity,
            dirty_jobs,
            dirty_job_capacity,
            scratch_units,
            scratch_unit_capacity,
            disk_bytes,
            disk_capacity_bytes,
        ) = self
            .inner
            .resources
            .snapshot()
            .map(|snapshot| {
                (
                    snapshot.used.retained_bytes(),
                    snapshot.limit.retained_bytes(),
                    snapshot.used.resident_bytes(),
                    snapshot.limit.resident_bytes(),
                    snapshot.used.file_descriptors(),
                    snapshot.limit.file_descriptors(),
                    snapshot.used.worker_jobs(),
                    snapshot.limit.worker_jobs(),
                    snapshot.used.primitive_jobs(),
                    snapshot.limit.primitive_jobs(),
                    snapshot.used.hydration_jobs(),
                    snapshot.limit.hydration_jobs(),
                    snapshot.used.io_slots(),
                    snapshot.limit.io_slots(),
                    snapshot.used.blocking_jobs(),
                    snapshot.limit.blocking_jobs(),
                    snapshot.used.recovery_jobs(),
                    snapshot.limit.recovery_jobs(),
                    snapshot.used.dirty_jobs(),
                    snapshot.limit.dirty_jobs(),
                    snapshot.used.scratch_units(),
                    snapshot.limit.scratch_units(),
                    snapshot.used.disk_bytes(),
                    snapshot.limit.disk_bytes(),
                )
            })
            .unwrap_or((
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            ));
        CellRuntimeStats {
            active_cells: self.inner.pool.active_cells(),
            active_cell_capacity: self.inner.pool.active_cell_capacity(),
            resident_bytes: resident,
            resident_capacity_bytes: resident_capacity,
            file_descriptors,
            file_descriptor_capacity,
            retained_bytes: retained,
            retained_capacity_bytes: retained_capacity,
            worker_jobs,
            worker_job_capacity,
            primitive_jobs,
            primitive_job_capacity,
            hydration_jobs,
            hydration_job_capacity,
            io_slots,
            io_slot_capacity,
            blocking_jobs,
            blocking_job_capacity,
            recovery_jobs,
            recovery_job_capacity,
            dirty_jobs,
            dirty_job_capacity,
            scratch_units,
            scratch_unit_capacity,
            local_disk_reserved_bytes: disk_bytes,
            local_disk_capacity_bytes: disk_capacity_bytes,
            unpublished_node_log_bytes: self
                .inner
                .unpublished_node_log_bytes
                .load(Ordering::Acquire),
        }
    }

    /// Reserves node-wide bytes for native work retained outside a Cell mailbox.
    ///
    /// Returns a capacity error without waiting when the shared budget is full,
    /// and returns `RuntimeClosed` once terminal drain begins.
    pub fn try_reserve_node_bytes(&self, bytes: usize) -> crate::Result<NodeByteReservation> {
        self.ensure_running()?;
        if bytes == 0 {
            return Err(Error::Capacity("node retained bytes"));
        }
        let reservation = self
            .inner
            .resources
            .try_reserve(ResourceCost::zero().with_retained_bytes(bytes))
            .map_err(|error| match error {
                Error::Capacity(_) => Error::Capacity("node retained bytes"),
                error => error,
            })?;
        Ok(NodeByteReservation {
            _reservation: reservation,
        })
    }

    /// Tries to reserve one worker-job slot from the same ledger as SQL work.
    ///
    /// A full ledger returns `Ok(None)` so schedulers can leave durable work
    /// unclaimed and retry on the next scan. Other failures preserve their
    /// original runtime error.
    pub fn try_reserve_worker_job(&self) -> crate::Result<Option<NodeJobReservation>> {
        self.ensure_running()?;
        match self
            .inner
            .resources
            .try_reserve(ResourceCost::zero().with_primitive_jobs(1))
        {
            Ok(reservation) => Ok(Some(NodeJobReservation {
                _reservation: reservation,
            })),
            Err(Error::Capacity(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Resolves an active local owner without exposing the dispatcher's Cell map.
    pub async fn local_handle(
        &self,
        catalog: CatalogProof,
        control: &VersionedControl,
    ) -> crate::Result<Option<CellHandle>> {
        self.ensure_running()?;
        let value = control.value();
        if catalog.entry().cell() != value.cell {
            return Err(Error::Control("scheduler catalog and control differ"));
        }
        if value
            .owner
            .as_ref()
            .is_none_or(|owner| owner.session != self.inner.session)
            || value.root.is_none()
        {
            return Ok(None);
        }
        let (reply, response) = oneshot::channel();
        self.inner
            .sender
            .send(Message::Lookup {
                cell: value.cell,
                require_resident: false,
                reply,
            })
            .await
            .map_err(|_| Error::RuntimeClosed)?;
        let Some(local) = response.await.map_err(|_| Error::RuntimeClosed)? else {
            return Ok(None);
        };
        if local.incarnation != value.incarnation
            || local.code != value.code
            || local.schema != value.schema
        {
            return Ok(None);
        }
        Ok(Some(CellHandle {
            cell: value.cell,
            incarnation: value.incarnation,
            code: value.code,
            schema: value.schema,
            catalog,
            inner: self.inner.clone(),
            admission: local.admission,
        }))
    }

    /// Creates, initializes and publishes a new Cell before returning a handle.
    pub async fn bootstrap<F>(
        &self,
        catalog: CatalogProof,
        replica: crab_ltx::CellReplica,
        authority: CellAuthority,
        observed: VersionedControl,
        destination: PathBuf,
        initialize: F,
    ) -> crate::Result<CellHandle>
    where
        F: for<'connection> FnOnce(
                &crab_ltx::rusqlite::Transaction<'connection>,
            ) -> crate::Result<()>
            + Send
            + 'static,
    {
        self.ensure_acquiring()?;
        self.activation_cell(&catalog, &observed)?;
        if observed.value().state != crate::ControlState::Recovering
            || observed.value().root.is_some()
        {
            return Err(Error::Control("bootstrap requires an unpublished control"));
        }
        let reservation = self.inner.pool.reserve_activation()?;
        let incarnation = observed.value().incarnation;
        let schema = observed.value().schema;
        self.activate_inner(
            catalog,
            Activation::Bootstrap(Box::new(BootstrapActivation {
                replica: replica.clone(),
                destination,
                incarnation,
                schema,
                initialize: Box::new(initialize),
                reservation,
            })),
            replica,
            authority,
            observed,
        )
        .await
    }

    /// Takes over an unchanged unpublished owner, initializes and publishes the Cell.
    #[expect(
        clippy::too_many_arguments,
        reason = "the takeover boundary keeps every authority and activation input explicit"
    )]
    pub async fn takeover_unpublished<F>(
        &self,
        catalog: CatalogProof,
        replica: crab_ltx::CellReplica,
        authority: CellAuthority,
        mut observed: VersionedControl,
        takeover: crate::NodeTakeoverProof,
        destination: PathBuf,
        owner: Owner,
        initialize: F,
    ) -> crate::Result<CellHandle>
    where
        F: for<'connection> FnOnce(
                &crab_ltx::rusqlite::Transaction<'connection>,
            ) -> crate::Result<()>
            + Send
            + 'static,
    {
        self.ensure_acquiring()?;
        let cell = self.claiming_cell(&catalog, &observed, &owner)?;
        if owner.session != takeover.claimant() {
            return Err(Error::Fenced);
        }
        loop {
            if observed.value().state != crate::ControlState::Recovering
                || observed.value().owner.is_none()
                || observed.value().root.is_some()
            {
                return Err(Error::Control(
                    "unpublished takeover requires an active rootless control",
                ));
            }
            if observed.value().owner.as_ref().map(|owner| owner.session)
                != Some(takeover.session())
            {
                return Err(Error::Fenced);
            }
            self.ensure_running()?;
            let current = authority.load(cell).await?.ok_or(Error::Fenced)?;
            if current.value() != observed.value() {
                self.claiming_cell(&catalog, &current, &owner)?;
                observed = current;
                continue;
            }
            let reservation = self.inner.pool.reserve_activation()?;
            let successor = current.value().takeover(owner.clone())?;
            let claimed = match authority
                .transition(&current, successor.clone(), Transition::Takeover)
                .await
            {
                Ok(claimed) => claimed,
                Err(error) => {
                    let latest = authority.load(cell).await?.ok_or(Error::Fenced)?;
                    if latest.value() == &successor {
                        latest
                    } else if matches!(
                        &error,
                        Error::Storage(crab_storage::StorageError::StateConflict { .. })
                    ) {
                        observed = latest;
                        continue;
                    } else {
                        return Err(error);
                    }
                }
            };
            let incarnation = claimed.value().incarnation;
            let schema = claimed.value().schema;
            return self
                .activate_inner(
                    catalog,
                    Activation::Bootstrap(Box::new(BootstrapActivation {
                        replica: replica.clone(),
                        destination,
                        incarnation,
                        schema,
                        initialize: Box::new(initialize),
                        reservation,
                    })),
                    replica,
                    authority,
                    claimed,
                )
                .await;
        }
    }

    /// Cold-opens the exact authoritative root on the Cell's SQL worker.
    pub async fn activate_restored(
        &self,
        catalog: CatalogProof,
        replica: crab_ltx::CellReplica,
        authority: CellAuthority,
        observed: VersionedControl,
        recovery_store: crate::RecoveryManifestStore,
        destination: PathBuf,
    ) -> crate::Result<CellHandle> {
        self.ensure_acquiring()?;
        self.activation_cell(&catalog, &observed)?;
        let replica = self.replica_with_directory_cache(replica, &destination)?;
        let observed = self
            .publish_attached_recovery(&replica, &authority, observed, &recovery_store)
            .await?;
        let reservation = self.inner.pool.reserve_activation()?;
        self.activate_restored_reserved(
            catalog,
            replica,
            authority,
            observed,
            destination,
            reservation,
        )
        .await
    }

    /// Acquires an idle published Cell and restores its exact immutable root.
    pub async fn acquire_idle_restored(
        &self,
        catalog: CatalogProof,
        replica: crab_ltx::CellReplica,
        authority: CellAuthority,
        observed: VersionedControl,
        destination: PathBuf,
        owner: Owner,
    ) -> crate::Result<CellHandle> {
        self.ensure_acquiring()?;
        let rollback_node_lease = self.inner.node_lease.guard()?;
        self.claiming_cell(&catalog, &observed, &owner)?;
        if observed.value().state != crate::ControlState::Idle
            || observed.value().owner.is_some()
            || observed.value().root.is_none()
        {
            return Err(Error::Control(
                "idle acquisition requires a published idle control",
            ));
        }
        let reservation = self.inner.pool.reserve_activation()?;
        let successor = observed.value().takeover(owner)?;
        let claimed = match authority
            .transition(&observed, successor.clone(), Transition::Takeover)
            .await
        {
            Ok(claimed) => claimed,
            Err(error) => {
                let current = authority
                    .load(observed.value().cell)
                    .await?
                    .ok_or(Error::Fenced)?;
                if current.value() != &successor {
                    return Err(error);
                }
                current
            }
        };
        let rollback_authority = authority.clone();
        let rollback_claim = claimed.clone();
        let rollback_replica = replica.clone();
        match self
            .activate_restored_reserved(
                catalog,
                replica,
                authority,
                claimed,
                destination,
                reservation,
            )
            .await
        {
            Ok(handle) => Ok(handle),
            Err(error) => {
                match rollback_failed_acquisition(
                    &rollback_authority,
                    &rollback_claim,
                    &rollback_replica,
                    rollback_node_lease,
                )
                .await
                {
                    Ok(()) => Err(error),
                    Err(cleanup) => Err(cleanup),
                }
            }
        }
    }

    /// Takes over an unchanged owner after its exact node session is fenced.
    #[expect(
        clippy::too_many_arguments,
        reason = "the takeover boundary keeps every authority, recovery and activation input explicit"
    )]
    pub async fn takeover_restored(
        &self,
        catalog: CatalogProof,
        replica: crab_ltx::CellReplica,
        authority: CellAuthority,
        mut observed: VersionedControl,
        takeover: crate::NodeTakeoverProof,
        recovery_store: crate::RecoveryManifestStore,
        destination: PathBuf,
        owner: Owner,
    ) -> crate::Result<CellHandle> {
        self.ensure_acquiring()?;
        let replica = self.replica_with_directory_cache(replica, &destination)?;
        let rollback_node_lease = self.inner.node_lease.guard()?;
        let rollback_authority = authority.clone();
        let rollback_replica = replica.clone();
        let cell = self.claiming_cell(&catalog, &observed, &owner)?;
        if owner.session != takeover.claimant() {
            return Err(Error::Fenced);
        }
        loop {
            if !matches!(
                observed.value().state,
                crate::ControlState::Recovering | crate::ControlState::Serving
            ) || observed.value().owner.is_none()
                || observed.value().root.is_none()
            {
                return Err(Error::Control(
                    "takeover requires a published control with an active owner",
                ));
            }
            if observed.value().owner.as_ref().map(|owner| owner.session)
                != Some(takeover.session())
            {
                return Err(Error::Fenced);
            }
            self.ensure_running()?;
            let current = authority.load(cell).await?.ok_or(Error::Fenced)?;
            if current.value() != observed.value() {
                self.claiming_cell(&catalog, &current, &owner)?;
                observed = current;
                continue;
            }
            let reservation = self.inner.pool.reserve_activation()?;
            let successor = current.value().takeover(owner.clone())?;
            let claimed = match authority
                .transition(&current, successor.clone(), Transition::Takeover)
                .await
            {
                Ok(claimed) => claimed,
                Err(error) => {
                    let latest = authority.load(cell).await?.ok_or(Error::Fenced)?;
                    if latest.value() == &successor {
                        latest
                    } else if matches!(
                        &error,
                        Error::Storage(crab_storage::StorageError::StateConflict { .. })
                    ) {
                        observed = latest;
                        continue;
                    } else {
                        return Err(error);
                    }
                }
            };
            let recovery_rollback_claim = claimed.clone();
            let claimed = match self
                .publish_attached_recovery(&replica, &authority, claimed, &recovery_store)
                .await
            {
                Ok(claimed) => claimed,
                Err(error) => {
                    match rollback_failed_acquisition(
                        &rollback_authority,
                        &recovery_rollback_claim,
                        &rollback_replica,
                        rollback_node_lease.clone(),
                    )
                    .await
                    {
                        Ok(()) => return Err(error),
                        Err(cleanup) => return Err(cleanup),
                    }
                }
            };
            let rollback_claim = claimed.clone();
            return match self
                .activate_restored_reserved(
                    catalog,
                    replica,
                    authority,
                    claimed,
                    destination,
                    reservation,
                )
                .await
            {
                Ok(handle) => Ok(handle),
                Err(error) => {
                    match rollback_failed_acquisition(
                        &rollback_authority,
                        &rollback_claim,
                        &rollback_replica,
                        rollback_node_lease,
                    )
                    .await
                    {
                        Ok(()) => Err(error),
                        Err(cleanup) => Err(cleanup),
                    }
                }
            };
        }
    }

    async fn publish_attached_recovery(
        &self,
        replica: &crab_ltx::CellReplica,
        authority: &CellAuthority,
        observed: VersionedControl,
        recovery_store: &crate::RecoveryManifestStore,
    ) -> crate::Result<VersionedControl> {
        let Some(recovery) = observed.value().recovery.as_ref() else {
            return Ok(observed);
        };
        let overlay = recovery_store
            .load_overlay(
                observed.value().cell,
                observed.value().incarnation,
                recovery,
            )
            .await?;
        let prepared = replica
            .prepare_recovered_overlay(&overlay, observed.value().schema)
            .await?;
        self.ensure_running()?;
        let successor = observed
            .value()
            .publish_recovery(&prepared, observed.value().next_due_ms)?;
        match authority
            .transition(&observed, successor.clone(), Transition::PublishRecovery)
            .await
        {
            Ok(published) => Ok(published),
            Err(error) => {
                let current = authority
                    .load(observed.value().cell)
                    .await?
                    .ok_or(Error::Fenced)?;
                if current.value() == &successor {
                    Ok(current)
                } else {
                    Err(error)
                }
            }
        }
    }

    async fn activate_restored_reserved(
        &self,
        catalog: CatalogProof,
        replica: crab_ltx::CellReplica,
        authority: CellAuthority,
        observed: VersionedControl,
        destination: PathBuf,
        reservation: CellReservation,
    ) -> crate::Result<CellHandle> {
        // Root verification precedes actor activation, so it must use the same
        // persistent directory cache as the publisher path below.
        let replica = self.replica_with_directory_cache(replica, &destination)?;
        let cell = self.activation_cell(&catalog, &observed)?;
        let control = observed.value();
        let root = control
            .ltx_root()
            .ok_or(Error::Control("activation requires a published root"))?;
        let incarnation = control.incarnation;
        let schema = control.schema;
        let verified = replica.open_root(&root).await?;
        if verified.schema() != schema {
            return Err(Error::Control(
                "immutable root schema does not match control",
            ));
        }
        let database = verified.paged().prepare_writable(&destination).await?;
        let current = authority.load(cell).await?.ok_or(Error::Fenced)?;
        if !current.value().is_same_or_pure_renewal_of(observed.value()) {
            return Err(Error::Fenced);
        }
        self.activate_inner(
            catalog,
            Activation::Restored(Box::new(RestoredActivation {
                database,
                destination,
                incarnation,
                schema,
                root,
                reservation,
            })),
            replica,
            authority,
            current,
        )
        .await
    }

    fn claiming_cell(
        &self,
        catalog: &CatalogProof,
        observed: &VersionedControl,
        owner: &Owner,
    ) -> crate::Result<CellId> {
        let cell = catalog.entry().cell();
        if observed.value().cell != cell {
            return Err(Error::Control("ownership control changed Cell"));
        }
        if owner.session != self.inner.session {
            return Err(Error::Fenced);
        }
        if observed
            .value()
            .owner
            .as_ref()
            .is_some_and(|current| current.session == owner.session)
        {
            return Err(Error::CellAlreadyActive);
        }
        Ok(cell)
    }

    fn activation_cell(
        &self,
        catalog: &CatalogProof,
        observed: &VersionedControl,
    ) -> crate::Result<CellId> {
        let cell = catalog.entry().cell();
        if observed.value().cell != cell {
            return Err(Error::Control("activation control changed Cell"));
        }
        if observed
            .value()
            .owner
            .as_ref()
            .is_none_or(|owner| owner.session != self.inner.session)
        {
            return Err(Error::Fenced);
        }
        Ok(cell)
    }

    fn ensure_running(&self) -> crate::Result<()> {
        if self.inner.shutting_down.load(Ordering::Acquire) {
            return Err(Error::RuntimeClosed);
        }
        self.inner.node_lease.check()
    }

    fn ensure_acquiring(&self) -> crate::Result<()> {
        self.ensure_running()?;
        if !self.inner.accepting_cells.load(Ordering::Acquire) {
            return Err(Error::CellDraining);
        }
        Ok(())
    }

    async fn activate_inner(
        &self,
        catalog: CatalogProof,
        activation: Activation,
        replica: crab_ltx::CellReplica,
        authority: CellAuthority,
        observed: VersionedControl,
    ) -> crate::Result<CellHandle> {
        let cell = self.activation_cell(&catalog, &observed)?;
        let incarnation = observed.value().incarnation;
        let code = observed.value().code;
        let schema = observed.value().schema;
        let scratch_directory = match &activation {
            Activation::Restored(activation) => activation.destination.parent(),
            Activation::Bootstrap(activation) => activation.destination.parent(),
        }
        .ok_or(Error::Control("Cell activation destination has no parent"))?
        .to_owned();
        let replica = self.replica_with_directory_cache(replica, &scratch_directory)?;
        let (reply, response) = oneshot::channel();
        let mut publisher = CellPublisher::new(replica, authority, observed, scratch_directory);
        if let Some(node_lease) = self.inner.node_lease.guard()? {
            publisher = publisher.with_node_lease(node_lease);
        }
        publisher = publisher.with_node_durability_slot(Arc::clone(&self.inner.node_durability));
        publisher = publisher.with_telemetry(self.inner.telemetry.clone());
        self.inner
            .sender
            .send(Message::Activate {
                cell,
                role: catalog.entry().role(),
                activation,
                publisher: Box::new(publisher),
                reply,
            })
            .await
            .map_err(|_| Error::RuntimeClosed)?;
        let admission = response.await.map_err(|_| Error::RuntimeClosed)??;
        Ok(CellHandle {
            cell,
            incarnation,
            code,
            schema,
            catalog,
            inner: self.inner.clone(),
            admission,
        })
    }

    fn replica_with_directory_cache(
        &self,
        replica: crab_ltx::CellReplica,
        destination: &Path,
    ) -> crate::Result<crab_ltx::CellReplica> {
        let scratch_directory = destination
            .parent()
            .ok_or(Error::Control("Cell activation destination has no parent"))?;
        let host = self
            .inner
            .replica_host
            .clone()
            .with_directory_cache(scratch_directory.join(".crab-cell-directory-cache"));
        Ok(replica.with_host(host))
    }

    /// Resolves a verified resident owner without reading catalog or authority objects.
    pub async fn resident_handle(
        &self,
        target: &CellTarget,
        role: CatalogRole,
    ) -> crate::Result<Option<CellHandle>> {
        self.ensure_running()?;
        let (reply, response) = oneshot::channel();
        self.inner
            .sender
            .send(Message::Lookup {
                cell: target.cell_id(),
                require_resident: true,
                reply,
            })
            .await
            .map_err(|_| Error::RuntimeClosed)?;
        let local = match response.await {
            Ok(local) => local,
            Err(_) => {
                self.inner
                    .telemetry
                    .resident_route(crate::ResidentRouteOutcome::Refused);
                return Err(Error::RuntimeClosed);
            }
        };
        let Some(local) = local else {
            self.inner
                .telemetry
                .resident_route(crate::ResidentRouteOutcome::Miss);
            return Ok(None);
        };
        self.inner
            .telemetry
            .resident_route(crate::ResidentRouteOutcome::Hit);
        let entry = CatalogEntry::new(target, role, local.code, local.schema)?;
        Ok(Some(CellHandle {
            cell: target.cell_id(),
            incarnation: local.incarnation,
            code: local.code,
            schema: local.schema,
            catalog: CatalogProof::local(entry),
            inner: self.inner.clone(),
            admission: local.admission,
        }))
    }
}

enum Activation {
    Restored(Box<RestoredActivation>),
    Bootstrap(Box<BootstrapActivation>),
}

struct RestoredActivation {
    database: crab_ltx::CellWritableDatabase,
    destination: PathBuf,
    incarnation: crate::IncarnationId,
    schema: u32,
    root: crab_ltx::RootRef,
    reservation: CellReservation,
}

struct BootstrapActivation {
    replica: crab_ltx::CellReplica,
    destination: PathBuf,
    incarnation: crate::IncarnationId,
    schema: u32,
    initialize: Initializer,
    reservation: CellReservation,
}

type IdleTransferCandidates = Vec<(CellId, u64, i64, CatalogRole)>;

enum Message {
    Activate {
        cell: CellId,
        role: CatalogRole,
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

struct QueuedCommand {
    cell: CellId,
    admission: Arc<CellAdmission>,
    operation: QueuedOperation,
    now_ms: i64,
    max_result_bytes: usize,
    handler: Option<Handler>,
    reply: Option<oneshot::Sender<crate::Result<StoredOutcome>>>,
    _work: WorkAdmission,
}

#[derive(Clone, Copy)]
enum QueuedOperation {
    Mutation {
        identity: MutationIdentity,
        operation_digest: Digest,
    },
    Effect {
        delivery: InboxDelivery,
    },
}

impl QueuedOperation {
    fn unknown(self, source: Error) -> Error {
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

struct QueuedQuery {
    cell: CellId,
    admission: Arc<CellAdmission>,
    max_result_bytes: usize,
    handler: Option<QueryHandler>,
    reply: Option<oneshot::Sender<crate::Result<Vec<u8>>>>,
    _work: WorkAdmission,
}

struct QueuedResolve {
    cell: CellId,
    admission: Arc<CellAdmission>,
    operation: ResolveOperation,
    now_ms: i64,
    max_result_bytes: usize,
    reply: Option<oneshot::Sender<crate::Result<Resolution>>>,
    _work: WorkAdmission,
}

struct QueuedMigration {
    cell: CellId,
    admission: Arc<CellAdmission>,
    successor_admission: Arc<CellAdmission>,
    plan: MigrationPlan,
    now_ms: i64,
    reply: Option<oneshot::Sender<crate::Result<MigratedAdmission>>>,
    _work: WorkAdmission,
}

struct MigratedAdmission {
    admission: Arc<CellAdmission>,
    outcome: MigrationOutcome,
}

#[derive(Clone, Copy)]
enum ResolveOperation {
    Mutation {
        identity: MutationIdentity,
        operation_digest: Digest,
    },
    Effect {
        delivery: InboxDelivery,
    },
}

enum QueuedWork {
    Command(Box<QueuedCommand>),
    Query(Box<QueuedQuery>),
    Resolve(Box<QueuedResolve>),
    Migration(Box<QueuedMigration>),
}

struct ActiveCell {
    generation: u64,
    admission: Arc<CellAdmission>,
    incarnation: crate::IncarnationId,
    code: Digest,
    schema: u32,
    role: CatalogRole,
    interrupt: Arc<crab_ltx::rusqlite::InterruptHandle>,
    publisher: Option<CellPublisher>,
    durability_submitter: CellDurabilitySubmitter,
    publications: VecDeque<QueuedPublication>,
    publication_bytes: u64,
    unpublished_node_logs: usize,
    queue: VecDeque<QueuedWork>,
    coordination: CoordinationState,
    persisted_work: crate::PersistedWorkInventory,
    inventory_refreshing: bool,
    drain: Option<oneshot::Sender<crate::Result<()>>>,
    // Transfer closes the old capability and installs a fresh one; failed fresh
    // inventory must leave the current owner serving through that capability.
    transfer: Option<TransferPreflight>,
    last_used_ms: i64,
    last_work_at: std::time::Instant,
    compaction_retry_at: std::time::Instant,
}

struct TransferPreflight {
    reply: oneshot::Sender<crate::Result<()>>,
}

struct QueuedPublication {
    pending: PendingCommit,
    durability: Option<PendingDurability>,
    retained_reservation: ResourceReservation,
    submitted_at: std::time::Instant,
    proof: oneshot::Sender<crate::Result<()>>,
}

impl ActiveCell {
    fn draining(&self) -> bool {
        self.drain.is_some()
            || self.transfer.is_some()
            || self.coordination.is_draining()
            || self.coordination.is_transfer_preparing()
    }

    fn busy(&self) -> bool {
        self.coordination.is_busy()
    }

    fn renewing(&self) -> bool {
        self.coordination.is_renewing()
    }

    fn begin_task(&mut self, effect: CoordinationEffect) -> u64 {
        self.coordination.begin_effect(effect)
    }

    fn finish_task(&mut self, effect_id: u64, effect: CoordinationEffect) -> bool {
        matches!(
            self.coordination
                .step(CoordinationInput::CompleteEffect { effect_id, effect }),
            CoordinationDecision::EffectCompleted
        )
    }
}

struct ShutdownState {
    reply: oneshot::Sender<crate::Result<()>>,
    draining: bool,
    error: Option<Error>,
}

struct LocalCell {
    admission: Arc<CellAdmission>,
    incarnation: crate::IncarnationId,
    code: Digest,
    schema: u32,
}

enum TaskResult {
    Activated {
        cell: CellId,
        generation: u64,
        role: CatalogRole,
        publisher: Box<CellPublisher>,
        admission: Arc<CellAdmission>,
        reply: oneshot::Sender<crate::Result<Arc<CellAdmission>>>,
        result: crate::Result<(
            Arc<crab_ltx::rusqlite::InterruptHandle>,
            Option<crab_ltx::Hydration>,
        )>,
        persisted_work: crate::Result<crate::PersistedWorkInventory>,
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
        result: crate::Result<crate::PersistedWorkInventory>,
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

enum CommandTaskResult {
    Recorded(StoredOutcome),
    Pending {
        pending: Box<PendingCommit>,
        durability: Option<PendingDurability>,
        retained_reservation: ResourceReservation,
    },
}

async fn run(
    mut receiver: mpsc::Receiver<Message>,
    pool: SqlWorkerPool,
    node_lease: Arc<RuntimeNodeLease>,
    unpublished_node_log_bytes: Arc<AtomicU64>,
) {
    let mut cells = HashMap::<CellId, ActiveCell>::new();
    let mut transitioning = HashSet::<CellId>::new();
    let mut tasks = JoinSet::<TaskResult>::new();
    let mut next_generation = 0_u64;
    let mut shutdown = None::<ShutdownState>;
    let mut pressure = match PressureClassifier::new(800, 600, 1_000) {
        Ok(classifier) => classifier,
        Err(_) => return,
    };
    let mut movement = match MovementBudget::new(2, 1_000) {
        Ok(budget) => budget,
        Err(_) => return,
    };
    let mut movement_permits = HashMap::<CellId, MovementPermit>::new();
    let mut renewal_tick = tokio::time::interval(RENEWAL_SCAN);
    renewal_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    renewal_tick.tick().await;
    let mut hydration_tick = tokio::time::interval(HYDRATION_TICK);
    hydration_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    hydration_tick.tick().await;
    loop {
        if shutdown.as_ref().is_some_and(|state| state.draining) {
            if tasks.is_empty() {
                if !cells.is_empty() || !transitioning.is_empty() {
                    fail_shutdown(
                        &mut shutdown,
                        Error::Control("Cell shutdown left local state without a task"),
                    );
                }
                finish_shutdown(&mut shutdown);
                return;
            }
            let Some(Ok(result)) = tasks.join_next().await else {
                fail_shutdown(&mut shutdown, Error::RuntimeClosed);
                finish_shutdown(&mut shutdown);
                return;
            };
            handle_task(
                result,
                &pool,
                &mut cells,
                &mut transitioning,
                &mut tasks,
                &mut shutdown,
                &node_lease,
                &unpublished_node_log_bytes,
                &mut movement,
                &mut movement_permits,
            );
            continue;
        }
        if tasks.is_empty() {
            tokio::select! {
                message = receiver.recv() => {
                    let Some(message) = message else {
                        if shutdown.is_some() {
                            start_shutdown_drain(
                                &pool,
                                &mut cells,
                                &mut transitioning,
                                &mut tasks,
                                &mut shutdown,
                                &node_lease,
                            );
                            continue;
                        }
                        break;
                    };
                    handle_message(message, &mut receiver, &pool, &mut cells, &mut transitioning, &mut tasks, &mut shutdown, &node_lease, &mut pressure, &mut movement, &mut movement_permits, &mut next_generation);
                }
                _ = renewal_tick.tick() => {
                    start_due_renewals(&pool, &mut cells, &mut tasks, &node_lease);
                }
                _ = hydration_tick.tick() => {
                    start_background_hydration(
                        &pool,
                        &mut cells,
                        &mut tasks,
                        &node_lease,
                    );
                    start_background_inventory(&pool, &mut cells, &mut tasks, &node_lease);
                    start_background_compaction(&pool, &mut cells, &mut tasks, &node_lease);
                }
            }
            continue;
        }
        tokio::select! {
            message = receiver.recv() => {
                let Some(message) = message else {
                    if shutdown.is_some() {
                        start_shutdown_drain(
                            &pool,
                            &mut cells,
                            &mut transitioning,
                            &mut tasks,
                            &mut shutdown,
                            &node_lease,
                        );
                        continue;
                    }
                    while let Some(result) = tasks.join_next().await {
                        let Ok(result) = result else { return; };
                        handle_task(result, &pool, &mut cells, &mut transitioning, &mut tasks, &mut shutdown, &node_lease, &unpublished_node_log_bytes, &mut movement, &mut movement_permits);
                    }
                    break;
                };
                    handle_message(message, &mut receiver, &pool, &mut cells, &mut transitioning, &mut tasks, &mut shutdown, &node_lease, &mut pressure, &mut movement, &mut movement_permits, &mut next_generation);
            }
            result = tasks.join_next() => {
                let Some(Ok(result)) = result else {
                    return;
                };
                handle_task(result, &pool, &mut cells, &mut transitioning, &mut tasks, &mut shutdown, &node_lease, &unpublished_node_log_bytes, &mut movement, &mut movement_permits);
            }
            _ = renewal_tick.tick() => {
                start_due_renewals(&pool, &mut cells, &mut tasks, &node_lease);
            }
            _ = hydration_tick.tick() => {
                start_background_hydration(
                    &pool,
                    &mut cells,
                    &mut tasks,
                    &node_lease,
                );
                start_background_inventory(&pool, &mut cells, &mut tasks, &node_lease);
                start_background_compaction(&pool, &mut cells, &mut tasks, &node_lease);
            }
        }
    }
}

fn start_shutdown_drain(
    pool: &SqlWorkerPool,
    cells: &mut HashMap<CellId, ActiveCell>,
    transitioning: &mut HashSet<CellId>,
    tasks: &mut JoinSet<TaskResult>,
    shutdown: &mut Option<ShutdownState>,
    node_lease: &RuntimeNodeLease,
) {
    let Some(state) = shutdown.as_mut() else {
        return;
    };
    if state.draining {
        return;
    }
    state.draining = true;
    let mut ready = Vec::new();
    for (cell, active) in cells.iter_mut() {
        if let Some(transfer) = active.transfer.take() {
            active.coordination.step(CoordinationInput::AbortTransfer);
            let _ = transfer.reply.send(Err(Error::CellDraining));
        }
        active.inventory_refreshing = false;
        active.coordination.step(CoordinationInput::BeginShutdown);
        active.admission.draining.store(true, Ordering::Release);
        active.admission.requests.close();
        active.admission.bytes.close();
        if !active.queue.is_empty() {
            start_next(active, pool, tasks, node_lease);
        }
        match schedule(active, node_lease.check().is_ok()) {
            CoordinationDecision::ReadyToDeactivate => {
                ready.push((*cell, false, active.unpublished_node_logs != 0));
            }
            CoordinationDecision::ReadyToDeactivateFenced => {
                ready.push((*cell, true, active.unpublished_node_logs != 0));
            }
            CoordinationDecision::Fence => fence_active(active),
            _ => {}
        }
    }
    for (cell, fenced, preserve_owner) in ready {
        if fenced {
            start_fenced_deactivate(cell, pool, cells, transitioning, tasks, preserve_owner);
        } else {
            start_deactivate(cell, pool, cells, transitioning, tasks);
        }
    }
}

fn fail_shutdown(shutdown: &mut Option<ShutdownState>, error: Error) {
    if let Some(state) = shutdown.as_mut()
        && state.error.is_none()
    {
        state.error = Some(error);
    }
}

fn finish_shutdown(shutdown: &mut Option<ShutdownState>) {
    let Some(mut state) = shutdown.take() else {
        return;
    };
    let result = state.error.take().map_or(Ok(()), Err);
    let _ = state.reply.send(result);
}

#[expect(
    clippy::too_many_arguments,
    reason = "the actor adapter passes each independently owned protocol facility explicitly"
)]
fn handle_message(
    message: Message,
    receiver: &mut mpsc::Receiver<Message>,
    pool: &SqlWorkerPool,
    cells: &mut HashMap<CellId, ActiveCell>,
    transitioning: &mut HashSet<CellId>,
    tasks: &mut JoinSet<TaskResult>,
    shutdown: &mut Option<ShutdownState>,
    node_lease: &RuntimeNodeLease,
    pressure: &mut PressureClassifier,
    movement: &mut MovementBudget,
    movement_permits: &mut HashMap<CellId, MovementPermit>,
    next_generation: &mut u64,
) {
    if !matches!(message, Message::Shutdown { .. }) && node_lease.check().is_err() {
        for active in cells.values_mut() {
            active.coordination.step(CoordinationInput::Fence);
            fence_active(active);
        }
        reject_fenced_message(message);
        return;
    }
    match message {
        Message::Activate {
            cell,
            role,
            activation,
            publisher,
            reply,
        } => {
            if cells.contains_key(&cell) || !transitioning.insert(cell) {
                let _ = reply.send(Err(Error::CellAlreadyActive));
                return;
            }
            *next_generation = next_generation.wrapping_add(1).max(1);
            let generation = *next_generation;
            let admission = new_cell_admission();
            let pool = pool.clone();
            tasks.spawn(async move {
                let mut publisher = publisher;
                let result = match activation {
                    Activation::Restored(activation) => {
                        activate_restored_and_publish(cell, &pool, &mut publisher, *activation)
                            .await
                    }
                    Activation::Bootstrap(activation) => {
                        bootstrap_and_publish(cell, &pool, &mut publisher, *activation).await
                    }
                };
                let (result, persisted_work) = match result {
                    Ok(hydration) => {
                        match pool.interrupt_handle(cell).await {
                            Ok(interrupt) => {
                                let persisted_work =
                                    pool.persisted_work_inventory(cell, role).await;
                                (Ok((Arc::new(interrupt), hydration)), persisted_work)
                            }
                            Err(error) => {
                                let result =
                                    match cleanup_failed_activation(cell, &pool, &mut publisher)
                                        .await
                                    {
                                        Ok(()) => error,
                                        Err(cleanup) => cleanup,
                                    };
                                (Err(result), Err(Error::CellNotActive))
                            }
                        }
                    }
                    Err(error) => {
                        let result =
                            match cleanup_failed_activation(cell, &pool, &mut publisher).await {
                                Ok(()) => error,
                                Err(cleanup) => cleanup,
                            };
                        (Err(result), Err(Error::CellNotActive))
                    }
                };
                TaskResult::Activated {
                    cell,
                    generation,
                    role,
                    publisher,
                    admission,
                    reply,
                    result,
                    persisted_work,
                }
            });
        }
        Message::Execute(mut command) => {
            let Some(active) = cells.get_mut(&command.cell) else {
                send_command_reply(&mut command, Err(Error::CellNotActive));
                return;
            };
            if active.transfer.is_some() {
                send_command_reply(&mut command, Err(Error::CellDraining));
                return;
            }
            match active.coordination.step(CoordinationInput::Admit {
                kind: AdmissionKind::Command,
                admission_matches: Arc::ptr_eq(&active.admission, &command.admission),
            }) {
                CoordinationDecision::Admit => {
                    active.queue.push_back(QueuedWork::Command(command));
                    start_next(active, pool, tasks, node_lease);
                }
                CoordinationDecision::Reject(reason) => {
                    send_command_reply(&mut command, Err(rejection_error(reason)));
                }
                _ => send_command_reply(&mut command, Err(Error::CellNotActive)),
            }
        }
        Message::Query(mut query) => {
            let Some(active) = cells.get_mut(&query.cell) else {
                send_query_reply(&mut query, Err(Error::CellNotActive));
                return;
            };
            if active.transfer.is_some() {
                send_query_reply(&mut query, Err(Error::CellDraining));
                return;
            }
            match active.coordination.step(CoordinationInput::Admit {
                kind: AdmissionKind::Query,
                admission_matches: Arc::ptr_eq(&active.admission, &query.admission),
            }) {
                CoordinationDecision::Admit => {
                    active.queue.push_back(QueuedWork::Query(query));
                    start_next(active, pool, tasks, node_lease);
                }
                CoordinationDecision::Reject(reason) => {
                    send_query_reply(&mut query, Err(rejection_error(reason)));
                }
                _ => send_query_reply(&mut query, Err(Error::CellNotActive)),
            }
        }
        Message::Resolve(mut resolve) => {
            let Some(active) = cells.get_mut(&resolve.cell) else {
                send_resolve_reply(&mut resolve, Err(Error::CellNotActive));
                return;
            };
            if active.transfer.is_some() {
                send_resolve_reply(&mut resolve, Ok(Resolution::Unknown));
                return;
            }
            match active.coordination.step(CoordinationInput::Admit {
                kind: AdmissionKind::Resolve,
                admission_matches: Arc::ptr_eq(&active.admission, &resolve.admission),
            }) {
                CoordinationDecision::Admit => {
                    active.queue.push_back(QueuedWork::Resolve(resolve));
                    start_next(active, pool, tasks, node_lease);
                }
                CoordinationDecision::ResolveUnknown => {
                    send_resolve_reply(&mut resolve, Ok(Resolution::Unknown));
                }
                CoordinationDecision::Reject(reason) => {
                    send_resolve_reply(&mut resolve, Err(rejection_error(reason)));
                }
                _ => send_resolve_reply(&mut resolve, Err(Error::CellNotActive)),
            }
        }
        Message::Migrate(mut migration) => {
            let Some(active) = cells.get_mut(&migration.cell) else {
                send_migration_reply(&mut migration, Err(Error::CellNotActive));
                return;
            };
            let decision = active.coordination.step(CoordinationInput::Admit {
                kind: AdmissionKind::Migration,
                admission_matches: Arc::ptr_eq(&active.admission, &migration.admission),
            });
            if let CoordinationDecision::Reject(reason) = decision {
                send_migration_reply(&mut migration, Err(rejection_error(reason)));
                return;
            }
            if !matches!(decision, CoordinationDecision::Admit) {
                send_migration_reply(&mut migration, Err(Error::CellNotActive));
                return;
            }
            if active.code != migration.plan.from_code()
                || active.schema != migration.plan.from_schema()
            {
                send_migration_reply(
                    &mut migration,
                    Err(Error::Registry("migration plan does not match active Cell")),
                );
                return;
            }
            match active.coordination.step(CoordinationInput::BeginMigration) {
                CoordinationDecision::Started => {}
                CoordinationDecision::Reject(reason) => {
                    send_migration_reply(&mut migration, Err(rejection_error(reason)));
                    return;
                }
                _ => {
                    send_migration_reply(&mut migration, Err(Error::CellDraining));
                    return;
                }
            }
            active.admission = Arc::clone(&migration.successor_admission);
            active.queue.push_back(QueuedWork::Migration(migration));
            start_next(active, pool, tasks, node_lease);
        }
        Message::Lookup {
            cell,
            require_resident,
            reply,
        } => {
            let local = cells.get(&cell).and_then(|active| {
                matches!(
                    active.coordination.lookup(),
                    CoordinationDecision::LocalHandle
                )
                .then_some(active)
                .filter(|active| {
                    active.transfer.is_none()
                        && (!require_resident
                            || active.coordination.residency() == Residency::Resident)
                })
                .map(|active| LocalCell {
                    admission: active.admission.clone(),
                    incarnation: active.incarnation,
                    code: active.code,
                    schema: active.schema,
                })
            });
            let _ = reply.send(local);
        }
        Message::Drain {
            cell,
            admission,
            reply,
        } => {
            let Some(active) = cells.get_mut(&cell) else {
                let _ = reply.send(Err(Error::CellNotActive));
                return;
            };
            if !Arc::ptr_eq(&active.admission, &admission) {
                let _ = reply.send(Err(if active.transfer.is_some() {
                    Error::CellDraining
                } else {
                    Error::CellNotActive
                }));
                return;
            }
            if active.transfer.is_some() {
                if let Some(transfer) = active.transfer.take() {
                    active.coordination.step(CoordinationInput::AbortTransfer);
                    let _ = transfer.reply.send(Err(Error::CellDraining));
                }
                let decision = active.coordination.step(CoordinationInput::BeginDrain);
                if let CoordinationDecision::Reject(reason) = decision {
                    let _ = reply.send(Err(rejection_error(reason)));
                    return;
                }
                active.drain = Some(reply);
                if matches!(
                    schedule(active, node_lease.check().is_ok()),
                    CoordinationDecision::ReadyToDeactivate
                ) {
                    start_deactivate(cell, pool, cells, transitioning, tasks);
                }
                return;
            }
            let decision = active.coordination.step(CoordinationInput::BeginDrain);
            if let CoordinationDecision::Reject(reason) = decision {
                let _ = reply.send(Err(rejection_error(reason)));
                return;
            }
            active.drain = Some(reply);
            match schedule(active, node_lease.check().is_ok()) {
                CoordinationDecision::ReadyToDeactivate => {
                    start_deactivate(cell, pool, cells, transitioning, tasks);
                }
                CoordinationDecision::Fence => fence_active(active),
                _ => {}
            }
        }
        Message::EvictIdle { limit, reply } => {
            let count = start_bounded_evictions(
                limit,
                unix_millis(),
                pool,
                cells,
                transitioning,
                tasks,
                movement,
                movement_permits,
            );
            let _ = reply.send(Ok(count));
        }
        Message::IdleTransferCandidates { reply } => {
            if node_lease.check().is_err() {
                let _ = reply.send(Err(Error::Fenced));
                return;
            }
            let candidates = cells
                .iter()
                .filter_map(|(cell, active)| {
                    transfer_candidate_observation(*cell, active)
                        .eligible()
                        .then_some(active)
                        .filter(|active| !active.draining())
                        .map(|active| (*cell, active.generation, active.last_used_ms, active.role))
                })
                .collect();
            let _ = reply.send(Ok(candidates));
        }
        Message::UnreleasedCellCount { reply } => {
            let _ = reply.send(Ok(cells.len().saturating_add(transitioning.len())));
        }
        Message::ReleaseIdleCell {
            cell,
            generation,
            reply,
        } => {
            if node_lease.check().is_err() {
                let _ = reply.send(Err(Error::Fenced));
                return;
            }
            let Some(active) = cells.get(&cell) else {
                let _ = reply.send(Err(Error::CellNotActive));
                return;
            };
            if active.generation != generation
                || active.draining()
                || active.transfer.is_some()
                || active.inventory_refreshing
            {
                let _ = reply.send(Err(Error::CellDraining));
                return;
            }
            let Ok(mut permit) = movement.try_start(unix_millis()) else {
                let _ = reply.send(Err(Error::Capacity("movement budget")));
                return;
            };
            {
                let Some(active) = cells.get_mut(&cell) else {
                    movement.complete(&mut permit);
                    let _ = reply.send(Err(Error::CellNotActive));
                    return;
                };
                if active.transfer.is_some()
                    || active.drain.is_some()
                    || active.inventory_refreshing
                {
                    movement.complete(&mut permit);
                    let _ = reply.send(Err(Error::CellDraining));
                    return;
                }
                let decision =
                    active
                        .coordination
                        .step(CoordinationInput::BeginTransferPreflight {
                            queue_empty: active.queue.is_empty(),
                            publication_idle: active.coordination.publication_count() == 0,
                            lease_live: node_lease.check().is_ok(),
                        });
                match decision {
                    CoordinationDecision::Started => {}
                    CoordinationDecision::Fence => {
                        fence_active(active);
                        movement.complete(&mut permit);
                        let _ = reply.send(Err(Error::Fenced));
                        return;
                    }
                    CoordinationDecision::Reject(reason) => {
                        movement.complete(&mut permit);
                        let _ = reply.send(Err(rejection_error(reason)));
                        return;
                    }
                    CoordinationDecision::Ignored => {
                        movement.complete(&mut permit);
                        let _ = reply.send(Err(Error::CellDraining));
                        return;
                    }
                    _ => {
                        movement.complete(&mut permit);
                        let _ = reply.send(Err(Error::CellDraining));
                        return;
                    }
                }
                active.admission.draining.store(true, Ordering::Release);
                active.admission.requests.close();
                active.admission.bytes.close();
                active.admission = new_cell_admission();
                active.transfer = Some(TransferPreflight { reply });
            };
            movement_permits.insert(cell, permit);
            continue_cell(cell, pool, cells, transitioning, tasks, node_lease);
        }
        Message::ObservePressure { sample, reply } => {
            let result = pressure.observe(sample);
            if let Ok(state) = result
                && matches!(state, PressureState::Shedding | PressureState::Critical)
                && movement_permits.len() < 2
            {
                let _ = start_bounded_evictions(
                    1,
                    sample.at_ms,
                    pool,
                    cells,
                    transitioning,
                    tasks,
                    movement,
                    movement_permits,
                );
            }
            let _ = reply.send(result);
        }
        Message::Shutdown { reply } => {
            if shutdown.is_some() {
                let _ = reply.send(Err(Error::RuntimeClosed));
                return;
            }
            receiver.close();
            *shutdown = Some(ShutdownState {
                reply,
                draining: false,
                error: None,
            });
        }
    }
}

fn reject_fenced_message(message: Message) {
    match message {
        Message::Activate { reply, .. } => {
            let _ = reply.send(Err(Error::Fenced));
        }
        Message::Execute(mut command) => {
            send_command_reply(&mut command, Err(Error::Fenced));
        }
        Message::Query(mut query) => {
            send_query_reply(&mut query, Err(Error::Fenced));
        }
        Message::Resolve(mut resolve) => {
            send_resolve_reply(&mut resolve, Ok(Resolution::Unknown));
        }
        Message::Migrate(mut migration) => {
            send_migration_reply(&mut migration, Err(Error::Fenced));
        }
        Message::Lookup { reply, .. } => {
            let _ = reply.send(None);
        }
        Message::Drain { reply, .. } => {
            let _ = reply.send(Err(Error::Fenced));
        }
        Message::EvictIdle { reply, .. } => {
            let _ = reply.send(Err(Error::Fenced));
        }
        Message::IdleTransferCandidates { reply } => {
            let _ = reply.send(Err(Error::Fenced));
        }
        Message::UnreleasedCellCount { reply } => {
            let _ = reply.send(Err(Error::Fenced));
        }
        Message::ReleaseIdleCell { reply, .. } => {
            let _ = reply.send(Err(Error::Fenced));
        }
        Message::ObservePressure { reply, .. } => {
            let _ = reply.send(Err(Error::Fenced));
        }
        Message::Shutdown { reply } => {
            let _ = reply.send(Err(Error::RuntimeClosed));
        }
    }
}

fn start_bounded_evictions(
    limit: usize,
    now_ms: i64,
    pool: &SqlWorkerPool,
    cells: &mut HashMap<CellId, ActiveCell>,
    transitioning: &mut HashSet<CellId>,
    tasks: &mut JoinSet<TaskResult>,
    movement: &mut MovementBudget,
    movement_permits: &mut HashMap<CellId, MovementPermit>,
) -> usize {
    let mut started = 0;
    for _ in 0..limit {
        let Ok(permit) = movement.try_start(now_ms) else {
            break;
        };
        let mut selected = begin_idle_evictions(1, pool, cells, transitioning, tasks);
        let Some(cell) = selected.pop() else {
            let mut permit = permit;
            movement.complete(&mut permit);
            break;
        };
        movement_permits.insert(cell, permit);
        started += 1;
    }
    started
}

fn begin_idle_evictions(
    limit: usize,
    pool: &SqlWorkerPool,
    cells: &mut HashMap<CellId, ActiveCell>,
    transitioning: &mut HashSet<CellId>,
    tasks: &mut JoinSet<TaskResult>,
) -> Vec<CellId> {
    let observations = cells
        .iter()
        .map(|(cell, active)| eviction_observation(*cell, active))
        .collect::<Vec<_>>();
    let victims = select_victims(&observations, limit);
    let mut started = Vec::new();
    for cell in victims {
        if begin_idle_cell_eviction(cell, pool, cells, transitioning, tasks, None) {
            started.push(cell);
        }
    }
    started
}

fn eviction_observation(cell: CellId, active: &ActiveCell) -> EvictionObservation {
    EvictionObservation {
        cell,
        state: if active.draining() {
            EvictionState::Quiescing
        } else {
            EvictionState::Idle
        },
        last_used_ms: active.last_used_ms,
        cost: ResourceCost::active_cell()
            .with_retained_bytes(usize::try_from(active.publication_bytes).unwrap_or(usize::MAX)),
        busy: active.busy() || active.renewing() || active.transfer.is_some(),
        retained_obligation: active.coordination.publication_count() != 0,
        migrating: active.publisher.is_none(),
        backup_pinned: active.unpublished_node_logs != 0,
        leased_work: !active.queue.is_empty(),
        primitive_obligation: !active.persisted_work.is_transfer_settled(),
        accounting_known: !active.persisted_work.is_unknown(),
    }
}

fn transfer_candidate_observation(cell: CellId, active: &ActiveCell) -> EvictionObservation {
    let mut observation = eviction_observation(cell, active);
    if !active.persisted_work.is_unknown() {
        observation.primitive_obligation = false;
    }
    observation
}

fn transfer_observation(
    cell: CellId,
    active: &ActiveCell,
    inventory: crate::primitives::maintenance::TransferWorkInventory,
) -> EvictionObservation {
    let mut observation = eviction_observation(cell, active);
    observation.primitive_obligation = !inventory.is_settled();
    observation.accounting_known = true;
    observation
}

fn begin_idle_cell_eviction(
    cell: CellId,
    pool: &SqlWorkerPool,
    cells: &mut HashMap<CellId, ActiveCell>,
    transitioning: &mut HashSet<CellId>,
    tasks: &mut JoinSet<TaskResult>,
    reply: Option<oneshot::Sender<crate::Result<()>>>,
) -> bool {
    let Some(active) = cells.get_mut(&cell) else {
        if let Some(reply) = reply {
            let _ = reply.send(Err(Error::CellNotActive));
        }
        return false;
    };
    let decision = active.coordination.step(CoordinationInput::BeginDrain);
    if matches!(decision, CoordinationDecision::Reject(_)) {
        if let Some(reply) = reply {
            let _ = reply.send(Err(Error::CellDraining));
        }
        return false;
    }
    active.admission.draining.store(true, Ordering::Release);
    active.admission.requests.close();
    active.admission.bytes.close();
    active.drain = reply;
    if matches!(
        schedule(active, true),
        CoordinationDecision::ReadyToDeactivate
    ) {
        start_deactivate(cell, pool, cells, transitioning, tasks);
    }
    true
}

async fn activate_restored_and_publish(
    cell: CellId,
    pool: &SqlWorkerPool,
    publisher: &mut CellPublisher,
    activation: RestoredActivation,
) -> crate::Result<Option<crab_ltx::Hydration>> {
    let RestoredActivation {
        database,
        destination,
        incarnation,
        schema,
        root,
        reservation,
    } = activation;
    pool.activate_restored(
        cell,
        database,
        destination,
        incarnation,
        schema,
        root,
        reservation,
    )
    .await?;
    if let Err(error) = publisher.activate().await {
        return match pool.deactivate(cell).await {
            Ok(()) => Err(error),
            Err(cleanup) => Err(cleanup),
        };
    }
    pool.hydration(cell).await
}

async fn bootstrap_and_publish(
    cell: CellId,
    pool: &SqlWorkerPool,
    publisher: &mut CellPublisher,
    activation: BootstrapActivation,
) -> crate::Result<Option<crab_ltx::Hydration>> {
    let BootstrapActivation {
        replica,
        destination,
        incarnation,
        schema,
        initialize,
        reservation,
    } = activation;
    let bootstrap = pool.bootstrap(
        cell,
        replica,
        destination,
        incarnation,
        schema,
        initialize,
        reservation,
    );
    tokio::pin!(bootstrap);
    let mut renewal_error = None;
    let bootstrap = loop {
        tokio::select! {
            result = &mut bootstrap => break result,
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(publisher.renewal_at())) => {
                if let Err(error) = publisher.renew().await {
                    renewal_error = Some(error);
                    break bootstrap.await;
                }
            }
        }
    };
    if let Some(error) = renewal_error {
        return match bootstrap {
            Ok(_) => match pool.deactivate(cell).await {
                Ok(()) => Err(error),
                Err(cleanup) => Err(cleanup),
            },
            Err(bootstrap) => Err(bootstrap),
        };
    }
    let bootstrap = bootstrap?;
    let publication = async {
        let prepared = publisher.prepare_initial(&bootstrap.cuts).await?;
        publisher
            .publish_prepared(&prepared, bootstrap.next_due_ms)
            .await?;
        pool.confirm_bootstrap_published(cell, bootstrap.cuts)
            .await?;
        Ok(())
    }
    .await;
    if let Err(error) = publication {
        return match pool.deactivate(cell).await {
            Ok(()) => Err(error),
            Err(cleanup) => Err(cleanup),
        };
    }
    Ok(None)
}

fn start_background_hydration(
    pool: &SqlWorkerPool,
    cells: &mut HashMap<CellId, ActiveCell>,
    tasks: &mut JoinSet<TaskResult>,
    node_lease: &RuntimeNodeLease,
) {
    if node_lease.check().is_err() {
        for active in cells.values_mut() {
            active.coordination.step(CoordinationInput::Fence);
            fence_active(active);
        }
        return;
    }
    let resources = pool.resource_ledger();
    let candidates = cells
        .iter_mut()
        .filter_map(|(cell, active)| {
            let reservation = resources
                .try_reserve(ResourceCost::zero().with_hydration_jobs(1))
                .ok()?;
            match active.coordination.step(CoordinationInput::BeginHydration {
                queue_empty: active.queue.is_empty(),
                publication_idle: active.coordination.publication_count() == 0,
                lease_live: node_lease.check().is_ok(),
            }) {
                CoordinationDecision::Started => {}
                CoordinationDecision::Fence => {
                    drop(reservation);
                    fence_active(active);
                    return None;
                }
                _ => {
                    drop(reservation);
                    return None;
                }
            }
            let effect_id = active.begin_task(CoordinationEffect::Hydration);
            Some((*cell, active.generation, effect_id, reservation))
        })
        .collect::<Vec<_>>();

    for (cell, generation, effect_id, reservation) in candidates {
        let pool = pool.clone();
        tasks.spawn(async move {
            let _reservation = reservation;
            let deadline = std::time::Instant::now() + SQL_WALL_DEADLINE;
            let result = tokio::time::timeout_at(
                deadline.into(),
                pool.hydrate(cell, HYDRATION_PAGES_PER_STEP, deadline),
            )
            .await
            .map_err(|_| Error::Deadline)
            .and_then(|result| result);
            if result.is_err() {
                let _ = pool.fence(cell).await;
            }
            TaskResult::Hydrated {
                cell,
                generation,
                effect_id,
                result,
            }
        });
    }
}

fn start_background_inventory(
    pool: &SqlWorkerPool,
    cells: &mut HashMap<CellId, ActiveCell>,
    tasks: &mut JoinSet<TaskResult>,
    node_lease: &RuntimeNodeLease,
) {
    let candidates = cells
        .iter_mut()
        .filter_map(|(cell, active)| {
            let decision = active.coordination.step(CoordinationInput::BeginInventory {
                queue_empty: active.queue.is_empty(),
                publication_idle: active.coordination.publication_count() == 0,
                inventory_unknown: active.persisted_work.is_unknown(),
                refreshing: active.inventory_refreshing,
                lease_live: node_lease.check().is_ok(),
            });
            if matches!(decision, CoordinationDecision::Fence) {
                fence_active(active);
                return None;
            }
            if !matches!(decision, CoordinationDecision::Started) {
                return None;
            }
            let effect_id = active.begin_task(CoordinationEffect::Inventory);
            active.inventory_refreshing = true;
            Some((*cell, active.generation, active.role, effect_id))
        })
        .collect::<Vec<_>>();

    for (cell, generation, role, effect_id) in candidates {
        let pool = pool.clone();
        tasks.spawn(async move {
            let deadline = std::time::Instant::now() + SQL_WALL_DEADLINE;
            let result =
                tokio::time::timeout_at(deadline.into(), pool.persisted_work_inventory(cell, role))
                    .await
                    .map_err(|_| Error::Deadline)
                    .and_then(|result| result);
            TaskResult::InventoryRefreshed {
                cell,
                generation,
                effect_id,
                result,
            }
        });
    }
}

fn start_background_compaction(
    pool: &SqlWorkerPool,
    cells: &mut HashMap<CellId, ActiveCell>,
    tasks: &mut JoinSet<TaskResult>,
    node_lease: &RuntimeNodeLease,
) {
    let now = std::time::Instant::now();
    for (cell, active) in cells {
        if now.duration_since(active.last_work_at) < COMPACTION_QUIET
            || now < active.compaction_retry_at
        {
            continue;
        }
        let due = active
            .publisher
            .as_ref()
            .is_some_and(CellPublisher::compaction_due);
        let decision = active
            .coordination
            .step(CoordinationInput::BeginCompaction {
                queue_empty: active.queue.is_empty(),
                publication_idle: active.coordination.publication_count() == 0,
                publisher_ready: active.publisher.is_some(),
                due,
                lease_live: node_lease.check().is_ok(),
            });
        if matches!(decision, CoordinationDecision::Fence) {
            fence_active(active);
            continue;
        }
        if !matches!(decision, CoordinationDecision::Started) {
            continue;
        }
        let Some(mut publisher) = active.publisher.take() else {
            active
                .coordination
                .step(CoordinationInput::FinishCompaction { fenced: true });
            fence_active(active);
            continue;
        };
        let cell = *cell;
        let generation = active.generation;
        let effect_id = active.begin_task(CoordinationEffect::Compaction);
        let pool = pool.clone();
        tasks.spawn(async move {
            let started = std::time::Instant::now();
            let result = publisher.compact_one_quiet().await;
            tracing::debug!(
                elapsed_ms = started.elapsed().as_millis(),
                promoted = matches!(result, Ok(Some(true))),
                retry = matches!(result, Ok(None)),
                succeeded = result.is_ok(),
                "Cell LTX quiet compaction completed"
            );
            if result.is_err() {
                let _ = pool.fence(cell).await;
            }
            TaskResult::Compacted {
                cell,
                generation,
                effect_id,
                publisher: Box::new(publisher),
                result,
            }
        });
    }
}

fn schedule(active: &mut ActiveCell, lease_live: bool) -> CoordinationDecision {
    let publication_blocked = active.queue.front().is_some_and(|work| {
        matches!(work, QueuedWork::Command(_))
            && (active.coordination.publication_count() >= MAX_PENDING_PUBLICATIONS
                || active.publication_bytes >= PENDING_PUBLICATION_HIGH_WATER_BYTES)
    });
    active.coordination.step(CoordinationInput::Schedule {
        queue_empty: active.queue.is_empty(),
        publisher_ready: active.publisher.is_some(),
        publication_blocked,
        lease_live,
    })
}

fn start_next(
    active: &mut ActiveCell,
    pool: &SqlWorkerPool,
    tasks: &mut JoinSet<TaskResult>,
    node_lease: &RuntimeNodeLease,
) {
    let decision = schedule(active, node_lease.check().is_ok());
    if matches!(decision, CoordinationDecision::Fence) {
        fence_active(active);
        return;
    }
    if !matches!(decision, CoordinationDecision::StartQueuedWork) {
        return;
    }
    let Some(work) = active.queue.pop_front() else {
        return;
    };
    if matches!(&work, QueuedWork::Command(_) | QueuedWork::Migration(_)) {
        // Durable command outcomes, effects, Queue rows, and Workflow runs
        // remain release obligations until a fresh inventory proves otherwise.
        active.persisted_work = crate::PersistedWorkInventory::unknown();
    }
    let generation = active.generation;
    active.last_used_ms = unix_millis();
    active.last_work_at = std::time::Instant::now();
    let kind = match &work {
        QueuedWork::Command(_) => AdmissionKind::Command,
        QueuedWork::Query(_) => AdmissionKind::Query,
        QueuedWork::Resolve(_) => AdmissionKind::Resolve,
        QueuedWork::Migration(_) => AdmissionKind::Migration,
    };
    if !matches!(
        active.coordination.step(CoordinationInput::BeginWork {
            kind,
            publisher_ready: active.publisher.is_some(),
        }),
        CoordinationDecision::Started
    ) {
        active.queue.push_front(work);
        return;
    }
    let pool = pool.clone();
    let interrupt = active.interrupt.clone();
    match work {
        QueuedWork::Command(command) => {
            let durability = active.durability_submitter.clone();
            let effect_id = active.begin_task(CoordinationEffect::Work(kind));
            tasks.spawn(async move {
                execute_command(pool, durability, command, interrupt, generation, effect_id).await
            });
        }
        QueuedWork::Query(query) => {
            let effect_id = active.begin_task(CoordinationEffect::Work(kind));
            tasks.spawn(async move {
                execute_query(pool, query, interrupt, generation, effect_id).await
            });
        }
        QueuedWork::Resolve(resolve) => {
            let effect_id = active.begin_task(CoordinationEffect::Work(kind));
            tasks.spawn(async move {
                execute_resolve(pool, resolve, interrupt, generation, effect_id).await
            });
        }
        QueuedWork::Migration(migration) => {
            let Some(publisher) = active.publisher.take() else {
                let mut migration = migration;
                send_migration_reply(&mut migration, Err(Error::Fenced));
                finish_migration(active, true);
                return;
            };
            let effect_id = active.begin_task(CoordinationEffect::Work(kind));
            tasks.spawn(async move {
                execute_migration(
                    pool,
                    Box::new(publisher),
                    migration,
                    interrupt,
                    generation,
                    effect_id,
                )
                .await
            });
        }
    }
}

async fn execute_migration(
    pool: SqlWorkerPool,
    mut publisher: Box<CellPublisher>,
    mut migration: Box<QueuedMigration>,
    interrupt: Arc<crab_ltx::rusqlite::InterruptHandle>,
    generation: u64,
    effect_id: u64,
) -> TaskResult {
    let mut preserve_owner = false;
    let mut unpublished_bytes = 0;
    let deadline = std::time::Instant::now() + SQL_WALL_DEADLINE;
    let operation = pool.migrate(migration.cell, migration.plan, migration.now_ms, deadline);
    tokio::pin!(operation);
    let pending = match tokio::time::timeout_at(deadline.into(), &mut operation).await {
        Ok(result) => result,
        Err(_) => {
            interrupt.interrupt();
            fence_admission(&migration.admission);
            send_migration_reply(&mut migration, Err(Error::Deadline));
            let _ = operation.await;
            let _ = pool.fence(migration.cell).await;
            return TaskResult::Migrated {
                cell: migration.cell,
                generation,
                effect_id,
                publisher,
                migration,
                result: Err(Error::Deadline),
                fenced: true,
                preserve_owner: false,
                unpublished_bytes: 0,
            };
        }
    };
    let result = match pending {
        Ok(pending) => {
            let durability_started = std::time::Instant::now();
            let durability = publisher.submit_migration_durability(&pending).await;
            match durability {
                Err(error) => Err(error),
                Ok(durability) => {
                    let node_logged = durability.is_some();
                    let retained_bytes = pending.retained_bytes();
                    let cell = migration.cell;
                    let early_outcome = MigrationOutcome {
                        code: pending.code(),
                        schema: pending.to_schema(),
                        commit_sequence: pending.commit_sequence(),
                    };
                    let object = async {
                        let prepared = publisher.prepare_migration(&pending).await?;
                        pool.bind_migration_prepared(cell, prepared.clone()).await?;
                        let root = publisher
                            .publish_migration(
                                &prepared,
                                pending.next_due_ms(),
                                pending.code(),
                                pending.to_schema(),
                            )
                            .await?;
                        if let Some(durability) = durability.as_ref() {
                            durability.prove_object().await?;
                        } else {
                            publisher.record_object_proof(durability_started.elapsed());
                        }
                        pool.confirm_migration_published(cell, root).await
                    };
                    tokio::pin!(object);
                    let result = match durability.as_ref() {
                        Some(durability) => {
                            let fleet = durability.prove_fleet();
                            tokio::pin!(fleet);
                            tokio::select! {
                                result = &mut object => result,
                                fleet = &mut fleet => {
                                    if fleet.is_ok() {
                                        let admission = Arc::clone(&migration.successor_admission);
                                        send_migration_reply(
                                            &mut migration,
                                            Ok(MigratedAdmission {
                                                admission,
                                                outcome: early_outcome,
                                            }),
                                        );
                                    }
                                    object.await
                                }
                            }
                        }
                        None => object.await,
                    };
                    preserve_owner = node_logged && result.is_err();
                    if preserve_owner {
                        unpublished_bytes = retained_bytes;
                    }
                    result
                }
            }
        }
        Err(error) => Err(error),
    };
    let fenced = result.is_err();
    if fenced {
        let _ = pool.fence(migration.cell).await;
    }
    TaskResult::Migrated {
        cell: migration.cell,
        generation,
        effect_id,
        publisher,
        migration,
        result,
        fenced,
        preserve_owner,
        unpublished_bytes,
    }
}

async fn execute_command(
    pool: SqlWorkerPool,
    durability: CellDurabilitySubmitter,
    mut command: Box<QueuedCommand>,
    interrupt: Arc<crab_ltx::rusqlite::InterruptHandle>,
    generation: u64,
    effect_id: u64,
) -> TaskResult {
    let deadline = std::time::Instant::now() + SQL_WALL_DEADLINE;
    let execution = match command.handler.take() {
        Some(handler) => {
            let cell = command.cell;
            let queued_operation = command.operation;
            let now_ms = command.now_ms;
            let max_result_bytes = command.max_result_bytes;
            let worker_pool = pool.clone();
            let operation = async move {
                match queued_operation {
                    QueuedOperation::Mutation {
                        identity,
                        operation_digest,
                    } => {
                        worker_pool
                            .execute_until(
                                cell,
                                identity,
                                operation_digest,
                                now_ms,
                                max_result_bytes,
                                deadline,
                                handler,
                            )
                            .await
                    }
                    QueuedOperation::Effect { delivery } => {
                        worker_pool
                            .deliver_effect(
                                cell,
                                delivery,
                                now_ms,
                                max_result_bytes,
                                deadline,
                                handler,
                            )
                            .await
                    }
                }
            };
            tokio::pin!(operation);
            match tokio::time::timeout_at(deadline.into(), &mut operation).await {
                Ok(result) => result,
                Err(_) => {
                    interrupt.interrupt();
                    fence_admission(&command.admission);
                    let error = command.operation.unknown(Error::Deadline);
                    send_command_reply(&mut command, Err(error));
                    let _ = operation.await;
                    let _ = pool.fence(command.cell).await;
                    return TaskResult::Executed {
                        cell: command.cell,
                        generation,
                        effect_id,
                        command,
                        result: Err(Error::Deadline),
                        fenced: true,
                    };
                }
            }
        }
        None => Err(Error::Fenced),
    };
    let (result, must_fence) = match execution {
        Ok(WorkerExecution::Recorded(outcome)) => (Ok(CommandTaskResult::Recorded(outcome)), false),
        Ok(WorkerExecution::Pending(pending)) => {
            let retained_bytes = usize::try_from(pending.retained_bytes())
                .map_err(|_| Error::Capacity("pending publication bytes"));
            let result = retained_bytes.and_then(|retained_bytes| {
                pool.resource_ledger()
                    .try_reserve(ResourceCost::zero().with_retained_bytes(retained_bytes))
                    .map_err(|error| match error {
                        Error::Capacity(_) => Error::Capacity("pending publication bytes"),
                        error => error,
                    })
            });
            let result = match result {
                Ok(retained_reservation) => durability
                    .submit(pending.outcome().commit_sequence(), pending.cuts())
                    .await
                    .map(|durability| CommandTaskResult::Pending {
                        pending,
                        durability,
                        retained_reservation,
                    }),
                Err(error) => Err(error),
            };
            (result, true)
        }
        Err(error) => (
            Err(error),
            !matches!(pool.state(command.cell).await, Ok(WorkerState::Ready)),
        ),
    };
    let fenced = must_fence && result.is_err();
    if fenced {
        let _ = pool.fence(command.cell).await;
    }
    let result = if fenced {
        result.map_err(|source| command.operation.unknown(source))
    } else {
        result
    };
    TaskResult::Executed {
        cell: command.cell,
        generation,
        effect_id,
        command,
        result,
        fenced,
    }
}

async fn prove_command(
    pool: SqlWorkerPool,
    command: Box<QueuedCommand>,
    outcome: StoredOutcome,
    commit_sequence: u64,
    durability: Option<PendingDurability>,
    mut object: oneshot::Receiver<crate::Result<()>>,
    generation: u64,
    effect_id: u64,
) -> TaskResult {
    let proof = match durability {
        Some(durability) => {
            let fleet_or_object = durability.prove();
            tokio::pin!(fleet_or_object);
            tokio::select! {
                object = &mut object => match receive_publication_proof(object) {
                    Ok(()) => Ok(()),
                    // Object publication failure does not invalidate an
                    // independently fsynced follower proof for this cut.
                    Err(_) => fleet_or_object.await.map(|_| ()),
                },
                result = &mut fleet_or_object => match result {
                    Ok(()) => Ok(()),
                    // Losing the follower path does not invalidate the same
                    // cut's object publication, which remains the fallback.
                    Err(_) => receive_publication_proof(object.await),
                }
            }
        }
        None => receive_publication_proof(object.await),
    };
    let result = match proof {
        Ok(()) => pool
            .confirm_durable(command.cell, commit_sequence)
            .await
            .map(|()| outcome),
        Err(error) => Err(error),
    };
    let fenced = result.is_err();
    if fenced {
        let _ = pool.fence(command.cell).await;
    }
    let result = if fenced {
        result.map_err(|source| command.operation.unknown(source))
    } else {
        result
    };
    TaskResult::Proven {
        cell: command.cell,
        generation,
        effect_id,
        command,
        result,
        fenced,
    }
}

fn receive_publication_proof(
    result: std::result::Result<crate::Result<()>, oneshot::error::RecvError>,
) -> crate::Result<()> {
    result.map_err(|_| Error::RuntimeClosed)?
}

fn start_publication(
    cell: CellId,
    active: &mut ActiveCell,
    pool: &SqlWorkerPool,
    tasks: &mut JoinSet<TaskResult>,
) {
    let Some(mut publisher) = active.publisher.take() else {
        return;
    };
    let Some(publication) = active.publications.pop_front() else {
        active.publisher = Some(publisher);
        return;
    };
    let node_logged = publication.durability.is_some();
    tracing::debug!(
        queue_wait_ms = publication.submitted_at.elapsed().as_millis(),
        pending_publications = active.coordination.publication_count(),
        publication_bytes = active.publication_bytes,
        "Cell LTX publication started"
    );
    let generation = active.generation;
    let effect_id = active.begin_task(CoordinationEffect::Publication);
    // Moving the publisher out of ActiveCell is the serialization token for
    // root preparation and CAS; no second object publisher can overtake it.
    let pool = pool.clone();
    let retained_reservation = publication.retained_reservation;
    tasks.spawn(async move {
        let _retained_reservation = retained_reservation;
        let retained_bytes = publication.pending.retained_bytes();
        let mut publication_proof = Some(publication.proof);
        let fleet_deadline = std::time::Instant::now() + FLEET_PUBLICATION_GRACE;
        let mut retry_delay = std::time::Duration::from_millis(100);
        let result = async {
            let expected = publication.pending.outcome().clone();
            let prepared = loop {
                match publisher.prepare(&publication.pending).await {
                    Ok(prepared) => break prepared,
                    Err(error) if node_logged && is_storage_publication_error(&error) => {
                        if let Some(durability) = publication.durability.as_ref() {
                            wait_for_fleet_proof(durability, fleet_deadline).await?;
                        }
                        if let Some(proof) = publication_proof.take() {
                            let _ = proof.send(Err(error));
                        }
                        if std::time::Instant::now() >= fleet_deadline {
                            return Err(Error::Fenced);
                        }
                        tokio::time::sleep(retry_delay).await;
                        retry_delay = retry_delay
                            .saturating_mul(2)
                            .min(std::time::Duration::from_secs(2));
                    }
                    Err(error) => return Err(error),
                }
            };
            pool.bind_prepared(cell, prepared.clone()).await?;
            let root = loop {
                match publisher
                    .publish_prepared(&prepared, publication.pending.next_due_ms())
                    .await
                {
                    Ok(root) => break root,
                    Err(error) if node_logged && is_storage_publication_error(&error) => {
                        if let Some(durability) = publication.durability.as_ref() {
                            wait_for_fleet_proof(durability, fleet_deadline).await?;
                        }
                        if let Some(proof) = publication_proof.take() {
                            let _ = proof.send(Err(error));
                        }
                        if std::time::Instant::now() >= fleet_deadline {
                            return Err(Error::Fenced);
                        }
                        tokio::time::sleep(retry_delay).await;
                        retry_delay = retry_delay
                            .saturating_mul(2)
                            .min(std::time::Duration::from_secs(2));
                    }
                    Err(error) => return Err(error),
                }
            };
            if let Some(durability) = publication.durability.as_ref() {
                durability.prove_object().await?;
            } else {
                publisher.record_object_proof(publication.submitted_at.elapsed());
            }
            let published = pool.confirm_published(cell, root).await?;
            if published != expected {
                return Err(Error::Control(
                    "published result does not match queued commit",
                ));
            }
            Ok(())
        }
        .await;
        tracing::debug!(
            publication_lag_ms = publication.submitted_at.elapsed().as_millis(),
            succeeded = result.is_ok(),
            "Cell LTX publication completed"
        );
        let fenced = result.is_err();
        if let Some(proof) = publication_proof {
            let _ = proof.send(if fenced { Err(Error::Fenced) } else { Ok(()) });
        }
        if fenced {
            let _ = pool.fence(cell).await;
        }
        TaskResult::Published {
            cell,
            generation,
            effect_id,
            publisher: Box::new(publisher),
            retained_bytes,
            node_logged,
            result,
            fenced,
        }
    });
}

fn is_storage_publication_error(error: &Error) -> bool {
    matches!(
        error,
        Error::Storage(_) | Error::Ltx(crab_ltx::CrabError::Storage(_))
    )
}

async fn wait_for_fleet_proof(
    durability: &PendingDurability,
    deadline: std::time::Instant,
) -> crate::Result<()> {
    tokio::time::timeout_at(
        tokio::time::Instant::from_std(deadline),
        durability.prove_fleet(),
    )
    .await
    .map_err(|_| Error::Fenced)?
}

async fn execute_query(
    pool: SqlWorkerPool,
    mut query: Box<QueuedQuery>,
    interrupt: Arc<crab_ltx::rusqlite::InterruptHandle>,
    generation: u64,
    effect_id: u64,
) -> TaskResult {
    let deadline = std::time::Instant::now() + SQL_WALL_DEADLINE;
    let result = match query.handler.take() {
        Some(handler) => {
            let operation = pool.query(query.cell, query.max_result_bytes, deadline, handler);
            tokio::pin!(operation);
            match tokio::time::timeout_at(deadline.into(), &mut operation).await {
                Ok(result) => result,
                Err(_) => {
                    interrupt.interrupt();
                    fence_admission(&query.admission);
                    send_query_reply(&mut query, Err(Error::Deadline));
                    let _ = operation.await;
                    let _ = pool.fence(query.cell).await;
                    return TaskResult::Queried {
                        cell: query.cell,
                        generation,
                        effect_id,
                        query,
                        result: Err(Error::Deadline),
                        fenced: true,
                    };
                }
            }
        }
        None => Err(Error::Fenced),
    };
    let fenced = result.is_err() && !matches!(pool.state(query.cell).await, Ok(WorkerState::Ready));
    if fenced {
        let _ = pool.fence(query.cell).await;
    }
    TaskResult::Queried {
        cell: query.cell,
        generation,
        effect_id,
        query,
        result,
        fenced,
    }
}

async fn execute_resolve(
    pool: SqlWorkerPool,
    mut resolve: Box<QueuedResolve>,
    interrupt: Arc<crab_ltx::rusqlite::InterruptHandle>,
    generation: u64,
    effect_id: u64,
) -> TaskResult {
    let deadline = std::time::Instant::now() + SQL_WALL_DEADLINE;
    let cell = resolve.cell;
    let resolve_operation = resolve.operation;
    let now_ms = resolve.now_ms;
    let max_result_bytes = resolve.max_result_bytes;
    let worker_pool = pool.clone();
    let operation = async move {
        match resolve_operation {
            ResolveOperation::Mutation {
                identity,
                operation_digest,
            } => {
                worker_pool
                    .resolve(
                        cell,
                        identity,
                        operation_digest,
                        now_ms,
                        max_result_bytes,
                        deadline,
                    )
                    .await
            }
            ResolveOperation::Effect { delivery } => {
                worker_pool
                    .resolve_effect(cell, delivery, now_ms, max_result_bytes, deadline)
                    .await
            }
        }
    };
    tokio::pin!(operation);
    let result = match tokio::time::timeout_at(deadline.into(), &mut operation).await {
        Ok(result) => result,
        Err(_) => {
            interrupt.interrupt();
            fence_admission(&resolve.admission);
            send_resolve_reply(&mut resolve, Ok(Resolution::Unknown));
            let _ = operation.await;
            let _ = pool.fence(resolve.cell).await;
            return TaskResult::Resolved {
                cell: resolve.cell,
                generation,
                effect_id,
                resolve,
                result: Ok(Resolution::Unknown),
                fenced: true,
            };
        }
    };
    let deadline = matches!(result, Err(Error::Deadline));
    let result = if deadline {
        Ok(Resolution::Unknown)
    } else {
        result
    };
    let fenced = deadline
        || result.is_err() && !matches!(pool.state(resolve.cell).await, Ok(WorkerState::Ready));
    if fenced {
        let _ = pool.fence(resolve.cell).await;
    }
    TaskResult::Resolved {
        cell: resolve.cell,
        generation,
        effect_id,
        resolve,
        result,
        fenced,
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "the actor adapter passes each independently owned protocol facility explicitly"
)]
fn handle_task(
    result: TaskResult,
    pool: &SqlWorkerPool,
    cells: &mut HashMap<CellId, ActiveCell>,
    transitioning: &mut HashSet<CellId>,
    tasks: &mut JoinSet<TaskResult>,
    shutdown: &mut Option<ShutdownState>,
    node_lease: &RuntimeNodeLease,
    unpublished_node_log_bytes: &AtomicU64,
    movement: &mut MovementBudget,
    movement_permits: &mut HashMap<CellId, MovementPermit>,
) {
    match result {
        TaskResult::Activated {
            cell,
            generation,
            role,
            publisher,
            admission,
            reply,
            result,
            persisted_work,
        } => match result {
            Ok((interrupt, hydration)) => {
                if node_lease.check().is_err() {
                    fence_admission(&admission);
                    let _ = reply.send(Err(Error::Fenced));
                    start_orphan_deactivate(
                        cell,
                        pool,
                        *publisher,
                        transitioning,
                        tasks,
                        false,
                        generation,
                    );
                    return;
                }
                if shutdown.as_ref().is_some_and(|state| state.draining) {
                    admission.draining.store(true, Ordering::Release);
                    admission.requests.close();
                    admission.bytes.close();
                    let _ = reply.send(Err(Error::RuntimeClosed));
                    start_orphan_deactivate(
                        cell,
                        pool,
                        *publisher,
                        transitioning,
                        tasks,
                        true,
                        generation,
                    );
                    return;
                }
                if reply.send(Ok(admission.clone())).is_err() {
                    start_orphan_deactivate(
                        cell,
                        pool,
                        *publisher,
                        transitioning,
                        tasks,
                        false,
                        generation,
                    );
                    return;
                }
                transitioning.remove(&cell);
                let control = publisher.control().value();
                let incarnation = control.incarnation;
                let code = control.code;
                let schema = control.schema;
                let durability_submitter = publisher.durability_submitter();
                let residency = hydration.map_or(Residency::Resident, |progress| {
                    if progress.complete() {
                        Residency::Resident
                    } else {
                        Residency::Sparse
                    }
                });
                let persisted_work = match persisted_work {
                    Ok(inventory) => inventory,
                    Err(_) => {
                        // An inventory read is a safety precondition for
                        // eviction. Unknown accounting must remain ineligible.
                        crate::PersistedWorkInventory::unknown()
                    }
                };
                cells.insert(
                    cell,
                    ActiveCell {
                        generation,
                        admission,
                        incarnation,
                        code,
                        schema,
                        role,
                        interrupt,
                        publisher: Some(*publisher),
                        durability_submitter,
                        publications: VecDeque::new(),
                        publication_bytes: 0,
                        unpublished_node_logs: 0,
                        queue: VecDeque::new(),
                        coordination: CoordinationState::serving_with_residency(true, residency),
                        persisted_work,
                        inventory_refreshing: false,
                        drain: None,
                        transfer: None,
                        last_used_ms: unix_millis(),
                        last_work_at: std::time::Instant::now(),
                        compaction_retry_at: std::time::Instant::now(),
                    },
                );
            }
            Err(error) => {
                transitioning.remove(&cell);
                let error = if node_lease.check().is_err() {
                    Error::Fenced
                } else {
                    error
                };
                let _ = reply.send(Err(error));
            }
        },
        TaskResult::Hydrated {
            cell,
            generation,
            effect_id,
            result,
        } => {
            let Some(active) = cells.get_mut(&cell) else {
                return;
            };
            if active.generation != generation
                || !active
                    .coordination
                    .effect_matches(effect_id, CoordinationEffect::Hydration)
            {
                return;
            }
            active.finish_task(effect_id, CoordinationEffect::Hydration);
            match result {
                Ok(Some(progress)) => {
                    active
                        .coordination
                        .step(CoordinationInput::FinishHydration {
                            complete: progress.complete(),
                            stale: false,
                        });
                }
                Ok(None) => {
                    active
                        .coordination
                        .step(CoordinationInput::FinishHydration {
                            complete: true,
                            stale: false,
                        });
                }
                Err(_) => {
                    let decision = active
                        .coordination
                        .step(CoordinationInput::FinishHydration {
                            complete: false,
                            stale: true,
                        });
                    if matches!(decision, CoordinationDecision::Fence) {
                        fence_active(active);
                    }
                }
            }
            continue_cell(cell, pool, cells, transitioning, tasks, node_lease);
        }
        TaskResult::InventoryRefreshed {
            cell,
            generation,
            effect_id,
            result,
        } => {
            let Some(active) = cells.get_mut(&cell) else {
                return;
            };
            if active.generation != generation
                || !active
                    .coordination
                    .effect_matches(effect_id, CoordinationEffect::Inventory)
            {
                return;
            }
            active.finish_task(effect_id, CoordinationEffect::Inventory);
            active.inventory_refreshing = false;
            if let Ok(inventory) = result {
                active.persisted_work = inventory;
            }
            continue_cell(cell, pool, cells, transitioning, tasks, node_lease);
        }
        TaskResult::TransferPreflight {
            cell,
            generation,
            effect_id,
            result,
        } => {
            let preflight = {
                let Some(active) = cells.get_mut(&cell) else {
                    return;
                };
                if active.generation != generation
                    || !active
                        .coordination
                        .effect_matches(effect_id, CoordinationEffect::Inventory)
                {
                    return;
                }
                active.finish_task(effect_id, CoordinationEffect::Inventory);
                active.inventory_refreshing = false;
                match active.transfer.take() {
                    Some(transfer) => {
                        let mut ready_to_deactivate = false;
                        let mut fenced = false;
                        let transfer_result = if node_lease.check().is_err() {
                            fenced = true;
                            active.coordination.step(CoordinationInput::Fence);
                            fence_active(active);
                            Err(Error::Fenced)
                        } else {
                            match result {
                                Ok(inventory) => {
                                    if inventory.is_settled()
                                        && active.queue.is_empty()
                                        && active.coordination.can_deactivate()
                                        && transfer_observation(cell, active, inventory).eligible()
                                    {
                                        match active
                                            .coordination
                                            .step(CoordinationInput::ConfirmTransfer)
                                        {
                                            CoordinationDecision::ReadyToDeactivate => {
                                                ready_to_deactivate = true;
                                                Ok(())
                                            }
                                            CoordinationDecision::Started => Ok(()),
                                            CoordinationDecision::Reject(RejectReason::Fenced) => {
                                                fenced = true;
                                                fence_active(active);
                                                Err(Error::Fenced)
                                            }
                                            CoordinationDecision::Reject(reason) => {
                                                Err(rejection_error(reason))
                                            }
                                            _ => Err(Error::CellDraining),
                                        }
                                    } else {
                                        Err(Error::CellDraining)
                                    }
                                }
                                Err(error) => Err(error),
                            }
                        };
                        if transfer_result.is_err() && !fenced {
                            active.coordination.step(CoordinationInput::AbortTransfer);
                        }
                        Some((transfer, ready_to_deactivate, fenced, transfer_result))
                    }
                    None => None,
                }
            };
            let Some((transfer, ready_to_deactivate, _fenced, transfer_result)) = preflight else {
                continue_cell(cell, pool, cells, transitioning, tasks, node_lease);
                return;
            };
            if transfer_result.is_ok() {
                if ready_to_deactivate {
                    if let Some(active) = cells.get_mut(&cell) {
                        active.drain = Some(transfer.reply);
                    }
                    start_deactivate(cell, pool, cells, transitioning, tasks);
                } else if let Some(active) = cells.get_mut(&cell) {
                    active.drain = Some(transfer.reply);
                    continue_cell(cell, pool, cells, transitioning, tasks, node_lease);
                } else {
                    if let Some(mut permit) = movement_permits.remove(&cell) {
                        movement.complete(&mut permit);
                    }
                }
            } else {
                let _ = transfer.reply.send(transfer_result);
                if let Some(mut permit) = movement_permits.remove(&cell) {
                    movement.complete(&mut permit);
                }
                continue_cell(cell, pool, cells, transitioning, tasks, node_lease);
            }
        }
        TaskResult::Executed {
            cell,
            generation,
            effect_id,
            mut command,
            mut result,
            mut fenced,
        } => {
            let Some(active) = cells.get_mut(&cell) else {
                // Publication failure may fence and remove the Cell before its proof waiter
                // completes; the accepted command still owns exactly one terminal outcome.
                send_command_task_reply(&mut command, result);
                return;
            };
            if active.generation != generation
                || !active
                    .coordination
                    .effect_matches(effect_id, CoordinationEffect::Work(AdmissionKind::Command))
            {
                send_command_task_reply(&mut command, result);
                return;
            }
            active.finish_task(effect_id, CoordinationEffect::Work(AdmissionKind::Command));
            if node_lease.check().is_err() {
                result = Err(command.operation.unknown(Error::Fenced));
                fenced = true;
            }
            if fenced {
                finish_work(active, true);
                send_command_task_reply(&mut command, result);
                continue_cell(cell, pool, cells, transitioning, tasks, node_lease);
                return;
            }
            match result {
                Ok(CommandTaskResult::Recorded(outcome)) => {
                    finish_work(active, false);
                    send_command_reply(&mut command, Ok(outcome));
                }
                Ok(CommandTaskResult::Pending {
                    pending,
                    durability,
                    retained_reservation,
                }) => {
                    let retained_bytes = pending.retained_bytes();
                    let publication = active
                        .coordination
                        .step(CoordinationInput::BeginPublication);
                    let CoordinationDecision::Started = publication else {
                        drop(retained_reservation);
                        finish_work(active, false);
                        let error = command.operation.unknown(match publication {
                            CoordinationDecision::Reject(reason) => rejection_error(reason),
                            _ => Error::Fenced,
                        });
                        send_command_reply(&mut command, Err(error));
                        continue_cell(cell, pool, cells, transitioning, tasks, node_lease);
                        return;
                    };
                    active.publication_bytes =
                        match active.publication_bytes.checked_add(retained_bytes) {
                            Some(bytes) => bytes,
                            None => {
                                finish_work(active, true);
                                let error = command
                                    .operation
                                    .unknown(Error::Capacity("pending publication bytes"));
                                send_command_reply(&mut command, Err(error));
                                continue_cell(cell, pool, cells, transitioning, tasks, node_lease);
                                return;
                            }
                        };
                    let outcome = pending.outcome().clone();
                    let commit_sequence = outcome.commit_sequence();
                    let (proof, object) = oneshot::channel();
                    active.publications.push_back(QueuedPublication {
                        pending: *pending,
                        durability: durability.clone(),
                        retained_reservation,
                        submitted_at: std::time::Instant::now(),
                        proof,
                    });
                    if durability.is_some() {
                        active.unpublished_node_logs += 1;
                        unpublished_node_log_bytes.fetch_add(retained_bytes, Ordering::AcqRel);
                    }
                    start_publication(cell, active, pool, tasks);
                    let pool = pool.clone();
                    let generation = active.generation;
                    let effect_id = active.begin_task(CoordinationEffect::Proof);
                    tasks.spawn(async move {
                        prove_command(
                            pool,
                            command,
                            outcome,
                            commit_sequence,
                            durability,
                            object,
                            generation,
                            effect_id,
                        )
                        .await
                    });
                }
                Err(error) => {
                    finish_work(active, false);
                    send_command_reply(&mut command, Err(error));
                }
            }
            continue_cell(cell, pool, cells, transitioning, tasks, node_lease);
        }
        TaskResult::Proven {
            cell,
            generation,
            effect_id,
            mut command,
            mut result,
            mut fenced,
        } => {
            let Some(active) = cells.get_mut(&cell) else {
                // The publication task may fence and remove the actor first; proof owns the
                // caller's final result and must not be rewritten as CellNotActive.
                send_command_reply(&mut command, result);
                return;
            };
            if active.generation != generation
                || !active
                    .coordination
                    .effect_matches(effect_id, CoordinationEffect::Proof)
            {
                send_command_reply(&mut command, result);
                return;
            }
            active.finish_task(effect_id, CoordinationEffect::Proof);
            if node_lease.check().is_err() {
                result = Err(command.operation.unknown(Error::Fenced));
                fenced = true;
            }
            finish_work(active, fenced);
            send_command_reply(&mut command, result);
            continue_cell(cell, pool, cells, transitioning, tasks, node_lease);
        }
        TaskResult::Published {
            cell,
            generation,
            effect_id,
            publisher,
            retained_bytes,
            node_logged,
            mut result,
            mut fenced,
        } => {
            let Some(active) = cells.get_mut(&cell) else {
                return;
            };
            if active.generation != generation
                || !active
                    .coordination
                    .effect_matches(effect_id, CoordinationEffect::Publication)
            {
                return;
            }
            active.finish_task(effect_id, CoordinationEffect::Publication);
            active.last_work_at = std::time::Instant::now();
            let object_published = result.is_ok();
            if node_lease.check().is_err() {
                result = Err(Error::Fenced);
                fenced = true;
            }
            active.publisher = Some(*publisher);
            active.publication_bytes = active.publication_bytes.saturating_sub(retained_bytes);
            if node_logged && object_published {
                active.unpublished_node_logs = active.unpublished_node_logs.saturating_sub(1);
                subtract_unpublished_bytes(unpublished_node_log_bytes, retained_bytes);
            }
            let decision = active
                .coordination
                .step(CoordinationInput::FinishPublication {
                    fenced,
                    succeeded: result.is_ok(),
                });
            if matches!(decision, CoordinationDecision::Fence) {
                fence_active(active);
            } else {
                start_publication(cell, active, pool, tasks);
            }
            continue_cell(cell, pool, cells, transitioning, tasks, node_lease);
        }
        TaskResult::Compacted {
            cell,
            generation,
            effect_id,
            publisher,
            result,
        } => {
            let Some(active) = cells.get_mut(&cell) else {
                return;
            };
            if active.generation != generation
                || !active
                    .coordination
                    .effect_matches(effect_id, CoordinationEffect::Compaction)
            {
                return;
            }
            active.finish_task(effect_id, CoordinationEffect::Compaction);
            let fenced = result.is_err() || node_lease.check().is_err();
            active.publisher = Some(*publisher);
            if matches!(result, Ok(None)) {
                active.compaction_retry_at = std::time::Instant::now() + COMPACTION_RETRY;
            }
            let decision = active
                .coordination
                .step(CoordinationInput::FinishCompaction { fenced });
            if matches!(decision, CoordinationDecision::Fence) {
                fence_active(active);
            }
            continue_cell(cell, pool, cells, transitioning, tasks, node_lease);
        }
        TaskResult::Queried {
            cell,
            generation,
            effect_id,
            mut query,
            mut result,
            mut fenced,
        } => {
            let Some(active) = cells.get_mut(&cell) else {
                // A query accepted before a fence keeps its result even when deactivation wins
                // the actor turn before this completion is delivered.
                send_query_reply(&mut query, result);
                return;
            };
            if active.generation != generation
                || !active
                    .coordination
                    .effect_matches(effect_id, CoordinationEffect::Work(AdmissionKind::Query))
            {
                send_query_reply(&mut query, result);
                return;
            }
            active.finish_task(effect_id, CoordinationEffect::Work(AdmissionKind::Query));
            if node_lease.check().is_err() {
                result = Err(Error::Fenced);
                fenced = true;
            }
            finish_work(active, fenced);
            send_query_reply(&mut query, result);
            continue_cell(cell, pool, cells, transitioning, tasks, node_lease);
        }
        TaskResult::Resolved {
            cell,
            generation,
            effect_id,
            mut resolve,
            mut result,
            mut fenced,
        } => {
            let Some(active) = cells.get_mut(&cell) else {
                // Resolution is an accepted observation, not a new admission; preserve its
                // unknown/committed result across a concurrent fenced deactivation.
                send_resolve_reply(&mut resolve, result);
                return;
            };
            if active.generation != generation
                || !active
                    .coordination
                    .effect_matches(effect_id, CoordinationEffect::Work(AdmissionKind::Resolve))
            {
                send_resolve_reply(&mut resolve, result);
                return;
            }
            active.finish_task(effect_id, CoordinationEffect::Work(AdmissionKind::Resolve));
            if node_lease.check().is_err() {
                result = Ok(Resolution::Unknown);
                fenced = true;
            }
            finish_work(active, fenced);
            send_resolve_reply(&mut resolve, result);
            continue_cell(cell, pool, cells, transitioning, tasks, node_lease);
        }
        TaskResult::Migrated {
            cell,
            generation,
            effect_id,
            publisher,
            mut migration,
            mut result,
            mut fenced,
            preserve_owner,
            unpublished_bytes,
        } => {
            let Some(active) = cells.get_mut(&cell) else {
                let result = match result {
                    Ok(_) => Err(Error::Fenced),
                    Err(error) => Err(error),
                };
                send_migration_reply(&mut migration, result);
                return;
            };
            if active.generation != generation
                || !active.coordination.effect_matches(
                    effect_id,
                    CoordinationEffect::Work(AdmissionKind::Migration),
                )
            {
                let result = match result {
                    Ok(_) => Err(Error::Fenced),
                    Err(error) => Err(error),
                };
                send_migration_reply(&mut migration, result);
                return;
            }
            active.finish_task(
                effect_id,
                CoordinationEffect::Work(AdmissionKind::Migration),
            );
            if node_lease.check().is_err() {
                result = Err(Error::Fenced);
                fenced = true;
            }
            active.publisher = Some(*publisher);
            if preserve_owner {
                active.unpublished_node_logs = active.unpublished_node_logs.saturating_add(1);
                unpublished_node_log_bytes.fetch_add(unpublished_bytes, Ordering::AcqRel);
            }
            let completion = finish_migration(active, fenced);
            match result {
                Ok(outcome) if !matches!(completion, CoordinationDecision::Fence) => {
                    active.code = outcome.code;
                    active.schema = outcome.schema;
                    let admission = Arc::clone(&migration.successor_admission);
                    send_migration_reply(
                        &mut migration,
                        Ok(MigratedAdmission { admission, outcome }),
                    );
                }
                Ok(_) => send_migration_reply(&mut migration, Err(Error::Fenced)),
                Err(error) => send_migration_reply(&mut migration, Err(error)),
            }
            continue_cell(cell, pool, cells, transitioning, tasks, node_lease);
        }
        TaskResult::Renewed {
            cell,
            generation,
            effect_id,
            publisher,
            mut result,
        } => {
            let Some(active) = cells.get_mut(&cell) else {
                return;
            };
            if active.generation != generation
                || !active
                    .coordination
                    .effect_matches(effect_id, CoordinationEffect::Renewal)
            {
                return;
            }
            active.finish_task(effect_id, CoordinationEffect::Renewal);
            if node_lease.check().is_err() {
                result = Err(Error::Fenced);
            }
            active.publisher = Some(*publisher);
            let decision = active.coordination.step(CoordinationInput::FinishRenewal {
                fenced: result.is_err(),
            });
            if matches!(decision, CoordinationDecision::Fence) {
                fence_active(active);
            }
            continue_cell(cell, pool, cells, transitioning, tasks, node_lease);
        }
        TaskResult::Deactivated {
            cell,
            generation,
            reply,
            shutdown_drain,
            result,
        } => {
            if cells
                .get(&cell)
                .is_some_and(|active| active.generation != generation)
            {
                return;
            }
            if let Some(mut permit) = movement_permits.remove(&cell) {
                movement.complete(&mut permit);
            }
            transitioning.remove(&cell);
            let runtime_waiting =
                shutdown_drain || shutdown.as_ref().is_some_and(|state| state.draining);
            match (reply, runtime_waiting) {
                (Some(reply), true) => {
                    if result.is_err() {
                        fail_shutdown(
                            shutdown,
                            Error::Control("one or more Cells failed to drain"),
                        );
                    }
                    let _ = reply.send(result);
                }
                (Some(reply), false) => {
                    let _ = reply.send(result);
                }
                (None, true) => {
                    if let Err(error) = result {
                        fail_shutdown(shutdown, error);
                    }
                }
                (None, false) => {}
            }
        }
    }
}

fn subtract_unpublished_bytes(total: &AtomicU64, bytes: u64) {
    let _ = total.fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
        Some(current.saturating_sub(bytes))
    });
}

fn rejection_error(reason: RejectReason) -> Error {
    match reason {
        RejectReason::NotActive => Error::CellNotActive,
        RejectReason::Fenced => Error::Fenced,
        RejectReason::Draining => Error::CellDraining,
        RejectReason::Busy => Error::CellDraining,
        RejectReason::PublicationPending => Error::PendingPublication,
    }
}

fn finish_work(active: &mut ActiveCell, fenced: bool) -> CoordinationDecision {
    active.last_work_at = std::time::Instant::now();
    let decision = active
        .coordination
        .step(CoordinationInput::FinishWork { fenced });
    if matches!(decision, CoordinationDecision::Fence) {
        fence_active(active);
    }
    decision
}

fn finish_migration(active: &mut ActiveCell, fenced: bool) -> CoordinationDecision {
    active.last_work_at = std::time::Instant::now();
    let decision = active
        .coordination
        .step(CoordinationInput::FinishMigration { fenced });
    if matches!(decision, CoordinationDecision::Fence) {
        fence_active(active);
    }
    decision
}

fn fence_active(active: &mut ActiveCell) {
    active.coordination.step(CoordinationInput::Fence);
    fence_admission(&active.admission);
    if let Some(transfer) = active.transfer.take() {
        let _ = transfer.reply.send(Err(Error::Fenced));
    }
    active.inventory_refreshing = false;
    while let Some(publication) = active.publications.pop_front() {
        active
            .coordination
            .step(CoordinationInput::FinishPublication {
                fenced: true,
                succeeded: false,
            });
        active.publication_bytes = active
            .publication_bytes
            .saturating_sub(publication.pending.retained_bytes());
        let _ = publication.proof.send(Err(Error::Fenced));
    }
    while let Some(queued) = active.queue.pop_front() {
        match queued {
            QueuedWork::Command(mut command) => {
                send_command_reply(&mut command, Err(Error::Fenced));
            }
            QueuedWork::Query(mut query) => {
                send_query_reply(&mut query, Err(Error::Fenced));
            }
            QueuedWork::Resolve(mut resolve) => {
                send_resolve_reply(&mut resolve, Ok(Resolution::Unknown));
            }
            QueuedWork::Migration(mut migration) => {
                send_migration_reply(&mut migration, Err(Error::Fenced));
            }
        }
    }
}

fn continue_cell(
    cell: CellId,
    pool: &SqlWorkerPool,
    cells: &mut HashMap<CellId, ActiveCell>,
    transitioning: &mut HashSet<CellId>,
    tasks: &mut JoinSet<TaskResult>,
    node_lease: &RuntimeNodeLease,
) {
    if cells
        .get(&cell)
        .is_some_and(|active| active.transfer.is_some())
    {
        let ready = cells.get(&cell).is_some_and(|active| {
            !active.inventory_refreshing
                && active.queue.is_empty()
                && active.coordination.can_deactivate()
        });
        if ready {
            start_transfer_inspection(cell, pool, cells, tasks, node_lease);
            return;
        }
        if let Some(active) = cells.get_mut(&cell)
            && !active.inventory_refreshing
            && !active.queue.is_empty()
        {
            start_next(active, pool, tasks, node_lease);
        }
        return;
    }
    let Some(active) = cells.get_mut(&cell) else {
        return;
    };
    let decision = schedule(active, node_lease.check().is_ok());
    match decision {
        CoordinationDecision::ReadyToDeactivateFenced => {
            let preserve_owner = active.unpublished_node_logs != 0;
            start_fenced_deactivate(cell, pool, cells, transitioning, tasks, preserve_owner);
        }
        CoordinationDecision::ReadyToDeactivate => {
            start_deactivate(cell, pool, cells, transitioning, tasks);
        }
        CoordinationDecision::StartQueuedWork => {
            start_next(active, pool, tasks, node_lease);
        }
        CoordinationDecision::Ignored
        | CoordinationDecision::Admit
        | CoordinationDecision::ResolveUnknown
        | CoordinationDecision::LocalHandle
        | CoordinationDecision::Reject(_)
        | CoordinationDecision::Started
        | CoordinationDecision::EffectCompleted
        | CoordinationDecision::StaleEffect => {}
        CoordinationDecision::Fence => {
            fence_active(active);
        }
    }
}

fn start_transfer_inspection(
    cell: CellId,
    pool: &SqlWorkerPool,
    cells: &mut HashMap<CellId, ActiveCell>,
    tasks: &mut JoinSet<TaskResult>,
    node_lease: &RuntimeNodeLease,
) {
    let Some(active) = cells.get_mut(&cell) else {
        return;
    };
    if active.transfer.is_none()
        || active.inventory_refreshing
        || !active.queue.is_empty()
        || !active.coordination.can_deactivate()
    {
        return;
    }
    if node_lease.check().is_err() {
        fence_active(active);
        return;
    }
    let generation = active.generation;
    let role = active.role;
    let effect_id = active.begin_task(CoordinationEffect::Inventory);
    active.inventory_refreshing = true;
    let pool = pool.clone();
    tasks.spawn(async move {
        let deadline = std::time::Instant::now() + SQL_WALL_DEADLINE;
        let result = tokio::time::timeout_at(
            deadline.into(),
            pool.transfer_work_inventory(cell, role, unix_millis()),
        )
        .await
        .map_err(|_| Error::Deadline)
        .and_then(|result| result);
        TaskResult::TransferPreflight {
            cell,
            generation,
            effect_id,
            result,
        }
    });
}

fn fence_admission(admission: &CellAdmission) {
    admission.fenced.store(true, Ordering::Release);
    admission.draining.store(true, Ordering::Release);
    admission.requests.close();
    admission.bytes.close();
}

fn new_cell_admission() -> Arc<CellAdmission> {
    Arc::new(CellAdmission {
        requests: Arc::new(Semaphore::new(CELL_REQUESTS)),
        bytes: Arc::new(Semaphore::new(CELL_BYTES)),
        draining: AtomicBool::new(false),
        fenced: AtomicBool::new(false),
    })
}

fn send_command_reply(command: &mut QueuedCommand, result: crate::Result<StoredOutcome>) {
    if let Some(reply) = command.reply.take() {
        let _ = reply.send(result);
    }
}

fn send_command_task_reply(command: &mut QueuedCommand, result: crate::Result<CommandTaskResult>) {
    let result = match result {
        Ok(CommandTaskResult::Recorded(outcome)) => Ok(outcome),
        Ok(CommandTaskResult::Pending { .. }) => Err(command.operation.unknown(Error::Fenced)),
        Err(error) => Err(error),
    };
    send_command_reply(command, result);
}

fn send_query_reply(query: &mut QueuedQuery, result: crate::Result<Vec<u8>>) {
    if let Some(reply) = query.reply.take() {
        let _ = reply.send(result);
    }
}

fn send_resolve_reply(resolve: &mut QueuedResolve, result: crate::Result<Resolution>) {
    if let Some(reply) = resolve.reply.take() {
        let _ = reply.send(result);
    }
}

fn send_migration_reply(migration: &mut QueuedMigration, result: crate::Result<MigratedAdmission>) {
    if let Some(reply) = migration.reply.take() {
        let _ = reply.send(result);
    }
}

fn start_due_renewals(
    pool: &SqlWorkerPool,
    cells: &mut HashMap<CellId, ActiveCell>,
    tasks: &mut JoinSet<TaskResult>,
    node_lease: &RuntimeNodeLease,
) {
    let active_renewals = cells.values().filter(|active| active.renewing()).count();
    let mut available = MAX_RENEWALS_IN_FLIGHT.saturating_sub(active_renewals);
    if available == 0 {
        return;
    }
    let now = std::time::Instant::now();
    for (cell, active) in cells.iter_mut() {
        if available == 0 {
            break;
        }
        if active
            .publisher
            .as_ref()
            .is_none_or(|publisher| !publisher.renewal_due(now))
        {
            continue;
        }
        match active.coordination.step(CoordinationInput::BeginRenewal {
            queue_empty: active.queue.is_empty(),
            publication_idle: active.coordination.publication_count() == 0,
            lease_live: node_lease.check().is_ok(),
        }) {
            CoordinationDecision::Started => {}
            CoordinationDecision::Fence => {
                fence_active(active);
                continue;
            }
            _ => continue,
        }
        let Some(mut publisher) = active.publisher.take() else {
            active
                .coordination
                .step(CoordinationInput::FinishRenewal { fenced: true });
            continue;
        };
        let generation = active.generation;
        let effect_id = active.begin_task(CoordinationEffect::Renewal);
        available -= 1;
        let cell = *cell;
        let pool = pool.clone();
        tasks.spawn(async move {
            let result = publisher.renew().await;
            if result.is_err() {
                let _ = pool.fence(cell).await;
            }
            TaskResult::Renewed {
                cell,
                generation,
                effect_id,
                publisher: Box::new(publisher),
                result,
            }
        });
    }
}

fn start_deactivate(
    cell: CellId,
    pool: &SqlWorkerPool,
    cells: &mut HashMap<CellId, ActiveCell>,
    transitioning: &mut HashSet<CellId>,
    tasks: &mut JoinSet<TaskResult>,
) {
    let Some(active) = cells.remove(&cell) else {
        return;
    };
    transitioning.insert(cell);
    let generation = active.generation;
    let pool = pool.clone();
    tasks.spawn(async move {
        let result = async {
            pool.deactivate(cell).await?;
            let mut publisher = active.publisher.ok_or(Error::Fenced)?;
            publisher.release().await
        }
        .await;
        TaskResult::Deactivated {
            cell,
            generation,
            reply: active.drain,
            shutdown_drain: active.coordination.is_shutdown(),
            result,
        }
    });
}

fn start_fenced_deactivate(
    cell: CellId,
    pool: &SqlWorkerPool,
    cells: &mut HashMap<CellId, ActiveCell>,
    transitioning: &mut HashSet<CellId>,
    tasks: &mut JoinSet<TaskResult>,
    preserve_owner: bool,
) {
    let Some(active) = cells.remove(&cell) else {
        return;
    };
    transitioning.insert(cell);
    let generation = active.generation;
    let pool = pool.clone();
    tasks.spawn(async move {
        let result = async {
            pool.discard(cell).await?;
            // An unpublished node-log cut must keep its owner record so
            // takeover seals and replays it instead of treating the Cell as idle.
            if preserve_owner {
                return Ok(());
            }
            let mut publisher = active.publisher.ok_or(Error::Fenced)?;
            publisher.release_after_fence().await
        }
        .await;
        TaskResult::Deactivated {
            cell,
            generation,
            reply: active.drain,
            shutdown_drain: active.coordination.is_shutdown(),
            result,
        }
    });
}

async fn cleanup_failed_activation(
    cell: CellId,
    pool: &SqlWorkerPool,
    publisher: &mut CellPublisher,
) -> crate::Result<()> {
    match pool.deactivate(cell).await {
        Ok(()) | Err(Error::CellNotActive) => {}
        Err(error) => return Err(error),
    }
    if publisher.control().value().root.is_some() {
        publisher.release().await
    } else {
        Ok(())
    }
}

async fn rollback_failed_acquisition(
    authority: &CellAuthority,
    claimed: &VersionedControl,
    replica: &crab_ltx::CellReplica,
    node_lease: Option<NodeLeaseGuard>,
) -> crate::Result<()> {
    let current = authority
        .load(claimed.value().cell)
        .await?
        .ok_or(Error::Fenced)?;
    if current.value().state == crate::ControlState::Idle && current.value().owner.is_none() {
        return Ok(());
    }
    if current.value().epoch != claimed.value().epoch
        || current.value().owner != claimed.value().owner
        || current.value().root != claimed.value().root
        || current.value().recovery != claimed.value().recovery
        || current.value().code != claimed.value().code
        || current.value().schema != claimed.value().schema
        || current.value().recovery.is_some()
    {
        return Ok(());
    }
    let mut publisher =
        CellPublisher::new(replica.clone(), authority.clone(), current, PathBuf::new());
    if let Some(node_lease) = node_lease {
        publisher = publisher.with_node_lease(node_lease);
    }
    match publisher.release().await {
        Ok(_) => Ok(()),
        Err(error) => {
            let latest = authority
                .load(claimed.value().cell)
                .await?
                .ok_or(Error::Fenced)?;
            if (latest.value().state == crate::ControlState::Idle && latest.value().owner.is_none())
                || latest.value().epoch != claimed.value().epoch
                || latest.value().owner != claimed.value().owner
                || latest.value().root != claimed.value().root
                || latest.value().recovery != claimed.value().recovery
                || latest.value().code != claimed.value().code
                || latest.value().schema != claimed.value().schema
            {
                Ok(())
            } else {
                Err(error)
            }
        }
    }
}

fn start_orphan_deactivate(
    cell: CellId,
    pool: &SqlWorkerPool,
    mut publisher: CellPublisher,
    transitioning: &mut HashSet<CellId>,
    tasks: &mut JoinSet<TaskResult>,
    shutdown_drain: bool,
    generation: u64,
) {
    let pool = pool.clone();
    tasks.spawn(async move {
        let result = async {
            pool.deactivate(cell).await?;
            publisher.release().await
        }
        .await;
        TaskResult::Deactivated {
            cell,
            generation,
            reply: None,
            shutdown_drain,
            result,
        }
    });
    transitioning.insert(cell);
}

#[cfg(test)]
mod tests {
    use super::{CellRuntimeStats, bounded_u32};

    #[test]
    fn placement_projection_saturates_large_node_counters() {
        let stats = CellRuntimeStats {
            active_cells: usize::MAX,
            active_cell_capacity: usize::MAX,
            resident_bytes: 0,
            resident_capacity_bytes: 0,
            file_descriptors: 0,
            file_descriptor_capacity: 0,
            retained_bytes: 0,
            retained_capacity_bytes: 0,
            worker_jobs: usize::MAX,
            worker_job_capacity: usize::MAX,
            primitive_jobs: usize::MAX,
            primitive_job_capacity: usize::MAX,
            hydration_jobs: usize::MAX,
            hydration_job_capacity: usize::MAX,
            io_slots: 0,
            io_slot_capacity: 0,
            blocking_jobs: 0,
            blocking_job_capacity: 0,
            recovery_jobs: 0,
            recovery_job_capacity: 0,
            dirty_jobs: 0,
            dirty_job_capacity: 0,
            scratch_units: 0,
            scratch_unit_capacity: 0,
            local_disk_reserved_bytes: 0,
            local_disk_capacity_bytes: 0,
            unpublished_node_log_bytes: 0,
        };

        assert_eq!(bounded_u32(usize::MAX), u32::MAX);
        assert_eq!(stats.placement_active_cells(), u32::MAX);
        assert_eq!(stats.placement_active_cell_capacity(), u32::MAX);
        assert_eq!(stats.placement_running_jobs(), u32::MAX);
        assert_eq!(stats.placement_job_capacity(), u32::MAX);
    }
}
