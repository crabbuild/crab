//! Mount pipeline builder: encapsulates the 9-step mount pipeline.
//!
//! The pipeline takes a repository (local or remote) from zero to a
//! ready-to-mount state. `execute()` runs these steps:
//!
//! 1. Blobless clone (if not cached) → `git clone --bare --filter=blob:none`
//! 2. Resolve HEAD → read ref, get OID
//! 3. Build snapshot → walk tree, detect pointers, publish generation
//! 4. Open overlay (unless read-only) → SQLite + upper dir
//! 5. Reconcile overlay → discard stale entries against current HEAD
//! 6. Run `git read-tree HEAD` → populate index for subsequent operations
//! 7. Start hydration → create workers, wire chunk cache
//! 8. Create resolver → merge snapshot + overlay
//! 9. Create engine → wire resolver + overlay + hydration + ODB reader
//!
//! FUSE mount and the refresh loop are handled outside `execute()` by
//! the caller (coordinator, daemon, or CLI foreground mode).
//!
//! The builder pattern allows callers to configure the pipeline
//! incrementally and then execute it in one shot.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info};

use crate::ChunkCache;
use crate::StoreLayout;
use crate::core::error::{CrabError, Result};
use crate::engine::{OdbReader, VfsEngine};
use crate::hydration::HydrationService;
use crate::integration::MountReadContext;
use crate::overlay::OverlayStore;
use crate::refresh::{
    GitRemoteRefFetcher, NoopRemoteRefFetcher, RefreshConfig, RefreshService, redact_url,
    run_read_tree_head,
};
use crate::resolver::{FuseResolver, OverlayLookup};
use crate::snapshot::{NodeType, SnapshotStore};
use crate::source::MountSource;
use crate::verified_set::VerifiedSet;

// ---------------------------------------------------------------------------
// PipelineConfig
// ---------------------------------------------------------------------------

/// Configuration for a single mount pipeline execution.
///
/// Passed to [`MountPipelineBuilder`] to control how the pipeline
/// resolves, clones, and mounts the repository.
#[derive(Debug, Clone)]
pub struct PipelineConfig {
    /// Remote URL or local path identifying the source repository.
    pub source: String,
    /// Path to the `.git` directory (bare clone or local repo).
    /// For remote sources this is the cache directory where the
    /// blobless clone lives; for local sources it's the repo's `.git`.
    pub git_dir: PathBuf,
    /// Branch or ref to mount (e.g. `refs/heads/main`).
    /// If `None`, the pipeline resolves whatever HEAD points to.
    pub ref_name: Option<String>,
    /// Mount in read-only mode (no overlay).
    pub read_only: bool,
    /// Root directory for per-repo state (snapshot, overlay, blob cache).
    pub cache_dir: PathBuf,
    /// Cancellation token for graceful shutdown of this mount.
    pub cancel_token: CancellationToken,
}

// ---------------------------------------------------------------------------
// PipelineOutput
// ---------------------------------------------------------------------------

/// Successful output of the mount pipeline.
///
/// Contains the three primary components needed to operate the mount,
/// plus auxiliary handles for lifecycle management.
pub struct PipelineOutput {
    /// VFS resolver (snapshot + overlay merged view).
    pub resolver: Arc<FuseResolver>,
    /// VFS engine (handles read/write/hydration dispatch).
    pub engine: Arc<VfsEngine>,
    /// Hydration service (background workers fetching chunks).
    pub hydration: Arc<HydrationService>,
    /// Snapshot store (SQLite-backed generation tracking).
    pub snapshot: Arc<SnapshotStore>,
    /// Overlay store (SQLite + upper dir), `None` if read-only.
    pub overlay: Option<Arc<OverlayStore>>,
    /// HEAD OID resolved during pipeline execution.
    pub head_oid: String,
    /// Ref name resolved during pipeline execution.
    pub head_ref: String,
    /// Snapshot generation published during this pipeline run.
    pub generation: i64,
    /// Hydration worker join handles (abort to stop workers).
    pub hydrator_handles: Vec<JoinHandle<()>>,
}

