//! Local control channel for a running NFS mount helper.

#[cfg(any(not(unix), test))]
use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[cfg(any(not(unix), test))]
use rand::Rng as _;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::core::error::{CrabError, Result};
use crate::engine::{
    VfsAdaptiveReadMetricsSnapshot, VfsEngine, VfsReadMetricsSnapshot, VfsSourceReadMetricsSnapshot,
};
use crate::hydration::HydrationReadStatsSnapshot;
use crate::nfs::{
    NfsDirectoryPageCache, NfsDirectoryPageCacheSnapshot, NfsProtocolStats,
    NfsProtocolStatsSnapshot, NfsRuntimeSnapshot, NfsWriteJournal, NfsWriteJournalSnapshot,
    NfsWriteStability,
};
use crate::pipeline::{PipelineConfig, PipelineOutput};
use crate::read_lease_pool::{ReadLeasePool, ReadLeasePoolSnapshot};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const PUBLISH_REQUEST_TIMEOUT: Duration = Duration::from_mins(30);
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
pub const CONTROL_ENDPOINT_ENV: &str = "CRAB_NFS_CONTROL_ENDPOINT";
#[cfg(any(not(unix), test))]
const TCP_CONTROL_PORT_BASE: u16 = 49_152;
#[cfg(any(not(unix), test))]
const TCP_CONTROL_PORT_RANGE: u16 = 16_384;

#[derive(Clone)]
pub struct NfsControlState {
    /// Request native detachment without closing the transport before it completes.
    pub shutdown: CancellationToken,
    pub mountpoint: PathBuf,
    pub read_only: bool,
    pub read_leases: Arc<ReadLeasePool>,
    pub directory_pages: Arc<NfsDirectoryPageCache>,
    pub write_journal: Arc<NfsWriteJournal>,
    pub protocol_stats: Arc<NfsProtocolStats>,
    pub engine: Option<Arc<VfsEngine>>,
    pub runtime: Option<Arc<Mutex<NfsMountRuntime>>>,
    pub lifecycle: NfsMountLifecycleStatus,
}

