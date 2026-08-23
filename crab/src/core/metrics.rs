//! Lock-free performance counters for the data plane.
//!
//! [`Metrics`] is cheaply shareable (`Arc`-backed via [`AppContext`]) and
//! uses only [`AtomicU64`] with [`Relaxed`] ordering — no locks, no
//! contention on the hot path.
//!
//! Subsystems call `inc_*()` to bump counters; diagnostic commands call
//! [`Metrics::snapshot`] to read a consistent-ish point-in-time view.

use std::fmt;
use std::fs::OpenOptions;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use fs4::fs_std::FileExt as LockFileExt;
use serde::{Deserialize, Serialize};

use super::error::{CrabError, Result};

/// Plain-data snapshot of all perf counters at a point in time.
///
/// Returned by [`Metrics::snapshot`]. All values are monotonically
/// increasing totals since process start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MetricsSnapshot {
    /// Chunk-existence queries answered by the persistent on-disk index.
    pub chunk_index_persistent_hits: u64,
    /// Total bloom filter queries issued against shard blooms.
    pub shard_bloom_queries: u64,
    /// Bloom queries that returned a hit but the shard did not contain the hash.
    pub shard_bloom_false_positives: u64,
    /// Cumulative staging compression ratio (scaled by 1000 for integer math).
    pub staging_compression_ratio: u64,
    /// Effective adaptive dedup threshold (scaled by 1000 for integer math).
    pub adaptive_threshold_effective: u64,
    /// Total chunks looked up in the `ChunkIndex`.
    pub chunk_index_lookups: u64,
    /// Chunks found in the `ChunkIndex` (dedup hits).
    pub chunk_index_hits: u64,
    /// Shards installed into the `ChunkIndex`.
    pub chunk_index_shards_installed: u64,
    /// Total entries currently in the `ChunkIndex`.
    pub chunk_index_entries: u64,
    /// Segments sealed in the staging area.
    pub staging_segments_sealed: u64,
    /// Segments compacted in the staging area.
    pub staging_segments_compacted: u64,
    /// Total bytes written to staging segments.
    pub staging_bytes_written: u64,
    /// Total bytes read from staging segments.
    pub staging_bytes_read: u64,
    /// Total fsync calls issued by the staging area.
    pub staging_fsyncs: u64,
    /// Compaction attempts skipped because a push was inflight.
    pub staging_compactions_skipped_inflight: u64,
    /// Times the streaming push pipeline detected backpressure (channel full).
    pub push_stream_backpressure_events: u64,
    /// Number of LIST requests issued during batched HEAD resume checks.
    pub head_list_requests: u64,
    /// Number of individual HEAD requests issued during resume checks.
    pub head_point_requests: u64,
    /// Number of xorbs whose HEAD/LIST resume check exhausted retries on a
    /// transient error and were conservatively treated as needing upload.
    pub head_check_errors: u64,
    /// Number of xorbs skipped because they already exist on the remote.
    pub xorbs_skipped: u64,
    /// Number of times the pipelined (parallel) CAS commit path was used.
    pub cas_pipelined_commits: u64,
    /// Number of times the clean fast path was taken (bloom hit + HEAD 200).
    pub clean_fastpath_taken: u64,
    /// Number of clean fast-path bloom hits that turned out to be false positives.
    pub clean_fastpath_false_positives: u64,
    /// Hydrations that used the shard-hint fast path successfully.
    pub shard_hint_hits: u64,
    /// Hydrations where the shard-hint was stale/missing and fell back to the file-index.
    pub shard_hint_misses: u64,
    /// Prefetch reconstructions submitted to the filter-process prefetch queue.
    pub prefetch_started: u64,
    /// Prefetch reconstructions that completed successfully.
    pub prefetch_completed: u64,
    /// Total bytes produced by completed prefetch reconstructions.
    pub prefetch_bytes: u64,
    /// Prefetch submissions satisfied entirely from the local chunk cache
    /// without issuing a remote fetch.
    pub prefetch_cache_hits: u64,
    /// Xorb fetch requests saved by cross-file coalescing in delayed smudge.
    pub xorb_fetch_requests_coalesced: u64,
    /// Bytes saved by cross-file coalescing (avoided re-fetches).
    pub xorb_fetch_bytes_saved: u64,
    /// Number of files in the most recent smudge batch.
    pub smudge_batch_size: u64,
    /// Number of multipart uploads resumed from a previous interrupted push.
    pub multipart_resumed_uploads: u64,

    /// Current upload concurrency level (gauge, not monotonic).
    pub upload_concurrency_current: u64,

    // --- deduplication byte counters ---
    /// Total bytes processed by the deduplication pipeline.
    pub dedup_total_bytes: u64,
    /// Bytes saved by deduplication (existing chunks skipped).
    pub dedup_saved_bytes: u64,
    /// New bytes that required upload (not deduplicated).
    pub dedup_new_bytes: u64,
    /// Bytes that could have been deduplicated but were kept for read locality.
    pub dedup_defrag_prevented_bytes: u64,
    /// Bytes deduplicated via the global dedup path.
    pub dedup_global_bytes: u64,

    // --- FUSE operation counters ---
    /// Total FUSE read operations served.
    pub fuse_read_count: u64,
    /// Total bytes served via FUSE read operations.
    pub fuse_read_bytes: u64,
    /// Current depth of the hydration priority queue.
    pub hydration_queue_depth: u64,
    /// Cache hit count for FUSE/hydration chunk lookups.
    pub cache_hits: u64,
    /// Cache miss count for FUSE/hydration chunk lookups.
    pub cache_misses: u64,

    // --- xet-core xorb-range DiskCache counters ---
    /// Hits reported by the xet-core `DiskCache` (shared across all
    /// `FileReconstructor` instances) at the last snapshot.
    pub chunk_cache_hits: u64,
    /// Misses reported by the xet-core `DiskCache` at the last snapshot.
    pub chunk_cache_misses: u64,
    /// Total bytes currently resident in the xet-core `DiskCache`.
    pub chunk_cache_bytes: u64,

    // --- compression counters ---
    /// Chunks compressed with plain LZ4.
    pub chunks_compressed: u64,
    /// Chunks compressed with BG4 byte-grouping + LZ4.
    pub chunks_bg4_transformed: u64,
    /// Chunks stored without compression.
    pub chunks_stored_raw: u64,
    /// Total bytes saved by compression (uncompressed − compressed).
    pub compression_bytes_saved: u64,

    // --- operation-level counters ---
    /// Total push wall-clock time in milliseconds.
    pub push_duration_ms: u64,
    /// Total xorb payload bytes uploaded during push operations.
    ///
    /// Excludes Git packs, indexes, manifests, refs, locks, and metadb objects.
    pub bytes_uploaded: u64,
    /// Total fetch wall-clock time in milliseconds.
    pub fetch_duration_ms: u64,
    /// Total bytes downloaded during fetch operations.
    pub bytes_downloaded: u64,
    /// GC wall-clock time in milliseconds.
    pub gc_duration_ms: u64,
    /// Number of objects deleted by GC.
    pub gc_objects_deleted: u64,

    // --- import counters ---
    /// Unique file paths present in the final HEAD tree after assemble.
    pub import_files_total: u64,
    /// Total entries across every commit window (> `import_files_total`
    /// for versioned mode where one path contributes multiple versions).
    pub import_versions_total: u64,
    /// Git commits produced by the assemble stage.
    pub import_commits_total: u64,
    /// Summed source-object byte sizes across staged entries.
    pub import_bytes_source_total: u64,
    /// Summed bytes written to the staging area during ingest.
    pub import_bytes_staged_total: u64,
    /// Entries marked `Failed` by the ingest stage.
    pub import_failures_total: u64,

    // --- workflow counters ---
    /// Workflow stages that ran the user command to successful
    /// `Committed` (miss-path completions).
    pub workflow_stages_executed: u64,
    /// Workflow stage cache hits served from the local chunk cache.
    pub workflow_stage_cache_hits_local: u64,
    /// Workflow stage cache hits served from a remote ref. Phase 1
    /// never increments this; wired up alongside the remote fetch path.
    pub workflow_stage_cache_hits_remote: u64,
    /// Workflow stages that ended in a terminal `Failed` state.
    pub workflow_stages_failed: u64,
    /// Retry attempts across all workflow stages (one per re-execution,
    /// not counting the first attempt).
    pub workflow_stage_retry_attempts: u64,
    /// Non-terminal workflow journals discovered and scanned for
    /// resume — counted once per prior run, not once per stage row.
    pub workflow_journal_resumes: u64,
    /// Total workflow DAG runs completed (success + failure + partial).
    pub workflow_runs_total: u64,
    /// Total workflow stages dispatched (all outcomes).
    pub workflow_stages_total: u64,
    /// Total retry attempts across all workflow stages.
    pub workflow_retry_attempts_total: u64,
    /// Total bytes pushed to the remote workflow cache.
    pub workflow_cache_push_bytes_total: u64,
    /// Total bytes pulled from the remote workflow cache.
    pub workflow_cache_pull_bytes_total: u64,

    // --- storage economy counters ---
    /// Tier plans generated (dry-run or pre-apply).
    pub tier_plans_generated: u64,
    /// Tier plans successfully applied to a provider.
    pub tier_plans_applied: u64,
    /// Tier apply attempts that failed (CAS conflict, auth, etc.).
    pub tier_apply_failures: u64,
    /// Lifecycle conflict detections during tier apply.
    pub tier_lifecycle_conflicts: u64,
    /// Restore requests issued for S3 Standard tier.
    pub hydrate_restore_requests_s3_standard: u64,
    /// Restore requests issued for S3 Expedited tier.
    pub hydrate_restore_requests_s3_expedited: u64,
    /// Restore requests issued for S3 Bulk tier.
    pub hydrate_restore_requests_s3_bulk: u64,
    /// Restore requests issued for Azure High priority.
    pub hydrate_restore_requests_azure_high: u64,
    /// Restore requests issued for Azure Standard priority.
    pub hydrate_restore_requests_azure_standard: u64,
    /// Restore completions (object became warm).
    pub hydrate_restore_completions: u64,
    /// Restore timeouts (deadline exceeded while polling).
    pub hydrate_restore_timeouts: u64,
    /// GC objects skipped due to early-delete retention guard.
    pub gc_blocked_early_delete: u64,
    /// GC objects force-deleted despite early-delete penalty.
    pub gc_force_early_delete: u64,
    /// Xorbs processed by the xorb optimization executor.
    pub optimize_xorbs_processed: u64,
    /// Corrupt source xorbs encountered during xorb optimization.
    pub optimize_xorbs_corrupt: u64,
    /// Objects scanned by the cost inventory walker.
    pub cost_inventory_objects_scanned: u64,

    // --- speculation counters ---
    /// Speculative hydrations launched (background tasks spawned).
    pub speculation_hydrates_total: u64,
    /// Speculation hits (user later explicitly opened a speculatively-hydrated file).
    pub speculation_hits_total: u64,
    /// Speculation evictions (skipped due to cache pressure).
    pub speculation_evictions_total: u64,

    // --- manifest CAS counters ---
    /// CAS retries due to concurrent pushes on the unified manifest.
    pub manifest_cas_conflicts_total: u64,
    /// Permanent CAS failures (conflicting ref updates or max retries exceeded).
    pub manifest_cas_failures_total: u64,

    // --- metadb observability ---
    /// Number of times a `MetaDbGuard` had to fall back to the `Drop`
    /// close path because the owning session never called `close()`.
    /// A healthy process reports `0` — any bump indicates a code path
    /// that leaks the guard and should be audited.
    pub metadb_close_on_drop_count: u64,

    // --- metadb per-DB gauges (point-in-time observations) ---
    /// Current entry count in the per-repo `file_index_db`.
    pub metadb_file_index_entries: u64,
    /// Current SSTable count on level 0 of the per-repo `file_index_db`.
    pub metadb_file_index_sstables: u64,
    /// Current WAL segment count for the per-repo `file_index_db`.
    pub metadb_file_index_wal_segments: u64,
    /// Current entry count in the global `chunk_index_db`.
    pub metadb_chunk_index_entries: u64,
    /// Current SSTable count on level 0 of the global `chunk_index_db`.
    pub metadb_chunk_index_sstables: u64,
    /// Current WAL segment count for the global `chunk_index_db`.
    pub metadb_chunk_index_wal_segments: u64,

    // --- metadb local cache gauges ---
    /// Current entry count in the local chunk-index cache
    /// (in-memory + persistent tiers combined).
    pub metadb_chunk_index_cache_entries: u64,
    /// Approximate bytes resident in the local chunk-index cache.
    pub metadb_chunk_index_cache_size_bytes: u64,

    // --- metadb counters ---
    /// Total single-key `Db::get` calls across both SlateDB instances.
    pub metadb_get_count: u64,
    /// Single-key `Db::get` calls that returned `Some`.
    pub metadb_get_hits: u64,
    /// Total `Db::get_batch` calls across both SlateDB instances.
    pub metadb_batch_get_count: u64,
    /// Total `Db::write` calls across both SlateDB instances.
    pub metadb_batch_write_count: u64,
    /// Repairable metadata batches buffered before an explicit flush.
    pub metadb_buffered_batch_write_count: u64,
    /// Explicit WAL durability flushes issued after durable metadata writes.
    pub metadb_wal_flush_count: u64,
    /// Explicit memtable flushes that amortize repairable metadata batches.
    pub metadb_memtable_flush_count: u64,
    /// Total bytes written through `Db::write` across both SlateDB
    /// instances.
    pub metadb_write_bytes: u64,
    /// Total successful `Db::open` calls across both SlateDB
    /// instances.
    pub metadb_open_count: u64,
    /// Total `Db::close` calls across both SlateDB instances.
    pub metadb_close_count: u64,

    // --- metadb local cache counters ---
    /// Local chunk-index cache hits (in-memory + persistent tier).
    pub metadb_chunk_index_cache_hits: u64,
    /// Local chunk-index cache misses (fell through to remote).
    pub metadb_chunk_index_cache_misses: u64,
    /// Lazy-fill writes into the local chunk-index cache after a
    /// remote hit.
    pub metadb_chunk_index_cache_lazy_fills: u64,

    // --- metadb timing (milliseconds) ---
    /// Wall-clock of the last `MetaDb::commit`.
    pub metadb_last_push_batch_ms: u64,
    /// Wall-clock of the last compaction run. Zero when no compaction
    /// has been triggered through the metadb layer yet — SlateDB
    /// manages compaction in the background and the trigger surface is
    /// out of scope for this cut.
    pub metadb_last_compaction_ms: u64,
}

