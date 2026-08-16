//! Integration test for the coordinator graceful shutdown sequence.
//!
//! Verifies that starting a coordinator and then shutting it down properly
//! removes daemon.sock, daemon.pid, and daemon.lock files.

#![cfg(feature = "fuse")]
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "test assertions")]

use std::sync::Arc;

use tokio::sync::Mutex;

use crab::vfs::coordinator::{Coordinator, CoordinatorConfig};

/// Start a coordinator, trigger shutdown via cancellation, and verify all
/// daemon files are removed.
#[tokio::test]
async fn shutdown_removes_daemon_files() {
    let tmp = tempfile::tempdir().unwrap();
    let config = CoordinatorConfig::with_base_dir(tmp.path().to_path_buf());

    let mut coordinator = Coordinator::start(config.clone()).unwrap();

    // Verify files were created during startup.
    assert!(
        config.lock_path().exists(),
        "lock file should exist after start"
    );
    assert!(
        config.pid_path().exists(),
        "PID file should exist after start"
    );

    // Trigger graceful shutdown.
    coordinator.shutdown_graceful().await;

    // Verify all daemon files are removed.
    assert!(
        !config.socket_path().exists(),
        "socket file should be removed after shutdown"
    );
    assert!(
        !config.pid_path().exists(),
        "PID file should be removed after shutdown"
    );
    assert!(
        !config.lock_path().exists(),
        "lock file should be removed after shutdown"
    );
}

/// Start a coordinator with no mounts, cancel the token, and verify
/// shutdown completes quickly (no 10s wait when there are no mounts).
#[tokio::test]
async fn shutdown_with_no_mounts_completes_immediately() {
    let tmp = tempfile::tempdir().unwrap();
    let config = CoordinatorConfig::with_base_dir(tmp.path().to_path_buf());

    let mut coordinator = Coordinator::start(config.clone()).unwrap();

    let start = std::time::Instant::now();
    coordinator.shutdown_graceful().await;
    let elapsed = start.elapsed();

    // With no mounts, shutdown should be nearly instant (well under 1s).
    assert!(
        elapsed.as_secs() < 2,
        "shutdown with no mounts took too long: {elapsed:?}"
    );

    assert!(!config.pid_path().exists());
    assert!(!config.lock_path().exists());
}

/// Verify that the synchronous shutdown() also cleans up files.
#[test]
fn sync_shutdown_removes_daemon_files() {
    let tmp = tempfile::tempdir().unwrap();
    let config = CoordinatorConfig::with_base_dir(tmp.path().to_path_buf());

    let mut coordinator = Coordinator::start(config.clone()).unwrap();

    assert!(config.lock_path().exists());
    assert!(config.pid_path().exists());

    coordinator.shutdown();

    assert!(
        !config.pid_path().exists(),
        "PID file should be removed after sync shutdown"
    );
    assert!(
        !config.lock_path().exists(),
        "lock file should be removed after sync shutdown"
    );
}

/// Verify that cancelling the coordinator token propagates to mount child
/// tokens during graceful shutdown.
#[tokio::test]
async fn shutdown_cancels_all_mount_child_tokens() {
    let tmp = tempfile::tempdir().unwrap();
    let config = CoordinatorConfig::with_base_dir(tmp.path().to_path_buf());

    let mut coordinator = Coordinator::start(config.clone()).unwrap();

    // Create child tokens as if mounts were registered.
    let child_a = coordinator.child_cancel_token();
    let child_b = coordinator.child_cancel_token();
    let child_c = coordinator.child_cancel_token();

    assert!(!child_a.is_cancelled());
    assert!(!child_b.is_cancelled());
    assert!(!child_c.is_cancelled());

    coordinator.shutdown_graceful().await;

    // All child tokens should be cancelled after shutdown.
    assert!(
        child_a.is_cancelled(),
        "child token A should be cancelled after shutdown"
    );
    assert!(
        child_b.is_cancelled(),
        "child token B should be cancelled after shutdown"
    );
    assert!(
        child_c.is_cancelled(),
        "child token C should be cancelled after shutdown"
    );
}

/// Simulate the full start → shutdown → verify pattern as described in the
/// task: start coordinator, then shut it down, verify files removed.
#[tokio::test]
async fn full_lifecycle_start_shutdown_verify() {
    let tmp = tempfile::tempdir().unwrap();
    let config = CoordinatorConfig::with_base_dir(tmp.path().to_path_buf());

    // Start.
    let coordinator = Coordinator::start(config.clone()).unwrap();
    let cancel_token = coordinator.cancel_token().clone();
    let coordinator = Arc::new(Mutex::new(coordinator));

    // Verify running state.
    {
        let coord = coordinator.lock().await;
        assert_eq!(coord.mount_count(), 0);
        assert!(coord.uptime_secs() < 5);
    }

    // Trigger shutdown via cancellation token (simulates signal or IPC shutdown).
    cancel_token.cancel();

    // Perform graceful shutdown.
    {
        let mut coord = coordinator.lock().await;
        coord.shutdown_graceful().await;
    }

    // Verify all files removed.
    assert!(
        !config.socket_path().exists(),
        "socket should not exist after lifecycle"
    );
    assert!(
        !config.pid_path().exists(),
        "PID file should not exist after lifecycle"
    );
    assert!(
        !config.lock_path().exists(),
        "lock file should not exist after lifecycle"
    );
}
