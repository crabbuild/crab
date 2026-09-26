//! Tasks internals for the Cell node host.

use super::*;

struct AbortOnDrop<T> {
    pub(super) handle: JoinHandle<T>,
}

impl<T> AbortOnDrop<T> {
    pub(super) fn new(handle: JoinHandle<T>) -> Self {
        Self { handle }
    }

    pub(super) async fn join(&mut self) -> std::result::Result<T, tokio::task::JoinError> {
        (&mut self.handle).await
    }
}

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Bounded task supervisor owned by a [`CellNode`] facility.
pub struct CellNodeTaskGroup {
    pub(super) cancellation: CancellationToken,
    pub(super) node_shutdown: CancellationToken,
    tasks: Mutex<Vec<NodeTask>>,
    pub(super) failed: Arc<AtomicBool>,
    pub(super) draining: AtomicBool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TaskPhase {
    Work,
    Lease,
}

struct NodeTask {
    phase: TaskPhase,
    handle: JoinHandle<FacilityResult>,
}

impl Drop for CellNodeTaskGroup {
    fn drop(&mut self) {
        let tasks = match self.tasks.lock() {
            Ok(tasks) => tasks,
            Err(poisoned) => poisoned.into_inner(),
        };
        for task in tasks.iter() {
            task.handle.abort();
        }
    }
}

impl CellNodeTaskGroup {
    pub(super) fn cancel_work(&self) {
        self.draining.store(true, Ordering::Release);
        self.cancellation.cancel();
    }

    /// Creates a task group whose cancellation tokens are controlled by the product host.
    #[must_use]
    pub fn new(cancellation: CancellationToken, node_shutdown: CancellationToken) -> Self {
        Self {
            cancellation,
            node_shutdown,
            tasks: Mutex::new(Vec::new()),
            failed: Arc::new(AtomicBool::new(false)),
            draining: AtomicBool::new(false),
        }
    }

    pub(super) fn is_healthy(&self) -> bool {
        if self.failed.load(Ordering::Acquire) || self.draining.load(Ordering::Acquire) {
            return false;
        }
        self.tasks
            .lock()
            .map(|tasks| tasks.iter().all(|task| !task.handle.is_finished()))
            .unwrap_or(false)
    }

    pub(super) fn ensure_accepting_tasks(&self) -> crab_cell_runtime::Result<()> {
        if self.draining.load(Ordering::Acquire) {
            return Err(Error::CellDraining);
        }
        Ok(())
    }

    /// Spawns one bounded node task and retains its join handle for drain.
    pub fn spawn<F, E>(&self, task: F) -> crab_cell_runtime::Result<()>
    where
        F: Future<Output = std::result::Result<(), E>> + Send + 'static,
        E: std::error::Error + Send + Sync + 'static,
    {
        self.spawn_task(
            async move {
                task.await
                    .map_err(|error| Box::new(error) as Box<dyn std::error::Error + Send + Sync>)
            },
            TaskPhase::Work,
        )
    }

    /// Retains lease renewal until the node has drained its runtime and closed its log.
    ///
    /// This task must stop on the node-shutdown token rather than the work
    /// cancellation token. It shares the ordinary task limit and supervision.
    pub fn spawn_lease_maintenance<F, E>(&self, task: F) -> crab_cell_runtime::Result<()>
    where
        F: Future<Output = std::result::Result<(), E>> + Send + 'static,
        E: std::error::Error + Send + Sync + 'static,
    {
        self.spawn_task(
            async move {
                task.await
                    .map_err(|error| Box::new(error) as Box<dyn std::error::Error + Send + Sync>)
            },
            TaskPhase::Lease,
        )
    }

    /// Spawns one task that already uses the node's boxed facility error type.
    pub fn spawn_boxed<F>(&self, task: F) -> crab_cell_runtime::Result<()>
    where
        F: Future<Output = FacilityResult> + Send + 'static,
    {
        self.spawn_task(task, TaskPhase::Work)
    }

