//! Multi-repo daemon: shared cache, hydration pool, and repo registry.
//!
//! The daemon manages multiple FUSE or NFS mounts sharing a single process,
//! chunk cache, and hydration worker pool. Content-addressed chunks
//! are deduplicated across repos automatically — two repos built from
//! the same base model share chunks without re-downloading.
//!
//! Lifecycle:
//! 1. Open registry (repos.sqlite)
//! 2. Start shared `ChunkCache` and `HydrationService`
//! 3. For each registered repo: clone, build snapshot, mount, refresh
//! 4. Block until SIGINT → cancel all, unmount, exit

use std::collections::HashMap;
#[cfg(feature = "nfs")]
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use crate::ChunkCache;
use crate::StoreLayout;
use crate::clone_cache::MountCacheLock;
use crate::core::error::{CrabError, Result};
use crate::engine::{OdbReader, VfsEngine};
use crate::hydration::HydrationService;
use crate::integration::{MountReadResolver, NoopMountReadResolver};
#[cfg(feature = "fuse")]
use crate::mount::MountConfig;
use crate::overlay::OverlayStore;
use crate::refresh::{
    GitRemoteRefFetcher, RefreshConfig, RefreshService, redact_url, run_read_tree_head,
};
use crate::resolver::{FuseResolver, OverlayLookup};
use crate::snapshot::SnapshotStore;
use crate::verified_set::VerifiedSet;

// ---------------------------------------------------------------------------
// MountFailure — per-repo mount retry backoff
// ---------------------------------------------------------------------------

/// Tracks mount failure state for exponential backoff retry.
struct MountFailure {
    last_attempt: Instant,
    backoff: Duration,
}

impl MountFailure {
    /// Initial backoff duration for mount failures.
    const INITIAL_BACKOFF: Duration = Duration::from_secs(30);
    /// Maximum backoff duration for mount failures.
    const MAX_BACKOFF: Duration = Duration::from_mins(5);

    fn new() -> Self {
        Self {
            last_attempt: Instant::now(),
            backoff: Self::INITIAL_BACKOFF,
        }
    }

    /// Whether enough time has elapsed to retry.
    fn can_retry(&self) -> bool {
        self.last_attempt.elapsed() >= self.backoff
    }

    /// Record another failure: double the backoff (capped).
    fn record_failure(&mut self) {
        self.last_attempt = Instant::now();
        self.backoff = (self.backoff * 2).min(Self::MAX_BACKOFF);
    }
}

// ---------------------------------------------------------------------------
// RepoConfig — persisted per-repo settings
// ---------------------------------------------------------------------------

/// Configuration for a single daemon-managed repository.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepoConfig {
    /// Unique name for this repo within the daemon.
    pub name: String,
    /// Remote URL to clone from.
    pub remote: String,
    /// Redacted remote URL (credentials replaced with `***`).
    pub remote_redacted: String,
    /// Branch to track.
    pub branch: String,
    /// Root directory under which the repo is mounted (`{mount_root}/{name}/`).
    pub mount_root: String,
    /// Refresh interval in seconds (default 30).
    pub refresh_interval_secs: u64,
    /// Whether this repo is enabled for mounting.
    pub enabled: bool,
    /// Whether this repo should be mounted read-only.
    pub read_only: bool,
    /// Filesystem backend used for this repo.
    pub backend: DaemonMountBackend,
}

/// Filesystem backend for a daemon-managed repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum DaemonMountBackend {
    Fuse,
    Nfs,
}

impl std::fmt::Display for DaemonMountBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Fuse => f.write_str("fuse"),
            Self::Nfs => f.write_str("nfs"),
        }
    }
}

impl std::str::FromStr for DaemonMountBackend {
    type Err = CrabError;

    fn from_str(value: &str) -> Result<Self> {
        match value {
            "fuse" => Ok(Self::Fuse),
            "nfs" => Ok(Self::Nfs),
            _ => Err(CrabError::Configuration {
                key: format!("unsupported daemon mount backend '{value}'"),
                origin: "crab daemon add-repo --backend".into(),
            }),
        }
    }
}

impl RepoConfig {
    /// Compute all derived paths from the daemon root and repo config.
    /// Mirrors artifact-fs's `fillPaths` pattern.
    pub fn computed_paths(&self, daemon_root: &Path) -> ComputedPaths {
        let repo_dir = daemon_root.join("repos").join(&self.name);
        let git_dir = repo_dir.join(".git");
        let overlay_dir = repo_dir.join("overlay/upper");
        let blob_cache_dir = daemon_root.join("cache/blobs").join(&self.name);
        let meta_db_path = repo_dir.join("snapshot.sqlite");
        let overlay_db_path = repo_dir.join("overlay.db");
        let control_endpoint_path = repo_dir.join("nfs-control-endpoint");
        let mount_path = PathBuf::from(&self.mount_root).join(&self.name);

        ComputedPaths {
            repo_dir,
            git_dir,
            overlay_dir,
            blob_cache_dir,
            meta_db_path,
            overlay_db_path,
            control_endpoint_path,
            mount_path,
        }
    }
}

/// Derived filesystem paths for a daemon-managed repo.
#[derive(Debug, Clone)]
pub struct ComputedPaths {
    /// Root directory for this repo's state.
    pub repo_dir: PathBuf,
    /// Path to the `.git` directory.
    pub git_dir: PathBuf,
    /// Path to the overlay upper directory.
    pub overlay_dir: PathBuf,
    /// Path to the blob cache directory.
    pub blob_cache_dir: PathBuf,
    /// Path to the snapshot SQLite database.
    pub meta_db_path: PathBuf,
    /// Path to the overlay SQLite database.
    pub overlay_db_path: PathBuf,
    /// Private endpoint used to control a live daemon-owned NFS mount.
    pub control_endpoint_path: PathBuf,
    /// Path where the filesystem is mounted.
    pub mount_path: PathBuf,
}

// ---------------------------------------------------------------------------
// RepoRuntimeState
// ---------------------------------------------------------------------------

/// Runtime state of a daemon-managed repo.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum RepoRuntimeState {
    /// Registered but not yet mounted.
    Registered,
    /// Clone / snapshot build in progress.
    Initializing,
    /// Filesystem mount active and refresh loop running.
    Running,
    /// Mount failed or was stopped.
    Stopped,
}

impl std::fmt::Display for RepoRuntimeState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Registered => write!(f, "registered"),
            Self::Initializing => write!(f, "initializing"),
            Self::Running => write!(f, "running"),
            Self::Stopped => write!(f, "stopped"),
        }
    }
}

// ---------------------------------------------------------------------------
// RepoRuntime — per-repo live state
// ---------------------------------------------------------------------------

/// Per-repo runtime state held by the daemon while a repo is mounted.
#[allow(dead_code, reason = "fields used when mount pipeline is fully wired")]
struct RepoRuntime {
    config: RepoConfig,
    state: RepoRuntimeState,
    /// HEAD OID from the last snapshot build.
    head_oid: Option<String>,
    /// Snapshot store (SQLite).
    snapshot: Option<Arc<SnapshotStore>>,
    /// Overlay store (SQLite + upper dir).
    overlay: Option<Arc<OverlayStore>>,
    /// Hydration worker join handles.
    hydrator_handles: Vec<JoinHandle<()>>,
    /// VFS resolver.
    resolver: Option<Arc<FuseResolver>>,
    /// Backend-specific mounted session.
    mount_session: Option<RepoMountSession>,
    /// HEAD watcher join handle.
    watcher_handle: Option<JoinHandle<()>>,
    /// Refresh loop join handle.
    refresh_handle: Option<JoinHandle<()>>,
    /// Per-repo cancellation token (child of daemon cancel).
    repo_cancel: CancellationToken,
    /// Computed paths for this repo.
    paths: Option<ComputedPaths>,
    /// Cross-process guard for the repo runtime cache.
    cache_lock: Option<MountCacheLock>,
}

enum RepoMountSession {
    #[cfg(feature = "fuse")]
    Fuse(fuser::BackgroundSession),
    #[cfg(feature = "nfs")]
    Nfs {
        cancel: CancellationToken,
        handle: JoinHandle<Result<()>>,
    },
}

impl RepoMountSession {
    fn is_finished(&self) -> bool {
        match self {
            #[cfg(feature = "fuse")]
            Self::Fuse(_) => false,
            #[cfg(feature = "nfs")]
            Self::Nfs { handle, .. } => handle.is_finished(),
        }
    }
}

// ---------------------------------------------------------------------------
// Registry — SQLite-backed repo persistence
// ---------------------------------------------------------------------------

/// SQLite-backed repo registry persisted at `{daemon-root}/config/repos.sqlite`.
///
/// All methods are synchronous and acquire a `std::sync::Mutex` around
/// the SQLite connection. Callers that invoke Registry methods from an
/// async context **must** wrap them in `tokio::task::spawn_blocking` —
/// holding a `std::sync::Mutex` across a SQLite transaction can block
/// a tokio worker thread under heavy load and block other async work.
pub struct Registry {
    db: std::sync::Mutex<rusqlite::Connection>,
}

