//! Owned Git subprocess lifetime mechanics shared by product callers.

use process_wrap::std::{ChildWrapper, CommandWrap};
use std::{
    io,
    process::{ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus},
};

/// Retain a child identity through process-tree signalling and reaping.
///
/// Callers must stop before joining pipe workers on failures. Exit polling
/// retains the Unix leader, preventing PID reuse from redirecting cleanup.
pub struct OwnedChild {
    inner: Box<dyn ChildWrapper>,
    finished: bool,
}

impl OwnedChild {
    /// Spawn a command in an owned Unix process group or Windows job.
    pub fn spawn(command: Command) -> io::Result<Self> {
        let mut command = CommandWrap::from(command);
        #[cfg(unix)]
        command.wrap(process_wrap::std::ProcessGroup::leader());
        #[cfg(windows)]
        command.wrap(process_wrap::std::JobObject);
        Ok(Self {
            inner: command.spawn()?,
            finished: false,
        })
    }

    /// Take the command's piped stdin, if configured by the caller.
    pub fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.inner.stdin().take()
    }

    /// Take the command's piped stdout, if configured by the caller.
    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.inner.stdout().take()
    }

    /// Take the command's piped stderr, if configured by the caller.
    pub fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.inner.stderr().take()
    }

    /// Poll exit without releasing process-tree identity.
    #[cfg(unix)]
    pub fn has_exited(&mut self) -> io::Result<bool> {
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

    /// Poll exit without consuming full-job completion events.
    #[cfg(windows)]
    pub fn has_exited(&mut self) -> io::Result<bool> {
        if self.finished {
            return Ok(true);
        }
        // run installs exactly one JobObject wrapper. Poll its lower child:
        // polling the job consumes completion events needed by stop's full-job
        // wait. The native child caches its status for that later wait.
        self.inner
            .inner_mut()
            .try_wait()
            .map(|status| status.is_some())
    }

    /// Request graceful Unix shutdown or terminate the Windows job.
    pub fn request_shutdown(&mut self) -> io::Result<()> {
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

    /// Kill and reap the process tree; repeated calls return its exit status.
    pub fn stop(&mut self) -> io::Result<ExitStatus> {
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
            tracing::error!(%error, "Git child cleanup failed");
        }
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests;
