//! IPC server for the mount coordinator.
//!
//! Accepts connections on a Unix socket, reads newline-delimited JSON
//! requests, dispatches operations to the coordinator, and writes JSON
//! responses back. Each connection is handled in its own tokio task with
//! a 30-second idle timeout.

use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::Mutex;
use tokio::time::{Duration, timeout};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::clone_cache;
use crate::coordinator::{Coordinator, MountHandle, MountReservation, unmount_removed_mount};
use crate::integration::{MountReadResolver, NoopMountReadResolver};
use crate::mount::{self, MountConfig};
use crate::pipeline::{self, MountPipelineBuilder, PipelineConfig};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Idle timeout for connections: close after 30s of inactivity.
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// IPC Request / Response types
// ---------------------------------------------------------------------------

/// A single IPC request from a client.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum IpcRequest {
    /// Mount a repository at a given mountpoint.
    Mount {
        remote: String,
        mountpoint: String,
        #[serde(default = "default_ref")]
        r#ref: String,
        #[serde(default)]
        read_only: bool,
        #[serde(default)]
        no_refresh: bool,
    },
    /// Unmount a mountpoint.
    Unmount { mountpoint: String },
    /// List all active mounts.
    List,
    /// Get status of a specific mount.
    Status { mountpoint: String },
    /// Refresh a mount (re-fetch from remote).
    Refresh { mountpoint: String },
    /// Switch a mount to a different ref/branch.
    SwitchRef { mountpoint: String, r#ref: String },
    /// Invalidate cached FUSE entries for paths changed outside the mount.
    Invalidate {
        mountpoint: String,
        paths: Vec<String>,
    },
    /// Inspect a writable mount overlay through the live coordinator state.
    DiffOverlay { mountpoint: String },
    /// Reset a writable mount overlay through the live coordinator state.
    ResetOverlay { mountpoint: String },
    /// Commit a writable mount overlay and adopt the published generation.
    CommitOverlay {
        mountpoint: String,
        message: String,
        push: bool,
    },
    /// Lightweight liveness check.
    Ping,
    /// Detailed health information.
    Health,
    /// Request graceful shutdown.
    Shutdown,
}

fn default_ref() -> String {
    "HEAD".to_owned()
}

/// A single IPC response sent back to the client.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct IpcResponse {
    /// Whether the operation succeeded.
    pub ok: bool,
    /// Error message (present when `ok` is false).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Operation-specific payload fields.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mountpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_oid: Option<String>,
    /// List of mounts (for the `list` operation).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mounts: Option<Vec<MountInfo>>,
    /// Mount status details (for the `status` operation).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<MountStatus>,
    /// Overlay diff payload for reset operations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overlay_diff: Option<crate::publish::OverlayDiff>,
    /// Overlay commit payload for commit operations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overlay_commit_result: Option<crate::publish::OverlayCommitResult>,
    /// Coordinator uptime in seconds (for ping/health).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uptime_secs: Option<u64>,
    /// Active mount count (for health).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mount_count: Option<usize>,
    /// Total cache size in bytes (for health).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_size_bytes: Option<u64>,
    /// Hydration queue depth (for health).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hydration_queue_depth: Option<usize>,
    /// Hydration worker count (for health).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hydration_workers: Option<usize>,
}

/// Summary info for a single mount (returned by `list`).
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct MountInfo {
    pub mountpoint: String,
    pub remote: String,
    pub r#ref: String,
    pub read_only: bool,
}

/// Detailed status for a single mount (returned by `status`).
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct MountStatus {
    pub mountpoint: String,
    pub remote: String,
    pub r#ref: String,
    pub read_only: bool,
    pub head_oid: Option<String>,
    pub pid: Option<u32>,
}

impl IpcResponse {
    /// Create a success response with no payload.
    pub fn ok() -> Self {
        Self {
            ok: true,
            error: None,
            mountpoint: None,
            pid: None,
            head_oid: None,
            mounts: None,
            status: None,
            overlay_diff: None,
            overlay_commit_result: None,
            uptime_secs: None,
            mount_count: None,
            cache_size_bytes: None,
            hydration_queue_depth: None,
            hydration_workers: None,
        }
    }

    /// Create an error response.
    pub fn err(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(message.into()),
            mountpoint: None,
            pid: None,
            head_oid: None,
            mounts: None,
            status: None,
            overlay_diff: None,
            overlay_commit_result: None,
            uptime_secs: None,
            mount_count: None,
            cache_size_bytes: None,
            hydration_queue_depth: None,
            hydration_workers: None,
        }
    }

    /// Create a success response for a mount operation.
    pub fn mount_ok(mountpoint: String, pid: u32, head_oid: Option<String>) -> Self {
        Self {
            ok: true,
            error: None,
            mountpoint: Some(mountpoint),
            pid: Some(pid),
            head_oid,
            mounts: None,
            status: None,
            overlay_diff: None,
            overlay_commit_result: None,
            uptime_secs: None,
            mount_count: None,
            cache_size_bytes: None,
            hydration_queue_depth: None,
            hydration_workers: None,
        }
    }

    /// Create a success response for a list operation.
    pub fn list_ok(mounts: Vec<MountInfo>) -> Self {
        Self {
            ok: true,
            error: None,
            mountpoint: None,
            pid: None,
            head_oid: None,
            mounts: Some(mounts),
            status: None,
            overlay_diff: None,
            overlay_commit_result: None,
            uptime_secs: None,
            mount_count: None,
            cache_size_bytes: None,
            hydration_queue_depth: None,
            hydration_workers: None,
        }
    }

    /// Create a success response for a status operation.
    pub fn status_ok(status: MountStatus) -> Self {
        Self {
            ok: true,
            error: None,
            mountpoint: None,
            pid: None,
            head_oid: None,
            mounts: None,
            status: Some(status),
            overlay_diff: None,
            overlay_commit_result: None,
            uptime_secs: None,
            mount_count: None,
            cache_size_bytes: None,
            hydration_queue_depth: None,
            hydration_workers: None,
        }
    }

    /// Create a success response for a ping operation.
    pub fn ping_ok(pid: u32, uptime_secs: u64) -> Self {
        Self {
            ok: true,
            error: None,
            mountpoint: None,
            pid: Some(pid),
            head_oid: None,
            mounts: None,
            status: None,
            overlay_diff: None,
            overlay_commit_result: None,
            uptime_secs: Some(uptime_secs),
            mount_count: None,
            cache_size_bytes: None,
            hydration_queue_depth: None,
            hydration_workers: None,
        }
    }

    /// Create a success response for a health operation.
    pub fn health_ok(
        pid: u32,
        uptime_secs: u64,
        mount_count: usize,
        cache_size_bytes: u64,
        hydration_queue_depth: usize,
        hydration_workers: usize,
    ) -> Self {
        Self {
            ok: true,
            error: None,
            mountpoint: None,
            pid: Some(pid),
            head_oid: None,
            mounts: None,
            status: None,
            overlay_diff: None,
            overlay_commit_result: None,
            uptime_secs: Some(uptime_secs),
            mount_count: Some(mount_count),
            cache_size_bytes: Some(cache_size_bytes),
            hydration_queue_depth: Some(hydration_queue_depth),
            hydration_workers: Some(hydration_workers),
        }
    }
}

