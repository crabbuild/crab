//! User-facing progress reporting for fetch and push operations.
//!
//! Output goes to stderr so it doesn't interfere with the remote helper's
//! stdout protocol channel. All methods are no-ops when progress is disabled.
//!
//! The push progress display mimics git-lfs style:
//! ```text
//! Enumerating objects: 42 files
//! Uploading data objects: 100% (4/4), 256 MiB | 48.2 MiB/s
//! Updating refs: done.
//! ```

use std::io::{Stderr, Stdout, Write};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{
    AtomicBool, AtomicU8, AtomicU64, Ordering::Acquire, Ordering::Relaxed, Ordering::Release,
};
use std::time::Duration;

use tokio::task::JoinHandle;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use xet_data::deduplication::DeduplicationMetrics;
use xet_data::progress_tracking::GroupProgress;
use xet_data::progress_tracking::upload_tracking::CompletionTracker;

use crate::core::output::event_payloads::ProgressPayload;
use crate::core::output::{JsonlStream, OutputMode};

// ---------------------------------------------------------------------------
// Byte formatting
// ---------------------------------------------------------------------------

/// Format a byte count as a human-readable string (e.g. "1.2 GiB").
pub fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;

    let b = bytes as f64;
    if b >= GIB {
        format!("{:.1} GiB", b / GIB)
    } else if b >= MIB {
        format!("{:.1} MiB", b / MIB)
    } else if b >= KIB {
        format!("{:.1} KiB", b / KIB)
    } else {
        format!("{bytes} B")
    }
}

/// Format a bytes-per-second rate as a human-readable string.
pub fn format_rate(bytes_per_sec: f64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;

    if bytes_per_sec >= GIB {
        format!("{:.1} GiB/s", bytes_per_sec / GIB)
    } else if bytes_per_sec >= MIB {
        format!("{:.1} MiB/s", bytes_per_sec / MIB)
    } else if bytes_per_sec >= KIB {
        format!("{:.1} KiB/s", bytes_per_sec / KIB)
    } else {
        format!("{:.0} B/s", bytes_per_sec)
    }
}

const DEFAULT_MIN_OBSERVATIONS_FOR_RATE: u32 = 4;

struct DecayingAverage {
    half_life_secs: f64,
    last_update: Instant,
    weight: f64,
    value: f64,
}

impl DecayingAverage {
    fn new(half_life: Duration) -> Self {
        Self {
            half_life_secs: half_life.as_secs_f64().max(f64::MIN_POSITIVE),
            last_update: Instant::now(),
            weight: 0.0,
            value: 0.0,
        }
    }

    fn update(&mut self, sample: f64, weight: f64) {
        if weight <= 0.0 || !weight.is_finite() || !sample.is_finite() {
            return;
        }

        let now = Instant::now();
        let elapsed = (now - self.last_update).as_secs_f64();
        let decay = (-elapsed / self.half_life_secs).exp2();

        self.weight = self.weight * decay + weight;
        self.value = self.value * decay + sample;
        self.last_update = now;
    }

    fn value(&self) -> f64 {
        if self.weight == 0.0 {
            0.0
        } else {
            self.value / self.weight
        }
    }
}

struct SpeedTracker {
    bytes_rate: DecayingAverage,
    transfer_rate: DecayingAverage,
    last_bytes_completed: u64,
    last_transfer_bytes_completed: u64,
    last_report_time: Instant,
    observation_count: u32,
    min_initial_interval_secs: f64,
    min_observations_for_rate: u32,
}

impl SpeedTracker {
    fn new(half_life: Duration) -> Self {
        Self {
            bytes_rate: DecayingAverage::new(half_life),
            transfer_rate: DecayingAverage::new(half_life),
            last_bytes_completed: 0,
            last_transfer_bytes_completed: 0,
            last_report_time: Instant::now(),
            observation_count: 0,
            min_initial_interval_secs: half_life.as_secs_f64(),
            min_observations_for_rate: DEFAULT_MIN_OBSERVATIONS_FOR_RATE,
        }
    }

    fn with_min_observations(mut self, n: u32) -> Self {
        self.min_observations_for_rate = n;
        self
    }

    fn update(&mut self, bytes_completed: u64, transfer_bytes_completed: u64) {
        let now = Instant::now();
        let mut elapsed = (now - self.last_report_time).as_secs_f64();

        if elapsed <= 0.0 {
            return;
        }

        if self.observation_count == 0 {
            elapsed = elapsed.max(self.min_initial_interval_secs);
        }

        let bytes_delta = bytes_completed.saturating_sub(self.last_bytes_completed);
        let transfer_delta =
            transfer_bytes_completed.saturating_sub(self.last_transfer_bytes_completed);

        self.bytes_rate.update(bytes_delta as f64, elapsed);
        self.transfer_rate.update(transfer_delta as f64, elapsed);

        self.last_bytes_completed = bytes_completed;
        self.last_transfer_bytes_completed = transfer_bytes_completed;
        self.last_report_time = now;
        self.observation_count = self.observation_count.saturating_add(1);
    }

    fn rates(&self) -> (Option<f64>, Option<f64>) {
        if self.observation_count >= self.min_observations_for_rate {
            (
                Some(self.bytes_rate.value()),
                Some(self.transfer_rate.value()),
            )
        } else {
            (None, None)
        }
    }
}

// ---------------------------------------------------------------------------
// Bar rendering
// ---------------------------------------------------------------------------

/// Render a progress bar string with colored filled/unfilled regions.
///
/// `fraction` is clamped to 0.0–1.0. `width` is the bar width in characters.
/// When `color` is true, uses ANSI escapes for green filled / gray unfilled.
/// When false, uses `#` and `-`.
pub fn render_bar(fraction: f64, width: usize, color: bool) -> String {
    let clamped = fraction.clamp(0.0, 1.0);
    let filled = (clamped * width as f64).round() as usize;
    let unfilled = width.saturating_sub(filled);

    if color {
        format!(
            "\x1b[32m{}\x1b[90m{}\x1b[0m",
            "█".repeat(filled),
            "░".repeat(unfilled),
        )
    } else {
        format!("{}{}", "#".repeat(filled), "-".repeat(unfilled))
    }
}

// ---------------------------------------------------------------------------
// TTY detection
// ---------------------------------------------------------------------------