/// Lock-free performance counters for the data plane.
///
/// All fields are [`AtomicU64`] bumped with [`Relaxed`] ordering — good
/// enough for monotonic counters where we never need cross-counter
/// consistency within a single observation.
#[derive(Debug, Default)]
pub struct Metrics {
    chunk_index_persistent_hits: AtomicU64,
    shard_bloom_queries: AtomicU64,
    shard_bloom_false_positives: AtomicU64,
    staging_compression_ratio: AtomicU64,
    adaptive_threshold_effective: AtomicU64,
    chunk_index_lookups: AtomicU64,
    chunk_index_hits: AtomicU64,
    chunk_index_shards_installed: AtomicU64,
    chunk_index_entries: AtomicU64,
    staging_segments_sealed: AtomicU64,
    staging_segments_compacted: AtomicU64,
    staging_bytes_written: AtomicU64,
    staging_bytes_read: AtomicU64,
    staging_fsyncs: AtomicU64,
    staging_compactions_skipped_inflight: AtomicU64,
    push_stream_backpressure_events: AtomicU64,
    head_list_requests: AtomicU64,
    head_point_requests: AtomicU64,
    head_check_errors: AtomicU64,
    xorbs_skipped: AtomicU64,
    cas_pipelined_commits: AtomicU64,
    clean_fastpath_taken: AtomicU64,
    clean_fastpath_false_positives: AtomicU64,
    shard_hint_hits: AtomicU64,
    shard_hint_misses: AtomicU64,
    prefetch_started: AtomicU64,
    prefetch_completed: AtomicU64,
    prefetch_bytes: AtomicU64,
    prefetch_cache_hits: AtomicU64,
    xorb_fetch_requests_coalesced: AtomicU64,
    xorb_fetch_bytes_saved: AtomicU64,
    smudge_batch_size: AtomicU64,
    multipart_resumed_uploads: AtomicU64,

    // --- upload concurrency gauge ---
    upload_concurrency_current: AtomicU64,

    // --- deduplication byte counters ---
    dedup_total_bytes: AtomicU64,
    dedup_saved_bytes: AtomicU64,
    dedup_new_bytes: AtomicU64,
    dedup_defrag_prevented_bytes: AtomicU64,
    dedup_global_bytes: AtomicU64,

    // --- FUSE operation counters ---
    fuse_read_count: AtomicU64,
    fuse_read_bytes: AtomicU64,
    hydration_queue_depth: AtomicU64,
    cache_hits: AtomicU64,
    cache_misses: AtomicU64,

    // --- xet-core xorb-range DiskCache counters ---
    chunk_cache_hits: AtomicU64,
    chunk_cache_misses: AtomicU64,
    chunk_cache_bytes: AtomicU64,

    // --- compression counters ---
    chunks_compressed: AtomicU64,
    chunks_bg4_transformed: AtomicU64,
    chunks_stored_raw: AtomicU64,
    compression_bytes_saved: AtomicU64,

    // --- operation-level counters ---
    push_duration_ms: AtomicU64,
    bytes_uploaded: AtomicU64,
    fetch_duration_ms: AtomicU64,
    bytes_downloaded: AtomicU64,
    gc_duration_ms: AtomicU64,
    gc_objects_deleted: AtomicU64,

    // --- import counters ---
    //
    // Lifetime totals for the `crab import` pipeline, populated by
    // the ingest and assemble stages. `IngestStats` / `AssembleStats`
    // remain the per-run report surfaces; these counters aggregate
    // across runs in the same process for observability dashboards.
    import_files_total: AtomicU64,
    import_versions_total: AtomicU64,
    import_commits_total: AtomicU64,
    import_bytes_source_total: AtomicU64,
    import_bytes_staged_total: AtomicU64,
    import_failures_total: AtomicU64,

    // --- workflow counters ---
    //
    // Per R21: lifetime totals for the workflow executor. Bumped from
    // `workflow::executor` (miss/hit/fail/retry) and from `cmd::run`
    // (journal resumes). `workflow_stage_cache_hits_remote` is
    // reserved for the remote-ref fetch path that lands alongside
    // phase 3; phase 1 only bumps the `_local` counter.
    workflow_stages_executed: AtomicU64,
    workflow_stage_cache_hits_local: AtomicU64,
    workflow_stage_cache_hits_remote: AtomicU64,
    workflow_stages_failed: AtomicU64,
    workflow_stage_retry_attempts: AtomicU64,
    workflow_journal_resumes: AtomicU64,
    workflow_runs_total: AtomicU64,
    workflow_stages_total: AtomicU64,
    workflow_retry_attempts_total: AtomicU64,
    workflow_cache_push_bytes_total: AtomicU64,
    workflow_cache_pull_bytes_total: AtomicU64,

    // --- storage economy counters ---
    tier_plans_generated: AtomicU64,
    tier_plans_applied: AtomicU64,
    tier_apply_failures: AtomicU64,
    tier_lifecycle_conflicts: AtomicU64,
    hydrate_restore_requests_s3_standard: AtomicU64,
    hydrate_restore_requests_s3_expedited: AtomicU64,
    hydrate_restore_requests_s3_bulk: AtomicU64,
    hydrate_restore_requests_azure_high: AtomicU64,
    hydrate_restore_requests_azure_standard: AtomicU64,
    hydrate_restore_completions: AtomicU64,
    hydrate_restore_timeouts: AtomicU64,
    gc_blocked_early_delete: AtomicU64,
    gc_force_early_delete: AtomicU64,
    optimize_xorbs_processed: AtomicU64,
    optimize_xorbs_corrupt: AtomicU64,
    cost_inventory_objects_scanned: AtomicU64,

    // --- speculation counters ---
    speculation_hydrates_total: AtomicU64,
    speculation_hits_total: AtomicU64,
    speculation_evictions_total: AtomicU64,

    // --- manifest CAS counters ---
    manifest_cas_conflicts_total: AtomicU64,
    manifest_cas_failures_total: AtomicU64,

    // --- metadb observability ---
    metadb_close_on_drop_count: AtomicU64,

    // --- metadb per-DB gauges ---
    metadb_file_index_entries: AtomicU64,
    metadb_file_index_sstables: AtomicU64,
    metadb_file_index_wal_segments: AtomicU64,
    metadb_chunk_index_entries: AtomicU64,
    metadb_chunk_index_sstables: AtomicU64,
    metadb_chunk_index_wal_segments: AtomicU64,

    // --- metadb local cache gauges ---
    metadb_chunk_index_cache_entries: AtomicU64,
    metadb_chunk_index_cache_size_bytes: AtomicU64,

    // --- metadb counters ---
    metadb_get_count: AtomicU64,
    metadb_get_hits: AtomicU64,
    metadb_batch_get_count: AtomicU64,
    metadb_batch_write_count: AtomicU64,
    metadb_buffered_batch_write_count: AtomicU64,
    metadb_wal_flush_count: AtomicU64,
    metadb_memtable_flush_count: AtomicU64,
    metadb_write_bytes: AtomicU64,
    metadb_open_count: AtomicU64,
    metadb_close_count: AtomicU64,

    // --- metadb local cache counters ---
    metadb_chunk_index_cache_hits: AtomicU64,
    metadb_chunk_index_cache_misses: AtomicU64,
    metadb_chunk_index_cache_lazy_fills: AtomicU64,

    // --- metadb timing ---
    metadb_last_push_batch_ms: AtomicU64,
    metadb_last_compaction_ms: AtomicU64,
}

impl Metrics {
    /// Create a zeroed counter set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    // --- increment helpers ---

    /// Record a chunk-existence hit from the persistent index.
    pub fn inc_chunk_index_persistent_hits(&self) {
        self.chunk_index_persistent_hits.fetch_add(1, Relaxed);
    }

    /// Record a shard bloom query.
    pub fn inc_shard_bloom_queries(&self) {
        self.shard_bloom_queries.fetch_add(1, Relaxed);
    }

    /// Record a shard bloom false positive.
    pub fn inc_shard_bloom_false_positives(&self) {
        self.shard_bloom_false_positives.fetch_add(1, Relaxed);
    }

    /// Record a staging compression ratio observation (caller scales).
    pub fn inc_staging_compression_ratio(&self, value: u64) {
        self.staging_compression_ratio.fetch_add(value, Relaxed);
    }

    /// Record the effective adaptive threshold (caller scales).
    pub fn inc_adaptive_threshold_effective(&self, value: u64) {
        self.adaptive_threshold_effective.fetch_add(value, Relaxed);
    }

    /// Set the effective adaptive threshold (absolute, not additive).
    ///
    /// Caller scales the threshold by 1000 (e.g. 0.25 → 250).
    pub fn set_adaptive_threshold_effective(&self, value: u64) {
        self.adaptive_threshold_effective.store(value, Relaxed);
    }

    /// Record a chunk index lookup.
    pub fn inc_chunk_index_lookups(&self) {
        self.chunk_index_lookups.fetch_add(1, Relaxed);
    }

    /// Record a chunk index hit (dedup).
    pub fn inc_chunk_index_hits(&self) {
        self.chunk_index_hits.fetch_add(1, Relaxed);
    }

    /// Record a shard installed into the chunk index.
    pub fn inc_chunk_index_shards_installed(&self) {
        self.chunk_index_shards_installed.fetch_add(1, Relaxed);
    }

