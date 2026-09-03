//! `crab hydrate` — materialize pointer files into full content.
//!
//! Walks the working tree, identifies crab-tracked pointer files
//! matching the resolved pattern set, and batch-hydrates them via the
//! [`Hydrator`] trait. Configured repositories use [`ShardHydrator`];
//! unconfigured repositories retain local staging reconstruction through
//! [`SmudgeSessionHydrator`]. A [`StubHydrator`] is available for tests.
//!
//! All writes go through a tempfile-then-rename pattern for crash
//! safety: a SIGINT mid-write leaves only a tempfile that the OS
//! cleans up, never a half-written working-tree file.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::io::{Stdout, Write};
use std::path::{Component, Path, PathBuf};
use std::pin::Pin;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

use futures_util::stream::{self, FuturesUnordered, StreamExt};
use schemars::JsonSchema;
use serde::Serialize;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::cache::ChunkCache;
use crate::core::config::Config;
use crate::core::context::AppContext;
use crate::core::cow_clone;
use crate::core::error::{self, Result};
use crate::core::output::event_payloads::{FileDonePayload, ProgressPayload};
use crate::core::output::{JsonlStream, OutputMode, emit_json};
use crate::core::pattern::{PatternFilter, build_filter};
use crate::engine::pointer::is_working_tree_pointer;
use crate::git::progress::{format_rate, is_tty, render_bar};
use crate::git::smudge::SmudgeSession;
use crate::git::worktree::{
    WorktreeContext, normalize_identity_path, parse_worktree_list_porcelain,
    refresh_verified_index_stats,
};
use crate::git::worktree_hydration::{
    WorktreeHydrationMode, WorktreeHydrationPolicyFile, WorktreeHydrationPolicyStatus,
    WorktreeHydrationSelector,
};
use crab_metadata::file_index_lookup::SharedFileIndexLookup;
use crab_types::pointer::{MAX_POINTER_SIZE, Pointer};
use crab_xet::hash::MerkleHash;
use crab_xet::shard::FileDataSequenceEntry;
use crab_xet::xorb::format::MAX_XORB_SIZE;

/// Per-file hydration result sent through the progress channel.
///
/// Carries the timing and size data needed for JSONL per-file progress
/// rows. Sent by the hydrator as each file completes so the caller can
/// emit streaming JSONL without buffering.
#[derive(Debug, Clone)]
pub struct HydrateFileResult {
    /// Absolute path of the hydrated file.
    pub path: PathBuf,
    /// Outcome of the hydration attempt.
    pub outcome: HydrateFileOutcome,
    /// Wall-clock duration for this individual file.
    pub duration: Duration,
    /// Materialized file size in bytes (0 for failures).
    pub bytes: u64,
}

/// Outcome of a single file hydration attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HydrateFileOutcome {
    /// File was successfully hydrated.
    Hydrated,
    /// File was already hydrated and skipped.
    Skipped,
    /// Hydration failed for this file.
    Failed,
}

/// JSONL per-file progress row emitted during manifest hydration.
///
/// One row per file, streamed as each file completes.
#[derive(Debug, Serialize, JsonSchema)]
pub struct ManifestHydrateFileRow {
    /// Relative path of the file.
    pub path: String,
    /// Hydration strategy used (e.g. `"shard_batch"`).
    pub strategy: String,
    /// Wall-clock duration for this file in milliseconds.
    pub duration_ms: u64,
    /// Materialized file size in bytes.
    pub bytes: u64,
}

/// JSONL final summary record emitted after all manifest files are processed.
///
/// Distinguished from per-file rows by the `"type": "summary"` field.
#[derive(Debug, Serialize, JsonSchema)]
pub struct ManifestHydrateSummaryRow {
    /// Discriminator so consumers can tell this apart from per-file rows.
    #[serde(rename = "type")]
    pub row_type: String,
    /// Total number of files in the manifest.
    pub total: u64,
    /// Number of files successfully hydrated.
    pub hydrated: u64,
    /// Number of files skipped (already hydrated).
    pub skipped: u64,
    /// Number of files that failed to hydrate.
    pub failed: u64,
    /// Number of files materialized through a sibling-worktree CoW clone.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub cow_cloned: u64,
    /// Bytes materialized through sibling-worktree CoW clones.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub bytes_cow_cloned: u64,
    /// Total wall-clock duration in milliseconds.
    pub duration_ms: u64,
}

/// Arguments for the hydrate command.
#[derive(Debug, Clone)]
pub struct HydrateArgs {
    /// Positional glob patterns to hydrate.
    pub patterns: Vec<String>,
    /// Additional include patterns (`--include`).
    pub include: Vec<String>,
    /// Exclude patterns (`--exclude`).
    pub exclude: Vec<String>,
    /// Hydrate all pointer files.
    pub all: bool,
    /// Output mode resolved from `--json` / `--jsonl` flags.
    pub mode: OutputMode,
    /// Path to a newline-delimited manifest file, or `-` for stdin.
    pub manifest: Option<String>,
    /// Git ref to read the manifest from (e.g. `HEAD:.crab/manifests/ci.txt`).
    pub manifest_ref: Option<String>,
    /// Named prefetch profile from `crab.toml`.
    pub profile: Option<String>,
    /// Ignore sparse-checkout config during hydrate.
    ///
    /// When the `gix-worktree` feature gates on the worktree-state
    /// integration (Task 7.4), `crab hydrate --all` honors
    /// `.git/info/sparse-checkout` by default — a behavior change from
    /// the pre-adoption path, which hydrated every pointer regardless.
    /// This flag restores the legacy "everything" behavior for users
    /// who depend on it. The legacy code path (feature flag off)
    /// ignores the flag entirely.
    pub ignore_sparse: bool,
    /// Optional source for `--recover-from`: when set, every pointer
    /// is checked against a candidate file under this path before any
    /// remote fetch. Matching candidates are copied into place
    /// directly, bypassing the remote entirely. The path may be a
    /// single file (recovers exactly one pointer with the same
    /// content) or a directory (each pointer's basename is looked up
    /// inside).
    ///
    /// Designed for disaster-recovery: when a remote shard is missing
    /// chunks (an incomplete push, accidental GC), but the user still
    /// has the original file on local disk, this turns a fatal
    /// hydrate failure into a one-command recovery.
    pub recover_from: Option<PathBuf>,
}

/// Summary of a batch hydration run.
#[derive(Debug, Clone, Default)]
pub struct HydrateSummary {
    /// Number of files successfully hydrated.
    pub hydrated: u64,
    /// Total bytes written to the working tree.
    pub bytes_written: u64,
    /// Number of files skipped (already hydrated).
    pub skipped: u64,
    /// Total bytes in skipped (already hydrated) files.
    pub bytes_skipped: u64,
    /// Number of files that failed to hydrate.
    pub failed: u64,
    /// Number of files materialized from a local `--recover-from`
    /// source instead of the remote. Counted toward `hydrated` /
    /// `bytes_written` as well; this field is for the user-facing
    /// summary line that calls out the recovery path.
    pub recovered: u64,
    /// Bytes restored via `--recover-from`. Subset of `bytes_written`.
    pub bytes_recovered: u64,
    /// Files materialized through a verified sibling-worktree CoW clone.
    /// Subset of `hydrated`.
    pub cow_cloned: u64,
    /// Bytes materialized through verified sibling-worktree CoW clones.
    /// Subset of `bytes_written`.
    pub bytes_cow_cloned: u64,
    /// Exact post-publication stats for content verified during this run.
    pub(crate) verified_paths: Vec<crate::cache::add_validation::VerifiedPath>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct VerifiedWrite {
    bytes: u64,
    index_stat: crate::cmd::stream_stage::VerifiedIndexStat,
}

fn verified_path(
    path: &Path,
    pointer: &Pointer,
    write: VerifiedWrite,
) -> crate::cache::add_validation::VerifiedPath {
    crate::cache::add_validation::VerifiedPath {
        path: path.to_owned(),
        file_hash: pointer.file_hash,
        size: write.bytes,
        index_stat: write.index_stat,
    }
}

#[derive(Debug, Clone, Default)]
pub struct CachePrefetchSummary {
    pub prefetched: u64,
    pub bytes_prefetched: u64,
    pub failed: u64,
}

/// Serializable payload for `--json` / `--jsonl` result events.
#[derive(Debug, Serialize, JsonSchema)]
pub struct HydrateSummaryPayload {
    pub hydrated: u64,
    pub bytes_written: u64,
    pub skipped: u64,
    pub bytes_skipped: u64,
    pub failed: u64,
    pub duration_ms: u64,
    /// Files materialized from `--recover-from` (subset of `hydrated`).
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub recovered: u64,
    /// Bytes restored via `--recover-from` (subset of `bytes_written`).
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub bytes_recovered: u64,
    /// Files materialized through sibling-worktree CoW clones.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub cow_cloned: u64,
    /// Bytes materialized through sibling-worktree CoW clones.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub bytes_cow_cloned: u64,
}

struct PendingWorktreeHydration {
    args: HydrateArgs,
    policy: WorktreeHydrationPolicyFile,
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde skip_serializing_if passes field values by reference"
)]
fn is_zero_u64(v: &u64) -> bool {
    *v == 0
}

impl HydrateSummaryPayload {
    fn from_summary(summary: &HydrateSummary, elapsed: Duration) -> Self {
        Self {
            hydrated: summary.hydrated,
            bytes_written: summary.bytes_written,
            skipped: summary.skipped,
            bytes_skipped: summary.bytes_skipped,
            failed: summary.failed,
            duration_ms: elapsed.as_millis() as u64,
            recovered: summary.recovered,
            bytes_recovered: summary.bytes_recovered,
            cow_cloned: summary.cow_cloned,
            bytes_cow_cloned: summary.bytes_cow_cloned,
        }
    }
}

/// Shared progress state for live hydration reporting.
///
/// Updated atomically by the hydrator as files complete. A background
/// ticker reads these counters to render a live progress bar on stderr.
pub struct HydrateProgress {
    /// Total number of files to hydrate (set once before the batch starts).
    pub total_files: AtomicU64,
    /// Total bytes across all files to hydrate.
    pub total_bytes: AtomicU64,
    /// Files completed so far (hydrated + skipped + failed).
    pub files_done: AtomicU64,
    /// Bytes completed so far (written + skipped).
    pub bytes_done: AtomicU64,
    /// Name of the file currently being hydrated (for display).
    current_file: std::sync::Mutex<String>,
    /// Timestamp when the batch started.
    start: Instant,
    /// Optional channel for streaming per-file results. When set, the
    /// hydrator sends a [`HydrateFileResult`] as each file completes so
    /// the caller can emit JSONL rows without buffering.
    file_result_tx: Option<tokio::sync::mpsc::UnboundedSender<HydrateFileResult>>,
}

impl HydrateProgress {
    fn new(total_files: u64, total_bytes: u64) -> Self {
        Self {
            total_files: AtomicU64::new(total_files),
            total_bytes: AtomicU64::new(total_bytes),
            files_done: AtomicU64::new(0),
            bytes_done: AtomicU64::new(0),
            current_file: std::sync::Mutex::new(String::new()),
            start: Instant::now(),
            file_result_tx: None,
        }
    }

    /// Create progress state with a per-file result channel for JSONL streaming.
    fn with_file_result_tx(
        total_files: u64,
        total_bytes: u64,
        tx: tokio::sync::mpsc::UnboundedSender<HydrateFileResult>,
    ) -> Self {
        Self {
            file_result_tx: Some(tx),
            ..Self::new(total_files, total_bytes)
        }
    }

    /// Send a per-file result through the channel (if configured).
    /// Failures are silently ignored — JSONL emission is best-effort.
    fn send_file_result(&self, result: HydrateFileResult) {
        if let Some(tx) = &self.file_result_tx {
            let _ = tx.send(result);
        }
    }

    fn add_bytes_done(&self, bytes: u64) {
        if bytes == 0 {
            return;
        }
        let total = self.total_bytes.load(Relaxed);
        let _ = self.bytes_done.fetch_update(Relaxed, Relaxed, |current| {
            let next = current.saturating_add(bytes);
            Some(if total > 0 { next.min(total) } else { next })
        });
    }

    fn set_current_file(&self, name: &str) {
        if let Ok(mut f) = self.current_file.lock() {
            f.clear();
            f.push_str(name);
        }
    }

    fn current_file_name(&self) -> String {
        self.current_file
            .lock()
            .map(|f| f.clone())
            .unwrap_or_default()
    }

    /// Render a single progress line for the terminal.
    fn render_line(&self, color: bool) -> String {
        let done = self.bytes_done.load(Relaxed);
        let total = self.total_bytes.load(Relaxed);
        let files_done = self.files_done.load(Relaxed);
        let total_files = self.total_files.load(Relaxed);

        let fraction = if total > 0 {
            done as f64 / total as f64
        } else {
            0.0
        };
        let pct = (fraction * 100.0).min(100.0);

        let elapsed = self.start.elapsed();
        let rate = if elapsed.as_secs_f64() > 0.0 {
            done as f64 / elapsed.as_secs_f64()
        } else {
            0.0
        };

        let bar = render_bar(fraction, 20, color);
        let current = self.current_file_name();
        let file_label = if current.is_empty() {
            String::new()
        } else {
            // Truncate long filenames to keep the line readable.
            let max_name = 30;
            if current.len() > max_name {
                format!(" …{}", &current[current.len() - max_name..])
            } else {
                format!(" {current}")
            }
        };

        format!(
            "Hydrating: {pct:5.1}% {bar} ({files_done}/{total_files}) {done_fmt} / {total_fmt} | {rate_fmt}{file_label}",
            done_fmt = format_bytes(done),
            total_fmt = format_bytes(total),
            rate_fmt = format_rate(rate),
        )
    }

    /// Spawn a background ticker that redraws the progress line on stderr.
    ///
    /// Returns `None` if stderr is not a TTY (CI, piped output).
    fn start_ticker(self: &Arc<Self>, cancel: &CancellationToken) -> Option<JoinHandle<()>> {
        if !is_tty() {
            return None;
        }
        let progress = Arc::clone(self);
        let cancel = cancel.clone();
        Some(tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(200));
            loop {
                tokio::select! {
                    () = cancel.cancelled() => break,
                    _ = interval.tick() => (),
                }
                let line = progress.render_line(true);
                // Overwrite the current line with \r.
                eprint!("\r\x1b[2K{line}");
            }
        }))
    }
}

/// Trait for batch-hydrating pointer files into full content.
///
/// Implementations receive a batch of `(absolute_path, Pointer)` pairs
/// and are responsible for reconstructing the original content and
/// writing it to the working tree. Writes must be atomic (tempfile +
/// rename) so that cancellation never leaves a corrupt file.
pub trait Hydrator: Send + Sync {
    fn hydrate_batch<'a>(
        &'a self,
        items: &'a [(PathBuf, Pointer)],
        cancel: &'a CancellationToken,
        progress: Option<&'a Arc<HydrateProgress>>,
    ) -> Pin<Box<dyn Future<Output = Result<HydrateSummary>> + Send + 'a>>;
}

/// Production hydrator backed by [`SmudgeSession`].
///
/// For each pointer file, calls `SmudgeSession::smudge_file` to
/// reconstruct the original content via the full smudge pipeline
/// (file-index → shard → xorb Range GETs → blake3 verify), then
/// writes the result atomically to the working tree.
pub struct SmudgeSessionHydrator {
    session: SmudgeSession,
}

impl SmudgeSessionHydrator {
    pub fn new(ctx: AppContext) -> Self {
        Self {
            session: SmudgeSession::new(ctx),
        }
    }

    /// Create a hydrator with a shared chunk cache.
    pub fn with_chunk_cache(ctx: AppContext, cache: Arc<ChunkCache>) -> Self {
        Self {
            session: SmudgeSession::with_chunk_cache(ctx, cache),
        }
    }
}

impl Hydrator for SmudgeSessionHydrator {
    fn hydrate_batch<'a>(
        &'a self,
        items: &'a [(PathBuf, Pointer)],
        cancel: &'a CancellationToken,
        progress: Option<&'a Arc<HydrateProgress>>,
    ) -> Pin<Box<dyn Future<Output = Result<HydrateSummary>> + Send + 'a>> {
        Box::pin(async move {
            let mut summary = HydrateSummary::default();

            for (path, ptr) in items {
                error::check_cancelled(cancel)?;

                if let Some(p) = progress {
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    p.set_current_file(&name);
                }

                let file_start = Instant::now();

                if is_already_hydrated(path, ptr, cancel)? {
                    debug!(path = %path.display(), "already hydrated, skipping");
                    summary.skipped += 1;
                    summary.bytes_skipped += ptr.size;
                    if let Some(p) = progress {
                        p.files_done.fetch_add(1, Relaxed);
                        p.bytes_done.fetch_add(ptr.size, Relaxed);
                        p.send_file_result(HydrateFileResult {
                            path: path.clone(),
                            outcome: HydrateFileOutcome::Skipped,
                            duration: file_start.elapsed(),
                            bytes: ptr.size,
                        });
                    }
                    continue;
                }

                let pointer_bytes = ptr.serialize();
                let result = match self.session.smudge_file(&pointer_bytes).await {
                    Ok(content) => {
                        atomic_write_with_progress(path, &content, None).map(|index_stat| {
                            VerifiedWrite {
                                bytes: content.len() as u64,
                                index_stat,
                            }
                        })
                    }
                    Err(remote_error) => match try_hydrate_from_staging(path, ptr, cancel).await {
                        Ok(Some(bytes)) => Ok(bytes),
                        Ok(None) => Err(remote_error),
                        Err(staging_error) => {
                            warn!(
                                path = %path.display(),
                                error = %staging_error,
                                "local staging hydrate fallback failed"
                            );
                            Err(remote_error)
                        }
                    },
                };
                match result {
                    Ok(write) => {
                        summary.hydrated += 1;
                        summary.bytes_written += write.bytes;
                        summary.verified_paths.push(verified_path(path, ptr, write));
                        if let Some(p) = progress {
                            p.files_done.fetch_add(1, Relaxed);
                            p.bytes_done.fetch_add(write.bytes, Relaxed);
                            p.send_file_result(HydrateFileResult {
                                path: path.clone(),
                                outcome: HydrateFileOutcome::Hydrated,
                                duration: file_start.elapsed(),
                                bytes: write.bytes,
                            });
                        }
                    }
                    Err(e) => {
                        debug!(path = %path.display(), err = %e, "smudge failed");
                        summary.failed += 1;
                        if let Some(p) = progress {
                            p.files_done.fetch_add(1, Relaxed);
                            p.send_file_result(HydrateFileResult {
                                path: path.clone(),
                                outcome: HydrateFileOutcome::Failed,
                                duration: file_start.elapsed(),
                                bytes: 0,
                            });
                        }
                    }
                }
            }

            Ok(summary)
        })
    }
}

/// Shard-based hydrator that reconstructs files via the file-index and
/// MDB shard metadata.
///
/// Replaces the legacy manifest-based approach with O(1) file-index
/// lookups and cached shard downloads. Shares the unified `LocalCache`
/// with push and diff so shards downloaded by any command are
/// immediately available to all others.
pub struct ShardHydrator {
    store: crab_cache_store::CachingStore,
    router: HydrateStoreLayout,
    /// In-memory cache of downloaded xorb bytes, keyed by xorb hash.
    /// Used by the delta-reconstruction path (`try_delta_reconstruct`)
    /// which fetches xorbs directly rather than via `FileReconstructor`.
    xorb_cache: tokio::sync::Mutex<std::collections::HashMap<MerkleHash, bytes::Bytes>>,
    /// Tracks the total body bytes held by `xorb_cache`. This is updated only
    /// while the cache mutex is held, so the atomic avoids a second lock for
    /// the accounting field without allowing the cache to grow unbounded.
    xorb_cache_bytes: AtomicU64,
    /// Optional perf counters. When set, shard-hint hit/miss events are recorded.
    metrics: Option<Arc<crate::core::metrics::Metrics>>,
    /// Shared concurrency controller used by [`FileReconstructor`] to
    /// bound in-flight xorb downloads. Held as an [`Arc`] so prefetch
    /// and smudge can share the same controller for global throttling.
    concurrency: Arc<xet_client::cas_client::adaptive_concurrency::AdaptiveConcurrencyController>,
    /// Shared byte-denominated bound for decompressed reconstruction buffers.
    buffer_semaphore: Arc<xet_runtime::utils::adjustable_semaphore::AdjustableSemaphore>,
    /// Maximum number of files reconstructed concurrently in one batch.
    file_concurrency: usize,
    /// Optional xet-core xorb-range chunk cache. When set, every
    /// [`FileReconstructor`] spun up by this hydrator shares the same
    /// on-disk cache with the prefetch queue and filter-process smudge.
    chunk_cache: Option<Arc<dyn xet_client::chunk_cache::ChunkCache>>,
    /// Restore coordinator used when a direct xorb fetch finds an
    /// archive-class object.
    restore_orchestrator: Option<Arc<crate::tier::restore::RestoreOrchestrator>>,
    /// Effective restore behavior for this hydrator.
    auto_restore: bool,
}

/// Tag for the hydrate-path concurrency controller. `'static` because
/// `AdaptiveConcurrencyController` stores the tag for logging.
const HYDRATE_CONCURRENCY_TAG: &str = "crab-hydrate";
const MAX_HYDRATE_FILE_CONCURRENCY: usize = 4;
const MAX_PREFETCH_CANDIDATES: usize = 1_000_000;
const MAX_GIT_INVENTORY_BYTES: usize = 512 * 1024 * 1024;
const MAX_HYDRATE_SHARD_BYTES: u64 = 512 * 1024 * 1024;
const MAX_HYDRATE_XORB_CACHE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_HYDRATE_XORB_CACHE_ENTRIES: usize = 4_096;

fn hydrate_file_concurrency(download_concurrency: usize) -> usize {
    download_concurrency.clamp(1, MAX_HYDRATE_FILE_CONCURRENCY)
}

fn hydrate_buffer_semaphore(
    budget_bytes: u64,
) -> Arc<xet_runtime::utils::adjustable_semaphore::AdjustableSemaphore> {
    let budget_bytes = budget_bytes.max(1);
    xet_runtime::utils::adjustable_semaphore::AdjustableSemaphore::new(
        budget_bytes,
        (budget_bytes, budget_bytes),
    )
}

/// Storage-domain path router used by the hydrator Implementation.
pub type HydrateStoreLayout = crab_storage::StoreLayout<crab_storage::Store>;

fn hydrate_layout_from_cli(
    store: &crab_cache_store::CachingStore,
    router: &crate::storage::StoreLayout,
) -> HydrateStoreLayout {
    HydrateStoreLayout::with_global_prefix(
        store.origin().clone(),
        router.repo_prefix().to_owned(),
        router.global_prefix().to_owned(),
    )
}

impl ShardHydrator {
    /// Create a new shard hydrator with a `StoreLayout` for path routing.
    ///
    /// Uses the default hydrate concurrency bound
    /// ([`HydrateConfig::download_concurrency`] default: 4). For an
    /// explicit bound, chain [`with_concurrency`](Self::with_concurrency)
    /// or construct via [`with_config`](Self::with_config).
    ///
    /// # Errors
    ///
    /// Returns an error when the xet runtime context for the download
    /// concurrency controller cannot be created.
    pub fn new(store: crab_cache_store::CachingStore, router: HydrateStoreLayout) -> Result<Self> {
        let concurrency = fixed_hydrate_concurrency(
            crate::core::config::HydrateConfig::default().download_concurrency,
        )?;
        Ok(Self::with_concurrency(store, router, concurrency))
    }

    /// Create a new shard hydrator from the legacy CLI storage layout Adapter.
    ///
    /// This keeps current CLI/SDK read-selection callers compiling while the
    /// hydrator Implementation uses the storage-domain layout internally.
    pub fn new_from_cli_layout(
        store: crab_cache_store::CachingStore,
        router: crate::storage::StoreLayout,
    ) -> Result<Self> {
        let router = hydrate_layout_from_cli(&store, &router);
        Self::new(store, router)
    }

    /// Create a new shard hydrator with an explicit concurrency
    /// controller. The controller bounds in-flight xorb downloads for
    /// the `FileReconstructor` path and is shared with the prefetch
    /// queue and filter-process smudge when those paths are active.
    #[must_use]
    pub fn with_concurrency(
        store: crab_cache_store::CachingStore,
        router: HydrateStoreLayout,
        concurrency: Arc<
            xet_client::cas_client::adaptive_concurrency::AdaptiveConcurrencyController,
        >,
    ) -> Self {
        let hydrate = crate::core::config::HydrateConfig::default();
        Self {
            store,
            router,
            xorb_cache: tokio::sync::Mutex::new(std::collections::HashMap::new()),
            xorb_cache_bytes: AtomicU64::new(0),
            metrics: None,
            concurrency,
            buffer_semaphore: hydrate_buffer_semaphore(hydrate.prefetch_budget),
            file_concurrency: hydrate_file_concurrency(hydrate.download_concurrency),
            chunk_cache: None,
            restore_orchestrator: None,
            auto_restore: false,
        }
    }

    /// Create a new shard hydrator honoring the hydrate section of
    /// [`Config`]. This is the preferred constructor for CLI entry
    /// points that already have a parsed config in hand.
    ///
    /// # Errors
    ///
    /// Returns an error when the xet runtime context for the download
    /// concurrency controller cannot be created.
    pub fn with_config(
        store: crab_cache_store::CachingStore,
        router: HydrateStoreLayout,
        config: &Config,
    ) -> Result<Self> {
        let concurrency = fixed_hydrate_concurrency(config.hydrate.download_concurrency)?;
        let mut hydrator = Self::with_concurrency(store, router, concurrency);
        hydrator.buffer_semaphore = hydrate_buffer_semaphore(config.hydrate.prefetch_budget);
        hydrator.file_concurrency = hydrate_file_concurrency(config.hydrate.download_concurrency);
        Ok(hydrator)
    }

    /// Create a config-backed hydrator from the legacy CLI storage layout Adapter.
    ///
    /// New callers that already own a `crab_storage::Store` should use
    /// [`Self::with_config`] with [`HydrateStoreLayout`] directly.
    pub fn with_config_from_cli_layout(
        store: crab_cache_store::CachingStore,
        router: crate::storage::StoreLayout,
        config: &Config,
    ) -> Result<Self> {
        let router = hydrate_layout_from_cli(&store, &router);
        Self::with_config(store, router, config)
    }

    /// Attach shared perf counters. When present, the hydrator records
    /// `shard_hint_hits` on fast-path success and `shard_hint_misses`
    /// on any fallback to the file-index path.
    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<crate::core::metrics::Metrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Attach the shared xet-core xorb-range chunk cache. Every
    /// [`FileReconstructor`] created by [`reconstruct_file`] will be
    /// chained with `.with_chunk_cache()`, so warm entries populated
    /// by prefetch or previous hydrations are served locally.
    ///
    /// The cache is optional: passing `None` (by not calling this
    /// builder) preserves the legacy cold-fetch behavior.
    #[must_use]
    pub fn with_xet_chunk_cache(
        mut self,
        cache: Arc<dyn xet_client::chunk_cache::ChunkCache>,
    ) -> Self {
        self.chunk_cache = Some(cache);
        self
    }

    /// Build the delayed-smudge prefetch queue from the same store,
    /// router, cache, concurrency, and optional chunk cache as inline
    /// hydration.
    #[must_use]
    pub fn prefetch_queue(
        &self,
        config: &Config,
        cancel: tokio_util::sync::CancellationToken,
        handle: tokio::runtime::Handle,
    ) -> crate::git::prefetch::PrefetchQueue {
        let file_index_lookup = self.shared_file_index_lookup();
        let client = crate::git::store_client::StoreClient::new(
            self.store.clone(),
            self.router.clone(),
            self.concurrency.clone(),
        )
        .with_file_index_lookup(file_index_lookup.clone());
        let client = match self.metrics.clone() {
            Some(metrics) => client.with_metrics(metrics),
            None => client,
        };
        let shard_hints = client.shared_shard_hints();
        let client: Arc<dyn xet_client::cas_client::Client> = Arc::new(client);
        let queue = crate::git::prefetch::PrefetchQueue::new(
            client,
            config.hydrate.prefetch_budget,
            cancel,
            handle,
        )
        .with_file_index_lookup(file_index_lookup)
        .with_shard_hints(shard_hints);
        let queue = match self.chunk_cache.clone() {
            Some(cache) => queue.with_chunk_cache(cache),
            None => queue,
        };
        match self.metrics.clone() {
            Some(metrics) => queue.with_metrics(metrics),
            None => queue,
        }
    }

    pub async fn prefetch_batch(
        &self,
        items: &[(PathBuf, Pointer)],
        cancel: &CancellationToken,
    ) -> Result<CachePrefetchSummary> {
        let file_index_lookup = self.shared_file_index_lookup();
        let result = async {
            let mut summary = CachePrefetchSummary::default();
            let mut results = stream::iter(items.iter())
                .map(|(path, ptr)| {
                    let file_index_lookup = file_index_lookup.clone();
                    async move {
                        let result = async {
                            error::check_cancelled(cancel)?;
                            self.reconstruct_to_writer_with(
                                ptr,
                                std::io::sink(),
                                Some(&file_index_lookup),
                                cancel,
                            )
                            .await
                        }
                        .await;
                        (path, result)
                    }
                })
                .buffer_unordered(self.file_concurrency);
            while let Some((path, result)) = results.next().await {
                match result {
                    Ok(bytes) => {
                        summary.prefetched += 1;
                        summary.bytes_prefetched += bytes;
                        debug!(
                            path = %path.display(),
                            bytes,
                            "cache prefetch reconstructed selected pointer"
                        );
                    }
                    Err(e) => {
                        summary.failed += 1;
                        warn!(path = %path.display(), error = %e, "cache prefetch failed");
                    }
                }
            }

            Ok(summary)
        }
        .await;

        if let Err(e) = file_index_lookup.close().await {
            warn!(err = %e, "hydrate prefetch file-index lookup session close failed");
        }
        result
    }

    /// Attach archive restore handling for direct xorb fetches.
    #[must_use]
    pub fn with_restore(
        mut self,
        orchestrator: Option<Arc<crate::tier::restore::RestoreOrchestrator>>,
        auto_restore: bool,
    ) -> Self {
        self.restore_orchestrator = orchestrator;
        self.auto_restore = auto_restore;
        self
    }

    fn shared_file_index_lookup(&self) -> SharedFileIndexLookup {
        SharedFileIndexLookup::new_for_storage(
            self.store.origin(),
            self.router.repo_prefix().to_owned(),
        )
    }

    fn store_client_for_pointer(
        &self,
        file_index_lookup: Option<&SharedFileIndexLookup>,
        ptr: &Pointer,
    ) -> Arc<dyn xet_client::cas_client::Client> {
        let file_hash = MerkleHash::from(ptr.file_hash);
        let shard_hint = ptr
            .shard_hint
            .map(|hint| (file_hash, MerkleHash::from(hint)));
        Arc::new(self.store_client_adapter(file_index_lookup, shard_hint))
    }

    fn store_client_adapter(
        &self,
        file_index_lookup: Option<&SharedFileIndexLookup>,
        shard_hint: Option<(MerkleHash, MerkleHash)>,
    ) -> crate::git::store_client::StoreClient {
        let client = crate::git::store_client::StoreClient::new(
            self.store.clone(),
            self.router.clone(),
            self.concurrency.clone(),
        );
        let client = match self.metrics.clone() {
            Some(metrics) => client.with_metrics(metrics),
            None => client,
        };
        let client = match file_index_lookup {
            Some(lookup) => client.with_file_index_lookup(lookup.clone()),
            None => client,
        };
        match shard_hint {
            Some((file_hash, shard_hash)) => client.with_shard_hint(file_hash, shard_hash),
            None => client,
        }
    }

