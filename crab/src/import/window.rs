//! Window planning for `crab import`.
//!
//! The assemble stage (Task 12) walks a sequence of
//! [`CommitWindow`] records and emits one git commit per window.
//! This module is the pure, I/O-free planner that turns a set of
//! already-enumerated [`ImportEntry`] rows into that sequence.
//!
//! Three entry points cover the three source modes:
//!
//! - [`plan_commit_windows`] — versioned buckets with default
//!   `--window <duration>`. Groups entries into
//!   wall-clock-aligned time buckets.
//! - [`plan_snapshot`] — `--at <rfc3339>` single-commit mode.
//!   Picks the latest version per key whose `last_modified ≤ at`
//!   and drops delete-marker entries.
//! - [`plan_flat_single_commit`] — flat buckets, one commit for
//!   the whole staged set.
//!
//! All three are synchronous `fn`s: unit tests do not need a
//! runtime, and the coordinator (Task 14) is free to call them
//! after the async enumerate pass has filled the journal.
//!
//! # Ordering invariant
//!
//! Within every window the entries are sorted by
//! `(last_modified, relative_path, version_id)` — the same key
//! used in [`Journal::iter_entries_sorted_by_time`] so repeated
//! imports of an unchanged bucket produce byte-identical commits.
//! Requirement I1a calls this out explicitly; the same tuple is
//! the tiebreaker across this module's tests.
//!
//! # Wall-clock alignment
//!
//! For a non-zero `window`, window start/end are snapped to
//! multiples of the window width (e.g. for 1 h:
//! `2025-01-01T00:00:00Z`, `2025-01-01T01:00:00Z`, …). This makes
//! the commit graph deterministic across reruns and independent
//! of when the first version in the bucket landed.
//!
//! [`Journal::iter_entries_sorted_by_time`]: crate::import::Journal::iter_entries_sorted_by_time

use std::time::Duration;

use crate::core::error::{CrabError, Result};
use crate::import::journal::{EntryState, ImportEntry};

/// One commit boundary: a window over the timeline plus the
/// staged entries that fall inside it.
///
/// `window_start` and `window_end` are epoch seconds (UTC).
/// `window_end` is exclusive for multi-entry buckets
/// (`[start, start+window_secs)`), but equals `window_start` when
/// the planner collapses a single instant (e.g. `Duration::ZERO`
/// or `plan_snapshot`). Consumers should treat the pair as a
/// closed-open range when `window_end > window_start` and as a
/// single-instant commit otherwise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitWindow {
    pub window_start: i64,
    pub window_end: i64,
    pub entries: Vec<ImportEntry>,
}

/// Sort key for determinism within a window — matches the
/// invariant from requirement I1a.
fn sort_key(entry: &ImportEntry) -> (i64, &str, &str) {
    (
        entry.last_modified,
        entry.relative_path.as_str(),
        entry.version_id.as_str(),
    )
}

/// Validate a `--since` / `--until` pair up front.
///
/// Returns [`CrabError::ImportInvalidHistoryRange`] when both
/// bounds are present and `since > until`. A single-sided bound
/// (or neither) is always valid.
///
/// The error payload carries the raw epoch seconds as decimal
/// strings — the CLI layer overwrites them with the user's
/// RFC3339 input before surfacing if it wishes.
pub fn validate_history_range(since: Option<i64>, until: Option<i64>) -> Result<()> {
    if let (Some(s), Some(u)) = (since, until)
        && s > u
    {
        return Err(CrabError::ImportInvalidHistoryRange {
            since: s.to_string(),
            until: u.to_string(),
        });
    }
    Ok(())
}

