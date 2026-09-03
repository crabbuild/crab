//! Long-running filter protocol v2 implementation.
//!
//! Git invokes this as `git-filter-process` and communicates via stdin/stdout
//! using the packet-line framing from `gix-packetline`. The protocol declares
//! `clean`, `smudge`, and `delay` capabilities.
//!
//! Because the filter process is stdin/stdout bound (blocking I/O), all
//! packet-line reads and writes run inside `spawn_blocking`. The async
//! boundary lives in the command dispatch loop.

use std::collections::HashMap;
use std::io::{self, BufRead, BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::core::context::AppContext;
use crate::core::error::{CrabError, Result};
use crate::git::prefetch::PrefetchQueue;
use crate::git::worktree::WorktreeContext;
use crate::speculation::access_db::AsyncAccessDb;
use crate::speculation::driver::SpeculativeDriver;
use crate::speculation::predictor::Predictor;
use crab_git::lfs_pointer::MAX_LFS_POINTER_SIZE;
use crab_git::pointer_detect::{PointerKind, classify};
use crab_lfs::LfsObjectStore;
use crab_staging::StagingArea;
use crab_types::pointer::{MAX_POINTER_SIZE, Pointer};

use bytes::Bytes;

/// Protocol version string sent by git during the handshake.
const GIT_FILTER_CLIENT: &str = "git-filter-client";

/// Protocol version we declare.
const FILTER_PROTOCOL_VERSION: &str = "version=2";

/// Capabilities we advertise to git.
const CAPABILITIES: &[&str] = &["clean", "smudge", "delay"];
const SPECULATION_DECAY_WINDOW_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// Synchronously resolves an LFS object store when a non-lazy smudge needs it.
pub type LfsStoreLoader = Arc<dyn Fn() -> Option<Arc<LfsObjectStore>> + Send + Sync + 'static>;

#[derive(Clone)]
struct LfsStoreSource {
    eager: Option<Arc<LfsObjectStore>>,
    loader: Option<LfsStoreLoader>,
    resolved: Arc<std::sync::OnceLock<Option<Arc<LfsObjectStore>>>>,
}

impl LfsStoreSource {
    #[cfg(test)]
    fn eager(store: Option<Arc<LfsObjectStore>>) -> Self {
        Self::new(store, None)
    }

    fn new(store: Option<Arc<LfsObjectStore>>, loader: Option<LfsStoreLoader>) -> Self {
        Self {
            eager: store,
            loader,
            resolved: Arc::new(std::sync::OnceLock::new()),
        }
    }

    fn resolve(&self) -> Option<Arc<LfsObjectStore>> {
        if let Some(store) = &self.eager {
            return Some(Arc::clone(store));
        }

        let loader = self.loader.as_ref()?;
        self.resolved.get_or_init(|| loader()).clone()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FilterWorktreePaths {
    current_worktree_root: PathBuf,
    shared_staging_root: PathBuf,
}

#[derive(Debug)]
enum SmudgeOutput {
    Bytes(Bytes),
    LfsFile {
        path: PathBuf,
        oid: [u8; 32],
        size: u64,
    },
    TemporaryFile(tempfile::TempPath),
}

#[derive(Debug)]
enum SmudgeInput {
    PointerCandidate(Vec<u8>),
    PassthroughFile(tempfile::TempPath),
}

/// How long the filter process will wait for the next command before
/// assuming git has exited without closing stdin (SIGKILL, crash, IDE
/// integration dropping the pipe) and shutting down cleanly.
///
/// Git dispatches filter commands back-to-back; a real session never
/// sees an inter-command gap this large. Without it, a blocking
/// `read_exact` on stdin parks the OS thread forever when the pipe's
/// write end stays open after git is gone — the orphaned filter process
/// keeps holding the staging flock, which then stalls every subsequent
/// `git add`/`crab add` for the full lock budget. See `read_ready`.
#[cfg(unix)]
pub const FILTER_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Runs the long-running filter protocol v2 loop.
///
/// Reads packet-line framed commands from `input` and writes responses to
/// `output`. Each clean/smudge operation is dispatched to the provided
/// `handler`. Session state (bloom filter, chunk buffer) is maintained
/// across operations within a single invocation.
///
/// When `lfs_store` is provided, smudge operations can download LFS objects
/// for non-lazy mode. Pass `None` when LFS support is not configured.
///
/// When `prefetch` is provided, smudge commands that arrive with
/// `can-delay=1` are queued for background reconstruction via the
/// [`PrefetchQueue`]. Pass `None` to disable delayed-smudge handling
/// (e.g. when no remote is configured) — the filter falls back to the
/// inline smudge path for every request.
///
/// When `hydrator` is provided, non-lazy smudge of crab pointers
/// reconstructs the file inline via the shard-based hydration pipeline.
/// Pass `None` when no remote is configured — the filter falls back to
/// passing the pointer through unchanged.
///
/// Uses `spawn_blocking` internally since `gix-packetline` uses blocking I/O.
pub async fn run_filter_process<R, W>(
    input: R,
    output: W,
    ctx: AppContext,
    lfs_store: Option<Arc<LfsObjectStore>>,
    prefetch: Option<Arc<PrefetchQueue>>,
    hydrator: Option<Arc<crate::cmd::hydrate::HydrationRuntime>>,
    #[cfg(unix)] idle: Option<(std::os::fd::RawFd, std::time::Duration)>,
) -> Result<()>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    run_filter_process_with_lfs_loader(
        input,
        output,
        ctx,
        lfs_store,
        None,
        prefetch,
        hydrator,
        #[cfg(unix)]
        idle,
    )
    .await
}

/// Runs the filter protocol with an optional lazy LFS store resolver.
///
/// The resolver is called only after a non-lazy LFS smudge misses the local
/// cache. This keeps clean and cache-hit smudge operations independent of
/// remote configuration while preserving the existing eager-store API.
pub async fn run_filter_process_with_lfs_loader<R, W>(
    input: R,
    output: W,
    ctx: AppContext,
    lfs_store: Option<Arc<LfsObjectStore>>,
    lfs_store_loader: Option<LfsStoreLoader>,
    prefetch: Option<Arc<PrefetchQueue>>,
    hydrator: Option<Arc<crate::cmd::hydrate::HydrationRuntime>>,
    #[cfg(unix)] idle: Option<(std::os::fd::RawFd, std::time::Duration)>,
) -> Result<()>
where
    R: Read + Send + 'static,
    W: Write + Send + 'static,
{
    // Do NOT open the staging area here. Staging is only needed by
    // the `clean` command; deferring the open until the first clean
    // arrives means `git status` — which only drives smudge commands
    // through the filter — never acquires `LOCK_EX` on the staging
    // root and doesn't block on a concurrent `crab add`. The
    // blocking open still happens for sessions that actually do stage,
    // just one level deeper inside the dispatch loop.
    let staging_root = resolve_staging_root();

    let handle = tokio::runtime::Handle::current();

    // Lazy-open cell shared between the outer task (for final flush)
    // and the blocking loop. `std::sync::Mutex` is fine because the
    // cell is only inspected from synchronous code inside
    // `spawn_blocking`; the lock is never held across `.await`.
    let staging_cell: Arc<std::sync::Mutex<LazyStaging>> =
        Arc::new(std::sync::Mutex::new(LazyStaging::from_root(staging_root)));
    let lfs_store = LfsStoreSource::new(lfs_store, lfs_store_loader);

    // Lazily-initialized speculation state. Initialized on the first
    // smudge when `hydrate.speculative = true`; `None` otherwise.
    let speculation_cell: Arc<std::sync::Mutex<Option<Arc<SpeculationState>>>> =
        Arc::new(std::sync::Mutex::new(None));

    // Pre-initialize speculation if enabled so the first smudge doesn't
    // pay the SQLite open cost on the hot path.
    if ctx.config().hydrate.speculative {
        let concurrency = ctx.config().hydrate.speculative_concurrency;
        if let Some(state) = init_speculation(
            concurrency,
            hydrator.clone(),
            Some(Arc::clone(ctx.metrics_arc())),
        )
        .await
        {
            let mut guard = speculation_cell
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *guard = Some(Arc::new(state));
        }
    }

    let prefetch_clone = prefetch.clone();
    let hydrator_clone = hydrator.clone();
    let staging_cell_clone = staging_cell.clone();
    let speculation_cell_clone = speculation_cell.clone();
    let handle_clone = handle.clone();
    let result = tokio::task::spawn_blocking(move || {
        #[cfg(unix)]
        let input: Box<dyn Read + Send> = match idle {
            Some((fd, timeout)) => Box::new(IdleRead::new(input, fd, timeout)),
            None => Box::new(input),
        };
        #[cfg(not(unix))]
        let input: Box<dyn Read + Send> = Box::new(input);

        let input = BufReader::with_capacity(256 * 1024, input);
        let mut output = BufWriter::with_capacity(256 * 1024, output);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_filter_loop_with_lfs_source(
                input,
                &mut output,
                ctx,
                staging_cell_clone,
                lfs_store,
                prefetch_clone,
                hydrator_clone,
                Some(handle_clone),
                speculation_cell_clone,
            )
        }));
        // Every complete response is explicitly flushed. On failure, discard
        // pending bytes so BufWriter's destructor cannot retry a broken frame.
        let _ = output.into_parts();
        result.unwrap_or_else(|panic_info| {
            Err(CrabError::Internal(format!(
                "filter session panicked: {}",
                panic_payload_to_string(&panic_info)
            )))
        })
    })
    .await
    .map_err(|e| CrabError::Internal(format!("filter process task panicked: {e}")))
    .and_then(|result| result);

    // Drain the prefetch queue so no background reconstructors outlive
    // the filter session. This fires the shared cancellation token and
    // joins every remaining task.
    if let Some(pf) = prefetch {
        pf.drain_for_shutdown().await;
    }

    // Final flush: only meaningful if the session actually opened the
    // staging area (at least one `clean` command arrived). Sessions
    // that only smudged never touched staging and there's nothing to
    // flush.
    let final_staging = {
        let mut cell = staging_cell
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::mem::replace(&mut *cell, LazyStaging::Unavailable)
    };
    if let LazyStaging::Writer(sa) = final_staging {
        match Arc::try_unwrap(sa) {
            Ok(s) => {
                if let Err(e) = s.close().await {
                    tracing::warn!(error = %e, "failed to close staging area cleanly");
                }
            }
            Err(arc) => {
                // Another Arc<StagingArea> clone still exists — typically a
                // StagingChunkStager attached to the CleanSession, or a
                // background task that outlived the loop. Those clones keep
                // the staging flock alive until they drop, so log the strong
                // count to make leaks diagnosable. The idle-timeout guard in
                // the loop ensures this process itself always exits, which
                // releases any leaked flock via fd close.
                tracing::warn!(
                    strong_count = Arc::strong_count(&arc),
                    "staging area still referenced; flock held until clones drop"
                );
            }
        }
    }

    result
}

/// State of the filter session's staging area, opened lazily.
///
/// The filter session starts in [`Unopened`] when a staging root can be
/// resolved, even if the directory has not been created yet. The first
/// `clean` command creates/opens it and transitions to [`Writer`] by
/// acquiring `LOCK_EX`; if that acquisition fails after the blocking
/// budget, the cell transitions to [`Locked`] so subsequent cleans
/// surface the same holder PID without retrying the flock.
///
/// Smudge commands never touch this cell, so a `git status` that only
/// drives smudges never acquires a lock and can run concurrently with
/// `crab add`.
///
/// [`Unopened`]: LazyStaging::Unopened
/// [`Unavailable`]: LazyStaging::Unavailable
/// [`Writer`]: LazyStaging::Writer
/// [`Locked`]: LazyStaging::Locked
enum LazyStaging {
    /// Staging root has been resolved and has not been opened yet.
    /// First `clean` dispatch calls [`StagingArea::open_blocking_default`]
    /// and transitions the cell into one of the terminal variants below.
    Unopened { staging_root: PathBuf },
    /// Writable handle held for the rest of the session.
    Writer(Arc<StagingArea>),
    /// Lock was held by another process for the full blocking budget.
    /// Cached so repeated cleans in the same session don't each wait
    /// out their own `FLOCK_BLOCKING_DEFAULT_BUDGET`.
    Locked { holder_pid: Option<u32> },
    /// Either `.crab` couldn't be resolved or an `open` error other
    /// than `StagingLocked` surfaced. Clean refuses with `CRAB-E0081`;
    /// smudge continues.
    Unavailable,
}

impl LazyStaging {
    /// Build the initial cell state from a resolved staging root.
    fn from_root(root: Option<PathBuf>) -> Self {
        match root {
            Some(path) => Self::Unopened { staging_root: path },
            _ => Self::Unavailable,
        }
    }
}

/// Outcome of acquiring a writer handle on the first `clean`.
///
/// Mirrors the transitions out of [`LazyStaging::Unopened`]; the caller
/// uses the outcome to either attach a real stager to the clean
/// session or mark the session staging-unavailable.
enum StagingAcquire {
    /// Writable handle, ready to stage chunks.
    Writer(Arc<StagingArea>),
    /// Lock held elsewhere after the full wait budget.
    Locked { holder_pid: Option<u32> },
    /// Staging root doesn't exist or open failed for a non-lock reason.
    Unavailable,
}

// ---------------------------------------------------------------------------
// Speculative hydration session state
// ---------------------------------------------------------------------------

/// Lazily-initialized speculation system for the filter-process session.
///
/// Created once on the first smudge when `hydrate.speculative = true` and
/// reused for the rest of the session. Initialization failures are
/// swallowed — speculation bugs must never break smudge.
struct SpeculationState {
    access_db: AsyncAccessDb,
    driver: Arc<SpeculativeDriver>,
    run_id: String,
    /// Handle for the background roll-up task; aborted on drop.
    _rollup_handle: tokio::task::JoinHandle<()>,
    /// Handle for the daily decay task; aborted on drop.
    _decay_handle: tokio::task::JoinHandle<()>,
}

/// Lazily initialize the speculation system.
///
/// Opens the current worktree's access DB, creates a predictor and driver,
/// and spawns the background roll-up task. Returns `None` on any error —
/// speculation is strictly best-effort.
async fn init_speculation(
    concurrency: usize,
    hydrator: Option<Arc<crate::cmd::hydrate::HydrationRuntime>>,
    metrics: Option<Arc<crate::core::metrics::Metrics>>,
) -> Option<SpeculationState> {
    let worktree_ctx = crate::git::worktree::WorktreeContext::resolve().ok()?;
    let repo_root = worktree_ctx.current_worktree_root.clone();
    let db_path = crate::speculation::access_db::path_for_context(&worktree_ctx);
    if let Some(parent) = db_path.parent()
        && let Err(e) = std::fs::create_dir_all(parent)
    {
        tracing::warn!(
            path = %parent.display(),
            error = %e,
            "speculation: failed to create per-worktree state directory"
        );
        return None;
    }

    let access_db = match AsyncAccessDb::open(db_path).await {
        Ok(db) => db,
        Err(e) => {
            tracing::warn!(error = %e, "speculation: failed to open access DB");
            return None;
        }
    };

    let predictor = Arc::new(Predictor::new(
        access_db.clone(),
        crate::speculation::predictor::DEFAULT_WINDOW_MS,
        crate::speculation::predictor::DEFAULT_TOP_K,
        crate::speculation::predictor::DEFAULT_MIN_COUNT,
    ));

    let rollup_handle = Arc::clone(&predictor)
        .spawn_background_rollup(crate::speculation::predictor::DEFAULT_DEBOUNCE_MS);

    // Build the hydrate callback. When a ShardHydrator is available,
    // use it for real reconstruction; otherwise the callback is a no-op
    // (speculation still records co-access for future sessions).
    let hydrate_fn: Arc<dyn crate::speculation::driver::HydrateFn> = if let Some(h) = hydrator {
        Arc::new(move |path: String| {
            let h = Arc::clone(&h);
            Box::pin(async move { warm_pointer_cache(Path::new(&path), &h).await })
                as std::pin::Pin<
                    Box<dyn std::future::Future<Output = crate::core::error::Result<()>> + Send>,
                >
        })
    } else {
        Arc::new(|_path: String| {
            Box::pin(async { Ok(()) })
                as std::pin::Pin<
                    Box<dyn std::future::Future<Output = crate::core::error::Result<()>> + Send>,
                >
        })
    };

    // The is_hydrated check reads the first bytes of the file on disk
    // to see if it's still a pointer (dehydrated) or real content.
    let repo_root_owned = repo_root;
    let is_hydrated_fn: Arc<dyn Fn(&str) -> bool + Send + Sync> =
        Arc::new(move |rel_path: &str| {
            let full = repo_root_owned.join(rel_path);
            path_is_hydrated(&full)
        });

    // Cache-pressure callback: when the chunk cache is ≥80% full,
    // speculation pauses to avoid evicting useful cached data.
    //
    // TODO(#cache-pressure): Wire to real ChunkCache usage metrics once
    // xet-core's DiskCache exposes a cheap current-size query. For now
    // this always reports no pressure so speculation runs unrestricted.
    let cache_pressure_fn: Option<Arc<dyn Fn() -> bool + Send + Sync>> = Some(Arc::new(|| false));

    let driver = if let Some(m) = metrics {
        Arc::new(SpeculativeDriver::with_metrics(
            predictor,
            concurrency,
            hydrate_fn,
            is_hydrated_fn,
            cache_pressure_fn,
            m,
        ))
    } else {
        Arc::new(SpeculativeDriver::new(
            predictor,
            concurrency,
            hydrate_fn,
            is_hydrated_fn,
            cache_pressure_fn,
        ))
    };

    let run_id = uuid::Uuid::now_v7().to_string();

    // One-time startup decay: remove entries older than 30 days.
    match access_db.decay(SPECULATION_DECAY_WINDOW_MS).await {
        Ok(removed) if removed > 0 => {
            tracing::info!(removed, "speculation: startup decay removed old entries");
        }
        Ok(_) => {
            tracing::debug!("speculation: startup decay found nothing to remove");
        }
        Err(e) => {
            tracing::warn!(error = %e, "speculation: startup decay failed");
        }
    }

    let decay_handle = spawn_daily_decay(access_db.clone());

    tracing::info!(run_id = %run_id, concurrency, "speculation system initialized");

    Some(SpeculationState {
        access_db,
        driver,
        run_id,
        _rollup_handle: rollup_handle,
        _decay_handle: decay_handle,
    })
}