pub struct NfsMountRuntime {
    pub output: PipelineOutput,
    pub config: PipelineConfig,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NfsMountLifecycleStatus {
    pub server_bind_ms: u64,
    pub native_mount_ms: u64,
    pub startup_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum NfsControlRequest {
    Ping,
    Status,
    Refresh,
    SwitchRef { r#ref: String },
    DiffOverlay,
    ExportOverlay { destination: PathBuf },
    ResetOverlay,
    Commit { message: String, push: bool },
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct NfsTcpControlRequest {
    token: String,
    #[serde(flatten)]
    request: NfsControlRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NfsControlResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<NfsControlStatus>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub update: Option<NfsControlUpdate>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overlay_diff: Option<crate::publish::OverlayDiff>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit_result: Option<crate::publish::OverlayCommitResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NfsControlStatus {
    pub mountpoint: String,
    pub pid: u32,
    pub read_only: bool,
    pub head_oid: Option<String>,
    pub head_ref: Option<String>,
    pub runtime: NfsRuntimeStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NfsControlUpdate {
    pub mountpoint: String,
    pub head_oid: String,
    pub head_ref: String,
    pub generation: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NfsRuntimeStatus {
    pub lifecycle: NfsMountLifecycleStatus,
    pub read_leases: NfsReadLeasePoolStatus,
    pub directory_pages: NfsDirectoryPageCacheStatus,
    pub write_journal: NfsWriteJournalStatus,
    pub protocol: NfsProtocolStatus,
    pub vfs: NfsVfsReadStatus,
    pub hydration: NfsHydrationReadStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NfsProtocolStatus {
    pub read_rpcs: u64,
    pub read_requested_bytes: u64,
    pub read_returned_bytes: u64,
    pub read_size_le_4k: u64,
    pub read_size_le_64k: u64,
    pub read_size_le_1m: u64,
    pub read_size_gt_1m: u64,
    pub readdirplus_rpcs: u64,
    pub readdirplus_entries: u64,
    pub readdirplus_materialized_entries: u64,
    pub readdirplus_returned_candidates: u64,
    pub readdirplus_attr_resolutions: u64,
    pub readdirplus_prefetch_paths: u64,
    pub readdirplus_cookie_resumes: u64,
    pub readdirplus_cookie_misses: u64,
    pub readdirplus_skipped_entries: u64,
    pub readdirplus_large_dirs: u64,
    pub readdirplus_prefetch_errors: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NfsReadLeasePoolStatus {
    pub entries: usize,
    pub max_entries: usize,
    pub estimated_bytes: usize,
    pub max_estimated_bytes: usize,
    pub pinned_entries: usize,
    pub active_pins: u64,
    pub temporary_overflows: u64,
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    pub stale_retries: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NfsDirectoryPageCacheStatus {
    pub entries: usize,
    pub max_entries: usize,
    pub estimated_bytes: usize,
    pub max_estimated_bytes: usize,
    pub hits: u64,
    pub misses: u64,
    pub evictions: u64,
    pub stale_evictions: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NfsVfsReadStatus {
    pub open_read_calls: u64,
    pub read_at_calls: u64,
    pub returned_bytes: u64,
    pub stale_generation_rejections: u64,
    pub stale_overlay_view_rejections: u64,
    pub stale_overlay_file_rejections: u64,
    pub source_cache_entries: usize,
    pub source_cache_max_entries: usize,
    pub source_cache_estimated_bytes: usize,
    pub source_cache_max_estimated_bytes: usize,
    pub source_cache_hits: u64,
    pub resolver_calls_avoided: u64,
    pub source_cache_misses: u64,
    pub source_cache_evictions: u64,
    pub source_cache_invalidations: u64,
    pub source_cache_stale_evictions: u64,
    pub invalidation_path_events: u64,
    pub invalidation_subtree_events: u64,
    pub invalidation_rename_events: u64,
    pub invalidation_generation_events: u64,
    pub invalidation_overlay_reset_events: u64,
    pub invalidation_compacted_full_resets: u64,
    pub base_pointer: NfsVfsSourceReadStatus,
    pub base_blob: NfsVfsSourceReadStatus,
    pub base_empty: NfsVfsSourceReadStatus,
    pub overlay_file: NfsVfsSourceReadStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NfsVfsSourceReadStatus {
    pub reads: u64,
    pub bytes: u64,
    pub adaptive: NfsAdaptiveReadStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NfsAdaptiveReadStatus {
    pub first: u64,
    pub sequential: u64,
    pub strided: u64,
    pub repeated: u64,
    pub random: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NfsHydrationReadStatus {
    pub read_range_requests: u64,
    pub read_range_requested_bytes: u64,
    pub read_range_returned_bytes: u64,
    pub read_window_cache_hits: u64,
    pub read_window_cache_misses: u64,
    pub read_window_inflight_waits: u64,
    pub read_window_remote_fetches: u64,
    pub read_window_remote_bytes: u64,
    pub read_window_prefetch_requests: u64,
    pub read_window_prefetch_scheduled: u64,
    pub read_window_prefetch_skipped: u64,
    pub read_window_prefetch_errors: u64,
    pub chunk_cache_hits: u64,
    pub chunk_cache_misses: u64,
    pub chunk_inflight_waits: u64,
    pub chunk_remote_fetches: u64,
    pub chunk_remote_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NfsWriteJournalStatus {
    pub pending_paths: usize,
    pub oldest_dirty_age_secs: Option<u64>,
    pub paths_with_sync_errors: usize,
    pub sync_attempts: u64,
    pub sync_successes: u64,
    pub sync_failures: u64,
    pub total_sync_latency_ms: u64,
    pub last_sync_latency_ms: Option<u64>,
    pub max_sync_latency_ms: Option<u64>,
    pub poisoned: bool,
    pub entries: Vec<NfsWriteJournalPathStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NfsWriteJournalPathStatus {
    pub path: String,
    pub overlay_version: u64,
    pub last_write_stability: String,
    pub dirty_age_secs: Option<u64>,
    pub last_sync_error: Option<String>,
}

impl NfsControlResponse {
    fn shutdown_ok(cache_dir: Option<PathBuf>) -> Self {
        Self {
            ok: true,
            error: None,
            status: None,
            update: None,
            overlay_diff: None,
            commit_result: None,
            cache_dir,
            pid: None,
        }
    }

    fn err(error: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(error.into()),
            status: None,
            update: None,
            overlay_diff: None,
            commit_result: None,
            cache_dir: None,
            pid: None,
        }
    }

    fn ping_ok() -> Self {
        Self {
            ok: true,
            error: None,
            status: None,
            update: None,
            overlay_diff: None,
            commit_result: None,
            cache_dir: None,
            pid: Some(std::process::id()),
        }
    }

    fn status_ok(status: NfsControlStatus) -> Self {
        Self {
            ok: true,
            error: None,
            status: Some(status),
            update: None,
            overlay_diff: None,
            commit_result: None,
            cache_dir: None,
            pid: Some(std::process::id()),
        }
    }

    fn update_ok(update: NfsControlUpdate) -> Self {
        Self {
            ok: true,
            error: None,
            status: None,
            update: Some(update),
            overlay_diff: None,
            commit_result: None,
            cache_dir: None,
            pid: Some(std::process::id()),
        }
    }

    fn overlay_diff_ok(diff: crate::publish::OverlayDiff) -> Self {
        Self {
            ok: true,
            error: None,
            status: None,
            update: None,
            overlay_diff: Some(diff),
            commit_result: None,
            cache_dir: None,
            pid: Some(std::process::id()),
        }
    }

    fn commit_ok(result: crate::publish::OverlayCommitResult) -> Self {
        Self {
            ok: true,
            error: None,
            status: None,
            update: None,
            overlay_diff: None,
            commit_result: Some(result),
            cache_dir: None,
            pid: Some(std::process::id()),
        }
    }
}

impl NfsControlStatus {
    fn from_state(state: &NfsControlState) -> Result<Self> {
        let (head_oid, head_ref) = state
            .runtime
            .as_ref()
            .map(runtime_head)
            .transpose()?
            .unwrap_or((None, None));
        Ok(Self {
            mountpoint: state.mountpoint.display().to_string(),
            pid: std::process::id(),
            read_only: state.read_only,
            head_oid,
            head_ref,
            runtime: NfsRuntimeStatus::from_snapshot(
                NfsRuntimeSnapshot {
                    read_leases: state.read_leases.snapshot(),
                    directory_pages: state.directory_pages.snapshot(),
                    write_journal: state.write_journal.snapshot(),
                    protocol: state.protocol_stats.snapshot(),
                    vfs: state
                        .engine
                        .as_ref()
                        .map(|engine| engine.read_metrics_snapshot())
                        .unwrap_or_default(),
                    hydration: state
                        .engine
                        .as_ref()
                        .map(|engine| engine.hydration_read_stats_snapshot())
                        .unwrap_or_default(),
                },
                state.lifecycle.clone(),
            ),
        })
    }
}

impl NfsRuntimeStatus {
    pub fn from_snapshot(snapshot: NfsRuntimeSnapshot, lifecycle: NfsMountLifecycleStatus) -> Self {
        Self {
            lifecycle,
            read_leases: NfsReadLeasePoolStatus::from(snapshot.read_leases),
            directory_pages: NfsDirectoryPageCacheStatus::from(snapshot.directory_pages),
            write_journal: NfsWriteJournalStatus::from(snapshot.write_journal),
            protocol: NfsProtocolStatus::from(snapshot.protocol),
            vfs: NfsVfsReadStatus::from(snapshot.vfs),
            hydration: NfsHydrationReadStatus::from(snapshot.hydration),
        }
    }
}

impl From<NfsProtocolStatsSnapshot> for NfsProtocolStatus {
    fn from(snapshot: NfsProtocolStatsSnapshot) -> Self {
        Self {
            read_rpcs: snapshot.read_rpcs,
            read_requested_bytes: snapshot.read_requested_bytes,
            read_returned_bytes: snapshot.read_returned_bytes,
            read_size_le_4k: snapshot.read_size_le_4k,
            read_size_le_64k: snapshot.read_size_le_64k,
            read_size_le_1m: snapshot.read_size_le_1m,
            read_size_gt_1m: snapshot.read_size_gt_1m,
            readdirplus_rpcs: snapshot.readdirplus_rpcs,
            readdirplus_entries: snapshot.readdirplus_entries,
            readdirplus_materialized_entries: snapshot.readdirplus_materialized_entries,
            readdirplus_returned_candidates: snapshot.readdirplus_returned_candidates,
            readdirplus_attr_resolutions: snapshot.readdirplus_attr_resolutions,
            readdirplus_prefetch_paths: snapshot.readdirplus_prefetch_paths,
            readdirplus_cookie_resumes: snapshot.readdirplus_cookie_resumes,
            readdirplus_cookie_misses: snapshot.readdirplus_cookie_misses,
            readdirplus_skipped_entries: snapshot.readdirplus_skipped_entries,
            readdirplus_large_dirs: snapshot.readdirplus_large_dirs,
            readdirplus_prefetch_errors: snapshot.readdirplus_prefetch_errors,
        }
    }
}

impl From<ReadLeasePoolSnapshot> for NfsReadLeasePoolStatus {
    fn from(snapshot: ReadLeasePoolSnapshot) -> Self {
        Self {
            entries: snapshot.entries,
            max_entries: snapshot.max_entries,
            estimated_bytes: snapshot.estimated_bytes,
            max_estimated_bytes: snapshot.max_estimated_bytes,
            pinned_entries: snapshot.pinned_entries,
            active_pins: snapshot.active_pins,
            temporary_overflows: snapshot.temporary_overflows,
            hits: snapshot.hits,
            misses: snapshot.misses,
            evictions: snapshot.evictions,
            stale_retries: snapshot.stale_retries,
        }
    }
}

impl From<NfsDirectoryPageCacheSnapshot> for NfsDirectoryPageCacheStatus {
    fn from(snapshot: NfsDirectoryPageCacheSnapshot) -> Self {
        Self {
            entries: snapshot.entries,
            max_entries: snapshot.max_entries,
            estimated_bytes: snapshot.estimated_bytes,
            max_estimated_bytes: snapshot.max_estimated_bytes,
            hits: snapshot.hits,
            misses: snapshot.misses,
            evictions: snapshot.evictions,
            stale_evictions: snapshot.stale_evictions,
        }
    }
}

impl From<VfsReadMetricsSnapshot> for NfsVfsReadStatus {
    fn from(snapshot: VfsReadMetricsSnapshot) -> Self {
        Self {
            open_read_calls: snapshot.open_read_calls,
            read_at_calls: snapshot.read_at_calls,
            returned_bytes: snapshot.returned_bytes,
            stale_generation_rejections: snapshot.stale_generation_rejections,
            stale_overlay_view_rejections: snapshot.stale_overlay_view_rejections,
            stale_overlay_file_rejections: snapshot.stale_overlay_file_rejections,
            source_cache_entries: snapshot.source_cache_entries,
            source_cache_max_entries: snapshot.source_cache_max_entries,
            source_cache_estimated_bytes: snapshot.source_cache_estimated_bytes,
            source_cache_max_estimated_bytes: snapshot.source_cache_max_estimated_bytes,
            source_cache_hits: snapshot.source_cache_hits,
            resolver_calls_avoided: snapshot.resolver_calls_avoided,
            source_cache_misses: snapshot.source_cache_misses,
            source_cache_evictions: snapshot.source_cache_evictions,
            source_cache_invalidations: snapshot.source_cache_invalidations,
            source_cache_stale_evictions: snapshot.source_cache_stale_evictions,
            invalidation_path_events: snapshot.invalidation_path_events,
            invalidation_subtree_events: snapshot.invalidation_subtree_events,
            invalidation_rename_events: snapshot.invalidation_rename_events,
            invalidation_generation_events: snapshot.invalidation_generation_events,
            invalidation_overlay_reset_events: snapshot.invalidation_overlay_reset_events,
            invalidation_compacted_full_resets: snapshot.invalidation_compacted_full_resets,
            base_pointer: NfsVfsSourceReadStatus::from(snapshot.base_pointer),
            base_blob: NfsVfsSourceReadStatus::from(snapshot.base_blob),
            base_empty: NfsVfsSourceReadStatus::from(snapshot.base_empty),
            overlay_file: NfsVfsSourceReadStatus::from(snapshot.overlay_file),
        }
    }
}

impl From<VfsSourceReadMetricsSnapshot> for NfsVfsSourceReadStatus {
    fn from(snapshot: VfsSourceReadMetricsSnapshot) -> Self {
        Self {
            reads: snapshot.reads,
            bytes: snapshot.bytes,
            adaptive: NfsAdaptiveReadStatus::from(snapshot.adaptive),
        }
    }
}

impl From<VfsAdaptiveReadMetricsSnapshot> for NfsAdaptiveReadStatus {
    fn from(snapshot: VfsAdaptiveReadMetricsSnapshot) -> Self {
        Self {
            first: snapshot.first,
            sequential: snapshot.sequential,
            strided: snapshot.strided,
            repeated: snapshot.repeated,
            random: snapshot.random,
        }
    }
}

impl From<HydrationReadStatsSnapshot> for NfsHydrationReadStatus {
    fn from(snapshot: HydrationReadStatsSnapshot) -> Self {
        Self {
            read_range_requests: snapshot.read_range_requests,
            read_range_requested_bytes: snapshot.read_range_requested_bytes,
            read_range_returned_bytes: snapshot.read_range_returned_bytes,
            read_window_cache_hits: snapshot.read_window_cache_hits,
            read_window_cache_misses: snapshot.read_window_cache_misses,
            read_window_inflight_waits: snapshot.read_window_inflight_waits,
            read_window_remote_fetches: snapshot.read_window_remote_fetches,
            read_window_remote_bytes: snapshot.read_window_remote_bytes,
            read_window_prefetch_requests: snapshot.read_window_prefetch_requests,
            read_window_prefetch_scheduled: snapshot.read_window_prefetch_scheduled,
            read_window_prefetch_skipped: snapshot.read_window_prefetch_skipped,
            read_window_prefetch_errors: snapshot.read_window_prefetch_errors,
            chunk_cache_hits: snapshot.chunk_cache_hits,
            chunk_cache_misses: snapshot.chunk_cache_misses,
            chunk_inflight_waits: snapshot.chunk_inflight_waits,
            chunk_remote_fetches: snapshot.chunk_remote_fetches,
            chunk_remote_bytes: snapshot.chunk_remote_bytes,
        }
    }
}

impl From<NfsWriteJournalSnapshot> for NfsWriteJournalStatus {
    fn from(snapshot: NfsWriteJournalSnapshot) -> Self {
        Self {
            pending_paths: snapshot.pending_paths,
            oldest_dirty_age_secs: snapshot.oldest_dirty_age_secs,
            paths_with_sync_errors: snapshot.paths_with_sync_errors,
            sync_attempts: snapshot.sync_attempts,
            sync_successes: snapshot.sync_successes,
            sync_failures: snapshot.sync_failures,
            total_sync_latency_ms: snapshot.total_sync_latency_ms,
            last_sync_latency_ms: snapshot.last_sync_latency_ms,
            max_sync_latency_ms: snapshot.max_sync_latency_ms,
            poisoned: snapshot.poisoned,
            entries: snapshot
                .entries
                .into_iter()
                .map(|entry| NfsWriteJournalPathStatus {
                    path: entry.path,
                    overlay_version: entry.overlay_version,
                    last_write_stability: write_stability_name(entry.last_write_stability)
                        .to_owned(),
                    dirty_age_secs: entry.dirty_age_secs,
                    last_sync_error: entry.last_sync_error.map(|status| format!("{status:?}")),
                })
                .collect(),
        }
    }
}

fn write_stability_name(stability: NfsWriteStability) -> &'static str {
    match stability {
        NfsWriteStability::Unstable => "unstable",
        NfsWriteStability::DataSync => "data_sync",
        NfsWriteStability::FileSync => "file_sync",
    }
}

pub fn endpoint_for_mountpoint(mountpoint: &Path) -> Result<Option<String>> {
    endpoint_for_mountpoint_with_override(
        mountpoint,
        std::env::var_os(CONTROL_ENDPOINT_ENV).as_deref(),
    )
}

pub fn fresh_endpoint_for_mountpoint(mountpoint: &Path) -> Result<Option<String>> {
    generated_endpoint_for_mountpoint(mountpoint)
}

fn endpoint_for_mountpoint_with_override(
    mountpoint: &Path,
    override_endpoint: Option<&std::ffi::OsStr>,
) -> Result<Option<String>> {
    let Some(endpoint) = override_endpoint else {
        return generated_endpoint_for_mountpoint(mountpoint);
    };
    let endpoint = endpoint.to_str().ok_or_else(|| CrabError::Configuration {
        key: format!("{CONTROL_ENDPOINT_ENV} must be valid UTF-8"),
        origin: "crab mount --backend=nfs".into(),
    })?;
    validate_control_endpoint(endpoint)?;
    Ok(Some(endpoint.to_owned()))
}

fn generated_endpoint_for_mountpoint(mountpoint: &Path) -> Result<Option<String>> {
    #[cfg(unix)]
    {
        let path = unix_socket_path_for_mountpoint(mountpoint)?;
        Ok(Some(format!("unix:{}", path.display())))
    }

    #[cfg(not(unix))]
    {
        Ok(Some(tcp_endpoint_for_mountpoint(mountpoint)))
    }
}

fn validate_control_endpoint(endpoint: &str) -> Result<()> {
    if endpoint.starts_with("tcp:") {
        tcp_endpoint_from_endpoint(endpoint)?;
        return Ok(());
    }

    if endpoint.starts_with("unix:") {
        #[cfg(unix)]
        {
            unix_path_from_endpoint(endpoint)?;
            return Ok(());
        }
        #[cfg(not(unix))]
        {
            return Err(CrabError::Configuration {
                key: format!(
                    "unsupported NFS control endpoint: {}",
                    display_control_endpoint(endpoint)
                ),
                origin: "crab mount --backend=nfs".into(),
            });
        }
    }

    Err(unsupported_control_endpoint(endpoint))
}

pub fn probe_endpoint_available(endpoint: Option<&str>) -> Result<()> {
    let Some(endpoint) = endpoint else {
        return Err(CrabError::Configuration {
            key: "NFS control endpoint unavailable".into(),
            origin: "crab mount --backend=nfs".into(),
        });
    };

    if endpoint.starts_with("tcp:") {
        return probe_tcp_endpoint(endpoint);
    }

    if endpoint.starts_with("unix:") {
        #[cfg(unix)]
        {
            return probe_unix_endpoint(endpoint);
        }
        #[cfg(not(unix))]
        {
            return Err(CrabError::Configuration {
                key: format!(
                    "unsupported NFS control endpoint: {}",
                    display_control_endpoint(endpoint)
                ),
                origin: "crab mount --backend=nfs".into(),
            });
        }
    }

    Err(unsupported_control_endpoint(endpoint))
}

#[cfg(unix)]
fn probe_unix_endpoint(endpoint: &str) -> Result<()> {
    use std::os::unix::fs::FileTypeExt;
    use std::os::unix::net::{UnixListener, UnixStream};

    let path = unix_path_from_endpoint(endpoint)?;
    let _ownership = lock_unix_endpoint(&path)?;

    match std::fs::symlink_metadata(&path) {
        Ok(meta) => {
            let file_type = meta.file_type();
            if file_type.is_socket() {
                match UnixStream::connect(&path) {
                    Ok(_) => {
                        return Err(CrabError::Configuration {
                            key: format!("NFS control endpoint already has a listener: {endpoint}"),
                            origin: "crab mount --backend=nfs".into(),
                        });
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                        ) => {}
                    Err(error) => return Err(error.into()),
                }
                std::fs::remove_file(&path).map_err(CrabError::Io)?;
            } else if file_type.is_file() && meta.len() == 0 {
                std::fs::remove_file(&path).map_err(CrabError::Io)?;
            } else {
                return Err(CrabError::Configuration {
                    key: format!(
                        "NFS control endpoint path is not replaceable: {}",
                        path.display()
                    ),
                    origin: "crab mount --backend=nfs".into(),
                });
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(CrabError::Io(error)),
    }

    let listener = UnixListener::bind(&path).map_err(CrabError::Io)?;
    let owner = std::fs::symlink_metadata(&path)?;
    // The same endpoint lease covers probing, binding and unlink. It prevents
    // another process from replacing the path during the bind/listen window.
    let result = remove_owned_unix_socket(&path, &owner);
    drop(listener);
    result.map_err(CrabError::Io)
}

fn probe_tcp_endpoint(endpoint: &str) -> Result<()> {
    let endpoint = tcp_endpoint_from_endpoint(endpoint)?;
    let listener =
        std::net::TcpListener::bind(endpoint.addr).map_err(|error| CrabError::Configuration {
            key: format!(
                "NFS control endpoint {} is not bindable: {error}",
                endpoint.addr
            ),
            origin: "crab mount --backend=nfs".into(),
        })?;
    drop(listener);
    Ok(())
}

#[cfg(unix)]
fn unix_socket_path_for_mountpoint(mountpoint: &Path) -> Result<PathBuf> {
    let hash = crate::clone_cache::compute_cache_hash(&mountpoint.display().to_string());
    // Keep the default Unix socket path short: macOS rejects long
    // sockaddr_un paths, and smoke artifacts often live under deep temp roots.
    // SAFETY: getuid is a side-effect-free libc call.
    let uid = unsafe { libc::getuid() };
    Ok(PathBuf::from("/tmp")
        .join(format!("crab-nfs-{uid}"))
        .join("control")
        .join(format!("nfs-{hash}.sock")))
}

#[cfg(any(not(unix), test))]
fn tcp_endpoint_for_mountpoint(mountpoint: &Path) -> String {
    let mountpoint = mountpoint.display().to_string();
    let hash = crate::clone_cache::compute_cache_hash(&mountpoint);
    let port = tcp_control_port_from_hash(&hash);
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    format!("tcp:{addr}?token={}", random_control_token())
}

#[cfg(any(not(unix), test))]
fn random_control_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill(&mut bytes);
    let mut token = String::with_capacity(bytes.len() * 2);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in bytes {
        token.push(HEX[usize::from(byte >> 4)] as char);
        token.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    token
}

#[cfg(any(not(unix), test))]
fn tcp_control_port_from_hash(hash: &str) -> u16 {
    let prefix = hash.get(..4).unwrap_or_default();
    let value = u16::from_str_radix(prefix, 16).unwrap_or(0);
    TCP_CONTROL_PORT_BASE + (value % TCP_CONTROL_PORT_RANGE)
}

/// Bound control endpoint retained through startup and serving.
pub struct BoundControlListener {
    listener: ControlListener,
}

enum ControlListener {
    Tcp(tokio::net::TcpListener, String),
    #[cfg(unix)]
    Unix(tokio::net::UnixListener, UnixEndpointLease),
}

impl BoundControlListener {
    /// Serve an already bound endpoint until cancellation and handler drain.
    pub fn spawn(
        self,
        state: NfsControlState,
        cancel: CancellationToken,
    ) -> tokio::task::JoinHandle<Result<()>> {
        tokio::spawn(async move {
            let result = match self.listener {
                ControlListener::Tcp(listener, token) => {
                    run_tcp_listener(listener, token, state, cancel).await
                }
                #[cfg(unix)]
                ControlListener::Unix(listener, ownership) => {
                    run_unix_listener(listener, ownership, state, cancel).await
                }
            };
            if let Err(error) = &result {
                warn!(%error, "NFS control server stopped with error");
            }
            result
        })
    }
}

/// Bind and own a control endpoint before native mount visibility.
pub async fn bind_server(endpoint: &str) -> Result<BoundControlListener> {
    if endpoint.starts_with("tcp:") {
        let endpoint = tcp_endpoint_from_endpoint(endpoint)?;
        let listener = tokio::net::TcpListener::bind(endpoint.addr).await?;
        return Ok(BoundControlListener {
            listener: ControlListener::Tcp(listener, endpoint.token),
        });
    }

    if endpoint.starts_with("unix:") {
        #[cfg(unix)]
        {
            let path = unix_path_from_endpoint(endpoint)?;
            let mut ownership = lock_unix_endpoint(&path)?;
            // Preflight owns stale cleanup; binding cannot unlink a new owner.
            let listener = tokio::net::UnixListener::bind(&path)?;
            let owner = std::fs::symlink_metadata(&path)?;
            ownership.socket = Some((path, owner));
            info!(endpoint, "NFS control server listening");
            return Ok(BoundControlListener {
                listener: ControlListener::Unix(listener, ownership),
            });
        }
        #[cfg(not(unix))]
        {
            return Err(CrabError::Configuration {
                key: format!(
                    "unsupported NFS control endpoint: {}",
                    display_control_endpoint(endpoint)
                ),
                origin: "crab mount --backend=nfs".into(),
            });
        }
    }

    Err(CrabError::Configuration {
        key: format!(
            "unsupported NFS control endpoint: {}",
            display_control_endpoint(endpoint)
        ),
        origin: "crab mount --backend=nfs".into(),
    })
}

#[cfg(unix)]
async fn run_unix_listener(
    listener: tokio::net::UnixListener,
    mut ownership: UnixEndpointLease,
    state: NfsControlState,
    cancel: CancellationToken,
) -> Result<()> {
    let mut connections = JoinSet::new();
    let result = loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => break Ok(()),
            Some(result) = connections.join_next(), if !connections.is_empty() => {
                if let Err(error) = result {
                    break Err(CrabError::ReadTask(error));
                }
            }
            accepted = listener.accept() => {
                let (stream, _) = match accepted {
                    Ok(connection) => connection,
                    Err(error) => break Err(error.into()),
                };
                let state = state.clone();
                let cancel = cancel.clone();
                connections.spawn(async move {
                    let (reader, writer) = stream.into_split();
                    if let Err(error) = handle_connection(reader, writer, state, cancel, None).await {
                        debug!(error = %error, "NFS control connection ended");
                    }
                });
            }
        }
    };
    cancel.cancel();
    let drained = drain_control_connections(&mut connections).await;
    // Retain endpoint ownership through connection drain and unlink. Neither
    // preflight nor another server may replace this path before cleanup finishes.
    let cleanup = ownership.clear_socket().map_err(CrabError::Io);
    drop(listener);
    drop(ownership);
    result.and(drained).and(cleanup)
}

#[cfg(unix)]
struct UnixEndpointLease {
    file: std::fs::File,
    socket: Option<(PathBuf, std::fs::Metadata)>,
}

#[cfg(unix)]
impl UnixEndpointLease {
    fn clear_socket(&mut self) -> std::io::Result<()> {
        match self.socket.take() {
            Some((path, owner)) => remove_owned_unix_socket(&path, &owner),
            None => Ok(()),
        }
    }
}

#[cfg(unix)]
impl Drop for UnixEndpointLease {
    fn drop(&mut self) {
        // A prepared listener can be dropped before its serving task starts.
        // Retire only its socket while the endpoint lease is still exclusive.
        if let Err(error) = self.clear_socket() {
            warn!(%error, "failed to remove NFS control socket");
        }
        // A forked child can retain this open-file description until exec.
        // The serving owner joins handlers and cleans its socket before drop;
        // explicitly unlock so unrelated children cannot extend its lease.
        if let Err(error) = fs4::fs_std::FileExt::unlock(&self.file) {
            tracing::warn!(%error, "failed to release NFS control endpoint lease");
        }
    }
}

#[cfg(unix)]
fn lock_unix_endpoint(path: &Path) -> std::io::Result<UnixEndpointLease> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    // Persistent lease files must keep one inode across owners; unlinking one
    // would let a replacement bypass a process still holding its descriptor.
    let mut lock_path = path.as_os_str().to_owned();
    lock_path.push(".lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(PathBuf::from(lock_path))?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "NFS control endpoint lease is not a regular file",
        ));
    }
    if !fs4::fs_std::FileExt::try_lock_exclusive(&file)? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            "NFS control endpoint already has an owner",
        ));
    }
    Ok(UnixEndpointLease { file, socket: None })
}

#[cfg(unix)]
fn remove_owned_unix_socket(path: &Path, owner: &std::fs::Metadata) -> std::io::Result<()> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};

    let current = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if !current.file_type().is_socket()
        || current.dev() != owner.dev()
        || current.ino() != owner.ino()
    {
        return Ok(());
    }
    match std::fs::remove_file(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        result => result,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TcpControlEndpoint {
    addr: SocketAddr,
    token: String,
}

async fn run_tcp_listener(
    listener: tokio::net::TcpListener,
    token: String,
    state: NfsControlState,
    cancel: CancellationToken,
) -> Result<()> {
    let addr = listener.local_addr()?;
    info!(endpoint = %addr, "NFS TCP control server listening");
    let mut connections = JoinSet::new();
    let result = loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => break Ok(()),
            Some(result) = connections.join_next(), if !connections.is_empty() => {
                if let Err(error) = result {
                    break Err(CrabError::ReadTask(error));
                }
            }
            accepted = listener.accept() => {
                let (stream, peer) = match accepted {
                    Ok(connection) => connection,
                    Err(error) => break Err(error.into()),
                };
                if !peer.ip().is_loopback() {
                    debug!(peer = %peer, "rejected non-loopback NFS control connection");
                    continue;
                }
                let state = state.clone();
                let cancel = cancel.clone();
                let token = Some(token.clone());
                connections.spawn(async move {
                    let (reader, writer) = stream.into_split();
                    if let Err(error) = handle_connection(reader, writer, state, cancel, token).await {
                        debug!(error = %error, "NFS control connection ended");
                    }
                });
            }
        }
    };
    cancel.cancel();
    let drained = drain_control_connections(&mut connections).await;
    result.and(drained)
}

async fn drain_control_connections(connections: &mut JoinSet<()>) -> Result<()> {
    // Dropping a JoinSet aborts its tasks. Even listener failures must retain
    // admitted handlers until their blocking work and final replies finish.
    let mut result = Ok(());
    while let Some(joined) = connections.join_next().await {
        if let Err(error) = joined
            && result.is_ok()
        {
            result = Err(CrabError::ReadTask(error));
        }
    }
    result
}

async fn handle_connection<R, W>(
    reader: R,
    mut writer: W,
    state: NfsControlState,
    cancel: CancellationToken,
    token: Option<String>,
) -> std::io::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut lines = BufReader::new(reader).lines();
    loop {
        let line = tokio::select! {
            biased;
            () = cancel.cancelled() => return Ok(()),
            result = tokio::time::timeout(IDLE_TIMEOUT, lines.next_line()) => {
                match result {
                    Ok(Ok(Some(line))) => line,
                    Ok(Ok(None)) => return Ok(()),
                    Ok(Err(error)) => return Err(error),
                    Err(_) => return Ok(()),
                }
            }
        };
        let request = if let Some(expected) = &token {
            serde_json::from_str::<NfsTcpControlRequest>(&line)
                .map(|request| (request.token == *expected).then_some(request.request))
        } else {
            serde_json::from_str::<NfsControlRequest>(&line).map(Some)
        };
        let response = match request {
            Ok(Some(request)) => dispatch_request(request, &state).await,
            Ok(None) => NfsControlResponse::err("unauthorized NFS control request"),
            Err(error) => NfsControlResponse::err(format!("invalid request: {error}")),
        };
        // Admitted operations and their final replies finish after cancellation,
        // including the shutdown acknowledgement needed by the CLI cache handoff.
        write_response(&mut writer, &response).await?;
    }
}

async fn dispatch_request(
    request: NfsControlRequest,
    state: &NfsControlState,
) -> NfsControlResponse {
    match request {
        NfsControlRequest::Ping => NfsControlResponse::ping_ok(),
        NfsControlRequest::Status => match NfsControlStatus::from_state(state) {
            Ok(status) => NfsControlResponse::status_ok(status),
            Err(error) => NfsControlResponse::err(error.to_string()),
        },
        NfsControlRequest::Refresh => match refresh_runtime(state).await {
            Ok(update) => NfsControlResponse::update_ok(update),
            Err(error) => NfsControlResponse::err(error.to_string()),
        },
        NfsControlRequest::SwitchRef { r#ref } => match switch_runtime(state, r#ref).await {
            Ok(update) => NfsControlResponse::update_ok(update),
            Err(error) => NfsControlResponse::err(error.to_string()),
        },
        NfsControlRequest::DiffOverlay => match inspect_overlay_runtime(state).await {
            Ok(diff) => NfsControlResponse::overlay_diff_ok(diff),
            Err(error) => NfsControlResponse::err(error.to_string()),
        },
        NfsControlRequest::ExportOverlay { destination } => {
            match export_overlay_runtime(state, destination).await {
                Ok(diff) => NfsControlResponse::overlay_diff_ok(diff),
                Err(error) => NfsControlResponse::err(error.to_string()),
            }
        }
        NfsControlRequest::ResetOverlay => match reset_overlay_runtime(state).await {
            Ok(diff) => NfsControlResponse::overlay_diff_ok(diff),
            Err(error) => NfsControlResponse::err(error.to_string()),
        },
        NfsControlRequest::Commit { message, push } => {
            match commit_runtime(state, message, push).await {
                Ok(result) => NfsControlResponse::commit_ok(result),
                Err(error) => NfsControlResponse::err(error.to_string()),
            }
        }
        NfsControlRequest::Shutdown => {
            let cache_dir = state
                .runtime
                .as_ref()
                .map(|runtime| {
                    lock_runtime(runtime).map(|runtime| runtime.config.cache_dir.clone())
                })
                .transpose();
            match cache_dir {
                Ok(cache_dir) => {
                    state.shutdown.cancel();
                    NfsControlResponse::shutdown_ok(cache_dir)
                }
                Err(error) => NfsControlResponse::err(error.to_string()),
            }
        }
    }
}

fn overlay_target(
    state: &NfsControlState,
) -> Result<(Arc<VfsEngine>, Arc<crate::overlay::OverlayStore>, PathBuf)> {
    if state.read_only {
        return Err(CrabError::Forbidden {
            path: format!("read-only mount: {}", state.mountpoint.display()),
        });
    }
    let runtime = state
        .runtime
        .as_ref()
        .ok_or_else(|| CrabError::Internal("NFS overlay runtime unavailable".into()))?;
    let runtime = lock_runtime(runtime)?;
    let overlay = runtime
        .output
        .overlay
        .clone()
        .ok_or_else(|| CrabError::Internal("NFS overlay unavailable".into()))?;
    Ok((
        Arc::clone(&runtime.output.engine),
        overlay,
        runtime.config.cache_dir.clone(),
    ))
}

async fn inspect_overlay_runtime(state: &NfsControlState) -> Result<crate::publish::OverlayDiff> {
    let (engine, overlay, _) = overlay_target(state)?;
    crate::mount_runtime::inspect_overlay_runtime(engine, overlay, state.mountpoint.clone()).await
}

async fn export_overlay_runtime(
    state: &NfsControlState,
    destination: PathBuf,
) -> Result<crate::publish::OverlayDiff> {
    let (engine, overlay, cache_dir) = overlay_target(state)?;
    crate::mount_runtime::export_overlay_runtime(
        engine,
        overlay,
        cache_dir,
        state.mountpoint.clone(),
        destination,
    )
    .await
}

async fn reset_overlay_runtime(state: &NfsControlState) -> Result<crate::publish::OverlayDiff> {
    let engine = Arc::clone(
        state
            .engine
            .as_ref()
            .ok_or_else(|| CrabError::Internal("NFS reset engine unavailable".into()))?,
    );
    let state = state.clone();
    engine
        .run_control(async move { reset_overlay_runtime_owned(&state).await })
        .await
}

async fn reset_overlay_runtime_owned(
    state: &NfsControlState,
) -> Result<crate::publish::OverlayDiff> {
    if state.read_only {
        return Err(CrabError::Forbidden {
            path: format!("read-only mount: {}", state.mountpoint.display()),
        });
    }
    let runtime = state
        .runtime
        .as_ref()
        .ok_or_else(|| CrabError::Internal("NFS control reset unavailable".into()))?;
    let engine = state
        .engine
        .as_ref()
        .ok_or_else(|| CrabError::Internal("NFS control reset engine unavailable".into()))?;
    let _reset = crate::mount_runtime::begin_mount_update(engine, &state.mountpoint).await?;
    state.write_journal.sync_all(engine)?;
    let (output, _) = snapshot_runtime(runtime).await?;
    let overlay = output
        .overlay
        .ok_or_else(|| CrabError::Internal("NFS control reset overlay unavailable".into()))?;
    let diff = tokio::task::spawn_blocking(move || crate::publish::reset_overlay_store(&overlay))
        .await
        .map_err(|error| CrabError::Internal(format!("NFS reset task failed: {error}")))??;
    invalidate_generation_caches(&state.read_leases, &state.directory_pages);
    Ok(diff)
}

async fn commit_runtime(
    state: &NfsControlState,
    message: String,
    push: bool,
) -> Result<crate::publish::OverlayCommitResult> {
    let engine = Arc::clone(
        state
            .engine
            .as_ref()
            .ok_or_else(|| CrabError::Internal("NFS commit engine unavailable".into()))?,
    );
    let state = state.clone();
    engine
        .run_control(async move { commit_runtime_owned(&state, message, push).await })
        .await
}

async fn commit_runtime_owned(
    state: &NfsControlState,
    message: String,
    push: bool,
) -> Result<crate::publish::OverlayCommitResult> {
    if state.read_only {
        return Err(CrabError::Forbidden {
            path: format!("read-only mount: {}", state.mountpoint.display()),
        });
    }
    let runtime = state
        .runtime
        .as_ref()
        .ok_or_else(|| CrabError::Internal("NFS control commit unavailable".into()))?;
    let engine = state
        .engine
        .as_ref()
        .ok_or_else(|| CrabError::Internal("NFS control commit engine unavailable".into()))?;
    let engine = Arc::clone(engine);
    let reset = crate::mount_runtime::begin_mount_update(&engine, &state.mountpoint).await?;
    state.write_journal.sync_all(&engine)?;
    let (mut output, config) = snapshot_runtime(runtime).await?;
    let result =
        crate::mount_runtime::commit_mount_runtime(&mut output, &config, &reset, message, push)
            .await?;
    if result.commit_oid.is_some() {
        lock_runtime(runtime)?.output = output;
    }

    if result.commit_oid.is_some() {
        invalidate_generation_caches(&state.read_leases, &state.directory_pages);
    }
    Ok(result)
}

async fn refresh_runtime(state: &NfsControlState) -> Result<NfsControlUpdate> {
    let runtime = state
        .runtime
        .as_ref()
        .ok_or_else(|| CrabError::Internal("NFS control refresh unavailable".into()))?;
    let (mut output, config) = snapshot_runtime(runtime).await?;
    let update =
        crate::mount_runtime::refresh_mount_runtime(&mut output, &config, &state.mountpoint)
            .await?;
    if output.resolver.generation() == update.generation {
        lock_runtime(runtime)?.output = output;
    }
    invalidate_generation_caches(&state.read_leases, &state.directory_pages);
    Ok(NfsControlUpdate {
        mountpoint: state.mountpoint.display().to_string(),
        head_oid: update.head_oid,
        head_ref: update.head_ref,
        generation: update.generation,
    })
}

pub(crate) fn spawn_auto_refresh(
    state: NfsControlState,
    interval: Duration,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            tokio::select! {
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(interval) => {}
            }
            match refresh_runtime(&state).await {
                Ok(update) => debug!(
                    generation = update.generation,
                    head_oid = %update.head_oid,
                    "NFS mount auto-refresh completed"
                ),
                Err(error) => warn!(error = %error, "NFS mount auto-refresh failed"),
            }
        }
    })
}

async fn switch_runtime(state: &NfsControlState, new_ref: String) -> Result<NfsControlUpdate> {
    let runtime = state
        .runtime
        .as_ref()
        .ok_or_else(|| CrabError::Internal("NFS control switch unavailable".into()))?;
    let (mut output, mut config) = snapshot_runtime(runtime).await?;
    let update = crate::mount_runtime::switch_mount_runtime(
        &mut output,
        &mut config,
        &state.mountpoint,
        &new_ref,
    )
    .await?;
    if output.resolver.generation() == update.generation {
        let mut runtime = lock_runtime(runtime)?;
        runtime.output = output;
        runtime.config = config;
    }
    invalidate_generation_caches(&state.read_leases, &state.directory_pages);
    Ok(NfsControlUpdate {
        mountpoint: state.mountpoint.display().to_string(),
        head_oid: update.head_oid,
        head_ref: update.head_ref,
        generation: update.generation,
    })
}

fn invalidate_generation_caches(
    read_leases: &ReadLeasePool,
    directory_pages: &NfsDirectoryPageCache,
) {
    // Refresh and switch change the VFS generation globally. Clear protocol
    // caches at that boundary so the first native-client reads do not churn
    // through stale lease retries before repopulating hot entries.
    read_leases.invalidate_all();
    directory_pages.invalidate_all();
}

async fn snapshot_runtime(
    runtime: &Arc<Mutex<NfsMountRuntime>>,
) -> Result<(PipelineOutput, PipelineConfig)> {
    let runtime = Arc::clone(runtime);
    // A contended synchronous runtime lock must not occupy a Tokio worker;
    // teardown needs that worker to cancel and join the admitted operation.
    tokio::task::spawn_blocking(move || {
        let runtime = lock_runtime(&runtime)?;
        Ok((runtime.output.clone(), runtime.config.clone()))
    })
    .await
    .map_err(CrabError::ReadTask)?
}

fn runtime_head(runtime: &Arc<Mutex<NfsMountRuntime>>) -> Result<(Option<String>, Option<String>)> {
    let runtime = lock_runtime(runtime)?;
    Ok(match runtime.output.resolver.head() {
        Some((oid, selected_ref)) => (Some(oid), Some(selected_ref)),
        None => (None, None),
    })
}

fn lock_runtime(
    runtime: &Arc<Mutex<NfsMountRuntime>>,
) -> Result<std::sync::MutexGuard<'_, NfsMountRuntime>> {
    runtime
        .lock()
        .map_err(|_| CrabError::Internal("NFS mount runtime mutex was poisoned".into()))
}

async fn write_response<W>(writer: &mut W, response: &NfsControlResponse) -> std::io::Result<()>
where
    W: AsyncWriteExt + Unpin,
{
    let mut json = serde_json::to_vec(response).map_err(std::io::Error::other)?;
    json.push(b'\n');
    let write = async {
        writer.write_all(&json).await?;
        writer.flush().await
    };
    tokio::time::timeout(REQUEST_TIMEOUT, write)
        .await
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::TimedOut, error))?
}