    /// Set the current chunk index entry count (absolute, not additive).
    pub fn set_chunk_index_entries(&self, count: u64) {
        self.chunk_index_entries.store(count, Relaxed);
    }

    /// Record a staging segment seal.
    pub fn inc_staging_segments_sealed(&self) {
        self.staging_segments_sealed.fetch_add(1, Relaxed);
    }

    /// Record a staging segment compaction.
    pub fn inc_staging_segments_compacted(&self) {
        self.staging_segments_compacted.fetch_add(1, Relaxed);
    }

    /// Record bytes written to a staging segment.
    pub fn add_staging_bytes_written(&self, n: u64) {
        self.staging_bytes_written.fetch_add(n, Relaxed);
    }

    /// Record bytes read from a staging segment.
    pub fn add_staging_bytes_read(&self, n: u64) {
        self.staging_bytes_read.fetch_add(n, Relaxed);
    }

    /// Record an fsync issued by the staging area.
    pub fn inc_staging_fsyncs(&self) {
        self.staging_fsyncs.fetch_add(1, Relaxed);
    }

    /// Record a compaction skipped because a push was inflight.
    pub fn inc_staging_compactions_skipped_inflight(&self) {
        self.staging_compactions_skipped_inflight
            .fetch_add(1, Relaxed);
    }

    /// Record a backpressure event in the streaming push pipeline.
    pub fn inc_push_stream_backpressure_events(&self) {
        self.push_stream_backpressure_events.fetch_add(1, Relaxed);
    }

    /// Record a LIST request issued during batched HEAD resume.
    pub fn inc_head_list_requests(&self) {
        self.head_list_requests.fetch_add(1, Relaxed);
    }

    /// Record multiple LIST requests issued during batched HEAD resume.
    pub fn add_head_list_requests(&self, n: u64) {
        self.head_list_requests.fetch_add(n, Relaxed);
    }

    /// Record an individual HEAD request issued during resume checks.
    pub fn inc_head_point_requests(&self) {
        self.head_point_requests.fetch_add(1, Relaxed);
    }

    /// Record multiple individual HEAD requests issued during resume checks.
    pub fn add_head_point_requests(&self, n: u64) {
        self.head_point_requests.fetch_add(n, Relaxed);
    }

    /// Record a HEAD/LIST resume-check request that exhausted retries on a
    /// transient error (xorb conservatively treated as needing upload).
    pub fn inc_head_check_errors(&self) {
        self.head_check_errors.fetch_add(1, Relaxed);
    }

    /// Record multiple HEAD/LIST resume-check requests that exhausted retries.
    pub fn add_head_check_errors(&self, n: u64) {
        self.head_check_errors.fetch_add(n, Relaxed);
    }

    /// Record a xorb skipped because it already exists on the remote.
    pub fn inc_xorbs_skipped(&self) {
        self.xorbs_skipped.fetch_add(1, Relaxed);
    }

    /// Record multiple xorbs skipped because they already exist on the remote.
    pub fn add_xorbs_skipped(&self, n: u64) {
        self.xorbs_skipped.fetch_add(n, Relaxed);
    }

    /// Record a pipelined (parallel) CAS commit.
    pub fn inc_cas_pipelined_commits(&self) {
        self.cas_pipelined_commits.fetch_add(1, Relaxed);
    }

    /// Record a clean fast-path taken (bloom hit confirmed by HEAD 200).
    pub fn inc_clean_fastpath_taken(&self) {
        self.clean_fastpath_taken.fetch_add(1, Relaxed);
    }

    /// Record a clean fast-path bloom false positive (bloom hit but HEAD non-200).
    pub fn inc_clean_fastpath_false_positives(&self) {
        self.clean_fastpath_false_positives.fetch_add(1, Relaxed);
    }

    /// Record a successful shard-hint fast-path hydration.
    pub fn inc_shard_hint_hits(&self) {
        self.shard_hint_hits.fetch_add(1, Relaxed);
    }

    /// Record a stale/missing shard-hint that fell back to the file-index path.
    pub fn inc_shard_hint_misses(&self) {
        self.shard_hint_misses.fetch_add(1, Relaxed);
    }

    /// Record a prefetch reconstruction that has been submitted to the
    /// filter-process prefetch queue.
    pub fn inc_prefetch_started(&self) {
        self.prefetch_started.fetch_add(1, Relaxed);
    }

    /// Record a prefetch reconstruction that completed successfully.
    pub fn inc_prefetch_completed(&self) {
        self.prefetch_completed.fetch_add(1, Relaxed);
    }

    /// Record bytes produced by a completed prefetch reconstruction.
    pub fn add_prefetch_bytes(&self, n: u64) {
        self.prefetch_bytes.fetch_add(n, Relaxed);
    }

    /// Record a prefetch that was satisfied entirely from the local chunk
    /// cache (no remote fetch issued).
    pub fn inc_prefetch_cache_hits(&self) {
        self.prefetch_cache_hits.fetch_add(1, Relaxed);
    }

    /// Record xorb fetch requests saved by cross-file coalescing.
    pub fn add_xorb_fetch_requests_coalesced(&self, n: u64) {
        self.xorb_fetch_requests_coalesced.fetch_add(n, Relaxed);
    }

    /// Record bytes saved by cross-file coalescing.
    pub fn add_xorb_fetch_bytes_saved(&self, n: u64) {
        self.xorb_fetch_bytes_saved.fetch_add(n, Relaxed);
    }

    /// Set the smudge batch size (absolute, not additive).
    pub fn set_smudge_batch_size(&self, n: u64) {
        self.smudge_batch_size.store(n, Relaxed);
    }

    /// Record a multipart upload that was resumed from a previous session.
    pub fn inc_multipart_resumed_uploads(&self) {
        self.multipart_resumed_uploads.fetch_add(1, Relaxed);
    }

    /// Set the current upload concurrency level (absolute gauge).
    pub fn set_upload_concurrency_current(&self, n: u64) {
        self.upload_concurrency_current.store(n, Relaxed);
    }

    // --- deduplication byte counter helpers ---

    /// Copy all fields from a `DeduplicationMetrics` snapshot into the
    /// corresponding atomic counters. Called once after a push completes
    /// so the observability layer can export dedup stats.
    pub fn set_dedup_metrics(&self, dm: &xet_data::deduplication::DeduplicationMetrics) {
        self.dedup_total_bytes.store(dm.total_bytes, Relaxed);
        self.dedup_saved_bytes.store(dm.deduped_bytes, Relaxed);
        self.dedup_new_bytes.store(dm.new_bytes, Relaxed);
        self.dedup_defrag_prevented_bytes
            .store(dm.defrag_prevented_dedup_bytes, Relaxed);
        self.dedup_global_bytes
            .store(dm.deduped_bytes_by_global_dedup, Relaxed);
    }

    // --- FUSE operation counter helpers ---

    /// Record a FUSE read operation.
    pub fn inc_fuse_read_count(&self) {
        self.fuse_read_count.fetch_add(1, Relaxed);
    }

    /// Record bytes served via a FUSE read operation.
    pub fn add_fuse_read_bytes(&self, n: u64) {
        self.fuse_read_bytes.fetch_add(n, Relaxed);
    }

    /// Set the current hydration queue depth (absolute, not additive).
    pub fn set_hydration_queue_depth(&self, depth: u64) {
        self.hydration_queue_depth.store(depth, Relaxed);
    }

    /// Record a cache hit during FUSE/hydration chunk lookup.
    pub fn inc_cache_hits(&self) {
        self.cache_hits.fetch_add(1, Relaxed);
    }

    /// Record a cache miss during FUSE/hydration chunk lookup.
    pub fn inc_cache_misses(&self) {
        self.cache_misses.fetch_add(1, Relaxed);
    }

    // --- xet-core xorb-range DiskCache counter helpers ---

    /// Record a hit against the xet-core chunk `DiskCache`.
    pub fn inc_chunk_cache_hits(&self) {
        self.chunk_cache_hits.fetch_add(1, Relaxed);
    }

    /// Record `n` hits against the xet-core chunk `DiskCache`.
    pub fn add_chunk_cache_hits(&self, n: u64) {
        self.chunk_cache_hits.fetch_add(n, Relaxed);
    }

    /// Record a miss against the xet-core chunk `DiskCache`.
    pub fn inc_chunk_cache_misses(&self) {
        self.chunk_cache_misses.fetch_add(1, Relaxed);
    }

    /// Record `n` misses against the xet-core chunk `DiskCache`.
    pub fn add_chunk_cache_misses(&self, n: u64) {
        self.chunk_cache_misses.fetch_add(n, Relaxed);
    }

    /// Record the current resident byte count of the xet-core chunk
    /// `DiskCache`. This is an absolute observation; callers usually
    /// query the cache directly and write the value verbatim.
    pub fn set_chunk_cache_bytes(&self, bytes: u64) {
        self.chunk_cache_bytes.store(bytes, Relaxed);
    }

    // --- compression counter helpers ---

    /// Record a chunk compressed with plain LZ4.
    pub fn add_chunks_compressed(&self, n: u64) {
        self.chunks_compressed.fetch_add(n, Relaxed);
    }

    /// Record a chunk compressed with BG4 byte-grouping + LZ4.
    pub fn add_chunks_bg4_transformed(&self, n: u64) {
        self.chunks_bg4_transformed.fetch_add(n, Relaxed);
    }

    /// Record a chunk stored without compression.
    pub fn add_chunks_stored_raw(&self, n: u64) {
        self.chunks_stored_raw.fetch_add(n, Relaxed);
    }

    /// Record bytes saved by compression (uncompressed − compressed).
    pub fn add_compression_bytes_saved(&self, n: u64) {
        self.compression_bytes_saved.fetch_add(n, Relaxed);
    }

    // --- operation-level counter helpers ---

    /// Record push wall-clock time in milliseconds.
    pub fn add_push_duration_ms(&self, ms: u64) {
        self.push_duration_ms.fetch_add(ms, Relaxed);
    }

    /// Record bytes uploaded during a push.
    pub fn add_bytes_uploaded(&self, n: u64) {
        self.bytes_uploaded.fetch_add(n, Relaxed);
    }

    /// Record fetch wall-clock time in milliseconds.
    pub fn add_fetch_duration_ms(&self, ms: u64) {
        self.fetch_duration_ms.fetch_add(ms, Relaxed);
    }

    /// Record bytes downloaded during a fetch.
    pub fn add_bytes_downloaded(&self, n: u64) {
        self.bytes_downloaded.fetch_add(n, Relaxed);
    }

    /// Record GC wall-clock time in milliseconds.
    pub fn add_gc_duration_ms(&self, ms: u64) {
        self.gc_duration_ms.fetch_add(ms, Relaxed);
    }

    /// Record objects deleted by GC.
    pub fn add_gc_objects_deleted(&self, n: u64) {
        self.gc_objects_deleted.fetch_add(n, Relaxed);
    }

    // --- import counter helpers ---

    /// Record `n` files visible in the final HEAD tree of an import.
    /// Called once per import when assemble has walked every window.
    pub fn add_import_files_total(&self, n: u64) {
        self.import_files_total.fetch_add(n, Relaxed);
    }

    /// Record `n` entries across every window of an import.
    pub fn add_import_versions_total(&self, n: u64) {
        self.import_versions_total.fetch_add(n, Relaxed);
    }

    /// Record a single commit landed by the assemble stage.
    pub fn inc_import_commits_total(&self) {
        self.import_commits_total.fetch_add(1, Relaxed);
    }

    /// Record source-object bytes contributed by one staged entry.
    pub fn add_import_bytes_source_total(&self, n: u64) {
        self.import_bytes_source_total.fetch_add(n, Relaxed);
    }

    /// Record bytes written to staging for one staged entry.
    pub fn add_import_bytes_staged_total(&self, n: u64) {
        self.import_bytes_staged_total.fetch_add(n, Relaxed);
    }

    /// Record one failed ingest entry.
    pub fn inc_import_failures_total(&self) {
        self.import_failures_total.fetch_add(1, Relaxed);
    }

