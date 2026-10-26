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
//! 6. Populate the owned bare clone index; preserve local source indexes
//! 7. Prepare hydration → wire chunk cache
//! 8. Create resolver → merge snapshot + overlay
//! 9. Create engine → wire resolver + overlay + hydration + ODB reader
//! 10. Start owned hydration workers → return the service to the caller
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
use crate::blob::OdbReader;
use crate::core::error::{CrabError, Result};
use crate::engine::VfsEngine;
use crate::hydration::HydrationService;
use crate::integration::MountReadContext;
use crate::overlay::OverlayStore;
use crate::refresh::{RefreshConfig, RefreshService};
use crate::repository::refresh_mount_index;
use crate::repository::{GitRemoteRefFetcher, NoopRemoteRefFetcher, redact_url, resolve_mount_ref};
use crate::resolver::{FuseResolver, OverlayLookup};
use crate::snapshot::{NodeType, SnapshotStore};
use crate::verified_set::VerifiedSet;

// ---------------------------------------------------------------------------
// PipelineConfig
// ---------------------------------------------------------------------------

/// Ownership of the Git metadata used by a mount.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitMetadataOwnership {
    /// Borrow source metadata without fetching refs or replacing its index.
    Borrowed,
    /// Own a cached clone whose refs and index the mount may update.
    Mount,
}

/// Configuration for a single mount pipeline execution.
///
/// Passed to [`MountPipelineBuilder`] to control how the pipeline
/// resolves, clones, and mounts the repository.
#[derive(Debug, Clone)]
pub struct PipelineConfig {
    /// Remote URL or local path identifying the source repository.
    /// A remote data origin does not imply ownership of `git_dir`.
    pub source: String,
    /// Git directory borrowed from a source or allocated for a mount clone.
    /// Its path and bare/worktree shape do not establish mutation ownership.
    pub git_dir: PathBuf,
    /// Controls mutations independently of the data origin and bare/worktree shape.
    pub git_metadata: GitMetadataOwnership,
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

impl PipelineConfig {
    pub(crate) fn mount_index_path(&self) -> Option<PathBuf> {
        match self.git_metadata {
            GitMetadataOwnership::Mount => Some(self.git_dir.join("index")),
            GitMetadataOwnership::Borrowed => None,
        }
    }
}

// ---------------------------------------------------------------------------
// PipelineOutput
// ---------------------------------------------------------------------------

/// Successful output of the mount pipeline.
///
/// Contains the three primary components needed to operate the mount,
/// plus auxiliary handles for lifecycle management.
#[derive(Clone)]
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
    pub(crate) read_context: Option<MountReadContext>,
}

// ---------------------------------------------------------------------------
// MountPipelineBuilder
// ---------------------------------------------------------------------------

/// Builder that encapsulates the 11-step mount pipeline.
///
/// Construct via [`MountPipelineBuilder::new`], optionally configure
/// shared resources, then call [`Self::execute`] to run the full pipeline.
pub struct MountPipelineBuilder {
    config: PipelineConfig,
    /// Shared chunk cache. If not provided, a per-mount cache is created.
    chunk_cache: Option<Arc<ChunkCache>>,
    /// Store layout for xorb fetching. A layout requires a read context so
    /// pointer reconstruction uses the authenticated v2 resolver.
    store_layout: Option<StoreLayout>,
    /// Configured read-side hydrator supplied by the CLI integration.
    read_hydrator: Option<Arc<crab_read::ShardHydrator>>,
    read_context: Option<MountReadContext>,
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
            read_context: None,
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
        self.store_layout = Some(context.store_layout.clone());
        self.read_hydrator = Some(Arc::clone(&context.hydrator));
        self.read_context = Some(context);
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

