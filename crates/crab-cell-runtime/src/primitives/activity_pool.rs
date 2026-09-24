//! Bounded blocking pool for Activity work.
use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, Mutex, mpsc},
    thread::JoinHandle,
};

use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot};

use crate::primitives::workflow::ActivityExecution;
use crate::{Error, Result};

const MAX_ACTIVITY_WORKERS: usize = 16;

type ActivityJob = Box<dyn FnOnce() -> Result<ActivityExecution> + Send + 'static>;

/// Fixed native blocking-activity pool with claim-before-execution admission.
#[derive(Clone)]
pub struct BlockingActivityPool {
    inner: Arc<PoolInner>,
}

impl BlockingActivityPool {
    /// Starts one bounded OS-thread pool for trusted blocking activity handlers.
    pub fn new(worker_count: usize) -> Result<Self> {
        if worker_count == 0 || worker_count > MAX_ACTIVITY_WORKERS {
            return Err(Error::Capacity("invalid blocking activity worker count"));
        }
        let (sender, receiver) = mpsc::sync_channel(worker_count);
        let receiver = Arc::new(Mutex::new(receiver));
        let mut threads: Vec<JoinHandle<()>> = Vec::with_capacity(worker_count);
        for index in 0..worker_count {
            let receiver = Arc::clone(&receiver);
            let thread = match std::thread::Builder::new()
                .name(format!("crab-cell-activity-{index}"))
                .spawn(move || run_worker(&receiver))
            {
                Ok(thread) => thread,
                Err(error) => {
                    drop(sender);
                    for thread in threads {
                        let _ = thread.join();
                    }
                    return Err(Error::ActivityWorkerStart(Box::new(error)));
                }
            };
            threads.push(thread);
        }
        Ok(Self {
            inner: Arc::new(PoolInner {
                lifecycle: Mutex::new(ActivityLifecycle {
                    sender: Some(sender),
                    threads,
                    closing: false,
                }),
                admission: Arc::new(Semaphore::new(worker_count)),
            }),
        })
    }

    /// Uses the node's CPU count, capped at sixteen blocking callbacks.
    pub fn for_system() -> Result<Self> {
        Self::new(
            std::thread::available_parallelism()
                .map(usize::from)
                .unwrap_or(1)
                .clamp(1, MAX_ACTIVITY_WORKERS),
        )
    }

    /// Reserves one actual blocking slot without waiting or claiming durable work.
    pub fn try_reserve(&self) -> Result<Option<BlockingActivityReservation>> {
        let lifecycle = self
            .inner
            .lifecycle
            .lock()
            .map_err(|_| Error::RuntimeClosed)?;
        if lifecycle.closing {
            return Err(Error::RuntimeClosed);
        }
        let permit = match Arc::clone(&self.inner.admission).try_acquire_owned() {
            Ok(permit) => permit,
            Err(tokio::sync::TryAcquireError::NoPermits) => return Ok(None),
            Err(tokio::sync::TryAcquireError::Closed) => return Err(Error::RuntimeClosed),
        };
        Ok(Some(BlockingActivityReservation {
            pool: Arc::clone(&self.inner),
            permit: Some(permit),
        }))
    }

    /// Stops admission, drains submitted callbacks and joins every worker.
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
            lifecycle.closing = true;
            self.inner.admission.close();
            lifecycle.sender.take();
            std::mem::take(&mut lifecycle.threads)
        };
        tokio::task::spawn_blocking(move || join_workers(threads))
            .await
            .map_err(Error::ActivityWorkerJoin)?
    }
}

/// A pre-claim blocking slot retained until its callback actually terminates.
pub struct BlockingActivityReservation {
    pool: Arc<PoolInner>,
    permit: Option<OwnedSemaphorePermit>,
}

impl BlockingActivityReservation {
    pub(crate) async fn execute(mut self, handler: ActivityJob) -> Result<ActivityExecution> {
        let (reply, response) = oneshot::channel();
        let job = BlockingJob {
            handler,
            reply,
            permit: self.permit.take().ok_or(Error::RuntimeClosed)?,
        };
        let sender = {
            let lifecycle = self
                .pool
                .lifecycle
                .lock()
                .map_err(|_| Error::RuntimeClosed)?;
            if lifecycle.closing {
                return Err(Error::RuntimeClosed);
            }
            lifecycle
                .sender
                .as_ref()
                .cloned()
                .ok_or(Error::RuntimeClosed)?
        };
        sender.try_send(job).map_err(|error| match error {
            mpsc::TrySendError::Full(_) => {
                Error::Capacity("blocking activity worker queue is full")
            }
            mpsc::TrySendError::Disconnected(_) => Error::RuntimeClosed,
        })?;
        response.await.map_err(|_| Error::RuntimeClosed)?
    }
}

struct PoolInner {
    lifecycle: Mutex<ActivityLifecycle>,
    admission: Arc<Semaphore>,
}

struct ActivityLifecycle {
    sender: Option<mpsc::SyncSender<BlockingJob>>,
    threads: Vec<JoinHandle<()>>,
    closing: bool,
}

impl Drop for PoolInner {
    fn drop(&mut self) {
        let lifecycle = match self.lifecycle.get_mut() {
            Ok(lifecycle) => lifecycle,
            Err(poisoned) => poisoned.into_inner(),
        };
        lifecycle.sender.take();
        let _ = join_workers(std::mem::take(&mut lifecycle.threads));
    }
}

struct BlockingJob {
    handler: ActivityJob,
    reply: oneshot::Sender<Result<ActivityExecution>>,
    permit: OwnedSemaphorePermit,
}

fn run_worker(receiver: &Mutex<mpsc::Receiver<BlockingJob>>) {
    loop {
        let job = receiver
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .recv();
        let Ok(job) = job else {
            return;
        };
        let result = catch_unwind(AssertUnwindSafe(job.handler))
            .map_err(|_| Error::ActivityPanic)
            .and_then(|result| result);
        // Completion is observable only after admission reopens, allowing the
        // caller to reserve the next worker immediately after execute returns.
        drop(job.permit);
        let _ = job.reply.send(result);
    }
}

fn join_workers(threads: Vec<JoinHandle<()>>) -> Result<()> {
    let mut panicked = false;
    for thread in threads {
        if thread.join().is_err() {
            panicked = true;
        }
    }
    if panicked {
        Err(Error::ActivityWorkerPanic)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests;
