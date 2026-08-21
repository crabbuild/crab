//! Segment-based staging area for chunks awaiting upload.
//!
//! Layout (v2):
//! ```text
//! {root}/
//! ├── segments/
//! │   ├── current.seg
//! │   ├── {id:016x}.seg …
//! ├── index.db            `SQLite` WAL mode
//! ├── lockfile
//! └── push-{uuid}.inflight
//! ```
//!
//! Chunks are appended to segment files and indexed in `SQLite`. The v1
//! per-chunk file layout is deleted — see design doc for rationale.
//!
//! # Concurrency
//!
//! Two lock layers protect the staging area:
//!
//! 1. **Advisory flock** on `lockfile` — process-level mutual exclusion.
//!    Writers acquire `LOCK_EX` (exclusive); readers acquire `LOCK_SH`
//!    (shared). Multiple readers can coexist, but a writer blocks all
//!    other opens until it drops the lock. Both paths retry with
//!    exponential backoff before giving up.
//!
//! 2. **In-process locks** — the index (`std::sync::Mutex<Index>`) and
//!    writer (`tokio::sync::Mutex<SegmentWriter>`) protect concurrent
//!    access within a single `StagingArea` instance.
//!
//! The crate-level `#![deny(clippy::await_holding_lock)]` enforces that
//! no `std::sync::Mutex` is held across `.await`. The index
//! (`std::sync::Mutex<Index>`) is always acquired in scoped blocks and
//! dropped before any suspension point. The writer
//! (`tokio::sync::Mutex<SegmentWriter>`) is designed for async and may
//! be held across `.await` (e.g. `spawn_blocking` for seal/fsync).
//! The `ReaderPool` bookkeeping `Mutex` is held only for synchronous
//! `HashMap` lookups, never across `.await`.

mod add_push_plan;
pub mod config;
pub mod error;
mod index;
pub mod metrics;
pub mod multipart_resume;
pub mod push_plan;
pub mod recipe;
mod recovery;
mod segment;
pub mod stats;
pub mod stream;

#[cfg(test)]
mod prop_compaction_preserves_reads;
#[cfg(test)]
mod prop_orphan_sweep_idempotence;
#[cfg(test)]
mod prop_torn_tail_recovery;

pub use config::StagingConfig;
pub use error::{Result, StagingError};
pub use metrics::StagingMetrics;
pub use stats::{
    CompactionStats, RetireStats, StagingCleanStats, StagingLifecycleHealth, StagingStats,
    StagingVerifyStats,
};

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use fs4::fs_std::FileExt as LockFileExt;
use tracing::{debug, warn};

use crab_xet::hash::{MerkleHash, compute_data_hash};
use crab_xet::xorb::parser::XorbParser;

use self::index::{
    FileChunkLocator, Index, PendingRow, PreparedChunkLocator, PreparedXorbPlacementWrite,
    PreparedXorbWrite, RecipeVerification,
};
use self::segment::{ChunkLocator, PreparedRecord, ReaderPool, SegmentWriter};

/// Default flush threshold in bytes (256 MiB). When pending bytes exceed
/// this, the current durability boundary is recorded or the segment is sealed.
const FLUSH_THRESHOLD: u64 = 256 * 1024 * 1024;

/// Maximum number of retry attempts when acquiring the exclusive flock.
const FLOCK_MAX_RETRIES: u32 = 5;

/// Base delay between flock retry attempts (doubles each iteration).
const FLOCK_BASE_DELAY: std::time::Duration = std::time::Duration::from_millis(50);

/// Cap on the per-retry sleep in the blocking acquisition path.
///
/// Without a cap, exponential backoff runs away quickly (50ms → 100ms
/// → … → 25.6s at the 10th retry). We cap the sleep at 500ms so a
/// waiter reacts within half a second once the holder releases, even
/// on a long queue.
const FLOCK_BLOCKING_MAX_SLEEP: std::time::Duration = std::time::Duration::from_millis(500);

/// Default wait budget for the blocking flock path used by the clean
/// filter and long-running push pipeline. Sized to cover a full clean
/// pass on a multi-GiB working-tree file at modest disk speeds without
/// giving up. If the holder is still grinding after this, something
/// is genuinely stuck and we'd rather surface an error than silently
/// queue forever.
const FLOCK_BLOCKING_DEFAULT_BUDGET: std::time::Duration = std::time::Duration::from_secs(120);

/// Chunk payloads read per staging verification batch.
const VERIFY_BATCH_CHUNKS: usize = 128;

/// Attempt-unique owner of staged path leases.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct StagingBatchId(String);

impl StagingBatchId {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn new_staging_batch_id() -> StagingBatchId {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static NEXT_BATCH: AtomicU64 = AtomicU64::new(0);
    let sequence = NEXT_BATCH.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"crab staging batch v1\0");
    hasher.update(&std::process::id().to_le_bytes());
    hasher.update(&nanos.to_le_bytes());
    hasher.update(&sequence.to_le_bytes());
    StagingBatchId(hasher.finalize().to_hex().to_string())
}

#[cfg(unix)]
fn staging_path_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
fn staging_path_bytes(path: &Path) -> Vec<u8> {
    path.to_string_lossy().as_bytes().to_vec()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PositionCollisionCheck {
    Required,
    AlreadyRetired,
}

/// Lock acquisition strategy passed into `StagingArea::open_with_lock`
/// and its read-only sibling. Keeps the behavior choice at the public
/// constructor boundary instead of sprinkling branches inside the
/// post-lock initialization code.
#[derive(Debug, Clone, Copy)]
enum LockAcquisition {
    /// Short, bounded retry (legacy behavior). Returns
    /// `StagingLocked` on contention after ~3 seconds.
    NonBlocking,
    /// Block up to the given budget waiting for the holder to
    /// release. Returns `StagingLocked` on budget exhaustion.
    Blocking(std::time::Duration),
}

/// Name of the lockfile inside the staging root.
const LOCKFILE_NAME: &str = "lockfile";

#[derive(Debug, Clone, Copy)]
enum StagingLockKind {
    Exclusive,
    Shared,
}

/// On-disk staging area for chunks awaiting upload.
///
/// All chunk writes go through append-only segment files indexed by
/// `SQLite`. A single `StagingArea` per process per staging root,
/// enforced by an advisory flock on `lockfile`.
pub struct StagingArea {
    root: PathBuf,
    index: Arc<Mutex<Index>>,
    writer: Arc<tokio::sync::Mutex<SegmentWriter>>,
    readers: Arc<ReaderPool>,
    cfg: StagingConfig,
    metrics: Option<Arc<dyn StagingMetrics>>,
    // Held for the lifetime of the struct to keep the advisory lock.
    _lock_file: File,
}

/// Shared push handle to a staging area.
///
/// Acquires a shared (`LOCK_SH`) advisory flock so multiple readers can
/// coexist. No segment writer is opened. Push may atomically write lifecycle
/// metadata (recipe snapshots and retirement) but never appends payload bytes.
pub struct StagingAreaReadOnly {
    root: PathBuf,
    index: Arc<Mutex<Index>>,
    readers: Arc<ReaderPool>,
    metrics: Option<Arc<dyn StagingMetrics>>,
    _lock_file: File,
}

/// Guard marker held while post-push cleanup retires staging rows.
///
/// Dropping the guard removes its marker file. If a process crashes,
/// later pushes prune inflight markers whose recorded PID is dead;
/// `staging clean` still removes all `push-*.inflight` files.
pub struct StagingRetirementGuard {
    path: PathBuf,
}

impl Drop for StagingRetirementGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Read the PID stored in the lockfile, if any.
fn read_lock_holder(root: &Path) -> Option<u32> {
    let lock_path = root.join(LOCKFILE_NAME);
    let mut file = File::open(&lock_path).ok()?;
    let mut buf = String::new();
    file.read_to_string(&mut buf).ok()?;
    buf.trim().parse().ok()
}

const RETIREMENT_MARKER_PREFIX: &str = "retire-";

fn inflight_marker_path(root: &Path, push_id: &str) -> PathBuf {
    root.join(format!("push-{push_id}.inflight"))
}

fn is_retirement_marker(id: &str) -> bool {
    id.starts_with(RETIREMENT_MARKER_PREFIX)
}

fn retirement_marker_id(push_id: &str) -> String {
    format!("{RETIREMENT_MARKER_PREFIX}{push_id}")
}

struct InflightMarker {
    id: String,
    path: PathBuf,
}

fn marker_payload(id: &str) -> String {
    format!("pid={}\nid={id}\n", std::process::id())
}

fn write_inflight_marker(root: &Path, id: &str) -> Result<()> {
    let path = inflight_marker_path(root, id);
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let tmp_path = root.join(format!(
        ".push-{id}.inflight.tmp-{}-{nonce}",
        std::process::id()
    ));

    let write_result = (|| -> Result<()> {
        let mut file = File::create(&tmp_path)?;
        file.write_all(marker_payload(id).as_bytes())?;
        file.sync_data()?;
        drop(file);
        std::fs::rename(&tmp_path, &path)?;
        if let Ok(dir) = File::open(root) {
            let _ = dir.sync_all();
        }
        Ok(())
    })();

    if write_result.is_err() {
        let _ = std::fs::remove_file(&tmp_path);
    }
    write_result?;
    Ok(())
}

fn marker_pid(marker: &InflightMarker) -> Option<u32> {
    let raw = std::fs::read_to_string(&marker.path).ok()?;
    let payload_id = raw.lines().find_map(|line| line.strip_prefix("id="))?;
    if payload_id != marker.id {
        return None;
    }
    raw.lines()
        .find_map(|line| line.strip_prefix("pid=")?.parse().ok())
}

fn list_inflight_markers(root: &Path) -> Result<Vec<InflightMarker>> {
    let mut markers = Vec::new();
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(markers),
        Err(e) => return Err(e.into()),
    };
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(rest) = name.strip_prefix("push-")
            && let Some(id) = rest.strip_suffix(".inflight")
        {
            markers.push(InflightMarker {
                id: id.to_string(),
                path: entry.path(),
            });
        }
    }
    Ok(markers)
}

fn list_inflight_ids(root: &Path) -> Result<Vec<String>> {
    Ok(list_inflight_markers(root)?
        .into_iter()
        .map(|marker| marker.id)
        .collect())
}

fn clear_push_inflight_marker(root: &Path, push_id: &str) -> Result<()> {
    match std::fs::remove_file(inflight_marker_path(root, push_id)) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

fn mark_push_inflight_marker(root: &Path, push_id: &str) -> Result<Vec<String>> {
    write_inflight_marker(root, push_id)?;

    let removed = match prune_stale_inflight_markers(root) {
        Ok(removed) => removed,
        Err(error) => {
            let _ = clear_push_inflight_marker(root, push_id);
            return Err(error);
        }
    };

    let retirement_active = list_inflight_ids(root)?
        .iter()
        .any(|id| is_retirement_marker(id));
    if retirement_active {
        clear_push_inflight_marker(root, push_id)?;
        return Err(StagingError::StagingLocked { holder_pid: None });
    }

    Ok(removed)
}

fn begin_retirement_marker(
    root: &Path,
    push_id: &str,
) -> Result<(Option<StagingRetirementGuard>, Vec<String>)> {
    let removed = prune_stale_inflight_markers(root)?;

    let marker_id = retirement_marker_id(push_id);
    let path = inflight_marker_path(root, &marker_id);
    write_inflight_marker(root, &marker_id)?;

    let has_other_inflight = list_inflight_ids(root)?
        .iter()
        .any(|id| id != push_id && !is_retirement_marker(id));

    if has_other_inflight {
        clear_push_inflight_marker(root, &marker_id)?;
        return Ok((None, removed));
    }

    Ok((Some(StagingRetirementGuard { path }), removed))
}

fn prune_stale_inflight_markers(root: &Path) -> Result<Vec<String>> {
    let mut removed = Vec::new();
    for marker in list_inflight_markers(root)? {
        let should_remove = marker_pid(&marker).is_none_or(|pid| !pid_is_alive(pid));
        if should_remove {
            clear_push_inflight_marker(root, &marker.id)?;
            if !is_retirement_marker(&marker.id) {
                removed.push(marker.id);
            }
        }
    }
    Ok(removed)
}

/// Best-effort lookup of the PID recorded in the staging lockfile.
///
/// Callers outside `crab-staging` use this to surface a
/// `StagingLocked { holder_pid }` error to the user without having to
/// own an open `StagingArea` — e.g. the push pipeline detects that it
/// opened staging read-only and found no data, and wants to tell the
/// user which process to resolve.
///
/// Returns `None` when the lockfile is missing, unreadable, or doesn't
/// contain a valid PID. The PID may also be stale (holder already
/// exited); the caller may use [`pid_is_alive`] to filter.
#[must_use]
pub fn read_lockfile_pid(staging_root: &Path) -> Option<u32> {
    read_lock_holder(staging_root)
}

/// Write the current process PID into the lockfile.
fn write_pid_to_lockfile(file: &mut File) {
    let pid = std::process::id();
    // Truncate and rewrite. Errors are non-fatal — the lock itself is
    // what matters, the PID is purely diagnostic.
    let _ = file.set_len(0);
    let _ = std::io::Seek::seek(file, std::io::SeekFrom::Start(0));
    let _ = file.write_all(pid.to_string().as_bytes());
    let _ = file.flush();
}

/// Check whether a PID is still alive.
#[cfg(unix)]
pub fn pid_is_alive(pid: u32) -> bool {
    // SAFETY: `kill(pid, 0)` checks process existence without sending a
    // signal. The pid is a u32 from the lockfile, cast to i32 for libc.
    #[expect(
        clippy::cast_possible_wrap,
        reason = "PID fits in i32 on all POSIX systems"
    )]
    let rc = unsafe { libc::kill(pid as i32, 0) };
    rc == 0
}

/// Best-effort PID liveness check for platforms without POSIX signals.
#[cfg(not(unix))]
pub fn pid_is_alive(_pid: u32) -> bool {
    true
}

/// Try a single non-blocking flock attempt.
///
/// Returns `Ok(())` on success, `Err(WouldBlock)` if held by another
/// process, or `Err(other)` on unexpected failure.
fn try_flock(file: &File, lock_type: StagingLockKind) -> std::result::Result<(), std::io::Error> {
    let acquired = match lock_type {
        StagingLockKind::Exclusive => LockFileExt::try_lock_exclusive(file),
        StagingLockKind::Shared => LockFileExt::try_lock_shared(file),
    }?;
    if acquired {
        Ok(())
    } else {
        Err(std::io::Error::from(std::io::ErrorKind::WouldBlock))
    }
}

/// Acquire an exclusive advisory flock with retry and exponential backoff.
///
/// Creates the lockfile if it doesn't exist. On success, writes the
/// current PID into the lockfile for diagnostics. Returns the open
/// `File` handle (must be kept alive to hold the lock) or
/// `StagingLocked` if another process still holds it after all retries.
fn acquire_flock_exclusive(root: &Path) -> Result<File> {
    let lock_path = root.join(LOCKFILE_NAME);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)?;

    let mut delay = FLOCK_BASE_DELAY;
    for attempt in 0..=FLOCK_MAX_RETRIES {
        match try_flock(&file, StagingLockKind::Exclusive) {
            Ok(()) => {
                write_pid_to_lockfile(&mut file);
                return Ok(file);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if attempt == FLOCK_MAX_RETRIES {
                    let holder_pid = read_lock_holder(root);
                    return Err(StagingError::StagingLocked { holder_pid });
                }
                debug!(
                    attempt = attempt + 1,
                    max = FLOCK_MAX_RETRIES,
                    delay_ms = delay.as_millis(),
                    "staging lock held, retrying"
                );
                std::thread::sleep(delay);
                delay *= 2;
            }
            Err(e) => return Err(StagingError::Io(e)),
        }
    }
    Err(StagingError::Internal(
        "staging exclusive lock acquisition exhausted unexpectedly".to_owned(),
    ))
}

/// Acquire an exclusive advisory flock, blocking up to `budget` for
/// another holder to release.
///
/// Unlike [`acquire_flock_exclusive`] — which gives up after a short
/// fixed retry budget and returns `StagingLocked` — this variant is
/// intended for workflows that tolerate queueing. It polls
/// non-blockingly (so we never get stuck in a system call that ignores
/// process cancellation) and sleeps between attempts with an
/// exponentially-growing delay capped at
/// [`FLOCK_BLOCKING_MAX_SLEEP`]. When the elapsed time exceeds
/// `budget`, returns [`StagingError::StagingLocked`] with the holder's
/// PID if available.
///
/// The polling + capped-sleep design keeps wake-up latency under half
/// a second once the previous holder releases, which matters for
/// interactive commands that end up in the filter process chain
/// (`git status`, `git add`, IDE integrations).
fn acquire_flock_exclusive_blocking(root: &Path, budget: std::time::Duration) -> Result<File> {
    let lock_path = root.join(LOCKFILE_NAME);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)?;

    let start = std::time::Instant::now();
    let mut delay = FLOCK_BASE_DELAY;
    let mut stale_recovery_attempted = false;
    loop {
        match try_flock(&file, StagingLockKind::Exclusive) {
            Ok(()) => {
                write_pid_to_lockfile(&mut file);
                return Ok(file);
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // If the recorded holder PID is provably dead, recover
                // only when the kernel flock has also become free. The
                // PID text can be stale while a live shared reader still
                // owns the lockfile inode, so the flock is authoritative.
                if !stale_recovery_attempted {
                    stale_recovery_attempted = true;
                    if let Some(recovered) = force_break_stale_lock(root)? {
                        drop(file);
                        warn!("recovered released stale-PID staging lock during blocking open");
                        return Ok(recovered);
                    }
                }
                if start.elapsed() >= budget {
                    let holder_pid = read_lock_holder(root);
                    warn!(
                        ?holder_pid,
                        waited_secs = start.elapsed().as_secs_f64(),
                        budget_secs = budget.as_secs_f64(),
                        "staging lock acquisition timed out; holder is still grinding"
                    );
                    return Err(StagingError::StagingLocked { holder_pid });
                }
                debug!(
                    waited_ms = start.elapsed().as_millis(),
                    budget_ms = budget.as_millis(),
                    delay_ms = delay.as_millis(),
                    "staging lock held, blocking for holder to release"
                );
                std::thread::sleep(delay);
                delay = std::cmp::min(delay * 2, FLOCK_BLOCKING_MAX_SLEEP);
            }
            Err(e) => return Err(StagingError::Io(e)),
        }
    }
}

/// Acquire a shared advisory flock (for read-only access).
///
/// Shared locks coexist with other shared locks but block on exclusive
/// locks. Uses the same retry+backoff strategy as the exclusive path.
///
/// Opens the lockfile read-only when possible. Falls back to read-write
/// if the file doesn't exist yet (creates it), but this is rare — the
/// writer normally creates it first.
fn acquire_flock_shared(root: &Path) -> Result<File> {
    let lock_path = root.join(LOCKFILE_NAME);

    // Try read-only first (avoids needing write permission).
    let file = match std::fs::OpenOptions::new().read(true).open(&lock_path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Lockfile doesn't exist yet — create it. This is the only
            // case where the shared path needs write access.
            std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(&lock_path)?
        }
        Err(e) => return Err(StagingError::Io(e)),
    };

    let mut delay = FLOCK_BASE_DELAY;
    for attempt in 0..=FLOCK_MAX_RETRIES {
        match try_flock(&file, StagingLockKind::Shared) {
            Ok(()) => return Ok(file),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if attempt == FLOCK_MAX_RETRIES {
                    let holder_pid = read_lock_holder(root);
                    return Err(StagingError::StagingLocked { holder_pid });
                }
                debug!(
                    attempt = attempt + 1,
                    max = FLOCK_MAX_RETRIES,
                    delay_ms = delay.as_millis(),
                    "staging lock held (shared attempt), retrying"
                );
                std::thread::sleep(delay);
                delay *= 2;
            }
            Err(e) => return Err(StagingError::Io(e)),
        }
    }
    Err(StagingError::Internal(
        "staging shared lock acquisition exhausted unexpectedly".to_owned(),
    ))
}

/// Acquire a shared advisory flock, blocking up to `budget`.
///
/// Shared-lock siblings to [`acquire_flock_exclusive_blocking`]. Lets
/// push pipelines and other read-only callers queue behind an
/// in-progress clean-filter session that legitimately holds
/// `LOCK_EX`, instead of bouncing off after a 3-second retry budget.
fn acquire_flock_shared_blocking(root: &Path, budget: std::time::Duration) -> Result<File> {
    let lock_path = root.join(LOCKFILE_NAME);

    let mut file = match std::fs::OpenOptions::new().read(true).open(&lock_path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)?,
        Err(e) => return Err(StagingError::Io(e)),
    };

    let start = std::time::Instant::now();
    let mut delay = FLOCK_BASE_DELAY;
    let mut stale_recovery_attempted = false;
    loop {
        match try_flock(&file, StagingLockKind::Shared) {
            Ok(()) => return Ok(file),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                // Self-heal dead-holder lock (see acquire_flock_exclusive_blocking).
                // A stale PID is only recoverable once the flock is free.
                // Shared callers then reopen normally and acquire `LOCK_SH`.
                if !stale_recovery_attempted {
                    stale_recovery_attempted = true;
                    if stale_pid_lock_is_free(root)? {
                        warn!("recovered released stale-PID staging lock during shared open");
                        file = std::fs::OpenOptions::new()
                            .create(true)
                            .truncate(false)
                            .read(true)
                            .write(true)
                            .open(&lock_path)?;
                        continue;
                    }
                }
                if start.elapsed() >= budget {
                    let holder_pid = read_lock_holder(root);
                    warn!(
                        ?holder_pid,
                        waited_secs = start.elapsed().as_secs_f64(),
                        budget_secs = budget.as_secs_f64(),
                        "staging shared-lock acquisition timed out"
                    );
                    return Err(StagingError::StagingLocked { holder_pid });
                }
                debug!(
                    waited_ms = start.elapsed().as_millis(),
                    budget_ms = budget.as_millis(),
                    delay_ms = delay.as_millis(),
                    "staging lock held, blocking shared acquisition"
                );
                std::thread::sleep(delay);
                delay = std::cmp::min(delay * 2, FLOCK_BLOCKING_MAX_SLEEP);
            }
            Err(e) => return Err(StagingError::Io(e)),
        }
    }
}

/// Return `true` when the lockfile records a dead PID and the flock is free.
///
/// Used by the blocking shared path, which needs to retry with `LOCK_SH`
/// instead of keeping the temporary exclusive probe. A stale PID alone is
/// not enough: read-only push handles may hold a live shared flock without
/// rewriting the diagnostic PID.
fn stale_pid_lock_is_free(root: &Path) -> Result<bool> {
    let Some(pid) = read_lock_holder(root) else {
        return Ok(false);
    };
    if pid_is_alive(pid) {
        debug!(pid, "staging lock holder alive, not breaking");
        return Ok(false);
    }

    match try_acquire_existing_exclusive(root)? {
        Some(file) => {
            drop(file);
            warn!(
                dead_pid = pid,
                "staging lock was free despite stale PID; retrying acquisition"
            );
            Ok(true)
        }
        None => {
            warn!(
                dead_pid = pid,
                "staging lock PID is stale but the flock is still held; refusing to unlink live lock"
            );
            Ok(false)
        }
    }
}

fn try_acquire_existing_exclusive(root: &Path) -> Result<Option<File>> {
    let lock_path = root.join(LOCKFILE_NAME);
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)?;

    match try_flock(&file, StagingLockKind::Exclusive) {
        Ok(()) => {
            write_pid_to_lockfile(&mut file);
            Ok(Some(file))
        }
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
        Err(e) => Err(StagingError::Io(e)),
    }
}

/// Force-break a stale lock by checking if the recorded PID is alive.
///
/// If the lockfile contains a PID that is no longer running, we only
/// proceed when the flock itself is already free. Returns `None` if the
/// holder is still alive, no PID is recorded, or the PID is stale but a
/// live process still holds the flock.
///
/// This is intentionally conservative: it only breaks the lock when
/// both the diagnostic PID and the kernel flock agree that there is no
/// live holder.
///
/// # TOCTOU note
///
/// There is a theoretical race between `pid_is_alive` and PID reuse. The
/// kernel flock is authoritative: even if the PID text is stale, we never
/// unlink the lockfile while another file description still holds it.
fn force_break_stale_lock(root: &Path) -> Result<Option<File>> {
    let holder = read_lock_holder(root);
    match holder {
        Some(pid) if !pid_is_alive(pid) => {
            warn!(
                dead_pid = pid,
                "staging lockfile records a dead PID; checking flock before force-open"
            );
            match try_acquire_existing_exclusive(root)? {
                Some(file) => Ok(Some(file)),
                None => {
                    warn!(
                        dead_pid = pid,
                        "staging lock PID is stale but the flock is still held; refusing to unlink live lock"
                    );
                    Ok(None)
                }
            }
        }
        Some(pid) => {
            debug!(
                pid,
                "staging lock holder is still alive, cannot force-break"
            );
            Ok(None)
        }
        None => {
            // No PID in lockfile. This can happen if:
            // 1. The holder acquired the lock but crashed before writing PID.
            // 2. The lockfile was created by an older version without PID support.
            // In either case, check if the lockfile is old enough to be stale.
            let lock_path = root.join(LOCKFILE_NAME);
            let is_stale = std::fs::metadata(&lock_path)
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > std::time::Duration::from_secs(3600));

            if is_stale {
                warn!("staging lockfile has no PID and is stale; checking flock before force-open");
                match try_acquire_existing_exclusive(root)? {
                    Some(file) => Ok(Some(file)),
                    None => {
                        warn!(
                            "staging lockfile has no PID but the flock is still held; refusing to unlink live lock"
                        );
                        Ok(None)
                    }
                }
            } else {
                debug!("no PID in lockfile and file is recent, cannot determine if lock is stale");
                Ok(None)
            }
        }
    }
}

/// Lock the index `Mutex`, converting a poisoned lock into `StagingError`.
fn lock_index(index: &Mutex<Index>) -> Result<std::sync::MutexGuard<'_, Index>> {
    index
        .lock()
        .map_err(|e| StagingError::Internal(format!("index lock poisoned: {e}")))
}

fn authoritative_file_push_plan(
    root: &Path,
    index: &Mutex<Index>,
    file_hash: &MerkleHash,
) -> Result<Option<push_plan::FilePushPlan>> {
    let fh: [u8; 32] = (*file_hash).into();
    let Some(stored) = lock_index(index)?.file_push_plan(&fh)? else {
        return Ok(None);
    };

    let plan = push_plan::deserialize_file_push_plan(&stored.plan_json)?;
    if stored.version != plan.version {
        return Err(StagingError::StagingCorrupt(format!(
            "stored push plan version {} disagrees with plan body version {} for file {}",
            stored.version,
            plan.version,
            file_hash.hex()
        )));
    }
    if stored.file_size != plan.file_size {
        return Err(StagingError::StagingCorrupt(format!(
            "stored push plan size {} disagrees with plan body size {} for file {}",
            stored.file_size,
            plan.file_size,
            file_hash.hex()
        )));
    }
    let plan_hash = plan.file_hash()?;
    if plan_hash != *file_hash {
        return Err(StagingError::StagingCorrupt(format!(
            "stored push plan for file {} contains body for file {}",
            file_hash.hex(),
            plan_hash.hex()
        )));
    }
    let chunks = plan.chunk_pairs()?;
    let chunk_count = u64::try_from(chunks.len()).map_err(|_| {
        StagingError::StagingCorrupt(format!(
            "push plan for file {} has too many chunks",
            file_hash.hex()
        ))
    })?;
    if stored.chunk_count != chunk_count {
        return Err(StagingError::StagingCorrupt(format!(
            "stored push plan chunk count {} disagrees with plan body count {} for file {}",
            stored.chunk_count,
            chunk_count,
            file_hash.hex()
        )));
    }
    let sequence_hash = push_plan::chunk_sequence_hash(&chunks);
    if stored.chunk_sequence_hash != sequence_hash {
        return Err(StagingError::StagingCorrupt(format!(
            "stored push plan chunk sequence hash disagrees with plan body for file {}",
            file_hash.hex()
        )));
    }
    validate_file_push_plan_matches_staging(root, index, &plan)?;
    prepared_xorb_index_records(&plan, &chunks)?;

    Ok(Some(plan))
}

