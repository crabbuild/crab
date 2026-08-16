//! `crab coordinator` hidden subcommand — manages the VFS mount coordinator
//! daemon lifecycle (start, stop, status).
//!
//! These commands are internal infrastructure, not user-facing. They are
//! invoked by `IpcClient::connect_or_spawn()` and by developers debugging
//! the coordinator.

use std::process::ExitCode;
#[cfg(any(feature = "fuse", test))]
use std::time::Duration;

use clap::Subcommand;
#[cfg(feature = "fuse")]
use tracing::{error, info, warn};

use crate::core::error::{CrabError, Result};
#[cfg(feature = "fuse")]
use crate::core::output::emit_json;

/// Subcommands for `crab coordinator` (hidden).
#[derive(Subcommand)]
pub enum CoordinatorCmd {
    /// Start the mount coordinator daemon.
    Start {
        /// Run in foreground (don't daemonize).
        #[arg(long)]
        foreground: bool,
    },
    /// Stop the running coordinator.
    Stop,
    /// Show coordinator status.
    Status {
        /// Output as JSON.
        #[arg(long)]
        json: bool,
    },
}

/// Dispatch a coordinator subcommand.
pub async fn run_coordinator(cmd: CoordinatorCmd) -> Result<ExitCode> {
    match cmd {
        CoordinatorCmd::Start { foreground } => run_start(foreground).await,
        CoordinatorCmd::Stop => run_stop().await,
        CoordinatorCmd::Status { json } => run_status(json).await,
    }
}

/// Run `coordinator start` before the top-level CLI runtime is created.
pub fn run_coordinator_start_standalone(foreground: bool) -> Result<ExitCode> {
    run_start_standalone(foreground)
}

// ---------------------------------------------------------------------------
// Start
// ---------------------------------------------------------------------------

/// Start the coordinator daemon.
///
/// If `foreground` is false, daemonizes via double-fork before proceeding.
/// Otherwise runs in the current process (useful for debugging).
///
/// Steps:
/// 1. Daemonize (if not foreground)
/// 2. Acquire flock on daemon.lock
/// 3. Remove stale socket if present
/// 4. Bind IpcServer
/// 5. Write PID to daemon.pid
/// 6. Signal readiness (if daemonized)
/// 7. Enter accept loop
#[cfg(feature = "fuse")]
async fn run_start(foreground: bool) -> Result<ExitCode> {
    if !foreground {
        return Err(CrabError::Internal(
            "coordinator start must run before the CLI runtime is created".into(),
        ));
    }

    let Some((config, pipe_write_fd, _log_guard)) = prepare_start(foreground)? else {
        return Ok(ExitCode::SUCCESS);
    };
    run_started_coordinator(config, foreground, pipe_write_fd).await
}

#[cfg(feature = "fuse")]
fn run_start_standalone(foreground: bool) -> Result<ExitCode> {
    let Some((config, pipe_write_fd, _log_guard)) = prepare_start(foreground)? else {
        return Ok(ExitCode::SUCCESS);
    };
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(std::thread::available_parallelism().map_or(4, |n| n.get().max(4)))
        .enable_all()
        .build()
        .map_err(|e| CrabError::Internal(format!("failed to build tokio runtime: {e}")))?;

    rt.block_on(run_started_coordinator(config, foreground, pipe_write_fd))
}

#[cfg(not(feature = "fuse"))]
fn run_start_standalone(_foreground: bool) -> Result<ExitCode> {
    Err(CrabError::Internal(
        "coordinator requires the 'fuse' feature (not available on this platform)".into(),
    ))
}

#[cfg(feature = "fuse")]
fn prepare_start(
    foreground: bool,
) -> Result<
    Option<(
        crate::vfs::coordinator::CoordinatorConfig,
        Option<i32>,
        Option<tracing_appender::non_blocking::WorkerGuard>,
    )>,
