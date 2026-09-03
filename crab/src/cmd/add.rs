//! `crab add <patterns>` — parallel file staging that bypasses git's
//! serial filter protocol.
//!
//! For each matching file: bounded streaming computes the blake3 hash,
//! CDC-chunks the bytes, and batch-stages them into the segment-based
//! staging area. The working tree file is left untouched — the original
//! content remains visible to the user.
//! Files are processed concurrently across CPU cores, giving a ~Nx speedup
//! over `git add` (which is limited to one file at a time by the filter
//! protocol).
//!
//! After all files are staged, the command flushes the staging segment
//! boundary and writes pointer blobs directly into Git's object database
//! and index. This mirrors the Git LFS model: git stores a lightweight
//! pointer, while the working tree keeps the real file.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom, Stdout, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

use bstr::ByteSlice;
use futures_util::stream::{FuturesUnordered, StreamExt};
use serde::Serialize;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::cache::add_validation::VerifiedPath;
use crate::cmd::stream_stage::cleanup_stream_prepared_xorbs;
use crate::core::error::{self, CrabError, Result};
use crate::core::output::event_payloads::{FileDonePayload, ProgressPayload};
use crate::core::output::{JsonlStream, OutputMode, emit_json};
use crate::core::pattern::{PatternFilter, build_filter};
use crate::git::progress::{format_bytes, format_rate, is_tty, render_bar};
use crate::git::push::PushConfig;
use crab_staging::{PublicationIntentEntry, PublicationIntentId, StagingArea, StagingAreaReadOnly};
use crab_types::pointer::Pointer;
use crab_xet::hash::MerkleHash;
use crab_xet::xorb::builder::XorbBuilder;

const ADD_PROGRESS_BAR_WIDTH: usize = 20;
const ADD_PROGRESS_BAR_MIN_WIDTH: usize = 8;
const ADD_PROGRESS_NAME_MIN_WIDTH: usize = 8;
const DEFAULT_TERMINAL_WIDTH: usize = 80;
const ADD_INCOMPLETE_PCT_FRACTION: f64 = 0.999;
const ADD_DUPLICATE_FINGERPRINT_BYTES: usize = 1024 * 1024;
const ADD_DUPLICATE_REUSE_MIN_BYTES: u64 = 32 * 1024 * 1024;
const ADD_STAGING_LOCK_WAIT_BUDGET: Duration = Duration::from_mins(30);

/// Arguments for the `crab add` command.
pub struct AddArgs {
    /// Glob patterns to add (e.g. `*.safetensors`, `models/`).
    pub patterns: Vec<String>,
    /// Maximum number of concurrent file-processing tasks.
    pub jobs: usize,
    /// Dry run: show what would be added without staging.
    pub dry_run: bool,
    /// Skip the final `git add` step (useful for scripting).
    pub skip_git_add: bool,
    /// Output mode resolved from `--json` / `--jsonl` flags.
    pub mode: OutputMode,
}

/// Summary of a completed add operation.
#[derive(Debug, Default, Serialize, schemars::JsonSchema)]
pub struct AddSummary {
    pub files_staged: u64,
    pub files_skipped: u64,
    pub validation_cache_hits: u64,
    pub validation_cache_hit_bytes: u64,
    pub files_failed: u64,
    pub chunks_staged: u64,
    pub bytes_processed: u64,
    pub lock_wait_duration_ms: u64,
    pub chunking_worker_duration_ms: u64,
    pub remote_lookup_duration_ms: u64,
    pub compression_worker_duration_ms: u64,
    pub payload_write_duration_ms: u64,
    pub staging_duration_ms: u64,
    pub planning_duration_ms: u64,
    pub flushing_duration_ms: u64,
    pub indexing_duration_ms: u64,
    pub duration_ms: u64,
}

/// Result of processing a single file.
struct FileResult {
    batch_id: crab_staging::StagingBatchId,
    /// Absolute path for git add.
    abs_path: PathBuf,
    /// Number of CDC chunks produced.
    chunks: usize,
    /// Original file size in bytes.
    size: u64,
    /// Blake3 hash of the full file content. Needed to assemble the
    /// pointer blob written into git's ODB during the indexing phase —
    /// skipping the clean-filter round-trip that `git add` would
    /// otherwise trigger.
    file_hash: [u8; 32],
    /// Canonical indexed recipe root produced by the verified chunking pass.
    recipe: crab_staging::recipe::FileRecipe,
    /// Prepared xorbs produced during the verified streaming pass.
    prepared_xorbs: Vec<crate::cmd::stream_stage::StreamStagePreparedXorb>,
    /// Stat snapshot captured while the staged bytes were verified.
    index_stat: Option<crate::cmd::stream_stage::VerifiedIndexStat>,
    timings: crab_staging::stream::StreamStageTimings,
    /// Wall-clock duration of the operation in milliseconds.
    duration_ms: u64,
}

struct StagedEntry {
    batch_id: Option<crab_staging::StagingBatchId>,
    abs_path: PathBuf,
    file_hash: [u8; 32],
    size: u64,
    recipe: crab_staging::recipe::FileRecipe,
    prepared_xorbs: Vec<crate::cmd::stream_stage::StreamStagePreparedXorb>,
    index_stat: Option<crate::cmd::stream_stage::VerifiedIndexStat>,
}

struct IndexPublicationEntry {
    batch_id: Option<crab_staging::StagingBatchId>,
    abs_path: PathBuf,
    file_hash: [u8; 32],
    size: u64,
    index_stat: Option<crate::cmd::stream_stage::VerifiedIndexStat>,
}

impl From<&StagedEntry> for IndexPublicationEntry {
    fn from(entry: &StagedEntry) -> Self {
        Self {
            batch_id: entry.batch_id.clone(),
            abs_path: entry.abs_path.clone(),
            file_hash: entry.file_hash,
            size: entry.size,
            index_stat: entry.index_stat,
        }
    }
}

struct AddExecutionPlans {
    duplicate_plan: DuplicateReusePlan,
    stream_xorb_plan: Option<StreamPreparedXorbPlan>,
    remote_classifier: Option<Arc<crate::git::push::AddRemoteChunkClassifier>>,
}

struct StreamPreparedXorbPlan {
    builder: crate::cmd::stream_stage::StreamStageXorbBuilder,
    enabled_paths: HashSet<PathBuf>,
}

const ADD_STREAM_XORB_BUILDERS: usize = 2;

impl StreamPreparedXorbPlan {
    fn builder_for(&self, path: &Path) -> Option<crate::cmd::stream_stage::StreamStageXorbBuilder> {
        self.enabled_paths
            .contains(path)
            .then(|| self.builder.clone())
    }

    fn bind_preparation(&mut self, preparation_id: crab_staging::AddPreparationId) {
        self.builder = self.builder.clone().bind_preparation(preparation_id);
    }

    fn preparation_id(&self) -> Option<&crab_staging::AddPreparationId> {
        self.builder.preparation_id()
    }
}

#[derive(Debug, Default)]
struct DuplicateReusePlan {
    representative_by_path: HashMap<PathBuf, PathBuf>,
}

impl DuplicateReusePlan {
    fn representative_for(&self, path: &Path) -> Option<&Path> {
        self.representative_by_path.get(path).map(PathBuf::as_path)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CandidateFingerprint {
    size: u64,
    head_hash: [u8; 32],
    middle_hash: [u8; 32],
    tail_hash: [u8; 32],
}

#[derive(Debug)]
struct CandidateFingerprintRecord {
    path: PathBuf,
    size: u64,
    fingerprint: CandidateFingerprint,
}

#[derive(Debug, Clone, Copy)]
struct StreamPreparedPlanGroup {
    representative_idx: usize,
    files: u64,
}

#[derive(Clone)]
struct ReusableStagedFile {
    file_hash: [u8; 32],
    size: u64,
    recipe: crab_staging::recipe::FileRecipe,
}

#[derive(Default)]
struct CleanIndexedSkipSummary {
    files: u64,
    bytes: u64,
    validation_cache_hits: u64,
    validation_cache_hit_bytes: u64,
}

struct CleanIndexPointer {
    expected_hash: [u8; 32],
    path_bytes: Vec<u8>,
    validation_token: Option<[u8; 32]>,
    verified_stat: crate::cmd::stream_stage::VerifiedIndexStat,
}

struct AddFileProgressSpec {
    name: String,
    total_bytes: u64,
}

struct AddFileProgress {
    name: String,
    total_bytes: u64,
    bytes_done: Arc<AtomicU64>,
    chunk_bytes_done: Arc<AtomicU64>,
    chunks_done: Arc<AtomicU64>,
    state: AtomicU8,
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AddFileState {
    Pending = 0,
    Running = 1,
    Done = 2,
    Failed = 3,
}

impl AddFileState {
    fn from_u8(n: u8) -> Self {
        match n {
            1 => Self::Running,
            2 => Self::Done,
            3 => Self::Failed,
            _ => Self::Pending,
        }
    }
}

impl AddFileProgress {
    fn new(spec: AddFileProgressSpec) -> Self {
        Self {
            name: spec.name,
            total_bytes: spec.total_bytes,
            bytes_done: Arc::new(AtomicU64::new(0)),
            chunk_bytes_done: Arc::new(AtomicU64::new(0)),
            chunks_done: Arc::new(AtomicU64::new(0)),
            state: AtomicU8::new(AddFileState::Pending as u8),
        }
    }

    fn set_state(&self, state: AddFileState) {
        self.state.store(state as u8, Relaxed);
    }

    fn state(&self) -> AddFileState {
        AddFileState::from_u8(self.state.load(Relaxed))
    }
}

/// Shared progress state for live add reporting.
///
/// Updated atomically by each file task as streaming and staging
/// complete. A background ticker reads these counters to render one
/// bounded terminal row per active file when stderr is a TTY.
struct AddProgress {
    /// Total number of files to add (set once before the batch starts).
    total_files: AtomicU64,
    /// Total bytes across all files to add.
    total_bytes: AtomicU64,
    /// Files completed so far (staged + failed).
    files_done: AtomicU64,
    /// Current phase of the add pipeline. See [`AddPhase`].
    phase: AtomicU8,
    /// Files whose add-time push plan has been prepared.
    plan_files_done: AtomicU64,
    /// Chunks considered while preparing add-time push plans.
    plan_chunks: AtomicU64,
    /// Chunks already known by the remote chunk index.
    plan_existing_candidates: AtomicU64,
    /// Prepared xorbs reused from local add/cache state.
    plan_prepared_cache_xorbs: AtomicU64,
    /// Prepared xorbs emitted for the current add.
    plan_prepared_xorbs: AtomicU64,
    /// Prepared xorb bytes available for a later push.
    plan_prepared_bytes: AtomicU64,
    /// Whether add-time planning is checking the remote chunk index.
    plan_remote_lookup: AtomicU8,
    /// Per-file slots. Running slots render as separate progress rows.
    files: Vec<Arc<AddFileProgress>>,
    /// Timestamp when the batch started.
    start: Instant,
}

/// Coarse-grained stages of the `crab add` pipeline, used to drive
/// the live progress bar's label so users see what's happening during
/// the visible streaming pass and the quieter segment flush + index
/// writes that follow it.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AddPhase {
    /// Single-pass streaming hashes bytes, CDC-chunks, and batches
    /// chunks into staging.
    Streaming = 0,
    /// Add-time push plan preparation validates staged rows, reuses
    /// local prepared xorbs, and may consult the remote chunk index.
    Planning = 1,
    /// `StagingArea::close` — fsync any residual pending chunks before
    /// direct pointer writes publish them into Git's index. No per-file
    /// counter moves here; the bar shows a spinner-like indeterminate.
    Flushing = 2,
    /// Pointer blobs are written into Git's object database and index.
    /// Bytes/files counters are already at 100%, but the label advertises
    /// why the command is still running.
    Indexing = 3,
}

impl AddPhase {
    fn from_u8(n: u8) -> Self {
        match n {
            1 => Self::Planning,
            2 => Self::Flushing,
            3 => Self::Indexing,
            _ => Self::Streaming,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Streaming => "Streaming:",
            Self::Planning => "Planning: ",
            Self::Flushing => "Flushing: ",
            Self::Indexing => "Indexing: ",
        }
    }
}

impl AddProgress {
    fn new(files: Vec<AddFileProgressSpec>) -> Self {
        let total_files = files.len() as u64;
        let total_bytes = files.iter().map(|f| f.total_bytes).sum();
        let files = files
            .into_iter()
            .map(AddFileProgress::new)
            .map(Arc::new)
            .collect();

        Self {
            total_files: AtomicU64::new(total_files),
            total_bytes: AtomicU64::new(total_bytes),
            files_done: AtomicU64::new(0),
            phase: AtomicU8::new(AddPhase::Streaming as u8),
            plan_files_done: AtomicU64::new(0),
            plan_chunks: AtomicU64::new(0),
            plan_existing_candidates: AtomicU64::new(0),
            plan_prepared_cache_xorbs: AtomicU64::new(0),
            plan_prepared_xorbs: AtomicU64::new(0),
            plan_prepared_bytes: AtomicU64::new(0),
            plan_remote_lookup: AtomicU8::new(0),
            files,
            start: Instant::now(),
        }
    }

    fn file_progress(&self, index: usize) -> Option<Arc<AddFileProgress>> {
        self.files.get(index).cloned()
    }

    fn set_phase(&self, phase: AddPhase) {
        self.phase.store(phase as u8, Relaxed);
    }

    fn update_plan_summary(&self, summary: &crab_staging::push_plan::AddPushPlanSummary) {
        self.plan_files_done.store(summary.files, Relaxed);
        self.plan_chunks.store(summary.chunks, Relaxed);
        self.plan_existing_candidates
            .store(summary.existing_candidates, Relaxed);
        self.plan_prepared_cache_xorbs
            .store(summary.prepared_cache_xorbs, Relaxed);
        self.plan_prepared_xorbs
            .store(summary.prepared_xorbs, Relaxed);
        self.plan_prepared_bytes
            .store(summary.prepared_bytes, Relaxed);
        self.plan_remote_lookup
            .store(u8::from(summary.remote_lookup), Relaxed);
    }

    fn bytes_done(&self) -> u64 {
        self.files.iter().map(|f| f.bytes_done.load(Relaxed)).sum()
    }

    fn chunk_bytes_done(&self) -> u64 {
        self.files
            .iter()
            .map(|f| f.chunk_bytes_done.load(Relaxed))
            .sum()
    }

    fn chunks_done(&self) -> u64 {
        self.files.iter().map(|f| f.chunks_done.load(Relaxed)).sum()
    }

    fn render_lines(&self, color: bool, width: usize) -> Vec<String> {
        let phase = AddPhase::from_u8(self.phase.load(Relaxed));
        match phase {
            AddPhase::Streaming => self.render_streaming_lines(color, width),
            AddPhase::Planning | AddPhase::Flushing | AddPhase::Indexing => {
                vec![self.render_batch_line(phase, color, width)]
            }
        }
    }

    fn render_streaming_lines(&self, color: bool, width: usize) -> Vec<String> {
        if self.files.len() == 1 {
            return self
                .files
                .first()
                .map(|file| self.render_file_line(file, color, width))
                .into_iter()
                .collect();
        }

        let active: Vec<_> = self
            .files
            .iter()
            .filter(|file| file.state() == AddFileState::Running)
            .cloned()
            .collect();

        if active.is_empty() {
            vec![self.render_batch_line(AddPhase::Streaming, color, width)]
        } else {
            active
                .iter()
                .map(|file| self.render_file_line(file, color, width))
                .collect()
        }
    }

    fn render_file_line(&self, file: &AddFileProgress, color: bool, width: usize) -> String {
        let state = file.state();
        let total = file.total_bytes;
        let stream_done = file.bytes_done.load(Relaxed);
        let chunk_done = file.chunk_bytes_done.load(Relaxed);
        let chunks = file.chunks_done.load(Relaxed);
        let complete = state == AddFileState::Done;
        let pass = file_active_pass(stream_done, total, complete);
        let pass_done = match pass {
            AddPass::Streaming => stream_done.min(total),
            AddPass::Chunking => chunk_done.min(total),
        };
        let pass_fraction = single_pass_fraction(pass_done, total, complete);
        let display_fraction = displayed_fraction(
            two_pass_fraction(stream_done, chunk_done, total, complete),
            complete,
        );
        let pct = (display_fraction * 100.0).min(100.0);
        let elapsed = self.start.elapsed();
        let rate = if elapsed.as_secs_f64() > 0.0 {
            pass_done as f64 / elapsed.as_secs_f64()
        } else {
            0.0
        };

        let label = match state {
            AddFileState::Failed => "Failed:",
            AddFileState::Done => "Added: ",
            _ => "Adding:",
        };
        let pass_label = pass.label();
        let pass_pct = (pass_fraction * 100.0).min(100.0);
        let metrics = vec![
            format!(
                "{pass_label} {pass_pct:.1}% | {} / {} | {} | {chunks} chunks",
                format_bytes(pass_done),
                format_bytes(total),
                format_rate(rate)
            ),
            format!(
                "{pass_label} {} / {} | {chunks} chunks",
                format_bytes(pass_done),
                format_bytes(total)
            ),
            format!("{pass_label} | {chunks} chunks"),
            pass_label.to_owned(),
        ];

        fit_progress_line(
            label,
            pct,
            display_fraction,
            complete,
            color,
            width,
            &metrics,
            &file.name,
        )
    }

    fn render_batch_line(&self, phase: AddPhase, color: bool, width: usize) -> String {
        let stream_done = self.bytes_done();
        let chunk_done = self.chunk_bytes_done();
        let total = self.total_bytes.load(Relaxed);
        let files_done = self.files_done.load(Relaxed);
        let total_files = self.total_files.load(Relaxed);
        let chunks = self.chunks_done();

        let fraction = match phase {
            AddPhase::Streaming => two_pass_fraction(
                stream_done,
                chunk_done,
                total,
                total_files == 0 || files_done >= total_files,
            ),
            AddPhase::Planning => {
                let planned = self.plan_files_done.load(Relaxed);
                if total_files > 0 {
                    planned as f64 / total_files as f64
                } else {
                    1.0
                }
            }
            AddPhase::Flushing => 1.0,
            AddPhase::Indexing => {
                if total_files > 0 {
                    files_done as f64 / total_files as f64
                } else {
                    1.0
                }
            }
        };
        let complete = match phase {
            AddPhase::Streaming | AddPhase::Indexing => {
                total_files == 0 || files_done >= total_files
            }
            AddPhase::Planning => {
                total_files == 0 || self.plan_files_done.load(Relaxed) >= total_files
            }
            AddPhase::Flushing => true,
        };
        let display_fraction = displayed_fraction(fraction, complete);
        let pct = (display_fraction * 100.0).min(100.0);

        let pass = batch_active_pass(stream_done, total, phase, complete);
        let pass_done = match pass {
            AddPass::Streaming => stream_done.min(total),
            AddPass::Chunking => chunk_done.min(total),
        };
        let pass_fraction = single_pass_fraction(pass_done, total, complete);
        let elapsed = self.start.elapsed();
        let rate = if elapsed.as_secs_f64() > 0.0 {
            pass_done as f64 / elapsed.as_secs_f64()
        } else {
            0.0
        };

        // Each phase surfaces the metrics that matter most for that
        // stage. During streaming the bar follows the current read and
        // the text names the active work.
        let label = match phase {
            AddPhase::Streaming if complete => "Added: ",
            AddPhase::Streaming => "Adding:",
            AddPhase::Planning | AddPhase::Flushing | AddPhase::Indexing => phase.label(),
        };
        let metrics = match phase {
            AddPhase::Streaming => {
                let pass_label = pass.label();
                let pass_pct = (pass_fraction * 100.0).min(100.0);
                vec![
                    format!(
                        "({files_done}/{total_files}) {pass_label} {pass_pct:.1}% | {} / {} | {} | {chunks} chunks",
                        format_bytes(pass_done),
                        format_bytes(total),
                        format_rate(rate)
                    ),
                    format!(
                        "({files_done}/{total_files}) {pass_label} {} / {} | {chunks} chunks",
                        format_bytes(pass_done),
                        format_bytes(total)
                    ),
                    format!("({files_done}/{total_files}) {pass_label} | {chunks} chunks"),
                ]
            }
            AddPhase::Planning => {
                let planned = self.plan_files_done.load(Relaxed);
                let plan_chunks = self.plan_chunks.load(Relaxed);
                let remote_hits = self.plan_existing_candidates.load(Relaxed);
                let cache_xorbs = self.plan_prepared_cache_xorbs.load(Relaxed);
                let prepared_xorbs = self.plan_prepared_xorbs.load(Relaxed);
                let prepared_bytes = self.plan_prepared_bytes.load(Relaxed);
                let remote = if self.plan_remote_lookup.load(Relaxed) == 1 {
                    "remote on"
                } else {
                    "remote off"
                };
                vec![
                    format!(
                        "({planned}/{total_files}) {plan_chunks} chunks | {prepared_xorbs} xorbs {} | cache {cache_xorbs} | {remote}, {remote_hits} hits",
                        format_bytes(prepared_bytes)
                    ),
                    format!(
                        "({planned}/{total_files}) {prepared_xorbs} xorbs | cache {cache_xorbs} | {remote}"
                    ),
                    format!("({planned}/{total_files}) push plan | {remote}"),
                ]
            }
            AddPhase::Flushing => vec!["staging segments fsync".to_owned()],
            AddPhase::Indexing => vec![format!("({files_done}/{total_files}) writing pointers")],
        };

        fit_progress_line(
            label,
            pct,
            display_fraction,
            complete,
            color,
            width,
            &metrics,
            "",
        )
    }

    /// Spawn a background ticker that redraws the progress line on stderr.
    ///
    /// Returns `None` if stderr is not a TTY (CI, piped output) — in
    /// that case the final summary line is still printed.
    fn start_ticker(self: &Arc<Self>, cancel: &CancellationToken) -> Option<JoinHandle<()>> {
        if !is_tty() {
            return None;
        }
        let progress = Arc::clone(self);
        let cancel = cancel.clone();
        Some(tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(200));
            let mut prev_lines = 0usize;
            loop {
                tokio::select! {
                    () = cancel.cancelled() => {
                        let mut stderr = std::io::stderr().lock();
                        clear_tty_lines(&mut stderr, &mut prev_lines);
                        break;
                    }
                    _ = interval.tick() => {}
                }
                let lines = progress.render_lines(true, terminal_width());
                let mut stderr = std::io::stderr().lock();
                render_tty_lines(&mut stderr, &lines, &mut prev_lines);
            }
        }))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AddPass {
    Streaming,
    Chunking,
}

impl AddPass {
    fn label(self) -> &'static str {
        match self {
            Self::Streaming => "streaming",
            Self::Chunking => "chunking",
        }
    }
}

fn file_active_pass(stream_done: u64, total: u64, complete: bool) -> AddPass {
    if complete || (total > 0 && stream_done >= total) {
        AddPass::Chunking
    } else {
        AddPass::Streaming
    }
}

fn batch_active_pass(stream_done: u64, total: u64, phase: AddPhase, complete: bool) -> AddPass {
    if phase == AddPhase::Streaming {
        file_active_pass(stream_done, total, complete)
    } else {
        AddPass::Streaming
    }
}

fn single_pass_fraction(done: u64, total: u64, complete: bool) -> f64 {
    if complete {
        return 1.0;
    }
    if total == 0 {
        return 0.0;
    }
    (done.min(total) as f64 / total as f64).clamp(0.0, 1.0)
}

fn two_pass_fraction(stream_done: u64, chunk_done: u64, total: u64, complete: bool) -> f64 {
    if complete {
        return 1.0;
    }
    if total == 0 {
        return 0.0;
    }
    let stream = stream_done.min(total) as f64;
    let chunk = chunk_done.min(total) as f64;
    ((stream + chunk) / (total as f64 * 2.0)).clamp(0.0, 1.0)
}

