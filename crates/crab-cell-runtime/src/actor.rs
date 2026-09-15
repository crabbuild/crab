use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError, mpsc, oneshot},
    task::JoinSet,
};

mod handle;

pub use handle::CellHandle;
use handle::{CellAdmission, WorkAdmission};

use crate::{
    CatalogProof, CellAuthority, CellId, CellPublisher, Digest, Error, InboxDelivery,
    MutationIdentity, Owner, Resolution, SessionId, SqlWorkerPool, StoredOutcome, Transition,
    VersionedControl, WorkerExecution,
    worker::{CellReservation, Handler, Initializer, QueryHandler, WorkerState},
};

const INGRESS_REQUESTS: usize = 1_024;
const CELL_REQUESTS: usize = 64;
const CELL_BYTES: usize = 8 * 1024 * 1024;
const RENEWAL_SCAN: std::time::Duration = std::time::Duration::from_millis(100);
const MAX_RENEWALS_IN_FLIGHT: usize = 32;
const TAKEOVER_OBSERVATION: std::time::Duration = std::time::Duration::from_secs(15);
const SQL_WALL_DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);

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

pub(super) struct RuntimeInner {
    sender: mpsc::Sender<Message>,
    node_bytes: Arc<Semaphore>,
    shutting_down: AtomicBool,
    session: SessionId,
    pool: SqlWorkerPool,
}

impl CellRuntime {
    /// Starts one dispatcher on the current Tokio runtime.
    pub fn new(
        pool: SqlWorkerPool,
        node_retained_bytes: usize,
        session: SessionId,
    ) -> crate::Result<Self> {
        if node_retained_bytes == 0 || node_retained_bytes > Semaphore::MAX_PERMITS {
            return Err(Error::Capacity("node retained bytes"));
        }
        let runtime = tokio::runtime::Handle::try_current().map_err(Error::RuntimeStart)?;
        let (sender, receiver) = mpsc::channel(INGRESS_REQUESTS);
        runtime.spawn(run(receiver, pool.clone()));
        Ok(Self {
            inner: Arc::new(RuntimeInner {
                sender,
                node_bytes: Arc::new(Semaphore::new(node_retained_bytes)),
                shutting_down: AtomicBool::new(false),
                session,
                pool,
            }),
        })
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
        match drain {
            Err(error) => Err(error),
            Ok(()) => workers,
        }
    }

