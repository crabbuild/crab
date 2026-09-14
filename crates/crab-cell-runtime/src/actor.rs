use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError, mpsc, oneshot},
    task::JoinSet,
};

use crate::{
    CellAuthority, CellExecutor, CellId, CellPublisher, Digest, Error, MutationIdentity,
    SqlWorkerPool, StoredOutcome, VersionedControl, WorkerExecution,
    worker::{Handler, WorkerState},
};

const INGRESS_REQUESTS: usize = 1_024;
const CELL_REQUESTS: usize = 64;
const CELL_BYTES: usize = 8 * 1024 * 1024;
const MAX_OPERATION_BYTES: usize = 1024 * 1024;
const MAX_RESULT_BYTES: usize = 1024 * 1024;

/// Node-wide dispatcher for bounded per-Cell command mailboxes.
#[derive(Clone)]
pub struct CellRuntime {
    inner: Arc<RuntimeInner>,
}

/// Cloneable capability for one activated Cell.
#[derive(Clone)]
pub struct CellHandle {
    cell: CellId,
    inner: Arc<RuntimeInner>,
    admission: Arc<CellAdmission>,
}

struct RuntimeInner {
    sender: mpsc::Sender<Message>,
    node_bytes: Arc<Semaphore>,
}

struct CellAdmission {
    requests: Arc<Semaphore>,
    bytes: Arc<Semaphore>,
    draining: AtomicBool,
    fenced: AtomicBool,
}

impl CellRuntime {
    /// Starts one dispatcher on the current Tokio runtime.
    pub fn new(pool: SqlWorkerPool, node_mailbox_bytes: usize) -> crate::Result<Self> {
        if node_mailbox_bytes == 0 || node_mailbox_bytes > Semaphore::MAX_PERMITS {
            return Err(Error::Capacity("node mailbox bytes"));
        }
        let runtime = tokio::runtime::Handle::try_current().map_err(Error::RuntimeStart)?;
        let (sender, receiver) = mpsc::channel(INGRESS_REQUESTS);
        runtime.spawn(run(receiver, pool));
        Ok(Self {
            inner: Arc::new(RuntimeInner {
                sender,
                node_bytes: Arc::new(Semaphore::new(node_mailbox_bytes)),
            }),
        })
    }

    /// Activates one restored executor and binds its publication authority.
    pub async fn activate(
        &self,
        cell: CellId,
        executor: CellExecutor,
        replica: crab_ltx::CellReplica,
        authority: CellAuthority,
        observed: VersionedControl,
    ) -> crate::Result<CellHandle> {
        if observed.value().cell != cell {
            return Err(Error::Control("activation control changed Cell"));
        }
        let (reply, response) = oneshot::channel();
        self.inner
            .sender
            .send(Message::Activate {
                cell,
                executor: Box::new(executor),
                publisher: Box::new(CellPublisher::new(replica, authority, observed)),
                reply,
            })
            .await
            .map_err(|_| Error::RuntimeClosed)?;
        let admission = response.await.map_err(|_| Error::RuntimeClosed)??;
        Ok(CellHandle {
            cell,
            inner: self.inner.clone(),
            admission,
        })
    }
}

impl CellHandle {
    /// Runs and publishes one command while retaining admission after cancellation.
    pub async fn execute<F>(
        &self,
        identity: MutationIdentity,
        operation_digest: Digest,
        now_ms: i64,
        operation_bytes: usize,
        max_result_bytes: usize,
        next_due_ms: Option<i64>,
        handler: F,
    ) -> crate::Result<StoredOutcome>
    where
        F: for<'connection> FnOnce(
                &crab_ltx::rusqlite::Transaction<'connection>,
            ) -> crate::Result<crate::HandlerOutcome>
            + Send
            + 'static,
    {
        if operation_bytes > MAX_OPERATION_BYTES || max_result_bytes > MAX_RESULT_BYTES {
            return Err(Error::Capacity("command operation or result bytes"));
        }
        let reservation_bytes = operation_bytes
            .checked_add(max_result_bytes)
            .ok_or(Error::Capacity("command mailbox bytes"))?;
        if reservation_bytes == 0 {
            return Err(Error::Capacity("command mailbox bytes"));
        }
        if self.admission.fenced.load(Ordering::Acquire) {
            return Err(Error::Fenced);
        }
        if self.admission.draining.load(Ordering::Acquire) {
            return Err(Error::CellDraining);
        }
        let request = try_one(self.admission.requests.clone(), "Cell mailbox requests")?;
        let cell_bytes = try_many(
            self.admission.bytes.clone(),
            reservation_bytes,
            "Cell mailbox bytes",
        )?;
        let node_bytes = try_many(
            self.inner.node_bytes.clone(),
            reservation_bytes,
            "node mailbox bytes",
        )?;
        if self.admission.fenced.load(Ordering::Acquire) {
            return Err(Error::Fenced);
        }
        if self.admission.draining.load(Ordering::Acquire) {
            return Err(Error::CellDraining);
        }

        let (reply, response) = oneshot::channel();
        self.inner
            .sender
            .send(Message::Execute(Box::new(QueuedCommand {
                cell: self.cell,
                admission: self.admission.clone(),
                identity,
                operation_digest,
                now_ms,
                max_result_bytes,
                next_due_ms,
                handler: Some(Box::new(handler)),
                reply,
                _request: request,
                _cell_bytes: cell_bytes,
                _node_bytes: node_bytes,
            })))
            .await
            .map_err(|_| Error::RuntimeClosed)?;
        response.await.map_err(|_| Error::RuntimeClosed)?
    }

    /// Stops admission, publishes accepted commands, then closes the SQLite handle.
    pub async fn drain(&self) -> crate::Result<()> {
        if self
            .admission
            .draining
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(Error::CellDraining);
        }
        self.admission.requests.close();
        self.admission.bytes.close();
        let (reply, response) = oneshot::channel();
        self.inner
            .sender
            .send(Message::Drain {
                cell: self.cell,
                admission: self.admission.clone(),
                reply,
            })
            .await
            .map_err(|_| Error::RuntimeClosed)?;
        response.await.map_err(|_| Error::RuntimeClosed)?
    }
}

