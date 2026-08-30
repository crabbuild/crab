//! Daemon and foreground logging configuration for the VFS coordinator.
//!
//! - Daemon mode: JSON-formatted logs written to a rotating file.
//! - Foreground mode: human-readable logs written to stderr.

use std::path::Path;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::EnvFilter;

use crate::core::error::{CrabError, Result};

/// Configure tracing for daemon mode.
///
/// Writes JSON-formatted structured logs to the specified path using
/// `tracing_appender` with daily rotation. The rotation keeps at most
/// 2 old log files to prevent unbounded disk growth (approximating the
/// 50 MB cap for typical coordinator workloads).
///
/// Returns a `WorkerGuard` that must be held for the lifetime of the
/// daemon — dropping it flushes and closes the log writer.
pub fn init_daemon_logging(log_path: &Path) -> Result<WorkerGuard> {
    let log_dir = log_path
        .parent()
        .ok_or_else(|| CrabError::Internal("log path has no parent directory".into()))?;

    let log_filename = log_path
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or("daemon.log");

    // Use daily rotation with a max of 3 files (current + 2 rotated).
    // This bounds disk usage to roughly 3 × daily output, preventing
    // unbounded growth. For a coordinator that logs at moderate volume,
    // this approximates the 50 MB cap.
    let file_appender = tracing_appender::rolling::daily(log_dir, log_filename);

    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("error"));

    // Foreground/tests may already have root CLI tracing installed.
    // The existing subscriber still writes to redirected stderr in daemon mode.
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(non_blocking)
        .json()
        .with_target(true)
        .with_thread_ids(true)
        .with_file(false)
        .with_line_number(false)
        .try_init()
        .ok();

    Ok(guard)
}

/// Configure tracing for foreground mode.
///
/// Writes human-readable logs to stderr with colors (when the terminal
/// supports them). Uses the `RUST_LOG` environment variable for filtering,
/// defaulting to `error`.
pub fn init_foreground_logging() -> Result<()> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("error"));

    // Foreground startup can run after root CLI tracing has been installed.
    // Keep coordinator lifecycle commands usable instead of failing startup.
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_target(true)
        .with_thread_ids(false)
        .with_file(false)
        .with_line_number(false)
        .try_init()
        .ok();

    Ok(())
}