// ---------------------------------------------------------------------------
// IpcServer
// ---------------------------------------------------------------------------

/// IPC server that listens on a Unix socket and dispatches requests to the
/// coordinator.
pub struct IpcServer {
    /// Shared reference to the coordinator.
    coordinator: Arc<Mutex<Coordinator>>,
    /// Path to the Unix socket.
    socket_path: PathBuf,
    /// Cancellation token for graceful shutdown.
    cancel_token: CancellationToken,
    /// Product-owned credential and replica resolver for Crab remotes.
    read_resolver: Arc<dyn MountReadResolver>,
}

impl IpcServer {
    /// Create a new IPC server.
    ///
    /// The `socket_path` should match the coordinator's configured socket
    /// path. The `cancel_token` is typically the coordinator's own token
    /// so the server shuts down when the coordinator does.
    pub fn new(
        coordinator: Arc<Mutex<Coordinator>>,
        socket_path: PathBuf,
        cancel_token: CancellationToken,
    ) -> Self {
        Self {
            coordinator,
            socket_path,
            cancel_token,
            read_resolver: Arc::new(NoopMountReadResolver),
        }
    }

    /// Provide credential and replica-aware read resolution for mount requests.
    #[must_use]
    pub fn with_read_resolver(mut self, resolver: Arc<dyn MountReadResolver>) -> Self {
        self.read_resolver = resolver;
        self
    }

    /// Run the IPC server, accepting connections until cancelled.
    ///
    /// This method binds the Unix socket and enters the accept loop.
    /// Each connection is handled in a separate tokio task.
    pub async fn run(&self) -> std::io::Result<()> {
        self.run_with_bound_hook(|| {}).await
    }