pub async fn status(endpoint: &str) -> Result<NfsControlStatus> {
    let response = request(endpoint, &NfsControlRequest::Status).await?;
    if !response.ok {
        return Err(CrabError::Internal(
            response
                .error
                .unwrap_or_else(|| "NFS control status failed".to_owned()),
        ));
    }
    response
        .status
        .ok_or_else(|| CrabError::Internal("NFS control response missing status".into()))
}

pub async fn ping(endpoint: &str) -> Result<()> {
    let response = request(endpoint, &NfsControlRequest::Ping).await?;
    if response.ok {
        return Ok(());
    }
    Err(CrabError::Internal(
        response
            .error
            .unwrap_or_else(|| "NFS control ping failed".to_owned()),
    ))
}

/// Request shutdown and return the running helper's cache identity when present.
pub async fn shutdown(endpoint: &str) -> Result<Option<PathBuf>> {
    let response = request(endpoint, &NfsControlRequest::Shutdown).await?;
    if response.ok {
        return Ok(response.cache_dir);
    }
    Err(CrabError::Internal(response.error.unwrap_or_else(|| {
        "NFS control shutdown failed".to_owned()
    })))
}

pub async fn refresh(endpoint: &str) -> Result<NfsControlUpdate> {
    let response = request(endpoint, &NfsControlRequest::Refresh).await?;
    control_update_from_response(response, "NFS control refresh failed")
}

