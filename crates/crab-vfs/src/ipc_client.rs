//! IPC client for communicating with the mount coordinator.
//!
//! Connects to the coordinator's Unix socket at `~/.crab/mounts/daemon.sock`,
//! sends JSON requests (newline-delimited), and reads JSON responses.
//! Used by the CLI to delegate mount/unmount/status operations to the
//! coordinator process.

use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tracing::{debug, info, warn};

use crate::core::error::{CrabError, Result};
use crate::ipc_server::{IpcRequest, IpcResponse};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Maximum total time to wait for the coordinator to become available.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// Initial backoff delay between connection retries.
const INITIAL_BACKOFF: Duration = Duration::from_millis(100);

/// Backoff multiplier for each retry attempt.
const BACKOFF_MULTIPLIER: u32 = 2;

/// Timeout for reading a response from the coordinator.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(30);

/// FUSE unmount can block while the kernel drains outstanding requests.
const UNMOUNT_RESPONSE_TIMEOUT: Duration = Duration::from_mins(2);

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// IPC client errors.
#[derive(thiserror::Error, Debug)]
pub enum IpcClientError {
    #[error("failed to connect to coordinator at {path}: {source}")]
    ConnectionFailed {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("coordinator not available after {elapsed:?} (socket: {path})")]
    CoordinatorUnavailable { path: PathBuf, elapsed: Duration },

    #[error("failed to spawn coordinator process: {0}")]
    SpawnFailed(String),

