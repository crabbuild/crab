use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use tokio::{
    sync::{Semaphore, mpsc, oneshot},
    task::JoinSet,
};

mod handle;

pub use handle::CellHandle;
use handle::{CellAdmission, WorkAdmission};

use crate::{
    CatalogProof, CellAuthority, CellId, CellPublisher, Digest, Error, MutationIdentity,
    Resolution, SessionId, SqlWorkerPool, StoredOutcome, VersionedControl, WorkerExecution,
    worker::{CellReservation, Handler, Initializer, QueryHandler, WorkerState},
};

const INGRESS_REQUESTS: usize = 1_024;
const CELL_REQUESTS: usize = 64;
const CELL_BYTES: usize = 8 * 1024 * 1024;
const RENEWAL_SCAN: std::time::Duration = std::time::Duration::from_millis(100);
const MAX_RENEWALS_IN_FLIGHT: usize = 32;

/// Node-wide dispatcher for bounded per-Cell command mailboxes.
#[derive(Clone)]
pub struct CellRuntime {
    inner: Arc<RuntimeInner>,
}

pub(super) struct RuntimeInner {
    sender: mpsc::Sender<Message>,
    node_bytes: Arc<Semaphore>,
    session: SessionId,
    pool: SqlWorkerPool,
}

impl CellRuntime {
    /// Starts one dispatcher on the current Tokio runtime.
    pub fn new(
        pool: SqlWorkerPool,
        node_mailbox_bytes: usize,
        session: SessionId,
    ) -> crate::Result<Self> {
        if node_mailbox_bytes == 0 || node_mailbox_bytes > Semaphore::MAX_PERMITS {
            return Err(Error::Capacity("node mailbox bytes"));
        }
        let runtime = tokio::runtime::Handle::try_current().map_err(Error::RuntimeStart)?;
        let (sender, receiver) = mpsc::channel(INGRESS_REQUESTS);
        runtime.spawn(run(receiver, pool.clone()));
        Ok(Self {
            inner: Arc::new(RuntimeInner {
                sender,
                node_bytes: Arc::new(Semaphore::new(node_mailbox_bytes)),
                session,
                pool,
            }),
        })
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

    /// Cold-opens the exact authoritative root on the Cell's SQL worker.
    pub async fn activate_restored(
        &self,
        catalog: CatalogProof,
        replica: crab_ltx::CellReplica,
        authority: CellAuthority,
        observed: VersionedControl,
        destination: PathBuf,
    ) -> crate::Result<CellHandle> {
        let cell = self.activation_cell(&catalog, &observed)?;
        let reservation = self.inner.pool.reserve_activation()?;
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
        let database = verified.paged().prepare_writable().await?;
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

    async fn activate_inner(
        &self,
        catalog: CatalogProof,
        activation: Activation,
        replica: crab_ltx::CellReplica,
        authority: CellAuthority,
        observed: VersionedControl,
    ) -> crate::Result<CellHandle> {
        let cell = self.activation_cell(&catalog, &observed)?;
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
    Drain {
        cell: CellId,
        admission: Arc<CellAdmission>,
        reply: oneshot::Sender<crate::Result<()>>,
    },
}

struct QueuedCommand {
    cell: CellId,
    admission: Arc<CellAdmission>,
    identity: MutationIdentity,
    operation_digest: Digest,
    now_ms: i64,
    max_result_bytes: usize,
    next_due_ms: Option<i64>,
    handler: Option<Handler>,
    reply: oneshot::Sender<crate::Result<StoredOutcome>>,
    _work: WorkAdmission,
}

struct QueuedQuery {
    cell: CellId,
    admission: Arc<CellAdmission>,
    max_result_bytes: usize,
    handler: Option<QueryHandler>,
    reply: oneshot::Sender<crate::Result<Vec<u8>>>,
    _work: WorkAdmission,
}

struct QueuedResolve {
    cell: CellId,
    admission: Arc<CellAdmission>,
    identity: MutationIdentity,
    operation_digest: Digest,
    now_ms: i64,
    max_result_bytes: usize,
    reply: oneshot::Sender<crate::Result<Resolution>>,
    _work: WorkAdmission,
}

enum QueuedWork {
    Command(Box<QueuedCommand>),
    Query(Box<QueuedQuery>),
    Resolve(Box<QueuedResolve>),
}

struct ActiveCell {
    admission: Arc<CellAdmission>,
    publisher: Option<CellPublisher>,
    queue: VecDeque<QueuedWork>,
    busy: bool,
    renewing: bool,
    fenced: bool,
    drain: Option<oneshot::Sender<crate::Result<()>>>,
}

enum TaskResult {
    Activated {
        cell: CellId,
        publisher: Box<CellPublisher>,
        admission: Arc<CellAdmission>,
        reply: oneshot::Sender<crate::Result<Arc<CellAdmission>>>,
        result: crate::Result<()>,
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
        result: crate::Result<()>,
    },
}

async fn run(mut receiver: mpsc::Receiver<Message>, pool: SqlWorkerPool) {
    let mut cells = HashMap::<CellId, ActiveCell>::new();
    let mut transitioning = HashSet::<CellId>::new();
    let mut tasks = JoinSet::<TaskResult>::new();
    let mut renewal_tick = tokio::time::interval(RENEWAL_SCAN);
    renewal_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    renewal_tick.tick().await;
    loop {
        if tasks.is_empty() {
            tokio::select! {
                message = receiver.recv() => {
                    let Some(message) = message else { break; };
                    handle_message(message, &pool, &mut cells, &mut transitioning, &mut tasks);
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
                    while let Some(result) = tasks.join_next().await {
                        let Ok(result) = result else { return; };
                        handle_task(result, &pool, &mut cells, &mut transitioning, &mut tasks);
                    }
                    break;
                };
                handle_message(message, &pool, &mut cells, &mut transitioning, &mut tasks);
            }
            result = tasks.join_next() => {
                let Some(Ok(result)) = result else {
                    return;
                };
                handle_task(result, &pool, &mut cells, &mut transitioning, &mut tasks);
            }
            _ = renewal_tick.tick() => {
                start_due_renewals(&pool, &mut cells, &mut tasks);
            }
        }
    }
}

fn handle_message(
    message: Message,
    pool: &SqlWorkerPool,
    cells: &mut HashMap<CellId, ActiveCell>,
    transitioning: &mut HashSet<CellId>,
    tasks: &mut JoinSet<TaskResult>,
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
                TaskResult::Activated {
                    cell,
                    publisher,
                    admission,
                    reply,
                    result,
                }
            });
        }
        Message::Execute(command) => {
            let Some(active) = cells.get_mut(&command.cell) else {
                let _ = command.reply.send(Err(Error::CellNotActive));
                return;
            };
            if !Arc::ptr_eq(&active.admission, &command.admission) {
                let _ = command.reply.send(Err(Error::CellNotActive));
                return;
            }
            if active.fenced || active.drain.is_some() {
                let _ = command.reply.send(Err(if active.fenced {
                    Error::Fenced
                } else {
                    Error::CellDraining
                }));
                return;
            }
            active.queue.push_back(QueuedWork::Command(command));
            start_next(active, pool, tasks);
        }
        Message::Query(query) => {
            let Some(active) = cells.get_mut(&query.cell) else {
                let _ = query.reply.send(Err(Error::CellNotActive));
                return;
            };
            if !Arc::ptr_eq(&active.admission, &query.admission) {
                let _ = query.reply.send(Err(Error::CellNotActive));
                return;
            }
            if active.fenced || active.drain.is_some() {
                let _ = query.reply.send(Err(if active.fenced {
                    Error::Fenced
                } else {
                    Error::CellDraining
                }));
                return;
            }
            active.queue.push_back(QueuedWork::Query(query));
            start_next(active, pool, tasks);
        }
        Message::Resolve(resolve) => {
            let Some(active) = cells.get_mut(&resolve.cell) else {
                let _ = resolve.reply.send(Err(Error::CellNotActive));
                return;
            };
            if !Arc::ptr_eq(&active.admission, &resolve.admission) {
                let _ = resolve.reply.send(Err(Error::CellNotActive));
                return;
            }
            if active.fenced {
                let _ = resolve.reply.send(Ok(Resolution::Unknown));
                return;
            }
            if active.drain.is_some() {
                let _ = resolve.reply.send(Err(Error::CellDraining));
                return;
            }
            active.queue.push_back(QueuedWork::Resolve(resolve));
            start_next(active, pool, tasks);
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
            if active.drain.is_some() {
                let _ = reply.send(Err(Error::CellDraining));
                return;
            }
            active.drain = Some(reply);
            if !active.busy && !active.renewing && active.queue.is_empty() {
                start_deactivate(cell, pool, cells, transitioning, tasks);
            }
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
    let cuts = pool
        .bootstrap(
            cell,
            replica,
            destination,
            incarnation,
            schema,
            initialize,
            reservation,
        )
        .await?;
    let publication = async {
        let prepared = publisher.prepare_initial(&cuts).await?;
        publisher.publish_prepared(&prepared, None).await?;
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
    match work {
        QueuedWork::Command(command) => {
            let Some(publisher) = active.publisher.take() else {
                let _ = command.reply.send(Err(Error::Fenced));
                active.fenced = true;
                active.busy = false;
                return;
            };
            tasks.spawn(
                async move { execute_and_publish(pool, Box::new(publisher), command).await },
            );
        }
        QueuedWork::Query(query) => {
            tasks.spawn(async move { execute_query(pool, query).await });
        }
        QueuedWork::Resolve(resolve) => {
            tasks.spawn(async move { execute_resolve(pool, resolve).await });
        }
    }
}

async fn execute_and_publish(
    pool: SqlWorkerPool,
    mut publisher: Box<CellPublisher>,
    mut command: Box<QueuedCommand>,
) -> TaskResult {
    let execution = match command.handler.take() {
        Some(handler) => {
            pool.execute(
                command.cell,
                command.identity,
                command.operation_digest,
                command.now_ms,
                command.max_result_bytes,
                handler,
            )
            .await
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
                    .publish_prepared(&prepared, command.next_due_ms)
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
        result.map_err(|source| Error::OutcomeUnknown {
            request_id: command.identity.request_id,
            operation_digest: command.operation_digest,
            source: Box::new(source),
        })
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

async fn execute_query(pool: SqlWorkerPool, mut query: Box<QueuedQuery>) -> TaskResult {
    let result = match query.handler.take() {
        Some(handler) => {
            pool.query(query.cell, query.max_result_bytes, handler)
                .await
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

async fn execute_resolve(pool: SqlWorkerPool, resolve: Box<QueuedResolve>) -> TaskResult {
    let result = pool
        .resolve(
            resolve.cell,
            resolve.identity,
            resolve.operation_digest,
            resolve.now_ms,
            resolve.max_result_bytes,
        )
        .await;
    let fenced =
        result.is_err() && !matches!(pool.state(resolve.cell).await, Ok(WorkerState::Ready));
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
) {
    match result {
        TaskResult::Activated {
            cell,
            publisher,
            admission,
            reply,
            result,
        } => match result {
            Ok(()) => {
                if reply.send(Ok(admission.clone())).is_err() {
                    start_orphan_deactivate(cell, pool, *publisher, transitioning, tasks);
                    return;
                }
                transitioning.remove(&cell);
                cells.insert(
                    cell,
                    ActiveCell {
                        admission,
                        publisher: Some(*publisher),
                        queue: VecDeque::new(),
                        busy: false,
                        renewing: false,
                        fenced: false,
                        drain: None,
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
            command,
            result,
            fenced,
        } => {
            let Some(active) = cells.get_mut(&cell) else {
                let _ = command.reply.send(Err(Error::CellNotActive));
                return;
            };
            active.busy = false;
            active.publisher = Some(*publisher);
            active.fenced |= fenced;
            if active.fenced {
                fence_active(active);
            }
            let _ = command.reply.send(result);
            if active.drain.is_some() && !active.renewing && active.queue.is_empty() {
                start_deactivate(cell, pool, cells, transitioning, tasks);
            } else {
                start_next(active, pool, tasks);
            }
        }
        TaskResult::Queried {
            cell,
            query,
            result,
            fenced,
        } => {
            let Some(active) = cells.get_mut(&cell) else {
                let _ = query.reply.send(Err(Error::CellNotActive));
                return;
            };
            active.busy = false;
            active.fenced |= fenced;
            if active.fenced {
                fence_active(active);
            }
            let _ = query.reply.send(result);
            if active.drain.is_some() && !active.renewing && active.queue.is_empty() {
                start_deactivate(cell, pool, cells, transitioning, tasks);
            } else {
                start_next(active, pool, tasks);
            }
        }
        TaskResult::Resolved {
            cell,
            resolve,
            result,
            fenced,
        } => {
            let Some(active) = cells.get_mut(&cell) else {
                let _ = resolve.reply.send(Err(Error::CellNotActive));
                return;
            };
            active.busy = false;
            active.fenced |= fenced;
            if active.fenced {
                fence_active(active);
            }
            let _ = resolve.reply.send(result);
            if active.drain.is_some() && !active.renewing && active.queue.is_empty() {
                start_deactivate(cell, pool, cells, transitioning, tasks);
            } else {
                start_next(active, pool, tasks);
            }
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
            if active.drain.is_some() && !active.busy && active.queue.is_empty() {
                start_deactivate(cell, pool, cells, transitioning, tasks);
            } else {
                start_next(active, pool, tasks);
            }
        }
        TaskResult::Deactivated {
            cell,
            reply,
            result,
        } => {
            transitioning.remove(&cell);
            if let Some(reply) = reply {
                let _ = reply.send(result);
            }
        }
    }
}

fn fence_active(active: &mut ActiveCell) {
    active.admission.fenced.store(true, Ordering::Release);
    active.admission.draining.store(true, Ordering::Release);
    active.admission.requests.close();
    active.admission.bytes.close();
    while let Some(queued) = active.queue.pop_front() {
        match queued {
            QueuedWork::Command(command) => {
                let _ = command.reply.send(Err(Error::Fenced));
            }
            QueuedWork::Query(query) => {
                let _ = query.reply.send(Err(Error::Fenced));
            }
            QueuedWork::Resolve(resolve) => {
                let _ = resolve.reply.send(Ok(Resolution::Unknown));
            }
        }
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
            || active.drain.is_some()
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
            result,
        }
    });
    transitioning.insert(cell);
}