pub async fn switch_ref(endpoint: &str, git_ref: &str) -> Result<NfsControlUpdate> {
    let response = request(
        endpoint,
        &NfsControlRequest::SwitchRef {
            r#ref: git_ref.to_owned(),
        },
    )
    .await?;
    control_update_from_response(response, "NFS control switch failed")
}

/// Inspect the running helper's writable overlay state.
pub async fn inspect_overlay(endpoint: &str) -> Result<crate::publish::OverlayDiff> {
    let response = request(endpoint, &NfsControlRequest::DiffOverlay).await?;
    overlay_diff_from_response(response)
}

/// Export the running helper's overlay under its owned read boundary.
pub async fn export_overlay(
    endpoint: &str,
    destination: &Path,
) -> Result<crate::publish::OverlayDiff> {
    let request = NfsControlRequest::ExportOverlay {
        destination: destination.to_owned(),
    };
    let response = request_with_timeout(endpoint, &request, PUBLISH_REQUEST_TIMEOUT).await?;
    overlay_diff_from_response(response)
}

pub async fn reset_overlay(endpoint: &str) -> Result<crate::publish::OverlayDiff> {
    let response = request(endpoint, &NfsControlRequest::ResetOverlay).await?;
    overlay_diff_from_response(response)
}

fn overlay_diff_from_response(response: NfsControlResponse) -> Result<crate::publish::OverlayDiff> {
    if !response.ok {
        return Err(CrabError::Internal(
            response
                .error
                .unwrap_or_else(|| "NFS overlay request failed".to_owned()),
        ));
    }
    response
        .overlay_diff
        .ok_or_else(|| CrabError::Internal("NFS control response missing overlay diff".into()))
}

