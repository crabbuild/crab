//! Native NFS mount lifecycle for Crab virtual filesystems.

use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nfs3_server::tcp::{NFSTcp, NFSTcpListener};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::core::error::{CrabError, Result};
use crate::engine::VfsEngine;
use crate::nfs::{
    CrabNfsFs, NfsDirectoryPageCache, NfsProtocolStats, NfsRuntimeSnapshot, NfsWriteJournal,
};
use crate::nfs_control::{self, NfsControlState, NfsMountLifecycleStatus, NfsMountRuntime};
use crate::read_lease_pool::ReadLeasePool;
use crate::resolver::FuseResolver;

const EXPORT_NAME: &str = "crab";
#[cfg(any(target_os = "macos", target_os = "linux"))]
const NFS_METADATA_TTL_SECS: u64 = 1;
#[cfg(any(target_os = "macos", target_os = "linux"))]
const NFS_IO_SIZE: usize = 1024 * 1024;
#[cfg(any(windows, test))]
const WINDOWS_PORTMAP_PORT: u16 = 111;

/// Configuration for an NFS mount.
pub struct NfsMountConfig {
    /// Path where the OS NFS client will mount the export.
    pub mountpoint: PathBuf,
    /// Absolute path to the real `.git` directory.
    pub git_dir: String,
    /// Stable verifier store for NFS exclusive creates.
    pub exclusive_verifiers_path: PathBuf,
    /// Whether to expose a read-only NFS export.
    pub read_only: bool,
    /// Poll interval for interactive remote refreshes. Daemon-managed mounts
    /// leave this unset because the daemon owns their refresh task.
    pub auto_refresh_interval: Option<Duration>,
    /// Explicit control endpoint. When absent, derive it from the mountpoint
    /// or the helper-provided environment override.
    pub control_endpoint_override: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub struct NfsPreflightReport {
    pub backend_available: bool,
    pub native_client_available: bool,
    pub mountpoint_ready: bool,
    pub loopback_bind_ready: bool,
    pub control_endpoint_ready: bool,
    pub privilege_ready: bool,
    pub warnings: Vec<NfsPreflightMessage>,
    pub blockers: Vec<NfsPreflightMessage>,
}

#[derive(Debug, Clone, serde::Serialize, PartialEq, Eq)]
pub struct NfsPreflightMessage {
    pub key: String,
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
}

impl NfsPreflightReport {
    fn ready() -> Self {
        Self {
            backend_available: true,
            native_client_available: true,
            mountpoint_ready: true,
            loopback_bind_ready: true,
            control_endpoint_ready: true,
            privilege_ready: true,
            warnings: Vec::new(),
            blockers: Vec::new(),
        }
    }

    fn block(
        &mut self,
        key: impl Into<String>,
        detail: impl Into<String>,
        action: impl Into<String>,
    ) {
        self.blockers.push(NfsPreflightMessage {
            key: key.into(),
            detail: detail.into(),
            action: Some(action.into()),
        });
    }

    #[cfg(windows)]
    fn warn(&mut self, key: impl Into<String>, detail: impl Into<String>) {
        self.warnings.push(NfsPreflightMessage {
            key: key.into(),
            detail: detail.into(),
            action: None,
        });
    }

    pub fn is_ready(&self) -> bool {
        self.blockers.is_empty()
    }

    pub fn ensure_ready(&self) -> Result<()> {
        if self.blockers.is_empty() {
            return Ok(());
        }
        Err(CrabError::Configuration {
            key: self.blocker_summary(),
            origin: "crab mount --backend=nfs".into(),
        })
    }

    pub fn blocker_summary(&self) -> String {
        let mut summary = format!(
            "NFS preflight failed with {} blocker(s):",
            self.blockers.len()
        );
        for blocker in &self.blockers {
            summary.push_str("\n- ");
            summary.push_str(&blocker.key);
            summary.push_str(": ");
            summary.push_str(&blocker.detail);
            if let Some(action) = &blocker.action {
                summary.push_str("\n  next: ");
                summary.push_str(action);
            }
        }
        summary
    }
}

/// Running NFS server plus mount lifecycle channels.
pub struct NfsMountedSession {
    server_handle: tokio::task::JoinHandle<Result<()>>,
    mount_rx: mpsc::Receiver<bool>,
    mountpoint: PathBuf,
    engine: Arc<VfsEngine>,
    read_leases: Arc<ReadLeasePool>,
    directory_pages: Arc<NfsDirectoryPageCache>,
    write_journal: Arc<NfsWriteJournal>,
    protocol_stats: Arc<NfsProtocolStats>,
    control_endpoint: Option<String>,
    read_only: bool,
    auto_refresh_interval: Option<Duration>,
    runtime: Option<Arc<Mutex<NfsMountRuntime>>>,
    lifecycle: NfsMountLifecycleStatus,
}

impl NfsMountedSession {
    pub fn runtime_snapshot(&self) -> NfsRuntimeSnapshot {
        NfsRuntimeSnapshot {
            read_leases: self.read_leases.snapshot(),
            directory_pages: self.directory_pages.snapshot(),
            write_journal: self.write_journal.snapshot(),
            protocol: self.protocol_stats.snapshot(),
            vfs: self.engine.read_metrics_snapshot(),
            hydration: self.engine.hydration_read_stats_snapshot(),
        }
    }

