//! Owned Git subprocesses with bounded diagnostics and joined pipe workers.

use crate::core::error::{CrabError, Result, check_cancelled};
use crab_git::process::OwnedChild;
use std::io::{self, Read as _};
use std::process::{ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

pub(crate) const CANCELLATION_GRACE: Duration = Duration::from_secs(10);
pub(crate) const MAX_CAPTURE_BYTES: u64 = 64 * 1024 * 1024;

// Commands own an explicit repository; inherited helper-relative paths must
// not redirect their reads or writes after changing the working directory.
pub(crate) const GIT_ENV_REMOVALS: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_SHALLOW_FILE",
    "GIT_GRAFT_FILE",
    "GIT_NAMESPACE",
    "GIT_PREFIX",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_CONFIG",
];

pub(crate) struct Output<T> {
    pub(crate) status: ExitStatus,
    pub(crate) stdout: T,
    pub(crate) stderr: Vec<u8>,
}

pub(crate) fn run<T: Send>(
    command: Command,
    cancel: &CancellationToken,
    write_stdin: Option<impl FnOnce(ChildStdin) -> Result<()> + Send>,
    read_stdout: impl FnOnce(ChildStdout) -> Result<T> + Send,
) -> Result<Output<T>> {
    run_with_stderr(command, cancel, write_stdin, read_stdout, |stderr| {
        Ok(capture_output(stderr, MAX_CAPTURE_BYTES)?)
    })
}

pub(crate) fn run_with_stderr<T: Send>(
    mut command: Command,
    cancel: &CancellationToken,
    write_stdin: Option<impl FnOnce(ChildStdin) -> Result<()> + Send>,
    read_stdout: impl FnOnce(ChildStdout) -> Result<T> + Send,
    read_stderr: impl FnOnce(ChildStderr) -> Result<Vec<u8>> + Send,
) -> Result<Output<T>> {
    check_cancelled(cancel)?;
    command
        .stdin(if write_stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = OwnedChild::spawn(command)?;
    // Stop the owned process tree before joining workers on any error. A
    // blocked pipe must not outlive the caller's repository/cache ownership.
    thread::scope(|scope| {
        let result = communicate(
            scope,
            &mut child,
            write_stdin,
            read_stdout,
            read_stderr,
            cancel,
        );
        if result.is_err() {
            child.stop()?;
            if cancel.is_cancelled() {
                return Err(CrabError::Cancelled);
            }
        }
        result
    })
}

enum PipeResult<T> {
    Stdin,
    Stdout(T),
    Stderr(Vec<u8>),
}

fn communicate<'scope, T: Send + 'scope>(
    scope: &'scope thread::Scope<'scope, '_>,
    child: &mut OwnedChild,
    write_stdin: Option<impl FnOnce(ChildStdin) -> Result<()> + Send + 'scope>,
    read_stdout: impl FnOnce(ChildStdout) -> Result<T> + Send + 'scope,
    read_stderr: impl FnOnce(ChildStderr) -> Result<Vec<u8>> + Send + 'scope,
    cancel: &CancellationToken,
) -> Result<Output<T>> {
    let (sender, receiver) = mpsc::channel();
    let stdout = child.take_stdout().ok_or_else(missing_pipe)?;
    let stderr = child.take_stderr().ok_or_else(missing_pipe)?;
    {
        let sender = sender.clone();
        thread::Builder::new().spawn_scoped(scope, move || {
            let _ = sender.send(read_stdout(stdout).map(PipeResult::Stdout));
        })?;
    }
    {
        let sender = sender.clone();
        thread::Builder::new().spawn_scoped(scope, move || {
            let _ = sender.send(read_stderr(stderr).map(PipeResult::Stderr));
        })?;
    }
    let mut pending = 2;
    if let Some(write_stdin) = write_stdin {
        let stdin = child.take_stdin().ok_or_else(missing_pipe)?;
        let sender = sender.clone();
        thread::Builder::new().spawn_scoped(scope, move || {
            // The callback owns stdin, so EOF precedes completion reporting.
            let _ = sender.send(write_stdin(stdin).map(|()| PipeResult::Stdin));
        })?;
        pending += 1;
    }
    let mut status = None;
    let mut cancelling = None;
    let mut stdout = None;
    let mut stderr = Vec::new();
    loop {
        if cancel.is_cancelled() && cancelling.is_none() {
            child.request_shutdown()?;
            cancelling = Some(Instant::now());
        }
        if let Some(started) = cancelling {
            if (pending == 0 && child.has_exited()?) || started.elapsed() >= CANCELLATION_GRACE {
                child.stop()?;
                return Err(CrabError::Cancelled);
            }
        } else if status.is_none() && child.has_exited()? {
            // Retain the Unix leader until the group signal, preventing PID
            // reuse from redirecting cleanup to an unrelated process group.
            status = Some(child.stop()?);
        }
        if pending == 0
            && let Some(status) = status
        {
            return Ok(Output {
                status,
                stdout: stdout.ok_or_else(missing_pipe)?,
                stderr,
            });
        }
        // Keep the original sender alive so closed pipes do not busy-loop
        // while the command still runs without producing further output.
        match receiver.recv_timeout(Duration::from_millis(10)) {
            Ok(result) => {
                match result {
                    Ok(PipeResult::Stdin) => {}
                    Ok(PipeResult::Stdout(value)) => stdout = Some(value),
                    Ok(PipeResult::Stderr(bytes)) => stderr = bytes,
                    Err(_) if cancelling.is_some() => {}
                    Err(error) => return Err(error),
                }
                pending -= 1;
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Err(missing_pipe().into()),
        }
    }
}

pub(crate) fn capture_output(mut pipe: impl io::Read, limit: u64) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    (&mut pipe).take(limit).read_to_end(&mut bytes)?;
    let mut excess = [0];
    if pipe.read(&mut excess)? != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Git subprocess output exceeds capture limit",
        ));
    }
    Ok(bytes)
}

fn missing_pipe() -> io::Error {
    io::Error::other("Git child pipe is unavailable")
}
