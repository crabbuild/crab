//! Mount coordinator: long-running process managing multiple FUSE mounts.
//!
//! The coordinator replaces the explicit `crab daemon` for user-facing
//! workflows. It auto-starts on the first `crab mount`, listens on a
//! Unix socket for IPC, and exits when the last mount is unmounted.
//!
//! Lifecycle:
//! 1. Acquire advisory flock on `~/.crab/mounts/daemon.lock`
//! 2. Bind Unix socket at `~/.crab/mounts/daemon.sock`
//! 3. Write PID to `~/.crab/mounts/daemon.pid`
//! 4. Initialize shared `ChunkCache` and `HydrationService`
//! 5. Accept mount/unmount/status requests via IPC
//! 6. On SIGTERM or last unmount: teardown all mounts, cleanup files

use std::collections::{BTreeSet, HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::ChunkCache;
use crate::clone_cache::MountCacheLock;
use crate::core::error::{CrabError, Result};
use crate::engine::VfsEngine;
use crate::fuse::FuseInvalidationIndex;
use crate::hydration::HydrationService;
use crate::overlay::OverlayStore;
use crate::pipeline::{PipelineConfig, PipelineOutput};
use crate::resolver::FuseResolver;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Default maximum chunk cache size (1 GiB).
const DEFAULT_CACHE_MAX_BYTES: u64 = 1024 * 1024 * 1024;

/// Default hydration worker count.
const DEFAULT_HYDRATION_WORKERS: usize = 4;

/// Keep the live reset gate closed briefly after clearing dirty overlay state.
///
/// Linux FUSE can deliver close-time write/setattr callbacks shortly after the
/// CLI reset has forced a sync. Keeping the engine reset epoch active turns
/// those stale callbacks into rejected pre-reset mutations instead of letting
/// them recreate DB rows whose backing files were just deleted.
const LIVE_RESET_QUIESCE: Duration = Duration::from_millis(250);
const LIVE_RESET_QUIESCE_PASSES: usize = 4;

// ---------------------------------------------------------------------------
// CoordinatorError
// ---------------------------------------------------------------------------

/// Coordinator-specific errors.
#[derive(thiserror::Error, Debug)]
pub enum CoordinatorError {
    #[error("failed to acquire daemon lock at {path}: {source}")]
    LockAcquisition {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("failed to bind Unix socket at {path}: {source}")]
    SocketBind {
        path: PathBuf,
        source: std::io::Error,
    },

    #[error("mount not found: {mountpoint}")]
    MountNotFound { mountpoint: String },

    #[error("mount already exists: {mountpoint}")]
    MountAlreadyExists { mountpoint: String },
}

// ---------------------------------------------------------------------------
// MountHandle
// ---------------------------------------------------------------------------

/// Per-mount runtime state held by the coordinator.
pub struct MountHandle {
    /// VFS resolver (snapshot + overlay merged view).
    pub resolver: Arc<FuseResolver>,
    /// VFS engine (handles read/write/hydration dispatch).
    pub engine: Arc<VfsEngine>,
    /// Hydration service for this mount.
    pub hydration: Arc<HydrationService>,
    /// Full pipeline output (snapshot, overlay, handles, etc.).
    pub pipeline_output: PipelineOutput,
    /// Configuration used to create this mount.
    pub config: PipelineConfig,
    /// FUSE background session.
    pub fuse_session: Option<fuser::BackgroundSession>,
    /// Live inode index for targeted kernel cache invalidation.
    pub invalidation_index: Option<FuseInvalidationIndex>,
    /// Per-mount cancellation token (child of coordinator's token).
    pub cancel_token: CancellationToken,
    /// Cross-process cache ownership guard held for the mount lifetime.
    pub _cache_lock: Option<MountCacheLock>,
}

#[derive(Debug)]
pub struct MountReservation {
    mountpoint: PathBuf,
    cache_dir: PathBuf,
}

pub struct MountRegistrationFailure {
    pub error: CrabError,
    pub handle: MountHandle,
}

pub struct OverlayResetTarget {
    engine: Arc<VfsEngine>,
    overlay: Arc<OverlayStore>,
    mountpoint: PathBuf,
}

pub struct OverlayCommitTarget {
    pub engine: Arc<VfsEngine>,
    pub snapshot: Arc<crate::snapshot::SnapshotStore>,
    pub options: crate::publish::OverlayCommitOptions,
    pub mountpoint: PathBuf,
    pub head_ref: String,
}

// ---------------------------------------------------------------------------
// CoordinatorConfig
// ---------------------------------------------------------------------------

/// Configuration for the coordinator process.
#[derive(Debug, Clone)]
pub struct CoordinatorConfig {
    /// Base directory for coordinator state (`~/.crab/mounts/`).
    pub base_dir: PathBuf,
    /// Maximum chunk cache size in bytes.
    pub cache_max_bytes: u64,
    /// Number of hydration worker tasks.
    pub hydration_workers: usize,
}

impl CoordinatorConfig {
    /// Create a config with the default base directory (`~/.crab/mounts/`).
    pub fn default_config() -> Result<Self> {
        let home = std::env::var("HOME").map_err(|_| CrabError::Configuration {
            key: "HOME environment variable not set".into(),
            origin: "coordinator".into(),
        })?;
        Ok(Self {
            base_dir: PathBuf::from(home).join(".crab").join("mounts"),
            cache_max_bytes: DEFAULT_CACHE_MAX_BYTES,
            hydration_workers: DEFAULT_HYDRATION_WORKERS,
        })
    }

    /// Create a config with a custom base directory (useful for testing).
    pub fn with_base_dir(base_dir: PathBuf) -> Self {
        Self {
            base_dir,
            cache_max_bytes: DEFAULT_CACHE_MAX_BYTES,
            hydration_workers: DEFAULT_HYDRATION_WORKERS,
        }
    }

    /// Path to the daemon lock file.
    pub fn lock_path(&self) -> PathBuf {
        self.base_dir.join("daemon.lock")
    }

    /// Path to the Unix socket.
    pub fn socket_path(&self) -> PathBuf {
        self.base_dir.join("daemon.sock")
    }

    /// Path to the PID file.
    pub fn pid_path(&self) -> PathBuf {
        self.base_dir.join("daemon.pid")
    }

    /// Path to the shared chunk cache directory.
    pub fn cache_dir(&self) -> PathBuf {
        self.base_dir.join("cache").join("chunks")
    }
}

// ---------------------------------------------------------------------------
// Coordinator
// ---------------------------------------------------------------------------

/// Long-running process managing multiple FUSE mounts with shared resources.
///
/// The coordinator holds:
/// - A shared `ChunkCache` (bounded, LRU) used by all mounts
/// - A map of active mounts keyed by mountpoint path
/// - A `CancellationToken` for coordinated shutdown
/// - The advisory lock file (held for the coordinator's lifetime)
pub struct Coordinator {
    /// Active mounts keyed by mountpoint path.
    mounts: HashMap<PathBuf, MountHandle>,
    /// Mountpoints admitted but not yet registered.
    pending_mountpoints: HashSet<PathBuf>,
    /// Per-repo cache directories with a mount pipeline in progress.
    pending_cache_dirs: HashMap<PathBuf, PathBuf>,
    /// Shared chunk cache across all mounts.
    chunk_cache: Arc<ChunkCache>,
    /// Coordinator-wide cancellation token.
    cancel_token: CancellationToken,
    /// Path to the Unix socket.
    socket_path: PathBuf,
    /// Advisory lock file (held for lifetime).
    _lock_file: File,
    /// Configuration.
    config: CoordinatorConfig,
    /// Timestamp when the coordinator started (for uptime calculation).
    start_time: Instant,
    /// Whether shutdown has already been performed (prevents double-cleanup in Drop).
    shut_down: bool,
}

impl Coordinator {
    /// Start the coordinator process.
    ///
    /// 1. Acquires advisory flock on `daemon.lock`
    /// 2. Binds Unix socket at `daemon.sock`
    /// 3. Writes PID to `daemon.pid`
    /// 4. Initializes shared `ChunkCache`
    ///
    /// Returns the coordinator ready to accept mount requests.
    /// The Unix socket listener is NOT started here — callers should
    /// use the returned `socket_path` to set up their own listener.
    pub fn start(config: CoordinatorConfig) -> Result<Self> {
        // Ensure base directory exists.
        fs::create_dir_all(&config.base_dir).map_err(|e| {
            CrabError::Internal(format!(
                "failed to create coordinator directory {}: {e}",
                config.base_dir.display()
            ))
        })?;

        // Step 1: Acquire advisory flock.
        let lock_file = acquire_daemon_lock(&config.lock_path())?;
        info!(lock = %config.lock_path().display(), "acquired daemon lock");

        // Step 2: Remove stale socket if present — only actual sockets or
        // empty files (stale socket artifacts), not regular files/dirs.
        let socket_path = config.socket_path();
        if socket_path.exists()
            && let Ok(meta) = fs::metadata(&socket_path)
        {
            let ft = meta.file_type();
            if ft.is_socket() || (ft.is_file() && meta.len() == 0) {
                let _ = fs::remove_file(&socket_path);
            }
        }

        // Step 3: Write PID file.
        write_pid_file(&config.pid_path())?;
        info!(pid = std::process::id(), "wrote daemon PID file");

        // Step 4: Initialize shared chunk cache.
        let cache_dir = config.cache_dir();
        fs::create_dir_all(&cache_dir).map_err(|e| {
            CrabError::Internal(format!(
                "failed to create cache directory {}: {e}",
                cache_dir.display()
            ))
        })?;

        let chunk_cache = Arc::new(
            ChunkCache::open(cache_dir, Some(config.cache_max_bytes)).map_err(|e| {
                error!(error = %e, "failed to open shared chunk cache");
                e
            })?,
        );
        info!(
            max_bytes = config.cache_max_bytes,
            "shared chunk cache initialized"
        );

        let cancel_token = CancellationToken::new();

        Ok(Self {
            mounts: HashMap::new(),
            pending_mountpoints: HashSet::new(),
            pending_cache_dirs: HashMap::new(),
            chunk_cache,
            cancel_token,
            socket_path,
            _lock_file: lock_file,
            config,
            start_time: Instant::now(),
            shut_down: false,
        })
    }

    /// Get a reference to the shared chunk cache.
    pub fn chunk_cache(&self) -> &Arc<ChunkCache> {
        &self.chunk_cache
    }

    /// Get the cancellation token for this coordinator.
    pub fn cancel_token(&self) -> &CancellationToken {
        &self.cancel_token
    }

    /// Get the socket path for IPC.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Get the number of active mounts.
    pub fn mount_count(&self) -> usize {
        self.mounts.len()
    }

    /// Get the configured hydration worker count.
    pub fn hydration_workers(&self) -> usize {
        self.config.hydration_workers
    }

    /// Uptime in seconds since the coordinator started.
    pub fn uptime_secs(&self) -> u64 {
        self.start_time.elapsed().as_secs()
    }

    /// Total cache capacity in bytes.
    pub fn cache_size_bytes(&self) -> u64 {
        self.chunk_cache.max_bytes()
    }

    /// Check if a mount exists at the given path.
    pub fn has_mount(&self, mountpoint: &Path) -> bool {
        self.mounts.contains_key(mountpoint)
    }

    /// Return the active mount using the given per-repo cache directory.
    pub fn mountpoint_for_cache_dir(&self, cache_dir: &Path) -> Option<PathBuf> {
        self.mounts
            .iter()
            .find(|(_, handle)| handle.config.cache_dir == cache_dir)
            .map(|(mountpoint, _)| mountpoint.clone())
    }

    /// Reserve mount ownership while the slow pipeline runs outside the coordinator lock.
    pub fn reserve_mount(
        &mut self,
        mountpoint: &Path,
        cache_dir: &Path,
    ) -> Result<MountReservation> {
        if self.mounts.contains_key(mountpoint) || self.pending_mountpoints.contains(mountpoint) {
            return Err(CrabError::Internal(format!(
                "mount already exists at {}",
                mountpoint.display()
            )));
        }

        if let Some(existing_mountpoint) = self.pending_cache_dirs.get(cache_dir) {
            return Err(CrabError::Internal(format!(
                "mount already starting for this repository at {}; wait for it to finish before starting another mount",
                existing_mountpoint.display()
            )));
        }
        if let Some(existing_mountpoint) = self.mountpoint_for_cache_dir(cache_dir) {
            return Err(CrabError::Internal(format!(
                "mount already active for this repository at {}; unmount it before starting another mount",
                existing_mountpoint.display()
            )));
        }

        let mountpoint = mountpoint.to_path_buf();
        self.pending_mountpoints.insert(mountpoint.clone());
        self.pending_cache_dirs
            .insert(cache_dir.to_path_buf(), mountpoint.clone());

        Ok(MountReservation {
            mountpoint,
            cache_dir: cache_dir.to_path_buf(),
        })
    }

    pub fn release_mount_reservation(&mut self, reservation: MountReservation) {
        self.pending_mountpoints.remove(&reservation.mountpoint);
        self.pending_cache_dirs.remove(&reservation.cache_dir);
        self.signal_shutdown_if_idle();
    }

    pub fn add_reserved_mount(
        &mut self,
        reservation: MountReservation,
        handle: MountHandle,
    ) -> std::result::Result<(), MountRegistrationFailure> {
        let mountpoint = reservation.mountpoint.clone();
        self.pending_mountpoints.remove(&reservation.mountpoint);
        self.pending_cache_dirs.remove(&reservation.cache_dir);
        if let Err(error) = self.validate_mount_can_be_added(&mountpoint, &handle.config) {
            self.signal_shutdown_if_idle();
            return Err(MountRegistrationFailure { error, handle });
        }

        self.insert_mount(mountpoint, handle);
        Ok(())
    }

    /// Register a mount with the coordinator.
    ///
    /// The caller is responsible for running the pipeline and creating
    /// the `MountHandle`. This method just tracks it.
    pub fn add_mount(&mut self, mountpoint: PathBuf, handle: MountHandle) -> Result<()> {
        self.validate_mount_can_be_added(&mountpoint, &handle.config)?;
        self.insert_mount(mountpoint, handle);
        Ok(())
    }

    fn validate_mount_can_be_added(
        &self,
        mountpoint: &Path,
        config: &PipelineConfig,
    ) -> Result<()> {
        if self.mounts.contains_key(mountpoint) || self.pending_mountpoints.contains(mountpoint) {
            return Err(CrabError::Internal(format!(
                "mount already exists at {}",
                mountpoint.display()
            )));
        }
        if let Some(existing_mountpoint) = self.mountpoint_for_cache_dir(&config.cache_dir) {
            return Err(CrabError::Internal(format!(
                "mount already active for this repository at {}; unmount it before starting another mount",
                existing_mountpoint.display()
            )));
        }

        Ok(())
    }

    fn insert_mount(&mut self, mountpoint: PathBuf, handle: MountHandle) {
        info!(mountpoint = %mountpoint.display(), "registered mount with coordinator");
        self.mounts.insert(mountpoint, handle);
    }

    /// Remove and teardown a single mount.
    ///
    /// Cancels the mount's token, drops the FUSE session (which unmounts),
    /// and removes it from the active map.
    pub fn remove_mount(&mut self, mountpoint: &Path) -> Result<()> {
        let handle = self.take_mount(mountpoint)?;

        let result = unmount_removed_mount(handle, mountpoint);
        self.signal_shutdown_if_idle();
        result?;

        info!(mountpoint = %mountpoint.display(), "unmounted and removed from coordinator");
        Ok(())
    }

    pub fn take_mount(&mut self, mountpoint: &Path) -> Result<MountHandle> {
        let handle = self
            .mounts
            .remove(mountpoint)
            .ok_or_else(|| CrabError::Internal(format!("no mount at {}", mountpoint.display())))?;
        handle.cancel_token.cancel();
        Ok(handle)
    }

    pub fn finish_mount_removal(&mut self) {
        self.signal_shutdown_if_idle();
    }

    fn signal_shutdown_if_idle(&self) {
        if self.mounts.is_empty() && self.pending_mountpoints.is_empty() {
            info!("no active or starting mounts remain, signaling coordinator shutdown");
            self.cancel_token.cancel();
        }
    }

    /// Invalidate cached kernel entries for paths changed outside FUSE.
    pub fn invalidate_mount_paths(&self, mountpoint: &Path, paths: &[String]) -> Result<()> {
        let handle = self
            .mounts
            .get(mountpoint)
            .ok_or_else(|| CrabError::Internal(format!("no mount at {}", mountpoint.display())))?;
        let Some(session) = &handle.fuse_session else {
            return Ok(());
        };

        let notifier = session.notifier();
        let mut fallback_top_level_names = BTreeSet::new();
        for path in paths {
            let target = handle
                .invalidation_index
                .as_ref()
                .and_then(|index| index.target_for_path(path));

            if let Some(target) = target {
                if let Err(e) = notifier.inval_entry(target.parent, &target.name) {
                    warn!(
                        mountpoint = %mountpoint.display(),
                        path = %path,
                        error = %e,
                        "failed to invalidate FUSE entry"
                    );
                }
                if let Some(inode) = target.inode
                    && let Err(e) = notifier.inval_inode(inode, 0, i64::MAX)
                {
                    warn!(
                        mountpoint = %mountpoint.display(),
                        path = %path,
                        error = %e,
                        "failed to invalidate FUSE inode"
                    );
                }
            } else if let Some(name) = path.split('/').next()
                && !name.is_empty()
            {
                fallback_top_level_names.insert(name.to_owned());
            }
        }

        for name in fallback_top_level_names {
            if let Err(e) = notifier.inval_entry(fuser::INodeNo::ROOT, OsStr::new(&name)) {
                warn!(
                    mountpoint = %mountpoint.display(),
                    path = %name,
                    error = %e,
                    "failed to invalidate FUSE entry"
                );
            }
        }
        if let Err(e) = notifier.inval_inode(fuser::INodeNo::ROOT, 0, 0) {
            warn!(
                mountpoint = %mountpoint.display(),
                error = %e,
                "failed to invalidate FUSE root inode"
            );
        }

        Ok(())
    }

    /// Return the live reset target for a mounted writable overlay.
    pub fn overlay_reset_target(&self, mountpoint: &Path) -> Result<OverlayResetTarget> {
        let handle = self
            .mounts
            .get(mountpoint)
            .ok_or_else(|| CrabError::Internal(format!("no mount at {}", mountpoint.display())))?;
        if handle.config.read_only {
            return Err(CrabError::Forbidden {
                path: format!("read-only mount: {}", mountpoint.display()),
            });
        }
        let overlay = handle.pipeline_output.overlay.as_ref().ok_or_else(|| {
            CrabError::Internal(format!("mount has no overlay: {}", mountpoint.display()))
        })?;

        Ok(OverlayResetTarget {
            engine: Arc::clone(&handle.engine),
            overlay: Arc::clone(overlay),
            mountpoint: mountpoint.to_path_buf(),
        })
    }

    /// Reset a mounted writable overlay through the live store.
    pub async fn reset_overlay_target(
        target: OverlayResetTarget,
    ) -> Result<crate::publish::OverlayDiff> {
        let _reset = target.engine.begin_overlay_reset().await;
        let diff = crate::publish::reset_overlay_store(&target.overlay)?;
        for pass in 1..=LIVE_RESET_QUIESCE_PASSES {
            tokio::time::sleep(LIVE_RESET_QUIESCE).await;
            let remaining = target.overlay.records()?;
            if remaining.is_empty() {
                info!(
                    mountpoint = %target.mountpoint.display(),
                    changes = diff.changes.len(),
                    "reset mounted overlay"
                );
                return Ok(diff);
            }
            warn!(
                mountpoint = %target.mountpoint.display(),
                pass,
                records = remaining.len(),
                "clearing delayed overlay mutations after reset"
            );
            target.overlay.clear()?;
        }
        Err(CrabError::Internal(
            "overlay reset did not quiesce delayed mutations".into(),
        ))
    }

    /// Inspect a mounted writable overlay through the live store.
    pub fn inspect_mount_overlay(&self, mountpoint: &Path) -> Result<crate::publish::OverlayDiff> {
        let handle = self
            .mounts
            .get(mountpoint)
            .ok_or_else(|| CrabError::Internal(format!("no mount at {}", mountpoint.display())))?;
        if handle.config.read_only {
            return Err(CrabError::Forbidden {
                path: format!("read-only mount: {}", mountpoint.display()),
            });
        }
        let overlay = handle.pipeline_output.overlay.as_ref().ok_or_else(|| {
            CrabError::Internal(format!("mount has no overlay: {}", mountpoint.display()))
        })?;
        let diff = crate::publish::inspect_overlay_store(overlay)?;
        overlay.checkpoint_wal()?;
        Ok(diff)
    }

    /// Return the live resources needed to commit a mounted overlay without
    /// holding the coordinator lock during Git and filesystem I/O.
    pub fn overlay_commit_target(
        &self,
        mountpoint: &Path,
        message: String,
        push: bool,
    ) -> Result<OverlayCommitTarget> {
        let handle = self
            .mounts
            .get(mountpoint)
            .ok_or_else(|| CrabError::Internal(format!("no mount at {}", mountpoint.display())))?;
        if handle.config.read_only {
            return Err(CrabError::Forbidden {
                path: format!("read-only mount: {}", mountpoint.display()),
            });
        }
        Ok(OverlayCommitTarget {
            engine: Arc::clone(&handle.engine),
            snapshot: Arc::clone(&handle.pipeline_output.snapshot),
            options: crate::publish::OverlayCommitOptions {
                cache_dir: handle.config.cache_dir.clone(),
                git_dir: handle.config.git_dir.clone(),
                ref_name: handle.pipeline_output.head_ref.clone(),
                message,
                push,
            },
            mountpoint: mountpoint.to_path_buf(),
            head_ref: handle.pipeline_output.head_ref.clone(),
        })
    }

    /// Swap a live mount to the generation persisted by a successful commit.
    pub fn adopt_mount_commit(
        &mut self,
        mountpoint: &Path,
        head_oid: &str,
        head_ref: &str,
    ) -> Result<()> {
        let handle = self
            .mounts
            .get_mut(mountpoint)
            .ok_or_else(|| CrabError::Internal(format!("no mount at {}", mountpoint.display())))?;
        crate::mount_runtime::adopt_published_snapshot(
            &mut handle.pipeline_output,
            &handle.config.git_dir,
            head_oid,
            head_ref,
        )?;
        Ok(())
    }

    /// Graceful shutdown: unmount all FUSE sessions, close socket, remove files.
    ///
    /// Called on SIGTERM/SIGINT or when the last mount is unmounted.
    /// This synchronous version cancels tokens and drops sessions but does not
    /// wait for hydrator tasks to finish. Prefer `shutdown_graceful()` in async
    /// contexts.
    pub fn shutdown(&mut self) {
        if self.shut_down {
            return;
        }
        self.shut_down = true;
        info!("coordinator shutting down");

        // Cancel all mount refresh loops and hydration workers.
        self.cancel_token.cancel();
        self.pending_mountpoints.clear();
        self.pending_cache_dirs.clear();

        // Unmount all FUSE sessions.
        let mountpoints: Vec<PathBuf> = self.mounts.keys().cloned().collect();
        for mountpoint in &mountpoints {
            if let Some(handle) = self.mounts.remove(mountpoint) {
                handle.cancel_token.cancel();
                unmount_session_logged(handle.fuse_session, mountpoint);
                info!(mountpoint = %mountpoint.display(), "unmounted during shutdown");
            }
        }

        self.cleanup_files();
    }

    /// Graceful async shutdown: cancel all mount child tokens, wait up to 10s
    /// for hydrator tasks to complete, log warnings for stuck mounts, then
    /// clean up daemon files.
    ///
    /// This is the preferred shutdown path when running inside a tokio runtime.
    pub async fn shutdown_graceful(&mut self) {
        use std::time::Duration;
        use tokio::time::timeout;

        const GRACE_PERIOD: Duration = Duration::from_secs(10);

        if self.shut_down {
            return;
        }
        self.shut_down = true;

        info!("coordinator initiating graceful shutdown");

        // Cancel the coordinator-wide token first. This propagates to all
        // child tokens, signaling hydration workers and refresh loops to stop.
        self.cancel_token.cancel();
        self.pending_mountpoints.clear();
        self.pending_cache_dirs.clear();

        // Collect all mount handles so we can cancel their tokens and await
        // their hydrator join handles with a timeout.
        let mountpoints: Vec<PathBuf> = self.mounts.keys().cloned().collect();
        let mut mount_handles: Vec<(PathBuf, MountHandle)> = mountpoints
            .into_iter()
            .filter_map(|mp| self.mounts.remove(&mp).map(|h| (mp, h)))
            .collect();

        // Cancel each mount's child token explicitly (belt-and-suspenders —
        // the coordinator token cancellation already propagates, but this
        // makes the intent clear).
        for (mountpoint, handle) in &mount_handles {
            handle.cancel_token.cancel();
            debug!(mountpoint = %mountpoint.display(), "cancelled mount child token");
        }

        // Wait for hydrator handles with a per-mount timeout.
        for (mountpoint, handle) in &mut mount_handles {
            let hydrator_count = handle.pipeline_output.hydrator_handles.len();
            if hydrator_count == 0 {
                info!(mountpoint = %mountpoint.display(), "mount has no active tasks, unmounting");
                continue;
            }

            let handles: Vec<_> = handle.pipeline_output.hydrator_handles.drain(..).collect();
            let mp_display = mountpoint.display().to_string();

            let wait_result = timeout(GRACE_PERIOD, async {
                for h in handles {
                    // Ignore join errors (task may have been aborted or panicked).
                    let _ = h.await;
                }
            })
            .await;

            match wait_result {
                Ok(()) => {
                    info!(mountpoint = %mp_display, "mount tasks completed within grace period");
                }
                Err(_) => {
                    warn!(
                        mountpoint = %mp_display,
                        grace_period_secs = GRACE_PERIOD.as_secs(),
                        "mount did not unmount within grace period"
                    );
                }
            }
        }

        // Unmount FUSE sessions after waiting for tasks.
        for (mountpoint, handle) in mount_handles {
            unmount_session_logged(handle.fuse_session, &mountpoint);
            info!(mountpoint = %mountpoint.display(), "unmounted during shutdown");
        }

        self.cleanup_files();
    }

    /// Remove daemon socket, PID, and lock files.
    fn cleanup_files(&self) {
        // Remove socket file.
        if self.socket_path.exists() {
            if let Err(e) = fs::remove_file(&self.socket_path) {
                warn!(
                    path = %self.socket_path.display(),
                    error = %e,
                    "failed to remove socket file"
                );
            } else {
                debug!(path = %self.socket_path.display(), "removed socket file");
            }
        }

        // Remove PID file.
        let pid_path = self.config.pid_path();
        if pid_path.exists() {
            if let Err(e) = fs::remove_file(&pid_path) {
                warn!(
                    path = %pid_path.display(),
                    error = %e,
                    "failed to remove PID file"
                );
            } else {
                debug!(path = %pid_path.display(), "removed PID file");
            }
        }

        // Remove lock file.
        let lock_path = self.config.lock_path();
        if lock_path.exists() {
            if let Err(e) = fs::remove_file(&lock_path) {
                warn!(
                    path = %lock_path.display(),
                    error = %e,
                    "failed to remove lock file"
                );
            } else {
                debug!(path = %lock_path.display(), "removed lock file");
            }
        }

        info!("coordinator shutdown complete");
    }

    /// Install signal handlers that trigger shutdown on SIGTERM/SIGINT.
    pub fn install_signal_handler(&self, rt: &tokio::runtime::Handle) {
        let cancel = self.cancel_token.clone();
        rt.spawn(async move {
            wait_for_shutdown_signal().await;
            cancel.cancel();
        });
    }

    /// Reload configuration from disk.
    ///
    /// Currently a stub that logs the reload request. Future implementations
    /// may re-read cache limits, hydration worker counts, or other tunable
    /// parameters without restarting the coordinator.
    pub fn reload_config(&mut self) -> Result<()> {
        info!("configuration reload requested");
        Ok(())
    }

    /// Create a child cancellation token for a new mount.
    ///
    /// When the coordinator shuts down, all child tokens are cancelled.
    pub fn child_cancel_token(&self) -> CancellationToken {
        self.cancel_token.child_token()
    }

    /// List all active mountpoints.
    pub fn list_mountpoints(&self) -> Vec<&Path> {
        self.mounts.keys().map(PathBuf::as_path).collect()
    }

    /// Get a reference to a mount handle by mountpoint.
    pub fn get_mount(&self, mountpoint: &Path) -> Option<&MountHandle> {
        self.mounts.get(mountpoint)
    }

    /// Get a mutable reference to a mount handle by mountpoint.
    pub fn get_mount_mut(&mut self, mountpoint: &Path) -> Option<&mut MountHandle> {
        self.mounts.get_mut(mountpoint)
    }

    /// Refresh a mount: fetch from remote, rebuild snapshot, swap generation.
    ///
    /// Triggers an immediate `git fetch origin` in the mount's git directory,
    /// resolves the new HEAD OID, rebuilds the snapshot, reconciles the
    /// overlay, and swaps the resolver's generation.
    ///
    /// Returns the new HEAD OID on success.
    pub fn refresh_mount(&mut self, mountpoint: &Path) -> Result<String> {
        let handle = self
            .mounts
            .get_mut(mountpoint)
            .ok_or_else(|| CrabError::Internal(format!("no mount at {}", mountpoint.display())))?;

        let update = crate::mount_runtime::refresh_mount_runtime(
            &mut handle.pipeline_output,
            &handle.config,
            mountpoint,
        )?;
        Ok(update.head_oid)
    }

    /// Switch a mount to a different ref/branch.
    ///
    /// Fetches the new ref, resolves it to an OID, rebuilds the snapshot,
    /// reconciles the overlay, swaps the resolver's generation, and updates
    /// the mount's tracked ref.
    ///
    /// Returns the new HEAD OID on success.
    pub fn switch_mount_ref(&mut self, mountpoint: &Path, new_ref: &str) -> Result<String> {
        let handle = self
            .mounts
            .get_mut(mountpoint)
            .ok_or_else(|| CrabError::Internal(format!("no mount at {}", mountpoint.display())))?;

        let update = crate::mount_runtime::switch_mount_runtime(
            &mut handle.pipeline_output,
            &mut handle.config,
            mountpoint,
            new_ref,
        )?;
        Ok(update.head_oid)
    }
}

impl Drop for Coordinator {
    fn drop(&mut self) {
        // Skip if shutdown was already called explicitly.
        if self.shut_down {
            return;
        }

        // Always perform cleanup on drop — cancel the token so any spawned
        // background tasks (refresh loops, hydrators) exit, unmount any
        // remaining FUSE sessions, and remove daemon files. This prevents
        // orphaned coordinator processes when tests panic or exit without
        // calling shutdown() explicitly.
        self.cancel_token.cancel();
        self.pending_mountpoints.clear();
        self.pending_cache_dirs.clear();

        // Unmount any remaining FUSE sessions.
        let mountpoints: Vec<PathBuf> = self.mounts.keys().cloned().collect();
        for mountpoint in &mountpoints {
            if let Some(handle) = self.mounts.remove(mountpoint) {
                handle.cancel_token.cancel();
                unmount_session_logged(handle.fuse_session, mountpoint);
            }
        }

        // Always clean up daemon files (PID, lock, socket).
        self.cleanup_files();
    }
}

fn unmount_session(session: Option<fuser::BackgroundSession>, mountpoint: &Path) -> Result<()> {
    let Some(session) = session else {
        return Ok(());
    };

    crate::mount::unmount_background_session(session, mountpoint)
}

pub fn unmount_removed_mount(handle: MountHandle, mountpoint: &Path) -> Result<()> {
    unmount_session(handle.fuse_session, mountpoint)
}

fn unmount_session_logged(session: Option<fuser::BackgroundSession>, mountpoint: &Path) {
    if let Err(e) = unmount_session(session, mountpoint) {
        warn!(
            mountpoint = %mountpoint.display(),
            error = %e,
            "failed to unmount FUSE session"
        );
    }
}

// ---------------------------------------------------------------------------
// Lock acquisition
// ---------------------------------------------------------------------------

/// Acquire an advisory flock on the daemon lock file.
///
/// Uses `LOCK_EX | LOCK_NB` (non-blocking exclusive lock). If another
/// coordinator already holds the lock, returns an error immediately
/// rather than blocking.
#[cfg(unix)]
fn acquire_daemon_lock(path: &Path) -> Result<File> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| {
            CrabError::Internal(format!(
                "failed to create lock directory {}: {e}",
                parent.display()
            ))
        })?;
    }

    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)
        .map_err(|e| {
            CrabError::Internal(format!("failed to open lock file {}: {e}", path.display()))
        })?;

    // Non-blocking exclusive lock.
    let fd = std::os::unix::io::AsRawFd::as_raw_fd(&file);
    // SAFETY: flock with LOCK_EX|LOCK_NB on a valid fd is safe.
    let ret = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        return Err(CrabError::Internal(format!(
            "another coordinator is already running (lock held at {}): {err}",
            path.display()
        )));
    }

    Ok(file)
}