fn displayed_fraction(fraction: f64, complete: bool) -> f64 {
    if complete {
        fraction
    } else {
        fraction.min(ADD_INCOMPLETE_PCT_FRACTION)
    }
}

fn render_add_bar(fraction: f64, width: usize, color: bool, complete: bool) -> String {
    let visual_fraction = if complete {
        fraction
    } else {
        // The shared renderer rounds filled cells, so high-but-incomplete
        // fractions can otherwise draw a full bar. Reserve one cell until
        // the active phase is semantically complete.
        let max_incomplete = width.saturating_sub(1) as f64 / width as f64;
        fraction.min(max_incomplete)
    };
    render_bar(visual_fraction, width, color)
}

fn fit_progress_line(
    label: &str,
    pct: f64,
    fraction: f64,
    complete: bool,
    color: bool,
    width: usize,
    metrics: &[String],
    name: &str,
) -> String {
    let width = width.max(1);
    for metric in metrics {
        for bar_width in (ADD_PROGRESS_BAR_MIN_WIDTH..=ADD_PROGRESS_BAR_WIDTH).rev() {
            let bar = render_add_bar(fraction, bar_width, color, complete);
            let mut line = format!("{label} {pct:5.1}% {bar}");
            if !metric.is_empty() {
                line.push(' ');
                line.push_str(metric);
            }
            if !append_name_to_fit(&mut line, name, width) {
                continue;
            }
            if visible_width(&line) <= width {
                return line;
            }
        }
    }

    let fallback = format!("{label} {pct:5.1}%");
    truncate_start(&fallback, width)
}

fn append_name_to_fit(line: &mut String, name: &str, width: usize) -> bool {
    if name.is_empty() {
        return true;
    }

    let used = visible_width(line);
    if used + 2 > width {
        return false;
    }

    let budget = width - used - 1;
    let min_name_width = name.chars().count().min(ADD_PROGRESS_NAME_MIN_WIDTH);
    if budget < min_name_width {
        return false;
    }
    line.push(' ');
    line.push_str(&truncate_start(name, budget));
    true
}

fn truncate_start(s: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let len = s.chars().count();
    if len <= width {
        return s.to_owned();
    }
    if width == 1 {
        return "…".to_owned();
    }

    let suffix_len = width - 1;
    let suffix: String = s
        .chars()
        .rev()
        .take(suffix_len)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("…{suffix}")
}

fn visible_width(s: &str) -> usize {
    let mut width = 0usize;
    let mut chars = s.chars();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            for esc in chars.by_ref() {
                if esc.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        width += 1;
    }
    width
}

fn terminal_width() -> usize {
    #[cfg(unix)]
    {
        // SAFETY: `ioctl(TIOCGWINSZ)` only writes to the provided winsize
        // struct for the valid stderr file descriptor.
        unsafe {
            let mut size: libc::winsize = std::mem::zeroed();
            if libc::ioctl(libc::STDERR_FILENO, libc::TIOCGWINSZ, &mut size) == 0 && size.ws_col > 0
            {
                return size.ws_col as usize;
            }
        }
    }

    std::env::var("COLUMNS")
        .ok()
        .and_then(|cols| cols.parse::<usize>().ok())
        .filter(|cols| *cols > 0)
        .unwrap_or(DEFAULT_TERMINAL_WIDTH)
}

fn render_tty_lines<W: Write>(writer: &mut W, lines: &[String], prev_lines: &mut usize) {
    if *prev_lines > 1 {
        for _ in 1..*prev_lines {
            let _ = write!(writer, "\x1b[A");
        }
    }

    let rows = (*prev_lines).max(lines.len());
    for row in 0..rows {
        if row > 0 {
            let _ = writeln!(writer);
        }
        let _ = write!(writer, "\r\x1b[2K");
        if let Some(line) = lines.get(row) {
            let _ = write!(writer, "{line}");
        }
    }

    if *prev_lines > lines.len() {
        for _ in 0..(*prev_lines - lines.len()) {
            let _ = write!(writer, "\x1b[A");
        }
    }

    *prev_lines = lines.len();
    let _ = writer.flush();
}

fn clear_tty_lines<W: Write>(writer: &mut W, prev_lines: &mut usize) {
    if *prev_lines == 0 {
        return;
    }

    if *prev_lines > 1 {
        for _ in 1..*prev_lines {
            let _ = write!(writer, "\x1b[A");
        }
    }

    for row in 0..*prev_lines {
        if row > 0 {
            let _ = writeln!(writer);
        }
        let _ = write!(writer, "\r\x1b[2K");
    }

    *prev_lines = 0;
    let _ = writer.flush();
}

/// Run the `crab add` command.
///
/// Discovers the repo root, resolves patterns against crab-tracked
/// files, processes matching files in parallel, and updates git's index.
pub async fn run_add(args: &AddArgs, cancel: &CancellationToken) -> Result<()> {
    execute_add(args, cancel, true).await.map(|_| ())
}

/// Run add without emitting its terminal result.
///
/// Composite commands use this so the outer command owns the sole terminal
/// machine-readable envelope.
pub(crate) async fn run_add_without_terminal_output(
    args: &AddArgs,
    cancel: &CancellationToken,
) -> Result<AddSummary> {
    execute_add(args, cancel, false).await
}