    // --- workflow counter helpers ---

    /// Record a workflow stage that completed on the miss path
    /// (user command executed, entry committed).
    pub fn inc_workflow_stages_executed(&self) {
        self.workflow_stages_executed.fetch_add(1, Relaxed);
    }

    /// Record a workflow stage cache hit served from the local
    /// chunk cache.
    pub fn inc_workflow_stage_cache_hits_local(&self) {
        self.workflow_stage_cache_hits_local.fetch_add(1, Relaxed);
    }

    /// Record a workflow stage cache hit served from a remote ref.
    ///
    /// Phase 1 never calls this; the remote-ref fetch path wires it
    /// up alongside phase 3's ref-CAS publication.
    pub fn inc_workflow_stage_cache_hits_remote(&self) {
        self.workflow_stage_cache_hits_remote.fetch_add(1, Relaxed);
    }

    /// Record a workflow stage that ended in a terminal `Failed`
    /// state.
    pub fn inc_workflow_stages_failed(&self) {
        self.workflow_stages_failed.fetch_add(1, Relaxed);
    }

    /// Record one workflow stage retry attempt (after the first
    /// attempt; each re-execution bumps this by one).
    pub fn inc_workflow_stage_retry_attempts(&self) {
        self.workflow_stage_retry_attempts.fetch_add(1, Relaxed);
    }

    /// Record one non-terminal workflow journal discovered during
    /// resume scan — once per prior run, not per stage row.
    pub fn inc_workflow_journal_resumes(&self) {
        self.workflow_journal_resumes.fetch_add(1, Relaxed);
    }

    /// Record a completed workflow DAG run (any outcome).
    pub fn inc_workflow_runs_total(&self) {
        self.workflow_runs_total.fetch_add(1, Relaxed);
    }

    /// Record a workflow stage dispatched (any outcome).
    pub fn inc_workflow_stages_total(&self) {
        self.workflow_stages_total.fetch_add(1, Relaxed);
    }

    /// Record a workflow stage retry attempt.
    pub fn inc_workflow_retry_attempts_total(&self) {
        self.workflow_retry_attempts_total.fetch_add(1, Relaxed);
    }

    /// Record bytes pushed to the remote workflow cache.
    pub fn add_workflow_cache_push_bytes_total(&self, n: u64) {
        self.workflow_cache_push_bytes_total.fetch_add(n, Relaxed);
    }

    /// Record bytes pulled from the remote workflow cache.
    pub fn add_workflow_cache_pull_bytes_total(&self, n: u64) {
        self.workflow_cache_pull_bytes_total.fetch_add(n, Relaxed);
    }

    // --- storage economy counter helpers ---

    /// Record a tier plan generated (dry-run or pre-apply).
    pub fn inc_tier_plans_generated(&self) {
        self.tier_plans_generated.fetch_add(1, Relaxed);
    }

    /// Record a tier plan successfully applied to a provider.
    pub fn inc_tier_plans_applied(&self) {
        self.tier_plans_applied.fetch_add(1, Relaxed);
    }

    /// Record a tier apply failure (CAS conflict, auth, etc.).
    pub fn inc_tier_apply_failures(&self) {
        self.tier_apply_failures.fetch_add(1, Relaxed);
    }

    /// Record a lifecycle conflict detection during tier apply.
    pub fn inc_tier_lifecycle_conflicts(&self) {
        self.tier_lifecycle_conflicts.fetch_add(1, Relaxed);
    }

    /// Record a restore request issued for S3 Standard tier.
    pub fn inc_hydrate_restore_requests_s3_standard(&self) {
        self.hydrate_restore_requests_s3_standard
            .fetch_add(1, Relaxed);
    }

    /// Record a restore request issued for S3 Expedited tier.
    pub fn inc_hydrate_restore_requests_s3_expedited(&self) {
        self.hydrate_restore_requests_s3_expedited
            .fetch_add(1, Relaxed);
    }

    /// Record a restore request issued for S3 Bulk tier.
    pub fn inc_hydrate_restore_requests_s3_bulk(&self) {
        self.hydrate_restore_requests_s3_bulk.fetch_add(1, Relaxed);
    }

    /// Record a restore request issued for Azure High priority.
    pub fn inc_hydrate_restore_requests_azure_high(&self) {
        self.hydrate_restore_requests_azure_high
            .fetch_add(1, Relaxed);
    }

    /// Record a restore request issued for Azure Standard priority.
    pub fn inc_hydrate_restore_requests_azure_standard(&self) {
        self.hydrate_restore_requests_azure_standard
            .fetch_add(1, Relaxed);
    }

    /// Record a restore completion (object became warm).
    pub fn inc_hydrate_restore_completions(&self) {
        self.hydrate_restore_completions.fetch_add(1, Relaxed);
    }

    /// Record a restore timeout (deadline exceeded while polling).
    pub fn inc_hydrate_restore_timeouts(&self) {
        self.hydrate_restore_timeouts.fetch_add(1, Relaxed);
    }

    /// Record a GC object skipped due to early-delete retention guard.
    pub fn inc_gc_blocked_early_delete(&self) {
        self.gc_blocked_early_delete.fetch_add(1, Relaxed);
    }

    /// Record a GC object force-deleted despite early-delete penalty.
    pub fn inc_gc_force_early_delete(&self) {
        self.gc_force_early_delete.fetch_add(1, Relaxed);
    }

    /// Record a xorb processed by the xorb optimization executor.
    pub fn inc_optimize_xorbs_processed(&self) {
        self.optimize_xorbs_processed.fetch_add(1, Relaxed);
    }

    /// Record a corrupt source xorb encountered during xorb optimization.
    pub fn inc_optimize_xorbs_corrupt(&self) {
        self.optimize_xorbs_corrupt.fetch_add(1, Relaxed);
    }

    /// Record objects scanned by the cost inventory walker.
    pub fn add_cost_inventory_objects_scanned(&self, n: u64) {
        self.cost_inventory_objects_scanned.fetch_add(n, Relaxed);
    }

    // --- speculation counter helpers ---

    /// Record a speculative hydration launched (background task spawned).
    pub fn inc_speculation_hydrates_total(&self) {
        self.speculation_hydrates_total.fetch_add(1, Relaxed);
    }

    /// Record a speculation hit (user opened a speculatively-hydrated file).
    pub fn inc_speculation_hits_total(&self) {
        self.speculation_hits_total.fetch_add(1, Relaxed);
    }

    /// Record a speculation eviction (skipped due to cache pressure).
    pub fn inc_speculation_evictions_total(&self) {
        self.speculation_evictions_total.fetch_add(1, Relaxed);
    }

    // --- manifest CAS counter helpers ---

    /// Record a CAS conflict (retry) on the unified manifest.
    pub fn inc_manifest_cas_conflicts_total(&self) {
        self.manifest_cas_conflicts_total.fetch_add(1, Relaxed);
    }

    /// Record a permanent CAS failure on the unified manifest.
    pub fn inc_manifest_cas_failures_total(&self) {
        self.manifest_cas_failures_total.fetch_add(1, Relaxed);
    }

    // --- metadb observability helpers ---

    /// Record a `MetaDbGuard` that fell back to the `Drop` close path
    /// because its owner never called `close()`. This should stay at
    /// zero in production; any non-zero reading points to a leak.
    pub fn inc_metadb_close_on_drop(&self) {
        self.metadb_close_on_drop_count.fetch_add(1, Relaxed);
    }

    // --- metadb per-DB gauge setters ---

    /// Set the approximate entry count for the per-repo
    /// `file_index_db` (absolute gauge).
    pub fn set_metadb_file_index_entries(&self, n: u64) {
        self.metadb_file_index_entries.store(n, Relaxed);
    }

    /// Set the level-0 SSTable count for the per-repo `file_index_db`.
    pub fn set_metadb_file_index_sstables(&self, n: u64) {
        self.metadb_file_index_sstables.store(n, Relaxed);
    }

    /// Set the WAL segment count for the per-repo `file_index_db`.
    pub fn set_metadb_file_index_wal_segments(&self, n: u64) {
        self.metadb_file_index_wal_segments.store(n, Relaxed);
    }

    /// Set the approximate entry count for the global `chunk_index_db`.
    pub fn set_metadb_chunk_index_entries(&self, n: u64) {
        self.metadb_chunk_index_entries.store(n, Relaxed);
    }

    /// Set the level-0 SSTable count for the global `chunk_index_db`.
    pub fn set_metadb_chunk_index_sstables(&self, n: u64) {
        self.metadb_chunk_index_sstables.store(n, Relaxed);
    }

    /// Set the WAL segment count for the global `chunk_index_db`.
    pub fn set_metadb_chunk_index_wal_segments(&self, n: u64) {
        self.metadb_chunk_index_wal_segments.store(n, Relaxed);
    }

    /// Set the current entry count in the local chunk-index cache.
    pub fn set_metadb_chunk_index_cache_entries(&self, n: u64) {
        self.metadb_chunk_index_cache_entries.store(n, Relaxed);
    }

    /// Set the approximate byte size of the local chunk-index cache.
    pub fn set_metadb_chunk_index_cache_size_bytes(&self, n: u64) {
        self.metadb_chunk_index_cache_size_bytes.store(n, Relaxed);
    }

    // --- metadb counter helpers ---

    /// Record a single-key `Db::get`.
    pub fn inc_metadb_get_count(&self) {
        self.metadb_get_count.fetch_add(1, Relaxed);
    }

    /// Record a single-key `Db::get` that returned `Some`.
    pub fn inc_metadb_get_hits(&self) {
        self.metadb_get_hits.fetch_add(1, Relaxed);
    }

    /// Record a `Db::get_batch` call.
    pub fn inc_metadb_batch_get_count(&self) {
        self.metadb_batch_get_count.fetch_add(1, Relaxed);
    }

    /// Record a `Db::write` call.
    pub fn inc_metadb_batch_write_count(&self) {
        self.metadb_batch_write_count.fetch_add(1, Relaxed);
    }

    /// Record a repairable batch buffered until an explicit flush boundary.
    pub fn inc_metadb_buffered_batch_write_count(&self) {
        self.metadb_buffered_batch_write_count.fetch_add(1, Relaxed);
    }

    /// Record one explicit SlateDB WAL flush.
    pub fn inc_metadb_wal_flush_count(&self) {
        self.metadb_wal_flush_count.fetch_add(1, Relaxed);
    }

    /// Record one explicit SlateDB memtable flush.
    pub fn inc_metadb_memtable_flush_count(&self) {
        self.metadb_memtable_flush_count.fetch_add(1, Relaxed);
    }

    /// Record bytes written through a `Db::write` call.
    pub fn add_metadb_write_bytes(&self, n: u64) {
        self.metadb_write_bytes.fetch_add(n, Relaxed);
    }

    /// Record a successful `Db::open`.
    pub fn inc_metadb_open_count(&self) {
        self.metadb_open_count.fetch_add(1, Relaxed);
    }

    /// Record a `Db::close`.
    pub fn inc_metadb_close_count(&self) {
        self.metadb_close_count.fetch_add(1, Relaxed);
    }

    /// Record a local chunk-index cache hit.
    pub fn inc_metadb_chunk_index_cache_hits(&self) {
        self.metadb_chunk_index_cache_hits.fetch_add(1, Relaxed);
    }

    /// Record multiple local chunk-index cache hits.
    pub fn add_metadb_chunk_index_cache_hits(&self, n: u64) {
        self.metadb_chunk_index_cache_hits.fetch_add(n, Relaxed);
    }

    /// Record a local chunk-index cache miss (fell through to remote).
    pub fn inc_metadb_chunk_index_cache_misses(&self) {
        self.metadb_chunk_index_cache_misses.fetch_add(1, Relaxed);
    }

    /// Record multiple local chunk-index cache misses.
    pub fn add_metadb_chunk_index_cache_misses(&self, n: u64) {
        self.metadb_chunk_index_cache_misses.fetch_add(n, Relaxed);
    }