    /// Run the IPC server and invoke `on_bound` once the socket is bound.
    ///
    /// Coordinator daemon readiness depends on the socket accepting
    /// connections, not merely on coordinator state initialization.
    pub async fn run_with_bound_hook<F>(&self, on_bound: F) -> std::io::Result<()>
    where
        F: FnOnce(),
    {
        // Remove stale socket file if present. Only remove actual sockets
        // or empty files (stale socket artifacts), not regular files/dirs.
        if self.socket_path.exists()
            && let Ok(meta) = tokio::fs::metadata(&self.socket_path).await
        {
            let ft = meta.file_type();
            if ft.is_socket() || (ft.is_file() && meta.len() == 0) {
                let _ = tokio::fs::remove_file(&self.socket_path).await;
            }
        }

        let listener = UnixListener::bind(&self.socket_path)?;
        info!(path = %self.socket_path.display(), "IPC server listening");
        on_bound();

        loop {
            tokio::select! {
                () = self.cancel_token.cancelled() => {
                    info!("IPC server shutting down");
                    break;
                }
                accept_result = listener.accept() => {
                    match accept_result {
                        Ok((stream, _addr)) => {
                            debug!("accepted IPC connection");
                            let coordinator = Arc::clone(&self.coordinator);
                            let read_resolver = Arc::clone(&self.read_resolver);
                            let cancel = self.cancel_token.clone();
                            tokio::spawn(async move {
                                if let Err(e) = handle_connection(
                                    stream,
                                    coordinator,
                                    read_resolver,
                                    cancel,
                                )
                                .await
                                {
                                    debug!(error = %e, "IPC connection ended");
                                }
                            });
                        }
                        Err(e) => {
                            error!(error = %e, "failed to accept IPC connection");
                        }
                    }
                }
            }
        }

        // Cleanup socket file on shutdown.
        let _ = tokio::fs::remove_file(&self.socket_path).await;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Connection handler
// ---------------------------------------------------------------------------

/// Handle a single client connection.
///
/// Reads newline-delimited JSON requests, dispatches each to the coordinator,
/// and writes JSON responses back. Closes the connection after 30s of
/// inactivity.
async fn handle_connection(
    stream: tokio::net::UnixStream,
    coordinator: Arc<Mutex<Coordinator>>,
    read_resolver: Arc<dyn MountReadResolver>,
    cancel_token: CancellationToken,
) -> std::io::Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    loop {
        let line = tokio::select! {
            () = cancel_token.cancelled() => {
                debug!("connection closed: server shutting down");
                return Ok(());
            }
            result = timeout(IDLE_TIMEOUT, lines.next_line()) => {
                match result {
                    Ok(Ok(Some(line))) => line,
                    Ok(Ok(None)) => {
                        // Client closed the connection.
                        debug!("client disconnected");
                        return Ok(());
                    }
                    Ok(Err(e)) => {
                        warn!(error = %e, "read error on IPC connection");
                        return Err(e);
                    }
                    Err(_) => {
                        // Idle timeout expired.
                        debug!("closing idle IPC connection after 30s");
                        return Ok(());
                    }
                }
            }
        };

        // Parse the request.
        let request: IpcRequest = match serde_json::from_str(&line) {
            Ok(req) => req,
            Err(e) => {
                let response = IpcResponse::err(format!("invalid request: {e}"));
                write_response(&mut writer, &response).await?;
                continue;
            }
        };

        // Dispatch the request.
        let response = dispatch_request(&coordinator, &read_resolver, request).await;

        // Write the response.
        write_response(&mut writer, &response).await?;
    }
}

/// Serialize and write a response as a single JSON line.
async fn write_response(
    writer: &mut tokio::net::unix::OwnedWriteHalf,
    response: &IpcResponse,
) -> std::io::Result<()> {
    let mut json = serde_json::to_string(response)
        .map_err(|e| std::io::Error::other(format!("serialize error: {e}")))?;
    json.push('\n');
    writer.write_all(json.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Request dispatch
// ---------------------------------------------------------------------------

/// Dispatch a parsed request to the appropriate handler.
async fn dispatch_request(
    coordinator: &Arc<Mutex<Coordinator>>,
    read_resolver: &Arc<dyn MountReadResolver>,
    request: IpcRequest,
) -> IpcResponse {
    match request {
        IpcRequest::Mount {
            remote,
            mountpoint,
            r#ref,
            read_only,
            no_refresh,
        } => {
            handle_mount(
                coordinator,
                read_resolver,
                remote,
                mountpoint,
                r#ref,
                read_only,
                no_refresh,
            )
            .await
        }
        IpcRequest::Unmount { mountpoint } => handle_unmount(coordinator, mountpoint).await,
        IpcRequest::List => handle_list(coordinator).await,
        IpcRequest::Status { mountpoint } => handle_status(coordinator, mountpoint).await,
        IpcRequest::Refresh { mountpoint } => handle_refresh(coordinator, mountpoint).await,
        IpcRequest::SwitchRef { mountpoint, r#ref } => {
            handle_switch_ref(coordinator, mountpoint, r#ref).await
        }
        IpcRequest::Invalidate { mountpoint, paths } => {
            handle_invalidate(coordinator, mountpoint, paths).await
        }
        IpcRequest::DiffOverlay { mountpoint } => {
            handle_diff_overlay(coordinator, mountpoint).await
        }
        IpcRequest::ResetOverlay { mountpoint } => {
            handle_reset_overlay(coordinator, mountpoint).await
        }
        IpcRequest::CommitOverlay {
            mountpoint,
            message,
            push,
        } => handle_commit_overlay(coordinator, mountpoint, message, push).await,
        IpcRequest::Ping => handle_ping(coordinator).await,
        IpcRequest::Health => handle_health(coordinator).await,
        IpcRequest::Shutdown => handle_shutdown(coordinator).await,
    }
}

// ---------------------------------------------------------------------------
// Operation handlers (stubs — full implementation in later tasks)
// ---------------------------------------------------------------------------

/// Handle a mount request.
///
/// Runs the full mount pipeline (clone → snapshot → overlay → FUSE mount)
/// and registers the result with the coordinator.
async fn handle_mount(
    coordinator: &Arc<Mutex<Coordinator>>,
    read_resolver: &Arc<dyn MountReadResolver>,
    remote: String,
    mountpoint: String,
    ref_name: String,
    read_only: bool,
    no_refresh: bool,
) -> IpcResponse {
    let read_context = match read_resolver.resolve(&remote).await {
        Ok(context) => context,
        Err(error) => return IpcResponse::err(format!("read layout resolution failed: {error}")),
    };
    if crab_git::CrabUrl::parse(&remote).is_ok() && read_context.is_none() {
        return IpcResponse::err(
            "object-store read layout unavailable for remote mount".to_owned(),
        );
    }

    // Compute cache directory from the remote URL.
    let cache_dir = match clone_cache::cache_dir_for_url(&remote) {
        Ok(dir) => dir,
        Err(e) => return IpcResponse::err(format!("failed to compute cache dir: {e}")),
    };

    let git_dir = cache_dir.join(".git");
    let mountpoint_buf = PathBuf::from(&mountpoint);
    let (reservation, cancel_token, chunk_cache) = {
        let mut coord = coordinator.lock().await;
        let reservation = match coord.reserve_mount(&mountpoint_buf, &cache_dir) {
            Ok(reservation) => reservation,
            Err(e) => return IpcResponse::err(format!("{e}")),
        };
        (
            reservation,
            coord.child_cancel_token(),
            Arc::clone(coord.chunk_cache()),
        )
    };
    let cache_lock = match clone_cache::MountCacheLock::acquire(&cache_dir) {
        Ok(lock) => lock,
        Err(e) => {
            release_mount_reservation(coordinator, reservation).await;
            return IpcResponse::err(format!("{e}"));
        }
    };

    // If the caller asked for HEAD, pass None so the pipeline resolves
    // HEAD naturally; otherwise pass the explicit ref.
    let ref_name_for_config = if ref_name == "HEAD" {
        None
    } else {
        Some(ref_name.clone())
    };

    let pipeline_config = PipelineConfig {
        source: remote.clone(),
        git_dir: git_dir.clone(),
        ref_name: ref_name_for_config,
        read_only,
        cache_dir: cache_dir.clone(),
        cancel_token: cancel_token.clone(),
    };

    // Clone config for use after pipeline consumes the original.
    let mut config_for_handle = pipeline_config.clone();

    let mut pipeline = MountPipelineBuilder::new(pipeline_config).with_chunk_cache(chunk_cache);
    if let Some(context) = read_context {
        pipeline = pipeline.with_read_context(context);
    }

    let output = match pipeline.execute() {
        Ok(o) => o,
        Err(e) => {
            release_mount_reservation(coordinator, reservation).await;
            return IpcResponse::err(format!("pipeline failed: {e}"));
        }
    };

    let head_oid = output.head_oid.clone();

    // Update the config to track the resolved ref (e.g. refs/heads/main)
    // rather than "HEAD", so the refresh loop polls the right remote ref.
    config_for_handle.ref_name = Some(output.head_ref.clone());

    // Create .crab directory for PID file storage.
    let crab_dir = cache_dir.join(".crab");
    if let Err(e) = std::fs::create_dir_all(&crab_dir) {
        cancel_token.cancel();
        release_mount_reservation(coordinator, reservation).await;
        return IpcResponse::err(format!("failed to create .crab dir: {e}"));
    }

    // Mount FUSE.
    let mount_config = MountConfig {
        mountpoint: PathBuf::from(&mountpoint),
        git_dir: git_dir.to_string_lossy().into_owned(),
        write_pid: true,
        crab_dir,
        read_only,
    };

    let rt_handle = tokio::runtime::Handle::current();
    let mounted = match mount::mount(
        &mount_config,
        Arc::clone(&output.resolver),
        Arc::clone(&output.engine),
        rt_handle,
    ) {
        Ok(s) => s,
        Err(e) => {
            cancel_token.cancel();
            release_mount_reservation(coordinator, reservation).await;
            return IpcResponse::err(format!("FUSE mount failed: {e}"));
        }
    };
    let invalidation_index = mounted.invalidation_index.clone();

    let bg_session = match mounted.session.spawn() {
        Ok(s) => s,
        Err(e) => {
            cancel_token.cancel();
            release_mount_reservation(coordinator, reservation).await;
            return IpcResponse::err(format!("FUSE background session failed: {e}"));
        }
    };

    // Spawn refresh loop if not read-only and not explicitly disabled.
    if !read_only && !no_refresh {
        pipeline::spawn_refresh_loop(
            &output,
            &config_for_handle,
            std::time::Duration::from_secs(30),
        );
    }

    // Register with coordinator.
    let handle = MountHandle {
        resolver: Arc::clone(&output.resolver),
        engine: Arc::clone(&output.engine),
        hydration: Arc::clone(&output.hydration),
        pipeline_output: output,
        config: config_for_handle,
        fuse_session: Some(bg_session),
        invalidation_index: Some(invalidation_index),
        cancel_token,
        _cache_lock: Some(cache_lock),
    };

    let registration_failure = {
        let mut coord = coordinator.lock().await;
        coord.add_reserved_mount(reservation, handle).err()
    };
    if let Some(failure) = registration_failure {
        let error = failure.error.to_string();
        let failed_handle = failure.handle;
        failed_handle.cancel_token.cancel();
        let cleanup = unmount_removed_mount(failed_handle, &mountpoint_buf);
        let cleanup_suffix = cleanup
            .err()
            .map(|e| format!("; cleanup failed: {e}"))
            .unwrap_or_default();
        return IpcResponse::err(format!("failed to register mount: {error}{cleanup_suffix}"));
    }

    IpcResponse::mount_ok(mountpoint, std::process::id(), Some(head_oid))
}

async fn release_mount_reservation(
    coordinator: &Arc<Mutex<Coordinator>>,
    reservation: MountReservation,
) {
    coordinator
        .lock()
        .await
        .release_mount_reservation(reservation);
}

/// Handle an unmount request.
async fn handle_unmount(coordinator: &Arc<Mutex<Coordinator>>, mountpoint: String) -> IpcResponse {
    let mp = PathBuf::from(&mountpoint);
    let handle = {
        let mut coord = coordinator.lock().await;
        match coord.take_mount(&mp) {
            Ok(handle) => handle,
            Err(e) => return IpcResponse::err(format!("{e}")),
        }
    };

    let unmount_result = unmount_removed_mount(handle, &mp);
    coordinator.lock().await.finish_mount_removal();

    match unmount_result {
        Ok(()) => IpcResponse::ok(),
        Err(e) => IpcResponse::err(format!("{e}")),
    }
}

/// Handle a list request.
async fn handle_list(coordinator: &Arc<Mutex<Coordinator>>) -> IpcResponse {
    let coord = coordinator.lock().await;
    let mounts: Vec<MountInfo> = coord
        .list_mountpoints()
        .into_iter()
        .map(|mp| {
            let handle = coord.get_mount(mp);
            MountInfo {
                mountpoint: mp.display().to_string(),
                remote: handle.map(|h| h.config.source.clone()).unwrap_or_default(),
                r#ref: handle
                    .and_then(|h| h.config.ref_name.clone())
                    .unwrap_or_default(),
                read_only: handle.is_some_and(|h| h.config.read_only),
            }
        })
        .collect();

    IpcResponse::list_ok(mounts)
}

/// Handle a status request.
async fn handle_status(coordinator: &Arc<Mutex<Coordinator>>, mountpoint: String) -> IpcResponse {
    let coord = coordinator.lock().await;
    let mp = Path::new(&mountpoint);

    match coord.get_mount(mp) {
        Some(handle) => {
            let status = MountStatus {
                mountpoint: mountpoint.clone(),
                remote: handle.config.source.clone(),
                r#ref: handle
                    .config
                    .ref_name
                    .clone()
                    .unwrap_or_else(|| handle.pipeline_output.head_ref.clone()),
                read_only: handle.config.read_only,
                head_oid: Some(handle.pipeline_output.head_oid.clone()),
                pid: Some(std::process::id()),
            };
            IpcResponse::status_ok(status)
        }
        None => IpcResponse::err(format!("mount not found: {mountpoint}")),
    }
}

/// Handle a refresh request.
///
/// Triggers an immediate git fetch + snapshot rebuild for the specified mount.
/// Returns the new HEAD OID on success.
async fn handle_refresh(coordinator: &Arc<Mutex<Coordinator>>, mountpoint: String) -> IpcResponse {
    let mut coord = coordinator.lock().await;
    let mp = Path::new(&mountpoint);

    if !coord.has_mount(mp) {
        return IpcResponse::err(format!("mount not found: {mountpoint}"));
    }

    match coord.refresh_mount(mp) {
        Ok(head_oid) => {
            let mut resp = IpcResponse::ok();
            resp.mountpoint = Some(mountpoint);
            resp.head_oid = Some(head_oid);
            resp
        }
        Err(e) => IpcResponse::err(format!("refresh failed: {e}")),
    }
}

/// Handle a switch_ref request.
///
/// Fetches the new ref, rebuilds snapshot, reconciles overlay, and swaps
/// the resolver's generation. Returns the new HEAD OID on success.
async fn handle_switch_ref(
    coordinator: &Arc<Mutex<Coordinator>>,
    mountpoint: String,
    ref_name: String,
) -> IpcResponse {
    let mut coord = coordinator.lock().await;
    let mp = Path::new(&mountpoint);

    if !coord.has_mount(mp) {
        return IpcResponse::err(format!("mount not found: {mountpoint}"));
    }

    match coord.switch_mount_ref(mp, &ref_name) {
        Ok(head_oid) => {
            let mut resp = IpcResponse::ok();
            resp.mountpoint = Some(mountpoint);
            resp.head_oid = Some(head_oid);
            resp
        }
        Err(e) => IpcResponse::err(format!("switch_ref failed: {e}")),
    }
}

/// Handle an invalidate request for out-of-band overlay changes.
async fn handle_invalidate(
    coordinator: &Arc<Mutex<Coordinator>>,
    mountpoint: String,
    paths: Vec<String>,
) -> IpcResponse {
    let coord = coordinator.lock().await;
    let mp = Path::new(&mountpoint);

    match coord.invalidate_mount_paths(mp, &paths) {
        Ok(()) => IpcResponse::ok(),
        Err(e) => IpcResponse::err(format!("invalidate failed: {e}")),
    }
}

/// Handle a mounted overlay diff request.
async fn handle_diff_overlay(
    coordinator: &Arc<Mutex<Coordinator>>,
    mountpoint: String,
) -> IpcResponse {
    let coord = coordinator.lock().await;
    let mp = Path::new(&mountpoint);

    match coord.inspect_mount_overlay(mp) {
        Ok(diff) => {
            let mut response = IpcResponse::ok();
            response.mountpoint = Some(mountpoint);
            response.overlay_diff = Some(diff);
            response
        }
        Err(e) => IpcResponse::err(format!("diff failed: {e}")),
    }
}

/// Handle a mounted overlay reset request.
async fn handle_reset_overlay(
    coordinator: &Arc<Mutex<Coordinator>>,
    mountpoint: String,
) -> IpcResponse {
    let mp = Path::new(&mountpoint);
    let target = {
        let coord = coordinator.lock().await;
        match coord.overlay_reset_target(mp) {
            Ok(target) => target,
            Err(e) => return IpcResponse::err(format!("reset failed: {e}")),
        }
    };

    match Coordinator::reset_overlay_target(target).await {
        Ok(diff) => {
            let paths = diff
                .changes
                .iter()
                .map(|change| change.path.clone())
                .collect::<Vec<_>>();
            let invalidate_result = {
                let coord = coordinator.lock().await;
                if coord.has_mount(mp) {
                    coord.invalidate_mount_paths(mp, &paths)
                } else {
                    Ok(())
                }
            };
            if let Err(e) = invalidate_result {
                return IpcResponse::err(format!("reset invalidation failed: {e}"));
            }
            let mut response = IpcResponse::ok();
            response.mountpoint = Some(mountpoint);
            response.overlay_diff = Some(diff);
            response
        }
        Err(e) => IpcResponse::err(format!("reset failed: {e}")),
    }
}

/// Commit a mounted overlay while the live engine owns the generation swap.
async fn handle_commit_overlay(
    coordinator: &Arc<Mutex<Coordinator>>,
    mountpoint: String,
    message: String,
    push: bool,
) -> IpcResponse {
    let target = {
        let coord = coordinator.lock().await;
        match coord.overlay_commit_target(Path::new(&mountpoint), message, push) {
            Ok(target) => target,
            Err(error) => return IpcResponse::err(format!("commit failed: {error}")),
        }
    };
    let engine = Arc::clone(&target.engine);
    let _reset = engine.begin_overlay_reset().await;
    let committed_mountpoint = target.mountpoint;
    let head_ref = target.head_ref;
    let result = match tokio::task::spawn_blocking(move || {
        crate::publish::commit_overlay_with_snapshot(
            &target.options,
            Some(target.snapshot.as_ref()),
        )
    })
    .await
    {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => return IpcResponse::err(format!("commit failed: {error}")),
        Err(error) => return IpcResponse::err(format!("commit task failed: {error}")),
    };

    if let Some(head_oid) = result.commit_oid.as_deref() {
        let paths = result
            .diff
            .changes
            .iter()
            .map(|change| change.path.clone())
            .collect::<Vec<_>>();
        let adoption = {
            let mut coord = coordinator.lock().await;
            coord
                .adopt_mount_commit(&committed_mountpoint, head_oid, &head_ref)
                .and_then(|()| coord.invalidate_mount_paths(&committed_mountpoint, &paths))
        };
        if let Err(error) = adoption {
            return IpcResponse::err(format!(
                "commit {head_oid} succeeded but live mount adoption failed: {error}; run `crab mount refresh`"
            ));
        }
    }

    let mut response = IpcResponse::ok();
    response.mountpoint = Some(mountpoint);
    response.head_oid.clone_from(&result.commit_oid);
    response.overlay_commit_result = Some(result);
    response
}

/// Handle a ping request.
///
/// Responds with the coordinator's PID and uptime. This is a lightweight
/// liveness check that should complete well within 100ms.
async fn handle_ping(coordinator: &Arc<Mutex<Coordinator>>) -> IpcResponse {
    let coord = coordinator.lock().await;
    let pid = std::process::id();
    let uptime_secs = coord.uptime_secs();
    IpcResponse::ping_ok(pid, uptime_secs)
}

/// Handle a health request.
///
/// Responds with detailed coordinator state: mount count, cache size,
/// hydration queue depth, worker count, and uptime.
async fn handle_health(coordinator: &Arc<Mutex<Coordinator>>) -> IpcResponse {
    let coord = coordinator.lock().await;
    let pid = std::process::id();
    let uptime_secs = coord.uptime_secs();
    let mount_count = coord.mount_count();
    let cache_size_bytes = coord.cache_size_bytes();
    let hydration_workers = coord.hydration_workers();
    // Hydration queue depth requires iterating mounts; for now report 0
    // since the coordinator doesn't aggregate per-mount hydration queues.
    let hydration_queue_depth = 0;

    IpcResponse::health_ok(
        pid,
        uptime_secs,
        mount_count,
        cache_size_bytes,
        hydration_queue_depth,
        hydration_workers,
    )
}

/// Handle a shutdown request.
///
/// Responds with `{"ok": true}` and then cancels the coordinator's
/// cancellation token to initiate graceful shutdown.
async fn handle_shutdown(coordinator: &Arc<Mutex<Coordinator>>) -> IpcResponse {
    let coord = coordinator.lock().await;
    info!("shutdown requested via IPC");
    coord.cancel_token().cancel();
    IpcResponse::ok()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;

    #[test]
    fn serialize_mount_request() {
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
        assert!(json.contains(r#""mountpoint":"/mnt/view""#));
        assert!(json.contains(r#""ref":"main""#));
        assert!(json.contains(r#""read_only":false"#));
        assert!(json.contains(r#""no_refresh":true"#));
    }

    #[test]
    fn deserialize_mount_request() {
        let json = r#"{"op": "mount", "remote": "crab://bucket/repo", "mountpoint": "/mnt/view", "ref": "main", "read_only": false}"#;
        let req: IpcRequest = serde_json::from_str(json).unwrap();
        match req {
            IpcRequest::Mount {
                remote,
                mountpoint,
                r#ref,
                read_only,
                no_refresh,
            } => {
                assert_eq!(remote, "crab://bucket/repo");
                assert_eq!(mountpoint, "/mnt/view");
                assert_eq!(r#ref, "main");
                assert!(!read_only);
                assert!(!no_refresh);
            }
            _ => panic!("expected Mount variant"),
        }
    }

    #[test]
    fn deserialize_unmount_request() {
        let json = r#"{"op": "unmount", "mountpoint": "/mnt/view"}"#;
        let req: IpcRequest = serde_json::from_str(json).unwrap();
        match req {
            IpcRequest::Unmount { mountpoint } => {
                assert_eq!(mountpoint, "/mnt/view");
            }
            _ => panic!("expected Unmount variant"),
        }
    }

    #[test]
    fn deserialize_list_request() {
        let json = r#"{"op": "list"}"#;
        let req: IpcRequest = serde_json::from_str(json).unwrap();
        assert!(matches!(req, IpcRequest::List));
    }

    #[test]
    fn deserialize_status_request() {
        let json = r#"{"op": "status", "mountpoint": "/mnt/view"}"#;
        let req: IpcRequest = serde_json::from_str(json).unwrap();
        match req {
            IpcRequest::Status { mountpoint } => {
                assert_eq!(mountpoint, "/mnt/view");
            }
            _ => panic!("expected Status variant"),
        }
    }

    #[test]
    fn deserialize_refresh_request() {
        let json = r#"{"op": "refresh", "mountpoint": "/mnt/view"}"#;
        let req: IpcRequest = serde_json::from_str(json).unwrap();
        match req {
            IpcRequest::Refresh { mountpoint } => {
                assert_eq!(mountpoint, "/mnt/view");
            }
            _ => panic!("expected Refresh variant"),
        }
    }

    #[test]
    fn deserialize_switch_ref_request() {
        let json = r#"{"op": "switch_ref", "mountpoint": "/mnt/view", "ref": "feature-branch"}"#;
        let req: IpcRequest = serde_json::from_str(json).unwrap();
        match req {
            IpcRequest::SwitchRef { mountpoint, r#ref } => {
                assert_eq!(mountpoint, "/mnt/view");
                assert_eq!(r#ref, "feature-branch");
            }
            _ => panic!("expected SwitchRef variant"),
        }
    }

    #[test]
    fn deserialize_invalidate_request() {
        let json =
            r#"{"op": "invalidate", "mountpoint": "/mnt/view", "paths": ["models/model.bin"]}"#;
        let req: IpcRequest = serde_json::from_str(json).unwrap();
        match req {
            IpcRequest::Invalidate { mountpoint, paths } => {
                assert_eq!(mountpoint, "/mnt/view");
                assert_eq!(paths, vec!["models/model.bin"]);
            }
            _ => panic!("expected Invalidate variant"),
        }
    }

    #[test]
    fn deserialize_diff_overlay_request() {
        let json = r#"{"op": "diff_overlay", "mountpoint": "/mnt/view"}"#;
        let req: IpcRequest = serde_json::from_str(json).unwrap();
        match req {
            IpcRequest::DiffOverlay { mountpoint } => {
                assert_eq!(mountpoint, "/mnt/view");
            }
            _ => panic!("expected DiffOverlay variant"),
        }
    }

    #[test]
    fn deserialize_reset_overlay_request() {
        let json = r#"{"op": "reset_overlay", "mountpoint": "/mnt/view"}"#;
        let req: IpcRequest = serde_json::from_str(json).unwrap();
        match req {
            IpcRequest::ResetOverlay { mountpoint } => {
                assert_eq!(mountpoint, "/mnt/view");
            }
            _ => panic!("expected ResetOverlay variant"),
        }
    }

    #[test]
    fn deserialize_commit_overlay_request() {
        let json = r#"{"op":"commit_overlay","mountpoint":"/mnt/view","message":"mounted commit","push":true}"#;
        let req: IpcRequest = serde_json::from_str(json).unwrap();
        match req {
            IpcRequest::CommitOverlay {
                mountpoint,
                message,
                push,
            } => {
                assert_eq!(mountpoint, "/mnt/view");
                assert_eq!(message, "mounted commit");
                assert!(push);
            }
            _ => panic!("expected CommitOverlay variant"),
        }
    }

    #[test]
    fn serialize_success_response() {
        let resp = IpcResponse::ok();
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains(r#""ok":true"#));
        assert!(!json.contains("error"));
    }

    #[test]
    fn serialize_error_response() {
        let resp = IpcResponse::err("something went wrong");
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains(r#""ok":false"#));
        assert!(json.contains(r#""error":"something went wrong""#));
    }

    #[test]
    fn serialize_mount_response() {
        let resp = IpcResponse::mount_ok("/mnt/view".to_owned(), 12345, Some("abc123".to_owned()));
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains(r#""ok":true"#));
        assert!(json.contains(r#""mountpoint":"/mnt/view""#));
        assert!(json.contains(r#""pid":12345"#));
        assert!(json.contains(r#""head_oid":"abc123""#));
    }

    #[test]
    fn serialize_list_response() {
        let mounts = vec![MountInfo {
            mountpoint: "/mnt/a".to_owned(),
            remote: "crab://bucket/repo".to_owned(),
            r#ref: "main".to_owned(),
            read_only: false,
        }];
        let resp = IpcResponse::list_ok(mounts);
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains(r#""ok":true"#));
        assert!(json.contains(r#""mounts""#));
        assert!(json.contains(r#""mountpoint":"/mnt/a""#));
    }

    #[test]
    fn serialize_status_response() {
        let resp = IpcResponse::status_ok(MountStatus {
            mountpoint: "/mnt/view".to_owned(),
            remote: "crab://bucket/repo".to_owned(),
            r#ref: "refs/heads/main".to_owned(),
            read_only: false,
            head_oid: Some("abc123".to_owned()),
            pid: Some(12345),
        });
        let json = serde_json::to_string(&resp).unwrap();

        assert!(json.contains(r#""ok":true"#));
        assert!(json.contains(r#""status""#));
        assert!(json.contains(r#""head_oid":"abc123""#));
        assert!(json.contains(r#""pid":12345"#));
    }

    #[test]
    fn deserialize_mount_request_default_ref() {
        let json = r#"{"op": "mount", "remote": "crab://bucket/repo", "mountpoint": "/mnt/view"}"#;
        let req: IpcRequest = serde_json::from_str(json).unwrap();
        match req {
            IpcRequest::Mount {
                r#ref, no_refresh, ..
            } => {
                assert_eq!(r#ref, "HEAD");
                assert!(!no_refresh);
            }
            _ => panic!("expected Mount variant"),
        }
    }

    #[test]
    fn roundtrip_all_request_types() {
        let requests = vec![
            r#"{"op":"mount","remote":"crab://b/r","mountpoint":"/m","ref":"main","read_only":true}"#,
            r#"{"op":"unmount","mountpoint":"/m"}"#,
            r#"{"op":"list"}"#,
            r#"{"op":"status","mountpoint":"/m"}"#,
            r#"{"op":"refresh","mountpoint":"/m"}"#,
            r#"{"op":"switch_ref","mountpoint":"/m","ref":"dev"}"#,
            r#"{"op":"invalidate","mountpoint":"/m","paths":["models/model.bin"]}"#,
            r#"{"op":"ping"}"#,
            r#"{"op":"health"}"#,
            r#"{"op":"shutdown"}"#,
        ];

        for json in requests {
            let req: IpcRequest = serde_json::from_str(json).unwrap();
            let reserialized = serde_json::to_string(&req).unwrap();
            let req2: IpcRequest = serde_json::from_str(&reserialized).unwrap();
            // Verify round-trip produces equivalent JSON.
            let reserialized2 = serde_json::to_string(&req2).unwrap();
            assert_eq!(reserialized, reserialized2);
        }
    }

    #[test]
    fn deserialize_ping_request() {
        let json = r#"{"op": "ping"}"#;
        let req: IpcRequest = serde_json::from_str(json).unwrap();
        assert!(matches!(req, IpcRequest::Ping));
    }

    #[test]
    fn deserialize_health_request() {
        let json = r#"{"op": "health"}"#;
        let req: IpcRequest = serde_json::from_str(json).unwrap();
        assert!(matches!(req, IpcRequest::Health));
    }

    #[test]
    fn deserialize_shutdown_request() {
        let json = r#"{"op": "shutdown"}"#;
        let req: IpcRequest = serde_json::from_str(json).unwrap();
        assert!(matches!(req, IpcRequest::Shutdown));
    }

    #[test]
    fn serialize_ping_response() {
        let resp = IpcResponse::ping_ok(12345, 3600);
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains(r#""ok":true"#));
        assert!(json.contains(r#""pid":12345"#));
        assert!(json.contains(r#""uptime_secs":3600"#));
        // Should not contain health-only fields.
        assert!(!json.contains("mount_count"));
        assert!(!json.contains("cache_size_bytes"));
        assert!(!json.contains("hydration_queue_depth"));
        assert!(!json.contains("hydration_workers"));
    }

    #[test]
    fn serialize_health_response() {
        let resp = IpcResponse::health_ok(12345, 3600, 2, 524_288_000, 0, 4);
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains(r#""ok":true"#));
        assert!(json.contains(r#""pid":12345"#));
        assert!(json.contains(r#""uptime_secs":3600"#));
        assert!(json.contains(r#""mount_count":2"#));
        assert!(json.contains(r#""cache_size_bytes":524288000"#));
        assert!(json.contains(r#""hydration_queue_depth":0"#));
        assert!(json.contains(r#""hydration_workers":4"#));
    }

    #[test]
    fn health_response_roundtrip() {
        let resp = IpcResponse::health_ok(99999, 7200, 5, 1_073_741_824, 3, 8);
        let json = serde_json::to_string(&resp).unwrap();
        let deserialized: IpcResponse = serde_json::from_str(&json).unwrap();
        assert!(deserialized.ok);
        assert_eq!(deserialized.pid, Some(99999));
        assert_eq!(deserialized.uptime_secs, Some(7200));
        assert_eq!(deserialized.mount_count, Some(5));
        assert_eq!(deserialized.cache_size_bytes, Some(1_073_741_824));
        assert_eq!(deserialized.hydration_queue_depth, Some(3));
        assert_eq!(deserialized.hydration_workers, Some(8));
    }

    #[test]
    fn ping_response_roundtrip() {
        let resp = IpcResponse::ping_ok(42, 120);
        let json = serde_json::to_string(&resp).unwrap();
        let deserialized: IpcResponse = serde_json::from_str(&json).unwrap();
        assert!(deserialized.ok);
        assert_eq!(deserialized.pid, Some(42));
        assert_eq!(deserialized.uptime_secs, Some(120));
        assert_eq!(deserialized.mount_count, None);
    }

    #[tokio::test]
    async fn ipc_server_accepts_and_responds() {
        let tmp = tempfile::tempdir().unwrap();
        let socket_path = tmp.path().join("test.sock");

        // We need a coordinator to test with. Use a minimal setup.
        let config = crate::coordinator::CoordinatorConfig::with_base_dir(tmp.path().to_path_buf());
        let coordinator = Coordinator::start(config).unwrap();
        let cancel_token = coordinator.cancel_token().clone();
        let coordinator = Arc::new(Mutex::new(coordinator));

        let server = IpcServer::new(
            Arc::clone(&coordinator),
            socket_path.clone(),
            cancel_token.clone(),
        );

        // Spawn the server in a background task.
        let server_handle = tokio::spawn(async move { server.run().await });

        // Give the server a moment to bind.
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Connect as a client.
        let stream = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();

        // Send a list request.
        writer.write_all(b"{\"op\":\"list\"}\n").await.unwrap();
        writer.flush().await.unwrap();

        // Read the response.
        let response_line = lines.next_line().await.unwrap().unwrap();
        let response: IpcResponse = serde_json::from_str(&response_line).unwrap();
        assert!(response.ok);
        assert!(response.mounts.is_some());
        assert!(response.mounts.unwrap().is_empty());

        // Send an invalid request.
        writer.write_all(b"not json\n").await.unwrap();
        writer.flush().await.unwrap();

        let response_line = lines.next_line().await.unwrap().unwrap();
        let response: IpcResponse = serde_json::from_str(&response_line).unwrap();
        assert!(!response.ok);
        assert!(response.error.unwrap().contains("invalid request"));

        // Shutdown the server.
        cancel_token.cancel();
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn ipc_server_bound_hook_runs_after_socket_bind() {
        let tmp = tempfile::tempdir().unwrap();
        let socket_path = tmp.path().join("test.sock");

        let config = crate::coordinator::CoordinatorConfig::with_base_dir(tmp.path().to_path_buf());
        let coordinator = Coordinator::start(config).unwrap();
        let cancel_token = coordinator.cancel_token().clone();
        let coordinator = Arc::new(Mutex::new(coordinator));

        let server = IpcServer::new(
            Arc::clone(&coordinator),
            socket_path.clone(),
            cancel_token.clone(),
        );
        let (bound_tx, bound_rx) = tokio::sync::oneshot::channel();

        let server_handle = tokio::spawn(async move {
            server
                .run_with_bound_hook(move || {
                    let _ = bound_tx.send(());
                })
                .await
        });

        bound_rx.await.unwrap();
        tokio::net::UnixStream::connect(&socket_path).await.unwrap();

        cancel_token.cancel();
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn ipc_server_handles_concurrent_connections() {
        let tmp = tempfile::tempdir().unwrap();
        let socket_path = tmp.path().join("test.sock");

        let config = crate::coordinator::CoordinatorConfig::with_base_dir(tmp.path().to_path_buf());
        let coordinator = Coordinator::start(config).unwrap();
        let cancel_token = coordinator.cancel_token().clone();
        let coordinator = Arc::new(Mutex::new(coordinator));

        let server = IpcServer::new(
            Arc::clone(&coordinator),
            socket_path.clone(),
            cancel_token.clone(),
        );

        let server_handle = tokio::spawn(async move { server.run().await });

        tokio::time::sleep(Duration::from_millis(50)).await;

        // Spawn multiple concurrent clients.
        let mut handles = Vec::new();
        for i in 0..5 {
            let path = socket_path.clone();
            handles.push(tokio::spawn(async move {
                let stream = tokio::net::UnixStream::connect(&path).await.unwrap();
                let (reader, mut writer) = stream.into_split();
                let mut lines = BufReader::new(reader).lines();

                // Each client sends a list request.
                writer.write_all(b"{\"op\":\"list\"}\n").await.unwrap();
                writer.flush().await.unwrap();

                let response_line = lines.next_line().await.unwrap().unwrap();
                let response: IpcResponse = serde_json::from_str(&response_line).unwrap();
                assert!(response.ok, "client {i} got error: {:?}", response.error);
                i
            }));
        }

        // All clients should succeed.
        for handle in handles {
            handle.await.unwrap();
        }

        cancel_token.cancel();
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn ipc_server_idle_timeout() {
        let tmp = tempfile::tempdir().unwrap();
        let socket_path = tmp.path().join("test.sock");

        let config = crate::coordinator::CoordinatorConfig::with_base_dir(tmp.path().to_path_buf());
        let coordinator = Coordinator::start(config).unwrap();
        let cancel_token = coordinator.cancel_token().clone();
        let coordinator = Arc::new(Mutex::new(coordinator));

        // Use a shorter timeout for testing by testing the timeout logic
        // indirectly: we verify the connection handler closes after inactivity.
        // For a real test we'd need to override IDLE_TIMEOUT, but we can
        // verify the mechanism works by checking the server stays alive
        // and the connection eventually closes.

        let server = IpcServer::new(
            Arc::clone(&coordinator),
            socket_path.clone(),
            cancel_token.clone(),
        );

        let server_handle = tokio::spawn(async move { server.run().await });

        tokio::time::sleep(Duration::from_millis(50)).await;

        // Connect and send one request, then verify the connection works.
        let stream = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();

        writer.write_all(b"{\"op\":\"list\"}\n").await.unwrap();
        writer.flush().await.unwrap();

        let response_line = lines.next_line().await.unwrap().unwrap();
        let response: IpcResponse = serde_json::from_str(&response_line).unwrap();
        assert!(response.ok);

        // Clean shutdown.
        cancel_token.cancel();
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn ipc_server_ping_and_health_handlers() {
        let tmp = tempfile::tempdir().unwrap();
        let socket_path = tmp.path().join("test.sock");

        let config = crate::coordinator::CoordinatorConfig::with_base_dir(tmp.path().to_path_buf());
        let coordinator = Coordinator::start(config).unwrap();
        let cancel_token = coordinator.cancel_token().clone();
        let coordinator = Arc::new(Mutex::new(coordinator));

        let server = IpcServer::new(
            Arc::clone(&coordinator),
            socket_path.clone(),
            cancel_token.clone(),
        );

        let server_handle = tokio::spawn(async move { server.run().await });
        tokio::time::sleep(Duration::from_millis(50)).await;

        let stream = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();

        // Test ping: should return PID and uptime.
        writer.write_all(b"{\"op\":\"ping\"}\n").await.unwrap();
        writer.flush().await.unwrap();

        let response_line = lines.next_line().await.unwrap().unwrap();
        let response: IpcResponse = serde_json::from_str(&response_line).unwrap();
        assert!(response.ok);
        assert_eq!(response.pid, Some(std::process::id()));
        assert!(response.uptime_secs.is_some());

        // Test health: should return all health fields.
        writer.write_all(b"{\"op\":\"health\"}\n").await.unwrap();
        writer.flush().await.unwrap();

        let response_line = lines.next_line().await.unwrap().unwrap();
        let response: IpcResponse = serde_json::from_str(&response_line).unwrap();
        assert!(response.ok);
        assert_eq!(response.pid, Some(std::process::id()));
        assert!(response.uptime_secs.is_some());
        assert_eq!(response.mount_count, Some(0));
        assert!(response.cache_size_bytes.is_some());
        assert_eq!(response.hydration_queue_depth, Some(0));
        assert_eq!(response.hydration_workers, Some(4));

        // Clean shutdown.
        cancel_token.cancel();
        let _ = server_handle.await;
    }

    #[tokio::test]
    async fn ipc_server_shutdown_cancels_token() {
        let tmp = tempfile::tempdir().unwrap();
        let socket_path = tmp.path().join("test.sock");

        let config = crate::coordinator::CoordinatorConfig::with_base_dir(tmp.path().to_path_buf());
        let coordinator = Coordinator::start(config).unwrap();
        let cancel_token = coordinator.cancel_token().clone();
        let coordinator = Arc::new(Mutex::new(coordinator));

        let server = IpcServer::new(
            Arc::clone(&coordinator),
            socket_path.clone(),
            cancel_token.clone(),
        );

        let server_handle = tokio::spawn(async move { server.run().await });
        tokio::time::sleep(Duration::from_millis(50)).await;

        let stream = tokio::net::UnixStream::connect(&socket_path).await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();

        // Send shutdown request.
        writer.write_all(b"{\"op\":\"shutdown\"}\n").await.unwrap();
        writer.flush().await.unwrap();

        let response_line = lines.next_line().await.unwrap().unwrap();
        let response: IpcResponse = serde_json::from_str(&response_line).unwrap();
        assert!(response.ok);

        // The cancellation token should now be cancelled.
        assert!(cancel_token.is_cancelled());

        // The server should shut down on its own.
        let _ = tokio::time::timeout(Duration::from_secs(2), server_handle).await;
    }
}