fn validate_file_push_plan_matches_staging(
    root: &Path,
    index: &Mutex<Index>,
    plan: &push_plan::FilePushPlan,
) -> Result<(MerkleHash, Vec<(MerkleHash, u64)>)> {
    if !plan.staged_chunk_sequence_verified {
        return Err(StagingError::StagingCorrupt(format!(
            "add-time push plan for file {} was not verified against staging",
            plan.file_hash
        )));
    }
    let file_hash = plan.file_hash()?;
    let chunks = plan.chunk_pairs()?;
    let staged_chunks = {
        let fh: [u8; 32] = file_hash.into();
        lock_index(index)?.chunks_for_file_with_sizes(&fh)?
    };
    let segment_sequence_matches = staged_chunks.len() == chunks.len()
        && staged_chunks.iter().zip(chunks.iter()).all(
            |((hash, size), (expected_hash, expected_size))| {
                MerkleHash::from(*hash) == *expected_hash && *size == *expected_size
            },
        );
    if !staged_chunks.is_empty() && !segment_sequence_matches {
        return Err(StagingError::StagingCorrupt(format!(
            "add-time push plan for file {} no longer matches staged chunk rows",
            file_hash.hex()
        )));
    }
    if staged_chunks.is_empty()
        && !prepared_xorbs_cover_chunk_sequence(root, &file_hash, &chunks, &plan.prepared_xorbs)
    {
        return Err(StagingError::StagingCorrupt(format!(
            "add-time push plan for file {} has neither matching segment rows nor complete prepared-xorb coverage",
            file_hash.hex()
        )));
    }
    let staged_size = chunks.iter().try_fold(0u64, |acc, (_, size)| {
        acc.checked_add(*size).ok_or_else(|| {
            StagingError::StagingCorrupt(format!(
                "add-time push plan size overflow for file {}",
                file_hash.hex()
            ))
        })
    })?;
    if staged_size != plan.file_size {
        return Err(StagingError::StagingCorrupt(format!(
            "add-time push plan for file {} totals {staged_size} bytes, expected {}",
            file_hash.hex(),
            plan.file_size
        )));
    }

    Ok((file_hash, chunks))
}

fn prepared_xorbs_cover_chunk_sequence(
    root: &Path,
    file_hash: &MerkleHash,
    chunks: &[(MerkleHash, u64)],
    prepared_xorbs: &[push_plan::PlannedXorb],
) -> bool {
    if chunks.is_empty() {
        return prepared_xorbs.is_empty();
    }
    let expected = chunks.iter().copied().collect::<HashMap<_, _>>();
    let mut covered = HashSet::with_capacity(expected.len());
    for planned in prepared_xorbs {
        let Ok(xorb_hash) = planned.hash() else {
            return false;
        };
        let path = push_plan::prepared_xorb_path(root, file_hash, &xorb_hash);
        if !push_plan::prepared_xorb_file_matches_cached_plan(&path, file_hash, &xorb_hash, planned)
        {
            return false;
        }
        for placement in &planned.placements {
            let Ok(placement) = placement.to_placement() else {
                return false;
            };
            if placement.xorb_hash != xorb_hash
                || expected.get(&placement.chunk_hash).copied()
                    != Some(u64::from(placement.uncompressed_size))
            {
                return false;
            }
            covered.insert(placement.chunk_hash);
        }
    }
    expected.keys().all(|hash| covered.contains(hash))
}