    #[error("failed to send request: {0}")]
    SendFailed(#[source] std::io::Error),

    #[error("failed to read response: {0}")]
    ReadFailed(#[source] std::io::Error),

    #[error("response timeout after {0:?}")]
    ResponseTimeout(Duration),

    #[error("failed to serialize request: {0}")]
    SerializeFailed(#[source] serde_json::Error),

    #[error("failed to parse response: {0}")]
    ParseFailed(#[source] serde_json::Error),

    #[error("coordinator returned error: {0}")]
    OperationFailed(String),
}

impl From<IpcClientError> for CrabError {
    fn from(e: IpcClientError) -> Self {
        CrabError::Internal(e.to_string())
    }
}

// ---------------------------------------------------------------------------
// IpcClient
// ---------------------------------------------------------------------------

/// Client for communicating with the mount coordinator via Unix socket IPC.
///
/// Connects to the coordinator's socket, sends newline-delimited JSON
/// requests, and reads JSON responses. Each client holds a single
/// connection that can be reused for multiple request/response cycles.
pub struct IpcClient {
    reader: tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    writer: tokio::net::unix::OwnedWriteHalf,
}

impl std::fmt::Debug for IpcClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IpcClient").finish_non_exhaustive()
    }
}

impl IpcClient {
    /// Connect to the coordinator socket at the given path.
    ///
    /// Returns an error immediately if the connection is refused (coordinator
    /// not running).
    pub async fn connect(socket_path: &Path) -> std::result::Result<Self, IpcClientError> {
        debug!(path = %socket_path.display(), "connecting to coordinator socket");

        let stream = UnixStream::connect(socket_path).await.map_err(|e| {
            IpcClientError::ConnectionFailed {
                path: socket_path.to_path_buf(),
                source: e,
            }
        })?;

        let (reader, writer) = stream.into_split();
        let lines = BufReader::new(reader).lines();

        debug!("connected to coordinator");
        Ok(Self {
            reader: lines,
            writer,
        })
    }

    /// Connect to the coordinator, spawning it if not already running.
    ///
    /// Attempts to connect to the socket. If the connection is refused,
    /// removes any stale socket file, spawns the coordinator process, and
    /// retries with exponential backoff (100ms, 200ms, 400ms, 800ms, 1600ms)
    /// up to 5 seconds total.
    pub async fn connect_or_spawn(socket_path: &Path) -> std::result::Result<Self, IpcClientError> {
        // First attempt: try to connect directly.
        match Self::connect(socket_path).await {
            Ok(client) => return Ok(client),
            Err(IpcClientError::ConnectionFailed { .. }) => {
                debug!("coordinator not running, spawning");
            }
            Err(e) => return Err(e),
        }

        // Remove stale socket file if it exists — the connection was refused,
        // so no coordinator is listening on it. Only remove actual sockets
        // or empty files (stale socket artifacts), not regular files/dirs.
        if socket_path.exists()
            && let Ok(meta) = std::fs::metadata(socket_path)
        {
            let ft = meta.file_type();
            if ft.is_socket() || (ft.is_file() && meta.len() == 0) {
                debug!(path = %socket_path.display(), "removing stale socket before spawning coordinator");
                let _ = std::fs::remove_file(socket_path);
            } else {
                warn!(path = %socket_path.display(), "socket path exists but is not a socket; refusing to remove");
            }
        }

        // Spawn the coordinator process.
        spawn_coordinator()?;

        // Retry with exponential backoff.
        let mut delay = INITIAL_BACKOFF;
        let start = tokio::time::Instant::now();

        let mut attempt = 0usize;
        while start.elapsed() < CONNECT_TIMEOUT {
            let remaining = CONNECT_TIMEOUT.saturating_sub(start.elapsed());
            tokio::time::sleep(delay.min(remaining)).await;
            attempt += 1;

            match Self::connect(socket_path).await {
                Ok(client) => {
                    info!(
                        attempts = attempt,
                        elapsed = ?start.elapsed(),
                        "connected to coordinator after spawn"
                    );
                    return Ok(client);
                }
                Err(IpcClientError::ConnectionFailed { .. }) => {
                    debug!(
                        attempt,
                        delay_ms = delay.as_millis(),
                        "coordinator not ready yet, retrying"
                    );
                    delay *= BACKOFF_MULTIPLIER;
                }
                Err(e) => return Err(e),
            }
        }

        Err(IpcClientError::CoordinatorUnavailable {
            path: socket_path.to_path_buf(),
            elapsed: start.elapsed(),
        })
    }

    /// Send a request and wait for the response.
    ///
    /// Serializes the request as JSON + newline, writes it to the socket,
    /// then reads and parses the response line.
    pub async fn send(
        &mut self,
        request: &IpcRequest,
    ) -> std::result::Result<IpcResponse, IpcClientError> {
        self.send_with_timeout(request, RESPONSE_TIMEOUT).await
    }

    /// Send a request and wait for the response using an operation-specific timeout.
    pub async fn send_with_timeout(
        &mut self,
        request: &IpcRequest,
        timeout: Duration,
    ) -> std::result::Result<IpcResponse, IpcClientError> {
        // Serialize request.
        let mut json = serde_json::to_string(request).map_err(IpcClientError::SerializeFailed)?;
        json.push('\n');

        // Write to socket.
        self.writer
            .write_all(json.as_bytes())
            .await
            .map_err(IpcClientError::SendFailed)?;
        self.writer
            .flush()
            .await
            .map_err(IpcClientError::SendFailed)?;

        debug!(op = ?request, "sent IPC request");

        // Read response with timeout.
        let response_line = tokio::time::timeout(timeout, self.reader.next_line())
            .await
            .map_err(|_| IpcClientError::ResponseTimeout(timeout))?
            .map_err(IpcClientError::ReadFailed)?
            .ok_or_else(|| {
                IpcClientError::ReadFailed(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "coordinator closed connection",
                ))
            })?;

        let response: IpcResponse =
            serde_json::from_str(&response_line).map_err(IpcClientError::ParseFailed)?;

        debug!(ok = response.ok, "received IPC response");
        Ok(response)
    }
}

// ---------------------------------------------------------------------------
// Coordinator spawning
// ---------------------------------------------------------------------------

/// Spawn the coordinator as a background process.
///
/// Runs `crab coordinator start` (or the equivalent internal mechanism)
/// as a detached child process. The coordinator will bind the Unix socket
/// once it's ready.
fn spawn_coordinator() -> std::result::Result<(), IpcClientError> {
    use std::process::Command;

    let crab_bin = crate::executable::crab_binary_path();

    debug!(bin = %crab_bin, "spawning coordinator process");

    // Spawn the coordinator as a detached background process.
    // The coordinator will:
    // 1. Acquire the daemon lock
    // 2. Bind the Unix socket
    // 3. Write its PID file
    // 4. Enter the accept loop
    let child = Command::new(&crab_bin)
        .args(["coordinator", "start"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| {
            IpcClientError::SpawnFailed(format!("failed to spawn coordinator at {crab_bin}: {e}"))
        })?;

    info!(pid = child.id(), "spawned coordinator process");
    Ok(())
}

// ---------------------------------------------------------------------------
// Default socket path
// ---------------------------------------------------------------------------

/// Get the default socket path for the coordinator.
///
/// Returns `~/.crab/mounts/daemon.sock`.
pub fn default_socket_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").map_err(|_| CrabError::Configuration {
        key: "HOME environment variable not set".into(),
        origin: "ipc_client".into(),
    })?;
    Ok(PathBuf::from(home)
        .join(".crab")
        .join("mounts")
        .join("daemon.sock"))
}