    fn control_state(&self) -> NfsControlState {
        NfsControlState {
            mountpoint: self.mountpoint.clone(),
            read_only: self.read_only,
            read_leases: Arc::clone(&self.read_leases),
            directory_pages: Arc::clone(&self.directory_pages),
            write_journal: Arc::clone(&self.write_journal),
            protocol_stats: Arc::clone(&self.protocol_stats),
            engine: Some(Arc::clone(&self.engine)),
            runtime: self.runtime.clone(),
            lifecycle: self.lifecycle.clone(),
        }
    }
}

/// Start the local NFS server and mount it through the native OS client.
pub async fn mount(
    config: &NfsMountConfig,
    resolver: Arc<FuseResolver>,
    engine: Arc<VfsEngine>,
    runtime: Option<NfsMountRuntime>,
) -> Result<NfsMountedSession> {
    let startup_start = Instant::now();
    let engine_for_drain = Arc::clone(&engine);
    let adapter = CrabNfsFs::new(
        resolver,
        engine,
        &config.git_dir,
        config.read_only,
        Some(config.exclusive_verifiers_path.clone()),
    );
    let read_leases = adapter.read_lease_pool();
    let directory_pages = adapter.directory_page_cache();
    let write_journal = adapter.write_journal();
    let protocol_stats = adapter.protocol_stats();
    let control_endpoint = control_endpoint(config)?;
    let bind_start = Instant::now();
    let mut listener = NFSTcpListener::bind(nfs_listen_addr(), adapter)
        .await
        .map_err(CrabError::Io)?;
    let server_bind_ms = duration_millis(bind_start.elapsed());
    listener.with_export_name(EXPORT_NAME);
    let port = listener.get_listen_port();
    let ip = listener.get_listen_ip();

    let (mount_tx, mount_rx) = mpsc::channel::<bool>(1);
    listener.set_mount_listener(mount_tx);

    info!(ip = %ip, port, "NFS server listening");
    let server_handle =
        tokio::spawn(async move { listener.handle_forever().await.map_err(CrabError::Io) });

    let native_mount_start = Instant::now();
    if let Err(error) = mount_native_nfs(config, ip, port) {
        server_handle.abort();
        return Err(error);
    }
    let native_mount_ms = duration_millis(native_mount_start.elapsed());
    let startup_ms = duration_millis(startup_start.elapsed());
    let lifecycle = NfsMountLifecycleStatus {
        server_bind_ms,
        native_mount_ms,
        startup_ms,
    };
    info!(
        server_bind_ms,
        native_mount_ms, startup_ms, "NFS native mount ready"
    );

    Ok(NfsMountedSession {
        server_handle,
        mount_rx,
        mountpoint: config.mountpoint.clone(),
        engine: engine_for_drain,
        read_leases,
        directory_pages,
        write_journal,
        protocol_stats,
        control_endpoint,
        read_only: config.read_only,
        auto_refresh_interval: config.auto_refresh_interval,
        runtime: runtime.map(|runtime| Arc::new(Mutex::new(runtime))),
        lifecycle,
    })
}

/// Wait for cancellation or unmount, then tear down the native mount.
pub async fn run_until_cancelled(
    mut session: NfsMountedSession,
    cancel: CancellationToken,
) -> Result<()> {
    let control_state = session.control_state();
    let control_handle = nfs_control::spawn_server(
        session.control_endpoint.clone(),
        control_state.clone(),
        cancel.clone(),
    );
    let refresh_handle = session
        .auto_refresh_interval
        .map(|interval| nfs_control::spawn_auto_refresh(control_state, interval, cancel.clone()));
    let mut server_error = None;
    loop {
        tokio::select! {
            () = cancel.cancelled() => {
                info!("cancellation received, unmounting NFS filesystem");
                break;
            }
            msg = session.mount_rx.recv() => {
                match msg {
                    Some(true) => continue,
                    _ => {
                        info!("NFS unmount detected, shutting down server");
                        break;
                    }
                }
            }
            result = &mut session.server_handle => {
                match result {
                    Ok(Ok(())) => {
                        info!("NFS server exited");
                        break;
                    }
                    Ok(Err(error)) => {
                        warn!(error = %error, "NFS server exited with error");
                        server_error = Some(error);
                        break;
                    }
                    Err(error) if error.is_cancelled() => break,
                    Err(error) => {
                        server_error = Some(CrabError::Internal(format!("NFS server task failed: {error}")));
                        break;
                    }
                }
            }
            () = tokio::time::sleep(Duration::from_secs(2)) => {
                if !is_mounted(&session.mountpoint) {
                    info!(mountpoint = %session.mountpoint.display(), "NFS mount disappeared");
                    break;
                }
            }
        }
    }

    let shutdown_start = Instant::now();
    cancel.cancel();
    if let Some(handle) = refresh_handle {
        handle.abort();
    }
    if let Some(handle) = control_handle {
        handle.abort();
    }
    // Flush overlay state while the server is still available. macOS needs a
    // forced unmount because its client sends a Mount v1 teardown RPC that
    // this NFSv3 server does not implement.
    let write_journal_drain_start = Instant::now();
    let write_journal_drain_result = session.write_journal.sync_all(&session.engine);
    let write_journal_drain_ms = duration_millis(write_journal_drain_start.elapsed());
    let native_unmount_start = Instant::now();
    let mut native_unmount_attempted = false;
    if is_mounted(&session.mountpoint) {
        native_unmount_attempted = true;
        unmount_native_nfs(&session.mountpoint);
    }
    let native_unmount_ms = duration_millis(native_unmount_start.elapsed());
    session.server_handle.abort();
    let stats = session.runtime_snapshot();
    debug!(
        read_lease_entries = stats.read_leases.entries,
        read_lease_hits = stats.read_leases.hits,
        read_lease_misses = stats.read_leases.misses,
        read_lease_stale_retries = stats.read_leases.stale_retries,
        directory_page_cache_entries = stats.directory_pages.entries,
        directory_page_cache_hits = stats.directory_pages.hits,
        directory_page_cache_misses = stats.directory_pages.misses,
        directory_page_cache_stale_evictions = stats.directory_pages.stale_evictions,
        nfs_server_bind_ms = session.lifecycle.server_bind_ms,
        nfs_native_mount_ms = session.lifecycle.native_mount_ms,
        nfs_startup_ms = session.lifecycle.startup_ms,
        pending_write_paths = stats.write_journal.pending_paths,
        paths_with_sync_errors = stats.write_journal.paths_with_sync_errors,
        read_rpcs = stats.protocol.read_rpcs,
        read_requested_bytes = stats.protocol.read_requested_bytes,
        read_returned_bytes = stats.protocol.read_returned_bytes,
        readdirplus_rpcs = stats.protocol.readdirplus_rpcs,
        readdirplus_entries = stats.protocol.readdirplus_entries,
        readdirplus_materialized_entries = stats.protocol.readdirplus_materialized_entries,
        readdirplus_attr_resolutions = stats.protocol.readdirplus_attr_resolutions,
        readdirplus_cookie_resumes = stats.protocol.readdirplus_cookie_resumes,
        readdirplus_cookie_misses = stats.protocol.readdirplus_cookie_misses,
        readdirplus_skipped_entries = stats.protocol.readdirplus_skipped_entries,
        readdirplus_large_dirs = stats.protocol.readdirplus_large_dirs,
        readdirplus_prefetch_errors = stats.protocol.readdirplus_prefetch_errors,
        vfs_read_at_calls = stats.vfs.read_at_calls,
        vfs_returned_bytes = stats.vfs.returned_bytes,
        vfs_source_cache_entries = stats.vfs.source_cache_entries,
        vfs_source_cache_hits = stats.vfs.source_cache_hits,
        vfs_resolver_calls_avoided = stats.vfs.resolver_calls_avoided,
        vfs_source_cache_misses = stats.vfs.source_cache_misses,
        vfs_source_cache_invalidations = stats.vfs.source_cache_invalidations,
        vfs_source_cache_stale_evictions = stats.vfs.source_cache_stale_evictions,
        hydration_read_window_hits = stats.hydration.read_window_cache_hits,
        hydration_read_window_misses = stats.hydration.read_window_cache_misses,
        "draining NFS mount runtime state"
    );
    let shutdown_ms = duration_millis(shutdown_start.elapsed());
    info!(
        nfs_shutdown_ms = shutdown_ms,
        nfs_native_unmount_attempted = native_unmount_attempted,
        nfs_native_unmount_ms = native_unmount_ms,
        nfs_write_journal_drain_ms = write_journal_drain_ms,
        "NFS mount shutdown complete"
    );
    write_journal_drain_result?;
    if let Some(error) = server_error {
        return Err(error);
    }
    Ok(())
}

fn duration_millis(duration: Duration) -> u64 {
    duration.as_millis().min(u128::from(u64::MAX)) as u64
}

/// Install Ctrl+C/SIGTERM handling for foreground NFS mounts.
pub fn install_signal_handler(cancel: CancellationToken) {
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};

