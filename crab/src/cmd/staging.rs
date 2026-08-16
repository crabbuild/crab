//! `crab staging stats` and `crab staging clean` subcommands.

use std::path::Path;

use serde::Serialize;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::core::error::{Result, check_cancelled};
use crate::core::output::{OutputMode, emit_json};
use crab_staging::stats::{StagingStats, StagingVerifyStats};
use crab_staging::{StagingArea, StagingAreaReadOnly};
use crab_types::pointer::hex_encode;

/// Default staging root relative to the repository root.
const STAGING_ROOT: &str = ".crab/staging";

/// Payload emitted by `crab staging stats --json`.
#[derive(Serialize, schemars::JsonSchema)]
pub struct StagingStatsPayload {
    #[serde(flatten)]
    pub stats: StagingStats,
    /// Per-file breakdown of staged chunks.
    pub files: Vec<StagedFileEntry>,
}

/// Per-file entry in the staging stats JSON output.
#[derive(Serialize, schemars::JsonSchema)]
pub struct StagedFileEntry {
    /// Hex-encoded Blake3 file hash.
    pub file_hash: String,
    /// Original file path relative to repo root, if recorded.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    /// Original file size in bytes.
    pub total_bytes: u64,
    /// Number of committed chunks (flushed to segment).
    pub committed_chunks: u64,
    /// Number of pending chunks (not yet flushed).
    pub pending_chunks: u64,
    /// Total chunk count (committed + pending).
    pub total_chunks: u64,
    /// Number of distinct segments holding this file's data.
    pub segments: u64,
}

/// Open the staging area and print a human-readable stats summary.
///
/// Uses a read-only (shared lock) handle so it never blocks writers.
///
/// # Errors
///
/// Returns [`crate::core::error::CrabError`] on staging open or query failure.
pub async fn run_staging_stats(mode: OutputMode) -> Result<()> {
    let root = Path::new(STAGING_ROOT);
    let staging = StagingAreaReadOnly::open(root.to_path_buf()).await?;

    let stats = staging.stats()?;
    let file_list = staging.list_files()?;

    if mode == OutputMode::Json {
        let files: Vec<StagedFileEntry> = file_list
            .iter()
            .map(|f| StagedFileEntry {
                file_hash: hex_encode(&f.file_hash),
                file_path: f.file_path.clone(),
                total_bytes: f.total_bytes,
                committed_chunks: f.committed_chunks,
                pending_chunks: f.pending_chunks,
                total_chunks: f.committed_chunks + f.pending_chunks,
                segments: f.segments,
            })
            .collect();
        let payload = StagingStatsPayload { stats, files };
        emit_json("staging.stats", "1.0", payload);
        return Ok(());
    }

    let inflight = staging.list_inflight()?;

    println!("Staging area: {}", root.display());
    println!("  Sealed segments:       {}", stats.segments_sealed);
    println!("  Current segment bytes: {}", stats.current_segment_bytes);
    println!("  Total staged bytes:    {}", stats.total_staged_bytes);
    println!("  Live bytes:            {}", stats.live_bytes);
    println!("  Dead bytes:            {}", stats.dead_bytes);
    println!("  Dead ratio:            {:.2}%", stats.dead_ratio * 100.0);
    println!("  Chunk count:           {}", stats.chunk_count);
    println!("  File count:            {}", stats.file_count);
    println!("  Inflight markers:      {}", inflight.len());

    if !file_list.is_empty() {
        println!("\n  Files:");
        for f in &file_list {
            let hash_hex = hex_encode(&f.file_hash);
            let total_chunks = f.committed_chunks + f.pending_chunks;
            let display_name = f.file_path.as_deref().unwrap_or(&hash_hex[..16]);
            println!(
                "    {} {:>10}  {} chunks ({} committed, {} pending) in {} seg{}",
                display_name,
                format_size(f.total_bytes),
                total_chunks,
                f.committed_chunks,
                f.pending_chunks,
                f.segments,
                if f.segments == 1 { "" } else { "s" },
            );
        }
    }

    Ok(())
}

