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
use crab_git::pointer_detect::{PointerKind, classify};
use crab_lfs::LfsObjectStore;
use crab_staging::StagingArea;
use crab_xet::hash::MerkleHash;

/// Protocol version string sent by git during the handshake.
const GIT_FILTER_CLIENT: &str = "git-filter-client";

/// Protocol version we declare.
const FILTER_PROTOCOL_VERSION: &str = "version=2";

/// Capabilities we advertise to git.
const CAPABILITIES: &[&str] = &["clean", "smudge", "delay"];
const SPECULATION_DECAY_WINDOW_MS: i64 = 30 * 24 * 60 * 60 * 1000;

#[derive(Debug, Clone, PartialEq, Eq)]
struct FilterWorktreePaths {
    current_worktree_root: PathBuf,
    shared_staging_root: PathBuf,
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
    hydrator: Option<Arc<crate::cmd::hydrate::ShardHydrator>>,
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
        let output = BufWriter::with_capacity(256 * 1024, output);
        run_filter_loop(
            input,
            output,
            ctx,
            staging_cell_clone,
            lfs_store,
            prefetch_clone,
            hydrator_clone,
            Some(handle_clone),
            speculation_cell_clone,
        )
    })
    .await
    .map_err(|e| CrabError::Internal(format!("filter process task panicked: {e}")))?;

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
    hydrator: Option<Arc<crate::cmd::hydrate::ShardHydrator>>,
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
            Box::pin(async move {
                // Read the pointer from disk and reconstruct.
                let content = tokio::fs::read(&path).await.map_err(|e| {
                    CrabError::Internal(format!(
                        "speculation: failed to read pointer at {path}: {e}"
                    ))
                })?;
                h.reconstruct_from_pointer(&content).await?;
                Ok(())
            })
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
            // If the file doesn't exist or can't be read, treat as not hydrated.
            let Ok(bytes) = std::fs::read(&full) else {
                return false;
            };
            // A file is hydrated if it's NOT a recognized pointer.
            matches!(classify(&bytes), PointerKind::NotAPointer)
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
    command: String,
    pathname: String,
    /// Additional key=value metadata from the command header. Used to
    /// observe the `can-delay=1` capability that git sets on smudge
    /// commands eligible for delayed processing.
    metadata: HashMap<String, String>,
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
fn run_filter_loop<R: BufRead, W: Write>(
    mut input: R,
    mut output: W,
    ctx: AppContext,
    staging_cell: Arc<std::sync::Mutex<LazyStaging>>,
    lfs_store: Option<Arc<LfsObjectStore>>,
    prefetch: Option<Arc<PrefetchQueue>>,
    hydrator: Option<Arc<crate::cmd::hydrate::ShardHydrator>>,
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

    // Load the `file_hash → shard_hash` map populated by previous pushes
    // so emitted pointers carry `shard-hint` and hydration can skip the
    // file-index GET.
    session.load_shard_hints_from_cache();
    let mut file_index_checker_attempted = false;

    // Configure LFS support: set the current worktree root for
    // .gitattributes lookup and LFS store for content staging.
    if let Some(root) = resolve_current_worktree_root() {
        session.set_repo_root(root);
    }
    if let Some(ref store) = lfs_store {
        session.set_lfs_store(Arc::clone(store));
    }

    loop {
        // Check for cancellation between operations.
        ctx.check_cancelled()?;

        let cmd = match read_command(&mut input) {
            Ok(Some(cmd)) => cmd,
            Ok(None) => break, // EOF — git closed stdin
            Err(e) => {
                tracing::warn!(error = %e, "failed to read filter command, ending session");
                break;
            }
        };

        if cmd.command == "clean" && !file_index_checker_attempted {
            file_index_checker_attempted = true;
            if let Some(handle) = handle.as_ref() {
                install_clean_file_index_checker(&mut session, &ctx, handle);
            }
        }

        // Session isolation: catch panics per-operation so one failure
        // doesn't corrupt session state for subsequent operations.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            dispatch_command(
                &cmd,
                &mut input,
                &mut output,
                &mut session,
                &ctx,
                &staging_cell,
                lfs_store.as_ref(),
                prefetch.as_ref(),
                hydrator.as_ref(),
                handle.as_ref(),
                &speculation,
            )
        }));

        match result {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::error!(
                    command = %cmd.command,
                    path = %cmd.pathname,
                    error = %e,
                    "filter operation failed"
                );
                session.reset_transient_state();
                // Drain any content that was still buffered in the input
                // stream. If dispatch_command failed before
                // read_content_until_flush could complete (e.g., panic
                // mid-parse or early error), leftover data packets would
                // be misread as the next command header. See CR4-F4.
                drain_until_flush(&mut input);
                write_status(&mut output, "error")?;
                write_flush(&mut output)?;
                // Flush the BufWriter so git sees the error response
                // immediately. Without this, the status=error bytes sit
                // in the 256 KiB buffer and git blocks on stdin — most
                // visible when the error occurs on the last file in a
                // batch. See finding S1-P4-1.
                output.flush().map_err(CrabError::Io)?;
            }
            Err(panic_info) => {
                let msg = panic_payload_to_string(&panic_info);
                tracing::error!(
                    command = %cmd.command,
                    path = %cmd.pathname,
                    panic = %msg,
                    "filter operation panicked"
                );
                session.reset_transient_state();
                drain_until_flush(&mut input);
                write_status(&mut output, "error")?;
                write_flush(&mut output)?;
                // See comment above — same flush requirement on the panic path.
                output.flush().map_err(CrabError::Io)?;
            }
        }
    }

    // Persist the bloom filter so the next filter-process session
    // starts with a warm fast-path.
    session.save_bloom_to_cache();

    // Flush any hydrated-pointer cache invalidations collected
    // during this session so the next process doesn't waste a
    // `matches_stat` call on the same stale entries.
    session.persist_hydrated_cache_invalidations();

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

    let router = file_index_checker_router(
        selection.store,
        selection.router.repo_prefix().to_owned(),
        ctx,
        handle,
    );
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
fn read_command<R: Read>(input: &mut R) -> Result<Option<FilterCommand>> {
    // First line is "command=<cmd>".
    let Some(command_line) = read_text_line(input)? else {
        return Ok(None);
    };

    let command = command_line
        .strip_prefix("command=")
        .ok_or_else(|| {
            CrabError::Protocol(format!("expected 'command=...', got '{command_line}'"))
        })?
        .to_owned();

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
    input: &mut R,
    output: &mut W,
    session: &mut super::clean::CleanSession,
    ctx: &AppContext,
    staging_cell: &Arc<std::sync::Mutex<LazyStaging>>,
    lfs_store: Option<&Arc<LfsObjectStore>>,
    prefetch: Option<&Arc<PrefetchQueue>>,
    hydrator: Option<&Arc<crate::cmd::hydrate::ShardHydrator>>,
    handle: Option<&tokio::runtime::Handle>,
    speculation: &Arc<std::sync::Mutex<Option<Arc<SpeculationState>>>>,
) -> Result<()> {
    match cmd.command.as_str() {
        "clean" => {
            // Resolve ownership before touching XET staging. LFS has its own
            // cache and remote publication path and must never contend on the
            // XET staging lock in a mixed-repository filter session.
            let is_lfs = session.resolve_filter_for(&cmd.pathname)
                == Some(crate::git::filter_attr_cache::FilterKind::Lfs);
            // Fast-fast path: if the hydrated-pointer cache already
            // has a live entry for this pathname, the upcoming clean
            // will be served from cache without needing staging.
            // Skip `acquire_writer` entirely so we don't block on the
            // default flock budget when another crab process
            // (shell-prompt `git status`, IDE integration, concurrent
            // `crab add`) holds `.crab/staging`.
            //
            // Stat mismatches are caught inside `clean_stream`'s
            // cache lookup, so a stale entry falls through to the
            // normal pipeline.
            let cache_short_circuit = session.has_live_hydrated_entry(&cmd.pathname);

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
            } else if cache_short_circuit {
                // Don't mutate the staging-unavailable flag here — if
                // a prior clean already opened staging successfully,
                // keep the writer attached so subsequent non-cached
                // cleans in this session reuse it. If no prior clean
                // acquired a writer and this one hits the cache, the
                // session stays in whatever state it was seeded with;
                // should a later clean on a different pathname miss
                // the cache, its branch will call `acquire_writer`
                // and transition the cell then.
                tracing::debug!(
                    path = %cmd.pathname,
                    "filter clean: hydrated-pointer cache hit, skipping staging flock"
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
            // internal window instead of the full file payload. The fresh
            // reader releases its borrow on `input` as soon as `clean_stream`
            // returns, so the session-isolation wrapper's
            // `drain_until_flush` on the error path still works.
            let mut reader = PktLineReader::from_read(&mut *input);
            let pointer_bytes = session.clean_stream(&cmd.pathname, &mut reader)?;

            // Response: status list + flush, content + flush, empty list + flush.
            write_status(output, "success")?;
            write_flush(output)?;
            write_content(output, &pointer_bytes)?;
            write_flush(output)?;
            // Empty second status list, terminated by flush.
            write_flush(output)?;
            output.flush().map_err(CrabError::Io)?;
        }
        "smudge" => {
            let lazy = ctx.config().checkout.lazy;

            // Packet boundaries are transport details: Git may split even a
            // small pointer across multiple frames. Accumulate only while
            // the bytes can still be a pointer, then stream raw content.
            let probe = read_smudge_probe(input)?;
            if probe.bytes.is_empty() && probe.ended {
                // Empty blob: respond with zero content frames so the
                // wire output stays byte-identical to the pre-streaming
                // writer for empty tracked files.
                write_status(output, "success")?;
                write_flush(output)?;
                write_flush(output)?;
                write_flush(output)?;
                output.flush().map_err(CrabError::Io)?;
                return Ok(());
            }
            let kind = classify(&probe.bytes);

            // Speculative hydration bookkeeping for crab pointers.
            // Errors are swallowed — speculation must never break smudge.
            if ctx.config().hydrate.speculative
                && let PointerKind::Crab(_) = kind
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

                    let driver_for_hit = Arc::clone(&state.driver);
                    let hit_path = pathname.clone();
                    h.block_on(async move {
                        driver_for_hit.record_hit_if_speculative(&hit_path).await;
                    });

                    state
                        .access_db
                        .record_access_fire_and_forget(pathname.clone(), ts_ms, run_id);

                    let driver = Arc::clone(&state.driver);
                    h.block_on(async move {
                        driver.launch_speculative(&pathname).await;
                    });
                }
            }

            // Delayed-smudge fast path: queue background reconstruction;
            // git collects the bytes later via list_available_blobs.
            if cmd.can_delay()
                && let (Some(pf), Some(h)) = (prefetch, handle)
                && let PointerKind::Crab(pointer) = kind
            {
                let file_hash = MerkleHash::from(pointer.file_hash);
                let shard_hint = pointer.shard_hint.map(MerkleHash::from);
                let pathname = cmd.pathname.clone();
                let pf = pf.clone();
                h.block_on(async move {
                    pf.submit_with_hint(pathname, file_hash, shard_hint).await;
                });

                if !probe.ended {
                    drain_until_flush(input);
                }
                write_delayed_response(output)?;
                output.flush().map_err(CrabError::Io)?;
                return Ok(());
            }

            // Collection path for a previously delayed smudge.
            if let (Some(pf), Some(h)) = (prefetch, handle)
                && let Some(bytes) = h.block_on(async {
                    match pf.take_result(&cmd.pathname).await {
                        Ok(bytes) => Some(bytes),
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
                if !probe.ended {
                    drain_until_flush(input);
                }
                write_status(output, "success")?;
                write_flush(output)?;
                write_content(output, &bytes)?;
                write_flush(output)?;
                write_flush(output)?;
                output.flush().map_err(CrabError::Io)?;
                return Ok(());
            }

            // Transform decision on the bounded probe only. Every
            // branch either produces buffered bytes (pointers resolve
            // to small results or spill to disk first) or streams the
            // input straight through — no path holds a whole large
            // blob in memory.
            let resolved_filter = session.resolve_filter_for(&cmd.pathname);

            let outcome = match resolved_filter {
                Some(crate::git::filter_attr_cache::FilterKind::Lfs) => {
                    if lazy {
                        SmudgeOutcome::Passthrough
                    } else if !session.should_lfs_smudge(&cmd.pathname) {
                        SmudgeOutcome::Passthrough
                    } else if let Ok(pointer) =
                        crab_git::lfs_pointer::LfsPointer::parse(&probe.bytes)
                    {
                        match lfs_pointer_bytes(
                            &pointer,
                            &cmd.pathname,
                            lfs_store,
                            session.repo_root(),
                        ) {
                            Ok(Some(bytes)) => SmudgeOutcome::Buffered(bytes),
                            Ok(None) | Err(_) => SmudgeOutcome::Passthrough,
                        }
                    } else {
                        SmudgeOutcome::Passthrough
                    }
                }
                Some(crate::git::filter_attr_cache::FilterKind::Crab) => {
                    if lazy && !session.should_auto_hydrate(&cmd.pathname) {
                        SmudgeOutcome::Passthrough
                    } else if let (Some(hydrator), Some(h)) = (hydrator, handle)
                        && let Ok(_pointer) = crab_types::pointer::Pointer::parse(&probe.bytes)
                    {
                        match reconstruct_spilled(hydrator, h, &probe.bytes) {
                            Ok(path) => SmudgeOutcome::Spilled(path),
                            Err(e) => {
                                tracing::warn!(
                                    path = %cmd.pathname,
                                    error = %e,
                                    "smudge: inline reconstruction failed; passing pointer through"
                                );
                                SmudgeOutcome::Passthrough
                            }
                        }
                    } else {
                        SmudgeOutcome::Passthrough
                    }
                }
                None => match kind {
                    PointerKind::Lfs(pointer) => {
                        if lazy || !session.should_lfs_smudge(&cmd.pathname) {
                            SmudgeOutcome::Passthrough
                        } else {
                            match lfs_pointer_bytes(
                                &pointer,
                                &cmd.pathname,
                                lfs_store,
                                session.repo_root(),
                            ) {
                                Ok(Some(bytes)) => SmudgeOutcome::Buffered(bytes),
                                Ok(None) | Err(_) => SmudgeOutcome::Passthrough,
                            }
                        }
                    }
                    PointerKind::Crab(_pointer) => {
                        if lazy && !session.should_auto_hydrate(&cmd.pathname) {
                            SmudgeOutcome::Passthrough
                        } else if let (Some(hydrator), Some(h)) = (hydrator, handle) {
                            match reconstruct_spilled(hydrator, h, &probe.bytes) {
                                Ok(path) => SmudgeOutcome::Spilled(path),
                                Err(e) => {
                                    tracing::warn!(
                                        path = %cmd.pathname,
                                        error = %e,
                                        "smudge: inline reconstruction failed; passing pointer through"
                                    );
                                    SmudgeOutcome::Passthrough
                                }
                            }
                        } else {
                            SmudgeOutcome::Passthrough
                        }
                    }
                    PointerKind::NotAPointer => SmudgeOutcome::Passthrough,
                },
            };

            match outcome {
                SmudgeOutcome::Buffered(bytes) => {
                    if !probe.ended {
                        drain_until_flush(input);
                    }
                    buffered_response(output, &bytes)?;
                }
                SmudgeOutcome::Spilled(file) => {
                    if !probe.ended {
                        drain_until_flush(input);
                    }
                    streamed_file_response(output, file.path())?;
                }
                SmudgeOutcome::Passthrough => {
                    passthrough_response(input, output, &probe.bytes, probe.ended)?;
                }
            }
        }
        "list_available_blobs" => {
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
        other => {
            tracing::warn!(command = %other, "unknown filter command");
            write_status(output, "error")?;
            write_flush(output)?;
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

enum SmudgeOutcome {
    Buffered(Vec<u8>),
    Spilled(tempfile::NamedTempFile),
    Passthrough,
}

fn passthrough_response<R: Read, W: Write>(
    input: &mut R,
    output: &mut W,
    prefix: &[u8],
    input_ended: bool,
) -> Result<()> {
    write_status(output, "success")?;
    write_flush(output)?;
    write_content(output, prefix)?;
    if !input_ended {
        stream_remaining_packets(input, output)?;
    }
    write_flush(output)?;
    write_flush(output)?;
    output.flush().map_err(CrabError::Io)
}

fn buffered_response<W: Write>(output: &mut W, bytes: &[u8]) -> Result<()> {
    write_status(output, "success")?;
    write_flush(output)?;
    write_content(output, bytes)?;
    write_flush(output)?;
    write_flush(output)?;
    output.flush().map_err(CrabError::Io)
}

fn streamed_file_response<W: Write>(output: &mut W, path: &Path) -> Result<()> {
    write_status(output, "success")?;
    write_flush(output)?;
    let mut file = std::fs::File::open(path).map_err(CrabError::Io)?;
    let mut chunk = vec![0u8; 256 * 1024];
    loop {
        let n = file.read(&mut chunk).map_err(CrabError::Io)?;
        if n == 0 {
            break;
        }
        write_content(output, &chunk[..n])?;
    }
    write_flush(output)?;
    write_flush(output)?;
    output.flush().map_err(CrabError::Io)
}

/// Reconstruct completely before responding so failure can still fall back
/// to the pointer. The named file remains self-cleaning across every return.
fn reconstruct_spilled(
    hydrator: &Arc<crate::cmd::hydrate::ShardHydrator>,
    handle: &tokio::runtime::Handle,
    pointer_bytes: &[u8],
) -> Result<tempfile::NamedTempFile> {
    let tmp = tempfile::NamedTempFile::new().map_err(CrabError::Io)?;
    handle.block_on(hydrator.reconstruct_from_pointer_to_path(pointer_bytes, tmp.path()))?;
    Ok(tmp)
}

fn lfs_pointer_bytes(
    pointer: &crab_git::lfs_pointer::LfsPointer,
    pathname: &str,
    lfs_store: Option<&Arc<LfsObjectStore>>,
    repo_root: Option<&Path>,
) -> Result<Option<Vec<u8>>> {
    let oid_hex = crab_git::lfs_pointer::hex_encode(&pointer.oid);
    if let Some(local) = try_local_lfs_cache(pointer, repo_root)? {
        return crate::lfs::extension::smudge_content(pointer, local, pathname).map(Some);
    }
    if let Some(store) = lfs_store {
        tracing::debug!(oid = %oid_hex, size = pointer.size, "smudge: downloading LFS object from remote");
        let rt = tokio::runtime::Handle::current();
        let bytes = rt.block_on(store.verify(&pointer.oid))?;
        cache_lfs_locally(pointer, &bytes, repo_root)?;
        return crate::lfs::extension::smudge_content(pointer, bytes.to_vec(), pathname).map(Some);
    }
    tracing::warn!(
        oid = %oid_hex,
        "smudge: LFS object not in local cache and no remote store available"
    );
    Ok(None)
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
    match input.read_exact(&mut buf) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(CrabError::Io(e)),
    }

    // Check for flush/delimiter/response-end.
    if &buf == b"0000" || &buf == b"0001" || &buf == b"0002" {
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

const POINTER_PROBE_LIMIT: usize = 8 * 1024;
const POINTER_PROBE_PACKET_LIMIT: usize = 128;
const CRAB_POINTER_HEADER: &[u8] = b"version https://crab.dev/spec/v1\n";
const LFS_POINTER_HEADER: &[u8] = b"version https://git-lfs.github.com/spec/v1\n";

struct SmudgeProbe {
    bytes: Vec<u8>,
    /// The content flush was consumed while probing.
    ended: bool,
}

fn could_be_pointer(bytes: &[u8]) -> bool {
    [CRAB_POINTER_HEADER, LFS_POINTER_HEADER]
        .iter()
        .any(|header| header.starts_with(bytes) || bytes.starts_with(header))
}

/// Read only enough content packets to classify a possible pointer.
///
/// A pointer may span arbitrary pkt-line boundaries. Raw content stops
/// accumulating as soon as its prefix cannot be a supported pointer, while
/// pointer-shaped content is capped to a small fixed probe budget.
fn read_smudge_probe<R: Read>(input: &mut R) -> Result<SmudgeProbe> {
    let mut bytes = Vec::with_capacity(1024);
    let mut packet = Vec::new();
    let mut packets = 0usize;
    loop {
        match read_packet_payload(input, &mut packet)? {
            PacketEnd::Flush | PacketEnd::Eof => return Ok(SmudgeProbe { bytes, ended: true }),
            PacketEnd::Data(()) => {
                packets += 1;
                bytes.extend_from_slice(&packet);
                packet.clear();
                if bytes.len() > POINTER_PROBE_LIMIT
                    || packets >= POINTER_PROBE_PACKET_LIMIT
                    || !could_be_pointer(&bytes)
                {
                    return Ok(SmudgeProbe {
                        bytes,
                        ended: false,
                    });
                }
            }
        }
    }
}

/// Copy remaining content packets straight into pkt-line output frames.
///
/// The passthrough smudge path for raw multi-GiB blobs runs here: bytes
/// flow packet-to-packet without whole-file buffering.
fn stream_remaining_packets<R: Read, W: Write>(input: &mut R, output: &mut W) -> Result<()> {
    const MAX_CHUNK: usize = 65516;
    let mut buf = Vec::with_capacity(MAX_CHUNK);
    loop {
        match read_packet_payload(input, &mut buf)? {
            PacketEnd::Flush | PacketEnd::Eof => break,
            PacketEnd::Data(()) => {
                write_frame(output, &buf)?;
                buf.clear();
            }
        }
    }
    Ok(())
}

enum PacketEnd {
    Flush,
    Eof,
    Data(()),
}

/// Read one packet's payload into `buf` (replacing contents).
fn read_packet_payload<R: Read>(input: &mut R, buf: &mut Vec<u8>) -> Result<PacketEnd> {
    let mut hdr = [0u8; 4];
    match input.read_exact(&mut hdr) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(PacketEnd::Eof),
        Err(e) => return Err(CrabError::Io(e)),
    }

    if &hdr == b"0000" || &hdr == b"0001" || &hdr == b"0002" {
        return Ok(PacketEnd::Flush);
    }

    let hex = std::str::from_utf8(&hdr)
        .map_err(|_| CrabError::Protocol("invalid packet-line hex".into()))?;
    let len: usize = u16::from_str_radix(hex, 16)
        .map_err(|_| CrabError::Protocol(format!("invalid packet-line length: {hex}")))?
        .into();

    if len < 4 {
        return Err(CrabError::Protocol(format!(
            "packet-line length too small: {len}"
        )));
    }

    buf.resize(len - 4, 0);
    input.read_exact(buf).map_err(CrabError::Io)?;
    Ok(PacketEnd::Data(()))
}

/// Write one raw-data pkt-line frame.
fn write_frame<W: Write>(output: &mut W, data: &[u8]) -> Result<()> {
    let len = data.len() + 4;
    write!(output, "{len:04x}").map_err(CrabError::Io)?;
    output.write_all(data).map_err(CrabError::Io)?;
    Ok(())
}

/// Drain packet-lines from `input` until a flush/delimiter packet is seen
/// or EOF is reached. Used as a best-effort recovery after an error
/// dispatching a command, so the next `read_command` call starts at a
/// protocol boundary rather than in the middle of the previous command's
/// content packets.
fn drain_until_flush<R: Read>(input: &mut R) {
    let mut hdr = [0u8; 4];
    loop {
        match input.read_exact(&mut hdr) {
            Ok(()) => {}
            Err(_) => return, // EOF or I/O error — give up.
        }

        if &hdr == b"0000" || &hdr == b"0001" || &hdr == b"0002" {
            return;
        }

        let Ok(hex) = std::str::from_utf8(&hdr) else {
            return;
        };
        let Ok(len) = u16::from_str_radix(hex, 16).map(usize::from) else {
            return;
        };
        if len < 4 {
            return;
        }
        let mut discard = vec![0u8; len - 4];
        if input.read_exact(&mut discard).is_err() {
            return;
        }
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

/// Write a flush packet (0000).
fn write_flush<W: Write>(output: &mut W) -> Result<()> {
    output.write_all(b"0000").map_err(CrabError::Io)?;
    Ok(())
}

/// Try to read an LFS object from the local `.git/lfs/objects/` cache.
fn try_local_lfs_cache(
    pointer: &crab_git::lfs_pointer::LfsPointer,
    repo_root: Option<&Path>,
) -> Result<Option<Vec<u8>>> {
    let ctx = match repo_root {
        Some(root) => match WorktreeContext::resolve_from_path(root) {
            Ok(ctx) => ctx,
            Err(error) => {
                tracing::debug!(error = %error, "local LFS cache unavailable outside a Git worktree");
                return Ok(None);
            }
        },
        None => match WorktreeContext::resolve() {
            Ok(ctx) => ctx,
            Err(error) => {
                tracing::debug!(error = %error, "local LFS cache unavailable outside a Git worktree");
                return Ok(None);
            }
        },
    };
    match crate::lfs::cache::read_pointer(&ctx.common_git_dir.join("lfs"), pointer) {
        Err(CrabError::LfsObjectCorrupt { .. }) => Ok(None),
        result => result,
    }
}

/// Cache an LFS object in the local `.git/lfs/objects/` directory.
fn cache_lfs_locally(
    pointer: &crab_git::lfs_pointer::LfsPointer,
    content: &[u8],
    repo_root: Option<&Path>,
) -> Result<()> {
    let ctx = match repo_root {
        Some(root) => match WorktreeContext::resolve_from_path(root) {
            Ok(ctx) => ctx,
            Err(error) => {
                tracing::debug!(error = %error, "skipping local LFS cache outside a Git worktree");
                return Ok(());
            }
        },
        None => match WorktreeContext::resolve() {
            Ok(ctx) => ctx,
            Err(error) => {
                tracing::debug!(error = %error, "skipping local LFS cache outside a Git worktree");
                return Ok(());
            }
        },
    };
    crate::lfs::cache::install_bytes(
        &ctx.common_git_dir.join("lfs"),
        &pointer.oid,
        pointer.size,
        content,
    )?;
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
}

impl<R: Read> PktLineReader<R> {
    /// Construct a reader wrapping an arbitrary `Read` source.
    pub fn from_read(inner: R) -> Self {
        Self {
            inner,
            buf: Vec::with_capacity(PKT_LINE_MAX_BODY),
        }
    }

    /// Read the next pkt-line packet.
    ///
    /// Returns `Ok(Some(body))` for a data packet, `Ok(None)` for a flush
    /// packet (`0000`), or an error on I/O failure or malformed framing.
    ///
    /// The returned slice borrows from the reader's internal buffer and is
    /// invalidated by the next call to `read_packet`. Callers that need to
    /// retain packet bytes past the next read must copy them.
    pub fn read_packet(&mut self) -> Result<Option<&[u8]>> {
        let mut hdr = [0u8; 4];
        self.inner.read_exact(&mut hdr).map_err(CrabError::Io)?;

        // Flush and the other delimiter packets all have zero body length.
        // `read_content_until_flush` treats `0001` and `0002` as flush too,
        // but the clean-filter content stream only uses `0000`. Accepting
        // the same set here keeps behavior aligned.
        if &hdr == b"0000" || &hdr == b"0001" || &hdr == b"0002" {
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

        let body_len = len - 4;
        self.buf.clear();
        self.buf.resize(body_len, 0);
        self.inner
            .read_exact(&mut self.buf[..])
            .map_err(CrabError::Io)?;
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
    use tokio_util::sync::CancellationToken;

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
        let _cwd_serial = CWD_SWAP_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        struct RestoreCwd(std::path::PathBuf);
        impl Drop for RestoreCwd {
            fn drop(&mut self) {
                let _ = std::env::set_current_dir(&self.0);
            }
        }
        let _cwd_guard = RestoreCwd(std::env::current_dir().unwrap());
        std::env::set_current_dir(repo.path()).unwrap();

        // Empty blob arrives as zero content packets.
        let mut input = build_handshake_input();
        input.extend(pkt_text("command=smudge"));
        input.extend(pkt_text("pathname=empty.bin"));
        input.extend(pkt_flush());
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

        // Response framing: status frame, then three flushes — no data
        // frames, so the worktree file stays empty.
        let expected = {
            use std::io::Write as _;
            let mut e = Vec::new();
            let status = "status=success\n";
            write!(&mut e, "{:04x}", status.len() + 4).unwrap();
            e.extend_from_slice(status.as_bytes());
            e.extend_from_slice(b"0000".repeat(3).as_slice());
            e
        };
        assert!(
            output.ends_with(&expected),
            "empty blob must produce status + three flushes and no content"
        );
    }

    /// Serializes tests that swap the process cwd; concurrent relative-
    /// path git work otherwise observes a fixture repo and poisons
    /// shared lazy state across dozens of unrelated tests.
    static CWD_SWAP_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
        assert_eq!(cmd.command, "clean");
        assert_eq!(cmd.pathname, "large.bin");
    }

    #[test]
    fn read_command_returns_none_on_eof() {
        let input: &[u8] = &[];
        let cmd = read_command(&mut &*input).unwrap();
        assert!(cmd.is_none());
    }

    #[test]
    fn full_clean_session() {
        let mut input = build_handshake_input();
        input.extend(pkt_text("command=clean"));
        input.extend(pkt_text("pathname=test.bin"));
        input.extend(pkt_flush());
        input.extend(pkt_data(b"file content here"));
        input.extend(pkt_flush());

        let mut output = Vec::new();
        let staging_root = tempfile::tempdir().unwrap();
        let rt = tokio::runtime::Runtime::new().unwrap();
        run_filter_loop(
            &mut &input[..],
            &mut output,
            AppContext::default(),
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

        let output = String::from_utf8_lossy(&output);
        assert!(output.contains("git-filter-server"));
        assert!(output.contains("status=success"));
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
        assert!(!staging_root.exists());

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

        assert!(String::from_utf8_lossy(&output).contains("status=success"));
        assert!(staging_root.join("index.db").exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn filter_clean_waits_past_retired_short_flock_budget() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().to_path_buf();
        let first = StagingArea::open(root.clone()).await.unwrap();
        let cell = Arc::new(std::sync::Mutex::new(LazyStaging::Unopened {
            staging_root: root,
        }));

        let acquiring_cell = Arc::clone(&cell);
        let acquire_task = tokio::spawn(async move {
            let started = std::time::Instant::now();
            let result = acquire_writer(acquiring_cell.as_ref()).await;
            (result, started.elapsed())
        });
        tokio::time::sleep(std::time::Duration::from_millis(3_300)).await;
        assert!(!acquire_task.is_finished());
        first.close().await.unwrap();

        let (result, waited) = acquire_task.await.unwrap();
        let writer = match result {
            StagingAcquire::Writer(staging) => staging,
            StagingAcquire::Locked { holder_pid } => {
                panic!("filter clean timed out; holder_pid={holder_pid:?}")
            }
            StagingAcquire::Unavailable => panic!("filter clean failed to open staging"),
        };
        assert!(waited >= std::time::Duration::from_secs(3));
        drop(writer);

        let final_staging = {
            let mut guard = cell
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::mem::replace(&mut *guard, LazyStaging::Unavailable)
        };
        match final_staging {
            LazyStaging::Writer(staging) => {
                Arc::try_unwrap(staging)
                    .ok()
                    .unwrap()
                    .close()
                    .await
                    .unwrap();
            }
            _ => panic!("acquired writer should be cached"),
        }
    }

    #[test]
    fn smudge_probe_collects_a_pointer_across_packet_boundaries() {
        let pointer = b"version https://git-lfs.github.com/spec/v1\n\
oid sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\n\
size 5\n";
        let mut input = Vec::new();
        for chunk in pointer.chunks(7) {
            input.extend(pkt_data(chunk));
        }
        input.extend(pkt_flush());

        let probe = read_smudge_probe(&mut &input[..]).unwrap();
        assert!(probe.ended);
        assert_eq!(probe.bytes, pointer);
        assert!(matches!(classify(&probe.bytes), PointerKind::Lfs(_)));
    }

    #[test]
    fn smudge_probe_returns_empty_on_immediate_flush() {
        let mut input = Vec::new();
        input.extend(pkt_flush());

        let probe = read_smudge_probe(&mut &input[..]).unwrap();
        assert!(probe.ended);
        assert!(probe.bytes.is_empty());
    }

    #[test]
    fn stream_remaining_packets_frames_each_payload() {
        let mut input = Vec::new();
        input.extend(pkt_data(b"hello "));
        input.extend(pkt_data(b"world"));
        input.extend(pkt_flush());

        // A leading frame already emitted for the peeked first packet.
        let mut output = Vec::new();
        write_frame(&mut output, b"peek").unwrap();
        stream_remaining_packets(&mut &input[..], &mut output).unwrap();

        let text = String::from_utf8(output).unwrap();
        // Frames for "peek", "hello ", "world"; the caller owns the
        // terminating flush.
        let expected = {
            let mut e = Vec::new();
            write_frame(&mut e, b"peek").unwrap();
            write_frame(&mut e, b"hello ").unwrap();
            write_frame(&mut e, b"world").unwrap();
            String::from_utf8(e).unwrap()
        };
        assert_eq!(text, expected);
    }

    #[test]
    fn smudge_passthrough_streams_multi_packet_content() {
        let ctx = AppContext::default();

        // 200 KiB of non-pointer content spanning many packets: the old
        // implementation buffered all of it; the streaming path must
        // relay it unchanged.
        let content: Vec<u8> = (0..200 * 1024).map(|i| (i % 251) as u8).collect();
        let mut input = build_handshake_input();
        input.extend(pkt_text("command=smudge"));
        input.extend(pkt_text("pathname=big.bin"));
        input.extend(pkt_flush());
        for chunk in content.chunks(60000) {
            input.extend(pkt_data(chunk));
        }
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

        // The full response is exactly: status frame, content frames for
        // every input packet, then two flushes. Compare byte-for-byte so
        // any framing drift fails loudly.
        let mut expected = Vec::new();
        {
            use std::io::Write as _;
            let status = format!("status=success\n");
            let len = status.len() + 4;
            write!(&mut expected, "{len:04x}").unwrap();
            expected.extend_from_slice(status.as_bytes());
            expected.extend_from_slice(b"0000");
            for chunk in content.chunks(60000) {
                let len = chunk.len() + 4;
                write!(&mut expected, "{len:04x}").unwrap();
                expected.extend_from_slice(chunk);
            }
            expected.extend_from_slice(b"0000");
            expected.extend_from_slice(b"0000");
        }
        // The loop's handshake reply precedes the smudge response, so
        // compare as a suffix.
        assert!(
            output.ends_with(&expected),
            "passthrough relays content byte-identically with clean framing"
        );
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

    #[test]
    fn lazy_smudge_passes_pointer_through_unchanged() {
        use crate::core::config::{CheckoutConfig, Config};

        let ctx = AppContext::new(
            Config {
                checkout: CheckoutConfig { lazy: true },
                ..Config::default()
            },
            CancellationToken::new(),
        );
        let pointer = b"version https://crab.dev/spec/v1\n\
file-hash 000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f\n\
size 1048576\n";
        let mut input = build_handshake_input();
        input.extend(pkt_text("command=smudge"));
        input.extend(pkt_text("pathname=model.bin"));
        input.extend(pkt_flush());
        input.extend(pkt_data(pointer));
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

        assert!(output.windows(pointer.len()).any(|bytes| bytes == pointer));
    }

    #[test]
    fn lazy_false_does_not_short_circuit_smudge() {
        let content = b"some file content";
        let mut input = build_handshake_input();
        input.extend(pkt_text("command=smudge"));
        input.extend(pkt_text("pathname=file.txt"));
        input.extend(pkt_flush());
        input.extend(pkt_data(content));
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

        assert!(String::from_utf8_lossy(&output).contains("status=success"));
        assert!(output.windows(content.len()).any(|bytes| bytes == content));
    }

    #[test]
    fn lazy_smudge_does_not_affect_clean_path() {
        use crate::core::config::{CheckoutConfig, Config};

        let worktree = tempfile::tempdir().unwrap();
        let git_dir = worktree.path().join(".git");
        std::fs::create_dir_all(&git_dir).unwrap();
        let _git_env = GitEnvGuard::set(&git_dir, worktree.path(), &git_dir);
        let ctx = AppContext::new(
            Config {
                checkout: CheckoutConfig { lazy: true },
                ..Config::default()
            },
            CancellationToken::new(),
        );
        let mut input = build_handshake_input();
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

        let version = b"version https://crab.dev/spec/v1";
        assert!(output.windows(version.len()).any(|bytes| bytes == version));
    }

    #[test]
    fn lfs_pointer_lazy_smudge_passes_through() {
        use crate::core::config::{CheckoutConfig, Config};
        use crab_git::lfs_pointer::LfsPointer;

        let ctx = AppContext::new(
            Config {
                checkout: CheckoutConfig { lazy: true },
                ..Config::default()
            },
            CancellationToken::new(),
        );
        let pointer = LfsPointer {
            oid: [0xAB; 32],
            size: 1024,
            extensions: Vec::new(),
        }
        .serialize();
        let mut input = build_handshake_input();
        input.extend(pkt_text("command=smudge"));
        input.extend(pkt_text("pathname=model.bin"));
        input.extend(pkt_flush());
        input.extend(pkt_data(&pointer));
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

        assert!(
            output
                .windows(pointer.len())
                .any(|bytes| bytes == pointer.as_slice())
        );
    }

    #[tokio::test]
    async fn lfs_pointer_non_lazy_smudge_downloads_content() {
        use crab_git::lfs_pointer::LfsPointer;
        use crab_storage::{RetryPolicy, Store};
        use object_store::memory::InMemory;
        use sha2::{Digest, Sha256};

        let original = b"hello LFS smudge world";
        let digest = Sha256::digest(original);
        let mut oid = [0; 32];
        oid.copy_from_slice(&digest);
        let inner: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
        let store = Store::with_retry(
            inner,
            RetryPolicy {
                max_attempts: 2,
                base: std::time::Duration::from_millis(1),
                cap: std::time::Duration::from_millis(5),
            },
        );
        let lfs_store = Arc::new(LfsObjectStore::new(store, "repo"));
        lfs_store
            .put(&oid, bytes::Bytes::copy_from_slice(original))
            .await
            .unwrap();
        let pointer = LfsPointer {
            oid,
            size: original.len() as u64,
            extensions: Vec::new(),
        }
        .serialize();
        let mut input = build_handshake_input();
        input.extend(pkt_text("command=smudge"));
        input.extend(pkt_text("pathname=model.bin"));
        input.extend(pkt_flush());
        input.extend(pkt_data(&pointer));
        input.extend(pkt_flush());

        tokio::task::spawn_blocking(move || {
            let mut output = Vec::new();
            run_filter_loop(
                &mut &input[..],
                &mut output,
                AppContext::default(),
                Arc::new(std::sync::Mutex::new(LazyStaging::Unavailable)),
                Some(lfs_store),
                None,
                None,
                None,
                Arc::new(std::sync::Mutex::new(None)),
            )
            .unwrap();

            assert!(
                output
                    .windows(original.len())
                    .any(|bytes| bytes == original)
            );
            assert!(!output.windows(pointer.len()).any(|bytes| bytes == pointer));
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
        let git_dir = repo.path().join(".git");
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::write(
            repo.path().join(".lfsconfig"),
            "[lfs]\n    fetchinclude = allowed\n",
        )
        .unwrap();
        let _git_env = GitEnvGuard::set(&git_dir, repo.path(), &git_dir);

        let original = b"filtered LFS object content";
        let digest = Sha256::digest(original);
        let mut oid = [0; 32];
        oid.copy_from_slice(&digest);
        let inner: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
        let store = Store::with_retry(
            inner,
            RetryPolicy {
                max_attempts: 2,
                base: std::time::Duration::from_millis(1),
                cap: std::time::Duration::from_millis(5),
            },
        );
        let lfs_store = Arc::new(LfsObjectStore::new(store, "repo"));
        lfs_store
            .put(&oid, bytes::Bytes::copy_from_slice(original))
            .await
            .unwrap();
        let pointer = LfsPointer {
            oid,
            size: original.len() as u64,
            extensions: Vec::new(),
        }
        .serialize();
        let mut input = build_handshake_input();
        for pathname in ["blocked/model.bin", "allowed/model.bin"] {
            input.extend(pkt_text("command=smudge"));
            input.extend(pkt_text(&format!("pathname={pathname}")));
            input.extend(pkt_flush());
            input.extend(pkt_data(&pointer));
            input.extend(pkt_flush());
        }

        tokio::task::spawn_blocking(move || {
            let mut output = Vec::new();
            run_filter_loop(
                &mut &input[..],
                &mut output,
                AppContext::default(),
                Arc::new(std::sync::Mutex::new(LazyStaging::Unavailable)),
                Some(lfs_store),
                None,
                None,
                None,
                Arc::new(std::sync::Mutex::new(None)),
            )
            .unwrap();

            assert!(
                output
                    .windows(pointer.len())
                    .any(|bytes| bytes == pointer.as_slice())
            );
            assert!(
                output
                    .windows(original.len())
                    .any(|bytes| bytes == original)
            );
        })
        .await
        .unwrap();
    }

    #[test]
    fn non_pointer_content_passes_through_unchanged() {
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
            AppContext::default(),
            Arc::new(std::sync::Mutex::new(LazyStaging::Unavailable)),
            None,
            None,
            None,
            None,
            Arc::new(std::sync::Mutex::new(None)),
        )
        .unwrap();

        assert!(output.windows(content.len()).any(|bytes| bytes == content));
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