impl Registry {
    /// Open or create the registry database.
    pub fn open(db_path: &Path) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let conn = rusqlite::Connection::open(db_path).map_err(map_sqlite_err)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")
            .map_err(map_sqlite_err)?;

        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS repos (
                name TEXT PRIMARY KEY,
                remote TEXT NOT NULL,
                branch TEXT NOT NULL,
                mount_root TEXT NOT NULL,
                refresh_interval_secs INTEGER NOT NULL DEFAULT 30
            )",
        )
        .map_err(map_sqlite_err)?;

        // Migrate: add columns introduced by VFS enhancements.
        Self::migrate_registry_columns(&conn)?;

        debug!(db = %db_path.display(), "daemon registry opened");
        Ok(Self {
            db: std::sync::Mutex::new(conn),
        })
    }

    /// Add columns introduced after the original registry schema.
    fn migrate_registry_columns(conn: &rusqlite::Connection) -> Result<()> {
        let mut has_enabled = false;
        let mut has_read_only = false;
        let mut has_remote_redacted = false;
        let mut has_backend = false;

        let mut stmt = conn
            .prepare("PRAGMA table_info(repos)")
            .map_err(map_sqlite_err)?;
        let rows = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(map_sqlite_err)?;
        for col in rows {
            match col.map_err(map_sqlite_err)?.as_str() {
                "enabled" => has_enabled = true,
                "read_only" => has_read_only = true,
                "remote_redacted" => has_remote_redacted = true,
                "backend" => has_backend = true,
                _ => {}
            }
        }

        if !has_remote_redacted {
            conn.execute_batch(
                "ALTER TABLE repos ADD COLUMN remote_redacted TEXT NOT NULL DEFAULT ''",
            )
            .map_err(map_sqlite_err)?;
            debug!("migrated registry: added remote_redacted column");
        }
        if !has_enabled {
            conn.execute_batch("ALTER TABLE repos ADD COLUMN enabled INTEGER NOT NULL DEFAULT 1")
                .map_err(map_sqlite_err)?;
            debug!("migrated registry: added enabled column");
        }
        if !has_read_only {
            conn.execute_batch("ALTER TABLE repos ADD COLUMN read_only INTEGER NOT NULL DEFAULT 0")
                .map_err(map_sqlite_err)?;
            debug!("migrated registry: added read_only column");
        }
        if !has_backend {
            conn.execute_batch("ALTER TABLE repos ADD COLUMN backend TEXT NOT NULL DEFAULT 'fuse'")
                .map_err(map_sqlite_err)?;
            debug!("migrated registry: added backend column");
        }

        Ok(())
    }

    /// Register a new repo or update an existing one.
    pub fn add_repo(&self, config: &RepoConfig) -> Result<()> {
        let db = self.db.lock().map_err(|_| lock_poisoned())?;
        let redacted = redact_url(&config.remote);
        db.execute(
            "INSERT INTO repos(name, remote, remote_redacted, branch, mount_root, refresh_interval_secs, enabled, read_only, backend)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT(name) DO UPDATE SET
               remote = excluded.remote,
               remote_redacted = excluded.remote_redacted,
               branch = excluded.branch,
               mount_root = excluded.mount_root,
               refresh_interval_secs = excluded.refresh_interval_secs,
               enabled = excluded.enabled,
               read_only = excluded.read_only,
               backend = excluded.backend",
            rusqlite::params![
                config.name,
                config.remote,
                redacted,
                config.branch,
                config.mount_root,
                config.refresh_interval_secs as i64,
                i64::from(config.enabled),
                i64::from(config.read_only),
                config.backend.to_string(),
            ],
        )
        .map_err(map_sqlite_err)?;
        info!(name = %config.name, remote = %redacted, "repo registered");
        Ok(())
    }

    /// Remove a repo from the registry.
    pub fn remove_repo(&self, name: &str) -> Result<bool> {
        let db = self.db.lock().map_err(|_| lock_poisoned())?;
        let rows = db
            .execute("DELETE FROM repos WHERE name = ?1", rusqlite::params![name])
            .map_err(map_sqlite_err)?;
        if rows > 0 {
            info!(name = %name, "repo deregistered");
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// List all registered repos.
    pub fn list_repos(&self) -> Result<Vec<RepoConfig>> {
        let db = self.db.lock().map_err(|_| lock_poisoned())?;
        let mut stmt = db
            .prepare(
                "SELECT name, remote, remote_redacted, branch, mount_root,
                        refresh_interval_secs, enabled, read_only, backend
                 FROM repos ORDER BY name",
            )
            .map_err(map_sqlite_err)?;

        let rows = stmt
            .query_map([], |row| {
                Ok(RepoConfig {
                    name: row.get(0)?,
                    remote: row.get(1)?,
                    remote_redacted: row.get(2)?,
                    branch: row.get(3)?,
                    mount_root: row.get(4)?,
                    refresh_interval_secs: row.get::<_, i64>(5)? as u64,
                    enabled: row.get::<_, i64>(6)? != 0,
                    read_only: row.get::<_, i64>(7)? != 0,
                    backend: parse_backend_column(row.get::<_, String>(8)?)?,
                })
            })
            .map_err(map_sqlite_err)?;

        let mut configs = Vec::new();
        for row in rows {
            configs.push(row.map_err(map_sqlite_err)?);
        }
        Ok(configs)
    }

    /// Get a single repo by name.
    pub fn get_repo(&self, name: &str) -> Result<Option<RepoConfig>> {
        let db = self.db.lock().map_err(|_| lock_poisoned())?;
        let result = db
            .query_row(
                "SELECT name, remote, remote_redacted, branch, mount_root,
                        refresh_interval_secs, enabled, read_only, backend
                 FROM repos WHERE name = ?1",
                rusqlite::params![name],
                |row| {
                    Ok(RepoConfig {
                        name: row.get(0)?,
                        remote: row.get(1)?,
                        remote_redacted: row.get(2)?,
                        branch: row.get(3)?,
                        mount_root: row.get(4)?,
                        refresh_interval_secs: row.get::<_, i64>(5)? as u64,
                        enabled: row.get::<_, i64>(6)? != 0,
                        read_only: row.get::<_, i64>(7)? != 0,
                        backend: parse_backend_column(row.get::<_, String>(8)?)?,
                    })
                },
            )
            .optional()
            .map_err(map_sqlite_err)?;
        Ok(result)
    }

    /// Update the refresh interval for a repo.
    pub fn set_refresh_interval(&self, name: &str, interval_secs: u64) -> Result<bool> {
        let db = self.db.lock().map_err(|_| lock_poisoned())?;
        let rows = db
            .execute(
                "UPDATE repos SET refresh_interval_secs = ?1 WHERE name = ?2",
                rusqlite::params![interval_secs as i64, name],
            )
            .map_err(map_sqlite_err)?;
        Ok(rows > 0)
    }

    /// Enable a repo for mounting on the next sync cycle.
    pub fn enable_repo(&self, name: &str) -> Result<bool> {
        let db = self.db.lock().map_err(|_| lock_poisoned())?;
        let rows = db
            .execute(
                "UPDATE repos SET enabled = 1 WHERE name = ?1",
                rusqlite::params![name],
            )
            .map_err(map_sqlite_err)?;
        if rows > 0 {
            info!(name = %name, "repo enabled");
        }
        Ok(rows > 0)
    }

    /// Disable a repo. The daemon will unmount it on the next sync cycle.
    pub fn disable_repo(&self, name: &str) -> Result<bool> {
        let db = self.db.lock().map_err(|_| lock_poisoned())?;
        let rows = db
            .execute(
                "UPDATE repos SET enabled = 0 WHERE name = ?1",
                rusqlite::params![name],
            )
            .map_err(map_sqlite_err)?;
        if rows > 0 {
            info!(name = %name, "repo disabled");
        }
        Ok(rows > 0)
    }

    /// Set the read-only flag for a repo.
    pub fn set_read_only(&self, name: &str, read_only: bool) -> Result<bool> {
        let db = self.db.lock().map_err(|_| lock_poisoned())?;
        let rows = db
            .execute(
                "UPDATE repos SET read_only = ?1 WHERE name = ?2",
                rusqlite::params![i64::from(read_only), name],
            )
            .map_err(map_sqlite_err)?;
        Ok(rows > 0)
    }
}

use rusqlite::OptionalExtension;

fn parse_backend_column(value: String) -> rusqlite::Result<DaemonMountBackend> {
    value.parse().map_err(|error: CrabError| {
        rusqlite::Error::FromSqlConversionFailure(8, rusqlite::types::Type::Text, Box::new(error))
    })
}

// ---------------------------------------------------------------------------
// DaemonService
// ---------------------------------------------------------------------------

/// Multi-repo daemon managing shared resources and per-repo mounts.
pub struct DaemonService {
    /// Root directory for daemon state.
    root: PathBuf,
    /// Shared chunk cache across all repos.
    cache: Arc<ChunkCache>,
    /// Repo registry (SQLite).
    registry: Arc<Registry>,
    /// Per-repo runtime state.
    running: RwLock<HashMap<String, RepoRuntime>>,
    /// Per-repo mount failure backoff tracking.
    mount_failures: RwLock<HashMap<String, MountFailure>>,
    /// Cancellation token for graceful shutdown.
    cancel: CancellationToken,
    /// Product-owned credential and replica resolver for Crab remotes.
    read_resolver: Arc<dyn MountReadResolver>,
}

impl DaemonService {
    /// Create a new daemon service.
    ///
    /// Opens the registry and chunk cache but does not start any mounts.
    /// Call [`start`] to mount all registered repos.
    pub fn new(root: PathBuf, cancel: CancellationToken) -> Result<Self> {
        let registry_path = root.join("config/repos.sqlite");
        let registry = Registry::open(&registry_path)?;

        let cache_dir = root.join("cache/chunks");
        let cache = ChunkCache::open(cache_dir, None)?;

        info!(root = %root.display(), "daemon service created");
        Ok(Self {
            root,
            cache: Arc::new(cache),
            registry: Arc::new(registry),
            running: RwLock::new(HashMap::new()),
            mount_failures: RwLock::new(HashMap::new()),
            cancel,
            read_resolver: Arc::new(NoopMountReadResolver),
        })
    }

    /// Provide credential and replica-aware read resolution for Crab remotes.
    #[must_use]
    pub fn with_read_resolver(mut self, resolver: Arc<dyn MountReadResolver>) -> Self {
        self.read_resolver = resolver;
        self
    }

    /// Start the daemon: mount all registered repos.
    ///
    /// For each registered repo, attempts to initialize and mount it.
    /// Repos that fail to mount are logged as warnings but don't prevent
    /// other repos from starting.
    pub async fn start(&self) -> Result<()> {
        let repos = self.registry.list_repos()?;
        info!(
            repo_count = repos.len(),
            "starting daemon, mounting registered repos"
        );

        for config in &repos {
            if let Err(e) = self.start_repo(config).await {
                warn!(
                    name = %config.name,
                    error = %e,
                    "failed to start repo, skipping"
                );
            }
        }

        Ok(())
    }

    /// Reconcile running repos with the registry.
    ///
    /// Mounts new/enabled repos, unmounts removed/disabled repos, and
    /// respects mount failure backoff for previously failed repos.
    pub async fn sync_repos(&self) -> Result<()> {
        let configs = self.registry.list_repos()?;
        // Unmount repos that were removed from the registry or disabled.
        {
            let running = self.running.read().await;
            let to_remove: Vec<(String, bool, bool)> = running
                .iter()
                .filter_map(|(name, runtime)| {
                    let unexpected_exit = runtime
                        .mount_session
                        .as_ref()
                        .is_some_and(RepoMountSession::is_finished);
                    let registered = configs.iter().find(|config| config.name == *name);
                    let removed_or_disabled = registered.is_none_or(|config| !config.enabled);
                    let configuration_changed = registered
                        .is_some_and(|config| config.enabled && *config != runtime.config);
                    (removed_or_disabled || configuration_changed || unexpected_exit)
                        .then(|| (name.clone(), unexpected_exit, configuration_changed))
                })
                .collect();
            drop(running);

            for (name, unexpected_exit, configuration_changed) in &to_remove {
                let runtime = self.running.write().await.remove(name);
                if let Some(mut runtime) = runtime {
                    if let Err(error) = self.teardown_runtime(&mut runtime).await {
                        warn!(name = %name, error = %error, "repo teardown failed during sync");
                    }
                    runtime.state = RepoRuntimeState::Stopped;
                    if *unexpected_exit {
                        let mut failures = self.mount_failures.write().await;
                        failures
                            .entry(name.clone())
                            .and_modify(MountFailure::record_failure)
                            .or_insert_with(MountFailure::new);
                        warn!(name = %name, "repo mount session exited unexpectedly");
                    } else if *configuration_changed {
                        info!(name = %name, "repo unmounted for configuration change");
                    } else {
                        info!(name = %name, "repo unmounted (removed or disabled)");
                    }
                }
            }
        }

        // Mount new/enabled repos that aren't already running.
        for config in &configs {
            if !config.enabled {
                continue;
            }

            // Already running?
            {
                let running = self.running.read().await;
                if running.contains_key(&config.name) {
                    continue;
                }
            }

            // Check mount failure backoff.
            {
                let failures = self.mount_failures.read().await;
                if let Some(failure) = failures.get(&config.name)
                    && !failure.can_retry()
                {
                    debug!(
                        name = %config.name,
                        backoff = ?failure.backoff,
                        "skipping mount — backoff not elapsed"
                    );
                    continue;
                }
            }

            match self.start_repo(config).await {
                Ok(()) => {
                    // Clear any previous failure record on success.
                    let mut failures = self.mount_failures.write().await;
                    failures.remove(&config.name);
                }
                Err(e) => {
                    warn!(
                        name = %config.name,
                        error = %e,
                        "mount failed during sync"
                    );
                    let mut failures = self.mount_failures.write().await;
                    failures
                        .entry(config.name.clone())
                        .and_modify(MountFailure::record_failure)
                        .or_insert_with(MountFailure::new);
                }
            }
        }

        Ok(())
    }

    /// Stop the daemon: teardown all repos in reverse order, then exit.
    pub async fn stop(&self) -> Result<()> {
        info!("stopping daemon");
        self.cancel.cancel();

        let runtimes: Vec<(String, RepoRuntime)> = {
            let mut running = self.running.write().await;
            running.drain().collect()
        };
        let mut first_error = None;
        for (name, mut runtime) in runtimes {
            info!(name = %name, "tearing down repo");
            if let Err(error) = self.teardown_runtime(&mut runtime).await {
                warn!(name = %name, error = %error, "repo teardown failed");
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
            runtime.state = RepoRuntimeState::Stopped;
            info!(name = %name, "repo stopped");
        }
        info!("daemon stopped");
        first_error.map_or(Ok(()), Err)
    }

    /// Start a single repo: full mount pipeline.
    ///
    /// Pipeline: blobless clone → resolve HEAD → snapshot build → overlay
    /// open → reconcile → read-tree HEAD → hydration start → resolver →
    /// engine (with ODB reader) → filesystem mount → HEAD watcher → refresh loop.
    ///
    /// On failure at any step: log step name + error, cleanup partial
    /// resources, set state to Stopped.
    async fn start_repo(&self, config: &RepoConfig) -> Result<()> {
        #[cfg(not(feature = "nfs"))]
        if config.backend == DaemonMountBackend::Nfs {
            return Err(CrabError::Configuration {
                key: "NFS support was not compiled into this Crab build".into(),
                origin: "crab daemon".into(),
            });
        }
        #[cfg(not(feature = "fuse"))]
        if config.backend == DaemonMountBackend::Fuse {
            return Err(CrabError::Configuration {
                key: "FUSE support was not compiled into this Crab build".into(),
                origin: "crab daemon".into(),
            });
        }

        let redacted = if config.remote_redacted.is_empty() {
            redact_url(&config.remote)
        } else {
            config.remote_redacted.clone()
        };
        let _span = tracing::info_span!(
            "daemon_start_repo",
            name = %config.name,
            remote = %redacted,
            branch = %config.branch,
        )
        .entered();

        info!("initializing repo");

        let repo_cancel = self.cancel.child_token();
        let paths = config.computed_paths(&self.root);
        let cache_lock = MountCacheLock::acquire(&paths.repo_dir)?;

        // Ensure repo directory exists.
        std::fs::create_dir_all(&paths.repo_dir)?;

        let runtime = RepoRuntime {
            config: config.clone(),
            state: RepoRuntimeState::Initializing,
            head_oid: None,
            snapshot: None,
            overlay: None,
            hydrator_handles: Vec::new(),
            resolver: None,
            mount_session: None,
            watcher_handle: None,
            refresh_handle: None,
            repo_cancel: repo_cancel.clone(),
            paths: Some(paths.clone()),
            cache_lock: Some(cache_lock),
        };

        {
            let mut running = self.running.write().await;
            running.insert(config.name.clone(), runtime);
        }

        // Execute the pipeline, cleaning up on failure.
        match self
            .execute_mount_pipeline(config, &paths, &repo_cancel)
            .await
        {
            Ok(()) => {
                let mut running = self.running.write().await;
                if let Some(rt) = running.get_mut(&config.name) {
                    rt.state = RepoRuntimeState::Running;
                }
                info!(name = %config.name, "repo mounted and running");
                Ok(())
            }
            Err(e) => {
                warn!(
                    name = %config.name,
                    error = %e,
                    "mount pipeline failed, cleaning up"
                );
                // Cancel any spawned tasks for this repo.
                repo_cancel.cancel();
                // Clean up partial runtime state.
                let runtime = self.running.write().await.remove(&config.name);
                if let Some(mut runtime) = runtime
                    && let Err(cleanup_error) = self.teardown_runtime(&mut runtime).await
                {
                    warn!(
                        name = %config.name,
                        error = %cleanup_error,
                        "failed to clean up partial mount pipeline"
                    );
                }
                Err(e)
            }
        }
    }

    /// Execute the full mount pipeline for a repo.
    ///
    /// Each step is logged. On failure, the caller handles cleanup.
    async fn execute_mount_pipeline(
        &self,
        config: &RepoConfig,
        paths: &ComputedPaths,
        repo_cancel: &CancellationToken,
    ) -> Result<()> {
        // Step 1: Blobless clone (if git dir doesn't exist yet).
        if !paths.git_dir.exists() {
            info!(step = "clone", "cloning repository (blobless)");
            // SHELLOUT: delegating full clone to `git clone`.
            // Gitoxide's `gix::clone` exists but re-implementing
            // crab's blobless partial-clone flow against it
            // would be materially more code than keeping the
            // shellout; the Keep table in `requirements.md`
            // (Per-Site Decision Matrix) covers this site.

            // Ensure the parent directory (repo_dir) exists so git can
            // create the bare clone inside it at `git_dir`.
            std::fs::create_dir_all(&paths.repo_dir)?;

            let output = std::process::Command::new("git")
                .args([
                    "clone",
                    "--bare",
                    "--filter=blob:none",
                    "--single-branch",
                    "--branch",
                    &config.branch,
                    &config.remote,
                    &paths.git_dir.to_string_lossy(),
                ])
                .output()
                .map_err(|e| {
                    error!(step = "clone", error = %e, "failed to spawn git clone");
                    CrabError::Io(e)
                })?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let redacted_err = redact_url(stderr.trim());
                error!(step = "clone", error = %redacted_err, "blobless clone failed");
                return Err(CrabError::Internal(format!(
                    "blobless clone failed: {redacted_err}"
                )));
            }
        }

        // Step 2: Resolve HEAD → (oid, ref).
        info!(step = "resolve_head", "resolving HEAD");
        let (head_oid, head_ref) = crate::pipeline::resolve_head(&paths.git_dir)?;
        debug!(step = "resolve_head", oid = %head_oid, ref_name = %head_ref);

        // Step 3: Open snapshot store and build initial generation.
        info!(step = "snapshot", "building snapshot from HEAD");
        let snapshot = Arc::new(SnapshotStore::open_or_create(&paths.meta_db_path).map_err(
            |e| {
                error!(step = "snapshot", error = %e, "failed to open snapshot store");
                e
            },
        )?);

        snapshot
            .publish_generation_from_git(&paths.git_dir, &head_oid, &head_ref)
            .map_err(|e| {
                error!(step = "snapshot", error = %e, "failed to build and publish snapshot");
                e
            })?;

        let generation = snapshot
            .current_generation()?
            .ok_or_else(|| CrabError::Internal("no generation after publish".into()))?;

        // Step 4: Open overlay store (unless read-only).
        let overlay: Option<Arc<OverlayStore>> = if config.read_only {
            info!(step = "overlay", "read-only mode, skipping overlay");
            None
        } else {
            info!(step = "overlay", "opening overlay store");
            let ov = Arc::new(
                OverlayStore::open_with_orphan_cleanup(&paths.overlay_db_path, &paths.overlay_dir)
                    .map_err(|e| {
                        error!(step = "overlay", error = %e, "failed to open overlay store");
                        e
                    })?,
            );

            // Step 5: Reconcile overlay against current HEAD.
            info!(step = "reconcile", "reconciling overlay against HEAD");
            let snap_ref = &snapshot;
            let current_gen = generation;
            ov.reconcile(|path| {
                let node = snap_ref.get_node(current_gen, path).ok().flatten()?;
                Some(crate::overlay::ReconcileBaseInfo {
                    is_dir: node.node_type == crate::snapshot::NodeType::Dir,
                    object_oid: node.object_oid.clone(),
                })
            })
            .map_err(|e| {
                error!(step = "reconcile", error = %e, "overlay reconciliation failed");
                e
            })?;

            Some(ov)
        };

        // Step 6: Run git read-tree HEAD.
        info!(step = "read_tree", "running git read-tree HEAD");
        run_read_tree_head(&paths.git_dir);

        // Step 7: Create hydration service with shared cache.
        info!(step = "hydration", "starting hydration service");
        let read_context = self.read_resolver.resolve(&config.remote).await?;
        if crab_git::CrabUrl::parse(&config.remote).is_ok() && read_context.is_none() {
            return Err(CrabError::Configuration {
                key: "object-store read layout unavailable for daemon repo".into(),
                origin: "crab daemon".into(),
            });
        }
        let store_layout = read_context
            .as_ref()
            .map(|context| context.store_layout.clone());
        let read_hydrator = read_context.map(|context| context.hydrator);
        let read_range_cache_dir = store_layout
            .as_ref()
            .map(|_| paths.repo_dir.join("read_ranges"));
        let verified = Arc::new(VerifiedSet::default());
        let hydration = crate::pipeline::create_hydration(
            Arc::clone(&self.cache),
            verified,
            repo_cancel.clone(),
            store_layout,
            read_hydrator,
            read_range_cache_dir,
        )?;
        let hydrator_handles = hydration.spawn_workers();

        // Step 8: Create ODB reader.
        let odb_reader = OdbReader::new(&paths.git_dir, &paths.blob_cache_dir).map_err(|e| {
            error!(step = "odb_reader", error = %e, "failed to create ODB reader");
            e
        })?;

        // Step 9: Create resolver (snapshot + overlay).
        info!(step = "resolver", "creating VFS resolver");
        let commit_time = crate::pipeline::commit_time_from_head(&paths.git_dir).unwrap_or(0);
        let overlay_lookup: Option<Arc<dyn OverlayLookup>> = overlay
            .as_ref()
            .map(|ov| Arc::clone(ov) as Arc<dyn OverlayLookup>);
        let resolver = Arc::new(FuseResolver::new(
            Arc::clone(&snapshot),
            overlay_lookup,
            generation,
            commit_time,
        ));

        // Step 10: Create engine (resolver + overlay + hydration + ODB reader).
        info!(step = "engine", "creating VFS engine");
        let overlay_writer: Option<Arc<dyn crate::engine::OverlayWriter>> = overlay
            .as_ref()
            .map(|ov| Arc::clone(ov) as Arc<dyn crate::engine::OverlayWriter>);
        let engine = Arc::new(VfsEngine::new(
            Arc::clone(&resolver),
            overlay_writer,
            Arc::clone(&hydration),
            Some(odb_reader),
            Some(Arc::clone(&snapshot)),
        ));

        // Step 11: Mount the configured filesystem backend.
        std::fs::create_dir_all(&paths.mount_path)?;
        let mount_session = match config.backend {
            DaemonMountBackend::Fuse => {
                #[cfg(feature = "fuse")]
                {
                    info!(step = "fuse_mount", "mounting FUSE filesystem");
                    let mount_config = MountConfig {
                        mountpoint: paths.mount_path.clone(),
                        git_dir: paths.git_dir.to_string_lossy().into_owned(),
                        write_pid: true,
                        crab_dir: paths.repo_dir.join(".crab"),
                        read_only: config.read_only,
                    };
                    let mounted = crate::mount::mount(
                        &mount_config,
                        Arc::clone(&resolver),
                        Arc::clone(&engine),
                        tokio::runtime::Handle::current(),
                    )
                    .map_err(|e| {
                        error!(step = "fuse_mount", error = %e, "FUSE mount failed");
                        e
                    })?;
                    let session = mounted.session.spawn().map_err(|e| {
                        error!(step = "fuse_mount", error = %e, "failed to spawn FUSE background session");
                        CrabError::Internal(format!("FUSE background session: {e}"))
                    })?;
                    RepoMountSession::Fuse(session)
                }
                #[cfg(not(feature = "fuse"))]
                {
                    return Err(CrabError::Configuration {
                        key: "FUSE support was not compiled into this Crab build".into(),
                        origin: "crab daemon".into(),
                    });
                }
            }
            DaemonMountBackend::Nfs => {
                #[cfg(feature = "nfs")]
                {
                    info!(step = "nfs_mount", "mounting NFS filesystem");
                    let control_endpoint =
                        crate::nfs_control::fresh_endpoint_for_mountpoint(&paths.mount_path)?;
                    let mount_config = crate::nfs_mount::NfsMountConfig {
                        mountpoint: paths.mount_path.clone(),
                        git_dir: paths.git_dir.to_string_lossy().into_owned(),
                        exclusive_verifiers_path: paths
                            .repo_dir
                            .join("nfs-exclusive-verifiers.json"),
                        read_only: config.read_only,
                        control_endpoint_override: control_endpoint.clone(),
                    };
                    crate::nfs_mount::preflight_for_config(&mount_config).ensure_ready()?;
                    persist_nfs_control_endpoint(paths, control_endpoint.as_deref())?;
                    let control_runtime = crate::nfs_control::NfsMountRuntime {
                        output: crate::pipeline::PipelineOutput {
                            resolver: Arc::clone(&resolver),
                            engine: Arc::clone(&engine),
                            hydration: Arc::clone(&hydration),
                            snapshot: Arc::clone(&snapshot),
                            overlay: overlay.as_ref().map(Arc::clone),
                            head_oid: head_oid.clone(),
                            head_ref: head_ref.clone(),
                            generation,
                            hydrator_handles: Vec::new(),
                        },
                        config: crate::pipeline::PipelineConfig {
                            source: config.remote.clone(),
                            git_dir: paths.git_dir.clone(),
                            ref_name: Some(head_ref.clone()),
                            read_only: config.read_only,
                            cache_dir: paths.repo_dir.clone(),
                            cancel_token: repo_cancel.clone(),
                        },
                    };
                    let mounted = crate::nfs_mount::mount(
                        &mount_config,
                        Arc::clone(&resolver),
                        Arc::clone(&engine),
                        Some(control_runtime),
                    )
                    .await
                    .map_err(|e| {
                        error!(step = "nfs_mount", error = %e, "NFS mount failed");
                        e
                    })?;
                    let cancel = repo_cancel.child_token();
                    let task_cancel = cancel.clone();
                    let handle = tokio::spawn(async move {
                        crate::nfs_mount::run_until_cancelled(mounted, task_cancel).await
                    });
                    if let Err(error) =
                        wait_for_nfs_control(control_endpoint.as_deref(), &handle).await
                    {
                        cancel.cancel();
                        let _ = handle.await;
                        return Err(error);
                    }
                    RepoMountSession::Nfs { cancel, handle }
                }
                #[cfg(not(feature = "nfs"))]
                {
                    return Err(CrabError::Configuration {
                        key: "NFS support was not compiled into this Crab build".into(),
                        origin: "crab daemon".into(),
                    });
                }
            }
        };

        // Step 12: Start refresh loop (if not read-only and overlay exists).
        let mut refresh_handle = if let Some(ref ov) = overlay {
            info!(step = "refresh", "starting refresh loop");
            let refresh_config = RefreshConfig {
                remote_poll_interval: Duration::from_secs(config.refresh_interval_secs),
                local_poll_interval: Duration::from_millis(500),
                git_dir: paths.git_dir.clone(),
                tracked_ref: Some(tracked_branch_ref(&config.branch)),
            };

            let fetcher = Arc::new(GitRemoteRefFetcher::new(paths.git_dir.clone()));
            let refresh_svc = Arc::new(RefreshService::new(
                Arc::clone(&resolver),
                Arc::clone(&snapshot),
                Arc::clone(ov),
                fetcher,
                refresh_config,
                repo_cancel.clone(),
            ));

            let handle = tokio::spawn(async move {
                refresh_svc.run().await;
            });
            Some(handle)
        } else {
            None
        };

        // Update runtime with all components.
        let mut hydrator_handles = Some(hydrator_handles);
        let mut mount_session = Some(mount_session);
        let installed = {
            let mut running = self.running.write().await;
            if let Some(rt) = running.get_mut(&config.name) {
                rt.head_oid = Some(head_oid);
                rt.snapshot = Some(snapshot);
                rt.overlay = overlay;
                rt.hydrator_handles = hydrator_handles.take().unwrap_or_default();
                rt.resolver = Some(resolver);
                rt.mount_session = mount_session.take();
                rt.refresh_handle = refresh_handle.take();
                true
            } else {
                false
            }
        };
        if !installed {
            repo_cancel.cancel();
            if let Some(handle) = refresh_handle {
                handle.abort();
            }
            if let Some(handles) = hydrator_handles {
                for handle in handles {
                    handle.abort();
                }
            }
            if let Some(session) = mount_session {
                let shutdown_result =
                    shutdown_mount_session(&config.name, session, Some(paths)).await;
                remove_nfs_control_endpoint(paths);
                shutdown_result?;
            }
            return Err(CrabError::NotFound {
                path: format!("daemon repo '{}' was removed while mounting", config.name),
            });
        }

        Ok(())
    }

    /// Teardown a repo runtime in reverse order.
    ///
    /// Cancel refresh → stop watcher → unmount backend → stop hydration
    /// workers → close overlay → close snapshot.
    async fn teardown_runtime(&self, rt: &mut RepoRuntime) -> Result<()> {
        let name = &rt.config.name;

        // 1. Cancel refresh loop.
        if let Some(handle) = rt.refresh_handle.take() {
            handle.abort();
            debug!(name = %name, "refresh loop cancelled");
        }

        // 2. Stop HEAD watcher.
        if let Some(handle) = rt.watcher_handle.take() {
            handle.abort();
            debug!(name = %name, "watcher stopped");
        }

        // 3. Unmount the backend before releasing any state used by requests.
        let mount_result = match rt.mount_session.take() {
            Some(session) => shutdown_mount_session(name, session, rt.paths.as_ref()).await,
            None => Ok(()),
        };

        // 4. Stop hydration workers.
        for handle in rt.hydrator_handles.drain(..) {
            handle.abort();
        }
        debug!(name = %name, "hydration workers stopped");

        // 5. Close overlay (drop Arc).
        rt.overlay = None;
        debug!(name = %name, "overlay closed");

        // 6. Close snapshot (drop Arc).
        rt.snapshot = None;
        debug!(name = %name, "snapshot closed");

        // 7. Drop resolver.
        rt.resolver = None;

        // 8. Release cache ownership after all cache-backed state is closed.
        if let Some(paths) = &rt.paths {
            remove_nfs_control_endpoint(paths);
        }
        rt.cache_lock = None;

        // Cancel repo-level token.
        rt.repo_cancel.cancel();
        mount_result
    }

    /// Add a repo to the registry and optionally start it.
    pub async fn add_repo(&self, config: &RepoConfig) -> Result<()> {
        self.registry.add_repo(config)?;

        // If the daemon is running (not cancelled), try to start the repo.
        if !self.cancel.is_cancelled()
            && let Err(e) = self.start_repo(config).await
        {
            warn!(
                name = %config.name,
                error = %e,
                "repo registered but failed to start"
            );
        }

        Ok(())
    }

    /// Remove a repo: stop it and deregister from the registry.
    pub async fn remove_repo(&self, name: &str) -> Result<()> {
        // Stop the repo if running.
        let runtime = self.running.write().await.remove(name);
        if let Some(mut runtime) = runtime {
            self.teardown_runtime(&mut runtime).await?;
            runtime.state = RepoRuntimeState::Stopped;
            info!(name = %name, "repo unmounted");
        }

        // Clear any mount failure backoff record.
        {
            let mut failures = self.mount_failures.write().await;
            failures.remove(name);
        }

        // Deregister from the registry.
        let removed = self.registry.remove_repo(name)?;
        if !removed {
            return Err(CrabError::NotFound {
                path: format!("repo '{name}' not found in registry"),
            });
        }

        Ok(())
    }

    /// List all registered repos with their runtime state.
    pub async fn list_repos(&self) -> Result<Vec<RepoStatus>> {
        let configs = self.registry.list_repos()?;
        let running = self.running.read().await;

        let mut statuses = Vec::with_capacity(configs.len());
        for config in &configs {
            let status = match running.get(&config.name) {
                Some(rt) => {
                    let (dirty_count, dirty_paths) = rt.overlay.as_ref().map_or_else(
                        || (0, Vec::new()),
                        |overlay| publishable_overlay_state(overlay),
                    );
                    let generation = rt
                        .snapshot
                        .as_ref()
                        .and_then(|s| s.current_generation().ok().flatten());
                    let head_oid = rt
                        .snapshot
                        .as_ref()
                        .and_then(|s| s.head_oid().ok().flatten())
                        .or_else(|| rt.head_oid.clone());
                    RepoStatus {
                        name: config.name.clone(),
                        remote: config.remote.clone(),
                        remote_redacted: config.remote_redacted.clone(),
                        branch: config.branch.clone(),
                        mount_root: config.mount_root.clone(),
                        refresh_interval_secs: config.refresh_interval_secs,
                        state: rt.state,
                        head_oid,
                        generation,
                        dirty_count,
                        dirty_paths,
                        last_fetch_at: None,
                        enabled: config.enabled,
                        read_only: config.read_only,
                        backend: config.backend,
                        is_live: true,
                    }
                }
                None => RepoStatus {
                    name: config.name.clone(),
                    remote: config.remote.clone(),
                    remote_redacted: config.remote_redacted.clone(),
                    branch: config.branch.clone(),
                    mount_root: config.mount_root.clone(),
                    refresh_interval_secs: config.refresh_interval_secs,
                    state: RepoRuntimeState::Registered,
                    head_oid: None,
                    generation: None,
                    dirty_count: 0,
                    dirty_paths: Vec::new(),
                    last_fetch_at: None,
                    enabled: config.enabled,
                    read_only: config.read_only,
                    backend: config.backend,
                    is_live: false,
                },
            };
            statuses.push(status);
        }

        Ok(statuses)
    }

    /// Get detailed status for a single repo.
    pub async fn repo_status(&self, name: &str) -> Result<RepoStatus> {
        let config = self
            .registry
            .get_repo(name)?
            .ok_or_else(|| CrabError::NotFound {
                path: format!("repo '{name}' not found in registry"),
            })?;

        let running = self.running.read().await;
        match running.get(name) {
            Some(rt) => {
                let (dirty_count, dirty_paths) = rt.overlay.as_ref().map_or_else(
                    || (0, Vec::new()),
                    |overlay| publishable_overlay_state(overlay),
                );
                let generation = rt
                    .snapshot
                    .as_ref()
                    .and_then(|s| s.current_generation().ok().flatten());
                let head_oid = rt
                    .snapshot
                    .as_ref()
                    .and_then(|s| s.head_oid().ok().flatten())
                    .or_else(|| rt.head_oid.clone());
                Ok(RepoStatus {
                    name: config.name,
                    remote: config.remote,
                    remote_redacted: config.remote_redacted,
                    branch: config.branch,
                    mount_root: config.mount_root,
                    refresh_interval_secs: config.refresh_interval_secs,
                    state: rt.state,
                    head_oid,
                    generation,
                    dirty_count,
                    dirty_paths,
                    last_fetch_at: None,
                    enabled: config.enabled,
                    read_only: config.read_only,
                    backend: config.backend,
                    is_live: true,
                })
            }
            None => {
                // Out-of-process: reconstruct from persisted stores.
                Ok(read_persisted_status(&config, &self.root))
            }
        }
    }

    /// Update the refresh interval for a repo.
    pub fn set_refresh_interval(&self, name: &str, interval_secs: u64) -> Result<()> {
        let updated = self.registry.set_refresh_interval(name, interval_secs)?;
        if !updated {
            return Err(CrabError::NotFound {
                path: format!("repo '{name}' not found in registry"),
            });
        }
        info!(name = %name, interval_secs, "refresh interval updated");
        Ok(())
    }

    /// Remount a repo: stop it and re-execute the full mount pipeline.
    ///
    /// Unmounts the active session, cancels the refresh loop, and re-starts
    /// the repo with a fresh snapshot from the current HEAD. If the
    /// re-mount fails, the repo is left in `Stopped` state and remains
    /// eligible for retry on the next `sync_repos` cycle.
    pub async fn remount_repo(&self, name: &str) -> Result<()> {
        let config = self
            .registry
            .get_repo(name)?
            .ok_or_else(|| CrabError::NotFound {
                path: format!("repo '{name}'"),
            })?;

        // Stop the repo if running.
        let runtime = self.running.write().await.remove(name);
        if let Some(mut runtime) = runtime {
            self.teardown_runtime(&mut runtime).await?;
            runtime.state = RepoRuntimeState::Stopped;
            info!(name = %name, "repo stopped for remount");
        }

        // Re-start with fresh snapshot.
        self.start_repo(&config).await?;
        info!(name = %name, "repo remounted");
        Ok(())
    }

    /// Trigger an immediate `git fetch` for a repo, bypassing the refresh timer.
    ///
    /// Runs `git fetch` in the repo's git directory and updates the
    /// runtime state with the result.
    pub async fn force_fetch(&self, name: &str) -> Result<()> {
        let config = self
            .registry
            .get_repo(name)?
            .ok_or_else(|| CrabError::NotFound {
                path: format!("repo '{name}'"),
            })?;

        let redacted = if config.remote_redacted.is_empty() {
            redact_url(&config.remote)
        } else {
            config.remote_redacted.clone()
        };

        let paths = config.computed_paths(&self.root);
        let git_dir = paths.git_dir;
        let ref_name = tracked_branch_ref(&config.branch);
        let refspec = format!("+{ref_name}:{ref_name}");

        info!(name = %name, remote = %redacted, "force-fetching");

        // SHELLOUT: delegating fetch to `git fetch origin`.
        // `gix-protocol` does fetch but re-implementing
        // crab's multi-remote fetch-all loop on top of it is
        // more code than the shellout; same Keep-table rationale
        // as the clone above.
        let output = std::process::Command::new("git")
            .args(["fetch", "origin", &refspec])
            .env("GIT_DIR", &git_dir)
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .output()
            .map_err(CrabError::Io)?;

        if output.status.success() {
            info!(name = %name, "force-fetch succeeded");
            let mut running = self.running.write().await;
            if let Some(_rt) = running.get_mut(name) {
                // Re-read HEAD after fetch.
                run_read_tree_head(&git_dir);
            }
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let redacted_err = redact_url(stderr.trim());
            warn!(name = %name, error = %redacted_err, "force-fetch failed");
            return Err(CrabError::Internal(format!(
                "git fetch failed for '{name}': {redacted_err}"
            )));
        }

        Ok(())
    }

    /// Enable a repo in the registry.
    pub fn enable_repo(&self, name: &str) -> Result<()> {
        let updated = self.registry.enable_repo(name)?;
        if !updated {
            return Err(CrabError::NotFound {
                path: format!("repo '{name}'"),
            });
        }
        info!(name = %name, "repo enabled");
        Ok(())
    }

    /// Disable a repo in the registry.
    ///
    /// The repo will be unmounted on the next `sync_repos` cycle.
    pub fn disable_repo(&self, name: &str) -> Result<()> {
        let updated = self.registry.disable_repo(name)?;
        if !updated {
            return Err(CrabError::NotFound {
                path: format!("repo '{name}'"),
            });
        }
        info!(name = %name, "repo disabled");
        Ok(())
    }

    /// Access the shared chunk cache.
    pub fn cache(&self) -> &Arc<ChunkCache> {
        &self.cache
    }

    /// Access the registry.
    pub fn registry(&self) -> &Arc<Registry> {
        &self.registry
    }

    /// Daemon root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Run the daemon: initial mount + 3-second polling loop.
    ///
    /// Blocks until the `CancellationToken` is cancelled (SIGINT/SIGTERM).
    /// On exit, tears down all repos and returns.
    pub async fn run(&self) -> Result<()> {
        // Initial mount of all registered repos.
        self.sync_repos().await?;

        let mut ticker = tokio::time::interval(Duration::from_secs(3));
        loop {
            tokio::select! {
                () = self.cancel.cancelled() => break,
                _ = ticker.tick() => {
                    if let Err(e) = self.sync_repos().await {
                        warn!(error = %e, "registry sync failed");
                    }
                }
            }
        }

        self.stop().await?;
        Ok(())
    }
}

async fn shutdown_mount_session(
    name: &str,
    session: RepoMountSession,
    _paths: Option<&ComputedPaths>,
) -> Result<()> {
    match session {
        #[cfg(feature = "fuse")]
        RepoMountSession::Fuse(session) => {
            let result = match _paths {
                Some(paths) => crate::mount::unmount_background_session(session, &paths.mount_path),
                None => session.umount_and_join().map_err(CrabError::Io),
            };
            if let Err(error) = &result {
                warn!(name, error = %error, "FUSE session unmount failed");
            } else {
                debug!(name, "FUSE session unmounted");
            }
            result
        }
        #[cfg(feature = "nfs")]
        RepoMountSession::Nfs { cancel, handle } => {
            cancel.cancel();
            match handle.await {
                Ok(Ok(())) => {
                    debug!(name, "NFS session unmounted");
                    Ok(())
                }
                Ok(Err(error)) => {
                    warn!(name, error = %error, "NFS session unmount failed");
                    Err(error)
                }
                Err(error) => {
                    let error = CrabError::Internal(format!(
                        "NFS session task failed during teardown: {error}"
                    ));
                    warn!(name, error = %error, "NFS session teardown failed");
                    Err(error)
                }
            }
        }
    }
}

/// Read the private control endpoint for a daemon-owned NFS mount.
pub fn read_nfs_control_endpoint(paths: &ComputedPaths) -> Result<Option<String>> {
    match std::fs::read_to_string(&paths.control_endpoint_path) {
        Ok(endpoint) => Ok(Some(endpoint.trim().to_owned())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(CrabError::Io(error)),
    }
}

#[cfg(feature = "nfs")]
fn persist_nfs_control_endpoint(paths: &ComputedPaths, endpoint: Option<&str>) -> Result<()> {
    let Some(endpoint) = endpoint else {
        return Err(CrabError::Configuration {
            key: "daemon NFS control endpoint unavailable".into(),
            origin: "crab daemon".into(),
        });
    };
    let temporary = paths.control_endpoint_path.with_extension("tmp");
    let mut options = std::fs::OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary).map_err(CrabError::Io)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(CrabError::Io)?;
    }
    file.write_all(endpoint.as_bytes()).map_err(CrabError::Io)?;
    file.sync_all().map_err(CrabError::Io)?;
    #[cfg(windows)]
    match std::fs::remove_file(&paths.control_endpoint_path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(CrabError::Io(error)),
    }
    std::fs::rename(&temporary, &paths.control_endpoint_path).map_err(CrabError::Io)
}

