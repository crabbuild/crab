use std::{
    collections::HashMap,
    panic::{AssertUnwindSafe, catch_unwind},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

use tokio::sync::{mpsc, oneshot};

use crate::{
    CellExecutor, CellId, CommandExecution, Digest, Error, HandlerOutcome, InboxDelivery,
    MutationIdentity, PendingCommit, Resolution, Result, StoredOutcome,
};

const MAX_WORKERS: usize = 16;
const MAX_ACTIVE_CELLS: usize = 10_000;
const WORKER_QUEUE: usize = 256;
const DEFAULT_PAGE_IO_DEADLINE: Duration = Duration::from_secs(30);

pub(crate) type Handler = Box<
    dyn for<'connection> FnOnce(
            &crab_ltx::rusqlite::Transaction<'connection>,
        ) -> Result<HandlerOutcome>
        + Send
        + 'static,
>;

pub(crate) type Initializer = Box<
    dyn for<'connection> FnOnce(&crab_ltx::rusqlite::Transaction<'connection>) -> Result<()>
        + Send
        + 'static,
>;

pub(crate) type QueryHandler =
    Box<dyn FnOnce(&crab_ltx::rusqlite::Connection) -> Result<Vec<u8>> + Send + 'static>;

pub(crate) struct BootstrapExecution {
    pub(crate) cuts: crab_ltx::CaptureBatch,
    pub(crate) next_due_ms: Option<i64>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkerState {
    Ready,
    Pending,
    Fenced,
}

/// Result returned by one SQL worker without releasing pending command output.
#[derive(Clone)]
pub enum WorkerExecution {
    Recorded(StoredOutcome),
    Pending(Box<PendingCommit>),
}

/// Fixed shard pool whose threads exclusively own all active SQLite handles.
#[derive(Clone)]
pub struct SqlWorkerPool {
    inner: Arc<PoolInner>,
}

impl SqlWorkerPool {
    /// Starts a fixed pool with bounded queues and active-Cell admission.
    pub fn new(worker_count: usize, max_active_cells: usize) -> Result<Self> {
        if worker_count == 0
            || worker_count > MAX_WORKERS
            || max_active_cells == 0
            || max_active_cells > MAX_ACTIVE_CELLS
        {
            return Err(Error::Capacity("invalid SQL worker configuration"));
        }
        let active = Arc::new(AtomicUsize::new(0));
        let mut workers = Vec::with_capacity(worker_count);
        let mut threads: Vec<JoinHandle<()>> = Vec::with_capacity(worker_count);
        for index in 0..worker_count {
            let (sender, receiver) = mpsc::channel(WORKER_QUEUE);
            let thread = match std::thread::Builder::new()
                .name(format!("crab-cell-sql-{index}"))
                .spawn(move || run_worker(receiver))
            {
                Ok(thread) => thread,
                Err(error) => {
                    drop(workers);
                    for thread in threads {
                        let _ = thread.join();
                    }
                    return Err(Error::WorkerStart(Box::new(error)));
                }
            };
            workers.push(sender);
            threads.push(thread);
        }
        Ok(Self {
            inner: Arc::new(PoolInner {
                lifecycle: Mutex::new(WorkerLifecycle {
                    workers,
                    threads,
                    closing: false,
                }),
                active,
                max_active_cells,
                worker_count,
            }),
        })
    }

    /// Uses the design's CPU-derived worker count, capped at sixteen.
    pub fn for_system(max_active_cells: usize) -> Result<Self> {
        let workers = std::thread::available_parallelism()
            .map(usize::from)
            .unwrap_or(1)
            .clamp(1, MAX_WORKERS);
        Self::new(workers, max_active_cells)
    }

    /// Moves a newly restored/opened Cell executor onto its sole worker.
    pub async fn activate(&self, cell: CellId, executor: CellExecutor) -> Result<()> {
        let reservation = self.reserve_activation()?;
        let (reply, response) = oneshot::channel();
        self.send(
            cell,
            WorkerCommand::Activate {
                cell,
                executor: Box::new(executor),
                reservation,
                reply,
            },
        )
        .await?;
        receive(response).await
    }

    /// Creates and captures a new Cell on its assigned SQL worker.
    pub(crate) async fn bootstrap(
        &self,
        cell: CellId,
        replica: crab_ltx::CellReplica,
        destination: PathBuf,
        incarnation: crate::IncarnationId,
        schema: u32,
        initialize: Initializer,
        reservation: CellReservation,
    ) -> Result<BootstrapExecution> {
        let (reply, response) = oneshot::channel();
        self.send(
            cell,
            WorkerCommand::Bootstrap(Box::new(WorkerBootstrap {
                cell,
                replica,
                destination,
                incarnation,
                schema,
                initialize,
                reservation,
                reply,
            })),
        )
        .await?;
        receive(response).await
    }