pub async fn commit(
    endpoint: &str,
    message: &str,
    push: bool,
) -> Result<crate::publish::OverlayCommitResult> {
    let response = request_with_timeout(
        endpoint,
        &NfsControlRequest::Commit {
            message: message.to_owned(),
            push,
        },
        PUBLISH_REQUEST_TIMEOUT,
    )
    .await?;
    if !response.ok {
        return Err(CrabError::Internal(
            response
                .error
                .unwrap_or_else(|| "NFS control commit failed".to_owned()),
        ));
    }
    response
        .commit_result
        .ok_or_else(|| CrabError::Internal("NFS control response missing commit result".into()))
}

fn control_update_from_response(
    response: NfsControlResponse,
    default_error: &str,
) -> Result<NfsControlUpdate> {
    if !response.ok {
        return Err(CrabError::Internal(
            response.error.unwrap_or_else(|| default_error.to_owned()),
        ));
    }
    response
        .update
        .ok_or_else(|| CrabError::Internal("NFS control response missing update".into()))
}

async fn request(endpoint: &str, request: &NfsControlRequest) -> Result<NfsControlResponse> {
    request_with_timeout(endpoint, request, REQUEST_TIMEOUT).await
}

async fn request_with_timeout(
    endpoint: &str,
    request: &NfsControlRequest,
    request_timeout: Duration,
) -> Result<NfsControlResponse> {
    // A stalled helper can stop reading before a response is possible. Bound the
    // entire exchange; dropping it closes this request's socket without a retry.
    tokio::time::timeout(request_timeout, async {
        if endpoint.starts_with("tcp:") {
            let endpoint = tcp_endpoint_from_endpoint(endpoint)?;
            let stream = tokio::net::TcpStream::connect(endpoint.addr).await?;
            let request = NfsTcpControlRequest {
                token: endpoint.token,
                request: request.clone(),
            };
            return exchange_request(stream, &request).await;
        }

        #[cfg(unix)]
        if endpoint.starts_with("unix:") {
            let path = unix_path_from_endpoint(endpoint)?;
            let stream = tokio::net::UnixStream::connect(path).await?;
            return exchange_request(stream, request).await;
        }

        Err(unsupported_control_endpoint(endpoint))
    })
    .await
    .map_err(|_| CrabError::Internal("timed out waiting for NFS control exchange".into()))?
}