fn prepared_xorb_index_records(
    plan: &push_plan::FilePushPlan,
    chunks: &[(MerkleHash, u64)],
) -> Result<Vec<PreparedXorbWrite>> {
    let chunk_sizes = chunks.iter().copied().collect::<HashMap<MerkleHash, u64>>();
    let mut records = Vec::with_capacity(plan.prepared_xorbs.len());
    for planned in &plan.prepared_xorbs {
        let xorb_hash = planned.hash()?;
        let xorb_hash_bytes: [u8; 32] = xorb_hash.into();
        let mut covers_file = false;
        let placements = planned
            .placements
            .iter()
            .map(|placement| {
                let placement = placement.to_placement()?;
                if placement.xorb_hash != xorb_hash {
                    return Err(StagingError::StagingCorrupt(format!(
                        "prepared xorb {} contains placement for {}",
                        xorb_hash.hex(),
                        placement.xorb_hash.hex()
                    )));
                }
                if let Some(expected_size) = chunk_sizes.get(&placement.chunk_hash) {
                    covers_file = true;
                    if *expected_size != u64::from(placement.uncompressed_size) {
                        return Err(StagingError::StagingCorrupt(format!(
                            "prepared xorb {} placement for chunk {} has size {}, expected {}",
                            xorb_hash.hex(),
                            placement.chunk_hash.hex(),
                            placement.uncompressed_size,
                            expected_size
                        )));
                    }
                }
                Ok(PreparedXorbPlacementWrite {
                    chunk_hash: placement.chunk_hash.into(),
                    chunk_index: placement.chunk_index,
                    uncompressed_size: placement.uncompressed_size,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        if !covers_file {
            return Err(StagingError::StagingCorrupt(format!(
                "prepared xorb {} does not cover file {}",
                xorb_hash.hex(),
                plan.file_hash
            )));
        }
        records.push(PreparedXorbWrite {
            xorb_hash: xorb_hash_bytes,
            payload_hash: planned.payload_hash_bytes()?,
            bytes: planned.bytes,
            planned_json: push_plan::serialize_planned_xorb(planned)?,
            placements,
        });
    }
    Ok(records)
}

fn indexed_prepared_xorb_cache_for_chunks(
    root: &Path,
    index: &Mutex<Index>,
    wanted_chunks: &HashSet<MerkleHash>,
) -> Result<push_plan::PreparedXorbCache> {
    if wanted_chunks.is_empty() {
        return Ok(push_plan::PreparedXorbCache::default());
    }
    let wanted: Vec<[u8; 32]> = wanted_chunks.iter().map(|chunk| (*chunk).into()).collect();
    let stored = lock_index(index)?.prepared_xorbs_for_chunks(&wanted)?;
    let mut cache = push_plan::PreparedXorbCache::default();
    let mut authoritative_sources: HashMap<MerkleHash, Option<HashMap<MerkleHash, Vec<u8>>>> =
        HashMap::new();

    for stored_xorb in stored {
        let source_file_hash = MerkleHash::from(stored_xorb.file_hash);
        let stored_xorb_hash = MerkleHash::from(stored_xorb.xorb_hash);
        let source_xorbs = authoritative_sources
            .entry(source_file_hash)
            .or_insert_with(|| {
                authoritative_prepared_xorbs_by_hash(root, index, &source_file_hash)
            });
        let Some(source_xorbs) = source_xorbs.as_ref() else {
            continue;
        };
        let Some(source_planned_json) = source_xorbs.get(&stored_xorb_hash) else {
            continue;
        };
        if source_planned_json.as_slice() != stored_xorb.planned_json.as_slice() {
            continue;
        }

        let Ok(planned) = push_plan::deserialize_planned_xorb(&stored_xorb.planned_json) else {
            continue;
        };
        let Ok(planned_xorb_hash) = planned.hash() else {
            continue;
        };
        if planned_xorb_hash != stored_xorb_hash || planned.bytes != stored_xorb.bytes {
            continue;
        }
        let Ok(payload_hash) = planned.payload_hash_bytes() else {
            continue;
        };
        if payload_hash != stored_xorb.payload_hash {
            continue;
        }
        let path = push_plan::prepared_xorb_path(root, &source_file_hash, &planned_xorb_hash);
        if !push_plan::prepared_xorb_file_matches_cached_plan(
            &path,
            &source_file_hash,
            &planned_xorb_hash,
            &planned,
        ) {
            continue;
        }
        if cache
            .insert_prepared_xorb(source_file_hash, &planned)
            .is_err()
        {
            continue;
        }
    }

    Ok(cache)
}

fn authoritative_prepared_xorbs_by_hash(
    root: &Path,
    index: &Mutex<Index>,
    file_hash: &MerkleHash,
) -> Option<HashMap<MerkleHash, Vec<u8>>> {
    let plan = authoritative_file_push_plan(root, index, file_hash).ok()??;
    let mut prepared_xorbs = HashMap::with_capacity(plan.prepared_xorbs.len());
    for planned in &plan.prepared_xorbs {
        let Ok(xorb_hash) = planned.hash() else {
            continue;
        };
        let Ok(planned_json) = push_plan::serialize_planned_xorb(planned) else {
            continue;
        };
        prepared_xorbs.insert(xorb_hash, planned_json);
    }
    Some(prepared_xorbs)
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct StagedChunkLocator {
    pub hash: MerkleHash,
    pub size: u64,
    pub locator: ChunkLocator,
}

struct IndexedPreparedXorbStatsRef {
    payload_hash: [u8; 32],
    bytes: u64,
    planned_json: Vec<u8>,
}

fn hash_blob_for_stats(blob: &[u8]) -> Option<[u8; 32]> {
    <[u8; 32]>::try_from(blob).ok()
}

impl From<FileChunkLocator> for StagedChunkLocator {
    fn from(chunk: FileChunkLocator) -> Self {
        Self {
            hash: MerkleHash::from(chunk.chunk_hash),
            size: chunk.size,
            locator: chunk.locator,
        }
    }
}

async fn read_staged_chunks_batch(
    root: &Path,
    index: &Mutex<Index>,
    readers: &ReaderPool,
    metrics: Option<&dyn StagingMetrics>,
    hashes: &[MerkleHash],
) -> Result<Vec<(MerkleHash, Bytes)>> {
    if hashes.is_empty() {
        return Ok(Vec::new());
    }

    let hash_bytes: Vec<[u8; 32]> = hashes.iter().map(|h| <[u8; 32]>::from(*h)).collect();

    let (locators, prepared_locators) = {
        let idx = lock_index(index)?;
        (
            idx.locate_batch(&hash_bytes)?,
            idx.locate_prepared_batch(&hash_bytes)?,
        )
    };

    for (i, (locator, prepared)) in locators.iter().zip(&prepared_locators).enumerate() {
        if locator.is_none() && prepared.is_none() {
            return Err(StagingError::ChunkNotFound {
                hash: hashes[i].hex(),
            });
        }
    }

    let mut segment_chunks = Vec::new();
    let mut segment_positions = Vec::new();
    let mut prepared_chunks = Vec::new();
    for (position, ((hash, locator), prepared)) in hashes
        .iter()
        .zip(locators)
        .zip(prepared_locators)
        .enumerate()
    {
        if let Some(locator) = locator {
            segment_positions.push(position);
            segment_chunks.push(StagedChunkLocator {
                hash: *hash,
                size: u64::from(locator.length),
                locator,
            });
        } else if let Some(locator) = prepared {
            prepared_chunks.push((position, *hash, locator));
        }
    }

    let mut out = vec![None; hashes.len()];
    for (position, (_, data)) in segment_positions
        .into_iter()
        .zip(read_located_staged_chunks_batch(readers, metrics, &segment_chunks).await?)
    {
        out[position] = Some(data);
    }
    for (position, data) in
        read_prepared_staged_chunks_batch(root, metrics, &prepared_chunks).await?
    {
        out[position] = Some(data);
    }

    hashes
        .iter()
        .zip(out)
        .map(|(hash, data)| {
            data.map(|data| (*hash, data)).ok_or_else(|| {
                StagingError::Internal("staging batch read slot unfilled".to_owned())
            })
        })
        .collect()
}

type PreparedXorbIdentity = ([u8; 32], [u8; 32], [u8; 32], u64);

async fn read_prepared_staged_chunks_batch(
    root: &Path,
    metrics: Option<&dyn StagingMetrics>,
    chunks: &[(usize, MerkleHash, PreparedChunkLocator)],
) -> Result<Vec<(usize, Bytes)>> {
    let mut by_xorb = HashMap::<PreparedXorbIdentity, Vec<(usize, MerkleHash, u32, u32)>>::new();
    for (position, hash, locator) in chunks {
        by_xorb
            .entry((
                locator.file_hash,
                locator.xorb_hash,
                locator.payload_hash,
                locator.xorb_bytes,
            ))
            .or_default()
            .push((*position, *hash, locator.chunk_index, locator.size));
    }

    let mut tasks = Vec::with_capacity(by_xorb.len());
    for ((file_hash, xorb_hash, payload_hash, expected_bytes), wanted) in by_xorb {
        let path = push_plan::prepared_xorb_path(
            root,
            &MerkleHash::from(file_hash),
            &MerkleHash::from(xorb_hash),
        );
        tasks.push(tokio::task::spawn_blocking(move || {
            let payload = Bytes::from(std::fs::read(&path)?);
            let actual_payload_hash = blake3::hash(&payload);
            let size_matches = u64::try_from(payload.len()) == Ok(expected_bytes);
            let hash_matches = *actual_payload_hash.as_bytes() == payload_hash;
            if !size_matches || !hash_matches {
                return Err(StagingError::StagingCorrupt(format!(
                    "prepared xorb {} payload changed: expected {} bytes/{}, found {} bytes/{} (size_matches={size_matches}, hash_matches={hash_matches})",
                    MerkleHash::from(xorb_hash).hex(),
                    expected_bytes,
                    MerkleHash::from(payload_hash).hex(),
                    payload.len(),
                    actual_payload_hash.to_hex()
                )));
            }
            let parser = XorbParser::parse(payload)?;
            if parser.hash() != MerkleHash::from(xorb_hash) {
                return Err(StagingError::StagingCorrupt(format!(
                    "prepared xorb {} metadata hash changed",
                    MerkleHash::from(xorb_hash).hex()
                )));
            }
            parser.verify_payload_digest()?;

            let mut decoded = Vec::with_capacity(wanted.len());
            for (position, expected_hash, chunk_index, expected_size) in wanted {
                let chunk = parser.get_chunk(chunk_index)?;
                if chunk.hash != expected_hash || chunk.data.len() != expected_size as usize {
                    return Err(StagingError::StagingCorrupt(format!(
                        "prepared xorb chunk {} does not match its indexed placement",
                        expected_hash.hex()
                    )));
                }
                decoded.push((position, chunk.data));
            }
            Ok::<_, StagingError>((expected_bytes, decoded))
        }));
    }

    let mut out = Vec::with_capacity(chunks.len());
    let mut bytes_read = 0u64;
    for task in tasks {
        let (read, mut decoded) = task.await.map_err(|error| {
            StagingError::Internal(format!("prepared xorb read join: {error}"))
        })??;
        bytes_read = bytes_read.saturating_add(read);
        out.append(&mut decoded);
    }
    if let Some(metrics) = metrics {
        metrics.add_staging_bytes_read(bytes_read);
    }
    Ok(out)
}

async fn verify_unverified_recipe_payload(
    root: &Path,
    index: &Mutex<Index>,
    readers: &ReaderPool,
    recipe: &crate::recipe::FileRecipe,
) -> Result<()> {
    let mut hasher = blake3::Hasher::new();
    let mut bytes_checked = 0u64;
    for batch in recipe.sequence().spans.chunks(VERIFY_BATCH_CHUNKS) {
        let hashes = batch.iter().map(|span| span.chunk_hash).collect::<Vec<_>>();
        let payloads = read_staged_chunks_batch(root, index, readers, None, &hashes).await?;
        for (span, (actual_hash, data)) in batch.iter().zip(payloads) {
            if actual_hash != span.chunk_hash {
                return Err(StagingError::StagingCorrupt(
                    "unverified recipe returned a different chunk hash".to_owned(),
                ));
            }
            let size = u64::try_from(data.len()).map_err(|_| {
                StagingError::StagingCorrupt("migrated chunk size cannot be represented".to_owned())
            })?;
            if size != span.len {
                return Err(StagingError::StagingCorrupt(format!(
                    "unverified chunk {} has {size} bytes, recipe requires {}",
                    span.chunk_hash.hex(),
                    span.len
                )));
            }
            bytes_checked = bytes_checked.checked_add(size).ok_or_else(|| {
                StagingError::StagingCorrupt("migrated recipe size overflow".to_owned())
            })?;
            hasher.update(&data);
        }
    }
    if bytes_checked != recipe.sequence().file_size {
        return Err(StagingError::StagingCorrupt(format!(
            "unverified recipe covers {bytes_checked} bytes, expected {}",
            recipe.sequence().file_size
        )));
    }
    let actual = MerkleHash::from(*hasher.finalize().as_bytes());
    if actual != recipe.sequence().file_hash {
        return Err(StagingError::StagingCorrupt(format!(
            "unverified recipe {} reconstructs to {}",
            recipe.sequence().file_hash.hex(),
            actual.hex()
        )));
    }
    Ok(())
}

fn recipe_payload_error_is_corruption(error: &StagingError) -> bool {
    match error {
        StagingError::StagingCorrupt(_)
        | StagingError::ChunkNotFound { .. }
        | StagingError::NotFound { .. }
        | StagingError::HashMismatch { .. }
        | StagingError::CrcMismatch { .. }
        | StagingError::Xet(_) => true,
        StagingError::Io(error) => error.kind() == std::io::ErrorKind::NotFound,
        StagingError::Configuration { .. }
        | StagingError::StagingLocked { .. }
        | StagingError::Cancelled
        | StagingError::FileChangedDuringStaging { .. }
        | StagingError::Internal(_) => false,
    }
}

async fn complete_recipe_payload_validation(
    root: &Path,
    index: &Mutex<Index>,
    readers: &ReaderPool,
) -> Result<()> {
    let pending = lock_index(index)?.migration_validation_pending()?;
    if !pending {
        return Ok(());
    }
    let recipe_hashes = lock_index(index)?.unverified_recipe_hashes()?;
    for recipe_hash in recipe_hashes {
        let recipe = match lock_index(index)?.unverified_recipe(&recipe_hash) {
            Ok(recipe) => recipe,
            Err(error) if recipe_payload_error_is_corruption(&error) => {
                lock_index(index)?
                    .quarantine_unverified_recipe(&recipe_hash, &error.to_string())?;
                warn!(
                    recipe_hash = %MerkleHash::from(recipe_hash).hex(),
                    error = %error,
                    "quarantined structurally corrupt unverified recipe without deleting payload bytes"
                );
                continue;
            }
            Err(error) => return Err(error),
        };
        if let Err(error) = verify_unverified_recipe_payload(root, index, readers, &recipe).await {
            if !recipe_payload_error_is_corruption(&error) {
                return Err(error);
            }
            lock_index(index)?.quarantine_unverified_recipe(&recipe_hash, &error.to_string())?;
            warn!(
                recipe_hash = %MerkleHash::from(recipe_hash).hex(),
                file_hash = %recipe.sequence().file_hash.hex(),
                error = %error,
                "quarantined corrupt unverified recipe without deleting payload bytes"
            );
            continue;
        }
        lock_index(index)?.mark_recipe_verified(&recipe_hash)?;
    }
    lock_index(index)?.finish_migration_validation()
}

async fn read_located_staged_chunks_batch(
    readers: &ReaderPool,
    metrics: Option<&dyn StagingMetrics>,
    chunks: &[StagedChunkLocator],
) -> Result<Vec<(MerkleHash, Bytes)>> {
    if chunks.is_empty() {
        return Ok(Vec::new());
    }

    let mut by_segment: HashMap<u64, Vec<(usize, u64, u32)>> = HashMap::new();
    for (i, chunk) in chunks.iter().enumerate() {
        let loc = chunk.locator;
        by_segment
            .entry(loc.segment_id)
            .or_default()
            .push((i, loc.offset, loc.length));
    }
    for group in by_segment.values_mut() {
        group.sort_by_key(|&(_, offset, _)| offset);
    }

    let mut out: Vec<Option<Bytes>> = (0..chunks.len()).map(|_| None).collect();
    let mut tasks = Vec::with_capacity(by_segment.len());
    for (segment_id, group) in by_segment {
        let reader = readers.get(segment_id)?;
        tasks.push(tokio::task::spawn_blocking(move || {
            reader.read_many_sorted(&group)
        }));
    }

    let mut total_bytes_read: u64 = 0;
    for task in tasks {
        let results = task
            .await
            .map_err(|e| StagingError::Internal(format!("pread batch join: {e}")))??;
        for (i, data) in results {
            total_bytes_read += data.len() as u64;
            out[i] = Some(data);
        }
    }

    if let Some(m) = metrics {
        m.add_staging_bytes_read(total_bytes_read);
    }

    let mut result = Vec::with_capacity(chunks.len());
    for (i, slot) in out.into_iter().enumerate() {
        let data = slot.ok_or_else(|| StagingError::Internal("batch read slot unfilled".into()))?;
        let expected = chunks[i];
        if data.len() as u64 != expected.size {
            return Err(StagingError::StagingCorrupt(format!(
                "chunk {} size mismatch while reading staging: locator has {} bytes, segment has {} bytes",
                expected.hash.hex(),
                expected.size,
                data.len()
            )));
        }
        let actual_hash = compute_data_hash(&data);
        if actual_hash != expected.hash {
            return Err(StagingError::HashMismatch {
                requested: expected.hash.hex(),
                actual: actual_hash.hex(),
            });
        }
        result.push((expected.hash, data));
    }

    Ok(result)
}

fn staging_chunk_index(chunk_index_offset: u64, batch_index: usize) -> Result<i64> {
    let batch_index = u64::try_from(batch_index).map_err(|_| {
        StagingError::StagingCorrupt(format!(
            "staging batch position {batch_index} cannot be represented"
        ))
    })?;
    let chunk_index = chunk_index_offset.checked_add(batch_index).ok_or_else(|| {
        StagingError::StagingCorrupt(format!(
            "staging chunk index overflow at offset {chunk_index_offset}"
        ))
    })?;
    let chunk_index = u32::try_from(chunk_index).map_err(|_| {
        StagingError::StagingCorrupt(format!(
            "staging chunk index {chunk_index} exceeds shard format limit {}",
            u32::MAX
        ))
    })?;
    Ok(i64::from(chunk_index))
}

#[expect(
    clippy::too_many_arguments,
    reason = "the blocking writer boundary receives explicit ownership and durability state"
)]
fn stage_owned_chunks_batch_blocking(
    root: &Path,
    index: &Arc<Mutex<Index>>,
    writer: &Arc<tokio::sync::Mutex<SegmentWriter>>,
    cfg: &StagingConfig,
    metrics: Option<&dyn StagingMetrics>,
    chunks: &[(MerkleHash, Bytes)],
    file_hash: [u8; 32],
    chunk_index_offset: u64,
    position_collision_check: PositionCollisionCheck,
) -> Result<()> {
    let hashes: Vec<(usize, [u8; 32])> = chunks
        .iter()
        .enumerate()
        .map(|(index, (hash, _))| (index, (*hash).into()))
        .collect();
    let (existing, new_chunk_indices) =
        lock_index(index)?.batch_dedup_check(&hashes, &file_hash)?;

    let existing_refs = existing
        .into_iter()
        .map(|(index, chunk_hash, locator, _is_mapped)| {
            Ok(PendingRow {
                chunk_hash,
                file_hash,
                chunk_index: staging_chunk_index(chunk_index_offset, index)?,
                size: i64::from(locator.length),
                segment_id: locator.segment_id,
                segment_offset: locator.offset,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if !existing_refs.is_empty() {
        lock_index(index)?.insert_pending(&existing_refs)?;
    }
    if new_chunk_indices.is_empty() {
        return Ok(());
    }

    let new_chunk_positions = new_chunk_indices
        .iter()
        .map(|&index| staging_chunk_index(chunk_index_offset, index))
        .collect::<Result<Vec<_>>>()?;
    if position_collision_check == PositionCollisionCheck::Required
        && let Some(chunk_index) =
            lock_index(index)?.first_pending_position_for_file(&file_hash, &new_chunk_positions)?
    {
        return Err(StagingError::StagingCorrupt(format!(
            "pending chunk collision at chunk_index {chunk_index}: retire stale rows before re-stage"
        )));
    }

    let mut first_new_index_by_hash = HashMap::with_capacity(new_chunk_indices.len());
    let mut unique_new_indices = Vec::with_capacity(new_chunk_indices.len());
    let mut duplicate_new_refs = Vec::new();
    for index in new_chunk_indices {
        let hash: [u8; 32] = chunks[index].0.into();
        if let Some(&first_index) = first_new_index_by_hash.get(&hash) {
            duplicate_new_refs.push((index, first_index));
        } else {
            first_new_index_by_hash.insert(hash, index);
            unique_new_indices.push(index);
        }
    }

    let prepared_records = unique_new_indices
        .iter()
        .map(|&index| PreparedRecord::new(chunks[index].1.as_ref()))
        .collect::<Result<Vec<_>>>()?;
    let mut pending_rows = Vec::with_capacity(
        unique_new_indices
            .len()
            .saturating_add(duplicate_new_refs.len()),
    );
    let mut locator_by_new_index = HashMap::with_capacity(unique_new_indices.len());
    let mut total_bytes_written = 0u64;
    let mut writer = writer.blocking_lock();
    let mut record_cursor = 0usize;
    while record_cursor < prepared_records.len() {
        if writer.should_seal() {
            if !pending_rows.is_empty() {
                lock_index(index)?.insert_pending(&pending_rows)?;
                pending_rows.clear();
            }
            seal_current_segment_blocking(root, index, cfg, metrics, &mut writer)?;
        }

        let locators = writer.append_prepared_until_soft_cap(&prepared_records[record_cursor..])?;
        let appended = locators.len();
        for (relative_index, locator) in locators.into_iter().enumerate() {
            let index_in_batch = unique_new_indices[record_cursor + relative_index];
            locator_by_new_index.insert(index_in_batch, locator);
            total_bytes_written = total_bytes_written
                .checked_add(u64::from(locator.length) + 8)
                .ok_or_else(|| {
                    StagingError::StagingCorrupt("staging batch byte count overflow".to_owned())
                })?;
            pending_rows.push(PendingRow {
                chunk_hash: chunks[index_in_batch].0.into(),
                file_hash,
                chunk_index: staging_chunk_index(chunk_index_offset, index_in_batch)?,
                size: i64::from(locator.length),
                segment_id: locator.segment_id,
                segment_offset: locator.offset,
            });
        }
        record_cursor += appended;
    }

    for (index_in_batch, first_index) in duplicate_new_refs {
        let locator = locator_by_new_index.get(&first_index).ok_or_else(|| {
            StagingError::Internal("staging batch duplicate missing first locator".to_owned())
        })?;
        pending_rows.push(PendingRow {
            chunk_hash: chunks[index_in_batch].0.into(),
            file_hash,
            chunk_index: staging_chunk_index(chunk_index_offset, index_in_batch)?,
            size: i64::from(locator.length),
            segment_id: locator.segment_id,
            segment_offset: locator.offset,
        });
    }
    if !pending_rows.is_empty() {
        lock_index(index)?.insert_pending(&pending_rows)?;
    }
    if let Some(metrics) = metrics {
        metrics.add_staging_bytes_written(total_bytes_written);
    }

    if writer.pending_bytes() >= FLUSH_THRESHOLD {
        if writer.should_seal() {
            seal_current_segment_blocking(root, index, cfg, metrics, &mut writer)?;
        } else {
            let segment_id = writer.segment_id();
            let write_offset = writer.write_offset();
            writer.file.sync_data()?;
            writer.reset_pending();
            if let Some(metrics) = metrics {
                metrics.inc_staging_fsyncs();
            }
            lock_index(index)?.flush_pending(segment_id, write_offset)?;
        }
    }

    Ok(())
}

fn seal_current_segment_blocking(
    root: &Path,
    index: &Arc<Mutex<Index>>,
    cfg: &StagingConfig,
    metrics: Option<&dyn StagingMetrics>,
    writer: &mut SegmentWriter,
) -> Result<()> {
    let segments_dir = root.join("segments");
    let old_id = writer.segment_id();
    let old_size = writer.write_offset();
    let old_writer = std::mem::replace(
        writer,
        SegmentWriter::new(
            &segments_dir,
            0,
            cfg.segment_target_bytes,
            cfg.segment_hard_cap_bytes,
        )?,
    );
    old_writer.seal(&segments_dir)?;
    if let Some(metrics) = metrics {
        metrics.inc_staging_segments_sealed();
        metrics.inc_staging_fsyncs();
    }

    let new_id = {
        let index = lock_index(index)?;
        let flushed = index.flush_pending(old_id, old_size)?;
        index.seal_segment(old_id, old_size)?;
        debug!(
            segment_id = old_id,
            bytes = old_size,
            chunks = flushed,
            "sealed"
        );
        let new_id = index.allocate_segment_id()?;
        index.register_current_segment(new_id)?;
        new_id
    };
    *writer = SegmentWriter::new(
        &segments_dir,
        new_id,
        cfg.segment_target_bytes,
        cfg.segment_hard_cap_bytes,
    )?;
    Ok(())
}

impl StagingArea {
    /// Open or create a staging area at the given root path.
    ///
    /// Acquires an advisory flock, runs schema migrations, performs
    /// crash recovery, and opens the current segment for writing.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::StagingLocked`] if another process holds
    /// the lock, [`StagingError::StagingCorrupt`] if the on-disk layout
    /// is invalid, or [`StagingError::Io`] on filesystem failure.
    pub async fn open(root: PathBuf) -> Result<Self> {
        let staging = Self::open_with_lock(root, LockAcquisition::NonBlocking)?;
        complete_recipe_payload_validation(&staging.root, &staging.index, &staging.readers).await?;
        Ok(staging)
    }

    /// Open the staging area, blocking up to `budget` for another
    /// holder to release the exclusive lock.
    ///
    /// Callers that tolerate queueing — notably the clean filter
    /// invoked by `git status` / `git add`, where parallel git
    /// invocations would otherwise fail with `StagingLocked` — use
    /// this variant so concurrent filter-processes serialize cleanly
    /// instead of racing on `LOCK_NB`.
    ///
    /// When `budget` elapses without acquiring the lock, returns
    /// [`StagingError::StagingLocked`] with the holder's PID (if any),
    /// identical to the non-blocking path — so error handling is
    /// uniform.
    pub async fn open_blocking(root: PathBuf, budget: std::time::Duration) -> Result<Self> {
        let staging = Self::open_with_lock(root, LockAcquisition::Blocking(budget))?;
        complete_recipe_payload_validation(&staging.root, &staging.index, &staging.readers).await?;
        Ok(staging)
    }

    /// Open the staging area with the default blocking budget
    /// ([`FLOCK_BLOCKING_DEFAULT_BUDGET`], currently 120 seconds).
    ///
    /// Convenience wrapper for call sites that want the blocking
    /// behavior but don't need to customize the budget.
    pub async fn open_blocking_default(root: PathBuf) -> Result<Self> {
        let staging = Self::open_with_lock(
            root,
            LockAcquisition::Blocking(FLOCK_BLOCKING_DEFAULT_BUDGET),
        )?;
        complete_recipe_payload_validation(&staging.root, &staging.index, &staging.readers).await?;
        Ok(staging)
    }

    /// Shared implementation for `open` and `open_blocking`. Runs the
    /// v1-layout check, acquires the lock per `acq`, and performs the
    /// standard post-lock initialization (index, recovery, segment).
    fn open_with_lock(root: PathBuf, acq: LockAcquisition) -> Result<Self> {
        let cfg = StagingConfig::default();
        cfg.validate()?;

        std::fs::create_dir_all(&root)?;

        // Reject v1 layout: a `chunks/` directory means old per-chunk layout.
        if root.join("chunks").exists() {
            return Err(StagingError::StagingCorrupt(
                "v1 per-chunk layout detected (chunks/ directory exists); \
                 delete the staging root and retry"
                    .into(),
            ));
        }

        let lock_file = match acq {
            LockAcquisition::NonBlocking => acquire_flock_exclusive(&root)?,
            LockAcquisition::Blocking(budget) => acquire_flock_exclusive_blocking(&root, budget)?,
        };

        Self::open_with_acquired_lock(root, cfg, lock_file)
    }

    fn open_with_acquired_lock(root: PathBuf, cfg: StagingConfig, lock_file: File) -> Result<Self> {
        let db_path = root.join("index.db");
        let index = Index::open(&db_path)?;

        let segments_dir = root.join("segments");
        std::fs::create_dir_all(&segments_dir)?;

        // Run crash recovery: verify sealed segments, truncate current
        // segment to last committed offset, clean orphan temps.
        let recovered = recovery::recover(&root, &index)?;

        // Reuse the recovered current segment or allocate a fresh one.
        let writer = if let Some(rec) = recovered {
            SegmentWriter::open_recovered(
                &segments_dir,
                rec.segment_id,
                rec.write_offset,
                cfg.segment_target_bytes,
                cfg.segment_hard_cap_bytes,
            )?
        } else {
            let segment_id = index.allocate_segment_id()?;
            index.register_current_segment(segment_id)?;
            SegmentWriter::new(
                &segments_dir,
                segment_id,
                cfg.segment_target_bytes,
                cfg.segment_hard_cap_bytes,
            )?
        };

        let readers = ReaderPool::new(segments_dir, cfg.fd_pool_size);

        debug!(root = %root.display(), segment_id = writer.segment_id(), "staging area opened");

        Ok(Self {
            root,
            index: Arc::new(Mutex::new(index)),
            writer: Arc::new(tokio::sync::Mutex::new(writer)),
            readers: Arc::new(readers),
            cfg,
            metrics: None,
            _lock_file: lock_file,
        })
    }

    /// Open the staging area, force-breaking a stale lock if the holder
    /// process is dead.
    ///
    /// First attempts a normal exclusive open. If that fails with
    /// `StagingLocked`, checks whether the recorded PID is still alive.
    /// If the holder is dead, removes the lockfile and retries.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::StagingLocked`] if the holder is still
    /// alive or no PID is recorded (cannot determine staleness).
    pub async fn open_force(root: PathBuf) -> Result<Self> {
        match Self::open(root.clone()).await {
            Ok(sa) => Ok(sa),
            Err(StagingError::StagingLocked { .. }) => {
                if let Some(lock_file) = force_break_stale_lock(&root)? {
                    let cfg = StagingConfig::default();
                    cfg.validate()?;
                    let staging = Self::open_with_acquired_lock(root, cfg, lock_file)?;
                    complete_recipe_payload_validation(
                        &staging.root,
                        &staging.index,
                        &staging.readers,
                    )
                    .await?;
                    Ok(staging)
                } else {
                    let holder_pid = read_lock_holder(&root);
                    Err(StagingError::StagingLocked { holder_pid })
                }
            }
            Err(e) => Err(e),
        }
    }

    /// Stage a batch of chunks for a file in a single pass.
    ///
    /// This is the high-performance path for `git add`. Instead of
    /// acquiring locks per-chunk, it:
    /// 1. Batch dedup check — one index lock, one pass over all hashes
    /// 2. Batch segment append — one writer lock for all new chunks
    /// 3. Batch SQLite insert — one transaction for all pending rows
    /// 4. One flush check at the end
    ///
    /// The file must already exist in the `files` table (via
    /// [`pre_register_file`]).
    ///
    /// # Cancel safety
    ///
    /// If dropped before the SQLite insert commits, recovery truncates
    /// unreferenced segment bytes. If dropped after the insert but
    /// before flush, recovery discards pending rows and truncates the
    /// segment to the last committed offset. Either the locator row and
    /// bytes survive together, or neither is visible after recovery.
    ///
    /// Stage a batch of chunks under `file_hash`, starting at
    /// `chunk_index_offset`.
    ///
    /// Clean filter callers invoke this multiple times per file, once
    /// per CDC-output batch. Each call's chunks are assigned
    /// `chunk_index = offset + i`, so consecutive batches produce
    /// non-overlapping `(file_hash, chunk_index)` keys. A conflicting
    /// pending row is corruption, not a dedup hit.
    ///
    /// Pass `0` for the first batch of a file and the running chunk
    /// count for subsequent batches. The batch insert is atomic.
    pub async fn stage_chunks_batch(
        &self,
        chunks: &[(&MerkleHash, &[u8])],
        file_hash: &MerkleHash,
        chunk_index_offset: u64,
    ) -> Result<()> {
        let chunks = chunks
            .iter()
            .map(|(hash, data)| (**hash, Bytes::copy_from_slice(data)))
            .collect();
        self.stage_owned_chunks_batch_with_position_check(
            chunks,
            file_hash,
            chunk_index_offset,
            PositionCollisionCheck::Required,
        )
        .await
    }

    /// Stage a batch for a file whose stale chunk rows were already retired.
    ///
    /// Use only when the caller has just called [`retire_file`] for this
    /// `file_hash` and owns the non-overlapping chunk positions for the
    /// current staging pass. The pending insert still rejects conflicting
    /// positions, but this skips the extra pre-append position probe on
    /// the trusted hot path.
    pub async fn stage_chunks_batch_for_retired_file(
        &self,
        chunks: &[(&MerkleHash, &[u8])],
        file_hash: &MerkleHash,
        chunk_index_offset: u64,
    ) -> Result<()> {
        let chunks = chunks
            .iter()
            .map(|(hash, data)| (**hash, Bytes::copy_from_slice(data)))
            .collect();
        self.stage_owned_chunks_batch_with_position_check(
            chunks,
            file_hash,
            chunk_index_offset,
            PositionCollisionCheck::AlreadyRetired,
        )
        .await
    }

    async fn stage_owned_chunks_batch_for_retired_file(
        &self,
        chunks: Vec<(MerkleHash, Bytes)>,
        file_hash: &MerkleHash,
        chunk_index_offset: u64,
    ) -> Result<()> {
        self.stage_owned_chunks_batch_with_position_check(
            chunks,
            file_hash,
            chunk_index_offset,
            PositionCollisionCheck::AlreadyRetired,
        )
        .await
    }

    async fn stage_owned_chunks_batch_with_position_check(
        &self,
        chunks: Vec<(MerkleHash, Bytes)>,
        file_hash: &MerkleHash,
        chunk_index_offset: u64,
        position_collision_check: PositionCollisionCheck,
    ) -> Result<()> {
        if chunks.is_empty() {
            return Ok(());
        }

        staging_chunk_index(chunk_index_offset, chunks.len() - 1)?;
        let root = self.root.clone();
        let index = Arc::clone(&self.index);
        let writer = Arc::clone(&self.writer);
        let cfg = self.cfg.clone();
        let metrics = self.metrics.clone();
        let file_hash: [u8; 32] = (*file_hash).into();
        tokio::task::spawn_blocking(move || {
            stage_owned_chunks_batch_blocking(
                &root,
                &index,
                &writer,
                &cfg,
                metrics.as_deref(),
                &chunks,
                file_hash,
                chunk_index_offset,
                position_collision_check,
            )
        })
        .await
        .map_err(|e| StagingError::Internal(format!("staging writer join: {e}")))?
    }

    /// fsync the current segment and record its durable byte boundary.
    pub async fn flush_pending(&self) -> Result<()> {
        let (segment_id, write_offset) = {
            let mut writer = self.writer.lock().await;
            if writer.pending_bytes() == 0 {
                return Ok(());
            }
            let id = writer.segment_id();
            let offset = writer.write_offset();
            let file = writer.file.try_clone()?;
            tokio::task::spawn_blocking(move || file.sync_data())
                .await
                .map_err(|e| StagingError::Internal(format!("fsync join: {e}")))??;
            writer.reset_pending();
            (id, offset)
        };

        if let Some(ref m) = self.metrics {
            m.inc_staging_fsyncs();
        }

        lock_index(&self.index)?.flush_pending(segment_id, write_offset)?;
        Ok(())
    }

    /// Read a staged chunk by hash, verifying CRC and Blake3.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::CrcMismatch`] on integrity failure,
    /// [`StagingError::HashMismatch`] if the data doesn't match the
    /// requested hash, or [`StagingError::Io`] on read failure.
    pub async fn get_chunk(&self, hash: &MerkleHash) -> Result<Option<Bytes>> {
        let hash_bytes: [u8; 32] = (*hash).into();

        let (locator, prepared_exists) = {
            let index = lock_index(&self.index)?;
            let locator = index.locate(&hash_bytes)?;
            let prepared_exists = locator.is_none()
                && index
                    .locate_prepared_batch(&[hash_bytes])?
                    .first()
                    .is_some_and(Option::is_some);
            (locator, prepared_exists)
        };
        let Some(locator) = locator else {
            if !prepared_exists {
                return Ok(None);
            }
            return self
                .get_chunks_batch(&[*hash])
                .await
                .map(|mut chunks| chunks.pop().map(|(_, data)| data));
        };

        // Get a reader from the pool.
        let reader = self.readers.get(locator.segment_id)?;

        // pread + CRC verification in spawn_blocking.
        let offset = locator.offset;
        let length = locator.length;
        let data = tokio::task::spawn_blocking(move || reader.read(offset, length))
            .await
            .map_err(|e| StagingError::Internal(format!("pread join: {e}")))??;

        if let Some(ref m) = self.metrics {
            m.add_staging_bytes_read(u64::from(length));
        }

        // Blake3 hash verification.
        let actual_hash = compute_data_hash(&data);
        if actual_hash != *hash {
            return Err(StagingError::HashMismatch {
                requested: hash.hex(),
                actual: actual_hash.hex(),
            });
        }

        Ok(Some(data))
    }

    /// Read multiple staged chunks in a batch.
    pub async fn get_chunks_batch(
        &self,
        hashes: &[MerkleHash],
    ) -> Result<Vec<(MerkleHash, Bytes)>> {
        read_staged_chunks_batch(
            &self.root,
            &self.index,
            &self.readers,
            self.metrics.as_deref(),
            hashes,
        )
        .await
    }

    /// Check if a chunk exists in staging (pure `SQLite` lookup, no FS stat).
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::Internal`] on `SQLite` failure.
    #[expect(
        clippy::unused_async,
        reason = "async signature matches v1 API contract"
    )]
    pub async fn has_chunk(&self, hash: &MerkleHash) -> Result<bool> {
        let hash_bytes: [u8; 32] = (*hash).into();
        let index = lock_index(&self.index)?;
        Ok(index.chunk_exists_anywhere(&hash_bytes)?
            || index
                .locate_prepared_batch(&[hash_bytes])?
                .first()
                .is_some_and(Option::is_some))
    }

    /// Pre-insert a file row so pending rows can reference the final
    /// file hash while chunking is still in progress.
    ///
    /// Updates file metadata in place so calling `register_file`
    /// afterwards is safe and idempotent without detaching chunk rows.
    pub fn pre_register_file(&self, file_hash: &MerkleHash, total_bytes: u64) -> Result<()> {
        let fh: [u8; 32] = (*file_hash).into();
        lock_index(&self.index)?.insert_file(&fh, total_bytes)?;
        Ok(())
    }

    /// Pre-register a file with its original path for UX display.
    ///
    /// Same as [`pre_register_file`] but also records the file path in
    /// the `file_paths` side table so the desktop UI can show filenames
    /// instead of raw hashes.
    pub fn pre_register_file_with_path(
        &self,
        file_hash: &MerkleHash,
        total_bytes: u64,
        file_path: &str,
    ) -> Result<()> {
        let fh: [u8; 32] = (*file_hash).into();
        let idx = lock_index(&self.index)?;
        idx.insert_file(&fh, total_bytes)?;
        idx.insert_file_path(&fh, file_path)?;
        Ok(())
    }

    /// Record the original file path for a staged file hash.
    ///
    /// Advisory — used by the clean filter to persist the path mapping
    /// after staging completes. Non-fatal if the file_paths table
    /// doesn't exist yet (old staging areas).
    pub fn record_file_path(&self, file_hash: &MerkleHash, file_path: &str) -> Result<()> {
        let fh: [u8; 32] = (*file_hash).into();
        lock_index(&self.index)?.insert_file_path(&fh, file_path)?;
        Ok(())
    }

    /// Create an attempt-unique staging batch before recording path leases.
    pub fn create_batch(&self) -> Result<StagingBatchId> {
        let batch_id = new_staging_batch_id();
        lock_index(&self.index)?.insert_batch(batch_id.as_str())?;
        Ok(batch_id)
    }

    /// Persist an unverified immutable recipe and lease it to one native-byte path.
    ///
    /// The recipe remains invisible to push until the next staging open
    /// physically verifies its chunk payloads and whole-file hash.
    pub fn record_recipe_lease(
        &self,
        batch_id: &StagingBatchId,
        path: &Path,
        recipe: &crate::recipe::FileRecipe,
    ) -> Result<()> {
        let path_bytes = staging_path_bytes(path);
        let index = lock_index(&self.index)?;
        index.insert_recipe_lease(
            batch_id.as_str(),
            &path_bytes,
            recipe,
            RecipeVerification::Pending,
        )?;
        Ok(())
    }

    /// Persist a recipe already proven by the caller's single source-byte read.
    ///
    /// The caller must have computed the file hash, ordered chunk hashes, and
    /// sizes from the same stable byte stream and staged those exact chunks.
    /// This attestation lets push consume the recipe without reconstructing the
    /// whole file a second time; missing chunks are still hash-checked on read.
    pub fn record_verified_recipe_lease(
        &self,
        batch_id: &StagingBatchId,
        path: &Path,
        recipe: &crate::recipe::FileRecipe,
    ) -> Result<()> {
        let path_bytes = staging_path_bytes(path);
        let index = lock_index(&self.index)?;
        index.insert_recipe_lease(
            batch_id.as_str(),
            &path_bytes,
            recipe,
            RecipeVerification::CallerVerified,
        )?;
        Ok(())
    }

    /// Mark a batch as published after its Git index replacement commits.
    pub fn mark_batch_published(&self, batch_id: &StagingBatchId) -> Result<()> {
        lock_index(&self.index)?.mark_batch_published(batch_id.as_str())
    }

    /// Publish one complete recipe outside the multi-path `crab add` index transaction.
    ///
    /// Import and migration writers have their own durable commit boundary, so
    /// each path can publish independently. A failed publication removes the
    /// attempt lease and reclaims bytes only when no other batch owns them.
    pub fn publish_verified_recipe_lease(
        &self,
        path: &Path,
        recipe: &crate::recipe::FileRecipe,
    ) -> Result<StagingBatchId> {
        let batch_id = self.create_batch()?;
        let file_hash = recipe.sequence().file_hash;
        let publish = (|| -> Result<()> {
            self.pre_register_file(&file_hash, recipe.sequence().file_size)?;
            self.record_verified_recipe_lease(&batch_id, path, recipe)?;
            self.record_file_path(&file_hash, &path.to_string_lossy())?;
            self.mark_batch_published(&batch_id)
        })();
        if let Err(error) = publish {
            let _ = self.rollback_batch(&batch_id);
            let _ = self.retire_file_if_unleased(&file_hash);
            return Err(error);
        }
        Ok(batch_id)
    }

    /// Remove one batch's leases and reclaim only recipes with no other owner.
    pub fn rollback_batch(&self, batch_id: &StagingBatchId) -> Result<Vec<RetireStats>> {
        let unleased = lock_index(&self.index)?.rollback_batch(batch_id.as_str())?;
        let mut retired = Vec::with_capacity(unleased.len());
        for file_hash in unleased {
            retired.push(self.retire_file(&MerkleHash::from(file_hash))?);
            self.unregister_file(&MerkleHash::from(file_hash))?;
        }
        Ok(retired)
    }

    /// Reclaim a recipe only when no path lease in any batch still owns it.
    pub fn retire_file_if_unleased(&self, file_hash: &MerkleHash) -> Result<Option<RetireStats>> {
        let file_hash_bytes: [u8; 32] = (*file_hash).into();
        if lock_index(&self.index)?.has_file_lease(&file_hash_bytes)? {
            return Ok(None);
        }
        let retired = self.retire_file(file_hash)?;
        self.unregister_file(file_hash)?;
        Ok(Some(retired))
    }

    /// Move staged rows from a provisional hash to the final file hash.
    ///
    /// Used by the clean filter's streaming path: chunk bytes are
    /// written before the final Blake3 hash is known, then adopted
    /// under the final hash before the pointer is emitted.
    pub fn adopt_staged_file(
        &self,
        source_file_hash: &MerkleHash,
        target_file_hash: &MerkleHash,
        total_bytes: u64,
    ) -> Result<u64> {
        let source: [u8; 32] = (*source_file_hash).into();
        let target: [u8; 32] = (*target_file_hash).into();
        let adopted = lock_index(&self.index)?.adopt_file_hash(&source, &target, total_bytes)?;
        if source_file_hash != target_file_hash {
            push_plan::remove_file_push_plan(&self.root, source_file_hash)?;
            push_plan::remove_file_push_plan(&self.root, target_file_hash)?;
        }
        Ok(adopted)
    }

    /// Delete every staged chunk for a file from both `chunks` and
    /// `pending_chunks` and decrement the corresponding segments'
    /// `live_chunk_count`.
    ///
    /// Intended for the re-add path in `crab add`: chunks from a
    /// prior add for this file must be retired before the new chunks
    /// are staged, otherwise the new pending rows collide with the old
    /// `(file_hash, chunk_index)` positions after segment bytes have
    /// already been appended.
    ///
    /// A `file_hash` with no staged chunks returns empty stats — the
    /// method is a no-op in that case.
    pub fn retire_file(&self, file_hash: &MerkleHash) -> Result<RetireStats> {
        let fh: [u8; 32] = (*file_hash).into();
        let (rows_deleted, segments_touched) =
            lock_index(&self.index)?.delete_chunks_for_file(&fh)?;
        push_plan::remove_file_push_plan(&self.root, file_hash)?;
        Ok(RetireStats {
            rows_deleted,
            segments_touched,
        })
    }

    /// Return the ordered list of chunk hashes staged for a given file.
    pub fn chunks_for_file(&self, file_hash: &MerkleHash) -> Result<Vec<MerkleHash>> {
        if let Some(recipe) = self.published_recipe_for_file(file_hash)? {
            return Ok(recipe
                .sequence()
                .spans
                .iter()
                .map(|span| span.chunk_hash)
                .collect());
        }
        let fh: [u8; 32] = (*file_hash).into();
        let raw_hashes = lock_index(&self.index)?.chunks_for_file(&fh)?;
        Ok(raw_hashes.into_iter().map(MerkleHash::from).collect())
    }

    /// Return the ordered staged chunk hashes and sizes for a file.
    pub fn chunks_for_file_with_sizes(
        &self,
        file_hash: &MerkleHash,
    ) -> Result<Vec<(MerkleHash, u64)>> {
        if let Some(recipe) = self.published_recipe_for_file(file_hash)? {
            return Ok(recipe
                .sequence()
                .spans
                .iter()
                .map(|span| (span.chunk_hash, span.len))
                .collect());
        }
        let fh: [u8; 32] = (*file_hash).into();
        let chunks = lock_index(&self.index)?.chunks_for_file_with_sizes(&fh)?;
        Ok(chunks
            .into_iter()
            .map(|(hash, size)| (MerkleHash::from(hash), size))
            .collect())
    }

    /// Return the immutable recipe owned by a published staging batch.
    pub fn published_recipe_for_file(
        &self,
        file_hash: &MerkleHash,
    ) -> Result<Option<crate::recipe::FileRecipe>> {
        let file_hash: [u8; 32] = (*file_hash).into();
        lock_index(&self.index)?.published_recipe_for_file(&file_hash)
    }

    /// Promote a verified add-time push plan into the staging index.
    #[expect(
        clippy::unused_async,
        reason = "async signature matches add-time planning callers"
    )]
    pub async fn write_file_push_plan(&self, plan: &push_plan::FilePushPlan) -> Result<()> {
        let (file_hash, chunks) =
            validate_file_push_plan_matches_staging(&self.root, &self.index, plan)?;
        let plan_json = push_plan::serialize_file_push_plan(plan)?;
        let sequence_hash = push_plan::chunk_sequence_hash(&chunks);
        let prepared_xorbs = prepared_xorb_index_records(plan, &chunks)?;
        let retained_prepared_xorbs = prepared_xorbs
            .iter()
            .map(|record| MerkleHash::from(record.xorb_hash))
            .collect::<HashSet<_>>();
        let fh: [u8; 32] = file_hash.into();
        let chunk_count = u64::try_from(chunks.len()).map_err(|_| {
            StagingError::StagingCorrupt(format!(
                "add-time push plan for file {} has too many chunks",
                file_hash.hex()
            ))
        })?;
        lock_index(&self.index)?.insert_file_push_plan(
            &fh,
            plan.version,
            plan.file_size,
            chunk_count,
            &sequence_hash,
            &plan_json,
            &prepared_xorbs,
        )?;
        if let Err(error) =
            push_plan::retain_file_prepared_xorbs(&self.root, &file_hash, &retained_prepared_xorbs)
        {
            warn!(
                file_hash = %file_hash.hex(),
                error = %error,
                "failed to prune stale prepared xorb payloads"
            );
        }
        Ok(())
    }

    /// Load the indexed add-time push plan for a file.
    #[expect(
        clippy::unused_async,
        reason = "async signature matches read-only staging API"
    )]
    pub async fn load_file_push_plan(
        &self,
        file_hash: &MerkleHash,
    ) -> Result<Option<push_plan::FilePushPlan>> {
        authoritative_file_push_plan(&self.root, &self.index, file_hash)
    }

    pub(crate) fn load_prepared_xorb_cache_for_chunks(
        &self,
        wanted_chunks: &HashSet<MerkleHash>,
    ) -> Result<push_plan::PreparedXorbCache> {
        indexed_prepared_xorb_cache_for_chunks(&self.root, &self.index, wanted_chunks)
    }

    pub(crate) fn chunks_for_file_with_locators(
        &self,
        file_hash: &MerkleHash,
    ) -> Result<Vec<StagedChunkLocator>> {
        let fh: [u8; 32] = (*file_hash).into();
        let chunks = lock_index(&self.index)?.chunks_for_file_with_locators(&fh)?;
        Ok(chunks.into_iter().map(StagedChunkLocator::from).collect())
    }

    /// Register a file and its chunks in the index.
    ///
    /// For each chunk in the list, verifies it exists in `chunks` or
    /// `pending_chunks`, then links the file to those locators in
    /// `chunks`. Segment `live_chunk_count` accounting is updated in
    /// the same transaction that replaces the chunk rows.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::Internal`] if any chunk is not found in
    /// staging, or on `SQLite` failure.
    pub fn register_file(
        &self,
        file_hash: &MerkleHash,
        total_bytes: u64,
        chunks: &[(MerkleHash, u64)],
    ) -> Result<()> {
        let fh: [u8; 32] = (*file_hash).into();
        let idx = lock_index(&self.index)?;

        // Verify all chunks exist before inserting.
        for (ch, _size) in chunks {
            let ch_bytes: [u8; 32] = (*ch).into();
            if !idx.chunk_exists_anywhere(&ch_bytes)? {
                return Err(StagingError::Internal(format!(
                    "chunk {} not found in staging during register_file",
                    ch.hex()
                )));
            }
        }

        // Insert the file row.
        idx.insert_file(&fh, total_bytes)?;

        // Insert chunk rows with locator info from existing chunks/pending.
        let chunk_pairs: Vec<([u8; 32], u64)> = chunks
            .iter()
            .map(|(ch, size)| ((*ch).into(), *size))
            .collect();
        idx.insert_chunks_for_file(&fh, &chunk_pairs)?;

        debug!(
            file_hash = %file_hash.hex(),
            chunks = chunks.len(),
            "registered file"
        );

        Ok(())
    }

    /// Remove a file and its chunks from the staging index.
    ///
    /// Deletes the file row, all associated chunk and pending-chunk rows,
    /// and decrements `live_chunk_count` on affected segments. Segment
    /// files are not deleted here — the next `clean` or `compact` pass
    /// will reclaim dead space.
    ///
    /// Returns `true` if the file existed and was removed, `false` if it
    /// was not found in staging.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::Internal`] on `SQLite` failure.
    pub fn unregister_file(&self, file_hash: &MerkleHash) -> Result<bool> {
        let fh: [u8; 32] = (*file_hash).into();
        let affected = {
            let idx = lock_index(&self.index)?;
            if !idx.file_exists(&fh)? {
                return Ok(false);
            }
            idx.remove_file(&fh)?
        };
        push_plan::remove_file_push_plan(&self.root, file_hash)?;

        debug!(
            file_hash = %file_hash.hex(),
            segments_affected = affected.len(),
            "unregistered file from staging"
        );

        Ok(true)
    }

    /// Create a push-inflight marker file.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::Io`] on filesystem failure.
    pub fn mark_push_inflight(&self, push_id: &str) -> Result<()> {
        let stale = mark_push_inflight_marker(&self.root, push_id)?;
        let index = lock_index(&self.index)?;
        for stale_id in stale {
            index.remove_open_push_snapshot(&stale_id)?;
        }
        Ok(())
    }

    /// Remove a push-inflight marker. Removing a non-existent marker is a no-op.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::Io`] on filesystem failure.
    pub fn clear_push_inflight(&self, push_id: &str) -> Result<()> {
        clear_push_inflight_marker(&self.root, push_id)
    }

    /// List push-inflight marker IDs.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::Io`] on filesystem failure.
    pub fn list_inflight(&self) -> Result<Vec<String>> {
        list_inflight_ids(&self.root)
    }

    /// Remove orphan segments with zero live chunks.
    ///
    /// Completes in O(segments), never walks individual chunks on disk.
    /// Returns the number of segments removed and accumulated stats for
    /// `bytes_reclaimed` and `chunks_reclaimed`.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::Internal`] on `SQLite` failure or
    /// [`StagingError::Io`] on filesystem failure.
    pub fn sweep_orphans(&self) -> Result<(u64, u64, u64)> {
        let segments_dir = self.root.join("segments");
        let idx = lock_index(&self.index)?;

        let candidates = idx.sweep_candidates()?;
        let mut segments_removed: u64 = 0;
        let mut bytes_reclaimed: u64 = 0;
        let mut chunks_reclaimed: u64 = 0;

        for seg_id in &candidates {
            let (size_bytes, chunk_count) = idx.segment_info(*seg_id)?;

            let seg_path = segments_dir.join(format!("{seg_id:016x}.seg"));
            match std::fs::remove_file(&seg_path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // Already gone — still drop the index rows.
                }
                Err(e) => return Err(e.into()),
            }

            idx.drop_segment(*seg_id)?;

            segments_removed += 1;
            bytes_reclaimed += size_bytes;
            chunks_reclaimed += chunk_count;
        }

        drop(idx);

        // fsync the segments directory after the batch unlink.
        if segments_removed > 0 {
            let dir_fd = File::open(&segments_dir)?;
            dir_fd.sync_all()?;
        }

        debug!(
            segments_removed,
            bytes_reclaimed, chunks_reclaimed, "sweep_orphans complete"
        );

        Ok((segments_removed, bytes_reclaimed, chunks_reclaimed))
    }

    /// Clean staging: remove stale markers, sweep orphans, optionally compact.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::Internal`] on `SQLite` failure or
    /// [`StagingError::Io`] on filesystem failure.
    pub fn clean(&self) -> Result<StagingCleanStats> {
        // 1. The exclusive staging lock proves no push reader is alive. Clear
        // stale markers, complete any post-CAS snapshot retirement, and only
        // then remove snapshot journals. Open snapshots never crossed the
        // recorded manifest-commit transition and keep their path leases.
        let inflight = self.list_inflight()?;
        let mut stale_markers_removed: u64 = 0;
        for id in &inflight {
            self.clear_push_inflight(id)?;
            stale_markers_removed += 1;
        }
        let snapshots = lock_index(&self.index)?.push_snapshot_states()?;
        for (snapshot_id, state) in snapshots {
            if state == "open" {
                lock_index(&self.index)?.remove_open_push_snapshot(&snapshot_id)?;
                continue;
            }
            let unleased = lock_index(&self.index)?.retire_push_snapshot(&snapshot_id)?;
            for file_hash in unleased {
                let file_hash = MerkleHash::from(file_hash);
                self.retire_file(&file_hash)?;
                self.unregister_file(&file_hash)?;
            }
            lock_index(&self.index)?.remove_push_snapshot(&snapshot_id)?;
        }

        // 2. Sweep orphan segments.
        let (segments_removed, bytes_reclaimed, chunks_reclaimed) = self.sweep_orphans()?;

        // 3. Compact if auto_compact is enabled.
        let mut segments_compacted: u64 = 0;
        if self.cfg.auto_compact {
            let compaction_stats = self.compact_sync(true).unwrap_or_default();
            segments_compacted = compaction_stats.segments_compacted;
        }

        debug!(
            segments_removed,
            segments_compacted,
            bytes_reclaimed,
            chunks_reclaimed,
            stale_markers_removed,
            "clean complete"
        );

        Ok(StagingCleanStats {
            segments_removed,
            segments_compacted,
            bytes_reclaimed,
            chunks_reclaimed,
            stale_markers_removed,
        })
    }

    /// Reclaim segments that were rolled over but never sealed (pre-fix
    /// orphans) or otherwise abandoned with no chunk locator rows.
    ///
    /// Skips the writer's live current segment unless it has no chunk
    /// locator rows and still has non-zero bytes on disk.
    ///
    /// Refuses to run when push-inflight markers exist unless `force`
    /// is true.
    ///
    /// A segment is "abandoned" when it has zero rows in both `chunks`
    /// and `pending_chunks`. Pending rows can be valid staged content
    /// waiting for push, so cleanup must not treat them as disposable.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::Internal`] on `SQLite` failure or
    /// [`StagingError::Io`] on filesystem failure.
    pub async fn clean_abandoned(&self, force: bool) -> Result<(u64, u64, u64)> {
        if !force {
            let inflight = self.list_inflight()?;
            if !inflight.is_empty() {
                return Err(StagingError::StagingCorrupt(format!(
                    "clean_abandoned refused: {} push(es) inflight. \
                     Wait for them to finish, or pass force=true.",
                    inflight.len()
                )));
            }
        }

        let (current_id, current_write_offset) = {
            let writer = self.writer.lock().await;
            (writer.segment_id(), writer.write_offset())
        };

        let segments_dir = self.root.join("segments");

        let (candidates, current_is_abandoned) = {
            let idx = lock_index(&self.index)?;
            let candidates = idx.abandoned_segments(current_id)?;
            // The current segment counts as abandoned only when it has
            // no locator rows at all and still has bytes on disk. Pending
            // rows may back a pointer already written to Git's index, so
            // preserve them until retire_file removes them explicitly.
            let committed = idx.segment_committed_chunk_count(current_id)?;
            let pending = idx.segment_pending_chunk_count(current_id)?;
            let (persisted_size, _) = idx.segment_info(current_id).unwrap_or((0, 0));
            let current_size = persisted_size.max(current_write_offset);
            let current_is_abandoned = committed == 0 && pending == 0 && current_size > 0;
            (candidates, current_is_abandoned)
        };

        let mut segments_removed: u64 = 0;
        let mut bytes_reclaimed: u64 = 0;
        let mut pending_removed: u64 = 0;

        for (seg_id, size_bytes) in &candidates {
            let n = lock_index(&self.index)?.delete_pending_for_segment(*seg_id)?;
            pending_removed += n;

            let seg_path = segments_dir.join(format!("{seg_id:016x}.seg"));
            match std::fs::remove_file(&seg_path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }

            lock_index(&self.index)?.drop_segment(*seg_id)?;

            segments_removed += 1;
            bytes_reclaimed += size_bytes;
        }

        // Reset the current segment when it has bytes on disk but no
        // locator rows. Keep the writer running by rolling it onto a
        // fresh segment id with an empty `current.seg`.
        if current_is_abandoned {
            let (old_size, n_pending) = {
                let idx = lock_index(&self.index)?;
                let persisted_size = idx.segment_info(current_id).map(|(s, _)| s).unwrap_or(0);
                let size = persisted_size.max(current_write_offset);
                let n = idx.delete_pending_for_segment(current_id)?;
                idx.drop_segment(current_id)?;
                (size, n)
            };

            // Drop the existing current.seg, reallocate a fresh id
            // under the writer lock, and rotate the writer onto it.
            let tmp_path = segments_dir.join("current.seg");
            match std::fs::remove_file(&tmp_path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }

            let new_id = {
                let idx = lock_index(&self.index)?;
                let id = idx.allocate_segment_id()?;
                idx.register_current_segment(id)?;
                id
            };

            let mut writer = self.writer.lock().await;
            *writer = SegmentWriter::new(
                &segments_dir,
                new_id,
                self.cfg.segment_target_bytes,
                self.cfg.segment_hard_cap_bytes,
            )?;

            segments_removed += 1;
            bytes_reclaimed += old_size;
            pending_removed += n_pending;

            debug!(
                old_segment_id = current_id,
                new_segment_id = new_id,
                reclaimed = old_size,
                pending_removed = n_pending,
                "reset orphan current segment"
            );
        }

        if segments_removed > 0 {
            let dir_fd = File::open(&segments_dir)?;
            dir_fd.sync_all()?;
        }

        // Sweep any pending_chunks rows whose segment was dropped by
        // a previous run that missed the pending cleanup. Safe because
        // no segment row → no .seg file → no way to ever read those
        // chunks back.
        let orphan_pending = {
            let idx = lock_index(&self.index)?;
            idx.delete_orphan_pending_rows()?
        };
        pending_removed += orphan_pending;

        debug!(
            segments_removed,
            bytes_reclaimed, pending_removed, orphan_pending, "clean_abandoned complete"
        );

        Ok((segments_removed, bytes_reclaimed, pending_removed))
    }

    /// Return a point-in-time snapshot of staging statistics.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::Internal`] on `SQLite` failure.
    pub fn stats(&self) -> Result<stats::StagingStats> {
        let current_segment_bytes = self.writer.blocking_lock().write_offset();
        let idx = lock_index(&self.index)?;
        idx.staging_stats(current_segment_bytes)
    }

    pub fn lifecycle_health(&self) -> Result<stats::StagingLifecycleHealth> {
        lock_index(&self.index)?.lifecycle_health()
    }

    /// List registered files while holding the exclusive staging handle.
    pub fn list_files(&self) -> Result<Vec<index::StagedFileInfo>> {
        lock_index(&self.index)?.list_files_with_chunks()
    }

    /// Compact segments whose dead-byte ratio exceeds the threshold.
    ///
    /// Refuses to run when push-inflight markers exist. Use
    /// [`compact_force`](Self::compact_force) to bypass this check.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::Internal`] on `SQLite` failure or
    /// [`StagingError::Io`] on filesystem failure.
    #[expect(
        clippy::unused_async,
        reason = "async signature for API consistency with other staging methods"
    )]
    pub async fn compact(&self) -> Result<CompactionStats> {
        self.compact_sync(false)
    }

    /// Force compaction even when push-inflight markers exist.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::Internal`] on `SQLite` failure or
    /// [`StagingError::Io`] on filesystem failure.
    #[expect(
        clippy::unused_async,
        reason = "async signature for API consistency with other staging methods"
    )]
    pub async fn compact_force(&self) -> Result<CompactionStats> {
        self.compact_sync(true)
    }

    /// Synchronous compaction implementation.
    ///
    /// When `force` is false, refuses to run if push-inflight markers exist.
    fn compact_sync(&self, force: bool) -> Result<CompactionStats> {
        if !force {
            let inflight = self.list_inflight()?;
            if !inflight.is_empty() {
                debug!(
                    inflight_count = inflight.len(),
                    "compaction skipped: push-inflight markers present"
                );
                if let Some(ref m) = self.metrics {
                    m.inc_staging_compactions_skipped_inflight();
                }
                return Ok(CompactionStats::default());
            }
        }

        let segments_dir = self.root.join("segments");
        let dead_ratio = self.cfg.compact_dead_ratio;

        let candidates = lock_index(&self.index)?.compaction_candidates(dead_ratio)?;

        if candidates.is_empty() {
            return Ok(CompactionStats::default());
        }

        let mut total_stats = CompactionStats::default();

        for old_segment_id in &candidates {
            let stats = self.compact_one_segment(*old_segment_id, &segments_dir)?;
            total_stats.segments_compacted += stats.segments_compacted;
            total_stats.bytes_reclaimed += stats.bytes_reclaimed;
            total_stats.chunks_moved += stats.chunks_moved;
        }

        tracing::info!(
            segments_compacted = total_stats.segments_compacted,
            bytes_reclaimed = total_stats.bytes_reclaimed,
            chunks_moved = total_stats.chunks_moved,
            "compacted"
        );

        if let Some(ref m) = self.metrics {
            for _ in 0..total_stats.segments_compacted {
                m.inc_staging_segments_compacted();
            }
        }

        Ok(total_stats)
    }

    /// Compact a single segment: read live chunks from old, write to new,
    /// swap locators atomically, delete old file.
    fn compact_one_segment(
        &self,
        old_segment_id: u64,
        segments_dir: &Path,
    ) -> Result<CompactionStats> {
        let live_chunks = lock_index(&self.index)?.live_chunks_for_segment(old_segment_id)?;

        if live_chunks.is_empty() {
            return Ok(CompactionStats::default());
        }

        // Allocate a new segment id for the compacted output.
        let new_segment_id = lock_index(&self.index)?.allocate_segment_id()?;

        // Open the old segment for reading.
        let old_reader = self.readers.get(old_segment_id)?;

        // Write to a temp file first — if we crash before the SQLite tx
        // commits, recovery discards the orphan temp (Req S3.4 / S5.4).
        let tmp_path = segments_dir.join("current.seg.tmp");
        let new_seg_path = segments_dir.join(format!("{new_segment_id:016x}.seg"));

        let mut new_file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(&tmp_path)?;

        let mut new_offset: u64 = 0;
        let mut updates: Vec<([u8; 32], [u8; 32], i64, u64)> =
            Vec::with_capacity(live_chunks.len());

        for (chunk_hash, file_hash, chunk_index, size, old_offset) in &live_chunks {
            #[expect(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "size is always non-negative and bounded by segment hard cap"
            )]
            let length = *size as u32;

            // Read the full record from the old segment (CRC-verified).
            let data = old_reader.read(*old_offset, length)?;

            // Re-encode and write to the new segment.
            let record = segment::encode_record(&data);
            std::io::Write::write_all(&mut new_file, &record)?;

            updates.push((*chunk_hash, *file_hash, *chunk_index, new_offset));

            new_offset += record.len() as u64;
        }

        // fsync the new segment before making it visible.
        new_file.sync_data()?;
        drop(new_file);

        // Rename temp → final name (atomic on POSIX).
        std::fs::rename(&tmp_path, &new_seg_path)?;

        // fsync the directory to make the rename durable.
        let dir_fd = File::open(segments_dir)?;
        dir_fd.sync_all()?;

        // Atomic locator swap in SQLite.
        lock_index(&self.index)?.swap_locators(
            old_segment_id,
            new_segment_id,
            new_offset,
            &updates,
        )?;

        // Now safe to delete the old segment file.
        let old_seg_path = segments_dir.join(format!("{old_segment_id:016x}.seg"));
        let old_size = std::fs::metadata(&old_seg_path).map_or(0, |m| m.len());

        match std::fs::remove_file(&old_seg_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }

        // fsync directory after unlink.
        let dir_fd = File::open(segments_dir)?;
        dir_fd.sync_all()?;

        // Drop the old segment from the index (it has 0 live chunks now).
        lock_index(&self.index)?.drop_segment(old_segment_id)?;

        let bytes_reclaimed = old_size.saturating_sub(new_offset);

        debug!(
            old_segment_id,
            new_segment_id,
            chunks_moved = updates.len(),
            bytes_reclaimed,
            "compacted segment"
        );

        Ok(CompactionStats {
            segments_compacted: 1,
            bytes_reclaimed,
            chunks_moved: updates.len() as u64,
        })
    }

    /// Explicitly close the staging area, flushing and releasing resources.
    ///
    /// Flushes any pending chunks, fsyncs the current segment, and drops
    /// all resources. The advisory flock is released when the lock file
    /// handle is dropped.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::Io`] on fsync failure or
    /// [`StagingError::Internal`] on `SQLite` failure.
    pub async fn close(self) -> Result<()> {
        // Flush any remaining pending chunks.
        self.flush_pending().await?;
        // Dropping self releases the writer, readers, index, and lock file.
        Ok(())
    }

    /// Path to the staging root directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Attach shared metrics counters for staging operations.
    ///
    /// Call after construction to wire staging events into the
    /// application-wide [`StagingMetrics`] instance.
    pub fn set_metrics<M>(&mut self, metrics: Arc<M>)
    where
        M: StagingMetrics + 'static,
    {
        self.metrics = Some(metrics);
    }
}