    /// Record a lazy-fill write into the local chunk-index cache.
    pub fn inc_metadb_chunk_index_cache_lazy_fills(&self) {
        self.metadb_chunk_index_cache_lazy_fills
            .fetch_add(1, Relaxed);
    }

    /// Record multiple lazy-fill writes into the local chunk-index cache.
    pub fn add_metadb_chunk_index_cache_lazy_fills(&self, n: u64) {
        self.metadb_chunk_index_cache_lazy_fills
            .fetch_add(n, Relaxed);
    }

    /// Set the wall-clock of the last `MetaDb::commit` (absolute gauge).
    pub fn set_metadb_last_push_batch_ms(&self, ms: u64) {
        self.metadb_last_push_batch_ms.store(ms, Relaxed);
    }

    /// Set the wall-clock of the last compaction run (absolute gauge).
    pub fn set_metadb_last_compaction_ms(&self, ms: u64) {
        self.metadb_last_compaction_ms.store(ms, Relaxed);
    }

    // --- snapshot ---

    /// Read all counters into a plain struct.
    ///
    /// Because each load is [`Relaxed`], the snapshot is *not* a
    /// consistent cut across counters — but for diagnostics that's fine.
    #[must_use]
    pub fn snapshot(&self) -> MetricsSnapshot {
        MetricsSnapshot {
            chunk_index_persistent_hits: self.chunk_index_persistent_hits.load(Relaxed),
            shard_bloom_queries: self.shard_bloom_queries.load(Relaxed),
            shard_bloom_false_positives: self.shard_bloom_false_positives.load(Relaxed),
            staging_compression_ratio: self.staging_compression_ratio.load(Relaxed),
            adaptive_threshold_effective: self.adaptive_threshold_effective.load(Relaxed),
            chunk_index_lookups: self.chunk_index_lookups.load(Relaxed),
            chunk_index_hits: self.chunk_index_hits.load(Relaxed),
            chunk_index_shards_installed: self.chunk_index_shards_installed.load(Relaxed),
            chunk_index_entries: self.chunk_index_entries.load(Relaxed),
            staging_segments_sealed: self.staging_segments_sealed.load(Relaxed),
            staging_segments_compacted: self.staging_segments_compacted.load(Relaxed),
            staging_bytes_written: self.staging_bytes_written.load(Relaxed),
            staging_bytes_read: self.staging_bytes_read.load(Relaxed),
            staging_fsyncs: self.staging_fsyncs.load(Relaxed),
            staging_compactions_skipped_inflight: self
                .staging_compactions_skipped_inflight
                .load(Relaxed),
            push_stream_backpressure_events: self.push_stream_backpressure_events.load(Relaxed),
            head_list_requests: self.head_list_requests.load(Relaxed),
            head_point_requests: self.head_point_requests.load(Relaxed),
            head_check_errors: self.head_check_errors.load(Relaxed),
            xorbs_skipped: self.xorbs_skipped.load(Relaxed),
            cas_pipelined_commits: self.cas_pipelined_commits.load(Relaxed),
            clean_fastpath_taken: self.clean_fastpath_taken.load(Relaxed),
            clean_fastpath_false_positives: self.clean_fastpath_false_positives.load(Relaxed),
            shard_hint_hits: self.shard_hint_hits.load(Relaxed),
            shard_hint_misses: self.shard_hint_misses.load(Relaxed),
            prefetch_started: self.prefetch_started.load(Relaxed),
            prefetch_completed: self.prefetch_completed.load(Relaxed),
            prefetch_bytes: self.prefetch_bytes.load(Relaxed),
            prefetch_cache_hits: self.prefetch_cache_hits.load(Relaxed),
            xorb_fetch_requests_coalesced: self.xorb_fetch_requests_coalesced.load(Relaxed),
            xorb_fetch_bytes_saved: self.xorb_fetch_bytes_saved.load(Relaxed),
            smudge_batch_size: self.smudge_batch_size.load(Relaxed),
            multipart_resumed_uploads: self.multipart_resumed_uploads.load(Relaxed),
            upload_concurrency_current: self.upload_concurrency_current.load(Relaxed),
            dedup_total_bytes: self.dedup_total_bytes.load(Relaxed),
            dedup_saved_bytes: self.dedup_saved_bytes.load(Relaxed),
            dedup_new_bytes: self.dedup_new_bytes.load(Relaxed),
            dedup_defrag_prevented_bytes: self.dedup_defrag_prevented_bytes.load(Relaxed),
            dedup_global_bytes: self.dedup_global_bytes.load(Relaxed),
            chunks_compressed: self.chunks_compressed.load(Relaxed),
            chunks_bg4_transformed: self.chunks_bg4_transformed.load(Relaxed),
            chunks_stored_raw: self.chunks_stored_raw.load(Relaxed),
            compression_bytes_saved: self.compression_bytes_saved.load(Relaxed),
            fuse_read_count: self.fuse_read_count.load(Relaxed),
            fuse_read_bytes: self.fuse_read_bytes.load(Relaxed),
            hydration_queue_depth: self.hydration_queue_depth.load(Relaxed),
            cache_hits: self.cache_hits.load(Relaxed),
            cache_misses: self.cache_misses.load(Relaxed),
            chunk_cache_hits: self.chunk_cache_hits.load(Relaxed),
            chunk_cache_misses: self.chunk_cache_misses.load(Relaxed),
            chunk_cache_bytes: self.chunk_cache_bytes.load(Relaxed),
            push_duration_ms: self.push_duration_ms.load(Relaxed),
            bytes_uploaded: self.bytes_uploaded.load(Relaxed),
            fetch_duration_ms: self.fetch_duration_ms.load(Relaxed),
            bytes_downloaded: self.bytes_downloaded.load(Relaxed),
            gc_duration_ms: self.gc_duration_ms.load(Relaxed),
            gc_objects_deleted: self.gc_objects_deleted.load(Relaxed),
            import_files_total: self.import_files_total.load(Relaxed),
            import_versions_total: self.import_versions_total.load(Relaxed),
            import_commits_total: self.import_commits_total.load(Relaxed),
            import_bytes_source_total: self.import_bytes_source_total.load(Relaxed),
            import_bytes_staged_total: self.import_bytes_staged_total.load(Relaxed),
            import_failures_total: self.import_failures_total.load(Relaxed),
            workflow_stages_executed: self.workflow_stages_executed.load(Relaxed),
            workflow_stage_cache_hits_local: self.workflow_stage_cache_hits_local.load(Relaxed),
            workflow_stage_cache_hits_remote: self.workflow_stage_cache_hits_remote.load(Relaxed),
            workflow_stages_failed: self.workflow_stages_failed.load(Relaxed),
            workflow_stage_retry_attempts: self.workflow_stage_retry_attempts.load(Relaxed),
            workflow_journal_resumes: self.workflow_journal_resumes.load(Relaxed),
            workflow_runs_total: self.workflow_runs_total.load(Relaxed),
            workflow_stages_total: self.workflow_stages_total.load(Relaxed),
            workflow_retry_attempts_total: self.workflow_retry_attempts_total.load(Relaxed),
            workflow_cache_push_bytes_total: self.workflow_cache_push_bytes_total.load(Relaxed),
            workflow_cache_pull_bytes_total: self.workflow_cache_pull_bytes_total.load(Relaxed),
            tier_plans_generated: self.tier_plans_generated.load(Relaxed),
            tier_plans_applied: self.tier_plans_applied.load(Relaxed),
            tier_apply_failures: self.tier_apply_failures.load(Relaxed),
            tier_lifecycle_conflicts: self.tier_lifecycle_conflicts.load(Relaxed),
            hydrate_restore_requests_s3_standard: self
                .hydrate_restore_requests_s3_standard
                .load(Relaxed),
            hydrate_restore_requests_s3_expedited: self
                .hydrate_restore_requests_s3_expedited
                .load(Relaxed),
            hydrate_restore_requests_s3_bulk: self.hydrate_restore_requests_s3_bulk.load(Relaxed),
            hydrate_restore_requests_azure_high: self
                .hydrate_restore_requests_azure_high
                .load(Relaxed),
            hydrate_restore_requests_azure_standard: self
                .hydrate_restore_requests_azure_standard
                .load(Relaxed),
            hydrate_restore_completions: self.hydrate_restore_completions.load(Relaxed),
            hydrate_restore_timeouts: self.hydrate_restore_timeouts.load(Relaxed),
            gc_blocked_early_delete: self.gc_blocked_early_delete.load(Relaxed),
            gc_force_early_delete: self.gc_force_early_delete.load(Relaxed),
            optimize_xorbs_processed: self.optimize_xorbs_processed.load(Relaxed),
            optimize_xorbs_corrupt: self.optimize_xorbs_corrupt.load(Relaxed),
            cost_inventory_objects_scanned: self.cost_inventory_objects_scanned.load(Relaxed),
            speculation_hydrates_total: self.speculation_hydrates_total.load(Relaxed),
            speculation_hits_total: self.speculation_hits_total.load(Relaxed),
            speculation_evictions_total: self.speculation_evictions_total.load(Relaxed),
            manifest_cas_conflicts_total: self.manifest_cas_conflicts_total.load(Relaxed),
            manifest_cas_failures_total: self.manifest_cas_failures_total.load(Relaxed),
            metadb_close_on_drop_count: self.metadb_close_on_drop_count.load(Relaxed),
            metadb_file_index_entries: self.metadb_file_index_entries.load(Relaxed),
            metadb_file_index_sstables: self.metadb_file_index_sstables.load(Relaxed),
            metadb_file_index_wal_segments: self.metadb_file_index_wal_segments.load(Relaxed),
            metadb_chunk_index_entries: self.metadb_chunk_index_entries.load(Relaxed),
            metadb_chunk_index_sstables: self.metadb_chunk_index_sstables.load(Relaxed),
            metadb_chunk_index_wal_segments: self.metadb_chunk_index_wal_segments.load(Relaxed),
            metadb_chunk_index_cache_entries: self.metadb_chunk_index_cache_entries.load(Relaxed),
            metadb_chunk_index_cache_size_bytes: self
                .metadb_chunk_index_cache_size_bytes
                .load(Relaxed),
            metadb_get_count: self.metadb_get_count.load(Relaxed),
            metadb_get_hits: self.metadb_get_hits.load(Relaxed),
            metadb_batch_get_count: self.metadb_batch_get_count.load(Relaxed),
            metadb_batch_write_count: self.metadb_batch_write_count.load(Relaxed),
            metadb_buffered_batch_write_count: self.metadb_buffered_batch_write_count.load(Relaxed),
            metadb_wal_flush_count: self.metadb_wal_flush_count.load(Relaxed),
            metadb_memtable_flush_count: self.metadb_memtable_flush_count.load(Relaxed),
            metadb_write_bytes: self.metadb_write_bytes.load(Relaxed),
            metadb_open_count: self.metadb_open_count.load(Relaxed),
            metadb_close_count: self.metadb_close_count.load(Relaxed),
            metadb_chunk_index_cache_hits: self.metadb_chunk_index_cache_hits.load(Relaxed),
            metadb_chunk_index_cache_misses: self.metadb_chunk_index_cache_misses.load(Relaxed),
            metadb_chunk_index_cache_lazy_fills: self
                .metadb_chunk_index_cache_lazy_fills
                .load(Relaxed),
            metadb_last_push_batch_ms: self.metadb_last_push_batch_ms.load(Relaxed),
            metadb_last_compaction_ms: self.metadb_last_compaction_ms.load(Relaxed),
        }
    }
}

impl crab_xet::xorb::builder::CompressionMetrics for Metrics {
    fn add_chunks_compressed(&self, n: u64) {
        Self::add_chunks_compressed(self, n);
    }

    fn add_chunks_bg4_transformed(&self, n: u64) {
        Self::add_chunks_bg4_transformed(self, n);
    }

    fn add_chunks_stored_raw(&self, n: u64) {
        Self::add_chunks_stored_raw(self, n);
    }