            let mut sigint = signal(SignalKind::interrupt()).ok();
            let mut sigterm = signal(SignalKind::terminate()).ok();
            tokio::select! {
                _ = async {
                    if let Some(ref mut stream) = sigint {
                        stream.recv().await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => {}
                _ = async {
                    if let Some(ref mut stream) = sigterm {
                        stream.recv().await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                } => {}
                _ = tokio::signal::ctrl_c() => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        cancel.cancel();
    });
}

fn nfs_listen_addr() -> &'static str {
    #[cfg(windows)]
    {
        windows_nfs_listen_addr()
    }
    #[cfg(not(windows))]
    {
        "127.0.0.1:0"
    }
}

#[cfg(any(windows, test))]
fn windows_nfs_listen_addr() -> &'static str {
    // Windows Client for NFS does not document per-mount NFS port options.
    // Bind a unique loopback IP on the standard portmapper port instead.
    "auto:111"
}

#[cfg(any(windows, test))]
fn validate_windows_nfs_port(ip: IpAddr, port: u16) -> Result<()> {
    if port == WINDOWS_PORTMAP_PORT {
        return Ok(());
    }
    Err(CrabError::Configuration {
        key: format!(
            "Windows Client for NFS requires Crab to listen on port {WINDOWS_PORTMAP_PORT}; helper bound {ip}:{port}"
        ),
        origin: "crab mount --backend=nfs".into(),
    })
}