// ---------------------------------------------------------------------------
// CLI helper functions
// ---------------------------------------------------------------------------

/// Send a mount request to the coordinator via IPC.
///
/// Used by `crab mount` in background mode. Connects to the coordinator
/// (spawning it if needed), sends the mount request, and prints the result.
pub async fn try_ipc_mount(
    repo: &str,
    mountpoint: &str,
    git_ref: &str,
    read_only: bool,
    no_refresh: bool,
) -> Result<()> {
    let socket_path = default_socket_path()?;
    let mut client = IpcClient::connect_or_spawn(&socket_path)
        .await
        .map_err(CrabError::from)?;

    let request = IpcRequest::Mount {
        remote: repo.to_owned(),
        mountpoint: mountpoint.to_owned(),
        r#ref: git_ref.to_owned(),
        read_only,
        no_refresh,
    };

    let response = client.send(&request).await.map_err(CrabError::from)?;

    if response.ok {
        let mp = response.mountpoint.as_deref().unwrap_or(mountpoint);
        println!("Mounted at {mp}");
        if let Some(pid) = response.pid {
            println!("  PID: {pid}");
        }
        if let Some(ref oid) = response.head_oid {
            println!("  HEAD: {oid}");
        }
        println!("Use `crab unmount --mountpoint {mp}` to stop.");
        Ok(())
    } else {
        let msg = response.error.unwrap_or_else(|| "unknown error".to_owned());
        Err(CrabError::Internal(format!("mount failed: {msg}")))
    }
}

/// Send an unmount request to the coordinator via IPC.
///
/// Used by `crab unmount` when the coordinator is running.
pub async fn try_ipc_unmount(mountpoint: &str) -> Result<()> {
    let socket_path = default_socket_path()?;
    let mut client = IpcClient::connect(&socket_path)
        .await
        .map_err(CrabError::from)?;

    let request = IpcRequest::Unmount {
        mountpoint: mountpoint.to_owned(),
    };

    let response = client
        .send_with_timeout(&request, UNMOUNT_RESPONSE_TIMEOUT)
        .await
        .map_err(CrabError::from)?;

    if response.ok {
        println!("Unmounted {mountpoint}.");
        Ok(())
    } else {
        let msg = response.error.unwrap_or_else(|| "unknown error".to_owned());
        Err(CrabError::Internal(format!("unmount failed: {msg}")))
    }
}