impl Drop for StagingArea {
    fn drop(&mut self) {
        // All resources are RAII-managed and close deterministically:
        // - SQLite Connection closes when the Index is dropped
        //   (Mutex<Index> → Index → Connection::drop)
        // - Segment writer fd closes when SegmentWriter is dropped
        //   (tokio::sync::Mutex<SegmentWriter> → SegmentWriter → File::drop)
        // - Reader pool fds close when ReaderPool is dropped
        //   (Arc<File> refcounts reach zero → File::drop)
        // - Advisory flock releases when _lock_file is dropped
        //   (File::drop closes the fd, kernel releases the flock)
    }
}

// --- StagingAreaReadOnly implementation ---

impl StagingAreaReadOnly {
    /// Open a shared push handle to the staging area.
    ///
    /// Acquires a shared (`LOCK_SH`) advisory flock so multiple readers
    /// can coexist with each other (but not with an exclusive writer
    /// that hasn't released yet). No segment writer is opened; lifecycle-only
    /// index writes remain serialized by SQLite WAL transactions.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::StagingLocked`] if an exclusive lock
    /// cannot be shared (should not happen in normal operation),
    /// [`StagingError::StagingCorrupt`] if the layout is invalid, or
    /// [`StagingError::Io`] on filesystem failure.
    pub async fn open(root: PathBuf) -> Result<Self> {
        let staging = Self::open_with_lock(root, LockAcquisition::NonBlocking)?;
        complete_recipe_payload_validation(&staging.root, &staging.index, &staging.readers).await?;
        Ok(staging)
    }

    /// Read-only open, blocking up to `budget` for a writer to release.
    ///
    /// Used by the push pipeline so a concurrent clean filter session
    /// that holds the exclusive write lock queues the push instead of
    /// failing it — which matters because the clean filter is often
    /// invoked by `git status` / IDE integrations while the user is
    /// pushing.
    pub async fn open_blocking(root: PathBuf, budget: std::time::Duration) -> Result<Self> {
        let staging = Self::open_with_lock(root, LockAcquisition::Blocking(budget))?;
        complete_recipe_payload_validation(&staging.root, &staging.index, &staging.readers).await?;
        Ok(staging)
    }

    /// Read-only open with the default blocking budget
    /// ([`FLOCK_BLOCKING_DEFAULT_BUDGET`]).
    pub async fn open_blocking_default(root: PathBuf) -> Result<Self> {
        let staging = Self::open_with_lock(
            root,
            LockAcquisition::Blocking(FLOCK_BLOCKING_DEFAULT_BUDGET),
        )?;
        complete_recipe_payload_validation(&staging.root, &staging.index, &staging.readers).await?;
        Ok(staging)
    }

    /// Shared implementation for read-only open variants.
    fn open_with_lock(root: PathBuf, acq: LockAcquisition) -> Result<Self> {
        if !root.exists() {
            return Err(StagingError::NotFound {
                path: root.display().to_string(),
            });
        }

        let lock_file = match acq {
            LockAcquisition::NonBlocking => acquire_flock_shared(&root)?,
            LockAcquisition::Blocking(budget) => acquire_flock_shared_blocking(&root, budget)?,
        };

        let db_path = root.join("index.db");
        if !db_path.exists() {
            return Err(StagingError::NotFound {
                path: db_path.display().to_string(),
            });
        }
        let index = Index::open_readonly(&db_path)?;

        let segments_dir = root.join("segments");
        let cfg = StagingConfig::default();
        let readers = ReaderPool::new(segments_dir, cfg.fd_pool_size);

        debug!(root = %root.display(), "staging area opened (read-only)");

        Ok(Self {
            root,
            index: Arc::new(Mutex::new(index)),
            readers: Arc::new(readers),
            metrics: None,
            _lock_file: lock_file,
        })
    }

    /// Read a staged chunk by hash, verifying CRC and Blake3.
    pub async fn get_chunk(&self, hash: &MerkleHash) -> Result<Option<Bytes>> {
        let hash_bytes: [u8; 32] = (*hash).into();

        let (locator, prepared_exists) = {
            let index = lock_index(&self.index)?;
            let locator = index.locate(&hash_bytes)?;
            let prepared_exists = locator.is_none()
                && index
                    .locate_prepared_batch(&[hash_bytes])?
                    .first()
                    .is_some_and(Option::is_some);
            (locator, prepared_exists)
        };
        let Some(locator) = locator else {
            if !prepared_exists {
                return Ok(None);
            }
            return self
                .get_chunks_batch(&[*hash])
                .await
                .map(|mut chunks| chunks.pop().map(|(_, data)| data));
        };

        let reader = self.readers.get(locator.segment_id)?;
        let offset = locator.offset;
        let length = locator.length;
        let data = tokio::task::spawn_blocking(move || reader.read(offset, length))
            .await
            .map_err(|e| StagingError::Internal(format!("pread join: {e}")))??;

        if let Some(ref m) = self.metrics {
            m.add_staging_bytes_read(u64::from(length));
        }

        let actual_hash = compute_data_hash(&data);
        if actual_hash != *hash {
            return Err(StagingError::HashMismatch {
                requested: hash.hex(),
                actual: actual_hash.hex(),
            });
        }

        Ok(Some(data))
    }

    /// Read multiple staged chunks in a batch.
    ///
    /// Resolves all locators in a single SQLite round-trip, groups reads
    /// by segment file (sorted by offset for sequential access), and
    /// verifies CRC32C + blake3 for each chunk. Returns chunks in the
    /// same order as `hashes`.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::ChunkNotFound`] if any requested hash is
    /// absent from staging. Same verification errors as [`get_chunk`]
    /// on corruption: [`StagingError::CrcMismatch`],
    /// [`StagingError::HashMismatch`], or [`StagingError::Io`].
    pub async fn get_chunks_batch(
        &self,
        hashes: &[MerkleHash],
    ) -> Result<Vec<(MerkleHash, Bytes)>> {
        read_staged_chunks_batch(
            &self.root,
            &self.index,
            &self.readers,
            self.metrics.as_deref(),
            hashes,
        )
        .await
    }