pub fn preflight(config: &NfsMountConfig, ip: IpAddr, port: u16) -> NfsPreflightReport {
    let mut report = NfsPreflightReport::ready();
    check_native_client(&mut report);
    check_mountpoint(config, &mut report);
    check_loopback_endpoint(ip, port, &mut report);
    if report.mountpoint_ready && report.loopback_bind_ready {
        check_control_endpoint(config, &mut report);
    }
    check_mount_privilege(&mut report);
    report
}

pub fn preflight_for_mountpoint(mountpoint: &Path) -> NfsPreflightReport {
    let config = NfsMountConfig {
        mountpoint: mountpoint.to_path_buf(),
        git_dir: String::new(),
        exclusive_verifiers_path: PathBuf::new(),
        read_only: true,
        auto_refresh_interval: None,
        control_endpoint_override: None,
    };
    preflight_for_config(&config)
}

/// Diagnose native NFS readiness for a fully resolved mount configuration.
pub fn preflight_for_config(config: &NfsMountConfig) -> NfsPreflightReport {
    let (ip, port) = preflight_probe_endpoint();
    preflight(config, ip, port)
}

fn preflight_probe_endpoint() -> (IpAddr, u16) {
    #[cfg(windows)]
    {
        (IpAddr::from([127, 0, 0, 1]), WINDOWS_PORTMAP_PORT)
    }
    #[cfg(not(windows))]
    {
        (IpAddr::from([127, 0, 0, 1]), 0)
    }
}