async fn exchange_request(
    mut stream: impl AsyncRead + AsyncWrite + Unpin,
    request: &impl Serialize,
) -> Result<NfsControlResponse> {
    let mut json = serde_json::to_string(request).map_err(|error| {
        CrabError::Internal(format!("failed to serialize NFS control request: {error}"))
    })?;
    json.push('\n');
    stream.write_all(json.as_bytes()).await?;
    stream.flush().await?;

    let line = BufReader::new(stream)
        .lines()
        .next_line()
        .await?
        .ok_or_else(|| CrabError::Internal("NFS control server closed connection".into()))?;
    serde_json::from_str(&line).map_err(|error| {
        CrabError::Internal(format!("failed to parse NFS control response: {error}"))
    })
}

fn tcp_endpoint_from_endpoint(endpoint: &str) -> Result<TcpControlEndpoint> {
    let value = endpoint
        .strip_prefix("tcp:")
        .ok_or_else(|| unsupported_control_endpoint(endpoint))?;
    let (addr, token) = value
        .split_once("?token=")
        .ok_or_else(|| unsupported_control_endpoint(endpoint))?;
    if token.is_empty() {
        return Err(unsupported_control_endpoint(endpoint));
    }
    let addr = addr
        .parse::<SocketAddr>()
        .map_err(|_| unsupported_control_endpoint(endpoint))?;
    if !addr.ip().is_loopback() {
        return Err(CrabError::Configuration {
            key: format!(
                "NFS control endpoint must be loopback: {}",
                display_control_endpoint(endpoint)
            ),
            origin: "crab mount --backend=nfs".into(),
        });
    }
    Ok(TcpControlEndpoint {
        addr,
        token: token.to_owned(),
    })
}