    /// Opens and verifies one exact immutable root on its assigned SQL worker.
    pub(crate) async fn activate_restored(
        &self,
        cell: CellId,
        database: crab_ltx::CellWritableDatabase,
        destination: PathBuf,
        incarnation: crate::IncarnationId,
        schema: u32,
        root: crab_ltx::RootRef,
        reservation: CellReservation,
    ) -> Result<()> {
        let (reply, response) = oneshot::channel();
        self.send(
            cell,
            WorkerCommand::ActivateRestored {
                cell,
                database: Box::new(database),
                destination,
                incarnation,
                schema,
                root,
                reservation,
                reply,
            },
        )
        .await?;
        receive(response).await
    }

    /// Runs one synchronous handler on the Cell's assigned SQLite worker.
    pub async fn execute<F>(
        &self,
        cell: CellId,
        identity: MutationIdentity,
        operation_digest: Digest,
        now_ms: i64,
        max_result_bytes: usize,
        handler: F,
    ) -> Result<WorkerExecution>
    where
        F: for<'connection> FnOnce(
                &crab_ltx::rusqlite::Transaction<'connection>,
            ) -> Result<HandlerOutcome>
            + Send
            + 'static,
    {
        self.execute_until(
            cell,
            identity,
            operation_digest,
            now_ms,
            max_result_bytes,
            Instant::now() + DEFAULT_PAGE_IO_DEADLINE,
            Box::new(handler),
        )
        .await
    }

    pub(crate) async fn execute_until(
        &self,
        cell: CellId,
        identity: MutationIdentity,
        operation_digest: Digest,
        now_ms: i64,
        max_result_bytes: usize,
        deadline: Instant,
        handler: Handler,
    ) -> Result<WorkerExecution> {
        let (reply, response) = oneshot::channel();
        self.send(
            cell,
            WorkerCommand::Execute {
                cell,
                identity,
                operation_digest,
                now_ms,
                max_result_bytes,
                deadline,
                handler,
                reply,
            },
        )
        .await?;
        receive(response).await
    }

    /// Applies one destination inbox delivery on the Cell's assigned worker.
    pub(crate) async fn deliver_effect(
        &self,
        cell: CellId,
        delivery: InboxDelivery,
        now_ms: i64,
        max_result_bytes: usize,
        deadline: Instant,
        handler: Handler,
    ) -> Result<WorkerExecution> {
        let (reply, response) = oneshot::channel();
        self.send(
            cell,
            WorkerCommand::DeliverEffect {
                cell,
                delivery,
                now_ms,
                max_result_bytes,
                deadline,
                handler,
                reply,
            },
        )
        .await?;
        receive(response).await
    }

    /// Runs one synchronous read on the Cell's assigned SQLite worker.
    pub(crate) async fn query(
        &self,
        cell: CellId,
        max_result_bytes: usize,
        deadline: Instant,
        handler: QueryHandler,
    ) -> Result<Vec<u8>> {
        let (reply, response) = oneshot::channel();
        self.send(
            cell,
            WorkerCommand::Query {
                cell,
                max_result_bytes,
                deadline,
                handler,
                reply,
            },
        )
        .await?;
        receive(response).await
    }

    /// Resolves one request ledger entry on the Cell's assigned worker.
    pub(crate) async fn resolve(
        &self,
        cell: CellId,
        identity: MutationIdentity,
        operation_digest: Digest,
        now_ms: i64,
        max_result_bytes: usize,
        deadline: Instant,
    ) -> Result<Resolution> {
        let (reply, response) = oneshot::channel();
        self.send(
            cell,
            WorkerCommand::Resolve {
                cell,
                identity,
                operation_digest,
                now_ms,
                max_result_bytes,
                deadline,
                reply,
            },
        )
        .await?;
        receive(response).await
    }

    /// Resolves one destination inbox identity on the Cell's assigned worker.
    pub(crate) async fn resolve_effect(
        &self,
        cell: CellId,
        delivery: InboxDelivery,
        now_ms: i64,
        max_result_bytes: usize,
        deadline: Instant,
    ) -> Result<Resolution> {
        let (reply, response) = oneshot::channel();
        self.send(
            cell,
            WorkerCommand::ResolveEffect {
                cell,
                delivery,
                now_ms,
                max_result_bytes,
                deadline,
                reply,
            },
        )
        .await?;
        receive(response).await
    }