fn remove_nfs_control_endpoint(paths: &ComputedPaths) {
    match std::fs::remove_file(&paths.control_endpoint_path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => warn!(
            path = %paths.control_endpoint_path.display(),
            error = %error,
            "failed to remove daemon NFS control endpoint"
        ),
    }
}

#[cfg(feature = "nfs")]
async fn wait_for_nfs_control(
    endpoint: Option<&str>,
    session: &JoinHandle<Result<()>>,
) -> Result<()> {
    let endpoint = endpoint.ok_or_else(|| CrabError::Configuration {
        key: "daemon NFS control endpoint unavailable".into(),
        origin: "crab daemon".into(),
    })?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        if crate::nfs_control::ping(endpoint).await.is_ok() {
            return Ok(());
        }
        if session.is_finished() {
            return Err(CrabError::Internal(
                "daemon NFS session exited before its control endpoint became ready".into(),
            ));
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(CrabError::Internal(
                "timed out waiting for daemon NFS control endpoint".into(),
            ));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ---------------------------------------------------------------------------
// Mount table detection
// ---------------------------------------------------------------------------

/// Check whether a mountpoint is currently active in the OS mount table.
///
/// macOS: parses `/sbin/mount` output for the path.
/// Linux: parses `/proc/mounts` for the path.
pub fn is_mounted(path: &Path) -> bool {
    let path_str = path.to_string_lossy();

    #[cfg(target_os = "macos")]
    {
        let output = std::process::Command::new("/sbin/mount").output().ok();
        output.is_some_and(|o| String::from_utf8_lossy(&o.stdout).contains(path_str.as_ref()))
    }

    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/mounts").map_or(false, |s| s.contains(path_str.as_ref()))
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = path_str;
        false
    }
}

// ---------------------------------------------------------------------------
// Out-of-process status reconstruction
// ---------------------------------------------------------------------------

/// Reconstruct `RepoStatus` from persisted stores when the daemon is not
/// sharing memory with the CLI process.
///
/// Reads: snapshot SQLite (HEAD OID, generation), overlay SQLite (dirty count),
/// OS mount table (mount state), `FETCH_HEAD` mtime (last fetch time).
pub fn read_persisted_status(config: &RepoConfig, daemon_root: &Path) -> RepoStatus {
    let paths = config.computed_paths(daemon_root);

    let mut status = RepoStatus {
        name: config.name.clone(),
        remote: config.remote.clone(),
        remote_redacted: config.remote_redacted.clone(),
        branch: config.branch.clone(),
        mount_root: config.mount_root.clone(),
        refresh_interval_secs: config.refresh_interval_secs,
        state: RepoRuntimeState::Stopped,
        head_oid: None,
        generation: None,
        dirty_count: 0,
        dirty_paths: Vec::new(),
        last_fetch_at: None,
        enabled: config.enabled,
        read_only: config.read_only,
        backend: config.backend,
        is_live: false,
    };

    // Check OS mount table for active mount.
    if is_mounted(&paths.mount_path) {
        status.state = RepoRuntimeState::Running;
    }

    // Read HEAD OID and generation from snapshot SQLite.
    if let Ok(snap) = SnapshotStore::open_existing(&paths.meta_db_path) {
        status.head_oid = snap.head_oid().ok().flatten();
        status.generation = snap.current_generation().ok().flatten();
    }

    // Read overlay dirty count from SQLite without creating overlay state.
    if !config.read_only
        && paths.overlay_db_path.exists()
        && let Ok(diff) = crate::publish::inspect_overlay(
            &crate::publish::OverlayPaths::from_cache_dir(&paths.repo_dir),
        )
    {
        status.dirty_count = i64::try_from(diff.changes.len()).unwrap_or(i64::MAX);
        status.dirty_paths = diff.changes.into_iter().map(|change| change.path).collect();
    }

    // Best-effort last fetch time from FETCH_HEAD mtime.
    let fetch_head = paths.git_dir.join("FETCH_HEAD");
    if let Ok(meta) = std::fs::metadata(&fetch_head) {
        status.last_fetch_at = meta.modified().ok();
    }

    status
}

fn publishable_overlay_state(overlay: &OverlayStore) -> (i64, Vec<String>) {
    crate::publish::inspect_overlay_store(overlay).map_or_else(
        |_| (0, Vec::new()),
        |diff| {
            (
                i64::try_from(diff.changes.len()).unwrap_or(i64::MAX),
                diff.changes.into_iter().map(|change| change.path).collect(),
            )
        },
    )
}

/// Read persisted repo state and verify a live daemon-owned NFS control plane.
pub async fn read_status(config: &RepoConfig, daemon_root: &Path) -> RepoStatus {
    let mut status = read_persisted_status(config, daemon_root);
    #[cfg(feature = "nfs")]
    if config.backend == DaemonMountBackend::Nfs {
        let paths = config.computed_paths(daemon_root);
        if let Ok(Some(endpoint)) = read_nfs_control_endpoint(&paths)
            && let Ok(live) = crate::nfs_control::status(&endpoint).await
        {
            status.state = RepoRuntimeState::Running;
            status.is_live = true;
            status.head_oid = live.head_oid;
        }
    }
    status
}

// ---------------------------------------------------------------------------
// Helper: resolve HEAD from a git directory
// ---------------------------------------------------------------------------

/// Read `.git/HEAD` and resolve it to `(oid_hex, ref_name)`.
///
/// Checks the loose ref file first, then falls back to `packed-refs`
/// (git packs refs after clone and gc, so the loose file may not exist).
#[allow(dead_code)]
fn resolve_head(git_dir: &Path) -> Result<(String, String)> {
    let head_path = git_dir.join("HEAD");
    let head_content = std::fs::read_to_string(&head_path)
        .map_err(|e| CrabError::Internal(format!("failed to read {}: {e}", head_path.display())))?;
    let head_content = head_content.trim();

    if let Some(ref_name) = head_content.strip_prefix("ref: ") {
        let ref_name = ref_name.trim();

        // Try loose ref file first.
        let ref_path = git_dir.join(ref_name);
        if let Ok(content) = std::fs::read_to_string(&ref_path) {
            let oid = content.trim().to_owned();
            return Ok((oid, ref_name.to_owned()));
        }

        // Fall back to packed-refs.
        if let Some(oid) = lookup_packed_ref(git_dir, ref_name) {
            return Ok((oid, ref_name.to_owned()));
        }

        Err(CrabError::Internal(format!(
            "ref {ref_name} not found as loose ref or in packed-refs"
        )))
    } else {
        // Detached HEAD — the content is the OID itself.
        Ok((head_content.to_owned(), "HEAD".to_owned()))
    }
}

/// Look up a ref in the `packed-refs` file.
///
/// Returns the OID hex string if found, `None` otherwise.
#[allow(dead_code)]
fn lookup_packed_ref(git_dir: &Path, ref_name: &str) -> Option<String> {
    let packed_refs_path = git_dir.join("packed-refs");
    let content = std::fs::read_to_string(&packed_refs_path).ok()?;

    for line in content.lines() {
        // Skip comments and peeled entries (^...).
        if line.starts_with('#') || line.starts_with('^') {
            continue;
        }
        // Format: "<oid> <ref_name>"
        if let Some((oid, name)) = line.split_once(' ')
            && name.trim() == ref_name
        {
            return Some(oid.trim().to_owned());
        }
    }
    None
}

fn tracked_branch_ref(branch: &str) -> String {
    if branch.starts_with("refs/") {
        branch.to_owned()
    } else {
        format!("refs/heads/{branch}")
    }
}

/// Read the commit timestamp from HEAD for use as mtime on base files.
///
/// On `--features gix-facade`, resolves via
/// `gix::Repository::find_commit(head_id)` + `committer().time.seconds`.
/// Default builds shell out to `git log -1 --format=%ct`.
#[allow(dead_code)]
fn commit_time_from_head(git_dir: &Path) -> Option<i64> {
    #[cfg(feature = "gix-facade")]
    {
        let repo = crab_git::facade::open_at(git_dir).ok()?;
        crab_git::facade::head_commit_time(&repo).ok().flatten()
    }

    #[cfg(not(feature = "gix-facade"))]
    {
        let output = std::process::Command::new("git")
            .args(["log", "-1", "--format=%ct"])
            .env("GIT_DIR", git_dir)
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .output()
            .ok()?;

        if !output.status.success() {
            return None;
        }

        let ts_str = String::from_utf8_lossy(&output.stdout);
        ts_str.trim().parse::<i64>().ok()
    }
}

/// Create a hydration service with stub resolvers for the mount pipeline.
///
/// When a `StoreLayout` is provided, the xorb fetcher routes through the
/// global `.crab/xorbs/` prefix via the object store. File-index and
/// shard resolvers remain stubs until the full transport layer is wired.
#[allow(dead_code)]
fn create_stub_hydration(
    cache: Arc<ChunkCache>,
    verified: Arc<VerifiedSet>,
    cancel: CancellationToken,
    router: Option<StoreLayout>,
) -> Arc<HydrationService> {
    let xorb_fetcher: Arc<dyn crate::data_plane::XorbFetcher> = match router {
        Some(layout) => {
            let rt = tokio::runtime::Handle::current();
            Arc::new(crate::hydration::StoreBackedXorbFetcher::new(layout, rt))
        }
        None => Arc::new(StubXorbFetcher),
    };

    HydrationService::new(
        cache,
        verified,
        Arc::new(StubFileIndexResolver),
        Arc::new(StubShardLoader),
        xorb_fetcher,
        None,
        None,
        Some(2), // reduced concurrency for stub
        cancel,
    )
}

#[allow(dead_code)]
struct StubFileIndexResolver;
impl crate::data_plane::FileIndexResolver for StubFileIndexResolver {
    fn resolve_file_index(
        &self,
        _file_hash: &[u8; 32],
        _shard_hint: Option<&[u8; 32]>,
    ) -> Result<Option<[u8; 32]>> {
        Ok(None)
    }
    fn scan_shard_list_for_file(&self, _file_hash: &[u8; 32]) -> Result<Option<[u8; 32]>> {
        Ok(None)
    }
}

#[allow(dead_code)]
struct StubShardLoader;
impl crate::data_plane::ShardLoader for StubShardLoader {
    fn load_reconstruction_terms(
        &self,
        _shard_hash: &[u8; 32],
        _file_hash: &[u8; 32],
    ) -> Result<Vec<crate::data_plane::ReconstructionTerm>> {
        Ok(Vec::new())
    }
}

#[allow(dead_code)]
struct StubXorbFetcher;
impl crate::data_plane::XorbFetcher for StubXorbFetcher {
    fn fetch_range(&self, _xorb_hash: &[u8; 32], _range: std::ops::Range<u64>) -> Result<Vec<u8>> {
        Err(CrabError::Internal("stub xorb fetcher".into()))
    }
}

// ---------------------------------------------------------------------------
// RepoStatus — public status report
// ---------------------------------------------------------------------------

/// Status report for a daemon-managed repo.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct RepoStatus {
    pub name: String,
    pub remote: String,
    pub remote_redacted: String,
    pub branch: String,
    pub mount_root: String,
    pub refresh_interval_secs: u64,
    pub state: RepoRuntimeState,
    pub head_oid: Option<String>,
    pub generation: Option<i64>,
    pub dirty_count: i64,
    pub dirty_paths: Vec<String>,
    #[serde(serialize_with = "serialize_system_time_opt")]
    #[schemars(with = "Option<String>")]
    pub last_fetch_at: Option<SystemTime>,
    pub enabled: bool,
    pub read_only: bool,
    pub backend: DaemonMountBackend,
    /// Whether this status was read from live daemon memory (`true`) or
    /// reconstructed from persisted stores (`false`).
    pub is_live: bool,
}