async fn warm_pointer_cache(
    path: &Path,
    hydrator: &crate::cmd::hydrate::HydrationRuntime,
) -> Result<()> {
    // The file may have been hydrated since prediction; never collect its
    // payload merely to decide whether it is a pointer.
    let file = tokio::fs::File::open(path).await?;
    let mut reader = tokio::io::AsyncReadExt::take(file, MAX_POINTER_SIZE as u64 + 1);
    let mut content = Vec::new();
    tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut content).await?;
    let pointer = Pointer::parse(&content)?;
    // Warming needs verified cache fills, not a retained whole file.
    hydrator.reconstruct_to_writer(&pointer, io::sink()).await?;
    Ok(())
}

fn path_is_hydrated(path: &Path) -> bool {
    // classify rejects this many bytes, so a bounded prefix preserves its
    // Crab/LFS size rules even if the worktree file is growing concurrently.
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let mut bytes = Vec::new();
    if file
        .take(crab_git::lfs_pointer::MAX_LFS_POINTER_SIZE as u64)
        .read_to_end(&mut bytes)
        .is_err()
    {
        return false;
    }
    matches!(classify(&bytes), PointerKind::NotAPointer)
}

/// Spawn a background task that runs decay once every 24 hours.
///
/// Errors are logged and retried on the next cycle — the task never
/// terminates on its own (it runs until the process exits or the
/// handle is aborted).
fn spawn_daily_decay(db: AsyncAccessDb) -> tokio::task::JoinHandle<()> {
    const TWENTY_FOUR_HOURS: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

    tracing::info!("spawning daily speculation decay task");

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(TWENTY_FOUR_HOURS);

        // The first tick completes immediately; consume it because
        // startup decay already ran in init_speculation.
        interval.tick().await;

        loop {
            interval.tick().await;

            match db.decay(SPECULATION_DECAY_WINDOW_MS).await {
                Ok(removed) if removed > 0 => {
                    tracing::info!(removed, "daily speculation decay removed old entries");
                }
                Ok(_) => {
                    tracing::debug!("daily speculation decay: nothing to remove");
                }
                Err(e) => {
                    tracing::warn!(error = %e, "daily speculation decay failed; will retry next cycle");
                }
            }
        }
    })
}

/// Resolve `.crab/staging` from the repo root, or `None` if the repo
/// can't be discovered. Captured once at session start so subsequent
/// lazy opens don't re-walk the filesystem on every clean.
fn filter_paths_from_context(ctx: &WorktreeContext) -> FilterWorktreePaths {
    FilterWorktreePaths {
        current_worktree_root: ctx.current_worktree_root.clone(),
        shared_staging_root: ctx.shared_staging_dir(),
    }
}

fn resolve_filter_worktree_paths() -> Option<FilterWorktreePaths> {
    WorktreeContext::resolve()
        .ok()
        .map(|ctx| filter_paths_from_context(&ctx))
}

fn resolve_staging_root() -> Option<PathBuf> {
    resolve_filter_worktree_paths().map(|paths| paths.shared_staging_root)
}

fn resolve_current_worktree_root() -> Option<PathBuf> {
    resolve_filter_worktree_paths().map(|paths| paths.current_worktree_root)
}

/// Transition the lazy cell to a writer, or to a terminal failure
/// variant. Idempotent: once the cell reaches a terminal variant it
/// stays there for the rest of the session.
///
/// Uses the blocking-default flock budget so concurrent clean filters
/// and `crab add` invocations queue rather than erroring out on the
/// first collision. When the budget expires, the `Locked` variant is
/// cached and the error surfaces to git as `CRAB-E0081`.
async fn acquire_writer(cell: &std::sync::Mutex<LazyStaging>) -> StagingAcquire {
    // Phase 1: inspect current state without holding the lock across
    // `.await`. Short-circuit if already terminal.
    let staging_root = {
        let guard = cell
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match &*guard {
            LazyStaging::Writer(sa) => return StagingAcquire::Writer(sa.clone()),
            LazyStaging::Locked { holder_pid } => {
                return StagingAcquire::Locked {
                    holder_pid: *holder_pid,
                };
            }
            LazyStaging::Unavailable => return StagingAcquire::Unavailable,
            LazyStaging::Unopened { staging_root } => staging_root.clone(),
        }
    };

    // Phase 2: open the staging area with the shared blocking budget.
    // A clean command is writing staged chunks, so ordinary `git add`
    // must queue behind concurrent `crab add` / clean-filter writers
    // instead of failing after a filter-specific short timeout. Smudge
    // and cache-hit clean paths never reach this branch.
    let open_result = StagingArea::open_blocking_default(staging_root).await;

    // Phase 3: install the outcome. Filter dispatch is serial today so
    // a concurrent acquire isn't expected, but the Mutex re-check is
    // cheap insurance against future refactors.
    let mut guard = cell
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match &*guard {
        LazyStaging::Writer(sa) => return StagingAcquire::Writer(sa.clone()),
        LazyStaging::Locked { holder_pid } => {
            return StagingAcquire::Locked {
                holder_pid: *holder_pid,
            };
        }
        LazyStaging::Unavailable => return StagingAcquire::Unavailable,
        LazyStaging::Unopened { .. } => {}
    }

    match open_result {
        Ok(sa) => {
            let arc = Arc::new(sa);
            *guard = LazyStaging::Writer(arc.clone());
            StagingAcquire::Writer(arc)
        }
        Err(crab_staging::StagingError::StagingLocked { holder_pid }) => {
            tracing::warn!(
                ?holder_pid,
                "staging area is locked after waiting for the default budget; \
                 clean operations will fail with E0081"
            );
            *guard = LazyStaging::Locked { holder_pid };
            StagingAcquire::Locked { holder_pid }
        }
        Err(e) => {
            tracing::warn!(error = %e, "could not open staging area");
            *guard = LazyStaging::Unavailable;
            StagingAcquire::Unavailable
        }
    }
}

/// Filter command parsed from the protocol stream.
#[derive(Debug, Clone)]
struct FilterCommand {
    command: FilterOperation,
    pathname: String,
    /// Additional key=value metadata from the command header. Used to
    /// observe the `can-delay=1` capability that git sets on smudge
    /// commands eligible for delayed processing.
    metadata: HashMap<String, String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FilterOperation {
    Clean,
    Smudge,
    ListAvailableBlobs,
}

impl FilterCommand {
    /// Returns true when git sent `can-delay=1` on this command,
    /// indicating the filter may respond "delayed" and retrieve the
    /// real content later via `list_available_blobs`.
    fn can_delay(&self) -> bool {
        matches!(
            self.metadata.get("can-delay").map(String::as_str),
            Some("1")
        )
    }
}

/// Blocking filter protocol loop.
///
/// Performs the v2 handshake, then enters the command loop. Each operation
/// is isolated via `catch_unwind` so a panic in one clean/smudge does not
/// tear down the session.
#[allow(clippy::needless_pass_by_value)]
#[allow(clippy::too_many_arguments)]
#[cfg(test)]
fn run_filter_loop<R: BufRead, W: Write>(
    input: R,
    output: W,
    ctx: AppContext,
    staging_cell: Arc<std::sync::Mutex<LazyStaging>>,
    lfs_store: Option<Arc<LfsObjectStore>>,
    prefetch: Option<Arc<PrefetchQueue>>,
    hydrator: Option<Arc<crate::cmd::hydrate::HydrationRuntime>>,
    handle: Option<tokio::runtime::Handle>,
    speculation: Arc<std::sync::Mutex<Option<Arc<SpeculationState>>>>,
) -> Result<()> {
    run_filter_loop_with_lfs_source(
        input,
        output,
        ctx,
        staging_cell,
        LfsStoreSource::eager(lfs_store),
        prefetch,
        hydrator,
        handle,
        speculation,
    )
}

#[allow(clippy::needless_pass_by_value)]
#[allow(clippy::too_many_arguments)]
fn run_filter_loop_with_lfs_source<R: BufRead, W: Write>(
    mut input: R,
    mut output: W,
    ctx: AppContext,
    staging_cell: Arc<std::sync::Mutex<LazyStaging>>,
    lfs_store: LfsStoreSource,
    prefetch: Option<Arc<PrefetchQueue>>,
    hydrator: Option<Arc<crate::cmd::hydrate::HydrationRuntime>>,
    handle: Option<tokio::runtime::Handle>,
    speculation: Arc<std::sync::Mutex<Option<Arc<SpeculationState>>>>,
) -> Result<()> {
    handshake(&mut input, &mut output)?;

    // Start with a no-op stager. The first `clean` command lazily
    // opens the staging area (LOCK_EX) and swaps in a real
    // `StagingChunkStager` via `CleanSession::set_chunk_stager`.
    // Sessions that only receive smudge commands never touch staging
    // and never acquire `LOCK_EX` — that's what lets `git status` run
    // concurrently with `crab add`.
    let mut session = super::clean::CleanSession::new(ctx.clone());

    // Seed the session's staging-unavailable flag from the cell's
    // current state so a `clean` that arrives before any open attempt
    // still surfaces a truthful error in the edge cases (missing
    // `.crab/staging` or pre-cached Locked). Successful lazy open
    // later clears the flag via `set_chunk_stager`; a terminal failure
    // re-asserts it via `set_staging_locked` / `set_staging_unavailable`.
    {
        let cell = staging_cell
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match &*cell {
            LazyStaging::Unopened { .. } | LazyStaging::Writer(_) => {
                // Optimistic: no open attempted yet (Unopened) or already
                // writable (Writer from a previous clean in this session).
                // Leave the flag unset so a clean can proceed normally.
            }
            LazyStaging::Locked { holder_pid } => {
                session.set_staging_locked(*holder_pid);
            }
            LazyStaging::Unavailable => {
                session.set_staging_unavailable();
            }
        }
    }

    // Load persisted bloom filter from a previous session so the fast
    // path is effective immediately (no cold-start penalty).
    session.load_bloom_from_cache();

    let mut file_index_checker_attempted = false;

    // Configure LFS support: set the current worktree root for
    // .gitattributes lookup and LFS store for content staging.
    if let Some(root) = resolve_current_worktree_root() {
        session.set_repo_root(root);
    }
    loop {
        // Check for cancellation between operations.
        ctx.check_cancelled()?;

        let Some(cmd) = read_command(&mut input)? else {
            break;
        };

        let is_lfs_clean = cmd.command == FilterOperation::Clean
            && session.resolve_filter_for(&cmd.pathname)
                == Some(crate::git::filter_attr_cache::FilterKind::Lfs);
        if cmd.command == FilterOperation::Clean && !is_lfs_clean && !file_index_checker_attempted {
            file_index_checker_attempted = true;
            if let Some(handle) = handle.as_ref() {
                install_clean_file_index_checker(&mut session, &ctx, handle);
            }
        }

        // Dispatch and recovery share the content boundary. A fresh reader
        // during recovery would consume the next request after a late failure.
        let mut content = PktLineReader::from_read(&mut input);
        let mut response = FilterResponse::new(&mut output);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            dispatch_command(
                &cmd,
                &mut content,
                &mut response,
                &mut session,
                &ctx,
                &staging_cell,
                &lfs_store,
                prefetch.as_ref(),
                hydrator.as_ref(),
                handle.as_ref(),
                &speculation,
            )
        }));

        let error = match result {
            Ok(Ok(())) => continue,
            Ok(Err(e)) => {
                tracing::error!(
                    command = ?cmd.command,
                    path = %cmd.pathname,
                    error = %e,
                    "filter operation failed"
                );
                e
            }
            Err(panic_info) => {
                let msg = panic_payload_to_string(&panic_info);
                tracing::error!(
                    command = ?cmd.command,
                    path = %cmd.pathname,
                    panic = %msg,
                    "filter operation panicked"
                );
                CrabError::Internal(format!("filter operation panicked: {msg}"))
            }
        };
        session.reset_transient_state();
        if matches!(response.state, ResponseState::Complete) {
            continue;
        }
        // A partial response cannot be replaced with a new status list: Git
        // would parse that status as content or as the remainder of a packet.
        if matches!(response.state, ResponseState::Started)
            || matches!(content.state, ContentState::Failed)
        {
            return Err(error);
        }
        // Git requires the content flush before an error response. Delayed
        // blob-list requests have no body; malformed content ends the session
        // because its next packet boundary cannot be recovered safely.
        if matches!(
            cmd.command,
            FilterOperation::Clean | FilterOperation::Smudge
        ) {
            while content.read_packet()?.is_some() {}
        }
        write_status(&mut output, "error")?;
        write_flush(&mut output)?;
        output.flush().map_err(CrabError::Io)?;
    }

    // Persist the bloom filter so the next filter-process session
    // starts with a warm fast-path.
    session.save_bloom_to_cache();

    Ok(())
}

fn install_clean_file_index_checker(
    session: &mut super::clean::CleanSession,
    ctx: &AppContext,
    handle: &tokio::runtime::Handle,
) {
    let Some(remote_url) = ctx.config().remote_url.as_deref() else {
        return;
    };
    let parsed = match crate::git::url::CrabUrl::parse(remote_url) {
        Ok(parsed) => parsed,
        Err(e) => {
            tracing::debug!(
                remote_url,
                error = %e,
                "filter clean: remote file-index fast path disabled; remote URL is not crab://"
            );
            return;
        }
    };

    let cancel = ctx.cancel_token();
    let selection = match handle.block_on(
        crate::replication::StoreResolver::new(ctx.config(), &parsed, &cancel)
            .read_store("filter-clean-file-index"),
    ) {
        Ok(selection) => selection,
        Err(e) => {
            tracing::warn!(
                remote_url,
                error = %e,
                "filter clean: remote file-index fast path disabled; failed to build read store"
            );
            return;
        }
    };

    let shard_hint_scope = crate::cache::shard_hints::ShardHintScope::new(
        &selection.store.bucket_identity(),
        selection.router.global_prefix(),
    );

    let router = file_index_checker_router(
        selection.store,
        selection.router.repo_prefix().to_owned(),
        ctx,
        handle,
    );
    session.load_shard_hints_from_cache(&shard_hint_scope);
    session.set_file_index_checker(Box::new(super::clean::StoreFileIndexChecker::new(
        router,
        handle.clone(),
    )));
}

fn file_index_checker_router(
    store: crate::storage::store::Store,
    repo_prefix: String,
    ctx: &AppContext,
    handle: &tokio::runtime::Handle,
) -> crate::storage::StoreLayout {
    let scope = store.storage_scope().cloned();
    let identity = store.bucket_identity();
    let checker_store = match handle.block_on(crab_cache_store::CachingStore::try_build_healthy(
        store.as_storage().clone(),
        &ctx.config().cache,
    )) {
        Some(cache) => {
            let mut wrapped = crate::storage::store::Store::new(cache.object_store())
                .with_bucket_identity(identity);
            if let Some(scope) = scope {
                wrapped = wrapped.with_storage_scope(scope);
            }
            wrapped
        }
        None => store,
    };

    crate::storage::StoreLayout::new(checker_store, repo_prefix)
}

/// Perform the git filter protocol v2 handshake.
///
/// Expects:
///   client: "git-filter-client\n" "version=2\n" flush
///   server: "git-filter-server\n" "version=2\n" flush
///   client: "capability=clean\n" ... flush
///   server: "capability=clean\n" "capability=smudge\n" "capability=delay\n" flush
fn handshake<R: Read, W: Write>(input: &mut R, output: &mut W) -> Result<()> {
    // Read client welcome.
    let welcome = read_text_line(input)?
        .ok_or_else(|| CrabError::Protocol("expected git-filter-client".into()))?;
    if welcome != GIT_FILTER_CLIENT {
        return Err(CrabError::Protocol(format!(
            "expected '{GIT_FILTER_CLIENT}', got '{welcome}'"
        )));
    }

    let version =
        read_text_line(input)?.ok_or_else(|| CrabError::Protocol("expected version=2".into()))?;
    if version != FILTER_PROTOCOL_VERSION {
        return Err(CrabError::Protocol(format!(
            "expected '{FILTER_PROTOCOL_VERSION}', got '{version}'"
        )));
    }

    // Consume flush after version.
    let flush_check = read_text_line(input)?;
    if flush_check.is_some() {
        return Err(CrabError::Protocol("expected flush after version".into()));
    }

    // Send server welcome.
    write_text_line(output, "git-filter-server")?;
    write_text_line(output, FILTER_PROTOCOL_VERSION)?;
    write_flush(output)?;
    // Flush immediately — git blocks until it receives our welcome
    // before sending capabilities. Without this flush, the bytes sit
    // in the BufWriter and we deadlock.
    output.flush().map_err(CrabError::Io)?;

    // Read client capabilities until flush.
    let mut client_caps = Vec::new();
    while let Some(line) = read_text_line(input)? {
        if let Some(cap) = line.strip_prefix("capability=") {
            client_caps.push(cap.to_owned());
        }
    }

    tracing::debug!(?client_caps, "client capabilities received");

    // Advertise our capabilities.
    for cap in CAPABILITIES {
        write_text_line(output, &format!("capability={cap}"))?;
    }
    write_flush(output)?;

    output.flush().map_err(CrabError::Io)?;

    Ok(())
}