async fn execute_add(
    args: &AddArgs,
    cancel: &CancellationToken,
    emit_terminal: bool,
) -> Result<AddSummary> {
    let start = Instant::now();

    let worktree_ctx = crate::git::worktree::WorktreeContext::resolve()?;
    let repo_root = worktree_ctx.current_worktree_root.clone();

    // Open the consolidated .gitattributes classifier (gix_attributes
    // under the `gix-pathmatch` feature, simple-suffix legacy otherwise).
    let mut generated_tracking_patterns = Vec::new();
    let mut classifier = TrackedClassifier::open(&repo_root)?;
    if classifier.is_empty() {
        if args.dry_run {
            if !args.mode.is_machine() {
                println!("No crab-tracked patterns in .gitattributes; dry-run made no changes.");
            }
            return Ok(empty_add_summary(start));
        }
        // Approach 2: Auto-track large files when no patterns exist yet.
        // Instead of failing with "run crab track first", scan the working
        // tree and auto-track extensions of files above the size threshold.
        if !args.mode.is_machine() {
            eprintln!("No crab-tracked patterns in .gitattributes. Scanning for large files...");
        }
        generated_tracking_patterns = crate::cmd::init::auto_track_large_files(&repo_root)?;
        if generated_tracking_patterns.is_empty() {
            if !args.mode.is_machine() {
                println!(
                    "No crab-tracked patterns in .gitattributes and no large files found.\n\
                     Run `crab track <glob>` to configure tracking patterns."
                );
            }
            return Ok(empty_add_summary(start));
        }
        // Re-open the classifier now that we've added patterns.
        classifier = TrackedClassifier::open(&repo_root)?;
        if classifier.is_empty() {
            if !args.mode.is_machine() {
                println!(
                    "No crab-tracked patterns in .gitattributes. Run `crab track <glob>` first."
                );
            }
            return Ok(empty_add_summary(start));
        }
    }

    // Build the user's pattern filter.
    let filter = build_filter(&args.patterns, &[])?;

    // Walk the working tree and collect files to process.
    let candidates = collect_candidates(&repo_root, &classifier, &filter, cancel)?;

    if candidates.is_empty() {
        if args.dry_run {
            if !args.mode.is_machine() {
                println!("No matching files found; dry-run made no changes.");
            }
            return Ok(empty_add_summary(start));
        }
        if !args.mode.is_machine() {
            // Provide actionable diagnostics: figure out WHY nothing matched.
            let untracked_exts =
                diagnose_untracked_extensions(&repo_root, &classifier, &args.patterns);
            if untracked_exts.is_empty() {
                println!("No matching files found.");
            } else {
                // Approach 2 (continued): Auto-track the discovered extensions
                // rather than just printing instructions.
                eprintln!(
                    "Found {} untracked extension(s) matching your patterns. Auto-tracking...",
                    untracked_exts.len()
                );
                for ext in &untracked_exts {
                    let pattern = format!("*.{ext}");
                    if let Err(e) = crate::cmd::track::run_track_in(&pattern, &repo_root) {
                        warn!(ext = %ext, error = %e, "failed to auto-track extension");
                    } else {
                        generated_tracking_patterns.push(pattern);
                    }
                }
                eprintln!(
                    "Tracked: {}",
                    untracked_exts
                        .iter()
                        .map(|e| format!("*.{e}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                eprintln!("Re-run `crab add` to stage the matching files.");
            }
        }
        if !args.skip_git_add && !generated_tracking_patterns.is_empty() {
            publish_generated_tracking_rules(&repo_root, &generated_tracking_patterns)?;
        }
        return Ok(empty_add_summary(start));
    }

    if args.dry_run {
        if !args.mode.is_machine() {
            println!("Would add {} file(s):", candidates.len());
            for (path, size) in &candidates {
                let rel = path.strip_prefix(&repo_root).unwrap_or(path);
                println!("  {} ({})", rel.display(), format_bytes(*size));
            }
        }
        return Ok(empty_add_summary(start));
    }

    let total_candidate_files = candidates.len() as u64;
    let total_candidate_bytes: u64 = candidates.iter().map(|(_, s)| *s).sum();
    let jobs = effective_add_jobs(args.jobs);

    // Acquire ownership before consulting the index so a queued add observes
    // pointers published by the process that just released the staging lock.
    let staging_root = worktree_ctx.shared_staging_dir();
    let lock_wait_start = Instant::now();
    let staging = Arc::new(open_add_staging(&staging_root, &repo_root).await?);
    let lock_wait_duration_ms = lock_wait_start.elapsed().as_millis() as u64;
    let (candidates, clean_skipped) =
        filter_clean_indexed_candidates(&repo_root, candidates, jobs, cancel).await?;

    if jobs != args.jobs {
        warn!(
            configured_jobs = args.jobs,
            jobs, "clamped add jobs to avoid stalled processing"
        );
    }

    info!(
        files = candidates.len(),
        skipped_clean = clean_skipped.files,
        jobs,
        "starting parallel add"
    );

    let total_files = candidates.len() as u64;
    let total_bytes: u64 = candidates.iter().map(|(_, s)| *s).sum();
    let mut execution_plans = add_execution_plans(&candidates, total_bytes);
    if let Some(plan) = execution_plans.stream_xorb_plan.as_mut() {
        plan.bind_preparation(staging.create_add_preparation()?);
    }
    execution_plans.remote_classifier = open_add_remote_classifier(cancel).await;

    // Build the optional JSONL stream for streaming mode.
    let jsonl_stream: Option<Arc<Mutex<JsonlStream<Stdout>>>> = match args.mode {
        OutputMode::Jsonl if emit_terminal => Some(Arc::new(Mutex::new(JsonlStream::new(
            "add.event",
            "1.0",
            std::io::stdout(),
        )))),
        _ => None,
    };

    // Live progress: each file task owns a stable progress slot, drawn by
    // a background ticker on stderr when attached to a TTY.
    let progress_specs = candidates
        .iter()
        .map(|(path, size)| AddFileProgressSpec {
            name: path
                .strip_prefix(&repo_root)
                .unwrap_or(path)
                .to_string_lossy()
                .into_owned(),
            total_bytes: *size,
        })
        .collect();
    let progress = Arc::new(AddProgress::new(progress_specs));
    let ticker_cancel = CancellationToken::new();
    let mut ticker = if args.mode == OutputMode::Text {
        progress.start_ticker(&ticker_cancel)
    } else {
        None
    };

    // Process files concurrently with bounded parallelism.
    let semaphore = Arc::new(tokio::sync::Semaphore::new(jobs));
    let mut pending_tasks = FuturesUnordered::new();
    let mut deferred_duplicates_by_representative =
        HashMap::<PathBuf, Vec<(usize, PathBuf)>>::new();
    let mut duplicate_representatives = HashSet::<PathBuf>::new();
    let staging_phase_start = Instant::now();

    for (index, (abs_path, _size)) in candidates.into_iter().enumerate() {
        if let Some(representative) = execution_plans.duplicate_plan.representative_for(&abs_path) {
            duplicate_representatives.insert(representative.to_path_buf());
            deferred_duplicates_by_representative
                .entry(representative.to_path_buf())
                .or_default()
                .push((index, abs_path));
            continue;
        }

        let stream_xorb_builder = execution_plans
            .stream_xorb_plan
            .as_ref()
            .and_then(|plan| plan.builder_for(&abs_path));
        let existing_lookup = execution_plans
            .remote_classifier
            .as_ref()
            .map(|classifier| {
                Arc::clone(classifier) as Arc<dyn crab_staging::push_plan::ExistingChunkLookup>
            });
        let file_progress = progress.file_progress(index).ok_or_else(|| {
            CrabError::Internal(format!("missing add progress slot for file #{index}"))
        })?;
        let handle = spawn_primary_add_task(
            Arc::clone(&semaphore),
            Arc::clone(&staging),
            cancel.clone(),
            repo_root.clone(),
            Arc::clone(&progress),
            Arc::clone(&file_progress),
            abs_path,
            stream_xorb_builder,
            existing_lookup,
        );
        pending_tasks.push(join_add_task(file_progress, handle));
    }

    // Collect results.
    let mut summary = AddSummary {
        files_staged: 0,
        files_skipped: clean_skipped.files,
        validation_cache_hits: clean_skipped.validation_cache_hits,
        validation_cache_hit_bytes: clean_skipped.validation_cache_hit_bytes,
        files_failed: 0,
        chunks_staged: 0,
        bytes_processed: 0,
        lock_wait_duration_ms,
        chunking_worker_duration_ms: 0,
        remote_lookup_duration_ms: 0,
        compression_worker_duration_ms: 0,
        payload_write_duration_ms: 0,
        staging_duration_ms: 0,
        planning_duration_ms: 0,
        flushing_duration_ms: 0,
        indexing_duration_ms: 0,
        duration_ms: 0,
    };
    // Entries used by the indexing phase to write pointer blobs directly
    // into git's ODB + index without going through `git add` / the clean filter.
    let mut staged_entries: Vec<StagedEntry> = Vec::new();
    let mut bytes_done: u64 = clean_skipped.bytes;
    let accounting = AddResultAccounting {
        repo_root: &repo_root,
        total_candidate_files,
        total_candidate_bytes,
        start,
        jsonl_stream: jsonl_stream.as_ref(),
    };

    while let Some((file_progress, joined)) = pending_tasks.next().await {
        if let Some((representative_path, reusable)) = record_add_task_join(
            file_progress,
            joined,
            &progress,
            &accounting,
            &mut summary,
            &mut bytes_done,
            &mut staged_entries,
            &duplicate_representatives,
        ) {
            if let Some(waiting) =
                deferred_duplicates_by_representative.remove(&representative_path)
            {
                for (index, abs_path) in waiting {
                    let stream_xorb_builder = execution_plans
                        .stream_xorb_plan
                        .as_ref()
                        .and_then(|plan| plan.builder_for(&abs_path));
                    let existing_lookup =
                        execution_plans
                            .remote_classifier
                            .as_ref()
                            .map(|classifier| {
                                Arc::clone(classifier)
                                    as Arc<dyn crab_staging::push_plan::ExistingChunkLookup>
                            });
                    let file_progress = progress.file_progress(index).ok_or_else(|| {
                        CrabError::Internal(format!("missing add progress slot for file #{index}"))
                    })?;
                    let handle = spawn_duplicate_add_task(
                        Arc::clone(&semaphore),
                        Arc::clone(&staging),
                        cancel.clone(),
                        repo_root.clone(),
                        Arc::clone(&progress),
                        Arc::clone(&file_progress),
                        abs_path,
                        Some(reusable.clone()),
                        stream_xorb_builder,
                        existing_lookup,
                    );
                    pending_tasks.push(join_add_task(file_progress, handle));
                }
            }
        }
    }

    for waiting in deferred_duplicates_by_representative.into_values() {
        for (index, abs_path) in waiting {
            let stream_xorb_builder = execution_plans
                .stream_xorb_plan
                .as_ref()
                .and_then(|plan| plan.builder_for(&abs_path));
            let existing_lookup = execution_plans
                .remote_classifier
                .as_ref()
                .map(|classifier| {
                    Arc::clone(classifier) as Arc<dyn crab_staging::push_plan::ExistingChunkLookup>
                });
            let file_progress = progress.file_progress(index).ok_or_else(|| {
                CrabError::Internal(format!("missing add progress slot for file #{index}"))
            })?;
            let handle = spawn_duplicate_add_task(
                Arc::clone(&semaphore),
                Arc::clone(&staging),
                cancel.clone(),
                repo_root.clone(),
                Arc::clone(&progress),
                Arc::clone(&file_progress),
                abs_path,
                None,
                stream_xorb_builder,
                existing_lookup,
            );
            pending_tasks.push(join_add_task(file_progress, handle));
        }
    }

    while let Some((file_progress, joined)) = pending_tasks.next().await {
        let _ = record_add_task_join(
            file_progress,
            joined,
            &progress,
            &accounting,
            &mut summary,
            &mut bytes_done,
            &mut staged_entries,
            &duplicate_representatives,
        );
    }
    if let Some(classifier) = execution_plans.remote_classifier.take() {
        classifier.close().await;
    }
    summary.staging_duration_ms = staging_phase_start.elapsed().as_millis() as u64;

    if let Some(preparation_id) = execution_plans
        .stream_xorb_plan
        .as_ref()
        .and_then(StreamPreparedXorbPlan::preparation_id)
    {
        if summary.files_failed > 0 || cancel.is_cancelled() {
            if let Err(error) = staging.abort_add_preparation(preparation_id) {
                stop_progress_ticker(ticker.take(), &ticker_cancel).await;
                return Err(error.into());
            }
        } else if let Err(error) = staging.finalize_add_preparation(preparation_id) {
            let _ = staging.abort_add_preparation(preparation_id);
            if let Err(cleanup_error) = rollback_unpublished_open_entries(
                &staging,
                &mut summary,
                &mut staged_entries,
                "failed to finalize canonical prepared ownership",
            ) {
                stop_progress_ticker(ticker.take(), &ticker_cancel).await;
                return Err(cleanup_error);
            }
            stop_progress_ticker(ticker.take(), &ticker_cancel).await;
            return Err(error.into());
        }
    }

    if summary.files_failed > 0
        && let Err(e) = rollback_unpublished_open_entries(
            &staging,
            &mut summary,
            &mut staged_entries,
            "add failed before Git index publication",
        )
    {
        stop_progress_ticker(ticker.take(), &ticker_cancel).await;
        return Err(e);
    }

    if let Err(e) =
        abort_if_cancelled_before_indexing(cancel, &staging, &mut summary, &mut staged_entries)
    {
        stop_progress_ticker(ticker.take(), &ticker_cancel).await;
        return Err(e);
    }

    if !staged_entries.is_empty() {
        progress.set_phase(AddPhase::Planning);
        let plan_progress = Arc::clone(&progress);
        let plan_jsonl_stream = jsonl_stream.clone();
        let plan_start = Instant::now();
        let mut on_plan_progress = |plan_summary: &crab_staging::push_plan::AddPushPlanSummary| {
            plan_progress.update_plan_summary(plan_summary);
            if let Some(ref stream) = plan_jsonl_stream
                && let Ok(mut s) = stream.lock()
            {
                let elapsed = plan_start.elapsed();
                let rate = if elapsed.as_secs_f64() > 0.0 {
                    plan_summary.prepared_bytes as f64 / elapsed.as_secs_f64()
                } else {
                    0.0
                };
                s.emit_progress(ProgressPayload {
                    operation: "push-plan".to_owned(),
                    current: plan_summary.files,
                    total: total_files,
                    bytes: plan_summary.prepared_bytes,
                    total_bytes,
                    rate_bytes_per_sec: rate,
                    xorbs_produced: Some(plan_summary.prepared_xorbs),
                });
            }
        };
        let plan_result = if can_use_stream_prepared_plans(&staged_entries) {
            Some(
                write_stream_prepared_push_plans(&staging, &staged_entries, &mut on_plan_progress)
                    .await,
            )
        } else {
            debug!(
                files = staged_entries.len(),
                "add push-plan: deferring non-prepared files to bounded push planning"
            );
            None
        };
        if let Some(Err(e)) = plan_result {
            cleanup_stream_prepared_entries(&staged_entries);
            if let Err(cleanup_err) = rollback_unpublished_open_entries(
                &staging,
                &mut summary,
                &mut staged_entries,
                "add push-plan preparation failed before Git index publication",
            ) {
                stop_progress_ticker(ticker.take(), &ticker_cancel).await;
                return Err(cleanup_err);
            }
            stop_progress_ticker(ticker.take(), &ticker_cancel).await;
            return Err(e);
        }
        summary.planning_duration_ms = plan_start.elapsed().as_millis() as u64;
        cleanup_stream_prepared_entries(&staged_entries);
        for entry in &mut staged_entries {
            entry.prepared_xorbs = Vec::new();
        }
    }

    let publish_git_index = should_publish_git_index(args, &summary, staged_entries.len());
    if publish_git_index
        && let Err(error) =
            pin_committed_recipes_before_path_replacement(&staging, &repo_root, &staged_entries)
    {
        if let Err(cleanup_error) = rollback_unpublished_open_entries(
            &staging,
            &mut summary,
            &mut staged_entries,
            "failed to preserve committed recipes before Git index publication",
        ) {
            stop_progress_ticker(ticker.take(), &ticker_cancel).await;
            return Err(cleanup_error);
        }
        stop_progress_ticker(ticker.take(), &ticker_cancel).await;
        return Err(error);
    }

    // Close the staging area before publishing pointers into Git's index.
    let flushing_start = Instant::now();
    progress.set_phase(AddPhase::Flushing);
    if let Err(e) = close_staging_before_indexing(staging).await {
        stop_progress_ticker(ticker.take(), &ticker_cancel).await;
        return Err(e);
    }
    if let Err(e) = abort_if_cancelled_after_staging_close(
        cancel,
        &staging_root,
        &mut summary,
        &mut staged_entries,
    )
    .await
    {
        stop_progress_ticker(ticker.take(), &ticker_cancel).await;
        return Err(e);
    }
    summary.flushing_duration_ms = flushing_start.elapsed().as_millis() as u64;

    // Write pointer blobs directly into git's ODB + index, bypassing
    // `git add` and its clean-filter round-trip. For large batches this
    // is the dominant post-chunk cost, and doing it ourselves lets the
    // live bar tick per file instead of waiting on an opaque subprocess.
    if publish_git_index {
        let indexing_start = Instant::now();
        progress.set_phase(AddPhase::Indexing);
        // Re-purpose `files_done` for the indexing bar: starts at 0,
        // ticks up to `total_files` as each pointer is staged.
        progress.files_done.store(0, Relaxed);

        let publication_staging = StagingAreaReadOnly::open(staging_root.clone()).await?;
        let index_entries = staged_entries
            .iter()
            .map(IndexPublicationEntry::from)
            .collect::<Vec<_>>();
        let index_repo_root = repo_root.clone();
        let tracking_patterns = generated_tracking_patterns.clone();
        let progress_cb = Arc::clone(&progress);
        let index_result = tokio::task::spawn_blocking(move || {
            let shard_hints = crate::cache::ShardHintCache::load_sync(
                &crate::cache::shard_hints::default_path(),
            )
            .unwrap_or_else(|err| {
                debug!(error = %err, "failed to load shard-hint cache; pointers will omit hints");
                crate::cache::ShardHintCache::new()
            });
            let mut publication_intent = None;
            let write_result = write_pointers_and_tracking_to_git_index(
                &index_entries,
                &index_repo_root,
                &shard_hints,
                &tracking_patterns,
                |git_entries, current_index| {
                    let entries = index_entries
                        .iter()
                        .zip(git_entries)
                        .map(|(staged, indexed)| {
                            let batch_id = staged.batch_id.clone().ok_or_else(|| {
                                CrabError::StagingCorrupt(format!(
                                    "staged path {} has no publication batch",
                                    staged.abs_path.display()
                                ))
                            })?;
                            let path = staged
                                .abs_path
                                .strip_prefix(&index_repo_root)
                                .unwrap_or(&staged.abs_path)
                                .to_path_buf();
                            let index_path = git_index_path_bstring(&path);
                            Ok(PublicationIntentEntry {
                                batch_id,
                                path,
                                expected_pointer_oid: indexed.sha.clone(),
                                previous_index_state: git_index_path_state(
                                    current_index,
                                    index_path.as_bstr(),
                                ),
                            })
                        })
                        .collect::<Result<Vec<_>>>()?;
                    publication_intent =
                        Some(publication_staging.create_publication_intent(&entries)?);
                    Ok(())
                },
                || {
                    progress_cb.files_done.fetch_add(1, Relaxed);
                },
            );
            if let Err(error) = write_result {
                return Err((error, publication_intent));
            }
            let Some(publication_intent) = publication_intent else {
                return Err((
                    GitIndexWriteError::IndexMutationUncertain(CrabError::Internal(
                        "Git index publication intent was not recorded".to_owned(),
                    )),
                    None,
                ));
            };
            if let Err(error) = publication_staging.publish_publication_intent(&publication_intent)
            {
                return Err((
                    GitIndexWriteError::IndexMutationUncertain(error.into()),
                    Some(publication_intent),
                ));
            }

            let verified_paths = index_entries
                .iter()
                .filter_map(|entry| {
                    entry.index_stat.map(|index_stat| VerifiedPath {
                        path: entry.abs_path.clone(),
                        file_hash: entry.file_hash,
                        size: entry.size,
                        index_stat,
                    })
                })
                .collect::<Vec<_>>();
            match crate::cache::add_validation::record_verified_paths(
                &index_repo_root,
                &verified_paths,
            ) {
                Ok(validations) => debug!(validations, "recorded verified add validations"),
                Err(error) => debug!(
                    error = %error,
                    "failed to persist verified add validations; later add will rehash"
                ),
            }
            Ok(())
        })
        .await;
        let index_error = match index_result {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(error),
            Err(error) => Some((
                GitIndexWriteError::IndexMutationUncertain(CrabError::Io(std::io::Error::other(
                    error,
                ))),
                None,
            )),
        };
        if let Some((error, publication_intent)) = index_error {
            let error = handle_git_index_write_error(
                &staging_root,
                &staged_entries,
                publication_intent.as_ref(),
                error,
            )
            .await;
            stop_progress_ticker(ticker.take(), &ticker_cancel).await;
            return Err(error);
        }
        summary.indexing_duration_ms = indexing_start.elapsed().as_millis() as u64;
    }

    // Stop the live progress ticker before any line-oriented summary
    // output so the final text doesn't interleave with bar redraws.
    stop_progress_ticker(ticker.take(), &ticker_cancel).await;

    let elapsed = start.elapsed();
    summary.duration_ms = elapsed.as_millis() as u64;

    if summary.files_failed > 0 {
        return Err(CrabError::Internal(format!(
            "{} file(s) failed during add",
            summary.files_failed,
        )));
    }

    if emit_terminal {
        match args.mode {
            OutputMode::Text => {
                println!(
                    "Added {} file(s) ({}, {} chunks) in {:.1}s",
                    summary.files_staged,
                    format_bytes(summary.bytes_processed),
                    summary.chunks_staged,
                    elapsed.as_secs_f64(),
                );
                if summary.files_skipped > 0 {
                    println!(
                        "  {} skipped (already staged and clean)",
                        summary.files_skipped
                    );
                    if summary.validation_cache_hits > 0 {
                        println!(
                            "    {} validation-cache hit(s), {} of file reads avoided",
                            summary.validation_cache_hits,
                            format_bytes(summary.validation_cache_hit_bytes)
                        );
                    }
                }
                if summary.files_failed > 0 {
                    println!("  {} failed", summary.files_failed);
                }
            }
            OutputMode::Json => {
                emit_json("add", "1.0", &summary);
            }
            OutputMode::Jsonl => {
                if let Some(ref stream) = jsonl_stream
                    && let Ok(mut s) = stream.lock()
                {
                    s.emit_result(&summary);
                }
            }
        }
    }

    Ok(summary)
}

async fn open_add_staging(staging_root: &Path, repo_root: &Path) -> Result<StagingArea> {
    let staging =
        StagingArea::open_blocking(staging_root.to_path_buf(), ADD_STAGING_LOCK_WAIT_BUDGET)
            .await
            .map_err(CrabError::from)?;
    reconcile_publication_intents_writer(&staging, repo_root)?;
    Ok(staging)
}

fn empty_add_summary(start: Instant) -> AddSummary {
    AddSummary {
        duration_ms: start.elapsed().as_millis() as u64,
        ..AddSummary::default()
    }
}

struct AddResultAccounting<'a> {
    repo_root: &'a Path,
    total_candidate_files: u64,
    total_candidate_bytes: u64,
    start: Instant,
    jsonl_stream: Option<&'a Arc<Mutex<JsonlStream<Stdout>>>>,
}

fn record_successful_file_result(
    result: FileResult,
    accounting: &AddResultAccounting<'_>,
    summary: &mut AddSummary,
    bytes_done: &mut u64,
    staged_entries: &mut Vec<StagedEntry>,
) {
    let rel_path = result
        .abs_path
        .strip_prefix(accounting.repo_root)
        .unwrap_or(&result.abs_path);

    summary.files_staged += 1;
    summary.chunks_staged += result.chunks as u64;
    summary.bytes_processed += result.size;
    summary.chunking_worker_duration_ms = summary
        .chunking_worker_duration_ms
        .saturating_add(result.timings.chunking_duration_ms);
    summary.remote_lookup_duration_ms = summary
        .remote_lookup_duration_ms
        .saturating_add(result.timings.remote_lookup_duration_ms);
    summary.compression_worker_duration_ms = summary
        .compression_worker_duration_ms
        .saturating_add(result.timings.compression_duration_ms);
    summary.payload_write_duration_ms = summary
        .payload_write_duration_ms
        .saturating_add(result.timings.payload_write_duration_ms);
    *bytes_done += result.size;

    if let Some(stream) = accounting.jsonl_stream
        && let Ok(mut s) = stream.lock()
    {
        s.emit_file_done(FileDonePayload {
            path: rel_path.to_string_lossy().into_owned(),
            bytes: result.size,
            duration_ms: result.duration_ms,
            status: "ok".to_owned(),
        });

        let elapsed = accounting.start.elapsed();
        let rate = if elapsed.as_secs_f64() > 0.0 {
            *bytes_done as f64 / elapsed.as_secs_f64()
        } else {
            0.0
        };
        s.emit_progress(ProgressPayload {
            operation: "staging".to_owned(),
            current: summary.files_staged + summary.files_skipped + summary.files_failed,
            total: accounting.total_candidate_files,
            bytes: *bytes_done,
            total_bytes: accounting.total_candidate_bytes,
            rate_bytes_per_sec: rate,
            xorbs_produced: None,
        });
    }

    staged_entries.push(StagedEntry {
        batch_id: Some(result.batch_id),
        abs_path: result.abs_path,
        file_hash: result.file_hash,
        size: result.size,
        recipe: result.recipe,
        prepared_xorbs: result.prepared_xorbs,
        index_stat: result.index_stat,
    });
}

fn spawn_primary_add_task(
    sem: Arc<tokio::sync::Semaphore>,
    staging: Arc<StagingArea>,
    cancel: CancellationToken,
    repo_root: PathBuf,
    progress: Arc<AddProgress>,
    file_progress: Arc<AddFileProgress>,
    abs_path: PathBuf,
    stream_xorb_builder: Option<crate::cmd::stream_stage::StreamStageXorbBuilder>,
    existing_lookup: Option<Arc<dyn crab_staging::push_plan::ExistingChunkLookup>>,
) -> JoinHandle<Result<FileResult>> {
    tokio::spawn(async move {
        let _permit = sem
            .acquire()
            .await
            .map_err(|e| CrabError::Internal(format!("semaphore closed: {e}")))?;

        error::check_cancelled(&cancel)?;

        process_file(
            &abs_path,
            &repo_root,
            &staging,
            &progress,
            &file_progress,
            stream_xorb_builder,
            existing_lookup,
            &cancel,
        )
        .await
    })
}

fn spawn_duplicate_add_task(
    sem: Arc<tokio::sync::Semaphore>,
    staging: Arc<StagingArea>,
    cancel: CancellationToken,
    repo_root: PathBuf,
    progress: Arc<AddProgress>,
    file_progress: Arc<AddFileProgress>,
    abs_path: PathBuf,
    reusable: Option<ReusableStagedFile>,
    stream_xorb_builder: Option<crate::cmd::stream_stage::StreamStageXorbBuilder>,
    existing_lookup: Option<Arc<dyn crab_staging::push_plan::ExistingChunkLookup>>,
) -> JoinHandle<Result<FileResult>> {
    tokio::spawn(async move {
        let _permit = sem
            .acquire()
            .await
            .map_err(|e| CrabError::Internal(format!("semaphore closed: {e}")))?;

        error::check_cancelled(&cancel)?;

        process_duplicate_candidate(
            &abs_path,
            &repo_root,
            &staging,
            &progress,
            &file_progress,
            reusable,
            stream_xorb_builder,
            existing_lookup,
            &cancel,
        )
        .await
    })
}

async fn join_add_task(
    file_progress: Arc<AddFileProgress>,
    handle: JoinHandle<Result<FileResult>>,
) -> (
    Arc<AddFileProgress>,
    std::result::Result<Result<FileResult>, tokio::task::JoinError>,
) {
    (file_progress, handle.await)
}

fn record_add_task_join(
    file_progress: Arc<AddFileProgress>,
    joined: std::result::Result<Result<FileResult>, tokio::task::JoinError>,
    progress: &AddProgress,
    accounting: &AddResultAccounting<'_>,
    summary: &mut AddSummary,
    bytes_done: &mut u64,
    staged_entries: &mut Vec<StagedEntry>,
    duplicate_representatives: &HashSet<PathBuf>,
) -> Option<(PathBuf, ReusableStagedFile)> {
    match joined {
        Ok(Ok(result)) => {
            let reusable = duplicate_representatives
                .contains(&result.abs_path)
                .then(|| {
                    (
                        result.abs_path.clone(),
                        ReusableStagedFile {
                            file_hash: result.file_hash,
                            size: result.size,
                            recipe: result.recipe.clone(),
                        },
                    )
                });
            record_successful_file_result(result, accounting, summary, bytes_done, staged_entries);
            reusable
        }
        Ok(Err(e)) => {
            warn!(error = %e, "failed to process file");
            summary.files_failed += 1;
            None
        }
        Err(e) => {
            warn!(error = %e, "file processing task panicked");
            summary.files_failed += 1;
            file_progress.set_state(AddFileState::Failed);
            progress.files_done.fetch_add(1, Relaxed);
            None
        }
    }
}

/// Process a single file: stream hash, stream CDC, stage chunks.
///
/// The working tree file is left untouched. After every file has
/// staged successfully and staging has been flushed, `run_add` writes
/// pointer blobs directly into Git's object database and index while
/// the user keeps the original file visible in the working tree.
///
/// Uses the shared streaming helper so peak memory is bounded
/// by read buffers and one small staging batch rather than file size.
async fn process_file(
    abs_path: &Path,
    repo_root: &Path,
    staging: &StagingArea,
    progress: &Arc<AddProgress>,
    file_progress: &Arc<AddFileProgress>,
    xorb_builder: Option<crate::cmd::stream_stage::StreamStageXorbBuilder>,
    existing_lookup: Option<Arc<dyn crab_staging::push_plan::ExistingChunkLookup>>,
    cancel: &CancellationToken,
) -> Result<FileResult> {
    file_progress.set_state(AddFileState::Running);

    let result = match crate::cmd::stream_stage::stage_file_streaming(
        abs_path,
        repo_root,
        staging,
        crate::cmd::stream_stage::StreamStageProgress {
            bytes_done: Some(Arc::clone(&file_progress.bytes_done)),
            chunk_bytes_done: Some(Arc::clone(&file_progress.chunk_bytes_done)),
            chunks_done: Some(Arc::clone(&file_progress.chunks_done)),
            xorb_builder,
            existing_lookup,
        },
        cancel,
    )
    .await
    {
        Ok(result) => result,
        Err(err) => {
            file_progress.set_state(AddFileState::Failed);
            progress.files_done.fetch_add(1, Relaxed);
            return Err(err.into());
        }
    };

    // Publish file-level completion. The helper has already advanced
    // the byte counters and the CDC chunk counter.
    file_progress.set_state(AddFileState::Done);
    progress.files_done.fetch_add(1, Relaxed);

    let rel_path = result
        .abs_path
        .strip_prefix(repo_root)
        .unwrap_or(&result.abs_path);
    debug!(
        path = %rel_path.display(),
        size = result.size,
        chunks = result.chunks,
        "staged chunks, working tree file untouched"
    );

    Ok(FileResult {
        batch_id: result.batch_id,
        abs_path: result.abs_path,
        chunks: result.chunks,
        size: result.size,
        file_hash: result.file_hash,
        recipe: result.recipe,
        prepared_xorbs: result.prepared_xorbs,
        index_stat: result.index_stat,
        timings: result.timings,
        duration_ms: result.duration_ms,
    })
}

async fn process_duplicate_candidate(
    abs_path: &Path,
    repo_root: &Path,
    staging: &StagingArea,
    progress: &Arc<AddProgress>,
    file_progress: &Arc<AddFileProgress>,
    reusable: Option<ReusableStagedFile>,
    fallback_xorb_builder: Option<crate::cmd::stream_stage::StreamStageXorbBuilder>,
    existing_lookup: Option<Arc<dyn crab_staging::push_plan::ExistingChunkLookup>>,
    cancel: &CancellationToken,
) -> Result<FileResult> {
    let Some(reusable) = reusable else {
        return process_file(
            abs_path,
            repo_root,
            staging,
            progress,
            file_progress,
            fallback_xorb_builder,
            existing_lookup,
            cancel,
        )
        .await;
    };

    file_progress.set_state(AddFileState::Running);
    let start = Instant::now();
    let hash_result = crate::cmd::stream_stage::stream_hash_file(
        abs_path,
        Some(&file_progress.bytes_done),
        cancel,
    )
    .await;
    let verified = match hash_result {
        Ok(result) => result,
        Err(err) => {
            file_progress.set_state(AddFileState::Failed);
            progress.files_done.fetch_add(1, Relaxed);
            return Err(err.into());
        }
    };
    let file_hash = verified.hash;
    let size = verified.size;

    if file_hash != reusable.file_hash || size != reusable.size {
        file_progress.bytes_done.store(0, Relaxed);
        file_progress.chunk_bytes_done.store(0, Relaxed);
        file_progress.chunks_done.store(0, Relaxed);
        return process_file(
            abs_path,
            repo_root,
            staging,
            progress,
            file_progress,
            fallback_xorb_builder,
            existing_lookup,
            cancel,
        )
        .await;
    }

    let file_merkle = MerkleHash::from(file_hash);
    let rel_path = abs_path.strip_prefix(repo_root).unwrap_or(abs_path);
    let batch_id = staging.create_batch()?;
    if let Some(preparation_id) = fallback_xorb_builder
        .as_ref()
        .and_then(crate::cmd::stream_stage::StreamStageXorbBuilder::preparation_id)
        && let Err(error) = staging.attach_add_preparation_batch(preparation_id, &batch_id)
    {
        let _ = staging.rollback_batch(&batch_id);
        return Err(error.into());
    }
    let recipe = reusable.recipe.clone();
    if let Err(error) = staging.record_verified_recipe_lease(&batch_id, rel_path, &recipe) {
        let _ = staging.rollback_batch(&batch_id);
        return Err(error.into());
    }
    if let Err(err) = staging.record_file_path(&file_merkle, &rel_path.to_string_lossy()) {
        let _ = staging.rollback_batch(&batch_id);
        file_progress.set_state(AddFileState::Failed);
        progress.files_done.fetch_add(1, Relaxed);
        return Err(CrabError::from(err));
    }

    file_progress.chunk_bytes_done.store(size, Relaxed);
    file_progress
        .chunks_done
        .store(recipe.chunk_count(), Relaxed);
    file_progress.set_state(AddFileState::Done);
    progress.files_done.fetch_add(1, Relaxed);

    debug!(
        path = %rel_path.display(),
        size,
        chunks = recipe.chunk_count(),
        "reused staged chunks from verified duplicate payload"
    );

    Ok(FileResult {
        batch_id,
        abs_path: abs_path.to_path_buf(),
        chunks: usize::try_from(recipe.chunk_count()).map_err(|_| {
            CrabError::StagingCorrupt("recipe chunk count cannot fit usize".to_owned())
        })?,
        size,
        file_hash,
        recipe,
        prepared_xorbs: Vec::new(),
        index_stat: Some(verified.index_stat),
        timings: crab_staging::stream::StreamStageTimings::default(),
        duration_ms: start.elapsed().as_millis() as u64,
    })
}

fn add_execution_plans(candidates: &[(PathBuf, u64)], total_bytes: u64) -> AddExecutionPlans {
    let push_config = add_push_config(total_bytes);
    let fingerprint_min_size = push_config
        .as_ref()
        .map_or(ADD_DUPLICATE_REUSE_MIN_BYTES, |push_config| {
            ADD_DUPLICATE_REUSE_MIN_BYTES.min(push_config.min_xorb_size)
        });
    let fingerprints = repeated_candidate_fingerprints(
        candidates,
        fingerprint_min_size,
        ADD_DUPLICATE_FINGERPRINT_BYTES,
    );
    let duplicate_plan = duplicate_reuse_plan_from_fingerprints(&fingerprints);
    let stream_xorb_plan = push_config
        .filter(|push_config| {
            stream_prepared_xorbs_are_efficient(
                candidates.iter().map(|(_, size)| *size),
                push_config.min_xorb_size,
            )
        })
        .map(|push_config| {
            let enabled_paths = stream_prepared_xorb_enabled_paths_from_fingerprints(
                candidates,
                push_config.min_xorb_size,
                &fingerprints,
            );
            StreamPreparedXorbPlan {
                builder: crate::cmd::stream_stage::StreamStageXorbBuilder::new(
                    ADD_STREAM_XORB_BUILDERS,
                    move || {
                        let builder = XorbBuilder::with_policy(push_config.compression_policy());
                        push_config.configure_builder(builder)
                    },
                ),
                enabled_paths,
            }
        });

    AddExecutionPlans {
        duplicate_plan,
        stream_xorb_plan,
        remote_classifier: None,
    }
}

async fn open_add_remote_classifier(
    cancel: &CancellationToken,
) -> Option<Arc<crate::git::push::AddRemoteChunkClassifier>> {
    let config = match crate::core::config::Config::resolve_local() {
        Ok(config) => config,
        Err(error) => {
            warn!(error = %error, "add remote classifier: config unavailable; packing locally");
            return None;
        }
    };
    if matches!(
        config.auth.provider,
        crate::core::config::AuthProvider::CrabAuth
    ) {
        debug!("add remote classifier: protected push requires a push session; packing locally");
        return None;
    }
    let target = match crate::cmd::push::resolve_push_target(None) {
        Ok(target) => target,
        Err(error) => {
            debug!(error = %error, "add remote classifier: no unambiguous Crab remote");
            return None;
        }
    };
    let selection =
        match crate::replication::StoreResolver::new(&config, &target.parsed_url, cancel)
            .write_store("add remote classification")
            .await
        {
            Ok(selection) => selection,
            Err(error) => {
                warn!(error = %error, "add remote classifier: origin unavailable; packing locally");
                return None;
            }
        };
    let caching_store = crab_cache_store::CachingStore::try_build_healthy(
        selection.store.as_storage().clone(),
        &config.cache,
    )
    .await;
    Some(Arc::new(crate::git::push::AddRemoteChunkClassifier::new(
        PushConfig::from_config(&config),
        selection.store,
        caching_store,
        selection.router,
        cancel.clone(),
    )))
}

fn add_push_config(total_bytes: u64) -> Option<Arc<PushConfig>> {
    if total_bytes == 0 {
        return None;
    }

    let config = crate::core::config::Config::resolve_local().unwrap_or_else(|e| {
        warn!(error = %e, "add stream-plan: failed to load config, using defaults");
        crate::core::config::Config::default()
    });
    Some(Arc::new(PushConfig::from_config(&config)))
}

fn repeated_candidate_fingerprints(
    candidates: &[(PathBuf, u64)],
    min_size: u64,
    fingerprint_bytes: usize,
) -> Vec<CandidateFingerprintRecord> {
    let mut sizes = HashMap::<u64, usize>::new();
    for (_, size) in candidates {
        *sizes.entry(*size).or_default() += 1;
    }

    let mut fingerprints = Vec::new();
    for (path, size) in candidates {
        if *size < min_size || sizes.get(size).copied().unwrap_or_default() <= 1 {
            continue;
        }
        let Some(fingerprint) = candidate_fingerprint(path, *size, fingerprint_bytes) else {
            continue;
        };
        fingerprints.push(CandidateFingerprintRecord {
            path: path.clone(),
            size: *size,
            fingerprint,
        });
    }
    fingerprints
}

fn duplicate_reuse_plan_from_fingerprints(
    fingerprints: &[CandidateFingerprintRecord],
) -> DuplicateReusePlan {
    let mut first_path_by_fingerprint = HashMap::<CandidateFingerprint, PathBuf>::new();
    let mut representative_by_path = HashMap::<PathBuf, PathBuf>::new();
    for record in fingerprints {
        if let Some(representative) = first_path_by_fingerprint.get(&record.fingerprint) {
            representative_by_path.insert(record.path.clone(), representative.clone());
        } else {
            first_path_by_fingerprint.insert(record.fingerprint.clone(), record.path.clone());
        }
    }

    DuplicateReusePlan {
        representative_by_path,
    }
}

fn stream_prepared_xorbs_are_efficient(
    file_sizes: impl IntoIterator<Item = u64>,
    min_xorb_size: u64,
) -> bool {
    let mut non_empty_files = 0usize;
    let mut has_small_file = false;
    for size in file_sizes {
        if size == 0 {
            continue;
        }
        non_empty_files += 1;
        has_small_file |= size < min_xorb_size;
    }
    non_empty_files <= 1 || !has_small_file
}

#[cfg(test)]
fn stream_prepared_xorb_enabled_paths(
    candidates: &[(PathBuf, u64)],
    min_xorb_size: u64,
    fingerprint_bytes: usize,
) -> HashSet<PathBuf> {
    let fingerprints =
        repeated_candidate_fingerprints(candidates, min_xorb_size, fingerprint_bytes);
    stream_prepared_xorb_enabled_paths_from_fingerprints(candidates, min_xorb_size, &fingerprints)
}

fn stream_prepared_xorb_enabled_paths_from_fingerprints(
    candidates: &[(PathBuf, u64)],
    min_xorb_size: u64,
    fingerprints: &[CandidateFingerprintRecord],
) -> HashSet<PathBuf> {
    let mut enabled: HashSet<PathBuf> = candidates
        .iter()
        .filter_map(|(path, size)| (*size > 0).then(|| path.clone()))
        .collect();
    if enabled.len() <= 1 {
        return enabled;
    }

    let mut first_path_by_fingerprint = HashMap::<CandidateFingerprint, PathBuf>::new();
    for record in fingerprints {
        if record.size < min_xorb_size {
            continue;
        }
        if first_path_by_fingerprint
            .insert(record.fingerprint.clone(), record.path.clone())
            .is_some()
        {
            enabled.remove(&record.path);
        }
    }

    enabled
}

fn candidate_fingerprint(
    path: &Path,
    size: u64,
    fingerprint_bytes: usize,
) -> Option<CandidateFingerprint> {
    let sample_len = fingerprint_bytes.min(usize::try_from(size).ok()?);
    if sample_len == 0 {
        return None;
    }

    let mut file = std::fs::File::open(path).ok()?;
    let mut head = vec![0u8; sample_len];
    file.read_exact(&mut head).ok()?;

    let middle_start = size.saturating_sub(sample_len as u64) / 2;
    file.seek(SeekFrom::Start(middle_start)).ok()?;
    let mut middle = vec![0u8; sample_len];
    file.read_exact(&mut middle).ok()?;

    let tail_start = size.saturating_sub(sample_len as u64);
    file.seek(SeekFrom::Start(tail_start)).ok()?;
    let mut tail = vec![0u8; sample_len];
    file.read_exact(&mut tail).ok()?;

    Some(CandidateFingerprint {
        size,
        head_hash: *blake3::hash(&head).as_bytes(),
        middle_hash: *blake3::hash(&middle).as_bytes(),
        tail_hash: *blake3::hash(&tail).as_bytes(),
    })
}

fn can_use_stream_prepared_plans(entries: &[StagedEntry]) -> bool {
    stream_prepared_plan_groups(entries).is_some()
}

fn stream_prepared_plan_groups(entries: &[StagedEntry]) -> Option<Vec<StreamPreparedPlanGroup>> {
    let mut groups = Vec::<StreamPreparedPlanGroup>::new();
    let mut group_by_file = HashMap::<[u8; 32], usize>::new();

    for (entry_idx, entry) in entries.iter().enumerate() {
        if let Some(&group_idx) = group_by_file.get(&entry.file_hash) {
            let group = &mut groups[group_idx];
            let representative = &entries[group.representative_idx];
            if representative.size != entry.size || representative.recipe != entry.recipe {
                return None;
            }
            group.files += 1;
            if representative.prepared_xorbs.is_empty() && !entry.prepared_xorbs.is_empty() {
                group.representative_idx = entry_idx;
            }
            continue;
        }

        group_by_file.insert(entry.file_hash, groups.len());
        groups.push(StreamPreparedPlanGroup {
            representative_idx: entry_idx,
            files: 1,
        });
    }

    for group in &groups {
        let entry = &entries[group.representative_idx];
        if entry.recipe.chunk_count() > 0 && entry.prepared_xorbs.is_empty() {
            return None;
        }
    }

    Some(groups)
}

async fn write_stream_prepared_push_plans(
    _staging: &StagingArea,
    entries: &[StagedEntry],
    on_progress: &mut (dyn FnMut(&crab_staging::push_plan::AddPushPlanSummary) + Send),
) -> Result<crab_staging::push_plan::AddPushPlanSummary> {
    let groups = stream_prepared_plan_groups(entries).ok_or_else(|| {
        CrabError::Internal("stream-prepared push plans lost verified coverage".to_owned())
    })?;
    let mut summary = crab_staging::push_plan::AddPushPlanSummary::default();
    for group in groups {
        let entry = &entries[group.representative_idx];
        summary.files += group.files;
        summary.chunks += entry.recipe.chunk_count();
        summary.prepared_xorbs += entry.prepared_xorbs.len() as u64;
        summary.prepared_bytes += entry
            .prepared_xorbs
            .iter()
            .map(|prepared| prepared.bytes)
            .sum::<u64>();
        on_progress(&summary);
    }
    Ok(summary)
}

fn cleanup_stream_prepared_entries(entries: &[StagedEntry]) {
    for entry in entries {
        cleanup_stream_prepared_xorbs(&entry.prepared_xorbs);
    }
}

async fn close_staging_before_indexing(staging: Arc<StagingArea>) -> Result<()> {
    staging.flush_pending().await?;
    match Arc::try_unwrap(staging) {
        Ok(s) => s.close().await.map_err(CrabError::from),
        Err(arc) => Err(CrabError::Internal(format!(
            "staging area still has {} live references after add tasks completed; \
             refusing to write Git pointers",
            Arc::strong_count(&arc)
        ))),
    }
}

fn rollback_open_staging_entries(
    staging: &StagingArea,
    entries: &[StagedEntry],
    reason: &'static str,
) -> Result<u64> {
    cleanup_stream_prepared_entries(entries);
    let mut rows_deleted = 0u64;
    for entry in entries {
        let file_hash = MerkleHash::from(entry.file_hash);
        let stats = match &entry.batch_id {
            Some(batch_id) => staging.rollback_batch(batch_id)?,
            None => vec![staging.retire_file(&file_hash)?],
        };
        let entry_rows = stats.iter().map(|stats| stats.rows_deleted).sum::<u64>();
        rows_deleted += entry_rows;
        if entry_rows > 0 {
            debug!(
                file_hash = %file_hash.hex(),
                rows = entry_rows,
                reason,
                "rolled back unpublished staged rows"
            );
        }
    }
    match staging.sweep_orphans() {
        Ok((segments_removed, bytes_reclaimed, chunks_reclaimed)) => {
            if segments_removed > 0 {
                debug!(
                    segments_removed,
                    bytes_reclaimed,
                    chunks_reclaimed,
                    reason,
                    "reclaimed unpublished staging segments"
                );
            }
        }
        Err(e) => warn!(
            error = %e,
            reason,
            "failed to reclaim unpublished staging segments"
        ),
    }
    Ok(rows_deleted)
}

fn rollback_unpublished_open_entries(
    staging: &StagingArea,
    summary: &mut AddSummary,
    staged_entries: &mut Vec<StagedEntry>,
    reason: &'static str,
) -> Result<()> {
    if staged_entries.is_empty() {
        return Ok(());
    }

    rollback_open_staging_entries(staging, staged_entries, reason)?;
    clear_rolled_back_summary(summary, staged_entries);
    Ok(())
}

fn abort_if_cancelled_before_indexing(
    cancel: &CancellationToken,
    staging: &StagingArea,
    summary: &mut AddSummary,
    staged_entries: &mut Vec<StagedEntry>,
) -> Result<()> {
    if !cancel.is_cancelled() {
        return Ok(());
    }

    rollback_unpublished_open_entries(
        staging,
        summary,
        staged_entries,
        "add cancelled before Git index publication",
    )?;
    Err(CrabError::Cancelled)
}

async fn rollback_closed_staging_entries(
    staging_root: &Path,
    entries: &[StagedEntry],
    reason: &'static str,
) -> Result<u64> {
    cleanup_stream_prepared_entries(entries);
    let staging = StagingAreaReadOnly::open(staging_root.to_path_buf()).await?;
    let mut rows_deleted = 0u64;
    for entry in entries {
        let file_hash = MerkleHash::from(entry.file_hash);
        let stats = match &entry.batch_id {
            Some(batch_id) => staging.rollback_batch(batch_id).await?,
            None => vec![staging.retire_file(&file_hash).await?],
        };
        let entry_rows = stats.iter().map(|stats| stats.rows_deleted).sum::<u64>();
        rows_deleted += entry_rows;
        if entry_rows > 0 {
            debug!(
                file_hash = %file_hash.hex(),
                rows = entry_rows,
                reason,
                "rolled back unpublished staged rows"
            );
        }
    }
    match staging.sweep_orphans() {
        Ok((segments_removed, bytes_reclaimed, chunks_reclaimed)) => {
            if segments_removed > 0 {
                debug!(
                    segments_removed,
                    bytes_reclaimed,
                    chunks_reclaimed,
                    reason,
                    "reclaimed unpublished staging segments"
                );
            }
        }
        Err(e) => warn!(
            error = %e,
            reason,
            "failed to reclaim unpublished staging segments"
        ),
    }
    Ok(rows_deleted)
}

async fn rollback_unpublished_closed_entries(
    staging_root: &Path,
    summary: &mut AddSummary,
    staged_entries: &mut Vec<StagedEntry>,
    reason: &'static str,
) -> Result<()> {
    if staged_entries.is_empty() {
        return Ok(());
    }

    rollback_closed_staging_entries(staging_root, staged_entries, reason).await?;
    clear_rolled_back_summary(summary, staged_entries);
    Ok(())
}

async fn abort_if_cancelled_after_staging_close(
    cancel: &CancellationToken,
    staging_root: &Path,
    summary: &mut AddSummary,
    staged_entries: &mut Vec<StagedEntry>,
) -> Result<()> {
    if !cancel.is_cancelled() {
        return Ok(());
    }

    rollback_unpublished_closed_entries(
        staging_root,
        summary,
        staged_entries,
        "add cancelled after staging flush before Git index publication",
    )
    .await?;
    Err(CrabError::Cancelled)
}

async fn handle_git_index_write_error(
    staging_root: &Path,
    staged_entries: &[StagedEntry],
    publication_intent: Option<&PublicationIntentId>,
    error: GitIndexWriteError,
) -> CrabError {
    match error {
        GitIndexWriteError::BeforeIndexMutation(error) => {
            let cleanup = async {
                if let Some(publication_intent) = publication_intent {
                    let staging = StagingAreaReadOnly::open(staging_root.to_path_buf()).await?;
                    staging
                        .rollback_publication_intent(publication_intent)
                        .await?;
                    Ok(())
                } else {
                    rollback_closed_staging_entries(
                        staging_root,
                        staged_entries,
                        "Git index preparation failed before publication",
                    )
                    .await
                    .map(|_| ())
                }
            }
            .await;
            if let Err(cleanup_err) = cleanup {
                warn!(
                    error = %cleanup_err,
                    "failed to roll back staged rows after Git index preparation failed"
                );
            }
            error
        }
        GitIndexWriteError::IndexMutationUncertain(error) => {
            // After invoking git's index writer, keep staging as the retry
            // source even if this git version rejected the batch atomically.
            warn!(
                error = %error,
                "preserving staged rows after Git index publication failed"
            );
            error
        }
    }
}

fn clear_rolled_back_summary(summary: &mut AddSummary, staged_entries: &mut Vec<StagedEntry>) {
    summary.files_staged = 0;
    summary.chunks_staged = 0;
    summary.bytes_processed = 0;
    summary.chunking_worker_duration_ms = 0;
    summary.remote_lookup_duration_ms = 0;
    summary.compression_worker_duration_ms = 0;
    summary.payload_write_duration_ms = 0;
    summary.planning_duration_ms = 0;
    summary.flushing_duration_ms = 0;
    summary.indexing_duration_ms = 0;
    staged_entries.clear();
}

async fn stop_progress_ticker(ticker: Option<JoinHandle<()>>, cancel: &CancellationToken) {
    if let Some(handle) = ticker {
        cancel.cancel();
        let _ = handle.await;
    }
}

fn effective_add_jobs(jobs: usize) -> usize {
    jobs.max(1)
}

fn should_publish_git_index(
    args: &AddArgs,
    summary: &AddSummary,
    staged_entry_count: usize,
) -> bool {
    // Staging rows are durable and retryable, but Git's index is the
    // commit boundary. Keep a failed add from publishing only the
    // successfully processed siblings.
    !args.skip_git_add && staged_entry_count > 0 && summary.files_failed == 0
}

fn pin_committed_recipes_before_path_replacement(
    staging: &StagingArea,
    repo_root: &Path,
    entries: &[StagedEntry],
) -> Result<()> {
    let paths = entries
        .iter()
        .map(|staged| {
            staged
                .abs_path
                .strip_prefix(repo_root)
                .unwrap_or(&staged.abs_path)
                .to_path_buf()
        })
        .collect::<Vec<_>>();
    let committed_pointers = crate::git::worktree::committed_pointers_for_paths(repo_root, &paths)?;
    let mut pinned_hashes = HashSet::new();

    for (staged, relative_path) in entries.iter().zip(paths) {
        let Some(pointers) = committed_pointers.get(&relative_path) else {
            continue;
        };
        for pointer in pointers {
            if pointer.file_hash == staged.file_hash || !pinned_hashes.insert(pointer.file_hash) {
                continue;
            }
            let file_hash = MerkleHash::from(pointer.file_hash);
            staging.preserve_published_history_recipe(&file_hash, pointer.size)?;
        }
    }
    Ok(())
}

/// Collect candidate files: walk the working tree, filter by tracked
/// patterns and user patterns, skip files that are already pointers.
fn collect_candidates(
    repo_root: &Path,
    classifier: &TrackedClassifier,
    filter: &PatternFilter,
    cancel: &CancellationToken,
) -> Result<Vec<(PathBuf, u64)>> {
    let mut candidates = Vec::new();

    #[cfg(feature = "gix-pathmatch")]
    {
        let ignore = crate::core::attrs::IgnoreReader::open(repo_root)?;
        walk_candidates(
            repo_root,
            repo_root,
            classifier,
            filter,
            Some(&ignore),
            cancel,
            &mut candidates,
        )?;
    }

    #[cfg(not(feature = "gix-pathmatch"))]
    walk_candidates(
        repo_root,
        repo_root,
        classifier,
        filter,
        cancel,
        &mut candidates,
    )?;

    Ok(candidates)
}

async fn filter_clean_indexed_candidates(
    repo_root: &Path,
    candidates: Vec<(PathBuf, u64)>,
    jobs: usize,
    cancel: &CancellationToken,
) -> Result<(Vec<(PathBuf, u64)>, CleanIndexedSkipSummary)> {
    if candidates.is_empty() {
        return Ok((candidates, CleanIndexedSkipSummary::default()));
    }

    let ctx = match crate::git::worktree::WorktreeContext::resolve_from_path(repo_root) {
        Ok(ctx) => ctx,
        Err(e) => {
            debug!(error = %e, "clean-index add fast path disabled: worktree context unavailable");
            return Ok((candidates, CleanIndexedSkipSummary::default()));
        }
    };
    let index_path = ctx.index_path();
    if !index_path.exists() {
        return Ok((candidates, CleanIndexedSkipSummary::default()));
    }

    let index = match gix_index::File::at(
        &index_path,
        gix_hash::Kind::Sha1,
        true,
        gix_index::decode::Options::default(),
    ) {
        Ok(index) => index,
        Err(e) => {
            debug!(error = %e, "clean-index add fast path disabled: failed to open index");
            return Ok((candidates, CleanIndexedSkipSummary::default()));
        }
    };
    let repo = match gix::open(repo_root) {
        Ok(repo) => repo,
        Err(e) => {
            debug!(error = %e, "clean-index add fast path disabled: failed to open git repository");
            return Ok((candidates, CleanIndexedSkipSummary::default()));
        }
    };
    let honor_filemode = git_honors_filemode(repo_root);
    let cache_path = crate::cache::add_validation::cache_path_for_context(&ctx);
    let mut validation_cache =
        match crate::cache::add_validation::AddValidationCache::open(&cache_path) {
            Ok(cache) => Some(cache),
            Err(error) => {
                debug!(
                    path = %cache_path.display(),
                    error = %error,
                    "clean-index add validation cache unavailable; hashing candidates"
                );
                None
            }
        };
    let mut prepared = Vec::with_capacity(candidates.len());
    for (abs_path, size) in candidates {
        let indexed =
            clean_index_pointer(repo_root, &repo, &index, &abs_path, size, honor_filemode);
        let cache_hit = match (
            validation_cache.as_ref(),
            indexed.as_ref(),
            indexed
                .as_ref()
                .and_then(|indexed| indexed.validation_token.as_ref()),
        ) {
            (Some(cache), Some(indexed), Some(token)) => {
                match cache.contains(&indexed.path_bytes, token) {
                    Ok(hit) => hit,
                    Err(error) => {
                        debug!(
                            path = %cache_path.display(),
                            error = %error,
                            "clean-index add validation cache query failed; hashing candidates"
                        );
                        validation_cache = None;
                        false
                    }
                }
            }
            _ => false,
        };
        prepared.push((abs_path, size, indexed, cache_hit));
    }
    let mut checks = futures_util::stream::iter(prepared)
        .map(|(abs_path, size, indexed, cache_hit)| async move {
            let (matches, verified) = if cache_hit {
                (
                    crate::cmd::stream_stage::VerifiedIndexStat::from_path_no_follow(&abs_path)
                        == indexed.as_ref().map(|indexed| indexed.verified_stat),
                    None,
                )
            } else {
                match indexed {
                    Some(indexed) => {
                        let verified = worktree_content_matches_pointer(
                            &abs_path,
                            size,
                            indexed.expected_hash,
                            cancel,
                        )
                        .await?;
                        (
                            verified.is_some(),
                            verified.map(|stat| (indexed.expected_hash, stat)),
                        )
                    }
                    None => (false, None),
                }
            };
            Ok::<_, CrabError>((abs_path, size, matches, cache_hit, verified))
        })
        .buffered(jobs.max(1));

    let mut to_process = Vec::new();
    let mut skipped = CleanIndexedSkipSummary::default();
    let mut newly_verified = Vec::new();
    while let Some(result) = checks.next().await {
        let (abs_path, size, matches, cache_hit, verified) = result?;
        if matches {
            skipped.files += 1;
            skipped.bytes += size;
            if cache_hit {
                skipped.validation_cache_hits += 1;
                skipped.validation_cache_hit_bytes += size;
            } else if let Some((file_hash, index_stat)) = verified {
                newly_verified.push(VerifiedPath {
                    path: abs_path.clone(),
                    file_hash,
                    size,
                    index_stat,
                });
            }
        } else {
            to_process.push((abs_path, size));
        }
    }

    if !newly_verified.is_empty()
        && let Err(error) =
            crate::cache::add_validation::record_verified_paths(repo_root, &newly_verified)
    {
        debug!(
            path = %cache_path.display(),
            error = %error,
            "failed to persist content-verified add validations"
        );
    }

    if skipped.files > 0 {
        debug!(
            files = skipped.files,
            bytes = skipped.bytes,
            validation_cache_hits = skipped.validation_cache_hits,
            validation_cache_hit_bytes = skipped.validation_cache_hit_bytes,
            "skipped content-verified indexed files during add"
        );
    }

    Ok((to_process, skipped))
}

async fn worktree_content_matches_pointer(
    path: &Path,
    expected_size: u64,
    expected_hash: [u8; 32],
    cancel: &CancellationToken,
) -> Result<Option<crate::cmd::stream_stage::VerifiedIndexStat>> {
    let verified = crate::cmd::stream_stage::stream_hash_file(path, None, cancel).await?;
    Ok(
        (verified.size == expected_size && verified.hash == expected_hash)
            .then_some(verified.index_stat),
    )
}

fn clean_index_pointer(
    repo_root: &Path,
    repo: &gix::Repository,
    index: &gix_index::File,
    abs_path: &Path,
    size: u64,
    honor_filemode: bool,
) -> Option<CleanIndexPointer> {
    use bstr::ByteSlice;
    use gix_index::entry;

    let rel_path = abs_path.strip_prefix(repo_root).unwrap_or(abs_path);
    let rel_bstr = git_index_path_bstring(rel_path);
    let entry = index.entry_by_path_and_stage(rel_bstr.as_bstr(), entry::Stage::Unconflicted)?;

    let expected_mode = index_mode_for_worktree_file(abs_path, honor_filemode);
    if entry.mode != expected_mode {
        return None;
    }

    let current_stat = crate::cmd::stream_stage::VerifiedIndexStat::from_path_no_follow(abs_path)?;
    if current_stat.len != size {
        return None;
    }

    let stat_options = gix_index::entry::stat::Options::default();
    if !current_stat.stat.matches(&entry.stat, stat_options) {
        return None;
    }

    let blob = match repo.find_blob(entry.id) {
        Ok(blob) => Some(blob),
        Err(e) => {
            debug!(
                path = %rel_path.display(),
                oid = %entry.id,
                error = %e,
                "clean-index add fast path: failed to read indexed blob"
            );
            None
        }
    }?;
    let pointer = Pointer::parse(&blob.data).ok()?;
    if pointer.size != size {
        return None;
    }
    let path_bytes = rel_bstr.as_bytes().to_vec();
    let validation_token = crate::cache::add_validation::stat_is_cacheable(&current_stat.stat)
        .then(|| {
            crate::cache::add_validation::validation_token(
                &path_bytes,
                entry.id.as_bytes(),
                &blob.data,
                entry.mode.bits(),
                &current_stat.stat,
                current_stat.len,
            )
        });
    Some(CleanIndexPointer {
        expected_hash: pointer.file_hash,
        path_bytes,
        validation_token,
        verified_stat: current_stat,
    })
}

/// Recursive directory walker that collects `(abs_path, file_size)` pairs.
#[cfg(feature = "gix-pathmatch")]
fn walk_candidates(
    root: &Path,
    dir: &Path,
    classifier: &TrackedClassifier,
    filter: &PatternFilter,
    ignore: Option<&crate::core::attrs::IgnoreReader>,
    cancel: &CancellationToken,
    out: &mut Vec<(PathBuf, u64)>,
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

            // Consult ignore rules for this directory before descending.
            if let (Some(ignore), Ok(rel_dir)) = (ignore, path.strip_prefix(root)) {
                let rel_str = rel_dir.to_string_lossy();
                if !rel_str.is_empty() && ignore.is_ignored(&rel_str, true) {
                    debug!(dir = %rel_str, "skipping ignored directory");
                    continue;
                }
                // Pick up nested .gitignore so precedence matches git.
                let nested = path.join(".gitignore");
                ignore.append_patterns_from_file(&nested, Some(root));
            }

            walk_candidates(root, &path, classifier, filter, ignore, cancel, out)?;
            continue;
        }

        if !file_type.is_file() {
            continue;
        }

        let Ok(rel_path) = path.strip_prefix(root) else {
            continue;
        };

        // Must not match a .gitignore / .git/info/exclude rule.
        if let Some(ignore) = ignore {
            let rel_str = rel_path.to_string_lossy();
            if ignore.is_ignored(&rel_str, false) {
                continue;
            }
        }

        // Must match a crab-tracked pattern from .gitattributes.
        if !classifier.is_tracked(rel_path) {
            continue;
        }

        let rel_str = rel_path.to_string_lossy();

        // Must match the user's add pattern filter.
        if !filter.matches(&rel_str) {
            continue;
        }

        // Get file size for reporting.
        let metadata = std::fs::metadata(&path)?;
        let file_size = metadata.len();

        // Skip files that are already pointers (e.g. lazy-checkout left
        // the pointer on disk, or `crab dehydrate` was run). These don't
        // need re-staging.
        if crate::engine::pointer::is_working_tree_pointer(&path).unwrap_or(false) {
            continue;
        }

        out.push((path, file_size));
    }

    Ok(())
}