/// Serialize `Option<SystemTime>` as an RFC 3339 UTC string with
/// millisecond precision, or `null` when absent.
#[expect(
    clippy::ref_option,
    reason = "serde serialize_with passes the field by reference"
)]
fn serialize_system_time_opt<S: serde::Serializer>(
    time: &Option<SystemTime>,
    serializer: S,
) -> std::result::Result<S::Ok, S::Error> {
    match time {
        Some(t) => {
            let ms = t.duration_since(UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
            let s = crab_types::time::from_epoch_millis(ms);
            serializer.serialize_some(&s)
        }
        None => serializer.serialize_none(),
    }
}

impl std::fmt::Display for RepoStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mode = if self.read_only { "ro" } else { "rw" };
        let enabled = if self.enabled { "on" } else { "off" };
        let live = if self.is_live { "live" } else { "persisted" };
        write!(
            f,
            "{:<20} {:<12} {:<40} {} [{}] [{mode}] [{enabled}] ({live})",
            self.name,
            self.state,
            self.head_oid.as_deref().unwrap_or("(none)"),
            self.branch,
            self.backend,
        )
    }
}

// ---------------------------------------------------------------------------
// Error helpers
// ---------------------------------------------------------------------------

fn map_sqlite_err(e: rusqlite::Error) -> CrabError {
    CrabError::Internal(format!("daemon registry sqlite: {e}"))
}

