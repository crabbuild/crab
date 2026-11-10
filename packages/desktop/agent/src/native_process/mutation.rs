use std::future::Future;
use std::io;
use std::process::{Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};

use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use super::group::{GRACE, supervise};

struct Mutation {
    stop: CancellationToken,
    unconfirmed: AtomicBool,
}

tokio::task_local! {
    static MUTATION: Mutation;
}

pub(super) async fn scope<T>(stop: CancellationToken, operation: impl Future<Output = T>) -> T {
    MUTATION
        .scope(
            Mutation {
                stop,
                unconfirmed: AtomicBool::new(false),
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

pub(crate) async fn output(command: &mut Command) -> io::Result<Output> {
    let Ok(stop) = MUTATION.try_with(|owner| owner.stop.clone()) else {
        return command.output().await;
    };
    if stop.is_cancelled() || requires_recovery() {
        return Err(io::Error::other(
            "Task mutation stopped before starting its next command",
        ));
    }
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    let mut child = command.spawn()?;
    // Clear this only after the group and its pipes are both retired. Any
    // incomplete observation keeps the durable workspace fence in place.
    let _ = MUTATION.try_with(|owner| owner.unconfirmed.store(true, Ordering::Relaxed));
    let pid = child
        .id()
        .ok_or_else(|| io::Error::other("Task command PID unavailable"))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("Task stdout unavailable"))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("Task stderr unavailable"))?;
    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    let force = CancellationToken::new();
    let status = {
        let wait = supervise(&mut child, pid, &stop, &force);
        let drain = async {
            tokio::try_join!(
                stdout.read_to_end(&mut stdout_bytes),
                stderr.read_to_end(&mut stderr_bytes)
            )
        };
        tokio::pin!(wait, drain);
        tokio::select! {
            result = &mut drain => {
                let status = wait.await?;
                result?;
                status
            }
            status = &mut wait => {
                let status = status?;
                tokio::time::timeout(GRACE, drain).await??;
                status
            }
        }
    };
    let _ = MUTATION.try_with(|owner| owner.unconfirmed.store(false, Ordering::Relaxed));
    Ok(Output {
        status,
        stdout: stdout_bytes,
        stderr: stderr_bytes,
    })
}