// ---------------------------------------------------------------------------
// MountPipelineBuilder
// ---------------------------------------------------------------------------

/// Builder that encapsulates the 11-step mount pipeline.
///
/// Construct via [`MountPipelineBuilder::new`], optionally configure
/// shared resources, then call [`execute`] to run the full pipeline.
pub struct MountPipelineBuilder {
    config: PipelineConfig,
    /// Shared chunk cache. If not provided, a per-mount cache is created.
    chunk_cache: Option<Arc<ChunkCache>>,
    /// Store layout for xorb fetching. `None` uses stub resolvers.
    store_layout: Option<StoreLayout>,
    /// Configured read-side hydrator supplied by the CLI integration.
    read_hydrator: Option<Arc<crab_read::ShardHydrator>>,
    /// Refresh interval override. Defaults to 30s.
    refresh_interval: Duration,
    /// Whether to disable the refresh loop entirely.
    no_refresh: bool,
}

impl MountPipelineBuilder {
    /// Create a new pipeline builder with the given configuration.
    #[must_use]
    pub fn new(config: PipelineConfig) -> Self {
        Self {
            config,
            chunk_cache: None,
            store_layout: None,
            read_hydrator: None,
            refresh_interval: Duration::from_secs(30),
            no_refresh: false,
        }
    }

    /// Provide a shared chunk cache (used when multiple mounts share
    /// a coordinator process).
    #[must_use]
    pub fn with_chunk_cache(mut self, cache: Arc<ChunkCache>) -> Self {
        self.chunk_cache = Some(cache);
        self
    }

    /// Provide a store layout for xorb fetching from object storage.
    #[must_use]
    pub fn with_store_layout(mut self, layout: StoreLayout) -> Self {
        self.store_layout = Some(layout);
        self
    }

    /// Provide the object-store layout and configured read-side hydrator.
    #[must_use]
    pub fn with_read_context(mut self, context: MountReadContext) -> Self {
        self.store_layout = Some(context.store_layout);
        self.read_hydrator = Some(context.hydrator);
        self
    }

    /// Override the refresh interval (default 30s).
    #[must_use]
    pub fn with_refresh_interval(mut self, interval: Duration) -> Self {
        self.refresh_interval = interval;
        self
    }

    /// Disable the automatic refresh loop.
    #[must_use]
    pub fn with_no_refresh(mut self, no_refresh: bool) -> Self {
        self.no_refresh = no_refresh;
        self
    }

    /// Execute the full 11-step mount pipeline.
    ///
    /// Returns the resolver, engine, and hydration service on success.
    /// On failure, partial resources are cleaned up before returning
    /// the error.
    pub fn execute(self) -> Result<PipelineOutput> {
        let config = &self.config;
        let redacted = redact_url(&config.source);

        info!(
            source = %redacted,
            git_dir = %config.git_dir.display(),
            read_only = config.read_only,
            "executing mount pipeline"
        );

        // Step 1: Blobless clone (if git dir doesn't exist yet).
        if !config.git_dir.exists() {
            self.step_blobless_clone()?;
        }

        // Step 2: Resolve HEAD → (oid, ref).
        let (head_oid, head_ref) = self.step_resolve_head()?;

        // Step 3: Build snapshot from HEAD tree.
        let snapshot = self.step_build_snapshot(&head_oid, &head_ref)?;

        let generation = snapshot
            .current_generation()?
            .ok_or_else(|| CrabError::Internal("no generation after publish".into()))?;

        // Step 4: Open overlay (unless read-only).
        let overlay = self.step_open_overlay()?;

        // Step 5: Reconcile overlay against current HEAD.
        if let Some(ref ov) = overlay {
            Self::step_reconcile_overlay(ov, &snapshot, generation)?;
        }

        // Step 6: Run git read-tree HEAD.
        info!(step = "read_tree", "running git read-tree HEAD");
        run_read_tree_head(&config.git_dir);

        // Step 7: Start hydration service.
        let (hydration, hydrator_handles) = self.step_start_hydration()?;

        // Step 8: Create resolver (snapshot + overlay).
        let resolver = self.step_create_resolver(&snapshot, overlay.as_ref(), generation);

        // Step 9: Create engine (resolver + overlay + hydration + ODB reader).
        let engine = self.step_create_engine(&resolver, overlay.as_ref(), &hydration, &snapshot)?;

        info!(
            generation,
            head_oid = %head_oid,
            "mount pipeline complete"
        );

        Ok(PipelineOutput {
            resolver,
            engine,
            hydration,
            snapshot,
            overlay,
            head_oid,
            head_ref,
            generation,
            hydrator_handles,
        })
    }