fn unsupported_control_endpoint(endpoint: &str) -> CrabError {
    CrabError::Configuration {
        key: format!(
            "unsupported NFS control endpoint: {}",
            display_control_endpoint(endpoint)
        ),
        origin: "crab mount --backend=nfs".into(),
    }
}

fn display_control_endpoint(endpoint: &str) -> String {
    let Some(value) = endpoint.strip_prefix("tcp:") else {
        return endpoint.to_owned();
    };
    let Some((addr, _token)) = value.split_once("?token=") else {
        return endpoint.to_owned();
    };
    format!("tcp:{addr}?token=<redacted>")
}

#[cfg(unix)]
fn unix_path_from_endpoint(endpoint: &str) -> Result<PathBuf> {
    let path = endpoint
        .strip_prefix("unix:")
        .ok_or_else(|| CrabError::Configuration {
            key: format!(
                "unsupported NFS control endpoint: {}",
                display_control_endpoint(endpoint)
            ),
            origin: "crab mount --backend=nfs".into(),
        })?;
    Ok(PathBuf::from(path))
}

#[cfg(test)]
#[path = "nfs_control_connection_tests.rs"]
mod connection_lifecycle_tests;

#[cfg(test)]
#[expect(
    clippy::unwrap_used,
    reason = "test setup and assertions fail on unexpected errors"
)]
mod tests {
    use super::*;
    use crate::engine::{ReadSourceKey, VfsReadLease};

    #[test]
    fn control_request_uses_snake_case_tag() {
        let json = serde_json::to_string(&NfsControlRequest::Shutdown).unwrap();
        assert_eq!(json, r#"{"op":"shutdown"}"#);

        let json = serde_json::to_string(&NfsControlRequest::Refresh).unwrap();
        assert_eq!(json, r#"{"op":"refresh"}"#);

        let json = serde_json::to_string(&NfsControlRequest::SwitchRef {
            r#ref: "main".to_owned(),
        })
        .unwrap();
        assert_eq!(json, r#"{"op":"switch_ref","ref":"main"}"#);

        let json = serde_json::to_string(&NfsControlRequest::ResetOverlay).unwrap();
        assert_eq!(json, r#"{"op":"reset_overlay"}"#);

        let json = serde_json::to_string(&NfsControlRequest::Commit {
            message: "mounted commit".to_owned(),
            push: true,
        })
        .unwrap();
        assert_eq!(
            json,
            r#"{"op":"commit","message":"mounted commit","push":true}"#
        );
    }

    #[test]
    fn generation_cache_invalidation_clears_protocol_read_leases() {
        let read_leases = ReadLeasePool::new(4, usize::MAX);
        let lease = VfsReadLease::for_test(ReadSourceKey::BaseEmpty {
            generation: 1,
            overlay_version: 0,
            path: "models/weights.bin".to_owned(),
        });
        drop(read_leases.insert_and_pin(42, lease));
        let directory_pages = NfsDirectoryPageCache::new(4, 4096);

        invalidate_generation_caches(&read_leases, &directory_pages);

        let snapshot = read_leases.snapshot();
        assert_eq!(snapshot.entries, 0);
        assert_eq!(snapshot.evictions, 1);
    }

    #[test]
    #[cfg(unix)]
    fn endpoint_for_mountpoint_is_deterministic_unix_socket() {
        let first = endpoint_for_mountpoint(Path::new("/tmp/crab-view"))
            .unwrap()
            .unwrap();
        let second = endpoint_for_mountpoint(Path::new("/tmp/crab-view"))
            .unwrap()
            .unwrap();

        assert_eq!(first, second);
        assert!(first.starts_with("unix:"));
        assert!(first.contains("/tmp/crab-nfs-"));
        assert!(first.contains("/control/nfs-"));
    }

    #[test]
    #[cfg(unix)]
    fn endpoint_for_mountpoint_stays_short_for_deep_mountpoints() {
        let mountpoint = Path::new(
            "/private/tmp/crab-mount-nfs-macos-smoke/mount-nfs-macos-20260707-211506/Crab Mount",
        );

        let endpoint = endpoint_for_mountpoint(mountpoint).unwrap().unwrap();
        let path = unix_path_from_endpoint(&endpoint).unwrap();

        assert!(path.display().to_string().len() < 100);
        assert!(endpoint.contains("/tmp/crab-nfs-"));
    }

    #[test]
    fn tcp_endpoint_for_mountpoint_uses_stable_loopback_port_and_random_token() {
        let first = tcp_endpoint_for_mountpoint(Path::new("Z:\\Crab View"));
        let second = tcp_endpoint_for_mountpoint(Path::new("Z:\\Crab View"));

        let first = tcp_endpoint_from_endpoint(&first).unwrap();
        let second = tcp_endpoint_from_endpoint(&second).unwrap();

        assert_eq!(first.addr, second.addr);
        assert!(first.addr.ip().is_loopback());
        assert!(first.addr.port() >= TCP_CONTROL_PORT_BASE);
        assert_ne!(first.token, second.token);
        assert_eq!(first.token.len(), 64);
        assert!(first.token.chars().all(|ch| ch.is_ascii_hexdigit()));
    }

    #[test]
    fn endpoint_for_mountpoint_accepts_parent_control_endpoint_override() {
        let endpoint = std::ffi::OsString::from("tcp:127.0.0.1:50000?token=parent-secret");

        let result = endpoint_for_mountpoint_with_override(
            Path::new("Z:\\Crab View"),
            Some(endpoint.as_os_str()),
        )
        .unwrap();

        assert_eq!(
            result.as_deref(),
            Some("tcp:127.0.0.1:50000?token=parent-secret")
        );
    }

    #[test]
    fn endpoint_for_mountpoint_rejects_invalid_control_endpoint_override() {
        let endpoint = std::ffi::OsString::from("tcp:192.0.2.10:50000?token=parent-secret");

        let error = endpoint_for_mountpoint_with_override(
            Path::new("Z:\\Crab View"),
            Some(endpoint.as_os_str()),
        )
        .unwrap_err()
        .to_string();

        assert!(error.contains("loopback"));
    }

    #[test]
    fn tcp_endpoint_parser_rejects_missing_token_and_non_loopback() {
        assert!(tcp_endpoint_from_endpoint("tcp:127.0.0.1:50000").is_err());
        assert!(tcp_endpoint_from_endpoint("tcp:127.0.0.1:50000?token=").is_err());
        assert!(tcp_endpoint_from_endpoint("tcp:192.0.2.10:50000?token=secret").is_err());
    }

    #[test]
    fn tcp_endpoint_errors_redact_control_token() {
        let malformed = tcp_endpoint_from_endpoint("tcp:not-an-addr?token=secret-token")
            .unwrap_err()
            .to_string();
        assert!(malformed.contains("tcp:not-an-addr?token=<redacted>"));
        assert!(!malformed.contains("secret-token"));

        let non_loopback = tcp_endpoint_from_endpoint("tcp:192.0.2.10:50000?token=secret-token")
            .unwrap_err()
            .to_string();
        assert!(non_loopback.contains("tcp:192.0.2.10:50000?token=<redacted>"));
        assert!(!non_loopback.contains("secret-token"));
    }

    #[test]
    fn control_probe_rejects_occupied_tcp_endpoint() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("tcp:{}?token=test-token", listener.local_addr().unwrap());

        let error = probe_endpoint_available(Some(&endpoint))
            .unwrap_err()
            .to_string();

        assert!(error.contains("NFS control endpoint"));
        assert!(error.contains("not bindable"));
    }

    #[test]
    #[cfg(unix)]
    fn control_probe_replaces_stale_unix_socket_before_bind() {
        const CHILD_ROOT: &str = "CRAB_VFS_TEST_STALE_SOCKET_CHILD";
        if let Some(root) = std::env::var_os(CHILD_ROOT) {
            let listener =
                std::os::unix::net::UnixListener::bind(PathBuf::from(root).join("stale.sock"))
                    .unwrap();
            drop(listener);
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("stale.sock");
        // Concurrent Git fixtures may fork while a listener is open, retaining
        // its fd until exec. Create the stale socket in an isolated process so
        // successful exit proves every listening descriptor has closed.
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "nfs_control::tests::control_probe_replaces_stale_unix_socket_before_bind",
                "--nocapture",
            ])
            .env(CHILD_ROOT, tmp.path())
            .output()
            .unwrap();
        assert!(
            child.status.success() && path.exists(),
            "stale socket setup: {}",
            String::from_utf8_lossy(&child.stderr)
        );
        let endpoint = format!("unix:{}", path.display());

        probe_endpoint_available(Some(&endpoint)).unwrap();

        assert!(!path.exists());
    }

    #[test]
    #[cfg(unix)]
    fn control_probe_replaces_empty_unix_placeholder_before_bind() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("placeholder.sock");
        std::fs::File::create(&path).unwrap();
        let endpoint = format!("unix:{}", path.display());

        probe_endpoint_available(Some(&endpoint)).unwrap();

        assert!(!path.exists());
    }

    #[test]
    #[cfg(unix)]
    fn control_probe_creates_private_unix_socket_directory() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let socket_dir = tmp.path().join("control");
        let socket_path = socket_dir.join("probe.sock");
        let endpoint = format!("unix:{}", socket_path.display());

        probe_endpoint_available(Some(&endpoint)).unwrap();

        let dir_mode = std::fs::metadata(&socket_dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);
        assert!(!socket_path.exists());
    }