    /// Resolve `file_hash → shard_hash` via the per-repo `file_index_db`.
    ///
    /// Uses the caller-provided shared lookup session when present;
    /// otherwise falls back to the one-shot compatibility helper.
    /// A miss is mapped to [`Error::ChunkNotFound`] with a file-index
    /// marker so the caller surfaces a clear "pointer references a
    /// file we've never seen" error rather than a silent empty
    /// reconstruction.
    async fn resolve_file_index(
        &self,
        file_hash: &MerkleHash,
        file_index_lookup: Option<&SharedFileIndexLookup>,
    ) -> Result<MerkleHash> {
        let hit = match file_index_lookup {
            Some(lookup) => lookup.lookup(file_hash).await?,
            None => {
                let storage = self.store.cache_aware_storage();
                let session =
                    crab_metadata::file_index_lookup::FileIndexLookupSession::open_for_storage(
                        &storage,
                        self.router.repo_prefix(),
                    )
                    .await?;
                let result = session.lookup(file_hash).await;
                if let Err(close_error) = session.close().await {
                    warn!(error = %close_error, "hydrate file-index lookup close failed");
                }
                result?
            }
        };

        match hit {
            Some(shard_hash) => Ok(shard_hash),
            None => Err(error::CrabError::ChunkNotFound {
                hash: format!("file_index:{}", file_hash.hex()),
            }),
        }
    }

    /// Fetch an xorb by hash, returning cached bytes on repeat access.
    ///
    /// Consults `LocalCache` under [`CacheKey::Xorb`] before issuing an
    /// S3 GET so xorbs warmed by `post_success_cleanup` (step 13 of the
    /// push pipeline) are served from disk. The in-memory `xorb_cache`
    /// still short-circuits repeat reads of the same xorb within a
    /// single hydrate run.
    async fn get_xorb(&self, hash: &MerkleHash) -> Result<bytes::Bytes> {
        {
            let cache = self.xorb_cache.lock().await;
            if let Some(data) = cache.get(hash) {
                return Ok(data.clone());
            }
        }
        let origin = self.store.origin().clone();
        let path = self.router.xorb_path(hash);
        let hash_for_fetch = *hash;
        let restore_orchestrator = self.restore_orchestrator.clone();
        let auto_restore = self.auto_restore;
        debug!(xorb_hash = %hash_for_fetch.hex(), "hydrate: downloading xorb");
        let restore_origin = crate::storage::Store::from_storage(origin);
        crate::cmd::hydrate_restore::resolve_xorb_with_class_probe(
            &restore_origin,
            &path,
            restore_orchestrator.as_deref(),
            auto_restore,
        )
        .await?;
        // CachingStore owns the local and remote cache read-through order.
        // Calling the origin directly here bypasses a warmed cache service.
        let (data, _) = self
            .store
            .get_with_etag_bounded(&path, MAX_XORB_SIZE as u64)
            .await?;
        self.cache_xorb(*hash, &data).await?;
        Ok(data)
    }

    async fn refetch_xorb_from_origin(&self, hash: &MerkleHash) -> Result<bytes::Bytes> {
        let key = crate::cache::CacheKey::Xorb(*hash);
        let path = self.router.xorb_path(hash);

        self.store.local_cache().evict(&key).await?;

        let origin = crate::storage::Store::from_storage(self.store.origin().clone());
        crate::cmd::hydrate_restore::resolve_xorb_with_class_probe(
            &origin,
            &path,
            self.restore_orchestrator.as_deref(),
            self.auto_restore,
        )
        .await?;
        let (data, _) = self
            .store
            .origin()
            .get_with_etag_bounded(&path, MAX_XORB_SIZE as u64)
            .await?;

        if let Err(e) = self.store.local_cache().put(&key, &data).await {
            warn!(
                xorb_hash = %hash.hex(),
                error = %e,
                "failed to rewrite repaired hydrate xorb cache entry"
            );
        }
        self.cache_xorb(*hash, &data).await?;
        Ok(data)
    }

    async fn cache_xorb(&self, hash: MerkleHash, data: &bytes::Bytes) -> Result<()> {
        if data.len() > MAX_XORB_SIZE {
            return Err(error::CrabError::CorruptObject {
                path: self.router.xorb_path(&hash).to_string(),
                reason: format!(
                    "xorb body is {} bytes; the Xet format supports at most {MAX_XORB_SIZE} bytes",
                    data.len()
                ),
            });
        }

        let mut cache = self.xorb_cache.lock().await;
        let old_len = cache.get(&hash).map_or(0, bytes::Bytes::len) as u64;
        let incoming_len = data.len() as u64;
        let mut cached_bytes = self.xorb_cache_bytes.load(Relaxed).saturating_sub(old_len);
        let replacing = cache.contains_key(&hash);
        if (!replacing && cache.len() >= MAX_HYDRATE_XORB_CACHE_ENTRIES)
            || cached_bytes.saturating_add(incoming_len) > MAX_HYDRATE_XORB_CACHE_BYTES
        {
            cache.clear();
            cached_bytes = 0;
        }
        cache.insert(hash, data.clone());
        self.xorb_cache_bytes
            .store(cached_bytes.saturating_add(incoming_len), Relaxed);
        Ok(())
    }

    async fn decode_xorb_with_cache_repair<T>(
        &self,
        hash: &MerkleHash,
        mut decode: impl FnMut(bytes::Bytes) -> Result<T>,
    ) -> Result<T> {
        let data = self.get_xorb(hash).await?;
        match decode(data) {
            Ok(result) => Ok(result),
            Err(cache_error) => {
                warn!(
                    xorb_hash = %hash.hex(),
                    error = %cache_error,
                    "cached xorb failed verification, evicting and retrying origin once"
                );
                let repaired = self.refetch_xorb_from_origin(hash).await?;
                decode(repaired)
            }
        }
    }

    /// Download a shard via the unified `LocalCache`, or return a cached
    /// copy. Hash verification is handled by `LocalCache::get_or_fetch`.
    async fn get_or_download_shard(
        &self,
        hash: &MerkleHash,
    ) -> Result<crab_xet::shard::ShardReader> {
        let obj_path = self.router.shard_path(hash);
        debug!(shard_hash = %hash.hex(), "downloading shard");
        // Keep shard reads on the same cache-aware path as xorb reads so
        // push-warmed immutable objects are reusable by new clones.
        let (data, _) = self
            .store
            .get_with_etag_bounded(&obj_path, MAX_HYDRATE_SHARD_BYTES)
            .await?;

        Ok(crab_xet::shard::ShardReader::from_bytes(data, *hash))
    }

    /// Download a shard for a specific file, consulting the bloom
    /// pre-filter first when the shard is not yet cached.
    ///
    /// The file-index points `file_hash → shard_hash`, but that mapping
    /// can be stale (the shard has been repacked since the pointer was
    /// written) or corrupt. A small Range-GET on the shard's v1 bloom
    /// trailer can prove the file is absent before we pay for the full
    /// shard body. A definitive-absent result surfaces as a
    /// [`CrabError::CorruptObject`] so the caller can fall back to a
    /// slower path or report the file as unreconstructable.
    ///
    /// Cached shards bypass the pre-filter entirely: disk reads are
    /// already faster than any Range-GET round-trip.
    async fn get_or_download_shard_for_file(
        &self,
        shard_hash: &MerkleHash,
        file_hash: &MerkleHash,
    ) -> Result<crab_xet::shard::ShardReader> {
        // With a cache service configured, keep immutable shard reads behind
        // that boundary instead of probing origin directly for the bloom
        // trailer. Origin fallback still happens inside `CachingStore`.
        if !self.store.has_cache_service()
            && !self
                .store
                .local_cache()
                .contains(&crate::cache::CacheKey::Shard(*shard_hash))
                .await
        {
            let shard_path = self.router.shard_path(shard_hash);
            match crab_metadata::bloom_prefilter::check_shard_file_bloom(
                self.store.origin(),
                &shard_path,
                file_hash,
            )
            .await
            {
                Ok(crab_metadata::bloom_prefilter::BloomCheck::DefinitelyAbsent) => {
                    debug!(
                        file_hash = %file_hash.hex(),
                        shard_hash = %shard_hash.hex(),
                        "bloom pre-filter: file absent from shard, aborting download"
                    );
                    return Err(error::CrabError::CorruptObject {
                        path: format!("file_index_db/{}", file_hash.hex()),
                        reason: format!(
                            "shard {} does not contain file (bloom reports definitely absent); \
                             file_index_db entry is stale",
                            shard_hash.hex()
                        ),
                    });
                }
                Ok(_) => {
                    // PossiblyPresent or NoBloom — fall through.
                }
                Err(e) => {
                    debug!(
                        shard_hash = %shard_hash.hex(),
                        error = %e,
                        "bloom pre-filter failed, falling back to full shard download"
                    );
                }
            }
        }

        self.get_or_download_shard(shard_hash).await
    }

    /// Reconstruct a single file from its pointer using xet-core's
    /// [`FileReconstructor`].
    ///
    /// The reconstructor drives parallel xorb fetches through the
    /// [`StoreClient`] adapter, reassembles chunks in order, and
    /// respects the shared concurrency budget. Blake3 verification of
    /// the reassembled bytes is performed here (xet-core does not do
    /// it internally).
    ///
    /// Reconstruct a file from its pointer bytes, returning the full
    /// content. This is the public entry point used by the filter-process
    /// smudge path for inline (non-lazy) reconstruction.
    ///
    /// Parses the pointer, delegates to [`reconstruct_file`], and returns
    /// the byte-identical original content (blake3-verified).
    pub async fn reconstruct_from_pointer(&self, pointer_bytes: &[u8]) -> Result<Vec<u8>> {
        let ptr = crab_types::pointer::Pointer::parse(pointer_bytes)?;
        self.reconstruct_file(&ptr, None).await
    }

    /// Pre-flight check: ask the shard how many bytes its
    /// reconstruction terms cover, and bail out early when that sum
    /// is less than the pointer's declared size.
    ///
    /// Without this, an incomplete shard would let
    /// [`xet_data::file_reconstruction::FileReconstructor`] silently
    /// produce a short file that fails the trailing blake3 check
    /// with a generic [`error::CrabError::HashMismatch`]. The mismatch
    /// is real but the message buries the actual cause — the remote
    /// is missing chunks, not corrupting them.
    ///
    /// A `None` from `get_file_reconstruction_info` (no file-index
    /// entry, missing shard) is *not* a partial-coverage problem; we
    /// let the downstream reconstructor surface its usual error so
    /// the diagnosis stays accurate. Same for any error from the
    /// adapter: better to attempt the reconstruction and let the
    /// real error bubble up than to misclassify it as incomplete
    /// coverage.
    async fn preflight_shard_coverage(
        &self,
        client: &Arc<dyn xet_client::cas_client::Client>,
        ptr: &Pointer,
    ) -> Result<()> {
        let file_hash = MerkleHash::from(ptr.file_hash);
        // No reconstruction info available: let the downstream path
        // produce its specific error (NotFound, etc.).
        let Ok(Some((info, _))) = client.get_file_reconstruction_info(&file_hash).await else {
            return Ok(());
        };

        let covered: u64 = info
            .segments
            .iter()
            .map(|s| u64::from(s.unpacked_segment_bytes))
            .sum();
        if covered >= ptr.size {
            return Ok(());
        }

        let total_chunks: u32 = info
            .segments
            .iter()
            .map(|s| s.chunk_index_end - s.chunk_index_start)
            .sum();
        let example = info.segments.first().map_or_else(
            || (0, String::new()),
            |s| (s.chunk_index_start, s.xorb_hash.hex()),
        );

        tracing::error!(
            file_hash = %file_hash.hex(),
            expected_bytes = ptr.size,
            covered_bytes = covered,
            num_segments = info.segments.len(),
            total_chunks,
            "preflight: shard reconstruction terms do not cover the full file; \
             remote is missing chunks (incomplete push). Try \
             `crab hydrate --recover-from <path>` if you have the original file locally."
        );

        Err(error::CrabError::IncompleteShardReconstruction {
            file_hash: file_hash.hex(),
            path: None,
            // Report the byte-shortfall translated into a chunk count
            // estimate. We don't know which chunk indices the missing
            // bytes correspond to — that lives with whichever push
            // dropped them — so the example carries the first segment
            // we *do* have, scoped by index 0 of the file's chunk list.
            uncovered_chunks: 1,
            example_chunk_hash: example.1,
            example_chunk_index: example.0,
        })
    }

    /// Pointer-carried shard hints are installed on the StoreClient
    /// before xet-core asks for reconstruction terms. The adapter
    /// verifies hinted shards contain the file and falls back to the
    /// file-index on stale or unreadable hints.
    async fn reconstruct_file(
        &self,
        ptr: &Pointer,
        file_index_lookup: Option<&SharedFileIndexLookup>,
    ) -> Result<Vec<u8>> {
        let file_hash = MerkleHash::from(ptr.file_hash);

        let client = self.store_client_for_pointer(file_index_lookup, ptr);

        // Pre-flight: detect partial-coverage shards before we burn
        // bandwidth fetching xorbs that won't add up to the full file.
        self.preflight_shard_coverage(&client, ptr).await?;

        // Reconstruct into an in-memory buffer. We hold the full bytes
        // so the caller can blake3-verify and then hand them to the
        // atomic-write path; streaming verification is a future knob.
        #[expect(
            clippy::cast_possible_truncation,
            reason = "pointer size fits usize on all platforms crab runs on"
        )]
        let expected_size = ptr.size as usize;
        let buffer: std::io::Cursor<Vec<u8>> =
            std::io::Cursor::new(Vec::with_capacity(expected_size));
        let shared = Arc::new(std::sync::Mutex::new(buffer));
        let writer = SharedCursorWriter(shared.clone());

        let xet_context = new_xet_context()?;
        let reconstructor =
            xet_data::file_reconstruction::FileReconstructor::new(&xet_context, &client, file_hash)
                .with_buffer_semaphore(Arc::clone(&self.buffer_semaphore));
        // Chunk cache and prefetch share the same concurrency
        // controller via `StoreClient::acquire_download_permit`. The
        // cancellation token plumbing flows through
        // `hydrate_batch` → `Hydrator`; we rely on the controller's
        // permit acquisition being cancel-safe.
        let reconstructor = match self.chunk_cache.clone() {
            Some(cache) => reconstructor.with_chunk_cache(cache),
            None => reconstructor,
        };

        reconstructor
            .reconstruct_to_writer(writer)
            .await
            .map_err(|e| {
                error::CrabError::Internal(format!(
                    "file reconstruction failed for {}: {e}",
                    file_hash.hex()
                ))
            })?;

        let content = {
            let mut guard = shared.lock().map_err(|_| {
                error::CrabError::Internal("reconstruction writer poisoned".to_string())
            })?;
            std::mem::take(guard.get_mut())
        };

        tracing::debug!(
            file_hash = %file_hash.hex(),
            reconstructed_bytes = content.len(),
            expected_bytes = ptr.size,
            "reconstruct: FileReconstructor returned content"
        );

        // Verify blake3 hash. The FileReconstructor does not perform
        // this check internally — it is the caller's responsibility.
        let actual_hash: [u8; 32] = *blake3::hash(&content).as_bytes();
        if actual_hash != ptr.file_hash {
            tracing::error!(
                file_hash = %file_hash.hex(),
                expected_hash = %crab_types::pointer::hex_encode(&ptr.file_hash),
                actual_hash = %crab_types::pointer::hex_encode(&actual_hash),
                reconstructed_bytes = content.len(),
                expected_bytes = ptr.size,
                "reconstruct: HASH MISMATCH — reconstructed content does not \
                 match pointer. If reconstructed_bytes ≈ 2× expected_bytes, \
                 this is likely the duplicate-staging bug (CRAB-E0082). \
                 Re-push the file to regenerate the shard: \
                 crab add <file> && git add <file> && git commit --amend \
                 --no-edit && git push --force"
            );
            // Report hashes in the same raw-byte hex format as the pointer
            // file on disk, so a user can grep-match the returned error
            // against `git show :path`. `MerkleHash::hex()` uses a
            // little-endian u64 rendering that does not match the pointer
            // text format. See S1-P2-3.
            return Err(error::CrabError::HashMismatch {
                requested: crab_types::pointer::hex_encode(&ptr.file_hash),
                actual: crab_types::pointer::hex_encode(&actual_hash),
            });
        }

        Ok(content)
    }

    /// Reconstruct a file from its pointer and stream the bytes
    /// directly into `dest`, verifying the blake3 hash on the fly.
    ///
    /// Memory bound is independent of file size: the reconstructor
    /// writes chunks into a tee writer that forwards them to the
    /// destination file and feeds them into a
    /// [`blake3::Hasher`] incrementally. Peak RSS during a
    /// reconstruction is dominated by the xet-core chunk pipeline
    /// itself (bounded by the shared [`AdaptiveConcurrencyController`]
    /// and the xorb-chunk LRU), not by a whole-file `Vec<u8>`.
    ///
    /// Integrity: the hasher is finalised after
    /// [`FileReconstructor::reconstruct_to_writer`] returns and
    /// compared against [`Pointer::file_hash`]. A mismatch deletes
    /// the destination file before returning so a caller that
    /// catches the error does not leave a half-corrupt file on disk
    /// under the target path.
    ///
    /// Atomicity: the write is not atomic on its own — we call
    /// `File::create(dest)`, which truncates. Callers that need
    /// atomic replacement should write to a sibling path and
    /// `rename(2)` on success. Keeping the semantics simple here
    /// mirrors `reconstruct_file` and matches what the CLI's
    /// `crab hydrate` actually wants (atomic write is a higher-
    /// layer concern around this primitive).
    ///
    /// # Returns
    ///
    /// Number of bytes written to `dest`. Guaranteed to equal
    /// `ptr.size` on a successful call — the blake3 check fails
    /// first if the size is off, so a successful return also
    /// witnesses size agreement.
    ///
    /// # Errors
    ///
    /// - [`error::CrabError::Io`] for destination-file failures
    ///   (can't create, partial write, flush failure). The
    ///   destination is removed on any post-create failure to
    ///   prevent half-written content from sticking around under
    ///   the target path.
    /// - [`error::CrabError::HashMismatch`] when the reconstructed
    ///   bytes hash to a different blake3 than the pointer declares.
    ///   The destination is removed.
    /// - Anything
    ///   [`xet_data::file_reconstruction::FileReconstructor::reconstruct_to_writer`]
    ///   can return (network failures, missing chunks, shard
    ///   resolution errors). The destination is removed.
    pub async fn reconstruct_to_path(&self, ptr: &Pointer, dest: &std::path::Path) -> Result<u64> {
        self.reconstruct_to_path_with_progress(ptr, dest, None)
            .await
    }

    /// Reconstruct a file from its pointer into an arbitrary blocking writer.
    pub async fn reconstruct_to_writer<W>(&self, ptr: &Pointer, writer: W) -> Result<u64>
    where
        W: Write + Send + 'static,
    {
        self.reconstruct_to_writer_with(ptr, writer, None, &CancellationToken::new())
            .await
    }

    async fn reconstruct_to_writer_with<W>(
        &self,
        ptr: &Pointer,
        writer: W,
        file_index_lookup: Option<&SharedFileIndexLookup>,
        cancel: &CancellationToken,
    ) -> Result<u64>
    where
        W: Write + Send + 'static,
    {
        error::check_cancelled(cancel)?;
        let file_hash = MerkleHash::from(ptr.file_hash);

        let client = self.store_client_for_pointer(file_index_lookup, ptr);

        self.preflight_shard_coverage(&client, ptr).await?;

        let tap_state = Arc::new(std::sync::Mutex::new(GenericHasherTapState {
            writer: Some(writer),
            hasher: blake3::Hasher::new(),
            bytes_written: 0,
        }));
        let writer = GenericHasherTap {
            shared: tap_state.clone(),
        };

        let xet_context = new_xet_context()?;
        // xet-data cancels its supplied token when an internal worker fails.
        // Isolate that signal so the operation token still distinguishes a
        // user cancellation from the reconstruction error returned below.
        let reconstruction_cancel = cancel.child_token();
        let reconstructor =
            xet_data::file_reconstruction::FileReconstructor::new(&xet_context, &client, file_hash)
                .with_buffer_semaphore(Arc::clone(&self.buffer_semaphore))
                .with_cancellation_token(reconstruction_cancel);
        let reconstructor = match self.chunk_cache.clone() {
            Some(cache) => reconstructor.with_chunk_cache(cache),
            None => reconstructor,
        };

        if let Err(e) = reconstructor.reconstruct_to_writer(writer).await {
            if cancel.is_cancelled() {
                return Err(error::CrabError::Cancelled);
            }
            return Err(error::CrabError::Internal(format!(
                "file reconstruction failed for {}: {e}",
                file_hash.hex(),
            )));
        }

        let (actual_hash, bytes_written) = {
            let mut guard = tap_state
                .lock()
                .map_err(|_| error::CrabError::Internal("hasher tap poisoned".to_owned()))?;
            if let Some(writer) = guard.writer.as_mut() {
                writer.flush().map_err(error::CrabError::Io)?;
            }
            drop(guard.writer.take());
            let hash: [u8; 32] = guard.hasher.finalize().into();
            (hash, guard.bytes_written)
        };

        if actual_hash != ptr.file_hash {
            return Err(error::CrabError::HashMismatch {
                requested: crab_types::pointer::hex_encode(&ptr.file_hash),
                actual: crab_types::pointer::hex_encode(&actual_hash),
            });
        }
        if bytes_written != ptr.size {
            return Err(error::CrabError::Internal(format!(
                "file reconstruction size mismatch for {}: expected {}, got {}",
                file_hash.hex(),
                ptr.size,
                bytes_written,
            )));
        }

        Ok(bytes_written)
    }

    async fn reconstruct_to_path_with_progress(
        &self,
        ptr: &Pointer,
        dest: &std::path::Path,
        progress: Option<Arc<HydrateProgress>>,
    ) -> Result<u64> {
        let file = std::fs::File::create(dest).map_err(error::CrabError::Io)?;
        self.reconstruct_to_open_file(ptr, file, dest, progress, None, CancellationToken::new())
            .await
    }

    async fn reconstruct_to_atomic_path_with_cancel(
        &self,
        ptr: &Pointer,
        dest: &std::path::Path,
        progress: Option<Arc<HydrateProgress>>,
        file_index_lookup: Option<&SharedFileIndexLookup>,
        cancel: CancellationToken,
    ) -> Result<VerifiedWrite> {
        error::check_cancelled(&cancel)?;
        let parent = dest.parent().unwrap_or(Path::new("."));
        ensure_atomic_reconstruction_space(parent, ptr.size)?;
        let tmp = tempfile::NamedTempFile::new_in(parent).map_err(error::CrabError::Io)?;
        let tmp_path = tmp.path().to_owned();
        let file = tmp.reopen().map_err(error::CrabError::Io)?;
        let bytes = self
            .reconstruct_to_open_file(
                ptr,
                file,
                &tmp_path,
                progress,
                file_index_lookup,
                cancel.clone(),
            )
            .await?;
        error::check_cancelled(&cancel)?;
        persist_verified_temp(tmp, dest, bytes)
    }

    async fn reconstruct_to_open_file(
        &self,
        ptr: &Pointer,
        file: std::fs::File,
        cleanup_path: &std::path::Path,
        progress: Option<Arc<HydrateProgress>>,
        file_index_lookup: Option<&SharedFileIndexLookup>,
        cancel: CancellationToken,
    ) -> Result<u64> {
        let file_hash = MerkleHash::from(ptr.file_hash);

        let client = self.store_client_for_pointer(file_index_lookup, ptr);

        // Pre-flight: detect partial-coverage shards before we open
        // the destination file. Avoids creating an empty `dest` that
        // we'd then have to clean up when the reconstruction can't
        // possibly produce the full content.
        self.preflight_shard_coverage(&client, ptr).await?;

        // Open the destination synchronously — the reconstructor
        // wants a `W: Write + Send + 'static`, and a `std::fs::File`
        // is the cheapest thing that satisfies those bounds.
        // `create` truncates any existing content, matching
        // `tokio::fs::File::create` semantics the previous in-memory
        // `download()` used.
        let dest_for_cleanup = cleanup_path.to_owned();

        // Wrap the file in a tee writer that also feeds the
        // blake3 hasher incrementally. The shared state holds both
        // so the caller can extract the finalised hash after
        // `reconstruct_to_writer` has moved and dropped the writer.
        // `Arc<Mutex>` is the cheapest way to satisfy the
        // `W: Send + 'static` bound while retaining post-call
        // visibility into the hasher; any lock-free alternative
        // trades safety for complexity we don't need here.
        let tap_state = Arc::new(std::sync::Mutex::new(HasherTapState {
            file: Some(file),
            hasher: blake3::Hasher::new(),
            bytes_written: 0,
            progress,
        }));
        let writer = HasherTap {
            shared: tap_state.clone(),
        };

        let xet_context = new_xet_context()?;
        // Keep xet-data's error-triggered cancellation local to this file;
        // otherwise one failed reconstruction poisons the whole hydrate batch.
        let reconstruction_cancel = cancel.child_token();
        let reconstructor =
            xet_data::file_reconstruction::FileReconstructor::new(&xet_context, &client, file_hash)
                .with_buffer_semaphore(Arc::clone(&self.buffer_semaphore))
                .with_cancellation_token(reconstruction_cancel);
        let reconstructor = match self.chunk_cache.clone() {
            Some(cache) => reconstructor.with_chunk_cache(cache),
            None => reconstructor,
        };

        // Any failure below must delete `dest` so a caller that
        // retries doesn't observe a half-written file at the target
        // path. `remove_file` errors are swallowed — the
        // reconstruction error is the load-bearing one, and a
        // missing-destination cleanup failure is a strictly worse
        // signal to surface.
        let cleanup = |err: error::CrabError| -> error::CrabError {
            let _ = std::fs::remove_file(&dest_for_cleanup);
            err
        };

        if let Err(e) = reconstructor.reconstruct_to_writer(writer).await {
            if cancel.is_cancelled() {
                return Err(cleanup(error::CrabError::Cancelled));
            }
            return Err(cleanup(error::CrabError::Internal(format!(
                "file reconstruction failed for {}: {e}",
                file_hash.hex(),
            ))));
        }

        // The writer dropped when `reconstruct_to_writer` returned —
        // finalise the hasher and verify against the pointer.
        let (actual_hash, bytes_written) = {
            let mut guard = tap_state.lock().map_err(|_| {
                cleanup(error::CrabError::Internal("hasher tap poisoned".to_owned()))
            })?;
            // Drop the file handle first so the kernel buffer is
            // flushed before we finalise — ensures the `stat(dest)`
            // a caller would do immediately after sees the full
            // content, not a partial prefix.
            drop(guard.file.take());
            let hash: [u8; 32] = guard.hasher.finalize().into();
            (hash, guard.bytes_written)
        };

        if actual_hash != ptr.file_hash {
            tracing::error!(
                file_hash = %file_hash.hex(),
                expected_hash = %crab_types::pointer::hex_encode(&ptr.file_hash),
                actual_hash = %crab_types::pointer::hex_encode(&actual_hash),
                reconstructed_bytes = bytes_written,
                expected_bytes = ptr.size,
                "reconstruct_to_path: HASH MISMATCH — streamed content \
                 does not match pointer; destination removed",
            );
            return Err(cleanup(error::CrabError::HashMismatch {
                requested: crab_types::pointer::hex_encode(&ptr.file_hash),
                actual: crab_types::pointer::hex_encode(&actual_hash),
            }));
        }

        tracing::debug!(
            file_hash = %file_hash.hex(),
            bytes_written,
            expected_bytes = ptr.size,
            dest = %cleanup_path.display(),
            "reconstruct_to_path: streamed reconstruction complete",
        );

        Ok(bytes_written)
    }

    /// Variant of [`Self::reconstruct_to_path`] that accepts raw
    /// pointer bytes.
    ///
    /// Parses the pointer with
    /// [`crab_types::pointer::Pointer::parse`] and delegates.
    /// Handy for call sites that already have the pointer blob in
    /// hand and don't want to re-parse twice.
    pub async fn reconstruct_from_pointer_to_path(
        &self,
        pointer_bytes: &[u8],
        dest: &std::path::Path,
    ) -> Result<u64> {
        let ptr = crab_types::pointer::Pointer::parse(pointer_bytes)?;
        self.reconstruct_to_path(&ptr, dest).await
    }

    /// Reconstruct a byte range of a file from its pointer.
    ///
    /// Returns the bytes covering `[start, end)` of the original file.
    /// Used by the SDK's streaming [`crate::cmd::hydrate::HydrationService`]
    /// to fetch only the chunks overlapping a seek + read. The range
    /// is clamped to the pointer's declared size; `start >= size`
    /// returns an empty buffer, and `end > size` is treated as
    /// `end = size`.
    ///
    /// Unlike `reconstruct_file`, this path does **not** blake3-verify
    /// the returned bytes against the pointer hash — blake3 is a
    /// whole-file digest, and a partial range has no equivalent.
    /// xet-core's `FileReconstructor` still enforces per-chunk
    /// integrity internally, so a tampered chunk on the wire
    /// surfaces as a reconstruction error.
    ///
    /// # Errors
    ///
    /// - [`error::CrabError::Internal`] if range arithmetic is
    ///   malformed (`end < start`).
    /// - Any error
    ///   [`xet_data::file_reconstruction::FileReconstructor::reconstruct_to_writer`]
    ///   can return — network failures, missing chunks, shard
    ///   resolution errors — propagated with the full source chain.
    pub async fn reconstruct_range_from_pointer(
        &self,
        pointer_bytes: &[u8],
        start: u64,
        end: u64,
    ) -> Result<Vec<u8>> {
        let ptr = crab_types::pointer::Pointer::parse(pointer_bytes)?;

        if end < start {
            return Err(error::CrabError::Internal(format!(
                "reconstruct_range_from_pointer: end ({end}) < start ({start})"
            )));
        }

        // Clamp against the declared file size. Reading past EOF
        // simply returns fewer bytes; that matches `Read::read` on a
        // std::fs::File and the Python file-object protocol.
        let end = end.min(ptr.size);
        let start = start.min(ptr.size);
        if start >= end {
            return Ok(Vec::new());
        }

        let file_hash = MerkleHash::from(ptr.file_hash);

        let file_index_lookup = self.shared_file_index_lookup();
        let client = self.store_client_for_pointer(Some(&file_index_lookup), &ptr);

        #[expect(
            clippy::cast_possible_truncation,
            reason = "range bounds fit usize on every platform crab runs on"
        )]
        let capacity = (end - start) as usize;
        let buffer: std::io::Cursor<Vec<u8>> = std::io::Cursor::new(Vec::with_capacity(capacity));
        let shared = Arc::new(std::sync::Mutex::new(buffer));
        let writer = SharedCursorWriter(shared.clone());

        let range = xet_client::cas_types::FileRange::new(start, end);
        let xet_context = new_xet_context()?;
        let reconstructor =
            xet_data::file_reconstruction::FileReconstructor::new(&xet_context, &client, file_hash)
                .with_buffer_semaphore(Arc::clone(&self.buffer_semaphore))
                .with_byte_range(range);
        let reconstructor = match self.chunk_cache.clone() {
            Some(cache) => reconstructor.with_chunk_cache(cache),
            None => reconstructor,
        };

        let reconstruction_result =
            reconstructor
                .reconstruct_to_writer(writer)
                .await
                .map_err(|e| {
                    error::CrabError::Internal(format!(
                        "range reconstruction failed for {} [{start}..{end}]: {e}",
                        file_hash.hex()
                    ))
                });

        if let Err(e) = file_index_lookup.close().await {
            warn!(err = %e, "range reconstruction file-index lookup session close failed");
        }

        reconstruction_result?;

        let content = {
            let mut guard = shared.lock().map_err(|_| {
                error::CrabError::Internal("reconstruction writer poisoned".to_string())
            })?;
            std::mem::take(guard.get_mut())
        };

        Ok(content)
    }

    /// Attempt delta reconstruction using an existing file on disk as the
    /// base version.
    ///
    /// Reads the current file at `path`, resolves its reconstruction terms
    /// from the shard metadata, then uses [`reconstruct_from_delta`] to
    /// build the target version by reusing unchanged chunks.
    ///
    /// Returns an error if:
    /// - The file at `path` doesn't exist or isn't a hydrated file
    /// - The base file's reconstruction terms can't be resolved
    /// - The reuse ratio is too low to justify the delta path (<10%)
    async fn try_delta_reconstruct(
        &self,
        path: &Path,
        target_ptr: &Pointer,
        file_index_lookup: Option<&SharedFileIndexLookup>,
    ) -> Result<Vec<u8>> {
        use crate::git::delta_reconstruct::{estimate_reuse_ratio, reconstruct_from_delta};

        // Read the existing file on disk. If it's a pointer or doesn't
        // exist, delta reconstruction isn't possible.
        let base_content = tokio::fs::read(path).await.map_err(|e| {
            error::CrabError::Internal(format!("cannot read base file for delta: {e}"))
        })?;

        // Don't try delta if the file is a pointer (not hydrated).
        if is_working_tree_pointer(path).unwrap_or(false) {
            return Err(error::CrabError::Internal(
                "base file is a pointer, not hydrated".into(),
            ));
        }

        // Compute the base file's hash to look up its reconstruction terms.
        let base_hash_bytes: [u8; 32] = *blake3::hash(&base_content).as_bytes();
        let base_hash = MerkleHash::from(base_hash_bytes);

        // Resolve base file's shard and reconstruction terms.
        let base_shard_hash = self
            .resolve_file_index(&base_hash, file_index_lookup)
            .await?;
        let base_shard = self
            .get_or_download_shard_for_file(&base_shard_hash, &base_hash)
            .await?;
        let base_file_info =
            base_shard
                .get_file_info(&base_hash)?
                .ok_or_else(|| error::CrabError::NotFound {
                    path: format!("base file-info {}", base_hash.hex()),
                })?;

        // Resolve target file's shard and reconstruction terms.
        let target_hash = MerkleHash::from(target_ptr.file_hash);
        let target_shard_hash = self
            .resolve_file_index(&target_hash, file_index_lookup)
            .await?;
        let target_shard = self
            .get_or_download_shard_for_file(&target_shard_hash, &target_hash)
            .await?;
        let target_file_info = target_shard.get_file_info(&target_hash)?.ok_or_else(|| {
            error::CrabError::NotFound {
                path: format!("target file-info {}", target_hash.hex()),
            }
        })?;

        // Convert FileDataSequenceEntry segments to ReconstructionTerms.
        // This requires resolving each segment's xorb to get chunk-level
        // offsets. For the delta path, we use a simplified mapping where
        // each segment maps to one ReconstructionTerm.
        let base_terms = self.segments_to_terms(&base_file_info.segments).await?;
        let target_terms = self.segments_to_terms(&target_file_info.segments).await?;

        // Check if delta is worthwhile.
        let reuse_ratio = estimate_reuse_ratio(&base_terms, &target_terms);
        if reuse_ratio < 0.1 {
            return Err(error::CrabError::Internal(format!(
                "reuse ratio too low for delta: {:.1}%",
                reuse_ratio * 100.0
            )));
        }

        tracing::info!(
            path = %path.display(),
            reuse_ratio = format!("{:.1}%", reuse_ratio * 100.0),
            base_segments = base_terms.len(),
            target_segments = target_terms.len(),
            "using delta reconstruction"
        );

        // Build a pre-populated fetcher with all target chunks that need
        // fetching. We decompress them now so the delta engine can verify
        // blake3 hashes directly.
        let fetcher = self.build_delta_fetcher(&target_terms).await?;

        let delta_result =
            reconstruct_from_delta(&base_terms, &base_content, &target_terms, &fetcher, None)?;

        // Verify the final content.
        let actual_hash: [u8; 32] = *blake3::hash(&delta_result.content).as_bytes();
        if actual_hash != target_ptr.file_hash {
            // See S1-P2-3 — report hashes using raw byte hex for
            // consistency with the pointer file on disk.
            return Err(error::CrabError::HashMismatch {
                requested: crab_types::pointer::hex_encode(&target_ptr.file_hash),
                actual: crab_types::pointer::hex_encode(&actual_hash),
            });
        }

        tracing::info!(
            path = %path.display(),
            reused_bytes = delta_result.reused_bytes,
            fetched_bytes = delta_result.fetched_bytes,
            "delta reconstruction complete"
        );

        Ok(delta_result.content)
    }

    /// Convert `FileDataSequenceEntry` segments into `ReconstructionTerm`s.
    ///
    /// Each segment references a range of chunks within an xorb. We fetch
    /// the xorb and parse it to extract per-chunk byte offsets and hashes.
    /// The `offset` and `length` in each term use synthetic values that
    /// are consistent with what [`build_delta_fetcher`] pre-populates.
    async fn segments_to_terms(
        &self,
        segments: &[FileDataSequenceEntry],
    ) -> Result<Vec<crate::git::smudge::ReconstructionTerm>> {
        use crab_xet::xorb::parser::XorbParser;

        let mut terms = Vec::new();
        for seg in segments {
            let xorb_hash_bytes: [u8; 32] = seg.xorb_hash.into();
            let segment_terms = self
                .decode_xorb_with_cache_repair(&seg.xorb_hash, |xorb_data| {
                    let parser = XorbParser::parse(xorb_data)?;
                    let mut segment_terms = Vec::new();
                    for idx in seg.chunk_index_start..seg.chunk_index_end {
                        let chunk = parser.get_chunk(idx)?;
                        let chunk_hash = *blake3::hash(&chunk.data).as_bytes();
                        // Use chunk index as synthetic offset and uncompressed
                        // length as the range length. The ShardHydratorXorbFetcher
                        // is keyed by (xorb_hash, offset, length) using these
                        // same values.
                        segment_terms.push(crate::git::smudge::ReconstructionTerm {
                            xorb_hash: xorb_hash_bytes,
                            offset: u64::from(idx),
                            length: chunk.data.len() as u64,
                            chunk_hash,
                        });
                    }
                    Ok(segment_terms)
                })
                .await?;
            terms.extend(segment_terms);
        }
        Ok(terms)
    }

    /// Build a pre-populated [`ShardHydratorXorbFetcher`] containing
    /// decompressed chunk data for all terms that the delta engine might
    /// need to fetch.
    async fn build_delta_fetcher(
        &self,
        terms: &[crate::git::smudge::ReconstructionTerm],
    ) -> Result<ShardHydratorXorbFetcher> {
        use crab_xet::xorb::parser::XorbParser;

        let mut chunks = std::collections::HashMap::new();
        // Group terms by xorb hash to avoid re-parsing the same xorb.
        let mut by_xorb: std::collections::HashMap<
            [u8; 32],
            Vec<&crate::git::smudge::ReconstructionTerm>,
        > = std::collections::HashMap::new();
        for term in terms {
            by_xorb.entry(term.xorb_hash).or_default().push(term);
        }

        for (xorb_hash_bytes, xorb_terms) in &by_xorb {
            let merkle_hash = MerkleHash::from(*xorb_hash_bytes);
            let xorb_chunks = self
                .decode_xorb_with_cache_repair(&merkle_hash, |xorb_data| {
                    let parser = XorbParser::parse(xorb_data)?;
                    let mut xorb_chunks = Vec::new();
                    for term in xorb_terms {
                        // term.offset is the chunk index (synthetic, set by segments_to_terms).
                        let chunk_idx = term.offset as u32;
                        let chunk = parser.get_chunk(chunk_idx)?;
                        let key = (*xorb_hash_bytes, term.offset, term.length);
                        xorb_chunks.push((key, chunk.data.to_vec()));
                    }
                    Ok(xorb_chunks)
                })
                .await?;
            chunks.extend(xorb_chunks);
        }

        Ok(ShardHydratorXorbFetcher { chunks })
    }
}