/// Check if stderr is a terminal (for progress bar rendering).
///
/// When true, progress output can use `\r` carriage returns and ANSI
/// escape codes for in-place updates. When false, falls back to
/// line-based output with no ANSI codes.
pub fn is_tty() -> bool {
    #[cfg(unix)]
    {
        // SAFETY: `isatty` is a standard POSIX function that checks whether
        // file descriptor 2 (stderr) refers to a terminal. It has no
        // preconditions beyond a valid fd number.
        unsafe { libc::isatty(2) != 0 }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

// ---------------------------------------------------------------------------
// ProgressBackend — output routing for HelperProgress
// ---------------------------------------------------------------------------

/// Selects how progress events are rendered.
///
/// `HelperProgress::with_mode` picks the variant based on `OutputMode` and
/// TTY detection. Existing TTY / line behavior is preserved; JSONL mode
/// routes through `JsonlStream`; Silent mode suppresses all output (used
/// for `--json` where only the terminal result matters).
pub enum ProgressBackend {
    /// Interactive TTY: in-place updates with `\r` and ANSI escapes.
    Tty { stderr: Stderr },
    /// Non-TTY pipe: one line per update, no ANSI codes.
    Line { stderr: Stderr },
    /// JSONL streaming: progress events emitted as JSON lines to stdout.
    Jsonl {
        stream: Arc<Mutex<JsonlStream<Stdout>>>,
    },
    /// JSONL streaming to stderr. Used by the remote helper where git owns
    /// stdout — JSONL events MUST go to stderr in that context. Activated
    /// when `CRAB_PROGRESS_FORMAT=jsonl` is set.
    JsonlStderr {
        stream: Arc<Mutex<JsonlStream<Stderr>>>,
    },
    /// No output at all (`--json` mode — result only).
    Silent,
}

// ---------------------------------------------------------------------------
// HelperProgress (original simple reporter, kept for fetch)
// ---------------------------------------------------------------------------

/// Reports progress during fetch/push to a writer (typically stderr).
///
/// When the backend is `Silent`, all reporting methods are no-ops.
/// When the backend is `Jsonl`, progress is emitted as structured events
/// through the shared `JsonlStream`, bypassing TTY detection (SO7.2).
pub struct HelperProgress<W> {
    writer: W,
    enabled: bool,
    backend: ProgressBackend,
}

impl<W: Write> HelperProgress<W> {
    /// Create a new progress reporter with explicit writer and enabled flag.
    ///
    /// Uses `Line` backend by default (legacy behavior). Prefer
    /// [`HelperProgress::with_mode`] for new call sites that have an
    /// `OutputMode`.
    pub fn new(writer: W, enabled: bool) -> Self {
        Self {
            writer,
            enabled,
            backend: if enabled {
                ProgressBackend::Line {
                    stderr: std::io::stderr(),
                }
            } else {
                ProgressBackend::Silent
            },
        }
    }

    /// Whether progress reporting is active.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Report fetch progress, e.g. "Fetching objects: 3/5".
    ///
    /// # Errors
    ///
    /// Returns `std::io::Error` if the underlying write fails.
    pub fn report_fetch_progress(
        &mut self,
        packs_done: usize,
        packs_total: usize,
    ) -> std::io::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        match &self.backend {
            ProgressBackend::Tty { .. } | ProgressBackend::Line { .. } => {
                writeln!(self.writer, "Fetching objects: {packs_done}/{packs_total}")
            }
            ProgressBackend::Jsonl { stream } => {
                let payload = ProgressPayload {
                    operation: "fetching".to_owned(),
                    current: packs_done as u64,
                    total: packs_total as u64,
                    bytes: 0,
                    total_bytes: 0,
                    rate_bytes_per_sec: 0.0,
                    xorbs_produced: None,
                };
                if let Ok(mut s) = stream.lock() {
                    s.emit_progress(payload);
                }
                Ok(())
            }
            ProgressBackend::JsonlStderr { stream } => {
                let payload = ProgressPayload {
                    operation: "fetching".to_owned(),
                    current: packs_done as u64,
                    total: packs_total as u64,
                    bytes: 0,
                    total_bytes: 0,
                    rate_bytes_per_sec: 0.0,
                    xorbs_produced: None,
                };
                if let Ok(mut s) = stream.lock() {
                    s.emit_progress(payload);
                }
                Ok(())
            }
            ProgressBackend::Silent => Ok(()),
        }
    }

    /// Report push progress, e.g. "Pushing objects: 10/25".
    ///
    /// # Errors
    ///
    /// Returns `std::io::Error` if the underlying write fails.
    pub fn report_push_progress(
        &mut self,
        objects_done: usize,
        objects_total: usize,
    ) -> std::io::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        match &self.backend {
            ProgressBackend::Tty { .. } | ProgressBackend::Line { .. } => {
                writeln!(
                    self.writer,
                    "Pushing objects: {objects_done}/{objects_total}"
                )
            }
            ProgressBackend::Jsonl { stream } => {
                let payload = ProgressPayload {
                    operation: "pushing".to_owned(),
                    current: objects_done as u64,
                    total: objects_total as u64,
                    bytes: 0,
                    total_bytes: 0,
                    rate_bytes_per_sec: 0.0,
                    xorbs_produced: None,
                };
                if let Ok(mut s) = stream.lock() {
                    s.emit_progress(payload);
                }
                Ok(())
            }
            ProgressBackend::JsonlStderr { stream } => {
                let payload = ProgressPayload {
                    operation: "pushing".to_owned(),
                    current: objects_done as u64,
                    total: objects_total as u64,
                    bytes: 0,
                    total_bytes: 0,
                    rate_bytes_per_sec: 0.0,
                    xorbs_produced: None,
                };
                if let Ok(mut s) = stream.lock() {
                    s.emit_progress(payload);
                }
                Ok(())
            }
            ProgressBackend::Silent => Ok(()),
        }
    }

    /// Report a transfer percentage, e.g. "Uploading: 75%".
    ///
    /// # Errors
    ///
    /// Returns `std::io::Error` if the underlying write fails.
    pub fn report_transfer_percent(&mut self, label: &str, percent: u8) -> std::io::Result<()> {
        if !self.enabled {
            return Ok(());
        }
        match &self.backend {
            ProgressBackend::Tty { .. } | ProgressBackend::Line { .. } => {
                writeln!(self.writer, "{label}: {percent}%")
            }
            ProgressBackend::Jsonl { stream } => {
                let payload = ProgressPayload {
                    operation: label.to_owned(),
                    current: u64::from(percent),
                    total: 100,
                    bytes: 0,
                    total_bytes: 0,
                    rate_bytes_per_sec: 0.0,
                    xorbs_produced: None,
                };
                if let Ok(mut s) = stream.lock() {
                    s.emit_progress(payload);
                }
                Ok(())
            }
            ProgressBackend::JsonlStderr { stream } => {
                let payload = ProgressPayload {
                    operation: label.to_owned(),
                    current: u64::from(percent),
                    total: 100,
                    bytes: 0,
                    total_bytes: 0,
                    rate_bytes_per_sec: 0.0,
                    xorbs_produced: None,
                };
                if let Ok(mut s) = stream.lock() {
                    s.emit_progress(payload);
                }
                Ok(())
            }
            ProgressBackend::Silent => Ok(()),
        }
    }
}

/// Convenience constructor targeting stderr.
impl HelperProgress<std::io::Stderr> {
    /// Create a progress reporter that writes to stderr.
    pub fn stderr(enabled: bool) -> Self {
        Self::new(std::io::stderr(), enabled)
    }

    /// Create a progress reporter with output mode selection.
    ///
    /// - `Text` + TTY → `Tty` backend (in-place ANSI updates)
    /// - `Text` + no TTY → `Line` backend (one line per update)
    /// - `Jsonl` → `Jsonl` backend (always emits, bypasses TTY detection per SO7.2)
    /// - `Json` → `Silent` backend (result only, no progress)
    pub fn with_mode(mode: OutputMode, stream: Option<Arc<Mutex<JsonlStream<Stdout>>>>) -> Self {
        match mode {
            OutputMode::Text => {
                let tty = is_tty();
                Self {
                    writer: std::io::stderr(),
                    enabled: true,
                    backend: if tty {
                        ProgressBackend::Tty {
                            stderr: std::io::stderr(),
                        }
                    } else {
                        ProgressBackend::Line {
                            stderr: std::io::stderr(),
                        }
                    },
                }
            }
            OutputMode::Jsonl => Self {
                writer: std::io::stderr(),
                // Always enabled in JSONL mode — TTY detection bypassed (SO7.2).
                enabled: true,
                backend: match stream {
                    Some(s) => ProgressBackend::Jsonl { stream: s },
                    None => ProgressBackend::Silent,
                },
            },
            OutputMode::Json => Self {
                writer: std::io::stderr(),
                enabled: false,
                backend: ProgressBackend::Silent,
            },
        }
    }

    /// Create a progress reporter that emits JSONL to stderr.
    ///
    /// Used by the remote helper where git owns stdout. JSONL events go
    /// to stderr via `JsonlStream<Stderr>` when `CRAB_PROGRESS_FORMAT=jsonl`
    /// is set. Text mode falls back to the normal TTY/Line backend.
    pub fn with_mode_stderr(
        mode: OutputMode,
        stream: Option<Arc<Mutex<JsonlStream<Stderr>>>>,
    ) -> Self {
        match mode {
            OutputMode::Text => {
                let tty = is_tty();
                Self {
                    writer: std::io::stderr(),
                    enabled: true,
                    backend: if tty {
                        ProgressBackend::Tty {
                            stderr: std::io::stderr(),
                        }
                    } else {
                        ProgressBackend::Line {
                            stderr: std::io::stderr(),
                        }
                    },
                }
            }
            OutputMode::Jsonl => Self {
                writer: std::io::stderr(),
                enabled: true,
                backend: match stream {
                    Some(s) => ProgressBackend::JsonlStderr { stream: s },
                    None => ProgressBackend::Silent,
                },
            },
            OutputMode::Json => Self {
                writer: std::io::stderr(),
                enabled: false,
                backend: ProgressBackend::Silent,
            },
        }
    }
}

// ---------------------------------------------------------------------------
// NativePushProgress — progress tracker for the native push pipeline
// ---------------------------------------------------------------------------

/// Thread-safe progress tracker for the native push pipeline.
///
/// Shared via `Arc<NativePushProgress>` across all pipeline stages.
/// Uses `AtomicU64` counters with `Relaxed` ordering (no locks on the
/// hot path) and a background ticker that redraws live progress lines
/// every 100ms.
pub struct NativePushProgress {
    /// Whether progress output is enabled at all.
    enabled: bool,
    /// Whether to use ANSI color codes in output.
    color: bool,
    /// Whether to show per-file and per-xorb detail.
    verbose: bool,
    /// Output routing backend (TTY / Line / JSONL / Silent).
    backend: ProgressBackend,

    // Phase tracking
    phase: AtomicU8,

    // Packing counters
    pack_files_total: AtomicU64,
    pack_files_done: AtomicU64,
    pack_xorbs_produced: AtomicU64,
    pack_bytes_total: AtomicU64,
    pack_bytes_done: AtomicU64,

    // Upload counters
    upload_xorbs_total: AtomicU64,
    upload_xorbs_done: AtomicU64,
    upload_bytes_total: AtomicU64,
    upload_bytes_done: AtomicU64,
    upload_totals_final: AtomicBool,

    // Metadata counters
    meta_total: AtomicU64,
    meta_done: AtomicU64,

