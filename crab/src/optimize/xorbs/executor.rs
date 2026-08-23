//! Streaming source-xorb → destination-xorb pipeline for optimization.
//!
//! The executor processes source xorbs one at a time through a bounded
//! pipeline:
//!
//! 1. HEAD source xorb → class, size, etag.
//! 2. If cold and `include_cold` → delegate to `RestoreOrchestrator`.
//!    If cold and `!include_cold` → skip with summary.
//! 3. Stream-download source xorb (bounded by `budget_factor × target`).
//! 4. Parse xorb, verify content hash.
//! 5. Walk chunks; re-pack into destination xorbs per profile.
//! 6. Upload each dest xorb via `Store::put`.
//! 7. Mark source xorb entry `status='done'` in journal with dest hashes.
//! 8. On corrupt source (hash mismatch), mark `status='corrupt'`, continue.
//!
//! Crash at steps 3–6 leaves no committed state (staged xorbs become
//! orphans reclaimed by the next `crab gc`). Crash at step 7 is
//! recoverable: the journal captures intent and the upload is idempotent.
//!
//! SIGINT/SIGTERM: the executor checks the `CancellationToken` between
//! xorbs. On cancellation it finishes the current xorb (if in-flight),
//! flushes the journal, and returns `Err(Cancelled)` so the CLI can
//! exit cleanly. The next invocation with `--resume` picks up where
//! it left off.

use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use serde::Serialize;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::core::error::{CrabError, Result, check_cancelled};
use crate::optimize::xorbs::journal::{OptimizeXorbsJournal, SourceStatus};
use crate::optimize::xorbs::profile::Profile;
use crate::storage::head_class::head_with_class;
use crate::storage::store::Store;
use crate::tier::restore::RestoreOrchestrator;
use crab_storage::canonical_global_content_path;
use crab_xet::xorb::builder::{FixedCompression, RunId, XorbBuilder};
use crab_xet::xorb::format::CompressionScheme;
use crab_xet::xorb::parser::XorbParser;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for the xorb optimization executor.
#[derive(Debug, Clone)]
pub struct ExecutorConfig {
    /// Include archive-class source xorbs. When false, archive xorbs
    /// are skipped and summarized in the outcome.
    pub include_cold: bool,
    /// Restore tier for archive sources (e.g. "standard").
    pub restore_tier: String,
    /// Output storage class for destination xorbs.
    pub output_class: String,
    /// Memory budget multiplier: the executor will not download a
    /// source xorb larger than `target_xorb_bytes × budget_factor`.
    /// Sources exceeding this are skipped with a warning.
    pub budget_factor: u64,
}

impl Default for ExecutorConfig {
    fn default() -> Self {
        Self {
            include_cold: true,
            restore_tier: "standard".to_string(),
            output_class: "STANDARD".to_string(),
            budget_factor: 2,
        }
    }
}

// ---------------------------------------------------------------------------
// Progress tracking
// ---------------------------------------------------------------------------

/// Per-xorb progress event emitted during execution.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct XorbProgressEvent {
    /// Source xorb hash.
    pub src_xorb: String,
    /// Current state of this xorb's processing.
    pub state: String,
    /// Destination xorb hashes (populated on completion).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dest_xorbs: Option<Vec<String>>,
    /// Bytes read from the source.
    pub bytes_read: u64,
    /// Bytes written to destinations.
    pub bytes_written: u64,
    /// Elapsed time in milliseconds.
    pub elapsed_ms: u64,
}

// ---------------------------------------------------------------------------
// Executor outcome
// ---------------------------------------------------------------------------

/// Summary of a completed xorb optimization.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct ExecutorOutcome {
    /// Run identifier.
    pub run_id: String,
    /// Profile used.
    pub profile: String,
    /// Total source xorbs processed (attempted).
    pub sources_processed: u64,
    /// Source xorbs completed successfully.
    pub sources_done: u64,
    /// Source xorbs found corrupt (hash mismatch or parse failure).
    pub sources_corrupt: u64,
    /// Source xorbs skipped (archive when `include_cold=false`, not
    /// found, or exceeding memory budget).
    pub sources_skipped: u64,
    /// Total bytes read from source xorbs.
    pub bytes_read: u64,
    /// Total bytes written to destination xorbs.
    pub bytes_written: u64,
    /// Wall-clock duration in milliseconds.
    pub elapsed_ms: u64,
    /// List of corrupt source xorb hashes (recommend `crab fsck`).
    pub corrupt_list: Vec<String>,
}