    /// Check if a chunk exists in staging.
    #[expect(
        clippy::unused_async,
        reason = "async signature matches StagingArea API"
    )]
    pub async fn has_chunk(&self, hash: &MerkleHash) -> Result<bool> {
        let hash_bytes: [u8; 32] = (*hash).into();
        let index = lock_index(&self.index)?;
        Ok(index.chunk_exists_anywhere(&hash_bytes)?
            || index
                .locate_prepared_batch(&[hash_bytes])?
                .first()
                .is_some_and(Option::is_some))
    }

    /// Return the ordered list of chunk hashes staged for a given file.
    pub fn chunks_for_file(&self, file_hash: &MerkleHash) -> Result<Vec<MerkleHash>> {
        if let Some(recipe) = self.published_recipe_for_file(file_hash)? {
            return Ok(recipe
                .sequence()
                .spans
                .iter()
                .map(|span| span.chunk_hash)
                .collect());
        }
        let fh: [u8; 32] = (*file_hash).into();
        let raw_hashes = lock_index(&self.index)?.chunks_for_file(&fh)?;
        Ok(raw_hashes.into_iter().map(MerkleHash::from).collect())
    }

    /// Return the ordered staged chunk hashes and sizes for a file.
    pub fn chunks_for_file_with_sizes(
        &self,
        file_hash: &MerkleHash,
    ) -> Result<Vec<(MerkleHash, u64)>> {
        if let Some(recipe) = self.published_recipe_for_file(file_hash)? {
            return Ok(recipe
                .sequence()
                .spans
                .iter()
                .map(|span| (span.chunk_hash, span.len))
                .collect());
        }
        let fh: [u8; 32] = (*file_hash).into();
        let chunks = lock_index(&self.index)?.chunks_for_file_with_sizes(&fh)?;
        Ok(chunks
            .into_iter()
            .map(|(hash, size)| (MerkleHash::from(hash), size))
            .collect())
    }

    /// Return the immutable recipe owned by a published staging batch.
    pub fn published_recipe_for_file(
        &self,
        file_hash: &MerkleHash,
    ) -> Result<Option<crate::recipe::FileRecipe>> {
        let file_hash: [u8; 32] = (*file_hash).into();
        lock_index(&self.index)?.published_recipe_for_file(&file_hash)
    }

    /// Load the indexed add-time push plan for a file.
    #[expect(
        clippy::unused_async,
        reason = "async signature matches writable staging API"
    )]
    pub async fn load_file_push_plan(
        &self,
        file_hash: &MerkleHash,
    ) -> Result<Option<push_plan::FilePushPlan>> {
        authoritative_file_push_plan(&self.root, &self.index, file_hash)
    }

    /// Retire the staged chunks for a successfully-pushed file.
    ///
    /// Removes every `chunks` row with this `file_hash` and decrements
    /// `live_chunk_count` on each touched segment. Segments that drop
    /// to zero live chunks and are sealed become eligible for reclaim
    /// on the next `sweep_orphans` call — this method does not touch
    /// segment files on disk.
    ///
    /// Only the index lock is acquired (not a segment writer lock), so
    /// concurrent clean-filter staging work is unaffected. Safe to
    /// call from the post-push cleanup path. A `file_hash` with no
    /// staged chunks (e.g. a fast-path pointer) returns an empty
    /// [`RetireStats`] — the method is a no-op in that case.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::Internal`] on `SQLite` failure.
    #[expect(
        clippy::unused_async,
        reason = "async signature matches the rest of the staging API; callers already await"
    )]
    pub async fn retire_file(&self, file_hash: &MerkleHash) -> Result<RetireStats> {
        let fh: [u8; 32] = (*file_hash).into();
        let (rows_deleted, segments_touched) =
            lock_index(&self.index)?.delete_chunks_for_file(&fh)?;
        push_plan::remove_file_push_plan(&self.root, file_hash)?;
        Ok(RetireStats {
            rows_deleted,
            segments_touched,
        })
    }

    /// Mark a staged batch published after the Git index commit.
    pub fn mark_batch_published(&self, batch_id: &StagingBatchId) -> Result<()> {
        lock_index(&self.index)?.mark_batch_published(batch_id.as_str())
    }

    /// Roll back one batch while preserving recipes leased by another path.
    pub async fn rollback_batch(&self, batch_id: &StagingBatchId) -> Result<Vec<RetireStats>> {
        let unleased = lock_index(&self.index)?.rollback_batch(batch_id.as_str())?;
        let mut retired = Vec::with_capacity(unleased.len());
        for file_hash in unleased {
            let file_hash = MerkleHash::from(file_hash);
            let stats = self.retire_file(&file_hash).await?;
            let file_hash_bytes: [u8; 32] = file_hash.into();
            lock_index(&self.index)?.remove_file(&file_hash_bytes)?;
            retired.push(stats);
        }
        Ok(retired)
    }

    /// Pin the exact published recipes read by one push.
    pub fn create_push_snapshot(
        &self,
        push_id: &str,
        recipes: &[crate::recipe::FileRecipe],
    ) -> Result<()> {
        lock_index(&self.index)?.create_push_snapshot(push_id, recipes)
    }

    /// Mark a recipe snapshot committed before local lease retirement.
    pub fn commit_push_snapshot(&self, push_id: &str) -> Result<()> {
        lock_index(&self.index)?.commit_push_snapshot(push_id)
    }

    /// Retire the exact leases pinned by one committed push snapshot.
    pub async fn retire_push_snapshot(&self, push_id: &str) -> Result<Vec<RetireStats>> {
        let unleased = lock_index(&self.index)?.retire_push_snapshot(push_id)?;
        let mut retired = Vec::with_capacity(unleased.len());
        for file_hash in unleased {
            let file_hash = MerkleHash::from(file_hash);
            let stats = self.retire_file(&file_hash).await?;
            let file_hash_bytes: [u8; 32] = file_hash.into();
            lock_index(&self.index)?.remove_file(&file_hash_bytes)?;
            retired.push(stats);
        }
        Ok(retired)
    }

    /// Remove a push recipe snapshot on success or failure.
    pub fn remove_push_snapshot(&self, push_id: &str) -> Result<()> {
        lock_index(&self.index)?.remove_push_snapshot(push_id)
    }

    /// Create a push-inflight marker file from a shared push handle.
    ///
    /// Push uses shared staging locks so multiple local pushes can read
    /// staged chunks concurrently. Markers prevent one successful push
    /// from retiring segment rows while another push is still packing
    /// from them.
    pub fn mark_push_inflight(&self, push_id: &str) -> Result<()> {
        let stale = mark_push_inflight_marker(&self.root, push_id)?;
        let index = lock_index(&self.index)?;
        for stale_id in stale {
            index.remove_open_push_snapshot(&stale_id)?;
        }
        Ok(())
    }

    /// Remove a push-inflight marker. Removing a non-existent marker is a no-op.
    pub fn clear_push_inflight(&self, push_id: &str) -> Result<()> {
        clear_push_inflight_marker(&self.root, push_id)
    }

    /// Mark that this handle is retiring pushed staging rows.
    ///
    /// Returns `Ok(None)` when another push is already inflight. While
    /// the returned guard is alive, new push markers fail with
    /// [`StagingError::StagingLocked`] so they cannot start packing rows
    /// that cleanup is about to retire.
    pub fn begin_retirement(&self, push_id: &str) -> Result<Option<StagingRetirementGuard>> {
        let (guard, stale) = begin_retirement_marker(&self.root, push_id)?;
        let index = lock_index(&self.index)?;
        for stale_id in stale {
            index.remove_open_push_snapshot(&stale_id)?;
        }
        Ok(guard)
    }

    /// Unlink segment files that have zero live chunks.
    ///
    /// Intended to run immediately after a successful push retires every
    /// pushed file's chunks: segments that backed those chunks are now
    /// unreferenced and their bytes reclaimable. Sealed segments use
    /// their id-named files; the healthy lone unsealed segment uses
    /// `current.seg`. Segments shared with other live files are left
    /// intact.
    ///
    /// Safe from a read-only handle: the `LOCK_SH` advisory flock blocks
    /// any concurrent `StagingArea` writer, and the index mutex
    /// serializes the SQL delete + unlink within this process. Does not
    /// run compaction (rewriting live bytes needs exclusive access).
    ///
    /// Returns `(segments_removed, bytes_reclaimed, chunks_reclaimed)`.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::Internal`] on `SQLite` failure or
    /// [`StagingError::Io`] on filesystem failure.
    pub fn sweep_orphans(&self) -> Result<(u64, u64, u64)> {
        let segments_dir = self.root.join("segments");
        let idx = lock_index(&self.index)?;

        let candidates = idx.sweep_candidates()?;
        let mut segments_removed: u64 = 0;
        let mut bytes_reclaimed: u64 = 0;
        let mut chunks_reclaimed: u64 = 0;

        for seg_id in &candidates {
            let (size_bytes, chunk_count) = idx.segment_info(*seg_id)?;

            let seg_path = segments_dir.join(format!("{seg_id:016x}.seg"));
            match std::fs::remove_file(&seg_path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // Already gone — still drop the index rows below.
                }
                Err(e) => return Err(e.into()),
            }

            idx.drop_segment(*seg_id)?;

            segments_removed += 1;
            bytes_reclaimed += size_bytes;
            chunks_reclaimed += chunk_count;
        }

        if let Some((seg_id, size_bytes, chunk_count)) = idx.abandoned_current_segment()? {
            let seg_path = segments_dir.join("current.seg");
            match std::fs::remove_file(&seg_path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // Already gone — still drop the index row below.
                }
                Err(e) => return Err(e.into()),
            }

            idx.drop_segment(seg_id)?;

            segments_removed += 1;
            bytes_reclaimed += size_bytes;
            chunks_reclaimed += chunk_count;
        }

        drop(idx);

        // fsync the segments directory after the batch unlink so the
        // removal survives a crash.
        if segments_removed > 0 {
            let dir_fd = File::open(&segments_dir)?;
            dir_fd.sync_all()?;
        }

        debug!(
            segments_removed,
            bytes_reclaimed, chunks_reclaimed, "sweep_orphans (read-only) complete"
        );

        Ok((segments_removed, bytes_reclaimed, chunks_reclaimed))
    }

    /// List push-inflight marker IDs.
    pub fn list_inflight(&self) -> Result<Vec<String>> {
        list_inflight_ids(&self.root)
    }

    /// Return a point-in-time snapshot of staging statistics.
    pub fn stats(&self) -> Result<stats::StagingStats> {
        let current_path = self.root.join("segments/current.seg");
        let current_segment_bytes = match std::fs::metadata(&current_path) {
            Ok(metadata) => metadata.len(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
            Err(error) => return Err(error.into()),
        };
        lock_index(&self.index)?.staging_stats(current_segment_bytes)
    }

    pub fn lifecycle_health(&self) -> Result<stats::StagingLifecycleHealth> {
        lock_index(&self.index)?.lifecycle_health()
    }

    /// Return add-time push-plan inventory for currently staged files.
    pub async fn push_plan_stats(
        &self,
        options: push_plan::PushPlanSummaryOptions,
    ) -> Result<push_plan::PushPlanStats> {
        let files = lock_index(&self.index)?.list_files_with_chunks()?;
        let mut stats = push_plan::new_push_plan_stats(options);
        let mut referenced_xorbs = HashSet::new();
        let mut referenced_indexed_xorbs = HashMap::new();

        for file in files {
            let file_hash = MerkleHash::from(file.file_hash);
            let plan = match self.load_file_push_plan(&file_hash).await {
                Ok(Some(plan)) => plan,
                Ok(None) => continue,
                Err(_) => {
                    stats.invalid_plan_files += 1;
                    continue;
                }
            };
            if !push_plan::accumulate_file_plan_for_hash(
                &self.root,
                &file_hash,
                &plan,
                options,
                &mut stats,
                &mut referenced_xorbs,
            )? {
                stats.invalid_plan_files += 1;
            }
            for planned_xorb in &plan.prepared_xorbs {
                let xorb_hash = planned_xorb.hash()?;
                referenced_indexed_xorbs.insert(
                    (file.file_hash, <[u8; 32]>::from(xorb_hash)),
                    IndexedPreparedXorbStatsRef {
                        payload_hash: planned_xorb.payload_hash_bytes()?,
                        bytes: planned_xorb.bytes,
                        planned_json: push_plan::serialize_planned_xorb(planned_xorb)?,
                    },
                );
            }
        }

        for row in lock_index(&self.index)?.raw_prepared_xorb_rows()? {
            stats.indexed_prepared_xorbs += 1;
            let Some(file_hash) = hash_blob_for_stats(&row.file_hash) else {
                stats.invalid_indexed_prepared_xorbs += 1;
                continue;
            };
            let Some(xorb_hash) = hash_blob_for_stats(&row.xorb_hash) else {
                stats.invalid_indexed_prepared_xorbs += 1;
                continue;
            };
            let Some(expected) = referenced_indexed_xorbs.get(&(file_hash, xorb_hash)) else {
                stats.orphaned_indexed_prepared_xorbs += 1;
                continue;
            };
            let Some(payload_hash) = hash_blob_for_stats(&row.payload_hash) else {
                stats.invalid_indexed_prepared_xorbs += 1;
                continue;
            };
            let Ok(bytes) = u64::try_from(row.bytes) else {
                stats.invalid_indexed_prepared_xorbs += 1;
                continue;
            };
            if payload_hash != expected.payload_hash
                || bytes != expected.bytes
                || row.planned_json != expected.planned_json
            {
                stats.invalid_indexed_prepared_xorbs += 1;
            }
        }
        push_plan::scan_stale_prepared_xorbs(&self.root, &referenced_xorbs, &mut stats)?;
        Ok(stats)
    }

    /// List all files in the staging index with per-file chunk breakdown.
    ///
    /// Returns one entry per registered file with committed/pending chunk
    /// counts and segment distribution. Useful for the desktop UI's
    /// staging area panel.
    pub fn list_files(&self) -> Result<Vec<index::StagedFileInfo>> {
        lock_index(&self.index)?.list_files_with_chunks()
    }

    /// Verify the staging index and every referenced chunk payload.
    ///
    /// Reads staged chunks through the normal segment readers, so CRC
    /// and Blake3 validation match the push path. Repeated chunk hashes
    /// are read once while every file reference is still size-checked.
    ///
    /// # Errors
    ///
    /// Returns [`StagingError::StagingCorrupt`] for inconsistent index
    /// metadata, or the same read/verification errors as
    /// [`Self::get_chunks_batch`] for corrupt segment bytes.
    pub async fn verify(&self) -> Result<StagingVerifyStats> {
        let files = self.list_files()?;
        let mut summary = StagingVerifyStats {
            files_checked: u64::try_from(files.len())
                .map_err(|_| StagingError::Internal("too many staged files to count".to_owned()))?,
            ..StagingVerifyStats::default()
        };
        let mut expected_size_by_hash: HashMap<MerkleHash, u64> = HashMap::new();

        for file in files {
            let file_hash = MerkleHash::from(file.file_hash);
            let chunks = self.chunks_for_file_with_sizes(&file_hash)?;
            let mut file_bytes = 0u64;

            for (chunk_hash, size) in &chunks {
                file_bytes = file_bytes.checked_add(*size).ok_or_else(|| {
                    StagingError::StagingCorrupt(format!(
                        "file {} staged chunk sizes overflow",
                        file_hash.hex()
                    ))
                })?;
                summary.chunk_refs_checked =
                    summary.chunk_refs_checked.checked_add(1).ok_or_else(|| {
                        StagingError::Internal(
                            "too many staged chunk references to count".to_owned(),
                        )
                    })?;

                if let Some(previous_size) = expected_size_by_hash.get(chunk_hash) {
                    if *previous_size != *size {
                        return Err(StagingError::StagingCorrupt(format!(
                            "chunk {} has inconsistent staged sizes: {previous_size} and {}",
                            chunk_hash.hex(),
                            size
                        )));
                    }
                    continue;
                }

                expected_size_by_hash.insert(*chunk_hash, *size);
            }

            if file_bytes != file.total_bytes {
                return Err(StagingError::StagingCorrupt(format!(
                    "file {} staged chunks total {file_bytes} bytes, expected {}",
                    file_hash.hex(),
                    file.total_bytes
                )));
            }
            self.verify_file_reconstruction_from_chunks(&file_hash, file.total_bytes, &chunks)
                .await?;
        }

        summary.unique_chunks_checked =
            u64::try_from(expected_size_by_hash.len()).map_err(|_| {
                StagingError::Internal("too many unique staged chunks to count".to_owned())
            })?;
        for size in expected_size_by_hash.values() {
            summary.bytes_checked = summary.bytes_checked.checked_add(*size).ok_or_else(|| {
                StagingError::Internal("too many staged bytes to count".to_owned())
            })?;
        }

        Ok(summary)
    }

    /// Verify that one staged file reconstructs to `file_hash`.
    ///
    /// This is the push-path guard for index corruption that leaves
    /// individual chunk hashes valid but orders or associates them under
    /// the wrong file. It reads through the normal segment readers so
    /// per-chunk CRC and Blake3 checks still apply.
    pub async fn verify_file_reconstruction(
        &self,
        file_hash: &MerkleHash,
        expected_size: u64,
    ) -> Result<()> {
        let chunks = self.chunks_for_file_with_sizes(file_hash)?;
        self.verify_file_reconstruction_from_chunks(file_hash, expected_size, &chunks)
            .await?;
        Ok(())
    }

    pub async fn verify_file_reconstruction_from_chunks(
        &self,
        file_hash: &MerkleHash,
        expected_size: u64,
        chunks: &[(MerkleHash, u64)],
    ) -> Result<u64> {
        let mut hasher = blake3::Hasher::new();
        let mut bytes_checked = 0u64;

        for batch in chunks.chunks(VERIFY_BATCH_CHUNKS) {
            self.verify_file_chunk_batch(batch, &mut hasher, &mut bytes_checked)
                .await?;
        }

        if bytes_checked != expected_size {
            return Err(StagingError::StagingCorrupt(format!(
                "file {} staged chunks total {bytes_checked} bytes, expected {expected_size}",
                file_hash.hex()
            )));
        }

        let actual_hash = MerkleHash::from(*hasher.finalize().as_bytes());
        if actual_hash != *file_hash {
            return Err(StagingError::StagingCorrupt(format!(
                "staged chunks for file {} reconstruct to {}",
                file_hash.hex(),
                actual_hash.hex()
            )));
        }

        Ok(bytes_checked)
    }

    async fn verify_file_chunk_batch(
        &self,
        batch: &[(MerkleHash, u64)],
        hasher: &mut blake3::Hasher,
        bytes_checked: &mut u64,
    ) -> Result<()> {
        let hashes: Vec<MerkleHash> = batch.iter().map(|(hash, _)| *hash).collect();
        let payloads = self.get_chunks_batch(&hashes).await?;

        for ((expected_hash, expected_size), (actual_hash, data)) in batch.iter().zip(payloads) {
            if actual_hash != *expected_hash {
                return Err(StagingError::StagingCorrupt(format!(
                    "verification read chunk {}, expected {}",
                    actual_hash.hex(),
                    expected_hash.hex()
                )));
            }
            let actual_size = u64::try_from(data.len()).map_err(|_| {
                StagingError::Internal("staged payload is too large to count".to_owned())
            })?;
            if actual_size != *expected_size {
                return Err(StagingError::StagingCorrupt(format!(
                    "chunk {} size mismatch: index says {expected_size} bytes, segment has {actual_size} bytes",
                    expected_hash.hex()
                )));
            }
            *bytes_checked = bytes_checked.checked_add(actual_size).ok_or_else(|| {
                StagingError::Internal("too many staged bytes to count".to_owned())
            })?;
            hasher.update(data.as_ref());
        }

        Ok(())
    }

    /// Path to the staging root directory.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Attach shared metrics counters.
    pub fn set_metrics<M>(&mut self, metrics: Arc<M>)
    where
        M: StagingMetrics + 'static,
    {
        self.metrics = Some(metrics);
    }
}

/// Determine if a staged chunk can be passed through to the xorb without
/// re-compression.
///
/// Returns the compressed payload (without the staging header) if the
/// staged chunk was compressed at `target_level`, or `None` if transcoding
/// is needed (level mismatch or raw chunk).
///
/// Retained from v1 for the packer layer; will be revisited when
/// compression moves into the segment format.
#[must_use]
pub fn try_passthrough(raw_staged: &[u8], target_level: i32) -> Option<&[u8]> {
    // Magic bytes identifying a zstd-compressed staging chunk.
    const STAGING_COMPRESSED_MAGIC: &[u8; 4] = b"ZSTG";
    // Length of the staging compression header: 4-byte magic + 1-byte level.
    const STAGING_HEADER_LEN: usize = 5;

    if raw_staged.len() <= STAGING_HEADER_LEN || raw_staged[..4] != *STAGING_COMPRESSED_MAGIC {
        return None;
    }
    let staged_level = i32::from(raw_staged[4]);
    if staged_level == target_level {
        Some(&raw_staged[STAGING_HEADER_LEN..])
    } else {
        warn!(
            staged_level,
            target_level, "staging/xorb compression level mismatch, transcoding"
        );
        None
    }
}