fn new_xet_context() -> Result<xet_runtime::core::XetContext> {
    xet_runtime::core::XetContext::default().map_err(|error| {
        error::CrabError::Internal(format!("failed to initialize xet context: {error}"))
    })
}

fn fixed_hydrate_concurrency(
    concurrency: usize,
) -> Result<Arc<xet_client::cas_client::adaptive_concurrency::AdaptiveConcurrencyController>> {
    let context = new_xet_context()?;
    Ok(
        xet_client::cas_client::adaptive_concurrency::AdaptiveConcurrencyController::new_fixed(
            context,
            HYDRATE_CONCURRENCY_TAG,
            concurrency,
        ),
    )
}

impl ShardHydrator {
    async fn hydrate_one(
        &self,
        path: &Path,
        ptr: &Pointer,
        cancel: &CancellationToken,
        progress: Option<&Arc<HydrateProgress>>,
        file_index_lookup: &SharedFileIndexLookup,
    ) -> Result<HydrateOneResult> {
        error::check_cancelled(cancel)?;

        if let Some(progress) = progress {
            let name = path
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_default();
            progress.set_current_file(&name);
        }

        let file_start = Instant::now();
        if is_already_hydrated(path, ptr, cancel)? {
            tracing::debug!(path = %path.display(), "already hydrated, skipping");
            let result = HydrateFileResult {
                path: path.to_owned(),
                outcome: HydrateFileOutcome::Skipped,
                duration: file_start.elapsed(),
                bytes: ptr.size,
            };
            if let Some(progress) = progress {
                progress.files_done.fetch_add(1, Relaxed);
                progress.bytes_done.fetch_add(ptr.size, Relaxed);
                progress.send_file_result(result.clone());
            }
            return Ok(HydrateOneResult {
                result,
                verified_path: None,
            });
        }

        let result = match self
            .try_delta_reconstruct(path, ptr, Some(file_index_lookup))
            .await
        {
            Ok(content) => {
                let bytes = content.len() as u64;
                atomic_write_with_progress(path, &content, progress)
                    .map(|index_stat| VerifiedWrite { bytes, index_stat })
            }
            Err(e) => {
                tracing::debug!(
                    path = %path.display(),
                    err = %e,
                    "delta reconstruction unavailable, falling back to full"
                );
                self.reconstruct_to_atomic_path_with_cancel(
                    ptr,
                    path,
                    progress.cloned(),
                    Some(file_index_lookup),
                    cancel.clone(),
                )
                .await
            }
        };
        error::check_cancelled(cancel)?;
        let result = match result {
            Ok(bytes) => Ok(bytes),
            Err(remote_error) => match try_hydrate_from_staging(path, ptr, cancel).await {
                Ok(Some(write)) => {
                    if let Some(progress) = progress {
                        progress.add_bytes_done(write.bytes);
                    }
                    Ok(write)
                }
                Ok(None) => Err(remote_error),
                Err(staging_error) => {
                    warn!(
                        path = %path.display(),
                        error = %staging_error,
                        "local staging hydrate fallback failed"
                    );
                    Err(remote_error)
                }
            },
        };

        let (outcome, write) = match result {
            Ok(write) => (HydrateFileOutcome::Hydrated, Some(write)),
            Err(e) => {
                tracing::warn!(path = %path.display(), err = %e, "reconstruction failed");
                (HydrateFileOutcome::Failed, None)
            }
        };
        let bytes = write.map_or(0, |write| write.bytes);
        let result = HydrateFileResult {
            path: path.to_owned(),
            outcome,
            duration: file_start.elapsed(),
            bytes,
        };
        if let Some(progress) = progress {
            progress.files_done.fetch_add(1, Relaxed);
            progress.send_file_result(result.clone());
        }
        Ok(HydrateOneResult {
            result,
            verified_path: write.map(|write| verified_path(path, ptr, write)),
        })
    }
}

struct HydrateOneResult {
    result: HydrateFileResult,
    verified_path: Option<crate::cache::add_validation::VerifiedPath>,
}

impl Hydrator for ShardHydrator {
    fn hydrate_batch<'a>(
        &'a self,
        items: &'a [(PathBuf, Pointer)],
        cancel: &'a CancellationToken,
        progress: Option<&'a Arc<HydrateProgress>>,
    ) -> Pin<Box<dyn Future<Output = Result<HydrateSummary>> + Send + 'a>> {
        Box::pin(async move {
            let file_index_lookup = self.shared_file_index_lookup();
            let result = async {
                let mut summary = HydrateSummary::default();
                let mut remaining = items.iter();
                let mut results = FuturesUnordered::new();
                for (path, ptr) in remaining.by_ref().take(self.file_concurrency) {
                    results.push(self.hydrate_one(path, ptr, cancel, progress, &file_index_lookup));
                }

                // Refill the bounded set as each file completes. Fixed-size waves
                // leave download slots idle behind one large straggler.
                while let Some(result) = results.next().await {
                    let result = result?;
                    match result.result.outcome {
                        HydrateFileOutcome::Hydrated => {
                            summary.hydrated += 1;
                            summary.bytes_written += result.result.bytes;
                            if let Some(verified) = result.verified_path {
                                summary.verified_paths.push(verified);
                            }
                        }
                        HydrateFileOutcome::Skipped => {
                            summary.skipped += 1;
                            summary.bytes_skipped += result.result.bytes;
                        }
                        HydrateFileOutcome::Failed => {
                            summary.failed += 1;
                        }
                    }
                    if let Some((path, ptr)) = remaining.next() {
                        results.push(self.hydrate_one(
                            path,
                            ptr,
                            cancel,
                            progress,
                            &file_index_lookup,
                        ));
                    }
                }

                Ok(summary)
            }
            .await;

            if let Err(e) = file_index_lookup.close().await {
                warn!(err = %e, "hydrate file-index lookup session close failed");
            }
            result
        })
    }
}

/// Adapter that bridges [`ShardHydrator`]'s xorb cache to the
/// synchronous [`XorbFetcher`] trait used by the delta reconstruction
/// engine.
///
/// Pre-populates a map of `(xorb_hash_bytes, offset, length) → decompressed_data`
/// so that `fetch_range` calls from the delta engine resolve locally
/// without any async bridging.
struct ShardHydratorXorbFetcher {
    /// Pre-resolved chunk data keyed by `(xorb_hash, offset, length)`.
    chunks: std::collections::HashMap<([u8; 32], u64, u64), Vec<u8>>,
}

/// A `'static + Send` writer that appends into a shared `Cursor<Vec<u8>>`.
///
/// xet-core's [`FileReconstructor::reconstruct_to_writer`] bounds the
/// writer `W: Write + Send + 'static`, so we can't borrow a local
/// buffer directly. Wrapping the cursor in `Arc<Mutex<_>>` and handing
/// out one writer satisfies the bound while letting the caller pull
/// bytes out after the reconstruction resolves.
struct SharedCursorWriter(Arc<std::sync::Mutex<std::io::Cursor<Vec<u8>>>>);

impl std::io::Write for SharedCursorWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .map_err(|_| std::io::Error::other("shared cursor mutex poisoned"))?
            .write(buf)
    }

    fn write_vectored(&mut self, bufs: &[std::io::IoSlice<'_>]) -> std::io::Result<usize> {
        self.0
            .lock()
            .map_err(|_| std::io::Error::other("shared cursor mutex poisoned"))?
            .write_vectored(bufs)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.0
            .lock()
            .map_err(|_| std::io::Error::other("shared cursor mutex poisoned"))?
            .flush()
    }
}

/// Writer state shared between [`HasherTap`] and
/// [`ShardHydrator::reconstruct_to_path`].
///
/// Both sides need access to the destination file (for writing) and
/// the blake3 hasher (for whole-file integrity verification). The
/// file lives inside an `Option` so the caller can `.take()` it to
/// drop the handle — releasing the fd and flushing the kernel buffer
/// — before finalising the hasher.
struct HasherTapState {
    /// Destination file handle. `None` after the caller explicitly
    /// drops it post-reconstruction.
    file: Option<std::fs::File>,
    /// Incremental blake3 hasher updated with every chunk the
    /// reconstructor writes. Finalised after the writer is dropped
    /// to compute the whole-file hash for integrity verification.
    hasher: blake3::Hasher,
    /// Running count of bytes successfully written to `file`. Equal
    /// to `ptr.size` on a successful reconstruction.
    bytes_written: u64,
    /// Optional hydrate progress sink. Updated from the reconstructor's
    /// writer thread so large single-file hydrations advance live.
    progress: Option<Arc<HydrateProgress>>,
}

struct GenericHasherTapState<W: Write> {
    writer: Option<W>,
    hasher: blake3::Hasher,
    bytes_written: u64,
}

struct GenericHasherTap<W: Write> {
    shared: Arc<std::sync::Mutex<GenericHasherTapState<W>>>,
}

fn hash_vectored_prefix(
    hasher: &mut blake3::Hasher,
    bufs: &[std::io::IoSlice<'_>],
    mut written: usize,
) {
    for buf in bufs {
        if written == 0 {
            break;
        }
        let take = written.min(buf.len());
        hasher.update(&buf[..take]);
        written -= take;
    }
}

impl<W: Write> std::io::Write for GenericHasherTap<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut guard = self
            .shared
            .lock()
            .map_err(|_| std::io::Error::other("hasher tap mutex poisoned"))?;
        let written = guard
            .writer
            .as_mut()
            .ok_or_else(|| std::io::Error::other("hasher tap: writer already taken"))?
            .write(buf)?;
        guard.hasher.update(&buf[..written]);
        guard.bytes_written = guard.bytes_written.saturating_add(written as u64);
        Ok(written)
    }

    fn write_vectored(&mut self, bufs: &[std::io::IoSlice<'_>]) -> std::io::Result<usize> {
        let mut guard = self
            .shared
            .lock()
            .map_err(|_| std::io::Error::other("hasher tap mutex poisoned"))?;
        let written = guard
            .writer
            .as_mut()
            .ok_or_else(|| std::io::Error::other("hasher tap: writer already taken"))?
            .write_vectored(bufs)?;
        hash_vectored_prefix(&mut guard.hasher, bufs, written);
        guard.bytes_written = guard.bytes_written.saturating_add(written as u64);
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let mut guard = self
            .shared
            .lock()
            .map_err(|_| std::io::Error::other("hasher tap mutex poisoned"))?;
        match guard.writer.as_mut() {
            Some(writer) => writer.flush(),
            None => Ok(()),
        }
    }
}

/// A `Write + Send + 'static` writer that fans every byte into both
/// a destination file and an incremental blake3 hasher.
///
/// xet-core's [`xet_data::file_reconstruction::FileReconstructor::reconstruct_to_writer`]
/// bounds the writer `W: Write + Send + 'static`, so we need owned
/// access. A plain [`std::fs::File`] satisfies the bound but gives no
/// hook for integrity verification — once the writer is moved and
/// dropped by the reconstructor, there's no way to recover the bytes
/// for hashing.
///
/// This adapter solves that: it owns an `Arc<Mutex<HasherTapState>>`
/// that both the write path and the finalisation path hold. Writes
/// feed the hasher inline so the memory footprint stays bounded
/// (single write-buffer sized, not file sized), and the caller
/// pulls the finalised hash out of the shared state after the
/// reconstructor returns.
struct HasherTap {
    shared: Arc<std::sync::Mutex<HasherTapState>>,
}

impl std::io::Write for HasherTap {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut guard = self
            .shared
            .lock()
            .map_err(|_| std::io::Error::other("hasher tap mutex poisoned"))?;
        let written = guard
            .file
            .as_mut()
            .ok_or_else(|| std::io::Error::other("hasher tap: destination file already taken"))?
            .write(buf)?;
        guard.hasher.update(&buf[..written]);
        guard.bytes_written = guard.bytes_written.saturating_add(written as u64);
        if let Some(progress) = &guard.progress {
            progress.add_bytes_done(written as u64);
        }
        Ok(written)
    }

    fn write_vectored(&mut self, bufs: &[std::io::IoSlice<'_>]) -> std::io::Result<usize> {
        let mut guard = self
            .shared
            .lock()
            .map_err(|_| std::io::Error::other("hasher tap mutex poisoned"))?;
        let written = guard
            .file
            .as_mut()
            .ok_or_else(|| std::io::Error::other("hasher tap: destination file already taken"))?
            .write_vectored(bufs)?;
        hash_vectored_prefix(&mut guard.hasher, bufs, written);
        guard.bytes_written = guard.bytes_written.saturating_add(written as u64);
        if let Some(progress) = &guard.progress {
            progress.add_bytes_done(written as u64);
        }
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let mut guard = self
            .shared
            .lock()
            .map_err(|_| std::io::Error::other("hasher tap mutex poisoned"))?;
        match guard.file.as_mut() {
            Some(file) => file.flush(),
            // Dropped already — nothing to flush. Treat as success
            // so a late `flush` call from the reconstructor's
            // shutdown path doesn't trip a spurious error.
            None => Ok(()),
        }
    }
}

impl crate::git::smudge::XorbFetcher for ShardHydratorXorbFetcher {
    fn fetch_range(
        &self,
        xorb_hash: &[u8; 32],
        range: std::ops::Range<u64>,
    ) -> crab_vfs::Result<Vec<u8>> {
        let key = (*xorb_hash, range.start, range.end - range.start);
        self.chunks
            .get(&key)
            .cloned()
            .ok_or_else(|| crab_vfs::VfsError::NotFound {
                path: format!(
                    "xorb {} range {}..{}",
                    crab_types::pointer::hex_encode(xorb_hash),
                    range.start,
                    range.end,
                ),
            })
    }
}

fn ensure_atomic_reconstruction_space(parent: &Path, needed: u64) -> Result<()> {
    if let Some(available) = crate::workflow::cache::available_disk_space(parent)
        && available < needed
    {
        return Err(error::CrabError::InsufficientSpace { needed, available });
    }
    Ok(())
}

/// Stub hydrator that writes pointer bytes back unchanged.
///
/// Useful for testing the walk/filter/atomic-write machinery without
/// requiring a real smudge pipeline. Exercises the full atomic-write
/// path (tempfile → rename) so crash-safety is validated.
pub struct StubHydrator;

impl Hydrator for StubHydrator {
    fn hydrate_batch<'a>(
        &'a self,
        items: &'a [(PathBuf, Pointer)],
        cancel: &'a CancellationToken,
        progress: Option<&'a Arc<HydrateProgress>>,
    ) -> Pin<Box<dyn Future<Output = Result<HydrateSummary>> + Send + 'a>> {
        Box::pin(async move {
            let mut summary = HydrateSummary::default();

            for (path, ptr) in items {
                error::check_cancelled(cancel)?;

                let file_start = Instant::now();

                if is_already_hydrated(path, ptr, cancel)? {
                    debug!(path = %path.display(), "already hydrated, skipping");
                    summary.skipped += 1;
                    summary.bytes_skipped += ptr.size;
                    if let Some(p) = progress {
                        p.files_done.fetch_add(1, Relaxed);
                        p.bytes_done.fetch_add(ptr.size, Relaxed);
                        p.send_file_result(HydrateFileResult {
                            path: path.clone(),
                            outcome: HydrateFileOutcome::Skipped,
                            duration: file_start.elapsed(),
                            bytes: ptr.size,
                        });
                    }
                    continue;
                }

                let content = ptr.serialize();
                match atomic_write(path, &content) {
                    Ok(()) => {
                        let bytes = content.len() as u64;
                        summary.hydrated += 1;
                        summary.bytes_written += bytes;
                        if let Some(p) = progress {
                            p.files_done.fetch_add(1, Relaxed);
                            p.bytes_done.fetch_add(bytes, Relaxed);
                            p.send_file_result(HydrateFileResult {
                                path: path.clone(),
                                outcome: HydrateFileOutcome::Hydrated,
                                duration: file_start.elapsed(),
                                bytes,
                            });
                        }
                    }
                    Err(e) => {
                        debug!(path = %path.display(), err = %e, "failed to hydrate file");
                        summary.failed += 1;
                        if let Some(p) = progress {
                            p.files_done.fetch_add(1, Relaxed);
                            p.send_file_result(HydrateFileResult {
                                path: path.clone(),
                                outcome: HydrateFileOutcome::Failed,
                                duration: file_start.elapsed(),
                                bytes: 0,
                            });
                        }
                    }
                }
            }

            Ok(summary)
        })
    }
}

/// Verify a file that became hydrated after pointer discovery.
fn is_already_hydrated(path: &Path, ptr: &Pointer, cancel: &CancellationToken) -> Result<bool> {
    let Ok(meta) = std::fs::metadata(path) else {
        return Ok(false);
    };

    if meta.len() != ptr.size {
        return Ok(false);
    }

    // If the file parses as a pointer it hasn't been hydrated yet.
    if !matches!(is_working_tree_pointer(path), Ok(false)) {
        return Ok(false);
    }
    Ok(hash_file_blake3_cancellable(path, cancel)? == ptr.file_hash)
}

/// Write `content` to `dest` atomically via a sibling tempfile and rename.
///
/// The tempfile is created in the same directory as `dest` so the rename
/// is guaranteed to be on the same filesystem (no cross-device link error).
///
/// SIGINT safety: if the process is interrupted during `write_all`, the
/// `NamedTempFile` is dropped and its destructor removes the tempfile.
/// The final path is only updated by `persist()` (an atomic rename), so
/// a signal can never leave a half-written file at `dest`.
fn atomic_write(dest: &Path, content: &[u8]) -> Result<()> {
    atomic_write_with_progress(dest, content, None).map(|_| ())
}

