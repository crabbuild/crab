//! Owned Git subprocesses with bounded diagnostics and joined pipe workers.

use crate::core::error::{CrabError, Result, check_cancelled};
use process_wrap::std::{ChildWrapper, CommandWrap};
use std::io::{self, Read as _};
use std::process::{ChildStdin, ChildStdout, Command, ExitStatus, Stdio};
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
    mut command: Command,
    cancel: &CancellationToken,
    write_stdin: Option<impl FnOnce(ChildStdin) -> Result<()> + Send>,
    read_stdout: impl FnOnce(ChildStdout) -> Result<T> + Send,
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
    let mut command = CommandWrap::from(command);
    #[cfg(unix)]
    command.wrap(process_wrap::std::ProcessGroup::leader());
    #[cfg(windows)]
    command.wrap(process_wrap::std::JobObject);
    let mut child = OwnedChild {
        inner: command.spawn()?,
        finished: false,
    };
    // Stop the owned process tree before joining workers on any error. A
    // blocked pipe must not outlive the caller's repository/cache ownership.
    thread::scope(|scope| {
        let result = communicate(scope, &mut child, write_stdin, read_stdout, cancel);
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
    cancel: &CancellationToken,
) -> Result<Output<T>> {
    let (sender, receiver) = mpsc::channel();
    let stdout = child.inner.stdout().take().ok_or_else(missing_pipe)?;
    let stderr = child.inner.stderr().take().ok_or_else(missing_pipe)?;
    {
        let sender = sender.clone();
        thread::Builder::new().spawn_scoped(scope, move || {
            let _ = sender.send(read_stdout(stdout).map(PipeResult::Stdout));
        })?;
    }
    {
        let sender = sender.clone();
        thread::Builder::new().spawn_scoped(scope, move || {
            let result = capture_output(stderr, MAX_CAPTURE_BYTES)
                .map(PipeResult::Stderr)
                .map_err(CrabError::from);
            let _ = sender.send(result);
        })?;
    }
    let mut pending = 2;
    if let Some(write_stdin) = write_stdin {
        let stdin = child.inner.stdin().take().ok_or_else(missing_pipe)?;
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

struct OwnedChild {
    inner: Box<dyn ChildWrapper>,
    finished: bool,
}

impl OwnedChild {
    #[cfg(unix)]
    fn has_exited(&mut self) -> io::Result<bool> {
        if self.finished {
            return Ok(true);
        }
        // SAFETY: siginfo_t may be zero-initialized; waitid receives a writable
        // pointer valid for this call. WNOWAIT retains our unreaped child and
        // therefore its process-group identity until stop signals and waits.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: the initialized buffer and retained child identity above
        // satisfy waitid's pointer and process-selection contract.
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                self.inner.id(),
                &raw mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result == -1 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                return Ok(false);
            }
            return Err(error);
        }
        // SAFETY: waitid initialized the SIGCHLD fields, or left our zero PID
        // unchanged when the child has no reportable exit yet.
        Ok(unsafe { info.si_pid() } != 0)
    }

    #[cfg(windows)]
    fn has_exited(&mut self) -> io::Result<bool> {
        if self.finished {
            return Ok(true);
        }
        // Poll only the leader: JobObject::try_wait consumes completion-port
        // events needed by its final full-job wait. Windows retains the native
        // process handle, and termination targets the job rather than a PID.
        self.inner
            .try_inner_child_mut()
            .ok_or_else(missing_pipe)?
            .try_wait()
            .map(|status| status.is_some())
    }

    fn request_shutdown(&mut self) -> io::Result<()> {
        if self.finished {
            return Ok(());
        }
        #[cfg(unix)]
        {
            let result = self.inner.signal(libc::SIGTERM);
            self.signal_result(result)
        }
        #[cfg(windows)]
        {
            self.inner.start_kill()
        }
    }

    fn stop(&mut self) -> io::Result<ExitStatus> {
        if self.finished {
            return self.inner.wait();
        }
        let killed = self.inner.start_kill();
        #[cfg(unix)]
        let killed = self.signal_result(killed);
        // Reap even when signalling raced with exit. Never release ownership
        // merely because the signal was sent; the pipes are joined afterward.
        let status = self.inner.wait()?;
        self.finished = true;
        killed?;
        Ok(status)
    }

    #[cfg(unix)]
    fn signal_result(&mut self, result: io::Result<()>) -> io::Result<()> {
        match result {
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => Ok(()),
            // XNU can exclude exiting members before waitid reports their exit.
            // Keep the leader unreaped, but prove the whole pinned group has no
            // live members before treating killpg's EPERM as an exit race.
            #[cfg(target_os = "macos")]
            Err(error)
                if error.raw_os_error() == Some(libc::EPERM)
                    && macos_group_finished(self.inner.id())? =>
            {
                Ok(())
            }
            other => other,
        }
    }
}

#[cfg(target_os = "macos")]
fn macos_group_finished(group: u32) -> io::Result<bool> {
    // PROC_PGRP_ONLY in Darwin's sys/proc_info.h; libc exposes the function
    // but not this selector. A full buffer is incomplete evidence: grow it.
    const PROC_PGRP_ONLY: u32 = 2;
    let mut pids = vec![0i32; 32];
    let pids = loop {
        let bytes =
            i32::try_from(std::mem::size_of_val(pids.as_slice())).map_err(io::Error::other)?;
        // SAFETY: the PID array is writable for the exact supplied byte size.
        let filled =
            unsafe { libc::proc_listpids(PROC_PGRP_ONLY, group, pids.as_mut_ptr().cast(), bytes) };
        if filled <= 0 {
            return Err(io::Error::last_os_error());
        }
        if filled < bytes {
            pids.truncate(filled as usize / std::mem::size_of::<i32>());
            break pids;
        }
        pids.resize(pids.len() * 2, 0);
    };
    for pid in pids {
        // SAFETY: proc_bsdinfo consists of integer fields and fixed arrays;
        // zero initialization is valid and the API receives its exact size.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let bytes = i32::try_from(std::mem::size_of_val(&info)).map_err(io::Error::other)?;
        // SAFETY: info is writable for bytes and the selector matches its type.
        let filled = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                (&raw mut info).cast(),
                bytes,
            )
        };
        // XNU publishes allproc entries only after clearing P_REF_NEW; the
        // listed PID's ESRCH may therefore reflect exit before SZOMB/waitid.
        // Exec waits/retries internally; inspection denial must still fail.
        if filled == 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            continue;
        }
        if filled != bytes {
            return Err(io::Error::last_os_error());
        }
        if info.pbi_pgid == group && info.pbi_status != libc::SZOMB {
            return Ok(false);
        }
    }
    Ok(true)
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        if !self.finished
            && let Err(error) = self.stop()
        {
            tracing::error!(%error, "mirror child cleanup failed");
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests;
