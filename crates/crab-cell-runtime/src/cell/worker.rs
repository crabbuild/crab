//! Bounded SQL worker pool and its per-shard worker loop.
use std::{
    collections::HashMap,
    panic::{AssertUnwindSafe, catch_unwind},
    path::PathBuf,
    sync::{Arc, Mutex},
    thread::JoinHandle,
    time::{Duration, Instant},
};

use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};

use crate::cell::catalog::CatalogRole;
use crate::cell::executor::{
    CellExecutor, CommandExecution, HandlerOutcome, MigrationOutcome, PendingCommit,
    PendingMigration, StoredOutcome,
};
use crate::cell::executor::{MutationIdentity, Resolution};
use crate::fleet::resource::ResourceCost;
use crate::fleet::resource::{
    ACTIVE_CELL_NATIVE_BYTES, HYDRATION_JOB_CAPACITY, ResourceLedger, ResourceReservation,
};
use crate::identity::{CellId, Digest};
use crate::primitives::effects::InboxDelivery;
use crate::primitives::maintenance::PersistedWorkInventory;
use crate::primitives::maintenance::TransferWorkInventory;
use crate::registry::MigrationPlan;
use crate::{Error, Result};

mod run;

const MAX_WORKERS: usize = 16;
const MAX_ACTIVE_CELLS: usize = 10_000;
const WORKER_QUEUE: usize = 256;
const DEFAULT_PAGE_IO_DEADLINE: Duration = Duration::from_secs(30);

/// Minimum SQLite page-cache reservation for one active Cell.
pub const ACTIVE_CELL_PAGE_CACHE_BYTES: u64 =
    crab_ltx::MANAGED_SQLITE_CONNECTIONS * crab_ltx::MANAGED_CONNECTION_PAGE_CACHE_BYTES;

pub use crate::fleet::resource::ACTIVE_CELL_FILE_DESCRIPTORS;

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

#[derive(Debug)]
pub(crate) enum HydrationStep {
    Progress(Option<crab_ltx::Hydration>),
    Deferred(Duration),
}