    /// EWMA-based smoothed transfer rate. Tracks `(bytes_completed,
    /// transfer_bytes_completed)` with a 3s half-life so the displayed
    /// MiB/s decays gracefully during stalls and isn't noisy on startup.
    speed_tracker: Mutex<SpeedTracker>,
    /// Shared group-progress backing the `CompletionTracker`. Lives here
    /// so the file-level completion aggregation survives across phases.
    group_progress: Arc<GroupProgress>,
    /// Tracks per-file completion across shared xorbs. Advances each
    /// file's `completed_bytes` as the xorbs it depends on are uploaded,
    /// even when multiple files share the same xorb.
    completion_tracker: Arc<CompletionTracker>,
}

impl NativePushProgress {
    /// Create a new progress tracker.
    ///
    /// When `enabled` is false, all reporting methods become no-ops.
    /// When `color` is false, ANSI escape codes are omitted.
    /// When `verbose` is true, per-file and per-xorb detail is shown.
    ///
    /// Uses TTY/Line backend based on TTY detection (legacy behavior).
    /// Prefer [`NativePushProgress::with_mode`] for new call sites.
    #[must_use]
    pub fn new(enabled: bool, color: bool, verbose: bool) -> Self {
        let group_progress = GroupProgress::new();
        let completion_tracker = Arc::new(CompletionTracker::new(Arc::clone(&group_progress)));
        let backend = if !enabled {
            ProgressBackend::Silent
        } else if is_tty() {
            ProgressBackend::Tty {
                stderr: std::io::stderr(),
            }
        } else {
            ProgressBackend::Line {
                stderr: std::io::stderr(),
            }
        };
        Self {
            enabled,
            color,
            verbose,
            backend,
            phase: AtomicU8::new(0),
            pack_files_total: AtomicU64::new(0),
            pack_files_done: AtomicU64::new(0),
            pack_xorbs_produced: AtomicU64::new(0),
            pack_bytes_total: AtomicU64::new(0),
            pack_bytes_done: AtomicU64::new(0),
            upload_xorbs_total: AtomicU64::new(0),
            upload_xorbs_done: AtomicU64::new(0),
            upload_bytes_total: AtomicU64::new(0),
            upload_bytes_done: AtomicU64::new(0),
            upload_totals_final: AtomicBool::new(false),
            meta_total: AtomicU64::new(0),
            meta_done: AtomicU64::new(0),
            speed_tracker: Mutex::new(
                SpeedTracker::new(Duration::from_secs(3)).with_min_observations(2),
            ),
            group_progress,
            completion_tracker,
        }
    }

    /// Create a progress tracker with output mode selection.
    ///
    /// - `Text` + TTY → `Tty` backend (in-place ANSI updates)
    /// - `Text` + no TTY → `Line` backend (one line per update)
    /// - `Jsonl` → `Jsonl` backend (always emits, bypasses TTY detection)
    /// - `Json` → `Silent` backend (result only, no progress)
    #[must_use]
    pub fn with_mode(
        color: bool,
        verbose: bool,
        mode: OutputMode,
        stream: Option<Arc<Mutex<JsonlStream<Stdout>>>>,
    ) -> Self {
        let group_progress = GroupProgress::new();
        let completion_tracker = Arc::new(CompletionTracker::new(Arc::clone(&group_progress)));
        let (enabled, backend) = match mode {
            OutputMode::Text => {
                let backend = if is_tty() {
                    ProgressBackend::Tty {
                        stderr: std::io::stderr(),
                    }
                } else {
                    ProgressBackend::Line {
                        stderr: std::io::stderr(),
                    }
                };
                (true, backend)
            }
            OutputMode::Jsonl => {
                let backend = match stream {
                    Some(s) => ProgressBackend::Jsonl { stream: s },
                    None => ProgressBackend::Silent,
                };
                // Always enabled in JSONL mode — TTY detection bypassed.
                (true, backend)
            }
            OutputMode::Json => (false, ProgressBackend::Silent),
        };
        Self {
            enabled,
            color,
            verbose,
            backend,
            phase: AtomicU8::new(0),
            pack_files_total: AtomicU64::new(0),
            pack_files_done: AtomicU64::new(0),
            pack_xorbs_produced: AtomicU64::new(0),
            pack_bytes_total: AtomicU64::new(0),
            pack_bytes_done: AtomicU64::new(0),
            upload_xorbs_total: AtomicU64::new(0),
            upload_xorbs_done: AtomicU64::new(0),
            upload_bytes_total: AtomicU64::new(0),
            upload_bytes_done: AtomicU64::new(0),
            upload_totals_final: AtomicBool::new(false),
            meta_total: AtomicU64::new(0),
            meta_done: AtomicU64::new(0),
            speed_tracker: Mutex::new(
                SpeedTracker::new(Duration::from_secs(3)).with_min_observations(2),
            ),
            group_progress,
            completion_tracker,
        }
    }

    /// Create a progress tracker that emits JSONL to stderr.
    ///
    /// Used by the remote helper where git owns stdout. JSONL events go
    /// to stderr via `JsonlStream<Stderr>` when `CRAB_PROGRESS_FORMAT=jsonl`
    /// is set.
    #[must_use]
    pub fn with_mode_stderr(
        color: bool,
        verbose: bool,
        mode: OutputMode,
        stream: Option<Arc<Mutex<JsonlStream<Stderr>>>>,
    ) -> Self {
        let group_progress = GroupProgress::new();
        let completion_tracker = Arc::new(CompletionTracker::new(Arc::clone(&group_progress)));
        let (enabled, backend) = match mode {
            OutputMode::Text => {
                let backend = if is_tty() {
                    ProgressBackend::Tty {
                        stderr: std::io::stderr(),
                    }
                } else {
                    ProgressBackend::Line {
                        stderr: std::io::stderr(),
                    }
                };
                (true, backend)
            }
            OutputMode::Jsonl => {
                let backend = match stream {
                    Some(s) => ProgressBackend::JsonlStderr { stream: s },
                    None => ProgressBackend::Silent,
                };
                (true, backend)
            }
            OutputMode::Json => (false, ProgressBackend::Silent),
        };
        Self {
            enabled,
            color,
            verbose,
            backend,
            phase: AtomicU8::new(0),
            pack_files_total: AtomicU64::new(0),
            pack_files_done: AtomicU64::new(0),
            pack_xorbs_produced: AtomicU64::new(0),
            pack_bytes_total: AtomicU64::new(0),
            pack_bytes_done: AtomicU64::new(0),
            upload_xorbs_total: AtomicU64::new(0),
            upload_xorbs_done: AtomicU64::new(0),
            upload_bytes_total: AtomicU64::new(0),
            upload_bytes_done: AtomicU64::new(0),
            upload_totals_final: AtomicBool::new(false),
            meta_total: AtomicU64::new(0),
            meta_done: AtomicU64::new(0),
            speed_tracker: Mutex::new(
                SpeedTracker::new(Duration::from_secs(3)).with_min_observations(2),
            ),
            group_progress,
            completion_tracker,
        }
    }

    /// Whether progress reporting is active.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Whether color output is enabled.
    pub fn use_color(&self) -> bool {
        self.color
    }

    /// Whether verbose mode is active.
    pub fn is_verbose(&self) -> bool {
        self.verbose
    }

    /// Read the current pipeline phase.
    pub fn phase(&self) -> u8 {
        self.phase.load(Relaxed)
    }

    /// Shared group-progress backing the `CompletionTracker`.
    ///
    /// Exposed so callers can register per-file [`ItemProgressUpdater`](
    /// xet_data::progress_tracking::ItemProgressUpdater) items against the
    /// same group used by the tracker.
    pub fn group_progress(&self) -> &Arc<GroupProgress> {
        &self.group_progress
    }

    /// Handle to the file-level completion tracker.
    ///
    /// Callers register files, xorbs, and dependencies against this tracker
    /// during the streaming pipeline; the ticker queries [`CompletionTracker::status`]
    /// to render accurate per-file upload progress.
    pub fn completion_tracker(&self) -> &Arc<CompletionTracker> {
        &self.completion_tracker
    }

    // -- Phase reporting methods --------------------------------------------

    /// Format a duration as a compact string (e.g. "1.2s").
    fn fmt_elapsed(elapsed: Duration) -> String {
        format!("{:.1}s", elapsed.as_secs_f64())
    }