/// Read a single filter command header (command + pathname + metadata + flush).
///
/// Returns `None` on EOF (git closed stdin).
fn read_command<R: BufRead>(input: &mut R) -> Result<Option<FilterCommand>> {
    // EOF is normal only between requests. Once any header byte arrives,
    // a missing packet or flush is a terminal framing error, not completion.
    if input.fill_buf().map_err(CrabError::Io)?.is_empty() {
        return Ok(None);
    }
    let command_line = read_text_line(input)?
        .ok_or_else(|| CrabError::Protocol("expected filter command, got flush".to_owned()))?;

    // Unknown operations have no agreed content shape. Reject them before
    // dispatch instead of acknowledging or guessing how to drain their body.
    let command = match command_line.strip_prefix("command=") {
        Some("clean") => FilterOperation::Clean,
        Some("smudge") => FilterOperation::Smudge,
        Some("list_available_blobs") => FilterOperation::ListAvailableBlobs,
        _ => {
            return Err(CrabError::Protocol(format!(
                "unsupported filter command: {command_line}"
            )));
        }
    };

    // Read remaining key=value pairs until flush.
    let mut pathname = String::new();
    let mut metadata = HashMap::new();

    while let Some(line) = read_text_line(input)? {
        if let Some(path) = line.strip_prefix("pathname=") {
            path.clone_into(&mut pathname);
        } else if let Some((k, v)) = line.split_once('=') {
            metadata.insert(k.to_owned(), v.to_owned());
        }
    }
    if command != FilterOperation::ListAvailableBlobs && pathname.is_empty() {
        return Err(CrabError::Protocol(
            "filter content request has no pathname".to_owned(),
        ));
    }

    Ok(Some(FilterCommand {
        command,
        pathname,
        metadata,
    }))
}

/// Dispatch a parsed command to the appropriate handler.
///
/// When `prefetch` is attached and git sets `can-delay=1` on a smudge,
/// the request is queued for background reconstruction and the filter
/// immediately replies with `status=delayed`. On `list_available_blobs`
/// the queue is polled and completed pathnames are reported. Ordinary
/// smudge responses follow the standard content-then-flush protocol.
#[allow(clippy::too_many_arguments)]
fn dispatch_command<R: Read, W: Write>(
    cmd: &FilterCommand,
    input: &mut PktLineReader<R>,
    output: &mut FilterResponse<W>,
    session: &mut super::clean::CleanSession,
    ctx: &AppContext,
    staging_cell: &Arc<std::sync::Mutex<LazyStaging>>,
    lfs_store: &LfsStoreSource,
    prefetch: Option<&Arc<PrefetchQueue>>,
    hydrator: Option<&Arc<crate::cmd::hydrate::HydrationRuntime>>,
    handle: Option<&tokio::runtime::Handle>,
    speculation: &Arc<std::sync::Mutex<Option<Arc<SpeculationState>>>>,
) -> Result<()> {
    match cmd.command {
        FilterOperation::Clean => {
            // Resolve ownership before touching XET staging. LFS has its own
            // cache and remote publication path and must never contend on the
            // XET staging lock in a mixed-repository filter session.
            let is_lfs = session.resolve_filter_for(&cmd.pathname)
                == Some(crate::git::filter_attr_cache::FilterKind::Lfs);
            // Lazily acquire a writer handle on the first clean of the
            // session. Subsequent cleans reuse the cached handle
            // without re-flocking. Sessions that only smudge never
            // reach this branch and never acquire `LOCK_EX`, which is
            // what lets a concurrent `git status` coexist with a
            // long-running `crab add`.
            if is_lfs {
                tracing::debug!(
                    path = %cmd.pathname,
                    "filter clean: LFS route resolved, skipping XET staging"
                );
            } else if let Some(h) = handle {
                match h.block_on(acquire_writer(staging_cell.as_ref())) {
                    StagingAcquire::Writer(sa) => {
                        let stager = super::clean::StagingChunkStager::new(sa, h.clone());
                        session.set_chunk_stager(Box::new(stager));
                    }
                    StagingAcquire::Locked { holder_pid } => {
                        session.set_staging_locked(holder_pid);
                    }
                    StagingAcquire::Unavailable => {
                        session.set_staging_unavailable();
                    }
                }
            } else {
                // No runtime handle is only expected in unit tests
                // that drive `run_filter_loop` synchronously. Fall
                // back to the "unavailable" clean path so the session
                // surfaces the missing dependency instead of
                // producing an unbacked pointer.
                session.set_staging_unavailable();
            }

            // Stream pkt-line packets straight into the CDC chunker and
            // blake3 hasher, bounding peak memory to one packet
            // (≤64 KiB), the ≤1 KiB pointer probe, and the chunker's
            // internal window instead of the full file payload.
            let pointer_bytes = session.clean_stream(&cmd.pathname, input)?;

            output.content_response(|output| write_content(output, &pointer_bytes))?;
        }
        FilterOperation::Smudge => {
            let content = match read_smudge_input_until_flush(input)? {
                SmudgeInput::PointerCandidate(content) => content,
                SmudgeInput::PassthroughFile(path) => {
                    output.content_response(|output| write_content_file(output, &path))?;
                    return Ok(());
                }
            };
            let lazy = ctx.config().checkout.lazy;

            // Speculative hydration: on smudge of a dehydrated file
            // (crab pointer), fire-and-forget record the access event
            // and launch speculative pre-fetches for predicted neighbors.
            // Also check if this file was speculatively pre-hydrated in
            // a previous smudge — if so, record a speculation hit.
            // All errors are swallowed — speculation must never break smudge.
            if ctx.config().hydrate.speculative
                && let PointerKind::Crab(_) = classify(&content)
            {
                let spec_state = {
                    let guard = speculation
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    guard.clone()
                };
                if let (Some(state), Some(h)) = (spec_state, handle) {
                    let pathname = cmd.pathname.clone();
                    let ts_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as i64)
                        .unwrap_or(0);
                    let run_id = state.run_id.clone();

                    // Check if this file was speculatively pre-hydrated.
                    let driver_for_hit = Arc::clone(&state.driver);
                    let hit_path = pathname.clone();
                    h.block_on(async move {
                        driver_for_hit.record_hit_if_speculative(&hit_path).await;
                    });

                    // Fire-and-forget access recording.
                    state
                        .access_db
                        .record_access_fire_and_forget(pathname.clone(), ts_ms, run_id);

                    // Launch speculative hydrations for predicted neighbors.
                    let driver = Arc::clone(&state.driver);
                    h.block_on(async move {
                        driver.launch_speculative(&pathname).await;
                    });
                }
            }

            // Delayed-smudge fast path. When git advertises `can-delay=1`
            // and the filter has a prefetch queue wired up, parse the
            // crab pointer and kick off background reconstruction.
            // The filter replies `status=delayed` with no content; git
            // will come back later via `list_available_blobs` +
            // `command=smudge` (without `can-delay`) to collect the
            // reconstructed bytes.
            //
            // LFS pointers and non-pointer content do not benefit from
            // prefetch today — they fall through to the inline path.
            if cmd.can_delay()
                && let (Some(pf), Some(h)) = (prefetch, handle)
                && let PointerKind::Crab(pointer) = classify(&content)
            {
                let pathname = cmd.pathname.clone();
                let pf = pf.clone();
                h.block_on(async move {
                    pf.submit(pathname, pointer).await;
                });

                write_delayed_response(output)?;
                output.flush().map_err(CrabError::Io)?;
                return Ok(());
            }

            // Non-delayed path: if a pathname was previously delayed,
            // git will re-issue `command=smudge` without `can-delay=1`
            // to collect the queued content. In that case the content
            // bytes git sent are the same pointer bytes as before — we
            // just hand back the prefetched result.
            if let (Some(pf), Some(h)) = (prefetch, handle)
                && let Some(file) = h.block_on(async {
                    // Non-blocking peek: take_result only succeeds if
                    // the result is already materialized or the task is
                    // ready. A missing entry surfaces as NotFound and
                    // we fall through to the inline smudge path.
                    match pf.take_result(&cmd.pathname).await {
                        Ok(file) => Some(file),
                        Err(CrabError::NotFound { .. }) => None,
                        Err(e) => {
                            tracing::warn!(
                                path = %cmd.pathname,
                                error = %e,
                                "prefetched result unavailable, falling back to inline smudge"
                            );
                            None
                        }
                    }
                })
            {
                output.content_response(|output| write_content_file(output, file.path()))?;
                return Ok(());
            }

            let result = match try_stream_lfs_smudge(
                &content,
                &cmd.pathname,
                lfs_store,
                session,
                lazy,
                handle,
            )? {
                Some(output) => output,
                None => smudge_content(
                    &content,
                    &cmd.pathname,
                    lazy,
                    lfs_store,
                    session,
                    hydrator,
                    handle,
                )?,
            };

            output.content_response(|output| write_smudge_output(output, &result))?;
        }
        FilterOperation::ListAvailableBlobs => {
            // Git asks which delayed blobs are ready. Drain the queue's
            // completed list; each pathname is echoed back so git
            // retrieves those first.
            let ready: Vec<String> = if let (Some(pf), Some(h)) = (prefetch, handle) {
                h.block_on(async { pf.wait_completed().await })
            } else {
                Vec::new()
            };

            write_available_blobs_response(output, &ready)?;
            output.flush().map_err(CrabError::Io)?;
        }
    }
    Ok(())
}

fn write_available_blobs_response<W: Write>(output: &mut W, ready: &[String]) -> Result<()> {
    for path in ready {
        write_text_line(output, &format!("pathname={path}"))?;
    }
    write_flush(output)?;
    write_status(output, "success")?;
    write_flush(output)?;
    Ok(())
}

fn write_delayed_response<W: Write>(output: &mut W) -> Result<()> {
    write_status(output, "delayed")?;
    write_flush(output)?;
    Ok(())
}

/// Stream a common LFS smudge case directly from the verified cache file.
///
/// The packet-line response can be written incrementally, so ordinary
/// extension-free LFS objects do not need to be retained as a second full
/// payload in the filter process. Extension-bearing objects fall back to the
/// existing byte-oriented pipeline because extensions transform the content.
fn try_stream_lfs_smudge(
    content: &[u8],
    pathname: &str,
    lfs_store: &LfsStoreSource,
    session: &super::clean::CleanSession,
    lazy: bool,
    handle: Option<&tokio::runtime::Handle>,
) -> Result<Option<SmudgeOutput>> {
    if lazy || !session.should_lfs_smudge(pathname) {
        return Ok(None);
    }
    if matches!(
        session.resolve_filter_for(pathname),
        Some(crate::git::filter_attr_cache::FilterKind::Crab)
    ) {
        return Ok(None);
    }

    let PointerKind::Lfs(pointer) = classify(content) else {
        return Ok(None);
    };
    if !pointer.extensions.is_empty() {
        return Ok(None);
    }

    let lfs_dir = match session_lfs_storage_dir(session) {
        Ok(Some(path)) => path,
        Ok(None) | Err(_) => return Ok(None),
    };
    let local_path = crate::lfs::cache::object_path(&lfs_dir, &pointer.oid);
    if crate::lfs::cache::is_valid(&lfs_dir, &pointer.oid, pointer.size)? {
        return Ok(Some(SmudgeOutput::LfsFile {
            path: local_path,
            oid: pointer.oid,
            size: pointer.size,
        }));
    }

    let Some(store) = lfs_store.resolve() else {
        return if session.should_skip_lfs_download_errors() {
            Ok(Some(SmudgeOutput::Bytes(Bytes::copy_from_slice(content))))
        } else {
            Ok(None)
        };
    };

    let Some(handle) = handle else {
        return Ok(None);
    };
    let temp = crate::lfs::cache::new_temp_path(&lfs_dir)?;
    let temp_path = temp.to_path_buf();
    let result = handle.block_on(store.download_to_file(&pointer.oid, pointer.size, &temp_path));
    match result {
        Ok(()) => {
            let installed = crate::lfs::cache::install_verified_temp_path(
                &lfs_dir,
                &pointer.oid,
                pointer.size,
                temp,
            )?;
            Ok(Some(SmudgeOutput::LfsFile {
                path: installed,
                oid: pointer.oid,
                size: pointer.size,
            }))
        }
        Err(error) if session.should_skip_lfs_download_errors() => {
            tracing::warn!(
                oid = %crab_git::lfs_pointer::hex_encode(&pointer.oid),
                error = %error,
                "smudge: LFS download failed; preserving pointer because skipdownloaderrors is enabled"
            );
            Ok(Some(SmudgeOutput::Bytes(Bytes::copy_from_slice(content))))
        }
        Err(error) => Err(CrabError::from(error)),
    }
}

fn reconstruct_crab_to_temp(
    hydrator: &crate::cmd::hydrate::HydrationRuntime,
    handle: &tokio::runtime::Handle,
    pointer_bytes: &[u8],
) -> Result<tempfile::TempPath> {
    // Git has not consumed this output yet; it is operation state, not an
    // evictable cache entry. A disabled cache must not disable smudging.
    let path = tempfile::NamedTempFile::new()
        .map_err(CrabError::Io)?
        .into_temp_path();
    handle.block_on(hydrator.reconstruct_from_pointer_to_path(pointer_bytes, &path))?;
    Ok(path)
}

/// Classify incoming smudge content and return its bounded output source.
///
/// Dispatches based on pointer type:
/// - LFS pointer + lazy mode → pass through unchanged
/// - LFS pointer + non-lazy mode → download content from LFS object store
/// - Crab pointer + lazy mode → pass through for on-demand hydration
/// - Crab pointer + non-lazy mode → reconstruct from xorbs if store available
/// - Not a pointer → pass through unchanged
fn smudge_content(
    content: &[u8],
    pathname: &str,
    lazy: bool,
    lfs_store: &LfsStoreSource,
    session: &super::clean::CleanSession,
    hydrator: Option<&Arc<crate::cmd::hydrate::HydrationRuntime>>,
    handle: Option<&tokio::runtime::Handle>,
) -> Result<SmudgeOutput> {
    // Resolve filter from .gitattributes before blob classification.
    let resolved_filter = session.resolve_filter_for(pathname);

    match resolved_filter {
        Some(crate::git::filter_attr_cache::FilterKind::Lfs) => {
            // User explicitly chose LFS for this path. If the blob is already
            // an LFS pointer, smudge it. If it's a Crab pointer, warn and try
            // LFS smudge anyway (re-processing will happen on next clean).
            if lazy {
                tracing::debug!(path = %pathname, "smudge: LFS-filtered path in lazy mode, passing through");
                return Ok(SmudgeOutput::Bytes(Bytes::copy_from_slice(content)));
            }
            if !session.should_lfs_smudge(pathname) {
                tracing::debug!(path = %pathname, "smudge: LFS fetch filters excluded path, passing through");
                return Ok(SmudgeOutput::Bytes(Bytes::copy_from_slice(content)));
            }
            // Git LFS deliberately leaves empty tracked files as empty Git
            // blobs. They are not pointers and have no remote dependency.
            if content.is_empty() {
                return Ok(SmudgeOutput::Bytes(Bytes::new()));
            }
            // Try LFS smudge regardless of blob content type.
            if let Ok(ptr) = crab_git::lfs_pointer::LfsPointer::parse(content) {
                let oid_hex = crab_git::lfs_pointer::hex_encode(&ptr.oid);
                if let Some(local) = try_local_lfs_cache(&ptr, session)? {
                    tracing::debug!(oid = %oid_hex, "smudge: LFS-filtered path resolved from local cache");
                    let content = crate::lfs::extension::smudge_content(&ptr, local, pathname)?;
                    return Ok(SmudgeOutput::Bytes(Bytes::from(content)));
                }
                if let Some(store) = lfs_store.resolve() {
                    tracing::debug!(oid = %oid_hex, "smudge: downloading LFS object for LFS-filtered path");
                    let rt = tokio::runtime::Handle::current();
                    let bytes = match rt.block_on(store.verify(&ptr.oid)) {
                        Ok(bytes) => bytes,
                        Err(error) if session.should_skip_lfs_download_errors() => {
                            tracing::warn!(
                                oid = %oid_hex,
                                error = %error,
                                "smudge: LFS download failed; preserving pointer because skipdownloaderrors is enabled"
                            );
                            return Ok(SmudgeOutput::Bytes(Bytes::copy_from_slice(content)));
                        }
                        Err(error) => return Err(CrabError::from(error)),
                    };
                    cache_lfs_locally(&ptr, &bytes, session)?;
                    let content =
                        crate::lfs::extension::smudge_content(&ptr, bytes.to_vec(), pathname)?;
                    return Ok(SmudgeOutput::Bytes(Bytes::from(content)));
                }
                if session.should_skip_lfs_download_errors() {
                    tracing::warn!(
                        oid = %oid_hex,
                        "smudge: LFS object unavailable; preserving pointer because skipdownloaderrors is enabled"
                    );
                    return Ok(SmudgeOutput::Bytes(Bytes::copy_from_slice(content)));
                }
                tracing::warn!(oid = %oid_hex, "smudge: LFS object not available for LFS-filtered path");
                return Err(CrabError::Configuration {
                    key: "lfs remote".to_owned(),
                    origin: "non-lazy LFS smudge could not resolve a remote store".to_owned(),
                });
            }
            // Content isn't an LFS pointer — pass through.
            tracing::debug!(path = %pathname, "smudge: LFS-filtered path, content is not an LFS pointer, passing through");
            Ok(SmudgeOutput::Bytes(Bytes::copy_from_slice(content)))
        }
        Some(crate::git::filter_attr_cache::FilterKind::Crab) => {
            // User explicitly chose XET for this path. Try Crab reconstruction.
            if lazy && !session.should_auto_hydrate(pathname) {
                tracing::debug!(path = %pathname, "smudge: Crab-filtered path in lazy mode, passing through");
                return Ok(SmudgeOutput::Bytes(Bytes::copy_from_slice(content)));
            }
            if let Ok(pointer) = crab_types::pointer::Pointer::parse(content)
                && let (Some(h), Some(rt)) = (hydrator, handle)
            {
                tracing::debug!(
                    file_hash = %crab_types::pointer::hex_encode(&pointer.file_hash),
                    "smudge: reconstructing Crab-filtered path inline"
                );
                match reconstruct_crab_to_temp(h, rt, content) {
                    Ok(path) => return Ok(SmudgeOutput::TemporaryFile(path)),
                    Err(e) => {
                        tracing::warn!(
                            file_hash = %crab_types::pointer::hex_encode(&pointer.file_hash),
                            error = %e,
                            "smudge: inline reconstruction failed for Crab-filtered path"
                        );
                    }
                }
            }
            tracing::debug!(path = %pathname, "smudge: Crab-filtered path, passing through");
            Ok(SmudgeOutput::Bytes(Bytes::copy_from_slice(content)))
        }
        None => {
            // No .gitattributes filter — fall back to blob classification.
            smudge_by_blob_classification(
                content, pathname, lazy, lfs_store, session, hydrator, handle,
            )
        }
    }
}