/// Plan one commit per time bucket.
///
/// Versioned-mode entry point. The caller owns any `--since` /
/// `--until` filtering at enumerate time; this function receives
/// the already-filtered set and cares only about grouping and
/// ordering.
///
/// # Arguments
///
/// - `entries` — every staged / delete-marker row the assemble
///   stage plans to honor. Skipped / Failed rows should already
///   be filtered out by the caller.
/// - `window` — bucket width. `Duration::ZERO` means "one commit
///   per distinct `last_modified`"; the resulting windows each
///   contain a single instant (entries with equal timestamps
///   still share that instant).
/// - `max_commits` — hard cap on the number of windows. Exceeding
///   the cap returns [`CrabError::ImportCommitCeilingExceeded`]
///   so the user can widen `--window` and retry.
///
/// # Behavior
///
/// Empty input returns an empty `Vec`. No work to do is not an
/// error — the coordinator short-circuits upstream. Callers that
/// expect at least one commit (e.g. the CLI summary) should check
/// for emptiness themselves.
pub fn plan_commit_windows(
    entries: Vec<ImportEntry>,
    window: Duration,
    max_commits: u32,
) -> Result<Vec<CommitWindow>> {
    if entries.is_empty() {
        return Ok(Vec::new());
    }

    // Sort by the deterministic tiebreaker first so the windowing
    // pass walks entries in monotonic timestamp order.
    let mut sorted = entries;
    sorted.sort_by(|a, b| sort_key(a).cmp(&sort_key(b)));

    let window_secs_raw = window.as_secs();

    // `Duration::ZERO` case: collapse runs of equal timestamps
    // into instant-sized windows (one commit per distinct
    // `last_modified`, since entries at the exact same instant
    // are indistinguishable at human time resolution).
    if window_secs_raw == 0 {
        let mut windows: Vec<CommitWindow> = Vec::new();
        let mut bucket: Vec<ImportEntry> = Vec::new();
        let mut bucket_ts: Option<i64> = None;
        for entry in sorted {
            match bucket_ts {
                Some(ts) if ts == entry.last_modified => {
                    bucket.push(entry);
                }
                _ => {
                    if let Some(ts) = bucket_ts.take() {
                        windows.push(CommitWindow {
                            window_start: ts,
                            window_end: ts,
                            entries: std::mem::take(&mut bucket),
                        });
                    }
                    bucket_ts = Some(entry.last_modified);
                    bucket.push(entry);
                }
            }
        }
        if let Some(ts) = bucket_ts {
            windows.push(CommitWindow {
                window_start: ts,
                window_end: ts,
                entries: bucket,
            });
        }
        enforce_commit_ceiling(windows.len(), max_commits)?;
        return Ok(windows);
    }

    // Non-zero window: snap each entry to `floor(ts/width) * width`
    // so boundaries align on wall-clock multiples of the window.
    // We cast to i64 once; widths larger than i64::MAX seconds are
    // not meaningful for this planner (and would be rejected by
    // the CLI layer long before here).
    let window_secs = i64::try_from(window_secs_raw).unwrap_or(i64::MAX);

    let mut windows: Vec<CommitWindow> = Vec::new();
    let mut current_start: Option<i64> = None;
    let mut bucket: Vec<ImportEntry> = Vec::new();

    for entry in sorted {
        let aligned_start = align_down(entry.last_modified, window_secs);
        match current_start {
            Some(start) if start == aligned_start => {
                bucket.push(entry);
            }
            _ => {
                if let Some(start) = current_start.take() {
                    windows.push(CommitWindow {
                        window_start: start,
                        window_end: start.saturating_add(window_secs),
                        entries: std::mem::take(&mut bucket),
                    });
                }
                current_start = Some(aligned_start);
                bucket.push(entry);
            }
        }
    }
    if let Some(start) = current_start {
        windows.push(CommitWindow {
            window_start: start,
            window_end: start.saturating_add(window_secs),
            entries: bucket,
        });
    }

    enforce_commit_ceiling(windows.len(), max_commits)?;
    Ok(windows)
}

