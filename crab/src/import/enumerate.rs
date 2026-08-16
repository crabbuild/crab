//! Enumerate stage for `crab import`.
//!
//! Walks the source bucket (flat or versioned) via the
//! [`VersionedList`] trait, applies the exclude-then-include filter
//! pipeline, classifies entries, and streams them into the resume
//! journal in 1 000-row batches. Downstream stages read from the
//! journal rather than holding the enumeration in memory, so this
//! module only keeps a single batch's worth of rows resident at a
//! time.
//!
//! The caller chooses the dispatch via [`SourceMode`]:
//!
//! - [`SourceMode::Flat`] — `source.enumerate(None, None, cb)`; all
//!   records land with `version_id = ""` and are never delete
//!   markers.
//! - [`SourceMode::Versioned`] — `source.enumerate(since, until, cb)`;
//!   the `since` / `until` bounds are pushed into the backend
//!   where the cloud can do the filtering server-side. A client-
//!   side bounds check runs against every record as a safety net
//!   so records the backend happens to return outside the window
//!   surface as `Skipped(OutsideHistoryWindow)` rather than leaking
//!   into the history plan.
//! - [`SourceMode::SingleSnapshot { at }`] —
//!   `source.enumerate_at(at, cb)`; the `since` / `until` bounds
//!   are not meaningful here (the snapshot timestamp is the bound)
//!   and the client-side check is skipped.
//!
//! # Filter pipeline
//!
//! Apply in this order to the **relative key** (the cloud key with
//! the source prefix stripped — the backend already does that):
//!
//! 1. Trailing `/` → zero-byte directory placeholder. These appear
//!    in consoles that create "folders" as empty objects. They are
//!    never useful to import; we count them and drop them. A single
//!    `debug!` fires on the first hit so logs don't pile up per
//!    key.
//! 2. Path validation → keys must map to a stable worktree path
//!    and must not collide with Git/Crab control files. Rather
//!    than letting the failure surface deep inside assemble we
//!    mark the row [`SkipReason::InvalidGitPath`] here. The check
//!    is a safety net, not a replacement: git itself ultimately
//!    guards the tree-entry write.
//! 3. Exclude globs (if any match → skipped silently, not even
//!    counted in a skip bucket — excludes are deliberate user
//!    intent).
//! 4. Include globs — **empty include means "match everything"**,
//!    diverging from [`crate::core::pattern::PatternFilter`] whose
//!    empty-include case matches nothing. The enumerate contract
//!    mirrors `git log` / `git ls-tree`: no include filter →
//!    everything lands.
//! 5. (Versioned mode only) `last_modified` bounds — records
//!    outside `[since, until]` become
//!    [`SkipReason::OutsideHistoryWindow`].
//!
//! # Progress events
//!
//! A [`ProgressSink`] receives an `enumerate.event` every 1 000
//! records or every 500 ms, whichever fires first. The first event
//! carries `versioning: true` when the source mode is anything
//! other than flat so text-mode consumers can switch progress-bar
//! style; JSONL consumers see the same flag on the wire. A final
//! event is emitted when enumeration finishes or is cancelled.
//!
//! # Cancel-safety
//!
//! The cancellation check runs between journal flushes, not
//! per-record — a tight-loop `is_cancelled()` on every cloud
//! callback is pointless given flushes already happen at a
//! bounded 1 000-row cadence. When cancellation fires we still
//! flush the in-flight batch before exiting so the journal never
//! sits with half a batch's worth of unlanded rows.

use std::collections::HashSet;
use std::time::Instant;

use globset::{Glob, GlobSet, GlobSetBuilder};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, trace};

use crate::core::error::{Result, check_cancelled};
use crate::import::detect::SourceMode;
use crate::import::journal::{EntryState, ImportEntry, Journal, SkipReason};
use crate::import::versions::{VersionRecord, VersionedList};

/// Trait through which enumerate surfaces progress.
///
/// Intentionally minimal for V1 — Task 16 (structured output) will
/// expand this into a full event family. The single method keeps
/// the plumbing honest: callers that don't care about progress use
/// the `impl ProgressSink for ()` blanket and everything compiles.
pub trait ProgressSink {
    /// Deliver one `enumerate.event`.
    fn enumerate_event(&mut self, event: EnumerateEvent);
}

impl ProgressSink for () {
    fn enumerate_event(&mut self, _event: EnumerateEvent) {}
}

