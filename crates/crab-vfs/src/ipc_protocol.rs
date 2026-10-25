//! Request and response contracts shared by Unix mount clients and the coordinator.

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
