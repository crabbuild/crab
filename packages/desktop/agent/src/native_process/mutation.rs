use std::future::Future;
use std::io;
use std::process::ExitStatus;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::process::{Child, Command};
use tokio_util::sync::CancellationToken;

use super::group::supervise;

pub(crate) mod worker;

struct Mutation {
    stop: CancellationToken,
    unconfirmed: Arc<AtomicBool>,
}

tokio::task_local! {
    static MUTATION: Mutation;
}

async fn scope<T>(stop: CancellationToken, operation: impl Future<Output = T>) -> T {
    MUTATION
        .scope(
            Mutation {
                stop,
                unconfirmed: Arc::default(),
            },
            operation,
        )
        .await
}

pub(crate) fn requires_recovery() -> bool {
    MUTATION
        .try_with(|owner| owner.unconfirmed.load(Ordering::Relaxed) || owner.stop.is_cancelled())
        .unwrap_or(false)
}

pub(crate) fn active() -> bool {
    MUTATION.try_with(|_| ()).is_ok()
}

pub(crate) struct CommandScope {
    stop: CancellationToken,
    unconfirmed: Arc<AtomicBool>,
    pid: u32,
    closed: bool,
}

impl CommandScope {
    pub(crate) fn spawn(command: &mut Command) -> io::Result<(Child, Option<Self>)> {
        let Ok((stop, unconfirmed)) =
            MUTATION.try_with(|owner| (owner.stop.clone(), Arc::clone(&owner.unconfirmed)))
        else {
            return Ok((command.spawn()?, None));
        };
        if stop.is_cancelled() || unconfirmed.load(Ordering::Relaxed) {
            return Err(io::Error::other(
                "Task mutation stopped before starting its next command",
            ));
        }
        let child = command.process_group(0).spawn()?;
        let Some(pid) = child.id() else {
            unconfirmed.store(true, Ordering::Relaxed);
            return Err(io::Error::other("Task command PID unavailable"));
        };
        Ok((
            child,
            Some(Self {
                stop,
                unconfirmed,
                pid,
                closed: false,
            }),
        ))
    }

    pub(crate) async fn wait(&self, child: &mut Child) -> io::Result<ExitStatus> {
        supervise(child, self.pid, &self.stop, &CancellationToken::new()).await
    }

    pub(crate) fn confirm_closed(&mut self) {
        self.closed = true;
    }
}

impl Drop for CommandScope {
    fn drop(&mut self) {
        // A failed or abandoned observation must retain the workspace fence.
        // Other commands may run while a prepared ref transaction owns its lock.
        if !self.closed {
            self.unconfirmed.store(true, Ordering::Relaxed);
        }
    }
}