#[cfg(not(unix))]
fn acquire_daemon_lock(path: &Path) -> Result<File> {
    // On non-Unix platforms, skip locking (best-effort).
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| {
            CrabError::Internal(format!(
                "failed to create lock directory {}: {e}",
                parent.display()
            ))
        })?;
    }

    OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)
        .map_err(|e| {
            CrabError::Internal(format!("failed to open lock file {}: {e}", path.display()))
        })
}

// ---------------------------------------------------------------------------
// PID file
// ---------------------------------------------------------------------------

/// Write the current process PID to the given path.
fn write_pid_file(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let pid = std::process::id();
    fs::write(path, pid.to_string()).map_err(|e| {
        CrabError::Internal(format!("failed to write PID file {}: {e}", path.display()))
    })?;
    Ok(())
}

/// Read the PID from a PID file, if it exists.
pub fn read_daemon_pid(base_dir: &Path) -> Option<u32> {
    let path = base_dir.join("daemon.pid");
    fs::read_to_string(&path)
        .ok()
        .and_then(|s| s.trim().parse().ok())
}

/// Check if the coordinator is running by testing if the PID is alive.
pub fn is_coordinator_running(base_dir: &Path) -> bool {
    let Some(pid) = read_daemon_pid(base_dir) else {
        return false;
    };

    // Check if the process is alive via kill(pid, 0).
    #[cfg(unix)]
    {
        // SAFETY: kill with signal 0 just checks if the process exists.
        let ret = unsafe { libc::kill(pid as libc::pid_t, 0) };
        ret == 0
    }

    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

// ---------------------------------------------------------------------------
// Signal handling
// ---------------------------------------------------------------------------

/// Wait for SIGTERM or SIGINT.
async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let mut sigint = match signal(SignalKind::interrupt()) {
            Ok(s) => s,
            Err(e) => {
                error!(error = %e, "failed to register SIGINT handler");
                // Fall back to ctrl_c.
                let _ = tokio::signal::ctrl_c().await;
                info!("coordinator received ctrl-c (fallback)");
                return;
            }
        };
        let mut sigterm = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, "failed to register SIGTERM handler, using SIGINT only");
                sigint.recv().await;
                info!("coordinator received SIGINT");
                return;
            }
        };

        tokio::select! {
            _ = sigint.recv() => {
                info!("coordinator received SIGINT");
            }
            _ = sigterm.recv() => {
                info!("coordinator received SIGTERM");
            }
        }
    }

    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        info!("coordinator received ctrl-c");
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;

    #[test]
    fn lock_acquisition_succeeds() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_path = tmp.path().join("daemon.lock");

        let file = acquire_daemon_lock(&lock_path).unwrap();
        assert!(lock_path.exists());
        drop(file);
    }

    #[cfg(unix)]
    #[test]
    fn lock_acquisition_fails_when_held() {
        let tmp = tempfile::tempdir().unwrap();
        let lock_path = tmp.path().join("daemon.lock");

        // Acquire the lock.
        let _file = acquire_daemon_lock(&lock_path).unwrap();

        // Second acquisition should fail.
        let result = acquire_daemon_lock(&lock_path);
        assert!(result.is_err());
        let err_msg = format!("{}", result.unwrap_err());
        assert!(err_msg.contains("another coordinator is already running"));
    }

    #[test]
    fn pid_file_write_and_read() {
        let tmp = tempfile::tempdir().unwrap();
        let pid_path = tmp.path().join("daemon.pid");

        write_pid_file(&pid_path).unwrap();

        let pid = read_daemon_pid(tmp.path());
        assert_eq!(pid, Some(std::process::id()));
    }

    #[test]
    fn coordinator_config_paths() {
        let config = CoordinatorConfig::with_base_dir(PathBuf::from("/tmp/test-mounts"));

        assert_eq!(
            config.lock_path(),
            PathBuf::from("/tmp/test-mounts/daemon.lock")
        );
        assert_eq!(
            config.socket_path(),
            PathBuf::from("/tmp/test-mounts/daemon.sock")
        );
        assert_eq!(
            config.pid_path(),
            PathBuf::from("/tmp/test-mounts/daemon.pid")
        );
        assert_eq!(
            config.cache_dir(),
            PathBuf::from("/tmp/test-mounts/cache/chunks")
        );
    }

    #[test]
    fn coordinator_start_and_shutdown() {
        let tmp = tempfile::tempdir().unwrap();
        let config = CoordinatorConfig::with_base_dir(tmp.path().to_path_buf());

        let mut coordinator = Coordinator::start(config).unwrap();

        // Verify files were created.
        assert!(tmp.path().join("daemon.lock").exists());
        assert!(tmp.path().join("daemon.pid").exists());
        assert!(tmp.path().join("cache/chunks").exists());

        // Verify initial state.
        assert_eq!(coordinator.mount_count(), 0);
        assert!(!coordinator.has_mount(Path::new("/mnt/test")));

        // Shutdown should clean up files.
        coordinator.shutdown();

        assert!(!tmp.path().join("daemon.pid").exists());
        assert!(!tmp.path().join("daemon.lock").exists());
    }

    #[test]
    fn mount_reservation_blocks_same_mountpoint_and_starting_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let config = CoordinatorConfig::with_base_dir(tmp.path().to_path_buf());
        let mut coordinator = Coordinator::start(config).unwrap();
        let cache_dir = tmp.path().join("repo-cache");
        let mountpoint = Path::new("/mnt/repo");

        let reservation = coordinator.reserve_mount(mountpoint, &cache_dir).unwrap();

        let duplicate_mountpoint = coordinator
            .reserve_mount(mountpoint, &tmp.path().join("other-cache"))
            .unwrap_err();
        assert!(
            duplicate_mountpoint
                .to_string()
                .contains("mount already exists")
        );

        let duplicate_cache = coordinator
            .reserve_mount(Path::new("/mnt/other"), &cache_dir)
            .unwrap_err();
        assert!(
            duplicate_cache
                .to_string()
                .contains("mount already starting")
        );

        coordinator.release_mount_reservation(reservation);
        assert!(coordinator.pending_mountpoints.is_empty());
        assert!(coordinator.pending_cache_dirs.is_empty());
    }

    #[test]
    fn pending_mount_prevents_idle_shutdown_until_released() {
        let tmp = tempfile::tempdir().unwrap();
        let config = CoordinatorConfig::with_base_dir(tmp.path().to_path_buf());
        let mut coordinator = Coordinator::start(config).unwrap();

        let reservation = coordinator
            .reserve_mount(Path::new("/mnt/repo"), &tmp.path().join("repo-cache"))
            .unwrap();

        coordinator.finish_mount_removal();
        assert!(!coordinator.cancel_token().is_cancelled());

        coordinator.release_mount_reservation(reservation);
        assert!(coordinator.cancel_token().is_cancelled());
    }

    #[test]
    fn is_coordinator_running_false_when_no_pid() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!is_coordinator_running(tmp.path()));
    }

    #[cfg(unix)]
    #[test]
    fn is_coordinator_running_true_for_current_process() {
        let tmp = tempfile::tempdir().unwrap();
        let pid_path = tmp.path().join("daemon.pid");
        write_pid_file(&pid_path).unwrap();

        assert!(is_coordinator_running(tmp.path()));
    }

    #[cfg(unix)]
    #[test]
    fn is_coordinator_running_false_for_dead_pid() {
        let tmp = tempfile::tempdir().unwrap();
        let pid_path = tmp.path().join("daemon.pid");
        // Use a very high PID that's unlikely to exist.
        fs::write(&pid_path, "999999999").unwrap();

        assert!(!is_coordinator_running(tmp.path()));
    }

    #[test]
    fn coordinator_child_cancel_token_linked() {
        let tmp = tempfile::tempdir().unwrap();
        let config = CoordinatorConfig::with_base_dir(tmp.path().to_path_buf());

        let mut coordinator = Coordinator::start(config).unwrap();
        let child = coordinator.child_cancel_token();

        assert!(!child.is_cancelled());

        coordinator.cancel_token.cancel();
        assert!(child.is_cancelled());

        coordinator.shutdown();
    }

    /// Integration test verifying the shared resource model:
    /// - Coordinator holds a single shared ChunkCache
    /// - Multiple mounts share the same cache instance
    /// - Per-mount isolation: each mount has its own Snapshot, Resolver, Engine
    /// - Ref-counting: removing the last mount signals coordinator shutdown
    #[test]
    fn shared_resources_and_ref_counting() {
        use crate::ChunkCache;
        use crate::engine::VfsEngine;
        use crate::hydration::HydrationService;
        use crate::pipeline::{PipelineConfig, PipelineOutput};
        use crate::resolver::FuseResolver;
        use crate::snapshot::SnapshotStore;
        use crate::verified_set::VerifiedSet;

        // Stub implementations for hydration dependencies.
        struct StubFileIndexResolver;
        impl crate::data_plane::FileIndexResolver for StubFileIndexResolver {
            fn resolve_file_index(
                &self,
                _file_hash: &[u8; 32],
                _shard_hint: Option<&[u8; 32]>,
            ) -> crate::core::error::Result<Option<[u8; 32]>> {
                Ok(None)
            }
            fn scan_shard_list_for_file(
                &self,
                _file_hash: &[u8; 32],
            ) -> crate::core::error::Result<Option<[u8; 32]>> {
                Ok(None)
            }
        }

        struct StubShardLoader;
        impl crate::data_plane::ShardLoader for StubShardLoader {
            fn load_reconstruction_terms(
                &self,
                _shard_hash: &[u8; 32],
                _file_hash: &[u8; 32],
            ) -> crate::core::error::Result<Vec<crate::data_plane::ReconstructionTerm>>
            {
                Ok(Vec::new())
            }
        }

        struct StubXorbFetcher;
        impl crate::data_plane::XorbFetcher for StubXorbFetcher {
            fn fetch_range(
                &self,
                _xorb_hash: &[u8; 32],
                _range: std::ops::Range<u64>,
            ) -> crate::core::error::Result<Vec<u8>> {
                Err(crate::core::error::CrabError::Internal(
                    "stub xorb fetcher".into(),
                ))
            }
        }

        /// Build a minimal MountHandle sharing the given cache.
        fn make_mount_handle(
            cache: &Arc<ChunkCache>,
            snapshot_path: &Path,
            source: &str,
            read_only: bool,
            cancel: CancellationToken,
        ) -> MountHandle {
            let snapshot = Arc::new(SnapshotStore::open_or_create(snapshot_path).unwrap());
            let resolver = Arc::new(FuseResolver::new(Arc::clone(&snapshot), None, 0, 0));
            let hydration = HydrationService::new(
                Arc::clone(cache),
                Arc::new(VerifiedSet::default()),
                Arc::new(StubFileIndexResolver),
                Arc::new(StubShardLoader),
                Arc::new(StubXorbFetcher),
                None,
                None,
                Some(2),
                cancel.clone(),
            );
            let engine = Arc::new(VfsEngine::new(
                Arc::clone(&resolver),
                None,
                Arc::clone(&hydration),
                None,
                Some(Arc::clone(&snapshot)),
            ));

            let config = PipelineConfig {
                source: source.into(),
                git_dir: snapshot_path.parent().unwrap().join(".git"),
                ref_name: Some("refs/heads/main".into()),
                read_only,
                cache_dir: snapshot_path.parent().unwrap().to_path_buf(),
                cancel_token: cancel.clone(),
            };

            let pipeline_output = PipelineOutput {
                resolver: Arc::clone(&resolver),
                engine: Arc::clone(&engine),
                hydration: Arc::clone(&hydration),
                snapshot: Arc::clone(&snapshot),
                overlay: None,
                head_oid: "deadbeef".into(),
                head_ref: "refs/heads/main".into(),
                generation: 0,
                hydrator_handles: Vec::new(),
            };

            MountHandle {
                resolver,
                engine,
                hydration,
                pipeline_output,
                config,
                fuse_session: None,
                invalidation_index: None,
                cancel_token: cancel,
                _cache_lock: None,
            }
        }

        let tmp = tempfile::tempdir().unwrap();
        let config = CoordinatorConfig::with_base_dir(tmp.path().to_path_buf());
        let mut coordinator = Coordinator::start(config).unwrap();

        let shared_cache = Arc::clone(coordinator.chunk_cache());

        // Create per-mount cancel tokens (children of coordinator's token).
        let cancel_a = coordinator.child_cancel_token();
        let cancel_b = coordinator.child_cancel_token();

        let handle_a = make_mount_handle(
            &shared_cache,
            &tmp.path().join("mount_a/snapshot.sqlite"),
            "crab://bucket/repo-a",
            false,
            cancel_a.clone(),
        );
        let handle_b = make_mount_handle(
            &shared_cache,
            &tmp.path().join("mount_b/snapshot.sqlite"),
            "crab://bucket/repo-b",
            true,
            cancel_b.clone(),
        );

        // Register both mounts.
        let mp_a = PathBuf::from("/mnt/repo-a");
        let mp_b = PathBuf::from("/mnt/repo-b");

        coordinator.add_mount(mp_a.clone(), handle_a).unwrap();
        coordinator.add_mount(mp_b.clone(), handle_b).unwrap();

        assert_eq!(coordinator.mount_count(), 2);
        assert!(coordinator.has_mount(&mp_a));
        assert!(coordinator.has_mount(&mp_b));
        assert_eq!(
            coordinator.mountpoint_for_cache_dir(&tmp.path().join("mount_a")),
            Some(mp_a.clone())
        );
        assert_eq!(
            coordinator.mountpoint_for_cache_dir(&tmp.path().join("mount_b")),
            Some(mp_b.clone())
        );

        let active_cache_mount = coordinator
            .reserve_mount(
                Path::new("/mnt/repo-a-readonly"),
                &tmp.path().join("mount_a"),
            )
            .unwrap_err();
        assert!(
            active_cache_mount
                .to_string()
                .contains("mount already active")
        );

        let conflict_reservation = coordinator
            .reserve_mount(
                Path::new("/mnt/repo-a-conflict"),
                &tmp.path().join("reservation-cache"),
            )
            .unwrap();
        let conflict_cancel = coordinator.child_cancel_token();
        let conflict_handle = make_mount_handle(
            &shared_cache,
            &tmp.path().join("mount_a/conflict-snapshot.sqlite"),
            "crab://bucket/repo-a",
            false,
            conflict_cancel,
        );
        let registration_failure =
            match coordinator.add_reserved_mount(conflict_reservation, conflict_handle) {
                Ok(()) => panic!("conflicting writable mount registered"),
                Err(failure) => failure,
            };
        assert!(
            registration_failure
                .error
                .to_string()
                .contains("mount already active")
        );
        assert!(!registration_failure.handle.cancel_token.is_cancelled());
        assert!(coordinator.pending_mountpoints.is_empty());
        assert!(coordinator.pending_cache_dirs.is_empty());

        coordinator.remove_mount(&mp_a).unwrap();
        assert_eq!(coordinator.mount_count(), 1);
        assert!(!coordinator.cancel_token().is_cancelled());

        // Per-mount cancel token for mount A should be cancelled.
        assert!(cancel_a.is_cancelled());
        // Mount B's cancel token should still be active.
        assert!(!cancel_b.is_cancelled());

        coordinator.remove_mount(&mp_b).unwrap();
        assert_eq!(coordinator.mount_count(), 0);
        assert!(coordinator.cancel_token().is_cancelled());

        // Mount B's cancel token should now be cancelled (child of coordinator).
        assert!(cancel_b.is_cancelled());
    }
}