/// Single-snapshot plan for `--at <timestamp>`.
///
/// Walks `entries` once, keeping per key the version with the
/// greatest `last_modified ≤ at`. Delete-marker entries that win
/// the per-key contest are dropped (they represent "this key does
/// not exist at `at`"). Non-marker entries are emitted in the
/// same `(last_modified, relative_path, version_id)` order the
/// rest of the planner uses.
///
/// The returned [`CommitWindow`] has both `window_start` and
/// `window_end` set to `at` — a single instant. Assemble writes
/// exactly one commit dated at that timestamp.
///
/// # Errors
///
/// Returns [`CrabError::Internal`] when `entries` is empty
/// after filtering; assemble has nothing to commit. Callers can
/// treat that as an early-exit signal (no import to perform) or
/// an upstream programmer error — the coordinator will never
/// produce an empty staged set in practice.
pub fn plan_snapshot(entries: Vec<ImportEntry>, at: i64) -> Result<CommitWindow> {
    // Pick the latest-per-key version whose timestamp is ≤ `at`.
    // Walk all entries and retain the best candidate per path.
    use std::collections::HashMap;

    let mut best: HashMap<String, ImportEntry> = HashMap::new();
    for entry in entries {
        if entry.last_modified > at {
            continue;
        }
        match best.get(&entry.relative_path) {
            Some(current) if current.last_modified > entry.last_modified => {}
            Some(current)
                if current.last_modified == entry.last_modified
                    && current.version_id.as_str() >= entry.version_id.as_str() =>
            {
                // Stable tiebreak: larger version_id wins, matching
                // the deterministic ordering invariant.
            }
            _ => {
                best.insert(entry.relative_path.clone(), entry);
            }
        }
    }

    // Drop delete markers — `at` observed a deletion for that
    // key, so it simply doesn't exist in the commit.
    let mut kept: Vec<ImportEntry> = best.into_values().filter(|e| !e.is_delete_marker).collect();

    if kept.is_empty() {
        return Err(CrabError::Internal(
            "plan_snapshot: no entries remain after filtering to --at timestamp".into(),
        ));
    }

    kept.sort_by(|a, b| sort_key(a).cmp(&sort_key(b)));

    Ok(CommitWindow {
        window_start: at,
        window_end: at,
        entries: kept,
    })
}

/// Flat-mode plan: one commit containing every entry.
///
/// `window_start` holds the oldest `last_modified` in the set and
/// `window_end` the newest, matching the decision in
/// requirements.md open question #12 (use the oldest
/// `last_modified` in the bucket as the commit timestamp so flat
/// imports are honest about when the data landed).
///
/// # Errors
///
/// Returns [`CrabError::Internal`] when `entries` is empty —
/// there is no first commit to assemble. The coordinator rejects
/// empty-after-filtering imports far upstream (I2 refuses with a
/// clear error when nothing matches), so hitting this in the
/// planner is a programmer bug.
pub fn plan_flat_single_commit(entries: Vec<ImportEntry>) -> Result<CommitWindow> {
    if entries.is_empty() {
        return Err(CrabError::Internal(
            "plan_flat_single_commit: no entries to plan".into(),
        ));
    }

    let mut sorted = entries;
    sorted.sort_by(|a, b| sort_key(a).cmp(&sort_key(b)));

    // Safe: non-empty by construction.
    let window_start = sorted.first().map_or(0, |e| e.last_modified);
    let window_end = sorted.last().map_or(window_start, |e| e.last_modified);

    Ok(CommitWindow {
        window_start,
        window_end,
        entries: sorted,
    })
}

/// Round `ts` down to the nearest multiple of `width`. Handles
/// negative timestamps (pre-1970) by biasing the division so the
/// result stays ≤ ts — Rust's integer division truncates toward
/// zero, which is not the same as floor division for negatives.
fn align_down(ts: i64, width: i64) -> i64 {
    if width <= 0 {
        return ts;
    }
    if ts >= 0 {
        (ts / width) * width
    } else {
        // e.g. ts = -1, width = 60 → want -60 (the bucket covering
        // [-60, 0)), not 0. Bias by (width - 1) toward negative
        // infinity before dividing, then subtract the bias back.
        let q = (ts - (width - 1)) / width;
        q * width
    }
}

