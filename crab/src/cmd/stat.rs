//! `crab stat` subcommand — prints staging area statistics.
//! `crab stat perf` — prints persisted performance counters.

use std::path::Path;

use crate::core::error::Result;
use crate::core::metrics::{MetricsSummary, load_metrics_summary};
use crate::core::output::{OutputMode, emit_json};
use crab_staging::push_plan::{PushPlanStats, PushPlanSummaryOptions, empty_push_plan_stats};
use crab_staging::stats::StagingStats;
use crab_staging::{StagingAreaReadOnly, StagingError};
use serde::Serialize;

/// Payload emitted by `crab stat --json`.
#[derive(Serialize, schemars::JsonSchema)]
pub struct StatPayload {
    #[serde(flatten)]
    pub stats: StagingStats,
}

/// Open the staging area at `root` and print a human-readable stats summary.
///
/// # Errors
///
/// Returns [`crate::core::error::CrabError`] on staging open or query failure.
pub async fn run(root: &Path, mode: OutputMode) -> Result<()> {
    let staging = StagingAreaReadOnly::open(root.to_path_buf()).await?;
    let stats = staging.stats()?;

    if mode == OutputMode::Json {
        let payload = StatPayload { stats };
        emit_json("stat", "1.0", payload);
        return Ok(());
    }

    println!("Staging area: {}", root.display());
    println!("  Sealed segments:       {}", stats.segments_sealed);
    println!("  Current segment bytes: {}", stats.current_segment_bytes);
    println!("  Total staged bytes:    {}", stats.total_staged_bytes);
    println!("  Live bytes:            {}", stats.live_bytes);
    println!("  Dead bytes:            {}", stats.dead_bytes);
    println!("  Dead ratio:            {:.2}%", stats.dead_ratio * 100.0);
    println!("  Chunk count:           {}", stats.chunk_count);
    println!("  File count:            {}", stats.file_count);

    Ok(())
}

/// Print add-time push-plan inventory for the staging area.
///
/// # Errors
///
/// Returns [`crate::core::error::CrabError`] on filesystem failures.
pub async fn run_push_plan(root: &Path, verify: bool, mode: OutputMode) -> Result<()> {
    let options = PushPlanSummaryOptions {
        verify_prepared_xorbs: verify,
    };
    let stats = push_plan_stats_for_root(root, options).await?;

    if mode == OutputMode::Json {
        emit_json("stat.push-plan", "1.0", StatPushPlanPayload { stats });
        return Ok(());
    }

    println!("Push plan store: {}", root.display());
    println!(
        "  Prepared xorb verify: {}",
        if stats.verified_prepared_xorbs {
            "enabled"
        } else {
            "disabled"
        }
    );
    println!("  Valid file plans:       {}", stats.plan_files);
    println!("  Invalid file plans:     {}", stats.invalid_plan_files);
    println!(
        "  Planned file bytes:     {}",
        format_size(stats.planned_file_bytes)
    );
    println!("  Planned chunk refs:     {}", stats.planned_chunks);
    println!("  Existing chunk refs:    {}", stats.existing_chunks);
    println!(
        "  Prepared xorbs:         {} ({})",
        stats.prepared_xorbs,
        format_size(stats.prepared_bytes)
    );
    println!("  Prepared chunk refs:    {}", stats.prepared_chunks);
    println!("  Indexed prepared rows:  {}", stats.indexed_prepared_xorbs);
    println!(
        "  Orphaned indexed rows:  {}",
        stats.orphaned_indexed_prepared_xorbs
    );
    println!(
        "  Invalid indexed rows:   {}",
        stats.invalid_indexed_prepared_xorbs
    );
    println!(
        "  Prepared files on disk: {} ({})",
        stats.referenced_prepared_xorb_files,
        format_size(stats.referenced_prepared_xorb_file_bytes)
    );
    println!(
        "  Missing prepared files: {}",
        stats.missing_prepared_xorb_files
    );
    println!(
        "  Size mismatches:        {}",
        stats.mismatched_prepared_xorb_files
    );
    println!(
        "  Stale prepared files:   {} ({})",
        stats.stale_prepared_xorb_files,
        format_size(stats.stale_prepared_xorb_file_bytes)
    );
    if stats.verified_prepared_xorbs {
        println!(
            "  Verified prepared:      {} ({})",
            stats.verified_prepared_xorb_files,
            format_size(stats.verified_prepared_xorb_file_bytes)
        );
        println!(
            "  Payload hash mismatch:  {}",
            stats.payload_hash_mismatched_prepared_xorb_files
        );
        println!(
            "  Corrupt prepared files: {}",
            stats.corrupt_prepared_xorb_files
        );
        println!(
            "  Metadata mismatches:    {}",
            stats.metadata_mismatched_prepared_xorb_files
        );
    }

    Ok(())
}