fn lock_poisoned() -> CrabError {
    CrabError::Internal("daemon registry mutex poisoned".into())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;

    fn temp_registry() -> (tempfile::TempDir, Registry) {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("config/repos.sqlite");
        let registry = Registry::open(&db_path).unwrap();
        (dir, registry)
    }

    fn sample_config(name: &str) -> RepoConfig {
        RepoConfig {
            name: name.to_owned(),
            remote: format!("https://example.com/{name}.git"),
            remote_redacted: format!("https://example.com/{name}.git"),
            branch: "main".to_owned(),
            mount_root: "/tmp/mounts".to_owned(),
            refresh_interval_secs: 30,
            enabled: true,
            read_only: false,
            backend: DaemonMountBackend::Fuse,
        }
    }

    #[test]
    fn tracked_branch_ref_accepts_branch_names_and_full_refs() {
        assert_eq!(tracked_branch_ref("main"), "refs/heads/main");
        assert_eq!(tracked_branch_ref("refs/heads/main"), "refs/heads/main");
    }

    #[test]
    fn registry_add_and_get() {
        let (_dir, registry) = temp_registry();
        let config = sample_config("model-repo");

        registry.add_repo(&config).unwrap();

        let got = registry.get_repo("model-repo").unwrap().unwrap();
        assert_eq!(got.name, "model-repo");
        assert_eq!(got.branch, "main");
        assert_eq!(got.refresh_interval_secs, 30);
        assert_eq!(got.backend, DaemonMountBackend::Fuse);
    }

    #[test]
    fn registry_round_trips_nfs_backend() {
        let (_dir, registry) = temp_registry();
        let mut config = sample_config("nfs-repo");
        config.backend = DaemonMountBackend::Nfs;

        registry.add_repo(&config).unwrap();

        let got = registry.get_repo("nfs-repo").unwrap().unwrap();
        assert_eq!(got.backend, DaemonMountBackend::Nfs);
    }

    #[cfg(feature = "nfs")]
    #[test]
    fn daemon_nfs_control_endpoint_is_private_and_removable() {
        let dir = tempfile::tempdir().unwrap();
        let paths = sample_config("nfs-repo").computed_paths(dir.path());
        std::fs::create_dir_all(&paths.repo_dir).unwrap();

        persist_nfs_control_endpoint(&paths, Some("tcp:127.0.0.1:49152?token=private-token"))
            .unwrap();

        assert_eq!(
            read_nfs_control_endpoint(&paths).unwrap().as_deref(),
            Some("tcp:127.0.0.1:49152?token=private-token")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&paths.control_endpoint_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }

        remove_nfs_control_endpoint(&paths);
        assert_eq!(read_nfs_control_endpoint(&paths).unwrap(), None);
    }

    #[test]
    fn registry_add_is_upsert() {
        let (_dir, registry) = temp_registry();
        let mut config = sample_config("repo-a");
        registry.add_repo(&config).unwrap();

        config.branch = "develop".to_owned();
        registry.add_repo(&config).unwrap();

        let got = registry.get_repo("repo-a").unwrap().unwrap();
        assert_eq!(got.branch, "develop");

        // Should still be one entry.
        let all = registry.list_repos().unwrap();
        assert_eq!(all.len(), 1);
    }

    #[test]
    fn registry_remove() {
        let (_dir, registry) = temp_registry();
        registry.add_repo(&sample_config("repo-a")).unwrap();
        registry.add_repo(&sample_config("repo-b")).unwrap();

        let removed = registry.remove_repo("repo-a").unwrap();
        assert!(removed);

        assert!(registry.get_repo("repo-a").unwrap().is_none());
        assert!(registry.get_repo("repo-b").unwrap().is_some());
    }

    #[test]
    fn registry_remove_nonexistent() {
        let (_dir, registry) = temp_registry();
        let removed = registry.remove_repo("ghost").unwrap();
        assert!(!removed);
    }

    #[test]
    fn registry_list_repos_sorted() {
        let (_dir, registry) = temp_registry();
        registry.add_repo(&sample_config("charlie")).unwrap();
        registry.add_repo(&sample_config("alpha")).unwrap();
        registry.add_repo(&sample_config("bravo")).unwrap();

        let repos = registry.list_repos().unwrap();
        let names: Vec<&str> = repos.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "bravo", "charlie"]);
    }

    #[test]
    fn registry_set_refresh_interval() {
        let (_dir, registry) = temp_registry();
        registry.add_repo(&sample_config("repo-a")).unwrap();

        let updated = registry.set_refresh_interval("repo-a", 60).unwrap();
        assert!(updated);

        let got = registry.get_repo("repo-a").unwrap().unwrap();
        assert_eq!(got.refresh_interval_secs, 60);
    }

    #[test]
    fn registry_set_refresh_nonexistent() {
        let (_dir, registry) = temp_registry();
        let updated = registry.set_refresh_interval("ghost", 60).unwrap();
        assert!(!updated);
    }

    #[test]
    fn registry_get_nonexistent() {
        let (_dir, registry) = temp_registry();
        assert!(registry.get_repo("nope").unwrap().is_none());
    }

    #[tokio::test]
    async fn daemon_service_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        let daemon = DaemonService::new(dir.path().to_path_buf(), cancel.clone()).unwrap();

        // Start with no repos — should succeed.
        daemon.start().await.unwrap();

        // Add a repo.
        daemon.add_repo(&sample_config("test-repo")).await.unwrap();

        // List should show the repo.
        let statuses = daemon.list_repos().await.unwrap();
        assert_eq!(statuses.len(), 1);
        assert_eq!(statuses[0].name, "test-repo");

        // Status for the repo.
        let status = daemon.repo_status("test-repo").await.unwrap();
        assert_eq!(status.name, "test-repo");

        // Remove the repo.
        daemon.remove_repo("test-repo").await.unwrap();
        let statuses = daemon.list_repos().await.unwrap();
        assert!(statuses.is_empty());

        // Stop.
        daemon.stop().await.unwrap();
    }

    #[tokio::test]
    async fn daemon_remove_nonexistent_repo() {
        let dir = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        let daemon = DaemonService::new(dir.path().to_path_buf(), cancel).unwrap();

        let err = daemon.remove_repo("ghost").await.unwrap_err();
        assert!(matches!(err, CrabError::NotFound { .. }));
    }

    #[tokio::test]
    async fn daemon_set_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        let daemon = DaemonService::new(dir.path().to_path_buf(), cancel).unwrap();

        daemon.add_repo(&sample_config("repo-a")).await.unwrap();
        daemon.set_refresh_interval("repo-a", 120).unwrap();

        let status = daemon.repo_status("repo-a").await.unwrap();
        assert_eq!(status.refresh_interval_secs, 120);
    }

    #[tokio::test]
    async fn force_fetch_uses_daemon_repo_git_dir() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let remote = dir.path().join("remote.git");
        let daemon_root = dir.path().join("daemon");
        let mount_root = dir.path().join("mounts");

        std::fs::create_dir_all(&source).unwrap();
        git(&source, ["init", "-b", "main"]);
        git(&source, ["config", "user.email", "daemon-test@crab.local"]);
        git(&source, ["config", "user.name", "daemon test"]);
        std::fs::write(source.join("file.txt"), "base").unwrap();
        git(&source, ["add", "file.txt"]);
        git(&source, ["commit", "-m", "base"]);
        git_in(
            dir.path(),
            ["clone", "--bare", source.to_str().unwrap(), "remote.git"],
        );

        let cancel = CancellationToken::new();
        let daemon = DaemonService::new(daemon_root.clone(), cancel).unwrap();
        let mut config = sample_config("repo-a");
        config.remote = remote.to_string_lossy().to_string();
        config.remote_redacted = config.remote.clone();
        config.mount_root = mount_root.to_string_lossy().to_string();
        daemon.registry().add_repo(&config).unwrap();

        let paths = config.computed_paths(&daemon_root);
        std::fs::create_dir_all(&paths.repo_dir).unwrap();
        git_in(
            dir.path(),
            [
                "clone",
                "--bare",
                remote.to_str().unwrap(),
                paths.git_dir.to_str().unwrap(),
            ],
        );

        std::fs::write(source.join("file.txt"), "updated").unwrap();
        git(&source, ["add", "file.txt"]);
        git(&source, ["commit", "-m", "update"]);
        git(&source, ["push", remote.to_str().unwrap(), "main"]);

        daemon.force_fetch("repo-a").await.unwrap();

        assert_eq!(
            git_stdout(&paths.git_dir, ["rev-parse", "refs/heads/main"]),
            git_stdout(&source, ["rev-parse", "HEAD"])
        );
        assert!(paths.git_dir.join("FETCH_HEAD").exists());
        assert!(!mount_root.join("repo-a/.git").exists());
    }

    // --- Registry schema migration tests ---

    #[test]
    fn registry_migration_adds_new_columns() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("config/repos.sqlite");

        // Create a legacy registry without the new columns.
        {
            std::fs::create_dir_all(db_path.parent().unwrap()).unwrap();
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE repos (
                    name TEXT PRIMARY KEY,
                    remote TEXT NOT NULL,
                    branch TEXT NOT NULL,
                    mount_root TEXT NOT NULL,
                    refresh_interval_secs INTEGER NOT NULL DEFAULT 30
                )",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO repos(name, remote, branch, mount_root) VALUES('old-repo', 'https://example.com/old.git', 'main', '/mnt')",
                [],
            )
            .unwrap();
        }

        // Open with migration.
        let registry = Registry::open(&db_path).unwrap();
        let repo = registry.get_repo("old-repo").unwrap().unwrap();
        assert!(repo.enabled);
        assert!(!repo.read_only);
        assert_eq!(repo.remote_redacted, "");
        assert_eq!(repo.backend, DaemonMountBackend::Fuse);
    }

    #[test]
    fn registry_enable_disable() {
        let (_dir, registry) = temp_registry();
        registry.add_repo(&sample_config("repo-a")).unwrap();

        // Starts enabled.
        let repo = registry.get_repo("repo-a").unwrap().unwrap();
        assert!(repo.enabled);

        // Disable.
        let ok = registry.disable_repo("repo-a").unwrap();
        assert!(ok);
        let repo = registry.get_repo("repo-a").unwrap().unwrap();
        assert!(!repo.enabled);

        // Enable.
        let ok = registry.enable_repo("repo-a").unwrap();
        assert!(ok);
        let repo = registry.get_repo("repo-a").unwrap().unwrap();
        assert!(repo.enabled);
    }

    #[test]
    fn registry_enable_nonexistent() {
        let (_dir, registry) = temp_registry();
        let ok = registry.enable_repo("ghost").unwrap();
        assert!(!ok);
    }

    #[test]
    fn registry_read_only_flag() {
        let (_dir, registry) = temp_registry();
        let mut config = sample_config("repo-a");
        config.read_only = true;
        registry.add_repo(&config).unwrap();

        let repo = registry.get_repo("repo-a").unwrap().unwrap();
        assert!(repo.read_only);

        registry.set_read_only("repo-a", false).unwrap();
        let repo = registry.get_repo("repo-a").unwrap().unwrap();
        assert!(!repo.read_only);
    }

    #[test]
    fn registry_stores_redacted_url() {
        let (_dir, registry) = temp_registry();
        let mut config = sample_config("repo-a");
        config.remote = "https://token@github.com/org/repo.git".to_owned();
        registry.add_repo(&config).unwrap();

        let repo = registry.get_repo("repo-a").unwrap().unwrap();
        assert!(!repo.remote_redacted.contains("token"));
        assert!(repo.remote_redacted.contains("***"));
        assert!(repo.remote_redacted.contains("github.com"));
    }

    // --- MountFailure tests ---

    #[test]
    fn mount_failure_backoff_doubles() {
        let mut failure = MountFailure::new();
        assert_eq!(failure.backoff, MountFailure::INITIAL_BACKOFF);

        failure.record_failure();
        assert_eq!(failure.backoff, Duration::from_secs(60));

        failure.record_failure();
        assert_eq!(failure.backoff, Duration::from_secs(120));
    }

    #[test]
    fn mount_failure_backoff_caps_at_max() {
        let mut failure = MountFailure::new();
        for _ in 0..20 {
            failure.record_failure();
        }
        assert_eq!(failure.backoff, MountFailure::MAX_BACKOFF);
    }

    #[test]
    fn mount_failure_can_retry_after_elapsed() {
        let mut failure = MountFailure::new();
        // Set backoff to zero for testing.
        failure.backoff = Duration::from_secs(0);
        failure.last_attempt = Instant::now() - Duration::from_secs(1);
        assert!(failure.can_retry());
    }

    #[test]
    fn mount_failure_cannot_retry_before_elapsed() {
        let failure = MountFailure::new();
        // Just created — backoff is 30s, last_attempt is now.
        assert!(!failure.can_retry());
    }

    // --- Status display tests ---

    #[tokio::test]
    async fn daemon_status_includes_backend_enabled_and_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        let daemon = DaemonService::new(dir.path().to_path_buf(), cancel).unwrap();

        let mut config = sample_config("test-repo");
        config.read_only = true;
        config.backend = DaemonMountBackend::Nfs;
        daemon.add_repo(&config).await.unwrap();

        let status = daemon.repo_status("test-repo").await.unwrap();
        assert!(status.read_only);
        assert!(status.enabled);
        assert_eq!(status.backend, DaemonMountBackend::Nfs);
    }

    #[tokio::test]
    async fn sync_repos_skips_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        let daemon = DaemonService::new(dir.path().to_path_buf(), cancel).unwrap();

        let mut config = sample_config("disabled-repo");
        config.enabled = false;
        daemon.registry().add_repo(&config).unwrap();

        daemon.sync_repos().await.unwrap();

        // Disabled repo should not be in the running set.
        let running = daemon.running.read().await;
        assert!(!running.contains_key("disabled-repo"));
    }

    // --- ComputedPaths tests ---

    #[test]
    fn computed_paths_derives_all_paths() {
        let config = sample_config("my-repo");
        let daemon_root = Path::new("/tmp/daemon");
        let paths = config.computed_paths(daemon_root);

        assert_eq!(paths.repo_dir, PathBuf::from("/tmp/daemon/repos/my-repo"));
        assert_eq!(
            paths.git_dir,
            PathBuf::from("/tmp/daemon/repos/my-repo/.git")
        );
        assert_eq!(
            paths.overlay_dir,
            PathBuf::from("/tmp/daemon/repos/my-repo/overlay/upper")
        );
        assert_eq!(
            paths.blob_cache_dir,
            PathBuf::from("/tmp/daemon/cache/blobs/my-repo")
        );
        assert_eq!(
            paths.meta_db_path,
            PathBuf::from("/tmp/daemon/repos/my-repo/snapshot.sqlite")
        );
        assert_eq!(
            paths.overlay_db_path,
            PathBuf::from("/tmp/daemon/repos/my-repo/overlay.db")
        );
        assert_eq!(paths.mount_path, PathBuf::from("/tmp/mounts/my-repo"));
    }

    // --- is_mounted tests ---

    #[test]
    fn is_mounted_returns_false_for_nonexistent_path() {
        assert!(!is_mounted(Path::new("/nonexistent/mount/point/xyz123")));
    }

    // --- read_persisted_status tests ---

    #[test]
    fn read_persisted_status_returns_stopped_for_empty_state() {
        let dir = tempfile::tempdir().unwrap();
        let config = sample_config("test-repo");
        let status = read_persisted_status(&config, dir.path());

        assert_eq!(status.name, "test-repo");
        assert_eq!(status.state, RepoRuntimeState::Stopped);
        assert!(status.head_oid.is_none());
        assert!(status.generation.is_none());
        assert_eq!(status.dirty_count, 0);
        assert!(!status.is_live);
    }

    #[test]
    fn read_persisted_status_reads_snapshot_state() {
        let dir = tempfile::tempdir().unwrap();
        let config = sample_config("test-repo");
        let paths = config.computed_paths(dir.path());

        // Create a snapshot store with a generation.
        std::fs::create_dir_all(&paths.repo_dir).unwrap();
        let snap = SnapshotStore::open_or_create(&paths.meta_db_path).unwrap();
        snap.publish_generation("aabbccdd", "refs/heads/main", &[])
            .unwrap();
        drop(snap);

        let status = read_persisted_status(&config, dir.path());
        assert_eq!(status.head_oid.as_deref(), Some("aabbccdd"));
        assert_eq!(status.generation, Some(1));
    }

    fn git<const N: usize>(repo: &Path, args: [&str; N]) {
        let _git_env = crate::test_support::GIT_DIR_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_in<I, S>(cwd: &Path, args: I)
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        let _git_env = crate::test_support::GIT_DIR_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn git_stdout<const N: usize>(repo: &Path, args: [&str; N]) -> String {
        let _git_env = crate::test_support::GIT_DIR_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }

    #[test]
    fn read_persisted_status_reads_overlay_dirty_count() {
        let dir = tempfile::tempdir().unwrap();
        let config = sample_config("test-repo");
        let paths = config.computed_paths(dir.path());

        // Create an overlay store with a dirty entry.
        std::fs::create_dir_all(&paths.repo_dir).unwrap();
        let ov = OverlayStore::open(&paths.overlay_db_path, &paths.overlay_dir).unwrap();
        use crate::engine::OverlayWriter;
        ov.create_file("dirty.txt", 0o100644).unwrap();
        ov.create_file("._dirty.txt", 0o100644).unwrap();
        drop(ov);

        let status = read_persisted_status(&config, dir.path());
        assert_eq!(status.dirty_count, 1);
        assert_eq!(status.dirty_paths, ["dirty.txt"]);
    }

    #[test]
    fn read_persisted_status_counts_delete_only_overlay() {
        let dir = tempfile::tempdir().unwrap();
        let config = sample_config("test-repo");
        let paths = config.computed_paths(dir.path());

        std::fs::create_dir_all(&paths.repo_dir).unwrap();
        let ov = OverlayStore::open(&paths.overlay_db_path, &paths.overlay_dir).unwrap();
        use crate::engine::OverlayWriter;
        ov.remove("deleted.txt").unwrap();
        drop(ov);

        let status = read_persisted_status(&config, dir.path());
        assert_eq!(status.dirty_count, 1);
        assert_eq!(status.dirty_paths, ["deleted.txt"]);
    }

    // --- resolve_head tests ---

    #[test]
    fn resolve_head_reads_symbolic_ref() {
        let dir = tempfile::tempdir().unwrap();
        let git_dir = dir.path().join(".git");
        std::fs::create_dir_all(git_dir.join("refs/heads")).unwrap();
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(git_dir.join("refs/heads/main"), "aabbccdd\n").unwrap();

        let (oid, ref_name) = resolve_head(&git_dir).unwrap();
        assert_eq!(oid, "aabbccdd");
        assert_eq!(ref_name, "refs/heads/main");
    }

    #[test]
    fn resolve_head_reads_detached_head() {
        let dir = tempfile::tempdir().unwrap();
        let git_dir = dir.path().join(".git");
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::write(git_dir.join("HEAD"), "deadbeef1234\n").unwrap();

        let (oid, ref_name) = resolve_head(&git_dir).unwrap();
        assert_eq!(oid, "deadbeef1234");
        assert_eq!(ref_name, "HEAD");
    }
}