fn enforce_commit_ceiling(planned: usize, ceiling: u32) -> Result<()> {
    let planned_u64 = u64::try_from(planned).unwrap_or(u64::MAX);
    let ceiling_u64 = u64::from(ceiling);
    if planned_u64 > ceiling_u64 {
        return Err(CrabError::ImportCommitCeilingExceeded {
            planned: planned_u64,
            ceiling: ceiling_u64,
        });
    }
    Ok(())
}

/// Build a `Staged` [`ImportEntry`] for tests and for callers
/// that need to hand the planner a synthetic entry. Lives here
/// rather than in `#[cfg(test)]` so downstream modules (the
/// coordinator smoke tests in particular) can reuse it.
#[doc(hidden)]
#[must_use]
pub fn staged_entry(
    relative_path: &str,
    version_id: &str,
    size: u64,
    last_modified: i64,
) -> ImportEntry {
    ImportEntry {
        relative_path: relative_path.into(),
        version_id: version_id.into(),
        size,
        etag: None,
        last_modified,
        is_delete_marker: false,
        state: EntryState::Staged {
            file_hash: [0u8; 32],
        },
    }
}

/// Build a delete-marker [`ImportEntry`] for tests. The entry is
/// recorded as `Staged` with a sentinel file hash so assemble
/// recognizes it as a deletion (Task 9.5 wires the sentinel
/// semantics end-to-end).
#[doc(hidden)]
#[must_use]
pub fn delete_marker_entry(
    relative_path: &str,
    version_id: &str,
    last_modified: i64,
) -> ImportEntry {
    ImportEntry {
        relative_path: relative_path.into(),
        version_id: version_id.into(),
        size: 0,
        etag: None,
        last_modified,
        is_delete_marker: true,
        state: EntryState::Staged {
            file_hash: [0u8; 32],
        },
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    reason = "test assertions"
)]
mod tests {
    use super::*;

    /// 1 h wall-clock window. Matches the CLI default in I1a.
    const ONE_HOUR: Duration = Duration::from_secs(3_600);

    // 2025-01-01T00:00:00Z
    const T_2025_01_01: i64 = 1_735_689_600;
    /// Ceiling high enough that no test trips it unless it's
    /// explicitly testing the ceiling path.
    const NO_CEILING: u32 = 1_000_000;

    // ── Window alignment ───────────────────────────────────────

    #[test]
    fn one_hour_window_splits_three_hours_into_three_windows() {
        // Entries at 00:15, 01:30, 02:45 within the same day →
        // three 1 h windows with aligned boundaries.
        let entries = vec![
            staged_entry("a", "", 10, T_2025_01_01 + 15 * 60),
            staged_entry("b", "", 20, T_2025_01_01 + 3_600 + 30 * 60),
            staged_entry("c", "", 30, T_2025_01_01 + 2 * 3_600 + 45 * 60),
        ];

        let windows = plan_commit_windows(entries, ONE_HOUR, NO_CEILING).unwrap();
        assert_eq!(windows.len(), 3);

        assert_eq!(windows[0].window_start, T_2025_01_01);
        assert_eq!(windows[0].window_end, T_2025_01_01 + 3_600);
        assert_eq!(windows[0].entries.len(), 1);
        assert_eq!(windows[0].entries[0].relative_path, "a");

        assert_eq!(windows[1].window_start, T_2025_01_01 + 3_600);
        assert_eq!(windows[1].window_end, T_2025_01_01 + 2 * 3_600);
        assert_eq!(windows[1].entries[0].relative_path, "b");

        assert_eq!(windows[2].window_start, T_2025_01_01 + 2 * 3_600);
        assert_eq!(windows[2].window_end, T_2025_01_01 + 3 * 3_600);
        assert_eq!(windows[2].entries[0].relative_path, "c");
    }

