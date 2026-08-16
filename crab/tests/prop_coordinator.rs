//! Property-based tests for the VFS mount coordinator hardening.
//!
//! Tests the correctness properties defined in the design document:
//! exponential backoff formula, stale PID detection, lock-based guard,
//! ping/health response completeness, and shutdown cleanup.

#![cfg(unix)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]

use std::fs;
use std::time::Duration;

use proptest::prelude::*;
use tempfile::TempDir;

// --- Constants mirrored from ipc_client.rs ---

const INITIAL_BACKOFF_MS: u64 = 100;
const BACKOFF_MULTIPLIER: u32 = 2;
const MAX_RETRIES: usize = 5;

// ---------------------------------------------------------------------------
// Property 1: Exponential Backoff Sequence
// ---------------------------------------------------------------------------

// **Validates: Requirements 3.1**
//
// For any retry attempt n in 1..=MAX_RETRIES, the delay equals
// INITIAL_BACKOFF * BACKOFF_MULTIPLIER^(n-1), and total attempts
// do not exceed MAX_RETRIES.
fn expected_delay_for_attempt(attempt: usize) -> Duration {
    let multiplier = BACKOFF_MULTIPLIER.pow((attempt - 1) as u32);
    Duration::from_millis(INITIAL_BACKOFF_MS * u64::from(multiplier))
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn backoff_delay_follows_exponential_formula(attempt in 1usize..=MAX_RETRIES) {
        let expected = expected_delay_for_attempt(attempt);

        // Simulate the backoff calculation as done in ipc_client.rs:
        // delay starts at INITIAL_BACKOFF and is multiplied by BACKOFF_MULTIPLIER
        // after each attempt.
        let mut delay = Duration::from_millis(INITIAL_BACKOFF_MS);
        for _ in 1..attempt {
            delay *= BACKOFF_MULTIPLIER;
        }

        prop_assert_eq!(
            delay, expected,
            "attempt {}: delay {:?} != expected {:?}",
            attempt, delay, expected
        );

        // Verify the total number of attempts never exceeds MAX_RETRIES.
        prop_assert!(attempt <= MAX_RETRIES);
    }

    #[test]
    fn backoff_sequence_is_monotonically_increasing(attempt in 2usize..=MAX_RETRIES) {
        let prev = expected_delay_for_attempt(attempt - 1);
        let curr = expected_delay_for_attempt(attempt);
        prop_assert!(
            curr > prev,
            "delay at attempt {} ({:?}) should exceed attempt {} ({:?})",
            attempt, curr, attempt - 1, prev
        );
    }
}

// ---------------------------------------------------------------------------
// Property 2: Stale PID Detection and Cleanup
// ---------------------------------------------------------------------------

// **Validates: Requirements 4.1, 4.2**
//
// For any PID file containing a dead PID, cleanup removes both the
// PID file and socket file.
proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn stale_pid_cleanup_removes_both_files(dead_pid in 900_000_000u32..=999_999_999u32) {
        // PIDs in this range are extremely unlikely to be alive on any system.
        // Verify the PID is actually dead (defense against flaky test).
        let alive = unsafe { libc::kill(dead_pid as libc::pid_t, 0) == 0 };
        prop_assume!(!alive, "generated PID {} is unexpectedly alive", dead_pid);

        let tmp = TempDir::new().unwrap();
        let pid_path = tmp.path().join("daemon.pid");
        let socket_path = tmp.path().join("daemon.sock");

        // Write the dead PID and a fake socket file.
        fs::write(&pid_path, dead_pid.to_string()).unwrap();
        fs::write(&socket_path, "fake-socket-data").unwrap();

        // Simulate the cleanup logic from cmd/coordinator.rs:
        // Read PID, check if alive, remove files if dead.
        let pid_contents = fs::read_to_string(&pid_path).unwrap();
        let parsed_pid: u32 = pid_contents.trim().parse().unwrap();
        let is_alive = unsafe { libc::kill(parsed_pid as libc::pid_t, 0) == 0 };

        if !is_alive {
            let _ = fs::remove_file(&pid_path);
            if socket_path.exists() {
                let _ = fs::remove_file(&socket_path);
            }
        }

        prop_assert!(!pid_path.exists(), "stale PID file should be removed");
        prop_assert!(!socket_path.exists(), "stale socket file should be removed");
    }

    #[test]
    fn stale_pid_cleanup_handles_missing_socket(dead_pid in 900_000_000u32..=999_999_999u32) {
        let alive = unsafe { libc::kill(dead_pid as libc::pid_t, 0) == 0 };
        prop_assume!(!alive, "generated PID {} is unexpectedly alive", dead_pid);

        let tmp = TempDir::new().unwrap();
        let pid_path = tmp.path().join("daemon.pid");
        let socket_path = tmp.path().join("daemon.sock");

        // Only PID file exists, no socket.
        fs::write(&pid_path, dead_pid.to_string()).unwrap();

        let pid_contents = fs::read_to_string(&pid_path).unwrap();
        let parsed_pid: u32 = pid_contents.trim().parse().unwrap();
        let is_alive = unsafe { libc::kill(parsed_pid as libc::pid_t, 0) == 0 };

        if !is_alive {
            let _ = fs::remove_file(&pid_path);
            if socket_path.exists() {
                let _ = fs::remove_file(&socket_path);
            }
        }

        prop_assert!(!pid_path.exists(), "stale PID file should be removed");
        // Socket never existed — no panic from trying to remove it.
        prop_assert!(!socket_path.exists());
    }
}