    /// Binds the immutable proposal before any authority CAS can use it.
    pub async fn bind_prepared(
        &self,
        cell: CellId,
        prepared: crab_ltx::PreparedRoot,
    ) -> Result<()> {
        let (reply, response) = oneshot::channel();
        self.send(
            cell,
            WorkerCommand::BindPrepared {
                cell,
                prepared: Box::new(prepared),
                reply,
            },
        )
        .await?;
        receive(response).await
    }

    /// Returns a clone of worker-owned publication state without releasing it.
    pub async fn pending(&self, cell: CellId) -> Result<Option<PendingCommit>> {
        let (reply, response) = oneshot::channel();
        self.send(cell, WorkerCommand::Pending { cell, reply })
            .await?;
        receive(response).await
    }

    /// Releases one worker-retained result after exact root publication.
    pub async fn confirm_published(
        &self,
        cell: CellId,
        root: crab_ltx::RootRef,
    ) -> Result<StoredOutcome> {
        let (reply, response) = oneshot::channel();
        self.send(cell, WorkerCommand::ConfirmPublished { cell, root, reply })
            .await?;
        receive(response).await
    }

    /// Stops admission after a publication or worker invariant becomes unsafe.
    pub(crate) async fn fence(&self, cell: CellId) -> Result<()> {
        let (reply, response) = oneshot::channel();
        self.send(cell, WorkerCommand::Fence { cell, reply })
            .await?;
        receive(response).await
    }

    pub(crate) async fn state(&self, cell: CellId) -> Result<WorkerState> {
        let (reply, response) = oneshot::channel();
        self.send(cell, WorkerCommand::State { cell, reply })
            .await?;
        receive(response).await
    }

    pub(crate) async fn interrupt_handle(
        &self,
        cell: CellId,
    ) -> Result<crab_ltx::rusqlite::InterruptHandle> {
        let (reply, response) = oneshot::channel();
        self.send(cell, WorkerCommand::InterruptHandle { cell, reply })
            .await?;
        receive(response).await
    }

    /// Closes and removes one fully drained Cell from its worker.
    pub async fn deactivate(&self, cell: CellId) -> Result<()> {
        let (reply, response) = oneshot::channel();
        self.send(cell, WorkerCommand::Deactivate { cell, reply })
            .await?;
        receive(response).await
    }

    /// Removes and closes a fenced Cell for authoritative-root recovery.
    pub(crate) async fn discard(&self, cell: CellId) -> Result<()> {
        let (reply, response) = oneshot::channel();
        self.send(cell, WorkerCommand::Discard { cell, reply })
            .await?;
        receive(response).await
    }

    /// Closes the empty pool and joins every SQL worker thread.
    ///
    /// All Cells must first be drained and deactivated. Once shutdown starts,
    /// every clone is permanently closed and a second call returns
    /// [`Error::RuntimeClosed`].
    pub async fn shutdown(&self) -> Result<()> {
        let threads = {
            let mut lifecycle = self
                .inner
                .lifecycle
                .lock()
                .map_err(|_| Error::RuntimeClosed)?;
            if lifecycle.closing {
                return Err(Error::RuntimeClosed);
            }
            if self.inner.active.load(Ordering::Acquire) != 0 {
                return Err(Error::Control(
                    "SQL worker shutdown requires every Cell to be deactivated",
                ));
            }
            lifecycle.closing = true;
            lifecycle.workers.clear();
            std::mem::take(&mut lifecycle.threads)
        };
        tokio::task::spawn_blocking(move || {
            let mut panicked = false;
            for thread in threads {
                if thread.join().is_err() {
                    panicked = true;
                }
            }
            if panicked {
                Err(Error::WorkerPanic)
            } else {
                Ok(())
            }
        })
        .await
        .map_err(Error::WorkerJoin)?
    }

    async fn send(&self, cell: CellId, command: WorkerCommand) -> Result<()> {
        let sender = {
            let lifecycle = self
                .inner
                .lifecycle
                .lock()
                .map_err(|_| Error::RuntimeClosed)?;
            if lifecycle.closing {
                return Err(Error::RuntimeClosed);
            }
            lifecycle.workers[worker_index(cell, self.inner.worker_count)].clone()
        };
        sender.send(command).await.map_err(|_| Error::RuntimeClosed)
    }

