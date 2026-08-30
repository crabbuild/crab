//! Fetch pipeline: download packs from the object store into `.git/objects/pack/`.
//!
//! The key invariant is atomicity: a partially-written pack never lands
//! in the pack directory. Each pack is written to a tempfile on the same
//! filesystem, then atomically renamed into place. The `tempfile` crate
//! handles cleanup on any error path.

use std::collections::HashSet;
use std::hash::BuildHasher;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tracing::{debug, info, warn};

use crate::core::error::{CrabError, Result, check_cancelled};
use crate::git::remote_helper::{FetchEntry, FetchOptions};
use crate::git::shallow;
use crab_metadata::commit_graph::CommitGraphTraversal;
use crab_metadata::manifests::{Manifest, PackList, validate_manifest_payload};

/// Metadata for a single pack available on the remote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackInfo {
    pub pack_id: String,
    pub size: u64,
}

/// Trait abstracting the object store operations needed by the fetch pipeline.
///
/// The real implementation wraps [`crate::storage::Store`]; test doubles
/// live in the `tests` module below.
pub trait PackStore: Send + Sync {
    /// List all packs available on the remote.
    fn list_remote_packs(&self) -> impl std::future::Future<Output = Result<Vec<PackInfo>>> + Send;

    /// Download a single pack to a local filesystem path.
    fn download_pack_to_path(
        &self,
        pack_id: &str,
        dest: &Path,
    ) -> impl std::future::Future<Output = Result<u64>> + Send;

    /// Validate immutable companion artifacts required by the backing store.
    ///
    /// Generic pack sources may not have separate indexes. Direct Crab object
    /// stores override this to fail closed when the published remote index is
    /// missing or corrupt. A returned checksum is compared with the downloaded
    /// pack before the fetch batch can be accepted.
    fn validate_pack_index(
        &self,
        _pack_id: &str,
    ) -> impl std::future::Future<Output = Result<Option<String>>> + Send {
        async { Ok(None) }
    }
}

impl<S: PackStore> PackStore for Arc<S> {
    fn list_remote_packs(&self) -> impl std::future::Future<Output = Result<Vec<PackInfo>>> + Send {
        (**self).list_remote_packs()
    }

    fn download_pack_to_path(
        &self,
        pack_id: &str,
        dest: &Path,
    ) -> impl std::future::Future<Output = Result<u64>> + Send {
        (**self).download_pack_to_path(pack_id, dest)
    }

    fn validate_pack_index(
        &self,
        pack_id: &str,
    ) -> impl std::future::Future<Output = Result<Option<String>>> + Send {
        (**self).validate_pack_index(pack_id)
    }
}

/// Provides access to the remote's commit graph and pack list.
///
/// Used by the shallow fetch path to compute boundaries and filter packs.
/// Separated from [`PackStore`] because not all callers need graph access.
pub trait CommitGraphProvider: Send + Sync {
    /// Fetch the complete generation-bound commit graph from the remote.
    ///
    /// Returns `None` if the remote has no commit graph summary (e.g.
    /// legacy repositories that predate shallow support).
    fn fetch_commit_graph(
        &self,
    ) -> impl std::future::Future<Output = Result<Option<Arc<dyn CommitGraphTraversal>>>> + Send;

    /// Fetch the current [`PackList`] manifest from the remote.
    fn fetch_pack_list(&self) -> impl std::future::Future<Output = Result<PackList>> + Send;
}

/// Configuration for the fetch pipeline.
#[derive(Debug, Clone)]
pub struct FetchConfig {
    /// Maximum number of concurrent pack downloads.
    pub download_concurrency: usize,
    /// Maximum retries per individual pack on failure.
    pub max_retries: u32,
    /// Path to the `.git` directory (e.g. `/repo/.git`).
    pub git_dir: PathBuf,
    /// Use Pack_Metadata ref tips to download only packs relevant to the
    /// requested refs. Packs without metadata are downloaded unconditionally.
    /// Default: `false`.
    pub ref_filtering: bool,
    /// Skip indexing objects already present locally after downloading packs.
    /// Packs are still downloaded whole, but duplicate objects are not indexed
    /// twice during pack installation. Default: `false`.
    pub object_level_filtering: bool,
    /// Maximum bytes the fetch batch may transfer from the remote.
    /// `0` disables the check. Mirrors git's
    /// `uploadpack.maxEgressBytes`; see
    /// [`crate::core::config::Config::uploadpack_max_egress_bytes`].
    pub max_egress_bytes: u64,
}

impl Default for FetchConfig {
    fn default() -> Self {
        Self {
            download_concurrency: 8,
            max_retries: 3,
            git_dir: crate::git::discover::discover_git_dir()
                .unwrap_or_else(|_| PathBuf::from(".git")),
            ref_filtering: false,
            object_level_filtering: false,
            // Matches `Config::uploadpack_max_egress_bytes` default so a
            // `FetchConfig` built without a `Config` enforces the same
            // cap as one constructed via `from_config`.
            max_egress_bytes: 10 * 1024 * 1024 * 1024,
        }
    }
}

impl FetchConfig {
    /// Build a `FetchConfig` from the resolved [`Config`].
    ///
    /// Maps the top-level config fields into the fetch-specific struct.
    #[must_use]
    pub fn from_config(config: &crate::core::config::Config) -> Self {
        Self {
            download_concurrency: config.download_concurrency,
            max_retries: config.max_retries,
            ref_filtering: config.fetch_ref_filtering,
            object_level_filtering: config.fetch_object_level_filtering,
            max_egress_bytes: config.uploadpack_max_egress_bytes,
            ..Self::default()
        }
    }

    /// Returns the path to `.git/objects/pack/`.
    fn pack_dir(&self) -> PathBuf {
        self.git_dir.join("objects").join("pack")
    }
}

#[derive(Debug)]
enum ShallowUpdate {
    Preserve,
    Replace(Vec<String>),
    Remove,
}

#[derive(Debug)]
struct PreparedFetch {
    installed: Vec<PathBuf>,
    shallow_update: ShallowUpdate,
}

impl PreparedFetch {
    fn preserve(installed: Vec<PathBuf>) -> Self {
        Self {
            installed,
            shallow_update: ShallowUpdate::Preserve,
        }
    }

    fn remove_shallow(installed: Vec<PathBuf>) -> Self {
        Self {
            installed,
            shallow_update: ShallowUpdate::Remove,
        }
    }
}

/// Run the fetch pipeline for a batch of refs.
///
/// 1. Lists remote packs via `store`.
/// 2. Diffs against local `.git/objects/pack/`.
/// 3. Downloads missing packs with per-pack atomic write (tempfile + rename).
///
/// When `fetch_options` carries a depth constraint, the pipeline reads the
/// remote's complete commit graph, computes the shallow boundary, filters
/// packs, and writes `.git/shallow`. On `--unshallow` (depth=0 with an
/// existing shallow file), all remaining packs are downloaded and the
/// shallow file is removed.
///
/// Each pack is written to a tempfile in the pack directory, then
/// atomically renamed into `pack-{hash}.pack`. On any error the
/// tempfile is automatically cleaned up.
pub async fn run_fetch_batch<S: PackStore + 'static, G: CommitGraphProvider>(
    entries: &[FetchEntry],
    manifest: &Manifest,
    config: &FetchConfig,
    store: Arc<S>,
    graph_provider: Option<&G>,
    fetch_options: &FetchOptions,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Vec<PathBuf>> {
    if let Some(filter) = &fetch_options.filter {
        return Err(CrabError::Protocol(format!(
            "partial clone filter `{filter}` is not supported"
        )));
    }
    validate_manifest_payload(manifest)?;
    let pack_dir = config.pack_dir();
    tokio::fs::create_dir_all(&pack_dir).await?;

    let is_unshallow = is_unshallow_request(fetch_options, &config.git_dir);

    // Shallow fetch: compute boundary and filter packs.
    let prepared = if let Some(depth) = fetch_options.depth {
        if depth > 0 && !is_unshallow {
            run_shallow_fetch(
                entries,
                config,
                store.clone(),
                graph_provider,
                depth,
                fetch_options.deepen_relative,
                cancel,
            )
            .await?
        } else if is_unshallow {
            run_unshallow_fetch::<S, G>(entries, config, store.clone(), cancel).await?
        } else {
            PreparedFetch::preserve(
                run_full_fetch(entries, config, store.clone(), graph_provider, cancel).await?,
            )
        }
    } else if is_unshallow {
        run_unshallow_fetch::<S, G>(entries, config, store.clone(), cancel).await?
    } else {
        // Normal (full) fetch.
        PreparedFetch::preserve(
            run_full_fetch(entries, config, store.clone(), graph_provider, cancel).await?,
        )
    };

    // Object-level filtering: when enabled, duplicate objects from downloaded
    // packs are not indexed twice during pack installation. The packs are
    // still downloaded whole — this flag will be consumed by the indexing
    // pipeline once wired.
    if config.object_level_filtering {
        info!(
            installed_packs = prepared.installed.len(),
            "object-level filtering active: skipping locally-present objects during indexing"
        );
    }

    let _install_lock = acquire_fetch_install_lock(&pack_dir).await?;
    let ref_tips = entries
        .iter()
        .map(|entry| entry.sha.clone())
        .collect::<Vec<_>>();
    if let Err(validation_error) =
        crate::git::pack::validate_fetched_ref_tips(&config.git_dir, &ref_tips).await
    {
        if let Err(rollback_error) = rollback_fetch_batch(&pack_dir, &prepared.installed).await {
            return Err(CrabError::Internal(format!(
                "{validation_error}; failed to roll back rejected fetch batch: {rollback_error}"
            )));
        }
        return Err(validation_error);
    }

    if let Err(update_error) = apply_shallow_update(&config.git_dir, &prepared.shallow_update).await
    {
        if let Err(rollback_error) = rollback_fetch_batch(&pack_dir, &prepared.installed).await {
            return Err(CrabError::Internal(format!(
                "{update_error}; failed to roll back fetch after shallow-state update failed: {rollback_error}"
            )));
        }
        return Err(update_error);
    }

    Ok(prepared.installed)
}

async fn apply_shallow_update(git_dir: &Path, update: &ShallowUpdate) -> Result<()> {
    match update {
        ShallowUpdate::Preserve => Ok(()),
        ShallowUpdate::Replace(boundary) => shallow::write_shallow_file(git_dir, boundary).await,
        ShallowUpdate::Remove => shallow::remove_shallow_file(git_dir).await,
    }
}