/// Recursive directory walker that collects `(abs_path, file_size)` pairs.
#[cfg(not(feature = "gix-pathmatch"))]
fn walk_candidates(
    root: &Path,
    dir: &Path,
    classifier: &TrackedClassifier,
    filter: &PatternFilter,
    cancel: &CancellationToken,
    out: &mut Vec<(PathBuf, u64)>,
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
            walk_candidates(root, &path, classifier, filter, cancel, out)?;
            continue;
        }

        if !file_type.is_file() {
            continue;
        }

        let Ok(rel_path) = path.strip_prefix(root) else {
            continue;
        };

        // Must match a crab-tracked pattern from .gitattributes.
        if !classifier.is_tracked(rel_path) {
            continue;
        }

        let rel_str = rel_path.to_string_lossy();

        // Must match the user's add pattern filter.
        if !filter.matches(&rel_str) {
            continue;
        }

        // Get file size for reporting.
        let metadata = std::fs::metadata(&path)?;
        let file_size = metadata.len();

        // Skip files that are already pointers (e.g. lazy-checkout left
        // the pointer on disk, or `crab dehydrate` was run). These don't
        // need re-staging.
        if crate::engine::pointer::is_working_tree_pointer(&path).unwrap_or(false) {
            continue;
        }

        out.push((path, file_size));
    }

    Ok(())
}

/// Per-site classifier for `.gitattributes filter=crab` lookup.
///
/// Under `gix-pathmatch`, wraps the consolidated
/// [`core::attrs::TrackedClassifier`] (backed by `gix_attributes::Search`).
/// Otherwise, falls back to a simple suffix-matching helper driven by
/// patterns parsed out of the root `.gitattributes` line-by-line.
#[cfg(feature = "gix-pathmatch")]
struct TrackedClassifier {
    inner: crate::core::attrs::TrackedClassifier,
    has_patterns: bool,
}