fn try_one(
    semaphore: Arc<Semaphore>,
    resource: &'static str,
) -> crate::Result<OwnedSemaphorePermit> {
    semaphore
        .try_acquire_owned()
        .map_err(|error| admission_error(error, resource))
}

fn try_many(
    semaphore: Arc<Semaphore>,
    permits: usize,
    resource: &'static str,
) -> crate::Result<OwnedSemaphorePermit> {
    let permits = u32::try_from(permits).map_err(|_| Error::Capacity(resource))?;
    semaphore
        .try_acquire_many_owned(permits)
        .map_err(|error| admission_error(error, resource))
}

fn admission_error(error: TryAcquireError, resource: &'static str) -> Error {
    match error {
        TryAcquireError::Closed => Error::CellDraining,
        TryAcquireError::NoPermits => Error::Capacity(resource),
    }
}

enum Message {
    Activate {
        cell: CellId,
        executor: Box<CellExecutor>,
        publisher: Box<CellPublisher>,
        reply: oneshot::Sender<crate::Result<Arc<CellAdmission>>>,
    },
    Execute(Box<QueuedCommand>),
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
    _request: OwnedSemaphorePermit,
    _cell_bytes: OwnedSemaphorePermit,
    _node_bytes: OwnedSemaphorePermit,
}

struct ActiveCell {
    admission: Arc<CellAdmission>,
    publisher: Option<CellPublisher>,
    queue: VecDeque<Box<QueuedCommand>>,
    busy: bool,
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
    loop {
        if tasks.is_empty() {
            let Some(message) = receiver.recv().await else {
                break;
            };
            handle_message(message, &pool, &mut cells, &mut transitioning, &mut tasks);
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
            executor,
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
                TaskResult::Activated {
                    cell,
                    publisher,
                    admission,
                    reply,
                    result: pool.activate(cell, *executor).await,
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
            active.queue.push_back(command);
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
            if !active.busy && active.queue.is_empty() {
                start_deactivate(cell, pool, cells, transitioning, tasks);
            }
        }
    }
}

fn start_next(active: &mut ActiveCell, pool: &SqlWorkerPool, tasks: &mut JoinSet<TaskResult>) {
    if active.busy || active.fenced {
        return;
    }
    let Some(command) = active.queue.pop_front() else {
        return;
    };
    let Some(publisher) = active.publisher.take() else {
        let _ = command.reply.send(Err(Error::Fenced));
        active.fenced = true;
        return;
    };
    active.busy = true;
    let pool = pool.clone();
    tasks.spawn(async move { execute_and_publish(pool, Box::new(publisher), command).await });
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
                    start_orphan_deactivate(cell, pool, transitioning, tasks);
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
                active.admission.fenced.store(true, Ordering::Release);
                active.admission.draining.store(true, Ordering::Release);
                active.admission.requests.close();
                active.admission.bytes.close();
                while let Some(queued) = active.queue.pop_front() {
                    let _ = queued.reply.send(Err(Error::Fenced));
                }
            }
            let _ = command.reply.send(result);
            if active.drain.is_some() && active.queue.is_empty() {
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
        TaskResult::Deactivated {
            cell,
            reply: active.drain,
            result: pool.deactivate(cell).await,
        }
    });
}

fn start_orphan_deactivate(
    cell: CellId,
    pool: &SqlWorkerPool,
    transitioning: &mut HashSet<CellId>,
    tasks: &mut JoinSet<TaskResult>,
) {
    let pool = pool.clone();
    tasks.spawn(async move {
        TaskResult::Deactivated {
            cell,
            reply: None,
            result: pool.deactivate(cell).await,
        }
    });
    transitioning.insert(cell);
}