fn check_native_client(report: &mut NfsPreflightReport) {
    #[cfg(target_os = "macos")]
    {
        if find_macos_mount_nfs().is_none() {
            report.native_client_available = false;
            report.block(
                "mount_nfs not found",
                "macOS NFS mounts require the native mount_nfs client",
                "Install or restore the macOS NFS client tools, then rerun crab mount --backend=nfs.",
            );
        }
    }

    #[cfg(target_os = "linux")]
    {
        if find_mount_nfs().is_none() {
            report.native_client_available = false;
            report.block(
                "mount.nfs not found",
                "Linux NFS mounts require mount.nfs from nfs-common or nfs-utils",
                "Install nfs-common on Debian/Ubuntu or nfs-utils on RHEL/Fedora, then rerun crab mount --backend=nfs.",
            );
        }
    }

    #[cfg(windows)]
    {
        if let Err(error) = windows_system_command("mount.exe") {
            report.native_client_available = false;
            report.block(
                "Windows Client for NFS mount.exe not found",
                error.to_string(),
                "Enable Windows Client for NFS, then rerun crab mount --backend=nfs.",
            );
        }
        if let Err(error) = windows_system_command("umount.exe") {
            report.warn(
                "Windows Client for NFS umount.exe not found",
                format!("unmount may require manual cleanup: {error}"),
            );
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        report.backend_available = false;
        report.native_client_available = false;
        report.block(
            "unsupported NFS platform",
            "Crab NFS mounting is currently supported on macOS, Linux, and Windows",
            "Use a supported platform or build/run Crab with the FUSE backend instead.",
        );
    }
}

fn check_mountpoint(config: &NfsMountConfig, report: &mut NfsPreflightReport) {
    #[cfg(windows)]
    {
        match windows_mount_target(&config.mountpoint) {
            Ok(target) => {
                if is_mounted(&config.mountpoint) {
                    report.mountpoint_ready = false;
                    report.block(
                        "Windows NFS drive already mounted",
                        format!("{target} is already mounted"),
                        "Choose an unused drive target such as Z: or unmount the existing drive first.",
                    );
                }
            }
            Err(error) => {
                report.mountpoint_ready = false;
                report.block(
                    "invalid Windows NFS mountpoint",
                    error.to_string(),
                    "Use an explicit drive target such as Z: for Windows NFS mounts.",
                );
            }
        }
    }

    #[cfg(not(windows))]
    {
        if let Err(error) = path_to_string(&config.mountpoint) {
            report.mountpoint_ready = false;
            report.block(
                "invalid NFS mountpoint path",
                error.to_string(),
                "Use a UTF-8 mountpoint path for Crab NFS mounts.",
            );
            return;
        }
        if !config.mountpoint.exists() {
            report.mountpoint_ready = false;
            report.block(
                "NFS mountpoint does not exist",
                format!("{}", config.mountpoint.display()),
                "Create the mountpoint directory or let `crab mount` prepare it before starting the NFS helper.",
            );
            return;
        }
        if !config.mountpoint.is_dir() {
            report.mountpoint_ready = false;
            report.block(
                "NFS mountpoint is not a directory",
                format!("{}", config.mountpoint.display()),
                "Choose an empty directory as the NFS mountpoint.",
            );
            return;
        }
        if is_mounted(&config.mountpoint) {
            report.mountpoint_ready = false;
            report.block(
                "NFS mountpoint is already mounted",
                format!("{}", config.mountpoint.display()),
                "Unmount the existing filesystem or choose another mountpoint.",
            );
        }
    }
}

fn check_loopback_endpoint(ip: IpAddr, port: u16, report: &mut NfsPreflightReport) {
    if !ip.is_loopback() {
        report.loopback_bind_ready = false;
        report.block(
            "NFS server is not bound to loopback",
            format!("helper bound {ip}:{port}"),
            "Bind Crab's NFS helper to a loopback address before invoking the native client.",
        );
    }

    #[cfg(windows)]
    if let Err(error) = validate_windows_nfs_port(ip, port) {
        report.loopback_bind_ready = false;
        report.block(
            "Windows NFS port contract failed",
            error.to_string(),
            format!("Bind Crab's Windows NFS helper on port {WINDOWS_PORTMAP_PORT}."),
        );
    }
}

fn check_control_endpoint(config: &NfsMountConfig, report: &mut NfsPreflightReport) {
    let result = control_endpoint(config)
        .and_then(|endpoint| nfs_control::probe_endpoint_available(endpoint.as_deref()));
    if let Err(error) = result {
        report.control_endpoint_ready = false;
        report.block(
            "NFS control endpoint unavailable",
            error.to_string(),
            "Choose another mountpoint or remove the stale Crab NFS control endpoint, then rerun crab mount --backend=nfs.",
        );
    }
}

fn control_endpoint(config: &NfsMountConfig) -> Result<Option<String>> {
    match &config.control_endpoint_override {
        Some(endpoint) => Ok(Some(endpoint.clone())),
        None => nfs_control::endpoint_for_mountpoint(&config.mountpoint),
    }
}

#[cfg(target_os = "linux")]
fn check_mount_privilege(report: &mut NfsPreflightReport) {
    if running_as_root() || sudo_noninteractive_available() {
        return;
    }
    report.privilege_ready = false;
    report.block(
        "Linux NFS mount permission unavailable",
        "Crab is not running as root and `sudo -n true` did not succeed",
        "Run crab mount with root/CAP_SYS_ADMIN privileges or configure passwordless sudo for mount.nfs.",
    );
}

#[cfg(not(target_os = "linux"))]
fn check_mount_privilege(_report: &mut NfsPreflightReport) {}

fn mount_native_nfs(config: &NfsMountConfig, ip: IpAddr, port: u16) -> Result<()> {
    let report = preflight(config, ip, port);
    for warning in &report.warnings {
        warn!(
            key = %warning.key,
            detail = %warning.detail,
            "NFS preflight warning"
        );
    }
    report.ensure_ready()?;

    #[cfg(windows)]
    let mountpoint = windows_mount_target(&config.mountpoint)?;
    #[cfg(not(windows))]
    let mountpoint = path_to_string(&config.mountpoint)?;

    #[cfg(target_os = "macos")]
    {
        let mount_nfs = find_macos_mount_nfs().ok_or_else(|| CrabError::Configuration {
            key: "mount_nfs not found. Restore the macOS NFS client tools.".into(),
            origin: "crab mount --backend=nfs".into(),
        })?;
        let mut opts = format!(
            "locallocks,nonegnamecache,vers=3,tcp,rsize={NFS_IO_SIZE},actimeo={NFS_METADATA_TTL_SECS},port={port},mountport={port}"
        );
        if config.read_only {
            opts = format!("rdonly,{opts}");
        } else {
            opts = format!("{opts},wsize={NFS_IO_SIZE}");
        }
        run_mount_command(Command::new(mount_nfs).args([
            "-o",
            &opts,
            &format!("{ip}:/{EXPORT_NAME}"),
            &mountpoint,
        ]))
    }

    #[cfg(target_os = "linux")]
    {
        let mount_nfs = find_mount_nfs().ok_or_else(|| {
            CrabError::Configuration {
                key: "mount.nfs not found. Install nfs-common (Debian/Ubuntu) or nfs-utils (RHEL/Fedora).".into(),
                origin: "crab mount --backend=nfs".into(),
            }
        })?;
        let mut opts = format!(
            "nolock,lookupcache=positive,vers=3,tcp,rsize={NFS_IO_SIZE},actimeo={NFS_METADATA_TTL_SECS},port={port},mountport={port}"
        );
        if config.read_only {
            opts = format!("ro,{opts}");
        } else {
            opts = format!("{opts},wsize={NFS_IO_SIZE}");
        }
        let mut command = if running_as_root() {
            Command::new(&mount_nfs)
        } else {
            let mut command = Command::new("sudo");
            command.arg("-n").arg(&mount_nfs);
            command
        };
        run_mount_command(command.args(["-o", &opts, &format!("{ip}:/{EXPORT_NAME}"), &mountpoint]))
    }

    #[cfg(windows)]
    {
        validate_windows_nfs_port(ip, port)?;
        let mount_exe = windows_system_command("mount.exe")?;
        run_mount_command(Command::new(mount_exe).args([
            "-o",
            windows_mount_options(),
            &format!("\\\\{ip}\\{EXPORT_NAME}"),
            &mountpoint,
        ]))
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        let _ = (config, ip, port);
        Err(CrabError::Configuration {
            key: "NFS mounting is not supported on this platform".into(),
            origin: "crab mount --backend=nfs".into(),
        })
    }
}

#[cfg(windows)]
pub fn is_windows_mount_target(path: &Path) -> bool {
    windows_mount_target(path).is_ok()
}

pub fn windows_mount_target(path: &Path) -> Result<String> {
    let raw = path_to_string(path)?;
    if let Some(target) = parse_windows_drive_target(&raw) {
        return Ok(target);
    }
    Err(CrabError::Configuration {
        key: "Windows NFS mountpoint".into(),
        origin: format!(
            "Windows NFS mounts require an explicit drive target such as Z:; got {}",
            path.display()
        ),
    })
}

fn parse_windows_drive_target(raw: &str) -> Option<String> {
    let target = raw.trim_end_matches(['\\', '/']);
    let bytes = target.as_bytes();
    if bytes.len() == 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return Some(target.to_ascii_uppercase());
    }
    None
}

