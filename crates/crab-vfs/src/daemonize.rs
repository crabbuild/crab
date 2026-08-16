//! Double-fork daemonization with readiness signaling.
//!
//! Implements the classic Unix double-fork pattern so the coordinator
//! fully detaches from the controlling terminal and cannot accidentally
//! reacquire one. A pipe between the original parent and the final daemon
//! grandchild carries a single-byte readiness signal.

use std::path::Path;
use std::time::Duration;

use crate::core::error::{CrabError, Result};

// ---------------------------------------------------------------------------
// DaemonizeResult
// ---------------------------------------------------------------------------

/// Outcome of the daemonization call — tells the caller which side of the
/// fork they ended up on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonizeResult {
    /// We are the original parent process. The daemon child signaled
    /// readiness (or we timed out waiting).
    Parent { child_pid: u32 },
    /// We are the final daemon grandchild — proceed with coordinator startup.
    /// The caller should use `signal_ready` or `signal_failed` on the
    /// provided pipe write fd after binding the socket.
    Daemon { pipe_write_fd: i32 },
}

// ---------------------------------------------------------------------------
// Readiness protocol bytes
// ---------------------------------------------------------------------------

/// Byte written by the daemon to indicate successful startup.
pub const READY_BYTE: u8 = 0x01;

/// Byte written by the daemon to indicate startup failure.
pub const FAILED_BYTE: u8 = 0x00;

// ---------------------------------------------------------------------------
// Public API (Unix)
// ---------------------------------------------------------------------------

/// Daemonize the current process using the double-fork pattern.
///
/// 1. Create a pipe for readiness signaling
/// 2. First fork: parent waits on pipe with timeout, child continues
/// 3. Child calls `setsid()` to become session leader
/// 4. Second fork: intermediate exits immediately, grandchild continues
/// 5. Grandchild redirects stdin→/dev/null, stdout/stderr→log file
/// 6. Returns `DaemonizeResult::Daemon` with the pipe write fd
///
/// The parent blocks until it reads a byte from the pipe or the timeout
/// expires. On `READY_BYTE` it returns `Parent { child_pid }`. On
/// `FAILED_BYTE` or timeout it returns an error.
#[cfg(unix)]
pub fn daemonize(log_path: &Path, timeout: Duration) -> Result<DaemonizeResult> {
    // Step 1: Create the readiness pipe.
    let (pipe_read_fd, pipe_write_fd) = create_pipe()?;

    // Step 2: First fork.
    let fork_result = fork()?;
    if fork_result > 0 {
        // We are the parent. Close write end and wait for readiness.
        // SAFETY: closing a valid fd we own.
        unsafe { libc::close(pipe_write_fd) };

        let child_pid = fork_result as u32;
        wait_for_readiness(pipe_read_fd, timeout, child_pid)?;

        return Ok(DaemonizeResult::Parent { child_pid });
    }

    // We are the first child. Close read end of pipe.
    // SAFETY: closing a valid fd we own.
    unsafe { libc::close(pipe_read_fd) };

    // Step 3: Create a new session (detach from terminal).
    let sid = unsafe { libc::setsid() };
    if sid < 0 {
        let err = std::io::Error::last_os_error();
        return Err(CrabError::Internal(format!("setsid() failed: {err}")));
    }

    // Step 4: Second fork — the intermediate child exits, grandchild continues.
    let fork2_result = fork()?;
    if fork2_result > 0 {
        // Intermediate child exits immediately.
        std::process::exit(0);
    }

    // We are the grandchild (final daemon process).

    // Step 5: Redirect file descriptors.
    redirect_fds(log_path)?;

    // Change working directory to root to avoid holding mounts.
    // SAFETY: chdir to "/" is always valid.
    unsafe { libc::chdir(c"/".as_ptr()) };

    // Return Daemon with the pipe write fd so the caller can signal readiness
    // after binding the socket.
    Ok(DaemonizeResult::Daemon { pipe_write_fd })
}