fn atomic_write_with_progress(
    dest: &Path,
    content: &[u8],
    progress: Option<&Arc<HydrateProgress>>,
) -> Result<crate::cmd::stream_stage::VerifiedIndexStat> {
    let parent = dest.parent().unwrap_or(Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    for chunk in content.chunks(1024 * 1024) {
        tmp.write_all(chunk)?;
        if let Some(p) = progress {
            p.add_bytes_done(chunk.len() as u64);
        }
    }
    persist_verified_temp(tmp, dest, content.len() as u64).map(|write| write.index_stat)
}

fn persist_verified_temp(
    mut tmp: tempfile::NamedTempFile,
    dest: &Path,
    bytes: u64,
) -> Result<VerifiedWrite> {
    tmp.flush()?;
    preserve_destination_permissions(dest, tmp.as_file())?;
    crate::git::worktree::age_filter_output_mtime(tmp.as_file());
    let file = tmp.persist(dest).map_err(|e| e.error)?;
    let verified_stat = crate::cmd::stream_stage::VerifiedIndexStat::from_file(&file)
        .ok_or_else(|| error::CrabError::Internal("stat published hydration file".to_owned()))?;
    if verified_stat.len != bytes {
        return Err(error::CrabError::Internal(format!(
            "verified hydration size mismatch for {}: expected {bytes}, got {}",
            dest.display(),
            verified_stat.len
        )));
    }
    if crate::cmd::stream_stage::VerifiedIndexStat::from_path_no_follow(dest) != Some(verified_stat)
    {
        return Err(error::CrabError::Internal(format!(
            "hydrated file changed during atomic publication: {}",
            dest.display()
        )));
    }
    Ok(VerifiedWrite {
        bytes,
        index_stat: verified_stat,
    })
}

fn preserve_destination_permissions(dest: &Path, temporary: &std::fs::File) -> Result<()> {
    match std::fs::metadata(dest) {
        Ok(metadata) => temporary
            .set_permissions(metadata.permissions())
            .map_err(Into::into),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

const STAGING_HYDRATE_BATCH_CHUNKS: usize = 512;

/// Restore an unpushed pointer from its exact published staging recipe.
///
/// Failed pushes intentionally preserve staging. This fallback keeps that
/// content recoverable after a pull turns the worktree back into pointers,
/// while the tempfile and whole-file hash gate prevent partial or stale data
/// from replacing the destination.
async fn try_hydrate_from_staging(
    dest: &Path,
    ptr: &Pointer,
    cancel: &CancellationToken,
) -> Result<Option<VerifiedWrite>> {
    let Some(start) = dest.parent() else {
        return Ok(None);
    };
    let Ok(context) = WorktreeContext::resolve_from_path(start) else {
        return Ok(None);
    };
    let staging_root = context.shared_staging_dir();
    let staging = match crab_staging::StagingAreaReadOnly::open_blocking_default(staging_root).await
    {
        Ok(staging) => staging,
        Err(error) => {
            debug!(error = %error, "local staging unavailable for hydrate fallback");
            return Ok(None);
        }
    };
    let file_hash = MerkleHash::from(ptr.file_hash);
    let Some(recipe) = staging.published_recipe_for_file(&file_hash)? else {
        return Ok(None);
    };
    if recipe.file_hash() != file_hash || recipe.file_size() != ptr.size {
        return Err(error::CrabError::StagingCorrupt(format!(
            "published recipe for {} does not match pointer size {}",
            file_hash.hex(),
            ptr.size
        )));
    }

    let parent = dest.parent().unwrap_or(Path::new("."));
    let mut tmp = tempfile::NamedTempFile::new_in(parent)?;
    let mut hasher = blake3::Hasher::new();
    let mut written = 0u64;
    let mut next_occurrence = 0u64;
    while next_occurrence < recipe.chunk_count() {
        error::check_cancelled(cancel)?;
        let page = staging.recipe_page(&recipe, next_occurrence)?;
        for spans in page.chunks.chunks(STAGING_HYDRATE_BATCH_CHUNKS) {
            let hashes = spans.iter().map(|span| span.chunk_hash).collect::<Vec<_>>();
            let chunks = staging.get_chunks_batch(&hashes).await?;
            for (span, (chunk_hash, data)) in spans.iter().zip(chunks) {
                if chunk_hash != span.chunk_hash || data.len() as u64 != span.len {
                    return Err(error::CrabError::StagingCorrupt(format!(
                        "staged chunk {} does not match the published recipe",
                        span.chunk_hash.hex()
                    )));
                }
                tmp.write_all(&data)?;
                hasher.update(&data);
                written = written.checked_add(data.len() as u64).ok_or_else(|| {
                    error::CrabError::StagingCorrupt(
                        "staging hydrate byte count overflow".to_owned(),
                    )
                })?;
            }
        }
        next_occurrence = page.next_occurrence();
    }
    if written != ptr.size || hasher.finalize().as_bytes() != &ptr.file_hash {
        return Err(error::CrabError::StagingCorrupt(format!(
            "staged content for {} does not reconstruct the requested pointer",
            file_hash.hex()
        )));
    }
    let verified = persist_verified_temp(tmp, dest, written)?;
    debug!(path = %dest.display(), bytes = written, "hydrated unpushed pointer from local staging");
    Ok(Some(verified))
}

/// Result of attempting to recover a single pointer from a local source.
#[derive(Debug)]
enum RecoverOutcome {
    /// Candidate found and matched the pointer's blake3 hash. Bytes
    /// have already been written to `dest` (atomic rename completed).
    Recovered { write: VerifiedWrite },
    /// No candidate file at the resolved path — fall through to remote.
    NoCandidate,
    /// Candidate was found but its content does not match the pointer's
    /// `file_hash`. We never overwrite `dest` with mismatched content;
    /// the caller falls through to the remote hydrate path. The actual
    /// hash is logged for diagnostics.
    HashMismatch,
}

/// Resolve the candidate path for a single pointer under `recover_from`.
///
/// When `recover_from` is a regular file, only one pointer can match it
/// (callers enforce this when `to_hydrate.len() > 1`). When
/// `recover_from` is a directory, the candidate is `<dir>/<basename>`.
/// Returns `None` when neither shape applies (e.g. the path doesn't
/// exist, or it's a directory and the basename can't be derived).
fn resolve_recover_candidate(recover_from: &Path, dest: &Path) -> Option<PathBuf> {
    let meta = std::fs::metadata(recover_from).ok()?;
    if meta.is_file() {
        Some(recover_from.to_path_buf())
    } else if meta.is_dir() {
        let name = dest.file_name()?;
        Some(recover_from.join(name))
    } else {
        None
    }
}

/// Try to recover a single pointer from a local source path.
///
/// On match, copies the candidate to `dest` via the same
/// tempfile-then-rename dance [`atomic_write`] uses, so an interrupt
/// mid-copy never leaves a half-written file at the destination.
///
/// On mismatch (or no candidate present), returns the appropriate
/// [`RecoverOutcome`] without touching `dest`.
fn try_recover_one(recover_from: &Path, dest: &Path, ptr: &Pointer) -> Result<RecoverOutcome> {
    let Some(candidate) = resolve_recover_candidate(recover_from, dest) else {
        return Ok(RecoverOutcome::NoCandidate);
    };
    if !candidate.exists() {
        return Ok(RecoverOutcome::NoCandidate);
    }

    let candidate_size = std::fs::metadata(&candidate)
        .map(|m| m.len())
        .unwrap_or_default();
    if candidate_size != ptr.size {
        debug!(
            candidate = %candidate.display(),
            candidate_size,
            expected_size = ptr.size,
            "recover-from: candidate size mismatch, skipping"
        );
        return Ok(RecoverOutcome::HashMismatch);
    }

    // Copy and hash the exact published bytes in one bounded pass. Hashing the
    // source first and reopening it for copy leaves a TOCTOU window and doubles
    // source I/O for large recovery files.
    use std::io::Read;
    let parent = dest.parent().unwrap_or(Path::new("."));
    let mut src = std::fs::File::open(&candidate).map_err(error::CrabError::Io)?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent).map_err(error::CrabError::Io)?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    let mut bytes = 0u64;
    loop {
        let read = src.read(&mut buffer).map_err(error::CrabError::Io)?;
        if read == 0 {
            break;
        }
        tmp.write_all(&buffer[..read])?;
        hasher.update(&buffer[..read]);
        bytes = bytes
            .checked_add(read as u64)
            .ok_or_else(|| error::CrabError::Internal("recovery size overflow".to_owned()))?;
    }
    let actual_hash = *hasher.finalize().as_bytes();
    if bytes != ptr.size || actual_hash != ptr.file_hash {
        debug!(
            candidate = %candidate.display(),
            expected_hash = %crab_types::pointer::hex_encode(&ptr.file_hash),
            actual_hash = %crab_types::pointer::hex_encode(&actual_hash),
            "recover-from: candidate changed or hash mismatched during copy"
        );
        return Ok(RecoverOutcome::HashMismatch);
    }
    let write = persist_verified_temp(tmp, dest, bytes)?;

    debug!(
        candidate = %candidate.display(),
        dest = %dest.display(),
        bytes,
        "recover-from: hash verified and copied to working tree"
    );
    Ok(RecoverOutcome::Recovered { write })
}

/// Walk the to-hydrate list, attempting `--recover-from` recovery for
/// each pointer, and return the residual list (those that could not be
/// recovered locally) along with summary counters for what was.
///
/// Pointers that recover successfully are removed from the residual
/// list — the normal hydrator never sees them and the remote is never
/// consulted. Pointers with no candidate fall through unchanged. A
/// hash-mismatch is treated as "no candidate" (we don't overwrite a
/// hydrated file with content that doesn't match its pointer); a
/// debug-level log records the mismatch for diagnosis.
///
/// Cancellation is honored at the per-pointer boundary so a `Ctrl-C`
/// during a large recovery run cuts in cleanly.
fn run_recover_phase(
    recover_from: &Path,
    to_hydrate: Vec<(PathBuf, Pointer)>,
    cancel: &CancellationToken,
    progress: Option<&Arc<HydrateProgress>>,
) -> Result<(Vec<(PathBuf, Pointer)>, RecoverPhaseStats)> {
    let mut residual = Vec::with_capacity(to_hydrate.len());
    let mut stats = RecoverPhaseStats::default();

    for (path, ptr) in to_hydrate {
        error::check_cancelled(cancel)?;

        if is_already_hydrated(&path, &ptr, cancel)? {
            // Defer to the normal hydrator's skip path so the summary
            // accounting stays in one place.
            residual.push((path, ptr));
            continue;
        }

        if let Some(p) = progress {
            p.set_current_file(&path.file_name().map_or_else(
                || path.to_string_lossy().into_owned(),
                |n| n.to_string_lossy().into_owned(),
            ));
        }

        let file_start = Instant::now();
        match try_recover_one(recover_from, &path, &ptr)? {
            RecoverOutcome::Recovered { write } => {
                stats.recovered += 1;
                stats.bytes_recovered += write.bytes;
                stats.verified_paths.push(verified_path(&path, &ptr, write));
                if let Some(p) = progress {
                    p.files_done.fetch_add(1, Relaxed);
                    p.bytes_done.fetch_add(write.bytes, Relaxed);
                    p.send_file_result(HydrateFileResult {
                        path: path.clone(),
                        outcome: HydrateFileOutcome::Hydrated,
                        duration: file_start.elapsed(),
                        bytes: write.bytes,
                    });
                }
                info!(
                    path = %path.display(),
                    bytes = write.bytes,
                    "recovered from local source"
                );
            }
            RecoverOutcome::NoCandidate | RecoverOutcome::HashMismatch => {
                residual.push((path, ptr));
            }
        }
    }

    Ok((residual, stats))
}

#[derive(Debug, Default)]
struct RecoverPhaseStats {
    recovered: u64,
    bytes_recovered: u64,
    verified_paths: Vec<crate::cache::add_validation::VerifiedPath>,
}

const MAX_COW_CANDIDATES_PER_POINTER: usize = 8;
type CowPointerKey = ([u8; 32], u64);

#[derive(Debug, Default)]
struct CowCandidateIndex {
    by_pointer: HashMap<CowPointerKey, Vec<PathBuf>>,
}

impl CowCandidateIndex {
    fn candidates(&self, pointer: &Pointer) -> &[PathBuf] {
        self.by_pointer
            .get(&(pointer.file_hash, pointer.size))
            .map_or(&[], Vec::as_slice)
    }

    fn insert(&mut self, pointer: &Pointer, path: PathBuf) {
        let candidates = self
            .by_pointer
            .entry((pointer.file_hash, pointer.size))
            .or_default();
        if candidates.len() < MAX_COW_CANDIDATES_PER_POINTER {
            candidates.push(path);
        }
    }
}

fn safe_cached_candidate(root: &Path, canonical_root: &Path, relative: &str) -> Option<PathBuf> {
    let relative = Path::new(relative);
    if relative.as_os_str().is_empty()
        || relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return None;
    }

    let mut candidate = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return None;
        };
        candidate.push(name);
        let metadata = std::fs::symlink_metadata(&candidate).ok()?;
        if metadata.file_type().is_symlink() {
            return None;
        }
    }

    let metadata = std::fs::symlink_metadata(&candidate).ok()?;
    if !metadata.file_type().is_file() {
        return None;
    }
    let canonical_candidate = std::fs::canonicalize(&candidate).ok()?;
    canonical_candidate
        .starts_with(canonical_root)
        .then_some(candidate)
}

/// Build one advisory index from sibling worktree caches. Cache entries only
/// locate candidates; a cloned destination is always hashed before publication.
fn sibling_cow_candidates(root: &Path) -> CowCandidateIndex {
    let current_context = match WorktreeContext::resolve_from_path(root) {
        Ok(context) => context,
        Err(error) => {
            debug!(error = %error, "CoW candidate discovery unavailable outside a worktree");
            return CowCandidateIndex::default();
        }
    };
    let current_identity = normalize_identity_path(&current_context.current_worktree_root);
    let output = match Command::new("git")
        .args(["-C"])
        .arg(&current_context.current_worktree_root)
        .args(["worktree", "list", "--porcelain", "-z"])
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .output()
    {
        Ok(output) if output.status.success() => output,
        Ok(output) => {
            debug!(status = %output.status, "git worktree list failed during CoW discovery");
            return CowCandidateIndex::default();
        }
        Err(error) => {
            debug!(error = %error, "could not list sibling worktrees for CoW discovery");
            return CowCandidateIndex::default();
        }
    };
    let records = match parse_worktree_list_porcelain(&output.stdout, true) {
        Ok(records) => records,
        Err(error) => {
            debug!(error = %error, "could not parse sibling worktrees for CoW discovery");
            return CowCandidateIndex::default();
        }
    };

    let mut index = CowCandidateIndex::default();
    for record in records {
        if record.bare || record.prunable {
            continue;
        }
        let worktree_root = PathBuf::from(record.path);
        if !worktree_root.is_dir() || normalize_identity_path(&worktree_root) == current_identity {
            continue;
        }
        let context = match WorktreeContext::resolve_from_path(&worktree_root) {
            Ok(context) => context,
            Err(error) => {
                debug!(path = %worktree_root.display(), error = %error, "ignoring unresolvable CoW sibling");
                continue;
            }
        };
        let cache_path = crate::cache::hydrated_pointer::cache_path_for_context(&context);
        let cache = crate::cache::HydratedPointerCache::load_sync(&cache_path);
        let Ok(canonical_root) = std::fs::canonicalize(&worktree_root) else {
            continue;
        };
        for (relative, entry) in cache.entries() {
            let Some(candidate) = safe_cached_candidate(&worktree_root, &canonical_root, relative)
            else {
                continue;
            };
            if !crate::cache::hydrated_pointer::matches_stat(&candidate, entry) {
                continue;
            }
            let Some(pointer_bytes) = crate::cache::hydrated_pointer::decode_pointer(entry) else {
                continue;
            };
            let Ok(pointer) = Pointer::parse(&pointer_bytes) else {
                continue;
            };
            if pointer.size != entry.size {
                continue;
            }
            index.insert(&pointer, candidate);
        }
    }
    index
}

fn hash_file_blake3_cancellable(path: &Path, cancel: &CancellationToken) -> Result<[u8; 32]> {
    use std::io::Read;

    let mut file = std::fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0u8; 1024 * 1024];
    loop {
        error::check_cancelled(cancel)?;
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    error::check_cancelled(cancel)?;
    Ok(*hasher.finalize().as_bytes())
}

fn remove_file_if_present(path: &Path) {
    if let Err(error) = std::fs::remove_file(path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        debug!(path = %path.display(), error = %error, "failed to remove CoW temporary file");
    }
}

/// Clone and verify one candidate without exposing unverified bytes at `dest`.
/// Any filesystem clone failure is an advisory miss so normal hydration remains
/// the canonical fallback. Cancellation is the only error propagated directly.
fn try_cow_clone_candidate(
    source: &Path,
    dest: &Path,
    pointer: &Pointer,
    cancel: &CancellationToken,
) -> Result<Option<VerifiedWrite>> {
    error::check_cancelled(cancel)?;
    let parent = dest.parent().unwrap_or(Path::new("."));
    let reservation = match tempfile::Builder::new()
        .prefix(".crab-cow-")
        .tempfile_in(parent)
    {
        Ok(reservation) => reservation,
        Err(error) => {
            debug!(path = %dest.display(), error = %error, "could not reserve CoW temporary path");
            return Ok(None);
        }
    };
    let temporary = reservation.path().to_path_buf();
    if let Err(error) = reservation.close() {
        debug!(path = %temporary.display(), error = %error, "could not release CoW temporary reservation");
        return Ok(None);
    }

    if let Err(error) = cow_clone::clone_file(source, &temporary) {
        remove_file_if_present(&temporary);
        debug!(source = %source.display(), path = %dest.display(), error = %error, "CoW clone unavailable; falling back to hydration");
        return Ok(None);
    }

    let verified = (|| -> Result<bool> {
        error::check_cancelled(cancel)?;
        let metadata = std::fs::symlink_metadata(&temporary)?;
        if !metadata.file_type().is_file() || metadata.len() != pointer.size {
            return Ok(false);
        }
        Ok(hash_file_blake3_cancellable(&temporary, cancel)? == pointer.file_hash)
    })();
    match verified {
        Ok(true) => {}
        Ok(false) => {
            remove_file_if_present(&temporary);
            debug!(source = %source.display(), path = %dest.display(), "CoW clone verification mismatch; falling back to hydration");
            return Ok(None);
        }
        Err(error::CrabError::Cancelled) => {
            remove_file_if_present(&temporary);
            return Err(error::CrabError::Cancelled);
        }
        Err(error) => {
            remove_file_if_present(&temporary);
            debug!(source = %source.display(), path = %dest.display(), error = %error, "CoW clone verification failed; falling back to hydration");
            return Ok(None);
        }
    }

    let prepare = (|| -> Result<std::fs::File> {
        let temporary_file = std::fs::OpenOptions::new().write(true).open(&temporary)?;
        preserve_destination_permissions(dest, &temporary_file)?;
        crate::git::worktree::age_filter_output_mtime(&temporary_file);
        let pre_publish_stat =
            crate::cmd::stream_stage::VerifiedIndexStat::from_file(&temporary_file).ok_or_else(
                || error::CrabError::Internal("stat verified CoW hydration tempfile".to_owned()),
            )?;
        if pre_publish_stat.len != pointer.size {
            return Err(error::CrabError::Internal(format!(
                "verified CoW hydration size mismatch for {}",
                dest.display()
            )));
        }
        Ok(temporary_file)
    })();
    let temporary_file = match prepare {
        Ok(temporary_file) => temporary_file,
        Err(error::CrabError::Cancelled) => {
            remove_file_if_present(&temporary);
            return Err(error::CrabError::Cancelled);
        }
        Err(error) => {
            remove_file_if_present(&temporary);
            debug!(source = %source.display(), path = %dest.display(), error = %error, "CoW clone publication failed; falling back to hydration");
            return Ok(None);
        }
    };
    if let Err(error) = error::check_cancelled(cancel) {
        remove_file_if_present(&temporary);
        return Err(error);
    }
    if let Err(error) = std::fs::rename(&temporary, dest) {
        remove_file_if_present(&temporary);
        debug!(source = %source.display(), path = %dest.display(), error = %error, "CoW clone publication failed; falling back to hydration");
        return Ok(None);
    }
    let index_stat = crate::cmd::stream_stage::VerifiedIndexStat::from_file(&temporary_file)
        .ok_or_else(|| {
            error::CrabError::Internal("stat published CoW hydration file".to_owned())
        })?;
    if crate::cmd::stream_stage::VerifiedIndexStat::from_path_no_follow(dest) != Some(index_stat) {
        return Err(error::CrabError::Internal(format!(
            "CoW hydrated file changed during publication: {}",
            dest.display()
        )));
    }
    Ok(Some(VerifiedWrite {
        bytes: pointer.size,
        index_stat,
    }))
}

#[derive(Debug, Default)]
struct CowPhaseStats {
    cloned: u64,
    bytes_cloned: u64,
    verified_paths: Vec<crate::cache::add_validation::VerifiedPath>,
}

async fn run_cow_phase(
    to_hydrate: Vec<(PathBuf, Pointer)>,
    candidates: &CowCandidateIndex,
    cancel: &CancellationToken,
    progress: Option<&Arc<HydrateProgress>>,
) -> Result<(Vec<(PathBuf, Pointer)>, CowPhaseStats)> {
    let mut residual = Vec::with_capacity(to_hydrate.len());
    let mut stats = CowPhaseStats::default();

    for (path, pointer) in to_hydrate {
        error::check_cancelled(cancel)?;
        if is_already_hydrated(&path, &pointer, cancel)? {
            residual.push((path, pointer));
            continue;
        }

        let mut cloned = false;
        for source in candidates.candidates(&pointer) {
            let file_start = Instant::now();
            let source = source.clone();
            let destination = path.clone();
            let pointer_for_clone = pointer.clone();
            let cancel_for_clone = cancel.clone();
            let outcome = tokio::task::spawn_blocking(move || {
                try_cow_clone_candidate(
                    &source,
                    &destination,
                    &pointer_for_clone,
                    &cancel_for_clone,
                )
            })
            .await
            .map_err(|error| {
                error::CrabError::Internal(format!("CoW hydration worker failed: {error}"))
            })??;
            let Some(write) = outcome else {
                continue;
            };

            stats.cloned += 1;
            stats.bytes_cloned += write.bytes;
            stats
                .verified_paths
                .push(verified_path(&path, &pointer, write));
            if let Some(progress) = progress {
                progress.files_done.fetch_add(1, Relaxed);
                progress.bytes_done.fetch_add(write.bytes, Relaxed);
                progress.send_file_result(HydrateFileResult {
                    path: path.clone(),
                    outcome: HydrateFileOutcome::Hydrated,
                    duration: file_start.elapsed(),
                    bytes: write.bytes,
                });
            }
            info!(path = %path.display(), bytes = write.bytes, "hydrated through sibling-worktree CoW clone");
            cloned = true;
            break;
        }
        if !cloned {
            residual.push((path, pointer));
        }
    }

    Ok((residual, stats))
}