#[cfg(not(feature = "gix-pathmatch"))]
struct TrackedClassifier {
    patterns: Vec<String>,
}

impl TrackedClassifier {
    fn open(root: &Path) -> Result<Self> {
        #[cfg(feature = "gix-pathmatch")]
        {
            Ok(TrackedClassifier {
                inner: crate::core::attrs::TrackedClassifier::open(root, "crab")?,
                has_patterns: gitattributes_contain_filter(root, "crab")?,
            })
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
            !self.has_patterns
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
            self.inner.is_tracked(&rel_str)
        }
        #[cfg(not(feature = "gix-pathmatch"))]
        {
            let _ = rel_str;
            matches_any_tracked_legacy(rel_path, &self.patterns)
        }
    }
}

#[cfg(feature = "gix-pathmatch")]
fn gitattributes_contain_filter(root: &Path, filter_name: &str) -> Result<bool> {
    gitattributes_dir_contains_filter(root, filter_name, 0)
}

#[cfg(feature = "gix-pathmatch")]
fn gitattributes_dir_contains_filter(dir: &Path, filter_name: &str, depth: usize) -> Result<bool> {
    const MAX_DEPTH: usize = 32;
    if depth >= MAX_DEPTH {
        return Ok(false);
    }

    if gitattributes_file_contains_filter(&dir.join(".gitattributes"), filter_name)? {
        return Ok(true);
    }

    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name == ".git" || name == ".crab" {
            continue;
        }

        let is_dir = entry.file_type().map(|ft| ft.is_dir()).unwrap_or(false);
        if is_dir && gitattributes_dir_contains_filter(&entry.path(), filter_name, depth + 1)? {
            return Ok(true);
        }
    }

    Ok(false)
}

#[cfg(feature = "gix-pathmatch")]
fn gitattributes_file_contains_filter(path: &Path, filter_name: &str) -> Result<bool> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
    };
    let target = format!("filter={filter_name}");

    Ok(content.lines().any(|line| {
        let line = line.trim();
        !line.is_empty()
            && !line.starts_with('#')
            && line.split_whitespace().skip(1).any(|attr| attr == target)
    }))
}

/// Legacy simple-suffix matcher; retained for builds without
/// `gix-pathmatch`. The consolidated matcher lives in `core::attrs`.
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

/// Parse `.gitattributes` for crab-tracked glob patterns.
///
/// Legacy fallback for builds without `gix-pathmatch`.
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

/// Diagnose why `crab add` found no matching files by checking whether
/// the user's patterns reference file extensions that exist on disk but
/// are not tracked in `.gitattributes`.
///
/// Returns a deduplicated list of extensions (without the leading dot)
/// that exist in the working tree but have no corresponding
/// `filter=crab` pattern.
fn diagnose_untracked_extensions(
    repo_root: &Path,
    classifier: &TrackedClassifier,
    user_patterns: &[String],
) -> Vec<String> {
    use std::collections::BTreeSet;

    let mut untracked: BTreeSet<String> = BTreeSet::new();

    for pattern in user_patterns {
        // Check if the pattern looks like a literal filename (no glob chars).
        let is_literal = !pattern.contains('*') && !pattern.contains('?') && !pattern.contains('[');

        if is_literal {
            // The user typed something like `companies.db` — check if
            // the file exists and whether its extension is tracked.
            let candidate = repo_root.join(pattern);
            if candidate.is_file() {
                let rel = Path::new(pattern);
                if !classifier.is_tracked(rel)
                    && let Some(ext) = rel.extension()
                {
                    untracked.insert(ext.to_string_lossy().into_owned());
                }
            }
        } else {
            // Glob pattern like `*.parquet` — extract the extension from
            // the pattern itself and check if it's tracked.
            if let Some(suffix) = pattern.strip_prefix("*.") {
                // Only consider simple `*.ext` patterns.
                if !suffix.contains('*') && !suffix.contains('/') {
                    let probe_name = format!("test.{suffix}");
                    let probe = Path::new(&probe_name);
                    if !classifier.is_tracked(probe) {
                        untracked.insert(suffix.to_owned());
                    }
                }
            }
        }
    }

    untracked.into_iter().collect()
}

#[derive(Debug)]
enum GitIndexWriteError {
    BeforeIndexMutation(CrabError),
    IndexMutationUncertain(CrabError),
}

/// Write crab pointer blobs directly into git's index, bypassing
/// `git add` and the clean filter.
///
/// For each staged entry this function:
///   1. Builds a [`Pointer`] from the already-computed file metadata,
///      attaching a [`ShardHintCache`] hint if one is known for this
///      file hash (from a prior push).
///   2. Writes the pointer payload into git's object database as a blob
///      via gix's object database writer, returning the blob's object id.
///   3. Locks and rereads the current index, applies only the selected
///      pointer/stat deltas, and commits one atomic replacement.
///
/// Why not just `git add`?  `git add` invokes the crab clean filter,
/// which re-reads and re-hashes the full file just to emit the same
/// pointer we can assemble from the data we already have. For a 1.6 GiB
/// file that's a ~20 s detour through the filter-process protocol with
/// no visible progress, because the subprocess is opaque from the
/// caller's perspective. Writing the pointer directly is near-instant
/// per file and lets the live bar tick once per staged file.
///
/// The clean filter is still correct on its own — a `git add` on a
/// crab-tracked path at the shell (outside `crab add`) will still
/// produce the same pointer via the filter.
#[cfg(test)]
fn write_pointers_to_git_index(
    entries: &[StagedEntry],
    repo_root: &Path,
    shard_hints: &crate::cache::ShardHintCache,
    mut on_file_done: impl FnMut(),
) -> std::result::Result<(), GitIndexWriteError> {
    let entries = entries
        .iter()
        .map(IndexPublicationEntry::from)
        .collect::<Vec<_>>();
    write_pointers_and_tracking_to_git_index(
        &entries,
        repo_root,
        shard_hints,
        &[],
        |_, _| Ok(()),
        &mut on_file_done,
    )
}

fn write_pointers_and_tracking_to_git_index(
    entries: &[IndexPublicationEntry],
    repo_root: &Path,
    shard_hints: &crate::cache::ShardHintCache,
    tracking_patterns: &[String],
    before_index_mutation: impl FnOnce(&[GitIndexEntry], &gix_index::File) -> Result<()>,
    mut on_file_done: impl FnMut(),
) -> std::result::Result<(), GitIndexWriteError> {
    let honor_filemode = git_honors_filemode(repo_root);
    let mut index_entries = Vec::with_capacity(entries.len());

    for entry in entries {
        // The cache may not contain this file on the first push; the
        // smudge path tolerates a missing hint.
        let pointer = shard_hints.pointer_for(entry.file_hash, entry.size);
        let payload = pointer.serialize();
        let sha = write_pointer_blob(repo_root, &payload)
            .map_err(GitIndexWriteError::BeforeIndexMutation)?;

        // The index-info record bypasses git's normal worktree mode detection,
        // so compute the regular-file mode here to match `git add`.
        let index_mode = index_mode_for_worktree_file(&entry.abs_path, honor_filemode);
        let index_stat = entry.index_stat.ok_or_else(|| {
            GitIndexWriteError::BeforeIndexMutation(CrabError::FileChangedDuringStaging {
                path: entry.abs_path.display().to_string(),
                first_hash: MerkleHash::from(entry.file_hash).hex(),
                second_hash: "stat-unavailable".to_owned(),
                first_size: entry.size,
                second_size: 0,
            })
        })?;
        index_entries.push(GitIndexEntry {
            abs_path: entry.abs_path.clone(),
            mode: index_mode,
            sha,
            index_stat,
        });

        on_file_done();
    }

    publish_git_index_entries_with_tracking(
        &index_entries,
        repo_root,
        tracking_patterns,
        before_index_mutation,
    )?;

    Ok(())
}

struct GitIndexEntry {
    abs_path: PathBuf,
    mode: gix_index::entry::Mode,
    sha: String,
    index_stat: crate::cmd::stream_stage::VerifiedIndexStat,
}

fn write_pointer_blob(repo_root: &Path, payload: &[u8]) -> Result<String> {
    let repo = gix::open(repo_root).map_err(|e| {
        CrabError::Internal(format!("failed to open git repository for blob write: {e}"))
    })?;
    let oid = repo
        .write_blob(payload)
        .map_err(|e| CrabError::Internal(format!("failed to write pointer blob: {e}")))?;
    Ok(oid.to_string())
}

fn git_index_path_state(index: &gix_index::File, path: &bstr::BStr) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab git index path state v1\0");
    if let Some(range) = index.entry_range(path) {
        for entry in &index.entries()[range] {
            hasher.update(&entry.flags.bits().to_le_bytes());
            hasher.update(&entry.mode.bits().to_le_bytes());
            hasher.update(entry.id.as_bytes());
        }
    }
    hasher.finalize().to_hex().to_string()
}

pub(crate) async fn reconcile_publication_intents(
    staging: &StagingAreaReadOnly,
    repo_root: &Path,
) -> Result<()> {
    for action in
        publication_recovery_actions(staging.unresolved_publication_intents()?, repo_root)?
    {
        match action {
            PublicationRecoveryAction::Publish(intent_id) => {
                staging.publish_publication_intent(&intent_id)?;
            }
            PublicationRecoveryAction::Rollback(intent_id) => {
                staging.rollback_publication_intent(&intent_id).await?;
            }
        }
    }
    Ok(())
}

fn reconcile_publication_intents_writer(staging: &StagingArea, repo_root: &Path) -> Result<()> {
    for action in
        publication_recovery_actions(staging.unresolved_publication_intents()?, repo_root)?
    {
        match action {
            PublicationRecoveryAction::Publish(intent_id) => {
                staging.publish_publication_intent(&intent_id)?;
            }
            PublicationRecoveryAction::Rollback(intent_id) => {
                staging.rollback_publication_intent(&intent_id)?;
            }
        }
    }
    Ok(())
}

enum PublicationRecoveryAction {
    Publish(PublicationIntentId),
    Rollback(PublicationIntentId),
}

fn publication_recovery_actions(
    intents: Vec<crab_staging::UnresolvedPublicationIntent>,
    repo_root: &Path,
) -> Result<Vec<PublicationRecoveryAction>> {
    use bstr::ByteSlice;
    use gix_index::{File, decode, entry};

    if intents.is_empty() {
        return Ok(Vec::new());
    }
    let index_path =
        crate::git::worktree::WorktreeContext::resolve_from_path(repo_root)?.index_path();
    let index = File::at_or_default(
        &index_path,
        gix_hash::Kind::Sha1,
        false,
        decode::Options::default(),
    )
    .map_err(|error| {
        CrabError::Internal(format!(
            "failed to read Git index for add publication recovery: {error}"
        ))
    })?;

    let mut actions = Vec::with_capacity(intents.len());
    for intent in intents {
        let new_matches = intent
            .entries
            .iter()
            .filter(|expected| {
                index
                    .entry_by_path_and_stage(
                        expected.path_bytes.as_bstr(),
                        entry::Stage::Unconflicted,
                    )
                    .is_some_and(|actual| actual.id.to_string() == expected.expected_pointer_oid)
            })
            .count();
        let old_matches = intent
            .entries
            .iter()
            .filter(|expected| {
                git_index_path_state(&index, expected.path_bytes.as_bstr())
                    == expected.previous_index_state
            })
            .count();
        if new_matches == intent.entries.len() {
            actions.push(PublicationRecoveryAction::Publish(intent.intent_id));
        } else if old_matches == intent.entries.len() {
            actions.push(PublicationRecoveryAction::Rollback(intent.intent_id));
        } else {
            return Err(CrabError::StagingCorrupt(format!(
                "add publication {} has an ambiguous Git index state ({new_matches}/{} new paths, {old_matches}/{} prior paths); restore one complete index state, then retry",
                intent.intent_id.as_str(),
                intent.entries.len(),
                intent.entries.len(),
            )));
        }
    }
    Ok(actions)
}

#[cfg(test)]
fn publish_git_index_entries(
    entries: &[GitIndexEntry],
    repo_root: &Path,
) -> std::result::Result<(), GitIndexWriteError> {
    publish_git_index_entries_with_tracking(entries, repo_root, &[], |_, _| Ok(()))
}

fn publish_generated_tracking_rules(repo_root: &Path, patterns: &[String]) -> Result<()> {
    publish_git_index_entries_with_tracking(&[], repo_root, patterns, |_, _| Ok(())).map_err(
        |error| match error {
            GitIndexWriteError::BeforeIndexMutation(error)
            | GitIndexWriteError::IndexMutationUncertain(error) => error,
        },
    )
}

fn publish_git_index_entries_with_tracking(
    entries: &[GitIndexEntry],
    repo_root: &Path,
    tracking_patterns: &[String],
    before_index_mutation: impl FnOnce(&[GitIndexEntry], &gix_index::File) -> Result<()>,
) -> std::result::Result<(), GitIndexWriteError> {
    use bstr::ByteSlice;
    use gix_index::{File, decode, entry};
    if entries.is_empty() && tracking_patterns.is_empty() {
        return Ok(());
    }

    let index_path = crate::git::worktree::WorktreeContext::resolve_from_path(repo_root)
        .map_err(GitIndexWriteError::BeforeIndexMutation)?
        .index_path();
    let latest_worktree_mtime_secs = entries
        .iter()
        .map(|entry| entry.index_stat.stat.mtime.secs)
        .max();
    wait_for_index_timestamp_after_worktree(latest_worktree_mtime_secs);
    let lock = gix_lock::File::acquire_to_update_resource(
        &index_path,
        gix_lock::acquire::Fail::Immediately,
        None,
    )
    .map_err(|error| {
        GitIndexWriteError::BeforeIndexMutation(CrabError::Internal(format!(
            "failed to lock Git index: {error}"
        )))
    })?;
    let mut index = File::at_or_default(
        &index_path,
        gix_hash::Kind::Sha1,
        false,
        decode::Options::default(),
    )
    .map_err(|error| {
        GitIndexWriteError::BeforeIndexMutation(CrabError::Internal(format!(
            "failed to reread locked Git index: {error}"
        )))
    })?;

    struct PreparedIndexEntry {
        rel_path: bstr::BString,
        oid: gix_hash::ObjectId,
        stat: gix_index::entry::Stat,
        mode: gix_index::entry::Mode,
    }

    let mut prepared =
        Vec::with_capacity(entries.len() + usize::from(!tracking_patterns.is_empty()));
    for selected in entries {
        let current =
            crate::cmd::stream_stage::VerifiedIndexStat::from_path_no_follow(&selected.abs_path);
        if current != Some(selected.index_stat) {
            return Err(GitIndexWriteError::BeforeIndexMutation(
                CrabError::FileChangedDuringStaging {
                    path: selected.abs_path.display().to_string(),
                    first_hash: selected.sha.clone(),
                    second_hash: "changed-before-index-publication".to_owned(),
                    first_size: selected.index_stat.len,
                    second_size: current.map_or(0, |stat| stat.len),
                },
            ));
        }

        let rel_path = selected
            .abs_path
            .strip_prefix(repo_root)
            .unwrap_or(&selected.abs_path);
        let rel_path = git_index_path_bstring(rel_path);
        let oid = gix_hash::ObjectId::from_hex(selected.sha.as_bytes()).map_err(|error| {
            GitIndexWriteError::BeforeIndexMutation(CrabError::Internal(format!(
                "invalid pointer blob id {}: {error}",
                selected.sha
            )))
        })?;
        prepared.push(PreparedIndexEntry {
            rel_path,
            oid,
            stat: selected.index_stat.stat,
            mode: selected.mode,
        });
    }

    if !tracking_patterns.is_empty() {
        let attributes_path = repo_root.join(".gitattributes");
        let worktree_bytes = std::fs::read(&attributes_path)
            .map_err(|error| GitIndexWriteError::BeforeIndexMutation(CrabError::Io(error)))?;
        let worktree_content = std::str::from_utf8(&worktree_bytes).map_err(|error| {
            GitIndexWriteError::BeforeIndexMutation(CrabError::Internal(format!(
                ".gitattributes is not valid UTF-8: {error}"
            )))
        })?;
        let lines = tracking_patterns
            .iter()
            .map(|pattern| crate::cmd::track::attrs_line(pattern))
            .collect::<Vec<_>>();
        for line in &lines {
            if !worktree_content.lines().any(|current| current == line) {
                return Err(GitIndexWriteError::BeforeIndexMutation(
                    CrabError::Internal(format!(
                        "generated tracking rule disappeared before index publication: {line}"
                    )),
                ));
            }
        }

        let rel_path = bstr::BString::from(".gitattributes");
        let existing_entry = index
            .entry_by_path_and_stage(rel_path.as_bstr(), entry::Stage::Unconflicted)
            .cloned();
        let mut indexed_content = match existing_entry.as_ref() {
            Some(existing) => {
                let repo = gix::open(repo_root).map_err(|error| {
                    GitIndexWriteError::BeforeIndexMutation(CrabError::Internal(format!(
                        "failed to open Git repository for .gitattributes publication: {error}"
                    )))
                })?;
                let blob = repo.find_blob(existing.id).map_err(|error| {
                    GitIndexWriteError::BeforeIndexMutation(CrabError::Internal(format!(
                        "failed to read indexed .gitattributes: {error}"
                    )))
                })?;
                String::from_utf8(blob.data.to_vec()).map_err(|error| {
                    GitIndexWriteError::BeforeIndexMutation(CrabError::Internal(format!(
                        "indexed .gitattributes is not valid UTF-8: {error}"
                    )))
                })?
            }
            None => String::new(),
        };
        let mut changed = false;
        for line in &lines {
            if indexed_content.lines().any(|current| current == line) {
                continue;
            }
            if !indexed_content.is_empty() && !indexed_content.ends_with('\n') {
                indexed_content.push('\n');
            }
            indexed_content.push_str(line);
            indexed_content.push('\n');
            changed = true;
        }
        if changed {
            let repo = gix::open(repo_root).map_err(|error| {
                GitIndexWriteError::BeforeIndexMutation(CrabError::Internal(format!(
                    "failed to open Git repository for .gitattributes blob write: {error}"
                )))
            })?;
            let oid = repo
                .write_blob(indexed_content.as_bytes())
                .map_err(|error| {
                    GitIndexWriteError::BeforeIndexMutation(CrabError::Internal(format!(
                        "failed to write .gitattributes blob: {error}"
                    )))
                })?;
            let stat = if indexed_content.as_bytes() == worktree_bytes {
                crate::cmd::stream_stage::VerifiedIndexStat::from_path_no_follow(&attributes_path)
                    .map_or_else(gix_index::entry::Stat::default, |value| value.stat)
            } else {
                gix_index::entry::Stat::default()
            };
            prepared.push(PreparedIndexEntry {
                rel_path,
                oid: oid.into(),
                stat,
                mode: existing_entry.map_or(gix_index::entry::Mode::FILE, |value| value.mode),
            });
        }
    }

    before_index_mutation(entries, &index).map_err(GitIndexWriteError::BeforeIndexMutation)?;

    // Path lookups require sorted entries. Remove every old entry before
    // dangerously_push_entry invalidates that ordering for subsequent lookups.
    for selected in &prepared {
        while let Some(range) = index.entry_range(selected.rel_path.as_bstr()) {
            for position in range.rev() {
                index.remove_entry_at_index(position);
            }
        }
    }
    for selected in prepared {
        index.dangerously_push_entry(
            selected.stat,
            selected.oid,
            entry::Flags::from_stage(entry::Stage::Unconflicted),
            selected.mode,
            selected.rel_path.as_bstr(),
        );
    }
    index.sort_entries();
    index.remove_tree();

    let mut writer = std::io::BufWriter::with_capacity(64 * 1024, lock);
    index
        .write_to(&mut writer, gix_index::write::Options::default())
        .map_err(|error| {
            GitIndexWriteError::BeforeIndexMutation(CrabError::Internal(format!(
                "failed to encode Git index: {error}"
            )))
        })?;
    let lock = writer.into_inner().map_err(|error| {
        GitIndexWriteError::BeforeIndexMutation(CrabError::Io(error.into_error()))
    })?;
    lock.commit().map_err(|error| {
        GitIndexWriteError::IndexMutationUncertain(CrabError::Internal(format!(
            "failed to commit Git index replacement: {error}"
        )))
    })?;
    Ok(())
}

fn wait_for_index_timestamp_after_worktree(latest_worktree_mtime_secs: Option<u32>) {
    let Some(latest_worktree_mtime_secs) = latest_worktree_mtime_secs else {
        return;
    };
    let Ok(now) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) else {
        return;
    };
    let target_secs = u64::from(latest_worktree_mtime_secs).saturating_add(1);
    let wait_secs = target_secs.saturating_sub(now.as_secs());
    if wait_secs == 0 || wait_secs > 2 {
        return;
    }

    // Git treats entries from the same second as the index timestamp as racy and re-reads them.
    // Pointer entries intentionally differ from raw worktree bytes, so commit just after the
    // next second to keep a freshly staged file clean without hiding later edits. Leave a small
    // margin for filesystem timestamp rounding and scheduler wake-up jitter.
    let remaining = std::time::Duration::from_secs(wait_secs)
        .saturating_sub(std::time::Duration::from_nanos(u64::from(
            now.subsec_nanos(),
        )))
        .saturating_add(std::time::Duration::from_millis(100));
    std::thread::sleep(remaining);
}

fn git_honors_filemode(repo_root: &Path) -> bool {
    #[cfg(unix)]
    {
        let output = std::process::Command::new("git")
            .args(["config", "--bool", "core.filemode"])
            .current_dir(repo_root)
            .output();

        match output {
            Ok(output) if output.status.success() => {
                let value = String::from_utf8_lossy(&output.stdout);
                !matches!(value.trim(), "false" | "0" | "no" | "off")
            }
            _ => true,
        }
    }

    #[cfg(not(unix))]
    {
        let _ = repo_root;
        false
    }
}

fn index_mode_for_worktree_file(abs_path: &Path, honor_filemode: bool) -> gix_index::entry::Mode {
    if honor_filemode && worktree_file_is_executable(abs_path) {
        gix_index::entry::Mode::FILE_EXECUTABLE
    } else {
        gix_index::entry::Mode::FILE
    }
}