/// Result returned by one SQL worker without releasing pending command output.
#[derive(Clone)]
pub enum WorkerExecution {
    /// The command has a durable outcome.
    Recorded(StoredOutcome),
    /// The command committed locally and awaits publication.
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
        let resources = ResourceLedger::new(
            ResourceCost::zero()
                .with_active_cells(max_active_cells)
                .with_resident_bytes(max_active_cells.saturating_mul(ACTIVE_CELL_NATIVE_BYTES))
                .with_file_descriptors(
                    max_active_cells.saturating_mul(ACTIVE_CELL_FILE_DESCRIPTORS),
                )
                .with_worker_jobs(worker_count)
                .with_primitive_jobs(worker_count)
                .with_hydration_jobs(HYDRATION_JOB_CAPACITY),
        );
        let mut workers = Vec::with_capacity(worker_count);
        let mut threads: Vec<JoinHandle<()>> = Vec::with_capacity(worker_count);
        for index in 0..worker_count {
            let (sender, receiver) = mpsc::channel(WORKER_QUEUE);
            let thread = match std::thread::Builder::new()
                .name(format!("crab-cell-sql-{index}"))
                .spawn(move || run::run_worker(receiver))
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
                resources,
                max_active_cells,
                worker_count,
                worker_permits: (0..worker_count)
                    .map(|_| Arc::new(Semaphore::new(1)))
                    .collect(),
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
        incarnation: crate::identity::IncarnationId,
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

    pub(crate) async fn confirm_bootstrap_published(
        &self,
        cell: CellId,
        cuts: crab_ltx::CaptureBatch,
    ) -> Result<()> {
        let (reply, response) = oneshot::channel();
        self.send(
            cell,
            WorkerCommand::ConfirmBootstrapPublished {
                cell,
                cuts: Box::new(cuts),
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
        database: RestoredDatabase,
        destination: PathBuf,
        incarnation: crate::identity::IncarnationId,
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
        self.send_worker_job(
            cell,
            WorkerCommand::Execute {
                trace: tracing::Span::current(),
                queued_at: Instant::now(),
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

    /// Runs one registry-verified schema step on the Cell's assigned worker.
    pub(crate) async fn migrate(
        &self,
        cell: CellId,
        plan: MigrationPlan,
        now_ms: i64,
        deadline: Instant,
    ) -> Result<PendingMigration> {
        let (reply, response) = oneshot::channel();
        self.send_worker_job(
            cell,
            WorkerCommand::Migrate {
                cell,
                plan,
                now_ms,
                deadline,
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
        self.send_worker_job(
            cell,
            WorkerCommand::DeliverEffect {
                trace: tracing::Span::current(),
                queued_at: Instant::now(),
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
        self.send_worker_job(
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

    /// Fetches a bounded sparse batch asynchronously, then installs it on its worker.
    pub(crate) async fn hydrate(
        &self,
        cell: CellId,
        pages: u32,
        deadline: Instant,
    ) -> Result<HydrationStep> {
        let (reply, response) = oneshot::channel();
        let preparation = async {
            self.send_worker_job(
                cell,
                WorkerCommand::PrepareHydration {
                    cell,
                    pages,
                    deadline,
                    reply,
                },
            )
            .await?;
            receive(response).await
        };
        // Preparation only selects pages. Abandoning its waiter cannot install
        // bytes or release foreground ownership, even if it was dispatched.
        let read = match tokio::time::timeout_at(deadline.into(), preparation).await {
            Ok(result) => result?,
            Err(_) => return Ok(HydrationStep::Deferred(Duration::ZERO)),
        };
        let Some(read) = read else {
            return Ok(HydrationStep::Progress(None));
        };
        let retained = match self
            .inner
            .resources
            .try_reserve(ResourceCost::zero().with_retained_bytes(read.retained_bytes()))
        {
            Ok(retained) => retained,
            // Background work yields to retained foreground bytes. No page was
            // fetched or installed, so retry later without fencing the owner.
            Err(Error::Capacity(_)) => return Ok(HydrationStep::Deferred(Duration::ZERO)),
            Err(error) => return Err(error),
        };
        let batch = match tokio::time::timeout_at(deadline.into(), read.fetch()).await {
            Ok(Ok(batch)) => batch,
            Ok(Err(error)) => match error.classify() {
                crab_ltx::FailureClass::Retryable { after } => {
                    return Ok(HydrationStep::Deferred(after.unwrap_or_default()));
                }
                crab_ltx::FailureClass::Capacity => {
                    return Ok(HydrationStep::Deferred(Duration::ZERO));
                }
                _ => return Err(error.into()),
            },
            Err(_) => return Ok(HydrationStep::Deferred(Duration::ZERO)),
        };
        let (reply, response) = oneshot::channel();
        // Move the payload's reservation into the dispatched install: dropping
        // its waiter cannot release bytes still owned by the worker queue.
        let installation = async {
            self.send_worker_job(
                cell,
                WorkerCommand::InstallHydration {
                    cell,
                    batch,
                    retained,
                    deadline,
                    reply,
                },
            )
            .await?;
            receive(response).await
        };
        let progress = tokio::time::timeout_at(deadline.into(), installation)
            .await
            .map_err(|_| Error::Deadline)??;
        Ok(HydrationStep::Progress(Some(progress)))
    }

    pub(crate) async fn hydration(&self, cell: CellId) -> Result<Option<crab_ltx::Hydration>> {
        let (reply, response) = oneshot::channel();
        self.send(cell, WorkerCommand::Hydration { cell, reply })
            .await?;
        receive(response).await
    }

    pub(crate) async fn persisted_work_inventory(
        &self,
        cell: CellId,
        role: CatalogRole,
    ) -> Result<PersistedWorkInventory> {
        let (reply, response) = oneshot::channel();
        self.send_worker_job(cell, WorkerCommand::PersistedWork { cell, role, reply })
            .await?;
        receive(response).await
    }

    pub(crate) async fn transfer_work_inventory(
        &self,
        cell: CellId,
        role: CatalogRole,
        now_ms: i64,
    ) -> Result<TransferWorkInventory> {
        let (reply, response) = oneshot::channel();
        self.send_worker_job(
            cell,
            WorkerCommand::TransferWork {
                cell,
                role,
                now_ms,
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
        self.send_worker_job(
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
        self.send_worker_job(
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

    pub(crate) async fn bind_migration_prepared(
        &self,
        cell: CellId,
        prepared: crab_ltx::PreparedRoot,
    ) -> Result<()> {
        let (reply, response) = oneshot::channel();
        self.send(
            cell,
            WorkerCommand::BindMigrationPrepared {
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

    pub(crate) async fn confirm_durable(&self, cell: CellId, commit_sequence: u64) -> Result<()> {
        let (reply, response) = oneshot::channel();
        self.send(
            cell,
            WorkerCommand::ConfirmDurable {
                cell,
                commit_sequence,
                reply,
            },
        )
        .await?;
        receive(response).await
    }

    pub(crate) async fn confirm_migration_published(
        &self,
        cell: CellId,
        root: crab_ltx::RootRef,
    ) -> Result<MigrationOutcome> {
        let (reply, response) = oneshot::channel();
        self.send(
            cell,
            WorkerCommand::ConfirmMigrationPublished { cell, root, reply },
        )
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

    /// Closes a drained Cell and leaves a resume record for the root it holds.
    pub(crate) async fn deactivate_resumable(
        &self,
        cell: CellId,
        root: crate::control::RootRef,
        code: Digest,
    ) -> Result<()> {
        let (reply, response) = oneshot::channel();
        self.send(
            cell,
            WorkerCommand::DeactivateResumable {
                cell,
                root,
                code,
                reply,
            },
        )
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
            if self
                .inner
                .resources
                .snapshot()
                .map(|snapshot| snapshot.used.active_cells() != 0)
                .unwrap_or(true)
            {
                return Err(Error::Control(
                    "SQL worker shutdown requires every Cell to be deactivated",
                ));
            }
            lifecycle.closing = true;
            for permits in &self.inner.worker_permits {
                permits.close();
            }
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

    async fn send_worker_job(&self, cell: CellId, command: WorkerCommand) -> Result<()> {
        let worker_permits = {
            let lifecycle = self
                .inner
                .lifecycle
                .lock()
                .map_err(|_| Error::RuntimeClosed)?;
            if lifecycle.closing {
                return Err(Error::RuntimeClosed);
            }
            // A queued job must wait for its own shard, without consuming
            // the admission capacity an idle worker needs to make progress.
            Arc::clone(&self.inner.worker_permits[worker_index(cell, self.inner.worker_count)])
        };
        let permit = worker_permits
            .acquire_owned()
            .await
            .map_err(|_| Error::RuntimeClosed)?;
        let reservation = self
            .inner
            .resources
            .try_reserve(ResourceCost::zero().with_worker_jobs(1))?;
        self.send(
            cell,
            WorkerCommand::Reserved {
                command: Box::new(command),
                reservation: WorkerJobReservation {
                    _reservation: reservation,
                    _permit: permit,
                },
            },
        )
        .await
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
        let reservation = self
            .inner
            .resources
            .try_reserve(ResourceCost::active_cell())
            .map_err(|error| match error {
                Error::Capacity(_) => Error::Capacity("active Cells per node"),
                error => error,
            })?;
        Ok(CellReservation {
            _reservation: reservation,
        })
    }

    pub(crate) fn configure_retained_capacity(&self, bytes: usize) -> Result<()> {
        self.inner.resources.set_retained_limit(bytes)
    }

    pub(crate) fn resource_ledger(&self) -> ResourceLedger {
        self.inner.resources.clone()
    }

    pub(crate) fn active_cells(&self) -> usize {
        self.inner
            .resources
            .snapshot()
            .map(|snapshot| snapshot.used.active_cells())
            .unwrap_or(self.inner.max_active_cells)
    }

    pub(crate) fn active_cell_capacity(&self) -> usize {
        self.inner.max_active_cells
    }
}

struct PoolInner {
    lifecycle: Mutex<WorkerLifecycle>,
    resources: ResourceLedger,
    max_active_cells: usize,
    worker_count: usize,
    worker_permits: Vec<Arc<Semaphore>>,
}

struct WorkerLifecycle {
    workers: Vec<mpsc::Sender<WorkerCommand>>,
    threads: Vec<JoinHandle<()>>,
    closing: bool,
}

impl Drop for PoolInner {
    fn drop(&mut self) {
        for permits in &self.worker_permits {
            permits.close();
        }
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

/// How one activation reaches its exact root before the worker serves it.
pub(crate) enum RestoredDatabase {
    /// The exact root is restored into a fresh destination and read sparsely.
    Paged(Box<crab_ltx::CellWritableDatabase>),
    /// A local database already holds the root, so the origin is never read.
    Local(Box<crab_ltx::Db>),
}

enum WorkerCommand {
    Reserved {
        command: Box<WorkerCommand>,
        reservation: WorkerJobReservation,
    },
    Activate {
        cell: CellId,
        executor: Box<CellExecutor>,
        reservation: CellReservation,
        reply: oneshot::Sender<Result<()>>,
    },
    ActivateRestored {
        cell: CellId,
        database: Box<RestoredDatabase>,
        destination: PathBuf,
        incarnation: crate::identity::IncarnationId,
        schema: u32,
        root: crab_ltx::RootRef,
        reservation: CellReservation,
        reply: oneshot::Sender<Result<()>>,
    },
    Bootstrap(Box<WorkerBootstrap>),
    Execute {
        trace: tracing::Span,
        queued_at: Instant,
        cell: CellId,
        identity: MutationIdentity,
        operation_digest: Digest,
        now_ms: i64,
        max_result_bytes: usize,
        deadline: Instant,
        handler: Handler,
        reply: oneshot::Sender<Result<WorkerExecution>>,
    },
    Migrate {
        cell: CellId,
        plan: MigrationPlan,
        now_ms: i64,
        deadline: Instant,
        reply: oneshot::Sender<Result<PendingMigration>>,
    },
    DeliverEffect {
        trace: tracing::Span,
        queued_at: Instant,
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
    PrepareHydration {
        cell: CellId,
        pages: u32,
        deadline: Instant,
        reply: oneshot::Sender<Result<Option<crab_ltx::db::HydrationRead>>>,
    },
    InstallHydration {
        cell: CellId,
        batch: crab_ltx::db::HydrationBatch,
        retained: ResourceReservation,
        deadline: Instant,
        reply: oneshot::Sender<Result<crab_ltx::Hydration>>,
    },
    Hydration {
        cell: CellId,
        reply: oneshot::Sender<Result<Option<crab_ltx::Hydration>>>,
    },
    PersistedWork {
        cell: CellId,
        role: CatalogRole,
        reply: oneshot::Sender<Result<PersistedWorkInventory>>,
    },
    TransferWork {
        cell: CellId,
        role: CatalogRole,
        now_ms: i64,
        reply: oneshot::Sender<Result<TransferWorkInventory>>,
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
    BindMigrationPrepared {
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
    ConfirmDurable {
        cell: CellId,
        commit_sequence: u64,
        reply: oneshot::Sender<Result<()>>,
    },
    ConfirmBootstrapPublished {
        cell: CellId,
        cuts: Box<crab_ltx::CaptureBatch>,
        reply: oneshot::Sender<Result<()>>,
    },
    ConfirmMigrationPublished {
        cell: CellId,
        root: crab_ltx::RootRef,
        reply: oneshot::Sender<Result<MigrationOutcome>>,
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
    DeactivateResumable {
        cell: CellId,
        root: crate::control::RootRef,
        code: Digest,
        reply: oneshot::Sender<Result<()>>,
    },
    Discard {
        cell: CellId,
        reply: oneshot::Sender<Result<()>>,
    },
}

struct WorkerJobReservation {
    _reservation: ResourceReservation,
    _permit: OwnedSemaphorePermit,
}

struct WorkerBootstrap {
    cell: CellId,
    replica: crab_ltx::CellReplica,
    destination: PathBuf,
    incarnation: crate::identity::IncarnationId,
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
    _reservation: ResourceReservation,
}

fn worker_index(cell: CellId, workers: usize) -> usize {
    let mut prefix = [0; 8];
    prefix.copy_from_slice(&cell.as_bytes()[..8]);
    u64::from_be_bytes(prefix) as usize % workers
}

async fn receive<T>(response: oneshot::Receiver<Result<T>>) -> Result<T> {
    response.await.map_err(|_| Error::RuntimeClosed)?
}

#[cfg(test)]
mod tests;