/// One progress tick from the enumerate stage.
///
/// `done` is the cumulative count of records the stage has
/// considered (including those it ultimately skipped) — a raw
/// enumeration rate, not a "kept" rate. Callers rendering human
/// progress bars typically want to pair this with the skipped
/// counters in [`EnumerateStats`] for the summary line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumerateEvent {
    /// Total records observed so far.
    pub done: u64,
    /// Cumulative records that survived every filter.
    pub kept: u64,
    /// `true` for versioned / single-snapshot modes, `false` for
    /// flat. Present on every event (not just the first) so late
    /// subscribers still get the signal.
    pub versioning: bool,
    /// Set exactly once on the final event the stage emits.
    pub terminal: bool,
}

/// Totals the stage returns to its caller.
///
/// Task 16 will render these into the structured `ImportSummary`;
/// holding them here keeps the per-reason counters out of the
/// event stream.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnumerateStats {
    /// Records that survived every filter and landed in the
    /// journal as `Pending` (or, for delete markers, still
    /// `Pending` — ingest handles the delete-marker fast path).
    pub kept: u64,
    /// Total bytes across `kept` records.
    pub total_bytes: u64,
    /// Whether the source was enumerated in versioned or single-
    /// snapshot mode. Mirrored on the first progress event.
    pub versioning: bool,
    /// Zero-byte placeholders (`foo/bar/`) dropped on sight.
    pub skipped_directory_placeholders: u64,
    /// Keys containing characters git rejects in tree entries.
    pub skipped_invalid_git_path: u64,
    /// Records whose `last_modified` fell outside the
    /// `--since` / `--until` window.
    pub skipped_outside_window: u64,
    /// Records an exclude or include filter dropped. Not split
    /// further — the difference is not actionable in the summary.
    pub skipped_filtered: u64,
}

/// Enumerate the source bucket into the journal.
///
/// Returns totals for the coordinator to fold into its summary.
/// On cancellation, the in-flight batch flushes first and then
/// the stage returns [`CrabError::Cancelled`].
///
/// # Arguments
///
/// - `source` — per-backend `VersionedList` (Local for `file://`
///   and tests, cloud backends via `VersionedListImpl`).
/// - `mode` — decision from the detect stage.
/// - `include`, `exclude` — compiled glob lists. An empty
///   `include` slice matches every key. Apply exclude first.
/// - `since`, `until` — epoch-second bounds applied client-side
///   to catch records the backend happens to return outside the
///   window.
/// - `journal` — open resume journal; batches land as
///   `Pending` / `Skipped` rows.
/// - `cancel` — checked between batches.
/// - `progress` — sink for `enumerate.event` ticks.
///
/// [`CrabError::Cancelled`]: crate::core::error::CrabError::Cancelled
#[expect(
    clippy::too_many_arguments,
    reason = "pipeline stage entry point; grouping would obscure which are filters vs outputs"
)]
pub async fn enumerate<'a>(
    source: &'a dyn VersionedList,
    mode: SourceMode,
    include: &'a [String],
    exclude: &'a [String],
    since: Option<i64>,
    until: Option<i64>,
    journal: &'a mut Journal,
    cancel: &'a CancellationToken,
    progress: &'a mut (dyn ProgressSink + Send),
) -> Result<EnumerateStats> {
    let include_set = compile_glob_set(include)?;
    let exclude_set = compile_glob_set(exclude)?;
    let include_empty = include.is_empty();

    let versioning = !matches!(mode, SourceMode::Flat);

    let mut state = EnumerateState::new(versioning, cancel, journal, progress);

    // Callback must be `FnMut + Send` to satisfy the trait. We
    // borrow `state` mutably rather than moving it so the
    // post-walk flush can still reach the buffers and progress
    // sink. The closure surfaces cancellation as a `Cancelled`
    // error that short-circuits the backend walk; the outer
    // post-walk path flushes the in-flight batch regardless of
    // which exit path we take.
    let walk_result: Result<()> = {
        let state_ref = &mut state;
        let include_set_ref = &include_set;
        let exclude_set_ref = &exclude_set;

        let mut cb = move |rec: VersionRecord| -> Result<()> {
            let rec_outcome = classify_record(
                &rec,
                include_empty,
                include_set_ref,
                exclude_set_ref,
                since,
                until,
                versioning,
            );
            state_ref.observe(rec, rec_outcome);

            // Flush + cancellation check happen at the batch
            // boundary. Per-record cancellation checks on a hot
            // callback path buy nothing — the cloud backends batch
            // at ~1 000 keys per paginated response anyway.
            if state_ref.pending.len() + state_ref.skipped.len() >= BATCH_SIZE {
                state_ref.flush_journal()?;
                state_ref.tick_progress(false);
                check_cancelled(state_ref.cancel)?;
            } else if state_ref.should_tick_on_time() {
                state_ref.tick_progress(false);
            }
            Ok(())
        };

        match mode {
            SourceMode::Flat => source.enumerate(None, None, &mut cb).await,
            SourceMode::Versioned => source.enumerate(since, until, &mut cb).await,
            SourceMode::SingleSnapshot { at } => source.enumerate_at(at, &mut cb).await,
        }
    };

    // Drain whatever landed in the buffer, whether the walk
    // completed or we're unwinding a cancellation. The journal
    // must never see half a batch of unlanded rows — a later
    // `--resume` has to find a consistent state regardless of
    // which path got us here.
    state.flush_journal()?;
    state.tick_progress(true);

    walk_result?;

    info!(
        kept = state.stats.kept,
        total_bytes = state.stats.total_bytes,
        skipped_directory_placeholders = state.stats.skipped_directory_placeholders,
        skipped_invalid_git_path = state.stats.skipped_invalid_git_path,
        skipped_outside_window = state.stats.skipped_outside_window,
        skipped_filtered = state.stats.skipped_filtered,
        versioning = state.stats.versioning,
        "enumerate stage complete"
    );

    Ok(state.stats)
}