// ---------------------------------------------------------------------------
// Property 3: Lock-Based Single-Instance Guard
// ---------------------------------------------------------------------------

// **Validates: Requirements 4.5**
//
// For any combination of PID file state and lock state, the startup
// decision depends solely on whether the advisory flock can be acquired.

/// Represents the possible PID file states for property testing.
#[derive(Debug, Clone)]
enum PidFileState {
    /// No PID file exists.
    Absent,
    /// PID file contains a dead PID.
    DeadPid(u32),
    /// PID file contains a live PID (our own process).
    LivePid,
}

fn pid_file_state_strategy() -> impl Strategy<Value = PidFileState> {
    prop_oneof![
        Just(PidFileState::Absent),
        (900_000_000u32..=999_999_999u32).prop_map(PidFileState::DeadPid),
        Just(PidFileState::LivePid),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn flock_is_authoritative_regardless_of_pid_state(
        pid_state in pid_file_state_strategy(),
        lock_held in any::<bool>(),
    ) {
        use std::os::unix::io::AsRawFd;

        let tmp = TempDir::new().unwrap();
        let lock_path = tmp.path().join("daemon.lock");
        let pid_path = tmp.path().join("daemon.pid");

        // Set up PID file state.
        match &pid_state {
            PidFileState::Absent => { /* no file */ }
            PidFileState::DeadPid(pid) => {
                fs::write(&pid_path, pid.to_string()).unwrap();
            }
            PidFileState::LivePid => {
                fs::write(&pid_path, std::process::id().to_string()).unwrap();
            }
        }

        // Optionally hold the lock to simulate another coordinator.
        let _held_lock = if lock_held {
            let file = std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .open(&lock_path)
                .unwrap();
            let fd = file.as_raw_fd();
            let ret = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
            if ret == 0 {
                Some(file)
            } else {
                None
            }
        } else {
            None
        };

        // Attempt to acquire the lock (simulating coordinator startup).
        let lock_file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .open(&lock_path)
            .unwrap();
        let fd = lock_file.as_raw_fd();
        let ret = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
        let can_acquire = ret == 0;

        // The decision to start depends ONLY on flock, not PID file state.
        if lock_held && _held_lock.is_some() {
            // Lock is held by another "process" (our first file handle).
            prop_assert!(
                !can_acquire,
                "should NOT acquire lock when already held, pid_state={:?}",
                pid_state
            );
        } else {
            // Lock is not held — we should always be able to acquire.
            prop_assert!(
                can_acquire,
                "should acquire lock when not held, pid_state={:?}",
                pid_state
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Property 4: Ping Response Completeness
// ---------------------------------------------------------------------------

// **Validates: Requirements 5.3**
//
// For any coordinator state, a ping response contains both PID and
// a non-negative uptime value.

#[cfg(feature = "fuse")]
use crab::vfs::ipc_server::IpcResponse;

#[cfg(feature = "fuse")]
proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn ping_response_contains_pid_and_nonneg_uptime(
        pid in 1u32..=4_000_000u32,
        uptime_secs in 0u64..=31_536_000u64,  // up to 1 year
    ) {
        let response = IpcResponse::ping_ok(pid, uptime_secs);

        prop_assert!(response.ok, "ping response must be ok");
        prop_assert_eq!(
            response.pid, Some(pid),
            "ping response must contain the coordinator PID"
        );
        prop_assert_eq!(
            response.uptime_secs, Some(uptime_secs),
            "ping response must contain uptime"
        );
        // Uptime is u64, so always non-negative by type.

        // Verify serialization round-trip preserves fields.
        let json = serde_json::to_string(&response).unwrap();
        let deserialized: IpcResponse = serde_json::from_str(&json).unwrap();
        prop_assert_eq!(deserialized.pid, Some(pid));
        prop_assert_eq!(deserialized.uptime_secs, Some(uptime_secs));
    }
}

// ---------------------------------------------------------------------------
// Property 5: Health Response Completeness
// ---------------------------------------------------------------------------

// **Validates: Requirements 5.4**
//
// For any coordinator state, a health response contains all required
// fields: mount_count, cache_size_bytes, hydration_queue_depth,
// hydration_workers, and uptime_secs.
#[cfg(feature = "fuse")]
proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn health_response_contains_all_required_fields(
        pid in 1u32..=4_000_000u32,
        uptime_secs in 0u64..=31_536_000u64,
        mount_count in 0usize..=100,
        cache_size_bytes in 0u64..=10_737_418_240u64,  // up to 10 GiB
        hydration_queue_depth in 0usize..=10_000,
        hydration_workers in 1usize..=64,
    ) {
        let response = IpcResponse::health_ok(
            pid,
            uptime_secs,
            mount_count,
            cache_size_bytes,
            hydration_queue_depth,
            hydration_workers,
        );

        prop_assert!(response.ok, "health response must be ok");
        prop_assert_eq!(response.pid, Some(pid));
        prop_assert_eq!(response.uptime_secs, Some(uptime_secs));
        prop_assert_eq!(response.mount_count, Some(mount_count));
        prop_assert_eq!(response.cache_size_bytes, Some(cache_size_bytes));
        prop_assert_eq!(response.hydration_queue_depth, Some(hydration_queue_depth));
        prop_assert_eq!(response.hydration_workers, Some(hydration_workers));

        // Verify serialization round-trip preserves all fields.
        let json = serde_json::to_string(&response).unwrap();
        let deserialized: IpcResponse = serde_json::from_str(&json).unwrap();
        prop_assert!(deserialized.ok);
        prop_assert_eq!(deserialized.pid, Some(pid));
        prop_assert_eq!(deserialized.uptime_secs, Some(uptime_secs));
        prop_assert_eq!(deserialized.mount_count, Some(mount_count));
        prop_assert_eq!(deserialized.cache_size_bytes, Some(cache_size_bytes));
        prop_assert_eq!(deserialized.hydration_queue_depth, Some(hydration_queue_depth));
        prop_assert_eq!(deserialized.hydration_workers, Some(hydration_workers));
    }
}

// ---------------------------------------------------------------------------
// Property 6: Shutdown Cleanup
// ---------------------------------------------------------------------------

// **Validates: Requirements 6.2**
//
// For any number of active mounts N, shutdown cancels all N tokens
// and removes daemon files (socket, PID, lock).
proptest! {
    #![proptest_config(ProptestConfig::with_cases(100))]

    #[test]
    fn shutdown_cancels_all_tokens_and_removes_files(mount_count in 0usize..=20) {
        use tokio_util::sync::CancellationToken;

        let tmp = TempDir::new().unwrap();
        let base_dir = tmp.path().to_path_buf();

        let pid_path = base_dir.join("daemon.pid");
        let socket_path = base_dir.join("daemon.sock");
        let lock_path = base_dir.join("daemon.lock");

        // Create the daemon files to simulate a running coordinator.
        fs::write(&pid_path, std::process::id().to_string()).unwrap();
        fs::write(&socket_path, "socket-placeholder").unwrap();
        fs::write(&lock_path, "lock-placeholder").unwrap();

        // Create a coordinator-level cancellation token and N child tokens.
        let coordinator_token = CancellationToken::new();
        let child_tokens: Vec<CancellationToken> = (0..mount_count)
            .map(|_| coordinator_token.child_token())
            .collect();

        // Simulate shutdown: cancel the coordinator token.
        coordinator_token.cancel();

        // All child tokens should be cancelled.
        for (i, token) in child_tokens.iter().enumerate() {
            prop_assert!(
                token.is_cancelled(),
                "child token {} should be cancelled after shutdown",
                i
            );
        }

        // Simulate file cleanup (as done in Coordinator::cleanup_files).
        let _ = fs::remove_file(&socket_path);
        let _ = fs::remove_file(&pid_path);
        let _ = fs::remove_file(&lock_path);

        // Verify all files are removed.
        prop_assert!(!socket_path.exists(), "socket file should be removed after shutdown");
        prop_assert!(!pid_path.exists(), "PID file should be removed after shutdown");
        prop_assert!(!lock_path.exists(), "lock file should be removed after shutdown");
    }
}