async fn push_plan_stats_for_root(
    root: &Path,
    options: PushPlanSummaryOptions,
) -> Result<PushPlanStats> {
    match StagingAreaReadOnly::open(root.to_path_buf()).await {
        Ok(staging) => staging.push_plan_stats(options).await.map_err(Into::into),
        Err(StagingError::NotFound { .. }) => Ok(empty_push_plan_stats(options)),
        Err(error) => Err(error.into()),
    }
}

/// Payload emitted by `crab stat push-plan --json`.
#[derive(Serialize, schemars::JsonSchema)]
pub struct StatPushPlanPayload {
    #[serde(flatten)]
    pub stats: PushPlanStats,
}

/// Read persisted perf counters and print the `MetricsSummary`.
/// If the file doesn't exist, prints zeroed counters.
///
/// Uses the provided `perf_path` from the resolved config, falling back
/// to the default `.crab/perf-state.json`.
///
/// # Errors
///
/// Returns [`crate::core::error::CrabError`] on unexpected I/O failure.
pub fn run_perf(perf_path: &str, mode: OutputMode) -> Result<()> {
    let path = Path::new(perf_path);
    let summary = load_perf_summary(path)?;

    if mode == OutputMode::Json {
        emit_json("stat.perf", "1.0", &summary);
        return Ok(());
    }

    println!("{summary}");
    Ok(())
}

/// Load a [`MetricsSummary`] from a JSON file, returning a zeroed summary
/// if the file is missing or malformed.
fn load_perf_summary(path: &Path) -> Result<MetricsSummary> {
    load_metrics_summary(path)
}

/// Payload emitted by `crab stat classes --json`.
#[derive(Serialize, schemars::JsonSchema)]
pub struct StatClassesPayload {
    /// Per-class breakdown.
    pub classes: Vec<ClassEntry>,
    /// Total bytes across all classes.
    pub total_bytes: u64,
    /// Total objects across all classes.
    pub total_objects: u64,
}

/// A single class entry in the stat classes output.
#[derive(Serialize, schemars::JsonSchema)]
pub struct ClassEntry {
    /// Storage class name.
    pub class: String,
    /// Total bytes in this class.
    pub bytes: u64,
    /// Number of objects in this class.
    pub objects: u64,
    /// Share of total bytes as a fraction (0.0-1.0).
    pub share: f64,
}

/// Run `crab stat classes` — per-storage-class bytes and object counts.
///
/// Reuses the inventory subsystem. Currently outputs a placeholder
/// since connecting to a live bucket requires store configuration.
///
/// # Errors
///
/// Returns [`crate::core::error::CrabError`] on failure.
pub async fn run_classes(mode: OutputMode) -> Result<()> {
    // In a full implementation, this would:
    // 1. Resolve the store from config
    // 2. Run a live inventory walk (or read a report)
    // 3. Aggregate per-class stats
    // For now, emit a placeholder indicating the feature is available.

    let payload = StatClassesPayload {
        classes: Vec::new(),
        total_bytes: 0,
        total_objects: 0,
    };

    if mode == OutputMode::Json {
        emit_json("stat.classes", "1.0", &payload);
        return Ok(());
    }

    println!("crab stat classes\n");
    println!("  No inventory data available.");
    println!("  Run from a crab-initialized repo with a connected bucket.");

    Ok(())
}

fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    if bytes >= GB {
        format!("{:.2} GiB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MiB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KiB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_missing_file_returns_zeroed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("nonexistent.json");
        let summary = load_perf_summary(&path).expect("load");
        assert_eq!(summary, MetricsSummary::zeroed());
    }

    #[test]
    fn load_corrupt_file_returns_zeroed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("perf-state.json");
        std::fs::write(&path, "not valid json {{{").expect("write");
        let summary = load_perf_summary(&path).expect("load");
        assert_eq!(summary, MetricsSummary::zeroed());
    }

    #[test]
    fn load_valid_file_round_trips() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("perf-state.json");

        let original = MetricsSummary {
            push_duration_ms: 42,
            bytes_uploaded: 1024,
            fetch_duration_ms: 100,
            bytes_downloaded: 2048,
            gc_duration_ms: 10,
            gc_objects_deleted: 3,
            chunk_index_lookups: 50,
            chunk_index_hits: 25,
            shard_bloom_queries: 100,
            shard_bloom_false_positives: 2,
            staging_bytes_written: 4096,
            staging_bytes_read: 3072,
            xorbs_skipped: 5,
            clean_fastpath_taken: 8,
            xorb_fetch_requests_coalesced: 12,
            xorb_fetch_bytes_saved: 512,
            multipart_resumed_uploads: 1,
            head_list_requests: 2,
            head_point_requests: 4,
            metadb_buffered_batch_write_count: 8,
            metadb_wal_flush_count: 1,
            metadb_memtable_flush_count: 1,
            workflow_runs_total: 0,
            workflow_stages_total: 0,
            workflow_retry_attempts_total: 0,
            workflow_cache_push_bytes_total: 0,
            workflow_cache_pull_bytes_total: 0,
        };

        let json = serde_json::to_string_pretty(&original).expect("serialize");
        std::fs::write(&path, json).expect("write");

        let loaded = load_perf_summary(&path).expect("load");
        assert_eq!(loaded, original);
    }

    #[test]
    fn zeroed_summary_has_all_zeros() {
        let z = MetricsSummary::zeroed();
        assert_eq!(z.push_duration_ms, 0);
        assert_eq!(z.bytes_uploaded, 0);
        assert_eq!(z.fetch_duration_ms, 0);
        assert_eq!(z.bytes_downloaded, 0);
        assert_eq!(z.gc_duration_ms, 0);
        assert_eq!(z.gc_objects_deleted, 0);
        assert_eq!(z.chunk_index_lookups, 0);
        assert_eq!(z.chunk_index_hits, 0);
        assert_eq!(z.shard_bloom_queries, 0);
        assert_eq!(z.shard_bloom_false_positives, 0);
        assert_eq!(z.staging_bytes_written, 0);
        assert_eq!(z.staging_bytes_read, 0);
        assert_eq!(z.xorbs_skipped, 0);
        assert_eq!(z.clean_fastpath_taken, 0);
        assert_eq!(z.xorb_fetch_requests_coalesced, 0);
        assert_eq!(z.xorb_fetch_bytes_saved, 0);
        assert_eq!(z.multipart_resumed_uploads, 0);
        assert_eq!(z.workflow_runs_total, 0);
        assert_eq!(z.workflow_stages_total, 0);
        assert_eq!(z.workflow_retry_attempts_total, 0);
        assert_eq!(z.workflow_cache_push_bytes_total, 0);
        assert_eq!(z.workflow_cache_pull_bytes_total, 0);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn push_plan_stats_for_missing_staging_returns_empty_inventory() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let stats = push_plan_stats_for_root(
            &tmp.path().join("missing-staging"),
            PushPlanSummaryOptions {
                verify_prepared_xorbs: true,
            },
        )
        .await
        .expect("stats for missing staging");

        assert!(stats.verified_prepared_xorbs);
        assert_eq!(stats.plan_files, 0);
        assert_eq!(stats.invalid_plan_files, 0);
        assert_eq!(stats.prepared_xorbs, 0);
        assert_eq!(stats.stale_prepared_xorb_files, 0);
    }
}