/// Check whether this is an unshallow request (depth=0 with existing shallow file).
fn is_unshallow_request(fetch_options: &FetchOptions, git_dir: &Path) -> bool {
    fetch_options.depth == Some(0) && git_dir.join("shallow").exists()
}

/// Verify that a commit OID is within the shallow boundary.
///
/// Returns `Ok(())` if the OID is in the reachable set, or
/// `Err(BeyondShallowBoundary)` if it is not.
pub fn check_shallow_boundary<S: BuildHasher>(
    oid: &str,
    reachable_oids: &HashSet<String, S>,
) -> Result<()> {
    if reachable_oids.contains(oid) {
        Ok(())
    } else {
        Err(CrabError::BeyondShallowBoundary {
            oid: oid.to_owned(),
        })
    }
}

/// Run a shallow fetch: compute boundary, filter packs, download, write `.git/shallow`.
async fn run_shallow_fetch<S: PackStore + 'static, G: CommitGraphProvider>(
    entries: &[FetchEntry],
    config: &FetchConfig,
    store: Arc<S>,
    graph_provider: Option<&G>,
    depth: u32,
    deepen_relative: bool,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<PreparedFetch> {
    let graph_provider = graph_provider.ok_or_else(|| {
        CrabError::Protocol(
            "shallow fetch requires a commit graph provider, but none was supplied".into(),
        )
    })?;

    let Some(graph) = graph_provider.fetch_commit_graph().await? else {
        warn!("no complete commit graph on remote — falling back to full fetch");
        let installed =
            run_full_fetch(entries, config, store, Some(graph_provider), cancel).await?;
        return Ok(if deepen_relative {
            PreparedFetch::remove_shallow(installed)
        } else {
            PreparedFetch::preserve(installed)
        });
    };

    let ref_tips: Vec<String> = entries.iter().map(|e| e.sha.clone()).collect();
    let current_boundary = if deepen_relative {
        let current = shallow::read_shallow_file(&config.git_dir).await?;
        if current.is_empty() {
            return Err(CrabError::Protocol(
                "relative deepen requires an existing shallow boundary".to_owned(),
            ));
        }
        Some(current)
    } else {
        None
    };
    let traversal_roots = current_boundary.as_deref().unwrap_or(&ref_tips);
    if traversal_roots.iter().any(|oid| !graph.contains_oid(oid)) {
        warn!(
            "shallow traversal root is outside the bounded commit graph; falling back to full fetch"
        );
        return if deepen_relative {
            run_unshallow_fetch::<S, G>(entries, config, store, cancel).await
        } else {
            Ok(PreparedFetch::preserve(
                run_full_fetch(entries, config, store, Some(graph_provider), cancel).await?,
            ))
        };
    }

    let boundary = if let Some(current) = current_boundary.as_ref() {
        shallow::compute_shallow_boundary(graph.as_ref(), current, depth.saturating_add(1))
    } else {
        shallow::compute_shallow_boundary(graph.as_ref(), &ref_tips, depth)
    };

    if let Some(current) = current_boundary.as_ref() {
        let current_set: HashSet<&str> = current.iter().map(String::as_str).collect();
        let next_set: HashSet<&str> = boundary.iter().map(std::convert::AsRef::as_ref).collect();
        if current_set == next_set {
            warn!(
                "bounded commit graph cannot deepen past its retained edge; falling back to full fetch"
            );
            return run_unshallow_fetch::<S, G>(entries, config, store, cancel).await;
        }
    }

    debug!(
        depth,
        boundary_len = boundary.len(),
        "computed shallow boundary"
    );

    let pack_list = graph_provider.fetch_pack_list().await?;
    let filtered_ids =
        shallow::filter_packs_by_depth(&pack_list, graph.as_ref(), &boundary, &ref_tips);

    let pack_dir = config.pack_dir();
    tokio::fs::create_dir_all(&pack_dir).await?;

    // Build PackInfo list from filtered IDs.
    let filtered_packs: Vec<PackInfo> = pack_list
        .entries
        .iter()
        .filter(|e| filtered_ids.contains(&e.pack_id))
        .map(|e| PackInfo {
            pack_id: e.pack_id.clone(),
            size: e.size,
        })
        .collect();

    let missing = diff_missing_packs(&filtered_packs, &pack_dir)?;

    if missing.is_empty() {
        info!("all shallow packs already present locally");
    } else {
        debug!(count = missing.len(), "downloading shallow packs");
    }

    let installed = download_packs_concurrent(&missing, config, store, cancel).await?;

    info!(
        count = installed.len(),
        boundary_len = boundary.len(),
        "shallow fetch complete"
    );
    let shallow_update = if boundary.is_empty() {
        ShallowUpdate::Remove
    } else {
        ShallowUpdate::Replace(boundary.iter().map(ToString::to_string).collect())
    };
    Ok(PreparedFetch {
        installed,
        shallow_update,
    })
}

/// Run an unshallow fetch: download all remaining packs, remove `.git/shallow`.
async fn run_unshallow_fetch<S: PackStore + 'static, G: CommitGraphProvider>(
    entries: &[FetchEntry],
    config: &FetchConfig,
    store: Arc<S>,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<PreparedFetch> {
    info!("unshallow: downloading all remaining packs");

    // Unshallow downloads all packs — no ref filtering.
    let installed = run_full_fetch::<S, G>(entries, config, store, None, cancel).await?;

    info!(count = installed.len(), "unshallow fetch complete");
    Ok(PreparedFetch::remove_shallow(installed))
}

/// Run a normal (full) fetch: download all missing packs.
///
/// When `config.ref_filtering` is true and a `CommitGraphProvider` is
/// available, a pack is omitted only when Git proves the complete object
/// closure of every recorded pack tip is already in the local ODB.
async fn run_full_fetch<S: PackStore + 'static, G: CommitGraphProvider>(
    _entries: &[FetchEntry],
    config: &FetchConfig,
    store: Arc<S>,
    graph_provider: Option<&G>,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Vec<PathBuf>> {
    let pack_dir = config.pack_dir();
    tokio::fs::create_dir_all(&pack_dir).await?;

    let remote_packs = store.list_remote_packs().await?;

    let packs_to_fetch = if config.ref_filtering
        && let Some(gp) = graph_provider
    {
        match gp.fetch_pack_list().await {
            Ok(pack_list) => match filter_packs_by_local_object_closure(
                &remote_packs,
                &pack_list,
                &config.git_dir,
            )
            .await
            {
                Ok(filtered) => filtered,
                Err(e) => {
                    warn!(error = %e, "local object-closure proof failed, falling back to full fetch");
                    remote_packs
                }
            },
            Err(e) => {
                warn!(error = %e, "pack inventory read failed, falling back to full fetch");
                remote_packs
            }
        }
    } else {
        remote_packs
    };

    let missing = diff_missing_packs(&packs_to_fetch, &pack_dir)?;

    if missing.is_empty() {
        info!("all packs already present locally");
        return Ok(Vec::new());
    }

    debug!(count = missing.len(), "downloading missing packs");

    let installed = download_packs_concurrent(&missing, config, store, cancel).await?;

    info!(count = installed.len(), "fetch complete");
    Ok(installed)
}

async fn filter_packs_by_local_object_closure(
    remote_packs: &[PackInfo],
    pack_list: &PackList,
    git_dir: &Path,
) -> Result<Vec<PackInfo>> {
    let metadata: std::collections::HashMap<_, _> = pack_list
        .entries
        .iter()
        .map(|entry| (entry.pack_id.as_str(), entry))
        .collect();
    let git_dir = git_dir.to_owned();
    let remote_packs = remote_packs.to_vec();
    let tip_sets = remote_packs
        .iter()
        .map(|pack| {
            metadata
                .get(pack.pack_id.as_str())
                .and_then(|entry| (entry.size == pack.size).then(|| entry.ref_tips.clone()))
        })
        .collect::<Vec<_>>();

    tokio::task::spawn_blocking(move || {
        let mut proofs = std::collections::HashMap::<Vec<String>, bool>::new();
        let mut kept = Vec::new();
        let mut skipped_count = 0usize;
        let mut skipped_bytes = 0u64;
        for (pack, tips) in remote_packs.into_iter().zip(tip_sets) {
            let Some(mut tips) = tips.filter(|tips| !tips.is_empty()) else {
                kept.push(pack);
                continue;
            };
            tips.sort_unstable();
            tips.dedup();
            let complete = *proofs
                .entry(tips.clone())
                .or_insert_with(|| local_object_closure_is_complete(&git_dir, &tips));
            if complete {
                skipped_count += 1;
                skipped_bytes = skipped_bytes.saturating_add(pack.size);
            } else {
                kept.push(pack);
            }
        }
        if skipped_bytes > 0 {
            info!(
                skipped_packs = skipped_count,
                skipped_bytes, "exact local object-closure proof omitted remote packs"
            );
        }
        Ok(kept)
    })
    .await
    .map_err(|error| CrabError::Internal(format!("local object-closure proof join: {error}")))?
}

fn local_object_closure_is_complete(git_dir: &Path, tips: &[String]) -> bool {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut child = match Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .args([
            "rev-list",
            "--objects",
            "--quiet",
            "--missing=error",
            "--stdin",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(child) => child,
        Err(_) => return false,
    };
    let Some(mut stdin) = child.stdin.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return false;
    };
    for tip in tips {
        if writeln!(stdin, "{tip}").is_err() {
            let _ = child.kill();
            let _ = child.wait();
            return false;
        }
    }
    drop(stdin);
    child.wait().is_ok_and(|status| status.success())
}

/// Determine which remote packs are not yet present locally.
#[allow(clippy::unnecessary_wraps)]
fn diff_missing_packs(remote: &[PackInfo], pack_dir: &Path) -> Result<Vec<PackInfo>> {
    let mut missing = Vec::new();
    for info in remote {
        let pack_path = pack_dir.join(pack_filename(&info.pack_id));
        let idx_path = pack_dir.join(idx_filename(&info.pack_id));
        if !pack_path.is_file() || !idx_path.is_file() {
            missing.push(info.clone());
        }
    }
    Ok(missing)
}

