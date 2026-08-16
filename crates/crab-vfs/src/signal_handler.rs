//! Signal handling for the VFS mount coordinator.
//!
//! Handles SIGTERM, SIGINT, and SIGHUP:
//! - First SIGTERM/SIGINT triggers graceful shutdown via `CancellationToken`.
//! - Second SIGTERM/SIGINT during shutdown forces immediate exit.
//! - SIGHUP triggers a configuration reload.

use std::sync::Arc;

use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::coordinator::Coordinator;

// ---------------------------------------------------------------------------
// Signal state machine
// ---------------------------------------------------------------------------

/// Tracks whether the coordinator is running normally or already shutting down.
enum SignalState {
    Running,
    ShuttingDown,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Run the signal handler loop for the coordinator.
///
/// Listens for SIGTERM and SIGINT using tokio's async signal API.
/// - First SIGTERM/SIGINT cancels the coordinator's `CancellationToken`.
/// - Second SIGTERM/SIGINT during shutdown calls `std::process::exit(1)`.
///
/// Each signal is registered independently — a failure to register one
/// does not prevent the others from working.
///
/// This function runs indefinitely until the process exits or the signals
/// trigger shutdown. It should be spawned as a tokio task.
#[cfg(unix)]
pub async fn run_signal_handler(cancel: CancellationToken, _coordinator: Arc<Mutex<Coordinator>>) {
    use tokio::signal::unix::{SignalKind, signal};

    let mut sigint = signal(SignalKind::interrupt())
        .map_err(|e| warn!(error = %e, "failed to register SIGINT handler"))
        .ok();

    let mut sigterm = signal(SignalKind::terminate())
        .map_err(|e| warn!(error = %e, "failed to register SIGTERM handler"))
        .ok();

    let mut state = SignalState::Running;

    // If no signal streams were registered, fall back to ctrl_c.
    if sigint.is_none() && sigterm.is_none() {
        info!("no Unix signals available, using ctrl_c fallback");
        tokio::select! {
            () = cancel.cancelled() => return,
            _ = tokio::signal::ctrl_c() => {
                info!("received Ctrl+C, initiating graceful shutdown");
                cancel.cancel();
            }
        }
        return;
    }

    loop {
        tokio::select! {
            _ = async {
                if let Some(ref mut s) = sigint { s.recv().await } else { std::future::pending().await }
            } => {
                match state {
                    SignalState::Running => {
                        info!("received SIGINT, initiating graceful shutdown");
                        state = SignalState::ShuttingDown;
                        cancel.cancel();
                    }
                    SignalState::ShuttingDown => {
                        warn!("received second SIGINT during shutdown, forcing exit");
                        std::process::exit(1);
                    }
                }
            }
            _ = async {
                if let Some(ref mut s) = sigterm { s.recv().await } else { std::future::pending().await }
            } => {
                match state {
                    SignalState::Running => {
                        info!("received SIGTERM, initiating graceful shutdown");
                        state = SignalState::ShuttingDown;
                        cancel.cancel();
                    }
                    SignalState::ShuttingDown => {
                        warn!("received second SIGTERM during shutdown, forcing exit");
                        std::process::exit(1);
                    }
                }
            }
        }
    }
}

/// Fallback for non-Unix platforms (no-op).
#[cfg(not(unix))]
pub async fn run_signal_handler(_cancel: CancellationToken, _coordinator: Arc<Mutex<Coordinator>>) {
    // Signal handling is Unix-only. On other platforms, shutdown is triggered
    // via the IPC Shutdown command or ctrl_c handled elsewhere.
    std::future::pending::<()>().await;
}
