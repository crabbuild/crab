use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
};

use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError, mpsc, oneshot},
    task::JoinSet,
};

mod handle;

pub use handle::CellHandle;
use handle::{CellAdmission, WorkAdmission};

use crate::executor::{MAX_PENDING_PUBLICATIONS, PENDING_PUBLICATION_HIGH_WATER_BYTES};
use crate::publication::{CellDurabilitySubmitter, NodeDurabilitySlot, PendingDurability};
use crate::{
    ApplicationId, CatalogProof, CellAuthority, CellId, CellPublisher, Digest, Error,
    InboxDelivery, MigrationOutcome, MigrationPlan, MutationIdentity, NodeDurability,
    NodeLeaseGuard, Owner, PendingCommit, Resolution, SessionId, SqlWorkerPool, StoredOutcome,
    Transition, VersionedControl, WorkerExecution,
    worker::{CellReservation, Handler, Initializer, QueryHandler, WorkerState},
};

const INGRESS_REQUESTS: usize = 1_024;
const CELL_REQUESTS: usize = 64;
const CELL_BYTES: usize = 8 * 1024 * 1024;
const RENEWAL_SCAN: std::time::Duration = std::time::Duration::from_millis(100);
const MAX_RENEWALS_IN_FLIGHT: usize = 32;
const SQL_WALL_DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);

/// Conservative per-active-Cell reservation for actor state and native tasks.
///
/// This is admission accounting rather than an RSS guarantee. Embedders must
/// qualify the estimate against their compiled registry and workload.
pub const ACTIVE_CELL_NATIVE_BYTES: u64 = 64 * 1024;

/// Node-wide dispatcher for bounded per-Cell command mailboxes.
#[derive(Clone)]
pub struct CellRuntime {
    inner: Arc<RuntimeInner>,
}

/// Opaque node-wide byte reservation held until it is dropped.
#[must_use = "dropping the reservation immediately releases its capacity"]
pub struct NodeByteReservation {
    _permit: OwnedSemaphorePermit,
}