/// Target batch size for journal upserts + progress ticks.
const BATCH_SIZE: usize = 1_000;

/// Wall-clock cap between progress ticks.
const PROGRESS_INTERVAL_MS: u128 = 500;

/// Outcome of classifying one [`VersionRecord`] against the filter
/// pipeline. Converted into journal rows by the state machine.
enum RecordOutcome {
    /// Keep the record as a Pending row for ingest.
    Keep,
    /// Drop silently (matched an exclude rule or fell outside the
    /// include set). Counted under `skipped_filtered`.
    Filtered,
    /// Skipped under a structured reason the summary surfaces.
    SkippedWithReason(SkipReason),
}

/// Per-enumeration mutable state — buffers, counters, last tick,
/// cancel token, and references to the journal + progress sink.
/// Held behind a `&mut` borrow so the callback closure can mutate
/// it while the outer function reaches it again after the walk.
struct EnumerateState<'a> {
    stats: EnumerateStats,
    /// Kept rows pending a journal flush. Never held across
    /// `.await` (the callback closure is sync).
    pending: Vec<ImportEntry>,
    /// Skipped rows pending a journal flush. Tracked separately
    /// only to keep the stats counters honest when a caller wants
    /// to inspect them in tests without draining `pending`.
    skipped: Vec<ImportEntry>,
    /// Cumulative records observed. Mirrored to progress events.
    records_observed: u64,
    last_tick: Instant,
    ticks_emitted: u64,
    cancel: &'a CancellationToken,
    journal: &'a mut Journal,
    progress: &'a mut (dyn ProgressSink + Send),
    /// Dedupe the directory-placeholder debug log so a bucket
    /// full of `foo/bar/` keys doesn't spam the log.
    directory_keys_warned: bool,
    /// Track duplicate `(path, version_id)` pairs within a single
    /// enumerate pass. The journal's `INSERT OR REPLACE` would
    /// swallow dupes silently; surfacing them at debug level
    /// helps chasing "why is my version count smaller than the
    /// cloud console"? questions.
    seen_keys: HashSet<(String, String)>,
}

impl<'a> EnumerateState<'a> {
    fn new(
        versioning: bool,
        cancel: &'a CancellationToken,
        journal: &'a mut Journal,
        progress: &'a mut (dyn ProgressSink + Send),
    ) -> Self {
        Self {
            stats: EnumerateStats {
                versioning,
                ..EnumerateStats::default()
            },
            pending: Vec::with_capacity(BATCH_SIZE),
            skipped: Vec::with_capacity(BATCH_SIZE / 4),
            records_observed: 0,
            last_tick: Instant::now(),
            ticks_emitted: 0,
            cancel,
            journal,
            progress,
            directory_keys_warned: false,
            seen_keys: HashSet::new(),
        }
    }