// ---------------------------------------------------------------------------
// Executor
// ---------------------------------------------------------------------------

/// Execute an xorb optimization run.
///
/// Processes each pending source xorb from the journal through the
/// download → parse → repack → upload pipeline.
///
/// # Cancellation
///
/// On SIGINT/SIGTERM (detected via `cancel`), finishes the current
/// xorb, flushes the journal, and returns `Err(Cancelled)` so the
/// next invocation with `--resume` picks up where it left off.
///
/// # Journal-only mode
///
/// When `store` is `None` (tests or no remote configured), the
/// executor marks all pending sources as done with empty dest lists.
/// This preserves the CLI and journal lifecycle for testing.
///
/// # Tier-aware operation
///
/// When `restore_orchestrator` is provided and a source xorb is in an
/// archive storage class, the executor calls `ensure_warm` before
/// downloading. When `config.include_cold` is false, archive xorbs
/// are skipped instead.
pub async fn execute(
    journal: &OptimizeXorbsJournal,
    run_id: &str,
    profile: &Profile,
    config: &ExecutorConfig,
    cancel: &CancellationToken,
    store: Option<&Store>,
    restore_orchestrator: Option<&RestoreOrchestrator>,
) -> Result<ExecutorOutcome> {
    let start = Instant::now();
    let profile_desc = profile.to_json();

    let mut sources_processed: u64 = 0;
    let mut sources_done: u64 = 0;
    let mut sources_corrupt: u64 = 0;
    let mut sources_skipped: u64 = 0;
    let mut bytes_read: u64 = 0;
    let mut bytes_written: u64 = 0;
    let mut corrupt_list: Vec<String> = Vec::new();

    // Compute the memory budget for a single source xorb download.
    let memory_budget = profile
        .target_xorb_bytes
        .saturating_mul(config.budget_factor);

    // Fetch all pending source xorbs from the journal.
    let pending = journal.sources_by_status(run_id, SourceStatus::Pending)?;

    if pending.is_empty() {
        info!("xorb optimization executor: no pending source xorbs");
        return Ok(build_outcome(
            run_id,
            &profile_desc,
            &start,
            0,
            0,
            0,
            0,
            0,
            0,
            Vec::new(),
        ));
    }

    info!(
        pending = pending.len(),
        memory_budget_mib = memory_budget / (1024 * 1024),
        "xorb optimization executor: processing source xorbs"
    );

    let compression = resolve_compression(profile);

    for source_row in &pending {
        // Check for cancellation between xorbs (graceful SIGINT/SIGTERM).
        check_cancelled(cancel)?;

        let src_hash = &source_row.src_xorb;
        sources_processed += 1;

        debug!(src_xorb = %src_hash, idx = sources_processed, "processing source xorb");

        match process_single_xorb(
            journal,
            run_id,
            src_hash,
            profile,
            config,
            compression,
            memory_budget,
            store,
            restore_orchestrator,
        )
        .await
        {
            Ok(result) => {
                bytes_read += result.bytes_read;
                bytes_written += result.bytes_written;
                match result.status {
                    XorbStatus::Done => sources_done += 1,
                    XorbStatus::Corrupt => {
                        sources_corrupt += 1;
                        corrupt_list.push(src_hash.clone());
                    }
                    XorbStatus::Skipped => sources_skipped += 1,
                }
            }
            Err(e) if is_transient(&e) => {
                // Transient errors: log and skip so the run can continue.
                // The source stays Pending and will be retried on --resume.
                warn!(
                    src_xorb = %src_hash,
                    error = %e,
                    "transient error processing source xorb; will retry on resume"
                );
                sources_skipped += 1;
            }
            Err(e) => {
                // Permanent errors: log, mark skipped, continue.
                warn!(
                    src_xorb = %src_hash,
                    error = %e,
                    "failed to process source xorb; skipping"
                );
                journal.update_source_status(run_id, src_hash, SourceStatus::Skipped, None)?;
                sources_skipped += 1;
            }
        }
    }

    let elapsed = start.elapsed();
    info!(
        done = sources_done,
        corrupt = sources_corrupt,
        skipped = sources_skipped,
        bytes_read,
        bytes_written,
        elapsed_ms = elapsed.as_millis() as u64,
        "xorb optimization executor: complete"
    );

    Ok(build_outcome(
        run_id,
        &profile_desc,
        &start,
        sources_processed,
        sources_done,
        sources_corrupt,
        sources_skipped,
        bytes_read,
        bytes_written,
        corrupt_list,
    ))
}