#[cfg(unix)]
fn worktree_file_is_executable(abs_path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;

    std::fs::metadata(abs_path)
        .map(|meta| meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn worktree_file_is_executable(_abs_path: &Path) -> bool {
    false
}

#[cfg(unix)]
fn git_index_path_bstring(path: &Path) -> bstr::BString {
    use std::os::unix::ffi::OsStrExt;

    bstr::BString::from(path.as_os_str().as_bytes().to_vec())
}

#[cfg(not(unix))]
fn git_index_path_bstring(path: &Path) -> bstr::BString {
    bstr::BString::from(path.to_string_lossy().into_owned())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "test assertions")]
mod tests {
    use super::*;
    use crab_staging::StagingArea;
    use crab_staging::push_plan::prepared_xorb_path;
    use crab_xet::hash::compute_data_hash;

    static CWD_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    struct CurrentDirGuard {
        original: PathBuf,
    }

    impl CurrentDirGuard {
        fn enter(path: &Path) -> Self {
            let original = std::env::current_dir().unwrap();
            std::env::set_current_dir(path).unwrap();
            Self { original }
        }
    }

    impl Drop for CurrentDirGuard {
        fn drop(&mut self) {
            std::env::set_current_dir(&self.original).unwrap();
        }
    }

    fn staged_entry(abs_path: PathBuf, file_hash: [u8; 32], size: u64) -> StagedEntry {
        let index_stat =
            crate::cmd::stream_stage::VerifiedIndexStat::from_path_no_follow(&abs_path);
        let recipe_chunks = if size == 0 {
            Vec::new()
        } else {
            vec![(MerkleHash::from(file_hash), size)]
        };
        let recipe = crab_staging::recipe::FileRecipe::from_staged_chunks(
            crab_staging::recipe::ChunkingPolicyId::XetGearV1_64KiB,
            MerkleHash::from(file_hash),
            size,
            &recipe_chunks,
        )
        .unwrap();
        StagedEntry {
            batch_id: None,
            abs_path,
            file_hash,
            size,
            recipe,
            prepared_xorbs: Vec::new(),
            index_stat,
        }
    }

    fn init_git_repo(path: &Path) -> bool {
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(path)
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }

    fn make_file_mtime_old(path: &Path) {
        let old = filetime::FileTime::from_unix_time(1_700_000_000, 0);
        filetime::set_file_mtime(path, old).unwrap();
    }

    async fn stage_publication_recipe(
        staging: &StagingArea,
        rel_path: &Path,
        data: &[u8],
    ) -> (crab_staging::StagingBatchId, MerkleHash) {
        let file_hash = MerkleHash::from(*blake3::hash(data).as_bytes());
        let chunk_hash = compute_data_hash(data);
        staging
            .pre_register_file(&file_hash, data.len() as u64)
            .unwrap();
        staging
            .stage_chunks_batch(&[(&chunk_hash, data)], &file_hash, 0)
            .await
            .unwrap();
        staging.flush_pending().await.unwrap();
        let recipe = crab_staging::recipe::FileRecipe::from_staged_chunks(
            crab_staging::recipe::ChunkingPolicyId::XetGearV1_64KiB,
            file_hash,
            data.len() as u64,
            &[(chunk_hash, data.len() as u64)],
        )
        .unwrap();
        let batch_id = staging.create_batch().unwrap();
        staging
            .record_verified_recipe_lease(&batch_id, rel_path, &recipe)
            .unwrap();
        (batch_id, file_hash)
    }

    fn publication_index_entry(repo: &Path, rel_path: &Path, payload: &[u8]) -> GitIndexEntry {
        let abs_path = repo.join(rel_path);
        std::fs::write(&abs_path, payload).unwrap();
        make_file_mtime_old(&abs_path);
        GitIndexEntry {
            abs_path: abs_path.clone(),
            mode: gix_index::entry::Mode::FILE,
            sha: write_pointer_blob(repo, payload).unwrap(),
            index_stat: crate::cmd::stream_stage::VerifiedIndexStat::from_path_no_follow(&abs_path)
                .unwrap(),
        }
    }

    fn read_test_index(repo: &Path) -> gix_index::File {
        use gix_index::{File, decode};

        let index_path = crate::git::worktree::WorktreeContext::resolve_from_path(repo)
            .unwrap()
            .index_path();
        File::at_or_default(
            &index_path,
            gix_hash::Kind::Sha1,
            false,
            decode::Options::default(),
        )
        .unwrap()
    }

    #[test]
    #[cfg(not(feature = "gix-pathmatch"))]
    fn matches_any_tracked_extension_wildcard() {
        let patterns = vec!["*.bin".to_owned()];
        assert!(matches_any_tracked_legacy(
            Path::new("model.bin"),
            &patterns
        ));
        assert!(!matches_any_tracked_legacy(
            Path::new("model.txt"),
            &patterns
        ));
    }

    #[test]
    #[cfg(not(feature = "gix-pathmatch"))]
    fn matches_any_tracked_catch_all() {
        let patterns = vec!["**/*".to_owned()];
        assert!(matches_any_tracked_legacy(
            Path::new("anything.xyz"),
            &patterns
        ));
    }

    #[test]
    #[cfg(not(feature = "gix-pathmatch"))]
    fn matches_any_tracked_exact_match() {
        let patterns = vec!["model.bin".to_owned()];
        assert!(matches_any_tracked_legacy(
            Path::new("model.bin"),
            &patterns
        ));
        assert!(!matches_any_tracked_legacy(
            Path::new("other.bin"),
            &patterns
        ));
    }

    #[test]
    #[cfg(not(feature = "gix-pathmatch"))]
    fn parse_gitattributes_extracts_crab_patterns() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".gitattributes"),
            "*.bin filter=crab diff=crab merge=crab -text\n\
             *.txt text\n\
             *.safetensors filter=crab diff=crab merge=crab -text\n",
        )
        .unwrap();

        let globs = parse_gitattributes_globs_legacy(dir.path()).unwrap();
        assert_eq!(globs, vec!["*.bin", "*.safetensors"]);
    }

    #[test]
    #[cfg(not(feature = "gix-pathmatch"))]
    fn parse_gitattributes_returns_empty_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        let globs = parse_gitattributes_globs_legacy(dir.path()).unwrap();
        assert!(globs.is_empty());
    }

    // Coverage under gix-pathmatch: classifier detects the same shapes
    // the legacy helpers do. Full cross-site coverage lives in
    // `tests/pathmatch_cross_site_parity.rs`.
    #[test]
    #[cfg(feature = "gix-pathmatch")]
    fn classifier_detects_crab_tracked_extension() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitattributes"), "*.bin filter=crab\n").unwrap();
        let cls = TrackedClassifier::open(dir.path()).unwrap();
        assert!(!cls.is_empty());
        assert!(cls.is_tracked(Path::new("model.bin")));
        assert!(!cls.is_tracked(Path::new("model.txt")));
    }

    #[test]
    fn classifier_matches_root_pathspec_for_nested_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".gitattributes"),
            "qualification/*.bin filter=crab\n",
        )
        .unwrap();
        let cls = TrackedClassifier::open(dir.path()).unwrap();

        assert!(cls.is_tracked(Path::new("qualification/model.bin")));
        assert!(!cls.is_tracked(Path::new("other/model.bin")));
    }

    #[test]
    #[cfg(feature = "gix-pathmatch")]
    fn classifier_is_empty_without_crab_filter_patterns() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitattributes"), "*.txt text\n").unwrap();
        let cls = TrackedClassifier::open(dir.path()).unwrap();

        assert!(cls.is_empty());
    }

    #[test]
    #[cfg(feature = "gix-pathmatch")]
    fn classifier_is_not_empty_for_nested_crab_filter_patterns() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("models")).unwrap();
        std::fs::write(
            dir.path().join("models").join(".gitattributes"),
            "*.bin filter=crab\n",
        )
        .unwrap();
        let cls = TrackedClassifier::open(dir.path()).unwrap();

        assert!(!cls.is_empty());
        assert!(cls.is_tracked(Path::new("models/model.bin")));
    }

    fn progress_with_file(name: &str, total_bytes: u64) -> (AddProgress, Arc<AddFileProgress>) {
        let progress = AddProgress::new(vec![AddFileProgressSpec {
            name: name.to_owned(),
            total_bytes,
        }]);
        let file = progress.file_progress(0).unwrap();
        (progress, file)
    }

    #[test]
    fn progress_line_starts_chunking_after_streaming_counter_finishes() {
        let (progress, file) = progress_with_file("model.safetensors", 100);
        file.set_state(AddFileState::Running);
        file.bytes_done.store(100, Relaxed);
        file.chunks_done.store(42, Relaxed);

        let line = progress.render_lines(false, 120).remove(0);

        assert!(line.starts_with("Adding:"));
        assert!(line.contains(" 50.0%"));
        assert!(line.contains("##########----------"));
        assert!(line.contains("chunking 0.0%"));
        assert!(line.contains("42 chunks model.safetensors"));
        assert!(!line.contains("100.0%"));
    }

    #[test]
    fn progress_line_advances_during_chunking_pass() {
        let (progress, file) = progress_with_file("model.safetensors", 100);
        file.set_state(AddFileState::Running);
        file.bytes_done.store(100, Relaxed);
        file.chunk_bytes_done.store(40, Relaxed);
        file.chunks_done.store(42, Relaxed);

        let line = progress.render_lines(false, 120).remove(0);

        assert!(line.starts_with("Adding:"));
        assert!(line.contains(" 70.0%"));
        assert!(line.contains("##############------"));
        assert!(line.contains("chunking 40.0%"));
        assert!(line.contains("42 chunks model.safetensors"));
    }

    #[test]
    fn progress_line_fits_single_terminal_row() {
        let total = 4 * 1024 * 1024 * 1024;
        let (progress, file) = progress_with_file("model.safetensors", total);
        file.set_state(AddFileState::Running);
        file.bytes_done.store(total, Relaxed);
        file.chunk_bytes_done.store(total / 2, Relaxed);
        file.chunks_done.store(13_451, Relaxed);

        let line = progress.render_lines(false, 80).remove(0);

        assert!(visible_width(&line) <= 80, "{line}");
        assert!(!line.contains('\n'));
    }

    #[test]
    fn progress_line_keeps_streaming_bar_incomplete_before_bytes_finish() {
        let (progress, file) = progress_with_file("model.bin", 1000);
        file.set_state(AddFileState::Running);
        file.bytes_done.store(990, Relaxed);

        let line = progress.render_lines(false, 120).remove(0);

        assert!(line.starts_with("Adding:"));
        assert!(line.contains(" 49.5%"));
        assert!(line.contains("##########----------"));
        assert!(line.contains("streaming 99.0%"));
    }

    #[test]
    fn progress_line_reaches_complete_after_file_finishes() {
        let (progress, file) = progress_with_file("model.bin", 100);
        file.bytes_done.store(100, Relaxed);
        file.chunk_bytes_done.store(100, Relaxed);
        file.set_state(AddFileState::Done);
        progress.files_done.store(1, Relaxed);

        let line = progress.render_lines(false, 120).remove(0);

        assert!(line.starts_with("Added:"));
        assert!(line.contains("100.0%"));
        assert!(line.contains("####################"));
    }

    #[test]
    fn progress_line_keeps_indexing_bar_incomplete_until_all_files_finish() {
        let progress = AddProgress::new(
            (0..100)
                .map(|i| AddFileProgressSpec {
                    name: format!("model-{i}.bin"),
                    total_bytes: 10,
                })
                .collect(),
        );
        progress.set_phase(AddPhase::Indexing);
        progress.files_done.store(99, Relaxed);

        let line = progress.render_lines(false, 120).remove(0);

        assert!(line.starts_with("Indexing:"));
        assert!(line.contains(" 99.0%"));
        assert!(line.contains("###################-"));
        assert!(line.contains("(99/100)"));
    }

    #[test]
    fn progress_line_reports_push_plan_phase() {
        let progress = AddProgress::new(
            (0..4)
                .map(|i| AddFileProgressSpec {
                    name: format!("model-{i}.bin"),
                    total_bytes: 10,
                })
                .collect(),
        );
        progress.set_phase(AddPhase::Planning);
        progress.update_plan_summary(&crab_staging::push_plan::AddPushPlanSummary {
            files: 2,
            chunks: 128,
            remote_lookup: true,
            existing_candidates: 8,
            prepared_cache_xorbs: 3,
            prepared_xorbs: 5,
            prepared_bytes: 4096,
            ..Default::default()
        });

        let line = progress.render_lines(false, 160).remove(0);

        assert!(line.starts_with("Planning:"));
        assert!(line.contains(" 50.0%"));
        assert!(line.contains("(2/4)"));
        assert!(line.contains("128 chunks"));
        assert!(line.contains("5 xorbs"));
        assert!(line.contains("cache 3"));
        assert!(line.contains("remote on, 8 hits"));
    }

    #[test]
    fn failed_add_does_not_publish_partial_git_index() {
        let args = AddArgs {
            patterns: vec!["*.bin".to_owned()],
            jobs: 2,
            dry_run: false,
            skip_git_add: false,
            mode: OutputMode::Text,
        };
        let summary = AddSummary {
            files_staged: 1,
            files_skipped: 0,
            files_failed: 1,
            chunks_staged: 4,
            bytes_processed: 1024,
            staging_duration_ms: 0,
            planning_duration_ms: 0,
            flushing_duration_ms: 0,
            indexing_duration_ms: 0,
            duration_ms: 0,
            ..AddSummary::default()
        };

        assert!(!should_publish_git_index(&args, &summary, 1));
    }

    #[test]
    fn failed_add_summary_drops_rolled_back_entries() {
        let mut summary = AddSummary {
            files_staged: 1,
            files_skipped: 0,
            files_failed: 1,
            chunks_staged: 4,
            bytes_processed: 1024,
            staging_duration_ms: 0,
            planning_duration_ms: 0,
            flushing_duration_ms: 0,
            indexing_duration_ms: 0,
            duration_ms: 0,
            ..AddSummary::default()
        };
        let mut entries = vec![staged_entry(PathBuf::from("model.bin"), [7; 32], 1024)];

        clear_rolled_back_summary(&mut summary, &mut entries);

        assert_eq!(summary.files_staged, 0);
        assert_eq!(summary.files_failed, 1);
        assert_eq!(summary.chunks_staged, 0);
        assert_eq!(summary.bytes_processed, 0);
        assert!(entries.is_empty());
    }

    #[tokio::test]
    async fn stream_prepared_plan_writer_writes_verified_push_plan() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("model.bin");
        std::fs::write(&path, vec![0xAB; 2 * 1024 * 1024]).unwrap();
        let staging = StagingArea::open(dir.path().join(".crab/staging"))
            .await
            .unwrap();

        let result = crate::cmd::stream_stage::stage_file_streaming(
            &path,
            dir.path(),
            &staging,
            crate::cmd::stream_stage::StreamStageProgress {
                xorb_builder: Some(crate::cmd::stream_stage::StreamStageXorbBuilder::new(
                    1,
                    XorbBuilder::new,
                )),
                ..crate::cmd::stream_stage::StreamStageProgress::default()
            },
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        let batch_id = result.batch_id.clone();
        let entries = vec![StagedEntry {
            batch_id: Some(batch_id.clone()),
            abs_path: result.abs_path,
            file_hash: result.file_hash,
            size: result.size,
            recipe: result.recipe,
            prepared_xorbs: result.prepared_xorbs,
            index_stat: result.index_stat,
        }];

        assert!(can_use_stream_prepared_plans(&entries));
        let mut progress_calls = 0_u64;
        let summary =
            write_stream_prepared_push_plans(&staging, &entries, &mut |_| progress_calls += 1)
                .await
                .unwrap();
        staging.mark_batch_published(&batch_id).unwrap();
        let file_hash = MerkleHash::from(entries[0].file_hash);
        let plan = staging
            .load_file_push_plan(&file_hash)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(summary.files, 1);
        assert_eq!(summary.chunks, entries[0].recipe.chunk_count());
        assert_eq!(
            summary.prepared_xorbs,
            entries[0].prepared_xorbs.len() as u64
        );
        assert_eq!(progress_calls, 1);
        assert!(plan.staged_chunk_sequence_verified);
        assert_eq!(plan.prepared_xorbs.len(), entries[0].prepared_xorbs.len());
        let planned_chunks: std::collections::HashSet<_> = plan
            .prepared_xorbs
            .iter()
            .flat_map(|xorb| xorb.placements.iter())
            .map(|placement| {
                (
                    MerkleHash::from_hex(&placement.chunk_hash).unwrap(),
                    u64::from(placement.uncompressed_size),
                )
            })
            .collect();
        let recipe_chunks = staging
            .chunks_for_file_with_sizes(&file_hash)
            .unwrap()
            .into_iter()
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(planned_chunks, recipe_chunks);
        assert_eq!(plan.chunk_count, entries[0].recipe.chunk_count());
        assert_eq!(
            plan.sequence_hash().unwrap(),
            entries[0].recipe.sequence_hash()
        );
        for prepared in &entries[0].prepared_xorbs {
            assert!(prepared_xorb_path(staging.root(), &prepared.hash).exists());
            assert!(
                !prepared.payload_path.exists(),
                "stream-prepared temp file should be moved into push-plan cache"
            );
        }
    }

    #[test]
    fn successful_add_publishes_git_index_when_enabled() {
        let args = AddArgs {
            patterns: vec!["*.bin".to_owned()],
            jobs: 2,
            dry_run: false,
            skip_git_add: false,
            mode: OutputMode::Text,
        };
        let summary = AddSummary {
            files_staged: 1,
            files_skipped: 0,
            files_failed: 0,
            chunks_staged: 4,
            bytes_processed: 1024,
            staging_duration_ms: 0,
            planning_duration_ms: 0,
            flushing_duration_ms: 0,
            indexing_duration_ms: 0,
            duration_ms: 0,
            ..AddSummary::default()
        };

        assert!(should_publish_git_index(&args, &summary, 1));
    }

    #[test]
    fn add_jobs_zero_clamps_to_single_worker() {
        assert_eq!(effective_add_jobs(0), 1);
        assert_eq!(effective_add_jobs(4), 4);
    }

    #[test]
    fn stream_prepared_xorbs_allow_single_small_file() {
        assert!(stream_prepared_xorbs_are_efficient([1024], 16 * 1024));
    }

    #[test]
    fn stream_prepared_xorbs_skip_multi_file_tiny_xorbs() {
        assert!(!stream_prepared_xorbs_are_efficient(
            [1024, 32 * 1024],
            16 * 1024
        ));
    }

    #[test]
    fn stream_prepared_xorbs_allow_multi_file_full_xorbs() {
        assert!(stream_prepared_xorbs_are_efficient(
            [16 * 1024, 32 * 1024],
            16 * 1024
        ));
    }

    #[test]
    fn add_execution_plans_use_direct_xorb_authority_for_crab_remote() {
        let _cwd_guard = CWD_LOCK.lock().unwrap();
        let _git_env = crate::test::git_repo::CleanGitEnvGuard::new();
        let dir = tempfile::tempdir().unwrap();
        if !init_git_repo(dir.path()) {
            eprintln!("SKIP: git init failed");
            return;
        }
        let remote = std::process::Command::new("git")
            .args(["remote", "add", "origin", "crab://bucket/repo"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(remote.success());
        let _dir_guard = CurrentDirGuard::enter(dir.path());

        let candidates = vec![(PathBuf::from("model.bin"), 64 * 1024 * 1024)];
        let plans = add_execution_plans(&candidates, 64 * 1024 * 1024);

        assert!(plans.stream_xorb_plan.is_some());
    }

    #[test]
    fn add_execution_plans_defer_tiny_files_to_push() {
        let _cwd_guard = CWD_LOCK.lock().unwrap();
        let _git_env = crate::test::git_repo::CleanGitEnvGuard::new();
        let dir = tempfile::tempdir().unwrap();
        if !init_git_repo(dir.path()) {
            eprintln!("SKIP: git init failed");
            return;
        }
        let remote = std::process::Command::new("git")
            .args(["remote", "add", "origin", "crab://bucket/repo"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(remote.success());
        let _dir_guard = CurrentDirGuard::enter(dir.path());

        let candidates = vec![
            (PathBuf::from("first.bin"), 512),
            (PathBuf::from("second.bin"), 512),
        ];
        let plans = add_execution_plans(&candidates, 1024);

        assert!(plans.stream_xorb_plan.is_none());
    }

    #[test]
    fn stream_prepared_xorb_enabled_paths_skips_likely_duplicate_payloads() {
        let dir = tempfile::tempdir().unwrap();
        let first = dir.path().join("first.bin");
        let second = dir.path().join("second.bin");
        let unique = dir.path().join("unique.bin");
        std::fs::write(&first, vec![0xAB; 4 * 1024 * 1024]).unwrap();
        std::fs::copy(&first, &second).unwrap();
        let mut unique_bytes = vec![0xAB; 4 * 1024 * 1024];
        unique_bytes[2 * 1024 * 1024] = 0xCD;
        std::fs::write(&unique, unique_bytes).unwrap();

        let candidates = vec![
            (first.clone(), 4 * 1024 * 1024),
            (second.clone(), 4 * 1024 * 1024),
            (unique.clone(), 4 * 1024 * 1024),
        ];
        let enabled = stream_prepared_xorb_enabled_paths(&candidates, 1024 * 1024, 1024 * 1024);

        assert!(enabled.contains(&first));
        assert!(!enabled.contains(&second));
        assert!(enabled.contains(&unique));
    }

    #[test]
    fn duplicate_reuse_plan_serializes_candidates_at_stream_threshold() {
        let fingerprint = CandidateFingerprint {
            size: 16 * 1024 * 1024,
            head_hash: [1; 32],
            middle_hash: [2; 32],
            tail_hash: [3; 32],
        };
        let first = PathBuf::from("first.bin");
        let second = PathBuf::from("second.bin");
        let plan = duplicate_reuse_plan_from_fingerprints(&[
            CandidateFingerprintRecord {
                path: first.clone(),
                size: fingerprint.size,
                fingerprint: fingerprint.clone(),
            },
            CandidateFingerprintRecord {
                path: second.clone(),
                size: fingerprint.size,
                fingerprint,
            },
        ]);

        assert_eq!(plan.representative_for(&second), Some(first.as_path()));
    }

    #[tokio::test]
    async fn stream_prepared_plan_writer_reuses_duplicate_file_hash_plan() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("model.bin");
        std::fs::write(&path, vec![0xAB; 2 * 1024 * 1024]).unwrap();
        let staging = StagingArea::open(dir.path().join(".crab/staging"))
            .await
            .unwrap();

        let result = crate::cmd::stream_stage::stage_file_streaming(
            &path,
            dir.path(),
            &staging,
            crate::cmd::stream_stage::StreamStageProgress {
                xorb_builder: Some(crate::cmd::stream_stage::StreamStageXorbBuilder::new(
                    1,
                    XorbBuilder::new,
                )),
                ..crate::cmd::stream_stage::StreamStageProgress::default()
            },
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        let duplicate = StagedEntry {
            batch_id: None,
            abs_path: path.with_file_name("duplicate.bin"),
            file_hash: result.file_hash,
            size: result.size,
            recipe: result.recipe.clone(),
            prepared_xorbs: Vec::new(),
            index_stat: result.index_stat,
        };
        let entries = vec![
            StagedEntry {
                batch_id: None,
                abs_path: result.abs_path,
                file_hash: result.file_hash,
                size: result.size,
                recipe: result.recipe,
                prepared_xorbs: result.prepared_xorbs,
                index_stat: result.index_stat,
            },
            duplicate,
        ];

        assert!(can_use_stream_prepared_plans(&entries));
        let mut progress_calls = 0_u64;
        let summary =
            write_stream_prepared_push_plans(&staging, &entries, &mut |_| progress_calls += 1)
                .await
                .unwrap();

        assert_eq!(summary.files, 2);
        assert_eq!(progress_calls, 1);
    }

    #[test]
    fn progress_lines_render_running_files_separately() {
        let progress = AddProgress::new(vec![
            AddFileProgressSpec {
                name: "a.bin".to_owned(),
                total_bytes: 64 * 1024 * 1024,
            },
            AddFileProgressSpec {
                name: "b.bin".to_owned(),
                total_bytes: 64 * 1024 * 1024,
            },
            AddFileProgressSpec {
                name: "queued.bin".to_owned(),
                total_bytes: 64 * 1024 * 1024,
            },
        ]);
        let first = progress.file_progress(0).unwrap();
        let second = progress.file_progress(1).unwrap();
        first.set_state(AddFileState::Running);
        first.bytes_done.store(64 * 1024 * 1024, Relaxed);
        first.chunks_done.store(512, Relaxed);
        second.set_state(AddFileState::Running);
        second.bytes_done.store(64 * 1024 * 1024, Relaxed);
        second.chunks_done.store(512, Relaxed);

        let lines = progress.render_lines(false, 80);

        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("a.bin"));
        assert!(lines[1].contains("b.bin"));
        assert!(!lines.iter().any(|line| line.contains("queued.bin")));
        assert!(lines.iter().all(|line| visible_width(line) <= 80));
    }

    #[test]
    #[cfg(unix)]
    fn direct_index_writer_preserves_executable_mode() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let init = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        if !init.success() {
            eprintln!("SKIP: git init failed");
            return;
        }
        let _ = std::process::Command::new("git")
            .args(["config", "core.filemode", "true"])
            .current_dir(dir.path())
            .status();

        let path = dir.path().join("model.bin");
        std::fs::write(&path, b"model payload").unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(perms.mode() | 0o111);
        std::fs::set_permissions(&path, perms).unwrap();

        let shard_hints = crate::cache::ShardHintCache::new();
        write_pointers_to_git_index(
            &[staged_entry(path, [7_u8; 32], 13)],
            dir.path(),
            &shard_hints,
            || {},
        )
        .unwrap();

        let ls_files = std::process::Command::new("git")
            .args(["ls-files", "-s", "model.bin"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(
            ls_files.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&ls_files.stderr)
        );
        let stdout = String::from_utf8_lossy(&ls_files.stdout);
        assert!(
            stdout.starts_with("100755 "),
            "expected executable index mode, got {stdout:?}"
        );
    }

    #[test]
    #[cfg(unix)]
    fn direct_index_writer_preserves_literal_path_bytes() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let dir = tempfile::tempdir().unwrap();
        let init = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        if !init.success() {
            eprintln!("SKIP: git init failed");
            return;
        }

        let raw_name = b"model,one\nnext.bin".to_vec();
        let path = dir.path().join(OsString::from_vec(raw_name.clone()));
        std::fs::write(&path, b"model payload").unwrap();

        let shard_hints = crate::cache::ShardHintCache::new();
        write_pointers_to_git_index(
            &[staged_entry(path, [8_u8; 32], 13)],
            dir.path(),
            &shard_hints,
            || {},
        )
        .unwrap();

        let ls_files = std::process::Command::new("git")
            .args(["ls-files", "-z"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(
            ls_files.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&ls_files.stderr)
        );
        assert_eq!(
            ls_files.stdout,
            [raw_name.as_slice(), b"\0"].concat(),
            "index path bytes must match the worktree path"
        );
    }

    #[test]
    fn direct_index_writer_preserves_update_completed_before_index_lock() {
        let dir = tempfile::tempdir().unwrap();
        if !init_git_repo(dir.path()) {
            eprintln!("SKIP: git init failed");
            return;
        }
        let selected = dir.path().join("selected.bin");
        let unrelated = dir.path().join("unrelated.txt");
        std::fs::write(&selected, b"selected payload").unwrap();
        std::fs::write(&unrelated, b"unrelated payload").unwrap();

        let shard_hints = crate::cache::ShardHintCache::new();
        write_pointers_to_git_index(
            &[staged_entry(selected, [42_u8; 32], 16)],
            dir.path(),
            &shard_hints,
            || {
                let status = std::process::Command::new("git")
                    .args(["add", "--", "unrelated.txt"])
                    .current_dir(dir.path())
                    .status()
                    .unwrap();
                assert!(status.success());
            },
        )
        .unwrap();

        let output = std::process::Command::new("git")
            .args(["ls-files", "-z"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            output.stdout, b"selected.bin\0unrelated.txt\0",
            "lock-owned reread must preserve the unrelated index update"
        );
    }

    #[test]
    fn direct_index_writer_stages_only_generated_tracking_rule() {
        let dir = tempfile::tempdir().unwrap();
        if !init_git_repo(dir.path()) {
            eprintln!("SKIP: git init failed");
            return;
        }

        let attributes = dir.path().join(".gitattributes");
        std::fs::write(&attributes, "# staged\n").unwrap();
        let status = std::process::Command::new("git")
            .args(["add", "--", ".gitattributes"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(status.success());
        std::fs::write(&attributes, "# staged\n# unstaged\n").unwrap();
        crate::cmd::track::run_track_in("*.bin", dir.path()).unwrap();

        let path = dir.path().join("model.bin");
        let payload = b"model payload";
        std::fs::write(&path, payload).unwrap();
        let staged = staged_entry(
            path,
            *blake3::hash(payload).as_bytes(),
            payload.len() as u64,
        );
        write_pointers_and_tracking_to_git_index(
            &[IndexPublicationEntry::from(&staged)],
            dir.path(),
            &crate::cache::ShardHintCache::new(),
            &["*.bin".to_owned()],
            |_, _| Ok(()),
            || {},
        )
        .unwrap();

        let indexed = std::process::Command::new("git")
            .args(["show", ":.gitattributes"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(indexed.status.success());
        assert_eq!(
            String::from_utf8_lossy(&indexed.stdout),
            "# staged\n*.bin filter=crab diff=crab merge=crab -text\n"
        );
        assert_eq!(
            std::fs::read_to_string(attributes).unwrap(),
            "# staged\n# unstaged\n*.bin filter=crab diff=crab merge=crab -text\n"
        );
    }

    #[test]
    fn direct_index_writer_replaces_multiple_entries_without_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        if !init_git_repo(dir.path()) {
            eprintln!("SKIP: git init failed");
            return;
        }

        let mut entries = Vec::new();
        for index in 0_u8..5 {
            let path = dir.path().join(format!("model-{index}.bin"));
            let payload = vec![index; usize::from(index) + 1];
            std::fs::write(&path, &payload).unwrap();
            entries.push(staged_entry(
                path,
                *blake3::hash(&payload).as_bytes(),
                payload.len() as u64,
            ));
        }
        let initial = std::process::Command::new("git")
            .args([
                "add",
                "--",
                "model-0.bin",
                "model-1.bin",
                "model-2.bin",
                "model-3.bin",
                "model-4.bin",
            ])
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert!(initial.success());

        write_pointers_to_git_index(
            &entries,
            dir.path(),
            &crate::cache::ShardHintCache::new(),
            || {},
        )
        .unwrap();

        let output = std::process::Command::new("git")
            .args(["ls-files", "--stage", "-z"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(output.status.success());
        assert_eq!(
            output.stdout.iter().filter(|byte| **byte == 0).count(),
            entries.len(),
            "each selected path must have exactly one stage-0 index entry"
        );
    }

    #[test]
    #[cfg(unix)]
    fn git_index_path_bstring_preserves_non_utf8_bytes() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let raw_name = b"model-\xFF.bin".to_vec();
        let path = PathBuf::from(OsString::from_vec(raw_name.clone()));
        let bstring = git_index_path_bstring(&path);
        let got: &[u8] = bstring.as_ref();

        assert_eq!(got, raw_name.as_slice());
    }

    #[test]
    fn native_pointer_blob_writer_creates_git_readable_blob() {
        let dir = tempfile::tempdir().unwrap();
        if !init_git_repo(dir.path()) {
            eprintln!("SKIP: git init failed");
            return;
        }

        let payload = b"version https://crab.build/spec/v1\n";
        let oid = write_pointer_blob(dir.path(), payload).unwrap();

        let kind = std::process::Command::new("git")
            .args(["cat-file", "-t", &oid])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(
            kind.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&kind.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&kind.stdout).trim(), "blob");

        let data = std::process::Command::new("git")
            .args(["cat-file", "-p", &oid])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(
            data.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&data.stderr)
        );
        assert_eq!(data.stdout, payload);
    }

    #[test]
    #[cfg(unix)]
    fn direct_index_writer_populates_stat_cache_for_non_utf8_path() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let dir = tempfile::tempdir().unwrap();
        if !init_git_repo(dir.path()) {
            eprintln!("SKIP: git init failed");
            return;
        }

        let raw_name = b"model-\xFF.bin".to_vec();
        let path = dir.path().join(OsString::from_vec(raw_name.clone()));
        if let Err(err) = std::fs::write(&path, b"model payload") {
            eprintln!("SKIP: filesystem rejected non-UTF-8 path: {err}");
            return;
        }

        let shard_hints = crate::cache::ShardHintCache::new();
        write_pointers_to_git_index(
            &[staged_entry(path, [9_u8; 32], 13)],
            dir.path(),
            &shard_hints,
            || {},
        )
        .unwrap();

        let status = std::process::Command::new("git")
            .args(["status", "--porcelain", "-z"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(
            status.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&status.stderr)
        );
        assert_eq!(
            status.stdout,
            [b"A  ".as_slice(), raw_name.as_slice(), b"\0"].concat(),
            "stat cache must avoid an unstaged change for literal path bytes"
        );
    }

    #[tokio::test]
    async fn clean_index_filter_skips_non_racy_crab_pointer_entry() {
        let dir = tempfile::tempdir().unwrap();
        if !init_git_repo(dir.path()) {
            eprintln!("SKIP: git init failed");
            return;
        }

        let path = dir.path().join("model.bin");
        let payload = b"model payload";
        std::fs::write(&path, payload).unwrap();
        make_file_mtime_old(&path);

        let shard_hints = crate::cache::ShardHintCache::new();
        write_pointers_to_git_index(
            &[staged_entry(
                path.clone(),
                *blake3::hash(payload).as_bytes(),
                payload.len() as u64,
            )],
            dir.path(),
            &shard_hints,
            || {},
        )
        .unwrap();

        let (to_process, skipped) = filter_clean_indexed_candidates(
            dir.path(),
            vec![(path, payload.len() as u64)],
            1,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert!(to_process.is_empty());
        assert_eq!(skipped.files, 1);
        assert_eq!(skipped.bytes, payload.len() as u64);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn clean_index_filter_reuses_verified_add_validation() {
        let dir = tempfile::tempdir().unwrap();
        if !init_git_repo(dir.path()) {
            eprintln!("SKIP: git init failed");
            return;
        }

        let path = dir.path().join("model.bin");
        let payload = b"model payload";
        std::fs::write(&path, payload).unwrap();
        make_file_mtime_old(&path);

        write_pointers_to_git_index(
            &[staged_entry(
                path.clone(),
                *blake3::hash(payload).as_bytes(),
                payload.len() as u64,
            )],
            dir.path(),
            &crate::cache::ShardHintCache::new(),
            || {},
        )
        .unwrap();
        crate::cache::add_validation::record_verified_paths(
            dir.path(),
            &[VerifiedPath {
                path: path.clone(),
                file_hash: *blake3::hash(payload).as_bytes(),
                size: payload.len() as u64,
                index_stat: crate::cmd::stream_stage::VerifiedIndexStat::from_path_no_follow(&path)
                    .unwrap(),
            }],
        )
        .unwrap();

        let (to_process, skipped) = filter_clean_indexed_candidates(
            dir.path(),
            vec![(path, payload.len() as u64)],
            1,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert!(to_process.is_empty());
        assert_eq!(skipped.files, 1);
        assert_eq!(skipped.validation_cache_hits, 1);
        assert_eq!(skipped.validation_cache_hit_bytes, payload.len() as u64);
    }

    #[test]
    #[cfg(unix)]
    fn add_validation_rejects_file_changed_after_verified_read() {
        let dir = tempfile::tempdir().unwrap();
        if !init_git_repo(dir.path()) {
            eprintln!("SKIP: git init failed");
            return;
        }

        let path = dir.path().join("model.bin");
        let original = b"original payload";
        let replacement = b"replaced payload";
        assert_eq!(original.len(), replacement.len());
        std::fs::write(&path, original).unwrap();
        make_file_mtime_old(&path);
        let file_hash = *blake3::hash(original).as_bytes();
        let index_stat =
            crate::cmd::stream_stage::VerifiedIndexStat::from_path_no_follow(&path).unwrap();
        write_pointers_to_git_index(
            &[staged_entry(path.clone(), file_hash, original.len() as u64)],
            dir.path(),
            &crate::cache::ShardHintCache::new(),
            || {},
        )
        .unwrap();

        std::fs::write(&path, replacement).unwrap();
        crate::git::worktree::refresh_index_stats(dir.path(), std::slice::from_ref(&path)).unwrap();

        assert_eq!(
            crate::cache::add_validation::record_verified_paths(
                dir.path(),
                &[VerifiedPath {
                    path,
                    file_hash,
                    size: original.len() as u64,
                    index_stat,
                }],
            )
            .unwrap(),
            0
        );
    }

    #[test]
    #[cfg(unix)]
    fn add_validation_rejects_index_pointer_changed_after_verified_read() {
        let dir = tempfile::tempdir().unwrap();
        if !init_git_repo(dir.path()) {
            eprintln!("SKIP: git init failed");
            return;
        }

        let path = dir.path().join("model.bin");
        let payload = b"model payload";
        std::fs::write(&path, payload).unwrap();
        make_file_mtime_old(&path);
        let file_hash = *blake3::hash(payload).as_bytes();
        let index_stat =
            crate::cmd::stream_stage::VerifiedIndexStat::from_path_no_follow(&path).unwrap();
        write_pointers_to_git_index(
            &[staged_entry(path.clone(), file_hash, payload.len() as u64)],
            dir.path(),
            &crate::cache::ShardHintCache::new(),
            || {},
        )
        .unwrap();
        let mut other_hash = file_hash;
        other_hash[0] ^= 0xff;
        write_pointers_to_git_index(
            &[staged_entry(path.clone(), other_hash, payload.len() as u64)],
            dir.path(),
            &crate::cache::ShardHintCache::new(),
            || {},
        )
        .unwrap();

        assert_eq!(
            crate::cache::add_validation::record_verified_paths(
                dir.path(),
                &[VerifiedPath {
                    path,
                    file_hash,
                    size: payload.len() as u64,
                    index_stat,
                }],
            )
            .unwrap(),
            0
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn clean_index_filter_rehashes_same_size_same_mtime_replacement() {
        let dir = tempfile::tempdir().unwrap();
        if !init_git_repo(dir.path()) {
            eprintln!("SKIP: git init failed");
            return;
        }

        let path = dir.path().join("model.bin");
        let original = b"original payload";
        let replacement = b"replaced payload";
        assert_eq!(original.len(), replacement.len());
        std::fs::write(&path, original).unwrap();
        make_file_mtime_old(&path);
        write_pointers_to_git_index(
            &[staged_entry(
                path.clone(),
                *blake3::hash(original).as_bytes(),
                original.len() as u64,
            )],
            dir.path(),
            &crate::cache::ShardHintCache::new(),
            || {},
        )
        .unwrap();
        crate::cache::add_validation::record_verified_paths(
            dir.path(),
            &[VerifiedPath {
                path: path.clone(),
                file_hash: *blake3::hash(original).as_bytes(),
                size: original.len() as u64,
                index_stat: crate::cmd::stream_stage::VerifiedIndexStat::from_path_no_follow(&path)
                    .unwrap(),
            }],
        )
        .unwrap();

        std::fs::write(&path, replacement).unwrap();
        make_file_mtime_old(&path);
        let (to_process, skipped) = filter_clean_indexed_candidates(
            dir.path(),
            vec![(path, replacement.len() as u64)],
            1,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(to_process.len(), 1);
        assert_eq!(skipped.files, 0);
        assert_eq!(skipped.validation_cache_hits, 0);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn clean_index_filter_hashes_racy_pointer_once_then_uses_validation() {
        use bstr::ByteSlice;

        let dir = tempfile::tempdir().unwrap();
        if !init_git_repo(dir.path()) {
            eprintln!("SKIP: git init failed");
            return;
        }

        let path = dir.path().join("model.bin");
        let payload = b"model payload";
        std::fs::write(&path, payload).unwrap();
        make_file_mtime_old(&path);
        write_pointers_to_git_index(
            &[staged_entry(
                path.clone(),
                *blake3::hash(payload).as_bytes(),
                payload.len() as u64,
            )],
            dir.path(),
            &crate::cache::ShardHintCache::new(),
            || {},
        )
        .unwrap();

        let future = filetime::FileTime::from_system_time(
            std::time::SystemTime::now() + Duration::from_secs(60),
        );
        filetime::set_file_mtime(&path, future).unwrap();
        assert_eq!(
            crate::git::worktree::refresh_index_stats(dir.path(), std::slice::from_ref(&path))
                .unwrap(),
            1
        );
        let context = crate::git::worktree::WorktreeContext::resolve_from_path(dir.path()).unwrap();
        let index = gix_index::File::at(
            context.index_path(),
            gix_hash::Kind::Sha1,
            true,
            gix_index::decode::Options::default(),
        )
        .unwrap();
        let entry = index
            .entry_by_path_and_stage(
                git_index_path_bstring(Path::new("model.bin")).as_bstr(),
                gix_index::entry::Stage::Unconflicted,
            )
            .unwrap();
        assert!(
            entry.stat.is_racy(
                index.timestamp(),
                gix_index::entry::stat::Options::default()
            ),
            "fixture must exercise Git's racy-stat path"
        );

        let (_, first) = filter_clean_indexed_candidates(
            dir.path(),
            vec![(path.clone(), payload.len() as u64)],
            1,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        let (to_process, second) = filter_clean_indexed_candidates(
            dir.path(),
            vec![(path, payload.len() as u64)],
            1,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(first.files, 1);
        assert_eq!(first.validation_cache_hits, 0);
        assert!(to_process.is_empty());
        assert_eq!(second.validation_cache_hits, 1);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn clean_index_filter_does_not_skip_mode_only_change() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        if !init_git_repo(dir.path()) {
            eprintln!("SKIP: git init failed");
            return;
        }
        let _ = std::process::Command::new("git")
            .args(["config", "core.filemode", "true"])
            .current_dir(dir.path())
            .status();

        let path = dir.path().join("model.bin");
        let payload = b"model payload";
        std::fs::write(&path, payload).unwrap();
        make_file_mtime_old(&path);

        let shard_hints = crate::cache::ShardHintCache::new();
        write_pointers_to_git_index(
            &[staged_entry(
                path.clone(),
                *blake3::hash(payload).as_bytes(),
                payload.len() as u64,
            )],
            dir.path(),
            &shard_hints,
            || {},
        )
        .unwrap();

        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(perms.mode() | 0o111);
        std::fs::set_permissions(&path, perms).unwrap();
        make_file_mtime_old(&path);

        let (to_process, skipped) = filter_clean_indexed_candidates(
            dir.path(),
            vec![(path, payload.len() as u64)],
            1,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(to_process.len(), 1);
        assert_eq!(skipped.files, 0);
        assert_eq!(skipped.bytes, 0);
    }

    #[tokio::test]
    async fn clean_index_filter_does_not_trust_matching_stat_and_size() {
        let dir = tempfile::tempdir().unwrap();
        if !init_git_repo(dir.path()) {
            eprintln!("SKIP: git init failed");
            return;
        }

        let path = dir.path().join("model.bin");
        let original = b"original payload";
        let replacement = b"replaced payload";
        assert_eq!(original.len(), replacement.len());
        std::fs::write(&path, replacement).unwrap();
        make_file_mtime_old(&path);

        write_pointers_to_git_index(
            &[staged_entry(
                path.clone(),
                *blake3::hash(original).as_bytes(),
                original.len() as u64,
            )],
            dir.path(),
            &crate::cache::ShardHintCache::new(),
            || {},
        )
        .unwrap();

        let (to_process, skipped) = filter_clean_indexed_candidates(
            dir.path(),
            vec![(path, replacement.len() as u64)],
            1,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(to_process.len(), 1);
        assert_eq!(skipped.files, 0);
        assert_eq!(skipped.bytes, 0);
    }

    #[test]
    fn direct_index_writer_rejects_file_changed_after_staging() {
        let dir = tempfile::tempdir().unwrap();
        if !init_git_repo(dir.path()) {
            eprintln!("SKIP: git init failed");
            return;
        }

        let path = dir.path().join("model.bin");
        let original = b"original model payload";
        std::fs::write(&path, original).unwrap();
        let entry = staged_entry(
            path.clone(),
            *blake3::hash(original).as_bytes(),
            original.len() as u64,
        );
        std::fs::write(&path, b"changed model payload with a different size").unwrap();

        let shard_hints = crate::cache::ShardHintCache::new();
        let err = write_pointers_to_git_index(&[entry], dir.path(), &shard_hints, || {})
            .expect_err("changed file must fail before index publication");
        assert!(matches!(err, GitIndexWriteError::BeforeIndexMutation(_)));

        let status = std::process::Command::new("git")
            .args(["status", "--porcelain", "-z"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(
            status.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&status.stderr)
        );
        assert!(
            status.stdout.starts_with(b"?? "),
            "failed publication must leave the worktree file untracked"
        );
    }

    #[test]
    fn direct_index_writer_does_not_publish_partial_batch_on_failure() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let init = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo)
            .status()
            .unwrap();
        if !init.success() {
            eprintln!("SKIP: git init failed");
            return;
        }

        let good_path = repo.join("model.bin");
        let bad_path = repo.join("bad.bin");
        std::fs::write(&good_path, b"model payload").unwrap();
        std::fs::write(&bad_path, b"bad payload").unwrap();

        let sha = write_pointer_blob(&repo, b"pointer").unwrap();
        let good_stat =
            crate::cmd::stream_stage::VerifiedIndexStat::from_path_no_follow(&good_path).unwrap();
        let bad_stat =
            crate::cmd::stream_stage::VerifiedIndexStat::from_path_no_follow(&bad_path).unwrap();
        let err = publish_git_index_entries(
            &[
                GitIndexEntry {
                    abs_path: good_path,
                    mode: gix_index::entry::Mode::FILE,
                    sha: sha.clone(),
                    index_stat: good_stat,
                },
                GitIndexEntry {
                    abs_path: bad_path,
                    mode: gix_index::entry::Mode::FILE,
                    sha: "not-a-git-object".to_owned(),
                    index_stat: bad_stat,
                },
            ],
            &repo,
        )
        .expect_err("invalid object id must reject the batch");
        assert!(matches!(err, GitIndexWriteError::BeforeIndexMutation(_)));

        let ls_files = std::process::Command::new("git")
            .args(["ls-files", "-z"])
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(
            ls_files.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&ls_files.stderr)
        );
        assert!(
            ls_files.stdout.is_empty(),
            "batch failure must not leave sibling index entries"
        );
    }

    #[test]
    fn direct_index_writer_reports_lock_contention_before_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let init = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo)
            .status()
            .unwrap();
        if !init.success() {
            eprintln!("SKIP: git init failed");
            return;
        }

        let path = repo.join("model.bin");
        std::fs::write(&path, b"model payload").unwrap();
        std::fs::write(repo.join(".git/index.lock"), b"stale lock").unwrap();

        let shard_hints = crate::cache::ShardHintCache::new();
        let err = write_pointers_to_git_index(
            &[staged_entry(path, [10_u8; 32], 13)],
            &repo,
            &shard_hints,
            || {},
        )
        .expect_err("locked Git index must fail during publication");

        match err {
            GitIndexWriteError::BeforeIndexMutation(error) => {
                assert!(
                    error.to_string().contains("failed to lock Git index"),
                    "unexpected error: {error}"
                );
            }
            GitIndexWriteError::IndexMutationUncertain(error) => {
                panic!("lock contention must be certain before replacement, got {error}");
            }
        }
    }

    #[tokio::test]
    async fn close_staging_before_indexing_flushes_then_refuses_live_refs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join(".crab/staging");
        let staging = Arc::new(StagingArea::open(root.clone()).await.unwrap());
        let leaked_ref = Arc::clone(&staging);

        let data = b"durable before pointer write".to_vec();
        let file_hash = MerkleHash::from(*blake3::hash(&data).as_bytes());
        let chunk_hash = compute_data_hash(&data);
        staging
            .pre_register_file(&file_hash, data.len() as u64)
            .unwrap();
        let refs = vec![(&chunk_hash, data.as_slice())];
        staging
            .stage_chunks_batch(&refs, &file_hash, 0)
            .await
            .unwrap();

        let err = close_staging_before_indexing(staging)
            .await
            .expect_err("live staging refs must block Git pointer indexing");
        assert!(
            err.to_string().contains("refusing to write Git pointers"),
            "unexpected error: {err}"
        );

        drop(leaked_ref);
        let reopened = StagingArea::open(root).await.unwrap();
        assert_eq!(
            reopened.chunks_for_file(&file_hash).unwrap(),
            vec![chunk_hash],
            "flush must commit the segment boundary before refusing indexing"
        );
        let staged = reopened
            .get_chunk(&chunk_hash)
            .await
            .unwrap()
            .expect("chunk must survive recovery");
        assert_eq!(staged.as_ref(), data.as_slice());
        reopened.close().await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_add_waits_then_observes_the_preceding_index_publication() {
        let dir = tempfile::tempdir().unwrap();
        let repo_root = dir.path().join("repo");
        std::fs::create_dir(&repo_root).unwrap();
        if !init_git_repo(&repo_root) {
            eprintln!("SKIP: git init failed");
            return;
        }
        let path = repo_root.join("model.bin");
        let data = b"concurrent add must observe the preceding pointer";
        std::fs::write(&path, data).unwrap();
        make_file_mtime_old(&path);
        let root = repo_root.join(".crab/staging");
        let holder = StagingArea::open(root.clone()).await.unwrap();
        let waiter = tokio::spawn({
            let root = root.clone();
            let repo_root = repo_root.clone();
            let path = path.clone();
            async move {
                let staging = open_add_staging(&root, &repo_root).await?;
                let filtered = filter_clean_indexed_candidates(
                    &repo_root,
                    vec![(path, data.len() as u64)],
                    1,
                    &CancellationToken::new(),
                )
                .await;
                staging.close().await?;
                filtered
            }
        });

        tokio::time::sleep(Duration::from_millis(150)).await;
        assert!(
            !waiter.is_finished(),
            "a concurrent add must queue behind the active staging writer"
        );

        let pointer = Pointer {
            file_hash: *blake3::hash(data).as_bytes(),
            size: data.len() as u64,
            shard_hint: None,
        }
        .serialize();
        publish_git_index_entries(
            &[GitIndexEntry {
                abs_path: path,
                mode: gix_index::entry::Mode::FILE,
                sha: write_pointer_blob(&repo_root, &pointer).unwrap(),
                index_stat: crate::cmd::stream_stage::VerifiedIndexStat::from_path_no_follow(
                    &repo_root.join("model.bin"),
                )
                .unwrap(),
            }],
            &repo_root,
        )
        .unwrap();
        drop(holder);

        let (candidates, skipped) = tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("queued add should acquire after release")
            .expect("add-open task should not panic")
            .expect("queued add should re-evaluate the current index");
        assert!(candidates.is_empty());
        assert_eq!(skipped.files, 1);
        assert_eq!(skipped.bytes, data.len() as u64);
    }

    #[tokio::test]
    async fn committed_recipe_survives_same_path_restage_until_push() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        if !init_git_repo(&repo) {
            eprintln!("SKIP: git init failed");
            return;
        }
        let staging = StagingArea::open(repo.join(".crab/staging")).await.unwrap();
        let relative_path = PathBuf::from("model.bin");
        let first_data = b"committed staged version";
        let (first_batch, first_hash) =
            stage_publication_recipe(&staging, &relative_path, first_data).await;
        staging.mark_batch_published(&first_batch).unwrap();

        let pointer = Pointer {
            file_hash: first_hash.into(),
            size: first_data.len() as u64,
            shard_hint: None,
        };
        std::fs::write(repo.join(&relative_path), pointer.serialize()).unwrap();
        let add = std::process::Command::new("git")
            .args(["add", "model.bin"])
            .current_dir(&repo)
            .status()
            .unwrap();
        assert!(add.success());
        let commit = std::process::Command::new("git")
            .args([
                "-c",
                "user.name=Crab Test",
                "-c",
                "user.email=crab@example.invalid",
                "commit",
                "-qm",
                "first pointer",
            ])
            .current_dir(&repo)
            .status()
            .unwrap();
        assert!(commit.success());

        let second_data = b"replacement staged version";
        let (second_batch, second_hash) =
            stage_publication_recipe(&staging, &relative_path, second_data).await;
        let mut second_entry = staged_entry(
            repo.join(&relative_path),
            second_hash.into(),
            second_data.len() as u64,
        );
        second_entry.batch_id = Some(second_batch.clone());

        pin_committed_recipes_before_path_replacement(
            &staging,
            &repo,
            std::slice::from_ref(&second_entry),
        )
        .unwrap();
        staging.mark_batch_published(&second_batch).unwrap();

        assert!(
            staging
                .published_recipe_for_file(&first_hash)
                .unwrap()
                .is_some(),
            "the committed first version must remain push-visible"
        );
        assert!(
            staging
                .published_recipe_for_file(&second_hash)
                .unwrap()
                .is_some(),
            "the replacement must become the current path head"
        );
        staging.close().await.unwrap();
    }

    #[tokio::test]
    async fn rollback_open_staging_entries_removes_unpublished_rows() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join(".crab/staging");
        let staging = StagingArea::open(root).await.unwrap();

        let data = b"unpublished successful sibling".to_vec();
        let file_hash_raw = *blake3::hash(&data).as_bytes();
        let file_hash = MerkleHash::from(file_hash_raw);
        let chunk_hash = compute_data_hash(&data);
        staging
            .pre_register_file(&file_hash, data.len() as u64)
            .unwrap();
        staging
            .stage_chunks_batch(&[(&chunk_hash, data.as_slice())], &file_hash, 0)
            .await
            .unwrap();

        let rows = rollback_open_staging_entries(
            &staging,
            &[staged_entry(
                dir.path().join("model.bin"),
                file_hash_raw,
                data.len() as u64,
            )],
            "test rollback",
        )
        .unwrap();

        assert_eq!(rows, 1);
        assert!(staging.chunks_for_file(&file_hash).unwrap().is_empty());
        staging.close().await.unwrap();
    }

    #[tokio::test]
    async fn cancel_before_indexing_rolls_back_unpublished_rows() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join(".crab/staging");
        let staging = StagingArea::open(root).await.unwrap();

        let data = b"cancelled before git index publication".to_vec();
        let file_path = dir.path().join("model.bin");
        std::fs::write(&file_path, &data).unwrap();
        let file_hash_raw = *blake3::hash(&data).as_bytes();
        let file_hash = MerkleHash::from(file_hash_raw);
        let chunk_hash = compute_data_hash(&data);
        staging
            .pre_register_file(&file_hash, data.len() as u64)
            .unwrap();
        staging
            .stage_chunks_batch(&[(&chunk_hash, data.as_slice())], &file_hash, 0)
            .await
            .unwrap();

        let mut summary = AddSummary {
            files_staged: 1,
            files_skipped: 0,
            files_failed: 0,
            chunks_staged: 1,
            bytes_processed: data.len() as u64,
            staging_duration_ms: 0,
            planning_duration_ms: 0,
            flushing_duration_ms: 0,
            indexing_duration_ms: 0,
            duration_ms: 0,
            ..AddSummary::default()
        };
        let mut staged_entries = vec![staged_entry(file_path, file_hash_raw, data.len() as u64)];
        let cancel = CancellationToken::new();
        cancel.cancel();

        let err = abort_if_cancelled_before_indexing(
            &cancel,
            &staging,
            &mut summary,
            &mut staged_entries,
        )
        .expect_err("cancelled add must stop before indexing");

        assert!(matches!(err, CrabError::Cancelled));
        assert!(staged_entries.is_empty());
        assert_eq!(summary.files_staged, 0);
        assert_eq!(summary.chunks_staged, 0);
        assert_eq!(summary.bytes_processed, 0);
        assert!(staging.chunks_for_file(&file_hash).unwrap().is_empty());
        staging.close().await.unwrap();
    }

    #[tokio::test]
    async fn rollback_closed_staging_entries_removes_unpublished_rows() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join(".crab/staging");
        let staging = Arc::new(StagingArea::open(root.clone()).await.unwrap());

        let data = b"unpublished after index failure".to_vec();
        let file_hash_raw = *blake3::hash(&data).as_bytes();
        let file_hash = MerkleHash::from(file_hash_raw);
        let chunk_hash = compute_data_hash(&data);
        staging
            .pre_register_file(&file_hash, data.len() as u64)
            .unwrap();
        staging
            .stage_chunks_batch(&[(&chunk_hash, data.as_slice())], &file_hash, 0)
            .await
            .unwrap();
        close_staging_before_indexing(staging).await.unwrap();

        let rows = rollback_closed_staging_entries(
            &root,
            &[staged_entry(
                dir.path().join("model.bin"),
                file_hash_raw,
                data.len() as u64,
            )],
            "test rollback",
        )
        .await
        .unwrap();

        assert_eq!(rows, 1);
        let reopened = StagingAreaReadOnly::open(root).await.unwrap();
        assert!(reopened.chunks_for_file(&file_hash).unwrap().is_empty());
    }

    #[tokio::test]
    async fn index_publication_failure_preserves_closed_staging_rows() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join(".crab/staging");
        let staging = Arc::new(StagingArea::open(root.clone()).await.unwrap());

        let data = b"preserve after git index uncertainty".to_vec();
        let file_hash_raw = *blake3::hash(&data).as_bytes();
        let file_hash = MerkleHash::from(file_hash_raw);
        let chunk_hash = compute_data_hash(&data);
        staging
            .pre_register_file(&file_hash, data.len() as u64)
            .unwrap();
        staging
            .stage_chunks_batch(&[(&chunk_hash, data.as_slice())], &file_hash, 0)
            .await
            .unwrap();
        close_staging_before_indexing(staging).await.unwrap();

        let error = handle_git_index_write_error(
            &root,
            &[staged_entry(
                dir.path().join("model.bin"),
                file_hash_raw,
                data.len() as u64,
            )],
            None,
            GitIndexWriteError::IndexMutationUncertain(CrabError::Internal(
                "git update-index failed".to_owned(),
            )),
        )
        .await;

        assert!(
            error.to_string().contains("git update-index failed"),
            "unexpected error: {error}"
        );
        let reopened = StagingAreaReadOnly::open(root).await.unwrap();
        assert_eq!(
            reopened.chunks_for_file(&file_hash).unwrap(),
            vec![chunk_hash]
        );
    }

    #[tokio::test]
    async fn exclusive_add_open_recovers_publication_when_every_pointer_is_in_index() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        if !init_git_repo(&repo) {
            eprintln!("SKIP: git init failed");
            return;
        }
        let staging_root = repo.join(".crab/staging");
        let staging = StagingArea::open(staging_root.clone()).await.unwrap();
        let rel_path = PathBuf::from("model.bin");
        let data = b"publication recovery new index state";
        let (batch_id, file_hash) = stage_publication_recipe(&staging, &rel_path, data).await;
        let entry = publication_index_entry(&repo, &rel_path, b"crab pointer v1\n");
        let mut intent = None;

        publish_git_index_entries_with_tracking(
            std::slice::from_ref(&entry),
            &repo,
            &[],
            |entries, index| {
                intent = Some(staging.create_publication_intent(&[PublicationIntentEntry {
                    batch_id,
                    path: rel_path.clone(),
                    expected_pointer_oid: entries[0].sha.clone(),
                    previous_index_state: git_index_path_state(
                        index,
                        git_index_path_bstring(&rel_path).as_bstr(),
                    ),
                }])?);
                Ok(())
            },
        )
        .unwrap();
        assert!(intent.is_some());
        staging.close().await.unwrap();

        let recovered = open_add_staging(&staging_root, &repo).await.unwrap();

        assert!(
            recovered
                .unresolved_publication_intents()
                .unwrap()
                .is_empty()
        );
        assert!(
            recovered
                .published_recipe_for_file(&file_hash)
                .unwrap()
                .is_some(),
            "exact new index state must publish the staged recipe"
        );
    }

    #[tokio::test]
    async fn publication_recovery_rolls_back_when_every_path_has_prior_state() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        if !init_git_repo(&repo) {
            eprintln!("SKIP: git init failed");
            return;
        }
        let staging_root = repo.join(".crab/staging");
        let staging = StagingArea::open(staging_root.clone()).await.unwrap();
        let rel_path = PathBuf::from("model.bin");
        let data = b"publication recovery prior index state";
        let (batch_id, file_hash) = stage_publication_recipe(&staging, &rel_path, data).await;
        let index = read_test_index(&repo);
        staging
            .create_publication_intent(&[PublicationIntentEntry {
                batch_id,
                path: rel_path.clone(),
                expected_pointer_oid: "1111111111111111111111111111111111111111".to_owned(),
                previous_index_state: git_index_path_state(
                    &index,
                    git_index_path_bstring(&rel_path).as_bstr(),
                ),
            }])
            .unwrap();
        staging.close().await.unwrap();

        let recovered = StagingAreaReadOnly::open(staging_root).await.unwrap();
        reconcile_publication_intents(&recovered, &repo)
            .await
            .unwrap();

        assert!(
            recovered
                .unresolved_publication_intents()
                .unwrap()
                .is_empty()
        );
        assert!(
            recovered
                .published_recipe_for_file(&file_hash)
                .unwrap()
                .is_none(),
            "exact prior index state must roll back the unpublished recipe"
        );
    }

    #[tokio::test]
    async fn publication_recovery_rejects_mixed_index_state() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        if !init_git_repo(&repo) {
            eprintln!("SKIP: git init failed");
            return;
        }
        let staging_root = repo.join(".crab/staging");
        let staging = StagingArea::open(staging_root.clone()).await.unwrap();
        let first_path = PathBuf::from("first.bin");
        let second_path = PathBuf::from("second.bin");
        let (first_batch, _) =
            stage_publication_recipe(&staging, &first_path, b"first staged file").await;
        let (second_batch, _) =
            stage_publication_recipe(&staging, &second_path, b"second staged file").await;
        let first_entry = publication_index_entry(&repo, &first_path, b"first pointer\n");
        let second_entry = publication_index_entry(&repo, &second_path, b"second pointer\n");
        let index = read_test_index(&repo);
        staging
            .create_publication_intent(&[
                PublicationIntentEntry {
                    batch_id: first_batch,
                    path: first_path.clone(),
                    expected_pointer_oid: first_entry.sha.clone(),
                    previous_index_state: git_index_path_state(
                        &index,
                        git_index_path_bstring(&first_path).as_bstr(),
                    ),
                },
                PublicationIntentEntry {
                    batch_id: second_batch,
                    path: second_path.clone(),
                    expected_pointer_oid: second_entry.sha.clone(),
                    previous_index_state: git_index_path_state(
                        &index,
                        git_index_path_bstring(&second_path).as_bstr(),
                    ),
                },
            ])
            .unwrap();
        publish_git_index_entries(std::slice::from_ref(&first_entry), &repo).unwrap();
        staging.close().await.unwrap();

        let recovered = StagingAreaReadOnly::open(staging_root).await.unwrap();
        let error = reconcile_publication_intents(&recovered, &repo)
            .await
            .expect_err("mixed publication state must fail closed");

        assert!(error.to_string().contains("ambiguous Git index state"));
        assert_eq!(recovered.unresolved_publication_intents().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn cancel_after_staging_close_rolls_back_unpublished_rows() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join(".crab/staging");
        let staging = Arc::new(StagingArea::open(root.clone()).await.unwrap());

        let data = b"cancelled after staging close".to_vec();
        let file_path = dir.path().join("model.bin");
        std::fs::write(&file_path, &data).unwrap();
        let file_hash_raw = *blake3::hash(&data).as_bytes();
        let file_hash = MerkleHash::from(file_hash_raw);
        let chunk_hash = compute_data_hash(&data);
        staging
            .pre_register_file(&file_hash, data.len() as u64)
            .unwrap();
        staging
            .stage_chunks_batch(&[(&chunk_hash, data.as_slice())], &file_hash, 0)
            .await
            .unwrap();
        close_staging_before_indexing(staging).await.unwrap();

        let mut summary = AddSummary {
            files_staged: 1,
            files_skipped: 0,
            files_failed: 0,
            chunks_staged: 1,
            bytes_processed: data.len() as u64,
            staging_duration_ms: 0,
            planning_duration_ms: 0,
            flushing_duration_ms: 0,
            indexing_duration_ms: 0,
            duration_ms: 0,
            ..AddSummary::default()
        };
        let mut staged_entries = vec![staged_entry(file_path, file_hash_raw, data.len() as u64)];
        let cancel = CancellationToken::new();
        cancel.cancel();

        let err = abort_if_cancelled_after_staging_close(
            &cancel,
            &root,
            &mut summary,
            &mut staged_entries,
        )
        .await
        .expect_err("cancelled add must stop before indexing");

        assert!(matches!(err, CrabError::Cancelled));
        assert!(staged_entries.is_empty());
        assert_eq!(summary.files_staged, 0);
        assert_eq!(summary.chunks_staged, 0);
        assert_eq!(summary.bytes_processed, 0);
        let reopened = StagingAreaReadOnly::open(root).await.unwrap();
        assert!(reopened.chunks_for_file(&file_hash).unwrap().is_empty());
    }

    #[tokio::test]
    async fn process_file_streams_and_stages_chunks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("model.bin");
        let data: Vec<u8> = (0..(2 * 1024 * 1024) as u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 11) as u8)
            .collect();
        std::fs::write(&path, &data).unwrap();
        let staging = StagingArea::open(dir.path().join(".crab/staging"))
            .await
            .unwrap();
        let progress = Arc::new(AddProgress::new(vec![AddFileProgressSpec {
            name: "model.bin".to_owned(),
            total_bytes: data.len() as u64,
        }]));
        let file_progress = progress.file_progress(0).unwrap();

        let result = process_file(
            &path,
            dir.path(),
            &staging,
            &progress,
            &file_progress,
            None,
            None,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(result.size, data.len() as u64);
        assert_eq!(result.file_hash, *blake3::hash(&data).as_bytes());
        assert_eq!(file_progress.bytes_done.load(Relaxed), data.len() as u64);
        assert_eq!(
            file_progress.chunk_bytes_done.load(Relaxed),
            data.len() as u64
        );
        assert_eq!(file_progress.state(), AddFileState::Done);
        assert_eq!(progress.files_done.load(Relaxed), 1);

        let staged = staging
            .chunks_for_file(&MerkleHash::from(result.file_hash))
            .unwrap();
        assert_eq!(staged.len(), result.chunks);
        assert_eq!(result.recipe.chunk_count(), result.chunks as u64);
        assert_eq!(
            result.recipe.file_hash(),
            MerkleHash::from(result.file_hash)
        );
        assert!(!staged.is_empty());
    }

    #[tokio::test]
    async fn duplicate_candidate_reuses_representative_without_segment_append() {
        let dir = tempfile::tempdir().unwrap();
        let representative_path = dir.path().join("representative.bin");
        let duplicate_path = dir.path().join("duplicate.bin");
        let data: Vec<u8> = (0..(2 * 1024 * 1024) as u32)
            .map(|i| (i.wrapping_mul(1_103_515_245).wrapping_add(12_345) >> 9) as u8)
            .collect();
        std::fs::write(&representative_path, &data).unwrap();
        std::fs::write(&duplicate_path, &data).unwrap();

        let staging_root = dir.path().join(".crab/staging");
        let staging = StagingArea::open(staging_root.clone()).await.unwrap();
        let progress = Arc::new(AddProgress::new(vec![
            AddFileProgressSpec {
                name: "representative.bin".to_owned(),
                total_bytes: data.len() as u64,
            },
            AddFileProgressSpec {
                name: "duplicate.bin".to_owned(),
                total_bytes: data.len() as u64,
            },
        ]));
        let representative_progress = progress.file_progress(0).unwrap();
        let duplicate_progress = progress.file_progress(1).unwrap();

        let representative = process_file(
            &representative_path,
            dir.path(),
            &staging,
            &progress,
            &representative_progress,
            None,
            None,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        let segment_len_after_representative =
            std::fs::metadata(staging_root.join("segments/current.seg"))
                .unwrap()
                .len();
        let reusable = ReusableStagedFile {
            file_hash: representative.file_hash,
            size: representative.size,
            recipe: representative.recipe.clone(),
        };

        let duplicate = process_duplicate_candidate(
            &duplicate_path,
            dir.path(),
            &staging,
            &progress,
            &duplicate_progress,
            Some(reusable),
            None,
            None,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        let segment_len_after_duplicate =
            std::fs::metadata(staging_root.join("segments/current.seg"))
                .unwrap()
                .len();
        assert_eq!(duplicate.file_hash, representative.file_hash);
        assert_eq!(duplicate.recipe, representative.recipe);
        assert_eq!(
            segment_len_after_duplicate, segment_len_after_representative,
            "verified duplicate reuse must not append another staged copy"
        );
        assert_eq!(
            duplicate_progress.bytes_done.load(Relaxed),
            data.len() as u64
        );
        assert_eq!(
            duplicate_progress.chunk_bytes_done.load(Relaxed),
            data.len() as u64
        );
        assert_eq!(
            duplicate_progress.chunks_done.load(Relaxed),
            representative.chunks as u64
        );
        assert_eq!(progress.files_done.load(Relaxed), 2);

        assert!(
            staging
                .has_verified_local_recipe(&representative.recipe)
                .unwrap()
        );
        assert!(
            staging
                .published_recipe_for_file(&MerkleHash::from(representative.file_hash))
                .unwrap()
                .is_none(),
            "duplicate staging must remain push-invisible until Git index publication"
        );
    }

    #[tokio::test]
    async fn duplicate_candidate_false_positive_falls_back_to_full_staging() {
        let dir = tempfile::tempdir().unwrap();
        let representative_path = dir.path().join("representative.bin");
        let candidate_path = dir.path().join("candidate.bin");
        let representative_data: Vec<u8> = (0..(2 * 1024 * 1024) as u32)
            .map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8)
            .collect();
        let candidate_data: Vec<u8> = (0..(2 * 1024 * 1024) as u32)
            .map(|i| (i.wrapping_mul(747_796_405).wrapping_add(2_891_336_453) >> 11) as u8)
            .collect();
        std::fs::write(&representative_path, &representative_data).unwrap();
        std::fs::write(&candidate_path, &candidate_data).unwrap();

        let staging_root = dir.path().join(".crab/staging");
        let staging = StagingArea::open(staging_root.clone()).await.unwrap();
        let progress = Arc::new(AddProgress::new(vec![
            AddFileProgressSpec {
                name: "representative.bin".to_owned(),
                total_bytes: representative_data.len() as u64,
            },
            AddFileProgressSpec {
                name: "candidate.bin".to_owned(),
                total_bytes: candidate_data.len() as u64,
            },
        ]));
        let representative_progress = progress.file_progress(0).unwrap();
        let candidate_progress = progress.file_progress(1).unwrap();

        let representative = process_file(
            &representative_path,
            dir.path(),
            &staging,
            &progress,
            &representative_progress,
            None,
            None,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        let segment_len_after_representative =
            std::fs::metadata(staging_root.join("segments/current.seg"))
                .unwrap()
                .len();
        let reusable = ReusableStagedFile {
            file_hash: representative.file_hash,
            size: representative.size,
            recipe: representative.recipe.clone(),
        };

        let candidate = process_duplicate_candidate(
            &candidate_path,
            dir.path(),
            &staging,
            &progress,
            &candidate_progress,
            Some(reusable),
            None,
            None,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        let segment_len_after_candidate =
            std::fs::metadata(staging_root.join("segments/current.seg"))
                .unwrap()
                .len();
        assert_ne!(candidate.file_hash, representative.file_hash);
        assert_eq!(
            candidate.file_hash,
            *blake3::hash(&candidate_data).as_bytes()
        );
        assert!(
            segment_len_after_candidate > segment_len_after_representative,
            "false-positive duplicate candidates must stage their own chunks"
        );
        assert_eq!(
            candidate_progress.bytes_done.load(Relaxed),
            candidate_data.len() as u64
        );
        assert_eq!(
            candidate_progress.chunk_bytes_done.load(Relaxed),
            candidate_data.len() as u64
        );
        assert_eq!(progress.files_done.load(Relaxed), 2);

        let representative_staged = staging
            .chunks_for_file(&MerkleHash::from(representative.file_hash))
            .unwrap();
        let candidate_staged = staging
            .chunks_for_file(&MerkleHash::from(candidate.file_hash))
            .unwrap();
        assert_eq!(representative_staged.len(), representative.chunks);
        assert_eq!(candidate_staged.len(), candidate.chunks);
        assert!(!candidate_staged.is_empty());
    }
}