/// Send a list request to the coordinator via IPC.
///
/// Used by `crab mount list` when the coordinator is running.
/// If `json` is true, outputs raw JSON; otherwise prints a table.
pub async fn try_ipc_list(json: bool) -> Result<()> {
    let socket_path = default_socket_path()?;
    let mut client = IpcClient::connect(&socket_path)
        .await
        .map_err(CrabError::from)?;

    let request = IpcRequest::List;
    let response = client.send(&request).await.map_err(CrabError::from)?;

    if response.ok {
        if let Some(mounts) = response.mounts {
            if json {
                let output = serde_json::to_string_pretty(&mounts)
                    .map_err(|e| CrabError::Internal(format!("failed to serialize mounts: {e}")))?;
                println!("{output}");
            } else if mounts.is_empty() {
                println!("No active mounts.");
            } else {
                println!(
                    "{:<30} {:<40} {:<15} READ-ONLY",
                    "MOUNTPOINT", "REMOTE", "REF"
                );
                for m in &mounts {
                    println!(
                        "{:<30} {:<40} {:<15} {}",
                        m.mountpoint, m.remote, m.r#ref, m.read_only
                    );
                }
            }
        }
        Ok(())
    } else {
        let msg = response.error.unwrap_or_else(|| "unknown error".to_owned());
        Err(CrabError::Internal(format!("list failed: {msg}")))
    }
}

/// Send a refresh request to the coordinator via IPC.
///
/// Used by `crab mount refresh` to trigger an immediate fetch + snapshot rebuild.
pub async fn try_ipc_refresh(mountpoint: &str) -> Result<()> {
    let socket_path = default_socket_path()?;
    let mut client = IpcClient::connect(&socket_path)
        .await
        .map_err(CrabError::from)?;

    let request = IpcRequest::Refresh {
        mountpoint: mountpoint.to_owned(),
    };

    let response = client.send(&request).await.map_err(CrabError::from)?;

    if response.ok {
        println!("Refreshed mount at {mountpoint}.");
        if let Some(ref oid) = response.head_oid {
            println!("  New HEAD: {oid}");
        }
        Ok(())
    } else {
        let msg = response.error.unwrap_or_else(|| "unknown error".to_owned());
        Err(CrabError::Internal(format!("refresh failed: {msg}")))
    }
}

/// Send a switch_ref request to the coordinator via IPC.
///
/// Used by `crab mount switch` to change the tracked branch.
pub async fn try_ipc_switch(mountpoint: &str, git_ref: &str) -> Result<()> {
    let socket_path = default_socket_path()?;
    let mut client = IpcClient::connect(&socket_path)
        .await
        .map_err(CrabError::from)?;

    let request = IpcRequest::SwitchRef {
        mountpoint: mountpoint.to_owned(),
        r#ref: git_ref.to_owned(),
    };

    let response = client.send(&request).await.map_err(CrabError::from)?;

    if response.ok {
        println!("Switched mount at {mountpoint} to ref '{git_ref}'.");
        if let Some(ref oid) = response.head_oid {
            println!("  New HEAD: {oid}");
        }
        Ok(())
    } else {
        let msg = response.error.unwrap_or_else(|| "unknown error".to_owned());
        Err(CrabError::Internal(format!("switch failed: {msg}")))
    }
}

/// Ask the coordinator to invalidate cached FUSE entries for changed paths.
pub async fn try_ipc_invalidate(mountpoint: &str, paths: Vec<String>) -> Result<()> {
    let socket_path = default_socket_path()?;
    let mut client = IpcClient::connect(&socket_path)
        .await
        .map_err(CrabError::from)?;

    let request = IpcRequest::Invalidate {
        mountpoint: mountpoint.to_owned(),
        paths,
    };
    let response = client.send(&request).await.map_err(CrabError::from)?;
    if response.ok {
        return Ok(());
    }
    let msg = response.error.unwrap_or_else(|| "unknown error".to_owned());
    Err(CrabError::Internal(format!("invalidate failed: {msg}")))
}