/// Original smudge logic based on blob content classification.
/// Used as fallback when no .gitattributes filter matches.
fn smudge_by_blob_classification(
    content: &[u8],
    pathname: &str,
    lazy: bool,
    lfs_store: &LfsStoreSource,
    session: &super::clean::CleanSession,
    hydrator: Option<&Arc<crate::cmd::hydrate::HydrationRuntime>>,
    handle: Option<&tokio::runtime::Handle>,
) -> Result<SmudgeOutput> {
    match classify(content) {
        PointerKind::Lfs(pointer) => {
            if lazy {
                tracing::debug!(
                    oid = %crab_git::lfs_pointer::hex_encode(&pointer.oid),
                    "smudge: LFS pointer in lazy mode, passing through"
                );
                return Ok(SmudgeOutput::Bytes(Bytes::copy_from_slice(content)));
            }
            if !session.should_lfs_smudge(pathname) {
                tracing::debug!(path = %pathname, "smudge: LFS fetch filters excluded path, passing through");
                return Ok(SmudgeOutput::Bytes(Bytes::copy_from_slice(content)));
            }

            let oid_hex = crab_git::lfs_pointer::hex_encode(&pointer.oid);

            // Non-lazy: try local .git/lfs/objects/ cache first, then remote.
            if let Some(local) = try_local_lfs_cache(&pointer, session)? {
                tracing::debug!(oid = %oid_hex, "smudge: resolved from local LFS cache");
                let content = crate::lfs::extension::smudge_content(&pointer, local, pathname)?;
                return Ok(SmudgeOutput::Bytes(Bytes::from(content)));
            }

            // Fall back to remote store download.
            if let Some(store) = lfs_store.resolve() {
                tracing::debug!(
                    oid = %oid_hex,
                    size = pointer.size,
                    "smudge: downloading LFS object from remote"
                );
                let rt = tokio::runtime::Handle::current();
                let bytes = match rt.block_on(store.verify(&pointer.oid)) {
                    Ok(bytes) => bytes,
                    Err(error) if session.should_skip_lfs_download_errors() => {
                        tracing::warn!(
                            oid = %oid_hex,
                            error = %error,
                            "smudge: LFS download failed; preserving pointer because skipdownloaderrors is enabled"
                        );
                        return Ok(SmudgeOutput::Bytes(Bytes::copy_from_slice(content)));
                    }
                    Err(error) => return Err(CrabError::from(error)),
                };
                // Cache locally for future checkouts.
                cache_lfs_locally(&pointer, &bytes, session)?;
                let content =
                    crate::lfs::extension::smudge_content(&pointer, bytes.to_vec(), pathname)?;
                return Ok(SmudgeOutput::Bytes(Bytes::from(content)));
            }

            if session.should_skip_lfs_download_errors() {
                tracing::warn!(
                    oid = %oid_hex,
                    "smudge: LFS object unavailable; preserving pointer because skipdownloaderrors is enabled"
                );
                return Ok(SmudgeOutput::Bytes(Bytes::copy_from_slice(content)));
            }

            // Non-lazy smudge must not silently materialize a pointer when
            // the required remote is unavailable.
            tracing::warn!(
                oid = %oid_hex,
                "smudge: LFS object not in local cache and no remote store available"
            );
            Err(CrabError::Configuration {
                key: "lfs remote".to_owned(),
                origin: "non-lazy LFS smudge could not resolve a remote store".to_owned(),
            })
        }
        PointerKind::Crab(pointer) => {
            // Lazy mode: pass the pointer through for on-demand hydration,
            // UNLESS the pathname matches an auto-hydrate pattern
            // (`checkout.auto_hydrate_patterns` config). The previous
            // code passed the empty string here, making the match always
            // fail. See finding CR4-F2.
            if lazy && !session.should_auto_hydrate(pathname) {
                tracing::debug!(path = %pathname, "smudge: crab pointer in lazy mode, passing through");
                return Ok(SmudgeOutput::Bytes(Bytes::copy_from_slice(content)));
            }

            // Non-lazy or auto-hydrate: attempt inline reconstruction
            // via the ShardHydrator when available.
            if let (Some(h), Some(rt)) = (hydrator, handle) {
                tracing::debug!(
                    file_hash = %crab_types::pointer::hex_encode(&pointer.file_hash),
                    size = pointer.size,
                    "smudge: reconstructing crab pointer inline"
                );
                match reconstruct_crab_to_temp(h, rt, content) {
                    Ok(path) => return Ok(SmudgeOutput::TemporaryFile(path)),
                    Err(e) => {
                        tracing::warn!(
                            file_hash = %crab_types::pointer::hex_encode(&pointer.file_hash),
                            error = %e,
                            "smudge: inline reconstruction failed, deferring to hydrate command"
                        );
                    }
                }
            }

            // No hydrator available or reconstruction failed — pass
            // through. The user can run `crab hydrate` to reconstruct.
            tracing::debug!(
                file_hash = %crab_types::pointer::hex_encode(&pointer.file_hash),
                size = pointer.size,
                "smudge: crab pointer, deferring to hydrate command"
            );
            Ok(SmudgeOutput::Bytes(Bytes::copy_from_slice(content)))
        }
        PointerKind::NotAPointer => {
            // Not a recognized pointer — pass through unchanged.
            tracing::debug!("smudge: not a pointer, passing through");
            Ok(SmudgeOutput::Bytes(Bytes::copy_from_slice(content)))
        }
    }
}

// --- Packet-line I/O helpers ---
//
// These wrap `gix_packetline::blocking_io` for the specific patterns used
// in the filter protocol. The filter protocol uses text mode (lines end
// with \n) and flush packets as delimiters.

/// Poll a file descriptor for readability with a timeout.
///
/// Used to bound the blocking stdin read in the filter-process loop so a
/// git process that exits without closing stdin (SIGKILL, crash, IDE pipe
/// leak) does not leave the filter parked in `read_exact` forever holding
/// the staging flock. Returns `true` if the fd is readable (data or EOF
/// pending), `false` on timeout. Retries on `EINTR`.
#[cfg(unix)]
struct IdleRead<R> {
    inner: R,
    fd: std::os::fd::RawFd,
    timeout: std::time::Duration,
    timed_out: bool,
}

#[cfg(unix)]
impl<R> IdleRead<R> {
    fn new(inner: R, fd: std::os::fd::RawFd, timeout: std::time::Duration) -> Self {
        Self {
            inner,
            fd,
            timeout,
            timed_out: false,
        }
    }
}

#[cfg(unix)]
impl<R: Read> Read for IdleRead<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.timed_out {
            return Ok(0);
        }
        match read_ready(self.fd, self.timeout) {
            Ok(true) => self.inner.read(buf),
            Ok(false) => {
                tracing::info!(
                    timeout_secs = self.timeout.as_secs(),
                    "filter-process idle timeout; git likely exited, shutting down"
                );
                self.timed_out = true;
                Ok(0)
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "stdin poll failed; exiting filter-process"
                );
                self.timed_out = true;
                Ok(0)
            }
        }
    }
}

#[cfg(unix)]
fn read_ready(fd: std::os::fd::RawFd, timeout: std::time::Duration) -> io::Result<bool> {
    // SAFETY: `pollfd` is a plain struct; we pass exactly one with the fd
    // and POLLIN. `poll(2)` blocks the calling thread up to `timeout` and
    // is the standard way to bound a std read on a pipe. POLLIN covers both
    // "data available" and "peer closed" (read returns 0/EOF), so a real
    // EOF from git reports `true` immediately and the loop exits cleanly.
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        // SAFETY: `poll` is FFI-safe with the above struct; timeout is a
        // bounded `c_int` of milliseconds (FILTER_IDLE_TIMEOUT ≤ 60s).
        #[expect(
            clippy::cast_possible_truncation,
            reason = "timeout fits in c_int for any value ≤ ~24 days"
        )]
        let rc = unsafe {
            libc::poll(
                std::ptr::addr_of_mut!(pfd),
                1,
                timeout.as_millis() as libc::c_int,
            )
        };
        if rc < 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(err);
        }
        return Ok(rc > 0);
    }
}

/// Read a text line, stripping the trailing newline. Returns `None` on flush.
fn read_text_line<R: Read>(input: &mut R) -> Result<Option<String>> {
    let mut buf = [0u8; 4];
    input.read_exact(&mut buf).map_err(CrabError::Io)?;

    if &buf == b"0000" {
        return Ok(None);
    }

    let hex = std::str::from_utf8(&buf)
        .map_err(|_| CrabError::Protocol("invalid packet-line hex".into()))?;
    let len: usize = u16::from_str_radix(hex, 16)
        .map_err(|_| CrabError::Protocol(format!("invalid packet-line length: {hex}")))?
        .into();

    if len < 4 {
        return Err(CrabError::Protocol(format!(
            "packet-line length too small: {len}"
        )));
    }

    let data_len = len - 4;
    let mut data = vec![0u8; data_len];
    input.read_exact(&mut data).map_err(CrabError::Io)?;

    // Strip trailing newline if present.
    if data.last() == Some(&b'\n') {
        data.pop();
    }

    String::from_utf8(data)
        .map(Some)
        .map_err(|_| CrabError::Protocol("non-UTF-8 packet-line data".into()))
}

/// Read one complete smudge request without retaining a non-pointer body.
///
/// Git requires the request flush before the filter starts its response.
/// Direct LFS parsing accepts `MAX_LFS_POINTER_SIZE` bytes, so only after that
/// boundary is exceeded can the body be classified as passthrough and
/// spooled packet-by-packet to disk with bounded memory.
fn read_smudge_input_until_flush<R: Read>(reader: &mut PktLineReader<R>) -> Result<SmudgeInput> {
    let mut pointer_candidate = Vec::with_capacity(MAX_LFS_POINTER_SIZE);
    let mut passthrough: Option<tempfile::NamedTempFile> = None;
    while let Some(packet) = reader.read_packet()? {
        if let Some(file) = passthrough.as_mut() {
            file.write_all(packet).map_err(CrabError::Io)?;
            continue;
        }
        let next_len = pointer_candidate
            .len()
            .checked_add(packet.len())
            .ok_or_else(|| CrabError::Protocol("smudge input length overflow".to_owned()))?;
        if next_len <= MAX_LFS_POINTER_SIZE {
            pointer_candidate.extend_from_slice(packet);
            continue;
        }

        let mut file = tempfile::NamedTempFile::new().map_err(CrabError::Io)?;
        file.write_all(&pointer_candidate).map_err(CrabError::Io)?;
        file.write_all(packet).map_err(CrabError::Io)?;
        pointer_candidate.clear();
        passthrough = Some(file);
    }

    match passthrough {
        Some(mut file) => {
            file.flush().map_err(CrabError::Io)?;
            Ok(SmudgeInput::PassthroughFile(file.into_temp_path()))
        }
        None => Ok(SmudgeInput::PointerCandidate(pointer_candidate)),
    }
}

struct FilterResponse<W: Write> {
    inner: W,
    state: ResponseState,
    failed: bool,
}

enum ResponseState {
    NotStarted,
    Started,
    Complete,
}

impl<W: Write> FilterResponse<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            state: ResponseState::NotStarted,
            failed: false,
        }
    }

    fn content_response(&mut self, emit: impl FnOnce(&mut Self) -> Result<()>) -> Result<()> {
        write_status(self, "success")?;
        write_flush(self)?;
        let result = emit(self);
        if self.failed {
            return result;
        }
        // Content readers fail between packets; finish that response with a
        // final error status. Transport failures and panics may split a packet
        // and must instead terminate the session without another response.
        write_flush(self)?;
        if result.is_err() {
            write_status(self, "error")?;
        }
        write_flush(self)?;
        self.flush().map_err(CrabError::Io)?;
        self.state = ResponseState::Complete;
        result
    }
}

impl<W: Write> Write for FilterResponse<W> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.state = ResponseState::Started;
        // Poison before calling the transport so a panic cannot look like a
        // recoverable source-read failure after partially writing a packet.
        self.failed = true;
        let count = self.inner.write(bytes)?;
        self.failed = count == 0 && !bytes.is_empty();
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.state = ResponseState::Started;
        self.failed = true;
        self.inner.flush()?;
        self.failed = false;
        Ok(())
    }
}

/// Write a text line (with trailing newline) in packet-line format.
fn write_text_line<W: Write>(output: &mut W, text: &str) -> Result<()> {
    let data = format!("{text}\n");
    let len = data.len() + 4;
    write!(output, "{len:04x}").map_err(CrabError::Io)?;
    output.write_all(data.as_bytes()).map_err(CrabError::Io)?;
    Ok(())
}

/// Write a status=<value> line.
fn write_status<W: Write>(output: &mut W, status: &str) -> Result<()> {
    write_text_line(output, &format!("status={status}"))
}

/// Write raw content as packet-line data packets.
fn write_content<W: Write>(output: &mut W, data: &[u8]) -> Result<()> {
    // Split into chunks that fit in a single packet-line (max 65516 data bytes).
    const MAX_CHUNK: usize = 65516;
    for chunk in data.chunks(MAX_CHUNK) {
        let len = chunk.len() + 4;
        write!(output, "{len:04x}").map_err(CrabError::Io)?;
        output.write_all(chunk).map_err(CrabError::Io)?;
    }
    Ok(())
}

fn write_smudge_output<W: Write>(output: &mut W, content: &SmudgeOutput) -> Result<()> {
    match content {
        SmudgeOutput::Bytes(data) => write_content(output, data),
        SmudgeOutput::LfsFile { path, oid, size } => {
            crate::lfs::cache::stream_verified(path, oid, *size, |bytes| {
                write_content(output, bytes)
            })
        }
        SmudgeOutput::TemporaryFile(path) => write_content_file(output, path),
    }
}

fn write_content_file<W: Write>(output: &mut W, path: &Path) -> Result<()> {
    let mut file = std::fs::File::open(path).map_err(CrabError::Io)?;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(CrabError::Io)?;
        if read == 0 {
            return Ok(());
        }
        write_content(output, &buffer[..read])?;
    }
}

/// Write a flush packet (0000).
fn write_flush<W: Write>(output: &mut W) -> Result<()> {
    output.write_all(b"0000").map_err(CrabError::Io)?;
    Ok(())
}