/// Download and install multiple packs concurrently using a semaphore-bounded `JoinSet`.
///
/// Each pack is downloaded and installed in its own spawned task, bounded by
/// `config.download_concurrency`. Individual failures retry per `max_retries`
/// before propagating. A single failure does not cancel other in-flight downloads;
/// all tasks run to completion and errors are collected afterward.
///
/// # Egress cap
///
/// When `config.max_egress_bytes > 0`, the planned missing-pack byte total is
/// checked before any download starts. A limit of `0` disables the check
/// entirely.
async fn download_packs_concurrent<S: PackStore + 'static>(
    missing: &[PackInfo],
    config: &FetchConfig,
    store: Arc<S>,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<Vec<PathBuf>> {
    if missing.is_empty() {
        return Ok(Vec::new());
    }

    let semaphore = Arc::new(Semaphore::new(config.download_concurrency));
    let limit = config.max_egress_bytes;
    if limit != 0 {
        let planned = missing
            .iter()
            .fold(0u64, |acc, pack| acc.saturating_add(pack.size));
        if planned > limit {
            warn!(
                planned_bytes = planned,
                limit_bytes = limit,
                pack_count = missing.len(),
                "fetch egress cap exceeded before download; aborting batch"
            );
            return Err(CrabError::FetchTooLarge {
                size: planned,
                limit,
            });
        }
    }

    // Track bytes installed across all concurrent tasks. The limit check
    // runs in the join-set collection loop below so the enforcement
    // observes a single serialized view even though downloads run in
    // parallel under the semaphore.
    let total_bytes = Arc::new(std::sync::atomic::AtomicU64::new(0));
    // Keep object-store downloads parallel, but serialize canonical pack
    // installation under the cross-process pack-dir lock below.
    let install_semaphore = Arc::new(Semaphore::new(1));
    let mut join_set = JoinSet::new();

    for pack_info in missing {
        let sem = semaphore.clone();
        let store = store.clone();
        let config = config.clone();
        let pack_info = pack_info.clone();
        let cancel = cancel.clone();
        let total_bytes = total_bytes.clone();
        let install_semaphore = install_semaphore.clone();

        join_set.spawn(async move {
            let _permit = sem.acquire().await.map_err(|_| CrabError::Cancelled)?;
            check_cancelled(&cancel)?;
            let path = install_pack(&pack_info, &config, &*store, &install_semaphore).await?;
            // Account against the egress budget only after a pack is
            // fully installed so a failed download does not consume
            // budget. `pack_info.size` is the remote-advertised size,
            // which matches the bytes we pulled because
            // `verify_pack_sha1` would have rejected any truncation.
            let running = total_bytes
                .fetch_add(pack_info.size, std::sync::atomic::Ordering::Relaxed)
                + pack_info.size;
            Ok::<(PathBuf, u64), CrabError>((path, running))
        });
    }

    let mut installed = Vec::with_capacity(missing.len());
    let mut first_error = None;
    while let Some(result) = join_set.join_next().await {
        let (path, running) = match result {
            Ok(Ok(installed)) => installed,
            Ok(Err(error)) => {
                if first_error.is_none() {
                    first_error = Some(error);
                }
                continue;
            }
            Err(error) => {
                if first_error.is_none() {
                    first_error = Some(CrabError::Internal(format!(
                        "download task panicked: {error}"
                    )));
                }
                continue;
            }
        };
        installed.push(path);
        // `0` is the documented opt-out; trusted internal repos (or
        // unit tests for other parts of the fetch path) disable the
        // check by setting the field to zero.
        if limit != 0 && running > limit {
            warn!(
                running_bytes = running,
                limit_bytes = limit,
                installed_so_far = installed.len(),
                "fetch egress cap exceeded; aborting batch"
            );
            return Err(CrabError::FetchTooLarge {
                size: running,
                limit,
            });
        }
    }

    if let Some(error) = first_error {
        if let Err(rollback_error) = rollback_fetch_batch(&config.pack_dir(), &installed).await {
            return Err(CrabError::Internal(format!(
                "{error}; failed to roll back incomplete fetch batch: {rollback_error}"
            )));
        }
        return Err(error);
    }

    Ok(installed)
}

struct FetchInstallLock {
    _file: std::fs::File,
}

// Acquires the per-clone lock that serializes local pack install and validation.
// Independent git/crab processes can share the same `.git/objects`
// directory. Without this advisory lock, one process can rename a pack while
// another process validates requested tips against the local ODB snapshot.
async fn acquire_fetch_install_lock(pack_dir: &Path) -> Result<FetchInstallLock> {
    let pack_dir = pack_dir.to_owned();
    tokio::task::spawn_blocking(move || -> Result<FetchInstallLock> {
        use fs4::fs_std::FileExt as LockFileExt;

        std::fs::create_dir_all(&pack_dir)?;
        let lock_path = fetch_install_lock_path(&pack_dir);
        let file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)?;

        file.lock_exclusive()?;
        Ok(FetchInstallLock { _file: file })
    })
    .await
    .map_err(|e| CrabError::Internal(format!("fetch install lock join: {e}")))?
}

fn fetch_install_lock_path(pack_dir: &Path) -> PathBuf {
    if pack_dir.file_name().and_then(|name| name.to_str()) == Some("pack")
        && let Some(objects_dir) = pack_dir.parent()
    {
        return objects_dir.join(".crab-fetch-install.lock");
    }

    pack_dir.join(".crab-fetch-install.lock")
}

/// Download a single pack and atomically install it into the pack directory.
///
/// Retries up to `config.max_retries` on transient failures. The tempfile
/// is created in the same directory as the final destination so the rename
/// is guaranteed to be atomic (same filesystem).
async fn install_pack<S: PackStore>(
    pack_info: &PackInfo,
    config: &FetchConfig,
    store: &S,
    install_semaphore: &Semaphore,
) -> Result<PathBuf> {
    let pack_dir = config.pack_dir();
    let final_path = pack_dir.join(pack_filename(&pack_info.pack_id));

    let mut last_err = None;
    for attempt in 0..=config.max_retries {
        if attempt > 0 {
            warn!(
                pack_id = %pack_info.pack_id,
                attempt,
                "retrying pack download"
            );
        }

        match download_and_install(pack_info, store, &pack_dir, &final_path, install_semaphore)
            .await
        {
            Ok(path) => return Ok(path),
            Err(e) => {
                // Auth errors are fatal — retrying won't help.
                if let CrabError::NoCredentials
                | CrabError::AuthFailed { .. }
                | CrabError::AuthExpired { .. } = &e
                {
                    return Err(e);
                }
                warn!(
                    pack_id = %pack_info.pack_id,
                    attempt,
                    error = %e,
                    "pack download failed"
                );
                last_err = Some(e);
            }
        }
    }

    Err(last_err
        .unwrap_or_else(|| CrabError::Internal("retry loop exited without error".to_owned())))
}

/// Download pack bytes to a tempfile, verify the trailing SHA-1, and write the
/// `.pack` and matching `.idx`/`.rev` files to the pack directory atomically.
/// Object validation runs once the complete fetch batch is installed so Git
/// can resolve dependencies across sibling packs.
async fn download_and_install<S: PackStore>(
    pack_info: &PackInfo,
    store: &S,
    pack_dir: &Path,
    final_path: &Path,
    install_semaphore: &Semaphore,
) -> Result<PathBuf> {
    let published_pack_checksum = store.validate_pack_index(&pack_info.pack_id).await?;

    let temp = tokio::task::spawn_blocking({
        let dir = pack_dir.to_owned();
        move || -> Result<tempfile::TempPath> {
            Ok(tempfile::Builder::new()
                .prefix(".crab-pack-")
                .suffix(".pack.tmp")
                .tempfile_in(&dir)?
                .into_temp_path())
        }
    })
    .await
    .map_err(|e| CrabError::Internal(format!("temp pack create join: {e}")))??;
    let temp_path = temp.to_path_buf();

    let downloaded_bytes = match store
        .download_pack_to_path(&pack_info.pack_id, &temp_path)
        .await
    {
        Ok(bytes) => bytes,
        Err(e) => {
            // Surface auth errors with specific CRAB-E#### codes.
            if let CrabError::Storage(ref obj_err) = e
                && let Some(auth_err) =
                    crab_storage::classify_auth_error(obj_err).map(CrabError::from)
            {
                return Err(auth_err);
            }
            return Err(e);
        }
    };

    debug!(
        pack_id = %pack_info.pack_id,
        downloaded_bytes,
        "downloaded pack to temporary file"
    );

    if downloaded_bytes != pack_info.size {
        return Err(CrabError::CorruptObject {
            path: temp_path.display().to_string(),
            reason: format!(
                "downloaded pack has {downloaded_bytes} bytes, manifest advertises {}",
                pack_info.size
            ),
        });
    }

    let _install_permit = install_semaphore
        .acquire()
        .await
        .map_err(|_| CrabError::Cancelled)?;
    let _process_install_lock = acquire_fetch_install_lock(pack_dir).await?;

    let final_idx = pack_dir.join(idx_filename(&pack_info.pack_id));
    if final_path.exists() && !final_idx.exists() {
        warn!(
            pack_id = %pack_info.pack_id,
            "found pack without idx; rolling back before reinstall"
        );
        crate::git::pack::rollback_installed_pack(pack_dir, &pack_info.pack_id).await?;
    }

    // `max_input_size = 0` disables the size check on the fetch side:
    // the push-intake cap is a defense against hostile clients, not
    // against remotes we fetch from. The fetch-side equivalent
    // (`uploadpack.maxEgressBytes`) is tracked separately (F0-2).
    let installed = crate::git::pack::install_pack_file_locally(
        pack_dir,
        &temp_path,
        &pack_info.pack_id,
        0,
        false,
    )
    .await?;
    if let Some(expected) = published_pack_checksum
        && expected != installed.git_sha1
    {
        let computed = installed.git_sha1;
        crate::git::pack::rollback_installed_pack(pack_dir, &pack_info.pack_id).await?;
        return Err(CrabError::PackIntegrity { expected, computed });
    }
    debug_assert_eq!(installed.pack_path, final_path);
    drop(temp);
    Ok(installed.pack_path)
}

async fn rollback_fetch_batch(pack_dir: &Path, installed: &[PathBuf]) -> Result<()> {
    for pack_path in installed {
        let file_name = pack_path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                CrabError::Internal(format!(
                    "installed pack path has no UTF-8 filename: {}",
                    pack_path.display()
                ))
            })?;
        let pack_id = crab_git::pack::canonical_pack_id_from_object_filename(file_name)
            .ok_or_else(|| {
                CrabError::Internal(format!(
                    "installed pack has non-canonical name: {file_name}"
                ))
            })?;
        crate::git::pack::rollback_installed_pack(pack_dir, pack_id).await?;
    }
    Ok(())
}