/// Signal to the waiting parent that the daemon started successfully.
///
/// Writes `READY_BYTE` (0x01) to the pipe and closes it.
#[cfg(unix)]
pub fn signal_ready(pipe_write_fd: i32) {
    let buf = [READY_BYTE];
    // SAFETY: writing a single byte to a valid pipe fd.
    unsafe {
        libc::write(pipe_write_fd, buf.as_ptr().cast::<libc::c_void>(), 1);
        libc::close(pipe_write_fd);
    }
}

/// Signal to the waiting parent that the daemon failed to start.
///
/// Writes `FAILED_BYTE` (0x00) to the pipe and closes it.
#[cfg(unix)]
pub fn signal_failed(pipe_write_fd: i32) {
    let buf = [FAILED_BYTE];
    // SAFETY: writing a single byte to a valid pipe fd.
    unsafe {
        libc::write(pipe_write_fd, buf.as_ptr().cast::<libc::c_void>(), 1);
        libc::close(pipe_write_fd);
    }
}

// ---------------------------------------------------------------------------
// Non-Unix fallback
// ---------------------------------------------------------------------------

/// Daemonization is not supported on this platform.
///
/// Returns an error directing the user to use `--foreground` mode.
#[cfg(not(unix))]
pub fn daemonize(_log_path: &Path, _timeout: Duration) -> Result<DaemonizeResult> {
    Err(CrabError::Internal(
        "daemonization is not supported on this platform; use --foreground".into(),
    ))
}

/// No-op on non-Unix — should never be called.
#[cfg(not(unix))]
pub fn signal_ready(_pipe_write_fd: i32) {}

/// No-op on non-Unix — should never be called.
#[cfg(not(unix))]
pub fn signal_failed(_pipe_write_fd: i32) {}

// ---------------------------------------------------------------------------
// Internal helpers (Unix)
// ---------------------------------------------------------------------------

/// Create an anonymous pipe, returning (read_fd, write_fd).
#[cfg(unix)]
fn create_pipe() -> Result<(i32, i32)> {
    let mut fds = [0i32; 2];
    // SAFETY: pipe() with a valid 2-element array is safe.
    let ret = unsafe { libc::pipe(fds.as_mut_ptr()) };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        return Err(CrabError::Internal(format!("pipe() failed: {err}")));
    }
    Ok((fds[0], fds[1]))
}

/// Fork the current process. Returns the child PID to the parent (>0),
/// 0 to the child.
#[cfg(unix)]
fn fork() -> Result<libc::pid_t> {
    // SAFETY: fork() is safe when we handle both sides correctly.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        let err = std::io::Error::last_os_error();
        return Err(CrabError::Internal(format!("fork() failed: {err}")));
    }
    Ok(pid)
}

/// Wait for a readiness byte on the pipe read fd with a timeout.
///
/// Uses `poll()` to implement the timeout. Returns Ok on READY_BYTE,
/// error on FAILED_BYTE or timeout.
#[cfg(unix)]
fn wait_for_readiness(pipe_read_fd: i32, timeout: Duration, child_pid: u32) -> Result<()> {
    let timeout_ms = timeout.as_millis() as i32;

    let mut pollfd = libc::pollfd {
        fd: pipe_read_fd,
        events: libc::POLLIN,
        revents: 0,
    };

    // SAFETY: poll() with a valid pollfd struct is safe.
    let ret = unsafe { libc::poll(std::ptr::addr_of_mut!(pollfd), 1, timeout_ms) };

    if ret == 0 {
        // Timeout — daemon didn't signal in time.
        // SAFETY: closing our pipe fd.
        unsafe { libc::close(pipe_read_fd) };
        return Err(CrabError::Internal(format!(
            "daemon (pid {child_pid}) did not signal readiness within {}s",
            timeout.as_secs()
        )));
    }

    if ret < 0 {
        let err = std::io::Error::last_os_error();
        // SAFETY: closing our pipe fd.
        unsafe { libc::close(pipe_read_fd) };
        return Err(CrabError::Internal(format!(
            "poll() on readiness pipe failed: {err}"
        )));
    }

    // Read the signal byte.
    let mut buf = [0u8; 1];
    // SAFETY: reading one byte from a valid pipe fd.
    let n = unsafe { libc::read(pipe_read_fd, buf.as_mut_ptr().cast::<libc::c_void>(), 1) };
    // SAFETY: closing our pipe fd.
    unsafe { libc::close(pipe_read_fd) };

    if n <= 0 {
        return Err(CrabError::Internal(format!(
            "daemon (pid {child_pid}) closed pipe without signaling readiness"
        )));
    }

    match buf[0] {
        READY_BYTE => Ok(()),
        FAILED_BYTE => Err(CrabError::Internal(format!(
            "daemon (pid {child_pid}) reported startup failure"
        ))),
        other => Err(CrabError::Internal(format!(
            "daemon (pid {child_pid}) sent unexpected readiness byte: 0x{other:02x}"
        ))),
    }
}