fn session_lfs_storage_dir(session: &super::clean::CleanSession) -> Result<Option<PathBuf>> {
    if let Some(path) = session.lfs_storage_dir() {
        return Ok(Some(path.to_path_buf()));
    }
    let Some(root) = session.repo_root() else {
        return Ok(None);
    };
    let worktree = match WorktreeContext::resolve_from_path(root) {
        Ok(worktree) => worktree,
        Err(error) => {
            tracing::debug!(
                root = %root.display(),
                error = %error,
                "local LFS cache unavailable outside a Git worktree"
            );
            return Ok(None);
        }
    };
    let config = match crate::lfs::config::LfsConfig::resolve(&worktree.current_worktree_root) {
        Ok(config) => config,
        Err(error) => {
            tracing::debug!(
                root = %root.display(),
                error = %error,
                "invalid LFS config; using the default local cache path"
            );
            crate::lfs::config::LfsConfig::default()
        }
    };
    Ok(Some(config.storage_dir(&worktree.common_git_dir)))
}

/// Try to read an LFS object from the configured local cache.
fn try_local_lfs_cache(
    pointer: &crab_git::lfs_pointer::LfsPointer,
    session: &super::clean::CleanSession,
) -> Result<Option<Vec<u8>>> {
    let Some(lfs_dir) = session_lfs_storage_dir(session)? else {
        return Ok(None);
    };
    match crate::lfs::cache::read_pointer(&lfs_dir, pointer) {
        Err(CrabError::LfsObjectCorrupt { .. }) => Ok(None),
        result => result,
    }
}

/// Cache an LFS object in the configured local LFS directory.
fn cache_lfs_locally(
    pointer: &crab_git::lfs_pointer::LfsPointer,
    content: &[u8],
    session: &super::clean::CleanSession,
) -> Result<()> {
    let Some(lfs_dir) = session_lfs_storage_dir(session)? else {
        return Ok(());
    };
    crate::lfs::cache::install_bytes(&lfs_dir, &pointer.oid, pointer.size, content)?;
    Ok(())
}

/// Extract a human-readable message from a panic payload.
fn panic_payload_to_string(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_owned()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_owned()
    }
}

/// Maximum body length (bytes) of a single pkt-line packet.
///
/// The git protocol caps a pkt-line at 65 520 bytes total including the
/// 4-byte length header, so the body fits in 65 516 bytes.
const PKT_LINE_MAX_BODY: usize = 65516;

/// Streaming reader for git's pkt-line framing.
///
/// Each pkt-line has a 4-byte ASCII-hex length header covering the whole
/// packet (header + body). A length of `0000` is the flush packet and
/// signals end of stream. The reader surfaces data packets as borrowed
/// slices from its internal buffer and `Ok(None)` on flush.
///
/// Designed for the streaming clean filter: we want to feed packet bodies
/// directly into the CDC chunker and blake3 hasher without copying into an
/// accumulator first, so peak memory stays at one packet (≤64 KiB) instead
/// of the full file.
pub struct PktLineReader<R: Read> {
    inner: R,
    buf: Vec<u8>,
    state: ContentState,
}

enum ContentState {
    Reading,
    Finished,
    Failed,
}

impl<R: Read> PktLineReader<R> {
    /// Construct a reader wrapping an arbitrary `Read` source.
    pub fn from_read(inner: R) -> Self {
        Self {
            inner,
            buf: Vec::with_capacity(PKT_LINE_MAX_BODY),
            state: ContentState::Reading,
        }
    }

    /// Read the next pkt-line packet.
    ///
    /// Returns `Ok(Some(body))` for a data packet, `Ok(None)` for a flush
    /// packet (`0000`), or an error on I/O failure or malformed framing.
    /// Flush is terminal for this reader. After a framing failure the reader
    /// rejects further reads rather than guessing the next packet boundary.
    ///
    /// The returned slice borrows from the reader's internal buffer and is
    /// invalidated by the next call to `read_packet`. Callers that need to
    /// retain packet bytes past the next read must copy them.
    pub fn read_packet(&mut self) -> Result<Option<&[u8]>> {
        match self.state {
            ContentState::Finished => return Ok(None),
            ContentState::Failed => {
                return Err(CrabError::Protocol(
                    "filter content framing lost".to_owned(),
                ));
            }
            ContentState::Reading => {}
        }
        // A partial read or panic loses framing. Only a complete packet makes
        // subsequent draining safe; never reinterpret its remainder as a header.
        self.state = ContentState::Failed;
        let mut hdr = [0u8; 4];
        self.inner.read_exact(&mut hdr).map_err(CrabError::Io)?;

        // Only flush ends filter content. Delimiter/response-end belong to
        // other Git protocols and cannot certify a complete filter request.
        if &hdr == b"0000" {
            self.state = ContentState::Finished;
            return Ok(None);
        }

        let hex = std::str::from_utf8(&hdr).map_err(|_| {
            CrabError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "pkt-line length header is not ASCII",
            ))
        })?;
        let len: usize = u16::from_str_radix(hex, 16)
            .map_err(|_| {
                CrabError::Io(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("pkt-line length header is not hex: {hex:?}"),
                ))
            })?
            .into();

        if len < 4 {
            return Err(CrabError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("pkt-line length {len} shorter than header"),
            )));
        }
        if len - 4 > PKT_LINE_MAX_BODY {
            return Err(CrabError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("pkt-line body exceeds {PKT_LINE_MAX_BODY} bytes"),
            )));
        }

        let body_len = len - 4;
        self.buf.clear();
        self.buf.resize(body_len, 0);
        self.inner
            .read_exact(&mut self.buf[..])
            .map_err(CrabError::Io)?;
        self.state = ContentState::Reading;
        Ok(Some(&self.buf[..]))
    }
}

impl<'a> PktLineReader<&'a [u8]> {
    /// Construct a reader from an in-memory byte slice.
    ///
    /// `&[u8]` implements `Read`, so this is just a thin wrapper around
    /// `from_read`. Useful for the `clean_file` code path that already
    /// owns the full payload.
    pub fn from_slice(data: &'a [u8]) -> Self {
        Self::from_read(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::{Command, Output};
    use std::sync::MutexGuard;

    #[tokio::test]
    async fn speculative_warm_verifies_bytes_without_materializing_worktree() {
        for corrupt_origin in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let (hydrator, pointer, counted) = crate::read::test_support::stored_file(
                &root.path().join("cache"),
                Bytes::from(vec![42; 4 * 1024 * 1024]),
                corrupt_origin,
            )
            .await
            .unwrap();
            let path = root.path().join("model.bin");
            let pointer_bytes = pointer.serialize();
            std::fs::write(&path, &pointer_bytes).unwrap();
            let result = warm_pointer_cache(&path, &hydrator).await;
            if corrupt_origin {
                assert_eq!(result.unwrap_err().code(), "CRAB-E0020");
            } else {
                result.unwrap();
                let reads = counted.counts().body_requests();
                counted.set_body_reads_enabled(false);
                warm_pointer_cache(&path, &hydrator).await.unwrap();
                assert_eq!(counted.counts().body_requests(), reads);
            }
            assert_eq!(std::fs::read(&path).unwrap(), pointer_bytes);
        }
    }

    #[test]
    fn speculative_pointer_probe_preserves_pointer_size_boundaries() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("file");
        assert!(!path_is_hydrated(&path));
        let crab = Pointer {
            file_hash: [1; 32],
            size: 8 * 1024 * 1024,
            shard_hint: None,
        };
        let lfs = crab_git::lfs_pointer::LfsPointer {
            oid: [2; 32],
            size: 8 * 1024 * 1024,
            extensions: Vec::new(),
        };
        for (pointer, limit) in [
            (crab.serialize(), MAX_POINTER_SIZE + 1),
            (lfs.serialize(), crab_git::lfs_pointer::MAX_LFS_POINTER_SIZE),
        ] {
            std::fs::write(&path, &pointer).unwrap();
            assert!(!path_is_hydrated(&path));
            let mut full_content = pointer;
            full_content.resize(limit, b'\n');
            std::fs::write(&path, &full_content).unwrap();
            assert!(path_is_hydrated(&path));
        }
    }

    fn output_bytes(output: &SmudgeOutput) -> Vec<u8> {
        match output {
            SmudgeOutput::Bytes(bytes) => bytes.to_vec(),
            SmudgeOutput::LfsFile { path, .. } => std::fs::read(path).unwrap(),
            SmudgeOutput::TemporaryFile(path) => std::fs::read(path).unwrap(),
        }
    }

    #[test]
    fn lfs_store_source_caches_unavailable_remote() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let calls = Arc::new(AtomicUsize::new(0));
        let loader_calls = Arc::clone(&calls);
        let source = LfsStoreSource::new(
            None,
            Some(Arc::new(move || {
                loader_calls.fetch_add(1, Ordering::SeqCst);
                None
            })),
        );

        assert!(source.resolve().is_none());
        assert!(source.resolve().is_none());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    struct GitEnvGuard {
        _lock: MutexGuard<'static, ()>,
        prev_git_dir: Option<std::ffi::OsString>,
        prev_git_work_tree: Option<std::ffi::OsString>,
        prev_git_common_dir: Option<std::ffi::OsString>,
    }

    impl GitEnvGuard {
        fn set(git_dir: &Path, work_tree: &Path, common_dir: &Path) -> Self {
            let lock = crate::test::git_repo::GIT_DIR_MUTEX
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let prev_git_dir = std::env::var_os("GIT_DIR");
            let prev_git_work_tree = std::env::var_os("GIT_WORK_TREE");
            let prev_git_common_dir = std::env::var_os("GIT_COMMON_DIR");
            // SAFETY: test environment mutation is serialized by GIT_DIR_MUTEX.
            unsafe {
                std::env::set_var("GIT_DIR", git_dir);
                std::env::set_var("GIT_WORK_TREE", work_tree);
                std::env::set_var("GIT_COMMON_DIR", common_dir);
            }
            Self {
                _lock: lock,
                prev_git_dir,
                prev_git_work_tree,
                prev_git_common_dir,
            }
        }
    }

    impl Drop for GitEnvGuard {
        fn drop(&mut self) {
            // SAFETY: test environment mutation is serialized by GIT_DIR_MUTEX.
            unsafe {
                match &self.prev_git_dir {
                    Some(value) => std::env::set_var("GIT_DIR", value),
                    None => std::env::remove_var("GIT_DIR"),
                }
                match &self.prev_git_work_tree {
                    Some(value) => std::env::set_var("GIT_WORK_TREE", value),
                    None => std::env::remove_var("GIT_WORK_TREE"),
                }
                match &self.prev_git_common_dir {
                    Some(value) => std::env::set_var("GIT_COMMON_DIR", value),
                    None => std::env::remove_var("GIT_COMMON_DIR"),
                }
            }
        }
    }

    fn run_git<I, S>(cwd: &Path, args: I) -> Option<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<std::ffi::OsStr>,
    {
        Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .ok()
    }

    #[test]
    fn filter_paths_use_current_worktree_for_files_and_shared_staging_for_chunks() {
        let tmp = tempfile::tempdir().unwrap();
        let main = tmp.path().join("repo");
        let linked = tmp.path().join("linked");
        let common_git_dir = main.join(".git");
        let ctx = WorktreeContext {
            current_worktree_root: linked.clone(),
            main_worktree_root: main.clone(),
            common_git_dir: common_git_dir.clone(),
            per_worktree_git_dir: common_git_dir.join("worktrees").join("linked"),
            shared_crab_dir: main.join(".crab"),
            per_worktree_crab_dir: main.join(".crab").join("worktrees").join("linked"),
            identity: "linked".to_owned(),
        };

        let paths = filter_paths_from_context(&ctx);

        assert_eq!(paths.current_worktree_root, linked);
        assert_eq!(
            paths.shared_staging_root,
            main.join(".crab").join("staging")
        );
    }

    #[test]
    fn available_blobs_response_lists_paths_before_success_status() {
        let mut output = Vec::new();

        write_available_blobs_response(&mut output, &["data/main.bin".to_owned()]).unwrap();

        let output = String::from_utf8_lossy(&output);
        let pathname = output.find("pathname=data/main.bin").unwrap();
        let status = output.find("status=success").unwrap();
        assert!(pathname < status);
    }

    #[test]
    fn delayed_response_has_no_extra_flush_packet() {
        let mut output = Vec::new();

        write_delayed_response(&mut output).unwrap();

        let output = String::from_utf8_lossy(&output);
        assert!(output.contains("status=delayed"));
        assert_eq!(output.matches("0000").count(), 1);
    }

    #[test]
    fn filter_loop_honors_linked_worktree_git_environment_for_attributes() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let linked = tmp.path().join("linked");
        if !Command::new("git")
            .args(["init", "-q", repo.to_str().unwrap()])
            .status()
            .is_ok_and(|status| status.success())
        {
            eprintln!("SKIP: git unavailable or fixture setup failed");
            return;
        }
        let _ = run_git(&repo, ["config", "user.email", "test@example.com"]);
        let _ = run_git(&repo, ["config", "user.name", "test"]);
        std::fs::write(repo.join("README.md"), "initial\n").unwrap();
        let _ = run_git(&repo, ["add", "README.md"]);
        let Some(commit) = run_git(&repo, ["commit", "-q", "-m", "initial"]) else {
            eprintln!("SKIP: git commit unavailable");
            return;
        };
        if !commit.status.success() {
            eprintln!("SKIP: git commit fixture setup failed");
            return;
        }
        let Some(add) = run_git(
            &repo,
            [
                "worktree",
                "add",
                "-q",
                "--detach",
                linked.to_str().unwrap(),
                "HEAD",
            ],
        ) else {
            eprintln!("SKIP: git worktree unavailable");
            return;
        };
        if !add.status.success() {
            eprintln!("SKIP: git worktree fixture setup failed");
            return;
        }

        std::fs::write(
            repo.join(".gitattributes"),
            "*.bin filter=crab diff=crab merge=crab -text\n",
        )
        .unwrap();
        std::fs::write(
            linked.join(".gitattributes"),
            "*.bin filter=lfs diff=lfs merge=lfs -text\n",
        )
        .unwrap();

        let repo = repo.canonicalize().unwrap();
        let linked = linked.canonicalize().unwrap();
        let admin_dir = repo
            .join(".git")
            .join("worktrees")
            .join("linked")
            .canonicalize()
            .unwrap();
        let _env = GitEnvGuard::set(&admin_dir, &linked, &repo.join(".git"));

        let paths = resolve_filter_worktree_paths().expect("filter worktree paths");
        assert_eq!(paths.current_worktree_root, linked);
        assert_eq!(
            paths.shared_staging_root,
            repo.join(".crab").join("staging")
        );

        let mut input = build_handshake_input();
        input.extend(pkt_text("command=clean"));
        input.extend(pkt_text("pathname=model.bin"));
        input.extend(pkt_flush());
        input.extend(pkt_data(b"linked worktree lfs payload"));
        input.extend(pkt_flush());

        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut output = Vec::new();
        let staging = Arc::new(std::sync::Mutex::new(LazyStaging::Locked {
            holder_pid: Some(4242),
        }));
        run_filter_loop(
            &mut &input[..],
            &mut output,
            AppContext::default(),
            Arc::clone(&staging),
            None,
            None,
            None,
            Some(rt.handle().clone()),
            Arc::new(std::sync::Mutex::new(None)),
        )
        .unwrap();