    fn add_compression_bytes_saved(&self, n: u64) {
        Self::add_compression_bytes_saved(self, n);
    }
}

impl crab_workflow::WorkflowMetrics for Metrics {
    fn inc_workflow_stages_executed(&self) {
        Self::inc_workflow_stages_executed(self);
    }

    fn inc_workflow_stage_cache_hits_local(&self) {
        Self::inc_workflow_stage_cache_hits_local(self);
    }

    fn inc_workflow_stage_cache_hits_remote(&self) {
        Self::inc_workflow_stage_cache_hits_remote(self);
    }

    fn inc_workflow_stages_failed(&self) {
        Self::inc_workflow_stages_failed(self);
    }

    fn inc_workflow_stage_retry_attempts(&self) {
        Self::inc_workflow_stage_retry_attempts(self);
    }
}

impl crab_staging::StagingMetrics for Metrics {
    fn add_staging_bytes_read(&self, value: u64) {
        Self::add_staging_bytes_read(self, value);
    }

    fn add_staging_bytes_written(&self, value: u64) {
        Self::add_staging_bytes_written(self, value);
    }

    fn inc_staging_segments_sealed(&self) {
        Self::inc_staging_segments_sealed(self);
    }

    fn inc_staging_segments_compacted(&self) {
        Self::inc_staging_segments_compacted(self);
    }

    fn inc_staging_fsyncs(&self) {
        Self::inc_staging_fsyncs(self);
    }

    fn inc_staging_compactions_skipped_inflight(&self) {
        Self::inc_staging_compactions_skipped_inflight(self);
    }
}

/// Structured summary of operation-level metrics for `crab stat perf`.
///
/// Groups counters by subsystem so callers can render or serialize
/// without knowing the flat field layout of [`MetricsSnapshot`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct MetricsSummary {
    pub push_duration_ms: u64,
    pub bytes_uploaded: u64,
    pub fetch_duration_ms: u64,
    pub bytes_downloaded: u64,
    pub gc_duration_ms: u64,
    pub gc_objects_deleted: u64,
    pub chunk_index_lookups: u64,
    pub chunk_index_hits: u64,
    pub shard_bloom_queries: u64,
    pub shard_bloom_false_positives: u64,
    pub staging_bytes_written: u64,
    pub staging_bytes_read: u64,
    pub xorbs_skipped: u64,
    pub clean_fastpath_taken: u64,
    pub xorb_fetch_requests_coalesced: u64,
    pub xorb_fetch_bytes_saved: u64,
    pub multipart_resumed_uploads: u64,
    #[serde(default)]
    pub head_list_requests: u64,
    #[serde(default)]
    pub head_point_requests: u64,
    #[serde(default)]
    pub metadb_buffered_batch_write_count: u64,
    #[serde(default)]
    pub metadb_wal_flush_count: u64,
    #[serde(default)]
    pub metadb_memtable_flush_count: u64,
    // --- workflow counters ---
    #[serde(default)]
    pub workflow_runs_total: u64,
    #[serde(default)]
    pub workflow_stages_total: u64,
    #[serde(default)]
    pub workflow_retry_attempts_total: u64,
    #[serde(default)]
    pub workflow_cache_push_bytes_total: u64,
    #[serde(default)]
    pub workflow_cache_pull_bytes_total: u64,
}

impl MetricsSnapshot {
    /// Produce a structured summary of the most operationally relevant
    /// counters, suitable for `crab stat perf` output.
    #[must_use]
    pub fn summary(&self) -> MetricsSummary {
        MetricsSummary {
            push_duration_ms: self.push_duration_ms,
            bytes_uploaded: self.bytes_uploaded,
            fetch_duration_ms: self.fetch_duration_ms,
            bytes_downloaded: self.bytes_downloaded,
            gc_duration_ms: self.gc_duration_ms,
            gc_objects_deleted: self.gc_objects_deleted,
            chunk_index_lookups: self.chunk_index_lookups,
            chunk_index_hits: self.chunk_index_hits,
            shard_bloom_queries: self.shard_bloom_queries,
            shard_bloom_false_positives: self.shard_bloom_false_positives,
            staging_bytes_written: self.staging_bytes_written,
            staging_bytes_read: self.staging_bytes_read,
            xorbs_skipped: self.xorbs_skipped,
            clean_fastpath_taken: self.clean_fastpath_taken,
            xorb_fetch_requests_coalesced: self.xorb_fetch_requests_coalesced,
            xorb_fetch_bytes_saved: self.xorb_fetch_bytes_saved,
            multipart_resumed_uploads: self.multipart_resumed_uploads,
            head_list_requests: self.head_list_requests,
            head_point_requests: self.head_point_requests,
            metadb_buffered_batch_write_count: self.metadb_buffered_batch_write_count,
            metadb_wal_flush_count: self.metadb_wal_flush_count,
            metadb_memtable_flush_count: self.metadb_memtable_flush_count,
            workflow_runs_total: self.workflow_runs_total,
            workflow_stages_total: self.workflow_stages_total,
            workflow_retry_attempts_total: self.workflow_retry_attempts_total,
            workflow_cache_push_bytes_total: self.workflow_cache_push_bytes_total,
            workflow_cache_pull_bytes_total: self.workflow_cache_pull_bytes_total,
        }
    }
}

impl MetricsSummary {
    /// Return a summary with all counters set to zero.
    #[must_use]
    pub fn zeroed() -> Self {
        Self {
            push_duration_ms: 0,
            bytes_uploaded: 0,
            fetch_duration_ms: 0,
            bytes_downloaded: 0,
            gc_duration_ms: 0,
            gc_objects_deleted: 0,
            chunk_index_lookups: 0,
            chunk_index_hits: 0,
            shard_bloom_queries: 0,
            shard_bloom_false_positives: 0,
            staging_bytes_written: 0,
            staging_bytes_read: 0,
            xorbs_skipped: 0,
            clean_fastpath_taken: 0,
            xorb_fetch_requests_coalesced: 0,
            xorb_fetch_bytes_saved: 0,
            multipart_resumed_uploads: 0,
            head_list_requests: 0,
            head_point_requests: 0,
            metadb_buffered_batch_write_count: 0,
            metadb_wal_flush_count: 0,
            metadb_memtable_flush_count: 0,
            workflow_runs_total: 0,
            workflow_stages_total: 0,
            workflow_retry_attempts_total: 0,
            workflow_cache_push_bytes_total: 0,
            workflow_cache_pull_bytes_total: 0,
        }
    }

    /// Adds another process-local delta to this cumulative summary.
    pub fn merge(&mut self, delta: &Self) {
        macro_rules! add_fields {
            ($($field:ident),+ $(,)?) => {
                $(self.$field = self.$field.saturating_add(delta.$field);)+
            };
        }
        add_fields!(
            push_duration_ms,
            bytes_uploaded,
            fetch_duration_ms,
            bytes_downloaded,
            gc_duration_ms,
            gc_objects_deleted,
            chunk_index_lookups,
            chunk_index_hits,
            shard_bloom_queries,
            shard_bloom_false_positives,
            staging_bytes_written,
            staging_bytes_read,
            xorbs_skipped,
            clean_fastpath_taken,
            xorb_fetch_requests_coalesced,
            xorb_fetch_bytes_saved,
            multipart_resumed_uploads,
            head_list_requests,
            head_point_requests,
            metadb_buffered_batch_write_count,
            metadb_wal_flush_count,
            metadb_memtable_flush_count,
            workflow_runs_total,
            workflow_stages_total,
            workflow_retry_attempts_total,
            workflow_cache_push_bytes_total,
            workflow_cache_pull_bytes_total,
        );
    }

    /// Returns the monotonic delta since an earlier process-local snapshot.
    #[must_use]
    pub fn delta_since(&self, earlier: &Self) -> Self {
        let mut delta = Self::zeroed();
        macro_rules! subtract_fields {
            ($($field:ident),+ $(,)?) => {
                $(delta.$field = self.$field.saturating_sub(earlier.$field);)+
            };
        }
        subtract_fields!(
            push_duration_ms,
            bytes_uploaded,
            fetch_duration_ms,
            bytes_downloaded,
            gc_duration_ms,
            gc_objects_deleted,
            chunk_index_lookups,
            chunk_index_hits,
            shard_bloom_queries,
            shard_bloom_false_positives,
            staging_bytes_written,
            staging_bytes_read,
            xorbs_skipped,
            clean_fastpath_taken,
            xorb_fetch_requests_coalesced,
            xorb_fetch_bytes_saved,
            multipart_resumed_uploads,
            head_list_requests,
            head_point_requests,
            metadb_buffered_batch_write_count,
            metadb_wal_flush_count,
            metadb_memtable_flush_count,
            workflow_runs_total,
            workflow_stages_total,
            workflow_retry_attempts_total,
            workflow_cache_push_bytes_total,
            workflow_cache_pull_bytes_total,
        );
        delta
    }
}

impl Default for MetricsSummary {
    fn default() -> Self {
        Self::zeroed()
    }
}

/// Loads persisted cumulative counters, accepting older partial schemas.
///
/// Missing and malformed files produce a zeroed summary so diagnostics remain
/// usable after an interrupted local write.
pub fn load_metrics_summary(path: &Path) -> Result<MetricsSummary> {
    match std::fs::read_to_string(path) {
        Ok(data) => Ok(serde_json::from_str(&data).unwrap_or_default()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(MetricsSummary::zeroed()),
        Err(error) => Err(CrabError::Io(error)),
    }
}

/// Atomically merges one process-local counter delta into persistent state.
///
/// A sibling advisory lock serializes concurrent helper processes. Unknown
/// JSON fields are retained because adaptive tuning state shares this file.
pub fn persist_metrics_delta(path: &Path, delta: &MetricsSummary) -> Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty());
    if let Some(parent) = parent {
        std::fs::create_dir_all(parent)?;
    }

    let lock_path = path.with_extension("json.lock");
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(lock_path)?;
    lock.lock_exclusive()?;

    let existing_value = std::fs::read_to_string(path)
        .ok()
        .and_then(|data| serde_json::from_str::<serde_json::Value>(&data).ok())
        .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
    let mut cumulative =
        serde_json::from_value::<MetricsSummary>(existing_value.clone()).unwrap_or_default();
    cumulative.merge(delta);

    let mut output = match existing_value {
        serde_json::Value::Object(fields) => fields,
        _ => serde_json::Map::new(),
    };
    let serialized = serde_json::to_value(cumulative)
        .map_err(|error| CrabError::Internal(format!("serialize perf counters: {error}")))?;
    if let serde_json::Value::Object(fields) = serialized {
        output.extend(fields);
    }
    let json = serde_json::to_vec_pretty(&output)
        .map_err(|error| CrabError::Internal(format!("serialize perf counters: {error}")))?;
    let temp_parent = parent.unwrap_or_else(|| Path::new("."));
    let temp = tempfile::NamedTempFile::new_in(temp_parent)?;
    std::fs::write(temp.path(), json)?;
    temp.persist(path)
        .map_err(|error| CrabError::Internal(format!("persist perf counters: {error}")))?;
    Ok(())
}