/// Scale a compression ratio (0.0–1.0) to an integer suitable for the
/// `staging_compression_ratio` metrics counter (multiplied by 1000).
#[must_use]
pub fn ratio_to_metric(ratio: f64) -> u64 {
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "ratio is in [0.0, 1.0], so ratio*1000 fits in u64"
    )]
    let scaled = (ratio * 1000.0) as u64;
    scaled
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    #[derive(Default)]
    struct CountingMetrics {
        fsyncs: AtomicU64,
    }

    impl StagingMetrics for CountingMetrics {
        fn inc_staging_fsyncs(&self) {
            self.fsyncs.fetch_add(1, Ordering::Relaxed);
        }
    }

    impl CountingMetrics {
        fn fsyncs(&self) -> u64 {
            self.fsyncs.load(Ordering::Relaxed)
        }
    }

    /// Build a deterministic chunk of `size` bytes seeded by `idx`.
    fn make_chunk(idx: u32, size: usize) -> (MerkleHash, Vec<u8>) {
        let mut data = vec![0u8; size];
        data[..4].copy_from_slice(&idx.to_le_bytes());
        // Vary later bytes so chunks of the same idx+size differ only
        // by the seed — enough to produce distinct blake3 hashes.
        for (i, b) in data.iter_mut().enumerate().skip(4) {
            *b = ((i as u32).wrapping_mul(idx).wrapping_add(0x9E3779B9)) as u8;
        }
        let hash = compute_data_hash(&data);
        (hash, data)
    }

    pub(super) fn make_chunk_pub(idx: u32, size: usize) -> (MerkleHash, Vec<u8>) {
        make_chunk(idx, size)
    }

    #[tokio::test(flavor = "current_thread")]
    async fn blocked_staging_index_does_not_block_async_runtime() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let staging = StagingArea::open(tmp.path().to_path_buf())
            .await
            .expect("open staging");
        let (chunk_hash, data) = make_chunk(16, 4096);
        let file_hash = compute_data_hash(&data);
        staging
            .pre_register_file(&file_hash, data.len() as u64)
            .expect("register file");

        let blocked_index = Arc::clone(&staging.index);
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let blocker = std::thread::spawn(move || {
            let _guard = blocked_index.lock().expect("lock staging index");
            locked_tx.send(()).expect("signal locked index");
            std::thread::sleep(std::time::Duration::from_millis(200));
        });
        locked_rx.recv().expect("wait for blocked index");

        let started = std::time::Instant::now();
        let chunks = [(&chunk_hash, data.as_slice())];
        let stage = staging.stage_chunks_batch(&chunks, &file_hash, 0);
        let heartbeat = async {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            started.elapsed()
        };
        let (stage_result, heartbeat_elapsed) = tokio::join!(stage, heartbeat);
        stage_result.expect("stage after index unblocks");
        blocker.join().expect("join index blocker");

        assert!(
            heartbeat_elapsed < std::time::Duration::from_millis(150),
            "staging blocked the async runtime for {heartbeat_elapsed:?}"
        );
    }

    #[tokio::test]
    async fn readonly_stats_include_pending_current_segment_payloads() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().to_path_buf();
        let (chunk_hash, data) = make_chunk(17, 4096);
        let file_hash = compute_data_hash(&data);

        {
            let staging = StagingArea::open(root.clone()).await.expect("open rw");
            staging
                .pre_register_file(&file_hash, data.len() as u64)
                .expect("register file");
            staging
                .stage_chunks_batch(&[(&chunk_hash, data.as_slice())], &file_hash, 0)
                .await
                .expect("stage pending chunk");
            staging.flush_pending().await.expect("flush pending");
            staging.close().await.expect("close rw");
        }

        let staging = StagingAreaReadOnly::open(root).await.expect("open ro");
        let stats = staging.stats().expect("read stats");
        assert_eq!(stats.current_segment_bytes, data.len() as u64 + 8);
        assert_eq!(stats.total_staged_bytes, data.len() as u64 + 8);
        assert_eq!(stats.chunk_count, 1);
        assert_eq!(stats.live_bytes, data.len() as u64 + 8);
        assert_eq!(stats.dead_bytes, 0);
    }

    pub(super) async fn stage_chunks_as_synthetic_file(
        staging: &StagingArea,
        chunks: &[(MerkleHash, Vec<u8>)],
    ) -> MerkleHash {
        let total_bytes: u64 = chunks.iter().map(|(_, d)| d.len() as u64).sum();
        let mut hasher = blake3::Hasher::new();
        for (_, data) in chunks {
            hasher.update(data);
        }
        let file_hash = MerkleHash::from(*hasher.finalize().as_bytes());
        staging
            .pre_register_file(&file_hash, total_bytes)
            .expect("pre-register");

        let refs: Vec<(&MerkleHash, &[u8])> =
            chunks.iter().map(|(h, d)| (h, d.as_slice())).collect();
        staging
            .stage_chunks_batch(&refs, &file_hash, 0)
            .await
            .expect("batch stage");
        file_hash
    }

    fn corrupt_first_chunk_size(root: &std::path::Path, file_hash: &MerkleHash) {
        let conn = rusqlite::Connection::open(root.join("index.db")).expect("open raw index db");
        let fh: [u8; 32] = (*file_hash).into();
        let committed = conn
            .execute(
                "UPDATE chunks
                 SET size = size + 1
                 WHERE file_hash = ?1 AND chunk_index = 0",
                rusqlite::params![fh.as_slice()],
            )
            .expect("corrupt committed first chunk size");
        let pending = conn
            .execute(
                "UPDATE pending_chunks
                 SET size = size + 1
                 WHERE file_hash = ?1 AND chunk_index = 0",
                rusqlite::params![fh.as_slice()],
            )
            .expect("corrupt pending first chunk size");
        assert_eq!(committed + pending, 1);
    }

    fn assert_stale_plan_error(error: StagingError) {
        assert!(
            matches!(
                error,
                StagingError::StagingCorrupt(ref message)
                    if message.contains("no longer matches staged chunk rows")
            ),
            "unexpected error: {error}"
        );
    }

    fn assert_prepared_xorb_without_file_coverage_error(error: StagingError) {
        assert!(
            matches!(
                error,
                StagingError::StagingCorrupt(ref message)
                    if message.contains("prepared xorb") && message.contains("does not cover file")
            ),
            "unexpected error: {error}"
        );
    }

    fn replace_indexed_file_push_plan_body(
        root: &std::path::Path,
        file_hash: &MerkleHash,
        plan: &push_plan::FilePushPlan,
    ) {
        let conn = rusqlite::Connection::open(root.join("index.db")).expect("open raw index db");
        let fh: [u8; 32] = (*file_hash).into();
        let plan_json = push_plan::serialize_file_push_plan(plan).expect("serialize plan");
        let updated = conn
            .execute(
                "UPDATE file_push_plans
                 SET plan_json = ?1
                 WHERE file_hash = ?2",
                rusqlite::params![plan_json.as_slice(), fh.as_slice()],
            )
            .expect("replace indexed plan body");
        assert_eq!(updated, 1);
    }

    fn prepared_plan_for_first_chunk(
        file_hash: MerkleHash,
        total_bytes: u64,
        chunk_pairs: &[(MerkleHash, u64)],
        xorb_hash: MerkleHash,
        payload: &[u8],
    ) -> push_plan::FilePushPlan {
        let mut plan =
            push_plan::FilePushPlan::new_verified_staging(file_hash, total_bytes, chunk_pairs);
        plan.prepared_xorbs.push(push_plan::PlannedXorb {
            hash: xorb_hash.hex(),
            payload_hash: blake3::hash(payload).to_hex().to_string(),
            bytes: payload.len() as u64,
            upload: true,
            placements: vec![push_plan::PlannedPlacement {
                chunk_hash: chunk_pairs[0].0.hex(),
                xorb_hash: xorb_hash.hex(),
                chunk_index: 0,
                uncompressed_size: u32::try_from(chunk_pairs[0].1).expect("chunk size fits u32"),
            }],
        });
        plan
    }

    fn build_xorb_for_chunks(
        chunks: &[(MerkleHash, Vec<u8>)],
    ) -> (
        Bytes,
        MerkleHash,
        Vec<crab_xet::xorb::format::ChunkPlacement>,
    ) {
        let mut builder = crab_xet::xorb::builder::XorbBuilder::new();
        for (hash, data) in chunks {
            builder
                .push(
                    &crab_xet::xorb::format::Chunk {
                        hash: *hash,
                        data: Bytes::from(data.clone()),
                    },
                    crab_xet::xorb::builder::RunId(0),
                )
                .expect("push test chunk");
        }
        let mut results = builder.finalize().expect("finalize test xorb");
        assert_eq!(results.len(), 1);
        let result = results.pop().expect("one test xorb");
        (result.bytes, result.hash, result.placements)
    }

    fn prepared_plan_for_xorb(
        file_hash: MerkleHash,
        chunk_pairs: &[(MerkleHash, u64)],
        xorb_bytes: &Bytes,
        xorb_hash: MerkleHash,
        placements: &[crab_xet::xorb::format::ChunkPlacement],
    ) -> push_plan::FilePushPlan {
        let total_bytes = chunk_pairs.iter().map(|(_, size)| *size).sum();
        let mut plan =
            push_plan::FilePushPlan::new_verified_staging(file_hash, total_bytes, chunk_pairs);
        plan.prepared_xorbs.push(push_plan::PlannedXorb {
            hash: xorb_hash.hex(),
            payload_hash: blake3::hash(xorb_bytes).to_hex().to_string(),
            bytes: xorb_bytes.len() as u64,
            upload: true,
            placements: placements
                .iter()
                .map(push_plan::PlannedPlacement::from_placement)
                .collect(),
        });
        plan
    }

    fn insert_extra_prepared_xorb_row(
        root: &std::path::Path,
        file_hash: MerkleHash,
        planned: &push_plan::PlannedXorb,
    ) {
        let conn = rusqlite::Connection::open(root.join("index.db")).expect("open raw index db");
        let fh: [u8; 32] = file_hash.into();
        let xorb_hash = planned.hash().expect("planned xorb hash");
        let xh: [u8; 32] = xorb_hash.into();
        let payload_hash = planned.payload_hash_bytes().expect("payload hash");
        let planned_json = push_plan::serialize_planned_xorb(planned).expect("planned xorb json");
        conn.execute(
            "INSERT INTO prepared_xorbs
             (file_hash, xorb_hash, payload_hash, bytes, planned_json, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, datetime('now'))",
            rusqlite::params![
                fh.as_slice(),
                xh.as_slice(),
                payload_hash.as_slice(),
                i64::try_from(planned.bytes).expect("prepared bytes fit sqlite"),
                planned_json.as_slice(),
            ],
        )
        .expect("insert extra prepared xorb");

        for placement in &planned.placements {
            let placement = placement.to_placement().expect("planned placement");
            let chunk_hash: [u8; 32] = placement.chunk_hash.into();
            conn.execute(
                "INSERT INTO prepared_xorb_chunks
                 (file_hash, xorb_hash, chunk_hash, chunk_index, uncompressed_size)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    fh.as_slice(),
                    xh.as_slice(),
                    chunk_hash.as_slice(),
                    i64::from(placement.chunk_index),
                    i64::from(placement.uncompressed_size),
                ],
            )
            .expect("insert extra prepared xorb chunk");
        }
    }

    fn insert_invalid_prepared_xorb_key_row(root: &std::path::Path, file_hash: MerkleHash) {
        let conn = rusqlite::Connection::open(root.join("index.db")).expect("open raw index db");
        let fh: [u8; 32] = file_hash.into();
        let payload_hash = blake3::hash(b"invalid indexed prepared row");
        conn.execute(
            "INSERT INTO prepared_xorbs
             (file_hash, xorb_hash, payload_hash, bytes, planned_json, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, datetime('now'))",
            rusqlite::params![
                fh.as_slice(),
                b"bad".as_slice(),
                payload_hash.as_bytes().as_slice(),
                0_i64,
                b"{}".as_slice(),
            ],
        )
        .expect("insert invalid prepared xorb key");
    }

    fn corrupt_indexed_prepared_xorb_row_bytes(
        root: &std::path::Path,
        file_hash: MerkleHash,
        xorb_hash: MerkleHash,
    ) {
        let conn = rusqlite::Connection::open(root.join("index.db")).expect("open raw index db");
        let fh: [u8; 32] = file_hash.into();
        let xh: [u8; 32] = xorb_hash.into();
        let updated = conn
            .execute(
                "UPDATE prepared_xorbs
                 SET bytes = bytes + 1
                 WHERE file_hash = ?1 AND xorb_hash = ?2",
                rusqlite::params![fh.as_slice(), xh.as_slice()],
            )
            .expect("corrupt prepared xorb row bytes");
        assert_eq!(updated, 1);
    }

    /// Open a fresh writable staging area, stage `chunks`, flush, close,
    /// then reopen read-only. Returns the read-only handle plus the
    /// owned tempdir (kept alive by the caller).
    ///
    /// Uses `stage_chunks_batch` under a synthetic file so each chunk
    /// gets a distinct `(file_hash, chunk_index)` key in
    /// `pending_chunks`.
    async fn populate_and_reopen(
        chunks: &[(MerkleHash, Vec<u8>)],
    ) -> (StagingAreaReadOnly, tempfile::TempDir) {
        let tmp = tempfile::tempdir().expect("tempdir");
        {
            let staging = StagingArea::open(tmp.path().to_path_buf())
                .await
                .expect("open staging rw");
            stage_chunks_as_synthetic_file(&staging, chunks).await;
            staging.flush_pending().await.expect("flush");
            staging.close().await.expect("close");
        }
        let ro = StagingAreaReadOnly::open(tmp.path().to_path_buf())
            .await
            .expect("open ro");
        (ro, tmp)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn file_push_plan_is_loaded_from_index_without_json_cache() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let chunks = [make_chunk(90, 4096), make_chunk(91, 4096)];
        let total_bytes: u64 = chunks.iter().map(|(_, data)| data.len() as u64).sum();
        let chunk_pairs: Vec<(MerkleHash, u64)> = chunks
            .iter()
            .map(|(hash, data)| (*hash, data.len() as u64))
            .collect();

        let file_hash;
        {
            let staging = StagingArea::open(tmp.path().to_path_buf())
                .await
                .expect("open staging");
            file_hash = stage_chunks_as_synthetic_file(&staging, &chunks).await;
            staging.flush_pending().await.expect("flush");
            let plan =
                push_plan::FilePushPlan::new_verified_staging(file_hash, total_bytes, &chunk_pairs);
            staging
                .write_file_push_plan(&plan)
                .await
                .expect("write file push plan");
            assert!(
                !push_plan::file_plan_path(staging.root(), &file_hash).exists(),
                "indexed staging should not mirror push plans to JSON"
            );
            staging.close().await.expect("close staging");
        }

        let ro = StagingAreaReadOnly::open(tmp.path().to_path_buf())
            .await
            .expect("open readonly");
        let loaded = ro
            .load_file_push_plan(&file_hash)
            .await
            .expect("load DB push plan")
            .expect("push plan should exist in index");

        assert!(loaded.staged_chunk_sequence_verified);
        assert_eq!(loaded.file_hash().expect("file hash"), file_hash);
        assert_eq!(loaded.file_size, total_bytes);
        assert_eq!(loaded.chunk_pairs().expect("chunks"), chunk_pairs);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn push_plan_stats_reads_index_when_json_cache_is_missing() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let chunks = [make_chunk(104, 4096), make_chunk(105, 4096)];
        let total_bytes: u64 = chunks.iter().map(|(_, data)| data.len() as u64).sum();
        let chunk_pairs: Vec<(MerkleHash, u64)> = chunks
            .iter()
            .map(|(hash, data)| (*hash, data.len() as u64))
            .collect();

        let file_hash;
        {
            let staging = StagingArea::open(tmp.path().to_path_buf())
                .await
                .expect("open staging");
            file_hash = stage_chunks_as_synthetic_file(&staging, &chunks).await;
            staging.flush_pending().await.expect("flush");
            let plan =
                push_plan::FilePushPlan::new_verified_staging(file_hash, total_bytes, &chunk_pairs);
            staging
                .write_file_push_plan(&plan)
                .await
                .expect("write file push plan");
            staging.close().await.expect("close staging");
        }

        let ro = StagingAreaReadOnly::open(tmp.path().to_path_buf())
            .await
            .expect("open readonly");
        let stats = ro
            .push_plan_stats(push_plan::PushPlanSummaryOptions::default())
            .await
            .expect("summarize indexed push plans");

        assert_eq!(stats.plan_files, 1);
        assert_eq!(stats.invalid_plan_files, 0);
        assert_eq!(stats.planned_file_bytes, total_bytes);
        assert_eq!(stats.planned_chunks, chunk_pairs.len() as u64);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn push_plan_stats_verifies_indexed_prepared_xorb_payload() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let chunks = [make_chunk(106, 4096), make_chunk(107, 4096)];
        let chunk_pairs: Vec<(MerkleHash, u64)> = chunks
            .iter()
            .map(|(hash, data)| (*hash, data.len() as u64))
            .collect();

        let file_hash;
        let xorb_len;
        {
            let staging = StagingArea::open(tmp.path().to_path_buf())
                .await
                .expect("open staging");
            file_hash = stage_chunks_as_synthetic_file(&staging, &chunks).await;
            staging.flush_pending().await.expect("flush");
            let (xorb_bytes, xorb_hash, placements) = build_xorb_for_chunks(&chunks);
            xorb_len = xorb_bytes.len() as u64;
            push_plan::write_prepared_xorb(
                staging.root(),
                &file_hash,
                &xorb_hash,
                xorb_bytes.clone(),
            )
            .await
            .expect("write prepared xorb");
            let plan = prepared_plan_for_xorb(
                file_hash,
                &chunk_pairs,
                &xorb_bytes,
                xorb_hash,
                &placements,
            );
            staging
                .write_file_push_plan(&plan)
                .await
                .expect("write file push plan");
            staging.close().await.expect("close staging");
        }

        let ro = StagingAreaReadOnly::open(tmp.path().to_path_buf())
            .await
            .expect("open readonly");
        let stats = ro
            .push_plan_stats(push_plan::PushPlanSummaryOptions {
                verify_prepared_xorbs: true,
            })
            .await
            .expect("summarize indexed push plans");

        assert!(stats.verified_prepared_xorbs);
        assert_eq!(stats.plan_files, 1);
        assert_eq!(stats.prepared_xorbs, 1);
        assert_eq!(stats.indexed_prepared_xorbs, 1);
        assert_eq!(stats.orphaned_indexed_prepared_xorbs, 0);
        assert_eq!(stats.invalid_indexed_prepared_xorbs, 0);
        assert_eq!(stats.verified_prepared_xorb_files, 1);
        assert_eq!(stats.verified_prepared_xorb_file_bytes, xorb_len);
        assert_eq!(stats.payload_hash_mismatched_prepared_xorb_files, 0);
        assert_eq!(stats.corrupt_prepared_xorb_files, 0);
        assert_eq!(stats.metadata_mismatched_prepared_xorb_files, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn push_plan_stats_reports_orphaned_indexed_prepared_xorb_rows() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let chunks = [make_chunk(116, 4096), make_chunk(117, 4096)];
        let chunk_pairs: Vec<(MerkleHash, u64)> = chunks
            .iter()
            .map(|(hash, data)| (*hash, data.len() as u64))
            .collect();

        let file_hash;
        {
            let staging = StagingArea::open(tmp.path().to_path_buf())
                .await
                .expect("open staging");
            file_hash = stage_chunks_as_synthetic_file(&staging, &chunks).await;
            staging.flush_pending().await.expect("flush");
            let (xorb_bytes, xorb_hash, placements) = build_xorb_for_chunks(&chunks);
            push_plan::write_prepared_xorb(
                staging.root(),
                &file_hash,
                &xorb_hash,
                xorb_bytes.clone(),
            )
            .await
            .expect("write prepared xorb");
            let plan = prepared_plan_for_xorb(
                file_hash,
                &chunk_pairs,
                &xorb_bytes,
                xorb_hash,
                &placements,
            );
            staging
                .write_file_push_plan(&plan)
                .await
                .expect("write file push plan");
            staging.close().await.expect("close staging");
        }

        let extra_payload = Bytes::from_static(b"extra indexed prepared row");
        let extra_plan = prepared_plan_for_first_chunk(
            file_hash,
            chunk_pairs.iter().map(|(_, size)| *size).sum(),
            &chunk_pairs,
            MerkleHash::from([0xB3; 32]),
            &extra_payload,
        );
        let extra_planned_xorb = extra_plan
            .prepared_xorbs
            .first()
            .expect("extra prepared xorb");
        insert_extra_prepared_xorb_row(tmp.path(), file_hash, extra_planned_xorb);

        let ro = StagingAreaReadOnly::open(tmp.path().to_path_buf())
            .await
            .expect("open readonly");
        let stats = ro
            .push_plan_stats(push_plan::PushPlanSummaryOptions::default())
            .await
            .expect("summarize indexed push plans");

        assert_eq!(stats.plan_files, 1);
        assert_eq!(stats.prepared_xorbs, 1);
        assert_eq!(stats.indexed_prepared_xorbs, 2);
        assert_eq!(stats.orphaned_indexed_prepared_xorbs, 1);
        assert_eq!(stats.invalid_indexed_prepared_xorbs, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn push_plan_stats_reports_invalid_indexed_prepared_xorb_key_rows() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let chunks = [make_chunk(118, 4096), make_chunk(119, 4096)];
        let total_bytes: u64 = chunks.iter().map(|(_, data)| data.len() as u64).sum();
        let chunk_pairs: Vec<(MerkleHash, u64)> = chunks
            .iter()
            .map(|(hash, data)| (*hash, data.len() as u64))
            .collect();

        let file_hash;
        {
            let staging = StagingArea::open(tmp.path().to_path_buf())
                .await
                .expect("open staging");
            file_hash = stage_chunks_as_synthetic_file(&staging, &chunks).await;
            staging.flush_pending().await.expect("flush");
            let plan =
                push_plan::FilePushPlan::new_verified_staging(file_hash, total_bytes, &chunk_pairs);
            staging
                .write_file_push_plan(&plan)
                .await
                .expect("write file push plan");
            staging.close().await.expect("close staging");
        }

        insert_invalid_prepared_xorb_key_row(tmp.path(), file_hash);

        let ro = StagingAreaReadOnly::open(tmp.path().to_path_buf())
            .await
            .expect("open readonly");
        let stats = ro
            .push_plan_stats(push_plan::PushPlanSummaryOptions::default())
            .await
            .expect("summarize indexed push plans");

        assert_eq!(stats.plan_files, 1);
        assert_eq!(stats.prepared_xorbs, 0);
        assert_eq!(stats.indexed_prepared_xorbs, 1);
        assert_eq!(stats.orphaned_indexed_prepared_xorbs, 0);
        assert_eq!(stats.invalid_indexed_prepared_xorbs, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn push_plan_stats_reports_indexed_prepared_xorb_metadata_mismatch() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let chunks = [make_chunk(120, 4096), make_chunk(121, 4096)];
        let chunk_pairs: Vec<(MerkleHash, u64)> = chunks
            .iter()
            .map(|(hash, data)| (*hash, data.len() as u64))
            .collect();

        let file_hash;
        let xorb_hash;
        {
            let staging = StagingArea::open(tmp.path().to_path_buf())
                .await
                .expect("open staging");
            file_hash = stage_chunks_as_synthetic_file(&staging, &chunks).await;
            staging.flush_pending().await.expect("flush");
            let (xorb_bytes, built_xorb_hash, placements) = build_xorb_for_chunks(&chunks);
            xorb_hash = built_xorb_hash;
            push_plan::write_prepared_xorb(
                staging.root(),
                &file_hash,
                &xorb_hash,
                xorb_bytes.clone(),
            )
            .await
            .expect("write prepared xorb");
            let plan = prepared_plan_for_xorb(
                file_hash,
                &chunk_pairs,
                &xorb_bytes,
                xorb_hash,
                &placements,
            );
            staging
                .write_file_push_plan(&plan)
                .await
                .expect("write file push plan");
            staging.close().await.expect("close staging");
        }

        corrupt_indexed_prepared_xorb_row_bytes(tmp.path(), file_hash, xorb_hash);

        let ro = StagingAreaReadOnly::open(tmp.path().to_path_buf())
            .await
            .expect("open readonly");
        let stats = ro
            .push_plan_stats(push_plan::PushPlanSummaryOptions::default())
            .await
            .expect("summarize indexed push plans");

        assert_eq!(stats.plan_files, 1);
        assert_eq!(stats.prepared_xorbs, 1);
        assert_eq!(stats.indexed_prepared_xorbs, 1);
        assert_eq!(stats.orphaned_indexed_prepared_xorbs, 0);
        assert_eq!(stats.invalid_indexed_prepared_xorbs, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn indexed_file_push_plan_rejects_stale_staged_rows() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let chunks = [make_chunk(98, 4096), make_chunk(99, 4096)];
        let total_bytes: u64 = chunks.iter().map(|(_, data)| data.len() as u64).sum();
        let chunk_pairs: Vec<(MerkleHash, u64)> = chunks
            .iter()
            .map(|(hash, data)| (*hash, data.len() as u64))
            .collect();

        let file_hash;
        {
            let staging = StagingArea::open(tmp.path().to_path_buf())
                .await
                .expect("open staging");
            file_hash = stage_chunks_as_synthetic_file(&staging, &chunks).await;
            staging.flush_pending().await.expect("flush");
            let plan =
                push_plan::FilePushPlan::new_verified_staging(file_hash, total_bytes, &chunk_pairs);
            staging
                .write_file_push_plan(&plan)
                .await
                .expect("write indexed plan");
            staging.close().await.expect("close staging");
        }

        corrupt_first_chunk_size(tmp.path(), &file_hash);

        let ro = StagingAreaReadOnly::open(tmp.path().to_path_buf())
            .await
            .expect("open readonly");
        let error = ro
            .load_file_push_plan(&file_hash)
            .await
            .expect_err("stale indexed plan should be rejected");
        assert_stale_plan_error(error);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn file_push_plan_rejects_prepared_xorb_without_file_coverage() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let chunks = [make_chunk(112, 4096), make_chunk(113, 4096)];
        let total_bytes: u64 = chunks.iter().map(|(_, data)| data.len() as u64).sum();
        let chunk_pairs: Vec<(MerkleHash, u64)> = chunks
            .iter()
            .map(|(hash, data)| (*hash, data.len() as u64))
            .collect();

        let staging = StagingArea::open(tmp.path().to_path_buf())
            .await
            .expect("open staging");
        let file_hash = stage_chunks_as_synthetic_file(&staging, &chunks).await;
        staging.flush_pending().await.expect("flush");
        let xorb_hash = MerkleHash::from([0xB1; 32]);
        let foreign_chunk = compute_data_hash(b"not part of this file");
        let mut plan =
            push_plan::FilePushPlan::new_verified_staging(file_hash, total_bytes, &chunk_pairs);
        plan.prepared_xorbs.push(push_plan::PlannedXorb {
            hash: xorb_hash.hex(),
            payload_hash: blake3::hash(b"unused payload").to_hex().to_string(),
            bytes: 14,
            upload: true,
            placements: vec![push_plan::PlannedPlacement {
                chunk_hash: foreign_chunk.hex(),
                xorb_hash: xorb_hash.hex(),
                chunk_index: 0,
                uncompressed_size: 14,
            }],
        });

        let error = staging
            .write_file_push_plan(&plan)
            .await
            .expect_err("plan with no file coverage should fail");
        assert_prepared_xorb_without_file_coverage_error(error);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn indexed_file_push_plan_load_rejects_prepared_xorb_without_file_coverage() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let chunks = [make_chunk(114, 4096), make_chunk(115, 4096)];
        let total_bytes: u64 = chunks.iter().map(|(_, data)| data.len() as u64).sum();
        let chunk_pairs: Vec<(MerkleHash, u64)> = chunks
            .iter()
            .map(|(hash, data)| (*hash, data.len() as u64))
            .collect();

        let file_hash;
        let mut corrupt_plan;
        {
            let staging = StagingArea::open(tmp.path().to_path_buf())
                .await
                .expect("open staging");
            file_hash = stage_chunks_as_synthetic_file(&staging, &chunks).await;
            staging.flush_pending().await.expect("flush");
            let plan =
                push_plan::FilePushPlan::new_verified_staging(file_hash, total_bytes, &chunk_pairs);
            staging
                .write_file_push_plan(&plan)
                .await
                .expect("write valid plan");
            corrupt_plan = plan;
            staging.close().await.expect("close staging");
        }

        let xorb_hash = MerkleHash::from([0xB2; 32]);
        let foreign_chunk = compute_data_hash(b"raw corrupt outside file chunk");
        corrupt_plan.prepared_xorbs.push(push_plan::PlannedXorb {
            hash: xorb_hash.hex(),
            payload_hash: blake3::hash(b"unused payload").to_hex().to_string(),
            bytes: 14,
            upload: true,
            placements: vec![push_plan::PlannedPlacement {
                chunk_hash: foreign_chunk.hex(),
                xorb_hash: xorb_hash.hex(),
                chunk_index: 0,
                uncompressed_size: 14,
            }],
        });
        replace_indexed_file_push_plan_body(tmp.path(), &file_hash, &corrupt_plan);

        let ro = StagingAreaReadOnly::open(tmp.path().to_path_buf())
            .await
            .expect("open readonly");
        let error = ro
            .load_file_push_plan(&file_hash)
            .await
            .expect_err("corrupt indexed plan body should be rejected");
        assert_prepared_xorb_without_file_coverage_error(error);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn prepared_cache_ignores_candidates_from_stale_source_plan() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let chunks = [make_chunk(108, 4096), make_chunk(109, 4096)];
        let chunk_pairs: Vec<(MerkleHash, u64)> = chunks
            .iter()
            .map(|(hash, data)| (*hash, data.len() as u64))
            .collect();
        let wanted_chunks = chunk_pairs
            .iter()
            .map(|(hash, _)| *hash)
            .collect::<HashSet<_>>();

        let file_hash;
        {
            let staging = StagingArea::open(tmp.path().to_path_buf())
                .await
                .expect("open staging");
            file_hash = stage_chunks_as_synthetic_file(&staging, &chunks).await;
            staging.flush_pending().await.expect("flush");
            let (xorb_bytes, xorb_hash, placements) = build_xorb_for_chunks(&chunks);
            push_plan::write_prepared_xorb(
                staging.root(),
                &file_hash,
                &xorb_hash,
                xorb_bytes.clone(),
            )
            .await
            .expect("write prepared xorb");
            let plan = prepared_plan_for_xorb(
                file_hash,
                &chunk_pairs,
                &xorb_bytes,
                xorb_hash,
                &placements,
            );
            staging
                .write_file_push_plan(&plan)
                .await
                .expect("write file push plan");
            assert!(
                !staging
                    .load_prepared_xorb_cache_for_chunks(&wanted_chunks)
                    .expect("load prepared cache")
                    .is_empty()
            );
            staging.close().await.expect("close staging");
        }

        corrupt_first_chunk_size(tmp.path(), &file_hash);

        let staging = StagingArea::open(tmp.path().to_path_buf())
            .await
            .expect("reopen staging");
        assert!(
            staging
                .load_prepared_xorb_cache_for_chunks(&wanted_chunks)
                .expect("load stale-source prepared cache")
                .is_empty(),
            "prepared xorb candidates from stale source plans should not be reused"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn prepared_cache_ignores_candidate_missing_from_source_plan_body() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let chunks = [make_chunk(110, 4096), make_chunk(111, 4096)];
        let total_bytes: u64 = chunks.iter().map(|(_, data)| data.len() as u64).sum();
        let chunk_pairs: Vec<(MerkleHash, u64)> = chunks
            .iter()
            .map(|(hash, data)| (*hash, data.len() as u64))
            .collect();
        let wanted_chunks = chunk_pairs
            .iter()
            .map(|(hash, _)| *hash)
            .collect::<HashSet<_>>();

        let file_hash;
        let extra_planned_xorb;
        {
            let staging = StagingArea::open(tmp.path().to_path_buf())
                .await
                .expect("open staging");
            file_hash = stage_chunks_as_synthetic_file(&staging, &chunks).await;
            staging.flush_pending().await.expect("flush");
            let (xorb_bytes, xorb_hash, placements) = build_xorb_for_chunks(&chunks);
            push_plan::write_prepared_xorb(
                staging.root(),
                &file_hash,
                &xorb_hash,
                xorb_bytes.clone(),
            )
            .await
            .expect("write prepared xorb");
            let mut extra_plan = prepared_plan_for_xorb(
                file_hash,
                &chunk_pairs,
                &xorb_bytes,
                xorb_hash,
                &placements,
            );
            extra_planned_xorb = extra_plan
                .prepared_xorbs
                .pop()
                .expect("extra prepared xorb");
            let authoritative_plan =
                push_plan::FilePushPlan::new_verified_staging(file_hash, total_bytes, &chunk_pairs);
            staging
                .write_file_push_plan(&authoritative_plan)
                .await
                .expect("write authoritative plan without prepared xorb");
            staging.close().await.expect("close staging");
        }

        insert_extra_prepared_xorb_row(tmp.path(), file_hash, &extra_planned_xorb);

        let staging = StagingArea::open(tmp.path().to_path_buf())
            .await
            .expect("reopen staging");
        assert!(
            staging
                .load_prepared_xorb_cache_for_chunks(&wanted_chunks)
                .expect("load extra-row prepared cache")
                .is_empty(),
            "prepared xorb rows not present in the source plan body should not be reused"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn json_only_file_push_plan_is_not_authoritative() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let chunks = [make_chunk(100, 4096), make_chunk(101, 4096)];
        let total_bytes: u64 = chunks.iter().map(|(_, data)| data.len() as u64).sum();
        let chunk_pairs: Vec<(MerkleHash, u64)> = chunks
            .iter()
            .map(|(hash, data)| (*hash, data.len() as u64))
            .collect();

        let file_hash;
        {
            let staging = StagingArea::open(tmp.path().to_path_buf())
                .await
                .expect("open staging");
            file_hash = stage_chunks_as_synthetic_file(&staging, &chunks).await;
            staging.flush_pending().await.expect("flush");
            let plan =
                push_plan::FilePushPlan::new_verified_staging(file_hash, total_bytes, &chunk_pairs);
            push_plan::write_file_push_plan(staging.root(), &plan)
                .await
                .expect("write JSON-only plan");
            staging.close().await.expect("close staging");
        }

        let ro = StagingAreaReadOnly::open(tmp.path().to_path_buf())
            .await
            .expect("open readonly");
        let loaded = ro
            .load_file_push_plan(&file_hash)
            .await
            .expect("load indexed plan");
        assert!(
            loaded.is_none(),
            "JSON-only plans are debug artifacts, not staging authority"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn replacing_file_push_plan_prunes_stale_prepared_xorb_payloads() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let chunks = [make_chunk(102, 4096), make_chunk(103, 4096)];
        let total_bytes: u64 = chunks.iter().map(|(_, data)| data.len() as u64).sum();
        let chunk_pairs: Vec<(MerkleHash, u64)> = chunks
            .iter()
            .map(|(hash, data)| (*hash, data.len() as u64))
            .collect();
        let staging = StagingArea::open(tmp.path().to_path_buf())
            .await
            .expect("open staging");
        let file_hash = stage_chunks_as_synthetic_file(&staging, &chunks).await;
        staging.flush_pending().await.expect("flush");

        let first_xorb = MerkleHash::from([0xA1; 32]);
        let first_payload = Bytes::from_static(b"first prepared payload");
        push_plan::write_prepared_xorb(
            staging.root(),
            &file_hash,
            &first_xorb,
            first_payload.clone(),
        )
        .await
        .expect("write first prepared xorb");
        let first_plan = prepared_plan_for_first_chunk(
            file_hash,
            total_bytes,
            &chunk_pairs,
            first_xorb,
            &first_payload,
        );
        staging
            .write_file_push_plan(&first_plan)
            .await
            .expect("write first push plan");
        assert!(push_plan::prepared_xorb_path(staging.root(), &file_hash, &first_xorb).exists());

        let second_xorb = MerkleHash::from([0xA2; 32]);
        let second_payload = Bytes::from_static(b"second prepared payload");
        push_plan::write_prepared_xorb(
            staging.root(),
            &file_hash,
            &second_xorb,
            second_payload.clone(),
        )
        .await
        .expect("write second prepared xorb");
        let second_plan = prepared_plan_for_first_chunk(
            file_hash,
            total_bytes,
            &chunk_pairs,
            second_xorb,
            &second_payload,
        );
        staging
            .write_file_push_plan(&second_plan)
            .await
            .expect("replace push plan");

        assert!(!push_plan::prepared_xorb_path(staging.root(), &file_hash, &first_xorb).exists());
        assert!(push_plan::prepared_xorb_path(staging.root(), &file_hash, &second_xorb).exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn malformed_push_plan_sidecar_does_not_block_indexed_plan_lifecycle() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let chunks = [make_chunk(96, 4096), make_chunk(97, 4096)];
        let total_bytes: u64 = chunks.iter().map(|(_, data)| data.len() as u64).sum();
        let chunk_pairs: Vec<(MerkleHash, u64)> = chunks
            .iter()
            .map(|(hash, data)| (*hash, data.len() as u64))
            .collect();

        let staging = StagingArea::open(tmp.path().to_path_buf())
            .await
            .expect("open staging");
        let file_hash = stage_chunks_as_synthetic_file(&staging, &chunks).await;
        staging.flush_pending().await.expect("flush");
        std::fs::write(staging.root().join("push-plans"), b"not a directory")
            .expect("create stale push-plan path");

        let plan =
            push_plan::FilePushPlan::new_verified_staging(file_hash, total_bytes, &chunk_pairs);
        staging
            .write_file_push_plan(&plan)
            .await
            .expect("write indexed push plan");

        let loaded = staging
            .load_file_push_plan(&file_hash)
            .await
            .expect("load indexed plan")
            .expect("indexed plan exists");
        assert_eq!(loaded.chunk_pairs().expect("chunks"), chunk_pairs);

        let wanted_chunks = chunk_pairs
            .iter()
            .map(|(hash, _)| *hash)
            .collect::<HashSet<_>>();
        assert!(
            staging
                .load_prepared_xorb_cache_for_chunks(&wanted_chunks)
                .expect("load indexed prepared cache")
                .is_empty()
        );

        let retired = staging.retire_file(&file_hash).expect("retire file");
        assert_eq!(retired.rows_deleted, chunks.len() as u64);
        assert!(
            staging
                .load_file_push_plan(&file_hash)
                .await
                .expect("load retired plan")
                .is_none()
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unregister_file_removes_indexed_push_plan() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let chunks = [make_chunk(92, 4096), make_chunk(93, 4096)];
        let total_bytes: u64 = chunks.iter().map(|(_, data)| data.len() as u64).sum();
        let chunk_pairs: Vec<(MerkleHash, u64)> = chunks
            .iter()
            .map(|(hash, data)| (*hash, data.len() as u64))
            .collect();
        let staging = StagingArea::open(tmp.path().to_path_buf())
            .await
            .expect("open staging");
        let file_hash = stage_chunks_as_synthetic_file(&staging, &chunks).await;
        staging.flush_pending().await.expect("flush");
        let prepared_xorb = MerkleHash::from([0xA3; 32]);
        let prepared_payload = Bytes::from_static(b"unregister prepared payload");
        push_plan::write_prepared_xorb(
            staging.root(),
            &file_hash,
            &prepared_xorb,
            prepared_payload.clone(),
        )
        .await
        .expect("write prepared xorb");
        let prepared_path =
            push_plan::prepared_xorb_path(staging.root(), &file_hash, &prepared_xorb);
        let plan = prepared_plan_for_first_chunk(
            file_hash,
            total_bytes,
            &chunk_pairs,
            prepared_xorb,
            &prepared_payload,
        );
        staging
            .write_file_push_plan(&plan)
            .await
            .expect("write file push plan");
        assert!(prepared_path.exists());

        assert!(staging.unregister_file(&file_hash).expect("unregister"));

        assert!(
            staging
                .load_file_push_plan(&file_hash)
                .await
                .expect("load removed plan")
                .is_none()
        );
        assert!(!prepared_path.exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn adopt_staged_file_removes_stale_push_plans() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let chunks = [make_chunk(94, 4096), make_chunk(95, 4096)];
        let total_bytes: u64 = chunks.iter().map(|(_, data)| data.len() as u64).sum();
        let chunk_pairs: Vec<(MerkleHash, u64)> = chunks
            .iter()
            .map(|(hash, data)| (*hash, data.len() as u64))
            .collect();
        let staging = StagingArea::open(tmp.path().to_path_buf())
            .await
            .expect("open staging");
        let source_hash = stage_chunks_as_synthetic_file(&staging, &chunks).await;
        staging.flush_pending().await.expect("flush");
        let source_xorb = MerkleHash::from([0xA4; 32]);
        let source_payload = Bytes::from_static(b"adopt source prepared payload");
        push_plan::write_prepared_xorb(
            staging.root(),
            &source_hash,
            &source_xorb,
            source_payload.clone(),
        )
        .await
        .expect("write source prepared xorb");
        let plan = prepared_plan_for_first_chunk(
            source_hash,
            total_bytes,
            &chunk_pairs,
            source_xorb,
            &source_payload,
        );
        staging
            .write_file_push_plan(&plan)
            .await
            .expect("write file push plan");
        let source_prepared_path =
            push_plan::prepared_xorb_path(staging.root(), &source_hash, &source_xorb);

        let target_hash = compute_data_hash(b"adopted-final-file-hash");
        let target_xorb = MerkleHash::from([0xA5; 32]);
        let target_payload = Bytes::from_static(b"stale target prepared payload");
        push_plan::write_prepared_xorb(staging.root(), &target_hash, &target_xorb, target_payload)
            .await
            .expect("write target stale prepared xorb");
        let target_prepared_path =
            push_plan::prepared_xorb_path(staging.root(), &target_hash, &target_xorb);
        assert!(source_prepared_path.exists());
        assert!(target_prepared_path.exists());

        let adopted = staging
            .adopt_staged_file(&source_hash, &target_hash, total_bytes)
            .expect("adopt staged file");

        assert_eq!(adopted, chunks.len() as u64);
        assert!(
            staging
                .load_file_push_plan(&source_hash)
                .await
                .expect("load source plan")
                .is_none()
        );
        assert_eq!(
            staging
                .chunks_for_file(&target_hash)
                .expect("target chunks"),
            chunk_pairs
                .iter()
                .map(|(hash, _)| *hash)
                .collect::<Vec<_>>()
        );
        assert!(!source_prepared_path.exists());
        assert!(!target_prepared_path.exists());
    }

    #[tokio::test]
    async fn flush_pending_skips_when_no_bytes_are_pending() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let metrics = Arc::new(CountingMetrics::default());
        let mut staging = StagingArea::open(tmp.path().to_path_buf())
            .await
            .expect("open");
        staging.set_metrics(Arc::clone(&metrics));

        let chunk = make_chunk(1, 4096);
        stage_chunks_as_synthetic_file(&staging, &[chunk]).await;
        staging.flush_pending().await.expect("first flush");
        staging.flush_pending().await.expect("second flush");
        staging.close().await.expect("close");

        assert_eq!(
            metrics.fsyncs(),
            1,
            "only the first flush should sync newly appended bytes"
        );
    }

    #[tokio::test]
    async fn verify_checks_staged_file_layout_and_payloads() {
        let chunks = vec![make_chunk(1, 1024), make_chunk(2, 2048)];
        let (ro, _tmp) = populate_and_reopen(&chunks).await;

        let summary = ro.verify().await.expect("verify");

        assert_eq!(summary.files_checked, 1);
        assert_eq!(summary.chunk_refs_checked, 2);
        assert_eq!(summary.unique_chunks_checked, 2);
        assert_eq!(summary.bytes_checked, 3072);
    }

    #[tokio::test]
    async fn verify_rejects_file_size_mismatch() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().to_path_buf();
        let (chunk_hash, chunk_data) = make_chunk(3, 1024);
        let file_hash = compute_data_hash(b"size-mismatch-file");

        {
            let staging = StagingArea::open(root.clone()).await.expect("open rw");
            staging
                .pre_register_file(&file_hash, chunk_data.len() as u64)
                .expect("pre-register");
            staging
                .stage_chunks_batch(&[(&chunk_hash, chunk_data.as_slice())], &file_hash, 0)
                .await
                .expect("stage");
            staging.flush_pending().await.expect("flush");
            staging.close().await.expect("close");
        }

        let index = Index::open(&root.join("index.db")).expect("open index");
        let file_hash_bytes: [u8; 32] = file_hash.into();
        index
            .connection()
            .execute(
                "UPDATE files SET total_bytes = ?1 WHERE file_hash = ?2",
                rusqlite::params![2048_i64, file_hash_bytes.as_slice()],
            )
            .expect("corrupt file size");
        drop(index);

        let ro = StagingAreaReadOnly::open(root).await.expect("open ro");
        let err = ro.verify().await.expect_err("verify should fail");

        assert!(matches!(err, StagingError::StagingCorrupt(_)));
        assert!(err.to_string().contains("staged chunks total 1024 bytes"));
        assert!(err.to_string().contains("expected 2048"));
    }

    #[tokio::test]
    async fn verify_rejects_wrong_file_reconstruction_order() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().to_path_buf();
        let (first_hash, first_data) = make_chunk(4, 1024);
        let (second_hash, second_data) = make_chunk(5, 2048);

        let mut hasher = blake3::Hasher::new();
        hasher.update(&first_data);
        hasher.update(&second_data);
        let file_hash = MerkleHash::from(*hasher.finalize().as_bytes());
        let total_bytes = (first_data.len() + second_data.len()) as u64;

        {
            let staging = StagingArea::open(root.clone()).await.expect("open rw");
            staging
                .pre_register_file(&file_hash, total_bytes)
                .expect("pre-register");
            staging
                .stage_chunks_batch(
                    &[
                        (&second_hash, second_data.as_slice()),
                        (&first_hash, first_data.as_slice()),
                    ],
                    &file_hash,
                    0,
                )
                .await
                .expect("stage wrong order");
            staging.flush_pending().await.expect("flush");
            staging.close().await.expect("close");
        }

        let ro = StagingAreaReadOnly::open(root).await.expect("open ro");
        let err = ro.verify().await.expect_err("verify should fail");

        assert!(matches!(err, StagingError::StagingCorrupt(_)));
        assert!(
            err.to_string().contains("reconstruct to"),
            "unexpected error: {err}"
        );
    }

    #[tokio::test]
    async fn retirement_marker_blocks_new_push_markers() {
        let tmp = tempfile::tempdir().expect("tempdir");
        {
            let staging = StagingArea::open(tmp.path().to_path_buf())
                .await
                .expect("open staging rw");
            staging.close().await.expect("close staging");
        }
        let ro = StagingAreaReadOnly::open(tmp.path().to_path_buf())
            .await
            .expect("open ro");

        ro.mark_push_inflight("active-push")
            .expect("mark active push");
        let guard = ro
            .begin_retirement("active-push")
            .expect("begin retirement")
            .expect("no other push should block retirement");

        let err = ro
            .mark_push_inflight("new-push")
            .expect_err("new push must not mark while retirement is active");
        assert!(matches!(err, StagingError::StagingLocked { .. }));

        let inflight = ro.list_inflight().expect("list inflight");
        assert!(inflight.iter().any(|id| id == "active-push"));
        assert!(
            inflight.iter().any(|id| id == "retire-active-push"),
            "retirement guard marker should be visible while active"
        );
        assert!(
            inflight.iter().all(|id| id != "new-push"),
            "failed mark must clean up its own marker"
        );

        drop(guard);
        let after_drop = ro.list_inflight().expect("list after drop");
        assert!(
            after_drop.iter().all(|id| id != "retire-active-push"),
            "dropping the guard should clear the retirement marker"
        );
        ro.clear_push_inflight("active-push")
            .expect("clear active marker");
    }

    #[tokio::test]
    async fn retirement_marker_does_not_block_another_retirement() {
        let tmp = tempfile::tempdir().expect("tempdir");
        {
            let staging = StagingArea::open(tmp.path().to_path_buf())
                .await
                .expect("open staging rw");
            staging.close().await.expect("close staging");
        }
        let ro = StagingAreaReadOnly::open(tmp.path().to_path_buf())
            .await
            .expect("open ro");

        let first = ro
            .begin_retirement("first-push")
            .expect("begin first retirement")
            .expect("first retirement should start");
        let second = ro
            .begin_retirement("second-push")
            .expect("begin second retirement")
            .expect("retirement markers must not pin cleanup");

        let err = ro
            .mark_push_inflight("new-push")
            .expect_err("retirement must still block new push readers");
        assert!(matches!(err, StagingError::StagingLocked { .. }));

        drop(first);
        drop(second);
        assert!(
            ro.list_inflight().expect("list inflight").is_empty(),
            "retirement guards should clear their marker files"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stale_retirement_marker_from_dead_pid_does_not_block_new_push() {
        let tmp = tempfile::tempdir().expect("tempdir");
        {
            let staging = StagingArea::open(tmp.path().to_path_buf())
                .await
                .expect("open staging rw");
            staging.close().await.expect("close staging");
        }
        let ro = StagingAreaReadOnly::open(tmp.path().to_path_buf())
            .await
            .expect("open ro");

        let stale_id = "retire-crashed-push";
        std::fs::write(
            inflight_marker_path(tmp.path(), stale_id),
            "pid=999999\nid=retire-crashed-push\n",
        )
        .expect("write stale marker");

        ro.mark_push_inflight("new-push")
            .expect("stale retirement marker should be pruned");

        let inflight = ro.list_inflight().expect("list inflight");
        assert!(inflight.iter().any(|id| id == "new-push"));
        assert!(
            inflight.iter().all(|id| id != stale_id),
            "dead retirement marker should be removed before accepting a new push"
        );
        ro.clear_push_inflight("new-push")
            .expect("clear new marker");
    }

    #[tokio::test]
    async fn malformed_retirement_marker_does_not_block_new_push() {
        let tmp = tempfile::tempdir().expect("tempdir");
        {
            let staging = StagingArea::open(tmp.path().to_path_buf())
                .await
                .expect("open staging rw");
            staging.close().await.expect("close staging");
        }
        let ro = StagingAreaReadOnly::open(tmp.path().to_path_buf())
            .await
            .expect("open ro");

        let stale_id = "retire-crashed-push";
        std::fs::write(
            inflight_marker_path(tmp.path(), stale_id),
            "id=retire-crashed-push\n",
        )
        .expect("write malformed marker");

        ro.mark_push_inflight("new-push")
            .expect("malformed retirement marker should be pruned");

        let inflight = ro.list_inflight().expect("list inflight");
        assert!(inflight.iter().any(|id| id == "new-push"));
        assert!(
            inflight.iter().all(|id| id != stale_id),
            "malformed retirement marker should be removed before accepting a new push"
        );
        ro.clear_push_inflight("new-push")
            .expect("clear new marker");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn clean_completes_committed_snapshot_retirement_before_removing_journal() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let file_path = tmp.path().join("large.bin");
        std::fs::write(&file_path, vec![0x5a; 2 * 1024 * 1024]).expect("write fixture");
        let staging = StagingArea::open(tmp.path().join("staging"))
            .await
            .expect("open staging");
        let staged = crate::stream::stage_file_streaming(
            &file_path,
            tmp.path(),
            &staging,
            crate::stream::StreamStageProgress::default(),
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("stage file");
        staging
            .mark_batch_published(&staged.batch_id)
            .expect("publish batch");
        {
            let index = lock_index(&staging.index).expect("lock index");
            index
                .create_push_snapshot("committed-push", std::slice::from_ref(&staged.recipe))
                .expect("snapshot");
            index
                .commit_push_snapshot("committed-push")
                .expect("commit snapshot");
        }

        staging.clean().expect("clean staging");

        let file_hash = MerkleHash::from(staged.file_hash);
        assert!(
            staging
                .chunks_for_file(&file_hash)
                .expect("chunks")
                .is_empty()
        );
        let health = staging.lifecycle_health().expect("health");
        assert_eq!(health.committed_push_snapshots, 0);
        assert_eq!(health.path_leases, 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stale_push_marker_from_dead_pid_does_not_defer_retirement() {
        let tmp = tempfile::tempdir().expect("tempdir");
        {
            let staging = StagingArea::open(tmp.path().to_path_buf())
                .await
                .expect("open staging rw");
            staging.close().await.expect("close staging");
        }
        let ro = StagingAreaReadOnly::open(tmp.path().to_path_buf())
            .await
            .expect("open ro");

        let stale_id = "crashed-push";
        std::fs::write(
            inflight_marker_path(tmp.path(), stale_id),
            "pid=999999\nid=crashed-push\n",
        )
        .expect("write stale marker");

        let guard = ro
            .begin_retirement("new-push")
            .expect("begin retirement")
            .expect("dead push marker should not defer retirement");

        let inflight = ro.list_inflight().expect("list inflight");
        assert!(
            inflight.iter().all(|id| id != stale_id),
            "dead push marker should be removed before retirement checks sibling pushes"
        );
        assert!(inflight.iter().any(|id| id == "retire-new-push"));
        drop(guard);
    }

    #[tokio::test]
    async fn malformed_push_marker_does_not_defer_retirement() {
        let tmp = tempfile::tempdir().expect("tempdir");
        {
            let staging = StagingArea::open(tmp.path().to_path_buf())
                .await
                .expect("open staging rw");
            staging.close().await.expect("close staging");
        }
        let ro = StagingAreaReadOnly::open(tmp.path().to_path_buf())
            .await
            .expect("open ro");

        let stale_id = "crashed-push";
        std::fs::write(
            inflight_marker_path(tmp.path(), stale_id),
            "pid=123\nid=some-other-push\n",
        )
        .expect("write malformed marker");

        let guard = ro
            .begin_retirement("new-push")
            .expect("begin retirement")
            .expect("malformed push marker should not defer retirement");

        let inflight = ro.list_inflight().expect("list inflight");
        assert!(
            inflight.iter().all(|id| id != stale_id),
            "malformed push marker should be removed before retirement checks sibling pushes"
        );
        assert!(inflight.iter().any(|id| id == "retire-new-push"));
        drop(guard);
    }

    #[tokio::test]
    async fn get_chunks_batch_empty_input_returns_empty() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let staging = StagingArea::open(tmp.path().to_path_buf())
            .await
            .expect("open");
        staging.close().await.expect("close");
        let ro = StagingAreaReadOnly::open(tmp.path().to_path_buf())
            .await
            .expect("open ro");

        let result = ro.get_chunks_batch(&[]).await.expect("batch");
        assert!(result.is_empty());
    }

    #[tokio::test]
    async fn get_chunks_batch_returns_chunks_in_input_order() {
        let chunks: Vec<_> = (0..5u32).map(|i| make_chunk(i, 1024)).collect();
        let (ro, _tmp) = populate_and_reopen(&chunks).await;

        // Request in a shuffled order.
        let order = [3usize, 0, 4, 1, 2];
        let hashes: Vec<MerkleHash> = order.iter().map(|&i| chunks[i].0).collect();

        let result = ro.get_chunks_batch(&hashes).await.expect("batch");

        assert_eq!(result.len(), order.len());
        for (pos, &src_idx) in order.iter().enumerate() {
            assert_eq!(result[pos].0, chunks[src_idx].0);
            assert_eq!(result[pos].1.as_ref(), chunks[src_idx].1.as_slice());
        }
    }

    #[tokio::test]
    async fn get_chunks_batch_preserves_duplicate_input_positions() {
        let chunks: Vec<_> = (0..2u32).map(|i| make_chunk(i, 1024)).collect();
        let (ro, _tmp) = populate_and_reopen(&chunks).await;
        let hashes = vec![chunks[0].0, chunks[1].0, chunks[0].0];

        let result = ro.get_chunks_batch(&hashes).await.expect("batch");

        assert_eq!(result.len(), hashes.len());
        assert_eq!(result[0].0, chunks[0].0);
        assert_eq!(result[0].1.as_ref(), chunks[0].1.as_slice());
        assert_eq!(result[1].0, chunks[1].0);
        assert_eq!(result[1].1.as_ref(), chunks[1].1.as_slice());
        assert_eq!(result[2].0, chunks[0].0);
        assert_eq!(result[2].1.as_ref(), chunks[0].1.as_slice());
    }

    #[tokio::test]
    async fn get_chunks_batch_matches_get_chunk_per_hash() {
        let chunks: Vec<_> = (0..20u32)
            .map(|i| make_chunk(i, 2048 + i as usize))
            .collect();
        let (ro, _tmp) = populate_and_reopen(&chunks).await;

        let hashes: Vec<MerkleHash> = chunks.iter().map(|(h, _)| *h).collect();

        let per_chunk: Vec<Bytes> = {
            let mut v = Vec::with_capacity(hashes.len());
            for h in &hashes {
                v.push(ro.get_chunk(h).await.expect("get").expect("some"));
            }
            v
        };

        let batched = ro.get_chunks_batch(&hashes).await.expect("batch");

        assert_eq!(batched.len(), per_chunk.len());
        for (i, (h, data)) in batched.iter().enumerate() {
            assert_eq!(*h, hashes[i]);
            assert_eq!(data.as_ref(), per_chunk[i].as_ref());
        }
    }

    #[tokio::test]
    async fn get_chunks_batch_missing_chunk_returns_error() {
        let chunks: Vec<_> = (0..3u32).map(|i| make_chunk(i, 512)).collect();
        let (ro, _tmp) = populate_and_reopen(&chunks).await;

        // Invent a hash that isn't in staging.
        let (ghost, _) = make_chunk(9999, 512);
        let hashes = vec![chunks[0].0, ghost, chunks[1].0];

        let err = ro.get_chunks_batch(&hashes).await.expect_err("should fail");
        match err {
            StagingError::ChunkNotFound { hash } => {
                assert_eq!(hash, ghost.hex());
            }
            other => panic!("expected ChunkNotFound, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn get_chunks_batch_groups_reads_by_segment() {
        // Stage enough chunks to span multiple segments. The default
        // flush threshold is 8 MiB, but segments don't roll over until
        // the soft cap is hit. For coverage of the grouping code path
        // a single-segment run is sufficient — the HashMap<u64,_> path
        // is exercised regardless of how many unique segment_ids exist.
        let chunks: Vec<_> = (0..16u32).map(|i| make_chunk(i, 256 * 1024)).collect();
        let (ro, _tmp) = populate_and_reopen(&chunks).await;

        let hashes: Vec<MerkleHash> = chunks.iter().map(|(h, _)| *h).collect();
        let result = ro.get_chunks_batch(&hashes).await.expect("batch");

        assert_eq!(result.len(), chunks.len());
        for (i, (h, data)) in result.iter().enumerate() {
            assert_eq!(*h, chunks[i].0);
            assert_eq!(data.as_ref(), chunks[i].1.as_slice());
        }
    }

    /// Regression: concurrent `StagingArea::open` calls on the same
    /// root used to be fatal — the second caller bounced off
    /// `LOCK_NB` after a 3-second retry budget and returned
    /// `StagingLocked`, which then surfaced as
    /// `clean filter 'crab' failed` when multiple git invocations
    /// (status, add, IDE integrations) overlap. The blocking variant
    /// queues up instead of failing.
    #[tokio::test(flavor = "multi_thread")]
    async fn open_blocking_waits_for_writer_to_release() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().to_path_buf();

        // Pre-create the staging area so both opens hit the flock path.
        let first = StagingArea::open(root.clone()).await.expect("first open");

        // Second open should block, not immediately fail. We prove it
        // by releasing the first open after a short delay and asserting
        // the second open eventually succeeds.
        let root_clone = root.clone();
        let second_task = tokio::spawn(async move {
            let start = std::time::Instant::now();
            let sa = StagingArea::open_blocking(root_clone, std::time::Duration::from_secs(10))
                .await
                .expect("blocking open must eventually succeed");
            (sa, start.elapsed())
        });

        // Give the second task a moment to hit the flock and block.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Release the first lock.
        first.close().await.expect("close first");

        let (second, waited) = second_task.await.expect("second task must not panic");

        // Must have waited at least a short while (couldn't have
        // succeeded instantly — first was still holding the lock).
        assert!(
            waited >= std::time::Duration::from_millis(80),
            "second open completed too fast ({waited:?}); the blocking \
             path apparently didn't wait for the first holder"
        );

        second.close().await.expect("close second");
    }

    /// Counterpart: if the budget elapses before the holder releases,
    /// the blocking variant still surfaces `StagingLocked` — we never
    /// want it to wait forever, because a genuinely stuck holder
    /// should still be reported.
    #[tokio::test(flavor = "multi_thread")]
    async fn open_blocking_times_out_when_holder_never_releases() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().to_path_buf();

        let _first = StagingArea::open(root.clone()).await.expect("first open");

        let result = StagingArea::open_blocking(root, std::time::Duration::from_millis(200)).await;

        match result {
            Err(StagingError::StagingLocked { .. }) => {}
            Err(other) => panic!("expected StagingLocked, got {other:?}"),
            Ok(_) => panic!("blocking open must time out when holder never releases"),
        }
    }

    /// A stale PID in the lockfile is not enough to break the lock while
    /// the kernel flock is still held. Shared push readers do not update
    /// the PID text, so unlinking on PID alone can let `staging clean
    /// --force` run on a new lockfile while the push still reads segments.
    #[tokio::test(flavor = "multi_thread")]
    async fn open_blocking_does_not_unlink_live_holder_with_stale_pid() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().to_path_buf();

        let lock_path = root.join(LOCKFILE_NAME);
        let holder_fd = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .expect("open lockfile");
        assert!(
            holder_fd
                .try_lock_exclusive()
                .expect("acquire exclusive lock on holder fd"),
            "holder fd should acquire exclusive lock"
        );

        // Overwrite the PID with one guaranteed to be dead. PID 1 (init)
        // is always alive, so pick a high PID that no process holds.
        let dead_pid = 999_999u32;
        std::fs::write(&lock_path, dead_pid.to_string()).expect("write dead PID to lockfile");

        let start = std::time::Instant::now();
        let result = StagingArea::open_blocking(root, std::time::Duration::from_millis(300)).await;
        let elapsed = start.elapsed();

        match result {
            Err(StagingError::StagingLocked { holder_pid }) => {
                assert_eq!(holder_pid, Some(dead_pid));
                assert!(
                    elapsed >= std::time::Duration::from_millis(250),
                    "stale-PID live flock returned too fast ({elapsed:?}); lockfile may have been unlinked"
                );
            }
            Err(other) => panic!("expected StagingLocked, got {other:?}"),
            Ok(_) => panic!("must not acquire while a live holder keeps the flock"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn open_force_reuses_free_stale_pid_lockfile_without_double_locking() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().to_path_buf();

        let lock_path = root.join(LOCKFILE_NAME);
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .expect("open lockfile");

        let dead_pid = 999_999u32;
        std::fs::write(&lock_path, dead_pid.to_string()).expect("write dead PID to lockfile");

        let lock_file = force_break_stale_lock(&root)
            .expect("force check")
            .expect("free stale-PID lockfile should be reusable");
        let cfg = StagingConfig::default();
        cfg.validate().expect("valid config");
        let staging = StagingArea::open_with_acquired_lock(root.clone(), cfg, lock_file)
            .expect("open with recovered lock");

        let (hash, data) = make_chunk(88, 1024);
        stage_chunks_as_synthetic_file(&staging, &[(hash, data.clone())]).await;
        staging.flush_pending().await.expect("flush");
        let got = staging.get_chunk(&hash).await.expect("get").expect("chunk");
        assert_eq!(got.as_ref(), data.as_slice());

        staging.close().await.expect("close staging");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn open_force_does_not_bypass_live_shared_reader_with_stale_pid() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().to_path_buf();
        let (hash, data) = make_chunk(89, 1024);

        {
            let staging = StagingArea::open(root.clone()).await.expect("open staging");
            stage_chunks_as_synthetic_file(&staging, &[(hash, data.clone())]).await;
            staging.flush_pending().await.expect("flush");
            staging.close().await.expect("close staging");
        }

        let ro = StagingAreaReadOnly::open(root.clone())
            .await
            .expect("open shared reader");
        let lock_path = root.join(LOCKFILE_NAME);
        let dead_pid = 999_999u32;
        std::fs::write(&lock_path, dead_pid.to_string()).expect("write stale PID");

        match StagingArea::open_force(root.clone()).await {
            Err(StagingError::StagingLocked { holder_pid }) => {
                assert_eq!(holder_pid, Some(dead_pid));
            }
            Err(other) => panic!("expected StagingLocked, got {other:?}"),
            Ok(_) => panic!("force-open must not bypass a live shared staging reader"),
        }

        let got = ro.get_chunk(&hash).await.expect("get").expect("chunk");
        assert_eq!(got.as_ref(), data.as_slice());
    }

    /// Sanity for the conservative side of the self-heal: a lock held by
    /// a provably-alive process is NOT broken, even after retries. This
    /// guards against the self-heal incorrectly breaking a legitimate
    /// concurrent operation. Reuses the existing timeout test's setup.
    #[tokio::test(flavor = "multi_thread")]
    async fn open_blocking_does_not_break_alive_holder() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().to_path_buf();

        // Real open writes this process's PID and holds the flock.
        let _first = StagingArea::open(root.clone()).await.expect("first open");

        let start = std::time::Instant::now();
        let result = StagingArea::open_blocking(root, std::time::Duration::from_millis(300)).await;
        let elapsed = start.elapsed();

        match result {
            Err(StagingError::StagingLocked { .. }) => {
                // Must have waited roughly the full budget — the alive
                // holder was not broken, just polled until timeout.
                assert!(
                    elapsed >= std::time::Duration::from_millis(250),
                    "alive-holder open returned too fast ({elapsed:?}); self-heal may have broken a live holder"
                );
            }
            Err(other) => panic!("expected StagingLocked, got {other:?}"),
            Ok(_) => panic!("must not acquire while a live holder holds the lock"),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod batch_diagnostic_tests {
    use super::tests::{make_chunk_pub, stage_chunks_as_synthetic_file};
    use super::*;

    #[tokio::test]
    async fn diag_single_stage_and_ro_get() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (hash, data) = make_chunk_pub(42, 1024);
        {
            let staging = StagingArea::open(tmp.path().to_path_buf())
                .await
                .expect("open rw");
            stage_chunks_as_synthetic_file(&staging, &[(hash, data.clone())]).await;
            // Check RW get_chunk sees it.
            let rw = staging.get_chunk(&hash).await.expect("rw get");
            println!("rw get: {:?}", rw.is_some());
            staging.flush_pending().await.expect("flush");
            let rw2 = staging.get_chunk(&hash).await.expect("rw get 2");
            println!("rw get after flush: {:?}", rw2.is_some());
            staging.close().await.expect("close");
        }
        let ro = StagingAreaReadOnly::open(tmp.path().to_path_buf())
            .await
            .expect("open ro");
        let got = ro.get_chunk(&hash).await.expect("ro get");
        println!("ro get: {:?}", got.is_some());
        assert!(got.is_some(), "ro get_chunk returned None");
    }

    #[tokio::test]
    async fn diag_batch_single_hash() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let (hash, data) = make_chunk_pub(42, 1024);
        {
            let staging = StagingArea::open(tmp.path().to_path_buf())
                .await
                .expect("open rw");
            stage_chunks_as_synthetic_file(&staging, &[(hash, data)]).await;
            staging.flush_pending().await.expect("flush");
            staging.close().await.expect("close");
        }
        let ro = StagingAreaReadOnly::open(tmp.path().to_path_buf())
            .await
            .expect("open ro");

        // Call both single and batch paths.
        let single = ro.get_chunk(&hash).await.expect("single get");
        println!("single: {:?}", single.is_some());

        let batch_result = ro.get_chunks_batch(&[hash]).await;
        println!("batch result: {:?}", batch_result.is_ok());
        if let Err(ref e) = batch_result {
            println!("batch err: {e:?}");
        }

        assert!(batch_result.is_ok(), "batch should succeed");
    }

    #[tokio::test]
    async fn diag_batch_five_hashes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let chunks: Vec<_> = (0..5u32).map(|i| make_chunk_pub(i, 1024)).collect();
        // Print hashes.
        for (i, (h, _)) in chunks.iter().enumerate() {
            println!("chunk[{i}] hash = {}", h.hex());
        }
        {
            let staging = StagingArea::open(tmp.path().to_path_buf())
                .await
                .expect("open rw");
            stage_chunks_as_synthetic_file(&staging, &chunks).await;
            staging.flush_pending().await.expect("flush");
            staging.close().await.expect("close");
        }
        let ro = StagingAreaReadOnly::open(tmp.path().to_path_buf())
            .await
            .expect("open ro");

        // Single per chunk.
        for (i, (h, _)) in chunks.iter().enumerate() {
            let g = ro.get_chunk(h).await.expect("get");
            println!("single[{i}]: {:?}", g.is_some());
        }

        // Batch with all 5.
        let hashes: Vec<MerkleHash> = chunks.iter().map(|(h, _)| *h).collect();
        let batch_result = ro.get_chunks_batch(&hashes).await;
        println!("batch result ok: {:?}", batch_result.is_ok());
        if let Err(ref e) = batch_result {
            println!("batch err: {e}");
        }
    }

    /// Stage chunks for two files, retire file A via the read-only
    /// handle, verify only file A's rows are gone and the shared chunk
    /// is still reachable via file B.
    #[tokio::test]
    async fn retire_file_removes_only_target_file_rows_leaving_shared_chunks_reachable() {
        let tmp = tempfile::tempdir().expect("tempdir");

        // Three distinct chunks. File A references [c0, c1]; file B
        // references [c1, c2]. The shared chunk c1 exercises the
        // "dedup-matched chunk stays because another file references
        // it" invariant — `chunks` is keyed by
        // `(chunk_hash, file_hash, chunk_index)`, so deleting by
        // `file_hash = A` leaves B's row for c1 intact.
        let c0 = make_chunk_pub(0, 4096);
        let c1 = make_chunk_pub(1, 4096);
        let c2 = make_chunk_pub(2, 4096);

        let file_a = compute_data_hash(b"file-a-seed");
        let file_b = compute_data_hash(b"file-b-seed");
        let a_chunks = [&c0, &c1];
        let b_chunks = [&c1, &c2];

        {
            let staging = StagingArea::open(tmp.path().to_path_buf())
                .await
                .expect("open rw");

            let a_total: u64 = a_chunks.iter().map(|(_, d)| d.len() as u64).sum();
            let b_total: u64 = b_chunks.iter().map(|(_, d)| d.len() as u64).sum();
            staging
                .pre_register_file(&file_a, a_total)
                .expect("pre-register A");
            staging
                .pre_register_file(&file_b, b_total)
                .expect("pre-register B");

            let a_refs: Vec<(&MerkleHash, &[u8])> =
                a_chunks.iter().map(|(h, d)| (h, d.as_slice())).collect();
            let b_refs: Vec<(&MerkleHash, &[u8])> =
                b_chunks.iter().map(|(h, d)| (h, d.as_slice())).collect();
            staging
                .stage_chunks_batch(&a_refs, &file_a, 0)
                .await
                .expect("stage A");
            staging
                .stage_chunks_batch(&b_refs, &file_b, 0)
                .await
                .expect("stage B");
            staging.flush_pending().await.expect("flush");

            // Promote `pending_chunks` rows into `chunks` via the same
            // `register_file` call the clean filter makes once the file
            // hash is known. Without this, `retire_file` would see zero
            // rows because the DELETE targets only the committed `chunks`
            // table.
            let a_pairs: Vec<(MerkleHash, u64)> =
                a_chunks.iter().map(|(h, d)| (*h, d.len() as u64)).collect();
            let b_pairs: Vec<(MerkleHash, u64)> =
                b_chunks.iter().map(|(h, d)| (*h, d.len() as u64)).collect();
            staging
                .register_file(&file_a, a_total, &a_pairs)
                .expect("register A");
            staging
                .register_file(&file_b, b_total, &b_pairs)
                .expect("register B");

            staging.close().await.expect("close");
        }

        let ro = StagingAreaReadOnly::open(tmp.path().to_path_buf())
            .await
            .expect("open ro");

        let stats = ro.retire_file(&file_a).await.expect("retire A");
        assert_eq!(stats.rows_deleted, 2, "retire A removes both A rows");
        assert!(
            !stats.segments_touched.is_empty(),
            "at least one segment was touched by the delete"
        );

        // Retiring file A a second time is a no-op — committed rows
        // for file A are gone, so no further deletes occur.
        let stats2 = ro.retire_file(&file_a).await.expect("retire A again");
        assert_eq!(stats2.rows_deleted, 0);
        assert!(stats2.segments_touched.is_empty());

        // Retiring file B drops exactly its two rows, including the
        // shared chunk c1 — the `chunks` row key is
        // `(file_hash, chunk_index)`, so A's delete never touched B's
        // row for the same chunk hash.
        let stats_b = ro.retire_file(&file_b).await.expect("retire B");
        assert_eq!(stats_b.rows_deleted, 2, "retire B removes both B rows");
    }

    /// `sweep_orphans` on a read-only handle unlinks sealed segment
    /// files whose `live_chunk_count` dropped to zero and removes their
    /// index rows. Mirrors the post-push cleanup contract: after
    /// `retire_file` zeroes a sealed segment's live count, a RO sweep
    /// must reclaim the disk.
    #[tokio::test(flavor = "multi_thread")]
    async fn sweep_orphans_from_readonly_unlinks_empty_segments() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().to_path_buf();
        let segments_dir = root.join("segments");

        // Prime the staging layout: open the RW handle so the lockfile,
        // segments dir, and SQLite schema all exist, then close. We
        // then craft a sealed zero-live segment at the Index level
        // below — faster than writing hundreds of MiB to trigger a natural seal.
        {
            let rw = StagingArea::open(root.clone()).await.expect("open rw");
            rw.close().await.expect("close rw");
        }

        // Build a sealed, zero-live segment directly:
        //   1. allocate a segment id in the index
        //   2. write a fake `.seg` file on disk at its id-named path
        //   3. mark it sealed with a non-zero size
        // This reproduces the end-state `retire_file` leaves when it
        // zeroes the last chunk in a sealed segment.
        let seg_id = {
            let db_path = root.join("index.db");
            let idx = index::Index::open(&db_path).expect("open index");
            let seg_id = idx.allocate_segment_id().expect("alloc");

            // Write a non-empty file so the unlink+reclaim stats are
            // observable. Contents don't matter — no chunks point here.
            let seg_path = segments_dir.join(format!("{seg_id:016x}.seg"));
            std::fs::write(&seg_path, vec![0u8; 4096]).expect("write seg");

            idx.seal_segment(seg_id, 4096).expect("seal");
            seg_id
        };

        let seg_path = segments_dir.join(format!("{seg_id:016x}.seg"));
        assert!(
            seg_path.exists(),
            "sealed segment file must exist pre-sweep"
        );

        let ro = StagingAreaReadOnly::open(root.clone())
            .await
            .expect("open ro");

        let (segments_removed, bytes_reclaimed, chunks_reclaimed) =
            ro.sweep_orphans().expect("sweep");

        assert_eq!(segments_removed, 1, "exactly one segment should sweep");
        assert_eq!(bytes_reclaimed, 4096, "bytes match segment size");
        assert_eq!(chunks_reclaimed, 0, "no chunks were attached");

        assert!(!seg_path.exists(), "segment file must be unlinked by sweep");

        // Second sweep is a no-op.
        let (second_removed, _, _) = ro.sweep_orphans().expect("second sweep");
        assert_eq!(second_removed, 0);
    }

    /// Small-file adds normally leave durable rows in `pending_chunks`
    /// and bytes in the unsealed `current.seg`. After a successful push
    /// retires those rows, read-only sweep must reclaim that file too;
    /// otherwise the common below-soft-cap path strands staging bytes.
    #[tokio::test(flavor = "multi_thread")]
    async fn sweep_orphans_from_readonly_reclaims_retired_current_segment() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().to_path_buf();
        let segments_dir = root.join("segments");
        let current_path = segments_dir.join("current.seg");

        let file_hash = compute_data_hash(b"retired-current-file");
        let chunks = [make_chunk_pub(80, 4096), make_chunk_pub(81, 4096)];
        let expected_bytes: u64 = chunks.iter().map(|(_, data)| data.len() as u64 + 8).sum();

        {
            let staging = StagingArea::open(root.clone()).await.expect("open rw");
            let total_bytes: u64 = chunks.iter().map(|(_, data)| data.len() as u64).sum();
            staging
                .pre_register_file(&file_hash, total_bytes)
                .expect("pre-register");
            let refs: Vec<(&MerkleHash, &[u8])> = chunks
                .iter()
                .map(|(hash, data)| (hash, data.as_slice()))
                .collect();
            staging
                .stage_chunks_batch(&refs, &file_hash, 0)
                .await
                .expect("stage");
            staging.flush_pending().await.expect("flush");
            staging.close().await.expect("close");
        }

        assert_eq!(
            std::fs::metadata(&current_path)
                .expect("current segment")
                .len(),
            expected_bytes,
            "test setup should leave a non-empty unsealed current segment"
        );

        let ro = StagingAreaReadOnly::open(root.clone())
            .await
            .expect("open ro");
        let retired = ro.retire_file(&file_hash).await.expect("retire");
        assert_eq!(retired.rows_deleted, chunks.len() as u64);

        let (segments_removed, bytes_reclaimed, chunks_reclaimed) =
            ro.sweep_orphans().expect("sweep");
        assert_eq!(segments_removed, 1, "retired current segment reclaims");
        assert_eq!(
            bytes_reclaimed, expected_bytes,
            "reclaimed bytes match flushed current size"
        );
        assert_eq!(
            chunks_reclaimed, 0,
            "pending-only staged rows never increment chunk_count"
        );
        assert!(
            !current_path.exists(),
            "current.seg must be unlinked after retirement"
        );

        let (second_removed, _, _) = ro.sweep_orphans().expect("second sweep");
        assert_eq!(second_removed, 0);
        drop(ro);

        let staging = StagingArea::open(root.clone()).await.expect("reopen rw");
        let next_file_hash = compute_data_hash(b"new-current-file");
        let (next_hash, next_data) = make_chunk_pub(82, 2048);
        staging
            .pre_register_file(&next_file_hash, next_data.len() as u64)
            .expect("pre-register new");
        staging
            .stage_chunks_batch(&[(&next_hash, next_data.as_slice())], &next_file_hash, 0)
            .await
            .expect("stage new");
        staging.flush_pending().await.expect("flush new");
        let restored = staging
            .get_chunk(&next_hash)
            .await
            .expect("read new")
            .expect("new chunk exists");
        assert_eq!(restored.as_ref(), next_data.as_slice());
        staging.close().await.expect("close reopened");
    }

    /// `clean_abandoned` reclaims non-current segments that have zero
    /// committed or pending chunk rows, the classic stranded state
    /// where a rollover wrote bytes but never registered locators.
    /// The writer's active current segment is preserved.
    #[tokio::test(flavor = "multi_thread")]
    async fn clean_abandoned_reclaims_unsealed_stranded_segments() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().to_path_buf();
        let segments_dir = root.join("segments");

        // Open RW once to establish layout + allocate the current
        // segment id. Don't close — we'll seed abandoned segments
        // under the same handle to avoid recovery picking one as
        // the new current on reopen.
        let staging = StagingArea::open(root.clone()).await.expect("open rw");

        // Seed two abandoned segments directly in the index:
        //   - Write .seg files on disk
        //   - Mark them sealed in the index (imitates the post-fix
        //     steady state where every rolled segment has `sealed_at`)
        //   - No chunk locator row is ever inserted, so they qualify
        //     as abandoned.
        let abandoned_ids = {
            let idx = lock_index(&staging.index).expect("lock idx");
            let a = idx.allocate_segment_id().expect("alloc a");
            let b = idx.allocate_segment_id().expect("alloc b");

            std::fs::write(segments_dir.join(format!("{a:016x}.seg")), vec![0u8; 2048])
                .expect("write a");
            std::fs::write(segments_dir.join(format!("{b:016x}.seg")), vec![0u8; 4096])
                .expect("write b");

            idx.seal_segment(a, 2048).expect("seal a");
            idx.seal_segment(b, 4096).expect("seal b");

            vec![a, b]
        };

        let (removed, bytes, pending) = staging
            .clean_abandoned(false)
            .await
            .expect("clean_abandoned");

        // The two seeded abandoned segments reclaim. The writer's
        // pristine current segment (no chunks, no pending, size=0)
        // is preserved.
        assert_eq!(removed, 2, "both seeded abandoned segments reclaim");
        assert_eq!(
            bytes,
            2048 + 4096,
            "size counters sum to seeded sealed sizes"
        );
        assert_eq!(pending, 0, "no pending rows existed for abandoned");

        for id in &abandoned_ids {
            assert!(
                !segments_dir.join(format!("{id:016x}.seg")).exists(),
                "abandoned segment {id:016x} should be unlinked"
            );
        }

        // The current segment is preserved (pristine, nothing to reset).
        assert!(
            !abandoned_ids.contains(&staging.writer.lock().await.segment_id()),
            "current segment id must not be a reclaimed one"
        );

        // Re-running is a no-op now that the abandoned segments are
        // gone and the current segment is still pristine.
        let (again, _, _) = staging
            .clean_abandoned(false)
            .await
            .expect("clean_abandoned again");
        assert_eq!(again, 0, "second call reclaims nothing");

        staging.close().await.expect("close");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn clean_abandoned_preserves_current_pending_rows() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let staging = StagingArea::open(tmp.path().to_path_buf())
            .await
            .expect("open rw");

        let file_hash = compute_data_hash(b"pending-current-file");
        let chunks = [make_chunk_pub(70, 4096), make_chunk_pub(71, 4096)];
        let total_bytes: u64 = chunks.iter().map(|(_, data)| data.len() as u64).sum();
        staging
            .pre_register_file(&file_hash, total_bytes)
            .expect("pre-register");
        let refs: Vec<(&MerkleHash, &[u8])> = chunks
            .iter()
            .map(|(hash, data)| (hash, data.as_slice()))
            .collect();
        staging
            .stage_chunks_batch(&refs, &file_hash, 0)
            .await
            .expect("stage");
        staging.flush_pending().await.expect("flush");

        let before = staging.chunks_for_file(&file_hash).expect("before");
        assert_eq!(before.len(), chunks.len());

        let (removed, _, pending_removed) = staging
            .clean_abandoned(true)
            .await
            .expect("clean abandoned");
        assert_eq!(removed, 0, "pending current segment must not reset");
        assert_eq!(pending_removed, 0, "pending rows must be preserved");

        let after = staging.chunks_for_file(&file_hash).expect("after");
        assert_eq!(after, before);

        staging.close().await.expect("close");
    }

    /// Regression: `stage_chunks_batch` must preserve ALL positions
    /// (chunk indices) for a file, even when chunk hashes repeat across
    /// batches.
    ///
    /// The clean filter emits chunks in fixed-size batches. If a chunk
    /// hash appears in batch N and again in batch N+1 within the same
    /// file, the second occurrence must land at its own `chunk_index`
    /// position even when `batch_dedup_check` reports the hash as
    /// already mapped.
    ///
    /// Dropping the second occurrence leaves a gap in `chunk_index` and
    /// makes `chunks_for_file` return fewer chunks than the file
    /// actually has, which the push pipeline turns into a short shard
    /// that hydrate cannot reconstruct byte-identically.
    ///
    /// Invariant 7 (see `crab.md`): staging `chunks_for_file(file_hash)`
    /// must return all chunks for that file version, including chunks
    /// deduplicated against other file versions.
    #[tokio::test(flavor = "multi_thread")]
    async fn stage_chunks_batch_preserves_all_positions_on_repeat_hash_across_batches() {
        use tests::make_chunk_pub as make_chunk;

        let tmp = tempfile::tempdir().expect("tempdir");
        let staging = StagingArea::open(tmp.path().to_path_buf())
            .await
            .expect("open");

        // Build a file with two distinct chunks A, B. The file layout is
        //   [A, B, A]  — 3 positions, 2 unique hashes.
        let (a_hash, a_data) = make_chunk(1, 4096);
        let (b_hash, b_data) = make_chunk(2, 4096);
        let file_hash = compute_data_hash(b"repeat-across-batches");
        let total_bytes = (a_data.len() * 2 + b_data.len()) as u64;
        staging
            .pre_register_file(&file_hash, total_bytes)
            .expect("pre-register");

        // Batch 1: positions 0..2 = [A, B]
        let batch1: Vec<(&MerkleHash, &[u8])> =
            vec![(&a_hash, a_data.as_slice()), (&b_hash, b_data.as_slice())];
        staging
            .stage_chunks_batch(&batch1, &file_hash, 0)
            .await
            .expect("batch 1");

        // Batch 2: position 2 = [A]  — A is a repeat, but must still land
        // at chunk_index == 2.
        let batch2: Vec<(&MerkleHash, &[u8])> = vec![(&a_hash, a_data.as_slice())];
        staging
            .stage_chunks_batch(&batch2, &file_hash, 2)
            .await
            .expect("batch 2");

        staging.flush_pending().await.expect("flush");

        // Expected: 3 rows at chunk_index 0, 1, 2.
        let chunks = staging.chunks_for_file(&file_hash).expect("chunks");
        assert_eq!(
            chunks.len(),
            3,
            "expected 3 chunks (positions 0, 1, 2) but got {} — the repeat at position 2 was dropped",
            chunks.len()
        );
        assert_eq!(chunks[0], a_hash, "position 0 should be A");
        assert_eq!(chunks[1], b_hash, "position 1 should be B");
        assert_eq!(chunks[2], a_hash, "position 2 should be A");

        staging.close().await.expect("close");
    }

    /// Intra-batch duplicate: when a chunk hash repeats within the same
    /// batch AND the chunk already exists on disk (e.g. from an earlier
    /// file that shares chunks), both positions must be inserted.
    ///
    /// Prior buggy behavior: `batch_dedup_check` marked both occurrences
    /// `is_mapped=false` since the chunk wasn't yet in THIS file, so
    /// both got inserted — that case worked. The failure mode is the
    /// cross-batch one above. This test locks down the intra-batch
    /// cross-file shared-chunk case for regression coverage.
    #[tokio::test(flavor = "multi_thread")]
    async fn stage_chunks_batch_preserves_positions_on_repeat_hash_within_single_batch() {
        use tests::make_chunk_pub as make_chunk;

        let tmp = tempfile::tempdir().expect("tempdir");
        let staging = StagingArea::open(tmp.path().to_path_buf())
            .await
            .expect("open");

        let (a_hash, a_data) = make_chunk(1, 4096);
        let (b_hash, b_data) = make_chunk(2, 4096);

        // Stage chunk A under an unrelated first file so A lands in
        // `pending_chunks` before the real file is staged.
        let other_file = compute_data_hash(b"other-file");
        staging
            .pre_register_file(&other_file, a_data.len() as u64)
            .expect("pre-register other");
        let first_batch: Vec<(&MerkleHash, &[u8])> = vec![(&a_hash, a_data.as_slice())];
        staging
            .stage_chunks_batch(&first_batch, &other_file, 0)
            .await
            .expect("stage other");

        // Real file layout: [A, B, A] as a single batch. `batch_dedup_check`
        // will see A as existing (from the other file) with
        // `is_mapped=false` for the real file's rows.
        let real_file = compute_data_hash(b"real-file");
        let total_bytes = (a_data.len() * 2 + b_data.len()) as u64;
        staging
            .pre_register_file(&real_file, total_bytes)
            .expect("pre-register real");

        let batch: Vec<(&MerkleHash, &[u8])> = vec![
            (&a_hash, a_data.as_slice()),
            (&b_hash, b_data.as_slice()),
            (&a_hash, a_data.as_slice()),
        ];
        staging
            .stage_chunks_batch(&batch, &real_file, 0)
            .await
            .expect("stage real");

        staging.flush_pending().await.expect("flush");

        let chunks = staging.chunks_for_file(&real_file).expect("chunks");
        assert_eq!(chunks.len(), 3, "all three positions must be preserved");
        assert_eq!(chunks[0], a_hash);
        assert_eq!(chunks[1], b_hash);
        assert_eq!(chunks[2], a_hash);

        staging.close().await.expect("close");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stage_chunks_batch_writes_new_intra_batch_duplicate_once() {
        use tests::make_chunk_pub as make_chunk;

        let tmp = tempfile::tempdir().expect("tempdir");
        let staging = StagingArea::open(tmp.path().to_path_buf())
            .await
            .expect("open");

        let (a_hash, a_data) = make_chunk(11, 4096);
        let file_hash = compute_data_hash(b"same-new-chunk-three-times");
        staging
            .pre_register_file(&file_hash, (a_data.len() * 3) as u64)
            .expect("pre-register");

        let batch: Vec<(&MerkleHash, &[u8])> = vec![
            (&a_hash, a_data.as_slice()),
            (&a_hash, a_data.as_slice()),
            (&a_hash, a_data.as_slice()),
        ];
        staging
            .stage_chunks_batch(&batch, &file_hash, 0)
            .await
            .expect("stage repeated new chunk");

        let chunks = staging.chunks_for_file(&file_hash).expect("chunks");
        assert_eq!(chunks, vec![a_hash, a_hash, a_hash]);

        let segment_len = std::fs::metadata(tmp.path().join("segments/current.seg"))
            .expect("current segment")
            .len();
        assert_eq!(
            segment_len,
            a_data.len() as u64 + 8,
            "duplicate new hashes should share one physical staged record"
        );

        staging.close().await.expect("close");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stage_chunks_batch_rejects_unencodable_chunk_index_before_append() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let staging = StagingArea::open(tmp.path().to_path_buf())
            .await
            .expect("open");

        let (a_hash, a_data) = make_chunk_pub(31, 4096);
        let (b_hash, b_data) = make_chunk_pub(32, 4096);
        let file_hash = compute_data_hash(b"too-many-chunks");
        staging
            .pre_register_file(&file_hash, (a_data.len() + b_data.len()) as u64)
            .expect("pre-register");

        let batch: Vec<(&MerkleHash, &[u8])> =
            vec![(&a_hash, a_data.as_slice()), (&b_hash, b_data.as_slice())];
        let err = staging
            .stage_chunks_batch(&batch, &file_hash, u64::from(u32::MAX))
            .await
            .expect_err("batch must fail before chunk_index wraps shard format");

        assert!(
            matches!(err, StagingError::StagingCorrupt(ref msg) if msg.contains("exceeds shard format limit")),
            "unexpected error: {err:?}"
        );
        assert!(
            staging
                .chunks_for_file(&file_hash)
                .expect("chunks")
                .is_empty(),
            "failed batch must not create chunk rows"
        );
        let current_len = std::fs::metadata(tmp.path().join("segments/current.seg"))
            .expect("current segment")
            .len();
        assert_eq!(current_len, 0, "failed batch must not append bytes");

        staging.close().await.expect("close");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn stage_chunks_batch_rejects_stale_pending_position_before_append() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let staging = StagingArea::open(tmp.path().to_path_buf())
            .await
            .expect("open");

        let file_hash = compute_data_hash(b"stale-pending-position");
        let first = [make_chunk_pub(41, 4096), make_chunk_pub(42, 4096)];
        let second = [make_chunk_pub(51, 4096), make_chunk_pub(52, 4096)];

        let first_total: u64 = first.iter().map(|(_, data)| data.len() as u64).sum();
        staging
            .pre_register_file(&file_hash, first_total)
            .expect("pre-register first");
        let first_refs: Vec<(&MerkleHash, &[u8])> = first
            .iter()
            .map(|(hash, data)| (hash, data.as_slice()))
            .collect();
        staging
            .stage_chunks_batch(&first_refs, &file_hash, 0)
            .await
            .expect("stage first");
        staging.flush_pending().await.expect("flush first");
        let before_len = std::fs::metadata(tmp.path().join("segments/current.seg"))
            .expect("current segment")
            .len();

        let second_refs: Vec<(&MerkleHash, &[u8])> = second
            .iter()
            .map(|(hash, data)| (hash, data.as_slice()))
            .collect();
        let err = staging
            .stage_chunks_batch(&second_refs, &file_hash, 0)
            .await
            .expect_err("stale pending positions must fail before append");

        assert!(
            matches!(err, StagingError::StagingCorrupt(ref msg) if msg.contains("pending chunk collision")),
            "unexpected error: {err:?}"
        );
        let after_len = std::fs::metadata(tmp.path().join("segments/current.seg"))
            .expect("current segment after")
            .len();
        assert_eq!(after_len, before_len);
        assert_eq!(
            staging.chunks_for_file(&file_hash).expect("chunks"),
            first.iter().map(|(hash, _)| *hash).collect::<Vec<_>>()
        );

        staging.close().await.expect("close");
    }

    /// Re-adding the same file via `retire_file` + `stage_chunks_batch`
    /// must produce a clean chunk sequence starting at index 0.
    /// Without the retire, the second add's rows collide on
    /// `(file_hash, chunk_index)` with the survivors from the first
    /// add and staging fails after appending bytes.
    #[tokio::test(flavor = "multi_thread")]
    async fn retire_then_restage_yields_fresh_zero_indexed_chunks() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let staging = StagingArea::open(tmp.path().to_path_buf())
            .await
            .expect("open rw");

        let file_hash = compute_data_hash(b"re-add-file");
        let first = [make_chunk_pub(10, 4096), make_chunk_pub(11, 4096)];
        let second = [
            make_chunk_pub(20, 4096),
            make_chunk_pub(21, 4096),
            make_chunk_pub(22, 4096),
        ];

        let total_bytes: u64 = first.iter().map(|(_, d)| d.len() as u64).sum();
        staging
            .pre_register_file(&file_hash, total_bytes)
            .expect("pre-register 1");

        let first_refs: Vec<(&MerkleHash, &[u8])> =
            first.iter().map(|(h, d)| (h, d.as_slice())).collect();
        staging
            .stage_chunks_batch(&first_refs, &file_hash, 0)
            .await
            .expect("stage first add");
        staging.flush_pending().await.expect("flush 1");

        let after_first = staging.chunks_for_file(&file_hash).expect("chunks 1");
        assert_eq!(after_first.len(), 2);

        // Simulate re-add: retire, then restage with a different chunk set.
        let retired = staging.retire_file(&file_hash).expect("retire");
        assert_eq!(retired.rows_deleted, 2, "both first-add rows purged");

        let total_bytes_2: u64 = second.iter().map(|(_, d)| d.len() as u64).sum();
        staging
            .pre_register_file(&file_hash, total_bytes_2)
            .expect("pre-register 2");

        let second_refs: Vec<(&MerkleHash, &[u8])> =
            second.iter().map(|(h, d)| (h, d.as_slice())).collect();
        staging
            .stage_chunks_batch(&second_refs, &file_hash, 0)
            .await
            .expect("stage second add");
        staging.flush_pending().await.expect("flush 2");

        let after_second = staging.chunks_for_file(&file_hash).expect("chunks 2");
        assert_eq!(
            after_second.len(),
            3,
            "re-add after retire must produce exactly the new chunk set"
        );
        for (got, want) in after_second.iter().zip(second.iter().map(|(h, _)| h)) {
            assert_eq!(got, want, "re-add must reflect second chunk sequence");
        }

        staging.close().await.expect("close");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn retired_file_batch_staging_yields_fresh_zero_indexed_chunks() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let staging = StagingArea::open(tmp.path().to_path_buf())
            .await
            .expect("open rw");

        let file_hash = compute_data_hash(b"fresh-retired-re-add-file");
        let first = [make_chunk_pub(60, 4096), make_chunk_pub(61, 4096)];
        let second = [
            make_chunk_pub(70, 4096),
            make_chunk_pub(71, 4096),
            make_chunk_pub(72, 4096),
        ];

        let retired = staging.retire_file(&file_hash).expect("retire empty");
        assert_eq!(retired.rows_deleted, 0);

        let total_bytes: u64 = first.iter().map(|(_, d)| d.len() as u64).sum();
        staging
            .pre_register_file(&file_hash, total_bytes)
            .expect("pre-register 1");

        let first_refs: Vec<(&MerkleHash, &[u8])> =
            first.iter().map(|(h, d)| (h, d.as_slice())).collect();
        staging
            .stage_chunks_batch_for_retired_file(&first_refs, &file_hash, 0)
            .await
            .expect("stage first add");
        staging.flush_pending().await.expect("flush 1");

        let after_first = staging.chunks_for_file(&file_hash).expect("chunks 1");
        assert_eq!(after_first.len(), 2);

        let retired = staging.retire_file(&file_hash).expect("retire");
        assert_eq!(retired.rows_deleted, 2);

        let total_bytes_2: u64 = second.iter().map(|(_, d)| d.len() as u64).sum();
        staging
            .pre_register_file(&file_hash, total_bytes_2)
            .expect("pre-register 2");

        let second_refs: Vec<(&MerkleHash, &[u8])> =
            second.iter().map(|(h, d)| (h, d.as_slice())).collect();
        staging
            .stage_chunks_batch_for_retired_file(&second_refs, &file_hash, 0)
            .await
            .expect("stage second add");
        staging.flush_pending().await.expect("flush 2");

        let after_second = staging.chunks_for_file(&file_hash).expect("chunks 2");
        assert_eq!(
            after_second,
            second.iter().map(|(h, _)| *h).collect::<Vec<_>>()
        );

        staging.close().await.expect("close");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn retired_file_batch_staging_still_rejects_position_collision() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let staging = StagingArea::open(tmp.path().to_path_buf())
            .await
            .expect("open rw");

        let file_hash = compute_data_hash(b"fresh-retired-contract-violation");
        let first = [make_chunk_pub(80, 4096), make_chunk_pub(81, 4096)];
        let second = [make_chunk_pub(90, 4096), make_chunk_pub(91, 4096)];

        let total_bytes: u64 = first.iter().map(|(_, d)| d.len() as u64).sum();
        staging
            .pre_register_file(&file_hash, total_bytes)
            .expect("pre-register 1");
        let first_refs: Vec<(&MerkleHash, &[u8])> =
            first.iter().map(|(h, d)| (h, d.as_slice())).collect();
        staging
            .stage_chunks_batch(&first_refs, &file_hash, 0)
            .await
            .expect("stage first");
        staging.flush_pending().await.expect("flush 1");

        let second_refs: Vec<(&MerkleHash, &[u8])> =
            second.iter().map(|(h, d)| (h, d.as_slice())).collect();
        let err = staging
            .stage_chunks_batch_for_retired_file(&second_refs, &file_hash, 0)
            .await
            .expect_err("misused retired-file staging must reject conflicting positions");

        assert!(
            matches!(err, StagingError::StagingCorrupt(ref msg) if msg.contains("pending chunk collision")),
            "unexpected error: {err:?}"
        );
        assert_eq!(
            staging.chunks_for_file(&file_hash).expect("chunks"),
            first.iter().map(|(hash, _)| *hash).collect::<Vec<_>>()
        );

        staging.close().await.expect("close");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn legacy_migration_quarantines_wrong_file_hash_without_deleting_payload() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().to_path_buf();
        let content = b"legacy payload remains recoverable".to_vec();
        let chunk_hash = compute_data_hash(&content);
        let original_file_hash = chunk_hash;
        let wrong_file_hash = MerkleHash::from([0xA5; 32]);
        {
            let staging = StagingArea::open(root.clone()).await.expect("open staging");
            staging
                .pre_register_file(&original_file_hash, content.len() as u64)
                .expect("pre-register");
            staging
                .stage_chunks_batch(&[(&chunk_hash, content.as_slice())], &original_file_hash, 0)
                .await
                .expect("stage payload");
            staging.flush_pending().await.expect("flush");
            staging.close().await.expect("close staging");
        }

        let connection = rusqlite::Connection::open(root.join("index.db")).expect("open index");
        connection
            .execute(
                "UPDATE chunks SET file_hash = ?1 WHERE file_hash = ?2",
                rusqlite::params![
                    <[u8; 32]>::from(wrong_file_hash).as_slice(),
                    <[u8; 32]>::from(original_file_hash).as_slice()
                ],
            )
            .expect("rewrite chunk owner");
        connection
            .execute(
                "UPDATE files SET file_hash = ?1 WHERE file_hash = ?2",
                rusqlite::params![
                    <[u8; 32]>::from(wrong_file_hash).as_slice(),
                    <[u8; 32]>::from(original_file_hash).as_slice()
                ],
            )
            .expect("rewrite file hash");
        connection
            .execute(
                "UPDATE staging_meta SET value = '1' WHERE key = 'layout_version'",
                [],
            )
            .expect("mark legacy layout");
        drop(connection);

        let staging = StagingArea::open(root).await.expect("migrate staging");
        assert!(
            staging
                .published_recipe_for_file(&wrong_file_hash)
                .expect("published recipe lookup")
                .is_none()
        );
        assert_eq!(
            staging
                .lifecycle_health()
                .expect("lifecycle health")
                .quarantined_entries,
            1
        );
        assert_eq!(
            staging
                .get_chunk(&chunk_hash)
                .await
                .expect("read preserved payload")
                .expect("payload exists"),
            Bytes::from(content)
        );
        staging.close().await.expect("close migrated staging");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn legacy_migration_physically_verifies_recipe_before_publication() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path().to_path_buf();
        let content = b"legacy recipe physical verification".to_vec();
        let chunk_hash = compute_data_hash(&content);
        let file_hash = MerkleHash::from(*blake3::hash(&content).as_bytes());
        {
            let staging = StagingArea::open(root.clone()).await.expect("open staging");
            staging
                .pre_register_file(&file_hash, content.len() as u64)
                .expect("pre-register");
            staging
                .stage_chunks_batch(&[(&chunk_hash, content.as_slice())], &file_hash, 0)
                .await
                .expect("stage payload");
            staging.flush_pending().await.expect("flush");
            staging.close().await.expect("close staging");
        }

        let connection = rusqlite::Connection::open(root.join("index.db")).expect("open index");
        connection
            .execute(
                "UPDATE staging_meta SET value = '1' WHERE key = 'layout_version'",
                [],
            )
            .expect("mark legacy layout");
        drop(connection);

        let staging = StagingArea::open(root).await.expect("migrate staging");
        let recipe = staging
            .published_recipe_for_file(&file_hash)
            .expect("published recipe lookup")
            .expect("physically verified recipe becomes publishable");
        assert_eq!(recipe.sequence().file_hash, file_hash);
        assert_eq!(recipe.sequence().file_size, content.len() as u64);
        assert_eq!(recipe.sequence().spans.len(), 1);
        assert_eq!(recipe.sequence().spans[0].chunk_hash, chunk_hash);
        assert_eq!(
            staging
                .lifecycle_health()
                .expect("lifecycle health")
                .quarantined_entries,
            0
        );
        staging.close().await.expect("close migrated staging");
    }
}