    // -----------------------------------------------------------------------
    // Pipeline steps
    // -----------------------------------------------------------------------

    /// Step 1: Perform a blobless clone of the remote repository.
    fn step_blobless_clone(&self) -> Result<()> {
        let config = &self.config;
        let branch_arg = clone_branch_arg(config)?;

        info!(step = "clone", branch = %branch_arg, "cloning repository (blobless)");

        // Ensure parent directory exists.
        if let Some(parent) = config.git_dir.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let output = std::process::Command::new("git")
            .args([
                "clone",
                "--bare",
                "--filter=blob:none",
                "--single-branch",
                "--branch",
                &branch_arg,
                &config.source,
                &config.git_dir.to_string_lossy(),
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

        info!(step = "clone", "blobless clone complete");
        Ok(())
    }

    /// Step 2: Resolve HEAD from the git directory to (oid, ref_name).
    fn step_resolve_head(&self) -> Result<(String, String)> {
        info!(step = "resolve_head", "resolving HEAD");
        let (oid, ref_name) = resolve_head(&self.config.git_dir)?;
        debug!(step = "resolve_head", oid = %oid, ref_name = %ref_name);
        Ok((oid, ref_name))
    }

    /// Step 3: Open snapshot store and build initial generation from HEAD.
    fn step_build_snapshot(&self, head_oid: &str, head_ref: &str) -> Result<Arc<SnapshotStore>> {
        info!(step = "snapshot", "building snapshot from HEAD");

        let meta_db_path = self.config.cache_dir.join("snapshot.sqlite");
        let snapshot = Arc::new(SnapshotStore::open_or_create(&meta_db_path).map_err(|e| {
            error!(step = "snapshot", error = %e, "failed to open snapshot store");
            e
        })?);

        let node_count = snapshot
            .publish_generation_from_git(&self.config.git_dir, head_oid, head_ref)
            .map_err(|e| {
                error!(step = "snapshot", error = %e, "failed to build and publish snapshot");
                e
            })?;

        debug!(step = "snapshot", node_count, "snapshot published");

        Ok(snapshot)
    }

    /// Step 4: Open overlay store (SQLite + upper dir), or `None` if read-only.
    fn step_open_overlay(&self) -> Result<Option<Arc<OverlayStore>>> {
        if self.config.read_only {
            info!(step = "overlay", "read-only mode, skipping overlay");
            return Ok(None);
        }

        info!(step = "overlay", "opening overlay store");

        let overlay_db_path = self.config.cache_dir.join("overlay.db");
        let overlay_dir = self.config.cache_dir.join("overlay/upper");

        let ov = Arc::new(
            OverlayStore::open_with_orphan_cleanup(&overlay_db_path, &overlay_dir).map_err(
                |e| {
                    error!(step = "overlay", error = %e, "failed to open overlay store");
                    e
                },
            )?,
        );

        Ok(Some(ov))
    }

    /// Step 5: Reconcile overlay entries against the current HEAD snapshot.
    fn step_reconcile_overlay(
        overlay: &Arc<OverlayStore>,
        snapshot: &Arc<SnapshotStore>,
        generation: i64,
    ) -> Result<()> {
        info!(step = "reconcile", "reconciling overlay against HEAD");

        let snap_ref = Arc::clone(snapshot);
        overlay
            .reconcile(|path| {
                let node = snap_ref.get_node(generation, path).ok().flatten()?;
                Some(crate::overlay::ReconcileBaseInfo {
                    is_dir: node.node_type == NodeType::Dir,
                    object_oid: node.object_oid.clone(),
                })
            })
            .map_err(|e| {
                error!(step = "reconcile", error = %e, "overlay reconciliation failed");
                e
            })?;

        Ok(())
    }

    /// Step 7: Create and start the hydration service with workers.
    fn step_start_hydration(&self) -> Result<(Arc<HydrationService>, Vec<JoinHandle<()>>)> {
        info!(step = "hydration", "starting hydration service");

        let cache = if let Some(c) = &self.chunk_cache {
            Arc::clone(c)
        } else {
            let cache_dir = self.config.cache_dir.join("chunks");
            std::fs::create_dir_all(&cache_dir)?;
            Arc::new(ChunkCache::open(cache_dir, None)?)
        };

        let verified = Arc::new(VerifiedSet::default());

        let hydration = create_hydration(
            cache,
            verified,
            self.config.cancel_token.clone(),
            self.store_layout.clone(),
            self.read_hydrator.clone(),
            self.store_layout
                .as_ref()
                .map(|_| self.config.cache_dir.join("read_ranges")),
        )?;

        let handles = hydration.spawn_workers();
        Ok((hydration, handles))
    }

    /// Step 8: Create the VFS resolver (merges snapshot + overlay).
    fn step_create_resolver(
        &self,
        snapshot: &Arc<SnapshotStore>,
        overlay: Option<&Arc<OverlayStore>>,
        generation: i64,
    ) -> Arc<FuseResolver> {
        info!(step = "resolver", "creating VFS resolver");

        let commit_time = commit_time_from_head(&self.config.git_dir).unwrap_or(0);
        let overlay_lookup: Option<Arc<dyn OverlayLookup>> =
            overlay.map(|ov| Arc::clone(ov) as Arc<dyn OverlayLookup>);

        Arc::new(FuseResolver::new(
            Arc::clone(snapshot),
            overlay_lookup,
            generation,
            commit_time,
        ))
    }

    /// Step 9: Create the VFS engine (wires resolver + overlay + hydration + ODB).
    fn step_create_engine(
        &self,
        resolver: &Arc<FuseResolver>,
        overlay: Option<&Arc<OverlayStore>>,
        hydration: &Arc<HydrationService>,
        snapshot: &Arc<SnapshotStore>,
    ) -> Result<Arc<VfsEngine>> {
        info!(step = "engine", "creating VFS engine");

        let blob_cache_dir = self.config.cache_dir.join("blob_cache");
        let odb_reader = OdbReader::new(&self.config.git_dir, &blob_cache_dir).map_err(|e| {
            error!(step = "odb_reader", error = %e, "failed to create ODB reader");
            e
        })?;

        let overlay_writer: Option<Arc<dyn crate::engine::OverlayWriter>> =
            overlay.map(|ov| Arc::clone(ov) as Arc<dyn crate::engine::OverlayWriter>);

        Ok(Arc::new(VfsEngine::new(
            Arc::clone(resolver),
            overlay_writer,
            Arc::clone(hydration),
            Some(odb_reader),
            Some(Arc::clone(snapshot)),
        )))
    }
}

// ---------------------------------------------------------------------------
// Refresh loop builder (step 11)
// ---------------------------------------------------------------------------

/// Spawn the refresh loop for a mounted pipeline.
///
/// This is separated from the main pipeline because the FUSE mount
/// (step 10) must succeed before the refresh loop makes sense.
/// Callers invoke this after mounting.
pub fn spawn_refresh_loop(
    output: &PipelineOutput,
    config: &PipelineConfig,
    refresh_interval: Duration,
) -> Option<JoinHandle<()>> {
    if config.read_only {
        debug!("read-only mount, skipping refresh loop");
        return None;
    }

    let overlay = match &output.overlay {
        Some(ov) => Arc::clone(ov),
        None => return None,
    };

    let tracked_ref = config.ref_name.clone();

    let refresh_config = RefreshConfig {
        remote_poll_interval: refresh_interval,
        local_poll_interval: Duration::from_millis(500),
        git_dir: config.git_dir.clone(),
        tracked_ref,
    };

    let fetcher: Arc<dyn crate::refresh::RemoteRefFetcher> =
        match MountSource::parse(&config.source) {
            Ok(MountSource::Remote { .. }) => {
                Arc::new(GitRemoteRefFetcher::new(config.git_dir.clone()))
            }
            _ => Arc::new(NoopRemoteRefFetcher),
        };
    let refresh_svc = Arc::new(RefreshService::new(
        Arc::clone(&output.resolver),
        Arc::clone(&output.snapshot),
        overlay,
        fetcher,
        refresh_config,
        config.cancel_token.clone(),
    ));

    let handle = tokio::spawn(async move {
        refresh_svc.run().await;
    });

    info!("refresh loop started");
    Some(handle)
}

fn clone_branch_arg(config: &PipelineConfig) -> Result<String> {
    match config.ref_name.as_deref() {
        Some(ref_name) => Ok(branch_arg_from_ref(ref_name).to_owned()),
        None => resolve_default_clone_branch(&config.source),
    }
}

fn branch_arg_from_ref(ref_name: &str) -> &str {
    ref_name.strip_prefix("refs/heads/").unwrap_or(ref_name)
}

fn resolve_default_clone_branch(source: &str) -> Result<String> {
    let output = std::process::Command::new("git")
        .args(["ls-remote", "--symref", source, "HEAD", "refs/heads/*"])
        .output()
        .map_err(CrabError::Io)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let redacted_err = redact_url(stderr.trim());
        return Err(CrabError::Internal(format!(
            "failed to resolve remote default branch: {redacted_err}"
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let (advertised_default, heads) = parse_ls_remote_heads(&stdout);
    select_default_clone_branch(advertised_default.as_deref(), &heads)
}

fn parse_ls_remote_heads(output: &str) -> (Option<String>, Vec<String>) {
    let mut advertised_default = None;
    let mut heads = Vec::new();

    for line in output.lines() {
        if let Some(rest) = line.strip_prefix("ref: ") {
            let mut parts = rest.split_whitespace();
            if let (Some(target), Some(alias)) = (parts.next(), parts.next())
                && alias == "HEAD"
            {
                advertised_default = Some(branch_arg_from_ref(target).to_owned());
            }
            continue;
        }

        let mut parts = line.split_whitespace();
        let (Some(_oid), Some(ref_name)) = (parts.next(), parts.next()) else {
            continue;
        };
        if let Some(branch) = ref_name.strip_prefix("refs/heads/") {
            heads.push(branch.to_owned());
        }
    }

    heads.sort();
    heads.dedup();
    (advertised_default, heads)
}

fn select_default_clone_branch(default_branch: Option<&str>, heads: &[String]) -> Result<String> {
    if let Some(branch) = default_branch
        && !branch.is_empty()
    {
        return Ok(branch.to_owned());
    }

    if heads.iter().any(|branch| branch == "main") {
        return Ok("main".to_owned());
    }

    if heads.iter().any(|branch| branch == "master") {
        return Ok("master".to_owned());
    }

    if let [branch] = heads {
        return Ok(branch.clone());
    }

    Err(CrabError::Internal(
        "remote default branch is ambiguous; pass --ref to choose a branch".to_owned(),
    ))
}

// ---------------------------------------------------------------------------
// Helper: resolve HEAD from a git directory
// ---------------------------------------------------------------------------

/// Read `.git/HEAD` and resolve it to `(oid_hex, ref_name)`.
///
/// Checks the loose ref file first, then falls back to `packed-refs`
/// (git packs refs after clone and gc, so the loose file may not exist).
pub fn resolve_head(git_dir: &Path) -> Result<(String, String)> {
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
pub fn lookup_packed_ref(git_dir: &Path, ref_name: &str) -> Option<String> {
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

/// Read the commit timestamp from HEAD for use as mtime on base files.
pub fn commit_time_from_head(git_dir: &Path) -> Option<i64> {
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
            .output()
            .ok()?;

        if !output.status.success() {
            return None;
        }

        let ts_str = String::from_utf8_lossy(&output.stdout);
        ts_str.trim().parse::<i64>().ok()
    }
}

// ---------------------------------------------------------------------------
// Hydration service construction
// ---------------------------------------------------------------------------

/// Create a hydration service with the appropriate resolvers.
///
/// When a `StoreLayout` is provided, the xorb fetcher routes through
/// object storage. Otherwise, stub resolvers are used (suitable for
/// local mounts where small files come from the git ODB).
pub fn create_hydration(
    cache: Arc<ChunkCache>,
    verified: Arc<VerifiedSet>,
    cancel: CancellationToken,
    store_layout: Option<StoreLayout>,
    read_hydrator: Option<Arc<crab_read::ShardHydrator>>,
    read_range_cache_dir: Option<PathBuf>,
) -> Result<Arc<HydrationService>> {
    let xorb_fetcher: Arc<dyn crate::data_plane::XorbFetcher> = match store_layout {
        Some(layout) => {
            let rt = tokio::runtime::Handle::current();
            Arc::new(crate::hydration::StoreBackedXorbFetcher::new(layout, rt))
        }
        None => Arc::new(StubXorbFetcher),
    };

    Ok(HydrationService::new(
        cache,
        verified,
        Arc::new(StubFileIndexResolver),
        Arc::new(StubShardLoader),
        xorb_fetcher,
        read_hydrator,
        read_range_cache_dir,
        Some(2),
        cancel,
    ))
}

pub struct StubFileIndexResolver;
impl crate::data_plane::FileIndexResolver for StubFileIndexResolver {
    fn resolve_file_index(
        &self,
        _file_hash: &[u8; 32],
        _shard_hint: Option<&[u8; 32]>,
    ) -> Result<Option<[u8; 32]>> {
        Err(CrabError::Internal(
            "file index resolution not yet wired; set store_layout for full hydration support"
                .into(),
        ))
    }
    fn scan_shard_list_for_file(&self, _file_hash: &[u8; 32]) -> Result<Option<[u8; 32]>> {
        Err(CrabError::Internal(
            "shard list scan not yet wired; set store_layout for full hydration support".into(),
        ))
    }
}

pub struct StubShardLoader;
impl crate::data_plane::ShardLoader for StubShardLoader {
    fn load_reconstruction_terms(
        &self,
        _shard_hash: &[u8; 32],
        _file_hash: &[u8; 32],
    ) -> Result<Vec<crate::data_plane::ReconstructionTerm>> {
        Ok(Vec::new())
    }
}

pub struct StubXorbFetcher;
impl crate::data_plane::XorbFetcher for StubXorbFetcher {
    fn fetch_range(&self, _xorb_hash: &[u8; 32], _range: std::ops::Range<u64>) -> Result<Vec<u8>> {
        Err(CrabError::Internal("stub xorb fetcher".into()))
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
    fn resolve_head_symbolic_ref() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = tmp.path();

        // Create a symbolic ref HEAD → refs/heads/main.
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::create_dir_all(git_dir.join("refs/heads")).unwrap();
        std::fs::write(
            git_dir.join("refs/heads/main"),
            "abcdef1234567890abcdef1234567890abcdef12\n",
        )
        .unwrap();

        let (oid, ref_name) = resolve_head(git_dir).unwrap();
        assert_eq!(oid, "abcdef1234567890abcdef1234567890abcdef12");
        assert_eq!(ref_name, "refs/heads/main");
    }

    #[test]
    fn resolve_head_detached() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = tmp.path();

        std::fs::write(
            git_dir.join("HEAD"),
            "deadbeef1234567890abcdef1234567890abcdef\n",
        )
        .unwrap();

        let (oid, ref_name) = resolve_head(git_dir).unwrap();
        assert_eq!(oid, "deadbeef1234567890abcdef1234567890abcdef");
        assert_eq!(ref_name, "HEAD");
    }

    #[test]
    fn resolve_head_missing_file_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = tmp.path().join("nonexistent");

        let result = resolve_head(&git_dir);
        assert!(result.is_err());
    }

    #[test]
    fn resolve_head_packed_refs_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let git_dir = tmp.path();

        // Create HEAD pointing to a ref that only exists in packed-refs.
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/master\n").unwrap();
        std::fs::create_dir_all(git_dir.join("refs/heads")).unwrap();
        // No loose ref file — only packed-refs.
        std::fs::write(
            git_dir.join("packed-refs"),
            "# pack-refs with: peeled fully-peeled sorted\n\
             aabbccdd1234567890abcdef1234567890abcdef refs/heads/master\n",
        )
        .unwrap();

        let (oid, ref_name) = resolve_head(git_dir).unwrap();
        assert_eq!(oid, "aabbccdd1234567890abcdef1234567890abcdef");
        assert_eq!(ref_name, "refs/heads/master");
    }

    #[test]
    fn parse_ls_remote_heads_extracts_advertised_default() {
        let output = "\
ref: refs/heads/main\tHEAD\n\
1111111111111111111111111111111111111111\tHEAD\n\
1111111111111111111111111111111111111111\trefs/heads/main\n\
2222222222222222222222222222222222222222\trefs/heads/dev\n";

        let (default, heads) = parse_ls_remote_heads(output);

        assert_eq!(default.as_deref(), Some("main"));
        assert_eq!(heads, vec!["dev".to_owned(), "main".to_owned()]);
    }

    #[test]
    fn select_default_clone_branch_prefers_advertised_default() {
        let heads = vec!["dev".to_owned(), "main".to_owned()];
        let branch = select_default_clone_branch(Some("release"), &heads).unwrap();
        assert_eq!(branch, "release");
    }

    #[test]
    fn select_default_clone_branch_uses_main_without_advertised_head() {
        let heads = vec!["dev".to_owned(), "main".to_owned()];
        let branch = select_default_clone_branch(None, &heads).unwrap();
        assert_eq!(branch, "main");
    }

    #[test]
    fn select_default_clone_branch_uses_master_without_main() {
        let heads = vec!["dev".to_owned(), "master".to_owned()];
        let branch = select_default_clone_branch(None, &heads).unwrap();
        assert_eq!(branch, "master");
    }

    #[test]
    fn select_default_clone_branch_uses_single_branch() {
        let heads = vec!["release".to_owned()];
        let branch = select_default_clone_branch(None, &heads).unwrap();
        assert_eq!(branch, "release");
    }

    #[test]
    fn select_default_clone_branch_rejects_ambiguous_remote() {
        let heads = vec!["dev".to_owned(), "release".to_owned()];
        let err = select_default_clone_branch(None, &heads).unwrap_err();
        assert!(err.to_string().contains("--ref"));
    }

    #[test]
    fn pipeline_config_builder_defaults() {
        let config = PipelineConfig {
            source: "crab://bucket/repo".into(),
            git_dir: PathBuf::from("/tmp/test/.git"),
            ref_name: Some("refs/heads/main".into()),
            read_only: false,
            cache_dir: PathBuf::from("/tmp/test/cache"),
            cancel_token: CancellationToken::new(),
        };

        let builder = MountPipelineBuilder::new(config);
        assert!(builder.chunk_cache.is_none());
        assert!(builder.store_layout.is_none());
        assert_eq!(builder.refresh_interval, Duration::from_secs(30));
        assert!(!builder.no_refresh);
    }

    #[test]
    fn pipeline_builder_with_options() {
        let config = PipelineConfig {
            source: "/local/repo".into(),
            git_dir: PathBuf::from("/local/repo/.git"),
            ref_name: None,
            read_only: true,
            cache_dir: PathBuf::from("/tmp/cache"),
            cancel_token: CancellationToken::new(),
        };

        let builder = MountPipelineBuilder::new(config)
            .with_refresh_interval(Duration::from_secs(60))
            .with_no_refresh(true);

        assert_eq!(builder.refresh_interval, Duration::from_secs(60));
        assert!(builder.no_refresh);
    }
}