    #[test]
    fn entries_in_same_hour_collapse_into_one_window() {
        // Five entries spanning the same aligned hour → one window
        // regardless of arrival order.
        let entries = vec![
            staged_entry("a", "", 10, T_2025_01_01 + 5),
            staged_entry("b", "", 10, T_2025_01_01 + 100),
            staged_entry("c", "", 10, T_2025_01_01 + 3_599),
            staged_entry("d", "", 10, T_2025_01_01 + 500),
            staged_entry("e", "", 10, T_2025_01_01),
        ];
        let windows = plan_commit_windows(entries, ONE_HOUR, NO_CEILING).unwrap();
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].window_start, T_2025_01_01);
        assert_eq!(windows[0].window_end, T_2025_01_01 + 3_600);
        assert_eq!(windows[0].entries.len(), 5);
    }

    #[test]
    fn window_boundaries_are_wall_clock_aligned() {
        // An entry at T+0:45 lands in the `[T, T+1h)` bucket, not
        // in a bucket that starts at T+0:45. Alignment is the
        // invariant that lets repeated imports reproduce byte-
        // identical commits.
        let entries = vec![staged_entry("a", "", 10, T_2025_01_01 + 45 * 60)];
        let windows = plan_commit_windows(entries, ONE_HOUR, NO_CEILING).unwrap();
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].window_start, T_2025_01_01);
        assert_eq!(windows[0].window_end, T_2025_01_01 + 3_600);
    }

    // ── Deletion handling ──────────────────────────────────────

    #[test]
    fn delete_markers_are_preserved_in_plan_commit_windows() {
        // Delete markers travel with the window so assemble can
        // decide what to do (emit a `git rm` for that path).
        // plan_commit_windows does not filter them.
        let entries = vec![
            staged_entry("keep.bin", "", 10, T_2025_01_01 + 30),
            delete_marker_entry("gone.bin", "v1", T_2025_01_01 + 60),
        ];
        let windows = plan_commit_windows(entries, ONE_HOUR, NO_CEILING).unwrap();
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].entries.len(), 2);
        let gone = windows[0]
            .entries
            .iter()
            .find(|e| e.relative_path == "gone.bin")
            .unwrap();
        assert!(gone.is_delete_marker);
    }

    // ── --window 0 ─────────────────────────────────────────────

    #[test]
    fn zero_window_puts_each_distinct_timestamp_in_its_own_window() {
        // `--window 0` means "one commit per distinct
        // last_modified". Entries at the same instant share a
        // window because they're indistinguishable at human
        // resolution.
        let entries = vec![
            staged_entry("a", "", 10, 100),
            staged_entry("b", "", 10, 100),
            staged_entry("c", "", 10, 200),
            staged_entry("d", "", 10, 300),
        ];
        let windows = plan_commit_windows(entries, Duration::ZERO, NO_CEILING).unwrap();
        assert_eq!(windows.len(), 3);

        assert_eq!(windows[0].window_start, 100);
        assert_eq!(windows[0].window_end, 100);
        assert_eq!(windows[0].entries.len(), 2);

        assert_eq!(windows[1].window_start, 200);
        assert_eq!(windows[1].window_end, 200);
        assert_eq!(windows[1].entries.len(), 1);

        assert_eq!(windows[2].window_start, 300);
        assert_eq!(windows[2].window_end, 300);
        assert_eq!(windows[2].entries.len(), 1);
    }

    // ── plan_snapshot --at selection ──────────────────────────

    #[test]
    fn plan_snapshot_picks_latest_version_at_or_before_at() {
        // Key `a` has three versions; --at lands between v2 and
        // v3 → pick v2. Key `b` has a single version well before
        // --at → included as-is. Key `c`'s only version is after
        // --at → excluded.
        let entries = vec![
            staged_entry("a.bin", "v1", 10, 100),
            staged_entry("a.bin", "v2", 11, 200),
            staged_entry("a.bin", "v3", 12, 400),
            staged_entry("b.bin", "v1", 20, 50),
            staged_entry("c.bin", "v1", 30, 500),
        ];

        let window = plan_snapshot(entries, 300).unwrap();
        assert_eq!(window.window_start, 300);
        assert_eq!(window.window_end, 300);

        let paths: Vec<&str> = window
            .entries
            .iter()
            .map(|e| e.relative_path.as_str())
            .collect();
        assert_eq!(paths, vec!["b.bin", "a.bin"]);

        let a = window
            .entries
            .iter()
            .find(|e| e.relative_path == "a.bin")
            .unwrap();
        assert_eq!(a.version_id, "v2");
        assert_eq!(a.size, 11);
    }

    #[test]
    fn plan_snapshot_drops_delete_markers() {
        // Key `a`'s latest version at `at` is a delete marker →
        // the key is not present in the commit tree.
        let entries = vec![
            staged_entry("a.bin", "v1", 10, 100),
            delete_marker_entry("a.bin", "v2", 200),
            staged_entry("b.bin", "v1", 20, 150),
        ];
        let window = plan_snapshot(entries, 300).unwrap();
        let paths: Vec<&str> = window
            .entries
            .iter()
            .map(|e| e.relative_path.as_str())
            .collect();
        assert_eq!(paths, vec!["b.bin"]);
    }

    // ── Commit ceiling ─────────────────────────────────────────

    #[test]
    fn commit_ceiling_exceeded_errors_with_counts() {
        // `--window 0` makes every distinct timestamp its own
        // commit; plant 5 timestamps with a ceiling of 3.
        let entries: Vec<ImportEntry> = (0..5)
            .map(|i| staged_entry("a", "", 10, 1_000 + i))
            .collect();

        let err = plan_commit_windows(entries, Duration::ZERO, 3).unwrap_err();
        match err {
            CrabError::ImportCommitCeilingExceeded { planned, ceiling } => {
                assert_eq!(planned, 5);
                assert_eq!(ceiling, 3);
            }
            other => panic!("expected ImportCommitCeilingExceeded, got {other:?}"),
        }
    }

    #[test]
    fn commit_ceiling_at_exact_limit_succeeds() {
        // `planned == ceiling` is allowed — the error only fires
        // when the planner would exceed the ceiling.
        let entries: Vec<ImportEntry> = (0..3)
            .map(|i| staged_entry("a", "", 10, 1_000 + i))
            .collect();
        let windows = plan_commit_windows(entries, Duration::ZERO, 3).unwrap();
        assert_eq!(windows.len(), 3);
    }

    // ── Invalid history range ──────────────────────────────────

    #[test]
    fn validate_history_range_rejects_since_greater_than_until() {
        let err = validate_history_range(Some(1_000), Some(500)).unwrap_err();
        match err {
            CrabError::ImportInvalidHistoryRange { since, until } => {
                assert_eq!(since, "1000");
                assert_eq!(until, "500");
            }
            other => panic!("expected ImportInvalidHistoryRange, got {other:?}"),
        }
    }

    #[test]
    fn validate_history_range_accepts_equal_bounds() {
        // A single-instant window is degenerate but not invalid.
        validate_history_range(Some(500), Some(500)).unwrap();
    }

    #[test]
    fn validate_history_range_accepts_open_bounds() {
        validate_history_range(None, None).unwrap();
        validate_history_range(Some(100), None).unwrap();
        validate_history_range(None, Some(100)).unwrap();
    }

    // ── Empty entries ──────────────────────────────────────────

    #[test]
    fn empty_entries_returns_empty_vec_for_commit_windows() {
        // Documented behavior: `plan_commit_windows` treats empty
        // input as "no work to do" and returns `Ok(vec![])`. The
        // coordinator short-circuits before this in practice.
        let windows = plan_commit_windows(Vec::new(), ONE_HOUR, NO_CEILING).unwrap();
        assert!(windows.is_empty());
    }

    #[test]
    fn empty_entries_errors_for_plan_snapshot() {
        // `plan_snapshot` returns a single CommitWindow; there is
        // no sensible "empty commit" answer, so surface Internal.
        let err = plan_snapshot(Vec::new(), 100).unwrap_err();
        assert!(matches!(err, CrabError::Internal(_)));
    }

    #[test]
    fn empty_entries_errors_for_plan_flat_single_commit() {
        let err = plan_flat_single_commit(Vec::new()).unwrap_err();
        assert!(matches!(err, CrabError::Internal(_)));
    }

    // ── plan_flat_single_commit ────────────────────────────────

    #[test]
    fn plan_flat_single_commit_uses_oldest_start_newest_end() {
        // The commit timestamp is the oldest last_modified
        // (requirements.md open question #12). The end marker is
        // the newest. Entries are sorted.
        let entries = vec![
            staged_entry("z", "", 10, 500),
            staged_entry("a", "", 10, 100),
            staged_entry("m", "", 10, 300),
        ];
        let window = plan_flat_single_commit(entries).unwrap();
        assert_eq!(window.window_start, 100);
        assert_eq!(window.window_end, 500);
        assert_eq!(window.entries.len(), 3);
        let paths: Vec<&str> = window
            .entries
            .iter()
            .map(|e| e.relative_path.as_str())
            .collect();
        assert_eq!(paths, vec!["a", "m", "z"]);
    }

    // ── Ordering within a window ───────────────────────────────

    #[test]
    fn same_timestamp_different_keys_sort_alphabetically() {
        // Tie on last_modified → secondary key is relative_path,
        // ascending.
        let entries = vec![
            staged_entry("c", "", 10, 100),
            staged_entry("a", "", 10, 100),
            staged_entry("b", "", 10, 100),
        ];
        let windows = plan_commit_windows(entries, ONE_HOUR, NO_CEILING).unwrap();
        assert_eq!(windows.len(), 1);
        let paths: Vec<&str> = windows[0]
            .entries
            .iter()
            .map(|e| e.relative_path.as_str())
            .collect();
        assert_eq!(paths, vec!["a", "b", "c"]);
    }

    #[test]
    fn same_key_same_timestamp_sorts_by_version_id_lexicographic() {
        // Tertiary key: version_id. S3 version ids are opaque
        // strings; lexicographic comparison is what makes reruns
        // stable.
        let entries = vec![
            staged_entry("k", "v3", 10, 100),
            staged_entry("k", "v1", 10, 100),
            staged_entry("k", "v2", 10, 100),
        ];
        let windows = plan_commit_windows(entries, ONE_HOUR, NO_CEILING).unwrap();
        assert_eq!(windows.len(), 1);
        let ids: Vec<&str> = windows[0]
            .entries
            .iter()
            .map(|e| e.version_id.as_str())
            .collect();
        assert_eq!(ids, vec!["v1", "v2", "v3"]);
    }

    // ── align_down ─────────────────────────────────────────────

    #[test]
    fn align_down_handles_zero_and_exact_multiples() {
        assert_eq!(align_down(0, 3_600), 0);
        assert_eq!(align_down(3_600, 3_600), 3_600);
        assert_eq!(align_down(7_200, 3_600), 7_200);
    }

    #[test]
    fn align_down_rounds_positive_values_toward_negative_infinity() {
        assert_eq!(align_down(3_599, 3_600), 0);
        assert_eq!(align_down(7_199, 3_600), 3_600);
    }

    #[test]
    fn align_down_rounds_negative_values_toward_negative_infinity() {
        // -1 belongs in the [-3600, 0) bucket, not the [0, 3600)
        // bucket. Rust's integer division rounds toward zero, so
        // we need explicit handling.
        assert_eq!(align_down(-1, 3_600), -3_600);
        assert_eq!(align_down(-3_600, 3_600), -3_600);
        assert_eq!(align_down(-3_601, 3_600), -7_200);
    }
}