/// Open the staging area and verify registered files plus chunk payloads.
///
/// Uses a read-only shared lock and the same chunk readers as push, so
/// segment CRC and Blake3 mismatches surface before the user reaches a
/// later upload or hydrate path.
///
/// # Errors
///
/// Returns [`crate::core::error::CrabError`] on staging open, index
/// inconsistency, or staged payload corruption.
pub async fn run_staging_verify() -> Result<StagingVerifyStats> {
    let root = Path::new(STAGING_ROOT);
    let staging = StagingAreaReadOnly::open(root.to_path_buf()).await?;
    let summary = staging.verify().await?;

    println!(
        "Verified staging area: {} file(s), {} chunk reference(s), {} unique chunk(s), {}",
        summary.files_checked,
        summary.chunk_refs_checked,
        summary.unique_chunks_checked,
        format_size(summary.bytes_checked),
    );

    Ok(summary)
}

/// Format bytes as a human-readable size string.
fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{bytes} B")
    }
}

/// Open the staging area and purge stale data.
///
/// Delegates to [`StagingArea::clean`] which removes stale inflight
/// markers, sweeps orphan segments, and optionally compacts.
///
/// When `force` is true, attempts to break a stale lock held by a dead
/// process before opening. This is safe because `flock` is advisory and
/// the PID liveness check ensures we only break locks from dead holders.
///
/// When `prune_abandoned` is true, additionally reclaims segments that
/// were rolled over but never sealed (pre-fix orphans from a crashed
/// `crab add` or older binaries). Those segments hold only pending
/// chunk rows which were never promoted to committed chunks.
///
/// # Errors
///
/// Returns [`crate::core::error::CrabError`] on staging open, clean,
/// or close failure, or [`crate::core::error::CrabError::Cancelled`]
/// if the cancellation token fires.
pub async fn run_staging_clean(
    cancel: &CancellationToken,
    force: bool,
    prune_abandoned: bool,
) -> Result<()> {
    check_cancelled(cancel)?;

    let root = Path::new(STAGING_ROOT);
    let staging = if force {
        StagingArea::open_force(root.to_path_buf()).await?
    } else {
        StagingArea::open(root.to_path_buf()).await?
    };

    check_cancelled(cancel)?;

    let clean_stats = staging.clean()?;

    let abandoned_stats = if prune_abandoned {
        check_cancelled(cancel)?;
        Some(staging.clean_abandoned(force).await?)
    } else {
        None
    };

    info!(
        segments_removed = clean_stats.segments_removed,
        segments_compacted = clean_stats.segments_compacted,
        bytes_reclaimed = clean_stats.bytes_reclaimed,
        chunks_reclaimed = clean_stats.chunks_reclaimed,
        stale_markers_removed = clean_stats.stale_markers_removed,
        abandoned_segments_removed = abandoned_stats.map_or(0, |(s, _, _)| s),
        abandoned_bytes_reclaimed = abandoned_stats.map_or(0, |(_, b, _)| b),
        "staging clean complete"
    );

    println!("Staging clean complete:");
    println!("  Segments removed:      {}", clean_stats.segments_removed);
    println!(
        "  Segments compacted:    {}",
        clean_stats.segments_compacted
    );
    println!("  Bytes reclaimed:       {}", clean_stats.bytes_reclaimed);
    println!("  Chunks reclaimed:      {}", clean_stats.chunks_reclaimed);
    println!(
        "  Stale markers removed: {}",
        clean_stats.stale_markers_removed
    );
    if let Some((segs, bytes, pending)) = abandoned_stats {
        println!("  Abandoned segments:    {segs}");
        println!("  Abandoned bytes:       {bytes}");
        println!("  Abandoned pending:     {pending}");
    }

    staging.close().await?;
    Ok(())
}