    /// Return a green checkmark or plain "ok" depending on color setting.
    fn checkmark(&self) -> &'static str {
        if self.color {
            "\x1b[32m✓\x1b[0m"
        } else {
            "ok"
        }
    }

    /// Print completed discovery line to stderr.
    ///
    /// Format: `Discovering changes: {files} files in {commits} new commits  ✓ {elapsed}`
    pub fn report_discover(&self, files: u64, commits: u64, elapsed: Duration) {
        if !self.enabled {
            return;
        }
        match &self.backend {
            ProgressBackend::Tty { .. } | ProgressBackend::Line { .. } => {
                eprintln!(
                    "Discovering changes: {files} files in {commits} new commits  {} {}",
                    self.checkmark(),
                    Self::fmt_elapsed(elapsed),
                );
            }
            ProgressBackend::Jsonl { stream } => {
                let payload = ProgressPayload {
                    operation: "discovering".to_owned(),
                    current: files,
                    total: 0,
                    bytes: 0,
                    total_bytes: 0,
                    rate_bytes_per_sec: 0.0,
                    xorbs_produced: None,
                };
                if let Ok(mut s) = stream.lock() {
                    s.emit_progress(payload);
                }
            }
            ProgressBackend::JsonlStderr { stream } => {
                let payload = ProgressPayload {
                    operation: "discovering".to_owned(),
                    current: files,
                    total: 0,
                    bytes: 0,
                    total_bytes: 0,
                    rate_bytes_per_sec: 0.0,
                    xorbs_produced: None,
                };
                if let Ok(mut s) = stream.lock() {
                    s.emit_progress(payload);
                }
            }
            ProgressBackend::Silent => {}
        }
    }

    /// Print completed shard sync line to stderr.
    ///
    /// Format: `Syncing metadata: {shards} new shards (ChunkIndex: {entries} entries)  ✓ {elapsed}`
    pub fn report_shard_sync(&self, shards: u64, chunk_index_entries: u64, elapsed: Duration) {
        if !self.enabled {
            return;
        }
        match &self.backend {
            ProgressBackend::Tty { .. } | ProgressBackend::Line { .. } => {
                eprintln!(
                    "Syncing metadata:   {shards} new shards (ChunkIndex: {chunk_index_entries} entries)  {} {}",
                    self.checkmark(),
                    Self::fmt_elapsed(elapsed),
                );
            }
            ProgressBackend::Jsonl { stream } => {
                let payload = ProgressPayload {
                    operation: "syncing_metadata".to_owned(),
                    current: shards,
                    total: 0,
                    bytes: 0,
                    total_bytes: 0,
                    rate_bytes_per_sec: 0.0,
                    xorbs_produced: None,
                };
                if let Ok(mut s) = stream.lock() {
                    s.emit_progress(payload);
                }
            }
            ProgressBackend::JsonlStderr { stream } => {
                let payload = ProgressPayload {
                    operation: "syncing_metadata".to_owned(),
                    current: shards,
                    total: 0,
                    bytes: 0,
                    total_bytes: 0,
                    rate_bytes_per_sec: 0.0,
                    xorbs_produced: None,
                };
                if let Ok(mut s) = stream.lock() {
                    s.emit_progress(payload);
                }
            }
            ProgressBackend::Silent => {}
        }
    }

    /// Increment the packing files-done counter.
    pub fn inc_pack_file(&self) {
        self.pack_files_done.fetch_add(1, Relaxed);
    }

    /// Set the total packing counters (files and bytes).
    pub fn set_pack_totals(&self, files: u64, bytes: u64) {
        self.pack_files_total.store(files, Relaxed);
        self.pack_bytes_total.store(bytes, Relaxed);
    }

    /// Add bytes to the packing bytes-done counter.
    pub fn add_pack_bytes(&self, bytes: u64) {
        self.pack_bytes_done.fetch_add(bytes, Relaxed);
    }

    /// Read the current packing bytes-done counter.
    pub fn pack_bytes_done(&self) -> u64 {
        self.pack_bytes_done.load(Relaxed)
    }

    /// Set the packing xorbs-produced counter.
    pub fn set_pack_xorbs_produced(&self, count: u64) {
        self.pack_xorbs_produced.store(count, Relaxed);
    }

    /// Increment the upload xorbs-done counter and add bytes.
    ///
    /// Also feeds the cumulative upload total into the EWMA speed tracker
    /// so the displayed transfer rate decays smoothly during stalls. The
    /// `total` and `transfer` channels get the same value here — they only
    /// diverge once [`CompletionTracker`](xet_data::progress_tracking::upload_tracking::CompletionTracker)
    /// is wired in (task 9.2b).
    pub fn inc_upload_xorb(&self, bytes: u64) {
        self.upload_xorbs_done.fetch_add(1, Relaxed);
        self.add_upload_bytes(bytes);
    }

    /// Add bytes to the upload counter without incrementing the xorb count.
    ///
    /// Used by multipart uploads to report per-part progress so the
    /// byte-driven progress bar advances smoothly during a single large
    /// xorb upload, rather than jumping only when the whole xorb finishes.
    /// The xorb-completion counter is still bumped via
    /// [`inc_upload_xorb_bytes_already_reported`] once all parts complete.
    pub fn add_upload_bytes(&self, bytes: u64) {
        if bytes == 0 {
            return;
        }
        let total = self.upload_bytes_done.fetch_add(bytes, Relaxed) + bytes;
        if let Ok(mut tracker) = self.speed_tracker.lock() {
            tracker.update(total, total);
        }
    }

    /// Mark one xorb as complete without re-crediting its bytes.
    ///
    /// For multipart uploads that already fed per-part bytes through
    /// [`add_upload_bytes`]. Bumping the byte counter again here would
    /// double-count the xorb's size.
    pub fn inc_upload_xorb_bytes_already_reported(&self) {
        self.upload_xorbs_done.fetch_add(1, Relaxed);
    }

    /// Set the total upload counters (xorbs and bytes).
    pub fn set_upload_totals(&self, xorbs: u64, bytes: u64) {
        self.upload_xorbs_total.store(xorbs, Relaxed);
        self.upload_bytes_total.store(bytes, Relaxed);
    }

    /// Mark the streaming upload totals as complete and immutable.
    pub(crate) fn mark_upload_totals_final(&self) {
        self.upload_totals_final.store(true, Release);
    }

    /// Return whether the streaming producer has closed and totals are final.
    pub(crate) fn upload_totals_are_final(&self) -> bool {
        self.upload_totals_final.load(Acquire)
    }

    /// Return the cumulative xorb bytes uploaded so far.
    pub fn upload_bytes_done(&self) -> u64 {
        self.upload_bytes_done.load(Relaxed)
    }

    /// Return the cumulative xorbs uploaded so far.
    pub fn upload_xorbs_done(&self) -> u64 {
        self.upload_xorbs_done.load(Relaxed)
    }

    /// Increment the metadata-done counter.
    pub fn inc_meta(&self) {
        self.meta_done.fetch_add(1, Relaxed);
    }

    /// Set the total metadata counter.
    pub fn set_meta_total(&self, n: u64) {
        self.meta_total.store(n, Relaxed);
    }

    /// Print dedup feedback line to stderr in yellow.
    ///
    /// Shows dedup ratio, skipped bytes/chunks, and optionally defrag-prevented bytes.
    pub fn report_dedup(&self, metrics: &DeduplicationMetrics) {
        if !self.enabled {
            return;
        }
        match &self.backend {
            ProgressBackend::Tty { .. } | ProgressBackend::Line { .. } => {
                let ratio = if metrics.total_bytes > 0 {
                    (metrics.deduped_bytes * 100) / metrics.total_bytes
                } else {
                    0
                };
                let deduped_bytes_str = format_bytes(metrics.deduped_bytes);
                let deduped_chunks = metrics.deduped_chunks;
                if self.color {
                    eprintln!(
                        "                    \x1b[33m↳ {ratio}% dedup — skipped {deduped_bytes_str} of unchanged chunks ({deduped_chunks} chunks)\x1b[0m",
                    );
                } else {
                    eprintln!(
                        "                    ↳ {ratio}% dedup — skipped {deduped_bytes_str} of unchanged chunks ({deduped_chunks} chunks)",
                    );
                }
                if metrics.defrag_prevented_dedup_bytes > 0 {
                    let defrag_str = format_bytes(metrics.defrag_prevented_dedup_bytes);
                    if self.color {
                        eprintln!(
                            "                    \x1b[33m  ({defrag_str} kept for read locality)\x1b[0m",
                        );
                    } else {
                        eprintln!("                      ({defrag_str} kept for read locality)",);
                    }
                }
            }
            ProgressBackend::Jsonl { stream } => {
                let payload = ProgressPayload {
                    operation: "dedup".to_owned(),
                    current: metrics.deduped_bytes,
                    total: metrics.total_bytes,
                    bytes: metrics.deduped_bytes,
                    total_bytes: metrics.total_bytes,
                    rate_bytes_per_sec: 0.0,
                    xorbs_produced: None,
                };
                if let Ok(mut s) = stream.lock() {
                    s.emit_progress(payload);
                }
            }
            ProgressBackend::JsonlStderr { stream } => {
                let payload = ProgressPayload {
                    operation: "dedup".to_owned(),
                    current: metrics.deduped_bytes,
                    total: metrics.total_bytes,
                    bytes: metrics.deduped_bytes,
                    total_bytes: metrics.total_bytes,
                    rate_bytes_per_sec: 0.0,
                    xorbs_produced: None,
                };
                if let Ok(mut s) = stream.lock() {
                    s.emit_progress(payload);
                }
            }
            ProgressBackend::Silent => {}
        }
    }

    /// Finalize a phase line with a checkmark and elapsed time.
    ///
    /// Format: `{label}  ✓ {elapsed}`
    pub fn report_phase_done(&self, label: &str, elapsed: Duration) {
        if !self.enabled {
            return;
        }
        match &self.backend {
            ProgressBackend::Tty { .. } | ProgressBackend::Line { .. } => {
                eprintln!(
                    "{}  {} {}",
                    label,
                    self.checkmark(),
                    Self::fmt_elapsed(elapsed),
                );
            }
            ProgressBackend::Jsonl { stream } => {
                let payload = ProgressPayload {
                    operation: label.to_owned(),
                    current: 0,
                    total: 0,
                    bytes: 0,
                    total_bytes: 0,
                    rate_bytes_per_sec: 0.0,
                    xorbs_produced: None,
                };
                if let Ok(mut s) = stream.lock() {
                    s.emit_progress(payload);
                }
            }
            ProgressBackend::JsonlStderr { stream } => {
                let payload = ProgressPayload {
                    operation: label.to_owned(),
                    current: 0,
                    total: 0,
                    bytes: 0,
                    total_bytes: 0,
                    rate_bytes_per_sec: 0.0,
                    xorbs_produced: None,
                };
                if let Ok(mut s) = stream.lock() {
                    s.emit_progress(payload);
                }
            }
            ProgressBackend::Silent => {}
        }
    }

    /// Print a ref update line to stderr.
    ///
    /// Format: `Updating refs:      {ref_name} → {sha}`
    pub fn report_refs(&self, ref_name: &str, sha: &str) {
        if !self.enabled {
            return;
        }
        match &self.backend {
            ProgressBackend::Tty { .. } | ProgressBackend::Line { .. } => {
                if self.color {
                    eprintln!("\x1b[36mUpdating refs:\x1b[0m      {ref_name} → {sha}",);
                } else {
                    eprintln!("Updating refs:      {ref_name} -> {sha}");
                }
            }
            ProgressBackend::Jsonl { stream } => {
                let payload = ProgressPayload {
                    operation: "updating_refs".to_owned(),
                    current: 0,
                    total: 0,
                    bytes: 0,
                    total_bytes: 0,
                    rate_bytes_per_sec: 0.0,
                    xorbs_produced: None,
                };
                if let Ok(mut s) = stream.lock() {
                    s.emit_progress(payload);
                }
            }
            ProgressBackend::JsonlStderr { stream } => {
                let payload = ProgressPayload {
                    operation: "updating_refs".to_owned(),
                    current: 0,
                    total: 0,
                    bytes: 0,
                    total_bytes: 0,
                    rate_bytes_per_sec: 0.0,
                    xorbs_produced: None,
                };
                if let Ok(mut s) = stream.lock() {
                    s.emit_progress(payload);
                }
            }
            ProgressBackend::Silent => {}
        }
    }

    /// Print the final push summary to stdout.
    ///
    /// This is the only method that writes to stdout (not stderr),
    /// matching the design requirement that the summary goes to stdout.
    /// In JSONL mode, the terminal result is emitted by the caller
    /// through `JsonlStream::emit_result`, so this is a no-op.
    pub fn report_summary(
        &self,
        files: u64,
        bytes: u64,
        xorbs: u64,
        remote: &str,
        elapsed: Duration,
        dedup: Option<&DeduplicationMetrics>,
    ) {
        if !self.enabled {
            return;
        }
        match &self.backend {
            ProgressBackend::Tty { .. } | ProgressBackend::Line { .. } => {
                let bytes_str = format_bytes(bytes);
                let elapsed_str = Self::fmt_elapsed(elapsed);
                match dedup {
                    Some(dm) => {
                        let uploaded_str = format_bytes(dm.xorb_bytes_uploaded);
                        let deduped_str = format_bytes(dm.deduped_bytes);
                        let new_chunks = dm.new_chunks;
                        println!(
                            "\nTo {remote}\n   {files} files pushed ({bytes_str}, {xorbs} xorbs) in {elapsed_str}\n   Uploaded: {uploaded_str} ({new_chunks} new chunks), Deduplicated: {deduped_str}",
                        );
                    }
                    None => {
                        println!(
                            "\nTo {remote}\n   {files} files pushed ({bytes_str}, {xorbs} xorbs) in {elapsed_str}",
                        );
                    }
                }
            }
            // JSONL terminal result is emitted by the caller.
            ProgressBackend::Jsonl { .. }
            | ProgressBackend::JsonlStderr { .. }
            | ProgressBackend::Silent => {}
        }
    }

    // -- Phase constants ----------------------------------------------------

    /// Phase: packing files into xorbs.
    pub const PHASE_PACKING: u8 = 1;
    /// Phase: uploading xorbs to remote.
    pub const PHASE_UPLOADING: u8 = 2;
    /// Phase: both packing and uploading active (streaming pipeline).
    pub const PHASE_STREAMING: u8 = 3;
    /// Phase: uploading metadata (shards + file-index).
    pub const PHASE_METADATA: u8 = 4;

    /// Set the current pipeline phase.
    pub fn set_phase(&self, phase: u8) {
        self.phase.store(phase, Relaxed);
    }

    // -- Live progress rendering --------------------------------------------

    /// Render the packing progress line.
    fn render_pack_line(&self) -> String {
        let done = self.pack_files_done.load(Relaxed);
        let total = self.pack_files_total.load(Relaxed);
        let xorbs = self.pack_xorbs_produced.load(Relaxed);
        let bytes = self.pack_bytes_done.load(Relaxed);
        let total_bytes = self.pack_bytes_total.load(Relaxed);

        let fraction = if total_bytes > 0 {
            bytes as f64 / total_bytes as f64
        } else if total > 0 {
            done as f64 / total as f64
        } else {
            0.0
        };
        let pct = (fraction * 100.0).min(100.0) as u64;
        let bar = render_bar(fraction, 30, self.color);
        let bytes_str = format_bytes(bytes);
        let byte_progress = if total_bytes > 0 {
            format!("{bytes_str} / {}", format_bytes(total_bytes))
        } else {
            bytes_str
        };

        format!(
            "Packing:            {bar} {pct:>3}%  {done}/{total} files, {xorbs} xorbs ({byte_progress})"
        )
    }

    /// Render the upload progress line.
    fn render_upload_line(&self) -> String {
        let done = self.upload_xorbs_done.load(Relaxed);
        let total = self.upload_xorbs_total.load(Relaxed);

        // Prefer file-level completion when the tracker has been populated
        // (streaming pipeline). The tracker correctly attributes shared
        // xorbs across files, which raw counters can't express. Falls back
        // to the atomic upload bytes when the tracker is empty.
        let (tracker_done, tracker_total) = self.completion_tracker.status();
        let (bytes_done, bytes_total) = if tracker_total > 0 {
            (tracker_done, tracker_total)
        } else {
            (
                self.upload_bytes_done.load(Relaxed),
                self.upload_bytes_total.load(Relaxed),
            )
        };

        // Both counters must complete before the bar can complete: uploaded
        // bytes may land before the corresponding xorb future is reaped.
        let fraction = if bytes_total > 0 && total > 0 {
            (bytes_done as f64 / bytes_total as f64).min(done as f64 / total as f64)
        } else if bytes_total > 0 {
            bytes_done as f64 / bytes_total as f64
        } else if total > 0 {
            done as f64 / total as f64
        } else {
            0.0
        };
        let pct = (fraction * 100.0).min(100.0) as u64;
        let bar = render_bar(fraction, 30, self.color);
        let bytes_str = format_bytes(bytes_done);

        // Smoothed transfer rate (EWMA with 3s half-life). None until
        // enough observations; decays toward zero during stalls.
        let rate = self
            .speed_tracker
            .lock()
            .ok()
            .and_then(|t| t.rates().1)
            .unwrap_or(0.0);
        let rate_str = format_rate(rate);

        format!("Uploading xorbs:    {bar} {pct:>3}%  {done}/{total}, {bytes_str} | {rate_str}")
    }

    fn render_streaming_upload_line(&self) -> String {
        if self.upload_totals_are_final() {
            return self.render_upload_line();
        }

        let done = self.upload_xorbs_done.load(Relaxed);
        let packed_so_far = self.upload_xorbs_total.load(Relaxed);
        let bytes_str = format_bytes(self.upload_bytes_done.load(Relaxed));
        let rate = self
            .speed_tracker
            .lock()
            .ok()
            .and_then(|tracker| tracker.rates().1)
            .unwrap_or(0.0);
        let rate_str = format_rate(rate);

        format!(
            "Uploading xorbs:    streaming  {done} uploaded, {packed_so_far} packed so far, {bytes_str} | {rate_str}"
        )
    }

    /// Render the metadata upload progress line.
    fn render_meta_line(&self) -> String {
        let done = self.meta_done.load(Relaxed);
        let total = self.meta_total.load(Relaxed);

        let fraction = if total > 0 {
            done as f64 / total as f64
        } else {
            0.0
        };
        let pct = (fraction * 100.0).min(100.0) as u64;
        let bar = render_bar(fraction, 30, self.color);

        format!("Uploading metadata: {bar} {pct:>3}%  {done}/{total} entries")
    }

    /// Render live progress to stderr (TTY mode) or emit JSONL progress events.
    ///
    /// In TTY mode, reads atomic counters, formats the appropriate progress
    /// line(s), and writes to stderr with `\r\x1b[2K` for in-place updates.
    /// In JSONL mode, emits a structured progress event through the stream.
    fn render_live(&self, prev_lines: &mut usize) {
        match &self.backend {
            ProgressBackend::Jsonl { stream } => {
                self.emit_jsonl_progress(stream);
                *prev_lines = 0;
            }
            ProgressBackend::JsonlStderr { stream } => {
                self.emit_jsonl_progress_stderr(stream);
                *prev_lines = 0;
            }
            ProgressBackend::Tty { .. } => {
                self.render_live_tty(prev_lines);
            }
            ProgressBackend::Line { .. } | ProgressBackend::Silent => {
                *prev_lines = 0;
            }
        }
    }

    /// Emit a JSONL progress event based on the current phase counters.
    fn emit_jsonl_progress(&self, stream: &Arc<Mutex<JsonlStream<Stdout>>>) {
        let payload = self.build_jsonl_progress_payload();
        if let Some(payload) = payload {
            if let Ok(mut s) = stream.lock() {
                s.emit_progress(payload);
            }
        }
    }

    /// Emit a JSONL progress event to stderr (remote helper context).
    fn emit_jsonl_progress_stderr(&self, stream: &Arc<Mutex<JsonlStream<Stderr>>>) {
        let payload = self.build_jsonl_progress_payload();
        if let Some(payload) = payload {
            if let Ok(mut s) = stream.lock() {
                s.emit_progress(payload);
            }
        }
    }

    /// Build the JSONL progress payload from current phase counters.
    ///
    /// Returns `None` if the phase is unknown (no event to emit).
    fn build_jsonl_progress_payload(&self) -> Option<ProgressPayload> {
        let phase = self.phase.load(Relaxed);
        let payload = match phase {
            Self::PHASE_PACKING => {
                let done = self.pack_files_done.load(Relaxed);
                let total = self.pack_files_total.load(Relaxed);
                let bytes = self.pack_bytes_done.load(Relaxed);
                let total_bytes = self.pack_bytes_total.load(Relaxed);
                let xorbs = self.pack_xorbs_produced.load(Relaxed);
                ProgressPayload {
                    operation: "packing".to_owned(),
                    current: done,
                    total,
                    bytes,
                    total_bytes,
                    rate_bytes_per_sec: 0.0,
                    // Surface running xorb count during packing so
                    // consumers can report "X xorbs created" even when
                    // every chunk dedupes against the remote and the
                    // upload phase reports 0.
                    xorbs_produced: Some(xorbs),
                }
            }
            Self::PHASE_UPLOADING => {
                let done = self.upload_xorbs_done.load(Relaxed);
                let total = self.upload_xorbs_total.load(Relaxed);
                let bytes_done = self.upload_bytes_done.load(Relaxed);
                let bytes_total = self.upload_bytes_total.load(Relaxed);
                let xorbs = self.pack_xorbs_produced.load(Relaxed);
                let rate = self
                    .speed_tracker
                    .lock()
                    .ok()
                    .and_then(|t| t.rates().1)
                    .unwrap_or(0.0);
                ProgressPayload {
                    operation: "uploading".to_owned(),
                    current: done,
                    total,
                    bytes: bytes_done,
                    total_bytes: bytes_total,
                    rate_bytes_per_sec: rate,
                    // Carry the packing-phase xorb count forward so
                    // the renderer's summary doesn't drop to 0 when
                    // every xorb deduped to existing remote storage.
                    xorbs_produced: Some(xorbs),
                }
            }
            Self::PHASE_STREAMING => {
                // In streaming mode, report the upload progress (more useful
                // than packing for consumers tracking overall throughput).
                let done = self.upload_xorbs_done.load(Relaxed);
                let totals_final = self.upload_totals_are_final();
                let total = if totals_final {
                    self.upload_xorbs_total.load(Relaxed)
                } else {
                    0
                };
                let bytes_done = self.upload_bytes_done.load(Relaxed);
                let bytes_total = if totals_final {
                    self.upload_bytes_total.load(Relaxed)
                } else {
                    0
                };
                let xorbs = self.pack_xorbs_produced.load(Relaxed);
                let rate = self
                    .speed_tracker
                    .lock()
                    .ok()
                    .and_then(|t| t.rates().1)
                    .unwrap_or(0.0);
                ProgressPayload {
                    operation: "uploading".to_owned(),
                    current: done,
                    total,
                    bytes: bytes_done,
                    total_bytes: bytes_total,
                    rate_bytes_per_sec: rate,
                    xorbs_produced: Some(xorbs),
                }
            }
            Self::PHASE_METADATA => {
                let done = self.meta_done.load(Relaxed);
                let total = self.meta_total.load(Relaxed);
                ProgressPayload {
                    operation: "uploading_metadata".to_owned(),
                    current: done,
                    total,
                    bytes: 0,
                    total_bytes: 0,
                    rate_bytes_per_sec: 0.0,
                    xorbs_produced: None,
                }
            }
            _ => return None,
        };
        Some(payload)
    }

    /// Render live progress to stderr using in-place TTY updates.
    fn render_live_tty(&self, prev_lines: &mut usize) {
        let phase = self.phase.load(Relaxed);
        let lines = match phase {
            Self::PHASE_PACKING => vec![self.render_pack_line()],
            Self::PHASE_UPLOADING => vec![self.render_upload_line()],
            Self::PHASE_STREAMING => {
                vec![self.render_pack_line(), self.render_streaming_upload_line()]
            }
            Self::PHASE_METADATA => vec![self.render_meta_line()],
            _ => Vec::new(),
        };
        render_tty_frame(&mut std::io::stderr().lock(), &lines, prev_lines);
    }

    /// Emit a final newline after the live progress lines so subsequent
    /// output starts on a fresh line.
    fn finalize_live(&self, prev_lines: usize) {
        if prev_lines > 0 {
            eprintln!();
        }
    }

    /// Whether this backend can render periodic live updates.
    #[must_use]
    pub(crate) fn supports_live_ticker(&self) -> bool {
        if !self.enabled {
            return false;
        }

        match &self.backend {
            ProgressBackend::Tty { .. } => is_tty(),
            ProgressBackend::Jsonl { .. } | ProgressBackend::JsonlStderr { .. } => true,
            ProgressBackend::Line { .. } | ProgressBackend::Silent => false,
        }
    }

    /// Start the background ticker task for live progress updates.
    ///
    /// Returns `(JoinHandle, CancellationToken)`. Cancel the token to
    /// stop the ticker. The handle can be awaited for clean shutdown.
    ///
    /// In TTY mode, renders in-place progress bars every 100ms.
    /// In JSONL mode, emits structured progress events (rate-limited
    /// by `JsonlStream` to 250ms).
    /// In Line/Silent mode, returns `None` — no ticker is started.
    pub fn start_ticker(self: &Arc<Self>) -> Option<(JoinHandle<()>, CancellationToken)> {
        if !self.supports_live_ticker() {
            return None;
        }

        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        let progress = Arc::clone(self);

        let handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(100));
            let mut prev_lines = 0usize;

            loop {
                tokio::select! {
                    _ = cancel_clone.cancelled() => {
                        // Final render before exiting.
                        progress.render_live(&mut prev_lines);
                        progress.finalize_live(prev_lines);
                        break;
                    }
                    _ = interval.tick() => {
                        progress.render_live(&mut prev_lines);
                    }
                }
            }
        });

        Some((handle, cancel))
    }
}