/// Format a byte count as a human-readable string with binary units.
fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    const TIB: u64 = 1024 * GIB;

    if bytes >= TIB {
        format!("{:.2} TiB", bytes as f64 / TIB as f64)
    } else if bytes >= GIB {
        format!("{:.2} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.2} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.2} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

/// Format a duration as `Xm Ys` or `Xs` for short durations.
fn format_elapsed(elapsed: std::time::Duration) -> String {
    let secs = elapsed.as_secs();
    if secs >= 60 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

/// Wipe the speculation database (`access.db`).
///
/// Opens the current worktree's access DB, calls `clear()` to delete all rows
/// from both `access_events` and `co_access`, and prints a confirmation
/// message.
pub fn run_clear_speculation() -> Result<()> {
    let ctx = crate::git::worktree::WorktreeContext::resolve()?;
    let db_path = crate::speculation::access_db::path_for_context(&ctx);

    if !db_path.exists() {
        info!("no speculation database found, nothing to clear");
        println!("No speculation database found.");
        return Ok(());
    }

    let db = crate::speculation::access_db::AccessDb::open(&db_path)?;
    db.clear()?;
    info!(path = %db_path.display(), "speculation database cleared");
    println!("Speculation database cleared.");
    Ok(())
}

/// Run hydration at an explicit root using its resolved storage and restore policy.
///
/// Configured remotes fail closed; only an absent remote selects local staging.
/// Returns selection, reconstruction, filesystem and cancellation errors.
pub async fn run_hydrate(
    root: &Path,
    args: &HydrateArgs,
    config: &Config,
    restore_flags: &crate::cmd::hydrate_restore::RestoreFlags,
    cancel: &CancellationToken,
) -> Result<()> {
    error::check_cancelled(cancel)?;
    if let Some(parsed) = resolve_hydrate_remote_url(config)? {
        let selection =
            crate::replication::select_read_store(config, &parsed, "hydrate", cancel).await?;
        if let crate::replication::ReadSource::Replica { name } = &selection.source {
            debug!(replica = %name, "selected read replica for hydrate");
        }
        let caching_store = crab_cache_store::CachingStore::new(selection.store, &config.cache)?;
        // Bulk hydrate already streams through the full-xorb cache. A decoded
        // range cache here would add writes and eviction churn to one-pass reads.
        let mut hydrator =
            ShardHydrator::with_config_from_cli_layout(caching_store, selection.router, config)?;
        let requested_restore = restore_flags.resolve_auto_restore(config.hydrate.auto_restore);
        if restore_flags.restore && !config.tier.enabled {
            return Err(error::CrabError::Configuration {
                key: "tier.enabled is false; cannot restore archived xorbs".into(),
                origin: "hydrate --restore".into(),
            });
        }
        if requested_restore && config.tier.enabled {
            let mut options = crate::tier::runtime::restore_options_from_config(config)?;
            if let Some(tier) = &restore_flags.restore_tier {
                options.tier = crate::tier::runtime::parse_restore_tier(tier)?;
            }
            if let Some(days) = restore_flags.restore_duration_days {
                options.duration = Duration::from_secs(u64::from(days) * 86_400);
            }
            let backend = crate::tier::runtime::build_restore_backend(config, &parsed).await?;
            let orchestrator = Arc::new(crate::tier::restore::RestoreOrchestrator::with_options(
                backend,
                config.tier.restore_max_concurrency,
                Duration::from_secs(config.tier.restore_timeout_secs),
                options,
            ));
            hydrator = hydrator.with_restore(Some(orchestrator), true);
        } else {
            hydrator = hydrator.with_restore(None, false);
        }
        error::check_cancelled(cancel)?;
        return run_hydrate_in(root, args, config, &hydrator, cancel).await;
    }

    // Local unpublished staging remains readable without a configured remote.
    // Never enter this path after a configured provider or replica fails.
    let ctx = AppContext::new(config.clone(), cancel.clone());
    let hydrator = SmudgeSessionHydrator::new(ctx);
    run_hydrate_in(root, args, config, &hydrator, cancel).await
}

/// Parse the configured hydration remote, distinguishing absence from invalid policy.
pub fn resolve_hydrate_remote_url(config: &Config) -> Result<Option<crate::git::url::CrabUrl>> {
    let Some(url) = config.remote_url.as_deref() else {
        return Ok(None);
    };
    if url.trim().is_empty() {
        return Err(error::CrabError::Configuration {
            key: "remote.url".into(),
            origin: "crab.toml contains an empty [remote].url".into(),
        });
    }
    crate::git::url::CrabUrl::parse(url).map(Some)
}

/// Run local smudge-session hydration with a shared chunk cache.
///
/// This low-level entry point does not compose remote readers; CLI callers use
/// [`run_hydrate`] so configured storage policy is honored.
pub async fn run_hydrate_with_cache(
    args: &HydrateArgs,
    config: &Config,
    cache: Arc<ChunkCache>,
    cancel: &CancellationToken,
) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let ctx = AppContext::new(config.clone(), cancel.clone());
    let hydrator = SmudgeSessionHydrator::with_chunk_cache(ctx, cache);
    run_hydrate_in(&cwd, args, config, &hydrator, cancel).await
}

/// Hydrate implementation that accepts an explicit root directory and hydrator.
pub async fn run_hydrate_in(
    root: &Path,
    args: &HydrateArgs,
    config: &Config,
    hydrator: &dyn Hydrator,
    cancel: &CancellationToken,
) -> Result<()> {
    let pending_hydration = pending_worktree_hydration(root, args)?;
    let args = pending_hydration
        .as_ref()
        .map_or(args, |pending| &pending.args);

    // When a manifest or profile is provided, resolve it into a set of
    // paths/globs and use those as the hydrate filter instead of the
    // normal pattern resolution. --manifest, --manifest-ref, and
    // --profile are mutually exclusive (enforced by clap).
    let manifest_entries = resolve_manifest(args, root)?;

    let filter = if manifest_entries.is_some() {
        // Manifest mode: match everything — filtering is done by the
        // manifest entries themselves during the walk.
        Some(build_all_filter()?)
    } else {
        resolve_patterns(args, config)?
    };

    let Some(filter) = filter else {
        if !args.mode.is_machine() {
            print_help();
        }
        return Ok(());
    };

    // A `git pull` after the initial `crab clone` can land new
    // pointer blobs for extensions that are not yet in
    // `.gitattributes`. Rescan the working tree so extensions
    // discovered since the last clone are picked up automatically.
    // Best-effort: failures here must not block hydrate. Idempotent —
    // only appends missing rules, existing ones are left alone.
    if let Err(e) = crate::cmd::clone::autotrack_pointer_extensions(root, args.mode) {
        debug!(error = %e, "autotrack rescan failed; continuing with existing patterns");
    }

    let mut to_hydrate: Vec<(PathBuf, Pointer)> = Vec::new();
    let manifest_filter;
    let selection_filter = if let Some(ref entries) = manifest_entries {
        manifest_filter = build_manifest_filter(entries, &args.exclude)?;
        &manifest_filter
    } else {
        &filter
    };

    let tracked = TrackedClassifier::open(root)?;
    if tracked.is_empty() {
        debug!("no crab-tracked attributes found in working tree");
    } else {
        debug!("loaded crab-tracked attributes");
        walk_and_parse_pointers(
            root,
            root,
            &tracked,
            selection_filter,
            cancel,
            &mut to_hydrate,
        )?;
        if let Some(ref entries) = manifest_entries {
            info!(
                manifest_entries = entries.len(),
                matched_files = to_hydrate.len(),
                "manifest: resolved entries to pointer files"
            );
        }
    }

    collect_missing_index_pointers(root, selection_filter, cancel, &mut to_hydrate)?;

    if to_hydrate.is_empty() {
        if let Some(pending) = pending_hydration.as_ref() {
            mark_pending_worktree_hydration_applied(root, &pending.policy)?;
        }
        emit_empty_hydrate_summary(args.mode);
        return Ok(());
    }

    info!(count = to_hydrate.len(), "hydrating files");

    // Compute totals for progress reporting.
    let total_bytes: u64 = to_hydrate.iter().map(|(_, ptr)| ptr.size).sum();
    let total_files = to_hydrate.len() as u64;

    // In JSONL manifest mode, set up a per-file result channel so the
    // hydrator streams one JSONL row per file as it completes.
    let is_manifest_jsonl = manifest_entries.is_some() && args.mode == OutputMode::Jsonl;
    let (file_result_tx, file_result_rx) = if is_manifest_jsonl {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<HydrateFileResult>();
        (Some(tx), Some(rx))
    } else {
        (None, None)
    };

    let progress = Arc::new(if let Some(tx) = file_result_tx {
        HydrateProgress::with_file_result_tx(total_files, total_bytes, tx)
    } else {
        HydrateProgress::new(total_files, total_bytes)
    });

    // Build the optional JSONL stream for streaming mode.
    let jsonl_stream: Option<Arc<std::sync::Mutex<JsonlStream<Stdout>>>> = match args.mode {
        OutputMode::Jsonl => Some(Arc::new(std::sync::Mutex::new(JsonlStream::new(
            "hydrate.event",
            "1.0",
            std::io::stdout(),
        )))),
        _ => None,
    };

    // Spawn a background task that reads per-file results from the
    // channel and emits JSONL progress rows as each file completes.
    // The strategy is "shard_batch" for manifest-based hydration.
    let jsonl_emitter_handle =
        if let (Some(mut rx), Some(stream)) = (file_result_rx, jsonl_stream.clone()) {
            let root_buf = root.to_path_buf();
            Some(tokio::spawn(async move {
                while let Some(file_result) = rx.recv().await {
                    let rel = file_result
                        .path
                        .strip_prefix(&root_buf)
                        .unwrap_or(&file_result.path);
                    let row = ManifestHydrateFileRow {
                        path: rel.to_string_lossy().into_owned(),
                        strategy: "shard_batch".to_owned(),
                        duration_ms: file_result.duration.as_millis() as u64,
                        bytes: file_result.bytes,
                    };
                    if let Ok(mut s) = stream.lock() {
                        s.emit_file_done(&row);
                    }
                }
            }))
        } else {
            None
        };

    // Spawn a live progress ticker when stderr is a TTY (text mode only).
    let ticker = if args.mode == OutputMode::Text {
        progress.start_ticker(cancel)
    } else {
        None
    };

    let start = Instant::now();
    let selected_to_hydrate = to_hydrate.clone();

    // --- `--recover-from` phase --------------------------------------
    //
    // When the user supplies a local source, walk the to-hydrate list
    // first and short-circuit any pointer whose blake3 hash matches a
    // candidate file under the supplied path. Recovered pointers are
    // dropped from `to_hydrate` so the remote hydrator never sees them.
    //
    // Recovery is best-effort and side-effect-bounded: a hash mismatch
    // or missing candidate falls through to the normal hydrator, never
    // to a write that doesn't match the pointer.
    let (to_hydrate, recover_stats) = if let Some(recover_path) = args.recover_from.as_deref() {
        if !args.mode.is_machine() {
            eprintln!("Recovering from local source: {}", recover_path.display(),);
        }
        run_recover_phase(recover_path, to_hydrate, cancel, Some(&progress))?
    } else {
        (to_hydrate, RecoverPhaseStats::default())
    };

    // A sibling cache is only a candidate locator. Build it once off the async
    // runtime, then clone and hash each candidate before the normal hydrator.
    let candidate_root = root.to_path_buf();
    let cow_candidates =
        tokio::task::spawn_blocking(move || sibling_cow_candidates(&candidate_root))
            .await
            .map_err(|error| {
                error::CrabError::Internal(format!(
                    "CoW candidate discovery worker failed: {error}"
                ))
            })?;
    let (to_hydrate, cow_stats) =
        run_cow_phase(to_hydrate, &cow_candidates, cancel, Some(&progress)).await?;

    let result = hydrator
        .hydrate_batch(&to_hydrate, cancel, Some(&progress))
        .await;

    // Drop the progress Arc so the channel sender is closed and the
    // emitter task can drain remaining items and terminate. After
    // hydrate_batch returns, no more sends will happen.
    drop(progress);

    // Wait for the JSONL emitter to finish draining.
    if let Some(handle) = jsonl_emitter_handle {
        let _ = handle.await;
    }

    // Stop the ticker before printing the final summary.
    if let Some(handle) = ticker {
        handle.abort();
        let _ = handle.await;
        // Clear the progress line.
        eprint!("\r\x1b[2K");
    }

    let mut summary = result?;
    // Fold the `--recover-from` phase results into the user-facing
    // summary. Each recovered file counts toward `hydrated` /
    // `bytes_written` (the working tree was materialized) and is also
    // tallied separately so the summary line can call out the
    // recovery path.
    summary.hydrated += recover_stats.recovered;
    summary.bytes_written += recover_stats.bytes_recovered;
    summary.recovered = recover_stats.recovered;
    summary.bytes_recovered = recover_stats.bytes_recovered;
    summary.verified_paths.extend(recover_stats.verified_paths);
    summary.hydrated += cow_stats.cloned;
    summary.bytes_written += cow_stats.bytes_cloned;
    summary.cow_cloned = cow_stats.cloned;
    summary.bytes_cow_cloned = cow_stats.bytes_cloned;
    summary.verified_paths.extend(cow_stats.verified_paths);
    let elapsed = start.elapsed();

    if let Some(pending) = pending_hydration.as_ref()
        && summary.failed == 0
    {
        mark_pending_worktree_hydration_applied(root, &pending.policy)?;
    }

    // Publish only descriptor-safe proofs captured by successful atomic
    // writes. Sibling worktrees use this cache to locate CoW candidates;
    // they still hash each candidate before publication. Best-effort: an
    // unavailable cache only disables that local optimization.
    if summary.hydrated > 0 {
        let pointers = selected_to_hydrate
            .iter()
            .map(|(path, pointer)| (path.as_path(), pointer))
            .collect::<HashMap<_, _>>();
        let updates = summary
            .verified_paths
            .iter()
            .filter_map(|verified| {
                let pointer = pointers.get(verified.path.as_path())?;
                if pointer.file_hash != verified.file_hash || pointer.size != verified.size {
                    return None;
                }
                let rel = verified.path.strip_prefix(root).unwrap_or(&verified.path);
                let rel_str = rel.to_string_lossy().replace('\\', "/");
                crate::cache::hydrated_pointer::entry_for_verified_stat(
                    verified.index_stat,
                    &pointer.serialize(),
                )
                .map(|entry| (rel_str, entry))
            })
            .collect::<Vec<_>>();
        if !updates.is_empty() {
            match crate::cache::hydrated_pointer::cache_path_for_worktree_root(root) {
                Ok(cache_path) => {
                    if let Err(e) =
                        crate::cache::HydratedPointerCache::update_on_disk(&cache_path, updates)
                    {
                        debug!(
                            path = %cache_path.display(),
                            error = %e,
                            "failed to persist hydrated-pointer cache (non-fatal)"
                        );
                    }
                }
                Err(e) => {
                    debug!(
                        root = %root.display(),
                        error = %e,
                        "hydrated-pointer cache unavailable for hydrate"
                    );
                }
            }
        }
        refresh_hydrated_index_entries(root, &summary.verified_paths);
    }

    // In non-manifest JSONL mode, emit retroactive file_done events.
    // Manifest JSONL mode already streamed per-file rows above.
    if !is_manifest_jsonl && let Some(stream) = &jsonl_stream {
        for (path, ptr) in &selected_to_hydrate {
            let rel = path.strip_prefix(root).unwrap_or(path);
            let is_pointer = is_working_tree_pointer(path).unwrap_or(false);
            let (status, bytes) = if is_pointer {
                ("skipped", ptr.size)
            } else {
                ("ok", ptr.size)
            };
            if let Ok(mut s) = stream.lock() {
                s.emit_file_done(FileDonePayload {
                    path: rel.to_string_lossy().into_owned(),
                    bytes,
                    duration_ms: elapsed.as_millis() as u64,
                    status: status.to_owned(),
                });
            }
        }

        // Emit a final progress event.
        if let Ok(mut s) = stream.lock() {
            let rate = if elapsed.as_secs_f64() > 0.0 {
                summary.bytes_written as f64 / elapsed.as_secs_f64()
            } else {
                0.0
            };
            s.emit_progress(ProgressPayload {
                operation: "hydrating".to_owned(),
                current: summary.hydrated + summary.skipped + summary.failed,
                total: total_files,
                bytes: summary.bytes_written + summary.bytes_skipped,
                total_bytes,
                rate_bytes_per_sec: rate,
                xorbs_produced: None,
            });
        }
    }

    let payload = HydrateSummaryPayload::from_summary(&summary, elapsed);

    match args.mode {
        OutputMode::Text => {
            let mut parts = Vec::new();
            parts.push(format!(
                "Hydrated {} file(s) ({})",
                summary.hydrated,
                format_bytes(summary.bytes_written),
            ));
            parts.push(format!("in {}", format_elapsed(elapsed)));
            if summary.recovered > 0 {
                parts.push(format!(
                    "{} recovered from local source ({})",
                    summary.recovered,
                    format_bytes(summary.bytes_recovered),
                ));
            }
            if summary.cow_cloned > 0 {
                parts.push(format!(
                    "{} CoW cloned from sibling worktrees ({})",
                    summary.cow_cloned,
                    format_bytes(summary.bytes_cow_cloned),
                ));
            }
            if summary.skipped > 0 {
                parts.push(format!(
                    "{} skipped ({}, already hydrated)",
                    summary.skipped,
                    format_bytes(summary.bytes_skipped),
                ));
            }
            if summary.failed > 0 {
                parts.push(format!("{} failed", summary.failed));
            }
            println!("{}", parts.join(", "));
            // When some files were recovered locally, surface the
            // remote-repair hint exactly once. The remote shard for
            // those files is still incomplete; the hydrate command
            // didn't push anything itself.
            if summary.recovered > 0 {
                println!(
                    "\nNote: recovered files are present in the working tree but the \
                     remote shard for them is still incomplete. To repair the remote, \
                     re-push:\n  \
                     crab add <files> && git add <files> && \
                     git commit --amend --no-edit && git push --force"
                );
            }
        }
        OutputMode::Json => {
            emit_json("hydrate", "1.0", &payload);
        }
        OutputMode::Jsonl => {
            if let Some(stream) = &jsonl_stream {
                if is_manifest_jsonl {
                    // Manifest mode: emit the typed summary record.
                    let summary_row = ManifestHydrateSummaryRow {
                        row_type: "summary".to_owned(),
                        total: total_files,
                        hydrated: summary.hydrated,
                        skipped: summary.skipped,
                        failed: summary.failed,
                        cow_cloned: summary.cow_cloned,
                        bytes_cow_cloned: summary.bytes_cow_cloned,
                        duration_ms: elapsed.as_millis() as u64,
                    };
                    if let Ok(mut s) = stream.lock() {
                        s.emit_result(&summary_row);
                    }
                } else if let Ok(mut s) = stream.lock() {
                    s.emit_result(&payload);
                }
            }
        }
    }

    if summary.failed > 0 {
        return Err(error::CrabError::Internal(format!(
            "hydrate failed for {} file(s)",
            summary.failed
        )));
    }

    Ok(())
}

fn refresh_hydrated_index_entries(
    root: &Path,
    verified_paths: &[crate::cache::add_validation::VerifiedPath],
) {
    let paths = verified_paths
        .iter()
        .map(|verified| {
            (
                verified.path.clone(),
                verified.index_stat.stat,
                verified.index_stat.len,
                verified.file_hash,
                verified.size,
            )
        })
        .collect::<Vec<_>>();
    if let Err(e) = refresh_verified_index_stats(root, &paths) {
        debug!(
            files = paths.len(),
            error = %e,
            "failed to refresh hydrated file index stat metadata"
        );
        return;
    }
    match crate::cache::add_validation::record_verified_paths(root, verified_paths) {
        Ok(validations) => {
            debug!(validations, "recorded verified hydrate validations");
        }
        Err(error) => {
            debug!(
                error = %error,
                "failed to persist verified hydrate validations; later add will rehash"
            );
        }
    }
}

pub fn resolve_git_pointer_prefetch_candidates(
    root: &Path,
    args: &HydrateArgs,
    config: &Config,
    cancel: &CancellationToken,
) -> Result<Vec<(PathBuf, Pointer)>> {
    let manifest_entries = resolve_manifest(args, root)?;
    let filter = if manifest_entries.is_some() {
        build_all_filter()?
    } else {
        let Some(filter) = resolve_patterns(args, config)? else {
            return Ok(Vec::new());
        };
        filter
    };
    let manifest_filter;
    let selection_filter = if let Some(ref entries) = manifest_entries {
        manifest_filter = build_manifest_filter(entries, &args.exclude)?;
        &manifest_filter
    } else {
        &filter
    };
    let mut candidates = Vec::new();
    collect_git_pointer_blobs(
        root,
        selection_filter,
        cancel,
        &mut candidates,
        GitPointerCollectionMode::All,
    )?;
    Ok(candidates)
}

/// Resolve unique Crab pointer files reachable from every local Git ref.
///
/// This is the `crab fetch --all` inventory path. It reads Git objects in two
/// batch processes, filtering large blobs before materializing pointer bodies.
pub fn resolve_all_ref_pointer_prefetch_candidates(
    root: &Path,
    include: &[String],
    exclude: &[String],
    cancel: &CancellationToken,
) -> Result<Vec<(PathBuf, Pointer)>> {
    let include = if include.is_empty() {
        vec!["*".to_owned()]
    } else {
        include.to_vec()
    };
    let filter = build_filter(&include, exclude)?;
    let refs = git_refs(root)?;
    let mut entries = Vec::new();
    let mut seen_entries = HashSet::new();
    for git_ref in refs {
        error::check_cancelled(cancel)?;
        for entry in git_tree_entries(root, &git_ref)? {
            if filter.matches(&entry.path)
                && seen_entries.insert((entry.oid.clone(), entry.path.clone()))
            {
                if entries.len() >= MAX_PREFETCH_CANDIDATES {
                    return Err(error::CrabError::Configuration {
                        key: "fetch candidate count".to_owned(),
                        origin: format!(
                            "reachable Git pointer inventory exceeds the safety limit of {MAX_PREFETCH_CANDIDATES}"
                        ),
                    });
                }
                entries.push(entry);
            }
        }
    }

    let blobs = git_small_blobs(root, entries.iter().map(|entry| entry.oid.as_str()))?;
    let mut candidates = Vec::new();
    let mut seen_files = HashSet::new();
    for entry in entries {
        error::check_cancelled(cancel)?;
        let Some(blob) = blobs.get(&entry.oid) else {
            continue;
        };
        let Ok(pointer) = Pointer::parse(blob) else {
            continue;
        };
        if seen_files.insert(pointer.file_hash) {
            if candidates.len() >= MAX_PREFETCH_CANDIDATES {
                return Err(error::CrabError::Configuration {
                    key: "fetch candidate count".to_owned(),
                    origin: format!(
                        "reachable Crab pointer inventory exceeds the safety limit of {MAX_PREFETCH_CANDIDATES}"
                    ),
                });
            }
            candidates.push((root.join(entry.path), pointer));
        }
    }
    Ok(candidates)
}

fn emit_empty_hydrate_summary(mode: OutputMode) {
    let summary = HydrateSummary::default();
    let payload = HydrateSummaryPayload::from_summary(&summary, Duration::default());
    match mode {
        OutputMode::Text => {
            println!("No pointer files match the given patterns.");
        }
        OutputMode::Json => {
            emit_json("hydrate", "1.0", &payload);
        }
        OutputMode::Jsonl => {
            let mut stream = JsonlStream::new("hydrate.event", "1.0", std::io::stdout());
            stream.emit_result(&payload);
        }
    }
}

fn pending_worktree_hydration(
    root: &Path,
    args: &HydrateArgs,
) -> Result<Option<PendingWorktreeHydration>> {
    if hydrate_args_has_explicit_selector(args) {
        return Ok(None);
    }

    let Some(policy) = WorktreeHydrationPolicyFile::read_for_worktree_root(root)? else {
        return Ok(None);
    };
    if policy.status != WorktreeHydrationPolicyStatus::Pending {
        return Ok(None);
    }

    Ok(hydrate_args_from_worktree_policy(args, &policy)
        .map(|args| PendingWorktreeHydration { args, policy }))
}

fn hydrate_args_has_explicit_selector(args: &HydrateArgs) -> bool {
    args.all
        || !args.patterns.is_empty()
        || !args.include.is_empty()
        || !args.exclude.is_empty()
        || args.manifest.is_some()
        || args.manifest_ref.is_some()
        || args.profile.is_some()
}

fn hydrate_args_from_worktree_policy(
    base: &HydrateArgs,
    policy: &WorktreeHydrationPolicyFile,
) -> Option<HydrateArgs> {
    let mut args = base.clone();
    args.patterns.clear();
    args.include.clear();
    args.exclude.clear();
    args.all = false;
    args.manifest = None;
    args.manifest_ref = None;
    args.profile = None;

    match &policy.selector {
        WorktreeHydrationSelector::All => {
            args.all = true;
        }
        WorktreeHydrationSelector::Patterns { include, exclude } => {
            args.include.clone_from(include);
            args.exclude.clone_from(exclude);
        }
        WorktreeHydrationSelector::Manifest { path, exclude } => {
            args.manifest = Some(path.clone());
            args.exclude.clone_from(exclude);
        }
        WorktreeHydrationSelector::ManifestRef { spec, exclude } => {
            args.manifest_ref = Some(spec.clone());
            args.exclude.clone_from(exclude);
        }
        WorktreeHydrationSelector::Profile { name, exclude } => {
            args.profile = Some(name.clone());
            args.exclude.clone_from(exclude);
        }
        WorktreeHydrationSelector::CloneDefaults => match policy.mode {
            WorktreeHydrationMode::Full => {
                args.all = true;
            }
            WorktreeHydrationMode::Lazy
            | WorktreeHydrationMode::PointerOnly
            | WorktreeHydrationMode::Selective => return None,
        },
    }

    Some(args)
}

fn mark_pending_worktree_hydration_applied(
    root: &Path,
    policy: &WorktreeHydrationPolicyFile,
) -> Result<()> {
    let ctx = WorktreeContext::resolve_from_path(root)?;
    let mut policy = policy.clone();
    policy.status = WorktreeHydrationPolicyStatus::Applied;
    policy.checkout_suppressed = false;
    policy.write_for_context(&ctx)
}

fn collect_missing_index_pointers(
    root: &Path,
    filter: &PatternFilter,
    cancel: &CancellationToken,
    out: &mut Vec<(PathBuf, Pointer)>,
) -> Result<()> {
    if WorktreeContext::resolve_from_path(root).is_err() {
        return Ok(());
    }
    collect_git_pointer_blobs(
        root,
        filter,
        cancel,
        out,
        GitPointerCollectionMode::MissingOnly,
    )
}

#[derive(Debug, Clone, Copy)]
enum GitPointerCollectionMode {
    All,
    MissingOnly,
}

fn collect_git_pointer_blobs(
    root: &Path,
    filter: &PatternFilter,
    cancel: &CancellationToken,
    out: &mut Vec<(PathBuf, Pointer)>,
    mode: GitPointerCollectionMode,
) -> Result<()> {
    let mut entries = git_index_entries(root)?;
    if entries.is_empty() {
        entries = git_head_tree_entries(root)?;
    }
    let entries = entries
        .into_iter()
        .filter(|entry| filter.matches(&entry.path))
        .filter(|entry| is_safe_repo_relative_path(Path::new(&entry.path)))
        .collect::<Vec<_>>();
    if entries.len() > MAX_PREFETCH_CANDIDATES {
        return Err(error::CrabError::Configuration {
            key: "hydrate candidate count".to_owned(),
            origin: format!(
                "Git pointer inventory exceeds the safety limit of {MAX_PREFETCH_CANDIDATES}"
            ),
        });
    }
    let blobs = git_small_blobs(root, entries.iter().map(|entry| entry.oid.as_str()))?;
    let mut seen_paths: HashSet<PathBuf> = out.iter().map(|(path, _)| path.clone()).collect();
    for entry in entries {
        error::check_cancelled(cancel)?;
        let rel_path = Path::new(&entry.path);
        let full_path = root.join(rel_path);
        if seen_paths.contains(&full_path) {
            continue;
        }
        if matches!(mode, GitPointerCollectionMode::MissingOnly) && full_path.exists() {
            continue;
        }
        let Some(blob) = blobs.get(&entry.oid) else {
            continue;
        };
        let Ok(pointer) = Pointer::parse(blob) else {
            continue;
        };
        if matches!(mode, GitPointerCollectionMode::MissingOnly)
            && let Some(parent) = full_path.parent()
        {
            std::fs::create_dir_all(parent).map_err(error::CrabError::Io)?;
        }
        if out.len() >= MAX_PREFETCH_CANDIDATES {
            return Err(error::CrabError::Configuration {
                key: "hydrate candidate count".to_owned(),
                origin: format!(
                    "Git pointer inventory exceeds the safety limit of {MAX_PREFETCH_CANDIDATES}"
                ),
            });
        }
        seen_paths.insert(full_path.clone());
        out.push((full_path, pointer));
    }
    Ok(())
}

#[derive(Debug)]
struct GitBlobEntry {
    oid: String,
    path: String,
}

fn git_index_entries(root: &Path) -> Result<Vec<GitBlobEntry>> {
    let output = Command::new("git")
        .args(["ls-files", "-s", "-z"])
        .current_dir(root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .output()
        .map_err(error::CrabError::Io)?;
    if !output.status.success() {
        return Err(error::CrabError::Internal(format!(
            "`git ls-files -s -z` failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    if output.stdout.len() > MAX_GIT_INVENTORY_BYTES {
        return Err(error::CrabError::Configuration {
            key: "Git index inventory size".to_owned(),
            origin: format!(
                "Git index listing exceeds the safety limit of {MAX_GIT_INVENTORY_BYTES} bytes"
            ),
        });
    }
    parse_git_blob_records(&output.stdout, GitBlobRecordFormat::Index)
}

fn git_head_tree_entries(root: &Path) -> Result<Vec<GitBlobEntry>> {
    if !git_ref_exists(root, "HEAD")? {
        return Ok(Vec::new());
    }
    git_tree_entries(root, "HEAD")
}

fn git_tree_entries(root: &Path, git_ref: &str) -> Result<Vec<GitBlobEntry>> {
    let output = Command::new("git")
        .args(["ls-tree", "-r", "-z", git_ref])
        .current_dir(root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .output()
        .map_err(error::CrabError::Io)?;
    if !output.status.success() {
        return Err(error::CrabError::Internal(format!(
            "`git ls-tree -r -z {git_ref}` failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    if output.stdout.len() > MAX_GIT_INVENTORY_BYTES {
        return Err(error::CrabError::Configuration {
            key: "Git tree inventory size".to_owned(),
            origin: format!(
                "Git tree listing exceeds the safety limit of {MAX_GIT_INVENTORY_BYTES} bytes"
            ),
        });
    }
    parse_git_blob_records(&output.stdout, GitBlobRecordFormat::Tree)
}

fn git_refs(root: &Path) -> Result<Vec<String>> {
    let output = Command::new("git")
        .args(["for-each-ref", "--format=%(refname)"])
        .current_dir(root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .output()
        .map_err(error::CrabError::Io)?;
    if !output.status.success() {
        return Err(error::CrabError::Internal(format!(
            "`git for-each-ref` failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    if output.stdout.len() > MAX_GIT_INVENTORY_BYTES {
        return Err(error::CrabError::Configuration {
            key: "Git ref inventory size".to_owned(),
            origin: format!(
                "Git ref listing exceeds the safety limit of {MAX_GIT_INVENTORY_BYTES} bytes"
            ),
        });
    }
    let mut refs = output
        .stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .map(|line| String::from_utf8_lossy(line).into_owned())
        .collect::<Vec<_>>();
    if git_ref_exists(root, "HEAD")? {
        refs.push("HEAD".to_owned());
    }
    refs.sort();
    refs.dedup();
    if refs.len() > MAX_PREFETCH_CANDIDATES {
        return Err(error::CrabError::Configuration {
            key: "Git ref count".to_owned(),
            origin: format!(
                "Git ref inventory exceeds the safety limit of {MAX_PREFETCH_CANDIDATES}"
            ),
        });
    }
    Ok(refs)
}

fn git_ref_exists(root: &Path, git_ref: &str) -> Result<bool> {
    let status = Command::new("git")
        .args(["rev-parse", "--verify", "--quiet", git_ref])
        .current_dir(root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map_err(error::CrabError::Io)?;
    Ok(status.success())
}

#[derive(Debug, Clone, Copy)]
enum GitBlobRecordFormat {
    Index,
    Tree,
}

fn parse_git_blob_records(bytes: &[u8], format: GitBlobRecordFormat) -> Result<Vec<GitBlobEntry>> {
    if bytes.len() > MAX_GIT_INVENTORY_BYTES {
        return Err(error::CrabError::Configuration {
            key: "Git blob inventory size".to_owned(),
            origin: format!(
                "Git blob listing exceeds the safety limit of {MAX_GIT_INVENTORY_BYTES} bytes"
            ),
        });
    }
    let mut entries = Vec::new();
    for record in bytes
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        let Some(tab) = record.iter().position(|byte| *byte == b'\t') else {
            continue;
        };
        let meta = String::from_utf8_lossy(&record[..tab]);
        let path = String::from_utf8_lossy(&record[tab + 1..]).into_owned();
        let fields = meta.split_whitespace().collect::<Vec<_>>();
        let oid = match format {
            GitBlobRecordFormat::Index if fields.len() >= 3 => fields[1],
            GitBlobRecordFormat::Tree if fields.len() >= 3 && fields[1] == "blob" => fields[2],
            GitBlobRecordFormat::Tree | GitBlobRecordFormat::Index => continue,
        };
        if entries.len() >= MAX_PREFETCH_CANDIDATES {
            return Err(error::CrabError::Configuration {
                key: "Git blob entry count".to_owned(),
                origin: format!(
                    "Git blob inventory exceeds the safety limit of {MAX_PREFETCH_CANDIDATES}"
                ),
            });
        }
        entries.push(GitBlobEntry {
            oid: oid.to_owned(),
            path,
        });
    }
    Ok(entries)
}

fn git_small_blobs<'a>(
    root: &Path,
    oids: impl Iterator<Item = &'a str>,
) -> Result<HashMap<String, Vec<u8>>> {
    let mut oids = oids.map(str::to_owned).collect::<Vec<_>>();
    oids.sort();
    oids.dedup();
    if oids.is_empty() {
        return Ok(HashMap::new());
    }
    let input = format!("{}\n", oids.join("\n"));
    if input.len() > MAX_GIT_INVENTORY_BYTES {
        return Err(error::CrabError::Configuration {
            key: "Git blob request size".to_owned(),
            origin: format!(
                "Git blob request exceeds the safety limit of {MAX_GIT_INVENTORY_BYTES} bytes"
            ),
        });
    }

    let check = Command::new("git")
        .args([
            "cat-file",
            "--batch-check=%(objectname) %(objecttype) %(objectsize)",
        ])
        .current_dir(root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .map_err(error::CrabError::Io)?;
    let checked = command_output_with_input(check, input, "git cat-file --batch-check")?;
    if !checked.status.success() {
        return Err(error::CrabError::Internal(format!(
            "`git cat-file --batch-check` failed: {}",
            String::from_utf8_lossy(&checked.stderr)
        )));
    }
    if checked.stdout.len() > MAX_GIT_INVENTORY_BYTES {
        return Err(error::CrabError::Configuration {
            key: "Git blob metadata response size".to_owned(),
            origin: format!(
                "Git blob metadata response exceeds the safety limit of {MAX_GIT_INVENTORY_BYTES} bytes"
            ),
        });
    }
    let small_oids = String::from_utf8_lossy(&checked.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let oid = fields.next()?;
            let kind = fields.next()?;
            let size = fields.next()?.parse::<usize>().ok()?;
            (kind == "blob" && size <= MAX_POINTER_SIZE).then(|| oid.to_owned())
        })
        .collect::<Vec<_>>();
    if small_oids.is_empty() {
        return Ok(HashMap::new());
    }

    let batch = Command::new("git")
        .args(["cat-file", "--batch"])
        .current_dir(root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .map_err(error::CrabError::Io)?;
    let batch_input = format!("{}\n", small_oids.join("\n"));
    if batch_input.len() > MAX_GIT_INVENTORY_BYTES {
        return Err(error::CrabError::Configuration {
            key: "Git blob request size".to_owned(),
            origin: format!(
                "Git blob request exceeds the safety limit of {MAX_GIT_INVENTORY_BYTES} bytes"
            ),
        });
    }
    let output = command_output_with_input(batch, batch_input, "git cat-file --batch")?;
    if !output.status.success() {
        return Err(error::CrabError::Internal(format!(
            "`git cat-file --batch` failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    if output.stdout.len() > MAX_GIT_INVENTORY_BYTES {
        return Err(error::CrabError::Configuration {
            key: "Git blob response size".to_owned(),
            origin: format!(
                "Git blob response exceeds the safety limit of {MAX_GIT_INVENTORY_BYTES} bytes"
            ),
        });
    }

    let mut blobs = HashMap::new();
    let mut cursor = 0usize;
    while cursor < output.stdout.len() {
        let Some(header_len) = output.stdout[cursor..]
            .iter()
            .position(|byte| *byte == b'\n')
        else {
            return Err(error::CrabError::Internal(
                "git cat-file batch response ended inside a header".to_owned(),
            ));
        };
        let header_end = cursor + header_len;
        let header = String::from_utf8_lossy(&output.stdout[cursor..header_end]);
        let mut fields = header.split_whitespace();
        let oid = fields.next().ok_or_else(|| {
            error::CrabError::Internal("git cat-file returned an empty header".to_owned())
        })?;
        let kind = fields.next().ok_or_else(|| {
            error::CrabError::Internal("git cat-file returned a malformed header".to_owned())
        })?;
        let size = fields
            .next()
            .ok_or_else(|| {
                error::CrabError::Internal("git cat-file omitted object size".to_owned())
            })?
            .parse::<usize>()
            .map_err(|e| {
                error::CrabError::Internal(format!("git cat-file returned bad object size: {e}"))
            })?;
        let content_start = header_end + 1;
        let content_end = content_start.checked_add(size).ok_or_else(|| {
            error::CrabError::Internal("git cat-file object size overflow".to_owned())
        })?;
        if kind != "blob" || content_end >= output.stdout.len() {
            return Err(error::CrabError::Internal(
                "git cat-file returned a truncated or non-blob response".to_owned(),
            ));
        }
        blobs.insert(
            oid.to_owned(),
            output.stdout[content_start..content_end].to_vec(),
        );
        cursor = content_end + 1;
    }
    Ok(blobs)
}

fn command_output_with_input(
    mut child: std::process::Child,
    input: String,
    operation: &str,
) -> Result<std::process::Output> {
    let mut stdin = child
        .stdin
        .take()
        .ok_or_else(|| error::CrabError::Internal(format!("{operation} stdin unavailable")))?;
    // Read output while input is written. Large Git batches can fill both OS
    // pipes and deadlock if either side is drained only after the other.
    let writer = std::thread::spawn(move || stdin.write_all(input.as_bytes()));
    let output = child.wait_with_output().map_err(error::CrabError::Io)?;
    let write_result = writer
        .join()
        .map_err(|_| error::CrabError::Internal(format!("{operation} stdin writer panicked")))?;
    if output.status.success() {
        write_result.map_err(error::CrabError::Io)?;
    }
    Ok(output)
}

fn is_safe_repo_relative_path(path: &Path) -> bool {
    !path.is_absolute()
        && path
            .components()
            .all(|component| !matches!(component, Component::ParentDir | Component::Prefix(_)))
}

/// Resolve a manifest from `--manifest`, `--manifest-ref`, or `--profile` args.
///
/// Returns `None` when none of the flags are set. The three flags are
/// mutually exclusive (enforced by clap's `conflicts_with_all`).
///
/// `--manifest <path>` reads from a file (or stdin when `path` is `-`).
/// `--manifest-ref <ref>` reads from a Git ref via `git show <ref>`.
/// `--profile <name>` loads the named profile from `crab.toml`.
fn resolve_manifest(
    args: &HydrateArgs,
    root: &Path,
) -> Result<Option<Vec<crate::hydrate::manifest::ManifestEntry>>> {
    use crate::hydrate::manifest::{parse_manifest, parse_manifest_from};

    // Handle --profile: load the prefetch config and convert the named
    // profile's globs into manifest entries.
    if let Some(ref profile_name) = args.profile {
        debug!(profile = %profile_name, "resolving manifest from prefetch profile");
        let config = crate::hydrate::profile::load_prefetch(root)?;
        let globs = config.profile(profile_name.as_str())?;
        let entries: Vec<crate::hydrate::manifest::ManifestEntry> = globs
            .iter()
            .map(|g| crate::hydrate::manifest::ManifestEntry::Glob(g.clone()))
            .collect();
        info!(
            profile = %profile_name,
            entries = entries.len(),
            "resolved prefetch profile to manifest entries"
        );
        return Ok(Some(entries));
    }

    match (&args.manifest, &args.manifest_ref) {
        (Some(_), Some(_)) => Err(error::CrabError::Configuration {
            key: "--manifest / --manifest-ref".to_string(),
            origin: "CLI flags are mutually exclusive".to_string(),
        }),
        (Some(path), None) => {
            debug!(path = %path, "resolving manifest from file");
            let entries = parse_manifest_from(path)?;
            info!(entries = entries.len(), path = %path, "parsed manifest file");
            Ok(Some(entries))
        }
        (None, Some(git_ref)) => {
            debug!(git_ref = %git_ref, "resolving manifest from git ref");
            let content = git_show_ref(git_ref)?;
            let reader = std::io::Cursor::new(content);
            let entries = parse_manifest(reader)?;
            info!(entries = entries.len(), git_ref = %git_ref, "parsed manifest from git ref");
            Ok(Some(entries))
        }
        (None, None) => Ok(None),
    }
}

/// Read the contents of a Git object via `git show <ref>`.
///
/// Used by `--manifest-ref` to extract a manifest file from a committed
/// tree (e.g. `HEAD:.crab/manifests/ci.txt`).
fn git_show_ref(git_ref: &str) -> Result<String> {
    let output = std::process::Command::new("git")
        .args(["show", git_ref])
        .output()
        .map_err(|e| error::CrabError::Internal(format!("failed to run `git show`: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(error::CrabError::Internal(format!(
            "`git show {git_ref}` failed: {stderr}"
        )));
    }
    if output.stdout.len() > crate::hydrate::manifest::MAX_MANIFEST_BYTES {
        return Err(error::CrabError::Configuration {
            key: "manifest-ref size".to_owned(),
            origin: format!(
                "git show output exceeds the safety limit of {} bytes",
                crate::hydrate::manifest::MAX_MANIFEST_BYTES
            ),
        });
    }

    String::from_utf8(output.stdout).map_err(|e| {
        error::CrabError::Internal(format!("`git show {git_ref}` produced invalid UTF-8: {e}"))
    })
}

/// Build a [`PatternFilter`] from parsed manifest entries.
///
/// Literal paths are converted to exact-match globs; glob entries are
/// used directly. The resulting filter matches any path that appears in
/// the manifest.
fn build_manifest_filter(
    entries: &[crate::hydrate::manifest::ManifestEntry],
    exclude: &[String],
) -> Result<PatternFilter> {
    let mut patterns: Vec<String> = Vec::with_capacity(entries.len());
    for entry in entries {
        match entry {
            crate::hydrate::manifest::ManifestEntry::Path(p) => {
                // Literal paths need to match exactly. Convert to a
                // string pattern that build_filter can compile.
                patterns.push(p.to_string_lossy().into_owned());
            }
            crate::hydrate::manifest::ManifestEntry::Glob(g) => {
                patterns.push(g.glob().to_owned());
            }
        }
    }
    build_filter(&patterns, exclude)
}

/// Resolve effective patterns from args and config.
///
/// Priority: `--all` > positional globs > `--include/--exclude` >
/// config `hydrate.include/exclude` > `None` (print help).
fn resolve_patterns(args: &HydrateArgs, config: &Config) -> Result<Option<PatternFilter>> {
    // --all: match everything.
    if args.all {
        let filter = build_all_filter()?;
        return Ok(Some(filter));
    }

    // Positional globs take priority.
    if !args.patterns.is_empty() {
        let filter = build_filter(&args.patterns, &[])?;
        return Ok(Some(filter));
    }

    // Explicit --include/--exclude flags.
    if !args.include.is_empty() || !args.exclude.is_empty() {
        let include = if args.include.is_empty() {
            vec!["*".to_owned()]
        } else {
            args.include.clone()
        };
        let filter = build_filter(&include, &args.exclude)?;
        return Ok(Some(filter));
    }

    // Fall back to persistent config patterns.
    if !config.hydrate.include.is_empty() || !config.hydrate.exclude.is_empty() {
        let include = if config.hydrate.include.is_empty() {
            vec!["*".to_owned()]
        } else {
            config.hydrate.include.clone()
        };
        let filter = build_filter(&include, &config.hydrate.exclude)?;
        return Ok(Some(filter));
    }

    // Nothing specified — caller should print help.
    Ok(None)
}

fn build_all_filter() -> Result<PatternFilter> {
    // Git pathspec `**/*` excludes repository-root files; bare `*`
    // matches both root and nested paths.
    build_filter(&["*".to_owned()], &[])
}

/// Print a help message when no patterns are provided.
fn print_help() {
    eprintln!("Usage: crab hydrate <glob>... [--include=<pattern>] [--exclude=<pattern>] [--all]");
    eprintln!("       crab hydrate --manifest <path>");
    eprintln!("       crab hydrate --manifest-ref <ref>");
    eprintln!("       crab hydrate --profile <name>");
    eprintln!();
    eprintln!("Materialize pointer files into full content.");
    eprintln!();
    eprintln!("Examples:");
    eprintln!("  crab hydrate \"*.safetensors\"");
    eprintln!("  crab hydrate --include=\"models/**\" --exclude=\"models/archive/**\"");
    eprintln!("  crab hydrate --all");
    eprintln!("  crab hydrate --manifest .crab/manifests/ci.txt");
    eprintln!("  git ls-files | crab hydrate --manifest -");
    eprintln!("  crab hydrate --manifest-ref HEAD:.crab/manifests/ci.txt");
    eprintln!("  crab hydrate --profile ci");
    eprintln!();
    eprintln!("Or set persistent patterns in .crab/local.toml:");
    eprintln!("  [hydrate]");
    eprintln!("  include = [\"models/current/**\"]");
    eprintln!("  exclude = [\"models/current/archive/**\"]");
}

/// Per-site classifier for `.gitattributes filter=crab` lookup.
///
/// Under `gix-pathmatch`, wraps the consolidated
/// [`core::attrs::TrackedClassifier`] (backed by `gix_attributes::Search`).
/// Otherwise falls back to the legacy suffix-matching helper driven by
/// patterns parsed out of the root `.gitattributes` line-by-line.
#[cfg(feature = "gix-pathmatch")]
struct TrackedClassifier(crate::core::attrs::TrackedClassifier);

#[cfg(not(feature = "gix-pathmatch"))]
struct TrackedClassifier {
    patterns: Vec<String>,
}

impl TrackedClassifier {
    fn open(root: &Path) -> Result<Self> {
        #[cfg(feature = "gix-pathmatch")]
        {
            Ok(TrackedClassifier(
                crate::core::attrs::TrackedClassifier::open(root, "crab")?,
            ))
        }
        #[cfg(not(feature = "gix-pathmatch"))]
        {
            Ok(TrackedClassifier {
                patterns: parse_gitattributes_globs_legacy(root)?,
            })
        }
    }

    fn is_empty(&self) -> bool {
        #[cfg(feature = "gix-pathmatch")]
        {
            false
        }
        #[cfg(not(feature = "gix-pathmatch"))]
        {
            self.patterns.is_empty()
        }
    }

    fn is_tracked(&self, rel_path: &Path) -> bool {
        let rel_str = rel_path.to_string_lossy();
        #[cfg(feature = "gix-pathmatch")]
        {
            self.0.is_tracked(&rel_str)
        }
        #[cfg(not(feature = "gix-pathmatch"))]
        {
            let _ = rel_str;
            matches_any_tracked_legacy(rel_path, &self.patterns)
        }
    }
}

/// Legacy fallback for builds without `gix-pathmatch`. The consolidated
/// matcher lives in `core::attrs`.
#[cfg(not(feature = "gix-pathmatch"))]
fn parse_gitattributes_globs_legacy(root: &Path) -> Result<Vec<String>> {
    let ga_path = root.join(".gitattributes");
    let content = match std::fs::read_to_string(&ga_path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };

    let globs = content
        .lines()
        .filter(|line| {
            let trimmed = line.trim();
            !trimmed.is_empty() && !trimmed.starts_with('#') && trimmed.contains("filter=crab")
        })
        .filter_map(|line| line.split_whitespace().next().map(String::from))
        .collect();

    Ok(globs)
}

/// Legacy suffix-matching helper retained for builds without
/// `gix-pathmatch`.
#[cfg(not(feature = "gix-pathmatch"))]
fn matches_any_tracked_legacy(rel_path: &Path, patterns: &[String]) -> bool {
    let path_str = rel_path.to_string_lossy();

    for pattern in patterns {
        if pattern == "*" || pattern == "**" || pattern == "**/*" {
            return true;
        }
        if let Some(suffix) = pattern.strip_prefix('*')
            && path_str.ends_with(suffix)
        {
            return true;
        }
        if *pattern == *path_str {
            return true;
        }
    }
    false
}

/// Recursively walk the directory tree, collecting pointer files that
/// match both the tracked patterns and the user's hydrate filter.
///
/// Each collected entry is the absolute path paired with its parsed
/// [`Pointer`], ready for batch hydration.
fn walk_and_parse_pointers(
    root: &Path,
    dir: &Path,
    tracked: &TrackedClassifier,
    filter: &PatternFilter,
    cancel: &CancellationToken,
    out: &mut Vec<(PathBuf, Pointer)>,
) -> Result<()> {
    error::check_cancelled(cancel)?;

    let entries = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            debug!(dir = %dir.display(), "skipping unreadable directory");
            return Ok(());
        }
        Err(e) => return Err(e.into()),
    };

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let file_type = entry.file_type()?;

        // Skip hidden directories (.git, .crab, etc.).
        if file_type.is_dir() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with('.') {
                continue;
            }
            walk_and_parse_pointers(root, &path, tracked, filter, cancel, out)?;
            continue;
        }

        if !file_type.is_file() {
            continue;
        }

        let Ok(rel_path) = path.strip_prefix(root) else {
            continue;
        };

        // Must be a crab-tracked file.
        if !tracked.is_tracked(rel_path) {
            continue;
        }

        let rel_str = rel_path.to_string_lossy();

        // Must match the user's hydrate pattern filter.
        if !filter.matches(&rel_str) {
            continue;
        }

        // Must be in pointer state (unhydrated). Parse the pointer for
        // the hydration batch.
        let metadata = std::fs::metadata(&path)?;
        if metadata.len() > crab_types::pointer::MAX_POINTER_SIZE as u64 {
            continue;
        }
        let content = std::fs::read(&path)?;
        let Ok(ptr) = Pointer::parse(&content) else {
            continue;
        };

        out.push((path.clone(), ptr));
    }

    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;

    #[derive(Default)]
    struct PartialVectoredWriter {
        bytes: Vec<u8>,
    }

    impl std::io::Write for PartialVectoredWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let written = buf.len().min(2);
            self.bytes.extend_from_slice(&buf[..written]);
            Ok(written)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }

        fn write_vectored(&mut self, bufs: &[std::io::IoSlice<'_>]) -> std::io::Result<usize> {
            let mut remaining = 5usize;
            let mut written = 0usize;
            for buf in bufs {
                let take = remaining.min(buf.len());
                self.bytes.extend_from_slice(&buf[..take]);
                written += take;
                remaining -= take;
                if remaining == 0 {
                    break;
                }
            }
            Ok(written)
        }
    }

    #[test]
    fn generic_hasher_tap_hashes_only_partial_vectored_write() {
        let state = Arc::new(std::sync::Mutex::new(GenericHasherTapState {
            writer: Some(PartialVectoredWriter::default()),
            hasher: blake3::Hasher::new(),
            bytes_written: 0,
        }));
        let mut tap = GenericHasherTap {
            shared: Arc::clone(&state),
        };
        let bufs = [
            std::io::IoSlice::new(b"abc"),
            std::io::IoSlice::new(b"defg"),
        ];

        let written = tap.write_vectored(&bufs).expect("vectored write");

        assert_eq!(written, 5);
        let state = state.lock().expect("tap state");
        assert_eq!(state.bytes_written, 5);
        assert_eq!(state.writer.as_ref().expect("writer").bytes, b"abcde");
        assert_eq!(state.hasher.clone().finalize(), blake3::hash(b"abcde"));
    }

    use std::future::Future;
    use std::io::{self, Write};
    use std::pin::Pin;
    use std::process::Command;
    use std::sync::MutexGuard;

    use crate::git::push::{PushConfig, RefPushOutcome, run_push_batch};
    use crate::git::remote_helper::PushSpec;
    use crate::storage::StoreLayout;
    use crate::storage::store::Store;
    use crate::test::git_repo::{CacheDirGuard, GIT_DIR_MUTEX};
    use bytes::Bytes;
    use crab_staging::{StagingArea, StagingAreaReadOnly};
    use crab_types::pointer::Pointer;
    use crab_xet::chunker::GearChunker;
    use object_store::memory::InMemory;

    const TEST_REPLICA_PREFIX: &str = "org/repo";

    struct ScopedGitDir {
        _lock: MutexGuard<'static, ()>,
        prev: Option<String>,
    }

    impl ScopedGitDir {
        fn acquire() -> Self {
            let lock = GIT_DIR_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
            let prev = std::env::var("GIT_DIR").ok();
            // SAFETY: access is serialised by GIT_DIR_MUTEX.
            unsafe { std::env::remove_var("GIT_DIR") };
            Self { _lock: lock, prev }
        }

        fn set_git_dir(&self, git_dir: &Path) {
            // SAFETY: access is serialised by GIT_DIR_MUTEX.
            unsafe { std::env::set_var("GIT_DIR", git_dir) };
        }
    }

    impl Drop for ScopedGitDir {
        fn drop(&mut self) {
            match &self.prev {
                // SAFETY: access is serialised by GIT_DIR_MUTEX.
                Some(value) => unsafe { std::env::set_var("GIT_DIR", value) },
                None => unsafe { std::env::remove_var("GIT_DIR") },
            }
        }
    }

    struct GitFixture {
        dir: tempfile::TempDir,
    }

    impl GitFixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().expect("create tempdir");
            run_git(dir.path(), &["init", "--initial-branch=main"]);
            run_git(dir.path(), &["config", "user.email", "test@test.com"]);
            run_git(dir.path(), &["config", "user.name", "Test"]);
            run_git(dir.path(), &["config", "commit.gpgsign", "false"]);
            Self { dir }
        }

        fn work_tree(&self) -> &Path {
            self.dir.path()
        }

        fn git_dir(&self) -> PathBuf {
            self.dir.path().join(".git")
        }
    }

    fn run_git(cwd: &Path, args: &[&str]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .output()
            .unwrap_or_else(|err| panic!("failed to spawn git {args:?}: {err}"));
        if !output.status.success() {
            let _ = writeln!(
                io::stderr(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            panic!("git command failed");
        }
    }

    fn git_stdout(cwd: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(cwd)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .output()
            .unwrap_or_else(|err| panic!("failed to spawn git {args:?}: {err}"));
        if !output.status.success() {
            panic!(
                "git {args:?} failed\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    #[cfg(unix)]
    #[test]
    fn refresh_index_entry_clears_filtered_status_after_hydrate_and_dehydrate() {
        use std::os::unix::fs::PermissionsExt;

        let _git_env = ScopedGitDir::acquire();
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();

        let clean_script = tmp.path().join("clean.sh");
        let clean_calls = tmp.path().join("clean-calls");
        std::fs::write(
            &clean_script,
            format!(
                "#!/bin/sh\nprintf x >> '{}'\ncat >/dev/null\nprintf 'POINTER\\n'\n",
                clean_calls.display()
            ),
        )
        .unwrap();
        let mut perms = std::fs::metadata(&clean_script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&clean_script, perms).unwrap();

        run_git(&repo, &["init", "--initial-branch=main"]);
        run_git(&repo, &["config", "user.email", "test@test.com"]);
        run_git(&repo, &["config", "user.name", "Test"]);
        run_git(
            &repo,
            &[
                "config",
                "filter.demo.clean",
                clean_script.to_str().unwrap(),
            ],
        );
        run_git(&repo, &["config", "filter.demo.smudge", "cat"]);
        run_git(&repo, &["config", "filter.demo.required", "true"]);

        std::fs::write(repo.join(".gitattributes"), "*.bin filter=demo -text\n").unwrap();
        std::fs::write(repo.join("model.bin"), "POINTER\n").unwrap();
        run_git(&repo, &["add", ".gitattributes", "model.bin"]);
        run_git(&repo, &["commit", "-m", "init"]);

        atomic_write_with_progress(&repo.join("model.bin"), b"hydrated bytes\n", None).unwrap();
        assert_eq!(git_stdout(&repo, &["diff", "--name-only"]), "");
        assert!(git_stdout(&repo, &["status", "--porcelain=v1"]).contains(" M model.bin"));
        std::fs::remove_file(&clean_calls).unwrap();

        assert_eq!(
            crate::git::worktree::refresh_index_stats(&repo, &[repo.join("model.bin")]).unwrap(),
            1
        );

        let index = gix_index::File::at(
            repo.join(".git/index"),
            gix_hash::Kind::Sha1,
            true,
            gix_index::decode::Options::default(),
        )
        .unwrap();
        let entry = index
            .entries()
            .iter()
            .find(|entry| entry.path(&index) == b"model.bin")
            .unwrap();
        let metadata =
            gix_index::fs::Metadata::from_path_no_follow(&repo.join("model.bin")).unwrap();
        let expected_stat = gix_index::entry::Stat::from_fs(&metadata).unwrap();
        assert_eq!(entry.stat, expected_stat);
        assert_eq!(git_stdout(&repo, &["status", "--porcelain=v1"]), "");
        assert!(!clean_calls.exists());

        atomic_write_with_progress(&repo.join("model.bin"), b"POINTER\n", None).unwrap();
        assert!(git_stdout(&repo, &["status", "--porcelain=v1"]).contains(" M model.bin"));
        let _ = std::fs::remove_file(&clean_calls);
        assert_eq!(
            crate::git::worktree::refresh_index_stats(&repo, &[repo.join("model.bin")]).unwrap(),
            1
        );
        assert_eq!(git_stdout(&repo, &["status", "--porcelain=v1"]), "");
        assert!(!clean_calls.exists());
    }

    #[cfg(unix)]
    #[test]
    fn refresh_hydrated_index_entries_updates_multiple_files_in_one_batch() {
        use std::os::unix::fs::PermissionsExt;

        let _git_env = ScopedGitDir::acquire();
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();

        let clean_script = tmp.path().join("clean.sh");
        std::fs::write(
            &clean_script,
            "#!/bin/sh\ncat >/dev/null\nprintf 'POINTER\\n'\n",
        )
        .unwrap();
        let mut perms = std::fs::metadata(&clean_script).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&clean_script, perms).unwrap();

        run_git(&repo, &["init", "--initial-branch=main"]);
        run_git(
            &repo,
            &[
                "config",
                "filter.demo.clean",
                clean_script.to_str().unwrap(),
            ],
        );
        run_git(&repo, &["config", "filter.demo.smudge", "cat"]);
        run_git(&repo, &["config", "filter.demo.required", "true"]);

        std::fs::write(repo.join(".gitattributes"), "*.bin filter=demo -text\n").unwrap();
        std::fs::write(repo.join("model.bin"), "POINTER\n").unwrap();
        std::fs::write(repo.join("adapter.bin"), "POINTER\n").unwrap();
        run_git(
            &repo,
            &["add", ".gitattributes", "model.bin", "adapter.bin"],
        );

        std::fs::write(repo.join("model.bin"), "hydrated model bytes\n").unwrap();
        std::fs::write(repo.join("adapter.bin"), "hydrated adapter bytes\n").unwrap();
        assert_eq!(
            crate::git::worktree::refresh_index_stats(
                &repo,
                &[repo.join("model.bin"), repo.join("adapter.bin")],
            )
            .unwrap(),
            2
        );

        let index = gix_index::File::at(
            repo.join(".git/index"),
            gix_hash::Kind::Sha1,
            true,
            gix_index::decode::Options::default(),
        )
        .unwrap();
        for name in ["model.bin", "adapter.bin"] {
            let entry = index
                .entries()
                .iter()
                .find(|entry| entry.path(&index) == name.as_bytes())
                .unwrap();
            let metadata = gix_index::fs::Metadata::from_path_no_follow(&repo.join(name)).unwrap();
            assert_eq!(
                entry.stat,
                gix_index::entry::Stat::from_fs(&metadata).unwrap()
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn hydrate_proof_seeds_first_add_validation() {
        use bstr::ByteSlice;

        let _git_env = ScopedGitDir::acquire();
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        run_git(&repo, &["init", "--initial-branch=main"]);

        let content = b"verified hydrated bytes";
        let pointer = Pointer {
            file_hash: *blake3::hash(content).as_bytes(),
            size: content.len() as u64,
            shard_hint: None,
        };
        let path = repo.join("model.bin");
        std::fs::write(&path, pointer.serialize()).unwrap();
        run_git(&repo, &["add", "model.bin"]);
        let index_stat = atomic_write_with_progress(&path, content, None).unwrap();
        if !crate::cache::add_validation::stat_is_cacheable(&index_stat.stat) {
            eprintln!("SKIP: filesystem change-time precision cannot support validation cache");
            return;
        }
        let verified = crate::cache::add_validation::VerifiedPath {
            path: path.clone(),
            file_hash: pointer.file_hash,
            size: pointer.size,
            index_stat,
        };

        refresh_hydrated_index_entries(&repo, std::slice::from_ref(&verified));

        let context = WorktreeContext::resolve_from_path(&repo).unwrap();
        let index = gix_index::File::at(
            context.index_path(),
            gix_hash::Kind::Sha1,
            true,
            gix_index::decode::Options::default(),
        )
        .unwrap();
        let entry = index
            .entry_by_path_and_stage(
                b"model.bin".as_bstr(),
                gix_index::entry::Stage::Unconflicted,
            )
            .unwrap();
        let git_repo = gix::open(&repo).unwrap();
        let blob = git_repo.find_blob(entry.id).unwrap();
        let token = crate::cache::add_validation::validation_token(
            b"model.bin",
            entry.id.as_bytes(),
            &blob.data,
            entry.mode.bits(),
            &index_stat.stat,
            index_stat.len,
        );
        let cache_path = crate::cache::add_validation::cache_path_for_context(&context);
        let cache = crate::cache::add_validation::AddValidationCache::open(&cache_path).unwrap();

        assert!(cache.contains(b"model.bin", &token).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn hydrate_proof_rejects_content_changed_before_index_refresh() {
        use bstr::ByteSlice;

        let _git_env = ScopedGitDir::acquire();
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        run_git(&repo, &["init", "--initial-branch=main"]);

        let content = b"verified hydrated bytes";
        let replacement = b"modified hydrated bytes";
        assert_eq!(content.len(), replacement.len());
        let pointer = Pointer {
            file_hash: *blake3::hash(content).as_bytes(),
            size: content.len() as u64,
            shard_hint: None,
        };
        let path = repo.join("model.bin");
        std::fs::write(&path, pointer.serialize()).unwrap();
        run_git(&repo, &["add", "model.bin"]);
        let index_stat = atomic_write_with_progress(&path, content, None).unwrap();
        let verified = crate::cache::add_validation::VerifiedPath {
            path: path.clone(),
            file_hash: pointer.file_hash,
            size: pointer.size,
            index_stat,
        };
        std::fs::write(&path, replacement).unwrap();

        refresh_hydrated_index_entries(&repo, std::slice::from_ref(&verified));

        let context = WorktreeContext::resolve_from_path(&repo).unwrap();
        let index = gix_index::File::at(
            context.index_path(),
            gix_hash::Kind::Sha1,
            true,
            gix_index::decode::Options::default(),
        )
        .unwrap();
        let entry = index
            .entry_by_path_and_stage(
                b"model.bin".as_bstr(),
                gix_index::entry::Stage::Unconflicted,
            )
            .unwrap();
        let current =
            crate::cmd::stream_stage::VerifiedIndexStat::from_path_no_follow(&path).unwrap();
        assert_ne!(entry.stat, current.stat);
        let cache_path = crate::cache::add_validation::cache_path_for_context(&context);
        let connection = rusqlite::Connection::open(cache_path).unwrap();
        let rows: u64 = connection
            .query_row("SELECT COUNT(*) FROM add_validations", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 0);
    }

    #[cfg(unix)]
    #[test]
    fn hydrate_proof_rejects_pointer_changed_before_index_refresh() {
        use bstr::ByteSlice;

        let _git_env = ScopedGitDir::acquire();
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        run_git(&repo, &["init", "--initial-branch=main"]);

        let content = b"verified hydrated bytes";
        let pointer = pointer_for_content(content);
        let path = repo.join("model.bin");
        std::fs::write(&path, pointer.serialize()).unwrap();
        run_git(&repo, &["add", "model.bin"]);
        let index_stat = atomic_write_with_progress(&path, content, None).unwrap();
        let verified = crate::cache::add_validation::VerifiedPath {
            path: path.clone(),
            file_hash: pointer.file_hash,
            size: pointer.size,
            index_stat,
        };

        let mut other_hash = pointer.file_hash;
        other_hash[0] ^= 0xff;
        let other_pointer = Pointer {
            file_hash: other_hash,
            size: pointer.size,
            shard_hint: None,
        };
        let pointer_file = repo.join("other.pointer");
        std::fs::write(&pointer_file, other_pointer.serialize()).unwrap();
        let oid = git_stdout(&repo, &["hash-object", "-w", "other.pointer"])
            .trim()
            .to_owned();
        let cache_info = format!("100644,{oid},model.bin");
        run_git(&repo, &["update-index", "--cacheinfo", &cache_info]);
        assert_eq!(
            crate::cmd::stream_stage::VerifiedIndexStat::from_path_no_follow(&path),
            Some(index_stat)
        );

        refresh_hydrated_index_entries(&repo, std::slice::from_ref(&verified));

        let context = WorktreeContext::resolve_from_path(&repo).unwrap();
        let index = gix_index::File::at(
            context.index_path(),
            gix_hash::Kind::Sha1,
            true,
            gix_index::decode::Options::default(),
        )
        .unwrap();
        let entry = index
            .entry_by_path_and_stage(
                b"model.bin".as_bstr(),
                gix_index::entry::Stage::Unconflicted,
            )
            .unwrap();
        assert_eq!(entry.id.to_hex().to_string(), oid);
        assert_ne!(entry.stat, index_stat.stat);

        let cache_path = crate::cache::add_validation::cache_path_for_context(&context);
        let connection = rusqlite::Connection::open(cache_path).unwrap();
        let rows: u64 = connection
            .query_row("SELECT COUNT(*) FROM add_validations", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 0);
    }

    fn run_git_without_crab_filter(cwd: &Path, args: &[&str]) {
        let mut git_args = vec![
            "-c",
            "filter.crab.process=",
            "-c",
            "filter.crab.clean=",
            "-c",
            "filter.crab.smudge=",
            "-c",
            "filter.crab.required=false",
        ];
        git_args.extend_from_slice(args);
        run_git(cwd, &git_args);
    }

    fn sample_hash() -> [u8; 32] {
        let mut h = [0u8; 32];
        for (i, byte) in h.iter_mut().enumerate() {
            *byte = i as u8;
        }
        h
    }

    fn sample_pointer(size: u64) -> Pointer {
        Pointer {
            file_hash: sample_hash(),
            size,
            shard_hint: None,
        }
    }

    #[test]
    fn hydrate_file_concurrency_tracks_download_bound_with_internal_cap() {
        assert_eq!(hydrate_file_concurrency(0), 1);
        assert_eq!(hydrate_file_concurrency(3), 3);
        assert_eq!(hydrate_file_concurrency(32), MAX_HYDRATE_FILE_CONCURRENCY);
    }

    #[test]
    fn hydrate_buffer_semaphore_preserves_byte_budget() {
        let budget = 384 * 1024 * 1024;
        let semaphore = hydrate_buffer_semaphore(budget);

        assert_eq!(semaphore.total_permits(), budget);
        assert_eq!(semaphore.available_permits(), budget);
    }

    #[cfg(unix)]
    #[test]
    fn ensure_atomic_reconstruction_space_reports_insufficient_space() {
        let dir = tempfile::tempdir().unwrap();
        let err = ensure_atomic_reconstruction_space(dir.path(), u64::MAX)
            .expect_err("impossible reconstruction size should fail preflight");

        assert!(
            matches!(
                err,
                error::CrabError::InsufficientSpace {
                    needed: u64::MAX,
                    available
                } if available < u64::MAX
            ),
            "unexpected error: {err}"
        );
    }

    fn command_path_content() -> Vec<u8> {
        b"replica hydrate command path proof\n".repeat(4096)
    }

    fn chunk_and_hash(content: &[u8]) -> (Vec<(MerkleHash, Bytes)>, [u8; 32]) {
        let file_hash = *blake3::hash(content).as_bytes();
        let mut chunker = GearChunker::new();
        let mut chunks = Vec::new();
        for block in content.chunks(128 * 1024) {
            for chunk in chunker.feed(block) {
                chunks.push((chunk.hash, chunk.data));
            }
        }
        if let Some(last) = chunker.finalize() {
            chunks.push((last.hash, last.data));
        }
        (chunks, file_hash)
    }

    async fn stage_command_path_file(
        work_tree: &Path,
        file_hash: [u8; 32],
        chunks: &[(MerkleHash, Bytes)],
        size: u64,
    ) -> StagingAreaReadOnly {
        let staging_root = work_tree.join(".crab").join("staging");
        let staging = StagingArea::open(staging_root.clone())
            .await
            .expect("open staging");
        let file_merkle = MerkleHash::from(file_hash);
        staging
            .pre_register_file(&file_merkle, size)
            .expect("pre-register file");
        let refs = chunks
            .iter()
            .map(|(hash, data)| (hash, data.as_ref()))
            .collect::<Vec<_>>();
        const BATCH: usize = 512;
        let mut offset: u64 = 0;
        for group in refs.chunks(BATCH) {
            staging
                .stage_chunks_batch(group, &file_merkle, offset)
                .await
                .expect("stage chunks");
            offset += group.len() as u64;
        }
        staging.flush_pending().await.expect("flush staging");
        let chunk_pairs = chunks
            .iter()
            .map(|(hash, data)| (*hash, data.len() as u64))
            .collect::<Vec<_>>();
        let recipe = crab_staging::recipe::FileRecipe::from_staged_chunks(
            crab_staging::recipe::ChunkingPolicyId::XetGearV1_64KiB,
            file_merkle,
            size,
            &chunk_pairs,
        )
        .expect("build command-path recipe");
        staging
            .publish_verified_recipe_lease(Path::new("model.bin"), &recipe)
            .expect("publish command-path recipe");
        staging.close().await.expect("close staging");
        StagingAreaReadOnly::open(staging_root)
            .await
            .expect("reopen staging read-only")
    }

    #[tokio::test]
    async fn staging_fallback_restores_unpushed_pointer() {
        let fixture = GitFixture::new();
        let content = command_path_content();
        let (chunks, file_hash) = chunk_and_hash(&content);
        let pointer = Pointer {
            file_hash,
            size: content.len() as u64,
            shard_hint: None,
        };
        let staging =
            stage_command_path_file(fixture.work_tree(), file_hash, &chunks, pointer.size).await;
        drop(staging);
        let path = fixture.work_tree().join("model.bin");
        std::fs::write(&path, pointer.serialize()).expect("write pointer");

        let restored = try_hydrate_from_staging(&path, &pointer, &CancellationToken::new())
            .await
            .expect("staging fallback");

        assert_eq!(restored.map(|write| write.bytes), Some(pointer.size));
        assert_eq!(std::fs::read(path).expect("read restored file"), content);
    }

    async fn push_command_path_pointer(
        fixture: &GitFixture,
        store: Store,
        staging: StagingAreaReadOnly,
        pointer: &Pointer,
    ) {
        std::fs::write(
            fixture.work_tree().join(".gitattributes"),
            "*.bin filter=crab diff=crab merge=crab -text\n",
        )
        .expect("write attributes");
        std::fs::write(fixture.work_tree().join("model.bin"), pointer.serialize())
            .expect("write pointer");
        run_git_without_crab_filter(fixture.work_tree(), &["add", ".gitattributes", "model.bin"]);
        run_git(fixture.work_tree(), &["commit", "-m", "pointer"]);

        let router = StoreLayout::new(store.clone(), TEST_REPLICA_PREFIX.to_owned());
        crate::cmd::init::initialize_remote_repository_store(&store, &router, "refs/heads/main")
            .await
            .expect("initialize canonical command-path repository");

        let result = run_push_batch(
            &[PushSpec {
                force: false,
                src: "refs/heads/main".to_owned(),
                dst: "refs/heads/main".to_owned(),
            }],
            &PushConfig::default(),
            Some(store.clone()),
            None,
            Some(Arc::new(staging)),
            router,
            None,
            CancellationToken::new(),
            None,
        )
        .await;

        assert!(
            matches!(
                result.outcomes.get("refs/heads/main"),
                Some(RefPushOutcome::Ok)
            ),
            "push must publish reconstructible metadata; outcomes: {:?}",
            result.outcomes
        );

        let global_objects = store
            .list_prefix(&object_store::path::Path::from(".crab"))
            .await
            .expect("list published global objects");
        let global_keys = global_objects
            .iter()
            .map(|meta| meta.location.as_ref().to_owned())
            .collect::<Vec<_>>();
        assert!(
            global_keys
                .iter()
                .any(|key| key.starts_with(".crab/xorbs/")),
            "push must publish xorb objects before metadata references them; published global keys: {global_keys:?}"
        );
    }

    fn setup_tracked_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".gitattributes"),
            "*.bin filter=crab diff=crab merge=crab -text\n",
        )
        .unwrap();
        dir
    }

    fn default_args() -> HydrateArgs {
        HydrateArgs {
            patterns: Vec::new(),
            include: Vec::new(),
            exclude: Vec::new(),
            all: false,
            mode: OutputMode::Text,
            manifest: None,
            manifest_ref: None,
            profile: None,
            ignore_sparse: false,
            recover_from: None,
        }
    }

    #[tokio::test]
    async fn hydration_owner_cancellation_precedes_root_and_remote_access() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("absent");
        let mut config = Config::default();
        config.remote_url = Some("invalid-remote".to_owned());
        let cancel = CancellationToken::new();
        cancel.cancel();

        let result = run_hydrate(
            &root,
            &default_args(),
            &config,
            &crate::cmd::hydrate_restore::RestoreFlags::default(),
            &cancel,
        )
        .await;

        assert!(matches!(result, Err(error::CrabError::Cancelled)));
    }

    #[tokio::test]
    async fn hydration_owner_rejects_invalid_remote_before_local_recovery() {
        let dir = tempfile::tempdir().unwrap();
        for remote in ["", " ", "not-a-crab-url"] {
            let mut config = Config::default();
            config.remote_url = Some(remote.to_owned());
            let result = run_hydrate(
                &dir.path().join("absent"),
                &default_args(),
                &config,
                &crate::cmd::hydrate_restore::RestoreFlags::default(),
                &CancellationToken::new(),
            )
            .await;
            assert!(matches!(
                result,
                Err(error::CrabError::Configuration { .. })
            ));
        }
    }

    #[tokio::test]
    async fn hydration_owner_uses_explicit_root_for_unpublished_staging() {
        let fixture = GitFixture::new();
        let root = fixture.work_tree();
        let content = command_path_content();
        let (chunks, file_hash) = chunk_and_hash(&content);
        let pointer = Pointer {
            file_hash,
            size: content.len() as u64,
            shard_hint: None,
        };
        let staging = stage_command_path_file(root, file_hash, &chunks, pointer.size).await;
        drop(staging);
        let path = root.join("model.bin");
        std::fs::write(&path, pointer.serialize()).unwrap();
        std::fs::write(root.join(".gitattributes"), "*.bin filter=crab -text\n").unwrap();
        run_git(root, &["add", "model.bin", ".gitattributes"]);
        let args = HydrateArgs {
            all: true,
            ..default_args()
        };

        run_hydrate(
            root,
            &args,
            &Config::default(),
            &crate::cmd::hydrate_restore::RestoreFlags::default(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(std::fs::read(path).unwrap(), content);
    }

    #[derive(Default)]
    struct RecordingHydrator {
        paths: std::sync::Mutex<Vec<PathBuf>>,
    }

    impl RecordingHydrator {
        fn relative_paths(&self, root: &Path) -> Vec<String> {
            let mut paths = self
                .paths
                .lock()
                .unwrap()
                .iter()
                .map(|path| {
                    path.strip_prefix(root)
                        .unwrap_or(path)
                        .to_string_lossy()
                        .replace('\\', "/")
                })
                .collect::<Vec<_>>();
            paths.sort();
            paths
        }
    }

    impl Hydrator for RecordingHydrator {
        fn hydrate_batch<'a>(
            &'a self,
            items: &'a [(PathBuf, Pointer)],
            cancel: &'a CancellationToken,
            _progress: Option<&'a Arc<HydrateProgress>>,
        ) -> Pin<Box<dyn Future<Output = Result<HydrateSummary>> + Send + 'a>> {
            Box::pin(async move {
                error::check_cancelled(cancel)?;
                let mut paths = self.paths.lock().unwrap();
                paths.extend(items.iter().map(|(path, _)| path.clone()));
                Ok(HydrateSummary {
                    hydrated: items.len() as u64,
                    ..HydrateSummary::default()
                })
            })
        }
    }

    struct ContentHydrator {
        content: Vec<u8>,
    }

    impl Hydrator for ContentHydrator {
        fn hydrate_batch<'a>(
            &'a self,
            items: &'a [(PathBuf, Pointer)],
            cancel: &'a CancellationToken,
            _progress: Option<&'a Arc<HydrateProgress>>,
        ) -> Pin<Box<dyn Future<Output = Result<HydrateSummary>> + Send + 'a>> {
            Box::pin(async move {
                error::check_cancelled(cancel)?;
                let mut summary = HydrateSummary::default();
                for (path, pointer) in items {
                    let index_stat = atomic_write_with_progress(path, &self.content, None)?;
                    let write = VerifiedWrite {
                        bytes: self.content.len() as u64,
                        index_stat,
                    };
                    summary
                        .verified_paths
                        .push(verified_path(path, pointer, write));
                }
                summary.hydrated = items.len() as u64;
                summary.bytes_written = self.content.len() as u64 * items.len() as u64;
                Ok(summary)
            })
        }
    }

    struct FailingHydrator;

    impl Hydrator for FailingHydrator {
        fn hydrate_batch<'a>(
            &'a self,
            items: &'a [(PathBuf, Pointer)],
            cancel: &'a CancellationToken,
            _progress: Option<&'a Arc<HydrateProgress>>,
        ) -> Pin<Box<dyn Future<Output = Result<HydrateSummary>> + Send + 'a>> {
            Box::pin(async move {
                error::check_cancelled(cancel)?;
                Ok(HydrateSummary {
                    failed: items.len() as u64,
                    ..HydrateSummary::default()
                })
            })
        }
    }

    fn write_pending_worktree_policy(
        root: &Path,
        mode: crate::git::worktree_hydration::WorktreeHydrationMode,
        selector: crate::git::worktree_hydration::WorktreeHydrationSelector,
    ) {
        let ctx = crate::git::worktree::WorktreeContext::resolve_from_path(root).unwrap();
        crate::git::worktree_hydration::WorktreeHydrationPolicyFile {
            version: 1,
            source: crate::git::worktree_hydration::WorktreeHydrationPolicySource::Explicit,
            status: crate::git::worktree_hydration::WorktreeHydrationPolicyStatus::Pending,
            mode,
            checkout_suppressed: true,
            prefetch: false,
            selector,
        }
        .write_for_context(&ctx)
        .unwrap();
    }

    fn read_worktree_policy(
        root: &Path,
    ) -> crate::git::worktree_hydration::WorktreeHydrationPolicyFile {
        let ctx = crate::git::worktree::WorktreeContext::resolve_from_path(root).unwrap();
        crate::git::worktree_hydration::WorktreeHydrationPolicyFile::read_for_context(&ctx)
            .unwrap()
            .unwrap()
    }

    // --- resolve_patterns tests ---

    #[test]
    fn resolve_all_matches_everything() {
        let args = HydrateArgs {
            all: true,
            ..default_args()
        };
        let config = Config::default();
        let filter = resolve_patterns(&args, &config).unwrap().unwrap();
        assert!(filter.matches("root.bin"));
        assert!(filter.matches("any/path/file.bin"));
    }

    #[test]
    fn resolve_positional_globs() {
        let args = HydrateArgs {
            patterns: vec!["*.bin".to_owned()],
            ..default_args()
        };
        let config = Config::default();
        let filter = resolve_patterns(&args, &config).unwrap().unwrap();
        assert!(filter.matches("model.bin"));
        assert!(!filter.matches("model.txt"));
    }

    #[test]
    fn resolve_include_exclude_flags() {
        let args = HydrateArgs {
            include: vec!["**/*.bin".to_owned()],
            exclude: vec!["**/archive/**".to_owned()],
            ..default_args()
        };
        let config = Config::default();
        let filter = resolve_patterns(&args, &config).unwrap().unwrap();
        assert!(filter.matches("models/v1/model.bin"));
        assert!(!filter.matches("models/archive/old.bin"));
    }

    #[test]
    fn resolve_falls_back_to_config() {
        let args = default_args();
        let mut config = Config::default();
        config.hydrate.include = vec!["*.safetensors".to_owned()];
        let filter = resolve_patterns(&args, &config).unwrap().unwrap();
        assert!(filter.matches("weights.safetensors"));
        assert!(!filter.matches("weights.bin"));
    }

    #[test]
    fn resolve_returns_none_when_nothing_specified() {
        let args = default_args();
        let config = Config::default();
        assert!(resolve_patterns(&args, &config).unwrap().is_none());
    }

    #[test]
    fn all_ref_prefetch_inventory_filters_paths_and_deduplicates_file_hashes() {
        let fixture = GitFixture::new();
        let root = fixture.work_tree();
        let main_pointer = sample_pointer(10);
        std::fs::write(root.join("main.bin"), main_pointer.serialize()).unwrap();
        run_git(root, &["add", "main.bin"]);
        run_git(root, &["commit", "-m", "main pointer"]);
        run_git(root, &["checkout", "-b", "feature"]);
        let mut feature_pointer = sample_pointer(20);
        feature_pointer.file_hash[0] ^= 0xff;
        std::fs::write(root.join("feature.bin"), feature_pointer.serialize()).unwrap();
        std::fs::write(root.join("large.dat"), vec![0u8; MAX_POINTER_SIZE + 1]).unwrap();
        run_git(root, &["add", "feature.bin", "large.dat"]);
        run_git(root, &["commit", "-m", "feature pointer"]);

        let all = resolve_all_ref_pointer_prefetch_candidates(
            root,
            &["*.bin".to_owned()],
            &[],
            &CancellationToken::new(),
        )
        .unwrap();
        let main_only = resolve_all_ref_pointer_prefetch_candidates(
            root,
            &["*.bin".to_owned()],
            &["feature.bin".to_owned()],
            &CancellationToken::new(),
        )
        .unwrap();

        assert_eq!(all.len(), 2);
        assert_eq!(main_only.len(), 1);
        assert_eq!(main_only[0].1.file_hash, main_pointer.file_hash);
    }

    #[test]
    fn git_small_blobs_drains_large_input_and_output_concurrently() {
        let fixture = GitFixture::new();
        let root = fixture.work_tree();
        let paths = (0..2_500)
            .map(|index| {
                let path = format!("blob-{index:04}.txt");
                std::fs::write(root.join(&path), format!("blob {index}\n")).unwrap();
                path
            })
            .collect::<Vec<_>>();
        let output = Command::new("git")
            .args(["hash-object", "-w"])
            .args(&paths)
            .current_dir(root)
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_COMMON_DIR")
            .output()
            .unwrap();
        assert!(output.status.success());
        let stdout = String::from_utf8(output.stdout).unwrap();
        let oids = stdout.lines().map(str::to_owned).collect::<Vec<_>>();
        assert_eq!(oids.len(), paths.len());

        let blobs = git_small_blobs(root, oids.iter().map(String::as_str)).unwrap();

        assert_eq!(blobs.len(), paths.len());
        assert!(blobs.values().any(|blob| blob == b"blob 2499\n"));
    }

    #[tokio::test]
    async fn hydrate_no_args_uses_pending_worktree_full_policy() {
        let fixture = GitFixture::new();
        let root = fixture.work_tree();
        std::fs::write(
            root.join(".gitattributes"),
            "*.bin filter=crab diff=crab merge=crab -text\n",
        )
        .unwrap();
        let ptr = sample_pointer(4096);
        std::fs::write(root.join("a.bin"), ptr.serialize()).unwrap();
        std::fs::write(root.join("b.bin"), ptr.serialize()).unwrap();
        write_pending_worktree_policy(
            root,
            crate::git::worktree_hydration::WorktreeHydrationMode::Full,
            crate::git::worktree_hydration::WorktreeHydrationSelector::All,
        );

        let args = default_args();
        let config = Config::default();
        let cancel = CancellationToken::new();
        let hydrator = RecordingHydrator::default();
        run_hydrate_in(root, &args, &config, &hydrator, &cancel)
            .await
            .unwrap();

        assert_eq!(hydrator.relative_paths(root), vec!["a.bin", "b.bin"]);
        let policy = read_worktree_policy(root);
        assert_eq!(
            policy.status,
            crate::git::worktree_hydration::WorktreeHydrationPolicyStatus::Applied
        );
        assert!(!policy.checkout_suppressed);
    }

    #[tokio::test]
    async fn hydrate_no_args_uses_pending_worktree_pattern_policy() {
        let fixture = GitFixture::new();
        let root = fixture.work_tree();
        std::fs::write(
            root.join(".gitattributes"),
            "*.bin filter=crab diff=crab merge=crab -text\n",
        )
        .unwrap();
        let ptr = sample_pointer(4096);
        std::fs::write(root.join("model.bin"), ptr.serialize()).unwrap();
        std::fs::write(root.join("skip.bin"), ptr.serialize()).unwrap();
        write_pending_worktree_policy(
            root,
            crate::git::worktree_hydration::WorktreeHydrationMode::Selective,
            crate::git::worktree_hydration::WorktreeHydrationSelector::Patterns {
                include: vec!["*.bin".to_owned()],
                exclude: vec!["skip.bin".to_owned()],
            },
        );

        let args = default_args();
        let config = Config::default();
        let cancel = CancellationToken::new();
        let hydrator = RecordingHydrator::default();
        run_hydrate_in(root, &args, &config, &hydrator, &cancel)
            .await
            .unwrap();

        assert_eq!(hydrator.relative_paths(root), vec!["model.bin"]);
    }

    #[tokio::test]
    async fn hydrate_failed_pending_worktree_policy_stays_retryable() {
        let fixture = GitFixture::new();
        let root = fixture.work_tree();
        std::fs::write(
            root.join(".gitattributes"),
            "*.bin filter=crab diff=crab merge=crab -text\n",
        )
        .unwrap();
        let ptr = sample_pointer(4096);
        std::fs::write(root.join("a.bin"), ptr.serialize()).unwrap();
        write_pending_worktree_policy(
            root,
            crate::git::worktree_hydration::WorktreeHydrationMode::Full,
            crate::git::worktree_hydration::WorktreeHydrationSelector::All,
        );

        let args = default_args();
        let config = Config::default();
        let cancel = CancellationToken::new();
        let hydrator = FailingHydrator;
        let err = run_hydrate_in(root, &args, &config, &hydrator, &cancel)
            .await
            .expect_err("failed hydrate batch must return an error");
        assert!(
            err.to_string().contains("hydrate failed for 1 file(s)"),
            "unexpected error: {err}"
        );

        let policy = read_worktree_policy(root);
        assert_eq!(
            policy.status,
            crate::git::worktree_hydration::WorktreeHydrationPolicyStatus::Pending
        );
        assert!(policy.checkout_suppressed);
    }

    #[tokio::test]
    async fn hydrate_explicit_selector_overrides_pending_worktree_policy() {
        let fixture = GitFixture::new();
        let root = fixture.work_tree();
        std::fs::write(
            root.join(".gitattributes"),
            "*.bin filter=crab diff=crab merge=crab -text\n",
        )
        .unwrap();
        let ptr = sample_pointer(4096);
        std::fs::write(root.join("a.bin"), ptr.serialize()).unwrap();
        std::fs::write(root.join("b.bin"), ptr.serialize()).unwrap();
        write_pending_worktree_policy(
            root,
            crate::git::worktree_hydration::WorktreeHydrationMode::Full,
            crate::git::worktree_hydration::WorktreeHydrationSelector::All,
        );

        let args = HydrateArgs {
            patterns: vec!["b.bin".to_owned()],
            ..default_args()
        };
        let config = Config::default();
        let cancel = CancellationToken::new();
        let hydrator = RecordingHydrator::default();
        run_hydrate_in(root, &args, &config, &hydrator, &cancel)
            .await
            .unwrap();

        assert_eq!(hydrator.relative_paths(root), vec!["b.bin"]);
        let policy = read_worktree_policy(root);
        assert_eq!(
            policy.status,
            crate::git::worktree_hydration::WorktreeHydrationPolicyStatus::Pending
        );
        assert!(policy.checkout_suppressed);
    }

    #[tokio::test]
    async fn hydrate_no_checkout_worktree_materializes_only_selected_index_pointers() {
        let fixture = GitFixture::new();
        let root = fixture.work_tree();
        std::fs::write(
            root.join(".gitattributes"),
            "*.bin filter=crab diff=crab merge=crab -text\n",
        )
        .unwrap();
        std::fs::create_dir(root.join("models")).unwrap();
        std::fs::create_dir(root.join("docs")).unwrap();
        let ptr = sample_pointer(4096);
        std::fs::write(root.join("models/model.bin"), ptr.serialize()).unwrap();
        std::fs::write(root.join("docs/readme.txt"), b"not a pointer\n").unwrap();
        run_git_without_crab_filter(
            root,
            &[
                "add",
                ".gitattributes",
                "models/model.bin",
                "docs/readme.txt",
            ],
        );
        run_git(root, &["commit", "-m", "add pointer and normal file"]);

        let linked_parent = tempfile::tempdir().unwrap();
        let linked = linked_parent.path().join("linked-no-checkout");
        let linked_arg = linked.to_string_lossy().into_owned();
        run_git(
            root,
            &[
                "worktree",
                "add",
                "--detach",
                "--no-checkout",
                linked_arg.as_str(),
                "HEAD",
            ],
        );
        assert!(!linked.join("models/model.bin").exists());
        assert!(!linked.join("docs/readme.txt").exists());

        let args = HydrateArgs {
            all: true,
            ..default_args()
        };
        let config = Config::default();
        let cancel = CancellationToken::new();
        let hydrator = StubHydrator;
        run_hydrate_in(&linked, &args, &config, &hydrator, &cancel)
            .await
            .unwrap();

        assert_eq!(
            std::fs::read(linked.join("models/model.bin")).unwrap(),
            ptr.serialize()
        );
        assert!(!linked.join("docs/readme.txt").exists());
        assert!(!linked.join(".gitattributes").exists());
    }

    #[tokio::test]
    async fn linked_worktree_hydrate_materializes_content_and_updates_own_cache() {
        let fixture = GitFixture::new();
        let root = fixture.work_tree();
        std::fs::write(
            root.join(".gitattributes"),
            "*.bin filter=crab diff=crab merge=crab -text\n",
        )
        .unwrap();
        std::fs::create_dir(root.join("models")).unwrap();
        let content = b"linked worktree hydrated bytes\n".repeat(32);
        let ptr = sample_pointer(content.len() as u64);
        std::fs::write(root.join("models/model.bin"), ptr.serialize()).unwrap();
        run_git_without_crab_filter(root, &["add", ".gitattributes", "models/model.bin"]);
        run_git(root, &["commit", "-m", "add pointer"]);

        let linked_parent = tempfile::tempdir().unwrap();
        let linked = linked_parent.path().join("linked-hydrate");
        let linked_arg = linked.to_string_lossy().into_owned();
        run_git(
            root,
            &["worktree", "add", "--detach", linked_arg.as_str(), "HEAD"],
        );
        assert!(Pointer::parse(&std::fs::read(linked.join("models/model.bin")).unwrap()).is_ok());

        let args = HydrateArgs {
            all: true,
            ..default_args()
        };
        let config = Config::default();
        let cancel = CancellationToken::new();
        let hydrator = ContentHydrator {
            content: content.clone(),
        };
        run_hydrate_in(&linked, &args, &config, &hydrator, &cancel)
            .await
            .unwrap();

        assert_eq!(
            std::fs::read(linked.join("models/model.bin")).unwrap(),
            content
        );
        let linked_cache_path =
            crate::cache::hydrated_pointer::cache_path_for_worktree_root(&linked).unwrap();
        let main_cache_path =
            crate::cache::hydrated_pointer::cache_path_for_worktree_root(root).unwrap();
        assert_ne!(linked_cache_path, main_cache_path);
        let cache = crate::cache::HydratedPointerCache::load_sync(&linked_cache_path);
        assert_eq!(cache.len(), 1);
        assert!(cache.get("models/model.bin").is_some());
    }

    #[tokio::test]
    async fn linked_worktree_hydrate_uses_verified_sibling_cow_or_normal_fallback() {
        let fixture = GitFixture::new();
        let root = fixture.work_tree();
        std::fs::write(
            root.join(".gitattributes"),
            "*.bin filter=crab diff=crab merge=crab -text\n",
        )
        .unwrap();
        std::fs::create_dir(root.join("models")).unwrap();
        let content = b"production linked worktree content".repeat(4096);
        let pointer = pointer_for_content(&content);
        let source = root.join("models/model.bin");
        std::fs::write(&source, pointer.serialize()).unwrap();
        run_git_without_crab_filter(root, &["add", ".gitattributes", "models/model.bin"]);
        run_git(root, &["commit", "-m", "add pointer"]);

        let linked_parent = tempfile::tempdir().unwrap();
        let linked = linked_parent.path().join("linked-cow");
        let linked_arg = linked.to_string_lossy().into_owned();
        run_git(
            root,
            &["worktree", "add", "--detach", linked_arg.as_str(), "HEAD"],
        );
        let destination = linked.join("models/model.bin");
        assert_eq!(std::fs::read(&destination).unwrap(), pointer.serialize());

        std::fs::write(&source, &content).unwrap();
        let source_cache =
            crate::cache::hydrated_pointer::cache_path_for_worktree_root(root).unwrap();
        let source_entry =
            crate::cache::hydrated_pointer::entry_for_path(&source, &pointer.serialize()).unwrap();
        crate::cache::HydratedPointerCache::update_on_disk(
            &source_cache,
            [("models/model.bin".to_owned(), source_entry)],
        )
        .unwrap();

        let hydrator = RecordingHydrator::default();
        run_hydrate_in(
            &linked,
            &HydrateArgs {
                all: true,
                ..default_args()
            },
            &Config::default(),
            &hydrator,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        let remote_paths = hydrator.relative_paths(&linked);
        if remote_paths.is_empty() {
            assert_eq!(std::fs::read(&destination).unwrap(), content);
            std::fs::write(&destination, b"independent linked edit").unwrap();
            assert_eq!(std::fs::read(&source).unwrap(), content);
            let linked_cache =
                crate::cache::hydrated_pointer::cache_path_for_worktree_root(&linked).unwrap();
            assert!(
                crate::cache::HydratedPointerCache::load_sync(&linked_cache)
                    .get("models/model.bin")
                    .is_some()
            );
        } else {
            assert_eq!(remote_paths, [PathBuf::from("models/model.bin")]);
            assert_eq!(std::fs::read(&destination).unwrap(), pointer.serialize());
        }
    }

    // --- walk and hydrate tests ---

    #[tokio::test]
    async fn hydrate_collects_matching_pointer_files() {
        let dir = setup_tracked_dir();
        let ptr = sample_pointer(4096);
        std::fs::write(dir.path().join("model.bin"), ptr.serialize()).unwrap();
        // Non-pointer file should be skipped.
        std::fs::write(dir.path().join("data.bin"), vec![0xAB; 8192]).unwrap();

        let args = HydrateArgs {
            patterns: vec!["*.bin".to_owned()],
            ..default_args()
        };
        let config = Config::default();
        let cancel = CancellationToken::new();
        let hydrator = StubHydrator;
        run_hydrate_in(dir.path(), &args, &config, &hydrator, &cancel)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn hydrate_skips_non_matching_patterns() {
        let dir = setup_tracked_dir();
        let ptr = sample_pointer(4096);
        std::fs::write(dir.path().join("model.bin"), ptr.serialize()).unwrap();

        let args = HydrateArgs {
            patterns: vec!["*.txt".to_owned()],
            ..default_args()
        };
        let config = Config::default();
        let cancel = CancellationToken::new();
        let hydrator = StubHydrator;
        // Should report no matching files.
        run_hydrate_in(dir.path(), &args, &config, &hydrator, &cancel)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn hydrate_all_collects_all_pointers() {
        let dir = setup_tracked_dir();
        let ptr = sample_pointer(4096);
        std::fs::write(dir.path().join("a.bin"), ptr.serialize()).unwrap();
        std::fs::write(dir.path().join("b.bin"), ptr.serialize()).unwrap();

        let args = HydrateArgs {
            all: true,
            ..default_args()
        };
        let config = Config::default();
        let cancel = CancellationToken::new();
        let hydrator = StubHydrator;
        run_hydrate_in(dir.path(), &args, &config, &hydrator, &cancel)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn hydrate_no_args_prints_help() {
        let dir = setup_tracked_dir();
        let args = default_args();
        let config = Config::default();
        let cancel = CancellationToken::new();
        let hydrator = StubHydrator;
        // Should print help and return Ok.
        run_hydrate_in(dir.path(), &args, &config, &hydrator, &cancel)
            .await
            .unwrap();
    }

    #[test]
    fn hydrate_skips_hidden_directories() {
        let dir = setup_tracked_dir();
        let hidden = dir.path().join(".hidden");
        std::fs::create_dir(&hidden).unwrap();
        let ptr = sample_pointer(4096);
        std::fs::write(hidden.join("secret.bin"), ptr.serialize()).unwrap();

        let args = HydrateArgs {
            all: true,
            ..default_args()
        };
        let config = Config::default();
        let cancel = CancellationToken::new();

        let filter = resolve_patterns(&args, &config).unwrap().unwrap();
        let tracked = TrackedClassifier::open(dir.path()).unwrap();
        let mut out = Vec::new();
        walk_and_parse_pointers(dir.path(), dir.path(), &tracked, &filter, &cancel, &mut out)
            .unwrap();
        assert!(out.is_empty(), "hidden dir files should be skipped");
    }

    #[test]
    fn hydrate_walks_subdirectories() {
        let dir = setup_tracked_dir();
        let sub = dir.path().join("models");
        std::fs::create_dir(&sub).unwrap();
        let ptr = sample_pointer(4096);
        std::fs::write(sub.join("weights.bin"), ptr.serialize()).unwrap();

        let args = HydrateArgs {
            all: true,
            ..default_args()
        };
        let config = Config::default();
        let cancel = CancellationToken::new();

        let filter = resolve_patterns(&args, &config).unwrap().unwrap();
        let tracked = TrackedClassifier::open(dir.path()).unwrap();
        let mut out = Vec::new();
        walk_and_parse_pointers(dir.path(), dir.path(), &tracked, &filter, &cancel, &mut out)
            .unwrap();
        assert_eq!(out.len(), 1);
        assert!(out[0].0.to_string_lossy().contains("weights.bin"));
    }

    #[test]
    fn hydrate_respects_cancellation() {
        let dir = setup_tracked_dir();
        let ptr = sample_pointer(4096);
        std::fs::write(dir.path().join("model.bin"), ptr.serialize()).unwrap();

        let cancel = CancellationToken::new();
        cancel.cancel();

        let filter = build_filter(&["**/*".to_owned()], &[]).unwrap();
        let tracked = TrackedClassifier::open(dir.path()).unwrap();
        let mut out = Vec::new();
        let result =
            walk_and_parse_pointers(dir.path(), dir.path(), &tracked, &filter, &cancel, &mut out);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn hydrate_no_gitattributes_reports_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let args = HydrateArgs {
            all: true,
            ..default_args()
        };
        let config = Config::default();
        let cancel = CancellationToken::new();
        let hydrator = StubHydrator;
        run_hydrate_in(dir.path(), &args, &config, &hydrator, &cancel)
            .await
            .unwrap();
    }

    /// Regression: after `git pull` lands a pointer file whose extension
    /// is not yet in `.gitattributes`, `crab hydrate` used to silently
    /// report "No pointer files match" because the walker filters by
    /// tracked globs. The autotrack rescan at the top of `run_hydrate_in`
    /// fixes that by picking up the new extension before the walk.
    #[tokio::test]
    async fn hydrate_autotracks_new_pointer_extensions_after_pull() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // Simulate state after `crab clone` + later `git pull`:
        // - `.gitattributes` already tracks `*.zip` (from a prior clone),
        // - `git pull` just dropped a `.dmg` pointer into the tree.
        std::fs::write(
            root.join(".gitattributes"),
            "*.zip filter=crab diff=crab merge=crab -text\n",
        )
        .unwrap();
        let ptr = sample_pointer(1_024);
        std::fs::write(root.join("big.dmg"), ptr.serialize()).unwrap();

        let args = HydrateArgs {
            all: true,
            ..default_args()
        };
        let config = Config::default();
        let cancel = CancellationToken::new();
        let hydrator = StubHydrator;

        run_hydrate_in(root, &args, &config, &hydrator, &cancel)
            .await
            .unwrap();

        // The rescan must have appended a `*.dmg` rule so a subsequent
        // hydrate (or the same one, walk-wise) can pick the file up.
        let ga = std::fs::read_to_string(root.join(".gitattributes")).unwrap();
        assert!(ga.contains("*.zip filter=crab"), "existing rule preserved");
        assert!(
            ga.contains("*.dmg filter=crab"),
            "new extension auto-tracked after pull",
        );
    }

    #[test]
    fn hydrate_exclude_only_uses_wildcard_include() {
        let args = HydrateArgs {
            exclude: vec!["**/archive/**".to_owned()],
            ..default_args()
        };
        let config = Config::default();
        let filter = resolve_patterns(&args, &config).unwrap().unwrap();
        assert!(filter.matches("models/v1/model.bin"));
        assert!(!filter.matches("models/archive/old.bin"));
    }

    // --- Hydrator and atomic write tests ---

    #[tokio::test]
    async fn stub_hydrator_writes_all_files() {
        let dir = tempfile::tempdir().unwrap();
        let ptr = sample_pointer(4096);
        let path_a = dir.path().join("a.bin");
        let path_b = dir.path().join("b.bin");
        std::fs::write(&path_a, ptr.serialize()).unwrap();
        std::fs::write(&path_b, ptr.serialize()).unwrap();

        let items = vec![(path_a.clone(), ptr.clone()), (path_b.clone(), ptr.clone())];
        let cancel = CancellationToken::new();
        let hydrator = StubHydrator;
        let summary = hydrator.hydrate_batch(&items, &cancel, None).await.unwrap();

        assert_eq!(summary.hydrated, 2);
        assert_eq!(summary.failed, 0);
        assert!(summary.bytes_written > 0);
    }

    #[tokio::test]
    async fn stub_hydrator_respects_cancellation() {
        let dir = tempfile::tempdir().unwrap();
        let ptr = sample_pointer(4096);
        let path = dir.path().join("a.bin");
        std::fs::write(&path, ptr.serialize()).unwrap();

        let items = vec![(path, ptr)];
        let cancel = CancellationToken::new();
        cancel.cancel();

        let hydrator = StubHydrator;
        let result = hydrator.hydrate_batch(&items, &cancel, None).await;
        assert!(result.is_err());
    }

    #[test]
    fn atomic_write_creates_file() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("output.bin");
        let content = b"hello world";
        atomic_write(&dest, content).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), content);
    }

    #[test]
    fn atomic_write_overwrites_existing() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("output.bin");
        std::fs::write(&dest, b"old content").unwrap();
        let content = b"new content";
        atomic_write(&dest, content).unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), content);
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_preserves_executable_mode() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("executable.bin");
        std::fs::write(&dest, b"pointer content").unwrap();
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755)).unwrap();

        atomic_write(&dest, b"hydrated content").unwrap();

        assert_eq!(std::fs::metadata(&dest).unwrap().mode() & 0o777, 0o755);
    }

    fn pointer_for_content(content: &[u8]) -> Pointer {
        Pointer {
            file_hash: *blake3::hash(content).as_bytes(),
            size: content.len() as u64,
            shard_hint: None,
        }
    }

    #[test]
    fn recover_from_hashes_the_exact_published_copy() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.bin");
        let dest = dir.path().join("dest.bin");
        let content = b"verified recovery content".repeat(4096);
        let pointer = pointer_for_content(&content);
        std::fs::write(&source, &content).unwrap();
        std::fs::write(&dest, pointer.serialize()).unwrap();

        let RecoverOutcome::Recovered { write } =
            try_recover_one(&source, &dest, &pointer).unwrap()
        else {
            panic!("matching recovery source must be published");
        };

        assert_eq!(write.bytes, pointer.size);
        assert_eq!(
            crate::cmd::stream_stage::VerifiedIndexStat::from_path_no_follow(&dest),
            Some(write.index_stat)
        );
        assert_eq!(std::fs::read(dest).unwrap(), content);
    }

    #[test]
    fn recover_from_mismatch_leaves_pointer_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.bin");
        let dest = dir.path().join("dest.bin");
        let expected = b"expected recovery bytes";
        let pointer = pointer_for_content(expected);
        let pointer_bytes = pointer.serialize();
        let mut incorrect = expected.to_vec();
        incorrect[0] ^= 0xff;
        std::fs::write(&source, incorrect).unwrap();
        std::fs::write(&dest, &pointer_bytes).unwrap();

        assert!(matches!(
            try_recover_one(&source, &dest, &pointer).unwrap(),
            RecoverOutcome::HashMismatch
        ));
        assert_eq!(std::fs::read(dest).unwrap(), pointer_bytes);
    }

    #[cfg(unix)]
    #[test]
    fn cow_clone_candidate_verifies_content_preserves_mode_and_is_independent() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.bin");
        let dest = dir.path().join("dest.bin");
        let content = b"verified sibling worktree content".repeat(4096);
        let pointer = pointer_for_content(&content);
        std::fs::write(&source, &content).unwrap();
        std::fs::write(&dest, pointer.serialize()).unwrap();
        std::fs::set_permissions(&dest, std::fs::Permissions::from_mode(0o755)).unwrap();

        let outcome =
            try_cow_clone_candidate(&source, &dest, &pointer, &CancellationToken::new()).unwrap();
        if outcome.is_none() {
            eprintln!("SKIP: filesystem CoW unavailable");
            return;
        }

        assert_eq!(std::fs::read(&dest).unwrap(), content);
        assert_eq!(std::fs::metadata(&dest).unwrap().mode() & 0o777, 0o755);
        std::fs::write(&dest, b"independent edit").unwrap();
        assert_eq!(std::fs::read(&source).unwrap(), content);
    }

    #[test]
    fn cow_clone_candidate_mismatch_and_cancellation_leave_pointer_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.bin");
        let dest = dir.path().join("dest.bin");
        let expected = b"expected bytes";
        let pointer = pointer_for_content(expected);
        let pointer_bytes = pointer.serialize();
        std::fs::write(&source, b"wrong content!").unwrap();
        std::fs::write(&dest, &pointer_bytes).unwrap();

        assert_eq!(
            try_cow_clone_candidate(&source, &dest, &pointer, &CancellationToken::new(),).unwrap(),
            None
        );
        assert_eq!(std::fs::read(&dest).unwrap(), pointer_bytes);
        assert!(
            !std::fs::read_dir(dir.path())
                .unwrap()
                .filter_map(std::result::Result::ok)
                .any(|entry| entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".crab-cow-"))
        );

        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(matches!(
            try_cow_clone_candidate(&source, &dest, &pointer, &cancel),
            Err(error::CrabError::Cancelled)
        ));
        assert_eq!(std::fs::read(&dest).unwrap(), pointer.serialize());
    }

    #[test]
    fn concurrent_cow_clone_publication_never_exposes_partial_content() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.bin");
        let dest = dir.path().join("dest.bin");
        let content = b"concurrent verified clone".repeat(8192);
        let pointer = pointer_for_content(&content);
        std::fs::write(&source, &content).unwrap();
        std::fs::write(&dest, pointer.serialize()).unwrap();

        let barrier = Arc::new(std::sync::Barrier::new(3));
        let mut workers = Vec::new();
        for _ in 0..2 {
            let barrier = Arc::clone(&barrier);
            let source = source.clone();
            let dest = dest.clone();
            let pointer = pointer.clone();
            workers.push(std::thread::spawn(move || {
                barrier.wait();
                try_cow_clone_candidate(&source, &dest, &pointer, &CancellationToken::new())
                    .unwrap()
            }));
        }
        barrier.wait();
        let outcomes = workers
            .into_iter()
            .map(|worker| worker.join().unwrap())
            .collect::<Vec<_>>();

        if outcomes.iter().any(Option::is_some) {
            assert_eq!(std::fs::read(&dest).unwrap(), content);
        } else {
            assert_eq!(std::fs::read(&dest).unwrap(), pointer.serialize());
        }
        assert!(
            !std::fs::read_dir(dir.path())
                .unwrap()
                .filter_map(std::result::Result::ok)
                .any(|entry| entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".crab-cow-"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn cached_candidate_rejects_unsafe_and_non_regular_paths() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("models")).unwrap();
        std::fs::write(dir.path().join("models/model.bin"), b"content").unwrap();
        symlink(
            dir.path().join("models/model.bin"),
            dir.path().join("models/link.bin"),
        )
        .unwrap();
        let canonical_root = std::fs::canonicalize(dir.path()).unwrap();

        assert!(safe_cached_candidate(dir.path(), &canonical_root, "models/model.bin").is_some());
        assert!(safe_cached_candidate(dir.path(), &canonical_root, "../model.bin").is_none());
        assert!(safe_cached_candidate(dir.path(), &canonical_root, "/model.bin").is_none());
        assert!(safe_cached_candidate(dir.path(), &canonical_root, "models/link.bin").is_none());
        assert!(safe_cached_candidate(dir.path(), &canonical_root, "models").is_none());
    }

    #[test]
    fn sibling_candidate_index_uses_valid_per_worktree_cache() {
        let fixture = GitFixture::new();
        let root = fixture.work_tree();
        std::fs::write(
            root.join(".gitattributes"),
            "*.bin filter=crab diff=crab merge=crab -text\n",
        )
        .unwrap();
        std::fs::create_dir(root.join("models")).unwrap();
        let content = b"candidate content from main worktree".repeat(128);
        let pointer = pointer_for_content(&content);
        let source = root.join("models/model.bin");
        std::fs::write(&source, pointer.serialize()).unwrap();
        run_git_without_crab_filter(root, &["add", ".gitattributes", "models/model.bin"]);
        run_git(root, &["commit", "-m", "add pointer"]);

        let linked_parent = tempfile::tempdir().unwrap();
        let linked = linked_parent.path().join("linked");
        let linked_arg = linked.to_string_lossy().into_owned();
        run_git(
            root,
            &["worktree", "add", "--detach", linked_arg.as_str(), "HEAD"],
        );

        std::fs::write(&source, &content).unwrap();
        let cache_path =
            crate::cache::hydrated_pointer::cache_path_for_worktree_root(root).unwrap();
        let entry =
            crate::cache::hydrated_pointer::entry_for_path(&source, &pointer.serialize()).unwrap();
        crate::cache::HydratedPointerCache::update_on_disk(
            &cache_path,
            [("models/model.bin".to_owned(), entry)],
        )
        .unwrap();

        let index = sibling_cow_candidates(&linked);
        assert_eq!(
            index.candidates(&pointer),
            [std::fs::canonicalize(&source).unwrap()]
        );

        std::fs::write(root.join("models/renamed.bin"), &content).unwrap();
        let renamed_entry = crate::cache::hydrated_pointer::entry_for_path(
            &root.join("models/renamed.bin"),
            &pointer.serialize(),
        )
        .unwrap();
        let other_content = b"content from a different branch";
        let other_pointer = pointer_for_content(other_content);
        std::fs::write(root.join("models/other.bin"), other_content).unwrap();
        let other_entry = crate::cache::hydrated_pointer::entry_for_path(
            &root.join("models/other.bin"),
            &other_pointer.serialize(),
        )
        .unwrap();
        crate::cache::HydratedPointerCache::update_on_disk(
            &cache_path,
            [
                ("models/renamed.bin".to_owned(), renamed_entry),
                ("models/other.bin".to_owned(), other_entry),
            ],
        )
        .unwrap();
        let index = sibling_cow_candidates(&linked);
        assert_eq!(index.candidates(&pointer).len(), 2);
        assert_eq!(index.candidates(&other_pointer).len(), 1);

        std::fs::write(&source, b"source changed after cache fingerprint").unwrap();
        std::fs::remove_file(root.join("models/renamed.bin")).unwrap();
        let index = sibling_cow_candidates(&linked);
        assert!(index.candidates(&pointer).is_empty());

        std::fs::write(&cache_path, b"{corrupt cache").unwrap();
        let index = sibling_cow_candidates(&linked);
        assert!(index.candidates(&other_pointer).is_empty());
    }

    #[test]
    fn sibling_candidate_index_ignores_a_moved_worktree_record() {
        let fixture = GitFixture::new();
        let root = fixture.work_tree();
        std::fs::write(
            root.join(".gitattributes"),
            "*.bin filter=crab diff=crab merge=crab -text\n",
        )
        .unwrap();
        let content = b"content in worktree that moves".repeat(64);
        let pointer = pointer_for_content(&content);
        std::fs::write(root.join("model.bin"), pointer.serialize()).unwrap();
        run_git_without_crab_filter(root, &["add", ".gitattributes", "model.bin"]);
        run_git(root, &["commit", "-m", "add pointer"]);

        let linked_parent = tempfile::tempdir().unwrap();
        let current = linked_parent.path().join("current");
        let source_root = linked_parent.path().join("source");
        let current_arg = current.to_string_lossy().into_owned();
        let source_arg = source_root.to_string_lossy().into_owned();
        run_git(
            root,
            &["worktree", "add", "--detach", current_arg.as_str(), "HEAD"],
        );
        run_git(
            root,
            &["worktree", "add", "--detach", source_arg.as_str(), "HEAD"],
        );
        let source = source_root.join("model.bin");
        std::fs::write(&source, &content).unwrap();
        let cache_path =
            crate::cache::hydrated_pointer::cache_path_for_worktree_root(&source_root).unwrap();
        let entry =
            crate::cache::hydrated_pointer::entry_for_path(&source, &pointer.serialize()).unwrap();
        crate::cache::HydratedPointerCache::update_on_disk(
            &cache_path,
            [("model.bin".to_owned(), entry)],
        )
        .unwrap();
        assert_eq!(
            sibling_cow_candidates(&current).candidates(&pointer).len(),
            1
        );

        std::fs::rename(&source_root, linked_parent.path().join("source-moved")).unwrap();
        assert!(
            sibling_cow_candidates(&current)
                .candidates(&pointer)
                .is_empty()
        );
    }

    #[test]
    fn hasher_tap_reports_progress_before_file_completion() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("streamed.bin");
        let file = std::fs::File::create(&dest).unwrap();
        let progress = Arc::new(HydrateProgress::new(1, 10));
        let state = Arc::new(std::sync::Mutex::new(HasherTapState {
            file: Some(file),
            hasher: blake3::Hasher::new(),
            bytes_written: 0,
            progress: Some(progress.clone()),
        }));
        let mut tap = HasherTap { shared: state };

        tap.write_all(b"abcd").unwrap();

        assert_eq!(progress.bytes_done.load(Relaxed), 4);
        assert_eq!(progress.files_done.load(Relaxed), 0);
    }

    #[test]
    fn walk_parses_pointers_correctly() {
        let dir = setup_tracked_dir();
        let ptr = sample_pointer(4096);
        std::fs::write(dir.path().join("model.bin"), ptr.serialize()).unwrap();

        let filter = build_all_filter().unwrap();
        let tracked = TrackedClassifier::open(dir.path()).unwrap();
        assert!(filter.matches("model.bin"));
        assert!(tracked.is_tracked(Path::new("model.bin")));
        let cancel = CancellationToken::new();
        let mut out = Vec::new();
        walk_and_parse_pointers(dir.path(), dir.path(), &tracked, &filter, &cancel, &mut out)
            .unwrap();

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].1, ptr);
    }

    #[tokio::test]
    async fn hydrate_batch_reports_summary() {
        let dir = setup_tracked_dir();
        let ptr = sample_pointer(4096);
        std::fs::write(dir.path().join("a.bin"), ptr.serialize()).unwrap();
        std::fs::write(dir.path().join("b.bin"), ptr.serialize()).unwrap();

        let args = HydrateArgs {
            all: true,
            ..default_args()
        };
        let config = Config::default();
        let cancel = CancellationToken::new();
        let hydrator = StubHydrator;
        // Should complete without error and print summary.
        run_hydrate_in(dir.path(), &args, &config, &hydrator, &cancel)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn hydrate_command_materializes_pointer_from_replica_backed_hydrator() {
        let git_guard = ScopedGitDir::acquire();
        let cache_root = tempfile::tempdir().expect("cache root");
        let _cache_guard = CacheDirGuard::new(cache_root.path());
        let fixture = GitFixture::new();
        git_guard.set_git_dir(&fixture.git_dir());

        let original = command_path_content();
        let (chunks, file_hash) = chunk_and_hash(&original);
        let pointer = Pointer {
            file_hash,
            size: original.len() as u64,
            shard_hint: None,
        };
        let staging =
            stage_command_path_file(fixture.work_tree(), file_hash, &chunks, pointer.size).await;

        let inner: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
        let primary_store = Store::new(Arc::clone(&inner));
        push_command_path_pointer(&fixture, primary_store, staging, &pointer).await;

        {
            let direct_store = Store::new(Arc::clone(&inner));
            let direct_cache_dir = tempfile::tempdir().expect("direct hydrate cache");
            let direct_cache = Arc::new(crate::cache::LocalCache::new(
                direct_cache_dir.path().to_path_buf(),
            ));
            let direct_caching_store = crab_cache_store::CachingStore::new_with_local_cache(
                direct_store.clone(),
                &crate::core::config::CacheConfig::default(),
                direct_cache,
            )
            .expect("build direct caching store");
            let direct_hydrator = ShardHydrator::new_from_cli_layout(
                direct_caching_store,
                StoreLayout::new(direct_store, TEST_REPLICA_PREFIX.to_owned()),
            )
            .expect("build direct shard hydrator");
            let cancelled_probe = fixture.work_tree().join("cancelled-probe.bin");
            let sentinel = pointer.serialize();
            std::fs::write(&cancelled_probe, &sentinel).expect("write cancellation sentinel");
            let cancelled = CancellationToken::new();
            cancelled.cancel();
            let error = direct_hydrator
                .reconstruct_to_atomic_path_with_cancel(
                    &pointer,
                    &cancelled_probe,
                    None,
                    None,
                    cancelled,
                )
                .await
                .expect_err("cancelled reconstruction must not publish");
            assert!(matches!(error, error::CrabError::Cancelled));
            assert_eq!(
                std::fs::read(&cancelled_probe).expect("read cancellation sentinel"),
                sentinel,
                "cancellation must leave the destination untouched"
            );
            std::fs::remove_file(&cancelled_probe).expect("remove cancellation probe");
            let direct_probe = fixture.work_tree().join("direct-probe.bin");
            direct_hydrator
                .reconstruct_to_atomic_path_with_cancel(
                    &pointer,
                    &direct_probe,
                    None,
                    None,
                    CancellationToken::new(),
                )
                .await
                .expect("replica-backed hydrator reconstructs directly");
            assert_eq!(
                std::fs::read(&direct_probe).expect("read direct probe"),
                original
            );
            std::fs::remove_file(&direct_probe).expect("remove direct probe");
        }

        let replica_store = Store::new(inner);
        let cache_dir = tempfile::tempdir().expect("hydrate cache");
        let cache = Arc::new(crate::cache::LocalCache::new(
            cache_dir.path().to_path_buf(),
        ));
        let caching_store = crab_cache_store::CachingStore::new_with_local_cache(
            replica_store.clone(),
            &crate::core::config::CacheConfig::default(),
            cache,
        )
        .expect("build caching store");
        let hydrator = ShardHydrator::new_from_cli_layout(
            caching_store,
            StoreLayout::new(replica_store, TEST_REPLICA_PREFIX.to_owned()),
        )
        .expect("build replica shard hydrator");

        let args = HydrateArgs {
            all: true,
            mode: OutputMode::Json,
            ..default_args()
        };
        let config = Config::default();
        let cancel = CancellationToken::new();

        run_hydrate_in(fixture.work_tree(), &args, &config, &hydrator, &cancel)
            .await
            .expect("hydrate command");

        let hydrated = std::fs::read(fixture.work_tree().join("model.bin")).expect("read hydrated");
        assert_eq!(hydrated, original);
        assert!(
            Pointer::parse(&hydrated).is_err(),
            "hydrate must materialize content, not leave the pointer blob"
        );
    }

    #[tokio::test]
    async fn delta_terms_repair_corrupt_xorb_cache_from_origin() {
        use crab_xet::shard::FileDataSequenceEntry;
        use crab_xet::xorb::format::Chunk;

        use crab_xet::xorb::builder::{RunId, XorbBuilder};

        let chunks = vec![
            Bytes::from_static(b"base chunk payload"),
            Bytes::from_static(b"target chunk payload"),
        ];
        let mut builder = XorbBuilder::new();
        for chunk in &chunks {
            builder.push(&Chunk::new(chunk.clone()), RunId(0)).unwrap();
        }
        let xorb = builder.finalize().unwrap().pop().expect("one xorb");

        let inner: Arc<dyn object_store::ObjectStore> = Arc::new(InMemory::new());
        let origin = Store::new(inner);
        let caching_store = crab_cache_store::CachingStore::new(
            origin.clone(),
            &crate::core::config::CacheConfig::default(),
        )
        .expect("build caching store");
        let store_cache = Arc::clone(caching_store.local_cache());
        let router = StoreLayout::new(origin, TEST_REPLICA_PREFIX.to_owned());
        caching_store
            .origin()
            .put(&router.xorb_path(&xorb.hash), xorb.bytes.clone())
            .await
            .expect("upload xorb");

        let key = crate::cache::CacheKey::Xorb(xorb.hash);
        store_cache
            .put_unchecked_for_test(&key, b"not a valid xorb")
            .await
            .expect("seed corrupt store xorb");

        let hydrator = ShardHydrator::new_from_cli_layout(caching_store, router)
            .expect("build shard hydrator");
        let segment = FileDataSequenceEntry::new(
            xorb.hash,
            chunks.iter().map(Bytes::len).sum::<usize>() as u32,
            0u32,
            chunks.len() as u32,
        );
        let terms = hydrator
            .segments_to_terms(&[segment])
            .await
            .expect("corrupt cache should be repaired from origin");

        assert_eq!(terms.len(), chunks.len());
        for (term, chunk) in terms.iter().zip(&chunks) {
            assert_eq!(term.chunk_hash, *blake3::hash(chunk).as_bytes());
            assert_eq!(term.length, chunk.len() as u64);
        }

        let repaired = store_cache
            .get_or_fetch(&key, || async {
                panic!("repaired cache should be present")
            })
            .await
            .expect("read repaired hydrate cache");
        assert_eq!(repaired, xorb.bytes);
    }

    #[tokio::test]
    async fn hydrate_batch_skips_already_hydrated_files() {
        let dir = tempfile::tempdir().unwrap();

        let hydrated = vec![0xAB; 4096];
        let ptr = pointer_for_content(&hydrated);

        // File A: still a pointer on disk — should be hydrated.
        let path_a = dir.path().join("a.bin");
        std::fs::write(&path_a, ptr.serialize()).unwrap();

        // File B: already hydrated — non-pointer content whose size
        // matches ptr.size. The hydrator should skip it.
        let path_b = dir.path().join("b.bin");
        std::fs::write(&path_b, hydrated).unwrap();

        let items = vec![(path_a.clone(), ptr.clone()), (path_b.clone(), ptr.clone())];
        let cancel = CancellationToken::new();
        let hydrator = StubHydrator;
        let summary = hydrator.hydrate_batch(&items, &cancel, None).await.unwrap();

        assert_eq!(
            summary.skipped, 1,
            "already-hydrated file should be skipped"
        );
        assert_eq!(summary.hydrated, 1, "pointer file should be hydrated");
        assert_eq!(summary.failed, 0);
    }

    #[test]
    fn is_already_hydrated_returns_false_for_pointer_file() {
        let dir = tempfile::tempdir().unwrap();
        let ptr = sample_pointer(4096);
        let path = dir.path().join("ptr.bin");
        std::fs::write(&path, ptr.serialize()).unwrap();
        assert!(!is_already_hydrated(&path, &ptr, &CancellationToken::new()).unwrap());
    }

    #[test]
    fn is_already_hydrated_requires_matching_hash() {
        let dir = tempfile::tempdir().unwrap();
        let content = vec![0xCD; 512];
        let ptr = pointer_for_content(&content);
        let path = dir.path().join("data.bin");
        std::fs::write(&path, &content).unwrap();
        assert!(is_already_hydrated(&path, &ptr, &CancellationToken::new()).unwrap());

        std::fs::write(&path, vec![0xCE; 512]).unwrap();
        assert!(!is_already_hydrated(&path, &ptr, &CancellationToken::new()).unwrap());
    }

    #[test]
    fn is_already_hydrated_returns_false_for_size_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        let ptr = sample_pointer(4096);
        let path = dir.path().join("data.bin");
        std::fs::write(&path, vec![0xCD; 9999]).unwrap();
        assert!(!is_already_hydrated(&path, &ptr, &CancellationToken::new()).unwrap());
    }

    #[test]
    fn is_already_hydrated_returns_false_for_missing_file() {
        let ptr = sample_pointer(4096);
        assert!(
            !is_already_hydrated(
                Path::new("/nonexistent/file.bin"),
                &ptr,
                &CancellationToken::new()
            )
            .unwrap()
        );
    }

    // --- format_bytes and format_elapsed tests ---

    #[test]
    fn format_bytes_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1024), "1.00 KiB");
        assert_eq!(format_bytes(1_048_576), "1.00 MiB");
        assert_eq!(format_bytes(1_073_741_824), "1.00 GiB");
        assert_eq!(format_bytes(1_099_511_627_776), "1.00 TiB");
    }

    #[test]
    fn format_elapsed_short_duration() {
        assert_eq!(format_elapsed(std::time::Duration::from_secs(0)), "0s");
        assert_eq!(format_elapsed(std::time::Duration::from_secs(42)), "42s");
        assert_eq!(format_elapsed(std::time::Duration::from_secs(59)), "59s");
    }

    #[test]
    fn format_elapsed_minutes_and_seconds() {
        assert_eq!(format_elapsed(std::time::Duration::from_secs(60)), "1m 0s");
        assert_eq!(
            format_elapsed(std::time::Duration::from_secs(134)),
            "2m 14s"
        );
        assert_eq!(
            format_elapsed(std::time::Duration::from_secs(3661)),
            "61m 1s"
        );
    }

    // --- SIGINT / cancellation safety tests ---

    /// Hydrator that cancels the token after writing the first file,
    /// simulating a SIGINT arriving mid-batch.
    struct CancelAfterFirstHydrator;

    impl Hydrator for CancelAfterFirstHydrator {
        fn hydrate_batch<'a>(
            &'a self,
            items: &'a [(PathBuf, Pointer)],
            cancel: &'a CancellationToken,
            _progress: Option<&'a Arc<HydrateProgress>>,
        ) -> Pin<Box<dyn Future<Output = Result<HydrateSummary>> + Send + 'a>> {
            Box::pin(async move {
                let mut summary = HydrateSummary::default();

                for (i, (path, ptr)) in items.iter().enumerate() {
                    error::check_cancelled(cancel)?;

                    let content = ptr.serialize();
                    match atomic_write(path, &content) {
                        Ok(()) => {
                            summary.hydrated += 1;
                            summary.bytes_written += content.len() as u64;
                        }
                        Err(e) => {
                            debug!(path = %path.display(), err = %e, "failed to hydrate file");
                            summary.failed += 1;
                        }
                    }

                    // Cancel after the first file is fully written.
                    if i == 0 {
                        cancel.cancel();
                    }
                }

                Ok(summary)
            })
        }
    }

    #[tokio::test]
    async fn cancel_mid_batch_leaves_no_corrupt_files() {
        // Set up three pointer files. The custom hydrator cancels after
        // writing the first one. We verify:
        //   - File 1: fully written (atomic rename completed before cancel)
        //   - Files 2 & 3: still contain their original pointer bytes
        //   - No tempfiles left behind in the directory
        let dir = tempfile::tempdir().unwrap();
        let ptr = sample_pointer(4096);
        let pointer_bytes = ptr.serialize();

        let path_a = dir.path().join("a.bin");
        let path_b = dir.path().join("b.bin");
        let path_c = dir.path().join("c.bin");
        std::fs::write(&path_a, &pointer_bytes).unwrap();
        std::fs::write(&path_b, &pointer_bytes).unwrap();
        std::fs::write(&path_c, &pointer_bytes).unwrap();

        let items = vec![
            (path_a.clone(), ptr.clone()),
            (path_b.clone(), ptr.clone()),
            (path_c.clone(), ptr.clone()),
        ];
        let cancel = CancellationToken::new();
        let hydrator = CancelAfterFirstHydrator;
        let result = hydrator.hydrate_batch(&items, &cancel, None).await;

        // The batch returns Cancelled after the second item checks the token.
        assert!(result.is_err());

        // File A was fully written before cancel — content is valid.
        let a_content = std::fs::read(&path_a).unwrap();
        assert_eq!(
            a_content, pointer_bytes,
            "first file should be fully written"
        );

        // Files B and C were never touched — still original pointer bytes.
        let b_content = std::fs::read(&path_b).unwrap();
        assert_eq!(b_content, pointer_bytes, "second file should be untouched");
        let c_content = std::fs::read(&path_c).unwrap();
        assert_eq!(c_content, pointer_bytes, "third file should be untouched");

        // No stray tempfiles in the directory.
        let entries: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .collect();
        let filenames: Vec<String> = entries
            .iter()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            filenames.len(),
            3,
            "only the three original files should exist, got: {filenames:?}"
        );
    }

    #[tokio::test]
    async fn hydrate_summary_tracks_bytes_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let content = vec![0xAB; 4096];
        let ptr = pointer_for_content(&content);

        // File A: still a pointer — will be hydrated.
        let path_a = dir.path().join("a.bin");
        std::fs::write(&path_a, ptr.serialize()).unwrap();

        // File B: already hydrated with the exact pointer content.
        let path_b = dir.path().join("b.bin");
        std::fs::write(&path_b, content).unwrap();

        let items = vec![(path_a, ptr.clone()), (path_b, ptr.clone())];
        let cancel = CancellationToken::new();
        let hydrator = StubHydrator;
        let summary = hydrator.hydrate_batch(&items, &cancel, None).await.unwrap();

        assert_eq!(summary.skipped, 1);
        assert_eq!(summary.bytes_skipped, 4096);
    }

    // --- manifest resolution tests ---

    #[test]
    fn resolve_manifest_returns_none_when_no_flags() {
        let dir = tempfile::tempdir().unwrap();
        let args = default_args();
        let result = resolve_manifest(&args, dir.path()).unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn resolve_manifest_parses_file() {
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("manifest.txt");
        std::fs::write(&manifest_path, "src/main.rs\n*.toml\n").unwrap();

        let args = HydrateArgs {
            manifest: Some(manifest_path.to_string_lossy().into_owned()),
            ..default_args()
        };
        let entries = resolve_manifest(&args, dir.path()).unwrap().unwrap();
        assert_eq!(entries.len(), 2);
        assert!(matches!(
            &entries[0],
            crate::hydrate::manifest::ManifestEntry::Path(_)
        ));
        assert!(matches!(
            &entries[1],
            crate::hydrate::manifest::ManifestEntry::Glob(_)
        ));
    }

    #[test]
    fn resolve_manifest_errors_when_both_flags_set() {
        let dir = tempfile::tempdir().unwrap();
        let args = HydrateArgs {
            manifest: Some("file.txt".to_string()),
            manifest_ref: Some("HEAD:file.txt".to_string()),
            ..default_args()
        };
        let result = resolve_manifest(&args, dir.path());
        assert!(result.is_err());
    }

    #[test]
    fn resolve_manifest_profile_returns_glob_entries() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("crab.toml"),
            "version = 1\n\n[remote]\nurl = \"crab://bucket/repo\"\n\n[prefetch.profiles.ci]\npaths = [\"tests/**\", \"*.toml\"]\n",
        )
        .unwrap();

        let args = HydrateArgs {
            profile: Some("ci".to_string()),
            ..default_args()
        };
        let entries = resolve_manifest(&args, dir.path()).unwrap().unwrap();
        assert_eq!(entries.len(), 2);
        assert!(matches!(
            &entries[0],
            crate::hydrate::manifest::ManifestEntry::Glob(_)
        ));
        assert!(matches!(
            &entries[1],
            crate::hydrate::manifest::ManifestEntry::Glob(_)
        ));
    }

    #[test]
    fn resolve_manifest_profile_not_found_errors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("crab.toml"),
            "version = 1\n\n[remote]\nurl = \"crab://bucket/repo\"\n\n[prefetch.profiles.always]\npaths = [\"*.md\"]\n",
        )
        .unwrap();

        let args = HydrateArgs {
            profile: Some("nonexistent".to_string()),
            ..default_args()
        };
        let result = resolve_manifest(&args, dir.path());
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(
                err,
                crate::core::error::CrabError::PrefetchProfileNotFound { .. }
            ),
            "expected PrefetchProfileNotFound, got: {err:?}"
        );
    }

    #[test]
    fn resolve_manifest_profile_missing_toml_errors() {
        let dir = tempfile::tempdir().unwrap();
        // No crab.toml — load_prefetch returns empty config,
        // so the profile lookup should fail with PrefetchProfileNotFound.
        let args = HydrateArgs {
            profile: Some("ci".to_string()),
            ..default_args()
        };
        let result = resolve_manifest(&args, dir.path());
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            crate::core::error::CrabError::PrefetchProfileNotFound { .. }
        ));
    }

    #[test]
    fn build_manifest_filter_matches_literals_and_globs() {
        use crate::hydrate::manifest::ManifestEntry;

        let entries = vec![
            ManifestEntry::Path(PathBuf::from("src/main.rs")),
            ManifestEntry::Glob(globset::Glob::new("*.toml").unwrap()),
        ];
        let filter = build_manifest_filter(&entries, &[]).unwrap();
        assert!(filter.matches("src/main.rs"));
        assert!(filter.matches("Cargo.toml"));
        assert!(!filter.matches("README.md"));
    }

    #[tokio::test]
    async fn hydrate_manifest_file_selects_matching_pointers() {
        let dir = setup_tracked_dir();
        let ptr = sample_pointer(4096);

        // Create pointer files.
        std::fs::write(dir.path().join("model.bin"), ptr.serialize()).unwrap();
        std::fs::write(dir.path().join("data.bin"), ptr.serialize()).unwrap();

        // Create a manifest that only lists model.bin.
        let manifest_path = dir.path().join("manifest.txt");
        std::fs::write(&manifest_path, "model.bin\n").unwrap();

        let args = HydrateArgs {
            manifest: Some(manifest_path.to_string_lossy().into_owned()),
            ..default_args()
        };
        let config = Config::default();
        let cancel = CancellationToken::new();
        let hydrator = StubHydrator;

        // Should hydrate only model.bin, not data.bin.
        run_hydrate_in(dir.path(), &args, &config, &hydrator, &cancel)
            .await
            .unwrap();

        // model.bin should have been written (stub writes pointer bytes back).
        let model_content = std::fs::read(dir.path().join("model.bin")).unwrap();
        assert_eq!(model_content, ptr.serialize());

        // data.bin should still be the original pointer (untouched by hydrate).
        let data_content = std::fs::read(dir.path().join("data.bin")).unwrap();
        assert_eq!(data_content, ptr.serialize());
    }

    #[tokio::test]
    async fn hydrate_manifest_with_glob_expands_patterns() {
        let dir = setup_tracked_dir();
        let ptr = sample_pointer(4096);

        // Create pointer files in a subdirectory.
        let sub = dir.path().join("models");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("a.bin"), ptr.serialize()).unwrap();
        std::fs::write(sub.join("b.bin"), ptr.serialize()).unwrap();
        // Non-matching file at root.
        std::fs::write(dir.path().join("readme.bin"), ptr.serialize()).unwrap();

        // Manifest with a glob that matches only the models/ subdirectory.
        let manifest_path = dir.path().join("manifest.txt");
        std::fs::write(&manifest_path, "models/*.bin\n").unwrap();

        let args = HydrateArgs {
            manifest: Some(manifest_path.to_string_lossy().into_owned()),
            ..default_args()
        };
        let config = Config::default();
        let cancel = CancellationToken::new();
        let hydrator = StubHydrator;

        run_hydrate_in(dir.path(), &args, &config, &hydrator, &cancel)
            .await
            .unwrap();
    }

    // --- JSONL manifest output tests ---

    #[test]
    fn manifest_hydrate_file_row_serializes_expected_fields() {
        let row = ManifestHydrateFileRow {
            path: "src/main.rs".to_owned(),
            strategy: "shard_batch".to_owned(),
            duration_ms: 42,
            bytes: 8192,
        };
        let json = serde_json::to_value(&row).unwrap();
        assert_eq!(json["path"], "src/main.rs");
        assert_eq!(json["strategy"], "shard_batch");
        assert_eq!(json["duration_ms"], 42);
        assert_eq!(json["bytes"], 8192);
    }

    #[test]
    fn manifest_hydrate_summary_row_has_type_summary() {
        let row = ManifestHydrateSummaryRow {
            row_type: "summary".to_owned(),
            total: 50,
            hydrated: 48,
            skipped: 1,
            failed: 1,
            cow_cloned: 2,
            bytes_cow_cloned: 4096,
            duration_ms: 3200,
        };
        let json = serde_json::to_value(&row).unwrap();
        assert_eq!(json["type"], "summary");
        assert_eq!(json["total"], 50);
        assert_eq!(json["hydrated"], 48);
        assert_eq!(json["skipped"], 1);
        assert_eq!(json["failed"], 1);
        assert_eq!(json["cow_cloned"], 2);
        assert_eq!(json["bytes_cow_cloned"], 4096);
        assert_eq!(json["duration_ms"], 3200);
    }

    #[tokio::test]
    async fn hydrate_file_result_channel_sends_per_file_results() {
        let dir = tempfile::tempdir().unwrap();
        let content = vec![0xAB; 4096];
        let ptr = pointer_for_content(&content);

        let path_a = dir.path().join("a.bin");
        let path_b = dir.path().join("b.bin");
        std::fs::write(&path_a, ptr.serialize()).unwrap();
        // b.bin is already hydrated with the exact pointer content.
        std::fs::write(&path_b, content).unwrap();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<HydrateFileResult>();
        let total_bytes = ptr.size * 2;
        let progress = Arc::new(HydrateProgress::with_file_result_tx(2, total_bytes, tx));

        let items = vec![(path_a.clone(), ptr.clone()), (path_b.clone(), ptr.clone())];
        let cancel = CancellationToken::new();
        let hydrator = StubHydrator;
        let summary = hydrator
            .hydrate_batch(&items, &cancel, Some(&progress))
            .await
            .unwrap();

        assert_eq!(summary.hydrated, 1);
        assert_eq!(summary.skipped, 1);

        // Drop progress to close the channel sender.
        drop(progress);

        let mut results = Vec::new();
        while let Some(r) = rx.recv().await {
            results.push(r);
        }

        assert_eq!(results.len(), 2, "should receive one result per file");

        // First result: a.bin was hydrated.
        assert_eq!(results[0].path, path_a);
        assert_eq!(results[0].outcome, HydrateFileOutcome::Hydrated);
        assert!(results[0].bytes > 0);

        // Second result: b.bin was skipped.
        assert_eq!(results[1].path, path_b);
        assert_eq!(results[1].outcome, HydrateFileOutcome::Skipped);
        assert_eq!(results[1].bytes, 4096);
    }

    #[tokio::test]
    async fn manifest_jsonl_mode_emits_streaming_rows_and_summary() {
        let dir = setup_tracked_dir();
        let ptr = sample_pointer(4096);
        std::fs::write(dir.path().join("model.bin"), ptr.serialize()).unwrap();

        let manifest_path = dir.path().join("manifest.txt");
        std::fs::write(&manifest_path, "model.bin\n").unwrap();

        let args = HydrateArgs {
            manifest: Some(manifest_path.to_string_lossy().into_owned()),
            mode: OutputMode::Jsonl,
            ..default_args()
        };
        let config = Config::default();
        let cancel = CancellationToken::new();
        let hydrator = StubHydrator;

        // The JSONL output goes to stdout; we just verify the function
        // completes without error in manifest + JSONL mode.
        run_hydrate_in(dir.path(), &args, &config, &hydrator, &cancel)
            .await
            .unwrap();
    }
}