/// Verify the trailing SHA1 checksum of a git pack.
///
/// Git packs end with a 20-byte SHA1 computed over all preceding bytes.
/// Returns `CrabError::PackIntegrity` on mismatch.
pub fn verify_pack_sha1(pack_bytes: &[u8]) -> Result<()> {
    crab_git::pack::verify_pack_sha1(pack_bytes).map_err(CrabError::from)
}

/// Canonical pack filename: `pack-{hash}.pack`.
fn pack_filename(pack_id: &str) -> String {
    format!("pack-{pack_id}.pack")
}

fn idx_filename(pack_id: &str) -> String {
    format!("pack-{pack_id}.idx")
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use sha1::{Digest, Sha1};
    use std::collections::HashMap;
    use std::process::Command;
    use std::sync::{Arc, Mutex};
    use tokio_util::sync::CancellationToken;

    use crate::git::remote_helper::FetchOptions;
    use crab_metadata::commit_graph::{CommitEntry, CommitGraphSummary};
    use crab_metadata::manifests::{PackEntry, PackList};

    /// Build pack bytes with a valid trailing SHA1 checksum.
    fn pack_with_sha1(content: &[u8]) -> Bytes {
        let mut hasher = Sha1::new();
        hasher.update(content);
        let hash = hasher.finalize();
        let mut buf = Vec::with_capacity(content.len() + crab_git::pack::PACK_SHA1_LEN);
        buf.extend_from_slice(content);
        buf.extend_from_slice(&hash);
        Bytes::from(buf)
    }

    /// In-memory pack store for testing.
    struct TestPackStore {
        packs: Mutex<HashMap<String, Bytes>>,
    }

    impl TestPackStore {
        fn new(packs: Vec<(String, Bytes)>) -> Self {
            Self {
                packs: Mutex::new(packs.into_iter().collect()),
            }
        }
    }

    impl PackStore for TestPackStore {
        async fn list_remote_packs(&self) -> Result<Vec<PackInfo>> {
            let packs = self.packs.lock().unwrap();
            Ok(packs
                .iter()
                .map(|(id, data)| PackInfo {
                    pack_id: id.clone(),
                    size: data.len() as u64,
                })
                .collect())
        }

        async fn download_pack_to_path(&self, pack_id: &str, dest: &Path) -> Result<u64> {
            let bytes = {
                let packs = self.packs.lock().unwrap();
                packs
                    .get(pack_id)
                    .cloned()
                    .ok_or_else(|| CrabError::NotFound {
                        path: pack_id.to_owned(),
                    })?
            };
            tokio::fs::write(dest, &bytes).await?;
            Ok(bytes.len() as u64)
        }
    }

    struct MissingIndexPackStore {
        download_called: std::sync::atomic::AtomicBool,
    }

    impl PackStore for MissingIndexPackStore {
        async fn list_remote_packs(&self) -> Result<Vec<PackInfo>> {
            Ok(vec![PackInfo {
                pack_id: "missing-index".to_owned(),
                size: 1,
            }])
        }

        async fn download_pack_to_path(&self, _pack_id: &str, _dest: &Path) -> Result<u64> {
            self.download_called
                .store(true, std::sync::atomic::Ordering::SeqCst);
            Err(CrabError::Internal(
                "pack download must not start before index validation".to_owned(),
            ))
        }

        async fn validate_pack_index(&self, pack_id: &str) -> Result<Option<String>> {
            Err(CrabError::NotFound {
                path: format!("packs/pack-{pack_id}.idx"),
            })
        }
    }

    struct MismatchedIndexPackStore {
        inner: TestPackStore,
    }

    impl PackStore for MismatchedIndexPackStore {
        async fn list_remote_packs(&self) -> Result<Vec<PackInfo>> {
            self.inner.list_remote_packs().await
        }

        async fn download_pack_to_path(&self, pack_id: &str, dest: &Path) -> Result<u64> {
            self.inner.download_pack_to_path(pack_id, dest).await
        }

        async fn validate_pack_index(&self, _pack_id: &str) -> Result<Option<String>> {
            Ok(Some("0".repeat(40)))
        }
    }

    /// Store that fails the first N download attempts, then succeeds.
    struct FailingPackStore {
        inner: TestPackStore,
        failures_remaining: Mutex<u32>,
    }

    impl FailingPackStore {
        fn new(packs: Vec<(String, Bytes)>, fail_count: u32) -> Self {
            Self {
                inner: TestPackStore::new(packs),
                failures_remaining: Mutex::new(fail_count),
            }
        }
    }

    impl PackStore for FailingPackStore {
        async fn list_remote_packs(&self) -> Result<Vec<PackInfo>> {
            self.inner.list_remote_packs().await
        }

        async fn download_pack_to_path(&self, pack_id: &str, dest: &Path) -> Result<u64> {
            {
                let mut remaining = self.failures_remaining.lock().unwrap();
                if *remaining > 0 {
                    *remaining -= 1;
                    return Err(CrabError::NetworkTransient(object_store::Error::Generic {
                        store: "test",
                        source: "injected failure".into(),
                    }));
                }
            }
            self.inner.download_pack_to_path(pack_id, dest).await
        }
    }

    fn temp_git_dir() -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        let initialized = Command::new("git")
            .args(["init", "--bare"])
            .arg(directory.path())
            .output()
            .unwrap();
        assert!(initialized.status.success());
        directory
    }

    fn temp_repo_with_commit() -> (tempfile::TempDir, String, String) {
        let directory = tempfile::tempdir().unwrap();
        let initialized = Command::new("git")
            .arg("init")
            .arg(directory.path())
            .output()
            .unwrap();
        assert!(initialized.status.success());
        std::fs::write(directory.path().join("tracked.txt"), b"reachable content\n").unwrap();
        let added = Command::new("git")
            .arg("-C")
            .arg(directory.path())
            .args(["add", "tracked.txt"])
            .output()
            .unwrap();
        assert!(added.status.success());
        let committed = Command::new("git")
            .arg("-C")
            .arg(directory.path())
            .args([
                "-c",
                "user.name=Crab Test",
                "-c",
                "user.email=crab@example.invalid",
                "commit",
                "-m",
                "fixture",
            ])
            .output()
            .unwrap();
        assert!(committed.status.success());
        let commit = Command::new("git")
            .arg("-C")
            .arg(directory.path())
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        assert!(commit.status.success());
        let blob = Command::new("git")
            .arg("-C")
            .arg(directory.path())
            .args(["rev-parse", "HEAD:tracked.txt"])
            .output()
            .unwrap();
        assert!(blob.status.success());
        (
            directory,
            String::from_utf8(commit.stdout).unwrap().trim().to_owned(),
            String::from_utf8(blob.stdout).unwrap().trim().to_owned(),
        )
    }

    fn real_git_pack() -> Bytes {
        pack_with_sha1(b"PACK\0\0\0\x02\0\0\0\0")
    }

    /// No-op graph provider for tests that don't need shallow support.
    struct NoOpGraphProvider;

    impl CommitGraphProvider for NoOpGraphProvider {
        async fn fetch_commit_graph(&self) -> Result<Option<Arc<dyn CommitGraphTraversal>>> {
            Ok(None)
        }
        async fn fetch_pack_list(&self) -> Result<PackList> {
            Ok(PackList::default())
        }
    }

    /// Graph provider with a pre-configured summary and pack list.
    struct TestGraphProvider {
        summary: Option<CommitGraphSummary>,
        pack_list: PackList,
    }

    impl CommitGraphProvider for TestGraphProvider {
        async fn fetch_commit_graph(&self) -> Result<Option<Arc<dyn CommitGraphTraversal>>> {
            Ok(self
                .summary
                .clone()
                .map(|summary| Arc::new(summary) as Arc<dyn CommitGraphTraversal>))
        }
        async fn fetch_pack_list(&self) -> Result<PackList> {
            Ok(self.pack_list.clone())
        }
    }

    fn config_for(git_dir: &Path) -> FetchConfig {
        FetchConfig {
            download_concurrency: 4,
            max_retries: 3,
            git_dir: git_dir.to_owned(),
            ref_filtering: false,
            object_level_filtering: false,
            // Most existing tests never exercise the egress cap —
            // disable it so their pre-cap behavior is preserved.
            // Tests that *do* exercise the cap set it explicitly.
            max_egress_bytes: 0,
        }
    }

    async fn run_test_fetch_batch<S: PackStore + 'static, G: CommitGraphProvider>(
        entries: &[FetchEntry],
        config: &FetchConfig,
        store: Arc<S>,
        graph_provider: Option<&G>,
        fetch_options: &FetchOptions,
        cancel: &CancellationToken,
    ) -> Result<Vec<PathBuf>> {
        let manifest = Manifest::default_for_repo("refs/heads/main");
        run_fetch_batch(
            entries,
            &manifest,
            config,
            store,
            graph_provider,
            fetch_options,
            cancel,
        )
        .await
    }

    #[tokio::test]
    async fn fetch_rejects_stale_manifest_proof_before_installation() {
        let tmp = temp_git_dir();
        let config = config_for(tmp.path());
        let store = Arc::new(TestPackStore::new(vec![(
            "unread".to_owned(),
            real_git_pack(),
        )]));
        let mut manifest = Manifest::default_for_repo("refs/heads/main");
        manifest.generation = 1;

        let error = run_fetch_batch(
            &[],
            &manifest,
            &config,
            store,
            None::<&NoOpGraphProvider>,
            &FetchOptions::default(),
            &CancellationToken::new(),
        )
        .await
        .expect_err("stale manifest proof must fail closed");

        assert!(matches!(error, CrabError::CorruptObject { .. }));
        assert_eq!(
            std::fs::read_dir(tmp.path().join("objects/pack"))
                .expect("read untouched pack directory")
                .count(),
            0
        );
    }

    #[tokio::test]
    async fn fetch_rejects_partial_clone_filter_before_io() {
        let tmp = tempfile::tempdir().unwrap();
        let config = config_for(tmp.path());
        let store = Arc::new(TestPackStore::new(Vec::new()));
        let fetch_options = FetchOptions {
            depth: None,
            deepen_relative: false,
            filter: Some(crate::git::remote_helper::FilterSpec::BlobNone),
        };

        let error = run_test_fetch_batch(
            &[],
            &config,
            store,
            None::<&NoOpGraphProvider>,
            &fetch_options,
            &CancellationToken::new(),
        )
        .await
        .expect_err("partial clone filters must fail before pack access");

        assert!(matches!(error, CrabError::Protocol(message) if message.contains("blob:none")));
        assert!(
            !tmp.path().join("objects/pack").exists(),
            "filter rejection must happen before creating the pack directory"
        );
    }

    #[tokio::test]
    async fn fetch_rejects_missing_remote_index_before_pack_download() {
        let tmp = temp_git_dir();
        let mut config = config_for(tmp.path());
        config.max_retries = 0;
        let store = Arc::new(MissingIndexPackStore {
            download_called: std::sync::atomic::AtomicBool::new(false),
        });

        let error = run_test_fetch_batch(
            &[],
            &config,
            Arc::clone(&store),
            None::<&NoOpGraphProvider>,
            &FetchOptions::default(),
            &CancellationToken::new(),
        )
        .await
        .expect_err("missing remote index must reject fetch");

        assert!(matches!(error, CrabError::NotFound { path } if path.ends_with(".idx")));
        assert!(
            !store
                .download_called
                .load(std::sync::atomic::Ordering::SeqCst),
            "pack download must not start when its remote index is missing"
        );
        assert_eq!(
            std::fs::read_dir(tmp.path().join("objects/pack"))
                .expect("read pack directory")
                .count(),
            0
        );
    }

    #[tokio::test]
    async fn fetch_rejects_index_for_different_pack_and_rolls_back() {
        let tmp = temp_git_dir();
        let config = config_for(tmp.path());
        let pack_id = "d".repeat(64);
        let store = Arc::new(MismatchedIndexPackStore {
            inner: TestPackStore::new(vec![(pack_id.clone(), real_git_pack())]),
        });

        let error = run_test_fetch_batch(
            &[],
            &config,
            store,
            None::<&NoOpGraphProvider>,
            &FetchOptions::default(),
            &CancellationToken::new(),
        )
        .await
        .expect_err("index checksum for another pack must fail fetch");

        assert!(matches!(error, CrabError::PackIntegrity { .. }));
        let pack_dir = tmp.path().join("objects/pack");
        assert!(!pack_dir.join(pack_filename(&pack_id)).exists());
        assert!(!pack_dir.join(idx_filename(&pack_id)).exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn fetch_install_lock_serializes_acquirers() {
        let pack_dir = tempfile::tempdir().unwrap();
        let first = acquire_fetch_install_lock(pack_dir.path()).await.unwrap();
        let second_dir = pack_dir.path().to_owned();

        let second_task =
            tokio::spawn(async move { acquire_fetch_install_lock(&second_dir).await.unwrap() });

        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        assert!(
            !second_task.is_finished(),
            "second lock should wait for the first guard to drop"
        );

        drop(first);
        let second = tokio::time::timeout(std::time::Duration::from_secs(1), second_task)
            .await
            .expect("second lock should acquire after first guard drops")
            .unwrap();
        drop(second);
    }

    // --- SHA1 verification ---

    #[test]
    fn valid_pack_passes_sha1_verification() {
        let pack = pack_with_sha1(b"PACK valid content here");
        assert!(verify_pack_sha1(&pack).is_ok());
    }

    #[test]
    fn corrupted_pack_fails_sha1_verification() {
        let mut pack = pack_with_sha1(b"PACK valid content here").to_vec();
        // Flip a bit in the content region.
        pack[4] ^= 0x01;
        assert!(matches!(
            verify_pack_sha1(&pack),
            Err(CrabError::PackIntegrity { .. })
        ));
    }

    #[test]
    fn truncated_pack_fails_sha1_verification() {
        // Fewer than 20 bytes — can't even hold a SHA1 trailer.
        let short = b"too short";
        assert!(matches!(
            verify_pack_sha1(short),
            Err(CrabError::PackIntegrity { .. })
        ));
    }

    // --- Successful fetch writes pack to correct location ---

    #[tokio::test]
    async fn fetch_writes_pack_to_pack_dir() {
        let tmp = temp_git_dir();
        let git_dir = tmp.path();
        let config = config_for(git_dir);

        let pack_data = real_git_pack();
        let store = Arc::new(TestPackStore::new(vec![(
            "abc123".to_owned(),
            pack_data.clone(),
        )]));

        let installed = run_test_fetch_batch(
            &[],
            &config,
            store,
            None::<&NoOpGraphProvider>,
            &FetchOptions::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(installed.len(), 1);
        let expected = git_dir.join("objects/pack/pack-abc123.pack");
        assert_eq!(installed[0], expected);
        assert!(expected.exists());

        let on_disk = std::fs::read(&expected).unwrap();
        assert_eq!(on_disk, &pack_data[..]);
    }

    #[tokio::test]
    async fn fetch_rejects_download_size_mismatch_before_installation() {
        let tmp = temp_git_dir();
        let pack_dir = tmp.path().join("objects/pack");
        let pack_data = real_git_pack();
        let store = TestPackStore::new(vec![("size-mismatch".to_owned(), pack_data.clone())]);
        let pack_info = PackInfo {
            pack_id: "size-mismatch".to_owned(),
            size: pack_data.len() as u64 + 1,
        };
        let final_path = pack_dir.join(pack_filename(&pack_info.pack_id));

        let error = download_and_install(
            &pack_info,
            &store,
            &pack_dir,
            &final_path,
            &Semaphore::new(1),
        )
        .await
        .unwrap_err();

        assert!(matches!(error, CrabError::CorruptObject { .. }));
        assert!(error.to_string().contains("manifest advertises"));
        assert!(!final_path.exists());
        assert_eq!(std::fs::read_dir(&pack_dir).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn batch_validation_rejects_missing_ref_tip_and_rolls_back() {
        let tmp = temp_git_dir();
        let config = config_for(tmp.path());
        let pack_id = "b".repeat(64);
        let store = Arc::new(TestPackStore::new(vec![(pack_id.clone(), real_git_pack())]));
        let missing_tip = "a".repeat(40);
        let entries = [FetchEntry {
            sha: missing_tip.clone(),
            ref_name: "refs/heads/main".to_owned(),
        }];

        let error = run_test_fetch_batch(
            &entries,
            &config,
            store,
            None::<&NoOpGraphProvider>,
            &FetchOptions::default(),
            &CancellationToken::new(),
        )
        .await
        .expect_err("an advertised ref tip must exist after fetch");

        assert!(matches!(
            error,
            CrabError::FetchMalformedObject { oid, kind, .. }
                if oid == missing_tip && kind == "ref-tip"
        ));
        let pack_dir = tmp.path().join("objects/pack");
        assert!(!pack_dir.join(pack_filename(&pack_id)).exists());
        assert!(!pack_dir.join(idx_filename(&pack_id)).exists());
        assert!(!pack_dir.join(format!("pack-{pack_id}.rev")).exists());
    }

    #[tokio::test]
    async fn rejected_unshallow_preserves_existing_boundary() {
        let tmp = temp_git_dir();
        let config = config_for(tmp.path());
        let original_boundary = "2222222222222222222222222222222222222222\n";
        std::fs::write(tmp.path().join("shallow"), original_boundary).unwrap();
        let pack_id = "c".repeat(64);
        let pack_data = real_git_pack();
        let store = Arc::new(TestPackStore::new(vec![(
            pack_id.clone(),
            pack_data.clone(),
        )]));
        let missing_tip = "1111111111111111111111111111111111111111".to_owned();
        let graph = TestGraphProvider {
            summary: Some(CommitGraphSummary {
                generation: 1,
                commits: vec![CommitEntry {
                    oid: missing_tip.clone(),
                    gen_number: 0,
                    parents: Vec::new(),
                }],
            }),
            pack_list: PackList {
                generation: 1,
                entries: vec![PackEntry::new(&pack_id, pack_data.len() as u64, Vec::new())],
            },
        };
        let entries = [FetchEntry {
            sha: missing_tip,
            ref_name: "refs/heads/main".to_owned(),
        }];

        run_test_fetch_batch(
            &entries,
            &config,
            store,
            Some(&graph),
            &FetchOptions {
                depth: Some(u32::MAX),
                deepen_relative: false,
                filter: None,
            },
            &CancellationToken::new(),
        )
        .await
        .expect_err("missing advertised tip must reject the prepared unshallow fetch");

        assert_eq!(
            std::fs::read_to_string(tmp.path().join("shallow")).unwrap(),
            original_boundary
        );
    }

    // --- Tempfile is cleaned up on error ---

    #[tokio::test]
    async fn tempfile_cleaned_up_on_persistent_error() {
        let tmp = temp_git_dir();
        let git_dir = tmp.path();
        let config = FetchConfig {
            download_concurrency: 4,
            max_retries: 0,
            git_dir: git_dir.to_owned(),
            ref_filtering: false,
            object_level_filtering: false,
            max_egress_bytes: 0,
        };

        let store = Arc::new(FailingPackStore::new(
            vec![("bad-pack".to_owned(), pack_with_sha1(b"data"))],
            100,
        ));

        let result = run_test_fetch_batch(
            &[],
            &config,
            store,
            None::<&NoOpGraphProvider>,
            &FetchOptions::default(),
            &CancellationToken::new(),
        )
        .await;
        assert!(result.is_err());

        // No files should remain in the pack directory (tempfile cleaned up).
        let pack_dir = git_dir.join("objects/pack");
        if pack_dir.exists() {
            let entries: Vec<_> = std::fs::read_dir(&pack_dir)
                .unwrap()
                .filter_map(|e| e.ok())
                .collect();
            assert!(
                entries.is_empty(),
                "expected no files in pack dir, found: {entries:?}"
            );
        }
    }

    // --- Atomic rename ensures no partial packs ---

    #[tokio::test]
    async fn no_partial_pack_on_success() {
        let tmp = temp_git_dir();
        let git_dir = tmp.path();
        let config = config_for(git_dir);

        let pack_data = real_git_pack();
        let store = Arc::new(TestPackStore::new(vec![(
            "def456".to_owned(),
            pack_data.clone(),
        )]));

        let installed = run_test_fetch_batch(
            &[],
            &config,
            store,
            None::<&NoOpGraphProvider>,
            &FetchOptions::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(installed.len(), 1);

        // Only canonical final artifacts should exist — no temp files.
        let pack_dir = git_dir.join("objects/pack");
        let entries: Vec<_> = std::fs::read_dir(&pack_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        let mut names = entries
            .iter()
            .map(|entry| entry.file_name())
            .collect::<Vec<_>>();
        names.sort();
        assert_eq!(
            names,
            ["pack-def456.idx", "pack-def456.pack", "pack-def456.rev",]
                .map(std::ffi::OsString::from)
        );
    }

    #[tokio::test]
    async fn successful_indexed_fetch_removes_download_tempfile() {
        let tmp = temp_git_dir();
        let git_dir = tmp.path();
        let config = config_for(git_dir);
        let store = Arc::new(TestPackStore::new(vec![(
            "indexed".to_owned(),
            real_git_pack(),
        )]));

        run_test_fetch_batch(
            &[],
            &config,
            store,
            None::<&NoOpGraphProvider>,
            &FetchOptions::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        let pack_dir = git_dir.join("objects/pack");
        let mut entries: Vec<_> = std::fs::read_dir(pack_dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        entries.sort();
        assert_eq!(
            entries,
            ["pack-indexed.idx", "pack-indexed.pack", "pack-indexed.rev",]
                .map(std::ffi::OsString::from)
        );
    }

    // --- Retry succeeds after transient failure ---

    #[tokio::test]
    async fn retry_succeeds_after_transient_failure() {
        let tmp = temp_git_dir();
        let git_dir = tmp.path();
        let config = config_for(git_dir);

        let pack_data = real_git_pack();
        let store = Arc::new(FailingPackStore::new(
            vec![("retry-pack".to_owned(), pack_data.clone())],
            2,
        ));

        let installed = run_test_fetch_batch(
            &[],
            &config,
            store,
            None::<&NoOpGraphProvider>,
            &FetchOptions::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(installed.len(), 1);

        let expected = git_dir.join("objects/pack/pack-retry-pack.pack");
        assert!(expected.exists());
        let on_disk = std::fs::read(&expected).unwrap();
        assert_eq!(on_disk, &pack_data[..]);
    }

    // --- Already-present packs are skipped ---

    #[tokio::test]
    async fn already_present_packs_are_skipped() {
        let tmp = temp_git_dir();
        let git_dir = tmp.path();
        let config = config_for(git_dir);

        let pack_dir = git_dir.join("objects/pack");
        std::fs::create_dir_all(&pack_dir).unwrap();
        std::fs::write(pack_dir.join("pack-existing.pack"), b"already here").unwrap();
        std::fs::write(pack_dir.join("pack-existing.idx"), b"index").unwrap();

        let store = Arc::new(TestPackStore::new(vec![(
            "existing".to_owned(),
            pack_with_sha1(b"remote data"),
        )]));

        let installed = run_test_fetch_batch(
            &[],
            &config,
            store,
            None::<&NoOpGraphProvider>,
            &FetchOptions::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(installed.is_empty(), "should skip already-present pack");

        let on_disk = std::fs::read(pack_dir.join("pack-existing.pack")).unwrap();
        assert_eq!(on_disk, b"already here");
    }

    #[tokio::test]
    async fn pack_without_idx_is_fetched_again() {
        let tmp = temp_git_dir();
        let git_dir = tmp.path();
        let config = config_for(git_dir);

        let pack_dir = git_dir.join("objects/pack");
        std::fs::create_dir_all(&pack_dir).unwrap();
        std::fs::write(pack_dir.join("pack-existing.pack"), b"pack without index").unwrap();

        let remote_pack = real_git_pack();
        let store = Arc::new(TestPackStore::new(vec![(
            "existing".to_owned(),
            remote_pack.clone(),
        )]));

        let installed = run_test_fetch_batch(
            &[],
            &config,
            store,
            None::<&NoOpGraphProvider>,
            &FetchOptions::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(installed.len(), 1, "pack-only install should be repaired");
        let on_disk = std::fs::read(pack_dir.join("pack-existing.pack")).unwrap();
        assert_eq!(on_disk, &remote_pack[..]);
    }

    // --- Multiple packs fetched ---

    #[tokio::test]
    async fn fetches_multiple_packs() {
        let tmp = temp_git_dir();
        let git_dir = tmp.path();
        let config = config_for(git_dir);

        let store = Arc::new(TestPackStore::new(vec![
            ("pack-a".to_owned(), real_git_pack()),
            ("pack-b".to_owned(), real_git_pack()),
        ]));

        let installed = run_test_fetch_batch(
            &[],
            &config,
            store,
            None::<&NoOpGraphProvider>,
            &FetchOptions::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(installed.len(), 2);

        let pack_dir = git_dir.join("objects/pack");
        assert!(pack_dir.join("pack-pack-a.pack").exists());
        assert!(pack_dir.join("pack-pack-b.pack").exists());
    }

    // --- Opportunistic shard sync removed ---
    //
    // The `ShardSyncer` trait, `spawn_opportunistic_shard_sync`, and
    // `NoOpSyncer` have been deleted. `ShardSynchronizer` (the concrete
    // type in `metadata/shard_sync.rs`) is now invoked directly from
    // clone/pull/fetch in section 10 of the SlateDB metadata spec.

    // --- Auth error propagation ---

    /// Store that returns an auth error on download.
    struct AuthFailingStore {
        inner: TestPackStore,
        error: Mutex<Option<CrabError>>,
    }

    impl AuthFailingStore {
        fn permission_denied(packs: Vec<(String, Bytes)>) -> Self {
            Self {
                inner: TestPackStore::new(packs),
                error: Mutex::new(Some(CrabError::Storage(
                    object_store::Error::PermissionDenied {
                        path: "repo/packs/test".into(),
                        source: "403 Forbidden".into(),
                    },
                ))),
            }
        }

        fn unauthenticated(packs: Vec<(String, Bytes)>) -> Self {
            Self {
                inner: TestPackStore::new(packs),
                error: Mutex::new(Some(CrabError::Storage(
                    object_store::Error::Unauthenticated {
                        path: "repo/packs/test".into(),
                        source: "401 Unauthorized".into(),
                    },
                ))),
            }
        }

        fn expired_token(packs: Vec<(String, Bytes)>) -> Self {
            Self {
                inner: TestPackStore::new(packs),
                error: Mutex::new(Some(CrabError::Storage(object_store::Error::Generic {
                    store: "S3",
                    source: "The security token included in the request is expired".into(),
                }))),
            }
        }
    }

    impl PackStore for AuthFailingStore {
        async fn list_remote_packs(&self) -> Result<Vec<PackInfo>> {
            self.inner.list_remote_packs().await
        }

        async fn download_pack_to_path(&self, pack_id: &str, dest: &Path) -> Result<u64> {
            let err = {
                let mut guard = self.error.lock().unwrap();
                guard.take()
            };
            match err {
                Some(e) => Err(e),
                None => self.inner.download_pack_to_path(pack_id, dest).await,
            }
        }
    }

    #[tokio::test]
    async fn auth_failed_propagated_with_correct_error_code() {
        let tmp = temp_git_dir();
        let config = config_for(tmp.path());

        let store = Arc::new(AuthFailingStore::permission_denied(vec![(
            "auth-pack".to_owned(),
            pack_with_sha1(b"PACK data"),
        )]));

        let result = run_test_fetch_batch(
            &[],
            &config,
            store,
            None::<&NoOpGraphProvider>,
            &FetchOptions::default(),
            &CancellationToken::new(),
        )
        .await;
        assert!(
            matches!(result, Err(CrabError::AuthFailed { .. })),
            "expected AuthFailed, got {result:?}"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("CRAB-E0042"),
            "error message should contain CRAB-E0042: {err_msg}"
        );
    }

    #[tokio::test]
    async fn no_credentials_propagated_with_correct_error_code() {
        let tmp = temp_git_dir();
        let config = config_for(tmp.path());

        let store = Arc::new(AuthFailingStore::unauthenticated(vec![(
            "auth-pack".to_owned(),
            pack_with_sha1(b"PACK data"),
        )]));

        let result = run_test_fetch_batch(
            &[],
            &config,
            store,
            None::<&NoOpGraphProvider>,
            &FetchOptions::default(),
            &CancellationToken::new(),
        )
        .await;
        assert!(
            matches!(result, Err(CrabError::NoCredentials)),
            "expected NoCredentials, got {result:?}"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("CRAB-E0040"),
            "error message should contain CRAB-E0040: {err_msg}"
        );
    }

    #[tokio::test]
    async fn expired_token_propagated_with_correct_error_code() {
        let tmp = temp_git_dir();
        let config = config_for(tmp.path());

        let store = Arc::new(AuthFailingStore::expired_token(vec![(
            "auth-pack".to_owned(),
            pack_with_sha1(b"PACK data"),
        )]));

        let result = run_test_fetch_batch(
            &[],
            &config,
            store,
            None::<&NoOpGraphProvider>,
            &FetchOptions::default(),
            &CancellationToken::new(),
        )
        .await;
        assert!(
            matches!(result, Err(CrabError::AuthExpired { .. })),
            "expected AuthExpired, got {result:?}"
        );
        let err_msg = result.unwrap_err().to_string();
        assert!(
            err_msg.contains("CRAB-E0043"),
            "error message should contain CRAB-E0043: {err_msg}"
        );
    }

    #[tokio::test]
    async fn auth_errors_are_not_retried() {
        let tmp = temp_git_dir();
        let config = FetchConfig {
            download_concurrency: 4,
            max_retries: 5,
            git_dir: tmp.path().to_owned(),
            ref_filtering: false,
            object_level_filtering: false,
            max_egress_bytes: 0,
        };

        // The store only has one error to give. If the fetch pipeline
        // retried, the second attempt would succeed — so a failure here
        // proves the auth error was propagated immediately.
        let store = Arc::new(AuthFailingStore::permission_denied(vec![(
            "retry-auth".to_owned(),
            pack_with_sha1(b"PACK data"),
        )]));

        let result = run_test_fetch_batch(
            &[],
            &config,
            store,
            None::<&NoOpGraphProvider>,
            &FetchOptions::default(),
            &CancellationToken::new(),
        )
        .await;
        assert!(
            matches!(result, Err(CrabError::AuthFailed { .. })),
            "auth error should propagate immediately without retry, got {result:?}"
        );
    }

    // --- Shallow fetch ---

    fn linear_chain(len: usize) -> (CommitGraphSummary, String) {
        let mut commits = Vec::with_capacity(len);
        for i in 0..len {
            let parents = if i == 0 {
                vec![]
            } else {
                vec![format!("c{}", i - 1)]
            };
            commits.push(CommitEntry {
                oid: format!("c{i}"),
                gen_number: i as u64,
                parents,
            });
        }
        let tip = format!("c{}", len - 1);
        (
            CommitGraphSummary {
                generation: 1,
                commits,
            },
            tip,
        )
    }

    #[tokio::test]
    async fn shallow_fetch_writes_shallow_file() {
        let tmp = temp_git_dir();
        let git_dir = tmp.path();
        let config = config_for(git_dir);

        let pack_data = real_git_pack();
        let store = Arc::new(TestPackStore::new(vec![(
            "shallow-pack".to_owned(),
            pack_data.clone(),
        )]));

        let (summary, tip) = linear_chain(5);
        let graph = TestGraphProvider {
            summary: Some(summary),
            pack_list: PackList {
                generation: 1,
                entries: vec![PackEntry::new(
                    "shallow-pack",
                    pack_data.len() as u64,
                    Vec::new(),
                )],
            },
        };

        let entries = vec![FetchEntry {
            sha: tip,
            ref_name: "refs/heads/main".into(),
        }];

        let installed = run_shallow_fetch(
            &entries,
            &config,
            store,
            Some(&graph),
            2,
            false,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(installed.installed.len(), 1);
        apply_shallow_update(git_dir, &installed.shallow_update)
            .await
            .unwrap();

        // .git/shallow should exist with boundary commits.
        let shallow_path = git_dir.join("shallow");
        assert!(shallow_path.exists(), ".git/shallow should be written");
        let content = std::fs::read_to_string(&shallow_path).unwrap();
        assert!(!content.is_empty(), "shallow file should have content");
    }

    #[tokio::test]
    async fn relative_deepen_advances_existing_shallow_boundary() {
        let tmp = temp_git_dir();
        let git_dir = tmp.path();
        let config = config_for(git_dir);
        shallow::write_shallow_file(git_dir, &["c4"]).await.unwrap();

        let pack_data = real_git_pack();
        let store = Arc::new(TestPackStore::new(vec![(
            "relative-pack".to_owned(),
            pack_data.clone(),
        )]));
        let (summary, tip) = linear_chain(5);
        let graph = TestGraphProvider {
            summary: Some(summary),
            pack_list: PackList {
                generation: 1,
                entries: vec![PackEntry::new(
                    "relative-pack",
                    pack_data.len() as u64,
                    Vec::new(),
                )],
            },
        };
        let entries = vec![FetchEntry {
            sha: tip,
            ref_name: "refs/heads/main".to_owned(),
        }];

        let prepared = run_shallow_fetch(
            &entries,
            &config,
            store,
            Some(&graph),
            1,
            true,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        apply_shallow_update(git_dir, &prepared.shallow_update)
            .await
            .unwrap();

        assert_eq!(shallow::read_shallow_file(git_dir).await.unwrap(), ["c3"]);
    }

    #[tokio::test]
    async fn relative_deepen_past_compacted_edge_falls_back_to_full_fetch() {
        let tmp = temp_git_dir();
        let git_dir = tmp.path();
        let config = config_for(git_dir);
        shallow::write_shallow_file(git_dir, &["c1"]).await.unwrap();

        let pack_data = real_git_pack();
        let store = Arc::new(TestPackStore::new(vec![(
            "compacted-pack".to_owned(),
            pack_data.clone(),
        )]));
        let (mut summary, tip) = linear_chain(5);
        summary.compact_to_limit(4);
        let graph = TestGraphProvider {
            summary: Some(summary),
            pack_list: PackList::default(),
        };
        let entries = vec![FetchEntry {
            sha: tip,
            ref_name: "refs/heads/main".to_owned(),
        }];

        let installed = run_shallow_fetch(
            &entries,
            &config,
            store,
            Some(&graph),
            1,
            true,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(installed.installed.len(), 1);
        apply_shallow_update(git_dir, &installed.shallow_update)
            .await
            .unwrap();
        assert!(!git_dir.join("shallow").exists());
    }

    #[tokio::test]
    async fn shallow_fetch_without_graph_falls_back_to_full() {
        let tmp = temp_git_dir();
        let git_dir = tmp.path();
        let config = config_for(git_dir);

        let pack_data = real_git_pack();
        let store = Arc::new(TestPackStore::new(vec![(
            "fb-pack".to_owned(),
            pack_data.clone(),
        )]));

        // Graph provider returns None — no summary on remote.
        let graph = TestGraphProvider {
            summary: None,
            pack_list: PackList::default(),
        };

        let fetch_opts = FetchOptions {
            depth: Some(3),
            deepen_relative: false,
            filter: None,
        };

        let installed = run_test_fetch_batch(
            &[],
            &config,
            store,
            Some(&graph),
            &fetch_opts,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        // Falls back to full fetch — pack is downloaded.
        assert_eq!(installed.len(), 1);

        // No shallow file should be written (full fetch fallback).
        assert!(!git_dir.join("shallow").exists());
    }

    #[tokio::test]
    async fn unshallow_removes_shallow_file() {
        let tmp = temp_git_dir();
        let git_dir = tmp.path();
        let config = config_for(git_dir);

        // Pre-create a .git/shallow file to simulate an existing shallow clone.
        std::fs::write(git_dir.join("shallow"), b"c3\n").unwrap();

        let pack_data = real_git_pack();
        let store = Arc::new(TestPackStore::new(vec![(
            "us-pack".to_owned(),
            pack_data.clone(),
        )]));

        let fetch_opts = FetchOptions {
            depth: Some(0),
            deepen_relative: false,
            filter: None,
        };

        let installed = run_test_fetch_batch(
            &[],
            &config,
            store,
            None::<&NoOpGraphProvider>,
            &fetch_opts,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(installed.len(), 1);
        // .git/shallow should be removed.
        assert!(
            !git_dir.join("shallow").exists(),
            ".git/shallow should be removed after unshallow"
        );
    }

    #[tokio::test]
    async fn depth_zero_without_shallow_file_is_normal_fetch() {
        let tmp = temp_git_dir();
        let git_dir = tmp.path();
        let config = config_for(git_dir);

        let pack_data = real_git_pack();
        let store = Arc::new(TestPackStore::new(vec![(
            "norm-pack".to_owned(),
            pack_data.clone(),
        )]));

        // depth=0 but no .git/shallow file — not an unshallow, just normal.
        let fetch_opts = FetchOptions {
            depth: Some(0),
            deepen_relative: false,
            filter: None,
        };

        let installed = run_test_fetch_batch(
            &[],
            &config,
            store,
            None::<&NoOpGraphProvider>,
            &fetch_opts,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(installed.len(), 1);
    }

    // --- Boundary check ---

    #[test]
    fn check_shallow_boundary_reachable_oid() {
        let reachable: HashSet<String> = ["c4", "c3"].iter().map(|s| s.to_string()).collect();
        assert!(check_shallow_boundary("c4", &reachable).is_ok());
    }

    #[test]
    fn check_shallow_boundary_unreachable_oid() {
        let reachable: HashSet<String> = ["c4", "c3"].iter().map(|s| s.to_string()).collect();
        let result = check_shallow_boundary("c1", &reachable);
        assert!(
            matches!(result, Err(CrabError::BeyondShallowBoundary { .. })),
            "expected BeyondShallowBoundary, got {result:?}"
        );
    }

    #[test]
    fn beyond_shallow_boundary_error_message() {
        let err = CrabError::BeyondShallowBoundary {
            oid: "abc123".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("abc123"));
        assert!(msg.contains("CRAB-E0091"));
        assert!(msg.contains("--deepen"));
        assert!(msg.contains("--unshallow"));
    }

    // --- Ref-based pack filtering ---

    #[test]
    fn fetch_config_ref_filtering_defaults_to_false() {
        let config = FetchConfig::default();
        assert!(!config.ref_filtering);
    }

    #[tokio::test]
    async fn ref_tip_intersection_never_omits_unproven_packs() {
        let tmp = temp_git_dir();
        let git_dir = tmp.path();
        let mut config = config_for(git_dir);
        config.ref_filtering = true;

        let relevant_data = real_git_pack();
        let irrelevant_data = real_git_pack();

        let store = Arc::new(TestPackStore::new(vec![
            ("relevant-pack".to_owned(), relevant_data.clone()),
            ("irrelevant-pack".to_owned(), irrelevant_data.clone()),
        ]));

        let graph = TestGraphProvider {
            summary: Some(CommitGraphSummary {
                generation: 1,
                commits: vec![
                    CommitEntry {
                        oid: "sha_main".to_owned(),
                        gen_number: 0,
                        parents: Vec::new(),
                    },
                    CommitEntry {
                        oid: "sha_other_branch".to_owned(),
                        gen_number: 0,
                        parents: Vec::new(),
                    },
                ],
            }),
            pack_list: PackList {
                generation: 1,
                entries: vec![
                    PackEntry::new(
                        "relevant-pack",
                        relevant_data.len() as u64,
                        vec!["sha_main".to_string()],
                    ),
                    PackEntry::new(
                        "irrelevant-pack",
                        irrelevant_data.len() as u64,
                        vec!["sha_other_branch".to_string()],
                    ),
                ],
            },
        };

        let entries = vec![FetchEntry {
            sha: "sha_main".to_string(),
            ref_name: "refs/heads/main".into(),
        }];

        let installed = run_full_fetch(
            &entries,
            &config,
            store,
            Some(&graph),
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(installed.len(), 2);
        let pack_dir = git_dir.join("objects/pack");
        assert!(pack_dir.join("pack-relevant-pack.pack").exists());
        assert!(
            pack_dir.join("pack-irrelevant-pack.pack").exists(),
            "requested-ref intersection is not exact local-object proof"
        );
    }

    #[tokio::test]
    async fn ref_filtering_keeps_ancestor_packs_for_descendant_tip() {
        let tmp = temp_git_dir();
        let git_dir = tmp.path();
        let mut config = config_for(git_dir);
        config.ref_filtering = true;

        let base_data = real_git_pack();
        let child_data = real_git_pack();

        let store = Arc::new(TestPackStore::new(vec![
            ("base-pack".to_owned(), base_data.clone()),
            ("child-pack".to_owned(), child_data.clone()),
        ]));

        let graph = TestGraphProvider {
            summary: Some(CommitGraphSummary {
                generation: 1,
                commits: vec![
                    CommitEntry {
                        oid: "sha_base".to_owned(),
                        gen_number: 0,
                        parents: Vec::new(),
                    },
                    CommitEntry {
                        oid: "sha_child".to_owned(),
                        gen_number: 1,
                        parents: vec!["sha_base".to_owned()],
                    },
                ],
            }),
            pack_list: PackList {
                generation: 1,
                entries: vec![
                    PackEntry::new(
                        "base-pack",
                        base_data.len() as u64,
                        vec!["sha_base".to_owned()],
                    ),
                    PackEntry::new(
                        "child-pack",
                        child_data.len() as u64,
                        vec!["sha_child".to_owned()],
                    ),
                ],
            },
        };

        let entries = vec![FetchEntry {
            sha: "sha_child".to_owned(),
            ref_name: "refs/heads/main".into(),
        }];

        let installed = run_full_fetch(
            &entries,
            &config,
            store,
            Some(&graph),
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(installed.len(), 2);
        let pack_dir = git_dir.join("objects/pack");
        assert!(pack_dir.join("pack-base-pack.pack").exists());
        assert!(pack_dir.join("pack-child-pack.pack").exists());
    }

    #[tokio::test]
    async fn ref_filtering_keeps_packs_without_filtering_proof() {
        let tmp = temp_git_dir();
        let git_dir = tmp.path();
        let mut config = config_for(git_dir);
        config.ref_filtering = true;

        let legacy_data = real_git_pack();
        let tagged_data = real_git_pack();

        let store = Arc::new(TestPackStore::new(vec![
            ("legacy-pack".to_owned(), legacy_data.clone()),
            ("tagged-pack".to_owned(), tagged_data.clone()),
        ]));

        let graph = TestGraphProvider {
            summary: Some(CommitGraphSummary {
                generation: 1,
                commits: vec![
                    CommitEntry {
                        oid: "sha_main".to_owned(),
                        gen_number: 0,
                        parents: Vec::new(),
                    },
                    CommitEntry {
                        oid: "sha_other".to_owned(),
                        gen_number: 0,
                        parents: Vec::new(),
                    },
                ],
            }),
            pack_list: PackList {
                generation: 1,
                entries: vec![
                    // Empty tips carry no filtering proof.
                    PackEntry::new("legacy-pack", legacy_data.len() as u64, Vec::new()),
                    // Tagged pack with non-matching ref tips.
                    PackEntry::new(
                        "tagged-pack",
                        tagged_data.len() as u64,
                        vec!["sha_other".to_string()],
                    ),
                ],
            },
        };

        let entries = vec![FetchEntry {
            sha: "sha_main".to_string(),
            ref_name: "refs/heads/main".into(),
        }];

        let installed = run_full_fetch(
            &entries,
            &config,
            store,
            Some(&graph),
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(installed.len(), 2);
        let pack_dir = git_dir.join("objects/pack");
        assert!(
            pack_dir.join("pack-legacy-pack.pack").exists(),
            "pack without a filtering proof should always be downloaded"
        );
        assert!(
            pack_dir.join("pack-tagged-pack.pack").exists(),
            "a non-local recorded tip must remain conservative"
        );
    }

    #[tokio::test]
    async fn ref_filtering_disabled_downloads_all_packs() {
        let tmp = temp_git_dir();
        let git_dir = tmp.path();
        let config = config_for(git_dir);
        // ref_filtering is false by default in config_for.
        assert!(!config.ref_filtering);

        let pack_a = real_git_pack();
        let pack_b = real_git_pack();

        let store = Arc::new(TestPackStore::new(vec![
            ("pack-a".to_owned(), pack_a.clone()),
            ("pack-b".to_owned(), pack_b.clone()),
        ]));

        let graph = TestGraphProvider {
            summary: None,
            pack_list: PackList {
                generation: 1,
                entries: vec![
                    PackEntry::new("pack-a", pack_a.len() as u64, vec!["sha_main".to_string()]),
                    PackEntry::new("pack-b", pack_b.len() as u64, vec!["sha_other".to_string()]),
                ],
            },
        };

        let entries = vec![FetchEntry {
            sha: "sha_main".to_string(),
            ref_name: "refs/heads/main".into(),
        }];

        let installed = run_full_fetch(
            &entries,
            &config,
            store,
            Some(&graph),
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        // Both packs downloaded when filtering is disabled.
        assert_eq!(installed.len(), 2);
    }

    #[tokio::test]
    async fn ref_filtering_without_graph_provider_downloads_all() {
        let tmp = temp_git_dir();
        let git_dir = tmp.path();
        let mut config = config_for(git_dir);
        config.ref_filtering = true;

        let pack_data = real_git_pack();
        let store = Arc::new(TestPackStore::new(vec![(
            "ng-pack".to_owned(),
            pack_data.clone(),
        )]));

        let entries = vec![FetchEntry {
            sha: "sha_main".to_string(),
            ref_name: "refs/heads/main".into(),
        }];

        // No graph provider — filtering cannot run, falls back to full fetch.
        let installed = run_full_fetch(
            &entries,
            &config,
            store,
            None::<&NoOpGraphProvider>,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(installed.len(), 1);
    }

    #[tokio::test]
    async fn local_object_closure_omits_only_exactly_complete_packs() {
        let (repo, tip, _) = temp_repo_with_commit();
        let remote = vec![
            PackInfo {
                pack_id: "complete".to_owned(),
                size: 100,
            },
            PackInfo {
                pack_id: "legacy".to_owned(),
                size: 200,
            },
        ];
        let pack_list = PackList {
            generation: 1,
            entries: vec![
                PackEntry::new("complete", 100, vec![tip]),
                PackEntry::new("legacy", 200, Vec::new()),
            ],
        };

        let selected =
            filter_packs_by_local_object_closure(&remote, &pack_list, &repo.path().join(".git"))
                .await
                .unwrap();

        assert_eq!(selected, vec![remote[1].clone()]);
    }

    #[tokio::test]
    async fn local_object_closure_keeps_pack_when_reachable_blob_is_missing() {
        let (repo, tip, blob) = temp_repo_with_commit();
        let loose_blob = repo
            .path()
            .join(".git/objects")
            .join(&blob[..2])
            .join(&blob[2..]);
        std::fs::remove_file(loose_blob).unwrap();
        let remote = vec![PackInfo {
            pack_id: "incomplete".to_owned(),
            size: 100,
        }];
        let pack_list = PackList {
            generation: 1,
            entries: vec![PackEntry::new("incomplete", 100, vec![tip])],
        };

        let selected =
            filter_packs_by_local_object_closure(&remote, &pack_list, &repo.path().join(".git"))
                .await
                .unwrap();

        assert_eq!(selected, remote);
    }

    // --- Object-level filtering ---

    #[test]
    fn fetch_config_object_level_filtering_defaults_to_false() {
        let config = FetchConfig::default();
        assert!(!config.object_level_filtering);
    }

    #[tokio::test]
    async fn object_level_filtering_enabled_fetch_succeeds() {
        let tmp = temp_git_dir();
        let git_dir = tmp.path();
        let mut config = config_for(git_dir);
        config.object_level_filtering = true;

        let pack_data = real_git_pack();
        let store = Arc::new(TestPackStore::new(vec![("olf-pack".to_owned(), pack_data)]));

        let installed = run_test_fetch_batch(
            &[],
            &config,
            store,
            None::<&NoOpGraphProvider>,
            &FetchOptions::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(installed.len(), 1);
        let pack_dir = git_dir.join("objects/pack");
        assert!(pack_dir.join("pack-olf-pack.pack").exists());
    }

    // --- Egress size cap (uploadpack.maxEgressBytes) ---

    /// Serialize installs (`download_concurrency = 1`) so the egress
    /// check observes packs in deterministic order. The cap semantics
    /// we assert are insensitive to ordering, but serializing makes
    /// "only N of M packs installed" checks reliable.
    fn serialized_egress_config(git_dir: &Path, max_egress_bytes: u64) -> FetchConfig {
        FetchConfig {
            download_concurrency: 1,
            max_retries: 0,
            git_dir: git_dir.to_owned(),
            ref_filtering: false,
            object_level_filtering: false,
            max_egress_bytes,
        }
    }

    #[tokio::test]
    async fn fetch_exceeding_max_egress_cancels_cleanly() {
        let tmp = temp_git_dir();
        let git_dir = tmp.path();
        let pack_a = real_git_pack();
        let pack_b = real_git_pack();
        let pack_c = real_git_pack();
        let planned = (pack_a.len() + pack_b.len() + pack_c.len()) as u64;
        let cap = planned - 1;
        let config = serialized_egress_config(git_dir, cap);

        let store = Arc::new(TestPackStore::new(vec![
            ("egress-a".to_owned(), pack_a),
            ("egress-b".to_owned(), pack_b),
            ("egress-c".to_owned(), pack_c),
        ]));

        let result = run_test_fetch_batch(
            &[],
            &config,
            store,
            None::<&NoOpGraphProvider>,
            &FetchOptions::default(),
            &CancellationToken::new(),
        )
        .await;

        match result {
            Err(CrabError::FetchTooLarge { size, limit }) => {
                assert!(size > cap, "running total {size} must exceed cap {cap}");
                assert_eq!(limit, cap);
            }
            other => panic!("expected FetchTooLarge, got {other:?}"),
        }

        let pack_dir = git_dir.join("objects/pack");
        let on_disk: Vec<_> = std::fs::read_dir(&pack_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        assert!(
            on_disk.is_empty(),
            "preflight egress rejection should avoid installing packs"
        );
    }

    #[tokio::test]
    async fn fetch_at_limit_succeeds() {
        let tmp = temp_git_dir();
        let git_dir = tmp.path();
        let pack_a = real_git_pack();
        let pack_b = real_git_pack();
        let cap = (pack_a.len() + pack_b.len()) as u64;
        let config = serialized_egress_config(git_dir, cap);

        let store = Arc::new(TestPackStore::new(vec![
            ("at-limit-a".to_owned(), pack_a),
            ("at-limit-b".to_owned(), pack_b),
        ]));

        let installed = run_test_fetch_batch(
            &[],
            &config,
            store,
            None::<&NoOpGraphProvider>,
            &FetchOptions::default(),
            &CancellationToken::new(),
        )
        .await
        .expect("fetch exactly at the cap should succeed");

        assert_eq!(installed.len(), 2);
        let pack_dir = git_dir.join("objects/pack");
        assert!(pack_dir.join("pack-at-limit-a.pack").exists());
        assert!(pack_dir.join("pack-at-limit-b.pack").exists());
    }

    #[tokio::test]
    async fn fetch_max_egress_zero_means_unlimited() {
        let tmp = temp_git_dir();
        let git_dir = tmp.path();
        let config = serialized_egress_config(git_dir, 0);

        let mut packs = Vec::new();
        for i in 0..10 {
            packs.push((format!("unlim-{i}"), real_git_pack()));
        }
        let store = Arc::new(TestPackStore::new(packs));

        let installed = run_test_fetch_batch(
            &[],
            &config,
            store,
            None::<&NoOpGraphProvider>,
            &FetchOptions::default(),
            &CancellationToken::new(),
        )
        .await
        .expect("max_egress_bytes = 0 should disable the check entirely");

        assert_eq!(installed.len(), 10);
    }
}