/// Point-in-time node admission usage for one embedded Cell runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellRuntimeStats {
    active_cells: usize,
    active_cell_capacity: usize,
    retained_bytes: usize,
    retained_capacity_bytes: usize,
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
    node_bytes: Arc<Semaphore>,
    node_retained_bytes: usize,
    shutting_down: AtomicBool,
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
                node_bytes: Arc::new(Semaphore::new(node_retained_bytes)),
                node_retained_bytes,
                shutting_down: AtomicBool::new(false),
                session,
                pool,
                replica_host,
                node_lease,
                node_durability: Arc::new(std::sync::RwLock::new(None)),
                telemetry: crate::CellTelemetryHandle::default(),
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
        self.inner.node_bytes.close();
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

    /// Reports whether node-wide admission has entered its terminal drain.
    #[must_use]
    pub fn is_shutting_down(&self) -> bool {
        self.inner.shutting_down.load(Ordering::Acquire)
    }

    /// Samples node-wide admission usage without waiting for actor work.
    #[must_use]
    pub fn stats(&self) -> CellRuntimeStats {
        let retained_available = self.inner.node_bytes.available_permits();
        CellRuntimeStats {
            active_cells: self.inner.pool.active_cells(),
            active_cell_capacity: self.inner.pool.active_cell_capacity(),
            retained_bytes: self
                .inner
                .node_retained_bytes
                .saturating_sub(retained_available),
            retained_capacity_bytes: self.inner.node_retained_bytes,
            local_disk_reserved_bytes: self.inner.replica_host.local_disk_used(),
            local_disk_capacity_bytes: self.inner.replica_host.local_disk_capacity(),
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
        let permits = u32::try_from(bytes)
            .ok()
            .filter(|permits| *permits != 0)
            .ok_or(Error::Capacity("node retained bytes"))?;
        let permit = Arc::clone(&self.inner.node_bytes)
            .try_acquire_many_owned(permits)
            .map_err(|error| match error {
                TryAcquireError::Closed => Error::RuntimeClosed,
                TryAcquireError::NoPermits => Error::Capacity("node retained bytes"),
            })?;
        Ok(NodeByteReservation { _permit: permit })
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
        self.ensure_running()?;
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
        self.ensure_running()?;
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
        self.ensure_running()?;
        self.activation_cell(&catalog, &observed)?;
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
        self.ensure_running()?;
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
        self.activate_restored_reserved(
            catalog,
            replica,
            authority,
            claimed,
            destination,
            reservation,
        )
        .await
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
        self.ensure_running()?;
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
            let claimed = self
                .publish_attached_recovery(&replica, &authority, claimed, &recovery_store)
                .await?;
            return self
                .activate_restored_reserved(
                    catalog,
                    replica,
                    authority,
                    claimed,
                    destination,
                    reservation,
                )
                .await;
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
        let replica = replica.with_host(self.inner.replica_host.clone());
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

    async fn activate_inner(
        &self,
        catalog: CatalogProof,
        activation: Activation,
        replica: crab_ltx::CellReplica,
        authority: CellAuthority,
        observed: VersionedControl,
    ) -> crate::Result<CellHandle> {
        let replica = replica.with_host(self.inner.replica_host.clone());
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

enum Message {
    Activate {
        cell: CellId,
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
        reply: oneshot::Sender<Option<LocalCell>>,
    },
    Drain {
        cell: CellId,
        admission: Arc<CellAdmission>,
        reply: oneshot::Sender<crate::Result<()>>,
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
    admission: Arc<CellAdmission>,
    incarnation: crate::IncarnationId,
    code: Digest,
    schema: u32,
    interrupt: Arc<crab_ltx::rusqlite::InterruptHandle>,
    publisher: Option<CellPublisher>,
    durability_submitter: CellDurabilitySubmitter,
    publications: VecDeque<QueuedPublication>,
    publication_count: usize,
    publication_bytes: u64,
    unpublished_node_logs: usize,
    queue: VecDeque<QueuedWork>,
    busy: bool,
    renewing: bool,
    fenced: bool,
    drain: Option<oneshot::Sender<crate::Result<()>>>,
    migrating: bool,
    shutdown_drain: bool,
}

struct QueuedPublication {
    pending: PendingCommit,
    durability: Option<PendingDurability>,
    submitted_at: std::time::Instant,
    proof: oneshot::Sender<crate::Result<()>>,
}

impl ActiveCell {
    fn draining(&self) -> bool {
        self.drain.is_some() || self.migrating || self.shutdown_drain
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
        publisher: Box<CellPublisher>,
        admission: Arc<CellAdmission>,
        reply: oneshot::Sender<crate::Result<Arc<CellAdmission>>>,
        result: crate::Result<Arc<crab_ltx::rusqlite::InterruptHandle>>,
    },
    Executed {
        cell: CellId,
        command: Box<QueuedCommand>,
        result: crate::Result<CommandTaskResult>,
        fenced: bool,
    },
    Proven {
        cell: CellId,
        command: Box<QueuedCommand>,
        result: crate::Result<StoredOutcome>,
        fenced: bool,
    },
    Published {
        cell: CellId,
        publisher: Box<CellPublisher>,
        retained_bytes: u64,
        node_logged: bool,
        result: crate::Result<()>,
        fenced: bool,
    },
    Queried {
        cell: CellId,
        query: Box<QueuedQuery>,
        result: crate::Result<Vec<u8>>,
        fenced: bool,
    },
    Resolved {
        cell: CellId,
        resolve: Box<QueuedResolve>,
        result: crate::Result<Resolution>,
        fenced: bool,
    },
    Migrated {
        cell: CellId,
        publisher: Box<CellPublisher>,
        migration: Box<QueuedMigration>,
        result: crate::Result<MigrationOutcome>,
        fenced: bool,
        preserve_owner: bool,
        unpublished_bytes: u64,
    },
    Renewed {
        cell: CellId,
        publisher: Box<CellPublisher>,
        result: crate::Result<()>,
    },
    Deactivated {
        cell: CellId,
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
    let mut shutdown = None::<ShutdownState>;
    let mut renewal_tick = tokio::time::interval(RENEWAL_SCAN);
    renewal_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    renewal_tick.tick().await;
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
            );
            continue;
        }
        if tasks.is_empty() {
            tokio::select! {
                message = receiver.recv() => {
                    let Some(message) = message else {
                        if shutdown.is_some() {
                            start_shutdown_drain(&pool, &mut cells, &mut transitioning, &mut tasks, &mut shutdown);
                            continue;
                        }
                        break;
                    };
                    handle_message(message, &mut receiver, &pool, &mut cells, &mut transitioning, &mut tasks, &mut shutdown, &node_lease);
                }
                _ = renewal_tick.tick() => {
                    start_due_renewals(&pool, &mut cells, &mut tasks);
                }
            }
            continue;
        }
        tokio::select! {
            message = receiver.recv() => {
                let Some(message) = message else {
                    if shutdown.is_some() {
                        start_shutdown_drain(&pool, &mut cells, &mut transitioning, &mut tasks, &mut shutdown);
                        continue;
                    }
                    while let Some(result) = tasks.join_next().await {
                        let Ok(result) = result else { return; };
                        handle_task(result, &pool, &mut cells, &mut transitioning, &mut tasks, &mut shutdown, &node_lease, &unpublished_node_log_bytes);
                    }
                    break;
                };
                handle_message(message, &mut receiver, &pool, &mut cells, &mut transitioning, &mut tasks, &mut shutdown, &node_lease);
            }
            result = tasks.join_next() => {
                let Some(Ok(result)) = result else {
                    return;
                };
                handle_task(result, &pool, &mut cells, &mut transitioning, &mut tasks, &mut shutdown, &node_lease, &unpublished_node_log_bytes);
            }
            _ = renewal_tick.tick() => {
                start_due_renewals(&pool, &mut cells, &mut tasks);
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
        active.shutdown_drain = true;
        active.admission.draining.store(true, Ordering::Release);
        active.admission.requests.close();
        active.admission.bytes.close();
        if !active.busy
            && !active.renewing
            && active.queue.is_empty()
            && active.publication_count == 0
            && active.publisher.is_some()
        {
            ready.push((*cell, active.fenced, active.unpublished_node_logs != 0));
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

fn handle_message(
    message: Message,
    receiver: &mut mpsc::Receiver<Message>,
    pool: &SqlWorkerPool,
    cells: &mut HashMap<CellId, ActiveCell>,
    transitioning: &mut HashSet<CellId>,
    tasks: &mut JoinSet<TaskResult>,
    shutdown: &mut Option<ShutdownState>,
    node_lease: &RuntimeNodeLease,
) {
    if !matches!(message, Message::Shutdown { .. }) && node_lease.check().is_err() {
        for active in cells.values_mut() {
            active.fenced = true;
            fence_active(active);
        }
        reject_fenced_message(message);
        return;
    }
    match message {
        Message::Activate {
            cell,
            activation,
            publisher,
            reply,
        } => {
            if cells.contains_key(&cell) || !transitioning.insert(cell) {
                let _ = reply.send(Err(Error::CellAlreadyActive));
                return;
            }
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
                let result = match result {
                    Ok(()) => pool.interrupt_handle(cell).await.map(Arc::new),
                    Err(error) => Err(error),
                };
                TaskResult::Activated {
                    cell,
                    publisher,
                    admission,
                    reply,
                    result,
                }
            });
        }
        Message::Execute(mut command) => {
            let Some(active) = cells.get_mut(&command.cell) else {
                send_command_reply(&mut command, Err(Error::CellNotActive));
                return;
            };
            if !Arc::ptr_eq(&active.admission, &command.admission) {
                send_command_reply(&mut command, Err(Error::CellNotActive));
                return;
            }
            if active.fenced || active.drain.is_some() || active.shutdown_drain {
                let error = if active.fenced {
                    Error::Fenced
                } else {
                    Error::CellDraining
                };
                send_command_reply(&mut command, Err(error));
                return;
            }
            active.queue.push_back(QueuedWork::Command(command));
            start_next(active, pool, tasks, node_lease);
        }
        Message::Query(mut query) => {
            let Some(active) = cells.get_mut(&query.cell) else {
                send_query_reply(&mut query, Err(Error::CellNotActive));
                return;
            };
            if !Arc::ptr_eq(&active.admission, &query.admission) {
                send_query_reply(&mut query, Err(Error::CellNotActive));
                return;
            }
            if active.fenced || active.drain.is_some() || active.shutdown_drain {
                let error = if active.fenced {
                    Error::Fenced
                } else {
                    Error::CellDraining
                };
                send_query_reply(&mut query, Err(error));
                return;
            }
            active.queue.push_back(QueuedWork::Query(query));
            start_next(active, pool, tasks, node_lease);
        }
        Message::Resolve(mut resolve) => {
            let Some(active) = cells.get_mut(&resolve.cell) else {
                send_resolve_reply(&mut resolve, Err(Error::CellNotActive));
                return;
            };
            if !Arc::ptr_eq(&active.admission, &resolve.admission) {
                send_resolve_reply(&mut resolve, Err(Error::CellNotActive));
                return;
            }
            if active.fenced {
                send_resolve_reply(&mut resolve, Ok(Resolution::Unknown));
                return;
            }
            if active.drain.is_some() || active.shutdown_drain {
                send_resolve_reply(&mut resolve, Err(Error::CellDraining));
                return;
            }
            active.queue.push_back(QueuedWork::Resolve(resolve));
            start_next(active, pool, tasks, node_lease);
        }
        Message::Migrate(mut migration) => {
            let Some(active) = cells.get_mut(&migration.cell) else {
                send_migration_reply(&mut migration, Err(Error::CellNotActive));
                return;
            };
            if !Arc::ptr_eq(&active.admission, &migration.admission) {
                send_migration_reply(&mut migration, Err(Error::CellNotActive));
                return;
            }
            if active.fenced || active.draining() {
                let error = if active.fenced {
                    Error::Fenced
                } else {
                    Error::CellDraining
                };
                send_migration_reply(&mut migration, Err(error));
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
            active.migrating = true;
            active.admission = Arc::clone(&migration.successor_admission);
            active.queue.push_back(QueuedWork::Migration(migration));
            start_next(active, pool, tasks, node_lease);
        }
        Message::Lookup { cell, reply } => {
            let local = cells.get(&cell).and_then(|active| {
                (!active.fenced && !active.draining()).then(|| LocalCell {
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
                let _ = reply.send(Err(Error::CellNotActive));
                return;
            }
            if active.draining() {
                let _ = reply.send(Err(Error::CellDraining));
                return;
            }
            active.drain = Some(reply);
            if !active.busy
                && !active.renewing
                && active.queue.is_empty()
                && active.publication_count == 0
                && active.publisher.is_some()
            {
                start_deactivate(cell, pool, cells, transitioning, tasks);
            }
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
        Message::Shutdown { reply } => {
            let _ = reply.send(Err(Error::RuntimeClosed));
        }
    }
}

async fn activate_restored_and_publish(
    cell: CellId,
    pool: &SqlWorkerPool,
    publisher: &mut CellPublisher,
    activation: RestoredActivation,
) -> crate::Result<()> {
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
    Ok(())
}

async fn bootstrap_and_publish(
    cell: CellId,
    pool: &SqlWorkerPool,
    publisher: &mut CellPublisher,
    activation: BootstrapActivation,
) -> crate::Result<()> {
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
    Ok(())
}

fn start_next(
    active: &mut ActiveCell,
    pool: &SqlWorkerPool,
    tasks: &mut JoinSet<TaskResult>,
    node_lease: &RuntimeNodeLease,
) {
    if active.busy || active.renewing || active.fenced {
        return;
    }
    if node_lease.check().is_err() {
        active.fenced = true;
        fence_active(active);
        return;
    }
    let Some(work) = active.queue.front() else {
        return;
    };
    match work {
        QueuedWork::Command(_)
            if active.publication_count >= MAX_PENDING_PUBLICATIONS
                || active.publication_bytes >= PENDING_PUBLICATION_HIGH_WATER_BYTES =>
        {
            return;
        }
        // Migrations change the schema used to prepare roots, so they cannot
        // cross an older command cut that still targets the current schema.
        QueuedWork::Migration(_) if active.publication_count != 0 || active.publisher.is_none() => {
            return;
        }
        _ => {}
    }
    let Some(work) = active.queue.pop_front() else {
        return;
    };
    active.busy = true;
    let pool = pool.clone();
    let interrupt = active.interrupt.clone();
    match work {
        QueuedWork::Command(command) => {
            let durability = active.durability_submitter.clone();
            tasks.spawn(async move { execute_command(pool, durability, command, interrupt).await });
        }
        QueuedWork::Query(query) => {
            tasks.spawn(async move { execute_query(pool, query, interrupt).await });
        }
        QueuedWork::Resolve(resolve) => {
            tasks.spawn(async move { execute_resolve(pool, resolve, interrupt).await });
        }
        QueuedWork::Migration(migration) => {
            let Some(publisher) = active.publisher.take() else {
                let mut migration = migration;
                send_migration_reply(&mut migration, Err(Error::Fenced));
                active.fenced = true;
                active.busy = false;
                return;
            };
            tasks.spawn(async move {
                execute_migration(pool, Box::new(publisher), migration, interrupt).await
            });
        }
    }
}

async fn execute_migration(
    pool: SqlWorkerPool,
    mut publisher: Box<CellPublisher>,
    mut migration: Box<QueuedMigration>,
    interrupt: Arc<crab_ltx::rusqlite::InterruptHandle>,
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
            let result = durability
                .submit(pending.outcome().commit_sequence(), pending.cuts())
                .await
                .map(|durability| CommandTaskResult::Pending {
                    pending,
                    durability,
                });
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
    if active.fenced {
        return;
    }
    let Some(mut publisher) = active.publisher.take() else {
        return;
    };
    let Some(publication) = active.publications.pop_front() else {
        active.publisher = Some(publisher);
        return;
    };
    let node_logged = publication.durability.is_some();
    // Moving the publisher out of ActiveCell is the serialization token for
    // root preparation and CAS; no second object publisher can overtake it.
    let pool = pool.clone();
    tasks.spawn(async move {
        let retained_bytes = publication.pending.retained_bytes();
        let result = async {
            let expected = publication.pending.outcome().clone();
            let prepared = publisher.prepare(&publication.pending).await?;
            pool.bind_prepared(cell, prepared.clone()).await?;
            let root = publisher
                .publish_prepared(&prepared, publication.pending.next_due_ms())
                .await?;
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
        let fenced = result.is_err();
        let proof = if fenced { Err(Error::Fenced) } else { Ok(()) };
        let _ = publication.proof.send(proof);
        if fenced {
            let _ = pool.fence(cell).await;
        }
        TaskResult::Published {
            cell,
            publisher: Box::new(publisher),
            retained_bytes,
            node_logged,
            result,
            fenced,
        }
    });
}

async fn execute_query(
    pool: SqlWorkerPool,
    mut query: Box<QueuedQuery>,
    interrupt: Arc<crab_ltx::rusqlite::InterruptHandle>,
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
        query,
        result,
        fenced,
    }
}

async fn execute_resolve(
    pool: SqlWorkerPool,
    mut resolve: Box<QueuedResolve>,
    interrupt: Arc<crab_ltx::rusqlite::InterruptHandle>,
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
        resolve,
        result,
        fenced,
    }
}

fn handle_task(
    result: TaskResult,
    pool: &SqlWorkerPool,
    cells: &mut HashMap<CellId, ActiveCell>,
    transitioning: &mut HashSet<CellId>,
    tasks: &mut JoinSet<TaskResult>,
    shutdown: &mut Option<ShutdownState>,
    node_lease: &RuntimeNodeLease,
    unpublished_node_log_bytes: &AtomicU64,
) {
    match result {
        TaskResult::Activated {
            cell,
            publisher,
            admission,
            reply,
            result,
        } => match result {
            Ok(interrupt) => {
                if node_lease.check().is_err() {
                    fence_admission(&admission);
                    let _ = reply.send(Err(Error::Fenced));
                    start_orphan_deactivate(cell, pool, *publisher, transitioning, tasks, false);
                    return;
                }
                if shutdown.as_ref().is_some_and(|state| state.draining) {
                    admission.draining.store(true, Ordering::Release);
                    admission.requests.close();
                    admission.bytes.close();
                    let _ = reply.send(Err(Error::RuntimeClosed));
                    start_orphan_deactivate(cell, pool, *publisher, transitioning, tasks, true);
                    return;
                }
                if reply.send(Ok(admission.clone())).is_err() {
                    start_orphan_deactivate(cell, pool, *publisher, transitioning, tasks, false);
                    return;
                }
                transitioning.remove(&cell);
                let control = publisher.control().value();
                let incarnation = control.incarnation;
                let code = control.code;
                let schema = control.schema;
                let durability_submitter = publisher.durability_submitter();
                cells.insert(
                    cell,
                    ActiveCell {
                        admission,
                        incarnation,
                        code,
                        schema,
                        interrupt,
                        publisher: Some(*publisher),
                        durability_submitter,
                        publications: VecDeque::new(),
                        publication_count: 0,
                        publication_bytes: 0,
                        unpublished_node_logs: 0,
                        queue: VecDeque::new(),
                        busy: false,
                        renewing: false,
                        fenced: false,
                        drain: None,
                        migrating: false,
                        shutdown_drain: false,
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
        TaskResult::Executed {
            cell,
            mut command,
            mut result,
            mut fenced,
        } => {
            let Some(active) = cells.get_mut(&cell) else {
                send_command_reply(&mut command, Err(Error::CellNotActive));
                return;
            };
            if node_lease.check().is_err() {
                result = Err(command.operation.unknown(Error::Fenced));
                fenced = true;
            }
            active.fenced |= fenced;
            if active.fenced {
                active.busy = false;
                fence_active(active);
                send_command_task_reply(&mut command, result);
                continue_cell(cell, pool, cells, transitioning, tasks, node_lease);
                return;
            }
            match result {
                Ok(CommandTaskResult::Recorded(outcome)) => {
                    active.busy = false;
                    send_command_reply(&mut command, Ok(outcome));
                }
                Ok(CommandTaskResult::Pending {
                    pending,
                    durability,
                }) => {
                    let retained_bytes = pending.retained_bytes();
                    active.publication_count += 1;
                    active.publication_bytes =
                        match active.publication_bytes.checked_add(retained_bytes) {
                            Some(bytes) => bytes,
                            None => {
                                active.busy = false;
                                active.fenced = true;
                                fence_active(active);
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
                        submitted_at: std::time::Instant::now(),
                        proof,
                    });
                    if durability.is_some() {
                        active.unpublished_node_logs += 1;
                        unpublished_node_log_bytes.fetch_add(retained_bytes, Ordering::AcqRel);
                    }
                    start_publication(cell, active, pool, tasks);
                    let pool = pool.clone();
                    tasks.spawn(async move {
                        prove_command(pool, command, outcome, commit_sequence, durability, object)
                            .await
                    });
                }
                Err(error) => {
                    active.busy = false;
                    send_command_reply(&mut command, Err(error));
                }
            }
            continue_cell(cell, pool, cells, transitioning, tasks, node_lease);
        }
        TaskResult::Proven {
            cell,
            mut command,
            mut result,
            mut fenced,
        } => {
            let Some(active) = cells.get_mut(&cell) else {
                send_command_reply(&mut command, Err(Error::CellNotActive));
                return;
            };
            if node_lease.check().is_err() {
                result = Err(command.operation.unknown(Error::Fenced));
                fenced = true;
            }
            active.busy = false;
            active.fenced |= fenced;
            if active.fenced {
                fence_active(active);
            }
            send_command_reply(&mut command, result);
            continue_cell(cell, pool, cells, transitioning, tasks, node_lease);
        }
        TaskResult::Published {
            cell,
            publisher,
            retained_bytes,
            node_logged,
            mut result,
            mut fenced,
        } => {
            let Some(active) = cells.get_mut(&cell) else {
                return;
            };
            let object_published = result.is_ok();
            if node_lease.check().is_err() {
                result = Err(Error::Fenced);
                fenced = true;
            }
            active.publisher = Some(*publisher);
            active.publication_count = active.publication_count.saturating_sub(1);
            active.publication_bytes = active.publication_bytes.saturating_sub(retained_bytes);
            if node_logged && object_published {
                active.unpublished_node_logs = active.unpublished_node_logs.saturating_sub(1);
                subtract_unpublished_bytes(unpublished_node_log_bytes, retained_bytes);
            }
            active.fenced |= fenced || result.is_err();
            if active.fenced {
                fence_active(active);
            } else {
                start_publication(cell, active, pool, tasks);
            }
            continue_cell(cell, pool, cells, transitioning, tasks, node_lease);
        }
        TaskResult::Queried {
            cell,
            mut query,
            mut result,
            mut fenced,
        } => {
            let Some(active) = cells.get_mut(&cell) else {
                send_query_reply(&mut query, Err(Error::CellNotActive));
                return;
            };
            if node_lease.check().is_err() {
                result = Err(Error::Fenced);
                fenced = true;
            }
            active.busy = false;
            active.fenced |= fenced;
            if active.fenced {
                fence_active(active);
            }
            send_query_reply(&mut query, result);
            continue_cell(cell, pool, cells, transitioning, tasks, node_lease);
        }
        TaskResult::Resolved {
            cell,
            mut resolve,
            mut result,
            mut fenced,
        } => {
            let Some(active) = cells.get_mut(&cell) else {
                send_resolve_reply(&mut resolve, Err(Error::CellNotActive));
                return;
            };
            if node_lease.check().is_err() {
                result = Ok(Resolution::Unknown);
                fenced = true;
            }
            active.busy = false;
            active.fenced |= fenced;
            if active.fenced {
                fence_active(active);
            }
            send_resolve_reply(&mut resolve, result);
            continue_cell(cell, pool, cells, transitioning, tasks, node_lease);
        }
        TaskResult::Migrated {
            cell,
            publisher,
            mut migration,
            mut result,
            mut fenced,
            preserve_owner,
            unpublished_bytes,
        } => {
            let Some(active) = cells.get_mut(&cell) else {
                send_migration_reply(&mut migration, Err(Error::CellNotActive));
                return;
            };
            if node_lease.check().is_err() {
                result = Err(Error::Fenced);
                fenced = true;
            }
            active.busy = false;
            active.migrating = false;
            active.publisher = Some(*publisher);
            if preserve_owner {
                active.unpublished_node_logs = active.unpublished_node_logs.saturating_add(1);
                unpublished_node_log_bytes.fetch_add(unpublished_bytes, Ordering::AcqRel);
            }
            active.fenced |= fenced;
            match result {
                Ok(outcome) if !active.fenced => {
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
            if active.fenced {
                fence_active(active);
            }
            continue_cell(cell, pool, cells, transitioning, tasks, node_lease);
        }
        TaskResult::Renewed {
            cell,
            publisher,
            mut result,
        } => {
            let Some(active) = cells.get_mut(&cell) else {
                return;
            };
            if node_lease.check().is_err() {
                result = Err(Error::Fenced);
            }
            active.renewing = false;
            active.publisher = Some(*publisher);
            if result.is_err() {
                active.fenced = true;
                fence_active(active);
            }
            continue_cell(cell, pool, cells, transitioning, tasks, node_lease);
        }
        TaskResult::Deactivated {
            cell,
            reply,
            shutdown_drain,
            result,
        } => {
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

fn fence_active(active: &mut ActiveCell) {
    fence_admission(&active.admission);
    while let Some(publication) = active.publications.pop_front() {
        active.publication_count = active.publication_count.saturating_sub(1);
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
    let Some(active) = cells.get_mut(&cell) else {
        return;
    };
    if active.busy || active.renewing {
        return;
    }
    if active.fenced {
        if active.publication_count == 0 && active.publisher.is_some() {
            let preserve_owner = active.unpublished_node_logs != 0;
            start_fenced_deactivate(cell, pool, cells, transitioning, tasks, preserve_owner);
        }
    } else if active.draining() && active.queue.is_empty() {
        if active.publication_count == 0 && active.publisher.is_some() {
            start_deactivate(cell, pool, cells, transitioning, tasks);
        }
    } else {
        start_next(active, pool, tasks, node_lease);
    }
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
) {
    let active_renewals = cells.values().filter(|active| active.renewing).count();
    let mut available = MAX_RENEWALS_IN_FLIGHT.saturating_sub(active_renewals);
    if available == 0 {
        return;
    }
    let now = std::time::Instant::now();
    for (cell, active) in cells.iter_mut() {
        if available == 0 {
            break;
        }
        if active.busy
            || active.renewing
            || active.fenced
            || active.draining()
            || active.publication_count != 0
            || !active.queue.is_empty()
            || active
                .publisher
                .as_ref()
                .is_none_or(|publisher| !publisher.renewal_due(now))
        {
            continue;
        }
        let Some(mut publisher) = active.publisher.take() else {
            continue;
        };
        active.renewing = true;
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
            reply: active.drain,
            shutdown_drain: active.shutdown_drain,
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
            reply: active.drain,
            shutdown_drain: active.shutdown_drain,
            result,
        }
    });
}

fn start_orphan_deactivate(
    cell: CellId,
    pool: &SqlWorkerPool,
    mut publisher: CellPublisher,
    transitioning: &mut HashSet<CellId>,
    tasks: &mut JoinSet<TaskResult>,
    shutdown_drain: bool,
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
            reply: None,
            shutdown_drain,
            result,
        }
    });
    transitioning.insert(cell);
}
