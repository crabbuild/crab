//! Replica-only executor and worker contracts.

use std::io;

/// Blocking dispatch boundary; success means the job was accepted for execution.
///
/// The dispatcher must eventually run or drop the job. Dropped jobs and panics
/// become errors to the awaiting operation; cancellation does not undo side effects.
#[cfg(feature = "replica")]
pub trait Executor: Send + Sync {
    fn dispatch(&self, job: Box<dyn FnOnce() + Send>) -> io::Result<()>;
    /// Starts a long-lived worker independently of the caller and dispatch pool.
    ///
    /// Must not queue behind the blocking caller: SQLite waits synchronously
    /// for this worker. The worker drives its own Tokio I/O runtime until closed.
    fn start_worker(&self, job: Box<dyn FnOnce() + Send>) -> io::Result<Box<dyn Worker>>;
}

/// An independently progressing worker joined after its input queue closes.
#[cfg(feature = "replica")]
pub trait Worker: Send + Sync {
    fn join(self: Box<Self>) -> io::Result<()>;
}

#[cfg(feature = "replica")]
pub(crate) struct TokioExecutor;
#[cfg(feature = "replica")]
impl Executor for TokioExecutor {
    fn dispatch(&self, job: Box<dyn FnOnce() + Send>) -> io::Result<()> {
        tokio::runtime::Handle::try_current()
            .map_err(io::Error::other)?
            .spawn_blocking(job);
        Ok(())
    }
    fn start_worker(&self, job: Box<dyn FnOnce() + Send>) -> io::Result<Box<dyn Worker>> {
        Ok(Box::new(
            std::thread::Builder::new()
                .name("crab-ltx-paged".into())
                .spawn(job)?,
        ))
    }
}

#[cfg(feature = "replica")]
impl Worker for std::thread::JoinHandle<()> {
    fn join(self: Box<Self>) -> io::Result<()> {
        (*self)
            .join()
            .map_err(|_| io::Error::other("host worker panicked"))
    }
}