        let output_str = String::from_utf8_lossy(&output);
        assert!(output_str.contains("status=success"));
        assert!(output_str.contains("version https://git-lfs.github.com/spec/v1"));
        assert!(!output_str.contains("version https://crab.dev/spec/v1"));
        assert!(matches!(
            &*staging
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            LazyStaging::Locked {
                holder_pid: Some(4242)
            }
        ));
    }

    #[test]
    fn lfs_smudge_leaves_empty_tracked_file_empty() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::write(
            repo.path().join(".gitattributes"),
            "*.bin filter=lfs diff=lfs merge=lfs -text\n",
        )
        .unwrap();
        let mut session = super::super::clean::CleanSession::new(AppContext::default());
        session.set_repo_root(repo.path().to_path_buf());

        let output = smudge_content(
            b"",
            "empty.bin",
            false,
            &LfsStoreSource::eager(None),
            &session,
            None,
            None,
        )
        .unwrap();

        assert!(output_bytes(&output).is_empty());
    }

    #[test]
    fn lfs_smudge_streams_from_configured_cache_path() {
        use crab_git::lfs_pointer::LfsPointer;
        use sha2::{Digest, Sha256};

        let repo = tempfile::tempdir().unwrap();
        assert!(
            Command::new("git")
                .args(["init", "--quiet"])
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("GIT_COMMON_DIR")
                .current_dir(repo.path())
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("git")
                .args(["config", "lfs.storage", "custom-lfs-cache"])
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("GIT_COMMON_DIR")
                .current_dir(repo.path())
                .status()
                .unwrap()
                .success()
        );

        let content = b"local LFS cache content";
        let oid: [u8; 32] = Sha256::digest(content).into();
        let pointer = LfsPointer {
            oid,
            size: content.len() as u64,
            extensions: Vec::new(),
        };
        let lfs_dir = repo.path().join(".git/custom-lfs-cache");
        crate::lfs::cache::install_bytes(&lfs_dir, &oid, pointer.size, content).unwrap();
        let lfs_dir = lfs_dir.canonicalize().unwrap();

        let _env = GitEnvGuard::set(
            &repo.path().join(".git"),
            repo.path(),
            &repo.path().join(".git"),
        );
        let mut session = super::super::clean::CleanSession::new(AppContext::default());
        session.set_repo_root(repo.path().to_path_buf());
        let result = try_stream_lfs_smudge(
            &pointer.serialize(),
            "model.bin",
            &LfsStoreSource::eager(None),
            &session,
            false,
            None,
        )
        .unwrap()
        .expect("valid local LFS object should use the file-backed path");

        let path = match &result {
            SmudgeOutput::LfsFile { path, .. } => path,
            SmudgeOutput::Bytes(_) => {
                panic!("local LFS object should not be materialized in memory")
            }
            SmudgeOutput::TemporaryFile(_) => {
                panic!("local LFS object should use its persistent cache path")
            }
        };
        assert_eq!(path, &crate::lfs::cache::object_path(&lfs_dir, &oid));

        let mut output = Vec::new();
        write_smudge_output(&mut output, &result).unwrap();
        assert!(
            output
                .windows(content.len())
                .any(|window| window == content),
            "file-backed smudge output should contain the cached content"
        );
    }

    /// Build a packet-line text frame: 4-hex-length + data + \n.
    fn pkt_text(s: &str) -> Vec<u8> {
        let data = format!("{s}\n");
        let len = data.len() + 4;
        format!("{len:04x}{data}").into_bytes()
    }

    fn pkt_flush() -> Vec<u8> {
        b"0000".to_vec()
    }

    fn pkt_data(data: &[u8]) -> Vec<u8> {
        let len = data.len() + 4;
        let mut buf = format!("{len:04x}").into_bytes();
        buf.extend_from_slice(data);
        buf
    }

    /// Build a complete handshake input from git's side.
    fn build_handshake_input() -> Vec<u8> {
        let mut input = Vec::new();
        input.extend(pkt_text("git-filter-client"));
        input.extend(pkt_text("version=2"));
        input.extend(pkt_flush());
        input.extend(pkt_text("capability=clean"));
        input.extend(pkt_text("capability=smudge"));
        input.extend(pkt_text("capability=delay"));
        input.extend(pkt_flush());
        input
    }

    #[test]
    fn handshake_produces_correct_bytes() {
        let input = build_handshake_input();
        let mut output = Vec::new();

        handshake(&mut &input[..], &mut output).unwrap();

        let output_str = String::from_utf8(output).unwrap();

        // Server should respond with git-filter-server, version=2, flush,
        // then capabilities, flush.
        assert!(output_str.contains("git-filter-server"));
        assert!(output_str.contains("version=2"));
        assert!(output_str.contains("capability=clean"));
        assert!(output_str.contains("capability=smudge"));
        assert!(output_str.contains("capability=delay"));

        // Snapshot the exact bytes for regression.
        insta::assert_snapshot!("handshake_response", output_str);
    }

    #[test]
    fn handshake_rejects_wrong_client() {
        let mut input = Vec::new();
        input.extend(pkt_text("git-filter-wrong"));
        input.extend(pkt_text("version=2"));
        input.extend(pkt_flush());

        let mut output = Vec::new();
        let err = handshake(&mut &input[..], &mut output).unwrap_err();
        assert!(err.to_string().contains("git-filter-client"));
    }

    #[test]
    fn handshake_rejects_wrong_version() {
        let mut input = Vec::new();
        input.extend(pkt_text("git-filter-client"));
        input.extend(pkt_text("version=3"));
        input.extend(pkt_flush());

        let mut output = Vec::new();
        let err = handshake(&mut &input[..], &mut output).unwrap_err();
        assert!(err.to_string().contains("version=2"));
    }

    #[test]
    fn read_command_parses_clean() {
        let mut input = Vec::new();
        input.extend(pkt_text("command=clean"));
        input.extend(pkt_text("pathname=large.bin"));
        input.extend(pkt_flush());

        let cmd = read_command(&mut &input[..]).unwrap().unwrap();
        assert_eq!(cmd.command, FilterOperation::Clean);
        assert_eq!(cmd.pathname, "large.bin");
    }

    #[test]
    fn read_command_returns_none_on_eof() {
        let input: &[u8] = &[];
        let cmd = read_command(&mut &*input).unwrap();
        assert!(cmd.is_none());
    }

    #[test]
    fn invalid_filter_requests_are_terminal_without_acknowledgment() {
        let repo = tempfile::tempdir().unwrap();
        let git_dir = repo.path().join(".git");
        let _env = GitEnvGuard::set(&git_dir, repo.path(), &git_dir);
        let mut expected = Vec::new();
        handshake(&mut &build_handshake_input()[..], &mut expected).unwrap();
        let smudge = [pkt_text("command=smudge"), pkt_text("pathname=plain.txt")].concat();
        let requests = [
            (
                "unknown command",
                [pkt_text("command=unsupported"), pkt_flush()].concat(),
            ),
            ("wrong command key", pkt_text("operation=smudge")),
            ("partial length", b"001".to_vec()),
            ("partial text", b"0010command=".to_vec()),
            ("unexpected flush", pkt_flush()),
            (
                "missing list flush",
                pkt_text("command=list_available_blobs"),
            ),
            (
                "missing pathname",
                [pkt_text("command=smudge"), pkt_flush(), pkt_flush()].concat(),
            ),
            (
                "header delimiter",
                [smudge.clone(), b"0001".to_vec(), pkt_flush()].concat(),
            ),
            (
                "header response end",
                [smudge.clone(), b"0002".to_vec(), pkt_flush()].concat(),
            ),
            (
                "content delimiter",
                [smudge.clone(), pkt_flush(), b"0001".to_vec()].concat(),
            ),
            (
                "content response end",
                [smudge, pkt_flush(), b"0002".to_vec()].concat(),
            ),
        ];
        for (name, request) in requests {
            let mut input = build_handshake_input();
            input.extend(request);
            let mut output = Vec::new();
            let result = run_filter_loop(
                &mut &input[..],
                &mut output,
                AppContext::default(),
                Arc::new(std::sync::Mutex::new(LazyStaging::Unavailable)),
                None,
                None,
                None,
                None,
                Arc::new(std::sync::Mutex::new(None)),
            );
            assert!(result.is_err() && output == expected, "{name}: {result:?}");
        }
    }

    #[test]
    fn handshake_requires_the_final_capability_flush() {
        let mut input = build_handshake_input();
        input.truncate(input.len() - 4);
        assert!(handshake(&mut &input[..], &mut Vec::new()).is_err());
    }

    #[test]
    fn read_smudge_input_collects_pointer_candidate_across_packets() {
        let mut input = Vec::new();
        input.extend(pkt_data(b"hello "));
        input.extend(pkt_data(b"world"));
        input.extend(pkt_flush());

        let content =
            read_smudge_input_until_flush(&mut PktLineReader::from_slice(&input)).unwrap();
        assert!(matches!(
            content,
            SmudgeInput::PointerCandidate(bytes) if bytes == b"hello world"
        ));
    }

    #[test]
    fn read_smudge_input_supports_pointer_split_at_every_byte() {
        let pointer = crab_types::pointer::Pointer {
            file_hash: [7; 32],
            size: 42,
            shard_hint: Some([8; 32]),
        }
        .serialize();
        let mut input = Vec::new();
        for byte in &pointer {
            input.extend(pkt_data(std::slice::from_ref(byte)));
        }
        input.extend(pkt_flush());

        let content =
            read_smudge_input_until_flush(&mut PktLineReader::from_slice(&input)).unwrap();
        assert!(matches!(
            content,
            SmudgeInput::PointerCandidate(bytes) if bytes == pointer
        ));
    }

    #[test]
    fn read_smudge_input_spools_large_passthrough_across_many_packets() {
        let body = (0..(MAX_LFS_POINTER_SIZE * 3 + 17))
            .map(|idx| (idx % 251) as u8)
            .collect::<Vec<_>>();
        let mut input = Vec::new();
        for chunk in body.chunks(7) {
            input.extend(pkt_data(chunk));
        }
        input.extend(pkt_flush());

        let content =
            read_smudge_input_until_flush(&mut PktLineReader::from_slice(&input)).unwrap();
        let SmudgeInput::PassthroughFile(path) = content else {
            panic!("large non-pointer input must be spooled");
        };
        assert_eq!(std::fs::read(path).unwrap(), body);
    }

    #[test]
    fn read_smudge_input_retains_direct_lfs_parse_size_boundary() {
        let mut body = b"version https://git-lfs.github.com/spec/v1\noid sha256:0000000000000000000000000000000000000000000000000000000000000000\nsize 1\n".to_vec();
        body.resize(MAX_LFS_POINTER_SIZE, b'\n');
        let mut input = Vec::new();
        for chunk in body.chunks(11) {
            input.extend(pkt_data(chunk));
        }
        input.extend(pkt_flush());

        let content =
            read_smudge_input_until_flush(&mut PktLineReader::from_slice(&input)).unwrap();

        assert!(matches!(
            content,
            SmudgeInput::PointerCandidate(bytes) if bytes == body
        ));
    }

    #[test]
    fn full_clean_session() {
        let mut input = build_handshake_input();

        // Send a clean command.
        input.extend(pkt_text("command=clean"));
        input.extend(pkt_text("pathname=test.bin"));
        input.extend(pkt_flush());
        // Content.
        input.extend(pkt_data(b"file content here"));
        input.extend(pkt_flush());

        // EOF after one command.
        let mut output = Vec::new();
        let ctx = AppContext::default();
        let staging_root = tempfile::tempdir().unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();

        run_filter_loop(
            &mut &input[..],
            &mut output,
            ctx,
            Arc::new(std::sync::Mutex::new(LazyStaging::Unopened {
                staging_root: staging_root.path().to_path_buf(),
            })),
            None,
            None,
            None,
            Some(rt.handle().clone()),
            Arc::new(std::sync::Mutex::new(None)),
        )
        .unwrap();

        let output_str = String::from_utf8_lossy(&output);
        // Should contain handshake + status=success.
        assert!(output_str.contains("git-filter-server"));
        assert!(output_str.contains("status=success"));
    }

    #[test]
    fn clean_creates_missing_fresh_staging_root() {
        let mut input = build_handshake_input();
        input.extend(pkt_text("command=clean"));
        input.extend(pkt_text("pathname=payload.bin"));
        input.extend(pkt_flush());
        input.extend(pkt_data(b"fresh repo content"));
        input.extend(pkt_flush());

        let repo = tempfile::tempdir().unwrap();
        let staging_root = repo.path().join(".crab").join("staging");
        assert!(
            !staging_root.exists(),
            "regression setup must start before staging exists"
        );

        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut output = Vec::new();
        run_filter_loop(
            &mut &input[..],
            &mut output,
            AppContext::default(),
            Arc::new(std::sync::Mutex::new(LazyStaging::from_root(Some(
                staging_root.clone(),
            )))),
            None,
            None,
            None,
            Some(rt.handle().clone()),
            Arc::new(std::sync::Mutex::new(None)),
        )
        .unwrap();

        let output_str = String::from_utf8_lossy(&output);
        assert!(output_str.contains("status=success"));
        assert!(
            staging_root.join("index.db").exists(),
            "first clean should create the staging database"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn filter_clean_waits_past_retired_short_flock_budget() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let first = StagingArea::open(root.clone())
            .await
            .expect("first writer open");
        let cell = Arc::new(std::sync::Mutex::new(LazyStaging::Unopened {
            staging_root: root,
        }));

        let acquiring_cell = Arc::clone(&cell);
        let acquire_task = tokio::spawn(async move {
            let started = std::time::Instant::now();
            let result = acquire_writer(acquiring_cell.as_ref()).await;
            (result, started.elapsed())
        });

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(
            !acquire_task.is_finished(),
            "second writer should wait while the first writer holds staging"
        );

        tokio::time::sleep(std::time::Duration::from_millis(3_200)).await;
        assert!(
            !acquire_task.is_finished(),
            "filter clean should stay queued beyond the retired 3s filter-only lock budget"
        );

        first.close().await.expect("close first writer");

        let (result, waited) = acquire_task.await.expect("acquire task");
        let writer = match result {
            StagingAcquire::Writer(sa) => sa,
            StagingAcquire::Locked { holder_pid } => {
                panic!("filter clean timed out instead of queueing; holder_pid={holder_pid:?}")
            }
            StagingAcquire::Unavailable => panic!("filter clean failed to open staging"),
        };
        assert!(
            waited >= std::time::Duration::from_secs(3),
            "filter clean acquired staging before proving the retired short budget was gone"
        );
        drop(writer);

        let final_staging = {
            let mut guard = cell
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::mem::replace(&mut *guard, LazyStaging::Unavailable)
        };
        match final_staging {
            LazyStaging::Writer(sa) => {
                let staging = Arc::try_unwrap(sa).ok().expect("only cell holds writer");
                staging.close().await.expect("close filter writer");
            }
            _ => panic!("acquired writer should be cached in the lazy staging cell"),
        }
    }

    #[test]
    fn lazy_smudge_passes_pointer_through_unchanged() {
        use crate::core::config::{CheckoutConfig, Config};
        use tokio_util::sync::CancellationToken;

        let config = Config {
            checkout: CheckoutConfig { lazy: true },
            ..Config::default()
        };
        let ctx = AppContext::new(config, CancellationToken::new());

        let mut input = build_handshake_input();

        // Build a valid pointer to use as smudge input.
        let pointer_bytes = b"version https://crab.dev/spec/v1\n\
file-hash 000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f\n\
size 1048576\n";

        input.extend(pkt_text("command=smudge"));
        input.extend(pkt_text("pathname=model.bin"));
        input.extend(pkt_flush());
        input.extend(pkt_data(pointer_bytes));
        input.extend(pkt_flush());

        let mut output = Vec::new();
        run_filter_loop(
            &mut &input[..],
            &mut output,
            ctx,
            Arc::new(std::sync::Mutex::new(LazyStaging::Unavailable)),
            None,
            None,
            None,
            None,
            Arc::new(std::sync::Mutex::new(None)),
        )
        .unwrap();

        let output_str = String::from_utf8_lossy(&output);
        assert!(output_str.contains("status=success"));

        // Verify the pointer bytes appear in the output unchanged.
        // The output contains packet-line framing, so check the raw bytes
        // are embedded within.
        assert!(
            output
                .windows(pointer_bytes.len())
                .any(|w| w == pointer_bytes),
            "pointer bytes should pass through unchanged in lazy mode"
        );
    }

    #[test]
    fn lazy_false_does_not_short_circuit_smudge() {
        // With lazy=false (default), smudge should go through the normal path.
        let ctx = AppContext::default();
        assert!(!ctx.config().checkout.lazy);

        let mut input = build_handshake_input();

        let content = b"some file content";
        input.extend(pkt_text("command=smudge"));
        input.extend(pkt_text("pathname=file.txt"));
        input.extend(pkt_flush());
        input.extend(pkt_data(content));
        input.extend(pkt_flush());

        let mut output = Vec::new();
        run_filter_loop(
            &mut &input[..],
            &mut output,
            ctx,
            Arc::new(std::sync::Mutex::new(LazyStaging::Unavailable)),
            None,
            None,
            None,
            None,
            Arc::new(std::sync::Mutex::new(None)),
        )
        .unwrap();

        let output_str = String::from_utf8_lossy(&output);
        assert!(output_str.contains("status=success"));
    }

    #[test]
    fn lazy_smudge_does_not_affect_clean_path() {
        use crate::core::config::{CheckoutConfig, Config};
        use tokio_util::sync::CancellationToken;

        let filter_worktree = tempfile::tempdir().unwrap();
        let git_dir = filter_worktree.path().join(".git");
        std::fs::create_dir_all(&git_dir).unwrap();
        let _git_env = GitEnvGuard::set(&git_dir, filter_worktree.path(), &git_dir);

        let config = Config {
            checkout: CheckoutConfig { lazy: true },
            ..Config::default()
        };
        let ctx = AppContext::new(config, CancellationToken::new());

        let mut input = build_handshake_input();

        // Clean command should still produce a pointer regardless of lazy mode.
        input.extend(pkt_text("command=clean"));
        input.extend(pkt_text("pathname=test.bin"));
        input.extend(pkt_flush());
        input.extend(pkt_data(b"file content here"));
        input.extend(pkt_flush());

        let staging_root = tempfile::tempdir().unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let mut output = Vec::new();
        run_filter_loop(
            &mut &input[..],
            &mut output,
            ctx,
            Arc::new(std::sync::Mutex::new(LazyStaging::Unopened {
                staging_root: staging_root.path().to_path_buf(),
            })),
            None,
            None,
            None,
            Some(rt.handle().clone()),
            Arc::new(std::sync::Mutex::new(None)),
        )
        .unwrap();

        let output_str = String::from_utf8_lossy(&output);
        assert!(output_str.contains("status=success"));
        // The clean path should produce a pointer (contains version line).
        assert!(
            output
                .windows(b"version https://crab.dev/spec/v1".len())
                .any(|w| w == b"version https://crab.dev/spec/v1"),
            "clean path should produce a pointer even with lazy=true"
        );
    }

    #[test]
    fn lfs_pointer_lazy_smudge_passes_through() {
        use crate::core::config::{CheckoutConfig, Config};
        use crab_git::lfs_pointer::LfsPointer;
        use tokio_util::sync::CancellationToken;

        let config = Config {
            checkout: CheckoutConfig { lazy: true },
            ..Config::default()
        };
        let ctx = AppContext::new(config, CancellationToken::new());

        // Build a valid LFS pointer.
        let oid = [0xABu8; 32];
        let lfs_pointer = LfsPointer {
            oid,
            size: 1024,
            extensions: Vec::new(),
        };
        let pointer_bytes = lfs_pointer.serialize();

        let mut input = build_handshake_input();
        input.extend(pkt_text("command=smudge"));
        input.extend(pkt_text("pathname=model.bin"));
        input.extend(pkt_flush());
        input.extend(pkt_data(&pointer_bytes));
        input.extend(pkt_flush());

        let mut output = Vec::new();
        run_filter_loop(
            &mut &input[..],
            &mut output,
            ctx,
            Arc::new(std::sync::Mutex::new(LazyStaging::Unavailable)),
            None,
            None,
            None,
            None,
            Arc::new(std::sync::Mutex::new(None)),
        )
        .unwrap();

        let output_str = String::from_utf8_lossy(&output);
        assert!(output_str.contains("status=success"));

        // LFS pointer should pass through unchanged in lazy mode.
        assert!(
            output
                .windows(pointer_bytes.len())
                .any(|w| w == pointer_bytes.as_slice()),
            "LFS pointer bytes should pass through unchanged in lazy mode"
        );
    }

    #[test]
    fn partial_response_write_failure_does_not_start_another_response() {
        #[derive(Clone, Copy, Debug)]
        enum Fault {
            Error,
            Zero,
            Panic,
            Flush,
        }
        struct FailOnceWriter {
            bytes: Vec<u8>,
            fail_at: usize,
            failed: bool,
            writes_after_failure: usize,
            fault: Fault,
        }
        impl Write for FailOnceWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if self.failed {
                    self.writes_after_failure += 1;
                } else if self.bytes.len() == self.fail_at && !matches!(self.fault, Fault::Flush) {
                    self.failed = true;
                    return match self.fault {
                        Fault::Zero => Ok(0),
                        Fault::Panic => panic!("injected response write panic"),
                        _ => Err(io::Error::other("injected response write failure")),
                    };
                }
                let count = if self.failed || matches!(self.fault, Fault::Flush) {
                    bytes.len()
                } else {
                    bytes.len().min(self.fail_at - self.bytes.len())
                };
                self.bytes.extend_from_slice(&bytes[..count]);
                Ok(count)
            }

            fn flush(&mut self) -> io::Result<()> {
                if self.failed {
                    self.writes_after_failure += 1;
                } else if matches!(self.fault, Fault::Flush) && self.bytes.len() >= self.fail_at {
                    self.failed = true;
                    return Err(io::Error::other("injected response flush failure"));
                }
                Ok(())
            }
        }
        let mut handshake_output = Vec::new();
        handshake(&mut &build_handshake_input()[..], &mut handshake_output).unwrap();
        let mut input = build_handshake_input();
        for pathname in ["first.txt", "next.txt"] {
            input.extend(pkt_text("command=smudge"));
            input.extend(pkt_text(&format!("pathname={pathname}")));
            input.extend(pkt_flush());
            input.extend(pkt_data(b"ordinary content"));
            input.extend(pkt_flush());
        }
        for fault in [Fault::Error, Fault::Zero, Fault::Panic, Fault::Flush] {
            for offset in [1, 2, 27, 45] {
                let mut output = FailOnceWriter {
                    bytes: Vec::new(),
                    fail_at: handshake_output.len() + offset,
                    failed: false,
                    writes_after_failure: 0,
                    fault,
                };
                let result = run_filter_loop(
                    &mut &input[..],
                    &mut output,
                    AppContext::default(),
                    Arc::new(std::sync::Mutex::new(LazyStaging::Unavailable)),
                    None,
                    None,
                    None,
                    None,
                    Arc::new(std::sync::Mutex::new(None)),
                );
                assert!(
                    result.is_err() && output.failed && output.writes_after_failure == 0,
                    "fault {fault:?} at response byte {offset}"
                );
            }
        }
    }

    #[test]
    fn content_read_failure_ends_with_error_status_at_a_packet_boundary() {
        let mut bytes = Vec::new();
        let mut response = FilterResponse::new(&mut bytes);
        response
            .content_response(|output| {
                write_content(output, b"partial content")?;
                Err(CrabError::Io(io::Error::other(
                    "injected content read failure",
                )))
            })
            .unwrap_err();
        let mut expected = pkt_text("status=success");
        expected.extend(pkt_flush());
        expected.extend(pkt_data(b"partial content"));
        expected.extend(pkt_flush());
        expected.extend(pkt_text("status=error"));
        expected.extend(pkt_flush());
        assert_eq!(bytes, expected);
    }

    #[tokio::test]
    async fn filter_process_discards_buffered_output_after_transport_failure() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct OutputProbe {
            written: usize,
            fail_at: usize,
            attempts: Arc<AtomicUsize>,
            panic_on_failure: bool,
        }
        impl Write for OutputProbe {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if self.written == self.fail_at {
                    self.attempts.fetch_add(1, Ordering::SeqCst);
                    assert!(!self.panic_on_failure, "injected buffered transport panic");
                    return Err(io::Error::other("injected buffered transport failure"));
                }
                let count = bytes.len().min(self.fail_at - self.written);
                self.written += count;
                Ok(count)
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut handshake_output = Vec::new();
        handshake(&mut &build_handshake_input()[..], &mut handshake_output).unwrap();
        let mut input = build_handshake_input();
        input.extend(pkt_text("command=smudge"));
        input.extend(pkt_text("pathname=plain.txt"));
        input.extend(pkt_flush());
        input.extend(pkt_data(b"ordinary content"));
        input.extend(pkt_flush());
        for fail_at in [2, handshake_output.len() + 2] {
            for panic_on_failure in [false, true] {
                let attempts = Arc::new(AtomicUsize::new(0));
                let output = OutputProbe {
                    written: 0,
                    fail_at,
                    attempts: Arc::clone(&attempts),
                    panic_on_failure,
                };
                let result = run_filter_process(
                    io::Cursor::new(input.clone()),
                    output,
                    AppContext::default(),
                    None,
                    None,
                    None,
                    #[cfg(unix)]
                    None,
                )
                .await;
                assert!(result.is_err() && attempts.load(Ordering::SeqCst) == 1);
            }
        }
    }

    #[test]
    fn lfs_stream_rejects_cache_changes_after_validation() {
        use sha2::{Digest, Sha256};

        let repo = tempfile::tempdir().unwrap();
        assert!(
            Command::new("git")
                .args(["init", "--quiet"])
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("GIT_COMMON_DIR")
                .current_dir(repo.path())
                .status()
                .unwrap()
                .success()
        );
        let git_dir = repo.path().join(".git");
        let _env = GitEnvGuard::set(&git_dir, repo.path(), &git_dir);
        let body = b"verified LFS content";
        let pointer = crab_git::lfs_pointer::LfsPointer {
            oid: Sha256::digest(body).into(),
            size: body.len() as u64,
            extensions: Vec::new(),
        };
        let lfs_dir = git_dir.join("lfs");
        let mut session = super::super::clean::CleanSession::new(AppContext::default());
        session.set_repo_root(repo.path().to_path_buf());
        for changed in [Vec::new(), vec![0; body.len()], vec![0; body.len() + 1]] {
            let path = crate::lfs::cache::install_bytes(&lfs_dir, &pointer.oid, pointer.size, body)
                .unwrap();
            let source = try_stream_lfs_smudge(
                &pointer.serialize(),
                "model.bin",
                &LfsStoreSource::eager(None),
                &session,
                false,
                None,
            )
            .unwrap()
            .unwrap();
            std::fs::write(path, &changed).unwrap();
            let mut bytes = Vec::new();
            let result = FilterResponse::new(&mut bytes)
                .content_response(|output| write_smudge_output(output, &source));
            assert!(
                matches!(result, Err(CrabError::LfsObjectCorrupt { .. })),
                "changed {}-byte cache must not complete successfully",
                changed.len()
            );
        }
    }

    #[test]
    fn disappearing_lfs_cache_reports_a_final_error_and_preserves_the_next_request() {
        use sha2::{Digest, Sha256};

        struct RemovingWriter {
            bytes: Vec<u8>,
            cache_path: PathBuf,
            remove_at: usize,
            removed: bool,
        }
        impl Write for RemovingWriter {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.bytes.extend_from_slice(bytes);
                if !self.removed && self.bytes.len() >= self.remove_at {
                    std::fs::remove_file(&self.cache_path)?;
                    self.removed = true;
                }
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let repo = tempfile::tempdir().unwrap();
        assert!(
            Command::new("git")
                .args(["init", "--quiet"])
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("GIT_COMMON_DIR")
                .current_dir(repo.path())
                .status()
                .unwrap()
                .success()
        );
        let git_dir = repo.path().join(".git");
        let _env = GitEnvGuard::set(&git_dir, repo.path(), &git_dir);
        let body = b"verified LFS content";
        let pointer = crab_git::lfs_pointer::LfsPointer {
            oid: Sha256::digest(body).into(),
            size: body.len() as u64,
            extensions: Vec::new(),
        };
        let lfs_dir = git_dir.join("lfs");
        crate::lfs::cache::install_bytes(&lfs_dir, &pointer.oid, pointer.size, body).unwrap();
        let mut expected = Vec::new();
        handshake(&mut &build_handshake_input()[..], &mut expected).unwrap();
        expected.extend(pkt_text("status=success"));
        expected.extend(pkt_flush());
        let mut output = RemovingWriter {
            bytes: Vec::new(),
            cache_path: crate::lfs::cache::object_path(&lfs_dir, &pointer.oid),
            remove_at: expected.len(),
            removed: false,
        };
        expected.extend(pkt_flush());
        expected.extend(pkt_text("status=error"));
        expected.extend(pkt_flush());
        expected.extend(pkt_text("status=success"));
        expected.extend(pkt_flush());
        expected.extend(pkt_data(b"next file"));
        expected.extend(pkt_flush());
        expected.extend(pkt_flush());
        let mut input = build_handshake_input();
        for (path, content) in [
            ("cached.bin", pointer.serialize()),
            ("readme.txt", b"next file".to_vec()),
        ] {
            input.extend(pkt_text("command=smudge"));
            input.extend(pkt_text(&format!("pathname={path}")));
            input.extend(pkt_flush());
            input.extend(pkt_data(&content));
            input.extend(pkt_flush());
        }
        run_filter_loop(
            &mut &input[..],
            &mut output,
            AppContext::default(),
            Arc::new(std::sync::Mutex::new(LazyStaging::Unavailable)),
            None,
            None,
            None,
            None,
            Arc::new(std::sync::Mutex::new(None)),
        )
        .unwrap();
        assert!(output.removed && output.bytes == expected);
    }

    #[test]
    fn failed_smudge_preserves_the_next_request() {
        assert_failed_smudge_preserves_the_next_request(LfsStoreSource::eager(None));
    }

    #[test]
    fn panicking_smudge_preserves_the_next_request() {
        assert_failed_smudge_preserves_the_next_request(LfsStoreSource::new(
            None,
            Some(Arc::new(|| panic!("injected remote resolution panic"))),
        ));
    }

    fn assert_failed_smudge_preserves_the_next_request(source: LfsStoreSource) {
        let repo = tempfile::tempdir().unwrap();
        let git_dir = repo.path().join(".git");
        std::fs::create_dir(&git_dir).unwrap();
        let _env = GitEnvGuard::set(&git_dir, repo.path(), &git_dir);
        let pointer = crab_git::lfs_pointer::LfsPointer {
            oid: [0xab; 32],
            size: 1024,
            extensions: Vec::new(),
        };
        let content = b"ordinary content after a failed request";
        let mut input = build_handshake_input();
        for (path, body) in [
            ("missing.bin", pointer.serialize()),
            ("readme.txt", content.to_vec()),
        ] {
            input.extend(pkt_text("command=smudge"));
            input.extend(pkt_text(&format!("pathname={path}")));
            input.extend(pkt_flush());
            input.extend(pkt_data(&body));
            input.extend(pkt_flush());
        }
        let mut output = Vec::new();
        run_filter_loop_with_lfs_source(
            &mut &input[..],
            &mut output,
            AppContext::default(),
            Arc::new(std::sync::Mutex::new(LazyStaging::Unavailable)),
            source,
            None,
            None,
            None,
            Arc::new(std::sync::Mutex::new(None)),
        )
        .unwrap();
        let mut expected = Vec::new();
        handshake(&mut &build_handshake_input()[..], &mut expected).unwrap();
        expected.extend(pkt_text("status=error"));
        expected.extend(pkt_flush());
        expected.extend(pkt_text("status=success"));
        expected.extend(pkt_flush());
        expected.extend(pkt_data(content));
        expected.extend(pkt_flush());
        expected.extend(pkt_flush());
        assert_eq!(output, expected);
    }

    #[test]
    fn non_lazy_lfs_smudge_fails_without_remote_store() {
        use crab_git::lfs_pointer::LfsPointer;

        let repo = tempfile::tempdir().unwrap();
        let mut session = super::super::clean::CleanSession::new(AppContext::default());
        session.set_repo_root(repo.path().to_path_buf());
        let pointer = LfsPointer {
            oid: [0xabu8; 32],
            size: 1024,
            extensions: Vec::new(),
        };

        let error = smudge_content(
            &pointer.serialize(),
            "model.bin",
            false,
            &LfsStoreSource::eager(None),
            &session,
            None,
            None,
        )
        .unwrap_err();

        assert!(matches!(error, CrabError::Configuration { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn filter_errors_reply_after_flush_without_waiting_for_the_next_request() {
        use std::net::Shutdown;
        use std::os::unix::net::UnixStream;
        use std::time::Duration;

        let repo = tempfile::tempdir().unwrap();
        let git_dir = repo.path().join(".git");
        std::fs::create_dir_all(git_dir.join("objects")).unwrap();
        std::fs::create_dir(git_dir.join("refs")).unwrap();
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(repo.path().join(".gitattributes"), "*.bin filter=lfs\n").unwrap();
        // A non-directory cache root fails clean before its remaining packets.
        std::fs::write(git_dir.join("lfs"), "not a directory").unwrap();
        let _env = GitEnvGuard::set(&git_dir, repo.path(), &git_dir);
        let pointer = crab_git::lfs_pointer::LfsPointer {
            oid: [0xab; 32],
            size: 1024,
            extensions: Vec::new(),
        };

        for command in ["clean", "smudge"] {
            let (mut client, server) = UnixStream::pair().unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let read_end = server.try_clone().unwrap();
            let worker = std::thread::spawn(move || {
                run_filter_loop(
                    BufReader::new(read_end),
                    BufWriter::new(server),
                    AppContext::default(),
                    Arc::new(std::sync::Mutex::new(LazyStaging::Unavailable)),
                    None,
                    None,
                    None,
                    None,
                    Arc::new(std::sync::Mutex::new(None)),
                )
            });
            // Always close the peer and join, including timeout/assertion failure.
            let exchange = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                client.write_all(&build_handshake_input()).unwrap();
                let mut expected = Vec::new();
                handshake(&mut &build_handshake_input()[..], &mut expected).unwrap();
                let mut received = vec![0; expected.len()];
                client.read_exact(&mut received).unwrap();
                assert_eq!(received, expected);

                client
                    .write_all(&pkt_text(&format!("command={command}")))
                    .unwrap();
                client.write_all(&pkt_text("pathname=missing.bin")).unwrap();
                client.write_all(&pkt_flush()).unwrap();
                let body = if command == "clean" {
                    vec![0x5a; MAX_LFS_POINTER_SIZE * 2]
                } else {
                    pointer.serialize()
                };
                client.write_all(&pkt_data(&body)).unwrap();
                client
                    .set_read_timeout(Some(Duration::from_millis(100)))
                    .unwrap();
                let pending = client.read(&mut [0]);
                assert!(
                    matches!(pending, Err(ref e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut))
                );
                client
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                if command == "clean" {
                    client.write_all(&pkt_data(b"unread clean tail")).unwrap();
                }
                client.write_all(&pkt_flush()).unwrap();
                let mut expected = pkt_text("status=error");
                expected.extend(pkt_flush());
                let mut received = vec![0; expected.len()];
                client.read_exact(&mut received).unwrap();
                assert_eq!(received, expected);

                // A bodyless delay query must not borrow the following smudge.
                client
                    .write_all(&pkt_text("command=list_available_blobs"))
                    .unwrap();
                client.write_all(&pkt_flush()).unwrap();
                let mut expected = pkt_flush();
                expected.extend(pkt_text("status=success"));
                expected.extend(pkt_flush());
                let mut received = vec![0; expected.len()];
                client.read_exact(&mut received).unwrap();
                assert_eq!(received, expected);
                client.write_all(&pkt_text("command=smudge")).unwrap();
                client.write_all(&pkt_text("pathname=readme.txt")).unwrap();
                client.write_all(&pkt_flush()).unwrap();
                client.write_all(&pkt_data(b"next file")).unwrap();
                client.write_all(&pkt_flush()).unwrap();
                let mut expected = pkt_text("status=success");
                expected.extend(pkt_flush());
                expected.extend(pkt_data(b"next file"));
                expected.extend(pkt_flush());
                expected.extend(pkt_flush());
                let mut received = vec![0; expected.len()];
                client.read_exact(&mut received).unwrap();
                assert_eq!(received, expected);
            }));
            let _ = client.shutdown(Shutdown::Both);
            worker.join().unwrap().unwrap();
            exchange.unwrap();
        }
    }

    #[test]
    fn skip_download_errors_preserves_pointer_without_remote_store() {
        use crab_git::lfs_pointer::LfsPointer;

        let repo = tempfile::tempdir().unwrap();
        std::fs::write(
            repo.path().join(".lfsconfig"),
            "[lfs]\n    skipdownloaderrors = true\n",
        )
        .unwrap();
        let mut session = super::super::clean::CleanSession::new(AppContext::default());
        session.set_repo_root(repo.path().to_path_buf());
        let pointer = LfsPointer {
            oid: [0xabu8; 32],
            size: 1024,
            extensions: Vec::new(),
        };
        let pointer_bytes = pointer.serialize();

        let result = smudge_content(
            &pointer_bytes,
            "model.bin",
            false,
            &LfsStoreSource::eager(None),
            &session,
            None,
            None,
        )
        .unwrap();

        assert_eq!(output_bytes(&result), pointer_bytes);
    }

    #[tokio::test]
    async fn lfs_pointer_non_lazy_smudge_downloads_content() {
        use crate::core::config::{CheckoutConfig, Config};
        use crab_git::lfs_pointer::LfsPointer;
        use crab_storage::{RetryPolicy, Store};
        use object_store::memory::InMemory;
        use sha2::{Digest, Sha256};
        use tokio_util::sync::CancellationToken;

        let config = Config {
            checkout: CheckoutConfig { lazy: false },
            ..Config::default()
        };
        let ctx = AppContext::new(config, CancellationToken::new());

        // Set up an in-memory LFS object store with content.
        let original_content = b"hello LFS smudge world";
        let sha256_hash = Sha256::digest(original_content);
        let mut oid = [0u8; 32];
        oid.copy_from_slice(&sha256_hash);

        let inner: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
        let policy = RetryPolicy {
            max_attempts: 2,
            base: std::time::Duration::from_millis(1),
            cap: std::time::Duration::from_millis(5),
        };
        let store = Store::with_retry(inner, policy);
        let lfs_store = Arc::new(LfsObjectStore::new(store, "repo"));

        // Stage the content.
        lfs_store
            .put(&oid, bytes::Bytes::from(original_content.to_vec()))
            .await
            .unwrap();

        // Build a valid LFS pointer for this content.
        let lfs_pointer = LfsPointer {
            oid,
            size: original_content.len() as u64,
            extensions: Vec::new(),
        };
        let pointer_bytes = lfs_pointer.serialize();

        let mut input = build_handshake_input();
        input.extend(pkt_text("command=smudge"));
        input.extend(pkt_text("pathname=model.bin"));
        input.extend(pkt_flush());
        input.extend(pkt_data(&pointer_bytes));
        input.extend(pkt_flush());

        let lfs_store_clone = Some(Arc::clone(&lfs_store));
        let mut output = Vec::new();

        // Run inside spawn_blocking since smudge_content uses block_on.
        tokio::task::spawn_blocking(move || {
            run_filter_loop(
                &mut &input[..],
                &mut output,
                ctx,
                Arc::new(std::sync::Mutex::new(LazyStaging::Unavailable)),
                lfs_store_clone,
                None,
                None,
                None,
                Arc::new(std::sync::Mutex::new(None)),
            )
            .unwrap();

            let output_str = String::from_utf8_lossy(&output);
            assert!(output_str.contains("status=success"));

            // The output should contain the original content, not the pointer.
            assert!(
                output
                    .windows(original_content.len())
                    .any(|w| w == original_content),
                "non-lazy LFS smudge should return the original file content"
            );

            // The output should NOT contain the LFS pointer version line.
            assert!(
                !output
                    .windows(b"version https://git-lfs.github.com/spec/v1".len())
                    .any(|w| w == b"version https://git-lfs.github.com/spec/v1"),
                "non-lazy LFS smudge should not return the pointer"
            );
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn lfs_process_smudge_honors_fetch_filters() {
        use crab_git::lfs_pointer::LfsPointer;
        use crab_storage::{RetryPolicy, Store};
        use object_store::memory::InMemory;
        use sha2::{Digest, Sha256};

        let repo = tempfile::tempdir().unwrap();
        std::fs::write(
            repo.path().join(".lfsconfig"),
            "[lfs]\n    fetchinclude = allowed\n",
        )
        .unwrap();

        let mut session = super::super::clean::CleanSession::new(AppContext::default());
        session.set_repo_root(repo.path().to_path_buf());

        let original_content = b"filtered LFS object content";
        let sha256_hash = Sha256::digest(original_content);
        let mut oid = [0u8; 32];
        oid.copy_from_slice(&sha256_hash);

        let inner: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
        let policy = RetryPolicy {
            max_attempts: 2,
            base: std::time::Duration::from_millis(1),
            cap: std::time::Duration::from_millis(5),
        };
        let store = Store::with_retry(inner, policy);
        let lfs_store = Arc::new(LfsObjectStore::new(store, "repo"));
        lfs_store
            .put(&oid, bytes::Bytes::from_static(original_content))
            .await
            .unwrap();

        let pointer_bytes = LfsPointer {
            oid,
            size: original_content.len() as u64,
            extensions: Vec::new(),
        }
        .serialize();

        tokio::task::spawn_blocking(move || {
            let excluded = smudge_content(
                &pointer_bytes,
                "blocked/model.bin",
                false,
                &LfsStoreSource::eager(Some(Arc::clone(&lfs_store))),
                &session,
                None,
                None,
            )
            .unwrap();
            assert_eq!(output_bytes(&excluded), pointer_bytes);

            let included = smudge_content(
                &pointer_bytes,
                "allowed/model.bin",
                false,
                &LfsStoreSource::eager(Some(Arc::clone(&lfs_store))),
                &session,
                None,
                None,
            )
            .unwrap();
            assert_eq!(output_bytes(&included), original_content);
        })
        .await
        .unwrap();
    }

    #[test]
    fn non_pointer_content_passes_through_unchanged() {
        let ctx = AppContext::default();

        let content = b"this is just regular file content, not a pointer";

        let mut input = build_handshake_input();
        input.extend(pkt_text("command=smudge"));
        input.extend(pkt_text("pathname=readme.txt"));
        input.extend(pkt_flush());
        input.extend(pkt_data(content));
        input.extend(pkt_flush());

        let mut output = Vec::new();
        run_filter_loop(
            &mut &input[..],
            &mut output,
            ctx,
            Arc::new(std::sync::Mutex::new(LazyStaging::Unavailable)),
            None,
            None,
            None,
            None,
            Arc::new(std::sync::Mutex::new(None)),
        )
        .unwrap();

        let output_str = String::from_utf8_lossy(&output);
        assert!(output_str.contains("status=success"));

        // Non-pointer content should pass through unchanged.
        assert!(
            output.windows(content.len()).any(|w| w == content),
            "non-pointer content should pass through unchanged"
        );
    }

    #[test]
    fn large_non_pointer_smudge_roundtrips_after_request_flush() {
        let body = (0..(MAX_LFS_POINTER_SIZE * 3 + 17))
            .map(|idx| (idx % 251) as u8)
            .collect::<Vec<_>>();
        let mut input = build_handshake_input();
        input.extend(pkt_text("command=smudge"));
        input.extend(pkt_text("pathname=large.bin"));
        input.extend(pkt_flush());
        for chunk in body.chunks(7) {
            input.extend(pkt_data(chunk));
        }
        input.extend(pkt_flush());

        let mut output = Vec::new();
        run_filter_loop(
            &mut &input[..],
            &mut output,
            AppContext::default(),
            Arc::new(std::sync::Mutex::new(LazyStaging::Unavailable)),
            None,
            None,
            None,
            None,
            Arc::new(std::sync::Mutex::new(None)),
        )
        .unwrap();

        let status = output
            .windows(b"status=success".len())
            .position(|window| window == b"status=success")
            .unwrap();
        let content = output
            .windows(body.len())
            .position(|window| window == body)
            .unwrap();
        assert!(status < content);
    }

    #[test]
    fn file_index_checker_router_preserves_storage_scope() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let inner: Arc<dyn object_store::ObjectStore> =
            Arc::new(object_store::memory::InMemory::new());
        let scope = crate::auth::StorageScope {
            repo_prefix: "scoped/repo".to_owned(),
            global_prefix: "scoped/global".to_owned(),
            source_repo: "org/repo".to_owned(),
            scope_hash: "scope-hash".to_owned(),
        };
        let store = crate::storage::store::Store::new(inner).with_storage_scope(scope);

        let router = file_index_checker_router(
            store,
            "org/repo".to_owned(),
            &AppContext::default(),
            rt.handle(),
        );

        assert_eq!(router.repo_prefix(), "scoped/repo");
        assert_eq!(router.global_prefix(), "scoped/global");
    }

    // --- PktLineReader tests ---

    #[test]
    fn malformed_content_ends_session_without_a_response() {
        for malformed in [b"zzzz".as_slice(), b"0008ab", b"000"] {
            let mut input = build_handshake_input();
            input.extend(pkt_text("command=smudge"));
            input.extend(pkt_text("pathname=broken.txt"));
            input.extend(pkt_flush());
            input.extend(malformed);
            let mut output = Vec::new();
            let result = run_filter_loop(
                &mut &input[..],
                &mut output,
                AppContext::default(),
                Arc::new(std::sync::Mutex::new(LazyStaging::Unavailable)),
                None,
                None,
                None,
                None,
                Arc::new(std::sync::Mutex::new(None)),
            );
            let mut expected = Vec::new();
            handshake(&mut &build_handshake_input()[..], &mut expected).unwrap();
            assert!(result.is_err() && output == expected);
        }
    }

    #[test]
    fn pkt_line_reader_terminal_states_do_not_consume_another_packet() {
        for terminal in [b"0000", b"zzzz"] {
            let mut input = terminal.to_vec();
            input.extend(pkt_data(b"next request"));
            let mut cursor = io::Cursor::new(input);
            {
                let mut reader = PktLineReader::from_read(&mut cursor);
                for _ in 0..2 {
                    match reader.read_packet() {
                        Ok(None) if terminal == b"0000" => {}
                        Err(_) if terminal == b"zzzz" => {}
                        other => panic!("unexpected terminal result: {other:?}"),
                    }
                }
            }
            assert_eq!(cursor.position(), 4);
        }
    }

    #[test]
    fn pkt_line_reader_does_not_resume_after_a_partial_read_panic() {
        struct PanickingRead<'a>(&'a mut io::Cursor<Vec<u8>>);
        impl Read for PanickingRead<'_> {
            fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
                let count = buf.len().min(2);
                self.0.read_exact(&mut buf[..count])?;
                panic!("injected partial packet read panic");
            }
        }
        let mut cursor = io::Cursor::new(pkt_data(b"payload"));
        {
            let mut reader = PktLineReader::from_read(PanickingRead(&mut cursor));
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = reader.read_packet();
            }));
            assert!(result.is_err());
            assert!(reader.read_packet().is_err());
        }
        assert_eq!(cursor.position(), 2);
    }

    #[test]
    fn pkt_line_reader_reads_single_data_packet() {
        let mut input = Vec::new();
        input.extend(pkt_data(b"hello"));
        input.extend(pkt_flush());

        let mut reader = PktLineReader::from_slice(&input);
        let body = reader.read_packet().unwrap().unwrap().to_vec();
        assert_eq!(body, b"hello");
        assert!(reader.read_packet().unwrap().is_none());
    }

    #[test]
    fn pkt_line_reader_reads_multiple_packets_then_flush() {
        let mut input = Vec::new();
        input.extend(pkt_data(b"one"));
        input.extend(pkt_data(b"two"));
        input.extend(pkt_data(b"three"));
        input.extend(pkt_flush());

        let mut reader = PktLineReader::from_slice(&input);
        let first = reader.read_packet().unwrap().unwrap().to_vec();
        let second = reader.read_packet().unwrap().unwrap().to_vec();
        let third = reader.read_packet().unwrap().unwrap().to_vec();
        assert_eq!(first, b"one");
        assert_eq!(second, b"two");
        assert_eq!(third, b"three");
        assert!(reader.read_packet().unwrap().is_none());
    }

    #[test]
    fn pkt_line_reader_flush_only_stream() {
        let input = pkt_flush();
        let mut reader = PktLineReader::from_slice(&input);
        assert!(reader.read_packet().unwrap().is_none());
    }

    #[test]
    fn pkt_line_reader_rejects_non_hex_header() {
        // Length header contains bytes outside ASCII hex ('z' is 0x7A).
        let input = b"zzzzbody";
        let mut reader = PktLineReader::from_slice(&input[..]);
        let err = reader.read_packet().unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("pkt-line length header") || msg.contains("hex"),
            "expected framing error, got: {msg}"
        );
    }

    #[test]
    fn pkt_line_reader_rejects_truncated_body() {
        // Header says 100 bytes total (96 body), but only 10 are present.
        let mut input = Vec::new();
        input.extend_from_slice(b"0064"); // 0x64 = 100
        input.extend_from_slice(&[0u8; 10]);

        let mut reader = PktLineReader::from_slice(&input);
        let err = reader.read_packet().unwrap_err();
        assert!(
            matches!(err, CrabError::Io(_)),
            "expected I/O error on short read, got: {err}"
        );
    }

    #[test]
    fn pkt_line_reader_rejects_header_shorter_than_four() {
        // Length header `0003` < 4 is structurally impossible (header
        // itself is 4 bytes). Malformed framing.
        let input = b"0003";
        let mut reader = PktLineReader::from_slice(&input[..]);
        let err = reader.read_packet().unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("shorter than header") || msg.contains("length"),
            "expected length-too-small error, got: {msg}"
        );
    }

    #[test]
    fn pkt_line_reader_handles_max_size_packet() {
        // Maximum allowed body is 65_516 bytes (fff0 total - 4 header).
        let body = vec![0xABu8; PKT_LINE_MAX_BODY];
        let mut input = Vec::new();
        input.extend_from_slice(b"fff0");
        input.extend_from_slice(&body);
        input.extend(pkt_flush());

        let mut reader = PktLineReader::from_slice(&input);
        let got = reader.read_packet().unwrap().unwrap().to_vec();
        assert_eq!(got.len(), PKT_LINE_MAX_BODY);
        assert_eq!(got, body);
        assert!(reader.read_packet().unwrap().is_none());
    }

    #[test]
    fn pkt_line_reader_from_read_matches_from_slice() {
        use std::io::Cursor;

        let mut input = Vec::new();
        input.extend(pkt_data(b"alpha"));
        input.extend(pkt_data(b"beta"));
        input.extend(pkt_flush());

        let mut via_slice = PktLineReader::from_slice(&input);
        let mut via_read = PktLineReader::from_read(Cursor::new(input.clone()));

        loop {
            let a = via_slice.read_packet().unwrap().map(<[u8]>::to_vec);
            let b = via_read.read_packet().unwrap().map(<[u8]>::to_vec);
            assert_eq!(a, b);
            if a.is_none() {
                break;
            }
        }
    }

    /// The idle-timeout guard must exit the loop when git stops sending
    /// commands without closing stdin (SIGKILL, crash, IDE pipe leak).
    /// Without it, the process parks in `read_exact` forever holding the
    /// staging flock. We feed a complete command, leave the pipe open but
    /// silent, and assert the loop returns within a few seconds — far
    /// below "forever".
    #[cfg(unix)]
    #[test]
    fn idle_timeout_exits_when_pipe_stays_open_but_silent() {
        use std::io::BufReader;
        use std::os::unix::io::AsRawFd;
        use std::os::unix::net::UnixStream;
        use std::sync::mpsc;
        use std::time::Duration;

        let (read_end, mut write_end) = UnixStream::pair().expect("socketpair");

        // Feed a complete handshake + one clean command so the loop
        // processes at least one command before going idle.
        let mut input = build_handshake_input();
        input.extend(pkt_text("command=clean"));
        input.extend(pkt_text("pathname=test.bin"));
        input.extend(pkt_flush());
        input.extend(pkt_data(b"content"));
        input.extend(pkt_flush());
        write_end
            .write_all(&input)
            .expect("write handshake+command");
        write_end.flush().expect("flush");

        read_end.set_nonblocking(false).expect("set blocking");

        // Capture the fd before moving the stream into the idle wrapper.
        // The wrapper times out only when the buffered reader needs more
        // bytes from the pipe, so prefetched command bytes stay visible.
        let read_fd = read_end.as_raw_fd();
        let idle_timeout = Duration::from_millis(200);

        let (tx, rx) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            let mut output: Vec<u8> = Vec::new();
            let ctx = AppContext::default();
            let input = IdleRead::new(read_end, read_fd, idle_timeout);
            let result = run_filter_loop(
                BufReader::new(input),
                &mut output,
                ctx,
                Arc::new(std::sync::Mutex::new(LazyStaging::Unavailable)),
                None,
                None,
                None,
                None,
                Arc::new(std::sync::Mutex::new(None)),
            );
            let _ = tx.send(result);
        });

        // If the idle guard is missing, recv blocks forever and the test
        // times out at the harness level.
        let result = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("filter loop did not exit within 5s — idle timeout missing?");
        thread.join().expect("loop thread panicked");
        result.expect("loop should exit cleanly on idle timeout");
    }
}