    fn spawn_task<F>(&self, task: F, phase: TaskPhase) -> crab_cell_runtime::Result<()>
    where
        F: Future<Output = FacilityResult> + Send + 'static,
    {
        self.ensure_accepting_tasks()?;
        let mut tasks = self
            .tasks
            .lock()
            .map_err(|_| Error::Control("CellNode task group lock poisoned"))?;
        self.ensure_accepting_tasks()?;
        if tasks.len() >= MAX_NODE_TASKS {
            return Err(Error::Capacity("CellNode task limit reached"));
        }
        let failed = Arc::clone(&self.failed);
        let handle = tokio::spawn(async move {
            let mut task = AbortOnDrop::new(tokio::spawn(task));
            match task.join().await {
                Ok(result) => {
                    if result.is_err() {
                        failed.store(true, Ordering::Release);
                    }
                    result
                }
                Err(error) => {
                    failed.store(true, Ordering::Release);
                    Err(Box::new(error) as Box<dyn std::error::Error + Send + Sync>)
                }
            }
        });
        tasks.push(NodeTask { phase, handle });
        Ok(())
    }

    pub(super) async fn drain_work_until(&self, deadline: Option<Instant>) -> FacilityResult {
        self.cancel_work();
        self.join_until(deadline, false).await
    }

    /// Cancels admission and joins tasks in reverse registration order.
    pub async fn drain(&self) -> FacilityResult {
        self.drain_until(None).await
    }

    /// Cancels admission and joins tasks until an optional absolute deadline.
    pub async fn drain_until(&self, deadline: Option<Instant>) -> FacilityResult {
        self.cancel_work();
        self.node_shutdown.cancel();
        self.join_until(deadline, true).await
    }

    async fn join_until(&self, deadline: Option<Instant>, include_lease: bool) -> FacilityResult {
        let tasks = match self.tasks.lock() {
            Ok(mut tasks) => {
                let (joining, retained): (Vec<_>, Vec<_>) = std::mem::take(&mut *tasks)
                    .into_iter()
                    .partition(|task| include_lease || task.phase == TaskPhase::Work);
                *tasks = retained;
                joining.into_iter().map(|task| task.handle).collect()
            }
            Err(poisoned) => {
                for task in poisoned.into_inner().drain(..) {
                    task.handle.abort();
                }
                return Err(Box::new(std::io::Error::other(
                    "CellNode task group lock poisoned",
                )));
            }
        };
        let mut tasks = TaskBatch {
            tasks,
            abort_on_drop: true,
        };
        let mut first_error = None;
        let mut timed_out = false;
        while let Some(index) = tasks.tasks.len().checked_sub(1) {
            let result = match deadline {
                Some(deadline) => {
                    match tokio::time::timeout_at(deadline.into(), &mut tasks.tasks[index]).await {
                        Ok(result) => result,
                        Err(_) => {
                            timed_out = true;
                            first_error.get_or_insert_with(|| {
                                Box::new(std::io::Error::new(
                                    std::io::ErrorKind::TimedOut,
                                    "CellNode task group drain deadline exceeded",
                                ))
                                    as Box<dyn std::error::Error + Send + Sync>
                            });
                            break;
                        }
                    }
                }
                None => (&mut tasks.tasks[index]).await,
            };
            tasks.tasks.pop();
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) if first_error.is_none() => first_error = Some(error),
                Ok(Err(_)) => {}
                Err(error) if first_error.is_none() => {
                    first_error = Some(Box::new(error) as Box<dyn std::error::Error + Send + Sync>)
                }
                Err(_) => {}
            }
        }
        if !timed_out {
            tasks.abort_on_drop = false;
        }
        first_error.map_or(Ok(()), Err)
    }
}

struct TaskBatch {
    pub(super) tasks: Vec<JoinHandle<FacilityResult>>,
    pub(super) abort_on_drop: bool,
}

impl Drop for TaskBatch {
    fn drop(&mut self) {
        if self.abort_on_drop {
            for task in &self.tasks {
                task.abort();
            }
        }
    }
}