> {
    use crate::vfs::coordinator::CoordinatorConfig;
    use crate::vfs::daemonize::{DaemonizeResult, daemonize, signal_failed};
    use crate::vfs::logging;

    let config = CoordinatorConfig::default_config()?;
    let log_path = config.base_dir.join("daemon.log");

    let (pipe_write_fd, log_guard) = if foreground {
        logging::init_foreground_logging()?;
        (None, None)
    } else {
        // Create the log directory before daemonization so stdout/stderr can
        // be redirected even if file tracing initialization fails later.
        std::fs::create_dir_all(&config.base_dir).map_err(|e| {
            CrabError::Internal(format!(
                "failed to create log directory {}: {e}",
                config.base_dir.display()
            ))
        })?;
        #[cfg(unix)]
        {
            let timeout = Duration::from_secs(5);
            match daemonize(&log_path, timeout)? {
                DaemonizeResult::Parent { child_pid } => {
                    info!(child_pid, "coordinator daemon started");
                    return Ok(None);
                }
                DaemonizeResult::Daemon { pipe_write_fd } => {
                    let guard = match logging::init_daemon_logging(&log_path) {
                        Ok(guard) => guard,
                        Err(err) => {
                            signal_failed(pipe_write_fd);
                            return Err(err.into());
                        }
                    };
                    (Some(pipe_write_fd), Some(guard))
                }
            }
        }
        #[cfg(not(unix))]
        {
            return Err(CrabError::Internal(
                "daemonization is not supported on this platform; use --foreground".into(),
            ));
        }
    };

    Ok(Some((config, pipe_write_fd, log_guard)))
}

#[cfg(feature = "fuse")]
async fn run_started_coordinator(
    config: crate::vfs::coordinator::CoordinatorConfig,
    foreground: bool,
    pipe_write_fd: Option<i32>,
) -> Result<ExitCode> {
    use std::sync::Arc;

    use tokio::sync::Mutex;

    use crate::vfs::coordinator::Coordinator;
    use crate::vfs::daemonize::{signal_failed, signal_ready};
    use crate::vfs::ipc_server::IpcServer;

    // Pre-startup: clean up stale PID/socket from a previously crashed coordinator.
    cleanup_stale_pid(&config);

    // Step 2–5: Start the coordinator (acquires lock, writes PID, etc.).
    let coordinator = match Coordinator::start(config) {
        Ok(c) => c,
        Err(e) => {
            error!(error = %e, "coordinator startup failed");
            if let Some(fd) = pipe_write_fd {
                signal_failed(fd);
            }
            return Err(e.into());
        }
    };

    let socket_path = coordinator.socket_path().to_path_buf();
    let cancel_token = coordinator.cancel_token().clone();
    let coordinator = Arc::new(Mutex::new(coordinator));

    // Step 4: Create and bind the IPC server.
    let server = IpcServer::new(
        Arc::clone(&coordinator),
        socket_path.clone(),
        cancel_token.clone(),
    )
    .with_read_resolver(crate::cmd::mount::mount_read_resolver());

    // Spawn signal handler alongside the IPC server.
    let signal_cancel = cancel_token.clone();
    let signal_coordinator = Arc::clone(&coordinator);
    tokio::spawn(crate::vfs::signal_handler::run_signal_handler(
        signal_cancel,
        signal_coordinator,
    ));

    // Spawn idle-exit watchdog: if the coordinator has zero mounts for
    // longer than the idle timeout, cancel the token and exit. This
    // prevents orphaned coordinator processes during development when
    // tests spawn a coordinator but never send a shutdown request.
    let idle_cancel = cancel_token.clone();
    let idle_coordinator = Arc::clone(&coordinator);
    tokio::spawn(async move {
        const IDLE_TIMEOUT: Duration = Duration::from_secs(300); // 5 minutes
        const POLL_INTERVAL: Duration = Duration::from_secs(30);

        let mut idle_since: Option<tokio::time::Instant> = Some(tokio::time::Instant::now());

        loop {
            tokio::select! {
                () = idle_cancel.cancelled() => break,
                () = tokio::time::sleep(POLL_INTERVAL) => {}
            }

            let coord = idle_coordinator.lock().await;
            if coord.mount_count() == 0 {
                if let Some(start) = idle_since {
                    if start.elapsed() >= IDLE_TIMEOUT {
                        info!(
                            idle_secs = start.elapsed().as_secs(),
                            "coordinator idle with no mounts, exiting"
                        );
                        idle_cancel.cancel();
                        break;
                    }
                } else {
                    idle_since = Some(tokio::time::Instant::now());
                }
            } else {
                // Reset idle timer when mounts are active.
                idle_since = None;
            }
        }
    });

    // Step 7: Enter accept loop (runs until cancellation).
    let ready_socket_path = socket_path.clone();
    let socket_bound = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let socket_bound_for_hook = Arc::clone(&socket_bound);
    if let Err(e) = server
        .run_with_bound_hook(move || {
            socket_bound_for_hook.store(true, std::sync::atomic::Ordering::SeqCst);
            if let Some(fd) = pipe_write_fd {
                signal_ready(fd);
            }

            info!(
                pid = std::process::id(),
                socket = %ready_socket_path.display(),
                version = env!("CRAB_BUILD_VERSION"),
                foreground,
                "coordinator ready"
            );
        })
        .await
    {
        if !socket_bound.load(std::sync::atomic::Ordering::SeqCst)
            && let Some(fd) = pipe_write_fd
        {
            signal_failed(fd);
        }
        error!(error = %e, "IPC server error");
    }

    // Shutdown: gracefully wait for mount tasks, then clean up.
    let mut coord = coordinator.lock().await;
    let final_mount_count = coord.mount_count();

    coord.shutdown_graceful().await;

    info!(
        shutdown_reason = "cancellation",
        mount_count = final_mount_count,
        "coordinator exited"
    );

    Ok(ExitCode::SUCCESS)
}