    #[test]
    #[cfg(unix)]
    fn control_probe_rejects_active_unix_socket() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("active.sock");
        let _listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let endpoint = format!("unix:{}", path.display());

        let error = probe_endpoint_available(Some(&endpoint))
            .unwrap_err()
            .to_string();

        assert!(error.contains("already has a listener"));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn control_server_reports_status_accepts_shutdown_and_removes_socket() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let mountpoint = tmp.path().join("view");
        let socket_dir = tmp.path().join("control");
        let socket_path = socket_dir.join("nfs-control.sock");
        let endpoint = format!("unix:{}", socket_path.display());
        let cancel = CancellationToken::new();
        let state = NfsControlState {
            shutdown: cancel.clone(),
            mountpoint: mountpoint.clone(),
            read_only: true,
            read_leases: ReadLeasePool::new(4, 4096),
            directory_pages: NfsDirectoryPageCache::new(4, 4096),
            write_journal: Arc::new(NfsWriteJournal::new()),
            protocol_stats: Arc::new(NfsProtocolStats::new()),
            engine: None,
            runtime: None,
            lifecycle: NfsMountLifecycleStatus {
                server_bind_ms: 3,
                native_mount_ms: 5,
                startup_ms: 8,
            },
        };
        let handle = bind_server(&endpoint)
            .await
            .unwrap()
            .spawn(state, cancel.clone());

        let status = status(&endpoint).await.unwrap();

        let dir_mode = std::fs::metadata(&socket_dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);
        assert_eq!(status.mountpoint, mountpoint.display().to_string());
        assert!(status.read_only);
        assert_eq!(status.head_oid, None);
        assert_eq!(status.head_ref, None);
        assert_eq!(status.runtime.read_leases.entries, 0);
        assert_eq!(status.runtime.directory_pages.entries, 0);
        assert_eq!(status.runtime.directory_pages.hits, 0);
        assert_eq!(status.runtime.protocol.read_rpcs, 0);
        assert_eq!(status.runtime.vfs.read_at_calls, 0);
        assert_eq!(status.runtime.hydration.read_range_requests, 0);
        assert_eq!(status.runtime.write_journal.pending_paths, 0);
        assert_eq!(status.runtime.write_journal.sync_attempts, 0);
        assert_eq!(status.runtime.write_journal.last_sync_latency_ms, None);
        assert_eq!(status.runtime.lifecycle.server_bind_ms, 3);
        assert_eq!(status.runtime.lifecycle.native_mount_ms, 5);
        assert_eq!(status.runtime.lifecycle.startup_ms, 8);

        shutdown(&endpoint).await.unwrap();
        assert!(cancel.is_cancelled());
        handle.await.unwrap().unwrap();
        assert!(!socket_path.exists());
    }

    #[tokio::test]
    async fn tcp_control_server_requires_token_and_accepts_shutdown() {
        let tmp = tempfile::tempdir().unwrap();
        let mountpoint = tmp.path().join("view");
        let listener = tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .await
            .unwrap();
        let addr = listener.local_addr().unwrap();
        let endpoint = format!("tcp:{addr}?token=test-token");
        let cancel = CancellationToken::new();
        let state = NfsControlState {
            shutdown: cancel.clone(),
            mountpoint: mountpoint.clone(),
            read_only: true,
            read_leases: ReadLeasePool::new(4, 4096),
            directory_pages: NfsDirectoryPageCache::new(4, 4096),
            write_journal: Arc::new(NfsWriteJournal::new()),
            protocol_stats: Arc::new(NfsProtocolStats::new()),
            engine: None,
            runtime: None,
            lifecycle: NfsMountLifecycleStatus::default(),
        };
        let handle = tokio::spawn(run_tcp_listener(
            listener,
            "test-token".to_owned(),
            state,
            cancel.clone(),
        ));

        let rejected = request(&format!("tcp:{addr}?token=wrong"), &NfsControlRequest::Ping)
            .await
            .unwrap();
        assert!(!rejected.ok);
        assert_eq!(
            rejected.error.as_deref(),
            Some("unauthorized NFS control request")
        );

        let status = status(&endpoint).await.unwrap();

        assert_eq!(status.mountpoint, mountpoint.display().to_string());
        assert!(status.read_only);
        assert_eq!(status.runtime.read_leases.entries, 0);
        assert_eq!(status.runtime.directory_pages.entries, 0);
        assert_eq!(status.runtime.lifecycle, NfsMountLifecycleStatus::default());

        shutdown(&endpoint).await.unwrap();
        assert!(cancel.is_cancelled());
        handle.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn tcp_control_deadline_includes_stalled_writes() {
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let endpoint = format!("tcp:{}?token=test-token", listener.local_addr().unwrap());
        assert_stalled_write_times_out(&endpoint).await;
    }

    #[tokio::test]
    async fn control_response_timeout_closes_connection() {
        use tokio::io::AsyncReadExt;

        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let endpoint = format!("tcp:{}?token=test-token", listener.local_addr().unwrap());
        let (response, received) = tokio::join!(
            request_with_timeout(
                &endpoint,
                &NfsControlRequest::Ping,
                Duration::from_millis(100),
            ),
            tokio::time::timeout(Duration::from_secs(3), async {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                stream.read_to_end(&mut bytes).await.unwrap();
                serde_json::from_slice::<NfsTcpControlRequest>(&bytes).unwrap()
            }),
        );
        assert!(
            matches!(response, Err(CrabError::Internal(message)) if message.contains("timed out"))
        );
        assert_eq!(received.unwrap().request, NfsControlRequest::Ping);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_control_deadline_includes_stalled_writes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("control.sock");
        let _listener = tokio::net::UnixListener::bind(&path).unwrap();
        assert_stalled_write_times_out(&format!("unix:{}", path.display())).await;
    }

    async fn assert_stalled_write_times_out(endpoint: &str) {
        // Exceed the socket buffers while the listener deliberately never reads.
        // A response-only timeout cannot interrupt the pending request write.
        let request = NfsControlRequest::Commit {
            message: "x".repeat(8 * 1024 * 1024),
            push: false,
        };
        let result = tokio::time::timeout(
            Duration::from_secs(3),
            request_with_timeout(endpoint, &request, Duration::from_millis(100)),
        )
        .await
        .unwrap();
        assert!(
            matches!(result, Err(CrabError::Internal(message)) if message.contains("timed out"))
        );
    }
}