#[cfg(any(windows, test))]
fn windows_mount_options() -> &'static str {
    "anon,nolock,mtype=hard,rsize=32,wsize=32,fileaccess=766,casesensitive"
}

#[cfg(windows)]
pub fn windows_system_command(name: &str) -> Result<PathBuf> {
    let windows_dir = std::env::var_os("WINDIR")
        .or_else(|| std::env::var_os("SystemRoot"))
        .ok_or_else(|| CrabError::Configuration {
            key: format!("Windows system directory not found while resolving {name}"),
            origin: "crab mount --backend=nfs".into(),
        })?;
    let windows_dir = PathBuf::from(windows_dir);
    let candidates = [
        windows_dir.join("System32").join(name),
        windows_dir.join("Sysnative").join(name),
    ];

    for candidate in candidates {
        if candidate.is_file() {
            return Ok(candidate);
        }
    }

    Err(CrabError::Configuration {
        key: format!("Windows Client for NFS command not found: {name}"),
        origin: "crab mount --backend=nfs".into(),
    })
}

fn run_mount_command(command: &mut Command) -> Result<()> {
    debug!(command = ?command, "running NFS mount command");
    let output = command.output().map_err(CrabError::Io)?;
    if output.status.success() {
        return Ok(());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let hint = nfs_mount_failure_hint(&stderr);
    Err(CrabError::Internal(format!(
        "NFS mount command failed with {}: stdout={stdout} stderr={stderr}{hint}",
        output.status
    )))
}

fn nfs_mount_failure_hint(stderr: &str) -> &'static str {
    if cfg!(target_os = "linux") && stderr.contains("Operation not permitted") {
        "\nhint: the mount syscall was denied. NFS mounts require CAP_SYS_ADMIN or host mount permission; run the container with SYS_ADMIN/privileged mount access or mount from a host that allows NFS."
    } else {
        ""
    }
}

#[cfg(target_os = "linux")]
fn running_as_root() -> bool {
    // SAFETY: getuid is a side-effect-free libc call.
    unsafe { libc::getuid() == 0 }
}

fn unmount_native_nfs(mountpoint: &Path) {
    #[cfg(target_os = "linux")]
    {
        let status = if running_as_root() {
            Command::new("umount").arg(mountpoint).status()
        } else {
            Command::new("sudo")
                .args(["-n", "umount"])
                .arg(mountpoint)
                .status()
        };
        if let Err(error) = status {
            warn!(mountpoint = %mountpoint.display(), error = %error, "NFS unmount command failed");
        }
    }

    #[cfg(target_os = "macos")]
    {
        if let Err(error) = Command::new("umount").arg("-f").arg(mountpoint).status() {
            warn!(mountpoint = %mountpoint.display(), error = %error, "NFS unmount command failed");
        }
    }

    #[cfg(windows)]
    {
        let Ok(target) = windows_mount_target(mountpoint) else {
            warn!(mountpoint = %mountpoint.display(), "invalid Windows NFS mount target");
            return;
        };
        let umount_exe = match windows_system_command("umount.exe") {
            Ok(command) => command,
            Err(error) => {
                warn!(mountpoint = %target, error = %error, "NFS unmount command unavailable");
                return;
            }
        };
        if let Err(error) = Command::new(umount_exe).arg(&target).status() {
            warn!(mountpoint = %target, error = %error, "NFS unmount command failed");
        }
    }
}