    fn observe(&mut self, rec: VersionRecord, outcome: RecordOutcome) {
        self.records_observed = self.records_observed.saturating_add(1);

        match outcome {
            RecordOutcome::Keep => {
                let key = (rec.key.clone(), rec.version_id.clone());
                if !self.seen_keys.insert(key) {
                    // Duplicate pair within the same pass —
                    // possible for cloud backends that emit
                    // overlapping pages or clients that retry.
                    // INSERT OR REPLACE in the journal keeps the
                    // final state consistent; the debug line
                    // gives operators a breadcrumb.
                    debug!(
                        key = %rec.key,
                        version_id = %rec.version_id,
                        "enumerate: duplicate (key, version_id) observed"
                    );
                }

                let size = rec.size;
                self.pending
                    .push(version_record_to_entry(rec, EntryState::Pending));
                self.stats.kept = self.stats.kept.saturating_add(1);
                self.stats.total_bytes = self.stats.total_bytes.saturating_add(size);
            }
            RecordOutcome::Filtered => {
                self.stats.skipped_filtered = self.stats.skipped_filtered.saturating_add(1);
                trace!(key = %rec.key, "enumerate: dropped by include/exclude");
            }
            RecordOutcome::SkippedWithReason(reason) => {
                match &reason {
                    SkipReason::ZeroByteDirectoryKey => {
                        if !self.directory_keys_warned {
                            debug!(key = %rec.key, "enumerate: dropping zero-byte directory placeholder keys (logged once per run)");
                            self.directory_keys_warned = true;
                        }
                        self.stats.skipped_directory_placeholders =
                            self.stats.skipped_directory_placeholders.saturating_add(1);
                    }
                    SkipReason::InvalidGitPath => {
                        self.stats.skipped_invalid_git_path =
                            self.stats.skipped_invalid_git_path.saturating_add(1);
                    }
                    SkipReason::OutsideHistoryWindow => {
                        self.stats.skipped_outside_window =
                            self.stats.skipped_outside_window.saturating_add(1);
                    }
                    // LfsPointer is an ingest-stage decision, not
                    // enumerate's. Landing here would be a
                    // programmer bug; fold it into the filtered
                    // bucket rather than crashing.
                    SkipReason::LfsPointer => {
                        self.stats.skipped_filtered = self.stats.skipped_filtered.saturating_add(1);
                    }
                }
                self.skipped
                    .push(version_record_to_entry(rec, EntryState::Skipped { reason }));
            }
        }
    }

    fn flush_journal(&mut self) -> Result<()> {
        if self.pending.is_empty() && self.skipped.is_empty() {
            return Ok(());
        }
        // Combined upsert — one transaction for both buckets
        // keeps the journal's row count monotonic from the
        // observer's perspective. Capacity stays pinned to
        // BATCH_SIZE so the next round reuses the allocation.
        let mut batch = Vec::with_capacity(self.pending.len() + self.skipped.len());
        batch.append(&mut self.pending);
        batch.append(&mut self.skipped);
        self.journal.upsert_entry_batch(&batch)?;
        Ok(())
    }

    fn should_tick_on_time(&self) -> bool {
        self.last_tick.elapsed().as_millis() >= PROGRESS_INTERVAL_MS
    }

    fn tick_progress(&mut self, terminal: bool) {
        self.progress.enumerate_event(EnumerateEvent {
            done: self.records_observed,
            kept: self.stats.kept,
            versioning: self.stats.versioning,
            terminal,
        });
        self.last_tick = Instant::now();
        self.ticks_emitted = self.ticks_emitted.saturating_add(1);
    }
}

fn classify_record(
    rec: &VersionRecord,
    include_empty: bool,
    include_set: &GlobSet,
    exclude_set: &GlobSet,
    since: Option<i64>,
    until: Option<i64>,
    versioning: bool,
) -> RecordOutcome {
    // Skip zero-byte directory placeholders regardless of filter
    // state. A user-supplied exclude for `*/` would never see
    // them because we drop here first; the counter is the only
    // signal.
    if rec.key.ends_with('/') {
        return RecordOutcome::SkippedWithReason(SkipReason::ZeroByteDirectoryKey);
    }

    if !crate::import::is_importable_relative_path(&rec.key) {
        return RecordOutcome::SkippedWithReason(SkipReason::InvalidGitPath);
    }

    // Exclude first — matching an exclude shadows any include.
    if exclude_set.is_match(&rec.key) {
        return RecordOutcome::Filtered;
    }

    // Empty include = match everything, per module docstring.
    if !include_empty && !include_set.is_match(&rec.key) {
        return RecordOutcome::Filtered;
    }

    // Versioned client-side bounds safety net. Only meaningful
    // when the caller intends a time-bounded walk; single-
    // snapshot mode collapses the timeline to the `at`
    // timestamp and has no window to validate here.
    if versioning {
        if let Some(min) = since
            && rec.last_modified < min
        {
            return RecordOutcome::SkippedWithReason(SkipReason::OutsideHistoryWindow);
        }
        if let Some(max) = until
            && rec.last_modified > max
        {
            return RecordOutcome::SkippedWithReason(SkipReason::OutsideHistoryWindow);
        }
    }

    RecordOutcome::Keep
}