/// Ask the coordinator to inspect the live writable overlay for a mounted path.
pub async fn try_ipc_overlay_diff(mountpoint: &str) -> Result<crate::publish::OverlayDiff> {
    let socket_path = default_socket_path()?;
    let mut client = IpcClient::connect(&socket_path)
        .await
        .map_err(CrabError::from)?;

    let request = IpcRequest::DiffOverlay {
        mountpoint: mountpoint.to_owned(),
    };
    let response = client.send(&request).await.map_err(CrabError::from)?;
    if response.ok {
        return response
            .overlay_diff
            .ok_or_else(|| CrabError::Internal("diff response missing overlay diff".into()));
    }
    let msg = response.error.unwrap_or_else(|| "unknown error".to_owned());
    Err(CrabError::Internal(format!("diff failed: {msg}")))
}

/// Ask the coordinator to reset the live writable overlay for a mounted path.
pub async fn try_ipc_reset_overlay(mountpoint: &str) -> Result<crate::publish::OverlayDiff> {
    let socket_path = default_socket_path()?;
    let mut client = IpcClient::connect(&socket_path)
        .await
        .map_err(CrabError::from)?;

    let request = IpcRequest::ResetOverlay {
        mountpoint: mountpoint.to_owned(),
    };
    let response = client.send(&request).await.map_err(CrabError::from)?;
    if response.ok {
        return response
            .overlay_diff
            .ok_or_else(|| CrabError::Internal("reset response missing overlay diff".into()));
    }
    let msg = response.error.unwrap_or_else(|| "unknown error".to_owned());
    Err(CrabError::Internal(format!("reset failed: {msg}")))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;
    use crate::ipc_server::{IpcRequest, IpcResponse};

    #[test]
    fn serialize_mount_request_for_ipc() {
        let req = IpcRequest::Mount {
            remote: "crab://bucket/repo".to_owned(),
            mountpoint: "/mnt/view".to_owned(),
            r#ref: "main".to_owned(),
            read_only: false,
            no_refresh: true,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""op":"mount""#));
        assert!(json.contains(r#""remote":"crab://bucket/repo""#));
        assert!(json.contains(r#""no_refresh":true"#));

        // Verify it can be deserialized back.
        let parsed: IpcRequest = serde_json::from_str(&json).unwrap();
        let reserialized = serde_json::to_string(&parsed).unwrap();
        assert_eq!(json, reserialized);
    }

    #[test]
    fn serialize_unmount_request_for_ipc() {
        let req = IpcRequest::Unmount {
            mountpoint: "/mnt/view".to_owned(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""op":"unmount""#));
        assert!(json.contains(r#""mountpoint":"/mnt/view""#));
    }

    #[test]
    fn serialize_list_request_for_ipc() {
        let req = IpcRequest::List;
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""op":"list""#));
    }

    #[test]
    fn serialize_status_request_for_ipc() {
        let req = IpcRequest::Status {
            mountpoint: "/mnt/view".to_owned(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""op":"status""#));
    }

    #[test]
    fn serialize_refresh_request_for_ipc() {
        let req = IpcRequest::Refresh {
            mountpoint: "/mnt/view".to_owned(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""op":"refresh""#));
    }

    #[test]
    fn serialize_switch_ref_request_for_ipc() {
        let req = IpcRequest::SwitchRef {
            mountpoint: "/mnt/view".to_owned(),
            r#ref: "feature-branch".to_owned(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""op":"switch_ref""#));
        assert!(json.contains(r#""ref":"feature-branch""#));
    }

    #[test]
    fn serialize_invalidate_request_for_ipc() {
        let req = IpcRequest::Invalidate {
            mountpoint: "/mnt/view".to_owned(),
            paths: vec!["models/model.bin".to_owned()],
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""op":"invalidate""#));
        assert!(json.contains(r#""paths":["models/model.bin"]"#));
    }

    #[test]
    fn serialize_diff_overlay_request_for_ipc() {
        let req = IpcRequest::DiffOverlay {
            mountpoint: "/mnt/view".to_owned(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""op":"diff_overlay""#));
        assert!(json.contains(r#""mountpoint":"/mnt/view""#));
    }

    #[test]
    fn serialize_reset_overlay_request_for_ipc() {
        let req = IpcRequest::ResetOverlay {
            mountpoint: "/mnt/view".to_owned(),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""op":"reset_overlay""#));
        assert!(json.contains(r#""mountpoint":"/mnt/view""#));
    }

    #[test]
    fn serialize_commit_overlay_request_for_ipc() {
        let req = IpcRequest::CommitOverlay {
            mountpoint: "/mnt/view".to_owned(),
            message: "mounted commit".to_owned(),
            push: true,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains(r#""op":"commit_overlay""#));
        assert!(json.contains(r#""message":"mounted commit""#));
        assert!(json.contains(r#""push":true"#));
    }

    #[test]
    fn parse_success_response() {
        let json = r#"{"ok":true}"#;
        let resp: IpcResponse = serde_json::from_str(json).unwrap();
        assert!(resp.ok);
        assert!(resp.error.is_none());
    }

    #[test]
    fn parse_error_response() {
        let json = r#"{"ok":false,"error":"mount not found"}"#;
        let resp: IpcResponse = serde_json::from_str(json).unwrap();
        assert!(!resp.ok);
        assert_eq!(resp.error.as_deref(), Some("mount not found"));
    }

    #[test]
    fn parse_mount_response() {
        let json = r#"{"ok":true,"mountpoint":"/mnt/view","pid":12345,"head_oid":"abc123"}"#;
        let resp: IpcResponse = serde_json::from_str(json).unwrap();
        assert!(resp.ok);
        assert_eq!(resp.mountpoint.as_deref(), Some("/mnt/view"));
        assert_eq!(resp.pid, Some(12345));
        assert_eq!(resp.head_oid.as_deref(), Some("abc123"));
    }

    #[test]
    fn parse_list_response() {
        let json = r#"{"ok":true,"mounts":[{"mountpoint":"/mnt/a","remote":"crab://b/r","ref":"main","read_only":false}]}"#;
        let resp: IpcResponse = serde_json::from_str(json).unwrap();
        assert!(resp.ok);
        let mounts = resp.mounts.unwrap();
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].mountpoint, "/mnt/a");
        assert_eq!(mounts[0].remote, "crab://b/r");
    }

    #[test]
    fn parse_status_response() {
        let json = r#"{"ok":true,"status":{"mountpoint":"/mnt/a","remote":"crab://b/r","ref":"main","read_only":false,"head_oid":"def456","pid":12345}}"#;
        let resp: IpcResponse = serde_json::from_str(json).unwrap();
        assert!(resp.ok);
        let status = resp.status.unwrap();
        assert_eq!(status.mountpoint, "/mnt/a");
        assert_eq!(status.head_oid.as_deref(), Some("def456"));
        assert_eq!(status.pid, Some(12345));
    }

    #[test]
    fn default_socket_path_contains_daemon_sock() {
        // This test may fail if HOME is not set, but that's fine for CI.
        if std::env::var("HOME").is_ok() {
            let path = default_socket_path().unwrap();
            assert!(path.ends_with("daemon.sock"));
            assert!(path.to_string_lossy().contains(".crab/mounts"));
        }
    }

    #[test]
    fn ipc_client_error_display() {
        let err = IpcClientError::CoordinatorUnavailable {
            path: PathBuf::from("/tmp/daemon.sock"),
            elapsed: Duration::from_secs(5),
        };
        let msg = err.to_string();
        assert!(msg.contains("coordinator not available"));
        assert!(msg.contains("/tmp/daemon.sock"));
    }

    #[test]
    fn ipc_client_error_converts_to_crab_error() {
        let err = IpcClientError::OperationFailed("test error".to_owned());
        let crab_err: CrabError = err.into();
        let msg = crab_err.to_string();
        assert!(msg.contains("test error"));
    }

    #[tokio::test]
    async fn client_connect_fails_on_missing_socket() {
        let tmp = tempfile::tempdir().unwrap();
        let socket_path = tmp.path().join("nonexistent.sock");

        let result = IpcClient::connect(&socket_path).await;
        assert!(result.is_err());

        match result.unwrap_err() {
            IpcClientError::ConnectionFailed { path, .. } => {
                assert_eq!(path, socket_path);
            }
            other => panic!("expected ConnectionFailed, got: {other:?}"),
        }
    }

    #[tokio::test]
    async fn client_send_and_receive() {
        use std::sync::Arc;
        use tokio::sync::Mutex;

        let tmp = tempfile::tempdir().unwrap();
        let socket_path = tmp.path().join("test.sock");

        // Set up a minimal coordinator for the IPC server.
        let config = crate::coordinator::CoordinatorConfig::with_base_dir(tmp.path().to_path_buf());
        let coordinator = crate::coordinator::Coordinator::start(config).unwrap();
        let cancel_token = coordinator.cancel_token().clone();
        let coordinator = Arc::new(Mutex::new(coordinator));

        let server = crate::ipc_server::IpcServer::new(
            Arc::clone(&coordinator),
            socket_path.clone(),
            cancel_token.clone(),
        );

        // Spawn the server.
        let _server_handle = tokio::spawn(async move { server.run().await });

        // Give the server time to bind.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Connect as a client.
        let mut client = IpcClient::connect(&socket_path).await.unwrap();

        // Send a list request.
        let response = client.send(&IpcRequest::List).await.unwrap();
        assert!(response.ok);
        assert!(response.mounts.is_some());
        assert!(response.mounts.unwrap().is_empty());

        // Send a status request for a non-existent mount.
        let response = client
            .send(&IpcRequest::Status {
                mountpoint: "/mnt/nonexistent".to_owned(),
            })
            .await
            .unwrap();
        assert!(!response.ok);
        assert!(response.error.unwrap().contains("mount not found"));

        // Clean up.
        cancel_token.cancel();
    }

    #[tokio::test]
    async fn connect_or_spawn_removes_stale_socket_file() {
        let tmp = tempfile::tempdir().unwrap();
        let socket_path = tmp.path().join("daemon.sock");

        // Create an empty stale socket artifact (socket files may appear as
        // zero-byte regular files on some platforms after crashes).
        std::fs::write(&socket_path, "").unwrap();
        assert!(socket_path.exists());

        // connect_or_spawn will fail to connect (not a real socket), then
        // remove the stale empty file before attempting to spawn. The spawn
        // will fail in test (no coordinator binary), but the file should be
        // gone.
        let result = IpcClient::connect_or_spawn(&socket_path).await;
        assert!(result.is_err());

        // The stale socket file should have been removed.
        assert!(
            !socket_path.exists(),
            "stale empty socket artifact should be removed"
        );
    }

    #[tokio::test]
    async fn connect_or_spawn_refuses_to_remove_regular_file() {
        let tmp = tempfile::tempdir().unwrap();
        let socket_path = tmp.path().join("daemon.sock");

        // Create a regular file with content (not a stale socket artifact).
        std::fs::write(&socket_path, "important-data").unwrap();
        assert!(socket_path.exists());

        // connect_or_spawn should refuse to remove a regular file with content
        // and preserve it.
        let result = IpcClient::connect_or_spawn(&socket_path).await;
        assert!(result.is_err());

        // The regular file should still exist.
        assert!(
            socket_path.exists(),
            "regular file at socket path should not be removed"
        );
    }
}