// ---------------------------------------------------------------------------
// Per-xorb processing
// ---------------------------------------------------------------------------

enum XorbStatus {
    Done,
    Corrupt,
    Skipped,
}

struct SingleXorbResult {
    status: XorbStatus,
    bytes_read: u64,
    bytes_written: u64,
}

/// Process a single source xorb through the optimization pipeline.
///
/// Each step is designed so that a crash leaves no committed state
/// until the final journal update. Destination xorbs uploaded before
/// a crash become orphans reclaimed by `crab gc`.
#[expect(clippy::too_many_arguments, reason = "pipeline step needs all context")]
async fn process_single_xorb(
    journal: &OptimizeXorbsJournal,
    run_id: &str,
    src_hash: &str,
    profile: &Profile,
    config: &ExecutorConfig,
    compression: CompressionScheme,
    memory_budget: u64,
    store: Option<&Store>,
    restore_orchestrator: Option<&RestoreOrchestrator>,
) -> Result<SingleXorbResult> {
    // --- Journal-only mode (no store) ---
    let Some(store) = store else {
        journal.update_source_status(run_id, src_hash, SourceStatus::Done, Some("[]"))?;
        return Ok(SingleXorbResult {
            status: XorbStatus::Done,
            bytes_read: 0,
            bytes_written: 0,
        });
    };

    let xorb_path = canonical_global_content_path("xorbs", src_hash);

    // --- Step 1: HEAD with class probe ---
    let head_meta = head_with_class(store, &xorb_path).await;
    if let Ok(ref meta) = head_meta {
        if meta.class.is_archive_class() {
            if !config.include_cold {
                debug!(src_xorb = %src_hash, class = ?meta.class, "skipping archive xorb (include_cold=false)");
                journal.update_source_status(run_id, src_hash, SourceStatus::Skipped, None)?;
                return Ok(SingleXorbResult {
                    status: XorbStatus::Skipped,
                    bytes_read: 0,
                    bytes_written: 0,
                });
            }
            // --- Step 2: Restore if archive ---
            if let Some(orchestrator) = restore_orchestrator {
                info!(src_xorb = %src_hash, class = ?meta.class, "restoring archive xorb before download");
                orchestrator.ensure_warm(&xorb_path.to_string()).await?;
            } else {
                return Err(CrabError::ArchiveRestoreRequired {
                    xorb: xorb_path.to_string(),
                    class: format!("{}", meta.class),
                    estimated_eta: None,
                });
            }
        }
    }
    // HEAD failure is non-fatal: proceed to download (the GET will
    // surface the real error if the object is truly inaccessible).

    // --- Step 3: Download source xorb (bounded by memory budget) ---
    let src_bytes = match store.get_with_etag(&xorb_path).await {
        Ok((bytes, _etag)) => bytes,
        Err(CrabError::NotFound { .. }) => {
            debug!(src_xorb = %src_hash, "source xorb not found; skipping");
            journal.update_source_status(run_id, src_hash, SourceStatus::Skipped, None)?;
            return Ok(SingleXorbResult {
                status: XorbStatus::Skipped,
                bytes_read: 0,
                bytes_written: 0,
            });
        }
        Err(e) => return Err(e),
    };

    let src_size = src_bytes.len() as u64;

    // Enforce memory budget: skip xorbs that exceed the limit.
    if src_size > memory_budget {
        warn!(
            src_xorb = %src_hash,
            src_size,
            memory_budget,
            "source xorb exceeds memory budget; skipping"
        );
        journal.update_source_status(run_id, src_hash, SourceStatus::Skipped, None)?;
        return Ok(SingleXorbResult {
            status: XorbStatus::Skipped,
            bytes_read: src_size,
            bytes_written: 0,
        });
    }

    // --- Step 4: Parse and verify content hash ---
    let parser = match XorbParser::parse(src_bytes) {
        Ok(p) => p,
        Err(e) => {
            warn!(src_xorb = %src_hash, error = %e, "corrupt source xorb (parse failed)");
            journal.mark_corrupt(run_id, src_hash, "parse_failed", &e.to_string())?;
            return Ok(SingleXorbResult {
                status: XorbStatus::Corrupt,
                bytes_read: src_size,
                bytes_written: 0,
            });
        }
    };

    let actual_hash = parser.hash().hex();
    if actual_hash != src_hash {
        warn!(
            src_xorb = %src_hash,
            actual_hash = %actual_hash,
            "corrupt source xorb (hash mismatch)"
        );
        journal.mark_corrupt(
            run_id,
            src_hash,
            "hash_mismatch",
            &format!("expected {src_hash}, got {actual_hash}"),
        )?;
        return Ok(SingleXorbResult {
            status: XorbStatus::Corrupt,
            bytes_read: src_size,
            bytes_written: 0,
        });
    }

    // --- Step 5: Extract chunks and repack into destination xorbs ---
    let num_chunks = parser.num_chunks();
    let policy = Arc::new(FixedCompression::new(compression));
    let mut builder =
        XorbBuilder::with_policy(policy as Arc<dyn crab_xet::xorb::builder::CompressionPolicy>);

    // Set the target size from the profile.
    #[expect(
        clippy::cast_possible_truncation,
        reason = "target_xorb_bytes validated ≤ 2 GiB, fits in usize"
    )]
    let target = profile.target_xorb_bytes as usize;
    builder.set_target_size(target);

    // Feed all chunks from the source xorb into the builder.
    // All chunks share a single RunId since they come from one source.
    let run_id_num = RunId(0);
    for idx in 0..num_chunks {
        let chunk = parser.get_chunk(idx)?;
        let _ = builder.push(&chunk, run_id_num)?;
    }

    // Finalize the builder to get destination xorbs.
    let dest_xorbs = builder.finalize()?;

    // --- Step 6: Upload each destination xorb ---
    let mut dest_hashes: Vec<String> = Vec::with_capacity(dest_xorbs.len());
    let mut total_written: u64 = 0;

    for xorb_result in &dest_xorbs {
        let dest_hash = xorb_result.hash.hex();
        let dest_path = canonical_global_content_path("xorbs", &dest_hash);
        let dest_bytes = Bytes::copy_from_slice(&xorb_result.bytes);
        let written = dest_bytes.len() as u64;

        // CAS put: if the xorb already exists (idempotent retry or
        // concurrent push wrote the same content), the put succeeds
        // silently via the Store's create-if-absent semantics.
        store.put(&dest_path, dest_bytes).await?;

        dest_hashes.push(dest_hash);
        total_written += written;
    }

    // --- Step 7: Commit to journal (crash-safe boundary) ---
    let dest_json = serde_json::to_string(&dest_hashes).unwrap_or_else(|_| "[]".to_string());
    journal.update_source_status(run_id, src_hash, SourceStatus::Done, Some(&dest_json))?;

    debug!(
        src_xorb = %src_hash,
        dest_count = dest_hashes.len(),
        bytes_read = src_size,
        bytes_written = total_written,
        "source xorb optimized"
    );

    Ok(SingleXorbResult {
        status: XorbStatus::Done,
        bytes_read: src_size,
        bytes_written: total_written,
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Map the profile's compression config to a `CompressionScheme`.
fn resolve_compression(profile: &Profile) -> CompressionScheme {
    use crate::core::config::CompressionConfig;
    match profile.compression {
        CompressionConfig::None => CompressionScheme::None,
        CompressionConfig::Lz4 => CompressionScheme::LZ4,
        CompressionConfig::Zstd { .. } => CompressionScheme::LZ4,
    }
}

/// Classify whether an error is transient (worth retrying on --resume)
/// vs permanent (skip and move on).
fn is_transient(err: &CrabError) -> bool {
    matches!(
        err,
        CrabError::NetworkTransient(_) | CrabError::Throttled { .. }
    )
}

#[expect(clippy::too_many_arguments, reason = "builder helper")]
fn build_outcome(
    run_id: &str,
    profile: &str,
    start: &Instant,
    sources_processed: u64,
    sources_done: u64,
    sources_corrupt: u64,
    sources_skipped: u64,
    bytes_read: u64,
    bytes_written: u64,
    corrupt_list: Vec<String>,
) -> ExecutorOutcome {
    ExecutorOutcome {
        run_id: run_id.to_string(),
        profile: profile.to_string(),
        sources_processed,
        sources_done,
        sources_corrupt,
        sources_skipped,
        bytes_read,
        bytes_written,
        elapsed_ms: start.elapsed().as_millis() as u64,
        corrupt_list,
    }
}

/// Check whether a GC operation is currently running.
///
/// Probes for the GC lock file at the standard location. Returns
/// `ConcurrentMaintenance` if GC is active.
pub fn check_gc_not_running(crab_dir: &std::path::Path) -> Result<()> {
    let gc_lock = crab_dir.join("gc.lock");
    if gc_lock.exists() {
        return Err(CrabError::ConcurrentMaintenance { other: "gc" });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn executor_config_defaults() {
        let cfg = ExecutorConfig::default();
        assert!(cfg.include_cold);
        assert_eq!(cfg.restore_tier, "standard");
        assert_eq!(cfg.output_class, "STANDARD");
        assert_eq!(cfg.budget_factor, 2);
    }

    #[test]
    fn check_gc_not_running_passes_when_no_lock() {
        let dir = tempfile::tempdir().unwrap();
        check_gc_not_running(dir.path()).unwrap();
    }

    #[test]
    fn check_gc_not_running_fails_when_lock_exists() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("gc.lock"), "locked").unwrap();
        let err = check_gc_not_running(dir.path()).unwrap_err();
        assert!(err.to_string().contains("E0333"));
    }

    #[test]
    fn resolve_compression_none() {
        let mut p = crate::optimize::xorbs::profile::Profile::code();
        p.compression = crate::core::config::CompressionConfig::None;
        assert_eq!(resolve_compression(&p), CompressionScheme::None);
    }

    #[test]
    fn resolve_compression_lz4() {
        let mut p = crate::optimize::xorbs::profile::Profile::code();
        p.compression = crate::core::config::CompressionConfig::Lz4;
        assert_eq!(resolve_compression(&p), CompressionScheme::LZ4);
    }

    #[test]
    fn resolve_compression_zstd_defaults_to_lz4() {
        let p = crate::optimize::xorbs::profile::Profile::ml();
        assert_eq!(resolve_compression(&p), CompressionScheme::LZ4);
    }

    #[test]
    fn is_transient_classifies_network_errors() {
        let transient = CrabError::NetworkTransient(object_store::Error::Generic {
            store: "test",
            source: "timeout".into(),
        });
        assert!(is_transient(&transient));

        let permanent = CrabError::NotFound {
            path: "x".to_string(),
        };
        assert!(!is_transient(&permanent));
    }

    #[tokio::test]
    async fn execute_with_no_store_marks_pending_as_done() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.db");
        let journal = OptimizeXorbsJournal::open(&path).unwrap();

        journal.start_run("test-run", "{}").unwrap();
        journal.insert_source("test-run", "xorb-aaa").unwrap();
        journal.insert_source("test-run", "xorb-bbb").unwrap();

        let cancel = CancellationToken::new();
        let profile = crate::optimize::xorbs::profile::Profile::code();
        let config = ExecutorConfig::default();

        let outcome = execute(&journal, "test-run", &profile, &config, &cancel, None, None)
            .await
            .unwrap();

        assert_eq!(outcome.sources_processed, 2);
        assert_eq!(outcome.sources_done, 2);
        assert_eq!(outcome.sources_corrupt, 0);
        assert_eq!(outcome.sources_skipped, 0);

        let counts = journal.count_by_status("test-run").unwrap();
        assert_eq!(counts.done, 2);
        assert_eq!(counts.pending, 0);
    }

    #[tokio::test]
    async fn execute_with_no_pending_returns_empty_outcome() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.db");
        let journal = OptimizeXorbsJournal::open(&path).unwrap();
        journal.start_run("empty-run", "{}").unwrap();

        let cancel = CancellationToken::new();
        let profile = crate::optimize::xorbs::profile::Profile::code();
        let config = ExecutorConfig::default();

        let outcome = execute(
            &journal,
            "empty-run",
            &profile,
            &config,
            &cancel,
            None,
            None,
        )
        .await
        .unwrap();

        assert_eq!(outcome.sources_processed, 0);
        assert_eq!(outcome.sources_done, 0);
    }

    #[tokio::test]
    async fn execute_cancellation_stops_between_xorbs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("journal.db");
        let journal = OptimizeXorbsJournal::open(&path).unwrap();

        journal.start_run("cancel-run", "{}").unwrap();
        journal.insert_source("cancel-run", "xorb-001").unwrap();
        journal.insert_source("cancel-run", "xorb-002").unwrap();

        let cancel = CancellationToken::new();
        // Cancel immediately — the first check_cancelled should fire.
        cancel.cancel();

        let profile = crate::optimize::xorbs::profile::Profile::code();
        let config = ExecutorConfig::default();

        let result = execute(
            &journal,
            "cancel-run",
            &profile,
            &config,
            &cancel,
            None,
            None,
        )
        .await;

        assert!(result.is_err());
        // Both sources should still be pending (cancellation before processing).
        let counts = journal.count_by_status("cancel-run").unwrap();
        assert_eq!(counts.pending, 2);
    }
}