    pub(crate) fn reserve_activation(&self) -> Result<CellReservation> {
        let lifecycle = self
            .inner
            .lifecycle
            .lock()
            .map_err(|_| Error::RuntimeClosed)?;
        if lifecycle.closing {
            return Err(Error::RuntimeClosed);
        }
        self.inner
            .active
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < self.inner.max_active_cells).then_some(active + 1)
            })
            .map_err(|_| Error::Capacity("active Cells per node"))?;
        Ok(CellReservation {
            active: self.inner.active.clone(),
        })
    }
}

struct PoolInner {
    lifecycle: Mutex<WorkerLifecycle>,
    active: Arc<AtomicUsize>,
    max_active_cells: usize,
    worker_count: usize,
}

struct WorkerLifecycle {
    workers: Vec<mpsc::Sender<WorkerCommand>>,
    threads: Vec<JoinHandle<()>>,
    closing: bool,
}

impl Drop for PoolInner {
    fn drop(&mut self) {
        let lifecycle = match self.lifecycle.get_mut() {
            Ok(lifecycle) => lifecycle,
            Err(poisoned) => poisoned.into_inner(),
        };
        lifecycle.workers.clear();
        let threads = std::mem::take(&mut lifecycle.threads);
        for thread in threads {
            let _ = thread.join();
        }
    }
}

enum WorkerCommand {
    Activate {
        cell: CellId,
        executor: Box<CellExecutor>,
        reservation: CellReservation,
        reply: oneshot::Sender<Result<()>>,
    },
    ActivateRestored {
        cell: CellId,
        database: Box<crab_ltx::CellWritableDatabase>,
        destination: PathBuf,
        incarnation: crate::IncarnationId,
        schema: u32,
        root: crab_ltx::RootRef,
        reservation: CellReservation,
        reply: oneshot::Sender<Result<()>>,
    },
    Bootstrap(Box<WorkerBootstrap>),
    Execute {
        cell: CellId,
        identity: MutationIdentity,
        operation_digest: Digest,
        now_ms: i64,
        max_result_bytes: usize,
        deadline: Instant,
        handler: Handler,
        reply: oneshot::Sender<Result<WorkerExecution>>,
    },
    DeliverEffect {
        cell: CellId,
        delivery: InboxDelivery,
        now_ms: i64,
        max_result_bytes: usize,
        deadline: Instant,
        handler: Handler,
        reply: oneshot::Sender<Result<WorkerExecution>>,
    },
    Query {
        cell: CellId,
        max_result_bytes: usize,
        deadline: Instant,
        handler: QueryHandler,
        reply: oneshot::Sender<Result<Vec<u8>>>,
    },
    Resolve {
        cell: CellId,
        identity: MutationIdentity,
        operation_digest: Digest,
        now_ms: i64,
        max_result_bytes: usize,
        deadline: Instant,
        reply: oneshot::Sender<Result<Resolution>>,
    },
    ResolveEffect {
        cell: CellId,
        delivery: InboxDelivery,
        now_ms: i64,
        max_result_bytes: usize,
        deadline: Instant,
        reply: oneshot::Sender<Result<Resolution>>,
    },
    BindPrepared {
        cell: CellId,
        prepared: Box<crab_ltx::PreparedRoot>,
        reply: oneshot::Sender<Result<()>>,
    },
    Pending {
        cell: CellId,
        reply: oneshot::Sender<Result<Option<PendingCommit>>>,
    },
    ConfirmPublished {
        cell: CellId,
        root: crab_ltx::RootRef,
        reply: oneshot::Sender<Result<StoredOutcome>>,
    },
    Fence {
        cell: CellId,
        reply: oneshot::Sender<Result<()>>,
    },
    State {
        cell: CellId,
        reply: oneshot::Sender<Result<WorkerState>>,
    },
    InterruptHandle {
        cell: CellId,
        reply: oneshot::Sender<Result<crab_ltx::rusqlite::InterruptHandle>>,
    },
    Deactivate {
        cell: CellId,
        reply: oneshot::Sender<Result<()>>,
    },
    Discard {
        cell: CellId,
        reply: oneshot::Sender<Result<()>>,
    },
}

struct WorkerBootstrap {
    cell: CellId,
    replica: crab_ltx::CellReplica,
    destination: PathBuf,
    incarnation: crate::IncarnationId,
    schema: u32,
    initialize: Initializer,
    reservation: CellReservation,
    reply: oneshot::Sender<Result<BootstrapExecution>>,
}