pub fn is_mounted(mountpoint: &Path) -> bool {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/mounts")
            .map(|mounts| linux_mounts_contains(&mounts, mountpoint))
            .unwrap_or(false)
    }

    #[cfg(target_os = "macos")]
    {
        use std::ffi::CStr;
        use std::os::unix::ffi::OsStrExt;

        // SAFETY: getfsstat initializes at most the capacity-sized buffer;
        // C strings are fixed fields in every initialized statfs entry.
        // MNT_NOWAIT avoids entering a stalled NFS client during health checks.
        unsafe {
            let count = libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT);
            if count <= 0 {
                return false;
            }
            let mut mounts = Vec::<libc::statfs>::with_capacity(count as usize);
            let bytes = count.saturating_mul(std::mem::size_of::<libc::statfs>() as i32);
            let count = libc::getfsstat(mounts.as_mut_ptr(), bytes, libc::MNT_NOWAIT);
            if count <= 0 {
                return false;
            }
            mounts.set_len(count as usize);
            mounts.iter().any(|stat| {
                let target = CStr::from_ptr(stat.f_mntonname.as_ptr());
                let fstype = CStr::from_ptr(stat.f_fstypename.as_ptr());
                target.to_bytes() == mountpoint.as_os_str().as_bytes()
                    && fstype.to_bytes() == b"nfs"
            })
        }
    }

    #[cfg(windows)]
    {
        let Ok(target) = windows_mount_target(mountpoint) else {
            return false;
        };
        let Ok(mount_exe) = windows_system_command("mount.exe") else {
            return false;
        };
        let Ok(output) = Command::new(mount_exe).output() else {
            return false;
        };
        if !output.status.success() {
            return false;
        }
        windows_mount_output_contains(&String::from_utf8_lossy(&output.stdout), &target)
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        let _ = mountpoint;
        false
    }
}

#[cfg(any(target_os = "linux", test))]
fn linux_mounts_contains(mounts: &str, mountpoint: &Path) -> bool {
    let needle = mountpoint.to_string_lossy();
    mounts.lines().any(|line| {
        line.split_whitespace()
            .nth(1)
            .is_some_and(|field| decode_linux_mount_field(field) == needle)
    })
}