fn render_tty_frame<W: Write>(writer: &mut W, lines: &[String], prev_lines: &mut usize) {
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

    if !lines.is_empty() && *prev_lines > lines.len() {
        for _ in 0..(*prev_lines - lines.len()) {
            let _ = write!(writer, "\x1b[A");
        }
    }

    *prev_lines = lines.len();
    let _ = writer.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- render_bar ----------------------------------------------------------

    #[test]
    fn render_bar_zero_percent_no_color() {
        let bar = render_bar(0.0, 30, false);
        assert_eq!(bar, "------------------------------");
    }

    #[test]
    fn render_bar_fifty_percent_no_color() {
        let bar = render_bar(0.5, 30, false);
        assert_eq!(bar, "###############---------------");
    }

    #[test]
    fn render_bar_hundred_percent_no_color() {
        let bar = render_bar(1.0, 30, false);
        assert_eq!(bar, "##############################");
    }

    #[test]
    fn render_bar_zero_percent_with_color() {
        let bar = render_bar(0.0, 30, true);
        assert_eq!(
            bar,
            format!("\x1b[32m{}\x1b[90m{}\x1b[0m", "", "░".repeat(30),)
        );
    }

    #[test]
    fn render_bar_hundred_percent_with_color() {
        let bar = render_bar(1.0, 30, true);
        assert_eq!(
            bar,
            format!("\x1b[32m{}\x1b[90m{}\x1b[0m", "█".repeat(30), "",)
        );
    }

    #[test]
    fn render_bar_clamps_above_one() {
        let bar = render_bar(1.5, 30, false);
        assert_eq!(bar, "##############################");
    }

    #[test]
    fn render_bar_clamps_below_zero() {
        let bar = render_bar(-0.5, 30, false);
        assert_eq!(bar, "------------------------------");
    }

    #[test]
    fn render_bar_zero_width() {
        assert_eq!(render_bar(0.5, 0, false), "");
        assert_eq!(render_bar(0.5, 0, true), "\x1b[32m\x1b[90m\x1b[0m");
    }

    // -- format_bytes --------------------------------------------------------

    #[test]
    fn format_bytes_zero() {
        assert_eq!(format_bytes(0), "0 B");
    }

    #[test]
    fn format_bytes_bytes() {
        assert_eq!(format_bytes(512), "512 B");
    }

    #[test]
    fn format_bytes_kib() {
        assert_eq!(format_bytes(2048), "2.0 KiB");
    }

    #[test]
    fn format_bytes_mib() {
        assert_eq!(format_bytes(10 * 1024 * 1024), "10.0 MiB");
    }

    #[test]
    fn format_bytes_gib() {
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }

    // -- format_rate ---------------------------------------------------------

    #[test]
    fn format_rate_bytes_per_sec() {
        assert_eq!(format_rate(500.0), "500 B/s");
    }

    #[test]
    fn format_rate_kib_per_sec() {
        assert_eq!(format_rate(2048.0), "2.0 KiB/s");
    }

    #[test]
    fn format_rate_mib_per_sec() {
        assert_eq!(format_rate(10.0 * 1024.0 * 1024.0), "10.0 MiB/s");
    }

    #[test]
    fn format_rate_gib_per_sec() {
        assert_eq!(format_rate(2.0 * 1024.0 * 1024.0 * 1024.0), "2.0 GiB/s");
    }

    // -- is_tty --------------------------------------------------------------

    #[test]
    fn is_tty_returns_bool() {
        // In test harness stderr is typically not a TTY.
        let result = is_tty();
        assert!(!result || result); // just verify it returns without panic
    }

    // -- NativePushProgress --------------------------------------------------

    #[test]
    fn native_push_progress_new_stores_flags() {
        let p = NativePushProgress::new(true, false, true);
        assert!(p.is_enabled());
        assert!(!p.use_color());
        assert!(p.is_verbose());
    }

    #[test]
    fn native_push_progress_counters_start_at_zero() {
        let p = NativePushProgress::new(true, true, false);
        assert_eq!(p.phase(), 0);
        assert_eq!(p.pack_files_total.load(Relaxed), 0);
        assert_eq!(p.pack_files_done.load(Relaxed), 0);
        assert_eq!(p.pack_xorbs_produced.load(Relaxed), 0);
        assert_eq!(p.pack_bytes_total.load(Relaxed), 0);
        assert_eq!(p.pack_bytes_done.load(Relaxed), 0);
        assert_eq!(p.upload_xorbs_total.load(Relaxed), 0);
        assert_eq!(p.upload_xorbs_done.load(Relaxed), 0);
        assert_eq!(p.upload_bytes_total.load(Relaxed), 0);
        assert_eq!(p.upload_bytes_done.load(Relaxed), 0);
        assert!(!p.upload_totals_final.load(Relaxed));
        assert_eq!(p.meta_total.load(Relaxed), 0);
        assert_eq!(p.meta_done.load(Relaxed), 0);
    }

    #[test]
    fn native_push_progress_disabled_by_default_when_not_enabled() {
        let p = NativePushProgress::new(false, false, false);
        assert!(!p.is_enabled());
        assert!(!p.use_color());
        assert!(!p.is_verbose());
    }

    // -- Phase constants and set_phase ---------------------------------------

    #[test]
    fn set_phase_updates_phase() {
        let p = NativePushProgress::new(true, false, false);
        assert_eq!(p.phase(), 0);
        p.set_phase(NativePushProgress::PHASE_PACKING);
        assert_eq!(p.phase(), NativePushProgress::PHASE_PACKING);
        p.set_phase(NativePushProgress::PHASE_STREAMING);
        assert_eq!(p.phase(), NativePushProgress::PHASE_STREAMING);
    }

    #[test]
    fn phase_constants_are_distinct() {
        let phases = [
            NativePushProgress::PHASE_PACKING,
            NativePushProgress::PHASE_UPLOADING,
            NativePushProgress::PHASE_STREAMING,
            NativePushProgress::PHASE_METADATA,
        ];
        for (i, a) in phases.iter().enumerate() {
            for (j, b) in phases.iter().enumerate() {
                if i != j {
                    assert_ne!(a, b);
                }
            }
        }
    }

    // -- render_pack_line ----------------------------------------------------

    #[test]
    fn render_pack_line_zero_progress_no_color() {
        let p = NativePushProgress::new(true, false, false);
        p.set_pack_totals(42, 0);
        let line = p.render_pack_line();
        assert!(line.starts_with("Packing:"));
        assert!(line.contains("0/42 files"));
        assert!(line.contains("  0%"));
    }

    #[test]
    fn render_pack_line_half_progress() {
        let p = NativePushProgress::new(true, false, false);
        p.set_pack_totals(10, 1024 * 1024);
        for _ in 0..5 {
            p.inc_pack_file();
        }
        p.pack_bytes_done.store(512 * 1024, Relaxed);
        p.pack_xorbs_produced.store(2, Relaxed);
        let line = p.render_pack_line();
        assert!(line.contains("5/10 files"));
        assert!(line.contains(" 50%"));
        assert!(line.contains("2 xorbs"));
    }

    #[test]
    fn render_pack_line_zero_total_shows_zero_percent() {
        let p = NativePushProgress::new(true, false, false);
        let line = p.render_pack_line();
        assert!(line.contains("  0%"));
        assert!(line.contains("0/0 files"));
    }

    #[test]
    fn render_pack_line_advances_by_bytes_with_one_active_file() {
        let p = NativePushProgress::new(true, false, false);
        p.set_pack_totals(1, 1024 * 1024);
        p.add_pack_bytes(512 * 1024);
        p.set_pack_xorbs_produced(8);

        let line = p.render_pack_line();

        assert!(line.contains(" 50%"));
        assert!(line.contains("0/1 files"));
        assert!(line.contains("8 xorbs"));
        assert!(line.contains("512.0 KiB / 1.0 MiB"));
    }

    // -- render_upload_line --------------------------------------------------

    #[test]
    fn render_upload_line_shows_rate() {
        let p = NativePushProgress::new(true, false, false);
        p.set_upload_totals(12, 1024 * 1024 * 1024);
        for _ in 0..4 {
            p.inc_upload_xorb(64 * 1024 * 1024);
        }
        let line = p.render_upload_line();
        assert!(line.starts_with("Uploading xorbs:"));
        assert!(line.contains("4/12"));
        assert!(line.contains("|")); // rate separator
    }

    #[test]
    fn render_upload_line_reserves_100_percent_for_all_xorbs_complete() {
        let p = NativePushProgress::new(true, false, false);
        p.set_upload_totals(2, 64 * 1024 * 1024);
        p.inc_upload_xorb(64 * 1024 * 1024);

        let incomplete = p.render_upload_line();
        assert!(!incomplete.contains("100%"));
        assert!(incomplete.contains("1/2"));

        p.inc_upload_xorb(0);
        let complete = p.render_upload_line();
        assert!(complete.contains("100%"));
        assert!(complete.contains("2/2"));
    }

    #[test]
    fn streaming_upload_waits_for_final_total_before_showing_percentage() {
        let p = NativePushProgress::new(true, false, false);
        p.set_upload_totals(17, 17 * 64 * 1024 * 1024);
        for _ in 0..16 {
            p.inc_upload_xorb(64 * 1024 * 1024);
        }

        let streaming = p.render_streaming_upload_line();
        assert!(!streaming.contains("94%"));
        assert!(streaming.contains("16 uploaded"));
        assert!(streaming.contains("17 packed so far"));

        p.mark_upload_totals_final();
        let final_total = p.render_streaming_upload_line();
        assert!(final_total.contains("94%"));
        assert!(final_total.contains("16/17"));
    }

    #[test]
    fn streaming_jsonl_omits_total_until_producer_closes() {
        let p = NativePushProgress::new(true, false, false);
        p.set_phase(NativePushProgress::PHASE_STREAMING);
        p.set_upload_totals(17, 17 * 64 * 1024 * 1024);
        p.inc_upload_xorb(64 * 1024 * 1024);

        let streaming = p.build_jsonl_progress_payload().expect("streaming payload");
        assert_eq!(streaming.current, 1);
        assert_eq!(streaming.total, 0);
        assert_eq!(streaming.total_bytes, 0);

        p.mark_upload_totals_final();
        let final_total = p.build_jsonl_progress_payload().expect("final payload");
        assert_eq!(final_total.total, 17);
        assert_eq!(final_total.total_bytes, 17 * 64 * 1024 * 1024);
    }

    // -- render_meta_line ----------------------------------------------------

    #[test]
    fn render_meta_line_complete() {
        let p = NativePushProgress::new(true, false, false);
        p.set_meta_total(5);
        for _ in 0..5 {
            p.inc_meta();
        }
        let line = p.render_meta_line();
        assert!(line.contains("100%"));
        assert!(line.contains("5/5 entries"));
    }

    #[test]
    fn tty_frame_rewrites_two_lines_without_walking_up_screen() {
        let mut output = Vec::new();
        let mut prev_lines = 0;
        render_tty_frame(
            &mut output,
            &["packing 0".to_owned(), "uploading 0".to_owned()],
            &mut prev_lines,
        );
        output.clear();

        render_tty_frame(
            &mut output,
            &["packing 1".to_owned(), "uploading 1".to_owned()],
            &mut prev_lines,
        );

        assert_eq!(output, b"\x1b[A\r\x1b[2Kpacking 1\n\r\x1b[2Kuploading 1");
        assert_eq!(prev_lines, 2);
    }

    #[test]
    fn tty_frame_rewrites_one_line_without_moving_vertically() {
        let mut output = Vec::new();
        let mut prev_lines = 0;
        render_tty_frame(&mut output, &["uploading 0".to_owned()], &mut prev_lines);
        output.clear();

        render_tty_frame(&mut output, &["uploading 1".to_owned()], &mut prev_lines);

        assert_eq!(output, b"\r\x1b[2Kuploading 1");
        assert_eq!(prev_lines, 1);
    }

    #[test]
    fn tty_frame_clears_rows_removed_by_phase_transition() {
        let mut output = Vec::new();
        let mut prev_lines = 0;
        render_tty_frame(
            &mut output,
            &["packing".to_owned(), "uploading".to_owned()],
            &mut prev_lines,
        );
        output.clear();

        render_tty_frame(&mut output, &["metadata".to_owned()], &mut prev_lines);

        assert_eq!(output, b"\x1b[A\r\x1b[2Kmetadata\n\r\x1b[2K\x1b[A");
        assert_eq!(prev_lines, 1);
    }

    #[test]
    fn tty_frame_empty_state_clears_all_previous_rows() {
        let mut output = Vec::new();
        let mut prev_lines = 0;
        render_tty_frame(
            &mut output,
            &["packing".to_owned(), "uploading".to_owned()],
            &mut prev_lines,
        );
        output.clear();

        render_tty_frame(&mut output, &[], &mut prev_lines);

        assert_eq!(output, b"\x1b[A\r\x1b[2K\n\r\x1b[2K");
        assert_eq!(prev_lines, 0);
    }

    // -- start_ticker --------------------------------------------------------

    #[tokio::test]
    async fn start_ticker_returns_none_when_disabled() {
        let p = Arc::new(NativePushProgress::new(false, false, false));
        assert!(p.start_ticker().is_none());
    }

    #[tokio::test]
    async fn start_ticker_respects_tty_detection() {
        // When stderr is a TTY, the ticker starts; when not, it returns None.
        // Both outcomes are valid depending on the test environment.
        let p = Arc::new(NativePushProgress::new(true, false, false));
        let result = p.start_ticker();
        if is_tty() {
            // TTY environment: ticker should start.
            let (handle, cancel) = result.expect("ticker should start on TTY");
            cancel.cancel();
            let _ = handle.await;
        } else {
            // Non-TTY environment (CI, piped): ticker should not start.
            assert!(result.is_none());
        }
    }

    // -- reset_rate_window ---------------------------------------------------

    #[test]
    fn inc_upload_xorb_feeds_speed_tracker() {
        // Sanity check that repeated upload notifications update the cumulative
        // byte counter and feed the EWMA tracker without panicking. Rates stay
        // None until `min_observations` is reached; the tracker clamps the
        // first elapsed to the half-life.
        let p = NativePushProgress::new(true, false, false);
        p.inc_upload_xorb(1024);
        p.inc_upload_xorb(2048);
        assert_eq!(p.upload_bytes_done.load(Relaxed), 3072);
        assert_eq!(p.upload_xorbs_done.load(Relaxed), 2);
    }

    // -- ProgressBackend / HelperProgress::with_mode -------------------------

    #[test]
    fn helper_progress_new_preserves_legacy_behavior() {
        let mut buf = Vec::new();
        let mut hp = HelperProgress::new(&mut buf, true);
        hp.report_fetch_progress(3, 10).unwrap();
        let output = String::from_utf8(buf).unwrap();
        assert!(output.contains("Fetching objects: 3/10"));
    }

    #[test]
    fn helper_progress_disabled_is_silent() {
        let mut buf = Vec::new();
        let mut hp = HelperProgress::new(&mut buf, false);
        hp.report_fetch_progress(1, 5).unwrap();
        hp.report_push_progress(2, 8).unwrap();
        hp.report_transfer_percent("Upload", 50).unwrap();
        assert!(buf.is_empty());
    }

    #[test]
    fn with_mode_json_selects_silent_backend() {
        let hp = HelperProgress::with_mode(OutputMode::Json, None);
        assert!(!hp.is_enabled());
        assert!(matches!(hp.backend, ProgressBackend::Silent));
    }

    #[test]
    fn with_mode_text_selects_tty_or_line() {
        let hp = HelperProgress::with_mode(OutputMode::Text, None);
        assert!(hp.is_enabled());
        // In CI (non-TTY) we get Line; in a terminal we get Tty.
        assert!(matches!(
            hp.backend,
            ProgressBackend::Tty { .. } | ProgressBackend::Line { .. }
        ));
    }

    #[test]
    fn with_mode_jsonl_with_stream_selects_jsonl_backend() {
        use crate::core::output::JsonlStream;
        use std::io;

        let stream = Arc::new(Mutex::new(JsonlStream::new(
            "test.event",
            "1.0",
            io::stdout(),
        )));
        let hp = HelperProgress::with_mode(OutputMode::Jsonl, Some(stream));
        assert!(hp.is_enabled());
        assert!(matches!(hp.backend, ProgressBackend::Jsonl { .. }));
    }

    #[test]
    fn with_mode_jsonl_without_stream_selects_silent() {
        let hp = HelperProgress::with_mode(OutputMode::Jsonl, None);
        assert!(hp.is_enabled());
        assert!(matches!(hp.backend, ProgressBackend::Silent));
    }

    #[test]
    fn silent_backend_report_methods_are_noop() {
        let mut hp = HelperProgress::with_mode(OutputMode::Json, None);
        // All methods should succeed without writing anything.
        assert!(hp.report_fetch_progress(1, 5).is_ok());
        assert!(hp.report_push_progress(2, 8).is_ok());
        assert!(hp.report_transfer_percent("Upload", 50).is_ok());
    }

    // -- NativePushProgress::with_mode ---------------------------------------

    #[test]
    fn native_with_mode_json_selects_silent() {
        let p = NativePushProgress::with_mode(false, false, OutputMode::Json, None);
        assert!(!p.is_enabled());
        assert!(matches!(p.backend, ProgressBackend::Silent));
    }

    #[test]
    fn native_with_mode_text_selects_tty_or_line() {
        let p = NativePushProgress::with_mode(false, false, OutputMode::Text, None);
        assert!(p.is_enabled());
        assert!(matches!(
            p.backend,
            ProgressBackend::Tty { .. } | ProgressBackend::Line { .. }
        ));
    }

    #[test]
    fn native_with_mode_jsonl_with_stream_selects_jsonl() {
        use crate::core::output::JsonlStream;
        use std::io;

        let stream = Arc::new(Mutex::new(JsonlStream::new(
            "push.event",
            "1.0",
            io::stdout(),
        )));
        let p = NativePushProgress::with_mode(false, false, OutputMode::Jsonl, Some(stream));
        assert!(p.is_enabled());
        assert!(matches!(p.backend, ProgressBackend::Jsonl { .. }));
    }

    #[test]
    fn native_with_mode_jsonl_without_stream_selects_silent() {
        let p = NativePushProgress::with_mode(false, false, OutputMode::Jsonl, None);
        assert!(p.is_enabled());
        assert!(matches!(p.backend, ProgressBackend::Silent));
    }

    #[test]
    fn native_new_initializes_backend_based_on_tty() {
        let p = NativePushProgress::new(true, false, false);
        // In CI (non-TTY) we get Tty or Line; when disabled we get Silent.
        assert!(matches!(
            p.backend,
            ProgressBackend::Tty { .. } | ProgressBackend::Line { .. }
        ));

        let p_disabled = NativePushProgress::new(false, false, false);
        assert!(matches!(p_disabled.backend, ProgressBackend::Silent));
    }
}