    /// Prepare the mount state, then start its hydration workers.
    ///
    /// Returns the resolver, engine, and hydration service on success.
    /// On failure, partial resources are cleaned up before returning
    /// the error.
    pub fn execute(self) -> Result<PipelineOutput> {
        let config = &self.config;
        if config.cancel_token.is_cancelled() {
            return Err(CrabError::Cancelled);
        }
        let redacted = redact_url(&config.source);

        info!(
            source = %redacted,
            git_dir = %config.git_dir.display(),
            read_only = config.read_only,
            "executing mount pipeline"
        );

        // Step 1: Blobless clone (if git dir doesn't exist yet).
        if !config.git_dir.exists() {
            if config.git_metadata == GitMetadataOwnership::Borrowed {
                return Err(CrabError::NotFound {
                    path: config.git_dir.display().to_string(),
                });
            }
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

        // Step 6: Populate only mount-owned Git state.
        refresh_mount_index(
            &config.git_dir,
            config.mount_index_path().as_deref(),
            &head_oid,
        )?;

        // Step 7: Prepare hydration without starting tasks.
        let hydration = self.step_create_hydration()?;

        // Step 8: Create resolver (snapshot + overlay).
        let resolver = self.step_create_resolver(
            &snapshot,
            overlay.as_ref(),
            generation,
            &head_oid,
            &hydration,
        )?;

        // Step 9: Create engine (resolver + overlay + hydration + ODB reader).
        let engine = self.step_create_engine(&resolver, overlay.as_ref(), &hydration, &snapshot)?;

        // All fallible preparation must finish before workers retain cache state.
        // From here the service passes directly to the successful pipeline owner.
        if config.cancel_token.is_cancelled() {
            return Err(CrabError::Cancelled);
        }
        hydration.spawn_workers();

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
            read_context: self.read_context,
        })
    }

    // -----------------------------------------------------------------------
    // Pipeline steps
    // -----------------------------------------------------------------------

    /// Step 1: Prepare the owned blobless clone through the shared Git path.
    fn step_blobless_clone(&self) -> Result<()> {
        crate::repository::ensure_blobless_clone(
            &self.config.source,
            self.config.ref_name.as_deref(),
            &self.config.git_dir,
        )?;
        Ok(())
    }

    /// Step 2: Resolve HEAD from the git directory to (oid, ref_name).
    fn step_resolve_head(&self) -> Result<(String, String)> {
        info!(step = "resolve_head", "resolving HEAD");
        let (oid, ref_name) =
            resolve_mount_ref(&self.config.git_dir, self.config.ref_name.as_deref())?;
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

    fn step_create_hydration(&self) -> Result<Arc<HydrationService>> {
        info!(step = "hydration", "preparing hydration service");

        let cache = if let Some(c) = &self.chunk_cache {
            Arc::clone(c)
        } else {
            let cache_dir = self.config.cache_dir.join("chunks");
            Arc::new(ChunkCache::open(cache_dir, None)?)
        };

        let verified = Arc::new(VerifiedSet::default());

        create_hydration(
            cache,
            verified,
            // Native clients can remain busy after shutdown is requested. The
            // backend owns foreground hydration until kernel detachment.
            CancellationToken::new(),
            self.store_layout.clone(),
            self.read_hydrator.clone(),
            Some(crate::repository::staging_root_for_git_dir(
                &self.config.git_dir,
            )?),
            Some(self.config.git_dir.clone()),
        )
    }

    /// Step 8: Create the VFS resolver (merges snapshot + overlay).
    fn step_create_resolver(
        &self,
        snapshot: &Arc<SnapshotStore>,
        overlay: Option<&Arc<OverlayStore>>,
        generation: i64,
        head_oid: &str,
        hydration: &Arc<HydrationService>,
    ) -> Result<Arc<FuseResolver>> {
        info!(step = "resolver", "creating VFS resolver");

        let commit_time =
            crate::repository::commit_time_from_oid(&self.config.git_dir, head_oid).unwrap_or(0);
        let overlay_lookup: Option<Arc<dyn OverlayLookup>> =
            overlay.map(|ov| Arc::clone(ov) as Arc<dyn OverlayLookup>);
        // Ref reconciliation updates fully qualified branches. Retaining a
        // short selector here would refresh visible bytes without advancing
        // the owned branch used by remount and commit.
        let tracked_ref = self
            .config
            .ref_name
            .as_deref()
            .map(crate::repository::normalized_fetch_ref)
            .transpose()?;

        Ok(Arc::new(FuseResolver::new(
            Arc::clone(snapshot),
            overlay_lookup,
            generation,
            commit_time,
            Some(Arc::clone(hydration)),
            tracked_ref,
        )?))
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

        let odb_reader = OdbReader::new(&self.config.git_dir).map_err(|e| {
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
/// Callers invoke this after mounting, retain the returned handle, and await it
/// after cancelling the configured token. Aborting the task can detach a
/// blocking Git fetch that is still using repository state.
pub fn spawn_refresh_loop(
    output: &PipelineOutput,
    config: &PipelineConfig,
    mountpoint: &Path,
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

    let fetcher: Arc<dyn crate::repository::RemoteRefFetcher> = match config.git_metadata {
        GitMetadataOwnership::Mount => Arc::new(GitRemoteRefFetcher::new(config.git_dir.clone())),
        GitMetadataOwnership::Borrowed => Arc::new(NoopRemoteRefFetcher),
    };
    let refresh_config = RefreshConfig {
        remote_poll_interval: refresh_interval,
        local_poll_interval: Duration::from_millis(500),
        git_dir: config.git_dir.clone(),
    };

    let refresh_svc = Arc::new(RefreshService::new(
        output,
        mountpoint,
        overlay,
        fetcher,
        refresh_config,
        config.clone(),
        config.cancel_token.clone(),
    ));

    let handle = tokio::spawn(async move {
        refresh_svc.run().await;
    });

    info!("refresh loop started");
    Some(handle)
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
/// A `StoreLayout` is only valid with the authenticated v2 read hydrator.
/// Without it, the legacy synchronous index/shard adapters are stubs and
/// would otherwise let a mount start before failing on its first pointer read.
pub fn create_hydration(
    cache: Arc<ChunkCache>,
    verified: Arc<VerifiedSet>,
    cancel: CancellationToken,
    store_layout: Option<StoreLayout>,
    read_hydrator: Option<Arc<crab_read::ShardHydrator>>,
    staging_root: Option<PathBuf>,
    git_dir: Option<PathBuf>,
) -> Result<Arc<HydrationService>> {
    if store_layout.is_some() && read_hydrator.is_none() {
        return Err(CrabError::Configuration {
            key: "read_hydrator".into(),
            origin: "object-store mount requires an authenticated v2 read context".into(),
        });
    }

    let xorb_fetcher: Arc<dyn crate::data_plane::XorbFetcher> = match store_layout {
        Some(layout) => {
            let rt = tokio::runtime::Handle::current();
            Arc::new(crate::hydration::StoreBackedXorbFetcher::new(layout, rt))
        }
        None => Arc::new(StubXorbFetcher),
    };

    // Git subprocesses must finish before callbacks start: spawning from a read
    // can close inherited client descriptors and re-enter the locked FUSE inode.
    let lfs_storage = git_dir
        .as_deref()
        .map(crab_lfs::local::storage_from_git)
        .transpose()?;
    Ok(HydrationService::new(
        cache,
        verified,
        Arc::new(StubFileIndexResolver),
        Arc::new(StubShardLoader),
        xorb_fetcher,
        (read_hydrator.is_some() || staging_root.is_some() || lfs_storage.is_some()).then(|| {
            Arc::new(crab_read::PointerReader::new(
                read_hydrator,
                staging_root,
                lfs_storage,
            ))
        }),
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

    #[tokio::test]
    async fn cancelled_preparation_does_not_start_a_clone() {
        let root = tempfile::tempdir().unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let git_dir = root.path().join("git");
        let result = MountPipelineBuilder::new(PipelineConfig {
            source: root.path().join("missing-source").display().to_string(),
            git_dir: git_dir.clone(),
            git_metadata: GitMetadataOwnership::Mount,
            ref_name: None,
            read_only: true,
            cache_dir: root.path().join("state"),
            cancel_token: cancel,
        })
        .execute();
        assert!(matches!(result, Err(CrabError::Cancelled)));
        assert!(!git_dir.exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn disposable_blob_cache_path_does_not_block_preparation() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let state = tmp.path().join("state");
        std::fs::create_dir(&repo).unwrap();
        std::fs::create_dir(&state).unwrap();
        for args in [
            vec!["init", "-b", "main"],
            vec![
                "-c",
                "user.name=VFS test",
                "-c",
                "user.email=vfs@example.invalid",
                "commit",
                "--allow-empty",
                "-m",
                "empty fixture",
            ],
        ] {
            let _git_env = crate::test_support::GIT_DIR_MUTEX
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let output = std::process::Command::new("git")
                .args(args)
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("GIT_COMMON_DIR")
                .current_dir(&repo)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        std::fs::write(state.join("blob_cache"), b"not a directory").unwrap();
        let cache = Arc::new(ChunkCache::open(tmp.path().join("chunks"), None).unwrap());
        let config = PipelineConfig {
            source: repo.to_string_lossy().into_owned(),
            git_dir: repo.join(".git"),
            git_metadata: crate::pipeline::GitMetadataOwnership::Borrowed,
            ref_name: None,
            read_only: true,
            cache_dir: state.clone(),
            cancel_token: CancellationToken::new(),
        };
        let builder =
            MountPipelineBuilder::new(config.clone()).with_chunk_cache(Arc::clone(&cache));

        let output = builder.execute().unwrap();
        assert_eq!(
            std::fs::read(state.join("blob_cache")).unwrap(),
            b"not a directory"
        );
        config.cancel_token.cancel();
        tokio::time::timeout(Duration::from_secs(5), output.engine.shutdown_background())
            .await
            .unwrap();
        drop(output);
        assert_eq!(Arc::strong_count(&cache), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn hydration_startup_bypasses_chunk_storage_without_touching_live_state() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("mount");
        std::fs::create_dir(&state).unwrap();
        std::fs::write(state.join("chunks"), b"unavailable cache").unwrap();
        std::fs::write(state.join("overlay-sentinel"), b"live state").unwrap();
        let content = bytes::Bytes::from_static(b"actual remotely reconstructed bytes");
        let stored = crate::test_support::StoredPointer::new(
            &tmp.path().join("shared-cache"),
            content.clone(),
        )
        .await;
        let cancel = CancellationToken::new();
        let builder = MountPipelineBuilder::new(PipelineConfig {
            source: "crab://bucket/vfs-read-test".into(),
            git_dir: tmp.path().join("git"),
            git_metadata: crate::pipeline::GitMetadataOwnership::Mount,
            ref_name: None,
            read_only: false,
            cache_dir: state.clone(),
            cancel_token: cancel.clone(),
        })
        .with_read_context(stored.context.clone());
        let service = builder.step_create_hydration().unwrap();
        service.spawn_workers();
        // Shutdown is a detach request; a busy native mount still needs cold bytes.
        cancel.cancel();
        assert_eq!(
            service
                .read_range(
                    &crate::blob::FilePointer::Crab(stored.pointer.clone()),
                    0,
                    1024
                )
                .await
                .unwrap(),
            content
        );
        assert_eq!(stored.xorb_body_requests(), 1);
        cancel.cancel();
        tokio::time::timeout(Duration::from_secs(5), service.shutdown())
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(state.join("chunks")).unwrap(),
            b"unavailable cache"
        );
        assert_eq!(
            std::fs::read(state.join("overlay-sentinel")).unwrap(),
            b"live state"
        );
    }

    #[test]
    fn pipeline_config_builder_defaults() {
        let config = PipelineConfig {
            source: "crab://bucket/repo".into(),
            git_dir: PathBuf::from("/tmp/test/.git"),
            git_metadata: crate::pipeline::GitMetadataOwnership::Mount,
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
            git_metadata: crate::pipeline::GitMetadataOwnership::Borrowed,
            ref_name: None,
            read_only: true,
            cache_dir: PathBuf::from("/tmp/cache"),
            cancel_token: CancellationToken::new(),
        };

        let builder = MountPipelineBuilder::new(config)
            .with_refresh_interval(Duration::from_mins(1))
            .with_no_refresh(true);

        assert_eq!(builder.refresh_interval, Duration::from_mins(1));
        assert!(builder.no_refresh);
    }

    #[test]
    fn missing_borrowed_git_metadata_is_never_cloned() {
        let root = tempfile::tempdir().unwrap();
        let git_dir = root.path().join("missing.git");
        let config = PipelineConfig {
            source: "crab://bucket/read-origin".into(),
            git_dir: git_dir.clone(),
            git_metadata: GitMetadataOwnership::Borrowed,
            ref_name: None,
            read_only: true,
            cache_dir: root.path().join("cache"),
            cancel_token: CancellationToken::new(),
        };
        let result = MountPipelineBuilder::new(config).execute();
        assert!(matches!(result, Err(CrabError::NotFound { .. })) && !git_dir.exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn object_store_hydration_requires_v2_read_context() {
        let tmp = tempfile::tempdir().unwrap();
        let store = crab_storage::Store::new(Arc::new(object_store::memory::InMemory::new()));
        let layout = StoreLayout::new(store, "repo".to_owned());
        let error = create_hydration(
            Arc::new(ChunkCache::open(tmp.path().join("chunks"), Some(1024)).unwrap()),
            Arc::new(VerifiedSet::default()),
            CancellationToken::new(),
            Some(layout),
            None,
            None,
            None,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            CrabError::Configuration { key, .. } if key == "read_hydrator"
        ));
    }
}