/// Convert a backend-provided version record into a journal row.
fn version_record_to_entry(rec: VersionRecord, state: EntryState) -> ImportEntry {
    ImportEntry {
        relative_path: rec.key,
        version_id: rec.version_id,
        size: rec.size,
        etag: rec.etag,
        last_modified: rec.last_modified,
        is_delete_marker: rec.is_delete_marker,
        state,
    }
}

fn compile_glob_set(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for p in patterns {
        builder.add(Glob::new(p)?);
    }
    Ok(builder.build()?)
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

    use std::fs;
    use std::sync::Arc;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use tempfile::TempDir;

    use crate::import::journal::Journal;
    use crate::import::versions::{
        LocalVersionedList, VersionRecord, VersionSample, VersionedList,
    };

    /// Progress sink that records every event for assertions.
    #[derive(Default, Clone)]
    struct RecordingSink {
        events: Arc<Mutex<Vec<EnumerateEvent>>>,
    }

    impl RecordingSink {
        fn events(&self) -> Vec<EnumerateEvent> {
            self.events.lock().unwrap().clone()
        }
    }

    impl ProgressSink for RecordingSink {
        fn enumerate_event(&mut self, event: EnumerateEvent) {
            self.events.lock().unwrap().push(event);
        }
    }

    fn strings(vs: &[&str]) -> Vec<String> {
        vs.iter().map(|s| (*s).to_owned()).collect()
    }

    // --- 8.7 integration test: flat, ~100 objects ---

    #[tokio::test]
    async fn flat_enumerate_lands_all_records_in_journal() {
        // Seed a tempdir with a synthetic bucket, dispatch to
        // Flat mode, and assert the journal ends up with one
        // Pending row per input file.
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir_all(&src).unwrap();

        // Spread a couple of non-eligible entries in too so we
        // exercise the skip paths end-to-end. Directory
        // placeholders are cloud-side creations; the local
        // backend can't produce one, so exercise that case in a
        // focused mock test below.
        for i in 0..100 {
            fs::write(src.join(format!("file-{i:03}.bin")), [0u8; 16]).unwrap();
        }
        // One file at a nested path — exercises the key
        // normalization.
        fs::create_dir_all(src.join("nested")).unwrap();
        fs::write(src.join("nested/deep.bin"), [0u8; 16]).unwrap();

        let into = tmp.path().join("repo");
        fs::create_dir_all(&into).unwrap();
        let mut journal = Journal::open(&into).unwrap();

        let list = LocalVersionedList::new(src.clone());
        let cancel = CancellationToken::new();
        let mut sink = RecordingSink::default();

        let stats = enumerate(
            &list,
            SourceMode::Flat,
            &[],
            &[],
            None,
            None,
            &mut journal,
            &cancel,
            &mut sink,
        )
        .await
        .unwrap();

        assert_eq!(stats.kept, 101);
        assert!(!stats.versioning);
        assert_eq!(stats.skipped_directory_placeholders, 0);
        assert_eq!(stats.skipped_invalid_git_path, 0);

        // Journal row count must match kept + skipped. The only
        // way to probe is via the iter helper.
        let mut count = 0;
        journal
            .iter_entries_sorted_by_time(|_e| {
                count += 1;
                Ok(())
            })
            .unwrap();
        assert_eq!(count, 101);

        // A terminal event must have fired.
        let events = sink.events();
        assert!(
            events.iter().any(|e| e.terminal),
            "expected a terminal event, got {events:?}"
        );
    }

    #[tokio::test]
    async fn flat_enumerate_applies_exclude_then_include() {
        // Exclude takes precedence over include — a key matching
        // both must drop. An empty include matches everything.
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir_all(&src).unwrap();

        fs::write(src.join("keep.bin"), b"x").unwrap();
        fs::write(src.join("drop.tmp"), b"x").unwrap();
        fs::write(src.join("also.bin"), b"x").unwrap();
        fs::create_dir_all(src.join("tmp")).unwrap();
        fs::write(src.join("tmp/cache.bin"), b"x").unwrap();

        let into = tmp.path().join("repo");
        fs::create_dir_all(&into).unwrap();
        let mut journal = Journal::open(&into).unwrap();

        let list = LocalVersionedList::new(src.clone());
        let cancel = CancellationToken::new();
        let mut sink = RecordingSink::default();

        let stats = enumerate(
            &list,
            SourceMode::Flat,
            &strings(&["*.bin", "**/*.bin"]),
            &strings(&["tmp/**", "*.tmp"]),
            None,
            None,
            &mut journal,
            &cancel,
            &mut sink,
        )
        .await
        .unwrap();

        // keep.bin + also.bin survive. drop.tmp filtered by
        // exclude. tmp/cache.bin filtered by the tmp/** exclude.
        assert_eq!(stats.kept, 2);
        assert_eq!(stats.skipped_filtered, 2);
    }

    // --- 8.8 integration test: versioned mock ---

    /// Minimal in-memory `VersionedList` for versioned-mode tests.
    struct MockVersioned {
        records: Vec<VersionRecord>,
    }

    #[async_trait]
    impl VersionedList for MockVersioned {
        async fn sample(&self, limit: usize) -> Result<VersionSample> {
            let records: Vec<VersionRecord> = self.records.iter().take(limit).cloned().collect();
            let unique: HashSet<&str> = records.iter().map(|r| r.key.as_str()).collect();
            Ok(VersionSample {
                total_versions: records.len(),
                unique_keys: unique.len(),
                has_delete_markers: records.iter().any(|r| r.is_delete_marker),
                records,
            })
        }

        async fn enumerate(
            &self,
            since: Option<i64>,
            until: Option<i64>,
            callback: &mut (dyn FnMut(VersionRecord) -> Result<()> + Send),
        ) -> Result<()> {
            // Mirror the real backends' server-side bounds push.
            for rec in &self.records {
                if let Some(min) = since {
                    if rec.last_modified < min {
                        continue;
                    }
                }
                if let Some(max) = until {
                    if rec.last_modified > max {
                        continue;
                    }
                }
                callback(rec.clone())?;
            }
            Ok(())
        }

        async fn enumerate_at(
            &self,
            _at: i64,
            callback: &mut (dyn FnMut(VersionRecord) -> Result<()> + Send),
        ) -> Result<()> {
            for rec in &self.records {
                callback(rec.clone())?;
            }
            Ok(())
        }
    }

    fn ver(key: &str, version_id: &str, size: u64, ts: i64, dm: bool) -> VersionRecord {
        VersionRecord {
            key: key.into(),
            version_id: version_id.into(),
            size,
            etag: None,
            last_modified: ts,
            is_delete_marker: dm,
        }
    }

    #[tokio::test]
    async fn versioned_enumerate_lands_every_version_with_metadata() {
        // Five keys × 3 versions + 1 delete marker on the first
        // key. Journal must end up with 16 rows, preserving
        // version_id, is_delete_marker, and last_modified on
        // every row.
        let mut recs = Vec::new();
        for k in 0u32..5 {
            for v in 0u32..3 {
                recs.push(ver(
                    &format!("k{k}.bin"),
                    &format!("v{v}"),
                    100 + u64::from(v) * 10,
                    1_000 + (i64::from(k) * 100) + i64::from(v),
                    false,
                ));
            }
        }
        // Delete marker on k0.bin at a later timestamp.
        recs.push(ver("k0.bin", "v3", 0, 2_000, true));

        let mock = MockVersioned { records: recs };

        let tmp = TempDir::new().unwrap();
        let into = tmp.path().join("repo");
        fs::create_dir_all(&into).unwrap();
        let mut journal = Journal::open(&into).unwrap();

        let cancel = CancellationToken::new();
        let mut sink = RecordingSink::default();

        let stats = enumerate(
            &mock,
            SourceMode::Versioned,
            &[],
            &[],
            None,
            None,
            &mut journal,
            &cancel,
            &mut sink,
        )
        .await
        .unwrap();

        assert_eq!(stats.kept, 16);
        assert!(stats.versioning);

        let mut rows = Vec::new();
        journal
            .iter_entries_sorted_by_time(|e| {
                rows.push(e);
                Ok(())
            })
            .unwrap();
        assert_eq!(rows.len(), 16);

        // The delete marker must survive with is_delete_marker
        // set — assemble downstream needs it.
        let delete = rows
            .iter()
            .find(|r| r.is_delete_marker)
            .expect("delete marker row");
        assert_eq!(delete.relative_path, "k0.bin");
        assert_eq!(delete.version_id, "v3");
        assert_eq!(delete.last_modified, 2_000);

        // Spot-check a non-delete row carries its version id and
        // timestamp verbatim.
        let k1v2 = rows
            .iter()
            .find(|r| r.relative_path == "k1.bin" && r.version_id == "v2")
            .expect("k1 v2 row");
        assert_eq!(k1v2.last_modified, 1_000 + 100 + 2);
        assert_eq!(k1v2.size, 100 + 20);
    }

    #[tokio::test]
    async fn versioned_enumerate_client_side_since_until_safety_net() {
        // The backend honors bounds; the client-side filter is
        // still active. Use a mock that ignores bounds to prove
        // the safety net fires.
        struct LaxMock {
            records: Vec<VersionRecord>,
        }

        #[async_trait]
        impl VersionedList for LaxMock {
            async fn sample(&self, _limit: usize) -> Result<VersionSample> {
                unreachable!("not used in this test")
            }

            async fn enumerate(
                &self,
                _since: Option<i64>,
                _until: Option<i64>,
                callback: &mut (dyn FnMut(VersionRecord) -> Result<()> + Send),
            ) -> Result<()> {
                // Deliberately ignore bounds — the enumerate
                // stage's client-side filter must still trim.
                for rec in &self.records {
                    callback(rec.clone())?;
                }
                Ok(())
            }

            async fn enumerate_at(
                &self,
                _at: i64,
                _callback: &mut (dyn FnMut(VersionRecord) -> Result<()> + Send),
            ) -> Result<()> {
                unreachable!("not used in this test")
            }
        }

        let recs = vec![
            ver("a.bin", "v1", 10, 100, false),
            ver("a.bin", "v2", 10, 500, false),
            ver("a.bin", "v3", 10, 900, false),
        ];
        let mock = LaxMock { records: recs };

        let tmp = TempDir::new().unwrap();
        let into = tmp.path().join("repo");
        fs::create_dir_all(&into).unwrap();
        let mut journal = Journal::open(&into).unwrap();

        let cancel = CancellationToken::new();
        let mut sink = RecordingSink::default();

        let stats = enumerate(
            &mock,
            SourceMode::Versioned,
            &[],
            &[],
            Some(200),
            Some(800),
            &mut journal,
            &cancel,
            &mut sink,
        )
        .await
        .unwrap();

        // v1 at 100 and v3 at 900 are outside [200, 800]; v2 at
        // 500 survives.
        assert_eq!(stats.kept, 1);
        assert_eq!(stats.skipped_outside_window, 2);
    }

    #[tokio::test]
    async fn directory_placeholder_keys_are_skipped_with_a_counter() {
        // Mock a bucket that emits a `dir/` key alongside real
        // objects — the local backend can't produce these.
        let recs = vec![
            ver("real.bin", "", 10, 1, false),
            ver("dir/", "", 0, 2, false),
            ver("also-real.bin", "", 10, 3, false),
        ];
        let mock = MockVersioned { records: recs };

        let tmp = TempDir::new().unwrap();
        let into = tmp.path().join("repo");
        fs::create_dir_all(&into).unwrap();
        let mut journal = Journal::open(&into).unwrap();

        let cancel = CancellationToken::new();
        let mut sink = RecordingSink::default();

        let stats = enumerate(
            &mock,
            SourceMode::Flat,
            &[],
            &[],
            None,
            None,
            &mut journal,
            &cancel,
            &mut sink,
        )
        .await
        .unwrap();

        assert_eq!(stats.kept, 2);
        assert_eq!(stats.skipped_directory_placeholders, 1);
    }

    #[tokio::test]
    async fn invalid_git_path_keys_are_rejected() {
        let recs = vec![
            ver("good.bin", "", 10, 1, false),
            ver("data/model..v2.bin", "", 10, 2, false),
            ver("bad\nkey.bin", "", 10, 2, false),
            ver("bad\0key.bin", "", 10, 3, false),
            ver("data/../escape.bin", "", 10, 4, false),
            ver(".crab/staging/index.db", "", 10, 5, false),
            ver(".git/config", "", 10, 6, false),
            ver(".gitattributes", "", 10, 7, false),
        ];
        let mock = MockVersioned { records: recs };

        let tmp = TempDir::new().unwrap();
        let into = tmp.path().join("repo");
        fs::create_dir_all(&into).unwrap();
        let mut journal = Journal::open(&into).unwrap();

        let cancel = CancellationToken::new();
        let mut sink = RecordingSink::default();

        let stats = enumerate(
            &mock,
            SourceMode::Flat,
            &[],
            &[],
            None,
            None,
            &mut journal,
            &cancel,
            &mut sink,
        )
        .await
        .unwrap();

        assert_eq!(stats.kept, 2);
        assert_eq!(stats.skipped_invalid_git_path, 6);
    }

    #[tokio::test]
    async fn cancellation_flushes_in_flight_batch_before_exit() {
        // A cancel mid-walk must not leave orphan rows — the
        // post-walk flush guarantees the journal reflects
        // everything observed up to the cancel.
        struct CancelAfterN {
            records: Vec<VersionRecord>,
            cancel: CancellationToken,
            threshold: usize,
        }

        #[async_trait]
        impl VersionedList for CancelAfterN {
            async fn sample(&self, _limit: usize) -> Result<VersionSample> {
                unreachable!("not used")
            }

            async fn enumerate(
                &self,
                _since: Option<i64>,
                _until: Option<i64>,
                callback: &mut (dyn FnMut(VersionRecord) -> Result<()> + Send),
            ) -> Result<()> {
                for (i, rec) in self.records.iter().enumerate() {
                    if i == self.threshold {
                        self.cancel.cancel();
                    }
                    callback(rec.clone())?;
                }
                Ok(())
            }

            async fn enumerate_at(
                &self,
                _at: i64,
                _callback: &mut (dyn FnMut(VersionRecord) -> Result<()> + Send),
            ) -> Result<()> {
                unreachable!("not used")
            }
        }

        // Seed enough rows to cross at least two BATCH_SIZE
        // boundaries so the cancel check between batches has a
        // chance to fire after the mock signals cancellation.
        let total = BATCH_SIZE * 3;
        let mut recs = Vec::with_capacity(total);
        for i in 0..total {
            recs.push(ver(
                &format!("f{i:06}.bin"),
                "",
                1,
                i64::try_from(i).unwrap(),
                false,
            ));
        }

        let cancel = CancellationToken::new();
        let mock = CancelAfterN {
            records: recs,
            cancel: cancel.clone(),
            // Fire cancel just past the first batch so the
            // post-batch cancel check (at record BATCH_SIZE * 2)
            // surfaces the cancellation. The post-walk flush
            // then lands whatever the in-flight batch holds.
            threshold: BATCH_SIZE + 10,
        };

        let tmp = TempDir::new().unwrap();
        let into = tmp.path().join("repo");
        fs::create_dir_all(&into).unwrap();
        let mut journal = Journal::open(&into).unwrap();

        let mut sink = RecordingSink::default();
        let err = enumerate(
            &mock,
            SourceMode::Flat,
            &[],
            &[],
            None,
            None,
            &mut journal,
            &cancel,
            &mut sink,
        )
        .await
        .expect_err("cancellation must surface");
        assert!(matches!(err, crate::core::error::CrabError::Cancelled));

        // Every row the callback saw before the batch boundary
        // (BATCH_SIZE rows) must have landed from the in-flight
        // flush. The exact count depends on where the cancel
        // fired relative to the batch boundary, but we must see
        // at least BATCH_SIZE rows — the batch flush fires
        // before the cancel check.
        let mut count = 0;
        journal
            .iter_entries_sorted_by_time(|_| {
                count += 1;
                Ok(())
            })
            .unwrap();
        assert!(
            count >= BATCH_SIZE,
            "expected at least {BATCH_SIZE} rows flushed, got {count}"
        );
    }

    #[tokio::test]
    async fn first_event_carries_versioning_flag() {
        // The first emitted event (and every subsequent event)
        // must carry the versioning flag — downstream consumers
        // rely on it to switch progress bar styles.
        let recs = vec![ver("a.bin", "v1", 10, 100, false)];
        let mock = MockVersioned { records: recs };

        let tmp = TempDir::new().unwrap();
        let into = tmp.path().join("repo");
        fs::create_dir_all(&into).unwrap();
        let mut journal = Journal::open(&into).unwrap();

        let cancel = CancellationToken::new();
        let mut sink = RecordingSink::default();

        enumerate(
            &mock,
            SourceMode::Versioned,
            &[],
            &[],
            None,
            None,
            &mut journal,
            &cancel,
            &mut sink,
        )
        .await
        .unwrap();

        let events = sink.events();
        assert!(!events.is_empty(), "at least one event must fire");
        assert!(
            events.iter().all(|e| e.versioning),
            "every event must carry versioning=true in Versioned mode"
        );
    }

    #[tokio::test]
    async fn empty_include_matches_everything() {
        // Enumerate's contract diverges from core PatternFilter:
        // empty include matches every key. Guard the invariant.
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        fs::create_dir_all(&src).unwrap();
        for name in &["a.bin", "b.txt", "c.md", "d.log"] {
            fs::write(src.join(name), b"x").unwrap();
        }

        let into = tmp.path().join("repo");
        fs::create_dir_all(&into).unwrap();
        let mut journal = Journal::open(&into).unwrap();

        let list = LocalVersionedList::new(src.clone());
        let cancel = CancellationToken::new();
        let mut sink = RecordingSink::default();

        let stats = enumerate(
            &list,
            SourceMode::Flat,
            &[],
            &[],
            None,
            None,
            &mut journal,
            &cancel,
            &mut sink,
        )
        .await
        .unwrap();

        assert_eq!(stats.kept, 4);
    }
}