impl fmt::Display for MetricsSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Push:")?;
        writeln!(f, "  duration_ms:      {}", self.push_duration_ms)?;
        writeln!(f, "  bytes_uploaded:    {}", self.bytes_uploaded)?;
        writeln!(f, "Fetch:")?;
        writeln!(f, "  duration_ms:      {}", self.fetch_duration_ms)?;
        writeln!(f, "  bytes_downloaded:  {}", self.bytes_downloaded)?;
        writeln!(f, "GC:")?;
        writeln!(f, "  duration_ms:      {}", self.gc_duration_ms)?;
        writeln!(f, "  objects_deleted:   {}", self.gc_objects_deleted)?;
        writeln!(f, "Dedup:")?;
        writeln!(f, "  index_lookups:     {}", self.chunk_index_lookups)?;
        writeln!(f, "  index_hits:        {}", self.chunk_index_hits)?;
        writeln!(f, "  bloom_queries:     {}", self.shard_bloom_queries)?;
        writeln!(
            f,
            "  bloom_false_pos:   {}",
            self.shard_bloom_false_positives
        )?;
        writeln!(f, "  xorbs_skipped:     {}", self.xorbs_skipped)?;
        writeln!(f, "  fastpath_taken:    {}", self.clean_fastpath_taken)?;
        writeln!(f, "Staging:")?;
        writeln!(f, "  bytes_written:     {}", self.staging_bytes_written)?;
        writeln!(f, "  bytes_read:        {}", self.staging_bytes_read)?;
        writeln!(f, "Smudge:")?;
        writeln!(
            f,
            "  coalesced_reqs:    {}",
            self.xorb_fetch_requests_coalesced
        )?;
        writeln!(f, "  coalesced_bytes:   {}", self.xorb_fetch_bytes_saved)?;
        writeln!(f, "Resume:")?;
        writeln!(f, "  multipart_resumed: {}", self.multipart_resumed_uploads)?;
        writeln!(f, "Cost signals:")?;
        writeln!(f, "  resume_list_reqs:  {}", self.head_list_requests)?;
        writeln!(f, "  resume_head_reqs:  {}", self.head_point_requests)?;
        writeln!(
            f,
            "  metadb_batches:    {}",
            self.metadb_buffered_batch_write_count
        )?;
        writeln!(f, "  metadb_wal_flush:  {}", self.metadb_wal_flush_count)?;
        writeln!(
            f,
            "  metadb_l0_flush:   {}",
            self.metadb_memtable_flush_count
        )?;
        writeln!(f, "Workflow:")?;
        writeln!(f, "  runs_total:        {}", self.workflow_runs_total)?;
        writeln!(f, "  stages_total:      {}", self.workflow_stages_total)?;
        writeln!(
            f,
            "  retry_attempts:    {}",
            self.workflow_retry_attempts_total
        )?;
        writeln!(
            f,
            "  cache_push_bytes:  {}",
            self.workflow_cache_push_bytes_total
        )?;
        write!(
            f,
            "  cache_pull_bytes:  {}",
            self.workflow_cache_pull_bytes_total
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_metrics_are_zeroed() {
        let m = Metrics::new();
        let snap = m.snapshot();
        assert_eq!(
            snap,
            MetricsSnapshot {
                chunk_index_persistent_hits: 0,
                shard_bloom_queries: 0,
                shard_bloom_false_positives: 0,
                staging_compression_ratio: 0,
                adaptive_threshold_effective: 0,
                chunk_index_lookups: 0,
                chunk_index_hits: 0,
                chunk_index_shards_installed: 0,
                chunk_index_entries: 0,
                staging_segments_sealed: 0,
                staging_segments_compacted: 0,
                staging_bytes_written: 0,
                staging_bytes_read: 0,
                staging_fsyncs: 0,
                staging_compactions_skipped_inflight: 0,
                push_stream_backpressure_events: 0,
                head_list_requests: 0,
                head_point_requests: 0,
                head_check_errors: 0,
                xorbs_skipped: 0,
                cas_pipelined_commits: 0,
                clean_fastpath_taken: 0,
                clean_fastpath_false_positives: 0,
                shard_hint_hits: 0,
                shard_hint_misses: 0,
                prefetch_started: 0,
                prefetch_completed: 0,
                prefetch_bytes: 0,
                prefetch_cache_hits: 0,
                xorb_fetch_requests_coalesced: 0,
                xorb_fetch_bytes_saved: 0,
                smudge_batch_size: 0,
                multipart_resumed_uploads: 0,
                upload_concurrency_current: 0,
                dedup_total_bytes: 0,
                dedup_saved_bytes: 0,
                dedup_new_bytes: 0,
                dedup_defrag_prevented_bytes: 0,
                dedup_global_bytes: 0,
                chunks_compressed: 0,
                chunks_bg4_transformed: 0,
                chunks_stored_raw: 0,
                compression_bytes_saved: 0,
                fuse_read_count: 0,
                fuse_read_bytes: 0,
                hydration_queue_depth: 0,
                cache_hits: 0,
                cache_misses: 0,
                chunk_cache_hits: 0,
                chunk_cache_misses: 0,
                chunk_cache_bytes: 0,
                push_duration_ms: 0,
                bytes_uploaded: 0,
                fetch_duration_ms: 0,
                bytes_downloaded: 0,
                gc_duration_ms: 0,
                gc_objects_deleted: 0,
                import_files_total: 0,
                import_versions_total: 0,
                import_commits_total: 0,
                import_bytes_source_total: 0,
                import_bytes_staged_total: 0,
                import_failures_total: 0,
                workflow_stages_executed: 0,
                workflow_stage_cache_hits_local: 0,
                workflow_stage_cache_hits_remote: 0,
                workflow_stages_failed: 0,
                workflow_stage_retry_attempts: 0,
                workflow_journal_resumes: 0,
                workflow_runs_total: 0,
                workflow_stages_total: 0,
                workflow_retry_attempts_total: 0,
                workflow_cache_push_bytes_total: 0,
                workflow_cache_pull_bytes_total: 0,
                tier_plans_generated: 0,
                tier_plans_applied: 0,
                tier_apply_failures: 0,
                tier_lifecycle_conflicts: 0,
                hydrate_restore_requests_s3_standard: 0,
                hydrate_restore_requests_s3_expedited: 0,
                hydrate_restore_requests_s3_bulk: 0,
                hydrate_restore_requests_azure_high: 0,
                hydrate_restore_requests_azure_standard: 0,
                hydrate_restore_completions: 0,
                hydrate_restore_timeouts: 0,
                gc_blocked_early_delete: 0,
                gc_force_early_delete: 0,
                optimize_xorbs_processed: 0,
                optimize_xorbs_corrupt: 0,
                cost_inventory_objects_scanned: 0,
                speculation_hydrates_total: 0,
                speculation_hits_total: 0,
                speculation_evictions_total: 0,
                manifest_cas_conflicts_total: 0,
                manifest_cas_failures_total: 0,
                metadb_close_on_drop_count: 0,
                metadb_file_index_entries: 0,
                metadb_file_index_sstables: 0,
                metadb_file_index_wal_segments: 0,
                metadb_chunk_index_entries: 0,
                metadb_chunk_index_sstables: 0,
                metadb_chunk_index_wal_segments: 0,
                metadb_chunk_index_cache_entries: 0,
                metadb_chunk_index_cache_size_bytes: 0,
                metadb_get_count: 0,
                metadb_get_hits: 0,
                metadb_batch_get_count: 0,
                metadb_batch_write_count: 0,
                metadb_buffered_batch_write_count: 0,
                metadb_wal_flush_count: 0,
                metadb_memtable_flush_count: 0,
                metadb_write_bytes: 0,
                metadb_open_count: 0,
                metadb_close_count: 0,
                metadb_chunk_index_cache_hits: 0,
                metadb_chunk_index_cache_misses: 0,
                metadb_chunk_index_cache_lazy_fills: 0,
                metadb_last_push_batch_ms: 0,
                metadb_last_compaction_ms: 0,
            }
        );
    }

    #[test]
    fn increment_and_snapshot_round_trip() {
        let m = Metrics::new();

        m.inc_chunk_index_persistent_hits();
        m.inc_shard_bloom_queries();
        m.inc_shard_bloom_queries();
        m.inc_shard_bloom_queries();
        m.inc_shard_bloom_false_positives();
        m.inc_staging_compression_ratio(750);
        m.inc_adaptive_threshold_effective(250);

        let snap = m.snapshot();
        assert_eq!(snap.chunk_index_persistent_hits, 1);
        assert_eq!(snap.shard_bloom_queries, 3);
        assert_eq!(snap.shard_bloom_false_positives, 1);
        assert_eq!(snap.staging_compression_ratio, 750);
        assert_eq!(snap.adaptive_threshold_effective, 250);
    }

    #[test]
    fn counters_are_monotonic_across_many_increments() {
        let m = Metrics::new();
        for _ in 0..1_000 {
            m.inc_shard_bloom_queries();
        }
        assert_eq!(m.snapshot().shard_bloom_queries, 1_000);
    }

    #[test]
    fn snapshot_is_independent_copy() {
        let m = Metrics::new();
        m.inc_chunk_index_persistent_hits();
        let snap1 = m.snapshot();

        m.inc_chunk_index_persistent_hits();
        let snap2 = m.snapshot();

        assert_eq!(snap1.chunk_index_persistent_hits, 1);
        assert_eq!(snap2.chunk_index_persistent_hits, 2);
    }

    #[test]
    fn head_list_requests_increment_and_add() {
        let m = Metrics::new();
        m.inc_head_list_requests();
        assert_eq!(m.snapshot().head_list_requests, 1);

        m.add_head_list_requests(4);
        assert_eq!(m.snapshot().head_list_requests, 5);
    }

    #[test]
    fn head_point_requests_increment_and_add() {
        let m = Metrics::new();
        m.inc_head_point_requests();
        assert_eq!(m.snapshot().head_point_requests, 1);

        m.add_head_point_requests(9);
        assert_eq!(m.snapshot().head_point_requests, 10);
    }

    #[test]
    fn head_check_errors_increment_and_add() {
        let m = Metrics::new();
        m.inc_head_check_errors();
        assert_eq!(m.snapshot().head_check_errors, 1);

        m.add_head_check_errors(4);
        assert_eq!(m.snapshot().head_check_errors, 5);
    }

    #[test]
    fn xorbs_skipped_increment_and_add() {
        let m = Metrics::new();
        m.inc_xorbs_skipped();
        assert_eq!(m.snapshot().xorbs_skipped, 1);

        m.add_xorbs_skipped(7);
        assert_eq!(m.snapshot().xorbs_skipped, 8);
    }

    #[test]
    fn shard_hint_counters_increment_independently() {
        let m = Metrics::new();
        m.inc_shard_hint_hits();
        m.inc_shard_hint_hits();
        m.inc_shard_hint_misses();

        let snap = m.snapshot();
        assert_eq!(snap.shard_hint_hits, 2);
        assert_eq!(snap.shard_hint_misses, 1);
    }

    #[test]
    fn prefetch_counters_increment_independently() {
        let m = Metrics::new();
        m.inc_prefetch_started();
        m.inc_prefetch_started();
        m.inc_prefetch_completed();
        m.add_prefetch_bytes(4096);
        m.inc_prefetch_cache_hits();

        let snap = m.snapshot();
        assert_eq!(snap.prefetch_started, 2);
        assert_eq!(snap.prefetch_completed, 1);
        assert_eq!(snap.prefetch_bytes, 4096);
        assert_eq!(snap.prefetch_cache_hits, 1);
    }

    #[test]
    fn chunk_cache_counters_accumulate_and_override() {
        let m = Metrics::new();
        m.inc_chunk_cache_hits();
        m.add_chunk_cache_hits(4);
        m.inc_chunk_cache_misses();
        m.add_chunk_cache_misses(2);
        m.set_chunk_cache_bytes(10_000);

        let snap = m.snapshot();
        assert_eq!(snap.chunk_cache_hits, 5);
        assert_eq!(snap.chunk_cache_misses, 3);
        assert_eq!(snap.chunk_cache_bytes, 10_000);

        // set_chunk_cache_bytes is absolute, not additive.
        m.set_chunk_cache_bytes(42);
        assert_eq!(m.snapshot().chunk_cache_bytes, 42);
    }

    #[test]
    fn operation_counters_add_and_accumulate() {
        let m = Metrics::new();

        m.add_push_duration_ms(150);
        m.add_push_duration_ms(200);
        m.add_bytes_uploaded(1024);
        m.add_fetch_duration_ms(300);
        m.add_bytes_downloaded(2048);
        m.add_gc_duration_ms(500);
        m.add_gc_objects_deleted(10);
        m.add_gc_objects_deleted(5);

        let snap = m.snapshot();
        assert_eq!(snap.push_duration_ms, 350);
        assert_eq!(snap.bytes_uploaded, 1024);
        assert_eq!(snap.fetch_duration_ms, 300);
        assert_eq!(snap.bytes_downloaded, 2048);
        assert_eq!(snap.gc_duration_ms, 500);
        assert_eq!(snap.gc_objects_deleted, 15);
    }

    #[test]
    fn import_counters_accumulate_independently() {
        let m = Metrics::new();
        m.add_import_files_total(3);
        m.add_import_versions_total(7);
        m.inc_import_commits_total();
        m.inc_import_commits_total();
        m.add_import_bytes_source_total(1_000_000);
        m.add_import_bytes_staged_total(900_000);
        m.inc_import_failures_total();

        let snap = m.snapshot();
        assert_eq!(snap.import_files_total, 3);
        assert_eq!(snap.import_versions_total, 7);
        assert_eq!(snap.import_commits_total, 2);
        assert_eq!(snap.import_bytes_source_total, 1_000_000);
        assert_eq!(snap.import_bytes_staged_total, 900_000);
        assert_eq!(snap.import_failures_total, 1);
    }

    #[test]
    fn summary_reflects_snapshot_values() {
        let m = Metrics::new();
        m.add_push_duration_ms(100);
        m.add_bytes_uploaded(512);
        m.add_fetch_duration_ms(200);
        m.add_bytes_downloaded(1024);
        m.add_gc_duration_ms(50);
        m.add_gc_objects_deleted(3);
        m.inc_chunk_index_lookups();
        m.inc_chunk_index_hits();

        let summary = m.snapshot().summary();
        assert_eq!(summary.push_duration_ms, 100);
        assert_eq!(summary.bytes_uploaded, 512);
        assert_eq!(summary.fetch_duration_ms, 200);
        assert_eq!(summary.bytes_downloaded, 1024);
        assert_eq!(summary.gc_duration_ms, 50);
        assert_eq!(summary.gc_objects_deleted, 3);
        assert_eq!(summary.chunk_index_lookups, 1);
        assert_eq!(summary.chunk_index_hits, 1);
    }

    #[test]
    fn summary_display_contains_sections() {
        let m = Metrics::new();
        m.add_push_duration_ms(42);
        m.add_gc_objects_deleted(7);

        let text = m.snapshot().summary().to_string();
        assert!(text.contains("Push:"));
        assert!(text.contains("Fetch:"));
        assert!(text.contains("GC:"));
        assert!(text.contains("42"));
        assert!(text.contains("7"));
    }

    #[test]
    fn set_dedup_metrics_copies_all_fields() {
        use xet_data::deduplication::DeduplicationMetrics;

        let m = Metrics::new();
        let dm = DeduplicationMetrics {
            total_bytes: 10_000,
            deduped_bytes: 7_000,
            new_bytes: 3_000,
            defrag_prevented_dedup_bytes: 500,
            deduped_bytes_by_global_dedup: 2_000,
            ..DeduplicationMetrics::default()
        };

        m.set_dedup_metrics(&dm);
        let snap = m.snapshot();
        assert_eq!(snap.dedup_total_bytes, 10_000);
        assert_eq!(snap.dedup_saved_bytes, 7_000);
        assert_eq!(snap.dedup_new_bytes, 3_000);
        assert_eq!(snap.dedup_defrag_prevented_bytes, 500);
        assert_eq!(snap.dedup_global_bytes, 2_000);

        // Calling again overwrites (absolute store, not additive).
        let dm2 = DeduplicationMetrics {
            total_bytes: 42,
            deduped_bytes: 0,
            new_bytes: 42,
            defrag_prevented_dedup_bytes: 0,
            deduped_bytes_by_global_dedup: 0,
            ..DeduplicationMetrics::default()
        };
        m.set_dedup_metrics(&dm2);
        let snap2 = m.snapshot();
        assert_eq!(snap2.dedup_total_bytes, 42);
        assert_eq!(snap2.dedup_saved_bytes, 0);
    }

    #[test]
    fn workflow_counters_default_to_zero_and_increment_by_one() {
        let m = Metrics::new();
        let zero = m.snapshot();
        assert_eq!(zero.workflow_stages_executed, 0);
        assert_eq!(zero.workflow_stage_cache_hits_local, 0);
        assert_eq!(zero.workflow_stage_cache_hits_remote, 0);
        assert_eq!(zero.workflow_stages_failed, 0);
        assert_eq!(zero.workflow_stage_retry_attempts, 0);
        assert_eq!(zero.workflow_journal_resumes, 0);
        assert_eq!(zero.workflow_runs_total, 0);
        assert_eq!(zero.workflow_stages_total, 0);
        assert_eq!(zero.workflow_retry_attempts_total, 0);
        assert_eq!(zero.workflow_cache_push_bytes_total, 0);
        assert_eq!(zero.workflow_cache_pull_bytes_total, 0);

        m.inc_workflow_stages_executed();
        m.inc_workflow_stage_cache_hits_local();
        m.inc_workflow_stage_cache_hits_remote();
        m.inc_workflow_stages_failed();
        m.inc_workflow_stage_retry_attempts();
        m.inc_workflow_journal_resumes();
        m.inc_workflow_runs_total();
        m.inc_workflow_stages_total();
        m.inc_workflow_retry_attempts_total();
        m.add_workflow_cache_push_bytes_total(1024);
        m.add_workflow_cache_pull_bytes_total(2048);

        let snap = m.snapshot();
        assert_eq!(snap.workflow_stages_executed, 1);
        assert_eq!(snap.workflow_stage_cache_hits_local, 1);
        assert_eq!(snap.workflow_stage_cache_hits_remote, 1);
        assert_eq!(snap.workflow_stages_failed, 1);
        assert_eq!(snap.workflow_stage_retry_attempts, 1);
        assert_eq!(snap.workflow_journal_resumes, 1);
        assert_eq!(snap.workflow_runs_total, 1);
        assert_eq!(snap.workflow_stages_total, 1);
        assert_eq!(snap.workflow_retry_attempts_total, 1);
        assert_eq!(snap.workflow_cache_push_bytes_total, 1024);
        assert_eq!(snap.workflow_cache_pull_bytes_total, 2048);
    }

    #[test]
    fn workflow_counters_accumulate_across_many_increments() {
        let m = Metrics::new();
        for _ in 0..100 {
            m.inc_workflow_stages_executed();
            m.inc_workflow_stage_retry_attempts();
        }
        let snap = m.snapshot();
        assert_eq!(snap.workflow_stages_executed, 100);
        assert_eq!(snap.workflow_stage_retry_attempts, 100);
        // Untouched counters remain zero.
        assert_eq!(snap.workflow_stage_cache_hits_local, 0);
        assert_eq!(snap.workflow_stages_failed, 0);
    }

    #[test]
    fn storage_economy_counters_default_to_zero_and_increment() {
        let m = Metrics::new();
        let zero = m.snapshot();
        assert_eq!(zero.tier_plans_generated, 0);
        assert_eq!(zero.tier_plans_applied, 0);
        assert_eq!(zero.tier_apply_failures, 0);
        assert_eq!(zero.tier_lifecycle_conflicts, 0);
        assert_eq!(zero.hydrate_restore_requests_s3_standard, 0);
        assert_eq!(zero.hydrate_restore_requests_s3_expedited, 0);
        assert_eq!(zero.hydrate_restore_requests_s3_bulk, 0);
        assert_eq!(zero.hydrate_restore_requests_azure_high, 0);
        assert_eq!(zero.hydrate_restore_requests_azure_standard, 0);
        assert_eq!(zero.hydrate_restore_completions, 0);
        assert_eq!(zero.hydrate_restore_timeouts, 0);
        assert_eq!(zero.gc_blocked_early_delete, 0);
        assert_eq!(zero.gc_force_early_delete, 0);
        assert_eq!(zero.optimize_xorbs_processed, 0);
        assert_eq!(zero.optimize_xorbs_corrupt, 0);
        assert_eq!(zero.cost_inventory_objects_scanned, 0);

        m.inc_tier_plans_generated();
        m.inc_tier_plans_applied();
        m.inc_tier_apply_failures();
        m.inc_tier_lifecycle_conflicts();
        m.inc_hydrate_restore_requests_s3_standard();
        m.inc_hydrate_restore_requests_s3_expedited();
        m.inc_hydrate_restore_requests_s3_bulk();
        m.inc_hydrate_restore_requests_azure_high();
        m.inc_hydrate_restore_requests_azure_standard();
        m.inc_hydrate_restore_completions();
        m.inc_hydrate_restore_timeouts();
        m.inc_gc_blocked_early_delete();
        m.inc_gc_force_early_delete();
        m.inc_optimize_xorbs_processed();
        m.inc_optimize_xorbs_corrupt();
        m.add_cost_inventory_objects_scanned(500);

        let snap = m.snapshot();
        assert_eq!(snap.tier_plans_generated, 1);
        assert_eq!(snap.tier_plans_applied, 1);
        assert_eq!(snap.tier_apply_failures, 1);
        assert_eq!(snap.tier_lifecycle_conflicts, 1);
        assert_eq!(snap.hydrate_restore_requests_s3_standard, 1);
        assert_eq!(snap.hydrate_restore_requests_s3_expedited, 1);
        assert_eq!(snap.hydrate_restore_requests_s3_bulk, 1);
        assert_eq!(snap.hydrate_restore_requests_azure_high, 1);
        assert_eq!(snap.hydrate_restore_requests_azure_standard, 1);
        assert_eq!(snap.hydrate_restore_completions, 1);
        assert_eq!(snap.hydrate_restore_timeouts, 1);
        assert_eq!(snap.gc_blocked_early_delete, 1);
        assert_eq!(snap.gc_force_early_delete, 1);
        assert_eq!(snap.optimize_xorbs_processed, 1);
        assert_eq!(snap.optimize_xorbs_corrupt, 1);
        assert_eq!(snap.cost_inventory_objects_scanned, 500);
    }

    #[test]
    fn speculation_counters_default_to_zero_and_increment() {
        let m = Metrics::new();
        let zero = m.snapshot();
        assert_eq!(zero.speculation_hydrates_total, 0);
        assert_eq!(zero.speculation_hits_total, 0);
        assert_eq!(zero.speculation_evictions_total, 0);

        m.inc_speculation_hydrates_total();
        m.inc_speculation_hydrates_total();
        m.inc_speculation_hits_total();
        m.inc_speculation_evictions_total();
        m.inc_speculation_evictions_total();
        m.inc_speculation_evictions_total();

        let snap = m.snapshot();
        assert_eq!(snap.speculation_hydrates_total, 2);
        assert_eq!(snap.speculation_hits_total, 1);
        assert_eq!(snap.speculation_evictions_total, 3);
    }

    #[test]
    fn persisted_deltas_accumulate_and_preserve_tuning_state() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("perf-state.json");
        std::fs::write(&path, r#"{"samples":[0.25]}"#).expect("seed state");

        let mut first = MetricsSummary::zeroed();
        first.bytes_uploaded = 12;
        first.metadb_wal_flush_count = 1;
        persist_metrics_delta(&path, &first).expect("persist first delta");

        let mut second = MetricsSummary::zeroed();
        second.bytes_uploaded = 30;
        second.head_point_requests = 4;
        persist_metrics_delta(&path, &second).expect("persist second delta");

        let loaded = load_metrics_summary(&path).expect("load counters");
        assert_eq!(loaded.bytes_uploaded, 42);
        assert_eq!(loaded.metadb_wal_flush_count, 1);
        assert_eq!(loaded.head_point_requests, 4);
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).expect("read state")).expect("parse state");
        assert_eq!(value["samples"], serde_json::json!([0.25]));
    }

    #[test]
    fn partial_older_summary_defaults_missing_counters() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("perf-state.json");
        std::fs::write(&path, r#"{"bytes_uploaded":9}"#).expect("seed state");

        let loaded = load_metrics_summary(&path).expect("load counters");
        assert_eq!(loaded.bytes_uploaded, 9);
        assert_eq!(loaded.metadb_memtable_flush_count, 0);
    }
}