    /// Reports whether node-wide admission has entered its terminal drain.
    #[must_use]
    pub fn is_shutting_down(&self) -> bool {
        self.inner.shutting_down.load(Ordering::Acquire)
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
    pub async fn takeover_unpublished<F>(
        &self,
        catalog: CatalogProof,
        replica: crab_ltx::CellReplica,
        authority: CellAuthority,
        mut observed: VersionedControl,
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
        loop {
            if observed.value().state != crate::ControlState::Recovering
                || observed.value().owner.is_none()
                || observed.value().root.is_some()
            {
                return Err(Error::Control(
                    "unpublished takeover requires an active rootless control",
                ));
            }
            tokio::time::sleep(TAKEOVER_OBSERVATION).await;
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
        destination: PathBuf,
    ) -> crate::Result<CellHandle> {
        self.ensure_running()?;
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

    /// Takes over an unchanged owner after the fixed observation interval.
    pub async fn takeover_restored(
        &self,
        catalog: CatalogProof,
        replica: crab_ltx::CellReplica,
        authority: CellAuthority,
        mut observed: VersionedControl,
        destination: PathBuf,
        owner: Owner,
    ) -> crate::Result<CellHandle> {
        self.ensure_running()?;
        let cell = self.claiming_cell(&catalog, &observed, &owner)?;
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
            tokio::time::sleep(TAKEOVER_OBSERVATION).await;
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

    async fn activate_restored_reserved(
        &self,
        catalog: CatalogProof,
        replica: crab_ltx::CellReplica,
        authority: CellAuthority,
        observed: VersionedControl,
        destination: PathBuf,
        reservation: CellReservation,
    ) -> crate::Result<CellHandle> {
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
        let (reply, response) = oneshot::channel();
        self.inner
            .sender
            .send(Message::Activate {
                cell,
                activation,
                publisher: Box::new(CellPublisher::new(replica, authority, observed)),
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
}

struct ActiveCell {
    admission: Arc<CellAdmission>,
    incarnation: crate::IncarnationId,
    code: Digest,
    schema: u32,
    interrupt: Arc<crab_ltx::rusqlite::InterruptHandle>,
    publisher: Option<CellPublisher>,
    queue: VecDeque<QueuedWork>,
    busy: bool,
    renewing: bool,
    fenced: bool,
    drain: Option<oneshot::Sender<crate::Result<()>>>,
    shutdown_drain: bool,
}

impl ActiveCell {
    fn draining(&self) -> bool {
        self.drain.is_some() || self.shutdown_drain
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
        publisher: Box<CellPublisher>,
        command: Box<QueuedCommand>,
        result: crate::Result<StoredOutcome>,
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

async fn run(mut receiver: mpsc::Receiver<Message>, pool: SqlWorkerPool) {
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
                    handle_message(message, &mut receiver, &pool, &mut cells, &mut transitioning, &mut tasks, &mut shutdown);
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
                        handle_task(result, &pool, &mut cells, &mut transitioning, &mut tasks, &mut shutdown);
                    }
                    break;
                };
                handle_message(message, &mut receiver, &pool, &mut cells, &mut transitioning, &mut tasks, &mut shutdown);
            }
            result = tasks.join_next() => {
                let Some(Ok(result)) = result else {
                    return;
                };
                handle_task(result, &pool, &mut cells, &mut transitioning, &mut tasks, &mut shutdown);
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
        if !active.busy && !active.renewing && active.queue.is_empty() {
            ready.push((*cell, active.fenced));
        }
    }
    for (cell, fenced) in ready {
        if fenced {
            start_fenced_deactivate(cell, pool, cells, transitioning, tasks);
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
) {
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
            let admission = Arc::new(CellAdmission {
                requests: Arc::new(Semaphore::new(CELL_REQUESTS)),
                bytes: Arc::new(Semaphore::new(CELL_BYTES)),
                draining: AtomicBool::new(false),
                fenced: AtomicBool::new(false),
            });
            let pool = pool.clone();
            tasks.spawn(async move {
                let mut publisher = publisher;
                let result = match activation {
                    Activation::Restored(activation) => {
                        let RestoredActivation {
                            database,
                            destination,
                            incarnation,
                            schema,
                            root,
                            reservation,
                        } = *activation;
                        pool.activate_restored(
                            cell,
                            database,
                            destination,
                            incarnation,
                            schema,
                            root,
                            reservation,
                        )
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
            if active.fenced || active.draining() {
                let error = if active.fenced {
                    Error::Fenced
                } else {
                    Error::CellDraining
                };
                send_command_reply(&mut command, Err(error));
                return;
            }
            active.queue.push_back(QueuedWork::Command(command));
            start_next(active, pool, tasks);
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
            if active.fenced || active.draining() {
                let error = if active.fenced {
                    Error::Fenced
                } else {
                    Error::CellDraining
                };
                send_query_reply(&mut query, Err(error));
                return;
            }
            active.queue.push_back(QueuedWork::Query(query));
            start_next(active, pool, tasks);
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
            if active.draining() {
                send_resolve_reply(&mut resolve, Err(Error::CellDraining));
                return;
            }
            active.queue.push_back(QueuedWork::Resolve(resolve));
            start_next(active, pool, tasks);
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
            if !active.busy && !active.renewing && active.queue.is_empty() {
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

fn start_next(active: &mut ActiveCell, pool: &SqlWorkerPool, tasks: &mut JoinSet<TaskResult>) {
    if active.busy || active.renewing || active.fenced {
        return;
    }
    let Some(work) = active.queue.pop_front() else {
        return;
    };
    active.busy = true;
    let pool = pool.clone();
    let interrupt = active.interrupt.clone();
    match work {
        QueuedWork::Command(mut command) => {
            let Some(publisher) = active.publisher.take() else {
                send_command_reply(&mut command, Err(Error::Fenced));
                active.fenced = true;
                active.busy = false;
                return;
            };
            tasks.spawn(async move {
                execute_and_publish(pool, Box::new(publisher), command, interrupt).await
            });
        }
        QueuedWork::Query(query) => {
            tasks.spawn(async move { execute_query(pool, query, interrupt).await });
        }
        QueuedWork::Resolve(resolve) => {
            tasks.spawn(async move { execute_resolve(pool, resolve, interrupt).await });
        }
    }
}

async fn execute_and_publish(
    pool: SqlWorkerPool,
    mut publisher: Box<CellPublisher>,
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
                        publisher,
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
        Ok(WorkerExecution::Recorded(outcome)) => (Ok(outcome), false),
        Ok(WorkerExecution::Pending(pending)) => {
            let result = async {
                let prepared = publisher.prepare(&pending).await?;
                pool.bind_prepared(command.cell, prepared.clone()).await?;
                let root = publisher
                    .publish_prepared(&prepared, pending.next_due_ms())
                    .await?;
                pool.confirm_published(command.cell, root).await
            }
            .await;
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
        publisher,
        command,
        result,
        fenced,
    }
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
                cells.insert(
                    cell,
                    ActiveCell {
                        admission,
                        incarnation,
                        code,
                        schema,
                        interrupt,
                        publisher: Some(*publisher),
                        queue: VecDeque::new(),
                        busy: false,
                        renewing: false,
                        fenced: false,
                        drain: None,
                        shutdown_drain: false,
                    },
                );
            }
            Err(error) => {
                transitioning.remove(&cell);
                let _ = reply.send(Err(error));
            }
        },
        TaskResult::Executed {
            cell,
            publisher,
            mut command,
            result,
            fenced,
        } => {
            let Some(active) = cells.get_mut(&cell) else {
                send_command_reply(&mut command, Err(Error::CellNotActive));
                return;
            };
            active.busy = false;
            active.publisher = Some(*publisher);
            active.fenced |= fenced;
            if active.fenced {
                fence_active(active);
            }
            send_command_reply(&mut command, result);
            continue_cell(cell, pool, cells, transitioning, tasks);
        }
        TaskResult::Queried {
            cell,
            mut query,
            result,
            fenced,
        } => {
            let Some(active) = cells.get_mut(&cell) else {
                send_query_reply(&mut query, Err(Error::CellNotActive));
                return;
            };
            active.busy = false;
            active.fenced |= fenced;
            if active.fenced {
                fence_active(active);
            }
            send_query_reply(&mut query, result);
            continue_cell(cell, pool, cells, transitioning, tasks);
        }
        TaskResult::Resolved {
            cell,
            mut resolve,
            result,
            fenced,
        } => {
            let Some(active) = cells.get_mut(&cell) else {
                send_resolve_reply(&mut resolve, Err(Error::CellNotActive));
                return;
            };
            active.busy = false;
            active.fenced |= fenced;
            if active.fenced {
                fence_active(active);
            }
            send_resolve_reply(&mut resolve, result);
            continue_cell(cell, pool, cells, transitioning, tasks);
        }
        TaskResult::Renewed {
            cell,
            publisher,
            result,
        } => {
            let Some(active) = cells.get_mut(&cell) else {
                return;
            };
            active.renewing = false;
            active.publisher = Some(*publisher);
            if result.is_err() {
                active.fenced = true;
                fence_active(active);
            }
            continue_cell(cell, pool, cells, transitioning, tasks);
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

fn fence_active(active: &mut ActiveCell) {
    fence_admission(&active.admission);
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
        }
    }
}

fn continue_cell(
    cell: CellId,
    pool: &SqlWorkerPool,
    cells: &mut HashMap<CellId, ActiveCell>,
    transitioning: &mut HashSet<CellId>,
    tasks: &mut JoinSet<TaskResult>,
) {
    let Some(active) = cells.get_mut(&cell) else {
        return;
    };
    if active.busy || active.renewing {
        return;
    }
    if active.fenced {
        start_fenced_deactivate(cell, pool, cells, transitioning, tasks);
    } else if active.draining() && active.queue.is_empty() {
        start_deactivate(cell, pool, cells, transitioning, tasks);
    } else {
        start_next(active, pool, tasks);
    }
}

fn fence_admission(admission: &CellAdmission) {
    admission.fenced.store(true, Ordering::Release);
    admission.draining.store(true, Ordering::Release);
    admission.requests.close();
    admission.bytes.close();
}

fn send_command_reply(command: &mut QueuedCommand, result: crate::Result<StoredOutcome>) {
    if let Some(reply) = command.reply.take() {
        let _ = reply.send(result);
    }
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
) {
    let Some(active) = cells.remove(&cell) else {
        return;
    };
    transitioning.insert(cell);
    let pool = pool.clone();
    tasks.spawn(async move {
        let result = async {
            pool.discard(cell).await?;
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