#[cfg(any(target_os = "linux", test))]
fn decode_linux_mount_field(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\'
            && index + 3 < bytes.len()
            && let Some(value) = decode_octal_escape(&bytes[index + 1..index + 4])
        {
            decoded.push(value);
            index += 4;
            continue;
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

#[cfg(any(target_os = "linux", test))]
fn decode_octal_escape(digits: &[u8]) -> Option<u8> {
    if digits.len() != 3 || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let mut value = 0u16;
    for digit in digits {
        let octal = digit.checked_sub(b'0')?;
        if octal > 7 {
            return None;
        }
        value = value * 8 + u16::from(octal);
    }
    u8::try_from(value).ok()
}

#[cfg(any(windows, test))]
fn windows_mount_output_contains(output: &str, target: &str) -> bool {
    let target = target.trim_end_matches(['\\', '/']).to_ascii_uppercase();
    output.lines().any(|line| {
        line.split_whitespace().next().is_some_and(|field| {
            field
                .trim_end_matches(['\\', '/'])
                .eq_ignore_ascii_case(&target)
        })
    })
}

fn path_to_string(path: &Path) -> Result<String> {
    path.to_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| CrabError::Configuration {
            key: format!("mount path is not valid UTF-8: {}", path.display()),
            origin: "crab mount --backend=nfs".into(),
        })
}

#[cfg(target_os = "macos")]
fn find_macos_mount_nfs() -> Option<PathBuf> {
    find_command_in_path("mount_nfs", &["/sbin", "/usr/sbin", "/bin", "/usr/bin"])
}

#[cfg(target_os = "linux")]
fn find_mount_nfs() -> Option<PathBuf> {
    find_command_in_path("mount.nfs", &["/sbin", "/usr/sbin", "/bin", "/usr/bin"])
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn find_command_in_path(name: &str, fallback_dirs: &[&str]) -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    for fallback in fallback_dirs {
        let dir = PathBuf::from(fallback);
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    dirs.into_iter()
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
}

#[cfg(target_os = "linux")]
fn sudo_noninteractive_available() -> bool {
    let mut command = Command::new("sudo");
    command
        .args(["-n", "true"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    command.status().is_ok_and(|status| status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_drive_target_parser_requires_explicit_drive() {
        assert_eq!(parse_windows_drive_target("z:"), Some("Z:".to_owned()));
        assert_eq!(parse_windows_drive_target("Z:\\"), Some("Z:".to_owned()));
        assert_eq!(parse_windows_drive_target("*"), None);
        assert_eq!(parse_windows_drive_target("C:\\mount"), None);
    }

    #[test]
    fn listen_addr_matches_os_client_port_contract() {
        if cfg!(windows) {
            assert_eq!(nfs_listen_addr(), "auto:111");
        } else {
            assert_eq!(nfs_listen_addr(), "127.0.0.1:0");
        }
        assert_eq!(windows_nfs_listen_addr(), "auto:111");
    }

    #[test]
    fn windows_port_validation_requires_portmapper_port() {
        let ip = IpAddr::from([127, 88, 0, 1]);

        assert!(validate_windows_nfs_port(ip, WINDOWS_PORTMAP_PORT).is_ok());
        assert!(validate_windows_nfs_port(ip, 2049).is_err());
    }

    #[test]
    fn windows_mount_options_match_client_for_nfs_contract() {
        let opts = windows_mount_options();

        assert!(opts.contains("anon"));
        assert!(opts.contains("nolock"));
        assert!(opts.contains("mtype=hard"));
        assert!(opts.contains("rsize=32"));
        assert!(opts.contains("wsize=32"));
        assert!(opts.contains("fileaccess=766"));
        assert!(opts.contains("casesensitive"));
        assert!(!opts.split(',').any(|opt| opt == "ro"));
    }

    #[test]
    fn nfs_preflight_summary_lists_actionable_blockers() {
        let mut report = NfsPreflightReport::ready();

        report.block(
            "mount.nfs not found",
            "Linux NFS mounts require mount.nfs",
            "Install nfs-common.",
        );
        report.block(
            "NFS mountpoint is already mounted",
            "/mnt/crab",
            "Unmount the existing filesystem.",
        );

        let error = report.ensure_ready().unwrap_err().to_string();

        assert!(error.contains("NFS preflight failed with 2 blocker(s)"));
        assert!(error.contains("mount.nfs not found"));
        assert!(error.contains("next: Install nfs-common."));
        assert!(error.contains("NFS mountpoint is already mounted"));
    }

    #[test]
    fn nfs_preflight_blocks_non_loopback_endpoint() {
        let tmp = tempfile::tempdir().unwrap();
        let config = NfsMountConfig {
            mountpoint: tmp.path().to_path_buf(),
            git_dir: ".git".to_owned(),
            exclusive_verifiers_path: tmp.path().join("verifiers.json"),
            read_only: false,
            auto_refresh_interval: None,
            control_endpoint_override: None,
        };

        let report = preflight(&config, IpAddr::from([192, 0, 2, 1]), 2049);

        assert!(!report.loopback_bind_ready);
        assert!(
            report
                .blockers
                .iter()
                .any(|blocker| blocker.key == "NFS server is not bound to loopback")
        );
    }

    #[test]
    #[cfg(not(windows))]
    fn nfs_preflight_requires_existing_directory_mountpoint() {
        let tmp = tempfile::tempdir().unwrap();
        let mountpoint = tmp.path().join("missing");
        let config = NfsMountConfig {
            mountpoint: mountpoint.clone(),
            git_dir: ".git".to_owned(),
            exclusive_verifiers_path: tmp.path().join("verifiers.json"),
            read_only: false,
            auto_refresh_interval: None,
            control_endpoint_override: None,
        };

        let report = preflight(&config, IpAddr::from([127, 0, 0, 1]), 2049);

        assert!(!report.mountpoint_ready);
        assert!(report.blockers.iter().any(|blocker| {
            blocker.key == "NFS mountpoint does not exist"
                && blocker.detail == mountpoint.display().to_string()
        }));
    }

    #[test]
    fn linux_mounts_parser_matches_exact_mountpoint() {
        let mounts = "\
127.0.0.1:/crab /mnt/crab nfs rw,vers=3 0 0\n\
127.0.0.1:/crab /mnt/crab-extra nfs rw,vers=3 0 0\n";

        assert!(linux_mounts_contains(mounts, Path::new("/mnt/crab")));
        assert!(!linux_mounts_contains(mounts, Path::new("/mnt/missing")));
    }

    #[test]
    fn linux_mounts_parser_decodes_escaped_mountpoint_fields() {
        let mounts = "\
127.0.0.1:/crab /mnt/Crab\\040Mount\\011One nfs rw,vers=3 0 0\n\
127.0.0.1:/crab /mnt/Crab\\040Mount\\011One-extra nfs rw,vers=3 0 0\n";

        assert!(linux_mounts_contains(
            mounts,
            Path::new("/mnt/Crab Mount\tOne")
        ));
        assert!(!linux_mounts_contains(
            mounts,
            Path::new("/mnt/Crab Mount\t")
        ));
    }

    #[test]
    fn linux_mount_field_decoder_leaves_invalid_escapes_literal() {
        assert_eq!(decode_linux_mount_field(r"/mnt/a\040b"), "/mnt/a b");
        assert_eq!(decode_linux_mount_field(r"/mnt/a\999b"), r"/mnt/a\999b");
        assert_eq!(decode_linux_mount_field(r"/mnt/a\04"), r"/mnt/a\04");
    }

    #[test]
    fn windows_mount_output_parser_matches_nfs_drive_entry() {
        let output = "\
Local    Remote                                Properties
-------------------------------------------------------------------------------
Z:       \\\\127.88.0.1\\crab                  UID=-2, GID=-2
                                           rsize=32768, wsize=32768
                                           mount=hard, timeout=0.8
Y:       \\\\127.88.0.2\\crab                  UID=-2, GID=-2
";

        assert!(windows_mount_output_contains(output, "Z:"));
        assert!(windows_mount_output_contains(output, "z:\\"));
        assert!(!windows_mount_output_contains(output, "X:"));
        assert!(!windows_mount_output_contains(output, "Z:\\other"));
    }

    #[test]
    fn linux_mount_error_hint_explains_permission_denial() {
        let hint = nfs_mount_failure_hint("mount.nfs: Operation not permitted");

        if cfg!(target_os = "linux") {
            assert!(hint.contains("CAP_SYS_ADMIN"));
        } else {
            assert_eq!(hint, "");
        }
    }
}