/// Fallback when the `fuse` feature is not enabled.
#[cfg(not(feature = "fuse"))]
async fn run_start(_foreground: bool) -> Result<ExitCode> {
    Err(CrabError::Internal(
        "coordinator requires the 'fuse' feature (not available on this platform)".into(),
    ))
}

// ---------------------------------------------------------------------------
// Stop
// ---------------------------------------------------------------------------

/// Stop the running coordinator.
///
/// Connects via IPC, sends a Shutdown request, then waits up to 15 seconds
/// for the process to exit. If it's still running after the timeout, sends
/// SIGKILL.
#[cfg(feature = "fuse")]
async fn run_stop() -> Result<ExitCode> {
    use crate::vfs::coordinator::{CoordinatorConfig, read_daemon_pid};
    use crate::vfs::ipc_client::IpcClient;
    use crate::vfs::ipc_server::IpcRequest;

    let config = CoordinatorConfig::default_config()?;
    let socket_path = config.socket_path();

    // Connect to the coordinator.
    let mut client = match IpcClient::connect(&socket_path).await {
        Ok(c) => c,
        Err(e) => {
            // If we can't connect, the coordinator may not be running.
            warn!(error = %e, "could not connect to coordinator");
            eprintln!("Coordinator is not running.");
            return Ok(ExitCode::FAILURE);
        }
    };

    // Send shutdown request.
    let response = client
        .send(&IpcRequest::Shutdown)
        .await
        .map_err(|e| CrabError::Internal(format!("failed to send shutdown: {e}")))?;

    if !response.ok {
        let msg = response.error.unwrap_or_else(|| "unknown error".into());
        eprintln!("Shutdown request failed: {msg}");
        return Ok(ExitCode::FAILURE);
    }

    info!("shutdown request acknowledged");

    // Wait for the process to exit (up to 15 seconds).
    let pid = read_daemon_pid(&config.base_dir);
    if let Some(pid) = pid {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
        loop {
            if !is_process_alive(pid) {
                println!("Coordinator stopped (pid {pid}).");
                return Ok(ExitCode::SUCCESS);
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }

        // Process still alive after 15s — send SIGKILL.
        warn!(pid, "coordinator did not exit within 15s, sending SIGKILL");
        force_kill(pid);
        eprintln!("Coordinator (pid {pid}) force-killed after 15s timeout.");
        return Ok(ExitCode::from(2));
    }

    println!("Coordinator stopped.");
    Ok(ExitCode::SUCCESS)
}

/// Fallback when the `fuse` feature is not enabled.
#[cfg(not(feature = "fuse"))]
async fn run_stop() -> Result<ExitCode> {
    Err(CrabError::Internal(
        "coordinator requires the 'fuse' feature (not available on this platform)".into(),
    ))
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

/// Show coordinator status by sending a Health request.
///
/// Connects via IPC, sends Health, and displays the results as plain text
/// or JSON depending on the `--json` flag.
#[cfg(feature = "fuse")]
async fn run_status(json: bool) -> Result<ExitCode> {
    use crate::vfs::coordinator::CoordinatorConfig;
    use crate::vfs::ipc_client::IpcClient;
    use crate::vfs::ipc_server::IpcRequest;

    let config = CoordinatorConfig::default_config()?;
    let socket_path = config.socket_path();

    // Connect to the coordinator.
    let Ok(mut client) = IpcClient::connect(&socket_path).await else {
        if json {
            emit_json(
                "coordinator.status",
                "1.0",
                serde_json::json!({ "running": false }),
            );
        } else {
            eprintln!("Coordinator is not running.");
        }
        return Ok(ExitCode::FAILURE);
    };

    // Send health request.
    let response = client
        .send(&IpcRequest::Health)
        .await
        .map_err(|e| CrabError::Internal(format!("failed to send health request: {e}")))?;

    if !response.ok {
        let msg = response.error.unwrap_or_else(|| "unknown error".into());
        eprintln!("Health check failed: {msg}");
        return Ok(ExitCode::FAILURE);
    }

    if json {
        // Output structured JSON.
        let output = serde_json::json!({
            "running": true,
            "pid": response.pid,
            "uptime_secs": response.uptime_secs,
            "mount_count": response.mount_count,
            "cache_size_bytes": response.cache_size_bytes,
            "hydration_queue_depth": response.hydration_queue_depth,
            "hydration_workers": response.hydration_workers,
        });
        emit_json("coordinator.status", "1.0", output);
    } else {
        // Human-readable output.
        println!("Coordinator: running");
        if let Some(pid) = response.pid {
            println!("  PID:               {pid}");
        }
        if let Some(uptime) = response.uptime_secs {
            println!("  Uptime:            {uptime}s");
        }
        if let Some(mounts) = response.mount_count {
            println!("  Active mounts:     {mounts}");
        }
        if let Some(cache) = response.cache_size_bytes {
            let mb = cache / (1024 * 1024);
            println!("  Cache capacity:    {mb} MiB");
        }
        if let Some(depth) = response.hydration_queue_depth {
            println!("  Hydration queue:   {depth}");
        }
        if let Some(workers) = response.hydration_workers {
            println!("  Hydration workers: {workers}");
        }
    }

    Ok(ExitCode::SUCCESS)
}

/// Fallback when the `fuse` feature is not enabled.
#[cfg(not(feature = "fuse"))]
async fn run_status(_json: bool) -> Result<ExitCode> {
    Err(CrabError::Internal(
        "coordinator requires the 'fuse' feature (not available on this platform)".into(),
    ))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Check if a process with the given PID is still alive.
#[cfg(all(unix, any(feature = "fuse", test)))]
fn is_process_alive(pid: u32) -> bool {
    // SAFETY: kill with signal 0 just checks if the process exists.
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[cfg(all(not(unix), any(feature = "fuse", test)))]
fn is_process_alive(_pid: u32) -> bool {
    false
}

/// Send SIGKILL to a process.
#[cfg(all(unix, feature = "fuse"))]
fn force_kill(pid: u32) {
    // SAFETY: sending SIGKILL to a valid PID is safe.
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
    }
}

#[cfg(all(not(unix), feature = "fuse"))]
fn force_kill(_pid: u32) {
    // No-op on non-Unix platforms.
}

/// Clean up stale PID and socket files from a previously crashed coordinator.
///
/// If a PID file exists and the recorded process is dead, removes both the
/// PID file and socket file so the new coordinator can start cleanly.
/// If the process is alive, we leave the files alone — the flock acquisition
/// in `Coordinator::start()` is the authoritative single-instance guard.
#[cfg(feature = "fuse")]
fn cleanup_stale_pid(config: &crate::vfs::coordinator::CoordinatorConfig) {
    use std::fs;

    let pid_path = config.pid_path();
    let socket_path = config.socket_path();

    // If no PID file exists, nothing to clean up.
    let Ok(pid_contents) = fs::read_to_string(&pid_path) else {
        return;
    };

    let Ok(pid) = pid_contents.trim().parse::<u32>() else {
        // Malformed PID file — remove it and the socket.
        warn!(path = %pid_path.display(), "removing malformed PID file");
        let _ = fs::remove_file(&pid_path);
        let _ = fs::remove_file(&socket_path);
        return;
    };

    if is_process_alive(pid) {
        // Process is alive. The flock in Coordinator::start() will determine
        // whether it's actually our coordinator or a recycled PID.
        info!(
            pid,
            "existing coordinator process detected, flock will arbitrate"
        );
    } else {
        // Process is dead — stale PID file from a crash.
        warn!(
            pid,
            pid_path = %pid_path.display(),
            socket_path = %socket_path.display(),
            "detected stale PID file from crashed coordinator, cleaning up"
        );
        if let Err(e) = fs::remove_file(&pid_path) {
            warn!(error = %e, path = %pid_path.display(), "failed to remove stale PID file");
        }
        if socket_path.exists()
            && let Err(e) = fs::remove_file(&socket_path)
        {
            warn!(error = %e, path = %socket_path.display(), "failed to remove stale socket file");
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;

    #[cfg(all(unix, feature = "fuse"))]
    #[test]
    fn cleanup_stale_pid_removes_files_for_dead_process() {
        use std::fs;

        use crate::vfs::coordinator::CoordinatorConfig;

        let tmp = tempfile::tempdir().unwrap();
        let config = CoordinatorConfig::with_base_dir(tmp.path().to_path_buf());

        // Create PID file with a dead PID (very high, unlikely to exist).
        let pid_path = config.pid_path();
        let socket_path = config.socket_path();
        fs::create_dir_all(tmp.path()).unwrap();
        fs::write(&pid_path, "999999999").unwrap();
        fs::write(&socket_path, "fake-socket").unwrap();

        assert!(pid_path.exists());
        assert!(socket_path.exists());

        cleanup_stale_pid(&config);

        // Both files should be removed.
        assert!(!pid_path.exists(), "stale PID file should be removed");
        assert!(!socket_path.exists(), "stale socket file should be removed");
    }

    #[cfg(all(unix, feature = "fuse"))]
    #[test]
    fn cleanup_stale_pid_leaves_files_for_live_process() {
        use std::fs;

        use crate::vfs::coordinator::CoordinatorConfig;

        let tmp = tempfile::tempdir().unwrap();
        let config = CoordinatorConfig::with_base_dir(tmp.path().to_path_buf());

        // Write our own PID — we're definitely alive.
        let pid_path = config.pid_path();
        let socket_path = config.socket_path();
        fs::create_dir_all(tmp.path()).unwrap();
        fs::write(&pid_path, std::process::id().to_string()).unwrap();
        fs::write(&socket_path, "fake-socket").unwrap();

        cleanup_stale_pid(&config);

        // Files should remain — the process is alive.
        assert!(pid_path.exists(), "PID file should remain for live process");
        assert!(
            socket_path.exists(),
            "socket file should remain for live process"
        );
    }

    #[cfg(all(unix, feature = "fuse"))]
    #[test]
    fn cleanup_stale_pid_handles_missing_pid_file() {
        use crate::vfs::coordinator::CoordinatorConfig;

        let tmp = tempfile::tempdir().unwrap();
        let config = CoordinatorConfig::with_base_dir(tmp.path().to_path_buf());

        // No PID file exists — should be a no-op.
        cleanup_stale_pid(&config);
        // No panic, no error.
    }

    #[cfg(all(unix, feature = "fuse"))]
    #[test]
    fn cleanup_stale_pid_handles_malformed_pid_file() {
        use std::fs;

        use crate::vfs::coordinator::CoordinatorConfig;

        let tmp = tempfile::tempdir().unwrap();
        let config = CoordinatorConfig::with_base_dir(tmp.path().to_path_buf());

        let pid_path = config.pid_path();
        let socket_path = config.socket_path();
        fs::create_dir_all(tmp.path()).unwrap();
        fs::write(&pid_path, "not-a-number").unwrap();
        fs::write(&socket_path, "fake-socket").unwrap();

        cleanup_stale_pid(&config);

        // Malformed PID file should be removed along with socket.
        assert!(!pid_path.exists(), "malformed PID file should be removed");
        assert!(
            !socket_path.exists(),
            "socket should be removed with malformed PID"
        );
    }

    #[cfg(all(unix, feature = "fuse"))]
    #[test]
    fn cleanup_stale_pid_no_socket_file_only_removes_pid() {
        use std::fs;

        use crate::vfs::coordinator::CoordinatorConfig;

        let tmp = tempfile::tempdir().unwrap();
        let config = CoordinatorConfig::with_base_dir(tmp.path().to_path_buf());

        // Create PID file with a dead PID but no socket file.
        let pid_path = config.pid_path();
        fs::create_dir_all(tmp.path()).unwrap();
        fs::write(&pid_path, "999999999").unwrap();

        cleanup_stale_pid(&config);

        assert!(!pid_path.exists(), "stale PID file should be removed");
        // No panic from trying to remove non-existent socket.
    }

    #[cfg(unix)]
    #[test]
    fn is_process_alive_true_for_self() {
        assert!(is_process_alive(std::process::id()));
    }

    #[cfg(unix)]
    #[test]
    fn is_process_alive_false_for_dead_pid() {
        // PID 999999999 is extremely unlikely to be a running process.
        assert!(!is_process_alive(999_999_999));
    }

    #[cfg(all(unix, feature = "fuse"))]
    #[test]
    fn flock_is_authoritative_guard_not_pid_file() {
        use std::fs;

        use crate::vfs::coordinator::{Coordinator, CoordinatorConfig};

        let tmp = tempfile::tempdir().unwrap();
        let config = CoordinatorConfig::with_base_dir(tmp.path().to_path_buf());

        // Write a PID file with our own PID (alive process).
        let pid_path = config.pid_path();
        fs::create_dir_all(tmp.path()).unwrap();
        fs::write(&pid_path, std::process::id().to_string()).unwrap();

        // cleanup_stale_pid leaves files alone for a live process.
        cleanup_stale_pid(&config);

        // But Coordinator::start() acquires the flock — first call succeeds.
        let _coordinator = Coordinator::start(config.clone()).unwrap();

        // Second call should fail because flock is held, regardless of PID file.
        let second_config = CoordinatorConfig::with_base_dir(tmp.path().to_path_buf());
        let result = Coordinator::start(second_config);
        assert!(result.is_err());
        let err_msg = format!("{}", result.as_ref().err().unwrap());
        assert!(
            err_msg.contains("another coordinator is already running"),
            "expected flock error, got: {err_msg}"
        );
    }

    #[cfg(feature = "fuse")]
    #[tokio::test]
    async fn connect_or_spawn_removes_stale_socket_file() {
        use std::fs;

        use crate::vfs::ipc_client::IpcClient;

        let tmp = tempfile::tempdir().unwrap();
        let socket_path = tmp.path().join("daemon.sock");

        // Create a stale socket artifact that the IPC client may remove.
        fs::write(&socket_path, "").unwrap();
        assert!(socket_path.exists());

        // connect_or_spawn will fail to connect, then try to remove the stale
        // socket and spawn. The spawn will fail (no real coordinator binary in
        // test), but the stale socket should be removed before the spawn attempt.
        let result = IpcClient::connect_or_spawn(&socket_path).await;

        // The call will fail (can't spawn coordinator in test), but the stale
        // socket should have been removed.
        assert!(result.is_err());
        assert!(
            !socket_path.exists(),
            "stale socket file should be removed before spawn attempt"
        );
    }
}