struct ActiveCell {
    executor: CellExecutor,
    _reservation: CellReservation,
}

pub(crate) struct CellReservation {
    active: Arc<AtomicUsize>,
}

impl Drop for CellReservation {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
    }
}

fn run_worker(mut receiver: mpsc::Receiver<WorkerCommand>) {
    let mut cells = HashMap::new();
    while let Some(command) = receiver.blocking_recv() {
        match command {
            WorkerCommand::Activate {
                cell,
                executor,
                reservation,
                reply,
            } => {
                let result = match cells.entry(cell) {
                    std::collections::hash_map::Entry::Occupied(_) => Err(Error::CellAlreadyActive),
                    std::collections::hash_map::Entry::Vacant(entry) => {
                        entry.insert(ActiveCell {
                            executor: *executor,
                            _reservation: reservation,
                        });
                        Ok(())
                    }
                };
                let _ = reply.send(result);
            }
            WorkerCommand::ActivateRestored {
                cell,
                database,
                destination,
                incarnation,
                schema,
                root,
                reservation,
                reply,
            } => {
                let result = match cells.entry(cell) {
                    std::collections::hash_map::Entry::Occupied(_) => Err(Error::CellAlreadyActive),
                    std::collections::hash_map::Entry::Vacant(entry) => (*database)
                        .open_writable(&destination)
                        .map_err(Error::from)
                        .and_then(|db| {
                            CellExecutor::from_restored(db, cell, incarnation, schema, root)
                        })
                        .map(|executor| {
                            entry.insert(ActiveCell {
                                executor,
                                _reservation: reservation,
                            });
                        }),
                };
                let _ = reply.send(result);
            }
            WorkerCommand::Bootstrap(bootstrap) => {
                let WorkerBootstrap {
                    cell,
                    replica,
                    destination,
                    incarnation,
                    schema,
                    initialize,
                    reservation,
                    reply,
                } = *bootstrap;
                let result = match catch_unwind(AssertUnwindSafe(|| match cells.entry(cell) {
                    std::collections::hash_map::Entry::Occupied(_) => Err(Error::CellAlreadyActive),
                    std::collections::hash_map::Entry::Vacant(entry) => replica
                        .open_new(&destination)
                        .map_err(Error::from)
                        .and_then(|db| {
                            CellExecutor::bootstrap(db, cell, incarnation, schema, initialize)
                        })
                        .map(|(executor, cuts, next_due_ms)| {
                            entry.insert(ActiveCell {
                                executor,
                                _reservation: reservation,
                            });
                            BootstrapExecution { cuts, next_due_ms }
                        }),
                })) {
                    Ok(result) => result,
                    Err(_) => Err(Error::NativePanic),
                };
                let _ = reply.send(result);
            }
            WorkerCommand::Execute {
                cell,
                identity,
                operation_digest,
                now_ms,
                max_result_bytes,
                deadline,
                handler,
                reply,
            } => {
                let result = run_native_callback(&mut cells, cell, deadline, move |active| {
                    match active.executor.execute(
                        identity,
                        operation_digest,
                        now_ms,
                        max_result_bytes,
                        handler,
                    )? {
                        CommandExecution::Recorded(outcome) => {
                            Ok(WorkerExecution::Recorded(outcome))
                        }
                        CommandExecution::Pending => active
                            .executor
                            .pending()
                            .cloned()
                            .map(Box::new)
                            .map(WorkerExecution::Pending)
                            .ok_or(Error::Fenced),
                    }
                });
                let _ = reply.send(result);
            }
            WorkerCommand::DeliverEffect {
                cell,
                delivery,
                now_ms,
                max_result_bytes,
                deadline,
                handler,
                reply,
            } => {
                let result = run_native_callback(&mut cells, cell, deadline, move |active| {
                    match active.executor.deliver_effect(
                        delivery,
                        now_ms,
                        max_result_bytes,
                        handler,
                    )? {
                        CommandExecution::Recorded(outcome) => {
                            Ok(WorkerExecution::Recorded(outcome))
                        }
                        CommandExecution::Pending => active
                            .executor
                            .pending()
                            .cloned()
                            .map(Box::new)
                            .map(WorkerExecution::Pending)
                            .ok_or(Error::Fenced),
                    }
                });
                let _ = reply.send(result);
            }
            WorkerCommand::Query {
                cell,
                max_result_bytes,
                deadline,
                handler,
                reply,
            } => {
                let result = run_native_callback(&mut cells, cell, deadline, move |active| {
                    active.executor.query(max_result_bytes, handler)
                });
                let _ = reply.send(result);
            }
            WorkerCommand::Resolve {
                cell,
                identity,
                operation_digest,
                now_ms,
                max_result_bytes,
                deadline,
                reply,
            } => {
                let result = crab_ltx::with_paged_io_deadline(deadline, || {
                    cells
                        .get_mut(&cell)
                        .ok_or(Error::CellNotActive)
                        .and_then(|cell| {
                            cell.executor.resolve(
                                identity,
                                operation_digest,
                                now_ms,
                                max_result_bytes,
                            )
                        })
                });
                let _ = reply.send(result);
            }
            WorkerCommand::ResolveEffect {
                cell,
                delivery,
                now_ms,
                max_result_bytes,
                deadline,
                reply,
            } => {
                let result = crab_ltx::with_paged_io_deadline(deadline, || {
                    cells
                        .get_mut(&cell)
                        .ok_or(Error::CellNotActive)
                        .and_then(|cell| {
                            cell.executor
                                .resolve_effect(delivery, now_ms, max_result_bytes)
                        })
                });
                let _ = reply.send(result);
            }
            WorkerCommand::BindPrepared {
                cell,
                prepared,
                reply,
            } => {
                let result = cells
                    .get_mut(&cell)
                    .ok_or(Error::CellNotActive)
                    .and_then(|cell| cell.executor.bind_prepared(&prepared));
                let _ = reply.send(result);
            }
            WorkerCommand::Pending { cell, reply } => {
                let result = cells
                    .get(&cell)
                    .ok_or(Error::CellNotActive)
                    .map(|cell| cell.executor.pending().cloned());
                let _ = reply.send(result);
            }
            WorkerCommand::ConfirmPublished { cell, root, reply } => {
                let result = cells
                    .get_mut(&cell)
                    .ok_or(Error::CellNotActive)
                    .and_then(|cell| cell.executor.confirm_published(&root));
                let _ = reply.send(result);
            }
            WorkerCommand::Fence { cell, reply } => {
                let result = cells
                    .get_mut(&cell)
                    .ok_or(Error::CellNotActive)
                    .map(|cell| cell.executor.fence());
                let _ = reply.send(result);
            }
            WorkerCommand::State { cell, reply } => {
                let result = cells
                    .get(&cell)
                    .ok_or(Error::CellNotActive)
                    .map(|cell| cell.executor.worker_state());
                let _ = reply.send(result);
            }
            WorkerCommand::InterruptHandle { cell, reply } => {
                let result = cells
                    .get(&cell)
                    .ok_or(Error::CellNotActive)
                    .map(|cell| cell.executor.interrupt_handle());
                let _ = reply.send(result);
            }
            WorkerCommand::Deactivate { cell, reply } => {
                let result = match cells.get(&cell) {
                    None => Err(Error::CellNotActive),
                    Some(cell) if !cell.executor.drained() => Err(Error::PendingPublication),
                    Some(_) => cells
                        .remove(&cell)
                        .ok_or(Error::CellNotActive)
                        .and_then(|cell| cell.executor.close()),
                };
                let _ = reply.send(result);
            }
            WorkerCommand::Discard { cell, reply } => {
                let result = cells
                    .remove(&cell)
                    .ok_or(Error::CellNotActive)
                    .and_then(|cell| cell.executor.discard());
                let _ = reply.send(result);
            }
        }
    }
}

fn run_native_callback<T>(
    cells: &mut HashMap<CellId, ActiveCell>,
    cell: CellId,
    deadline: Instant,
    callback: impl FnOnce(&mut ActiveCell) -> Result<T>,
) -> Result<T> {
    let result = catch_unwind(AssertUnwindSafe(|| {
        crab_ltx::with_paged_io_deadline(deadline, || {
            cells
                .get_mut(&cell)
                .ok_or(Error::CellNotActive)
                .and_then(callback)
        })
    }));
    match result {
        Ok(result) => result,
        Err(_) => {
            if let Some(active) = cells.get_mut(&cell) {
                active.executor.fence();
            }
            Err(Error::NativePanic)
        }
    }
}

fn worker_index(cell: CellId, workers: usize) -> usize {
    let mut prefix = [0; 8];
    prefix.copy_from_slice(&cell.as_bytes()[..8]);
    u64::from_be_bytes(prefix) as usize % workers
}

async fn receive<T>(response: oneshot::Receiver<Result<T>>) -> Result<T> {
    response.await.map_err(|_| Error::RuntimeClosed)?
}
