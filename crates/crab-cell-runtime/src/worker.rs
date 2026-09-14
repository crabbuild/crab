use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread::JoinHandle,
};

use tokio::sync::{mpsc, oneshot};

use crate::{
    CellExecutor, CellId, CommandExecution, Digest, Error, HandlerOutcome, MutationIdentity,
    PendingCommit, Result, StoredOutcome,
};

const MAX_WORKERS: usize = 16;
const MAX_ACTIVE_CELLS: usize = 10_000;
const WORKER_QUEUE: usize = 256;

pub(crate) type Handler = Box<
    dyn for<'connection> FnOnce(
            &crab_ltx::rusqlite::Transaction<'connection>,
        ) -> Result<HandlerOutcome>
        + Send
        + 'static,
>;

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
                workers,
                threads: Mutex::new(threads),
                active,
                max_active_cells,
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
        let (reply, response) = oneshot::channel();
        self.send(
            cell,
            WorkerCommand::Execute {
                cell,
                identity,
                operation_digest,
                now_ms,
                max_result_bytes,
                handler: Box::new(handler),
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

    /// Closes and removes one fully drained Cell from its worker.
    pub async fn deactivate(&self, cell: CellId) -> Result<()> {
        let (reply, response) = oneshot::channel();
        self.send(cell, WorkerCommand::Deactivate { cell, reply })
            .await?;
        receive(response).await
    }

    async fn send(&self, cell: CellId, command: WorkerCommand) -> Result<()> {
        self.inner.workers[worker_index(cell, self.inner.workers.len())]
            .send(command)
            .await
            .map_err(|_| Error::RuntimeClosed)
    }

    pub(crate) fn reserve_activation(&self) -> Result<CellReservation> {
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
    workers: Vec<mpsc::Sender<WorkerCommand>>,
    threads: Mutex<Vec<JoinHandle<()>>>,
    active: Arc<AtomicUsize>,
    max_active_cells: usize,
}

impl Drop for PoolInner {
    fn drop(&mut self) {
        self.workers.clear();
        let threads = match self.threads.get_mut() {
            Ok(threads) => std::mem::take(threads),
            Err(poisoned) => std::mem::take(poisoned.into_inner()),
        };
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
    Execute {
        cell: CellId,
        identity: MutationIdentity,
        operation_digest: Digest,
        now_ms: i64,
        max_result_bytes: usize,
        handler: Handler,
        reply: oneshot::Sender<Result<WorkerExecution>>,
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
    Deactivate {
        cell: CellId,
        reply: oneshot::Sender<Result<()>>,
    },
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
            WorkerCommand::Execute {
                cell,
                identity,
                operation_digest,
                now_ms,
                max_result_bytes,
                handler,
                reply,
            } => {
                let result = cells
                    .get_mut(&cell)
                    .ok_or(Error::CellNotActive)
                    .and_then(|cell| {
                        match cell.executor.execute(
                            identity,
                            operation_digest,
                            now_ms,
                            max_result_bytes,
                            handler,
                        )? {
                            CommandExecution::Recorded(outcome) => {
                                Ok(WorkerExecution::Recorded(outcome))
                            }
                            CommandExecution::Pending => cell
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