/// Redirect stdin to /dev/null and stdout/stderr to the log file.
#[cfg(unix)]
fn redirect_fds(log_path: &Path) -> Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    // Ensure the log file's parent directory exists.
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            CrabError::Internal(format!(
                "failed to create log directory {}: {e}",
                parent.display()
            ))
        })?;
    }

    // Open /dev/null for stdin.
    let dev_null = CString::new("/dev/null")
        .map_err(|e| CrabError::Internal(format!("CString creation failed: {e}")))?;
    // SAFETY: opening /dev/null with O_RDWR is always safe.
    let null_fd = unsafe { libc::open(dev_null.as_ptr(), libc::O_RDWR) };
    if null_fd < 0 {
        let err = std::io::Error::last_os_error();
        return Err(CrabError::Internal(format!(
            "failed to open /dev/null: {err}"
        )));
    }

    // Open log file for stdout/stderr.
    let log_path_c = CString::new(log_path.as_os_str().as_bytes())
        .map_err(|e| CrabError::Internal(format!("invalid log path: {e}")))?;
    // SAFETY: opening a file with O_WRONLY|O_CREAT|O_APPEND and mode 0o644.
    let log_fd = unsafe {
        libc::open(
            log_path_c.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_APPEND,
            0o644,
        )
    };
    if log_fd < 0 {
        let err = std::io::Error::last_os_error();
        // SAFETY: closing the null_fd we opened.
        unsafe { libc::close(null_fd) };
        return Err(CrabError::Internal(format!(
            "failed to open log file {}: {err}",
            log_path.display()
        )));
    }

    // Redirect stdin (fd 0) to /dev/null.
    // SAFETY: dup2 with valid fds is safe.
    unsafe {
        libc::dup2(null_fd, libc::STDIN_FILENO);
        // Redirect stdout (fd 1) and stderr (fd 2) to log file.
        libc::dup2(log_fd, libc::STDOUT_FILENO);
        libc::dup2(log_fd, libc::STDERR_FILENO);
        // Close the original fds (now duplicated).
        if null_fd > libc::STDERR_FILENO {
            libc::close(null_fd);
        }
        if log_fd > libc::STDERR_FILENO {
            libc::close(log_fd);
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;

    #[test]
    fn daemonize_result_parent_construction() {
        let result = DaemonizeResult::Parent { child_pid: 12345 };
        assert_eq!(result, DaemonizeResult::Parent { child_pid: 12345 });

        if let DaemonizeResult::Parent { child_pid } = result {
            assert_eq!(child_pid, 12345);
        } else {
            panic!("expected Parent variant");
        }
    }

    #[test]
    fn daemonize_result_daemon_construction() {
        let result = DaemonizeResult::Daemon { pipe_write_fd: 7 };
        assert_eq!(result, DaemonizeResult::Daemon { pipe_write_fd: 7 });

        if let DaemonizeResult::Daemon { pipe_write_fd } = result {
            assert_eq!(pipe_write_fd, 7);
        } else {
            panic!("expected Daemon variant");
        }
    }

    #[test]
    fn daemonize_result_variants_not_equal() {
        let parent = DaemonizeResult::Parent { child_pid: 1 };
        let daemon = DaemonizeResult::Daemon { pipe_write_fd: 1 };
        assert_ne!(parent, daemon);
    }

    #[test]
    fn ready_and_failed_bytes_are_distinct() {
        assert_ne!(READY_BYTE, FAILED_BYTE);
        assert_eq!(READY_BYTE, 0x01);
        assert_eq!(FAILED_BYTE, 0x00);
    }

    /// Verify the pipe readiness protocol: writing READY_BYTE through a pipe
    /// and reading it back simulates the daemon→parent communication.
    #[cfg(unix)]
    #[test]
    fn pipe_ready_signal_protocol() {
        let (read_fd, write_fd) = create_pipe().unwrap();

        // Simulate daemon signaling ready.
        signal_ready(write_fd);

        // Parent reads the byte.
        let mut buf = [0u8; 1];
        // SAFETY: reading from a valid pipe fd in a test.
        let n = unsafe { libc::read(read_fd, buf.as_mut_ptr() as *mut libc::c_void, 1) };
        unsafe { libc::close(read_fd) };

        assert_eq!(n, 1);
        assert_eq!(buf[0], READY_BYTE);
    }

    /// Verify the pipe failure protocol: writing FAILED_BYTE through a pipe.
    #[cfg(unix)]
    #[test]
    fn pipe_failed_signal_protocol() {
        let (read_fd, write_fd) = create_pipe().unwrap();

        // Simulate daemon signaling failure.
        signal_failed(write_fd);

        // Parent reads the byte.
        let mut buf = [0u8; 1];
        // SAFETY: reading from a valid pipe fd in a test.
        let n = unsafe { libc::read(read_fd, buf.as_mut_ptr() as *mut libc::c_void, 1) };
        unsafe { libc::close(read_fd) };

        assert_eq!(n, 1);
        assert_eq!(buf[0], FAILED_BYTE);
    }

    /// Verify that wait_for_readiness succeeds when READY_BYTE is written.
    #[cfg(unix)]
    #[test]
    fn wait_for_readiness_succeeds_on_ready_byte() {
        let (read_fd, write_fd) = create_pipe().unwrap();

        // Write ready byte from "daemon" side.
        let buf = [READY_BYTE];
        // SAFETY: writing to a valid pipe fd in a test.
        unsafe {
            libc::write(write_fd, buf.as_ptr() as *const libc::c_void, 1);
            libc::close(write_fd);
        }

        // Parent waits — should succeed immediately.
        let result = wait_for_readiness(read_fd, Duration::from_secs(1), 999);
        assert!(result.is_ok());
    }

    /// Verify that wait_for_readiness fails when FAILED_BYTE is written.
    #[cfg(unix)]
    #[test]
    fn wait_for_readiness_fails_on_failed_byte() {
        let (read_fd, write_fd) = create_pipe().unwrap();

        // Write failed byte from "daemon" side.
        let buf = [FAILED_BYTE];
        // SAFETY: writing to a valid pipe fd in a test.
        unsafe {
            libc::write(write_fd, buf.as_ptr() as *const libc::c_void, 1);
            libc::close(write_fd);
        }

        // Parent waits — should return error.
        let result = wait_for_readiness(read_fd, Duration::from_secs(1), 999);
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(err_msg.contains("startup failure"));
    }

    /// Verify that wait_for_readiness times out when nothing is written.
    #[cfg(unix)]
    #[test]
    fn wait_for_readiness_times_out() {
        let (read_fd, _write_fd) = create_pipe().unwrap();

        // Don't write anything — should timeout.
        let result = wait_for_readiness(read_fd, Duration::from_millis(50), 999);
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(err_msg.contains("did not signal readiness"));

        // Clean up write fd.
        // SAFETY: closing a valid fd in a test.
        unsafe { libc::close(_write_fd) };
    }

    /// Verify that wait_for_readiness fails when pipe is closed without writing.
    #[cfg(unix)]
    #[test]
    fn wait_for_readiness_fails_on_closed_pipe() {
        let (read_fd, write_fd) = create_pipe().unwrap();

        // Close write end without writing — simulates daemon crash.
        // SAFETY: closing a valid fd in a test.
        unsafe { libc::close(write_fd) };

        // Parent waits — should get an error about closed pipe.
        let result = wait_for_readiness(read_fd, Duration::from_secs(1), 999);
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(err_msg.contains("closed pipe without signaling"));
    }
}
